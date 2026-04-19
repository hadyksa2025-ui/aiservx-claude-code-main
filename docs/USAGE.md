# Usage guide

How to actually use Open Claude Code effectively — when to trust the
autonomous loop, when to stay in chat, how to read a trace, how to stop
things safely, and when to intervene manually.

If you want the top-level overview, see the [README](../README.md). If you
want the engineer's honest evaluation (what works, what doesn't), see
[EVALUATION.md](EVALUATION.md).

---

## The UI, at a glance

Four panes, top-to-bottom: **Explorer**, **Goal & Tasks**, **Chat**,
**Execution**.

- **Explorer** — file tree of the currently opened project. Auto-refreshes
  via the `notify` watcher (debounced 150 ms). Click a file to open it.
- **Goal & Tasks** — the autonomous controller's view. Goal input at the
  top, task tree below it with status chips, retry counts, result summary,
  and an expandable **Trace** panel under every task.
- **Chat** — streaming bubbles per role (planner / executor / reviewer).
  This is the chat-driven path; one user message = one Planner → Executor
  → Reviewer pass.
- **Execution** — the live event stream. Step timeline across the top
  (agent role chips, `running` / `done` / `failed`), tool calls + tool
  results inline below with colored diffs for writes and stdout/stderr
  chunks for commands.

Settings is top-right (gear icon). That's where autonomous mode, reviewer,
models, command allow-list, and the irreversible-confirm toggle live.

---

## Running a goal

The goal path is the autonomous one. Use it when you have a well-defined
outcome you can state in one or two sentences and you want the system to
drive the edits rather than you prompting for each one.

1. **Open the project root in Explorer.** The Goal panel won't work until
   a project is open — it needs a filesystem scope to run the scanner and
   write `PROJECT_MEMORY.json` into.
2. **Check Settings** once at the start of a session:
   - Ollama base URL + model — make sure `ollama serve` is running.
   - Reviewer enabled — leave on. The whole retry / NEEDS_FIX path
     depends on it.
   - `max_retries_per_task` — default 3 is reasonable; bump to 5 for
     hard tasks, drop to 1 if you want fail-fast behaviour.
   - `task_timeout_secs` — default 180 s per task. Raise if your
     executor is slow (a 1B model on CPU can be).
   - `goal_timeout_secs` — default 3600 s for the entire goal. Raise
     for big refactors.
   - **Autonomous mode** — on if you want the controller to run
     tasks back-to-back without a prompt between each one. Off if you
     want to approve each step.
   - **Autonomous confirm irreversible** — on if you want to be
     prompted for every `write_file` (on a change to an existing file)
     and every `run_cmd`, even inside an autonomous run. **Recommended
     the first time you run a goal against an unfamiliar project.**
3. **Type the goal.** Keep it specific; see "Writing good goals" below.
   Click **Run goal**.
4. **Watch the task tree fill in.** The controller's first action is
   to scan the project (emits `project:scan_done`) and then ask the
   planner for a JSON task list. Tasks appear in the panel as
   `pending`; one transitions to `running`; results / retries / errors
   populate as they happen.
5. **Expand the Trace** under any task to read what the agent actually
   did — user prompt, planner's plan, every tool call the executor
   issued, every result, every reviewer verdict, and retry markers.
6. **Stop when you need to.** The goal header has a **Cancel** button.
   It cancels the current task mid-flight (mid-SSE, mid-subprocess)
   and marks the rest of the tree `cancelled`. See "Stopping safely".

---

## When to enable autonomous mode

- **On**: refactors that span multiple files, build-fix loops, applying
  a known migration across a codebase, anything where the next-step
  decision is mechanical.
- **Off**: when you're exploring (use Chat), when the goal is
  architectural and you want to steer per-step, when the executor
  has made mistakes in the last few runs and you don't yet trust it.

The defaults are deliberately pessimistic:
- `autonomous_mode = false` — chat-driven by default.
- `autonomous_confirm_irreversible = false` — so turning `autonomous_mode`
  on doesn't silently change the confirm behaviour. Enable it explicitly
  when you want the safety belt.

## When to enable `autonomous_confirm_irreversible`

Turn it on when:

- You're running an autonomous goal against a project you don't fully
  trust the model to edit unprompted (basically always, on the first
  run).
- You want to *review* every destructive write and every command before
  it happens, while still letting the controller pick the next task.

Turn it off when:

- You've watched the agent run clean on this codebase multiple times
  and you know the task tree is mechanical.
- You're running in a throwaway environment (VM, CI-spawned sandbox)
  and don't need individual confirms.

What it actually gates:
- `write_file` prompts **only when** the file exists and new content
  differs (the write-would-change-existing-file gate uses the same
  `fs_ops::resolve` sandbox helper as the real write, so leading-slash
  paths can't bypass it).
- `run_cmd` prompts **every time**, even for commands on the
  allow-list. The allow-list is the interactive-chat auto-path; the
  irreversible toggle is the autonomous-mode escape hatch.

---

## Reading a trace

Every goal-driven task persists a bounded execution trace
(`desktop/src-tauri/src/trace.rs` → `Trace { entries, truncated }`).
It's the single most useful debugging surface in the system — more
useful than the live Execution pane, because it's consolidated per
task and survives restart (stored in
`PROJECT_MEMORY.json → task_history[].tasks[].trace`).

Open the **Trace** disclosure under any task. Entries are in order:

| Entry | What it means | What to look at |
|---|---|---|
| `User` | The task description fed to this attempt. | Compare against the goal — did the planner decompose the goal correctly? |
| `System` | The system prompt the executor saw. | Usually the same shape; check if the planner smuggled a wrong constraint. |
| `Planner` | Planner's output (if OpenRouter is configured). | Sanity-check the plan before the executor acts on it. |
| `Executor` | Assistant message from the executor model. | This is the model's reasoning between tool calls. |
| `ToolCall { name, args }` | The executor asked to run a tool. | Check `args` for wrong paths, wrong commands, wrong content sizes. |
| `ToolResult { ok, output, diff }` | What the tool returned. | `ok=false` plus output is the single most valuable signal when a task fails — the error is almost always here. For writes, `diff` shows the actual change. |
| `Reviewer { verdict }` | The reviewer's OK / NEEDS_FIX. | A `NeedsFix(instruction)` is what drives the next retry. Read the instruction — it'll tell you what the reviewer thinks went wrong. |
| `Retry { attempt, reason }` | Controller decided to retry. | Usually follows a `NeedsFix` or a `ToolResult { ok=false }`. |
| `Error { role, message }` | Something hard-failed. | Red. Read the message; it's not truncated. |

If the trace says `truncated = true`, the task produced more than 256
meaningful events (it happens on long build loops) — the first 256 are
kept, the rest are dropped. This is a known limitation, called out in
`EVALUATION.md`.

### Common trace patterns

- **Loop of `ToolCall run_cmd cargo build` → `ToolResult ok=false` →
  `Reviewer NeedsFix` → `Retry` → same thing.** The reviewer isn't
  extracting new information from the build error and the executor is
  re-applying the same failing edit. You should step in: either
  give the goal more specific guidance, or flip `autonomous_mode` off
  and use chat to fix the underlying issue.
- **`ToolCall write_file` with a massive `content` field.** The model
  is re-emitting the whole file instead of patching. Acceptable, but
  expect slower turns. If it's wrong, the reviewer should catch it.
- **`Error { role: "executor", message: "cancelled:Timeout" }`.** A
  task hit `task_timeout_secs`. Raise the timeout or shrink the task.

---

## Stopping safely

Cancellation is fully mid-flight after PR #5 + #6 + #7. You don't have
to wait for the current SSE stream or the current subprocess to finish
on its own terms.

| Action | Effect |
|---|---|
| Press **Cancel** on a chat turn | `cancel_chat` → `CancelToken.cancel()` with `CancelReason::User`. SSE readers are torn down immediately; any in-flight `run_cmd` is SIGTERM'd then SIGKILL'd, including grandchildren. The chat turn returns `Err("cancelled:User")`. |
| Press **Cancel** on a goal | `cancel_goal` → cancels both the goal token and the current-task token. Current task aborts as above; remaining tasks are marked `cancelled`. Tree is persisted to `task_history[]`. |
| Hit `task_timeout_secs` | Same teardown path, `CancelReason::Timeout`. Task marked `failed` with error `"task timeout"`. Controller proceeds to the next task (or retries, per `max_retries_per_task`). |
| Hit `goal_timeout_secs` | Same path, wraps the whole `run_tasks` loop. Any still-pending task is marked `failed` with `"goal timeout"`. |
| Hit `circuit_breaker_threshold` | After N consecutive failures, controller emits `ai:info { code: "circuit_open" }`, aborts the goal with `status = "circuit_open"`, remaining tasks marked failed. |

Close the app mid-run: the next startup restores the last persisted
`active_task_tree` from `PROJECT_MEMORY.json`, so you can see where
things stopped. The running processes are killed by the OS when the
app exits — this is not graceful, but it is reliable.

---

## Writing good goals

The planner is the weakest link in most runs. A vague goal produces a
vague plan, and a vague plan sends the executor on a fishing
expedition.

### Good

- *"Extract the JSON validation logic from `src/api/schema.ts` into a
  new `src/api/validation.ts` module, update all callers, keep the
  public API unchanged, run `bun run typecheck` at the end."*
- *"Add a `--json` flag to the CLI in `src/cli/args.ts` that prints
  the output as JSON instead of text. Update the argument parser and
  add a unit test in `src/cli/__tests__/args.test.ts`."*
- *"Run `cargo test --lib`. If any tests fail, read the errors, fix
  them, rerun until all pass. Do not modify tests."*

### Not so good

- *"Make the code better."* — No scope. The planner will invent one
  and it will almost certainly not match what you wanted.
- *"Fix all the bugs."* — No target surface, no reproduction steps.
  The reviewer has no way to decide `OK:` vs `NEEDS_FIX:`.
- *"Add authentication."* — Huge surface, many libraries, many policy
  decisions. Break it into one-library-per-goal steps.

### Rules of thumb

- **State the target files explicitly** when you know them. The planner
  won't pick them correctly at random.
- **State the invariant** ("do not change the public API", "do not
  modify tests", "keep the build green"). These become part of the
  reviewer's prompt and they actually cause `NEEDS_FIX:` when violated.
- **State the success check** ("run `cargo test --lib`", "run `bun run
  typecheck`"). This gives the loop a real termination condition.
- **Keep goals single-purpose.** One refactor, one feature, one bugfix
  per goal. Stack goals rather than cramming them into one.

---

## When to intervene manually

The autonomous loop doesn't replace you; it replaces a tedious sequence
of "read file, edit, run, repeat". Take over when:

- **The reviewer keeps asking for the same fix.** Feedback loops that
  don't converge mean the executor can't extract a new signal from the
  reviewer's instruction, or the reviewer is stuck on a superficial
  issue. Read the last few trace entries and step in.
- **You see the executor propose a write that deletes content you care
  about.** With `autonomous_confirm_irreversible` on, you just click
  Deny. With it off, press Cancel and inspect.
- **The goal completed `status=done` but the system is in a bad state.**
  Don't assume green = correct. Run your tests yourself. The reviewer
  is a model, not a compiler.
- **Cost / time matter.** The goal timeout and circuit breaker are
  safety nets, not optimisers. If you see time going sideways, cancel
  and revisit the goal.

---

## Best practices

- **Always open the project from the app.** Don't try to operate on a
  path you haven't opened as the active project — `PROJECT_MEMORY.json`
  is written into that root and tool paths are resolved under it.
- **Keep the reviewer on.** Turning it off gives a slight speedup but
  removes the retry path; the controller has no way to detect failure
  from the model's perspective.
- **Use Chat for exploration, Goal for execution.** Chat is great for
  "tell me what this module does" or "show me the three places this is
  called from". Goal is for applying a change.
- **Commit between goals.** The controller doesn't version-control its
  changes. If a goal ends in a bad state, `git reset` or `git checkout`
  is how you recover, not the app. Commit clean checkpoints yourself.
- **Watch RAM.** Running the 7B executor + 7B reviewer + the Tauri app
  is tight on 8 GB. Pair `deepseek-coder:6.7b` (executor) with
  `llama3.2:1b` (reviewer) if you're constrained.
- **Don't expect the model to be smarter than it is.** A 6.7B
  code-specialised model is good at local edits and bad at architectural
  decisions. Make the architectural decisions yourself and let the
  system apply them.

---

## Screenshots

_Screenshots are not yet captured in this snapshot._ They will be added
under `docs/screenshots/` once the UI has been exercised against a real
project with real Ollama models responding. Until then, the text in this
guide (UI layout, Trace table, cancel paths, etc.) is the reference —
everything described above reflects the code actually in tree.
