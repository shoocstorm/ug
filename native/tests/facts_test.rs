//! Derived facts, end to end through the store.
//!
//! The unit tests in `storage::facts` cover what each fact *is*. These
//! cover the two things only a real store can show: that facts survive a
//! write/read round-trip as queryable properties, and that they still land
//! when the embedder is unavailable — which is the whole point of storing
//! them separately from the vectors.

use std::collections::BTreeMap;
use tempfile::TempDir;
use ultragraph::storage::db::{nodes_by_ids, Db, NodeRow};
use ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM;
use ultragraph::storage::facts::FactValue;

fn unit_vector(seed: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DEFAULT_EMBEDDING_DIM];
    v[seed % DEFAULT_EMBEDDING_DIM] = 1.0;
    v
}

fn row_with(id: &str, vector: Vec<f32>, facts: BTreeMap<String, FactValue>) -> NodeRow {
    NodeRow {
        id: id.to_string(),
        name: id.to_string(),
        node_type: "Function".into(),
        description: String::new(),
        file: "src/a.rs".into(),
        start_line: 1,
        end_line: 10,
        last_update_at: 1_700_000_000,
        node_text: format!("Function: {}", id),
        vector,
        code: String::new(),
        file_hash: String::new(),
        facts,
    }
}

fn facts(pairs: &[(&str, FactValue)]) -> BTreeMap<String, FactValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

async fn open(tmp: &TempDir) -> Db {
    Db::open_or_create(tmp.path().to_str().unwrap(), DEFAULT_EMBEDDING_DIM as u32)
        .await
        .unwrap()
}

#[tokio::test]
async fn facts_survive_a_write_and_read_round_trip() {
    let tmp = TempDir::new().unwrap();
    let db = open(&tmp).await;

    let written = facts(&[
        ("loc", FactValue::Int(72)),
        ("in_degree", FactValue::Int(3)),
        ("is_test", FactValue::Int(0)),
        ("folder", FactValue::Str("src/storage".into())),
    ]);
    db.upsert_nodes(&[row_with("f:a", unit_vector(1), written.clone())])
        .await
        .unwrap();

    let back = nodes_by_ids(&db, &["f:a".to_string()]).await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].facts, written);
}

/// Facts are stored as plain sibling properties (`n.loc`, not `n.f_loc`)
/// so a query reads the way someone would write it by hand. That makes
/// them indistinguishable from the fixed columns on read-back, so the
/// split is by name — and a fact must never be able to overwrite a column
/// the rest of the code depends on.
#[tokio::test]
async fn a_fact_cannot_shadow_a_fixed_column() {
    let tmp = TempDir::new().unwrap();
    let db = open(&tmp).await;

    let hostile = facts(&[
        ("name", FactValue::Str("hijacked".into())),
        ("file", FactValue::Str("elsewhere.rs".into())),
        ("loc", FactValue::Int(5)),
    ]);
    db.upsert_nodes(&[row_with("f:a", unit_vector(1), hostile)])
        .await
        .unwrap();

    let back = nodes_by_ids(&db, &["f:a".to_string()]).await.unwrap();
    assert_eq!(back[0].name, "f:a", "the column wins");
    assert_eq!(back[0].file, "src/a.rs", "the column wins");
    assert_eq!(back[0].facts["loc"], FactValue::Int(5));
    assert!(
        !back[0].facts.contains_key("name"),
        "a column name never round-trips as a fact"
    );
}

/// The reason facts live in properties rather than riding along with the
/// vector: an index built while the embedding endpoint was down must still
/// answer structural and statistical questions.
#[tokio::test]
async fn nodes_persist_with_their_facts_when_embedding_failed() {
    let tmp = TempDir::new().unwrap();
    let db = open(&tmp).await;

    let f = facts(&[("loc", FactValue::Int(120)), ("in_degree", FactValue::Int(0))]);
    // An empty vector is what ingest writes when the embedder is
    // unreachable — "not embedded", not "bad vector".
    db.upsert_nodes(&[row_with("f:unembedded", Vec::new(), f.clone())])
        .await
        .unwrap();

    let back = nodes_by_ids(&db, &["f:unembedded".to_string()])
        .await
        .unwrap();
    assert_eq!(back.len(), 1, "the node is stored despite having no vector");
    assert_eq!(back[0].facts, f, "and its facts are fully queryable");
    assert!(back[0].vector.is_empty());
}

/// A wrong-width vector is still an error. Degrading on *absent* vectors
/// must not quietly accept a corrupt one.
#[tokio::test]
async fn a_wrong_width_vector_is_still_rejected() {
    let tmp = TempDir::new().unwrap();
    let db = open(&tmp).await;

    let err = db
        .upsert_nodes(&[row_with("f:bad", vec![0.1; 7], BTreeMap::new())])
        .await
        .unwrap_err();
    assert!(
        format!("{}", err).contains("dim 7"),
        "expected a dimension complaint, got: {}",
        err
    );
}

/// Re-ingest must be able to fill in a vector that a previous degraded run
/// left empty, without the facts being disturbed.
#[tokio::test]
async fn a_later_run_backfills_the_vector_and_keeps_the_facts() {
    let tmp = TempDir::new().unwrap();
    let db = open(&tmp).await;
    let f = facts(&[("loc", FactValue::Int(42))]);

    db.upsert_nodes(&[row_with("f:a", Vec::new(), f.clone())])
        .await
        .unwrap();
    db.upsert_nodes(&[row_with("f:a", unit_vector(9), f.clone())])
        .await
        .unwrap();

    let back = nodes_by_ids(&db, &["f:a".to_string()]).await.unwrap();
    assert_eq!(back[0].vector.len(), DEFAULT_EMBEDDING_DIM);
    assert_eq!(back[0].facts, f);
}

#[tokio::test]
async fn declaring_fact_indexes_is_safe_to_repeat() {
    let tmp = TempDir::new().unwrap();
    let db = open(&tmp).await;
    db.upsert_nodes(&[row_with(
        "f:a",
        unit_vector(1),
        facts(&[("loc", FactValue::Int(9))]),
    )])
    .await
    .unwrap();

    // Ingest calls this on every run, so it has to be idempotent.
    db.ensure_fact_indexes();
    db.ensure_fact_indexes();

    let back = nodes_by_ids(&db, &["f:a".to_string()]).await.unwrap();
    assert_eq!(back[0].facts["loc"], FactValue::Int(9));
}
