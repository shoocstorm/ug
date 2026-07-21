//! High-level query API on top of [`super::db`].
//!
//! Exposes the operations from docs/GRAPH-STORAGE.md plus the Phase 4
//! GraphRAG composition: seed search -> expand -> rerank -> assemble.
//!
//!   1. semantic_search - vector-only nearest-neighbour
//!   2. semantic_search_w_where   - vector + SQL filter (e.g. `node_type = 'Function'`)
//!   3. traverse        - BFS over the edges table from a seed node id
//!   4. rerank          - Maximal Marginal Relevance (MMR) to balance diversity and
//!     relevance
//!   5. assemble        - GraphRAG query composition: seeds → expand → rerank → final ranked list
//!   6. code_snippet    - retrieval helper that returns code snippets from nodes

use crate::storage::db::{EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use crate::storage::ppr::run_ppr;
use crate::storage::store::{KnowledgeStore, NodeFilter};
use crate::storage::text::build_sparse_keyword_vector;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Re-exported for back-compat — `Direction` now lives in `store.rs`.
pub use crate::storage::store::Direction;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub node: NodeRow,
    pub distance: f32,
}

#[derive(Debug, Default)]
pub struct TraversalResult {
    pub nodes: Vec<NodeRow>,
    pub edges: Vec<EdgeRow>,
    pub distances: HashMap<String, u32>,
}

/// Vector search by free-text query. Embeds `query` once with `embedder`,
/// then asks the backend for the top-`k` nearest node rows. Works
/// against any [`KnowledgeStore`].
pub async fn semantic_search(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let raw = store.vector_search(query_vec, k, None).await?;
    Ok(raw
        .into_iter()
        .map(|(node, distance)| SearchHit { node, distance })
        .collect())
}

/// Like [`semantic_search`] but with a `node_type` filter parsed from
/// the legacy SQL-flavored `WHERE` argument. Anything the parser
/// doesn't recognize degrades to no filter (matches pre-trait
/// OverGraph behavior — see `MIGRATION-OVERGRAPH §6 Q1`).
pub async fn semantic_search_w_where(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    query: &str,
    k: usize,
    where_clause: &str,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let filter = NodeFilter::from_legacy_where(where_clause);
    let raw = store.vector_search(query_vec, k, filter.as_ref()).await?;
    Ok(raw
        .into_iter()
        .map(|(node, distance)| SearchHit { node, distance })
        .collect())
}

/// Reciprocal Rank Fusion of dense + sparse results. Dispatches via
/// the [`KnowledgeStore`] trait — the OverGraph backend uses its native
/// RRF, the Neo4j backend fuses vector + full-text in app code.
///
/// `distance = -score` is preserved for downstream consumers that sort
/// ascending — lower `distance` means better hit.
pub async fn rrf_search(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    query: &str,
    k: usize,
    where_clause: Option<&str>,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let sparse = build_sparse_keyword_vector(query);
    let pool = (k * 4).max(20);
    let filter = where_clause.and_then(NodeFilter::from_legacy_where);
    let hits = store
        .hybrid_search(query_vec, sparse, query, pool, filter.as_ref())
        .await?;
    Ok(hits
        .into_iter()
        .take(k)
        .map(|(node, score)| SearchHit {
            node,
            distance: -score,
        })
        .collect())
}

/// Maximal Marginal Relevance rerank. `lambda` in [0, 1] balances
/// relevance (vs. query) against diversity (vs. already-picked items).
/// Uses the stored row vectors so no extra embedding calls are needed.
pub fn mmr_rerank(
    query_vec: &[f32],
    candidates: Vec<SearchHit>,
    k: usize,
    lambda: f32,
) -> Vec<SearchHit> {
    if candidates.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut remaining: Vec<SearchHit> = candidates;
    let mut picked: Vec<SearchHit> = Vec::new();

    let lambda = lambda.clamp(0.0, 1.0);

    while picked.len() < k && !remaining.is_empty() {
        let mut best_idx: usize = 0;
        let mut best_score: f32 = f32::MIN;

        for (i, cand) in remaining.iter().enumerate() {
            let rel = cosine(&cand.node.vector, query_vec);
            let div = picked
                .iter()
                .map(|p| cosine(&cand.node.vector, &p.node.vector))
                .fold(f32::MIN, f32::max);
            let div = if div == f32::MIN { 0.0 } else { div };
            let score = lambda * rel - (1.0 - lambda) * div;
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        picked.push(remaining.swap_remove(best_idx));
    }

    picked
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// BFS up to `max_hops` from `start_id` over the `edges` table.
/// Outbound-only, no edge-type filter. Kept for backwards compatibility.
pub async fn traverse(
    store: &dyn KnowledgeStore,
    start_id: &str,
    max_hops: u32,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    traverse_filtered(
        store,
        &[start_id.to_string()],
        max_hops,
        None,
        Direction::Outbound,
    )
    .await
}

/// Generalised graph expansion. Walks `direction` for up to `max_hops`
/// from each id in `start_ids`, optionally restricted to specific edge
/// types. Calls the trait's `traverse` per seed and merges results,
/// deduplicating shared neighbours.
pub async fn traverse_filtered(
    store: &dyn KnowledgeStore,
    start_ids: &[String],
    max_hops: u32,
    edge_types: Option<&[String]>,
    direction: Direction,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut nodes: Vec<NodeRow> = Vec::new();
    let mut edges: Vec<EdgeRow> = Vec::new();

    for start_id in start_ids {
        let page = store
            .traverse(start_id, max_hops, edge_types, direction)
            .await?;

        for tn in page.nodes {
            // Track depth even if we've already added the node — closer wins.
            distances
                .entry(tn.row.id.clone())
                .and_modify(|cur| {
                    if tn.depth < *cur {
                        *cur = tn.depth;
                    }
                })
                .or_insert(tn.depth);
            if visited.insert(tn.row.id.clone()) {
                nodes.push(tn.row);
            }
        }
        edges.extend(page.edges);
    }

    Ok(TraversalResult {
        nodes,
        edges,
        distances,
    })
}

/// Final ranked context returned from [`search_kb`]. Each item represents a
/// node selected for the agent prompt, with the actual code slice attached
/// when the node has line ranges and the file is readable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextItem {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub description: String,
    pub distance: f32,
    pub hop: u32,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RankedContext {
    pub query: String,
    pub items: Vec<ContextItem>,
    pub total_chars: usize,
    pub seed_id: Option<String>,
}

/// Read `start_line..=end_line` from `file` (1-indexed line numbers, both
/// inclusive). Returns `None` for missing files, unreadable files, or zero
/// line ranges. `repo_root` is prepended when the path is relative.
pub fn read_snippet(
    repo_root: &Path,
    file: &str,
    start_line: u32,
    end_line: u32,
) -> Option<String> {
    if file.is_empty() || start_line == 0 || end_line == 0 || end_line < start_line {
        return None;
    }
    let abs: PathBuf = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        repo_root.join(file)
    };
    let content = std::fs::read_to_string(&abs).ok()?;
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as u32;
        if n < start_line {
            continue;
        }
        if n > end_line {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Ranking strategy for the candidate pool produced by seed search +
/// graph context.
///
/// PPR is the only strategy the public surfaces expose — the MCP tool, the
/// CLI help and the HTTP docs no longer offer a choice. `Mmr` survives
/// because [`search_kb`] selects it automatically for backends without
/// native PPR (Neo4j without the GDS plugin); it is a fallback, not a user
/// option. The `--strategy` flag still parses for operator debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankStrategy {
    Ppr,
    Mmr,
}

impl RankStrategy {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "mmr" => RankStrategy::Mmr,
            _ => RankStrategy::Ppr,
        }
    }
}

/// Configuration for [`search_kb`]. Keeping this as a struct keeps the
/// NAPI signature small (one JSON blob) and allows future fields without
/// breaking callers.
#[derive(Debug, Clone)]
pub struct SearchKbOptions<'a> {
    pub query: &'a str,
    pub k: usize,
    pub hops: u32,
    pub edge_types: Option<&'a [String]>,
    pub direction: Direction,
    pub max_chars: usize,
    pub mmr_lambda: f32,
    pub repo_root: &'a Path,
    pub where_clause: Option<&'a str>,
    pub include_snippets: bool,
    pub strategy: RankStrategy,
    /// PPR teleport probability. Higher = stay closer to seeds; lower =
    /// let structural centrality dominate. Ignored unless
    /// `strategy == Ppr`.
    pub ppr_restart_prob: f32,
    /// PPR power-iteration cap. Ignored unless `strategy == Ppr`.
    pub ppr_max_iter: usize,
    /// Override the default edge-type weight table (id is
    /// case-insensitive). `None` = use defaults from
    /// [`crate::storage::ppr::default_edge_type_weights`].
    pub ppr_edge_weights: Option<HashMap<String, f32>>,
    /// Number of seeds passed into the PPR personalization vector. We
    /// use a wider seed pool than `k` so a single noisy hit doesn't
    /// dominate the random walker. Ignored unless `strategy == Ppr`.
    pub ppr_seed_pool: usize,
}

impl<'a> SearchKbOptions<'a> {
    pub fn new(query: &'a str, repo_root: &'a Path) -> Self {
        Self {
            query,
            k: 8,
            hops: 2,
            edge_types: None,
            direction: Direction::Both,
            max_chars: 12_000,
            mmr_lambda: 0.6,
            repo_root,
            where_clause: None,
            include_snippets: true,
            strategy: RankStrategy::Ppr,
            ppr_restart_prob: 0.15,
            ppr_max_iter: 30,
            ppr_edge_weights: None,
            ppr_seed_pool: 16,
        }
    }
}

/// [Advanced RAG Search] Phase 4 GraphRAG: seed search -> PPR ranking ->
/// snippet attachment -> token-budgeted assembly. Returns a JSON-friendly
/// [`RankedContext`].
///
/// Backends without native PPR (Neo4j without GDS) silently fall back to
/// MMR with a single warning log line — callers don't need to opt in, and
/// that fallback is the only reason [`RankStrategy::Mmr`] still exists.
pub async fn search_kb(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    let strategy = match opts.strategy {
        RankStrategy::Ppr if !store.supports_native_ppr() => {
            tracing::warn!(
                backend = store.backend_name(),
                "PPR strategy requested but backend lacks native PPR; falling back to MMR"
            );
            RankStrategy::Mmr
        }
        s => s,
    };
    match strategy {
        RankStrategy::Ppr => search_kb_ppr(store, embedder, opts).await,
        RankStrategy::Mmr => search_kb_mmr(store, embedder, opts).await,
    }
}

/// Default path: RRF seeds become a personalization vector for
/// Personalized PageRank over the full edge graph. PPR scores replace
/// both BFS expansion and MMR reranking — a single ranking that fuses
/// seed proximity with graph-wide centrality.
async fn search_kb_ppr(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    // 1. RRF seeds. Wider pool than `k` so a noisy top-1 doesn't
    //    dominate the personalization vector.
    let seed_pool = opts.ppr_seed_pool.max(opts.k.max(1));
    let seeds = rrf_search(store, embedder, opts.query, seed_pool, opts.where_clause).await?;
    let seed_id = seeds.first().map(|h| h.node.id.clone());

    // 2. Personalization vector: RRF score (stored as -score in
    //    `distance`) translated back to positive mass. Negate, floor
    //    at zero, fall back to rank-decayed weights when distances are
    //    zero (e.g. FTS-only path).
    let mut seed_mass: HashMap<String, f32> = HashMap::new();
    let mut any_positive = false;
    for h in seeds.iter() {
        let mass = (-h.distance).max(0.0);
        if mass > 0.0 {
            any_positive = true;
        }
        seed_mass.entry(h.node.id.clone()).or_insert(mass);
    }
    if !any_positive {
        for (rank, h) in seeds.iter().enumerate() {
            seed_mass.insert(h.node.id.clone(), 1.0 / (rank as f32 + 1.0));
        }
    }

    // 3. PPR — uniform seeds (see MIGRATION-OVERGRAPH §3.4 for the
    //    weighted-personalization deferral). `seed_mass` keys are still
    //    used downstream as the "is this a seed?" set.
    let take = (opts.k * 4).max(opts.k.max(1));
    let seed_strings: Vec<String> = seed_mass.keys().cloned().collect();
    let edge_types_owned: Option<Vec<String>> =
        opts.edge_types.map(|v| v.iter().cloned().collect());
    let ranked_pairs = run_ppr(
        store,
        &seed_strings,
        opts.direction,
        edge_types_owned.as_deref(),
        opts.ppr_restart_prob,
        opts.ppr_max_iter,
        Some(take),
    )
    .await?;

    // 4. Hydrate the top-N node rows. Take a generous slice so the
    //    char budget stage has room to discard sparse/empty entries.
    let top_ids: Vec<String> = ranked_pairs
        .iter()
        .take(take)
        .map(|(id, _)| id.clone())
        .collect();
    let score_by_id: HashMap<String, f32> = ranked_pairs.into_iter().collect();
    let nodes = store.nodes_by_ids(&top_ids).await?;
    let nodes_by_id: HashMap<String, NodeRow> =
        nodes.into_iter().map(|n| (n.id.clone(), n)).collect();

    let mut items: Vec<ContextItem> = Vec::new();
    let mut total_chars: usize = 0;
    for id in top_ids.iter() {
        let Some(n) = nodes_by_id.get(id) else {
            continue;
        };
        let score = score_by_id.get(id).copied().unwrap_or(0.0);
        let snippet = if opts.include_snippets {
            read_snippet(opts.repo_root, &n.file, n.start_line, n.end_line)
        } else {
            None
        };
        // `hop` field kept for backwards compatibility: 0 if seed,
        // else 1 (PPR has no hop concept; seed/non-seed is the most
        // useful signal we can preserve here).
        let hop: u32 = if seed_mass.contains_key(id) { 0 } else { 1 };
        let item = ContextItem {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: n.node_type.clone(),
            file: n.file.clone(),
            start_line: n.start_line,
            end_line: n.end_line,
            description: n.description.clone(),
            // Surface PPR score as a negated "distance" so existing
            // downstream consumers (sort-ascending) keep working.
            distance: -score,
            hop,
            snippet,
        };
        let item_chars = item.snippet.as_ref().map(|s| s.len()).unwrap_or(0)
            + item.description.len()
            + item.name.len();
        if total_chars + item_chars > opts.max_chars && !items.is_empty() {
            break;
        }
        total_chars += item_chars;
        items.push(item);
        if items.len() >= opts.k {
            break;
        }
    }

    Ok(RankedContext {
        query: opts.query.to_string(),
        items,
        total_chars,
        seed_id,
    })
}

/// Legacy path: seed -> BFS expand -> MMR rerank. Kept available via
/// `RankStrategy::Mmr` for callers who want diversity-first behavior.
async fn search_kb_mmr(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    // 1. Seed: RRF over vector + FTS, optionally filtered.
    let seeds = rrf_search(store, embedder, opts.query, opts.k.max(1), opts.where_clause).await?;
    let seed_id = seeds.first().map(|h| h.node.id.clone());

    // 2. Expand: walk the graph from each seed.
    let seed_ids: Vec<String> = seeds.iter().map(|h| h.node.id.clone()).collect();
    let traversal =
        traverse_filtered(store, &seed_ids, opts.hops, opts.edge_types, opts.direction).await?;

    // 3. Build candidate pool: union of seed hits + traversal nodes.
    let seed_dist: HashMap<String, f32> = seeds
        .iter()
        .map(|h| (h.node.id.clone(), h.distance))
        .collect();

    let mut candidates: Vec<SearchHit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for h in seeds.iter() {
        if seen.insert(h.node.id.clone()) {
            candidates.push(h.clone());
        }
    }
    for n in traversal.nodes.iter() {
        if seen.insert(n.id.clone()) {
            let hop = traversal.distances.get(&n.id).copied().unwrap_or(opts.hops);
            let dist = seed_dist
                .get(&n.id)
                .copied()
                .unwrap_or((hop as f32 * 0.1).max(0.05));
            candidates.push(SearchHit {
                node: n.clone(),
                distance: dist,
            });
        }
    }

    // 4. Rerank with MMR using the original query embedding for relevance.
    let query_vec = embedder
        .embed(&[opts.query.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let take = candidates.len().min(opts.k * 4).max(opts.k);
    let reranked = mmr_rerank(&query_vec, candidates, take, opts.mmr_lambda);

    // 5. Attach snippets and apply char budget.
    let mut items: Vec<ContextItem> = Vec::new();
    let mut total_chars: usize = 0;
    for hit in reranked {
        let hop = traversal.distances.get(&hit.node.id).copied().unwrap_or(0);
        let snippet = if opts.include_snippets {
            read_snippet(
                opts.repo_root,
                &hit.node.file,
                hit.node.start_line,
                hit.node.end_line,
            )
        } else {
            None
        };

        let item = ContextItem {
            id: hit.node.id.clone(),
            name: hit.node.name.clone(),
            node_type: hit.node.node_type.clone(),
            file: hit.node.file.clone(),
            start_line: hit.node.start_line,
            end_line: hit.node.end_line,
            description: hit.node.description.clone(),
            distance: hit.distance,
            hop,
            snippet,
        };

        let item_chars = item.snippet.as_ref().map(|s| s.len()).unwrap_or(0)
            + item.description.len()
            + item.name.len();
        if total_chars + item_chars > opts.max_chars && !items.is_empty() {
            break;
        }
        total_chars += item_chars;
        items.push(item);
        if items.len() >= opts.k {
            break;
        }
    }

    Ok(RankedContext {
        query: opts.query.to_string(),
        items,
        total_chars,
        seed_id,
    })
}
