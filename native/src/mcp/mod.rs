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
// `pub` for `ug connect` / `ug disconnect`, which are the promoted spelling of
// `ug mcp install` / `ug mcp uninstall` and call straight into it.
pub mod install;
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
    ingest_graph, open_store, search_kb, Direction, Embedder, EmbedderConfig, KnowledgeStore,
    RankStrategy, SearchKbOptions, StoreSpec,
};
use ultragraph::types::{GraphData, GraphNodeType};
use ultragraph::{build_graph, index_with_cache, C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW};

use crate::project;

const SERVER_NAME: &str = "ultragraph";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Returned from `initialize`, which MCP clients surface to the model once
/// per session.
///
/// It carries the two things no single tool description can: that these tools
/// answer *families* of questions in one call, and that the parameters are
/// more forgiving than they look (a name where an id is documented, a
/// wildcard where a name is). A model that has not been told will call
/// `find_symbols` in a loop, one symbol at a time, and never try `handle_*` —
/// the capability exists but goes unused. Kept short: it competes with the
/// user's own prompt for attention.
const SERVER_INSTRUCTIONS: &str = "\
These tools answer questions about ONE indexed codebase by querying its graph. \
Prefer them over grep/file reads for anything relational (who calls X, what \
breaks if I change X, how are A and B connected) or aggregate (how many, which \
are biggest, what fraction).

USE THEM WHILE EDITING, NOT ONLY WHILE READING. Before changing a symbol, ask \
who depends on it: find_usages, and analyze {preset: 'boundary_impact'} for \
whether the change escapes the system. After an edit burst, ask what it reached \
and what covers it: analyze {preset: 'diff_impact', args: {files: 'a.ts,b.rs'}} \
and {preset: 'diff_retest_scope'}. Grep cannot answer either question. The graph \
is kept current by git hooks (`ug hook install`), and you can refresh it yourself \
at any point with `ug update <file>...` — do that after editing and before asking \
a structural question about those files. get_code always reads the live working \
tree, so source is never silently stale; the structural tools say so in their \
output when the index has fallen behind.

Two habits make them cheap:

1. WILDCARDS — every parameter that names a symbol or file accepts a shell-style \
pattern: * (any run of chars), ? (one char), [abc]/[a-z], [!ab], {a,b}. Patterns \
match the WHOLE name (use *auth* to match anywhere); in paths * stops at / and **/ \
crosses directories. So one call covers a whole family: find_symbols {name: \
'handle_*'}, find_usages {nodeId: 'validate_*'}, file_outline {file: 'src/**/*.ts'}, \
find_symbols {name: '*', filePrefix: 'src/auth/**'}. Reach for this whenever you \
would otherwise loop.

2. NAMES WORK WHERE IDS ARE DOCUMENTED — the nodeId of get_code, find_usages, \
traverse and shortest_path also takes a plain symbol name, so find_symbols first \
is optional, not required.

For counts, rankings, distributions and blast radius, call analyze once rather \
than assembling the answer yourself. Every truncated or capped result says so — \
trust the stated totals over what you can see.";

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
    println!("  {C_CYAN}list{C_RESET}, {C_CYAN}ls{C_RESET}            Print the tools this server advertises.");
    println!("  {C_CYAN}call{C_RESET} <tool> [json]  Invoke one tool directly — the fastest way to see what");
    println!("                      an agent would get back.");
    println!("  {C_CYAN}install{C_RESET} <agent>     Older spelling of {C_BOLD}{C_GREEN}ug connect{C_RESET} — still works, and still");
    println!("                      installs either path. {C_CYAN}uninstall{C_RESET} is {C_BOLD}{C_GREEN}ug disconnect{C_RESET}.");
    println!();
    println!("{C_BOLD}Connecting an agent is {C_GREEN}ug connect{C_RESET}{C_BOLD}:{C_RESET}");
    println!("  It offers two ways to reach ug — the {C_CYAN}ug{C_RESET} CLI (via an agent skill, the");
    println!("  recommended path) or this MCP server — and wires up the one you pick.");
    println!("  See {C_CYAN}ug connect -h{C_RESET} for the flags and scopes.");
    println!();
    println!("{C_BOLD}Which project does it serve?{C_RESET}");
    println!("  {C_CYAN}UG_PROJECT{C_RESET} (baked into the config by {C_CYAN}install{C_RESET}) → the ~/.ug project matching");
    println!("  the cwd → the active project ({C_CYAN}ug active{C_RESET}) → a local ./ugdb.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug connect{C_RESET} claude --mcp --global");
    println!("  {C_CYAN}ug mcp{C_RESET} list");
    println!("  {C_CYAN}ug mcp{C_RESET} call find_symbols '{{\"name\":\"normalize_path\"}}'");
    println!("  {C_CYAN}ug mcp{C_RESET} call find_symbols '{{\"name\":\"handle_*\"}}'   {C_YELLOW}# wildcards work in every tool{C_RESET}");
    println!("  {C_CYAN}ug mcp{C_RESET} call find_usages '{{\"nodeId\":\"run_serve\"}}'  {C_YELLOW}# a name works where an id is documented{C_RESET}");
    println!("  {C_CYAN}ug mcp{C_RESET} uninstall cursor --project");
}

// ── project resolution ─────────────────────────────────────────────────────

#[derive(Clone)]
struct ProjectCtx {
    db_path: PathBuf,
    repo_root: PathBuf,
    graph_path: PathBuf,
}

/// Warning appended to the vector-backed tools when the project has nodes
/// that were written without vectors (any run without `--with-embed`, which
/// is the default and what the git hooks do).
///
/// Those nodes are in the graph and in every structural answer, but they are
/// invisible to a vector search — so a semantic result set can be quietly
/// incomplete in exactly the way that looks like "no such code exists".
/// Empty string when nothing is owed.
fn vectors_note(ctx: &ProjectCtx) -> String {
    let Some(dir) = ctx.db_path.parent() else {
        return String::new();
    };
    if crate::project::pending_vectors_age(dir).is_none() {
        return String::new();
    }
    let project = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "\n\n⚠ Some nodes have no vectors yet (embedding is opt-in — a refresh without \
         --with-embed, e.g. from the git hooks), \
         so this ranking may be missing recently-changed code. Structural tools (find_usages, \
         analyze) are unaffected. Run `ug ingest -n {}` to backfill.",
        project
    )
}

/// Resolve `gen`'s optional `files` argument to repo-relative paths.
///
/// Accepts the three spellings an agent reaches for — an absolute path under
/// the repo root, a repo-relative path, or a path relative to the cwd — and
/// mirrors `ug update`'s rule that an unresolvable target is an **error**, not
/// a silent skip. An agent that mistypes a path and is told the refresh
/// happened goes on to trust a blast radius that never saw its edit, which is
/// the exact failure this whole staleness story exists to prevent.
///
/// `Ok(vec![])` when `files` is absent: a whole-repo `gen`, unchanged.
fn gen_targets(ctx: &ProjectCtx, args: &Value) -> Result<Vec<String>, String> {
    let Some(raw) = args.get("files") else {
        return Ok(Vec::new());
    };
    // `normalize_args` already turns a stringified JSON array into an array.
    // A bare comma-separated string is the other spelling agents reach for,
    // because that is exactly how `analyze --arg files=a.ts,b.rs` takes its
    // file list — accepting it here beats an error the caller has to guess
    // its way out of.
    let owned: Vec<Value>;
    let list = match raw {
        Value::Array(a) => a,
        Value::String(s) => {
            owned = s
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| Value::String(p.to_string()))
                .collect();
            &owned
        }
        _ => return Err("gen: `files` must be an array of paths".to_string()),
    };
    // Canonicalise the root once: the recorded path can carry a symlink
    // (`/tmp` → `/private/tmp` on macOS), and comparing an un-canonicalised
    // root against a canonicalised file always reports "outside the repo" — a
    // failure that reads as a security denial and is really a bug.
    let root = std::fs::canonicalize(&ctx.repo_root).unwrap_or_else(|_| ctx.repo_root.clone());
    let mut out = Vec::new();
    for entry in list {
        let Some(p) = entry.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return Err("gen: every entry in `files` must be a non-empty string".to_string());
        };
        let candidate = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            root.join(p)
        };
        // A deleted file cannot be canonicalised, and "I just deleted this"
        // is a legitimate reason to refresh — fall back to the lexical path so
        // the removal still gets reported rather than rejected.
        let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        let rel = resolved.strip_prefix(&root).map_err(|_| {
            format!(
                "gen: {} is outside the indexed repo ({})",
                p,
                root.display()
            )
        })?;
        // `strip_prefix` is lexical, and the fallback path above is
        // un-canonicalised whenever the target does not exist — so
        // `<root>/../outside.rs` strips cleanly to `../outside.rs` and would
        // otherwise be accepted as repo-relative. Canonicalisation resolves
        // `..` for every path that *does* exist; this covers the ones that do
        // not, which is exactly the deleted-file case the fallback is for.
        if rel.components().any(|c| c == std::path::Component::ParentDir) {
            return Err(format!(
                "gen: {} is outside the indexed repo ({})",
                p,
                root.display()
            ));
        }
        out.push(rel.to_string_lossy().replace('\\', "/"));
    }
    Ok(out)
}

/// Per-file confirmation for a `gen` that named `files`: how many symbols the
/// refreshed graph holds for each. Empty when nothing was named.
fn per_file_report(graph: &GraphData, targets: &[String], repo_root: &Path) -> String {
    if targets.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n");
    for rel in targets {
        let symbols = graph
            .nodes
            .iter()
            .filter(|n| n.file.as_deref() == Some(rel.as_str()))
            .count();
        // Counted from the *new* graph, so this reflects what landed rather
        // than what the parse produced.
        let note = if symbols > 0 {
            format!("{} symbol(s)", symbols)
        } else if !repo_root.join(rel).exists() {
            "deleted — dropped from the index".to_string()
        } else {
            "0 symbols — extension not indexed, so this file is invisible to \
             every structural tool"
                .to_string()
        };
        out.push_str(&format!("\n  {}: {}", rel, note));
    }
    out
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
    /// Byte length of the `graph.json` this was parsed from, for cache
    /// accounting only.
    ///
    /// The text itself is deliberately **not** retained. It used to be, as an
    /// `Arc<String>`, for the sole purpose of handing it to
    /// `agent_tools::run_tool` so that `shortest_path` could re-parse it —
    /// which is exactly what P4.1 removed. Keeping it meant every cached
    /// project held a second full copy of its graph, 346 MB of it on the
    /// largest index here, that nothing ever read.
    raw_len: usize,
    mtime: Option<SystemTime>,
}

impl CachedGraph {
    /// Rough resident size: the raw JSON, plus the parsed graph it was
    /// deserialized into, which runs about 3× the text it came from. Same
    /// multiplier `ug serve` uses to size a snapshot (`approx_bytes` there).
    fn approx_bytes(&self) -> usize {
        // Still 4×: the parsed graph runs ~3× the text it came from, and the
        // text was one more. Now that only the parse is retained the true
        // figure is nearer 3×, but over-estimating only makes the cache more
        // conservative, and this multiplier is shared with `ug serve`.
        self.raw_len.saturating_mul(4)
    }
}

/// Byte ceiling for parsed graphs held resident by one MCP server process.
///
/// An MCP server is one agent session rather than a shared service, so it
/// needs far fewer projects resident than `ug serve` does — but 256 MiB still
/// keeps the ordinary case (an agent working across a handful of repos)
/// entirely cached.
fn graph_cache_budget() -> usize {
    const DEFAULT: usize = 256 * 1024 * 1024;
    std::env::var("UG_MCP_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT)
}

/// Parsed `graph.json` bodies keyed by path, under a byte ceiling.
///
/// This was a bare `HashMap` with no eviction, so it grew by one whole graph
/// — parsed *and* raw — for every project an agent touched and never gave any
/// of it back. `ug serve` hit the same thing (its `loaded` map pinned "half a
/// gigabyte across six mid-size repos") and answered it with an LRU over a
/// byte budget; this is that, and it matters more here, because an MCP server
/// lives as long as the session that started it and nothing restarts it
/// mid-way.
struct GraphCache {
    entries: HashMap<PathBuf, CachedGraph>,
    /// Recency order over `entries`, least-recently-used first.
    lru: Vec<PathBuf>,
    budget: usize,
}

impl GraphCache {
    fn new() -> Self {
        GraphCache {
            entries: HashMap::new(),
            lru: Vec::new(),
            budget: graph_cache_budget(),
        }
    }

    fn get(&mut self, path: &Path) -> Option<&CachedGraph> {
        if !self.entries.contains_key(path) {
            return None;
        }
        self.touch(path);
        self.entries.get(path)
    }

    fn insert(&mut self, path: PathBuf, entry: CachedGraph) {
        self.entries.insert(path.clone(), entry);
        self.touch(&path);
        self.evict_over_budget();
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
        self.lru.retain(|p| p != path);
    }

    fn touch(&mut self, path: &Path) {
        self.lru.retain(|p| p != path);
        self.lru.push(path.to_path_buf());
    }

    /// Drop least-recently-used graphs until the cache fits its budget.
    ///
    /// Never evicts the last entry: it is the one just inserted, and a single
    /// graph bigger than the whole budget would otherwise be thrown away
    /// before its caller could read it, making every call re-parse it.
    fn evict_over_budget(&mut self) {
        let mut total: usize = self.entries.values().map(CachedGraph::approx_bytes).sum();
        while total > self.budget && self.lru.len() > 1 {
            let victim = self.lru.remove(0);
            if let Some(evicted) = self.entries.remove(&victim) {
                total = total.saturating_sub(evicted.approx_bytes());
            }
        }
    }
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
/// Read `analyze` tool arguments off the JSON-RPC params.
///
/// `args` values are stringified rather than passed through as JSON:
/// preset parameters are coerced against their declared types in
/// `analyze::bind`, which is the one place that knows whether
/// `min_loc` wants a number. Models send `{"min_loc": 100}` and
/// `{"min_loc": "100"}` about equally often, so both have to arrive here
/// looking the same.
///
/// Infallible: every way this can be wrong — unknown preset, missing
/// required argument, malformed query — is caught in `analyze::run`,
/// which can name the alternatives in its error. There is nothing this
/// layer could reject more helpfully.
fn parse_analyze_args(args: &Value) -> ultragraph::analyze::AnalyzeParams {
    let str_field = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    };

    let mut parsed = ultragraph::analyze::AnalyzeParams {
        preset: str_field("preset"),
        gql: str_field("gql"),
        limit: args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize),
        range: str_field("range"),
        ..Default::default()
    };

    if let Some(obj) = args.get("args").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            let as_text = match v {
                Value::String(s) => s.clone(),
                Value::Null => continue,
                // A model often sends a list param as a JSON array
                // (`{"files": ["a.ts","b.rs"]}`). analyze binds list params
                // from a comma-separated string, so join the string elements
                // rather than stringify the array (which would keep the
                // brackets and break the split).
                Value::Array(items) => items
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

/// Answer a `analyze` tool call from JSON args against an already-open store.
///
/// Shared with the chat and tour toolboxes in `serve` and `ug chat`. All three
/// hand the model the same MCP schemas, so all three have to answer every tool
/// those schemas advertise — and unlike [`Mcp::tool_analyze`], the chat
/// paths already hold a store and must not open a second one.
pub(crate) async fn run_analyze_json(
    store: &dyn KnowledgeStore,
    args: &Value,
) -> Result<String, String> {
    let params = parse_analyze_args(args);
    let answer = ultragraph::analyze::run(store, &params).await?;
    Ok(ultragraph::analyze::render::render(
        &answer,
        Render::Markdown,
    ))
}

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
    graph_cache: Mutex<GraphCache>,
    /// Built lazily on the first DB-backed call, then reused. `None` until
    /// then; a failed build isn't cached so a transient outage can recover.
    embedder: Mutex<Option<Arc<Embedder>>>,
}

impl Mcp {
    fn new() -> Self {
        Mcp {
            default_ctx: default_ctx(),
            project_cache: Mutex::new(HashMap::new()),
            graph_cache: Mutex::new(GraphCache::new()),
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

    /// Parse graph.json (cached by path, invalidated on mtime change, and
    /// bounded by a byte budget — see [`GraphCache`]).
    fn load_graph(&self, graph_path: &Path) -> Result<Arc<GraphData>, String> {
        let current = mtime_of(graph_path);
        {
            let mut cache = self.graph_cache.lock().unwrap();
            if let Some(hit) = cache.get(graph_path) {
                if hit.mtime.is_some() && hit.mtime == current {
                    return Ok(hit.parsed.clone());
                }
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
        let raw_len = raw.len();
        drop(raw);
        self.graph_cache.lock().unwrap().insert(
            graph_path.to_path_buf(),
            CachedGraph {
                parsed: parsed.clone(),
                raw_len,
                mtime: current,
            },
        );
        Ok(parsed)
    }

    fn invalidate(&self, graph_path: &Path) {
        self.graph_cache.lock().unwrap().remove(graph_path);
    }

    /// Index-freshness note appended to tool outputs: graph.json mtime vs the
    /// current mtimes of the files it indexed. Empty string when fresh (or no
    /// graph yet — the tool that needed it raises its own error).
    fn staleness_note(&self, ctx: &ProjectCtx) -> String {
        let Ok(graph) = self.load_graph(&ctx.graph_path) else {
            return String::new();
        };
        let Some(built_at) = mtime_of(&ctx.graph_path) else {
            return String::new();
        };
        // A repo root that no longer exists is not "N files deleted" — the
        // whole tree is gone and the index is frozen by definition. Reporting
        // every file as missing would make each tool answer noise, so skip the
        // note entirely and let the agent keep working off the index.
        if !ctx.repo_root.exists() {
            return String::new();
        }
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
        // Named, not just counted. "3 changed" leaves an agent unable to tell
        // whether the drift is in the files it just edited — in which case this
        // answer is about the previous version of its own work and it must
        // refresh before trusting it — or somewhere it does not care about.
        // Edited paths before deleted ones, capped so the note stays a line.
        let mut edited: Vec<&str> = Vec::new();
        let mut deleted: Vec<String> = Vec::new();
        for f in &files {
            match mtime_of(&ctx.repo_root.join(f)) {
                Some(mt) if mt > built_at => {
                    changed += 1;
                    if edited.len() < project::STALE_SAMPLE {
                        edited.push(f);
                    }
                }
                Some(_) => {}
                None => {
                    missing += 1;
                    if deleted.len() < project::STALE_SAMPLE {
                        deleted.push(format!("{} (deleted)", f));
                    }
                }
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
        let mut sample: Vec<String> = edited.iter().map(|f| f.to_string()).collect();
        sample.extend(deleted);
        sample.truncate(project::STALE_SAMPLE);
        let rest = (changed + missing).saturating_sub(sample.len());
        let mut which = sample.join(", ");
        if rest > 0 {
            which.push_str(&format!(", +{} more", rest));
        }
        let age = built_at
            .elapsed()
            .ok()
            .map(|d| d.as_secs() / 86400)
            .filter(|days| *days > 0)
            .map(|days| format!(" (index built {} day(s) ago)", days))
            .unwrap_or_default();
        format!(
            // Names the `gen` tool (the CLI's `ug gen`), not a stale
            // spelling: this line tells an agent what to call next, and a
            // name that is no longer dispatched sends it into an error.
            // `files` is named too, because the whole-repo refresh is the
            // wrong-sized hammer for the case that produces this note most
            // often — an agent that just edited three files.
            "\n\n⚠ Index may be stale: {} of {} indexed files since the last index{}.\n\
             Drifted: {}\n\
             This answer describes the last index, not the current tree. Call the gen tool with \
             files: [...] naming what you changed (fast), or with no arguments to refresh \
             everything.",
            bits.join(", "),
            files.len(),
            age,
            which
        )
    }

    // ── dispatch ───────────────────────────────────────────────────────────
    // (`vectors_note` is a free function below — it needs no server state.)

    async fn call_tool(&self, raw_name: &str, raw_args: &Value) -> Result<String, String> {
        let name = raw_name;
        let project = raw_args.get("project").and_then(|v| v.as_str());
        let ctx = self.resolve_ctx(project)?;

        let mut args = raw_args.clone();
        if let Some(obj) = args.as_object_mut() {
            obj.remove("project");
        }
        // Same coercion the chat path applies — MCP clients stringify
        // array arguments just as readily as chat models do.
        tools::normalize_args(name, &mut args);

        let with_staleness = |text: String| -> String { text + &self.staleness_note(&ctx) };
        // Only the vector-backed tools get the vectors note: it is a
        // statement about recall, and appending it to a structural answer
        // that is entirely current would be noise that reads as a warning.
        let with_vectors_note =
            |text: String| -> String { with_staleness(text) + &vectors_note(&ctx) };

        match name {
            // One implementation, two entry points. `semantic_search` is
            // the retired standalone tool, kept as an alias so agent
            // configs and transcripts that still name it keep working: it
            // is `search` with expansion off unless the caller asked for it.
            "search" | "semantic_search" => Ok(with_vectors_note(
                self.tool_search(&ctx, &args, name == "search").await?,
            )),
            // Statistics are the one structural question that cannot be
            // answered from graph.json: aggregation and reachability need
            // the store's indexed properties. It still needs no embedder,
            // so it stays available when `search` is not.
            "analyze" => Ok(with_staleness(self.tool_analyze(&ctx, args).await?)),
            "graph_schema" => {
                let mut text = self.tool_graph(name, &ctx, args).await?;
                text.push_str(&self.query_capabilities(&ctx).await);
                Ok(with_staleness(text))
            }
            "find_symbols" | "file_outline" | "find_usages" | "traverse" | "shortest_path"
            | "project_overview" | "get_code" | "context" => {
                Ok(with_staleness(self.tool_graph(name, &ctx, args).await?))
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
            "gen" => self.tool_gen(&ctx, &args).await,
            "ping_embedder" => {
                self.embedder()?.ping().await.map_err(|e| e.to_string())?;
                Ok("ok".to_string())
            }
            other => Err(format!("Unknown tool: {}", other)),
        }
    }

    /// Open this project's store for reading properties.
    ///
    /// Deliberately does **not** build an embedder. Statistics read
    /// properties, never vectors, and starting an embedding backend to
    /// answer "how many functions are over 50 lines" would make the
    /// cheapest tool in the set depend on the most fragile part of the
    /// stack. The dim comes from the store's own manifest instead of from
    /// a model probe.
    async fn open_query_store(&self, ctx: &ProjectCtx) -> Result<Box<dyn KnowledgeStore>, String> {
        let dim = ultragraph::storage::db::stored_embedding_dim(&ctx.db_path)
            .unwrap_or(ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM as u32);
        let spec = store_spec(&ctx.db_path, dim)?;
        open_store(&spec).await.map_err(|e| {
            format!(
                "analyze needs the indexed database, and {} could not be opened: {}.\n\
                 Run `ug gen` for this project. (Structural tools like traverse and \
                 find_usages read graph.json and keep working without it.)",
                ctx.db_path.display(),
                e
            )
        })
    }

    async fn tool_analyze(&self, ctx: &ProjectCtx, args: Value) -> Result<String, String> {
        let store = self.open_query_store(ctx).await?;
        run_analyze_json(store.as_ref(), &args).await
    }

    /// The half of `graph_schema` that only the store can answer: which
    /// properties are populated, and what presets exist.
    ///
    /// Appended rather than folded into `agent_tools::graph_schema`,
    /// which is deliberately graph-only. Best-effort: a project with no
    /// usable store still gets the node/edge half of the manifest, with
    /// one line saying why the rest is missing — that is more useful than
    /// failing the call an agent makes to orient itself.
    async fn query_capabilities(&self, ctx: &ProjectCtx) -> String {
        use ultragraph::analyze::presets;

        let mut out = String::from("\n**Queryable properties** (analyze)\n");

        match self.open_query_store(ctx).await {
            Ok(store) => {
                // Probe the full vocabulary at once, so a property that
                // this build writes but this *index* predates shows up as
                // absent rather than silently missing from the list.
                let gql = format!(
                    "MATCH (n) RETURN {}",
                    ultragraph::analyze::QUERYABLE_PROPERTIES
                        .iter()
                        .map(|p| format!("n.{}", p))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let coverage = ultragraph::analyze::coverage_for(
                    store.as_ref(),
                    &gql,
                    &ultragraph::storage::store::QueryLimits::default(),
                )
                .await;
                if coverage.is_empty() {
                    out.push_str("  (could not read property coverage from the index)\n");
                } else {
                    for c in &coverage {
                        if c.is_absent() {
                            out.push_str(&format!(
                                "  {:<16} NOT INDEXED — querying it returns 0, not an error\n",
                                c.property
                            ));
                        } else {
                            out.push_str(&format!(
                                "  {:<16} {}/{}\n",
                                c.property, c.present, c.total
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                out.push_str(&format!("  unavailable — {}\n", e.lines().next().unwrap_or("")));
            }
        }

        out.push_str("\n**analyze presets**\n");
        let mut category = "";
        for p in presets::all() {
            if p.category.as_str() != category {
                category = p.category.as_str();
                out.push_str(&format!("  [{}]\n", category));
            }
            let args: Vec<String> = p
                .params
                .iter()
                .map(|q| {
                    if q.default.is_none() {
                        format!("{}=<required>", q.name)
                    } else {
                        q.name.to_string()
                    }
                })
                .collect();
            out.push_str(&format!(
                "  {:<26} {}{}\n",
                p.name,
                p.description,
                if args.is_empty() {
                    String::new()
                } else {
                    format!(" (args: {})", args.join(", "))
                }
            ));
        }
        out
    }

    /// Run a graph-backed tool, feeding it the source the index captured.
    ///
    /// The pre-fetch is what lets `get_code` and `find_usages` answer for a
    /// project whose repo is not on this machine — the common case for a
    /// desktop MCP client. Best effort: a project with no usable store
    /// still gets its structural answer, with source read from the working
    /// tree if it happens to be there.
    async fn tool_graph(&self, name: &str, ctx: &ProjectCtx, args: Value) -> Result<String, String> {
        let graph = self.load_graph(&ctx.graph_path)?;
        let ids = ultragraph::agent_tools::source_node_ids(name, graph.as_ref(), &args);
        let indexed = match self.open_query_store(ctx).await {
            Ok(store) => {
                ultragraph::agent_tools::IndexedSource::load(store.as_ref(), &ids).await
            }
            Err(_) => ultragraph::agent_tools::IndexedSource::default(),
        };
        let output = run_tool(
            name,
            graph.as_ref(),
            ultragraph::agent_tools::SourceCtx::new(&indexed, &ctx.repo_root),
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

    /// `expand_default` is what applies when the caller passes no `expand`:
    /// true for `search`, false for the `semantic_search` alias.
    async fn tool_search(
        &self,
        ctx: &ProjectCtx,
        args: &Value,
        expand_default: bool,
    ) -> Result<String, String> {
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
        opts.expand = a.expand.unwrap_or(expand_default);
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

        let expanded = opts.expand;
        let result = search_kb(store.as_ref(), embedder.as_ref(), opts)
            .await
            .map_err(|e| format!("search_kb failed: {}", e))?;
        Ok(format::format_ranked_context(&result, expanded))
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

    /// Quiet re-run of the whole `gen` pipeline (index → graph → ingest);
    /// the CLI half is `ug gen`. Ingest
    /// failure (embedder down) is reported but doesn't fail the call: the
    /// graph-backed tools are already fresh at that point.
    /// `gen`, optionally scoped to the files the caller just edited.
    ///
    /// The MCP mirror of `ug update <file>...`. The pipeline is the same either
    /// way — the parse cache (blake3 per file) and the ingest node-hash diff
    /// already keep unchanged work out of the hot path, and the cross-file edge
    /// graph has to be re-resolved over the whole repo regardless, because an
    /// edge into a changed file depends on names the change may have moved.
    ///
    /// What `files` buys is not speed, it is the answer: the caller is told how
    /// many symbols each path it named actually contributed. That closes a real
    /// silent failure — edit a `.go` file in a repo `ug` indexes only for its
    /// Markdown, refresh, and every structural answer stays confidently empty.
    /// A per-file `0 symbols` says so; a whole-repo "12043 nodes" does not.
    async fn tool_gen(&self, ctx: &ProjectCtx, args: &Value) -> Result<String, String> {
        if !ctx.repo_root.exists() {
            return Err(format!(
                "Repo root {} no longer exists — re-run `ug gen -i <path>` manually.",
                ctx.repo_root.display()
            ));
        }
        let targets = gen_targets(ctx, args)?;
        let output_dir = ctx
            .graph_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        // The whole index -> graph -> write pipeline is unbounded CPU and IO:
        // it walks and parses every file in the repo, builds the graph, and
        // writes two multi-megabyte files. Running it inline would pin a tokio
        // worker for its entire duration. The stdio request loop is serial
        // today so nothing else is queued behind it, but that is a property of
        // the caller, not of this function. See Agents.md §9b.
        let repo_root = ctx.repo_root.clone();
        let graph_path = ctx.graph_path.clone();
        let out_dir = output_dir.clone();
        let (graph, nodes, edges) = tokio::task::spawn_blocking(
            move || -> Result<(GraphData, usize, usize), String> {
                std::fs::create_dir_all(&out_dir)
                    .map_err(|e| format!("failed to create {}: {}", out_dir.display(), e))?;

                let index_json = index_with_cache(
                    repo_root.to_string_lossy().into_owned(),
                    out_dir.to_string_lossy().into_owned(),
                );
                let graph_str = build_graph(index_json.clone());
                std::fs::write(&graph_path, &graph_str)
                    .map_err(|e| format!("failed to write graph.json: {}", e))?;
                std::fs::write(out_dir.join("indexed-tree.json"), &index_json)
                    .map_err(|e| format!("failed to write indexed-tree.json: {}", e))?;

                let graph: GraphData = serde_json::from_str(&graph_str)
                    .map_err(|e| format!("invalid graph: {}", e))?;
                let (nodes, edges) = (graph.nodes.len(), graph.edges.len());

                let name = project::read_meta(&out_dir).map(|m| m.name).unwrap_or_else(|| {
                    out_dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("default")
                        .to_string()
                });
                // Record the same file index `ug gen` writes. Without this an
                // MCP-driven gen would leave project.json without a `files`
                // list, permanently forcing /api/projects/staleness onto its
                // slow read-and-parse-the-graph fallback for this project.
                let _ = project::write_meta(
                    &out_dir,
                    &project::ProjectMeta::new(
                        &name,
                        &repo_root.to_string_lossy(),
                        nodes,
                        edges,
                    )
                    .with_graph_index(&graph),
                );

                Ok((graph, nodes, edges))
            },
        )
        .await
        .map_err(|e| format!("gen task failed: {}", e))??;

        let ingest_msg = match self.reindex_ingest(ctx, &graph).await {
            Ok(m) => m,
            Err(e) => format!(
                "db ingest FAILED ({}) — graph tools (find_symbols/get_code/...) are fresh, but search serves the previous embeddings until the embedder is reachable",
                e
            ),
        };
        self.invalidate(&ctx.graph_path);
        Ok(format!(
            "Reindexed {} → {}\n{} nodes, {} edges\n{}{}",
            ctx.repo_root.display(),
            output_dir.display(),
            nodes,
            edges,
            ingest_msg,
            per_file_report(&graph, &targets, &ctx.repo_root)
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
    /// `false` = seeds only, no graph expansion. Absent means "caller
    /// didn't say", which lets the `semantic_search` alias supply
    /// `false` as its default without overriding an explicit `true`.
    expand: Option<bool>,
    strategy: Option<String>,
    ppr_restart_prob: Option<f32>,
    ppr_max_iter: Option<usize>,
    ppr_seed_pool: Option<usize>,
    ppr_edge_weights: Option<HashMap<String, f32>>,
}

// ── stdio JSON-RPC server ──────────────────────────────────────────────────

fn run_server() {
    let rt = crate::cli::embed::tokio_runtime();
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
                    "instructions": SERVER_INSTRUCTIONS,
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
    let canonical = tool;
    if !tools::is_known_tool(canonical) {
        eprintln!(
            "Unknown tool '{}' — see `ug mcp list` for available tools.",
            tool
        );
        std::process::exit(1);
    }
    let rt = crate::cli::embed::tokio_runtime();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_for(repo: &Path) -> ProjectCtx {
        ProjectCtx {
            db_path: repo.join("ugdb"),
            repo_root: repo.to_path_buf(),
            graph_path: repo.join("graph.json"),
        }
    }

    /// The three spellings an agent reaches for all land on the same
    /// repo-relative path, and a path outside the repo is refused.
    ///
    /// The refusal is the load-bearing half: `gen` reporting success for a
    /// file it never indexed is how an agent ends up trusting a blast radius
    /// that predates its own edit — the failure the staleness note exists to
    /// prevent, re-introduced by the tool that is supposed to fix it.
    #[test]
    fn gen_targets_resolves_every_spelling_and_refuses_escapes() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join("src")).expect("src");
        std::fs::write(repo.path().join("src/a.rs"), "fn a() {}").expect("a");
        // The recorded root can carry a symlink (`/tmp` → `/private/tmp` on
        // macOS); canonicalise here so the assertions compare like with like.
        let root = std::fs::canonicalize(repo.path()).expect("canonical");
        let ctx = ctx_for(&root);

        let rel = gen_targets(&ctx, &json!({ "files": ["src/a.rs"] })).expect("relative");
        let abs = gen_targets(&ctx, &json!({ "files": [root.join("src/a.rs").to_string_lossy()] }))
            .expect("absolute");
        // A bare comma-separated string, the spelling `analyze --arg
        // files=a,b` teaches — accepted rather than rejected.
        let csv = gen_targets(&ctx, &json!({ "files": "src/a.rs" })).expect("csv");
        assert_eq!(rel, vec!["src/a.rs".to_string()]);
        assert_eq!(abs, rel);
        assert_eq!(csv, rel);

        // Absent `files` is a whole-repo gen, not an error.
        assert!(gen_targets(&ctx, &json!({})).expect("no files").is_empty());

        // A file that does not exist yet cannot be canonicalised, but "I just
        // deleted this" is a legitimate refresh — it resolves lexically.
        let deleted = gen_targets(&ctx, &json!({ "files": ["src/gone.rs"] })).expect("deleted");
        assert_eq!(deleted, vec!["src/gone.rs".to_string()]);

        for escape in [json!(["/etc/hosts"]), json!(["../outside.rs"])] {
            let err = gen_targets(&ctx, &json!({ "files": escape }))
                .expect_err("must not accept a path outside the repo");
            assert!(err.contains("outside the indexed repo"), "{err}");
        }
    }

    /// A named file that contributed nothing has to say so. A repo whose
    /// language `ug` does not parse still produces a graph and still answers
    /// questions, so "0 symbols" is the only signal that a structural answer
    /// about this file will be confidently empty forever.
    #[test]
    fn per_file_report_flags_an_unindexed_file() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("main.go"), "package main").expect("go");
        let graph = GraphData {
            nodes: Vec::new(),
            edges: Vec::new(),
            stats: None,
            resolution: None,
        };

        let present = per_file_report(&graph, &["main.go".to_string()], repo.path());
        assert!(present.contains("0 symbols"), "{present}");
        assert!(present.contains("extension not indexed"), "{present}");

        // Gone from disk is a different story with a different fix.
        let absent = per_file_report(&graph, &["removed.rs".to_string()], repo.path());
        assert!(absent.contains("deleted"), "{absent}");

        // Nothing named, nothing appended — a whole-repo gen reads as before.
        assert!(per_file_report(&graph, &[], repo.path()).is_empty());
    }

    /// A cache entry whose `approx_bytes` is `4 * bytes`.
    fn entry(bytes: usize) -> CachedGraph {
        CachedGraph {
            parsed: Arc::new(GraphData {
                nodes: Vec::new(),
                edges: Vec::new(),
                stats: None,
                resolution: None,
            }),
            raw_len: bytes,
            mtime: None,
        }
    }

    fn cache_with_budget(budget: usize) -> GraphCache {
        GraphCache {
            entries: HashMap::new(),
            lru: Vec::new(),
            budget,
        }
    }

    /// The whole point of the bound: without it this map grew by one parsed
    /// graph per project an agent touched and never gave any of it back, for
    /// the life of the session.
    #[test]
    fn the_graph_cache_evicts_once_it_is_over_budget() {
        let mut cache = cache_with_budget(40);
        cache.insert(PathBuf::from("/a/graph.json"), entry(10));
        cache.insert(PathBuf::from("/b/graph.json"), entry(10));

        assert!(
            cache.get(Path::new("/a/graph.json")).is_none(),
            "the least recently used graph should have been dropped"
        );
        assert!(
            cache.get(Path::new("/b/graph.json")).is_some(),
            "the graph just inserted stays resident"
        );
    }

    #[test]
    fn reading_a_graph_makes_it_the_last_to_be_evicted() {
        let mut cache = cache_with_budget(90);
        cache.insert(PathBuf::from("/a/graph.json"), entry(10));
        cache.insert(PathBuf::from("/b/graph.json"), entry(10));
        assert!(cache.get(Path::new("/a/graph.json")).is_some(), "hit on /a");

        // Three entries at 40 bytes each is 120 against a budget of 90, so
        // exactly one has to go — and it must not be the one just read.
        cache.insert(PathBuf::from("/c/graph.json"), entry(10));

        assert!(
            cache.get(Path::new("/b/graph.json")).is_none(),
            "/b was the least recently used"
        );
        assert!(cache.get(Path::new("/a/graph.json")).is_some());
        assert!(cache.get(Path::new("/c/graph.json")).is_some());
    }

    /// A single graph larger than the entire budget must still be cached:
    /// evicting it on insert would make every call re-read and re-parse it,
    /// which is strictly worse than holding it.
    #[test]
    fn a_graph_bigger_than_the_budget_is_still_cached() {
        let mut cache = cache_with_budget(8);
        cache.insert(PathBuf::from("/big/graph.json"), entry(1000));
        assert!(cache.get(Path::new("/big/graph.json")).is_some());
    }

    #[test]
    fn removing_a_graph_clears_it_from_the_recency_order_too() {
        // A stale name left in `lru` would make the next eviction pass do
        // nothing useful on its turn through the list.
        let mut cache = cache_with_budget(1024);
        cache.insert(PathBuf::from("/a/graph.json"), entry(10));
        cache.remove(Path::new("/a/graph.json"));
        assert!(cache.lru.is_empty(), "recency order still names /a");
        assert!(cache.get(Path::new("/a/graph.json")).is_none());
    }
}
