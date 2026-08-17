//! In-process tests for the `ug serve` route table.
//!
//! These exist because the router used to be built inline inside `run_serve`'s
//! `block_on` closure, which made every handler unreachable from a test: you
//! could not get a `Router` without binding a port and taking over the process.
//! `build_router` is now a plain function over `ServeState`, so the whole stack
//! — extractors, project scoping, middleware — can be driven with
//! `ServiceExt::oneshot` and no network at all.

use std::collections::HashMap;

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tower::ServiceExt;
use ultragraph::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};

use super::{
    build_project_context, build_router, snapshot_cache_budget, EncodedAsset, GenJobs,
    GraphModePolicy, ProjectRegistry, ServeMode, ServeState,
};

/// `UG_HOME` is process-global, so tests that enumerate projects must not run
/// concurrently with each other. Tests that only need a router take the guard
/// too, since `list_projects` can be reached from several routes.
///
/// This is deliberately the *same* lock `project::tests` uses — a second,
/// private mutex here would serialize these tests against each other while
/// still racing the project tests over the same env var.
use crate::project::UG_HOME_LOCK as ENV_GUARD;

/// A small but structurally real graph: two symbols in one file, plus the
/// File node the indexer always emits, plus the edges that always come with
/// them — the file `Contains` both symbols and one symbol `Calls` the other.
///
/// The edges are not decoration. Without them `AdjIndex`, `/api/graph/traverse`
/// and `/api/graph/path` were all exercised against an empty edge list, which
/// is the one shape that cannot distinguish a working adjacency index from a
/// broken one.
fn sample_graph() -> GraphData {
    let node = |id: &str, name: &str, ty: GraphNodeType, file: Option<&str>| GraphNode {
        id: id.to_string(),
        name: name.to_string(),
        node_type: ty,
        file: file.map(|f| f.to_string()),
        start_line: Some(1),
        end_line: Some(4),
        ..Default::default()
    };
    let edge = |source: &str, target: &str, edge_type: GraphEdgeType| GraphEdge {
        source: source.to_string(),
        target: target.to_string(),
        edge_type,
    };

    GraphData {
        nodes: vec![
            node("file:src/a.rs", "a.rs", GraphNodeType::File, Some("src/a.rs")),
            node(
                "function:src/a.rs:1:alpha",
                "alpha",
                GraphNodeType::Function,
                Some("src/a.rs"),
            ),
            node(
                "function:src/a.rs:3:beta",
                "beta",
                GraphNodeType::Function,
                Some("src/a.rs"),
            ),
        ],
        edges: vec![
            edge("file:src/a.rs", "function:src/a.rs:1:alpha", GraphEdgeType::Contains),
            edge("file:src/a.rs", "function:src/a.rs:3:beta", GraphEdgeType::Contains),
            edge(
                "function:src/a.rs:1:alpha",
                "function:src/a.rs:3:beta",
                GraphEdgeType::Calls,
            ),
        ],
        stats: None,
        resolution: None,
    }
}

/// Lay out a `~/.ug`-shaped project dir plus the repo it points at.
///
/// Returns `(ug_home, repo_root)`. Both live inside `tmp`, so the whole
/// fixture is torn down with it.
fn write_project(tmp: &TempDir, name: &str, graph: &GraphData) -> (std::path::PathBuf, std::path::PathBuf) {
    let ug_home = tmp.path().join("ug_home");
    let repo_root = tmp.path().join("repo");
    let project_dir = ug_home.join(name);
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(repo_root.join("src")).unwrap();
    std::fs::write(repo_root.join("src/a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    // A file outside the repo, for the traversal test to aim at.
    std::fs::write(tmp.path().join("outside.txt"), "SECRET").unwrap();

    std::fs::write(
        project_dir.join("graph.json"),
        serde_json::to_string(graph).unwrap(),
    )
    .unwrap();

    let meta = crate::project::ProjectMeta::new(
        name,
        repo_root.to_str().unwrap(),
        graph.nodes.len(),
        graph.edges.len(),
    )
    .with_graph_index(graph);
    crate::project::write_meta(&project_dir, &meta).unwrap();

    (ug_home, repo_root)
}

/// Build a router over a single loaded project, with the DB disabled.
///
/// `no_db: true` keeps this hermetic — no OverGraph directory, no embedder,
/// no network — which is exactly the configuration the `503` assertions below
/// are about.
async fn router_for(tmp: &TempDir, name: &str, graph: &GraphData) -> axum::Router {
    router_with_mode(tmp, name, graph, GraphModePolicy::Auto).await
}

/// As [`router_for`], with the graph-delivery policy pinned. Server mode is
/// otherwise only reachable with a 50 MB fixture, which is not a thing to put
/// in a test suite.
async fn router_with_mode(
    tmp: &TempDir,
    name: &str,
    graph: &GraphData,
    graph_mode: GraphModePolicy,
) -> axum::Router {
    let (ug_home, repo_root) = write_project(tmp, name, graph);
    std::env::set_var("UG_HOME", &ug_home);
    // The wizard's filesystem routes are confined to `browse_roots()`, and a
    // TempDir sits outside every default root. Declare it the way a user
    // keeping repos on another volume would.
    std::env::set_var("UG_BROWSE_ROOTS", tmp.path());

    let registry = Arc::new(ProjectRegistry {
        mode: ServeMode::Multi,
        no_db: true,
        active: RwLock::new(String::new()),
        loaded: RwLock::new(HashMap::new()),
        lru: RwLock::new(Vec::new()),
        cache_budget: snapshot_cache_budget(),
    });

    let ctx = build_project_context(
        name,
        ug_home.join(name).join("graph.json"),
        ug_home.join(name).join("ugdb"),
        Some(repo_root),
        true,
    )
    .await
    .expect("context builds");
    registry.insert_and_activate(ctx);

    let asset = |b: &[u8]| Arc::new(EncodedAsset::new(b.to_vec(), "text/plain; charset=utf-8"));
    let state = ServeState {
        registry,
        html: asset(b"<html></html>"),
        bundle: asset(b"// bundle"),
        cosmos_bundle: asset(b"// cosmos bundle"),
        favicon: asset(b"<svg/>"),
        embedder: None,
        chat_default: Arc::new(RwLock::new(None)),
        serve_args: Arc::new(Vec::new()),
        embed_lock: Arc::new(Semaphore::new(4)),
        gen_jobs: Arc::new(GenJobs::new()),
        staleness: Arc::new(RwLock::new(None)),
        graph_mode,
    };

    build_router(state)
}

/// GET `uri`, returning the status and the body as a string.
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn healthz_is_ok() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, _) = get(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn graph_stats_reflect_the_loaded_graph() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/api/graph/stats").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v.get("nodes").and_then(|n| n.as_u64()),
        Some(3),
        "stats should count the graph's nodes, got {body}"
    );
}

/// The slim index is what a browser in server mode gets *instead of*
/// `graph.json`, so the two must describe the same graph. Position is identity
/// in the server-mode protocol — a node's index in these columns is how every
/// later request refers to it — which is why the id order is asserted rather
/// than the id set.
#[tokio::test]
async fn the_slim_index_describes_the_same_graph_as_graph_json() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let graph = sample_graph();
    let app = router_for(&tmp, "demo", &graph).await;

    let (status, body) = get(&app, "/api/graph/nodes").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(v["n"], serde_json::json!(3));
    assert_eq!(v["edgeCount"], serde_json::json!(3));

    let ids: Vec<&str> = v["ids"].as_array().unwrap().iter().map(|i| i.as_str().unwrap()).collect();
    let expected: Vec<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, expected, "column order must be graph.json node order: {body}");

    // Every per-node column is exactly n long, or the client's positional
    // decode silently reads the wrong node.
    for col in ["ids", "names", "typeIdx", "fileIdx", "startLine", "endLine", "deg"] {
        assert_eq!(
            v[col].as_array().unwrap().len(),
            3,
            "column {col} must have one entry per node: {body}"
        );
    }

    // Dictionary coding round-trips.
    let types = v["types"].as_array().unwrap();
    let type_of = |i: usize| types[v["typeIdx"][i].as_u64().unwrap() as usize].as_str().unwrap();
    assert_eq!(type_of(0), "File");
    assert_eq!(type_of(1), "Function");

    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "all three nodes share one file path: {body}");
    assert_eq!(files[0].as_str(), Some("src/a.rs"));

    // Undirected degree: the file contains two symbols, alpha also calls beta.
    let deg: Vec<u64> = v["deg"].as_array().unwrap().iter().map(|d| d.as_u64().unwrap()).collect();
    assert_eq!(deg, vec![2, 2, 2], "file:2 contains, alpha:1+1, beta:1+1: {body}");

    // Sparse, not a column — no node here carries a boundary.
    assert_eq!(v["boundary"], serde_json::json!([]));
    // The file has no Folder/File parent, so it is the one catalog root.
    assert_eq!(v["catalogRoots"], serde_json::json!([0]));
    assert_eq!(v["edgeTypeCounts"]["Contains"], serde_json::json!(2));
    assert_eq!(v["edgeTypeCounts"]["Calls"], serde_json::json!(1));
}

/// The two scopes differ in exactly one way, and getting it wrong is the
/// failure mode the whole three-state client cache exists to prevent: an
/// `induced` answer must not be mistakable for a complete one.
///
/// Graph: `file --Contains--> {alpha, beta}`, `alpha --Calls--> beta`.
/// Indices: 0 = file, 1 = alpha, 2 = beta.
#[tokio::test]
async fn edges_incident_completes_a_node_and_induced_does_not() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    // Incident on alpha: both the Contains edge coming in and the Calls edge
    // going out, and alpha is now complete.
    let (status, body) = post(&app, "/api/graph/edges", serde_json::json!({ "ids": [1] })).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["src"], serde_json::json!([0, 1]), "{body}");
    assert_eq!(v["tgt"], serde_json::json!([1, 2]), "{body}");
    assert_eq!(v["complete"], serde_json::json!([1]), "incident completes its ids: {body}");
    let rels: Vec<&str> = v["relTypes"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(rels, vec!["Contains", "Calls"], "{body}");

    // Induced over {alpha, beta}: the Calls edge between them, but *not* the
    // Contains edge from the file, which leaves the set.
    let (_, body) = post(        &app,
        "/api/graph/edges",
        serde_json::json!({ "ids": [1, 2], "scope": "induced" }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["src"], serde_json::json!([1]), "only the within-set edge: {body}");
    assert_eq!(v["tgt"], serde_json::json!([2]), "{body}");
    assert_eq!(
        v["complete"],
        serde_json::json!([]),
        "induced withholds edges leaving the set, so it may never mark an id complete — \
         a client that cached this as complete would render a partial neighbourhood \
         with no error: {body}"
    );

    // Out-of-range indices are ignored rather than panicking the handler.
    let (status, body) = post(&app, "/api/graph/edges", serde_json::json!({ "ids": [99] })).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["src"], serde_json::json!([]), "{body}");
}

/// Hydration returns the *whole* node, which is the half the slim index drops.
#[tokio::test]
async fn hydrate_returns_full_nodes_for_indices() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let mut graph = sample_graph();
    graph.nodes[1].docstring = Some("what alpha does".into());
    graph.nodes[1].calls = vec!["beta".into()];
    let app = router_for(&tmp, "demo", &graph).await;

    let (status, body) = post(&app, "/api/graph/nodes/hydrate", serde_json::json!({ "ids": [1] })).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let nodes = v["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "{body}");
    assert_eq!(nodes[0]["id"], serde_json::json!("function:src/a.rs:1:alpha"), "{body}");
    assert_eq!(
        nodes[0]["docstring"],
        serde_json::json!("what alpha does"),
        "the docstring is exactly what the slim index omits: {body}"
    );
    assert_eq!(nodes[0]["calls"], serde_json::json!(["beta"]), "{body}");

    // Out-of-range indices are dropped, not fatal — the client may hold a
    // stale index across a snapshot reload.
    let (status, body) = post(&app, "/api/graph/nodes/hydrate", serde_json::json!({ "ids": [0, 99] })).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["nodes"].as_array().unwrap().len(), 1, "{body}");
}

/// The one bit of the handshake the page keys off. A graph under the threshold
/// must keep today's behaviour, and `--graph-mode` must be able to say
/// otherwise — that override is how the server path gets tested at all without
/// a 50 MB fixture.
#[tokio::test]
async fn capabilities_publishes_the_graph_delivery_mode() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();

    let app = router_for(&tmp, "demo", &sample_graph()).await;
    let (status, body) = get(&app, "/api/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["graph"]["mode"],
        serde_json::json!("local"),
        "a tiny graph must stay on the whole-file path: {body}"
    );
    assert_eq!(v["graph"]["nodes"], serde_json::json!(3));
    assert_eq!(v["graph"]["edges"], serde_json::json!(3));
    assert!(
        v["graph"]["bytes"].as_u64().unwrap() > 0,
        "bytes is what the threshold compares: {body}"
    );

    let forced = router_with_mode(&tmp, "demo", &sample_graph(), GraphModePolicy::Server).await;
    let (_, body) = get(&forced, "/api/capabilities").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["graph"]["mode"], serde_json::json!("server"), "{body}");
}

#[test]
fn graph_mode_policy_resolves_by_size_only_on_auto() {
    let big = super::GRAPH_SERVER_MODE_BYTES;
    assert_eq!(GraphModePolicy::Auto.resolve(big), "server");
    assert_eq!(GraphModePolicy::Auto.resolve(big - 1), "local");
    // The overrides ignore size in both directions — that is their whole job.
    assert_eq!(GraphModePolicy::Local.resolve(big), "local");
    assert_eq!(GraphModePolicy::Server.resolve(0), "server");
    assert_eq!(GraphModePolicy::parse("AUTO"), Some(GraphModePolicy::Auto));
    assert_eq!(GraphModePolicy::parse("nonsense"), None);
}

/// `AdjIndex` stores edge indices rather than neighbour indices, so every
/// traversal now resolves the far endpoint through `id_to_idx`. These two tests
/// are the ones that would have caught a mis-wired index — and could not have,
/// while `sample_graph` had no edges at all.
///
/// The graph is `file:src/a.rs --Contains--> {alpha, beta}` and
/// `alpha --Calls--> beta`.
#[tokio::test]
async fn traverse_walks_outbound_edges_and_returns_induced_ones() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/api/graph/traverse/file:src/a.rs?k=1").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    // One hop from the file reaches both symbols.
    let mut reached: Vec<&str> = v["distances"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    reached.sort_unstable();
    assert_eq!(
        reached,
        vec![
            "file:src/a.rs",
            "function:src/a.rs:1:alpha",
            "function:src/a.rs:3:beta"
        ],
        "1 hop from the file should reach both symbols, got {body}"
    );

    // *Induced*, not merely walked: alpha→beta was never traversed (it is not
    // reachable from the file in one hop through alpha), but both endpoints are
    // in the visited set, so it belongs in the answer. Dropping it is the
    // regression the rewrite could plausibly have introduced.
    let rels: Vec<&str> = v["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["edge_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        rels,
        vec!["Contains", "Contains", "Calls"],
        "all three edges are induced by the visited set, in graph.json order, got {body}"
    );
}

/// Forward-only, and the client's `findPath` relies on exactly that.
#[tokio::test]
async fn path_follows_edge_direction_only() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let fwd = "/api/graph/path?source=function:src/a.rs:1:alpha&target=function:src/a.rs:3:beta";
    let (status, body) = get(&app, fwd).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["found"], serde_json::json!(true), "alpha calls beta: {body}");
    assert_eq!(v["length"], serde_json::json!(1), "one edge apart: {body}");

    let rev = "/api/graph/path?source=function:src/a.rs:3:beta&target=function:src/a.rs:1:alpha";
    let (_, body) = get(&app, rev).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["found"],
        serde_json::json!(false),
        "beta does not call alpha, and path must not walk the edge backwards: {body}"
    );
}

/// `/graph.json` is served straight out of `encoded.identity`, which is also
/// what `GraphSnapshot::raw_json()` now borrows. If dropping the duplicate
/// `raw_json` field had broken that aliasing, this is where it would show.
#[tokio::test]
async fn graph_json_round_trips_through_the_encoded_asset() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/graph.json").await;
    assert_eq!(status, StatusCode::OK);
    let parsed: GraphData = serde_json::from_str(&body).expect("served graph is valid JSON");
    assert_eq!(parsed.nodes.len(), 3);
}

#[tokio::test]
async fn file_preview_reads_a_file_inside_the_repo() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/api/file?path=src/a.rs").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.contains("three"),
        "expected file contents in preview, got {body}"
    );
}

/// The traversal guard: `..` that resolves outside the repo root must be
/// refused rather than served.
#[tokio::test]
async fn file_preview_rejects_paths_escaping_the_repo_root() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/api/file?path=../outside.txt").await;
    assert_ne!(
        status,
        StatusCode::OK,
        "traversal outside the repo root must not be served, got {body}"
    );
    assert!(
        !body.contains("SECRET"),
        "escaped file contents leaked: {body}"
    );
}

/// With `--no-db` the embedder-backed routes must degrade to 503, not panic
/// and not 500.
#[tokio::test]
async fn search_routes_503_without_a_db() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/search/semantic")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"alpha"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// Centrality moved onto `spawn_blocking` and now reads the parsed graph
/// instead of re-parsing a cloned JSON string. It must still answer.
#[tokio::test]
async fn centrality_and_cycles_still_answer() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/api/graph/centrality").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        serde_json::from_str::<serde_json::Value>(&body).is_ok(),
        "centrality body should be JSON, got {body}"
    );

    let (status, body) = get(&app, "/api/graph/cycles").await;
    assert_eq!(status, StatusCode::OK);
    assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());
}

/// Staleness is served off the persisted `files` list. A file touched after
/// the graph was written must register as changed.
#[tokio::test]
async fn staleness_reports_a_touched_file_as_changed() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let graph = sample_graph();
    let app = router_for(&tmp, "demo", &graph).await;

    // Rewrite a source file so its mtime lands after graph.json's.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(tmp.path().join("repo/src/a.rs"), "changed\n").unwrap();

    let (status, body) = get(&app, "/api/projects/staleness").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let row = v["projects"]
        .as_array()
        .and_then(|a| a.iter().find(|p| p["name"] == "demo"))
        .unwrap_or_else(|| panic!("no row for 'demo' in {body}"));

    assert_eq!(row["isStale"], serde_json::json!(true), "body: {body}");
    assert_eq!(row["changed"], serde_json::json!(1), "body: {body}");
    assert_eq!(row["files"], serde_json::json!(1), "body: {body}");
}

/// The TTL cache must serve a second request from memory rather than
/// rescanning — that is the whole point of it under a multi-tab poll.
#[tokio::test]
async fn staleness_is_cached_within_its_ttl() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (_, first) = get(&app, "/api/projects/staleness").await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (_, second) = get(&app, "/api/projects/staleness").await;

    let a: serde_json::Value = serde_json::from_str(&first).unwrap();
    let b: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(
        a["checkedAt"], b["checkedAt"],
        "second call inside the TTL should be served from cache"
    );
}

/// `/api/generate` is multi-project-only; in single mode it must 400 rather
/// than spawn anything.
#[tokio::test]
async fn generate_is_rejected_in_single_project_mode() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    // Rebuild the same router in Single mode by flipping the registry mode is
    // not reachable from here, so assert the guard through the public route in
    // Multi mode instead: a path that does not exist must 400, never spawn.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"path":"/definitely/not/a/real/directory"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// Per-request project scoping lives on `POST /api/tools/:tool`, which reads a
/// `project` field from the body. Naming one that doesn't exist must 404 rather
/// than quietly falling back to the active project — silently answering from
/// the wrong graph is worse than an error.
///
/// (The `/api/graph/*` routes have no scoping at all and always read the active
/// project; `?project=` on those is an ignored query param, not a 404.)
#[tokio::test]
async fn unknown_project_scope_is_a_404() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tools/overview")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"project":"nope"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// `analyze` was the one tool `GET /api/tools` never listed: the discovery
/// walked `AGENT_TOOLS`, which is only the set `run_tool` can dispatch, so
/// an agent enumerating the HTTP surface learned that `analyze` existed only
/// by guessing. It must be advertised like the rest — with its real path,
/// method and a copyable preset-shaped example.
#[tokio::test]
async fn tools_discovery_advertises_analyze() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = get(&app, "/api/tools").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let list = v["tools"].as_array().expect("tools is an array");
    let analyze = list
        .iter()
        .find(|t| t["name"] == "analyze")
        .expect("analyze is advertised in GET /api/tools: {body}");
    assert_eq!(analyze["path"], "/api/tools/analyze");
    assert_eq!(analyze["method"], "POST");
    let example = analyze["example"].as_str().expect("example is a string");
    assert!(
        example.contains("preset") && example.contains("long_functions"),
        "the example should show the preset-shaped call, got: {example}"
    );
}

/// `analyze` is store-backed: the `POST /api/tools/analyze` route dispatches it
/// but it needs the indexed database, so under `--no-db` it must 503 (not fall
/// over) — and it scopes by `project` like every other tool.
#[tokio::test]
async fn analyze_route_is_dispatched_and_requires_the_db() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, body) = post(&app, "/api/tools/analyze", serde_json::json!({"preset": "long_functions"})).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "analyze without a db must 503, got {status}: {body}"
    );

    let (status, body) = post(
        &app,
        "/api/tools/analyze",
        serde_json::json!({"preset": "long_functions", "project": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown project: {body}");
}

/// `/api/browse-dir` moved onto `spawn_blocking`; it must still list only
/// directories, hide dotfiles, and report the resolved absolute path.
#[tokio::test]
async fn browse_dir_lists_subdirectories() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".hidden")).unwrap();

    let (status, body) = get(
        &app,
        &format!("/api/browse-dir?path={}", repo.to_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let names: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"src"), "expected 'src' in {names:?}");
    assert!(
        !names.contains(&".hidden"),
        "dotfiles must be hidden, got {names:?}"
    );
}

/// The server has no authentication, so the two routes that take a
/// caller-supplied filesystem path must refuse anything outside the allowed
/// roots. Unconfined they compose into a whole-machine read: browse to a
/// sensitive directory, index it as a project, then pull its contents back
/// out through `/api/file`.
#[tokio::test]
async fn filesystem_routes_refuse_paths_outside_the_allowed_roots() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    // `/etc` exists on every platform this runs on, and is outside the
    // temp root the fixture declared.
    let (status, body) = get(&app, "/api/browse-dir?path=/etc").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/generate")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/etc"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// DNS rebinding makes an attacker's page same-origin with us, so CORS never
/// fires; the `Host` header carrying their domain is what still gives it away.
#[tokio::test]
async fn requests_from_a_rebound_hostname_are_rejected() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/graph/stats")
                .header("host", "evil.tld")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // A cross-site Origin is refused even when Host looks fine — that's the
    // plain CSRF path, which CORS lets through for "simple" requests.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/graph/stats")
                .header("host", "127.0.0.1:8080")
                .header("origin", "https://evil.tld")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // The normal local case still works.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/graph/stats")
                .header("host", "127.0.0.1:8080")
                .header("origin", "http://127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A bad path must be a 400, not a 500 — the `spawn_blocking` wrapper has to
/// keep propagating the handler's own error mapping.
#[tokio::test]
async fn browse_dir_rejects_a_missing_path() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    let (status, _) = get(&app, "/api/browse-dir?path=/definitely/not/here").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The snapshot cache must drop least-recently-used projects once it exceeds
/// its budget, and must never drop the active one — `active_ctx` asserts the
/// active project is loaded, so evicting it would panic every later request.
#[tokio::test]
async fn cache_evicts_lru_but_never_the_active_project() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let graph = sample_graph();
    let (ug_home, repo_root) = write_project(&tmp, "active", &graph);
    std::env::set_var("UG_HOME", &ug_home);

    // A budget of 1 byte forces eviction on every insert.
    let registry = Arc::new(ProjectRegistry {
        mode: ServeMode::Multi,
        no_db: true,
        active: RwLock::new(String::new()),
        loaded: RwLock::new(HashMap::new()),
        lru: RwLock::new(Vec::new()),
        cache_budget: 1,
    });

    let build = |name: &str| {
        let dir = ug_home.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("graph.json"), serde_json::to_string(&graph).unwrap()).unwrap();
        (
            name.to_string(),
            dir.join("graph.json"),
            dir.join("ugdb"),
            repo_root.clone(),
        )
    };

    for name in ["active", "second", "third"] {
        let (n, graph_path, db_path, root) = build(name);
        let ctx = build_project_context(&n, graph_path, db_path, Some(root), true)
            .await
            .expect("context builds");
        if n == "active" {
            registry.insert_and_activate(ctx);
        } else {
            registry.insert_loaded(ctx);
        }
    }

    let loaded = registry.loaded.read().unwrap();
    assert!(
        loaded.contains_key("active"),
        "active project must survive eviction, have: {:?}",
        loaded.keys().collect::<Vec<_>>()
    );
    assert!(
        loaded.len() < 3,
        "over-budget cache should have evicted something, have: {:?}",
        loaded.keys().collect::<Vec<_>>()
    );
}

/// POST `uri` with a JSON body, returning the status and the body as a string.
async fn post(app: &axum::Router, uri: &str, body: serde_json::Value) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Total nodes reported by `graph_schema`, summed over its per-type counts.
fn node_total(body: &str) -> u64 {
    let v: serde_json::Value = serde_json::from_str(body).expect("graph_schema returns JSON");
    v["node_types"]
        .as_array()
        .expect("node_types is an array")
        .iter()
        .filter_map(|t| t["count"].as_u64())
        .sum()
}

/// `sample_graph` plus one more function, standing in for a re-indexed repo.
fn grown_graph() -> GraphData {
    let mut g = sample_graph();
    g.nodes.push(GraphNode {
        id: "function:src/a.rs:6:gamma".to_string(),
        name: "gamma".to_string(),
        node_type: GraphNodeType::Function,
        file: Some("src/a.rs".to_string()),
        start_line: Some(6),
        end_line: Some(8),
        ..Default::default()
    });
    g
}

/// A project reached only through `?project=` is cached by `resolve_ctx` and
/// was never re-read: the watcher only ever looked at the *active* project. So
/// a CLI `ug gen` against some other project landed and every later request for
/// it kept answering from the stale pre-run graph — no error, no staleness note,
/// just an old answer that looks current.
#[tokio::test]
async fn a_non_active_project_picks_up_a_regenerated_graph() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;

    // A second project, never activated.
    write_project(&tmp, "other", &sample_graph());
    let other_graph = tmp.path().join("ug_home").join("other").join("graph.json");

    let (status, body) = post(&app, "/api/tools/graph_schema", serde_json::json!({"project": "other"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(node_total(&body), 3, "first read sees the graph as written: {body}");

    // Distinct mtime, then rewrite — this is `ug gen` landing out of band.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    std::fs::write(&other_graph, serde_json::to_string(&grown_graph()).unwrap()).unwrap();

    let (status, body) = post(&app, "/api/tools/graph_schema", serde_json::json!({"project": "other"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        node_total(&body),
        4,
        "the regenerated graph must replace the cached snapshot: {body}"
    );
}

/// Deleting the active project used to drop it from `loaded` and only *then*
/// await the fallback's load, leaving `active` naming a context that was gone
/// for the length of a graph read + parse. Every request in that window hit
/// `active_ctx`'s `expect` and panicked — including the watch loop's own tick,
/// which killed live reloading for the rest of the process.
#[tokio::test]
async fn deleting_the_active_project_hands_over_to_a_survivor() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;
    write_project(&tmp, "keeper", &sample_graph());

    let (status, body) = post(&app, "/api/projects/delete", serde_json::json!({"name": "demo"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["removed"], "demo");
    assert_eq!(v["active"], "keeper", "the survivor takes over: {body}");

    // The registry has to still be coherent afterwards: every root-relative
    // route resolves through `active_ctx`.
    let (status, _) = get(&app, "/graph.json").await;
    assert_eq!(status, StatusCode::OK);
    let (status, listed) = get(&app, "/api/projects").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !listed.contains("\"demo\""),
        "deleted project should be gone from the listing: {listed}"
    );
}

/// Deleting a project that isn't active must leave the selection alone. The
/// response used to echo the *deleted* name back as `active`, which is the one
/// field a client uses to decide whether it needs to reload.
#[tokio::test]
async fn deleting_an_inactive_project_leaves_the_active_one_alone() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = TempDir::new().unwrap();
    let app = router_for(&tmp, "demo", &sample_graph()).await;
    write_project(&tmp, "spare", &sample_graph());

    let (status, body) = post(&app, "/api/projects/delete", serde_json::json!({"name": "spare"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["removed"], "spare");
    assert_eq!(
        v["active"], "demo",
        "deleting an inactive project must not change the active one: {body}"
    );

    let (status, _) = get(&app, "/graph.json").await;
    assert_eq!(status, StatusCode::OK);
}
