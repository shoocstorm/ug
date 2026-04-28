//! High-level query API on top of [`super::db`].
//!
//! Exposes the three operations from docs/GRAPH-STORAGE.md:
//!   1. semantic_search - vector-only nearest-neighbour
//!   2. hybrid_search   - vector + SQL filter (e.g. `node_type = 'Function'`)
//!   3. traverse        - BFS over the edges table from a seed node id

use crate::storage::db::{edges_from, nodes_by_ids, vector_search, Db, EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
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
/// applied during the vector query (LanceDB pushes this into the index
/// scan, so it is more efficient than post-filtering).
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

/// BFS up to `max_hops` from `start_id` using the `edges` table.
///
/// One LanceDB query per hop fetches the outbound edges for the current
/// frontier; the visited set bounds total work to `O(reachable_nodes)`.
/// Final node objects are rehydrated in a single batched id-lookup.
pub async fn traverse(
    db: &Db,
    start_id: &str,
    max_hops: u32,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut frontier: Vec<String> = vec![start_id.to_string()];
    let mut all_edges: Vec<EdgeRow> = Vec::new();

    visited.insert(start_id.to_string());
    distances.insert(start_id.to_string(), 0);

    for hop in 0..max_hops {
        let mut next: Vec<String> = Vec::new();
        for node_id in &frontier {
            let edges = edges_from(db, node_id).await?;
            for e in edges {
                if !visited.contains(&e.target) {
                    visited.insert(e.target.clone());
                    distances.insert(e.target.clone(), hop + 1);
                    next.push(e.target.clone());
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
