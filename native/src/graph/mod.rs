//! The code graph: how it is built, and how it is queried.
//!
//! Two halves that share only the [`GraphData`] shape they hand between them,
//! so they live in two files rather than one 2k-line module:
//!
//! - [`build`] — [`IndexResult`](crate::types::IndexResult) in, [`GraphData`]
//!   out. File/symbol/folder nodes, and the name resolution that turns a call
//!   site into an edge.
//! - [`algos`] — a built [`GraphData`] in, an answer out. Traversal, shortest
//!   path, keyword search, centrality, cycles, and their result types.
//!
//! [`build_graph`] is the one function that spans both — it parses, builds and
//! re-serializes — so it stays here rather than picking a side.

mod algos;
mod build;

pub use algos::*;
pub use build::build_graph_from_index;

/// Parse an [`IndexResult`](crate::types::IndexResult) from JSON, build the
/// graph, and serialise it back.
///
/// Both ends of this are round trips an in-process caller does not need:
/// [`build_graph_from_index`] takes the typed value and returns the typed
/// graph. On a large repo the two encodings here are 162 MB in and 330 MB
/// out, and every caller in this crate already has — or immediately re-parses
/// — the typed form. Kept for the library-facing API and the tests that drive
/// the pipeline through JSON.
pub fn build_graph(index_json: String) -> String {
    let index_result: crate::types::IndexResult = match serde_json::from_str(&index_json) {
        Ok(r) => r,
        Err(_) => return "{}".to_string(),
    };

    let graph = build_graph_from_index(&index_result);
    serde_json::to_string(&graph).unwrap_or_default()
}
