//! Tool runtime. Executes tool calls emitted by the AI layer inside the
//! opened project root. Each tool returns an `(output, optional_diff)` pair.
//!
//! Safety model for `run_cmd`:
//!   1. A built-in **deny-list** of patterns (`rm -rf /`, `sudo`, `curl | sh`,
//!      `dd of=/dev/`, …) is rejected outright and never reaches the shell.
//!   2. Commands that prefix-match the user's `cmd_allow_list` run silently.
//!   3. Anything else emits an `ai:confirm_request` event and blocks until
//!      the UI resolves it via the `confirm_cmd` Tauri command.
//!
//! This gating only applies to calls issued through the AI tool loop
//! (`tools::execute_run_cmd_gated`). The direct `run_cmd` Tauri command
//! remains unrestricted so the UI's own Terminal/Explorer surfaces can run
//! arbitrary commands with user intent.
//!
//! **Terminal Authority (PROJECT_MEMORY.md §12).** Every side-effecting
//! AI tool call must be visible in the Agent terminal tab
//! (`terminal_id = "agent-main"`). `execute_run_cmd_gated`,
//! `execute_write_file_gated`, and the read-only surfaces in
//! `execute_safe` all emit `terminal:output` events keyed to
//! [`AGENT_TERMINAL_ID`] so the user sees exactly what the agent is
//! doing, matching Devin / Windsurf visible-execution behavior. Direct
//! user-driven terminals still use their own per-tab ids.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

use crate::cancel::CancelToken;
use crate::{fs_ops, AppState};

/// Stable terminal id used for every side-effecting AI tool invocation.
/// The frontend pins a tab with this id so the agent stream is always
/// visible regardless of how many user-driven terminals exist.
pub const AGENT_TERMINAL_ID: &str = "agent-main";

/// Emit a single line of agent output to the pinned Agent terminal tab.
/// `stream` is `"stdout"` (default content) or `"stderr"` (errors /
/// refusals). The caller is responsible for adding a trailing newline
/// when the line should end; raw streamed bytes pass through verbatim.
pub(crate) fn emit_agent_line(app: &AppHandle, stream: &str, data: impl Into<String>) {
    let _ = app.emit(
        "terminal:output",
        json!({
            "terminal_id": AGENT_TERMINAL_ID,
            "stream": stream,
            "data": data.into(),
        }),
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCmdResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Return the OpenAI/Ollama-compatible tool schema used by both providers.
pub fn tool_schema() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file relative to the opened project root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the project root." }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write (overwrite) a text file relative to the opened project root. Returns a unified-style diff.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List immediate children of a directory relative to the project root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_cmd",
                "description": "Run a short shell command inside the project root and return stdout+stderr+exit_code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" },
                        "timeout_ms": { "type": "integer" }
                    },
                    "required": ["cmd"]
                }
            }
        }
    ])
}

/// A tracked side-effect the AI tool loop performed. Returned alongside
/// `(output, diff)` so the memory layer can index files that were touched.
#[derive(Debug, Clone, Default)]
pub struct ToolEffect {
    pub touched_files: Vec<String>,
}

/// Dispatch a single tool call (read_file, write_file, list_dir) that is
/// safe to run without user confirmation. `run_cmd` must go through
/// [`execute_run_cmd_gated`] instead.
///
/// When `autonomous_confirm` is `true`, `write_file` on an existing file
/// with actually-changed content is routed through the confirm modal
/// via the shared [`await_user_confirmation`] helper. `read_file` and
/// `list_dir` are never gated (they're read-only).
pub async fn execute_safe(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    name: &str,
    args: &Value,
    cancel: &CancelToken,
    autonomous_confirm: bool,
) -> Result<(String, Option<String>, ToolEffect), String> {
    match name {
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            emit_agent_line(app, "stdout", format!("$ read_file {path}\n"));
            let content = fs_ops::read_file(project_dir.to_string(), path.to_string())?;
            let truncated = if content.len() > 100_000 {
                format!(
                    "{}\n… (truncated, {} bytes)",
                    &content[..100_000],
                    content.len()
                )
            } else {
                content
            };
            emit_agent_line(
                app,
                "stdout",
                format!("  → {} bytes\n", truncated.len()),
            );
            Ok((
                truncated,
                None,
                ToolEffect {
                    touched_files: vec![path.to_string()],
                },
            ))
        }
        "write_file" => {
            execute_write_file_gated(app, state, project_dir, args, cancel, autonomous_confirm)
                .await
        }
        "list_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            emit_agent_line(app, "stdout", format!("$ list_dir {path}\n"));
            let entries = fs_ops::list_dir(project_dir.to_string(), path.to_string())?;
            let summary = entries
                .iter()
                .map(|e| format!("{}{}", if e.is_dir { "📁 " } else { "📄 " }, e.name))
                .collect::<Vec<_>>()
                .join("\n");
            emit_agent_line(
                app,
                "stdout",
                format!("  → {} entries\n", entries.len()),
            );
            Ok((summary, None, ToolEffect::default()))
        }
        other => Err(format!("unknown safe tool: {other}")),
    }
}

/// Outcome of [`await_user_confirmation`]. Kept as a typed enum so
/// callers can distinguish "user said no" from "request never arrived"
/// (cancelled / timed out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmOutcome {
    /// User approved the operation.
    Approved,
    /// User denied (clicked Deny or closed without approving).
    Denied,
    /// Request timed out waiting for a human decision.
    TimedOut,
    /// Cancel token fired while the request was pending.
    Cancelled,
}

/// Emit an `ai:confirm_request` event and await the UI's Approve/Deny
/// response. Races against `cancel` and a 10-minute hard timeout so
/// the autonomous loop can never hang indefinitely.
pub(crate) async fn await_user_confirmation(
    app: &AppHandle,
    state: &AppState,
    cancel: &CancelToken,
    id: String,
    payload: Value,
) -> ConfirmOutcome {
    let (tx, rx) = oneshot::channel::<bool>();
    {
        let mut map = state.pending_confirms.lock().await;
        map.insert(id.clone(), tx);
    }
    let _ = app.emit("ai:confirm_request", payload);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let mut map = state.pending_confirms.lock().await;
            map.remove(&id);
            ConfirmOutcome::Cancelled
        }
        r = tokio::time::timeout(Duration::from_secs(600), rx) => match r {
            Ok(Ok(true)) => ConfirmOutcome::Approved,
            Ok(Ok(false)) => ConfirmOutcome::Denied,
            Ok(Err(_)) => ConfirmOutcome::Denied, // sender dropped
            Err(_) => {
                let mut map = state.pending_confirms.lock().await;
                map.remove(&id);
                ConfirmOutcome::TimedOut
            }
        },
    }
}

/// Gated `write_file`. When `autonomous_confirm` is true and the write
/// would actually change an existing file's content, the UI is asked
/// first via the same `ai:confirm_request` modal used by `run_cmd`.
/// Reads, list_dir, and no-op / create-new-file writes remain free.
pub(crate) async fn execute_write_file_gated(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    args: &Value,
    cancel: &CancelToken,
    autonomous_confirm: bool,
) -> Result<(String, Option<String>, ToolEffect), String> {
    if cancel.is_cancelled() {
        return Err(cancel.err_string());
    }
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");

    if autonomous_confirm {
        // Only prompt when the write is actually destructive: the file
        // must already exist AND the content must differ. Creating a
        // new file is still free, matching how the allow-list treats
        // read-only commands: we prompt on irreversible change, not on
        // every tool call.
        if let Some(should_prompt) = write_would_change_existing_file(project_dir, path, content) {
            if should_prompt {
                let id = format!("confirm_{}", uuid::Uuid::new_v4().simple());
                let payload = json!({
                    "id": id,
                    "kind": "write_file",
                    "path": path,
                    "bytes": content.len(),
                    "cmd": format!("write_file {path} ({} bytes)", content.len()),
                    "project_dir": project_dir,
                    "timeout_ms": 600_000,
                });
                match await_user_confirmation(app, state, cancel, id, payload).await {
                    ConfirmOutcome::Approved => { /* fall through */ }
                    ConfirmOutcome::Denied => {
                        return Ok((
                            format!("refused: user denied write_file `{path}`."),
                            None,
                            ToolEffect::default(),
                        ));
                    }
                    ConfirmOutcome::TimedOut => {
                        return Ok((
                            format!(
                                "refused: confirmation timed out (10 minutes) for write_file `{path}`."
                            ),
                            None,
                            ToolEffect::default(),
                        ));
                    }
                    ConfirmOutcome::Cancelled => {
                        return Err(cancel.err_string());
                    }
                }
            }
        }
    }

    emit_agent_line(
        app,
        "stdout",
        format!("$ write_file {path} ({} bytes)\n", content.len()),
    );
    let diff = fs_ops::write_file(
        project_dir.to_string(),
        path.to_string(),
        content.to_string(),
    )?;
    emit_agent_line(app, "stdout", format!("  ✓ wrote {path}\n"));
    Ok((
        format!("wrote {}", path),
        Some(diff),
        ToolEffect {
            touched_files: vec![path.to_string()],
        },
    ))
}

/// Returns `Some(true)` when writing `content` to `project_dir/sub_path`
/// would change an existing file, `Some(false)` when the target doesn't
/// exist (create), the contents are identical (no-op), or the path
/// escapes the sandbox (the eventual `fs_ops::write_file` call will
/// reject it, so there's nothing for the user to confirm).
///
/// Critical: we resolve the target path through [`fs_ops::resolve`],
/// the **same** resolver `fs_ops::write_file` uses, so the file we
/// stat here is the exact file the subsequent write will touch.
/// Using `Path::join` directly was a bypass: a `sub_path` with a
/// leading `/` would make `join` replace the base, the existence
/// check would read a nonexistent root-rooted path and return
/// `Some(false)` (no prompt), while `write_file` would strip the `/`
/// and silently overwrite a real file inside the project. That is
/// exactly the destructive write `autonomous_confirm_irreversible`
/// exists to catch.
pub(crate) fn write_would_change_existing_file(
    project_dir: &str,
    sub_path: &str,
    content: &str,
) -> Option<bool> {
    let target = match fs_ops::resolve(project_dir, sub_path) {
        Ok(p) => p,
        // Sandbox escape or unresolvable root: the real write will
        // error out, so there's nothing to prompt about.
        Err(_) => return Some(false),
    };
    if !target.exists() {
        return Some(false);
    }
    match std::fs::read_to_string(&target) {
        Ok(prev) => Some(prev != content),
        // Unreadable file (binary / permissions) — treat as an
        // irreversible overwrite and prompt, so the user can decide.
        Err(_) => Some(true),
    }
}

/// Gated `run_cmd`. Applies the deny-list and allow-list, emits an
/// `ai:confirm_request` event for everything else, and awaits the UI's
/// decision (up to 10 minutes) before executing.
///
/// `cancel` is checked before the deny-list, races the confirm modal
/// await, and is threaded into the child-process wait so cancel aborts
/// this call no matter where it is parked.
///
/// When `autonomous_confirm` is true the allow-list is **bypassed**:
/// every `run_cmd` is routed through the confirm modal even if it
/// would normally auto-approve. This is the opt-in "really auto" escape
/// hatch the UI exposes as "Confirm irreversible ops in autonomous
/// mode".
pub async fn execute_run_cmd_gated(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    args: &Value,
    cancel: &CancelToken,
    autonomous_confirm: bool,
) -> Result<(String, Option<String>, ToolEffect), String> {
    if cancel.is_cancelled() {
        return Err(cancel.err_string());
    }
    let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("").trim();
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(30_000);

    if cmd.is_empty() {
        return Err("run_cmd: empty command".into());
    }
    if let Some(reason) = deny_reason(cmd) {
        return Ok((
            format!("refused: {reason}\n(command blocked by built-in deny-list)"),
            None,
            ToolEffect::default(),
        ));
    }

    let (allow_list, confirm_required) = {
        let s = state.read_settings();
        (s.cmd_allow_list.clone(), s.cmd_confirm_required)
    };

    let should_prompt = should_prompt_run_cmd(
        cmd,
        &allow_list,
        confirm_required,
        autonomous_confirm,
    );

    if should_prompt {
        let id = format!("confirm_{}", uuid::Uuid::new_v4().simple());
        let payload = json!({
            "id": id,
            "kind": "run_cmd",
            "cmd": cmd,
            "project_dir": project_dir,
            "timeout_ms": timeout_ms,
        });
        match await_user_confirmation(app, state, cancel, id, payload).await {
            ConfirmOutcome::Approved => { /* fall through to execution */ }
            ConfirmOutcome::Denied => {
                return Ok((
                    format!("refused: user denied `{cmd}`."),
                    None,
                    ToolEffect::default(),
                ));
            }
            ConfirmOutcome::TimedOut => {
                return Ok((
                    "refused: confirmation timed out (10 minutes).".into(),
                    None,
                    ToolEffect::default(),
                ));
            }
            ConfirmOutcome::Cancelled => {
                return Err(cancel.err_string());
            }
        }
    }

    emit_agent_line(app, "stdout", format!("$ {cmd}\n"));
    let result = run_cmd_impl(project_dir, cmd, timeout_ms, Some(cancel), Some(app)).await;
    match &result {
        Ok(r) => emit_agent_line(app, "stdout", format!("[exit {}]\n", r.exit_code)),
        Err(e) => emit_agent_line(app, "stderr", format!("[error: {e}]\n")),
    }
    let result = result?;
    let mut out = String::new();
    out.push_str(&format!("exit {}\n", result.exit_code));
    if !result.stdout.is_empty() {
        out.push_str("--- stdout ---\n");
        out.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !result.stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&result.stderr);
    }
    Ok((out, None, ToolEffect::default()))
}

/// Patterns that are rejected outright. Anything that could irreversibly
/// harm the machine, exfiltrate credentials, or pipe an untrusted payload
/// into `sh`.
fn deny_reason(cmd: &str) -> Option<String> {
    let lower = cmd.to_ascii_lowercase();
    let patterns: &[(&str, &str)] = &[
        ("rm -rf /", "rm -rf targeting filesystem root"),
        ("rm -rf /*", "rm -rf with root glob"),
        ("rm -rf ~", "rm -rf targeting home directory"),
        ("mkfs", "filesystem format"),
        ("dd of=/dev/", "raw disk write"),
        (":(){:|:&};:", "fork bomb"),
        ("fork()", "fork bomb variant"),
        ("sudo ", "sudo requires human-in-the-loop"),
        ("doas ", "doas requires human-in-the-loop"),
        ("chown -r /", "chown targeting root"),
        ("chmod -r 777 /", "chmod 777 on root"),
        (">/dev/sda", "write to raw disk device"),
        (" > /etc/", "overwriting a file under /etc"),
    ];
    for (needle, reason) in patterns {
        if lower.contains(needle) {
            return Some((*reason).to_string());
        }
    }
    // Shell pipelines piping remote content into a shell.
    if (lower.contains("curl ") || lower.contains("wget "))
        && (lower.contains("| sh") || lower.contains("| bash") || lower.contains("|sh"))
    {
        return Some("piping remote content into a shell".into());
    }
    None
}

/// Pure gate decision for `run_cmd`: should the UI be asked to confirm
/// this invocation before we spawn a shell? Extracted so the three
/// interacting inputs (allow-list prefix match, `cmd_confirm_required`
/// setting, `autonomous_confirm_irreversible` setting) can be unit-tested
/// without standing up a `tauri::Builder`.
///
/// Invariants:
///  - When `autonomous_confirm` is true the allow-list is bypassed; every
///    `run_cmd` prompts. That is the whole point of the toggle.
///  - Otherwise an allow-list prefix match auto-approves the command.
///  - If nothing matched the allow-list we fall back to the existing
///    `cmd_confirm_required` behaviour.
pub(crate) fn should_prompt_run_cmd(
    cmd: &str,
    allow_list: &[String],
    confirm_required: bool,
    autonomous_confirm: bool,
) -> bool {
    let auto_ok = allow_list
        .iter()
        .any(|p| !p.is_empty() && cmd_matches_prefix(cmd, p));
    (!auto_ok && confirm_required) || autonomous_confirm
}

/// A command matches an allow-list entry if it equals it exactly or if it
/// starts with the entry followed by a space or end-of-string.
fn cmd_matches_prefix(cmd: &str, prefix: &str) -> bool {
    if cmd == prefix {
        return true;
    }
    if let Some(rest) = cmd.strip_prefix(prefix) {
        return rest.starts_with(' ') || rest.is_empty();
    }
    false
}

/// Tauri-exposed direct shell runner. Unrestricted — the UI invokes this
/// only in response to an explicit user action (Explorer/Terminal), not as
/// part of the AI loop.
#[tauri::command]
pub async fn run_cmd(
    project_dir: String,
    cmd: String,
    timeout_ms: Option<u64>,
) -> Result<RunCmdResult, String> {
    // Direct user-initiated invocation: no cooperative cancel is wired
    // here, only the timeout.
    run_cmd_impl(&project_dir, &cmd, timeout_ms.unwrap_or(30_000), None, None).await
}

/// Streaming shell runner. Emits `terminal:output` events with stdout/stderr
/// in real-time, then returns the final result.
#[tauri::command]
pub async fn run_cmd_stream(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    terminal_id: String,
    project_dir: String,
    cmd: String,
    timeout_ms: Option<u64>,
) -> Result<RunCmdResult, String> {
    if terminal_id.is_empty() {
        return Err("terminal_id cannot be empty".to_string());
    }

    let root = std::path::Path::new(&project_dir)
        .canonicalize()
        .map_err(|e| format!("invalid project root: {e}"))?;

    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/C".to_string(), cmd])
    } else {
        ("sh", vec!["-c".to_string(), cmd])
    };

    let mut builder = tokio::process::Command::new(program);
    builder
        .args(&args)
        .current_dir(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        builder.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let mut child = builder.spawn().map_err(|e| e.to_string())?;
    let _child_pid = child.id();

    if let Some(pid) = _child_pid {
        let mut map = state.terminal_pids.lock().await;
        map.insert(terminal_id.clone(), pid);
    }

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let timeout_ms = timeout_ms.unwrap_or(30_000);
    let timeout_dur = Duration::from_millis(timeout_ms);

    let mut out_str = String::new();
    let mut err_str = String::new();
    let mut out_buf = [0u8; 512];
    let mut err_buf = [0u8; 512];

    loop {
        tokio::select! {
            r = tokio::time::timeout(timeout_dur, child.wait()) => {
                // Process finished, drain remaining output
                let exit_code = r
                    .ok()
                    .and_then(|s| s.ok())
                    .map(|s| s.code().unwrap_or(-1))
                    .unwrap_or(-1);
                if let Some(ref mut so) = stdout_pipe {
                    while let Ok(n) = so.read(&mut out_buf).await {
                        if n == 0 { break; }
                        let text = String::from_utf8_lossy(&out_buf[..n]).to_string();
                        out_str.push_str(&text);
                        let _ = app.emit("terminal:output", serde_json::json!({ "terminal_id": terminal_id, "stream": "stdout", "data": text }));
                    }
                }
                if let Some(ref mut se) = stderr_pipe {
                    while let Ok(n) = se.read(&mut err_buf).await {
                        if n == 0 { break; }
                        let text = String::from_utf8_lossy(&err_buf[..n]).to_string();
                        err_str.push_str(&text);
                        let _ = app.emit("terminal:output", serde_json::json!({ "terminal_id": terminal_id, "stream": "stderr", "data": text }));
                    }
                }
                // Emit done event
                {
                    let mut map = state.terminal_pids.lock().await;
                    map.remove(&terminal_id);
                }
                let _ = app.emit("terminal:done", serde_json::json!({ "terminal_id": terminal_id, "exit_code": exit_code }));
                return Ok(RunCmdResult {
                    stdout: out_str,
                    stderr: err_str,
                    exit_code,
                });
            }
            n = async {
                if let Some(ref mut so) = stdout_pipe {
                    so.read(&mut out_buf).await
                } else {
                    Ok(0)
                }
            } => {
                if let Ok(n) = n {
                    if n > 0 {
                        let text = String::from_utf8_lossy(&out_buf[..n]).to_string();
                        out_str.push_str(&text);
                        let _ = app.emit("terminal:output", serde_json::json!({ "terminal_id": terminal_id, "stream": "stdout", "data": text }));
                    }
                }
            }
            n = async {
                if let Some(ref mut se) = stderr_pipe {
                    se.read(&mut err_buf).await
                } else {
                    Ok(0)
                }
            } => {
                if let Ok(n) = n {
                    if n > 0 {
                        let text = String::from_utf8_lossy(&err_buf[..n]).to_string();
                        err_str.push_str(&text);
                        let _ = app.emit("terminal:output", serde_json::json!({ "terminal_id": terminal_id, "stream": "stderr", "data": text }));
                    }
                }
            }
        }
    }
}

#[tauri::command]
pub async fn terminal_kill(
    state: tauri::State<'_, AppState>,
    terminal_id: String,
) -> Result<(), String> {
    if terminal_id.is_empty() {
        return Err("terminal_id cannot be empty".to_string());
    }

    // Get PID but keep it in the map until kill completes to avoid race condition
    let pid = {
        let map = state.terminal_pids.lock().await;
        map.get(&terminal_id).copied()
    };

    let Some(pid) = pid else {
        return Ok(());
    };

    #[cfg(windows)]
    {
        // `taskkill /T /F` = kill process tree.
        let result = tokio::process::Command::new("taskkill")
            .arg("/T")
            .arg("/F")
            .arg("/PID")
            .arg(pid.to_string())
            .status()
            .await;

        // Remove from map after kill attempt (success or failure)
        {
            let mut map = state.terminal_pids.lock().await;
            map.remove(&terminal_id);
        }

        if let Err(e) = result {
            tracing::warn!("Failed to kill terminal {} (PID {}): {}", terminal_id, pid, e);
        }
        return Ok(());
    }

    #[cfg(unix)]
    {
        let result = tokio::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await;

        // Remove from map after kill attempt
        {
            let mut map = state.terminal_pids.lock().await;
            map.remove(&terminal_id);
        }

        if let Err(e) = result {
            tracing::warn!("Failed to kill terminal {} (PID {}): {}", terminal_id, pid, e);
        }
        return Ok(());
    }
}

/// Resolves a pending `ai:confirm_request`. Used by the UI's confirm modal.
///
/// `async` so it can `await` the tokio `pending_confirms` lock — the map
/// is shared with the async tool-call path, so using `std::sync::Mutex`
/// here would require holding it across an `.await` point in the caller.
#[tauri::command]
pub async fn confirm_cmd(
    state: tauri::State<'_, AppState>,
    id: String,
    approved: bool,
) -> Result<(), String> {
    let tx = {
        let mut map = state.pending_confirms.lock().await;
        map.remove(&id)
    };
    match tx {
        Some(sender) => {
            let _ = sender.send(approved);
            Ok(())
        }
        None => Err(format!("no pending confirmation with id {id}")),
    }
}

/// Shared process-spawning implementation used by the legacy AI tool
/// loop (`execute_run_cmd_gated` / `run_cmd` / `run_cmd_stream`) and,
/// since Phase 2.B, by [`crate::run_cmd_gate::execute_run_cmd`]. The
/// function is crate-visible because the Phase 2.B gate wraps it with
/// classifier-driven policy decisions but reuses the same
/// cancel-aware, tree-killing, pipe-teeing implementation rather than
/// duplicating it.
pub(crate) async fn run_cmd_impl(
    project_dir: &str,
    cmd: &str,
    timeout_ms: u64,
    cancel: Option<&CancelToken>,
    app: Option<&AppHandle>,
) -> Result<RunCmdResult, String> {
    let root = std::path::Path::new(project_dir)
        .canonicalize()
        .map_err(|e| format!("invalid project root: {e}"))?;

    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/C".to_string(), cmd.to_string()])
    } else {
        ("sh", vec!["-c".to_string(), cmd.to_string()])
    };

    let mut builder = tokio::process::Command::new(program);
    builder
        .args(&args)
        .current_dir(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Make the child the leader of a fresh process group at *spawn
    // time* so a subtree kill actually propagates to everything it
    // spawned (`sh -c` often forks `cargo`, `npm`, etc. which in turn
    // fork more children — `child.kill()` alone only hits the direct
    // shell). Doing this after the fork is too late: `killpg` would
    // either no-op or, worse, kill the wrong pgid. On Unix the builder
    // sets pgid=0 via `CommandExt::process_group`; on Windows it sets
    // `CREATE_NEW_PROCESS_GROUP` so the shell is the root of a fresh
    // console-control group. Actual tree teardown is in `kill_tree`.
    #[cfg(unix)]
    {
        // `tokio::process::Command::process_group(0)` is a direct
        // inherent method (no trait import needed); `0` means "make
        // the child the leader of a fresh process group whose pgid
        // equals its pid", which is exactly the invariant `kill_tree`
        // relies on.
        builder.process_group(0);
    }
    #[cfg(windows)]
    {
        // `CREATE_NEW_PROCESS_GROUP` (0x00000200) makes the shell the
        // root of its own console-control group. That isn't strictly
        // required for `taskkill /T /F` to work (which walks by
        // parent-pid), but it keeps our child from inheriting the
        // parent's Ctrl+C handler and from accidentally sharing a
        // process group with other things running under the IDE.
        // `creation_flags` is an inherent method on
        // `tokio::process::Command` on Windows; no trait import.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        builder.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let mut child = builder.spawn().map_err(|e| e.to_string())?;
    let child_pid = child.id();

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Drive the child through its cancel/timeout gauntlet while tee-ing
    // any piped output to the Agent terminal tab in real time. On cancel
    // we kill the entire process *tree* (see `kill_tree`) so we don't
    // leak runaway grandchildren; the reaped status is discarded.
    let timeout_dur = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout_dur;

    let mut out_str = String::new();
    let mut err_str = String::new();
    let mut out_buf = [0u8; 512];
    let mut err_buf = [0u8; 512];

    // Read pipes incrementally alongside `child.wait()` so every chunk
    // can be forwarded to `app` (when present) before the process ends.
    // Each iteration races: cancel, timeout deadline, stdout ready,
    // stderr ready, and child-exit. The loop terminates when the child
    // exits *or* is torn down by cancel/timeout.
    let exit_code: i32 = loop {
        tokio::select! {
            biased;
            _ = async {
                match cancel {
                    Some(c) => c.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                kill_tree(&mut child, child_pid).await;
                return Err(cancel.map(|c| c.err_string()).unwrap_or_else(|| "cancelled".into()));
            }
            _ = tokio::time::sleep_until(deadline) => {
                kill_tree(&mut child, child_pid).await;
                return Err(format!("run_cmd timed out after {timeout_ms}ms"));
            }
            r = async {
                match stdout_pipe.as_mut() {
                    Some(p) => p.read(&mut out_buf).await,
                    None => std::future::pending().await,
                }
            } => {
                match r {
                    Ok(0) => { stdout_pipe = None; }
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&out_buf[..n]).to_string();
                        out_str.push_str(&text);
                        if let Some(app) = app {
                            emit_agent_line(app, "stdout", text);
                        }
                    }
                    Err(_) => { stdout_pipe = None; }
                }
            }
            r = async {
                match stderr_pipe.as_mut() {
                    Some(p) => p.read(&mut err_buf).await,
                    None => std::future::pending().await,
                }
            } => {
                match r {
                    Ok(0) => { stderr_pipe = None; }
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&err_buf[..n]).to_string();
                        err_str.push_str(&text);
                        if let Some(app) = app {
                            emit_agent_line(app, "stderr", text);
                        }
                    }
                    Err(_) => { stderr_pipe = None; }
                }
            }
            r = child.wait() => {
                let wait_status = r.map_err(|e| e.to_string())?;
                // Drain whatever is still buffered in the pipes so the
                // returned strings are complete (matches pre-refactor
                // semantics for callers that consume `stdout`/`stderr`).
                if let Some(ref mut so) = stdout_pipe {
                    loop {
                        match so.read(&mut out_buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let text = String::from_utf8_lossy(&out_buf[..n]).to_string();
                                out_str.push_str(&text);
                                if let Some(app) = app {
                                    emit_agent_line(app, "stdout", text);
                                }
                            }
                        }
                    }
                }
                if let Some(ref mut se) = stderr_pipe {
                    loop {
                        match se.read(&mut err_buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let text = String::from_utf8_lossy(&err_buf[..n]).to_string();
                                err_str.push_str(&text);
                                if let Some(app) = app {
                                    emit_agent_line(app, "stderr", text);
                                }
                            }
                        }
                    }
                }
                break wait_status.code().unwrap_or(-1);
            }
        }
    };

    Ok(RunCmdResult {
        stdout: out_str,
        stderr: err_str,
        exit_code,
    })
}

/// Best-effort kill of the child and every descendant it spawned.
///
/// **Escalation policy**: graceful-then-forceful so long-running tools
/// like `npm install` / `cargo build` get a chance to clean up lockfiles
/// / partial artifacts before we hammer them. Whether or not the grace
/// step is observed, the forceful step *always* runs on a 200ms window
/// so we never block callers longer than that.
///
/// - **Unix** (pgid set at spawn via `CommandExt::process_group(0)`):
///   1. `killpg(pgid, SIGTERM)` — polite request to the whole group.
///   2. Sleep up to 200ms to let well-behaved children exit.
///   3. If anything is still alive (`killpg(pgid, 0)` returns 0),
///      escalate to `killpg(pgid, SIGKILL)` — unconditional teardown.
///
/// - **Windows** (`CREATE_NEW_PROCESS_GROUP` set at spawn so the shell
///   and its descendants share one group we can target):
///   1. `child.kill()` — `TerminateProcess` on the shell. Handles the
///      common "just a shell, no subprocesses" case in one syscall.
///   2. Sleep 200ms so short-lived descendants self-exit.
///   3. `taskkill /T /F /PID <pid>` — sweep any surviving descendants
///      by parent-pid lookup. `/T` walks the tree, `/F` force-terminates.
///
/// Always ends with `child.wait()` so we don't leak a zombie even if
/// the signal path already raced to completion.
async fn kill_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            // `pid` is the shell's pid; because we set pgid=0 at spawn,
            // the process-group id equals the shell's pid. Negating the
            // pid in `libc::kill` signals the whole group.
            let signed_pid = pid as libc::pid_t;
            // Step 1: SIGTERM the whole group. SAFETY: signalling a
            // negated pid targets the pgrp. ESRCH (group already gone)
            // is fine and ignored.
            unsafe {
                libc::kill(-signed_pid, libc::SIGTERM);
            }
            // Step 2: give well-behaved children a window to exit.
            // 200ms is long enough for most cleanup hooks but short
            // enough that a user pressing Cancel doesn't notice the
            // delay.
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Step 3: is anything still alive? `kill(-pgid, 0)` with
            // signal 0 is a no-op that returns 0 iff the group still
            // has members. If so, escalate to SIGKILL.
            let alive = unsafe { libc::kill(-signed_pid, 0) == 0 };
            if alive {
                unsafe {
                    libc::kill(-signed_pid, libc::SIGKILL);
                }
            }
        } else {
            let _ = child.kill().await;
        }
    }
    #[cfg(windows)]
    {
        // Step 1: polite termination of the direct child. On Windows
        // `child.kill()` maps to `TerminateProcess` on the shell, which
        // does not recurse — descendants become orphans reparented to
        // `explorer.exe`. That's why we also do step 3 below.
        let _ = child.kill().await;
        if let Some(pid) = pid {
            // Step 2: let short-lived descendants self-exit.
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Step 3: sweep surviving descendants. `/T` = walk the
            // process tree by parent-pid, `/F` = force-terminate.
            let _ = tokio::process::Command::new("taskkill")
                .arg("/T")
                .arg("/F")
                .arg("/PID")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }
    // Always reap so we don't leak a zombie even if the signal path
    // already raced to completion.
    let _ = child.wait().await;
}

// Silence the unused-import lint when this module's `HashMap` re-export is
// not needed by consumers. Keeping the import local to `tools.rs` only.
#[allow(dead_code)]
fn _keep_hashmap_in_scope() -> HashMap<String, String> {
    HashMap::new()
}

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use crate::cancel::CancelReason;
    use std::time::Instant;

    // Spawn a long-running child inside a tempdir, trip the cancel
    // token mid-flight, and assert we come back with
    // `Err("cancelled: user")` well before the command's natural runtime
    // would have elapsed. This is the test that proves we are killing
    // the subprocess, not just checking a flag between iterations.
    #[tokio::test]
    async fn run_cmd_cancel_mid_flight_kills_child() {
        let dir = std::env::temp_dir();
        let token = CancelToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t2.cancel();
        });
        let start = Instant::now();
        // 30s sleep; if cancellation worked we return inside ~200ms.
        let res = run_cmd_impl(
            dir.to_str().unwrap(),
            if cfg!(windows) { "timeout /T 30" } else { "sleep 30" },
            30_000,
            Some(&token),
            None,
        )
        .await;
        let elapsed = start.elapsed();
        assert!(
            matches!(&res, Err(e) if e.starts_with("cancelled")),
            "expected cancelled, got {:?}",
            res
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel should unwind within a few seconds, took {:?}",
            elapsed
        );
    }

    // Regression guard: the cancel branch of run_cmd_impl used to compute
    // the error string *before* the select! fired, which captured the
    // token's pre-trip state (reason=None -> bare "cancelled") and
    // discarded whatever reason the cancel later carried. This asserts
    // the reason actually propagates on mid-flight cancellation.
    #[tokio::test]
    async fn run_cmd_mid_flight_cancel_preserves_reason() {
        let dir = std::env::temp_dir();
        let token = CancelToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            // Give the child a brief head start so cancellation is
            // genuinely mid-flight, not racing the spawn.
            tokio::time::sleep(Duration::from_millis(100)).await;
            t2.cancel_with(CancelReason::Goal);
        });
        let res = run_cmd_impl(
            dir.to_str().unwrap(),
            if cfg!(windows) { "timeout /T 30" } else { "sleep 30" },
            30_000,
            Some(&token),
            None,
        )
        .await;
        match &res {
            Err(e) => assert_eq!(
                e, "cancelled: goal",
                "mid-flight cancel should propagate the CancelReason"
            ),
            Ok(_) => panic!("expected cancelled error, got {:?}", res),
        }
    }

    #[tokio::test]
    async fn run_cmd_pre_cancelled_token_returns_before_spawn_completes() {
        let dir = std::env::temp_dir();
        let token = CancelToken::new();
        token.cancel_with(CancelReason::Goal);
        let res = run_cmd_impl(
            dir.to_str().unwrap(),
            if cfg!(windows) { "timeout /T 30" } else { "sleep 30" },
            30_000,
            Some(&token),
            None,
        )
        .await;
        // Reason is propagated through to the error string.
        match &res {
            Err(e) => assert_eq!(e, "cancelled: goal"),
            Ok(_) => panic!("expected cancelled error, got {:?}", res),
        }
    }

    // A healthy command with no cancel should complete normally through
    // the new select! path and not be affected by the cancel plumbing.
    #[tokio::test]
    async fn run_cmd_no_cancel_runs_to_completion() {
        let dir = std::env::temp_dir();
        let res = run_cmd_impl(
            dir.to_str().unwrap(),
            if cfg!(windows) { "echo hi" } else { "echo hi" },
            10_000,
            None,
            None,
        )
        .await
        .expect("command should succeed");
        assert_eq!(res.exit_code, 0);
        assert!(res.stdout.contains("hi"));
    }

    // The whole point of PR #6. Start a shell that forks a long-sleeping
    // grandchild, write its pid to a file we can stat from outside the
    // process tree, cancel mid-flight, then verify the grandchild was
    // actually killed (pid no longer exists) rather than orphaned and
    // reparented to init. Unix-only because the test uses /proc/<pid>
    // and `kill -0`; on Windows the semantics are covered by the
    // `taskkill /T /F` fallback but asserting it from a unit test is
    // significantly more finicky.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_cmd_cancel_kills_grandchild_not_just_shell() {
        // Fresh tempdir so a stale pid file from a prior run can't lie
        // to us about which process is "ours".
        let tmp = std::env::temp_dir().join(format!(
            "occ-treekill-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).expect("create tempdir");
        let pid_path = tmp.join("gc.pid");
        // Wipe any leftover file before the test.
        let _ = std::fs::remove_file(&pid_path);

        // The shell script: fork a `sleep 60` into the background, write
        // its pid to a file, then `wait` on it. Without process-group
        // kill, cancelling the shell orphans the `sleep` and we'd fail
        // the kill-check below.
        let pid_path_s = pid_path.to_str().unwrap().to_string();
        let script = format!(
            "sleep 60 & echo $! > {pid}; wait",
            pid = pid_path_s
        );

        let token = CancelToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            // Give the shell enough time to fork the grandchild and
            // write the pid file before we cancel.
            tokio::time::sleep(Duration::from_millis(300)).await;
            t2.cancel_with(CancelReason::Goal);
        });

        let res = run_cmd_impl(
            tmp.to_str().unwrap(),
            &script,
            60_000,
            Some(&token),
            None,
        )
        .await;
        assert!(
            matches!(&res, Err(e) if e.starts_with("cancelled")),
            "expected cancelled, got {:?}",
            res
        );

        // Read grandchild pid and probe it with kill -0 (signal 0 =
        // existence check, no actual signal delivered). Under the old
        // `child.kill()` implementation this was typically alive.
        let raw = std::fs::read_to_string(&pid_path).expect("read pid file");
        let gc_pid: libc::pid_t = raw.trim().parse().expect("parse pid");

        // Give the OS a beat to reap everything.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // kill(pid, 0) returns 0 iff the process exists. If it returns
        // -1 with errno=ESRCH, the process is gone (success for us).
        let alive = unsafe { libc::kill(gc_pid, 0) == 0 };
        assert!(
            !alive,
            "grandchild pid {} was still alive after tree-kill",
            gc_pid
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod autonomous_confirm_tests {
    use super::*;

    // --- should_prompt_run_cmd gating ----------------------------------

    #[test]
    fn allow_list_match_skips_prompt_in_normal_mode() {
        // Allow-list match + confirm_required + NOT autonomous_confirm:
        // the allow-list should auto-approve, so no prompt.
        let allow = vec!["cargo check".into()];
        assert!(!should_prompt_run_cmd("cargo check", &allow, true, false));
        assert!(!should_prompt_run_cmd(
            "cargo check --release",
            &allow,
            true,
            false,
        ));
    }

    #[test]
    fn autonomous_confirm_bypasses_allow_list() {
        // Same allow-list, but autonomous_confirm=true: every invocation
        // prompts anyway. This is the headline behaviour of PR #10.
        let allow = vec!["cargo check".into()];
        assert!(should_prompt_run_cmd("cargo check", &allow, false, true));
        assert!(should_prompt_run_cmd(
            "cargo check --release",
            &allow,
            false,
            true,
        ));
    }

    #[test]
    fn unknown_cmd_prompts_when_confirm_required_regardless_of_mode() {
        // No allow-list entry — both modes prompt so long as
        // cmd_confirm_required is on.
        let allow: Vec<String> = vec![];
        assert!(should_prompt_run_cmd("rm foo.txt", &allow, true, false));
        assert!(should_prompt_run_cmd("rm foo.txt", &allow, true, true));
    }

    #[test]
    fn unknown_cmd_without_confirm_required_only_prompts_in_autonomous() {
        // With confirm_required=false the non-autonomous path stays
        // silent; autonomous_confirm still forces a prompt.
        let allow: Vec<String> = vec![];
        assert!(!should_prompt_run_cmd("echo hi", &allow, false, false));
        assert!(should_prompt_run_cmd("echo hi", &allow, false, true));
    }

    #[test]
    fn empty_allow_list_entries_do_not_match() {
        // Sanity: an empty string in the allow-list must not silently
        // match every command.
        let allow: Vec<String> = vec!["".into()];
        assert!(should_prompt_run_cmd("anything", &allow, true, false));
    }

    // --- write_would_change_existing_file heuristic --------------------

    fn unique_tempdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "occ-writegate-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&p).expect("create tempdir");
        p
    }

    #[test]
    fn write_gate_new_file_is_not_destructive() {
        let tmp = unique_tempdir("new");
        let changed =
            write_would_change_existing_file(tmp.to_str().unwrap(), "fresh.txt", "hello");
        assert_eq!(changed, Some(false), "creating a new file should not prompt");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_gate_identical_content_is_no_op() {
        let tmp = unique_tempdir("same");
        let path = tmp.join("same.txt");
        std::fs::write(&path, "unchanged").expect("seed");
        let changed =
            write_would_change_existing_file(tmp.to_str().unwrap(), "same.txt", "unchanged");
        assert_eq!(
            changed,
            Some(false),
            "rewriting identical content should not prompt"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_gate_changed_content_is_destructive() {
        let tmp = unique_tempdir("diff");
        let path = tmp.join("diff.txt");
        std::fs::write(&path, "old").expect("seed");
        let changed =
            write_would_change_existing_file(tmp.to_str().unwrap(), "diff.txt", "new");
        assert_eq!(
            changed,
            Some(true),
            "changing an existing file should prompt when autonomous_confirm is on"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Regression for the Devin Review finding on PR #10: the gate used
    // `Path::new(project_dir).join(sub_path)`, which `join` replaces
    // with `sub_path` entirely when the latter is absolute. An AI that
    // emitted `/src/foo.rs` would therefore check `/src/foo.rs` (which
    // doesn't exist, so Some(false) → no prompt), while the actual
    // `fs_ops::write_file` call strips the leading `/` and overwrites
    // the real file inside the project. This assertion pins the fix:
    // a leading-slash sub_path must still be seen as a destructive
    // change to the sandboxed file.
    #[test]
    fn write_gate_leading_slash_matches_fs_ops_resolution() {
        let tmp = unique_tempdir("slash");
        let sub = tmp.join("src");
        std::fs::create_dir_all(&sub).expect("create subdir");
        std::fs::write(sub.join("foo.rs"), "old").expect("seed");
        // NB: sub_path has a leading `/`, exactly the shape the bug
        // relied on.
        let changed =
            write_would_change_existing_file(tmp.to_str().unwrap(), "/src/foo.rs", "new");
        assert_eq!(
            changed,
            Some(true),
            "leading-slash paths must resolve inside the sandbox like fs_ops::write_file does"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Paths that escape the sandbox (e.g. `..` traversal that lands
    // outside the project root) must NOT trigger a prompt: the actual
    // `fs_ops::write_file` call will reject them with an error, so
    // there's no meaningful user decision to make. The gate should
    // quietly return Some(false) and let the real write fail.
    #[test]
    fn write_gate_sandbox_escape_does_not_prompt() {
        let tmp = unique_tempdir("escape");
        // Walk far enough above the project root that we land outside,
        // pointing at /etc/passwd or similar sensitive paths.
        let changed = write_would_change_existing_file(
            tmp.to_str().unwrap(),
            "../../../../../../etc/passwd",
            "pwned",
        );
        assert_eq!(
            changed,
            Some(false),
            "sandbox-escape paths must not trip the confirm modal"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
