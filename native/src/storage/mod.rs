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
//!   - `facts`           - per-node queryable facts (loc, degrees, is_test, ...)
//!   - `types_registry`  - canonical string labels for OverGraph nodes and edges

pub mod backends;
pub mod comments;
pub mod db;
pub mod embed;
pub mod embed_local;
pub mod facts;
pub mod ingest;
pub mod ppr;
pub mod query;
pub mod source;
pub mod sparse_stats;
pub mod store;
pub mod text;
pub mod types_registry;

pub use db::{Db, EdgeRow, NodeRow};
pub use embed::{
    Embedder, EmbedderConfig, RemoteEmbedder, DEFAULT_BASE_URL, DEFAULT_EMBEDDING_DIM,
    DEFAULT_MODEL,
};
pub use embed_local::LocalEmbedder;
pub use ingest::{
    build_texts, capture_for_graph, graph_id_set, ingest_graph, plan_incremental_ingest, prune_to_graph,
    refresh_sparse_stats, IngestPlan, IngestStats,
};
pub use comments::extract_prose_comments;
pub use facts::{FactContext, FactValue, Facts};
pub use source::{
    capture_graph_code, file_matches_hash, CapturedCode, IndexedSource, StoredSource,
};
pub use ppr::{default_edge_type_weights, run_ppr};
pub use query::{
    mmr_rerank, read_snippet, snippet_for, search_kb, semantic_search, semantic_search_w_where,
    traverse, traverse_filtered, ContextItem, DEFAULT_CONTEXT_CHARS, RankStrategy, RankedContext,
    SearchHit, SearchKbOptions, TraversalResult,
};
pub use store::{
    open_store, Direction, KnowledgeStore, NodeFilter, StoreError, StoreSet, StoreSpec,
    TraversalNode, TraversalPage,
};
pub use text::{build_node_text, collect_related_names};
