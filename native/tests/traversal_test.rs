//! Shortest-path (P1.2) and k-hop BFS (P1.3) over a hand-built graph.
//!
//! `graph_test.rs` drives both through `index()` on real files, where the
//! resulting shape is whatever the extractors happened to produce — fine for
//! "does it run", useless for pinning traversal order or hop distance. These
//! build the edges directly so every expected answer is derivable on paper.

use std::collections::HashMap;
use ultragraph::{
    find_shortest_path, find_shortest_path_graph, k_hop_bfs,
    types::{BfsResult, GraphData, PathResult},
};

fn graph(ids: &[&str], edges: &[(&str, &str)]) -> GraphData {
    let nodes: Vec<_> = ids
        .iter()
        .map(|id| serde_json::json!({ "id": id, "name": id, "node_type": "Function" }))
        .collect();
    let edges: Vec<_> = edges
        .iter()
        .map(|(s, t)| serde_json::json!({ "source": s, "target": t, "edge_type": "Calls" }))
        .collect();
    serde_json::from_value(serde_json::json!({ "nodes": nodes, "edges": edges })).unwrap()
}

fn json(g: &GraphData) -> String {
    serde_json::to_string(g).unwrap()
}

fn bfs(g: &GraphData, start: &str, k: u32) -> BfsResult {
    serde_json::from_str(&k_hop_bfs(json(g), start.to_string(), k)).unwrap()
}

// ---------- P1.2: shortest path ----------

#[test]
fn path_follows_the_chain() {
    let g = graph(&["A", "B", "C", "D"], &[("A", "B"), ("B", "C"), ("C", "D")]);
    let r = find_shortest_path_graph(&g, "A", "D");
    assert!(r.found);
    assert_eq!(r.path, vec!["A", "B", "C", "D"]);
    assert_eq!(r.length, Some(3));
}

/// A short hop and a long way round to the same node: the answer is the short
/// one. The old implementation queued a full cloned path per edge and marked
/// `visited` on dequeue, so this is also the case that used to enqueue the
/// same node repeatedly.
#[test]
fn path_takes_the_shorter_of_two_routes() {
    let g = graph(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("B", "C"), ("C", "D"), ("A", "D")],
    );
    let r = find_shortest_path_graph(&g, "A", "D");
    assert!(r.found);
    assert_eq!(r.path, vec!["A", "D"]);
    assert_eq!(r.length, Some(1));
}

/// Edges are directed — there is no path back up the chain.
#[test]
fn path_does_not_walk_edges_backwards() {
    let g = graph(&["A", "B", "C"], &[("A", "B"), ("B", "C")]);
    assert!(find_shortest_path_graph(&g, "A", "C").found);

    let r = find_shortest_path_graph(&g, "C", "A");
    assert!(!r.found);
    assert!(r.path.is_empty());
    assert_eq!(r.length, None);
}

/// A node is trivially zero hops from itself, and the path is just itself.
#[test]
fn path_from_a_node_to_itself_is_zero_length() {
    let g = graph(&["A", "B"], &[("A", "B")]);
    let r = find_shortest_path_graph(&g, "A", "A");
    assert!(r.found);
    assert_eq!(r.path, vec!["A"]);
    assert_eq!(r.length, Some(0));
}

#[test]
fn path_to_an_unknown_node_is_not_found() {
    let g = graph(&["A", "B"], &[("A", "B")]);
    for (s, t) in [("A", "GHOST"), ("GHOST", "A"), ("GHOST", "OTHER")] {
        let r = find_shortest_path_graph(&g, s, t);
        assert!(!r.found, "{s} -> {t} should not be found");
    }
}

/// Two shortest paths of equal length: either is a correct answer, but it
/// must be a real path of the right length and not a splice of both.
#[test]
fn path_through_a_diamond_is_a_real_two_hop_route() {
    let g = graph(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")],
    );
    let r = find_shortest_path_graph(&g, "A", "D");
    assert!(r.found);
    assert_eq!(r.length, Some(2));
    assert_eq!(r.path.len(), 3);
    assert_eq!(r.path[0], "A");
    assert_eq!(r.path[2], "D");
    assert!(r.path[1] == "B" || r.path[1] == "C", "via {}", r.path[1]);
}

/// The `String` wrapper must agree with the entry point it now delegates to.
#[test]
fn the_json_wrapper_matches_the_graph_entry_point() {
    let g = graph(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("B", "C"), ("C", "D"), ("A", "D")],
    );
    let direct = find_shortest_path_graph(&g, "A", "D");
    let via_json: PathResult =
        serde_json::from_str(&find_shortest_path(json(&g), "A".into(), "D".into())).unwrap();
    assert_eq!(direct.found, via_json.found);
    assert_eq!(direct.path, via_json.path);
    assert_eq!(direct.length, via_json.length);
}

// ---------- P1.3: k-hop BFS ----------

fn distances(r: &BfsResult) -> HashMap<String, u32> {
    r.distances.clone()
}

/// **The regression this rewrite exists for.**
///
/// `A→B→C→D` plus a direct `A→D`. D is one hop from A. The old walk used
/// `Vec::pop` (LIFO) and marked `visited` when a node came *off* the queue,
/// so it descended A→B→C→D and recorded D at distance 3 before it ever got
/// back to the one-hop entry — then discarded the correct distance because D
/// was already marked.
#[test]
fn hop_distance_is_the_shortest_not_the_first_found() {
    let g = graph(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("B", "C"), ("C", "D"), ("A", "D")],
    );
    let d = distances(&bfs(&g, "A", 3));
    assert_eq!(d["A"], 0);
    assert_eq!(d["B"], 1);
    assert_eq!(d["C"], 2);
    assert_eq!(d["D"], 1, "D is a direct neighbour of A");
}

#[test]
fn k_bounds_how_far_the_walk_reaches() {
    let g = graph(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("B", "C"), ("C", "D")],
    );

    let d0 = distances(&bfs(&g, "A", 0));
    assert_eq!(d0.len(), 1, "k=0 reaches only the start");
    assert_eq!(d0["A"], 0);

    let d1 = distances(&bfs(&g, "A", 1));
    assert_eq!(d1.len(), 2);
    assert_eq!(d1["B"], 1);

    let d2 = distances(&bfs(&g, "A", 2));
    assert_eq!(d2.len(), 3);
    assert!(!d2.contains_key("D"), "D is three hops out");
}

/// Only edges with *both* endpoints reached come back, each exactly once —
/// an edge sits in the incident list of both its endpoints, so the dedup is
/// load-bearing.
#[test]
fn induced_edges_are_those_among_reached_nodes_only() {
    let g = graph(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("B", "C"), ("C", "D"), ("A", "D")],
    );
    let r = bfs(&g, "A", 1);

    let mut got: Vec<(String, String)> = r
        .edges
        .iter()
        .map(|e| (e.source.clone(), e.target.clone()))
        .collect();
    let before_dedup = got.len();
    got.sort();
    got.dedup();
    assert_eq!(before_dedup, got.len(), "an edge was returned twice");

    // Reached at k=1: A, B, D. Among them: A→B and A→D. B→C and C→D are out.
    assert_eq!(
        got,
        vec![
            ("A".to_string(), "B".to_string()),
            ("A".to_string(), "D".to_string()),
        ]
    );
}

/// The response keeps `graph.edges` order rather than whatever order the
/// traversal happened to reach them in — the edge list is positional to
/// consumers that zip it against the original.
#[test]
fn induced_edges_keep_graph_order() {
    // Declared deliberately out of traversal order: the A→C edge is last.
    let g = graph(
        &["A", "B", "C"],
        &[("A", "B"), ("B", "C"), ("A", "C")],
    );
    let r = bfs(&g, "A", 2);
    let got: Vec<(&str, &str)> = r
        .edges
        .iter()
        .map(|e| (e.source.as_str(), e.target.as_str()))
        .collect();
    assert_eq!(got, vec![("A", "B"), ("B", "C"), ("A", "C")]);
}

/// Node order likewise follows `graph.nodes`, not discovery.
#[test]
fn reached_nodes_keep_graph_order() {
    let g = graph(
        &["Z", "Y", "X"],
        &[("Z", "Y"), ("Y", "X")],
    );
    let r = bfs(&g, "Z", 2);
    let got: Vec<&str> = r.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(got, vec!["Z", "Y", "X"]);
}

#[test]
fn an_unknown_start_returns_nothing() {
    let g = graph(&["A", "B"], &[("A", "B")]);
    let r = bfs(&g, "GHOST", 3);
    assert!(r.nodes.is_empty());
    assert!(r.edges.is_empty());
    assert!(r.distances.is_empty());
}

/// A cycle must terminate rather than spin, and each node keeps its shortest
/// distance.
#[test]
fn a_cycle_terminates() {
    let g = graph(
        &["A", "B", "C"],
        &[("A", "B"), ("B", "C"), ("C", "A")],
    );
    let d = distances(&bfs(&g, "A", 8));
    assert_eq!(d["A"], 0);
    assert_eq!(d["B"], 1);
    assert_eq!(d["C"], 2);
    assert_eq!(d.len(), 3);
}

/// An edge pointing at something outside the node set is dropped rather than
/// panicking on the lookup.
#[test]
fn a_dangling_edge_is_ignored() {
    let g = graph(&["A", "B"], &[("A", "B"), ("B", "GHOST")]);
    let r = bfs(&g, "A", 3);
    let d = distances(&r);
    assert_eq!(d.len(), 2);
    assert_eq!(r.edges.len(), 1);
}
