# System evaluation & recommendations

This is the engineer's view, not a marketing page. I built the last
eleven PRs on this system and merged them after audit and review. This
document is what I'd tell someone deciding whether to use it for real
work today.

---

## What this system is actually good at

**Controlled iteration on local code with a mid-size model.** That's the
specific thing. Not "understand your entire codebase like Copilot." Not
"agentic planning across repositories." What works is:

- Take a small-to-medium, well-defined goal.
- Let the planner break it into 3–8 tasks.
- Let the executor run through the tool loop on each task.
- Watch, with the ability to stop *instantly* and a full trace to read
  afterward.

It's a bounded, observable, interruptible harness around a local LLM
with tool calling. That's the value proposition and it holds up.

### Concrete strengths (as implemented, not as wished-for)

1. **Cancellation actually works.** This is the thing I'd bet money on.
   Press cancel during a 30-second model stream → the TCP reader
   detaches immediately. Press cancel during `cargo build` → the
   whole process tree is gone (verified by a test that specifically
   checks grandchild survival after a `sleep 60 &`). The typed
   `CancelReason` propagates through the `tokio::select!` race and
   survives the fix in PR #7 so you actually know *why* something
   stopped.

2. **The execution trace is genuinely useful.** Every goal-driven task
   carries a bounded transcript of every message, tool call, tool
   result, retry, and error — persisted inside `PROJECT_MEMORY.json`
   with schema versioning. When a run goes wrong, the trace is where
   the answer is, not the live UI. It survives restart, so you can
   investigate yesterday's run today.

3. **The sandbox is coherent.** One `fs_ops::resolve` helper handles
   every path, including the confirm gate (this was the PR #11 fix —
   the gate was using `Path::join`, which let leading-slash paths
   bypass the check while the actual write resolved through the
   sandbox). After #11, there's exactly one path-resolution code path.
   That's the sort of thing that's easy to miss until it's a CVE.

4. **The bounds are real.** `task_timeout_secs`, `goal_timeout_secs`,
   `max_retries_per_task`, `circuit_breaker_threshold`,
   `max_total_tasks`, `max_iterations` — these are not decorative.
   Every one of them actually trips and actually terminates the loop
   cleanly with the right `CancelReason`. A runaway autonomous goal is
   not a real failure mode here.

5. **The confirm modal is trustworthy.** `write_file` only prompts on
   destructive changes (file exists, content differs) — no
   false-positive prompts for create or no-op rewrites. `run_cmd`
   under `autonomous_confirm_irreversible` prompts every time,
   bypassing the allow-list. The timeout on the modal (10 min) fails
   the task cleanly instead of hanging the loop.

6. **Graceful degradation when things are missing.** No OpenRouter
   key → planner path skipped, executor drives end-to-end. No
   reviewer enabled → retry path skipped but the system still runs.
   Planner returns invalid JSON → heuristic splitter turns the goal
   into a reasonable task list. None of these paths silently break.

---

## Known limitations (real ones)

These are the things that'll bite you, ranked by how often they'll
matter in practice.

### 1. Planner quality is the weakest link

The planner is asked to return a JSON task list for any goal. Local
models at the 1B–7B size do this with ~80% reliability — which sounds
fine but means roughly one in five goals has a bad plan. A bad plan
cascades: the executor follows the plan, the reviewer approves each
task in isolation, and you end up with a goal that "completed
successfully" but doesn't actually do what you asked.

Mitigation today: write specific goals, use the heuristic fallback
(one-task-per-sentence), and read the trace. There's no schema-level
enforcement on the planner output — we parse, and fall back, but we
don't validate tasks against the actual goal.

### 2. Reviewer is a model, not a compiler

The reviewer emits `OK:` / `NEEDS_FIX:` based on reading the executor's
output. It doesn't actually re-run the tests or re-check the build.
A task that says "fix the type error" can go `OK:` even when the type
error is still there, if the reviewer was convinced by a plausible-
looking diff.

Mitigation today: make the *last task in the plan* a verification
command (`bun run typecheck`, `cargo test`, whatever), and read the
`ToolResult` in the trace — `ok=true` from a shell command is a
stronger signal than `OK:` from the reviewer.

### 3. Sequential execution only

`max_parallel_tasks` exists in settings but the runner is sequential.
This is a deliberate choice (simpler, easier to reason about) but it
means a goal with 8 independent tasks takes 8× as long as it could.
For CPU-bound local models that share GPU/RAM anyway, parallelism
wouldn't help much; for mixed workloads (model calls + shell commands)
it would.

### 4. No code-intelligence layer

The executor sees raw bytes of files. It doesn't have an LSP, doesn't
see type information, doesn't know which functions call which. On a
well-named codebase this is fine; on a large, legacy codebase where
understanding requires call-graph traversal, the executor ends up
`grep`-ing for names via `run_cmd` which is slow and fragile.

### 5. Memory grows unboundedly by count, not size

`task_history[]` is capped at 200 entries. Each entry can include a
256-entry trace with 4 KiB strings each — in the worst case that's
~1 MiB per task. The overall memory file is capped at 4 MiB by the
atomic-write helper, so when you push past it, a write fails. In
practice this hasn't tripped because traces are usually much smaller
than worst-case, but it is a real boundary condition we haven't
solved (proper byte-based rotation / archival to a separate file is
the right fix, not done).

### 6. Trace truncation is lossy

`MAX_ENTRIES = 256`. On a long retry loop, the *last* 256 events are
dropped (we keep the first 256 and set `truncated = true`). The
last events are often the most interesting ones (the final error,
the last fix). Keeping a sliding window at the tail would be more
useful than capping at the head. This is a known issue, not a hard
one to fix.

### 7. Model-specific tool-call quirks

Models vary in how they emit tool calls. Some emit OpenAI-compatible
JSON in the `tool_calls` array; some emit free-text `<tool_call>{…}
</tool_call>` blocks; some emit nothing at all and just describe
what they would do. We handle the first two; the third looks like a
silent failure — the executor appears to be "thinking out loud" and
never actually acts. Only mitigation today is switching models.

### 8. The desktop app needs a proper dev/test harness

There's no end-to-end test suite for the UI. The backend has 42
`cargo test --lib` tests covering cancel semantics, trace schema, the
write gate, the planner parse, etc. The frontend has none. Any UI
regression has to be caught manually during development.

### 9. Windows support is claimed but not covered by tests

The process-group and `taskkill` fallback paths exist and compile.
They haven't been verified with a live Windows grandchild-survival
test — our CI runs on the repo host only and there's no Windows
runner. If you're on Windows, expect some friction and be ready to
report bugs.

---

## What would actually improve it next

Based on what I've seen go wrong during real runs, ranked by actual
impact not theoretical elegance.

### High impact

1. **Validate planner output against the goal.** Today we parse the
   JSON list and run with it. A second "plan validator" pass — either
   a model or heuristics — that asks "does this plan actually address
   the goal?" would catch the 20% of bad plans before execution. Low
   effort, high value.

2. **Make the reviewer actually run the verifier.** Instead of asking
   the reviewer "did this work?" in prose, have the controller run a
   user-specified verify command (settable per goal) and feed the
   result to the reviewer. The reviewer's job becomes "interpret the
   verify-command output" instead of "hallucinate whether it
   succeeded." Much cheaper and much more reliable.

3. **Trace: sliding window at the tail, not cap at the head.** One-
   hour fix: keep the last 256 entries instead of the first. The last
   entries are always the most relevant when debugging a failed run.

### Medium impact

4. **Byte-based memory rotation.** Move `task_history[]` to a separate
   rolling file (`history/0001.json`, `0002.json`, …) with a budget
   per file, keep `PROJECT_MEMORY.json` small and for active state
   only. Avoids the 4 MiB ceiling becoming a failure mode.

5. **Parallel independent tasks.** Only worth it if we also thread
   Ollama requests across GPUs (or have enough CPU headroom for
   multiple concurrent model calls). For typical laptop setups this
   is probably not the priority.

6. **LSP integration for at least TypeScript and Rust.** Give the
   executor a `lsp_hover`, `lsp_references`, `lsp_definition` toolset.
   Would replace most of the current `grep`-via-`run_cmd` usage and
   make multi-file refactors dramatically more accurate. Medium
   effort, large quality bump.

### Lower impact (nice to have)

7. **Structured planner JSON via Ollama's native format mode.** Ollama
   supports `format: "json"` now; combined with a Zod-style schema,
   we could guarantee parseable plans. The heuristic fallback works
   well enough that this is polish, not a blocker.

8. **A proper test harness for the frontend.** Playwright or similar.
   Would let us catch UI regressions in CI instead of during manual
   testing.

9. **Windows CI.** One runner, the existing tests, plus a Windows-
   specific `taskkill` grandchild-survival test. Would close the
   "works on my machine" gap.

10. **Cost accounting (paused).** The scaffold is on disk. If you ever
    attach this to a non-local provider, this becomes the #1
    unfinished item. For pure local-first usage, it's correctly
    deprioritised.

### Deliberately not on this list

- **"Agent memory" / long-term context.** The current
  `PROJECT_MEMORY.json` is enough for this system's scope. Adding
  vector-DB-backed "semantic memory" sounds impressive but solves a
  problem this system doesn't have (single-goal horizon, one project
  at a time).

- **"Multi-agent swarm."** We already have three agent roles; adding
  more roles without a concrete task they're good at is cargo-cult.

- **"Self-improving" / meta-agents.** Not a real feature. Build
  concrete tools, not philosophy.

---

## Final verdict

**Is this system usable and stable for local-first usage?** Yes, with
the caveats above. The core — cancel, tree-kill, trace, confirm,
timeouts, circuit breaker, sandbox — all work and are tested. The
outer loop depends on the model you're running. With
`deepseek-coder:6.7b` as executor and `llama3.2:1b` as reviewer on a
machine with 8 GB RAM, you'll get useful work out of it on well-scoped
goals and watch it spin wheels on vague ones. That's expected and it's
the correct behaviour given the model sizes.

**Is it production-ready for running unattended on critical code?**
No. And I wouldn't claim that. The reviewer is a model, not a
compiler. Run with `autonomous_confirm_irreversible` on, review the
trace, commit between goals.

**Is it worth using?** If the alternative is running an LLM CLI by
hand — prompting, copy-pasting code, re-running builds manually —
yes. The visibility + cancel + trace combination is a real quality-
of-life improvement on that workflow, and the setup is a few hundred
megs of model downloads plus a `cargo tauri dev`.
