//! Project-folder resolution for the `~/.ug/<project>` data layout.
//!
//! Generated data (graph.json, indexed-tree.json, ugdb/, project.json)
//! lives under one directory per indexed repo/project, rooted at
//! `ug_home()`. All project.json reads/writes go through this module so
//! the metadata backend can later be swapped for the project's own
//! OverGraph db.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::flag_value;

/// Root of all project data dirs: `$UG_HOME` if set, else `~/.ug`.
pub(crate) fn ug_home() -> PathBuf {
    if let Ok(h) = std::env::var("UG_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ug")
}

/// Make an arbitrary string safe as a directory name under `ug_home()`:
/// chars outside `[A-Za-z0-9._-]` become `-`, leading `.`/`-` are
/// stripped (no hidden dirs / flag lookalikes), capped at 64 chars.
/// Empty or `.`/`..` results fall back to `"default"`.
pub(crate) fn sanitize_name(raw: &str) -> String {
    let mapped: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stripped: String = mapped
        .trim_start_matches(['.', '-'])
        .chars()
        .take(64)
        .collect();
    if stripped.is_empty() || stripped == "." || stripped == ".." {
        "default".to_string()
    } else {
        stripped
    }
}

/// Project name derived from a path: basename of the canonicalized
/// input dir, sanitized. Falls back to `"default"`.
pub(crate) fn derive_project_name(input: &str) -> String {
    let p = Path::new(input);
    let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match canon.file_name().and_then(|n| n.to_str()) {
        Some(base) => sanitize_name(base),
        None => "default".to_string(),
    }
}

/// Resolve the project name for a command invocation: `-n/--name` flag
/// wins, else derive from the given input path (typically `-i` /
/// positional / cwd).
pub(crate) fn resolve_project_name(args: &[String], input: &str) -> String {
    match flag_value(args, &["-n", "--name"]) {
        Some(n) => sanitize_name(&n),
        None => derive_project_name(input),
    }
}

/// Data directory for a (sanitized) project name.
pub(crate) fn project_dir(name: &str) -> PathBuf {
    ug_home().join(sanitize_name(name))
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Flat per-project metadata persisted as `<project-dir>/project.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectMeta {
    pub name: String,
    #[serde(default)]
    pub repo_root: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub nodes: usize,
    #[serde(default)]
    pub edges: usize,
    #[serde(default)]
    pub ug_version: String,
}

impl ProjectMeta {
    pub(crate) fn new(name: &str, repo_root: &str, nodes: usize, edges: usize) -> Self {
        let now = now_epoch();
        ProjectMeta {
            name: name.to_string(),
            repo_root: repo_root.to_string(),
            created_at: now,
            updated_at: now,
            nodes,
            edges,
            ug_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

pub(crate) fn meta_path(dir: &Path) -> PathBuf {
    dir.join("project.json")
}

pub(crate) fn read_meta(dir: &Path) -> Option<ProjectMeta> {
    let raw = std::fs::read_to_string(meta_path(dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write project.json, preserving `created_at` from any existing file.
pub(crate) fn write_meta(dir: &Path, meta: &ProjectMeta) -> std::io::Result<()> {
    let mut out = meta.clone();
    if let Some(existing) = read_meta(dir) {
        if existing.created_at > 0 {
            out.created_at = existing.created_at;
        }
    }
    out.updated_at = now_epoch();
    let json = serde_json::to_string_pretty(&out).expect("ProjectMeta serializes");
    std::fs::create_dir_all(dir)?;
    std::fs::write(meta_path(dir), json)
}

/// Enumerate project dirs under `ug_home()`: any subdir containing a
/// `project.json` or a `graph.json`. When project.json is missing,
/// synthesize metadata from the dir name and graph.json mtime. Sorted
/// by `updated_at` descending (most recent first).
pub(crate) fn list_projects() -> Vec<(PathBuf, ProjectMeta)> {
    let root = ug_home();
    let mut out: Vec<(PathBuf, ProjectMeta)> = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let graph = dir.join("graph.json");
        if let Some(meta) = read_meta(&dir) {
            out.push((dir, meta));
        } else if graph.exists() {
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("default")
                .to_string();
            let mtime = std::fs::metadata(&graph)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut meta = ProjectMeta::new(&name, "", 0, 0);
            meta.created_at = mtime;
            meta.updated_at = mtime;
            meta.ug_version = String::new();
            out.push((dir, meta));
        }
    }
    out.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
    out
}

/// Default db path for read commands (chat, semantic_search, …) when
/// no `-d/--db` flag and no `UG_DB_PATH` env var is given:
/// `~/.ug/<cwd-basename>/ugdb` if it exists → legacy `./.ug/ugdb` if it
/// exists → `~/.ug/<cwd-basename>/ugdb` (so error messages point users
/// at the new layout).
pub(crate) fn default_read_db_path() -> String {
    let new_path = project_dir(&derive_project_name(".")).join("ugdb");
    if new_path.exists() {
        return new_path.to_string_lossy().into_owned();
    }
    let legacy = Path::new(".ug/ugdb");
    if legacy.exists() {
        return ".ug/ugdb".to_string();
    }
    new_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_names() {
        assert_eq!(sanitize_name("ug"), "ug");
        assert_eq!(sanitize_name("My_Repo-2.0"), "My_Repo-2.0");
    }

    #[test]
    fn sanitize_replaces_and_strips() {
        assert_eq!(sanitize_name("../evil"), "evil");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(sanitize_name("--flag"), "flag");
        assert_eq!(sanitize_name("a b/c"), "a-b-c");
        // all chars non-ascii → all '-' → leading dashes stripped → empty → default
        assert_eq!(sanitize_name("日本語"), "default");
    }

    #[test]
    fn sanitize_falls_back_to_default() {
        assert_eq!(sanitize_name(""), "default");
        assert_eq!(sanitize_name("."), "default");
        assert_eq!(sanitize_name(".."), "default");
        assert_eq!(sanitize_name("///"), "default");
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "x".repeat(200);
        assert_eq!(sanitize_name(&long).len(), 64);
    }

    #[test]
    fn derive_uses_basename() {
        let tmp = std::env::temp_dir().join("ug-project-test-dir");
        let _ = std::fs::create_dir_all(&tmp);
        assert_eq!(
            derive_project_name(tmp.to_str().unwrap()),
            "ug-project-test-dir"
        );
    }

    #[test]
    fn meta_roundtrip_preserves_created_at() {
        let dir = std::env::temp_dir().join(format!("ug-meta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ProjectMeta::new("t", "/repo", 1, 2);
        m.created_at = 1000;
        write_meta(&dir, &m).unwrap();
        let first = read_meta(&dir).unwrap();
        assert_eq!(first.created_at, 1000);
        let m2 = ProjectMeta::new("t", "/repo", 3, 4);
        write_meta(&dir, &m2).unwrap();
        let second = read_meta(&dir).unwrap();
        assert_eq!(second.created_at, 1000, "created_at preserved");
        assert_eq!(second.nodes, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
