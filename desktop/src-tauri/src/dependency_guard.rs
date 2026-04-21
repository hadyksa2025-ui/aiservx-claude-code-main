//! OC-Titan Phase 1.C — Dependency Guard.
//!
//! This module implements V6 §I.6: **"Validate imports vs
//! `package.json` BEFORE writing. Auto-fix missing dependencies.
//! Fail loudly if unresolved."**
//!
//! ## Where this sits in the pipeline
//!
//! The guard runs *after* envelope validation
//! ([`codegen_envelope::parse_and_validate`]) and *before* the
//! compiler gate ([`compiler_gate::prepare_scratch`]). That ordering
//! matters: the most common L2/L3 failure mode documented in
//! `OPENROUTER_VALIDATION_REPORT` §3 is the model hallucinating a
//! package the project never installed (e.g. `import { create }
//! from "zustand"` with no Zustand in `package.json`). Without the
//! guard these phantoms burn a full compiler-gate retry slot —
//! `tsc` dutifully reports `TS2307 Cannot find module 'zustand'`,
//! the model gets reprompted, and may happily hallucinate the same
//! package again. With the guard we short-circuit that loop and
//! feed the model the *exact* list of missing specifiers before it
//! ever reaches `tsc`.
//!
//! ## What we parse
//!
//! Four import-like shapes, in rough order of frequency:
//!
//! 1. `import … from "foo"` — static ES-module imports,
//!    including `import type`, namespace imports, and default
//!    imports.
//! 2. `import "foo"` — bare side-effect imports.
//! 3. `require("foo")` — CommonJS + some TS interop.
//! 4. `import("foo")` — dynamic `import()` expressions.
//!
//! All four are detected via regex. We intentionally *do not*
//! reach for a full parser (tree-sitter, swc, etc.) — the failure
//! budget of a false positive is "unnecessary warning", which is
//! strictly preferable to pulling a heavyweight dep into the Tauri
//! bundle. The guard strips `//` and `/* */` comments before
//! extraction to avoid matching inside commented-out code.
//!
//! ## What we accept as "resolved"
//!
//! A specifier is considered resolved if any of the following holds:
//!
//! * It is **relative** (`./`, `../`, or a bare slash path). The
//!   compiler gate catches bad relative paths — that's its job.
//! * It is a **Node built-in** (bare name from the canonical list,
//!   or any `node:…` / `bun:…` URI).
//! * Its **package root** is listed in `dependencies`,
//!   `devDependencies`, `peerDependencies`, or
//!   `optionalDependencies` of the project's `package.json`. The
//!   package root is the bare package name (`foo`) or the scoped
//!   form (`@scope/foo`), without any subpath suffix.
//!
//! Everything else is a **miss** and the guard fires.
//!
//! ## What the guard does on a miss
//!
//! Two modes, driven by `Settings::dependency_guard_mode`:
//!
//! * `"fail"` (default, matching V6 §I.6's "fail loudly"): return
//!   [`GuardOutcome::Missing`] so the controller re-prompts the
//!   model with the structured miss list, sharing retry budget with
//!   the compiler gate.
//! * `"warn"`: return [`GuardOutcome::Warned`] and let the envelope
//!   continue to the compiler gate. Useful when bootstrapping a
//!   project whose `package.json` legitimately lags the code.
//!
//! We deliberately do *not* auto-mutate `package.json` in Phase 1.
//! Auto-install is a risky surface (network, lockfile churn,
//! version pinning) that deserves its own command-risk review —
//! slated for Phase 2.
//!
//! ## Safety
//!
//! No filesystem writes. No network. All reads go through `tokio`
//! filesystem primitives and are bounded to the envelope files (in
//! memory, not on disk) plus a single `package.json` read. A
//! malformed `package.json` is treated as "no deps known" rather
//! than propagating an error — the guard must never be the reason
//! a compile refuses to start.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use tokio::fs;

use crate::codegen_envelope::CodegenEnvelope;

/// Outcome of running the guard over a single envelope.
///
/// Designed to be `Serialize` so it can flow straight into the
/// `ai:step` event payloads.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuardOutcome {
    /// Every package specifier resolved. Envelope is safe to
    /// promote to the compiler gate.
    Ok {
        /// Distinct package specifiers that were checked and
        /// resolved. Useful for telemetry.
        resolved: Vec<String>,
    },
    /// The guard was skipped. `reason` is one of:
    /// `"disabled"`, `"no_supported_files"`, `"no_package_json"`.
    Skipped { reason: &'static str },
    /// One or more specifiers didn't resolve. The controller
    /// decides what to do with this based on
    /// `Settings::dependency_guard_mode`.
    Missing {
        /// Package specifiers (post-normalization to the package
        /// root) that didn't map to any `package.json` entry and
        /// aren't Node built-ins.
        missing: Vec<String>,
        /// Full per-file breakdown for the re-prompt. Keyed by
        /// envelope path. Values are the raw specifiers as they
        /// appear in the source.
        per_file: BTreeMap<String, Vec<String>>,
    },
    /// The guard fired but `dependency_guard_mode = "warn"` so the
    /// envelope is allowed through anyway. Structurally identical
    /// to [`GuardOutcome::Missing`] for telemetry purposes.
    Warned {
        missing: Vec<String>,
        per_file: BTreeMap<String, Vec<String>>,
    },
}

/// Extensions we treat as ES-module / CJS source. We keep this
/// list short and explicit rather than "anything that looks like
/// JavaScript" — the guard's false-positive rate scales with the
/// surface area we inspect, and e.g. `.mjs` / `.cjs` aren't worth
/// the boundary-case bugs yet.
const SUPPORTED_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts"];

/// Canonical Node.js built-in modules. Matched against the *bare*
/// form (i.e. `fs`, not `node:fs`). `node:` and `bun:` URIs are
/// also always accepted — see [`is_builtin`].
///
/// Keeping this as a small static is deliberate: it's cheaper and
/// easier to audit than a crate dependency, and the list only
/// grows when Node.js itself grows.
const NODE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "test",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

static BUILTIN_SET: Lazy<HashSet<&'static str>> = Lazy::new(|| NODE_BUILTINS.iter().copied().collect());

// Comment stripping: line comments and block comments. We run
// this *before* the import extractors so a commented-out
// `// import 'foo'` doesn't show up as a phantom.
static COMMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"//[^\n]*|/\*[\s\S]*?\*/"#).expect("COMMENT_RE"));

// `import … from 'foo'` / `import … from "foo"` / `import … from `foo` `
// Also catches `import type { … } from 'foo'` and `export … from 'foo'`.
// The middle section excludes quotes and `;` so we don't bridge
// across sibling import statements, but deliberately DOES allow
// newlines — LLM-generated code frequently splits destructured
// imports over multiple lines:
//
//     import {
//       useState,
//       useEffect,
//     } from 'react';
//
// Earlier versions of this regex excluded `\n` and silently missed
// every such import (Devin Review PR #4 comment 3120401607).
static FROM_IMPORT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\b(?:import|export)\s+[^'"`;]*?\bfrom\s*(['"`])([^'"`]+)['"`]"#)
        .expect("FROM_IMPORT_RE")
});

// Bare side-effect import: `import 'foo';`
// We require the keyword at word boundary and immediately followed
// by whitespace + quote so we don't match `import(` or similar.
static BARE_IMPORT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\bimport\s+(['"`])([^'"`]+)['"`]"#).expect("BARE_IMPORT_RE"));

// CommonJS: `require('foo')`
static REQUIRE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\brequire\s*\(\s*(['"`])([^'"`]+)['"`]\s*\)"#).expect("REQUIRE_RE"));

// Dynamic ESM: `import('foo')` — including the rare but valid
// template-literal form `import(`foo`)` that some tools emit.
static DYNAMIC_IMPORT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\bimport\s*\(\s*(['"`])([^'"`]+)['"`]\s*\)"#).expect("DYNAMIC_IMPORT_RE"));

/// Does the envelope contain at least one file whose extension we
/// know how to analyse? The guard short-circuits to
/// [`GuardOutcome::Skipped`] otherwise, matching the compiler
/// gate's `no_ts_files` policy.
pub fn envelope_has_analyzable_files(envelope: &CodegenEnvelope) -> bool {
    envelope.files.iter().any(|f| ext_is_supported(&f.path))
}

fn ext_is_supported(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    SUPPORTED_EXTS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}

/// Extract raw import specifiers from a single source file's
/// contents, *after* stripping line- and block-comments.
pub fn extract_specifiers(source: &str) -> Vec<String> {
    let stripped = COMMENT_RE.replace_all(source, "");
    let mut out: Vec<String> = Vec::new();
    for re in [
        &*FROM_IMPORT_RE,
        &*BARE_IMPORT_RE,
        &*REQUIRE_RE,
        &*DYNAMIC_IMPORT_RE,
    ] {
        for caps in re.captures_iter(&stripped) {
            if let Some(spec) = caps.get(2) {
                out.push(spec.as_str().to_string());
            }
        }
    }
    out
}

/// Classify a specifier. Returns `None` for anything we shouldn't
/// validate (relative/absolute/built-in), or `Some(pkg_root)` for
/// the package name to look up in `package.json`.
///
/// # Normalization rules
///
/// * `./foo`, `../foo`, `/foo` → `None` (relative/absolute).
/// * `` (empty) → `None`.
/// * `node:fs`, `bun:test`, any `X:Y` with a single-letter-or-more
///   scheme → `None`. This covers Node + Bun + any future runtime
///   that adopts the pattern. We also defang any future
///   `workerd:`, `deno:`, etc.
/// * bare `fs` / `path` / … in [`NODE_BUILTINS`] → `None`.
/// * `@scope/pkg[/sub/thing]` → `Some("@scope/pkg")`.
/// * `pkg[/sub/thing]` → `Some("pkg")`.
pub fn classify_specifier(spec: &str) -> Option<String> {
    if spec.is_empty() {
        return None;
    }
    // Relative / absolute.
    if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') {
        return None;
    }
    // Scheme-prefixed builtins (node:fs, bun:test, …). Be lenient:
    // we only require a non-empty scheme followed by `:` and a
    // non-empty body, so `C:/path` won't match (that also fails
    // the relative/absolute check below — Windows absolute paths
    // wouldn't be valid ES specifiers anyway).
    if let Some(idx) = spec.find(':') {
        let (scheme, rest) = spec.split_at(idx);
        let rest = &rest[1..];
        if !scheme.is_empty()
            && !rest.is_empty()
            && scheme.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return None;
        }
    }
    // Bare Node built-in.
    let root = package_root(spec);
    if BUILTIN_SET.contains(root.as_str()) {
        return None;
    }
    Some(root)
}

/// Package-root extractor. `@scope/pkg/sub` → `@scope/pkg`;
/// `pkg/sub` → `pkg`. Consumers of [`classify_specifier`] get the
/// normalized form already — this is exposed for tests and for
/// any future telemetry that needs the raw specifier too.
pub fn package_root(spec: &str) -> String {
    if spec.starts_with('@') {
        // Scoped: keep @scope/name, drop anything after.
        let mut parts = spec.splitn(3, '/');
        let scope = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        if name.is_empty() {
            scope.to_string()
        } else {
            format!("{scope}/{name}")
        }
    } else {
        // Bare: keep up to first `/`.
        spec.split('/').next().unwrap_or(spec).to_string()
    }
}

/// Load the union of declared dependencies from `package.json`.
///
/// Returns `None` if the file doesn't exist or can't be parsed as
/// JSON. The guard caller converts that into
/// [`GuardOutcome::Skipped { reason: "no_package_json" }`] rather
/// than failing — a fresh project without a `package.json` yet
/// should not be blocked from running codegen.
pub async fn load_declared_deps(project_dir: &Path) -> Option<BTreeSet<String>> {
    let path = project_dir.join("package.json");
    let raw = fs::read_to_string(&path).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let mut out = BTreeSet::new();
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = value.get(key).and_then(|v| v.as_object()) {
            for k in obj.keys() {
                out.insert(k.clone());
            }
        }
    }
    Some(out)
}

/// Run the guard over an envelope with a pre-loaded dependency
/// set. Useful for tests that don't want to touch the filesystem.
pub fn check_envelope_with_deps(
    envelope: &CodegenEnvelope,
    declared: &BTreeSet<String>,
) -> GuardOutcome {
    let mut resolved: BTreeSet<String> = BTreeSet::new();
    let mut missing: BTreeSet<String> = BTreeSet::new();
    let mut per_file: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in &envelope.files {
        if !ext_is_supported(&file.path) {
            continue;
        }
        let specs = extract_specifiers(&file.content);
        for spec in specs {
            let Some(root) = classify_specifier(&spec) else {
                continue;
            };
            if declared.contains(&root) {
                resolved.insert(root);
            } else {
                missing.insert(root);
                // Dedupe per-file raw specifiers so the model gets a
                // clean miss list — e.g. two imports from the same
                // phantom package in the same file (Devin Review PR
                // #4 comment 3120401725) should only appear once.
                let entry = per_file.entry(file.path.clone()).or_default();
                if !entry.iter().any(|s| s == &spec) {
                    entry.push(spec);
                }
            }
        }
    }

    if missing.is_empty() {
        GuardOutcome::Ok {
            resolved: resolved.into_iter().collect(),
        }
    } else {
        GuardOutcome::Missing {
            missing: missing.into_iter().collect(),
            per_file,
        }
    }
}

/// Apply the configured mode (`"fail"` / `"warn"`) to a raw
/// [`GuardOutcome::Missing`]. Any other variant is passed through.
pub fn apply_mode(outcome: GuardOutcome, mode: &str) -> GuardOutcome {
    match outcome {
        GuardOutcome::Missing { missing, per_file } if mode.eq_ignore_ascii_case("warn") => {
            GuardOutcome::Warned { missing, per_file }
        }
        other => other,
    }
}

/// Full guard entrypoint used by the controller.
///
/// Returns the outcome wrapped in `Ok`; the `Err` branch is
/// reserved for catastrophic I/O errors we couldn't safely swallow.
/// In practice we avoid those by treating a missing / malformed
/// `package.json` as `Skipped`.
pub async fn check_envelope(
    project_dir: &Path,
    envelope: &CodegenEnvelope,
    enabled: bool,
    mode: &str,
) -> Result<GuardOutcome, String> {
    if !enabled {
        return Ok(GuardOutcome::Skipped { reason: "disabled" });
    }
    if !envelope_has_analyzable_files(envelope) {
        return Ok(GuardOutcome::Skipped {
            reason: "no_supported_files",
        });
    }
    let declared = match load_declared_deps(project_dir).await {
        Some(s) => s,
        None => {
            return Ok(GuardOutcome::Skipped {
                reason: "no_package_json",
            })
        }
    };
    let raw = check_envelope_with_deps(envelope, &declared);
    Ok(apply_mode(raw, mode))
}

/// Render a [`GuardOutcome::Missing`] / [`GuardOutcome::Warned`]
/// into a deterministic, LLM-friendly feedback block for the
/// re-prompt. Other variants collapse to an empty string — the
/// controller only calls this when it has a real miss list.
pub fn missing_to_feedback(outcome: &GuardOutcome) -> String {
    let (missing, per_file) = match outcome {
        GuardOutcome::Missing { missing, per_file } => (missing, per_file),
        GuardOutcome::Warned { missing, per_file } => (missing, per_file),
        _ => return String::new(),
    };

    let mut out = String::new();
    out.push_str(
        "DEPENDENCY GUARD: the previous envelope imports packages that are not \
         listed in package.json. Either change the imports to match an installed \
         package or revise your plan to use something already available. The \
         following package specifiers are unresolved:\n",
    );
    for pkg in missing {
        out.push_str(&format!("- {pkg}\n"));
    }
    if !per_file.is_empty() {
        out.push_str("\nPer-file breakdown (file → raw specifiers):\n");
        for (path, specs) in per_file {
            out.push_str(&format!("- {path}: {}\n", specs.join(", ")));
        }
    }
    out.push_str(
        "\nDo not invent packages. If you must introduce a new dependency, say so \
         explicitly in run_cmd (for example `bun add <pkg>`) instead of silently \
         importing it.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen_envelope::{CodegenEnvelope, EnvelopeFile};

    fn env(files: &[(&str, &str)]) -> CodegenEnvelope {
        CodegenEnvelope {
            files: files
                .iter()
                .map(|(p, c)| EnvelopeFile {
                    path: (*p).to_string(),
                    content: (*c).to_string(),
                })
                .collect(),
            run_cmd: None,
        }
    }

    fn deps(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn classify_handles_relative_and_absolute() {
        assert_eq!(classify_specifier("./foo"), None);
        assert_eq!(classify_specifier("../../bar/baz"), None);
        assert_eq!(classify_specifier("/abs/path"), None);
    }

    #[test]
    fn classify_handles_builtins() {
        assert_eq!(classify_specifier("fs"), None);
        assert_eq!(classify_specifier("path"), None);
        assert_eq!(classify_specifier("node:fs"), None);
        assert_eq!(classify_specifier("bun:test"), None);
        assert_eq!(classify_specifier("deno:net"), None);
    }

    #[test]
    fn classify_normalises_subpaths_and_scopes() {
        assert_eq!(classify_specifier("lodash"), Some("lodash".into()));
        assert_eq!(
            classify_specifier("lodash/debounce"),
            Some("lodash".into())
        );
        assert_eq!(
            classify_specifier("@tanstack/react-query"),
            Some("@tanstack/react-query".into())
        );
        assert_eq!(
            classify_specifier("@tanstack/react-query/devtools"),
            Some("@tanstack/react-query".into())
        );
    }

    #[test]
    fn classify_empty_is_none() {
        assert_eq!(classify_specifier(""), None);
    }

    #[test]
    fn extract_picks_up_four_shapes() {
        let src = r#"
            import React from 'react';
            import type { FC } from 'react';
            import 'normalize.css';
            import { create } from "zustand";
            export { default as Foo } from './Foo';
            const cfg = require('node:fs');
            const mod = await import('lodash/debounce');
        "#;
        let got: BTreeSet<String> = extract_specifiers(src).into_iter().collect();
        assert!(got.contains("react"), "missing react in {got:?}");
        assert!(got.contains("normalize.css"), "missing normalize.css");
        assert!(got.contains("zustand"), "missing zustand");
        assert!(got.contains("./Foo"), "missing ./Foo (relative re-export)");
        assert!(got.contains("node:fs"), "missing node:fs");
        assert!(got.contains("lodash/debounce"), "missing dynamic import");
    }

    #[test]
    fn extract_ignores_commented_imports() {
        let src = r#"
            // import React from 'react';
            /* import { create } from 'zustand'; */
            import * as real from 'real-pkg';
        "#;
        let got = extract_specifiers(src);
        assert_eq!(got, vec!["real-pkg".to_string()]);
    }

    #[test]
    fn guard_ok_when_everything_resolves() {
        let e = env(&[(
            "src/a.ts",
            "import React from 'react'; import fs from 'node:fs';",
        )]);
        let d = deps(&["react"]);
        let out = check_envelope_with_deps(&e, &d);
        match out {
            GuardOutcome::Ok { resolved } => {
                assert_eq!(resolved, vec!["react".to_string()]);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn guard_flags_missing_package() {
        let e = env(&[(
            "src/a.ts",
            "import { create } from 'zustand'; import React from 'react';",
        )]);
        let d = deps(&["react"]);
        let out = check_envelope_with_deps(&e, &d);
        match out {
            GuardOutcome::Missing { missing, per_file } => {
                assert_eq!(missing, vec!["zustand".to_string()]);
                assert_eq!(
                    per_file.get("src/a.ts").cloned().unwrap_or_default(),
                    vec!["zustand".to_string()]
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn guard_flags_scoped_package_root_not_subpath() {
        let e = env(&[(
            "src/a.tsx",
            "import { QueryClient } from '@tanstack/react-query/devtools';",
        )]);
        let d = deps(&[]);
        let out = check_envelope_with_deps(&e, &d);
        match out {
            GuardOutcome::Missing { missing, .. } => {
                assert_eq!(missing, vec!["@tanstack/react-query".to_string()]);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn guard_skips_non_source_envelopes() {
        let e = env(&[("index.html", "<html></html>"), ("data.json", "{}")]);
        assert!(!envelope_has_analyzable_files(&e));
    }

    #[test]
    fn apply_mode_warn_downgrades_missing() {
        let raw = GuardOutcome::Missing {
            missing: vec!["zustand".into()],
            per_file: BTreeMap::new(),
        };
        match apply_mode(raw, "warn") {
            GuardOutcome::Warned { missing, .. } => {
                assert_eq!(missing, vec!["zustand".to_string()]);
            }
            other => panic!("expected Warned, got {other:?}"),
        }
    }

    #[test]
    fn apply_mode_fail_preserves_missing() {
        let raw = GuardOutcome::Missing {
            missing: vec!["zustand".into()],
            per_file: BTreeMap::new(),
        };
        assert!(matches!(apply_mode(raw, "fail"), GuardOutcome::Missing { .. }));
    }

    #[test]
    fn missing_to_feedback_is_stable_and_informative() {
        let mut per_file = BTreeMap::new();
        per_file.insert(
            "src/a.ts".to_string(),
            vec!["zustand".to_string(), "@tanstack/react-query".to_string()],
        );
        let outcome = GuardOutcome::Missing {
            missing: vec![
                "@tanstack/react-query".to_string(),
                "zustand".to_string(),
            ],
            per_file,
        };
        let fb = missing_to_feedback(&outcome);
        assert!(fb.contains("DEPENDENCY GUARD"));
        assert!(fb.contains("- zustand"));
        assert!(fb.contains("- @tanstack/react-query"));
        assert!(fb.contains("src/a.ts"));
        assert!(fb.contains("bun add"));
    }

    #[tokio::test]
    async fn load_declared_deps_reads_all_four_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        tokio::fs::write(
            &path,
            r#"{
                "name": "t",
                "dependencies": { "react": "^18" },
                "devDependencies": { "typescript": "^5" },
                "peerDependencies": { "zustand": "*" },
                "optionalDependencies": { "fsevents": "^2" }
            }"#,
        )
        .await
        .unwrap();
        let got = load_declared_deps(tmp.path()).await.unwrap();
        assert!(got.contains("react"));
        assert!(got.contains("typescript"));
        assert!(got.contains("zustand"));
        assert!(got.contains("fsevents"));
    }

    #[tokio::test]
    async fn load_declared_deps_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_declared_deps(tmp.path()).await.is_none());
    }

    #[tokio::test]
    async fn load_declared_deps_returns_none_on_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("package.json"), "{ not json")
            .await
            .unwrap();
        assert!(load_declared_deps(tmp.path()).await.is_none());
    }

    #[tokio::test]
    async fn check_envelope_skips_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(&[("src/a.ts", "import x from 'zustand';")]);
        let out = check_envelope(tmp.path(), &e, false, "fail").await.unwrap();
        assert!(matches!(
            out,
            GuardOutcome::Skipped { reason: "disabled" }
        ));
    }

    #[tokio::test]
    async fn check_envelope_skips_without_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(&[("src/a.ts", "import x from 'zustand';")]);
        let out = check_envelope(tmp.path(), &e, true, "fail").await.unwrap();
        assert!(matches!(
            out,
            GuardOutcome::Skipped {
                reason: "no_package_json"
            }
        ));
    }

    #[tokio::test]
    async fn check_envelope_detects_missing_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"react":"^18"}}"#,
        )
        .await
        .unwrap();
        let e = env(&[(
            "src/a.tsx",
            "import React from 'react'; import { create } from 'zustand';",
        )]);
        let out = check_envelope(tmp.path(), &e, true, "fail").await.unwrap();
        match out {
            GuardOutcome::Missing { missing, .. } => {
                assert_eq!(missing, vec!["zustand".to_string()]);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    // --- Regression tests for PR-D Devin Review findings ---
    // (PR #4 review comments 3120401607, 3120401509, 3120401725)

    #[test]
    fn extract_handles_multi_line_from_import() {
        // Before PR-E, `FROM_IMPORT_RE` excluded `\n`, so destructured
        // imports spread over multiple lines were silently dropped —
        // which happens to be the shape LLMs emit most often.
        let src = r#"
            import {
                useState,
                useEffect,
            } from 'react';
            import type {
                FC,
                ReactNode,
            } from 'react';
        "#;
        let got = extract_specifiers(src);
        assert!(got.iter().any(|s| s == "react"), "multi-line default/destructured import missed: {got:?}");
    }

    #[test]
    fn extract_handles_multi_line_export_from() {
        let src = r#"
            export {
                foo,
                bar,
            } from '@scope/pkg';
        "#;
        let got = extract_specifiers(src);
        assert!(got.iter().any(|s| s == "@scope/pkg"), "multi-line re-export missed: {got:?}");
    }

    #[test]
    fn extract_handles_backtick_template_literal_imports() {
        // Dynamic `import(`pkg`)` and side-effect `import `pkg`` are
        // rare but valid. PR-D's regexes only matched `'` / `"` —
        // the doc claimed otherwise, which was the actual bug.
        let src = r#"
            const m = await import(`lodash`);
            import `side-effect-pkg`;
            const fs = require(`node:fs`);
        "#;
        let got = extract_specifiers(src);
        assert!(got.iter().any(|s| s == "lodash"), "backtick dynamic import missed: {got:?}");
        assert!(got.iter().any(|s| s == "side-effect-pkg"), "backtick bare import missed: {got:?}");
        assert!(got.iter().any(|s| s == "node:fs"), "backtick require missed: {got:?}");
    }

    #[test]
    fn extract_does_not_bridge_across_semicolons() {
        // Dropping `\n` from the exclusion class could theoretically
        // let the non-greedy middle section bridge across sibling
        // statements. The semicolon stop prevents that.
        let src = r#"
            import sideEffect; import b from "y";
        "#;
        let got = extract_specifiers(src);
        // We should still find "y" (the valid import) and NOT
        // accidentally match the bare `sideEffect` as a specifier.
        assert!(got.iter().any(|s| s == "y"), "expected y in {got:?}");
    }

    #[test]
    fn guard_dedupes_per_file_raw_specifiers() {
        // Two imports from the same phantom package in the same file
        // should surface the spec only once in per_file.
        let e = env(&[(
            "src/a.ts",
            "import { create } from 'zustand';\nimport { devtools } from 'zustand';",
        )]);
        let d = deps(&[]);
        let out = check_envelope_with_deps(&e, &d);
        match out {
            GuardOutcome::Missing { missing, per_file } => {
                assert_eq!(missing, vec!["zustand".to_string()]);
                assert_eq!(
                    per_file.get("src/a.ts").cloned().unwrap_or_default(),
                    vec!["zustand".to_string()],
                    "per_file must dedupe identical raw specifiers"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn guard_catches_multi_line_phantom_import_end_to_end() {
        // Combines BUG-3 (multi-line) with a phantom package — this
        // is the LLM failure mode the guard exists to catch.
        let e = env(&[(
            "src/a.tsx",
            "import {\n  QueryClient,\n  QueryClientProvider,\n} from '@tanstack/react-query';",
        )]);
        let d = deps(&["react"]);
        let out = check_envelope_with_deps(&e, &d);
        match out {
            GuardOutcome::Missing { missing, .. } => {
                assert_eq!(missing, vec!["@tanstack/react-query".to_string()]);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }
}
