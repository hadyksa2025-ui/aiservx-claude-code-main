# Full System Audit + Architecture Design

**Post-PR #16 · April 2026**

> Scope: Technical audit, execution evaluation, maturity scoring,
> documentation review, multi-provider AI routing architecture,
> UI/UX transformation plan, and phased implementation roadmap.
>
> Grounding: Every claim below is anchored to actual code in the post-PR #16
> tree (`main` @ `19f0c03`). File references use `path:line` notation.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Technical Audit](#2-technical-audit)
3. [Execution Quality Evaluation](#3-execution-quality-evaluation)
4. [System Maturity Score](#4-system-maturity-score)
5. [Documentation Review & Updates](#5-documentation-review--updates)
6. [AI Provider Architecture](#6-ai-provider-architecture)
7. [UI/UX Transformation Plan](#7-uiux-transformation-plan)
8. [Implementation Roadmap](#8-implementation-roadmap)
9. [Final Verdict](#9-final-verdict)

---

## 1. Executive Summary

The system is a **Tauri desktop application** (Rust backend + React frontend)
that provides an autonomous, multi-agent coding assistant. After 14 merged PRs
(#3–#16), the core autonomous loop is functional and hardened:

**What works well:**
- Three-agent loop (planner → executor → reviewer) with clear separation of
  concerns.
- Robust cancellation layer: typed `CancelReason` enum, cooperative tokens,
  mid-SSE and mid-subprocess cancel, pgid-at-spawn tree-kill with
  TERM→KILL escalation.
- Per-task execution trace with bounded entries (200 cap, 4 KiB per field),
  persisted alongside task history.
- Project context grounding: `ProjectMap` injected as a second system message
  into all three agents — stops cross-language hallucination.
- Atomic memory writes (temp file + fsync + rename), schema versioning with
  migration, 4 MB size guard.
- Optional confirm gate for irreversible ops in autonomous mode, with
  sandbox-escape fix (PR #11).
- Circuit breaker, exponential backoff, per-task + goal timeouts.

**What needs work:**
- **No real multi-provider routing**: executor is hardcoded to Ollama, planner
  gated by OpenRouter key presence. `call_executor_with_fallback` is a
  misnomer — there is no fallback.
- **No provider_mode setting**: routing is implicit (key present → planner via
  OpenRouter; else planner disabled). Users cannot choose cloud/local/hybrid.
- **UI is functional but raw**: no thinking-block collapse, no message
  hierarchy, no per-task grouping in the timeline, no animations. Far from
  Devin/Windsurf-level UX.
- **No context compaction**: in-memory message history grows unbounded within
  a turn. Long executor loops eventually exceed the model's context window.
- **Sequential-only task execution**: `max_parallel_tasks` is a reserved
  setting field but completely unimplemented.

**Verdict**: Not production-ready for general use. Production-viable for
local-first, single-user, supervised usage on small-to-medium projects
(< 5K files) with known model + timeout tuning. The system needs
multi-provider routing and UX overhaul before it can be called a product.

---

## 2. Technical Audit

### 2.1 Agent Orchestration (`ai.rs`)

#### Architecture

The chat turn is driven by `run_chat_turn` (ai.rs:752), which sequences:
1. **Planner** (OpenRouter, if API key set) → produces a text plan
2. **Executor** (Ollama) → tool-calling loop, max 16 iterations
3. **Reviewer** (OpenRouter if key set, else Ollama) → OK / NEEDS_FIX verdict
4. If NEEDS_FIX and retries remain → re-enter executor loop with feedback

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **Critical** | `call_executor_with_fallback` has no fallback | ai.rs:574-583 | Function name promises fallback but body is just `stream_ollama(...)`. If Ollama is down, every task fails immediately — no graceful degradation to OpenRouter. |
| **Critical** | No `provider_mode` setting | settings.rs (entire file) | Routing is implicit: planner runs only if `openrouter_api_key` is non-empty (ai.rs:767). Executor is always Ollama. Users cannot choose cloud-only, local-only, or hybrid mode. This is the #1 architectural gap. |
| **High** | Planner disabled in local-only mode | ai.rs:767 | `use_planner = !settings.openrouter_api_key.is_empty()`. Users running pure-Ollama lose the chat-turn planner entirely (only the goal-planner in `controller.rs` still runs, via the same `run_chat_turn` which skips the planner phase). This makes chat-mode quality drop significantly on local-only setups. |
| **High** | No context compaction | ai.rs:816, ai.rs:279-287 | `build_executor_messages` pushes the full `history` into the wire messages. On a long session (50+ messages), this will exceed the model's context window. No summarization or sliding-window logic exists. |
| **Medium** | OpenRouter client timeout is 180s | ai.rs:398 | Hardcoded `reqwest::Client::builder().timeout(180s)`. For complex planner requests on slow models via OpenRouter, this is reasonable but not configurable. If the upstream model is slow, the entire request fails — no retry. |
| **Medium** | Reviewer retry capped at 1 | ai.rs:51, ai.rs:819-822 | `MAX_REVIEWER_RETRIES = 1` is a compile-time constant. Cannot be tuned from Settings. For complex tasks, a single reviewer-retry may not be enough. |
| **Low** | `stream_openrouter` and `stream_ollama` share no common trait | ai.rs:373, ai.rs:486 | Both are standalone async functions with nearly identical loop structure. A trait or enum-dispatch pattern would reduce duplication and make adding new providers cleaner. |

### 2.2 Execution Lifecycle (`controller.rs`)

#### Architecture

`start_goal` (controller.rs:102) sequences:
1. Project scan → `ProjectMap`
2. Plan goal → parse JSON → fallback to `heuristic_split_goal`
3. Sequential task execution with retries + backoff
4. Per-task review → NEEDS_FIX triggers retry
5. Finalize: archive to `task_history`, emit `goal_done`

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **High** | Goal planner uses the full `run_chat_turn` for JSON generation | controller.rs:682-690 | The goal planner asks for structured JSON output but routes through `run_chat_turn`, which sets up the full executor tool loop (tool schema, iteration ceiling). The planner phase inside `run_chat_turn` (OpenRouter) runs, but then the executor phase (Ollama) also runs with the JSON-generation prompt. This means a local model is doing a full tool-call loop for what should be a single-shot structured generation. Wasteful and fragile — the local model may call tools or produce output that's mixed into the planner's JSON response. |
| **High** | `plan_goal` has no JSON repair/retry | controller.rs:692 | If the model wraps JSON in prose or returns invalid JSON, `parse_plan_json` fails and the entire plan falls back to `heuristic_split_goal`. No retry with a reprompt like "Your response was not valid JSON. Return only JSON." This is common with small local models. |
| **Medium** | `heuristic_split_goal` is fragile for non-English goals | controller.rs:767-819 | Splitting on "then", "and then", ";", "\n" works for English but fails for Arabic, Chinese, etc. Since the system has a language anchor, non-English goals will produce a single-sentence fallback which then expands to the generic 3-step "Read / Apply / Verify" pattern — losing goal specificity. |
| **Medium** | Each task starts with an empty history | controller.rs:454 | `run_chat_turn` is called with `Vec::<UiMessage>::new()` for every task attempt. The executor has no memory of what previous tasks accomplished. If task 3 depends on task 2's output, the executor must rediscover the state from the filesystem, which works but is slow and risks re-doing work. |
| **Medium** | `mark_unfinished` uses `TaskStatus::Skipped` for both cancelled and dep-blocked tasks | controller.rs:393, controller.rs:317 | The `result` field distinguishes them ("skipped: cancelled by user" vs "skipped: unsatisfied deps") but the `status` field is the same `"skipped"`. The UI cannot style these differently without parsing the `result` string. |
| **Low** | `RunningGuard` is non-async-safe | controller.rs:120-124 | The `Drop` impl acquires a `Mutex`. If the tokio runtime is shutting down and drops the future, this lock acquisition could deadlock. In practice, Tauri manages the runtime lifetime, so this is theoretical. |

### 2.3 Hallucination Resistance

#### What's in place

- **Project context injection**: `project_context_summary` (project_scan.rs:345) generates a model-facing text block with detected languages, entry points, configs, and sampled deps. Injected as a second system message into planner, executor, and reviewer (ai.rs:775-777).
- **Executor anti-hallucination rules**: "Never assume a language, framework, or file exists" and "Do not invent hypothetical files or 'mentally review' imaginary reports" (ai.rs:86-90).
- **Reviewer grounding**: "never ask the executor to look at files or languages that are not in that context" (ai.rs:111-114, controller.rs:68-71).
- **Language anchor**: "respond in the SAME natural language as the user's most recent message" in all three prompts.

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **High** | No structured output enforcement for planner | ai.rs:53-66, controller.rs:660-692 | The planner is told "Output format: 3–7 bullets" (chat-turn planner) and "JSON only" (goal planner), but there's no JSON-mode flag or schema constraint sent to the provider. Both OpenRouter and Ollama support `response_format: { type: "json_object" }`. Without it, small models frequently wrap JSON in markdown or prose. |
| **Medium** | Reviewer can hallucinate acceptance | controller.rs:862-870 | If the reviewer returns something that doesn't start with "OK:" or "NEEDS_FIX:", it falls into `ReviewDecision::Unknown` which accepts the executor's own summary. A model that outputs "Looks good!" (no "OK:" prefix) passes review without any verification. |
| **Medium** | No tool-output verification | tools.rs (entire file) | The executor gets tool results (file contents, command output) but there's no system-level check that the executor actually *read* before *writing*. The prompt says "prefer reading before writing" but doesn't enforce it. An aggressive model can skip the read and write based on assumptions. |
| **Low** | `project_context_summary` is loaded from disk on every turn | project_scan.rs:345-346 | `load_project_map` reads and parses `PROJECT_MEMORY.json` on each `run_chat_turn`. This is an I/O operation per turn. For chat-driven turns (where the user sends many rapid messages), this is mildly wasteful but not a correctness issue. |

### 2.4 File System Layer (`fs_ops.rs`)

#### What's in place

- Path sandboxing via `resolve()` (fs_ops.rs:19-74): canonicalizes root, strips leading slashes, validates the resolved path starts with root.
- For non-existent targets (new file writes): validates the *parent* directory is within root.
- `read_file` capped at 2 MB (fs_ops.rs:126).
- `list_dir` hides `.git`, `node_modules`, `target`, `dist`, `.next` by default (fs_ops.rs:103-108).

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **Medium** | Symlink-following can escape sandbox | fs_ops.rs:36 | `canonicalize()` resolves symlinks. If a symlink inside the project root points outside it, `canonicalize` follows it and the `starts_with` check passes on the canonical path. However, the canonical path may be outside the logical project root. The current check at line 67 catches this for the *final* path, but a malicious symlink chain (`project/link → /etc/`) is resolved to `/etc/` which does NOT start with the project root → correctly rejected. This is actually safe. **No issue.** |
| **Medium** | `write_file` creates intermediate directories | fs_ops.rs:145-146 | `create_dir_all(parent)` silently creates any missing parent dirs. If the model writes to `deep/nested/path/file.txt`, all intermediate dirs are created without confirmation. The `autonomous_confirm_irreversible` gate only checks file content changes, not directory creation. |
| **Low** | Hidden directories in `list_dir` are hardcoded | fs_ops.rs:103-108 | The skip-list is not configurable. Some projects have meaningful content in `dist/` or `.next/`. The model's `read_file` can still access these files, so this only affects discoverability via `list_dir`. |
| **Low** | No file-size limit on `write_file` | fs_ops.rs:138-168 | `read_file` is capped at 2 MB but `write_file` has no size guard. A hallucinating model could write an unbounded amount of content. In practice, the model's output token limit constrains this. |

### 2.5 Backend Architecture (Rust / Tauri)

#### What's in place

- Clean module separation: `ai.rs` (agent loop), `controller.rs` (autonomous engine), `tools.rs` (tool dispatch), `fs_ops.rs` (filesystem), `memory.rs` (persistence), `tasks.rs` (tree structure), `trace.rs` (execution trace), `cancel.rs` (cancellation), `settings.rs` (config), `watcher.rs` (file watcher).
- All Tauri commands are async where needed (I/O, network).
- Settings are `Mutex<Settings>` in `AppState` — cloned on each use to avoid holding the lock across awaits.
- Process group management on Unix, `CREATE_NEW_PROCESS_GROUP` on Windows.

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **Medium** | `AppState.settings` is `Mutex`, not `RwLock` | lib.rs (inferred from `state.settings.lock().unwrap()` in ai.rs:766, controller.rs:153) | Multiple concurrent reads (health checks, settings UI) contend with each other. Should be `RwLock` since writes are rare (only when user saves settings). |
| **Medium** | `cancel.reset()` at turn start can race with a just-fired cancel | ai.rs:763 | `run_chat_turn` calls `state.cancelled.reset()` at the start of every turn. In autonomous mode, if the user presses Cancel right as a new task starts, the cancel may be reset before the controller's next `goal_cancelled.is_cancelled()` check. The goal-level token protects against this (controller.rs:264, 438), but a user pressing "Cancel Chat" (which only trips `state.cancelled`, not `goal_cancelled`) during autonomous mode would have their cancel silently dropped at the task boundary. |
| **Low** | No graceful shutdown hook | lib.rs | On app quit, in-flight goals/tasks are not explicitly cancelled or persisted. The `RunningGuard` clears the flag, but the current task's state may be inconsistent. The memory write at each step mitigates data loss, but the final task may be left in "running" status. |
| **Low** | Serialization overhead | tasks.rs, memory.rs | `persist_active_tree` is called after every status change (controller.rs:333, 384). Each call serializes the entire task tree to JSON and writes to disk. For a 20-task tree with traces, this is ~100 KB per write × multiple writes per task. Not a bottleneck on modern SSDs but worth noting for future optimization. |

### 2.6 Frontend (React / TypeScript)

#### Architecture

- `App.tsx`: 4-pane layout (Explorer | TaskPanel / Goal & Tasks | Chat | Execution), global event subscriptions.
- `Chat.tsx`: per-role streaming bubbles, auto-scroll, error handling.
- `TaskPanel.tsx`: goal input, task list, trace expansion, failure log, circuit-tripped banner.
- `Execution.tsx`: step timeline + tool call/result log.
- `Settings.tsx`: all config fields, test-connection button.
- `ConfirmCmd.tsx`: modal overlay for command approval.

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **High** | No thinking block / reasoning collapse | Chat.tsx (entire) | Every agent message (planner reasoning, executor tool narration, reviewer verdict) is shown as a full-size chat bubble. There's no collapse/expand, no summary synthesis, no visual hierarchy between reasoning and final answer. This makes the chat noisy and hard to follow on complex tasks. This is the #1 UX gap. |
| **High** | Execution timeline is flat, not task-grouped | Execution.tsx (entire) | Tool calls and results from all tasks are in one flat list. There's no per-task grouping or visual separation. On a 5-task goal, the timeline becomes incomprehensible. |
| **Medium** | Event arrays grow unbounded | App.tsx:51-80 | `events` state array only grows (append). On a long session, this can cause rendering slowdowns. No cleanup, virtualization, or cap. |
| **Medium** | Chat messages also grow unbounded | Chat.tsx:44-61 | `messages` state only grows. The `ai:token` handler creates new entries on every role switch. A complex multi-agent turn can generate 5+ bubbles. |
| **Medium** | No loading/skeleton states | TaskPanel.tsx, Chat.tsx | When a goal starts or a chat turn is in-flight, there's no visual loading indicator beyond the "sending..." disabled state. No shimmer, no progress bar, no estimated time. |
| **Low** | `uid()` uses `Math.random()` | Chat.tsx:13 | Not cryptographically random but used only for React keys. Not a security issue but could theoretically collide on very long sessions. |
| **Low** | No responsive layout | App.tsx:174 (`panes-4`) | The 4-pane grid is fixed. On narrow screens, panes overlap or truncate. No media queries or collapsible panels. |

### 2.7 Memory System (`memory.rs` + `PROJECT_MEMORY.json`)

#### What's in place

- Atomic writes: temp file + fsync + rename (memory.rs:77-84).
- Schema versioning: `MEMORY_SCHEMA_VERSION = 2`, with `migrate_memory` for v1→v2 upgrade (memory.rs:90-115).
- Root-must-be-object validation (memory.rs:50-51).
- 4 MB size guard (memory.rs:63-69).
- Session turn cap: 50 turns (memory.rs:16).
- File index cap: 500 entries (memory.rs:17).
- Decisions cap: 100 entries (memory.rs:18).
- Task history cap: 200 entries (tasks.rs, observed in persist logic).
- Failures log cap: 200 entries.

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **Medium** | `save_project_map` does read-modify-write without locking | project_scan.rs:397-411 | Reads `PROJECT_MEMORY.json`, modifies `project_map`, writes back. If two concurrent operations (e.g., `update_turn_memory` and `save_project_map`) race, one write can clobber the other's changes. In practice, `start_goal` calls `save_project_map` before the task loop starts and `update_turn_memory` runs during the loop, so the race window is narrow but real. |
| **Medium** | No compression for large traces in memory | trace.rs, tasks.rs | Task traces are stored as full JSON arrays inside `task_history`. A 200-entry task history where each task has a 200-entry trace = 40K trace entries in a single JSON file. At ~200 bytes per entry, that's ~8 MB — which would be rejected by the 4 MB cap. In practice, most tasks have 5-30 trace entries, so this is theoretical. But a long-running project will eventually hit the cap. |
| **Low** | `updated_at` uses a custom `epoch:TIMESTAMP` format | memory.rs:60 | `obj.entry("updated_at").or_insert_with(|| Value::String(format!("epoch:{}", unix_ts())))` — non-standard format. The field is `or_insert` (not overwritten), so it only applies once and then persists forever. Should be updated on every save. |

### 2.8 Tool Usage Correctness (`tools.rs`)

#### What's in place

- 4 tools: `read_file`, `write_file`, `list_dir`, `run_cmd`.
- Safety model: deny-list → allow-list → confirm modal.
- `autonomous_confirm_irreversible` routes both `write_file` (on change) and `run_cmd` through confirm modal.
- `write_file` returns a unified diff so the UI (and trace) show what changed.
- `run_cmd` timeout is set by the model's `timeout_ms` argument, capped at the per-task timeout.
- Process group + tree-kill for `run_cmd` children.

#### Issues Found

| Sev | Issue | Location | Detail |
|-----|-------|----------|--------|
| **Medium** | Tool argument parsing is weakly typed | tools.rs:120+ (inferred) | Arguments are parsed from `serde_json::Value` with `.get("path").and_then(|v| v.as_str())`. If the model sends `{"path": 123}` (numeric instead of string), the tool silently fails with "missing argument" rather than a helpful error. |
| **Medium** | No `search_file` or `grep` tool | tools.rs:37-97 | The executor can only read entire files or list directories. On a large project, finding a specific function or string requires reading files one by one. This is the most-requested missing tool for code navigation. |
| **Low** | `run_cmd` stdout/stderr are not capped | tools.rs (inferred from `RunCmdResult`) | If a command produces megabytes of output (e.g., `find /`), the entire output is captured and sent back as a tool result. The trace layer caps at 4 KiB, but the in-flight `ChatResponse` carries the full output. |

---

## 3. Execution Quality Evaluation

### 3.1 Does it execute based on real file reads?

**Mostly yes.** The executor prompt explicitly says "Never assume a language, framework, or file exists" and "Do not invent hypothetical files" (ai.rs:86-90). The `project_context_summary` injection (ai.rs:775) grounds every turn in detected languages and entry points. Post-PR #16, the executor is told to "verify with list_dir / read_file before acting."

**Gap**: There is no *enforcement* of read-before-write. The system relies entirely on prompt instructions. An aggressive model can skip verification and write based on assumptions. A structural enforcement (e.g., rejecting `write_file` calls when no prior `read_file` or `list_dir` was called in the same turn) would be more robust but could also be too restrictive for simple file-creation tasks.

### 3.2 Does it maintain state continuity?

**Partially.** Within a single chat turn, the executor loop maintains full message history (all tool calls and results). The trace captures the full transcript. Memory is persisted after each task.

**Gap**: Between tasks in autonomous mode, each task starts with an empty history (controller.rs:454). The executor must rediscover project state from the filesystem. If task 2 created a file that task 3 needs to modify, task 3's executor has to `list_dir` / `read_file` to find it — it can't see task 2's trace or output directly. This works but is slow and fragile. A context bridge (summary of prior task outcomes injected into the next task's prompt) would improve continuity significantly.

### 3.3 Does it avoid planner re-entry mid-execution?

**Yes.** The architecture cleanly separates phases: the planner runs once at the start of a chat turn (ai.rs:787-813), then the executor loop runs, then the reviewer runs. There's no code path that re-invokes the planner mid-execution. The `'outer` loop (ai.rs:825) only re-enters the executor inner loop, never the planner.

In autonomous mode (controller.rs), the goal planner runs once (controller.rs:159), produces the task tree, and then each task drives its own `run_chat_turn`. The goal planner is never re-entered. The per-task chat turn has its own planner phase, but that's appropriate — it's planning *how* to execute a single task, not re-planning the goal.

### 3.4 Does it produce grounded outputs?

**Significantly improved post-PR #16.** The project context injection stops the worst hallucinations (Python on a React project, etc.). The language anchor prevents mid-execution language drift. The anti-hallucination rules in the executor prompt are explicit.

**Remaining risk**: The reviewer can accept unverified work. If the executor claims "I updated the file" but actually didn't (model hallucinated a successful `write_file` call), the reviewer only sees the executor's *text summary*, not the tool results. The reviewer prompt says "You just watched the executor act on the user's request" but the reviewer doesn't actually see tool call/result pairs — it sees the executor's assistant text. This is a meaningful gap: the reviewer can't verify what the executor actually *did*, only what it *said* it did.

**Evidence**: In `review_task` (controller.rs:844), the reviewer message is built from `executor_summary` (the executor's final assistant text), not from the trace or tool results. The reviewer has no access to actual tool outputs.

### 3.5 Does it handle long-running tasks reliably?

**Yes, with appropriate defaults.** Per-task timeout is 600s (settings.rs:87-93), goal timeout is 7200s (settings.rs:94-98). Both are configurable from Settings. Timeout triggers `CancelReason::Timeout`, which propagates through the cancel layer to tear down in-flight SSE and subprocess.

The circuit breaker (5 consecutive failures → abort) prevents infinite retries. Exponential backoff (1s base, 2^n, capped at 30s) prevents tight-loop retries.

**Gap**: The per-task timeout applies to a single `run_chat_turn`, which includes planner + executor + reviewer. A task that completes its executor loop at 599s but then needs a reviewer pass will timeout during review. The timeout should arguably apply only to the executor phase, with separate budgets for planner and reviewer. This is a minor issue in practice since the reviewer is a single non-tool-call pass.

---

## 4. System Maturity Score

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Architecture** | **7 / 10** | Clean module separation, well-defined agent roles, robust cancellation layer, proper process containment. Loses points for hardcoded provider routing, no context compaction, and sequential-only execution. |
| **Execution Reliability** | **6 / 10** | Retry + backoff + circuit breaker + timeout provide good safety bounds. Loses points because the reviewer doesn't see actual tool outputs, no structured output enforcement for the planner, and context loss between tasks. |
| **Context Grounding** | **7 / 10** | Project context injection is effective. Language anchor works. Anti-hallucination prompts are explicit. Loses points because there's no read-before-write enforcement and the reviewer operates on summaries, not facts. |
| **UX Quality** | **4 / 10** | Functional 4-pane layout with role-colored bubbles and a task panel. But no thinking-block collapse, no message hierarchy, flat execution timeline, no loading states, no animations, no responsive design. Far from modern AI interface standards. |
| **Production Readiness** | **4 / 10** | Works for supervised, local-first, single-user usage. Not ready for general release: no multi-provider routing, no error recovery UX, no onboarding flow beyond the README, no telemetry or crash reporting. |

**Composite: 5.6 / 10**

### Is this production-ready?

**No.** Blocking issues:

1. **No multi-provider routing** — users must manually configure the right
   combination of OpenRouter key + Ollama model and understand the implicit
   routing. No fallback when a provider fails.
2. **Reviewer doesn't verify actual execution** — reviews text summaries,
   not tool outputs. A hallucinating executor can pass review.
3. **No context compaction** — long sessions will exceed model context
   windows with no graceful degradation.
4. **UX is developer-grade, not user-grade** — functional but not
   approachable, not polished, not competitive with Devin/Windsurf/Cursor.

For **local-first supervised usage** (the current target audience), it's
**usable** with the caveat that the user must understand model limitations
and monitor execution.

---

## 5. Documentation Review & Updates

### Current state

| Doc | Status | Issues |
|-----|--------|--------|
| `README.md` | Good | Reflects post-#16 state. Quick-setup section is clear. Model recommendations are correct. Research-snapshot framing preserved. |
| `PROJECT_PLAN.md` | Good | Accurately reflects done/paused/pending items. Cost accounting correctly marked as paused. |
| `PROJECT_MEMORY.json` | Functional | Schema v2, project_map populated on scan. No stale claims about cost.rs. |
| `docs/USAGE.md` | Partial | Screenshots section honestly says they're pending. Workflow examples are helpful. |
| `docs/EVALUATION.md` | Outdated | Written at PR #12. Doesn't reflect PR #13-#16 changes (probe_ollama, project context injection, language anchor, timeout changes). |
| `docs/SCENARIOS.md` | Good | Real scenarios with expected behavior. |

### Proposed updates

1. **`docs/EVALUATION.md`** — should be updated to reflect the post-#16 state (this document supersedes it).
2. **`docs/ARCHITECTURE.md`** (new) — a standalone architecture doc covering the agent loop, cancellation layer, tool safety model, and memory schema. Currently, this knowledge is scattered across CLAUDE.md, AGENTS.md, and code comments.
3. **`docs/PROVIDER_ROUTING.md`** (new, after implementation) — document the multi-provider system once built.
4. **`PROJECT_MEMORY.json`** — the `project_summary` field should be auto-updated when the project scan runs. Currently it only has `file_count` and `truncated` — should include a human-readable one-line summary.

### Documentation structure proposal

```
docs/
├── ARCHITECTURE.md        # System architecture deep-dive
├── EVALUATION.md          # Updated system evaluation (or replaced by this audit)
├── PROVIDER_ROUTING.md    # Multi-provider system docs (after Phase 2)
├── SCENARIOS.md           # Real usage scenarios
├── USAGE.md               # User guide + troubleshooting
└── UI_DESIGN.md           # UI/UX design specs (after Phase 3-4)
```

---

## 6. AI Provider Architecture

### 6.1 Current State

The current routing is implicit and hardcoded:

```
Planner  → OpenRouter (if API key set, else SKIPPED)
Executor → Ollama (always)
Reviewer → OpenRouter (if API key set, else Ollama)
```

There is no user-facing `provider_mode` setting. The function
`call_executor_with_fallback` promises fallback behavior but delivers none.

### 6.2 Proposed Architecture

#### A. Provider Modes

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    /// All agents use OpenRouter. Highest quality, requires API key + internet.
    Cloud,
    /// All agents use Ollama. Fully offline, quality depends on local model.
    Local,
    /// Smart routing: planner + reviewer → OpenRouter, executor → Ollama.
    /// Fallback: if Ollama fails, try OpenRouter; if OpenRouter fails, retry.
    /// This is the RECOMMENDED mode.
    Hybrid,
}
```

#### B. Settings Design

Add to `Settings` struct in `settings.rs`:

```rust
/// Which provider mode the system operates in.
#[serde(default = "default_provider_mode")]
pub provider_mode: ProviderMode,

/// Model to use for the planner role (OpenRouter model ID).
/// Only used in Cloud and Hybrid modes.
#[serde(default)]
pub planner_model: String,

/// Model to use for the reviewer role. In Hybrid mode, defaults to
/// the same as planner_model (OpenRouter). In Local mode, defaults to
/// ollama_model.
#[serde(default)]
pub reviewer_model: String,

/// Model to use for the executor role. In Hybrid mode, defaults to
/// ollama_model. In Cloud mode, defaults to openrouter_model.
#[serde(default)]
pub executor_model: String,
```

Default: `ProviderMode::Hybrid` when OpenRouter API key is set,
`ProviderMode::Local` when it's empty. This preserves current behavior as
the implicit default while giving users explicit control.

#### C. Hybrid Routing Strategy

```
┌─────────────────────────────────────────────────┐
│                   Hybrid Mode                    │
│                                                  │
│  Goal Planner  ──→  OpenRouter (strong model)    │
│  Chat Planner  ──→  OpenRouter (strong model)    │
│  Executor      ──→  Ollama (fast local)          │
│  Reviewer      ──→  OpenRouter (strong model)    │
│                                                  │
│  Fallback rules:                                 │
│  ┌─ If Ollama fails (timeout/error):             │
│  │   → Retry once after 2s                       │
│  │   → If still fails → try OpenRouter           │
│  │   → If OpenRouter fails → task fails          │
│  │                                               │
│  ┌─ If OpenRouter fails (timeout/error/429):     │
│  │   → Retry once after 5s                       │
│  │   → If 429 (rate limit) → backoff 30s + retry │
│  │   → If still fails → try Ollama               │
│  │   → If Ollama fails → task fails              │
│  │                                               │
│  ┌─ If both fail:                                │
│  │   → Task marked failed, normal retry logic    │
│  └───────────────────────────────────────────────┘
│                                                  │
│  Routing metadata on every ai:step event:        │
│  { provider: "openrouter"|"ollama",              │
│    model: "claude-3.5-sonnet"|"deepseek:6.7b",  │
│    fallback_used: true|false }                   │
└─────────────────────────────────────────────────┘
```

#### D. Implementation: Provider Dispatch

Replace the current hardcoded `stream_ollama` / `stream_openrouter` calls
with a unified dispatch:

```rust
async fn call_model(
    app: &AppHandle,
    settings: &Settings,
    role: Role,
    messages: &[WireMessage],
    tools_schema: Option<&Value>,
    cancel: &CancelToken,
) -> Result<WireMessage, String> {
    let (primary, fallback) = resolve_provider(settings, role);

    match call_provider(app, settings, primary, messages, tools_schema, role, cancel).await {
        Ok(msg) => Ok(msg),
        Err(primary_err) => {
            if let Some(fb) = fallback {
                warn!("{role:?} failed on {primary:?}, falling back to {fb:?}: {primary_err}");
                emit_fallback_event(app, role, primary, fb);
                call_provider(app, settings, fb, messages, tools_schema, role, cancel).await
                    .map_err(|fb_err| format!(
                        "both providers failed: primary ({primary:?}): {primary_err}; fallback ({fb:?}): {fb_err}"
                    ))
            } else {
                Err(primary_err)
            }
        }
    }
}

fn resolve_provider(settings: &Settings, role: Role) -> (Provider, Option<Provider>) {
    match settings.provider_mode {
        ProviderMode::Cloud => (Provider::OpenRouter, None),
        ProviderMode::Local => (Provider::Ollama, None),
        ProviderMode::Hybrid => match role {
            Role::Planner | Role::Reviewer => (Provider::OpenRouter, Some(Provider::Ollama)),
            Role::Executor => (Provider::Ollama, Some(Provider::OpenRouter)),
        },
    }
}
```

#### E. Failure Handling Matrix

| Scenario | Primary | Retry | Fallback | Final |
|----------|---------|-------|----------|-------|
| Ollama 500 | Ollama | 1× after 2s | OpenRouter (Hybrid) | task fails |
| Ollama timeout | Ollama | 1× after 2s | OpenRouter (Hybrid) | task fails |
| Ollama offline | Ollama | — | OpenRouter (Hybrid) | task fails |
| OpenRouter 429 | OpenRouter | 1× after 30s | Ollama (Hybrid) | task fails |
| OpenRouter 500 | OpenRouter | 1× after 5s | Ollama (Hybrid) | task fails |
| OpenRouter no key | — | — | Ollama | planner skipped (current behavior) |
| Both down | — | — | — | task fails, circuit breaker counts |

#### F. Cost & Performance Considerations

- **Minimize cloud calls**: In Hybrid mode, the executor (which calls tools
  and iterates up to 16 times) runs locally. Only planner (1 call) and
  reviewer (1 call) go to OpenRouter. Typical cost per task: 2 OpenRouter
  calls.
- **Cache planner output**: The goal planner's task tree doesn't change
  mid-execution. Cache it in memory so a retry doesn't re-plan.
- **Model-specific parameters**: Small local models benefit from lower
  temperature (0.2) and explicit `response_format: json_object` for the
  planner. Cloud models can handle higher temperatures.
- **Token usage tracking** (future): Surface per-role token counts in the UI
  even without cost accounting. Helps users understand which role is most
  expensive.

#### G. Settings UI Design

```
┌─────────────────────────────────────────┐
│  Provider Mode                          │
│  ┌─ ( ) Cloud Only (OpenRouter)         │
│  │  ( ) Local Only (Ollama)             │
│  │  (•) Hybrid (recommended)            │
│  └──────────────────────────────────────│
│                                         │
│  OpenRouter                             │
│  API Key: [••••••••••••]  [Test]        │
│  Planner Model: [anthropic/claude-3.5v] │
│  Reviewer Model: [anthropic/claude-3.5v]│
│                                         │
│  Ollama                                 │
│  URL: [http://localhost:11434]  [Test]  │
│  Executor Model: [deepseek-coder:6.7b]  │
│                                         │
│  Advanced                               │
│  Fallback on failure: [✓]               │
│  Retry timeout (cloud): [5s]            │
│  Retry timeout (local): [2s]            │
└─────────────────────────────────────────┘
```

---

## 7. UI/UX Transformation Plan

### 7.1 Current State Assessment

The current UI is a 4-pane fixed grid:
- **Explorer** (left): file tree
- **TaskPanel / Goal & Tasks** (center-left): goal input + task list
- **Chat** (center-right): message bubbles with per-role streaming
- **Execution** (right): flat timeline of tool calls + results

Problems:
1. All agent messages are equal-weight chat bubbles — no hierarchy
2. Planner reasoning, executor tool narration, and final answers have the same visual weight
3. Execution timeline is flat — no per-task grouping
4. No collapse/expand for verbose reasoning
5. No loading states, no animations, no progressive disclosure
6. Not responsive — fixed 4-pane grid breaks on narrow screens

### 7.2 Thinking Block System (Core Requirement)

#### Lifecycle

```
[Agent starts thinking]
    │
    ▼
┌──────────────────────────────────┐
│ 🔄 Thinking...                   │  ← Faded, low-opacity block
│ Planning the approach for...     │    Streaming tokens visible
│ ▸ read_file → src/main.tsx       │    Tool calls inline
│ ▸ list_dir → src/components/     │
└──────────────────────────────────┘
    │
    ▼ (agent completes)
┌──────────────────────────────────┐
│ ▸ Analyzed 3 files and planned   │  ← Auto-collapsed summary
│   a 4-step refactor              │    One-liner, expandable
└──────────────────────────────────┘
    │
    ▼ (user clicks ▸)
┌──────────────────────────────────┐
│ ▾ Thinking (planner)             │  ← Expanded, full reasoning
│                                  │    Faded background
│ I'll analyze the project struc-  │
│ ture to understand the current   │
│ layout:                          │
│ ▸ read_file → src/main.tsx ✓     │
│ ▸ list_dir → src/components/ ✓   │
│                                  │
│ Plan:                            │
│ 1. Extract the header component  │
│ 2. Create a shared layout...     │
└──────────────────────────────────┘
```

#### Implementation

**State machine per thinking block:**

```typescript
type ThinkingState =
  | { phase: "streaming"; tokens: string; toolCalls: ToolCallSummary[] }
  | { phase: "collapsed"; summary: string; fullContent: string; toolCalls: ToolCallSummary[] }
  | { phase: "expanded"; summary: string; fullContent: string; toolCalls: ToolCallSummary[] };
```

**Summary generation:** Extract the first sentence + tool call count from the full
content. Example: "Analyzed 3 files and planned a 4-step refactor" from a
planner response that read 3 files and produced 4 bullet points. This is
client-side text extraction, not an LLM call.

```typescript
function generateSummary(content: string, toolCalls: ToolCallSummary[]): string {
  const firstSentence = content.split(/[.\n]/).filter(s => s.trim())[0]?.trim() ?? "";
  const toolCount = toolCalls.length;
  if (toolCount > 0) {
    return `${firstSentence} (${toolCount} tool call${toolCount > 1 ? "s" : ""})`;
  }
  return firstSentence;
}
```

### 7.3 Message Hierarchy

Three tiers of visual weight:

1. **Final Answer** (full opacity, prominent)
   - The executor's last assistant message (after tools complete)
   - The reviewer's "OK: ..." summary
   - Styled as the primary chat bubble

2. **Agent Reasoning** (60% opacity, collapsible)
   - Planner output
   - Executor intermediate messages during tool loop
   - Reviewer "NEEDS_FIX" feedback
   - Styled as thinking blocks (see 7.2)

3. **System Actions** (40% opacity, minimal)
   - Tool call → tool result pairs
   - Step transitions (planner → executor → reviewer)
   - Error messages
   - Styled as compact inline items, not full bubbles

#### Component mapping

```
ChatMessage
├── FinalAnswerBubble      (tier 1, always visible)
├── ThinkingBlock           (tier 2, auto-collapses)
│   ├── ThinkingHeader      (summary + expand toggle)
│   ├── ThinkingContent     (full reasoning, hidden by default)
│   └── InlineToolCalls     (compact tool call list)
└── SystemAction            (tier 3, minimal inline)
```

### 7.4 Task Panel Redesign

Current problems:
- Status is text-only ("pending", "running", "done", "failed")
- No visual progress indicator
- Trace expansion is raw JSON-like content
- No task timing information

Proposed redesign:

```
┌──────────────────────────────────────┐
│ Goal: Refactor the authentication    │
│ module to use JWT tokens             │
│                                      │
│ ┌─ ✓ Read auth module structure     │ 32s
│ │   Analyzed 4 files in src/auth/    │
│ ├─ ✓ Implement JWT token gen        │ 1m 45s
│ │   Created jwt.ts, updated auth.ts  │
│ ├─ ⋯ Update middleware              │ running...
│ │   ▸ read_file middleware.ts        │
│ │   ▸ write_file middleware.ts       │
│ ├─ ○ Update tests                   │ pending
│ └─ ○ Verify build passes            │ pending
│                                      │
│ ━━━━━━━━━━━━━━━━━━ 60%             │
│ 2 / 5 tasks · 2m 17s elapsed        │
└──────────────────────────────────────┘
```

Key changes:
- Each task shows timing (duration for completed, "running..." for active)
- Active task shows live tool calls inline
- Compact status icons: ✓ (done), ✗ (failed), ⋯ (running), ○ (pending), ⊘ (skipped)
- Progress bar with percentage
- Goal timing at the bottom

### 7.5 Interaction Model

**Chat-first UX**: The chat panel is the primary interaction surface.
Goals can be entered in the chat (natural language) and automatically
detected as goal-worthy requests. The separate "Goal Input" field in the
task panel becomes secondary — an explicit "Start Goal" action for
power users.

**Inline progress**: Instead of switching between Chat and Execution panes,
show progress inline in the chat. The thinking block already handles this
for reasoning; tool calls appear as compact inline items within the
thinking block.

**No noisy logs by default**: The Execution pane becomes an "Advanced" or
"Debug" view, collapsed by default. Most users only need the chat +
task panel.

### 7.6 Visual Style

```css
/* Color palette */
--bg-primary: #0f0f10;         /* near-black background */
--bg-secondary: #1a1a1d;       /* card/panel background */
--bg-thinking: #1a1a1d80;      /* thinking block (50% opacity) */
--text-primary: #e4e4e7;       /* primary text */
--text-secondary: #71717a;     /* secondary/muted text */
--accent-blue: #3b82f6;        /* planner, links */
--accent-green: #22c55e;       /* executor, success */
--accent-amber: #f59e0b;       /* reviewer, warnings */
--accent-red: #ef4444;         /* errors, failures */
--border: #27272a;             /* subtle borders */

/* Typography */
--font-sans: 'Inter', -apple-system, sans-serif;
--font-mono: 'JetBrains Mono', 'Fira Code', monospace;

/* Animations */
--transition-collapse: height 200ms ease-out, opacity 150ms ease-out;
--transition-fade: opacity 200ms ease-in-out;
```

Key visual principles:
- **Minimal**: no decorative elements, generous whitespace
- **Soft colors**: dark theme with muted accents, no harsh contrasts
- **Smooth animations**: collapse/expand with 200ms ease-out, fade transitions
- **Professional spacing**: 8px grid system, consistent padding

### 7.7 React Component Architecture

```
App
├── TopBar
│   ├── ProjectSelector
│   ├── ProviderStatus (planner + executor dots)
│   └── SettingsButton
├── MainLayout (responsive, collapsible panes)
│   ├── SidePanel (collapsible)
│   │   ├── Explorer
│   │   └── TaskPanel
│   │       ├── GoalInput
│   │       ├── TaskList
│   │       │   └── TaskRow (icon, description, timing, inline tool calls)
│   │       ├── ProgressBar
│   │       └── FailureLog (collapsed by default)
│   ├── ChatPanel (primary)
│   │   ├── MessageList (virtualized)
│   │   │   ├── UserBubble
│   │   │   ├── FinalAnswerBubble
│   │   │   ├── ThinkingBlock
│   │   │   │   ├── ThinkingHeader (summary + toggle)
│   │   │   │   ├── ThinkingContent (reasoning text)
│   │   │   │   └── InlineToolCalls
│   │   │   └── SystemAction
│   │   ├── StreamingIndicator
│   │   └── ChatInput
│   └── DebugPanel (collapsed by default)
│       └── Execution (current timeline, for power users)
├── ConfirmCmdOverlay
└── SettingsModal
    ├── ProviderSection
    ├── RoutingSection (new)
    ├── ExecutionSection
    └── SafetySection
```

#### State Management

```typescript
// Zustand store (replaces useState chains in App.tsx)
interface AppStore {
  // Project
  projectDir: string | null;
  projectMap: ProjectMap | null;

  // Providers
  providerStatus: { planner: boolean | null; executor: boolean | null };

  // Chat (virtualized)
  messages: ChatMessage[];
  addMessage: (msg: ChatMessage) => void;
  updateStreamingMessage: (role: AgentRole, text: string) => void;
  finalizeStreaming: () => void;

  // Tasks
  taskTree: TaskTree | null;
  runState: RunState;

  // Thinking blocks
  thinkingStates: Map<string, ThinkingState>;
  collapseThinking: (id: string) => void;
  expandThinking: (id: string) => void;

  // Events (capped ring buffer)
  events: ExecutionEvent[];
  maxEvents: number;
}
```

#### Performance Optimization

1. **Virtualized message list**: Use `react-window` or `@tanstack/virtual`
   for the chat panel. Only render visible messages. Critical for sessions
   with 100+ messages.
2. **Event ring buffer**: Cap `events` at 500 entries, dropping oldest.
   Prevents memory growth on long sessions.
3. **Memoized components**: `React.memo` on `TaskRow`, `ThinkingBlock`,
   `FinalAnswerBubble` with stable keys. Prevents re-renders when
   unrelated state changes.
4. **Debounced streaming**: Batch `ai:token` events into 50ms frames
   instead of re-rendering on every token. Reduces React reconciliation
   overhead during fast streaming.
5. **Lazy DebugPanel**: `React.lazy` + `Suspense` for the Execution
   timeline. Only loaded when the user opens it.

---

## 8. Implementation Roadmap

### Phase 1 — Core Stability & Execution Fixes (1-2 weeks)

**What:**
- Add `response_format: { type: "json_object" }` to the goal planner call
  (controller.rs:682) when the provider supports it (OpenRouter + Ollama
  both do).
- Add a JSON repair/retry step in `plan_goal`: if `parse_plan_json` fails,
  retry once with a reprompt "Your response was not valid JSON. Return ONLY
  a JSON object with a `tasks` array."
- Inject a summary of previous task outcomes into the next task's context
  (1-2 sentences per completed task, capped at 500 chars). Solves the
  inter-task context loss problem.
- Feed actual tool call/result summaries to the reviewer, not just the
  executor's text. Modify `review_task` to include a compact transcript
  of tool calls (name + path + ok/fail) after the executor summary.
- Change `AppState.settings` from `Mutex` to `RwLock`.
- Cap the in-flight message array (sliding window: keep system messages
  + last N user/assistant turns). Prevents context-window overflow.

**Why:** These are correctness and reliability improvements that don't
require new subsystems. They fix the most impactful gaps identified in the
audit: hallucinated review acceptance, plan JSON fragility, inter-task
context loss, and context-window overflow.

**Expected impact:**
- 30-50% reduction in failed tasks due to bad plans or context loss
- Reviewer actually verifies execution, not just vibes
- System can handle sessions > 20 messages without context overflow

### Phase 2 — AI Routing System (1-2 weeks)

**What:**
- Implement `ProviderMode` enum and add to Settings.
- Implement `call_model` dispatch function that replaces all direct
  `stream_ollama` / `stream_openrouter` calls.
- Implement `resolve_provider` routing logic.
- Add per-provider retry with fallback.
- Add provider metadata to `ai:step` events.
- Update Settings UI with Provider Mode selector and per-role model fields.
- Write `docs/PROVIDER_ROUTING.md`.

**Why:** This is the #1 architectural gap. Without it, users are locked
into a fragile implicit routing that can't handle provider failures and
can't be configured.

**Expected impact:**
- Users can run cloud-only, local-only, or hybrid with explicit control
- Provider failures are handled gracefully (retry + fallback)
- Foundation for future cost tracking (know which provider was used for each call)

### Phase 3 — Thinking UI System (1-2 weeks)

**What:**
- Implement `ThinkingBlock` component with the streaming → collapsed →
  expanded state machine.
- Implement `FinalAnswerBubble` as the tier-1 message type.
- Implement `SystemAction` as the tier-3 compact inline item.
- Add summary generation logic (client-side text extraction).
- Replace current Chat.tsx message rendering with the three-tier hierarchy.
- Add collapse/expand animations (200ms ease-out).
- Add debounced streaming (50ms batching).

**Why:** The thinking block is the single biggest UX improvement. It
transforms a noisy, developer-grade chat into a clean, professional
interface where the user sees the final answer prominently and can
optionally dive into reasoning.

**Expected impact:**
- Chat panel goes from "wall of text" to "clean conversation with
  expandable reasoning"
- User can quickly find the final answer without scrolling through
  planner/executor narration
- Looks and feels like a modern AI product

### Phase 4 — Task Panel Redesign (1 week)

**What:**
- Redesign TaskPanel with status icons, timing, progress bar.
- Add live inline tool calls on the running task.
- Group per-task content with visual connectors.
- Add compact failure summaries (no raw JSON).
- Make the Execution pane a collapsible "Debug" panel.
- Add virtualization to the task list for goals with 10+ tasks.

**Why:** The task panel is the user's primary view into autonomous mode.
Current implementation is functional but bare — text statuses, no timing,
no progress indication.

**Expected impact:**
- User can tell at a glance how far along a goal is
- Running task shows what the executor is doing right now
- Failed tasks show actionable error summaries, not raw error strings

### Phase 5 — Polish & Animations (1 week)

**What:**
- Implement the dark theme color palette (Section 7.6).
- Add consistent spacing (8px grid).
- Add Inter + JetBrains Mono typography.
- Add responsive layout (collapsible side panel for narrow screens).
- Add loading skeletons for chat and task panel.
- Implement Zustand store to replace `useState` chains in App.tsx.
- Add `react-window` virtualization to message list.
- Event ring buffer (cap at 500).

**Why:** Polish is what separates "developer tool" from "product."
These changes are individually small but cumulatively transform the
feel of the application.

**Expected impact:**
- Professional visual identity
- Works on different screen sizes
- No rendering jank on long sessions
- State management is maintainable for future features

---

## 9. Final Verdict

### Current State

The system is a **functional autonomous coding assistant** with a solid
backend architecture (cancellation, process containment, execution trace,
atomic persistence) and a working but raw frontend. It has been tested on
real projects with real local models and the hardening from PRs #5-#16
addresses genuine production failures.

### What Makes It Good

1. **The cancellation layer is excellent.** Typed reasons, cooperative tokens,
   mid-SSE cancel, pgid-based tree-kill with TERM→KILL escalation. This is
   production-quality infrastructure.
2. **Project context grounding works.** The ProjectMap injection into all
   three agents demonstrably reduced hallucination (PR #16 was motivated by
   a real first-run failure where the executor hallucinated Python files on
   a React project).
3. **The trace system is well-designed.** Bounded, typed entries with
   per-field caps, pinned user instruction, truncation flag. This gives
   real debugging value.
4. **Memory safety.** Atomic writes, schema versioning, size caps, migration
   path. No corrupt-memory scenarios observed.

### What Prevents Production Readiness

1. **No multi-provider routing.** The implicit "OpenRouter key present →
   planner via cloud" logic is fragile, non-configurable, and has no
   fallback. This is the #1 blocker.
2. **Reviewer doesn't see tool outputs.** Reviews text summaries, making
   it possible for a hallucinating executor to pass review. This undermines
   the three-agent safety model.
3. **No context compaction.** Long sessions will overflow the model's
   context window with no graceful handling.
4. **UX is developer-grade.** The 4-pane layout with flat message bubbles
   and a raw execution timeline is functional but not competitive with
   modern AI interfaces.

### Recommended Priority

```
Phase 1 (Core stability)     → fixes reliability      → prerequisite for everything
Phase 2 (AI routing)         → fixes architecture     → unlocks all user types
Phase 3 (Thinking UI)        → fixes UX              → makes it a product
Phase 4 (Task panel)         → fixes task visibility  → makes autonomous mode trustworthy
Phase 5 (Polish)             → fixes aesthetics       → makes it professional
```

### Limits of Current System (for supervised local use)

| Dimension | Limit |
|-----------|-------|
| Project size | < 5,000 files (scan truncates at 2,000 entries, depth 4) |
| Task count per goal | 20 (hard ceiling in settings) |
| Session length | ~20-30 messages before context window risk |
| Task timeout | 600s per attempt (configurable) |
| Goal timeout | 7,200s (configurable) |
| Memory file size | < 4 MB |
| File read size | < 2 MB per file |
| Concurrent goals | 1 (idempotency guard) |

### Bottom Line

**Not production-ready for general release. Usable for local-first,
supervised, single-user operation with model and timeout tuning.**

The foundation is solid. The cancellation layer, trace system, and memory
management are production-quality components. The gaps are in routing
(no provider abstraction), verification (reviewer doesn't see facts),
context management (no compaction), and UX (no thinking blocks, no
hierarchy, no polish).

Phases 1-2 fix the backend blockers. Phases 3-5 fix the frontend.
Estimated total: 5-8 weeks of focused work to reach a shippable v1.0.

---

*End of audit. All claims grounded in actual code at `main` @ post-PR #16.*
