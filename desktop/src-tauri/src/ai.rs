//! Hybrid AI layer: routes between an **OpenRouter** planner and an
//! **Ollama** executor using an OpenAI-compatible tool-calling protocol.
//!
//! The loop is:
//!   1. Build an OpenAI-shaped `messages` list (system + history + user).
//!   2. On the first iteration, if a planner key is configured, call the
//!      planner (OpenRouter). Otherwise, call the executor (Ollama).
//!   3. If the model returns `tool_calls`, execute each call in Rust, emit
//!      `ai:tool_call` + `ai:tool_result` events, append a `tool` message and
//!      loop back to step 2 (but now always using the executor).
//!   4. When the model returns a plain assistant message, emit `ai:done`
//!      and return.
//!
//! If any planner request fails, we transparently fall back to Ollama.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tracing::{info, warn};

use crate::{memory, tools, AppState, Settings};

const MAX_ITERATIONS: usize = 8;
const SYSTEM_PROMPT: &str = r#"You are Open Claude Code, an AI coding assistant embedded in a desktop IDE.

You have access to the following tools (OpenAI-style function calls):
  - read_file(path)          -> read a text file, relative to the project root
  - write_file(path, content)-> write/overwrite a text file; returns a diff
  - list_dir(path)           -> list immediate children of a directory
  - run_cmd(cmd, timeout_ms) -> run a shell command inside the project root

Guidelines:
- Prefer reading before writing. When you write, include the FULL new file
  contents — never partial snippets.
- Keep each response focused. Do not narrate every tool call; the UI already
  shows them. After the last tool call, produce a short human summary of what
  you did.
- Paths are always relative to the opened project root. Do not use absolute
  paths and do not try to escape the root.
- If you don't need any tools, just answer the user directly.
"#;

// ---------- UI message format ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiToolResult {
    pub id: String,
    pub ok: bool,
    pub output: String,
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiMessage {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Option<Vec<UiToolCall>>,
    #[serde(default)]
    pub tool_results: Option<Vec<UiToolResult>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    pub assistant: String,
    pub tool_calls: Vec<UiToolCall>,
    pub tool_results: Vec<UiToolResult>,
}

// ---------- Wire messages (OpenAI-shaped) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolCallFn {
    name: String,
    /// OpenRouter returns a JSON string here, Ollama returns an object.
    /// We keep it as a raw `Value` and handle both at call time.
    arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    _type: Option<String>,
    function: WireToolCallFn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireMessage {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    tool_call_id: Option<String>,
}

impl WireMessage {
    fn system(s: &str) -> Self {
        Self {
            role: "system".into(),
            content: Some(s.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    fn user(s: &str) -> Self {
        Self {
            role: "user".into(),
            content: Some(s.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    fn assistant(text: &str, tool_calls: Option<Vec<WireToolCall>>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(text.to_string()),
            tool_calls,
            tool_call_id: None,
        }
    }
    fn tool(id: &str, text: &str) -> Self {
        Self {
            role: "tool".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }
}

fn build_initial_messages(history: &[UiMessage], user: &str) -> Vec<WireMessage> {
    let mut msgs = Vec::with_capacity(history.len() + 2);
    msgs.push(WireMessage::system(SYSTEM_PROMPT));
    for m in history {
        match m.role.as_str() {
            "user" => msgs.push(WireMessage::user(&m.content)),
            "assistant" => {
                // We intentionally flatten prior tool_calls into plain text in
                // history so we don't need to replay tool_call_ids.
                msgs.push(WireMessage::assistant(&m.content, None));
            }
            "system" => msgs.push(WireMessage::system(&m.content)),
            _ => { /* ignore tool messages from the UI */ }
        }
    }
    msgs.push(WireMessage::user(user));
    msgs
}

// ---------- Provider calls ----------

async fn call_openrouter(
    settings: &Settings,
    messages: &[WireMessage],
    tools_schema: &Value,
) -> Result<WireMessage, String> {
    if settings.openrouter_api_key.is_empty() {
        return Err("no OpenRouter API key".into());
    }
    let body = json!({
        "model": settings.openrouter_model,
        "messages": messages,
        "tools": tools_schema,
        "tool_choice": "auto",
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&settings.openrouter_api_key)
        .header("HTTP-Referer", "https://github.com/salonadel6-sudo/open-claude-code-main")
        .header("X-Title", "Open Claude Code Desktop")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("openrouter request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("openrouter http {status}: {text}"));
    }
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    let choice = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .cloned()
        .ok_or_else(|| "openrouter: missing choices[0].message".to_string())?;
    let mut msg: WireMessage =
        serde_json::from_value(choice).map_err(|e| format!("openrouter parse: {e}"))?;
    // OpenAI returns tool_calls[].function.arguments as a JSON string; parse it
    // into a Value for internal uniformity with Ollama.
    if let Some(tcs) = msg.tool_calls.as_mut() {
        for tc in tcs.iter_mut() {
            if let Value::String(s) = &tc.function.arguments {
                if let Ok(v) = serde_json::from_str::<Value>(s) {
                    tc.function.arguments = v;
                }
            }
        }
    }
    Ok(msg)
}

async fn call_ollama(
    settings: &Settings,
    messages: &[WireMessage],
    tools_schema: &Value,
) -> Result<WireMessage, String> {
    let url = format!("{}/api/chat", settings.ollama_base_url.trim_end_matches('/'));
    // Ollama ignores tool_call_id; keep the rest.
    let body = json!({
        "model": settings.ollama_model,
        "messages": messages,
        "tools": tools_schema,
        "stream": false,
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("ollama request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("ollama http {status}: {text}"));
    }
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    let raw_msg = json
        .get("message")
        .cloned()
        .ok_or_else(|| "ollama: missing message".to_string())?;
    let mut msg: WireMessage =
        serde_json::from_value(raw_msg).map_err(|e| format!("ollama parse: {e}"))?;
    if let Some(tcs) = msg.tool_calls.as_mut() {
        for tc in tcs.iter_mut() {
            if tc.id.is_none() {
                tc.id = Some(format!("call_{}", uuid::Uuid::new_v4().simple()));
            }
        }
    }
    Ok(msg)
}

// ---------- Top-level commands ----------

#[tauri::command]
pub async fn check_planner(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let key = state.settings.lock().unwrap().openrouter_api_key.clone();
    Ok(!key.is_empty())
}

#[tauri::command]
pub async fn check_executor(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let base = state.settings.lock().unwrap().ollama_base_url.clone();
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    match client.get(&url).send().await {
        Ok(r) => Ok(r.status().is_success()),
        Err(_) => Ok(false),
    }
}

#[tauri::command]
pub fn cancel_chat(state: tauri::State<'_, AppState>) -> Result<(), String> {
    *state.cancelled.lock().unwrap() = true;
    Ok(())
}

#[tauri::command]
pub async fn send_chat(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    project_dir: String,
    message: String,
    history: Vec<UiMessage>,
) -> Result<ChatResponse, String> {
    // Reset cancel flag.
    *state.cancelled.lock().unwrap() = false;

    let settings = state.settings.lock().unwrap().clone();
    let use_planner = !settings.openrouter_api_key.is_empty();
    let schema = tools::tool_schema();

    let mut messages = build_initial_messages(&history, &message);

    let mut all_tool_calls: Vec<UiToolCall> = Vec::new();
    let mut all_tool_results: Vec<UiToolResult> = Vec::new();
    let mut final_assistant = String::new();

    for iteration in 0..MAX_ITERATIONS {
        // Check cancellation without holding the lock across await.
        let cancelled = { *state.cancelled.lock().unwrap() };
        if cancelled {
            break;
        }

        // Pick provider. Planner on the very first iteration if configured.
        let want_planner = use_planner && iteration == 0;
        info!(
            iter = iteration,
            want_planner, "calling AI"
        );

        let reply = if want_planner {
            match call_openrouter(&settings, &messages, &schema).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("planner failed, falling back to executor: {e}");
                    let _ = app.emit(
                        "ai:error",
                        json!({ "message": format!("planner failed, falling back: {e}") }),
                    );
                    call_ollama(&settings, &messages, &schema).await?
                }
            }
        } else {
            call_ollama(&settings, &messages, &schema).await?
        };

        let content = reply.content.clone().unwrap_or_default();
        let wire_tool_calls = reply.tool_calls.clone().unwrap_or_default();

        // Record the assistant turn into history, regardless of tool calls.
        messages.push(WireMessage::assistant(&content, reply.tool_calls.clone()));

        if wire_tool_calls.is_empty() {
            final_assistant = content;
            break;
        }

        // Execute each tool call, append tool messages, and emit events.
        for tc in &wire_tool_calls {
            let id = tc
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
            let args = tc.function.arguments.clone();
            let ui_call = UiToolCall {
                id: id.clone(),
                name: tc.function.name.clone(),
                args: args.clone(),
            };
            let _ = app.emit(
                "ai:tool_call",
                json!({ "id": ui_call.id, "name": ui_call.name, "args": ui_call.args }),
            );

            let (ok, output, diff) = match tools::execute(&project_dir, &tc.function.name, &args)
                .await
            {
                Ok((out, diff)) => (true, out, diff),
                Err(e) => (false, format!("error: {e}"), None),
            };
            let ui_result = UiToolResult {
                id: id.clone(),
                ok,
                output: output.clone(),
                diff: diff.clone(),
            };
            let _ = app.emit(
                "ai:tool_result",
                json!({
                    "id": ui_result.id,
                    "ok": ui_result.ok,
                    "output": ui_result.output,
                    "diff": ui_result.diff,
                }),
            );

            all_tool_calls.push(ui_call);
            all_tool_results.push(ui_result);

            // Feed the tool output back into the conversation.
            let tool_text = match &diff {
                Some(d) if !d.is_empty() => format!("{output}\n{d}"),
                _ => output,
            };
            messages.push(WireMessage::tool(&id, &tool_text));
        }
    }

    let _ = app.emit("ai:done", json!({ "assistant": final_assistant.clone() }));

    // Best-effort memory update. Failures are logged and ignored.
    if let Err(e) = update_memory(&project_dir, &message, &final_assistant, &all_tool_calls) {
        warn!("memory update failed: {e}");
    }

    Ok(ChatResponse {
        assistant: final_assistant,
        tool_calls: all_tool_calls,
        tool_results: all_tool_results,
    })
}

fn update_memory(
    project_dir: &str,
    user_message: &str,
    assistant: &str,
    tool_calls: &[UiToolCall],
) -> Result<(), String> {
    let path = std::path::Path::new(project_dir).join("PROJECT_MEMORY.json");
    let mut mem: Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !mem.is_object() {
        mem = json!({});
    }
    let obj = mem.as_object_mut().unwrap();

    // updated_at
    obj.insert(
        "updated_at".into(),
        Value::String(chrono_like_timestamp()),
    );

    // session.turns
    let session = obj
        .entry("session".to_string())
        .or_insert_with(|| json!({ "turns": [], "opened_project": null }));
    if !session.is_object() {
        *session = json!({ "turns": [] });
    }
    let sobj = session.as_object_mut().unwrap();
    sobj.insert(
        "opened_project".into(),
        Value::String(project_dir.to_string()),
    );
    let turns = sobj
        .entry("turns".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = turns.as_array_mut() {
        arr.push(json!({
            "user": user_message,
            "assistant": assistant,
            "tool_calls": tool_calls.iter().map(|t| &t.name).collect::<Vec<_>>(),
        }));
        // Keep only the last 50 turns to bound file size.
        let overflow = arr.len().saturating_sub(50);
        if overflow > 0 {
            arr.drain(..overflow);
        }
    }

    // tool_usage
    let tu = obj
        .entry("tool_usage".to_string())
        .or_insert_with(|| json!({}));
    if let Some(tu_obj) = tu.as_object_mut() {
        for tc in tool_calls {
            let entry = tu_obj
                .entry(tc.name.clone())
                .or_insert(Value::from(0u64));
            if let Some(n) = entry.as_u64() {
                *entry = Value::from(n + 1);
            } else {
                *entry = Value::from(1u64);
            }
        }
    }

    memory::save_memory_sync(project_dir, &mem)
}

fn chrono_like_timestamp() -> String {
    // Avoid pulling in `chrono` just for a timestamp.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}
