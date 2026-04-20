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

use crate::cancel::CancelToken;
use crate::settings::ProviderMode;
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

Language: respond in the SAME natural language as the user's most
recent message. Do not switch languages mid-response.
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
- Never assume a language, framework, or file exists. If a PROJECT
  CONTEXT message is present, trust those facts over any prior
  assumption; otherwise verify with list_dir / read_file before acting.
- Do not invent hypothetical files or "mentally review" imaginary
  reports — only work with files you actually read via tools.

Language: respond in the SAME natural language as the user's most
recent message. Do not switch languages mid-response.
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

Critical: when a PROJECT CONTEXT message lists the detected languages,
entry points, and configs, never ask the executor to look at files or
languages that are not in that context (e.g. asking for Python files in
a TypeScript project). Ground every NEEDS_FIX in the context you were given.

Language: respond in the SAME natural language as the user's most
recent message.
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

/// Which backend actually serves a role for a single call. A `Role` is
/// mapped to a primary `Provider` (and optional fallback) by
/// [`resolve_provider`], which consults [`ProviderMode`] on `Settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    OpenRouter,
    Ollama,
}

impl Provider {
    fn as_str(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::Ollama => "ollama",
        }
    }
}

/// Pick the primary provider (and optional fallback) for a role given the
/// configured `ProviderMode`:
///
/// - `Cloud`   → `(OpenRouter, None)` for every role. No fallback.
/// - `Local`   → `(Ollama, None)` for every role. No fallback.
/// - `Hybrid`  → planner + reviewer prefer OpenRouter with Ollama as
///   fallback (reasoning quality matters more than cost), executor
///   prefers Ollama with OpenRouter as fallback (tool throughput matters
///   more than reasoning quality).
fn resolve_provider(settings: &Settings, role: Role) -> (Provider, Option<Provider>) {
    match settings.provider_mode {
        ProviderMode::Cloud => (Provider::OpenRouter, None),
        ProviderMode::Local => (Provider::Ollama, None),
        ProviderMode::Hybrid => match role {
            Role::Planner | Role::Reviewer => {
                (Provider::OpenRouter, Some(Provider::Ollama))
            }
            Role::Executor => (Provider::Ollama, Some(Provider::OpenRouter)),
        },
    }
}

/// Per-role model override. When the role-specific slot is non-empty it
/// is used verbatim; otherwise the dispatcher falls back to the
/// provider's default model (`openrouter_model` / `ollama_model`). This
/// lets a user run, e.g., a cheap router for the planner and a beefier
/// one for the reviewer without touching the executor model.
fn model_for_role(settings: &Settings, role: Role, provider: Provider) -> String {
    let per_role = match role {
        Role::Planner => &settings.planner_model,
        Role::Executor => &settings.executor_model,
        Role::Reviewer => &settings.reviewer_model,
    };
    if !per_role.trim().is_empty() {
        return per_role.clone();
    }
    match provider {
        Provider::OpenRouter => settings.openrouter_model.clone(),
        Provider::Ollama => settings.ollama_model.clone(),
    }
}

/// Cheap guard used by the fallback path so we don't try a second
/// provider that is clearly not configured (e.g. OpenRouter without an
/// API key). Ollama has no auth so it is always "configured"; if it is
/// actually unreachable the request will fail loudly.
fn provider_has_credentials(settings: &Settings, provider: Provider) -> bool {
    match provider {
        Provider::OpenRouter => !settings.openrouter_api_key.trim().is_empty(),
        Provider::Ollama => true,
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
    /// Full execution transcript for this turn. For a chat-initiated
    /// turn this is discarded; the controller attaches it to the active
    /// task when driving the autonomous loop.
    #[serde(default, skip_serializing_if = "crate::trace::TaskTrace::is_empty")]
    pub trace: crate::trace::TaskTrace,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepSummary {
    pub index: u32,
    pub role: String,
    pub title: String,
    pub status: String, // "done" | "failed"
    /// Provider actually routed for this step (`"openrouter"` or
    /// `"ollama"`). Set at emit time based on `resolve_provider` for
    /// the role; left out of the payload when the step is not tied to
    /// a model call. The UI uses this to badge timeline entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Concrete model identifier sent on the wire (e.g.
    /// `"openrouter/auto"` or `"deepseek-coder:6.7b"`). Same lifecycle
    /// as `provider` — only present on model-call steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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

/// Maximum number of recent user/assistant history turns to keep when
/// building executor messages. System messages are always preserved.
/// This prevents context-window overflow on long sessions while keeping
/// the most recent conversation context available to the executor.
const MAX_HISTORY_TURNS: usize = 20;

fn build_executor_messages(
    history: &[UiMessage],
    user: &str,
    plan: Option<&str>,
    project_ctx: Option<&str>,
) -> Vec<WireMessage> {
    let mut msgs = Vec::with_capacity(history.len() + 4);
    msgs.push(WireMessage::system(EXECUTOR_PROMPT));
    if let Some(ctx) = project_ctx {
        if !ctx.trim().is_empty() {
            msgs.push(WireMessage::system(ctx));
        }
    }
    if let Some(plan_text) = plan {
        if !plan_text.trim().is_empty() {
            msgs.push(WireMessage::system(&format!(
                "The planner produced this plan. Treat it as guidance, not a hard contract:\n\n{plan_text}"
            )));
        }
    }
    // Sliding window: keep system messages (already added above) plus
    // the last MAX_HISTORY_TURNS user/assistant messages. This prevents
    // context-window overflow on long sessions (50+ messages) while
    // preserving the most recent conversation context.
    let non_system: Vec<&UiMessage> = history
        .iter()
        .filter(|m| m.role != "system")
        .collect();
    let skip = non_system.len().saturating_sub(MAX_HISTORY_TURNS);
    // Always include system messages from history.
    for m in history.iter().filter(|m| m.role == "system") {
        msgs.push(WireMessage::system(&m.content));
    }
    for m in non_system.into_iter().skip(skip) {
        match m.role.as_str() {
            "user" => msgs.push(WireMessage::user(&m.content)),
            "assistant" => msgs.push(WireMessage::assistant(&m.content, None)),
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
    json_mode: bool,
    cancel: &CancelToken,
) -> Result<WireMessage, String> {
    if cancel.is_cancelled() {
        return Err(cancel.err_string());
    }
    if settings.openrouter_api_key.is_empty() {
        return Err("no OpenRouter API key".into());
    }
    let mut body = json!({
        "model": model_override.unwrap_or(&settings.openrouter_model),
        "messages": messages,
        "stream": true,
    });
    // `json_mode` forces the model to return a valid JSON object. It is
    // intended for the goal planner (see `plan_goal`), where the
    // downstream code does strict `serde_json` parsing. When json mode
    // is on we skip tool schemas entirely: not every OpenRouter model
    // accepts `response_format` + `tools` simultaneously, and the goal
    // planner is pure text-in / JSON-out — no tools are needed.
    if json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    } else if let Some(schema) = tools_schema {
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
    loop {
        // Race the next chunk against cancellation so we abort mid-stream,
        // not just between frames. Dropping `resp` on cancel closes the
        // underlying TCP connection so we don't keep billing tokens we'll
        // never emit.
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(stream);
                return Err(cancel.err_string());
            }
            c = stream.next() => c,
        };
        let Some(chunk) = chunk else { break };
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
    model_override: Option<&str>,
    messages: &[WireMessage],
    tools_schema: Option<&Value>,
    role: Role,
    json_mode: bool,
    cancel: &CancelToken,
) -> Result<WireMessage, String> {
    if cancel.is_cancelled() {
        return Err(cancel.err_string());
    }
    let url = format!("{}/api/chat", settings.ollama_base_url.trim_end_matches('/'));
    let mut body = json!({
        "model": model_override.unwrap_or(&settings.ollama_model),
        "messages": messages,
        "stream": true,
    });
    // Ollama's native JSON-output constraint. Same semantics as
    // OpenRouter's `response_format: json_object` — forces a valid JSON
    // object on output. We also drop the tool schema when json mode is
    // on, both to match OpenRouter's behaviour and because the goal
    // planner does not need tools.
    if json_mode {
        body["format"] = json!("json");
    } else if let Some(schema) = tools_schema {
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
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(stream);
                return Err(cancel.err_string());
            }
            c = stream.next() => c,
        };
        let Some(chunk) = chunk else { break };
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

/// Dispatch a single model call for an agent `role`, routing through
/// the provider matrix configured on `Settings` (see
/// [`resolve_provider`]).
///
/// Contract:
/// 1. Resolve `(primary, fallback)` for the role.
/// 2. Call the primary provider with the role's resolved model (the
///    per-role override when set, else the provider default).
/// 3. On primary failure, emit `ai:error` with `{provider, model,
///    role}` so the UI can surface the provider swap, then — if a
///    fallback exists and has credentials — retry on the fallback with
///    its own resolved model.
/// 4. If both fail, return a combined error string mentioning both
///    attempts. If the fallback was skipped (no credentials), return
///    the primary error verbatim.
///
/// This replaces the legacy `call_executor_with_fallback` (which was
/// Ollama-first regardless of mode). Every site that previously called
/// `stream_openrouter` / `stream_ollama` directly should go through
/// here so provider routing is consistent across planner, executor,
/// and reviewer.
async fn call_model(
    app: &AppHandle,
    settings: &Settings,
    role: Role,
    messages: &[WireMessage],
    tools_schema: Option<&Value>,
    json_mode: bool,
    cancel: &CancelToken,
) -> Result<WireMessage, String> {
    let (primary, fallback) = resolve_provider(settings, role);
    let primary_model = model_for_role(settings, role, primary);
    match call_provider(
        app,
        settings,
        primary,
        &primary_model,
        role,
        messages,
        tools_schema,
        json_mode,
        cancel,
    )
    .await
    {
        Ok(msg) => Ok(msg),
        Err(primary_err) => {
            // A user-initiated cancel is not a failure we should mask
            // by retrying on the fallback provider — bubble it up so
            // the turn unwinds immediately.
            if cancel.is_cancelled() {
                return Err(primary_err);
            }
            let Some(fb) = fallback else {
                return Err(primary_err);
            };
            if !provider_has_credentials(settings, fb) {
                return Err(primary_err);
            }
            let fb_model = model_for_role(settings, role, fb);
            warn!(
                "{} on {} failed, falling back to {}: {}",
                role.as_str(),
                primary.as_str(),
                fb.as_str(),
                primary_err
            );
            let _ = app.emit(
                "ai:error",
                json!({
                    "message": format!(
                        "{} failed ({}), trying {}…",
                        primary.as_str(),
                        primary_err,
                        fb.as_str(),
                    ),
                    "role": role.as_str(),
                    "provider": primary.as_str(),
                    "model": primary_model,
                    "fallback_provider": fb.as_str(),
                    "fallback_model": fb_model,
                }),
            );
            call_provider(
                app,
                settings,
                fb,
                &fb_model,
                role,
                messages,
                tools_schema,
                json_mode,
                cancel,
            )
            .await
            .map_err(|fb_err| {
                format!(
                    "both providers failed — {}: {}; {}: {}",
                    primary.as_str(),
                    primary_err,
                    fb.as_str(),
                    fb_err,
                )
            })
        }
    }
}

/// Thin wrapper that routes to the concrete `stream_*` implementation
/// for a given provider. Kept as a separate function so both the
/// primary and the fallback branches of [`call_model`] go through
/// identical plumbing.
async fn call_provider(
    app: &AppHandle,
    settings: &Settings,
    provider: Provider,
    model: &str,
    role: Role,
    messages: &[WireMessage],
    tools_schema: Option<&Value>,
    json_mode: bool,
    cancel: &CancelToken,
) -> Result<WireMessage, String> {
    match provider {
        Provider::OpenRouter => {
            stream_openrouter(
                app,
                settings,
                Some(model),
                messages,
                tools_schema,
                role,
                json_mode,
                cancel,
            )
            .await
        }
        Provider::Ollama => {
            stream_ollama(
                app,
                settings,
                Some(model),
                messages,
                tools_schema,
                role,
                json_mode,
                cancel,
            )
            .await
        }
    }
}

// ---------- Top-level commands ----------

#[tauri::command]
pub async fn check_planner(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let key = state.settings.read().unwrap().openrouter_api_key.clone();
    Ok(!key.is_empty())
}

#[tauri::command]
pub async fn check_executor(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let base = state.settings.read().unwrap().ollama_base_url.clone();
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    // 10s (up from 3s): the first probe after launching `ollama serve`
    // can stall for several seconds while the daemon warms up its
    // model index, and a remote/LAN Ollama easily loses the race on a
    // 3s budget even when it is perfectly healthy.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    match client.get(&url).send().await {
        Ok(r) => Ok(r.status().is_success()),
        Err(_) => Ok(false),
    }
}

/// Detailed connection probe for the Ollama executor. Takes the form-level
/// `base_url` and `model` directly so Settings can test unsaved values
/// without round-tripping through save. `model` is optional; when present,
/// the response's `model_available` field reports whether an exact match
/// for that model tag is returned by `/api/tags`. `available_models` is
/// the (possibly truncated) list of tags Ollama reports, for friendlier
/// error messages in the UI.
#[derive(serde::Serialize)]
pub struct OllamaProbeResult {
    pub reachable: bool,
    pub model_available: bool,
    pub error: Option<String>,
    pub available_models: Vec<String>,
}

#[tauri::command]
pub async fn probe_ollama(
    base_url: String,
    model: Option<String>,
) -> Result<OllamaProbeResult, String> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    // See `check_executor` — 10s accommodates cold-start and remote
    // Ollama daemons that briefly block the first `/api/tags` call.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(OllamaProbeResult {
                reachable: false,
                model_available: false,
                error: Some(e.to_string()),
                available_models: Vec::new(),
            });
        }
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(OllamaProbeResult {
                reachable: false,
                model_available: false,
                error: Some(format!(
                    "cannot reach {}: {}",
                    url,
                    e
                )),
                available_models: Vec::new(),
            });
        }
    };
    if !resp.status().is_success() {
        return Ok(OllamaProbeResult {
            reachable: false,
            model_available: false,
            error: Some(format!(
                "{} returned {}",
                url,
                resp.status()
            )),
            available_models: Vec::new(),
        });
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Ok(OllamaProbeResult {
                reachable: true,
                model_available: false,
                error: Some(format!("could not parse /api/tags: {}", e)),
                available_models: Vec::new(),
            });
        }
    };
    let names: Vec<String> = body
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let model_available = match model.as_deref() {
        Some(want) if !want.is_empty() => {
            // Ollama's /api/tags always returns names with an explicit tag
            // (e.g. `qwen2.5:latest`), but users typically enter the bare
            // name (`qwen2.5`) — and /api/chat itself resolves the implicit
            // `:latest` tag. Normalise both forms so we don't falsely warn
            // "model not pulled" when the model works.
            let want_normalised = if want.contains(':') {
                want.to_string()
            } else {
                format!("{}:latest", want)
            };
            names
                .iter()
                .any(|n| n == want || n == &want_normalised)
        }
        _ => true,
    };
    Ok(OllamaProbeResult {
        reachable: true,
        model_available,
        error: None,
        available_models: names,
    })
}

/// Detailed connection probe for the OpenRouter planner/reviewer.
/// Mirrors [`probe_ollama`]: it takes the form-level `api_key` and
/// `model` so Settings can test unsaved values, and it answers every
/// reachability / auth / model-availability question in one round-trip
/// so the UI can render a single line of feedback.
///
/// OpenRouter exposes `/api/v1/models` without authentication, so
/// `reachable` is answered from that. `/api/v1/auth/key` validates the
/// API key; a successful response populates `key_valid` and (when the
/// API includes it) a rough `credits_remaining` dollar amount. When a
/// `model` is provided, it is matched against the `/api/v1/models`
/// listing so the UI can warn before the user pays for a failed chat.
#[derive(serde::Serialize)]
pub struct OpenRouterProbeResult {
    pub reachable: bool,
    pub key_valid: bool,
    pub model_available: bool,
    pub error: Option<String>,
    pub available_models: Vec<String>,
    /// Remaining credits in USD, as reported by `/api/v1/auth/key`.
    /// `None` when the endpoint did not return a credit balance.
    pub credits_remaining: Option<f64>,
}

#[tauri::command]
pub async fn probe_openrouter(
    api_key: String,
    model: Option<String>,
) -> Result<OpenRouterProbeResult, String> {
    // 10s matches `probe_ollama` — cold CDN edges and slow DNS can
    // briefly exceed a 3s budget even when OpenRouter is healthy.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(OpenRouterProbeResult {
                reachable: false,
                key_valid: false,
                model_available: false,
                error: Some(e.to_string()),
                available_models: Vec::new(),
                credits_remaining: None,
            });
        }
    };

    // 1) Reachability + model catalog — works without auth.
    let models_resp = match client
        .get("https://openrouter.ai/api/v1/models")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(OpenRouterProbeResult {
                reachable: false,
                key_valid: false,
                model_available: false,
                error: Some(format!("cannot reach openrouter.ai: {}", e)),
                available_models: Vec::new(),
                credits_remaining: None,
            });
        }
    };
    if !models_resp.status().is_success() {
        return Ok(OpenRouterProbeResult {
            reachable: false,
            key_valid: false,
            model_available: false,
            error: Some(format!(
                "openrouter /api/v1/models returned {}",
                models_resp.status()
            )),
            available_models: Vec::new(),
            credits_remaining: None,
        });
    }
    let models_body: Value = match models_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Ok(OpenRouterProbeResult {
                reachable: true,
                key_valid: false,
                model_available: false,
                error: Some(format!("could not parse /api/v1/models: {}", e)),
                available_models: Vec::new(),
                credits_remaining: None,
            });
        }
    };
    let names: Vec<String> = models_body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let model_available = match model.as_deref() {
        Some(want) if !want.is_empty() => {
            // `openrouter/auto` is a special router — it is always
            // accepted by the API even though it isn't listed in the
            // models catalog.
            if want == "openrouter/auto" {
                true
            } else {
                names.iter().any(|n| n == want)
            }
        }
        _ => true,
    };

    // 2) Key validity — requires auth. Skip if empty key (common on
    // first run); report `key_valid = false` rather than an error.
    if api_key.trim().is_empty() {
        return Ok(OpenRouterProbeResult {
            reachable: true,
            key_valid: false,
            model_available,
            error: Some("no API key provided".to_string()),
            available_models: names,
            credits_remaining: None,
        });
    }
    let auth_resp = match client
        .get("https://openrouter.ai/api/v1/auth/key")
        .bearer_auth(&api_key)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(OpenRouterProbeResult {
                reachable: true,
                key_valid: false,
                model_available,
                error: Some(format!("auth check failed: {}", e)),
                available_models: names,
                credits_remaining: None,
            });
        }
    };
    if !auth_resp.status().is_success() {
        let status = auth_resp.status();
        return Ok(OpenRouterProbeResult {
            reachable: true,
            key_valid: false,
            model_available,
            error: Some(format!("API key rejected (HTTP {})", status)),
            available_models: names,
            credits_remaining: None,
        });
    }
    let auth_body: Value = auth_resp.json().await.unwrap_or(Value::Null);
    // OpenRouter reports remaining credits as `limit_remaining` or
    // `limit - usage` under `data`; we try the obvious keys and
    // silently skip when the shape changes.
    let credits_remaining = auth_body
        .get("data")
        .and_then(|d| {
            d.get("limit_remaining")
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    let limit = d.get("limit").and_then(|v| v.as_f64());
                    let usage = d.get("usage").and_then(|v| v.as_f64());
                    match (limit, usage) {
                        (Some(l), Some(u)) => Some(l - u),
                        _ => None,
                    }
                })
        });
    Ok(OpenRouterProbeResult {
        reachable: true,
        key_valid: true,
        model_available,
        error: None,
        available_models: names,
        credits_remaining,
    })
}

#[tauri::command]
pub fn cancel_chat(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Trip the cooperative cancel token. Wakes every `cancelled()` awaiter
    // and makes subsequent `is_cancelled()` checks observe the cancel
    // synchronously. The in-flight SSE stream and any running `run_cmd`
    // child process will be torn down on the next `select!` poll.
    state.cancelled.cancel();
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
    // Chat-driven turns never force autonomous confirms — the user is
    // already in the loop by construction, and the existing confirm
    // modal handles unfamiliar `run_cmd`s via `cmd_confirm_required`.
    // `json_mode = false` — normal chat is free-form prose; only
    // `plan_goal` opts into strict JSON output.
    run_chat_turn(app, &state, project_dir, message, history, false, false).await
}

/// A chat turn's cancel scope. Run-time callers (`send_chat`,
/// controller) hand us a token to watch; we propagate it down to every
/// SSE read and every `run_cmd` invocation so cancel takes effect
/// mid-operation, not just between iterations.
fn turn_cancel_token(state: &AppState) -> CancelToken {
    // Callers reset the token at turn start; nothing else to do here.
    state.cancelled.clone()
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
    autonomous_confirm: bool,
    // When true, every model call in this turn is placed in
    // JSON-output mode (`response_format: {type: "json_object"}` on
    // OpenRouter, `format: "json"` on Ollama). Used by the goal
    // planner (`controller::plan_goal`) so small local models cannot
    // wrap the task list in prose/markdown. Off for normal chat.
    json_mode: bool,
) -> Result<ChatResponse, String> {
    // Re-arm the per-turn cancel token. A prior `cancel_chat` press must
    // not poison the next turn; the controller is responsible for not
    // resetting when it wants cancellation to persist across tasks.
    state.cancelled.reset();
    let cancel = turn_cancel_token(state);

    let settings = state.settings.read().unwrap().clone();
    // The planner runs when its resolved provider chain has credentials.
    // In `Local` mode this is always true (Ollama needs no auth); in
    // `Cloud` mode it requires an OpenRouter key; in `Hybrid` mode we
    // accept the planner as long as *either* OpenRouter or its Ollama
    // fallback is reachable (the dispatcher will pick the one that
    // works per call via [`call_model`]).
    let use_planner = {
        let (primary, fallback) = resolve_provider(&settings, Role::Planner);
        provider_has_credentials(&settings, primary)
            || fallback
                .map(|p| provider_has_credentials(&settings, p))
                .unwrap_or(false)
    };
    let max_iterations = (settings.max_iterations as usize).min(MAX_ITERATIONS_CEILING);
    let schema = tools::tool_schema();

    // Load the persisted project map once per turn and pass it to every
    // agent role as a second system message. Keeps the planner / executor /
    // reviewer anchored to real detected facts (languages, entry points,
    // deps) instead of hallucinating Python files on a React project.
    let project_ctx: Option<String> =
        crate::project_scan::project_context_summary(&project_dir);
    let project_ctx_ref: Option<&str> = project_ctx.as_deref();

    let mut all_tool_calls: Vec<UiToolCall> = Vec::new();
    let mut all_tool_results: Vec<UiToolResult> = Vec::new();
    let mut touched_files: Vec<String> = Vec::new();
    let mut steps: Vec<StepSummary> = Vec::new();
    let mut trace = crate::trace::TaskTrace::new();
    trace.push_user(&message, crate::tasks::unix_ts());

    // ---- Phase 1: Planner ----
    let plan_text: Option<String> = if use_planner {
        let (p_provider, _) = resolve_provider(&settings, Role::Planner);
        let p_model = model_for_role(&settings, Role::Planner, p_provider);
        emit_step(
            &app,
            &mut steps,
            Role::Planner,
            "planning",
            "running",
            Some((p_provider, &p_model)),
        );
        match call_model(
            &app,
            &settings,
            Role::Planner,
            &planner_messages(&history, &message, project_ctx_ref),
            None,
            json_mode,
            &cancel,
        )
        .await
        {
            Ok(msg) => {
                let text = msg.content.clone().unwrap_or_default();
                finish_step(&app, &mut steps, "done", Some(&first_line(&text)));
                if text.trim().is_empty() {
                    None
                } else {
                    trace.push_plan(&text, crate::tasks::unix_ts());
                    Some(text)
                }
            }
            Err(e) => {
                warn!("planner failed: {e}");
                let _ = app.emit(
                    "ai:error",
                    json!({ "message": format!("planner failed, falling back to executor-only: {e}"), "role": "planner" }),
                );
                finish_step(&app, &mut steps, "failed", Some(&truncate(&e, 120)));
                trace.push_error("planner", &e, crate::tasks::unix_ts());
                None
            }
        }
    } else {
        None
    };

    // ---- Phase 2: Executor tool loop ----
    let mut messages = build_executor_messages(&history, &message, plan_text.as_deref(), project_ctx_ref);
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
            if cancel.is_cancelled() {
                break 'outer;
            }
            let (e_provider, _) = resolve_provider(&settings, Role::Executor);
            let e_model = model_for_role(&settings, Role::Executor, e_provider);
            emit_step(
                &app,
                &mut steps,
                Role::Executor,
                &format!("executor step {}", iteration + 1),
                "running",
                Some((e_provider, &e_model)),
            );
            let reply = match call_model(
                &app,
                &settings,
                Role::Executor,
                &messages,
                // When the whole turn is in json_mode the executor
                // is not supposed to call tools — the goal planner
                // expects pure JSON output. Dropping the schema here
                // matches the behaviour we negotiate with the
                // provider in `stream_*`.
                if json_mode { None } else { Some(&schema) },
                json_mode,
                &cancel,
            )
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    finish_step(&app, &mut steps, "failed", Some(&truncate(&e, 120)));
                    let _ = app.emit("ai:error", json!({ "message": e, "role": "executor" }));
                    trace.push_error("executor", &e, crate::tasks::unix_ts());
                    return Err(e);
                }
            };

            let content = reply.content.clone().unwrap_or_default();
            let wire_tool_calls = reply.tool_calls.clone().unwrap_or_default();
            messages.push(WireMessage::assistant(&content, reply.tool_calls.clone()));

            if !content.trim().is_empty() {
                trace.push_assistant("executor", &content, crate::tasks::unix_ts());
            }

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
                trace.push_tool_call(
                    &id,
                    "executor",
                    &tc.function.name,
                    &serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                    crate::tasks::unix_ts(),
                );

                let exec_result = match tc.function.name.as_str() {
                    "run_cmd" => {
                        tools::execute_run_cmd_gated(
                            &app,
                            state,
                            &project_dir,
                            &args,
                            &cancel,
                            autonomous_confirm,
                        )
                        .await
                    }
                    other => {
                        tools::execute_safe(
                            &app,
                            state,
                            &project_dir,
                            other,
                            &args,
                            &cancel,
                            autonomous_confirm,
                        )
                        .await
                    }
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
                trace.push_tool_result(
                    &id,
                    "executor",
                    ok,
                    &output,
                    diff.as_deref(),
                    crate::tasks::unix_ts(),
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
        let (r_provider, _) = resolve_provider(&settings, Role::Reviewer);
        let r_model = model_for_role(&settings, Role::Reviewer, r_provider);
        emit_step(
            &app,
            &mut steps,
            Role::Reviewer,
            "reviewing",
            "running",
            Some((r_provider, &r_model)),
        );
        let review_messages = reviewer_messages(
            &message,
            &final_assistant,
            &all_tool_calls,
            &all_tool_results,
            project_ctx_ref,
        );
        // Reviewer routing is driven by `provider_mode` + per-role
        // resolution; the dispatcher falls back to the secondary
        // provider on any failure.
        let review_result = call_model(
            &app,
            &settings,
            Role::Reviewer,
            &review_messages,
            None,
            // Reviewer output is a short OK/NEEDS_FIX verdict,
            // never JSON. Always plain mode regardless of the outer
            // turn's `json_mode` flag.
            false,
            &cancel,
        )
        .await;
        let review_text = match review_result {
            Ok(m) => m.content.clone().unwrap_or_default(),
            Err(e) => {
                finish_step(&app, &mut steps, "failed", Some(&truncate(&e, 120)));
                let _ = app.emit("ai:error", json!({ "message": e, "role": "reviewer" }));
                trace.push_error("reviewer", &e, crate::tasks::unix_ts());
                break 'outer;
            }
        };
        let verdict = parse_review_verdict(&review_text);
        match verdict {
            ReviewVerdict::Ok(summary) => {
                finish_step(&app, &mut steps, "done", Some(&format!("OK: {summary}")));
                trace.push_review("ok", &review_text, crate::tasks::unix_ts());
                break 'outer;
            }
            ReviewVerdict::NeedsFix(instruction) => {
                finish_step(
                    &app,
                    &mut steps,
                    "done",
                    Some(&format!("NEEDS_FIX: {}", truncate(&instruction, 120))),
                );
                trace.push_review("needs_fix", &review_text, crate::tasks::unix_ts());
                reviewer_retries_left -= 1;
                // Feed the reviewer's critique back into the executor loop.
                trace.push_retry(
                    (MAX_REVIEWER_RETRIES - reviewer_retries_left) as u32,
                    &format!("reviewer NEEDS_FIX: {instruction}"),
                    crate::tasks::unix_ts(),
                );
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

    // Note: `final_assistant` is already in the trace — it was pushed
    // as an assistant entry inside the executor loop when its content
    // was first received (see the `push_assistant("executor", ...)`
    // call above). Pushing it again here would double the entry in
    // every successful turn and waste a slot of the 200-entry cap.

    Ok(ChatResponse {
        assistant: final_assistant,
        tool_calls: all_tool_calls,
        tool_results: all_tool_results,
        steps,
        trace,
    })
}

// ---------- Helpers ----------

fn planner_messages(
    history: &[UiMessage],
    user: &str,
    project_ctx: Option<&str>,
) -> Vec<WireMessage> {
    let mut msgs = Vec::with_capacity(history.len() + 3);
    msgs.push(WireMessage::system(PLANNER_PROMPT));
    if let Some(ctx) = project_ctx {
        if !ctx.trim().is_empty() {
            msgs.push(WireMessage::system(ctx));
        }
    }
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

fn reviewer_messages(
    user: &str,
    assistant: &str,
    calls: &[UiToolCall],
    results: &[UiToolResult],
    project_ctx: Option<&str>,
) -> Vec<WireMessage> {
    let tool_summary = if calls.is_empty() {
        "(no tools were called)".to_string()
    } else {
        // Build a compact transcript of tool calls WITH their results so
        // the reviewer can verify what the executor actually *did*, not
        // just what it *said* it did. Each result is capped at 200 chars
        // to keep the reviewer context manageable.
        calls
            .iter()
            .map(|c| {
                let result_line = results
                    .iter()
                    .find(|r| r.id == c.id)
                    .map(|r| {
                        let status = if r.ok { "✓" } else { "✗" };
                        let output = truncate(&r.output, 200);
                        format!("  → {status} {output}")
                    })
                    .unwrap_or_else(|| "  → (no result)".to_string());
                format!("- {} {}\n{}", c.name, args_preview(&c.args), result_line)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let transcript = format!(
        "User request:\n{user}\n\nExecutor tool calls and results:\n{tool_summary}\n\nExecutor summary:\n{assistant}\n"
    );
    let mut msgs: Vec<WireMessage> = Vec::with_capacity(3);
    msgs.push(WireMessage::system(REVIEWER_PROMPT));
    if let Some(ctx) = project_ctx {
        if !ctx.trim().is_empty() {
            msgs.push(WireMessage::system(ctx));
        }
    }
    msgs.push(WireMessage::user(&transcript));
    msgs
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

fn emit_step(
    app: &AppHandle,
    steps: &mut Vec<StepSummary>,
    role: Role,
    title: &str,
    status: &str,
    provider_meta: Option<(Provider, &str)>,
) {
    let index = steps.len() as u32;
    let (provider, model) = match provider_meta {
        Some((p, m)) => (Some(p.as_str().to_string()), Some(m.to_string())),
        None => (None, None),
    };
    let step = StepSummary {
        index,
        role: role.as_str().into(),
        title: title.into(),
        status: status.into(),
        provider,
        model,
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

#[cfg(test)]
mod sse_cancel_tests {
    //! Prove the cancel-vs-stream race pattern used by
    //! `stream_openrouter` and `stream_ollama` actually unwinds
    //! mid-stream, not just between frames. Both providers use the
    //! exact same `tokio::select! { biased; _ = cancel.cancelled() =>
    //! return Err(cancel.err_string()); c = stream.next() => c }`
    //! pattern; we exercise it here against two shapes of stream that
    //! are hard to mock through `reqwest::Response::bytes_stream()` —
    //! an idle stream (no chunks arriving) and a loaded stream (chunks
    //! arriving every 10ms indefinitely).
    use super::*;
    use crate::cancel::{CancelReason, CancelToken};
    use bytes::Bytes;
    use futures_util::stream;
    use std::time::Instant;
    use tokio::sync::mpsc;
    use tokio::time::sleep;
    use tokio_stream::wrappers::ReceiverStream;

    // The exact loop body used by stream_openrouter / stream_ollama,
    // distilled to just the cancel + next-chunk race. We only need to
    // prove the two exits (cancel wins vs chunk wins) work.
    async fn drive_stream_until_cancel<S>(
        mut stream: S,
        cancel: &CancelToken,
    ) -> Result<usize, String>
    where
        S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
    {
        let mut chunks_seen: usize = 0;
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Drop stream (equivalent to dropping the reqwest
                    // response: the underlying TCP connection is closed
                    // and we stop reading).
                    drop(stream);
                    return Err(cancel.err_string());
                }
                c = stream.next() => c,
            };
            match chunk {
                Some(Ok(_)) => chunks_seen += 1,
                Some(Err(e)) => return Err(e.to_string()),
                None => return Ok(chunks_seen), // natural end of stream
            }
        }
    }

    // An idle SSE-like stream: TCP is up, headers were read, but the
    // server hasn't sent a single frame yet. Without the select! race
    // we would block on `stream.next()` forever.
    #[tokio::test]
    async fn sse_cancel_unblocks_idle_stream() {
        let token = CancelToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            t2.cancel_with(CancelReason::User);
        });
        let start = Instant::now();
        // `stream::pending()` never yields — if select! didn't observe
        // cancel, this would hang.
        let pending = stream::pending::<Result<Bytes, std::io::Error>>();
        let res = drive_stream_until_cancel(pending, &token).await;
        let elapsed = start.elapsed();
        assert_eq!(res, Err("cancelled: user".to_string()));
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel should unblock an idle stream fast, took {:?}",
            elapsed
        );
    }

    // A loaded SSE-like stream: chunks arriving every 10ms forever.
    // The loop keeps consuming them; cancel still has to interrupt
    // mid-stream. This is the "under load" case the reviewer called
    // out.
    #[tokio::test]
    async fn sse_cancel_unblocks_loaded_stream_mid_flight() {
        let token = CancelToken::new();
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);

        // Producer: spam chunks forever.
        let producer_tx = tx.clone();
        let producer = tokio::spawn(async move {
            loop {
                if producer_tx
                    .send(Ok(Bytes::from_static(b"data: {}\n\n")))
                    .await
                    .is_err()
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        });
        drop(tx);

        // Canceller: trip the token after ~100ms, long after the
        // consumer has seen chunks flowing.
        let t2 = token.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            t2.cancel_with(CancelReason::Goal);
        });

        let stream = ReceiverStream::new(rx);
        let start = Instant::now();
        let res = drive_stream_until_cancel(stream, &token).await;
        let elapsed = start.elapsed();
        producer.abort();

        assert_eq!(res, Err("cancelled: goal".to_string()));
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel should unwind a loaded stream fast, took {:?}",
            elapsed
        );
    }

    // A pre-cancelled token should short-circuit on the very first
    // select! poll, before reading any chunk.
    #[tokio::test]
    async fn sse_cancel_pre_cancelled_token_returns_before_first_chunk() {
        let token = CancelToken::new();
        token.cancel_with(CancelReason::CircuitOpen);
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        // Pre-fill one chunk so the stream is immediately readable.
        tx.send(Ok(Bytes::from_static(b"data: {}\n\n")))
            .await
            .unwrap();
        drop(tx);
        let stream = ReceiverStream::new(rx);
        let res = drive_stream_until_cancel(stream, &token).await;
        // biased select! picks the cancel branch first even though the
        // stream would also be ready.
        assert_eq!(res, Err("cancelled: circuit_open".to_string()));
    }
}
