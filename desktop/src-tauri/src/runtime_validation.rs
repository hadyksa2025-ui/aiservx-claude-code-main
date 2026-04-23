//! Phase §V.3 — runtime validation of `run_cmd` execution outcomes.
//!
//! Consumes the [`ExecutionResult`](crate::run_cmd_gate::ExecutionResult)
//! produced by Phase 2.B and decides whether the envelope should be
//! accepted as-is, re-prompted with the stderr/stdout tails attached,
//! or left alone (because the gate never actually spawned a child).
//!
//! This module owns only the *policy* side: the outcome enum, the
//! pure classifier [`evaluate`], and the reprompt formatter
//! [`build_reprompt`]. Retry orchestration (budget sharing with the
//! compiler gate + dependency guard, `ai:step` event emission) lives
//! in [`controller::run_codegen_envelope`](crate::controller).
//!
//! ## Invariants
//!
//! * Pure + deterministic: same `ExecutionResult` → same
//!   [`RuntimeOutcome`].
//! * Never `panic!`s on `ExecutionResult` shapes the gate can
//!   legitimately produce (including exotic exit codes like `-1`
//!   marker for non-Executed status, UTF-8-safe tails, etc.).
//! * Short-circuits on every non-[`ExecutionStatus::Executed`] status:
//!   a Refused / UserDenied / ConfirmTimedOut / BlockedByPolicy /
//!   Skipped command has *no runtime to validate*, so it must never
//!   consume an attempt from the shared retry budget.

use crate::run_cmd_gate::{ExecutionResult, ExecutionStatus};

/// Three-way outcome the controller branches on.
///
/// * [`RuntimeOutcome::Ok`] — child exited 0, no reprompt, loop can
///   break successfully.
/// * [`RuntimeOutcome::Errors`] — child spawned and exited non-zero;
///   the controller should reprompt the model with
///   [`build_reprompt`] and consume one attempt from the shared
///   `max_compile_retries` budget.
/// * [`RuntimeOutcome::Skipped`] — either the feature is disabled,
///   there is no [`ExecutionResult`] to inspect, or the execution
///   status was anything other than [`ExecutionStatus::Executed`]
///   (refused, user-denied, confirm-timed-out, blocked-by-policy,
///   skipped). Never triggers a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeOutcome {
    Ok {
        exit_code: i32,
        duration_ms: u64,
    },
    Errors {
        exit_code: i32,
        stderr_tail: String,
        stdout_tail: String,
        duration_ms: u64,
    },
    Skipped {
        reason: &'static str,
    },
}

#[allow(dead_code)] // exercised by the runtime_validation unit tests; keeping
                    // these helpers on the public API so future UI consumers
                    // can drive ai:step payloads without re-deriving the enum.
impl RuntimeOutcome {
    /// Stable JSON tag for the `ai:step` payload so UI consumers can
    /// pattern-match without re-deriving the enum.
    pub fn event_label(&self) -> &'static str {
        match self {
            RuntimeOutcome::Ok { .. } => "runtime.ok",
            RuntimeOutcome::Errors { .. } => "runtime.errors",
            RuntimeOutcome::Skipped { .. } => "runtime.skipped",
        }
    }

    /// JSON status tag on the `ai:step` payload. Matches the
    /// conventions already used by the compiler gate + dependency
    /// guard (`"done"` / `"failed"` / `"skipped"`) so the UI can
    /// render uniform risk badges across all self-healing phases.
    pub fn event_status(&self) -> &'static str {
        match self {
            RuntimeOutcome::Ok { .. } => "done",
            RuntimeOutcome::Errors { .. } => "failed",
            RuntimeOutcome::Skipped { .. } => "skipped",
        }
    }

    /// True when the outcome warrants consuming a retry slot. Purely
    /// a convenience for the controller — the truth table is
    /// `matches!(self, RuntimeOutcome::Errors { .. })`.
    pub fn should_retry(&self) -> bool {
        matches!(self, RuntimeOutcome::Errors { .. })
    }
}

/// Map a non-[`ExecutionStatus::Executed`] status to a stable
/// `Skipped { reason }` tag. Exposed via `pub(crate)` so the
/// `evaluate` tests can pin the exact strings without going through
/// the whole [`ExecutionResult`] shim.
pub(crate) fn status_to_reason(s: ExecutionStatus) -> &'static str {
    match s {
        ExecutionStatus::Executed => "executed",
        ExecutionStatus::RefusedDangerous => "refused_dangerous",
        ExecutionStatus::BlockedByPolicy => "blocked_by_policy",
        ExecutionStatus::UserDenied => "user_denied",
        ExecutionStatus::ConfirmTimedOut => "confirm_timed_out",
        ExecutionStatus::Skipped => "execution_skipped",
    }
}

/// Classify the execution outcome. Pure + deterministic.
///
/// Short-circuit ordering:
///
/// 1. `enabled=false` → [`RuntimeOutcome::Skipped`] (`"disabled"`).
/// 2. `execution=None` (envelope had no `run_cmd`, or Phase 2.B
///    opt-out was off) → [`RuntimeOutcome::Skipped`] (`"no_execution"`).
/// 3. `status != Executed` → [`RuntimeOutcome::Skipped`] with a
///    status-specific reason. A Refused / UserDenied / BlockedByPolicy
///    command must not consume a retry slot because there is no
///    stderr to learn from.
/// 4. `exit_code == 0` → [`RuntimeOutcome::Ok`].
/// 5. `exit_code != 0` → [`RuntimeOutcome::Errors`] carrying the
///    UTF-8-safe tails already captured by the gate.
pub fn evaluate(execution: Option<&ExecutionResult>, enabled: bool) -> RuntimeOutcome {
    if !enabled {
        return RuntimeOutcome::Skipped { reason: "disabled" };
    }
    let Some(exec) = execution else {
        return RuntimeOutcome::Skipped {
            reason: "no_execution",
        };
    };
    if exec.status != ExecutionStatus::Executed {
        return RuntimeOutcome::Skipped {
            reason: status_to_reason(exec.status),
        };
    }
    if exec.exit_code == 0 {
        return RuntimeOutcome::Ok {
            exit_code: 0,
            duration_ms: exec.duration_ms,
        };
    }
    RuntimeOutcome::Errors {
        exit_code: exec.exit_code,
        stderr_tail: exec.stderr_tail.clone(),
        stdout_tail: exec.stdout_tail.clone(),
        duration_ms: exec.duration_ms,
    }
}

/// Build the executor-facing reprompt when the runtime has reported
/// a non-zero exit. Keeps the framing consistent with the compiler
/// gate + dependency guard reprompts — the model sees both the
/// original request and a bulleted list of the stderr/stdout tails.
///
/// `stderr_tail` / `stdout_tail` are assumed to be the
/// already-char-bounded values produced by the Phase 2.B gate (see
/// [`run_cmd_gate::truncate_for_log`](crate::run_cmd_gate)) — this
/// function does no further trimming. An empty tail is rendered as
/// the literal `"(empty)"` so the model is never shown a blank
/// section header.
pub fn build_reprompt(
    original_request: &str,
    exit_code: i32,
    stderr_tail: &str,
    stdout_tail: &str,
) -> String {
    let stderr_section = if stderr_tail.trim().is_empty() {
        "(empty)".to_string()
    } else {
        stderr_tail.to_string()
    };
    let stdout_section = if stdout_tail.trim().is_empty() {
        "(empty)".to_string()
    } else {
        stdout_tail.to_string()
    };
    format!(
        "{original_request}\n\n\
         [runtime validation] Your previous envelope compiled cleanly \
         but the `run_cmd` you emitted exited with a non-zero status \
         when executed. The command produced diagnostics you can use \
         to fix the underlying code — do NOT work around the failure \
         by changing the `run_cmd` itself. Read the stderr / stdout \
         tails below and rewrite the offending source files:\n\
         exit_code: {exit_code}\n\
         --- stderr (last chars) ---\n{stderr_section}\n\
         --- stdout (last chars) ---\n{stdout_section}\n\
         --- end ---\n\
         Emit a NEW, complete codegen envelope. Every `path` must \
         still be sandbox-relative and every `content` must contain \
         the FULL file contents. If the same `run_cmd` should stay \
         (e.g. `npm test`), keep it so the fix can be verified on \
         the next attempt."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_cmd_gate::{ExecutionResult, ExecutionStatus};
    use crate::security_gate::{Classification, SecurityClass};

    fn make_classification(class: SecurityClass) -> Classification {
        Classification {
            class,
            reason: "test".to_string(),
            matched_rule: "test",
            compound: false,
        }
    }

    fn executed(exit_code: i32, stderr: &str, stdout: &str) -> ExecutionResult {
        ExecutionResult {
            exit_code,
            duration_ms: 42,
            stdout_tail: stdout.to_string(),
            stderr_tail: stderr.to_string(),
            classification: Some(make_classification(SecurityClass::Safe)),
            decision: None,
            status: ExecutionStatus::Executed,
            reason: String::new(),
        }
    }

    fn non_executed(status: ExecutionStatus) -> ExecutionResult {
        ExecutionResult {
            exit_code: -1,
            duration_ms: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            classification: Some(make_classification(SecurityClass::Warning)),
            decision: None,
            status,
            reason: "test".to_string(),
        }
    }

    // ---- evaluate() truth table ----

    #[test]
    fn evaluate_disabled_always_skips_even_with_nonzero_exit() {
        let exec = executed(1, "boom", "");
        let out = evaluate(Some(&exec), false);
        assert_eq!(out, RuntimeOutcome::Skipped { reason: "disabled" });
        assert!(!out.should_retry(), "disabled must never retry");
    }

    #[test]
    fn evaluate_no_execution_returns_no_execution_skip() {
        let out = evaluate(None, true);
        assert_eq!(out, RuntimeOutcome::Skipped { reason: "no_execution" });
        assert!(!out.should_retry());
    }

    #[test]
    fn evaluate_exit_zero_returns_ok() {
        let exec = executed(0, "", "hello");
        let out = evaluate(Some(&exec), true);
        assert_eq!(
            out,
            RuntimeOutcome::Ok {
                exit_code: 0,
                duration_ms: 42,
            }
        );
        assert!(!out.should_retry(), "ok must not retry");
        assert_eq!(out.event_label(), "runtime.ok");
        assert_eq!(out.event_status(), "done");
    }

    #[test]
    fn evaluate_exit_nonzero_returns_errors_with_tails() {
        let exec = executed(2, "TypeError: boom", "diagnostic line");
        let out = evaluate(Some(&exec), true);
        match out.clone() {
            RuntimeOutcome::Errors {
                exit_code,
                stderr_tail,
                stdout_tail,
                duration_ms,
            } => {
                assert_eq!(exit_code, 2);
                assert_eq!(stderr_tail, "TypeError: boom");
                assert_eq!(stdout_tail, "diagnostic line");
                assert_eq!(duration_ms, 42);
            }
            other => panic!("expected Errors, got {other:?}"),
        }
        assert!(out.should_retry(), "non-zero exit must retry");
        assert_eq!(out.event_label(), "runtime.errors");
        assert_eq!(out.event_status(), "failed");
    }

    #[test]
    fn evaluate_exit_negative_one_on_executed_still_counts_as_errors() {
        // Some shells / SIGKILL paths produce -1 on Executed status.
        // It's still a real spawn, still a non-zero exit, still needs
        // reprompting — the -1 is *only* a Skipped sentinel when the
        // status is also non-Executed (see the non_executed helper).
        let exec = executed(-1, "killed by SIGKILL", "");
        let out = evaluate(Some(&exec), true);
        assert!(out.should_retry());
        assert_eq!(out.event_label(), "runtime.errors");
    }

    // ---- non-Executed statuses must short-circuit to Skipped ----

    #[test]
    fn evaluate_refused_dangerous_skips() {
        let exec = non_executed(ExecutionStatus::RefusedDangerous);
        let out = evaluate(Some(&exec), true);
        assert_eq!(
            out,
            RuntimeOutcome::Skipped {
                reason: "refused_dangerous",
            }
        );
    }

    #[test]
    fn evaluate_blocked_by_policy_skips() {
        let exec = non_executed(ExecutionStatus::BlockedByPolicy);
        let out = evaluate(Some(&exec), true);
        assert_eq!(
            out,
            RuntimeOutcome::Skipped {
                reason: "blocked_by_policy",
            }
        );
    }

    #[test]
    fn evaluate_user_denied_skips() {
        let exec = non_executed(ExecutionStatus::UserDenied);
        let out = evaluate(Some(&exec), true);
        assert_eq!(
            out,
            RuntimeOutcome::Skipped {
                reason: "user_denied",
            }
        );
    }

    #[test]
    fn evaluate_confirm_timed_out_skips() {
        let exec = non_executed(ExecutionStatus::ConfirmTimedOut);
        let out = evaluate(Some(&exec), true);
        assert_eq!(
            out,
            RuntimeOutcome::Skipped {
                reason: "confirm_timed_out",
            }
        );
    }

    #[test]
    fn evaluate_execution_skipped_skips() {
        let exec = non_executed(ExecutionStatus::Skipped);
        let out = evaluate(Some(&exec), true);
        assert_eq!(
            out,
            RuntimeOutcome::Skipped {
                reason: "execution_skipped",
            }
        );
    }

    // ---- build_reprompt formatting ----

    #[test]
    fn reprompt_includes_original_request_exit_and_both_tails() {
        let out = build_reprompt(
            "Please add a login page",
            1,
            "TypeError: cannot read 'foo' of undefined",
            "> test complete with failures",
        );
        assert!(out.contains("Please add a login page"));
        assert!(out.contains("exit_code: 1"));
        assert!(out.contains("TypeError: cannot read 'foo' of undefined"));
        assert!(out.contains("> test complete with failures"));
        assert!(out.contains("stderr (last chars)"));
        assert!(out.contains("stdout (last chars)"));
        assert!(out.contains("NEW, complete codegen envelope"));
    }

    #[test]
    fn reprompt_renders_empty_tails_as_explicit_marker() {
        let out = build_reprompt("req", 137, "", "");
        // Never blank-section — the model must see *something* under
        // each header so it doesn't hallucinate missing context.
        assert!(
            out.contains("--- stderr (last chars) ---\n(empty)"),
            "empty stderr must be rendered as `(empty)`, got:\n{out}"
        );
        assert!(
            out.contains("--- stdout (last chars) ---\n(empty)"),
            "empty stdout must be rendered as `(empty)`, got:\n{out}"
        );
        assert!(out.contains("exit_code: 137"));
    }

    #[test]
    fn reprompt_treats_whitespace_only_tails_as_empty() {
        let out = build_reprompt("req", 1, "   \n\t  ", "\n\n");
        assert!(out.contains("--- stderr (last chars) ---\n(empty)"));
        assert!(out.contains("--- stdout (last chars) ---\n(empty)"));
    }

    #[test]
    fn reprompt_is_utf8_safe_with_emoji_and_multibyte_tails() {
        // Regression vector: PR-C fixed a UTF-8 panic in
        // `truncate_for_log`. The reprompt builder must never
        // re-introduce byte slicing on the tails it's given.
        let err = "🔥 boom — الخطأ حصل";
        let out = build_reprompt("ابنِ صفحة تسجيل دخول", 1, err, "ا".repeat(10).as_str());
        assert!(out.contains("🔥 boom"));
        assert!(out.contains("الخطأ حصل"));
        assert!(out.contains("ابنِ صفحة تسجيل دخول"));
    }
}
