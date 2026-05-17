//! Drive the embedding + write pipeline for a complete graph.
//!
//! Given an in-memory [`GraphData`], this builds the per-node embedding
//! text, calls the [`Embedder`] in one batched call, and upserts both the
//! nodes and edges into one or more backends. Edges have no vector
//! column so no embedding work is needed for them.

use crate::storage::db::{EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use crate::storage::store::{KnowledgeStore, StoreError, StoreSet};
use crate::storage::text::{build_node_text, collect_related_names};
use crate::types::GraphData;

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub nodes_written: usize,
    pub edges_written: usize,
    pub embedding_calls: usize,
}

/// Single-destination ingest. Embeds every node once, then upserts
/// nodes + edges into the backend.
pub async fn ingest_graph(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<IngestStats, Box<dyn std::error::Error + Send + Sync>> {
    let related = collect_related_names(graph);
    let now = current_unix_secs();

    let texts: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            build_node_text(n, names)
        })
        .collect();

    let vectors = embedder.embed(&texts).await?;
    if vectors.len() != graph.nodes.len() {
        return Err(format!(
            "embedder returned {} vectors for {} nodes",
            vectors.len(),
            graph.nodes.len()
        )
        .into());
    }

    let node_rows = build_node_rows(graph, texts, vectors, now);
    let edge_rows = build_edge_rows(graph);

    store.upsert_nodes(&node_rows).await?;
    store.upsert_edges(&edge_rows).await?;

    Ok(IngestStats {
        nodes_written: node_rows.len(),
        edges_written: edge_rows.len(),
        embedding_calls: 1,
    })
}

/// Multi-destination ingest. Embeds the graph once, then fans the
/// upserts out across every backend in `set` (parallel, fail-fast on
/// any backend error). The embedding dim must match across all stores
/// — call [`StoreSet::validate_dims`] before this if you want a clear
/// error early instead of a per-row `BadVector`.
pub async fn ingest_graph_multi(
    set: &StoreSet,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<IngestStats, Box<dyn std::error::Error + Send + Sync>> {
    let related = collect_related_names(graph);
    let now = current_unix_secs();

    let texts: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            build_node_text(n, names)
        })
        .collect();

    let vectors = embedder.embed(&texts).await?;
    if vectors.len() != graph.nodes.len() {
        return Err(format!(
            "embedder returned {} vectors for {} nodes",
            vectors.len(),
            graph.nodes.len()
        )
        .into());
    }

    let node_rows = build_node_rows(graph, texts, vectors, now);
    let edge_rows = build_edge_rows(graph);

    set.upsert_nodes(&node_rows).await?;
    set.upsert_edges(&edge_rows).await?;

    Ok(IngestStats {
        nodes_written: node_rows.len(),
        edges_written: edge_rows.len(),
        embedding_calls: 1,
    })
}

fn build_node_rows(
    graph: &GraphData,
    texts: Vec<String>,
    vectors: Vec<Vec<f32>>,
    now: i64,
) -> Vec<NodeRow> {
    graph
        .nodes
        .iter()
        .zip(texts.into_iter())
        .zip(vectors.into_iter())
        .map(|((n, node_text), vector)| NodeRow {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: format!("{:?}", n.node_type),
            description: n.docstring.clone().unwrap_or_default(),
            file: n.file.clone().unwrap_or_default(),
            start_line: n.start_line.unwrap_or(0),
            end_line: n.end_line.unwrap_or(0),
            last_update_at: now,
            node_text,
            vector,
        })
        .collect()
}

fn build_edge_rows(graph: &GraphData) -> Vec<EdgeRow> {
    graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            EdgeRow {
                id,
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect()
}

/// Re-embed and upsert only the subset of nodes whose `id` appears in
/// `changed_ids`. Edges are left untouched - callers are expected to
/// recompute and upsert those separately when topology changes.
pub async fn reembed_nodes(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    graph: &GraphData,
    changed_ids: &[String],
) -> Result<IngestStats, Box<dyn std::error::Error + Send + Sync>> {
    if changed_ids.is_empty() {
        return Ok(IngestStats::default());
    }
    let related = collect_related_names(graph);
    let now = current_unix_secs();
    let id_set: std::collections::HashSet<&str> = changed_ids.iter().map(|s| s.as_str()).collect();

    let mut texts: Vec<String> = Vec::new();
    let mut targets: Vec<&crate::types::GraphNode> = Vec::new();
    for n in &graph.nodes {
        if !id_set.contains(n.id.as_str()) {
            continue;
        }
        let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
        texts.push(build_node_text(n, names));
        targets.push(n);
    }

    let vectors = embedder.embed(&texts).await?;
    let rows: Vec<NodeRow> = targets
        .iter()
        .zip(texts.into_iter())
        .zip(vectors.into_iter())
        .map(|((n, node_text), vector)| NodeRow {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: format!("{:?}", n.node_type),
            description: n.docstring.clone().unwrap_or_default(),
            file: n.file.clone().unwrap_or_default(),
            start_line: n.start_line.unwrap_or(0),
            end_line: n.end_line.unwrap_or(0),
            last_update_at: now,
            node_text,
            vector,
        })
        .collect();

    store.upsert_nodes(&rows).await?;

    Ok(IngestStats {
        nodes_written: rows.len(),
        edges_written: 0,
        embedding_calls: 1,
    })
}

// Helper kept for backwards compat; allows ingest_graph to skip the
// upsert_edges call when the caller prefers to run them separately.
#[allow(dead_code)]
pub(crate) async fn upsert_only_nodes(
    store: &dyn KnowledgeStore,
    rows: &[NodeRow],
) -> Result<(), StoreError> {
    store.upsert_nodes(rows).await
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
