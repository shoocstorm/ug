//! Native MCP server for UltraGraph — a stdio JSON-RPC server exposing the
//! GraphRAG knowledge-base tools, plus `install`/`uninstall`/`call`/`list`
//! subcommands. This replaces the old Node.js `cli.mjs` MCP server and its
//! NAPI bridge: every tool now runs the same Rust code the CLI and HTTP API
//! use.
//!
//! Transport: newline-delimited JSON-RPC 2.0 over stdin/stdout (the MCP stdio
//! framing — one JSON message per line, no `Content-Length` headers).
//! Diagnostics go to stderr; stdout is reserved for protocol frames.
//!
//! Configuration (env vars, mirroring the old server):
//!   UG_PROJECT / UG_HOME / UG_REPO_ROOT          — project + repo resolution
//!   UG_EMBED_BASE_URL / UG_EMBED_API_KEY / UG_EMBED_MODEL — embedder (falls
//!       back to persisted `ug config` values, then defaults)
//!   UG_DEST / UG_NEO4J_URI / UG_NEO4J_USER / UG_NEO4J_PASSWORD /
//!       UG_NEO4J_DATABASE                        — destination backend

pub(crate) mod format;
mod install;
pub mod tools;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use ultragraph::agent_tools::{run_tool, Render};
use ultragraph::storage::{
    ingest_graph, open_store, search_kb, semantic_search, semantic_search_w_where, Direction,
    Embedder, EmbedderConfig, RankStrategy, SearchKbOptions, StoreSpec,
};
use ultragraph::types::{GraphData, GraphNodeType};
use ultragraph::{build_graph, index_with_cache, C_BOLD, C_CYAN, C_RESET, C_YELLOW};

use crate::project;

const SERVER_NAME: &str = "ultragraph";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

/// `ug mcp [...]` entry point, replacing the old node-spawning `run_mcp`.
pub fn run(args: &[String]) {
    // `ug mcp` with no subcommand *is* the server — it's what an editor
    // launches over stdio. So only an explicit -h prints help; otherwise a
    // stray flag would leave the client waiting for a handshake.
    if args.first().map(String::as_str) == Some("-h")
        || args.first().map(String::as_str) == Some("--help")
        || (args.is_empty() && std::io::stdin().is_terminal())
    {
        print_mcp_help();
        return;
    }
    match args.first().map(String::as_str) {
        Some("install") => install::run_mcp_install(&args[1..]),
        Some("uninstall") => install::run_mcp_uninstall(&args[1..]),
        Some("call") | Some("c") => run_call(&args[1..]),
        Some("list") | Some("ls") => run_list_tools(),
        _ => run_server(),
    }
}

/// `ug mcp -h`. Also what a bare `ug mcp` prints from a terminal — the
/// server speaks JSON-RPC over stdin, so a human who lands here by hand
/// wants the map, not a silent process.
fn print_mcp_help() {
    println!("  {C_CYAN}ug mcp{C_RESET}  {C_YELLOW}— serve this project's graph to AI coding agents{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug mcp [<subcommand>]");
    println!();
    println!("  Run bare (with stdin piped) it {C_BOLD}is{C_RESET} the MCP server: JSON-RPC over stdio,");
    println!("  exposing this project's knowledge graph as tools an agent can call");
    println!("  ({C_CYAN}search{C_RESET}, {C_CYAN}find_symbols{C_RESET}, {C_CYAN}find_usages{C_RESET}, {C_CYAN}get_code{C_RESET}, …). Editors launch it for you;");
    println!("  you rarely type it yourself.");
    println!();
    println!("{C_BOLD}Subcommands:{C_RESET}");
    println!("  {C_CYAN}install{C_RESET} <agent>     Register the server in an agent's config, and drop in");
    println!("                      the tool guide. Agents: claude, claude-desk, cursor,");
    println!("                      windsurf, vscode, gemini, codex, opencode, zed, …");
    println!("                      Scope: {C_CYAN}--project{C_RESET} (this repo) or {C_CYAN}--global{C_RESET} (everywhere).");
    println!("  {C_CYAN}uninstall{C_RESET} <agent>   Remove it again, same scope flags.");
    println!("  {C_CYAN}list{C_RESET}, {C_CYAN}ls{C_RESET}            Print the tools this server advertises.");
    println!("  {C_CYAN}call{C_RESET} <tool> [json]  Invoke one tool directly — the fastest way to see what");
    println!("                      an agent would get back.");
    println!();
    println!("{C_BOLD}Which project does it serve?{C_RESET}");
    println!("  {C_CYAN}UG_PROJECT{C_RESET} (baked into the config by {C_CYAN}install{C_RESET}) → the ~/.ug project matching");
    println!("  the cwd → the active project ({C_CYAN}ug active{C_RESET}) → a local ./ugdb.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug mcp{C_RESET} install claude --global");
    println!("  {C_CYAN}ug mcp{C_RESET} list");
    println!("  {C_CYAN}ug mcp{C_RESET} call find_symbols '{{\"name\":\"normalize_path\"}}'");
    println!("  {C_CYAN}ug mcp{C_RESET} uninstall cursor --project");
}

// ── project resolution ─────────────────────────────────────────────────────

#[derive(Clone)]
struct ProjectCtx {
    db_path: PathBuf,
    repo_root: PathBuf,
    graph_path: PathBuf,
}

fn graph_path_for(db_path: &Path) -> PathBuf {
    // graph.json sits next to the project's ugdb dir.
    db_path
        .parent()
        .map(|p| p.join("graph.json"))
        .unwrap_or_else(|| PathBuf::from("graph.json"))
}

fn ctx_from_dir(dir: &Path) -> ProjectCtx {
    let db_path = dir.join("ugdb");
    let repo_root = std::env::var("UG_REPO_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            project::read_meta(dir)
                .map(|m| m.repo_root)
                .filter(|r| !r.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let graph_path = graph_path_for(&db_path);
    ProjectCtx {
        db_path,
        repo_root,
        graph_path,
    }
}

/// Startup resolution, most explicit first: `UG_PROJECT` → the persisted
/// active project (`ug active`) → the `~/.ug` project matching the cwd →
/// legacy `./ugdb`.
fn default_ctx() -> ProjectCtx {
    if let Ok(name) = std::env::var("UG_PROJECT") {
        if !name.is_empty() {
            return ctx_from_dir(&project::project_dir(&name));
        }
    }
    if let Some(active) = project::get_active_project() {
        return ctx_from_dir(&project::project_dir(&active));
    }
    let derived = project::project_dir(&project::derive_project_name("."));
    if derived.join("ugdb").exists() {
        return ctx_from_dir(&derived);
    }
    let repo_root = std::env::var("UG_REPO_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let db_path = PathBuf::from("./ugdb");
    let graph_path = graph_path_for(&db_path);
    ProjectCtx {
        db_path,
        repo_root,
        graph_path,
    }
}

// ── cached graph ───────────────────────────────────────────────────────────

struct CachedGraph {
    parsed: Arc<GraphData>,
    raw: Arc<String>,
    mtime: Option<SystemTime>,
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

// ── embedder ───────────────────────────────────────────────────────────────

/// Mirror of `embedder_from_args` (main.rs) but resolving purely from env +
/// persisted config (no CLI flags) and returning a `Result` so a bad
/// embedding endpoint fails one tool call instead of killing the server.
fn build_embedder() -> Result<Embedder, String> {
    let (dim_raw, _) = crate::config::resolve_pref_cfg(None, "embed.dim");
    let dim = dim_raw.and_then(|s| s.parse().ok());
    let (base_url, _) = crate::config::resolve_pref_cfg(None, "embed.base_url");
    let want_remote = base_url.is_some();
    let (api_key, _) = crate::config::resolve_pref_cfg(None, "embed.api_key");
    let (model, _) = crate::config::resolve_pref_cfg(None, "embed.model");
    let cfg = EmbedderConfig::with_overrides(base_url, api_key, model, dim, None, None);
    if want_remote {
        Embedder::remote(cfg).map_err(|e| e.to_string())
    } else {
        Embedder::local(cfg).map_err(|e| e.to_string())
    }
}

/// Single store spec from env (`UG_DEST` and friends). Unlike `ug serve`, MCP
/// targets exactly one backend.
fn store_spec(db_path: &Path, dim: u32) -> Result<StoreSpec, String> {
    let dest = std::env::var("UG_DEST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "overgraph".to_string())
        .to_lowercase();
    match dest.as_str() {
        "overgraph" | "og" => Ok(StoreSpec::Overgraph {
            path: db_path.to_path_buf(),
            embedding_dim: dim,
        }),
        "neo4j" | "neo" => {
            let uri = std::env::var("UG_NEO4J_URI")
                .map_err(|_| "UG_DEST=neo4j requires UG_NEO4J_URI".to_string())?;
            let password = std::env::var("UG_NEO4J_PASSWORD")
                .map_err(|_| "UG_DEST=neo4j requires UG_NEO4J_PASSWORD".to_string())?;
            Ok(StoreSpec::Neo4j {
                uri,
                user: std::env::var("UG_NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string()),
                password,
                database: std::env::var("UG_NEO4J_DATABASE").ok(),
                embedding_dim: dim,
            })
        }
        other => Err(format!(
            "Unknown UG_DEST value: {} (expected: overgraph, neo4j)",
            other
        )),
    }
}

// ── server state ───────────────────────────────────────────────────────────

struct Mcp {
    default_ctx: ProjectCtx,
    project_cache: Mutex<HashMap<String, ProjectCtx>>,
    graph_cache: Mutex<HashMap<PathBuf, CachedGraph>>,
    /// Built lazily on the first DB-backed call, then reused. `None` until
    /// then; a failed build isn't cached so a transient outage can recover.
    embedder: Mutex<Option<Arc<Embedder>>>,
}

impl Mcp {
    fn new() -> Self {
        Mcp {
            default_ctx: default_ctx(),
            project_cache: Mutex::new(HashMap::new()),
            graph_cache: Mutex::new(HashMap::new()),
            embedder: Mutex::new(None),
        }
    }

    /// Resolve the ctx for an optional `project` arg. Empty/absent → the
    /// server's startup ctx.
    fn resolve_ctx(&self, project: Option<&str>) -> Result<ProjectCtx, String> {
        let key = project.unwrap_or("");
        if key.is_empty() {
            return Ok(self.default_ctx.clone());
        }
        if let Some(hit) = self.project_cache.lock().unwrap().get(key) {
            return Ok(hit.clone());
        }
        let dir = project::project_dir(key);
        if !dir.join("ugdb").exists() && !dir.join("graph.json").exists() {
            return Err(format!(
                "No indexed project '{}' under {} — call list_projects to see what exists.",
                key,
                project::ug_home().display()
            ));
        }
        let ctx = ctx_from_dir(&dir);
        self.project_cache
            .lock()
            .unwrap()
            .insert(key.to_string(), ctx.clone());
        Ok(ctx)
    }

    fn embedder(&self) -> Result<Arc<Embedder>, String> {
        let mut slot = self.embedder.lock().unwrap();
        if let Some(e) = slot.as_ref() {
            return Ok(e.clone());
        }
        let e = Arc::new(build_embedder()?);
        *slot = Some(e.clone());
        Ok(e)
    }

    /// Parse graph.json (cached by path, invalidated on mtime change).
    fn load_graph(&self, graph_path: &Path) -> Result<(Arc<GraphData>, Arc<String>), String> {
        let current = mtime_of(graph_path);
        if let Some(hit) = self.graph_cache.lock().unwrap().get(graph_path) {
            if hit.mtime.is_some() && hit.mtime == current {
                return Ok((hit.parsed.clone(), hit.raw.clone()));
            }
        }
        let raw = std::fs::read_to_string(graph_path).map_err(|e| {
            format!(
                "graph.json not found at {} ({}) — run `ug gen` for this project first.",
                graph_path.display(),
                e
            )
        })?;
        let parsed: GraphData =
            serde_json::from_str(&raw).map_err(|e| format!("invalid graph.json: {}", e))?;
        let parsed = Arc::new(parsed);
        let raw = Arc::new(raw);
        self.graph_cache.lock().unwrap().insert(
            graph_path.to_path_buf(),
            CachedGraph {
                parsed: parsed.clone(),
                raw: raw.clone(),
                mtime: current,
            },
        );
        Ok((parsed, raw))
    }

    fn invalidate(&self, graph_path: &Path) {
        self.graph_cache.lock().unwrap().remove(graph_path);
    }

    /// Index-freshness note appended to tool outputs: graph.json mtime vs the
    /// current mtimes of the files it indexed. Empty string when fresh (or no
    /// graph yet — the tool that needed it raises its own error).
    fn staleness_note(&self, ctx: &ProjectCtx) -> String {
        let Ok((graph, _)) = self.load_graph(&ctx.graph_path) else {
            return String::new();
        };
        let Some(built_at) = mtime_of(&ctx.graph_path) else {
            return String::new();
        };
        let mut files: Vec<&str> = Vec::new();
        for n in &graph.nodes {
            if matches!(n.node_type, GraphNodeType::Folder) {
                continue;
            }
            if let Some(f) = n.file.as_deref() {
                if !f.is_empty() && !files.contains(&f) {
                    files.push(f);
                }
            }
        }
        let (mut changed, mut missing) = (0usize, 0usize);
        for f in &files {
            match mtime_of(&ctx.repo_root.join(f)) {
                Some(mt) if mt > built_at => changed += 1,
                Some(_) => {}
                None => missing += 1,
            }
        }
        if changed == 0 && missing == 0 {
            return String::new();
        }
        let mut bits = Vec::new();
        if changed > 0 {
            bits.push(format!("{} changed", changed));
        }
        if missing > 0 {
            bits.push(format!("{} deleted", missing));
        }
        let age = built_at
            .elapsed()
            .ok()
            .map(|d| d.as_secs() / 86400)
            .filter(|days| *days > 0)
            .map(|days| format!(" (index built {} day(s) ago)", days))
            .unwrap_or_default();
        format!(
            "\n\n⚠ Index may be stale: {} of {} indexed files since the last index{}. Call the reindex tool to refresh.",
            bits.join(", "),
            files.len(),
            age
        )
    }

    // ── dispatch ───────────────────────────────────────────────────────────

    async fn call_tool(&self, raw_name: &str, raw_args: &Value) -> Result<String, String> {
        let name = tools::canonical_tool_name(raw_name);
        let project = raw_args.get("project").and_then(|v| v.as_str());
        let ctx = self.resolve_ctx(project)?;

        // Merge alias defaults under the caller's args, then drop `project`.
        let mut args = match tools::alias_defaults(raw_name) {
            Some(Value::Object(defaults)) => {
                let mut m = defaults;
                if let Some(obj) = raw_args.as_object() {
                    for (k, v) in obj {
                        m.insert(k.clone(), v.clone());
                    }
                }
                Value::Object(m)
            }
            _ => raw_args.clone(),
        };
        if let Some(obj) = args.as_object_mut() {
            obj.remove("project");
        }
        // Same coercion the chat path applies — MCP clients stringify
        // array arguments just as readily as chat models do.
        tools::normalize_args(name, &mut args);

        let with_staleness = |text: String| -> String { text + &self.staleness_note(&ctx) };

        match name {
            "search" => Ok(with_staleness(self.tool_search(&ctx, &args).await?)),
            "semantic_search" => {
                Ok(with_staleness(self.tool_semantic_search(&ctx, &args).await?))
            }
            // get_code is split out because it is the one graph tool that
            // benefits from the store: serving a node's indexed source
            // keeps the code an agent reads consistent with the
            // description and embedding it searched on, and lets a stale
            // file be reported instead of silently mis-sliced.
            "get_code" => Ok(with_staleness(self.tool_get_code(&ctx, args).await?)),
            "find_symbols" | "file_outline" | "find_usages" | "traverse"
            | "shortest_path" | "project_overview" | "graph_schema" => {
                Ok(with_staleness(self.tool_graph(name, &ctx, args)?))
            }
            "list_projects" => {
                let text = self.tool_list_projects(&ctx);
                Ok(match project {
                    Some(_) => format!(
                        "\n⚠ list_projects ignores the project parameter — listing all projects under {}.\n\n{}",
                        project::ug_home().display(),
                        text
                    ),
                    None => text,
                })
            }
            "reindex" => self.tool_reindex(&ctx).await,
            "ping_embedder" => {
                self.embedder()?.ping().await.map_err(|e| e.to_string())?;
                Ok("ok".to_string())
            }
            other => Err(format!("Unknown tool: {}", other)),
        }
    }

    /// `get_code`, served from the index where possible.
    ///
    /// Falls back to the plain graph path whenever the store can't be
    /// opened or has nothing for these ids — reading source must keep
    /// working even with no database, which is what the CLI does.
    async fn tool_get_code(&self, ctx: &ProjectCtx, args: Value) -> Result<String, String> {
        let params: ultragraph::agent_tools::GetCodeParams =
            serde_json::from_value(args.clone())
                .map_err(|e| format!("invalid get_code params: {}", e))?;

        let mut stored: HashMap<String, ultragraph::agent_tools::StoredSource> = HashMap::new();
        if !params.node_id.is_empty() {
            if let Ok(dim) = self.embedder().map(|e| e.config().dim as u32) {
                if let Ok(spec) = store_spec(&ctx.db_path, dim) {
                    if let Ok(store) = open_store(&spec).await {
                        if let Ok(rows) = store.nodes_by_ids(&params.node_id).await {
                            for r in rows {
                                if !r.code.is_empty() {
                                    stored.insert(
                                        r.id.clone(),
                                        ultragraph::agent_tools::StoredSource {
                                            code: r.code,
                                            file_hash: r.file_hash,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        let (graph, _raw) = self.load_graph(&ctx.graph_path)?;
        let result = ultragraph::agent_tools::get_code_with_stored(
            graph.as_ref(),
            &ctx.repo_root,
            &params,
            &stored,
        );
        Ok(ultragraph::agent_tools::render_get_code(
            &result,
            Render::Markdown,
        ))
    }

    fn tool_graph(&self, name: &str, ctx: &ProjectCtx, args: Value) -> Result<String, String> {
        let (graph, raw) = self.load_graph(&ctx.graph_path)?;
        let output = run_tool(
            name,
            graph.as_ref(),
            &raw,
            &ctx.repo_root,
            &ctx.graph_path,
            args,
            Some(Render::Markdown),
        )?;
        Ok(match output {
            ultragraph::agent_tools::ToolOutput::Text(t) => t,
            ultragraph::agent_tools::ToolOutput::Json(v) => {
                serde_json::to_string_pretty(&v).unwrap_or_default()
            }
        })
    }

    async fn tool_search(&self, ctx: &ProjectCtx, args: &Value) -> Result<String, String> {
        let a: SearchArgs = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid search params: {}", e))?;
        if a.query.trim().is_empty() {
            return Err("search requires a non-empty query.".to_string());
        }
        let embedder = self.embedder()?;
        let dim = embedder.config().dim as u32;
        let spec = store_spec(&ctx.db_path, dim)?;
        let store = open_store(&spec)
            .await
            .map_err(|e| format!("failed to open {} store: {}", spec.name(), e))?;

        let edge_types = a.edge_types.clone();
        let where_clause = a.where_clause.clone();
        let mut opts = SearchKbOptions::new(&a.query, ctx.repo_root.as_path());
        if let Some(k) = a.k {
            opts.k = k;
        }
        if let Some(h) = a.hops {
            opts.hops = h;
        }
        opts.edge_types = edge_types.as_deref();
        opts.direction = a
            .direction
            .as_deref()
            .map(Direction::from_str_lossy)
            .unwrap_or(Direction::Both);
        if let Some(c) = a.max_chars {
            opts.max_chars = c;
        }
        if let Some(l) = a.mmr_lambda {
            opts.mmr_lambda = l;
        }
        opts.where_clause = where_clause.as_deref();
        if let Some(s) = a.include_snippets {
            opts.include_snippets = s;
        }
        if let Some(s) = a.strategy.as_deref() {
            opts.strategy = RankStrategy::from_str_lossy(s);
        }
        if let Some(p) = a.ppr_restart_prob {
            opts.ppr_restart_prob = p;
        }
        if let Some(m) = a.ppr_max_iter {
            opts.ppr_max_iter = m;
        }
        if let Some(p) = a.ppr_seed_pool {
            opts.ppr_seed_pool = p;
        }
        opts.ppr_edge_weights = a.ppr_edge_weights.clone();

        let result = search_kb(store.as_ref(), embedder.as_ref(), opts)
            .await
            .map_err(|e| format!("search_kb failed: {}", e))?;
        Ok(format::format_ranked_context(&result))
    }

    async fn tool_semantic_search(
        &self,
        ctx: &ProjectCtx,
        args: &Value,
    ) -> Result<String, String> {
        let a: SemanticArgs = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid semantic_search params: {}", e))?;
        if a.query.trim().is_empty() {
            return Err("semantic_search requires a non-empty query.".to_string());
        }
        let k = a.k.unwrap_or(10);
        let embedder = self.embedder()?;
        let dim = embedder.config().dim as u32;
        let spec = store_spec(&ctx.db_path, dim)?;
        let store = open_store(&spec)
            .await
            .map_err(|e| format!("failed to open {} store: {}", spec.name(), e))?;
        let hits = match a.where_clause.as_deref() {
            Some(w) => semantic_search_w_where(store.as_ref(), embedder.as_ref(), &a.query, k, w)
                .await
                .map_err(|e| format!("search failed: {}", e))?,
            None => semantic_search(store.as_ref(), embedder.as_ref(), &a.query, k)
                .await
                .map_err(|e| format!("search failed: {}", e))?,
        };
        Ok(format::format_semantic_hits(&a.query, &hits))
    }

    fn tool_list_projects(&self, ctx: &ProjectCtx) -> String {
        let mut infos = Vec::new();
        for (dir, meta) in project::list_projects() {
            if !dir.join("ugdb").exists() && !dir.join("graph.json").exists() {
                continue;
            }
            infos.push(format::ProjectInfo {
                name: meta.name,
                repo_root: if meta.repo_root.is_empty() {
                    "(unknown)".to_string()
                } else {
                    meta.repo_root
                },
                nodes: Some(meta.nodes),
                edges: Some(meta.edges),
            });
        }
        format::format_project_list(
            &infos,
            &ctx.repo_root.to_string_lossy(),
            &project::ug_home().display().to_string(),
        )
    }

    /// Quiet re-run of the gen pipeline (index → graph → ingest). Ingest
    /// failure (embedder down) is reported but doesn't fail the call: the
    /// graph-backed tools are already fresh at that point.
    async fn tool_reindex(&self, ctx: &ProjectCtx) -> Result<String, String> {
        if !ctx.repo_root.exists() {
            return Err(format!(
                "Repo root {} no longer exists — re-run `ug gen -i <path>` manually.",
                ctx.repo_root.display()
            ));
        }
        let output_dir = ctx
            .graph_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| format!("failed to create {}: {}", output_dir.display(), e))?;

        let index_json = index_with_cache(
            ctx.repo_root.to_string_lossy().into_owned(),
            output_dir.to_string_lossy().into_owned(),
        );
        let graph_str = build_graph(index_json.clone());
        std::fs::write(&ctx.graph_path, &graph_str)
            .map_err(|e| format!("failed to write graph.json: {}", e))?;
        std::fs::write(output_dir.join("indexed-tree.json"), &index_json)
            .map_err(|e| format!("failed to write indexed-tree.json: {}", e))?;

        let graph: GraphData =
            serde_json::from_str(&graph_str).map_err(|e| format!("invalid graph: {}", e))?;
        let (nodes, edges) = (graph.nodes.len(), graph.edges.len());
        let name = project::read_meta(&output_dir)
            .map(|m| m.name)
            .unwrap_or_else(|| {
                output_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("default")
                    .to_string()
            });
        let _ = project::write_meta(
            &output_dir,
            &project::ProjectMeta::new(&name, &ctx.repo_root.to_string_lossy(), nodes, edges),
        );

        let ingest_msg = match self.reindex_ingest(ctx, &graph).await {
            Ok(m) => m,
            Err(e) => format!(
                "db ingest FAILED ({}) — graph tools (find_symbols/get_code/...) are fresh, but search serves the previous embeddings until the embedder is reachable",
                e
            ),
        };
        self.invalidate(&ctx.graph_path);
        Ok(format!(
            "Reindexed {} → {}\n{} nodes, {} edges\n{}",
            ctx.repo_root.display(),
            output_dir.display(),
            nodes,
            edges,
            ingest_msg
        ))
    }

    async fn reindex_ingest(&self, ctx: &ProjectCtx, graph: &GraphData) -> Result<String, String> {
        let mut embedder = build_embedder()?;
        // Probe the endpoint's dim so models can be swapped without knowing it
        // ahead of time (mirrors the old db_ingest path).
        if let Ok(probed) = embedder.probe_dim().await {
            if probed != embedder.config().dim {
                embedder.set_dim(probed);
            }
        }
        let dim = embedder.config().dim as u32;
        let spec = store_spec(&ctx.db_path, dim)?;
        let store = open_store(&spec)
            .await
            .map_err(|e| format!("failed to open {} store: {}", spec.name(), e))?;
        let stats = ingest_graph(store.as_ref(), &embedder, graph)
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "db ingest: {} nodes, {} edges embedded",
            stats.nodes_written, stats.edges_written
        ))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchArgs {
    #[serde(default)]
    query: String,
    k: Option<usize>,
    hops: Option<u32>,
    edge_types: Option<Vec<String>>,
    direction: Option<String>,
    max_chars: Option<usize>,
    mmr_lambda: Option<f32>,
    where_clause: Option<String>,
    include_snippets: Option<bool>,
    strategy: Option<String>,
    ppr_restart_prob: Option<f32>,
    ppr_max_iter: Option<usize>,
    ppr_seed_pool: Option<usize>,
    ppr_edge_weights: Option<HashMap<String, f32>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SemanticArgs {
    #[serde(default)]
    query: String,
    k: Option<usize>,
    where_clause: Option<String>,
}

// ── stdio JSON-RPC server ──────────────────────────────────────────────────

fn run_server() {
    let rt = crate::tokio_runtime();
    rt.block_on(async {
        let mcp = Mcp::new();
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let mut stdout = tokio::io::stdout();

        if std::io::stdin().is_terminal() {
            eprintln!("UltraGraph MCP server (stdio mode)");
            eprintln!("This command is meant to be launched by an AI agent, not run by hand:");
            eprintln!("agents spawn it themselves and speak JSON-RPC over stdin/stdout.");
            eprintln!("To wire an agent up to it, run {C_CYAN}ug mcp install{C_RESET} instead.");
            eprintln!("(Waiting for JSON-RPC on stdin — Ctrl+C to exit.)");
        } else {
            eprintln!("start ultragraph mcp server...");
        }

        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    // Can't recover an id — emit a parse error with null id.
                    let resp = rpc_error(Value::Null, -32700, &format!("parse error: {}", e));
                    write_message(&mut stdout, &resp).await;
                    continue;
                }
            };
            if let Some(resp) = handle_message(&mcp, &msg).await {
                write_message(&mut stdout, &resp).await;
            }
        }
        eprintln!("ultragraph mcp server stopped.");
    });
}

async fn write_message(stdout: &mut tokio::io::Stdout, msg: &Value) {
    let mut s = serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    let _ = stdout.write_all(s.as_bytes()).await;
    let _ = stdout.flush().await;
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Dispatch one JSON-RPC message. Returns `None` for notifications (no `id`).
async fn handle_message(mcp: &Mcp, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    // Notifications (no id) never get a response.
    let Some(id) = id else {
        return None;
    };

    match method {
        "initialize" => {
            let protocol = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            Some(rpc_result(
                id,
                json!({
                    "protocolVersion": protocol,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                }),
            ))
        }
        "ping" => Some(rpc_result(id, json!({}))),
        "tools/list" => Some(rpc_result(id, json!({ "tools": tools::tool_list() }))),
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let result = mcp.call_tool(name, &args).await;
            Some(rpc_result(id, tool_call_result(result)))
        }
        other => Some(rpc_error(
            id,
            -32601,
            &format!("Method not found: {}", other),
        )),
    }
}

fn tool_call_result(result: Result<String, String>) -> Value {
    match result {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(e) => json!({
            "isError": true,
            "content": [{ "type": "text", "text": format!("Error: {}", e) }],
        }),
    }
}

// ── one-shot `ug mcp call` / `ug mcp list` ─────────────────────────────────

fn run_call(args: &[String]) {
    let tool = match args.first() {
        Some(t) => t,
        None => {
            eprintln!("Usage: mcp call <tool> <json>");
            std::process::exit(1);
        }
    };
    let json_str = args.get(1).map(String::as_str).unwrap_or("{}");
    let parsed: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Invalid JSON: {}", json_str);
            std::process::exit(1);
        }
    };
    let canonical = tools::canonical_tool_name(tool);
    if !tools::is_known_tool(canonical) {
        eprintln!(
            "Unknown tool '{}' — see `ug mcp list` for available tools.",
            tool
        );
        std::process::exit(1);
    }
    let rt = crate::tokio_runtime();
    let mcp = Mcp::new();
    match rt.block_on(mcp.call_tool(tool, &parsed)) {
        Ok(text) => println!("{}", text),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn run_list_tools() {
    let ctx = default_ctx();
    println!(
        "Available MCP tools (repo: {})\n",
        ctx.repo_root.display()
    );
    let list = tools::tool_list();
    if let Some(arr) = list.as_array() {
        for t in arr {
            let name = t["name"].as_str().unwrap_or("");
            let desc = t["description"].as_str().unwrap_or("");
            let first = desc.split('.').next().unwrap_or(desc);
            println!("{:<18}{}", name, first);
        }
    }
    println!("\nRun `ug mcp call <tool> <json>` to invoke one. Example:");
    println!("  ug mcp call find_symbols '{{\"name\":\"run_mcp\"}}'");
}
