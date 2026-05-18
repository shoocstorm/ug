//! HTTP server for the visualization UI plus a read-only graph API.
//! See `docs/SERVE.md` for the full design (Phases 1, 1.5, 2, 3).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
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

#[derive(Clone)]
struct ServeState {
    graph_path: Arc<PathBuf>,
    graph: Arc<RwLock<Arc<GraphSnapshot>>>,
    html: Arc<EncodedAsset>,
    d3: Arc<EncodedAsset>,
    /// `None` when `--no-db` is set or every configured store failed
    /// to open. Phase 3 routes return 503 in that case rather than
    /// panicking the server. With multi-dest, this is `Some` as long
    /// as at least one backend opened; per-dest readiness is reported
    /// in `/api/capabilities`.
    stores: Option<Arc<ServeStores>>,
    /// `None` when the embedder couldn't be constructed (e.g. missing endpoint).
    /// Phase 3 search routes need it; `/api/db/*` routes don't.
    embedder: Option<Arc<Embedder>>,
    repo_root: Arc<PathBuf>,
    /// Process-wide cap on concurrent embedding calls. Cheap insurance against
    /// hammering the embedding endpoint when many search requests land at once.
    embed_lock: Arc<Semaphore>,
    /// Reason all configured Phase 3 backends are unavailable —
    /// surfaced verbatim in 503s so the operator can tell `--no-db`
    /// apart from real connection failures. Per-dest errors live on
    /// `ServeStores::open_errors`.
    db_unavailable_reason: Arc<Option<String>>,
}

impl ServeState {
    fn snapshot(&self) -> Arc<GraphSnapshot> {
        self.graph.read().expect("graph state poisoned").clone()
    }
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

    let graph_file = flag_value_or(args, &["-i", "--input"], "ugout/graph.json");
    let graph_path = std::fs::canonicalize(&graph_file).unwrap_or_else(|e| {
        tracing::error!(path = %graph_file, error = %e, "failed to resolve graph path");
        std::process::exit(1);
    });

    let port: u16 = flag_value(args, &["-p", "--port"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let host = flag_value_or(args, &["--host"], "127.0.0.1");
    let watch = has_flag(args, "--watch");
    let no_db = has_flag(args, "--no-db");

    let db_path_raw = flag_value_or(args, &["-d", "--db"], "ugout/ugdb");
    let db_path = std::fs::canonicalize(&db_path_raw).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|c| c.join(&db_path_raw))
            .unwrap_or_else(|_| PathBuf::from(&db_path_raw))
    });

    let repo_root_raw = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let repo_root_path = std::fs::canonicalize(&repo_root_raw).unwrap_or_else(|e| {
        tracing::error!(path = %repo_root_raw.display(), error = %e, "failed to resolve repo root path");
        std::process::exit(1);
    });

    tracing::info!(
        graph = %graph_path.display(),
        db = %db_path.display(),
        repo_root = %repo_root_path.display(),
        "resolved paths"
    );

    let t0 = std::time::Instant::now();
    let snapshot = match load_snapshot(&graph_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to load graph snapshot");
            std::process::exit(1);
        }
    };
    let identity_size = snapshot.encoded.identity.len();
    let gzip_size = snapshot.encoded.gzip.len();
    let brotli_size = snapshot.encoded.brotli.len();
    let nodes = snapshot.parsed.nodes.len();
    let edges = snapshot.parsed.edges.len();

    let html = Arc::new(EncodedAsset::new(
        crate::VIS_HTML.as_bytes().to_vec(),
        "text/html; charset=utf-8",
    ));
    let d3 = Arc::new(EncodedAsset::new(
        crate::VIS_D3.to_vec(),
        "application/javascript; charset=utf-8",
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
        // Open every store listed in `UG_DEST`. Per-dest open failures
        // are non-fatal as long as at least one backend opens; the
        // operator sees per-dest status on `/api/capabilities` and the
        // UI gates its selector on it.
        let (stores_arc, db_unavailable_reason): (
            Option<Arc<ServeStores>>,
            Option<String>,
        ) = if no_db {
            (None, Some("started with --no-db".to_string()))
        } else {
            let specs = build_serve_store_specs(&db_path);
            let mut map: HashMap<String, Arc<dyn KnowledgeStore>> = HashMap::new();
            let mut node_counts: HashMap<String, OnceCell<Option<usize>>> = HashMap::new();
            let mut open_errors: HashMap<String, String> = HashMap::new();
            let mut primary: Option<String> = None;
            for spec in specs.iter() {
                let name = spec.name().to_string();
                match open_store(spec).await {
                    Ok(store) => {
                        tracing::info!(backend = %name, "store opened");
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
                // All backends failed to open — report all errors so
                // the operator can see what went wrong.
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
        };

        let state = ServeState {
            graph_path: Arc::new(graph_path.clone()),
            graph: Arc::new(RwLock::new(snapshot)),
            html,
            d3,
            stores: stores_arc,
            embedder: embedder_arc,
            repo_root: Arc::new(repo_root_path),
            embed_lock: Arc::new(Semaphore::new(4)),
            db_unavailable_reason: Arc::new(db_unavailable_reason),
        };

        let app = Router::new()
            .route("/", get(handle_index))
            .route("/index.html", get(handle_index))
            .route("/d3.v7.min.js", get(handle_d3))
            .route("/graph.json", get(handle_graph))
            .route("/healthz", get(handle_health))
            .route("/api/capabilities", get(api_capabilities))
            .route("/api/graph/stats", get(api_stats))
            .route("/api/graph/node/*id", get(api_node))
            .route("/api/graph/search", get(api_search))
            .route("/api/graph/bfs/*id", get(api_bfs))
            .route("/api/graph/path", get(api_path))
            .route("/api/graph/filter", get(api_filter))
            .route("/api/graph/centrality", get(api_centrality))
            .route("/api/graph/cycles", get(api_cycles))
            // Phase 3 — DB / embedder backed
            .route("/api/db/node/*id", get(api_db_node))
            .route("/api/db/traverse/*id", get(api_db_traverse))
            .route("/api/search/semantic", post(api_search_semantic))
            .route("/api/search/hybrid", post(api_search_hybrid))
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

        let db_api_enabled = state.stores.is_some() && state.embedder.is_some();
        tracing::info!(
            graph = %graph_path.display(),
            nodes,
            edges,
            identity_bytes = identity_size,
            gzip_bytes = gzip_size,
            brotli_bytes = brotli_size,
            encode_secs = t0.elapsed().as_secs_f32(),
            addr = %addr,
            db_api = db_api_enabled,
            db_path = %db_path.display(),
            db_unavailable_reason = state.db_unavailable_reason.as_deref().unwrap_or(""),
            watch,
            "ug serve ready"
        );
        if !db_api_enabled {
            tracing::warn!(
                reason = state.db_unavailable_reason.as_deref().unwrap_or("DB not opened"),
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
        let path = (*state.graph_path).clone();
        let mut last_mtime = file_mtime(&path);
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mtime = file_mtime(&path);
            if mtime.is_none() || mtime == last_mtime {
                continue;
            }
            last_mtime = mtime;
            let path_clone = path.clone();
            let state_clone = state.clone();
            // Parse + recompress can take a few hundred ms on big graphs;
            // do it off the runtime so we don't stall HTTP handlers.
            let _ = tokio::task::spawn_blocking(move || match load_snapshot(&path_clone) {
                Ok(snap) => {
                    let size = snap.encoded.identity.len();
                    let nodes = snap.parsed.nodes.len();
                    let edges = snap.parsed.edges.len();
                    if let Ok(mut w) = state_clone.graph.write() {
                        *w = snap;
                        tracing::info!(
                            target: "ug::serve::watch",
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

async fn handle_d3(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.d3, &headers)
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
    let db_ready = state.stores.is_some();
    let embedder_ready = state.embedder.is_some();

    // Per-destination probe + serialization. `db_node_count` and
    // `search_ready` at the top level reflect the primary backend so
    // existing clients keep working; the new `destinations` array is
    // what the UI keys off for the selector.
    let mut destinations_json: Vec<serde_json::Value> = Vec::new();
    let mut primary_count: Option<usize> = None;
    if let Some(stores) = state.stores.clone() {
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
        state
            .db_unavailable_reason
            .as_deref()
            .map(|s| s.to_string())
    } else if !has_data {
        Some("DB is open but contains no nodes (run `ug index` first)".to_string())
    } else {
        None
    };

    let primary_name = state
        .stores
        .as_ref()
        .map(|s| s.primary.clone())
        .unwrap_or_default();

    let body = serde_json::json!({
        "db_ready": db_ready,
        "embedder_ready": embedder_ready,
        "search_ready": search_ready,
        // Back-compat: existing UI reads `db_node_count` for the primary.
        "db_node_count": primary_count,
        "reason": reason,
        // New in multi-dest: full list with per-backend flags. UI shows
        // a selector when `destinations.length > 1`.
        "destinations": destinations_json,
        "primary": primary_name,
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
    let stores = state.stores.clone().ok_or_else(|| {
        let msg = state
            .db_unavailable_reason
            .as_deref()
            .unwrap_or("DB not opened");
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

    let mut opts = SearchKbOptions::new(&body.query, state.repo_root.as_path());
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

fn node_row_to_json(n: &storage::NodeRow) -> serde_json::Value {
    serde_json::json!({
        "id": n.id,
        "name": n.name,
        "node_type": n.node_type,
        "file": n.file,
        "start_line": n.start_line,
        "end_line": n.end_line,
        "description": n.description,
    })
}

pub fn print_serve_help() {
    println!("  {C_CYAN}ug serve{C_RESET}  {C_YELLOW}— serve visualization + graph API{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug serve [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>   Graph JSON to serve (default: ugout/graph.json)");
    println!("  {C_CYAN}-d, --db{C_RESET} <path>      OverGraph DB for /api/db + /api/search routes (default: ugout/ugdb)");
    println!("  {C_YELLOW}--no-db{C_RESET}            Don't open DB; routes return 503");
    println!("  {C_CYAN}-p, --port{C_RESET} <n>       TCP port (default: 8080)");
    println!("  {C_CYAN}--host{C_RESET} <addr>        Bind address (default: 127.0.0.1)");
    println!("  {C_GREEN}--watch{C_RESET}             Reload graph file when its mtime changes");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>   Repo root for hybrid-search snippet resolution");
    println!("  {C_CYAN}--base-url{C_RESET} <url>      Embedding endpoint");
    println!("  {C_CYAN}--api-key{C_RESET} <key>       Embedding API key");
    println!("  {C_CYAN}--model{C_RESET} <name>        Embedding model");
    println!();
    println!("{C_BOLD}API Endpoints:{C_RESET}");
    println!("  {C_CYAN}GET{C_RESET}  /api/graph/{{stats, node/<id>, search?q=&types=, bfs/<id>?k=,");
    println!("           path?source=&target=, filter?types=, centrality, cycles}}");
    println!("  {C_CYAN}GET{C_RESET}  /api/db/{{node/<id>, traverse/<id>?k=&dir=&types=}}");
    println!("  {C_CYAN}POST{C_RESET} /api/search/{{semantic, hybrid}}  body: JSON");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} -i ugout/graph.json -p 8080");
    println!("  {C_CYAN}ug serve{C_RESET} -i ugout/graph.json -d ugout/ugdb --watch");
}
