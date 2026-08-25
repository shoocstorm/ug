//! `snapshot.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use super::projects_api::file_mtime;
use super::*;
use ultragraph::types::{GraphData, GraphEdgeType, GraphNodeType};

// ---------- Graph snapshot (atomic-swap on watch reload) ----------

pub(crate) struct GraphSnapshot {
    /// `graph.json`'s bytes, kept resident only when a client might plausibly
    /// ask for them.
    ///
    /// In server mode the page is routed away from this file — that is what
    /// server mode *is* (see [`GRAPH_SERVER_MODE_BYTES`] and
    /// `00-preamble.js`) — so on exactly the graphs where the buffer is large
    /// it is 346 MB held for a request that never comes. Above the cutoff it
    /// is dropped and `GET /graph.json` re-reads from disk, which is a request
    /// the page does not make and `ug serve` binds to loopback to serve.
    ///
    /// Worse if it were kept and then asked for: `EncodedAsset::brotli()`
    /// would compress 346 MB and retain *that* forever too.
    ///
    /// See P10.5 in docs/dev/PERF-TUNING-JOURNEY.md.
    pub(crate) graph_asset: Option<EncodedAsset>,
    /// Size of `graph.json` on disk. Always known, whether or not the bytes
    /// are held: it is what `token()`, the capability report and the cache
    /// budget are actually asking for.
    pub(crate) graph_bytes: usize,
    pub(crate) parsed: GraphData,
    /// mtime of the `graph.json` this was read from, sampled *before* the
    /// read so a file rewritten mid-read reads as stale rather than current.
    /// `None` for a snapshot with no file behind it (the zero-project
    /// placeholder), which is never refreshed.
    ///
    /// This is what makes a cached snapshot checkable: see
    /// [`refresh_snapshot_if_stale`].
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) adj: OnceLock<AdjIndex>,
    pub(crate) centrality: OnceLock<String>,
    pub(crate) cycles: OnceLock<String>,
    /// The slim node index `/api/graph/nodes` serves — see [`build_slim_index`].
    /// Built on first request and encoded once, like `centrality` and `cycles`,
    /// because a browser in server mode asks for it exactly once per load.
    pub(crate) slim: OnceLock<EncodedAsset>,
    /// The binary form of the same index, served at `/api/graph/nodes.bin`.
    /// Separate `OnceLock` from `slim` because a page asks for exactly one of
    /// the two and building the other would be pure waste on the graphs where
    /// this matters most.
    pub(crate) slim_bin: OnceLock<EncodedAsset>,
    /// `/api/graph/stats`, rendered once per snapshot.
    ///
    /// Every field of it is derived from this snapshot and none of it can
    /// change without the snapshot being replaced, yet it was recomputed per
    /// request — a full pass over both the node and edge lists, which on a
    /// large repo is ~900k iterations to answer a question whose answer is
    /// fixed. The UI polls this.
    pub(crate) stats: OnceLock<String>,
    /// The last `/api/graph/search` needle and the node indices it matched.
    ///
    /// A search box is typed one character at a time, and substring
    /// containment is monotone under prefix: every node matching `node` in
    /// some field already matched `nod` in that field. So when the incoming
    /// needle extends the remembered one, the scan runs over this list instead
    /// of all 161k nodes — 23 ms to under 1 ms on a large graph. See P10.2 in
    /// docs/dev/PERF-TUNING-JOURNEY.md.
    ///
    /// Keyed on the needle **alone**, never on the type filter: a request
    /// narrowed by `?types=` matches a subset, and remembering that subset
    /// under the bare needle would under-answer the next request without one.
    ///
    /// Bounded by the node count — a single-letter needle matches nearly
    /// everything, because the ids are long paths — and invalidated for free
    /// by snapshot replacement, because it lives on the snapshot.
    pub(crate) search_memo: Mutex<Option<SearchMemo>>,
}

/// One remembered `/api/graph/search` result set — see
/// [`GraphSnapshot::search_memo`].
pub(crate) struct SearchMemo {
    pub(crate) needle: String,
    /// `Arc` so a request can take the candidate list and drop the lock,
    /// rather than holding it across the whole scan or cloning up to 650 KB.
    pub(crate) hits: Arc<Vec<u32>>,
}

impl GraphSnapshot {
    /// Identifies *this* snapshot, so a client that split one graph across
    /// several requests can tell whether they all came from the same one.
    ///
    /// Size plus node/edge counts plus mtime: cheap to compute per request and
    /// certain to change when the graph is regenerated. Not a hash of the
    /// content — hashing 346 MB per `/api/capabilities` call would cost more
    /// than the problem.
    pub(crate) fn token(&self) -> String {
        let mtime = self
            .mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!(
            "{}-{}-{}-{}",
            self.graph_bytes,
            self.parsed.nodes.len(),
            self.parsed.edges.len(),
            mtime
        )
    }
}

/// Adjacency built once per snapshot. `id_to_idx` maps a node's string id to
/// its index in `parsed.nodes`; the rows hold the **edge** indices into
/// `parsed.edges` whose source (respectively target) is node `i`.
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
///
/// The rows are **compressed sparse row**, not `Vec<Vec<u32>>`.
///
/// The obvious shape costs one heap allocation per node per direction — 323,450
/// of them on a large repo, each with its own header and its own doubling
/// overshoot — to hold 1.49 million `u32`s that are appended in a single known
/// pass. Two counting passes give exact capacity and four allocations total.
/// `graph::algos` already reached the same conclusion for Brandes and says so
/// on its own CSR. See P10.6 in docs/dev/PERF-TUNING-JOURNEY.md.
pub(crate) struct AdjIndex {
    pub(crate) id_to_idx: HashMap<String, usize>,
    /// `out_flat[out_off[i]..out_off[i + 1]]` — edge indices whose source is `i`.
    out_off: Vec<u32>,
    out_flat: Vec<u32>,
    /// The same, for edges whose target is `i`.
    inc_off: Vec<u32>,
    inc_flat: Vec<u32>,
    /// Endpoint node indices per edge, resolved once here so no caller has to
    /// hash `edges[ei].source` again. `api_edges` did that twice per edge it
    /// returned — 17,000 string hashes to answer one click on a hub.
    pub(crate) esrc: Vec<u32>,
    pub(crate) etgt: Vec<u32>,
}

impl AdjIndex {
    /// Every edge incident to node `i`, outbound first. Callers that care about
    /// direction compare [`Self::src_of`] themselves.
    pub(crate) fn incident(&self, i: usize) -> impl Iterator<Item = u32> + '_ {
        let o = &self.out_flat[self.out_off[i] as usize..self.out_off[i + 1] as usize];
        let c = &self.inc_flat[self.inc_off[i] as usize..self.inc_off[i + 1] as usize];
        o.iter().copied().chain(c.iter().copied())
    }

    /// Only the edges *leaving* node `i`, for the directed walks
    /// (`/api/graph/traverse`, `/api/graph/path`).
    pub(crate) fn outgoing(&self, i: usize) -> impl Iterator<Item = u32> + '_ {
        self.out_flat[self.out_off[i] as usize..self.out_off[i + 1] as usize]
            .iter()
            .copied()
    }

    /// Source node index of edge `ei`, or `None` when the edge names a node
    /// the graph does not contain — which `build_adj` skips and every caller
    /// must therefore be able to skip too.
    pub(crate) fn src_of(&self, ei: u32) -> Option<u32> {
        match self.esrc.get(ei as usize) {
            Some(&NO_NODE) | None => None,
            Some(&i) => Some(i),
        }
    }

    /// Target node index of edge `ei`. See [`Self::src_of`].
    pub(crate) fn tgt_of(&self, ei: u32) -> Option<u32> {
        match self.etgt.get(ei as usize) {
            Some(&NO_NODE) | None => None,
            Some(&i) => Some(i),
        }
    }
}

/// Endpoint sentinel for an edge naming a node that is not in the graph.
///
/// `build_adj` has always dropped such edges from the adjacency rows; this
/// records *which* they are so the endpoint columns stay 1:1 with
/// `parsed.edges` and a caller indexing by edge number cannot go off by one.
const NO_NODE: u32 = u32::MAX;

pub(crate) fn build_adj(graph: &GraphData) -> AdjIndex {
    let n = graph.nodes.len();
    let id_to_idx: HashMap<String, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect();

    // Pass 1: resolve endpoints and count degrees.
    let mut esrc: Vec<u32> = Vec::with_capacity(graph.edges.len());
    let mut etgt: Vec<u32> = Vec::with_capacity(graph.edges.len());
    let mut out_off: Vec<u32> = vec![0; n + 1];
    let mut inc_off: Vec<u32> = vec![0; n + 1];
    for e in &graph.edges {
        let s = id_to_idx.get(&*e.source).copied();
        let t = id_to_idx.get(&*e.target).copied();
        match (s, t) {
            (Some(si), Some(ti)) => {
                esrc.push(si as u32);
                etgt.push(ti as u32);
                out_off[si + 1] += 1;
                inc_off[ti + 1] += 1;
            }
            // Recorded, but contributes to no row — same edges `build_adj`
            // has always dropped.
            _ => {
                esrc.push(NO_NODE);
                etgt.push(NO_NODE);
            }
        }
    }

    // Prefix-sum the counts into row starts.
    for i in 0..n {
        out_off[i + 1] += out_off[i];
        inc_off[i + 1] += inc_off[i];
    }

    // Pass 2: place each edge index, walking a cursor per row so the order
    // within a row is edge order — which is what `Vec::push` produced before.
    let mut out_flat: Vec<u32> = vec![0; out_off[n] as usize];
    let mut inc_flat: Vec<u32> = vec![0; inc_off[n] as usize];
    let mut out_cur = out_off.clone();
    let mut inc_cur = inc_off.clone();
    for (ei, (&si, &ti)) in esrc.iter().zip(etgt.iter()).enumerate() {
        if si == NO_NODE {
            continue;
        }
        // An edge index that doesn't fit in a u32 would mean four billion edges
        // in one graph; the cast is checked rather than assumed away.
        let Ok(ei) = u32::try_from(ei) else { break };
        out_flat[out_cur[si as usize] as usize] = ei;
        out_cur[si as usize] += 1;
        inc_flat[inc_cur[ti as usize] as usize] = ei;
        inc_cur[ti as usize] += 1;
    }

    AdjIndex {
        id_to_idx,
        out_off,
        out_flat,
        inc_off,
        inc_flat,
        esrc,
        etgt,
    }
}

/// Bytes of `graph.json` at or above which the browser is told to leave the
/// file alone and ask the server its questions instead.
///
/// This is a property of what a browser tab can hold, not of the graph: past
/// roughly this size the download, the `JSON.parse` and the retained object
/// graph together cost more than every interaction the page then performs.
/// Measured on a 346 MB index, the whole-file path retains ~295 MB of JS heap
/// against ~66 MB for the slim index.
///
/// This is the *default*; the user can override it per machine with
/// `ug config set graph.server_mode_bytes <bytes>` (the settings panel's
/// Graph section), which [`server_mode_bytes`] resolves.
pub(crate) const GRAPH_SERVER_MODE_BYTES: usize = 50 * 1024 * 1024;

/// The effective server-mode cutoff: the persisted `graph.server_mode_bytes`
/// when set, otherwise the compiled-in default [`GRAPH_SERVER_MODE_BYTES`].
pub(crate) fn server_mode_bytes() -> usize {
    // Env first, like `snapshot_cache_budget`. The persisted config is read
    // through a process-wide `OnceLock`, so it is fixed for the life of the
    // process and cannot be varied per test; this is the knob that can.
    if let Some(v) = std::env::var("UG_SERVE_GRAPH_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return v;
    }
    crate::config::get("graph.server_mode_bytes")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(GRAPH_SERVER_MODE_BYTES)
}

/// How `ug serve` decides which of the two the browser gets.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum GraphModePolicy {
    /// Pick per project, by `graph.json`'s size. The default.
    Auto,
    /// Always ship the whole file — what every release before this did.
    Local,
    /// Always serve the slim index, whatever the size. For testing the server
    /// path against a small repo.
    Server,
}

impl GraphModePolicy {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "local" => Some(Self::Local),
            "server" => Some(Self::Server),
            _ => None,
        }
    }

    /// `"local"` or `"server"` for a graph of `bytes`, given the cutoff the
    /// caller resolved from `graph.server_mode_bytes` (or its default).
    pub(crate) fn resolve(self, bytes: usize, server_mode_cutoff: usize) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
            Self::Auto if bytes >= server_mode_cutoff => "server",
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
/// The columns behind both encodings of the slim index.
///
/// Extracted so `build_slim_index` (JSON) and `build_binary_index` (the
/// `.bin` frame) cannot drift: they are two serialisations of *this*, built by
/// one pass over the graph. `slim_index_encodings_describe_the_same_graph` in
/// `tests/slim_index_test.rs` is what holds them to it.
pub(crate) struct SlimColumns<'a> {
    pub(crate) n: usize,
    pub(crate) edge_count: usize,
    pub(crate) ids: Vec<&'a str>,
    pub(crate) names: Vec<&'a str>,
    pub(crate) type_names: Vec<&'static str>,
    pub(crate) types: Vec<u32>,
    pub(crate) file_names: Vec<&'a str>,
    pub(crate) files: Vec<i64>,
    pub(crate) start: Vec<u32>,
    pub(crate) end: Vec<u32>,
    /// Indices of the nodes carrying at least one boundary. Sparse on purpose:
    /// on a 161,725-node graph exactly 170 nodes carry one.
    pub(crate) boundary: Vec<u32>,
    pub(crate) deg: Vec<u32>,
    pub(crate) catalog_roots: Vec<u32>,
    pub(crate) node_type_counts: BTreeMap<&'static str, usize>,
    pub(crate) edge_type_counts: BTreeMap<&'static str, usize>,
    pub(crate) languages: Option<HashMap<String, u32>>,
    pub(crate) kb_type: Option<String>,
}

pub(crate) fn build_slim_columns(graph: &GraphData) -> SlimColumns<'_> {
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
        names.push(if node.name.is_empty() {
            &node.id
        } else {
            &node.name
        });

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
        let (Some(&si), Some(&ti)) = (idx_of.get(&*e.source), idx_of.get(&*e.target))
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
                && matches!(node.node_type, GraphNodeType::Folder | GraphNodeType::File)
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

    SlimColumns {
        n,
        edge_count: graph.edges.len(),
        ids,
        names,
        type_names,
        types,
        file_names,
        files,
        start,
        end,
        boundary,
        deg,
        catalog_roots,
        node_type_counts,
        edge_type_counts,
        languages: root_folder.map(|f| f.language_breakdown.clone()),
        kb_type: root_folder
            .and_then(|f| f.classification.as_ref())
            .map(|c| format!("{:?}", c).to_lowercase()),
    }
}

pub(crate) fn build_slim_index(graph: &GraphData) -> String {
    let c = build_slim_columns(graph);
    serde_json::json!({
        "v": 1,
        "n": c.n,
        "edgeCount": c.edge_count,
        // Not `snap.token()` — this builder only sees the parsed graph. The
        // client compares against the token in `/api/capabilities`, and the
        // two agree because both are derived from the same snapshot.
        "nodeCount": c.n,
        "ids": c.ids,
        "names": c.names,
        "types": c.type_names,
        "typeIdx": c.types,
        "files": c.file_names,
        "fileIdx": c.files,
        "startLine": c.start,
        "endLine": c.end,
        "boundary": c.boundary,
        "deg": c.deg,
        "catalogRoots": c.catalog_roots,
        "nodeTypeCounts": c.node_type_counts,
        "edgeTypeCounts": c.edge_type_counts,
        // Verbatim, because `IndexStats` already serialises to the camelCase
        // shape `transformData` reads — the page needs no translation layer.
        "stats": graph.stats,
        "languages": c.languages,
        "kbType": c.kb_type,
    })
    .to_string()
}

pub(crate) fn load_snapshot(path: &PathBuf) -> Result<Arc<GraphSnapshot>, String> {
    // Sampled before the read, so a rewrite that lands while we are reading
    // leaves the snapshot looking older than the file and the next freshness
    // check picks it up. The other order would record the post-write mtime
    // against pre-write content and never correct itself.
    let mtime = file_mtime(path);
    let size = std::fs::metadata(path)
        .map_err(|e| format!("stat {}: {}", path.display(), e))?
        .len() as usize;

    // Two ways in, chosen by whether the bytes are worth keeping.
    //
    // Below the cutoff: read the file, parse from the slice, and hand the same
    // allocation to the encoder — the bytes are wanted anyway, so reading them
    // once and keeping them is free.
    //
    // Above it: **stream** the parse and never materialise the file at all.
    // Reading 346 MB and dropping it after the parse frees the memory but does
    // not return it — the allocator keeps the region, so the process is left
    // sitting on the peak either way and the saving is invisible. Not
    // allocating it is the only version that shows up in RSS.
    let (parsed, graph_asset, graph_bytes) = if size < server_mode_bytes() {
        let raw = std::fs::read(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
        let n = raw.len();
        // `from_slice`, not `from_utf8` then `from_str`: serde validates UTF-8
        // as it parses, so the separate `String::from_utf8` was a second full
        // pass to prove something the parse proves anyway.
        let parsed: GraphData =
            serde_json::from_slice(&raw).map_err(|e| format!("parse {}: {}", path.display(), e))?;
        (
            parsed,
            Some(EncodedAsset::new(raw, "application/json; charset=utf-8")),
            n,
        )
    } else {
        // Memory-mapped, not read. `from_reader` over a `BufReader` also
        // avoids the allocation but parses roughly 4× slower (0.31 s → 1.33 s
        // on a 330 MB index), which is most of the startup win P3.1 bought.
        // A mapping gives `from_slice` its contiguous slice while the pages
        // stay file-backed: they are evictable under pressure and never
        // become anonymous memory the allocator then refuses to give back.
        let file =
            std::fs::File::open(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
        // SAFETY: the usual mmap caveat — another process truncating or
        // rewriting `graph.json` underneath this mapping is undefined. The
        // file is in `~/.ug`, written by `ug gen` via a whole-file write, and
        // `ug serve`'s own reload path replaces the snapshot rather than
        // editing in place.
        let mapped = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| format!("map {}: {}", path.display(), e))?;
        let parsed: GraphData = serde_json::from_slice(&mapped)
            .map_err(|e| format!("parse {}: {}", path.display(), e))?;
        (parsed, None, size)
    };
    Ok(Arc::new(GraphSnapshot {
        graph_asset,
        graph_bytes,
        parsed,
        mtime,
        adj: OnceLock::new(),
        centrality: OnceLock::new(),
        cycles: OnceLock::new(),
        slim: OnceLock::new(),
        slim_bin: OnceLock::new(),
        stats: OnceLock::new(),
        search_memo: Mutex::new(None),
    }))
}
