//! Personalized PageRank over the LanceDB edges table.
//!
//! Replaces the old "seed -> BFS -> MMR" cascade in `search_kb` with a
//! single graph-aware ranking. The seed search (RRF over vector + FTS)
//! still picks entry points, but instead of using them as fixed BFS
//! roots, we feed them as a *personalization vector* to PPR. The
//! random-walker repeatedly restarts at the seeds and otherwise follows
//! weighted edges; the stationary distribution gives a score per node
//! that combines proximity-to-seeds with structural centrality.
//!
//! Why this beats BFS+MMR:
//!   * stage-1 errors no longer compound — a wrong seed only contributes
//!     a fraction of the personalization mass instead of anchoring all
//!     downstream expansion
//!   * multi-seed is native (no winner-takes-all)
//!   * edge types are *weighted* rather than gated, so a strong Calls
//!     edge can outweigh a chain of weak Contains edges
//!   * MMR's diversity hack is replaced by genuine relevance scoring
//!
//! Algorithm: standard power iteration on `r = α·v + (1-α)·M^T·r`,
//! where v is the (normalized) personalization vector, M is the
//! row-normalized weighted adjacency, and α is the restart probability
//! (`restart_prob`, default 0.15). Dangling-mass redistribution sends
//! the mass of out-degree-zero nodes back through v, which is the
//! standard correctness fix for arbitrary directed graphs.

use crate::storage::db::{all_edges, Db};
use crate::storage::query::Direction;
use std::collections::{HashMap, HashSet};

/// Default edge-type weights. Higher = stronger semantic signal between
/// the two endpoints. Calls/Extends/Implements describe behavior;
/// Imports/Exports describe module boundaries; Contains is structural
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

#[derive(Debug, Clone)]
pub struct PprOptions {
    /// Probability of teleporting back to the personalization vector at
    /// each random-walk step. Classical PageRank uses 0.15.
    pub restart_prob: f32,
    pub max_iter: usize,
    /// L1 convergence threshold on the rank vector. Stops early when
    /// `||r_new - r||_1 < tol`.
    pub tol: f32,
    pub edge_type_weights: HashMap<String, f32>,
    pub default_edge_weight: f32,
    /// Walk direction. `Both` is undirected (the typical retrieval
    /// case); `Outbound` / `Inbound` mirror the existing
    /// `traverse_filtered` semantics.
    pub direction: Direction,
    /// Optional case-insensitive whitelist of edge types. `None` = no
    /// filter (every edge participates with its weight).
    pub allowed_edge_types: Option<HashSet<String>>,
    /// Drop ranked nodes whose final score is below this fraction of
    /// the top score. Keeps the candidate pool tight without an
    /// arbitrary cutoff.
    pub min_score_ratio: f32,
}

impl Default for PprOptions {
    fn default() -> Self {
        Self {
            restart_prob: 0.15,
            // With walk_prob = 0.85 the contraction rate per iteration
            // is ~0.85, so reaching tol=1e-4 from r₀=v takes ~60 iters
            // in the worst case. 100 leaves comfortable headroom.
            max_iter: 100,
            tol: 1e-4,
            edge_type_weights: default_edge_type_weights(),
            default_edge_weight: 0.5,
            direction: Direction::Both,
            allowed_edge_types: None,
            min_score_ratio: 1e-3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PprResult {
    /// Node id -> final PageRank score, sorted descending by score.
    pub ranked: Vec<(String, f32)>,
    pub iterations: usize,
    pub converged: bool,
}

/// Run Personalized PageRank with `seeds` as the personalization mass.
/// `seeds` maps node id -> non-negative weight (e.g. RRF score). All
/// seeds whose ids appear in the loaded edges *or* are passed in are
/// included in the index; isolated seeds still receive their teleport
/// mass and stay in the ranking.
pub async fn personalized_pagerank(
    db: &Db,
    seeds: &HashMap<String, f32>,
    opts: &PprOptions,
) -> Result<PprResult, Box<dyn std::error::Error + Send + Sync>> {
    let edges = all_edges(db).await?;
    Ok(run_ppr_from_edges(&edges, seeds, opts))
}

/// Pure-compute core, separated so it can be unit-tested without a DB.
/// Builds an internal node index from `seeds.keys()` ∪ every endpoint
/// observed in `edges`, normalizes the personalization vector, then
/// power-iterates until convergence or `max_iter`.
pub fn run_ppr_from_edges(
    edges: &[crate::storage::db::EdgeRow],
    seeds: &HashMap<String, f32>,
    opts: &PprOptions,
) -> PprResult {
    // 1. Index every node id that participates: seeds + edge endpoints.
    let mut id_to_idx: HashMap<String, usize> = HashMap::new();
    let mut idx_to_id: Vec<String> = Vec::new();
    let intern = |id: &str, map: &mut HashMap<String, usize>, vec: &mut Vec<String>| {
        if let Some(&i) = map.get(id) {
            return i;
        }
        let i = vec.len();
        vec.push(id.to_string());
        map.insert(id.to_string(), i);
        i
    };
    for id in seeds.keys() {
        intern(id, &mut id_to_idx, &mut idx_to_id);
    }
    for e in edges {
        intern(&e.source, &mut id_to_idx, &mut idx_to_id);
        intern(&e.target, &mut id_to_idx, &mut idx_to_id);
    }
    let n = idx_to_id.len();
    if n == 0 {
        return PprResult {
            ranked: Vec::new(),
            iterations: 0,
            converged: true,
        };
    }

    // 2. Build out-edges per node with edge-type weights and direction.
    //    `out[i]` is a list of (j, w) for transitions i -> j. We
    //    normalize each row at the end so weights become a probability
    //    distribution.
    let mut out: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    let allow = opts.allowed_edge_types.as_ref();
    for e in edges {
        if let Some(allow) = allow {
            if !allow.contains(&e.edge_type.to_ascii_lowercase()) {
                continue;
            }
        }
        let w = opts
            .edge_type_weights
            .get(&e.edge_type.to_ascii_lowercase())
            .copied()
            .unwrap_or(opts.default_edge_weight);
        if w <= 0.0 {
            continue;
        }
        let s = match id_to_idx.get(&e.source) {
            Some(&v) => v,
            None => continue,
        };
        let t = match id_to_idx.get(&e.target) {
            Some(&v) => v,
            None => continue,
        };

        match opts.direction {
            Direction::Outbound => out[s].push((t, w)),
            Direction::Inbound => out[t].push((s, w)),
            Direction::Both => {
                out[s].push((t, w));
                out[t].push((s, w));
            }
        }
    }

    // Row-normalize: P[i -> j] = w_ij / sum_k w_ik.
    for row in out.iter_mut() {
        let total: f32 = row.iter().map(|&(_, w)| w).sum();
        if total > 0.0 {
            for entry in row.iter_mut() {
                entry.1 /= total;
            }
        }
    }

    // 3. Personalization vector v: normalized non-negative seed mass.
    //    If the caller passed all-zero or an empty map, fall back to
    //    uniform — that gives plain PageRank, which is still a useful
    //    centrality ranking.
    let mut v = vec![0.0f32; n];
    let mut total_seed: f32 = 0.0;
    for (id, &mass) in seeds.iter() {
        if mass <= 0.0 {
            continue;
        }
        if let Some(&i) = id_to_idx.get(id) {
            v[i] += mass;
            total_seed += mass;
        }
    }
    if total_seed > 0.0 {
        for x in v.iter_mut() {
            *x /= total_seed;
        }
    } else {
        let u = 1.0 / n as f32;
        for x in v.iter_mut() {
            *x = u;
        }
    }

    // 4. Power iteration.
    let alpha = opts.restart_prob.clamp(0.0, 1.0);
    let walk = 1.0 - alpha;
    let mut r = v.clone();
    let mut r_new = vec![0.0f32; n];
    let mut iters = 0usize;
    let mut converged = false;
    for _ in 0..opts.max_iter {
        iters += 1;
        // Dangling mass: rank held by nodes with zero total out-weight.
        let mut dangling: f32 = 0.0;
        for i in 0..n {
            if out[i].is_empty() {
                dangling += r[i];
            }
        }
        // Base = teleport + dangling redistribution. Both flow back
        // through v, which is the standard PageRank fix for arbitrary
        // directed graphs.
        for i in 0..n {
            r_new[i] = alpha * v[i] + walk * dangling * v[i];
        }
        // Walk contribution: each node's mass spreads along its row.
        for i in 0..n {
            let row = &out[i];
            if row.is_empty() {
                continue;
            }
            let push = walk * r[i];
            if push == 0.0 {
                continue;
            }
            for &(j, p) in row.iter() {
                r_new[j] += push * p;
            }
        }
        // L1 convergence check.
        let mut delta: f32 = 0.0;
        for i in 0..n {
            delta += (r_new[i] - r[i]).abs();
        }
        std::mem::swap(&mut r, &mut r_new);
        // Zero out the buffer for the next iteration.
        for x in r_new.iter_mut() {
            *x = 0.0;
        }
        if delta < opts.tol {
            converged = true;
            break;
        }
    }

    // 5. Sort, prune by min_score_ratio.
    let mut ranked: Vec<(String, f32)> = idx_to_id.into_iter().zip(r.into_iter()).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((_, top)) = ranked.first().cloned() {
        if top > 0.0 {
            let cutoff = top * opts.min_score_ratio.max(0.0);
            ranked.retain(|(_, s)| *s >= cutoff);
        }
    }

    PprResult {
        ranked,
        iterations: iters,
        converged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::EdgeRow;

    fn edge(id: &str, s: &str, t: &str, et: &str) -> EdgeRow {
        EdgeRow {
            id: id.to_string(),
            source: s.to_string(),
            target: t.to_string(),
            edge_type: et.to_string(),
            properties: String::new(),
        }
    }

    fn opts_default() -> PprOptions {
        PprOptions::default()
    }

    #[test]
    fn ppr_concentrates_mass_near_seed() {
        // Star graph: a -[Calls]- b, a -[Calls]- c, b -[Calls]- d.
        let edges = vec![
            edge("e1", "a", "b", "Calls"),
            edge("e2", "a", "c", "Calls"),
            edge("e3", "b", "d", "Calls"),
        ];
        let mut seeds = HashMap::new();
        seeds.insert("a".to_string(), 1.0);
        let r = run_ppr_from_edges(&edges, &seeds, &opts_default());
        // Seed itself should rank first; nodes 1 hop away should beat
        // nodes 2 hops away.
        assert_eq!(r.ranked[0].0, "a", "ranked={:?}", r.ranked);
        let score_of = |id: &str| -> f32 {
            r.ranked.iter().find(|(n, _)| n == id).map(|(_, s)| *s).unwrap_or(0.0)
        };
        assert!(score_of("b") > score_of("d"));
        assert!(score_of("c") > score_of("d"));
        assert!(r.converged, "PPR did not converge: iters={}", r.iterations);
    }

    #[test]
    fn ppr_respects_edge_type_weights() {
        // Two parallel paths from `a`: one via Calls (weight 1.0), one
        // via Contains (weight 0.3). The Calls neighbor must rank above
        // the Contains neighbor.
        let edges = vec![
            edge("e1", "a", "calls_n", "Calls"),
            edge("e2", "a", "contains_n", "Contains"),
        ];
        let mut seeds = HashMap::new();
        seeds.insert("a".to_string(), 1.0);
        let r = run_ppr_from_edges(&edges, &seeds, &opts_default());
        let score_of = |id: &str| -> f32 {
            r.ranked.iter().find(|(n, _)| n == id).map(|(_, s)| *s).unwrap_or(0.0)
        };
        assert!(
            score_of("calls_n") > score_of("contains_n"),
            "calls {} vs contains {}",
            score_of("calls_n"),
            score_of("contains_n")
        );
    }

    #[test]
    fn ppr_multi_seed_distributes_mass() {
        // Disconnected components anchored by separate seeds. Total
        // PPR mass per component matches the seed mass (PageRank is
        // mass-preserving), so within each component the seed
        // outranks its neighbor and the heavier component's neighbor
        // can outrank the lighter component's seed — that's a
        // *correct* property of personalization, not a bug.
        let edges = vec![
            edge("e1", "a", "x", "Calls"),
            edge("e2", "b", "y", "Calls"),
        ];
        let mut seeds = HashMap::new();
        seeds.insert("a".to_string(), 0.7);
        seeds.insert("b".to_string(), 0.3);
        let r = run_ppr_from_edges(&edges, &seeds, &opts_default());
        let score_of = |id: &str| -> f32 {
            r.ranked.iter().find(|(n, _)| n == id).map(|(_, s)| *s).unwrap_or(0.0)
        };
        // Heavier seed > lighter seed.
        assert!(score_of("a") > score_of("b"));
        // Heavier component dominates the lighter component's
        // neighbor (more total mass to spread).
        assert!(score_of("a") > score_of("y"));
        assert!(score_of("x") > score_of("y"));
        // Within each component, seed > neighbor.
        assert!(score_of("a") > score_of("x"));
        assert!(score_of("b") > score_of("y"));
    }

    #[test]
    fn ppr_respects_allowed_edge_types() {
        let edges = vec![
            edge("e1", "a", "via_calls", "Calls"),
            edge("e2", "a", "via_imports", "Imports"),
        ];
        let mut seeds = HashMap::new();
        seeds.insert("a".to_string(), 1.0);
        let mut opts = PprOptions::default();
        let mut allow = HashSet::new();
        allow.insert("calls".to_string());
        opts.allowed_edge_types = Some(allow);
        let r = run_ppr_from_edges(&edges, &seeds, &opts);
        let score_of = |id: &str| -> f32 {
            r.ranked.iter().find(|(n, _)| n == id).map(|(_, s)| *s).unwrap_or(0.0)
        };
        // Only Calls survives the filter, so the Imports neighbor is
        // disconnected and only receives teleport mass.
        assert!(score_of("via_calls") > score_of("via_imports"));
    }

    #[test]
    fn ppr_outbound_direction_is_one_way() {
        let edges = vec![edge("e1", "a", "b", "Calls")];
        let mut seeds = HashMap::new();
        seeds.insert("b".to_string(), 1.0);
        let mut opts = PprOptions::default();
        opts.direction = Direction::Outbound;
        let r = run_ppr_from_edges(&edges, &seeds, &opts);
        let score_of = |id: &str| -> f32 {
            r.ranked.iter().find(|(n, _)| n == id).map(|(_, s)| *s).unwrap_or(0.0)
        };
        // With outbound-only walks from seed `b`, the upstream `a`
        // should not receive walk mass — only teleport. So `b` >> `a`.
        assert!(score_of("b") > score_of("a") * 5.0);
    }

    #[test]
    fn ppr_returns_empty_for_empty_input() {
        let r = run_ppr_from_edges(&[], &HashMap::new(), &opts_default());
        assert!(r.ranked.is_empty());
        assert!(r.converged);
    }

    #[test]
    fn ppr_seed_only_isolated_node_still_ranked() {
        // Seed has no edges; it should still appear in the ranking
        // (with all the personalization mass) instead of being dropped.
        let mut seeds = HashMap::new();
        seeds.insert("lonely".to_string(), 1.0);
        let r = run_ppr_from_edges(&[], &seeds, &opts_default());
        assert_eq!(r.ranked.len(), 1);
        assert_eq!(r.ranked[0].0, "lonely");
        assert!(r.ranked[0].1 > 0.0);
    }
}
