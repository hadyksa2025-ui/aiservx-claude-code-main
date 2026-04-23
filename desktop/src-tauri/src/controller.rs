//! Autonomous execution controller.
//!
//! Turns a single high-level user *goal* (e.g. _"refactor this project to
//! improve structure"_) into an ordered task tree and runs each task
//! through the existing multi-agent chat loop (`ai::run_chat_turn`). A
//! reviewer pass decides whether each task is actually complete; failed
//! tasks are retried up to `settings.max_retries_per_task` times before
//! being marked failed and moved on.
//!
//! Entry points (all Tauri commands):
//!
//! - `start_goal(project_dir, goal)` — plan + execute, returns when the
//!   whole tree is done, failed, cancelled, or timed out.
//! - `cancel_goal()` — set the cooperative cancel flag.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter};
use tokio::time::{sleep, timeout};
use tracing::warn;

use crate::ai::{self, UiMessage};
use crate::codegen_envelope::{CodegenEnvelope, EnvelopeFile};
use crate::compiler_gate::{self, CompileOutcome};
use crate::dependency_guard::{self, GuardOutcome};
use crate::fs_ops;
use crate::project_scan;
use crate::run_cmd_gate;
use crate::runtime_validation::{self, RuntimeOutcome};
use crate::security_gate;
use crate::tasks::{self, Task, TaskStatus, TaskTree};
use crate::AppState;

const PLANNER_GOAL_PROMPT: &str = r#"You are the GOAL PLANNER.

The user has given you a single high-level goal for the project. Produce an
ordered task list of concrete, self-contained steps the executor can perform.

Output MUST be a JSON object of the form:

{
  "tasks": [
    { "description": "step 1 ..." },
    { "description": "step 2 ..." }
  ]
}

Rules:
- Each task must be a single, imperative sentence.
- Prefer FEWER, LARGER tasks over many tiny ones. Aim for 3–8 tasks.
- Never include more than MAX_TOTAL tasks.
- Anchor every task in the real project map below — never propose steps
  for languages, frameworks, or files the project clearly does not use.
- Do not narrate, do not wrap in markdown — JSON only.
- Write every task description in the SAME natural language as the user's
  goal. Do not mix languages across tasks.
"#;

const TASK_REVIEWER_PROMPT: &str = r#"You are the TASK REVIEWER.

You just watched the executor attempt a single task that is part of a larger
goal. Answer in EXACTLY one of these two forms:

  OK: <one-sentence summary of what was accomplished for this task>

or

  NEEDS_FIX: <one specific, actionable instruction to retry the task>

Use NEEDS_FIX only if the task is not actually done (missing file, bug,
command that clearly failed). If the work is acceptable, answer OK.

Anchor every NEEDS_FIX in the detected project context (languages,
entry points, configs). Never instruct the executor to look at files or
languages the project does not actually use (e.g. Python files in a
TypeScript project).

Respond in the SAME natural language as the user's goal.
"#;

/// Hard cap on the backoff between task retries. Without this the
/// exponential schedule would rapidly exceed the per-task timeout and
/// dominate the goal's wall-clock budget.
const MAX_RETRY_BACKOFF_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
struct PlanJson {
    tasks: Vec<PlanTask>,
}

#[derive(Debug, Deserialize)]
struct PlanTask {
    description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalResult {
    pub goal_id: String,
    pub status: String,
    pub completed: usize,
    pub failed: usize,
}

// ---------- Tauri commands ----------

#[tauri::command]
pub async fn start_goal(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    project_dir: String,
    goal: String,
) -> Result<GoalResult, String> {
    // Idempotency guard: only one goal may run per process at a time.
    // Another `start_goal` call while one is already in flight would mutate
    // the same shared cancellation flags and emit interleaved task events.
    {
        let mut running = state.lock_goal_running();
        if *running {
            return Err("a goal is already running; cancel it first".into());
        }
        *running = true;
    }
    // RAII guard so the running flag always clears on every exit path
    // (early return, panic, timeout).
    struct RunningGuard<'a>(&'a AppState);
    impl<'a> Drop for RunningGuard<'a> {
        fn drop(&mut self) {
            *self.0.lock_goal_running() = false;
        }
    }
    let _running_guard = RunningGuard(&state);

    // Reset both cancellation flags for a fresh goal. CancelToken.reset()
    // clears the atomic flag without affecting pending waiters; since we
    // are the only task about to await, this is the right moment.
    state.cancelled.reset();
    state.goal_cancelled.reset();

    // Scenario-A §9.2 F-2: let the UI surface a "planning" chip the
    // moment a goal run starts — the scan and planner stream can each
    // take tens of seconds on slow hardware or small local models, and
    // without this the TaskPanel shows only its empty-state placeholder
    // for the entire pre-execution phase.
    tasks::emit_goal_planning(&app, &goal, "scanning");

    // 1. Scan the project so the executor (and the user) has a map.
    let pmap = project_scan::scan_project(&project_dir);
    if let Err(e) = project_scan::save_project_map(&project_dir, &pmap) {
        warn!("saving project_map failed: {e}");
    }
    let _ = app.emit(
        "project:scan_done",
        json!({
            "languages": pmap.languages,
            "entry_points": pmap.entry_points,
            "configs": pmap.configs,
            "dependencies": pmap.dependencies,
            "file_count": pmap.file_count,
            "workspace": pmap.workspace,
            "scan_ms": pmap.scan_ms,
            "truncated": pmap.truncated,
        }),
    );

    let settings = state.read_settings().clone();
    let max_total = settings.max_total_tasks.max(1) as usize;
    let goal_timeout_secs = settings.goal_timeout_secs;

    // Transition the "planning" chip from "scanning project…" to
    // "planner drafting task list…" now that the scan is done. See
    // `emit_goal_planning` docstring. Cleared in two places below: right
    // before `emit_goal_started` (success path), and in the planner-
    // failure heuristic fallback (so the chip never outlives the phase
    // it describes).
    tasks::emit_goal_planning(&app, &goal, "planning");

    // 2. Plan the goal into a task tree.
    let mut tree = TaskTree::new(goal.clone());
    match plan_goal(&app, &state, &project_dir, &goal, max_total, &pmap).await {
        Ok(planned) => {
            for pt in planned.into_iter().take(max_total) {
                let task = tasks::new_task(pt.description, Vec::new());
                tasks::emit_task_added(&app, &tree.id, &task);
                tree.tasks.push(task);
            }
        }
        Err(e) => {
            warn!("goal planner failed, using heuristic fallback: {e}");
            for desc in heuristic_split_goal(&goal, max_total) {
                let task = tasks::new_task(desc, Vec::new());
                tasks::emit_task_added(&app, &tree.id, &task);
                tree.tasks.push(task);
            }
        }
    }
    if tree.tasks.is_empty() {
        let t = tasks::new_task(goal.clone(), Vec::new());
        tasks::emit_task_added(&app, &tree.id, &t);
        tree.tasks.push(t);
    }
    tree.updated_at = tasks::unix_ts();
    let _ = tasks::persist_active_tree(&project_dir, &tree);
    // Planning is over (success path). Clear the pre-execution chip so
    // the TaskPanel switches from "planning…" to the real task list.
    tasks::emit_goal_planning_done(&app);
    tasks::emit_goal_started(&app, &tree);

    // 3. Execute tasks sequentially, bounded by the global goal timeout.
    let inner = run_tasks(&app, &state, &project_dir, &mut tree, &settings);
    let (completed, failed) = if goal_timeout_secs > 0 {
        match timeout(Duration::from_secs(goal_timeout_secs), inner).await {
            Ok(pair) => pair,
            Err(_) => {
                // Trip both tokens so anything still in flight — a
                // streaming SSE request, a long-running `run_cmd`
                // child, a confirm-modal race — sees `Timeout` as the
                // cancel reason and tears down now, instead of leaking
                // past the goal timeout.
                use crate::cancel::CancelReason;
                state.goal_cancelled.cancel_with(CancelReason::Timeout);
                state.cancelled.cancel_with(CancelReason::Timeout);
                tree.status = "timeout".into();
                mark_unfinished(&app, &project_dir, &mut tree, "goal timeout");
                let c = tree.tasks.iter().filter(|t| t.status == TaskStatus::Done.as_str()).count();
                let f = tree.tasks.iter().filter(|t| t.status == TaskStatus::Failed.as_str()).count();
                (c, f)
            }
        }
    } else {
        inner.await
    };

    // 4. Finalize.
    if tree.status == "running" {
        tree.status = if failed == 0 { "done".into() } else { "failed".into() };
    }
    tree.updated_at = tasks::unix_ts();
    let _ = tasks::persist_active_tree(&project_dir, &tree);
    tasks::emit_goal_done(&app, &tree);
    let _ = tasks::archive_active_tree(&project_dir, &tree);

    Ok(GoalResult {
        goal_id: tree.id.clone(),
        status: tree.status.clone(),
        completed,
        failed,
    })
}

#[tauri::command]
pub fn cancel_goal(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Trip both the goal token (observed between tasks) and the chat
    // token (observed inside the in-flight turn) with `CancelReason::Goal`
    // so downstream error strings / events distinguish goal-level cancel
    // from a user pressing Cancel in the chat panel. Every `cancelled()`
    // awaiter (SSE stream, `run_cmd` child wait, confirm-modal race)
    // unwinds now instead of waiting for the next iteration boundary.
    use crate::cancel::CancelReason;
    state.goal_cancelled.cancel_with(CancelReason::Goal);
    state.cancelled.cancel_with(CancelReason::Goal);
    Ok(())
}

// ---------- Core execution loop ----------

async fn run_tasks(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    tree: &mut TaskTree,
    settings: &crate::settings::Settings,
) -> (usize, usize) {
    let max_retries = settings.max_retries_per_task;
    let task_timeout = settings.task_timeout_secs;
    let circuit_threshold = settings.circuit_breaker_threshold;
    let backoff_base = settings.retry_backoff_base_ms;
    // When set, every `run_cmd` and every destructive `write_file`
    // tool call made by the autonomous loop is routed through the
    // confirm modal even if the command is on `cmd_allow_list`.
    let autonomous_confirm = settings.autonomous_confirm_irreversible;

    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut consecutive_failures = 0u32;

    loop {
        if state.goal_cancelled.is_cancelled() {
            tree.status = "cancelled".into();
            mark_unfinished(app, project_dir, tree, "cancelled by user");
            break;
        }

        // Circuit breaker: too many consecutive failures means the model /
        // environment is clearly not making progress — abort rather than
        // keep burning tokens.
        if circuit_threshold > 0 && consecutive_failures >= circuit_threshold {
            tree.status = "failed".into();
            let _ = app.emit(
                "task:circuit_tripped",
                json!({
                    "goal_id": tree.id,
                    "consecutive_failures": consecutive_failures,
                    "threshold": circuit_threshold,
                }),
            );
            mark_unfinished(
                app,
                project_dir,
                tree,
                &format!("circuit breaker tripped after {consecutive_failures} consecutive failures"),
            );
            break;
        }

        // Pick next pending task whose deps are all done. Skipped / failed
        // deps never satisfy — we explicitly surface that below so the
        // loop terminates instead of silently breaking.
        let done_ids: Vec<String> = tree
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Done.as_str())
            .map(|t| t.id.clone())
            .collect();
        let next_idx = tree.tasks.iter().position(|t| {
            t.status == TaskStatus::Pending.as_str()
                && t.deps.iter().all(|d| done_ids.contains(d))
        });
        let Some(idx) = next_idx else {
            // If we still have pending tasks with unsatisfied deps, mark
            // them as skipped with a reason so the UI and memory reflect
            // the real state. This replaces the earlier silent break.
            let unreachable: Vec<String> = tree
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Pending.as_str())
                .map(|t| t.id.clone())
                .collect();
            for tid in unreachable {
                if let Some(i) = tree.tasks.iter().position(|t| t.id == tid) {
                    tree.tasks[i].status = TaskStatus::Skipped.as_str().into();
                    tree.tasks[i].result = Some("skipped: unsatisfied deps".into());
                    tree.tasks[i].updated_at = tasks::unix_ts();
                    let snap = tree.tasks[i].clone();
                    tasks::emit_task_update(app, &tree.id, &snap);
                }
            }
            let _ = tasks::persist_active_tree(project_dir, tree);
            break;
        };

        // Mark running (idempotency: we only ever enter here with status=pending).
        tree.tasks[idx].status = TaskStatus::Running.as_str().into();
        tree.tasks[idx].updated_at = tasks::unix_ts();
        let snapshot = tree.tasks[idx].clone();
        tasks::emit_task_update(app, &tree.id, &snapshot);
        let _ = tasks::persist_active_tree(project_dir, tree);

        // Execute with retries (retries counter now lives in `tree` so the
        // loop actually terminates).
        let outcome = execute_task_with_retries(
            app,
            state,
            project_dir,
            &tree.goal.clone(),
            idx,
            tree,
            max_retries,
            task_timeout,
            backoff_base,
            autonomous_confirm,
        )
        .await;

        match outcome {
            TaskOutcome::Done(summary) => {
                tree.tasks[idx].status = TaskStatus::Done.as_str().into();
                tree.tasks[idx].result = Some(summary);
                completed += 1;
                consecutive_failures = 0;
            }
            TaskOutcome::Failed(err) => {
                tree.tasks[idx].status = TaskStatus::Failed.as_str().into();
                tree.tasks[idx].result = Some(err.clone());
                let task_id = tree.tasks[idx].id.clone();
                let _ = tasks::log_failure(project_dir, &task_id, &err);
                let _ = app.emit(
                    "task:failure_logged",
                    json!({ "task_id": task_id, "error": err }),
                );
                failed += 1;
                consecutive_failures = consecutive_failures.saturating_add(1);
            }
            TaskOutcome::Cancelled => {
                tree.tasks[idx].status = TaskStatus::Skipped.as_str().into();
                tree.status = "cancelled".into();
                tree.tasks[idx].updated_at = tasks::unix_ts();
                let snap = tree.tasks[idx].clone();
                tasks::emit_task_update(app, &tree.id, &snap);
                let _ = tasks::persist_active_tree(project_dir, tree);
                mark_unfinished(app, project_dir, tree, "cancelled by user");
                break;
            }
        }
        tree.tasks[idx].updated_at = tasks::unix_ts();
        let snap = tree.tasks[idx].clone();
        tasks::emit_task_update(app, &tree.id, &snap);
        let _ = tasks::persist_active_tree(project_dir, tree);
    }

    (completed, failed)
}

/// After a terminal condition (cancel / timeout / circuit-trip) mark every
/// still-pending or still-running task as skipped with `reason` so the
/// task tree is always in a consistent state.
fn mark_unfinished(app: &AppHandle, project_dir: &str, tree: &mut TaskTree, reason: &str) {
    let mut dirty = false;
    for i in 0..tree.tasks.len() {
        let s = tree.tasks[i].status.clone();
        if s == TaskStatus::Pending.as_str() || s == TaskStatus::Running.as_str() {
            tree.tasks[i].status = TaskStatus::Skipped.as_str().into();
            tree.tasks[i].result = Some(format!("skipped: {reason}"));
            tree.tasks[i].updated_at = tasks::unix_ts();
            let snap = tree.tasks[i].clone();
            tasks::emit_task_update(app, &tree.id, &snap);
            dirty = true;
        }
    }
    if dirty {
        let _ = tasks::persist_active_tree(project_dir, tree);
    }
}

// ---------- Internal helpers ----------

enum TaskOutcome {
    Done(String),
    Failed(String),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn execute_task_with_retries(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    goal: &str,
    idx: usize,
    tree: &mut TaskTree,
    max_retries: u32,
    task_timeout_secs: u64,
    backoff_base_ms: u64,
    autonomous_confirm: bool,
) -> TaskOutcome {
    let total = tree.tasks.len();
    let task_id = tree.tasks[idx].id.clone();
    let task_desc = tree.tasks[idx].description.clone();
    let mut last_feedback: Option<String> = None;

    loop {
        if state.goal_cancelled.is_cancelled() {
            return TaskOutcome::Cancelled;
        }

        let retries = tree.tasks[idx].retries;
        // Build a per-task message that includes goal context and any
        // reviewer feedback from a prior attempt.
        let snapshot = tree.tasks[idx].clone();
        // Collect completed prior tasks so the executor knows what was
        // already accomplished (solves inter-task context loss).
        let prior_tasks: Vec<Task> = tree.tasks[..idx]
            .iter()
            .filter(|t| t.status == TaskStatus::Done.as_str())
            .cloned()
            .collect();
        let context = build_task_message(goal, total, idx, &snapshot, last_feedback.as_deref(), &prior_tasks);

        // Run the multi-agent loop for this task, bounded by task_timeout.
        let fut = ai::run_chat_turn(
            app.clone(),
            state,
            project_dir.to_string(),
            context,
            Vec::<UiMessage>::new(),
            autonomous_confirm,
            // Autonomous tasks are free-form prose (and tool calls),
            // not a JSON plan. `JsonMode::Off`.
            ai::JsonMode::Off,
        );
        let turn = if task_timeout_secs > 0 {
            match timeout(Duration::from_secs(task_timeout_secs), fut).await {
                Ok(r) => r,
                Err(_) => {
                    let err_msg = format!(
                        "task timeout after {task_timeout_secs}s (attempt {})",
                        retries + 1
                    );
                    append_task_trace_error(app, tree, idx, "controller", &err_msg);
                    let _ = tasks::persist_active_tree(project_dir, tree);
                    if retries >= max_retries {
                        return TaskOutcome::Failed(err_msg);
                    }
                    last_feedback = Some(format!(
                        "previous attempt exceeded the {task_timeout_secs}s task timeout; be more concise"
                    ));
                    tasks::bump_task_retries(app, tree, &task_id);
                    append_task_trace_retry(
                        app,
                        tree,
                        idx,
                        tree.tasks[idx].retries,
                        "task timeout",
                    );
                    let _ = tasks::persist_active_tree(project_dir, tree);
                    apply_backoff(backoff_base_ms, tree.tasks[idx].retries).await;
                    continue;
                }
            }
        } else {
            fut.await
        };

        let resp = match turn {
            Ok(r) => r,
            Err(e) => {
                append_task_trace_error(app, tree, idx, "executor", &e);
                let _ = tasks::persist_active_tree(project_dir, tree);
                if retries >= max_retries {
                    return TaskOutcome::Failed(format!("executor error: {e}"));
                }
                last_feedback = Some(format!("previous attempt failed: {e}"));
                tasks::bump_task_retries(app, tree, &task_id);
                append_task_trace_retry(
                    app,
                    tree,
                    idx,
                    tree.tasks[idx].retries,
                    "executor error",
                );
                let _ = tasks::persist_active_tree(project_dir, tree);
                apply_backoff(backoff_base_ms, tree.tasks[idx].retries).await;
                continue;
            }
        };

        // Merge the per-turn trace onto the task, emit, and persist so
        // the UI sees the transcript incrementally (not just at the end
        // of the goal).
        merge_turn_trace_into_task(app, tree, idx, resp.trace.clone());
        let _ = tasks::persist_active_tree(project_dir, tree);

        // Review: did the executor actually finish the task?
        match review_task(
            app,
            state,
            project_dir,
            goal,
            &task_desc,
            &resp.assistant,
        )
        .await
        {
            ReviewDecision::Ok(summary) => {
                return TaskOutcome::Done(trim_to(&summary, 400));
            }
            ReviewDecision::NeedsFix(instr) => {
                if retries >= max_retries {
                    return TaskOutcome::Failed(format!(
                        "reviewer rejected after {retries} retries: {instr}"
                    ));
                }
                last_feedback = Some(instr.clone());
                tasks::bump_task_retries(app, tree, &task_id);
                append_task_trace_retry(
                    app,
                    tree,
                    idx,
                    tree.tasks[idx].retries,
                    &format!("reviewer NEEDS_FIX: {instr}"),
                );
                let _ = tasks::persist_active_tree(project_dir, tree);
                apply_backoff(backoff_base_ms, tree.tasks[idx].retries).await;
            }
            ReviewDecision::Unknown(fallback) => {
                // No reviewer or unparsed verdict — accept the executor's
                // assistant summary as the task result.
                return TaskOutcome::Done(trim_to(&fallback, 400));
            }
        }
    }
}

/// Append every entry from a just-finished chat turn's trace onto the
/// task's own trace. Runs through `TaskTrace::push` so the per-task cap
/// is enforced across attempts — a runaway tool loop on attempt #3
/// can't silently bloat the persisted tree.
fn merge_turn_trace_into_task(
    app: &AppHandle,
    tree: &mut TaskTree,
    idx: usize,
    turn: crate::trace::TaskTrace,
) {
    if turn.is_empty() {
        return;
    }
    let goal_id = tree.id.clone();
    let task = &mut tree.tasks[idx];
    for entry in turn.entries {
        task.trace.push(entry);
    }
    if turn.truncated {
        task.trace.truncated = true;
    }
    task.updated_at = tasks::unix_ts();
    tasks::emit_task_trace(app, &goal_id, task);
}

fn append_task_trace_retry(
    app: &AppHandle,
    tree: &mut TaskTree,
    idx: usize,
    attempt: u32,
    reason: &str,
) {
    let goal_id = tree.id.clone();
    let task = &mut tree.tasks[idx];
    task.trace.push_retry(attempt, reason, tasks::unix_ts());
    task.updated_at = tasks::unix_ts();
    tasks::emit_task_trace(app, &goal_id, task);
}

fn append_task_trace_error(
    app: &AppHandle,
    tree: &mut TaskTree,
    idx: usize,
    role: &str,
    message: &str,
) {
    let goal_id = tree.id.clone();
    let task = &mut tree.tasks[idx];
    task.trace.push_error(role, message, tasks::unix_ts());
    task.updated_at = tasks::unix_ts();
    tasks::emit_task_trace(app, &goal_id, task);
}

async fn apply_backoff(base_ms: u64, retries: u32) {
    if base_ms == 0 {
        return;
    }
    // Exponential backoff: base * 2^(retries-1), capped at MAX_RETRY_BACKOFF_MS.
    let shift = retries.saturating_sub(1).min(10);
    let delay = base_ms.saturating_mul(1u64 << shift).min(MAX_RETRY_BACKOFF_MS);
    sleep(Duration::from_millis(delay)).await;
}

fn build_task_message(
    goal: &str,
    total: usize,
    idx: usize,
    task: &Task,
    feedback: Option<&str>,
    prior_tasks: &[Task],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "OVERALL GOAL: {goal}\n\nYou are executing task {}/{} of that goal.\n\n",
        idx + 1,
        total
    ));
    // Inject a brief summary of what prior tasks accomplished so the
    // executor has context continuity without needing to rediscover
    // everything from the filesystem. Capped at 500 chars total.
    if !prior_tasks.is_empty() {
        s.push_str("COMPLETED TASKS SO FAR:\n");
        let mut budget = 500usize;
        for (i, pt) in prior_tasks.iter().enumerate() {
            if budget == 0 {
                break;
            }
            let result_text = pt.result.as_deref().unwrap_or("(no summary)");
            let line = format!(
                "  {}. {} → {}\n",
                i + 1,
                pt.description,
                result_text
            );
            let chars: usize = line.chars().count();
            if chars > budget {
                // Truncate the last line to fit.
                let truncated: String = line.chars().take(budget).collect();
                s.push_str(&truncated);
                s.push_str("…\n");
                budget = 0;
            } else {
                s.push_str(&line);
                budget = budget.saturating_sub(chars);
            }
        }
        s.push('\n');
    }
    s.push_str(&format!("CURRENT TASK: {}\n\n", task.description));
    if let Some(fb) = feedback {
        s.push_str(&format!(
            "REVIEWER FEEDBACK from previous attempt:\n{fb}\n\nAddress it this time.\n\n"
        ));
    }
    s.push_str(
        "Complete ONLY this task, use tools if needed, then write a short\n\
         summary of what you did and stop. Do not tackle future tasks.",
    );
    s
}

fn trim_to(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

// ---------- Planner ----------

async fn plan_goal(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    goal: &str,
    max_total: usize,
    pmap: &project_scan::ProjectMap,
) -> Result<Vec<PlanTask>, String> {
    let prompt = PLANNER_GOAL_PROMPT.replace("MAX_TOTAL", &max_total.to_string());
    let ctx = format!(
        "Project map snapshot:\n- root: {}\n- languages: {}\n- entry_points: {}\n- configs (first 8): {}\n\nUser goal: {}",
        pmap.root,
        pmap.languages.join(", "),
        pmap.entry_points.join(", "),
        pmap.configs.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
        goal
    );
    let full = format!("{prompt}\n\n{ctx}");

    // We reuse the normal chat loop but with planner-style intent: tell it
    // to produce ONLY JSON. No tool calls are expected, so
    // `autonomous_confirm` is trivially false here.
    let resp = ai::run_chat_turn(
        app.clone(),
        state,
        project_dir.to_string(),
        full.clone(),
        Vec::<UiMessage>::new(),
        false,
        // Goal planner — force JSON output. On OpenRouter this
        // sets `response_format: {type: "json_object"}`; on Ollama
        // it sets `format: "json"`. Both semantically constrain the
        // model to a valid JSON object, dramatically reducing the
        // prose/markdown wrapping that small local models tend to
        // emit.
        ai::JsonMode::PlannerPlan,
    )
    .await?;
    let text = resp.assistant.trim().to_string();
    if let Some(tasks) = parse_plan_json(&text) {
        return Ok(tasks);
    }

    // JSON repair/retry: small local models frequently wrap JSON in prose
    // or markdown. Retry once with an explicit reprompt.
    // `JSON_REPAIR_RETRIES = 1` (Phase 1.A).
    warn!("plan_goal: first attempt did not return valid JSON, retrying with reprompt");
    let reprompt = format!(
        "{full}\n\n\
         Your previous response was not valid JSON. Return ONLY a JSON object \
         with a \"tasks\" array. No markdown, no prose, no explanation — just \
         the raw JSON object."
    );
    let retry_resp = ai::run_chat_turn(
        app.clone(),
        state,
        project_dir.to_string(),
        reprompt,
        Vec::<UiMessage>::new(),
        false,
        // Retry also in JSON mode — the retry reprompt is explicit
        // about wanting JSON, so enforcement at the API level still
        // helps.
        ai::JsonMode::PlannerPlan,
    )
    .await?;
    let retry_text = retry_resp.assistant.trim().to_string();
    parse_plan_json(&retry_text)
        .ok_or_else(|| format!("planner did not return valid JSON after retry: {retry_text}"))
}

fn parse_plan_json(s: &str) -> Option<Vec<PlanTask>> {
    // Accept either a raw JSON object, a ```json block, or a leading/
    // trailing blurb around a JSON object.
    let stripped = strip_code_fences(s);
    // Find the first balanced JSON object via bracket counting instead
    // of slicing from first '{' to last '}'. This handles models that
    // emit thinking/reasoning text followed by the actual JSON — the
    // old `rfind('}')` approach would concatenate both blobs into an
    // invalid string.
    let slice = extract_first_balanced_json(&stripped)?;
    let parsed: PlanJson = serde_json::from_str(slice).ok()?;
    let tasks: Vec<PlanTask> = parsed
        .tasks
        .into_iter()
        .filter(|t| !t.description.trim().is_empty())
        .collect();
    if tasks.is_empty() {
        return None;
    }
    Some(tasks)
}

/// Extract the first balanced `{ … }` substring from `s` using bracket
/// counting. Returns `None` if no balanced object is found.
fn extract_first_balanced_json(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match b {
            b'\\' if in_string => {
                escape_next = true;
            }
            b'"' => {
                in_string = !in_string;
            }
            b'{' if !in_string => {
                depth += 1;
            }
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_code_fences(s: &str) -> String {
    let mut t = s.trim().to_string();
    if t.starts_with("```") {
        if let Some(first_nl) = t.find('\n') {
            t = t[first_nl + 1..].to_string();
        }
    }
    if t.ends_with("```") {
        t = t[..t.len() - 3].to_string();
    }
    t
}

/// Heuristic goal decomposition used when the planner call fails. Splits
/// the goal on separators that typically appear in multi-step instructions
/// ("then", ";", "and then", "," followed by an imperative verb, etc).
/// Never returns more than `max_total` entries. Always returns at least
/// one entry.
/// Strip leading English conjunctions / connectors that typically appear
/// at the start of a sub-clause after splitting on newlines or semicolons,
/// so the task list doesn't contain "and", "then", or "and then" as a
/// standalone task description.
fn strip_conjunctions(s: &str) -> String {
    let mut t = s.trim().to_string();
    loop {
        let lower = t.to_ascii_lowercase();
        let stripped = lower
            .strip_prefix("and then ")
            .or_else(|| lower.strip_prefix("and "))
            .or_else(|| lower.strip_prefix("then "))
            .or_else(|| lower.strip_prefix("also "))
            .or_else(|| lower.strip_prefix("next "));
        match stripped {
            Some(rest) => {
                // Re-slice original string by the number of bytes consumed
                // so casing is preserved.
                let consumed = t.len() - rest.len();
                t = t[consumed..].trim().to_string();
            }
            None => break,
        }
    }
    // Also handle the case where a chunk is only a conjunction.
    let only = t.to_ascii_lowercase();
    if matches!(only.as_str(), "and" | "then" | "and then" | "also" | "next") {
        return String::new();
    }
    t
}

fn heuristic_split_goal(goal: &str, max_total: usize) -> Vec<String> {
    let trimmed = goal.trim();
    if trimmed.is_empty() {
        return vec![goal.to_string()];
    }
    // Primary separators: "then", ";", " and then ", newlines.
    let mut parts: Vec<String> = Vec::new();
    let lowered = trimmed.to_ascii_lowercase();
    let separators: [&str; 4] = ["\n", ";", " and then ", " then "];
    // Walk the lowered string to find separator indices, then slice the
    // original string at those indices so casing is preserved.
    let mut cursor = 0usize;
    loop {
        let slice = &lowered[cursor..];
        let next = separators
            .iter()
            .filter_map(|sep| slice.find(sep).map(|p| (p, sep.len())))
            .min_by_key(|(p, _)| *p);
        match next {
            Some((pos, sep_len)) => {
                let absolute = cursor + pos;
                let chunk = strip_conjunctions(trimmed[cursor..absolute].trim());
                if !chunk.is_empty() {
                    parts.push(chunk);
                }
                cursor = absolute + sep_len;
            }
            None => {
                let chunk = strip_conjunctions(trimmed[cursor..].trim());
                if !chunk.is_empty() {
                    parts.push(chunk);
                }
                break;
            }
        }
    }
    if parts.is_empty() {
        parts.push(trimmed.to_string());
    }
    // If the user only gave one sentence, fall back to a standard 3-step
    // "explore / apply / verify" pattern framed around their goal.
    if parts.len() == 1 {
        let g = parts.remove(0);
        parts = vec![
            format!("Read the project and identify files relevant to: {g}"),
            format!("Apply the changes needed to: {g}"),
            format!("Verify the result of: {g} (build / run / sanity-check)"),
        ];
    }
    if parts.len() > max_total {
        parts.truncate(max_total);
    }
    parts
}

// ---------- Reviewer ----------

enum ReviewDecision {
    Ok(String),
    NeedsFix(String),
    /// Reviewer disabled, errored, or returned something we couldn't parse.
    /// The caller accepts the executor's own summary.
    Unknown(String),
}

async fn review_task(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    goal: &str,
    task_desc: &str,
    executor_summary: &str,
) -> ReviewDecision {
    let reviewer_enabled = state.read_settings().reviewer_enabled;
    if !reviewer_enabled || executor_summary.trim().is_empty() {
        return ReviewDecision::Unknown(executor_summary.into());
    }
    let prompt = format!(
        "{TASK_REVIEWER_PROMPT}\n\nGOAL: {goal}\nTASK: {task_desc}\n\nEXECUTOR RESPONSE:\n{executor_summary}"
    );
    // Reviewer is a pure-text verdict call — it never invokes tools, so
    // `autonomous_confirm` is trivially false.
    let resp = match ai::run_chat_turn(
        app.clone(),
        state,
        project_dir.to_string(),
        prompt,
        Vec::<UiMessage>::new(),
        false,
        // Task reviewer output is "OK: …" / "NEEDS_FIX: …",
        // never JSON. `JsonMode::Off`.
        ai::JsonMode::Off,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => return ReviewDecision::Unknown(executor_summary.into()),
    };
    let text = resp.assistant.trim().to_string();
    let first_line = text.lines().next().unwrap_or("").trim();
    if let Some(rest) = first_line.strip_prefix("OK:") {
        ReviewDecision::Ok(rest.trim().to_string())
    } else if let Some(rest) = first_line.strip_prefix("NEEDS_FIX:") {
        ReviewDecision::NeedsFix(rest.trim().to_string())
    } else {
        ReviewDecision::Unknown(executor_summary.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_fallback_single_sentence_expands_to_three() {
        let out = heuristic_split_goal("refactor the project to improve structure", 20);
        assert_eq!(out.len(), 3);
        assert!(out[0].starts_with("Read the project"));
        assert!(out[2].starts_with("Verify"));
    }

    #[test]
    fn heuristic_fallback_splits_on_then_semicolons_newlines() {
        let goal = "Add a README; then run the build\nand then commit";
        let out = heuristic_split_goal(goal, 20);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "Add a README");
        assert_eq!(out[1], "run the build");
        assert_eq!(out[2], "commit");
    }

    #[test]
    fn heuristic_fallback_caps_at_max_total() {
        let goal = "a; b; c; d; e; f; g; h";
        let out = heuristic_split_goal(goal, 3);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn parse_plan_rejects_empty_tasks_array() {
        let s = r#"{"tasks": []}"#;
        assert!(parse_plan_json(s).is_none());
    }

    #[test]
    fn parse_plan_accepts_markdown_wrapped_json() {
        let s = "```json\n{\"tasks\":[{\"description\":\"do x\"},{\"description\":\"do y\"}]}\n```";
        let tasks = parse_plan_json(s).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn parse_plan_handles_thinking_prefix_before_json() {
        // Models that emit reasoning text before the JSON object should
        // still parse correctly via balanced bracket extraction.
        let s = r#"Let me think about this...
The project needs these steps:
{"tasks":[{"description":"step one"},{"description":"step two"}]}
That should do it."#;
        let tasks = parse_plan_json(s).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].description, "step one");
    }

    #[test]
    fn parse_plan_handles_two_json_objects() {
        // A model that emits a thinking JSON blob followed by the real
        // plan. The old `rfind('}')` approach would concatenate both
        // into an invalid string.
        let s = r#"{"thinking": "hmm"} ok here is the plan: {"tasks":[{"description":"do x"}]}"#;
        // extract_first_balanced_json picks the first object, which is
        // the thinking blob — it won't parse as PlanJson. But the
        // function should still return None gracefully (not panic).
        // In practice the retry logic will handle this.
        let result = parse_plan_json(s);
        // The first balanced object is {"thinking":"hmm"} which lacks
        // a "tasks" field, so parse fails.
        assert!(result.is_none());
    }

    #[test]
    fn extract_balanced_json_handles_nested_braces_and_strings() {
        let s = r#"prefix {"outer": {"inner": "val with } brace"}} suffix"#;
        let extracted = extract_first_balanced_json(s).unwrap();
        assert_eq!(extracted, r#"{"outer": {"inner": "val with } brace"}}"#);
    }

    #[test]
    fn extract_balanced_json_returns_none_for_unbalanced() {
        assert!(extract_first_balanced_json("no braces here").is_none());
        assert!(extract_first_balanced_json("{unclosed").is_none());
    }
}

// ---------- Codegen envelope application (Phase 1.A J-7 / J-9) ----------

/// Per-file result produced by [`apply_codegen_envelope`]. Mirrors the
/// shape consumed by the UI's codegen panel: the path that was written
/// (sandbox-relative, re-echoed from the envelope), the number of bytes
/// written, and the unified diff against the previous contents so the
/// reviewer UI can surface exactly what changed.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedFile {
    pub path: String,
    pub bytes: usize,
    pub diff: String,
}

/// Outcome of applying a validated envelope to disk. `failed` is
/// non-empty iff one or more files could not be written (e.g. sandbox
/// resolve rejected a path).
///
/// ### Partial commits
///
/// `apply_codegen_envelope` writes files sequentially. If file N of M
/// is rejected by the sandbox, files `1..N` are already on disk when
/// the `failed` entry is recorded. The outer Tauri command
/// [`run_codegen_envelope`] treats any non-empty `failed` as a hard
/// error and returns `Err(..)` so the UI never silently accepts a
/// partially-written project. The struct still carries the partial
/// `applied` list for telemetry / rollback tooling.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedEnvelope {
    pub applied: Vec<AppliedFile>,
    pub failed: Vec<(String, String)>,
    /// Captured but NEVER auto-executed in Phase 1 (user-confirmed
    /// earlier: "parse + validate only, no execution, only surface as
    /// metadata"). Phase 2 security gate will decide execution policy.
    pub run_cmd: Option<String>,
    /// Phase 2.B — populated when `security_gate_execute_enabled` is
    /// on and the envelope carries a `run_cmd`. `None` when execution
    /// is disabled, the envelope has no `run_cmd`, or the command was
    /// whitespace. Callers (UI + §V.3 runtime validation) inspect
    /// `status` to tell Executed from RefusedDangerous / UserDenied /
    /// BlockedByPolicy / ConfirmTimedOut / Skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<run_cmd_gate::ExecutionResult>,
}

/// Write every file in a validated [`CodegenEnvelope`] to disk through
/// the sandbox (`fs_ops::write_file`), emitting `ai:step` telemetry for
/// each file so the Terminal Authority view stays consistent (V6 §VI.1).
///
/// This function assumes the envelope has already passed
/// `codegen_envelope::parse_and_validate` — it does not re-validate
/// paths beyond the sandbox check. Any path rejected by
/// `fs_ops::resolve` is recorded under `failed` with the reason.
pub fn apply_codegen_envelope(
    app: &AppHandle,
    project_dir: &str,
    envelope: &CodegenEnvelope,
) -> AppliedEnvelope {
    let mut applied: Vec<AppliedFile> = Vec::with_capacity(envelope.files.len());
    let mut failed: Vec<(String, String)> = Vec::new();

    for EnvelopeFile { path, content } in &envelope.files {
        match fs_ops::write_file(
            project_dir.to_string(),
            path.clone(),
            content.clone(),
        ) {
            Ok(diff) => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "executor",
                        "label": "codegen.envelope.write",
                        "status": "done",
                        "path": path,
                        "bytes": content.len(),
                    }),
                );
                applied.push(AppliedFile {
                    path: path.clone(),
                    bytes: content.len(),
                    diff,
                });
            }
            Err(e) => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "executor",
                        "label": "codegen.envelope.write",
                        "status": "failed",
                        "path": path,
                        "reason": e,
                    }),
                );
                failed.push((path.clone(), e));
            }
        }
    }

    let status = if failed.is_empty() { "done" } else { "failed" };
    let _ = app.emit(
        "ai:step",
        json!({
            "role": "executor",
            "label": "codegen.envelope.applied",
            "status": status,
            "files": applied.len(),
            "failed": failed.len(),
            "run_cmd": envelope.run_cmd,
        }),
    );

    AppliedEnvelope {
        applied,
        failed,
        run_cmd: envelope.run_cmd.clone(),
        execution: None,
    }
}

/// Build the executor-facing reprompt when the compiler gate has
/// rejected the previous envelope. Keeps the framing consistent with
/// the JSON-repair reprompt in `run_codegen_envelope_turn` — the
/// model sees both the original request and a bulleted diagnostic
/// list keyed by `path(line,col) TSxxxx: message`.
fn build_compile_feedback_prompt(
    original_request: &str,
    diagnostics_feedback: &str,
) -> String {
    format!(
        "{original_request}\n\n\
         [compiler gate] Your previous envelope compiled cleanly against \
         the JSON schema but failed `tsc --noEmit` in an isolated scratch \
         directory. The full list of TypeScript diagnostics:\n\
         {diagnostics_feedback}\n\n\
         Emit a NEW, complete codegen envelope that fixes all of these \
         errors. Every `path` must still be sandbox-relative and every \
         `content` must contain the FULL file contents. Re-emit files \
         you already sent even if only one line changes."
    )
}

/// Build the executor-facing reprompt when the dependency guard
/// (Phase 1.C) rejects the previous envelope. Frames phantom imports
/// as a *project-reality* mismatch so the model doesn't "fix" the
/// problem by inventing another non-existent package.
fn build_dependency_feedback_prompt(
    original_request: &str,
    guard_feedback: &str,
) -> String {
    format!(
        "{original_request}\n\n\
         [dependency guard] Your previous envelope imports packages \
         that are NOT listed in this project's package.json. The \
         project will not compile with those imports in place, and \
         inventing another missing package will not help. Read the \
         miss list below and either rewrite the imports to use a \
         package that is already installed, or drop the feature that \
         requires the missing package:\n\
         {guard_feedback}\n\
         Emit a NEW, complete codegen envelope. Every `path` must \
         still be sandbox-relative and every `content` must contain \
         the FULL file contents."
    )
}

/// Tauri command: run a full codegen envelope lifecycle and, on
/// success, land files into the project sandbox.
///
/// Lifecycle (Phase 1.A + Phase 1.B):
///
/// 1. [`ai::run_codegen_envelope_turn`] — JSON-schema validated
///    envelope with one repair retry (V6 §V.1).
/// 2. [`compiler_gate::skip_policy`] — skip the compile gate when
///    disabled in settings or when the envelope has no `.ts` / `.tsx`
///    files (HTML-only / JSON-only envelopes pay zero cost).
/// 3. [`compiler_gate::prepare_scratch`] — materialise the envelope
///    into `<project>/.oc-titan/scratch/<uuid>/` with copied tsconfig
///    and a `node_modules` symlink.
/// 4. [`compiler_gate::run_tsc`] — `tsc --noEmit` on the scratch dir
///    with a bounded timeout.
/// 5. On `CompileOutcome::Errors` the controller reprompts the model
///    with the structured diagnostics and loops up to
///    `settings.max_compile_retries` additional attempts (V6 §V.2).
/// 6. On `CompileOutcome::Ok` or `Skipped` the envelope is promoted
///    via [`apply_codegen_envelope`], which writes through
///    [`fs_ops::write_file`] (sandbox check, defence-in-depth).
/// 7. Any non-empty `failed` list is converted to an `Err` — the UI
///    never receives a silent partial commit.
///
/// Phase 1 surfaces `run_cmd` on the returned payload but does NOT
/// execute it — that is deferred to Phase 2 behind the command-risk
/// security gate.
#[tauri::command]
pub async fn run_codegen_envelope(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    project_dir: String,
    user_request: String,
    history: Vec<UiMessage>,
    autonomous_confirm: bool,
) -> Result<AppliedEnvelope, String> {
    let (
        gate_enabled,
        max_compile_retries,
        tsc_timeout_secs,
        dep_guard_enabled,
        dep_guard_mode,
        security_gate_enabled,
        security_gate_warning_mode,
        security_gate_execute_enabled,
        runtime_validation_enabled,
    ) = {
        let s = state.read_settings();
        (
            s.compiler_gate_enabled,
            s.max_compile_retries,
            s.tsc_timeout_secs,
            s.dependency_guard_enabled,
            s.dependency_guard_mode.clone(),
            s.security_gate_enabled,
            s.security_gate_warning_mode.clone(),
            s.security_gate_execute_enabled,
            s.runtime_validation_enabled,
        )
    };
    // §V.3 is gated on execution itself — if Phase 2.B is off there's
    // literally nothing to validate, so the feature transparently
    // no-ops regardless of the user's preference.
    let runtime_validation_effective = runtime_validation_enabled && security_gate_execute_enabled;

    let original_request = user_request.clone();
    let mut current_request = user_request;
    let mut last_diagnostics: Option<String> = None;
    let max_attempts = max_compile_retries.saturating_add(1);
    let mut applied_result: Option<AppliedEnvelope> = None;

    for attempt in 0..max_attempts {
        // Envelope chosen for this iteration, set when the dep-guard
        // + compile gate both pass (or cleanly skip). `None` means
        // the iteration already `continue`d to the next attempt via
        // a gate miss and must not reach the apply/execute block.
        let mut envelope_for_apply: Option<CodegenEnvelope> = None;

        let turn = ai::run_codegen_envelope_turn(
            app.clone(),
            state.inner(),
            project_dir.clone(),
            current_request.clone(),
            history.clone(),
            autonomous_confirm,
        )
        .await?;

        // ---- Phase 1.C dependency guard ----
        // Runs before the compiler gate so phantom imports are
        // caught without burning a full `tsc --noEmit` retry slot.
        // Shares `max_compile_retries` with the compiler gate —
        // a guard miss consumes the same attempt budget as a tsc
        // miss, because each one costs a model round-trip.
        let guard_outcome = match dependency_guard::check_envelope(
            std::path::Path::new(&project_dir),
            &turn.envelope,
            dep_guard_enabled,
            &dep_guard_mode,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                // Guard must never be the reason a compile refuses
                // to start — fall back to Skipped and emit a
                // diagnostic rather than bubbling the error.
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "guard",
                        "label": "dependency.error",
                        "status": "warning",
                        "reason": e,
                        "attempt": attempt,
                    }),
                );
                GuardOutcome::Skipped { reason: "internal_error" }
            }
        };
        match &guard_outcome {
            GuardOutcome::Ok { resolved } => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "guard",
                        "label": "dependency.ok",
                        "status": "done",
                        "attempt": attempt,
                        "resolved_count": resolved.len(),
                    }),
                );
            }
            GuardOutcome::Skipped { reason } => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "guard",
                        "label": "dependency.skipped",
                        "status": "done",
                        "reason": reason,
                        "attempt": attempt,
                    }),
                );
            }
            GuardOutcome::Warned { missing, .. } => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "guard",
                        "label": "dependency.warned",
                        "status": "warning",
                        "attempt": attempt,
                        "missing": missing,
                    }),
                );
            }
            GuardOutcome::Missing { missing, .. } => {
                let feedback = dependency_guard::missing_to_feedback(&guard_outcome);
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "guard",
                        "label": "dependency.missing",
                        "status": "failed",
                        "attempt": attempt,
                        "missing": missing,
                    }),
                );
                if attempt + 1 >= max_attempts {
                    return Err(format!(
                        "dependency guard: envelope imports {} unresolved package(s) after {} attempt(s):\n{feedback}",
                        missing.len(),
                        attempt + 1,
                    ));
                }
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "guard",
                        "label": "dependency.retry",
                        "status": "running",
                        "attempt": attempt + 1,
                        "max_attempts": max_attempts,
                    }),
                );
                current_request = build_dependency_feedback_prompt(&original_request, &feedback);
                // Skip the compiler gate this iteration — we already
                // know the envelope is structurally wrong.
                continue;
            }
        }
        // ---- end Phase 1.C dependency guard ----

        if let Some(reason) = compiler_gate::skip_policy(gate_enabled, &turn.envelope) {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "compiler",
                    "label": "compiler.skipped",
                    "status": "done",
                    "reason": reason,
                    "attempt": attempt,
                }),
            );
            envelope_for_apply = Some(turn.envelope);
        } else {

            let scratch = match compiler_gate::prepare_scratch(&project_dir, &turn.envelope).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = app.emit(
                        "ai:step",
                        json!({
                            "role": "compiler",
                            "label": "compiler.scratch_failed",
                            "status": "failed",
                            "reason": e,
                        }),
                    );
                    return Err(format!("compiler gate could not prepare scratch: {e}"));
                }
            };
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "compiler",
                    "label": "compiler.scratch_ready",
                    "status": "running",
                    "attempt": attempt,
                    "uuid": scratch.uuid,
                    "dir": scratch.dir.display().to_string(),
                }),
            );

            let project_path = std::path::Path::new(&project_dir);
            let toolchain_opt = compiler_gate::detect_toolchain(project_path).await;

            if toolchain_opt.is_none() {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "compiler",
                        "label": "compiler.skipped",
                        "status": "done",
                        "reason": "no_toolchain",
                        "attempt": attempt,
                    }),
                );
                // Best-effort cleanup, then promote anyway. This matches
                // the skip_policy contract — the user has no tsc
                // available, so the gate is a no-op.
                let _ = scratch.cleanup().await;
                envelope_for_apply = Some(turn.envelope);
            } else {
                let toolchain = toolchain_opt.expect("checked is_none above");
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "compiler",
                        "label": "compiler.running",
                        "status": "running",
                        "attempt": attempt,
                        "toolchain": toolchain.as_str(),
                        "timeout_secs": tsc_timeout_secs,
                    }),
                );

                let outcome = compiler_gate::run_tsc(&scratch, toolchain, tsc_timeout_secs).await;

                match outcome {
                    CompileOutcome::Ok { .. } => {
                        let _ = app.emit(
                            "ai:step",
                            json!({
                                "role": "compiler",
                                "label": "compiler.ok",
                                "status": "done",
                                "attempt": attempt,
                                "toolchain": toolchain.as_str(),
                            }),
                        );
                        let _ = scratch.cleanup().await;
                        envelope_for_apply = Some(turn.envelope);
                    }
                    CompileOutcome::Timeout {
                        toolchain: tc,
                        after_secs,
                    } => {
                        let _ = app.emit(
                            "ai:step",
                            json!({
                                "role": "compiler",
                                "label": "compiler.timeout",
                                "status": "failed",
                                "attempt": attempt,
                                "toolchain": tc.as_str(),
                                "after_secs": after_secs,
                            }),
                        );
                        let _ = scratch.cleanup().await;
                        return Err(format!(
                            "compiler gate: `tsc --noEmit` exceeded {after_secs}s timeout on attempt {attempt}"
                        ));
                    }
                    CompileOutcome::Errors {
                        toolchain: tc,
                        mut diagnostics,
                        raw_output,
                    } => {
                        compiler_gate::rewrite_paths_relative(&mut diagnostics, &scratch.uuid);
                        let feedback = compiler_gate::diagnostics_to_feedback(&diagnostics);
                        let _ = app.emit(
                            "ai:step",
                            json!({
                                "role": "compiler",
                                "label": "compiler.errors",
                                "status": "failed",
                                "attempt": attempt,
                                "toolchain": tc.as_str(),
                                "diagnostic_count": diagnostics.len(),
                                "diagnostics": diagnostics,
                            }),
                        );
                        let _ = scratch.cleanup().await;

                        last_diagnostics = Some(feedback.clone());
                        if attempt + 1 >= max_attempts {
                            warn!(
                                "compiler gate exhausted retries after {attempt} attempt(s); raw tsc output was: {}",
                                truncate_for_log(&raw_output)
                            );
                            return Err(format!(
                                "compiler gate: tsc reported {} error(s) after {} attempt(s):\n{feedback}",
                                diagnostics.len(),
                                attempt + 1,
                            ));
                        }
                        let _ = app.emit(
                            "ai:step",
                            json!({
                                "role": "compiler",
                                "label": "compiler.retry",
                                "status": "running",
                                "attempt": attempt + 1,
                                "max_attempts": max_attempts,
                            }),
                        );
                        current_request = build_compile_feedback_prompt(&original_request, &feedback);
                    }
                }
            } // end `else` of `if toolchain_opt.is_none()` — tsc invocation block
        } // end `else` of `if let Some(reason) = compiler_gate::skip_policy(...)`

        // If the compile gate set `envelope_for_apply`, promote the
        // envelope now. Otherwise the compile gate emitted an
        // `errors` diagnostic + queued a retry via `current_request`
        // and we fall through to the next attempt.
        let Some(envelope) = envelope_for_apply else {
            continue;
        };

        let mut result = apply_codegen_envelope(&app, &project_dir, &envelope);
        if !result.failed.is_empty() {
            let summary = result
                .failed
                .iter()
                .map(|(p, e)| format!("- {p}: {e}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "codegen envelope partially failed: {}/{} files could not be written:\n{summary}",
                result.failed.len(),
                result.failed.len() + result.applied.len()
            ));
        }
        let _ = app.emit(
            "ai:step",
            json!({
                "role": "compiler",
                "label": "compiler.promoted",
                "status": "done",
                "attempt": attempt,
                "files": result.applied.len(),
                "run_cmd": result.run_cmd,
            }),
        );

        // Phase 2.A (V6 §VII.2) — classify any surfaced `run_cmd` and
        // emit a `security.classified` event so the UI can render risk
        // before Phase 2.B wires up actual execution. The classifier is
        // deterministic (same input → same output) and has no side
        // effects, so it's safe to run unconditionally when `run_cmd`
        // is present.
        //
        // Phase 2.B (V6 §VII.2 + §V.3 hook) — when
        // `security_gate_execute_enabled` is on, we hand the classified
        // command to `run_cmd_gate::execute_run_cmd` which replays the
        // classification through the policy layer (Safe/AutoRun,
        // Warning/prompt-or-allow-or-block with allow-list + autonomous
        // override, Dangerous/refuse-or-prompt) and, if greenlit,
        // dispatches through the existing `tools::run_cmd_impl` runner.
        // This reuses the in-production child-spawn + cancel + tree-kill
        // machinery — no new execution engine.
        if security_gate_enabled {
            if let Some(cmd) = result.run_cmd.clone() {
                let classification = security_gate::classify(&cmd);
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "security",
                        "label": "security.classified",
                        "status": classification.class.as_event_status(),
                        "class": classification.class,
                        "reason": classification.reason,
                        "matched_rule": classification.matched_rule,
                        "compound": classification.compound,
                        "warning_mode": security_gate_warning_mode,
                        "run_cmd": cmd,
                    }),
                );

                if security_gate_execute_enabled {
                    let goal_cancel = state.goal_cancelled.clone();
                    // Thread the per-request `autonomous_confirm`
                    // flag through as an override so a one-off
                    // request can demand the confirm modal regardless
                    // of the persisted
                    // `autonomous_confirm_irreversible` setting.
                    // PR-I fix for Devin Review PR-H comment
                    // #3120677225.
                    match run_cmd_gate::execute_run_cmd(
                        Some(&app),
                        Some(state.inner()),
                        &project_dir,
                        &cmd,
                        Some(&goal_cancel),
                        Some(autonomous_confirm),
                    )
                    .await
                    {
                        Ok(exec) => {
                            result.execution = Some(exec);
                        }
                        Err(e) => {
                            // Infra-level failure (invalid project
                            // root, spawn error). Surface as a step
                            // event but do not fail the envelope —
                            // the files are already on disk and the
                            // user can retry the command manually
                            // from the terminal panel.
                            let _ = app.emit(
                                "ai:step",
                                json!({
                                    "role": "execution",
                                    "label": "run_cmd.error",
                                    "status": "error",
                                    "error": e,
                                    "cmd": cmd,
                                }),
                            );
                        }
                    }
                }
            }
        }

        // ---- Phase §V.3 runtime validation ----
        // Consumes `ExecutionResult.exit_code` + `stderr_tail` from
        // Phase 2.B. Non-zero exit on `Executed` status reprompts
        // the model with the tails attached and consumes one
        // attempt from the shared `max_compile_retries` budget
        // (same budget as the compiler gate + dependency guard).
        // Every non-Executed status (refused / denied / blocked /
        // skipped) short-circuits to `Skipped` so policy refusals
        // cannot exhaust the retry budget on their own.
        let runtime_outcome = runtime_validation::evaluate(
            result.execution.as_ref(),
            runtime_validation_effective,
        );
        match runtime_outcome {
            RuntimeOutcome::Ok {
                exit_code,
                duration_ms,
            } => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "runtime",
                        "label": "runtime.ok",
                        "status": "done",
                        "attempt": attempt,
                        "exit_code": exit_code,
                        "duration_ms": duration_ms,
                    }),
                );
                applied_result = Some(result);
                break;
            }
            RuntimeOutcome::Skipped { reason } => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "runtime",
                        "label": "runtime.skipped",
                        "status": "skipped",
                        "attempt": attempt,
                        "reason": reason,
                    }),
                );
                applied_result = Some(result);
                break;
            }
            RuntimeOutcome::Errors {
                exit_code,
                stderr_tail,
                stdout_tail,
                duration_ms,
            } => {
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "runtime",
                        "label": "runtime.errors",
                        "status": "failed",
                        "attempt": attempt,
                        "exit_code": exit_code,
                        "duration_ms": duration_ms,
                        "stderr_tail": stderr_tail,
                        "stdout_tail": stdout_tail,
                    }),
                );
                if attempt + 1 >= max_attempts {
                    let _ = app.emit(
                        "ai:step",
                        json!({
                            "role": "runtime",
                            "label": "runtime.exhausted",
                            "status": "failed",
                            "attempt": attempt,
                            "max_attempts": max_attempts,
                            "exit_code": exit_code,
                        }),
                    );
                    warn!(
                        "runtime validation exhausted retries after {} attempt(s); stderr tail: {}",
                        attempt + 1,
                        truncate_for_log(&stderr_tail)
                    );
                    return Err(format!(
                        "runtime validation: `run_cmd` exited with code {exit_code} after {} attempt(s):\nstderr: {}",
                        attempt + 1,
                        truncate_for_log(&stderr_tail)
                    ));
                }
                let _ = app.emit(
                    "ai:step",
                    json!({
                        "role": "runtime",
                        "label": "runtime.retry",
                        "status": "running",
                        "attempt": attempt + 1,
                        "max_attempts": max_attempts,
                    }),
                );
                last_diagnostics = Some(stderr_tail.clone());
                current_request = runtime_validation::build_reprompt(
                    &original_request,
                    exit_code,
                    &stderr_tail,
                    &stdout_tail,
                );
                // Fall through to the next iteration.
            }
        }
    }

    // `last_diagnostics` is retained for future telemetry sinks — it
    // contains the most recent successful-repair feedback, which is
    // useful for post-mortem / failure-memory aggregation (V6 §III.1).
    let _ = last_diagnostics;
    applied_result
        .ok_or_else(|| "codegen envelope loop exited without applying an envelope".to_string())
}

/// Phase 2.A (V6 §VII.2) — Tauri command that classifies a single
/// `run_cmd` string and returns the full [`security_gate::Classification`].
///
/// Exposed so the UI can preview the risk of a command before it is
/// ever executed. Pure function wrapper: no filesystem access, no
/// network, no LLM — the same input always returns the same output.
/// Phase 2.B will consume the same classifier inside the execution
/// layer; keeping this command additive lets the UI surface
/// classification today without depending on execution wiring.
#[tauri::command]
pub fn classify_run_cmd(cmd: String) -> security_gate::Classification {
    security_gate::classify(&cmd)
}

/// Truncate a long tsc output for log lines so a catastrophic compile
/// failure does not flood the trace. ~4 K characters keeps enough
/// context to diagnose issues without blowing up log storage.
///
/// Uses `chars().take()` rather than a byte slice — tsc is
/// UTF-8-aware and error messages frequently contain non-ASCII
/// (quoted source snippets, file paths with BMP characters, the Unicode
/// arrow tsc sometimes uses for "expected vs got"). A raw byte slice
/// would panic with `byte index X is not a char boundary` if the cut
/// fell mid-codepoint, turning a compile failure into a backend crash.
fn truncate_for_log(s: &str) -> String {
    const MAX_CHARS: usize = 4096;
    let total_chars = s.chars().count();
    if total_chars <= MAX_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_CHARS).collect();
        out.push_str(&format!(
            "… ({} chars truncated)",
            total_chars - MAX_CHARS
        ));
        out
    }
}

#[cfg(test)]
mod truncate_for_log_tests {
    use super::truncate_for_log;

    #[test]
    fn short_string_is_returned_as_is() {
        assert_eq!(truncate_for_log("hello"), "hello");
    }

    #[test]
    fn ascii_boundary_does_not_panic() {
        let s = "a".repeat(5000);
        let out = truncate_for_log(&s);
        assert!(out.contains("chars truncated"));
        assert!(out.starts_with(&"a".repeat(4096)));
    }

    #[test]
    fn multi_byte_boundary_does_not_panic() {
        // Arabic "ا" is a 2-byte codepoint. We need *more than
        // MAX_CHARS* characters to actually enter the truncation
        // branch — the early Devin Review on PR-C caught that the
        // previous `.repeat(3000)` left this test dormant inside the
        // `total_chars <= MAX_CHARS` short-circuit. 5000 × 2 bytes =
        // 10 000 bytes, and the naive byte slice `s[..4096]` would
        // land mid-codepoint and panic. The char-aware version must
        // truncate cleanly and keep every surviving char as `ا`.
        let s = "ا".repeat(5000);
        let out = truncate_for_log(&s);
        assert!(out.contains("chars truncated"));
        assert_eq!(
            out.chars().take(4096).filter(|&c| c == 'ا').count(),
            4096,
            "first 4096 chars must still be intact Arabic letters"
        );
    }

    #[test]
    fn four_byte_emoji_boundary_does_not_panic() {
        // "🔥" is a 4-byte codepoint — the worst-case boundary for a
        // byte-slicing truncator. Again we need > MAX_CHARS to hit
        // the truncation branch, not just > MAX_CHARS*bytes.
        let s = "🔥".repeat(5000);
        let out = truncate_for_log(&s);
        assert!(out.contains("chars truncated"));
        assert_eq!(
            out.chars().take(4096).filter(|&c| c == '🔥').count(),
            4096,
            "first 4096 chars must still be intact 🔥 codepoints"
        );
    }
}
