//! Task engine for the autonomous controller.
//!
//! A `TaskTree` is a flat, ordered list of `Task`s produced from a single
//! user goal. The controller (`controller.rs`) executes each task through
//! the existing multi-agent loop, collects the result, and updates the tree
//! back into `PROJECT_MEMORY.json` under the `active_task_tree` key.
//!
//! Events emitted to the UI:
//!
//! - `task:goal_started`   { goal, task_count }
//! - `task:added`          { task }
//! - `task:update`         { id, status, retries, result, updated_at }
//! - `task:goal_done`      { goal, status, completed, failed }
//! - `task:failure_logged` { task_id, error }

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

const MAX_TASK_HISTORY: usize = 200;
const MAX_FAILURES_LOG: usize = 200;

/// Status of a single task. String-backed so the JSON contract stays stable
/// even if new variants are added.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub result: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTree {
    pub id: String,
    pub goal: String,
    pub tasks: Vec<Task>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub status: String, // running | done | failed | cancelled
}

impl TaskTree {
    pub fn new(goal: String) -> Self {
        let now = unix_ts();
        Self {
            id: format!("goal_{}", uuid::Uuid::new_v4().simple()),
            goal,
            tasks: Vec::new(),
            created_at: now,
            updated_at: now,
            status: "running".into(),
        }
    }
}

pub fn unix_ts() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn new_task(description: String, deps: Vec<String>) -> Task {
    let now = unix_ts();
    Task {
        id: format!("task_{}", uuid::Uuid::new_v4().simple()),
        description,
        status: TaskStatus::Pending.as_str().into(),
        retries: 0,
        deps,
        result: None,
        created_at: now,
        updated_at: now,
    }
}

// ---------- Event emission ----------

pub fn emit_goal_started(app: &AppHandle, tree: &TaskTree) {
    let _ = app.emit(
        "task:goal_started",
        json!({
            "id": tree.id,
            "goal": tree.goal,
            "task_count": tree.tasks.len(),
            "created_at": tree.created_at,
        }),
    );
}

pub fn emit_task_added(app: &AppHandle, goal_id: &str, task: &Task) {
    let _ = app.emit(
        "task:added",
        json!({
            "goal_id": goal_id,
            "task": task,
        }),
    );
}

pub fn emit_task_update(app: &AppHandle, goal_id: &str, task: &Task) {
    let _ = app.emit(
        "task:update",
        json!({
            "goal_id": goal_id,
            "id": task.id,
            "status": task.status,
            "retries": task.retries,
            "result": task.result,
            "updated_at": task.updated_at,
        }),
    );
}

pub fn emit_goal_done(app: &AppHandle, tree: &TaskTree) {
    let completed = tree
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done.as_str())
        .count();
    let failed = tree
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed.as_str())
        .count();
    let _ = app.emit(
        "task:goal_done",
        json!({
            "id": tree.id,
            "goal": tree.goal,
            "status": tree.status,
            "completed": completed,
            "failed": failed,
        }),
    );
}

// ---------- Persistence into PROJECT_MEMORY.json ----------

/// Save the whole tree under `active_task_tree` and snapshot into
/// `task_history[]`. Never fails catastrophically; on a parse error the
/// affected subtree is reset.
pub fn persist_active_tree(project_dir: &str, tree: &TaskTree) -> Result<(), String> {
    let mut mem = read_memory(project_dir);
    let obj = mem.as_object_mut().unwrap();

    obj.insert(
        "updated_at".into(),
        Value::String(format!("epoch:{}", unix_ts())),
    );
    obj.insert(
        "active_task_tree".into(),
        serde_json::to_value(tree).unwrap_or(Value::Null),
    );

    crate::memory::save_memory_sync(project_dir, &mem)
}

/// Move the current active tree into `task_history[]` and clear the
/// `active_task_tree` slot.
pub fn archive_active_tree(project_dir: &str, tree: &TaskTree) -> Result<(), String> {
    let mut mem = read_memory(project_dir);
    let obj = mem.as_object_mut().unwrap();

    let history = obj
        .entry("task_history".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !history.is_array() {
        *history = Value::Array(Vec::new());
    }
    if let Some(arr) = history.as_array_mut() {
        arr.push(serde_json::to_value(tree).unwrap_or(Value::Null));
        let overflow = arr.len().saturating_sub(MAX_TASK_HISTORY);
        if overflow > 0 {
            arr.drain(..overflow);
        }
    }
    obj.insert("active_task_tree".into(), Value::Null);
    obj.insert(
        "updated_at".into(),
        Value::String(format!("epoch:{}", unix_ts())),
    );

    crate::memory::save_memory_sync(project_dir, &mem)
}

pub fn log_failure(project_dir: &str, task_id: &str, error: &str) -> Result<(), String> {
    let mut mem = read_memory(project_dir);
    let obj = mem.as_object_mut().unwrap();

    let fails = obj
        .entry("failures_log".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !fails.is_array() {
        *fails = Value::Array(Vec::new());
    }
    if let Some(arr) = fails.as_array_mut() {
        arr.push(json!({
            "at": unix_ts(),
            "task_id": task_id,
            "error": error,
        }));
        let overflow = arr.len().saturating_sub(MAX_FAILURES_LOG);
        if overflow > 0 {
            arr.drain(..overflow);
        }
    }

    crate::memory::save_memory_sync(project_dir, &mem)
}

fn read_memory(project_dir: &str) -> Value {
    let path = std::path::PathBuf::from(project_dir).join("PROJECT_MEMORY.json");
    let mut v: Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !v.is_object() {
        v = json!({});
    }
    v
}

// ---------- Tauri command ----------

#[tauri::command]
pub fn load_task_tree(project_dir: String) -> Result<Value, String> {
    let mem = read_memory(&project_dir);
    Ok(mem
        .get("active_task_tree")
        .cloned()
        .unwrap_or(Value::Null))
}
