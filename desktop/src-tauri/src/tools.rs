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
                return Err(cancel.err_string());
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
        builder.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = builder.spawn().map_err(|e| e.to_string())?;
    let child_pid = child.id();

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Drive the child through its cancel/timeout gauntlet. On cancel we
    // kill the entire process *tree* (see `kill_tree`) so we don't leak
    // runaway grandchildren; the reaped status is discarded.
    let cancel_fut = async {
        match cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };

    let timeout_dur = Duration::from_millis(timeout_ms);
    let cancel_err = match cancel {
        Some(c) => c.err_string(),
        None => "cancelled".to_string(),
    };

    let status = tokio::select! {
        biased;
        _ = cancel_fut => {
            kill_tree(&mut child, child_pid).await;
            return Err(cancel_err);
        }
        r = tokio::time::timeout(timeout_dur, child.wait()) => match r {
            Ok(s) => s.map_err(|e| e.to_string())?,
            Err(_) => {
                kill_tree(&mut child, child_pid).await;
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
            _ = cancel_fut => return Err(cancel.map(|c| c.err_string()).unwrap_or_else(|| "cancelled".into())),
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
            _ = cancel_fut => return Err(cancel.map(|c| c.err_string()).unwrap_or_else(|| "cancelled".into())),
            _ = drain => {}
        }
    }

    Ok(RunCmdResult {
        stdout: out_str,
        stderr: err_str,
        exit_code: status.code().unwrap_or(-1),
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
