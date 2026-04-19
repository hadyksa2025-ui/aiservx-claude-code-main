//! Tool runtime. Executes tool calls emitted by the AI layer inside the
//! opened project root. Each tool returns an `(output, optional_diff)` pair.

use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::fs_ops;

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

/// Run a single tool call. Returns (textual output, optional diff).
pub async fn execute(
    project_dir: &str,
    name: &str,
    args: &Value,
) -> Result<(String, Option<String>), String> {
    match name {
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = fs_ops::read_file(project_dir.to_string(), path.to_string())?;
            let truncated = if content.len() > 100_000 {
                format!("{}\n… (truncated, {} bytes)", &content[..100_000], content.len())
            } else {
                content
            };
            Ok((truncated, None))
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let diff = fs_ops::write_file(
                project_dir.to_string(),
                path.to_string(),
                content.to_string(),
            )?;
            Ok((format!("wrote {}", path), Some(diff)))
        }
        "list_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let entries = fs_ops::list_dir(project_dir.to_string(), path.to_string())?;
            let summary = entries
                .iter()
                .map(|e| format!("{}{}", if e.is_dir { "📁 " } else { "📄 " }, e.name))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((summary, None))
        }
        "run_cmd" => {
            let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);
            let result = run_cmd_impl(project_dir, cmd, timeout_ms).await?;
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
            Ok((out, None))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

#[tauri::command]
pub async fn run_cmd(
    project_dir: String,
    cmd: String,
    timeout_ms: Option<u64>,
) -> Result<RunCmdResult, String> {
    run_cmd_impl(&project_dir, &cmd, timeout_ms.unwrap_or(30_000)).await
}

async fn run_cmd_impl(project_dir: &str, cmd: &str, timeout_ms: u64) -> Result<RunCmdResult, String> {
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

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let wait_fut = async {
        let status = child.wait().await.map_err(|e| e.to_string())?;
        let mut out_str = String::new();
        if let Some(mut so) = stdout {
            let _ = so.read_to_string(&mut out_str).await;
        }
        let mut err_str = String::new();
        if let Some(mut se) = stderr {
            let _ = se.read_to_string(&mut err_str).await;
        }
        Ok::<_, String>((status, out_str, err_str))
    };

    match tokio::time::timeout(Duration::from_millis(timeout_ms), wait_fut).await {
        Ok(Ok((status, stdout, stderr))) => Ok(RunCmdResult {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("run_cmd timed out after {timeout_ms}ms")),
    }
}
