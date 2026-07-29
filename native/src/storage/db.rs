//! OverGraph persistence for graph nodes and edges.
//!
//! [`Db`] wraps an [`overgraph::DatabaseEngine`] and adds a
//! `String → u64` id cache so callers can keep using the project's
//! string ids (`"file:src/foo.ts"`, etc.) while OverGraph uses numeric
//! ids internally.
//!
//! The public function names and signatures intentionally mirror the
//! previous OverGraph layer (`upsert_nodes`, `upsert_edges`, `vector_search`,
//! `fts_search`, `edges_from`, `edges_to`, `nodes_by_ids`, `all_edges`)
//! so `query.rs` and `ingest.rs` don't need to change in this phase.
//! Phase D will retarget those callers to the more idiomatic OverGraph
//! APIs (native hybrid search, `db.traverse`, etc.).

use crate::storage::embed::DEFAULT_EMBEDDING_DIM;
use crate::storage::store::{
    Direction, KnowledgeStore, NodeFilter, NodeKey, QueryLimits, QueryPage, QueryParams,
    QueryValue, StoreError, TraversalNode, TraversalPage,
};
use crate::storage::facts::{FactValue, Facts};
use crate::storage::types_registry::{edge_label, node_label, ALL_NODE_LABELS};
use async_trait::async_trait;
use overgraph::{
    DatabaseEngine, DbOptions, DenseMetric, DenseVectorConfig, Direction as OgDirection, EdgeInput,
    EdgeView, EngineError, FusionMode, GqlExecutionMode, GqlExecutionOptions, GqlParamValue,
    GqlParams, GqlValue, HnswConfig, NeighborOptions, NodeInput, NodeKeyQuery, NodeView,
    PprAlgorithm, PprOptions as OgPprOptions, PprResult as OgPprResult, PropValue,
    VectorSearchMode, VectorSearchRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use crate::storage::sparse_stats::SparseStats;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Filename of the sidecar manifest written next to the OverGraph data
/// directory. Records the embedding dim the DB was created with so we
/// can reject mismatched re-opens (which would otherwise silently mix
/// vectors of different sizes).
const META_FILE: &str = "ug-meta.json";

/// On-disk layout version for the OverGraph store.
///
/// Bumped to 2 for the OverGraph 0.17 upgrade, which changed node and
/// edge typing from numeric `type_id`s to string labels. A v1 store's
/// segments encode types the 0.17 engine reads as different labels
/// entirely, so opening one would not fail — it would silently return
/// nothing for every typed lookup. Rejecting it outright and asking for
/// a reindex is the only honest option.
///
/// Bump this whenever the stored encoding changes in a way that makes an
/// older directory unreadable or, worse, quietly wrong.
const STORE_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DbMeta {
    /// Absent (deserializing to 0) on stores written before this field
    /// existed, which are by definition v1 — pre-0.17 numeric type ids.
    store_format: u32,
    embedding_dim: u32,
    /// The embedding model the store was last ingested with.
    ///
    /// Recorded because the dim check alone does not catch a model swap:
    /// `bge-small-en-v1.5` and `all-MiniLM-L6-v2` are both 384-dimensional,
    /// so switching between them leaves `node_text` identical and the
    /// incremental planner happily carries the *old* model's vectors
    /// forward — one store holding vectors from two incompatible spaces,
    /// with nothing to notice. Absent on stores written before this field
    /// existed, which is treated as "unknown", not "mismatched".
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

fn meta_path(db_path: &Path) -> PathBuf {
    db_path.join(META_FILE)
}

fn read_meta(db_path: &Path) -> Result<Option<DbMeta>, DbError> {
    let p = meta_path(db_path);
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DbError::Io(e)),
    }
}

fn write_meta(db_path: &Path, meta: &DbMeta) -> Result<(), DbError> {
    std::fs::create_dir_all(db_path)?;
    let s = serde_json::to_string_pretty(meta)?;
    std::fs::write(meta_path(db_path), s)?;
    Ok(())
}

/// Reject a store whose on-disk layout this build cannot read.
///
/// Keyed on the manifest alone, deliberately. An earlier version also
/// treated "data on disk but no manifest" as an old store, which is
/// wrong: [`Db::open`] never writes a manifest, so a store created
/// through it looked ancient the second time it was opened. Absence of a
/// manifest is genuinely ambiguous and is not evidence of anything.
///
/// That leaves one narrow hole — a store old enough to predate the
/// manifest entirely would be opened rather than rejected. Every store
/// written by any ug that recorded `embedding_dim` (which is every ug
/// that `ug gen` has shipped in) does have one, so the population this
/// misses is effectively empty, and it was already unreadable before
/// this check existed.
fn check_store_format(meta: Option<&DbMeta>) -> Result<(), DbError> {
    let Some(meta) = meta else {
        return Ok(());
    };
    if meta.store_format == STORE_FORMAT_VERSION {
        return Ok(());
    }
    // A manifest written before the field existed deserializes to 0; it
    // is a v1 store, and reporting "v0" would just confuse.
    Err(DbError::StoreFormatMismatch {
        existing: meta.store_format.max(1),
        supported: STORE_FORMAT_VERSION,
    })
}

/// Clear a store whose on-disk layout this build cannot read, so the
/// caller can rebuild it from scratch.
///
/// Read paths reject a stale store and tell the user to reindex — but the
/// reindex has to be able to *succeed*, and it cannot open the old store
/// to overwrite it. Something has to delete it, and ingest is the only
/// caller entitled to: it is about to replace every node anyway.
///
/// Deliberately narrow. It removes the directory only when a manifest is
/// present *and* records a different format. A missing manifest is
/// ambiguous (see [`check_store_format`]) and is never grounds for
/// deleting anything.
///
/// Returns whether it removed a store.
pub fn reset_if_stale_format(path: &Path) -> Result<bool, DbError> {
    let Some(meta) = read_meta(path)? else {
        return Ok(false);
    };
    if meta.store_format == STORE_FORMAT_VERSION {
        return Ok(false);
    }
    std::fs::remove_dir_all(path)?;
    Ok(true)
}

/// The embedding dimension a store on disk was built with, read from its
/// sidecar without opening the engine.
///
/// Callers that only read properties — statistical queries especially —
/// have no embedder and no reason to start one, but
/// [`Db::open_or_create`] still rejects a dim that disagrees with the
/// manifest. This lets them ask the store what it already is instead of
/// guessing, or spinning up an embedding backend to find out.
///
/// `None` means no manifest, in which case the caller should fall back to
/// [`DEFAULT_EMBEDDING_DIM`] exactly as [`Db::open`] does.
pub fn stored_embedding_dim(path: &Path) -> Option<u32> {
    read_meta(path)
        .ok()
        .flatten()
        .map(|m| m.embedding_dim)
        .filter(|d| *d > 0)
}

#[derive(Debug)]
pub enum DbError {
    Engine(EngineError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Unimplemented(&'static str),
    BadVector { id: String, got: usize, want: usize },
    UnknownEndpoint(String),
    DimMismatch { existing: u32, requested: u32 },
    StoreFormatMismatch { existing: u32, supported: u32 },
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Engine(e) => write!(f, "overgraph error: {}", e),
            DbError::Io(e) => write!(f, "io error: {}", e),
            DbError::Json(e) => write!(f, "json error: {}", e),
            DbError::Unimplemented(what) => write!(f, "not yet implemented: {}", what),
            DbError::BadVector { id, got, want } => {
                write!(f, "vector for {} has dim {}, expected {}", id, got, want)
            }
            DbError::UnknownEndpoint(s) => write!(f, "unknown edge endpoint: {}", s),
            DbError::DimMismatch { existing, requested } => write!(
                f,
                "embedding dim mismatch: db was created with dim {}, but {} was requested. \
                 Either pass the matching --embedding-dim, or delete the db directory to recreate it.",
                existing, requested
            ),
            DbError::StoreFormatMismatch { existing, supported } => write!(
                f,
                "this index was written by an older ug (store format v{}, this build needs v{}). \
                 Node and edge typing changed, so the old data cannot be read correctly. \
                 Run `ug regen` (or `ug gen`) to rebuild it.",
                existing, supported
            ),
        }
    }
}

impl std::error::Error for DbError {}

impl From<EngineError> for DbError {
    fn from(e: EngineError) -> Self {
        DbError::Engine(e)
    }
}
impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::Io(e)
    }
}
impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        DbError::Json(e)
    }
}

/// Wire-format DTO mirroring the previous `NodeRow` shape exactly so
/// `query.rs`, `ingest.rs`, and the JSON outputs downstream keep working
/// unchanged.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub description: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub last_update_at: i64,
    pub node_text: String,
    pub vector: Vec<f32>,
    /// The node's source text, captured at index time.
    ///
    /// Stored so retrieval is self-contained: without it every code read
    /// goes back to the working tree, which means the row's description
    /// and the code an agent sees can disagree, and a shifted line range
    /// silently returns the wrong lines with no error. Empty for nodes
    /// with no source (folders) and for rows written before this column
    /// existed — callers fall back to the filesystem in that case.
    ///
    /// Deliberately *not* part of `node_text`: bodies are not embedded.
    pub code: String,
    /// blake3 of the whole file `code` was taken from, so staleness is
    /// checkable against disk with one hash instead of guessed at.
    pub file_hash: String,
    /// Derived per-node facts (`loc`, `in_degree`, `is_test`, …), stored
    /// as individual node properties so they can be filtered and
    /// aggregated. See [`crate::storage::facts`].
    ///
    /// Empty for rows read back from a store written before these
    /// existed, which is what makes a query over them report "not
    /// indexed" rather than a confident zero.
    pub facts: Facts,
}

#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: String,
    pub source: String,
    pub target: String,
    pub edge_type: String,
    pub properties: String,
}

pub struct Db {
    pub engine: DatabaseEngine,
    /// Where the sidecar manifests live. Kept so the ingest profile and
    /// the sparse-corpus statistics can be read and re-stamped without the
    /// caller passing the path back in.
    path: PathBuf,
    /// Corpus statistics backing BM25 weighting, loaded from the sidecar at
    /// open and refreshed by ingest once it has recomputed them. `None`
    /// means keyword scoring falls back to plain term frequency.
    sparse_stats: RwLock<Option<Arc<SparseStats>>>,
    /// Embedding dimension this DB was opened with. Validated against
    /// the on-disk sidecar (`ug-meta.json`) to prevent mixing vectors
    /// of different sizes across runs.
    embedding_dim: u32,
    /// Project's string id (e.g. `"file:src/foo.ts"`) → OverGraph numeric id.
    /// Populated on every upsert; used by edge endpoint resolution and the
    /// traverse output (which must hand back string ids over the NAPI
    /// boundary).
    key_to_id: RwLock<HashMap<String, u64>>,
    /// Reverse cache used when hydrating traversal results back into the
    /// project's string-id wire format. Mutated together with `key_to_id`.
    id_to_key: RwLock<HashMap<u64, String>>,
}

impl Db {
    /// Open an existing OverGraph database at `path`, picking up the
    /// embedding dimension from its sidecar manifest. If no sidecar
    /// exists (legacy databases created before the manifest landed),
    /// falls back to [`DEFAULT_EMBEDDING_DIM`] (1024) for backwards
    /// compatibility.
    ///
    /// Use [`Db::open_or_create`] when ingesting — it writes the sidecar
    /// and rejects mismatched re-opens, which is what you actually want
    /// when the dim could differ between runs.
    ///
    /// OverGraph's open is synchronous; the `async` signature is preserved
    /// for call-site compatibility.
    pub async fn open(path: &str) -> Result<Self, DbError> {
        let path_buf = Path::new(path).to_path_buf();
        let meta = read_meta(&path_buf)?;
        check_store_format(meta.as_ref())?;
        let dim = meta
            .map(|m| m.embedding_dim)
            .unwrap_or(DEFAULT_EMBEDDING_DIM as u32);
        Self::open_inner(&path_buf, dim).await
    }

    /// Open the OverGraph database at `path`, creating it (and its
    /// sidecar manifest) if it does not yet exist. If the sidecar
    /// already records a different `embedding_dim`, returns
    /// [`DbError::DimMismatch`] rather than silently mixing vectors.
    pub async fn open_or_create(path: &str, embedding_dim: u32) -> Result<Self, DbError> {
        let path_buf = Path::new(path).to_path_buf();
        let meta = read_meta(&path_buf)?;
        check_store_format(meta.as_ref())?;
        match meta {
            Some(meta) if meta.embedding_dim != embedding_dim => {
                return Err(DbError::DimMismatch {
                    existing: meta.embedding_dim,
                    requested: embedding_dim,
                });
            }
            Some(_) => {}
            None => write_meta(
                &path_buf,
                &DbMeta {
                    store_format: STORE_FORMAT_VERSION,
                    embedding_dim,
                    model: None,
                },
            )?,
        }
        Self::open_inner(&path_buf, embedding_dim).await
    }

    /// The embedding model this store was last ingested with, if recorded.
    /// `None` on a fresh store or one written before the field existed.
    pub fn recorded_model(&self) -> Option<String> {
        read_meta(&self.path).ok().flatten().and_then(|m| m.model)
    }

    /// Stamp the model that just finished ingesting. Preserves the recorded
    /// dim — this rewrites the sidecar, and dropping the dim would make the
    /// next open fall back to the default and mismatch.
    pub fn record_model(&self, model: &str) -> Result<(), DbError> {
        let mut meta = read_meta(&self.path)?.unwrap_or_default();
        meta.store_format = STORE_FORMAT_VERSION;
        meta.embedding_dim = self.embedding_dim;
        meta.model = Some(model.to_string());
        write_meta(&self.path, &meta)
    }

    async fn open_inner(path: &Path, embedding_dim: u32) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let opts = DbOptions {
            dense_vector: Some(DenseVectorConfig {
                dimension: embedding_dim,
                metric: DenseMetric::Cosine,
                hnsw: HnswConfig::default(),
            }),
            ..Default::default()
        };
        let engine = DatabaseEngine::open(path, &opts)?;
        let sparse_stats = SparseStats::load(path).map(Arc::new);
        Ok(Self {
            engine,
            path: path.to_path_buf(),
            sparse_stats: RwLock::new(sparse_stats),
            embedding_dim,
            key_to_id: RwLock::new(HashMap::new()),
            id_to_key: RwLock::new(HashMap::new()),
        })
    }

    /// Embedding dimension this DB was opened with.
    pub fn embedding_dim(&self) -> u32 {
        self.embedding_dim
    }

    /// Return the OverGraph numeric id for a project string id, looking
    /// it up via the cache first, then via OverGraph if absent. Returns
    /// `None` for endpoints that haven't been ingested yet.
    pub fn lookup_id(&self, key: &str) -> Result<Option<u64>, DbError> {
        if let Some(id) = self.key_to_id.read().unwrap().get(key).copied() {
            return Ok(Some(id));
        }
        // Slow path — try every known node label. OverGraph keys nodes by
        // (label, key) so we have to probe; in practice the cache is
        // hot so this rarely fires.
        for label in ALL_NODE_LABELS {
            if let Some(rec) = self.engine.get_node_by_key(label, key)? {
                self.remember(key.to_string(), rec.id);
                return Ok(Some(rec.id));
            }
        }
        Ok(None)
    }

    /// Hydrate a single node row by its project string id. Cheaper than
    /// `traverse(id, 0)` (which OverGraph currently rejects) and avoids
    /// the over-fetch of `traverse(id, 1)`. Used by `ug serve`'s
    /// `/api/db/node/<id>` endpoint.
    pub fn fetch_node(&self, key: &str) -> Result<Option<NodeRow>, DbError> {
        let Some(numeric) = self.lookup_id(key)? else {
            return Ok(None);
        };
        let Some(rec) = self.engine.get_node(numeric)? else {
            return Ok(None);
        };
        self.remember(rec.key.clone(), rec.id);
        Ok(Some(node_record_to_row(&rec)))
    }

    fn remember(&self, key: String, id: u64) {
        self.key_to_id.write().unwrap().insert(key.clone(), id);
        self.id_to_key.write().unwrap().insert(id, key);
    }

    /// Drop a key from both caches after its node is deleted, so a later
    /// `lookup_id` can't hand out an id that no longer resolves.
    fn forget(&self, key: &str, id: u64) {
        self.key_to_id.write().unwrap().remove(key);
        self.id_to_key.write().unwrap().remove(&id);
    }

    /// Translate an OverGraph numeric id back to its project string id.
    /// Falls back to a synthetic `"node:<id>"` placeholder when the
    /// reverse cache misses; that should only happen for traversal hits
    /// that didn't pass through ingest in this process.
    pub fn key_for(&self, id: u64) -> String {
        if let Some(s) = self.id_to_key.read().unwrap().get(&id).cloned() {
            return s;
        }
        if let Ok(Some(rec)) = self.engine.get_node(id) {
            self.remember(rec.key.clone(), id);
            return rec.key;
        }
        format!("node:{}", id)
    }

    /// Upsert the given rows into the OverGraph nodes table. Caches the
    /// resulting `(string-id, u64)` mapping for edge endpoint resolution.
    pub async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), DbError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut inputs: Vec<NodeInput> = Vec::with_capacity(rows.len());
        let want = self.embedding_dim as usize;
        // One read for the whole batch — the stats only shape which terms
        // survive the per-node dimension cap.
        let stats = self.sparse_stats.read().ok().and_then(|g| g.clone());
        for r in rows {
            // An empty vector means "not embedded", not "bad vector". The
            // node is written without a dense vector: it stays absent from
            // the HNSW index (so semantic search simply doesn't surface it)
            // while its properties — including every derived fact — are
            // queryable. That is what lets `ug gen` produce a usable index
            // when the embedding endpoint is down, instead of nothing.
            // The next ingest sees `vector.len() != dim`, re-embeds it, and
            // fills the gap.
            if !r.vector.is_empty() && r.vector.len() != want {
                return Err(DbError::BadVector {
                    id: r.id.clone(),
                    got: r.vector.len(),
                    want,
                });
            }
            inputs.push(NodeInput {
                labels: vec![node_label(&r.node_type).to_string()],
                key: r.id.clone(),
                props: node_props(r),
                weight: 1.0,
                dense_vector: (!r.vector.is_empty()).then(|| r.vector.clone()),
                // Was `None`, which left the keyword half of
                // `hybrid_search` matching against nothing — queries built
                // a sparse vector but no node had one to score against.
                sparse_vector: Some(crate::storage::text::build_node_sparse_vector(
                    &r.node_text,
                    &r.code,
                    stats.as_deref(),
                )),
            });
        }
        let ids = self.engine.batch_upsert_nodes(inputs)?;
        let mut k2i = self.key_to_id.write().unwrap();
        let mut i2k = self.id_to_key.write().unwrap();
        for (row, id) in rows.iter().zip(ids.iter()) {
            k2i.insert(row.id.clone(), *id);
            i2k.insert(*id, row.id.clone());
        }
        Ok(())
    }

    /// Upsert edges. Endpoints (source/target) are resolved via the
    /// internal cache. Edge weights are baked from the edge type (see
    /// `default_edge_type_weights` in `ppr.rs`) so the native PPR sees
    /// the right structural bias.
    pub async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), DbError> {
        if rows.is_empty() {
            return Ok(());
        }
        let weights = crate::storage::ppr::default_edge_type_weights();
        let mut inputs: Vec<EdgeInput> = Vec::with_capacity(rows.len());
        for r in rows {
            let from = self
                .lookup_id(&r.source)?
                .ok_or_else(|| DbError::UnknownEndpoint(r.source.clone()))?;
            let to = self
                .lookup_id(&r.target)?
                .ok_or_else(|| DbError::UnknownEndpoint(r.target.clone()))?;
            let weight = weights
                .get(&r.edge_type.to_ascii_lowercase())
                .copied()
                .unwrap_or(0.5);
            inputs.push(EdgeInput {
                from,
                to,
                label: edge_label(&r.edge_type).to_string(),
                props: BTreeMap::new(),
                weight,
                valid_from: None,
                valid_to: None,
            });
        }
        self.engine.batch_upsert_edges(inputs)?;
        Ok(())
    }

    /// No-op on OverGraph — vector indexes are built per-segment at flush
    /// time. Kept for call-site compatibility with the previous OverGraph
    /// API;
    pub async fn try_create_vector_index(&self) -> Result<(), DbError> {
        Ok(())
    }

    /// Declare property indexes on the facts statistics queries filter by.
    ///
    /// Without these, every "how many functions over 50 lines" is an
    /// unanchored scan. `node_type` and `is_test` are equality-shaped;
    /// `loc` and `in_degree` are compared with `>` / `<` and need range
    /// indexes. Called after ingest, when the data they cover exists.
    ///
    /// Best-effort by design: an index that fails to build costs query
    /// speed, never correctness, so a failure is logged and swallowed
    /// rather than failing an otherwise-good ingest.
    pub fn ensure_fact_indexes(&self) {
        use overgraph::{SecondaryIndexField, SecondaryIndexSpec};

        const EQUALITY: &[&str] = &["node_type", "is_test", "folder", "has_doc"];
        const RANGE: &[&str] = &["loc", "in_degree", "out_degree", "params", "max_nesting"];

        for label in ALL_NODE_LABELS {
            for key in EQUALITY {
                let spec = SecondaryIndexSpec::equality([SecondaryIndexField::property(*key)]);
                if let Err(e) = self.engine.ensure_node_property_index(label, spec) {
                    tracing::debug!(label, key, error = %e, "could not declare equality index");
                }
            }
            for key in RANGE {
                let spec = SecondaryIndexSpec::range([SecondaryIndexField::property(*key)]);
                if let Err(e) = self.engine.ensure_node_property_index(label, spec) {
                    tracing::debug!(label, key, error = %e, "could not declare range index");
                }
            }
        }
    }

    /// Execute one read-only GQL statement and lower the result into the
    /// backend-portable [`QueryPage`].
    ///
    /// Three of the options here are load-bearing and none of them is the
    /// engine default:
    ///
    /// - **`mode: ReadOnly`** rejects mutation statements at parse time,
    ///   before any write staging. Query text reaches here from repo
    ///   `.ug/presets.toml` files and from agents, so "it cannot write"
    ///   has to be a property of the call, not a promise about the input.
    /// - **`allow_full_scan: true`**, because it defaults to `false` and a
    ///   statistic is a full scan by nature — "how many functions exceed
    ///   50 lines" has no bounded anchor to plan from. Without this every
    ///   preset fails at planning and the feature looks broken.
    /// - **the caps are pinned**, not inherited, so [`QueryPage`] can
    ///   report which one truncated. See the truncation note below.
    pub fn execute_gql(
        &self,
        gql: &str,
        params: &QueryParams,
        limits: &QueryLimits,
    ) -> Result<QueryPage, DbError> {
        let options = GqlExecutionOptions {
            mode: GqlExecutionMode::ReadOnly,
            allow_full_scan: true,
            max_rows: limits.max_rows,
            max_groups: limits.max_groups,
            max_frontier: limits.max_frontier,
            max_collect_items: limits.max_collect_items,
            max_path_hops: limits.max_path_hops,
            ..Default::default()
        };
        let bound: GqlParams = params
            .iter()
            .map(|(k, v)| (k.clone(), gql_param(v)))
            .collect();

        let result = self.engine.execute_gql(gql, &bound, &options)?;
        let rows: Vec<Vec<QueryValue>> = result
            .rows
            .iter()
            .map(|r| r.values.iter().map(query_value).collect())
            .collect();

        // The engine truncates at `max_rows` rather than erroring, and
        // says nothing about it. A result that exactly fills the cap is
        // indistinguishable from one that would have overflowed it, so
        // treat both as truncated: over-warning costs a line of output,
        // under-warning costs the caller a wrong number they trust.
        let truncated = rows.len() >= limits.max_rows;
        Ok(QueryPage {
            columns: result.columns,
            rows,
            rows_matched: result.stats.rows_matched,
            warnings: result.stats.warnings,
            truncated,
        })
    }

    pub async fn try_create_fts_index(&self) -> Result<(), DbError> {
        Ok(())
    }

    pub async fn count_nodes(&self) -> Result<usize, DbError> {
        // OverGraph's `stats` doesn't expose a live node count, but
        // `count_nodes_by_labels` counts a single label off the label
        // index without hydrating records, so summing our own inventory
        // is both precise and cheap.
        let mut total = 0usize;
        for label in ALL_NODE_LABELS {
            total += self.engine.count_nodes_by_labels(*label)? as usize;
        }
        Ok(total)
    }

    pub async fn count_edges(&self) -> Result<usize, DbError> {
        // Approximation via per-type degree sum is expensive; the project
        // only uses this for an "is the table populated" gate, so we
        // return 0 when no nodes exist and 1 otherwise. Phase F can
        // replace this with a precise count if benchmarks need it.
        if self.count_nodes().await? == 0 {
            Ok(0)
        } else {
            Ok(1)
        }
    }
}

/// Property names owned by [`NodeRow`]'s fixed columns.
///
/// Facts are stored as plain sibling properties — `n.loc`, not
/// `n.f_loc` — so a GQL query reads the way someone would write it by
/// hand. That makes the two indistinguishable on read-back, which is what
/// this list resolves: anything not named here came from
/// [`crate::storage::facts`] and goes back into [`NodeRow::facts`].
const RESERVED_PROPS: &[&str] = &[
    "name",
    "node_type",
    "description",
    "file",
    "start_line",
    "end_line",
    "last_update_at",
    "node_text",
    "code",
    "file_hash",
];

fn node_props(r: &NodeRow) -> BTreeMap<String, PropValue> {
    let mut m = BTreeMap::new();
    m.insert("name".into(), PropValue::String(r.name.clone()));
    m.insert("node_type".into(), PropValue::String(r.node_type.clone()));
    m.insert(
        "description".into(),
        PropValue::String(r.description.clone()),
    );
    m.insert("file".into(), PropValue::String(r.file.clone()));
    m.insert("start_line".into(), PropValue::UInt(r.start_line as u64));
    m.insert("end_line".into(), PropValue::UInt(r.end_line as u64));
    m.insert("last_update_at".into(), PropValue::Int(r.last_update_at));
    m.insert("node_text".into(), PropValue::String(r.node_text.clone()));
    m.insert("code".into(), PropValue::String(r.code.clone()));
    m.insert("file_hash".into(), PropValue::String(r.file_hash.clone()));
    for (k, v) in &r.facts {
        // A fact must never shadow a fixed column; the column is the one
        // every existing reader depends on.
        if RESERVED_PROPS.contains(&k.as_str()) {
            continue;
        }
        m.insert(
            k.clone(),
            match v {
                FactValue::Int(i) => PropValue::Int(*i),
                FactValue::Str(s) => PropValue::String(s.clone()),
            },
        );
    }
    m
}

/// Pull the non-column properties back out as facts.
fn facts_from_props(props: &BTreeMap<String, PropValue>) -> Facts {
    props
        .iter()
        .filter(|(k, _)| !RESERVED_PROPS.contains(&k.as_str()))
        .filter_map(|(k, v)| {
            let fact = match v {
                PropValue::Int(i) => FactValue::Int(*i),
                PropValue::UInt(u) => FactValue::Int(*u as i64),
                PropValue::String(s) => FactValue::Str(s.clone()),
                _ => return None,
            };
            Some((k.clone(), fact))
        })
        .collect()
}

/// Bind one portable parameter value for the engine.
fn gql_param(v: &QueryValue) -> GqlParamValue {
    match v {
        QueryValue::Null => GqlParamValue::Null,
        QueryValue::Bool(b) => GqlParamValue::Bool(*b),
        QueryValue::Int(i) => GqlParamValue::Int(*i),
        QueryValue::Float(f) => GqlParamValue::Float(*f),
        QueryValue::Str(s) => GqlParamValue::String(s.clone()),
        QueryValue::List(items) => GqlParamValue::List(items.iter().map(gql_param).collect()),
    }
}

/// Lower one engine value into the portable [`QueryValue`].
///
/// Nodes, edges and paths collapse to a string — for a node, its project
/// id, which is the one thing a caller can feed straight back into
/// `get_code` or `find_usages`. Returning a hydrated node here would put
/// the whole `code` property into a statistics answer, which is exactly
/// the token cost this feature exists to avoid.
fn query_value(v: &GqlValue) -> QueryValue {
    match v {
        GqlValue::Null => QueryValue::Null,
        GqlValue::Bool(b) => QueryValue::Bool(*b),
        GqlValue::Int(i) => QueryValue::Int(*i),
        GqlValue::UInt(u) => QueryValue::Int(*u as i64),
        GqlValue::Float(f) => QueryValue::Float(*f),
        GqlValue::String(s) => QueryValue::Str(s.clone()),
        GqlValue::List(items) => QueryValue::List(items.iter().map(query_value).collect()),
        GqlValue::Node(n) => match (&n.key, n.id) {
            (Some(k), _) => QueryValue::Str(k.clone()),
            (None, Some(id)) => QueryValue::Str(format!("node:{}", id)),
            _ => QueryValue::Null,
        },
        GqlValue::Edge(e) => QueryValue::Str(
            e.label
                .clone()
                .unwrap_or_else(|| "edge".to_string()),
        ),
        GqlValue::Path(p) => QueryValue::Int(p.edge_ids.len() as i64),
        // Byte blobs and maps have no place in an aggregate answer, and
        // rendering them would be guesswork. Absent beats invented.
        GqlValue::Bytes(_) | GqlValue::Map(_) => QueryValue::Null,
    }
}

fn prop_string(props: &BTreeMap<String, PropValue>, k: &str) -> String {
    match props.get(k) {
        Some(PropValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn prop_u32(props: &BTreeMap<String, PropValue>, k: &str) -> u32 {
    match props.get(k) {
        Some(PropValue::UInt(n)) => *n as u32,
        Some(PropValue::Int(n)) => *n as u32,
        _ => 0,
    }
}

fn prop_i64(props: &BTreeMap<String, PropValue>, k: &str) -> i64 {
    match props.get(k) {
        Some(PropValue::Int(n)) => *n,
        Some(PropValue::UInt(n)) => *n as i64,
        _ => 0,
    }
}

fn node_record_to_row(rec: &NodeView) -> NodeRow {
    NodeRow {
        id: rec.key.clone(),
        name: prop_string(&rec.props, "name"),
        node_type: {
            let s = prop_string(&rec.props, "node_type");
            if s.is_empty() {
                // Ingest writes exactly one label per node; fall back to
                // it when the `node_type` property is missing.
                rec.labels
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Unknown".to_string())
            } else {
                s
            }
        },
        description: prop_string(&rec.props, "description"),
        file: prop_string(&rec.props, "file"),
        start_line: prop_u32(&rec.props, "start_line"),
        end_line: prop_u32(&rec.props, "end_line"),
        last_update_at: prop_i64(&rec.props, "last_update_at"),
        node_text: prop_string(&rec.props, "node_text"),
        vector: rec.dense_vector.clone().unwrap_or_default(),
        code: prop_string(&rec.props, "code"),
        file_hash: prop_string(&rec.props, "file_hash"),
        facts: facts_from_props(&rec.props),
    }
}

/// Pure dense vector search. Wraps OverGraph's `vector_search` in
/// `Dense` mode. The optional `where_clause` argument is preserved for
/// call-site compatibility but currently ignored — see §6 Q1 in
/// `docs/MIGRATION-OVERGRAPH.md` for the SQL `WHERE` removal decision.
pub async fn vector_search(
    db: &Db,
    query_vec: Vec<f32>,
    limit: usize,
    where_clause: Option<&str>,
) -> Result<Vec<(NodeRow, f32)>, DbError> {
    let _ = where_clause; // TODO(overgraph-where): translate to type_filter / property predicate
    let req = VectorSearchRequest {
        mode: VectorSearchMode::Dense,
        dense_query: Some(query_vec),
        sparse_query: None,
        k: limit,
        label_filter: None,
        ef_search: None,
        scope: None,
        dense_weight: None,
        sparse_weight: None,
        fusion_mode: None,
    };
    let hits = db.engine.vector_search(&req)?;
    let mut out: Vec<(NodeRow, f32)> = Vec::with_capacity(hits.len());
    for h in hits {
        if let Some(rec) = db.engine.get_node(h.node_id)? {
            db.remember(rec.key.clone(), rec.id);
            out.push((node_record_to_row(&rec), h.score));
        }
    }
    Ok(out)
}

/// Hybrid dense + sparse search using OverGraph's native fusion. The
/// sparse vector is built by `text::build_sparse_keyword_vector`. This
/// is the function `query::rrf_search` retargets to in Phase D.
pub async fn hybrid_search(
    db: &Db,
    query_vec: Vec<f32>,
    sparse_vec: Vec<(u32, f32)>,
    limit: usize,
    where_clause: Option<&str>,
) -> Result<Vec<(NodeRow, f32)>, DbError> {
    let _ = where_clause;
    let req = VectorSearchRequest {
        mode: VectorSearchMode::Hybrid,
        dense_query: Some(query_vec),
        sparse_query: if sparse_vec.is_empty() {
            None
        } else {
            Some(sparse_vec)
        },
        k: limit,
        label_filter: None,
        ef_search: None,
        scope: None,
        dense_weight: None,
        sparse_weight: None,
        fusion_mode: Some(FusionMode::ReciprocalRankFusion),
    };
    let hits = db.engine.vector_search(&req)?;
    let mut out: Vec<(NodeRow, f32)> = Vec::with_capacity(hits.len());
    for h in hits {
        if let Some(rec) = db.engine.get_node(h.node_id)? {
            db.remember(rec.key.clone(), rec.id);
            out.push((node_record_to_row(&rec), h.score));
        }
    }
    Ok(out)
}

/// All outbound edges from `node_id` (a project string id). Reconstructs
/// the wire-format `EdgeRow` from OverGraph's `NeighborEntry`.
pub async fn edges_from(db: &Db, node_id: &str) -> Result<Vec<EdgeRow>, DbError> {
    edges_in_direction(db, node_id, OgDirection::Outgoing).await
}

pub async fn edges_to(db: &Db, node_id: &str) -> Result<Vec<EdgeRow>, DbError> {
    edges_in_direction(db, node_id, OgDirection::Incoming).await
}

async fn edges_in_direction(
    db: &Db,
    node_id: &str,
    direction: OgDirection,
) -> Result<Vec<EdgeRow>, DbError> {
    let Some(start) = db.lookup_id(node_id)? else {
        return Ok(Vec::new());
    };
    let opts = NeighborOptions {
        direction,
        ..Default::default()
    };
    let neighbors = db.engine.neighbors(start, &opts)?;
    let mut out: Vec<EdgeRow> = Vec::with_capacity(neighbors.len());
    for n in neighbors {
        let neighbor_key = db.key_for(n.node_id);
        let (source, target) = match direction {
            OgDirection::Outgoing => (node_id.to_string(), neighbor_key),
            OgDirection::Incoming => (neighbor_key, node_id.to_string()),
            OgDirection::Both => (node_id.to_string(), neighbor_key),
        };
        let edge_type = n.label.clone();
        out.push(EdgeRow {
            id: format!("{}|{}|{}", source, edge_type, target),
            source,
            target,
            edge_type,
            properties: String::new(),
        });
    }
    Ok(out)
}

/// Bulk-load every edge in the database. Used today only by the project
/// PPR fallback; native PPR replaces it in Phase C, so this is left as
/// `Unimplemented` to surface any caller we missed.
pub async fn all_edges(_db: &Db) -> Result<Vec<EdgeRow>, DbError> {
    Err(DbError::Unimplemented(
        "all_edges — replaced by native OverGraph PPR (see Phase C)",
    ))
}

/// FTS over `name` / `description` strings. OverGraph has no built-in
/// BM25; this stub keeps the call-site compatibility while
/// `text::build_sparse_keyword_vector` (Phase D) provides the actual
/// keyword channel via the hybrid sparse query.
///
/// Returning empty here means `query::rrf_search` degrades to dense-only
/// seeds during the Phase B/C window; Phase D collapses `rrf_search`
/// into `hybrid_search` directly and this function becomes unreachable.
pub async fn fts_search(
    _db: &Db,
    _query: &str,
    _limit: usize,
    _where_clause: Option<&str>,
) -> Result<Vec<NodeRow>, DbError> {
    // TODO(overgraph-fts): once Phase D lands, delete this and have
    // `query::rrf_search` call `db::hybrid_search` directly.
    Ok(Vec::new())
}

pub async fn nodes_by_ids(db: &Db, ids: &[String]) -> Result<Vec<NodeRow>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<NodeRow> = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(numeric_id) = db.lookup_id(id)? {
            if let Some(rec) = db.engine.get_node(numeric_id)? {
                out.push(node_record_to_row(&rec));
            }
        }
    }
    Ok(out)
}

/// Typed counterpart to [`nodes_by_ids`], used by the incremental
/// ingest planner.
///
/// Two things make this cheaper than `nodes_by_ids` on the path that
/// matters (a fresh `ug gen` process, cold `key_to_id`):
///
/// 1. The caller knows each node's type, so we can go straight to
///    `get_nodes_by_keys` instead of `lookup_id`'s probe across every
///    known node label.
/// 2. Every row found is `remember`ed. Ingest skips the upsert for
///    unchanged nodes, so without this the id cache would be left cold
///    and the *edge* upsert that follows would fall back to probing for
///    each endpoint.
pub async fn nodes_for_upsert(db: &Db, keys: &[NodeKey]) -> Result<Vec<NodeRow>, DbError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    // One batched engine round-trip for the whole chunk, keyed by
    // `(label, key)` — the pair OverGraph actually indexes on. Going
    // through `lookup_id` instead would probe every node label per node on
    // a cold cache, which is precisely the situation ingest runs in.
    let lookup: Vec<NodeKeyQuery> = keys
        .iter()
        .map(|k| NodeKeyQuery {
            label: node_label(&k.node_type).to_string(),
            key: k.id.clone(),
        })
        .collect();
    let recs = db.engine.get_nodes_by_keys(&lookup)?;

    let mut out: Vec<NodeRow> = Vec::with_capacity(recs.len());
    for rec in recs.into_iter().flatten() {
        db.remember(rec.key.clone(), rec.id);
        out.push(node_record_to_row(&rec));
    }
    Ok(out)
}

/// Delete every stored node whose key is absent from `keep`.
///
/// Sweeps each node label in turn (OverGraph indexes nodes per label, so
/// there is no single "all nodes" cursor) and deletes the misses. Edges
/// pointing at a deleted node are left to OverGraph's own tombstoning.
///
/// The caller is responsible for `keep` being a *complete* graph — see
/// [`KnowledgeStore::prune_nodes_absent_from`].
pub async fn prune_nodes_absent_from(
    db: &Db,
    keep: &std::collections::HashSet<String>,
) -> Result<usize, DbError> {
    let mut removed = 0usize;
    for label in ALL_NODE_LABELS {
        let ids = db.engine.nodes_by_labels(*label)?;
        for rec in db.engine.get_nodes(&ids)?.into_iter().flatten() {
            if keep.contains(&rec.key) {
                continue;
            }
            db.engine.delete_node(rec.id)?;
            db.forget(&rec.key, rec.id);
            removed += 1;
        }
    }
    Ok(removed)
}

/// Helper used by Phase D's `query::traverse_filtered` retarget — wraps
/// the OverGraph traversal and rehydrates results into the project's
/// (string-id, EdgeRow) wire format.
pub async fn traverse_string_ids(
    db: &Db,
    start_string_id: &str,
    max_hops: u32,
    edge_labels: Option<Vec<String>>,
    direction: OgDirection,
) -> Result<(Vec<NodeRow>, Vec<EdgeRow>, HashMap<String, u32>), DbError> {
    use overgraph::TraverseOptions;
    let Some(start) = db.lookup_id(start_string_id)? else {
        return Ok((Vec::new(), Vec::new(), HashMap::new()));
    };
    let opts = TraverseOptions {
        edge_label_filter: edge_labels,
        direction,
        ..Default::default()
    };
    let page = db.engine.traverse(start, max_hops, &opts)?;

    let mut nodes: Vec<NodeRow> = Vec::new();
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut node_ids: Vec<u64> = Vec::with_capacity(page.items.len() + 1);
    node_ids.push(start);
    for hit in &page.items {
        node_ids.push(hit.node_id);
    }
    let records = db.engine.get_nodes(&node_ids)?;
    for (ix, rec_opt) in records.iter().enumerate() {
        if let Some(rec) = rec_opt {
            db.remember(rec.key.clone(), rec.id);
            nodes.push(node_record_to_row(rec));
            let depth = if ix == 0 { 0 } else { page.items[ix - 1].depth };
            distances.insert(rec.key.clone(), depth);
        }
    }

    // Reconstruct edges by reading `via_edge_id` for each hit.
    let mut edges: Vec<EdgeRow> = Vec::new();
    let edge_ids: Vec<u64> = page.items.iter().filter_map(|h| h.via_edge_id).collect();
    let edge_records: Vec<Option<EdgeView>> = db.engine.get_edges(&edge_ids)?;
    for rec_opt in edge_records.into_iter().flatten() {
        edges.push(edge_record_to_row(db, &rec_opt));
    }
    Ok((nodes, edges, distances))
}

fn edge_record_to_row(db: &Db, rec: &EdgeView) -> EdgeRow {
    let source = db.key_for(rec.from);
    let target = db.key_for(rec.to);
    let edge_type = rec.label.clone();
    EdgeRow {
        id: format!("{}|{}|{}", source, edge_type, target),
        source,
        target,
        edge_type,
        properties: String::new(),
    }
}

fn to_og_direction(d: Direction) -> OgDirection {
    match d {
        Direction::Outbound => OgDirection::Outgoing,
        Direction::Inbound => OgDirection::Incoming,
        Direction::Both => OgDirection::Both,
    }
}

/// `Db` (the OverGraph backend) implements the cross-backend
/// [`KnowledgeStore`] trait. The trait methods delegate to the
/// existing inherent methods / free functions in this module so the
/// public OverGraph API stays as-is for back-compat tests.
///
/// Filter handling: OverGraph's `vector_search` doesn't yet honor
/// property predicates (see `MIGRATION-OVERGRAPH §6 Q1`). The trait's
/// `filter` argument is therefore accepted but currently ignored.
#[async_trait]
impl KnowledgeStore for Db {
    fn embedding_dim(&self) -> u32 {
        Db::embedding_dim(self)
    }

    fn supports_native_ppr(&self) -> bool {
        true
    }

    fn backend_name(&self) -> &'static str {
        "overgraph"
    }

    fn ingest_model(&self) -> Option<String> {
        Db::recorded_model(self)
    }

    fn sparse_stats(&self) -> Option<Arc<SparseStats>> {
        self.sparse_stats.read().ok().and_then(|g| g.clone())
    }

    fn ensure_query_indexes(&self) {
        Db::ensure_fact_indexes(self);
    }

    async fn execute_query(
        &self,
        gql: &str,
        params: &QueryParams,
        limits: &QueryLimits,
    ) -> Result<QueryPage, StoreError> {
        // The engine's GQL entry point is synchronous; the trait is async
        // because Neo4j's would not be.
        Ok(Db::execute_gql(self, gql, params, limits)?)
    }

    fn set_sparse_stats(&self, stats: Arc<SparseStats>) {
        if let Err(e) = stats.save(&self.path) {
            tracing::warn!(error = %e, "could not write sparse-stats sidecar");
        }
        if let Ok(mut slot) = self.sparse_stats.write() {
            *slot = Some(stats);
        }
    }

    fn record_ingest_model(&self, model: &str) {
        if let Err(e) = Db::record_model(self, model) {
            // Losing the stamp costs a redundant re-embed on the next run,
            // not correctness — never fail an otherwise-good ingest for it.
            tracing::warn!(error = %e, "could not record ingest model in ug-meta.json");
        }
    }

    async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), StoreError> {
        Db::upsert_nodes(self, rows).await.map_err(StoreError::from)
    }

    async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), StoreError> {
        Db::upsert_edges(self, rows).await.map_err(StoreError::from)
    }

    async fn vector_search(
        &self,
        query: Vec<f32>,
        k: usize,
        _filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError> {
        // TODO(overgraph-where): translate filter.node_types into
        // OverGraph's `type_filter`. v1 ignores it (matches the
        // pre-trait behavior).
        vector_search(self, query, k, None)
            .await
            .map_err(StoreError::from)
    }

    async fn hybrid_search(
        &self,
        query: Vec<f32>,
        sparse: Vec<(u32, f32)>,
        _query_text: &str,
        k: usize,
        _filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError> {
        // OverGraph uses the pre-built sparse vector. `query_text` is
        // the Neo4j input — irrelevant here.
        hybrid_search(self, query, sparse, k, None)
            .await
            .map_err(StoreError::from)
    }

    async fn traverse(
        &self,
        start: &str,
        max_hops: u32,
        edge_types: Option<&[String]>,
        direction: Direction,
    ) -> Result<TraversalPage, StoreError> {
        let edge_labels: Option<Vec<String>> = edge_types
            .filter(|v| !v.is_empty())
            .map(|v| v.iter().map(|s| edge_label(s).to_string()).collect());
        let og_dir = to_og_direction(direction);
        let (rows, edges, distances) =
            traverse_string_ids(self, start, max_hops, edge_labels, og_dir).await?;
        let nodes: Vec<TraversalNode> = rows
            .into_iter()
            .map(|row| {
                let depth = distances.get(&row.id).copied().unwrap_or(0);
                TraversalNode { row, depth }
            })
            .collect();
        Ok(TraversalPage { nodes, edges })
    }

    async fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<NodeRow>, StoreError> {
        nodes_by_ids(self, ids).await.map_err(StoreError::from)
    }

    async fn nodes_for_upsert(&self, keys: &[NodeKey]) -> Result<Vec<NodeRow>, StoreError> {
        nodes_for_upsert(self, keys)
            .await
            .map_err(StoreError::from)
    }

    async fn prune_nodes_absent_from(
        &self,
        keep: &std::collections::HashSet<String>,
    ) -> Result<usize, StoreError> {
        prune_nodes_absent_from(self, keep)
            .await
            .map_err(StoreError::from)
    }

    async fn fetch_node(&self, key: &str) -> Result<Option<NodeRow>, StoreError> {
        Db::fetch_node(self, key).map_err(StoreError::from)
    }

    async fn count_nodes(&self) -> Result<usize, StoreError> {
        Db::count_nodes(self).await.map_err(StoreError::from)
    }

    async fn count_edges(&self) -> Result<usize, StoreError> {
        Db::count_edges(self).await.map_err(StoreError::from)
    }

    async fn personalized_pagerank(
        &self,
        seeds: &[String],
        _direction: Direction,
        edge_types: Option<&[String]>,
        restart_prob: f32,
        max_iter: usize,
        max_results: Option<usize>,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let mut seed_ids: Vec<u64> = Vec::with_capacity(seeds.len());
        for s in seeds {
            if let Some(id) = self.lookup_id(s)? {
                seed_ids.push(id);
            }
        }
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let damping = (1.0 - restart_prob.clamp(0.01, 0.99)) as f64;
        let edge_label_filter: Option<Vec<String>> = edge_types
            .filter(|v| !v.is_empty())
            .map(|v| v.iter().map(|s| edge_label(s).to_string()).collect());
        let opts = OgPprOptions {
            algorithm: PprAlgorithm::ExactPowerIteration,
            damping_factor: damping,
            max_iterations: max_iter.max(1) as u32,
            epsilon: 1e-6,
            approx_residual_tolerance: 1e-5,
            edge_label_filter,
            max_results,
        };
        let result: OgPprResult = self
            .engine
            .personalized_pagerank(&seed_ids, &opts)
            .map_err(|e| StoreError::Backend(format!("overgraph ppr: {}", e)))?;
        let mut out: Vec<(String, f32)> = Vec::with_capacity(result.scores.len());
        for (id, score) in result.scores {
            out.push((self.key_for(id), score as f32));
        }
        Ok(out)
    }
}
