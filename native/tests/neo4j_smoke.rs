//! Neo4j backend smoke tests.
//!
//! These tests require a running Neo4j 5.13+ server with the user's
//! local credentials (see `docs/neo4j-cred.txt`). They are gated with
//! `#[ignore]` so the default `cargo test` run stays self-contained.
//!
//! Run them with:
//!     cargo test -p ultragraph --test neo4j_smoke -- --ignored --nocapture
//!
//! Phase 2 scope: open + capability probes + schema + read-side
//! happy-path. Write-side coverage lives in `neo4j_write_smoke.rs`
//! (Phase 3).

use ultragraph::storage::backends::neo4j::Neo4jStore;
use ultragraph::storage::store::KnowledgeStore;

const URI: &str = "neo4j://localhost:7687";
const USER: &str = "neo4j";
const PASSWORD: &str = "skooty5420";
const DIM: u32 = 384;

#[tokio::test]
#[ignore]
async fn open_probes_capabilities_and_creates_schema() {
    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    assert_eq!(store.embedding_dim(), DIM);
    assert_eq!(store.backend_name(), "neo4j");
    // GDS / APOC may or may not be present depending on plugins; just
    // exercise the call without asserting either way.
    let _ = store.supports_native_ppr();
    // count_nodes is a read-only sanity check; whatever value the server
    // returns is fine — we just need the call to succeed.
    let _ = store.count_nodes().await.expect("count_nodes");
}

#[tokio::test]
#[ignore]
async fn fetch_node_returns_none_for_missing_id() {
    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    let row = store
        .fetch_node("function:does/not/exist:1:nope")
        .await
        .expect("fetch_node");
    assert!(row.is_none());
}

#[tokio::test]
#[ignore]
async fn vector_search_returns_zero_or_more_hits() {
    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    let q = vec![0.1f32; DIM as usize];
    let hits = store
        .vector_search(q, 5, None)
        .await
        .expect("vector_search");
    // Don't assert non-empty — the DB may be empty. We're checking
    // that the query path works end-to-end.
    assert!(hits.len() <= 5);
}

#[tokio::test]
#[ignore]
async fn ppr_returns_unsupported_when_gds_absent() {
    let store = Neo4jStore::open(URI, USER, PASSWORD, None, DIM)
        .await
        .expect("open");
    if store.supports_native_ppr() {
        // GDS is installed — PPR would actually run; that's covered by
        // Phase 5 tests, not here.
        return;
    }
    let result = store
        .personalized_pagerank(
            &["any:seed".to_string()],
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
}
