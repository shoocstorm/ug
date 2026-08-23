//! `snapshot.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use super::projects_api::file_mtime;
use super::*;
use ultragraph::types::{GraphData, GraphEdgeType, GraphNodeType};

// ---------- Graph snapshot (atomic-swap on watch reload) ----------

pub(crate) struct GraphSnapshot {
    pub(crate) encoded: EncodedAsset,
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
pub(crate) struct AdjIndex {
    pub(crate) id_to_idx: HashMap<String, usize>,
    pub(crate) out: Vec<Vec<u32>>,
    pub(crate) inc: Vec<Vec<u32>>,
}

impl AdjIndex {
    /// Every edge incident to node `i`, outbound first. Callers that care about
    /// direction compare `edges[ei].source` themselves.
    pub(crate) fn incident(&self, i: usize) -> impl Iterator<Item = u32> + '_ {
        self.out[i]
            .iter()
            .copied()
            .chain(self.inc[i].iter().copied())
    }
}

pub(crate) fn build_adj(graph: &GraphData) -> AdjIndex {
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
    AdjIndex {
        id_to_idx,
        out,
        inc,
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
        slim_bin: OnceLock::new(),
        stats: OnceLock::new(),
    }))
}
