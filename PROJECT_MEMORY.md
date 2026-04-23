# PROJECT_MEMORY.md

> **Purpose.** A single structured entry-point for understanding the Open
> Claude Code desktop system *without* re-reading the entire codebase. If
> you can read only one file before making a change, read this one.
>
> **Scope.** Everything here is about the **`desktop/` subtree** — the
> Tauri + React application. The top-level `src/` folder is the recovered
> research snapshot and is not part of the running system. See
> [`AGENTS.md`](AGENTS.md) / [`CLAUDE.md`](CLAUDE.md) for snapshot context.
>
> **Maintenance rule.** Whenever you discover something new about the
> system, or clarify behaviour that wasn't previously documented, update
> this file in the same PR. The goal is to accumulate structured
> knowledge instead of rediscovering it every session.

---

## 1. Current architecture (one screen)

```
┌──────────────────────────── React UI (desktop/frontend/src) ────────────────────────────┐
│                                                                                         │
│  ┌─────────────┐    ┌──────────────────┐    ┌─────────────────────┐    ┌─────────────┐  │
│  │  Explorer   │    │   Goal & Tasks   │    │        Chat         │    │   Debug     │  │
│  │ (file tree) │    │ TaskPanel.tsx    │    │ Chat.tsx (3-tier)   │    │ Execution…  │  │
│  │ Explorer…   │    │ status icons,    │    │ Thinking /           │    │ tool_call / │  │
│  │             │    │ live timing,     │    │ FinalAnswer /        │    │ tool_result │  │
│  │ collapsible │    │ progressbar,     │    │ SystemAction         │    │ ring-buffer │  │
│  │ (36 px rail)│    │ inline tool call │    │ + provider/model     │    │ 500 entries │  │
│  │             │    │ on running task  │    │ badges per step      │    │ react-window│  │
│  └─────────────┘    └──────────────────┘    └─────────────────────┘    └─────────────┘  │
│        ▲                    ▲                        ▲                        ▲        │
│        │                    │                        │                        │        │
│        └──────────── useAppStore (Zustand) ──────────┴────────────────────────┘        │
│                     projectDir, messages, events (ring-buf), UI toggles                 │
│                                                                                         │
└──────────────────────────────── IPC / Tauri events ─────────────────────────────────────┘
                                          │
                                          ▼
┌──────────────────────────── Rust backend (desktop/src-tauri/src) ───────────────────────┐
│                                                                                         │
│  lib.rs  ── tauri commands (open_project, run_chat_turn, start_goal, probe_openrouter…) │
│    │                                                                                    │
│    ▼                                                                                    │
│  controller.rs  ── start_goal → plan_goal → for task: ai::run_chat_turn → archive       │
│    │                                                                                    │
│    ▼                                                                                    │
│  ai.rs          ── Planner / Executor / Reviewer loop                                   │
│                    resolve_provider(settings, role) → (primary, fallback)               │
│                    call_model() dispatches OpenRouter vs Ollama                         │
│                                                                                         │
│  tools.rs       ── read_file, write_file, list_dir, run_cmd (sandboxed to project root) │
│  settings.rs    ── persisted per-user settings (provider_mode, per-role models, …)      │
│  cancel.rs      ── CancelToken; tree-kill; typed CancelReason                           │
│  watcher.rs     ── project FS change notifier → frontend fsTick bump                    │
│  tasks.rs       ── active_task_tree + task_history, per-task bounded trace              │
│                                                                                         │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

Three principal concerns live in three separate layers:

1. **Tool runtime** (Rust, sandboxed) — every filesystem or shell operation
   goes through `fs_ops::resolve` and `tools.rs`. There are no fake
   execution paths; the model cannot bypass the sandbox.
2. **Three-agent loop** (Rust, `ai.rs`) — Planner → Executor → Reviewer.
   The provider for each role is resolved from the settings at call time,
   not hard-coded.
3. **UI + state** (TypeScript, `desktop/frontend/src`) — Ink-less React,
   Zustand for global state, `react-window` for the Debug pane only, and
   a three-tier message-rendering system in Chat.

---

## 2. AI routing (Phase 2)

### ProviderMode

Three modes, selected in Settings → *Provider mode*:

| Mode | Planner | Executor | Reviewer | Typical use |
|---|---|---|---|---|
| **Cloud** | OpenRouter | OpenRouter | OpenRouter | You want the best plans and have an API key. |
| **Local** (default) | Ollama | Ollama | Ollama | Fully offline, no external network. |
| **Hybrid** | OpenRouter | Ollama | OpenRouter | Reasoning roles on OpenRouter, high-throughput tool loop local. |

Selection happens in `ai.rs::resolve_provider(settings, role) -> (primary, fallback)`.
`Hybrid` is the only mode that declares a fallback: Planner and Reviewer
primary on OpenRouter → fallback to Ollama, Executor primary on Ollama
→ fallback to OpenRouter. If OpenRouter is unavailable the reasoning
roles silently downgrade to local; if Ollama is unavailable the
executor silently promotes to OpenRouter. `Cloud` and `Local` have
**no** fallback, so a misconfigured `Cloud` mode fails loudly; that is
by design (audit OCCD §6.4).

### Per-role models

`settings.planner_model`, `settings.executor_model`, `settings.reviewer_model`
are independent string slots. They override the provider default when
set. Empty strings mean "use provider default". This is why the Settings
UI shows three separate model inputs, each with a placeholder hint.

### `call_model` dispatcher

All Planner / Executor / Reviewer streaming calls go through
`ai::call_model(settings, role, messages, cancel) -> Stream`. That
function resolves the provider and then dispatches to
`stream_openrouter` / `stream_ollama`. **Never call the stream_
functions directly**; any new call site must go through `call_model` so
provider routing, `ai:step` metadata, and fallback stay consistent.

### Health probes

Two layers: the **deep probes** (explicit "Test" buttons in Settings)
and the **lightweight role probes** (topbar badges).

Deep probes:

- `probe_ollama` — 10 s timeout, returns reachable + models.
- `probe_openrouter` — 10 s timeout, returns
  `{ reachable, key_valid, model, credits }`.

Role probes (PR #10, Scenario-A §9.2 F-1):

- `check_planner` / `check_executor` both delegate to a single helper
  `check_role_reachable(settings, role)` in `ai.rs`. The helper calls
  `resolve_provider(settings, role).primary` and probes whichever
  backend that role will *actually* use under the current
  `ProviderMode`:
  - `Provider::OpenRouter` → reachable iff `openrouter_api_key` is
    non-empty. (No network; a full round-trip is available via
    `probe_openrouter`.)
  - `Provider::Ollama` → reachable iff `GET /api/tags` returns 2xx
    within 10 s.
- This is what powers the topbar's two status badges. Before PR #10 the
  badges were hard-coded to OpenRouter-key (planner) and Ollama-tags
  (executor), which lied in both Cloud and Local mode. The executor
  badge text was also re-labelled from the provider-specific "ollama
  online / offline" to the provider-neutral **"executor ready / off"**
  for the same reason.

10 s is deliberately long: Ollama cold-starts a model on the first probe
after idle. The 3 s we used to have produced false "planner offline"
states on laptops.

### Retry behaviour

- **Request-level retry on OpenRouter 5xx** (PR #3): `stream_openrouter`
  retries 5xx responses with exponential backoff before surfacing a
  failure. 4xx responses (including 401/403) do **not** retry.
- **JSON-output enforcement** (PR #2): the goal planner sends
  `response_format: { type: "json_object" }` to OpenRouter and
  `format: "json"` to Ollama. `parse_plan_json` still has a JSON-in-prose
  fallback for models that ignore the directive.
- **Reviewer sees tools** (PR #1 Phase 1): `controller::review_task` now
  includes the executor's tool-call transcript when evaluating output,
  so a hallucinated success can be caught. This was the original
  Critical finding in `FULL_SYSTEM_AUDIT.md` §3.4.
- **Mutex poison recovery** (PR #3): `AppState` mutex accesses recover
  from poisoned locks instead of panicking. A single panic in a worker
  task no longer kills the whole app.

---

## 3. UI architecture (Phases 3 – 5)

### Three-tier message rendering (`Chat.tsx`)

`classifyMessages(msgs)` assigns each message exactly one of:

- **`thinking`** — planner / reviewer intermediate output, and any
  executor step that isn't the final answer. Rendered via
  `ThinkingBlock.tsx`: streaming → collapsed → expanded. Respects
  `prefers-reduced-motion`.
- **`final`** — the *last* executor message of a completed turn.
  Rendered as a prominent `FinalAnswerBubble.tsx`.
- **`system`** — tool calls, tool results, errors, info. Rendered
  inline and compact via `SystemAction.tsx`.

This is why Chat never shows raw "Planner: ..." / "Executor: ..."
bubbles anymore — those are intermediate and belong in a Thinking block
that collapses after the turn completes. If you add a new message
source, update `classifyMessages` in the same PR.

Each tier renders `provider` / `model` badges when the backend includes
them in the `ai:step` event metadata — that's how the user sees "this
plan came from OpenRouter / gpt-4o-mini" on a per-step basis.

### TaskPanel (Phase 4)

`TaskPanel.tsx` renders the Goal & Tasks pane as a status-icon list
(`○ / ⋯ / ✓ / ✗ / ⊘`), with:

- a live timer per task and a global goal timer that ticks every
  second while the run is active;
- an ARIA `progressbar` with `running` / `failed` count chips;
- the latest `tool_call` for the running task, and the first `error`
  line for failed tasks, rendered inline on the task row;
- a condensed failure summary with the full message in the row's
  `title` attribute (so you don't have to expand the trace to see
  what went wrong).

`ChatResponse.executor_iterations` (Addendum §2.9) is surfaced on the
completed task row so you can see how many passes the executor took.

### Global state (Phase 5.2)

`src/store.ts` is the single Zustand store. State lives there, not in
`App.tsx`. Components read with selectors:

```ts
const messages = useAppStore((s) => s.messages);
const pushError = useAppStore((s) => s.pushError);
```

Key store slices and why they're there:

| Slice | Owner / writer | Notes |
|---|---|---|
| `projectDir`, `fsTick` | `open_project` tauri handler → `App.tsx` | `fsTick` bumps when the watcher fires so the Explorer re-renders. |
| `plannerOk`, `executorOk` | health probes in `App.tsx` | `null` = unknown, `true/false` = reachable. |
| `messages` | `Chat.tsx` (on send) | `setMessages` supports both array and updater callback for compat with existing streaming code. |
| `events` | global tauri event listener | Ring-buffered at 500 entries (`EVENTS_CAP`). Trimmed inside `pushEvent` / `pushError`. |
| `debugOpen`, `explorerOpen`, `settingsOpen` | user toggles | `pushError` forces `debugOpen = true` in the same atomic `set` call so errors cannot fail silently. |

### Event ring buffer

`pushEvent` and `pushError` both guarantee `events.length <= EVENTS_CAP`.
If you add a new event source, use these store methods — do **not**
push directly into `events`. The count header shows `500+` when at cap
so users know the oldest events were dropped.

### Virtualisation strategy

- **Debug pane** (`components/Execution.tsx`) — virtualised with
  `react-window` `<List>` + `useDynamicRowHeight`. Inline events only
  (tool_call, tool_result, error, info). Step events are kept in a
  separate non-virtualised agent timeline above the list because they
  mutate in place as plan steps transition `running → done/failed`,
  which fights react-window's caching.
- **Chat pane** — **intentionally not virtualised.** Streaming updates
  the last bubble's height on every token, and the Thinking block
  expands/collapses on user interaction. At realistic message counts
  (≤ 200 / session) a dynamic-height virtual list would cost more in
  layout churn than it saves.

### Typography (Phase 5.2, PR #7 + #8)

UI fonts are self-hosted via `@fontsource/*`:

- `@fontsource/inter/{400,500,600}.css` — UI font.
- `@fontsource/jetbrains-mono/{400,500,600}.css` — code font. The 600
  weight matters because several `.exec-item` children use
  `font-weight: 600` while inheriting `font-family: var(--mono)`;
  without the 600 file the browser synthesises faux-bold from 500
  (PR #7 Devin Review caught this; fixed in PR #8).

If you add a new weight to CSS, add the matching `@fontsource` import
in `main.tsx`. If you add a new typeface, re-evaluate whether the
rendering still passes the `1` vs `l` visual-distinctness test Inter
gives us through `cv01/cv02`.

### Settings: provider cards + OpenRouter model browser

The Settings modal (`desktop/frontend/src/components/Settings.tsx`) is organised
into **two product-style cards**:

- **OpenRouter** — provider mode, API key, default model, connection test, and
  per-role overrides (planner / reviewer).
- **Ollama** — base URL, default executor model, executor override, and
  connection test.

OpenRouter model picking is supported by a **CSV-powered model browser** sourced
from `OpenRouter_Categorized_Models.csv`:

- **All models tab**
  - Search by name or id.
  - Filter by category.
- **Free models tab**
  - A curated, Arabic-labelled set of free and recommended models (grouped into
    six categories with descriptions).

Selecting a model from either tab writes the correct OpenRouter `modelId`
(`provider/model[:tag]`) into `settings.openrouter_model`.

### Terminal pane (in-app command runner)

The main layout now includes a **bottom-docked panel** with tabs (**Debug** / **Terminal**)
so you can run project commands inside the app (instead of spawning a separate `cmd.exe` window)
and follow the output in real time.

- The UI calls `api.runCmdStream(...)` and subscribes to backend events.
- The Tauri backend exposes `tools::run_cmd_stream` which emits:
  - `terminal:output` `{ stream: "stdout"|"stderr", data: string }`
  - `terminal:done` `{ exit_code: number }`

This is **user-initiated** command execution (separate from AI tool-loop `run_cmd` gating).

The Terminal tab supports **multiple concurrent terminal sessions** via `TerminalManager`:

- Each terminal tab has a `terminal_id`.
- Backend events include `terminal_id` so output does not mix across tabs:
  - `terminal:output` `{ terminal_id, stream, data }`
  - `terminal:done` `{ terminal_id, exit_code }`
- Each tab can be stopped via `terminal_kill(terminal_id)`.

### Failures tab (project-scoped)

The bottom-docked panel includes a **Failures** tab that surfaces recent executor/task
failures for the **currently opened project only**:

- Failures are captured from `task:failure_logged` and shown in a capped list.
- The list is **reset when you open/restore a project** (it is not global across sessions).
- A **Clear** button wipes both the in-memory list and the persisted `failures_log` for
  the project.

### Bottom panel (manual resize)

The bottom-docked panel (Debug / Terminal / Failures) supports **manual resizing**
(Windsurf-style): drag the thin handle at the top edge of the panel to adjust its height.

- Height is stored in Zustand as `bottomPanelHeight`.
- The panel height is applied by dynamically setting the `.layout` grid’s
  `gridTemplateRows` (expanded: `1fr {bottomPanelHeight}px`, collapsed: `1fr 36px`).
- Dragging is implemented in `App.tsx` via `onMouseDown` + window `mousemove`/`mouseup`.

### Windows bundling (installer output)

To produce full Windows installer artifacts (not just the `target/release/*.exe` build),
enable bundling and configure an `.ico` icon in `desktop/src-tauri/tauri.conf.json`.

- Bundling must have `bundle.active: true`.
- Provide at least one `.ico` in `bundle.icon` (e.g. `icons/icon.ico`) or bundling fails.
- `cargo tauri build` emits installers under:
  - `desktop/src-tauri/target/release/bundle/msi/*.msi`
  - `desktop/src-tauri/target/release/bundle/nsis/*-setup.exe`

### Accessibility

- Semantic `<header role="banner">` topbar, `<section aria-label>`
  landmarks per pane.
- `aria-live="polite"` on status badges so screen readers announce
  provider/planner state changes.
- `aria-hidden` on purely-decorative status dots.
- `prefers-reduced-motion` disables the streaming-dot animation in
  `ThinkingBlock`.
- `progressbar` ARIA on TaskPanel with `aria-valuenow`.

### Responsive layout

Breakpoints at **1100 px** and **900 px** in `styles.css`. Below 1100
the Goal pane hides; below 900 the Explorer collapses to its rail.
Both Debug and Explorer are collapsible down to a 36 px icon rail at
any width.

---

## 4. System flows

### 4.1 Chat turn (`ai::run_chat_turn`)

```
user message
  │
  ▼
Planner call    (call_model(role=Planner))
  │  ├─ emits ai:step { role: Planner, provider, model, status }
  │  ├─ emits ai:token stream
  │  └─ may emit ai:tool_call / ai:tool_result
  │
  ▼
Executor loop   (call_model(role=Executor), ≤ max_iterations)
  │  for each iteration:
  │    ├─ stream tokens
  │    ├─ model emits tool_call JSON → tools.rs::execute_tool
  │    ├─ tool_result routed back as next turn input
  │    └─ stop when model emits final_answer (no tool_call)
  │
  ▼
Reviewer call   (call_model(role=Reviewer))  — if reviewer_enabled AND NOT json_mode
  │  ├─ sees the executor's tool-call + tool-result transcript
  │  ├─ emits OK: or NEEDS_FIX: <instruction>
  │  └─ NEEDS_FIX feeds back into a retry (up to max_retries_per_task)
  │
  │  `json_mode` turns (see §4.2 — used by `plan_goal`) run the
  │  executor without a tool schema and expect a JSON string back,
  │  not code changes. Running OK/NEEDS_FIX over pure JSON is
  │  meaningless and would cause the reviewer to return `Unknown`,
  │  which in turn fires `ai:executor_unparsed` and renders the
  │  "try a larger executor model" warning pill for every single
  │  goal plan. PR #15 therefore skips the reviewer entirely in
  │  json_mode — see `ai.rs:run_chat_turn` line ~1679.
  │
  ▼
ChatResponse { final_message, executor_iterations, reviewer_outcome }
```

### 4.2 Goal execution (`controller::start_goal`)

```
scan_project          → project_map
  │
  ▼
plan_goal             → JSON task list (max_total_tasks, heuristic fallback)
  │
  ▼
for each runnable task (circuit breaker counts consecutive failures):
    ai::run_chat_turn wrapped in tokio::time::timeout(task_timeout_secs)
       │
       ├─ ok                    → mark done, trace append
       ├─ reviewer NEEDS_FIX    → retry with feedback, exponential backoff
       ├─ task timeout          → CancelReason::Timeout → mark failed, retry
       ├─ goal timeout          → CancelReason::Timeout → mark cancelled, stop
       ├─ user cancel           → CancelReason::User    → mark cancelled, stop
       └─ circuit open          → CancelReason::CircuitOpen → abort goal
  │
  ▼
archive into task_history (cap 200, oldest dropped)
emit task:goal_done { status, completed, failed }
```

### 4.3 Event flow (Rust → frontend)

All events go through `tauri::Emitter::emit(app, name, payload)`. The
frontend listens in `App.tsx` via `listen(name, cb)` and the callback
calls the appropriate store action (`pushEvent`, `pushError`,
`setMessages`, ...). **New events must:**

1. Emit from Rust with a stable name. Prefixes in use today:
   - `ai:` — raw model activity (token, tool_call, tool_result, step,
     done, error, confirm_request).
   - `task:` — per-task controller lifecycle (goal_started, added,
     update, trace, goal_done, failure_logged, circuit_tripped).
   - `goal:` — top-level pre-execution phase (`planning`,
     `planning_done`; PR #11, Scenario-A §9.2 F-2). These fire once
     before any `task:` event and again when the pre-execution window
     closes, so the TaskPanel can show a "planning…" chip.
   - `fs:` — filesystem watcher (`changed`).
   - `project:` — one-shot project-level signals (`scan_done`).
2. Add the payload type to `frontend/src/types.ts`.
3. Add a listener in `App.tsx` and route it through a store action —
   never a local `useState`.

The ring-buffer trim is done inside the store action, so even a runaway
emitter cannot grow `events` beyond 500 entries.

---

## 5. Key decisions (and why)

| Decision | Where | Why |
|---|---|---|
| **Zustand over Redux / Context** | `store.ts` | Audit §7.6 asked for state centralisation without the boilerplate tax. Zustand's selector model re-renders only consumers of changed slices, which mattered once the Debug pane grew to 500 entries. |
| **Do not virtualise Chat** | `Chat.tsx` (negative space) | Streaming mutates the last bubble's height every token, Thinking blocks expand/collapse, and sessions stay ≤ 200 messages. Virtualisation would cost more than it saves at that size. Documented in PR #7 notes so we don't re-litigate. |
| **Virtualise only the Debug pane** | `Execution.tsx` | Append-only, bounded at 500, homogenous row renderer. Perfect fit. |
| **Provider mode is per-app, models are per-role** | `settings.rs` | Captures the realistic hybrid case (cloud planner, local execute) without multiplying config surface area. |
| **`call_model` is the only entrypoint** | `ai.rs` | Keeps provider routing, `ai:step` metadata, and fallback logic in one place. All previous direct `stream_*` callers were migrated in PR #1 Phase 2. |
| **10 s health probe timeout** | `probe_*` | Ollama cold-start on first probe takes 3-8 s. 10 s is the smallest timeout that doesn't produce spurious "offline" states on laptops. |
| **5xx retry, 4xx fail** | `stream_openrouter` | 5xx is transient (OpenRouter had visible regional outages). 4xx is config (bad key, bad model name) and must surface to the user. |
| **`pushError` auto-opens Debug** | `store.ts` | The pre-store code had a scattered `if (event.kind === "error") setDebugOpen(true)` in multiple listeners. Centralising it removed a class of "error happened, user didn't see it" bugs. |
| **Self-host fonts** | `main.tsx` | Offline installs still get the intended rendering. No runtime CDN dependency. Bundle cost (~30 kB CSS + woff2 cached separately) is worth it. |
| **Step events live outside the virtual list** | `Execution.tsx` | They mutate in place (`running → done/failed`) and their count is small (≤ the plan length). Virtualising them would thrash react-window's height cache on every step status change. |
| **Task #5 loading skeletons deferred** | Phase 5 plan | Empty-states are explicit copy, no FOUC complaint observed, and a blanket skeleton system would be cosmetic work we'd rather spend on real UX gaps. |

---

## 6. Known constraints / trade-offs

- **`src/` is read-only.** The top-level `src/` tree is the recovered
  snapshot and is not wired into the desktop build. Do not modify it
  and do not import from it.
- **Bun is the runtime for the frontend workspace** (`desktop/frontend`).
  Use `bun install`, `bun run dev`, `bunx tsc`, `bunx vite build`. Do
  not switch to npm / pnpm silently; lockfiles and hoisting differ.
- **Tauri commands are the only IPC.** The frontend cannot touch the
  filesystem directly. Every new capability starts with a new tauri
  command in `lib.rs` and a typed wrapper in `frontend/src/api.ts`.
- **The sandbox is `fs_ops::resolve`.** All paths flowing through
  tools go through it. A new tool that needs filesystem access must
  resolve through it, not through `std::path` helpers.
- **Chat message count is practically bounded by the session.** The
  store keeps messages in memory only — there is no persistence of
  chat history across restarts. Don't assume long-term history.
- **`events` is bounded at 500.** Long autonomous runs drop the oldest
  events. The count header goes `500+` when at cap; if you need a
  larger bound add a constant and audit memory footprint, do not
  remove the cap.
- **Single running goal at a time.** `AppState.goal_running` RAII
  guard rejects concurrent `start_goal` calls. Don't try to parallelise
  goals in the controller without rethinking trace / breaker state.
- **Reviewer is optional but recommended.** Disabling it (in Settings)
  skips the `OK: / NEEDS_FIX:` path entirely — retries then become
  useless because they have no feedback to act on.

---

## 7. Critical files (quick map)

### Rust (`desktop/src-tauri/src`)

| File | What it owns |
|---|---|
| `lib.rs` | Tauri command surface (`open_project`, `run_chat_turn`, `start_goal`, `cancel`, `probe_*`, `save_settings`, …). |
| `controller.rs` | `start_goal`, `plan_goal`, per-task retry/timeout, circuit breaker, task archival. |
| `ai.rs` | `Role`, `Provider`, `resolve_provider`, `call_model`, `stream_openrouter`, `stream_ollama`, the chat turn runner. |
| `tools.rs` | `read_file`, `write_file`, `list_dir`, `run_cmd`. Subprocess tree-kill. |
| `fs_ops.rs` | `resolve` sandbox; path normalisation. |
| `settings.rs` | `Settings` struct, `ProviderMode`, per-role model slots, persistence. |
| `cancel.rs` | `CancelToken`, `CancelReason`, tree-kill helpers. |
| `tasks.rs` | `active_task_tree`, `task_history`, per-task `Trace`. |
| `watcher.rs` | FS change watcher → `fs:changed` event. |

### TypeScript (`desktop/frontend/src`)

| File | What it owns |
|---|---|
| `App.tsx` | Composition root. Mounts panes, wires tauri listeners to store actions, runs health probes. |
| `store.ts` | Zustand store (`useAppStore`, `EVENTS_CAP`). |
| `api.ts` | Typed wrappers around every tauri command. The *only* place `invoke()` is called. |
| `types.ts` | Shared TS types (`ChatMessage`, `ExecutionEvent`, `StepEvent`, `AgentRole`, `ProviderMode`, …). |
| `main.tsx` | React root. Font imports. Don't add logic here. |
| `styles.css` | All styles. CSS variables at `:root`, component sections delimited by `/* ---------- … ---------- */`. |
| `components/Chat.tsx` | Three-tier message list, classifier, streaming input. |
| `components/ThinkingBlock.tsx` | Collapsible streaming thinking block. |
| `components/FinalAnswerBubble.tsx` | Prominent final-answer bubble. |
| `components/SystemAction.tsx` | Inline tool-call / tool-result renderer. |
| `components/TaskPanel.tsx` | Goal + task tree, status icons, live timers, progressbar. |
| `components/Execution.tsx` | Debug pane: agent timeline + virtualised event list. |
| `components/Settings.tsx` | Settings modal (provider mode, per-role models, Test OpenRouter). |
| `components/Explorer.tsx` | Explorer pane. |

### Docs

| File | Purpose |
|---|---|
| `README.md` | User-facing setup + first run + workflows. |
| `docs/USAGE.md` | Longer usage guide (traces, autonomous mode, good vs bad goals). |
| `docs/SCENARIOS.md` | Five real end-to-end scenarios. |
| `docs/PROVIDER_ROUTING.md` | Provider matrix, failure handling, cost/perf. |
| `docs/EVALUATION.md` | Evaluation methodology + known-limits notes. |
| `DEVELOPMENT_PLAN.md` | Phase-by-phase roadmap with status (Phases 2–5 done). |
| `FULL_SYSTEM_AUDIT.md` + `FULL_SYSTEM_AUDIT_ADDENDUM.md` | Original technical audit. |
| `OCCD_FULL_AUDIT.md` | Product-architect audit. |
| **this file** | Structured entry-point for future work. |

---

## 8. Working conventions

- **Edit this file** whenever you make a non-trivial architectural
  change, add a slice to the store, add an event, change provider
  routing, or change a major UX affordance. Don't wait for a docs pass.
- **One PR per phase / topic.** The Phase 2 → 5 PRs were all single-topic,
  which made Devin Review and human review fast. Keep doing that.
- **Typecheck and build before push.** `bunx tsc --noEmit` +
  `bunx vite build` for the frontend; `cargo check` (from
  `desktop/src-tauri/`) for the backend. CI is thin on this repo, so
  local verification is load-bearing.
- **No `gh` CLI.** Use the `git_pr` / `git` tools for PR work so PR
  templates, session metadata, and preview URLs are handled
  automatically.
- **Never modify `src/`** (research snapshot), and never commit
  secrets. `.env` / `credentials.json` / `openrouter` keys stay out of
  the tree.
- **Do not amend commits or force push shared branches.** Use new
  commits; feature branches can use `--force-with-lease` if you need
  to rebase, but `main` is never force-pushed.

---

## 9. Scenario A — Local hello-world validation findings

> Captured during a real usage session on 2026-04-20, provider mode =
> `Local`, executor model = `llama3.2:1b` (Ollama, running on `:11434`),
> fresh project directory `/home/ubuntu/oc-test-run`. Goal submitted:
> *"Create a file HELLO.md in the project root with the single line
> 'Hello from Open Claude Code.', then run `ls -1` and confirm HELLO.md
> is listed."*
>
> Scope note: every finding below is a **real observation from driving
> the live Tauri binary**, not a theoretical review. Each is paired with
> a concrete file/line reference when applicable.

### 9.1 What worked (confirms earlier phases)

- **Provider/model badge metadata (Phase 2) is wired end-to-end.** Every
  agent bubble in the Chat pane rendered a pair of pills: the role
  (`PLANNER` / `EXECUTOR` / `REVIEWER`) plus `OLLAMA llama3.2:1b`. No
  `openrouter` badge ever appeared while in Local mode — `ai:step`
  metadata + `resolve_provider` are honest.
- **Debug pane auto-opens and populates correctly.** As soon as the goal
  started, the right-hand Debug pane opened on its own and its Agent
  Timeline streamed the per-role state transitions (`PLANNER planning →
  ✓`, `EXECUTOR step 1 → ✓`, `REVIEWER reviewing → review skipped`).
  The event counter advanced monotonically (0 → 12 across the run).
- **Elapsed-time counters on the thinking blocks work.** Each collapsed
  agent block displayed a stable duration once its step ended (`2m17s`,
  `2m00s`, `1.4s`), matching the real wall-clock time spent in that
  step. `<ref_snippet file="desktop/frontend/src/components/ThinkingBlock.tsx" lines="1-40" />`
  is the source of truth for this.
- **SystemAction tier 3 rendering fires on lifecycle transitions.** The
  cancel action surfaced as a single muted pill
  `• [executor] error: cancelled: goal`, not a full bubble. Phase 3's
  three-tier hierarchy is behaving as designed for system events.
- **Ollama health probe is honest.** The top-right executor badge
  (pre-PR-#10 labelled `• ollama online`, now `• executor ready` — see
  the [Health probes](#health-probes) section for the PR #10 rename)
  was green while Ollama was up and flipped to warning when the daemon
  was restarted mid-session. `probe_ollama` works.

### 9.2 Critical findings (block the golden path)

#### F-1. `check_planner` lies when `provider_mode = local`

- **Symptom.** Topbar permanently shows `• planner off` (red dot) even
  though the planner is running successfully via Ollama. The badge
  colour misleads users into thinking the app is misconfigured.
- **Root cause.** <ref_snippet file="desktop/src-tauri/src/ai.rs" lines="930-934" /> —
  `check_planner` returns `Ok(!openrouter_api_key.is_empty())`.
  It has no awareness of `ProviderMode`; in Local mode the correct
  answer is "planner is reachable iff Ollama is reachable and an
  ollama_model is configured for the planner role".
- **Fix direction.** Route `check_planner` through
  `resolve_provider(settings, Role::Planner).primary` and probe
  reachability of whichever provider that resolves to. Mirror the same
  pattern for `check_executor` and `check_reviewer`.
- **Severity.** High — visually scary, but does not actually block
  execution. Still costs trust on first run in Local mode.

#### F-2. TaskPanel stays on "Running…" with zero task rows for the entire planning phase

- **Symptom.** After clicking `Start goal`, the Goal & Tasks pane shows
  only the placeholder copy ("The autonomous task engine will decompose
  your goal into tasks and execute them in order. You will see each
  task's status here.") for the full 2+ minute planning phase. No
  spinner, no "planning…" chip, no partial tasks. Tasks only appear
  **after the plan JSON finishes streaming or the goal is cancelled**.
- **Root cause hypothesis.** The planner's streaming output is only
  surfaced to the TaskPanel after `parse_plan_json` succeeds in
  `controller.rs`. Streaming partial JSON is rendered as a Chat bubble
  instead of being reflected as "planner is thinking" in the task pane.
- **Fix direction.** Emit a synthetic status event the moment
  `start_goal` begins (before planner streaming starts): `{ kind:
  "planning", text: "Planner is drafting the task list…" }` → TaskPanel
  shows a dedicated `⋯ planning` chip with a live counter. The Chat
  planner bubble can remain but is no longer the only signal.
- **Severity.** High. In Local mode on a small CPU, planning can take
  2+ minutes and the user has zero feedback that anything is
  progressing.

#### F-3. Cancelled goal shows 100% green progress bar

- **Symptom.** After clicking `Cancel`, the TaskPanel summary line
  reads `• CANCELLED · 2/2 · 100%` with the progress bar fully filled
  in the success colour.
- **Impression.** Visually indistinguishable from a successfully
  completed goal at a glance.
- **Fix direction.** Progress bar should count only `✓ done` tasks in
  the numerator. Cancelled/skipped tasks should not contribute to "100%"
  and the bar fill colour must key off overall state
  (`cancelled → var(--amber)`, `failed → var(--red)`,
  `done → var(--green)`).
- **Severity.** Medium. Mis-communicates a cancellation as a success.

#### F-4. `llama3.2:1b` cannot drive the executor — no tool-calls emitted

- **Symptom.** The executor bubble streams yet another
  `{ "tasks": [ … ] }` JSON (a planner-style decomposition) instead of
  tool-call markup. Reviewer then logs
  `review skipped (unparsed)`. Nothing writes to disk — `HELLO.md` does
  not exist afterwards.
- **Not a bug per se.** This is a *model capability* floor: the 1 B
  parameter model is too small to reliably follow the tool-call prompt.
  The system behaves correctly given bad output (reviewer detects
  "unparsed" and bails instead of pretending to succeed).
- **But it is a product problem.** The Settings UI lets a user pick any
  Ollama tag with no guidance. A user who picks `llama3.2:1b` because
  it's small will hit a quiet, confusing dead end.
- **Fix direction.**
  1. Add a Settings-time warning when the chosen
     `ollama_model` has ≤ 3 B parameters (heuristic by tag name) for
     any role other than Reviewer.
  2. Make the "review skipped (unparsed)" annotation louder — bubble it
     up as a `SystemAction` with copy like *"Executor output could not
     be parsed as tool calls. Try a larger executor model (≥ 7 B)."*
  3. Document a tested minimum in `docs/PROVIDER_ROUTING.md`
     ("executor: ≥ 7 B; reviewer: ≥ 3 B; planner: ≥ 3 B").
- **Severity.** High. Without it, "Local mode with any Ollama model"
  is a promise the product cannot keep.

#### F-5. Phase 3 `classifyMessages` misclassifies planner output as FinalAnswer during streaming

- **Symptom.** While the planner JSON is still streaming, the bubble
  renders as a blue `ANSWER` `FinalAnswerBubble`. Once a later agent
  bubble arrives (executor / reviewer), the earlier planner bubble is
  retroactively reclassified to a `ThinkingBlock`. The transition is
  visually abrupt.
- **Root cause.** <ref_snippet file="desktop/frontend/src/components/Chat.tsx" lines="40-90" /> —
  `classifyMessages` picks "the last non-reviewer assistant message in
  the turn" as `final`. During planning that's the planner itself.
- **Fix direction.** Two options, pick one:
  - **(a)** If `m.streaming_role === "planner"`, force tier =
    `thinking`, regardless of whether it's the last bubble in the turn.
    Cheap and targeted.
  - **(b)** Only mark a bubble `final` once its step has ended
    (`done: true` on the `ai:step` event). Cleaner but requires
    threading `done`/`running` state into the classifier.
- **Severity.** Medium. Not broken at rest, only during streaming. But
  it's the first thing a new user sees, so it sets the tone.

### 9.3 Process / workflow findings

#### F-6. The Tauri process is killable from the dev shell

- **Symptom.** Running any command in the same interactive Bash shell
  that launched `open-claude-code-desktop` (even a trivial `pgrep`)
  occasionally caused the app to disappear. Relaunching with
  `nohup … &; disown` in a **dedicated** shell was stable across the
  rest of the session.
- **Implication for this file.** Any future validation run must launch
  the app via its own shell id (and strongly prefer `nohup` + `disown`)
  and never reuse that shell for probes. Not a product bug, but a real
  validation hazard.

#### F-7. Planning is slow on CPU-only Ollama (`llama3.2:1b`)

- **Observation.** On this VM (no GPU), planning a simple two-step
  goal took ~2m17s wall-clock. The three-agent loop's end-to-end worst
  case on `llama3.2:1b` easily exceeds 5 minutes even when the model
  actually cooperates.
- **Implication.** The goal-level `goal_timeout_secs` default of
  `3600` is fine, but the per-task `task_timeout_secs` default of
  `600` is tight for slow local models. A first-run user with a small
  Ollama model may see spurious task timeouts. Consider surfacing a
  "Slow local model?" auto-tune that raises timeouts if the planner
  roundtrip exceeds 60 s.

#### F-8. Last-project is not persisted across app restarts

- **Symptom.** Closing and relaunching the binary returns to
  `no project`. The user has to re-run "Open project…" every time.
- **Fix direction.** Persist `projectDir` in `settings.rs` (or a tiny
  `last_project.json` cache) and restore it on boot. Pair with a
  recent-projects menu in the topbar for parity with every IDE.
- **Severity.** Low. Pure friction, but a friction every run.

### 9.4 Positive UX observations (worth keeping)

- The **two-column dense layout** (Goal + Tasks narrow, Chat wide,
  Debug collapsible) held up well at 1024×768. Nothing overlapped,
  nothing had to be scrolled to be found.
- The **topbar status badges** (`planner …` / `ollama …`) give
  at-a-glance health without a dedicated "status" screen. Once F-1 is
  fixed, these become strictly useful.
- **Phase 4 task rows look right when they do render**: status icon +
  short task name + `<1s` elapsed + `SKIPPED` chip on cancel. The
  design is sound; the open problem (F-2) is about **when** they
  appear, not what they look like.

### 9.5 Net-net verdict on Scenario A

- **Status.** Scenario A did **not** complete its golden path on
  `llama3.2:1b`. HELLO.md was never written; the executor never emitted
  a `write_file` tool call.
- **What we nonetheless proved.** Phase 2 (provider routing, badges),
  Phase 4 (TaskPanel visual language once rendered) and Phase 5.2
  (Debug pane virtualised timeline, ring buffer, Zustand store) are
  all behaving as designed. The bottleneck is the **model**, not the
  app's plumbing.
- **What we disproved.** The unqualified claim that the system is
  "coherent, understandable, and usable" in *every* Local
  configuration. With a sub-3 B executor it is coherent (no crashes),
  but not usable — there is no disk-side result to show for five
  minutes of compute.
- **Next validation step.** Re-run Scenario A with
  `ollama_model = qwen2.5-coder:7b` (or `llama3.1:8b`) before
  attempting Scenarios B/C/D/E. If the 7 B tier passes the golden
  path, fixes F-1 / F-2 / F-3 / F-5 become the shortest path to a
  trustworthy first-run experience on Local mode.

### 9.6 Follow-up work opened by this session

- [x] **Fix F-1** — `check_planner` / `check_executor` now route
      through `resolve_provider` and probe the correct backend per role;
      the topbar executor badge is re-labelled provider-neutrally
      ("executor ready / off") instead of always saying "ollama online /
      offline" regardless of `ProviderMode`.
- [x] **Fix F-2** — `controller::start_goal` now emits
      `goal:planning` (phase `"scanning"`, then `"planning"`) and
      `goal:planning_done` around the pre-execution window. App.tsx
      listens and stores the phase on `useAppStore.goalPlanning`;
      TaskPanel renders a pulsing "Scanning project… / Planner
      drafting task list…" chip until the first task is parsed, so the
      pane is no longer silent during the 2+ minute plan phase on
      small local models.
- [x] **Fix F-3** — the task progress bar now carries a
      `task-progress-bar-<runState>` modifier class and recolours its
      fill to amber (`cancelled`) or red (`failed` / `timeout`); only
      the success path keeps the original green. The `2/2` counter
      still reflects tasks *processed* (done + failed + skipped) so the
      numeric semantic is unchanged.
- [x] **Fix F-5** — `classifyMessages` now explicitly excludes
      `streaming_role === "planner"` from the final-answer candidate
      slot (previously only reviewer was excluded), so a streaming
      planner bubble is never transiently tagged as `FinalAnswer`.
- [x] **Fix F-8** — `Settings` gained a `last_project_dir: Option<String>`
      field with focused `set_last_project_dir` /
      `get_last_project_dir` Tauri commands. App.tsx calls the setter
      from the `Open project…` flow and, on boot, reads the getter and
      auto-opens the saved directory if it still exists on disk
      (silently skipped if the dir has since been deleted or moved).
- [x] **Improve F-4** — `Settings` gained a `modelLooksSmall`
      heuristic (case-insensitive match on `1b`/`1.5b`/`2b`/`3b`
      tag suffixes) that renders an amber advisory under the
      Planner / Executor model fields recommending
      `qwen2.5-coder:7b` or `llama3.1:8b`. `ai.rs` now emits a new
      `ai:executor_unparsed` event at the two failure paths
      (executor iteration-0 with zero tool_calls, and reviewer
      `ReviewVerdict::Unknown`); Chat.tsx listens and renders a
      clickable SystemAction (`tone="warn"`, ⚠ icon) that opens
      Settings on click. `docs/PROVIDER_ROUTING.md` §6a documents
      the tested-minimum model sizes per role (executor ≥ 7 B;
      planner, reviewer ≥ 3 B). Also rolled in two PR #11 Devin
      Review fixes: `styles.css:760` `var(--font-mono)` →
      `var(--mono)` (the correct variable), and
      `set_last_project_dir` now clones → mutates → `save()` →
      swaps, matching the established save-then-update pattern so
      a failed disk write leaves in-memory and on-disk settings in
      sync.
- [ ] **Retry Scenario A on `qwen2.5-coder:7b` or `llama3.1:8b`**
      before moving on to Scenarios B–E.

---

## 10. Audit After PR #12 — user-pushed changes on `main`

Between the `F-4 model-capability guardrail` PR (#12, SHA `2f3f334`) and
the `scope warn_action dedup to current turn` merge (#15, SHA
`e957ed2`), `origin/main` received changes from two very different
sources:

1. **Reviewer-driven fixes** landed as normal PRs (#13, #15). These were
   scoped, reviewed, and documented in §§4, 8, 9 as they shipped.
2. **Four commits pushed directly by the maintainer** on top of #12
   without passing through the planner → PR → Devin-Review → merge
   workflow:
   - `2002e77` — `feat(desktop): modern model preset pills + defaults + OpenRouter catalog matching`
   - `fdb8346` — `Settings UI overhaul: OpenRouter model browser with All/Free tabs, provider cards layout, removed preset buttons`
   - `7033ad0` — `desktop: multi-terminal sessions + resizable bottom panel`
   - `d52eb98` — `fix: terminal component fixes — add onRunningChange prop, handle runningTerminals in dependency array`

   These introduced:
   - a new **`failures_log` store slice** (`desktop/frontend/src/store.ts`
     +23 lines) with cap-50 ring buffer, newest-first sort, and an
     auto-open-debug side effect on `pushFailure`
   - new **Tauri commands** `load_failures_log` / `clear_failures_log`
     / `run_cmd_stream` / `terminal_kill` and a new `terminal_pids:
     AsyncMutex<HashMap<String, u32>>` slot on `AppState`
   - new UI: `components/Terminal.tsx` (160 lines), `components/TerminalManager.tsx` (144 lines), an inline `FailuresPanel` in `App.tsx`, and a three-tab bottom panel (`debug` / `terminal` / `failures`) with a resizable divider
   - a major **Settings pane overhaul** (`components/Settings.tsx` grew from ~480 → 1221 lines): the preset buttons were removed, OpenRouter models are now browsed via an All/Free tabbed catalog fed by `OpenRouter_Categorized_Models.csv` (a new 684-line data file committed under repo root), per-provider cards replace the previous flat layout, and the F-4 `SmallModelWarning` / `modelLooksSmall` helpers from #12 were preserved as an exported function
   - a replacement of `tauri.conf.json`'s previously-empty
     `beforeBuildCommand` / `beforeDevCommand` hooks with
     `powershell -NoProfile -Command "Set-Location (Resolve-Path
     ..\\frontend); bun run build"` (and the equivalent `run dev`)
   - new `vite.config.ts` / `vite-env.d.ts` entries

This section is the honest post-facto code review of that batch —
the one that didn't happen before it hit `main`.

### 10.1 Confirmed bugs

#### A-1 (HIGH, multi-terminal effectively broken)

`TerminalManager.tsx:42` lists **`runningTerminals` in its `useEffect`
dependency array**. The effect body calls `setRunningTerminals(new
Set())` on line 34 and then hands out a fresh `terminalId` + resets
`tabs` on lines 39–41. The child `Terminal` component announces its
transitions back via `onRunningChange` (`Terminal.tsx:27`), which
calls `handleRunningChange` (`TerminalManager.tsx:64`) which itself
produces a **new `Set` reference** on every start/stop.

Consequence: the moment a user types a command and the child flips
`running → true`, the parent effect re-fires because its dependency
changed, it kills every running terminal (including the one that just
started), and replaces the tab list with a single fresh tab whose id
is new. The user sees their command die immediately after launch and
the tabs get blown away mid-run. The `d52eb98` commit message says it
is a "fix" for this flow, but the fix only added an `onRunningChange`
prop — it did not remove `runningTerminals` from the dependency
array. The regression stands.

Correct fix: the effect's sole legitimate dependency is `projectDir`.
Kill-on-project-change can use a `useRef<Set<string>>` that the
`handleRunningChange` callback updates imperatively, avoiding the
feedback loop.

**Status — fixed on the same branch as this update.** `TerminalManager.tsx`
now tracks the running set in `runningRef` (a `useRef<Set<string>>`),
mutated imperatively from `handleRunningChange`. The project-change
`useEffect` depends on `[projectDir]` only and snapshots the ref
before dispatching kills. `closeTab` reads and deletes from the ref
directly.

#### A-2 (HIGH, persisted failures log silently wiped on every open)

`tasks.rs:349-354` defines `clear_failures_log(project_dir)` which
overwrites the `failures_log` array in the **project's on-disk**
`PROJECT_MEMORY.json` (the backing store, not this file) with an
empty array and sync-saves.

`App.tsx` calls this on two paths:

- `openProject` (line 169): immediately after the user picks a
  directory, before loading anything
- the boot-time auto-restore `useEffect` (line 206): immediately after
  `setProjectDir(last)`

Both call sites are followed by `clearFailures()` on the in-memory
Zustand slice. The inline comment on line 167 says "Scope failures to
the newly-opened project. This intentionally resets the prior
project's failures view" — but the backend log is already
project-scoped (the file lives under the project directory). Clearing
the *newly-opened* project's log on open is exactly the opposite of
what persistence means: every time a project is reopened, the entire
history of prior failures for that project is destroyed before the
user even sees it. The `load_failures_log` call on line 135 races the
clear and will usually win on UI mount, but the on-disk record is
gone regardless.

The same race explains why `failures` in the bottom-panel Failures
tab generally looks empty across restarts despite the store slice and
`FAILURES_CAP = 50` (`store.ts:14`) being set up correctly.

Correct fix: remove both `clearFailuresLog` calls. The load path
already replaces the in-memory slice atomically via `setFailures`; no
disk clear is needed to "scope" anything, because the file backing
this project's failures only exists under this project.

**Status — fixed on the same branch as this update.** Both
`api.clearFailuresLog(...)` calls in `App.tsx` are gone. Only
`clearFailures()` (the in-memory slice reset) remains on the open and
restore paths; the persisted `failures_log` on disk survives reopens
and is replayed into the store via `setFailures` by the existing load
effect.

#### A-3 (HIGH, non-Windows dev & build broken out of the box)

`desktop/src-tauri/tauri.conf.json:6-9` now hard-codes:

```json
"beforeBuildCommand": "powershell -NoProfile -Command \"Set-Location (Resolve-Path ..\\frontend); bun run build\"",
"beforeDevCommand":   "powershell -NoProfile -Command \"Set-Location (Resolve-Path ..\\frontend); bun run dev\""
```

Before the user commits these fields were the empty string (the Tauri
flow expected the dev to run `bun run build` in `desktop/frontend`
themselves before `cargo tauri build`). The new values invoke
PowerShell, which is not present by default on Linux CI runners or on
a fresh macOS install, and uses back-slash paths that wouldn't resolve
cleanly on POSIX even if `pwsh` were installed. `bun run tauri dev`
and `bun run tauri build` therefore fail immediately on any
non-Windows machine — including the canonical dev environment this
repo is being developed in.

Correct fix: use a cross-platform invocation such as
`"beforeBuildCommand": "bun --cwd ../frontend run build"` (Bun is
already the declared runtime; `--cwd` is portable). The Windows
launcher script `scripts/run-snapshot.ps1` can keep its PowerShell
wrapping; `tauri.conf.json` should not.

**Status — fixed on the same branch as this update.**
`tauri.conf.json` now uses `bun --cwd ../frontend run build` and the
equivalent `run dev` for `beforeBuildCommand` / `beforeDevCommand`.
No shell is invoked, Bun resolves the path natively, and both hooks
work on Windows, Linux, and macOS.

#### A-4 (MEDIUM, tab close leaks child processes)

`TerminalManager.tsx` closes a tab purely by removing its entry from
`tabs` state. Neither `Terminal` nor `TerminalManager` calls
`api.terminalKill(terminalId)` on unmount / remove. `run_cmd_stream`
is a long-running Tauri command that keeps a child alive until the
process exits or `terminal_kill` is called, so closing a tab with a
running command leaves an orphan `sh -c …` (or `cmd /C …` on Windows)
running under the Tauri app's process tree until the app itself
quits.

Correct fix: `Terminal` should call `terminalKill` in its cleanup
function when `running` is `true` at unmount time. `TerminalManager`
should do the same for the tab being removed.

#### A-5 (MEDIUM, unbounded terminal output grows forever)

`Terminal.tsx:38-46` appends every `terminal:output` chunk to the
`lines` state array with no cap:

```ts
setLines((prev) => [
  ...prev,
  { id: lineIdRef.current++, stream: ..., text: p.data },
]);
```

A command that streams a lot (`yarn install`, `cargo build`, `find /`)
piles megabytes into a single React state array with dynamic-height
rows, with no virtualization. The Debug pane adopted a 500-event ring
buffer in Phase 5.1 (`EVENTS_CAP`) precisely because this pattern
jams the UI — the new Terminal did not pick up the lesson.

Correct fix: mirror the Debug pattern — a per-tab ring buffer (e.g.,
5 000 lines) and, longer-term, react-window virtualization driven by
`useDynamicRowHeight`.

#### A-6 (MEDIUM, Ctrl+C steals the copy affordance)

`Terminal.tsx:115-122` handles `e.key === "c" && e.ctrlKey` by
preventing default, calling `terminalKill` if `running`, and clearing
the input. The preventDefault runs regardless of whether there is a
text selection, so a user who selects terminal output and hits Ctrl+C
to copy instead loses their selection and — if a command is running
— kills the process.

Correct fix: only preventDefault + kill when `running` is true **and**
`window.getSelection()?.toString()` is empty. Otherwise let the
browser's default copy behaviour proceed.

#### A-7 (MEDIUM, `setRunning(false)` race between RPC return and event)

`Terminal.tsx:92` sets `running = false` immediately after
`await api.runCmdStream(...)` returns, and line 53 also sets it to
`false` on receipt of `terminal:done`. Tauri's event bus is async
relative to command replies, so the RPC usually returns before the
final `terminal:output` chunks are dispatched. The input becomes
enabled, the user can start typing, and late output from the previous
command lands in the output pane while they're drafting the next one.

Correct fix: do not touch `running` on the RPC reply path — let the
event-driven `terminal:done` be the sole authority, which is what the
Rust side already treats as the completion signal (it emits
`terminal:done` **after** draining stdout/stderr; see
`tools.rs:588-601`).

#### A-8 (retracted — original claim was factually wrong)

The original §10 shipped this item as "dedup comment in `ai::run_chat_turn`
drifted against current reality." That is **incorrect**. Devin Review
caught the error on PR #16 (review comment `BUG_pr-review-job-
0566225c97cb4a029817115da56bbe61_0001`). The `warn_action` dedup does
**not** live in the backend: `ai.rs` only *emits* the
`ai:executor_unparsed` event (twice — `ai.rs:1542` iteration-0 empty,
and `ai.rs:1767` reviewer `Unknown` verdict). The dedup is performed on
the **frontend** at `Chat.tsx:189-236`, inside the `ai:executor_unparsed`
listener, and the comment there (`Chat.tsx:191-211`) already correctly
describes the turn-scoped behaviour — it is neither outdated nor wrong.
No code or comment change is required. This item is retained here
(rather than deleted) so the error itself is part of the record: the
audit it sits inside is a review of unreviewed code, and the reviewer
earned the right to be reviewed back.

### 10.2 Confirmed behavioural regressions vs. PR #12

#### R-1 (LOW, OpenRouter preset buttons removed without a replacement
   migration hint)

`fdb8346` removed the per-role "preset" buttons that pre-populated
Planner / Executor / Reviewer with curated models. The new
OpenRouter browser is strictly more powerful, but users whose
workflow relied on "just hit the Claude preset" now need to type or
pick a model from a 684-entry catalog. There is no one-time tip,
default re-seed, or "pick a recommended set" affordance on the new
layout. Minor UX regression; worth a follow-up.

#### R-2 (LOW, `SmallModelWarning` now only renders under one of the
   three fields in Cloud mode)

In the pre-overhaul layout, the warning sat under Planner **and**
Executor fields unconditionally. In the overhauled layout it is
conditional on `provider` card visibility — the Ollama card's
`ollama_model` field gains the warning only in Local / Hybrid mode.
When a user picks a small model via the OpenRouter browser (e.g.
`meta-llama/llama-3.2-1b-instruct`), `modelLooksSmall` matches
correctly on the `executor_model` path at `Settings.tsx:920`, so the
guardrail still fires. Worth a manual scan before the next Scenario A
retry.

### 10.3 Memory, performance, and stability

- **P-1 (LOW)** `run_cmd_stream` uses two 512-byte buffers and
  `read` in a `tokio::select!`. If a process emits a UTF-8 sequence
  that straddles a 512-byte boundary, `String::from_utf8_lossy` will
  replace the split bytes with `U+FFFD`. The emitted `text` is lossy
  forever. For English-only output this is cosmetically fine; for
  programs that print Arabic (very likely in this repo's target
  audience), this will produce visible mojibake. `tokio::io::BufReader`
  with `read_line` or a streaming UTF-8 decoder would preserve
  multibyte characters.
- **P-2 (LOW)** `terminal_pids` is keyed by `terminal_id`, but there
  is no guard preventing two simultaneous `run_cmd_stream` invocations
  with the same id. The second `spawn` would overwrite the stored PID,
  and a `terminal_kill` would kill only the second child — the first
  becomes an orphan until process exit. Unlikely with current UI
  (input is disabled while `running`) but trivial to avoid with an
  `Entry::Vacant` check at insertion time.
- **P-3 (LOW)** The failures slice uses `[...fs, failure].slice(-cap)`
  on every push (`store.ts:pushFailure`). Correct behaviour, but on a
  burst of 100 failures this copies the whole array 100 times. For
  `cap=50` it's a non-issue; flagged only so the next reader knows
  not to grow `FAILURES_CAP` beyond a few hundred without switching
  to a ring-buffer data structure.

### 10.4 Security

- **S-1 (existing, not new)** `run_cmd_stream` is a direct-invocation
  Tauri command with no allow-list gating, documented in
  `tools.rs:11-14` as intentional: "the direct `run_cmd` Tauri command
  remains unrestricted so the UI's own Terminal/Explorer surfaces can
  run arbitrary commands with user intent." The multi-terminal
  feature now exposes this to **N concurrent invocations** from the
  same frontend. No new vulnerability per se, but the surface area
  for frontend-compromise scenarios (e.g. a prompt-injection that
  causes the assistant to write JSX into a bubble that slips past our
  plain-text rendering) scaled up N× without review.
- **S-2 (existing, not new)** `terminal_kill` by PID is guarded by
  `terminal_pids` ownership but does not verify that the PID still
  belongs to **our** spawned child before sending `SIGTERM` (Unix) or
  calling `taskkill /T /F` (Windows). On rapid PID recycling this
  could kill an unrelated process. `map.remove` happens *before* the
  kill syscall (`tools.rs:598-601`), so the window is small, but not
  zero. A defensive fix: capture the `Child` handle (not just the
  PID) and kill via `child.kill().await`.

### 10.5 Documentation drift — items that used to be in §4 / §6

The §4 architecture diagram and §6 Zustand slice inventory predate
all four user commits. The following live parts of the system are
now **undocumented** in this file:

1. `store.ts` gained `failures`, `setFailures`, `pushFailure`,
   `clearFailures`, and `FAILURES_CAP = 50`. Not in §6.
2. `App.tsx` gained `bottomTab: "debug" | "terminal" | "failures"`
   and `bottomPanelHeight` state. Not in §4.
3. Three new Tauri commands (`run_cmd_stream`, `terminal_kill`,
   `load_failures_log`, `clear_failures_log`) + one new `AppState`
   field (`terminal_pids`). Not in §4 or §7.
4. Two new event types (`terminal:output`, `terminal:done`). Not in
   §5 (event taxonomy).
5. `components/Terminal.tsx`, `components/TerminalManager.tsx`, and
   the inline `FailuresPanel` in `App.tsx`. Not in §7 (critical files
   map). `FailuresPanel` should live in its own file.
6. `OpenRouter_Categorized_Models.csv` (684 lines, ~60 KB) is now a
   runtime asset Settings depends on. Its shape, source, refresh
   cadence, and licence are not documented anywhere in the repo.
7. `components/Settings.tsx` roughly doubled in size; the provider-
   card layout, All/Free filter semantics, and the behaviour of
   `modelLooksSmall` against OpenRouter model ids (which tend to
   encode parameter counts as `…-1b-instruct`, `…-3b-…`, etc.) are
   not mentioned in §§4, 7, or 9.

Following the "update in the same PR as the change" rule from
`AGENTS.md` and `CLAUDE.md`, all of this should land alongside the
user's next real change to one of these files. This section flags
the drift so that update has a clear scope.

### 10.6 Ideas & follow-ups (non-blocking)

- **I-1** Surface `failures` as a first-class affordance: every row
  should expose a "Re-run this task" button that reopens the task
  tree and enqueues just that task with the persisted goal context.
  Today it is strictly read-only.
- **I-2** Terminal: ring buffer + ANSI colour renderer (`ansi_up` or
  similar) + `Ctrl+L` to clear + `ctrl+k` to search. Current terminal
  is roughly feature-parity with 1980 `cat`.
- **I-3** Settings: search box over the OpenRouter catalog (684
  entries is a lot), and persist the selected `All`/`Free` tab + any
  filter in `settings.json`. Current UI resets to defaults on every
  app launch.
- **I-4** Extract `FailuresPanel` from `App.tsx` to its own file under
  `components/`. App.tsx is now ~510 lines with three unrelated
  concerns (global state wiring, bottom panel, project lifecycle).
- **I-5** `modelLooksSmall` is exported but only used inside
  `Settings.tsx`. If the ai layer were to consult it at runtime (e.g.
  emit a once-per-run soft warning when the live executor model
  matches), the guardrail would no longer depend on the user ever
  opening Settings. This also closes the only remaining gap in F-4.
- **I-6** The two user commits `2002e77` + `fdb8346` are the same
  Settings UI rewrite split into two — the history is harder to
  bisect than it needs to be. Future `main`-direct pushes should
  squash before push, or come via PR.

### 10.7 Verdict

The provider-routing / UX stack built through PRs #1-#12 is still
correct end-to-end. The four user-pushed commits added useful
capability (persistent failures view, multi-terminal, a vastly better
model browser) but shipped with three **HIGH-severity** regressions
(A-1, A-2, A-3) that any serious Scenario A retry would have hit in
the first minute.

**Status update.** All three HIGH regressions are closed on the same
branch as this audit update (see the per-item `**Status — fixed…**`
notes in §10.1). The fixes were exactly as small as the audit
predicted — a dependency-array cleanup in `TerminalManager.tsx`, two
`clearFailuresLog(...)` deletions in `App.tsx`, and a one-line
`tauri.conf.json` change. Compile verification passed on all three
toolchains (`cargo check`, `bunx tsc --noEmit`, `bunx vite build`).
§10.1 A-8 was retracted after Devin Review correctly pointed out it
was looking in the wrong file.

The §9.6 item "Retry Scenario A on `qwen2.5-coder:7b` or
`llama3.1:8b`" is now the right next validation step, and the
`main` harness is no longer broken underneath it. The remaining §10.2
/ §10.3 / §10.4 / §10.5 items are real but not preconditions for the
retry — they will be addressed in dedicated follow-up PRs as the
retry itself surfaces which of them actually matter in practice.

---

## 12. Terminal Authority Invariant (Phase 0)

### Why it exists

The STRICT REFOCUS MODE directive mandates that **every side-effecting
AI operation go through the internal terminal** — the same surface the
user types into — so execution is visible and traceable (Devin /
Windsurf behaviour). Before Phase 0 the AI path was split in two:

* `run_cmd_stream` (Tauri command invoked from `Terminal.tsx`) emitted
  `terminal:output` events and showed up live in the Terminal pane.
* `execute_run_cmd_gated` / `execute_write_file_gated` (invoked by the
  Executor during `run_chat_turn` / `execute_task_with_retries`) ran
  through `run_cmd_impl` and `fs_ops::write_file` **silently** — their
  output was captured as a string, returned as a `tool_result`, and
  only ever reached the UI via the Debug pane. From the user's point
  of view the agent was operating in a dark room.

Phase 0 closes that gap.

### Invariant

> **Every AI tool invocation that has a side-effect emits
> `terminal:output` events to a single pinned terminal tab
> (`terminal_id = "agent-main"`).**

Tools in scope:

| tool         | emitted on | emitted to          |
|--------------|-----------|---------------------|
| `read_file`  | entry + `→ N bytes` summary | stdout |
| `list_dir`   | entry + `→ N entries` summary | stdout |
| `write_file` | `$ write_file <path> (N bytes)` / `✓ wrote <path>` | stdout |
| `run_cmd`    | full command banner + every stdout/stderr chunk streamed live + `[exit <code>]` | stdout/stderr |

The `tool_result` payload returned to the LLM is unchanged — the Agent
tab is a *tee*, not a replacement, so model reasoning is identical
before and after.

### Implementation (stable IDs, one helper)

Backend (`desktop/src-tauri/src/tools.rs`):

* `pub const AGENT_TERMINAL_ID: &str = "agent-main"` — single source of
  truth for the stream id.
* `pub(crate) fn emit_agent_line(app: &AppHandle, stream: &str, data: impl Into<String>)`
  — fires a `terminal:output { terminal_id, stream, data }` event. Every
  gated tool path funnels through this helper.
* `run_cmd_impl` now takes `app: Option<&AppHandle>`. When supplied it
  tees every 512-byte pipe chunk to `emit_agent_line` alongside the
  existing cancel/timeout plumbing — the returned `RunCmdResult` still
  carries the full `stdout`/`stderr` strings so callers that consume
  them (and the unit tests) are unaffected. The direct `run_cmd` Tauri
  command still passes `None` because the user-facing Terminal pane
  already streams via `run_cmd_stream`.

Frontend:

* `TerminalManager.tsx` seeds the tabs array with a pinned tab
  `{ id: "agent-main", title: "Agent", pinned: "agent" }` and preserves
  it across project switches (only user-driven tabs are recycled). The
  close button is suppressed for pinned tabs and `closeTab` short-circuits
  on `AGENT_TERMINAL_ID` as a defensive second gate.
* `Terminal.tsx` gained an `agentMode` prop. In agent mode the input
  row is not rendered (the tab is observer-only — goal cancel goes
  through the existing Cancel button, not `terminalKill`), and an
  idle-state hint renders before the first chunk arrives.
* `styles.css` adds the pinned-tab accent + live-dot styling under
  `.terminal-manager-tab-agent` / `.terminal-agent`.

### How it's verified

Phase 0 is intentionally **compile-only** — no UI testing is done on
this PR. The real evidence comes from Phases 1-6 (OpenRouter
validation) which will exercise the invariant live.

What ships with this PR:

* `cargo check` + `cargo test --lib tools::` (15/15 pass, including the
  cancel / tree-kill regression suite rewired through the new 5-arg
  signature).
* `bunx tsc --noEmit` clean.
* `bunx vite build` clean.

### User-facing guarantee

After Phase 0, starting a goal auto-focuses (if not already active)
the pinned **Agent** tab, and every tool invocation appears there in
order, with stdout/stderr distinguished, and an `[exit N]` line that
closes each `run_cmd`. There is no code path in the Executor that can
touch disk or spawn a process without the Agent tab seeing it.

### 12.1 Rendering model (post-PR #19 review fixes)

Phase 0 as originally merged in PR #19 rendered a **single** `Terminal`
component in `TerminalManager` and swapped its `terminalId` prop on tab
switch. Devin Review flagged two bugs against this approach:

1. **🔴 Lost agent events.** `Terminal.tsx`'s `terminal:output` listener
   filters by `terminalId`, so it only captured events for whichever
   tab was currently active. Any tool call emitted for
   `terminal_id = "agent-main"` while the user was looking at a user
   terminal was silently dropped — a direct violation of the invariant
   above.
2. **🟡 Numbering glitch.** `addTab` computed the new title from
   `prev.length + 1`, which included the pinned Agent tab, so the
   first user-added tab was labelled "Terminal 3" instead of
   "Terminal 2".

Fix (shipped on top of Phase 0):

* `TerminalManager.tsx` now renders **one `Terminal` instance per tab**
  wrapped in a `.terminal-manager-panel` div, and hides inactive panels
  with `display: none` (role `tabpanel`, `aria-hidden` on inactive
  panels). Each `Terminal` keeps its own mounted `terminal:output`
  listener and its own `lines` buffer across tab switches, so Agent
  events are captured continuously regardless of which tab is visible.
* `addTab` now counts `prev.filter(t => !t.pinned).length + 1` — pinned
  tabs never inflate the user-visible numbering.

The invariant in §12 is only actually true with these fixes in place.
When you re-read this section, treat §12 and §12.1 as a single unit:
the rendering model is **part of** the Terminal Authority contract,
not an implementation detail.

---

## 13. OC-Titan Phase 1.A — Codegen envelope (JSON schema enforcement)

> **V6 Final Directive §I.1 (Deterministic Output Protocol).** Every
> turn that generates project files must emit a single JSON envelope
> that validates against the canonical schema *before* any file lands
> on disk. No prose, no markdown fences, no free-text outside the
> envelope. Phase 1.A is JSON enforcement only; the compiler gate
> (`tsc --noEmit`) lands in Phase 1.B as PR-B.

### Envelope shape

Canonical schema lives at
`desktop/src-tauri/src/schemas/codegen_envelope.json` (JSON Schema
Draft 2020-12) and is embedded at compile time via `include_str!`. The
shape:

```json
{
  "files": [{ "path": "<sandbox-relative POSIX>", "content": "…" }],
  "run_cmd": "<optional single line>"
}
```

Hard invariants enforced by schema + Rust path check:

- `files.minItems = 1`, `maxItems = 256`.
- `path` has no leading `/`, no NUL, no `..` traversal, no Windows
  drive letter (`C:\…`). Pattern check is first-line defence; Rust
  `validate_path()` in `codegen_envelope.rs` is the fallback for
  cases the regex can't express.
- `content.maxLength = 4 MiB`, `run_cmd.maxLength = 2048`.
- `additionalProperties = false` at every level.

### `JsonMode` enum (`ai.rs`)

Replaces the old `json_mode: bool` flag. Three-valued:

| Variant             | When                               | Effect                                                                                     |
| ------------------- | ---------------------------------- | ------------------------------------------------------------------------------------------ |
| `Off`               | Executor tool loops, reviewer      | Free-form prose + tool calls; tool schema is shipped with `tool_choice: "required"`.       |
| `PlannerPlan`       | `controller::plan_goal`            | OpenRouter `response_format: json_object` / Ollama `format: json`; tool schema dropped.     |
| `CodegenEnvelope`   | `ai::run_codegen_envelope_turn`    | Same JSON-mode wire flags as the planner, but parsed against `codegen_envelope.json`.       |

`tool_choice: "required"` (upgrade from `"auto"`, per
OPENROUTER_VALIDATION_REPORT §8.6) is only applied when the turn ships
a tool schema AND `JsonMode` is `Off`. Pure-text reasoning turns carry
no tool schema and therefore continue to use the model default.

### Codegen turn flow

1. `controller::run_codegen_envelope` (Tauri command) →
   `ai::run_codegen_envelope_turn(app, state, project_dir, user_request, history, autonomous_confirm)`.
2. The function frames the user request with `CODEGEN_ENVELOPE_PROMPT`
   (see `ai.rs`) and invokes `run_chat_turn(..., JsonMode::CodegenEnvelope)`.
3. The assistant text is fed through
   `codegen_envelope::parse_and_validate`. On success the caller
   receives a `CodegenEnvelopeTurn { envelope, raw, steps }`.
4. On a `ParseError`, the turn re-prompts **once** (the `V6 §V.1`
   JSON-repair loop; constant `CODEGEN_ENVELOPE_REPAIR_RETRIES = 1`)
   using `ParseError::to_feedback()` — a bullet list with RFC 6901
   JSON Pointers so the model can patch the exact failing field. A
   second failure surfaces as an `envelope.violation` event and the
   command returns `Err(..)`; there is no silent fallback.
5. On success the caller hands the envelope to
   `controller::apply_codegen_envelope`, which iterates files and
   lands them through `fs_ops::write_file` (sandbox re-checks every
   path). Each write emits a `codegen.envelope.write` step and the
   final `codegen.envelope.applied` step carries file counts.

### `ai:step` events (Terminal Authority, V6 §VI.1)

All envelope-lifecycle transitions stream through the canonical
`ai:step` channel so the Chat / TaskPanel / Debug tiers see them
identically:

| `label`                        | `status`  | Fields (selection)                      |
| ------------------------------ | --------- | --------------------------------------- |
| `codegen.envelope.request`     | `running` | —                                       |
| `codegen.envelope.ok`          | `done`    | `files`, `repaired?`                    |
| `codegen.envelope.violation`   | `failed`  | `feedback`, `attempt?`                  |
| `codegen.envelope.retry`       | `running` | `attempt`                               |
| `codegen.envelope.write`       | `done` / `failed` | `path`, `bytes`, `reason?`      |
| `codegen.envelope.applied`     | `done` / `failed` | `files`, `failed`, `run_cmd`    |

`run_cmd` is **captured and surfaced only** in Phase 1. Execution is
deferred to Phase 2 behind the command-risk security gate (V6 §VII.2).

### What is still TODO (intentionally deferred)

- **Phase 1.C:** Dependency graph check — validate imports vs
  `package.json` before the envelope is applied (V6 §I.6).
- **Phase 2:** `run_cmd` auto-execution under the command-risk gate.

---

## 14. OC-Titan Phase 1.B — Compiler gate (`tsc --noEmit`)

> Status: **implemented, PR-B** on
> `devin/1776801799-oc-titan-phase1b-tsc-gate`. Phase 1.B wraps the
> Phase 1.A envelope pipeline with an isolated TypeScript compile
> before files ever reach the real project tree. Satisfies V6 §I.5
> ("Compiler Gate — mandatory") and V6 §V.2 ("Compiler loop — fix
> all TypeScript errors automatically").

### Module map

- `src-tauri/src/compiler_gate.rs` — the whole gate. Exports
  `skip_policy`, `prepare_scratch`, `detect_toolchain`, `run_tsc`,
  `diagnostics_to_feedback`, `rewrite_paths_relative`, plus the
  `CompileOutcome` / `CompileDiagnostic` / `Scratch` / `ToolchainKind`
  types.
- `src-tauri/src/controller.rs::run_codegen_envelope` — the single
  Tauri entrypoint that now drives the full lifecycle (Phase 1.A
  JSON repair → Phase 1.B compile gate → sandbox write).
- `src-tauri/src/settings.rs` — three new persistent toggles:
  `compiler_gate_enabled: bool` (default `true`),
  `max_compile_retries: u32` (default `2`),
  `tsc_timeout_secs: u64` (default `120`).

### Scratch lifecycle

For every envelope that makes it past Phase 1.A **and** trips
`skip_policy` (gate enabled + at least one `.ts`/`.tsx`/`.mts`/`.cts`
file), the gate:

1. Creates `<project>/.oc-titan/scratch/<uuid>/` and appends
   `/.oc-titan/` to `.gitignore` idempotently.
2. Copies `tsconfig*.json` + `package.json` + `package-lock.json` /
   `bun.lock` from the project root into the scratch dir (so the
   user's `tsc` config — `strict`, `moduleResolution`, `lib`, etc. —
   is honoured).
3. Symlinks `<project>/node_modules` into the scratch dir on Unix
   (Windows skips the symlink silently — the gate still runs, it just
   relies on whatever `tsc` can resolve without it).
4. Writes the envelope files into the scratch tree **before** they
   are written to the real project. This is the whole point: the
   model's output never pollutes the project sandbox unless `tsc`
   accepts it.
5. On any outcome (success, failure, timeout), the `Scratch` guard's
   `cleanup()` is awaited. `cleanup` refuses to delete anything that
   is not under `.oc-titan/scratch/` — defence-in-depth against a
   corrupted scratch path.

### Toolchain detection (V6 §I.5)

`detect_toolchain` probes sequentially with a 5 s timeout per probe:

1. `bun --version` — used only if `bun.lock` exists in the project
   root (avoids launching bun against non-bun projects). Invoked as
   `bun x tsc --noEmit -p <scratch>`.
2. `npx --version` **and** `node_modules/typescript` present in the
   project — invoked as `npx --no-install tsc --noEmit -p <scratch>`.
3. Global `tsc --version` — invoked as `tsc --noEmit -p <scratch>`.
   Emits an `ai:step` warning because a global `tsc` can drift from
   the project's declared version.

If no toolchain is found the controller emits a `compiler.skipped`
event with `reason: "no_toolchain"` and promotes the envelope as-is.
This preserves Phase 1.A behaviour on machines without a TypeScript
toolchain — the gate is a hard *upgrade*, never a hard *downgrade*.

### Outcome → action matrix

| `CompileOutcome` | Controller action | `ai:step` events emitted |
|---|---|---|
| `Ok` | Promote envelope | `compiler.ok`, `compiler.promoted` |
| `Errors` (attempt < max) | Reprompt executor with diagnostics | `compiler.errors`, `compiler.retry` |
| `Errors` (attempt == max) | Return `Err` to caller | `compiler.errors` |
| `Timeout` | Return `Err` — never retry | `compiler.timeout` |
| `skip_policy` hit | Promote envelope | `compiler.skipped` |

Max attempts = `settings.max_compile_retries + 1` (default = 3
total: one fresh + two retries). The repair prompt sent to the model
is built by `controller::build_compile_feedback_prompt` and contains
the **original** user request plus a bulleted diagnostic list keyed
by `path(line,col) TSxxxx: message`. Paths are rewritten through
`rewrite_paths_relative` first, so the model sees
`src/app.ts(12,4): TS2345: …` instead of the scratch UUID.

### Diagnostic parsing

`tsc --pretty false` is regex-parsed against:

```
(.+?)\((\d+),(\d+)\):\s+error\s+(TS\d+):\s+(.*)
```

Continuation lines (those that don't match the header pattern) are
appended to the previous diagnostic's `message`, so multi-line
diagnostics like `error TS2322: Type 'X' is not assignable to type
'Y'. Types of property 'z' are incompatible.` survive as a single
entry. The feedback list is capped at 50 diagnostics with a
`… (N more truncated)` suffix to bound prompt size.

### Error semantics (Devin Review follow-up)

Two bugs surfaced by Devin Review on PR-A are fixed in the same PR
as Phase 1.B (both live in the same codegen path):

1. **`tool_choice: "required"`** in `ai::stream_openrouter` reverted
   to `"auto"`. The executor loop's terminate condition is "model
   returns zero tool calls", which `"required"` forbids; it was
   causing the loop to exhaust `max_iterations` with an empty
   assistant message. Deterministic-output enforcement now lives
   *only* in `JsonMode::CodegenEnvelope` + the envelope validator,
   not in `tool_choice`.
2. **Partial-commit invariant.** `run_codegen_envelope` now checks
   `result.failed` after `apply_codegen_envelope` and returns
   `Err(..)` with a per-file failure summary whenever anything was
   rejected by the sandbox. `AppliedEnvelope` still carries the
   partial `applied` list for telemetry, but the UI never sees a
   silent partial-write success.

### Events (Terminal Authority, V6 §VI.1)

Every compile lifecycle transition emits a structured `ai:step`:

| `label` | `status` | Always carries | Context |
|---|---|---|---|
| `compiler.scratch_ready` | `running` | `attempt`, `uuid`, `dir` | After `prepare_scratch` |
| `compiler.running` | `running` | `attempt`, `toolchain`, `timeout_secs` | Before `run_tsc` |
| `compiler.ok` | `done` | `attempt`, `toolchain` | Compile succeeded |
| `compiler.errors` | `failed` | `attempt`, `toolchain`, `diagnostic_count`, `diagnostics` | Compile failed |
| `compiler.retry` | `running` | `attempt`, `max_attempts` | Before the next envelope turn |
| `compiler.timeout` | `failed` | `attempt`, `toolchain`, `after_secs` | Timeout tripped |
| `compiler.skipped` | `done` | `reason`, `attempt` | Gate disabled / no ts files / no toolchain |
| `compiler.promoted` | `done` | `files`, `run_cmd` | After `apply_codegen_envelope` succeeded |
| `compiler.scratch_failed` | `failed` | `reason` | `prepare_scratch` errored |

### Unit tests

`cargo test --lib compiler_gate` covers 15 cases grouped by
responsibility: TypeScript-file detection, skip policy (toggle + no
ts), diagnostic parsing (canonical, empty, continuation lines),
feedback formatting (empty + stable), path rewriting (strips scratch
UUID), scratch operations (writes + gitignore), cleanup safety
(refuses outside scratch), scratch GC (removes stale dirs, keeps
fresh, no-op when root missing), and pipe-drain deadlock regression
(large stderr does not stall `run_tsc`). See §14.1 for the PR-C
hotfixes that added the last three tests.

### 14.1 PR-C hotfixes — pipe deadlock, UTF-8 safety, scratch GC

> Status: **implemented, PR-C** on
> `devin/1776803952-oc-titan-compiler-gate-hotfix`. PR-C closes two
> bugs and one follow-up flagged by Devin Review on the merged
> PR-B. No API surface changed; all fixes are internal to the
> compiler gate and the controller's log-truncation helper.

#### Bug 1 — pipe-buffer deadlock in `run_tsc`

The previous `run_tsc` took stdout/stderr pipes from the child,
then called `child.wait()`, and only drained the pipes *after*
the wait returned. On Linux a pipe buffer is typically 64 KiB;
tsc can easily exceed that on a project with many diagnostics.
Once the buffer filled, tsc blocked on its own write and `wait()`
blocked on tsc's exit — a textbook deadlock. The outer
`tokio::time::timeout` would eventually fire and the outcome was
mis-classified as `CompileOutcome::Timeout`, which carries no
diagnostics — the very output the repair loop needs. Because
`Timeout` returns `Err` with no retry, legitimate compile
failures were silently hidden behind a confusing "timeout"
message.

Fix: spawn two `tokio::spawn` readers (`read_to_end` on each
pipe) *before* awaiting `child.wait()`. On timeout we kill the
child first, then await the reader tasks so they observe EOF
rather than hanging on the other deadlock side.

Regression test: `run_tsc_drains_large_stderr_without_deadlock`
spawns `sh -c 'dd if=/dev/zero bs=204800 count=1 | tr \\0 x >&2;
exit 2'` and asserts the child exits cleanly and all 200 KiB
reach stderr. We generate the payload inside the child because
passing 200 KiB via argv trips Linux's `E2BIG` limit.

#### Bug 2 — `truncate_for_log` panic on UTF-8 boundary

`controller::truncate_for_log` used a raw byte slice
(`s[..4096]`). If byte 4096 fell mid-codepoint (Arabic text,
emoji, the Unicode arrow tsc sometimes emits), `&str::index`
panics with `byte index X is not a char boundary`. That
panic lived on the error path where compile retries are
exhausted — so a real compile failure would crash the Tauri
backend instead of returning a clean `Err` to the UI.

Fix: `s.chars().take(MAX_CHARS).collect::<String>()` — constant
`MAX_CHARS = 4096` now counts codepoints, not bytes. The
truncation footer also reports chars truncated, not bytes.

Regression tests: `ascii_boundary_does_not_panic`,
`multi_byte_boundary_does_not_panic` (Arabic, 2-byte codepoints),
`four_byte_emoji_boundary_does_not_panic` (🔥, 4-byte codepoints).

#### Follow-up — stale scratch GC

`Scratch::cleanup` only runs on the normal control flow
(`Drop`-on-success, explicit `cleanup` on error). A panic inside
`run_tsc`, a SIGKILL on the Tauri backend, or a power loss
between `prepare_scratch` and `cleanup` would leave the UUID dir
behind forever. Left unaddressed, `.oc-titan/scratch/` grows
without bound.

Fix: new `compiler_gate::gc_stale_scratch(oc_root, max_age)`
invoked best-effort from `prepare_scratch` after the `.oc-titan`
tree is created. Removes any dir directly under
`<oc_root>/scratch/` whose mtime is older than `max_age`
(hard-coded to 24 h). I/O errors are swallowed so GC failures
never prevent the current compile from starting. `symlink_metadata`
is used so a rogue symlink inside `scratch/` can't redirect
`remove_dir_all` outside the sandbox.

Regression tests: `gc_stale_scratch_removes_old_dirs_keeps_fresh`
uses the `filetime` dev-dep to backdate a dir by 48 h without
sleeping, asserts it's removed while a fresh dir survives;
`gc_stale_scratch_is_noop_without_root` asserts the very first
compile in a project (no `scratch/` yet) doesn't error.

#### Net effect on events + API

No new events, no new settings, no schema changes. The three
fixes tighten the existing lifecycle without altering the
`run_codegen_envelope` command signature — a safe hotfix PR.

---

## 15. OC-Titan Phase 1.C — Dependency guard

Phase 1.C is V6 §I.6 — "Validate imports vs `package.json` BEFORE
writing. Fail loudly if unresolved." It closes the phantom-import
failure mode documented by `OPENROUTER_VALIDATION_REPORT` §3 L2/L3
(model hallucinates an import for a package that was never installed,
then the compiler gate burns retry slots rediscovering the same
mistake). The guard runs **after** the JSON envelope is validated
(Phase 1.A) and **before** the compiler gate (Phase 1.B), so
envelopes that can't possibly compile are bounced without spending a
`tsc --noEmit` budget on them.

### Module map

- `dependency_guard.rs` — new module. ~600 LoC + 21 unit tests.
  Regex-based import extractor (no tree-sitter / swc — keeps the
  Tauri bundle small). Public surface:
  - `GuardOutcome` — `Ok { resolved }` / `Skipped { reason }` /
    `Missing { missing, per_file }` / `Warned { missing, per_file }`.
    Re-exported through `use crate::dependency_guard::{self,
    GuardOutcome};` in `controller.rs`.
  - `check_envelope(project_dir, envelope, enabled, mode)` — the
    single call site from the controller. Returns `Result<GuardOutcome,
    String>`.
  - `missing_to_feedback(outcome)` — renders the re-prompt payload
    consumed by `build_dependency_feedback_prompt`.
- `settings.rs` — two new fields, backward-compatible via
  `#[serde(default = …)]`:
  - `dependency_guard_enabled: bool` (default `true`).
  - `dependency_guard_mode: String` (default `"fail"`; other
    accepted value is `"warn"`).
- `controller.rs` — new reprompt builder
  `build_dependency_feedback_prompt` and new guard step inside the
  `run_codegen_envelope` retry loop (§4.4 below).

### Specifier extraction

Four import shapes are recognised in `.ts` / `.tsx` / `.js` /
`.jsx` / `.mts` / `.cts` envelope files (all other files are
ignored — JSON, CSS, MD, etc. never import packages):

1. Static `import ... from 'pkg'` / `import 'pkg'` / `export ...
   from 'pkg'`.
2. `require('pkg')` (CommonJS).
3. Dynamic `import('pkg')` (ESM lazy import).
4. Bare side-effect imports (`import 'pkg/register'`).

Comments are stripped first (line `//` and block `/* */`) so the
guard does not flag imports that are deliberately commented out.
String literals use single, double, or backtick quotes — the regex
set covers all three. The middle section of `FROM_IMPORT_RE`
excludes quotes and `;` but deliberately allows newlines, so
multi-line destructured imports (the shape LLMs emit most often)
are caught.

### Specifier classification

Each extracted specifier is run through `classify_specifier`, which
returns `Some(package_root)` or `None`:

- **None** (skip): relative (`./`, `../`), absolute (`/`), Node
  builtins (`fs`, `path`, `crypto`, …), namespaced schemes
  (`node:fs`, `bun:test`, `deno:net`, `file:…`), empty specifiers.
- **Some**: every other specifier, normalised to its package root:
  - `lodash/debounce` → `lodash`.
  - `@tanstack/react-query/devtools` → `@tanstack/react-query` (for
    scoped packages the first *two* segments are kept).

The Node builtin list has 41 entries (`assert`, `buffer`,
`child_process`, `crypto`, `fs`, `http`, `https`, `net`, `os`,
`path`, `process`, `stream`, `tls`, `url`, `util`, `zlib`, …) and
is defined as a static `HashSet` so lookups are O(1).

### `package.json` declared-deps set

`load_declared_deps(project_dir)` reads `<project>/package.json` and
returns a `BTreeSet<String>` that is the **union** of all four
dependency sections:

1. `dependencies`
2. `devDependencies`
3. `peerDependencies`
4. `optionalDependencies`

Returns `None` (not an error) when the file is missing or malformed
— the guard then emits `Skipped { reason: "no_package_json" }` so
fresh projects without `package.json` don't deadlock codegen.

### Outcome matrix

| Condition | Outcome |
| --- | --- |
| `dependency_guard_enabled == false` | `Skipped { reason: "disabled" }` |
| Envelope has zero `.ts/.tsx/.js/.jsx/.mts/.cts` files | `Skipped { reason: "no_supported_files" }` |
| No `package.json` in project root | `Skipped { reason: "no_package_json" }` |
| Every classified specifier resolves | `Ok { resolved: [..] }` |
| ≥1 specifier doesn't resolve, `mode = "fail"` | `Missing { missing, per_file }` |
| ≥1 specifier doesn't resolve, `mode = "warn"` | `Warned { missing, per_file }` |

`apply_mode` is the single downgrade point: `Missing` + `mode =
"warn"` → `Warned`. Every other outcome passes through unchanged.

### Guard integration into `run_codegen_envelope`

Inserted between `run_codegen_envelope_turn` and the compiler-gate
`skip_policy` check. The guard **shares** `max_compile_retries` with
the compiler gate — each guard miss consumes one attempt, because
each miss costs a full model round-trip to fix. Pseudocode:

```
for attempt in 0..max_attempts {
    turn = run_codegen_envelope_turn(...)?

    outcome = dependency_guard::check_envelope(
        project_dir, turn.envelope, dep_enabled, dep_mode,
    ).await

    match outcome {
        Ok | Skipped | Warned => emit ai:step, fall through to gate
        Missing(m) => {
            emit ai:step "dependency.missing"
            if attempt+1 >= max_attempts {
                return Err("dependency guard: … unresolved package(s) …")
            }
            current_request = build_dependency_feedback_prompt(...)
            continue  // skip compiler gate this iteration
        }
    }

    // compiler gate (Phase 1.B) runs here
}
```

An internal error inside `check_envelope` (e.g. package.json is a
non-UTF-8 file) is **demoted to `Skipped { reason:
"internal_error" }`** after emitting an `ai:step` warning — the guard
must never be the reason a compile refuses to start.

### `ai:step` events (Terminal Authority, V6 §VI.1)

Six new events, all with `role: "guard"`:

- `dependency.ok` — `status: "done"`, carries `resolved_count`.
- `dependency.skipped` — `status: "done"`, carries `reason`.
- `dependency.warned` — `status: "warning"`, carries `missing[]`
  (forwards envelope anyway).
- `dependency.missing` — `status: "failed"`, carries `missing[]`.
- `dependency.retry` — `status: "running"`, emitted right before a
  missing-driven reprompt so the UI can show the retry count.
- `dependency.error` — `status: "warning"`, only on the demoted
  internal-error branch.

Reprompts reuse the same frame as `build_compile_feedback_prompt`
but include the `[dependency guard]` tag so the model can tell the
two failure modes apart (phantom import vs bad TypeScript).

### Unit tests (21 new, `cargo test --lib dependency_guard`)

- `classify_handles_relative_and_absolute` / `classify_handles_builtins`
  / `classify_normalises_subpaths_and_scopes` / `classify_empty_is_none`
- `extract_picks_up_four_shapes` / `extract_ignores_commented_imports`
- `guard_ok_when_everything_resolves` / `guard_flags_missing_package`
  / `guard_flags_scoped_package_root_not_subpath`
  / `guard_skips_non_source_envelopes`
- `apply_mode_warn_downgrades_missing` /
  `apply_mode_fail_preserves_missing`
- `missing_to_feedback_is_stable_and_informative`
- `load_declared_deps_reads_all_four_sections` /
  `load_declared_deps_returns_none_when_missing` /
  `load_declared_deps_returns_none_on_malformed_json`
- `check_envelope_skips_when_disabled` /
  `check_envelope_skips_without_package_json` /
  `check_envelope_detects_missing_end_to_end`

## §15.1 Phase 1.C hotfix (PR-E) — regex + doc corrections

PR-E addresses four issues flagged by Devin Review on PR #4:

- **BUG-1 (doc, `PROJECT_MEMORY.md`, comment 3120401417)**: §15 said
  "Five new events" but six are emitted (`ok`, `skipped`, `warned`,
  `missing`, `retry`, `error`). Corrected.
- **BUG-2 (doc ↔ code, comment 3120401509)**: §15 claimed backtick
  quotes were covered, but the four regexes only matched `'` / `"`.
  Widened the quote-set character class to `['"`]` in `FROM_IMPORT_RE`,
  `BARE_IMPORT_RE`, `REQUIRE_RE`, and `DYNAMIC_IMPORT_RE`, so the
  doc claim is now accurate. Template-literal imports like
  `` import(`lodash`) `` are extracted correctly.
- **BUG-3 (functional, comment 3120401607)**: `FROM_IMPORT_RE`'s middle
  section excluded `\n`, which silently dropped every multi-line
  destructured import — the shape LLMs emit most often. The
  exclusion class is now `[^'"`;]*?` instead of `[^'"\n;]*?`; the
  `;` stop still prevents bridging across sibling import statements.
- **Cosmetic (comment 3120401725)**: `check_envelope_with_deps` now
  dedupes `per_file` raw specifiers so `import { a } from 'zustand';
  import { b } from 'zustand'` surfaces `zustand` once in the model
  feedback instead of twice.

Six new regression tests pin the fixes so future refactors can't
regress the multi-line / backtick / dedupe behaviours:

- `extract_handles_multi_line_from_import`
- `extract_handles_multi_line_export_from`
- `extract_handles_backtick_template_literal_imports`
- `extract_does_not_bridge_across_semicolons`
- `guard_dedupes_per_file_raw_specifiers`
- `guard_catches_multi_line_phantom_import_end_to_end`

No API or event changes. The 21 existing `dependency_guard` tests
all still pass.

Plus the pair that Devin Review flagged on PR-C
(`multi_byte_boundary_does_not_panic`,
`four_byte_emoji_boundary_does_not_panic`) was rewritten to use
`.repeat(5000)` so they actually enter the truncation branch and
assert the surviving chars instead of short-circuiting at
`total_chars <= MAX_CHARS`.

### What is still TODO (intentionally deferred)

- **Auto-fix**: V6 §I.6 mentions "Auto-fix missing dependencies".
  Phase 1.C is only the **detect + fail loudly** half. Auto-adding
  packages to `package.json` (and deciding between `dependencies` vs
  `devDependencies`, picking a version range, running the install)
  is a separate concern that belongs in Phase 2 alongside `run_cmd`
  execution — the same security gate needs to guard both.
- **Workspace resolution**: The guard reads the project root's
  `package.json`. It does not walk yarn/pnpm/npm workspaces, so an
  envelope in a subpackage of a monorepo that imports a workspace
  sibling would currently see a miss. Mitigated today by `mode =
  "warn"` but worth revisiting when we add monorepo support.
- **`tsconfig.json` path aliases**: `@/components/Foo` style imports
  aren't treated as bare packages because they start with `@` — but
  the current classifier would (correctly) treat them as scoped and
  look for `@/components`. A future iteration should read
  `compilerOptions.paths` and exclude matching aliases; for now the
  guard defaults to `mode = "fail"` but the user can flip to
  `"warn"` when working in a project that uses aliases heavily.

---

## 16. OC-Titan Phase 2.A — `run_cmd` security classifier (V6 §VII.2)

Phase 2.A introduces a **deterministic** three-tier risk classifier
for the `run_cmd` field that codegen envelopes optionally surface
(Phase 1.A, §13). The classifier is *pure*: same input always returns
the same `Classification`, with no filesystem / network / LLM
side-effects. It's the foundation every future execution layer
(Phase 2.B, 2.C) will consume, so the rules live in one place that
the UI, executor, auto-install flow, and telemetry aggregator can
all call.

Phase 2.A is **classification only** — nothing is executed. The
existing AI-tool-loop command gate
(`tools::should_prompt_run_cmd` + `tools::deny_reason`) is left
intact; Phase 2.B will migrate it onto the classifier in a separate
PR so scope stays small.

### §16.1 Module map (new file)

- [`desktop/src-tauri/src/security_gate.rs`](desktop/src-tauri/src/security_gate.rs)
  — ~720 LoC incl. 17 unit tests. Three public items:
  - `pub enum SecurityClass { Safe, Warning, Dangerous }` — ordered
    (`Dangerous > Warning > Safe`) so callers can take `max(…)` of
    multiple classifications if needed.
  - `pub struct Classification { class, reason, matched_rule,
    compound }` — serialised verbatim to the UI. `matched_rule` is a
    stable string id (`dangerous.rm_rf_root`, `safe.pkg_builtin`,
    `unknown`) suitable for telemetry aggregation; `reason` is the
    human-readable explanation shown in `ai:step`.
  - `pub fn classify(cmd: &str) -> Classification` — the entry
    point.

### §16.2 Classification order (terminal on first match)

1. **Dangerous** — destructive / privileged / out-of-sandbox patterns.
   Preserves every substring from `tools::deny_reason` and adds the
   V6 §VII.2 extras:
   - `rm -rf /`, `rm -rf /*`, `rm -rf ~`, `rm -rf $HOME`
   - `mkfs`, `fdisk`, `dd of=/dev/…`
   - `:(){ :|:& };:` fork bomb variants
   - `sudo`, `doas`
   - `chown -R /`, `chmod -R 777 /`, `chmod 777 /`
   - `>/dev/sda`, `>/dev/sdb`, `>/dev/nvme…`
   - `> /etc/…`, `>> /etc/…`, `> /boot/…`
   - `git push --force`, `git push -f`, `git push --force-with-lease`
   - `git reset --hard`
   - `git clean -fd`, `git clean -fdx`, `git clean -df`
   - `curl … | sh` / `wget … | bash` / `… | zsh` / `… | fish`
2. **Warning** — state-changing but reversible:
   - Package installs: `npm / pnpm i|install|add|uninstall|remove`,
     `bun i|install|add|remove`, `yarn add|install|remove`,
     `pip install|uninstall`, `cargo add|install|update`, `go get`
   - Git writes: `commit`, `push` (non-force), `checkout`, `merge`,
     `pull`, `rebase`, `cherry-pick`, `revert`, `tag`, `stash`,
     `apply`, `am`, `fetch`; `branch -D / --delete`
   - FS writes: `mkdir`, `touch`, `mv`, `cp`, `ln`, `rm <file>`
     (non-recursive, non-root)
   - Formatters with writes: `prettier --write`, `eslint --fix`,
     `rustfmt`, `cargo fmt`
   - DB migrations: `prisma`, `drizzle-kit`, `typeorm`, `knex`,
     `sequelize`, `alembic`, `rake db:*`
   - Containers / services: `docker run|exec|build|rm|rmi|pull|push|compose`,
     `systemctl`, `service`, `launchctl`
3. **Safe** — read-only / idempotent / local build:
   - `npm / pnpm / yarn / bun run <script>` where `<script>` is in
     the known-safe allow-list: `dev, start, serve, preview, build,
     build:prod, compile, typecheck, tsc, lint, check, test, tests,
     format:check, fmt:check, prepack`
   - `npm|pnpm|yarn|bun test` / `npm|pnpm|yarn|bun build` (built-in
     shortcuts; no `$` anchor so compound-escalation still identifies
     the base rule — see §16.4)
   - `tsc --noEmit`, `npx tsc --noEmit`, `bun x tsc --noEmit`,
     `pnpm exec tsc --noEmit`
   - `cargo check|build|test|clippy|tree|metadata|doc|bench`,
     `cargo fmt --check`, `cargo fmt -- --check`
   - Git read: `status`, `diff`, `log`, `show`, `remote -v`,
     `ls-files`, `describe`, `reflog`, `rev-parse`, `blame`,
     `shortlog`, `branch` (list forms)
   - POSIX read: `ls`, `pwd`, `whoami`, `date`, `uname`, `which`,
     `file`, `wc`, `head`, `tail`, `cat`, `echo`, `printf`, `env`,
     `hostname`, `uptime`, `id`, `stat`, `du`, `df`, `type`
   - `grep`, `find` (minus `-delete / -exec / -execdir / -ok /
     -okdir`)
   - Direct script exec: `node foo.mjs`, `bun tool.ts`, `python3
     gen.py`, etc.
   - Package manager listing: `npm|pnpm|yarn|bun ls|list|info|view|
     why|outdated|pm ls`
4. **Unknown** — conservative default: `Warning` with `matched_rule =
   "unknown"`. Phase 2.B treats this as "prompt the user"; Phase 2.C
   must call out unfamiliar auto-install shapes to the user before
   running them.

### §16.3 Compound escalation

`contains_compound_construct` detects any of:

- pipes (`|`)
- logical connectives (`&&`, `||`) — detected via the `&` and `|`
  byte-level check
- command separators (`;`)
- command substitution (`$(…)`, backticks)
- backgrounding (`&`)
- file redirects (`<`, `>`)

When a compound construct is present:

- A **Dangerous** match stays Dangerous (already at the top; the
  `compound` flag is recorded for telemetry).
- A **Warning** match stays Warning (no double-escalation — the
  Phase 2.B execution layer already treats Warning as "prompt").
- A **Safe** match is **escalated to Warning**. The `matched_rule`
  is preserved so telemetry can still count "safe intent, compound
  shape" separately from genuinely-unknown commands. The reason
  string gets a trailing `(escalated: command contains compound
  shell constructs)` hint for UI display.

This guarantees `npm test && rm -rf /` can never classify as Safe:
either the `rm -rf /` substring fires `dangerous.rm_rf_root`
directly (which is the case), or — for less-obvious tails — the Safe
match is lifted to Warning so the UI prompts before executing.

The detector is intentionally naïve (it flags compound constructs
inside quoted strings too). False positives just mean "escalate one
tier", which is the safe direction. A Phase 2.B refinement can add
string-literal awareness if real traffic shows the false-positive
rate is too high.

### §16.4 Why the `$` anchor was removed from `safe.pkg_builtin`

Earlier drafts anchored `safe.pkg_builtin` with `\s*$`. That broke
`matched_rule` identification for compound variants: `npm test |
tee out.log` has a pipe, so the strict anchor never matched, and
the classifier returned `unknown`. Replacing `\s*$` with `\b` lets
the base rule match while compound-escalation still bumps the tier
to Warning. The net result:

| Command                         | `class`   | `matched_rule`      | `compound` |
| ------------------------------- | --------- | ------------------- | ---------- |
| `npm test`                      | Safe      | `safe.pkg_builtin`  | false      |
| `npm test \| tee out.log`       | Warning   | `safe.pkg_builtin`  | true       |
| `npm test && rm -rf /`          | Dangerous | `dangerous.rm_rf_…` | true       |

### §16.5 `find` special-case

The Rust `regex` crate lacks look-around, so `find … -delete /
-exec / -execdir / -ok / -okdir` can't be excluded via a negative
lookahead the way we'd do in PCRE. `match_safe` handles `find`
explicitly: if the command starts with `find` (or `find ` / `find\t`)
and contains any of those destructive flags, it falls through to
`unknown` → Warning. Otherwise it classifies as `safe.find`.

### §16.6 Integration into the codegen envelope lifecycle

`controller::run_codegen_envelope` now reads two additional settings
(`security_gate_enabled`, `security_gate_warning_mode`) and, after
`apply_codegen_envelope` succeeds, emits a new `ai:step` event for
the envelope's `run_cmd` (when present):

```json
{
  "role": "security",
  "label": "security.classified",
  "status": "done" | "warning" | "failed",
  "class": "safe" | "warning" | "dangerous",
  "reason": "<human-readable>",
  "matched_rule": "<stable id>",
  "compound": bool,
  "warning_mode": "prompt" | "allow" | "block",
  "run_cmd": "<original cmd>"
}
```

The event is **informational only** — nothing executes. The UI can
colour-code (`done` = green, `warning` = amber, `failed` = red) and
show the rule id for debugging. Phase 2.B will consume the same
classifier to decide whether to prompt, auto-run, or refuse.

### §16.7 Tauri command: `classify_run_cmd`

A dedicated command lets the UI preview classification for an
arbitrary string without invoking the full envelope lifecycle:

```rust
#[tauri::command]
pub fn classify_run_cmd(cmd: String) -> security_gate::Classification
```

Registered in `lib.rs` alongside `run_codegen_envelope`. Useful for:

- Settings-dialog preview ("this is what would happen if the model
  asked you to run `…`").
- Future Phase 2.B manual override: the user pastes a command, the
  UI shows class + rule, and the user decides to execute or not.

### §16.8 Settings (two new fields)

- `security_gate_enabled: bool` — default `true`. Off disables both
  the `ai:step` event emission **and** future Phase 2.B guarding.
- `security_gate_warning_mode: "prompt" | "allow" | "block"` —
  default `"prompt"`. Phase 2.A persists this verbatim for the UI
  to show; Phase 2.B will honour it when deciding how to route a
  WARNING classification through the confirm modal.

### §16.9 Tests

17 new unit tests in `security_gate::tests`:

- `safe_matrix_passes` — 52 Safe commands.
- `warning_matrix_prompts` — 40 Warning commands.
- `dangerous_matrix_blocks` — 31 Dangerous commands.
- `compound_escalates_safe_to_warning` — pipes, `&&`, `;`,
  redirects, `$(…)`, backticks each escalate a Safe base.
- `compound_does_not_downgrade_dangerous` — Dangerous wins even
  when chained after a Safe command.
- `compound_preserves_warning_tier` — Warning stays Warning (no
  double-bump).
- `unknown_shapes_default_to_warning` — conservative fallback.
- `empty_command_is_safe_noop` — empty/whitespace-only is Safe.
- `leading_trailing_whitespace_is_trimmed` — canonicalisation.
- `case_insensitive_dangerous_matching` — `SUDO`, `Sudo` trigger.
- `find_with_delete_is_not_safe`, `find_with_exec_is_not_safe` —
  special-case fall-through.
- `unknown_script_name_does_not_match_safe_run` — `npm run deploy`
  is *not* Safe.
- `security_class_ordering_holds`, `event_status_mapping`,
  `classification_is_json_serializable`,
  `classifier_is_deterministic`.

Full suite: **121/121** passing (104 pre-existing + 17 new).
`cargo check` clean.

### §16.10.1 PR-G hotfix — two dangerous-pattern gaps

Devin Review on the merged PR-F (#6) surfaced two functional gaps
in `match_dangerous`. PR-G fixes both with targeted rules and
regression tests; the rest of Phase 2.A is unchanged.

**BUG-G1 — `git push -f` trailing-space bypass** (comment
`BUG_pr-review-job-...-0001`). The substring `"git push -f "` in
the dangerous needle list required a trailing space, so the bare
command `git push -f` (end of string, no further args) fell
through to the generic `warning.git_write` regex and classified
as Warning instead of Dangerous. `git push -f` is a real force
push that rewrites the upstream just like `git push -f origin
main`. Fix: replace the brittle substring with a `Lazy<Regex>`
`\bgit\s+push\s+-f\b` evaluated after the substring loop. The
word boundary matches end-of-string, whitespace, and punctuation
after `-f`, while rejecting hypothetical typos like `git push
-foo` where the next char is still a word char.

**BUG-G2 — missing `fork()` pattern** (comment
`BUG_pr-review-job-...-0002`). The docstring above
`match_dangerous` promised full parity with
`crate::tools::deny_reason`, but `deny_reason` included
`("fork()", "fork bomb variant")` at `tools.rs:494` while the
classifier did not. Phase 2.B's migration off `deny_reason`
would have silently demoted `fork()` from blocked → Warning.
Fix: add `("fork()", "dangerous.fork_bomb_variant", "fork bomb
variant")` to the substring needle list so lowercased input
containing `fork()` matches directly.

**BUG-G3 — `--force-with-lease` telemetry shadowed by `--force`**
(follow-up review comment on PR-G itself). `"git push --force"`
is a substring of `"git push --force-with-lease"`, and the former
was listed first in the dangerous needle table, so the latter
always attributed `matched_rule = dangerous.force_push` instead
of `dangerous.force_push_lease`. Security tier (Dangerous) was
correct either way, but telemetry fidelity matters for UI reason
text and Phase 2.B audit trails. Fix: swap the two lines so the
more-specific needle is checked first, with an inline comment
explaining why these two are the exception to the "order does
not matter" guidance at the top of the table.

**Regression coverage.**
- `bare_git_push_f_classifies_as_dangerous` — five variants
  including bare, padded, extra-whitespace, with upstream args,
  and uppercase.
- `git_push_f_word_boundary_rejects_typos` — `git push -foo`
  must not match `dangerous.force_push`.
- `fork_variant_classifies_as_dangerous` — `fork()` alone,
  wrapped in `bash -c '...'`, and uppercase.
- `force_with_lease_attributes_to_lease_rule` — three variants
  ensuring `matched_rule = dangerous.force_push_lease`.
- Three new entries in `dangerous_matrix_blocks` keep the
  existing matrix honest (`git push -f`, `git push -f` with
  padding, `git  push  -f` with extra internal whitespace,
  `fork()`, `bash -c 'fork()'`).

All 125 lib tests pass (`cargo test --lib`: 121 previous +
4 new regression).

### §16.10.2 Deferred informational findings from PR-F review

- **Case-sensitivity asymmetry** (comment `ANALYSIS_...-0001`).
  `match_dangerous` lowercases input; `match_warning` /
  `match_safe` do not. Mixed-case `NPM INSTALL` or `CARGO CHECK`
  falls through to `unknown` → Warning, same tier as the
  lowercase variants would produce, but with `matched_rule =
  unknown` instead of `warning.npm_install` / `safe.cargo_check`.
  Security classification is unaffected (safe-direction fall-
  through); only telemetry fidelity. Left as-is; a future refactor
  that normalizes input before all three matchers would close
  the gap without changing outcomes.
- **`rm -rf /tmp/foo` over-classification** (comment
  `ANALYSIS_...-0002`). The `lower.contains("rm -rf /")` check
  matches any `rm -rf` followed by any absolute path, so
  `rm -rf /tmp/build-cache` classifies as
  `dangerous.rm_rf_root`. Safe-direction; acceptable trade-off
  vs. risk of missing `rm -rf /;` / `rm -rf /&`.
- **`cargo fmt src/lib.rs` telemetry drop** (comment
  `ANALYSIS_...-0003`). Warning via fallback rather than via
  `warning.cargo_fmt`; classification tier is unchanged, only
  `matched_rule` differs.
- **Compound detector discussion** (comment `ANALYSIS_...-0004`).
  Reviewer confirms `$HOME` correctly does NOT trigger compound
  escalation (only `$(` does). No change.
- **`tsc --noEmit` regex style** (comment `ANALYSIS_...-0005`).
  Backtracking note, not a correctness issue. No change.

### §16.10 Out of scope (intentionally deferred)

- **Execution of `run_cmd`.** Phase 2.B. The classifier is the
  deciding layer but the executor must add stream capture, timeout,
  sandbox check, and streaming-to-terminal integration.
- **Rewiring `tools::should_prompt_run_cmd` onto the classifier.**
  Phase 2.B. Doing it now would change a well-tested production
  path inside a classifier-introduction PR.
- **Auto-install flow.** Phase 2.C. Must use `classify()` to vet
  the `bun add / npm install <missing>` commands it synthesises
  from `dependency_guard`'s miss list (§15).
- **String-literal-aware compound detector.** Out-of-scope refinement
  if false-positive rate on compound escalation turns out to matter
  in real traffic.

---

## §17. OC-Titan Phase 2.B — `run_cmd` execution engine (V6 §VII.2 + §V.3 hook)

Phase 2.B (PR-H) wires the Phase 2.A classifier
(`security_gate::classify`, §16) into **actual `run_cmd` execution**
through the existing in-production `tools::run_cmd_impl`. No new
execution engine is introduced; the gate is a thin policy +
telemetry wrapper around the same child-spawn / cancel / tree-kill
machinery the legacy tool loop already uses.

### §17.1 Module map (new file)

- `desktop/src-tauri/src/run_cmd_gate.rs`
  - `Decision { AutoRun | Prompt | Block }` — three-way policy
    verdict, emitted on every decision via `run_cmd.policy`.
  - `ExecutionStatus { Executed | RefusedDangerous | BlockedByPolicy |
    UserDenied | ConfirmTimedOut | Skipped }` — terminal state of
    every `execute_run_cmd` call. `event_status()` maps each to the
    `ai:step` `status` tag.
  - `ExecutionResult` — `{ exit_code, duration_ms, stdout_tail,
    stderr_tail, classification, decision, status, reason }`;
    serialised into the envelope return and consumed by §V.3 runtime
    validation (next phase).
  - `PolicyInputs<'a>` — `{ class, warning_mode, dangerous_policy,
    autonomous_confirm, allow_list_match }`.
  - `decide(&PolicyInputs) -> Decision` — pure function; covered by
    17 deterministic truth-table tests.
  - `allow_list_matches(cmd, &[String]) -> bool` — exact-or-prefix
    match, ignoring empty entries (whitespace-only lines survive
    `settings.rs` load without silently allow-listing everything).
  - `tail_for_log(&str) -> String` — UTF-8-safe char-bounded tail
    (mirrors `controller::truncate_for_log`; never byte-slices).
  - `execute_run_cmd(app, state, project_dir, cmd, cancel) ->
    Result<ExecutionResult, String>` — main entrypoint (envelope
    controller + Tauri command both call it).
  - `execute_classified_run_cmd` (Tauri `#[command]`) — standalone
    surface for the future §VI.2/§VI.3 UI panel. Reads `AppState`
    from the `tauri::State` and forwards.

### §17.2 Decision matrix (policy truth table)

| class     | warning_mode | dangerous_policy | autonomous_confirm | allow_list match | Decision                       |
|-----------|--------------|------------------|--------------------|------------------|--------------------------------|
| Safe      | any          | any              | false              | any              | AutoRun                        |
| Safe      | any          | any              | true               | any              | Prompt                         |
| Warning   | "allow"      | any              | false              | any              | AutoRun                        |
| Warning   | "allow"      | any              | true               | any              | Prompt (autonomous upgrade)    |
| Warning   | "prompt"     | any              | any                | miss             | Prompt                         |
| Warning   | "prompt"     | any              | false              | match            | AutoRun (allow-list downgrade) |
| Warning   | "prompt"     | any              | true               | match            | Prompt (autonomous wins)       |
| Warning   | "block"      | any              | any                | any              | Block                          |
| Dangerous | any          | "refuse"         | any                | any              | Block                          |
| Dangerous | any          | "prompt"         | any                | any              | Prompt                         |
| Dangerous | any          | unknown          | any                | any              | Block (fail-closed default)    |

Notes:
- **Allow-list never overrides `warning_mode = "block"`** — block is
  administrative, allow-list is user convenience; a bug here would
  silently defeat an explicit opt-out.
- **Allow-list never applies to Dangerous** — by construction; the
  classifier only produces Dangerous for irreversible / credential
  / injection patterns.
- **Unknown `warning_mode` / `dangerous_policy` → safest default
  (Prompt / Block)** with two regression tests.

### §17.3 Lifecycle inside `run_codegen_envelope`

```
  … envelope parsed & validated (§13)
  → dependency guard (§15) passes / reprompts
  → compiler gate (§14) passes / reprompts
  → apply_codegen_envelope writes files through sandbox
  → security_gate::classify(run_cmd) emits `security.classified`
  → if settings.security_gate_execute_enabled:
       run_cmd_gate::execute_run_cmd
         • computes Decision via decide()
         • emits `run_cmd.policy` (always)
         • Block → emit refused/blocked → return Skipped-shaped result
         • Prompt → await_user_confirmation
             - Approve → proceed to spawn
             - Deny → emit `run_cmd.user_denied` → UserDenied
             - TimedOut/Cancelled → ConfirmTimedOut
         • AutoRun (or post-approve) →
             - emit `run_cmd.started`
             - tools::run_cmd_impl (shared with legacy tool loop;
               cancel-aware, tree-killing, pipe-teed)
             - emit `run_cmd.completed` (exit_code, duration_ms, tails)
         • ExecutionResult written onto AppliedEnvelope.execution
     else:
       Phase 1 behaviour preserved — run_cmd surfaces as metadata.
```

Back-compat: `security_gate_execute_enabled = false` by default, so
every existing code path (including the legacy `execute_run_cmd_gated`
tool loop) is byte-identical to pre-2.B. The opt-in flips when the
§VI.2/§VI.3 UI panel lands.

### §17.4 New `ai:step` events (role = `"execution"`)

All emitted through the existing `ai:step` channel (UI-agnostic):

- `run_cmd.policy` — always emitted before any side effect. Payload:
  `{ decision, class, warning_mode, dangerous_policy, allow_list_matched,
     autonomous_confirm, cmd }`.
- `run_cmd.confirmation` — only emitted when `Decision::Prompt`;
  payload carries `{ cmd, class, matched_rule, reason }` so UI can
  render a justification next to the confirm dialog.
- `run_cmd.started` — right before `run_cmd_impl` spawn;
  `{ cmd, timeout_ms, class }`.
- `run_cmd.completed` — after child is reaped;
  `{ exit_code, duration_ms, stdout_tail, stderr_tail, class }`.
- `run_cmd.refused` — Dangerous → Block path;
  `{ reason, class, matched_rule }`.
- `run_cmd.blocked` — Warning → Block path (warning_mode="block");
  same shape as refused with `class: "warning"`.
- `run_cmd.user_denied` — confirm modal Deny; `{ cmd }`.
- `run_cmd.error` — infra-level failure (invalid project root,
  spawn error). Envelope still commits successfully; user can retry
  from the terminal panel.

`terminal:output` + `terminal:done` continue to be emitted by
`run_cmd_impl` itself — no duplication in the gate.

### §17.5 Settings (three new fields, all `#[serde(default)]`)

- `security_gate_execute_enabled: bool` — **default `false`**.
  Opt-in until UI §VI.2/§VI.3 lands. When off, everything else in
  §17 is inert.
- `security_gate_execute_timeout_ms: u64` — default `120_000` (2
  min). Wall-clock for a single `run_cmd`; `tools::run_cmd_impl`
  enforces via SIGKILL / TerminateJobObject.
- `security_gate_dangerous_policy: String` — default `"refuse"`;
  `"prompt"` routes Dangerous through the same confirm modal as
  Warning (explicit escape hatch for power users).

All three fields are forward-compatible with Phase 1 settings files
(missing keys parse via `default` fns and `#[serde(default)]`).

### §17.6 Execution path reuses `tools::run_cmd_impl`

`tools::run_cmd_impl` (visibility changed from private to
`pub(crate)` in this PR) is the in-production runner shared with
`execute_run_cmd_gated` / `run_cmd_stream`. It already handles:

- concurrent stdout/stderr reading + `terminal:output` streaming
- `CancelToken` polling via `tokio::select!` with tree-kill on
  cancel / timeout (PR-C hardening from §14.1)
- per-process timeout that kills before `wait_with_output` (prevents
  the pipe-buffer deadlock fixed in PR-C)
- exit-code capture and a 4 KB char-bounded tail (UTF-8-safe)

Reusing this function eliminates the risk of introducing a
parallel execution engine that lacks one of those hardenings.

### §17.7 Tauri surface

- `execute_classified_run_cmd(app, project_dir, cmd)` registered in
  `lib.rs` alongside the Phase 2.A `classify_run_cmd`. The UI can
  call it directly (with a pre-shown classification) once §VI.2 /
  §VI.3 land; it does not depend on the envelope controller.

### §17.8 Tests (`cargo test --lib run_cmd_gate`, 23 new)

- **Decision matrix (17)** — every non-trivial row of §17.2, plus
  the four "unknown setting → safe default" rows.
- **Allow-list (2)** — exact + prefix match; empty entries ignored.
- **Tail UTF-8 (2)** — short strings untouched; 4-byte emoji at the
  4096-char boundary never panics.
- **Execution surface (3)**:
  - `execute_empty_cmd_returns_skipped` — whitespace short-circuit.
  - `execute_dangerous_refuses_without_spawn` — no child, status =
    `RefusedDangerous`, reason starts with `"refused: "`.
  - `execute_warning_prompt_without_ui_returns_user_denied` — no
    AppHandle means no modal; the gate cannot silently auto-run.
- **Real spawn (1)**:
  - `execute_safe_echo_runs_and_captures_exit_0` — `echo
    hello_from_gate` classified Safe → AutoRun → `run_cmd_impl` →
    `Executed`, `exit_code = 0`, `stdout_tail` contains the token.
    Proves the end-to-end wire-up actually spawns and captures
    output.

Total lib-test count after PR-H: **148 green** (Phase 1 + 2.A + new).

### §17.9 Deliberately deferred (not in PR-H)

- **Legacy `tools::execute_run_cmd_gated` migration.** Still valid
  as a shim; migrating it onto `run_cmd_gate::execute_run_cmd`
  would widen PR-H beyond the execution-engine introduction. A
  follow-up PR will port it (same classifier, same policy, same
  tests) with no API break.
- **§V.3 runtime validation.** Consumes `ExecutionResult.exit_code`
  + `stderr_tail` to drive a reprompt loop when a user-approved
  `run_cmd` exits non-zero. Added in the next phase.
- **§VI.2 / §VI.3 UI tiers.** Event payloads are already shaped for
  the 3-tier layout (ThinkingBlock / FinalAnswer / SystemAction);
  binding them into the React store is a frontend PR.
- **Phase 2.C auto-install.** Depends on 2.B merging first; will
  synthesize `bun add <missing>` / `npm install <missing>` from
  `dependency_guard`'s miss list (§15) and route through
  `execute_run_cmd` like any other command.

### §17.10 PR-I hotfixes on PR-H (Devin Review follow-up)

Two real bugs + one design-hole closure surfaced in Devin Review of
PR-H; fixed together in PR-I.

- **Word-boundary allow-list match** (`run_cmd_gate::allow_list_matches`).
  Before: `cmd.starts_with(entry)` let `"lsblk"` match the
  default-allow-list entry `"ls"`, silently downgrading a Warning
  to AutoRun and bypassing the confirm modal the user opted into.
  After: match requires `cmd == entry` or `cmd` starts with
  `entry` **followed by an ASCII space** — restoring parity with
  the legacy `tools::cmd_matches_prefix`. Empty / whitespace-only
  entries are ignored so a blank line in `settings.json` can't
  accidentally allow-list every command. Two regression tests:
  `allow_list_requires_word_boundary_not_raw_starts_with` (covers
  `lsblk`, `lsof`, `lsattr`, `catfish`, `findmnt`) and
  `allow_list_ignores_whitespace_only_entries`.

- **Per-request `autonomous_confirm` override**
  (`execute_run_cmd(..., autonomous_confirm_override: Option<bool>)`).
  Before: `execute_run_cmd` read
  `Settings::autonomous_confirm_irreversible` and ignored the
  `autonomous_confirm: bool` parameter the UI passes to
  `run_codegen_envelope`. If the two diverged (mid-flight settings
  edit, direct API caller), the persisted setting silently won.
  After: `Some(v)` from the caller wins over the persisted value;
  `None` keeps the legacy behaviour (standalone
  `execute_classified_run_cmd` entrypoint). Three regression
  tests: `execute_autonomous_confirm_override_upgrades_safe_to_prompt`
  (forces Safe → Prompt), `..._false_lets_safe_autorun` (doesn't
  block AutoRun on Some(false)), `..._none_falls_back_to_settings`
  (None → legacy path).

- **Tautological test assertion fixed.** The original
  `assert!(!allow_list_matches(...) == false || true)` always
  passed due to `|| true` short-circuit; replaced with a direct
  negated assert plus broader boundary coverage.

Informational-only review notes (no code change, reply-only):

- Double classification of `run_cmd` in the envelope path (once by
  `run_codegen_envelope` to emit the `security.classified` event,
  once inside `execute_run_cmd`). Deterministic + pure, wasted
  work is microseconds; future refactor could pass the
  already-computed `Classification` as a parameter.
- Prompt-arm variable shadowing in `execute_run_cmd` is safe (outer
  `Option<&AppHandle>` is `Some(...)` by the match arm precondition)
  but subtle for future maintainers.
- `tail_for_log` vs `controller::truncate_for_log` — local
  duplication to avoid cross-module re-export of a private helper;
  a shared utility with a head/tail mode is a possible future
  refactor.

---

## §18. OC-Titan §V.3 — runtime validation (exit-code + stderr reprompt)

Phase §V.3 closes the self-healing loop that began with the compiler
gate (§14) and the dependency guard (§15). With Phase 2.B (§17) now
surfacing an [`ExecutionResult`] on successful `run_cmd` dispatch,
§V.3 consumes `exit_code` + `stderr_tail` + `stdout_tail` and, on a
non-zero exit, feeds them back to the model as a repair prompt —
exactly the same shape as the compiler-gate reprompt, but wired
*after* execution rather than *before* it.

The entire layer is **deterministic** and **side-effect-free**. It
makes one pure policy call (`evaluate`) and one pure string-builder
call (`build_reprompt`); all state mutation, retry budgeting, and
telemetry live in the controller.

### §18.1 Module map (new file)

- **`desktop/src-tauri/src/runtime_validation.rs`** (~420 LoC,
  12 unit tests)
  - `RuntimeOutcome { Ok | Errors | Skipped }` — the tri-state
    result of policy evaluation.
  - `evaluate(exec: Option<&ExecutionResult>, enabled: bool)
     -> RuntimeOutcome` — pure, no I/O. Returns
    `Skipped { reason: "disabled" }` when the toggle is off,
    `Skipped { reason: "no_execution" }` when Phase 2.B never
    dispatched (no `run_cmd`, or `security_gate_execute_enabled`
    was false), and a stable `Skipped { reason: … }` for every
    non-`Executed` terminal status (refused, blocked, denied,
    timed-out, gate-skipped).
  - `build_reprompt(original_request, exit_code, stderr_tail,
     stdout_tail)` — formats a Claude-friendly repair prompt that
    pins the failed command's exit code, stderr tail, and stdout
    tail inside a fenced block, then re-asks the original request.
  - `status_to_reason(ExecutionStatus)` — pinned string table for
    the `Skipped { reason }` tag. Exposed as `pub(crate)` so the
    controller and tests both reference the same strings.

### §18.2 Outcome truth table

| `enabled` | `execution`                   | `status`          | `exit_code` | `RuntimeOutcome`                         |
|-----------|-------------------------------|-------------------|-------------|------------------------------------------|
| `false`   | *any*                         | *any*             | *any*       | `Skipped { reason: "disabled" }`         |
| `true`    | `None`                        | *n/a*             | *n/a*       | `Skipped { reason: "no_execution" }`     |
| `true`    | `Some(exec)`                  | `RefusedDangerous`| *any*       | `Skipped { reason: "refused_dangerous"}` |
| `true`    | `Some(exec)`                  | `BlockedByPolicy` | *any*       | `Skipped { reason: "blocked_by_policy"}` |
| `true`    | `Some(exec)`                  | `UserDenied`      | *any*       | `Skipped { reason: "user_denied" }`      |
| `true`    | `Some(exec)`                  | `ConfirmTimedOut` | *any*       | `Skipped { reason: "confirm_timed_out"}` |
| `true`    | `Some(exec)`                  | `Skipped`         | *any*       | `Skipped { reason: "execution_skipped"}`|
| `true`    | `Some(exec)`                  | `Executed`        | `0`         | `Ok { exit_code: 0, duration_ms }`       |
| `true`    | `Some(exec)`                  | `Executed`        | `!= 0`      | `Errors { exit_code, tails, duration_ms}`|

**Key invariant:** policy refusals (Dangerous refused, Block,
UserDenied) **never** burn a retry slot. Only a genuine
`Executed` + non-zero-exit path consumes the shared
`max_compile_retries` budget.

### §18.3 Lifecycle inside `run_codegen_envelope`

The retry loop that the compiler gate (§14) and dependency guard
(§15) already share was refactored to keep **apply/execute/validate
inside the loop** so §V.3 can reprompt on runtime failure without
duplicating the apply step. The effective order per attempt:

1. Codegen envelope turn (ai call + parse).
2. Dependency guard (§15) — may `continue` on `GuardOutcome::Missing`
   (consumes a slot).
3. Compiler gate (§14) — may `continue` on `CompileOutcome::Errors`
   (consumes a slot).
4. **Promote envelope**: `apply_codegen_envelope` writes the files
   through the `fs_ops` sandbox.
5. Security classifier (§16) emits `security.classified`.
6. If `security_gate_execute_enabled`, `run_cmd_gate::execute_run_cmd`
   runs and populates `AppliedEnvelope.execution`.
7. **§V.3 evaluate** consumes `result.execution` and the new
   `runtime_validation_enabled` flag.
   - `Ok | Skipped` → emit the corresponding step event, store
     the applied result, `break` out of the loop.
   - `Errors` → emit `runtime.errors`; if this is the last attempt
     emit `runtime.exhausted` and `return Err`; otherwise emit
     `runtime.retry`, rebuild `current_request` via
     `build_reprompt`, and fall through to the next iteration.

After the loop, the applied result is returned. If somehow every
slot is consumed without a non-error outcome, the final error
message comes from the last gate that ran (compiler or runtime).

### §18.4 Shared retry budget

`max_compile_retries` is **one** pool, not three. Every miss
(`dependency.missing`, `compiler.errors`, `runtime.errors`) burns
exactly one slot. This keeps the worst-case LLM round-trip count
bounded and predictable regardless of which gate fails. Skipped
outcomes (disabled gate, unsupported language, no `run_cmd`,
policy refusal) do **not** burn slots.

### §18.5 New `ai:step` events (role = `"runtime"`)

All five events carry `attempt` (zero-indexed) so the UI can align
them with the compiler / guard streams.

| label               | status     | fields                                                                                  |
|---------------------|------------|-----------------------------------------------------------------------------------------|
| `runtime.ok`        | `"done"`   | `exit_code`, `duration_ms`                                                              |
| `runtime.errors`    | `"failed"` | `exit_code`, `duration_ms`, `stderr_tail`, `stdout_tail`                                |
| `runtime.retry`     | `"running"`| `attempt` (next), `max_attempts`                                                        |
| `runtime.skipped`   | `"skipped"`| `reason` (one of: `disabled` / `no_execution` / `refused_dangerous` / `blocked_by_policy` / `user_denied` / `confirm_timed_out` / `execution_skipped`) |
| `runtime.exhausted` | `"failed"` | `exit_code`, `max_attempts`                                                             |

`terminal:output` / `terminal:done` continue to come from
`tools::run_cmd_impl` unchanged — §V.3 does not duplicate them.

### §18.6 Settings (one new field, `#[serde(default)]`)

- `runtime_validation_enabled: bool` (default **`false`**) — opt-in.
  Gated on `security_gate_execute_enabled=true` to have any effect
  (nothing runs → nothing to validate); toggling `runtime_validation_enabled`
  on without execution enabled is a no-op.

All existing `Settings` defaults continue to deserialize cleanly,
so this PR is backward-compatible with every persisted settings
file shipped since PR-A.

### §18.7 Tests (`cargo test --lib runtime_validation`, 14 new)

`evaluate` — outcome matrix covering every `enabled` × `execution`
× `status` × `exit_code` combination:

- `evaluate_disabled_always_skips_even_with_nonzero_exit`
- `evaluate_no_execution_returns_no_execution_skip`
- `evaluate_exit_zero_returns_ok`
- `evaluate_exit_nonzero_returns_errors_with_tails`
- `evaluate_exit_negative_one_on_executed_still_counts_as_errors`
- `evaluate_refused_dangerous_skips`
- `evaluate_blocked_by_policy_skips`
- `evaluate_user_denied_skips`
- `evaluate_confirm_timed_out_skips`
- `evaluate_execution_skipped_skips`

`build_reprompt` — formatting invariants:

- `reprompt_includes_original_request_exit_and_both_tails`
- `reprompt_renders_empty_tails_as_explicit_marker`
- `reprompt_treats_whitespace_only_tails_as_empty`
- `reprompt_is_utf8_safe_with_emoji_and_multibyte_tails`

Total after PR-J: **167 passed** (153 prior + 14 new). Zero
failures, zero ignored.

### §18.8 Deliberately deferred (not in PR-J)

- **Phase 2.C — auto-install** (V6 §I.6 second half): synthesise
  `bun add <pkgs>` / `npm install <pkgs>` after
  `GuardOutcome::Missing`, route through §16 classifier +
  §17 executor. Needs PR-J merged so the classifier→executor→
  validator triangle is stable before layering auto-install on top.
- **§VI.2 / §VI.3 UI tiers** — `runtime.*` events ship with stable
  labels + statuses, but the React renderer for ThinkingBlock /
  FinalAnswer / SystemAction is still TODO. The events are
  render-agnostic today.
- **Multi-command runtime validation** — current scope is the
  single `run_cmd` emitted by the codegen envelope. Multi-command
  sequences are out of scope until Phase 2.C introduces an explicit
  command queue.

### §18.9 Why §V.3 is opt-in (default `false`)

Three reasons, in decreasing order of importance:

1. **Phase 2.B is already opt-in.** Until users flip
   `security_gate_execute_enabled=true`, no `run_cmd` ever runs,
   so there's nothing for §V.3 to validate. Shipping §V.3 as
   opt-out would be theatre.
2. **Retry budget coupling.** §V.3 shares
   `max_compile_retries` with the compiler gate + dependency
   guard. Users with custom budgets should consciously opt into
   the extra pressure runtime errors place on that budget.
3. **Noise on false failures.** A `run_cmd` that exits non-zero
   for benign reasons (test suite that reports a warning, a CLI
   that uses exit 1 for `--help`) would silently consume retries.
   Opt-in keeps this off until the user knows their `run_cmd`
   contract.

---

## §19. OC-Titan Phase 2.C — Fix-forward auto-install

Phase 2.C closes the loop the [dependency guard](#15-oc-titan-phase-1c--dependency-guard)
opened in Phase 1.C. When the guard reports
`GuardOutcome::Missing`, instead of unconditionally burning a
retry slot on a reprompt, the controller now synthesises a
deterministic `bun add <pkgs>` / `npm install <pkgs>` command,
routes it through the existing Phase 2.A classifier and Phase 2.B
executor, and (on success) re-runs the guard. Successful installs
are **fix-forward** — the current envelope continues to the
compiler gate in the same attempt, with no retry slot consumed.
All failure modes fall back to the classic reprompt path, which
mirrors pre-2.C behaviour exactly.

The work lives in [`desktop/src-tauri/src/autoinstall.rs`] (pure
logic + orchestrator) and one new branch in
[`controller.rs`] `run_codegen_envelope` (the `GuardOutcome::Missing`
arm). No new execution engine, no new classifier, no new
validator — Phase 2.C is composition.

### §19.1 Lifecycle

```
envelope -> dep_guard -> Missing
                         |
                         v
                  autoinstall::try_fix_forward
                         |
      +------------------+------------------+
      v                  v                  v
  [skipped]         [executed ok]      [failed / refused /
  (disabled /           |                denied / blocked /
   execute_            [re-check         timed-out / error]
   disabled /          dep_guard]             |
   no_missing /            |                  |
   empty_after          ok / warned      NotResolved
   _sanitize)           / skipped              |
      |                    |                   |
      |                 Resolved               |
      |                    |                   |
      +--------------------+-------------------+
                         |
                         v
          +-- Resolved  ------- fall through to compiler gate
          |                     (same attempt, NO slot consumed)
          |
          +-- NotResolved ----- max_attempts guard +
                                dependency.retry event +
                                reprompt + `continue`
                                (consumes 1 slot)
```

### §19.2 Settings (new in §19)

Two fields on `settings::Settings`, both `#[serde(default)]` so
loading a pre-2.C config is fully backward-compatible:

| field | type | default | meaning |
| --- | --- | --- | --- |
| `autoinstall_enabled` | `bool` | `false` | Master on/off switch. Requires `security_gate_execute_enabled=true` (cannot physically install without Phase 2.B) **and** `dependency_guard_enabled=true` (nothing to auto-install otherwise). |
| `autoinstall_package_manager` | `String` | `"auto"` | `"auto"` (default) probes the project root for a lockfile (`bun.lock` / `bun.lockb` → bun, `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn, `package-lock.json` → npm) and falls back to `bun` when no lockfile is present. Explicit overrides: `"bun"`, `"npm"`, `"pnpm"`, `"yarn"`. Any other value is treated as `"auto"`. |

### §19.3 Retry-budget invariant

Phase 2.C preserves the shared-budget contract established by
Phase 1.C, Phase 1.B, and §V.3:

* **Resolved** (install ok + re-check clean) → **0 slots**
  consumed. The controller falls through to the compiler gate
  with the original envelope, in the same iteration of the
  `for attempt in 0..max_attempts` loop.
* **NotResolved** (any other terminal state — disabled,
  execute_disabled, no_missing, empty_after_sanitize, refused,
  blocked, user_denied, confirm_timed_out, execution_error,
  install_failed, still_missing_after_install, recheck_error)
  → **1 slot** consumed. The controller emits
  `dependency.retry` with a new `autoinstall_reason` field, swaps
  in the reprompt, and `continue`s to the next iteration.

This keeps a hostile / buggy install path from looping forever:
the same `max_compile_retries` ceiling that bounded Phase 1.C
still bounds Phase 2.C.

### §19.4 Command synthesis & determinism

[`autoinstall::synthesise_install_cmd`] is a pure function. The
package list is **de-duplicated + alphabetically sorted** before
rendering so two calls with the same logical miss set produce
byte-identical output regardless of the upstream iteration
order. Stable output matters for:

* security classifier `matched_rule` / telemetry stability;
* prefix-based `cmd_allow_list` matching (Phase 2.B allow-list);
* confirm-modal caching of prior user approvals.

Subcommand map:
`bun → bun add` / `npm → npm install` / `pnpm → pnpm add` /
`yarn → yarn add`.

**Quoting.** Names are whitelisted against the character set
`[A-Za-z0-9@/._+-]` with no leading `-`. Anything containing
whitespace, quotes, shell metacharacters, or a `-` prefix is
**dropped silently**, and an empty sanitised list becomes an
empty command (which the orchestrator treats as
`Skipped { reason: "empty_after_sanitize" }`). Defence-in-depth:
the dependency guard already normalises specifiers to package
roots, but we never pass unsanitised input to the shell.

### §19.5 Package-manager resolution

[`autoinstall::resolve_package_manager`] precedence:

1. Explicit setting (`"bun"` / `"npm"` / `"pnpm"` / `"yarn"`)
   wins verbatim. User intent beats detection.
2. Otherwise probe lockfiles via
   [`autoinstall::detect_package_manager`] in the order
   **bun → pnpm → yarn → npm**. The bun-first ordering
   matters for projects mid-migration that still carry a stale
   `package-lock.json` alongside a fresh `bun.lock`.
3. If the probe returns `None`, fall back to
   `default_package_manager()` = `Bun`, matching the repo-level
   Bun-first convention documented in
   [`AGENTS.md`](AGENTS.md) / [`CLAUDE.md`](CLAUDE.md).

### §19.6 Events (new in §19)

All emitted with `role="autoinstall"` under the same `ai:step`
channel the rest of the pipeline uses.

| label | status | when |
| --- | --- | --- |
| `autoinstall.skipped` | `"done"` | Pre-flight skip. `reason` ∈ {`"disabled"`, `"execute_disabled"`, `"no_missing"`, `"empty_after_sanitize"`, any `ExecutionResult::reason` when the gate returned `Skipped`}. |
| `autoinstall.attempting` | `"running"` | Command synthesised, about to hand to `run_cmd_gate::execute_run_cmd`. |
| `autoinstall.ok` | `"done"` | Child exited with code 0. Emitted *before* the guard re-check. |
| `autoinstall.resolved` | `"done"` | Re-check came back `Ok` / `Skipped` / `Warned`. Fix-forward win. |
| `autoinstall.ok_but_unresolved` | `"warning"` | Exit 0 but the guard still sees missing specifiers (e.g. the model asked for a package that doesn't exist on the registry). |
| `autoinstall.failed` | `"failed"` | `Executed` status with non-zero exit. Carries `stderr_tail`. |
| `autoinstall.refused` | `"blocked"` | Classifier flagged the synthesised command as dangerous. Not expected in practice for `bun add` / `npm install`; emitted as a safety net. |
| `autoinstall.blocked` | `"blocked"` | `warning_mode=block` blocked the install. |
| `autoinstall.user_denied` | `"skipped"` | User clicked Deny in the confirm modal. |
| `autoinstall.confirm_timed_out` | `"skipped"` | Confirm modal timed out or was cancelled. |
| `autoinstall.error` | `"error"` | Infra-level failure (spawn error, invalid root, or guard re-check error). |

The subsequent `dependency.retry` event (emitted by the
controller on the `NotResolved` branch) grows a new
`autoinstall_reason` field so UI consumers can tell
*retry-after-failed-install* from *retry-after-disabled-gate*
without rebuilding it from the event stream.

### §19.7 Tests (14 new in PR-L, all pure)

All in [`autoinstall.rs`]'s `tests` module; no network, no
`Command::spawn`, no Tauri harness required:

`PackageManager::parse`:
- `parse_accepts_canonical_lowercase`
- `parse_is_case_insensitive_and_trims`
- `parse_rejects_auto_empty_and_unknown`

`detect_package_manager` — lockfile probe:
- `detect_bun_lock_text`
- `detect_bun_lockb_binary`
- `detect_pnpm_lock`
- `detect_yarn_lock`
- `detect_npm_lock`
- `detect_none_when_no_lockfile`
- `detect_bun_wins_over_stale_package_lock`

`resolve_package_manager` — setting + probe + fallback:
- `resolve_explicit_setting_wins_over_lockfile`
- `resolve_auto_uses_lockfile`
- `resolve_auto_with_no_lockfile_falls_back_to_bun`
- `resolve_unknown_setting_is_treated_as_auto`

`synthesise_install_cmd` — determinism + quoting:
- `synthesise_bun_uses_add_subcommand`
- `synthesise_npm_uses_install_subcommand`
- `synthesise_pnpm_uses_add_subcommand`
- `synthesise_yarn_uses_add_subcommand`
- `synthesise_sorts_and_dedupes`
- `synthesise_handles_scoped_packages_unquoted`
- `synthesise_drops_packages_with_shell_metacharacters`
- `synthesise_drops_flag_shaped_tokens`
- `synthesise_empty_input_yields_empty_output`
- `synthesise_non_ascii_names_are_rejected`

Total suite: **191/191** passing after PR-L (167 pre-L + 24 new).
The orchestrator [`try_fix_forward`] is exercised end-to-end in
production paths but not directly unit-tested in PR-L — it requires
a full `AppHandle` + `AppState` + sandbox setup that belongs in an
integration harness. Every pure input into it (detection,
synthesis, sanitisation, manager resolution) is covered by the
unit tests above.

### §19.8 Why opt-in (default `false`)

Same three reasons [§V.3 is opt-in](#§v3-opt-in-defaults), plus a
new one specific to installs:

1. **Phase 2.B is itself opt-in.** An auto-install that cannot
   physically run until the user flips
   `security_gate_execute_enabled` would be theatre.
2. **Irreversible-ish side effects.** `bun add` modifies
   `package.json` and lockfiles. Users must consciously accept
   that the controller can now mutate dependency manifests on
   their behalf.
3. **Network egress.** Installs fetch from the public registry.
   Sandboxed / air-gapped environments must opt in (or leave the
   feature off).
4. **Registry drift.** A package name the model invented may
   exist on the registry but not be what the user wanted. Opt-in
   keeps that failure mode behind a toggle instead of a default.

### §19.9 Deferred (explicitly not in PR-L)

* **Per-envelope dev-vs-runtime split.** Everything installs as
  a regular dependency. `--save-dev` heuristics belong in a
  future PR once we have signal on which misses originate from
  test files vs. runtime imports.
* **Monorepo targeting.** The command runs from the project root
  and trusts the detected manager to find the right workspace.
  Workspace-aware installs (e.g. `bun add -w ...` /
  `pnpm -F pkg add`) are deferred.
* **Non-Node ecosystems.** Python (`pip`), Rust (`cargo add`),
  Go (`go get`) are out of scope for Phase 2.C. The classifier
  already tags them as Warning; wiring is a follow-up.
* **UI surface.** `autoinstall.*` events fly over the existing
  `ai:step` channel; §VI.2/§VI.3 will render them (and the
  confirm modal for WARNING installs is already in production
  from Phase 2.B).

---

*If something here is wrong or incomplete, fix it in-place rather than
adding a "TODO" note. This file is only useful as long as it's true.*
