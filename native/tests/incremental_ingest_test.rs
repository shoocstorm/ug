//! Incremental re-ingest planning.
//!
//! `plan_incremental_ingest` is what keeps a re-index from re-embedding a
//! whole repo, so what matters here is that it puts each node in the right
//! bucket: unchanged nodes must not be embedded *or* written, nodes whose
//! embedding text is unchanged must reuse the stored vector, and anything
//! genuinely new or edited must go back through the embedder.
//!
//! Like `storage_test.rs`, these run without an embedding server — rows
//! are written straight through `Db` with hand-built vectors.

use tempfile::TempDir;
use ultragraph::storage::db::{Db, NodeRow};
use ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM;
use ultragraph::storage::store::KnowledgeStore;
use ultragraph::storage::{build_node_text, collect_related_names, plan_incremental_ingest};
use ultragraph::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};

fn unit_vector(seed: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DEFAULT_EMBEDDING_DIM];
    v[seed % DEFAULT_EMBEDDING_DIM] = 1.0;
    v
}

fn node(id: &str, name: &str, docstring: Option<&str>, start_line: u32) -> GraphNode {
    GraphNode {
        id: id.to_string(),
        name: name.to_string(),
        node_type: GraphNodeType::Function,
        file: Some(format!("src/{}.ts", name)),
        start_line: Some(start_line),
        end_line: Some(start_line + 5),
        metrics: None,
        signature: None,
        docstring: docstring.map(|s| s.to_string()),
        imports: Vec::new(),
        exports: Vec::new(),
        extends: Vec::new(),
        implements: Vec::new(),
        calls: Vec::new(),
        folder: None,
        ..Default::default()
    }
}

fn graph(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> GraphData {
    GraphData {
        nodes,
        edges,
        stats: None,
    }
}

/// The texts ingest would build for `g`, in `g.nodes` order.
fn texts_for(g: &GraphData) -> Vec<String> {
    let related = collect_related_names(g);
    g.nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            build_node_text(n, names)
        })
        .collect()
}

/// Seed the store with exactly what a first full ingest of `g` would write.
async fn seed(db: &Db, g: &GraphData) -> Vec<String> {
    let texts = texts_for(g);
    // Facts have to be the ones a real ingest would derive. Seeding them
    // empty would make every node look changed on the next plan, which
    // silently turns each "nothing was rewritten" assertion below into a
    // test of the wrong thing.
    let facts_ctx = ultragraph::storage::FactContext::new(g);
    let rows: Vec<NodeRow> = g
        .nodes
        .iter()
        .zip(texts.iter())
        .enumerate()
        .map(|(i, (n, text))| NodeRow {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: format!("{:?}", n.node_type),
            description: n.docstring.clone().unwrap_or_default(),
            file: n.file.clone().unwrap_or_default(),
            start_line: n.start_line.unwrap_or(0),
            end_line: n.end_line.unwrap_or(0),
            last_update_at: 1_700_000_000,
            node_text: text.clone(),
            vector: unit_vector(i + 1),
            code: String::new(),
            file_hash: String::new(),
            facts: ultragraph::storage::facts::compute(n, &facts_ctx),
        })
        .collect();
    db.upsert_nodes(&rows).await.unwrap();
    texts
}

/// These fixtures have no repo on disk, so nothing is captured — the
/// planner falls back to comparing everything but the source columns.
fn no_code() -> std::collections::HashMap<String, ultragraph::storage::CapturedCode> {
    std::collections::HashMap::new()
}

fn sample_graph() -> GraphData {
    graph(
        vec![
            node("function:a", "alpha", Some("does alpha things"), 1),
            node("function:b", "beta", Some("does beta things"), 20),
            node("function:c", "gamma", Some("does gamma things"), 40),
        ],
        vec![GraphEdge {
            source: "function:a".to_string(),
            target: "function:b".to_string(),
            edge_type: GraphEdgeType::Calls,
        }],
    )
}

#[tokio::test]
async fn first_ingest_embeds_everything() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();

    assert_eq!(plan.to_embed.len(), 3, "empty store: nothing is reusable");
    assert_eq!(plan.reusable.len(), 0);
    assert_eq!(plan.unchanged, 0);
}

#[tokio::test]
async fn unchanged_graph_embeds_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = seed(&db, &g).await;

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();

    assert_eq!(plan.to_embed.len(), 0, "no embedding for an unchanged repo");
    assert_eq!(plan.reusable.len(), 0, "no writes either");
    assert_eq!(plan.unchanged, 3);
}

#[tokio::test]
async fn edited_node_is_the_only_one_re_embedded() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let mut g = sample_graph();
    seed(&db, &g).await;

    // Edit `gamma`'s docstring — it has no edges, so nothing else's
    // embedding text moves with it.
    g.nodes[2].docstring = Some("now documented differently".to_string());
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();

    assert_eq!(plan.to_embed.len(), 1, "only the edited node re-embeds");
    assert_eq!(plan.to_embed[0].0, 2);
    assert_eq!(plan.unchanged, 2);
    assert_eq!(plan.reusable.len(), 0);
}

#[tokio::test]
async fn moved_node_reuses_its_vector() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let mut g = sample_graph();
    seed(&db, &g).await;

    // Line numbers don't feed `build_node_text`, so shifting a symbol
    // down the file must rewrite the row without paying to embed it.
    g.nodes[1].start_line = Some(120);
    g.nodes[1].end_line = Some(125);
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();

    assert_eq!(plan.to_embed.len(), 0, "text is unchanged: no embedding");
    assert_eq!(plan.reusable.len(), 1, "but the row still needs writing");
    assert_eq!(plan.unchanged, 2);

    let row = &plan.reusable[0];
    assert_eq!(row.id, "function:b");
    assert_eq!(row.start_line, 120, "row carries the new position");
    assert_eq!(
        row.vector,
        unit_vector(2),
        "and the vector stored for it originally"
    );
}

#[tokio::test]
async fn new_node_embeds_without_disturbing_the_rest() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let mut g = sample_graph();
    seed(&db, &g).await;

    g.nodes.push(node("function:d", "delta", Some("brand new"), 60));
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();

    assert_eq!(plan.to_embed.len(), 1);
    assert_eq!(plan.to_embed[0].0, 3, "the appended node");
    assert_eq!(plan.unchanged, 3);
}

#[tokio::test]
async fn new_edge_re_embeds_both_endpoints() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let mut g = sample_graph();
    seed(&db, &g).await;

    // `build_node_text` folds in neighbour names, so adding an edge
    // changes the text on both ends — and only on both ends.
    g.edges.push(GraphEdge {
        source: "function:b".to_string(),
        target: "function:c".to_string(),
        edge_type: GraphEdgeType::Calls,
    });
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();

    let mut re_embedded: Vec<usize> = plan.to_embed.iter().map(|(i, _)| *i).collect();
    re_embedded.sort_unstable();
    assert_eq!(re_embedded, vec![1, 2], "beta and gamma, not alpha");
    assert_eq!(plan.unchanged, 1);
}

#[tokio::test]
async fn always_write_keeps_every_row_for_fan_out() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = seed(&db, &g).await;

    // Multi-destination ingest plans against one store but must still
    // write every row to all of them.
    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, true, &no_code(), None)
        .await
        .unwrap();

    assert_eq!(plan.unchanged, 0, "nothing is skipped under always_write");
    assert_eq!(plan.to_embed.len(), 0, "but reuse still applies");
    assert_eq!(plan.reusable.len(), 3);
}

#[tokio::test]
async fn finish_assembles_rows_in_plan_order() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let mut g = sample_graph();
    seed(&db, &g).await;

    g.nodes[0].docstring = Some("edited".to_string());
    g.nodes[1].start_line = Some(200);
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();
    assert_eq!(plan.to_embed.len(), 1);
    assert_eq!(plan.reusable.len(), 1);

    let fresh = unit_vector(999);
    let rows = plan.finish(&g, vec![fresh.clone()], &no_code()).unwrap();
    assert_eq!(rows.len(), 2, "one moved row + one re-embedded row");

    let moved = rows.iter().find(|r| r.id == "function:b").unwrap();
    assert_eq!(moved.start_line, 200);
    assert_eq!(moved.vector, unit_vector(2));

    let edited = rows.iter().find(|r| r.id == "function:a").unwrap();
    assert_eq!(edited.description, "edited");
    assert_eq!(edited.vector, fresh);
}

#[tokio::test]
async fn finish_rejects_a_vector_count_mismatch() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = texts_for(&g);

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();
    assert_eq!(plan.to_embed.len(), 3);

    let err = plan.finish(&g, vec![unit_vector(1)], &no_code()).unwrap_err();
    assert!(err.contains("1 vectors for 3 nodes"), "got: {err}");
}

/// The planner is the only thing that warms the id cache for nodes it
/// skips. If it didn't, the edge upsert that follows would fall back to
/// probing every node type for each endpoint.
#[tokio::test]
async fn planning_warms_the_id_cache_for_skipped_nodes() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = seed(&db, &g).await;

    // A fresh handle on the same data starts with a cold cache.
    drop(db);
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let plan = plan_incremental_ingest(&db as &dyn KnowledgeStore, &g, &texts, false, &no_code(), None)
        .await
        .unwrap();
    assert_eq!(plan.unchanged, 3, "nothing to write");

    // Edges still resolve, which they can only do via the cache the
    // planner populated (or an expensive probe).
    let edge_rows: Vec<ultragraph::storage::db::EdgeRow> = g
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            ultragraph::storage::db::EdgeRow {
                id: format!("{}|{}|{}", e.source, edge_type, e.target),
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect();
    db.upsert_edges(&edge_rows).await.unwrap();

    let outs = ultragraph::storage::db::edges_from(&db, "function:a")
        .await
        .unwrap();
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0].target, "function:b");
}

// ─── Pruning ────────────────────────────────────────────────────────────
//
// Ingest is an upsert, so a node that disappears from the source used to
// linger in the store forever and keep surfacing in search results.

#[tokio::test]
async fn prune_removes_nodes_the_graph_no_longer_has() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    seed(&db, &g).await;

    // `gamma` is deleted from the source.
    let mut shrunk = g.clone();
    shrunk.nodes.retain(|n| n.id != "function:c");

    let removed = ultragraph::storage::prune_to_graph(&db as &dyn KnowledgeStore, &shrunk)
        .await
        .unwrap();
    assert_eq!(removed, 1, "exactly the dropped node");

    let left = db.count_nodes().await.unwrap();
    assert_eq!(left, 2, "survivors stay: {left}");
    assert!(
        db.fetch_node("function:c").unwrap().is_none(),
        "pruned node must be gone"
    );
    assert!(
        db.fetch_node("function:a").unwrap().is_some(),
        "kept node must remain"
    );
}

#[tokio::test]
async fn prune_is_a_noop_when_nothing_was_removed() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    seed(&db, &g).await;

    let removed = ultragraph::storage::prune_to_graph(&db as &dyn KnowledgeStore, &g)
        .await
        .unwrap();
    assert_eq!(removed, 0);
    assert_eq!(db.count_nodes().await.unwrap(), 3);
}

/// The guard that stops a failed index from erasing the store.
#[tokio::test]
async fn prune_refuses_to_run_against_an_empty_graph() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    seed(&db, &g).await;

    let empty = graph(Vec::new(), Vec::new());
    let removed = ultragraph::storage::prune_to_graph(&db as &dyn KnowledgeStore, &empty)
        .await
        .unwrap();
    assert_eq!(removed, 0, "an empty graph must not be treated as 'delete all'");
    assert_eq!(db.count_nodes().await.unwrap(), 3, "store untouched");
}

/// A pruned key must not stay resolvable through the id cache.
#[tokio::test]
async fn prune_clears_the_id_cache_for_removed_nodes() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    seed(&db, &g).await;
    // seed() populated key_to_id for all three via upsert.

    let mut shrunk = g.clone();
    shrunk.nodes.retain(|n| n.id != "function:c");
    ultragraph::storage::prune_to_graph(&db as &dyn KnowledgeStore, &shrunk)
        .await
        .unwrap();

    assert!(
        db.lookup_id("function:c").unwrap().is_none(),
        "stale id must not resolve after prune"
    );
}

/// Switching embedding models must invalidate every stored vector, even
/// when the text and the dimension are both unchanged.
///
/// This is the case the dim guard cannot see: `bge-small-en-v1.5` and
/// `all-MiniLM-L6-v2` are both 384-wide, so without the recorded model the
/// planner would call every row reusable and leave the store holding
/// vectors from two different embedding spaces — silently, and with search
/// quality quietly wrecked.
#[tokio::test]
async fn a_model_switch_forces_a_full_reembed() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = seed(&db, &g).await;
    db.record_ingest_model("bge-small-en-v1.5");

    // Same model: the usual incremental path, nothing to do.
    let plan = plan_incremental_ingest(
        &db as &dyn KnowledgeStore,
        &g,
        &texts,
        false,
        &no_code(),
        Some("bge-small-en-v1.5"),
    )
    .await
    .unwrap();
    assert_eq!(plan.unchanged, 3, "same model, same text: no work");
    assert_eq!(plan.to_embed.len(), 0);

    // Different model, identical text and dim: every node must be re-embedded.
    let plan = plan_incremental_ingest(
        &db as &dyn KnowledgeStore,
        &g,
        &texts,
        false,
        &no_code(),
        Some("all-MiniLM-L6-v2"),
    )
    .await
    .unwrap();
    assert_eq!(plan.to_embed.len(), 3, "vectors from another model are not reusable");
    assert_eq!(plan.unchanged, 0);
    assert_eq!(plan.reusable.len(), 0);
}

/// A store written before the model was recorded must keep working the way
/// it always did, rather than re-embedding itself on every run.
#[tokio::test]
async fn an_unrecorded_model_still_allows_reuse() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let g = sample_graph();
    let texts = seed(&db, &g).await;
    // No record_ingest_model call — this is the legacy sidecar shape.

    let plan = plan_incremental_ingest(
        &db as &dyn KnowledgeStore,
        &g,
        &texts,
        false,
        &no_code(),
        Some("bge-small-en-v1.5"),
    )
    .await
    .unwrap();
    assert_eq!(plan.unchanged, 3, "unknown != mismatched");
}
