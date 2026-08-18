//! High-level query API on top of [`super::db`].
//!
//! Exposes the operations from docs/GRAPH-STORAGE.md plus the Phase 4
//! GraphRAG composition: seed search -> expand -> rerank -> assemble.
//!
//!   1. semantic_search - vector-only nearest-neighbour
//!   2. semantic_search_w_where   - vector + SQL filter (e.g. `node_type = 'Function'`)
//!   3. traverse        - BFS over the edges table from a seed node id
//!   4. rerank          - Maximal Marginal Relevance (MMR) to balance diversity and
//!     relevance
//!   5. assemble        - GraphRAG query composition: seeds → expand → rerank → final ranked list
//!   6. code_snippet    - retrieval helper that returns code snippets from nodes

use crate::storage::db::{EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use crate::storage::ppr::run_ppr;
use crate::storage::store::{KnowledgeStore, NodeFilter, QueryLimits, QueryParams, QueryValue};
use crate::storage::text::build_sparse_query_vector;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Re-exported for back-compat — `Direction` now lives in `store.rs`.
pub use crate::storage::store::Direction;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub node: NodeRow,
    pub distance: f32,
}

#[derive(Debug, Default)]
pub struct TraversalResult {
    pub nodes: Vec<NodeRow>,
    pub edges: Vec<EdgeRow>,
    pub distances: HashMap<String, u32>,
}

/// Vector search by free-text query. Embeds `analyze` once with `embedder`,
/// then asks the backend for the top-`k` nearest node rows. Works
/// against any [`KnowledgeStore`].
pub async fn semantic_search(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let raw = store.vector_search(query_vec, k, None).await?;
    Ok(raw
        .into_iter()
        .map(|(node, distance)| SearchHit { node, distance })
        .collect())
}

/// Name-substring search that needs no embedder at all.
///
/// This is the fallback when `semantic_search` / `search` are asked to
/// run without one: it matches `name CONTAINS query` over the stored
/// graph, so a repo can still be searched the day the embedding backend
/// is down or unconfigured. Rows come back ordered by id so the answer
/// is stable between runs; `distance` carries the 1-based rank so callers
/// that sort on it get the same ordering as the rows themselves.
pub async fn name_search(
    store: &dyn KnowledgeStore,
    query: &str,
    k: usize,
    where_clause: Option<&str>,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let filter = where_clause.and_then(NodeFilter::from_legacy_where);
    let mut params = QueryParams::new();
    params.insert("q".into(), QueryValue::Str(query.to_string()));
    let type_pred = match &filter {
        Some(f) => f
            .node_types
            .as_ref()
            .map(|ts| {
                if ts.len() == 1 {
                    format!("AND n.node_type = '{}'", ts[0].replace('\'', "''"))
                } else {
                    let list = ts
                        .iter()
                        .map(|t| format!("'{}'", t.replace('\'', "''")))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("AND n.node_type IN ({list})")
                }
            })
            .unwrap_or_default(),
        None => String::new(),
    };
    let gql = format!(
        "MATCH (n) WHERE n.name CONTAINS $q {type_pred} \
         RETURN elementKey(n) AS id ORDER BY id LIMIT {k}"
    );
    let limits = QueryLimits {
        max_rows: k,
        ..Default::default()
    };
    let page = store.execute_query(&gql, &params, &limits).await?;
    let ids: Vec<String> = page
        .rows
        .into_iter()
        .filter_map(|row| match row.first() {
            Some(QueryValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let rows = store.nodes_by_ids(&ids).await?;
    let mut by_id: std::collections::HashMap<String, NodeRow> =
        rows.into_iter().map(|r| (r.id.clone(), r)).collect();
    let mut hits = Vec::new();
    for (rank, id) in ids.iter().enumerate() {
        if let Some(node) = by_id.remove(id) {
            hits.push(SearchHit {
                node,
                distance: (rank + 1) as f32,
            });
        }
    }
    Ok(hits)
}

/// Like [`semantic_search`] but with a `node_type` filter parsed from
/// the legacy SQL-flavored `WHERE` argument. Anything the parser
/// doesn't recognize degrades to no filter (matches pre-trait
/// OverGraph behavior — see `MIGRATION-OVERGRAPH §6 Q1`).
pub async fn semantic_search_w_where(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    query: &str,
    k: usize,
    where_clause: &str,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let filter = NodeFilter::from_legacy_where(where_clause);
    let raw = store.vector_search(query_vec, k, filter.as_ref()).await?;
    Ok(raw
        .into_iter()
        .map(|(node, distance)| SearchHit { node, distance })
        .collect())
}

/// Fused dense + sparse search that also returns the set of ids the **dense** channel
/// surfaced on its own, so each seed can be labelled `"semantic"` (in the
/// dense results) vs `"keyword"` (only in the fused/sparse side). The query
/// is embedded once and reused for both the fused hybrid search and the
/// dense-only search — no second embedding pass.
async fn seeds_and_dense_ids(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    query: &str,
    k: usize,
    where_clause: Option<&str>,
) -> Result<(Vec<SearchHit>, HashSet<String>), Box<dyn std::error::Error + Send + Sync>> {
    let vectors = embedder.embed(&[query.to_string()]).await?;
    let query_vec = vectors
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let sparse = build_sparse_query_vector(query, store.sparse_stats().as_deref());
    let pool = (k * 4).max(20);
    let filter = where_clause.and_then(NodeFilter::from_legacy_where);
    let fused = store
        .hybrid_search(query_vec.clone(), sparse, query, pool, filter.as_ref())
        .await?;
    let seeds: Vec<SearchHit> = fused
        .into_iter()
        .take(k)
        .map(|(node, score)| SearchHit {
            node,
            distance: -score,
        })
        .collect();
    // Dense-only top-pool: ids the semantic channel reached without the
    // sparse/keyword side. Membership here is the provenance signal.
    let dense = store.vector_search(query_vec, pool, filter.as_ref()).await?;
    let dense_ids: HashSet<String> = dense.into_iter().map(|(n, _)| n.id).collect();
    Ok((seeds, dense_ids))
}

/// Maximal Marginal Relevance rerank. `lambda` in [0, 1] balances
/// relevance (vs. query) against diversity (vs. already-picked items).
/// Uses the stored row vectors so no extra embedding calls are needed.
///
/// Output is **bit-identical** to the straightforward form this replaced.
/// Everything below is caching, not different arithmetic:
///
/// * relevance against the query is loop-invariant, and was recomputed for
///   every candidate on every one of the `k` rounds;
/// * `cosine` recomputed both vectors' norms on every call, so a candidate's
///   own norm was recomputed `k · picked` times — the norms are the same sums
///   either way, summed in the same order, so caching them changes nothing;
/// * the diversity term rescanned all of `picked` per candidate per round,
///   which is O(k) work to add one new maximum. A running maximum, extended
///   by the newly picked item, visits the same similarities and takes the
///   same max — `max` over finite floats does not care about order.
///
/// Together that turns O(k²·n·d) into O(k·n·d).
pub fn mmr_rerank(
    query_vec: &[f32],
    candidates: Vec<SearchHit>,
    k: usize,
    lambda: f32,
) -> Vec<SearchHit> {
    if candidates.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut remaining: Vec<SearchHit> = candidates;
    let mut picked: Vec<SearchHit> = Vec::new();

    let lambda = lambda.clamp(0.0, 1.0);
    let query_norm = sum_squares(query_vec);

    // All three run parallel to `remaining` and are `swap_remove`d with it,
    // so index `i` means the same candidate in every one of them.
    let mut norms: Vec<f32> = remaining.iter().map(|c| sum_squares(&c.node.vector)).collect();
    let mut rel: Vec<f32> = remaining
        .iter()
        .zip(&norms)
        .map(|(c, &n)| cosine_pre(&c.node.vector, query_vec, n, query_norm))
        .collect();
    // Highest similarity to anything already picked. `f32::MIN` is the
    // "nothing picked yet" sentinel the original used.
    let mut max_sim: Vec<f32> = vec![f32::MIN; remaining.len()];

    while picked.len() < k && !remaining.is_empty() {
        let mut best_idx: usize = 0;
        let mut best_score: f32 = f32::MIN;

        for i in 0..remaining.len() {
            let div = if max_sim[i] == f32::MIN { 0.0 } else { max_sim[i] };
            let score = lambda * rel[i] - (1.0 - lambda) * div;
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        let chosen = remaining.swap_remove(best_idx);
        let chosen_norm = norms.swap_remove(best_idx);
        rel.swap_remove(best_idx);
        max_sim.swap_remove(best_idx);

        // Fold the new pick into every remaining candidate's running maximum.
        for i in 0..remaining.len() {
            let sim = cosine_pre(
                &remaining[i].node.vector,
                &chosen.node.vector,
                norms[i],
                chosen_norm,
            );
            if sim > max_sim[i] {
                max_sim[i] = sim;
            }
        }

        picked.push(chosen);
    }

    picked
}

/// `Σ vᵢ²` — the squared L2 norm, accumulated in the same forward order the
/// original `cosine` used so the cached value is the identical float.
fn sum_squares(v: &[f32]) -> f32 {
    let mut n = 0.0f32;
    for x in v {
        n += x * x;
    }
    n
}

/// Cosine similarity given both vectors' precomputed squared norms.
/// Guard conditions and arithmetic match [`cosine`] exactly.
fn cosine_pre(a: &[f32], b: &[f32], na: f32, nb: f32) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// BFS up to `max_hops` from `start_id` over the `edges` table.
/// Outbound-only, no edge-type filter. Kept for backwards compatibility.
pub async fn traverse(
    store: &dyn KnowledgeStore,
    start_id: &str,
    max_hops: u32,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    traverse_filtered(
        store,
        &[start_id.to_string()],
        max_hops,
        None,
        Direction::Outbound,
    )
    .await
}

/// Generalised graph expansion. Walks `direction` for up to `max_hops`
/// from each id in `start_ids`, optionally restricted to specific edge
/// types. Calls the trait's `traverse` per seed and merges results,
/// deduplicating shared neighbours.
pub async fn traverse_filtered(
    store: &dyn KnowledgeStore,
    start_ids: &[String],
    max_hops: u32,
    edge_types: Option<&[String]>,
    direction: Direction,
) -> Result<TraversalResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut nodes: Vec<NodeRow> = Vec::new();
    let mut edges: Vec<EdgeRow> = Vec::new();

    for start_id in start_ids {
        let page = store
            .traverse(start_id, max_hops, edge_types, direction)
            .await?;

        for tn in page.nodes {
            // Track depth even if we've already added the node — closer wins.
            distances
                .entry(tn.row.id.clone())
                .and_modify(|cur| {
                    if tn.depth < *cur {
                        *cur = tn.depth;
                    }
                })
                .or_insert(tn.depth);
            if visited.insert(tn.row.id.clone()) {
                nodes.push(tn.row);
            }
        }
        edges.extend(page.edges);
    }

    Ok(TraversalResult {
        nodes,
        edges,
        distances,
    })
}

/// Final ranked context returned from [`search_kb`]. Each item represents a
/// node selected for the agent prompt, with the actual code slice attached
/// when the node has line ranges and the file is readable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextItem {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub description: String,
    pub distance: f32,
    pub hop: u32,
    pub snippet: Option<String>,
    /// How this node was reached by the hybrid search: `"semantic"` (dense
    /// vector seed), `"keyword"` (sparse/FTS seed — in the fused seed set but
    /// not the dense-only results), or `"graph"` (walked to via PPR/BFS, i.e.
    /// `hop >= 1`). A seed present in both channels is labelled `"semantic"`:
    /// the dense match is the stronger signal and the one worth surfacing.
    #[serde(default)]
    pub matched_by: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RankedContext {
    pub query: String,
    pub items: Vec<ContextItem>,
    pub total_chars: usize,
    pub seed_id: Option<String>,
}

/// Read `start_line..=end_line` from `file` (1-indexed line numbers, both
/// inclusive). Returns `None` for missing files, unreadable files, or zero
/// line ranges. `repo_root` is prepended when the path is relative.
/// Source for a row, preferring what ingest captured over the working tree.
///
/// The stored copy is what the row's description and embedding were built
/// from, so serving it keeps everything an agent sees internally
/// consistent. It also removes the silent-corruption case: a line range
/// that has drifted still *resolves* against the file on disk, handing back
/// the wrong lines with no error. Falls back to reading the file for rows
/// written before the column existed, or whose capture failed.
pub fn snippet_for(row: &NodeRow, repo_root: &Path) -> Option<String> {
    if !row.code.is_empty() {
        return Some(row.code.clone());
    }
    read_snippet(repo_root, &row.file, row.start_line, row.end_line)
}

/// File cache for one `search_kb` call's snippet extraction.
///
/// [`read_snippet`] reads and line-scans a whole file to pull out one
/// symbol's span, so a search returning twenty hits drawn from five files
/// read those five files twenty times — and symbols cluster by file, so hits
/// sharing one are the normal case rather than the unlucky one.
///
/// Deliberately **per call, not global**: a long-lived cache would keep
/// serving a snippet from before the user's last edit, and answering from a
/// stale working tree is the one failure `ug` exists to avoid. Living for the
/// length of one request buys the dedup without ever outliving the read.
#[derive(Default)]
pub struct SnippetCache {
    files: HashMap<PathBuf, Option<String>>,
}

impl SnippetCache {
    /// As [`snippet_for`], reusing any file already read during this call.
    pub fn snippet_for(&mut self, row: &NodeRow, repo_root: &Path) -> Option<String> {
        if !row.code.is_empty() {
            return Some(row.code.clone());
        }
        if row.file.is_empty()
            || row.start_line == 0
            || row.end_line == 0
            || row.end_line < row.start_line
        {
            return None;
        }
        let abs: PathBuf = if Path::new(&row.file).is_absolute() {
            PathBuf::from(&row.file)
        } else {
            repo_root.join(&row.file)
        };
        // A file that could not be read caches its failure too, so an
        // unreadable path is not retried once per hit that mentions it.
        let entry = self
            .files
            .entry(abs)
            .or_insert_with_key(|p| std::fs::read_to_string(p).ok());
        slice_lines(entry.as_deref()?, row.start_line, row.end_line)
    }
}

pub fn read_snippet(
    repo_root: &Path,
    file: &str,
    start_line: u32,
    end_line: u32,
) -> Option<String> {
    if file.is_empty() || start_line == 0 || end_line == 0 || end_line < start_line {
        return None;
    }
    let abs: PathBuf = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        repo_root.join(file)
    };
    let content = std::fs::read_to_string(&abs).ok()?;
    slice_lines(&content, start_line, end_line)
}

/// Lines `start_line..=end_line` (1-based, inclusive) of `content`.
fn slice_lines(content: &str, start_line: u32, end_line: u32) -> Option<String> {
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as u32;
        if n < start_line {
            continue;
        }
        if n > end_line {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Default total snippet budget for one `search_kb` call, in chars.
///
/// Retrieval stops adding results once the assembled context passes this,
/// so it is the cap a user actually feels as "search returned fewer hits
/// than `k`". Also used as the default context budget for chat/tour
/// prompt assembly. Unlike the index-time caps it costs nothing to change
/// — callers override it per call, and no re-index is involved.
pub const DEFAULT_CONTEXT_CHARS: usize = 60_000;

/// Ranking strategy for the candidate pool produced by seed search +
/// graph context.
///
/// PPR is the only strategy the public surfaces expose — the MCP tool, the
/// CLI help and the HTTP docs no longer offer a choice. `Mmr` survives
/// because [`search_kb`] selects it automatically for backends without
/// native PPR (Neo4j without the GDS plugin); it is a fallback, not a user
/// option. The `--strategy` flag still parses for operator debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankStrategy {
    Ppr,
    Mmr,
}

impl RankStrategy {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "mmr" => RankStrategy::Mmr,
            _ => RankStrategy::Ppr,
        }
    }
}

/// Configuration for [`search_kb`]. Keeping this as a struct keeps the
/// NAPI signature small (one JSON blob) and allows future fields without
/// breaking callers.
#[derive(Debug, Clone)]
pub struct SearchKbOptions<'a> {
    pub query: &'a str,
    pub k: usize,
    pub hops: u32,
    pub edge_types: Option<&'a [String]>,
    pub direction: Direction,
    pub max_chars: usize,
    pub mmr_lambda: f32,
    pub repo_root: &'a Path,
    pub where_clause: Option<&'a str>,
    pub include_snippets: bool,
    /// Whether results may include nodes the query did not match
    /// directly. `false` stops after the seed search: no PPR, no BFS,
    /// no neighbors — the lookup the old `semantic_search` command was.
    /// Cheaper, and the right shape when you want candidates to pick
    /// from rather than a context bundle to read.
    pub expand: bool,
    pub strategy: RankStrategy,
    /// PPR teleport probability. Higher = stay closer to seeds; lower =
    /// let structural centrality dominate. Ignored unless
    /// `strategy == Ppr`.
    pub ppr_restart_prob: f32,
    /// PPR power-iteration cap. Ignored unless `strategy == Ppr`.
    pub ppr_max_iter: usize,
    /// Override the default edge-type weight table (id is
    /// case-insensitive). `None` = use defaults from
    /// [`crate::storage::ppr::default_edge_type_weights`].
    pub ppr_edge_weights: Option<HashMap<String, f32>>,
    /// Number of seeds passed into the PPR personalization vector. We
    /// use a wider seed pool than `k` so a single noisy hit doesn't
    /// dominate the random walker. Ignored unless `strategy == Ppr`.
    pub ppr_seed_pool: usize,
}

impl<'a> SearchKbOptions<'a> {
    pub fn new(query: &'a str, repo_root: &'a Path) -> Self {
        Self {
            query,
            k: 8,
            hops: 2,
            edge_types: None,
            direction: Direction::Both,
            max_chars: DEFAULT_CONTEXT_CHARS,
            mmr_lambda: 0.6,
            repo_root,
            where_clause: None,
            include_snippets: false,
            expand: true,
            strategy: RankStrategy::Ppr,
            ppr_restart_prob: 0.15,
            ppr_max_iter: 30,
            ppr_edge_weights: None,
            ppr_seed_pool: 16,
        }
    }
}

/// [Advanced RAG Search] Phase 4 GraphRAG: seed search -> PPR ranking ->
/// snippet attachment -> token-budgeted assembly. Returns a JSON-friendly
/// [`RankedContext`].
///
/// Backends without native PPR (Neo4j without GDS) silently fall back to
/// MMR with a single warning log line — callers don't need to opt in, and
/// that fallback is the only reason [`RankStrategy::Mmr`] still exists.
///
/// `opts.expand == false` short-circuits to [`search_kb_flat`]: seeds
/// only, no ranking stage at all.
pub async fn search_kb(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    if !opts.expand {
        return search_kb_flat(store, embedder, opts).await;
    }
    let strategy = match opts.strategy {
        RankStrategy::Ppr if !store.supports_native_ppr() => {
            tracing::warn!(
                backend = store.backend_name(),
                "PPR strategy requested but backend lacks native PPR; falling back to MMR"
            );
            RankStrategy::Mmr
        }
        s => s,
    };
    match strategy {
        RankStrategy::Ppr => search_kb_ppr(store, embedder, opts).await,
        RankStrategy::Mmr => search_kb_mmr(store, embedder, opts).await,
    }
}

/// Default path: RRF seeds become a personalization vector for
/// Personalized PageRank over the full edge graph. PPR scores replace
/// both BFS expansion and MMR reranking — a single ranking that fuses
/// seed proximity with graph-wide centrality.
async fn search_kb_ppr(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    // 1. RRF seeds. Wider pool than `k` so a noisy top-1 doesn't
    //    dominate the personalization vector. `dense_ids` lets each seed
    //    be labelled semantic vs keyword (see [`ContextItem::matched_by`]).
    let seed_pool = opts.ppr_seed_pool.max(opts.k.max(1));
    let (seeds, dense_ids) =
        seeds_and_dense_ids(store, embedder, opts.query, seed_pool, opts.where_clause).await?;
    let seed_id = seeds.first().map(|h| h.node.id.clone());

    // 2. Personalization vector: RRF score (stored as -score in
    //    `distance`) translated back to positive mass. Negate, floor
    //    at zero, fall back to rank-decayed weights when distances are
    //    zero (e.g. FTS-only path).
    let mut seed_mass: HashMap<String, f32> = HashMap::new();
    let mut any_positive = false;
    for h in seeds.iter() {
        let mass = (-h.distance).max(0.0);
        if mass > 0.0 {
            any_positive = true;
        }
        seed_mass.entry(h.node.id.clone()).or_insert(mass);
    }
    if !any_positive {
        for (rank, h) in seeds.iter().enumerate() {
            seed_mass.insert(h.node.id.clone(), 1.0 / (rank as f32 + 1.0));
        }
    }

    // 3. PPR — uniform seeds (see MIGRATION-OVERGRAPH §3.4 for the
    //    weighted-personalization deferral). `seed_mass` keys are still
    //    used downstream as the "is this a seed?" set.
    let take = (opts.k * 4).max(opts.k.max(1));
    let seed_strings: Vec<String> = seed_mass.keys().cloned().collect();
    let edge_types_owned: Option<Vec<String>> =
        opts.edge_types.map(|v| v.iter().cloned().collect());
    let ranked_pairs = run_ppr(
        store,
        &seed_strings,
        opts.direction,
        edge_types_owned.as_deref(),
        opts.ppr_restart_prob,
        opts.ppr_max_iter,
        Some(take),
    )
    .await?;

    // 4. Hydrate the top-N node rows. Take a generous slice so the
    //    char budget stage has room to discard sparse/empty entries.
    let top_ids: Vec<String> = ranked_pairs
        .iter()
        .take(take)
        .map(|(id, _)| id.clone())
        .collect();
    let score_by_id: HashMap<String, f32> = ranked_pairs.into_iter().collect();
    let nodes = store.nodes_by_ids(&top_ids).await?;
    let nodes_by_id: HashMap<String, NodeRow> =
        nodes.into_iter().map(|n| (n.id.clone(), n)).collect();

    let mut items: Vec<ContextItem> = Vec::new();
    let mut total_chars: usize = 0;
    let mut snippets = SnippetCache::default();
    for id in top_ids.iter() {
        let Some(n) = nodes_by_id.get(id) else {
            continue;
        };
        let score = score_by_id.get(id).copied().unwrap_or(0.0);
        let snippet = if opts.include_snippets {
            snippets.snippet_for(n, opts.repo_root)
        } else {
            None
        };
        // `hop` field kept for backwards compatibility: 0 if seed,
        // else 1 (PPR has no hop concept; seed/non-seed is the most
        // useful signal we can preserve here).
        let is_seed = seed_mass.contains_key(id);
        let hop: u32 = if is_seed { 0 } else { 1 };
        let matched_by = if !is_seed {
            "graph"
        } else if dense_ids.contains(id) {
            "semantic"
        } else {
            "keyword"
        };
        let item = ContextItem {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: n.node_type.clone(),
            file: n.file.clone(),
            start_line: n.start_line,
            end_line: n.end_line,
            description: n.description.clone(),
            // Surface PPR score as a negated "distance" so existing
            // downstream consumers (sort-ascending) keep working.
            distance: -score,
            hop,
            snippet,
            matched_by: matched_by.to_string(),
        };
        let item_chars = item.snippet.as_ref().map(|s| s.len()).unwrap_or(0)
            + item.description.len()
            + item.name.len();
        if total_chars + item_chars > opts.max_chars && !items.is_empty() {
            break;
        }
        total_chars += item_chars;
        items.push(item);
        if items.len() >= opts.k {
            break;
        }
    }

    Ok(RankedContext {
        query: opts.query.to_string(),
        items,
        total_chars,
        seed_id,
    })
}

/// Seeds only: RRF fusion, then stop. No PPR, no BFS, no neighbors —
/// every item came back because the query matched it, so `hop` is always
/// 0 and `matched_by` is only ever `semantic` or `keyword`.
///
/// This is what `expand: false` selects, and what the standalone
/// `semantic_search` command used to be. One difference worth knowing:
/// seeds here are RRF-fused (vector + full-text), where the old command
/// was vector-only. For code that is usually better — an exact identifier
/// arrives through the keyword channel — and `matched_by` still says which
/// channel found each hit.
async fn search_kb_flat(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    let (seeds, dense_ids) =
        seeds_and_dense_ids(store, embedder, opts.query, opts.k.max(1), opts.where_clause).await?;
    let seed_id = seeds.first().map(|h| h.node.id.clone());

    let mut items: Vec<ContextItem> = Vec::new();
    let mut total_chars: usize = 0;
    let mut snippets = SnippetCache::default();
    for h in seeds.into_iter() {
        let n = h.node;
        let snippet = if opts.include_snippets {
            snippets.snippet_for(&n, opts.repo_root)
        } else {
            None
        };
        let matched_by = if dense_ids.contains(&n.id) {
            "semantic"
        } else {
            "keyword"
        };
        let item = ContextItem {
            id: n.id,
            name: n.name,
            node_type: n.node_type,
            file: n.file,
            start_line: n.start_line,
            end_line: n.end_line,
            description: n.description,
            distance: h.distance,
            hop: 0,
            snippet,
            matched_by: matched_by.to_string(),
        };
        let item_chars = item.snippet.as_ref().map(|s| s.len()).unwrap_or(0)
            + item.description.len()
            + item.name.len();
        if total_chars + item_chars > opts.max_chars && !items.is_empty() {
            break;
        }
        total_chars += item_chars;
        items.push(item);
        if items.len() >= opts.k {
            break;
        }
    }

    Ok(RankedContext {
        query: opts.query.to_string(),
        items,
        total_chars,
        seed_id,
    })
}

/// Legacy path: seed -> BFS expand -> MMR rerank. Kept available via
/// `RankStrategy::Mmr` for callers who want diversity-first behavior.
async fn search_kb_mmr(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    opts: SearchKbOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    // 1. Seed: RRF over vector + FTS, optionally filtered. `dense_ids`
    //    carries the semantic-channel membership used to label each seed.
    let (seeds, dense_ids) =
        seeds_and_dense_ids(store, embedder, opts.query, opts.k.max(1), opts.where_clause).await?;
    let seed_id = seeds.first().map(|h| h.node.id.clone());

    // 2. Expand: walk the graph from each seed.
    let seed_ids: Vec<String> = seeds.iter().map(|h| h.node.id.clone()).collect();
    let traversal =
        traverse_filtered(store, &seed_ids, opts.hops, opts.edge_types, opts.direction).await?;

    // 3. Build candidate pool: union of seed hits + traversal nodes.
    let seed_dist: HashMap<String, f32> = seeds
        .iter()
        .map(|h| (h.node.id.clone(), h.distance))
        .collect();

    let mut candidates: Vec<SearchHit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for h in seeds.iter() {
        if seen.insert(h.node.id.clone()) {
            candidates.push(h.clone());
        }
    }
    for n in traversal.nodes.iter() {
        if seen.insert(n.id.clone()) {
            let hop = traversal.distances.get(&n.id).copied().unwrap_or(opts.hops);
            let dist = seed_dist
                .get(&n.id)
                .copied()
                .unwrap_or((hop as f32 * 0.1).max(0.05));
            candidates.push(SearchHit {
                node: n.clone(),
                distance: dist,
            });
        }
    }

    // 4. Rerank with MMR using the original query embedding for relevance.
    let query_vec = embedder
        .embed(&[opts.query.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or("embedder returned no vectors")?;
    let take = candidates.len().min(opts.k * 4).max(opts.k);
    let reranked = mmr_rerank(&query_vec, candidates, take, opts.mmr_lambda);

    // 5. Attach snippets and apply char budget.
    let mut items: Vec<ContextItem> = Vec::new();
    let mut total_chars: usize = 0;
    let mut snippets = SnippetCache::default();
    for hit in reranked {
        let is_seed = seed_dist.contains_key(&hit.node.id);
        let hop = traversal.distances.get(&hit.node.id).copied().unwrap_or(0);
        let snippet = if opts.include_snippets {
            snippets.snippet_for(&hit.node, opts.repo_root)
        } else {
            None
        };
        let matched_by = if !is_seed {
            "graph"
        } else if dense_ids.contains(&hit.node.id) {
            "semantic"
        } else {
            "keyword"
        };

        let item = ContextItem {
            id: hit.node.id.clone(),
            name: hit.node.name.clone(),
            node_type: hit.node.node_type.clone(),
            file: hit.node.file.clone(),
            start_line: hit.node.start_line,
            end_line: hit.node.end_line,
            description: hit.node.description.clone(),
            distance: hit.distance,
            hop,
            snippet,
            matched_by: matched_by.to_string(),
        };

        let item_chars = item.snippet.as_ref().map(|s| s.len()).unwrap_or(0)
            + item.description.len()
            + item.name.len();
        if total_chars + item_chars > opts.max_chars && !items.is_empty() {
            break;
        }
        total_chars += item_chars;
        items.push(item);
        if items.len() >= opts.k {
            break;
        }
    }

    Ok(RankedContext {
        query: opts.query.to_string(),
        items,
        total_chars,
        seed_id,
    })
}
