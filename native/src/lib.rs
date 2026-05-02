mod graph;
mod indexer;
pub mod storage;
pub mod types;

pub use graph::{
    build_graph, calculate_centrality, detect_cycles, filter_edges_by_type, find_shortest_path,
    graph_keyword_search, k_hop_bfs,
};
pub use indexer::{index, index_with_cache};
pub use types::*;

// --- Shared Color Constants ---
pub const C_CYAN: &str = "\x1b[36m";
pub const C_MAGENTA: &str = "\x1b[35m";
pub const C_YELLOW: &str = "\x1b[33m";
pub const C_GREEN: &str = "\x1b[32m";
pub const C_BLUE: &str = "\x1b[34m";
pub const C_RESET: &str = "\x1b[0m";
pub const C_BOLD: &str = "\x1b[1m";