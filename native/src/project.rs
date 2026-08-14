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

use crate::cli::args::flag_value;

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

/// Resolve the project name for commands that default to a *generated*
/// project rather than the cwd: `-n/--name` wins, else the persisted
/// active project (`ug active`), else the cwd's basename.
///
/// This is the chain `regen`, `ingest`, and the read commands use so a
/// `ug active <name>` from outside an indexed repo lands on the project
/// the user pinned instead of silently picking the most recently
/// updated one. Generate commands (`gen`, `index`, `graph`) keep using
/// [`resolve_project_name`] — they create a project from the cwd and
/// must not be redirected by the active marker.
pub(crate) fn resolve_active_project_name(args: &[String], input: &str) -> String {
    match flag_value(args, &["-n", "--name"]) {
        Some(n) => sanitize_name(&n),
        None => get_active_project().unwrap_or_else(|| derive_project_name(input)),
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
    /// Repo-relative paths of every file represented in the graph.
    ///
    /// Recorded here so the staleness check (`GET /api/projects/staleness`,
    /// polled by the KB Manager every 2 minutes) can `stat` the source tree
    /// without reading and JSON-parsing a multi-megabyte `graph.json` per
    /// project on every poll. Empty for projects generated before this field
    /// existed; consumers fall back to reading the graph in that case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Node-type composition, also for staleness reporting without a re-parse.
    #[serde(default)]
    pub doc_nodes: usize,
    #[serde(default)]
    pub code_nodes: usize,
    /// When the oldest still-unpaid `--no-embed` run happened (epoch
    /// seconds), or 0 when the store's vectors are current.
    ///
    /// A run that skips embedding writes real structure and no vectors, so
    /// everything except semantic search stays correct — but nothing on disk
    /// distinguishes "this node has no vector because we skipped it" from
    /// "this node has no vector because the graph has none". This field is
    /// that distinction, and it is what `ug hook status` and the search
    /// tools report from. Cleared by any run that does embed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pending_vectors_since: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
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
            files: Vec::new(),
            doc_nodes: 0,
            code_nodes: 0,
            pending_vectors_since: 0,
        }
    }

    /// Carry the pending-vectors mark forward from the project.json already
    /// on disk.
    ///
    /// Every write path builds a *fresh* `ProjectMeta`, whose default says
    /// "vectors are current" — so a run that writes metadata before it knows
    /// how the ingest went would silently clear a debt that is still owed.
    /// Only [`set_pending_vectors`] may clear it, and only after an ingest
    /// that actually embedded.
    pub(crate) fn carrying_pending_vectors(mut self, dir: &Path) -> Self {
        self.pending_vectors_since = read_meta(dir).map(|m| m.pending_vectors_since).unwrap_or(0);
        self
    }

    /// Record the file list and node composition derived from `graph`.
    ///
    /// Folder nodes are excluded: they carry a `file` that names a directory,
    /// which would always `stat` clean and inflate the file count.
    pub(crate) fn with_graph_index(mut self, graph: &ultragraph::types::GraphData) -> Self {
        use ultragraph::types::GraphNodeType;
        let mut files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let (mut doc_nodes, mut code_nodes) = (0usize, 0usize);

        for node in &graph.nodes {
            match node.node_type {
                GraphNodeType::Folder | GraphNodeType::File => {}
                GraphNodeType::Concept => doc_nodes += 1,
                _ => code_nodes += 1,
            }
            if node.node_type != GraphNodeType::Folder {
                if let Some(file) = node.file.as_ref() {
                    if !file.is_empty() {
                        files.insert(file.clone());
                    }
                }
            }
        }

        self.files = files.into_iter().collect();
        self.doc_nodes = doc_nodes;
        self.code_nodes = code_nodes;
        self
    }
}

/// Serializes every test that mutates `UG_HOME`.
///
/// The env var is process-global, so `project::tests` and `serve::router_tests`
/// will clobber each other's temp homes if they each keep a private lock —
/// which is exactly what happened, as an intermittent failure in whichever
/// test lost the race. One lock, shared by every module whose tests touch it.
///
/// It is a `tokio::sync::Mutex` because the serve tests are `async` and must
/// hold it across `.await`; synchronous tests take it with `blocking_lock()`,
/// which is safe there because `#[test]` bodies run outside any runtime.
#[cfg(test)]
pub(crate) static UG_HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Record — or clear — the mark that says this project's store holds nodes
/// with no vectors because a run was told to skip embedding.
///
/// Setting keeps the *oldest* unpaid run: "pending since" is what makes the
/// report ("vectors 3 days behind") mean anything, and each new skipping run
/// would otherwise reset the clock to now.
pub(crate) fn set_pending_vectors(dir: &Path, pending: bool) {
    let Some(mut meta) = read_meta(dir) else {
        return;
    };
    let next = match (pending, meta.pending_vectors_since) {
        (true, 0) => now_epoch(),
        (true, oldest) => oldest,
        (false, _) => 0,
    };
    if next == meta.pending_vectors_since {
        return;
    }
    meta.pending_vectors_since = next;
    let _ = write_meta(dir, &meta);
}

/// How long a project's vectors have been owed, if they are.
pub(crate) fn pending_vectors_age(dir: &Path) -> Option<std::time::Duration> {
    let since = read_meta(dir).map(|m| m.pending_vectors_since).unwrap_or(0);
    if since == 0 {
        return None;
    }
    Some(std::time::Duration::from_secs(now_epoch().saturating_sub(since)))
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
/// no `-n/--name` flag is given:
/// the active project's `~/.ug/<active>/ugdb` if set and present →
/// `~/.ug/<cwd-basename>/ugdb` if it exists → legacy `./.ug/ugdb` if it
/// exists → the most recently updated project under `~/.ug` (covers
/// running a read command from outside any indexed repo) →
/// `~/.ug/<cwd-basename>/ugdb` (so error messages point users at the
/// new layout).
pub(crate) fn default_read_db_path() -> String {
    // The active marker is the same one `serve` and `mcp` honor, so a
    // read from outside an indexed repo lands where `ug active` pointed
    // instead of on whatever was touched last. Only used when its db
    // actually exists — an active project that was never ingested falls
    // through to the rest of the chain.
    if let Some(name) = get_active_project() {
        let active_path = project_dir(&name).join("ugdb");
        if active_path.exists() {
            return active_path.to_string_lossy().into_owned();
        }
    }
    let new_path = project_dir(&derive_project_name(".")).join("ugdb");
    if new_path.exists() {
        return new_path.to_string_lossy().into_owned();
    }
    let legacy = Path::new(".ug/ugdb");
    if legacy.exists() {
        return ".ug/ugdb".to_string();
    }
    if let Some((dir, _meta)) = list_projects().into_iter().next() {
        let fallback = dir.join("ugdb");
        if fallback.exists() {
            return fallback.to_string_lossy().into_owned();
        }
    }
    new_path.to_string_lossy().into_owned()
}

/// Whether a project dir holds usable data (a graph or a db), used to
/// validate `set_active_project` / resolve `get_active_project`.
fn project_has_data(dir: &Path) -> bool {
    dir.join("ugdb").exists() || dir.join("graph.json").exists()
}

/// Marker file recording the user's chosen default project:
/// `$UG_HOME/active` (one line, the sanitized project name). A plain file
/// alongside the per-project dirs; `list_projects` only scans dirs, so it
/// never mistakes this for a project.
pub(crate) fn active_path() -> PathBuf {
    ug_home().join("active")
}

/// The persisted active project, if one is set and its data still exists.
/// A stale marker (project since deleted) resolves to `None` rather than
/// erroring, so callers fall through to their next default cleanly.
pub(crate) fn get_active_project() -> Option<String> {
    let raw = std::fs::read_to_string(active_path()).ok()?;
    let name = sanitize_name(raw.trim());
    if project_has_data(&project_dir(&name)) {
        Some(name)
    } else {
        None
    }
}

/// Persist `name` as the active project. Errors with `NotFound` when no
/// such indexed project exists, so `ug active <name>` can't silently point
/// at nothing. Returns the sanitized name that was written.
pub(crate) fn set_active_project(name: &str) -> std::io::Result<String> {
    let name = sanitize_name(name);
    if !project_has_data(&project_dir(&name)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "no indexed project '{}' under {}",
                name,
                ug_home().display()
            ),
        ));
    }
    let home = ug_home();
    std::fs::create_dir_all(&home)?;
    std::fs::write(active_path(), format!("{}\n", name))?;
    Ok(name)
}

/// Remove the active-project marker. A no-op (not an error) when unset.
pub(crate) fn clear_active_project() -> std::io::Result<()> {
    let p = active_path();
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// Rename a project: move `<ug_home>/<old>` to `<ug_home>/<new>`,
/// rewrite the `name` in its project.json, and re-point the active
/// marker when it named the old project. Nothing inside a project dir
/// records its own path or name, so the move is the whole migration.
///
/// Errors (leaving everything untouched) when `old` has no data dir,
/// when the two names sanitize to the same thing, or when `new` already
/// exists — a rename never merges into or clobbers another project.
/// Returns the sanitized new name.
pub(crate) fn rename_project(old: &str, new: &str) -> std::io::Result<String> {
    let old = sanitize_name(old);
    let new = sanitize_name(new);
    let old_dir = project_dir(&old);
    let new_dir = project_dir(&new);

    if !old_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no project '{}' under {}", old, ug_home().display()),
        ));
    }
    if new == old {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("'{}' is already this project's name", new),
        ));
    }
    if new_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "a project named '{}' already exists at {}",
                new,
                new_dir.display()
            ),
        ));
    }

    // Read before the move: get_active_project() only resolves while the
    // data is still at the old path.
    let was_active = get_active_project().as_deref() == Some(old.as_str());

    std::fs::rename(&old_dir, &new_dir)?;

    if let Some(mut meta) = read_meta(&new_dir) {
        meta.name = new.clone();
        // Written directly rather than through `write_meta`, which stamps
        // `updated_at`: a rename doesn't touch the graph, and bumping it
        // would reshuffle `ug list`'s most-recent-first ordering.
        let json = serde_json::to_string_pretty(&meta).expect("ProjectMeta serializes");
        std::fs::write(meta_path(&new_dir), json)?;
    }

    if was_active {
        set_active_project(&new)?;
    }

    Ok(new)
}

/// Delete a project's data directory (`graph.json`, `ugdb/`,
/// `project.json`, etc). Errors with `NotFound` instead of silently
/// no-op'ing when the directory doesn't exist, so callers (`ug rm`) can
/// report a clean error rather than a false "removed" message.
pub(crate) fn remove_project_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} does not exist", dir.display()),
        ));
    }

    // Containment guard on an irreversible operation. Every caller today
    // routes the name through `sanitize_name` (which maps `/` to `-` and
    // strips leading dots), so nothing can currently escape — but this
    // function takes an arbitrary `&Path` and hands it to `remove_dir_all`,
    // and "the caller sanitized it" is exactly the assumption that stops
    // being true when a new caller appears.
    //
    // Both sides are canonicalized: comparing a resolved path against an
    // unresolved `ug_home()` would reject every legitimate delete on macOS,
    // where `/var` and `/tmp` are symlinks. See Agents.md §9a.
    let home = ug_home();
    let canon_home = std::fs::canonicalize(&home).unwrap_or(home);
    let canon_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !canon_dir.starts_with(&canon_home) || canon_dir == canon_home {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to delete {}: not a project directory under {}",
                canon_dir.display(),
                canon_home.display()
            ),
        ));
    }

    std::fs::remove_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mark has to survive the metadata rewrite that every refresh does,
    /// and only an embedding run may clear it — otherwise the next `ug
    /// update` silently forgets that vectors are owed, which is the whole
    /// thing the mark exists to prevent.
    #[test]
    fn the_pending_vectors_mark_survives_a_refresh_and_ages_from_the_oldest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        write_meta(dir, &ProjectMeta::new("demo", "/repo", 1, 1)).expect("write");
        assert!(pending_vectors_age(dir).is_none(), "starts current");

        set_pending_vectors(dir, true);
        let first = read_meta(dir).expect("meta").pending_vectors_since;
        assert!(first > 0, "a skipped run marks the project");

        // A later refresh rebuilds ProjectMeta from scratch — the mark has to
        // ride along, and a second skipped run must not reset the clock.
        let refreshed =
            ProjectMeta::new("demo", "/repo", 2, 2).carrying_pending_vectors(dir);
        write_meta(dir, &refreshed).expect("write");
        set_pending_vectors(dir, true);
        assert_eq!(
            read_meta(dir).expect("meta").pending_vectors_since,
            first,
            "`since` must stay the oldest unpaid run"
        );

        // The run that embeds is the one that clears it.
        set_pending_vectors(dir, false);
        assert_eq!(read_meta(dir).expect("meta").pending_vectors_since, 0);
        assert!(pending_vectors_age(dir).is_none());
    }

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

    #[test]
    fn remove_project_dir_deletes_existing_dir() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!("ug-rm-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("UG_HOME", &home);

        // A project dir is always *inside* ug_home — that is the only shape
        // production ever passes, and the only shape the containment guard
        // in `remove_project_dir` accepts.
        let dir = home.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("graph.json"), "{}").unwrap();
        assert!(dir.exists());
        remove_project_dir(&dir).unwrap();
        assert!(!dir.exists());

        std::env::remove_var("UG_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The containment guard: `remove_project_dir` hands its argument to
    /// `remove_dir_all`, so a path outside `ug_home()` — or `ug_home()` itself
    /// — must be refused rather than deleted.
    #[test]
    fn remove_project_dir_refuses_paths_outside_ug_home() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!("ug-rm-guard-home-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("ug-rm-guard-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(outside.join("precious")).unwrap();
        std::env::set_var("UG_HOME", &home);

        assert!(
            remove_project_dir(&outside).is_err(),
            "a directory outside ug_home must not be deleted"
        );
        assert!(outside.exists(), "the refused directory must still be there");

        assert!(
            remove_project_dir(&home).is_err(),
            "ug_home itself is not a project dir and must not be wiped"
        );
        assert!(home.exists());

        std::env::remove_var("UG_HOME");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn remove_project_dir_errors_when_missing() {
        let dir = std::env::temp_dir().join(format!("ug-rm-test-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(remove_project_dir(&dir).is_err());
    }

    // $UG_HOME is process-wide; the lock lives at module scope so the serve
    // tests share it. See `super::UG_HOME_LOCK`.
    use super::UG_HOME_LOCK;

    #[test]
    fn rename_moves_data_updates_meta_and_follows_active() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!("ug-rename-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("UG_HOME", &home);

        let old_dir = project_dir("old");
        std::fs::create_dir_all(old_dir.join("ugdb")).unwrap();
        std::fs::write(old_dir.join("graph.json"), "{\"nodes\":[]}").unwrap();
        let mut meta = ProjectMeta::new("old", "/repo/old", 7, 9);
        write_meta(&old_dir, &meta).unwrap();
        meta = read_meta(&old_dir).unwrap();
        set_active_project("old").unwrap();

        assert_eq!(rename_project("old", "new name").unwrap(), "new-name");

        let new_dir = project_dir("new-name");
        assert!(!old_dir.exists(), "old dir is gone");
        assert!(new_dir.join("ugdb").exists(), "db moved with the project");
        assert_eq!(
            std::fs::read_to_string(new_dir.join("graph.json")).unwrap(),
            "{\"nodes\":[]}"
        );

        let moved = read_meta(&new_dir).unwrap();
        assert_eq!(moved.name, "new-name", "project.json renamed");
        assert_eq!(moved.repo_root, "/repo/old", "rest of the meta untouched");
        assert_eq!(moved.nodes, 7);
        assert_eq!(
            moved.updated_at, meta.updated_at,
            "a rename is not a data update"
        );
        assert_eq!(
            get_active_project().as_deref(),
            Some("new-name"),
            "active marker follows"
        );

        std::env::remove_var("UG_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rename_refuses_missing_same_and_occupied_names() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!("ug-rename-err-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("UG_HOME", &home);

        // Nothing there at all.
        assert!(rename_project("ghost", "other").is_err());

        let a = project_dir("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("graph.json"), "{}").unwrap();
        let b = project_dir("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("graph.json"), "{\"b\":1}").unwrap();

        // Same name (including via sanitization) is a no-op error.
        assert!(rename_project("a", "a").is_err());
        assert!(rename_project("a", "../a").is_err());

        // Renaming onto an existing project never clobbers it.
        assert!(rename_project("a", "b").is_err());
        assert!(a.join("graph.json").exists(), "source left in place");
        assert_eq!(
            std::fs::read_to_string(b.join("graph.json")).unwrap(),
            "{\"b\":1}",
            "target untouched"
        );

        std::env::remove_var("UG_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn active_project_roundtrip_and_validation() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!("ug-active-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("UG_HOME", &home);

        // No marker yet.
        assert_eq!(get_active_project(), None);

        // Setting an absent project fails and writes nothing.
        assert!(set_active_project("ghost").is_err());
        assert_eq!(get_active_project(), None);

        // Create a project with data, then set it active.
        let proj = project_dir("demo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("graph.json"), "{}").unwrap();
        assert_eq!(set_active_project("demo").unwrap(), "demo");
        assert_eq!(get_active_project().as_deref(), Some("demo"));

        // A stale marker (data removed) resolves to None rather than erroring.
        std::fs::remove_dir_all(&proj).unwrap();
        assert_eq!(get_active_project(), None);

        // Clear is a no-op-safe removal.
        clear_active_project().unwrap();
        assert!(!active_path().exists());
        clear_active_project().unwrap();

        std::env::remove_var("UG_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
