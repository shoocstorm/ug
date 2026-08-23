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
/// This is the chain `gen` (when no input path is named — that is how a
/// re-run finds the project to re-run), `ingest`, and the read commands
/// use so a `ug active <name>` from outside an indexed repo lands on the
/// project the user pinned instead of silently picking the most recently
/// updated one. `gen` with an explicit path, and `index`/`graph`, keep
/// using [`resolve_project_name`] — they create a project from the named
/// tree and must not be redirected by the active marker.
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

pub(crate) fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Render epoch seconds as `YYYY-MM-DD HH:MM:SS` (UTC).
///
/// Every timestamp ug shows a user comes through here — `ug list`'s UPDATED
/// column and the git hook log's header — so the two are the same clock and
/// the same format when someone is correlating them.
pub(crate) fn format_epoch(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    // Days-from-civil algorithm (Howard Hinnant) — avoids a chrono dep.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
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
    /// When the oldest still-unpaid vector-less run happened (epoch
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

/// Where a project's index stands against the tree it was built from.
///
/// One `stat` per file recorded in `project.json`, compared against
/// `graph.json`'s mtime. Both `ug list` and `GET /api/projects/staleness`
/// report from this, so the CLI and the KB Manager can never disagree about
/// whether a project needs re-generating.
pub(crate) struct Staleness {
    /// graph.json's mtime — the moment the index describes.
    pub built_at: Option<u64>,
    pub files: usize,
    pub changed: usize,
    pub missing: usize,
    /// The indexed tree is gone. Not "every file was deleted": the index is
    /// simply frozen where it is, and counting its files as missing would
    /// report a catastrophe where there is only a moved checkout.
    pub repo_missing: bool,
    pub doc_nodes: usize,
    pub code_nodes: usize,
    /// The first few drifted paths, edited ones before deleted ones, capped at
    /// [`STALE_SAMPLE`].
    ///
    /// Counts are enough for a status column; they are not enough for the
    /// warning the structural commands print, because "3 changed" leaves an
    /// agent unable to tell whether the drift is in the files it just edited
    /// (refresh and re-ask) or somewhere it does not care about (proceed).
    /// Capped because this goes in a one-line warning, not a report.
    pub changed_sample: Vec<String>,
}

/// How many drifted paths [`Staleness`] carries. Four fits a terminal line
/// alongside the counts and is enough to recognise one's own edit burst.
pub(crate) const STALE_SAMPLE: usize = 4;

impl Staleness {
    pub(crate) fn is_stale(&self) -> bool {
        !self.repo_missing && (self.changed > 0 || self.missing > 0)
    }

    /// The drifted paths as a display list: up to [`STALE_SAMPLE`] names, then
    /// `+N more`. Empty when nothing drifted.
    pub(crate) fn changed_summary(&self) -> String {
        if self.changed_sample.is_empty() {
            return String::new();
        }
        let rest = (self.changed + self.missing).saturating_sub(self.changed_sample.len());
        let mut s = self.changed_sample.join(", ");
        if rest > 0 {
            s.push_str(&format!(", +{} more", rest));
        }
        s
    }

    /// Classify the KB by symbol composition: docs (markdown/PDF/office),
    /// code (source symbols), or mixed. File/Folder nodes are structural and
    /// were excluded upstream by [`ProjectMeta::with_graph_index`].
    pub(crate) fn kb_kind(&self) -> &'static str {
        if self.repo_missing {
            "unknown"
        } else if self.doc_nodes > 0 && self.code_nodes > 0 {
            "mixed"
        } else if self.doc_nodes > self.code_nodes {
            "docs"
        } else {
            "code"
        }
    }
}

/// Stat the tree behind `meta` and report how far its index has drifted.
/// `None` when the project holds no `graph.json` — there is no index to
/// compare against.
pub(crate) fn staleness(project_dir: &Path, meta: &ProjectMeta) -> Option<Staleness> {
    let graph_path = project_dir.join("graph.json");
    if !graph_path.exists() {
        return None;
    }

    let built_at = std::fs::metadata(&graph_path).ok().and_then(|m| {
        m.modified()
            .ok()
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
    });

    // Prefer the file list recorded in project.json at `ug gen` time. Falling
    // back to reading graph.json keeps projects generated before that field
    // existed working — at the old cost, but only for them.
    let (files, doc_nodes, code_nodes) = if !meta.files.is_empty() {
        (meta.files.clone(), meta.doc_nodes, meta.code_nodes)
    } else {
        match std::fs::read_to_string(&graph_path)
            .ok()
            .and_then(|c| serde_json::from_str::<ultragraph::types::GraphData>(&c).ok())
        {
            Some(graph) => {
                let derived =
                    ProjectMeta::new(&meta.name, &meta.repo_root, 0, 0).with_graph_index(&graph);
                (derived.files, derived.doc_nodes, derived.code_nodes)
            }
            None => (Vec::new(), 0, 0),
        }
    };

    let repo_root = PathBuf::from(&meta.repo_root);
    if !repo_root.exists() {
        return Some(Staleness {
            built_at,
            files: files.len(),
            changed: 0,
            missing: 0,
            repo_missing: true,
            doc_nodes: 0,
            code_nodes: 0,
            changed_sample: Vec::new(),
        });
    }

    let mut changed = 0usize;
    let mut missing = 0usize;
    // Edited paths first, deleted ones only if there is room left: an edit is
    // the drift an agent can act on by re-running `ug update`, while a delete
    // it already knows about.
    let mut edited_sample: Vec<String> = Vec::new();
    let mut deleted_sample: Vec<String> = Vec::new();
    for file in &files {
        match std::fs::metadata(repo_root.join(file)) {
            Ok(metadata) => {
                if let (Ok(modified), Some(built)) = (metadata.modified(), built_at) {
                    let file_mtime = modified
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if file_mtime > built {
                        changed += 1;
                        if edited_sample.len() < STALE_SAMPLE {
                            edited_sample.push(file.clone());
                        }
                    }
                }
            }
            Err(_) => {
                missing += 1;
                if deleted_sample.len() < STALE_SAMPLE {
                    deleted_sample.push(format!("{} (deleted)", file));
                }
            }
        }
    }
    let mut changed_sample = edited_sample;
    changed_sample.extend(deleted_sample);
    changed_sample.truncate(STALE_SAMPLE);

    Some(Staleness {
        built_at,
        files: files.len(),
        changed,
        missing,
        repo_missing: false,
        doc_nodes,
        code_nodes,
        changed_sample,
    })
}

/// Bytes on disk under `dir`, walked recursively.
///
/// What `ug list` reports as a project's size — almost entirely `ugdb/` and
/// `graph.json`, and the number a user needs before deciding what to `ug rm`.
/// Symlinks are counted as the links they are rather than followed: nothing
/// ug writes into a project dir is one, so a symlink here is not ours to
/// traverse.
pub(crate) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => dir_size(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
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

/// Default db path for read commands (chat, search, …) when
/// no `-n/--name` flag is given:
/// the active project's `~/.ug/<active>/ugdb` if set and present →
/// `~/.ug/<cwd-basename>/ugdb` if it exists → legacy `./.ug/ugdb` if it
/// exists → the most recently updated project under `~/.ug` (covers
/// running a read command from outside any indexed repo) →
/// `~/.ug/<cwd-basename>/ugdb` (so error messages point users at the
/// new layout).
pub(crate) fn default_read_db_path() -> String {
    default_read_db_path_with_origin().0
}

/// [`default_read_db_path`], plus a label naming which link of the chain
/// produced the path.
///
/// The label is what the scope banner prints. A read that silently fell
/// through to "most recently updated project" is precisely the case a user
/// needs told about — it is how a question about one repo gets a confident
/// answer about another.
pub(crate) fn default_read_db_path_with_origin() -> (String, &'static str) {
    // The active marker is the same one `serve` and `mcp` honor, so a
    // read from outside an indexed repo lands where `ug active` pointed
    // instead of on whatever was touched last. Only used when its db
    // actually exists — an active project that was never ingested falls
    // through to the rest of the chain.
    if let Some(name) = get_active_project() {
        let active_path = project_dir(&name).join("ugdb");
        if active_path.exists() {
            return (active_path.to_string_lossy().into_owned(), "active project");
        }
    }
    let new_path = project_dir(&derive_project_name(".")).join("ugdb");
    if new_path.exists() {
        return (new_path.to_string_lossy().into_owned(), "current directory");
    }
    let legacy = Path::new(".ug/ugdb");
    if legacy.exists() {
        return (".ug/ugdb".to_string(), "legacy ./.ug/ugdb");
    }
    if let Some((dir, _meta)) = list_projects().into_iter().next() {
        let fallback = dir.join("ugdb");
        if fallback.exists() {
            return (
                fallback.to_string_lossy().into_owned(),
                "most recently updated project",
            );
        }
    }
    (
        new_path.to_string_lossy().into_owned(),
        "current directory (not generated yet)",
    )
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

    /// Backdate a file's mtime so a test can produce a genuinely "newer"
    /// file without sleeping. The staleness comparison is in whole seconds,
    /// so a source and a graph written in the same tick read as current and
    /// the test would pass for the wrong reason.
    fn backdate(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).expect("open");
        let when = SystemTime::now() - std::time::Duration::from_secs(secs);
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
    }

    /// The three states `ug list` and `/api/projects/staleness` report from,
    /// each of which sends the user somewhere different: edit a file and it
    /// is `changed`, delete one and it is `missing`, move the whole checkout
    /// and neither count means anything.
    #[test]
    fn staleness_separates_edited_deleted_and_a_vanished_repo() {
        let repo = tempfile::tempdir().expect("repo");
        let data = tempfile::tempdir().expect("data");
        std::fs::write(repo.path().join("a.rs"), "fn a() {}").expect("a");
        std::fs::write(repo.path().join("b.rs"), "fn b() {}").expect("b");

        let mut meta = ProjectMeta::new("demo", &repo.path().to_string_lossy(), 2, 1);
        meta.files = vec!["a.rs".to_string(), "b.rs".to_string()];
        meta.code_nodes = 2;
        write_meta(data.path(), &meta).expect("meta");
        std::fs::write(data.path().join("graph.json"), "{}").expect("graph");
        // Sources at T-120, the graph built from them at T-60. Both are in the
        // past so the edit below lands strictly after the graph — the
        // comparison is in whole seconds, and a same-tick edit reads as
        // current.
        backdate(&repo.path().join("a.rs"), 120);
        backdate(&repo.path().join("b.rs"), 120);
        backdate(&data.path().join("graph.json"), 60);

        let fresh = staleness(data.path(), &meta).expect("has a graph");
        assert_eq!((fresh.changed, fresh.missing), (0, 0));
        assert!(!fresh.is_stale(), "nothing moved since the graph was built");
        assert_eq!(fresh.files, 2);
        assert_eq!(fresh.kb_kind(), "code");
        assert!(fresh.changed_summary().is_empty(), "nothing to name");

        // One edited, one deleted — counted separately, because only the
        // second means the file list itself is wrong.
        std::fs::write(repo.path().join("a.rs"), "fn a() { todo!() }").expect("edit");
        std::fs::remove_file(repo.path().join("b.rs")).expect("delete");
        let drifted = staleness(data.path(), &meta).expect("has a graph");
        assert_eq!((drifted.changed, drifted.missing), (1, 1));
        assert!(drifted.is_stale());
        // Named, not just counted — the staleness warning the structural
        // commands print has to let an agent recognise its own edit burst,
        // which "1 changed" cannot. Edited before deleted.
        assert_eq!(
            drifted.changed_sample,
            vec!["a.rs".to_string(), "b.rs (deleted)".to_string()]
        );
        assert_eq!(drifted.changed_summary(), "a.rs, b.rs (deleted)");

        // A repo that is gone is not "every file deleted": the index is
        // frozen, and reporting 2 missing files would send the user chasing
        // deletions that never happened.
        let mut orphan = meta.clone();
        orphan.repo_root = "/nonexistent/tree".to_string();
        let gone = staleness(data.path(), &orphan).expect("has a graph");
        assert!(gone.repo_missing);
        assert!(!gone.is_stale(), "a moved checkout is not drift");
        assert_eq!((gone.changed, gone.missing), (0, 0));
    }

    /// The sample is capped and says how much it left out. Without the
    /// `+N more`, a burst that touched 40 files and one that touched 5 would
    /// print the same four names and read as equally small.
    #[test]
    fn changed_sample_caps_and_reports_the_remainder() {
        let repo = tempfile::tempdir().expect("repo");
        let data = tempfile::tempdir().expect("data");
        let names: Vec<String> = (0..10).map(|i| format!("f{}.rs", i)).collect();
        for n in &names {
            std::fs::write(repo.path().join(n), "fn x() {}").expect("write");
        }

        let mut meta = ProjectMeta::new("demo", &repo.path().to_string_lossy(), 10, 0);
        meta.files = names.clone();
        write_meta(data.path(), &meta).expect("meta");
        std::fs::write(data.path().join("graph.json"), "{}").expect("graph");
        // Graph older than every source, so all ten read as changed.
        backdate(&data.path().join("graph.json"), 60);

        let stale = staleness(data.path(), &meta).expect("has a graph");
        assert_eq!(stale.changed, 10);
        assert_eq!(stale.changed_sample.len(), STALE_SAMPLE);
        let summary = stale.changed_summary();
        assert!(
            summary.ends_with(&format!("+{} more", 10 - STALE_SAMPLE)),
            "{summary}"
        );
    }

    /// No graph.json means there is no index to compare the tree against —
    /// distinct from "an index that happens to be current".
    #[test]
    fn staleness_is_none_without_a_graph() {
        let data = tempfile::tempdir().expect("data");
        let meta = ProjectMeta::new("demo", "/repo", 0, 0);
        write_meta(data.path(), &meta).expect("meta");
        assert!(staleness(data.path(), &meta).is_none());
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("top"), vec![0u8; 100]).expect("top");
        std::fs::create_dir_all(dir.path().join("ugdb/seg")).expect("nested");
        std::fs::write(dir.path().join("ugdb/seg/data"), vec![0u8; 250]).expect("nested file");
        assert_eq!(dir_size(dir.path()), 350);
        assert_eq!(dir_size(Path::new("/nonexistent/tree")), 0);
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
