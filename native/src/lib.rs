//! UltraGraph: the whole crate, library and CLI alike.
//!
//! Everything lives here rather than under `src/main.rs` so that the binary's
//! own modules (`cli`, `serve`, `chat`, `tour`, `project`, `config`, `mcp`)
//! are compiled *once*, into this rlib, instead of a second time into a
//! `ug(bin test)` harness. `src/main.rs` is a three-line shim over
//! [`cli::run`], and `[[bin]] test = false` in `Cargo.toml` is what stops
//! Cargo from generating that second harness. Their `#[cfg(test)]` modules
//! now run inside the lib test binary alongside the rest.
//!
//! The four link units an edit under `src/` used to trigger — lib, lib test,
//! bin, bin test, each a fresh ~60-160 MB Mach-O — are three.

// The modules moved up from `main.rs` refer to library items by the crate's
// external name (`ultragraph::storage::…`, ~150 sites) because that is what
// they were: an outside consumer of this rlib. Binding the crate to its own
// name keeps those paths resolving now that the two are one crate, without
// rewriting every import to `crate::`.
extern crate self as ultragraph;

pub mod agent_tools;
pub mod analyze;
mod graph;
mod indexer;
pub mod limits;
pub mod pattern;
pub mod storage;
pub mod style;
pub mod types;

// Formerly `main.rs`'s module tree. Only `cli` is public — it is all
// `src/main.rs` needs — so the rest keep the crate-internal visibility they
// had as sibling modules of the binary root.
mod assets;
mod chat;
pub mod cli;
mod config;
mod mcp;
mod project;
mod serve;
mod tour;

pub use graph::{
    build_graph, build_graph_from_index, calculate_centrality, detect_cycles,
    filter_edges_by_type, find_shortest_path, graph_keyword_search, k_hop_bfs, BfsResult,
    CentralityResult, CycleResult, FilteredEdgesResult, PathResult, SearchResult,
};
pub use indexer::{index, index_typed, index_with_cache, index_with_cache_typed};
pub(crate) use indexer::write_json_file_checked;
// `C_*`, the `color` gate and `Render` used to live here; keeping the glob
// means `ultragraph::C_CYAN` / `ultragraph::color::set` still resolve.
pub use style::*;
pub use types::*;
