//! Wall-clock micro-benchmark for the graph analysis entry points.
//!
//! Same shape as `storage_bench.rs` — not Criterion, just `Instant` and
//! `--nocapture`, so it adds no dependency. Runs only when asked:
//!   `cargo test -p ultragraph --test graph_bench -- --ignored --nocapture`
//!
//! Tracks P1.1–P1.3 in `docs/dev/PERF-TUNING-JOURNEY.md`. Numbers from a run
//! belong in that file's results log, with the machine noted — these are
//! comparative, not absolute.

mod centrality_baseline;

use std::path::PathBuf;
use std::time::Instant;
use ultragraph::{calculate_centrality_graph, types::GraphData};

/// Deterministic synthetic graph: `n` nodes, roughly `avg_degree · n` edges,
/// wired by a cheap LCG so the shape is reproducible across runs and machines
/// without pulling in `rand`.
fn synthetic(n: usize, avg_degree: usize) -> GraphData {
    let nodes: Vec<_> = (0..n)
        .map(|i| serde_json::json!({ "id": format!("n{i}"), "name": format!("n{i}"), "node_type": "Function" }))
        .collect();

    let mut edges = Vec::with_capacity(n * avg_degree);
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in 0..n {
        for _ in 0..avg_degree {
            let t = (next() as usize) % n;
            if t != i {
                edges.push(serde_json::json!({
                    "source": format!("n{i}"), "target": format!("n{t}"), "edge_type": "Calls"
                }));
            }
        }
    }

    serde_json::from_value(serde_json::json!({ "nodes": nodes, "edges": edges }))
        .expect("synthetic graph parses")
}

fn report(label: &str, graph: &GraphData, elapsed_ms: f64) {
    let v = graph.nodes.len();
    let e = graph.edges.len();
    // Brandes is O(V·E); this is the machine-independent-ish figure to compare.
    let ve = (v as f64) * (e as f64);
    println!(
        "  {label:<28} V={v:>7} E={e:>8}  {elapsed_ms:>10.1} ms   {:>8.1} M(V·E)/s",
        ve / elapsed_ms / 1_000.0
    );
}

#[test]
#[ignore]
fn centrality_synthetic() {
    println!("\ncalculate_centrality_graph — synthetic, avg degree 3");
    for n in [500usize, 1_000, 2_000, 4_000, 8_000] {
        let graph = synthetic(n, 3);
        let t = Instant::now();
        let out = calculate_centrality_graph(&graph);
        let ms = t.elapsed().as_secs_f64() * 1_000.0;
        assert!(out.len() > 2, "produced no result at n={n}");
        report(&format!("n={n}"), &graph, ms);
    }
    println!();
}

/// Every `~/.ug/<project>/graph.json` on this machine, smallest first.
fn fixtures() -> Vec<(String, PathBuf)> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let root = home.join(".ug");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf, u64)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path().join("graph.json");
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let name = e.file_name().to_string_lossy().to_string();
        out.push((name, path, meta.len()));
    }
    out.sort_by_key(|(_, _, size)| *size);
    out.into_iter().map(|(n, p, _)| (n, p)).collect()
}

/// Real graphs, if this machine has any indexed.
///
/// Brandes is O(V·E), so the largest fixture is minutes of genuine work even
/// at full speed — it is skipped unless `UG_BENCH_ALL=1`, and the node cap is
/// the honest boundary of what this entry point can answer interactively.
#[test]
#[ignore]
fn centrality_fixtures() {
    const CAP: usize = 20_000;
    let all = std::env::var("UG_BENCH_ALL").is_ok();

    let found = fixtures();
    if found.is_empty() {
        println!("\nno ~/.ug/*/graph.json fixtures on this machine — skipping\n");
        return;
    }

    println!("\ncalculate_centrality_graph — real fixtures");
    for (name, path) in found {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(graph) = serde_json::from_str::<GraphData>(&raw) else {
            println!("  {name:<28} (does not parse — skipped)");
            continue;
        };
        drop(raw);
        if graph.nodes.len() > CAP && !all {
            println!(
                "  {name:<28} V={:>7} E={:>8}  skipped (over {CAP}; set UG_BENCH_ALL=1)",
                graph.nodes.len(),
                graph.edges.len()
            );
            continue;
        }
        let t = Instant::now();
        let out = calculate_centrality_graph(&graph);
        let ms = t.elapsed().as_secs_f64() * 1_000.0;
        assert!(out.len() > 2, "produced no result for {name}");
        report(&name, &graph, ms);
    }
    println!();
}

/// Head-to-head against the implementation as it stood at `cdc9a2b`, so the
/// results log carries a measured ratio rather than an estimate.
///
/// Sizes stay small because the baseline is the thing being measured: it
/// rebuilds four whole-graph `HashMap<String, _>` per source, so its cost
/// climbs as V² in allocations alone and it stops being runnable long before
/// the fixtures do.
#[test]
#[ignore]
fn centrality_old_vs_new() {
    println!("\ncalculate_centrality_graph — baseline (cdc9a2b) vs current");
    println!("  {:<10} {:>12} {:>12} {:>10}", "size", "baseline", "current", "speedup");
    for n in [200usize, 400, 800, 1_600] {
        let graph = synthetic(n, 3);

        let t = Instant::now();
        let _ = centrality_baseline::calculate_centrality_baseline(&graph);
        let old_ms = t.elapsed().as_secs_f64() * 1_000.0;

        let t = Instant::now();
        let _ = calculate_centrality_graph(&graph);
        let new_ms = t.elapsed().as_secs_f64() * 1_000.0;

        println!(
            "  n={n:<8} {old_ms:>10.1} ms {new_ms:>10.1} ms {:>9.0}×",
            old_ms / new_ms
        );
    }
    println!();
}
