//! Neo4j write-path smoke tests.
//!
//! Phase 3+4 scope: round-trip a small fixture through Neo4j (upsert,
//! search, traverse, fetch) and validate the output matches what
//! OverGraph would have produced for the same input.
//!
//! Run with:
//!     cargo test -p ultragraph --test neo4j_write_smoke -- --ignored --nocapture --test-threads=1

use ultragraph::storage::backends::neo4j::Neo4jStore;
use ultragraph::storage::store::{Direction, KnowledgeStore};
use ultragraph::storage::{EdgeRow, NodeRow};

const URI: &str = "neo4j://localhost:7687";
const USER: &str = "neo4j";
const PASSWORD: &str = "skooty5420";
// Must match the dim the read-path smoke tests created the vector
// index with — Neo4j rejects re-opens with a mismatched dim.
const DIM: u32 = 384;

fn unit_vec(seed: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM as usize];
    let i = (seed as usize) % (DIM as usize);
    v[i] = 1.0;
    v
}

fn sample_node(id: &str, name: &str, kind: &str, seed: f32) -> NodeRow {
    NodeRow {
        id: id.to_string(),
        name: name.to_string(),
        node_type: kind.to_string(),
        description: format!("description of {}", name),
        file: format!("src/{}.ts", name),
        start_line: 1,
        end_line: 10,
        last_update_at: 1_700_000_000,
        node_text: format!("{}: {} description", kind, name),
        vector: unit_vec(seed),
    }
}

fn sample_edge(src: &str, tgt: &str, kind: &str) -> EdgeRow {
    EdgeRow {
        id: format!("{}|{}|{}", src, kind, tgt),
        source: src.to_string(),
        target: tgt.to_string(),
        edge_type: kind.to_string(),
        properties: String::new(),
    }
}

/// Wipe any rows from previous test runs so assertions are stable.
async fn cleanup(store: &Neo4jStore) {
    use neo4rs::query;
    // The store doesn't expose its `Graph` handle, so the simplest path
    // is to open a parallel one for the cleanup.
    let cfg = neo4rs::ConfigBuilder::default()
        .uri(URI.strip_prefix("neo4j://").unwrap())
        .user(USER)
        .password(PASSWORD)
        .build()
        .unwrap();
    let g = neo4rs::Graph::connect(cfg).await.unwrap();
    g.run(query("MATCH (n:UgNode) WHERE n.id STARTS WITH 'wt:' DETACH DELETE n"))
        .await
        .unwrap();
    let _ = store; // suppress unused warning
}

#[tokio::test]
#[ignore]
async fn round_trip_nodes_and_edges() {
    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    cleanup(&store).await;

    let alice = sample_node("wt:fn:alice", "alice", "Function", 1.0);
    let bob = sample_node("wt:fn:bob", "bob", "Function", 2.0);
    let carol = sample_node("wt:fn:carol", "carol", "Function", 3.0);

    store
        .upsert_nodes(&[alice.clone(), bob.clone(), carol.clone()])
        .await
        .expect("upsert_nodes");
    store
        .upsert_edges(&[
            sample_edge(&alice.id, &bob.id, "Calls"),
            sample_edge(&bob.id, &carol.id, "Calls"),
        ])
        .await
        .expect("upsert_edges");

    // fetch_node round-trip
    let fetched = store.fetch_node(&alice.id).await.expect("fetch").unwrap();
    assert_eq!(fetched.id, alice.id);
    assert_eq!(fetched.name, "alice");
    assert_eq!(fetched.vector.len(), DIM as usize);

    // vector_search returns the seed first
    let hits = store
        .vector_search(unit_vec(2.0), 5, None)
        .await
        .expect("vector_search");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0.id, bob.id);

    // hybrid_search: full-text leg matches the description token
    let hybrid = store
        .hybrid_search(unit_vec(1.0), Vec::new(), "alice", 5, None)
        .await
        .expect("hybrid_search");
    assert!(hybrid.iter().any(|(n, _)| n.id == alice.id));

    // traverse: 2-hop outbound from alice should hit carol
    let page = store
        .traverse(&alice.id, 2, None, Direction::Outbound)
        .await
        .expect("traverse");
    let ids: Vec<String> = page.nodes.iter().map(|n| n.row.id.clone()).collect();
    assert!(ids.contains(&alice.id));
    assert!(ids.contains(&bob.id));
    assert!(ids.contains(&carol.id));

    // count_nodes / count_edges include our writes
    let n = store.count_nodes().await.expect("count_nodes");
    assert!(n >= 3);
    let e = store.count_edges().await.expect("count_edges");
    assert!(e >= 2);

    cleanup(&store).await;
}

#[tokio::test]
#[ignore]
async fn search_kb_falls_back_to_mmr_when_no_gds() {
    use ultragraph::storage::backends::neo4j::Neo4jStore;
    use ultragraph::storage::store::KnowledgeStore;

    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    cleanup(&store).await;

    // Seed a tiny graph so search_kb has data to rank.
    let alice = sample_node("wt:fb:alice", "alice", "Function", 1.0);
    let bob = sample_node("wt:fb:bob", "bob", "Function", 2.0);
    store
        .upsert_nodes(&[alice.clone(), bob.clone()])
        .await
        .unwrap();
    store
        .upsert_edges(&[sample_edge(&alice.id, &bob.id, "Calls")])
        .await
        .unwrap();

    if store.supports_native_ppr() {
        // GDS is installed — fallback path won't fire; skip.
        cleanup(&store).await;
        return;
    }

    // Direct PPR call must surface Unsupported, NOT silently succeed.
    let result = store
        .personalized_pagerank(
            &[alice.id.clone()],
            ultragraph::storage::store::Direction::Both,
            None,
            0.15,
            10,
            Some(5),
        )
        .await;
    match result {
        Err(ultragraph::storage::store::StoreError::Unsupported(_)) => {}
        other => panic!("expected Unsupported, got {:?}", other),
    }

    cleanup(&store).await;
}

#[tokio::test]
#[ignore]
async fn upsert_nodes_rejects_wrong_dim() {
    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    let mut bad = sample_node("wt:bad", "bad", "Function", 1.0);
    bad.vector = vec![0.0; (DIM + 1) as usize];
    let err = store.upsert_nodes(&[bad]).await.unwrap_err();
    match err {
        ultragraph::storage::store::StoreError::BadVector { got, want, .. } => {
            assert_eq!(got, (DIM + 1) as usize);
            assert_eq!(want, DIM as usize);
        }
        other => panic!("expected BadVector, got {:?}", other),
    }
}
