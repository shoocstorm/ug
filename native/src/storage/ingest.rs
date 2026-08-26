//! Drive the embedding + write pipeline for a complete graph.
//!
//! Given an in-memory [`GraphData`], this builds the per-node embedding
//! text, embeds it, and upserts both the nodes and edges into one or
//! more backends. Edges have no vector column so no embedding work is
//! needed for them.
//!
//! # Incremental re-ingest
//!
//! Ingest is idempotent — re-running it over a mostly-unchanged repo is
//! the common case (`ug gen` on an existing project, the KB Manager's
//! re-index button). Embedding every node again on each of those runs is
//! by far the most expensive thing the pipeline does, and almost all of
//! it is redundant.
//!
//! So before embedding anything, [`plan_incremental_ingest`] diffs the
//! incoming graph against what the store already holds and sorts every
//! node into one of three buckets:
//!
//! - **unchanged** — the stored row is identical: no embed, no write.
//! - **reusable** — the node's embedding text is unchanged but some
//!   other column moved (line numbers are the usual one, since they
//!   don't feed the embedding text). Carry the stored vector over and
//!   write the row: no embed.
//! - **to embed** — new node, or its text changed. Full cost.
//!
//! Only the third bucket reaches the embedder.

use crate::storage::db::{EdgeRow, NodeRow};
use crate::storage::embed::Embedder;
use crate::storage::facts::FactContext;
use crate::storage::store::{KnowledgeStore, NodeKey, StoreError};
use crate::storage::source::{capture_graph_code, CapturedCode};
use crate::limits::EmbedBudget;
use crate::storage::sparse_stats::SparseStats;
use crate::storage::text::collect_related_names;
use crate::types::{GraphData, GraphNode};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// How many nodes to read back from the store at a time while planning.
/// Caps peak memory: the fetched rows carry their dense vectors, so an
/// unbounded read of a large graph would hold the entire vector set in
/// memory just to throw most of it away.
const PLAN_FETCH_CHUNK: usize = 2048;

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub nodes_written: usize,
    pub edges_written: usize,
    pub embedding_calls: usize,
    /// Nodes that actually went through the embedder this run.
    pub nodes_embedded: usize,
    /// Nodes whose stored row was already identical — neither embedded
    /// nor written.
    pub nodes_unchanged: usize,
    /// Stale rows removed because the incoming graph no longer has them.
    pub nodes_pruned: usize,
    /// Set when the embedder failed and nodes were written without
    /// vectors. The index is usable for structure and statistics but not
    /// for semantic search until the next successful ingest backfills
    /// them — callers must surface this rather than reporting success.
    pub embedding_error: Option<String>,
}

/// The work a re-ingest actually has to do, after diffing against the store.
#[derive(Debug, Default)]
pub struct IngestPlan {
    /// Rows to write whose vector was carried over from the store.
    pub reusable: Vec<NodeRow>,
    /// Nodes needing a fresh vector: index into `graph.nodes`, plus the
    /// embedding text already built for it.
    pub to_embed: Vec<(usize, String)>,
    /// Nodes identical to what's stored, so neither embedded nor written.
    pub unchanged: usize,
    /// Of the nodes this plan is *not* embedding, how many have no vector in
    /// the store.
    ///
    /// "Did this run skip embedding anything" and "does the index have
    /// vectors" stopped being the same question once a vector-less row could
    /// be recognised as unchanged (R4.2): a re-index over an unembedded store
    /// now skips every node, so counting only what *this* run declined to
    /// embed reports zero and the caller concludes the index is ready. It is
    /// not. This is the count that stays true across re-runs.
    pub vectorless_kept: usize,
}

impl IngestPlan {
    /// Fold freshly computed vectors — one per entry of [`to_embed`], in
    /// order — into the rows the plan already resolved, yielding the full
    /// set to upsert.
    ///
    /// Split out from the embedding step so callers that drive the
    /// embedder themselves (e.g. `ug gen`, which renders per-batch
    /// progress) can still share the row assembly.
    ///
    /// [`to_embed`]: IngestPlan::to_embed
    pub fn finish(
        self,
        graph: &GraphData,
        vectors: Vec<Vec<f32>>,
        captured: &HashMap<String, CapturedCode>,
    ) -> Result<Vec<NodeRow>, String> {
        Ok(self.rows(graph, vectors, captured)?.collect())
    }

    /// The same rows, produced one at a time.
    ///
    /// `NodeRow` carries `code` — the symbol's whole body — and `node_text`,
    /// both of which already exist elsewhere in memory, so materialising a row
    /// per node before the first batch reaches the store doubles the largest
    /// thing the ingest holds. The writer only ever needs `write_batch` of
    /// them at once. See P11.4 in docs/dev/PERF-TUNING-JOURNEY.md.
    ///
    /// `finish` remains for callers that genuinely want the whole vector.
    pub fn rows<'a>(
        self,
        graph: &'a GraphData,
        vectors: Vec<Vec<f32>>,
        captured: &'a HashMap<String, CapturedCode>,
    ) -> Result<RowStream<'a>, String> {
        if vectors.len() != self.to_embed.len() {
            return Err(format!(
                "embedder returned {} vectors for {} nodes",
                vectors.len(),
                self.to_embed.len()
            ));
        }
        Ok(RowStream {
            graph,
            captured,
            facts_ctx: FactContext::new(graph),
            now: current_unix_secs(),
            remaining: self.reusable.len() + self.to_embed.len(),
            reusable: self.reusable.into_iter(),
            to_embed: self.to_embed.into_iter().zip(vectors),
        })
    }
}

/// The node rows an [`IngestPlan`] resolves to, yielded lazily — see
/// [`IngestPlan::rows`].
///
/// Order is the reusable rows first, then the freshly embedded ones, which is
/// exactly what [`IngestPlan::finish`] produced.
pub struct RowStream<'a> {
    graph: &'a GraphData,
    captured: &'a HashMap<String, CapturedCode>,
    facts_ctx: FactContext,
    now: i64,
    remaining: usize,
    reusable: std::vec::IntoIter<NodeRow>,
    to_embed: std::iter::Zip<std::vec::IntoIter<(usize, String)>, std::vec::IntoIter<Vec<f32>>>,
}

impl RowStream<'_> {
    /// How many rows are still to come. Known up front, so a caller can render
    /// a progress meter without draining the stream to count it.
    pub fn len(&self) -> usize {
        self.remaining
    }

    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}

impl Iterator for RowStream<'_> {
    type Item = NodeRow;

    fn next(&mut self) -> Option<NodeRow> {
        if let Some(row) = self.reusable.next() {
            self.remaining -= 1;
            return Some(row);
        }
        let ((idx, node_text), vector) = self.to_embed.next()?;
        self.remaining -= 1;
        let n = &self.graph.nodes[idx];
        Some(node_row(
            n,
            n.node_type.as_str().to_string(),
            node_text,
            vector,
            self.now,
            self.captured.get(&n.id),
            &self.facts_ctx,
        ))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

/// Whether the run being planned is going to produce vectors.
///
/// This decides what a stored row with **no** vector means. Embedding is
/// opt-in (`--with-embed`), so the default `ug gen` writes every row with an
/// empty vector on purpose — and a later run that also is not embedding should
/// read that as "no vector wanted", not as "this row is out of date". Getting
/// it wrong in that direction re-wrote every node on every run; getting it
/// wrong in the other would leave nodes permanently without vectors, which is
/// worse, because `search` and `chat` would silently miss them.
///
/// A named type rather than a `bool` because it would sit next to
/// `always_write` in the argument list, and two adjacent booleans that mean
/// unrelated things are a transposition waiting to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorPlan {
    /// An embedder is present; every node in `to_embed` will get a vector.
    Embedding,
    /// No embedder. Rows keep whatever vector they already have — for most
    /// of them, none.
    Skipping,
}

/// Whether `prev`'s stored vector is one this run can carry forward.
///
/// Two ways to qualify: it is the width this store expects (a real vector,
/// still valid because the caller has already established the text matches),
/// or there is no vector and this run would not have produced one either.
/// A vector of some *other* width is a legacy row from before a dim change
/// and has to be redone.
fn vector_is_reusable(prev: &NodeRow, dim: usize, vectors: VectorPlan) -> bool {
    if prev.vector.len() == dim {
        return true;
    }
    prev.vector.is_empty() && vectors == VectorPlan::Skipping
}

/// Diff `graph` (with its pre-built `texts`) against `store`.
///
/// `always_write` forces every node into `reusable`/`to_embed` even when
/// the stored row is identical, leaving `unchanged` at 0. Multi-destination
/// ingest needs this: the plan is built against one store, and skipping
/// writes on that basis would let the *other* destinations drift.
///
/// `vectors` says whether this run will embed — see [`VectorPlan`], and R4.2
/// in docs/dev/PERF-TUNING-JOURNEY.md for what it cost to have it missing.
///
/// A store that errors on read is not fatal — it just means we cannot
/// prove anything is reusable, so the caller falls back to embedding
/// everything, exactly as it did before this optimization existed.
pub async fn plan_incremental_ingest(
    store: &dyn KnowledgeStore,
    graph: &GraphData,
    texts: &[String],
    always_write: bool,
    captured: &HashMap<String, CapturedCode>,
    model: Option<&str>,
    vectors: VectorPlan,
) -> Result<IngestPlan, StoreError> {
    let mut plan = IngestPlan {
        reusable: Vec::new(),
        to_embed: Vec::new(),
        unchanged: 0,
        vectorless_kept: 0,
    };
    let dim = store.embedding_dim() as usize;
    // A stored vector is only reusable if it came out of the same model.
    // The dim check below is not sufficient: bge-small and all-MiniLM-L6
    // are both 384-wide, so a swap between them leaves the text identical
    // and would carry vectors from the old embedding space forward.
    // Unrecorded (`None`) means an older store or a backend that can't
    // track it — reuse stays allowed there, as it always was.
    let reuse_vectors = match (store.ingest_model(), model) {
        (Some(stored), Some(current)) => stored == current,
        _ => true,
    };
    if !reuse_vectors {
        tracing::info!(
            stored = ?store.ingest_model(),
            current = ?model,
            "embedding model changed since last ingest; re-embedding every node"
        );
    }
    let now = current_unix_secs();
    let facts_ctx = FactContext::new(graph);
    let mut offset = 0usize;

    for chunk in graph.nodes.chunks(PLAN_FETCH_CHUNK) {
        let keys: Vec<NodeKey> = chunk
            .iter()
            .map(|n| NodeKey {
                id: n.id.clone(),
                node_type: n.node_type.as_str().to_string(),
            })
            .collect();
        let mut stored: HashMap<String, NodeRow> = store
            .nodes_for_upsert(&keys)
            .await?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();

        for (i, n) in chunk.iter().enumerate() {
            let idx = offset + i;
            let text = &texts[idx];
            let node_type = &keys[i].node_type;

            // A stored row is only useful if its embedding text still
            // matches (the vector is then still valid for it) and its
            // vector is one this run can carry forward — see
            // [`vector_is_reusable`].
            match stored.remove(&n.id) {
                Some(prev)
                    if reuse_vectors
                        && prev.node_text == *text
                        && vector_is_reusable(&prev, dim, vectors) =>
                {
                    if prev.vector.is_empty() {
                        plan.vectorless_kept += 1;
                    }
                    let cap = captured.get(&n.id);
                    if !always_write && stored_row_matches(&prev, n, node_type, cap, &facts_ctx) {
                        plan.unchanged += 1;
                    } else {
                        plan.reusable.push(node_row(
                            n,
                            node_type.clone(),
                            text.clone(),
                            prev.vector,
                            now,
                            cap,
                            &facts_ctx,
                        ));
                    }
                }
                _ => plan.to_embed.push((idx, text.clone())),
            }
        }
        offset += chunk.len();
    }

    Ok(plan)
}

/// Whether a stored row already carries exactly what we would write for
/// `n`, making the upsert a no-op.
///
/// `last_update_at` is deliberately excluded — it is pure bookkeeping,
/// and nothing reads it for correctness. Leaving it alone actually makes
/// it truthful: it now marks when the node last *changed* rather than
/// when ingest last ran. `vector` is excluded because the caller has
/// already established that `node_text` matches, which is what the
/// vector is derived from.
fn stored_row_matches(
    prev: &NodeRow,
    n: &GraphNode,
    node_type: &str,
    captured: Option<&CapturedCode>,
    facts_ctx: &FactContext,
) -> bool {
    // Facts are compared because several of them are not properties of
    // this node at all: `in_degree` moves when some *other* file starts or
    // stops calling it. Skipping the rewrite on a content match would
    // freeze those at whatever they were on first ingest, and every
    // statistic derived from them would drift a little further from the
    // truth on each incremental run — silently, since the node itself
    // looks perfectly up to date.
    if prev.facts != crate::storage::facts::compute(n, facts_ctx) {
        return false;
    }
    // Body edits move `code` without touching `node_text`, so this is what
    // routes an edited function into the reusable bucket — rewritten, not
    // re-embedded. Compared only when capture succeeded; a failed read
    // must not make every node look changed on every run.
    if let Some(c) = captured {
        if prev.code != c.code || prev.file_hash != c.file_hash {
            return false;
        }
    }
    prev.name == n.name
        && prev.node_type == node_type
        && prev.description == n.docstring.as_deref().unwrap_or("")
        && prev.file == n.file.as_deref().unwrap_or("")
        && prev.start_line == n.start_line.unwrap_or(0)
        && prev.end_line == n.end_line.unwrap_or(0)
}

/// Build the row for a single node. `node_type` is passed in because
/// callers have already rendered it once per node, and `now` because a
/// clock read per row across a large graph is pure overhead.
fn node_row(
    n: &GraphNode,
    node_type: String,
    node_text: String,
    vector: Vec<f32>,
    now: i64,
    captured: Option<&CapturedCode>,
    facts_ctx: &FactContext,
) -> NodeRow {
    NodeRow {
        id: n.id.clone(),
        name: n.name.clone(),
        node_type,
        description: n.docstring.clone().unwrap_or_default(),
        file: n.file.clone().unwrap_or_default(),
        start_line: n.start_line.unwrap_or(0),
        end_line: n.end_line.unwrap_or(0),
        last_update_at: now,
        node_text,
        vector,
        code: captured.map(|c| c.code.clone()).unwrap_or_default(),
        file_hash: captured.map(|c| c.file_hash.clone()).unwrap_or_default(),
        facts: crate::storage::facts::compute(n, facts_ctx),
    }
}

/// Run a plan's `to_embed` bucket through the embedder and fold the
/// results into the rows the plan already resolved. Returns the complete
/// set of rows to upsert plus how many nodes were embedded.
/// Returns the rows to write, how many were embedded, and — when the
/// embedder failed — why.
///
/// A failed embed is *not* fatal. Everything except the vectors is already
/// computed by this point: names, line ranges, source text, and every
/// derived fact. Throwing that away because one HTTP call failed leaves
/// the user with no index at all, when what they could have had is an
/// index that answers structural and statistical questions and is missing
/// only semantic search. So the rows are written with empty vectors, the
/// caller reports the failure, and the next ingest — which sees a vector
/// of the wrong width — backfills them.
async fn rows_from_plan(
    plan: IngestPlan,
    embedder: &Embedder,
    graph: &GraphData,
    captured: &HashMap<String, CapturedCode>,
) -> Result<(Vec<NodeRow>, usize, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    let embedded = plan.to_embed.len();
    if embedded == 0 {
        return Ok((plan.finish(graph, Vec::new(), captured)?, 0, None));
    }
    let texts: Vec<String> = plan.to_embed.iter().map(|(_, t)| t.clone()).collect();
    match embedder.embed(&texts).await {
        Ok(vectors) => Ok((plan.finish(graph, vectors, captured)?, embedded, None)),
        Err(e) => {
            let why = e.to_string();
            tracing::warn!(
                error = %why,
                nodes = embedded,
                "embedding failed; writing nodes without vectors so the \
                 structural index still lands"
            );
            let empty = vec![Vec::new(); embedded];
            Ok((plan.finish(graph, empty, captured)?, 0, Some(why)))
        }
    }
}

/// Repo root recorded on the graph's index stats, if any. Capture needs
/// it to resolve the relative paths nodes carry.
fn repo_root_of(graph: &GraphData) -> Option<std::path::PathBuf> {
    let root = &graph.stats.as_ref()?.repo_root;
    if root.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(root))
}

/// blake3 of the edge set, for [`KnowledgeStore::edges_digest`].
///
/// Order-dependent, deliberately. The graph builder emits edges in a
/// deterministic order (P11.10), so a stable set hashes stably — and if that
/// ever stops being true, a reordering shows up as "changed" and costs a
/// rewrite. That is the safe direction: a false *unchanged* would leave the
/// store disagreeing with `graph.json`, which is a failure mode nothing else
/// in this pipeline has.
///
/// The node count is folded in as well, so a run that pruned nodes cannot
/// match a digest taken before the prune.
pub fn edges_digest(graph: &GraphData) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&(graph.nodes.len() as u64).to_le_bytes());
    h.update(&(graph.edges.len() as u64).to_le_bytes());
    for e in &graph.edges {
        h.update(e.source.as_bytes());
        h.update(b"\x00");
        h.update(e.edge_type.as_str().as_bytes());
        h.update(b"\x00");
        h.update(e.target.as_bytes());
        h.update(b"\x00");
    }
    h.finalize().to_hex().to_string()
}

/// The complete id set of `graph`, for [`prune_nodes_absent_from`].
///
/// [`prune_nodes_absent_from`]: KnowledgeStore::prune_nodes_absent_from
/// Borrowed, not cloned: the callee only reads this set, and cloning every
/// id to build it copied ~23 MB of strings `graph.nodes` already owns. See
/// P11.9 in docs/dev/PERF-TUNING-JOURNEY.md.
pub fn graph_id_set(graph: &GraphData) -> HashSet<&str> {
    graph.nodes.iter().map(|n| n.id.as_str()).collect()
}

/// Drop store rows that the incoming graph no longer contains.
///
/// Refuses to run on an empty graph: ingest is an upsert of a *complete*
/// index, so an empty node list means indexing produced nothing (a bad
/// path, a failed parse) rather than "the repo is now empty" — and pruning
/// against it would erase the whole store.
pub async fn prune_to_graph(
    store: &dyn KnowledgeStore,
    graph: &GraphData,
) -> Result<usize, StoreError> {
    if graph.nodes.is_empty() {
        return Ok(0);
    }
    store.prune_nodes_absent_from(&graph_id_set(graph)).await
}

/// Capture the graph's source, resolving the repo root from its index
/// stats. Empty when no root is recorded (a hand-built or legacy graph),
/// in which case every code read falls back to the filesystem.
pub fn capture_for_graph(graph: &GraphData) -> HashMap<String, CapturedCode> {
    match repo_root_of(graph) {
        Some(root) => capture_graph_code(graph, &root),
        None => HashMap::new(),
    }
}

/// Recompute the corpus statistics behind BM25 keyword weighting and
/// install them on every destination.
///
/// Must run *before* the upsert: the stored sparse vectors are truncated to
/// [`MAX_SPARSE_DIMS`] by `saturated_tf × idf`, so the stats decide which
/// terms survive in the rows written by this same run.
///
/// Computed over the whole graph rather than the changed subset — document
/// frequency is a corpus-wide quantity, and deriving it from a delta would
/// make every incremental run disagree with the last. The work is one
/// tokenizer pass over text already in memory.
///
/// [`MAX_SPARSE_DIMS`]: crate::storage::text::MAX_SPARSE_DIMS
pub fn refresh_sparse_stats(
    stores: &[&dyn KnowledgeStore],
    texts: &[String],
    captured: &HashMap<String, CapturedCode>,
    graph: &GraphData,
) -> Arc<SparseStats> {
    // Parallel: this is the most expensive phase of an ingest — 7.2 s of a
    // 29.6 s run on a large repo — and it is a `map` over independent items
    // in a crate where the indexer that feeds it is already `par_iter`. The
    // `None` below is the proof the iterations cannot interact: the pass is
    // forbidden from reading the statistics it is building, so no iteration
    // can observe another's work. `collect` into a `Vec` keeps node order,
    // which `SparseStats::from_documents` depends on.
    // See R4.1 in docs/dev/PERF-TUNING-JOURNEY.md.
    let docs: Vec<Vec<u32>> = graph
        .nodes
        .par_iter()
        .zip(texts.par_iter())
        .map(|(n, text)| {
            let code = captured.get(&n.id).map(|c| c.code.as_str()).unwrap_or("");
            // `None` here on purpose: this pass exists to *produce* the
            // stats, so it must not consult them.
            crate::storage::text::build_node_sparse_vector(text, code, None)
                .into_iter()
                .map(|(dim, _)| dim)
                .collect()
        })
        .collect();

    let stats = Arc::new(SparseStats::from_documents(docs.iter().map(|d| d.as_slice())));
    for store in stores {
        store.set_sparse_stats(stats.clone());
    }
    stats
}

/// The embedding budget implied by an embedder's model, with no explicit
/// override.
///
/// The lib-side entry points take no `--section-cap`; the binary resolves
/// that from flags/config and calls [`build_texts`] directly with its own
/// [`EmbedBudget`].
pub fn budget_for(embedder: &Embedder) -> EmbedBudget {
    EmbedBudget::resolve(&embedder.config().model, None)
}

/// Extensions whose bodies are prose, not code.
///
/// The scanner in [`crate::storage::comments`] keys off `//`, `/* */` and
/// `#`, which in markdown means every heading line, and its string-literal
/// tracking trips on ordinary apostrophes and backticks — so what it
/// returns for a document is mangled prose, not a comment. These files
/// carry their description in the node's docstring already (see
/// `indexer::languages::markdown::section_prose`), so there is nothing here
/// to recover.
const PROSE_EXTS: &[&str] = &["md", "mdx", "markdown"];

fn is_prose_file(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) => PROSE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Build the per-node embedding texts for a graph, in `graph.nodes` order.
///
/// Takes the captured source so each node's own comments can be folded in
/// — for most nodes that is the only prose attached to them — and the
/// resolved [`EmbedBudget`], which decides how much of each description
/// fits the loaded model's window.
pub fn build_texts(
    graph: &GraphData,
    captured: &HashMap<String, CapturedCode>,
    budget: &EmbedBudget,
) -> Vec<String> {
    let related = collect_related_names(graph);
    // Shared across the graph so a licence header or file banner is
    // indexed once rather than on every node of the file.
    let mut seen_banner = HashSet::new();
    graph
        .nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            // Only nodes with a real line range get comments. A File node's
            // "span" is the entire file, so letting it participate would
            // hand it every comment in the file and — through the banner
            // dedup — starve the symbols underneath it.
            let comments = match (n.start_line, n.end_line, captured.get(&n.id)) {
                (Some(_), Some(_), Some(c)) if !is_prose_file(n.file.as_deref().unwrap_or("")) => {
                    crate::storage::comments::extract_prose_comments(&c.code, &mut seen_banner)
                }
                _ => String::new(),
            };
            crate::storage::text::build_node_text_with_comments(n, names, &comments, budget)
        })
        .collect()
}

/// Single-destination ingest. Embeds only the nodes that changed since
/// the last run (see the module docs), then upserts nodes + edges.
pub async fn ingest_graph(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<IngestStats, Box<dyn std::error::Error + Send + Sync>> {
    let captured = capture_for_graph(graph);
    let budget = budget_for(embedder);
    let model = embedder.config().model.clone();
    let texts = build_texts(graph, &captured, &budget);
    refresh_sparse_stats(&[store], &texts, &captured, graph);
    // This entry point takes an `Embedder` by value, so there is always one.
    let plan = plan_incremental_ingest(
        store,
        graph,
        &texts,
        false,
        &captured,
        Some(&model),
        VectorPlan::Embedding,
    )
    .await?;
    let unchanged = plan.unchanged;
    let (node_rows, embedded, embedding_error) =
        rows_from_plan(plan, embedder, graph, &captured).await?;
    let edge_rows = build_edge_rows(graph);

    store.upsert_nodes(&node_rows).await?;
    store.upsert_edges(&edge_rows).await?;
    let pruned = prune_to_graph(store, graph).await?;
    store.ensure_query_indexes();
    // Only stamp the model when the vectors actually came from it. Stamping
    // after a failed embed would tell the next run "these vectors are
    // current for this model" about rows that have no vectors at all.
    if embedding_error.is_none() {
        store.record_ingest_model(&model);
    }

    Ok(IngestStats {
        nodes_written: node_rows.len(),
        edges_written: edge_rows.len(),
        embedding_calls: usize::from(embedded > 0),
        nodes_embedded: embedded,
        nodes_unchanged: unchanged,
        nodes_pruned: pruned,
        embedding_error,
    })
}

fn build_edge_rows(graph: &GraphData) -> Vec<EdgeRow> {
    graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = e.edge_type.as_str().to_string();
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            EdgeRow {
                id,
                source: e.source.to_string(),
                target: e.target.to_string(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect()
}

// Helper kept for backwards compat; allows ingest_graph to skip the
// upsert_edges call when the caller prefers to run them separately.
#[allow(dead_code)]
pub(crate) async fn upsert_only_nodes(
    store: &dyn KnowledgeStore,
    rows: &[NodeRow],
) -> Result<(), StoreError> {
    store.upsert_nodes(rows).await
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::store::{
        Direction, NodeFilter, TraversalPage,
    };
    use crate::types::{GraphEdge, GraphEdgeType, GraphNodeType};
    use std::sync::Mutex;

    // ---- R4.1: the parallel stats pass must equal the serial one ---------

    /// `refresh_sparse_stats` was made parallel because it is the most
    /// expensive phase of an ingest (7.2 s of 29.6 s on a large repo). The
    /// guarantee it has to keep is exact equality with the serial pass, and
    /// that cannot be checked by re-running `ug gen` and comparing: the term
    /// count is *already* unstable run to run (R4.10). So both are computed
    /// here, over the same inputs, in one process.
    #[test]
    fn the_parallel_stats_pass_equals_a_serial_one() {
        let g = crate::types::GraphData {
            nodes: (0..64)
                .map(|i| {
                    let mut n = node(&format!("function:f{i}"));
                    n.name = format!("handler_{i}");
                    n.docstring = Some(format!(
                        "handles request number {i} for the shared parser and writes a record"
                    ));
                    n
                })
                .collect(),
            edges: Vec::new(),
            stats: None,
            resolution: None,
        };
        let captured = HashMap::new();
        let budget = EmbedBudget::resolve("bge-small-en-v1.5", None);
        let texts = build_texts(&g, &captured, &budget);

        // The serial form this replaced, spelled out.
        let serial_docs: Vec<Vec<u32>> = g
            .nodes
            .iter()
            .zip(&texts)
            .map(|(n, text)| {
                let code = captured.get(&n.id).map(|c| c.code.as_str()).unwrap_or("");
                crate::storage::text::build_node_sparse_vector(text, code, None)
                    .into_iter()
                    .map(|(dim, _)| dim)
                    .collect()
            })
            .collect();
        let expected = SparseStats::from_documents(serial_docs.iter().map(|d| d.as_slice()));

        let store = RecordingStore::default();
        let got = refresh_sparse_stats(&[&store as &dyn KnowledgeStore], &texts, &captured, &g);

        assert_eq!(got.total_docs, expected.total_docs);
        assert_eq!(got.terms(), expected.terms(), "same number of distinct terms");
        assert!(got.terms() > 0, "the fixture must produce terms at all");
        // Every dimension the serial pass saw, with the same document
        // frequency. `terms()` matching means there are no extras.
        for doc in &serial_docs {
            for &dim in doc {
                assert_eq!(
                    got.doc_freq(dim),
                    expected.doc_freq(dim),
                    "document frequency for dim {dim}"
                );
            }
        }
    }

    // ---- P11.6: the edge-set digest -------------------------------------

    fn graph_of(edges: Vec<GraphEdge>) -> GraphData {
        GraphData {
            nodes: vec![node("function:a"), node("function:b"), node("function:c")],
            edges,
            stats: None,
            resolution: None,
        }
    }

    #[test]
    fn the_same_edge_set_digests_the_same() {
        let g = graph_of(vec![
            GraphEdge::new("function:a", "function:b", GraphEdgeType::Calls),
            GraphEdge::new("function:b", "function:c", GraphEdgeType::Calls),
        ]);
        assert_eq!(edges_digest(&g), edges_digest(&g));
    }

    #[test]
    fn a_removed_edge_changes_the_digest() {
        let full = graph_of(vec![
            GraphEdge::new("function:a", "function:b", GraphEdgeType::Calls),
            GraphEdge::new("function:b", "function:c", GraphEdgeType::Calls),
        ]);
        let fewer = graph_of(vec![GraphEdge::new(
            "function:a",
            "function:b",
            GraphEdgeType::Calls,
        )]);
        assert_ne!(edges_digest(&full), edges_digest(&fewer));
    }

    #[test]
    fn changing_only_the_edge_type_changes_the_digest() {
        let calls = graph_of(vec![GraphEdge::new(
            "function:a",
            "function:b",
            GraphEdgeType::Calls,
        )]);
        let uses = graph_of(vec![GraphEdge::new(
            "function:a",
            "function:b",
            GraphEdgeType::References,
        )]);
        assert_ne!(edges_digest(&calls), edges_digest(&uses));
    }

    /// Endpoints are separated in the hash, so two different edge sets cannot
    /// collide by concatenating to the same bytes.
    #[test]
    fn endpoints_cannot_run_together_into_a_collision() {
        let a = graph_of(vec![GraphEdge::new("ab", "c", GraphEdgeType::Calls)]);
        let b = graph_of(vec![GraphEdge::new("a", "bc", GraphEdgeType::Calls)]);
        assert_ne!(edges_digest(&a), edges_digest(&b));
    }

    /// A prune removes *nodes*, which can tombstone edges with them. Folding
    /// the node count in means a digest taken before a prune cannot match the
    /// graph after one.
    #[test]
    fn dropping_a_node_changes_the_digest_even_with_the_same_edges() {
        let edges = vec![GraphEdge::new(
            "function:a",
            "function:b",
            GraphEdgeType::Calls,
        )];
        let mut fewer_nodes = graph_of(edges.clone());
        fewer_nodes.nodes.pop();
        assert_ne!(edges_digest(&graph_of(edges)), edges_digest(&fewer_nodes));
    }

    // ---- R4.2: what a stored vector means -------------------------------

    /// A pure-function test, because the case that motivates the width check
    /// cannot be built through the store: `upsert_nodes` rejects a mis-sized
    /// vector outright (`BadVector { got, want }`), so a wrong-width row only
    /// ever arises from a store whose dim changed under rows already written.
    fn row_with_vector(v: Vec<f32>) -> NodeRow {
        NodeRow {
            id: "n".into(),
            name: "n".into(),
            node_type: "Function".into(),
            description: String::new(),
            file: String::new(),
            start_line: 0,
            end_line: 0,
            last_update_at: 0,
            node_text: String::new(),
            vector: v,
            code: String::new(),
            file_hash: String::new(),
            facts: Default::default(),
        }
    }

    #[test]
    fn a_correctly_sized_vector_is_reusable_either_way() {
        let row = row_with_vector(vec![0.0; 384]);
        assert!(vector_is_reusable(&row, 384, VectorPlan::Skipping));
        assert!(vector_is_reusable(&row, 384, VectorPlan::Embedding));
    }

    #[test]
    fn no_vector_is_reusable_only_when_this_run_would_not_make_one() {
        let row = row_with_vector(Vec::new());
        // The default `ug gen`: no embedder now, none when this row was
        // written. Nothing to redo.
        assert!(vector_is_reusable(&row, 384, VectorPlan::Skipping));
        // `--with-embed`: the row is missing the vector this run can supply.
        assert!(!vector_is_reusable(&row, 384, VectorPlan::Embedding));
    }

    #[test]
    fn a_wrong_width_vector_is_never_reusable() {
        let row = row_with_vector(vec![0.0; 7]);
        assert!(!vector_is_reusable(&row, 384, VectorPlan::Skipping));
        assert!(!vector_is_reusable(&row, 384, VectorPlan::Embedding));
    }

    // ---- fixtures --------------------------------------------------------

    fn node(id: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: id.into(),
            node_type: GraphNodeType::Function,
            file: Some("src/a.ts".into()),
            start_line: Some(1),
            end_line: Some(9),
            ..Default::default()
        }
    }

    fn edge(source: &str, target: &str, edge_type: GraphEdgeType) -> GraphEdge {
        GraphEdge {
            source: source.into(),
            target: target.into(),
            edge_type,
        }
    }

    fn graph(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> GraphData {
        GraphData {
            nodes,
            edges,
            stats: None,
            resolution: None,
        }
    }

    /// A fact context over a graph with no edges — every degree is zero.
    /// Enough for the row-comparison tests, which vary node content rather
    /// than topology.
    fn fctx() -> FactContext {
        FactContext::new(&graph(vec![], vec![]))
    }

    /// The row a previous ingest would have written for `n`. Facts are
    /// computed rather than left empty so this really is "what was stored
    /// last time" — leaving them out would make every comparison test pass
    /// or fail for the wrong reason.
    fn stored(n: &GraphNode) -> NodeRow {
        NodeRow {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: n.node_type.as_str().to_string(),
            description: n.docstring.clone().unwrap_or_default(),
            file: n.file.clone().unwrap_or_default(),
            start_line: n.start_line.unwrap_or(0),
            end_line: n.end_line.unwrap_or(0),
            last_update_at: 0,
            node_text: String::new(),
            vector: Vec::new(),
            code: String::new(),
            file_hash: String::new(),
            facts: crate::storage::facts::compute(n, &fctx()),
        }
    }

    /// A store that records what it was asked to prune and does nothing
    /// else. Every other method is unreachable in these tests; reaching one
    /// is itself the failure.
    #[derive(Default)]
    struct RecordingStore {
        pruned: Mutex<Vec<HashSet<String>>>,
    }

    #[async_trait::async_trait]
    impl KnowledgeStore for RecordingStore {
        fn embedding_dim(&self) -> u32 {
            8
        }
        fn supports_native_ppr(&self) -> bool {
            false
        }
        fn backend_name(&self) -> &'static str {
            "recording"
        }
        async fn prune_nodes_absent_from(
            &self,
            keep: &HashSet<&str>,
        ) -> Result<usize, StoreError> {
            self.pruned
                .lock()
                .unwrap()
                .push(keep.iter().map(|s| s.to_string()).collect());
            Ok(keep.len())
        }
        async fn upsert_nodes(&self, _rows: &[NodeRow]) -> Result<(), StoreError> {
            unreachable!("upsert_nodes")
        }
        async fn upsert_edges(&self, _rows: &[EdgeRow]) -> Result<(), StoreError> {
            unreachable!("upsert_edges")
        }
        async fn vector_search(
            &self,
            _q: Vec<f32>,
            _k: usize,
            _f: Option<&NodeFilter>,
        ) -> Result<Vec<(NodeRow, f32)>, StoreError> {
            unreachable!("vector_search")
        }
        async fn hybrid_search(
            &self,
            _q: Vec<f32>,
            _s: Vec<(u32, f32)>,
            _t: &str,
            _k: usize,
            _f: Option<&NodeFilter>,
        ) -> Result<Vec<(NodeRow, f32)>, StoreError> {
            unreachable!("hybrid_search")
        }
        async fn traverse(
            &self,
            _s: &str,
            _h: u32,
            _e: Option<&[String]>,
            _d: Direction,
        ) -> Result<TraversalPage, StoreError> {
            unreachable!("traverse")
        }
        async fn nodes_by_ids(&self, _ids: &[String]) -> Result<Vec<NodeRow>, StoreError> {
            unreachable!("nodes_by_ids")
        }
        async fn fetch_node(&self, _key: &str) -> Result<Option<NodeRow>, StoreError> {
            unreachable!("fetch_node")
        }
        async fn count_nodes(&self) -> Result<usize, StoreError> {
            unreachable!("count_nodes")
        }
        async fn count_edges(&self) -> Result<usize, StoreError> {
            unreachable!("count_edges")
        }
        async fn personalized_pagerank(
            &self,
            _seeds: &[String],
            _d: Direction,
            _e: Option<&[String]>,
            _r: f32,
            _i: usize,
            _m: Option<usize>,
        ) -> Result<Vec<(String, f32)>, StoreError> {
            unreachable!("personalized_pagerank")
        }
    }

    // ---- is_prose_file ---------------------------------------------------

    #[test]
    fn prose_extensions_are_recognised_case_insensitively() {
        for p in ["README.md", "docs/a.MD", "notes.mdx", "book.markdown", "A.MarkDown"] {
            assert!(is_prose_file(p), "{p} should be prose");
        }
    }

    #[test]
    fn code_and_extensionless_paths_are_not_prose() {
        // Getting this wrong in the other direction is what matters: a code
        // file misread as prose loses its comments from the embedding text,
        // which for an undocumented symbol is all the prose it had.
        for p in [
            "src/a.ts",
            "src/a.rs",
            "src/a.java",
            "Makefile",
            "",
            "src/markdown/parser.ts",
            "a.md.ts",
        ] {
            assert!(!is_prose_file(p), "{p} should not be prose");
        }
    }

    #[test]
    fn a_dotfile_without_a_further_extension_is_not_prose() {
        // `rsplit_once('.')` on ".md" yields ("", "md"), so this pins that a
        // bare dotfile named like an extension isn't treated as a document.
        assert!(is_prose_file(".md"));
        assert!(!is_prose_file("md"));
    }

    // ---- graph_id_set / prune_to_graph -----------------------------------

    #[test]
    fn graph_id_set_collects_every_id_once() {
        let g = graph(vec![node("a"), node("b"), node("a")], vec![]);
        let ids = graph_id_set(&g);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("a") && ids.contains("b"));
    }

    #[test]
    fn graph_id_set_of_an_empty_graph_is_empty() {
        assert!(graph_id_set(&graph(vec![], vec![])).is_empty());
    }

    #[tokio::test]
    async fn pruning_an_empty_graph_is_refused_without_touching_the_store() {
        // The whole point of the guard: an empty node list means indexing
        // produced nothing — a bad path, a failed parse — not "the repo is
        // now empty". Passing it through would erase the entire store.
        let store = RecordingStore::default();
        let pruned = prune_to_graph(&store, &graph(vec![], vec![])).await.unwrap();
        assert_eq!(pruned, 0);
        assert!(
            store.pruned.lock().unwrap().is_empty(),
            "the store must not be asked to prune at all"
        );
    }

    #[tokio::test]
    async fn pruning_a_populated_graph_passes_its_complete_id_set_through() {
        let store = RecordingStore::default();
        let g = graph(vec![node("a"), node("b")], vec![]);
        let pruned = prune_to_graph(&store, &g).await.unwrap();

        assert_eq!(pruned, 2);
        let calls = store.pruned.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let expected: HashSet<String> = graph_id_set(&g).iter().map(|s| s.to_string()).collect();
        assert_eq!(calls[0], expected);
    }

    // ---- build_edge_rows -------------------------------------------------

    #[test]
    fn an_edge_row_id_is_the_triple_that_makes_it_unique() {
        // Source and target alone don't identify an edge — two nodes can be
        // related more than one way — so the type has to be in the key or
        // the second edge overwrites the first on upsert.
        let g = graph(
            vec![],
            vec![
                edge("a", "b", GraphEdgeType::Calls),
                edge("a", "b", GraphEdgeType::References),
            ],
        );
        let rows = build_edge_rows(&g);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a|Calls|b");
        assert_eq!(rows[1].id, "a|References|b");
        assert_ne!(rows[0].id, rows[1].id);
    }

    #[test]
    fn edge_rows_carry_the_variant_name_as_their_type() {
        // The string form is what `types_registry` canonicalizes into the
        // stored label, so it has to match the enum variant exactly.
        let g = graph(
            vec![],
            vec![
                edge("a", "b", GraphEdgeType::Overrides),
                edge("c", "d", GraphEdgeType::DependsOn),
            ],
        );
        let rows = build_edge_rows(&g);
        assert_eq!(rows[0].edge_type, "Overrides");
        assert_eq!(rows[1].edge_type, "DependsOn");
        assert_eq!(
            crate::storage::types_registry::edge_label("Overrides"),
            "Overrides",
            "the rendered name must round-trip through the registry"
        );
    }

    #[test]
    fn edge_rows_preserve_order_endpoints_and_leave_properties_empty() {
        let g = graph(vec![], vec![edge("src", "dst", GraphEdgeType::Contains)]);
        let rows = build_edge_rows(&g);
        assert_eq!(rows[0].source, "src");
        assert_eq!(rows[0].target, "dst");
        assert!(rows[0].properties.is_empty());
    }

    #[test]
    fn a_graph_with_no_edges_produces_no_rows() {
        assert!(build_edge_rows(&graph(vec![node("a")], vec![])).is_empty());
    }

    // ---- stored_row_matches ----------------------------------------------

    #[test]
    fn an_identical_row_matches_and_skips_the_upsert() {
        let n = node("a");
        assert!(stored_row_matches(&stored(&n), &n, "Function", None, &fctx()));
    }

    #[test]
    fn bookkeeping_fields_do_not_count_as_a_change() {
        // `last_update_at`, `node_text` and `vector` are excluded on
        // purpose: including them would make every node look changed on
        // every run, which is exactly what incremental ingest exists to
        // avoid.
        let n = node("a");
        let mut prev = stored(&n);
        prev.last_update_at = 1_700_000_000;
        prev.node_text = "anything".into();
        prev.vector = vec![0.5; 8];
        assert!(stored_row_matches(&prev, &n, "Function", None, &fctx()));
    }

    #[test]
    fn each_compared_field_on_its_own_forces_a_rewrite() {
        let n = node("a");
        type Mutate = fn(&mut NodeRow);
        let cases: &[(&str, Mutate)] = &[
            ("name", |r| r.name = "other".into()),
            ("description", |r| r.description = "doc".into()),
            ("file", |r| r.file = "src/b.ts".into()),
            ("start_line", |r| r.start_line = 2),
            ("end_line", |r| r.end_line = 99),
        ];
        for (label, mutate) in cases {
            let mut prev = stored(&n);
            mutate(&mut prev);
            assert!(
                !stored_row_matches(&prev, &n, "Function", None, &fctx()),
                "a changed {label} must not match"
            );
        }
        // The type is passed in rather than read off the row.
        assert!(!stored_row_matches(&stored(&n), &n, "Class", None, &fctx()));
    }

    #[test]
    fn a_node_missing_optional_fields_compares_against_empty_defaults() {
        // A File node carries no line range and no docstring; those must
        // compare equal to the stored zeros rather than looking changed.
        let bare = GraphNode {
            id: "file:src/a.ts".into(),
            name: "src/a.ts".into(),
            node_type: GraphNodeType::File,
            ..Default::default()
        };
        assert!(stored_row_matches(&stored(&bare), &bare, "File", None, &fctx()));
    }

    #[test]
    fn an_edited_body_forces_a_rewrite_even_when_the_text_is_identical() {
        // This is the case incremental ingest exists to catch cheaply: the
        // body changed, so `code` must be rewritten, but `node_text` didn't,
        // so it must not be re-embedded.
        let n = node("a");
        let prev = stored(&n);
        let captured = CapturedCode {
            code: "fn a() { changed(); }".into(),
            file_hash: "hash-2".into(),
        };
        assert!(!stored_row_matches(&prev, &n, "Function", Some(&captured), &fctx()));
    }

    #[test]
    fn a_matching_capture_still_matches() {
        let n = node("a");
        let mut prev = stored(&n);
        prev.code = "fn a() {}".into();
        prev.file_hash = "hash-1".into();
        let captured = CapturedCode {
            code: "fn a() {}".into(),
            file_hash: "hash-1".into(),
        };
        assert!(stored_row_matches(&prev, &n, "Function", Some(&captured), &fctx()));
    }

    /// The reason facts are part of the comparison at all. `in_degree` is
    /// not a property of this node — it moves when some *other* file starts
    /// calling it. If a content match short-circuited the rewrite, the
    /// stored degree would freeze at whatever the first ingest saw, and
    /// every statistic derived from it would drift further from the truth
    /// on each incremental run, with the node itself looking current.
    #[test]
    fn a_new_caller_elsewhere_forces_a_rewrite_even_though_the_node_is_identical() {
        let n = node("a");
        let prev = stored(&n); // ingested when nothing called it
        let with_caller = FactContext::new(&graph(
            vec![],
            vec![edge("caller", &n.id, GraphEdgeType::Calls)],
        ));
        assert!(
            !stored_row_matches(&prev, &n, "Function", None, &with_caller),
            "a node that gained a caller must be rewritten"
        );
    }

    #[test]
    fn a_row_stored_before_facts_existed_is_rewritten_rather_than_left_bare() {
        // Upgrading from a store that never wrote facts must backfill them,
        // otherwise `loc`/`in_degree` stay missing forever for untouched
        // nodes and every query over them under-reports.
        let n = node("a");
        let mut prev = stored(&n);
        prev.facts.clear();
        assert!(!stored_row_matches(&prev, &n, "Function", None, &fctx()));
    }

    #[test]
    fn a_failed_capture_is_ignored_rather_than_treated_as_a_change() {
        // If a read fails we pass `None`; letting that count as a diff would
        // make every node look changed on every run.
        let n = node("a");
        let mut prev = stored(&n);
        prev.code = "fn a() {}".into();
        prev.file_hash = "hash-1".into();
        assert!(stored_row_matches(&prev, &n, "Function", None, &fctx()));
    }

    // ---- budget_for ------------------------------------------------------

    /// A remote embedder builds nothing but an HTTP client — no model
    /// download, no network — so it is the cheap way to exercise anything
    /// that only reads the config.
    fn embedder_for_model(model: &str) -> Embedder {
        let cfg = crate::storage::embed::EmbedderConfig {
            model: model.to_string(),
            ..Default::default()
        };
        Embedder::remote(cfg).expect("remote embedder builds without I/O")
    }

    #[test]
    fn the_budget_follows_the_configured_model() {
        // The point of deriving this per model: a 512-token model and an
        // 8192-token one must not share a description cap, or one truncates
        // needlessly while the other overflows and gets silently cut.
        let small = budget_for(&embedder_for_model("bge-small-en-v1.5"));
        let large = budget_for(&embedder_for_model("nomic-embed-text-v1.5"));

        assert_eq!(small.window_tokens, Some(512));
        assert_eq!(large.window_tokens, Some(8192));
        assert!(
            large.description_chars > small.description_chars,
            "{} should exceed {}",
            large.description_chars,
            small.description_chars
        );
        assert_eq!(small.source, crate::limits::BudgetSource::Auto);
    }

    #[test]
    fn an_unknown_model_falls_back_to_the_fixed_budget() {
        let b = budget_for(&embedder_for_model("some-model-we-have-never-seen"));
        assert_eq!(b.window_tokens, None);
        assert_eq!(b.source, crate::limits::BudgetSource::Default);
        assert_eq!(b.description_chars, EmbedBudget::default().description_chars);
    }

    #[test]
    fn budget_for_never_passes_an_override() {
        // The lib-side entry points take no `--section-cap`; only the binary
        // resolves one. If this ever started passing `Some(..)` the flag
        // would apply where no flag was given.
        for model in ["bge-small-en-v1.5", "unknown"] {
            assert_ne!(
                budget_for(&embedder_for_model(model)).source,
                crate::limits::BudgetSource::Flag
            );
        }
    }

    // ---- current_unix_secs -----------------------------------------------

    #[test]
    fn the_clock_returns_a_plausible_and_monotonic_timestamp() {
        let t = current_unix_secs();
        // Later than 2020-01-01 and before 2100 — enough to catch a unit
        // mix-up (millis, nanos) or the `unwrap_or(0)` fallback firing.
        assert!(t > 1_577_836_800, "suspiciously early: {t}");
        assert!(t < 4_102_444_800, "suspiciously late: {t}");
        assert!(current_unix_secs() >= t);
    }
}
