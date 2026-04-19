# Open Claude Code — Desktop

> **Research snapshot.** This repository is a recovered Claude Code source
> snapshot intended for educational study, defensive security research,
> architecture review, and supply-chain analysis. It is **not** an official
> Anthropic repository. See [`AGENTS.md`](AGENTS.md) / [`CLAUDE.md`](CLAUDE.md)
> for the full context.

The `desktop/` subtree documented below is an **independent, self-contained
Tauri + React application** built on top of this snapshot: a **controlled
autonomous coding system** that runs locally against your own filesystem and
your own Ollama install. You give it a goal, it plans the work into a task
tree, runs the tasks through a Planner → Executor → Reviewer loop, and you
keep full visibility and full control: mid-flight cancel, subprocess
tree-kill, per-task execution trace, and optional human confirmation on
irreversible operations.

The desktop app is **local-first**. There are no mandatory cloud dependencies.
An OpenRouter key is supported as an optional planner, but everything
including autonomous goal execution works end-to-end with Ollama alone.

> The legacy `src/` folder is part of the read-only research snapshot and is
> not part of the running desktop system. All product code for the system
> described in this README lives under `desktop/`.

---

## What the system actually is

A single-process Tauri (Rust) + React desktop app with three working layers:

1. A **tool runtime** in Rust — `read_file`, `write_file`, `list_dir`, `run_cmd`
   — all scoped to an opened project root. Paths are resolved through a single
   `fs_ops::resolve` sandbox helper (leading `/`, `..`, symlinks all normalised).
2. A **three-agent loop** in `ai.rs` — Planner (optional, OpenRouter), Executor
   (Ollama), Reviewer (Ollama). Tool-call streaming is SSE end-to-end.
3. An **autonomous controller** in `controller.rs` — plans a user goal into a
   task tree, runs tasks sequentially through the same agent loop, enforces
   per-task retries, per-task timeouts, a global goal timeout, a consecutive-
   failure circuit breaker, and persists a bounded execution trace per task.

The UI is four panes: Explorer (file tree), Goal & Tasks (goal + task tree +
expandable per-task trace), Chat (streaming bubbles per role), Execution
(step timeline, tool calls, diffs, command logs).

---

## Core capabilities

| Capability | Where it lives | What it gives you |
|---|---|---|
| **Mid-flight cancel (SSE + subprocess)** | `cancel.rs`, `ai.rs`, `tools.rs` | Pressing Cancel during a 30-second model stream tears the TCP reader down immediately; during `cargo build` / `npm install` it kills the whole process tree, not just the shell parent. |
| **Subprocess tree-kill** | `tools.rs` | Children spawn with `process_group(0)` on Unix and `CREATE_NEW_PROCESS_GROUP` on Windows. Cancel path is SIGTERM → short grace → SIGKILL (Unix) / `child.kill()` → `taskkill /T /F` sweep (Windows). Verified by a grandchild-survives-shell test. |
| **Typed `CancelReason`** | `cancel.rs` | `User` / `Goal` / `Timeout` / `CircuitOpen`. The reason survives the `tokio::select!` race in `run_cmd` so UI and memory log why something stopped. |
| **Per-task execution trace** | `trace.rs`, persisted in `tasks.rs` | Each task carries a bounded transcript: user / system / planner / executor / reviewer messages, tool-call + tool-result pairs, retry markers, errors. Persisted with the task inside `active_task_tree` and `task_history` and rendered as an expandable panel under each task in the UI. |
| **Optional confirm for irreversible ops** | `settings.autonomous_confirm_irreversible` | When on, `write_file` (only when the file exists and content differs) and `run_cmd` are routed through the confirm modal even under `autonomous_mode=true`, bypassing the allow-list. Chat-driven turns are unaffected. |
| **Retry + backoff per task** | `controller.rs`, `settings.retry_backoff_base_ms` | Exponential (`base * 2^retries`, capped at 30 s). Reviewer's `NEEDS_FIX:` feedback is fed back to the next attempt. |
| **Per-task and global timeouts** | `settings.task_timeout_secs` (default 180 s), `settings.goal_timeout_secs` (default 3600 s) | Wrapped with `tokio::time::timeout`. Timeout propagates as `CancelReason::Timeout` so teardown is clean. |
| **Circuit breaker** | `settings.circuit_breaker_threshold` (default 5) | Consecutive task failures trip the breaker, the goal aborts with `status = "circuit_open"`, remaining tasks are marked failed with reason. |
| **Idempotency guard** | `AppState.goal_running` RAII guard | A second `start_goal` while one is running returns an error instead of racing. |
| **Sandbox-enforced writes** | `fs_ops::resolve` | `write_file` path resolution strips leading separators, walks `..` virtually, and rejects anything that escapes the project root. The autonomous-confirm write gate uses the same resolver, so it cannot be bypassed with `/src/foo.rs`-style paths. |
| **Memory with schema versioning** | `memory.rs` | Atomic temp-file + rename + fsync, ≤ 4 MiB serialized, `schema_version = 2`, migration path for older files. |

---

## Architecture overview

```
┌───────────────────────────────────────────────────────────────────────┐
│  UI (React + Vite + TypeScript)                                       │
│    Explorer  |  Goal & Tasks (+ trace)  |  Chat  |  Execution         │
│         tauri.invoke                    event.listen                  │
│                ▼                              ▲                       │
├───────────────────────────────────────────────────────────────────────┤
│  Tauri backend (Rust, tokio, reqwest, notify)                         │
│                                                                       │
│    controller::start_goal  ──►  tasks.rs (TaskTree + bounded trace)   │
│            │                                                          │
│            ▼                                                          │
│    ai::run_chat_turn   ──►  Planner / Executor / Reviewer loop        │
│            │                                                          │
│            ▼                                                          │
│    tools.rs  ──►  read_file / write_file / list_dir / run_cmd         │
│            │          └─ fs_ops::resolve sandbox                      │
│            │          └─ deny-list → allow-list → confirm modal       │
│            ▼                                                          │
│    cancel::CancelToken threaded through SSE readers and child wait    │
│    memory.rs  ──►  atomic PROJECT_MEMORY.json writes                  │
└───────────────────────────────────────────────────────────────────────┘
```

### Agent roles (all live in `ai.rs`)

- **Planner** — optional. If `OPENROUTER_API_KEY` is set, the first step of a
  chat turn and the goal decomposition go through OpenRouter for a short plan
  and/or initial tool calls. Falls back to the executor silently if the planner
  errors or is missing.
- **Executor** — Ollama. Runs the bounded tool-call loop
  (`settings.max_iterations`, default 8, hard cap 16). Emits `ai:token`,
  `ai:tool_call`, `ai:tool_result`, `ai:step` as it goes.
- **Reviewer** — Ollama. Inspects the executor's output and emits either
  `OK:` or `NEEDS_FIX: <instruction>`. `NEEDS_FIX` triggers up to
  `max_retries_per_task` corrective attempts with the reviewer's instruction
  fed back as feedback. Can be disabled via `settings.reviewer_enabled`.

### Goal execution flow (`controller::start_goal`)

```
user goal
  │
  ▼
scan_project        →  project_map (languages, entry points, configs, deps)
  │
  ▼
plan_goal           →  JSON task list (capped by max_total_tasks,
  │                     heuristic fallback if planner returns invalid JSON)
  │
  ▼
for each runnable task:
    ai::run_chat_turn  (planner → executor → reviewer)
       │
       ├─ timeout (task_timeout_secs)       → mark failed, retry with backoff
       ├─ reviewer NEEDS_FIX                → retry with feedback, backoff
       ├─ tool confirm denied               → mark failed, trace error
       ├─ cancel (user or goal_timeout)     → mark cancelled, stop cleanly
       └─ ok                                → mark done, append to trace
  │
  ▼
archive tree into task_history[]     (cap 200, oldest dropped)
emit task:goal_done                  { status, completed, failed }
```

---

## Running locally with Ollama

### 1. Install Ollama

```bash
# macOS / Linux — follow https://ollama.com/download
curl -fsSL https://ollama.com/install.sh | sh
ollama serve                       # runs on http://localhost:11434
```

### 2. Pull the models

The system is model-agnostic — it speaks the OpenAI-compatible tool-call
schema. These three are the ones this project has been exercised against
locally:

```bash
ollama pull deepseek-coder:6.7b    # strong executor for code tasks (~4 GB)
ollama pull qwen2.5:latest         # generalist executor / reviewer (~4.7 GB)
ollama pull llama3.2:1b            # fast, small reviewer (~1.3 GB, fits 8 GB RAM)
```

Recommended pairing:

| Role | Model | Why |
|---|---|---|
| Executor | `deepseek-coder:6.7b` or `qwen2.5:latest` | Both emit correct tool-call JSON and handle multi-file reasoning. |
| Reviewer | `llama3.2:1b` | Reviewer only needs to emit `OK:` or `NEEDS_FIX: …`; a 1B model is fast and accurate enough, and it frees RAM for the executor. |

You can run a single model for both — just point `OLLAMA_MODEL` at it.

### 3. Build the frontend and run the desktop app

```bash
cd desktop/frontend
npm install
npm run build                      # writes desktop/dist/

cd ../src-tauri
cargo tauri dev                    # starts the desktop app with hot reload
```

On Linux you'll need `webkit2gtk-4.1`, `libsoup-3.0`, `libjavascriptcoregtk-4.1`
installed. On Windows you need the WebView2 runtime (shipped with modern
Windows) and MSVC build tools. On macOS no extra install is required.

### 4. Configure once via Settings

Open the Settings dialog (top-right of the app) and set:

- **Ollama base URL** — defaults to `http://localhost:11434`, leave as-is if
  `ollama serve` is running locally.
- **Ollama model** — e.g. `deepseek-coder:6.7b`.
- **OpenRouter API key** — optional. Leave blank to run executor-only.
- **Reviewer enabled** — recommended on; the whole retry mechanism depends on
  the reviewer producing `NEEDS_FIX:` verdicts.
- **Cmd confirm required** — on by default. Commands that don't match the
  allow-list are routed through the confirm modal.
- **Autonomous mode** — off by default. Enable for goal-driven runs.
- **Autonomous confirm irreversible** — off by default. Enable to force the
  confirm modal on `write_file` (on changes to existing files) and `run_cmd`
  even inside an autonomous goal. This is the recommended safety setting when
  you trust the goal direction but don't fully trust the current executor.

Settings persist to the standard app config directory
(`~/.config/open-claude-code/settings.json` on Linux, equivalents on
macOS / Windows).

---

## Example workflows

These are the flows the system is actually built for — not hello-world demos.

### A. "Refactor this module across files" (autonomous)

1. Open your project in the Explorer pane.
2. Flip **Autonomous mode: on**. Leave **Autonomous confirm irreversible: on**
   the first time — you want visibility on writes.
3. In the Goal & Tasks pane, type:
   > *Extract the request-signing logic from `src/http/client.ts` into a new
   > `src/http/sign.ts` module and update all callers. Add a unit test.*
4. The controller will plan ~4–6 tasks, scan the project, then run each task:
   read files → propose edits → apply writes (you approve each overwrite) →
   run the test command (you approve that too).
5. Watch the per-task **Trace** panels expand as each task runs. If the
   reviewer flags `NEEDS_FIX:`, you'll see a retry task appear with the
   reviewer's feedback as input.

### B. "Fix the failing build" (autonomous)

1. Goal:
   > *Run `cargo build`. If it fails, read the errors, fix them, and rerun
   > until it succeeds. Do not change the public API.*
2. The controller plans a loop of run → read compile errors → edit → rerun.
3. The circuit breaker will trip after 5 consecutive failed fix attempts so
   the run doesn't spin forever; you'll see the breaker banner in the UI and
   can decide whether to intervene manually.

### C. "Add a feature end-to-end" (chat-driven, with intervention)

Use the **Chat** pane instead of a goal when you want to stay in the loop.
You get the same streaming tokens / tool calls / diffs, but no autonomous
continuation — each user turn is one Planner → Executor → Reviewer pass.
Good for: scaffolding a new module, adding a new CLI flag, writing a specific
function. Switch to a goal once the shape is clear.

### D. "Debug a runtime error"

1. Paste the stack trace / error into Chat (don't start a goal yet).
2. Let the executor use `read_file` / `list_dir` / `grep`-via-`run_cmd` to
   localise the problem.
3. Once you know where the fix belongs, either apply it yourself or start a
   small, well-scoped goal for the fix with `autonomous_confirm_irreversible`
   on so you see the writes before they land.

### E. "Deep trace investigation"

The trace panel under each task in **Goal & Tasks** is your primary debugging
tool. It's persisted in `PROJECT_MEMORY.json → task_history[].tasks[].trace`
so you can re-read old runs later. See
[`docs/USAGE.md`](docs/USAGE.md#reading-a-trace) for how to read one.

---

## Troubleshooting

### Cancel isn't stopping a `run_cmd`

If you're on Windows, confirm the child process actually spawned with a new
process group (the code path does this automatically via
`CREATE_NEW_PROCESS_GROUP`). If you're running inside an environment that
rewrites the shell (e.g. a nested sudo or a containerised shell that doesn't
honour process groups), tree-kill can't reach grandchildren. In that case the
cancel will still propagate `CancelReason::User` and the top-level command
will return, but you may have orphan children until they exit on their own.

### Ollama is returning empty tokens / no tool calls

Check that the model you selected actually supports tool calling. Older
models or heavily-quantised builds often emit the tool call as free text
(`<tool_call>{...}</tool_call>`) instead of the structured field. The code
path in `ai.rs` parses both, but models that don't follow either convention
won't work. `deepseek-coder:6.7b` and `qwen2.5:latest` both work.

### `write_file` silently succeeds but the file didn't change

This means the file existed and its contents were already identical. The
confirm gate explicitly treats identical-content writes as no-ops (no prompt,
no diff, no trace entry flagged destructive). If you want to force a
timestamp bump, edit a single char first.

### The confirm modal doesn't appear in autonomous mode

`autonomous_confirm_irreversible` is **off by default**. Turn it on in
Settings. Even then, `write_file` only prompts when the target exists and
the content would actually change — create-a-new-file writes go through
without a prompt.

### The trace panel is empty

A task's trace is attached to the task *after* the task runs. During a
running task you'll see live tokens in Chat and tool calls in Execution; the
consolidated Trace fills in once the task reaches a terminal state
(`done` / `failed` / `cancelled`).

### Build fails on Linux with a webkit / soup error

You're missing `webkit2gtk-4.1`, `libsoup-3.0`, or
`libjavascriptcoregtk-4.1`. See the Tauri v2 Linux prerequisites page.

### ENOSPC on Rust target directory

`desktop/src-tauri/target/` is the usual culprit. `cargo clean` in
`desktop/src-tauri` reclaims several GB.

### `cargo tauri dev` opens but the UI says "Failed to load memory"

The frontend expects the opened project root to have an editable
`PROJECT_MEMORY.json` or to be writeable (so one can be created). If the
root is read-only the command returns an error and the Goal panel stays
empty. Open a writeable project.

---

## Reference docs

- [`docs/USAGE.md`](docs/USAGE.md) — how to use the system effectively,
  real scenarios, how to read traces, when to intervene.
- [`docs/EVALUATION.md`](docs/EVALUATION.md) — strengths, real limitations,
  realistic next steps.
- [`PROJECT_PLAN.md`](PROJECT_PLAN.md) — living architecture + roadmap.
- [`PROJECT_MEMORY.json`](PROJECT_MEMORY.json) — machine-readable state:
  schema version, Tauri commands, events, settings surface, delivery status.

## License

Not yet declared.
