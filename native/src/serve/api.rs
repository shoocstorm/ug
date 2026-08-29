//! `api.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use axum::extract::{Json, Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use ultragraph::{calculate_centrality as lib_centrality, detect_cycles as lib_detect_cycles};

use super::encoding::asset_response;
use super::nidx::build_binary_index;
use super::registry::{ctx_indexed_source, resolve_ctx};
use super::snapshot::{build_adj, build_slim_index, server_mode_bytes, SearchMemo};
use super::*;
use ultragraph::types::{GraphEdge, GraphNode};

/// GET /api/tools — discovery for the graph-backed agent tools, so an agent
/// speaking HTTP can enumerate them the way an MCP client reads `tools/list`.
/// Lists the store-backed tools too (`POST /api/tools/:tool` dispatches those
/// through their own arm, not [`agent_tools::run_tool`]), so nothing an agent
/// can call is invisible.
pub(crate) async fn api_tools() -> Response {
    let store_backed: Vec<&str> = ultragraph::agent_tools::STORE_BACKED_AGENT_TOOLS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    let tools: Vec<serde_json::Value> = ultragraph::agent_tools::AGENT_TOOLS
        .iter()
        .chain(ultragraph::agent_tools::STORE_BACKED_AGENT_TOOLS.iter())
        .map(|(name, summary)| {
            let mut entry = serde_json::json!({
                "name": name,
                "summary": summary,
                "path": format!("/api/tools/{}", name),
                "method": "POST",
                // A copyable body beats a parameter list: it shows the shape
                // and the wildcard form in the same round trip.
                "example": ultragraph::agent_tools::tool_example(name),
            });
            if store_backed.contains(name) {
                entry["note"] = serde_json::json!(
                    "Store-backed: needs the indexed database (503 otherwise). \
                     The analyze preset catalog lives at GET /api/presets."
                );
            }
            entry
        })
        .collect();
    ok_json(
        serde_json::json!({
            "tools": tools,
            "params": "Canonical snake_case, same as the CLI flags and MCP tool params. Legacy camelCase spellings are accepted.",
            "project": "Optional `project` field targets another indexed project without changing the server's active one.",
            "wildcards": {
                "syntax": ultragraph::pattern::SYNTAX_SUMMARY,
                "where": "Anywhere a symbol or file is named: find_symbols.name / .node_types / .file_prefix, file_outline.file, and the node_id of get_code, find_usages, traverse and shortest_path — which also accept a plain symbol name instead of an id.",
                "expansion": format!(
                    "In the id-taking tools a name or pattern expands to at most {} symbols; hitting that cap is reported in the result, never silent.",
                    ultragraph::agent_tools::MAX_REF_EXPANSION
                ),
            },
        })
        .to_string(),
    )
}

/// GET /api/presets — the `analyze` preset registry.
///
/// Served from the same registry the MCP `graph_schema` manifest reads, so
/// the UI and an agent can never disagree about what exists. `source` is
/// there for the day presets can also come from a repo's
/// `.ug/presets.toml`: a card built from the working tree needs to be
/// visibly distinguishable from one ug shipped.
pub(crate) async fn api_presets() -> Response {
    let presets: Vec<serde_json::Value> = ultragraph::analyze::presets::all()
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "category": p.category.as_str(),
                "description": p.description,
                "source": "builtin",
                "params": p.params.iter().map(|q| serde_json::json!({
                    "name": q.name,
                    "description": q.description,
                    "required": q.default.is_none(),
                })).collect::<Vec<_>>(),
                "headline": p.headline,
            })
        })
        .collect();
    ok_json(
        serde_json::json!({
            "presets": presets,
            // Shipped alongside so the UI's "what can I query?" and the MCP
            // capability manifest cannot drift apart — both read the same
            // constant, and neither hardcodes a list that quietly goes stale
            // when a fact is added.
            "properties": ultragraph::analyze::QUERYABLE_PROPERTIES,
            "run": "POST /api/tools/analyze with {\"preset\": \"<name>\", \"args\": {…}}",
        })
        .to_string(),
    )
}

/// `analyze` over HTTP.
///
/// Split out of [`api_tool`] because it is the one tool there that needs
/// the store rather than `graph.json` — aggregation and reachability run
/// on indexed properties. Answers from the *resolved* project's store, so
/// a request carrying `project` analyses the project it named, like every
/// other tool answers from its graph. Returns rows as JSON rather than the
/// rendered text an agent reads, since the caller is the viz layer, which
/// wants to build its own table and map result ids onto graph selection.
pub(crate) async fn api_analyze(ctx: &ProjectContext, params: serde_json::Value) -> Response {
    let store = match ctx.stores.as_ref().and_then(|s| s.get(&s.primary)) {
        Some(s) => s.clone(),
        None => {
            let reason = ctx
                .db_unavailable_reason
                .as_deref()
                .unwrap_or("DB not opened");
            return err_json(StatusCode::SERVICE_UNAVAILABLE, reason);
        }
    };
    // Validation lives in `analyze::run` — an unknown preset, a missing
    // required argument and a malformed query all come back from there with
    // a message that names the alternatives, which is more than this layer
    // could say.
    let request = parse_analyze_body(&params);

    match ultragraph::analyze::run(store.as_ref(), &request).await {
        Ok(answer) => {
            // Ship only the requested window, not the whole result.
            // Returning every row and leaving the client to slice would
            // defeat the point of a range — the expensive part of paging
            // is transferring rows the caller already has.
            let total = answer.page.rows.len();
            let (from, to) = answer.window.slice(total);
            ok_json(
                serde_json::json!({
                    "title": answer.title,
                    "description": answer.description,
                    "gql": answer.gql,
                    "columns": answer.page.columns,
                    "rows": answer.page.rows[from..to].iter().map(|r| {
                        r.iter().map(query_value_to_json).collect::<Vec<_>>()
                    }).collect::<Vec<_>>(),
                    // Three different denominators, all needed to page honestly:
                    // how many rows exist, which ones these are, and how many
                    // graph elements matched before grouping collapsed them.
                    "rowsTotal": total,
                    "from": from + 1,
                    "to": to,
                    "rowsMatched": answer.page.rows_matched,
                    "truncated": answer.page.truncated,
                    "warnings": answer.page.warnings,
                    // Coverage rides along rather than being left to the
                    // client to remember: a chart drawn over an unindexed
                    // property is the same confident lie as a printed zero.
                    "coverage": answer.coverage.iter().map(|c| serde_json::json!({
                        "property": c.property,
                        "present": c.present,
                        "total": c.total,
                    })).collect::<Vec<_>>(),
                    "unindexed": answer.unindexed,
                    "text": ultragraph::analyze::render::render(
                        &answer,
                        ultragraph::agent_tools::Render::Markdown,
                    ),
                })
                .to_string(),
            )
        }
        Err(e) => err_json(StatusCode::BAD_REQUEST, &e),
    }
}

fn query_value_to_json(v: &ultragraph::storage::store::QueryValue) -> serde_json::Value {
    use ultragraph::storage::store::QueryValue as Q;
    match v {
        Q::Null => serde_json::Value::Null,
        Q::Bool(b) => serde_json::Value::Bool(*b),
        Q::Int(i) => serde_json::Value::from(*i),
        Q::Float(f) => serde_json::Value::from(*f),
        Q::Str(s) => serde_json::Value::from(s.clone()),
        Q::List(items) => serde_json::Value::Array(items.iter().map(query_value_to_json).collect()),
    }
}

fn parse_analyze_body(body: &serde_json::Value) -> ultragraph::analyze::AnalyzeParams {
    let text = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let mut parsed = ultragraph::analyze::AnalyzeParams {
        preset: text("preset"),
        gql: text("gql"),
        limit: body
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
        range: text("range"),
        ..Default::default()
    };
    if let Some(obj) = body.get("args").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            let as_text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                // A model often sends a list param as a JSON array
                // (`{"files": ["a.ts","b.rs"]}`). analyze binds list params
                // from a comma-separated string, so join the string elements
                // rather than stringify the array (which would keep the
                // brackets and break the split) — same as the MCP side.
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                other => other.to_string(),
            };
            parsed.args.insert(k.clone(), as_text);
        }
    }
    parsed
}

/// POST /api/tools/:name — run one graph-backed agent tool and return its
/// JSON envelope. Same dispatch, params and output as `ug <name> --json` and
/// the matching MCP tool.
pub(crate) async fn api_tool(
    State(state): State<ServeState>,
    AxPath(tool): AxPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let mut params = match body {
        Some(Json(serde_json::Value::Object(m))) => m,
        // No body, or a non-object body, means "no params" — valid for
        // project_overview and graph_schema.
        _ => serde_json::Map::new(),
    };
    let project = params
        .remove("project")
        .and_then(|v| v.as_str().map(str::to_string));

    let ctx = match resolve_ctx(&state.registry, project.as_deref()).await {
        Ok(c) => c,
        Err(e) if e.starts_with("unknown project") => return err_json(StatusCode::NOT_FOUND, &e),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };

    // `analyze` is store-backed, so it never reaches `run_tool` — that
    // dispatcher only knows about graph.json. It still gets the same arg
    // coercion as the MCP and chat paths, and answers from the resolved
    // project's store just like every other tool answers from its graph.
    if tool == "analyze" {
        let mut args = serde_json::Value::Object(params);
        crate::mcp::tools::normalize_args(&tool, &mut args);
        return api_analyze(&ctx, args).await;
    }

    let snap = ctx.graph.read().expect("graph state poisoned").clone();
    // Same coercion the chat and MCP paths apply — every entry point sees
    // the same stringified-array mistakes, so normalise in all of them.
    let mut args = serde_json::Value::Object(params);
    crate::mcp::tools::normalize_args(&tool, &mut args);
    let indexed = ctx_indexed_source(&ctx, &snap.parsed, &tool, &args).await;
    let result = ultragraph::agent_tools::run_tool(
        &tool,
        &snap.parsed,
        ultragraph::agent_tools::SourceCtx::new(&indexed, ctx.repo_root.as_path()),
        ctx.graph_path.as_path(),
        args,
        None,
    );

    match result {
        Ok(ultragraph::agent_tools::ToolOutput::Json(v)) => ok_json(v.to_string()),
        // `run_tool` only returns Text when a render style was requested.
        Ok(ultragraph::agent_tools::ToolOutput::Text(t)) => {
            ok_json(serde_json::json!({ "text": t }).to_string())
        }
        Err(e) if e.starts_with("Unknown agent tool") => err_json(StatusCode::NOT_FOUND, &e),
        Err(e) => err_json(StatusCode::BAD_REQUEST, &e),
    }
}

// ---------- API helpers ----------

pub(crate) fn ok_json(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

pub(crate) fn err_json(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

pub(crate) fn parse_csv(s: Option<String>) -> Option<Vec<String>> {
    s.and_then(|raw| {
        let v: Vec<String> = raw
            .split(',')
            .filter_map(|p| {
                let t = p.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    })
}

// ---------- API handlers (Phase 2) ----------

pub(crate) async fn api_stats(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    ok_json(snap.stats.get_or_init(|| render_stats(&snap)).clone())
}

/// Build the `/api/graph/stats` body. Called once per snapshot — see the
/// `stats` field on [`GraphSnapshot`].
pub(crate) fn render_stats(snap: &GraphSnapshot) -> String {
    let mut node_types: BTreeMap<&'static str, usize> = BTreeMap::new();
    for n in &snap.parsed.nodes {
        *node_types.entry(n.node_type.as_str()).or_insert(0) += 1;
    }
    let mut edge_types: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in &snap.parsed.edges {
        *edge_types.entry(e.edge_type.as_str()).or_insert(0) += 1;
    }
    // The repo-root folder node carries the per-language file counts and the
    // code/docs/mixed classification the indexer computed.
    let root_folder = snap
        .parsed
        .nodes
        .iter()
        .filter_map(|n| n.folder.as_ref())
        .min_by_key(|f| f.depth);

    let body = serde_json::json!({
        "nodes": snap.parsed.nodes.len(),
        "edges": snap.parsed.edges.len(),
        "node_types": node_types,
        "edge_types": edge_types,
        "graph_bytes": snap.graph_bytes,
        // Indexer-side counts (files, lines, timing). Present in graph.json
        // all along; surfaced here so the UI can show what was scanned, not
        // just what ended up in the graph.
        "index": snap.parsed.stats.as_ref().map(|s| serde_json::json!({
            "files": s.total_files,
            "cached_files": s.cached_files,
            "symbols": s.total_symbols,
            "folders": s.total_folders,
            "lines": s.total_lines,
            "indexed_at": s.last_indexed_at,
            "indexing_time_ms": s.indexing_time_ms,
            "repo_root": s.repo_root,
        })),
        "languages": root_folder.map(|f| f.language_breakdown.clone()),
        "kb_type": root_folder
            .and_then(|f| f.classification.as_ref())
            .map(|c| format!("{:?}", c).to_lowercase()),
    });
    body.to_string()
}

#[derive(serde::Deserialize)]
pub(crate) struct HydrateBody {
    /// Node indices into the slim index.
    ids: Vec<u32>,
}

/// `POST /api/graph/nodes/hydrate` — the heavy fields the slim index omits.
///
/// The slim index carries what the page needs *before* you click something:
/// id, name, type, file, lines, boundary flag. Everything else — docstring,
/// signature, metrics, imports, calls, extends, implements, the boundary
/// details — is per-node prose that only the info panel reads, and shipping it
/// for 162k nodes is most of what made `graph.json` 346 MB.
///
/// Batched because a selection asks about one node but a panel full of chips
/// can ask about dozens, and the singular `/api/graph/node/*id` is one request
/// each.
pub(crate) async fn api_hydrate(
    State(state): State<ServeState>,
    Json(body): Json<HydrateBody>,
) -> Response {
    let snap = state.snapshot();
    let n = snap.parsed.nodes.len();
    let nodes: Vec<&GraphNode> = body
        .ids
        .iter()
        .filter_map(|&i| snap.parsed.nodes.get(i as usize))
        .collect();
    match serde_json::to_string(
        &serde_json::json!({ "ids": body.ids, "nodes": nodes, "n": n, "token": snap.token() }),
    ) {
        Ok(s) => ok_json(s),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct EdgesBody {
    /// Node **indices** into the slim index, not string ids. Position is
    /// identity in server mode, and a 141-character id per endpoint is what
    /// this whole protocol exists to avoid sending.
    ids: Vec<u32>,
    /// `"incident"` (default) — every edge touching each id, which is what
    /// makes that id's adjacency list *complete*. `"induced"` — only edges
    /// whose **both** endpoints are in `ids`, used to fill in the cross-links
    /// between nodes already on the canvas.
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /api/graph/edges` — the one primitive server mode is built on.
///
/// Solo expansion, the Related tab, focus/Tab stepping, the graph walk and the
/// tour all reduce to "give me the edges around these nodes", so they all come
/// through here. Answering it is O(sum of degree) rather than O(edges) because
/// `AdjIndex` holds edge indices — see the note on that struct.
///
/// The response is columnar and index-based for the same reason the slim index
/// is: a 1,500-node neighbourhood is roughly 14,000 edges, which is ~4 MB as id
/// strings and ~200 KB as integers.
pub(crate) async fn api_edges(
    State(state): State<ServeState>,
    Json(body): Json<EdgesBody>,
) -> Response {
    let snap = state.snapshot();
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));
    let induced = body.scope.as_deref() == Some("induced");

    let n = snap.parsed.nodes.len();
    let wanted: HashSet<u32> = body
        .ids
        .iter()
        .copied()
        .filter(|&i| (i as usize) < n)
        .collect();

    // Collected by edge index and deduped: an edge incident to two requested
    // nodes would otherwise be sent twice, and the client stores edge objects
    // by identity.
    let mut edge_idx: Vec<u32> = Vec::new();
    for &i in &wanted {
        edge_idx.extend(adj.incident(i as usize));
    }
    edge_idx.sort_unstable();
    edge_idx.dedup();

    let mut rel_types: Vec<&'static str> = Vec::new();
    let mut rel_idx: HashMap<&'static str, u32> = HashMap::new();
    let (mut src, mut tgt, mut rel) = (Vec::new(), Vec::new(), Vec::new());

    for ei in edge_idx {
        let e = &snap.parsed.edges[ei as usize];
        // Endpoints resolved once when the index was built, not hashed again
        // per edge — a hub click asks about 8,680 of them.
        let (Some(si), Some(ti)) = (adj.src_of(ei), adj.tgt_of(ei)) else {
            continue;
        };
        if induced && !(wanted.contains(&si) && wanted.contains(&ti)) {
            continue;
        }
        let name = e.edge_type.as_str();
        let next = rel_types.len() as u32;
        let ri = *rel_idx.entry(name).or_insert_with(|| {
            rel_types.push(name);
            next
        });
        src.push(si);
        tgt.push(ti);
        rel.push(ri);
    }

    ok_json(
        serde_json::json!({
            "src": src,
            "tgt": tgt,
            "rel": rel,
            "relTypes": rel_types,
            // Which ids now have their *whole* edge list on the client. Only
            // an incident query can claim this: an induced query deliberately
            // withholds edges that leave the set, so marking its ids complete
            // would cache a half-answer as a whole one.
            "complete": if induced { Vec::new() } else { body.ids.clone() },
            "token": snap.token(),
        })
        .to_string(),
    )
}

/// `GET /api/graph/nodes` — the slim node index (see [`build_slim_index`]).
///
/// Encoded once per snapshot and served from the cache thereafter, the same
/// deal `/graph.json` gets. The build runs on a blocking thread: it walks every
/// node and every edge and serialises ~34 MB, which is not something to do on a
/// runtime worker.
pub(crate) async fn api_slim_index(
    State(state): State<ServeState>,
    headers: HeaderMap,
) -> Response {
    let snap = state.snapshot();
    let built = tokio::task::spawn_blocking(move || {
        snap.slim
            .get_or_init(|| {
                EncodedAsset::new(
                    build_slim_index(&snap.parsed).into_bytes(),
                    "application/json; charset=utf-8",
                )
            })
            .clone()
    })
    .await;
    match built {
        Ok(asset) => asset_response(&asset, &headers),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("slim index task failed: {}", e),
        ),
    }
}

/// `GET /api/graph/nodes.bin` — the same index as [`build_binary_index`].
///
/// Cached and served exactly like the JSON one. Content type is
/// `application/octet-stream` so nothing between here and the page tries to
/// re-encode it; the compression middleware still applies, and the frame
/// gzips to roughly a tenth of its size because the columns are far more
/// regular than the JSON was.
pub(crate) async fn api_slim_index_bin(
    State(state): State<ServeState>,
    headers: HeaderMap,
) -> Response {
    let snap = state.snapshot();
    let built = tokio::task::spawn_blocking(move || {
        snap.slim_bin
            .get_or_init(|| {
                EncodedAsset::new(build_binary_index(&snap.parsed), "application/octet-stream")
            })
            .clone()
    })
    .await;
    match built {
        Ok(asset) => asset_response(&asset, &headers),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("binary index task failed: {}", e),
        ),
    }
}

pub(crate) async fn api_node(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
) -> Response {
    let snap = state.snapshot();
    // Through the adjacency index's `id_to_idx`, not a linear scan. The map was
    // already being built for traverse/path; looking one node up by walking all
    // 162k of them was an O(V) answer to an O(1) question, and the node panel
    // fires one of these per selection.
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));
    match adj.id_to_idx.get(&id).map(|&i| &snap.parsed.nodes[i]) {
        Some(n) => match serde_json::to_string(n) {
            Ok(s) => ok_json(s),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
        },
        None => err_json(StatusCode::NOT_FOUND, "node not found"),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct SearchParams {
    q: Option<String>,
    types: Option<String>,
    limit: Option<usize>,
    /// `fields=id` trims each hit to what the page actually reads.
    ///
    /// The default shape is the whole `GraphNode` — metrics, signature,
    /// docstring, imports, calls, annotations — which is what the endpoint was
    /// built for and what a hand-driven caller still wants. The search box
    /// reads two things per hit: the id, to resolve through the node store,
    /// and whether the node has any boundaries. On a large graph that is
    /// 133 KB serialised per keystroke against ~30 KB of answer. See P10.9 in
    /// docs/dev/PERF-TUNING-JOURNEY.md.
    fields: Option<String>,
}

/// Nodes returned by `/api/graph/search` when the caller does not say.
const SEARCH_DEFAULT_LIMIT: usize = 200;
/// Ceiling on `?limit=`, so the endpoint cannot be asked to serialise the
/// whole graph however the query is spelled.
const SEARCH_MAX_LIMIT: usize = 5_000;

/// Reduce a match list to the best `limit` of it, in order.
///
/// The sort key is `(where the needle appears in the name, name length,
/// position)`, so a prefix match beats a mid-word one and the shorter of two
/// equals wins. Only the kept prefix is ordered: on a query matching most of a
/// 485k-node graph, fully sorting it to return 200 rows would be the whole cost
/// of the request.
fn keep_best_matches(ranked: &mut Vec<(usize, usize, usize)>, limit: usize) {
    let count = ranked.len();
    let keep = limit.min(count);
    if keep == 0 {
        // Not just an optimisation: `select_nth_unstable(0)` panics on an
        // empty slice, and a query that matches nothing is routine.
        ranked.clear();
        return;
    }
    if keep < count {
        ranked.select_nth_unstable(keep - 1);
        ranked.truncate(keep);
    }
    ranked.sort_unstable();
}

/// Rank name matches the way the search box does, and keep only the best
/// `limit` of them. Returns positions into `names`.
///
/// Split out of `api_search` so the ordering — which decides what the page
/// shows when a query matches more nodes than it can return — is testable
/// without a snapshot.
#[cfg(test)]
pub(crate) fn rank_search_matches(names: &[&str], needle: &str, limit: usize) -> Vec<usize> {
    let mut ranked: Vec<(usize, usize, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            let lower = n.to_lowercase();
            lower.find(needle).map(|at| (at, n.len(), i))
        })
        .collect();
    keep_best_matches(&mut ranked, limit);
    ranked.into_iter().map(|(_, _, i)| i).collect()
}

pub(crate) async fn api_search(
    State(state): State<ServeState>,
    Query(params): Query<SearchParams>,
) -> Response {
    let snap = state.snapshot();
    let needle = params.q.unwrap_or_default().to_lowercase();
    let type_filter: Option<Vec<String>> =
        parse_csv(params.types).map(|v| v.into_iter().map(|t| t.to_lowercase()).collect());
    let limit = params
        .limit
        .unwrap_or(SEARCH_DEFAULT_LIMIT)
        .min(SEARCH_MAX_LIMIT);

    // `count` stays the number of *matches*, not the number returned, so a
    // caller can tell "200 of 162,000" from "200 of 200". Unbounded before
    // this: an empty `?q=` with no `?types=` matched every node and
    // serialised all 162k of them, which is neither a useful answer nor one
    // the page could render.
    // Ranked, not first-come. The `limit` cut used to fall on whichever
    // matches appeared earliest in the node list, which was fine while this
    // endpoint was only reachable by hand — but in server mode it *is* the
    // page's search box, and "the 200 that happen to be first" is a different
    // answer from "the best 200". The key is the one the box has always
    // sorted by: where the needle appears in the name, then the shorter name.
    // A match on the id or the docstring only sorts after every name match.
    //
    // Scanned over the previous needle's matches when this one extends it,
    // rather than over every node — see [`GraphSnapshot::search_memo`]. The
    // candidate set is the only thing that changes: every node it yields runs
    // through the identical filter, match and rank below, so the answer is the
    // same set in the same order.
    let narrowed: Option<Arc<Vec<u32>>> = if needle.is_empty() {
        None
    } else {
        snap.search_memo
            .lock()
            .ok()
            .and_then(|g| match g.as_ref() {
                Some(m) if !m.needle.is_empty() && needle.starts_with(&m.needle) => {
                    Some(Arc::clone(&m.hits))
                }
                _ => None,
            })
    };

    // Every node examined that matched the needle, in graph order — what the
    // *next* keystroke narrows to. Recorded before the type filter, which is
    // not part of the memo's key.
    let mut hits: Vec<u32> = Vec::new();
    let mut ranked: Vec<(usize, usize, usize)> = Vec::new();

    let candidates: Box<dyn Iterator<Item = (usize, &GraphNode)>> = match &narrowed {
        Some(prev) => Box::new(
            prev.iter()
                .map(|&i| (i as usize, &snap.parsed.nodes[i as usize])),
        ),
        None => Box::new(snap.parsed.nodes.iter().enumerate()),
    };

    for (i, n) in candidates {
        if let Some(types) = &type_filter {
            // `eq_ignore_ascii_case` against the static name, rather than
            // allocating a lowercased `String` per node per request. The
            // filter list is already lowercased once by the caller.
            let nt = n.node_type.as_str();
            if !types.iter().any(|t| t.eq_ignore_ascii_case(nt)) {
                continue;
            }
        }
        let mut rank = 0usize;
        if !needle.is_empty() {
            let lower = n.name.to_lowercase();
            let at = lower.find(&needle);
            let name_match = at.is_some();
            rank = at.unwrap_or(usize::MAX);
            // The qualified id too. In server mode this endpoint *is* the
            // page's search box (the client has no name column it can afford
            // to scan — see §Round 2 of docs/dev/PERF-TUNING-JOURNEY.md), and
            // the box has always matched the id as well as the name. Without
            // this, searching for a path fragment silently stopped working on
            // exactly the large graphs server mode exists for.
            let id_match = !name_match && n.id.to_lowercase().contains(&needle);
            let doc_match = n
                .docstring
                .as_ref()
                .map(|d| d.to_lowercase().contains(&needle))
                .unwrap_or(false);
            if !name_match && !id_match && !doc_match {
                continue;
            }
            hits.push(i as u32);
        }
        ranked.push((rank, n.name.len(), i));
    }

    // Only an *unfiltered* request may record the memo. The type filter runs
    // before the match above, so a filtered request never even tests the nodes
    // its `?types=` excluded — `hits` is a subset of the needle's true matches,
    // and storing it would silently under-answer the next request that omits
    // the filter. Reading is always safe: what is stored is always unfiltered.
    if !needle.is_empty() && type_filter.is_none() {
        if let Ok(mut g) = snap.search_memo.lock() {
            *g = Some(SearchMemo {
                needle: needle.clone(),
                hits: Arc::new(hits),
            });
        }
    }

    let count = ranked.len();
    keep_best_matches(&mut ranked, limit);
    let matched: Vec<&GraphNode> = ranked
        .iter()
        .map(|&(_, _, i)| &snap.parsed.nodes[i])
        .collect();

    // Columnar when trimmed, matching `/api/graph/edges` — parallel arrays
    // rather than 200 objects with one field each.
    let body = if params.fields.as_deref() == Some("id") {
        serde_json::json!({
            "count": count,
            "returned": matched.len(),
            "truncated": count > matched.len(),
            "limit": limit,
            "ids": matched.iter().map(|n| &n.id).collect::<Vec<_>>(),
            "boundary": matched
                .iter()
                .map(|n| !n.boundaries.is_empty())
                .collect::<Vec<_>>(),
        })
    } else {
        serde_json::json!({
            "count": count,
            "returned": matched.len(),
            "truncated": count > matched.len(),
            "limit": limit,
            "nodes": matched,
        })
    };
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct BfsParams {
    #[serde(default = "default_k")]
    k: u32,
}
fn default_k() -> u32 {
    1
}

pub(crate) async fn api_traverse(
    State(state): State<ServeState>,
    AxPath(id): AxPath<String>,
    Query(params): Query<BfsParams>,
) -> Response {
    // Cap to keep an open server from being a runaway-expansion foot-gun.
    let k = params.k.min(8);
    let snap = state.snapshot();
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));

    let Some(&start) = adj.id_to_idx.get(&id) else {
        return ok_json(
            serde_json::json!({ "nodes": [], "edges": [], "distances": {} }).to_string(),
        );
    };

    let mut visited: HashSet<usize> = HashSet::new();
    let mut distances: HashMap<usize, u32> = HashMap::new();
    let mut queue: VecDeque<(usize, u32)> = VecDeque::new();
    queue.push_back((start, 0));
    visited.insert(start);
    distances.insert(start, 0);

    while let Some((idx, d)) = queue.pop_front() {
        if d == k {
            continue;
        }
        for ei in adj.outgoing(idx) {
            let Some(nb) = adj.tgt_of(ei).map(|t| t as usize) else {
                continue;
            };
            if visited.insert(nb) {
                distances.insert(nb, d + 1);
                queue.push_back((nb, d + 1));
            }
        }
    }

    let nodes: Vec<&GraphNode> = visited.iter().map(|&i| &snap.parsed.nodes[i]).collect();
    // Induced edges: both endpoints inside the visited set. Reached through the
    // visited nodes' own incident lists rather than by filtering the whole edge
    // list, which is the difference between O(reached degree) and O(edges) —
    // the latter being three quarters of a million per request on a large repo.
    // Collected by index and sorted, so the response keeps `graph.json` order
    // rather than inheriting a HashSet's.
    let mut edge_idx: Vec<u32> = visited
        .iter()
        .flat_map(|&i| adj.incident(i))
        .filter(|&ei| {
            let e = &snap.parsed.edges[ei as usize];
            matches!(
                (adj.id_to_idx.get(&*e.source), adj.id_to_idx.get(&*e.target)),
                (Some(si), Some(ti)) if visited.contains(si) && visited.contains(ti)
            )
        })
        .collect();
    edge_idx.sort_unstable();
    edge_idx.dedup();
    let edges: Vec<&GraphEdge> = edge_idx
        .iter()
        .map(|&ei| &snap.parsed.edges[ei as usize])
        .collect();
    let dist_by_id: HashMap<&str, u32> = distances
        .iter()
        .map(|(&i, &d)| (snap.parsed.nodes[i].id.as_str(), d))
        .collect();

    let body = serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "distances": dist_by_id,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct PathQuery {
    source: String,
    target: String,
}

pub(crate) async fn api_path(
    State(state): State<ServeState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let snap = state.snapshot();
    let adj = snap.adj.get_or_init(|| build_adj(&snap.parsed));

    let not_found = || ok_json(serde_json::json!({ "path": [], "found": false }).to_string());
    let (Some(&src), Some(&tgt)) = (
        adj.id_to_idx.get(&params.source),
        adj.id_to_idx.get(&params.target),
    ) else {
        return not_found();
    };

    // BFS with predecessor tracking — directed, forward edges only (matches CLI).
    let n = snap.parsed.nodes.len();
    let mut prev: Vec<Option<usize>> = vec![None; n];
    let mut visited: Vec<bool> = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    visited[src] = true;
    queue.push_back(src);

    let mut found = false;
    while let Some(cur) = queue.pop_front() {
        if cur == tgt {
            found = true;
            break;
        }
        for ei in adj.outgoing(cur) {
            let Some(nb) = adj.tgt_of(ei).map(|t| t as usize) else {
                continue;
            };
            if !visited[nb] {
                visited[nb] = true;
                prev[nb] = Some(cur);
                queue.push_back(nb);
            }
        }
    }

    if !found {
        return not_found();
    }

    let mut path_idx: Vec<usize> = Vec::new();
    let mut cur = tgt;
    loop {
        path_idx.push(cur);
        if cur == src {
            break;
        }
        match prev[cur] {
            Some(p) => cur = p,
            None => return not_found(),
        }
    }
    path_idx.reverse();
    let path: Vec<&str> = path_idx
        .iter()
        .map(|&i| snap.parsed.nodes[i].id.as_str())
        .collect();
    let length = (path.len() as u32).saturating_sub(1);

    let body = serde_json::json!({
        "path": path,
        "found": true,
        "length": length,
    });
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct FilterParams {
    types: Option<String>,
}

pub(crate) async fn api_filter(
    State(state): State<ServeState>,
    Query(params): Query<FilterParams>,
) -> Response {
    let Some(types) = parse_csv(params.types) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "?types= is required (comma-separated)",
        );
    };
    let lowered: Vec<String> = types.into_iter().map(|t| t.to_lowercase()).collect();
    let snap = state.snapshot();

    let matched: Vec<&GraphEdge> = snap
        .parsed
        .edges
        .iter()
        .filter(|e| {
            let et = e.edge_type.as_str();
            lowered.iter().any(|t| t.eq_ignore_ascii_case(et))
        })
        .collect();

    let body = serde_json::json!({
        "count": matched.len(),
        "edges": matched,
    });
    ok_json(body.to_string())
}

/// Betweenness centrality is O(V·E) — seconds of pure CPU on a 15k-node graph.
/// Computing it inline would park a runtime worker for that whole time and
/// stall every other in-flight request, so it goes to `spawn_blocking`. The
/// `OnceLock` still makes it a once-per-snapshot cost; this only governs where
/// that one computation runs.
pub(crate) async fn api_centrality(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    match tokio::task::spawn_blocking(move || {
        snap.centrality
            .get_or_init(|| {
                serde_json::to_string(&lib_centrality(&snap.parsed)).unwrap_or_default()
            })
            .clone()
    })
    .await
    {
        Ok(cached) => ok_json(cached),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("centrality task failed: {}", e),
        ),
    }
}

/// Same reasoning as [`api_centrality`] — cycle detection walks the whole graph
/// and must not run on a runtime thread.
pub(crate) async fn api_cycles(State(state): State<ServeState>) -> Response {
    let snap = state.snapshot();
    match tokio::task::spawn_blocking(move || {
        snap.cycles
            .get_or_init(|| {
                serde_json::to_string(&lib_detect_cycles(&snap.parsed)).unwrap_or_default()
            })
            .clone()
    })
    .await
    {
        Ok(cached) => ok_json(cached),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cycles task failed: {}", e),
        ),
    }
}

// ---------- Capabilities ----------

/// Surfaces enough state for the visualization UI to gate DB-dependent
/// panels (semantic / hybrid search) without having to make a probe
/// request per panel. `search_ready` is the single boolean the UI keys
/// off — it requires DB open, embedder configured, **and** at least one
/// node row in the table (an opened-but-empty DB still 200s on the
/// existing routes but returns nothing useful).
/// The `limits` block of `/api/capabilities`: every cap that shaped what
/// is in the store, plus the embedder's own token window.
///
/// The list comes from `ultragraph::limits`, which reads the enforcing
/// constants directly so the published numbers can't drift from the real
/// ones. Two entries are added here rather than there: the MCP snippet
/// preview, which is this binary's formatting concern and not the library's,
/// and the model token window, which depends on the embedder this server
/// happens to have open.
///
/// `embedder_token_window` is the honest headline. It binds *above* every
/// cap in the list — text past it is dropped by the tokenizer with no
/// truncation marker anywhere — and with the default 512-token model it
/// already sits below `document_page_text`. `null` means the active model
/// isn't one whose window we can state; see `limits::model_token_window`.
fn indexing_limits(state: &ServeState) -> serde_json::Value {
    let model = state
        .embedder
        .as_ref()
        .map(|e| e.config().model.clone())
        .unwrap_or_default();
    // The server has no `--section-cap` of its own — it reports the budget
    // the *model* implies. A store ingested under a pinned cap can differ;
    // the per-node `Storage metadata` is what shows what actually happened.
    let budget = ultragraph::limits::EmbedBudget::resolve(&model, None);

    let mut caps: Vec<serde_json::Value> = ultragraph::limits::all(&budget)
        .iter()
        .map(|l| serde_json::to_value(l).unwrap_or(serde_json::Value::Null))
        .collect();

    caps.push(serde_json::json!({
        "id": "mcp_snippet_preview",
        "label": "MCP snippet preview",
        "value": crate::mcp::format::SNIPPET_PREVIEW_CHARS,
        "unit": "chars",
        "stage": "retrieve",
        "extensions": [],
        "effect": "How much of each snippet an MCP search prints inline. Truncation \
                   is marked, and the full slice is one get_code call away.",
        "source": "mcp/format.rs:SNIPPET_PREVIEW_CHARS",
    }));

    serde_json::json!({
        "embedder_model": if model.is_empty() { serde_json::Value::Null } else { model.clone().into() },
        "embedder_token_window": budget.window_tokens,
        // How the description budget was arrived at: "auto" from the
        // model's window, or "default" when that window is unknown. The
        // server never sees a --section-cap, so it never reports "flag".
        "budget_source": budget.source,
        "advisory": budget.advisory(&model),
        "related_advisory": budget.related_advisory(),
        "caps": caps,
        "docs": "docs/INDEXING-AND-CHUNKING.md",
    })
}

pub(crate) async fn api_capabilities(State(state): State<ServeState>) -> Response {
    let active_stores = state.stores();
    let db_ready = active_stores.is_some();
    let embedder_ready = state.embedder.is_some();

    // Per-destination probe + serialization. `db_node_count` and
    // `search_ready` at the top level reflect the primary backend so
    // existing clients keep working; the new `destinations` array is
    // what the UI keys off for the selector.
    let mut destinations_json: Vec<serde_json::Value> = Vec::new();
    let mut primary_count: Option<usize> = None;
    if let Some(stores) = active_stores.clone() {
        for name in stores.names() {
            let store = stores.get(&name).cloned();
            let cell = stores.node_counts.get(&name);
            let count: Option<usize> = if let (Some(store), Some(cell)) = (store.as_ref(), cell) {
                let store_inner = store.clone();
                let name_for_log = name.clone();
                *cell.get_or_init(|| async move {
                    match store_inner.count_nodes().await {
                        Ok(n) => Some(n),
                        Err(e) => {
                            tracing::warn!(backend = %name_for_log, error = %e, "count_nodes failed");
                            None
                        }
                    }
                })
                .await
            } else {
                None
            };
            let supports_ppr = store.map(|s| s.supports_native_ppr()).unwrap_or(false);
            let is_primary = name == stores.primary;
            if is_primary {
                primary_count = count;
            }
            destinations_json.push(serde_json::json!({
                "name": name,
                "primary": is_primary,
                "node_count": count,
                "supports_native_ppr": supports_ppr,
            }));
        }
        // Also surface backends that failed to open so the operator
        // can see what's wrong from the UI/curl alone.
        for (name, err) in stores.open_errors.iter() {
            destinations_json.push(serde_json::json!({
                "name": name,
                "primary": false,
                "node_count": null,
                "supports_native_ppr": false,
                "error": err,
            }));
        }
    }

    let has_data = primary_count.map(|n| n > 0).unwrap_or(false);
    let search_ready = db_ready && embedder_ready && has_data;
    let reason = if search_ready {
        None
    } else if !db_ready || !embedder_ready {
        state.db_unavailable_reason()
    } else if !has_data {
        Some("DB is open but contains no nodes (run `ug index` first)".to_string())
    } else {
        None
    };

    let primary_name = active_stores
        .as_ref()
        .map(|s| s.primary.clone())
        .unwrap_or_default();

    let chat_default = state
        .chat_default
        .read()
        .expect("chat_default poisoned")
        .clone();
    let chat_ready = chat_default.is_some() && search_ready;
    let chat_info = chat_default.map(|c| {
        serde_json::json!({
            "model": c.model,
            "base_url": c.base_url,
        })
    });

    // How this project's graph reaches the browser. The page reads `mode` to
    // decide between downloading `graph.json` and asking the server, so this
    // block is what switches the two. Its *absence* means local — which is
    // exactly what a static host answers, and why the published demo needs no
    // shim change to keep working.
    let snap = state.snapshot();
    let graph_bytes = snap.graph_bytes;
    let graph_info = serde_json::json!({
        "mode": state.graph_mode.resolve(graph_bytes, server_mode_bytes()),
        "bytes": graph_bytes,
        "nodes": snap.parsed.nodes.len(),
        "edges": snap.parsed.edges.len(),
        "threshold": server_mode_bytes(),
        // Which snapshot the page is talking to. Server mode splits one graph
        // across many requests, so a `ug gen` landing mid-session would mix a
        // slim index from the old graph with edges from the new one — node
        // indices are positional, so that is not a stale answer but a
        // scrambled one. Every server-mode response carries this; a mismatch
        // means "reload", which is a thing the user can act on.
        "token": snap.token(),
    });

    let body = serde_json::json!({
        "db_ready": db_ready,
        "embedder_ready": embedder_ready,
        "graph": graph_info,
        // What the indexer had to leave out. Published because these caps
        // decide what a node's vector can match on at all, and a user who
        // doesn't know them reads a truncated chunk as a search failure.
        // See `ultragraph::limits` and docs/INDEXING-AND-CHUNKING.md.
        "limits": indexing_limits(&state),
        "search_ready": search_ready,
        "chat_ready": chat_ready,
        "chat": chat_info,
        // Resolved visualization prefs for the page's own rendering. Null
        // when unset — the page falls back to its built-in defaults (three
        // under THREE_D_MAX_ELEMENTS, cosmos above; solo past SOLO_THRESHOLD).
        // Carried here rather than via a separate /api/config fetch because
        // the page needs them *before* its first render, and capabilities is
        // the fetch it already makes in that window.
        "vis": serde_json::json!({
            "renderer": crate::config::get("vis.renderer"),
            "three_d_max_elements": crate::config::get("vis.three_d_max_elements"),
            "solo_threshold": crate::config::get("vis.solo_threshold"),
            "link_blending": crate::config::get("vis.link_blending"),
            "hover_delay_ms": crate::config::get("vis.hover_delay_ms"),
        }),
        // Back-compat: existing UI reads `db_node_count` for the primary.
        "db_node_count": primary_count,
        "reason": reason,
        // New in multi-dest: full list with per-backend flags. UI shows
        // a selector when `destinations.length > 1`.
        "destinations": destinations_json,
        "primary": primary_name,
        // Multi-project: which project this server is currently
        // answering for, and whether the UI should offer a switcher.
        "project": {
            "name": state.active().name,
            "mode": match state.registry.mode {
                ServeMode::Single => "single",
                ServeMode::Multi => "multi",
            },
        },
    });
    ok_json(body.to_string())
}
