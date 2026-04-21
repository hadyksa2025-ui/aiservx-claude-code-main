use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::AppState;

/// Which mix of providers should run the three agent roles (planner,
/// executor, reviewer).
///
/// - `Cloud`  — every role runs on OpenRouter. Requires an API key.
/// - `Local`  — every role runs on Ollama. No network required.
/// - `Hybrid` — planner + reviewer on OpenRouter (reasoning-heavy),
///   executor on Ollama (tool-heavy). Each role falls back to the
///   other provider if its primary is unreachable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    Cloud,
    Local,
    Hybrid,
}

impl Default for ProviderMode {
    fn default() -> Self {
        Self::Hybrid
    }
}

fn default_provider_mode() -> ProviderMode {
    // On first run we pick `Hybrid` if an OpenRouter key is available in
    // the environment (matches the legacy "planner on when key set"
    // behaviour), otherwise `Local`. Users can always change the mode
    // from the Settings dialog.
    if std::env::var("OPENROUTER_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        ProviderMode::Hybrid
    } else {
        ProviderMode::Local
    }
}

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
    /// How to split the three agent roles between OpenRouter and Ollama.
    /// See [`ProviderMode`] for the matrix.
    #[serde(default = "default_provider_mode")]
    pub provider_mode: ProviderMode,
    /// Optional per-role model override. When empty, the dispatcher
    /// uses `openrouter_model` / `ollama_model` depending on which
    /// provider the mode routes the role to. A non-empty value is
    /// applied verbatim as the model identifier for that role.
    #[serde(default)]
    pub planner_model: String,
    #[serde(default)]
    pub reviewer_model: String,
    #[serde(default)]
    pub executor_model: String,
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
    /// Per-task wall-clock timeout in seconds. Applies to a single executor
    /// attempt (one retry, not the cumulative sum). 0 disables.
    #[serde(default = "default_task_timeout_secs")]
    pub task_timeout_secs: u64,
    /// Global wall-clock timeout in seconds for a single `start_goal`
    /// invocation. 0 disables.
    #[serde(default = "default_goal_timeout_secs")]
    pub goal_timeout_secs: u64,
    /// Base backoff in milliseconds used between task retries. The actual
    /// delay is `base * 2^retries` capped at 30s.
    #[serde(default = "default_retry_backoff_base_ms")]
    pub retry_backoff_base_ms: u64,
    /// Maximum number of consecutive task failures before the controller
    /// trips the circuit breaker and aborts the goal. 0 disables.
    #[serde(default = "default_circuit_breaker_threshold")]
    pub circuit_breaker_threshold: u32,
    /// Reserved for a future parallel-execution mode. Currently always 1.
    /// Values > 1 are accepted but the controller executes sequentially.
    #[serde(default = "default_max_parallel_tasks")]
    pub max_parallel_tasks: u32,
    /// If true, `write_file` (on a change to an existing file) and
    /// `run_cmd` are routed through the confirm modal even when
    /// `autonomous_mode` is on — the allow-list is bypassed for
    /// irreversible commands too. Chat-driven turns are unaffected.
    /// Default is `false` so existing autonomous runs keep their
    /// current behaviour.
    #[serde(default)]
    pub autonomous_confirm_irreversible: bool,
    /// If true, `run_chat_turn` will drop chat history messages older
    /// than `context_compaction_keep_last` before sending them to the
    /// executor. This is a simple "sliding window" context compaction
    /// — it does not summarise dropped messages, just trims the oldest
    /// ones. Useful for small local models whose context windows are
    /// filled by long sessions; off by default so short sessions see
    /// no behavioural change.
    #[serde(default)]
    pub context_compaction_enabled: bool,
    /// How many of the most recent chat-history messages to preserve
    /// when `context_compaction_enabled` is true. Older messages are
    /// dropped (not summarised). This is measured in UI messages — a
    /// single user-then-assistant exchange counts as two. Must be >= 2;
    /// values below 2 are silently clamped at call time.
    #[serde(default = "default_context_compaction_keep_last")]
    pub context_compaction_keep_last: u32,
    /// Absolute path to the most recently opened project directory.
    /// Set by `set_last_project_dir` (called from `open_project` in
    /// the frontend) and consumed by `get_last_project_dir` on boot so
    /// the app can auto-restore the user's last working project.
    /// `None` on first run and after the user has never opened a
    /// project. Scenario-A §9.2 F-8.
    #[serde(default)]
    pub last_project_dir: Option<String>,
    /// OC-Titan Phase 1.B — enable the TypeScript compiler gate
    /// (`tsc --noEmit`) that validates every codegen envelope in a
    /// scratch dir before files are promoted into the real project.
    /// Default `true` — the gate silently skips envelopes that touch
    /// no `.ts` / `.tsx` files, so HTML-only / JSON-only envelopes
    /// (e.g. L1 prompts in OPENROUTER_VALIDATION_REPORT) pay no cost.
    #[serde(default = "default_true")]
    pub compiler_gate_enabled: bool,
    /// How many corrective retries the compiler gate will attempt when
    /// `tsc` reports errors before it gives up and surfaces the
    /// diagnostics to the UI. V6 §V.2 sets this to 2 by default.
    #[serde(default = "default_max_compile_retries")]
    pub max_compile_retries: u32,
    /// Wall-clock timeout for a single `tsc --noEmit` invocation, in
    /// seconds. 0 disables the timeout. Defaults to 120s, which is
    /// generous for typical SPA scratches and still bounds the
    /// worst-case case where `tsc` loops on a pathological input.
    #[serde(default = "default_tsc_timeout_secs")]
    pub tsc_timeout_secs: u64,
    /// Whether the OC-Titan dependency guard (Phase 1.C) runs before
    /// a generated envelope reaches the compiler gate. The guard
    /// parses imports / requires / dynamic imports out of every
    /// ts/tsx/js/jsx file in the envelope and checks that each
    /// external package specifier is listed under one of
    /// `dependencies`, `devDependencies`, `peerDependencies`, or
    /// `optionalDependencies` in the project's `package.json`. It
    /// defends V6 §I.6 against the common failure mode where the
    /// model hallucinates a package it has never installed (see
    /// `OPENROUTER_VALIDATION_REPORT` §3 L2/L3). Defaults to enabled.
    #[serde(default = "default_true")]
    pub dependency_guard_enabled: bool,
    /// What the dependency guard does when it finds an unresolved
    /// import. `"fail"` (default) aborts the envelope with a
    /// structured error and re-prompts the model once per
    /// `max_compile_retries`, matching the compile-gate contract.
    /// `"warn"` surfaces the diagnostic via `ai:step` but still
    /// forwards the envelope to the compiler gate — useful when
    /// iterating on a half-set-up project where `package.json`
    /// legitimately lags the code.
    #[serde(default = "default_dependency_guard_mode")]
    pub dependency_guard_mode: String,
    /// OC-Titan Phase 2.A — enable the deterministic `run_cmd`
    /// security classifier (V6 §VII.2). When on, every envelope's
    /// `run_cmd` is classified as SAFE / WARNING / DANGEROUS after the
    /// compiler gate succeeds and a `security.classified` `ai:step`
    /// event is emitted so the UI can surface the risk. This setting
    /// does not itself enable execution of `run_cmd` — that is
    /// deferred to Phase 2.B behind the same classifier. Defaults to
    /// enabled because the event is informational only.
    #[serde(default = "default_true")]
    pub security_gate_enabled: bool,
    /// Behaviour of the Phase 2.B execution layer (not yet wired) when
    /// the classifier returns WARNING: `"prompt"` routes the command
    /// through the existing confirm modal, `"allow"` auto-approves
    /// (matches today's allow-list behaviour for low-friction
    /// workflows), `"block"` refuses outright. Dangerous is always
    /// blocked and Safe is always allowed regardless of this setting.
    /// Phase 2.A reads and persists this value but does not yet act
    /// on it; Phase 2.B will consume it.
    #[serde(default = "default_security_gate_warning_mode")]
    pub security_gate_warning_mode: String,
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
fn default_task_timeout_secs() -> u64 {
    // Local CPU-only models doing multi-step code analysis routinely need
    // 3–6 minutes per task; the previous 180s default caused the first-run
    // experience to timeout on real projects. The cancel + circuit-breaker
    // layer still bounds worst-case runtime.
    600
}
fn default_goal_timeout_secs() -> u64 {
    // Raised in lockstep with `task_timeout_secs` so a 20-task goal has
    // realistic headroom. Users who want tighter bounds can lower either
    // knob from Settings.
    7200
}
fn default_retry_backoff_base_ms() -> u64 {
    1000
}
fn default_circuit_breaker_threshold() -> u32 {
    5
}
fn default_max_parallel_tasks() -> u32 {
    1
}
fn default_max_compile_retries() -> u32 {
    // V6 §V.2 Compiler loop — two corrective retries per envelope.
    // Keeps deterministic bounds while giving the model a second pass
    // after its initial fix attempt.
    2
}
fn default_tsc_timeout_secs() -> u64 {
    120
}
fn default_dependency_guard_mode() -> String {
    // V6 §I.6 "Fail loudly if unresolved" — default to hard-fail so
    // phantom imports are caught before the compiler gate burns a
    // retry slot on them. Users can relax this to `"warn"` from
    // Settings when bootstrapping a new project.
    "fail".to_string()
}
fn default_security_gate_warning_mode() -> String {
    // V6 §VII.2 — WARNING commands should require an explicit
    // human-in-the-loop decision by default. Phase 2.B will honour
    // this, but we persist the value now so users' preferences
    // survive across sessions before execution is wired up.
    "prompt".to_string()
}
fn default_context_compaction_keep_last() -> u32 {
    // 20 UI messages ≈ 10 user/assistant pairs. Comfortable for
    // free-form chat; easy to raise/lower from Settings. Must stay at
    // least 2 (see `context_compaction_keep_last` docs).
    20
}
fn default_openrouter_model() -> String {
    std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openrouter/auto".into())
}
fn default_ollama_url() -> String {
    std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into())
}
fn default_ollama_model() -> String {
    // Default matches the recommended first-run pairing in README.md
    // (deepseek-coder:6.7b as the executor, with llama3.2:1b as an optional
    // smaller reviewer). Users can always override in Settings.
    std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "deepseek-coder:6.7b".into())
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
            provider_mode: default_provider_mode(),
            planner_model: String::new(),
            reviewer_model: String::new(),
            executor_model: String::new(),
            reviewer_enabled: true,
            max_iterations: default_max_iterations(),
            cmd_confirm_required: true,
            cmd_allow_list: default_cmd_allow_list(),
            autonomous_mode: false,
            max_retries_per_task: default_max_retries_per_task(),
            max_total_tasks: default_max_total_tasks(),
            task_timeout_secs: default_task_timeout_secs(),
            goal_timeout_secs: default_goal_timeout_secs(),
            retry_backoff_base_ms: default_retry_backoff_base_ms(),
            circuit_breaker_threshold: default_circuit_breaker_threshold(),
            max_parallel_tasks: default_max_parallel_tasks(),
            autonomous_confirm_irreversible: false,
            context_compaction_enabled: false,
            context_compaction_keep_last: default_context_compaction_keep_last(),
            last_project_dir: None,
            compiler_gate_enabled: true,
            max_compile_retries: default_max_compile_retries(),
            tsc_timeout_secs: default_tsc_timeout_secs(),
            dependency_guard_enabled: true,
            dependency_guard_mode: default_dependency_guard_mode(),
            security_gate_enabled: true,
            security_gate_warning_mode: default_security_gate_warning_mode(),
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
    Ok(state.read_settings().clone())
}

#[tauri::command]
pub fn save_settings(
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    settings.save().map_err(|e| e.to_string())?;
    *state.write_settings() = settings;
    Ok(())
}

/// Persist `project_dir` as the last-opened project. Small focused
/// command so the frontend can call it from `open_project` without
/// having to round-trip a full `Settings` payload just to update one
/// field. Scenario-A §9.2 F-8.
#[tauri::command]
pub fn set_last_project_dir(
    state: tauri::State<'_, AppState>,
    project_dir: String,
) -> Result<(), String> {
    // Mirror `save_settings`: build the new value off a clone, persist
    // to disk first, and only then swap it into the shared state. If
    // `save()` fails we return `Err` without leaving in-memory settings
    // out of sync with what's on disk (PR #11 Devin Review).
    let mut updated = state.read_settings().clone();
    updated.last_project_dir = Some(project_dir);
    updated.save().map_err(|e| e.to_string())?;
    *state.write_settings() = updated;
    Ok(())
}

/// Return the last-opened project dir if one was recorded. The
/// frontend calls this on boot and, if a dir is returned and still
/// exists on disk, auto-opens it so the user's last project is
/// restored without having to click "Open project…" every launch.
#[tauri::command]
pub fn get_last_project_dir(
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    Ok(state.read_settings().last_project_dir.clone())
}
