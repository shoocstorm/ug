//! Cycle detection over a hand-built graph.
//!
//! `graph_test.rs` drives `detect_cycles` through `index()` on real files and
//! asserts only that acyclic input stays acyclic. Nothing there asserts that a
//! cycle is ever *found*, so a `detect_cycles` that returned "no cycles"
//! unconditionally passed the suite. These build the edges directly, the way
//! `traversal_test.rs` does, so every expected answer is derivable on paper.
//!
//! # The shape of a reported cycle
//!
//! Two properties of the output are easy to misread, and are pinned below:
//!
//! 1. Each cycle comes back **sorted**, not in traversal order. It names the
//!    set of nodes the loop passes through; the route through them is not
//!    recoverable from the result.
//! 2. The node the walk closed on appears **twice** — `A -> B -> A` reports
//!    `["A", "A", "B"]`. So `cycle.len()` is one more than the number of
//!    distinct nodes involved. This is long-standing output shape, pinned
//!    here as the contract it is rather than corrected in passing.

use ultragraph::{detect_cycles, types::GraphData, CycleResult};

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

fn cycles(ids: &[&str], edges: &[(&str, &str)]) -> CycleResult {
    detect_cycles(&graph(ids, edges))
}

// ---------- a cycle is actually found ----------

#[test]
fn a_two_node_loop_is_a_cycle() {
    let r = cycles(&["A", "B"], &[("A", "B"), ("B", "A")]);
    assert!(r.has_cycles);
    assert_eq!(r.cycles, vec![vec!["A", "A", "B"]]);
}

#[test]
fn a_three_node_loop_is_a_cycle() {
    let r = cycles(&["A", "B", "C"], &[("A", "B"), ("B", "C"), ("C", "A")]);
    assert!(r.has_cycles);
    assert_eq!(r.cycles, vec![vec!["A", "A", "B", "C"]]);
}

#[test]
fn a_self_loop_is_a_cycle() {
    // Direct recursion. The walk meets the start node while it is still on
    // the recursion stack, which is the same back edge as any longer loop.
    let r = cycles(&["A"], &[("A", "A")]);
    assert!(r.has_cycles);
    assert_eq!(r.cycles, vec![vec!["A", "A"]]);
}

// ---------- and is not found where there is none ----------

#[test]
fn a_diamond_is_not_a_cycle() {
    // The case a depth-first walk gets wrong when it treats "seen before" as
    // "cycle": D is reached twice, once through B and once through C, but it
    // is never on the path back to A. Only a node still on the recursion
    // stack closes a loop.
    let r = cycles(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")],
    );
    assert!(!r.has_cycles);
    assert!(r.cycles.is_empty());
}

#[test]
fn a_shared_tail_visited_twice_is_not_a_cycle() {
    // Same trap one hop deeper: the whole chain D -> E is walked under B,
    // then met again under C after it has been popped off the stack.
    let r = cycles(
        &["A", "B", "C", "D", "E"],
        &[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D"), ("D", "E")],
    );
    assert!(!r.has_cycles);
}

#[test]
fn an_edge_to_an_unknown_node_is_ignored() {
    // A dangling target has no index, so the walk must skip it rather than
    // index out of bounds.
    let r = cycles(&["A", "B"], &[("A", "B"), ("B", "GHOST")]);
    assert!(!r.has_cycles);
}

// ---------- every cycle, wherever it sits ----------

#[test]
fn two_disjoint_cycles_are_both_reported() {
    let r = cycles(
        &["A", "B", "C", "D"],
        &[("A", "B"), ("B", "A"), ("C", "D"), ("D", "C")],
    );
    assert_eq!(r.cycles, vec![vec!["A", "A", "B"], vec!["C", "C", "D"]]);
}

#[test]
fn a_cycle_is_found_even_when_the_first_root_cannot_reach_it() {
    // Z is walked first and reaches nothing. The outer loop has to try every
    // unvisited node as a root, not just the first one.
    let r = cycles(&["Z", "A", "B"], &[("A", "B"), ("B", "A")]);
    assert!(r.has_cycles);
    assert_eq!(r.cycles, vec![vec!["A", "A", "B"]]);
}

#[test]
fn a_tail_leading_into_a_cycle_is_not_part_of_it() {
    // T calls into the loop but nothing returns to T, so T is not in it.
    let r = cycles(&["T", "A", "B"], &[("T", "A"), ("A", "B"), ("B", "A")]);
    assert_eq!(r.cycles, vec![vec!["A", "A", "B"]]);
}

#[test]
fn the_same_loop_entered_from_two_places_is_reported_once() {
    // Both X and Y lead into the same A/B loop. Sorting each cycle before
    // de-duplication is what makes the two discoveries compare equal.
    let r = cycles(
        &["X", "Y", "A", "B"],
        &[("X", "A"), ("Y", "B"), ("A", "B"), ("B", "A")],
    );
    assert_eq!(r.cycles.len(), 1);
}

// ---------- the same graph answers the same way twice ----------

#[test]
fn cycle_order_is_stable_across_runs() {
    // De-duplication goes through a `HashSet`, which drains in its own
    // order. Before this was sorted, three cycles came back in all six
    // orders across 40 builds, so `ug graph_cycles` could not be diffed
    // against its own previous output.
    let ids = ["A", "B", "C", "D", "E", "F"];
    let edges = [
        ("A", "B"),
        ("B", "A"),
        ("C", "D"),
        ("D", "C"),
        ("E", "F"),
        ("F", "E"),
    ];
    let first = cycles(&ids, &edges).cycles;
    assert_eq!(
        first,
        vec![
            vec!["A", "A", "B"],
            vec!["C", "C", "D"],
            vec!["E", "E", "F"]
        ]
    );
    for _ in 0..20 {
        assert_eq!(cycles(&ids, &edges).cycles, first);
    }
}

#[test]
fn an_empty_graph_has_no_cycles() {
    let r = cycles(&[], &[]);
    assert!(!r.has_cycles);
    assert!(r.cycles.is_empty());
}
