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

- `probe_ollama` — 10 s timeout, returns reachable + models.
- `probe_openrouter` — 10 s timeout, returns
  `{ reachable, key_valid, model, credits }`.

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
Reviewer call   (call_model(role=Reviewer))  — if reviewer_enabled
  │  ├─ sees the executor's tool-call + tool-result transcript
  │  ├─ emits OK: or NEEDS_FIX: <instruction>
  │  └─ NEEDS_FIX feeds back into a retry (up to max_retries_per_task)
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

1. Emit from Rust with a stable name (prefix `ai:` for model activity,
   `task:` for controller activity).
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

*If something here is wrong or incomplete, fix it in-place rather than
adding a "TODO" note. This file is only useful as long as it's true.*
