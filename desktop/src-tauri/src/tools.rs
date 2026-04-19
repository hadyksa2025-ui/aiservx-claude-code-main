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
pub async fn execute_safe(
    project_dir: &str,
    name: &str,
    args: &Value,
) -> Result<(String, Option<String>, ToolEffect), String> {
    match name {
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
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
            Ok((
                truncated,
                None,
                ToolEffect {
                    touched_files: vec![path.to_string()],
                },
            ))
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let diff = fs_ops::write_file(
                project_dir.to_string(),
                path.to_string(),
                content.to_string(),
            )?;
            Ok((
                format!("wrote {}", path),
                Some(diff),
                ToolEffect {
                    touched_files: vec![path.to_string()],
                },
            ))
        }
        "list_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let entries = fs_ops::list_dir(project_dir.to_string(), path.to_string())?;
            let summary = entries
                .iter()
                .map(|e| format!("{}{}", if e.is_dir { "📁 " } else { "📄 " }, e.name))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((summary, None, ToolEffect::default()))
        }
        other => Err(format!("unknown safe tool: {other}")),
    }
}

/// Gated `run_cmd`. Applies the deny-list and allow-list, emits an
/// `ai:confirm_request` event for everything else, and awaits the UI's
/// decision (up to 10 minutes) before executing.
///
/// `cancel` is checked before the deny-list, races the confirm modal
/// await, and is threaded into the child-process wait so cancel aborts
/// this call no matter where it is parked.
pub async fn execute_run_cmd_gated(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    args: &Value,
    cancel: &CancelToken,
) -> Result<(String, Option<String>, ToolEffect), String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
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
        let s = state.settings.lock().unwrap();
        (s.cmd_allow_list.clone(), s.cmd_confirm_required)
    };

    let auto_ok = allow_list
        .iter()
        .any(|p| !p.is_empty() && cmd_matches_prefix(cmd, p));

    if !auto_ok && confirm_required {
        let id = format!("confirm_{}", uuid::Uuid::new_v4().simple());
        let (tx, rx) = oneshot::channel::<bool>();
        {
            let mut map = state.pending_confirms.lock().unwrap();
            map.insert(id.clone(), tx);
        }
        let _ = app.emit(
            "ai:confirm_request",
            json!({
                "id": id,
                "cmd": cmd,
                "project_dir": project_dir,
                "timeout_ms": timeout_ms,
            }),
        );
        let approved = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Evict and return cancelled. The modal will disappear
                // when the event listener on the UI side tears down.
                let mut map = state.pending_confirms.lock().unwrap();
                map.remove(&id);
                return Err("cancelled".into());
            }
            r = tokio::time::timeout(Duration::from_secs(600), rx) => match r {
                Ok(Ok(v)) => v,
                Ok(Err(_)) => false, // sender dropped -> deny
                Err(_) => {
                    // Timed out. Evict the pending confirm so it doesn't leak.
                    let mut map = state.pending_confirms.lock().unwrap();
                    map.remove(&id);
                    return Ok((
                        "refused: confirmation timed out (10 minutes).".into(),
                        None,
                        ToolEffect::default(),
                    ));
                }
            },
        };
        if !approved {
            return Ok((
                format!("refused: user denied `{cmd}`."),
                None,
                ToolEffect::default(),
            ));
        }
    }

    let result = run_cmd_impl(project_dir, cmd, timeout_ms, Some(cancel)).await?;
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
    run_cmd_impl(&project_dir, &cmd, timeout_ms.unwrap_or(30_000), None).await
}

/// Resolves a pending `ai:confirm_request`. Used by the UI's confirm modal.
#[tauri::command]
pub fn confirm_cmd(
    state: tauri::State<'_, AppState>,
    id: String,
    approved: bool,
) -> Result<(), String> {
    let tx = {
        let mut map = state.pending_confirms.lock().unwrap();
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

async fn run_cmd_impl(
    project_dir: &str,
    cmd: &str,
    timeout_ms: u64,
    cancel: Option<&CancelToken>,
) -> Result<RunCmdResult, String> {
    let root = std::path::Path::new(project_dir)
        .canonicalize()
        .map_err(|e| format!("invalid project root: {e}"))?;

    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/C".to_string(), cmd.to_string()])
    } else {
        ("sh", vec!["-c".to_string(), cmd.to_string()])
    };

    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .current_dir(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Drive the child through its cancel/timeout gauntlet. On cancel we
    // send a SIGKILL-equivalent via `child.kill()` so we don't leak a
    // runaway process; the reaped status is discarded.
    let cancel_fut = async {
        match cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };

    let timeout_dur = Duration::from_millis(timeout_ms);

    let status = tokio::select! {
        biased;
        _ = cancel_fut => {
            // Best-effort kill. The child may already be dead; ignore
            // the error in that case.
            let _ = child.kill().await;
            return Err("cancelled".into());
        }
        r = tokio::time::timeout(timeout_dur, child.wait()) => match r {
            Ok(s) => s.map_err(|e| e.to_string())?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(format!("run_cmd timed out after {timeout_ms}ms"));
            }
        }
    };

    // Drain pipes after the child has exited (or been killed). The drain
    // itself is also cancel-aware so a late cancel doesn't hang here.
    let mut out_str = String::new();
    if let Some(ref mut so) = stdout_pipe {
        let drain = so.read_to_string(&mut out_str);
        let cancel_fut = async {
            match cancel {
                Some(c) => c.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancel_fut => return Err("cancelled".into()),
            _ = drain => {}
        }
    }
    let mut err_str = String::new();
    if let Some(ref mut se) = stderr_pipe {
        let drain = se.read_to_string(&mut err_str);
        let cancel_fut = async {
            match cancel {
                Some(c) => c.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancel_fut => return Err("cancelled".into()),
            _ = drain => {}
        }
    }

    Ok(RunCmdResult {
        stdout: out_str,
        stderr: err_str,
        exit_code: status.code().unwrap_or(-1),
    })
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
    use std::time::Instant;

    // Spawn a long-running child inside a tempdir, trip the cancel
    // token mid-flight, and assert we come back with `Err("cancelled")`
    // well before the command's natural runtime would have elapsed. This
    // is the test that proves we are killing the subprocess, not just
    // checking a flag between iterations.
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
        )
        .await;
        let elapsed = start.elapsed();
        assert!(
            matches!(&res, Err(e) if e == "cancelled"),
            "expected cancelled, got {:?}",
            res
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel should unwind within a few seconds, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn run_cmd_pre_cancelled_token_returns_before_spawn_completes() {
        let dir = std::env::temp_dir();
        let token = CancelToken::new();
        token.cancel();
        let res = run_cmd_impl(
            dir.to_str().unwrap(),
            if cfg!(windows) { "timeout /T 30" } else { "sleep 30" },
            30_000,
            Some(&token),
        )
        .await;
        assert!(matches!(&res, Err(e) if e == "cancelled"));
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
        )
        .await
        .expect("command should succeed");
        assert_eq!(res.exit_code, 0);
        assert!(res.stdout.contains("hi"));
    }
}
