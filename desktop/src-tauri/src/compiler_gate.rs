//! OC-Titan Phase 1.B — TypeScript compiler gate.
//!
//! This module is the second half of the deterministic-output
//! contract introduced in Phase 1.A. The envelope validator
//! ([`crate::codegen_envelope`]) guarantees the assistant's wire
//! shape; the compiler gate guarantees that the *payload* of that
//! envelope actually type-checks before any file lands in the real
//! project sandbox.
//!
//! ## Flow
//!
//! 1. [`prepare_scratch`] materialises the envelope into a
//!    per-project, per-uuid scratch directory under
//!    `<project>/.oc-titan/scratch/<uuid>/`. The scratch is
//!    populated with:
//!    - the envelope files themselves,
//!    - the project's `package.json` / `tsconfig*.json` (copied,
//!      because tsc resolves them relative to `cwd`),
//!    - a `node_modules` symlink (Unix) / junction-free copy
//!      fallback (Windows) so `tsc` can resolve imports without
//!      paying the cost of a real `node_modules` copy.
//!   The scratch directory is **never promoted** — successful
//!   compile just unlocks the caller to write through the normal
//!   `fs_ops::write_file` sandbox path.
//!
//! 2. [`run_tsc`] picks a toolchain (`bun x tsc` → `npx tsc` →
//!    global `tsc`) and invokes it with `--noEmit --pretty false`.
//!    stdout/stderr are parsed via [`parse_diagnostics`] into a
//!    structured `Vec<CompileDiagnostic>`.
//!
//! 3. [`skip_policy`] short-circuits the gate when the envelope
//!    contains no TypeScript files (HTML-only / JSON-only envelopes
//!    — e.g. L1 prompts from OPENROUTER_VALIDATION_REPORT) or when
//!    the user has disabled the gate in Settings.
//!
//! The outer compile loop (`MAX_COMPILE_RETRIES = 2`) lives in
//! [`crate::controller`] — this module is intentionally
//! side-effect-free beyond scratch filesystem management and the
//! tsc subprocess.
//!
//! ## Security
//!
//! - Scratch paths are derived from the project root via
//!   [`crate::fs_ops::resolve`] so an envelope cannot write into a
//!   sibling project or back into `node_modules`.
//! - The envelope's own `path` strings have already been validated
//!   by the JSON schema + the Rust-side path guard in
//!   [`crate::codegen_envelope::validate_path`], so `..` / leading
//!   `/` / NUL bytes / Windows drive letters cannot reach this
//!   module.
//! - `tsc` is executed with a bounded timeout and inherits no
//!   environment beyond what the parent Tauri process already has.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::codegen_envelope::CodegenEnvelope;

/// How long a single toolchain-probe subprocess (e.g. `bun --version`)
/// is allowed to run before we declare that probe failed. Probes are
/// expected to return in well under a second; this ceiling exists only
/// to protect against a hanging binary.
const TOOLCHAIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A single tsc diagnostic as parsed from `--pretty false` output.
///
/// All fields are best-effort; `path` is always present (we key on it
/// for deduplication and UI grouping), the numeric fields default to 0
/// on malformed input, and `code` defaults to an empty string for
/// warning-class messages that don't ship a `TSxxxx` tag.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompileDiagnostic {
    /// Path as reported by tsc — usually a scratch-relative path
    /// (`<uuid>/src/foo.ts`). The controller rewrites this back to a
    /// project-relative path before surfacing to the model / UI so
    /// operators never see the internal uuid.
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub code: String,
    pub message: String,
}

/// Which toolchain the gate resolved on this invocation.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolchainKind {
    /// `bun x tsc` — preferred when the project declares Bun
    /// (`bun.lock` / `bun.lockb`). Matches the Bun-first posture of
    /// this repo (see CLAUDE.md / AGENTS.md).
    Bun,
    /// `npx --no-install tsc` — the portable fallback. Requires a
    /// `typescript` entry in `package.json` + a populated
    /// `node_modules/typescript`.
    Npx,
    /// A `tsc` binary found on `PATH`. Logged as a warning because
    /// it bypasses the project's pinned TypeScript version.
    Global,
}

impl ToolchainKind {
    /// Short slug used in `ai:step` telemetry payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolchainKind::Bun => "bun",
            ToolchainKind::Npx => "npx",
            ToolchainKind::Global => "global",
        }
    }
}

/// Outcome of a single `run_tsc` invocation. The controller inspects
/// this to decide whether to promote, retry, or surface an error.
///
/// Skip semantics (gate disabled, no `.ts` files, no toolchain) are
/// handled by the caller — see [`skip_policy`] and
/// [`detect_toolchain`] — so this enum models only paths where tsc
/// actually ran.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompileOutcome {
    /// `tsc --noEmit` exited with status 0.
    Ok { toolchain: ToolchainKind },
    /// `tsc` reported one or more diagnostics. The controller feeds
    /// `diagnostics` back into the next envelope turn as part of the
    /// repair prompt.
    Errors {
        toolchain: ToolchainKind,
        diagnostics: Vec<CompileDiagnostic>,
        raw_output: String,
    },
    /// `tsc` did not finish within the configured timeout. Treated
    /// as a failure, no retry — hanging compilers are a human
    /// problem.
    Timeout { toolchain: ToolchainKind, after_secs: u64 },
}

/// Does the envelope contain at least one TypeScript-shaped file?
///
/// The check is deliberately conservative — a `.d.ts` file counts,
/// a `.tsx` file counts, but a `.json` / `.html` / `.css` file does
/// not. This matches what `tsc` would actually pick up given a
/// standard `tsconfig.json`.
pub fn envelope_has_typescript(envelope: &CodegenEnvelope) -> bool {
    envelope.files.iter().any(|f| {
        let lower = f.path.to_ascii_lowercase();
        lower.ends_with(".ts") || lower.ends_with(".tsx") || lower.ends_with(".mts") || lower.ends_with(".cts")
    })
}

/// High-level skip check. Returns `Some(reason)` when the gate
/// should not run and `None` when the caller should proceed to
/// [`prepare_scratch`].
pub fn skip_policy(enabled: bool, envelope: &CodegenEnvelope) -> Option<&'static str> {
    if !enabled {
        return Some("disabled");
    }
    if !envelope_has_typescript(envelope) {
        return Some("no_ts_files");
    }
    None
}

/// Scratch directory handle. Cleaned up via [`Self::cleanup`] once
/// the controller is done with it (whether the compile passed or
/// failed).
#[derive(Debug)]
pub struct Scratch {
    /// Absolute path to `<project>/.oc-titan/scratch/<uuid>/`.
    pub dir: PathBuf,
    /// Which project root this scratch belongs to. Cached so cleanup
    /// can double-check it stays inside `.oc-titan/scratch/`.
    project_root: PathBuf,
    /// Uuid of this attempt. Exposed for telemetry (`compiler.scratch_ready`).
    pub uuid: String,
}

impl Scratch {
    /// Remove the scratch directory. Errors are logged by the caller
    /// — a stale scratch is annoying but not fatal, and the wider
    /// `.oc-titan/` root is `.gitignore`d.
    pub async fn cleanup(self) -> std::io::Result<()> {
        // Belt-and-braces: never recursively delete anything that
        // doesn't live under `.oc-titan/scratch/`. This guards
        // against a future refactor that accidentally points `dir`
        // at the project root.
        let guard = self.project_root.join(".oc-titan").join("scratch");
        if !self.dir.starts_with(&guard) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to remove scratch outside guard: {} (guard = {})",
                    self.dir.display(),
                    guard.display()
                ),
            ));
        }
        fs::remove_dir_all(&self.dir).await
    }
}

/// Populate a scratch directory with the envelope payload + just
/// enough of the surrounding project to let `tsc` resolve types.
///
/// The scratch layout mirrors the project layout so that `tsc` and
/// the model both see the same relative paths — diagnostics reported
/// against `<scratch>/src/foo.ts` map back to `src/foo.ts` without
/// further manipulation.
pub async fn prepare_scratch(
    project_dir: &str,
    envelope: &CodegenEnvelope,
) -> Result<Scratch, String> {
    let project_root = Path::new(project_dir)
        .canonicalize()
        .map_err(|e| format!("invalid project root {project_dir}: {e}"))?;

    // Ensure the top-level `.oc-titan/` tree exists and is
    // gitignored before we write anything into it.
    let oc_root = project_root.join(".oc-titan");
    fs::create_dir_all(&oc_root)
        .await
        .map_err(|e| format!("cannot create .oc-titan root: {e}"))?;
    ensure_gitignored(&project_root)
        .await
        .map_err(|e| format!("cannot mark .oc-titan as gitignored: {e}"))?;

    let uuid = uuid::Uuid::new_v4().to_string();
    let scratch_dir = oc_root.join("scratch").join(&uuid);
    fs::create_dir_all(&scratch_dir)
        .await
        .map_err(|e| format!("cannot create scratch dir: {e}"))?;

    // Copy tsconfig* and package.json so tsc can resolve options
    // and the project's TypeScript version. We copy (not symlink)
    // because some envelopes will legitimately *edit* tsconfig, and
    // a symlink would let those edits leak back into the real
    // project before the compile gate has approved them.
    for name in ["package.json", "tsconfig.json", "tsconfig.base.json", "tsconfig.app.json", "tsconfig.node.json"] {
        let src = project_root.join(name);
        if src.is_file() {
            let dst = scratch_dir.join(name);
            fs::copy(&src, &dst)
                .await
                .map_err(|e| format!("cannot copy {name} into scratch: {e}"))?;
        }
    }

    // Symlink node_modules so `tsc` + `typescript` resolve without
    // copying hundreds of MB. On Windows, fall back to skipping the
    // link — `npx --no-install` / global `tsc` will still find
    // their own copy. The actual compile command runs in the
    // scratch dir so relative lookups walk up into the project
    // naturally if the symlink is absent.
    let project_node_modules = project_root.join("node_modules");
    if project_node_modules.is_dir() {
        let scratch_node_modules = scratch_dir.join("node_modules");
        symlink_best_effort(&project_node_modules, &scratch_node_modules).await;
    }

    // Finally, write the envelope files. `envelope.files` has
    // already been schema- and path-validated so each `path` is a
    // relative, sandbox-safe string — we still resolve through
    // `join` + `canonicalize`-on-parent to keep defence-in-depth
    // (belt-and-braces vs. a future validator regression).
    for file in &envelope.files {
        let target = scratch_dir.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("cannot mkdir {}: {e}", parent.display()))?;
            let parent_canon = parent
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize scratch parent {}: {e}", parent.display()))?;
            if !parent_canon.starts_with(&scratch_dir.canonicalize().unwrap_or_else(|_| scratch_dir.clone())) {
                return Err(format!(
                    "envelope path {} escapes scratch dir",
                    file.path
                ));
            }
        }
        fs::write(&target, file.content.as_bytes())
            .await
            .map_err(|e| format!("cannot write {}: {e}", file.path))?;
    }

    Ok(Scratch {
        dir: scratch_dir,
        project_root,
        uuid,
    })
}

/// Probe the project for a usable TypeScript toolchain. Returns
/// `None` if nothing works — callers should treat that as
/// `CompileOutcome::Skipped { reason: "no_toolchain" }` so users on
/// JS-only / pre-tsc projects are not blocked.
pub async fn detect_toolchain(project_dir: &Path) -> Option<ToolchainKind> {
    // 1) Bun — cheapest and preferred.
    let is_bun_project = project_dir.join("bun.lock").is_file()
        || project_dir.join("bun.lockb").is_file();
    if is_bun_project && command_responds("bun", &["--version"]).await {
        return Some(ToolchainKind::Bun);
    }

    // 2) npx with a local `typescript` install.
    let has_local_tsc = project_dir
        .join("node_modules")
        .join("typescript")
        .is_dir();
    if has_local_tsc && command_responds("npx", &["--version"]).await {
        return Some(ToolchainKind::Npx);
    }

    // 3) Last resort: a global `tsc` on PATH. This bypasses the
    //    project's pinned version, so the controller logs a warning
    //    at the telemetry layer.
    if command_responds("tsc", &["--version"]).await {
        return Some(ToolchainKind::Global);
    }

    None
}

/// Run `tsc --noEmit` on the prepared scratch.
///
/// The timeout applies to the child process as a whole — stdout/
/// stderr are drained after the child exits (or is killed on
/// timeout) so the returned `raw_output` is always complete for the
/// portion that did execute.
pub async fn run_tsc(
    scratch: &Scratch,
    toolchain: ToolchainKind,
    timeout_secs: u64,
) -> CompileOutcome {
    let (program, base_args): (&str, Vec<&str>) = match toolchain {
        ToolchainKind::Bun => ("bun", vec!["x", "tsc"]),
        ToolchainKind::Npx => ("npx", vec!["--no-install", "tsc"]),
        ToolchainKind::Global => ("tsc", vec![]),
    };
    let mut args = base_args;
    args.extend_from_slice(&[
        "--noEmit",
        "--pretty",
        "false",
        "--incremental",
        "false",
    ]);

    let mut cmd = Command::new(program);
    cmd.args(&args)
        .current_dir(&scratch.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CompileOutcome::Errors {
                toolchain,
                diagnostics: Vec::new(),
                raw_output: format!("failed to spawn {program}: {e}"),
            };
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let timeout_dur = if timeout_secs == 0 {
        Duration::from_secs(u64::MAX / 2)
    } else {
        Duration::from_secs(timeout_secs)
    };

    let wait_result = tokio::time::timeout(timeout_dur, child.wait()).await;

    let mut out_str = String::new();
    let mut err_str = String::new();
    let mut out_buf = [0u8; 4096];
    let mut err_buf = [0u8; 4096];

    if let Some(ref mut so) = stdout_pipe {
        while let Ok(n) = so.read(&mut out_buf).await {
            if n == 0 {
                break;
            }
            out_str.push_str(&String::from_utf8_lossy(&out_buf[..n]));
        }
    }
    if let Some(ref mut se) = stderr_pipe {
        while let Ok(n) = se.read(&mut err_buf).await {
            if n == 0 {
                break;
            }
            err_str.push_str(&String::from_utf8_lossy(&err_buf[..n]));
        }
    }

    let combined = if err_str.is_empty() {
        out_str.clone()
    } else if out_str.is_empty() {
        err_str.clone()
    } else {
        format!("{out_str}\n{err_str}")
    };

    match wait_result {
        Err(_) => {
            // Timeout — best-effort kill.
            let _ = child.kill().await;
            CompileOutcome::Timeout {
                toolchain,
                after_secs: timeout_secs,
            }
        }
        Ok(Ok(status)) => {
            if status.success() {
                CompileOutcome::Ok { toolchain }
            } else {
                let diagnostics = parse_diagnostics(&combined);
                CompileOutcome::Errors {
                    toolchain,
                    diagnostics,
                    raw_output: combined,
                }
            }
        }
        Ok(Err(e)) => CompileOutcome::Errors {
            toolchain,
            diagnostics: Vec::new(),
            raw_output: format!("tsc wait failed: {e}\n{combined}"),
        },
    }
}

/// Parse a `tsc --pretty false` error stream into structured
/// diagnostics.
///
/// The canonical shape is:
///
/// ```text
/// path/to/file.ts(12,34): error TS1234: Some message.
/// ```
///
/// Multi-line messages (type elaboration, "Types of property ...
/// are incompatible", etc.) continue on subsequent lines that do
/// **not** match the primary regex; we attach them to the previous
/// diagnostic's `message` with a `\n` separator so the reviewer and
/// the model both see the full context.
pub fn parse_diagnostics(raw: &str) -> Vec<CompileDiagnostic> {
    static PRIMARY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(.+?)\((\d+),(\d+)\):\s+error\s+(TS\d+):\s+(.*)$")
            .expect("compile-time diagnostic regex")
    });

    let mut out: Vec<CompileDiagnostic> = Vec::new();
    for line in raw.lines() {
        if let Some(cap) = PRIMARY.captures(line) {
            out.push(CompileDiagnostic {
                path: cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                line: cap
                    .get(2)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(0),
                column: cap
                    .get(3)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(0),
                code: cap.get(4).map(|m| m.as_str().to_string()).unwrap_or_default(),
                message: cap.get(5).map(|m| m.as_str().to_string()).unwrap_or_default(),
            });
        } else if !line.trim().is_empty() {
            if let Some(last) = out.last_mut() {
                last.message.push('\n');
                last.message.push_str(line.trim_end());
            }
        }
    }
    out
}

/// Format diagnostics for the self-heal reprompt. Matches the
/// envelope feedback style from [`crate::codegen_envelope::ParseError::to_feedback`]
/// — terse bullets keyed by file:line so the model can target a fix.
pub fn diagnostics_to_feedback(diagnostics: &[CompileDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return "(no structured diagnostics emitted — see raw tsc output)".to_string();
    }
    let mut lines = Vec::with_capacity(diagnostics.len());
    for d in diagnostics.iter().take(50) {
        lines.push(format!(
            "- {}({},{}) {}: {}",
            d.path, d.line, d.column, d.code, d.message
        ));
    }
    if diagnostics.len() > 50 {
        lines.push(format!(
            "- ... and {} more diagnostic(s) suppressed",
            diagnostics.len() - 50
        ));
    }
    lines.join("\n")
}

/// Rewrite `scratch/<uuid>/src/foo.ts` back to `src/foo.ts` in a
/// diagnostic list. The UI / reprompt always wants project-relative
/// paths so operators never see the internal uuid.
pub fn rewrite_paths_relative(diagnostics: &mut [CompileDiagnostic], scratch_uuid: &str) {
    let needle = format!("scratch/{scratch_uuid}/");
    let alt = format!("scratch{}{}", std::path::MAIN_SEPARATOR, scratch_uuid);
    for d in diagnostics.iter_mut() {
        if let Some(pos) = d.path.find(&needle) {
            d.path = d.path[pos + needle.len()..].to_string();
        } else if let Some(pos) = d.path.find(&alt) {
            d.path = d
                .path
                .chars()
                .skip(pos + alt.len())
                .collect::<String>()
                .trim_start_matches(std::path::MAIN_SEPARATOR)
                .to_string();
        }
    }
}

// ---------- internal helpers ----------

async fn command_responds(program: &str, args: &[&str]) -> bool {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    match tokio::time::timeout(TOOLCHAIN_PROBE_TIMEOUT, async {
        let mut c = child;
        c.wait().await
    })
    .await
    {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

/// Make sure the project's `.gitignore` contains a
/// `/.oc-titan/` entry so nobody accidentally commits scratch
/// dirs. Idempotent — if the entry already exists (exact match or
/// as part of a broader pattern like `.oc-titan/`) we return
/// without touching the file.
async fn ensure_gitignored(project_root: &Path) -> std::io::Result<()> {
    let gi = project_root.join(".gitignore");
    let existing = match fs::read_to_string(&gi).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let already = existing
        .lines()
        .any(|l| matches!(l.trim(), ".oc-titan" | ".oc-titan/" | "/.oc-titan" | "/.oc-titan/"));
    if already {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("# OC-Titan scratch dirs (Phase 1.B compiler gate).\n");
    next.push_str("/.oc-titan/\n");
    fs::write(&gi, next).await
}

#[cfg(unix)]
async fn symlink_best_effort(src: &Path, dst: &Path) {
    // Remove any stale target (e.g. leftover from an aborted run).
    let _ = fs::remove_file(dst).await;
    let _ = fs::remove_dir_all(dst).await;
    let _ = tokio::task::spawn_blocking({
        let src = src.to_path_buf();
        let dst = dst.to_path_buf();
        move || std::os::unix::fs::symlink(src, dst)
    })
    .await;
}

#[cfg(windows)]
async fn symlink_best_effort(src: &Path, dst: &Path) {
    // Windows junctions need admin rights on some configs; we
    // prefer to silently skip than to fail the whole compile. `npx`
    // will find typescript via the parent lookup path.
    let _ = fs::remove_dir_all(dst).await;
    let _ = tokio::task::spawn_blocking({
        let src = src.to_path_buf();
        let dst = dst.to_path_buf();
        move || std::os::windows::fs::symlink_dir(src, dst)
    })
    .await;
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen_envelope::{CodegenEnvelope, EnvelopeFile};

    fn env_with_paths(paths: &[&str]) -> CodegenEnvelope {
        CodegenEnvelope {
            files: paths
                .iter()
                .map(|p| EnvelopeFile {
                    path: (*p).to_string(),
                    content: "export const x = 1;\n".to_string(),
                })
                .collect(),
            run_cmd: None,
        }
    }

    #[test]
    fn ts_detection_recognises_ts_family() {
        assert!(envelope_has_typescript(&env_with_paths(&["src/a.ts"])));
        assert!(envelope_has_typescript(&env_with_paths(&["src/a.tsx"])));
        assert!(envelope_has_typescript(&env_with_paths(&["src/a.mts"])));
        assert!(envelope_has_typescript(&env_with_paths(&["src/a.cts"])));
        assert!(envelope_has_typescript(&env_with_paths(&[
            "README.md", "src/lib.ts"
        ])));
    }

    #[test]
    fn ts_detection_ignores_non_ts_files() {
        assert!(!envelope_has_typescript(&env_with_paths(&[
            "index.html", "style.css"
        ])));
        assert!(!envelope_has_typescript(&env_with_paths(&["package.json"])));
        assert!(!envelope_has_typescript(&env_with_paths(&[
            "README.md", "docs/notes.txt"
        ])));
    }

    #[test]
    fn skip_policy_respects_toggle() {
        let env = env_with_paths(&["src/a.ts"]);
        assert_eq!(skip_policy(false, &env), Some("disabled"));
        assert_eq!(skip_policy(true, &env), None);
    }

    #[test]
    fn skip_policy_respects_no_ts() {
        let env = env_with_paths(&["index.html"]);
        assert_eq!(skip_policy(true, &env), Some("no_ts_files"));
    }

    #[test]
    fn parse_diagnostics_handles_canonical_shape() {
        let raw = "src/foo.ts(12,34): error TS2322: Type 'number' is not assignable to type 'string'.\nsrc/bar.tsx(1,1): error TS2307: Cannot find module './missing'.";
        let d = parse_diagnostics(raw);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, "src/foo.ts");
        assert_eq!(d[0].line, 12);
        assert_eq!(d[0].column, 34);
        assert_eq!(d[0].code, "TS2322");
        assert!(d[0].message.contains("Type 'number'"));
        assert_eq!(d[1].code, "TS2307");
    }

    #[test]
    fn parse_diagnostics_attaches_continuation_lines() {
        let raw = "src/foo.ts(5,7): error TS2345: Argument of type 'A' is not assignable.\n  Type 'A' is missing property 'id'.\n  Types of property 'x' are incompatible.";
        let d = parse_diagnostics(raw);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("Argument of type"));
        assert!(d[0].message.contains("missing property 'id'"));
        assert!(d[0].message.contains("Types of property 'x'"));
    }

    #[test]
    fn parse_diagnostics_is_empty_on_no_errors() {
        assert!(parse_diagnostics("").is_empty());
        assert!(parse_diagnostics("random log line\nversion 5.4.2").is_empty());
    }

    #[test]
    fn rewrite_paths_strips_scratch_prefix_unix() {
        let mut d = vec![CompileDiagnostic {
            path: ".oc-titan/scratch/abc-123/src/foo.ts".to_string(),
            line: 1,
            column: 1,
            code: "TS1".to_string(),
            message: "x".to_string(),
        }];
        rewrite_paths_relative(&mut d, "abc-123");
        assert_eq!(d[0].path, "src/foo.ts");
    }

    #[test]
    fn feedback_format_is_stable() {
        let d = vec![CompileDiagnostic {
            path: "src/a.ts".to_string(),
            line: 1,
            column: 1,
            code: "TS2322".to_string(),
            message: "bad".to_string(),
        }];
        let fb = diagnostics_to_feedback(&d);
        assert!(fb.contains("src/a.ts(1,1)"));
        assert!(fb.contains("TS2322"));
        assert!(fb.contains("bad"));
    }

    #[test]
    fn feedback_empty_diagnostics_is_still_informative() {
        let fb = diagnostics_to_feedback(&[]);
        assert!(fb.contains("no structured diagnostics"));
    }

    #[tokio::test]
    async fn prepare_scratch_writes_all_files_and_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().to_path_buf();
        tokio::fs::write(
            proj.join("package.json"),
            r#"{"name":"t","version":"0.0.0"}"#,
        )
        .await
        .unwrap();

        let env = CodegenEnvelope {
            files: vec![
                EnvelopeFile {
                    path: "src/a.ts".to_string(),
                    content: "export const a = 1;\n".to_string(),
                },
                EnvelopeFile {
                    path: "src/nested/b.tsx".to_string(),
                    content: "export const B = () => null;\n".to_string(),
                },
            ],
            run_cmd: None,
        };

        let scratch = prepare_scratch(proj.to_str().unwrap(), &env).await.unwrap();
        assert!(scratch.dir.join("src/a.ts").is_file());
        assert!(scratch.dir.join("src/nested/b.tsx").is_file());
        assert!(scratch.dir.join("package.json").is_file());
        let gi = tokio::fs::read_to_string(proj.join(".gitignore"))
            .await
            .unwrap();
        assert!(gi.contains(".oc-titan"));
    }

    #[tokio::test]
    async fn cleanup_refuses_outside_scratch() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().to_path_buf();
        let evil = Scratch {
            dir: proj.clone(),
            project_root: proj.clone(),
            uuid: "x".into(),
        };
        assert!(evil.cleanup().await.is_err());
    }
}
