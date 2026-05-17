//! Phase 3: semantic storage on top of a pluggable graph backend.
//!
//! Module layout:
//!   - `text`            - shape per-node embedding text + sparse keyword vectors
//!   - `embed`           - HTTP client to OpenAI-compatible /v1/embeddings
//!   - `db`              - OverGraph engine wrapper (also implements `KnowledgeStore`)
//!   - `backends::neo4j` - Neo4j driver wrapper implementing `KnowledgeStore`
//!   - `store`           - the `KnowledgeStore` trait + portable types + `open_store`
//!   - `query`           - semantic / hybrid / traversal queries (over `&dyn KnowledgeStore`)
//!   - `ingest`          - graph -> embed -> upsert pipeline (single + multi destination)
//!   - `ppr`             - thin wrapper around `KnowledgeStore::personalized_pagerank`
//!   - `types_registry`  - stable string ↔ u32 mapping for OverGraph type ids

pub mod backends;
pub mod db;
pub mod embed;
pub mod embed_local;
pub mod ingest;
pub mod napi_bindings;
pub mod ppr;
pub mod query;
pub mod store;
pub mod text;
pub mod types_registry;

pub use db::{Db, EdgeRow, NodeRow};
pub use embed::{Embedder, EmbedderConfig, RemoteEmbedder, DEFAULT_EMBEDDING_DIM, EMBEDDING_DIM};
pub use embed_local::LocalEmbedder;
pub use ingest::{ingest_graph, ingest_graph_multi, reembed_nodes, IngestStats};
pub use ppr::{default_edge_type_weights, run_ppr};
pub use query::{
    mmr_rerank, read_snippet, rrf_search, search_kb, semantic_search, semantic_search_w_where,
    traverse, traverse_filtered, ContextItem, RankStrategy, RankedContext, SearchHit,
    SearchKbOptions, TraversalResult,
};
pub use store::{
    open_store, Direction, KnowledgeStore, NodeFilter, StoreError, StoreSet, StoreSpec,
    TraversalNode, TraversalPage,
};
pub use text::{build_node_text, collect_related_names};
