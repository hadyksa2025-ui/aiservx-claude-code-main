//! Recursive, debounced directory watcher. Emits `fs:changed` events to the
//! frontend whenever files under the watched root are created/modified/removed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebouncedEvent};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::AppState;

/// Dynamic handle kept alive per-watcher — dropping it stops the watcher.
type WatcherHandle =
    notify_debouncer_full::Debouncer<notify::RecommendedWatcher, notify_debouncer_full::FileIdMap>;

#[derive(Default)]
pub struct Watchers {
    inner: Mutex<HashMap<String, WatcherHandle>>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FsChange {
    pub path: String,
    pub kind: String,
}

#[tauri::command]
pub fn watch_dir(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    project_dir: String,
) -> Result<(), String> {
    let root: PathBuf = PathBuf::from(&project_dir)
        .canonicalize()
        .map_err(|e| format!("invalid project root {project_dir}: {e}"))?;

    let mut map = state
        .watchers
        .inner
        .lock()
        .unwrap_or_else(|poisoned| {
            tracing::warn!("watchers Mutex poisoned on watch_dir; recovering inner guard");
            poisoned.into_inner()
        });
    if map.contains_key(&project_dir) {
        return Ok(());
    }

    let app_handle = app.clone();
    let root_for_task = root.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(150),
        None,
        move |res: Result<Vec<DebouncedEvent>, Vec<notify::Error>>| match res {
            Ok(events) => {
                for ev in events {
                    for path in ev.paths.iter() {
                        let rel = path
                            .strip_prefix(&root_for_task)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        // Filter out noisy paths.
                        if rel.contains("/.git/") || rel.starts_with(".git/") {
                            continue;
                        }
                        if rel.contains("/node_modules/")
                            || rel.contains("/target/")
                            || rel.contains("/dist/")
                        {
                            continue;
                        }
                        let kind = classify_event(&ev.event.kind);
                        let payload = FsChange {
                            path: rel,
                            kind: kind.to_string(),
                        };
                        let _ = app_handle.emit("fs:changed", payload);
                    }
                }
            }
            Err(errs) => {
                for e in errs {
                    tracing::warn!("watcher error: {e}");
                }
            }
        },
    )
    .map_err(|e| e.to_string())?;

    debouncer
        .watcher()
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    map.insert(project_dir, debouncer);
    Ok(())
}

#[tauri::command]
pub fn unwatch_dir(
    state: tauri::State<'_, AppState>,
    project_dir: String,
) -> Result<(), String> {
    let mut map = state
        .watchers
        .inner
        .lock()
        .unwrap_or_else(|poisoned| {
            tracing::warn!("watchers Mutex poisoned on unwatch_dir; recovering inner guard");
            poisoned.into_inner()
        });
    map.remove(&project_dir);
    Ok(())
}

fn classify_event(kind: &notify::EventKind) -> &'static str {
    use notify::EventKind::*;
    match kind {
        Create(_) => "created",
        Modify(_) => "modified",
        Remove(_) => "removed",
        Access(_) => "other",
        Any | Other => "other",
    }
}
