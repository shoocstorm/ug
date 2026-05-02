//! HTTP server for the visualization UI plus a read-only graph API.
//! See `docs/SERVE.md` for the full design (Phases 1, 1.5, 2, 3).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::compression::CompressionLayer;

use crate::{
    calculate_centrality as lib_centrality, detect_cycles as lib_cycles, flag_value,
    flag_value_or, has_flag, tokio_runtime,
};
use ultragraph_kb::types::{GraphData, GraphEdge, GraphNode};

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

#[derive(Clone)]
struct ServeState {
    graph_path: Arc<PathBuf>,
    graph: Arc<RwLock<Arc<GraphSnapshot>>>,
    html: Arc<EncodedAsset>,
    d3: Arc<EncodedAsset>,
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

// ---------- Entry point ----------

pub fn run_serve(args: &[String]) {
    let graph_file = flag_value_or(args, &["-i", "--input"], "ug-out/graph.json");
    let port: u16 = flag_value(args, &["-p", "--port"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let host = flag_value_or(args, &["--host"], "127.0.0.1");
    let watch = has_flag(args, "--watch");

    let graph_path = PathBuf::from(&graph_file);

    let t0 = std::time::Instant::now();
    let snapshot = match load_snapshot(&graph_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {}", e);
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

    let state = ServeState {
        graph_path: Arc::new(graph_path.clone()),
        graph: Arc::new(RwLock::new(snapshot)),
        html,
        d3,
    };

    let app = Router::new()
        .route("/", get(handle_index))
        .route("/index.html", get(handle_index))
        .route("/d3.v7.min.js", get(handle_d3))
        .route("/graph.json", get(handle_graph))
        .route("/healthz", get(handle_health))
        .route("/api/graph/stats", get(api_stats))
        .route("/api/graph/node/*id", get(api_node))
        .route("/api/graph/search", get(api_search))
        .route("/api/graph/bfs/*id", get(api_bfs))
        .route("/api/graph/path", get(api_path))
        .route("/api/graph/filter", get(api_filter))
        .route("/api/graph/centrality", get(api_centrality))
        .route("/api/graph/cycles", get(api_cycles))
        // CompressionLayer skips responses that already have Content-Encoding,
        // so it only kicks in for the dynamic /api/* JSON.
        .layer(CompressionLayer::new().br(true))
        .with_state(state.clone());

    let addr: SocketAddr = match format!("{}:{}", host, port).parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: invalid host/port '{}:{}': {}", host, port, e);
            std::process::exit(1);
        }
    };

    let rt = tokio_runtime();
    rt.block_on(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error: bind {} failed: {}", addr, e);
                std::process::exit(1);
            }
        };

        println!("⚡ ug serve");
        println!("  graph:       {} ({} nodes, {} edges)", graph_file, nodes, edges);
        println!(
            "  encoded:     identity {} B  gzip {} B  br {} B  (built in {:?})",
            identity_size,
            gzip_size,
            brotli_size,
            t0.elapsed()
        );
        println!("  listening:   http://{}", addr);
        println!("  ui:          GET /  /index.html");
        println!("  static:      GET /graph.json  /d3.v7.min.js  /healthz");
        println!("  api:         GET /api/graph/{{stats, node/<id>, search, bfs/<id>, path, filter, centrality, cycles}}");
        if watch {
            println!("  watch:       on (mtime poll, 2s)");
            spawn_watch(state.clone());
        }
        println!("  press Ctrl+C to stop");

        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("server error: {}", e);
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
                        println!(
                            "↻ reloaded {} ({} bytes, {} nodes, {} edges)",
                            path_clone.display(),
                            size,
                            nodes,
                            edges
                        );
                    }
                }
                Err(e) => eprintln!("reload failed: {}", e),
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
        .filter(|e| match (
            adj.id_to_idx.get(&e.source),
            adj.id_to_idx.get(&e.target),
        ) {
            (Some(&si), Some(&ti)) => visited.contains(&si) && visited.contains(&ti),
            _ => false,
        })
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

    let not_found = || {
        ok_json(serde_json::json!({ "path": [], "found": false }).to_string())
    };
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
