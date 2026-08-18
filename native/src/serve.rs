//! HTTP server for the visualization UI plus a read-only graph API.
//! See `docs/SERVE.md` for the full design (Phases 1, 1.5, 2, 3).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use tokio::sync::OnceCell;

use axum::body::{Body, Bytes};
use axum::extract::{Json, Path as AxPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Semaphore;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::chat::{self, ChatClient, ChatConfig, ChatMessage, ChatRagOptions};
use crate::cli::args::{flag_value, flag_value_or, has_flag};
use crate::cli::io::die;
use crate::cli::embed::{embedder_from_args, tokio_runtime};
use ultragraph::{
    calculate_centrality_graph as lib_centrality_graph, detect_cycles_graph as lib_cycles_graph,
    C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW,
};
use ultragraph::storage::{
    self, open_store, search_kb as storage_search_kb,
    semantic_search as storage_semantic_search, semantic_search_w_where, traverse_filtered,
    DEFAULT_CONTEXT_CHARS, Direction, Embedder, KnowledgeStore, RankStrategy, SearchKbOptions,
    StoreSpec, DEFAULT_EMBEDDING_DIM,
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
use ultragraph::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};

/// Cap on inbound HTTP request bodies. Every route the UI/agent uses takes a
/// small JSON payload (search query, chat message, config patch, a generate
/// path) — all KB-scale — so 4 MiB is generous headroom while stopping the
/// previous unbounded-read behaviour from being an OOM / abuse vector.
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

// ---------- Encoded asset (identity + gzip + br, all pre-built) ----------

/// `Clone` is cheap and refcounted: `Bytes` clones bump a refcount rather than
/// copying, which is what lets a cached asset be handed out of a `OnceLock`
/// without holding a borrow across an await.
#[derive(Clone)]
/// One response body, plus whichever compressed forms have been asked for.
///
/// Both encodings are built on first request rather than up front. Eagerly
/// compressing at construction meant every `graph.json` was gzip-9'd *and*
/// brotli-9'd before the server would answer anything — minutes of startup CPU
/// on a 330 MB index, holding four copies of it, to produce two bodies of which
/// any one client uses at most one. Past `GRAPH_SERVER_MODE_BYTES` the browser
/// is told to use the slim index and never fetches `graph.json` at all, so on
/// exactly the graphs where that cost hurt most, all of it was waste.
///
/// The trade is that the first client wanting an encoding waits for it on a
/// runtime worker instead of the process waiting at startup. That is the better
/// end of the deal — it is paid once, only when something actually wants the
/// bytes, and the large-graph case skips it — but see the follow-up note in
/// `docs/dev/PERF-TUNING-JOURNEY.md` about warming it in the background.
struct EncodedAsset {
    identity: Bytes,
    gzip: OnceLock<Bytes>,
    brotli: OnceLock<Bytes>,
    content_type: HeaderValue,
}

impl EncodedAsset {
    /// Wrap bytes. **No compression happens here** — see the field comments.
    fn new(raw: Vec<u8>, content_type: &'static str) -> Self {
        Self {
            identity: Bytes::from(raw),
            gzip: OnceLock::new(),
            brotli: OnceLock::new(),
            content_type: HeaderValue::from_static(content_type),
        }
    }

    fn gzip(&self) -> &Bytes {
        self.gzip.get_or_init(|| compress_gzip(&self.identity))
    }

    fn brotli(&self) -> &Bytes {
        self.brotli.get_or_init(|| compress_brotli(&self.identity))
    }

    /// Bytes this asset is holding *right now*, for the snapshot cache
    /// budget. An encoding nobody has asked for costs nothing and is not
    /// counted — which makes the budget an account of real memory rather
    /// than of memory we used to allocate unconditionally.
    fn retained(&self) -> usize {
        self.identity.len()
            + self.gzip.get().map_or(0, |b| b.len())
            + self.brotli.get().map_or(0, |b| b.len())
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
    /// mtime of the `graph.json` this was read from, sampled *before* the
    /// read so a file rewritten mid-read reads as stale rather than current.
    /// `None` for a snapshot with no file behind it (the zero-project
    /// placeholder), which is never refreshed.
    ///
    /// This is what makes a cached snapshot checkable: see
    /// [`refresh_snapshot_if_stale`].
    mtime: Option<SystemTime>,
    adj: OnceLock<AdjIndex>,
    centrality: OnceLock<String>,
    cycles: OnceLock<String>,
    /// The slim node index `/api/graph/nodes` serves — see [`build_slim_index`].
    /// Built on first request and encoded once, like `centrality` and `cycles`,
    /// because a browser in server mode asks for it exactly once per load.
    slim: OnceLock<EncodedAsset>,
    /// `/api/graph/stats`, rendered once per snapshot.
    ///
    /// Every field of it is derived from this snapshot and none of it can
    /// change without the snapshot being replaced, yet it was recomputed per
    /// request — a full pass over both the node and edge lists, which on a
    /// large repo is ~900k iterations to answer a question whose answer is
    /// fixed. The UI polls this.
    stats: OnceLock<String>,
}

impl GraphSnapshot {
    /// The graph JSON as text, borrowed from the bytes `/graph.json` already
    /// serves.
    ///
    /// This used to be a separate `raw_json: String` field alongside
    /// `encoded.identity`, which held a byte-identical copy — a straight
    /// duplicate of the whole graph (16 MB on a mid-size repo) per loaded
    /// project. `encoded.identity` is only ever built from a `String`, so the
    /// UTF-8 check below cannot fail; it is written as a fallback rather than
    /// an `expect` so a future non-UTF-8 asset source degrades instead of
    /// taking the server down.
    fn raw_json(&self) -> &str {
        std::str::from_utf8(&self.encoded.identity).unwrap_or("{}")
    }

    /// Identifies *this* snapshot, so a client that split one graph across
    /// several requests can tell whether they all came from the same one.
    ///
    /// Size plus node/edge counts plus mtime: cheap to compute per request and
    /// certain to change when the graph is regenerated. Not a hash of the
    /// content — hashing 346 MB per `/api/capabilities` call would cost more
    /// than the problem.
    fn token(&self) -> String {
        let mtime = self
            .mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!(
            "{}-{}-{}-{}",
            self.encoded.identity.len(),
            self.parsed.nodes.len(),
            self.parsed.edges.len(),
            mtime
        )
    }
}

/// Adjacency built once per snapshot. `id_to_idx` maps a node's string id to
/// its index in `parsed.nodes`; `out[i]` and `inc[i]` are the **edge** indices
/// into `parsed.edges` whose source (respectively target) is node `i`.
///
/// Storing edge indices rather than neighbour indices is what makes the edge
/// *type* reachable from the index. The previous form kept neighbour indices,
/// so every caller that needed `edge_type` had to rediscover it by scanning the
/// whole edge list — which is exactly what `api_traverse` did, once per request,
/// over three quarters of a million edges on a large repo.
///
/// Both directions are kept because the questions asked of this index are not
/// all directed: "who calls this" and "what is one hop away from this" are
/// inbound and undirected respectively, and a forward-only index answers
/// neither without a full scan.
struct AdjIndex {
    id_to_idx: HashMap<String, usize>,
    out: Vec<Vec<u32>>,
    inc: Vec<Vec<u32>>,
}

impl AdjIndex {
    /// Every edge incident to node `i`, outbound first. Callers that care about
    /// direction compare `edges[ei].source` themselves.
    fn incident(&self, i: usize) -> impl Iterator<Item = u32> + '_ {
        self.out[i].iter().copied().chain(self.inc[i].iter().copied())
    }
}

fn build_adj(graph: &GraphData) -> AdjIndex {
    let id_to_idx: HashMap<String, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect();
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); graph.nodes.len()];
    let mut inc: Vec<Vec<u32>> = vec![Vec::new(); graph.nodes.len()];
    for (ei, e) in graph.edges.iter().enumerate() {
        // An edge index that doesn't fit in a u32 would mean four billion edges
        // in one graph; the cast is checked rather than assumed away.
        let Ok(ei) = u32::try_from(ei) else { break };
        if let (Some(&si), Some(&ti)) = (id_to_idx.get(&e.source), id_to_idx.get(&e.target)) {
            out[si].push(ei);
            inc[ti].push(ei);
        }
    }
    AdjIndex { id_to_idx, out, inc }
}

/// Bytes of `graph.json` at or above which the browser is told to leave the
/// file alone and ask the server its questions instead.
///
/// This is a property of what a browser tab can hold, not of the graph: past
/// roughly this size the download, the `JSON.parse` and the retained object
/// graph together cost more than every interaction the page then performs.
/// Measured on a 346 MB index, the whole-file path retains ~295 MB of JS heap
/// against ~66 MB for the slim index.
const GRAPH_SERVER_MODE_BYTES: usize = 50 * 1024 * 1024;

/// How `ug serve` decides which of the two the browser gets.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum GraphModePolicy {
    /// Pick per project, by `graph.json`'s size. The default.
    Auto,
    /// Always ship the whole file — what every release before this did.
    Local,
    /// Always serve the slim index, whatever the size. For testing the server
    /// path against a small repo.
    Server,
}

impl GraphModePolicy {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "local" => Some(Self::Local),
            "server" => Some(Self::Server),
            _ => None,
        }
    }

    /// `"local"` or `"server"` for a graph of `bytes`.
    fn resolve(self, bytes: usize) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
            Self::Auto if bytes >= GRAPH_SERVER_MODE_BYTES => "server",
            Self::Auto => "local",
        }
    }
}

/// The slim node index: every node the graph has, carrying only the fields the
/// page needs *before* you click something.
///
/// The point is what it leaves out. `graph.json` is dominated by edges — 228 MB
/// of the 346 MB on a large Java repo, nearly all of it the same endpoint id
/// strings written out twice each — and by per-node prose the page does not
/// read until a node is selected. Dropping both is what takes the browser from
/// a 346 MB download to a 2.8 MB one.
///
/// Three encoding decisions, each load-bearing rather than cosmetic:
///
/// * **Columnar.** One array per field instead of one object per node. Saves
///   the parser 160k × 6 key strings, and lets the client build every node with
///   its properties assigned in the same order — one hidden class for the whole
///   graph instead of a shape per field combination.
/// * **Dictionary-coded `node_type` and `file`.** 158,638 file values on that
///   repo are 8,910 distinct paths; written inline they are ~15 MB of JSON and
///   ~32 MB of duplicate JS strings, coded they are under 1 MB and ~1.8 MB.
/// * **Position is identity.** A node's index in these arrays *is* its id
///   everywhere else in the server-mode protocol, which is what lets edge
///   endpoints travel as integers rather than as 141-character strings.
///
/// `boundary` is a sparse list of indices, not a column: on a 161,725-node
/// graph exactly 170 nodes carry one.
fn build_slim_index(graph: &GraphData) -> String {
    let n = graph.nodes.len();

    // Dictionaries, in first-seen order so the arrays stay stable between
    // builds of the same graph.
    let mut type_names: Vec<&'static str> = Vec::new();
    let mut type_idx: HashMap<&'static str, u32> = HashMap::new();
    let mut file_names: Vec<&str> = Vec::new();
    let mut file_idx: HashMap<&str, i64> = HashMap::new();

    let mut ids: Vec<&str> = Vec::with_capacity(n);
    let mut names: Vec<&str> = Vec::with_capacity(n);
    let mut types: Vec<u32> = Vec::with_capacity(n);
    let mut files: Vec<i64> = Vec::with_capacity(n);
    let mut start: Vec<u32> = Vec::with_capacity(n);
    let mut end: Vec<u32> = Vec::with_capacity(n);
    let mut boundary: Vec<u32> = Vec::new();

    for (i, node) in graph.nodes.iter().enumerate() {
        ids.push(&node.id);
        names.push(if node.name.is_empty() { &node.id } else { &node.name });

        let ty = node.node_type.as_str();
        let next = type_names.len() as u32;
        let ti = *type_idx.entry(ty).or_insert_with(|| {
            type_names.push(ty);
            next
        });
        types.push(ti);

        match node.file.as_deref() {
            Some(f) => {
                let next = file_names.len() as i64;
                let fi = *file_idx.entry(f).or_insert_with(|| {
                    file_names.push(f);
                    next
                });
                files.push(fi);
            }
            // -1 rather than null: a column of one type parses faster and the
            // client's check is `>= 0` either way.
            None => files.push(-1),
        }

        start.push(node.start_line.unwrap_or(0));
        end.push(node.end_line.unwrap_or(0));
        if !node.boundaries.is_empty() {
            boundary.push(i as u32);
        }
    }

    // Undirected degree, plus the Contains parentage the catalog needs. Both
    // are whole-graph answers the slim index cannot otherwise support, and both
    // are cheap here because we are already walking every edge once.
    let mut deg: Vec<u32> = vec![0; n];
    let mut has_container: Vec<bool> = vec![false; n];
    let idx_of: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut node_type_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for node in &graph.nodes {
        *node_type_counts.entry(node.node_type.as_str()).or_insert(0) += 1;
    }
    let mut edge_type_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in &graph.edges {
        *edge_type_counts.entry(e.edge_type.as_str()).or_insert(0) += 1;
        let (Some(&si), Some(&ti)) = (idx_of.get(e.source.as_str()), idx_of.get(e.target.as_str()))
        else {
            continue;
        };
        deg[si] += 1;
        if si != ti {
            deg[ti] += 1;
        }
        // Only Folder/File parents make a catalog root — a Function contained
        // by a File is a child in the tree, but a File contained by a Folder is
        // what stops that File being a root.
        if matches!(e.edge_type, GraphEdgeType::Contains)
            && matches!(
                graph.nodes[si].node_type,
                GraphNodeType::Folder | GraphNodeType::File
            )
            && matches!(
                graph.nodes[ti].node_type,
                GraphNodeType::Folder | GraphNodeType::File
            )
        {
            has_container[ti] = true;
        }
    }
    let catalog_roots: Vec<u32> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, node)| {
            !has_container[*i]
                && matches!(
                    node.node_type,
                    GraphNodeType::Folder | GraphNodeType::File
                )
        })
        .map(|(i, _)| i as u32)
        .collect();

    // The repo-root folder node carries the language breakdown, same as
    // `api_stats` reads it.
    let root_folder = graph
        .nodes
        .iter()
        .filter_map(|node| node.folder.as_ref())
        .min_by_key(|f| f.depth);

    serde_json::json!({
        "v": 1,
        "n": n,
        "edgeCount": graph.edges.len(),
        // Not `snap.token()` — this builder only sees the parsed graph. The
        // client compares against the token in `/api/capabilities`, and the
        // two agree because both are derived from the same snapshot.
        "nodeCount": n,
        "ids": ids,
        "names": names,
        "types": type_names,
        "typeIdx": types,
        "files": file_names,
        "fileIdx": files,
        "startLine": start,
        "endLine": end,
        "boundary": boundary,
        "deg": deg,
        "catalogRoots": catalog_roots,
        "nodeTypeCounts": node_type_counts,
        "edgeTypeCounts": edge_type_counts,
        // Verbatim, because `IndexStats` already serialises to the camelCase
        // shape `transformData` reads — the page needs no translation layer.
        "stats": graph.stats,
        "languages": root_folder.map(|f| f.language_breakdown.clone()),
        "kbType": root_folder
            .and_then(|f| f.classification.as_ref())
            .map(|c| format!("{:?}", c).to_lowercase()),
    })
    .to_string()
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

impl ProjectContext {
    /// Rough resident size of this project's snapshot, for cache accounting.
    ///
    /// The encoded buffers are measured exactly. `parsed` is estimated at 3×
    /// the identity bytes: a `GraphData` of `String`-heavy structs runs
    /// noticeably larger than its JSON, and over-estimating only makes the
    /// cache more conservative. Precision isn't the point — staying off an
    /// unbounded growth curve is.
    fn approx_bytes(&self) -> usize {
        let snap = self.graph.read().expect("graph poisoned");
        let identity = snap.encoded.identity.len();
        identity
            .saturating_mul(3)
            // `retained`, not identity + both encodings: an encoding nobody
            // has requested has not been built and is costing nothing, so
            // charging the project for it would evict live snapshots to make
            // room for memory that was never allocated.
            .saturating_add(snap.encoded.retained())
            // The slim index is another whole encoded asset once it has been
            // asked for — ~34 MB identity plus whatever compressions have been
            // served. Uncounted, the LRU would hold three of them for free.
            .saturating_add(snap.slim.get().map_or(0, |s| s.retained()))
    }
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
    /// Recency order over `loaded`, least-recently-used first. Kept
    /// alongside rather than inside the map so the hot read path
    /// (`active_ctx`) stays a plain lookup.
    lru: RwLock<Vec<String>>,
    /// Byte ceiling for cached snapshots — see [`snapshot_cache_budget`].
    cache_budget: usize,
}

/// How many bytes of graph snapshot `ug serve` keeps resident across all
/// projects before it starts evicting.
///
/// `loaded` used to grow without bound: `resolve_ctx` loads a project on demand
/// for any request carrying `?project=<name>`, so an agent walking every
/// project pinned every snapshot for the life of the process — half a gigabyte
/// across six mid-size repos. 512 MiB keeps the common case (a handful of
/// projects) entirely cached while putting a ceiling on the pathological one.
fn snapshot_cache_budget() -> usize {
    const DEFAULT: usize = 512 * 1024 * 1024;
    std::env::var("UG_SERVE_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT)
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
        let hit = self.loaded.read().expect("loaded poisoned").get(name).cloned();
        if hit.is_some() {
            self.touch(name);
        }
        hit
    }

    /// Move `name` to the most-recently-used end of the LRU order.
    fn touch(&self, name: &str) {
        let mut lru = self.lru.write().expect("lru poisoned");
        lru.retain(|n| n != name);
        lru.push(name.to_string());
    }

    /// Cache a loaded project without changing the active selection.
    /// Per-request project scoping uses this: a read targeting another
    /// project must not reconfigure the UI for every other client.
    fn insert_loaded(&self, ctx: Arc<ProjectContext>) {
        let name = ctx.name.clone();
        self.loaded
            .write()
            .expect("loaded poisoned")
            .insert(name.clone(), ctx);
        self.touch(&name);
        self.evict_over_budget();
    }

    fn insert_and_activate(&self, ctx: Arc<ProjectContext>) {
        let name = ctx.name.clone();
        self.loaded
            .write()
            .expect("loaded poisoned")
            .insert(name.clone(), ctx);
        *self.active.write().expect("active poisoned") = name.clone();
        self.touch(&name);
        self.evict_over_budget();
    }

    fn set_active(&self, name: &str) {
        *self.active.write().expect("active poisoned") = name.to_string();
    }

    /// Drop least-recently-used snapshots until the cache fits its budget.
    ///
    /// The active project is never evicted — [`active_ctx`] asserts it is
    /// loaded, and dropping it would panic every subsequent request. Evicting
    /// is safe for in-flight work regardless: handlers hold their own `Arc`
    /// clone, so removing the registry's reference frees the memory only once
    /// the last reader is done with it.
    fn evict_over_budget(&self) {
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
async fn close_project_stores(registry: &Arc<ProjectRegistry>, name: &str) {
    let Some(ctx) = registry.get_loaded(name) else { return };
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
fn build_placeholder_context(registry: &Arc<ProjectRegistry>) -> Arc<ProjectContext> {
    let empty_graph = GraphData {
        nodes: Vec::new(),
        edges: Vec::new(),
        stats: None,
        resolution: None,
    };
    let raw_json =
        serde_json::to_string(&empty_graph).unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string());
    let encoded = EncodedAsset::new(raw_json.into_bytes(), "application/json; charset=utf-8");
    let snapshot = Arc::new(GraphSnapshot {
        encoded,
        parsed: empty_graph,
        // No file behind it, so nothing to check it against.
        mtime: None,
        adj: OnceLock::new(),
        centrality: OnceLock::new(),
        cycles: OnceLock::new(),
        slim: OnceLock::new(),
        stats: OnceLock::new(),
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
pub(crate) struct ServeState {
    registry: Arc<ProjectRegistry>,
    html: Arc<EncodedAsset>,
    bundle: Arc<EncodedAsset>,
    cosmos_bundle: Arc<EncodedAsset>,
    favicon: Arc<EncodedAsset>,
    /// `None` when the embedder couldn't be constructed (e.g. missing endpoint).
    /// Phase 3 search routes need it; `/api/db/*` routes don't.
    embedder: Option<Arc<Embedder>>,
    /// Default chat config from CLI flags / env vars / `~/.ug/config.json`.
    /// The `/api/chat` route also accepts per-request overrides
    /// (chat_model, base_url, …) so the UI can flip models without
    /// restarting the server. `None` when no chat model is configured
    /// anywhere; routes return 503 in that case. Behind a lock because
    /// `POST /api/config` rebuilds it when the user saves settings.
    chat_default: Arc<RwLock<Option<ChatConfig>>>,
    /// The args `ug serve` was started with, kept so `/api/config` can
    /// report which values are pinned by CLI flags and rebuild
    /// `chat_default` with the same flag precedence after a save.
    serve_args: Arc<Vec<String>>,
    /// Process-wide cap on concurrent embedding calls. Cheap insurance against
    /// hammering the embedding endpoint when many search requests land at once.
    embed_lock: Arc<Semaphore>,
    /// Background `ug gen` jobs kicked off from the KB Manager wizard.
    gen_jobs: Arc<GenJobs>,
    /// Last computed `/api/projects/staleness` payload, reused for
    /// [`STALENESS_TTL`] so N open tabs cost one filesystem scan, not N.
    staleness: Arc<RwLock<Option<StalenessCache>>>,
    /// Whether the browser is handed the whole `graph.json` or the slim index.
    /// The *policy* is per-server (`--graph-mode`); the mode it resolves to is
    /// per-project, because size is a property of the graph.
    graph_mode: GraphModePolicy,
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
    // Sampled before the read, so a rewrite that lands while we are reading
    // leaves the snapshot looking older than the file and the next freshness
    // check picks it up. The other order would record the post-write mtime
    // against pre-write content and never correct itself.
    let mtime = file_mtime(path);
    let raw = std::fs::read(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let raw_json =
        String::from_utf8(raw).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
    let parsed: GraphData =
        serde_json::from_str(&raw_json).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    // Hand the JSON straight to the encoder — it becomes `encoded.identity`,
    // which `GraphSnapshot::raw_json()` borrows back. Cloning it into a
    // second field is what used to double this snapshot's footprint.
    let encoded = EncodedAsset::new(raw_json.into_bytes(), "application/json; charset=utf-8");
    Ok(Arc::new(GraphSnapshot {
        encoded,
        parsed,
        mtime,
        adj: OnceLock::new(),
        centrality: OnceLock::new(),
        cycles: OnceLock::new(),
        slim: OnceLock::new(),
        stats: OnceLock::new(),
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

/// Which project a `--project`-less `ug serve` starts on, and why.
///
/// Same precedence as `ug mcp install`: the project the user explicitly
/// pinned with `ug active` wins, then the one matching the cwd basename,
/// then the most recently indexed one (`names` comes from
/// `list_projects`, sorted most-recent first). Without the active step,
/// serving from a subdirectory (`.../ug/native` → no `native` project)
/// silently landed on whatever happened to be indexed last, so the UI
/// disagreed with what `ug active` reported.
///
/// Pure so the precedence can be tested without racing other tests over
/// `$UG_HOME`. `names` must be non-empty.
fn pick_initial_project(
    names: &[String],
    active: Option<String>,
    cwd_name: String,
) -> (String, &'static str) {
    // A stale marker can name a project that isn't listed; ignore it.
    if let Some(name) = active.filter(|a| names.iter().any(|n| n == a)) {
        return (name, "active project");
    }
    if let Some(name) = names.iter().find(|n| **n == cwd_name) {
        return (name.clone(), "matches the current directory");
    }
    (names[0].clone(), "most recently indexed project")
}

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
    // `flag_value` takes `--flag value`, never `--flag=value`. Every other flag
    // here has the same shape, but silently ignoring the `=` form would leave
    // the server in the opposite mode to the one asked for with nothing said —
    // so it is named as an error rather than skipped.
    if let Some(bad) = args.iter().find(|a| a.starts_with("--graph-mode=")) {
        die(
            2,
            format!("use `--graph-mode <value>`, not `{bad}` — this CLI takes flag values as a separate argument"),
        );
    }
    let graph_mode = match flag_value(args, &["--graph-mode"]) {
        Some(raw) => match GraphModePolicy::parse(&raw) {
            Some(p) => p,
            None => die(
                2,
                format!("--graph-mode must be auto, local or server (got {raw:?})"),
            ),
        },
        None => GraphModePolicy::Auto,
    };

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
                        let names: Vec<String> =
                            projects.iter().map(|(_, m)| m.name.clone()).collect();
                        let (initial, why) = pick_initial_project(
                            &names,
                            crate::project::get_active_project(),
                            crate::project::derive_project_name("."),
                        );
                        tracing::info!(project = %initial, reason = why, "initial project");
                        initial
                    }
                };
                Startup::Multi { initial }
            }
        }
    };

    let html = Arc::new(EncodedAsset::new(
        crate::assets::VIS_HTML.as_bytes().to_vec(),
        "text/html; charset=utf-8",
    ));
    let bundle = Arc::new(EncodedAsset::new(
        crate::assets::VIS_THREEJS_BUNDLE.to_vec(),
        "application/javascript; charset=utf-8",
    ));
    let cosmos_bundle = Arc::new(EncodedAsset::new(
        crate::assets::VIS_COSMOS_BUNDLE.to_vec(),
        "application/javascript; charset=utf-8",
    ));
    let favicon = Arc::new(EncodedAsset::new(
        crate::assets::VIS_FAVICON.to_vec(),
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
            lru: RwLock::new(Vec::new()),
            cache_budget: snapshot_cache_budget(),
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
                let repo_root_override = flag_value(args, &["--repo-root"])
                    .map(PathBuf::from)
                    .map(|raw| {
                        // A repo root that no longer exists must not stop the
                        // server — the index serves content on its own, and
                        // every consumer of repo_root already tolerates a
                        // missing path. Canonicalize when possible so relative
                        // roots resolve; otherwise keep the raw path.
                        std::fs::canonicalize(&raw).unwrap_or(raw)
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

        let (identity_size, nodes, edges) = {
            let snap = initial_ctx.graph.read().expect("graph state poisoned");
            (
                snap.encoded.identity.len(),
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
            cosmos_bundle,
            favicon,
            embedder: embedder_arc,
            chat_default: Arc::new(RwLock::new(chat_default)),
            serve_args: Arc::new(args.to_vec()),
            embed_lock: Arc::new(Semaphore::new(4)),
            gen_jobs: Arc::new(GenJobs::new()),
            staleness: Arc::new(RwLock::new(None)),
            graph_mode,
        };

        let app = build_router(state.clone());

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
            // No gzip/brotli sizes here any more: nothing has been compressed
            // by the time the server is ready, which is the point of P3.1.
            startup_secs = t0.elapsed().as_secs_f32(),
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
        tracing::warn!(
            "ug serve is for local use: it binds to loopback by default and has \
             no authentication. Do not expose it to a network or run it on a \
             production server without a secured reverse proxy (auth + TLS + \
             network policy) in front."
        );

        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "server crashed");
            std::process::exit(1);
        }
    });
}

/// Extra hostnames this server will answer to, from `UG_ALLOWED_HOSTS`
/// (comma-separated). Needed only when `ug serve` sits behind a reverse
/// proxy that forwards a real domain in `Host`; the loopback and
/// bare-IP cases below need no configuration.
fn extra_allowed_hosts() -> &'static HashSet<String> {
    static HOSTS: OnceLock<HashSet<String>> = OnceLock::new();
    HOSTS.get_or_init(|| {
        std::env::var("UG_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().trim_matches(['[', ']']).to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect()
    })
}

/// Strip the `:port` and any IPv6 brackets from a `Host`/`Origin` authority,
/// returning the lowercased hostname.
///
/// Splitting on the *last* colon is wrong for a bracketless IPv6 literal, so
/// bracketed forms are unwrapped first and anything still holding more than
/// one colon is treated as a bare IPv6 address rather than host:port.
fn host_label(authority: &str) -> String {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    if a.matches(':').count() > 1 {
        return a.to_ascii_lowercase(); // bare IPv6 literal
    }
    a.split(':').next().unwrap_or_default().to_ascii_lowercase()
}

/// Does `host` name *this* machine, as opposed to some attacker-controlled
/// domain that merely resolves to it right now?
///
/// An IP literal is accepted outright: a browser sends the hostname the page
/// was loaded from, so a rebinding attack always arrives carrying a domain
/// name, never a bare address. That keeps `--host 0.0.0.0` reachable over the
/// LAN by IP while still rejecting `http://evil.tld` rebound to 127.0.0.1.
fn is_allowed_host(host: &str) -> bool {
    let h = host_label(host);
    if h.is_empty() {
        return false;
    }
    h == "localhost"
        || h.ends_with(".localhost")
        || h.parse::<IpAddr>().is_ok()
        || extra_allowed_hosts().contains(&h)
}

/// Reject requests whose `Host` or `Origin` names a domain this server
/// doesn't answer to.
///
/// This is the DNS-rebinding defense, and it is what makes the rest of the
/// server's "it only listens on loopback" assumption actually hold. The
/// `CorsLayer` below stops a cross-origin page from *reading* a response, but
/// rebinding sidesteps CORS entirely: the attacker's own domain is re-pointed
/// at 127.0.0.1, so the browser considers the request same-origin and hands
/// over the reply. The one thing that still distinguishes it from a genuine
/// local request is the `Host` header, which carries the attacker's domain.
///
/// `Origin` is checked with the same predicate so a cross-site form post — a
/// "simple" request that needs no preflight and so is never blocked by CORS —
/// can't reach a state-changing route either.
async fn guard_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        // HTTP/2 carries the authority in the URI instead of a Host header.
        .or_else(|| req.uri().host().map(str::to_string));

    if let Some(host) = host {
        if !is_allowed_host(&host) {
            tracing::warn!(%host, "rejected request with a non-local Host header");
            return err_json(
                StatusCode::FORBIDDEN,
                "Host header is not a local address — refusing the request (set \
                 UG_ALLOWED_HOSTS if this server is behind a reverse proxy)",
            );
        }
    }

    // `Origin: null` (sandboxed iframe, file://) is not a host we can check
    // and not one we should trust with a state change.
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        let allowed = origin
            .split("://")
            .nth(1)
            .is_some_and(|authority| is_allowed_host(authority));
        if !allowed {
            tracing::warn!(%origin, "rejected request with a cross-site Origin header");
            return err_json(StatusCode::FORBIDDEN, "cross-site Origin is not allowed");
        }
    }

    next.run(req).await
}

/// Assemble the full route table and middleware stack over an already-built
/// [`ServeState`].
///
/// Split out of `run_serve` so the router can be constructed without binding a
/// port or taking over the process: tests drive it through
/// `tower::ServiceExt::oneshot`, which is the only way handler behaviour
/// (project scoping, traversal rejection, the `--no-db` 503 paths) is reachable
/// from a unit test at all.
pub(crate) fn build_router(state: ServeState) -> Router {
    Router::new()
        .route("/", get(handle_index))
        .route("/index.html", get(handle_index))
        .route("/threejs-vis.bundle.js", get(handle_bundle))
        .route("/cosmos-vis.bundle.js", get(handle_cosmos_bundle))
        .route("/favicon.svg", get(handle_favicon))
        .route("/graph.json", get(handle_graph))
        .route("/indexed-tree.json", get(handle_indexed_tree))
        .route("/healthz", get(handle_health))
        .route("/api/projects", get(api_projects))
        .route("/api/projects/select", post(api_projects_select))
        .route("/api/projects/delete", post(api_projects_delete))
        .route("/api/generate", post(api_generate))
        .route("/api/generate/status", get(api_generate_status))
        // Re-embed an already-indexed project from the UI. Same job
        // tracker as /api/generate, so its status is polled the same way.
        .route("/api/ingest", post(api_ingest))
        .route("/api/browse-dir", get(api_browse_dir))
        .route("/api/capabilities", get(api_capabilities))
        .route("/api/config", get(api_config_get).post(api_config_post))
        .route("/api/graph/stats", get(api_stats))
        .route("/api/projects/staleness", get(api_projects_staleness))
        .route("/api/graph/nodes", get(api_slim_index))
        .route("/api/graph/edges", post(api_edges))
        .route("/api/graph/nodes/hydrate", post(api_hydrate))
        .route("/api/graph/node/*id", get(api_node))
        .route("/api/graph/search", get(api_search))
        .route("/api/graph/traverse/*id", get(api_traverse))
        .route("/api/graph/path", get(api_path))
        .route("/api/graph/filter", get(api_filter))
        .route("/api/graph/centrality", get(api_centrality))
        .route("/api/graph/cycles", get(api_cycles))
        // Agent tools — the same seven the CLI and MCP expose.
        .route("/api/tools", get(api_tools))
        .route("/api/presets", get(api_presets))
        .route("/api/tools/:tool", post(api_tool))
        // Source file content for the right-panel "Preview" tab.
        .route("/api/file", get(api_file))
        // Phase 3 — DB / embedder backed
        .route("/api/db/node/*id", get(api_db_node))
        .route("/api/db/traverse/*id", get(api_db_traverse))
        .route("/api/search/semantic", post(api_search_semantic))
        .route("/api/search/hybrid", post(api_search_hybrid))
        .route("/api/chat", post(api_chat))
        .route("/api/tour", post(api_tour))
        .route("/api/chat/config", get(api_chat_config))
        // CompressionLayer skips responses that already have Content-Encoding,
        // so it only kicks in for the dynamic /api/* JSON.
        .layer(CompressionLayer::new().br(true))
        // Reject oversized request bodies before they reach a handler —
        // every legitimate payload is KB-scale, so 4 MiB is pure abuse
        // protection and never bites the app.
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        // Default CORS policy denies cross-origin requests: same-origin
        // clients (the web UI and the Tauri shell at 127.0.0.1:8080) are
        // never subject to CORS and pass through untouched, while a
        // cross-origin browser fetch hits an empty preflight response and
        // is blocked — the CSRF / drive-by defense.
        .layer(CorsLayer::new())
        // CORS alone does not stop DNS rebinding, which makes the attacker's
        // page same-origin with us. `guard_host` checks the one header that
        // still gives it away. Outermost so it runs before any handler —
        // and before the body limit, so a rejected host costs nothing.
        .layer(middleware::from_fn(guard_host))
        // One INFO span per request: method+uri on entry, status+latency on exit.
        // Matches the structured-log pattern the rest of the server uses.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::DEBUG))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}

// ---------- Watch (Phase 1.5) ----------

fn spawn_watch(state: ServeState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            // The snapshot carries the mtime it was read at, so "has this
            // changed" is answered by the context itself rather than by a
            // side table of last-seen mtimes here. That side table was the
            // only reason a *newly activated* project needed a priming tick,
            // and it could only ever track the active one.
            refresh_snapshot_if_stale(&state.registry.active_ctx()).await;
        }
    });
}

/// Reload a project's snapshot when its `graph.json` has changed since the
/// snapshot was read. No-op when it hasn't, or when there is no file to
/// compare against (the zero-project placeholder).
///
/// Every cached context needs this, not just the active one. `resolve_ctx`
/// loads a project on demand for any request carrying `?project=<name>` and
/// then keeps it in `loaded` indefinitely, while the watcher only ever looked
/// at the active project — so a CLI `ug gen` / `ug ingest` against some other
/// project landed, and every later request for it kept answering from the
/// pre-run graph, with no error and no staleness note. The MCP server has
/// always checked mtime on read (`mcp::Mcp::load_graph`); the two doors have
/// to agree about what the graph contains.
///
/// One `metadata` call inline, which is bounded; the read + parse behind it
/// scales with the graph, so it goes to `spawn_blocking`.
async fn refresh_snapshot_if_stale(ctx: &Arc<ProjectContext>) {
    let current = file_mtime(&ctx.graph_path);
    if current.is_none() {
        return; // no graph.json behind this context — nothing to compare
    }
    {
        let held = ctx.graph.read().expect("graph state poisoned");
        if held.mtime == current {
            return;
        }
    }

    let path = ctx.graph_path.clone();
    // Parse + recompress can take a few hundred ms on big graphs; do it off
    // the runtime so we don't stall HTTP handlers.
    let loaded = tokio::task::spawn_blocking(move || load_snapshot(&path)).await;
    match loaded {
        Ok(Ok(snap)) => {
            let bytes = snap.encoded.identity.len();
            let nodes = snap.parsed.nodes.len();
            let edges = snap.parsed.edges.len();
            if let Ok(mut w) = ctx.graph.write() {
                *w = snap;
                tracing::info!(
                    target: "ug::serve::watch",
                    project = %ctx.name,
                    path = %ctx.graph_path.display(),
                    bytes,
                    nodes,
                    edges,
                    "graph reloaded"
                );
            }
        }
        Ok(Err(e)) => tracing::warn!(
            target: "ug::serve::watch",
            project = %ctx.name,
            error = %e,
            "graph reload failed"
        ),
        Err(e) => tracing::warn!(
            target: "ug::serve::watch",
            project = %ctx.name,
            error = %e,
            "graph reload task failed"
        ),
    }
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
async fn ctx_indexed_source(
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
async fn resolve_ctx(
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

/// GET /api/tools — discovery for the graph-backed agent tools, so an agent
/// speaking HTTP can enumerate them the way an MCP client reads `tools/list`.
/// Lists the store-backed tools too (`POST /api/tools/:tool` dispatches those
/// through their own arm, not [`agent_tools::run_tool`]), so nothing an agent
/// can call is invisible.
async fn api_tools() -> Response {
    let store_backed: Vec<&str> = ultragraph::agent_tools::STORE_BACKED_AGENT_TOOLS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    let tools: Vec<serde_json::Value> = ultragraph::agent_tools::AGENT_TOOLS
        .iter()
        .chain(ultragraph::agent_tools::STORE_BACKED_AGENT_TOOLS.iter())
        .map(|(name, summary)| {
            let mut entry = serde_json::json!({
                "name": name,
                "summary": summary,
                "path": format!("/api/tools/{}", name),
                "method": "POST",
                // A copyable body beats a parameter list: it shows the shape
                // and the wildcard form in the same round trip.
                "example": ultragraph::agent_tools::tool_example(name),
            });
            if store_backed.contains(name) {
                entry["note"] = serde_json::json!(
                    "Store-backed: needs the indexed database (503 otherwise). \
                     The analyze preset catalog lives at GET /api/presets."
                );
            }
            entry
        })
        .collect();
    ok_json(
        serde_json::json!({
            "tools": tools,
            "params": "Canonical snake_case, same as the CLI flags and MCP tool params. Legacy camelCase spellings are accepted.",
            "project": "Optional `project` field targets another indexed project without changing the server's active one.",
            "wildcards": {
                "syntax": ultragraph::pattern::SYNTAX_SUMMARY,
                "where": "Anywhere a symbol or file is named: find_symbols.name / .node_types / .file_prefix, file_outline.file, and the node_id of get_code, find_usages, traverse and shortest_path — which also accept a plain symbol name instead of an id.",
                "expansion": format!(
                    "In the id-taking tools a name or pattern expands to at most {} symbols; hitting that cap is reported in the result, never silent.",
                    ultragraph::agent_tools::MAX_REF_EXPANSION
                ),
            },
        })
        .to_string(),
    )
}

/// GET /api/presets — the `analyze` preset registry.
///
/// Served from the same registry the MCP `graph_schema` manifest reads, so
/// the UI and an agent can never disagree about what exists. `source` is
/// there for the day presets can also come from a repo's
/// `.ug/presets.toml`: a card built from the working tree needs to be
/// visibly distinguishable from one ug shipped.
async fn api_presets() -> Response {
    let presets: Vec<serde_json::Value> = ultragraph::analyze::presets::all()
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "category": p.category.as_str(),
                "description": p.description,
                "source": "builtin",
                "params": p.params.iter().map(|q| serde_json::json!({
                    "name": q.name,
                    "description": q.description,
                    "required": q.default.is_none(),
                })).collect::<Vec<_>>(),
                "headline": p.headline,
            })
        })
        .collect();
    ok_json(
        serde_json::json!({
            "presets": presets,
            // Shipped alongside so the UI's "what can I query?" and the MCP
            // capability manifest cannot drift apart — both read the same
            // constant, and neither hardcodes a list that quietly goes stale
            // when a fact is added.
            "properties": ultragraph::analyze::QUERYABLE_PROPERTIES,
            "run": "POST /api/tools/analyze with {\"preset\": \"<name>\", \"args\": {…}}",
        })
        .to_string(),
    )
}

/// `analyze` over HTTP.
///
/// Split out of [`api_tool`] because it is the one tool there that needs
/// the store rather than `graph.json` — aggregation and reachability run
/// on indexed properties. Answers from the *resolved* project's store, so
/// a request carrying `project` analyses the project it named, like every
/// other tool answers from its graph. Returns rows as JSON rather than the
/// rendered text an agent reads, since the caller is the viz layer, which
/// wants to build its own table and map result ids onto graph selection.
async fn api_analyze(ctx: &ProjectContext, params: serde_json::Value) -> Response {
    let store = match ctx.stores.as_ref().and_then(|s| s.get(&s.primary)) {
        Some(s) => s.clone(),
        None => {
            let reason = ctx
                .db_unavailable_reason
                .as_deref()
                .unwrap_or("DB not opened");
            return err_json(StatusCode::SERVICE_UNAVAILABLE, reason);
        }
    };
    // Validation lives in `analyze::run` — an unknown preset, a missing
    // required argument and a malformed query all come back from there with
    // a message that names the alternatives, which is more than this layer
    // could say.
    let request = parse_analyze_body(&params);

    match ultragraph::analyze::run(store.as_ref(), &request).await {
        Ok(answer) => {
            // Ship only the requested window, not the whole result.
            // Returning every row and leaving the client to slice would
            // defeat the point of a range — the expensive part of paging
            // is transferring rows the caller already has.
            let total = answer.page.rows.len();
            let (from, to) = answer.window.slice(total);
            ok_json(
            serde_json::json!({
                "title": answer.title,
                "description": answer.description,
                "gql": answer.gql,
                "columns": answer.page.columns,
                "rows": answer.page.rows[from..to].iter().map(|r| {
                    r.iter().map(query_value_to_json).collect::<Vec<_>>()
                }).collect::<Vec<_>>(),
                // Three different denominators, all needed to page honestly:
                // how many rows exist, which ones these are, and how many
                // graph elements matched before grouping collapsed them.
                "rowsTotal": total,
                "from": from + 1,
                "to": to,
                "rowsMatched": answer.page.rows_matched,
                "truncated": answer.page.truncated,
                "warnings": answer.page.warnings,
                // Coverage rides along rather than being left to the
                // client to remember: a chart drawn over an unindexed
                // property is the same confident lie as a printed zero.
                "coverage": answer.coverage.iter().map(|c| serde_json::json!({
                    "property": c.property,
                    "present": c.present,
                    "total": c.total,
                })).collect::<Vec<_>>(),
                "unindexed": answer.unindexed,
                "text": ultragraph::analyze::render::render(
                    &answer,
                    ultragraph::agent_tools::Render::Markdown,
                ),
            })
            .to_string(),
            )
        }
        Err(e) => err_json(StatusCode::BAD_REQUEST, &e),
    }
}

fn query_value_to_json(v: &ultragraph::storage::store::QueryValue) -> serde_json::Value {
    use ultragraph::storage::store::QueryValue as Q;
    match v {
        Q::Null => serde_json::Value::Null,
        Q::Bool(b) => serde_json::Value::Bool(*b),
        Q::Int(i) => serde_json::Value::from(*i),
        Q::Float(f) => serde_json::Value::from(*f),
        Q::Str(s) => serde_json::Value::from(s.clone()),
        Q::List(items) => serde_json::Value::Array(items.iter().map(query_value_to_json).collect()),
    }
}

fn parse_analyze_body(body: &serde_json::Value) -> ultragraph::analyze::AnalyzeParams {
    let text = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let mut parsed = ultragraph::analyze::AnalyzeParams {
        preset: text("preset"),
        gql: text("gql"),
        limit: body.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize),
        range: text("range"),
        ..Default::default()
    };
    if let Some(obj) = body.get("args").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            let as_text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                // A model often sends a list param as a JSON array
                // (`{"files": ["a.ts","b.rs"]}`). analyze binds list params
                // from a comma-separated string, so join the string elements
                // rather than stringify the array (which would keep the
                // brackets and break the split) — same as the MCP side.
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                other => other.to_string(),
            };
            parsed.args.insert(k.clone(), as_text);
        }
    }
    parsed
}

/// POST /api/tools/:name — run one graph-backed agent tool and return its
/// JSON envelope. Same dispatch, params and output as `ug <name> --json` and
/// the matching MCP tool.
async fn api_tool(
    State(state): State<ServeState>,
    AxPath(tool): AxPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let mut params = match body {
        Some(Json(serde_json::Value::Object(m))) => m,
        // No body, or a non-object body, means "no params" — valid for
        // project_overview and graph_schema.
        _ => serde_json::Map::new(),
    };
    let project = params
        .remove("project")
        .and_then(|v| v.as_str().map(str::to_string));

    let ctx = match resolve_ctx(&state.registry, project.as_deref()).await {
        Ok(c) => c,
        Err(e) if e.starts_with("unknown project") => {
            return err_json(StatusCode::NOT_FOUND, &e)
        }
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };

    // `analyze` is store-backed, so it never reaches `run_tool` — that
    // dispatcher only knows about graph.json. It still gets the same arg
    // coercion as the MCP and chat paths, and answers from the resolved
    // project's store just like every other tool answers from its graph.
    if tool == "analyze" {
        let mut args = serde_json::Value::Object(params);
        crate::mcp::tools::normalize_args(&tool, &mut args);
        return api_analyze(&ctx, args).await;
    }

    let snap = ctx.graph.read().expect("graph state poisoned").clone();
    // Same coercion the chat and MCP paths apply — every entry point sees
    // the same stringified-array mistakes, so normalise in all of them.
    let mut args = serde_json::Value::Object(params);
    crate::mcp::tools::normalize_args(&tool, &mut args);
    let indexed = ctx_indexed_source(&ctx, &snap.parsed, &tool, &args).await;
    let result = ultragraph::agent_tools::run_tool(
        &tool,
        &snap.parsed,
        snap.raw_json(),
        ultragraph::agent_tools::SourceCtx::new(&indexed, ctx.repo_root.as_path()),
        ctx.graph_path.as_path(),
        args,
        None,
    );

    match result {
        Ok(ultragraph::agent_tools::ToolOutput::Json(v)) => ok_json(v.to_string()),
        // `run_tool` only returns Text when a render style was requested.
        Ok(ultragraph::agent_tools::ToolOutput::Text(t)) => ok_json(serde_json::json!({ "text": t }).to_string()),
        Err(e) if e.starts_with("Unknown agent tool") => err_json(StatusCode::NOT_FOUND, &e),
        Err(e) => err_json(StatusCode::BAD_REQUEST, &e),
    }
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
/// directory (mirrors `ug rm`) and drop it from the in-memory registry.
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
    let active_name = state.registry.active.read().expect("active poisoned").clone();

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
struct StalenessCache {
    computed_at: std::time::Instant,
    body: String,
}

/// One project's staleness row. Split out of the handler so the whole scan can
/// be handed to `spawn_blocking` as a single unit of plain sync work.
///
/// The scan itself lives in `project::staleness`, shared with `ug list` — the
/// CLI and the KB Manager must never disagree about whether a project is
/// stale, and two implementations of the same `stat` loop is how they would.
fn staleness_for_project(project_dir: &std::path::Path, meta: &crate::project::ProjectMeta) -> Option<serde_json::Value> {
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
async fn api_projects_staleness(State(state): State<ServeState>) -> Response {
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
struct IngestBody {
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
async fn api_ingest(State(state): State<ServeState>, Json(body): Json<IngestBody>) -> Response {
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
    cmd.arg("ingest").arg("-i").arg(&graph_path).arg("-o").arg(&db_path);
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
                j.error = Some(format!("ingest finished but reopening the store failed: {}", e));
            }
        }
    });

    ok_json(serde_json::json!({ "jobId": id, "project": project_name }).to_string())
}

#[derive(serde::Deserialize)]
struct BrowseDirQuery {
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
    for extra in std::env::var("UG_BROWSE_ROOTS").unwrap_or_default().split(':') {
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
    let confine = |p: &Path| {
        confine_to_browse_roots(p).map_err(|e| (e.status(), e.message().to_string()))
    };
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

async fn api_browse_dir(Query(params): Query<BrowseDirQuery>) -> Response {
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

/// Header carrying the body's size *before* `Content-Encoding` was applied.
///
/// `Content-Length` is the compressed size, and a client streaming the body
/// through `response.body.getReader()` counts **decoded** bytes — the browser
/// having already inflated them. Dividing one by the other makes a progress
/// bar that reaches 100% at roughly a tenth of the download and sits there.
/// There is no standard header for this, so the page reads ours.
const UNCOMPRESSED_LENGTH: &str = "x-uncompressed-length";

fn asset_response(asset: &EncodedAsset, headers: &HeaderMap) -> Response {
    // Only the encoding this client asked for is ever built; the other stays
    // uncomputed for the life of the process if nothing requests it.
    let (bytes, encoding) = match pick_encoding(headers) {
        Encoding::Brotli => (asset.brotli().clone(), Some("br")),
        Encoding::Gzip => (asset.gzip().clone(), Some("gzip")),
        Encoding::Identity => (asset.identity.clone(), None),
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type.clone())
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::VARY, "accept-encoding")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(UNCOMPRESSED_LENGTH, asset.identity.len());
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

async fn handle_indexed_tree(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    let ctx = state.active();
    let idx_path = ctx.graph_path.with_file_name("indexed-tree.json");
    match tokio::fs::read(&idx_path).await {
        Ok(data) => {
            let asset = EncodedAsset::new(data, "application/json; charset=utf-8");
            asset_response(&asset, &headers)
        }
        Err(e) => err_json(
            StatusCode::NOT_FOUND,
            &format!("indexed-tree.json not found at {}: {}", idx_path.display(), e),
        ),
    }
}

async fn handle_bundle(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.bundle, &headers)
}

async fn handle_cosmos_bundle(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.cosmos_bundle, &headers)
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
    ok_json(snap.stats.get_or_init(|| render_stats(&snap)).clone())
}

/// Build the `/api/graph/stats` body. Called once per snapshot — see the
/// `stats` field on [`GraphSnapshot`].
fn render_stats(snap: &GraphSnapshot) -> String {
    let mut node_types: BTreeMap<&'static str, usize> = BTreeMap::new();
    for n in &snap.parsed.nodes {
        *node_types.entry(n.node_type.as_str()).or_insert(0) += 1;
    }
    let mut edge_types: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in &snap.parsed.edges {
        *edge_types.entry(e.edge_type.as_str()).or_insert(0) += 1;
    }
    // The repo-root folder node carries the per-language file counts and the
    // code/docs/mixed classification the indexer computed.
    let root_folder = snap
        .parsed
        .nodes
        .iter()
        .filter_map(|n| n.folder.as_ref())
        .min_by_key(|f| f.depth);

    let body = serde_json::json!({
        "nodes": snap.parsed.nodes.len(),
        "edges": snap.parsed.edges.len(),
        "node_types": node_types,
        "edge_types": edge_types,
        "graph_bytes": snap.encoded.identity.len(),
        // Indexer-side counts (files, lines, timing). Present in graph.json
        // all along; surfaced here so the UI can show what was scanned, not
        // just what ended up in the graph.
        "index": snap.parsed.stats.as_ref().map(|s| serde_json::json!({
            "files": s.total_files,
            "cached_files": s.cached_files,
            "symbols": s.total_symbols,
            "folders": s.total_folders,
            "lines": s.total_lines,
            "indexed_at": s.last_indexed_at,
            "indexing_time_ms": s.indexing_time_ms,
            "repo_root": s.repo_root,
        })),
        "languages": root_folder.map(|f| f.language_breakdown.clone()),
        "kb_type": root_folder
            .and_then(|f| f.classification.as_ref())
            .map(|c| format!("{:?}", c).to_lowercase()),
    });
    body.to_string()
}

#[derive(serde::Deserialize)]
struct HydrateBody {
    /// Node indices into the slim index.
    ids: Vec<u32>,
}

/// `POST /api/graph/nodes/hydrate` — the heavy fields the slim index omits.
///
/// The slim index carries what the page needs *before* you click something:
/// id, name, type, file, lines, boundary flag. Everything else — docstring,
/// signature, metrics, imports, calls, extends, implements, the boundary
/// details — is per-node prose that only the info panel reads, and shipping it
/// for 162k nodes is most of what made `graph.json` 346 MB.
///
/// Batched because a selection asks about one node but a panel full of chips
/// can ask about dozens, and the singular `/api/graph/node/*id` is one request
/// each.
async fn api_hydrate(State(state): State<ServeState>, Json(body): Json<HydrateBody>) -> Response {
    let snap = state.snapshot();
    let n = snap.parsed.nodes.len();
    let nodes: Vec<&GraphNode> = body
        .ids
        .iter()
        .filter_map(|&i| snap.parsed.nodes.get(i as usize))
        .collect();
    match serde_json::to_string(&serde_json::json!({ "ids": body.ids, "nodes": nodes, "n": n, "token": snap.token() })) {
        Ok(s) => ok_json(s),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
    }
}

#[derive(serde::Deserialize)]
struct EdgesBody {
    /// Node **indices** into the slim index, not string ids. Position is
    /// identity in server mode, and a 141-character id per endpoint is what
    /// this whole protocol exists to avoid sending.
    ids: Vec<u32>,
    /// `"incident"` (default) — every edge touching each id, which is what
    /// makes that id's adjacency list *complete*. `"induced"` — only edges
    /// whose **both** endpoints are in `ids`, used to fill in the cross-links
    /// between nodes already on the canvas.
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /api/graph/edges` — the one primitive server mode is built on.
///
/// Solo expansion, the Related tab, focus/Tab stepping, the graph walk and the
/// tour all reduce to "give me the edges around these nodes", so they all come
/// through here. Answering it is O(sum of degree) rather than O(edges) because
/// `AdjIndex` holds edge indices — see the note on that struct.
///
/// The response is columnar and index-based for the same reason the slim index
/// is: a 1,500-node neighbourhood is roughly 14,000 edges, which is ~4 MB as id
/// strings and ~200 KB as integers.
async fn api_edges(State(state): State<ServeState>, Json(body): Json<EdgesBody>) -> Response {
    let snap = state.snapshot();
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));
    let induced = body.scope.as_deref() == Some("induced");

    let n = snap.parsed.nodes.len();
    let wanted: HashSet<u32> = body.ids.iter().copied().filter(|&i| (i as usize) < n).collect();

    // Collected by edge index and deduped: an edge incident to two requested
    // nodes would otherwise be sent twice, and the client stores edge objects
    // by identity.
    let mut edge_idx: Vec<u32> = Vec::new();
    for &i in &wanted {
        edge_idx.extend(adj.incident(i as usize));
    }
    edge_idx.sort_unstable();
    edge_idx.dedup();

    let mut rel_types: Vec<&'static str> = Vec::new();
    let mut rel_idx: HashMap<&'static str, u32> = HashMap::new();
    let (mut src, mut tgt, mut rel) = (Vec::new(), Vec::new(), Vec::new());

    for ei in edge_idx {
        let e = &snap.parsed.edges[ei as usize];
        let (Some(&si), Some(&ti)) = (adj.id_to_idx.get(&e.source), adj.id_to_idx.get(&e.target))
        else {
            continue;
        };
        let (si, ti) = (si as u32, ti as u32);
        if induced && !(wanted.contains(&si) && wanted.contains(&ti)) {
            continue;
        }
        let name = e.edge_type.as_str();
        let next = rel_types.len() as u32;
        let ri = *rel_idx.entry(name).or_insert_with(|| {
            rel_types.push(name);
            next
        });
        src.push(si);
        tgt.push(ti);
        rel.push(ri);
    }

    ok_json(
        serde_json::json!({
            "src": src,
            "tgt": tgt,
            "rel": rel,
            "relTypes": rel_types,
            // Which ids now have their *whole* edge list on the client. Only
            // an incident query can claim this: an induced query deliberately
            // withholds edges that leave the set, so marking its ids complete
            // would cache a half-answer as a whole one.
            "complete": if induced { Vec::new() } else { body.ids.clone() },
            "token": snap.token(),
        })
        .to_string(),
    )
}

/// `GET /api/graph/nodes` — the slim node index (see [`build_slim_index`]).
///
/// Encoded once per snapshot and served from the cache thereafter, the same
/// deal `/graph.json` gets. The build runs on a blocking thread: it walks every
/// node and every edge and serialises ~34 MB, which is not something to do on a
/// runtime worker.
async fn api_slim_index(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    let snap = state.snapshot();
    let built = tokio::task::spawn_blocking(move || {
        snap.slim
            .get_or_init(|| {
                EncodedAsset::new(
                    build_slim_index(&snap.parsed).into_bytes(),
                    "application/json; charset=utf-8",
                )
            })
            .clone()
    })
    .await;
    match built {
        Ok(asset) => asset_response(&asset, &headers),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("slim index task failed: {}", e),
        ),
    }
}

async fn api_node(State(state): State<ServeState>, AxPath(id): AxPath<String>) -> Response {
    let snap = state.snapshot();
    // Through the adjacency index's `id_to_idx`, not a linear scan. The map was
    // already being built for traverse/path; looking one node up by walking all
    // 162k of them was an O(V) answer to an O(1) question, and the node panel
    // fires one of these per selection.
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));
    match adj.id_to_idx.get(&id).map(|&i| &snap.parsed.nodes[i]) {
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
                // `eq_ignore_ascii_case` against the static name, rather than
                // allocating a lowercased `String` per node per request. The
                // filter list is already lowercased once by the caller.
                let nt = n.node_type.as_str();
                if !types.iter().any(|t| t.eq_ignore_ascii_case(nt)) {
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

async fn api_traverse(
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
        for &ei in &adj.out[idx] {
            let Some(&nb) = adj.id_to_idx.get(&snap.parsed.edges[ei as usize].target) else {
                continue;
            };
            if visited.insert(nb) {
                distances.insert(nb, d + 1);
                queue.push_back((nb, d + 1));
            }
        }
    }

    let nodes: Vec<&GraphNode> = visited.iter().map(|&i| &snap.parsed.nodes[i]).collect();
    // Induced edges: both endpoints inside the visited set. Reached through the
    // visited nodes' own incident lists rather than by filtering the whole edge
    // list, which is the difference between O(reached degree) and O(edges) —
    // the latter being three quarters of a million per request on a large repo.
    // Collected by index and sorted, so the response keeps `graph.json` order
    // rather than inheriting a HashSet's.
    let mut edge_idx: Vec<u32> = visited
        .iter()
        .flat_map(|&i| adj.incident(i))
        .filter(|&ei| {
            let e = &snap.parsed.edges[ei as usize];
            matches!(
                (adj.id_to_idx.get(&e.source), adj.id_to_idx.get(&e.target)),
                (Some(si), Some(ti)) if visited.contains(si) && visited.contains(ti)
            )
        })
        .collect();
    edge_idx.sort_unstable();
    edge_idx.dedup();
    let edges: Vec<&GraphEdge> = edge_idx
        .iter()
        .map(|&ei| &snap.parsed.edges[ei as usize])
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
        for &ei in &adj.out[cur] {
            let Some(&nb) = adj.id_to_idx.get(&snap.parsed.edges[ei as usize].target) else {
                continue;
            };
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
            let et = e.edge_type.as_str();
            lowered.iter().any(|t| t.eq_ignore_ascii_case(et))
        })
        .collect();

    let body = serde_json::json!({
        "count": matched.len(),
        "edges": matched,
    });
    ok_json(body.to_string())
}

/// Betweenness centrality is O(V·E) — seconds of pure CPU on a 15k-node graph.
/// Computing it inline would park a runtime worker for that whole time and
/// stall every other in-flight request, so it goes to `spawn_blocking`. The
/// `OnceLock` still makes it a once-per-snapshot cost; this only governs where
/// that one computation runs.
async fn api_centrality(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    match tokio::task::spawn_blocking(move || {
        snap.centrality
            .get_or_init(|| lib_centrality_graph(&snap.parsed))
            .clone()
    })
    .await
    {
        Ok(cached) => ok_json(cached),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("centrality task failed: {}", e),
        ),
    }
}

/// Same reasoning as [`api_centrality`] — cycle detection walks the whole graph
/// and must not run on a runtime thread.
async fn api_cycles(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    match tokio::task::spawn_blocking(move || {
        snap.cycles
            .get_or_init(|| lib_cycles_graph(&snap.parsed))
            .clone()
    })
    .await
    {
        Ok(cached) => ok_json(cached),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cycles task failed: {}", e),
        ),
    }
}

// ---------- Capabilities ----------

/// Surfaces enough state for the visualization UI to gate DB-dependent
/// panels (semantic / hybrid search) without having to make a probe
/// request per panel. `search_ready` is the single boolean the UI keys
/// off — it requires DB open, embedder configured, **and** at least one
/// node row in the table (an opened-but-empty DB still 200s on the
/// existing routes but returns nothing useful).
/// The `limits` block of `/api/capabilities`: every cap that shaped what
/// is in the store, plus the embedder's own token window.
///
/// The list comes from `ultragraph::limits`, which reads the enforcing
/// constants directly so the published numbers can't drift from the real
/// ones. Two entries are added here rather than there: the MCP snippet
/// preview, which is this binary's formatting concern and not the library's,
/// and the model token window, which depends on the embedder this server
/// happens to have open.
///
/// `embedder_token_window` is the honest headline. It binds *above* every
/// cap in the list — text past it is dropped by the tokenizer with no
/// truncation marker anywhere — and with the default 512-token model it
/// already sits below `document_page_text`. `null` means the active model
/// isn't one whose window we can state; see `limits::model_token_window`.
fn indexing_limits(state: &ServeState) -> serde_json::Value {
    let model = state
        .embedder
        .as_ref()
        .map(|e| e.config().model.clone())
        .unwrap_or_default();
    // The server has no `--section-cap` of its own — it reports the budget
    // the *model* implies. A store ingested under a pinned cap can differ;
    // the per-node `Storage metadata` is what shows what actually happened.
    let budget = ultragraph::limits::EmbedBudget::resolve(&model, None);

    let mut caps: Vec<serde_json::Value> = ultragraph::limits::all(&budget)
        .iter()
        .map(|l| serde_json::to_value(l).unwrap_or(serde_json::Value::Null))
        .collect();

    caps.push(serde_json::json!({
        "id": "mcp_snippet_preview",
        "label": "MCP snippet preview",
        "value": crate::mcp::format::SNIPPET_PREVIEW_CHARS,
        "unit": "chars",
        "stage": "retrieve",
        "extensions": [],
        "effect": "How much of each snippet an MCP search prints inline. Truncation \
                   is marked, and the full slice is one get_code call away.",
        "source": "mcp/format.rs:SNIPPET_PREVIEW_CHARS",
    }));

    serde_json::json!({
        "embedder_model": if model.is_empty() { serde_json::Value::Null } else { model.clone().into() },
        "embedder_token_window": budget.window_tokens,
        // How the description budget was arrived at: "auto" from the
        // model's window, or "default" when that window is unknown. The
        // server never sees a --section-cap, so it never reports "flag".
        "budget_source": budget.source,
        "advisory": budget.advisory(&model),
        "related_advisory": budget.related_advisory(),
        "caps": caps,
        "docs": "docs/INDEXING-AND-CHUNKING.md",
    })
}

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

    let chat_default = state.chat_default.read().expect("chat_default poisoned").clone();
    let chat_ready = chat_default.is_some() && search_ready;
    let chat_info = chat_default.map(|c| {
        serde_json::json!({
            "model": c.model,
            "base_url": c.base_url,
        })
    });

    // How this project's graph reaches the browser. The page reads `mode` to
    // decide between downloading `graph.json` and asking the server, so this
    // block is what switches the two. Its *absence* means local — which is
    // exactly what a static host answers, and why the published demo needs no
    // shim change to keep working.
    let snap = state.snapshot();
    let graph_bytes = snap.encoded.identity.len();
    let graph_info = serde_json::json!({
        "mode": state.graph_mode.resolve(graph_bytes),
        "bytes": graph_bytes,
        "nodes": snap.parsed.nodes.len(),
        "edges": snap.parsed.edges.len(),
        "threshold": GRAPH_SERVER_MODE_BYTES,
        // Which snapshot the page is talking to. Server mode splits one graph
        // across many requests, so a `ug gen` landing mid-session would mix a
        // slim index from the old graph with edges from the new one — node
        // indices are positional, so that is not a stale answer but a
        // scrambled one. Every server-mode response carries this; a mismatch
        // means "reload", which is a thing the user can act on.
        "token": snap.token(),
    });

    let body = serde_json::json!({
        "db_ready": db_ready,
        "embedder_ready": embedder_ready,
        "graph": graph_info,
        // What the indexer had to leave out. Published because these caps
        // decide what a node's vector can match on at all, and a user who
        // doesn't know them reads a truncated chunk as a search failure.
        // See `ultragraph::limits` and docs/INDEXING-AND-CHUNKING.md.
        "limits": indexing_limits(&state),
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

/// Cap on how much source a single preview may return, whichever copy it
/// comes from. A whole-file node's captured code can be megabytes; the panel
/// is a transparency view, not a file viewer.
const FILE_PREVIEW_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Reads a source file (or a line slice of one) so the UI's Preview tab can
/// show real content.
///
/// Prefers the live file from the repo — that is the tab's promise, "the file
/// as it is now" — but falls back to the source the store captured at index
/// time when the repo path is unavailable or the file has moved/deleted since
/// indexing. `node_text` in the DB is a synthetic embedding string, not the
/// source, which is why this route exists at all. The response's `source`
/// field tells the UI which copy it got (`filesystem` vs `db`).
async fn api_file(State(state): State<ServeState>, Query(params): Query<FileQuery>) -> Response {
    let rel = ultragraph::agent_tools::strip_file_id_prefix(params.path.trim());
    if rel.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "missing path");
    }

    if let Some(resp) = file_from_disk(&state, rel, &params).await {
        return resp;
    }

    if let Some(resp) = file_from_store(&state, rel, &params).await {
        return resp;
    }

    err_json(
        StatusCode::NOT_FOUND,
        &format!(
            "{} not found in the repo ({}) or the index",
            rel,
            state.repo_root().display()
        ),
    )
}

/// Read `rel` live from the repo root, if the repo is present and the file
/// exists and is UTF-8 text.
///
/// Returns `None` (not an error) when any of those fails, so the caller can
/// fall back to the indexed copy. A repo root that no longer exists is the
/// whole point of that fallback, so it is checked before any resolution.
async fn file_from_disk(state: &ServeState, rel: &str, params: &FileQuery) -> Option<Response> {
    let repo_root = state.repo_root();
    let root = repo_root.as_path();
    if !root.exists() {
        return None; // repo moved or deleted — nothing to read live
    }

    // Resolve against the repo root and canonicalize, then verify the result
    // is still inside the root — blocks `../` traversal and absolute-path
    // escapes. Only live reads can escape (the store only ever contains
    // indexed paths), so the check lives here rather than on the fallback.
    //
    // Both sides must be canonical for `starts_with` to mean anything: a root
    // reached through a symlink (`/tmp` → `/private/tmp` on macOS) compares
    // unequal to the resolved file path and would reject every legitimate
    // read. Fall back to the raw root if it can't be resolved — a
    // non-existent root is caught above, and comparing against the
    // unresolved form is stricter, never looser.
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon = std::fs::canonicalize(root.join(rel)).ok()?;
    if !canon.starts_with(&canon_root) {
        return Some(err_json(StatusCode::FORBIDDEN, "path escapes repo root"));
    }

    if let Ok(meta) = std::fs::metadata(&canon) {
        if meta.len() > FILE_PREVIEW_MAX_BYTES {
            return Some(err_json(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file too large to preview",
            ));
        }
    }

    // Non-UTF-8 files (binaries, PDFs) have no text to preview; fall through
    // to the store, which may still hold a captured textual span.
    let text = tokio::fs::read_to_string(&canon).await.ok()?;

    let (content, sliced, total_lines) = slice_file_text(text, params.start, params.end);

    Some(ok_json(
        serde_json::json!({
            "path": rel,
            "content": content,
            "start_line": params.start,
            "end_line": params.end,
            "total_lines": total_lines,
            "sliced": sliced,
            "source": "filesystem",
        })
        .to_string(),
    ))
}

/// Cut a 1-based inclusive line range out of a whole file, clamped to its
/// bounds. Returns `(content, sliced, total_lines)`, where `total_lines` is
/// always the *file's* length so the UI can place the slice.
///
/// Shared by the live read and the indexed fallback: a Preview request must
/// return the same lines whether or not the repo is on this machine, and
/// two copies of this arithmetic is how that stops being true.
fn slice_file_text(text: String, start: Option<usize>, end: Option<usize>) -> (String, bool, usize) {
    let all: Vec<&str> = text.lines().collect();
    let total_lines = all.len();
    match start {
        Some(s) if s >= 1 => {
            let lo = s - 1;
            let hi = end.unwrap_or(s).max(s).min(total_lines);
            let body = if lo >= total_lines {
                String::new()
            } else {
                all[lo..hi].join("\n")
            };
            (body, true, total_lines)
        }
        _ => (text, false, total_lines),
    }
}

/// Serve the source the store captured for this file at index time.
///
/// `None` when the store is unavailable or no matching node has captured
/// code. The matching itself lives in [`stored_source_for_file`] so the
/// fallback is testable without a running server.
async fn file_from_store(state: &ServeState, rel: &str, params: &FileQuery) -> Option<Response> {
    let store = pick_store(state, None).ok()?;
    let snap = state.snapshot();
    let (code, sliced) = stored_source_for_file(
        &snap.parsed,
        store.as_ref(),
        rel,
        params.start,
        params.end,
    )
    .await?;
    if code.len() as u64 > FILE_PREVIEW_MAX_BYTES {
        return Some(err_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "indexed file too large to preview",
        ));
    }
    // An exact-span match already *is* the requested lines; a whole-file
    // capture still has to be cut down to them, or a range request would
    // silently return the entire file whenever the repo is missing.
    let (content, sliced, total_lines) = if sliced {
        let n = code.lines().count();
        (code, true, n)
    } else {
        slice_file_text(code, params.start, params.end)
    };
    Some(ok_json(
        serde_json::json!({
            "path": rel,
            "content": content,
            "start_line": params.start,
            "end_line": params.end,
            "total_lines": total_lines,
            "sliced": sliced,
            "source": "db",
        })
        .to_string(),
    ))
}

/// Resolve `rel` (+ optional line range) to the source the store captured at
/// index time, with no filesystem involved.
///
/// Uses the in-memory graph to map `rel` + line range to a node id, then
/// reads that node's stored code. A span request matches the exact node
/// first, then falls back to the file's whole-capture node; a whole-file
/// request goes straight to that. Returns `(code, sliced)` — `sliced` says
/// whether `code` is already narrowed to the requested span, which is false
/// for the whole-file node and tells [`file_from_store`] it still has to cut
/// the range out. `None` when no matching node has captured code.
async fn stored_source_for_file(
    graph: &GraphData,
    store: &dyn KnowledgeStore,
    rel: &str,
    start: Option<usize>,
    end: Option<usize>,
) -> Option<(String, bool)> {
    // Whole-file nodes (File/Config) carry no line range; symbol nodes carry
    // their exact span.
    let whole_file = |n: &GraphNode| n.file.as_deref() == Some(rel) && n.start_line.is_none();
    let exact_span = |n: &GraphNode| {
        n.file.as_deref() == Some(rel)
            && match start {
                Some(s) => {
                    n.start_line == Some(s as u32)
                        && end.map(|e| n.end_line == Some(e as u32)).unwrap_or(true)
                }
                None => false,
            }
    };

    let mut candidates: Vec<&GraphNode> =
        graph.nodes.iter().filter(|n| exact_span(n)).collect();
    candidates.extend(graph.nodes.iter().filter(|n| whole_file(n)));

    for node in candidates {
        let Ok(Some(row)) = store.fetch_node(&node.id).await else {
            continue;
        };
        if row.code.is_empty() {
            continue;
        }
        // A span request that fell back to the whole-file node reports the
        // whole file (`sliced: false`), matching what the live read would do
        // if it had the file.
        let sliced = node.start_line.is_some() && start.is_some();
        return Some((row.code, sliced));
    }

    None
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
        Ok(Some(n)) => {
            let mut v = node_row_to_json(&n);
            let stats = db.sparse_stats();
            v["storage"] =
                node_storage_meta(&n, state.repo_root().as_path(), stats.as_deref());
            ok_json(v.to_string())
        }
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

/// Pull a default `ChatConfig` from CLI args, env vars, or the
/// persisted `~/.ug/config.json` (`ug config set chat.*`). Returns
/// `None` when no chat model is configured — the route then 503s with
/// a clear message rather than hitting a misconfigured endpoint.
///
/// Env-var fallbacks let `ug serve` be wrapped by `docker run -e
/// UG_CHAT_*` without rewriting the entrypoint.
fn build_chat_default_from_args(args: &[String]) -> Option<ChatConfig> {
    let (model, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--chat-model"]), "chat.model");
    // Chat borrows the embeddings endpoint/key when no chat-specific one
    // is given (the common single-host case): the chat.* chain resolves
    // first, then the embed.* chain (env/config only — its flags were
    // already folded in above).
    let chat_base_flag =
        flag_value(args, &["--chat-base-url"]).or_else(|| flag_value(args, &["--base-url"]));
    let base_url = crate::config::resolve_pref_cfg(chat_base_flag, "chat.base_url")
        .0
        .or_else(|| crate::config::resolve_pref_cfg(None, "embed.base_url").0);
    let chat_api_flag =
        flag_value(args, &["--chat-api-key"]).or_else(|| flag_value(args, &["--api-key"]));
    let api_key = crate::config::resolve_pref_cfg(chat_api_flag, "chat.api_key")
        .0
        .or_else(|| crate::config::resolve_pref_cfg(None, "embed.api_key").0);
    let (temp_raw, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--temperature"]), "chat.temperature");
    let temperature = temp_raw.and_then(|s| s.parse().ok());
    let (max_tok_raw, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--max-tokens"]), "chat.max_tokens");
    let max_tokens = max_tok_raw.and_then(|s| s.parse().ok());
    let (timeout_raw, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--chat-timeout"]), "chat.timeout_secs");
    let timeout = timeout_raw.and_then(|s| s.parse().ok());

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

// ---------- Settings (/api/config) ----------

/// JSON view of every persistable config key for the settings UI:
/// saved value, effective value after flag precedence, and which
/// tier won. Secrets are masked — a raw API key never leaves the
/// server, only a short prefix for recognition.
fn config_payload(state: &ServeState) -> serde_json::Value {
    let args: &[String] = &state.serve_args;
    let keys: Vec<serde_json::Value> = crate::config::CONFIG_KEYS
        .iter()
        .map(|key| {
            let saved = crate::config::get(key.name);
            let flag_val = flag_value(args, &[key.flag]);
            let flag_active = flag_val.is_some();
            let (effective, source) = crate::config::resolve_pref_cfg(flag_val, key.name);
            let source_label = match source {
                crate::cli::embed::PrefSource::Flag => "flag",
                crate::cli::embed::PrefSource::Config(_) => "config",
                crate::cli::embed::PrefSource::Default => "default",
            };
            let mask = |v: &String| {
                if key.secret {
                    crate::config::display_value(key, v)
                } else {
                    v.clone()
                }
            };
            serde_json::json!({
                "name": key.name,
                "section": key.section,
                "desc": key.desc,
                "kind": match key.kind {
                    crate::config::Kind::Str => "str",
                    crate::config::Kind::F32 => "f32",
                    crate::config::Kind::U32 => "u32",
                    crate::config::Kind::U64 => "u64",
                },
                "secret": key.secret,
                "saved": saved.as_ref().map(&mask),
                "effective": effective.as_ref().map(&mask),
                "source": source_label,
                "flag": key.flag,
                "flag_active": flag_active,
                "default": crate::config::default_for(key),
            })
        })
        .collect();
    serde_json::json!({
        "path": crate::config::config_path().display().to_string(),
        "keys": keys,
        // Chat settings apply immediately (chat_default is rebuilt on
        // save); the embedder is constructed at startup, so embed.*
        // changes need a server restart to take effect here.
        "live_sections": ["chat"],
    })
}

async fn api_config_get(State(state): State<ServeState>) -> Response {
    ok_json(config_payload(&state).to_string())
}

#[derive(serde::Deserialize)]
struct ConfigPostBody {
    /// key → new value. Strings and numbers accepted; a blank string
    /// clears the key (same as listing it in `unset`).
    #[serde(default)]
    set: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    unset: Vec<String>,
}

/// Persist settings changes from the UI into `~/.ug/config.json`, then
/// refresh the in-process view so chat picks them up immediately.
/// Validation failures reject the whole request — the file is only
/// written when every change parses.
async fn api_config_post(
    State(state): State<ServeState>,
    Json(body): Json<ConfigPostBody>,
) -> Response {
    let path = crate::config::config_path();
    let mut cfg = match crate::config::read_config_file(&path) {
        Ok(c) => c,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    for (name, val) in &body.set {
        let Some(key) = crate::config::find_key(name) else {
            return err_json(StatusCode::BAD_REQUEST, &format!("unknown config key: {}", name));
        };
        let raw = match val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    &format!("{} expects a string or number value", key.name),
                )
            }
        };
        if raw.trim().is_empty() {
            crate::config::value_unset(&mut cfg, key);
            continue;
        }
        if let Err(e) = crate::config::value_set(&mut cfg, key, raw.trim()) {
            return err_json(StatusCode::BAD_REQUEST, &e);
        }
    }
    for name in &body.unset {
        let Some(key) = crate::config::find_key(name) else {
            return err_json(StatusCode::BAD_REQUEST, &format!("unknown config key: {}", name));
        };
        crate::config::value_unset(&mut cfg, key);
    }
    if let Err(e) = crate::config::write_config_file(&path, &cfg) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    crate::config::reload();

    let new_default = build_chat_default_from_args(&state.serve_args);
    match new_default.as_ref() {
        Some(c) => tracing::info!(model = %c.model, base_url = %c.base_url, "chat config updated via /api/config"),
        None => tracing::info!("chat config cleared via /api/config (/api/chat will return 503)"),
    }
    *state.chat_default.write().expect("chat_default poisoned") = new_default;

    ok_json(config_payload(&state).to_string())
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
    /// `true` → respond as an SSE stream (`context` / `delta` / `done`
    /// / `error` events) instead of one JSON body. Providers that
    /// reject streaming still work: the server falls back to a plain
    /// completion and emits it as a single delta.
    #[serde(default)]
    stream: Option<bool>,
    /// Give the model the graph toolbox (search, outlines, call sites, …).
    /// On by default: a grounded answer beats a fluent one.
    #[serde(default)]
    tools: Option<bool>,
    /// Cap on tool-calling rounds before the model must answer.
    #[serde(default)]
    max_tool_rounds: Option<usize>,
    /// Let a reasoning model deliberate before answering. Off by default —
    /// the answer is grounded in retrieved context, and thinking is where a
    /// local model spends its minutes.
    #[serde(default)]
    think: Option<bool>,
}

/// Citation list shared by the JSON and SSE chat responses.
fn citations_json(items: &[ultragraph::storage::ContextItem]) -> Vec<serde_json::Value> {
    items
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
        .collect()
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
    let chat_default = state.chat_default.read().expect("chat_default poisoned").clone();
    let chat_cfg = match merge_chat_cfg(&chat_default, &body) {
        Ok(c) => c,
        Err(ChatCfgError::NotConfigured) => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "chat not configured (start serve with --chat-model or pass `chat_model` in the request body)",
            )
        }
        Err(ChatCfgError::Invalid(msg)) => return err_json(StatusCode::BAD_REQUEST, &msg),
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

    if body.stream.unwrap_or(false) {
        return api_chat_stream(state, body, db, embedder, chat_client);
    }

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
    let max_context_chars = body.max_context_chars.unwrap_or(DEFAULT_CONTEXT_CHARS).min(64_000);
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
    opts.fast = !body.think.unwrap_or(false);

    let dest_name = db.backend_name();
    let repo_root = state.repo_root();

    // The same toolbox the streaming path builds: `stream` picks how the
    // answer is delivered, not whether the model may consult the graph.
    let tool_state = state.clone();
    let tool_db = db.clone();
    let tool_embedder = Some(embedder.clone());
    let runner = move |name: &str, args: serde_json::Value| {
        let state = tool_state.clone();
        let db = tool_db.clone();
        let embedder = tool_embedder.clone();
        let name = name.to_string();
        Box::pin(async move { run_chat_tool(state, db, embedder, name, args).await })
            as futures::future::BoxFuture<'static, Result<String, String>>
    };
    let toolbox = body.tools.unwrap_or(true).then(|| chat::ToolBox {
        schemas: crate::mcp::tools::openai_tool_schemas(),
        run: &runner,
        max_rounds: body.max_tool_rounds.unwrap_or(4).min(8),
        max_result_chars: 6_000,
    });

    let outcome = chat::run_chat_rag(
        &*db,
        &embedder,
        &chat_client,
        repo_root.as_path(),
        &body.query,
        &history_owned,
        opts,
        toolbox.as_ref(),
    )
    .await;
    drop(_permit);

    match outcome {
        Ok(o) => {
            let citations = citations_json(&o.context.items);
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

/// SSE variant of `/api/chat` (`"stream": true` in the body). Event
/// sequence the UI consumes:
///
/// ```text
/// event: context   data: {"citations":[…],"seed_id":…,"retrieval_ms":…}
/// event: delta     data: {"content":"…"} | {"reasoning":"…"}
/// event: done      data: {"answer":…,"usage":…,"completion_ms":…,…}
/// event: error     data: {"error":"…"}      (terminal, replaces done)
/// ```
///
/// The RAG turn runs in a spawned task feeding an unbounded channel, so
/// the response starts (and heartbeats) immediately while retrieval is
/// still working.
fn api_chat_stream(
    state: ServeState,
    body: ChatBody,
    db: Arc<dyn KnowledgeStore>,
    embedder: Arc<Embedder>,
    chat_client: ChatClient,
) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use futures::StreamExt;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    let repo_root = state.repo_root();
    let embed_lock = state.embed_lock.clone();

    tokio::spawn(async move {
        let dest_name = db.backend_name();
        let model = chat_client.config().model.clone();
        let endpoint = chat_client.config().base_url.clone();
        let emit = |name: &'static str, payload: serde_json::Value| {
            let _ = tx.send(SseEvent::default().event(name).data(payload.to_string()));
        };

        let _permit = match embed_lock.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                emit("error", serde_json::json!({ "error": "embed semaphore closed" }));
                return;
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
        let max_context_chars = body
            .max_context_chars
            .unwrap_or(DEFAULT_CONTEXT_CHARS)
            .min(64_000);
        let edge_types_owned: Option<Vec<String>> = body.edge_types.filter(|v| !v.is_empty());
        let history_owned: Vec<ChatMessage> = body.history.unwrap_or_default();

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
        opts.fast = !body.think.unwrap_or(false);

        emit("phase", serde_json::json!({ "phase": "retrieving" }));

        // Hand the model the graph toolbox so it can chase what retrieval
        // only pointed at — call sites, outlines, exact source, paths.
        let tool_state = state.clone();
        let tool_db = db.clone();
        let tool_embedder = Some(embedder.clone());
        let runner = move |name: &str, args: serde_json::Value| {
            let state = tool_state.clone();
            let db = tool_db.clone();
            let embedder = tool_embedder.clone();
            let name = name.to_string();
            Box::pin(async move { run_chat_tool(state, db, embedder, name, args).await })
                as futures::future::BoxFuture<'static, Result<String, String>>
        };
        let toolbox = if body.tools.unwrap_or(true) {
            Some(chat::ToolBox {
                schemas: crate::mcp::tools::openai_tool_schemas(),
                run: &runner,
                max_rounds: body.max_tool_rounds.unwrap_or(4).min(8),
                max_result_chars: 6_000,
            })
        } else {
            None
        };

        let t_ret = std::time::Instant::now();
        let emit_ctx = emit.clone();
        let emit_tool = emit.clone();
        let emit_delta = emit.clone();
        let outcome = chat::run_chat_rag_stream(
            &*db,
            &embedder,
            &chat_client,
            repo_root.as_path(),
            &body.query,
            &history_owned,
            opts,
            toolbox.as_ref(),
            |ctx| {
                emit_ctx(
                    "context",
                    serde_json::json!({
                        "citations": citations_json(&ctx.items),
                        "seed_id": ctx.seed_id,
                        "retrieval_ms": t_ret.elapsed().as_millis() as u64,
                    }),
                );
            },
            |t: chat::ToolEvent| {
                emit_tool(
                    "tool",
                    serde_json::json!({
                        "name": t.name,
                        "args": t.args,
                        "args_json": t.args_json,
                        "state": if t.summary.is_some() { "done" } else { "start" },
                        "summary": t.summary,
                        "result": t.result,
                    }),
                );
            },
            |d| {
                let mut obj = serde_json::Map::new();
                if let Some(c) = d.content {
                    obj.insert("content".into(), serde_json::Value::String(c));
                }
                if let Some(r) = d.reasoning {
                    obj.insert("reasoning".into(), serde_json::Value::String(r));
                }
                if !obj.is_empty() {
                    emit_delta("delta", serde_json::Value::Object(obj));
                }
            },
        )
        .await;

        match outcome {
            Ok(o) => emit(
                "done",
                serde_json::json!({
                    "answer": o.answer,
                    "reasoning": if o.reasoning.is_empty() { None } else { Some(o.reasoning) },
                    "retrieval_ms": o.retrieval_ms,
                    "completion_ms": o.completion_ms,
                    "tool_calls": o.tool_calls,
                    "usage": o.usage,
                    "dest": dest_name,
                    "chat_model": model,
                }),
            ),
            Err(e) => {
                // Distinguish "your endpoint is down" from "the model erred":
                // only one of them is something the user can fix, and the UI
                // offers the fix when we say which it is.
                let unreachable = e
                    .downcast_ref::<chat::ChatError>()
                    .map(|c| c.is_unreachable())
                    .unwrap_or(false);
                emit(
                    "error",
                    serde_json::json!({
                        "error": format!("chat: {}", e),
                        "kind": if unreachable { "llm_unreachable" } else { "chat_failed" },
                        "endpoint": endpoint,
                    }),
                );
            }
        }
    });

    let stream =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// Why a request couldn't be turned into a usable `ChatConfig`.
///
/// Two different failures with two different status codes: "nothing is
/// configured anywhere" is the server's problem (503), while "your
/// override is not something we'll send" is the caller's (400).
#[derive(Debug)]
pub(crate) enum ChatCfgError {
    /// No model from flags, config, or the request body.
    NotConfigured,
    /// The request's own endpoint override was rejected.
    Invalid(String),
}

/// Hosts a request-supplied chat endpoint may never point at.
///
/// Cloud instance-metadata services hand out live credentials to anything
/// that can issue a plain HTTP request from inside the network, so they are
/// the one SSRF target worth naming explicitly. Everything else — including
/// loopback and LAN addresses — stays reachable on purpose: pointing chat at
/// a local Ollama or an on-prem vLLM is the feature.
const BLOCKED_CHAT_HOSTS: &[&str] = &[
    "169.254.169.254",
    "fd00:ec2::254",
    "metadata.google.internal",
    "metadata",
];

/// Scheme + host + port of a chat endpoint, lowercased, for comparing a
/// request's override against the configured default. `None` when the URL
/// doesn't parse or names no host.
fn chat_origin(raw: &str) -> Option<(String, String, Option<u16>)> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?.trim_matches(['[', ']']).to_ascii_lowercase();
    Some((
        url.scheme().to_ascii_lowercase(),
        host,
        url.port_or_known_default(),
    ))
}

/// Decide the endpoint and credential a chat/tour request may actually use.
///
/// The rule that matters: **a request-supplied `base_url` never inherits the
/// server's stored API key**. `ug serve` has no authentication, so anything
/// that can reach the port — another local process, or a browser page that
/// got there via DNS rebinding — could otherwise post one JSON body naming
/// its own endpoint and have the server deliver the user's real API key to
/// it in an `Authorization` header. Overriding the endpoint is still allowed
/// (flipping models mid-session is the point); it just has to bring its own
/// key, or go keyless for a local server that wants none.
///
/// An override pointing at the *same* origin as the configured default is not
/// a redirection at all — that one keeps the stored key, so a UI that echoes
/// the current `base_url` back in the body keeps working.
fn resolve_chat_endpoint(
    default: &ChatConfig,
    override_url: Option<&str>,
    override_key: Option<&str>,
) -> Result<(String, String), ChatCfgError> {
    let stored_key = || default.api_key.clone();
    let Some(raw) = override_url.map(str::trim).filter(|s| !s.is_empty()) else {
        // No endpoint override: the stored key is going where it always goes.
        return Ok((
            default.base_url.clone(),
            override_key.map(str::to_string).unwrap_or_else(stored_key),
        ));
    };

    let Some((scheme, host, port)) = chat_origin(raw) else {
        return Err(ChatCfgError::Invalid(format!(
            "chat_base_url is not a valid absolute URL: {raw}"
        )));
    };
    if scheme != "http" && scheme != "https" {
        return Err(ChatCfgError::Invalid(format!(
            "chat_base_url must be http or https, got {scheme}"
        )));
    }
    if BLOCKED_CHAT_HOSTS.contains(&host.as_str()) {
        return Err(ChatCfgError::Invalid(format!(
            "chat_base_url host {host} is not allowed"
        )));
    }

    let same_origin = chat_origin(&default.base_url)
        .is_some_and(|d| d == (scheme.clone(), host.clone(), port));
    let key = match override_key {
        Some(k) => k.to_string(),
        None if same_origin => stored_key(),
        None => {
            // The interesting case: redirected endpoint, no key of its own.
            // Send nothing rather than the stored secret.
            tracing::warn!(
                host = %host,
                "chat_base_url override points at a different origin than the configured \
                 endpoint — sending the request without the stored API key"
            );
            String::new()
        }
    };
    Ok((raw.to_string(), key))
}

/// Combine a default `ChatConfig` (from CLI/env at startup) with
/// per-request overrides. Errors when neither side provides a model, or
/// when the request's endpoint override is rejected by
/// [`resolve_chat_endpoint`].
fn merge_chat_cfg(
    default: &Option<ChatConfig>,
    body: &ChatBody,
) -> Result<ChatConfig, ChatCfgError> {
    let base_default = default.clone().unwrap_or_default();
    let model = body
        .chat_model
        .clone()
        .or_else(|| default.as_ref().map(|c| c.model.clone()))
        .ok_or(ChatCfgError::NotConfigured)?;
    let (base_url, api_key) = resolve_chat_endpoint(
        &base_default,
        body.chat_base_url.as_deref(),
        body.chat_api_key.as_deref(),
    )?;
    let temperature = body.temperature.unwrap_or(base_default.temperature);
    let max_tokens = body.max_tokens.unwrap_or(base_default.max_tokens);
    Ok(ChatConfig {
        extra_body: None,
        base_url,
        api_key,
        model,
        temperature,
        max_tokens,
        timeout_secs: base_default.timeout_secs,
    })
}

/// `GET /api/chat/config` — what the chat turn is actually made of.
///
/// The answer a model gives depends entirely on three things the UI
/// otherwise hides: the system prompt it was given, the tools it could
/// call, and how the context was retrieved. "Semantic search" is the
/// usual guess for the last one and it's wrong — so publish all three
/// rather than making people read the source to trust the output.
async fn api_chat_config(State(state): State<ServeState>) -> Response {
    use serde_json::json;

    let stores = state.stores();
    let primary = stores.as_ref().and_then(|s| s.get(&s.primary).cloned());
    let native_ppr = primary.as_ref().map(|p| p.supports_native_ppr());
    let backend = primary.as_ref().map(|p| p.backend_name());

    // PPR is the default; a backend without it silently ranks with MMR
    // instead, which changes the results enough to be worth naming.
    let effective = match native_ppr {
        Some(false) => "mmr",
        _ => "ppr",
    };
    let ranking = if effective == "ppr" {
        json!({
            "id": "ppr",
            "label": "Personalized PageRank over the graph",
            "detail": "The fused hits seed a PageRank run across the edge graph, so nodes that neighbour several good hits outrank a single lucky match.",
        })
    } else {
        json!({
            "id": "mmr",
            "label": "Maximal Marginal Relevance rerank",
            "detail": "This backend has no native PageRank, so results are reranked for relevance-vs-diversity instead of expanded through the graph.",
        })
    };

    let tools: Vec<serde_json::Value> = crate::mcp::tools::openai_tool_schemas()
        .into_iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name").cloned().unwrap_or_default(),
                "description": f.get("description").cloned().unwrap_or_default(),
                "parameters": f.get("parameters").cloned().unwrap_or_default(),
            }))
        })
        .collect();

    let body = json!({
        "system_prompt": chat::DEFAULT_SYSTEM_PROMPT,
        "tool_suffix": chat::TOOL_SYSTEM_SUFFIX,
        "tools_enabled_by_default": true,
        "tools": tools,
        "retrieval": {
            "summary": "Hybrid search (dense + keyword) seeds a graph ranking — not semantic-only.",
            "backend": backend,
            "strategy": effective,
            "stages": [
                {
                    "id": "hybrid",
                    "label": "Hybrid seed search — dense + keyword, fused with RRF",
                    "detail": "Your question is embedded and searched by vector similarity, and separately matched as keywords; the two rankings are merged by Reciprocal Rank Fusion so a hit either side can surface.",
                },
                ranking,
                {
                    "id": "budget",
                    "label": "Char-budgeted context pack",
                    "detail": "Top nodes are hydrated with their descriptions and source snippets, then trimmed to the context budget and numbered [#1], [#2] … for citation.",
                },
            ],
            "defaults": {
                "k": 8,
                "hops": 2,
                "direction": "both",
                "include_snippets": true,
                "max_context_chars": DEFAULT_CONTEXT_CHARS,
            },
        },
    });
    ok_json(body.to_string())
}

// ---------- Agent tools for chat & tour (`ToolBox`) ----------

/// Tools a chat/tour model may call, in OpenAI function-calling form.
///
/// The schemas come straight from the MCP registry, so the model behind
/// `/api/chat` sees exactly the toolbox an MCP client sees — one
/// description to maintain, not two. Tools that mutate or that only make
/// sense to an operator (`gen`, `list_projects`) are left out: a chat
/// turn should read the graph, not reshape it.
/// Run one tool against the server's live state.
///
/// Graph tools go through the same `agent_tools::run_tool` the MCP server
/// and `/api/tools/:tool` use; the two search tools need the vector store
/// and embedder, so they're wired here to the already-open handles rather
/// than opening their own.
async fn run_chat_tool(
    state: ServeState,
    db: Arc<dyn KnowledgeStore>,
    embedder: Option<Arc<Embedder>>,
    name: String,
    args: serde_json::Value,
) -> Result<String, String> {
    // Undo the model's stringified arrays/numbers before anything reads them.
    let mut args = args;
    crate::mcp::tools::normalize_args(&name, &mut args);
    if crate::mcp::tools::CHAT_TOOL_DENYLIST.contains(&name.as_str()) {
        return Err(format!("{} is not available from chat", name));
    }

    match name.as_str() {
        "search" | "semantic_search" => {
            chat::run_search_tool(&name, &args, &*db, embedder.as_deref(), state.repo_root().as_path())
                .await
        }
        // Statistics come from the store's indexed properties, not the graph —
        // the one advertised tool `agent_tools::run_tool` cannot answer.
        "analyze" => crate::mcp::run_analyze_json(&*db, &args).await,
        _ => {
            crate::mcp::tools::reject_if_store_backed(&name)?;
            let snap = state.snapshot();
            let ctx = state.active();
            // The chat path already holds an open store, so the source
            // pre-fetch costs one lookup rather than another open.
            let indexed = ultragraph::agent_tools::IndexedSource::load(
                &*db,
                &ultragraph::agent_tools::source_node_ids(&name, &snap.parsed, &args),
            )
            .await;
            let out = ultragraph::agent_tools::run_tool(
                &name,
                &snap.parsed,
                snap.raw_json(),
                ultragraph::agent_tools::SourceCtx::new(&indexed, ctx.repo_root.as_path()),
                ctx.graph_path.as_path(),
                args,
                Some(ultragraph::agent_tools::Render::Markdown),
            )?;
            Ok(match out {
                ultragraph::agent_tools::ToolOutput::Text(t) => t,
                ultragraph::agent_tools::ToolOutput::Json(v) => {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                }
            })
        }
    }
}

// ---------- Guided tour (/api/tour) ----------

#[derive(serde::Deserialize)]
struct TourBody {
    query: String,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    hops: Option<u32>,
    #[serde(default)]
    max_stops: Option<usize>,
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
    /// Cap on how many candidates may come from one file (0 = no cap).
    /// Keeps a single large file from swallowing the whole itinerary.
    #[serde(default)]
    max_per_file: Option<usize>,
    /// Attach the planning transcript (prompts + raw model reply + parsed
    /// plan) to the response. On by default so the UI can show the user
    /// the JSON the guide actually produced.
    #[serde(default)]
    include_debug: Option<bool>,
    #[serde(default, rename = "where")]
    where_clause: Option<String>,
    /// Skip the LLM guide and return a ranked itinerary from retrieval
    /// only. The route also degrades to this automatically when no chat
    /// model is configured, so a tour always works with just the DB.
    #[serde(default)]
    no_llm: Option<bool>,
    /// Stream planning progress as SSE instead of blocking until the tour
    /// is ready. Planning against a local model runs for minutes, so the
    /// UI wants a running account rather than a spinner.
    #[serde(default)]
    stream: Option<bool>,
    /// Let a reasoning model deliberate before planning. Off by default —
    /// thinking is where a local model spends its minutes, and a tour is a
    /// structured extraction, not a reasoning problem.
    #[serde(default)]
    think: Option<bool>,
    /// Let the guide research with the graph tools before routing.
    #[serde(default)]
    research: Option<bool>,
    /// Cap on research rounds.
    #[serde(default)]
    max_tool_rounds: Option<usize>,
    // Per-request chat overrides, same shape as /api/chat.
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
    #[serde(default)]
    dest: Option<String>,
}

/// Merge the default `ChatConfig` with per-request overrides for a tour.
/// [`ChatCfgError::NotConfigured`] means no model could be resolved — the
/// caller then plans a narration-free ranked tour instead of erroring —
/// while `Invalid` is a rejected endpoint override and must surface as a
/// 400. Endpoint/credential handling is [`resolve_chat_endpoint`]'s, the
/// same as `/api/chat`: a tour body carries the identical override fields
/// and would otherwise be the second way to walk off with the stored key.
fn merge_tour_chat_cfg(
    default: &Option<ChatConfig>,
    body: &TourBody,
) -> Result<ChatConfig, ChatCfgError> {
    let base_default = default.clone().unwrap_or_default();
    let model = body
        .chat_model
        .clone()
        .or_else(|| default.as_ref().map(|c| c.model.clone()))
        .ok_or(ChatCfgError::NotConfigured)?;
    let (base_url, api_key) = resolve_chat_endpoint(
        &base_default,
        body.chat_base_url.as_deref(),
        body.chat_api_key.as_deref(),
    )?;
    let temperature = body.temperature.unwrap_or(base_default.temperature);
    let max_tokens = body.max_tokens.unwrap_or(base_default.max_tokens);
    Ok(ChatConfig {
        extra_body: None,
        base_url,
        api_key,
        model,
        temperature,
        max_tokens,
        timeout_secs: base_default.timeout_secs,
    })
}

/// Shape a `TourOptions` from a request body. `edge_types` is passed
/// separately because it has to outlive the borrow.
fn tour_opts_from_body<'a>(
    body: &'a TourBody,
    edge_types: Option<&'a [String]>,
) -> crate::tour::TourOptions<'a> {
    let mut opts = crate::tour::TourOptions::new();
    opts.k = body.k.unwrap_or(14).clamp(1, 80);
    opts.hops = body.hops.unwrap_or(2).min(4);
    opts.max_stops = body
        .max_stops
        .unwrap_or(crate::tour::DEFAULT_MAX_STOPS)
        .clamp(1, crate::tour::MAX_STOPS_LIMIT);
    opts.strategy = body
        .strategy
        .as_deref()
        .map(RankStrategy::from_str_lossy)
        .unwrap_or(RankStrategy::Ppr);
    opts.direction = body
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    opts.edge_types = edge_types;
    opts.include_snippets = body.include_snippets.unwrap_or(true);
    opts.max_context_chars = body
        .max_context_chars
        .unwrap_or(DEFAULT_CONTEXT_CHARS)
        .min(64_000);
    opts.where_clause = body.where_clause.as_deref();
    opts.max_per_file = body.max_per_file.unwrap_or(opts.max_per_file).min(20);
    opts.include_debug = body.include_debug.unwrap_or(true);
    opts.stream = body.stream.unwrap_or(false);
    opts.fast = !body.think.unwrap_or(false);
    opts.research = body.research.unwrap_or(false);
    opts
}

/// Attach the fields the route adds on top of a planned `Tour`.
fn tour_response_json(
    tour: &crate::tour::Tour,
    dest: &str,
    model: Option<&str>,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut v = serde_json::to_value(tour)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("dest".into(), serde_json::Value::String(dest.to_string()));
        if let Some(m) = model {
            obj.insert("chat_model".into(), serde_json::Value::String(m.to_string()));
        }
    }
    Ok(v)
}

/// SSE variant of `/api/tour` (`"stream": true` in the body). Planning a
/// tour against a local model is a minutes-long wait dominated by token
/// generation, so the route narrates itself:
///
/// ```text
/// event: progress  data: {"phase":"retrieved","candidates":14,…}
/// event: progress  data: {"phase":"writing","chars":812,…}
/// event: tour      data: {…the full Tour…}
/// event: error     data: {"error":"…"}      (terminal, replaces tour)
/// ```
fn api_tour_stream(
    state: ServeState,
    body: TourBody,
    db: Arc<dyn KnowledgeStore>,
    embedder: Arc<Embedder>,
    chat_cfg: Option<ChatConfig>,
    want_llm: bool,
) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use futures::StreamExt;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    let repo_root = state.repo_root();
    let embed_lock = state.embed_lock.clone();

    tokio::spawn(async move {
        let dest_name = db.backend_name();
        let emit = |name: &'static str, payload: serde_json::Value| {
            let _ = tx.send(SseEvent::default().event(name).data(payload.to_string()));
        };

        let _permit = match embed_lock.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                emit("error", serde_json::json!({ "error": "embed semaphore closed" }));
                return;
            }
        };

        let edge_types_owned: Option<Vec<String>> =
            body.edge_types.clone().filter(|v| !v.is_empty());
        let opts = tour_opts_from_body(&body, edge_types_owned.as_deref());

        // Same graph toolbox chat gets, so a tour can look past the nodes
        // retrieval happened to surface. Off unless asked for: every tool
        // round is another wait before the first stop.
        let tool_state = state.clone();
        let tool_db = db.clone();
        let tool_embedder = Some(embedder.clone());
        let runner = move |name: &str, args: serde_json::Value| {
            let state = tool_state.clone();
            let db = tool_db.clone();
            let embedder = tool_embedder.clone();
            let name = name.to_string();
            Box::pin(async move { run_chat_tool(state, db, embedder, name, args).await })
                as futures::future::BoxFuture<'static, Result<String, String>>
        };
        let toolbox = opts.research.then(|| chat::ToolBox {
            schemas: crate::mcp::tools::openai_tool_schemas(),
            run: &runner,
            max_rounds: body.max_tool_rounds.unwrap_or(3).min(8),
            max_result_chars: 4_000,
        });

        let mut used_model: Option<String> = None;
        let result = match chat_cfg {
            Some(cfg) => match ChatClient::new(cfg) {
                Ok(client) => {
                    used_model = Some(client.config().model.clone());
                    let emit_progress = emit;
                    let mut on_progress = move |p: crate::tour::TourProgress| {
                        match serde_json::to_value(&p) {
                            Ok(v) => emit_progress("progress", v),
                            Err(e) => tracing::debug!(error = %e, "tour: progress encode failed"),
                        }
                    };
                    match crate::tour::plan_tour_with_progress(
                        &*db,
                        &embedder,
                        &client,
                        repo_root.as_path(),
                        &body.query,
                        opts.clone(),
                        toolbox.as_ref(),
                        &mut on_progress,
                    )
                    .await
                    {
                        Ok(t) => Ok(t),
                        Err(e) => {
                            tracing::warn!(error = %e, "tour guide LLM failed; falling back to ranked itinerary");
                            used_model = None;
                            let reason = e.to_string();
                            emit(
                                "progress",
                                serde_json::json!({ "phase": "fallback", "reason": reason }),
                            );
                            crate::tour::plan_tour_no_llm(
                                &*db,
                                &embedder,
                                repo_root.as_path(),
                                &body.query,
                                opts.clone(),
                            )
                            .await
                            .map(|mut t| {
                                t.warnings.push(format!(
                                    "The tour guide model was unreachable ({}); showing a ranked itinerary.",
                                    reason
                                ));
                                t
                            })
                        }
                    }
                }
                Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
            },
            None => {
                emit("progress", serde_json::json!({ "phase": "retrieving" }));
                crate::tour::plan_tour_no_llm(
                    &*db,
                    &embedder,
                    repo_root.as_path(),
                    &body.query,
                    opts.clone(),
                )
                .await
                .map(|mut t| {
                    if want_llm && !t.stops.is_empty() {
                        t.warnings.push(
                            "No chat model is configured, so this is a ranked itinerary rather than a narrated tour."
                                .to_string(),
                        );
                    }
                    t
                })
            }
        };

        match result {
            Ok(tour) => match tour_response_json(&tour, dest_name, used_model.as_deref()) {
                Ok(v) => emit("tour", v),
                Err(e) => emit("error", serde_json::json!({ "error": format!("encode: {}", e) })),
            },
            Err(e) => emit("error", serde_json::json!({ "error": format!("tour: {}", e) })),
        }
    });

    let stream =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// `POST /api/tour` — plan a guided, narrated walkthrough for a question.
/// Retrieval is required (needs DB + embedder); the LLM guide is optional
/// (falls back to a ranked itinerary), so this route works whenever
/// semantic search does. Returns the full `Tour` (stops carry node ids the
/// UI flies the camera to).
async fn api_tour(State(state): State<ServeState>, Json(body): Json<TourBody>) -> Response {
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

    let edge_types_owned: Option<Vec<String>> = body.edge_types.clone().filter(|v| !v.is_empty());

    // Decide the LLM path up front so we can fall back cleanly.
    let want_llm = !body.no_llm.unwrap_or(false);
    let chat_default = state.chat_default.read().expect("chat_default poisoned").clone();
    let chat_cfg = if want_llm {
        match merge_tour_chat_cfg(&chat_default, &body) {
            Ok(c) => Some(c),
            // No model anywhere is the documented fallback: plan the tour
            // without narration rather than failing the request.
            Err(ChatCfgError::NotConfigured) => None,
            // A rejected override is a caller error, not a reason to
            // silently narrate with a different endpoint than was asked for.
            Err(ChatCfgError::Invalid(msg)) => return err_json(StatusCode::BAD_REQUEST, &msg),
        }
    } else {
        None
    };

    if body.stream.unwrap_or(false) {
        return api_tour_stream(state, body, db, embedder, chat_cfg, want_llm);
    }

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let repo_root = state.repo_root();
    let dest_name = db.backend_name();

    let opts = tour_opts_from_body(&body, edge_types_owned.as_deref());

    let mut used_model: Option<String> = None;
    let result = match chat_cfg {
        Some(cfg) => match ChatClient::new(cfg) {
            Ok(client) => {
                used_model = Some(client.config().model.clone());
                match crate::tour::plan_tour(
                    &*db,
                    &embedder,
                    &client,
                    repo_root.as_path(),
                    &body.query,
                    opts.clone(),
                )
                .await
                {
                    Ok(t) => Ok(t),
                    Err(e) => {
                        // LLM unreachable/failed — still give a tour, but
                        // say why it isn't narrated.
                        tracing::warn!(error = %e, "tour guide LLM failed; falling back to ranked itinerary");
                        used_model = None;
                        let reason = e.to_string();
                        crate::tour::plan_tour_no_llm(
                            &*db,
                            &embedder,
                            repo_root.as_path(),
                            &body.query,
                            opts.clone(),
                        )
                        .await
                        .map(|mut t| {
                            t.warnings
                                .push(format!("The tour guide model was unreachable ({}); showing a ranked itinerary.", reason));
                            t
                        })
                    }
                }
            }
            Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
        },
        None => {
            let asked_for_llm = want_llm;
            crate::tour::plan_tour_no_llm(
                &*db,
                &embedder,
                repo_root.as_path(),
                &body.query,
                opts.clone(),
            )
            .await
            .map(|mut t| {
                if asked_for_llm && !t.stops.is_empty() {
                    t.warnings.push(
                        "No chat model is configured, so this is a ranked itinerary rather than a narrated tour."
                            .to_string(),
                    );
                }
                t
            })
        }
    };
    drop(_permit);

    match result {
        Ok(tour) => match tour_response_json(&tour, dest_name, used_model.as_deref()) {
            Ok(v) => ok_json(v.to_string()),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
        },
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("tour: {}", e)),
    }
}

/// Cap on how much captured source is sent to the UI in one node hydrate.
/// A whole-file node can be megabytes; the panel is a transparency view,
/// not a file viewer, and the Preview tab reads the live file anyway.
const STORED_CODE_PREVIEW_CHARS: usize = 20_000;

/// The parts of a stored row that aren't its text: what the vector store
/// holds *about* this node rather than what it embedded.
///
/// Split out from [`node_row_to_json`] because it costs real work — a
/// blake3 of the file on disk for the staleness check, and a sparse-vector
/// rebuild for the dimension count. That is fine for the single-row hydrate
/// behind a node click, and not fine for the traverse handler, which runs
/// `node_row_to_json` over every node it returns.
///
/// `sparse_dims` is the reason this exists. It is the one cap whose effect
/// the UI could not otherwise detect: `MAX_SPARSE_DIMS` truncates silently,
/// leaving nothing in the stored text to notice. Recomputing the vector here
/// uses the same function ingest used, so the count is exact rather than
/// estimated.
fn node_storage_meta(
    n: &storage::NodeRow,
    repo_root: &std::path::Path,
    stats: Option<&storage::sparse_stats::SparseStats>,
) -> serde_json::Value {
    let sparse_dims =
        storage::text::build_node_sparse_vector(&n.node_text, &n.code, stats).len();
    let stale = if n.file.is_empty() || n.file_hash.is_empty() {
        None
    } else {
        storage::file_matches_hash(repo_root, &n.file, &n.file_hash).map(|matches| !matches)
    };

    // The captured source itself, so the UI can show what the store holds
    // rather than only telling the user it holds something. This is what an
    // agent's snippet reads return and what the keyword index was built
    // from, and it can differ from the working tree — which is the whole
    // reason it is worth showing next to the Preview tab's live read.
    let code_chars = n.code.chars().count();
    let code = if code_chars > STORED_CODE_PREVIEW_CHARS {
        let cut = n
            .code
            .char_indices()
            .nth(STORED_CODE_PREVIEW_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(n.code.len());
        n.code[..cut].to_string()
    } else {
        n.code.clone()
    };

    serde_json::json!({
        "last_update_at": n.last_update_at,
        "file_hash": n.file_hash,
        "code": code,
        "code_truncated": code_chars > STORED_CODE_PREVIEW_CHARS,
        "code_chars": code_chars,
        "node_text_chars": n.node_text.chars().count(),
        "vector_dim": n.vector.len(),
        "sparse_dims": sparse_dims,
        // `null` when the file can't be read at all (deleted, or a binary
        // document that was never captured) — distinct from "not stale".
        "stale": stale,
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
        // Boundary facts, as the store holds them (comma-joined scalars —
        // see `storage::facts`). The canvas reads the structured
        // `GraphNode::boundaries` from graph.json instead; these are here so
        // a caller hitting the DB route directly is not told less than a
        // caller hitting the graph route.
        "boundary_kinds": fact_str(n, "boundary_kinds"),
        "boundary_detail": fact_str(n, "boundary_detail"),
    })
}

/// A string-valued fact, or `null` when the node does not carry it.
fn fact_str(n: &storage::NodeRow, key: &str) -> Option<String> {
    match n.facts.get(key) {
        Some(storage::facts::FactValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
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
    println!("                       (default: `ug active`, else cwd basename, else most recently generated)");
    println!("  {C_CYAN}-d, --db{C_RESET} <path>      OverGraph DB for /api/db + /api/search routes");
    println!("                       (default: per-project ugdb, or the graph file's sibling ugdb with -i)");
    println!("  {C_YELLOW}--no-db{C_RESET}            Don't open DB; routes return 503");
    println!("  {C_CYAN}-p, --port{C_RESET} <n>       TCP port (default: 8080)");
    println!("  {C_CYAN}--host{C_RESET} <addr>        Bind address (default: 127.0.0.1)");
    println!("  {C_GREEN}--watch{C_RESET}             Reload graph file when its mtime changes");
    println!("  {C_CYAN}--graph-mode{C_RESET} <mode>  How the browser gets the graph: auto|local|server");
    println!("                       (default: auto — `server` at or above 50 MB of graph.json,");
    println!("                        where the page asks this server instead of downloading it)");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>   Repo root for hybrid-search snippet resolution");
    println!("  {C_CYAN}--base-url{C_RESET} <url>      Embedding/chat base URL (OpenAI-compatible)");
    println!("  {C_CYAN}--api-key{C_RESET} <key>       Embedding/chat API key");
    println!("  {C_CYAN}--model{C_RESET} <name>        Embedding model (fastembed alias for local)");
    println!();
    println!("{C_BOLD}Security:{C_RESET}");
    println!("  {C_YELLOW}ug serve{C_RESET} is intended for {C_BOLD}local{C_RESET} use: it binds to 127.0.0.1 by default");
    println!("  and the HTTP API has {C_BOLD}no authentication{C_RESET}. Do not run it on a production");
    println!("  server or expose it to a network without a properly secured reverse proxy");
    println!("  (authentication + TLS + network policy) in front of it.");
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
    println!("  {C_CYAN}POST{C_RESET} /api/tour  body: {{ query, k?, hops?, max_stops?, no_llm?, ... }}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET}                          {C_YELLOW}# all projects under ~/.ug{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} --project myrepo --watch");
    println!("  {C_CYAN}ug serve{C_RESET} -i path/to/graph.json -p 8080   {C_YELLOW}# single-project mode{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} \\");
    println!("           --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("           --chat-model Qwen3.6-35B-A3B-MLX-8bit");
}

#[cfg(test)]
mod router_tests;

#[cfg(test)]
mod tests {
    use super::{
        is_allowed_host, host_label, pick_initial_project, resolve_chat_endpoint, slice_file_text,
        stored_source_for_file, ChatCfgError, ChatConfig,
    };

    fn cfg(base_url: &str, api_key: &str) -> ChatConfig {
        ChatConfig {
            base_url: base_url.into(),
            api_key: api_key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn request_supplied_endpoint_never_inherits_the_stored_key() {
        let default = cfg("https://api.openai.com/v1", "sk-real-secret");

        // The attack: point the endpoint elsewhere, send no key, collect the
        // server's key from the Authorization header. The key must not follow.
        let (url, key) =
            resolve_chat_endpoint(&default, Some("https://evil.tld/v1"), None).expect("allowed");
        assert_eq!(url, "https://evil.tld/v1");
        assert_eq!(key, "", "stored key leaked to a request-supplied endpoint");

        // A caller bringing its own key is fine — that's the legitimate
        // "flip to another provider" flow.
        let (_, key) =
            resolve_chat_endpoint(&default, Some("https://other.tld/v1"), Some("sk-theirs"))
                .expect("allowed");
        assert_eq!(key, "sk-theirs");

        // Same origin as the configured endpoint is not a redirection, so the
        // stored key still applies (the UI echoes base_url back in the body).
        let (_, key) = resolve_chat_endpoint(&default, Some("https://api.openai.com/v1/"), None)
            .expect("allowed");
        assert_eq!(key, "sk-real-secret");

        // No override at all: unchanged behaviour.
        let (url, key) = resolve_chat_endpoint(&default, None, None).expect("allowed");
        assert_eq!((url.as_str(), key.as_str()), ("https://api.openai.com/v1", "sk-real-secret"));
    }

    #[test]
    fn chat_endpoint_rejects_metadata_and_non_http_targets() {
        let default = cfg("https://api.openai.com/v1", "sk-real-secret");
        for bad in [
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(
                matches!(
                    resolve_chat_endpoint(&default, Some(bad), None),
                    Err(ChatCfgError::Invalid(_))
                ),
                "{bad} should have been rejected"
            );
        }

        // A local model server is the common legitimate case and stays open.
        assert!(resolve_chat_endpoint(&default, Some("http://127.0.0.1:11434/v1"), None).is_ok());
    }

    #[test]
    fn host_guard_accepts_local_names_and_rejects_domains() {
        assert_eq!(host_label("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_label("[::1]:8080"), "::1");
        assert_eq!(host_label("::1"), "::1");
        assert_eq!(host_label("Evil.TLD:80"), "evil.tld");

        for ok in ["localhost", "localhost:8080", "127.0.0.1:8080", "[::1]:8080", "192.168.1.9:8080"] {
            assert!(is_allowed_host(ok), "{ok} should be allowed");
        }
        // The rebinding case: attacker's domain, currently resolving to us.
        for bad in ["evil.tld", "evil.tld:8080", "ug.attacker.example", ""] {
            assert!(!is_allowed_host(bad), "{bad} should be rejected");
        }
    }
    use tempfile::TempDir;
    use ultragraph::storage::db::{Db, NodeRow};
    use ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM;
    use ultragraph::types::{GraphData, GraphNode, GraphNodeType};

    #[test]
    fn initial_project_prefers_the_active_one() {
        // list_projects order: most recently indexed first.
        let names: Vec<String> = ["dlab", "Ultra-Graph", "ug"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // `ug active Ultra-Graph` wins even from a subdir whose basename
        // (`native`) isn't a project, and even from the repo root, whose
        // basename (`ug`) is a different project.
        let (name, why) =
            pick_initial_project(&names, Some("Ultra-Graph".into()), "native".into());
        assert_eq!((name.as_str(), why), ("Ultra-Graph", "active project"));
        let (name, _) = pick_initial_project(&names, Some("Ultra-Graph".into()), "ug".into());
        assert_eq!(name, "Ultra-Graph");

        // No active project: the cwd match, else the most recent.
        let (name, why) = pick_initial_project(&names, None, "ug".into());
        assert_eq!((name.as_str(), why), ("ug", "matches the current directory"));
        let (name, why) = pick_initial_project(&names, None, "native".into());
        assert_eq!((name.as_str(), why), ("dlab", "most recently indexed project"));

        // A stale marker naming an unlisted project falls through.
        let (name, why) = pick_initial_project(&names, Some("deleted".into()), "native".into());
        assert_eq!((name.as_str(), why), ("dlab", "most recently indexed project"));
    }

    fn graph_node(id: &str, file: &str, start: Option<u32>, end: Option<u32>) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: id.into(),
            node_type: GraphNodeType::Function,
            file: Some(file.into()),
            start_line: start,
            end_line: end,
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            ..Default::default()
        }
    }

    fn file_node(id: &str, file: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: file.into(),
            node_type: GraphNodeType::File,
            file: Some(file.into()),
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            ..Default::default()
        }
    }

    fn row(id: &str, node_type: &str, file: &str, start: u32, end: u32, code: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            name: id.into(),
            node_type: node_type.into(),
            description: String::new(),
            file: file.into(),
            start_line: start,
            end_line: end,
            last_update_at: 0,
            node_text: String::new(),
            vector: vec![0.0; DEFAULT_EMBEDDING_DIM],
            code: code.into(),
            file_hash: String::new(),
            facts: Default::default(),
        }
    }

    /// `/api/file`'s repo-independent fallback: with the repo path gone, the
    /// store's captured source is what the Preview tab serves. The resolver
    /// must find the exact span first, fall back to the whole-file capture,
    /// and skip rows with no captured code.
    #[tokio::test]
    async fn stored_file_fallback_resolves_span_then_whole_file() {
        let tmp = TempDir::new().unwrap();
        let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

        let sym = row(
            "function:src/a.rs:2:sym",
            "Function",
            "src/a.rs",
            2,
            3,
            "two\nthree\n",
        );
        let whole = row("file:src/a.rs", "File", "src/a.rs", 0, 0, "one\ntwo\nthree\nfour\n");
        db.upsert_nodes(&[sym.clone(), whole.clone()]).await.unwrap();

        let graph = GraphData {
            nodes: vec![
                graph_node("function:src/a.rs:2:sym", "src/a.rs", Some(2), Some(3)),
                file_node("file:src/a.rs", "src/a.rs"),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };

        // Exact span match → the symbol's captured slice, flagged as sliced.
        let got = stored_source_for_file(&graph, &db, "src/a.rs", Some(2), Some(3))
            .await
            .unwrap();
        assert_eq!((got.0.as_str(), got.1), ("two\nthree\n", true));

        // Whole-file request → the file node's captured content.
        let got = stored_source_for_file(&graph, &db, "src/a.rs", None, None)
            .await
            .unwrap();
        assert_eq!((got.0.as_str(), got.1), ("one\ntwo\nthree\nfour\n", false));

        // Span with no exact match falls back to the whole-file capture, and
        // reports it as unsliced.
        let got = stored_source_for_file(&graph, &db, "src/a.rs", Some(1), Some(1))
            .await
            .unwrap();
        assert_eq!((got.0.as_str(), got.1), ("one\ntwo\nthree\nfour\n", false));

        // Unknown file → None.
        assert_eq!(
            stored_source_for_file(&graph, &db, "src/absent.rs", None, None).await,
            None
        );

        // A row whose capture is empty (pre-column, or a binary file) is
        // skipped rather than served as blank.
        db.upsert_nodes(&[row(
            "function:src/b.rs:5:empty",
            "Function",
            "src/b.rs",
            5,
            9,
            "",
        )])
        .await
        .unwrap();
        let graph2 = GraphData {
            nodes: vec![graph_node("function:src/b.rs:5:empty", "src/b.rs", Some(5), Some(9))],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        assert_eq!(
            stored_source_for_file(&graph2, &db, "src/b.rs", Some(5), Some(9)).await,
            None
        );
    }

    /// A range request must return the same lines whether it was answered
    /// from the repo or from the index — the whole-file capture is cut down
    /// to the span rather than served entire, and `total_lines` stays the
    /// file's length either way.
    #[test]
    fn a_range_is_cut_out_of_a_whole_file_the_same_way_from_either_source() {
        let text = "one\ntwo\nthree\nfour\n".to_string();

        let (body, sliced, total) = slice_file_text(text.clone(), Some(2), Some(3));
        assert_eq!((body.as_str(), sliced, total), ("two\nthree", true, 4));

        // No range → the whole file, untouched.
        let (body, sliced, total) = slice_file_text(text.clone(), None, None);
        assert_eq!((body.as_str(), sliced, total), (text.as_str(), false, 4));

        // `end` omitted means the single `start` line.
        let (body, _, _) = slice_file_text(text.clone(), Some(4), None);
        assert_eq!(body, "four");

        // Out-of-range bounds clamp rather than panic — an index can lag the
        // file it describes.
        let (body, _, total) = slice_file_text(text.clone(), Some(3), Some(99));
        assert_eq!((body.as_str(), total), ("three\nfour", 4));
        let (body, _, _) = slice_file_text(text, Some(99), Some(120));
        assert_eq!(body, "");
    }
}
