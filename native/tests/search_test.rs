use ultragraph_kb::{
    build_graph, graph_keyword_search, index,
    types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType, SearchResult},
};
use std::fs;
use tempfile::TempDir;

fn build_test_graph_from_ts(files: &[(&str, &str)]) -> String {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_string_lossy().to_string();
    for (name, src) in files {
        fs::write(dir.path().join(name), src).unwrap();
    }
    let index_result = index(dir_path);
    build_graph(index_result)
}

fn synthetic_graph_json() -> String {
    let nodes = vec![
        GraphNode {
            id: "function:loadConfig".to_string(),
            name: "loadConfig".to_string(),
            node_type: GraphNodeType::Function,
            file: Some("config.ts".to_string()),
            start_line: Some(1),
            end_line: Some(5),
            metrics: None,
            signature: None,
            docstring: Some("Loads configuration from disk".to_string()),
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
        },
        GraphNode {
            id: "class:ConfigStore".to_string(),
            name: "ConfigStore".to_string(),
            node_type: GraphNodeType::Class,
            file: Some("config.ts".to_string()),
            start_line: Some(7),
            end_line: Some(20),
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
        },
        GraphNode {
            id: "interface:Settings".to_string(),
            name: "Settings".to_string(),
            node_type: GraphNodeType::Interface,
            file: Some("config.ts".to_string()),
            start_line: Some(22),
            end_line: Some(25),
            metrics: None,
            signature: None,
            docstring: Some("User-facing settings shape".to_string()),
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
        },
        GraphNode {
            id: "file:src/handler.ts".to_string(),
            name: "src/handler.ts".to_string(),
            node_type: GraphNodeType::File,
            file: Some("src/handler.ts".to_string()),
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
        },
    ];
    let edges: Vec<GraphEdge> = vec![GraphEdge {
        source: "file:src/handler.ts".to_string(),
        target: "function:loadConfig".to_string(),
        edge_type: GraphEdgeType::Contains,
    }];
    let graph = GraphData { nodes, edges };
    serde_json::to_string(&graph).unwrap()
}

#[test]
fn test_search_by_name_match() {
    let graph_json = build_test_graph_from_ts(&[(
        "calc.ts",
        "function calculateTotal(): number { return 0; } function helper(): void {}",
    )]);

    let result_json = graph_keyword_search(graph_json, "calculate".to_string(), None);
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.count >= 1);
    assert!(result.nodes.iter().any(|n| n.name == "calculateTotal"));
    assert!(!result.nodes.iter().any(|n| n.name == "helper"));
}

#[test]
fn test_search_is_case_insensitive() {
    let graph_json = build_test_graph_from_ts(&[(
        "user.ts",
        "function getUserName(): string { return ''; }",
    )]);

    let result_json = graph_keyword_search(graph_json, "USERNAME".to_string(), None);
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.nodes.iter().any(|n| n.name == "getUserName"));
}

#[test]
fn test_search_partial_substring_matches() {
    let graph_json = build_test_graph_from_ts(&[(
        "auth.ts",
        "function authenticateUser(): void {}",
    )]);

    let result_json = graph_keyword_search(graph_json, "auth".to_string(), None);
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.nodes.iter().any(|n| n.name == "authenticateUser"));
}

#[test]
fn test_search_no_match_returns_empty() {
    let graph_json = build_test_graph_from_ts(&[(
        "test.ts",
        "function alpha(): void {} function beta(): void {}",
    )]);

    let result_json = graph_keyword_search(graph_json, "zzz_no_such_thing".to_string(), None);
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert_eq!(result.count, 0);
    assert!(result.nodes.is_empty());
}

#[test]
fn test_search_node_types_restricts_scope() {
    let graph_json = build_test_graph_from_ts(&[(
        "shared.ts",
        "function Config(): void {} class Config {} interface Config {}",
    )]);

    let result_json = graph_keyword_search(
        graph_json,
        "Config".to_string(),
        Some(vec!["class".to_string()]),
    );
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.count >= 1);
    assert!(result
        .nodes
        .iter()
        .all(|n| n.node_type == GraphNodeType::Class));
}

#[test]
fn test_search_node_types_multiple() {
    let graph_json = build_test_graph_from_ts(&[(
        "shared.ts",
        "function Config(): void {} class ConfigStore {} interface ConfigShape {}",
    )]);

    let result_json = graph_keyword_search(
        graph_json,
        "Config".to_string(),
        Some(vec!["class".to_string(), "interface".to_string()]),
    );
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.count >= 2);
    for n in &result.nodes {
        assert!(matches!(
            n.node_type,
            GraphNodeType::Class | GraphNodeType::Interface
        ));
    }
}

#[test]
fn test_search_empty_node_types_treated_as_unfiltered() {
    let graph_json = build_test_graph_from_ts(&[(
        "test.ts",
        "function widget(): void {} class WidgetBox {}",
    )]);

    let unfiltered = graph_keyword_search(graph_json.clone(), "widget".to_string(), None);
    let empty_filter = graph_keyword_search(graph_json, "widget".to_string(), Some(vec![]));

    let r_unfiltered: SearchResult = serde_json::from_str(&unfiltered).unwrap();
    let r_empty: SearchResult = serde_json::from_str(&empty_filter).unwrap();

    assert_eq!(r_unfiltered.count, r_empty.count);
}

#[test]
fn test_search_empty_keyword_returns_all_passing_type_filter() {
    let graph_json = build_test_graph_from_ts(&[(
        "many.ts",
        "function a(): void {} function b(): void {} class C {}",
    )]);

    let result_json = graph_keyword_search(
        graph_json,
        "".to_string(),
        Some(vec!["function".to_string()]),
    );
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.count >= 2);
    assert!(result
        .nodes
        .iter()
        .all(|n| n.node_type == GraphNodeType::Function));
}

#[test]
fn test_search_invalid_graph_json_returns_empty_object() {
    let result = graph_keyword_search("not json".to_string(), "anything".to_string(), None);
    assert_eq!(result, "{}");
}

#[test]
fn test_search_matches_docstring() {
    let graph_json = synthetic_graph_json();
    let result_json = graph_keyword_search(graph_json, "configuration".to_string(), None);
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.nodes.iter().any(|n| n.id == "function:loadConfig"));
}

#[test]
fn test_search_unknown_node_type_filters_everything_out() {
    let graph_json = synthetic_graph_json();
    let result_json = graph_keyword_search(
        graph_json,
        "Config".to_string(),
        Some(vec!["nonexistent_type".to_string()]),
    );
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert_eq!(result.count, 0);
}

#[test]
fn test_search_node_types_is_case_insensitive() {
    let graph_json = synthetic_graph_json();
    let result_json = graph_keyword_search(
        graph_json,
        "Settings".to_string(),
        Some(vec!["INTERFACE".to_string()]),
    );
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.nodes.iter().any(|n| n.id == "interface:Settings"));
}

#[test]
fn test_search_file_node_by_path_substring() {
    let graph_json = synthetic_graph_json();
    let result_json = graph_keyword_search(
        graph_json,
        "handler".to_string(),
        Some(vec!["file".to_string()]),
    );
    let result: SearchResult = serde_json::from_str(&result_json).unwrap();

    assert!(result.nodes.iter().any(|n| n.id == "file:src/handler.ts"));
}
