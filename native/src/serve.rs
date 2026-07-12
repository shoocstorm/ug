//! HTTP server for the visualization UI plus a read-only graph API.
//! See `docs/SERVE.md` for the full design (Phases 1, 1.5, 2, 3).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use tokio::sync::OnceCell;

use axum::body::{Body, Bytes};
use axum::extract::{Json, Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Semaphore;
use tower_http::compression::CompressionLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::chat::{self, ChatClient, ChatConfig, ChatMessage, ChatRagOptions};
use crate::{
    calculate_centrality as lib_centrality, detect_cycles as lib_cycles, embedder_from_args,
    flag_value, flag_value_or, has_flag, tokio_runtime, C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW,
};
use ultragraph::storage::{
    self, open_store, search_kb as storage_search_kb,
    semantic_search as storage_semantic_search, semantic_search_w_where, traverse_filtered,
    Direction, Embedder, KnowledgeStore, RankStrategy, SearchKbOptions, StoreSpec,
    DEFAULT_EMBEDDING_DIM,
};

/// Build the `StoreSpec`s for `ug serve` from env vars. `UG_DEST` is
/// comma-separated — when more than one backend is listed, the server
/// opens all of them and the UI shows a destination selector. The
/// first item parsed becomes the primary (default for requests that
/// don't specify a dest).
fn build_serve_store_specs(db_path: &PathBuf) -> Vec<StoreSpec> {
    let dest = std::env::var("UG_DEST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "overgraph".to_string());
    let dim = DEFAULT_EMBEDDING_DIM as u32;
    let mut specs: Vec<StoreSpec> = Vec::new();
    for kind in dest.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        match kind.as_str() {
            "neo4j" | "neo" => {
                let uri = std::env::var("UG_NEO4J_URI")
                    .expect("UG_DEST=neo4j requires UG_NEO4J_URI");
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
use ultragraph::types::{GraphData, GraphEdge, GraphNode};

// ---------- Encoded asset (identity + gzip + br, all pre-built) ----------

struct EncodedAsset {
    identity: Bytes,
    gzip: Bytes,
    brotli: Bytes,
    content_type: HeaderValue,
}

impl EncodedAsset {
    fn new(raw: Vec<u8>, content_type: &'static str) -> Self {
        let identity = Bytes::from(raw);
        let gzip = compress_gzip(&identity);
        let brotli = compress_brotli(&identity);
        Self {
            identity,
            gzip,
            brotli,
            content_type: HeaderValue::from_static(content_type),
        }
    }
}

fn compress_gzip(data: &[u8]) -> Bytes {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::with_capacity(data.len() / 4), Compression::new(9));
    enc.write_all(data).expect("gzip encode");
    Bytes::from(enc.finish().expect("gzip finish"))
}

fn compress_brotli(data: &[u8]) -> Bytes {
    use brotli::enc::BrotliEncoderParams;
    let mut out = Vec::with_capacity(data.len() / 4);
    let mut params = BrotliEncoderParams::default();
    // Quality 9 is a good size/CPU tradeoff for startup-time compression
    // (11 is slightly smaller but several times slower).
    params.quality = 9;
    params.lgwin = 22;
    let mut input = data;
    brotli::BrotliCompress(&mut input, &mut out, &params).expect("brotli compress");
    Bytes::from(out)
}

// ---------- Graph snapshot (atomic-swap on watch reload) ----------

struct GraphSnapshot {
    encoded: EncodedAsset,
    parsed: GraphData,
    raw_json: String,
    adj: OnceLock<AdjIndex>,
    centrality: OnceLock<String>,
    cycles: OnceLock<String>,
}

/// Forward adjacency built once per snapshot. `id_to_idx` lets us look up a
/// node's index in `parsed.nodes` from its string id; `out[i]` is the list of
/// neighbor indices reachable via outgoing edges from node `i`.
struct AdjIndex {
    id_to_idx: HashMap<String, usize>,
    out: Vec<Vec<usize>>,
}

fn build_adj(graph: &GraphData) -> AdjIndex {
    let id_to_idx: HashMap<String, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect();
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
    for e in &graph.edges {
        if let (Some(&si), Some(&ti)) = (id_to_idx.get(&e.source), id_to_idx.get(&e.target)) {
            out[si].push(ti);
        }
    }
    AdjIndex { id_to_idx, out }
}

/// One or more backends `ug serve` is wired up to. Populated when
/// `UG_DEST` lists one or more backend names; reads route to the
/// caller-selected dest (via a `dest` field on each search/traverse
/// request) or fall back to `primary`.
struct ServeStores {
    /// All opened stores keyed by backend name (`"overgraph"`, `"neo4j"`, …).
    map: HashMap<String, Arc<dyn KnowledgeStore>>,
    /// Default destination — first one parsed from `UG_DEST`.
    primary: String,
    /// Per-destination cached node-count probes. Populated on the first
    /// `/api/capabilities` poll, then reused for the rest of the
    /// session (the server itself doesn't write, so the count is
    /// effectively static).
    node_counts: HashMap<String, OnceCell<Option<usize>>>,
    /// Per-destination open failure reasons. Lets `/api/capabilities`
    /// tell the operator which backends came up and which didn't.
    open_errors: HashMap<String, String>,
}

impl ServeStores {
    fn get(&self, name: &str) -> Option<&Arc<dyn KnowledgeStore>> {
        self.map.get(name)
    }

    /// Reserved for future routes that hard-route to the primary; the
    /// per-request `pick_store` helper covers the current handlers.
    #[allow(dead_code)]
    fn primary_store(&self) -> &Arc<dyn KnowledgeStore> {
        self.map
            .get(&self.primary)
            .expect("primary backend always present in map")
    }

    /// Ordered list of available backend names. Sorted alphabetically
    /// so the UI selector renders deterministically across reloads.
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Everything the handlers need for one project: its graph snapshot,
/// opened stores, and repo root. In multi-project mode one of these is
/// built lazily per project the first time it's selected; in
/// single-project mode (`-i`) there is exactly one.
struct ProjectContext {
    name: String,
    graph_path: PathBuf,
    repo_root: PathBuf,
    graph: RwLock<Arc<GraphSnapshot>>,
    /// `None` when `--no-db` is set or every configured store failed
    /// to open. Phase 3 routes return 503 in that case rather than
    /// panicking the server. With multi-dest, this is `Some` as long
    /// as at least one backend opened; per-dest readiness is reported
    /// in `/api/capabilities`.
    stores: Option<Arc<ServeStores>>,
    /// Reason all configured Phase 3 backends are unavailable —
    /// surfaced verbatim in 503s so the operator can tell `--no-db`
    /// apart from real connection failures. Per-dest errors live on
    /// `ServeStores::open_errors`.
    db_unavailable_reason: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ServeMode {
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
struct ProjectRegistry {
    mode: ServeMode,
    no_db: bool,
    active: RwLock<String>,
    loaded: RwLock<HashMap<String, Arc<ProjectContext>>>,
}

impl ProjectRegistry {
    fn active_ctx(&self) -> Arc<ProjectContext> {
        let name = self.active.read().expect("active poisoned").clone();
        self.loaded
            .read()
            .expect("loaded poisoned")
            .get(&name)
            .cloned()
            .expect("active project is always loaded")
    }

    fn get_loaded(&self, name: &str) -> Option<Arc<ProjectContext>> {
        self.loaded.read().expect("loaded poisoned").get(name).cloned()
    }

    fn insert_and_activate(&self, ctx: Arc<ProjectContext>) {
        let name = ctx.name.clone();
        self.loaded
            .write()
            .expect("loaded poisoned")
            .insert(name.clone(), ctx);
        *self.active.write().expect("active poisoned") = name;
    }

    fn set_active(&self, name: &str) {
        *self.active.write().expect("active poisoned") = name.to_string();
    }
}

/// Build the per-project context: snapshot off the runtime (parse +
/// recompress is CPU-heavy), stores via the same env-driven specs as
/// before. `repo_root` comes from the project's project.json when
/// present so file preview works no matter where the server was
/// started; explicit `repo_root_override` (single mode) wins.
async fn build_project_context(
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
                .and_then(|dir| crate::project::read_meta(dir))
                .map(|m| PathBuf::from(m.repo_root))
                .filter(|p| p.as_os_str().len() > 0)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if !repo_root.exists() {
        tracing::warn!(
            project = %name,
            repo_root = %repo_root.display(),
            "repo root does not exist; file preview will 404"
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
fn build_placeholder_context(registry: &Arc<ProjectRegistry>) -> Arc<ProjectContext> {
    let empty_graph = GraphData {
        nodes: Vec::new(),
        edges: Vec::new(),
        stats: None,
    };
    let raw_json =
        serde_json::to_string(&empty_graph).unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string());
    let encoded = EncodedAsset::new(raw_json.clone().into_bytes(), "application/json; charset=utf-8");
    let snapshot = Arc::new(GraphSnapshot {
        encoded,
        parsed: empty_graph,
        raw_json,
        adj: OnceLock::new(),
        centrality: OnceLock::new(),
        cycles: OnceLock::new(),
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
async fn open_serve_stores(
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

#[derive(Clone)]
struct ServeState {
    registry: Arc<ProjectRegistry>,
    html: Arc<EncodedAsset>,
    bundle: Arc<EncodedAsset>,
    favicon: Arc<EncodedAsset>,
    /// `None` when the embedder couldn't be constructed (e.g. missing endpoint).
    /// Phase 3 search routes need it; `/api/db/*` routes don't.
    embedder: Option<Arc<Embedder>>,
    /// Default chat config baked from CLI flags. The `/api/chat` route
    /// also accepts per-request overrides (chat_model, base_url, …) so
    /// the UI can flip models without restarting the server. `None`
    /// when no `--chat-model` was passed and no `UG_CHAT_*` env vars
    /// are set; routes return 503 in that case.
    chat_default: Arc<Option<ChatConfig>>,
    /// Process-wide cap on concurrent embedding calls. Cheap insurance against
    /// hammering the embedding endpoint when many search requests land at once.
    embed_lock: Arc<Semaphore>,
    /// Background `ug gen` jobs kicked off from the KB Manager wizard.
    gen_jobs: Arc<GenJobs>,
}

impl ServeState {
    fn active(&self) -> Arc<ProjectContext> {
        self.registry.active_ctx()
    }

    fn snapshot(&self) -> Arc<GraphSnapshot> {
        self.active()
            .graph
            .read()
            .expect("graph state poisoned")
            .clone()
    }

    fn stores(&self) -> Option<Arc<ServeStores>> {
        self.active().stores.clone()
    }

    fn repo_root(&self) -> PathBuf {
        self.active().repo_root.clone()
    }

    fn db_unavailable_reason(&self) -> Option<String> {
        self.active().db_unavailable_reason.clone()
    }
}

// ---------- Background `ug gen` jobs (KB Manager wizard) ----------

#[derive(Copy, Clone, PartialEq, Eq)]
enum GenJobStatus {
    Running,
    Done,
    Error,
}

/// State for one wizard-triggered generation, run as a `ug gen`
/// subprocess so the pipeline logic isn't duplicated here. Streamed
/// stdout/stderr lines accumulate in `log` for the client to poll.
struct GenJob {
    status: GenJobStatus,
    log: Vec<String>,
    project_name: Option<String>,
    error: Option<String>,
}

/// In-memory registry of generation jobs, keyed by a per-process
/// monotonic id. Local dev tool, single user — no persistence or
/// eviction needed; the process restarting clears it.
struct GenJobs {
    next_id: AtomicU64,
    jobs: RwLock<HashMap<String, Arc<RwLock<GenJob>>>>,
}

impl GenJobs {
    fn new() -> Self {
        GenJobs {
            next_id: AtomicU64::new(1),
            jobs: RwLock::new(HashMap::new()),
        }
    }
}

/// Render `bytes` as a stream's current log line: overwrite the still-open
/// entry at `open_idx` if there is one, otherwise append a new entry and
/// mark it open. The log only ever grows, so the index stays valid.
fn write_gen_log_line(job: &RwLock<GenJob>, open_idx: &mut Option<usize>, bytes: &[u8]) {
    let line = strip_ansi(&String::from_utf8_lossy(bytes));
    let mut j = job.write().expect("job poisoned");
    match *open_idx {
        Some(i) if i < j.log.len() => j.log[i] = line,
        _ => {
            j.log.push(line);
            *open_idx = Some(j.log.len() - 1);
        }
    }
}

/// Stream one of the `ug gen` child's output pipes into the job log.
///
/// Splits on `\r` as well as `\n`: the pipeline prints long-phase progress
/// via `print!("\r…")` rewrites, so with a plain line reader an entire
/// phase (e.g. embedding thousands of nodes) surfaces as one giant line
/// only after its terminating `\n` — until then the log looks finished
/// while the job is still running. A `\r` rewrite updates the stream's
/// open log entry in place, and unterminated output is flushed after
/// every read so `print!` phase headers appear immediately. The open
/// entry is tracked per stream so interleaved stdout/stderr lines don't
/// overwrite each other.
async fn pump_gen_output<R>(mut stream: R, job: Arc<RwLock<GenJob>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    let mut partial: Vec<u8> = Vec::new();
    let mut open_idx: Option<usize> = None;
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            match b {
                b'\n' | b'\r' => {
                    if !partial.is_empty() {
                        write_gen_log_line(&job, &mut open_idx, &partial);
                        partial.clear();
                    } else if b == b'\n' && open_idx.is_none() {
                        // Bare println!() — preserve the blank line.
                        job.write().expect("job poisoned").log.push(String::new());
                    }
                    if b == b'\n' {
                        open_idx = None;
                    }
                }
                _ => partial.push(b),
            }
        }
        // `partial` keeps accumulating until a separator arrives; the
        // flush just renders its current state, so a line split across
        // reads is re-rendered whole on the next pass.
        if !partial.is_empty() {
            write_gen_log_line(&job, &mut open_idx, &partial);
        }
    }
    if !partial.is_empty() {
        write_gen_log_line(&job, &mut open_idx, &partial);
    }
}

/// Strip ANSI SGR escape sequences (`\x1b[...m`) from CLI output so the
/// wizard's plain-text log viewer doesn't show raw color codes.
fn strip_ansi(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("valid regex"));
    re.replace_all(s, "").into_owned()
}

fn load_snapshot(path: &PathBuf) -> Result<Arc<GraphSnapshot>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let raw_json =
        String::from_utf8(raw).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
    let parsed: GraphData =
        serde_json::from_str(&raw_json).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    let encoded = EncodedAsset::new(
        raw_json.clone().into_bytes(),
        "application/json; charset=utf-8",
    );
    Ok(Arc::new(GraphSnapshot {
        encoded,
        parsed,
        raw_json,
        adj: OnceLock::new(),
        centrality: OnceLock::new(),
        cycles: OnceLock::new(),
    }))
}

// ---------- Tracing ----------

/// Initialize a global `tracing` subscriber. No-ops if one is already
/// installed (so chained calls from `ug gen --serve` are safe).
///
/// Default filter: `info` for our crate + tower_http, `warn` for the
/// noisy hyper/reqwest internals. Override with `RUST_LOG=...`.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,ultragraph=info,tower_http=info,hyper=warn,h2=warn,reqwest=warn,rustls=warn",
        )
    });
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .try_init();
}

// ---------- Entry point ----------

pub fn run_serve(args: &[String]) {
    init_tracing();

    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_serve_help();
        return;
    }

    // Explicit -i/--input pins the server to one graph file (the
    // pre-multi-project behavior). Without it the server roots at
    // ug_home(), discovers every generated project, and lets the UI
    // switch between them at runtime.
    let input_flag = flag_value(args, &["-i", "--input"]);

    let port: u16 = flag_value(args, &["-p", "--port"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let host = flag_value_or(args, &["--host"], "127.0.0.1");
    let watch = has_flag(args, "--watch");
    let no_db = has_flag(args, "--no-db");

    enum Startup {
        Single { graph_file: String },
        Multi { initial: String },
    }

    let startup = match input_flag {
        Some(graph_file) => Startup::Single { graph_file },
        None => {
            let projects = crate::project::list_projects();
            if projects.is_empty() {
                // Legacy repo-local layout: keep `ug serve` working in
                // repos generated before the ~/.ug move.
                if std::path::Path::new(".ug/graph.json").exists() {
                    tracing::warn!(
                        home = %crate::project::ug_home().display(),
                        "no projects found; serving legacy ./.ug/graph.json — run `ug gen` to migrate to ~/.ug"
                    );
                    Startup::Single {
                        graph_file: ".ug/graph.json".to_string(),
                    }
                } else {
                    // No projects and no legacy graph: start anyway with
                    // an empty placeholder project. The KB Manager screen
                    // (always shown first when `/api/projects` reports
                    // zero projects) presents the "generate from scratch"
                    // wizard; an empty sentinel `initial` name signals
                    // that below.
                    tracing::info!(
                        home = %crate::project::ug_home().display(),
                        "no projects found — starting in multi-project mode; use the KB Manager UI to generate one"
                    );
                    Startup::Multi {
                        initial: String::new(),
                    }
                }
            } else {
                let requested =
                    flag_value(args, &["--project"]).map(|n| crate::project::sanitize_name(&n));
                let initial = match requested {
                    Some(r) => {
                        if !projects.iter().any(|(_, m)| m.name == r) {
                            let names: Vec<&str> =
                                projects.iter().map(|(_, m)| m.name.as_str()).collect();
                            tracing::error!(
                                requested = %r,
                                available = %names.join(", "),
                                "--project not found"
                            );
                            std::process::exit(1);
                        }
                        r
                    }
                    None => {
                        let cwd_name = crate::project::derive_project_name(".");
                        projects
                            .iter()
                            .find(|(_, m)| m.name == cwd_name)
                            .map(|(_, m)| m.name.clone())
                            // list_projects is sorted most-recent first.
                            .unwrap_or_else(|| projects[0].1.name.clone())
                    }
                };
                Startup::Multi { initial }
            }
        }
    };

    let html = Arc::new(EncodedAsset::new(
        crate::VIS_HTML.as_bytes().to_vec(),
        "text/html; charset=utf-8",
    ));
    let bundle = Arc::new(EncodedAsset::new(
        crate::VIS_BUNDLE.to_vec(),
        "application/javascript; charset=utf-8",
    ));
    let favicon = Arc::new(EncodedAsset::new(
        crate::VIS_FAVICON.to_vec(),
        "image/svg+xml",
    ));

    // Build embedder up-front (sync) — Phase 3 search routes need it.
    // Failure here is non-fatal: keep the rest of the server up and let
    // /api/search/* return 503.
    let (embedder_arc, embedder_err): (Option<Arc<Embedder>>, Option<String>) = if no_db {
        (None, Some("started with --no-db".to_string()))
    } else {
        match embedder_from_args(args) {
            e => (Some(Arc::new(e)), None),
        }
    };
    // `embedder_from_args` panics on construction failure today, so we don't
    // get a graceful error path for "endpoint config bogus" yet — but the
    // shape above is what we'd plug into if it returns Result later.
    let _ = embedder_err;

    let addr: SocketAddr = match format!("{}:{}", host, port).parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(host = %host, port, error = %e, "invalid bind address");
            std::process::exit(1);
        }
    };

    let rt = tokio_runtime();
    rt.block_on(async move {
        let t0 = std::time::Instant::now();

        let (mode, registry_seed) = match &startup {
            Startup::Single { .. } => (ServeMode::Single, None),
            Startup::Multi { initial } => (ServeMode::Multi, Some(initial.clone())),
        };
        let registry = Arc::new(ProjectRegistry {
            mode,
            no_db,
            active: RwLock::new(String::new()),
            loaded: RwLock::new(HashMap::new()),
        });

        let initial_ctx = match &startup {
            Startup::Single { graph_file } => {
                let graph_path = std::fs::canonicalize(graph_file).unwrap_or_else(|e| {
                    tracing::error!(path = %graph_file, error = %e, "failed to resolve graph path");
                    std::process::exit(1);
                });
                // Default db: the graph file's sibling ugdb — keeps
                // `-i .ug/graph.json` finding `.ug/ugdb` like before.
                let db_path_raw = flag_value(args, &["-d", "--db"]).unwrap_or_else(|| {
                    graph_path
                        .parent()
                        .map(|p| p.join("ugdb"))
                        .unwrap_or_else(|| PathBuf::from("ugdb"))
                        .to_string_lossy()
                        .into_owned()
                });
                let db_path = std::fs::canonicalize(&db_path_raw).unwrap_or_else(|_| {
                    std::env::current_dir()
                        .map(|c| c.join(&db_path_raw))
                        .unwrap_or_else(|_| PathBuf::from(&db_path_raw))
                });
                let repo_root_override = flag_value(args, &["--repo-root"]).map(|raw| {
                    std::fs::canonicalize(&raw).unwrap_or_else(|e| {
                        tracing::error!(path = %raw, error = %e, "failed to resolve repo root path");
                        std::process::exit(1);
                    })
                });
                let name = graph_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("single")
                    .to_string();
                let ctx =
                    build_project_context(&name, graph_path, db_path, repo_root_override, no_db)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::error!(error = %e, "failed to load graph snapshot");
                            std::process::exit(1);
                        });
                registry.insert_and_activate(ctx.clone());
                ctx
            }
            Startup::Multi { .. } => {
                let initial = registry_seed.expect("multi startup has initial project");
                if initial.is_empty() {
                    build_placeholder_context(&registry)
                } else {
                    activate_project(&registry, &initial).await.unwrap_or_else(|e| {
                        tracing::error!(project = %initial, error = %e, "failed to load initial project");
                        std::process::exit(1);
                    })
                }
            }
        };

        let (identity_size, gzip_size, brotli_size, nodes, edges) = {
            let snap = initial_ctx.graph.read().expect("graph state poisoned");
            (
                snap.encoded.identity.len(),
                snap.encoded.gzip.len(),
                snap.encoded.brotli.len(),
                snap.parsed.nodes.len(),
                snap.parsed.edges.len(),
            )
        };

        let chat_default = build_chat_default_from_args(args);
        if let Some(cfg) = chat_default.as_ref() {
            tracing::info!(
                model = %cfg.model,
                base_url = %cfg.base_url,
                "chat endpoint configured"
            );
        } else {
            tracing::info!("chat endpoint not configured (/api/chat will return 503)");
        }

        let state = ServeState {
            registry: registry.clone(),
            html,
            bundle,
            favicon,
            embedder: embedder_arc,
            chat_default: Arc::new(chat_default),
            embed_lock: Arc::new(Semaphore::new(4)),
            gen_jobs: Arc::new(GenJobs::new()),
        };

        let app = Router::new()
            .route("/", get(handle_index))
            .route("/index.html", get(handle_index))
            .route("/ug-vis.bundle.js", get(handle_bundle))
            .route("/favicon.svg", get(handle_favicon))
            .route("/graph.json", get(handle_graph))
            .route("/healthz", get(handle_health))
            .route("/api/projects", get(api_projects))
            .route("/api/projects/select", post(api_projects_select))
            .route("/api/projects/delete", post(api_projects_delete))
            .route("/api/generate", post(api_generate))
            .route("/api/generate/status", get(api_generate_status))
            .route("/api/browse-dir", get(api_browse_dir))
            .route("/api/capabilities", get(api_capabilities))
            .route("/api/graph/stats", get(api_stats))
            .route("/api/graph/node/*id", get(api_node))
            .route("/api/graph/search", get(api_search))
            .route("/api/graph/bfs/*id", get(api_bfs))
            .route("/api/graph/path", get(api_path))
            .route("/api/graph/filter", get(api_filter))
            .route("/api/graph/centrality", get(api_centrality))
            .route("/api/graph/cycles", get(api_cycles))
            // Source file content for the right-panel "Preview" tab.
            .route("/api/file", get(api_file))
            // Phase 3 — DB / embedder backed
            .route("/api/db/node/*id", get(api_db_node))
            .route("/api/db/traverse/*id", get(api_db_traverse))
            .route("/api/search/semantic", post(api_search_semantic))
            .route("/api/search/hybrid", post(api_search_hybrid))
            .route("/api/chat", post(api_chat))
            // CompressionLayer skips responses that already have Content-Encoding,
            // so it only kicks in for the dynamic /api/* JSON.
            .layer(CompressionLayer::new().br(true))
            // One INFO span per request: method+uri on entry, status+latency on exit.
            // Matches the structured-log pattern the rest of the server uses.
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                    .on_request(DefaultOnRequest::new().level(Level::DEBUG))
                    .on_response(DefaultOnResponse::new().level(Level::INFO)),
            )
            .with_state(state.clone());

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(addr = %addr, error = %e, "bind failed");
                std::process::exit(1);
            }
        };

        let db_api_enabled = state.stores().is_some() && state.embedder.is_some();
        let db_unavailable = state.db_unavailable_reason();
        tracing::info!(
            mode = match mode { ServeMode::Single => "single", ServeMode::Multi => "multi" },
            project = %initial_ctx.name,
            graph = %initial_ctx.graph_path.display(),
            nodes,
            edges,
            identity_bytes = identity_size,
            gzip_bytes = gzip_size,
            brotli_bytes = brotli_size,
            encode_secs = t0.elapsed().as_secs_f32(),
            addr = %addr,
            db_api = db_api_enabled,
            db_unavailable_reason = db_unavailable.as_deref().unwrap_or(""),
            watch,
            "ug serve ready"
        );
        if !db_api_enabled {
            tracing::warn!(
                reason = db_unavailable.as_deref().unwrap_or("DB not opened"),
                "Phase 3 routes will 503"
            );
        }
        if watch {
            spawn_watch(state.clone());
        }

        tracing::info!("Open http://{}\n", addr);

        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "server crashed");
            std::process::exit(1);
        }
    });
}

// ---------- Watch (Phase 1.5) ----------

fn spawn_watch(state: ServeState) {
    tokio::spawn(async move {
        // Per-project last-seen mtimes: the active project can change at
        // runtime, and each context keeps its own snapshot to reload into.
        let mut last_mtimes: HashMap<String, Option<SystemTime>> = HashMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let ctx = state.registry.active_ctx();
            let path = ctx.graph_path.clone();
            let mtime = file_mtime(&path);
            match last_mtimes.get(&ctx.name) {
                // First time watching this project: its snapshot was
                // freshly loaded on activation, so just record the mtime.
                None => {
                    last_mtimes.insert(ctx.name.clone(), mtime);
                    continue;
                }
                Some(prev) if mtime.is_none() || mtime == *prev => continue,
                Some(_) => {}
            }
            last_mtimes.insert(ctx.name.clone(), mtime);
            let path_clone = path.clone();
            let ctx_clone = ctx.clone();
            // Parse + recompress can take a few hundred ms on big graphs;
            // do it off the runtime so we don't stall HTTP handlers.
            let _ = tokio::task::spawn_blocking(move || match load_snapshot(&path_clone) {
                Ok(snap) => {
                    let size = snap.encoded.identity.len();
                    let nodes = snap.parsed.nodes.len();
                    let edges = snap.parsed.edges.len();
                    if let Ok(mut w) = ctx_clone.graph.write() {
                        *w = snap;
                        tracing::info!(
                            target: "ug::serve::watch",
                            project = %ctx_clone.name,
                            path = %path_clone.display(),
                            bytes = size,
                            nodes,
                            edges,
                            "graph reloaded"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    target: "ug::serve::watch",
                    error = %e,
                    "graph reload failed"
                ),
            })
            .await;
        }
    });
}

// ---------- Project switching (multi-project mode) ----------

/// Activate a project by name: reuse the cached context if it was
/// loaded before, otherwise discover it on disk under `ug_home()` and
/// build a fresh context (snapshot + stores). Errors are strings for
/// direct surfacing in API responses.
async fn activate_project(
    registry: &Arc<ProjectRegistry>,
    name: &str,
) -> Result<Arc<ProjectContext>, String> {
    if let Some(ctx) = registry.get_loaded(name) {
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

/// GET /api/projects — mode, active project, and the project list.
/// Multi mode re-lists from disk on every call so projects generated
/// after server start show up without a restart.
async fn api_projects(State(state): State<ServeState>) -> Response {
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
struct ProjectSelectBody {
    name: String,
}

/// POST /api/projects/select — switch the server-side active project.
/// The UI reloads after this so every root-relative fetch picks up the
/// new project.
async fn api_projects_select(
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
struct ProjectDeleteBody {
    name: String,
}

/// POST /api/projects/delete — delete a project's on-disk data
/// directory (mirrors `ug rm`) and drop it from the in-memory registry.
/// If the deleted project was active, falls back to another remaining
/// project, or the zero-project placeholder if none are left, so every
/// handler always has something to read from.
async fn api_projects_delete(
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
    if let Err(e) = crate::project::remove_project_dir(&dir) {
        return err_json(
            StatusCode::NOT_FOUND,
            &format!("failed to remove '{}': {}", name, e),
        );
    }
    state
        .registry
        .loaded
        .write()
        .expect("loaded poisoned")
        .remove(&name);

    let was_active = *state.registry.active.read().expect("active poisoned") == name;
    let mut active_name = name.clone();
    if was_active {
        let remaining = crate::project::list_projects();
        if let Some((_, meta)) = remaining.first() {
            match activate_project(&state.registry, &meta.name).await {
                Ok(ctx) => active_name = ctx.name.clone(),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to activate fallback project after delete");
                    build_placeholder_context(&state.registry);
                    active_name = "__none__".to_string();
                }
            }
        } else {
            build_placeholder_context(&state.registry);
            active_name = "__none__".to_string();
        }
    }

    tracing::info!(project = %name, "project deleted");
    ok_json(
        serde_json::json!({
            "removed": name,
            "active": active_name,
        })
        .to_string(),
    )
}

#[derive(serde::Deserialize)]
struct GenerateBody {
    path: String,
    name: Option<String>,
    #[serde(default)]
    no_ingest: bool,
}

/// POST /api/generate — KB Manager wizard entry point. Spawns `ug gen`
/// as a subprocess (reusing the exact same pipeline the CLI uses,
/// rather than duplicating it here) against `body.path`, and returns a
/// job id immediately; progress is polled via `/api/generate/status`.
/// Only available in multi-project mode — there's nowhere sensible to
/// discover a newly generated project from in single mode.
async fn api_generate(State(state): State<ServeState>, Json(body): Json<GenerateBody>) -> Response {
    if state.registry.mode == ServeMode::Single {
        return err_json(
            StatusCode::BAD_REQUEST,
            "generate is only available in multi-project mode",
        );
    }
    let raw_path = body.path.trim().to_string();
    let canon = match std::fs::canonicalize(&raw_path) {
        Ok(p) if p.is_dir() => p,
        Ok(_) => return err_json(StatusCode::BAD_REQUEST, "path is not a directory"),
        Err(e) => return err_json(StatusCode::BAD_REQUEST, &format!("invalid path: {}", e)),
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
    // Quiet the ASCII-art banner `main()` prints on every invocation —
    // it would otherwise dominate the wizard's log viewer.
    cmd.env("UG_QUIET_LOGO", "1");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let fallback_name =
        name.unwrap_or_else(|| crate::project::derive_project_name(&canon.to_string_lossy()));

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
struct GenJobQuery {
    job: String,
}

/// GET /api/generate/status?job=<id> — poll a generation job's status,
/// accumulated log lines, and (on success) the resulting project name.
async fn api_generate_status(
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
struct BrowseDirQuery {
    path: Option<String>,
}

/// GET /api/browse-dir?path=<dir> — list subdirectories of `path` (or the
/// user's home directory when omitted) for the KB Manager wizard's folder
/// browser. Read-only; only ever lists directory entries. Resolves
/// symlinks/`..` via `canonicalize` so the returned `path`/`parent` are
/// always absolute, and falls back to the parent directory if `path`
/// happens to point at a file rather than a directory.
async fn api_browse_dir(Query(params): Query<BrowseDirQuery>) -> Response {
    let requested = params
        .path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));

    let dir = match std::fs::canonicalize(&requested) {
        Ok(p) if p.is_dir() => p,
        Ok(p) => match p.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return err_json(StatusCode::BAD_REQUEST, "path is not a directory"),
        },
        Err(e) => return err_json(StatusCode::BAD_REQUEST, &format!("invalid path: {}", e)),
    };

    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => {
            return err_json(StatusCode::BAD_REQUEST, &format!("cannot read directory: {}", e))
        }
    };

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

    ok_json(
        serde_json::json!({
            "path": dir.to_string_lossy(),
            "parent": dir.parent().map(|p| p.to_string_lossy().to_string()),
            "entries": entries.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        })
        .to_string(),
    )
}

fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

// ---------- Encoding negotiation ----------

#[derive(Copy, Clone, PartialEq, Eq)]
enum Encoding {
    Identity,
    Gzip,
    Brotli,
}

fn pick_encoding(headers: &HeaderMap) -> Encoding {
    let Some(accept) = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return Encoding::Identity;
    };
    let mut has_gzip = false;
    let mut has_br = false;
    for part in accept.split(',') {
        let token = part
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match token.as_str() {
            "br" => has_br = true,
            "gzip" => has_gzip = true,
            _ => {}
        }
    }
    if has_br {
        Encoding::Brotli
    } else if has_gzip {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

fn asset_response(asset: &EncodedAsset, headers: &HeaderMap) -> Response {
    let (bytes, encoding) = match pick_encoding(headers) {
        Encoding::Brotli => (asset.brotli.clone(), Some("br")),
        Encoding::Gzip => (asset.gzip.clone(), Some("gzip")),
        Encoding::Identity => (asset.identity.clone(), None),
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type.clone())
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::VARY, "accept-encoding")
        .header(header::CONTENT_LENGTH, bytes.len());
    if let Some(e) = encoding {
        builder = builder.header(header::CONTENT_ENCODING, e);
    }
    builder.body(Body::from(bytes)).expect("build response")
}

// ---------- Static handlers ----------

async fn handle_index(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.html, &headers)
}

async fn handle_graph(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    let snap = state.snapshot();
    asset_response(&snap.encoded, &headers)
}

async fn handle_bundle(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.bundle, &headers)
}

async fn handle_favicon(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.favicon, &headers)
}

async fn handle_health() -> &'static str {
    "ok"
}

// ---------- API helpers ----------

fn ok_json(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

fn err_json(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

fn parse_csv(s: Option<String>) -> Option<Vec<String>> {
    s.and_then(|raw| {
        let v: Vec<String> = raw
            .split(',')
            .filter_map(|p| {
                let t = p.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    })
}

// ---------- API handlers (Phase 2) ----------

async fn api_stats(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    let mut node_types: BTreeMap<String, usize> = BTreeMap::new();
    for n in &snap.parsed.nodes {
        *node_types.entry(format!("{:?}", n.node_type)).or_insert(0) += 1;
    }
    let mut edge_types: BTreeMap<String, usize> = BTreeMap::new();
    for e in &snap.parsed.edges {
        *edge_types.entry(format!("{:?}", e.edge_type)).or_insert(0) += 1;
    }
    let body = serde_json::json!({
        "nodes": snap.parsed.nodes.len(),
        "edges": snap.parsed.edges.len(),
        "node_types": node_types,
        "edge_types": edge_types,
        "graph_bytes": snap.encoded.identity.len(),
    });
    ok_json(body.to_string())
}

async fn api_node(State(state): State<ServeState>, AxPath(id): AxPath<String>) -> Response {
    let snap = state.snapshot();
    match snap.parsed.nodes.iter().find(|n| n.id == id) {
        Some(n) => match serde_json::to_string(n) {
            Ok(s) => ok_json(s),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
        },
        None => err_json(StatusCode::NOT_FOUND, "node not found"),
    }
}

#[derive(serde::Deserialize)]
struct SearchParams {
    q: Option<String>,
    types: Option<String>,
}

async fn api_search(
    State(state): State<ServeState>,
    Query(params): Query<SearchParams>,
) -> Response {
    let snap = state.snapshot();
    let needle = params.q.unwrap_or_default().to_lowercase();
    let type_filter: Option<Vec<String>> =
        parse_csv(params.types).map(|v| v.into_iter().map(|t| t.to_lowercase()).collect());

    let matched: Vec<&GraphNode> = snap
        .parsed
        .nodes
        .iter()
        .filter(|n| {
            if let Some(types) = &type_filter {
                let nt = format!("{:?}", n.node_type).to_lowercase();
                if !types.contains(&nt) {
                    return false;
                }
            }
            if needle.is_empty() {
                return true;
            }
            let name_match = n.name.to_lowercase().contains(&needle);
            let doc_match = n
                .docstring
                .as_ref()
                .map(|d| d.to_lowercase().contains(&needle))
                .unwrap_or(false);
            name_match || doc_match
        })
        .collect();

    let body = serde_json::json!({
        "count": matched.len(),
        "nodes": matched,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
struct BfsParams {
    #[serde(default = "default_k")]
    k: u32,
}
fn default_k() -> u32 {
    1
}

async fn api_bfs(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
    Query(params): Query<BfsParams>,
) -> Response {
    // Cap to keep an open server from being a runaway-expansion foot-gun.
    let k = params.k.min(8);
    let snap = state.snapshot();
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));

    let Some(&start) = adj.id_to_idx.get(&id) else {
        return ok_json(
            serde_json::json!({ "nodes": [], "edges": [], "distances": {} }).to_string(),
        );
    };

    let mut visited: HashSet<usize> = HashSet::new();
    let mut distances: HashMap<usize, u32> = HashMap::new();
    let mut queue: VecDeque<(usize, u32)> = VecDeque::new();
    queue.push_back((start, 0));
    visited.insert(start);
    distances.insert(start, 0);

    while let Some((idx, d)) = queue.pop_front() {
        if d == k {
            continue;
        }
        for &nb in &adj.out[idx] {
            if visited.insert(nb) {
                distances.insert(nb, d + 1);
                queue.push_back((nb, d + 1));
            }
        }
    }

    let nodes: Vec<&GraphNode> = visited.iter().map(|&i| &snap.parsed.nodes[i]).collect();
    let edges: Vec<&GraphEdge> = snap
        .parsed
        .edges
        .iter()
        .filter(
            |e| match (adj.id_to_idx.get(&e.source), adj.id_to_idx.get(&e.target)) {
                (Some(&si), Some(&ti)) => visited.contains(&si) && visited.contains(&ti),
                _ => false,
            },
        )
        .collect();
    let dist_by_id: HashMap<&str, u32> = distances
        .iter()
        .map(|(&i, &d)| (snap.parsed.nodes[i].id.as_str(), d))
        .collect();

    let body = serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "distances": dist_by_id,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
struct PathQuery {
    source: String,
    target: String,
}

async fn api_path(State(state): State<ServeState>, Query(params): Query<PathQuery>) -> Response {
    let snap = state.snapshot();
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));

    let not_found = || ok_json(serde_json::json!({ "path": [], "found": false }).to_string());
    let (Some(&src), Some(&tgt)) = (
        adj.id_to_idx.get(&params.source),
        adj.id_to_idx.get(&params.target),
    ) else {
        return not_found();
    };

    // BFS with predecessor tracking — directed, forward edges only (matches CLI).
    let n = snap.parsed.nodes.len();
    let mut prev: Vec<Option<usize>> = vec![None; n];
    let mut visited: Vec<bool> = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    visited[src] = true;
    queue.push_back(src);

    let mut found = false;
    while let Some(cur) = queue.pop_front() {
        if cur == tgt {
            found = true;
            break;
        }
        for &nb in &adj.out[cur] {
            if !visited[nb] {
                visited[nb] = true;
                prev[nb] = Some(cur);
                queue.push_back(nb);
            }
        }
    }

    if !found {
        return not_found();
    }

    let mut path_idx: Vec<usize> = Vec::new();
    let mut cur = tgt;
    loop {
        path_idx.push(cur);
        if cur == src {
            break;
        }
        match prev[cur] {
            Some(p) => cur = p,
            None => return not_found(),
        }
    }
    path_idx.reverse();
    let path: Vec<&str> = path_idx
        .iter()
        .map(|&i| snap.parsed.nodes[i].id.as_str())
        .collect();
    let length = (path.len() as u32).saturating_sub(1);

    let body = serde_json::json!({
        "path": path,
        "found": true,
        "length": length,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
struct FilterParams {
    types: Option<String>,
}

async fn api_filter(
    State(state): State<ServeState>,
    Query(params): Query<FilterParams>,
) -> Response {
    let Some(types) = parse_csv(params.types) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "?types= is required (comma-separated)",
        );
    };
    let lowered: Vec<String> = types.into_iter().map(|t| t.to_lowercase()).collect();
    let snap = state.snapshot();

    let matched: Vec<&GraphEdge> = snap
        .parsed
        .edges
        .iter()
        .filter(|e| {
            let et = format!("{:?}", e.edge_type).to_lowercase();
            lowered.iter().any(|t| t == &et)
        })
        .collect();

    let body = serde_json::json!({
        "count": matched.len(),
        "edges": matched,
    });
    ok_json(body.to_string())
}

async fn api_centrality(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    let cached = snap
        .centrality
        .get_or_init(|| lib_centrality(snap.raw_json.clone()))
        .clone();
    ok_json(cached)
}

async fn api_cycles(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    let cached = snap
        .cycles
        .get_or_init(|| lib_cycles(snap.raw_json.clone()))
        .clone();
    ok_json(cached)
}

// ---------- Capabilities ----------

/// Surfaces enough state for the visualization UI to gate DB-dependent
/// panels (semantic / hybrid search) without having to make a probe
/// request per panel. `search_ready` is the single boolean the UI keys
/// off — it requires DB open, embedder configured, **and** at least one
/// node row in the table (an opened-but-empty DB still 200s on the
/// existing routes but returns nothing useful).
async fn api_capabilities(State(state): State<ServeState>) -> Response {
    let active_stores = state.stores();
    let db_ready = active_stores.is_some();
    let embedder_ready = state.embedder.is_some();

    // Per-destination probe + serialization. `db_node_count` and
    // `search_ready` at the top level reflect the primary backend so
    // existing clients keep working; the new `destinations` array is
    // what the UI keys off for the selector.
    let mut destinations_json: Vec<serde_json::Value> = Vec::new();
    let mut primary_count: Option<usize> = None;
    if let Some(stores) = active_stores.clone() {
        for name in stores.names() {
            let store = stores.get(&name).cloned();
            let cell = stores.node_counts.get(&name);
            let count: Option<usize> = if let (Some(store), Some(cell)) = (store.as_ref(), cell) {
                let store_inner = store.clone();
                let name_for_log = name.clone();
                cell.get_or_init(|| async move {
                    match store_inner.count_nodes().await {
                        Ok(n) => Some(n),
                        Err(e) => {
                            tracing::warn!(backend = %name_for_log, error = %e, "count_nodes failed");
                            None
                        }
                    }
                })
                .await
                .clone()
            } else {
                None
            };
            let supports_ppr = store.map(|s| s.supports_native_ppr()).unwrap_or(false);
            let is_primary = name == stores.primary;
            if is_primary {
                primary_count = count;
            }
            destinations_json.push(serde_json::json!({
                "name": name,
                "primary": is_primary,
                "node_count": count,
                "supports_native_ppr": supports_ppr,
            }));
        }
        // Also surface backends that failed to open so the operator
        // can see what's wrong from the UI/curl alone.
        for (name, err) in stores.open_errors.iter() {
            destinations_json.push(serde_json::json!({
                "name": name,
                "primary": false,
                "node_count": null,
                "supports_native_ppr": false,
                "error": err,
            }));
        }
    }

    let has_data = primary_count.map(|n| n > 0).unwrap_or(false);
    let search_ready = db_ready && embedder_ready && has_data;
    let reason = if search_ready {
        None
    } else if !db_ready || !embedder_ready {
        state.db_unavailable_reason()
    } else if !has_data {
        Some("DB is open but contains no nodes (run `ug index` first)".to_string())
    } else {
        None
    };

    let primary_name = active_stores
        .as_ref()
        .map(|s| s.primary.clone())
        .unwrap_or_default();

    let chat_default = state.chat_default.as_ref().as_ref();
    let chat_ready = chat_default.is_some() && search_ready;
    let chat_info = chat_default.map(|c| {
        serde_json::json!({
            "model": c.model,
            "base_url": c.base_url,
        })
    });

    let body = serde_json::json!({
        "db_ready": db_ready,
        "embedder_ready": embedder_ready,
        "search_ready": search_ready,
        "chat_ready": chat_ready,
        "chat": chat_info,
        // Back-compat: existing UI reads `db_node_count` for the primary.
        "db_node_count": primary_count,
        "reason": reason,
        // New in multi-dest: full list with per-backend flags. UI shows
        // a selector when `destinations.length > 1`.
        "destinations": destinations_json,
        "primary": primary_name,
        // Multi-project: which project this server is currently
        // answering for, and whether the UI should offer a switcher.
        "project": {
            "name": state.active().name,
            "mode": match state.registry.mode {
                ServeMode::Single => "single",
                ServeMode::Multi => "multi",
            },
        },
    });
    ok_json(body.to_string())
}

// ---------- Phase 3 — DB-backed handlers ----------

/// Resolve a per-request `dest` parameter to a concrete store. `None`
/// uses the primary. Returns a 503 if no backend is available, 404 if
/// the caller asked for a name we didn't open.
fn pick_store(
    state: &ServeState,
    dest: Option<&str>,
) -> Result<Arc<dyn KnowledgeStore>, Response> {
    let stores = state.stores().ok_or_else(|| {
        let reason = state.db_unavailable_reason();
        let msg = reason.as_deref().unwrap_or("DB not opened");
        err_json(StatusCode::SERVICE_UNAVAILABLE, msg)
    })?;
    let name = dest
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| stores.primary.clone());
    stores.get(&name).cloned().ok_or_else(|| {
        let available = stores.names().join(", ");
        err_json(
            StatusCode::NOT_FOUND,
            &format!(
                "unknown destination '{}' (available: {})",
                name, available
            ),
        )
    })
}

fn embedder_or_503(state: &ServeState) -> Result<Arc<Embedder>, Response> {
    state.embedder.clone().ok_or_else(|| {
        err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "embedder not configured (started with --no-db?)",
        )
    })
}

#[derive(serde::Deserialize)]
struct FileQuery {
    /// Repo-relative path of the source file to read.
    path: String,
    /// Optional 1-based inclusive line range. Omit both for the full file
    /// (File nodes); pass them to return just a chunk's span.
    start: Option<usize>,
    end: Option<usize>,
}

/// Reads a source file (or a line slice of one) from the indexed repo so the
/// UI's Preview tab can show real content. The synthetic `node_text` is an
/// embedding string, not the source — this is the actual file on disk.
async fn api_file(State(state): State<ServeState>, Query(params): Query<FileQuery>) -> Response {
    let rel = params.path.trim();
    if rel.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "missing path");
    }

    // Resolve against the repo root and canonicalize, then verify the result is
    // still inside the root — blocks `../` traversal and absolute-path escapes.
    let repo_root = state.repo_root();
    let root = repo_root.as_path();
    let canon = match std::fs::canonicalize(root.join(rel)) {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::NOT_FOUND, "file not found"),
    };
    if !canon.starts_with(root) {
        return err_json(StatusCode::FORBIDDEN, "path escapes repo root");
    }

    const MAX_BYTES: u64 = 2 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(&canon) {
        if meta.len() > MAX_BYTES {
            return err_json(StatusCode::PAYLOAD_TOO_LARGE, "file too large to preview");
        }
    }

    let text = match tokio::fs::read_to_string(&canon).await {
        Ok(t) => t,
        Err(_) => {
            return err_json(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "file is not UTF-8 text",
            )
        }
    };

    let all: Vec<&str> = text.lines().collect();
    let total_lines = all.len();

    // Optional 1-based inclusive slice; clamp to the file's bounds.
    let (content, sliced) = match params.start {
        Some(s) if s >= 1 => {
            let lo = s - 1;
            let hi = params.end.unwrap_or(s).max(s).min(total_lines);
            let body = if lo >= total_lines {
                String::new()
            } else {
                all[lo..hi].join("\n")
            };
            (body, true)
        }
        _ => (text, false),
    };

    let body = serde_json::json!({
        "path": rel,
        "content": content,
        "start_line": params.start,
        "end_line": params.end,
        "total_lines": total_lines,
        "sliced": sliced,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
struct DbNodeQuery {
    /// Optional destination name; defaults to the primary backend.
    /// Mirrors the `dest` field used by all the other DB-backed routes.
    dest: Option<String>,
}

async fn api_db_node(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
    Query(params): Query<DbNodeQuery>,
) -> Response {
    let db = match pick_store(&state, params.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // `KnowledgeStore::fetch_node` is the single-row hydrate; works
    // identically across OverGraph and Neo4j backends.
    match db.fetch_node(&id).await {
        Ok(Some(n)) => ok_json(node_row_to_json(&n).to_string()),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "node not found"),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("fetch_node: {}", e),
        ),
    }
}

#[derive(serde::Deserialize)]
struct DbTraverseQuery {
    #[serde(default = "default_db_k")]
    k: u32,
    dir: Option<String>,
    types: Option<String>,
    /// Optional destination name; defaults to the primary backend.
    dest: Option<String>,
}
fn default_db_k() -> u32 {
    2
}

async fn api_db_traverse(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
    Query(params): Query<DbTraverseQuery>,
) -> Response {
    let db = match pick_store(&state, params.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let hops = params.k.min(8);
    let direction = params
        .dir
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Outbound);
    let edge_types = parse_csv(params.types);

    let result = match traverse_filtered(
        &*db,
        std::slice::from_ref(&id),
        hops,
        edge_types.as_deref(),
        direction,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("traverse: {}", e),
            )
        }
    };

    let nodes_json: Vec<serde_json::Value> = result
        .nodes
        .iter()
        .map(|n| {
            let mut v = node_row_to_json(n);
            if let Some(d) = result.distances.get(&n.id) {
                v["distance"] = serde_json::Value::from(*d);
            }
            v
        })
        .collect();
    let edges_json: Vec<serde_json::Value> = result
        .edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "source": e.source,
                "target": e.target,
                "edge_type": e.edge_type,
            })
        })
        .collect();

    ok_json(
        serde_json::json!({
            "dest": db.backend_name(),
            "nodes": nodes_json,
            "edges": edges_json,
            "distances": result.distances,
        })
        .to_string(),
    )
}

#[derive(serde::Deserialize)]
struct SemanticBody {
    query: String,
    #[serde(default = "default_semantic_k")]
    k: usize,
    #[serde(default)]
    filter: Option<String>,
    /// Optional destination name; defaults to the primary backend.
    #[serde(default)]
    dest: Option<String>,
}
fn default_semantic_k() -> usize {
    10
}

async fn api_search_semantic(
    State(state): State<ServeState>,
    Json(body): Json<SemanticBody>,
) -> Response {
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let k = body.k.min(100).max(1);

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let hits = match body.filter.as_deref() {
        Some(f) => semantic_search_w_where(&*db, &embedder, &body.query, k, f).await,
        None => storage_semantic_search(&*db, &embedder, &body.query, k).await,
    };
    drop(_permit);

    let hits = match hits {
        Ok(h) => h,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("semantic_search: {}", e),
            )
        }
    };

    let body = serde_json::json!({
        "count": hits.len(),
        "dest": db.backend_name(),
        "hits": hits.iter().map(|h| {
            serde_json::json!({
                "id": h.node.id,
                "name": h.node.name,
                "node_type": h.node.node_type,
                "file": h.node.file,
                "start_line": h.node.start_line,
                "end_line": h.node.end_line,
                "description": h.node.description,
                "distance": h.distance,
            })
        }).collect::<Vec<_>>(),
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
struct HybridBody {
    query: String,
    #[serde(default = "default_hybrid_k")]
    k: usize,
    #[serde(default = "default_hybrid_hops")]
    hops: u32,
    #[serde(default)]
    edge_types: Option<Vec<String>>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default = "default_hybrid_max_chars")]
    max_chars: usize,
    #[serde(default = "default_hybrid_mmr_lambda")]
    mmr_lambda: f32,
    #[serde(default, rename = "where")]
    where_clause: Option<String>,
    #[serde(default = "default_hybrid_include_snippets")]
    include_snippets: bool,
    /// Optional destination name; defaults to the primary backend.
    #[serde(default)]
    dest: Option<String>,
}
fn default_hybrid_k() -> usize {
    8
}
fn default_hybrid_hops() -> u32 {
    2
}
fn default_hybrid_max_chars() -> usize {
    12_000
}
fn default_hybrid_mmr_lambda() -> f32 {
    0.6
}
fn default_hybrid_include_snippets() -> bool {
    true
}

async fn api_search_hybrid(
    State(state): State<ServeState>,
    Json(body): Json<HybridBody>,
) -> Response {
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let k = body.k.min(50).max(1);
    let hops = body.hops.min(4);
    let strategy = body
        .strategy
        .as_deref()
        .map(RankStrategy::from_str_lossy)
        .unwrap_or(RankStrategy::Ppr);
    let direction = body
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    let max_chars = body.max_chars.min(64_000);
    let mmr_lambda = body.mmr_lambda.clamp(0.0, 1.0);

    let edge_types_owned: Option<Vec<String>> = body.edge_types.filter(|v| !v.is_empty());

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let repo_root = state.repo_root();
    let mut opts = SearchKbOptions::new(&body.query, repo_root.as_path());
    opts.k = k;
    opts.hops = hops;
    opts.edge_types = edge_types_owned.as_deref();
    opts.direction = direction;
    opts.max_chars = max_chars;
    opts.mmr_lambda = mmr_lambda;
    opts.where_clause = body.where_clause.as_deref();
    opts.include_snippets = body.include_snippets;
    opts.strategy = strategy;

    let dest_name = db.backend_name();
    let result = storage_search_kb(&*db, &embedder, opts).await;
    drop(_permit);

    match result {
        Ok(ctx) => match serde_json::to_value(&ctx) {
            Ok(mut v) => {
                // Surface the actual backend the result came from so
                // the UI can display "results from <dest>" even when
                // the caller didn't pass an explicit `dest`.
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "dest".to_string(),
                        serde_json::Value::String(dest_name.to_string()),
                    );
                }
                ok_json(v.to_string())
            }
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
        },
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("search_kb: {}", e),
        ),
    }
}

// ---------- Phase 4 — Chat (/api/chat) ----------

/// Pull a default `ChatConfig` from CLI args or env vars. Returns
/// `None` when no chat model is configured — the route then 503s with
/// a clear message rather than hitting a misconfigured endpoint.
///
/// Env-var fallbacks let `ug serve` be wrapped by `docker run -e
/// UG_CHAT_*` without rewriting the entrypoint.
fn build_chat_default_from_args(args: &[String]) -> Option<ChatConfig> {
    let model = flag_value(args, &["--chat-model"]).or_else(|| std::env::var("UG_CHAT_MODEL").ok());
    let base_url = flag_value(args, &["--chat-base-url"])
        .or_else(|| std::env::var("UG_CHAT_BASE_URL").ok())
        .or_else(|| flag_value(args, &["--base-url"]))
        .or_else(|| std::env::var("UG_EMBED_BASE_URL").ok());
    let api_key = flag_value(args, &["--chat-api-key"])
        .or_else(|| std::env::var("UG_CHAT_API_KEY").ok())
        .or_else(|| flag_value(args, &["--api-key"]))
        .or_else(|| std::env::var("UG_EMBED_API_KEY").ok());
    let temperature = flag_value(args, &["--temperature"]).and_then(|s| s.parse().ok());
    let max_tokens = flag_value(args, &["--max-tokens"]).and_then(|s| s.parse().ok());
    let timeout = flag_value(args, &["--chat-timeout"]).and_then(|s| s.parse().ok());

    // Require at least a chat model — without it we can't reasonably
    // pick one and the endpoint would 4xx every request.
    let model = model?;
    let cfg = ChatConfig::with_overrides(
        base_url,
        api_key,
        Some(model),
        temperature,
        max_tokens,
        timeout,
    );
    Some(cfg)
}

#[derive(serde::Deserialize)]
struct ChatBody {
    query: String,
    #[serde(default)]
    history: Option<Vec<ChatMessage>>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    hops: Option<u32>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    edge_types: Option<Vec<String>>,
    #[serde(default)]
    include_snippets: Option<bool>,
    #[serde(default)]
    max_context_chars: Option<usize>,
    #[serde(default, rename = "where")]
    where_clause: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    // Per-request chat overrides (UI surfaces these). All optional —
    // anything missing falls back to the default `ChatConfig`.
    #[serde(default)]
    chat_model: Option<String>,
    #[serde(default)]
    chat_base_url: Option<String>,
    #[serde(default)]
    chat_api_key: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Optional destination name; defaults to the primary backend.
    #[serde(default)]
    dest: Option<String>,
}

async fn api_chat(State(state): State<ServeState>, Json(body): Json<ChatBody>) -> Response {
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };

    // Merge defaults with per-request overrides. Without a default and
    // without an override we can't pick a model, so the route 503s.
    let chat_cfg = match merge_chat_cfg(state.chat_default.as_ref(), &body) {
        Some(c) => c,
        None => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "chat not configured (start serve with --chat-model or pass `chat_model` in the request body)",
            )
        }
    };

    let chat_client = match ChatClient::new(chat_cfg) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("build chat client: {}", e),
            )
        }
    };

    let k = body.k.unwrap_or(8).min(50).max(1);
    let hops = body.hops.unwrap_or(2).min(4);
    let strategy = body
        .strategy
        .as_deref()
        .map(RankStrategy::from_str_lossy)
        .unwrap_or(RankStrategy::Ppr);
    let direction = body
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    let include_snippets = body.include_snippets.unwrap_or(true);
    let max_context_chars = body.max_context_chars.unwrap_or(chat::DEFAULT_CTX_MAX_CHARS).min(64_000);
    let edge_types_owned: Option<Vec<String>> = body.edge_types.filter(|v| !v.is_empty());
    let history_owned: Vec<ChatMessage> = body.history.unwrap_or_default();

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let mut opts = ChatRagOptions::new();
    opts.k = k;
    opts.hops = hops;
    opts.strategy = strategy;
    opts.direction = direction;
    opts.edge_types = edge_types_owned.as_deref();
    opts.include_snippets = include_snippets;
    opts.max_context_chars = max_context_chars;
    opts.where_clause = body.where_clause.as_deref();
    opts.system_prompt = body.system_prompt.as_deref();

    let dest_name = db.backend_name();
    let repo_root = state.repo_root();
    let outcome = chat::run_chat_rag(
        &*db,
        &embedder,
        &chat_client,
        repo_root.as_path(),
        &body.query,
        &history_owned,
        opts,
    )
    .await;
    drop(_permit);

    match outcome {
        Ok(o) => {
            let citations: Vec<serde_json::Value> = o
                .context
                .items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    serde_json::json!({
                        "index": i + 1,
                        "id": it.id,
                        "name": it.name,
                        "node_type": it.node_type,
                        "file": it.file,
                        "start_line": it.start_line,
                        "end_line": it.end_line,
                        "description": it.description,
                        "distance": it.distance,
                        "hop": it.hop,
                        "snippet": it.snippet,
                    })
                })
                .collect();
            let body_json = serde_json::json!({
                "query": body.query,
                "answer": o.answer,
                "citations": citations,
                "seed_id": o.context.seed_id,
                "retrieval_ms": o.retrieval_ms,
                "completion_ms": o.completion_ms,
                "usage": o.usage,
                "dest": dest_name,
                "chat_model": chat_client.config().model.clone(),
            });
            ok_json(body_json.to_string())
        }
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("chat: {}", e),
        ),
    }
}

/// Combine a default `ChatConfig` (from CLI/env at startup) with
/// per-request overrides. Returns `None` only when neither side
/// provides a model — without one we can't sensibly send the request.
fn merge_chat_cfg(default: &Option<ChatConfig>, body: &ChatBody) -> Option<ChatConfig> {
    let base_default = default.clone().unwrap_or_default();
    let model = body
        .chat_model
        .clone()
        .or_else(|| default.as_ref().map(|c| c.model.clone()))?;
    let base_url = body
        .chat_base_url
        .clone()
        .unwrap_or(base_default.base_url);
    let api_key = body.chat_api_key.clone().unwrap_or(base_default.api_key);
    let temperature = body.temperature.unwrap_or(base_default.temperature);
    let max_tokens = body.max_tokens.unwrap_or(base_default.max_tokens);
    Some(ChatConfig {
        base_url,
        api_key,
        model,
        temperature,
        max_tokens,
        timeout_secs: base_default.timeout_secs,
    })
}

fn node_row_to_json(n: &storage::NodeRow) -> serde_json::Value {
    serde_json::json!({
        "id": n.id,
        "name": n.name,
        "node_type": n.node_type,
        "file": n.file,
        "start_line": n.start_line,
        "end_line": n.end_line,
        "description": n.description,
        // Full chunk text — powers the right-panel "Preview" tab.
        "node_text": n.node_text,
    })
}

pub fn print_serve_help() {
    println!("  {C_CYAN}ug serve{C_RESET}  {C_YELLOW}— serve visualization + graph API{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug serve [options]");
    println!();
    println!("  Without {C_CYAN}-i{C_RESET}, serves {C_BOLD}every{C_RESET} project under ~/.ug (or $UG_HOME) in");
    println!("  multi-project mode — the UI gets a project switcher, and");
    println!("  {C_CYAN}POST /api/projects/select{C_RESET} swaps the active project at runtime.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>   Graph JSON to serve (forces single-project mode)");
    println!("  {C_CYAN}--project{C_RESET} <name>     Initially active project in multi-project mode");
    println!("                       (default: cwd basename, else most recently generated)");
    println!("  {C_CYAN}-d, --db{C_RESET} <path>      OverGraph DB for /api/db + /api/search routes");
    println!("                       (default: per-project ugdb, or the graph file's sibling ugdb with -i)");
    println!("  {C_YELLOW}--no-db{C_RESET}            Don't open DB; routes return 503");
    println!("  {C_CYAN}-p, --port{C_RESET} <n>       TCP port (default: 8080)");
    println!("  {C_CYAN}--host{C_RESET} <addr>        Bind address (default: 127.0.0.1)");
    println!("  {C_GREEN}--watch{C_RESET}             Reload graph file when its mtime changes");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>   Repo root for hybrid-search snippet resolution");
    println!("  {C_CYAN}--base-url{C_RESET} <url>      Embedding/chat base URL (OpenAI-compatible)");
    println!("  {C_CYAN}--api-key{C_RESET} <key>       Embedding/chat API key");
    println!("  {C_CYAN}--model{C_RESET} <name>        Embedding model (fastembed alias for local)");
    println!();
    println!("{C_BOLD}Chat (POST /api/chat):{C_RESET}");
    println!("  {C_CYAN}--chat-model{C_RESET} <name>     Chat completion model — required to enable /api/chat");
    println!("  {C_CYAN}--chat-base-url{C_RESET} <url>   Override base URL for chat (defaults to --base-url)");
    println!("  {C_CYAN}--chat-api-key{C_RESET} <key>    Override API key for chat (defaults to --api-key)");
    println!("  {C_CYAN}--temperature{C_RESET} <f>       Default sampling temperature (default: 0.2)");
    println!("  {C_CYAN}--max-tokens{C_RESET} <n>        Default max completion tokens (default: 1024)");
    println!("  {C_CYAN}--chat-timeout{C_RESET} <secs>   HTTP timeout for chat calls (default: 180)");
    println!("    Env fallbacks: UG_CHAT_MODEL, UG_CHAT_BASE_URL, UG_CHAT_API_KEY");
    println!();
    println!("{C_BOLD}API Endpoints:{C_RESET}");
    println!("  {C_CYAN}GET{C_RESET}  /api/projects              list projects + active selection");
    println!("  {C_CYAN}POST{C_RESET} /api/projects/select       body: {{ name }} — switch active project");
    println!("  {C_CYAN}POST{C_RESET} /api/projects/delete       body: {{ name }} — delete a project's data directory");
    println!("  {C_CYAN}GET{C_RESET}  /api/graph/{{stats, node/<id>, search?q=&types=, bfs/<id>?k=,");
    println!("           path?source=&target=, filter?types=, centrality, cycles}}");
    println!("  {C_CYAN}GET{C_RESET}  /api/db/{{node/<id>, traverse/<id>?k=&dir=&types=}}");
    println!("  {C_CYAN}POST{C_RESET} /api/search/{{semantic, hybrid}}  body: JSON");
    println!("  {C_CYAN}POST{C_RESET} /api/chat  body: {{ query, history?, k?, hops?, chat_model?, ... }}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET}                          {C_YELLOW}# all projects under ~/.ug{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} --project myrepo --watch");
    println!("  {C_CYAN}ug serve{C_RESET} -i path/to/graph.json -p 8080   {C_YELLOW}# single-project mode{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} \\");
    println!("           --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("           --chat-model Qwen3.6-35B-A3B-MLX-8bit");
}
