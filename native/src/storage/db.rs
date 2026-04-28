//! LanceDB persistence for graph nodes and edges.
//!
//! Two tables live side by side in the same connection directory:
//!
//! - `nodes`: one row per graph node, with the embedded text and a
//!   1024-dim FixedSizeList vector column. This is what semantic search
//!   queries against.
//! - `edges`: one row per graph edge, no vector column. Used by graph
//!   traversal queries that walk source -> target.
//!
//! Upserts are implemented as delete-by-id-then-add. LanceDB's
//! `merge_insert` would be slightly more efficient but the API surface
//! changes between minor versions; delete+add is stable across them and
//! the cost is negligible at our scale.

use crate::storage::embed::EMBEDDING_DIM;
use arrow_array::{
    builder::FixedSizeListBuilder, builder::Float32Builder, Array, ArrayRef, Float32Array,
    RecordBatch, RecordBatchIterator, RecordBatchReader, StringArray, UInt32Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, Connection, Table};
use std::sync::Arc;

pub const NODES_TABLE: &str = "nodes";
pub const EDGES_TABLE: &str = "edges";

#[derive(Debug)]
pub enum DbError {
    Lance(lancedb::Error),
    Arrow(arrow_schema::ArrowError),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Lance(e) => write!(f, "lancedb error: {}", e),
            DbError::Arrow(e) => write!(f, "arrow error: {}", e),
            DbError::Io(e) => write!(f, "io error: {}", e),
            DbError::Json(e) => write!(f, "json error: {}", e),
        }
    }
}

impl std::error::Error for DbError {}

impl From<lancedb::Error> for DbError {
    fn from(e: lancedb::Error) -> Self {
        DbError::Lance(e)
    }
}
impl From<arrow_schema::ArrowError> for DbError {
    fn from(e: arrow_schema::ArrowError) -> Self {
        DbError::Arrow(e)
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

/// A row that goes into the `nodes` table. The fields mirror the schema
/// returned by [`nodes_schema`]; keeping them as a plain struct lets the
/// upsert path stay readable without juggling columnar arrays everywhere.
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

pub fn nodes_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("node_type", DataType::Utf8, false),
        Field::new("description", DataType::Utf8, true),
        Field::new("file", DataType::Utf8, true),
        Field::new("start_line", DataType::UInt32, true),
        Field::new("end_line", DataType::UInt32, true),
        Field::new("last_update_at", DataType::Int64, false),
        Field::new("node_text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                EMBEDDING_DIM as i32,
            ),
            false,
        ),
    ]))
}

pub fn edges_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("edge_type", DataType::Utf8, false),
        Field::new("properties", DataType::Utf8, true),
    ]))
}

pub struct Db {
    pub conn: Connection,
    pub nodes: Table,
    pub edges: Table,
}

impl Db {
    /// Open or create the LanceDB instance at `path`. Both tables are
    /// created with empty contents on first call; subsequent calls reopen
    /// the existing tables.
    pub async fn open(path: &str) -> Result<Self, DbError> {
        let conn = connect(path).execute().await?;
        let existing = conn.table_names().execute().await?;

        if !existing.iter().any(|n| n == NODES_TABLE) {
            conn.create_empty_table(NODES_TABLE, nodes_schema())
                .execute()
                .await?;
        }
        if !existing.iter().any(|n| n == EDGES_TABLE) {
            conn.create_empty_table(EDGES_TABLE, edges_schema())
                .execute()
                .await?;
        }

        let nodes = conn.open_table(NODES_TABLE).execute().await?;
        let edges = conn.open_table(EDGES_TABLE).execute().await?;
        Ok(Self { conn, nodes, edges })
    }

    pub async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), DbError> {
        if rows.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        delete_by_ids(&self.nodes, &ids).await?;

        let batch = node_rows_to_batch(rows)?;
        let schema = nodes_schema();
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        self.nodes.add(reader).execute().await?;
        Ok(())
    }

    pub async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), DbError> {
        if rows.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        delete_by_ids(&self.edges, &ids).await?;

        let batch = edge_rows_to_batch(rows)?;
        let schema = edges_schema();
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        self.edges.add(reader).execute().await?;
        Ok(())
    }

    /// Best-effort vector index creation. IvfPq requires a minimum
    /// number of training rows; for tiny datasets the call would fail.
    /// We surface the error to the caller so they can decide whether to
    /// log-and-continue or fail hard.
    pub async fn try_create_vector_index(&self) -> Result<(), DbError> {
        use lancedb::index::Index;
        self.nodes
            .create_index(&["vector"], Index::Auto)
            .execute()
            .await?;
        Ok(())
    }

    pub async fn try_create_fts_index(&self) -> Result<(), DbError> {
        use lancedb::index::scalar::FtsIndexBuilder;
        use lancedb::index::Index;
        self.nodes
            .create_index(&["name"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await?;
        self.nodes
            .create_index(&["description"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await?;
        Ok(())
    }

    pub async fn count_nodes(&self) -> Result<usize, DbError> {
        Ok(self.nodes.count_rows(None).await?)
    }

    pub async fn count_edges(&self) -> Result<usize, DbError> {
        Ok(self.edges.count_rows(None).await?)
    }
}

async fn delete_by_ids(table: &Table, ids: &[&str]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    // SQL-quote each id, escaping embedded apostrophes. IDs in this
    // codebase are paths like `file:src/foo.ts` so escaping is overkill in
    // practice, but the cost is one allocation and it removes a footgun.
    let quoted: Vec<String> = ids
        .iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect();
    let predicate = format!("id IN ({})", quoted.join(","));
    table.delete(&predicate).await?;
    Ok(())
}

fn node_rows_to_batch(rows: &[NodeRow]) -> Result<RecordBatch, DbError> {
    let schema = nodes_schema();

    let ids = StringArray::from_iter_values(rows.iter().map(|r| r.id.as_str()));
    let names = StringArray::from_iter_values(rows.iter().map(|r| r.name.as_str()));
    let node_types = StringArray::from_iter_values(rows.iter().map(|r| r.node_type.as_str()));
    let descriptions =
        StringArray::from(rows.iter().map(|r| Some(r.description.as_str())).collect::<Vec<_>>());
    let files = StringArray::from(rows.iter().map(|r| Some(r.file.as_str())).collect::<Vec<_>>());
    let start_lines = UInt32Array::from(rows.iter().map(|r| Some(r.start_line)).collect::<Vec<_>>());
    let end_lines = UInt32Array::from(rows.iter().map(|r| Some(r.end_line)).collect::<Vec<_>>());
    let last_updates: Vec<i64> = rows.iter().map(|r| r.last_update_at).collect();
    let last_update_array = arrow_array::Int64Array::from(last_updates);
    let node_texts = StringArray::from_iter_values(rows.iter().map(|r| r.node_text.as_str()));

    // FixedSizeList<Float32, EMBEDDING_DIM>. Builder pattern is the
    // simplest way to populate one without manually constructing offsets.
    let mut vec_builder = FixedSizeListBuilder::new(
        Float32Builder::with_capacity(rows.len() * EMBEDDING_DIM),
        EMBEDDING_DIM as i32,
    );
    for r in rows {
        if r.vector.len() != EMBEDDING_DIM {
            return Err(DbError::Arrow(arrow_schema::ArrowError::InvalidArgumentError(
                format!(
                    "vector for {} has dim {}, expected {}",
                    r.id,
                    r.vector.len(),
                    EMBEDDING_DIM
                ),
            )));
        }
        vec_builder.values().append_slice(&r.vector);
        vec_builder.append(true);
    }
    let vectors = vec_builder.finish();

    let columns: Vec<ArrayRef> = vec![
        Arc::new(ids),
        Arc::new(names),
        Arc::new(node_types),
        Arc::new(descriptions),
        Arc::new(files),
        Arc::new(start_lines),
        Arc::new(end_lines),
        Arc::new(last_update_array),
        Arc::new(node_texts),
        Arc::new(vectors),
    ];
    Ok(RecordBatch::try_new(schema, columns)?)
}

fn edge_rows_to_batch(rows: &[EdgeRow]) -> Result<RecordBatch, DbError> {
    let schema = edges_schema();
    let ids = StringArray::from_iter_values(rows.iter().map(|r| r.id.as_str()));
    let sources = StringArray::from_iter_values(rows.iter().map(|r| r.source.as_str()));
    let targets = StringArray::from_iter_values(rows.iter().map(|r| r.target.as_str()));
    let etypes = StringArray::from_iter_values(rows.iter().map(|r| r.edge_type.as_str()));
    let props = StringArray::from(
        rows.iter()
            .map(|r| Some(r.properties.as_str()))
            .collect::<Vec<_>>(),
    );

    let columns: Vec<ArrayRef> = vec![
        Arc::new(ids),
        Arc::new(sources),
        Arc::new(targets),
        Arc::new(etypes),
        Arc::new(props),
    ];
    Ok(RecordBatch::try_new(schema, columns)?)
}

/// Run a vector query and decode each returned RecordBatch into our
/// in-memory [`NodeRow`] representation. The `_distance` column added by
/// LanceDB is mirrored into a parallel `Vec<f32>` so the caller can rank
/// by similarity without re-querying.
pub async fn vector_search(
    db: &Db,
    query_vec: Vec<f32>,
    limit: usize,
    where_clause: Option<&str>,
) -> Result<Vec<(NodeRow, f32)>, DbError> {
    let mut q = db.nodes.query().nearest_to(query_vec)?.limit(limit);
    if let Some(filter) = where_clause {
        q = q.only_if(filter);
    }
    let stream = q.execute().await?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    Ok(decode_node_batches_with_distance(&batches))
}

/// Read every edge whose source matches `node_id`. Used by graph
/// traversal to expand a frontier one hop at a time.
pub async fn edges_from(db: &Db, node_id: &str) -> Result<Vec<EdgeRow>, DbError> {
    let escaped = node_id.replace('\'', "''");
    let predicate = format!("source = '{}'", escaped);
    let stream = db
        .edges
        .query()
        .only_if(predicate)
        .execute()
        .await?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    Ok(decode_edge_batches(&batches))
}

pub async fn nodes_by_ids(db: &Db, ids: &[String]) -> Result<Vec<NodeRow>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let quoted: Vec<String> = ids
        .iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect();
    let predicate = format!("id IN ({})", quoted.join(","));
    let stream = db
        .nodes
        .query()
        .only_if(predicate)
        .execute()
        .await?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    Ok(decode_node_batches_with_distance(&batches)
        .into_iter()
        .map(|(r, _)| r)
        .collect())
}

fn decode_node_batches_with_distance(batches: &[RecordBatch]) -> Vec<(NodeRow, f32)> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::Float32Type;
    let mut out: Vec<(NodeRow, f32)> = Vec::new();
    for batch in batches {
        let schema = batch.schema();
        let n = batch.num_rows();
        if n == 0 {
            continue;
        }

        let ids = batch.column_by_name("id").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let names = batch.column_by_name("name").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let node_types = batch.column_by_name("node_type").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let descriptions = batch.column_by_name("description").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let files = batch.column_by_name("file").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let start_lines = batch.column_by_name("start_line").and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
        let end_lines = batch.column_by_name("end_line").and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
        let last_updates = batch.column_by_name("last_update_at").and_then(|c| c.as_any().downcast_ref::<arrow_array::Int64Array>());
        let node_texts = batch.column_by_name("node_text").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let distances = batch.column_by_name("_distance").and_then(|c| c.as_any().downcast_ref::<Float32Array>());

        let vectors_col = batch.column_by_name("vector");
        let vectors_list = vectors_col.map(|c| c.as_fixed_size_list());

        for i in 0..n {
            let mut vector: Vec<f32> = Vec::new();
            if let Some(list) = vectors_list {
                let single = list.value(i);
                if let Some(prim) = single.as_primitive_opt::<Float32Type>() {
                    vector = prim.values().to_vec();
                }
            }
            // Skip rows missing required columns rather than panicking; an
            // older table written with a different schema would otherwise
            // crash the query.
            let id = match ids.and_then(|a| Some(a.value(i).to_string())) { Some(v) => v, None => continue };
            let name = names.map(|a| a.value(i).to_string()).unwrap_or_default();
            let node_type = node_types.map(|a| a.value(i).to_string()).unwrap_or_default();
            let description = descriptions
                .map(|a| if a.is_null(i) { String::new() } else { a.value(i).to_string() })
                .unwrap_or_default();
            let file = files
                .map(|a| if a.is_null(i) { String::new() } else { a.value(i).to_string() })
                .unwrap_or_default();
            let start_line = start_lines
                .map(|a| if a.is_null(i) { 0 } else { a.value(i) })
                .unwrap_or(0);
            let end_line = end_lines
                .map(|a| if a.is_null(i) { 0 } else { a.value(i) })
                .unwrap_or(0);
            let last_update_at = last_updates.map(|a| a.value(i)).unwrap_or(0);
            let node_text = node_texts.map(|a| a.value(i).to_string()).unwrap_or_default();
            let distance = distances.map(|a| a.value(i)).unwrap_or(0.0);

            let _ = &schema;
            out.push((
                NodeRow {
                    id,
                    name,
                    node_type,
                    description,
                    file,
                    start_line,
                    end_line,
                    last_update_at,
                    node_text,
                    vector,
                },
                distance,
            ));
        }
    }
    out
}

fn decode_edge_batches(batches: &[RecordBatch]) -> Vec<EdgeRow> {
    let mut out: Vec<EdgeRow> = Vec::new();
    for batch in batches {
        let n = batch.num_rows();
        if n == 0 {
            continue;
        }
        let ids = batch.column_by_name("id").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let sources = batch.column_by_name("source").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let targets = batch.column_by_name("target").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let etypes = batch.column_by_name("edge_type").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let props = batch.column_by_name("properties").and_then(|c| c.as_any().downcast_ref::<StringArray>());

        for i in 0..n {
            let id = match ids.map(|a| a.value(i).to_string()) { Some(v) => v, None => continue };
            let source = sources.map(|a| a.value(i).to_string()).unwrap_or_default();
            let target = targets.map(|a| a.value(i).to_string()).unwrap_or_default();
            let edge_type = etypes.map(|a| a.value(i).to_string()).unwrap_or_default();
            let properties = props
                .map(|a| if a.is_null(i) { String::new() } else { a.value(i).to_string() })
                .unwrap_or_default();
            out.push(EdgeRow { id, source, target, edge_type, properties });
        }
    }
    out
}
