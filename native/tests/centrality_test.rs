//! Hand-checkable betweenness / degree centrality cases.
//!
//! `graph_test.rs` covers centrality by indexing real files, which makes the
//! expected numbers unknowable — those tests can only assert "not empty" and
//! "not negative". These build the graph directly so every expected value can
//! be derived on paper, which is what pins the algorithm rather than its
//! plumbing.
//!
//! Betweenness here is Brandes over a **directed** graph, counting ordered
//! pairs `(s, t)` with `s != t`, neither equal to the node being scored, and
//! normalized by `(n-1)(n-2)`.

use std::collections::HashMap;
use ultragraph::{calculate_centrality, types::GraphData, CentralityResult};

/// Build graph JSON from ids and directed `Calls` edges. Node type is
/// irrelevant to centrality — every node is `Function` so the fixture stays
/// about its shape.
fn graph_json(ids: &[&str], edges: &[(&str, &str)]) -> String {
    let nodes: Vec<_> = ids
        .iter()
        .map(|id| serde_json::json!({ "id": id, "name": id, "node_type": "Function" }))
        .collect();
    let edges: Vec<_> = edges
        .iter()
        .map(|(s, t)| serde_json::json!({ "source": s, "target": t, "edge_type": "Calls" }))
        .collect();
    serde_json::json!({ "nodes": nodes, "edges": edges }).to_string()
}

fn centrality(ids: &[&str], edges: &[(&str, &str)]) -> CentralityResult {
    let graph: GraphData =
        serde_json::from_str(&graph_json(ids, edges)).expect("fixture graph parses");
    calculate_centrality(&graph)
}

/// Compare a full betweenness map against hand-computed values.
fn assert_betweenness(actual: &HashMap<String, f64>, expected: &[(&str, f64)]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "scored {} nodes, expected {}",
        actual.len(),
        expected.len()
    );
    for (id, want) in expected {
        let got = *actual.get(*id).unwrap_or_else(|| panic!("{id} missing from betweenness"));
        assert!(
            (got - want).abs() < 1e-9,
            "betweenness[{id}] = {got}, expected {want}"
        );
    }
}

/// A→B→C→D→E. Every shortest path runs forward along the chain, so the
/// interior nodes carry the traffic and the two ends carry none.
///
/// Ordered pairs each interior node sits between:
///   B: (A,C) (A,D) (A,E)                   = 3
///   C: (A,D) (A,E) (B,D) (B,E)             = 4
///   D: (A,E) (B,E) (C,E)                   = 3
/// Normalizer (n-1)(n-2) = 4·3 = 12.
#[test]
fn betweenness_path_graph() {
    let r = centrality(
        &["A", "B", "C", "D", "E"],
        &[("A", "B"), ("B", "C"), ("C", "D"), ("D", "E")],
    );
    assert_betweenness(
        &r.betweenness_centrality,
        &[
            ("A", 0.0),
            ("B", 3.0 / 12.0),
            ("C", 4.0 / 12.0),
            ("D", 3.0 / 12.0),
            ("E", 0.0),
        ],
    );
}

/// Two sources into one hub, two sinks out of it. Every cross pair
/// (I1,O1) (I1,O2) (I2,O1) (I2,O2) routes through H and nothing else does.
/// raw(H) = 4, normalizer = 4·3 = 12.
///
/// This is the shape a real bridge has, and the one a betweenness score
/// exists to find.
#[test]
fn betweenness_hub() {
    let r = centrality(
        &["I1", "I2", "H", "O1", "O2"],
        &[("I1", "H"), ("I2", "H"), ("H", "O1"), ("H", "O2")],
    );
    assert_betweenness(
        &r.betweenness_centrality,
        &[
            ("I1", 0.0),
            ("I2", 0.0),
            ("H", 4.0 / 12.0),
            ("O1", 0.0),
            ("O2", 0.0),
        ],
    );
}

/// A→{B,C}→D. The single pair (A,D) has two shortest paths of equal length,
/// so B and C split its credit half and half — this is the case that pins
/// the σ path-counting, which a BFS that merely records *a* parent gets wrong.
/// raw(B) = raw(C) = 0.5, normalizer = 3·2 = 6.
#[test]
fn betweenness_splits_credit_across_equal_paths() {
    let r = centrality(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")],
    );
    assert_betweenness(
        &r.betweenness_centrality,
        &[
            ("A", 0.0),
            ("B", 0.5 / 6.0),
            ("C", 0.5 / 6.0),
            ("D", 0.0),
        ],
    );
}

/// A short branch and a long one to the same sink: A→B→D (2 hops) versus
/// A→C1→C2→D (3 hops). Only the short one is a shortest path, so C1 and C2
/// get no credit for (A,D) — they are paid only for the pairs they genuinely
/// sit between.
///
///   B:  (A,D)   = 1
///   C1: (A,C2)  = 1
///   C2: (C1,D)  = 1
/// Normalizer = 4·3 = 12.
#[test]
fn betweenness_ignores_longer_detour() {
    let r = centrality(
        &["A", "B", "C1", "C2", "D"],
        &[("A", "B"), ("B", "D"), ("A", "C1"), ("C1", "C2"), ("C2", "D")],
    );
    assert_betweenness(
        &r.betweenness_centrality,
        &[
            ("A", 0.0),
            ("B", 1.0 / 12.0),
            ("C1", 1.0 / 12.0),
            ("C2", 1.0 / 12.0),
            ("D", 0.0),
        ],
    );
}

/// Edges are directed: nothing reaches back up the chain, so reversing the
/// question gives every node a zero. Guards against a rewrite that quietly
/// symmetrizes the graph — undirected betweenness on the same path graph
/// would score B, C and D non-zero from both ends.
#[test]
fn betweenness_is_directed() {
    let r = centrality(&["A", "B", "C"], &[("B", "A"), ("C", "B")]);
    // Only pair with an intermediate is (C,A) via B: raw(B) = 1.
    // Normalizer = 2·1 = 2.
    assert_betweenness(
        &r.betweenness_centrality,
        &[("A", 0.0), ("B", 0.5), ("C", 0.0)],
    );
}

/// Degree centrality counts both endpoints of every incident edge and
/// divides by `n-1`. The hub touches all four edges; each leaf touches one.
#[test]
fn degree_centrality_counts_both_directions() {
    let r = centrality(
        &["I1", "I2", "H", "O1", "O2"],
        &[("I1", "H"), ("I2", "H"), ("H", "O1"), ("H", "O2")],
    );
    let d = &r.degree_centrality;
    assert!((d["H"] - 4.0 / 4.0).abs() < 1e-9, "hub degree = {}", d["H"]);
    for leaf in ["I1", "I2", "O1", "O2"] {
        assert!(
            (d[leaf] - 1.0 / 4.0).abs() < 1e-9,
            "{leaf} degree = {}",
            d[leaf]
        );
    }
}

/// A disconnected node is scored, not skipped — it simply scores zero. A
/// rewrite that sizes its accumulators from the reachable set rather than
/// the node list would drop it.
#[test]
fn isolated_node_is_scored_zero() {
    let r = centrality(&["A", "B", "C", "LONELY"], &[("A", "B"), ("B", "C")]);
    assert_betweenness(
        &r.betweenness_centrality,
        &[
            ("A", 0.0),
            ("B", 1.0 / 6.0), // pair (A,C)
            ("C", 0.0),
            ("LONELY", 0.0),
        ],
    );
    assert!((r.degree_centrality["LONELY"]).abs() < 1e-9);
}

/// Two nodes: the normalizer `(n-1)(n-2)` is zero, so the guard against
/// dividing by it has to hold and every score stays finite.
#[test]
fn two_node_graph_does_not_divide_by_zero() {
    let r = centrality(&["A", "B"], &[("A", "B")]);
    for (id, v) in &r.betweenness_centrality {
        assert!(v.is_finite(), "betweenness[{id}] = {v}");
        assert!(v.abs() < 1e-9, "betweenness[{id}] = {v}, expected 0");
    }
    for (id, v) in &r.degree_centrality {
        assert!(v.is_finite(), "degree[{id}] = {v}");
    }
}

/// An edge naming a node that is not in the node list must not panic or
/// corrupt the scores — `build_graph` can emit these when resolution points
/// at something outside the indexed set.
#[test]
fn dangling_edge_endpoint_is_ignored() {
    let r = centrality(&["A", "B", "C"], &[("A", "B"), ("B", "C"), ("B", "GHOST")]);
    assert_betweenness(
        &r.betweenness_centrality,
        &[("A", 0.0), ("B", 1.0 / 2.0), ("C", 0.0)],
    );
}
