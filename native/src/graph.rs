use crate::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};
use napi_derive::napi;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

fn build_graph_from_index(index_result: &crate::types::IndexResult) -> GraphData {
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    for file in &index_result.files {
        let file_node_id = format!("file:{}", file.path);

        nodes.push(GraphNode {
            id: file_node_id.clone(),
            name: file.path.clone(),
            node_type: GraphNodeType::File,
            file: Some(file.path.clone()),
            start_line: None,
            end_line: None,
        });

        for symbol in &file.symbols {
            let node_type = match symbol.kind.as_str() {
                "function" | "function_declaration" | "method_definition" => GraphNodeType::Function,
                "class" | "class_declaration" => GraphNodeType::Class,
                "interface" | "interface_declaration" => GraphNodeType::Interface,
                _ => GraphNodeType::Function,
            };

            let sym_node_id = format!("{}:{}", symbol.kind, symbol.name);

            nodes.push(GraphNode {
                id: sym_node_id.clone(),
                name: symbol.name.clone(),
                node_type,
                file: Some(file.path.clone()),
                start_line: Some(symbol.start_line),
                end_line: Some(symbol.end_line),
            });

            edges.push(GraphEdge {
                source: file_node_id.clone(),
                target: sym_node_id,
                edge_type: GraphEdgeType::Contains,
            });
        }
    }

    GraphData { nodes, edges }
}

fn run_k_hop_bfs(graph: &GraphData, start_node_id: &str, k: u32) -> crate::types::BfsResult {
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

    let start_idx = match index_map.get(start_node_id) {
        Some(idx) => *idx,
        None => {
            return crate::types::BfsResult {
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

    crate::types::BfsResult {
        nodes: result_nodes,
        edges: result_edges,
        distances,
    }
}

#[napi]
pub fn build_graph(index_json: String) -> String {
    let index_result: crate::types::IndexResult = match serde_json::from_str(&index_json) {
        Ok(r) => r,
        Err(_) => return "{}".to_string(),
    };

    let graph = build_graph_from_index(&index_result);
    serde_json::to_string(&graph).unwrap_or_default()
}

#[napi]
pub fn k_hop_bfs(graph_json: String, start_node_id: String, k: u32) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let result = run_k_hop_bfs(&graph, &start_node_id, k);
    serde_json::to_string(&result).unwrap_or_default()
}