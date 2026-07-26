//! Drive the embedding + write pipeline for a complete graph.
//!
//! Given an in-memory [`GraphData`], this builds the per-node embedding
//! text, embeds it, and upserts both the nodes and edges into one or
//! more backends. Edges have no vector column so no embedding work is
//! needed for them.
//!
//! # Incremental re-ingest
//!
//! Ingest is idempotent — re-running it over a mostly-unchanged repo is
//! the common case (`ug gen` on an existing project, the KB Manager's
//! re-index button). Embedding every node again on each of those runs is
//! by far the most expensive thing the pipeline does, and almost all of
//! it is redundant.
//!
//! So before embedding anything, [`plan_incremental_ingest`] diffs the
//! incoming graph against what the store already holds and sorts every
//! node into one of three buckets:
//!
//! - **unchanged** — the stored row is identical: no embed, no write.
//! - **reusable** — [`build_node_text`] output is unchanged but some
//!   other column moved (line numbers are the usual one, since they
//!   don't feed the embedding text). Carry the stored vector over and
//!   write the row: no embed.
//! - **to embed** — new node, or its text changed. Full cost.
//!
//! Only the third bucket reaches the embedder.

use crate::storage::db::{EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use crate::storage::store::{KnowledgeStore, NodeKey, StoreError, StoreSet};
use crate::storage::text::{build_node_text, collect_related_names};
use crate::types::{GraphData, GraphNode};
use std::collections::HashMap;

/// How many nodes to read back from the store at a time while planning.
/// Caps peak memory: the fetched rows carry their dense vectors, so an
/// unbounded read of a large graph would hold the entire vector set in
/// memory just to throw most of it away.
const PLAN_FETCH_CHUNK: usize = 2048;

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub nodes_written: usize,
    pub edges_written: usize,
    pub embedding_calls: usize,
    /// Nodes that actually went through the embedder this run.
    pub nodes_embedded: usize,
    /// Nodes whose stored row was already identical — neither embedded
    /// nor written.
    pub nodes_unchanged: usize,
}

/// The work a re-ingest actually has to do, after diffing against the store.
#[derive(Debug, Default)]
pub struct IngestPlan {
    /// Rows to write whose vector was carried over from the store.
    pub reusable: Vec<NodeRow>,
    /// Nodes needing a fresh vector: index into `graph.nodes`, plus the
    /// embedding text already built for it.
    pub to_embed: Vec<(usize, String)>,
    /// Nodes identical to what's stored, so neither embedded nor written.
    pub unchanged: usize,
}

impl IngestPlan {
    /// Fold freshly computed vectors — one per entry of [`to_embed`], in
    /// order — into the rows the plan already resolved, yielding the full
    /// set to upsert.
    ///
    /// Split out from the embedding step so callers that drive the
    /// embedder themselves (e.g. `ug gen`, which renders per-batch
    /// progress) can still share the row assembly.
    ///
    /// [`to_embed`]: IngestPlan::to_embed
    pub fn finish(self, graph: &GraphData, vectors: Vec<Vec<f32>>) -> Result<Vec<NodeRow>, String> {
        if vectors.len() != self.to_embed.len() {
            return Err(format!(
                "embedder returned {} vectors for {} nodes",
                vectors.len(),
                self.to_embed.len()
            ));
        }
        let now = current_unix_secs();
        let mut rows = self.reusable;
        rows.reserve(vectors.len());
        for ((idx, node_text), vector) in self.to_embed.into_iter().zip(vectors) {
            let n = &graph.nodes[idx];
            rows.push(node_row(
                n,
                format!("{:?}", n.node_type),
                node_text,
                vector,
                now,
            ));
        }
        Ok(rows)
    }
}

/// Diff `graph` (with its pre-built `texts`) against `store`.
///
/// `always_write` forces every node into `reusable`/`to_embed` even when
/// the stored row is identical, leaving `unchanged` at 0. Multi-destination
/// ingest needs this: the plan is built against one store, and skipping
/// writes on that basis would let the *other* destinations drift.
///
/// A store that errors on read is not fatal — it just means we cannot
/// prove anything is reusable, so the caller falls back to embedding
/// everything, exactly as it did before this optimization existed.
pub async fn plan_incremental_ingest(
    store: &dyn KnowledgeStore,
    graph: &GraphData,
    texts: &[String],
    always_write: bool,
) -> Result<IngestPlan, StoreError> {
    let mut plan = IngestPlan {
        reusable: Vec::new(),
        to_embed: Vec::new(),
        unchanged: 0,
    };
    let dim = store.embedding_dim() as usize;
    let now = current_unix_secs();
    let mut offset = 0usize;

    for chunk in graph.nodes.chunks(PLAN_FETCH_CHUNK) {
        let keys: Vec<NodeKey> = chunk
            .iter()
            .map(|n| NodeKey {
                id: n.id.clone(),
                node_type: format!("{:?}", n.node_type),
            })
            .collect();
        let mut stored: HashMap<String, NodeRow> = store
            .nodes_for_upsert(&keys)
            .await?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();

        for (i, n) in chunk.iter().enumerate() {
            let idx = offset + i;
            let text = &texts[idx];
            let node_type = &keys[i].node_type;

            // A stored row is only useful if its embedding text still
            // matches (the vector is then still valid for it) and its
            // vector is the width this store expects — a legacy row
            // written before a dim change would otherwise be carried
            // forward and rejected at upsert.
            match stored.remove(&n.id) {
                Some(prev) if prev.node_text == *text && prev.vector.len() == dim => {
                    if !always_write && stored_row_matches(&prev, n, node_type) {
                        plan.unchanged += 1;
                    } else {
                        plan.reusable.push(node_row(
                            n,
                            node_type.clone(),
                            text.clone(),
                            prev.vector,
                            now,
                        ));
                    }
                }
                _ => plan.to_embed.push((idx, text.clone())),
            }
        }
        offset += chunk.len();
    }

    Ok(plan)
}

/// Whether a stored row already carries exactly what we would write for
/// `n`, making the upsert a no-op.
///
/// `last_update_at` is deliberately excluded — it is pure bookkeeping,
/// and nothing reads it for correctness. Leaving it alone actually makes
/// it truthful: it now marks when the node last *changed* rather than
/// when ingest last ran. `vector` is excluded because the caller has
/// already established that `node_text` matches, which is what the
/// vector is derived from.
fn stored_row_matches(prev: &NodeRow, n: &GraphNode, node_type: &str) -> bool {
    prev.name == n.name
        && prev.node_type == node_type
        && prev.description == n.docstring.as_deref().unwrap_or("")
        && prev.file == n.file.as_deref().unwrap_or("")
        && prev.start_line == n.start_line.unwrap_or(0)
        && prev.end_line == n.end_line.unwrap_or(0)
}

/// Build the row for a single node. `node_type` is passed in because
/// callers have already rendered it once per node, and `now` because a
/// clock read per row across a large graph is pure overhead.
fn node_row(
    n: &GraphNode,
    node_type: String,
    node_text: String,
    vector: Vec<f32>,
    now: i64,
) -> NodeRow {
    NodeRow {
        id: n.id.clone(),
        name: n.name.clone(),
        node_type,
        description: n.docstring.clone().unwrap_or_default(),
        file: n.file.clone().unwrap_or_default(),
        start_line: n.start_line.unwrap_or(0),
        end_line: n.end_line.unwrap_or(0),
        last_update_at: now,
        node_text,
        vector,
    }
}

/// Run a plan's `to_embed` bucket through the embedder and fold the
/// results into the rows the plan already resolved. Returns the complete
/// set of rows to upsert plus how many nodes were embedded.
async fn rows_from_plan(
    plan: IngestPlan,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<(Vec<NodeRow>, usize), Box<dyn std::error::Error + Send + Sync>> {
    let embedded = plan.to_embed.len();
    if embedded == 0 {
        return Ok((plan.finish(graph, Vec::new())?, 0));
    }
    let texts: Vec<String> = plan.to_embed.iter().map(|(_, t)| t.clone()).collect();
    let vectors = embedder.embed(&texts).await?;
    Ok((plan.finish(graph, vectors)?, embedded))
}

/// Build the per-node embedding texts for a graph, in `graph.nodes` order.
fn build_texts(graph: &GraphData) -> Vec<String> {
    let related = collect_related_names(graph);
    graph
        .nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            build_node_text(n, names)
        })
        .collect()
}

/// Single-destination ingest. Embeds only the nodes that changed since
/// the last run (see the module docs), then upserts nodes + edges.
pub async fn ingest_graph(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<IngestStats, Box<dyn std::error::Error + Send + Sync>> {
    let texts = build_texts(graph);
    let plan = plan_incremental_ingest(store, graph, &texts, false).await?;
    let unchanged = plan.unchanged;
    let (node_rows, embedded) = rows_from_plan(plan, embedder, graph).await?;
    let edge_rows = build_edge_rows(graph);

    store.upsert_nodes(&node_rows).await?;
    store.upsert_edges(&edge_rows).await?;

    Ok(IngestStats {
        nodes_written: node_rows.len(),
        edges_written: edge_rows.len(),
        embedding_calls: usize::from(embedded > 0),
        nodes_embedded: embedded,
        nodes_unchanged: unchanged,
    })
}

/// Multi-destination ingest. Embeds only what changed, then fans the
/// upserts out across every backend in `set` (parallel, fail-fast on
/// any backend error). The embedding dim must match across all stores
/// — call [`StoreSet::validate_dims`] before this if you want a clear
/// error early instead of a per-row `BadVector`.
///
/// Vector reuse is planned against the first store, but *every* row is
/// still written to *every* destination (`always_write`). Skipping
/// writes on one store's say-so would let a destination that is missing
/// or behind on a node silently stay that way.
pub async fn ingest_graph_multi(
    set: &StoreSet,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<IngestStats, Box<dyn std::error::Error + Send + Sync>> {
    let texts = build_texts(graph);
    let plan = match set.stores.first() {
        Some(store) => plan_incremental_ingest(store.as_ref(), graph, &texts, true).await?,
        None => return Err("empty StoreSet".into()),
    };
    let (node_rows, embedded) = rows_from_plan(plan, embedder, graph).await?;
    let edge_rows = build_edge_rows(graph);

    set.upsert_nodes(&node_rows).await?;
    set.upsert_edges(&edge_rows).await?;

    Ok(IngestStats {
        nodes_written: node_rows.len(),
        edges_written: edge_rows.len(),
        embedding_calls: usize::from(embedded > 0),
        nodes_embedded: embedded,
        nodes_unchanged: 0,
    })
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
        .map(|((n, node_text), vector)| {
            node_row(n, format!("{:?}", n.node_type), node_text, vector, now)
        })
        .collect();

    store.upsert_nodes(&rows).await?;

    Ok(IngestStats {
        nodes_written: rows.len(),
        edges_written: 0,
        embedding_calls: 1,
        nodes_embedded: rows.len(),
        nodes_unchanged: 0,
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
