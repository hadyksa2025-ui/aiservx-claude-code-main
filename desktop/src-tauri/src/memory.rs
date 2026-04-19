//! Persistent project memory at `<project>/PROJECT_MEMORY.json`.
//!
//! This is intentionally just a JSON blob; callers hand us the full document
//! and we write it atomically (via temp-file + rename). The AI layer updates
//! `session.turns`, `file_index`, and `tool_usage` after every chat turn.

use std::path::PathBuf;

use serde_json::Value;

const FILE: &str = "PROJECT_MEMORY.json";

fn memory_path(project_dir: &str) -> PathBuf {
    PathBuf::from(project_dir).join(FILE)
}

#[tauri::command]
pub fn load_memory(project_dir: String) -> Result<Value, String> {
    let path = memory_path(&project_dir);
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_memory(project_dir: String, memory: Value) -> Result<(), String> {
    save_memory_sync(&project_dir, &memory)
}

pub(crate) fn save_memory_sync(project_dir: &str, memory: &Value) -> Result<(), String> {
    let path = memory_path(project_dir);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(memory).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}
