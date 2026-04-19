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
use crate::project_scan;
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
- Do not narrate, do not wrap in markdown — JSON only.
"#;

const TASK_REVIEWER_PROMPT: &str = r#"You are the TASK REVIEWER.

You just watched the executor attempt a single task that is part of a larger
goal. Answer in EXACTLY one of these two forms:

  OK: <one-sentence summary of what was accomplished for this task>

or

  NEEDS_FIX: <one specific, actionable instruction to retry the task>

Use NEEDS_FIX only if the task is not actually done (missing file, bug,
command that clearly failed). If the work is acceptable, answer OK.
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
        let mut running = state.goal_running.lock().unwrap();
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
            *self.0.goal_running.lock().unwrap() = false;
        }
    }
    let _running_guard = RunningGuard(&state);

    // Reset both cancellation flags for a fresh goal. CancelToken.reset()
    // clears the atomic flag without affecting pending waiters; since we
    // are the only task about to await, this is the right moment.
    state.cancelled.reset();
    state.goal_cancelled.reset();

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

    let settings = state.settings.lock().unwrap().clone();
    let max_total = settings.max_total_tasks.max(1) as usize;
    let goal_timeout_secs = settings.goal_timeout_secs;

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
        let context = build_task_message(goal, total, idx, &snapshot, last_feedback.as_deref());

        // Run the multi-agent loop for this task, bounded by task_timeout.
        let fut = ai::run_chat_turn(
            app.clone(),
            state,
            project_dir.to_string(),
            context,
            Vec::<UiMessage>::new(),
        );
        let turn = if task_timeout_secs > 0 {
            match timeout(Duration::from_secs(task_timeout_secs), fut).await {
                Ok(r) => r,
                Err(_) => {
                    if retries >= max_retries {
                        return TaskOutcome::Failed(format!(
                            "task timeout after {task_timeout_secs}s (attempt {})",
                            retries + 1
                        ));
                    }
                    last_feedback = Some(format!(
                        "previous attempt exceeded the {task_timeout_secs}s task timeout; be more concise"
                    ));
                    tasks::bump_task_retries(app, tree, &task_id);
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
                if retries >= max_retries {
                    return TaskOutcome::Failed(format!("executor error: {e}"));
                }
                last_feedback = Some(format!("previous attempt failed: {e}"));
                tasks::bump_task_retries(app, tree, &task_id);
                let _ = tasks::persist_active_tree(project_dir, tree);
                apply_backoff(backoff_base_ms, tree.tasks[idx].retries).await;
                continue;
            }
        };

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
                last_feedback = Some(instr);
                tasks::bump_task_retries(app, tree, &task_id);
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
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "OVERALL GOAL: {goal}\n\nYou are executing task {}/{} of that goal.\n\n",
        idx + 1,
        total
    ));
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
    // to produce ONLY JSON. No tool calls are expected.
    let resp = ai::run_chat_turn(
        app.clone(),
        state,
        project_dir.to_string(),
        full,
        Vec::<UiMessage>::new(),
    )
    .await?;
    let text = resp.assistant.trim().to_string();
    parse_plan_json(&text).ok_or_else(|| format!("planner did not return valid JSON: {text}"))
}

fn parse_plan_json(s: &str) -> Option<Vec<PlanTask>> {
    // Accept either a raw JSON object, a ```json block, or a leading/
    // trailing blurb around a JSON object.
    let stripped = strip_code_fences(s);
    let start = stripped.find('{')?;
    let end = stripped.rfind('}')?;
    if end <= start {
        return None;
    }
    let slice = &stripped[start..=end];
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
    let reviewer_enabled = state.settings.lock().unwrap().reviewer_enabled;
    if !reviewer_enabled || executor_summary.trim().is_empty() {
        return ReviewDecision::Unknown(executor_summary.into());
    }
    let prompt = format!(
        "{TASK_REVIEWER_PROMPT}\n\nGOAL: {goal}\nTASK: {task_desc}\n\nEXECUTOR RESPONSE:\n{executor_summary}"
    );
    let resp = match ai::run_chat_turn(
        app.clone(),
        state,
        project_dir.to_string(),
        prompt,
        Vec::<UiMessage>::new(),
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
}
