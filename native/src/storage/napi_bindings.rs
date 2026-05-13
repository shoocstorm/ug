//! NAPI surface for the storage module.
//!
//! Async functions exposed here return JSON-encoded strings; the TS side
//! parses them with `JSON.parse`. Keeping the wire format as JSON avoids
//! shipping every internal struct over the NAPI boundary as a typed
//! object - a single string is easier to evolve.
//!
//! Each call opens the OverGraph connection from `db_path`. OverGraph
//! reopens are cheap (memory-mapped) so this trades a tiny per-call
//! cost for a much simpler API surface and matches how the MCP server
//! is expected to be driven (one request -> one call).

use crate::storage::db::Db;
use crate::storage::embed::{Embedder, EmbedderConfig};
use crate::storage::ingest::ingest_graph;
use crate::storage::query::{
    search_kb as run_search_kb, semantic_search as run_semantic_search,
    traverse_filtered as run_traverse_filtered, ContextItem, Direction, RankStrategy,
    RankedContext, SearchKbOptions,
};
use crate::types::GraphData;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Optional embedder overrides. Mirrors [`EmbedderConfig`] but with all
/// fields optional so callers only specify what they want to override.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedderOptions {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub embedding_dim: Option<u32>,
    pub batch_size: Option<u32>,
    pub timeout_secs: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchKbJsonOptions {
    pub query: String,
    #[serde(default)]
    pub k: Option<u32>,
    #[serde(default)]
    pub hops: Option<u32>,
    #[serde(default)]
    pub edge_types: Option<Vec<String>>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub max_chars: Option<u32>,
    #[serde(default)]
    pub mmr_lambda: Option<f64>,
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub where_clause: Option<String>,
    #[serde(default)]
    pub include_snippets: Option<bool>,
    /// Ranking strategy: "ppr" (default) or "mmr".
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub ppr_restart_prob: Option<f64>,
    #[serde(default)]
    pub ppr_max_iter: Option<u32>,
    #[serde(default)]
    pub ppr_seed_pool: Option<u32>,
    /// Edge-type weight overrides for PPR. Keys are case-insensitive
    /// edge type names; values are non-negative weights. Edge types
    /// not listed here fall back to the built-in defaults.
    #[serde(default)]
    pub ppr_edge_weights: Option<HashMap<String, f64>>,
}

#[derive(Debug, Serialize)]
struct IngestStatsJson {
    nodes_written: usize,
    edges_written: usize,
    embedding_calls: usize,
}

#[derive(Debug, Serialize)]
struct TraversalJson {
    nodes: Vec<TraversalNodeJson>,
    edges: Vec<TraversalEdgeJson>,
}

#[derive(Debug, Serialize)]
struct TraversalNodeJson {
    id: String,
    name: String,
    node_type: String,
    file: String,
    distance: u32,
}

#[derive(Debug, Serialize)]
struct TraversalEdgeJson {
    source: String,
    target: String,
    edge_type: String,
}

#[derive(Debug, Serialize)]
struct SemanticHitJson {
    id: String,
    name: String,
    node_type: String,
    file: String,
    start_line: u32,
    end_line: u32,
    description: String,
    distance: f32,
}

fn build_embedder(opts: Option<&EmbedderOptions>) -> Result<Embedder, String> {
    let want_remote = opts
        .and_then(|o| o.base_url.as_deref())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let cfg = match opts {
        None => EmbedderConfig::default(),
        Some(o) => EmbedderConfig::with_overrides(
            o.base_url.clone(),
            o.api_key.clone(),
            o.model.clone(),
            o.embedding_dim.map(|v| v as usize),
            o.batch_size.map(|v| v as usize),
            o.timeout_secs.map(|v| v as u64),
        ),
    };
    if want_remote {
        Embedder::remote(cfg).map_err(|e| e.to_string())
    } else {
        Embedder::local(cfg).map_err(|e| e.to_string())
    }
}

fn parse_embedder_options(json: Option<String>) -> Result<Option<EmbedderOptions>, String> {
    match json {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| e.to_string()),
    }
}

/// Combines option-parse + embedder build with their NAPI error mapping.
/// Used by every public function that needs an embedder.
fn embedder_from_json(json: Option<String>) -> napi::Result<Embedder> {
    let opts = parse_embedder_options(json)
        .map_err(|e| napi::Error::from_reason(format!("invalid embedder options: {}", e)))?;
    build_embedder(opts.as_ref())
        .map_err(|e| napi::Error::from_reason(format!("embedder init failed: {}", e)))
}

/// Open a OverGraph connection with NAPI error mapping. Connections are
/// cheap (memory-mapped) so each call gets a fresh one.
async fn open_db(path: &str) -> napi::Result<Db> {
    Db::open(path)
        .await
        .map_err(|e| napi::Error::from_reason(format!("failed to open db: {}", e)))
}

/// Ingest a JSON graph (the output of `buildGraph`) into a OverGraph
/// instance at `db_path`. Returns ingest stats as JSON.
///
/// If the caller did not supply `embedderOptions.embeddingDim`, this
/// probes the embedding endpoint once and uses the discovered dim so
/// users can swap models without recompiling or knowing the dim ahead
/// of time. The discovered dim is then validated against (or persisted
/// to) the DB sidecar manifest by `Db::open_or_create`.
#[napi]
pub async fn db_ingest(
    graph_json: String,
    db_path: String,
    embedder_options: Option<String>,
) -> napi::Result<String> {
    let graph: GraphData = serde_json::from_str(&graph_json)
        .map_err(|e| napi::Error::from_reason(format!("invalid graph JSON: {}", e)))?;
    let opts = parse_embedder_options(embedder_options)
        .map_err(|e| napi::Error::from_reason(format!("invalid embedder options: {}", e)))?;
    let dim_was_explicit = opts
        .as_ref()
        .and_then(|o| o.embedding_dim)
        .is_some();
    let mut embedder = build_embedder(opts.as_ref())
        .map_err(|e| napi::Error::from_reason(format!("embedder init failed: {}", e)))?;
    if !dim_was_explicit {
        let probed = embedder
            .probe_dim()
            .await
            .map_err(|e| napi::Error::from_reason(format!("embedder dim probe failed: {}", e)))?;
        if probed != embedder.config().dim {
            embedder.set_dim(probed);
        }
    }
    let dim = embedder.config().dim as u32;
    let db = Db::open_or_create(&db_path, dim)
        .await
        .map_err(|e| napi::Error::from_reason(format!("failed to open db: {}", e)))?;

    let stats = ingest_graph(&db, &embedder, &graph)
        .await
        .map_err(|e| napi::Error::from_reason(format!("ingest failed: {}", e)))?;

    let out = IngestStatsJson {
        nodes_written: stats.nodes_written,
        edges_written: stats.edges_written,
        embedding_calls: stats.embedding_calls,
    };
    serde_json::to_string(&out)
        .map_err(|e| napi::Error::from_reason(format!("serialize stats: {}", e)))
}

/// Phase 4 entry point: end-to-end GraphRAG retrieval.
/// `options_json` must include at least `{ "query": "..." }`.
#[napi]
pub async fn db_hybrid_search(
    db_path: String,
    options_json: String,
    embedder_options: Option<String>,
) -> napi::Result<String> {
    let opts: SearchKbJsonOptions = serde_json::from_str(&options_json)
        .map_err(|e| napi::Error::from_reason(format!("invalid options: {}", e)))?;

    let embedder = embedder_from_json(embedder_options)?;
    let db = open_db(&db_path).await?;

    let repo_root_buf: PathBuf = opts
        .repo_root
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let edge_types = opts.edge_types.clone();
    let where_clause = opts.where_clause.clone();
    let direction = opts
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    let ppr_edge_weights: Option<HashMap<String, f32>> = opts
        .ppr_edge_weights
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), *v as f32)).collect());

    let mut kb_opts = SearchKbOptions::new(&opts.query, repo_root_buf.as_path());
    if let Some(k) = opts.k {
        kb_opts.k = k as usize;
    }
    if let Some(h) = opts.hops {
        kb_opts.hops = h;
    }
    kb_opts.edge_types = edge_types.as_deref();
    kb_opts.direction = direction;
    if let Some(c) = opts.max_chars {
        kb_opts.max_chars = c as usize;
    }
    if let Some(l) = opts.mmr_lambda {
        kb_opts.mmr_lambda = l as f32;
    }
    kb_opts.where_clause = where_clause.as_deref();
    if let Some(s) = opts.include_snippets {
        kb_opts.include_snippets = s;
    }
    if let Some(s) = opts.strategy.as_deref() {
        kb_opts.strategy = RankStrategy::from_str_lossy(s);
    }
    if let Some(p) = opts.ppr_restart_prob {
        kb_opts.ppr_restart_prob = p as f32;
    }
    if let Some(m) = opts.ppr_max_iter {
        kb_opts.ppr_max_iter = m as usize;
    }
    if let Some(p) = opts.ppr_seed_pool {
        kb_opts.ppr_seed_pool = p as usize;
    }
    kb_opts.ppr_edge_weights = ppr_edge_weights;

    let result: RankedContext = run_search_kb(&db, &embedder, kb_opts)
        .await
        .map_err(|e| napi::Error::from_reason(format!("search_kb failed: {}", e)))?;

    serde_json::to_string(&result)
        .map_err(|e| napi::Error::from_reason(format!("serialize result: {}", e)))
}

/// Pure vector search. Useful when the caller already has a seed and
/// just wants nearest neighbours.
#[napi]
pub async fn db_semantic_search(
    db_path: String,
    query: String,
    k: u32,
    where_clause: Option<String>,
    embedder_options: Option<String>,
) -> napi::Result<String> {
    let embedder = embedder_from_json(embedder_options)?;
    let db = open_db(&db_path).await?;

    let hits = match where_clause.as_deref() {
        Some(w) => {
            crate::storage::query::semantic_search_w_where(&db, &embedder, &query, k as usize, w)
                .await
                .map_err(|e| napi::Error::from_reason(format!("search failed: {}", e)))?
        }
        None => run_semantic_search(&db, &embedder, &query, k as usize)
            .await
            .map_err(|e| napi::Error::from_reason(format!("search failed: {}", e)))?,
    };

    let json: Vec<SemanticHitJson> = hits
        .into_iter()
        .map(|h| SemanticHitJson {
            id: h.node.id,
            name: h.node.name,
            node_type: h.node.node_type,
            file: h.node.file,
            start_line: h.node.start_line,
            end_line: h.node.end_line,
            description: h.node.description,
            distance: h.distance,
        })
        .collect();

    serde_json::to_string(&json)
        .map_err(|e| napi::Error::from_reason(format!("serialize hits: {}", e)))
}

/// DB-backed graph traversal with direction + edge-type filter.
/// `direction` accepts: "outbound" (default), "inbound", "both".
#[napi]
pub async fn db_traverse(
    db_path: String,
    start_node_ids: Vec<String>,
    hops: u32,
    edge_types: Option<Vec<String>>,
    direction: Option<String>,
) -> napi::Result<String> {
    let db = open_db(&db_path).await?;

    let dir = direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Outbound);

    let result = run_traverse_filtered(&db, &start_node_ids, hops, edge_types.as_deref(), dir)
        .await
        .map_err(|e| napi::Error::from_reason(format!("traverse failed: {}", e)))?;

    let nodes: Vec<TraversalNodeJson> = result
        .nodes
        .iter()
        .map(|n| TraversalNodeJson {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: n.node_type.clone(),
            file: n.file.clone(),
            distance: result.distances.get(&n.id).copied().unwrap_or(0),
        })
        .collect();
    let edges: Vec<TraversalEdgeJson> = result
        .edges
        .iter()
        .map(|e| TraversalEdgeJson {
            source: e.source.clone(),
            target: e.target.clone(),
            edge_type: e.edge_type.clone(),
        })
        .collect();

    serde_json::to_string(&TraversalJson { nodes, edges })
        .map_err(|e| napi::Error::from_reason(format!("serialize traversal: {}", e)))
}

/// Probe the embedding endpoint. Returns `"ok"` on success, throws on
/// failure with the upstream error message.
#[napi]
pub async fn ping_embedder(embedder_options: Option<String>) -> napi::Result<String> {
    let embedder = embedder_from_json(embedder_options)?;
    embedder
        .ping()
        .await
        .map_err(|e| napi::Error::from_reason(format!("ping failed: {}", e)))?;
    Ok("ok".to_string())
}

// Suppress dead_code on serializable shapes that exist only for the JSON
// boundary - they're "used" by serde via reflection.
#[allow(dead_code)]
fn _shape_witnesses() -> (Option<ContextItem>, Option<RankedContext>) {
    (None, None)
}
