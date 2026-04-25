use crate::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};
use napi_derive::napi;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

fn build_graph_from_index(index_result: &crate::types::IndexResult) -> GraphData {
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut symbol_id_map: HashMap<String, String> = HashMap::new();

    for file in &index_result.files {
        let file_node_id = format!("file:{}", file.path.replace('\\', "/"));

        nodes.push(GraphNode {
            id: file_node_id.clone(),
            name: file.path.clone(),
            node_type: GraphNodeType::File,
            file: Some(file.path.clone()),
            start_line: None,
            end_line: None,
        });

        for sym in &file.symbols {
            let node_type = match sym.kind.as_str() {
                "function" | "function_declaration" | "method_definition" => GraphNodeType::Function,
                "class" | "class_declaration" => GraphNodeType::Class,
                "interface" | "interface_declaration" => GraphNodeType::Interface,
                "variable" | "variable_declaration" => GraphNodeType::Function,
                "type" | "type_alias_declaration" => GraphNodeType::Interface,
                _ => GraphNodeType::Function,
            };

            let sym_node_id = format!("{}:{}", sym.kind, sym.name);

            nodes.push(GraphNode {
                id: sym_node_id.clone(),
                name: sym.name.clone(),
                node_type,
                file: Some(file.path.clone()),
                start_line: Some(sym.start_line),
                end_line: Some(sym.end_line),
            });

            symbol_id_map.insert(sym.name.clone(), sym_node_id.clone());

            edges.push(GraphEdge {
                source: file_node_id.clone(),
                target: sym_node_id.clone(),
                edge_type: GraphEdgeType::Contains,
            });

            for extended in &sym.extends {
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: format!("class:{}", extended),
                    edge_type: GraphEdgeType::Extends,
                });
            }

            for implemented in &sym.implements {
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: format!("interface:{}", implemented),
                    edge_type: GraphEdgeType::Implements,
                });
            }

            for called in &sym.calls {
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: format!("fn:{}", called),
                    edge_type: GraphEdgeType::Calls,
                });
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: format!("class:{}", called),
                    edge_type: GraphEdgeType::Calls,
                });
            }

            for type_ref in &sym.typed_as {
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: type_ref.name.clone(),
                    edge_type: GraphEdgeType::TypedAs,
                });
            }
        }

        for import in &file.imports {
            let source_file = file_node_id.clone();

            for imp in &import.imported {
                let target_sym = if import.is_external {
                    format!("external:{}", imp.name)
                } else {
                    imp.name.clone()
                };

                edges.push(GraphEdge {
                    source: source_file.clone(),
                    target: target_sym,
                    edge_type: GraphEdgeType::Imports,
                });
            }

            if !import.path.is_empty() && !import.is_external {
                let target_file = if import.path.starts_with('.') {
                    let base_dir = std::path::Path::new(&file.path)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let resolved = std::path::Path::new(&base_dir)
                        .join(import.path.trim_start_matches('.'))
                        .to_string_lossy()
                        .to_string();
                    format!("file:{}", resolved)
                } else {
                    format!("file:{}", import.path)
                };

                edges.push(GraphEdge {
                    source: source_file,
                    target: target_file,
                    edge_type: GraphEdgeType::Imports,
                });
            }
        }

        for exp in &file.exports {
            edges.push(GraphEdge {
                source: file_node_id.clone(),
                target: exp.name.clone(),
                edge_type: GraphEdgeType::Exports,
            });
        }
    }

    resolve_cross_file_references(&mut edges, &symbol_id_map);

    dedupe_edges(&mut edges);

    GraphData { nodes, edges }
}

fn resolve_cross_file_references(edges: &mut Vec<GraphEdge>, symbol_id_map: &HashMap<String, String>) {
    let import_edges: Vec<GraphEdge> = edges
        .iter()
        .filter(|e| e.edge_type == GraphEdgeType::Imports)
        .cloned()
        .collect();

    for edge in import_edges {
        if let Some(target_sym_id) = symbol_id_map.get(&edge.target) {
            edges.push(GraphEdge {
                source: edge.source.clone(),
                target: target_sym_id.clone(),
                edge_type: GraphEdgeType::References,
            });
        }
    }
}

fn dedupe_edges(edges: &mut Vec<GraphEdge>) {
    let mut seen: HashMap<(String, String, GraphEdgeType), bool> = HashMap::new();
    edges.retain(|e| {
        let key = (e.source.clone(), e.target.clone(), e.edge_type.clone());
        if seen.contains_key(&key) {
            false
        } else {
            seen.insert(key, true);
            true
        }
    });
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