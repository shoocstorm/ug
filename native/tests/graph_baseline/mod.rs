//! Baselines: the graph entry points exactly as they stood at cdc9a2b,
//! kept only so the bench can put a real before/after ratio in the results
//! log. Do not fix anything here — its value is being unchanged.
//!
//! Note it produced all-zero betweenness (a stale `w_dist` read meant sigma
//! never propagated), so the comparison is "time to produce zeros" against
//! "time to produce correct scores". The old cost was real regardless: the
//! per-source map rebuild and sort ran in full.
#![allow(dead_code)]

use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use ultragraph::types::{GraphData, GraphEdge, GraphNode};

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
            index_map.get(&edge.source),
            index_map.get(&edge.target),
        ) {
            di_graph.add_edge(src_idx, tgt_idx, ());
        }
    }

    (di_graph, index_map)
}

pub fn calculate_centrality_baseline(graph: &GraphData) -> String {
    let n = graph.nodes.len() as f64;
    if n == 0.0 {
        let result = ultragraph::CentralityResult {
            degree_centrality: HashMap::new(),
            betweenness_centrality: HashMap::new(),
        };
        return serde_json::to_string(&result).unwrap_or_default();
    }

    let mut degree_centrality: HashMap<String, f64> = HashMap::new();
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut out_degree: HashMap<String, usize> = HashMap::new();

    for node in &graph.nodes {
        degree_centrality.insert(node.id.clone(), 0.0);
        in_degree.insert(node.id.clone(), 0);
        out_degree.insert(node.id.clone(), 0);
    }

    for edge in &graph.edges {
        if let Some(c) = degree_centrality.get_mut(&edge.source) {
            *c += 1.0;
        }
        if let Some(c) = degree_centrality.get_mut(&edge.target) {
            *c += 1.0;
        }
        if let Some(c) = out_degree.get_mut(&edge.source) {
            *c += 1;
        }
        if let Some(c) = in_degree.get_mut(&edge.target) {
            *c += 1;
        }
    }

    for (_, c) in &mut degree_centrality {
        if n > 1.0 {
            *c /= n - 1.0;
        }
    }

    let mut betweenness: HashMap<String, f64> = HashMap::new();
    for node in &graph.nodes {
        betweenness.insert(node.id.clone(), 0.0);
    }

    if n > 1.0 {
    let (di_graph, index_map) = build_di_graph(graph);

    for node in &graph.nodes {
        let mut pred: HashMap<String, Vec<String>> = HashMap::new();
        let mut dist: HashMap<String, i32> = HashMap::new();
        let mut sigma: HashMap<String, usize> = HashMap::new();
        let mut delta: HashMap<String, f64> = HashMap::new();

        for n in &graph.nodes {
            pred.insert(n.id.clone(), vec![]);
            dist.insert(n.id.clone(), -1);
            sigma.insert(n.id.clone(), 0);
            delta.insert(n.id.clone(), 0.0);
        }
        sigma.insert(node.id.clone(), 1);
        dist.insert(node.id.clone(), 0);

        let source_idx = *index_map.get(&node.id).unwrap();
        let mut queue: Vec<NodeIndex> = vec![source_idx];

        while !queue.is_empty() {
            let v_idx = queue.remove(0);
            let v_id = graph.nodes[v_idx.index()].id.clone();
            let v_dist = *dist.get(&v_id).unwrap();

            for w_idx in di_graph.neighbors(v_idx) {
                let w_id = graph.nodes[w_idx.index()].id.clone();
                let w_dist = *dist.get(&w_id).unwrap();

                if w_dist == -1 {
                    *dist.get_mut(&w_id).unwrap() = v_dist + 1;
                    queue.push(w_idx);
                }

                if v_dist + 1 == w_dist {
                    let sigma_v = *sigma.get(&v_id).unwrap();
                    *sigma.get_mut(&w_id).unwrap() += sigma_v;
                    pred.get_mut(&w_id).unwrap().push(v_id.clone());
                }
            }
        }

        let node_ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
        let mut ordered: Vec<String> = node_ids.iter()
            .filter(|id| *dist.get(*id).unwrap() > 0)
            .cloned()
            .collect();
        ordered.sort_by(|a, b| {
            dist.get(b).unwrap().cmp(dist.get(a).unwrap())
        });

        for w in &ordered {
            for v in pred.get(w).unwrap_or(&vec![]) {
                let sigma_v = *sigma.get(v).unwrap() as f64;
                let sigma_w = *sigma.get(w).unwrap() as f64;
                let delta_v = *delta.get(v).unwrap();
                if sigma_w > 0.0 {
                    let contribution = (sigma_v / sigma_w) * (1.0 + delta_v);
                    *delta.get_mut(w).unwrap() += contribution;
                }
            }
            if w != &node.id {
                *betweenness.get_mut(w).unwrap() += delta.get(w).unwrap();
            }
        }
    }

    let normalizer = (n - 1.0) * (n - 2.0);
    if normalizer > 0.0 {
        for (_, c) in &mut betweenness {
            *c /= normalizer;
        }
    }
    }

    let result = ultragraph::CentralityResult {
        degree_centrality,
        betweenness_centrality: betweenness,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

// ---------- P1.2 / P1.3 baselines ----------

pub fn run_k_hop_bfs_baseline(graph: &GraphData, start_node_id: &str, k: u32) -> ultragraph::BfsResult {
    let (di_graph, index_map) = build_di_graph(graph);

    let start_idx = match index_map.get(start_node_id) {
        Some(idx) => *idx,
        None => {
            return ultragraph::BfsResult {
                nodes: vec![],
                edges: vec![],
                distances: HashMap::new(),
            }
        }
    };

    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut queue: Vec<(NodeIndex, u32)> = vec![(start_idx, 0)];
    let mut visited: HashMap<NodeIndex, bool> = HashMap::new();

    while let Some((node_idx, dist)) = queue.pop() {
        if dist > k {
            continue;
        }
        if visited.get(&node_idx) == Some(&true) {
            continue;
        }
        visited.insert(node_idx, true);

        let node_id = graph.nodes[node_idx.index()].id.clone();
        distances.insert(node_id.clone(), dist);

        for neighbor in di_graph.neighbors(node_idx) {
            if !visited.contains_key(&neighbor) {
                queue.push((neighbor, dist + 1));
            }
        }
    }

    let result_nodes: Vec<GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| distances.contains_key(&n.id))
        .cloned()
        .collect();

    let result_edges: Vec<GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| distances.contains_key(&e.source) && distances.contains_key(&e.target))
        .cloned()
        .collect();

    ultragraph::BfsResult {
        nodes: result_nodes,
        edges: result_edges,
        distances,
    }
}

pub fn find_shortest_path_baseline(graph_json: String, source_id: String, target_id: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let (di_graph, index_map) = build_di_graph(&graph);

    let source_idx = match index_map.get(&source_id) {
        Some(idx) => *idx,
        None => {
            let result = ultragraph::PathResult {
                path: vec![],
                found: false,
                length: None,
            };
            return serde_json::to_string(&result).unwrap_or_default();
        }
    };

    let target_idx = match index_map.get(&target_id) {
        Some(idx) => *idx,
        None => {
            let result = ultragraph::PathResult {
                path: vec![],
                found: false,
                length: None,
            };
            return serde_json::to_string(&result).unwrap_or_default();
        }
    };

    let mut queue: Vec<(NodeIndex, Vec<String>)> = vec![(source_idx, vec![source_id.clone()])];
    let mut visited: HashMap<NodeIndex, bool> = HashMap::new();

    while !queue.is_empty() {
        let (node_idx, path) = queue.remove(0);
        if node_idx == target_idx {
            let path_len = path.len() as u32;
            let result = ultragraph::PathResult {
                path: path.clone(),
                found: true,
                length: Some(path_len - 1),
            };
            return serde_json::to_string(&result).unwrap_or_default();
        }

        if visited.get(&node_idx) == Some(&true) {
            continue;
        }
        visited.insert(node_idx, true);

        for neighbor in di_graph.neighbors(node_idx) {
            if !visited.contains_key(&neighbor) {
                let mut new_path = path.clone();
                let neighbor_id = graph.nodes[neighbor.index()].id.clone();
                new_path.push(neighbor_id);
                queue.push((neighbor, new_path));
            }
        }
    }

    let result = ultragraph::PathResult {
        path: vec![],
        found: false,
        length: None,
    };
    serde_json::to_string(&result).unwrap_or_default()
}
