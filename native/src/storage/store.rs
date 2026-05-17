//! `KnowledgeStore`: the pluggable storage abstraction.
//!
//! Every backend (today: OverGraph; coming: Neo4j) implements one trait so
//! the upper layers (`ingest`, `query`, `ppr`, `serve`, `napi_bindings`) can
//! run against any of them. The wire-format DTOs `NodeRow` / `EdgeRow` are
//! shared; only persistence and search differ.
//!
//! See `docs/MULTI-DEST-PLAN.md` for the architectural rationale.

use crate::storage::db::{DbError, EdgeRow, NodeRow};
use async_trait::async_trait;
use std::path::PathBuf;

/// Direction of edge expansion during graph traversal and PPR.
/// Defined here so the trait module is self-contained — `query.rs`
/// re-exports this for back-compat call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outbound,
    Inbound,
    Both,
}

impl Direction {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "in" | "inbound" | "incoming" => Direction::Inbound,
            "both" | "all" | "any" => Direction::Both,
            _ => Direction::Outbound,
        }
    }
}

/// Backend-portable filter for vector / hybrid search.
///
/// v1 supports node-type filtering only. Arbitrary SQL-like `WHERE`
/// strings (the legacy CLI `--filter` form) are parsed via
/// `from_legacy_where` and degrade to no-op when the parser can't
/// recognize the predicate. The OverGraph backend ignored the legacy
/// argument anyway (see `MIGRATION-OVERGRAPH §6 Q1`), so this isn't a
/// regression.
#[derive(Debug, Clone, Default)]
pub struct NodeFilter {
    pub node_types: Option<Vec<String>>,
}

impl NodeFilter {
    pub fn type_only<I, S>(types: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            node_types: Some(types.into_iter().map(Into::into).collect()),
        }
    }

    /// Parse a tiny subset of SQL `WHERE` for back-compat. Recognizes
    /// `node_type = 'X'` and `node_type IN ('X','Y',...)` only. Anything
    /// else returns `None` (caller should treat that as "no filter").
    /// Detection is case-insensitive on the predicate; values inside
    /// quotes preserve their original case.
    pub fn from_legacy_where(s: &str) -> Option<Self> {
        parse_with_case(s.trim())
    }
}

fn strip_quotes(s: &str) -> Option<String> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

/// Re-parses with original case preserved on the values (the lowercase
/// path above is only used to detect the predicate structure).
fn parse_with_case(s: &str) -> Option<NodeFilter> {
    let lc = s.to_ascii_lowercase();
    let idx_eq = lc.find('=');
    let idx_in = lc.find(" in ");
    if let Some(i) = idx_eq {
        if lc[..i].trim() == "node_type" {
            let v = strip_quotes(s[i + 1..].trim())?;
            return Some(NodeFilter::type_only([v]));
        }
    }
    if let Some(i) = idx_in {
        if lc[..i].trim() == "node_type" {
            let rest = s[i + 4..].trim();
            if rest.starts_with('(') && rest.ends_with(')') {
                let inner = &rest[1..rest.len() - 1];
                let vals: Vec<String> =
                    inner.split(',').filter_map(|c| strip_quotes(c.trim())).collect();
                if !vals.is_empty() {
                    return Some(NodeFilter::type_only(vals));
                }
            }
        }
    }
    None
}

/// One node + its hop distance from the traversal seed.
#[derive(Debug, Clone)]
pub struct TraversalNode {
    pub row: NodeRow,
    pub depth: u32,
}

/// Result of [`KnowledgeStore::traverse`].
#[derive(Debug, Default, Clone)]
pub struct TraversalPage {
    /// Reachable nodes (including the seed at depth 0).
    pub nodes: Vec<TraversalNode>,
    /// Edges traversed, deduplicated by `(source, edge_type, target)`.
    pub edges: Vec<EdgeRow>,
}

/// Errors returned by `KnowledgeStore` operations. Each backend lowers
/// its native error into one of these variants.
#[derive(Debug)]
pub enum StoreError {
    /// Backend-specific error message (OverGraph engine, Neo4j Bolt, …).
    Backend(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The operation isn't supported on this backend (e.g. PPR on Neo4j
    /// without the GDS plugin).
    Unsupported(&'static str),
    BadVector {
        id: String,
        got: usize,
        want: usize,
    },
    UnknownEndpoint(String),
    DimMismatch {
        existing: u32,
        requested: u32,
    },
    /// Auth / connection failures (Neo4j specific).
    Auth(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Backend(e) => write!(f, "backend error: {}", e),
            StoreError::Io(e) => write!(f, "io error: {}", e),
            StoreError::Json(e) => write!(f, "json error: {}", e),
            StoreError::Unsupported(s) => write!(f, "unsupported: {}", s),
            StoreError::BadVector { id, got, want } => {
                write!(f, "vector for {} has dim {}, expected {}", id, got, want)
            }
            StoreError::UnknownEndpoint(s) => write!(f, "unknown edge endpoint: {}", s),
            StoreError::DimMismatch {
                existing,
                requested,
            } => write!(
                f,
                "embedding dim mismatch: store was created with dim {}, requested {}",
                existing, requested
            ),
            StoreError::Auth(s) => write!(f, "auth error: {}", s),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<DbError> for StoreError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::Io(e) => StoreError::Io(e),
            DbError::Json(e) => StoreError::Json(e),
            DbError::Unimplemented(s) => StoreError::Unsupported(s),
            DbError::BadVector { id, got, want } => StoreError::BadVector { id, got, want },
            DbError::UnknownEndpoint(s) => StoreError::UnknownEndpoint(s),
            DbError::DimMismatch {
                existing,
                requested,
            } => StoreError::DimMismatch {
                existing,
                requested,
            },
            DbError::Engine(e) => StoreError::Backend(format!("overgraph: {}", e)),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

/// The pluggable storage trait. Every backend implements this; callers
/// in `query.rs`, `ingest.rs`, etc. take `&dyn KnowledgeStore`.
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    /// Dense embedding dimension the store was opened with.
    fn embedding_dim(&self) -> u32;

    /// Whether [`personalized_pagerank`] runs natively on this backend.
    /// When `false`, `query::search_kb`'s PPR strategy falls back to MMR
    /// automatically with a single warning log line.
    ///
    /// [`personalized_pagerank`]: KnowledgeStore::personalized_pagerank
    fn supports_native_ppr(&self) -> bool;

    /// Short human-readable backend identifier (`"overgraph"`, `"neo4j"`).
    /// Used for logging and the `/api/capabilities` endpoint.
    fn backend_name(&self) -> &'static str;

    async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), StoreError>;
    async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), StoreError>;

    async fn vector_search(
        &self,
        query: Vec<f32>,
        k: usize,
        filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError>;

    /// Dense + keyword fusion. `sparse` is OverGraph's pre-tokenized
    /// sparse vector (FNV-hashed dimensions, term-frequency weights);
    /// `query_text` is the original text Neo4j's full-text index needs.
    /// Backends use whichever side is meaningful for them.
    async fn hybrid_search(
        &self,
        query: Vec<f32>,
        sparse: Vec<(u32, f32)>,
        query_text: &str,
        k: usize,
        filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError>;

    async fn traverse(
        &self,
        start: &str,
        max_hops: u32,
        edge_types: Option<&[String]>,
        direction: Direction,
    ) -> Result<TraversalPage, StoreError>;

    async fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<NodeRow>, StoreError>;
    async fn fetch_node(&self, key: &str) -> Result<Option<NodeRow>, StoreError>;
    async fn count_nodes(&self) -> Result<usize, StoreError>;
    async fn count_edges(&self) -> Result<usize, StoreError>;

    /// Backends without native PPR (Neo4j sans GDS) return
    /// [`StoreError::Unsupported`]. Callers should check
    /// [`supports_native_ppr`] first.
    ///
    /// [`supports_native_ppr`]: KnowledgeStore::supports_native_ppr
    async fn personalized_pagerank(
        &self,
        seeds: &[String],
        direction: Direction,
        edge_types: Option<&[String]>,
        restart_prob: f32,
        max_iter: usize,
        max_results: Option<usize>,
    ) -> Result<Vec<(String, f32)>, StoreError>;
}

/// Parsed destination specification, built from CLI flags or env vars.
#[derive(Debug, Clone)]
pub enum StoreSpec {
    Overgraph {
        path: PathBuf,
        embedding_dim: u32,
    },
    Neo4j {
        uri: String,
        user: String,
        password: String,
        database: Option<String>,
        embedding_dim: u32,
    },
}

impl StoreSpec {
    pub fn name(&self) -> &'static str {
        match self {
            StoreSpec::Overgraph { .. } => "overgraph",
            StoreSpec::Neo4j { .. } => "neo4j",
        }
    }

    pub fn embedding_dim(&self) -> u32 {
        match self {
            StoreSpec::Overgraph { embedding_dim, .. } => *embedding_dim,
            StoreSpec::Neo4j { embedding_dim, .. } => *embedding_dim,
        }
    }

    pub fn set_embedding_dim(&mut self, dim: u32) {
        match self {
            StoreSpec::Overgraph { embedding_dim, .. } => *embedding_dim = dim,
            StoreSpec::Neo4j { embedding_dim, .. } => *embedding_dim = dim,
        }
    }
}

/// Open a single store from a [`StoreSpec`]. The OverGraph variant uses
/// `open_or_create` semantics; the Neo4j variant connects to the existing
/// server (it does not provision Neo4j itself) and ensures the schema
/// (constraints + vector + full-text indexes) is in place.
pub async fn open_store(spec: &StoreSpec) -> Result<Box<dyn KnowledgeStore>, StoreError> {
    match spec {
        StoreSpec::Overgraph {
            path,
            embedding_dim,
        } => {
            let path_str = path
                .to_str()
                .ok_or_else(|| StoreError::Backend(format!("invalid path: {:?}", path)))?;
            let db = crate::storage::db::Db::open_or_create(path_str, *embedding_dim).await?;
            Ok(Box::new(db))
        }
        StoreSpec::Neo4j {
            uri,
            user,
            password,
            database,
            embedding_dim,
        } => {
            let store = crate::storage::backends::neo4j::Neo4jStore::open(
                uri,
                user,
                password,
                database.as_deref(),
                *embedding_dim,
            )
            .await?;
            Ok(Box::new(store))
        }
    }
}

/// Multi-destination fan-out wrapper used by ingest. Reads do **not** go
/// through `StoreSet`; pick exactly one store for retrieval.
pub struct StoreSet {
    pub stores: Vec<Box<dyn KnowledgeStore>>,
}

impl StoreSet {
    pub fn new(stores: Vec<Box<dyn KnowledgeStore>>) -> Self {
        Self { stores }
    }

    /// Probe every store's embedding dim; fail if they disagree. Called
    /// at the top of fan-out ingest to surface mismatches early.
    pub fn validate_dims(&self) -> Result<u32, StoreError> {
        let mut iter = self.stores.iter();
        let first = iter
            .next()
            .ok_or_else(|| StoreError::Backend("empty StoreSet".into()))?
            .embedding_dim();
        for s in iter {
            if s.embedding_dim() != first {
                return Err(StoreError::DimMismatch {
                    existing: first,
                    requested: s.embedding_dim(),
                });
            }
        }
        Ok(first)
    }

    /// Fan-out node upsert; fails fast if any backend errors.
    pub async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), StoreError> {
        let futs = self.stores.iter().map(|s| s.upsert_nodes(rows));
        futures::future::try_join_all(futs).await?;
        Ok(())
    }

    /// Fan-out edge upsert; fails fast if any backend errors.
    pub async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), StoreError> {
        let futs = self.stores.iter().map(|s| s.upsert_edges(rows));
        futures::future::try_join_all(futs).await?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.stores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.stores.iter().map(|s| s.backend_name()).collect()
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn parses_equality_predicate() {
        let f = NodeFilter::from_legacy_where("node_type = 'Function'").unwrap();
        assert_eq!(f.node_types.as_deref().unwrap(), &["Function".to_string()]);
    }

    #[test]
    fn parses_in_predicate() {
        let f = NodeFilter::from_legacy_where("node_type IN ('Function','Class')").unwrap();
        let got = f.node_types.unwrap();
        assert_eq!(got, vec!["Function".to_string(), "Class".to_string()]);
    }

    #[test]
    fn unknown_predicate_returns_none() {
        assert!(NodeFilter::from_legacy_where("file LIKE '%foo%'").is_none());
    }
}
