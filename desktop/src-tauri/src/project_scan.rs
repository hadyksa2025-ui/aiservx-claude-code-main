//! Pre-execution project scanner. Produces a lightweight `project_map`
//! describing the opened project's languages, entry points, config files,
//! and declared dependencies. Persisted into `PROJECT_MEMORY.json →
//! project_map` so both the model and the UI can read it later.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

const MAX_WALK_DEPTH: usize = 4;
const MAX_ENTRIES: usize = 2000;

/// Hidden directories and build outputs we never descend into. Keeps the
/// scan fast and stops us from picking up vendored dependencies as project
/// signals.
const IGNORE_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".cache",
    ".turbo",
    ".parcel-cache",
    "vendor",
];

#[derive(Debug, Clone, Serialize)]
pub struct ProjectMap {
    pub root: String,
    pub scanned_at: u64,
    pub languages: Vec<String>,
    pub entry_points: Vec<String>,
    pub configs: Vec<String>,
    pub dependencies: Vec<String>,
    pub file_count: usize,
}

pub fn scan_project(project_dir: &str) -> ProjectMap {
    let root = PathBuf::from(project_dir);
    let mut langs: std::collections::BTreeSet<String> = Default::default();
    let mut configs: Vec<String> = Vec::new();
    let mut entries: Vec<String> = Vec::new();
    let mut deps: std::collections::BTreeSet<String> = Default::default();
    let mut file_count = 0usize;

    walk(&root, &root, 0, &mut |rel, abs, is_dir| {
        if is_dir {
            return;
        }
        file_count += 1;
        if file_count > MAX_ENTRIES {
            return;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Language detection by extension.
        if let Some(ext) = rel.extension().and_then(|e| e.to_str()) {
            let lang = match ext.to_ascii_lowercase().as_str() {
                "rs" => Some("rust"),
                "ts" | "tsx" => Some("typescript"),
                "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
                "py" => Some("python"),
                "go" => Some("go"),
                "rb" => Some("ruby"),
                "php" => Some("php"),
                "java" => Some("java"),
                "kt" | "kts" => Some("kotlin"),
                "swift" => Some("swift"),
                "c" | "h" => Some("c"),
                "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Some("cpp"),
                "cs" => Some("csharp"),
                "sh" | "bash" => Some("shell"),
                _ => None,
            };
            if let Some(l) = lang {
                langs.insert(l.into());
            }
        }

        // Known config / manifest files. We read a handful to extract
        // declared dependencies.
        match name {
            "package.json" => {
                configs.push(rel_str.clone());
                langs.insert("typescript/javascript".into());
                if let Ok(text) = std::fs::read_to_string(abs) {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        for section in ["dependencies", "devDependencies", "peerDependencies"] {
                            if let Some(obj) = v.get(section).and_then(|x| x.as_object()) {
                                for k in obj.keys() {
                                    deps.insert(format!("npm:{k}"));
                                }
                            }
                        }
                    }
                }
            }
            "Cargo.toml" => {
                configs.push(rel_str.clone());
                langs.insert("rust".into());
                if let Ok(text) = std::fs::read_to_string(abs) {
                    // Cheap, tolerant extraction — avoids pulling in a toml
                    // parser for a feature that's best-effort anyway.
                    for line in text.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                            continue;
                        }
                        if let Some((k, _)) = line.split_once('=') {
                            let name = k.trim().trim_matches('"');
                            if !name.is_empty() && name.chars().all(is_crate_char) {
                                deps.insert(format!("cargo:{name}"));
                            }
                        }
                    }
                }
            }
            "pyproject.toml" | "setup.py" | "requirements.txt" | "Pipfile" => {
                configs.push(rel_str.clone());
                langs.insert("python".into());
                if name == "requirements.txt" {
                    if let Ok(text) = std::fs::read_to_string(abs) {
                        for line in text.lines() {
                            let line = line.trim();
                            if line.is_empty() || line.starts_with('#') {
                                continue;
                            }
                            let bare = line
                                .split(|c: char| "<>=! ;".contains(c))
                                .next()
                                .unwrap_or("")
                                .trim();
                            if !bare.is_empty() {
                                deps.insert(format!("pypi:{bare}"));
                            }
                        }
                    }
                }
            }
            "go.mod" => {
                configs.push(rel_str.clone());
                langs.insert("go".into());
            }
            "Gemfile" | "Gemfile.lock" => {
                configs.push(rel_str.clone());
                langs.insert("ruby".into());
            }
            "composer.json" => {
                configs.push(rel_str.clone());
                langs.insert("php".into());
            }
            "build.gradle" | "build.gradle.kts" | "settings.gradle" | "pom.xml" => {
                configs.push(rel_str.clone());
                langs.insert("java/kotlin".into());
            }
            "Dockerfile" | "docker-compose.yml" | "docker-compose.yaml" => {
                configs.push(rel_str.clone());
            }
            "tsconfig.json" | "tailwind.config.js" | "tailwind.config.ts" | "vite.config.ts"
            | "vite.config.js" | "webpack.config.js" | "next.config.js" | "next.config.mjs"
            | "astro.config.mjs" | "nuxt.config.ts" => {
                configs.push(rel_str.clone());
            }
            "tauri.conf.json" | "tauri.conf.json5" => {
                configs.push(rel_str.clone());
            }
            _ => {}
        }

        // Common entry points.
        if matches!(
            rel_str.as_str(),
            "src/main.rs"
                | "src/lib.rs"
                | "src/main.py"
                | "main.py"
                | "src/index.ts"
                | "src/index.tsx"
                | "src/main.ts"
                | "src/main.tsx"
                | "src/index.js"
                | "index.js"
                | "server.js"
                | "app.js"
                | "main.go"
                | "cmd/main.go"
        ) {
            entries.push(rel_str);
        }
    });

    let mut languages: Vec<String> = langs.into_iter().collect();
    languages.sort();
    let mut dependencies: Vec<String> = deps.into_iter().collect();
    dependencies.sort();
    dependencies.truncate(500);

    ProjectMap {
        root: project_dir.to_string(),
        scanned_at: crate::tasks::unix_ts(),
        languages,
        entry_points: entries,
        configs,
        dependencies,
        file_count,
    }
}

fn is_crate_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    cb: &mut dyn FnMut(&Path, &Path, bool),
) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let abs = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') && depth == 0 && name_str != "." {
            // Skip top-level dotfiles/dotdirs except we keep them countable.
            continue;
        }
        let is_dir = abs.is_dir();
        if is_dir && IGNORE_DIRS.iter().any(|d| *d == name_str) {
            continue;
        }
        let rel = abs.strip_prefix(root).unwrap_or(&abs).to_path_buf();
        cb(&rel, &abs, is_dir);
        if is_dir {
            walk(root, &abs, depth + 1, cb);
        }
    }
}

/// Persist a freshly built `ProjectMap` into `PROJECT_MEMORY.json →
/// project_map`.
pub fn save_project_map(project_dir: &str, map: &ProjectMap) -> Result<(), String> {
    let path = std::path::PathBuf::from(project_dir).join("PROJECT_MEMORY.json");
    let mut mem: Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !mem.is_object() {
        mem = json!({});
    }
    let obj = mem.as_object_mut().unwrap();
    obj.insert(
        "project_map".into(),
        serde_json::to_value(map).unwrap_or(Value::Null),
    );
    crate::memory::save_memory_sync(project_dir, &mem)
}

// ---------- Tauri command ----------

#[tauri::command]
pub fn scan_project_cmd(project_dir: String) -> Result<ProjectMap, String> {
    let map = scan_project(&project_dir);
    let _ = save_project_map(&project_dir, &map);
    Ok(map)
}
