//! Storage smoke tests against the OverGraph-backed `Db`.
//!
//! Phase D scope: prove the rewritten storage layer exposes the same
//! wire-format APIs the rest of the project depends on (DTO shapes,
//! upsert + retrieve round-trip, traversal). Phase F expands this with
//! ingest pipeline coverage and benchmark numbers.
//!
//! These tests run without an embedding server: they call the low-level
//! `Db` functions directly with hand-built vectors, so no network or
//! external model is needed.

use tempfile::TempDir;
use ultragraph_kb::storage::db::{
    edges_from, edges_to, hybrid_search, nodes_by_ids, traverse_string_ids, vector_search, Db,
    EdgeRow, NodeRow,
};
use ultragraph_kb::storage::embed::EMBEDDING_DIM;
use ultragraph_kb::storage::ppr::run_ppr;
use ultragraph_kb::storage::query::Direction;
use ultragraph_kb::storage::text::build_sparse_keyword_vector;

fn unit_vector(seed: f32) -> Vec<f32> {
    // Deterministic 1024-dim vector with a single high component so
    // ANN search returns the seeded node first. Cosine metric uses
    // direction only, magnitude is irrelevant.
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    let idx = (seed as usize) % EMBEDDING_DIM;
    v[idx] = 1.0;
    v
}

fn sample_node(id: &str, name: &str, node_type: &str, vector_seed: f32) -> NodeRow {
    NodeRow {
        id: id.to_string(),
        name: name.to_string(),
        node_type: node_type.to_string(),
        description: format!("description of {}", name),
        file: format!("src/{}.ts", name),
        start_line: 1,
        end_line: 10,
        last_update_at: 1_700_000_000,
        node_text: format!("Function: {}", name),
        vector: unit_vector(vector_seed),
    }
}

fn sample_edge(source: &str, target: &str, edge_type: &str) -> EdgeRow {
    EdgeRow {
        id: format!("{}|{}|{}", source, edge_type, target),
        source: source.to_string(),
        target: target.to_string(),
        edge_type: edge_type.to_string(),
        properties: String::new(),
    }
}

#[tokio::test]
async fn upsert_and_query_nodes_round_trip() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let alice = sample_node("function:src/alice.ts:1:alice", "alice", "Function", 7.0);
    let bob = sample_node("function:src/bob.ts:1:bob", "bob", "Function", 13.0);

    db.upsert_nodes(&[alice.clone(), bob.clone()]).await.unwrap();

    let rows = nodes_by_ids(&db, &[alice.id.clone(), bob.id.clone()])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"alice"));
    assert!(names.contains(&"bob"));
}

#[tokio::test]
async fn vector_search_returns_seed_first() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let target = sample_node("function:t", "target", "Function", 42.0);
    let other = sample_node("function:o", "other", "Function", 5.0);
    db.upsert_nodes(&[target.clone(), other.clone()])
        .await
        .unwrap();

    let hits = vector_search(&db, unit_vector(42.0), 5, None).await.unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(hits[0].0.id, target.id, "target should rank first");
}

#[tokio::test]
async fn hybrid_search_uses_dense_and_sparse() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let target = sample_node("function:t", "target", "Function", 1.0);
    db.upsert_nodes(&[target.clone()]).await.unwrap();

    let sparse = build_sparse_keyword_vector("description of target");
    let hits = hybrid_search(&db, unit_vector(1.0), sparse, 3, None)
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0.id, target.id);
}

#[tokio::test]
async fn upsert_edges_and_traverse_outbound() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let a = sample_node("function:a", "a", "Function", 1.0);
    let b = sample_node("function:b", "b", "Function", 2.0);
    let c = sample_node("function:c", "c", "Function", 3.0);
    db.upsert_nodes(&[a.clone(), b.clone(), c.clone()])
        .await
        .unwrap();
    db.upsert_edges(&[
        sample_edge(&a.id, &b.id, "Calls"),
        sample_edge(&b.id, &c.id, "Calls"),
    ])
    .await
    .unwrap();

    let outs = edges_from(&db, &a.id).await.unwrap();
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0].target, b.id);

    let ins = edges_to(&db, &c.id).await.unwrap();
    assert_eq!(ins.len(), 1);
    assert_eq!(ins[0].source, b.id);
}

#[tokio::test]
async fn traverse_string_ids_returns_reachable_nodes() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let a = sample_node("function:a", "a", "Function", 1.0);
    let b = sample_node("function:b", "b", "Function", 2.0);
    let c = sample_node("function:c", "c", "Function", 3.0);
    db.upsert_nodes(&[a.clone(), b.clone(), c.clone()])
        .await
        .unwrap();
    db.upsert_edges(&[
        sample_edge(&a.id, &b.id, "Calls"),
        sample_edge(&b.id, &c.id, "Calls"),
    ])
    .await
    .unwrap();

    let (nodes, _edges, distances) = traverse_string_ids(
        &db,
        &a.id,
        2,
        None,
        overgraph::Direction::Outgoing,
    )
    .await
    .unwrap();
    let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    assert!(ids.contains(&a.id), "start node should be present");
    assert!(ids.contains(&b.id), "1-hop neighbour should be present");
    assert!(ids.contains(&c.id), "2-hop neighbour should be present");
    assert_eq!(distances.get(&a.id).copied(), Some(0));
}

#[tokio::test]
async fn run_ppr_ranks_seed_neighborhood() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let seed = sample_node("function:seed", "seed", "Function", 1.0);
    let near = sample_node("function:near", "near", "Function", 2.0);
    let far = sample_node("function:far", "far", "Function", 3.0);
    db.upsert_nodes(&[seed.clone(), near.clone(), far.clone()])
        .await
        .unwrap();
    db.upsert_edges(&[sample_edge(&seed.id, &near.id, "Calls")])
        .await
        .unwrap();

    let ranked = run_ppr(
        &db,
        &[seed.id.clone()],
        Direction::Both,
        None,
        0.15,
        20,
        Some(10),
    )
    .await
    .unwrap();
    let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
    assert!(ids.contains(&seed.id), "seed should appear in PPR output");
    assert!(
        ids.contains(&near.id),
        "1-hop neighbour should appear in PPR output"
    );
}

#[test]
fn build_sparse_keyword_vector_is_deterministic() {
    let a = build_sparse_keyword_vector("hello world");
    let b = build_sparse_keyword_vector("hello world");
    assert_eq!(a, b);
    assert!(!a.is_empty());
}
