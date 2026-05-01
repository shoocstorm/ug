//! High-level query API on top of [`super::db`].
//!
//! Exposes the operations from docs/GRAPH-STORAGE.md plus the Phase 4
//! GraphRAG composition: seed search -> expand -> rerank -> assemble.
//!
//!   1. semantic_search - vector-only nearest-neighbour
//!   2. hybrid_search   - vector + SQL filter (e.g. `node_type = 'Function'`)
//!   3. traverse        - BFS over the edges table from a seed node id
//!   4. rerank          - Maximal Marginal Relevance (MMR) to balance diversity and
//!     relevance
//!   5. assemble        - GraphRAG query composition: seeds → expand → rerank → final ranked list
//!   6. code_snippet    - retrieval helper that returns code snippets from nodes

use crate::storage::db::{
    edges_from, edges_to, fts_search, nodes_by_ids, vector_search, Db, EdgeRow, NodeRow,
};
use crate::storage::embed::Embedder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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

/// Direction of edge expansion during graph traversal.
/// - `Outbound` walks `source -> target` (e.g. who does X call?).
/// - `Inbound`  walks `target -> source` (e.g. who calls X?).
/// - `Both`     unions the two; useful for "everything related to X".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outbound,
    Inbound,
    Both,
}

impl Direction {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "in" | "inbound" | "incoming" => Direction::Inbound,
            "both" | "all" | "any" => Direction::Both,
            _ => Direction::Outbound,
        }
    }
}

/// Vector search by free-text query. Embeds `query` once with `embedder`,
/// then asks LanceDB for the top-`k` nearest node rows.
pub async fn semantic_search(
    db: &Db,
    embedder: &Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let raw = vector_search(db, query_vec, k, None).await?;
    Ok(raw
        .into_iter()
        .map(|(node, distance)| SearchHit { node, distance })
        .collect())
}

/// Like [`semantic_search`] but with an additional SQL `WHERE` clause
/// applied during the vector query.
pub async fn hybrid_search(
    db: &Db,
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
    let raw = vector_search(db, query_vec, k, Some(where_clause)).await?;
    Ok(raw
        .into_iter()
        .map(|(node, distance)| SearchHit { node, distance })
        .collect())
}

/// Reciprocal Rank Fusion of vector + FTS results. Standard hybrid recipe:
/// each list contributes `1 / (k_const + rank_i)` per shared id, results
/// are sorted by combined score. `k_const = 60` is the canonical default
/// from the original RRF paper.
///
/// We expose the fused list as `SearchHit`s with `distance = -score` so
/// downstream code that sorts ascending by distance still gets the right
/// order (lower `distance` = better hit).
pub async fn rrf_search(
    db: &Db,
    embedder: &Embedder,
    query: &str,
    k: usize,
    where_clause: Option<&str>,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    const RRF_K: f32 = 60.0;
    let pool = (k * 4).max(20);

    // Vector half. Embed once.
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let vec_hits = vector_search(db, query_vec, pool, where_clause).await?;

    // FTS half. We swallow its error so a missing FTS index degrades to
    // vector-only instead of failing the whole query.
    let fts_hits = match fts_search(db, query, pool, where_clause).await {
        Ok(rows) => rows,
        Err(_) => Vec::new(),
    };

    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut keep: HashMap<String, NodeRow> = HashMap::new();

    for (rank, (node, _)) in vec_hits.iter().enumerate() {
        *scores.entry(node.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        keep.entry(node.id.clone()).or_insert_with(|| node.clone());
    }
    for (rank, node) in fts_hits.iter().enumerate() {
        *scores.entry(node.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        keep.entry(node.id.clone()).or_insert_with(|| node.clone());
    }

    let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(k);

    Ok(ranked
        .into_iter()
        .filter_map(|(id, score)| {
            keep.remove(&id).map(|node| SearchHit {
                node,
                distance: -score,
            })
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
    db: &Db,
    start_id: &str,
    max_hops: u32,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    traverse_filtered(
        db,
        &[start_id.to_string()],
        max_hops,
        None,
        Direction::Outbound,
    )
    .await
}

/// Generalised graph expansion. Walks `direction` for up to `max_hops`
/// from each id in `start_ids`, optionally restricted to specific edge
/// types (case-insensitive match against `EdgeRow.edge_type`).
///
/// One LanceDB query per hop per direction; the visited set bounds total
/// work. Final node objects are rehydrated in a single batched lookup.
pub async fn traverse_filtered(
    db: &Db,
    start_ids: &[String],
    max_hops: u32,
    edge_types: Option<&[String]>,
    direction: Direction,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut frontier: Vec<String> = Vec::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();

    let allow_set: Option<HashSet<String>> =
        edge_types.map(|v| v.iter().map(|s| s.to_ascii_lowercase()).collect());

    for id in start_ids {
        if visited.insert(id.clone()) {
            distances.insert(id.clone(), 0);
            frontier.push(id.clone());
        }
    }

    for hop in 0..max_hops {
        let mut next: Vec<String> = Vec::new();
        for node_id in &frontier {
            let edges = collect_edges(db, node_id, direction).await?;
            for e in edges {
                if let Some(ref allow) = allow_set {
                    if !allow.contains(&e.edge_type.to_ascii_lowercase()) {
                        continue;
                    }
                }
                let neighbour = neighbour_id(&e, node_id, direction);
                if !visited.contains(&neighbour) {
                    visited.insert(neighbour.clone());
                    distances.insert(neighbour.clone(), hop + 1);
                    next.push(neighbour);
                }
                all_edges.push(e);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    let ids: Vec<String> = visited.into_iter().collect();
    let nodes = nodes_by_ids(db, &ids).await?;
    Ok(TraversalResult {
        nodes,
        edges: all_edges,
        distances,
    })
}

async fn collect_edges(
    db: &Db,
    node_id: &str,
    direction: Direction,
) -> Result<Vec<EdgeRow>, Box<dyn std::error::Error + Send + Sync>> {
    match direction {
        Direction::Outbound => Ok(edges_from(db, node_id).await?),
        Direction::Inbound => Ok(edges_to(db, node_id).await?),
        Direction::Both => {
            let mut out = edges_from(db, node_id).await?;
            let inn = edges_to(db, node_id).await?;
            out.extend(inn);
            Ok(out)
        }
    }
}

fn neighbour_id(edge: &EdgeRow, current: &str, direction: Direction) -> String {
    match direction {
        Direction::Outbound => edge.target.clone(),
        Direction::Inbound => edge.source.clone(),
        Direction::Both => {
            if edge.source == current {
                edge.target.clone()
            } else {
                edge.source.clone()
            }
        }
    }
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
        }
    }
}

/// Phase 4 GraphRAG: seed search -> graph expansion -> rerank ->
/// snippet attachment -> token-budgeted assembly. Returns a JSON-friendly
/// [`RankedContext`].
pub async fn search_kb(
    db: &Db,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    // 1. Seed: RRF over vector + FTS, optionally filtered.
    let seeds = rrf_search(db, embedder, opts.query, opts.k.max(1), opts.where_clause).await?;
    let seed_id = seeds.first().map(|h| h.node.id.clone());

    // 2. Expand: walk the graph from each seed.
    let seed_ids: Vec<String> = seeds.iter().map(|h| h.node.id.clone()).collect();
    let traversal =
        traverse_filtered(db, &seed_ids, opts.hops, opts.edge_types, opts.direction).await?;

    // 3. Build candidate pool: union of seed hits + traversal nodes.
    // Inherit distances from the seed hits where available; otherwise
    // derive a synthetic distance from hop count so the reranker has a
    // reasonable relevance signal even for nodes pulled in by expansion.
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
            // Synthetic distance: each hop costs 0.1, floor at 0.05.
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
