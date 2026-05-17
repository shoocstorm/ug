//! Neo4j backend for [`KnowledgeStore`].
//!
//! Connects to a running Neo4j 5.13+ server via the Bolt protocol
//! (`neo4rs = "0.8"`), ensures the schema (`:UgNode` constraint +
//! vector index + full-text index) is in place, and probes for the
//! optional GDS / APOC plugins on `open`. PPR is gated on GDS — when
//! the plugin is missing, [`personalized_pagerank`] returns
//! [`StoreError::Unsupported`] and `query::search_kb` automatically
//! falls back to MMR.
//!
//! Schema:
//! - Every node is labeled `:UgNode` plus a dynamic label per
//!   `node_type` (`:Function`, `:Class`, …) for fast type filtering.
//! - `(n:UgNode { id })` is uniqueness-constrained.
//! - The dense vector lives on `n.embedding` (List<Float>).
//! - The full-text index covers `n.name`, `n.description`, `n.node_text`.
//! - Edge type names match the project's `GraphEdgeType` debug
//!   formatting (`Calls`, `Imports`, …), with `weight` baked from
//!   `default_edge_type_weights()` at ingest.
//!
//! See `docs/MULTI-DEST-PLAN.md` for the design rationale.

use crate::storage::db::{EdgeRow, NodeRow};
use crate::storage::ppr::default_edge_type_weights;
use crate::storage::store::{
    Direction, KnowledgeStore, NodeFilter, StoreError, TraversalNode, TraversalPage,
};
use crate::storage::text::reciprocal_rank_fusion;
use async_trait::async_trait;
use neo4rs::{query, BoltMap, BoltString, BoltType, ConfigBuilder, Graph, Node};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Singleton meta node label used to store the embedding dim.
const META_LABEL: &str = "UgMeta";
const NODE_LABEL: &str = "UgNode";
const VECTOR_INDEX: &str = "ug_node_vec";
const FTS_INDEX: &str = "ug_node_text";
const ID_CONSTRAINT: &str = "ug_node_id_unique";

pub struct Neo4jStore {
    graph: Arc<Graph>,
    /// Optional database name; reported in `db_name()` for diagnostics
    /// and reserved for future per-database routing.
    #[allow(dead_code)]
    database: Option<String>,
    embedding_dim: u32,
    gds_available: bool,
    /// Reported at open-time for diagnostics; future write paths can
    /// use it to switch to APOC-backed batch upserts.
    #[allow(dead_code)]
    apoc_available: bool,
}

impl Neo4jStore {
    /// Connect to Neo4j, probe APOC + GDS, and ensure schema. Idempotent
    /// — safe to call against a database that already has the indexes.
    pub async fn open(
        uri: &str,
        user: &str,
        password: &str,
        database: Option<&str>,
        embedding_dim: u32,
    ) -> Result<Self, StoreError> {
        // neo4rs takes `host:port` (no `bolt://` scheme). Strip it if the
        // caller passed a full URL — common in env vars copied from
        // docker-compose etc.
        let cleaned_uri = uri
            .strip_prefix("neo4j://")
            .or_else(|| uri.strip_prefix("bolt://"))
            .unwrap_or(uri)
            .trim_end_matches('/')
            .to_string();
        let mut cfg_builder = ConfigBuilder::default()
            .uri(cleaned_uri)
            .user(user)
            .password(password);
        if let Some(db) = database {
            cfg_builder = cfg_builder.db(db);
        }
        let cfg = cfg_builder
            .build()
            .map_err(|e| StoreError::Backend(format!("neo4j config: {}", e)))?;
        let graph = Graph::connect(cfg)
            .await
            .map_err(|e| StoreError::Auth(format!("neo4j connect: {}", e)))?;
        let graph = Arc::new(graph);

        // Capability probes. Both procedures are unavailable on plain
        // Community installs; we degrade gracefully.
        let gds_available = probe_procedure_prefix(&graph, "gds.").await?;
        let apoc_available = probe_procedure_prefix(&graph, "apoc.").await?;

        // Schema setup — idempotent; safe across re-opens.
        ensure_schema(&graph, embedding_dim).await?;
        // Persist the dim so re-opens can validate, and reject mismatched
        // re-opens to avoid silently mixing vector sizes.
        let stored_dim = read_or_write_meta(&graph, embedding_dim).await?;
        if stored_dim != embedding_dim {
            return Err(StoreError::DimMismatch {
                existing: stored_dim,
                requested: embedding_dim,
            });
        }

        tracing::info!(
            backend = "neo4j",
            gds = gds_available,
            apoc = apoc_available,
            dim = embedding_dim,
            "neo4j store opened"
        );

        Ok(Self {
            graph,
            database: database.map(|s| s.to_string()),
            embedding_dim,
            gds_available,
            apoc_available,
        })
    }

    /// Reported in diagnostics; reserved for future per-database routing.
    #[allow(dead_code)]
    fn db_name(&self) -> &str {
        self.database.as_deref().unwrap_or("neo4j")
    }
}

/// Probe `SHOW PROCEDURES` for any procedure whose name starts with
/// `prefix`. Used to detect GDS (`gds.*`) and APOC (`apoc.*`) plugins.
async fn probe_procedure_prefix(graph: &Graph, prefix: &str) -> Result<bool, StoreError> {
    let mut result = graph
        .execute(
            query(
                "SHOW PROCEDURES YIELD name WHERE name STARTS WITH $p RETURN count(name) AS c",
            )
            .param("p", prefix),
        )
        .await
        .map_err(|e| StoreError::Backend(format!("neo4j show procedures: {}", e)))?;
    let row = result
        .next()
        .await
        .map_err(|e| StoreError::Backend(format!("neo4j fetch row: {}", e)))?
        .ok_or_else(|| StoreError::Backend("neo4j show procedures returned no rows".into()))?;
    let c: i64 = row
        .get("c")
        .map_err(|e| StoreError::Backend(format!("neo4j parse c: {}", e)))?;
    Ok(c > 0)
}

/// Create the unique constraint + vector + full-text indexes if they
/// don't exist. Uses Neo4j 5.13+ syntax (`CREATE INDEX … IF NOT EXISTS`)
/// — the same statements are no-ops on a database that already has
/// them.
async fn ensure_schema(graph: &Graph, embedding_dim: u32) -> Result<(), StoreError> {
    let stmts = [
        format!(
            "CREATE CONSTRAINT {} IF NOT EXISTS FOR (n:{}) REQUIRE n.id IS UNIQUE",
            ID_CONSTRAINT, NODE_LABEL
        ),
        format!(
            "CREATE VECTOR INDEX {} IF NOT EXISTS FOR (n:{}) ON (n.embedding) \
             OPTIONS {{ indexConfig: {{ `vector.dimensions`: {}, `vector.similarity_function`: 'cosine' }} }}",
            VECTOR_INDEX, NODE_LABEL, embedding_dim
        ),
        format!(
            "CREATE FULLTEXT INDEX {} IF NOT EXISTS FOR (n:{}) ON EACH [n.name, n.description, n.node_text]",
            FTS_INDEX, NODE_LABEL
        ),
    ];
    for stmt in &stmts {
        if let Err(e) = graph.run(query(stmt)).await {
            // `IF NOT EXISTS` does not protect against the race where
            // two concurrent connections both start to create an
            // equivalent index. Neo4j reports
            // `EquivalentSchemaRuleAlreadyExists` in that case — the
            // outcome we actually wanted, so swallow it.
            let msg = format!("{}", e);
            if msg.contains("EquivalentSchemaRuleAlreadyExists")
                || msg.contains("already exists")
            {
                continue;
            }
            return Err(StoreError::Backend(format!(
                "neo4j schema setup: {}: {}",
                stmt, e
            )));
        }
    }
    Ok(())
}

/// Read the persisted embedding dim from the `:UgMeta` singleton node;
/// create it if absent. Mirrors `ug-meta.json` for OverGraph.
async fn read_or_write_meta(graph: &Graph, embedding_dim: u32) -> Result<u32, StoreError> {
    let mut result = graph
        .execute(query(&format!(
            "MERGE (m:{label} {{ key: 'singleton' }}) \
             ON CREATE SET m.embedding_dim = $dim \
             RETURN m.embedding_dim AS dim",
            label = META_LABEL
        )).param("dim", embedding_dim as i64))
        .await
        .map_err(|e| StoreError::Backend(format!("neo4j meta query: {}", e)))?;
    let row = result
        .next()
        .await
        .map_err(|e| StoreError::Backend(format!("neo4j meta row: {}", e)))?
        .ok_or_else(|| StoreError::Backend("neo4j meta MERGE returned no rows".into()))?;
    let dim: i64 = row
        .get("dim")
        .map_err(|e| StoreError::Backend(format!("neo4j meta parse: {}", e)))?;
    Ok(dim as u32)
}

/// Pull a string property off a Bolt `Node`, defaulting to empty.
fn node_str(node: &Node, key: &str) -> String {
    node.get::<String>(key).unwrap_or_default()
}

/// Pull an integer property as `u32`, defaulting to 0.
fn node_u32(node: &Node, key: &str) -> u32 {
    node.get::<i64>(key).unwrap_or(0) as u32
}

fn node_i64(node: &Node, key: &str) -> i64 {
    node.get::<i64>(key).unwrap_or(0)
}

/// Pull the dense `embedding` array. Missing or empty → empty vector
/// (callers downstream tolerate this — e.g. MMR rerank skips when
/// vectors are empty).
fn node_embedding(node: &Node) -> Vec<f32> {
    // Bolt stores Float as f64; convert. If the property is missing,
    // return an empty vec.
    node.get::<Vec<f64>>("embedding")
        .map(|v| v.into_iter().map(|x| x as f32).collect())
        .unwrap_or_default()
}

/// Project a Neo4j `Node` (the cypher `RETURN n` shape) into the
/// project's `NodeRow` DTO.
fn node_to_row(node: &Node) -> NodeRow {
    NodeRow {
        id: node_str(node, "id"),
        name: node_str(node, "name"),
        node_type: node_str(node, "node_type"),
        description: node_str(node, "description"),
        file: node_str(node, "file"),
        start_line: node_u32(node, "start_line"),
        end_line: node_u32(node, "end_line"),
        last_update_at: node_i64(node, "last_update_at"),
        node_text: node_str(node, "node_text"),
        vector: node_embedding(node),
    }
}

#[async_trait]
impl KnowledgeStore for Neo4jStore {
    fn embedding_dim(&self) -> u32 {
        self.embedding_dim
    }

    fn supports_native_ppr(&self) -> bool {
        self.gds_available
    }

    fn backend_name(&self) -> &'static str {
        "neo4j"
    }

    async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }

        // Vector dim validation up front — match OverGraph's behavior so
        // a misconfigured embedder fails the same way on both backends.
        let want = self.embedding_dim as usize;
        for r in rows {
            if r.vector.len() != want {
                return Err(StoreError::BadVector {
                    id: r.id.clone(),
                    got: r.vector.len(),
                    want,
                });
            }
        }

        // Group by node_type so we can MERGE + apply the dynamic label
        // in one Cypher per type. (Cypher labels must be literal — APOC
        // would let us avoid this grouping, but we don't require it.)
        let mut by_type: HashMap<&str, Vec<&NodeRow>> = HashMap::new();
        for r in rows {
            by_type.entry(r.node_type.as_str()).or_default().push(r);
        }

        for (node_type, group) in by_type {
            let label_for_type = sanitize_label(node_type);
            // Build the parameter payload: a list of maps, one per row.
            let payload: Vec<BoltType> = group
                .iter()
                .map(|r| BoltType::Map(node_to_bolt_map(r)))
                .collect();

            let cypher = format!(
                "UNWIND $rows AS r \
                 MERGE (n:{base} {{id: r.id}}) \
                 SET n.name           = r.name, \
                     n.node_type      = r.node_type, \
                     n.description    = r.description, \
                     n.file           = r.file, \
                     n.start_line     = r.start_line, \
                     n.end_line       = r.end_line, \
                     n.last_update_at = r.last_update_at, \
                     n.node_text      = r.node_text, \
                     n.embedding      = r.embedding, \
                     n:`{label}`",
                base = NODE_LABEL,
                label = label_for_type
            );
            self.graph
                .run(query(&cypher).param("rows", payload))
                .await
                .map_err(|e| {
                    StoreError::Backend(format!("neo4j upsert_nodes ({}): {}", node_type, e))
                })?;
        }
        Ok(())
    }

    async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let weights = default_edge_type_weights();

        // Group by edge_type — relationship type must be literal in
        // Cypher (same constraint as labels above).
        let mut by_type: HashMap<&str, Vec<&EdgeRow>> = HashMap::new();
        for r in rows {
            by_type.entry(r.edge_type.as_str()).or_default().push(r);
        }

        for (edge_type, group) in by_type {
            let rel_type = sanitize_label(edge_type);
            let weight = weights
                .get(&edge_type.to_ascii_lowercase())
                .copied()
                .unwrap_or(0.5);

            let payload: Vec<BoltType> = group
                .iter()
                .map(|r| {
                    let mut m = BoltMap::new();
                    m.put(BoltString::from("src"), BoltType::from(r.source.as_str()));
                    m.put(BoltString::from("tgt"), BoltType::from(r.target.as_str()));
                    BoltType::Map(m)
                })
                .collect();

            let cypher = format!(
                "UNWIND $rows AS r \
                 MATCH (a:{base} {{id: r.src}}), (b:{base} {{id: r.tgt}}) \
                 MERGE (a)-[rel:`{rel}`]->(b) \
                 SET rel.weight = $weight",
                base = NODE_LABEL,
                rel = rel_type
            );
            self.graph
                .run(
                    query(&cypher)
                        .param("rows", payload)
                        .param("weight", weight as f64),
                )
                .await
                .map_err(|e| {
                    StoreError::Backend(format!("neo4j upsert_edges ({}): {}", edge_type, e))
                })?;
        }
        Ok(())
    }

    async fn vector_search(
        &self,
        q: Vec<f32>,
        k: usize,
        filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError> {
        // db.index.vector.queryNodes returns nodes ordered by score
        // descending; we pull k * over-fetch when a type filter is
        // active so the post-filter result still has ~k rows.
        let want = filter
            .and_then(|f| f.node_types.as_ref())
            .map(|_| (k * 4).max(20))
            .unwrap_or(k);
        let q_f64: Vec<f64> = q.into_iter().map(|x| x as f64).collect();
        let mut result = self
            .graph
            .execute(
                query(
                    "CALL db.index.vector.queryNodes($idx, $k, $vec) \
                     YIELD node, score \
                     RETURN node, score",
                )
                .param("idx", VECTOR_INDEX)
                .param("k", want as i64)
                .param("vec", q_f64),
            )
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j vector_search: {}", e)))?;

        let allowed: Option<HashSet<String>> = filter
            .and_then(|f| f.node_types.as_ref())
            .map(|v| v.iter().cloned().collect());

        let mut out: Vec<(NodeRow, f32)> = Vec::new();
        while let Some(row) = result
            .next()
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j vector_search row: {}", e)))?
        {
            let node: Node = row
                .get("node")
                .map_err(|e| StoreError::Backend(format!("neo4j parse node: {}", e)))?;
            let score: f64 = row.get("score").unwrap_or(0.0);
            let nr = node_to_row(&node);
            if let Some(a) = allowed.as_ref() {
                if !a.contains(&nr.node_type) {
                    continue;
                }
            }
            out.push((nr, score as f32));
            if out.len() >= k {
                break;
            }
        }
        Ok(out)
    }

    async fn hybrid_search(
        &self,
        q: Vec<f32>,
        _sparse: Vec<(u32, f32)>,
        query_text: &str,
        k: usize,
        filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError> {
        // Neo4j has no sparse-vector type. Hybrid is implemented as
        // vector + full-text fused with RRF in app code (the shared
        // `text::reciprocal_rank_fusion` helper used by the OverGraph
        // back-compat path too).
        let pool = (k * 4).max(20);
        let vector_hits = self.vector_search(q, pool, filter).await?;

        // Full-text branch. Empty queries → skip the second leg and
        // return the vector hits unchanged.
        let fts_hits = if query_text.trim().is_empty() {
            Vec::new()
        } else {
            let mut result = self
                .graph
                .execute(
                    query(
                        "CALL db.index.fulltext.queryNodes($idx, $q) \
                         YIELD node, score \
                         RETURN node, score LIMIT $k",
                    )
                    .param("idx", FTS_INDEX)
                    .param("q", escape_lucene(query_text))
                    .param("k", pool as i64),
                )
                .await
                .map_err(|e| StoreError::Backend(format!("neo4j fts_search: {}", e)))?;
            let allowed: Option<HashSet<String>> = filter
                .and_then(|f| f.node_types.as_ref())
                .map(|v| v.iter().cloned().collect());
            let mut hits: Vec<(NodeRow, f32)> = Vec::new();
            while let Some(row) = result
                .next()
                .await
                .map_err(|e| StoreError::Backend(format!("neo4j fts row: {}", e)))?
            {
                let node: Node = row
                    .get("node")
                    .map_err(|e| StoreError::Backend(format!("neo4j parse fts node: {}", e)))?;
                let score: f64 = row.get("score").unwrap_or(0.0);
                let nr = node_to_row(&node);
                if let Some(a) = allowed.as_ref() {
                    if !a.contains(&nr.node_type) {
                        continue;
                    }
                }
                hits.push((nr, score as f32));
            }
            hits
        };

        Ok(reciprocal_rank_fusion(vector_hits, fts_hits, k))
    }

    async fn traverse(
        &self,
        start: &str,
        max_hops: u32,
        edge_types: Option<&[String]>,
        direction: Direction,
    ) -> Result<TraversalPage, StoreError> {
        // BFS in app code — one Cypher per hop level. Simple and
        // correct; performance is fine for typical max_hops ≤ 3.
        let mut visited: HashMap<String, u32> = HashMap::new();
        let mut edges_set: HashSet<(String, String, String)> = HashSet::new();
        let mut edges_out: Vec<EdgeRow> = Vec::new();
        let mut nodes_out: Vec<TraversalNode> = Vec::new();

        let edge_types_owned: Vec<String> = edge_types.map(|v| v.to_vec()).unwrap_or_default();

        // Seed node row (depth 0). We need it even if the seed has no
        // outgoing edges so the caller's distance map is correct.
        if let Some(seed_row) = self.fetch_node(start).await? {
            visited.insert(start.to_string(), 0);
            nodes_out.push(TraversalNode {
                row: seed_row,
                depth: 0,
            });
        } else {
            return Ok(TraversalPage::default());
        }

        let mut frontier: Vec<String> = vec![start.to_string()];
        for depth in 1..=max_hops {
            if frontier.is_empty() {
                break;
            }
            let cypher = build_hop_cypher(direction);
            let mut result = self
                .graph
                .execute(
                    query(&cypher)
                        .param("frontier", frontier.clone())
                        .param("types", edge_types_owned.clone()),
                )
                .await
                .map_err(|e| StoreError::Backend(format!("neo4j traverse: {}", e)))?;
            let mut next_frontier: Vec<String> = Vec::new();
            while let Some(row) = result
                .next()
                .await
                .map_err(|e| StoreError::Backend(format!("neo4j traverse row: {}", e)))?
            {
                let neighbor_node: Node = match row.get("n") {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let rel_src: String = row.get("rel_src").unwrap_or_default();
                let rel_tgt: String = row.get("rel_tgt").unwrap_or_default();
                let edge_type: String = row.get("edge_type").unwrap_or_default();
                let nr = node_to_row(&neighbor_node);

                let key = (rel_src.clone(), edge_type.clone(), rel_tgt.clone());
                if edges_set.insert(key) {
                    edges_out.push(EdgeRow {
                        id: format!("{}|{}|{}", rel_src, edge_type, rel_tgt),
                        source: rel_src,
                        target: rel_tgt,
                        edge_type,
                        properties: String::new(),
                    });
                }

                if !visited.contains_key(&nr.id) {
                    visited.insert(nr.id.clone(), depth);
                    next_frontier.push(nr.id.clone());
                    nodes_out.push(TraversalNode {
                        row: nr,
                        depth,
                    });
                }
            }
            frontier = next_frontier;
        }
        Ok(TraversalPage {
            nodes: nodes_out,
            edges: edges_out,
        })
    }

    async fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<NodeRow>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_owned: Vec<String> = ids.to_vec();
        let mut result = self
            .graph
            .execute(
                query(&format!(
                    "UNWIND $ids AS id MATCH (n:{}) WHERE n.id = id RETURN n",
                    NODE_LABEL
                ))
                .param("ids", ids_owned),
            )
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j nodes_by_ids: {}", e)))?;
        let mut by_id: HashMap<String, NodeRow> = HashMap::new();
        while let Some(row) = result
            .next()
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j nodes_by_ids row: {}", e)))?
        {
            let node: Node = row
                .get("n")
                .map_err(|e| StoreError::Backend(format!("neo4j parse: {}", e)))?;
            let nr = node_to_row(&node);
            by_id.insert(nr.id.clone(), nr);
        }
        // Preserve caller's input order — matches OverGraph behavior so
        // ranked-context output stays stable across backends.
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    async fn fetch_node(&self, key: &str) -> Result<Option<NodeRow>, StoreError> {
        let mut result = self
            .graph
            .execute(
                query(&format!(
                    "MATCH (n:{}) WHERE n.id = $id RETURN n LIMIT 1",
                    NODE_LABEL
                ))
                .param("id", key),
            )
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j fetch_node: {}", e)))?;
        let row = result
            .next()
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j fetch_node row: {}", e)))?;
        match row {
            None => Ok(None),
            Some(r) => {
                let node: Node = r
                    .get("n")
                    .map_err(|e| StoreError::Backend(format!("neo4j parse: {}", e)))?;
                Ok(Some(node_to_row(&node)))
            }
        }
    }

    async fn count_nodes(&self) -> Result<usize, StoreError> {
        let mut result = self
            .graph
            .execute(query(&format!(
                "MATCH (n:{}) RETURN count(n) AS c",
                NODE_LABEL
            )))
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j count_nodes: {}", e)))?;
        let row = result
            .next()
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j count_nodes row: {}", e)))?
            .ok_or_else(|| StoreError::Backend("neo4j count_nodes: no row".into()))?;
        let c: i64 = row
            .get("c")
            .map_err(|e| StoreError::Backend(format!("neo4j parse count: {}", e)))?;
        Ok(c as usize)
    }

    async fn count_edges(&self) -> Result<usize, StoreError> {
        let mut result = self
            .graph
            .execute(query(&format!(
                "MATCH (:{label})-[r]->(:{label}) RETURN count(r) AS c",
                label = NODE_LABEL
            )))
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j count_edges: {}", e)))?;
        let row = result
            .next()
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j count_edges row: {}", e)))?
            .ok_or_else(|| StoreError::Backend("neo4j count_edges: no row".into()))?;
        let c: i64 = row
            .get("c")
            .map_err(|e| StoreError::Backend(format!("neo4j parse count: {}", e)))?;
        Ok(c as usize)
    }

    async fn personalized_pagerank(
        &self,
        seeds: &[String],
        direction: Direction,
        edge_types: Option<&[String]>,
        restart_prob: f32,
        max_iter: usize,
        max_results: Option<usize>,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        if !self.gds_available {
            // Caller should have checked `supports_native_ppr` first;
            // surface this clearly so `search_kb` can fall back to MMR.
            return Err(StoreError::Unsupported(
                "neo4j PPR requires the Graph Data Science plugin",
            ));
        }
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        // 1) Resolve project string ids → internal Neo4j node ids. GDS
        //    operates on the latter.
        let seeds_owned: Vec<String> = seeds.to_vec();
        let mut result = self
            .graph
            .execute(
                query(&format!(
                    "MATCH (n:{}) WHERE n.id IN $ids RETURN id(n) AS nid",
                    NODE_LABEL
                ))
                .param("ids", seeds_owned),
            )
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j ppr seed lookup: {}", e)))?;
        let mut seed_internal_ids: Vec<i64> = Vec::new();
        while let Some(row) = result
            .next()
            .await
            .map_err(|e| StoreError::Backend(format!("neo4j ppr seed row: {}", e)))?
        {
            let nid: i64 = row
                .get("nid")
                .map_err(|e| StoreError::Backend(format!("neo4j parse nid: {}", e)))?;
            seed_internal_ids.push(nid);
        }
        if seed_internal_ids.is_empty() {
            return Ok(Vec::new());
        }

        // 2) Project a one-shot named graph. We pick a unique name per
        //    call to avoid race conditions across concurrent searches;
        //    the projection is dropped at the end (or on the next open
        //    if the call panics — GDS projections are in-memory only).
        let projection_name = format!(
            "ug-ppr-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        // Edge type filter: if specified, project only those rels;
        // otherwise project all relationships from any UgNode → UgNode.
        let rel_proj = match edge_types.filter(|v| !v.is_empty()) {
            Some(types) => {
                let cleaned: Vec<String> =
                    types.iter().map(|s| sanitize_label(s.as_str())).collect();
                format!(
                    "[{}]",
                    cleaned
                        .iter()
                        .map(|t| format!("'{}'", t))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            None => "'*'".to_string(),
        };

        let project_cypher = format!(
            "CALL gds.graph.project($pname, '{label}', {{ types: {rel}, properties: 'weight' }}) \
             YIELD graphName RETURN graphName",
            label = NODE_LABEL,
            rel = rel_proj
        );
        if let Err(e) = self
            .graph
            .run(query(&project_cypher).param("pname", projection_name.as_str()))
            .await
        {
            return Err(StoreError::Backend(format!("neo4j gds project: {}", e)));
        }

        // 3) Run PPR. damping = 1 - restart_prob (project convention).
        let damping = (1.0 - restart_prob.clamp(0.01, 0.99) as f64).max(0.01);
        let limit = max_results.unwrap_or(100) as i64;
        let _ = direction; // GDS uses the projection's orientation
                           // (NATURAL by default); per-call direction
                           // filter would require a different
                           // projection. Documented as a known
                           // follow-up matching MIGRATION-OVERGRAPH §3.4.

        let ppr_cypher = "CALL gds.pageRank.stream($pname, { \
                            sourceNodes: $seeds, \
                            dampingFactor: $damp, \
                            maxIterations: $maxiter, \
                            relationshipWeightProperty: 'weight' \
                          }) YIELD nodeId, score \
                          RETURN gds.util.asNode(nodeId).id AS id, score \
                          ORDER BY score DESC \
                          LIMIT $limit";
        let ppr_run = self
            .graph
            .execute(
                query(ppr_cypher)
                    .param("pname", projection_name.as_str())
                    .param("seeds", seed_internal_ids.clone())
                    .param("damp", damping)
                    .param("maxiter", max_iter as i64)
                    .param("limit", limit),
            )
            .await;

        let mut out: Vec<(String, f32)> = Vec::new();
        match ppr_run {
            Ok(mut stream) => {
                while let Some(row) = stream
                    .next()
                    .await
                    .map_err(|e| StoreError::Backend(format!("neo4j ppr row: {}", e)))?
                {
                    let id: String = row.get("id").unwrap_or_default();
                    let score: f64 = row.get("score").unwrap_or(0.0);
                    if !id.is_empty() {
                        out.push((id, score as f32));
                    }
                }
            }
            Err(e) => {
                // Drop the projection even if the PPR call failed.
                let _ = self
                    .graph
                    .run(
                        query("CALL gds.graph.drop($pname, false) YIELD graphName RETURN graphName")
                            .param("pname", projection_name.as_str()),
                    )
                    .await;
                return Err(StoreError::Backend(format!("neo4j gds.pageRank.stream: {}", e)));
            }
        }

        // 4) Drop the projection.
        let _ = self
            .graph
            .run(
                query("CALL gds.graph.drop($pname, false) YIELD graphName RETURN graphName")
                    .param("pname", projection_name.as_str()),
            )
            .await;

        Ok(out)
    }
}

/// Cypher template for one BFS hop. Direction toggles `->`, `<-`, or
/// `-`; the empty `$types` list disables the type filter.
///
/// `rel_src`/`rel_tgt` always reflect the underlying edge direction
/// regardless of how we traversed it, so the project's wire-format
/// EdgeRows stay correct for `Both` traversals.
fn build_hop_cypher(direction: Direction) -> String {
    let arrow = match direction {
        Direction::Outbound => "-[r]->",
        Direction::Inbound => "<-[r]-",
        Direction::Both => "-[r]-",
    };
    format!(
        "UNWIND $frontier AS sid \
         MATCH (s:{label} {{id: sid}}){arrow}(n:{label}) \
         WHERE size($types) = 0 OR type(r) IN $types \
         RETURN n, \
                startNode(r).id AS rel_src, \
                endNode(r).id   AS rel_tgt, \
                type(r)         AS edge_type",
        label = NODE_LABEL,
        arrow = arrow
    )
}

/// Convert a project `NodeRow` into a Bolt parameter map ready for
/// `UNWIND $rows`. Vector is sent as `Vec<f64>` (Neo4j's native float
/// type) so the vector index accepts it without further coercion.
fn node_to_bolt_map(r: &NodeRow) -> BoltMap {
    let mut m = BoltMap::with_capacity(9);
    m.put(BoltString::from("id"), BoltType::from(r.id.as_str()));
    m.put(BoltString::from("name"), BoltType::from(r.name.as_str()));
    m.put(BoltString::from("node_type"), BoltType::from(r.node_type.as_str()));
    m.put(
        BoltString::from("description"),
        BoltType::from(r.description.as_str()),
    );
    m.put(BoltString::from("file"), BoltType::from(r.file.as_str()));
    m.put(BoltString::from("start_line"), BoltType::from(r.start_line as i64));
    m.put(BoltString::from("end_line"), BoltType::from(r.end_line as i64));
    m.put(
        BoltString::from("last_update_at"),
        BoltType::from(r.last_update_at),
    );
    m.put(
        BoltString::from("node_text"),
        BoltType::from(r.node_text.as_str()),
    );
    let vec_f64: Vec<f64> = r.vector.iter().map(|x| *x as f64).collect();
    m.put(BoltString::from("embedding"), BoltType::from(vec_f64));
    m
}

/// Strip backticks from label / relationship type names so they don't
/// break the backtick-quoting we apply at the Cypher layer. The
/// project's GraphNodeType / GraphEdgeType debug strings are clean
/// (`Function`, `Calls`, …) so this is defensive.
fn sanitize_label(s: &str) -> String {
    s.replace('`', "")
}

/// Escape Lucene special characters so distinctive identifiers in
/// queries don't blow up the full-text parser. We're intentionally
/// permissive — pass through letters/digits and a handful of
/// punctuation safe in a Lucene query string; escape the rest.
fn escape_lucene(s: &str) -> String {
    // Lucene reserved: + - && || ! ( ) { } [ ] ^ " ~ * ? : \ /
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '+' | '-' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '"' | '~' | '*' | '?'
            | ':' | '\\' | '/' | '&' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}
