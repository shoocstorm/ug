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
pub mod types;

// Formerly `main.rs`'s module tree. Only `cli` is public — it is all
// `src/main.rs` needs — so the rest keep the crate-internal visibility they
// had as sibling modules of the binary root.
pub mod cli;
mod assets;
mod chat;
mod config;
mod mcp;
mod project;
mod serve;
mod tour;

pub use graph::{
    build_graph, calculate_centrality, calculate_centrality_graph, detect_cycles,
    detect_cycles_graph, filter_edges_by_type, find_shortest_path, find_shortest_path_graph, graph_keyword_search, k_hop_bfs, k_hop_bfs_graph,
};
pub use indexer::{index, index_with_cache};
pub use types::*;

// --- Shared Color Constants ---
//
// These are the escape codes used when colour is ON. The runtime gate lives
// in [`color`] below: when it is off, the `Render::Ansi` styling helpers in
// `agent_tools` return the plain string instead of wrapping it in these
// codes, so every agent-tool / `analyze` command emits plain text when piped
// or when the caller asks for it. Human-facing CLI banners (in `main.rs`)
// keep using these constants directly and stay coloured in a terminal.
pub const C_CYAN: &str = "\x1b[36m";
pub const C_MAGENTA: &str = "\x1b[35m";
pub const C_YELLOW: &str = "\x1b[33m";
pub const C_GREEN: &str = "\x1b[32m";
pub const C_RED: &str = "\x1b[31m";
pub const C_BLUE: &str = "\x1b[34m";
pub const C_RESET: &str = "\x1b[0m";
pub const C_BOLD: &str = "\x1b[1m";
pub const C_DIM: &str = "\x1b[2m";

/// Runtime colour gate for the agent-tool / `analyze` renderers.
///
// Set once at process start from `--no-color`, `NO_COLOR`, and whether
// stdout is a terminal. The `Render::Ansi` styling helpers consult
// [`enabled`] so that piping `ug` (or any non-tty consumer — an LLM, a
// log shipper) gets plain text without every command rewriting its format
// strings. `Render::Markdown` is already colour-free and is unaffected.
pub mod color {
    use std::sync::atomic::{AtomicBool, Ordering};

    static ENABLED: AtomicBool = AtomicBool::new(true);

    /// Set the gate. Call once near the top of `main`.
    pub fn set(on: bool) {
        ENABLED.store(on, Ordering::Relaxed);
    }

    /// Whether the `Render::Ansi` styling helpers should emit escape codes.
    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }
}