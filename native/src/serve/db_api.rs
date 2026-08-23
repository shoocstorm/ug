//! `db_api.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::sync::Arc;

use axum::extract::{Json, Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use ultragraph::storage::{
    self, search_kb as storage_search_kb, semantic_search as storage_semantic_search,
    semantic_search_w_where, traverse_filtered, Direction, Embedder, KnowledgeStore, RankStrategy,
    SearchKbOptions,
};

use super::api::parse_csv;
use super::*;
use ultragraph::types::{GraphData, GraphNode};

// ---------- Phase 3 — DB-backed handlers ----------

/// Resolve a per-request `dest` parameter to a concrete store. `None`
/// uses the primary. Returns a 503 if no backend is available, 404 if
/// the caller asked for a name we didn't open.
pub(crate) fn pick_store(
    state: &ServeState,
    dest: Option<&str>,
) -> Result<Arc<dyn KnowledgeStore>, Response> {
    let stores = state.stores().ok_or_else(|| {
        let reason = state.db_unavailable_reason();
        let msg = reason.as_deref().unwrap_or("DB not opened");
        err_json(StatusCode::SERVICE_UNAVAILABLE, msg)
    })?;
    let name = dest
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| stores.primary.clone());
    stores.get(&name).cloned().ok_or_else(|| {
        let available = stores.names().join(", ");
        err_json(
            StatusCode::NOT_FOUND,
            &format!("unknown destination '{}' (available: {})", name, available),
        )
    })
}

pub(crate) fn embedder_or_503(state: &ServeState) -> Result<Arc<Embedder>, Response> {
    state.embedder.clone().ok_or_else(|| {
        err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "embedder not configured (started with --no-db?)",
        )
    })
}

#[derive(serde::Deserialize)]
pub(crate) struct FileQuery {
    /// Repo-relative path of the source file to read.
    path: String,
    /// Optional 1-based inclusive line range. Omit both for the full file
    /// (File nodes); pass them to return just a chunk's span.
    start: Option<usize>,
    end: Option<usize>,
}

/// Cap on how much source a single preview may return, whichever copy it
/// comes from. A whole-file node's captured code can be megabytes; the panel
/// is a transparency view, not a file viewer.
const FILE_PREVIEW_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Reads a source file (or a line slice of one) so the UI's Preview tab can
/// show real content.
///
/// Prefers the live file from the repo — that is the tab's promise, "the file
/// as it is now" — but falls back to the source the store captured at index
/// time when the repo path is unavailable or the file has moved/deleted since
/// indexing. `node_text` in the DB is a synthetic embedding string, not the
/// source, which is why this route exists at all. The response's `source`
/// field tells the UI which copy it got (`filesystem` vs `db`).
pub(crate) async fn api_file(
    State(state): State<ServeState>,
    Query(params): Query<FileQuery>,
) -> Response {
    let rel = ultragraph::agent_tools::strip_file_id_prefix(params.path.trim());
    if rel.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "missing path");
    }

    if let Some(resp) = file_from_disk(&state, rel, &params).await {
        return resp;
    }

    if let Some(resp) = file_from_store(&state, rel, &params).await {
        return resp;
    }

    err_json(
        StatusCode::NOT_FOUND,
        &format!(
            "{} not found in the repo ({}) or the index",
            rel,
            state.repo_root().display()
        ),
    )
}

/// Read `rel` live from the repo root, if the repo is present and the file
/// exists and is UTF-8 text.
///
/// Returns `None` (not an error) when any of those fails, so the caller can
/// fall back to the indexed copy. A repo root that no longer exists is the
/// whole point of that fallback, so it is checked before any resolution.
async fn file_from_disk(state: &ServeState, rel: &str, params: &FileQuery) -> Option<Response> {
    let repo_root = state.repo_root();
    let root = repo_root.as_path();
    if !root.exists() {
        return None; // repo moved or deleted — nothing to read live
    }

    // Resolve against the repo root and canonicalize, then verify the result
    // is still inside the root — blocks `../` traversal and absolute-path
    // escapes. Only live reads can escape (the store only ever contains
    // indexed paths), so the check lives here rather than on the fallback.
    //
    // Both sides must be canonical for `starts_with` to mean anything: a root
    // reached through a symlink (`/tmp` → `/private/tmp` on macOS) compares
    // unequal to the resolved file path and would reject every legitimate
    // read. Fall back to the raw root if it can't be resolved — a
    // non-existent root is caught above, and comparing against the
    // unresolved form is stricter, never looser.
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon = std::fs::canonicalize(root.join(rel)).ok()?;
    if !canon.starts_with(&canon_root) {
        return Some(err_json(StatusCode::FORBIDDEN, "path escapes repo root"));
    }

    if let Ok(meta) = std::fs::metadata(&canon) {
        if meta.len() > FILE_PREVIEW_MAX_BYTES {
            return Some(err_json(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file too large to preview",
            ));
        }
    }

    // Non-UTF-8 files (binaries, PDFs) have no text to preview; fall through
    // to the store, which may still hold a captured textual span.
    let text = tokio::fs::read_to_string(&canon).await.ok()?;

    let (content, sliced, total_lines) = slice_file_text(text, params.start, params.end);

    Some(ok_json(
        serde_json::json!({
            "path": rel,
            "content": content,
            "start_line": params.start,
            "end_line": params.end,
            "total_lines": total_lines,
            "sliced": sliced,
            "source": "filesystem",
        })
        .to_string(),
    ))
}

/// Cut a 1-based inclusive line range out of a whole file, clamped to its
/// bounds. Returns `(content, sliced, total_lines)`, where `total_lines` is
/// always the *file's* length so the UI can place the slice.
///
/// Shared by the live read and the indexed fallback: a Preview request must
/// return the same lines whether or not the repo is on this machine, and
/// two copies of this arithmetic is how that stops being true.
pub(crate) fn slice_file_text(
    text: String,
    start: Option<usize>,
    end: Option<usize>,
) -> (String, bool, usize) {
    let all: Vec<&str> = text.lines().collect();
    let total_lines = all.len();
    match start {
        Some(s) if s >= 1 => {
            let lo = s - 1;
            let hi = end.unwrap_or(s).max(s).min(total_lines);
            let body = if lo >= total_lines {
                String::new()
            } else {
                all[lo..hi].join("\n")
            };
            (body, true, total_lines)
        }
        _ => (text, false, total_lines),
    }
}

/// Serve the source the store captured for this file at index time.
///
/// `None` when the store is unavailable or no matching node has captured
/// code. The matching itself lives in [`stored_source_for_file`] so the
/// fallback is testable without a running server.
async fn file_from_store(state: &ServeState, rel: &str, params: &FileQuery) -> Option<Response> {
    let store = pick_store(state, None).ok()?;
    let snap = state.snapshot();
    let (code, sliced) =
        stored_source_for_file(&snap.parsed, store.as_ref(), rel, params.start, params.end).await?;
    if code.len() as u64 > FILE_PREVIEW_MAX_BYTES {
        return Some(err_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "indexed file too large to preview",
        ));
    }
    // An exact-span match already *is* the requested lines; a whole-file
    // capture still has to be cut down to them, or a range request would
    // silently return the entire file whenever the repo is missing.
    let (content, sliced, total_lines) = if sliced {
        let n = code.lines().count();
        (code, true, n)
    } else {
        slice_file_text(code, params.start, params.end)
    };
    Some(ok_json(
        serde_json::json!({
            "path": rel,
            "content": content,
            "start_line": params.start,
            "end_line": params.end,
            "total_lines": total_lines,
            "sliced": sliced,
            "source": "db",
        })
        .to_string(),
    ))
}

/// Resolve `rel` (+ optional line range) to the source the store captured at
/// index time, with no filesystem involved.
///
/// Uses the in-memory graph to map `rel` + line range to a node id, then
/// reads that node's stored code. A span request matches the exact node
/// first, then falls back to the file's whole-capture node; a whole-file
/// request goes straight to that. Returns `(code, sliced)` — `sliced` says
/// whether `code` is already narrowed to the requested span, which is false
/// for the whole-file node and tells [`file_from_store`] it still has to cut
/// the range out. `None` when no matching node has captured code.
pub(crate) async fn stored_source_for_file(
    graph: &GraphData,
    store: &dyn KnowledgeStore,
    rel: &str,
    start: Option<usize>,
    end: Option<usize>,
) -> Option<(String, bool)> {
    // Whole-file nodes (File/Config) carry no line range; symbol nodes carry
    // their exact span.
    let whole_file = |n: &GraphNode| n.file.as_deref() == Some(rel) && n.start_line.is_none();
    let exact_span = |n: &GraphNode| {
        n.file.as_deref() == Some(rel)
            && match start {
                Some(s) => {
                    n.start_line == Some(s as u32)
                        && end.map(|e| n.end_line == Some(e as u32)).unwrap_or(true)
                }
                None => false,
            }
    };

    let mut candidates: Vec<&GraphNode> = graph.nodes.iter().filter(|n| exact_span(n)).collect();
    candidates.extend(graph.nodes.iter().filter(|n| whole_file(n)));

    for node in candidates {
        let Ok(Some(row)) = store.fetch_node(&node.id).await else {
            continue;
        };
        if row.code.is_empty() {
            continue;
        }
        // A span request that fell back to the whole-file node reports the
        // whole file (`sliced: false`), matching what the live read would do
        // if it had the file.
        let sliced = node.start_line.is_some() && start.is_some();
        return Some((row.code, sliced));
    }

    None
}

#[derive(serde::Deserialize)]
pub(crate) struct DbNodeQuery {
    /// Optional destination name; defaults to the primary backend.
    /// Mirrors the `dest` field used by all the other DB-backed routes.
    dest: Option<String>,
}

pub(crate) async fn api_db_node(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
    Query(params): Query<DbNodeQuery>,
) -> Response {
    let db = match pick_store(&state, params.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // `KnowledgeStore::fetch_node` is the single-row hydrate; works
    // identically across OverGraph and Neo4j backends.
    match db.fetch_node(&id).await {
        Ok(Some(n)) => {
            let mut v = node_row_to_json(&n);
            let stats = db.sparse_stats();
            v["storage"] = node_storage_meta(&n, state.repo_root().as_path(), stats.as_deref());
            ok_json(v.to_string())
        }
        Ok(None) => err_json(StatusCode::NOT_FOUND, "node not found"),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("fetch_node: {}", e),
        ),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct DbTraverseQuery {
    #[serde(default = "default_db_k")]
    k: u32,
    dir: Option<String>,
    types: Option<String>,
    /// Optional destination name; defaults to the primary backend.
    dest: Option<String>,
}
fn default_db_k() -> u32 {
    2
}

pub(crate) async fn api_db_traverse(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
    Query(params): Query<DbTraverseQuery>,
) -> Response {
    let db = match pick_store(&state, params.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let hops = params.k.min(8);
    let direction = params
        .dir
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Outbound);
    let edge_types = parse_csv(params.types);

    let result = match traverse_filtered(
        &*db,
        std::slice::from_ref(&id),
        hops,
        edge_types.as_deref(),
        direction,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("traverse: {}", e),
            )
        }
    };

    let nodes_json: Vec<serde_json::Value> = result
        .nodes
        .iter()
        .map(|n| {
            let mut v = node_row_to_json(n);
            if let Some(d) = result.distances.get(&n.id) {
                v["distance"] = serde_json::Value::from(*d);
            }
            v
        })
        .collect();
    let edges_json: Vec<serde_json::Value> = result
        .edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "source": e.source,
                "target": e.target,
                "edge_type": e.edge_type,
            })
        })
        .collect();

    ok_json(
        serde_json::json!({
            "dest": db.backend_name(),
            "nodes": nodes_json,
            "edges": edges_json,
            "distances": result.distances,
        })
        .to_string(),
    )
}

#[derive(serde::Deserialize)]
pub(crate) struct SemanticBody {
    query: String,
    #[serde(default = "default_semantic_k")]
    k: usize,
    #[serde(default)]
    filter: Option<String>,
    /// Optional destination name; defaults to the primary backend.
    #[serde(default)]
    dest: Option<String>,
}
fn default_semantic_k() -> usize {
    10
}

pub(crate) async fn api_search_semantic(
    State(state): State<ServeState>,
    Json(body): Json<SemanticBody>,
) -> Response {
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let k = body.k.min(100).max(1);

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let hits = match body.filter.as_deref() {
        Some(f) => semantic_search_w_where(&*db, &embedder, &body.query, k, f).await,
        None => storage_semantic_search(&*db, &embedder, &body.query, k).await,
    };
    drop(_permit);

    let hits = match hits {
        Ok(h) => h,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("semantic_search: {}", e),
            )
        }
    };

    let body = serde_json::json!({
        "count": hits.len(),
        "dest": db.backend_name(),
        "hits": hits.iter().map(|h| {
            serde_json::json!({
                "id": h.node.id,
                "name": h.node.name,
                "node_type": h.node.node_type,
                "file": h.node.file,
                "start_line": h.node.start_line,
                "end_line": h.node.end_line,
                "description": h.node.description,
                "distance": h.distance,
            })
        }).collect::<Vec<_>>(),
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct HybridBody {
    query: String,
    #[serde(default = "default_hybrid_k")]
    k: usize,
    #[serde(default = "default_hybrid_hops")]
    hops: u32,
    #[serde(default)]
    edge_types: Option<Vec<String>>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default = "default_hybrid_max_chars")]
    max_chars: usize,
    #[serde(default = "default_hybrid_mmr_lambda")]
    mmr_lambda: f32,
    #[serde(default, rename = "where")]
    where_clause: Option<String>,
    #[serde(default = "default_hybrid_include_snippets")]
    include_snippets: bool,
    /// Optional destination name; defaults to the primary backend.
    #[serde(default)]
    dest: Option<String>,
}
fn default_hybrid_k() -> usize {
    8
}
fn default_hybrid_hops() -> u32 {
    2
}
fn default_hybrid_max_chars() -> usize {
    12_000
}
fn default_hybrid_mmr_lambda() -> f32 {
    0.6
}
fn default_hybrid_include_snippets() -> bool {
    true
}

pub(crate) async fn api_search_hybrid(
    State(state): State<ServeState>,
    Json(body): Json<HybridBody>,
) -> Response {
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let k = body.k.min(50).max(1);
    let hops = body.hops.min(4);
    let strategy = body
        .strategy
        .as_deref()
        .map(RankStrategy::from_str_lossy)
        .unwrap_or(RankStrategy::Ppr);
    let direction = body
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    let max_chars = body.max_chars.min(64_000);
    let mmr_lambda = body.mmr_lambda.clamp(0.0, 1.0);

    let edge_types_owned: Option<Vec<String>> = body.edge_types.filter(|v| !v.is_empty());

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let repo_root = state.repo_root();
    let mut opts = SearchKbOptions::new(&body.query, repo_root.as_path());
    opts.k = k;
    opts.hops = hops;
    opts.edge_types = edge_types_owned.as_deref();
    opts.direction = direction;
    opts.max_chars = max_chars;
    opts.mmr_lambda = mmr_lambda;
    opts.where_clause = body.where_clause.as_deref();
    opts.include_snippets = body.include_snippets;
    opts.strategy = strategy;

    let dest_name = db.backend_name();
    let result = storage_search_kb(&*db, &embedder, opts).await;
    drop(_permit);

    match result {
        Ok(ctx) => match serde_json::to_value(&ctx) {
            Ok(mut v) => {
                // Surface the actual backend the result came from so
                // the UI can display "results from <dest>" even when
                // the caller didn't pass an explicit `dest`.
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "dest".to_string(),
                        serde_json::Value::String(dest_name.to_string()),
                    );
                }
                ok_json(v.to_string())
            }
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
        },
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("search_kb: {}", e),
        ),
    }
}

/// `sparse_dims` is the reason this exists. It is the one cap whose effect
/// the UI could not otherwise detect: `MAX_SPARSE_DIMS` truncates silently,
/// leaving nothing in the stored text to notice. Recomputing the vector here
/// uses the same function ingest used, so the count is exact rather than
/// Split out from [`node_row_to_json`] because it costs real work — a
/// blake3 of the file on disk for the staleness check, and a sparse-vector
/// rebuild for the dimension count. That is fine for the single-row hydrate
/// behind a node click, and not fine for the traverse handler, which runs
/// `node_row_to_json` over every node it returns.
///
/// Cap on how much captured source is sent to the UI in one node hydrate.
/// A whole-file node can be megabytes; the panel is a transparency view,
/// not a file viewer, and the Preview tab reads the live file anyway.
const STORED_CODE_PREVIEW_CHARS: usize = 20_000;

/// The parts of a stored row that aren't its text: what the vector store
/// holds *about* this node rather than what it embedded.
///
/// estimated.
pub(crate) fn node_storage_meta(
    n: &storage::NodeRow,
    repo_root: &std::path::Path,
    stats: Option<&storage::sparse_stats::SparseStats>,
) -> serde_json::Value {
    let sparse_dims = storage::text::build_node_sparse_vector(&n.node_text, &n.code, stats).len();
    let stale = if n.file.is_empty() || n.file_hash.is_empty() {
        None
    } else {
        storage::file_matches_hash(repo_root, &n.file, &n.file_hash).map(|matches| !matches)
    };

    // The captured source itself, so the UI can show what the store holds
    // rather than only telling the user it holds something. This is what an
    // agent's snippet reads return and what the keyword index was built
    // from, and it can differ from the working tree — which is the whole
    // reason it is worth showing next to the Preview tab's live read.
    let code_chars = n.code.chars().count();
    let code = if code_chars > STORED_CODE_PREVIEW_CHARS {
        let cut = n
            .code
            .char_indices()
            .nth(STORED_CODE_PREVIEW_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(n.code.len());
        n.code[..cut].to_string()
    } else {
        n.code.clone()
    };

    serde_json::json!({
        "last_update_at": n.last_update_at,
        "file_hash": n.file_hash,
        "code": code,
        "code_truncated": code_chars > STORED_CODE_PREVIEW_CHARS,
        "code_chars": code_chars,
        "node_text_chars": n.node_text.chars().count(),
        "vector_dim": n.vector.len(),
        "sparse_dims": sparse_dims,
        // `null` when the file can't be read at all (deleted, or a binary
        // document that was never captured) — distinct from "not stale".
        "stale": stale,
    })
}

pub(crate) fn node_row_to_json(n: &storage::NodeRow) -> serde_json::Value {
    serde_json::json!({
        "id": n.id,
        "name": n.name,
        "node_type": n.node_type,
        "file": n.file,
        "start_line": n.start_line,
        "end_line": n.end_line,
        "description": n.description,
        // Full chunk text — powers the right-panel "Preview" tab.
        "node_text": n.node_text,
        // Boundary facts, as the store holds them (comma-joined scalars —
        // see `storage::facts`). The canvas reads the structured
        // `GraphNode::boundaries` from graph.json instead; these are here so
        // a caller hitting the DB route directly is not told less than a
        // caller hitting the graph route.
        "boundary_kinds": fact_str(n, "boundary_kinds"),
        "boundary_detail": fact_str(n, "boundary_detail"),
    })
}

/// A string-valued fact, or `null` when the node does not carry it.
pub(crate) fn fact_str(n: &storage::NodeRow, key: &str) -> Option<String> {
    match n.facts.get(key) {
        Some(storage::facts::FactValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}
