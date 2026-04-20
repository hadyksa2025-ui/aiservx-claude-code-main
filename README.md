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

> **Coming in cold?** Start with [`PROJECT_MEMORY.md`](PROJECT_MEMORY.md).
> It is the single structured reference for the architecture, the state
> model, the AI-routing rules, the known constraints, and the critical
> files — designed so you can make an informed change without re-reading
> the whole codebase.

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

Each role's provider (OpenRouter vs Ollama) is resolved at call time from
`settings.provider_mode` — `Cloud`, `Local`, or `Hybrid` — via
`ai::resolve_provider(settings, role)`. Per-role model overrides
(`planner_model`, `executor_model`, `reviewer_model`) are independent
string slots; empty means "use provider default". See
[`docs/PROVIDER_ROUTING.md`](docs/PROVIDER_ROUTING.md) for the full matrix.

- **Planner** — decomposes a chat turn or goal into steps / initial tool
  calls. In `Hybrid` mode it runs on OpenRouter with Ollama as a
  silent fallback; in `Cloud`/`Local` it runs on the matching provider
  with no fallback (so misconfiguration fails loudly — by design).
- **Executor** — runs the bounded tool-call loop
  (`settings.max_iterations`, default 8, hard cap 16). Emits `ai:token`,
  `ai:tool_call`, `ai:tool_result`, `ai:step` as it goes. In `Hybrid`
  and `Local` modes the executor is Ollama; in `Cloud` it is OpenRouter.
- **Reviewer** — inspects the executor's *tool-call + tool-result
  transcript* (not just the final text) and emits either `OK:` or
  `NEEDS_FIX: <instruction>`. `NEEDS_FIX` triggers up to
  `max_retries_per_task` corrective attempts with the feedback fed
  back. Can be disabled via `settings.reviewer_enabled`.

All three roles go through a single `ai::call_model(settings, role, …)`
entrypoint — there are no direct `stream_openrouter` / `stream_ollama`
call sites anywhere else in the codebase. New call sites must use it
so provider routing, `ai:step` metadata, and 5xx retry behaviour stay
consistent.

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

## Quick setup (≤ 10 minutes)

Target: first successful run, local-only, no OpenRouter key required.

### 1. Install Ollama and start the daemon

```bash
# macOS / Linux — follow https://ollama.com/download for native installers
curl -fsSL https://ollama.com/install.sh | sh
ollama serve                       # runs on http://localhost:11434
```

### 2. Pull the recommended models

Two models cover the full loop. Pull both (~5 GB total), or start with just
the executor and enable Reviewer later.

```bash
ollama pull deepseek-coder:6.7b    # executor — strong at code (~4 GB)
ollama pull llama3.2:1b            # reviewer — tiny & fast, 1.3 GB
# optional generalist fallback:
# ollama pull qwen2.5:latest       # ~4.7 GB
```

Role pairing this project has been exercised against:

| Role | Model | Why |
|---|---|---|
| Executor | `deepseek-coder:6.7b` | Emits correct tool-call JSON and handles multi-file reasoning. |
| Reviewer | `llama3.2:1b` | Only needs to emit `OK:` or `NEEDS_FIX: …`; a 1B model is fast and frees RAM for the executor. |
| Either (fallback) | `qwen2.5:latest` | Generalist, works as a single shared model when RAM is tight. |

Single-model setup is fine — point the `ollama_model` setting at one
model and it plays both roles.

### 3. Run the desktop app

```bash
# from the repo root
cd desktop/frontend
npm install
cd ../src-tauri
cargo tauri dev                    # starts the app with hot reload
```

The Vite dev server and `cargo tauri dev` together will open the desktop
window. First compile is slow (Rust); subsequent runs are fast.

**Linux**: install `webkit2gtk-4.1`, `libsoup-3.0`, `libjavascriptcoregtk-4.1`
(distro packages vary by name). **Windows**: WebView2 runtime (ships with
modern Windows) + MSVC build tools. **macOS**: no extra install.

### 4. Verify the connection

Open **Settings** (gear icon, top-right) → scroll to the Ollama section →
click **Test Ollama connection**.

Expected: green `✓ reachable · model deepseek-coder:6.7b available`.

If you see `⚠ reachable, but model … is not pulled`, the app reached
`ollama serve` but the tag hasn't been pulled yet — re-run the `ollama pull`
from step 2. If you see `✗ cannot reach …`, `ollama serve` isn't running or
the base URL is wrong.

---

## First run

Now actually drive the system. This is the smallest end-to-end flow that
exercises every layer (planner → executor → reviewer, tool runtime, trace,
UI) without needing OpenRouter.

### Settings for the first run

| Setting | Value | Why |
|---|---|---|
| Provider mode | **Local** | Runs the full Planner / Executor / Reviewer loop on Ollama. Switch to `Hybrid` once you add an OpenRouter key, `Cloud` if you want everything on OpenRouter. See [`docs/PROVIDER_ROUTING.md`](docs/PROVIDER_ROUTING.md). |
| Ollama base URL | `http://localhost:11434` | Default. Leave alone. |
| Executor model | `deepseek-coder:6.7b` | Pre-filled default for Ollama. |
| Reviewer model | `llama3.2:1b` | Tiny & fast; only needs to emit `OK:` or `NEEDS_FIX:`. |
| Planner model | leave blank | Defaults to the provider default in `Local` mode. |
| Reviewer enabled | **on** | You want to see the full three-agent loop. |
| Autonomous mode | **off** for your first few runs | So the UI stops between steps and you can watch. Flip it on once you trust the loop. |
| Autonomous confirm irreversible | **on** | Safety net for when you do flip autonomous on later. Harmless when autonomous is off. |
| Cmd confirm required | **on** (default) | Any shell command not on the allow-list routes through the confirm modal. |
| OpenRouter API key | leave blank | Executor-only mode is fine for the first run. Required for `Cloud` / `Hybrid`. |

Everything else — timeouts, retries, circuit breaker — can stay at defaults.

### A first goal you can paste

Open any small project folder (**File → Open project**). For a brand-new
project, make an empty directory first:

```bash
mkdir ~/oc-first-run && cd ~/oc-first-run && git init
```

Then open that directory in the app, and in the **Goal & Tasks** pane paste:

```
Create a file HELLO.md in the project root with a single line that says
"Hello from Open Claude Code.", then run `ls -1` to show the project
contents and confirm the file is there.
```

Click **Run goal** (or equivalent).

### What you should see

Approximate sequence — exact wording depends on the model:

1. **Goal & Tasks pane** populates with 1–3 tasks (usually: *Create HELLO.md*,
   *Run `ls -1` to verify*). Each task gets a `pending` pill that flips to
   `running` then `done`.
2. **Chat pane** streams Planner / Executor / Reviewer bubbles. The executor
   will emit a `write_file` tool call, then a `run_cmd` tool call.
3. **Confirm modal** pops up for `ls -1` (since `ls` on its own is on the
   allow-list, but `ls -1` isn't a prefix match — this is the expected
   behaviour, you'd add `ls -1` to the allow-list to avoid it next time).
   Click **Allow once**.
4. **Execution pane** shows the tool call, the tool result (stdout of `ls`),
   and a small diff for `HELLO.md`.
5. Each task in the Goal & Tasks pane has an **expandable trace**; click it
   to see the full transcript that task was run against.
6. When the last task flips to `done`, the goal status reports
   `status = ok`, `completed = N`, `failed = 0`.

If something goes sideways (model emits invalid tool-call JSON, times out,
etc.) the task flips to `failed` with a reason, and the reviewer's
`NEEDS_FIX:` feedback drives a retry — up to `max_retries_per_task` (default
3). The consecutive-failure circuit breaker (default 5) trips the whole
goal if every task keeps dying.

### Stopping safely

- **Cancel** during a model stream tears the TCP reader down immediately.
- **Cancel** during a `run_cmd` kills the entire process tree
  (`cargo build`, `npm install`, and their children all die).
- **Cancel** is always safe — nothing is left half-running.

See [`docs/USAGE.md`](docs/USAGE.md) for the longer guide (how to read traces,
when to flip autonomous mode, good vs bad goals) and
[`docs/SCENARIOS.md`](docs/SCENARIOS.md) for five real scenarios past the
hello-world one above.

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
