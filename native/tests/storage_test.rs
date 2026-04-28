use tempfile::TempDir;
use ultragraph_kb::storage::db::{
    edges_from, edges_schema, nodes_by_ids, nodes_schema, vector_search, Db, EdgeRow, NodeRow,
};
use ultragraph_kb::storage::embed::EMBEDDING_DIM;
use ultragraph_kb::storage::text::{build_node_text, collect_related_names};
use ultragraph_kb::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};

fn make_graph() -> GraphData {
    let nodes = vec![
        GraphNode {
            id: "file:src/a.ts".to_string(),
            name: "a.ts".to_string(),
            node_type: GraphNodeType::File,
            file: Some("src/a.ts".to_string()),
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            docstring: Some("entry module".to_string()),
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
        },
        GraphNode {
            id: "function:src/a.ts:1:greet".to_string(),
            name: "greet".to_string(),
            node_type: GraphNodeType::Function,
            file: Some("src/a.ts".to_string()),
            start_line: Some(1),
            end_line: Some(3),
            metrics: None,
            signature: None,
            docstring: Some("Say hello to the user.".to_string()),
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
        },
    ];
    let edges = vec![GraphEdge {
        source: "file:src/a.ts".to_string(),
        target: "function:src/a.ts:1:greet".to_string(),
        edge_type: GraphEdgeType::Contains,
    }];
    GraphData { nodes, edges }
}

#[test]
fn build_node_text_uses_type_name_and_related() {
    let graph = make_graph();
    let related = collect_related_names(&graph);
    let names = related
        .get("file:src/a.ts")
        .cloned()
        .unwrap_or_default();
    let text = build_node_text(&graph.nodes[0], &names);
    assert!(text.starts_with("File: a.ts."));
    assert!(text.contains("entry module"));
    assert!(text.contains("Related: greet"));
}

#[test]
fn collect_related_names_is_bidirectional() {
    let graph = make_graph();
    let related = collect_related_names(&graph);
    let parent = related
        .get("function:src/a.ts:1:greet")
        .cloned()
        .unwrap_or_default();
    assert!(parent.contains(&"a.ts".to_string()));
}

#[test]
fn nodes_schema_has_fixed_size_vector_column() {
    let schema = nodes_schema();
    let field = schema.field_with_name("vector").expect("vector field");
    match field.data_type() {
        arrow_schema::DataType::FixedSizeList(_, n) => {
            assert_eq!(*n, EMBEDDING_DIM as i32);
        }
        other => panic!("expected FixedSizeList, got {:?}", other),
    }
}

#[test]
fn edges_schema_has_no_vector_column() {
    let schema = edges_schema();
    assert!(schema.field_with_name("vector").is_err());
    for required in ["id", "source", "target", "edge_type"] {
        schema
            .field_with_name(required)
            .unwrap_or_else(|_| panic!("missing field {}", required));
    }
}

fn sample_node(id: &str, value: f32) -> NodeRow {
    NodeRow {
        id: id.to_string(),
        name: format!("name-{}", id),
        node_type: "Function".to_string(),
        description: format!("desc {}", id),
        file: "src/a.ts".to_string(),
        start_line: 1,
        end_line: 2,
        last_update_at: 0,
        node_text: format!("text {}", id),
        vector: vec![value; EMBEDDING_DIM],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_and_query_nodes_round_trip() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().to_str().unwrap()).await.unwrap();

    let rows = vec![sample_node("a", 0.1), sample_node("b", 0.9)];
    db.upsert_nodes(&rows).await.unwrap();
    assert_eq!(db.count_nodes().await.unwrap(), 2);

    // Vector close to row "a" should rank "a" first.
    let query = vec![0.1f32; EMBEDDING_DIM];
    let hits = vector_search(&db, query, 2, None).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.id, "a");

    // Re-upsert "a" with a different value to verify upsert (not duplicate).
    let updated = vec![sample_node("a", 0.5)];
    db.upsert_nodes(&updated).await.unwrap();
    assert_eq!(db.count_nodes().await.unwrap(), 2);

    // SQL filter should restrict results.
    let filtered =
        vector_search(&db, vec![0.0f32; EMBEDDING_DIM], 5, Some("id = 'b'")).await.unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].0.id, "b");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_edges_and_traverse_outbound() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().to_str().unwrap()).await.unwrap();

    let edges = vec![
        EdgeRow {
            id: "a|Calls|b".to_string(),
            source: "a".to_string(),
            target: "b".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
        EdgeRow {
            id: "a|Calls|c".to_string(),
            source: "a".to_string(),
            target: "c".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
        EdgeRow {
            id: "b|Calls|d".to_string(),
            source: "b".to_string(),
            target: "d".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
    ];
    db.upsert_edges(&edges).await.unwrap();
    assert_eq!(db.count_edges().await.unwrap(), 3);

    let from_a = edges_from(&db, "a").await.unwrap();
    assert_eq!(from_a.len(), 2);
    let mut targets: Vec<String> = from_a.into_iter().map(|e| e.target).collect();
    targets.sort();
    assert_eq!(targets, vec!["b", "c"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_by_ids_returns_only_requested_rows() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().to_str().unwrap()).await.unwrap();

    let rows = vec![
        sample_node("a", 0.1),
        sample_node("b", 0.2),
        sample_node("c", 0.3),
    ];
    db.upsert_nodes(&rows).await.unwrap();

    let fetched = nodes_by_ids(&db, &vec!["a".to_string(), "c".to_string()])
        .await
        .unwrap();
    let mut ids: Vec<String> = fetched.into_iter().map(|n| n.id).collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "c"]);
}
