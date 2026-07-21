//! napi bridge for the graph-backed agent tools.
//!
//! The MCP server (`node/cli.mjs`) used to reimplement these tools in
//! JavaScript. It now calls straight through to [`crate::agent_tools`], so
//! MCP, the CLI and the HTTP API all execute the same code and produce the
//! same results.

use napi_derive::napi;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use crate::agent_tools::{self as tools, Render};
use crate::types::GraphData;

/// A parsed graph.json plus the raw text (`shortest_path` re-parses it via
/// `find_shortest_path`).
struct CachedGraph {
    parsed: Arc<GraphData>,
    raw: Arc<String>,
    mtime: Option<SystemTime>,
}

/// Parsing a 1 MB graph.json per tool call would dominate the cost of the
/// cheap lookups, and an MCP server makes many of them. Cache by path and
/// invalidate on mtime, so a `reindex` is picked up without a restart.
fn graph_cache() -> &'static Mutex<HashMap<String, CachedGraph>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedGraph>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn load_graph(graph_path: &str) -> napi::Result<(Arc<GraphData>, Arc<String>)> {
    let path = Path::new(graph_path);
    let current = mtime_of(path);

    if let Ok(cache) = graph_cache().lock() {
        if let Some(hit) = cache.get(graph_path) {
            // `None` mtime (stat failed) never satisfies this, so a broken
            // stat degrades to re-reading rather than serving a stale graph.
            if hit.mtime.is_some() && hit.mtime == current {
                return Ok((hit.parsed.clone(), hit.raw.clone()));
            }
        }
    }

    let raw = std::fs::read_to_string(path).map_err(|e| {
        napi::Error::from_reason(format!(
            "graph.json not found at {} ({}) — run `ug gen` for this project first.",
            graph_path, e
        ))
    })?;
    let parsed: GraphData = serde_json::from_str(&raw)
        .map_err(|e| napi::Error::from_reason(format!("invalid graph.json: {}", e)))?;

    let parsed = Arc::new(parsed);
    let raw = Arc::new(raw);
    if let Ok(mut cache) = graph_cache().lock() {
        cache.insert(
            graph_path.to_string(),
            CachedGraph {
                parsed: parsed.clone(),
                raw: raw.clone(),
                mtime: current,
            },
        );
    }
    Ok((parsed, raw))
}

fn parse_params(json: Option<String>) -> napi::Result<serde_json::Value> {
    match json {
        None => Ok(serde_json::json!({})),
        Some(s) if s.trim().is_empty() => Ok(serde_json::json!({})),
        Some(s) => serde_json::from_str(&s)
            .map_err(|e| napi::Error::from_reason(format!("invalid params: {}", e))),
    }
}

/// Run one graph-backed agent tool.
///
/// `tool` is the canonical name (`find_symbol`, `file_outline`, `get_code`,
/// `find_usages`, `project_overview`, `graph_schema`, `shortest_path`).
/// `params_json` uses the canonical snake_case vocabulary; the legacy MCP
/// camelCase spellings are accepted as aliases.
///
/// `render` selects the output: `"markdown"` (default) and `"ansi"` return
/// formatted text, `"json"` returns the serialized result envelope.
#[napi]
pub fn agent_tool(
    tool: String,
    graph_path: String,
    repo_root: String,
    params_json: Option<String>,
    render: Option<String>,
) -> napi::Result<String> {
    let (graph, raw) = load_graph(&graph_path)?;
    let graph = graph.as_ref();
    let repo_root = Path::new(&repo_root);
    let graph_path_ref = Path::new(&graph_path);

    let render = render.unwrap_or_else(|| "markdown".into());
    let style = if render.eq_ignore_ascii_case("json") {
        None
    } else if render.eq_ignore_ascii_case("ansi") {
        Some(Render::Ansi)
    } else {
        Some(Render::Markdown)
    };

    let output = tools::run_tool(
        &tool,
        graph,
        &raw,
        repo_root,
        graph_path_ref,
        parse_params(params_json)?,
        style,
    )
    .map_err(napi::Error::from_reason)?;

    match output {
        tools::ToolOutput::Text(t) => Ok(t),
        tools::ToolOutput::Json(v) => serde_json::to_string_pretty(&v)
            .map_err(|e| napi::Error::from_reason(format!("serialize result: {}", e))),
    }
}

/// Drop the cached parse for one graph.json (or every graph when `None`).
/// `reindex` calls this so the next tool call sees the fresh graph even if
/// the filesystem mtime resolution would have hidden the change.
#[napi]
pub fn agent_tool_invalidate(graph_path: Option<String>) {
    if let Ok(mut cache) = graph_cache().lock() {
        match graph_path {
            Some(p) => {
                cache.remove(&p);
            }
            None => cache.clear(),
        }
    }
}
