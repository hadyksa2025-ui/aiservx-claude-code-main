# Real example scenarios

Five scenarios worked through in detail: what you type, what the system
does, and what to look at if you want to debug or verify. These are the
flows the system is actually built for — not hello-world demos.

All scenarios assume:

- `ollama serve` is running.
- An executor model is selected (e.g. `deepseek-coder:6.7b`).
- A reviewer model is selected (e.g. `llama3.2:1b`).
- A project is opened in Explorer.
- Settings are at defaults unless stated.

---

## Scenario 1 — Refactor a module across files

**When to use this**: you know a piece of logic needs to move / be
extracted, you know the target shape, and you want the system to wire
up the callers.

### Goal

> Extract the request-signing helper from `src/http/client.ts` into a
> new file `src/http/sign.ts`. Export `signRequest(req): Promise<Req>`
> from the new file. Update every caller in `src/http/` and
> `src/api/` to import from the new location. Do not change the
> signing behaviour. Run `bun run typecheck` at the end.

Recommended settings for the first run:

- `autonomous_mode = true`
- `autonomous_confirm_irreversible = true` (you'll be prompted to
  approve every write to an existing file and every `run_cmd`)

### What the system does

1. `scan_project` → picks up TypeScript files, `package.json`,
   maps dependencies (emits `project:scan_done`).
2. Planner decomposes into ~5 tasks:
   - Read `src/http/client.ts`, locate the signing logic.
   - Create `src/http/sign.ts` with the extracted function.
   - Update `src/http/client.ts` to import from `sign.ts`.
   - Find all other importers in `src/http/` and `src/api/`, update
     them.
   - Run `bun run typecheck` and fix any type errors.
3. Per task:
   - Executor issues `read_file` / `list_dir` / `write_file` tool calls.
   - Write confirm modal pops for each existing-file change (because
     of `autonomous_confirm_irreversible`).
   - Reviewer checks: did the task actually do what the description
     said? Either `OK:` or `NEEDS_FIX: …` which triggers a retry.
4. Final task runs `bun run typecheck` via `run_cmd`. If it fails, the
   reviewer will flag it and the controller will schedule a retry with
   the type errors as context.

### What to look at

- **Trace under the "update callers" task** — this is where most
  mistakes happen. The executor sometimes misses a caller. Expand the
  trace and scan the `ToolCall list_dir` / `read_file` sequence to
  make sure it actually looked at both directories.
- **The diff in the `write_file` confirm modal** — don't just click
  Allow. The system is only as safe as this modal. For a rename /
  extraction, you should see *additions* in `sign.ts` and
  *removals + one import line* in the updated callers.
- **The final `run_cmd bun run typecheck` result** — `ok=true` is
  the actual success signal, not the reviewer's `OK:`.

---

## Scenario 2 — Fix a failing build

**When to use this**: you have a reproducible build failure, you want
the system to iterate on fixes rather than you re-running the build
manually.

### Goal

> Run `cargo build`. If it fails, read the errors from the output,
> open the relevant source files, fix the errors one by one, and rerun
> `cargo build` until it succeeds. Do not modify `Cargo.toml`. Do not
> modify any test files.

Recommended settings:

- `autonomous_mode = true`
- `autonomous_confirm_irreversible = true` — this is a fix loop
  against live code, you want to see every write.
- `max_retries_per_task = 5` — give the reviewer more attempts.
- `circuit_breaker_threshold = 5` — default. Stop after 5 consecutive
  failures so you don't waste an hour of CPU on a stuck loop.

### What the system does

1. Planner decomposes into a small task list — usually 3 tasks:
   - Run `cargo build` and capture errors.
   - Diagnose and fix errors (one task, run multiple times via
     retries).
   - Run `cargo build` again to verify.
2. The "diagnose and fix" task usually takes multiple retries. Each
   retry:
   - Executor reads the error output from the previous build,
     opens the offending file(s), proposes an edit, writes it (you
     approve).
   - Reviewer either accepts or pushes back with `NEEDS_FIX:`.
3. Circuit breaker protects against a stuck loop where the same fix
   is tried repeatedly.

### What to look at

- **Trace, specifically the `ToolResult run_cmd cargo build` entry.**
  Look at `output` — the actual compiler errors. If the model is
  fixing the wrong error (e.g. picking a warning over an error), step
  in.
- **Retry count on the fix task.** If it's climbing past 3 without
  progress (same error reappearing), cancel and diagnose yourself.
- **The circuit breaker banner** if it fires — this means you've hit
  a real sticking point, not a transient hiccup.

---

## Scenario 3 — Add a new feature using tools

**When to use this**: you want a feature added end-to-end: code +
hookup + test.

### Goal

> Add a `--dry-run` flag to the CLI in `desktop/src-tauri/src/lib.rs`
> entrypoint. When the flag is present, the app should print the
> parsed settings and the opened project root, and exit without
> starting the Tauri window. Add a short test (pure Rust, no UI) in a
> new `#[cfg(test)] mod dry_run_tests` that verifies the flag is
> parsed correctly.

Recommended settings:

- `autonomous_mode = false` for the first pass — review each step in
  chat. Once you're happy with the plan, switch to `true` and let it
  run.
- Keep reviewer on. For a feature-add, the reviewer is what catches
  "you forgot to register the handler" / "you forgot the test".

### What the system does

1. Executor reads `lib.rs` to understand the current argument parser.
2. Proposes an edit to add the flag.
3. Adds the `#[cfg(test)]` module with the test.
4. Optionally runs `cargo test --lib dry_run` to verify the test
   passes.
5. Reviewer looks at the final state and either OKs or asks for
   corrections.

### What to look at

- **The `ToolCall write_file` for `lib.rs` — the full diff.** The
  reviewer rarely catches subtle ordering bugs (e.g. parsing the flag
  after the Tauri builder runs). Eyeball it yourself.
- **The new test's actual assertions in the trace.** Sometimes the
  model writes a test that technically passes but doesn't actually
  assert anything meaningful (`assert!(true)`). Skim the assertions.
- **`cargo test --lib` result** if the executor runs it.

---

## Scenario 4 — Debug a runtime error

**When to use this**: you have a stack trace or an error message from
a running system and want to find the root cause.

### Approach — use Chat, not Goal.

For debugging, the goal path is a trap. The planner will decompose
the debug goal into "find the bug, fix the bug, test the fix" but
you don't yet know what the bug is. Stay in chat and let the model
use `read_file` / `list_dir` / `run_cmd` (grep, rg, find) to localise
the problem.

### Chat session

> User: I'm seeing this panic from the Tauri side on start_goal:
> ```
> thread 'tokio-runtime-worker' panicked at 'called `Option::unwrap()` on a `None` value',
> desktop/src-tauri/src/controller.rs:142:56
> ```
> What's going on there?

Executor will:

1. `read_file desktop/src-tauri/src/controller.rs` and jump to line
   142.
2. Explain what's being unwrapped and under what conditions it could
   be `None`.
3. Suggest a fix (usually propagating the error or matching on `None`
   explicitly).

### Then escalate to a goal

Once you understand the root cause:

> Goal: Replace the `unwrap()` at `desktop/src-tauri/src/controller.rs:142`
> with a match that logs the None case via `eprintln!` and returns
> early with a task failure. Do not change any other code.

Now the goal is narrow enough that the planner can't go wrong.

### What to look at

- **In the chat**: the executor's explanation. If it contradicts the
  actual code you're reading, don't let it proceed to a fix — the
  model has misread the file and a goal built on that misreading will
  make things worse.
- **In the follow-up goal**: the single `write_file` diff should be
  small (a few lines). If it's touching more than a few lines, the
  model is doing more than you asked — Deny and rewrite the goal.

---

## Scenario 5 — Deep trace investigation on a past run

**When to use this**: a goal finished yesterday and you want to
understand exactly what happened — which tools were called, how the
reviewer ruled, what the retry feedback looked like.

### Approach

All completed goals persist into
`PROJECT_MEMORY.json → task_history[]` (cap 200). Each archived task
tree contains every task, each task contains its bounded `trace`
(cap 256 entries, per-string-capped content).

1. Open the project — the TaskPanel will render the most recent tree
   from `active_task_tree` if the goal was still running when the app
   closed, or you can browse `task_history[]` directly from memory.
2. For a completed goal: expand the task, click **Trace** under any
   task in the tree, and read the entries.

Alternatively, read the JSON directly:

```bash
jq '.task_history[-1].tasks[0].trace.entries' PROJECT_MEMORY.json
```

### What to look at

- **The sequence of `Reviewer { verdict: NeedsFix(_) }` entries.**
  Each one should have a different instruction. If the reviewer is
  repeating itself, that's the tell for a stuck loop.
- **The `ToolResult { ok: false }` entries.** These are the actual
  failure events. The reviewer's `NEEDS_FIX` is downstream of them.
- **`Error { role, message }`** — hard failures. These end the task
  immediately (or trigger a retry, bounded by `max_retries_per_task`).
- **`truncated: true`** — the task exceeded `MAX_ENTRIES = 256`. The
  first 256 entries are kept. If you're hitting this often, the task
  is too big; split the goal.

---

## Scenario patterns that don't work well

Worth naming explicitly so you can avoid them.

- **"Rewrite this whole project in Rust."** Out-of-scope. The
  planner can produce a task list but the executor can't hold enough
  context to do a project-scale rewrite in one goal. Break it down —
  one crate / module at a time.
- **"Figure out why the tests are flaky."** Flakiness requires running
  tests many times under varying conditions; the executor runs each
  command once per task. Use chat to investigate and write a
  deterministic reproduction first.
- **"Migrate from library A to library B."** Works if you do it one
  subsystem at a time. Fails as one goal because the planner will
  produce a too-shallow task list and the executor will miss callers.
- **"Add documentation for the whole codebase."** Works per-file but
  the task list is too long for a single goal (max 20 tasks by
  default). Either raise `max_total_tasks` or batch by directory.
