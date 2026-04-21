//! Open Claude Code Desktop — Tauri backend.
//!
//! Exposes a small set of commands used by the React frontend:
//! file system access (`list_dir`, `read_file`, `write_file`), a recursive
//! directory watcher (`watch_dir`/`unwatch_dir`), a short-lived shell runner
//! (`run_cmd`), a settings store, a confirmation bridge for gated shell
//! commands (`confirm_cmd`), and the hybrid AI chat loop (`send_chat`) that
//! routes between an **OpenRouter** planner, an **Ollama** executor, and an
//! optional **Reviewer** pass through an OpenAI-style tool-calling protocol.
//!
//! This crate does not depend on the repository's top-level `src/` directory;
//! `src/` is a read-only research snapshot and is intentionally out of scope.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tauri::Manager;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

mod ai;
mod cancel;
mod codegen_envelope;
mod compiler_gate;
mod controller;
mod dependency_guard;
mod fs_ops;
mod memory;
mod project_scan;
mod settings;
mod tasks;
mod tools;
mod trace;
mod watcher;

pub(crate) use settings::Settings;

/// Shared, mutable application state, owned by Tauri.
pub struct AppState {
    /// Active settings (loaded from / persisted to the app config dir).
    /// `RwLock` because reads are frequent (every chat turn, health check,
    /// tool call) but writes are rare (only when the user saves settings).
    pub settings: RwLock<Settings>,
    /// Running directory watchers, keyed by project root.
    pub watchers: watcher::Watchers,
    /// Cancellation token shared by in-flight chat loops. Cooperative
    /// cancellation is implemented via `CancelToken`: code can both
    /// synchronously check `is_cancelled()` and asynchronously race a
    /// `select!` against `cancelled()` to abort in-flight tool execution
    /// and SSE streams, not just wait until the next iteration boundary.
    pub cancelled: cancel::CancelToken,
    /// Cancellation token for the top-level autonomous goal loop. Separate
    /// from `cancelled` so that per-turn cancellation does not stop the
    /// whole goal, and vice-versa. The controller typically creates a
    /// per-task child token that is linked from this one, so a goal cancel
    /// aborts whatever task is currently executing.
    pub goal_cancelled: cancel::CancelToken,
    /// `true` when a `start_goal` is currently in flight. Used by the
    /// controller as an idempotency guard against concurrent goal starts.
    pub goal_running: Mutex<bool>,
    /// In-flight `run_cmd` confirmation requests: request_id -> oneshot sender.
    /// The AI tool loop awaits the receiver; the UI resolves it via
    /// `confirm_cmd`.
    ///
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`) because the map is
    /// touched from `async` tool-call paths — a std Mutex held across
    /// `.await` is a deadlock hazard and triggers a `clippy::await_holding_lock`
    /// warning. The lock is only held for the microsecond-scale
    /// `insert`/`remove` so the async lock's cost is negligible.
    pub pending_confirms: AsyncMutex<HashMap<String, oneshot::Sender<bool>>>,

    /// Active terminal processes keyed by `terminal_id`. Used by the
    /// in-app Terminal tabs to support concurrent runs and per-tab stop.
    pub terminal_pids: AsyncMutex<HashMap<String, u32>>,
}

impl AppState {
    /// Acquire a read guard on `settings`, recovering transparently if
    /// the lock is poisoned (another thread panicked while holding a
    /// write guard).
    ///
    /// A poisoned lock is almost always benign here: `Settings` is a
    /// plain data struct and a panic during read/clone leaves it in a
    /// perfectly usable state. Without this recovery, every subsequent
    /// `send_chat` / `start_goal` / `run_cmd` would panic on
    /// `unwrap()`, turning one transient failure into a hard crash of
    /// the whole AI backend. We log once per recovery so ops can still
    /// see it in the trace.
    pub(crate) fn read_settings(&self) -> RwLockReadGuard<'_, Settings> {
        self.settings.read().unwrap_or_else(|poisoned| {
            warn!("settings RwLock poisoned on read; recovering inner guard");
            poisoned.into_inner()
        })
    }

    /// Acquire a write guard on `settings`, recovering transparently if
    /// the lock is poisoned. See [`AppState::read_settings`] for
    /// rationale.
    pub(crate) fn write_settings(&self) -> RwLockWriteGuard<'_, Settings> {
        self.settings.write().unwrap_or_else(|poisoned| {
            warn!("settings RwLock poisoned on write; recovering inner guard");
            poisoned.into_inner()
        })
    }

    /// Acquire the `goal_running` guard, recovering from poison. The
    /// `bool` inside is a simple idempotency flag: a poisoned lock
    /// cannot corrupt it in any meaningful way, so we just take the
    /// inner value and continue.
    pub(crate) fn lock_goal_running(&self) -> MutexGuard<'_, bool> {
        self.goal_running.lock().unwrap_or_else(|poisoned| {
            warn!("goal_running Mutex poisoned; recovering inner guard");
            poisoned.into_inner()
        })
    }
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
            settings: RwLock::new(initial_settings),
            watchers: watcher::Watchers::default(),
            cancelled: cancel::CancelToken::new(),
            goal_cancelled: cancel::CancelToken::new(),
            goal_running: Mutex::new(false),
            pending_confirms: AsyncMutex::new(HashMap::new()),
            terminal_pids: AsyncMutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            fs_ops::list_dir,
            fs_ops::read_file,
            fs_ops::write_file,
            watcher::watch_dir,
            watcher::unwatch_dir,
            tools::run_cmd,
            tools::run_cmd_stream,
            tools::terminal_kill,
            tools::confirm_cmd,
            ai::send_chat,
            ai::cancel_chat,
            ai::check_planner,
            ai::check_executor,
            ai::probe_ollama,
            ai::probe_openrouter,
            settings::get_settings,
            settings::save_settings,
            settings::set_last_project_dir,
            settings::get_last_project_dir,
            memory::load_memory,
            memory::save_memory,
            controller::start_goal,
            controller::cancel_goal,
            controller::run_codegen_envelope,
            project_scan::scan_project_cmd,
            tasks::load_task_tree,
            tasks::load_failures_log,
            tasks::clear_failures_log,
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
