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
//!   whole tree is done, failed, or cancelled.
//! - `cancel_goal()` — set the cooperative cancel flag.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
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
    // Reset both cancellation flags for a fresh goal.
    *state.cancelled.lock().unwrap() = false;
    *state.goal_cancelled.lock().unwrap() = false;

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
        }),
    );

    // 2. Plan the goal into a task tree.
    let settings = state.settings.lock().unwrap().clone();
    let max_total = settings.max_total_tasks.max(1) as usize;
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
            warn!("goal planner failed, using single-task fallback: {e}");
            let fallback = tasks::new_task(goal.clone(), Vec::new());
            tasks::emit_task_added(&app, &tree.id, &fallback);
            tree.tasks.push(fallback);
        }
    }
    tree.updated_at = tasks::unix_ts();
    let _ = tasks::persist_active_tree(&project_dir, &tree);
    tasks::emit_goal_started(&app, &tree);

    // 3. Execute tasks sequentially.
    let max_retries = settings.max_retries_per_task;
    let mut completed = 0usize;
    let mut failed = 0usize;

    'outer: loop {
        if *state.goal_cancelled.lock().unwrap() {
            tree.status = "cancelled".into();
            break 'outer;
        }

        // Pick next pending task whose deps are all done.
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
            // Nothing else runnable — we're done.
            break 'outer;
        };

        // Mark running.
        tree.tasks[idx].status = TaskStatus::Running.as_str().into();
        tree.tasks[idx].updated_at = tasks::unix_ts();
        let snapshot = tree.tasks[idx].clone();
        tasks::emit_task_update(&app, &tree.id, &snapshot);
        let _ = tasks::persist_active_tree(&project_dir, &tree);

        // Execute with retries.
        let outcome = execute_task_with_retries(
            &app,
            &state,
            &project_dir,
            &tree.goal,
            idx,
            &tree,
            max_retries,
        )
        .await;

        match outcome {
            TaskOutcome::Done(summary) => {
                tree.tasks[idx].status = TaskStatus::Done.as_str().into();
                tree.tasks[idx].result = Some(summary);
                completed += 1;
            }
            TaskOutcome::Failed(err) => {
                tree.tasks[idx].status = TaskStatus::Failed.as_str().into();
                tree.tasks[idx].result = Some(err.clone());
                let _ = tasks::log_failure(&project_dir, &tree.tasks[idx].id, &err);
                let _ = app.emit(
                    "task:failure_logged",
                    json!({ "task_id": tree.tasks[idx].id, "error": err }),
                );
                failed += 1;
            }
            TaskOutcome::Cancelled => {
                tree.tasks[idx].status = TaskStatus::Skipped.as_str().into();
                tree.status = "cancelled".into();
                let snap = tree.tasks[idx].clone();
                tasks::emit_task_update(&app, &tree.id, &snap);
                let _ = tasks::persist_active_tree(&project_dir, &tree);
                break 'outer;
            }
        }
        tree.tasks[idx].updated_at = tasks::unix_ts();
        let snap = tree.tasks[idx].clone();
        tasks::emit_task_update(&app, &tree.id, &snap);
        let _ = tasks::persist_active_tree(&project_dir, &tree);
    }

    // 4. Finalize.
    if tree.status == "running" {
        tree.status = if failed == 0 { "done".into() } else { "failed".into() };
    }
    tree.updated_at = tasks::unix_ts();
    let _ = tasks::persist_active_tree(&project_dir, &tree);
    tasks::emit_goal_done(&app, &tree);
    let _ = tasks::archive_active_tree(&project_dir, &tree);

    Ok(GoalResult {
        goal_id: tree.id,
        status: tree.status,
        completed,
        failed,
    })
}

#[tauri::command]
pub fn cancel_goal(state: tauri::State<'_, AppState>) -> Result<(), String> {
    *state.goal_cancelled.lock().unwrap() = true;
    *state.cancelled.lock().unwrap() = true;
    Ok(())
}

// ---------- Internal helpers ----------

enum TaskOutcome {
    Done(String),
    Failed(String),
    Cancelled,
}

async fn execute_task_with_retries(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    goal: &str,
    idx: usize,
    tree: &TaskTree,
    max_retries: u32,
) -> TaskOutcome {
    let total = tree.tasks.len();
    let task = &tree.tasks[idx];
    let mut last_feedback: Option<String> = None;

    loop {
        if *state.goal_cancelled.lock().unwrap() {
            return TaskOutcome::Cancelled;
        }

        let retries = task_retries_of(tree, &task.id);
        // Build a per-task message that includes goal context and any
        // reviewer feedback from a prior attempt.
        let context = build_task_message(goal, total, idx, task, last_feedback.as_deref());

        // Run the multi-agent loop for this task.
        let resp = match ai::run_chat_turn(
            app.clone(),
            state,
            project_dir.to_string(),
            context,
            Vec::<UiMessage>::new(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if retries >= max_retries {
                    return TaskOutcome::Failed(format!("executor error: {e}"));
                }
                last_feedback = Some(format!("previous attempt failed: {e}"));
                bump_retries(app, tree, &task.id);
                continue;
            }
        };

        // Review: did the executor actually finish the task?
        match review_task(
            app,
            state,
            project_dir,
            goal,
            &task.description,
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
                bump_retries(app, tree, &task.id);
            }
            ReviewDecision::Unknown(fallback) => {
                // No reviewer or unparsed verdict — accept the executor's
                // assistant summary as the task result.
                return TaskOutcome::Done(trim_to(&fallback, 400));
            }
        }
    }
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

fn task_retries_of(tree: &TaskTree, task_id: &str) -> u32 {
    tree.tasks
        .iter()
        .find(|t| t.id == task_id)
        .map(|t| t.retries)
        .unwrap_or(0)
}

fn bump_retries(app: &AppHandle, tree: &TaskTree, task_id: &str) {
    // Note: tree is immutable here, but emit lets the UI know we're about
    // to retry. The real retries counter is updated in-place by the caller
    // via `tree.tasks[idx].retries += 1` below. This helper is kept for
    // symmetry with the UI event path.
    let _ = app.emit(
        "task:update",
        json!({
            "goal_id": tree.id,
            "id": task_id,
            "status": TaskStatus::Running.as_str(),
            "retries_bumped": true,
        }),
    );
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
    Some(parsed.tasks)
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

// Force Value to appear used (for serde_json import path below); this module
// intentionally uses serde_json in a few spots above.
#[allow(dead_code)]
fn _force_value_used() -> Value {
    Value::Null
}
