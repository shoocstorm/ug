//! Drive the embedding + write pipeline for a complete graph.
//!
//! Given an in-memory [`GraphData`], this builds the per-node embedding
//! text, calls the [`Embedder`] in one batched call, and upserts both the
//! nodes and edges tables. Edges are written verbatim - they have no
//! vector column, so no embedding work is needed for them.

use crate::storage::db::{Db, EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use crate::storage::text::{build_node_text, collect_related_names};
use crate::types::GraphData;

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub nodes_written: usize,
    pub edges_written: usize,
    pub embedding_calls: usize,
}

pub async fn ingest_graph(
    db: &Db,
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

    let node_rows: Vec<NodeRow> = graph
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
        .collect();

    let edge_rows: Vec<EdgeRow> = graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            // Synthesize a stable id so re-ingesting the same graph
            // upserts instead of duplicating.
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            EdgeRow {
                id,
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect();

    db.upsert_nodes(&node_rows).await?;
    db.upsert_edges(&edge_rows).await?;

    // Best-effort: attempt to create vector + FTS indexes once we have
    // enough rows for them to be useful. Both are no-ops on tiny tables
    // (IvfPq needs a training minimum, FTS is happy at any size). Errors
    // are swallowed because the table is queryable without indexes;
    // logging them here would pollute test output.
    let _ = maybe_create_indexes(db, node_rows.len()).await;

    Ok(IngestStats {
        nodes_written: node_rows.len(),
        edges_written: edge_rows.len(),
        embedding_calls: 1,
    })
}

/// Vector indexing is worthwhile once a table has more rows than this.
/// Below it, scan latency is already low and IvfPq training tends to
/// fail. The exact threshold is approximate; OverGraph picks the index
/// type via `Index::Auto`, so the only thing we control is whether to
/// even try.
const MIN_ROWS_FOR_VECTOR_INDEX: usize = 256;

async fn maybe_create_indexes(
    db: &crate::storage::db::Db,
    last_write: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Use the latest row count; the table may already have rows from a
    // previous ingest plus the rows we just wrote.
    let total = db.count_nodes().await.unwrap_or(last_write);
    if total >= MIN_ROWS_FOR_VECTOR_INDEX {
        let _ = db.try_create_vector_index().await;
    }
    // FTS is cheap; create unconditionally on any non-empty table.
    if total > 0 {
        let _ = db.try_create_fts_index().await;
    }
    Ok(())
}

/// Re-embed and upsert only the subset of nodes whose `id` appears in
/// `changed_ids`. Edges are left untouched - callers are expected to
/// recompute and upsert those separately when topology changes.
pub async fn reembed_nodes(
    db: &Db,
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

    db.upsert_nodes(&rows).await?;

    Ok(IngestStats {
        nodes_written: rows.len(),
        edges_written: 0,
        embedding_calls: 1,
    })
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
