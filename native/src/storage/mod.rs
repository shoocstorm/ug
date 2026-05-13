//! Phase 3: semantic storage on top of OverGraph + a local embedding model.
//!
//! Module layout:
//!   - `text`            - shape per-node embedding text + sparse keyword vectors
//!   - `embed`           - HTTP client to OpenAI-compatible /v1/embeddings
//!   - `db`              - OverGraph engine wrapper + wire-format DTOs
//!   - `query`           - semantic / hybrid / traversal queries
//!   - `ingest`          - graph -> embed -> upsert pipeline
//!   - `ppr`             - thin wrapper around OverGraph native PPR
//!   - `types_registry`  - stable string ↔ u32 mapping for OverGraph type ids

pub mod db;
pub mod embed;
pub mod embed_local;
pub mod ingest;
pub mod napi_bindings;
pub mod ppr;
pub mod query;
pub mod text;
pub mod types_registry;

pub use db::{Db, EdgeRow, NodeRow};
pub use embed::{Embedder, EmbedderConfig, RemoteEmbedder, DEFAULT_EMBEDDING_DIM, EMBEDDING_DIM};
pub use embed_local::LocalEmbedder;
pub use ingest::{ingest_graph, reembed_nodes, IngestStats};
pub use ppr::{default_edge_type_weights, run_ppr};
pub use query::{
    semantic_search_w_where, mmr_rerank, read_snippet, rrf_search, search_kb, semantic_search, traverse,
    traverse_filtered, ContextItem, Direction, RankStrategy, RankedContext, SearchHit,
    SearchKbOptions, TraversalResult,
};
pub use text::{build_node_text, collect_related_names};
