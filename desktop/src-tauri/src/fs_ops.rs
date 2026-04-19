//! File-system commands. Every path the frontend sends is sandboxed to the
//! opened project root: we canonicalize the root and reject any path that
//! would escape it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

/// Resolve a path relative to `root` and ensure it stays inside `root`.
pub fn resolve(root: &str, sub: &str) -> Result<PathBuf, String> {
    let root_buf = Path::new(root);
    let root_canon = root_buf
        .canonicalize()
        .map_err(|e| format!("invalid project root {root}: {e}"))?;

    // Treat sub as a relative path always. Strip any leading separators.
    let sub_rel = sub.trim_start_matches(['/', '\\']);
    let joined = if sub_rel.is_empty() {
        root_canon.clone()
    } else {
        root_canon.join(sub_rel)
    };

    // Walk up and resolve `..` virtually; we can't `canonicalize` files that
    // don't exist yet (e.g. for write_file on a new file).
    let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
    for component in joined.components() {
        use std::path::Component::*;
        match component {
            Prefix(p) => stack.push(p.as_os_str()),
            RootDir => {
                stack.clear();
                stack.push(std::ffi::OsStr::new(std::path::MAIN_SEPARATOR_STR));
            }
            CurDir => {}
            ParentDir => {
                stack.pop();
            }
            Normal(n) => stack.push(n),
        }
    }
    let mut resolved = PathBuf::new();
    for part in stack {
        if resolved.as_os_str().is_empty() {
            resolved.push(part);
        } else {
            resolved.push(part);
        }
    }

    if !resolved.starts_with(&root_canon) {
        return Err(format!(
            "path {sub} escapes project root {}",
            root_canon.display()
        ));
    }
    Ok(resolved)
}

/// Render a path relative to `root`, with forward slashes.
fn rel_for_ui(root: &Path, full: &Path) -> String {
    let rel = full.strip_prefix(root).unwrap_or(full);
    rel.to_string_lossy().replace('\\', "/")
}

#[tauri::command]
pub fn list_dir(project_dir: String, sub_path: String) -> Result<Vec<FsEntry>, String> {
    let target = resolve(&project_dir, &sub_path)?;
    if !target.is_dir() {
        return Err(format!("not a directory: {}", target.display()));
    }
    let root_canon = Path::new(&project_dir)
        .canonicalize()
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for entry in fs::read_dir(&target).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Hide a handful of noisy directories by default; the UI still sees them
        // if a human explicitly navigates in via read_file.
        if matches!(
            name.as_str(),
            ".git" | "node_modules" | "target" | "dist" | ".next"
        ) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(FsEntry {
            name,
            path: rel_for_ui(&root_canon, &path),
            is_dir: meta.is_dir(),
            size: if meta.is_file() { Some(meta.len()) } else { None },
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn read_file(project_dir: String, sub_path: String) -> Result<String, String> {
    let target = resolve(&project_dir, &sub_path)?;
    const MAX_BYTES: u64 = 2 * 1024 * 1024;
    let meta = fs::metadata(&target).map_err(|e| e.to_string())?;
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "file is too large ({} bytes) for read_file; max is {} bytes",
            meta.len(),
            MAX_BYTES
        ));
    }
    fs::read_to_string(&target).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn write_file(
    project_dir: String,
    sub_path: String,
    content: String,
) -> Result<String, String> {
    let target = resolve(&project_dir, &sub_path)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let previous = fs::read_to_string(&target).unwrap_or_default();
    fs::write(&target, &content).map_err(|e| e.to_string())?;
    Ok(diff(&previous, &content))
}

pub fn diff(old: &str, new: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let d = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for change in d.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(prefix);
        out.push_str(change.to_string().trim_end_matches('\n'));
        out.push('\n');
    }
    out
}
