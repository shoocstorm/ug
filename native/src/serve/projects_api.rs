//! `projects_api.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::gen_jobs::GenJob;
use super::gen_jobs::{pump_gen_output, GenJobStatus};
use super::registry::{activate_project, build_placeholder_context, close_project_stores};
use super::*;

/// GET /api/projects — mode, active project, and the project list.
/// Multi mode re-lists from disk on every call so projects generated
/// after server start show up without a restart.
pub(crate) async fn api_projects(State(state): State<ServeState>) -> Response {
    let registry = &state.registry;
    let active = registry.active.read().expect("active poisoned").clone();
    let (mode, projects_json): (&str, Vec<serde_json::Value>) = match registry.mode {
        ServeMode::Single => {
            let ctx = registry.active_ctx();
            let snap = ctx.graph.read().expect("graph state poisoned").clone();
            (
                "single",
                vec![serde_json::json!({
                    "name": ctx.name,
                    "nodes": snap.parsed.nodes.len(),
                    "edges": snap.parsed.edges.len(),
                    "repoRoot": ctx.repo_root.display().to_string(),
                    "updatedAt": null,
                    "loaded": true,
                })],
            )
        }
        ServeMode::Multi => (
            "multi",
            crate::project::list_projects()
                .iter()
                .map(|(_, m)| {
                    serde_json::json!({
                        "name": m.name,
                        "nodes": m.nodes,
                        "edges": m.edges,
                        "repoRoot": m.repo_root,
                        "updatedAt": m.updated_at,
                        "loaded": registry.get_loaded(&m.name).is_some(),
                    })
                })
                .collect(),
        ),
    };
    let body = serde_json::json!({
        "mode": mode,
        "active": active,
        "projects": projects_json,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct ProjectSelectBody {
    name: String,
}

/// POST /api/projects/select — switch the server-side active project.
/// The UI reloads after this so every root-relative fetch picks up the
/// new project.
pub(crate) async fn api_projects_select(
    State(state): State<ServeState>,
    Json(body): Json<ProjectSelectBody>,
) -> Response {
    if state.registry.mode == ServeMode::Single {
        return err_json(
            StatusCode::BAD_REQUEST,
            "server is in single-project mode (started with -i); restart without -i to switch projects",
        );
    }
    let name = crate::project::sanitize_name(&body.name);
    match activate_project(&state.registry, &name).await {
        Ok(ctx) => {
            let snap = ctx.graph.read().expect("graph state poisoned").clone();
            ok_json(
                serde_json::json!({
                    "active": ctx.name,
                    "nodes": snap.parsed.nodes.len(),
                    "edges": snap.parsed.edges.len(),
                })
                .to_string(),
            )
        }
        Err(e) if e.starts_with("unknown project") => err_json(StatusCode::NOT_FOUND, &e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct ProjectDeleteBody {
    name: String,
}

/// The context to make active in place of `deleted`, resolved *without*
/// touching the registry.
///
/// Reuses the already-loaded context when there is one — building a second
/// would open a second handle on the same OverGraph directory, and the engine
/// is single-writer. `None` means no other project remains, so the caller
/// falls back to the placeholder.
async fn replacement_for_deleted(
    registry: &Arc<ProjectRegistry>,
    deleted: &str,
) -> Option<Arc<ProjectContext>> {
    let (dir, meta) = crate::project::list_projects()
        .into_iter()
        .find(|(_, m)| m.name != deleted)?;
    if let Some(ctx) = registry.get_loaded(&meta.name) {
        return Some(ctx);
    }
    match build_project_context(
        &meta.name,
        dir.join("graph.json"),
        dir.join("ugdb"),
        None,
        registry.no_db,
    )
    .await
    {
        Ok(ctx) => Some(ctx),
        Err(e) => {
            tracing::warn!(error = %e, "failed to build fallback project after delete");
            None
        }
    }
}

/// POST /api/projects/delete — delete a project's on-disk data
/// directory (mirrors `ug remove`) and drop it from the in-memory registry.
/// If the deleted project was active, falls back to another remaining
/// project, or the zero-project placeholder if none are left, so every
/// handler always has something to read from.
///
/// # Ordering
///
/// Three things have to happen in this order, and getting any of them wrong
/// is invisible until it isn't:
///
/// 1. **Resolve the replacement before dropping anything.** `active_ctx`
///    asserts the active project is loaded and every handler goes through it,
///    so `active` must never name a context that is absent from `loaded`.
///    Removing the entry first and *then* awaiting the fallback's load left a
///    window the length of a `graph.json` read + parse in which every request
///    panicked — including the watch loop's own tick, which killed live
///    reloading for the rest of the process.
/// 2. **Close the store before deleting its files.** OverGraph is embedded;
///    unlinking the directory under a live handle is the same hazard
///    [`close_project_stores`] exists for.
/// 3. **Swap the active selection before removing the deleted entry**, so the
///    two are never inconsistent in either direction.
pub(crate) async fn api_projects_delete(
    State(state): State<ServeState>,
    Json(body): Json<ProjectDeleteBody>,
) -> Response {
    if state.registry.mode == ServeMode::Single {
        return err_json(
            StatusCode::BAD_REQUEST,
            "server is in single-project mode (started with -i); restart without -i to manage projects",
        );
    }
    let name = crate::project::sanitize_name(&body.name);
    let dir = crate::project::project_dir(&name);

    // (1) Everything that can await happens while the registry is still
    // consistent: the project being deleted stays loaded and active until a
    // replacement is in hand.
    let was_active = *state.registry.active.read().expect("active poisoned") == name;
    let replacement = if was_active {
        replacement_for_deleted(&state.registry, &name).await
    } else {
        None
    };

    // (2) Drop this server's handle on the store before its files go away.
    // The context stays loaded (store-less) so `active_ctx` still resolves.
    let was_loaded = state.registry.get_loaded(&name).is_some();
    close_project_stores(&state.registry, &name).await;

    if let Err(e) = crate::project::remove_project_dir(&dir) {
        // Nothing was deleted, so put the store back rather than leaving a
        // live project permanently DB-less because a delete failed.
        if was_loaded {
            match build_project_context(
                &name,
                dir.join("graph.json"),
                dir.join("ugdb"),
                None,
                state.registry.no_db,
            )
            .await
            {
                Ok(ctx) => state.registry.insert_loaded(ctx),
                Err(reopen) => {
                    tracing::warn!(project = %name, error = %reopen, "failed delete left the store closed")
                }
            }
        }
        return err_json(
            StatusCode::NOT_FOUND,
            &format!("failed to remove '{}': {}", name, e),
        );
    }

    // (3) Activate the replacement first, drop the deleted entry second.
    if was_active {
        match replacement {
            Some(ctx) => state.registry.insert_and_activate(ctx),
            None => {
                build_placeholder_context(&state.registry);
            }
        }
    }
    state
        .registry
        .loaded
        .write()
        .expect("loaded poisoned")
        .remove(&name);
    state
        .registry
        .lru
        .write()
        .expect("lru poisoned")
        .retain(|n| n != &name);
    // The deleted project would otherwise keep appearing in the cached
    // staleness report until its TTL lapsed.
    *state.staleness.write().expect("staleness poisoned") = None;

    // Read back rather than tracked alongside: deleting a project that wasn't
    // active leaves the selection untouched, and this used to report the
    // *deleted* name as active in that case.
    let active_name = state
        .registry
        .active
        .read()
        .expect("active poisoned")
        .clone();

    tracing::info!(project = %name, "project deleted");
    ok_json(
        serde_json::json!({
            "removed": name,
            "active": active_name,
        })
        .to_string(),
    )
}

/// How long a computed staleness report stays fresh.
///
/// The KB Manager polls every 2 minutes (`STALENESS_POLL_MS`), and every open
/// tab polls independently. Caching for 60s collapses a burst of tabs into one
/// filesystem scan while still reacting well inside a single poll interval.
const STALENESS_TTL: Duration = Duration::from_secs(60);

/// Cached `/api/projects/staleness` payload plus when it was computed.
pub(crate) struct StalenessCache {
    computed_at: std::time::Instant,
    body: String,
}

/// One project's staleness row. Split out of the handler so the whole scan can
/// be handed to `spawn_blocking` as a single unit of plain sync work.
///
/// The scan itself lives in `project::staleness`, shared with `ug list` — the
/// CLI and the KB Manager must never disagree about whether a project is
/// stale, and two implementations of the same `stat` loop is how they would.
fn staleness_for_project(
    project_dir: &std::path::Path,
    meta: &crate::project::ProjectMeta,
) -> Option<serde_json::Value> {
    let s = crate::project::staleness(project_dir, meta)?;
    Some(serde_json::json!({
        "name": meta.name,
        "isStale": s.is_stale(),
        "repoMissing": s.repo_missing,
        "builtAt": s.built_at,
        "files": s.files,
        "changed": s.changed,
        "missing": s.missing,
        "kbKind": s.kb_kind(),
        "docNodes": s.doc_nodes,
        "codeNodes": s.code_nodes,
    }))
}

/// Walk every project and build the staleness payload. Pure blocking work —
/// directory enumeration plus one `stat` per indexed file.
fn compute_staleness_body(multi: bool) -> String {
    let projects = if multi {
        crate::project::list_projects()
    } else {
        vec![]
    };

    let rows: Vec<serde_json::Value> = projects
        .iter()
        .filter_map(|(dir, meta)| staleness_for_project(dir, meta))
        .collect();

    serde_json::json!({
        "projects": rows,
        "checkedAt": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    })
    .to_string()
}

/// GET /api/projects/staleness — check staleness for all projects.
/// Compares graph.json mtime against indexed files' mtimes and returns
/// changed/deleted counts. Runs on startup and every 2 minutes.
///
/// The scan is filesystem-bound (one `stat` per indexed file across every
/// project), so it runs on `spawn_blocking` rather than inline: doing it on a
/// runtime worker stalled every other in-flight request for the duration.
/// Results are cached for [`STALENESS_TTL`] so concurrent tabs share one scan.
pub(crate) async fn api_projects_staleness(State(state): State<ServeState>) -> Response {
    if let Some(cached) = state.staleness.read().expect("staleness poisoned").as_ref() {
        if cached.computed_at.elapsed() < STALENESS_TTL {
            return ok_json(cached.body.clone());
        }
    }

    let multi = state.registry.mode == ServeMode::Multi;
    let body = match tokio::task::spawn_blocking(move || compute_staleness_body(multi)).await {
        Ok(b) => b,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("staleness scan failed: {}", e),
            )
        }
    };

    *state.staleness.write().expect("staleness poisoned") = Some(StalenessCache {
        computed_at: std::time::Instant::now(),
        body: body.clone(),
    });
    ok_json(body)
}

#[derive(serde::Deserialize)]
pub(crate) struct GenerateBody {
    path: String,
    name: Option<String>,
    #[serde(default)]
    no_ingest: bool,
    /// Build vectors as part of the run. Opt-in, mirroring the CLI: the
    /// wizard's fast path indexes structure only, and semantic search /
    /// chat / tours are backfilled later by `/api/ingest` ("Ingest now").
    #[serde(default, alias = "withEmbed")]
    with_embed: bool,
}

/// POST /api/generate — KB Manager wizard entry point. Spawns `ug gen`
/// as a subprocess (reusing the exact same pipeline the CLI uses,
/// rather than duplicating it here) against `body.path`, and returns a
/// job id immediately; progress is polled via `/api/generate/status`.
/// Only available in multi-project mode — there's nowhere sensible to
/// discover a newly generated project from in single mode.
pub(crate) async fn api_generate(
    State(state): State<ServeState>,
    Json(body): Json<GenerateBody>,
) -> Response {
    if state.registry.mode == ServeMode::Single {
        return err_json(
            StatusCode::BAD_REQUEST,
            "generate is only available in multi-project mode",
        );
    }
    let raw_path = body.path.trim().to_string();
    // Confine before indexing: whatever is indexed here becomes a project
    // whose contents `/api/file` will then serve. Unrestricted, this is the
    // step that turns an unauthenticated port into a whole-machine read.
    let canon = match confine_to_browse_roots(Path::new(&raw_path)) {
        Ok(p) if p.is_dir() => p,
        Ok(_) => return err_json(StatusCode::BAD_REQUEST, "path is not a directory"),
        Err(e) => return err_json(e.status(), e.message()),
    };
    let name = body.name.as_deref().map(crate::project::sanitize_name);

    let id = state
        .gen_jobs
        .next_id
        .fetch_add(1, Ordering::SeqCst)
        .to_string();
    let job = Arc::new(RwLock::new(GenJob {
        status: GenJobStatus::Running,
        log: Vec::new(),
        project_name: None,
        error: None,
    }));
    state
        .gen_jobs
        .jobs
        .write()
        .expect("jobs poisoned")
        .insert(id.clone(), job.clone());

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ug"));
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("gen").arg("-i").arg(&canon);
    if let Some(n) = &name {
        cmd.arg("-n").arg(n);
    }
    if body.no_ingest {
        cmd.arg("--no-ingest");
    }
    if body.with_embed {
        cmd.arg("--with-embed");
    }
    // Quiet the ASCII-art banner `main()` prints on every invocation —
    // it would otherwise dominate the wizard's log viewer.
    cmd.env("UG_QUIET_LOGO", "1");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let fallback_name =
        name.unwrap_or_else(|| crate::project::derive_project_name(&canon.to_string_lossy()));
    // A finished `ug gen` changes the project list and every file mtime the
    // staleness scan looks at, so the cached report must not outlive it.
    let staleness = state.staleness.clone();

    tokio::spawn(async move {
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let mut j = job.write().expect("job poisoned");
                j.status = GenJobStatus::Error;
                j.error = Some(format!("failed to spawn ug gen: {}", e));
                return;
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let out_task = tokio::spawn(pump_gen_output(stdout, job.clone()));
        let err_task = tokio::spawn(pump_gen_output(stderr, job.clone()));

        let status = child.wait().await;
        let _ = out_task.await;
        let _ = err_task.await;

        *staleness.write().expect("staleness poisoned") = None;

        let mut j = job.write().expect("job poisoned");
        match status {
            Ok(s) if s.success() => {
                j.status = GenJobStatus::Done;
                j.project_name = Some(fallback_name);
            }
            Ok(s) => {
                j.status = GenJobStatus::Error;
                j.error = Some(format!("ug gen exited with {}", s));
            }
            Err(e) => {
                j.status = GenJobStatus::Error;
                j.error = Some(format!("failed to wait on ug gen: {}", e));
            }
        }
    });

    ok_json(serde_json::json!({ "jobId": id }).to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct GenJobQuery {
    job: String,
}

/// GET /api/generate/status?job=<id> — poll a generation job's status,
/// accumulated log lines, and (on success) the resulting project name.
pub(crate) async fn api_generate_status(
    State(state): State<ServeState>,
    Query(params): Query<GenJobQuery>,
) -> Response {
    let job = {
        let jobs = state.gen_jobs.jobs.read().expect("jobs poisoned");
        match jobs.get(&params.job) {
            Some(j) => j.clone(),
            None => return err_json(StatusCode::NOT_FOUND, "unknown job"),
        }
    };
    let j = job.read().expect("job poisoned");
    let status = match j.status {
        GenJobStatus::Running => "running",
        GenJobStatus::Done => "done",
        GenJobStatus::Error => "error",
    };
    ok_json(
        serde_json::json!({
            "status": status,
            "log": j.log,
            "projectName": j.project_name,
            "error": j.error,
        })
        .to_string(),
    )
}

#[derive(serde::Deserialize)]
pub(crate) struct IngestBody {
    /// Project to re-embed. Defaults to the active project, so the
    /// common case (the user just clicked "Ingest now" on the project
    /// they're already looking at) needs no parameter.
    name: Option<String>,
}

/// POST /api/ingest — kick off `ug ingest` against an already-indexed
/// project's `graph.json`. Used by the UI's "Ingest now" button when
/// `/api/capabilities` reports `search_ready=false`: the graph is loaded
/// but no vectors have been written (or the embedder was down last time).
///
/// Reuses the `GenJob` tracker from `/api/generate`, so progress is
/// polled with the same `/api/generate/status?job=<id>` endpoint. After
/// the subprocess exits successfully the active project's stores are
/// reopened in place so the new vectors show up without a server
/// restart — the UI just re-probes `/api/capabilities`.
pub(crate) async fn api_ingest(
    State(state): State<ServeState>,
    Json(body): Json<IngestBody>,
) -> Response {
    let project_name = body
        .name
        .as_deref()
        .map(crate::project::sanitize_name)
        .unwrap_or_else(|| state.active().name.clone());
    let dir = crate::project::project_dir(&project_name);
    let graph_path = dir.join("graph.json");
    let db_path = dir.join("ugdb");
    if !graph_path.exists() {
        return err_json(
            StatusCode::BAD_REQUEST,
            &format!(
                "project '{}' has no graph.json at {} — run `ug gen` first",
                project_name,
                graph_path.display()
            ),
        );
    }

    let id = state
        .gen_jobs
        .next_id
        .fetch_add(1, Ordering::SeqCst)
        .to_string();
    let job = Arc::new(RwLock::new(GenJob {
        status: GenJobStatus::Running,
        log: Vec::new(),
        project_name: Some(project_name.clone()),
        error: None,
    }));
    state
        .gen_jobs
        .jobs
        .write()
        .expect("jobs poisoned")
        .insert(id.clone(), job.clone());

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ug"));
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("ingest")
        .arg("-i")
        .arg(&graph_path)
        .arg("-o")
        .arg(&db_path);
    // Match the wizard: quiet the ASCII banner so the log viewer leads
    // with the actual progress, not the banner.
    cmd.env("UG_QUIET_LOGO", "1");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let registry = state.registry.clone();
    let job_project = project_name.clone();
    tokio::spawn(async move {
        // Drop this server's handle on the store before the subprocess
        // writes to it. OverGraph is an embedded single-writer engine:
        // two live handles on one directory corrupt the manifest, and the
        // reopen after the subprocess exits then fails with a `secondary
        // index references missing node label` error — which is exactly
        // what left `search_ready=false` and made the banner stick.
        close_project_stores(&registry, &job_project).await;

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let mut j = job.write().expect("job poisoned");
                j.status = GenJobStatus::Error;
                j.error = Some(format!("failed to spawn ug ingest: {}", e));
                return;
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let out_task = tokio::spawn(pump_gen_output(stdout, job.clone()));
        let err_task = tokio::spawn(pump_gen_output(stderr, job.clone()));

        let status = child.wait().await;
        let _ = out_task.await;
        let _ = err_task.await;

        // Reopen the project's stores now that the subprocess has exited.
        // Always rebuild — even if the ingest failed, the store-less
        // context swapped in above must be replaced or the project stays
        // permanently DB-less. When the project is still the active one
        // this also retires the "Ingest now" banner once search_ready
        // flips true.
        let rebuild = build_project_context(
            &job_project,
            graph_path.clone(),
            db_path.clone(),
            None,
            registry.no_db,
        )
        .await;
        match rebuild {
            Ok(ctx) => {
                registry
                    .loaded
                    .write()
                    .expect("loaded poisoned")
                    .insert(job_project.clone(), ctx.clone());
                let mut j = job.write().expect("job poisoned");
                if status.as_ref().map(|s| s.success()).unwrap_or(false) {
                    j.status = GenJobStatus::Done;
                } else {
                    j.status = GenJobStatus::Error;
                    j.error = Some(match status {
                        Ok(s) => format!("ug ingest exited with {}", s),
                        Err(e) => format!("failed to wait on ug ingest: {}", e),
                    });
                }
            }
            Err(e) => {
                tracing::warn!(project = %job_project, error = %e, "post-ingest store reopen failed");
                let mut j = job.write().expect("job poisoned");
                j.status = GenJobStatus::Error;
                j.error = Some(format!(
                    "ingest finished but reopening the store failed: {}",
                    e
                ));
            }
        }
    });

    ok_json(serde_json::json!({ "jobId": id, "project": project_name }).to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct BrowseDirQuery {
    path: Option<String>,
}

/// Directory trees the KB Manager's filesystem endpoints are allowed to
/// touch: the user's home, the project data dir, and the directory `ug
/// serve` was started in. `UG_BROWSE_ROOTS` (colon-separated) adds more,
/// for repos kept outside home on another volume.
///
/// The server has no authentication, so `/api/browse-dir` and
/// `/api/generate` are reachable by anything that can open a socket to the
/// port. Unconfined, they compose into a whole-machine read: browse to
/// `/etc` or `~/.ssh`, index it as a project, then pull the contents back
/// out through `/api/file` — which enforces "stay inside the repo root" but
/// happily uses whatever root the previous step just installed.
///
/// Recomputed per call rather than cached: `UG_HOME` and the process's
/// working directory can both change between requests, and a handful of
/// `canonicalize` calls is nothing next to the directory scan that follows.
fn browse_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if let Ok(c) = std::fs::canonicalize(&p) {
            if !roots.contains(&c) {
                roots.push(c);
            }
        }
    };
    for extra in std::env::var("UG_BROWSE_ROOTS")
        .unwrap_or_default()
        .split(':')
    {
        if !extra.trim().is_empty() {
            push(PathBuf::from(extra.trim()));
        }
    }
    if let Some(home) = dirs::home_dir() {
        push(home);
    }
    push(crate::project::ug_home());
    if let Ok(cwd) = std::env::current_dir() {
        push(cwd);
    }
    roots
}

/// Why a path was refused. The distinction matters to the caller: a path
/// that doesn't resolve is a bad request, while one that resolves outside
/// the allowed roots is a refusal to look there.
enum ConfineError {
    Invalid(String),
    Outside(String),
}

impl ConfineError {
    fn status(&self) -> StatusCode {
        match self {
            ConfineError::Invalid(_) => StatusCode::BAD_REQUEST,
            ConfineError::Outside(_) => StatusCode::FORBIDDEN,
        }
    }

    fn message(&self) -> &str {
        match self {
            ConfineError::Invalid(m) | ConfineError::Outside(m) => m,
        }
    }
}

/// Canonicalize `requested` and confirm it sits inside one of
/// [`browse_roots`]. The error names the roots — the UI needs that to
/// explain why a folder didn't open, and they're the user's own paths.
fn confine_to_browse_roots(requested: &Path) -> Result<PathBuf, ConfineError> {
    let canon = std::fs::canonicalize(requested)
        .map_err(|e| ConfineError::Invalid(format!("invalid path: {}", e)))?;
    let roots = browse_roots();
    if roots.iter().any(|r| canon.starts_with(r)) {
        return Ok(canon);
    }
    Err(ConfineError::Outside(format!(
        "{} is outside the allowed roots ({}). Set UG_BROWSE_ROOTS to add one.",
        canon.display(),
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// GET /api/browse-dir?path=<dir> — list subdirectories of `path` (or the
/// user's home directory when omitted) for the KB Manager wizard's folder
/// browser. Read-only; only ever lists directory entries. Resolves
/// symlinks/`..` via `canonicalize` so the returned `path`/`parent` are
/// always absolute, and falls back to the parent directory if `path`
/// happens to point at a file rather than a directory. The resolved path
/// must land inside [`browse_roots`]; anything else is a 403.
/// The scan itself: canonicalize, confine to [`browse_roots`], enumerate,
/// and stat a `.git` marker per child. Plain blocking IO, so the handler
/// runs it off the runtime — a directory on a stalled network mount would
/// otherwise pin a worker thread for as long as the filesystem takes to
/// answer.
fn browse_dir_body(requested: PathBuf) -> Result<String, (StatusCode, String)> {
    let confine =
        |p: &Path| confine_to_browse_roots(p).map_err(|e| (e.status(), e.message().to_string()));
    let canon = confine(&requested)?;
    let dir = if canon.is_dir() {
        canon
    } else {
        // A file was passed: show the folder holding it. Still inside a
        // root by construction, but re-checked rather than assumed.
        match canon.parent() {
            Some(parent) => confine(parent)?,
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "path is not a directory".to_string(),
                ))
            }
        }
    };

    let read = std::fs::read_dir(&dir).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("cannot read directory: {}", e),
        )
    })?;

    let mut entries: Vec<(String, serde_json::Value)> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_repo = path.join(".git").exists();
        entries.push((
            name.to_lowercase(),
            serde_json::json!({ "name": name, "path": path.to_string_lossy(), "isRepo": is_repo }),
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Only offer an "up" link the caller is actually allowed to follow —
    // at a root's own boundary there is nowhere further up to go.
    let parent = dir
        .parent()
        .filter(|p| confine(p).is_ok())
        .map(|p| p.to_string_lossy().to_string());

    Ok(serde_json::json!({
        "path": dir.to_string_lossy(),
        "parent": parent,
        "entries": entries.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    })
    .to_string())
}

pub(crate) async fn api_browse_dir(Query(params): Query<BrowseDirQuery>) -> Response {
    let requested = params
        .path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));

    match tokio::task::spawn_blocking(move || browse_dir_body(requested)).await {
        Ok(Ok(body)) => ok_json(body),
        Ok(Err((status, e))) => err_json(status, &e),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("browse task failed: {}", e),
        ),
    }
}

pub(crate) fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}
