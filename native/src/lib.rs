pub mod agent_tools;
pub mod analyze;
mod graph;
mod indexer;
pub mod limits;
pub mod pattern;
pub mod storage;
pub mod types;

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