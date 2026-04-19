//! Hybrid AI layer with **three cooperating agent roles** and a real
//! **streaming tool-calling loop**:
//!
//! ```text
//!             ┌────────────┐  plan(text, maybe tool_calls)
//!   user  ───►│  Planner   │──────────┐
//!             │ OpenRouter │          │
//!             └────────────┘          │
//!                                     ▼
//!                            ┌────────────────┐ tool_call ──► Rust tool runtime
//!                            │    Executor    │ ─────────► emits ai:token events
//!                            │     Ollama     │◄── tool_result feeds back here
//!                            └────────────────┘
//!                                     │
//!                                     ▼
//!                            ┌────────────────┐
//!                            │    Reviewer    │  optionally asks for a fix
//!                            │  (planner or   │  → one corrective executor pass
//!                            │   executor)    │
//!                            └────────────────┘
//! ```
//!
//! Streaming: every provider call uses `stream: true`. Content tokens are
//! emitted as `ai:token { text, role }`, tool calls are accumulated on the
//! fly and executed after the stream completes, and the full final message
//! (`assistant` + any `tool_calls`) is appended to conversation history.
//!
//! Fallback: if the planner is unavailable or the request fails for any
//! reason, we transparently degrade to executor-only.
//!
//! Observability: each phase of the loop emits an `ai:step` event the UI
//! renders as a timeline. `ai:tool_call`, `ai:tool_result`, and `ai:error`
//! carry the agent role so the UI can badge them.

use std::collections::BTreeSet;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tracing::warn;

use crate::{memory, tools, AppState, Settings};

// Hard caps that protect the UI from runaway behavior even if the model
// gets stuck in a tool loop. `Settings::max_iterations` can tighten this
// but cannot raise it past `MAX_ITERATIONS_CEILING`.
const MAX_ITERATIONS_CEILING: usize = 16;
const MAX_REVIEWER_RETRIES: usize = 1;

const PLANNER_PROMPT: &str = r#"You are the PLANNER in a multi-agent coding assistant.

Your job is to read the user's request and produce a SHORT, CONCRETE plan
of the steps an executor agent should take. You may call tools (read_file,
list_dir) to explore the project if that helps you write a better plan.

Output format: 3–7 bullets, each bullet an imperative sentence. Never
invent files that do not exist — check with list_dir/read_file first.
Do NOT write to disk and do NOT run shell commands; leave that to the
executor.
"#;

const EXECUTOR_PROMPT: &str = r#"You are the EXECUTOR in a multi-agent coding assistant.

You have access to these OpenAI-style tools:
  - read_file(path)          -> read a text file, relative to the project root
  - write_file(path, content)-> write/overwrite a text file; returns a diff
  - list_dir(path)           -> list immediate children of a directory
  - run_cmd(cmd, timeout_ms) -> run a shell command inside the project root
                                (may be gated by a user-approval prompt)

Rules:
- Prefer reading before writing. When you write, include the FULL new file
  contents — never partial snippets.
- Paths are always relative to the opened project root. Never use absolute
  paths and never try to escape the root.
- Keep each response focused. Do not narrate every tool call; the UI shows
  them. After the last tool call, produce a short human summary of what you
  did.
- If you do not need any tools, just answer the user directly.
"#;

const REVIEWER_PROMPT: &str = r#"You are the REVIEWER in a multi-agent coding assistant.

You just watched the executor act on the user's request. Review the transcript
and answer in EXACTLY one of these two forms:

  OK: <one-sentence summary of what was accomplished>

or

  NEEDS_FIX: <one specific, actionable instruction for the executor>

Use NEEDS_FIX only for concrete problems you can point at (wrong file, missing
step, bug in generated code, command that clearly failed). If the executor's
work is acceptable, answer OK.
"#;

/// Which agent produced an event. Surfaced on every UI event for badging.
#[derive(Debug, Clone, Copy)]
enum Role {
    Planner,
    Executor,
    Reviewer,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Executor => "executor",
            Role::Reviewer => "reviewer",
        }
    }
}

// ---------- UI message format ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiToolResult {
    pub id: String,
    pub ok: bool,
    pub output: String,
    pub diff: Option<String>,
    #[serde(default)]
    pub role: String,
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
    /// Ordered list of agent steps that happened during this turn.
    pub steps: Vec<StepSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepSummary {
    pub index: u32,
    pub role: String,
    pub title: String,
    pub status: String, // "done" | "failed"
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

fn build_executor_messages(history: &[UiMessage], user: &str, plan: Option<&str>) -> Vec<WireMessage> {
    let mut msgs = Vec::with_capacity(history.len() + 3);
    msgs.push(WireMessage::system(EXECUTOR_PROMPT));
    if let Some(plan_text) = plan {
        if !plan_text.trim().is_empty() {
            msgs.push(WireMessage::system(&format!(
                "The planner produced this plan. Treat it as guidance, not a hard contract:\n\n{plan_text}"
            )));
        }
    }
    for m in history {
        match m.role.as_str() {
            "user" => msgs.push(WireMessage::user(&m.content)),
            "assistant" => msgs.push(WireMessage::assistant(&m.content, None)),
            "system" => msgs.push(WireMessage::system(&m.content)),
            _ => {}
        }
    }
    msgs.push(WireMessage::user(user));
    msgs
}

// ---------- Streaming aggregation ----------

#[derive(Default)]
struct StreamAccumulator {
    content: String,
    /// tool_calls keyed by index so provider-side deltas can update them
    /// incrementally. `BTreeSet<usize>` keeps emission order stable.
    tool_calls: Vec<WireToolCall>,
    tool_indices: BTreeSet<usize>,
}

impl StreamAccumulator {
    fn finalize(self) -> WireMessage {
        WireMessage::assistant(
            &self.content,
            if self.tool_calls.is_empty() {
                None
            } else {
                Some(self.tool_calls)
            },
        )
    }
}

fn merge_tool_call_delta(acc: &mut StreamAccumulator, idx: usize, delta: &Value) {
    while acc.tool_calls.len() <= idx {
        acc.tool_calls.push(WireToolCall {
            id: None,
            _type: Some("function".into()),
            function: WireToolCallFn {
                name: String::new(),
                arguments: Value::String(String::new()),
            },
        });
    }
    let entry = &mut acc.tool_calls[idx];
    if let Some(id) = delta.get("id").and_then(|v| v.as_str()) {
        entry.id = Some(id.into());
    }
    if let Some(func) = delta.get("function") {
        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
            if !name.is_empty() {
                entry.function.name = name.into();
            }
        }
        if let Some(args) = func.get("arguments") {
            match args {
                Value::String(s) => {
                    // OpenAI streams arguments as a string that concatenates.
                    if let Value::String(existing) = &mut entry.function.arguments {
                        existing.push_str(s);
                    } else {
                        entry.function.arguments = Value::String(s.clone());
                    }
                }
                other => {
                    // Ollama returns the full object at once.
                    entry.function.arguments = other.clone();
                }
            }
        }
    }
    acc.tool_indices.insert(idx);
}

fn finalize_tool_arguments(tcs: &mut [WireToolCall]) {
    for tc in tcs.iter_mut() {
        if let Value::String(s) = &tc.function.arguments {
            if s.is_empty() {
                tc.function.arguments = json!({});
            } else if let Ok(v) = serde_json::from_str::<Value>(s) {
                tc.function.arguments = v;
            }
        }
        if tc.id.is_none() {
            tc.id = Some(format!("call_{}", uuid::Uuid::new_v4().simple()));
        }
    }
}

// ---------- Provider calls (streaming) ----------

async fn stream_openrouter(
    app: &AppHandle,
    settings: &Settings,
    model_override: Option<&str>,
    messages: &[WireMessage],
    tools_schema: Option<&Value>,
    role: Role,
) -> Result<WireMessage, String> {
    if settings.openrouter_api_key.is_empty() {
        return Err("no OpenRouter API key".into());
    }
    let mut body = json!({
        "model": model_override.unwrap_or(&settings.openrouter_model),
        "messages": messages,
        "stream": true,
    });
    if let Some(schema) = tools_schema {
        body["tools"] = schema.clone();
        body["tool_choice"] = json!("auto");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&settings.openrouter_api_key)
        .header("HTTP-Referer", "https://github.com/salonadel6-sudo/open-claude-code-main")
        .header("X-Title", "Open Claude Code Desktop")
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("openrouter request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("openrouter http {status}: {text}"));
    }

    let mut acc = StreamAccumulator::default();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        // SSE frames are separated by a blank line.
        while let Some(idx) = buf.find("\n\n") {
            let frame = buf[..idx].to_string();
            buf.drain(..idx + 2);
            for line in frame.lines() {
                let line = line.trim();
                let payload = if let Some(rest) = line.strip_prefix("data:") {
                    rest.trim()
                } else {
                    continue;
                };
                if payload == "[DONE]" {
                    finalize_tool_arguments(&mut acc.tool_calls);
                    return Ok(acc.finalize());
                }
                let v: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(delta) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                {
                    if let Some(text) = delta.get("content").and_then(|s| s.as_str()) {
                        if !text.is_empty() {
                            acc.content.push_str(text);
                            let _ = app.emit(
                                "ai:token",
                                json!({ "text": text, "role": role.as_str() }),
                            );
                        }
                    }
                    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let idx = tc
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as usize;
                            merge_tool_call_delta(&mut acc, idx, tc);
                        }
                    }
                }
            }
        }
    }
    finalize_tool_arguments(&mut acc.tool_calls);
    Ok(acc.finalize())
}

async fn stream_ollama(
    app: &AppHandle,
    settings: &Settings,
    messages: &[WireMessage],
    tools_schema: Option<&Value>,
    role: Role,
) -> Result<WireMessage, String> {
    let url = format!("{}/api/chat", settings.ollama_base_url.trim_end_matches('/'));
    let mut body = json!({
        "model": settings.ollama_model,
        "messages": messages,
        "stream": true,
    });
    if let Some(schema) = tools_schema {
        body["tools"] = schema.clone();
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
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

    let mut acc = StreamAccumulator::default();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim().to_string();
            buf.drain(..idx + 1);
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(msg) = v.get("message") {
                if let Some(text) = msg.get("content").and_then(|s| s.as_str()) {
                    if !text.is_empty() {
                        acc.content.push_str(text);
                        let _ = app.emit(
                            "ai:token",
                            json!({ "text": text, "role": role.as_str() }),
                        );
                    }
                }
                if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for (i, tc) in tcs.iter().enumerate() {
                        merge_tool_call_delta(&mut acc, i, tc);
                    }
                }
            }
            if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                finalize_tool_arguments(&mut acc.tool_calls);
                return Ok(acc.finalize());
            }
        }
    }
    finalize_tool_arguments(&mut acc.tool_calls);
    Ok(acc.finalize())
}

/// Attempt the planner first; fall back to Ollama on any error.
async fn call_executor_with_fallback(
    app: &AppHandle,
    settings: &Settings,
    messages: &[WireMessage],
    tools_schema: &Value,
    role: Role,
) -> Result<WireMessage, String> {
    // Executor path is always Ollama for now.
    stream_ollama(app, settings, messages, Some(tools_schema), role).await
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
    run_chat_turn(app, &state, project_dir, message, history).await
}

/// Runs a single multi-agent chat turn. Reusable by the higher-level
/// autonomous controller (`controller::start_goal`), which does not own
/// a `tauri::State` handle but does hold `&AppState`.
pub(crate) async fn run_chat_turn(
    app: AppHandle,
    state: &AppState,
    project_dir: String,
    message: String,
    history: Vec<UiMessage>,
) -> Result<ChatResponse, String> {
    *state.cancelled.lock().unwrap() = false;

    let settings = state.settings.lock().unwrap().clone();
    let use_planner = !settings.openrouter_api_key.is_empty();
    let max_iterations = (settings.max_iterations as usize).min(MAX_ITERATIONS_CEILING);
    let schema = tools::tool_schema();

    let mut all_tool_calls: Vec<UiToolCall> = Vec::new();
    let mut all_tool_results: Vec<UiToolResult> = Vec::new();
    let mut touched_files: Vec<String> = Vec::new();
    let mut steps: Vec<StepSummary> = Vec::new();

    // ---- Phase 1: Planner ----
    let plan_text: Option<String> = if use_planner {
        emit_step(&app, &mut steps, Role::Planner, "planning", "running");
        match stream_openrouter(&app, &settings, None, &planner_messages(&history, &message), None, Role::Planner).await {
            Ok(msg) => {
                let text = msg.content.clone().unwrap_or_default();
                finish_step(&app, &mut steps, "done", Some(&first_line(&text)));
                if text.trim().is_empty() { None } else { Some(text) }
            }
            Err(e) => {
                warn!("planner failed: {e}");
                let _ = app.emit(
                    "ai:error",
                    json!({ "message": format!("planner failed, falling back to executor-only: {e}"), "role": "planner" }),
                );
                finish_step(&app, &mut steps, "failed", Some(&truncate(&e, 120)));
                None
            }
        }
    } else {
        None
    };

    // ---- Phase 2: Executor tool loop ----
    let mut messages = build_executor_messages(&history, &message, plan_text.as_deref());
    let mut final_assistant = String::new();
    let mut executor_iterations = 0usize;
    let mut reviewer_retries_left = if settings.reviewer_enabled {
        MAX_REVIEWER_RETRIES
    } else {
        0
    };

    'outer: loop {
        for iteration in 0..max_iterations {
            executor_iterations += 1;
            if *state.cancelled.lock().unwrap() {
                break 'outer;
            }
            emit_step(
                &app,
                &mut steps,
                Role::Executor,
                &format!("executor step {}", iteration + 1),
                "running",
            );
            let reply = match call_executor_with_fallback(&app, &settings, &messages, &schema, Role::Executor).await {
                Ok(m) => m,
                Err(e) => {
                    finish_step(&app, &mut steps, "failed", Some(&truncate(&e, 120)));
                    let _ = app.emit("ai:error", json!({ "message": e, "role": "executor" }));
                    return Err(e);
                }
            };

            let content = reply.content.clone().unwrap_or_default();
            let wire_tool_calls = reply.tool_calls.clone().unwrap_or_default();
            messages.push(WireMessage::assistant(&content, reply.tool_calls.clone()));

            if wire_tool_calls.is_empty() {
                final_assistant = content;
                finish_step(
                    &app,
                    &mut steps,
                    "done",
                    Some(&first_line(&final_assistant)),
                );
                break;
            }
            finish_step(
                &app,
                &mut steps,
                "done",
                Some(&format!(
                    "{} tool call{}",
                    wire_tool_calls.len(),
                    if wire_tool_calls.len() == 1 { "" } else { "s" }
                )),
            );

            // Execute each tool call.
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
                    role: Role::Executor.as_str().into(),
                };
                let _ = app.emit(
                    "ai:tool_call",
                    json!({
                        "id": ui_call.id,
                        "name": ui_call.name,
                        "args": ui_call.args,
                        "role": ui_call.role,
                    }),
                );

                let exec_result = match tc.function.name.as_str() {
                    "run_cmd" => {
                        tools::execute_run_cmd_gated(&app, state, &project_dir, &args).await
                    }
                    other => tools::execute_safe(&project_dir, other, &args).await,
                };
                let (ok, output, diff, effect) = match exec_result {
                    Ok((o, d, eff)) => (true, o, d, eff),
                    Err(e) => (false, format!("error: {e}"), None, Default::default()),
                };
                for p in effect.touched_files {
                    if !p.is_empty() && !touched_files.contains(&p) {
                        touched_files.push(p);
                    }
                }
                let ui_result = UiToolResult {
                    id: id.clone(),
                    ok,
                    output: output.clone(),
                    diff: diff.clone(),
                    role: Role::Executor.as_str().into(),
                };
                let _ = app.emit(
                    "ai:tool_result",
                    json!({
                        "id": ui_result.id,
                        "ok": ui_result.ok,
                        "output": ui_result.output,
                        "diff": ui_result.diff,
                        "role": ui_result.role,
                    }),
                );
                all_tool_calls.push(ui_call);
                all_tool_results.push(ui_result);

                let tool_text = match &diff {
                    Some(d) if !d.is_empty() => format!("{output}\n{d}"),
                    _ => output,
                };
                messages.push(WireMessage::tool(&id, &tool_text));
            }
        }

        // ---- Phase 3: Reviewer (optional, at most one corrective retry) ----
        if !settings.reviewer_enabled || final_assistant.is_empty() || reviewer_retries_left == 0 {
            break 'outer;
        }
        emit_step(&app, &mut steps, Role::Reviewer, "reviewing", "running");
        let review_messages = reviewer_messages(&message, &final_assistant, &all_tool_calls);
        // Reviewer prefers the planner (OpenRouter) if available, else executor.
        let review_result = if use_planner {
            stream_openrouter(&app, &settings, None, &review_messages, None, Role::Reviewer).await
        } else {
            stream_ollama(&app, &settings, &review_messages, None, Role::Reviewer).await
        };
        let review_text = match review_result {
            Ok(m) => m.content.clone().unwrap_or_default(),
            Err(e) => {
                finish_step(&app, &mut steps, "failed", Some(&truncate(&e, 120)));
                let _ = app.emit("ai:error", json!({ "message": e, "role": "reviewer" }));
                break 'outer;
            }
        };
        let verdict = parse_review_verdict(&review_text);
        match verdict {
            ReviewVerdict::Ok(summary) => {
                finish_step(&app, &mut steps, "done", Some(&format!("OK: {summary}")));
                break 'outer;
            }
            ReviewVerdict::NeedsFix(instruction) => {
                finish_step(
                    &app,
                    &mut steps,
                    "done",
                    Some(&format!("NEEDS_FIX: {}", truncate(&instruction, 120))),
                );
                reviewer_retries_left -= 1;
                // Feed the reviewer's critique back into the executor loop.
                messages.push(WireMessage::user(&format!(
                    "Reviewer feedback: {instruction}\n\nAddress this and then stop."
                )));
                final_assistant.clear();
                // Continue the outer loop → another executor pass.
                continue 'outer;
            }
            ReviewVerdict::Unknown => {
                finish_step(&app, &mut steps, "done", Some("review skipped (unparsed)"));
                break 'outer;
            }
        }
    }

    let _ = app.emit(
        "ai:done",
        json!({
            "assistant": final_assistant.clone(),
            "iterations": executor_iterations,
        }),
    );

    if let Err(e) = memory::update_turn_memory(
        &project_dir,
        &message,
        &final_assistant,
        &all_tool_calls,
        &touched_files,
        plan_text.as_deref(),
    ) {
        warn!("memory update failed: {e}");
    }

    Ok(ChatResponse {
        assistant: final_assistant,
        tool_calls: all_tool_calls,
        tool_results: all_tool_results,
        steps,
    })
}

// ---------- Helpers ----------

fn planner_messages(history: &[UiMessage], user: &str) -> Vec<WireMessage> {
    let mut msgs = Vec::with_capacity(history.len() + 2);
    msgs.push(WireMessage::system(PLANNER_PROMPT));
    for m in history {
        match m.role.as_str() {
            "user" => msgs.push(WireMessage::user(&m.content)),
            "assistant" => msgs.push(WireMessage::assistant(&m.content, None)),
            _ => {}
        }
    }
    msgs.push(WireMessage::user(user));
    msgs
}

fn reviewer_messages(user: &str, assistant: &str, calls: &[UiToolCall]) -> Vec<WireMessage> {
    let tool_summary = if calls.is_empty() {
        "(no tools were called)".to_string()
    } else {
        calls
            .iter()
            .map(|c| format!("- {} {}", c.name, args_preview(&c.args)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let transcript = format!(
        "User request:\n{user}\n\nExecutor tool calls:\n{tool_summary}\n\nExecutor summary:\n{assistant}\n"
    );
    vec![
        WireMessage::system(REVIEWER_PROMPT),
        WireMessage::user(&transcript),
    ]
}

fn args_preview(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    truncate(&s, 120)
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn first_line(s: &str) -> String {
    let first = s.trim().lines().next().unwrap_or("").trim();
    truncate(first, 120)
}

enum ReviewVerdict {
    Ok(String),
    NeedsFix(String),
    Unknown,
}

fn parse_review_verdict(text: &str) -> ReviewVerdict {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("OK:").or_else(|| trimmed.strip_prefix("OK :")) {
        return ReviewVerdict::Ok(rest.trim().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("NEEDS_FIX:") {
        return ReviewVerdict::NeedsFix(rest.trim().to_string());
    }
    // Some models prefix with markdown/extra prose; search instead.
    let upper = trimmed.to_uppercase();
    if let Some(pos) = upper.find("NEEDS_FIX:") {
        return ReviewVerdict::NeedsFix(trimmed[pos + "NEEDS_FIX:".len()..].trim().to_string());
    }
    if let Some(pos) = upper.find("OK:") {
        return ReviewVerdict::Ok(trimmed[pos + "OK:".len()..].trim().to_string());
    }
    ReviewVerdict::Unknown
}

fn emit_step(app: &AppHandle, steps: &mut Vec<StepSummary>, role: Role, title: &str, status: &str) {
    let index = steps.len() as u32;
    let step = StepSummary {
        index,
        role: role.as_str().into(),
        title: title.into(),
        status: status.into(),
    };
    let _ = app.emit("ai:step", json!(step));
    steps.push(step);
}

/// Finish the most recently emitted step in-place and re-emit it so the UI
/// can update its timeline without bookkeeping indices itself.
fn finish_step(
    app: &AppHandle,
    steps: &mut [StepSummary],
    status: &str,
    append_to_title: Option<&str>,
) {
    if let Some(last) = steps.last_mut() {
        last.status = status.into();
        if let Some(extra) = append_to_title {
            if !extra.is_empty() {
                last.title = format!("{} — {}", last.title, extra);
            }
        }
        let _ = app.emit("ai:step", json!(last));
    }
}

// Silence the unused-timeout lint from reqwest when building for wasm etc.
#[allow(dead_code)]
fn _keep_duration_in_scope() -> Duration {
    Duration::from_secs(0)
}
