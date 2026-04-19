//! Open Claude Code Desktop — Tauri backend.
//!
//! Exposes a small set of commands used by the React frontend:
//! file system access (`list_dir`, `read_file`, `write_file`), a recursive
//! directory watcher (`watch_dir`/`unwatch_dir`), a short-lived shell runner
//! (`run_cmd`), a settings store, and the hybrid AI chat loop (`send_chat`)
//! that routes between an **OpenRouter** planner and an **Ollama** executor
//! through an OpenAI-style tool-calling protocol.
//!
//! This crate does not depend on the repository's top-level `src/` directory;
//! `src/` is a read-only research snapshot and is intentionally out of scope.

use std::sync::Mutex;

use tauri::Manager;
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

mod ai;
mod fs_ops;
mod memory;
mod settings;
mod tools;
mod watcher;

pub(crate) use settings::Settings;

/// Shared, mutable application state, owned by Tauri.
pub struct AppState {
    /// Active settings (loaded from / persisted to the app config dir).
    pub settings: Mutex<Settings>,
    /// Running directory watchers, keyed by project root.
    pub watchers: watcher::Watchers,
    /// Cancellation flag shared by in-flight chat loops.
    pub cancelled: Mutex<bool>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("open_claude_code_desktop=info,info")),
        )
        .try_init();

    info!("Starting Open Claude Code Desktop");

    let initial_settings = Settings::load().unwrap_or_default();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            settings: Mutex::new(initial_settings),
            watchers: watcher::Watchers::default(),
            cancelled: Mutex::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            fs_ops::list_dir,
            fs_ops::read_file,
            fs_ops::write_file,
            watcher::watch_dir,
            watcher::unwatch_dir,
            tools::run_cmd,
            ai::send_chat,
            ai::cancel_chat,
            ai::check_planner,
            ai::check_executor,
            settings::get_settings,
            settings::save_settings,
            memory::load_memory,
            memory::save_memory,
        ])
        .setup(|app| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_title("Open Claude Code");
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
