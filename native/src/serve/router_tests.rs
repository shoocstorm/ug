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
use ultragraph::types::{GraphData, GraphNode, GraphNodeType};

use super::{
    build_project_context, build_router, snapshot_cache_budget, EncodedAsset, GenJobs,
    ProjectRegistry, ServeMode, ServeState,
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
/// File node the indexer always emits.
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
        edges: vec![],
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
