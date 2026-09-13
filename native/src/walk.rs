//! Diff walk: turn a change into a guided walkthrough of the graph.
//!
//! `ug tour` answers a *question* by retrieving the neighbourhood that
//! matches it. A walk answers a *change*: the stops are the symbols a diff
//! actually touched, ordered by the call graph rather than by filename,
//! with the callers and tests the change reaches walked to from there.
//!
//! ```text
//!   git diff -U0  ──▶  hunks (file + line ranges)
//!                         │
//!                         ├─▶ innermost enclosing symbol  ─▶ seeds  (role=changed)
//!                         │
//!                         └─▶ inbound Calls/References    ─▶ ring 1 (role=caller|test)
//!                                       │
//!                                       ▼
//!                            tour::plan_from_candidates
//! ```
//!
//! Three things are worth knowing about this module:
//!
//! 1. **It reads `graph.json`, not the vector store.** Seeds come from git,
//!    so there is nothing to retrieve and nothing to embed — which means a
//!    walk runs on any generated project, with or without `ug ingest`, with
//!    or without an embedding backend. The LLM stays optional in the same
//!    way it is for a tour: without one you get the ordered itinerary and
//!    no narration.
//!
//! 2. **The innermost enclosing symbol wins.** A class node spans its
//!    methods, so a one-line edit inside a method overlaps both. Taking the
//!    smallest span that contains the hunk is what makes a walk stop at the
//!    method that changed instead of at the 400-line class around it.
//!
//! 3. **Cost is bounded by the diff, not by the repo.** The one whole-graph
//!    pass is an index of nodes by file, built once per walk; everything
//!    after it is proportional to the changed files and their symbols. A
//!    walk over a two-file commit does not get slower on a 500k-node graph
//!    (§1a).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crate::git::{self, ChangeStatus, DiffSummary, GitError, RevSpec};
use crate::tour::{
    self, Candidates, StopChange, Tour, TourEdge, TourOptions, TourProgress, ProgressFn,
};
use crate::types::{FileClassification, GraphData, GraphEdgeType, GraphNodeType};
use ultragraph::storage::ContextItem;

/// Edge types that mean "this code depends on that code". `Contains` is
/// deliberately absent: a file containing a changed function is not
/// affected by it, and including structural containment makes every
/// changed symbol's blast radius its own file.
const IMPACT_EDGES: [GraphEdgeType; 6] = [
    GraphEdgeType::Calls,
    GraphEdgeType::References,
    GraphEdgeType::Instantiates,
    GraphEdgeType::Overrides,
    GraphEdgeType::Implements,
    GraphEdgeType::Extends,
];

/// How many unchanged neighbours a walk may add per changed symbol. The
/// ring exists to show what the change *reaches*, and a symbol called from
/// 200 places would otherwise be the entire itinerary.
const MAX_RING_PER_SEED: usize = 3;

/// Ceiling on the whole ring, however many seeds there are.
const MAX_RING_TOTAL: usize = 24;

/// Why a stop is on the walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The diff touched these very lines.
    Changed,
    /// Unchanged code that reaches something that changed.
    Caller,
    /// A test that reaches something that changed.
    Test,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Changed => "changed",
            Role::Caller => "caller",
            Role::Test => "test",
        }
    }
}

/// One node on the walk, before it becomes a `ContextItem`.
#[derive(Clone, Debug)]
pub struct WalkNode {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub description: String,
    pub role: Role,
    pub status: Option<ChangeStatus>,
    pub added: u32,
    pub removed: u32,
}

impl WalkNode {
    fn change(&self) -> StopChange {
        StopChange {
            role: self.role.as_str().to_string(),
            status: self.status.map(|s| s.as_str().to_string()),
            added: self.added,
            removed: self.removed,
        }
    }
}

/// A planned walk: the diff behind it, and the itinerary through it.
pub struct Walk {
    pub tour: Tour,
    pub diff: DiffSummary,
    /// Changed files the index has no symbol for — a file `ug gen` never
    /// indexed, or one whose only change was outside every symbol.
    ///
    /// Surfaced rather than dropped: silence here is indistinguishable
    /// from "nothing changed there", and the usual cause is an index that
    /// predates the file.
    pub unmapped: Vec<String>,
}

// ── resolving the diff ─────────────────────────────────────────────────────

/// Read the diff for `spec` and re-express its paths against the root the
/// project was indexed from.
///
/// `project_root` is the repo root `ug` recorded at `gen` time, which is
/// not necessarily git's — see [`git::rebase_paths`].
pub fn resolve_diff(project_root: &Path, spec: &RevSpec) -> Result<DiffSummary, GitError> {
    let git_root = git::repo_root(project_root)?;
    let mut diff = git::diff(project_root, spec)?;
    git::rebase_paths(&mut diff, &git_root, project_root);
    Ok(diff)
}

/// Changed files whose line numbers in this diff no longer describe the
/// file on disk — and therefore no longer line up with the graph.
///
/// Best-effort: a repository that cannot answer the question gets an empty
/// set, which reads as "no known drift" rather than failing a walk over a
/// detail of its own reporting.
pub fn drifted_files(project_root: &Path, spec: &RevSpec, diff: &DiffSummary) -> Vec<String> {
    let Some(tip) = git::new_side_rev(spec, diff) else {
        return Vec::new();
    };
    let Ok(moved) = git::drifted_since(project_root, &tip) else {
        return Vec::new();
    };
    if moved.is_empty() {
        return Vec::new();
    }
    let git_root = git::repo_root(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    // `moved` is git-root relative; the diff's paths have already been
    // rebased onto the indexed root, so the comparison needs the prefix put
    // back rather than the paths compared as-is.
    let prefix = std::fs::canonicalize(project_root)
        .ok()
        .and_then(|p| p.strip_prefix(&git_root).ok().map(|r| r.to_path_buf()))
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .filter(|r| !r.is_empty())
        .map(|r| format!("{}/", r.trim_end_matches('/')))
        .unwrap_or_default();

    let mut out: Vec<String> = diff
        .files
        .iter()
        .filter(|f| moved.contains(&format!("{}{}", prefix, f.path)))
        .map(|f| f.path.clone())
        .collect();
    out.sort();
    out
}

/// The warning a drifted walk carries. Spelled out rather than hedged:
/// the stops are still the right *neighbourhood*, and the user needs to
/// know which half of that sentence to trust.
fn drift_warning(tip_label: &str, drifted: &[String]) -> String {
    const SHOW: usize = 4;
    let shown: Vec<&str> = drifted.iter().take(SHOW).map(String::as_str).collect();
    let more = drifted.len().saturating_sub(shown.len());
    let extra = if more > 0 {
        format!(" and {} more", more)
    } else {
        String::new()
    };
    format!(
        "{} of these files have changed since {}, so this walk maps that diff's line numbers onto today's code — stops in {}{} may be near the change rather than on it. Walk your uncommitted changes, or the most recent commit, for an exact mapping.",
        drifted.len(),
        tip_label,
        shown.join(", "),
        extra
    )
}

// ── mapping hunks onto nodes ───────────────────────────────────────────────

/// Which symbols a diff touched, innermost-first, ranked by how much of
/// each one moved.
///
/// Returns the touched nodes and the changed paths nothing could be mapped
/// to.
pub fn changed_symbols(graph: &GraphData, diff: &DiffSummary) -> (Vec<WalkNode>, Vec<String>) {
    // One pass over the graph, indexing only the files the diff mentions.
    // Filtering here rather than building a whole-repo index is what keeps
    // a two-file walk independent of graph size.
    let wanted: HashSet<&str> = diff
        .files
        .iter()
        .filter(|f| !f.hunks.is_empty())
        .map(|f| f.path.as_str())
        .collect();

    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        if n.node_type == GraphNodeType::Folder {
            continue;
        }
        let Some(file) = n.file.as_deref() else { continue };
        if !wanted.contains(file) {
            continue;
        }
        by_file.entry(file).or_default().push(i);
    }

    let mut touched: HashMap<usize, (u32, u32, u32)> = HashMap::new(); // idx -> (added, removed, span)
    let mut unmapped: Vec<String> = Vec::new();

    for f in &diff.files {
        if f.hunks.is_empty() {
            // A deletion or a binary file: there is nothing in the working
            // tree to stop at. Named in `unmapped` so the walk can say the
            // file changed even though it cannot visit it.
            unmapped.push(f.path.clone());
            continue;
        }
        let Some(candidates) = by_file.get(f.path.as_str()) else {
            unmapped.push(f.path.clone());
            continue;
        };

        // Innermost first, so a method claims its own lines before the
        // class — and then the file — can claim what is left. The id
        // tie-break keeps equal-span symbols in a stable order; without it
        // the same diff produces a different walk on every run.
        let mut ranked: Vec<usize> = candidates.clone();
        ranked.sort_by_key(|&i| {
            let n = &graph.nodes[i];
            (
                n.end_line
                    .unwrap_or(0)
                    .saturating_sub(n.start_line.unwrap_or(0)),
                // On a tie the File node loses. A one-symbol file gives the
                // symbol and the file the same extent, and "the file
                // changed" is never the better answer when a real symbol
                // covers the same lines.
                u8::from(n.node_type == GraphNodeType::File),
                n.id.clone(),
            )
        });

        let mut hit_any = false;
        for hunk in &f.hunks {
            let hunk_len = hunk.range.end.saturating_sub(hunk.range.start) + 1;
            // Which of the hunk's lines have been spoken for. A hunk is
            // frequently much larger than any one symbol — a 268-line test
            // module lands as a single `@@` — and crediting all of it to
            // whichever symbol it happens to overlap first reports a
            // three-line function as a 268-line change.
            let mut claimed: Vec<(u32, u32)> = Vec::new();
            for &idx in &ranked {
                let n = &graph.nodes[idx];
                let (Some(start), Some(end)) = (n.start_line, n.end_line) else {
                    continue;
                };
                let lo = start.max(hunk.range.start);
                let hi = end.min(hunk.range.end);
                if lo > hi {
                    continue;
                }
                // A File node spans the whole file, so it is last in rank
                // order and picks up exactly the lines no symbol covers —
                // an import, a top-level constant, a new function the
                // index has not seen yet.
                let gained = claim(lo, hi, &mut claimed);
                if gained == 0 {
                    continue;
                }
                hit_any = true;
                let e = touched.entry(idx).or_insert((0, 0, 0));
                // Additions are positional (with -U0 every new-side line
                // in the hunk is one), so a symbol's share is its line
                // count. Deletions have no new-side position at all, so
                // they are prorated by that same share — the only honest
                // option short of re-reading the old file.
                e.0 += mul_div(hunk.added, gained, hunk_len);
                e.1 += mul_div(hunk.removed, gained, hunk_len);
                e.2 += gained;
            }
        }
        if !hit_any {
            unmapped.push(f.path.clone());
        }
    }

    let status_of = git::by_path(diff);
    let mut nodes: Vec<WalkNode> = touched
        .into_iter()
        .map(|(idx, (added, removed, _lines))| {
            let n = &graph.nodes[idx];
            let file = n.file.clone().unwrap_or_default();
            WalkNode {
                id: n.id.clone(),
                name: n.name.clone(),
                node_type: n.node_type.as_str().to_string(),
                start_line: n.start_line.unwrap_or(0),
                end_line: n.end_line.unwrap_or(0),
                description: describe(n),
                role: Role::Changed,
                status: status_of.get(file.as_str()).map(|f| f.status),
                added,
                removed,
                file,
            }
        })
        .collect();

    // Most-changed first. The tie-break on id is not cosmetic: `HashMap`
    // iteration order is arbitrary, and without it the same diff produces
    // a different itinerary on every run (Agents.md §11 — `ug gen` had
    // exactly this bug).
    nodes.sort_by(|a, b| {
        (b.added + b.removed)
            .cmp(&(a.added + a.removed))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.id.cmp(&b.id))
    });
    unmapped.sort();
    unmapped.dedup();
    (nodes, unmapped)
}

/// Claim `[lo, hi]` and report how many of its lines were not already
/// claimed. `claimed` is kept sorted and merged.
///
/// Separate and tested because it is the arithmetic that decides what a
/// stop's `+n/-n` says, and an off-by-one here is invisible — the walk
/// still runs, it just reports the wrong size of change.
fn claim(lo: u32, hi: u32, claimed: &mut Vec<(u32, u32)>) -> u32 {
    let mut gained = 0u32;
    let mut cursor = lo;
    for &(cs, ce) in claimed.iter() {
        if ce < cursor {
            continue;
        }
        if cs > hi {
            break;
        }
        if cs > cursor {
            gained += cs - cursor;
        }
        cursor = cursor.max(ce.saturating_add(1));
        if cursor > hi {
            break;
        }
    }
    if cursor <= hi {
        gained += hi - cursor + 1;
    }
    if gained == 0 {
        return 0;
    }
    // Insert, then merge anything the new interval now touches, so the
    // scan above stays linear however many symbols a file has.
    let at = claimed.partition_point(|&(cs, _)| cs < lo);
    claimed.insert(at, (lo, hi));
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(claimed.len());
    for &(cs, ce) in claimed.iter() {
        match merged.last_mut() {
            Some(last) if cs <= last.1.saturating_add(1) => last.1 = last.1.max(ce),
            _ => merged.push((cs, ce)),
        }
    }
    *claimed = merged;
    gained
}

/// `n * num / den`, rounded to nearest, saturating. Used to split a hunk's
/// line counts across the symbols it spans.
fn mul_div(n: u32, num: u32, den: u32) -> u32 {
    if den == 0 {
        return 0;
    }
    let v = (u64::from(n) * u64::from(num) + u64::from(den) / 2) / u64::from(den);
    v.min(u64::from(u32::MAX)) as u32
}

/// A node's one-line description for the guide's menu. Docstrings are the
/// authored answer; the signature is the fallback that at least says what
/// shape the thing is.
fn describe(n: &crate::types::GraphNode) -> String {
    if let Some(d) = n.docstring.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    if let Some(route) = n.route.as_deref() {
        return format!("Route {}", route);
    }
    // No prose: reconstruct the signature, which at least says what shape
    // the thing is. The guide's menu with a blank line against a name is
    // one it cannot choose from.
    if let Some(sig) = n.signature.as_ref() {
        let params: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        let ret = sig
            .return_type
            .as_deref()
            .map(|r| format!(" -> {}", r))
            .unwrap_or_default();
        return format!("{}({}){}", n.name, params.join(", "), ret);
    }
    String::new()
}

// ── the impact ring ────────────────────────────────────────────────────────

/// Unchanged code that reaches something in `seeds` — the blast radius,
/// one hop out, with tests called out separately.
///
/// One hop, not three: a walk is a narrative and every extra ring is
/// another stop that is further from what the user actually changed.
/// `analyze diff_impact` is the tool for the full reachable set.
pub fn impact_ring(graph: &GraphData, seeds: &[WalkNode]) -> Vec<WalkNode> {
    if seeds.is_empty() {
        return Vec::new();
    }
    let seed_ids: HashSet<&str> = seeds.iter().map(|s| s.id.as_str()).collect();
    let by_id: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    // Inbound impact edges, grouped by the seed they point at.
    let mut inbound: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &graph.edges {
        if !IMPACT_EDGES.contains(&e.edge_type) {
            continue;
        }
        let target: &str = &e.target;
        let source: &str = &e.source;
        if !seed_ids.contains(target) || seed_ids.contains(source) {
            // A caller that also changed is already a stop; listing it
            // twice would make the route stutter.
            continue;
        }
        inbound.entry(target).or_default().push(source);
    }

    let mut out: Vec<WalkNode> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    // Seed order is rank order, so the most-changed symbols get their
    // callers in first when the total budget binds.
    for seed in seeds {
        if out.len() >= MAX_RING_TOTAL {
            break;
        }
        let Some(sources) = inbound.get(seed.id.as_str()) else {
            continue;
        };
        // Deterministic for the same reason the seed sort is: edge order
        // in graph.json is an artefact of extraction, not a ranking.
        let mut sources: Vec<&str> = sources.clone();
        sources.sort_unstable();
        sources.dedup();

        let mut taken = 0usize;
        for src in sources {
            if taken >= MAX_RING_PER_SEED || out.len() >= MAX_RING_TOTAL {
                break;
            }
            if !seen.insert(src) {
                continue;
            }
            let Some(&idx) = by_id.get(src) else { continue };
            let n = &graph.nodes[idx];
            if n.node_type == GraphNodeType::Folder || n.node_type == GraphNodeType::File {
                continue;
            }
            let is_test = n.classification == Some(FileClassification::Test);
            out.push(WalkNode {
                id: n.id.clone(),
                name: n.name.clone(),
                node_type: n.node_type.as_str().to_string(),
                file: n.file.clone().unwrap_or_default(),
                start_line: n.start_line.unwrap_or(0),
                end_line: n.end_line.unwrap_or(0),
                description: describe(n),
                role: if is_test { Role::Test } else { Role::Caller },
                status: None,
                added: 0,
                removed: 0,
            });
            taken += 1;
        }
    }
    out
}

// ── ordering ───────────────────────────────────────────────────────────────

/// Order the itinerary so callers come before what they call.
///
/// This is the whole pitch of a diff walk over a diff *view*: a patch is
/// ordered by path, which puts `src/api.rs` before `src/zod.rs` for no
/// reason anyone cares about. Ordering by the call graph makes consecutive
/// stops actually connected, so the narration can say "…which calls…" and
/// be telling the truth.
///
/// Kahn's algorithm over the changed set alone, with rank order as the
/// tie-break and as the fallback for the cycles a real call graph always
/// has.
pub fn order_by_flow(nodes: Vec<WalkNode>, graph: &GraphData) -> Vec<WalkNode> {
    if nodes.len() < 2 {
        return nodes;
    }
    let pos: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    let mut indegree = vec![0usize; nodes.len()];
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut seen_pairs: HashSet<(usize, usize)> = HashSet::new();
    for e in &graph.edges {
        if !IMPACT_EDGES.contains(&e.edge_type) {
            continue;
        }
        let (Some(&from), Some(&to)) = (
            pos.get(e.source.as_ref() as &str),
            pos.get(e.target.as_ref() as &str),
        ) else {
            continue;
        };
        if from == to || !seen_pairs.insert((from, to)) {
            continue;
        }
        out_edges[from].push(to);
        indegree[to] += 1;
    }

    // Ready set kept in rank order rather than as a heap: the list is at
    // most a few dozen stops, and "the highest-ranked thing that is ready"
    // is exactly a linear scan.
    let mut emitted = vec![false; nodes.len()];
    let mut order: Vec<usize> = Vec::with_capacity(nodes.len());
    for _ in 0..nodes.len() {
        let next = (0..nodes.len())
            .find(|&i| !emitted[i] && indegree[i] == 0)
            // Every remaining node is in a cycle; take the best-ranked one
            // and carry on rather than stopping the walk.
            .or_else(|| (0..nodes.len()).find(|&i| !emitted[i]));
        let Some(i) = next else { break };
        emitted[i] = true;
        order.push(i);
        for &j in &out_edges[i] {
            indegree[j] = indegree[j].saturating_sub(1);
        }
    }

    let mut slots: Vec<Option<WalkNode>> = nodes.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect()
}

// ── planning ───────────────────────────────────────────────────────────────

/// Standing instructions added to the tour guide's system prompt for a
/// walk. The guide is otherwise told it is answering a question, and it
/// will happily narrate a changed function as though the user asked about
/// it in the abstract — which reads as a code tour, not as a review.
const WALK_BRIEF: &str = "\
This tour is a CODE REVIEW WALK of a specific change, not a general tour of the codebase. Every \
item is labelled with its role:\n\
- [changed] — the diff edited these exact lines. These are the point of the walk; visit the ones \
that matter and say what the change does.\n\
- [caller] — unchanged code that calls or references something changed. Visit one only to show \
what the change is visible from.\n\
- [test] — a test that reaches something changed. Worth a stop when it shows the change is covered.\n\n\
Open at the changed item that best explains the intent of the change, follow the flow into what it \
affects, and close by saying what the change means for someone reading the code. Narrate what \
CHANGED at each [changed] stop — not what the symbol does in general. Never describe a [caller] or \
[test] stop as though it were edited.";

/// How the walk's question reads to the guide, and as the tour's title.
fn walk_query(diff: &DiffSummary) -> String {
    format!(
        "What does this change do, and what does it affect? ({}: {} file{}, +{}/-{})",
        diff.label,
        diff.files.len(),
        if diff.files.len() == 1 { "" } else { "s" },
        diff.insertions,
        diff.deletions
    )
}

/// Options for a walk. The retrieval knobs a tour carries are absent —
/// there is no retrieval — so this is the tour's planning subset plus the
/// one switch a walk adds.
#[derive(Clone, Debug)]
pub struct WalkOptions {
    pub max_stops: usize,
    /// Include the callers and tests the change reaches. On by default:
    /// "what does this affect" is half the question.
    pub expand: bool,
    pub include_snippets: bool,
    pub include_debug: bool,
    pub stream: bool,
    /// Let the guide deliberate. Off for the same reason a tour's is.
    pub think: bool,
}

impl WalkOptions {
    pub fn new() -> Self {
        Self {
            max_stops: tour::DEFAULT_MAX_STOPS,
            expand: true,
            include_snippets: true,
            include_debug: true,
            stream: false,
            think: false,
        }
    }
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the candidate pack a walk hands the tour planner.
///
/// Separated from [`plan_walk`] so the MCP tool — which wants the stops and
/// not the narration — can stop here.
pub fn build_candidates(
    graph: &GraphData,
    diff: &DiffSummary,
    opts: &WalkOptions,
) -> (Vec<WalkNode>, Vec<String>) {
    let (seeds, unmapped) = changed_symbols(graph, diff);
    let ring = if opts.expand {
        impact_ring(graph, &seeds)
    } else {
        Vec::new()
    };
    // Flow order over the changed set, then the ring appended: the ring is
    // *about* the seeds, so it reads as a coda rather than as more of the
    // change. The guide is free to interleave them; this order is what the
    // ranked fallback uses and what the guide sees first.
    let mut nodes = order_by_flow(seeds, graph);
    nodes.extend(ring);
    (nodes, unmapped)
}

/// Graph edges among the walk's own nodes, so the UI can draw the route
/// and the guide can see which stops are actually connected.
fn walk_edges(graph: &GraphData, nodes: &[WalkNode]) -> Vec<TourEdge> {
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut seen: HashSet<(&str, &str, &str)> = HashSet::new();
    let mut out = Vec::new();
    for e in &graph.edges {
        if e.edge_type == GraphEdgeType::Contains {
            continue;
        }
        let (source, target): (&str, &str) = (&e.source, &e.target);
        if !ids.contains(source) || !ids.contains(target) {
            continue;
        }
        if !seen.insert((source, target, e.edge_type.as_str())) {
            continue;
        }
        out.push(TourEdge {
            source: source.to_string(),
            target: target.to_string(),
            edge_type: e.edge_type.as_str().to_string(),
        });
    }
    out
}

/// A walk node as the planner's context item. `matched_by` carries the
/// role, which is what puts `[changed]` / `[caller]` in front of the guide.
fn to_context_item(n: &WalkNode) -> ContextItem {
    ContextItem {
        id: n.id.clone(),
        name: n.name.clone(),
        node_type: n.node_type.clone(),
        file: n.file.clone(),
        start_line: n.start_line,
        end_line: n.end_line,
        description: n.description.clone(),
        // Rank is positional here — the order is the itinerary — so a flat
        // distance keeps anything downstream from re-sorting by it.
        distance: 0.0,
        hop: if n.role == Role::Changed { 0 } else { 1 },
        snippet: None,
        matched_by: n.role.as_str().to_string(),
    }
}

/// Plan a walk over `diff`: map it onto the graph, order it, and (when a
/// model is configured) have the tour guide narrate it.
///
/// `chat` is `None` for the itinerary-only path — the same contract
/// [`tour::plan_from_candidates`] has, and what the MCP tool and a
/// model-less `ug walk` use.
pub async fn plan_walk(
    graph: &GraphData,
    repo_root: &Path,
    diff: DiffSummary,
    drifted: &[String],
    chat: Option<&crate::chat::ChatClient>,
    opts: &WalkOptions,
    on_progress: ProgressFn<'_>,
) -> Result<Walk, Box<dyn std::error::Error + Send + Sync>> {
    let t0 = Instant::now();
    on_progress(TourProgress::Retrieving);
    let (nodes, unmapped) = build_candidates(graph, &diff, opts);
    let mapping_ms = t0.elapsed().as_millis();
    on_progress(TourProgress::Retrieved {
        candidates: nodes.len(),
        retrieval_ms: mapping_ms,
    });

    let query = walk_query(&diff);
    if nodes.is_empty() {
        let tour = tour::empty_tour_with(&query, mapping_ms, empty_reason(&diff, &unmapped));
        return Ok(Walk {
            tour,
            diff,
            unmapped,
        });
    }

    let edges = walk_edges(graph, &nodes);
    on_progress(TourProgress::Linking { edges: edges.len() });

    let mut items: Vec<ContextItem> = nodes.iter().map(to_context_item).collect();
    if opts.include_snippets {
        let cap = items.len();
        tour::attach_prompt_snippets(&mut items, repo_root, cap);
        on_progress(TourProgress::ReadingCode {
            items: items.iter().filter(|i| i.snippet.is_some()).count(),
        });
    }

    let mut topts = TourOptions::new();
    topts.max_stops = opts.max_stops;
    topts.include_snippets = opts.include_snippets;
    topts.include_debug = opts.include_debug;
    topts.stream = opts.stream;
    topts.fast = !opts.think;

    let seed_id = nodes.first().map(|n| n.id.clone());
    let mut tour = tour::plan_from_candidates(
        chat,
        repo_root,
        &query,
        Candidates {
            items,
            edges,
            seed_id,
            retrieval_ms: mapping_ms,
            brief: Some(WALK_BRIEF),
        },
        &topts,
        None,
        on_progress,
    )
    .await?;

    // `fallback_tour` frames a ranked itinerary as "the most relevant
    // parts of the codebase for <question>", which is the wrong sentence
    // for a change: nothing was ranked by relevance and the question was
    // synthesised. Re-frame it before anything reads it.
    if tour.fallback {
        tour.title = diff.label.clone();
        // The label is already the title directly above this line; naming
        // it twice reads as a stutter, and a commit subject in the middle
        // of a sentence rarely parses as one.
        tour.intro =
            "What this change touched, in call-graph order — callers before the code they call."
                .to_string();
    }
    annotate(&mut tour, &nodes_by_id(&nodes));
    if !unmapped.is_empty() {
        tour.warnings.push(unmapped_warning(&unmapped));
    }
    if !drifted.is_empty() {
        tour.warnings.push(drift_warning(&diff.short_label(), drifted));
    }
    if diff.truncated {
        tour.warnings.push(format!(
            "This diff touches more than {} files; the walk covers the first {} of them.",
            git::MAX_DIFF_FILES,
            git::MAX_DIFF_FILES
        ));
    }
    Ok(Walk {
        tour,
        diff,
        unmapped,
    })
}

fn nodes_by_id(nodes: &[WalkNode]) -> HashMap<&str, &WalkNode> {
    nodes.iter().map(|n| (n.id.as_str(), n)).collect()
}

/// Stamp each stop with why it is on the walk.
///
/// Done after planning rather than before because the guide chooses the
/// route: until it has, there is no list of stops to annotate.
fn annotate(tour: &mut Tour, by_id: &HashMap<&str, &WalkNode>) {
    for stop in &mut tour.stops {
        if let Some(n) = by_id.get(stop.node_id.as_str()) {
            stop.change = Some(n.change());
        }
    }
}

/// Why a walk has no stops. The three causes need different fixes, and
/// "no stops" alone sends people to the wrong one.
fn empty_reason(diff: &DiffSummary, unmapped: &[String]) -> String {
    if diff.is_empty() {
        return format!("Nothing changed in {}.", diff.label);
    }
    if !unmapped.is_empty() {
        return format!(
            "{} changed {} file{}, but none of them are in this project's index — run `ug gen` \
             (or `ug update {}`) and walk it again.",
            diff.label,
            unmapped.len(),
            if unmapped.len() == 1 { "" } else { "s" },
            unmapped.first().map(String::as_str).unwrap_or("<file>")
        );
    }
    format!(
        "{} touched only code the index has no symbols for — comments, blank lines or \
         generated files.",
        diff.label
    )
}

fn unmapped_warning(unmapped: &[String]) -> String {
    const SHOW: usize = 5;
    let shown: Vec<&str> = unmapped.iter().take(SHOW).map(String::as_str).collect();
    let more = unmapped.len().saturating_sub(shown.len());
    format!(
        "{} changed file{} had no indexed symbol to stop at{}: {}{}. Deleted and binary files \
         have nothing to visit; anything else means the index predates the file — run `ug gen`.",
        unmapped.len(),
        if unmapped.len() == 1 { "" } else { "s" },
        if more > 0 { " (first few)" } else { "" },
        shown.join(", "),
        if more > 0 {
            format!(" and {} more", more)
        } else {
            String::new()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileChange, Hunk, LineRange};
    use crate::types::{GraphEdge, GraphNode};

    fn node(id: &str, name: &str, ty: GraphNodeType, file: &str, s: u32, e: u32) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: ty,
            file: Some(file.to_string()),
            start_line: Some(s),
            end_line: Some(e),
            ..Default::default()
        }
    }

    fn changed(path: &str, hunks: &[(u32, u32, u32, u32)]) -> FileChange {
        FileChange {
            path: path.to_string(),
            old_path: None,
            status: ChangeStatus::Modified,
            added: hunks.iter().map(|h| h.2).sum(),
            removed: hunks.iter().map(|h| h.3).sum(),
            hunks: hunks
                .iter()
                .map(|&(start, end, added, removed)| Hunk {
                    range: LineRange { start, end },
                    added,
                    removed,
                })
                .collect(),
            binary: false,
        }
    }

    fn summary(files: Vec<FileChange>) -> DiffSummary {
        DiffSummary {
            spec: "working".into(),
            label: "uncommitted changes".into(),
            insertions: files.iter().map(|f| f.added).sum(),
            deletions: files.iter().map(|f| f.removed).sum(),
            files,
            commits: vec![],
            truncated: false,
        }
    }

    /// A class spans its methods, so a hunk inside one overlaps both. The
    /// walk must stop at the method.
    #[test]
    fn innermost_enclosing_symbol_wins() {
        let graph = GraphData {
            nodes: vec![
                node("file:a.rs", "a.rs", GraphNodeType::File, "a.rs", 1, 200),
                node("class:Big", "Big", GraphNodeType::Class, "a.rs", 10, 100),
                node("fn:inner", "inner", GraphNodeType::Function, "a.rs", 40, 60),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let diff = summary(vec![changed("a.rs", &[(45, 47, 3, 1)])]);
        let (seeds, unmapped) = changed_symbols(&graph, &diff);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].id, "fn:inner");
        assert_eq!((seeds[0].added, seeds[0].removed), (3, 1));
        assert!(unmapped.is_empty());
    }

    /// A change between symbols — an import, a top-level constant — still
    /// belongs to the file, which is the only node that contains it.
    /// A file holding one symbol gives both the same extent. The symbol is
    /// still the better stop.
    #[test]
    fn a_symbol_spanning_its_whole_file_still_beats_the_file_node() {
        let graph = GraphData {
            nodes: vec![
                node("file:a.rs", "a.rs", GraphNodeType::File, "a.rs", 1, 4),
                node("fn:only", "only", GraphNodeType::Function, "a.rs", 1, 4),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let diff = summary(vec![changed("a.rs", &[(3, 3, 1, 1)])]);
        let (seeds, _) = changed_symbols(&graph, &diff);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].id, "fn:only");
    }

    #[test]
    fn a_change_outside_every_symbol_falls_back_to_the_file() {
        let graph = GraphData {
            nodes: vec![
                node("file:a.rs", "a.rs", GraphNodeType::File, "a.rs", 1, 200),
                node("fn:inner", "inner", GraphNodeType::Function, "a.rs", 40, 60),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let diff = summary(vec![changed("a.rs", &[(3, 4, 2, 0)])]);
        let (seeds, _) = changed_symbols(&graph, &diff);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].id, "file:a.rs");
    }

    /// The bug this guards: a 268-line test module lands as one `@@`, and
    /// crediting the whole hunk to whichever small symbol it happens to
    /// overlap reported a three-line function as a 268-line change.
    #[test]
    fn a_hunk_larger_than_a_symbol_only_credits_the_overlap() {
        let graph = GraphData {
            nodes: vec![
                node("file:a.rs", "a.rs", GraphNodeType::File, "a.rs", 1, 900),
                node("fn:tiny", "tiny", GraphNodeType::Function, "a.rs", 768, 770),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        // @@ -598,0 +606,268 @@ — 268 added lines spanning the small fn.
        let diff = summary(vec![changed("a.rs", &[(606, 873, 268, 0)])]);
        let (seeds, _) = changed_symbols(&graph, &diff);
        let by_id: HashMap<&str, &WalkNode> =
            seeds.iter().map(|n| (n.id.as_str(), n)).collect();
        assert_eq!(by_id["fn:tiny"].added, 3, "only its own three lines");
        assert_eq!(
            by_id["file:a.rs"].added, 265,
            "the rest belongs to the file, which is the only node that contains it"
        );
    }

    #[test]
    fn a_symbol_inside_a_class_claims_its_lines_before_the_class_does() {
        let graph = GraphData {
            nodes: vec![
                node("class:C", "C", GraphNodeType::Class, "a.rs", 10, 100),
                node("fn:m", "m", GraphNodeType::Function, "a.rs", 40, 60),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        // A hunk covering the whole class: the method takes its 21 lines,
        // the class keeps the other 70.
        let diff = summary(vec![changed("a.rs", &[(10, 100, 91, 0)])]);
        let (seeds, _) = changed_symbols(&graph, &diff);
        let by_id: HashMap<&str, &WalkNode> =
            seeds.iter().map(|n| (n.id.as_str(), n)).collect();
        assert_eq!(by_id["fn:m"].added, 21);
        assert_eq!(by_id["class:C"].added, 70);
    }

    #[test]
    fn claiming_counts_only_what_is_new_and_merges_as_it_goes() {
        let mut claimed = Vec::new();
        assert_eq!(claim(10, 20, &mut claimed), 11);
        // Wholly inside what is already claimed.
        assert_eq!(claim(12, 15, &mut claimed), 0);
        // Straddling the edge: only the four new lines count.
        assert_eq!(claim(18, 24, &mut claimed), 4);
        // Adjacent ranges merge, so the set stays one interval.
        assert_eq!(claimed, vec![(10, 24)]);
        // A gap below is still available.
        assert_eq!(claim(1, 9, &mut claimed), 9);
        assert_eq!(claimed, vec![(1, 24)], "adjacent intervals merge");
    }

    #[test]
    fn prorating_rounds_to_nearest_and_never_divides_by_zero() {
        assert_eq!(mul_div(10, 1, 3), 3);
        assert_eq!(mul_div(10, 2, 3), 7);
        assert_eq!(mul_div(268, 268, 268), 268);
        assert_eq!(mul_div(5, 1, 0), 0);
    }

    #[test]
    fn several_hunks_in_one_symbol_accumulate() {
        let graph = GraphData {
            nodes: vec![node("fn:f", "f", GraphNodeType::Function, "a.rs", 1, 100)],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let diff = summary(vec![changed("a.rs", &[(10, 12, 3, 0), (50, 50, 1, 4)])]);
        let (seeds, _) = changed_symbols(&graph, &diff);
        assert_eq!(seeds.len(), 1);
        assert_eq!((seeds[0].added, seeds[0].removed), (4, 4));
    }

    #[test]
    fn files_the_index_has_never_seen_are_reported_not_dropped() {
        let graph = GraphData {
            nodes: vec![node("fn:f", "f", GraphNodeType::Function, "a.rs", 1, 100)],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let diff = summary(vec![
            changed("a.rs", &[(5, 6, 2, 0)]),
            changed("brand/new.rs", &[(1, 9, 9, 0)]),
        ]);
        let (seeds, unmapped) = changed_symbols(&graph, &diff);
        assert_eq!(seeds.len(), 1);
        assert_eq!(unmapped, vec!["brand/new.rs".to_string()]);
        assert!(unmapped_warning(&unmapped).contains("brand/new.rs"));
    }

    /// The ordering claim the whole feature rests on: a caller is visited
    /// before what it calls, whatever order the diff listed them in.
    #[test]
    fn flow_order_puts_callers_before_callees() {
        let graph = GraphData {
            nodes: vec![
                node("fn:a", "a", GraphNodeType::Function, "z.rs", 1, 10),
                node("fn:b", "b", GraphNodeType::Function, "m.rs", 1, 10),
                node("fn:c", "c", GraphNodeType::Function, "a.rs", 1, 10),
            ],
            // a → b → c, listed against alphabetical file order so a
            // path-ordered walk would produce exactly the reverse.
            edges: vec![
                GraphEdge::new("fn:a", "fn:b", GraphEdgeType::Calls),
                GraphEdge::new("fn:b", "fn:c", GraphEdgeType::Calls),
            ],
            stats: None,
            resolution: None,
        };
        let nodes = vec![
            WalkNode {
                id: "fn:c".into(), name: "c".into(), node_type: "Function".into(),
                file: "a.rs".into(), start_line: 1, end_line: 10, description: String::new(),
                role: Role::Changed, status: None, added: 9, removed: 0,
            },
            WalkNode {
                id: "fn:b".into(), name: "b".into(), node_type: "Function".into(),
                file: "m.rs".into(), start_line: 1, end_line: 10, description: String::new(),
                role: Role::Changed, status: None, added: 5, removed: 0,
            },
            WalkNode {
                id: "fn:a".into(), name: "a".into(), node_type: "Function".into(),
                file: "z.rs".into(), start_line: 1, end_line: 10, description: String::new(),
                role: Role::Changed, status: None, added: 1, removed: 0,
            },
        ];
        let ordered = order_by_flow(nodes, &graph);
        let ids: Vec<&str> = ordered.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["fn:a", "fn:b", "fn:c"]);
    }

    /// A call cycle must not drop stops or hang the ordering.
    #[test]
    fn flow_order_survives_a_cycle() {
        let graph = GraphData {
            nodes: vec![],
            edges: vec![
                GraphEdge::new("fn:a", "fn:b", GraphEdgeType::Calls),
                GraphEdge::new("fn:b", "fn:a", GraphEdgeType::Calls),
            ],
            stats: None,
            resolution: None,
        };
        let mk = |id: &str, added: u32| WalkNode {
            id: id.into(), name: id.into(), node_type: "Function".into(),
            file: "a.rs".into(), start_line: 1, end_line: 2, description: String::new(),
            role: Role::Changed, status: None, added, removed: 0,
        };
        let ordered = order_by_flow(vec![mk("fn:a", 5), mk("fn:b", 9)], &graph);
        assert_eq!(ordered.len(), 2, "no stop is lost to a cycle");
        // Neither has indegree 0, so rank order decides — the input order,
        // which is the ranked one.
        assert_eq!(ordered[0].id, "fn:a");
    }

    #[test]
    fn the_ring_separates_tests_from_plain_callers() {
        let mut test_fn = node("fn:t", "t", GraphNodeType::Function, "t.rs", 1, 9);
        test_fn.classification = Some(FileClassification::Test);
        let graph = GraphData {
            nodes: vec![
                node("fn:seed", "seed", GraphNodeType::Function, "a.rs", 1, 9),
                node("fn:caller", "caller", GraphNodeType::Function, "b.rs", 1, 9),
                test_fn,
            ],
            edges: vec![
                GraphEdge::new("fn:caller", "fn:seed", GraphEdgeType::Calls),
                GraphEdge::new("fn:t", "fn:seed", GraphEdgeType::Calls),
                // Containment is not impact; including it would make every
                // changed symbol's own file a caller of it.
                GraphEdge::new("file:a.rs", "fn:seed", GraphEdgeType::Contains),
            ],
            stats: None,
            resolution: None,
        };
        let seeds = vec![WalkNode {
            id: "fn:seed".into(), name: "seed".into(), node_type: "Function".into(),
            file: "a.rs".into(), start_line: 1, end_line: 9, description: String::new(),
            role: Role::Changed, status: None, added: 3, removed: 0,
        }];
        let ring = impact_ring(&graph, &seeds);
        let roles: HashMap<&str, Role> =
            ring.iter().map(|n| (n.id.as_str(), n.role)).collect();
        assert_eq!(ring.len(), 2, "Contains must not contribute a stop");
        assert_eq!(roles.get("fn:caller"), Some(&Role::Caller));
        assert_eq!(roles.get("fn:t"), Some(&Role::Test));
    }

    /// A changed symbol that is also a caller of another changed symbol is
    /// one stop, not two.
    #[test]
    fn the_ring_never_repeats_a_changed_symbol() {
        let graph = GraphData {
            nodes: vec![
                node("fn:a", "a", GraphNodeType::Function, "a.rs", 1, 9),
                node("fn:b", "b", GraphNodeType::Function, "b.rs", 1, 9),
            ],
            edges: vec![GraphEdge::new("fn:a", "fn:b", GraphEdgeType::Calls)],
            stats: None,
            resolution: None,
        };
        let mk = |id: &str| WalkNode {
            id: id.into(), name: id.into(), node_type: "Function".into(),
            file: "x.rs".into(), start_line: 1, end_line: 9, description: String::new(),
            role: Role::Changed, status: None, added: 1, removed: 0,
        };
        assert!(impact_ring(&graph, &[mk("fn:a"), mk("fn:b")]).is_empty());
    }

    #[test]
    fn an_empty_diff_says_so_rather_than_blaming_the_index() {
        let d = summary(vec![]);
        assert!(empty_reason(&d, &[]).contains("Nothing changed"));
    }

    #[test]
    fn an_unindexed_diff_points_at_gen() {
        let d = summary(vec![changed("new.rs", &[(1, 2, 2, 0)])]);
        let reason = empty_reason(&d, &["new.rs".to_string()]);
        assert!(reason.contains("ug gen"), "{reason}");
    }
}
