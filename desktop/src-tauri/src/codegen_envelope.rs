//! OC-Titan codegen envelope (Phase 1.A, V6 §I.1).
//!
//! Deterministic multi-file output contract for any model turn that
//! produces project files. The canonical JSON Schema lives in
//! `schemas/codegen_envelope.json` and is compiled once at startup.
//!
//! Contract (also encoded in the schema):
//! ```json
//! {
//!   "files": [{ "path": "src/app.ts", "content": "…" }],
//!   "run_cmd": "bun run build"   // optional
//! }
//! ```
//!
//! Hard rules enforced by this module (all violations are surfaced with a
//! JSON Pointer so the self-heal retry can feed the exact location back to
//! the model):
//! - Top-level keys: only `files` (required) and `run_cmd` (optional).
//! - `files` is non-empty, at most 256 entries.
//! - Each `path` is POSIX-style, sandbox-relative (no leading `/`), no
//!   NUL bytes, and does not contain a `..` segment. Absolute paths and
//!   traversal are ALSO rejected again at apply time by `fs_ops::resolve`
//!   — this is intentional defense-in-depth.
//! - Each `content` is a string up to 4 MiB.
//! - `run_cmd` (if present) is a single-line string up to 2 KiB. Phase 1
//!   captures and surfaces it but **never auto-executes**.
//!
//! The strict path-safety check goes beyond what JSON Schema's
//! `pattern` can express, so it lives in Rust (`validate_path`). JSON
//! Schema still catches 95 % of nonsense (empty strings, wrong types,
//! extra keys) with a single cheap call.

use std::path::Component;
use std::sync::OnceLock;

use jsonschema::{Draft, JSONSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical JSON Schema for the envelope. Embedded at compile time so
/// the binary is self-contained and the schema cannot drift from the
/// validator at runtime.
pub const SCHEMA_JSON: &str = include_str!("schemas/codegen_envelope.json");

fn compiled_schema() -> &'static JSONSchema {
    static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let schema_value: Value = serde_json::from_str(SCHEMA_JSON)
            .expect("codegen_envelope.json must be valid JSON at compile time");
        JSONSchema::options()
            .with_draft(Draft::Draft202012)
            .compile(&schema_value)
            .expect("codegen_envelope.json must be a valid JSON Schema")
    })
}

/// One file entry in the envelope. Mirrors the JSON shape directly so
/// callers can apply it through `fs_ops::write_file` without
/// re-destructuring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvelopeFile {
    pub path: String,
    pub content: String,
}

/// Validated envelope. Only constructable via [`parse_and_validate`], so
/// every `CodegenEnvelope` value in the system has already passed the
/// schema + path-safety checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodegenEnvelope {
    pub files: Vec<EnvelopeFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_cmd: Option<String>,
}

/// Schema violation with a JSON Pointer into the offending payload. The
/// pointer is what the self-heal reprompt feeds back to the model.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SchemaError {
    /// RFC 6901 JSON Pointer (e.g. `/files/2/path`). Empty string ==
    /// whole document.
    pub pointer: String,
    /// Human-readable reason. Kept short so it fits in an SSE frame.
    pub reason: String,
}

/// Everything that can go wrong before we hand the envelope to
/// `fs_ops::write_file`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum ParseError {
    /// Response body was not valid JSON at all.
    NotJson { reason: String },
    /// JSON parsed but failed the canonical schema.
    SchemaViolations { violations: Vec<SchemaError> },
    /// Extra Rust-side path-safety checks (absolute paths, `..`
    /// traversal, NUL bytes, Windows drive letters). JSON Schema's
    /// `pattern` catches the common cases; this catches the rest.
    UnsafePath { index: usize, path: String, reason: String },
}

impl ParseError {
    /// Render the error as a terse bullet list the model can re-ingest.
    /// Example output:
    /// ```text
    /// - /files/0/path: path must not start with '/'
    /// - /files/2: missing required property 'content'
    /// ```
    pub fn to_feedback(&self) -> String {
        match self {
            ParseError::NotJson { reason } => format!("- /: response was not valid JSON: {reason}"),
            ParseError::SchemaViolations { violations } => violations
                .iter()
                .map(|v| {
                    let ptr = if v.pointer.is_empty() { "/" } else { v.pointer.as_str() };
                    format!("- {ptr}: {}", v.reason)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            ParseError::UnsafePath { index, path, reason } => {
                format!("- /files/{index}/path: unsafe path {path:?}: {reason}")
            }
        }
    }
}

/// Parse the model's raw response into a validated envelope.
///
/// Accepts either a bare JSON object or a JSON object with leading /
/// trailing whitespace. Does NOT accept markdown code fences — the V6
/// directive requires raw JSON and we fail loudly if the model drifts
/// (the repair loop in `ai.rs` will retry once with an explicit
/// reprompt that names the violating pointer).
pub fn parse_and_validate(raw: &str) -> Result<CodegenEnvelope, ParseError> {
    let trimmed = raw.trim();
    let value: Value = serde_json::from_str(trimmed).map_err(|e| ParseError::NotJson {
        reason: e.to_string(),
    })?;

    // 1) Canonical JSON Schema pass.
    if let Err(errors) = compiled_schema().validate(&value) {
        let violations: Vec<SchemaError> = errors
            .map(|e| SchemaError {
                pointer: e.instance_path.to_string(),
                reason: e.to_string(),
            })
            .collect();
        if !violations.is_empty() {
            return Err(ParseError::SchemaViolations { violations });
        }
    }

    // 2) Deserialize into typed struct. Schema already guaranteed the
    //    shape, so this should never fail — but we still surface a
    //    friendly error if it does instead of panicking.
    let envelope: CodegenEnvelope = serde_json::from_value(value).map_err(|e| {
        ParseError::SchemaViolations {
            violations: vec![SchemaError {
                pointer: String::new(),
                reason: format!("envelope shape post-schema deserialisation failed: {e}"),
            }],
        }
    })?;

    // 3) Extra path-safety pass. JSON Schema's `pattern` rejects leading
    //    '/' and NUL bytes; we still need to reject `..` traversal and
    //    Windows drive-letter absolutes.
    for (idx, f) in envelope.files.iter().enumerate() {
        if let Err(reason) = validate_path(&f.path) {
            return Err(ParseError::UnsafePath {
                index: idx,
                path: f.path.clone(),
                reason,
            });
        }
    }

    Ok(envelope)
}

/// Reject paths that JSON Schema's `pattern` cannot express cleanly:
/// - `..` components (parent traversal) anywhere in the path.
/// - Windows absolute paths (`C:\...`, `\\server\share\...`).
/// - Paths that parse as absolute even though they don't start with `/`
///   (e.g. on Windows, `C:foo` is drive-relative).
fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path is empty".into());
    }
    if path.contains('\0') {
        return Err("path contains NUL byte".into());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("path must be sandbox-relative (no leading '/' or '\\')".into());
    }
    // Windows drive-letter prefix: `C:`, `c:`, `Z:/...` etc.
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err("Windows drive-letter paths are not allowed (must be sandbox-relative)".into());
    }
    // `..` traversal — check via `Path::components` so we catch both
    // `../foo` and `foo/../bar`.
    let p = std::path::Path::new(path);
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                return Err("path must not contain '..' (parent traversal)".into());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be sandbox-relative".into());
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_compiles() {
        // Touches the OnceLock so a malformed schema would panic in CI,
        // not at first real use.
        let _ = compiled_schema();
    }

    #[test]
    fn accepts_minimal_valid_envelope() {
        let raw = r#"{"files":[{"path":"src/app.ts","content":"export const x = 1;"}]}"#;
        let env = parse_and_validate(raw).unwrap();
        assert_eq!(env.files.len(), 1);
        assert_eq!(env.files[0].path, "src/app.ts");
        assert!(env.run_cmd.is_none());
    }

    #[test]
    fn accepts_envelope_with_run_cmd() {
        let raw = r##"{"files":[{"path":"README.md","content":"# hi"}],"run_cmd":"bun run build"}"##;
        let env = parse_and_validate(raw).unwrap();
        assert_eq!(env.run_cmd.as_deref(), Some("bun run build"));
    }

    #[test]
    fn accepts_whitespace_wrapped_json() {
        let raw = "\n\n  {\"files\":[{\"path\":\"a.txt\",\"content\":\"hi\"}]}  \n";
        let env = parse_and_validate(raw).unwrap();
        assert_eq!(env.files[0].path, "a.txt");
    }

    #[test]
    fn rejects_markdown_fence() {
        let raw = "```json\n{\"files\":[{\"path\":\"a\",\"content\":\"b\"}]}\n```";
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::NotJson { .. } => {}
            other => panic!("expected NotJson, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_files_array() {
        let raw = r#"{"files":[]}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::SchemaViolations { violations } => {
                assert!(
                    violations.iter().any(|v| v.pointer.contains("files")),
                    "violations did not mention files: {violations:?}"
                );
            }
            other => panic!("expected SchemaViolations, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_content() {
        let raw = r#"{"files":[{"path":"a.txt"}]}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::SchemaViolations { .. } => {}
            other => panic!("expected SchemaViolations, got {other:?}"),
        }
    }

    #[test]
    fn rejects_extra_top_level_key() {
        let raw = r#"{"files":[{"path":"a","content":"b"}],"extra":"x"}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::SchemaViolations { violations } => {
                assert!(violations
                    .iter()
                    .any(|v| v.reason.to_lowercase().contains("additional")
                        || v.reason.to_lowercase().contains("extra")
                        || v.reason.contains("extra")));
            }
            other => panic!("expected SchemaViolations, got {other:?}"),
        }
    }

    #[test]
    fn rejects_absolute_path_via_schema() {
        let raw = r#"{"files":[{"path":"/etc/passwd","content":"x"}]}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::SchemaViolations { .. } | ParseError::UnsafePath { .. } => {}
            other => panic!("expected SchemaViolations or UnsafePath, got {other:?}"),
        }
    }

    #[test]
    fn rejects_parent_traversal() {
        let raw = r#"{"files":[{"path":"../secrets","content":"x"}]}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::UnsafePath { reason, .. } => {
                assert!(reason.contains(".."));
            }
            other => panic!("expected UnsafePath, got {other:?}"),
        }
    }

    #[test]
    fn rejects_nested_parent_traversal() {
        let raw = r#"{"files":[{"path":"src/../../etc/passwd","content":"x"}]}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::UnsafePath { .. } => {}
            other => panic!("expected UnsafePath, got {other:?}"),
        }
    }

    #[test]
    fn rejects_windows_drive_path() {
        let raw = r#"{"files":[{"path":"C:\\Windows\\System32","content":"x"}]}"#;
        let err = parse_and_validate(raw).unwrap_err();
        match err {
            ParseError::UnsafePath { .. } => {}
            other => panic!("expected UnsafePath, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_json() {
        let err = parse_and_validate("not even close to json").unwrap_err();
        match err {
            ParseError::NotJson { .. } => {}
            other => panic!("expected NotJson, got {other:?}"),
        }
    }

    #[test]
    fn feedback_format_is_stable() {
        let err = ParseError::SchemaViolations {
            violations: vec![SchemaError {
                pointer: "/files/0/path".into(),
                reason: "must not be empty".into(),
            }],
        };
        let s = err.to_feedback();
        assert!(s.starts_with("- /files/0/path:"));
    }
}
