//! Personalized PageRank — thin wrapper around OverGraph's native
//! `personalized_pagerank`.
//!
//! The previous implementation was a 445-line in-process power
//! iteration over a bulk-loaded edges table. OverGraph builds PPR into
//! the engine, so this file shrinks to the parts the project still
//! owns: the default edge-type weight table (consumed at *ingest* time
//! to bake structural bias into edge weights) and a small async wrapper
//! that translates project options to OverGraph options.
//!
//! Two API mismatches are deliberate, see `docs/MIGRATION-OVERGRAPH.md`
//! §3.4:
//!   * **Uniform seeds.** OverGraph PPR takes `&[u64]` — no per-seed
//!     mass. v1 passes the top-N RRF hits unweighted; v1.1 will patch
//!     OverGraph to accept a personalization vector if quality regresses.
//!   * **Edge weights at ingest, not query.** OverGraph PPR has no
//!     per-edge-type weight knob, only an edge-type filter. We bake the
//!     per-type weight into `EdgeInput.weight` at upsert time
//!     (`db::upsert_edges`). The `default_edge_type_weights` table
//!     below is the source of truth for that.

use crate::storage::db::{Db, DbError};
use crate::storage::query::Direction;
use crate::storage::types_registry::edge_type_to_id;
use overgraph::{
    Direction as OgDirection, PprAlgorithm, PprOptions as OgPprOptions, PprResult as OgPprResult,
};
use std::collections::HashMap;

/// Default edge-type weights used at ingest time so OverGraph's native
/// PPR sees the right structural bias (see module doc for why).
/// Higher = stronger semantic signal between endpoints. Calls/Extends/
/// Implements describe behavior; Imports/Exports describe module
/// boundaries; Contains is structural scaffolding (file → symbol,
/// folder → file). Lookups are case-insensitive.
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

/// Run native PPR over the OverGraph edge graph.
///
/// `seeds` are project string ids. Endpoints unknown to the cache are
/// silently dropped. `restart_prob` follows the project convention (0.15
/// default → OverGraph `damping_factor = 0.85`). `allowed_edge_types`,
/// when non-empty, filters which typed edges PPR walks.
///
/// Returns `(string-id, score)` pairs sorted by score descending.
pub async fn run_ppr(
    db: &Db,
    seeds: &[String],
    direction: Direction,
    allowed_edge_types: Option<&[String]>,
    restart_prob: f32,
    max_iter: usize,
    max_results: Option<usize>,
) -> Result<Vec<(String, f32)>, DbError> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }

    let mut seed_ids: Vec<u64> = Vec::with_capacity(seeds.len());
    for s in seeds {
        if let Some(id) = db.lookup_id(s)? {
            seed_ids.push(id);
        }
    }
    if seed_ids.is_empty() {
        return Ok(Vec::new());
    }

    let damping = (1.0 - restart_prob.clamp(0.01, 0.99)) as f64;
    let _ = direction; // OverGraph PPR walks all edges; direction filter
                       // would require a fork (§3.4 v1.1).
    let edge_type_filter: Option<Vec<u32>> = allowed_edge_types
        .filter(|v| !v.is_empty())
        .map(|v| v.iter().map(|s| edge_type_to_id(s)).collect());

    let opts = OgPprOptions {
        algorithm: PprAlgorithm::ExactPowerIteration,
        damping_factor: damping,
        max_iterations: max_iter.max(1) as u32,
        epsilon: 1e-6,
        approx_residual_tolerance: 1e-5,
        edge_type_filter,
        max_results,
    };

    let result: OgPprResult = db.engine.personalized_pagerank(&seed_ids, &opts)?;

    let mut out: Vec<(String, f32)> = Vec::with_capacity(result.scores.len());
    for (id, score) in result.scores {
        out.push((db.key_for(id), score as f32));
    }
    Ok(out)
}

/// Translate project `Direction` to OverGraph's. Kept here so callers
/// in `query.rs` don't need to import `overgraph::Direction` directly.
pub fn to_og_direction(d: Direction) -> OgDirection {
    match d {
        Direction::Outbound => OgDirection::Outgoing,
        Direction::Inbound => OgDirection::Incoming,
        Direction::Both => OgDirection::Both,
    }
}
