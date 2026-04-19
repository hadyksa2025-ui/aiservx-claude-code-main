use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub openrouter_api_key: String,
    #[serde(default = "default_openrouter_model")]
    pub openrouter_model: String,
    #[serde(default = "default_ollama_url")]
    pub ollama_base_url: String,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
    /// If true, run a Reviewer pass after the executor tool loop and allow one
    /// corrective retry when the reviewer asks for a fix.
    #[serde(default = "default_true")]
    pub reviewer_enabled: bool,
    /// Hard cap on the number of executor iterations per user turn.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// If true, any `run_cmd` that is not prefix-matched by `cmd_allow_list`
    /// and not matched by the built-in deny-list is routed to the UI for an
    /// explicit Approve/Deny decision.
    #[serde(default = "default_true")]
    pub cmd_confirm_required: bool,
    /// Command prefixes that are auto-approved (no confirm dialog).
    #[serde(default = "default_cmd_allow_list")]
    pub cmd_allow_list: Vec<String>,
    /// If true, `start_goal` runs the plan → execute → review → retry loop
    /// without waiting for user intervention between tasks.
    #[serde(default)]
    pub autonomous_mode: bool,
    /// Maximum retries the controller will attempt on a single failed task
    /// before marking it as failed and moving on.
    #[serde(default = "default_max_retries_per_task")]
    pub max_retries_per_task: u32,
    /// Hard safety ceiling for the total number of tasks in a single goal's
    /// task tree. The planner is asked for at most this many tasks.
    #[serde(default = "default_max_total_tasks")]
    pub max_total_tasks: u32,
}

fn default_true() -> bool {
    true
}
fn default_max_iterations() -> u32 {
    8
}
fn default_max_retries_per_task() -> u32 {
    3
}
fn default_max_total_tasks() -> u32 {
    20
}
fn default_openrouter_model() -> String {
    std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openrouter/auto".into())
}
fn default_ollama_url() -> String {
    std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into())
}
fn default_ollama_model() -> String {
    std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1:8b".into())
}
fn default_cmd_allow_list() -> Vec<String> {
    // Conservative default — read-only / build-ish commands that rarely cause
    // trouble. Users can tighten or widen this from the Settings dialog.
    [
        "ls", "cat", "pwd", "echo", "true", "false", "head", "tail", "wc",
        "grep", "rg", "fd", "find", "git status", "git log", "git diff",
        "git branch", "git remote", "node --version", "npm --version",
        "bun --version", "cargo --version", "python --version", "python3 --version",
        "pip --version", "pip3 --version",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            openrouter_api_key: std::env::var("OPENROUTER_API_KEY").unwrap_or_default(),
            openrouter_model: default_openrouter_model(),
            ollama_base_url: default_ollama_url(),
            ollama_model: default_ollama_model(),
            reviewer_enabled: true,
            max_iterations: default_max_iterations(),
            cmd_confirm_required: true,
            cmd_allow_list: default_cmd_allow_list(),
            autonomous_mode: false,
            max_retries_per_task: default_max_retries_per_task(),
            max_total_tasks: default_max_total_tasks(),
        }
    }
}

fn config_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("open-claude-code").join("settings.json")
}

impl Settings {
    pub fn load() -> Option<Self> {
        let path = config_path();
        let text = fs::read_to_string(&path).ok()?;
        serde_json::from_str::<Settings>(&text).ok()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        fs::write(path, text)
    }
}

#[tauri::command]
pub fn get_settings(state: tauri::State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.settings.lock().unwrap().clone())
}

#[tauri::command]
pub fn save_settings(
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    settings.save().map_err(|e| e.to_string())?;
    *state.settings.lock().unwrap() = settings;
    Ok(())
}
