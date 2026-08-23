//! `router.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use super::api::*;
use super::chat_api::*;
use super::db_api::*;
use super::encoding::asset_response;
use super::host_guard::guard_host;
use super::projects_api::*;
use super::*;

/// Cap on inbound HTTP request bodies. Every route the UI/agent uses takes a
/// small JSON payload (search query, chat message, config patch, a generate
/// path) — all KB-scale — so 4 MiB is generous headroom while stopping the
/// previous unbounded-read behaviour from being an OOM / abuse vector.
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Assemble the full route table and middleware stack over an already-built
/// [`ServeState`].
///
/// Split out of `run_serve` so the router can be constructed without binding a
/// port or taking over the process: tests drive it through
/// `tower::ServiceExt::oneshot`, which is the only way handler behaviour
/// (project scoping, traversal rejection, the `--no-db` 503 paths) is reachable
/// from a unit test at all.
pub(crate) fn build_router(state: ServeState) -> Router {
    Router::new()
        .route("/", get(handle_index))
        .route("/index.html", get(handle_index))
        .route("/threejs-vis.bundle.js", get(handle_bundle))
        .route("/cosmos-vis.bundle.js", get(handle_cosmos_bundle))
        .route("/favicon.svg", get(handle_favicon))
        .route("/graph.json", get(handle_graph))
        .route("/indexed-tree.json", get(handle_indexed_tree))
        .route("/healthz", get(handle_health))
        .route("/api/projects", get(api_projects))
        .route("/api/projects/select", post(api_projects_select))
        .route("/api/projects/delete", post(api_projects_delete))
        .route("/api/generate", post(api_generate))
        .route("/api/generate/status", get(api_generate_status))
        // Re-embed an already-indexed project from the UI. Same job
        // tracker as /api/generate, so its status is polled the same way.
        .route("/api/ingest", post(api_ingest))
        .route("/api/browse-dir", get(api_browse_dir))
        .route("/api/capabilities", get(api_capabilities))
        .route("/api/config", get(api_config_get).post(api_config_post))
        .route("/api/graph/stats", get(api_stats))
        .route("/api/projects/staleness", get(api_projects_staleness))
        .route("/api/graph/nodes", get(api_slim_index))
        .route("/api/graph/nodes.bin", get(api_slim_index_bin))
        .route("/api/graph/edges", post(api_edges))
        .route("/api/graph/nodes/hydrate", post(api_hydrate))
        .route("/api/graph/node/*id", get(api_node))
        .route("/api/graph/search", get(api_search))
        .route("/api/graph/traverse/*id", get(api_traverse))
        .route("/api/graph/path", get(api_path))
        .route("/api/graph/filter", get(api_filter))
        .route("/api/graph/centrality", get(api_centrality))
        .route("/api/graph/cycles", get(api_cycles))
        // Agent tools — the same seven the CLI and MCP expose.
        .route("/api/tools", get(api_tools))
        .route("/api/presets", get(api_presets))
        .route("/api/tools/:tool", post(api_tool))
        // Source file content for the right-panel "Preview" tab.
        .route("/api/file", get(api_file))
        // Phase 3 — DB / embedder backed
        .route("/api/db/node/*id", get(api_db_node))
        .route("/api/db/traverse/*id", get(api_db_traverse))
        .route("/api/search/semantic", post(api_search_semantic))
        .route("/api/search/hybrid", post(api_search_hybrid))
        .route("/api/chat", post(api_chat))
        .route("/api/tour", post(api_tour))
        .route("/api/chat/config", get(api_chat_config))
        // CompressionLayer skips responses that already have Content-Encoding,
        // so it only kicks in for the dynamic /api/* JSON.
        .layer(CompressionLayer::new().br(true))
        // Reject oversized request bodies before they reach a handler —
        // every legitimate payload is KB-scale, so 4 MiB is pure abuse
        // protection and never bites the app.
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        // Default CORS policy denies cross-origin requests: same-origin
        // clients (the web UI and the Tauri shell at 127.0.0.1:8080) are
        // never subject to CORS and pass through untouched, while a
        // cross-origin browser fetch hits an empty preflight response and
        // is blocked — the CSRF / drive-by defense.
        .layer(CorsLayer::new())
        // CORS alone does not stop DNS rebinding, which makes the attacker's
        // page same-origin with us. `guard_host` checks the one header that
        // still gives it away. Outermost so it runs before any handler —
        // and before the body limit, so a rejected host costs nothing.
        .layer(middleware::from_fn(guard_host))
        // One INFO span per request: method+uri on entry, status+latency on exit.
        // Matches the structured-log pattern the rest of the server uses.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::DEBUG))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}

// ---------- Static handlers ----------

async fn handle_index(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.html, &headers)
}

async fn handle_graph(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    let snap = state.snapshot();
    asset_response(&snap.encoded, &headers)
}

async fn handle_indexed_tree(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    let ctx = state.active();
    let idx_path = ctx.graph_path.with_file_name("indexed-tree.json");
    match tokio::fs::read(&idx_path).await {
        Ok(data) => {
            let asset = EncodedAsset::new(data, "application/json; charset=utf-8");
            asset_response(&asset, &headers)
        }
        Err(e) => err_json(
            StatusCode::NOT_FOUND,
            &format!(
                "indexed-tree.json not found at {}: {}",
                idx_path.display(),
                e
            ),
        ),
    }
}

async fn handle_bundle(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.bundle, &headers)
}

async fn handle_cosmos_bundle(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.cosmos_bundle, &headers)
}

async fn handle_favicon(State(state): State<ServeState>, headers: HeaderMap) -> Response {
    asset_response(&state.favicon, &headers)
}

async fn handle_health() -> &'static str {
    "ok"
}
