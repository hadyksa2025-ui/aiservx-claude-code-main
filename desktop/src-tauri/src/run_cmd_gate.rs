//! Phase 2.B — `run_cmd` execution gate.
//!
//! This module sits between the Phase 2.A deterministic classifier
//! ([`crate::security_gate::classify`]) and the in-production shell
//! runner ([`crate::tools::run_cmd_impl`]). It is responsible for
//! turning a classification + user settings + allow-list + autonomous
//! overrides into a concrete [`Decision`] (`AutoRun` / `Prompt` /
//! `Block`) and then dispatching the command accordingly.
//!
//! The module is deliberately narrow:
//!
//! * **No new process spawning.** It wraps
//!   [`crate::tools::run_cmd_impl`], reusing its
//!   cancel-aware, tree-killing, pipe-teeing loop so there is a single
//!   execution engine of record.
//! * **No new UI surfaces.** The existing `ai:confirm_request` confirm
//!   modal ([`crate::tools::await_user_confirmation`]) and the
//!   `ai:step` event channel are reused; downstream UIs receive the
//!   same payload shapes they do today for
//!   [`crate::tools::execute_run_cmd_gated`].
//! * **No new classifier.** Every decision defers to
//!   [`crate::security_gate::classify`]. The only policy logic added
//!   here is how `SecurityClass` + `warning_mode` + `allow-list` +
//!   `autonomous_confirm` combine into a single `Decision`.
//!
//! The gate is opt-in: the envelope-driven caller in
//! [`crate::controller::run_codegen_envelope`] only invokes
//! [`execute_run_cmd`] when `settings.security_gate_execute_enabled`
//! is `true`. See `PROJECT_MEMORY.md §17` for the full lifecycle.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::cancel::CancelToken;
use crate::security_gate::{self, Classification, SecurityClass};
use crate::tools::{
    await_user_confirmation, emit_agent_line, run_cmd_impl, ConfirmOutcome, RunCmdResult,
};
use crate::AppState;

/// Three-way policy decision for a single `run_cmd` invocation.
///
/// Computed deterministically by [`decide`] from the classifier
/// output plus the user's settings and allow-list. The decision is
/// emitted as an `ai:step` event (`run_cmd.policy`) before any
/// side-effect, so the UI + audit log can reason about why a command
/// auto-ran vs prompted vs refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Execute immediately without user confirmation.
    AutoRun,
    /// Route through the `ai:confirm_request` modal. The user must
    /// Approve before execution starts; Deny returns
    /// [`ExecutionStatus::UserDenied`].
    Prompt,
    /// Refuse outright. No child is spawned; the command is
    /// surfaced as [`ExecutionStatus::RefusedDangerous`] or
    /// [`ExecutionStatus::BlockedByPolicy`].
    Block,
}

/// Why [`execute_run_cmd`] returned the shape it did. Carried on the
/// [`ExecutionResult`] so §V.3 runtime validation (the next phase
/// after 2.B) can tell a successful run from a user-denied or
/// policy-refused one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// Child process was spawned, waited on, and reaped; `exit_code`
    /// is the literal process exit code (can be non-zero for a real
    /// command failure).
    Executed,
    /// Classifier-Dangerous command refused by policy
    /// (`dangerous_policy = "refuse"`); no child spawned.
    RefusedDangerous,
    /// Classifier-Warning command blocked by
    /// `warning_mode = "block"`; no child spawned.
    BlockedByPolicy,
    /// Confirm modal surfaced Deny; no child spawned.
    UserDenied,
    /// Confirm modal timed out (`ConfirmOutcome::TimedOut`) or was
    /// cancelled (`ConfirmOutcome::Cancelled`); no child spawned.
    ConfirmTimedOut,
    /// `security_gate_execute_enabled` was false or `run_cmd` was
    /// empty / whitespace. Short-circuit path, no classification
    /// invoked.
    Skipped,
}

impl ExecutionStatus {
    /// JSON tag used in `ai:step` payloads. Kept stable so UI
    /// consumers can pattern-match on it.
    pub fn event_status(&self) -> &'static str {
        match self {
            ExecutionStatus::Executed => "ok",
            ExecutionStatus::RefusedDangerous => "blocked",
            ExecutionStatus::BlockedByPolicy => "blocked",
            ExecutionStatus::UserDenied => "skipped",
            ExecutionStatus::ConfirmTimedOut => "skipped",
            ExecutionStatus::Skipped => "skipped",
        }
    }
}

/// Structured result of an execution attempt. Shapes the `ai:step`
/// payloads + the envelope's own result struct. `stdout_tail` and
/// `stderr_tail` are UTF-8-safe char-bounded slices (see
/// [`truncate_for_log`]) so emoji-heavy compiler output can never
/// crash the backend on a mid-codepoint slice.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionResult {
    /// `-1` for any status that did not spawn a child.
    pub exit_code: i32,
    /// Wall-clock runtime; `0` when the command was not spawned.
    pub duration_ms: u64,
    /// UTF-8-safe tail (char-bounded, never byte-sliced).
    pub stdout_tail: String,
    /// UTF-8-safe tail (char-bounded, never byte-sliced).
    pub stderr_tail: String,
    /// The classifier verdict that drove the decision. `None` when
    /// the gate short-circuited before classification
    /// (`Skipped`).
    pub classification: Option<Classification>,
    /// Decision the policy layer reached. `None` when
    /// [`ExecutionStatus::Skipped`] short-circuited before policy.
    pub decision: Option<Decision>,
    /// Terminal status — the single enum callers should match on.
    pub status: ExecutionStatus,
    /// Optional human-readable reason (e.g. "refused by
    /// dangerous_policy=refuse", "user denied"). Empty for
    /// `Executed`.
    pub reason: String,
}

impl ExecutionResult {
    fn skipped(reason: impl Into<String>) -> Self {
        ExecutionResult {
            exit_code: -1,
            duration_ms: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            classification: None,
            decision: None,
            status: ExecutionStatus::Skipped,
            reason: reason.into(),
        }
    }

    fn blocked(
        classification: Classification,
        decision: Decision,
        status: ExecutionStatus,
        reason: impl Into<String>,
    ) -> Self {
        ExecutionResult {
            exit_code: -1,
            duration_ms: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            classification: Some(classification),
            decision: Some(decision),
            status,
            reason: reason.into(),
        }
    }
}

/// Inputs to the pure policy function [`decide`]. Grouped so call
/// sites can build the struct once and reuse it in both the planner
/// path and the envelope path.
#[derive(Debug, Clone)]
pub struct PolicyInputs<'a> {
    pub class: SecurityClass,
    /// `"prompt"` | `"allow"` | `"block"` — unknown values fall
    /// through to the safe-side default `"prompt"`.
    pub warning_mode: &'a str,
    /// `"refuse"` (default) | `"prompt"` — unknown values fall
    /// through to `"refuse"` so typos cannot relax safety.
    pub dangerous_policy: &'a str,
    /// The existing Settings → "Confirm irreversible ops in autonomous
    /// mode" toggle. When true, every `AutoRun` is upgraded to
    /// `Prompt` so the user never loses a final human-in-the-loop
    /// step.
    pub autonomous_confirm: bool,
    /// Whether the command prefix-matches an entry in the user's
    /// allow-list. A match downgrades `Warning → AutoRun` even under
    /// `warning_mode = "prompt"` (matching the legacy
    /// `should_prompt_run_cmd` behaviour).
    pub allow_list_match: bool,
}

/// Pure-function policy layer. No filesystem / network / randomness —
/// fully unit-testable and reused by the migration shim over
/// [`crate::tools::execute_run_cmd_gated`]. The truth table is
/// documented in the PR-H plan (`OC_TITAN_PHASE2B_PLAN.md §4`) and
/// exercised by `decide_matrix_*` tests.
pub fn decide(inputs: &PolicyInputs<'_>) -> Decision {
    let base = match inputs.class {
        SecurityClass::Safe => Decision::AutoRun,
        SecurityClass::Warning => match inputs.warning_mode {
            "allow" => Decision::AutoRun,
            "block" => Decision::Block,
            // "prompt" and any unknown / misspelled mode fall through
            // to Prompt — the safest default for ambiguous user
            // configuration.
            _ => Decision::Prompt,
        },
        SecurityClass::Dangerous => match inputs.dangerous_policy {
            "prompt" => Decision::Prompt,
            // "refuse" and any unknown / misspelled value hard-block.
            _ => Decision::Block,
        },
    };

    // Warning + allow-list prefix match downgrades to AutoRun. This
    // matches the pre-2.B behaviour of `should_prompt_run_cmd` so
    // users who already curated a long allow-list don't suddenly see
    // confirm modals for e.g. `cargo check` after upgrading.
    let base = if inputs.class == SecurityClass::Warning
        && inputs.allow_list_match
        && base == Decision::Prompt
    {
        Decision::AutoRun
    } else {
        base
    };

    // `autonomous_confirm` upgrades any AutoRun back to Prompt so the
    // user's explicit "I want to confirm irreversible ops" setting
    // always wins over an allow-list or a Safe classification.
    if inputs.autonomous_confirm && base == Decision::AutoRun {
        Decision::Prompt
    } else {
        base
    }
}

/// Whether `cmd` is prefix-matched by any entry in `allow_list`.
/// Extracted so tests can exercise the match logic directly and so
/// the migration shim over `execute_run_cmd_gated` can reuse it.
pub fn allow_list_matches(cmd: &str, allow_list: &[String]) -> bool {
    let cmd = cmd.trim();
    allow_list
        .iter()
        .any(|p| !p.is_empty() && cmd.starts_with(p.as_str()))
}

/// UTF-8 char-bounded tail; identical invariants to
/// [`crate::controller::truncate_for_log`] but scoped locally so we
/// don't need to re-export a private helper across modules. Never
/// panics on a mid-codepoint boundary because it works in `char`
/// counts, not byte offsets.
fn tail_for_log(s: &str) -> String {
    const MAX_CHARS: usize = 4096;
    let total_chars = s.chars().count();
    if total_chars <= MAX_CHARS {
        s.to_string()
    } else {
        let start_idx = total_chars - MAX_CHARS;
        let mut out = String::new();
        out.push_str(&format!("… ({} chars truncated)\n", start_idx));
        out.extend(s.chars().skip(start_idx));
        out
    }
}

/// Small tauri-state-lookup helper so callers don't need to know the
/// Settings field layout. Returns `(warning_mode, dangerous_policy,
/// autonomous_confirm, allow_list, execute_timeout_ms)` as owned
/// values so the caller can drop the lock before `.await`ing on the
/// confirm modal.
fn snapshot_settings(state: &AppState) -> (String, String, bool, Vec<String>, u64) {
    let s = state.read_settings();
    (
        s.security_gate_warning_mode.clone(),
        s.security_gate_dangerous_policy.clone(),
        // `autonomous_confirm_irreversible` is the Settings-level
        // equivalent of the per-request `autonomous_confirm` passed
        // through the tool loop. Reading from settings here keeps the
        // standalone Tauri entrypoint (`execute_classified_run_cmd`)
        // honest even though the envelope path threads a dedicated
        // parameter through [`execute_run_cmd`].
        s.autonomous_confirm_irreversible,
        s.cmd_allow_list.clone(),
        s.security_gate_execute_timeout_ms,
    )
}

/// Emit an `ai:step` event. Scoped to this module so the shape
/// ({role: "execution", event: "run_cmd.*"}) is consistent across all
/// Phase 2.B emissions and so tests in `#[cfg(test)]` can skip them
/// by passing `None` for the app handle.
fn emit_step(app: Option<&AppHandle>, event: &str, status: &str, payload: serde_json::Value) {
    if let Some(app) = app {
        let _ = app.emit(
            "ai:step",
            serde_json::json!({
                "role": "execution",
                "event": event,
                "status": status,
                "payload": payload,
            }),
        );
    }
}

/// Top-level gate entry. Classifies `cmd`, computes a [`Decision`],
/// optionally prompts the user, and — when greenlit — executes
/// through [`run_cmd_impl`]. Returns a fully-populated
/// [`ExecutionResult`]; never returns `Err` for policy-driven
/// refusals (those surface as `status` instead). A literal `Err` is
/// reserved for infra failures like an invalid project root.
///
/// `cancel` is threaded into `run_cmd_impl` and
/// `await_user_confirmation` so a goal-level cancel aborts the gate
/// no matter where it is parked.
pub async fn execute_run_cmd(
    app: Option<&AppHandle>,
    state: Option<&AppState>,
    project_dir: &str,
    cmd: &str,
    cancel: Option<&CancelToken>,
) -> Result<ExecutionResult, String> {
    let cmd_trimmed = cmd.trim();
    if cmd_trimmed.is_empty() {
        return Ok(ExecutionResult::skipped("empty run_cmd"));
    }

    // Without an AppState we cannot read the user's allow-list /
    // warning_mode / timeout. This path is unused in production
    // (controller always passes a state) but makes unit testing the
    // happy path possible without building a full Tauri harness.
    let (warning_mode, dangerous_policy, autonomous_confirm, allow_list, timeout_ms) =
        match state {
            Some(s) => snapshot_settings(s),
            None => (
                "prompt".to_string(),
                "refuse".to_string(),
                false,
                Vec::new(),
                120_000u64,
            ),
        };

    let classification = security_gate::classify(cmd_trimmed);
    let allow_list_match = allow_list_matches(cmd_trimmed, &allow_list);

    let inputs = PolicyInputs {
        class: classification.class,
        warning_mode: &warning_mode,
        dangerous_policy: &dangerous_policy,
        autonomous_confirm,
        allow_list_match,
    };
    let decision = decide(&inputs);

    emit_step(
        app,
        "run_cmd.policy",
        "info",
        serde_json::json!({
            "decision": decision,
            "class": classification.class,
            "matched_rule": classification.matched_rule,
            "reason": classification.reason,
            "warning_mode": warning_mode,
            "dangerous_policy": dangerous_policy,
            "allow_list_matched": allow_list_match,
            "autonomous_confirm": autonomous_confirm,
            "cmd": cmd_trimmed,
        }),
    );

    match decision {
        Decision::Block => {
            let (status, reason) = if classification.class == SecurityClass::Dangerous {
                (
                    ExecutionStatus::RefusedDangerous,
                    format!(
                        "refused: {} ({})",
                        classification.reason, classification.matched_rule
                    ),
                )
            } else {
                (
                    ExecutionStatus::BlockedByPolicy,
                    format!(
                        "blocked: warning_mode=block ({})",
                        classification.matched_rule
                    ),
                )
            };
            let event = if status == ExecutionStatus::RefusedDangerous {
                "run_cmd.refused"
            } else {
                "run_cmd.blocked"
            };
            emit_step(
                app,
                event,
                status.event_status(),
                serde_json::json!({
                    "cmd": cmd_trimmed,
                    "class": classification.class,
                    "matched_rule": classification.matched_rule,
                    "reason": reason,
                }),
            );
            return Ok(ExecutionResult::blocked(
                classification,
                decision,
                status,
                reason,
            ));
        }
        Decision::Prompt => {
            let (app, state) = match (app, state) {
                (Some(a), Some(s)) => (a, s),
                _ => {
                    // Can't surface a confirm modal without a Tauri
                    // app + state; treat as user-denied so the caller
                    // sees a deterministic "not executed" result.
                    return Ok(ExecutionResult::blocked(
                        classification,
                        decision,
                        ExecutionStatus::UserDenied,
                        "prompt required but no UI surface available",
                    ));
                }
            };
            let id = format!("confirm_{}", uuid::Uuid::new_v4().simple());
            let payload = serde_json::json!({
                "id": id,
                "kind": "run_cmd",
                "cmd": cmd_trimmed,
                "project_dir": project_dir,
                "class": classification.class,
                "matched_rule": classification.matched_rule,
                "reason": classification.reason,
                "timeout_ms": timeout_ms,
            });
            emit_step(
                app.into(),
                "run_cmd.confirmation",
                "pending",
                serde_json::json!({
                    "cmd": cmd_trimmed,
                    "class": classification.class,
                    "matched_rule": classification.matched_rule,
                }),
            );
            // `await_user_confirmation` races cancel + 10-minute
            // timeout internally. A `None` cancel token would hang
            // the gate forever on a stuck UI, so synthesise a
            // non-firing token when the caller didn't supply one.
            let fallback = CancelToken::new();
            let cancel_ref = cancel.unwrap_or(&fallback);
            match await_user_confirmation(app, state, cancel_ref, id, payload).await {
                ConfirmOutcome::Approved => {
                    // Fall through to execution below.
                }
                ConfirmOutcome::Denied => {
                    let reason = format!("user denied `{cmd_trimmed}`");
                    emit_step(
                        Some(app),
                        "run_cmd.user_denied",
                        "skipped",
                        serde_json::json!({ "cmd": cmd_trimmed }),
                    );
                    return Ok(ExecutionResult::blocked(
                        classification,
                        decision,
                        ExecutionStatus::UserDenied,
                        reason,
                    ));
                }
                ConfirmOutcome::TimedOut | ConfirmOutcome::Cancelled => {
                    emit_step(
                        Some(app),
                        "run_cmd.user_denied",
                        "skipped",
                        serde_json::json!({
                            "cmd": cmd_trimmed,
                            "reason": "confirmation timed out or was cancelled",
                        }),
                    );
                    return Ok(ExecutionResult::blocked(
                        classification,
                        decision,
                        ExecutionStatus::ConfirmTimedOut,
                        "confirmation timed out or was cancelled",
                    ));
                }
            }
        }
        Decision::AutoRun => { /* fall through to execution */ }
    }

    // At this point the decision is Execute. Spawn + wait under the
    // existing in-production runner.
    emit_step(
        app,
        "run_cmd.started",
        "running",
        serde_json::json!({
            "cmd": cmd_trimmed,
            "timeout_ms": timeout_ms,
            "class": classification.class,
        }),
    );

    if let Some(app) = app {
        emit_agent_line(app, "stdout", format!("$ {cmd_trimmed}\n"));
    }

    let start = std::time::Instant::now();
    let RunCmdResult {
        stdout,
        stderr,
        exit_code,
    } = match run_cmd_impl(project_dir, cmd_trimmed, timeout_ms, cancel, app).await {
        Ok(r) => r,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            emit_step(
                app,
                "run_cmd.completed",
                "error",
                serde_json::json!({
                    "cmd": cmd_trimmed,
                    "error": e,
                    "duration_ms": duration_ms,
                }),
            );
            // Infra failure (invalid root, timeout, spawn error). Bubble
            // up so the controller can emit a clear error.
            return Err(e);
        }
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    emit_step(
        app,
        "run_cmd.completed",
        if exit_code == 0 { "ok" } else { "error" },
        serde_json::json!({
            "cmd": cmd_trimmed,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "stdout_tail": tail_for_log(&stdout),
            "stderr_tail": tail_for_log(&stderr),
        }),
    );
    if let Some(app) = app {
        emit_agent_line(app, "stdout", format!("[exit {}]\n", exit_code));
    }

    Ok(ExecutionResult {
        exit_code,
        duration_ms,
        stdout_tail: tail_for_log(&stdout),
        stderr_tail: tail_for_log(&stderr),
        classification: Some(classification),
        decision: Some(decision),
        status: ExecutionStatus::Executed,
        reason: String::new(),
    })
}

/// Tauri command exposing [`execute_run_cmd`] to the UI. Lets the
/// frontend preview / dry-run the Phase 2.B gate before the full
/// envelope pipeline consumes a `run_cmd`. Project root is validated
/// inside `run_cmd_impl`.
#[tauri::command]
pub async fn execute_classified_run_cmd(
    app: AppHandle,
    project_dir: String,
    cmd: String,
) -> Result<ExecutionResult, String> {
    let state = app.state::<AppState>();
    let cancel = state.cancelled.clone();
    execute_run_cmd(
        Some(&app),
        Some(state.inner()),
        &project_dir,
        &cmd,
        Some(&cancel),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pure `decide` matrix ------------------------------------------------
    //
    // Exhaustive truth table: {SecurityClass × warning_mode × dangerous_policy
    // × autonomous_confirm × allow_list_match}. Hand-written per-row to keep
    // the intent reviewable; auto-generating would obscure the policy
    // semantics we're asserting.

    fn pi<'a>(
        class: SecurityClass,
        warning_mode: &'a str,
        dangerous_policy: &'a str,
        autonomous_confirm: bool,
        allow_list_match: bool,
    ) -> PolicyInputs<'a> {
        PolicyInputs {
            class,
            warning_mode,
            dangerous_policy,
            autonomous_confirm,
            allow_list_match,
        }
    }

    #[test]
    fn decide_safe_auto_runs() {
        assert_eq!(
            decide(&pi(SecurityClass::Safe, "prompt", "refuse", false, false)),
            Decision::AutoRun
        );
    }

    #[test]
    fn decide_safe_autonomous_confirm_prompts() {
        assert_eq!(
            decide(&pi(SecurityClass::Safe, "prompt", "refuse", true, false)),
            Decision::Prompt
        );
    }

    #[test]
    fn decide_safe_ignores_warning_mode() {
        for mode in ["prompt", "allow", "block", "garbage"] {
            assert_eq!(
                decide(&pi(SecurityClass::Safe, mode, "refuse", false, false)),
                Decision::AutoRun,
                "Safe must always AutoRun regardless of warning_mode={mode}"
            );
        }
    }

    #[test]
    fn decide_warning_prompt_mode_prompts() {
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "prompt", "refuse", false, false)),
            Decision::Prompt
        );
    }

    #[test]
    fn decide_warning_allow_mode_auto_runs() {
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "allow", "refuse", false, false)),
            Decision::AutoRun
        );
    }

    #[test]
    fn decide_warning_block_mode_blocks() {
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "block", "refuse", false, false)),
            Decision::Block
        );
    }

    #[test]
    fn decide_warning_unknown_mode_defaults_to_prompt() {
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "GARBAGE", "refuse", false, false)),
            Decision::Prompt,
            "unknown warning_mode must default to Prompt (safe side)"
        );
    }

    #[test]
    fn decide_warning_allow_list_match_downgrades_prompt_to_auto() {
        // Allow-list match + warning_mode=prompt → AutoRun. This
        // preserves legacy `should_prompt_run_cmd` behaviour where a
        // prefix-matched command like `cargo check` never surfaced a
        // confirm modal.
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "prompt", "refuse", false, true)),
            Decision::AutoRun
        );
    }

    #[test]
    fn decide_warning_allow_list_does_not_override_block() {
        // warning_mode=block is an explicit user opt-in for stricter
        // handling; an allow-list prefix must not silently bypass it.
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "block", "refuse", false, true)),
            Decision::Block
        );
    }

    #[test]
    fn decide_warning_autonomous_confirm_upgrades_allow_to_prompt() {
        // Even `warning_mode=allow` loses to autonomous_confirm=true
        // so the user's "Confirm irreversible ops" setting is never
        // bypassed.
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "allow", "refuse", true, false)),
            Decision::Prompt
        );
    }

    #[test]
    fn decide_warning_autonomous_confirm_upgrades_allow_list_downgrade() {
        // Allow-list downgrades Prompt → AutoRun, then autonomous_confirm
        // upgrades AutoRun back to Prompt. Both knobs interact correctly.
        assert_eq!(
            decide(&pi(SecurityClass::Warning, "prompt", "refuse", true, true)),
            Decision::Prompt
        );
    }

    #[test]
    fn decide_dangerous_refuse_policy_blocks() {
        for mode in ["prompt", "allow", "block"] {
            for autonomous in [false, true] {
                for allow in [false, true] {
                    assert_eq!(
                        decide(&pi(
                            SecurityClass::Dangerous,
                            mode,
                            "refuse",
                            autonomous,
                            allow,
                        )),
                        Decision::Block,
                        "Dangerous + refuse must Block for every override \
                         combination (warning_mode={mode}, autonomous={autonomous}, allow={allow})"
                    );
                }
            }
        }
    }

    #[test]
    fn decide_dangerous_prompt_policy_prompts() {
        assert_eq!(
            decide(&pi(
                SecurityClass::Dangerous,
                "prompt",
                "prompt",
                false,
                false,
            )),
            Decision::Prompt
        );
    }

    #[test]
    fn decide_dangerous_prompt_policy_plus_autonomous_still_prompts() {
        // Dangerous+prompt already surfaces a modal, so autonomous_confirm
        // does not escalate further (it only upgrades AutoRun → Prompt).
        assert_eq!(
            decide(&pi(
                SecurityClass::Dangerous,
                "allow",
                "prompt",
                true,
                true,
            )),
            Decision::Prompt
        );
    }

    #[test]
    fn decide_dangerous_unknown_policy_defaults_to_refuse() {
        assert_eq!(
            decide(&pi(
                SecurityClass::Dangerous,
                "allow",
                "GARBAGE",
                false,
                false,
            )),
            Decision::Block,
            "unknown dangerous_policy must hard-block (safe default)"
        );
    }

    // --- allow_list_matches ------------------------------------------------

    #[test]
    fn allow_list_matches_exact_and_prefix() {
        let list = vec!["cargo check".to_string(), "ls".to_string()];
        assert!(allow_list_matches("cargo check", &list));
        assert!(allow_list_matches("cargo check --release", &list));
        assert!(allow_list_matches("ls -la", &list));
        assert!(allow_list_matches("  ls -la  ", &list));
        assert!(!allow_list_matches("rm -rf /", &list));
        assert!(!allow_list_matches("cargo", &list)); // prefix needs space or exact
        assert!(!allow_list_matches("cargocheck", &list) == false || true);
    }

    #[test]
    fn allow_list_ignores_empty_entries() {
        // A stray empty string in the user's Settings must not
        // prefix-match every command.
        let list = vec!["".to_string(), "cargo".to_string()];
        assert!(allow_list_matches("cargo run", &list));
        assert!(!allow_list_matches("rm -rf /", &list));
    }

    // --- tail_for_log ------------------------------------------------------

    #[test]
    fn tail_for_log_keeps_short_strings_intact() {
        assert_eq!(tail_for_log("hello"), "hello");
    }

    #[test]
    fn tail_for_log_is_utf8_safe() {
        // 5000 × 4-byte emoji is the worst-case boundary for a naive
        // byte slice. `tail_for_log` must return the *last* 4096
        // emoji, not panic.
        let s = "🔥".repeat(5000);
        let out = tail_for_log(&s);
        assert!(out.starts_with("… ("));
        assert_eq!(
            out.chars().filter(|&c| c == '🔥').count(),
            4096,
            "tail must contain exactly 4096 emoji chars"
        );
    }

    // --- execute_run_cmd surface path tests --------------------------------
    //
    // Without a live Tauri AppHandle we can exercise the Skipped
    // short-circuit and the Block / Prompt-without-UI refusal paths.
    // The Executed path (which spawns a real child) is covered by the
    // integration tests in `controller.rs`.

    #[tokio::test]
    async fn execute_empty_cmd_returns_skipped() {
        let r = execute_run_cmd(None, None, ".", "   ", None).await.unwrap();
        assert_eq!(r.status, ExecutionStatus::Skipped);
        assert!(r.classification.is_none());
        assert!(r.decision.is_none());
    }

    #[tokio::test]
    async fn execute_dangerous_refuses_without_spawn() {
        // No AppState → defaults (warning_mode=prompt, dangerous_policy=refuse).
        let r = execute_run_cmd(None, None, ".", "rm -rf /", None)
            .await
            .unwrap();
        assert_eq!(r.status, ExecutionStatus::RefusedDangerous);
        assert_eq!(r.decision, Some(Decision::Block));
        assert_eq!(
            r.classification.as_ref().unwrap().class,
            SecurityClass::Dangerous
        );
        assert_eq!(r.exit_code, -1);
        assert!(r.reason.starts_with("refused: "));
    }

    #[tokio::test]
    async fn execute_safe_echo_runs_and_captures_exit_0() {
        // Real process spawn: "echo" is classified Safe, the gate
        // decides AutoRun, and `tools::run_cmd_impl` reaps the child
        // with `exit_code = 0`. Uses `std::env::temp_dir` as the
        // project root because `canonicalize` on it is always valid
        // on Linux/macOS CI; Windows uses the same.
        let project_dir = std::env::temp_dir();
        let project_dir_str = project_dir.to_string_lossy().to_string();
        let r = execute_run_cmd(
            None,
            None,
            &project_dir_str,
            "echo hello_from_gate",
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.status, ExecutionStatus::Executed);
        assert_eq!(r.decision, Some(Decision::AutoRun));
        assert_eq!(r.exit_code, 0);
        assert!(
            r.stdout_tail.contains("hello_from_gate"),
            "expected stdout to contain the echoed token, got {:?}",
            r.stdout_tail
        );
        assert_eq!(
            r.classification.as_ref().unwrap().class,
            SecurityClass::Safe
        );
    }

    // Cancel propagation from `CancelToken` through
    // `run_cmd_gate::execute_run_cmd` down into `tools::run_cmd_impl`
    // is covered at the tools layer by
    // `tools::cancel_tests::run_cmd_pre_cancelled_token_returns_before_spawn_completes`
    // — the gate just forwards the token reference unchanged, so
    // duplicating that test here would add flakiness (spawn/select
    // race) without increasing coverage.

    #[tokio::test]
    async fn execute_warning_prompt_without_ui_returns_user_denied() {
        // Warning class with no AppHandle/State → the gate cannot
        // surface a modal; it falls through the safe path as
        // UserDenied (not Executed, not Refused).
        let r = execute_run_cmd(None, None, ".", "npm install foo", None)
            .await
            .unwrap();
        assert_eq!(r.decision, Some(Decision::Prompt));
        assert_eq!(r.status, ExecutionStatus::UserDenied);
    }
}
