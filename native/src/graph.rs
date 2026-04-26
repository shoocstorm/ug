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

        let file_node_type = match &file.classification {
            Some(crate::types::FileClassification::Config) => GraphNodeType::Config,
            _ => GraphNodeType::File,
        };

        nodes.push(GraphNode {
            id: file_node_id.clone(),
            name: file.path.clone(),
            node_type: file_node_type,
            file: Some(file.path.clone()),
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            docstring: None,
            imports: file.imports.iter().map(|imp| crate::types::GraphNodeImport {
                path: imp.path.clone(),
                imported: imp.imported.iter().map(|i| crate::types::GraphImportedItem {
                    name: i.name.clone(),
                    alias: i.alias.clone(),
                }).collect(),
            }).collect(),
            exports: file.exports.iter().map(|exp| crate::types::GraphNodeExport {
                name: exp.name.clone(),
                alias: exp.alias.clone(),
                is_default: exp.is_default,
            }).collect(),
            extends: vec![],
            implements: vec![],
            calls: vec![],

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

            let signature = sym.signature.as_ref().map(|s| crate::types::GraphNodeSignature {
                params: s.params.iter().map(|p| crate::types::Param {
                    name: p.name.clone(),
                    param_type: p.param_type.clone(),
                    optional: p.optional,
                    default: p.default.clone(),
                }).collect(),
                return_type: s.return_type.clone(),
            });

            let imports = sym.imports.iter().map(|imp| crate::types::GraphNodeImport {
                path: imp.path.clone(),
                imported: imp.imported.iter().map(|i| crate::types::GraphImportedItem {
                    name: i.name.clone(),
                    alias: i.alias.clone(),
                }).collect(),
            }).collect();

            let exports = sym.exports.iter().map(|exp| crate::types::GraphNodeExport {
                name: exp.name.clone(),
                alias: exp.alias.clone(),
                is_default: exp.is_default,
            }).collect();


            nodes.push(GraphNode {
                id: sym_node_id.clone(),
                name: sym.name.clone(),
                node_type,
                file: Some(file.path.clone()),
                start_line: Some(sym.start_line),
                end_line: Some(sym.end_line),
                metrics: sym.metrics.clone(),
                signature,
                docstring: sym.docstring.clone(),
                imports,
                exports,
                extends: sym.extends.clone(),
                implements: sym.implements.clone(),
                calls: sym.calls.clone(),

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


        }

        for import in &file.imports {
            let source_file = file_node_id.clone();

            for imp in &import.imported {
                edges.push(GraphEdge {
                    source: source_file.clone(),
                    target: imp.name.clone(),
                    edge_type: GraphEdgeType::Imports,
                });
            }

            if !import.path.is_empty() {
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
    let (di_graph, index_map) = build_di_graph(graph);

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

#[napi]
pub fn filter_edges_by_type(graph_json: String, edge_types: Vec<String>) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let filtered: Vec<GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| {
            edge_types.iter().any(|t| {
                let et_str = format!("{:?}", e.edge_type);
                et_str.to_lowercase() == t.to_lowercase()
            })
        })
        .cloned()
        .collect();

    let result = crate::types::FilteredEdgesResult {
        count: filtered.len(),
        edges: filtered,
    };

    serde_json::to_string(&result).unwrap_or_default()
}

#[napi]
pub fn find_shortest_path(graph_json: String, source_id: String, target_id: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let (di_graph, index_map) = build_di_graph(&graph);

    let source_idx = match index_map.get(&source_id) {
        Some(idx) => *idx,
        None => {
            let result = crate::types::PathResult {
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
            let result = crate::types::PathResult {
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
            let result = crate::types::PathResult {
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

    let result = crate::types::PathResult {
        path: vec![],
        found: false,
        length: None,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

#[napi]
pub fn calculate_centrality(graph_json: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let n = graph.nodes.len() as f64;
    if n == 0.0 {
        let result = crate::types::CentralityResult {
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
    let (di_graph, index_map) = build_di_graph(&graph);

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

    let result = crate::types::CentralityResult {
        degree_centrality,
        betweenness_centrality: betweenness,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

#[napi]
pub fn detect_cycles(graph_json: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let (di_graph, index_map) = build_di_graph(&graph);
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

    let result = crate::types::CycleResult {
        has_cycles: !unique_cycles.is_empty(),
        cycles: unique_cycles,
    };
    serde_json::to_string(&result).unwrap_or_default()
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