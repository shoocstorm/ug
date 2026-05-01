//! Phase 3: semantic storage on top of LanceDB + a local embedding model.
//!
//! Module layout:
//!   - `text`   - shape per-node embedding text
//!   - `embed`  - HTTP client to OpenAI-compatible /v1/embeddings
//!   - `db`     - LanceDB schemas and upsert
//!   - `query`  - semantic / hybrid / traversal queries
//!   - `ingest` - graph -> embed -> upsert pipeline

pub mod db;
pub mod embed;
pub mod ingest;
pub mod napi_bindings;
pub mod query;
pub mod text;

pub use db::{Db, EdgeRow, NodeRow};
pub use embed::{Embedder, EmbedderConfig, EMBEDDING_DIM};
pub use ingest::{ingest_graph, reembed_nodes, IngestStats};
pub use query::{
    hybrid_search, mmr_rerank, read_snippet, rrf_search, search_kb, semantic_search, traverse,
    traverse_filtered, ContextItem, Direction, RankedContext, SearchHit, SearchKbOptions,
    TraversalResult,
};
pub use text::{build_node_text, collect_related_names};
