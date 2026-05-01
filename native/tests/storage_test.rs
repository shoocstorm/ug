use std::io::Write;
use tempfile::TempDir;
use ultragraph_kb::storage::db::{
    edges_from, edges_schema, edges_to, fts_search, nodes_by_ids, nodes_schema, vector_search, Db,
    EdgeRow, NodeRow,
};
use ultragraph_kb::storage::embed::EMBEDDING_DIM;
use ultragraph_kb::storage::query::{
    mmr_rerank, read_snippet, traverse_filtered, Direction, SearchHit,
};
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
            folder: None,
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
            folder: None,
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
async fn edges_to_walks_inbound() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().to_str().unwrap()).await.unwrap();

    let edges = vec![
        EdgeRow {
            id: "a|Calls|c".to_string(),
            source: "a".to_string(),
            target: "c".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
        EdgeRow {
            id: "b|Calls|c".to_string(),
            source: "b".to_string(),
            target: "c".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
    ];
    db.upsert_edges(&edges).await.unwrap();

    let inbound = edges_to(&db, "c").await.unwrap();
    let mut sources: Vec<String> = inbound.into_iter().map(|e| e.source).collect();
    sources.sort();
    assert_eq!(sources, vec!["a", "b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traverse_filtered_respects_direction_and_edge_type() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().to_str().unwrap()).await.unwrap();

    // Star: a -[Calls]-> b, a -[Imports]-> c, d -[Calls]-> a.
    let edges = vec![
        EdgeRow {
            id: "a|Calls|b".to_string(),
            source: "a".to_string(),
            target: "b".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
        EdgeRow {
            id: "a|Imports|c".to_string(),
            source: "a".to_string(),
            target: "c".to_string(),
            edge_type: "Imports".to_string(),
            properties: String::new(),
        },
        EdgeRow {
            id: "d|Calls|a".to_string(),
            source: "d".to_string(),
            target: "a".to_string(),
            edge_type: "Calls".to_string(),
            properties: String::new(),
        },
    ];
    db.upsert_edges(&edges).await.unwrap();

    // Outbound, no filter: expect a, b, c.
    let r = traverse_filtered(&db, &vec!["a".to_string()], 1, None, Direction::Outbound)
        .await
        .unwrap();
    let mut ids: Vec<String> = r.distances.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "b", "c"]);

    // Outbound, filter to Calls only: expect a, b.
    let only_calls = vec!["Calls".to_string()];
    let r = traverse_filtered(
        &db,
        &vec!["a".to_string()],
        1,
        Some(&only_calls),
        Direction::Outbound,
    )
    .await
    .unwrap();
    let mut ids: Vec<String> = r.distances.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "b"]);

    // Inbound, no filter: expect a, d.
    let r = traverse_filtered(&db, &vec!["a".to_string()], 1, None, Direction::Inbound)
        .await
        .unwrap();
    let mut ids: Vec<String> = r.distances.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "d"]);

    // Both, no filter: expect a, b, c, d.
    let r = traverse_filtered(&db, &vec!["a".to_string()], 1, None, Direction::Both)
        .await
        .unwrap();
    let mut ids: Vec<String> = r.distances.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "b", "c", "d"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_search_finds_by_name() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().to_str().unwrap()).await.unwrap();

    let mut authn = sample_node("authn", 0.1);
    authn.name = "authenticate".to_string();
    authn.description = "verify user credentials".to_string();
    authn.node_text = "Function: authenticate. verify user credentials. Related: ".to_string();

    let mut other = sample_node("other", 0.2);
    other.name = "renderTable".to_string();
    other.description = "draw a UI table".to_string();
    other.node_text = "Function: renderTable. draw a UI table. Related: ".to_string();

    db.upsert_nodes(&vec![authn, other]).await.unwrap();
    let _ = db.try_create_fts_index().await; // OK if it fails on tiny tables.

    let hits = fts_search(&db, "authenticate", 5, None).await.unwrap();
    assert!(hits.iter().any(|h| h.id == "authn"));
}

#[test]
fn read_snippet_returns_inclusive_line_range() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("foo.txt");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "L1").unwrap();
        writeln!(f, "L2").unwrap();
        writeln!(f, "L3").unwrap();
        writeln!(f, "L4").unwrap();
    }
    let s = read_snippet(dir.path(), "foo.txt", 2, 3).unwrap();
    assert_eq!(s, "L2\nL3\n");
    assert!(read_snippet(dir.path(), "foo.txt", 0, 0).is_none());
    assert!(read_snippet(dir.path(), "missing.txt", 1, 1).is_none());
}

#[test]
fn mmr_rerank_orders_by_lambda() {
    // Three candidates, two of which are near-duplicates.
    let q = vec![1.0f32, 0.0, 0.0];
    let make = |id: &str, vec: Vec<f32>, dist: f32| SearchHit {
        node: NodeRow {
            id: id.to_string(),
            name: id.to_string(),
            node_type: "Function".to_string(),
            description: String::new(),
            file: String::new(),
            start_line: 0,
            end_line: 0,
            last_update_at: 0,
            node_text: String::new(),
            vector: vec,
        },
        distance: dist,
    };
    let cands = vec![
        make("a", vec![1.0, 0.0, 0.0], 0.0),
        make("b", vec![0.99, 0.0, 0.0], 0.01), // near-duplicate of a
        make("c", vec![0.0, 1.0, 0.0], 1.0),  // diverse
    ];

    // Lambda = 1: pure relevance, "a" then "b" (both are highly relevant).
    let r1 = mmr_rerank(&q, cands.clone(), 2, 1.0);
    assert_eq!(r1[0].node.id, "a");
    assert_eq!(r1[1].node.id, "b");

    // Lambda = 0.0: pure diversity. After "a", "c" should beat "b".
    let r2 = mmr_rerank(&q, cands.clone(), 2, 0.0);
    assert_eq!(r2[0].node.id, "a");
    assert_eq!(r2[1].node.id, "c");
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
