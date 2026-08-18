//! `RemoteEmbedder::embed` issues batches concurrently (P2.1). The property
//! that matters is that it still returns vectors in **input order**.
//!
//! Callers zip the returned vectors against their inputs positionally —
//! `plan.finish(graph, vectors, ...)` binds `vectors[k]` to
//! `plan.to_embed[k]` — so an ordering bug would not fail loudly. It would
//! attach every node's vector to a different node and produce an index whose
//! semantic search is confidently wrong. Nothing downstream can detect that,
//! which is why it is worth a real server rather than a unit test of the
//! combinator.
//!
//! The fake endpoint answers *later* batches sooner, so completion order is
//! roughly the reverse of request order and an implementation that collected
//! results as they arrived would fail these immediately.

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::time::Duration;
use ultragraph::storage::{EmbedderConfig, RemoteEmbedder};

const DIM: usize = 4;
const BATCH: usize = 4;

/// Each input is the decimal form of its own index; the embedding it gets
/// back is that number repeated across the dimension, so a vector carries
/// the identity of the text it came from and a misalignment is visible.
async fn handler(Json(body): Json<Value>) -> Json<Value> {
    let inputs: Vec<String> = serde_json::from_value(body["input"].clone()).unwrap();
    let first: u64 = inputs[0].parse().unwrap();

    // Later batches finish first.
    tokio::time::sleep(Duration::from_millis(150u64.saturating_sub(first * 2))).await;

    let mut data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let n: f32 = t.parse().unwrap();
            json!({ "index": i, "embedding": vec![n; DIM] })
        })
        .collect();
    // Shuffled within the batch too, so the `index` sort is exercised rather
    // than accidentally satisfied by the endpoint being tidy.
    data.reverse();

    Json(json!({ "data": data }))
}

/// Always fails, to check the error path still surfaces.
async fn failing_handler() -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "upstream exploded".to_string(),
    )
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn embedder(base_url: String, concurrency: usize) -> RemoteEmbedder {
    let cfg = EmbedderConfig {
        base_url,
        dim: DIM,
        batch_size: BATCH,
        concurrency,
        ..Default::default()
    };
    RemoteEmbedder::new(cfg).unwrap()
}

fn texts(n: usize) -> Vec<String> {
    (0..n).map(|i| i.to_string()).collect()
}

/// Every vector lands against the text it was produced from, even though the
/// endpoint answered the batches in roughly reverse order.
#[tokio::test]
async fn concurrent_batches_return_in_input_order() {
    let url = spawn(Router::new().route("/embeddings", post(handler))).await;
    let e = embedder(url, 8);

    let input = texts(64);
    let out = e.embed(&input).await.expect("embed succeeds");

    assert_eq!(out.len(), input.len(), "one vector per input");
    for (i, v) in out.iter().enumerate() {
        assert_eq!(v.len(), DIM, "vector {i} has the wrong width");
        assert_eq!(
            v[0], i as f32,
            "vector {i} came from text {}, not {i} — batches were reassembled out of order",
            v[0]
        );
    }
}

/// `concurrency = 1` is the documented escape hatch for a rate-limited
/// endpoint. It must produce exactly what the concurrent path does.
#[tokio::test]
async fn concurrency_of_one_matches_the_concurrent_result() {
    let url = spawn(Router::new().route("/embeddings", post(handler))).await;

    let input = texts(32);
    let concurrent = embedder(url.clone(), 8).embed(&input).await.unwrap();
    let sequential = embedder(url, 1).embed(&input).await.unwrap();

    assert_eq!(concurrent, sequential);
}

/// A batch whose length is not a multiple of `batch_size` still comes back
/// whole — the last, short batch is the one an off-by-one would drop.
#[tokio::test]
async fn trailing_partial_batch_is_included() {
    let url = spawn(Router::new().route("/embeddings", post(handler))).await;
    let e = embedder(url, 8);

    // 4 full batches of 4, plus a remainder of 3.
    let input = texts(19);
    let out = e.embed(&input).await.unwrap();

    assert_eq!(out.len(), 19);
    assert_eq!(out[18][0], 18.0);
}

/// An empty input never reaches the network.
#[tokio::test]
async fn empty_input_is_a_no_op() {
    // Deliberately points at a port with nothing on it: if this made a
    // request it would error rather than return empty.
    let e = embedder("http://127.0.0.1:1".to_string(), 8);
    assert!(e.embed(&[]).await.unwrap().is_empty());
}

/// A failing endpoint still surfaces as `Err` rather than a short result —
/// the caller writes nodes without vectors on `Err`, but a silently-truncated
/// `Ok` would misalign every vector after the gap.
#[tokio::test]
async fn a_failing_batch_surfaces_as_an_error() {
    let url = spawn(Router::new().route("/embeddings", post(failing_handler))).await;
    let e = embedder(url, 8);

    let err = e.embed(&texts(32)).await.expect_err("should fail");
    assert!(
        matches!(err, ultragraph::storage::embed::EmbedError::BadStatus(500, _)),
        "unexpected error: {err}"
    );
}
