//! Phase F micro-benchmark: bulk ingest + repeated hybrid searches.
//!
//! Not a Criterion bench (kept dependency-light) — just a wall-clock
//! sanity check. Runs only when explicitly requested:
//!   `cargo test -p ultragraph --test storage_bench -- --ignored --nocapture`
//!
//! Targets from MIGRATION-OVERGRAPH §F:
//!   * Ingest 1K nodes + 5K edges < 2s
//!   * 100 hybrid searches → record p50/p95

use std::time::Instant;
use tempfile::TempDir;
use ultragraph::storage::db::{hybrid_search, Db, EdgeRow, NodeRow};
use ultragraph::storage::embed::EMBEDDING_DIM;
use ultragraph::storage::text::build_sparse_keyword_vector;

const N_NODES: usize = 1000;
const N_EDGES: usize = 5000;
const N_QUERIES: usize = 100;

fn fake_vector(seed: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    // Spread the energy across 4 dimensions per seed so cosine search
    // produces meaningful rankings instead of degenerate ties.
    for offset in 0..4 {
        v[(seed + offset * 257) % EMBEDDING_DIM] = 1.0 / (offset as f32 + 1.0);
    }
    v
}

fn make_node(i: usize) -> NodeRow {
    NodeRow {
        id: format!("function:src/mod{}.ts:1:fn_{}", i / 10, i),
        name: format!("fn_{}", i),
        node_type: "Function".into(),
        description: format!("synthetic function number {}", i),
        file: format!("src/mod{}.ts", i / 10),
        start_line: 1,
        end_line: 5,
        last_update_at: 1_700_000_000,
        node_text: format!("Function: fn_{}. synthetic", i),
        vector: fake_vector(i),
    }
}

fn make_edge(src: usize, tgt: usize) -> EdgeRow {
    EdgeRow {
        id: format!("e:{}:{}", src, tgt),
        source: format!("function:src/mod{}.ts:1:fn_{}", src / 10, src),
        target: format!("function:src/mod{}.ts:1:fn_{}", tgt / 10, tgt),
        edge_type: "Calls".into(),
        properties: String::new(),
    }
}

#[tokio::test]
#[ignore]
async fn ingest_1k_nodes_5k_edges() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let nodes: Vec<NodeRow> = (0..N_NODES).map(make_node).collect();
    let edges: Vec<EdgeRow> = (0..N_EDGES)
        .map(|i| make_edge(i % N_NODES, (i * 7 + 3) % N_NODES))
        .collect();

    let t0 = Instant::now();
    db.upsert_nodes(&nodes).await.unwrap();
    let t_nodes = t0.elapsed();

    let t1 = Instant::now();
    db.upsert_edges(&edges).await.unwrap();
    let t_edges = t1.elapsed();

    println!(
        "ingest: {} nodes in {:.2?}, {} edges in {:.2?}, total {:.2?}",
        N_NODES,
        t_nodes,
        N_EDGES,
        t_edges,
        t0.elapsed()
    );
    assert!(t0.elapsed().as_secs_f32() < 5.0, "ingest > 5s; investigate");
}

#[tokio::test]
#[ignore]
async fn hybrid_search_p50_p95() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();
    let nodes: Vec<NodeRow> = (0..N_NODES).map(make_node).collect();
    db.upsert_nodes(&nodes).await.unwrap();

    let mut samples: Vec<u128> = Vec::with_capacity(N_QUERIES);
    let queries: Vec<String> = (0..N_QUERIES)
        .map(|i| format!("synthetic function {}", i))
        .collect();
    for (i, q) in queries.iter().enumerate() {
        let dense = fake_vector(i);
        let sparse = build_sparse_keyword_vector(q);
        let t = Instant::now();
        let _ = hybrid_search(&db, dense, sparse, 10, None).await.unwrap();
        samples.push(t.elapsed().as_micros());
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    let mean: u128 = samples.iter().sum::<u128>() / samples.len() as u128;
    println!(
        "hybrid_search ({} queries): p50={}μs p95={}μs mean={}μs",
        N_QUERIES, p50, p95, mean
    );
}
