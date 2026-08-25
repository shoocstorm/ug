//! `registry.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::OnceCell;

use ultragraph::storage::{open_store, KnowledgeStore, StoreSpec, DEFAULT_EMBEDDING_DIM};

use super::watch::refresh_snapshot_if_stale;
use super::*;

/// Build the `StoreSpec`s for `ug serve` from env vars. `UG_DEST` is
/// comma-separated — when more than one backend is listed, the server
/// opens all of them and the UI shows a destination selector. The
/// first item parsed becomes the primary (default for requests that
/// One or more backends `ug serve` is wired up to. Populated when
/// `UG_DEST` lists one or more backend names; reads route to the
/// caller-selected dest (via a `dest` field on each search/traverse
/// request) or fall back to `primary`.
/// don't specify a dest).
pub(crate) fn build_serve_store_specs(db_path: &PathBuf) -> Vec<StoreSpec> {
    let dest = std::env::var("UG_DEST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "overgraph".to_string());
    let dim = DEFAULT_EMBEDDING_DIM as u32;
    let mut specs: Vec<StoreSpec> = Vec::new();
    for kind in dest
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
    {
        match kind.as_str() {
            "neo4j" | "neo" => {
                let uri =
                    std::env::var("UG_NEO4J_URI").expect("UG_DEST=neo4j requires UG_NEO4J_URI");
                let user = std::env::var("UG_NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string());
                let password = std::env::var("UG_NEO4J_PASSWORD")
                    .expect("UG_DEST=neo4j requires UG_NEO4J_PASSWORD");
                let database = std::env::var("UG_NEO4J_DATABASE").ok();
                specs.push(StoreSpec::Neo4j {
                    uri,
                    user,
                    password,
                    database,
                    embedding_dim: dim,
                });
            }
            "overgraph" | "og" => specs.push(StoreSpec::Overgraph {
                path: db_path.clone(),
                embedding_dim: dim,
            }),
            other => panic!(
                "UG_DEST contains unknown backend '{}' (expected: overgraph, neo4j)",
                other
            ),
        }
    }
    if specs.is_empty() {
        specs.push(StoreSpec::Overgraph {
            path: db_path.clone(),
            embedding_dim: dim,
        });
    }
    specs
}
use ultragraph::types::GraphData;

pub(crate) struct ServeStores {
    /// All opened stores keyed by backend name (`"overgraph"`, `"neo4j"`, …).
    pub(crate) map: HashMap<String, Arc<dyn KnowledgeStore>>,
    /// Default destination — first one parsed from `UG_DEST`.
    pub(crate) primary: String,
    /// Per-destination cached node-count probes. Populated on the first
    /// `/api/capabilities` poll, then reused for the rest of the
    /// session (the server itself doesn't write, so the count is
    /// effectively static).
    pub(crate) node_counts: HashMap<String, OnceCell<Option<usize>>>,
    /// Per-destination open failure reasons. Lets `/api/capabilities`
    /// tell the operator which backends came up and which didn't.
    pub(crate) open_errors: HashMap<String, String>,
}

impl ServeStores {
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn KnowledgeStore>> {
        self.map.get(name)
    }

    /// Reserved for future routes that hard-route to the primary; the
    /// per-request `pick_store` helper covers the current handlers.
    #[allow(dead_code)]
    pub(crate) fn primary_store(&self) -> &Arc<dyn KnowledgeStore> {
        self.map
            .get(&self.primary)
            .expect("primary backend always present in map")
    }

    /// Ordered list of available backend names. Sorted alphabetically
    /// so the UI selector renders deterministically across reloads.
    pub(crate) fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Everything the handlers need for one project: its graph snapshot,
/// opened stores, and repo root. In multi-project mode one of these is
/// built lazily per project the first time it's selected; in
/// single-project mode (`-i`) there is exactly one.
pub(crate) struct ProjectContext {
    pub(crate) name: String,
    pub(crate) graph_path: PathBuf,
    pub(crate) repo_root: PathBuf,
    pub(crate) graph: RwLock<Arc<GraphSnapshot>>,
    /// `None` when `--no-db` is set or every configured store failed
    /// to open. Phase 3 routes return 503 in that case rather than
    /// panicking the server. With multi-dest, this is `Some` as long
    /// as at least one backend opened; per-dest readiness is reported
    /// in `/api/capabilities`.
    pub(crate) stores: Option<Arc<ServeStores>>,
    /// Reason all configured Phase 3 backends are unavailable —
    /// surfaced verbatim in 503s so the operator can tell `--no-db`
    /// apart from real connection failures. Per-dest errors live on
    /// `ServeStores::open_errors`.
    pub(crate) db_unavailable_reason: Option<String>,
}

impl ProjectContext {
    /// Rough resident size of this project's snapshot, for cache accounting.
    ///
    /// The encoded buffers are measured exactly. `parsed` is estimated at 3×
    /// the identity bytes: a `GraphData` of `String`-heavy structs runs
    /// noticeably larger than its JSON, and over-estimating only makes the
    /// cache more conservative. Precision isn't the point — staying off an
    /// unbounded growth curve is.
    pub(crate) fn approx_bytes(&self) -> usize {
        let snap = self.graph.read().expect("graph poisoned");
        let identity = snap.graph_bytes;
        identity
            .saturating_mul(3)
            // `retained`, not identity + both encodings: an encoding nobody
            // has requested has not been built and is costing nothing, so
            // charging the project for it would evict live snapshots to make
            // room for memory that was never allocated. A snapshot above the
            // server-mode cutoff holds no `graph_asset` at all, and is charged
            // only for the parsed graph.
            .saturating_add(snap.graph_asset.as_ref().map_or(0, |a| a.retained()))
            // The slim index is another whole encoded asset once it has been
            // asked for — ~34 MB identity plus whatever compressions have been
            // served. Uncounted, the LRU would hold three of them for free.
            .saturating_add(snap.slim.get().map_or(0, |s| s.retained()))
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum ServeMode {
    /// Explicit `-i <graph.json>` — exactly one project, no switcher.
    Single,
    /// Rooted at `ug_home()`; projects discovered from disk and
    /// switchable at runtime via `POST /api/projects/select`.
    Multi,
}

/// Which project the handlers read from. The active project is a
/// server-side selection (one per process): switching swaps what every
/// root-relative route (`/graph.json`, `/api/*`) resolves to, so the
/// UI just reloads after a switch.
pub(crate) struct ProjectRegistry {
    pub(crate) mode: ServeMode,
    pub(crate) no_db: bool,
    pub(crate) active: RwLock<String>,
    pub(crate) loaded: RwLock<HashMap<String, Arc<ProjectContext>>>,
    /// Recency order over `loaded`, least-recently-used first. Kept
    /// alongside rather than inside the map so the hot read path
    /// (`active_ctx`) stays a plain lookup.
    pub(crate) lru: RwLock<Vec<String>>,
    /// Byte ceiling for cached snapshots — see [`snapshot_cache_budget`].
    pub(crate) cache_budget: usize,
}

/// How many bytes of graph snapshot `ug serve` keeps resident across all
/// projects before it starts evicting.
///
/// `loaded` used to grow without bound: `resolve_ctx` loads a project on demand
/// for any request carrying `?project=<name>`, so an agent walking every
/// project pinned every snapshot for the life of the process — half a gigabyte
/// across six mid-size repos. 512 MiB keeps the common case (a handful of
/// projects) entirely cached while putting a ceiling on the pathological one.
pub(crate) fn snapshot_cache_budget() -> usize {
    const DEFAULT: usize = 512 * 1024 * 1024;
    std::env::var("UG_SERVE_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT)
}

impl ProjectRegistry {
    pub(crate) fn active_ctx(&self) -> Arc<ProjectContext> {
        let name = self.active.read().expect("active poisoned").clone();
        self.loaded
            .read()
            .expect("loaded poisoned")
            .get(&name)
            .cloned()
            .expect("active project is always loaded")
    }

    pub(crate) fn get_loaded(&self, name: &str) -> Option<Arc<ProjectContext>> {
        let hit = self
            .loaded
            .read()
            .expect("loaded poisoned")
            .get(name)
            .cloned();
        if hit.is_some() {
            self.touch(name);
        }
        hit
    }

    /// Move `name` to the most-recently-used end of the LRU order.
    pub(crate) fn touch(&self, name: &str) {
        let mut lru = self.lru.write().expect("lru poisoned");
        lru.retain(|n| n != name);
        lru.push(name.to_string());
    }

    /// Cache a loaded project without changing the active selection.
    /// Per-request project scoping uses this: a read targeting another
    /// project must not reconfigure the UI for every other client.
    pub(crate) fn insert_loaded(&self, ctx: Arc<ProjectContext>) {
        let name = ctx.name.clone();
        self.loaded
            .write()
            .expect("loaded poisoned")
            .insert(name.clone(), ctx);
        self.touch(&name);
        self.evict_over_budget();
    }

    pub(crate) fn insert_and_activate(&self, ctx: Arc<ProjectContext>) {
        let name = ctx.name.clone();
        self.loaded
            .write()
            .expect("loaded poisoned")
            .insert(name.clone(), ctx);
        *self.active.write().expect("active poisoned") = name.clone();
        self.touch(&name);
        self.evict_over_budget();
    }

    pub(crate) fn set_active(&self, name: &str) {
        *self.active.write().expect("active poisoned") = name.to_string();
    }

    /// Drop least-recently-used snapshots until the cache fits its budget.
    ///
    /// The active project is never evicted — [`active_ctx`] asserts it is
    /// loaded, and dropping it would panic every subsequent request. Evicting
    /// is safe for in-flight work regardless: handlers hold their own `Arc`
    /// clone, so removing the registry's reference frees the memory only once
    /// the last reader is done with it.
    pub(crate) fn evict_over_budget(&self) {
        // `active` before `loaded`, matching `active_ctx`'s order, so the two
        // paths can't deadlock against each other.
        let active = self.active.read().expect("active poisoned").clone();
        let mut loaded = self.loaded.write().expect("loaded poisoned");
        let mut lru = self.lru.write().expect("lru poisoned");

        let mut total: usize = loaded.values().map(|c| c.approx_bytes()).sum();
        if total <= self.cache_budget {
            return;
        }

        let mut idx = 0;
        while total > self.cache_budget && idx < lru.len() {
            let name = lru[idx].clone();
            if name == active {
                idx += 1; // never evict the active project
                continue;
            }
            match loaded.remove(&name) {
                Some(ctx) => {
                    total = total.saturating_sub(ctx.approx_bytes());
                    tracing::debug!(
                        project = %name,
                        freed_bytes = ctx.approx_bytes(),
                        "evicted graph snapshot to stay within cache budget"
                    );
                }
                None => {
                    // Stale LRU entry (project deleted); just drop it.
                }
            }
            lru.remove(idx);
        }
    }
}

/// Drop a project's open store handles so a `ug ingest` subprocess can be
/// the sole writer to its OverGraph directory. The engine is embedded and
/// single-writer: keeping the server's handle open while the subprocess
/// writes corrupts the manifest, and the post-ingest reopen then fails
/// with `secondary index references missing node label`, leaving
/// `search_ready=false` forever.
///
/// The loaded context is swapped for one that shares the same graph
/// snapshot but has `stores: None`, so the active project stays loadable
/// and DB-backed routes 503 until the ingest lands. The caller must
/// rebuild the project (with stores) once the subprocess exits.
pub(crate) async fn close_project_stores(registry: &Arc<ProjectRegistry>, name: &str) {
    let Some(ctx) = registry.get_loaded(name) else {
        return;
    };
    let graph = ctx.graph.read().expect("graph poisoned").clone();
    let closed = Arc::new(ProjectContext {
        name: name.to_string(),
        graph_path: ctx.graph_path.clone(),
        repo_root: ctx.repo_root.clone(),
        graph: RwLock::new(graph),
        stores: None,
        db_unavailable_reason: Some(
            "store closed for re-ingest; DB routes unavailable until it finishes".to_string(),
        ),
    });
    registry
        .loaded
        .write()
        .expect("loaded poisoned")
        .insert(name.to_string(), closed);
}

/// Build the per-project context: snapshot off the runtime (parse +
/// recompress is CPU-heavy), stores via the same env-driven specs as
/// before. `repo_root` comes from the project's project.json when
/// present so file preview works no matter where the server was
/// started; explicit `repo_root_override` (single mode) wins.
pub(crate) async fn build_project_context(
    name: &str,
    graph_path: PathBuf,
    db_path: PathBuf,
    repo_root_override: Option<PathBuf>,
    no_db: bool,
) -> Result<Arc<ProjectContext>, String> {
    let path_for_load = graph_path.clone();
    let snapshot = tokio::task::spawn_blocking(move || load_snapshot(&path_for_load))
        .await
        .map_err(|e| format!("snapshot task: {}", e))??;

    let repo_root = repo_root_override
        .or_else(|| {
            graph_path
                .parent()
                .and_then(crate::project::read_meta)
                .map(|m| PathBuf::from(m.repo_root))
                .filter(|p| !p.as_os_str().is_empty())
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // Canonicalize here so the invariant holds no matter which caller supplied
    // the path. `file_from_disk` compares a *canonicalized* candidate against
    // this root, so a root that still contains a symlink component (macOS
    // `/tmp` -> `/private/tmp`, `/var` -> `/private/var`, or a project.json
    // written before `ug gen` started canonicalizing) fails that prefix check
    // and turns every legitimate file preview into a 403 "path escapes repo
    // root". A root that doesn't exist keeps its raw form — the index still
    // serves content without it.
    let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
    if !repo_root.exists() {
        tracing::warn!(
            project = %name,
            repo_root = %repo_root.display(),
            "repo root does not exist; serving source from the index"
        );
    }

    let (stores, db_unavailable_reason) = open_serve_stores(&db_path, no_db).await;

    Ok(Arc::new(ProjectContext {
        name: name.to_string(),
        graph_path,
        repo_root,
        graph: RwLock::new(snapshot),
        stores,
        db_unavailable_reason,
    }))
}

/// Zero-project startup: rather than failing to start, register an
/// empty placeholder project and activate it so every handler still
/// has something to read from (`GET /graph.json` just returns an empty
/// graph). The KB Manager screen shows the "generate from scratch"
/// wizard whenever `/api/projects` reports zero real projects; once
/// the user generates or selects one, `activate_project` replaces this
/// as the active context.
pub(crate) fn build_placeholder_context(registry: &Arc<ProjectRegistry>) -> Arc<ProjectContext> {
    let empty_graph = GraphData {
        nodes: Vec::new(),
        edges: Vec::new(),
        stats: None,
        resolution: None,
    };
    let raw_json = serde_json::to_string(&empty_graph)
        .unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string());
    let graph_bytes = raw_json.len();
    let encoded = EncodedAsset::new(raw_json.into_bytes(), "application/json; charset=utf-8");
    let snapshot = Arc::new(GraphSnapshot {
        graph_asset: Some(encoded),
        graph_bytes,
        parsed: empty_graph,
        // No file behind it, so nothing to check it against.
        mtime: None,
        adj: OnceLock::new(),
        centrality: OnceLock::new(),
        cycles: OnceLock::new(),
        slim: OnceLock::new(),
        slim_bin: OnceLock::new(),
        stats: OnceLock::new(),
        search_memo: std::sync::Mutex::new(None),
    });
    let ctx = Arc::new(ProjectContext {
        name: "__none__".to_string(),
        graph_path: PathBuf::new(),
        repo_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        graph: RwLock::new(snapshot),
        stores: None,
        db_unavailable_reason: Some("no knowledge base selected yet".to_string()),
    });
    registry.insert_and_activate(ctx.clone());
    ctx
}

/// Open every store listed in `UG_DEST` for `db_path`. Per-dest open
/// failures are non-fatal as long as at least one backend opens; the
/// operator sees per-dest status on `/api/capabilities`.
pub(crate) async fn open_serve_stores(
    db_path: &PathBuf,
    no_db: bool,
) -> (Option<Arc<ServeStores>>, Option<String>) {
    if no_db {
        return (None, Some("started with --no-db".to_string()));
    }
    let specs = build_serve_store_specs(db_path);
    let mut map: HashMap<String, Arc<dyn KnowledgeStore>> = HashMap::new();
    let mut node_counts: HashMap<String, OnceCell<Option<usize>>> = HashMap::new();
    let mut open_errors: HashMap<String, String> = HashMap::new();
    let mut primary: Option<String> = None;
    for spec in specs.iter() {
        let name = spec.name().to_string();
        match open_store(spec).await {
            Ok(store) => {
                tracing::info!(backend = %name, db = %db_path.display(), "store opened");
                if primary.is_none() {
                    primary = Some(name.clone());
                }
                map.insert(name.clone(), Arc::from(store));
                node_counts.insert(name, OnceCell::new());
            }
            Err(e) => {
                let reason = format!("{}", e);
                tracing::warn!(error = %reason, backend = %name, "store open failed");
                open_errors.insert(name, reason);
            }
        }
    }
    if let Some(primary) = primary {
        (
            Some(Arc::new(ServeStores {
                map,
                primary,
                node_counts,
                open_errors,
            })),
            None,
        )
    } else {
        // All backends failed to open — report all errors so the
        // operator can see what went wrong.
        let summary = if open_errors.is_empty() {
            "no destinations configured".to_string()
        } else {
            let parts: Vec<String> = open_errors
                .iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect();
            format!("all backends failed: {}", parts.join("; "))
        };
        (None, Some(summary))
    }
}

// ---------- Project switching (multi-project mode) ----------

/// Activate a project by name: reuse the cached context if it was
/// loaded before, otherwise discover it on disk under `ug_home()` and
/// build a fresh context (snapshot + stores). Errors are strings for
/// direct surfacing in API responses.
pub(crate) async fn activate_project(
    registry: &Arc<ProjectRegistry>,
    name: &str,
) -> Result<Arc<ProjectContext>, String> {
    if let Some(ctx) = registry.get_loaded(name) {
        refresh_snapshot_if_stale(&ctx).await;
        registry.set_active(name);
        return Ok(ctx);
    }
    let projects = crate::project::list_projects();
    let (dir, _meta) = projects
        .into_iter()
        .find(|(_, m)| m.name == name)
        .ok_or_else(|| format!("unknown project '{}'", name))?;
    let graph_path = dir.join("graph.json");
    let db_path = dir.join("ugdb");
    let ctx = build_project_context(name, graph_path, db_path, None, registry.no_db).await?;
    registry.insert_and_activate(ctx.clone());
    tracing::info!(project = %name, "project activated");
    Ok(ctx)
}

/// The source this project's index captured for whatever `tool` is about to
/// read, from its primary store.
///
/// Empty when the project has no open store (`--no-db`, an index that would
/// not open), in which case the tool falls back to the working tree — the
/// server may well be running inside the repo, and if it isn't the tool
/// reports that rather than serving wrong lines.
pub(crate) async fn ctx_indexed_source(
    ctx: &ProjectContext,
    graph: &GraphData,
    tool: &str,
    args: &serde_json::Value,
) -> ultragraph::agent_tools::IndexedSource {
    let ids = ultragraph::agent_tools::source_node_ids(tool, graph, args);
    if ids.is_empty() {
        return Default::default();
    }
    let Some(stores) = ctx.stores.as_ref() else {
        return Default::default();
    };
    let Some(store) = stores.get(&stores.primary) else {
        return Default::default();
    };
    ultragraph::agent_tools::IndexedSource::load(store.as_ref(), &ids).await
}

/// Resolve which project a request targets: the one it named (loaded on
/// demand) or the server's active one. Unlike [`activate_project`] this
/// leaves the active selection alone — see [`ProjectRegistry::insert_loaded`].
pub(crate) async fn resolve_ctx(
    registry: &Arc<ProjectRegistry>,
    name: Option<&str>,
) -> Result<Arc<ProjectContext>, String> {
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return Ok(registry.active_ctx());
    };
    let name = crate::project::sanitize_name(name);
    if let Some(ctx) = registry.get_loaded(&name) {
        refresh_snapshot_if_stale(&ctx).await;
        return Ok(ctx);
    }
    let (dir, _meta) = crate::project::list_projects()
        .into_iter()
        .find(|(_, m)| m.name == name)
        .ok_or_else(|| format!("unknown project '{}'", name))?;
    let ctx = build_project_context(
        &name,
        dir.join("graph.json"),
        dir.join("ugdb"),
        None,
        registry.no_db,
    )
    .await?;
    registry.insert_loaded(ctx.clone());
    Ok(ctx)
}
