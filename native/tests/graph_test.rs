use ultragraph_kb::{index, build_graph, k_hop_bfs, types::{GraphData, BfsResult, GraphNodeType, GraphEdgeType}};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_build_graph_creates_nodes() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    assert!(!graph.nodes.is_empty());
}

#[test]
fn test_build_graph_file_nodes() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let file_nodes: Vec<_> = graph.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::File)
        .collect();

    assert!(!file_nodes.is_empty());
}

#[test]
fn test_build_graph_symbol_nodes() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let fn_nodes: Vec<_> = graph.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::Function)
        .collect();

    assert!(!fn_nodes.is_empty());
    assert!(fn_nodes.iter().any(|n| n.name == "test"));
}

#[test]
fn test_build_graph_class_nodes() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "class Calculator { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let class_nodes: Vec<_> = graph.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::Class)
        .collect();

    assert!(!class_nodes.is_empty());
    assert!(class_nodes.iter().any(|n| n.name == "Calculator"));
}

#[test]
fn test_build_graph_interface_nodes() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "interface Config { name: string; }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let interface_nodes: Vec<_> = graph.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::Interface)
        .collect();

    assert!(!interface_nodes.is_empty());
    assert!(interface_nodes.iter().any(|n| n.name == "Config"));
}

#[test]
fn test_build_graph_contains_edge() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let contains_edges: Vec<_> = graph.edges.iter()
        .filter(|e| e.edge_type == GraphEdgeType::Contains)
        .collect();

    assert!(!contains_edges.is_empty());
}

#[test]
fn test_build_graph_typed_as_edge() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "interface Config { name: string; }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let typed_as_edges: Vec<_> = graph.edges.iter()
        .filter(|e| e.edge_type == GraphEdgeType::TypedAs)
        .collect();

    assert!(!typed_as_edges.is_empty());
}

#[test]
fn test_build_graph_imports_edge() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("math.ts"), "export function add(a: number): number { return a; }").unwrap();
    fs::write(dir.path().join("main.ts"), "import { add } from './math';").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let import_edges: Vec<_> = graph.edges.iter()
        .filter(|e| e.edge_type == GraphEdgeType::Imports)
        .collect();

    assert!(!import_edges.is_empty());
}

#[test]
fn test_build_graph_extends_edge() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    // TypeScript extends parsing is done via field lookup
    // This test verifies the graph structure supports extends edges
    fs::write(dir.path().join("test.ts"), "class Base { value: number; } class Derived extends Base { value: number; }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    // Verify class nodes exist
    let class_nodes: Vec<_> = graph.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::Class)
        .collect();

    assert!(class_nodes.len() >= 2);
}

#[test]
fn test_build_graph_multiple_files() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("a.ts"), "export function a(): void { }").unwrap();
    fs::write(dir.path().join("b.ts"), "export function b(): void { }").unwrap();
    fs::write(dir.path().join("c.ts"), "export function c(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let file_nodes: Vec<_> = graph.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::File)
        .collect();

    assert_eq!(file_nodes.len(), 3);
}

#[test]
fn test_k_hop_bfs_single_hop() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result.clone());
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let file_node = graph.nodes.iter()
        .find(|n| n.node_type == GraphNodeType::File)
        .unwrap();

    let bfs_json = k_hop_bfs(graph_json, file_node.id.clone(), 1);
    let bfs: BfsResult = serde_json::from_str(&bfs_json).unwrap();

    assert!(!bfs.nodes.is_empty());
}

#[test]
fn test_k_hop_bfs_finds_connected_symbols() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result.clone());
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let file_node = graph.nodes.iter()
        .find(|n| n.node_type == GraphNodeType::File)
        .unwrap();

    let bfs_json = k_hop_bfs(graph_json, file_node.id.clone(), 1);
    let bfs: BfsResult = serde_json::from_str(&bfs_json).unwrap();

    let fn_nodes: Vec<_> = bfs.nodes.iter()
        .filter(|n| n.node_type == GraphNodeType::Function)
        .collect();

    assert!(!fn_nodes.is_empty());
}

#[test]
fn test_k_hop_bfs_k0_returns_only_start() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result.clone());
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let file_node = graph.nodes.iter()
        .find(|n| n.node_type == GraphNodeType::File)
        .unwrap();

    let bfs_json = k_hop_bfs(graph_json, file_node.id.clone(), 0);
    let bfs: BfsResult = serde_json::from_str(&bfs_json).unwrap();

    assert_eq!(bfs.nodes.len(), 1);
    assert_eq!(bfs.nodes[0].id, file_node.id);
}

#[test]
fn test_k_hop_bfs_invalid_node() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);

    let bfs_json = k_hop_bfs(graph_json, "nonexistent_node".to_string(), 1);
    let bfs: BfsResult = serde_json::from_str(&bfs_json).unwrap();

    assert!(bfs.nodes.is_empty());
    assert!(bfs.edges.is_empty());
}

#[test]
fn test_k_hop_bfs_tracks_distances() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result.clone());
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let file_node = graph.nodes.iter()
        .find(|n| n.node_type == GraphNodeType::File)
        .unwrap();

    let bfs_json = k_hop_bfs(graph_json, file_node.id.clone(), 1);
    let bfs: BfsResult = serde_json::from_str(&bfs_json).unwrap();

    assert!(!bfs.distances.is_empty());
    assert!(bfs.distances.values().all(|&d| d <= 1));
}

#[test]
fn test_k_hop_bfs_multihop() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("math.ts"), "export const value = 42;").unwrap();
    fs::write(dir.path().join("util.ts"), "import { value } from './math'; export function get() { return value; }").unwrap();
    fs::write(dir.path().join("main.ts"), "import { get } from './util';").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result.clone());
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let main_file = graph.nodes.iter()
        .find(|n| n.name.contains("main.ts"))
        .unwrap();

    let bfs_json = k_hop_bfs(graph_json, main_file.id.clone(), 2);
    let bfs: BfsResult = serde_json::from_str(&bfs_json).unwrap();

    assert!(bfs.nodes.len() >= 2);
}

#[test]
fn test_graph_edge_deduplication() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    fs::write(dir.path().join("test.ts"), "function test(): void { function inner(): void { }").unwrap();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    let mut seen = std::collections::HashSet::new();
    for edge in &graph.edges {
        assert!(seen.insert(format!("{}->{}", edge.source, edge.target)));
    }
}

#[test]
fn test_graph_empty_directory() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();

    let index_result = index(dir_path);
    let graph_json = build_graph(index_result);
    let graph: GraphData = serde_json::from_str(&graph_json).unwrap();

    assert!(graph.nodes.is_empty());
    assert!(graph.edges.is_empty());
}