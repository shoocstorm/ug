//! Personalized PageRank — thin wrapper over [`KnowledgeStore::personalized_pagerank`].
//!
//! Each backend implements PPR natively (OverGraph: built-in;
//! Neo4j: GDS plugin when available, else `Unsupported`). This module
//! is the back-compat surface kept for older callers; new code can call
//! the trait method directly.
//!
//! `default_edge_type_weights` lives here too because it is consumed at
//! ingest time (in `db::upsert_edges`) to bake structural bias into
//! per-edge weights — the OverGraph PPR API has no per-edge-type knob.
//! Neo4j's GDS PPR uses the same baked weights via
//! `relationshipWeightProperty: 'weight'`.

use crate::storage::store::{Direction, KnowledgeStore, StoreError};
use std::collections::HashMap;

/// Default edge-type weights used at ingest time so native PPR sees
/// the right structural bias. Higher = stronger semantic signal between
/// endpoints. Calls/Extends/Implements describe behavior; Imports/
/// Exports describe module boundaries; Contains is structural
/// scaffolding (file → symbol, folder → file). Lookups are
/// case-insensitive.
pub fn default_edge_type_weights() -> HashMap<String, f32> {
    let mut m = HashMap::new();
    m.insert("calls".to_string(), 1.0);
    m.insert("extends".to_string(), 0.9);
    m.insert("implements".to_string(), 0.9);
    m.insert("imports".to_string(), 0.7);
    m.insert("requires".to_string(), 0.7);
    m.insert("exports".to_string(), 0.6);
    m.insert("uses".to_string(), 0.6);
    m.insert("references".to_string(), 0.5);
    m.insert("dependson".to_string(), 0.4);
    m.insert("contains".to_string(), 0.3);
    m
}

/// Run native PPR via the trait. `seeds` are project string ids;
/// endpoints unknown to the backend are silently dropped. `restart_prob`
/// follows the project convention (0.15 default → damping factor 0.85).
///
/// Returns `(string-id, score)` pairs sorted by score descending.
pub async fn run_ppr(
    store: &dyn KnowledgeStore,
    seeds: &[String],
    direction: Direction,
    allowed_edge_types: Option<&[String]>,
    restart_prob: f32,
    max_iter: usize,
    max_results: Option<usize>,
) -> Result<Vec<(String, f32)>, StoreError> {
    store
        .personalized_pagerank(
            seeds,
            direction,
            allowed_edge_types,
            restart_prob,
            max_iter,
            max_results,
        )
        .await
}
