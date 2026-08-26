//! `KnowledgeStore`: the pluggable storage abstraction.
//!
//! Every backend (today: OverGraph; coming: Neo4j) implements one trait so
//! the upper layers (`ingest`, `analyze`, `ppr`, `serve`, `mcp`) can
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

/// A node as the ingest pipeline is about to write it: its project
/// string id plus the type it will be stored under.
///
/// Exists so [`KnowledgeStore::nodes_for_upsert`] can hand backends the
/// type alongside the id. Backends that key nodes by `(type, id)` need
/// it to avoid probing; backends with a flat id space ignore it.
#[derive(Debug, Clone)]
pub struct NodeKey {
    pub id: String,
    pub node_type: String,
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

// ── statistical queries ────────────────────────────────────────────────

/// One cell of a query result, in the shapes every backend can produce.
///
/// Deliberately not `overgraph::GqlValue`: this crosses the
/// [`KnowledgeStore`] boundary, so it has to stay backend-neutral, and the
/// engine's node/edge/path values are flattened to their string keys on
/// the way through — a statistics answer wants an identifier it can hand
/// to `get_code`, not a hydrated node.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<QueryValue>),
}

impl QueryValue {
    /// The number this cell represents, for the render layer's percentile
    /// and ratio helpers. `None` for anything non-numeric — including
    /// [`QueryValue::Null`], which is "no value", not zero.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            QueryValue::Int(i) => Some(*i as f64),
            QueryValue::Float(f) => Some(*f),
            QueryValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

/// Execution caps for one statistical query.
///
/// These are pinned by the caller rather than inherited from the engine's
/// defaults, deliberately. Caps in OverGraph **truncate rather than
/// error**, so a query that trips one returns a confident under-count —
/// and a cap the caller never chose is one it cannot warn about. See
/// [`QueryPage::truncated`].
#[derive(Debug, Clone)]
pub struct QueryLimits {
    pub max_rows: usize,
    pub max_groups: usize,
    pub max_frontier: usize,
    pub max_collect_items: usize,
    /// Upper bound on variable-length path expansion. Every `*1..N` in a
    /// query must stay under this or the walk is silently clipped.
    pub max_path_hops: u8,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_rows: 10_000,
            max_groups: 65_536,
            max_frontier: 65_536,
            max_collect_items: 65_536,
            max_path_hops: 16,
        }
    }
}

/// Named query parameters, bound by the engine rather than substituted
/// into the query text. Preset arguments arrive from users and from
/// repo-supplied files, and interpolating them would make a preset a
/// string-concatenation hazard.
pub type QueryParams = std::collections::BTreeMap<String, QueryValue>;

/// The result of one statistical query.
#[derive(Debug, Clone, Default)]
pub struct QueryPage {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<QueryValue>>,
    /// Rows the engine matched before projection — the denominator behind
    /// a `LIMIT`ed answer.
    pub rows_matched: usize,
    /// Engine diagnostics, passed through verbatim. Includes plan notes
    /// (missing index, fallback scan) as well as cap trips.
    pub warnings: Vec<String>,
    /// The row cap was reached, so this answer is a **lower bound**.
    pub truncated: bool,
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
    /// The store on disk was written by an older build whose layout this
    /// one cannot read. Recoverable only by reindexing.
    StoreFormatMismatch {
        existing: u32,
        supported: u32,
    },
    /// The store's on-disk data cannot be parsed at all (corrupt manifest,
    /// WAL, or record — e.g. from a concurrent writer). The ingest command
    /// is the one writer entitled to replace it: it wipes and rebuilds.
    Corrupt(String),
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
            StoreError::StoreFormatMismatch {
                existing,
                supported,
            } => write!(
                f,
                "this index was written by an older ug (store format v{}, this build needs v{}). \
                 Run `ug gen` to rebuild it.",
                existing, supported
            ),
            StoreError::Corrupt(msg) => write!(f, "corrupt store on disk: {}", msg),
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
            DbError::StoreFormatMismatch {
                existing,
                supported,
            } => StoreError::StoreFormatMismatch {
                existing,
                supported,
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

    /// The embedding model this store was last ingested with, when the
    /// backend records one.
    ///
    /// Ingest compares this against the model in hand and refuses to reuse
    /// stored vectors when they disagree — see
    /// [`plan_incremental_ingest`]. `None` means "unrecorded", which is
    /// treated as "reuse allowed": a backend that cannot track this (or a
    /// store predating the field) must keep behaving as it did.
    ///
    /// [`plan_incremental_ingest`]: crate::storage::ingest::plan_incremental_ingest
    fn ingest_model(&self) -> Option<String> {
        None
    }

    /// Stamp the model that just finished ingesting. Backends without a
    /// place to keep it no-op.
    fn record_ingest_model(&self, _model: &str) {}

    /// blake3 of the edge set this store was last ingested with, if it keeps
    /// one. `None` means "unknown" and makes the caller write every edge —
    /// the safe direction, since a wrong "unchanged" would leave the store
    /// disagreeing with `graph.json`. See P11.6 in
    /// docs/dev/PERF-TUNING-JOURNEY.md.
    fn edges_digest(&self) -> Option<String> {
        None
    }

    /// Stamp the edge-set digest. Backends without a place to keep it no-op,
    /// which simply means they never skip the edge write.
    fn record_edges_digest(&self, _digest: &str) {}

    /// Corpus statistics backing BM25 weighting of the keyword channel.
    /// `None` means the caller should fall back to unweighted term
    /// frequency — a store ingested before the sidecar existed, or a
    /// backend with its own full-text engine (Neo4j already scores BM25
    /// through Lucene and ignores the sparse vector entirely).
    fn sparse_stats(&self) -> Option<std::sync::Arc<crate::storage::sparse_stats::SparseStats>> {
        None
    }

    /// Install freshly computed corpus statistics, persisting them where
    /// the backend keeps its sidecars. Called by ingest before the upsert,
    /// so the vectors written in that same run are weighted with them.
    fn set_sparse_stats(&self, _stats: std::sync::Arc<crate::storage::sparse_stats::SparseStats>) {}

    /// Declare whatever indexes make statistical queries over the stored
    /// facts cheap. Called once at the end of ingest, when the data exists.
    ///
    /// Purely a performance hint — the default no-op is correct for any
    /// backend that indexes automatically or cannot declare indexes at
    /// all, and no caller may depend on it having done anything.
    fn ensure_query_indexes(&self) {}

    /// Run one read-only GQL statement over the stored graph.
    ///
    /// This is the whole-repo statistics surface: counting, grouping and
    /// bounded reachability over the facts ingest wrote (see
    /// [`crate::storage::facts`]). Implementations **must** reject
    /// mutations — presets can arrive from a cloned repository's
    /// `.ug/presets.toml`, and read-only execution is what makes that
    /// safe by construction rather than by review.
    ///
    /// The default returns [`StoreError::Unsupported`]: a backend without
    /// a query language should say so, not answer approximately.
    async fn execute_query(
        &self,
        gql: &str,
        params: &QueryParams,
        limits: &QueryLimits,
    ) -> Result<QueryPage, StoreError> {
        let _ = (gql, params, limits);
        Err(StoreError::Unsupported(
            "this backend cannot run GQL statistical queries",
        ))
    }

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

    /// Read back the rows for nodes the caller is about to upsert, so
    /// ingest can tell which ones actually changed (see
    /// `ingest::plan_incremental_ingest`). Missing ids are simply absent
    /// from the result — a first-ever ingest returns an empty vec.
    ///
    /// Same contract as [`nodes_by_ids`], except the caller also supplies
    /// each node's type. OverGraph keys nodes by `(type_id, key)`, so with
    /// the type in hand it can do one keyed read per node instead of
    /// `lookup_id`'s probe across every known type id — which matters here
    /// because ingest runs in a fresh process with a cold id cache, where
    /// that probe would otherwise cost ~9 engine reads per node.
    ///
    /// The default implementation drops the type and delegates, which is
    /// correct for any backend whose ids are unique on their own.
    ///
    /// [`nodes_by_ids`]: KnowledgeStore::nodes_by_ids
    async fn nodes_for_upsert(&self, keys: &[NodeKey]) -> Result<Vec<NodeRow>, StoreError> {
        let ids: Vec<String> = keys.iter().map(|k| k.id.clone()).collect();
        self.nodes_by_ids(&ids).await
    }

    /// Delete stored nodes whose id is **not** in `keep`, returning how many
    /// were removed.
    ///
    /// Ingest is an upsert, so without this a node that disappears from the
    /// source — a deleted file, a renamed symbol — lingers in the store and
    /// keeps turning up in search results forever. Callers must pass the
    /// complete id set of a full graph; pruning against a partial graph
    /// would delete everything else.
    ///
    /// The default implementation prunes nothing and reports 0, for backends
    /// that cannot enumerate their key space. That is a silent divergence
    /// from a backend that *does* prune, so implement it where you can.
    async fn prune_nodes_absent_from(
        &self,
        _keep: &std::collections::HashSet<&str>,
    ) -> Result<usize, StoreError> {
        Ok(0)
    }

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

/// Clear any destination whose on-disk format this build cannot read, so
/// the ingest that follows rebuilds it instead of failing against it.
///
/// Only ingest calls this — see
/// [`crate::storage::db::reset_if_stale_format`] for why the deletion
/// lives on the write path and not in `open_store`. Neo4j is unaffected:
/// its schema is server-side and versionless here.
///
/// Returns the paths it cleared, for the caller to report.
pub fn reset_stale_format_stores(specs: &[StoreSpec]) -> Result<Vec<PathBuf>, StoreError> {
    let mut cleared = Vec::new();
    for spec in specs {
        if let StoreSpec::Overgraph { path, .. } = spec {
            if crate::storage::db::reset_if_stale_format(path)? {
                cleared.push(path.clone());
            }
        }
    }
    Ok(cleared)
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
            let db = crate::storage::db::Db::open_or_create(path_str, *embedding_dim).await
                .map_err(|e| match &e {
                    crate::storage::db::DbError::Engine(
                        crate::storage::db::EngineError::ManifestError(_)
                        | crate::storage::db::EngineError::CorruptWal(_)
                        | crate::storage::db::EngineError::CorruptRecord(_)
                        | crate::storage::db::EngineError::SerializationError(_),
                    ) => StoreError::Corrupt(e.to_string()),
                    _ => e.into(),
                })?;
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

    // ---- NodeFilter edge cases -----------------------------------------

    #[test]
    fn the_predicate_is_matched_case_insensitively() {
        // The legacy CLI accepted any casing, and a filter that silently
        // stopped applying would quietly widen every search rather than
        // erroring.
        for s in [
            "NODE_TYPE = 'Function'",
            "Node_Type='Function'",
            "  node_type   =   'Function'  ",
        ] {
            let f = NodeFilter::from_legacy_where(s)
                .unwrap_or_else(|| panic!("should parse: {s}"));
            assert_eq!(f.node_types.as_deref().unwrap(), &["Function".to_string()]);
        }
        for s in ["node_type in ('Function')", "node_type In ('Function')"] {
            assert!(NodeFilter::from_legacy_where(s).is_some(), "should parse: {s}");
        }
    }

    #[test]
    fn quoted_values_keep_their_own_case() {
        // Node types are compared exactly downstream, so lowercasing the
        // value while detecting the predicate would match nothing.
        let f = NodeFilter::from_legacy_where("NODE_TYPE = 'Function'").unwrap();
        assert_eq!(f.node_types.as_deref().unwrap(), &["Function".to_string()]);
    }

    #[test]
    fn both_quote_styles_are_accepted() {
        for s in ["node_type = 'Class'", "node_type = \"Class\""] {
            let f = NodeFilter::from_legacy_where(s).unwrap();
            assert_eq!(f.node_types.as_deref().unwrap(), &["Class".to_string()]);
        }
    }

    #[test]
    fn an_unquoted_value_is_not_accepted() {
        // Rejecting is the safe direction: a filter we can't read becomes
        // no filter, which returns more than asked rather than less.
        assert!(NodeFilter::from_legacy_where("node_type = Function").is_none());
        assert!(NodeFilter::from_legacy_where("node_type = 'Function").is_none());
        assert!(NodeFilter::from_legacy_where("node_type = Function'").is_none());
    }

    #[test]
    fn an_in_list_tolerates_spacing_and_drops_unquoted_members() {
        let f = NodeFilter::from_legacy_where("node_type IN ( 'Function' , 'Class' )").unwrap();
        assert_eq!(
            f.node_types.unwrap(),
            vec!["Function".to_string(), "Class".to_string()]
        );
        // A partially-quoted list keeps what it can read.
        let f = NodeFilter::from_legacy_where("node_type IN ('Function', Class)").unwrap();
        assert_eq!(f.node_types.unwrap(), vec!["Function".to_string()]);
    }

    #[test]
    fn an_empty_or_unparseable_in_list_returns_none() {
        assert!(NodeFilter::from_legacy_where("node_type IN ()").is_none());
        assert!(NodeFilter::from_legacy_where("node_type IN (Function)").is_none());
        // Missing parentheses is not a list.
        assert!(NodeFilter::from_legacy_where("node_type IN 'Function'").is_none());
    }

    #[test]
    fn a_different_column_is_never_mistaken_for_node_type() {
        // Substring matching here would make `my_node_type` or a `file`
        // predicate silently filter on node type instead.
        assert!(NodeFilter::from_legacy_where("my_node_type = 'Function'").is_none());
        assert!(NodeFilter::from_legacy_where("file = 'src/a.ts'").is_none());
        assert!(NodeFilter::from_legacy_where("").is_none());
        assert!(NodeFilter::from_legacy_where("   ").is_none());
    }

    #[test]
    fn type_only_builds_from_any_string_like_iterator() {
        let f = NodeFilter::type_only(["Function", "Class"]);
        assert_eq!(
            f.node_types.unwrap(),
            vec!["Function".to_string(), "Class".to_string()]
        );
        let f = NodeFilter::type_only(vec![String::from("Folder")]);
        assert_eq!(f.node_types.unwrap(), vec!["Folder".to_string()]);
        // An empty list is still a filter — it means "match nothing" — and
        // must stay distinguishable from `None`, which means "no filter".
        let f = NodeFilter::type_only(Vec::<String>::new());
        assert_eq!(f.node_types.as_deref(), Some(&[][..]));
    }

    #[test]
    fn the_default_filter_is_no_filter() {
        assert!(NodeFilter::default().node_types.is_none());
    }

    // ---- Direction ------------------------------------------------------

    #[test]
    fn direction_accepts_every_spelling_of_each_case() {
        for s in ["in", "IN", "In", "inbound", "INBOUND", "incoming"] {
            assert_eq!(Direction::from_str_lossy(s), Direction::Inbound, "{s}");
        }
        for s in ["both", "BOTH", "all", "any", "Any"] {
            assert_eq!(Direction::from_str_lossy(s), Direction::Both, "{s}");
        }
        for s in ["out", "outbound", "outgoing", "OUT"] {
            assert_eq!(Direction::from_str_lossy(s), Direction::Outbound, "{s}");
        }
    }

    #[test]
    fn an_unrecognised_direction_falls_back_to_outbound() {
        // "Lossy" is the contract: this parses user input from a CLI flag
        // and an API field, and following a call graph forwards is the
        // least surprising thing to do with a typo.
        for s in ["", "sideways", "backwards", "  in  ", "inn"] {
            assert_eq!(Direction::from_str_lossy(s), Direction::Outbound, "{s:?}");
        }
    }

    // ---- StoreSpec ------------------------------------------------------

    fn overgraph_spec(dim: u32) -> StoreSpec {
        StoreSpec::Overgraph {
            path: PathBuf::from("/tmp/kb"),
            embedding_dim: dim,
        }
    }

    fn neo4j_spec(dim: u32) -> StoreSpec {
        StoreSpec::Neo4j {
            uri: "bolt://localhost:7687".into(),
            user: "neo4j".into(),
            password: "secret".into(),
            database: None,
            embedding_dim: dim,
        }
    }

    #[test]
    fn store_spec_reports_its_backend_name() {
        // These strings reach the user in `--dest` output and logs.
        assert_eq!(overgraph_spec(384).name(), "overgraph");
        assert_eq!(neo4j_spec(384).name(), "neo4j");
    }

    #[test]
    fn embedding_dim_reads_through_either_variant() {
        assert_eq!(overgraph_spec(384).embedding_dim(), 384);
        assert_eq!(neo4j_spec(768).embedding_dim(), 768);
    }

    #[test]
    fn set_embedding_dim_writes_through_either_variant() {
        // The dimension is only known once the embedder loads, so the spec
        // is built with a placeholder and corrected in place. A setter that
        // missed a variant would open a store sized for the wrong model.
        let mut og = overgraph_spec(384);
        og.set_embedding_dim(1024);
        assert_eq!(og.embedding_dim(), 1024);

        let mut neo = neo4j_spec(384);
        neo.set_embedding_dim(1024);
        assert_eq!(neo.embedding_dim(), 1024);
    }

    #[test]
    fn setting_the_dimension_leaves_the_rest_of_the_spec_alone() {
        let mut neo = neo4j_spec(384);
        neo.set_embedding_dim(512);
        match neo {
            StoreSpec::Neo4j {
                uri, user, database, ..
            } => {
                assert_eq!(uri, "bolt://localhost:7687");
                assert_eq!(user, "neo4j");
                assert_eq!(database, None);
            }
            _ => panic!("variant changed"),
        }
    }
}
