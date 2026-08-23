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

pub fn build_graph(index_json: String) -> String {
    let index_result: crate::types::IndexResult = match serde_json::from_str(&index_json) {
        Ok(r) => r,
        Err(_) => return "{}".to_string(),
    };

    let graph = build::build_graph_from_index(&index_result);
    serde_json::to_string(&graph).unwrap_or_default()
}
