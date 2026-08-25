//! Reading a finished [`GraphData`] back: traversal, paths, search,
//! centrality, cycles — and the result shapes they return.
//!
//! This is the half of `graph` that *queries* the graph. It assumes a built
//! [`GraphData`] and never constructs one; construction is [`super::build`].
//! `cli/graph_algos.rs` is the CLI face of this module.
//!
//! Two different adjacency structures live here on purpose: [`EdgeAdj`] for
//! the traversal entry points and [`Csr`] for centrality. The doc comment on
//! each explains why one cannot serve both.

use crate::types::{GraphData, GraphEdge, GraphNode};
use petgraph::graph::{DiGraph, NodeIndex};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Node-index adjacency for the traversal entry points, built once per call.
///
/// Distinct from [`Csr`], which centrality uses: that one stores *deduped
/// target node indices*, because two relationships between the same pair are
/// one route and counting them twice corrupts σ. These traversals need the
/// opposite — real **edge indices**, so `k`-hop can hand back the actual edge
/// objects it reached — and they do not care about duplicates, since BFS
/// visits a node once however many edges point at it.
struct EdgeAdj<'a> {
    id_to_idx: HashMap<&'a str, u32>,
    /// CSR over outgoing *target node indices*, for walking forward.
    out_offsets: Vec<usize>,
    out_targets: Vec<u32>,
    /// CSR over incident (in **and** out) *edge indices*, for reading back
    /// the edges among a set of reached nodes without touching the rest.
    inc_offsets: Vec<usize>,
    inc_edges: Vec<u32>,
}

impl<'a> EdgeAdj<'a> {
    fn build(graph: &'a GraphData) -> Self {
        let n = graph.nodes.len();
        // Last duplicate id wins, matching `build_di_graph`'s `collect()`.
        let mut id_to_idx: HashMap<&str, u32> = HashMap::with_capacity(n);
        for (i, node) in graph.nodes.iter().enumerate() {
            id_to_idx.insert(node.id.as_str(), i as u32);
        }

        let mut out_lists: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut inc_lists: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (ei, e) in graph.edges.iter().enumerate() {
            let (Some(&sx), Some(&tx)) = (
                id_to_idx.get(&*e.source),
                id_to_idx.get(&*e.target),
            ) else {
                // An edge naming something outside the node set — dropped,
                // exactly as `build_di_graph` dropped it.
                continue;
            };
            out_lists[sx as usize].push(tx);
            inc_lists[sx as usize].push(ei as u32);
            if sx != tx {
                inc_lists[tx as usize].push(ei as u32);
            }
        }

        let flatten = |lists: Vec<Vec<u32>>| {
            let mut offsets = Vec::with_capacity(n + 1);
            let mut flat = Vec::new();
            offsets.push(0);
            for l in lists {
                flat.extend_from_slice(&l);
                offsets.push(flat.len());
            }
            (offsets, flat)
        };
        let (out_offsets, out_targets) = flatten(out_lists);
        let (inc_offsets, inc_edges) = flatten(inc_lists);

        EdgeAdj { id_to_idx, out_offsets, out_targets, inc_offsets, inc_edges }
    }

    #[inline]
    fn out(&self, i: usize) -> &[u32] {
        &self.out_targets[self.out_offsets[i]..self.out_offsets[i + 1]]
    }

    #[inline]
    fn incident(&self, i: usize) -> &[u32] {
        &self.inc_edges[self.inc_offsets[i]..self.inc_offsets[i + 1]]
    }
}

/// Shortest directed path between two nodes.
///
/// Breadth-first with a predecessor array, reconstructing the path once at
/// the end.
///
/// Replaces a BFS that carried a full `Vec<String>` of the path along with
/// *every queued node* — cloning it per edge — dequeued with
/// `Vec::remove(0)`, and marked `visited` on dequeue rather than enqueue, so
/// the same node was queued once per edge pointing at it. This is the shape
/// `serve::api_path` already used.
pub fn find_shortest_path(
    graph: &GraphData,
    source_id: &str,
    target_id: &str,
) -> PathResult {
    let not_found = || PathResult { path: vec![], found: false, length: None };

    let adj = EdgeAdj::build(graph);
    let (Some(&src), Some(&tgt)) = (
        adj.id_to_idx.get(source_id),
        adj.id_to_idx.get(target_id),
    ) else {
        return not_found();
    };
    let (src, tgt) = (src as usize, tgt as usize);

    let n = graph.nodes.len();
    let mut prev: Vec<Option<usize>> = vec![None; n];
    let mut visited: Vec<bool> = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    visited[src] = true;
    queue.push_back(src);

    let mut found = false;
    while let Some(cur) = queue.pop_front() {
        if cur == tgt {
            found = true;
            break;
        }
        for &w in adj.out(cur) {
            let wi = w as usize;
            if !visited[wi] {
                visited[wi] = true;
                prev[wi] = Some(cur);
                queue.push_back(wi);
            }
        }
    }

    if !found {
        return not_found();
    }

    // Walk the predecessors back from the target. Bounded by `n` because
    // every step moves to a strictly earlier BFS layer.
    let mut path_idx: Vec<usize> = Vec::new();
    let mut cur = tgt;
    loop {
        path_idx.push(cur);
        if cur == src {
            break;
        }
        match prev[cur] {
            Some(p) => cur = p,
            None => return not_found(),
        }
    }
    path_idx.reverse();

    let path: Vec<String> = path_idx
        .iter()
        .map(|&i| graph.nodes[i].id.clone())
        .collect();
    let length = (path.len() as u32).saturating_sub(1);

    PathResult { path, found: true, length: Some(length) }
}

/// Every node within `k` directed hops of `start_node_id`, plus the edges
/// induced among them.
///
/// Was a `Vec::pop` "BFS" — LIFO, with `visited` marked on pop — which made it
/// a depth-first walk that could record a longer distance for a node it
/// reached the long way round first. It then selected its results by scanning
/// the whole node list and the whole edge list, so a 1-hop question cost
/// O(V + E). Both are fixed the way `serve::api_traverse` already had them:
/// a real queue marking on enqueue, and edges read off the reached nodes'
/// incident lists.
pub fn k_hop_bfs(graph: &GraphData, start_node_id: &str, k: u32) -> BfsResult {
    let empty = || BfsResult {
        nodes: vec![],
        edges: vec![],
        distances: HashMap::new(),
    };

    let adj = EdgeAdj::build(graph);
    let Some(&start) = adj.id_to_idx.get(start_node_id) else {
        return empty();
    };

    let n = graph.nodes.len();
    let mut dist: Vec<Option<u32>> = vec![None; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    dist[start as usize] = Some(0);
    queue.push_back(start as usize);

    while let Some(v) = queue.pop_front() {
        let d = dist[v].expect("queued nodes carry a distance");
        // Nodes exactly `k` out are part of the answer but are not expanded.
        if d == k {
            continue;
        }
        for &w in adj.out(v) {
            let wi = w as usize;
            if dist[wi].is_none() {
                dist[wi] = Some(d + 1);
                queue.push_back(wi);
            }
        }
    }

    let reached: Vec<usize> = (0..n).filter(|&i| dist[i].is_some()).collect();

    // Induced edges: both endpoints reached. Gathered from the reached nodes'
    // own incident lists rather than by filtering every edge in the graph —
    // O(reached degree) instead of O(E), which on a large repo is the
    // difference between a handful of lookups and three quarters of a million.
    // Collected by index then sorted, so the result keeps `graph.edges` order
    // rather than inheriting the traversal's.
    let mut edge_idx: Vec<u32> = reached
        .iter()
        .flat_map(|&i| adj.incident(i))
        .copied()
        .filter(|&ei| {
            let e = &graph.edges[ei as usize];
            matches!(
                (
                    adj.id_to_idx.get(&*e.source),
                    adj.id_to_idx.get(&*e.target),
                ),
                (Some(&si), Some(&ti))
                    if dist[si as usize].is_some() && dist[ti as usize].is_some()
            )
        })
        .collect();
    edge_idx.sort_unstable();
    edge_idx.dedup();

    BfsResult {
        nodes: reached.iter().map(|&i| graph.nodes[i].clone()).collect(),
        edges: edge_idx
            .iter()
            .map(|&ei| graph.edges[ei as usize].clone())
            .collect(),
        distances: reached
            .iter()
            .map(|&i| (graph.nodes[i].id.clone(), dist[i].expect("reached")))
            .collect(),
    }
}

fn build_di_graph(graph: &GraphData) -> (DiGraph<(), ()>, HashMap<String, NodeIndex>) {
    let mut di_graph: DiGraph<(), ()> = DiGraph::new();
    let index_map: HashMap<String, NodeIndex> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), NodeIndex::new(i)))
        .collect();

    for _ in &graph.nodes {
        di_graph.add_node(());
    }

    for edge in &graph.edges {
        if let (Some(&src_idx), Some(&tgt_idx)) = (
            index_map.get(&*edge.source),
            index_map.get(&*edge.target),
        ) {
            di_graph.add_edge(src_idx, tgt_idx, ());
        }
    }

    (di_graph, index_map)
}

pub fn filter_edges_by_type(graph_json: String, edge_types: Vec<String>) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let filtered: Vec<GraphEdge> = graph
        .edges
        .iter()
        // `as_str()` against the static name, and the requested types
        // lowercased once by the caller above — this allocated two `String`s
        // per (edge × requested type), which on a large graph is millions of
        // allocations to compare a handful of fixed names. The same fix P3.3
        // made to `/api/graph/stats`. See P10.10 in
        // docs/dev/PERF-TUNING-JOURNEY.md.
        .filter(|e| {
            let et = e.edge_type.as_str();
            edge_types.iter().any(|t| t.eq_ignore_ascii_case(et))
        })
        .cloned()
        .collect();

    let result = FilteredEdgesResult {
        count: filtered.len(),
        edges: filtered,
    };

    serde_json::to_string(&result).unwrap_or_default()
}

/// Keyword-based search over graph nodes. Matches `keyword` (case-insensitive
/// substring) against each node's `name` and `docstring`. When `node_types`
/// is provided and non-empty, only nodes whose `node_type` (lowercased) is in
/// the list are considered. An empty `keyword` returns every node that passes
/// the type filter.
pub fn graph_keyword_search(
    graph_json: String,
    keyword: String,
    node_types: Option<Vec<String>>,
) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let needle = keyword.to_lowercase();
    let type_filter: Option<Vec<String>> = node_types
        .map(|v| v.into_iter().map(|t| t.to_lowercase()).collect::<Vec<_>>())
        .filter(|v| !v.is_empty());

    let matched: Vec<GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| {
            if let Some(types) = &type_filter {
                // As in `filter_edges_by_type`: the static name, not a
                // `Debug`-formatted clone per node.
                let nt = n.node_type.as_str();
                if !types.iter().any(|t| t.eq_ignore_ascii_case(nt)) {
                    return false;
                }
            }

            if needle.is_empty() {
                return true;
            }

            let name_match = n.name.to_lowercase().contains(&needle);
            let doc_match = n
                .docstring
                .as_ref()
                .map(|d| d.to_lowercase().contains(&needle))
                .unwrap_or(false);

            name_match || doc_match
        })
        .cloned()
        .collect();

    let result = SearchResult {
        count: matched.len(),
        nodes: matched,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

/// Forward adjacency in compressed-sparse-row form, over node *indices*.
///
/// Brandes walks the whole adjacency once per source node, so the layout it
/// reads from is the hot structure in the whole computation: one flat
/// `targets` array read sequentially beats a `Vec<Vec<_>>` (a pointer chase
/// per node) and beats petgraph's edge list (a pointer chase per edge).
struct Csr {
    /// `offsets[i]..offsets[i + 1]` is node `i`'s slice of `targets`.
    offsets: Vec<usize>,
    targets: Vec<u32>,
}

impl Csr {
    /// Build forward adjacency, dropping edges whose endpoints aren't nodes.
    ///
    /// Parallel edges are collapsed. `build_graph` already dedupes by
    /// (source, target, type), but two *different* types between the same
    /// pair — `A Calls B` and `A Uses B` — still arrive as two edges, and to
    /// a shortest-path count those would read as two distinct routes and
    /// double σ. One hop between two nodes is one route however many
    /// relationships it stands for.
    fn build(graph: &GraphData, id_to_idx: &HashMap<&str, u32>, n: usize) -> Self {
        let mut lists: Vec<Vec<u32>> = vec![Vec::new(); n];
        for e in &graph.edges {
            if let (Some(&s), Some(&t)) = (
                id_to_idx.get(&*e.source),
                id_to_idx.get(&*e.target),
            ) {
                lists[s as usize].push(t);
            }
        }

        let mut offsets = Vec::with_capacity(n + 1);
        let mut targets = Vec::new();
        offsets.push(0);
        for list in lists.iter_mut() {
            list.sort_unstable();
            list.dedup();
            targets.extend_from_slice(list);
            offsets.push(targets.len());
        }

        Csr { offsets, targets }
    }

    #[inline]
    fn neighbors(&self, i: usize) -> &[u32] {
        &self.targets[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// Per-worker scratch for Brandes, allocated once and reset per source.
///
/// The reset is a `fill` (a memset) rather than `n` map insertions, and
/// `pred`'s inner vectors are `clear`ed so they keep their capacity — after
/// the first few sources the whole traversal is allocation-free.
struct BrandesScratch {
    dist: Vec<i32>,
    sigma: Vec<f64>,
    delta: Vec<f64>,
    pred: Vec<Vec<u32>>,
    /// Nodes in BFS discovery order. Popping this is what gives the
    /// non-increasing-distance order the accumulation needs, so no sort.
    stack: Vec<u32>,
    queue: VecDeque<u32>,
    /// Running betweenness for every source this worker has handled.
    betweenness: Vec<f64>,
}

impl BrandesScratch {
    fn new(n: usize) -> Self {
        BrandesScratch {
            dist: vec![-1; n],
            sigma: vec![0.0; n],
            delta: vec![0.0; n],
            pred: vec![Vec::new(); n],
            stack: Vec::with_capacity(n),
            queue: VecDeque::with_capacity(n),
            betweenness: vec![0.0; n],
        }
    }

    /// One source's contribution, accumulated into `self.betweenness`.
    fn run_source(&mut self, s: usize, csr: &Csr) {
        self.dist.fill(-1);
        self.sigma.fill(0.0);
        self.delta.fill(0.0);
        for p in self.pred.iter_mut() {
            p.clear();
        }
        self.stack.clear();
        self.queue.clear();

        self.sigma[s] = 1.0;
        self.dist[s] = 0;
        self.queue.push_back(s as u32);

        while let Some(v) = self.queue.pop_front() {
            let vi = v as usize;
            self.stack.push(v);
            let dv = self.dist[vi];
            let sigma_v = self.sigma[vi];
            for &w in csr.neighbors(vi) {
                let wi = w as usize;
                // First time seen: this is a shortest path to w by BFS order.
                if self.dist[wi] < 0 {
                    self.dist[wi] = dv + 1;
                    self.queue.push_back(w);
                }
                // Re-read `dist[wi]`, never a copy taken before the line
                // above — reading it once up front is what made the previous
                // implementation compare against a stale `-1`, so σ never
                // propagated and every betweenness score came out zero.
                if self.dist[wi] == dv + 1 {
                    self.sigma[wi] += sigma_v;
                    self.pred[wi].push(v);
                }
            }
        }

        // Dependency accumulation, in reverse BFS order. Every node on the
        // stack was reached, so σ[w] ≥ 1 and the division is safe.
        while let Some(w) = self.stack.pop() {
            let wi = w as usize;
            let coeff = (1.0 + self.delta[wi]) / self.sigma[wi];
            for &v in &self.pred[wi] {
                self.delta[v as usize] += self.sigma[v as usize] * coeff;
            }
            if wi != s {
                self.betweenness[wi] += self.delta[wi];
            }
        }
    }
}

/// Degree + Brandes betweenness centrality over an already-parsed graph.
///
/// Betweenness is directed, counts ordered pairs `(s, t)` with `s != t` and
/// neither equal to the scored node, and is normalized by `(n-1)(n-2)`.
///
/// This is O(V·E) and runs to completion on the calling thread; async callers
/// must push it onto `spawn_blocking` rather than awaiting it inline. Sources
/// are scored across rayon's pool, so it will use every core it can get.
///
/// Everything below indexes nodes by position rather than by id string. The
/// previous implementation rebuilt four `HashMap<String, _>` covering the
/// whole graph *per source node* and cloned an id on every edge relaxation —
/// O(V²) allocations before any arithmetic. See P1.1 in
/// `docs/dev/PERF-TUNING-JOURNEY.md`.
pub fn calculate_centrality(graph: &GraphData) -> CentralityResult {
    let n = graph.nodes.len();
    if n == 0 {
        return CentralityResult {
            degree_centrality: HashMap::new(),
            betweenness_centrality: HashMap::new(),
        };
    }
    let nf = n as f64;

    // Last duplicate id wins, matching what `build_di_graph`'s `collect()`
    // has always done for a graph that somehow carries two nodes with one id.
    let mut id_to_idx: HashMap<&str, u32> = HashMap::with_capacity(n);
    for (i, node) in graph.nodes.iter().enumerate() {
        id_to_idx.insert(node.id.as_str(), i as u32);
    }

    // Degree: both endpoints of every edge whose endpoints both resolve.
    let mut degree: Vec<f64> = vec![0.0; n];
    for e in &graph.edges {
        if let Some(&s) = id_to_idx.get(&*e.source) {
            degree[s as usize] += 1.0;
        }
        if let Some(&t) = id_to_idx.get(&*e.target) {
            degree[t as usize] += 1.0;
        }
    }
    if n > 1 {
        for d in degree.iter_mut() {
            *d /= nf - 1.0;
        }
    }

    let mut betweenness: Vec<f64> = vec![0.0; n];
    if n > 1 {
        let csr = Csr::build(graph, &id_to_idx, n);

        // One scratch buffer per worker, reused across every source that
        // worker handles, so the O(V)-sized allocations happen `threads`
        // times rather than once per source.
        //
        // Partitioned by stride rather than by contiguous block, and *not*
        // through `par_iter().fold()`: fold builds one accumulator per split
        // chunk, and rayon chooses how many of those to make, so a scratch
        // that costs ~8 MB at 162k nodes would be allocated an unbounded
        // number of times. Striding fixes the count at `threads` while still
        // spreading the expensive sources — the ones inside a large connected
        // component — evenly, which a contiguous split would pile onto
        // whichever worker drew that range.
        let threads = rayon::current_num_threads().max(1);
        betweenness = (0..threads)
            .into_par_iter()
            .map(|t| {
                let mut scratch = BrandesScratch::new(n);
                let mut s = t;
                while s < n {
                    scratch.run_source(s, &csr);
                    s += threads;
                }
                scratch.betweenness
            })
            .reduce(
                || vec![0.0; n],
                |mut acc, part| {
                    for (a, b) in acc.iter_mut().zip(part) {
                        *a += b;
                    }
                    acc
                },
            );

        let normalizer = (nf - 1.0) * (nf - 2.0);
        if normalizer > 0.0 {
            for b in betweenness.iter_mut() {
                *b /= normalizer;
            }
        }
    }

    // Back to id-keyed maps for the wire format. Duplicate ids collapse with
    // the last one winning, as they did before.
    let mut degree_centrality: HashMap<String, f64> = HashMap::with_capacity(n);
    let mut betweenness_centrality: HashMap<String, f64> = HashMap::with_capacity(n);
    for (i, node) in graph.nodes.iter().enumerate() {
        degree_centrality.insert(node.id.clone(), degree[i]);
        betweenness_centrality.insert(node.id.clone(), betweenness[i]);
    }

    CentralityResult {
        degree_centrality,
        betweenness_centrality,
    }
}

/// Cycle detection over an already-parsed graph. Like
/// [`calculate_centrality`], this is CPU-bound and must not be awaited
/// inline on an async runtime thread.
pub fn detect_cycles(graph: &GraphData) -> CycleResult {
    let (di_graph, index_map) = build_di_graph(graph);
    let mut visited: HashMap<String, bool> = HashMap::new();
    let mut rec_stack: HashMap<String, bool> = HashMap::new();
    let mut cycles: Vec<Vec<String>> = vec![];

    for node in &graph.nodes {
        if !visited.contains_key(&node.id) {
            detect_cycles_dfs(
                &di_graph,
                &graph.nodes,
                &index_map,
                &node.id,
                &mut visited,
                &mut rec_stack,
                &mut vec![],
                &mut cycles,
            );
        }
    }

    let unique_cycles: Vec<Vec<String>> = cycles
        .into_iter()
        .map(|mut c| { c.sort(); c })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    CycleResult {
        has_cycles: !unique_cycles.is_empty(),
        cycles: unique_cycles,
    }
}

fn detect_cycles_dfs(
    di_graph: &DiGraph<(), ()>,
    nodes: &[GraphNode],
    index_map: &HashMap<String, NodeIndex>,
    node_id: &str,
    visited: &mut HashMap<String, bool>,
    rec_stack: &mut HashMap<String, bool>,
    path: &mut Vec<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    visited.insert(node_id.to_string(), true);
    rec_stack.insert(node_id.to_string(), true);
    path.push(node_id.to_string());

    if let Some(&idx) = index_map.get(node_id) {
        for neighbor_idx in di_graph.neighbors(idx) {
            let neighbor_id = nodes[neighbor_idx.index()].id.clone();

            if !visited.contains_key(&neighbor_id) {
                detect_cycles_dfs(
                    di_graph, nodes, index_map,
                    &neighbor_id, visited, rec_stack, path, cycles,
                );
            } else if rec_stack.get(&neighbor_id) == Some(&true) {
                let mut cycle = vec![];
                let start_pos = path.iter().position(|n| n == &neighbor_id).unwrap();
                for (i, n) in path.iter().enumerate() {
                    if i >= start_pos {
                        cycle.push(n.clone());
                    }
                }
                cycle.push(neighbor_id.clone());
                cycles.push(cycle);
            }
        }
    }

    path.pop();
    rec_stack.insert(node_id.to_string(), false);
}
// ---------------------------------------------------------------------------
// Algorithm results
// ---------------------------------------------------------------------------
//
// The return shapes of the functions above. They live beside the algorithms
// that build them rather than in `types.rs`, which describes the *graph* —
// nodes, edges, the indexer output — not what querying it produces.
// `lib.rs` re-exports them, so `ultragraph::BfsResult` still resolves.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BfsResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub distances: std::collections::HashMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathResult {
    pub path: Vec<String>,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CentralityResult {
    pub degree_centrality: std::collections::HashMap<String, f64>,
    pub betweenness_centrality: std::collections::HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    pub has_cycles: bool,
    pub cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredEdgesResult {
    pub edges: Vec<GraphEdge>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub nodes: Vec<GraphNode>,
    pub count: usize,
}
