//! OC-Titan Phase 2.C — fix-forward auto-install of packages the
//! [`dependency_guard`](crate::dependency_guard) reports as
//! missing.
//!
//! # Why this module exists
//!
//! Phase 1.C taught the controller to **catch** phantom imports
//! before they hit `tsc --noEmit`, but the fix was always the same:
//! reprompt the model and burn a retry slot. With Phase 2.A (command
//! classifier) and Phase 2.B (executor) now in production, we can do
//! one better: synthesise a deterministic `bun add <pkgs>` /
//! `npm install <pkgs>` command, route it through the exact same
//! WARNING-tier gate that any human-typed `run_cmd` would go through,
//! and only fall back to the reprompt path when the install is
//! refused, denied, or fails at runtime.
//!
//! # Lifecycle
//!
//! Called from [`controller::run_codegen_envelope`] on every
//! [`dependency_guard::GuardOutcome::Missing`] (and, when
//! `dependency_guard_mode="warn"`, on `Warned` too) **before** the
//! classic reprompt branch:
//!
//! 1. If `autoinstall_enabled=false` → `Skipped { reason: "disabled" }`.
//! 2. If `security_gate_execute_enabled=false` → `Skipped { reason:
//!    "execute_disabled" }`. The synthesised command physically can
//!    not run without Phase 2.B enabled, so we don't pretend.
//! 3. If `missing.is_empty()` → `Skipped { reason: "no_missing" }`.
//!    Defensive; the controller never calls us with an empty list
//!    today but the signature promise is cheap.
//! 4. [`resolve_package_manager`] picks a manager from the setting +
//!    lockfile probe. `"auto"` with no lockfile falls back to
//!    [`PackageManager::Bun`] to match the rest of the repo's
//!    Bun-first defaults.
//! 5. [`synthesise_install_cmd`] renders `<pm> <subcmd> <pkgs…>`
//!    with a deterministic, alphabetically-sorted + de-duplicated
//!    package list so the string is stable across retries and
//!    safely classifiable by [`security_gate::classify`].
//! 6. [`run_cmd_gate::execute_run_cmd`] runs the usual policy truth
//!    table (Warning → prompt / auto / block depending on
//!    `warning_mode` + `allow_list_match`). The caller is
//!    responsible for forwarding the autonomous-confirm override.
//! 7. Return an [`AutoInstallOutcome::Executed`] carrying the
//!    manager, the rendered command, and the [`ExecutionResult`].
//!    The controller decides whether to re-run the dependency
//!    guard (`exit_code == 0`) or fall back to the reprompt path.
//!
//! # Budget invariant
//!
//! A **successful** install (exit 0 + post-install guard clean) is
//! *fix-forward*: the controller does not consume a retry slot from
//! `max_compile_retries`, it simply continues with the compiler
//! gate in the same iteration. Every other terminal state
//! (refused, blocked, user-denied, confirm-timed-out, non-zero
//! exit, post-install guard still missing) falls back to the
//! classic `dependency.missing` reprompt path, which *does*
//! consume a slot. Documented in `PROJECT_MEMORY.md §19.3`.
//!
//! # Why a dedicated module
//!
//! Package-manager detection, argument quoting, and command
//! synthesis are pure, unit-testable logic that would drown the
//! controller if inlined. Keeping them here also means Phase 2.C
//! can evolve (e.g. `--save-dev` handling, Python/Cargo support)
//! without churning `controller.rs`.

use std::path::Path;

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::codegen_envelope::CodegenEnvelope;
use crate::dependency_guard::{self, GuardOutcome};
use crate::run_cmd_gate::{self, ExecutionStatus};
use crate::AppState;

/// The four Node-ecosystem package managers we know how to drive.
///
/// The ordering here — `Bun` first — matters for
/// [`default_package_manager`] and for the `"auto"` fallback when
/// no lockfile is present in the project directory. The rest of
/// the repo treats Bun as the primary runtime (see
/// `AGENTS.md` / `CLAUDE.md`), so the fallback mirrors that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Bun,
    Npm,
    Pnpm,
    Yarn,
}

impl PackageManager {
    /// Canonical lowercase string — matches the value accepted by
    /// [`Settings::autoinstall_package_manager`] overrides and the
    /// `pm` field on every `ai:step` event this module emits.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }

    /// Parse a user-supplied `autoinstall_package_manager` setting
    /// value. Returns `None` for any unknown string so the caller
    /// can fall back to `"auto"` detection — we deliberately do
    /// not panic or error on bad input, because the guardrail
    /// here is "worst case, the user gets bun instead of their
    /// unknown preferred manager".
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bun" => Some(Self::Bun),
            "npm" => Some(Self::Npm),
            "pnpm" => Some(Self::Pnpm),
            "yarn" => Some(Self::Yarn),
            _ => None,
        }
    }
}

/// Probe the project root for a package-manager lockfile. First
/// match wins; order is **bun → pnpm → yarn → npm** so a workspace
/// that happens to carry both a stale `package-lock.json` and a
/// fresh `bun.lock` (common while migrating) resolves to `bun`.
///
/// Returns `None` when the project is not on disk yet (e.g. a
/// scratch dir the envelope will populate) or has no lockfile at
/// all. `"auto"` callers then fall through to
/// [`default_package_manager`].
pub fn detect_package_manager(project_dir: &Path) -> Option<PackageManager> {
    // Bun's lockfile is `bun.lock` (text, since 1.2) or `bun.lockb`
    // (binary, pre-1.2). Accept both so we don't fail detection on
    // projects that haven't migrated.
    for name in ["bun.lock", "bun.lockb"] {
        if project_dir.join(name).exists() {
            return Some(PackageManager::Bun);
        }
    }
    if project_dir.join("pnpm-lock.yaml").exists() {
        return Some(PackageManager::Pnpm);
    }
    if project_dir.join("yarn.lock").exists() {
        return Some(PackageManager::Yarn);
    }
    if project_dir.join("package-lock.json").exists() {
        return Some(PackageManager::Npm);
    }
    None
}

/// The fallback manager for `"auto"` callers when
/// [`detect_package_manager`] returns `None`. See module docs for
/// why this is Bun.
pub fn default_package_manager() -> PackageManager {
    PackageManager::Bun
}

/// Resolve a `Settings::autoinstall_package_manager` value into a
/// concrete [`PackageManager`]. The resolution order is:
///
/// 1. If `setting` is a known manager name (`"bun"` / `"npm"` /
///    `"pnpm"` / `"yarn"`), use it verbatim. User intent wins.
/// 2. Otherwise (including `"auto"`, empty, or any unknown value)
///    probe the project lockfile via [`detect_package_manager`].
/// 3. Fall back to [`default_package_manager`] when the probe
///    returns `None`.
pub fn resolve_package_manager(setting: &str, project_dir: &Path) -> PackageManager {
    if let Some(pm) = PackageManager::parse(setting) {
        return pm;
    }
    detect_package_manager(project_dir).unwrap_or_else(default_package_manager)
}

/// Render a deterministic install command for the given manager
/// and package list.
///
/// # Determinism
///
/// The package list is **de-duplicated** and **alphabetically
/// sorted** before rendering, so two calls with the same logical
/// input produce byte-identical output regardless of the iteration
/// order of the upstream `dependency_guard` miss set. This matters
/// for three reasons:
///
/// * The [`security_gate::classify`](crate::security_gate::classify)
///   rule lookup is prefix-based but the reason/`matched_rule`
///   strings include the whole command for telemetry — stable
///   input → stable telemetry.
/// * The `cmd_allow_list` matching in
///   [`run_cmd_gate`](crate::run_cmd_gate) is prefix-based; users
///   who pre-allow e.g. `bun add react react-dom` should get a
///   hit regardless of map-iteration order.
/// * Reprompt loops that retry the same miss list should re-emit
///   the same command so the user's previous approval can be
///   cached by the confirm UI.
///
/// # Quoting
///
/// Package names we accept from the dependency guard are either
/// bare (`react`) or scoped (`@tanstack/react-query`). Both are
/// safe to pass as literal shell tokens — no quoting required and
/// none applied. We explicitly **reject** any package name that
/// contains shell-metacharacters (whitespace, `|`, `&`, `;`, `$`,
/// backticks, `(`, `)`, `<`, `>`, `'`, `"`, or `\`), dropping it
/// from the list. Returning an empty command for a list that
/// contained only rejected names makes the caller fall back to the
/// classic reprompt path instead of issuing e.g. `bun add
/// "; rm -rf /"`.
pub fn synthesise_install_cmd(pm: PackageManager, packages: &[String]) -> String {
    let sanitised: Vec<String> = packages
        .iter()
        .filter(|p| is_safe_package_token(p))
        .cloned()
        .collect();

    // De-dup + sort for determinism.
    let mut deduped = sanitised;
    deduped.sort();
    deduped.dedup();

    if deduped.is_empty() {
        return String::new();
    }

    let subcmd = match pm {
        PackageManager::Bun => "add",
        PackageManager::Npm => "install",
        PackageManager::Pnpm => "add",
        PackageManager::Yarn => "add",
    };

    format!("{} {} {}", pm.as_str(), subcmd, deduped.join(" "))
}

/// True when the package token is structurally safe to pass as a
/// literal shell argument. The allow-set is intentionally
/// conservative: ASCII letters, digits, and the small glyph set
/// that legitimately appears in npm package names (`@`, `/`, `-`,
/// `_`, `.`, `+`). Anything else (whitespace, quotes, shell
/// metacharacters, path separators we don't want, non-ASCII) is
/// rejected.
///
/// A rejected token is dropped silently by
/// [`synthesise_install_cmd`]; the caller treats an empty output
/// as "nothing to install, fall back to the reprompt path".
fn is_safe_package_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // Disallow a leading `-` so a malformed miss can never become
    // a flag (e.g. `--registry=…`).
    if token.starts_with('-') {
        return false;
    }
    token.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '-' | '_' | '.' | '+')
    })
}

/// Terminal verdict of [`try_fix_forward`] — the controller pattern
/// matches on this to decide whether to fall through to the compiler
/// gate (fix-forward, **no** retry slot consumed) or execute the
/// classic `dependency.missing` reprompt path (consumes one slot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixForwardResult {
    /// Auto-install ran, the child exited with code 0, and the
    /// re-run of the dependency guard came back clean (or
    /// intentionally `Skipped` / downgraded to `Warned`). The
    /// envelope is safe to promote to the compiler gate in the
    /// current attempt.
    Resolved,
    /// Auto-install was not attempted or did not fully resolve the
    /// miss set. The controller should emit the classic
    /// `dependency.retry` event, swap in the reprompt, and
    /// `continue` to burn a retry slot.
    ///
    /// The attached `reason` is a short stable tag useful for the
    /// `dependency.retry` payload so UI consumers can tell a
    /// retry-after-failed-install from a retry-after-disabled-gate
    /// without re-deriving it from the event stream.
    NotResolved { reason: &'static str },
}

/// Fix-forward orchestrator for Phase 2.C. Synthesises an install
/// command, routes it through the Phase 2.A classifier and the
/// Phase 2.B executor, and (on success) re-runs the Phase 1.C
/// dependency guard to confirm the miss set is actually resolved.
///
/// All telemetry is emitted via `ai:step` events under
/// `role="autoinstall"`:
///
/// * `autoinstall.skipped` — `reason`: `"disabled"` /
///   `"execute_disabled"` / `"no_missing"` /
///   `"empty_after_sanitize"`.
/// * `autoinstall.attempting` — `pm`, `cmd`, `pkgs`.
/// * `autoinstall.refused` — classifier-dangerous. Not expected
///   for `bun add` / `npm install`; emitted only as a safety net.
/// * `autoinstall.blocked` — `warning_mode=block`.
/// * `autoinstall.user_denied` — confirm modal Deny.
/// * `autoinstall.confirm_timed_out` — confirm modal timed out.
/// * `autoinstall.failed` — exit != 0 on `Executed`.
/// * `autoinstall.error` — infra-level failure from
///   [`run_cmd_gate::execute_run_cmd`] (spawn error, invalid root).
/// * `autoinstall.ok` — exit 0, before the guard re-check runs.
/// * `autoinstall.resolved` — re-check clean, fix-forward.
/// * `autoinstall.ok_but_unresolved` — install succeeded but the
///   guard still sees missing specifiers (e.g. the model asked
///   for a package that doesn't exist on the registry and the
///   manager silently installed nothing).
///
/// All branches except `resolved` return
/// [`FixForwardResult::NotResolved`]; the caller then runs the
/// classic reprompt path.
#[allow(clippy::too_many_arguments)]
pub async fn try_fix_forward(
    app: &AppHandle,
    state: &AppState,
    project_dir: &str,
    envelope: &CodegenEnvelope,
    missing: &[String],
    autoinstall_enabled: bool,
    autoinstall_package_manager: &str,
    security_gate_execute_enabled: bool,
    dep_guard_enabled: bool,
    dep_guard_mode: &str,
    autonomous_confirm: bool,
    attempt: u32,
) -> FixForwardResult {
    // ---- Pre-flight skips (no side effects) ----
    if !autoinstall_enabled {
        let _ = app.emit(
            "ai:step",
            json!({
                "role": "autoinstall",
                "label": "autoinstall.skipped",
                "status": "done",
                "reason": "disabled",
                "attempt": attempt,
            }),
        );
        return FixForwardResult::NotResolved { reason: "disabled" };
    }
    if !security_gate_execute_enabled {
        // We physically cannot install without the Phase 2.B
        // executor. Emit a distinct reason so users can tell the
        // difference between "autoinstall is off" and "autoinstall
        // wants to run but its substrate is off".
        let _ = app.emit(
            "ai:step",
            json!({
                "role": "autoinstall",
                "label": "autoinstall.skipped",
                "status": "done",
                "reason": "execute_disabled",
                "attempt": attempt,
            }),
        );
        return FixForwardResult::NotResolved { reason: "execute_disabled" };
    }
    if missing.is_empty() {
        let _ = app.emit(
            "ai:step",
            json!({
                "role": "autoinstall",
                "label": "autoinstall.skipped",
                "status": "done",
                "reason": "no_missing",
                "attempt": attempt,
            }),
        );
        return FixForwardResult::NotResolved { reason: "no_missing" };
    }

    // ---- Manager + command synthesis (pure) ----
    let project_path = Path::new(project_dir);
    let pm = resolve_package_manager(autoinstall_package_manager, project_path);
    let owned: Vec<String> = missing.to_vec();
    let cmd = synthesise_install_cmd(pm, &owned);
    if cmd.is_empty() {
        // Every miss got dropped as unsafe — defence in depth,
        // don't try to synthesise an install of nothing.
        let _ = app.emit(
            "ai:step",
            json!({
                "role": "autoinstall",
                "label": "autoinstall.skipped",
                "status": "done",
                "reason": "empty_after_sanitize",
                "missing": missing,
                "attempt": attempt,
            }),
        );
        return FixForwardResult::NotResolved { reason: "empty_after_sanitize" };
    }

    let _ = app.emit(
        "ai:step",
        json!({
            "role": "autoinstall",
            "label": "autoinstall.attempting",
            "status": "running",
            "pm": pm.as_str(),
            "cmd": cmd,
            "missing": missing,
            "attempt": attempt,
        }),
    );

    // ---- Execution via the existing Phase 2.A + 2.B pipeline ----
    // Use the same autonomous-confirm override the controller
    // threads to every other `execute_run_cmd` call so a one-off
    // non-autonomous turn can still see the confirm modal.
    let exec = match run_cmd_gate::execute_run_cmd(
        Some(app),
        Some(state),
        project_dir,
        &cmd,
        None,
        Some(autonomous_confirm),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.error",
                    "status": "error",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "error": e,
                    "attempt": attempt,
                }),
            );
            return FixForwardResult::NotResolved { reason: "execution_error" };
        }
    };

    // ---- Dispatch on the terminal execution status ----
    match exec.status {
        ExecutionStatus::Executed if exec.exit_code == 0 => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.ok",
                    "status": "done",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "exit_code": exec.exit_code,
                    "duration_ms": exec.duration_ms,
                    "attempt": attempt,
                }),
            );
            recheck_guard(
                app,
                project_path,
                envelope,
                dep_guard_enabled,
                dep_guard_mode,
                pm,
                &cmd,
                missing,
                attempt,
            )
            .await
        }
        ExecutionStatus::Executed => {
            // Non-zero exit — install failed at runtime
            // (network, version conflict, registry error, etc).
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.failed",
                    "status": "failed",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "exit_code": exec.exit_code,
                    "stderr_tail": exec.stderr_tail,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "install_failed" }
        }
        ExecutionStatus::RefusedDangerous => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.refused",
                    "status": "blocked",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "reason": exec.reason,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "refused" }
        }
        ExecutionStatus::BlockedByPolicy => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.blocked",
                    "status": "blocked",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "reason": exec.reason,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "blocked" }
        }
        ExecutionStatus::UserDenied => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.user_denied",
                    "status": "skipped",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "user_denied" }
        }
        ExecutionStatus::ConfirmTimedOut => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.confirm_timed_out",
                    "status": "skipped",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "confirm_timed_out" }
        }
        ExecutionStatus::Skipped => {
            // The gate short-circuited before classifying — the
            // run_cmd was empty or Phase 2.B was off (we already
            // guarded the latter above, so this is an empty-cmd
            // regression guard).
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.skipped",
                    "status": "done",
                    "reason": exec.reason,
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "execution_skipped" }
        }
    }
}

/// Re-run the Phase 1.C dependency guard against the original
/// envelope after a successful install. If the miss set now
/// resolves (or the guard intentionally skips / downgrades to
/// Warned) the caller can promote the envelope to the compiler
/// gate without burning a retry slot. Otherwise we emit an
/// `autoinstall.ok_but_unresolved` telemetry event and fall back
/// to the classic reprompt path.
#[allow(clippy::too_many_arguments)]
async fn recheck_guard(
    app: &AppHandle,
    project_path: &Path,
    envelope: &CodegenEnvelope,
    dep_guard_enabled: bool,
    dep_guard_mode: &str,
    pm: PackageManager,
    cmd: &str,
    missing: &[String],
    attempt: u32,
) -> FixForwardResult {
    match dependency_guard::check_envelope(project_path, envelope, dep_guard_enabled, dep_guard_mode).await {
        Ok(GuardOutcome::Ok { .. })
        | Ok(GuardOutcome::Skipped { .. })
        | Ok(GuardOutcome::Warned { .. }) => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.resolved",
                    "status": "done",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "installed": missing,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::Resolved
        }
        Ok(GuardOutcome::Missing { missing: still_missing, .. }) => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.ok_but_unresolved",
                    "status": "warning",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "still_missing": still_missing,
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "still_missing_after_install" }
        }
        Err(e) => {
            let _ = app.emit(
                "ai:step",
                json!({
                    "role": "autoinstall",
                    "label": "autoinstall.error",
                    "status": "error",
                    "pm": pm.as_str(),
                    "cmd": cmd,
                    "error": format!("guard re-check failed: {e}"),
                    "attempt": attempt,
                }),
            );
            FixForwardResult::NotResolved { reason: "recheck_error" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ------------------------------------------------------------------
    // PackageManager::parse
    // ------------------------------------------------------------------

    #[test]
    fn parse_accepts_canonical_lowercase() {
        assert_eq!(PackageManager::parse("bun"), Some(PackageManager::Bun));
        assert_eq!(PackageManager::parse("npm"), Some(PackageManager::Npm));
        assert_eq!(PackageManager::parse("pnpm"), Some(PackageManager::Pnpm));
        assert_eq!(PackageManager::parse("yarn"), Some(PackageManager::Yarn));
    }

    #[test]
    fn parse_is_case_insensitive_and_trims() {
        assert_eq!(PackageManager::parse("  BUN  "), Some(PackageManager::Bun));
        assert_eq!(PackageManager::parse("Npm"), Some(PackageManager::Npm));
        assert_eq!(PackageManager::parse("PNPM"), Some(PackageManager::Pnpm));
    }

    #[test]
    fn parse_rejects_auto_empty_and_unknown() {
        assert_eq!(PackageManager::parse(""), None);
        assert_eq!(PackageManager::parse("auto"), None);
        assert_eq!(PackageManager::parse("deno"), None);
        assert_eq!(PackageManager::parse("cargo"), None);
    }

    // ------------------------------------------------------------------
    // detect_package_manager — lockfile probe
    // ------------------------------------------------------------------

    #[test]
    fn detect_bun_lock_text() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("bun.lock"), "").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Bun)
        );
    }

    #[test]
    fn detect_bun_lockb_binary() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("bun.lockb"), "").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Bun)
        );
    }

    #[test]
    fn detect_pnpm_lock() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Pnpm)
        );
    }

    #[test]
    fn detect_yarn_lock() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("yarn.lock"), "").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Yarn)
        );
    }

    #[test]
    fn detect_npm_lock() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Npm)
        );
    }

    #[test]
    fn detect_none_when_no_lockfile() {
        let tmp = tempdir().unwrap();
        assert_eq!(detect_package_manager(tmp.path()), None);
    }

    #[test]
    fn detect_bun_wins_over_stale_package_lock() {
        // Common during migrations: keep the old npm lockfile around
        // while adopting bun. Detection must honour the newer bun
        // lock or we will install into the wrong tree.
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("bun.lock"), "").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Bun)
        );
    }

    // ------------------------------------------------------------------
    // resolve_package_manager — setting + probe + fallback
    // ------------------------------------------------------------------

    #[test]
    fn resolve_explicit_setting_wins_over_lockfile() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("bun.lock"), "").unwrap();
        assert_eq!(
            resolve_package_manager("npm", tmp.path()),
            PackageManager::Npm
        );
    }

    #[test]
    fn resolve_auto_uses_lockfile() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            resolve_package_manager("auto", tmp.path()),
            PackageManager::Pnpm
        );
    }

    #[test]
    fn resolve_auto_with_no_lockfile_falls_back_to_bun() {
        let tmp = tempdir().unwrap();
        assert_eq!(
            resolve_package_manager("auto", tmp.path()),
            PackageManager::Bun
        );
    }

    #[test]
    fn resolve_unknown_setting_is_treated_as_auto() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("yarn.lock"), "").unwrap();
        assert_eq!(
            resolve_package_manager("deno", tmp.path()),
            PackageManager::Yarn
        );
    }

    // ------------------------------------------------------------------
    // synthesise_install_cmd — determinism + quoting
    // ------------------------------------------------------------------

    #[test]
    fn synthesise_bun_uses_add_subcommand() {
        let cmd = synthesise_install_cmd(PackageManager::Bun, &["react".to_string()]);
        assert_eq!(cmd, "bun add react");
    }

    #[test]
    fn synthesise_npm_uses_install_subcommand() {
        let cmd = synthesise_install_cmd(PackageManager::Npm, &["react".to_string()]);
        assert_eq!(cmd, "npm install react");
    }

    #[test]
    fn synthesise_pnpm_uses_add_subcommand() {
        let cmd = synthesise_install_cmd(PackageManager::Pnpm, &["react".to_string()]);
        assert_eq!(cmd, "pnpm add react");
    }

    #[test]
    fn synthesise_yarn_uses_add_subcommand() {
        let cmd = synthesise_install_cmd(PackageManager::Yarn, &["react".to_string()]);
        assert_eq!(cmd, "yarn add react");
    }

    #[test]
    fn synthesise_sorts_and_dedupes() {
        let cmd = synthesise_install_cmd(
            PackageManager::Bun,
            &[
                "zustand".to_string(),
                "react".to_string(),
                "react".to_string(),
                "@tanstack/react-query".to_string(),
            ],
        );
        // Scoped packages sort before bare ones because `@` < `a-z`.
        assert_eq!(cmd, "bun add @tanstack/react-query react zustand");
    }

    #[test]
    fn synthesise_handles_scoped_packages_unquoted() {
        // Scoped names don't need shell quoting — they're made of
        // safe characters. The render stays one token per name.
        let cmd = synthesise_install_cmd(
            PackageManager::Npm,
            &["@types/node".to_string(), "@tanstack/query-core".to_string()],
        );
        assert_eq!(cmd, "npm install @tanstack/query-core @types/node");
    }

    #[test]
    fn synthesise_drops_packages_with_shell_metacharacters() {
        // Defence-in-depth: the dependency guard already normalises
        // specifiers to package roots, but if a malicious envelope
        // somehow slips e.g. a backtick-laden name through we must
        // never forward it to the shell.
        let cmd = synthesise_install_cmd(
            PackageManager::Bun,
            &[
                "react".to_string(),
                "evil;rm".to_string(),
                "`backticks`".to_string(),
                "with space".to_string(),
                "quote\"name".to_string(),
                "with\\backslash".to_string(),
                "$(sub)".to_string(),
            ],
        );
        assert_eq!(cmd, "bun add react");
    }

    #[test]
    fn synthesise_drops_flag_shaped_tokens() {
        let cmd = synthesise_install_cmd(
            PackageManager::Bun,
            &["-g".to_string(), "--registry=https://evil.example".to_string()],
        );
        assert_eq!(cmd, "");
    }

    #[test]
    fn synthesise_empty_input_yields_empty_output() {
        assert_eq!(synthesise_install_cmd(PackageManager::Bun, &[]), "");
    }

    #[test]
    fn synthesise_non_ascii_names_are_rejected() {
        // Real npm names are ASCII-lowercase-only per the registry
        // rules. A non-ASCII name is either a typo or an attack
        // surface; drop it rather than pass it through.
        let cmd = synthesise_install_cmd(
            PackageManager::Bun,
            &["rëact".to_string(), "react".to_string()],
        );
        assert_eq!(cmd, "bun add react");
    }

}
