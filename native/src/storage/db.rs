//! OverGraph persistence for graph nodes and edges.
//!
//! [`Db`] wraps an [`overgraph::DatabaseEngine`] and adds a
//! `String → u64` id cache so callers can keep using the project's
//! string ids (`"file:src/foo.ts"`, etc.) while OverGraph uses numeric
//! ids internally.
//!
//! The public function names and signatures intentionally mirror the
//! previous LanceDB layer (`upsert_nodes`, `upsert_edges`, `vector_search`,
//! `fts_search`, `edges_from`, `edges_to`, `nodes_by_ids`, `all_edges`)
//! so `query.rs` and `ingest.rs` don't need to change in this phase.
//! Phase D will retarget those callers to the more idiomatic OverGraph
//! APIs (native hybrid search, `db.traverse`, etc.).

use crate::storage::embed::EMBEDDING_DIM;
use crate::storage::types_registry::{
    edge_type_from_id, edge_type_to_id, node_type_from_id, node_type_to_id,
};
use overgraph::{
    DatabaseEngine, DbOptions, DenseMetric, DenseVectorConfig, Direction as OgDirection, EdgeInput,
    EdgeRecord, EngineError, FusionMode, HnswConfig, NeighborOptions, NodeInput, NodeRecord,
    PropValue, VectorSearchMode, VectorSearchRequest,
};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::RwLock;

#[derive(Debug)]
pub enum DbError {
    Engine(EngineError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Unimplemented(&'static str),
    BadVector { id: String, got: usize, want: usize },
    UnknownEndpoint(String),
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
/// `query.rs`, `ingest.rs`, and the JSON outputs in `napi_bindings.rs`
/// keep working unchanged.
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
    /// Open or create the OverGraph database at `path`. The dense vector
    /// space is configured for 1024-dim cosine embeddings to match the
    /// project's Qwen3 model.
    ///
    /// OverGraph's open is synchronous; the `async` signature is preserved
    /// for call-site compatibility. The blocking call is microsecond-scale
    /// at worst (memory-mapping segments) so we don't bother with
    /// `spawn_blocking` here.
    pub async fn open(path: &str) -> Result<Self, DbError> {
        let path = Path::new(path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let opts = DbOptions {
            dense_vector: Some(DenseVectorConfig {
                dimension: EMBEDDING_DIM as u32,
                metric: DenseMetric::Cosine,
                hnsw: HnswConfig::default(),
            }),
            ..Default::default()
        };
        let engine = DatabaseEngine::open(path, &opts)?;
        Ok(Self {
            engine,
            key_to_id: RwLock::new(HashMap::new()),
            id_to_key: RwLock::new(HashMap::new()),
        })
    }

    /// Return the OverGraph numeric id for a project string id, looking
    /// it up via the cache first, then via OverGraph if absent. Returns
    /// `None` for endpoints that haven't been ingested yet.
    pub fn lookup_id(&self, key: &str) -> Result<Option<u64>, DbError> {
        if let Some(id) = self.key_to_id.read().unwrap().get(key).copied() {
            return Ok(Some(id));
        }
        // Slow path — try every known node type. OverGraph keys nodes by
        // (type_id, key) so we have to probe; in practice the cache is
        // hot so this rarely fires.
        for &type_id in &[1u32, 2, 3, 4, 5, 6, 7, 8, 99] {
            if let Some(rec) = self.engine.get_node_by_key(type_id, key)? {
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
        for r in rows {
            if r.vector.len() != EMBEDDING_DIM {
                return Err(DbError::BadVector {
                    id: r.id.clone(),
                    got: r.vector.len(),
                    want: EMBEDDING_DIM,
                });
            }
            inputs.push(NodeInput {
                type_id: node_type_to_id(&r.node_type),
                key: r.id.clone(),
                props: node_props(r),
                weight: 1.0,
                dense_vector: Some(r.vector.clone()),
                sparse_vector: None,
            });
        }
        let ids = self.engine.batch_upsert_nodes(&inputs)?;
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
                type_id: edge_type_to_id(&r.edge_type),
                props: BTreeMap::new(),
                weight,
                valid_from: None,
                valid_to: None,
            });
        }
        self.engine.batch_upsert_edges(&inputs)?;
        Ok(())
    }

    /// No-op on OverGraph — vector indexes are built per-segment at flush
    /// time. Kept for call-site compatibility with the previous LanceDB
    /// API;
    pub async fn try_create_vector_index(&self) -> Result<(), DbError> {
        Ok(())
    }

    pub async fn try_create_fts_index(&self) -> Result<(), DbError> {
        Ok(())
    }

    pub async fn count_nodes(&self) -> Result<usize, DbError> {
        let stats = self.engine.stats()?;
        // OverGraph's `stats` doesn't expose live node count directly; we
        // fall back to summing visible types. Cheap when called rarely.
        let total = (1u32..=99)
            .filter_map(|tid| self.engine.nodes_by_type(tid).ok())
            .map(|v| v.len())
            .sum();
        let _ = stats;
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
    m
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

fn node_record_to_row(rec: &NodeRecord) -> NodeRow {
    NodeRow {
        id: rec.key.clone(),
        name: prop_string(&rec.props, "name"),
        node_type: {
            let s = prop_string(&rec.props, "node_type");
            if s.is_empty() {
                node_type_from_id(rec.type_id).to_string()
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
        type_filter: None,
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
        type_filter: None,
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
        let edge_type = edge_type_from_id(n.edge_type_id).to_string();
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

/// Helper used by Phase D's `query::traverse_filtered` retarget — wraps
/// the OverGraph traversal and rehydrates results into the project's
/// (string-id, EdgeRow) wire format.
pub async fn traverse_string_ids(
    db: &Db,
    start_string_id: &str,
    max_hops: u32,
    edge_type_ids: Option<Vec<u32>>,
    direction: OgDirection,
) -> Result<(Vec<NodeRow>, Vec<EdgeRow>, HashMap<String, u32>), DbError> {
    use overgraph::TraverseOptions;
    let Some(start) = db.lookup_id(start_string_id)? else {
        return Ok((Vec::new(), Vec::new(), HashMap::new()));
    };
    let opts = TraverseOptions {
        edge_type_filter: edge_type_ids,
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
    let edge_records: Vec<Option<EdgeRecord>> = db.engine.get_edges(&edge_ids)?;
    for rec_opt in edge_records.into_iter().flatten() {
        edges.push(edge_record_to_row(db, &rec_opt));
    }
    Ok((nodes, edges, distances))
}

fn edge_record_to_row(db: &Db, rec: &EdgeRecord) -> EdgeRow {
    let source = db.key_for(rec.from);
    let target = db.key_for(rec.to);
    let edge_type = edge_type_from_id(rec.type_id).to_string();
    EdgeRow {
        id: format!("{}|{}|{}", source, edge_type, target),
        source,
        target,
        edge_type,
        properties: String::new(),
    }
}
