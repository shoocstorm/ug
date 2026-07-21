use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use ultragraph::storage::{
    self, open_store, search_kb as storage_search_kb,
    semantic_search as storage_semantic_search, Direction, Embedder,
    EmbedderConfig, KnowledgeStore, RankStrategy, SearchKbOptions, StoreSet, StoreSpec,
    DEFAULT_BASE_URL as DEFAULT_EMBED_BASE_URL, DEFAULT_MODEL as DEFAULT_EMBED_MODEL,
};
use ultragraph::agent_tools::{
    self, by_id_map, edge_type_str, looks_like_node_id, node_loc, node_type_str,
    strip_file_id_prefix, Render,
};
use ultragraph::types::{GraphData, GraphEdge, GraphNode, GraphNodeType};
use ultragraph::{
    build_graph, calculate_centrality, detect_cycles, index, index_with_cache, C_BLUE, C_BOLD,
    C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW,
};

mod chat;
mod config;
mod project;
mod serve;

// Bundled visualization assets so `ug gen` can produce a self-contained
// output directory without needing the source tree at runtime.
pub(crate) const VIS_HTML: &str = include_str!("./vis/visualization.html");
pub(crate) const VIS_BUNDLE: &[u8] = include_bytes!("./vis/ug-vis.bundle.js");
pub(crate) const VIS_FAVICON: &[u8] = include_bytes!("./vis/favicon.svg");
const VIS_MD: &str = include_str!("../../README.md");

fn main() {
    install_panic_hook();
    // Load environment defaults from `.env` (in CWD or any parent
    // directory). Real env vars still win — `dotenvy::dotenv` does not
    // override values already set in the process environment. Quiet
    // when no `.env` is present.
    let _ = dotenvy::dotenv();

    let args: Vec<String> = env::args().collect();

    // Suppressed when spawned as a subprocess by `ug serve`'s KB Manager
    // wizard (`POST /api/generate`) — the banner would otherwise dominate
    // the wizard's streamed log viewer. Also suppressed for bare `ug mcp`
    // (no `install`/`uninstall` subcommand): that mode is a stdio JSON-RPC
    // server, and the logo on stdout would corrupt the protocol stream.
    let is_mcp_server_mode = args.get(1).map(String::as_str) == Some("mcp")
        && !matches!(args.get(2).map(String::as_str), Some("install") | Some("uninstall"));
    if std::env::var("UG_QUIET_LOGO").is_err() && !is_mcp_server_mode {
        print_logo();
    }

    if args.len() >= 2 && (args[1] == "-v" || args[1] == "--version") {
        println!("ug version {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.len() < 2 {
        // No subcommand: just start the server. `ug serve` is safe even
        // with zero generated projects — it shows the KB Manager wizard
        // instead of erroring — so this removes the old "run gen, then
        // remember to run serve" two-step for the common case.
        eprintln!(
            "{C_CYAN}▸{C_RESET} No command given — starting {C_BOLD}ug serve{C_RESET}. Run {C_CYAN}ug help{C_RESET} for other commands."
        );
        serve::run_serve(&[]);
        return;
    }

    let cmd = &args[1];
    let cmd_args = &args[2..];

    match cmd.as_str() {
        // Primary entry points.
        "gen" => run_gen(cmd_args),
        "serve" => serve::run_serve(cmd_args),
        "app" => run_app(cmd_args),
        "api" => run_api(cmd_args),
        // Pipeline steps `gen` runs for you.
        "index" => run_index(cmd_args),
        "graph" => run_graph(cmd_args),
        "ingest" => run_ingest(cmd_args),
        // Graph analysis (offline, in-memory). Project-scoped via
        // -n/--name; the pre-rename names stay as aliases.
        "graph_analyze" | "analyze" => run_graph_analyze(cmd_args),
        "graph_bfs" | "bfs" => run_graph_bfs(cmd_args),
        "graph_filter" | "filter" => run_graph_filter(cmd_args),
        "graph_centrality" | "centrality" => run_graph_centrality(cmd_args),
        "graph_cycles" | "cycles" => run_graph_cycles(cmd_args),
        // Agent tools (graph.json-backed, for AI coding agents). Names match
        // the MCP tools one-for-one.
        "find_symbols" => run_find_symbols(cmd_args),
        // Same scan as find_symbols, with docstring matching on by default.
        "graph_search" => run_graph_search(cmd_args),
        "file_outline" => run_file_outline(cmd_args),
        "get_code" => run_get_code(cmd_args),
        "find_usages" => run_find_usages(cmd_args),
        "project_overview" => run_project_overview(cmd_args),
        "shortest_path" | "graph_path" | "path" => run_graph_path(cmd_args),
        "graph_schema" => run_graph_schema(cmd_args),
        // Retrieval (OverGraph-backed). `search` is the canonical name the
        // MCP tool uses; `hybrid_search` is the pre-rename alias.
        "semantic_search" => run_semantic_search(cmd_args),
        "search" | "hybrid_search" => run_hybrid_search(cmd_args),
        "traverse" => run_traverse(cmd_args),
        "chat" => run_chat(cmd_args),
        // Project management.
        "list_projects" | "list" => run_list(cmd_args),
        "rm" => run_rm(cmd_args),
        "uninstall" => run_uninstall(cmd_args),
        "upgrade" | "update" => run_upgrade(cmd_args),
        "config" => run_config(cmd_args),
        "doctor" => run_doctor(cmd_args),
        "mcp" => run_mcp(cmd_args),
        "help" | "-h" | "--help" => {
            print_help();
        }
        _ => {
            eprintln!("Unknown command: {}", cmd);
            print_help();
            std::process::exit(1);
        }
    }
}

// ---------- Argument helpers ----------

/// Find the first value for any of the given flag names. Returns the
/// argument immediately following the matched flag, or `None` if no
/// flag matched or it was the last token.
pub(crate) fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if names.contains(&args[i].as_str()) && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

pub(crate) fn flag_value_or(args: &[String], names: &[&str], default: &str) -> String {
    flag_value(args, names).unwrap_or_else(|| default.to_string())
}

pub(crate) fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Collect every value for a repeatable flag (e.g. `-t function -t class`).
fn multi_flag(args: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if names.contains(&args[i].as_str()) && i + 1 < args.len() {
            out.push(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// First non-flag positional argument, skipping flag/value pairs whose
/// flag name is listed in `value_flags`. Anything else starting with
/// `-` (or that doesn't start with `-`) is treated as a positional.
fn first_positional(args: &[String], value_flags: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if value_flags.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Some(a.clone());
        }
    }
    None
}

// ---------- IO helpers ----------

fn write_file(path: &str, data: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, data).expect("Failed to write output");
}

/// If `output_path` is set, write to it and print a confirmation;
/// otherwise dump the payload to stdout.
fn write_or_print(output_path: Option<&str>, data: &str, label: &str) {
    match output_path {
        Some(p) => {
            if Path::new(p).is_dir() {
                eprintln!(
                    "Error: '{}' is a directory, not a file. Omit -o flag or specify a file path.",
                    p
                );
                std::process::exit(1);
            }
            write_file(p, data);
            println!("Wrote {} to {}", label, p);
        }
        None => println!("{}", data),
    }
}

// ---------- Precedence helper ----------

/// Where a resolved config value came from: an explicit CLI flag, a
/// named env var, a key persisted in `~/.ug/config.json` (`ug config
/// set`), or none of those (caller applies its own default). `ug
/// doctor` reports this so the multi-tier fallback chain is inspectable
/// instead of implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefSource {
    Flag,
    Env(&'static str),
    Config(&'static str),
    Default,
}

/// Three-tier precedence: an explicit flag value wins, else the named
/// env var (blank values are treated as unset), else `None`/`Default`.
pub(crate) fn resolve_pref(
    flag: Option<String>,
    env_key: &'static str,
) -> (Option<String>, PrefSource) {
    if let Some(v) = flag {
        return (Some(v), PrefSource::Flag);
    }
    match std::env::var(env_key) {
        Ok(v) if !v.trim().is_empty() => (Some(v), PrefSource::Env(env_key)),
        _ => (None, PrefSource::Default),
    }
}

#[cfg(test)]
mod pref_tests {
    use super::{resolve_pref, PrefSource};

    // Each test uses its own env var name so they can't race each other
    // under cargo's default parallel test execution.

    #[test]
    fn flag_wins_over_env_and_default() {
        std::env::set_var("UG_TEST_PREF_FLAG_WINS", "from-env");
        let (val, src) = resolve_pref(Some("from-flag".to_string()), "UG_TEST_PREF_FLAG_WINS");
        assert_eq!(val.as_deref(), Some("from-flag"));
        assert_eq!(src, PrefSource::Flag);
        std::env::remove_var("UG_TEST_PREF_FLAG_WINS");
    }

    #[test]
    fn env_wins_when_no_flag() {
        std::env::set_var("UG_TEST_PREF_ENV_WINS", "from-env");
        let (val, src) = resolve_pref(None, "UG_TEST_PREF_ENV_WINS");
        assert_eq!(val.as_deref(), Some("from-env"));
        assert_eq!(src, PrefSource::Env("UG_TEST_PREF_ENV_WINS"));
        std::env::remove_var("UG_TEST_PREF_ENV_WINS");
    }

    #[test]
    fn default_when_neither_set() {
        std::env::remove_var("UG_TEST_PREF_NEITHER_SET");
        let (val, src) = resolve_pref(None, "UG_TEST_PREF_NEITHER_SET");
        assert_eq!(val, None);
        assert_eq!(src, PrefSource::Default);
    }

    #[test]
    fn blank_env_value_treated_as_unset() {
        std::env::set_var("UG_TEST_PREF_BLANK", "   ");
        let (val, src) = resolve_pref(None, "UG_TEST_PREF_BLANK");
        assert_eq!(val, None);
        assert_eq!(src, PrefSource::Default);
        std::env::remove_var("UG_TEST_PREF_BLANK");
    }
}

// ---------- Embedder / runtime helpers ----------

pub(crate) fn embedder_from_args(args: &[String]) -> Embedder {
    let (dim_raw, _) = config::resolve_pref_cfg(flag_value(args, &["--embedding-dim"]), "embed.dim");
    let dim = dim_raw.and_then(|s| s.parse().ok());
    let (base_url, _) = config::resolve_pref_cfg(flag_value(args, &["--base-url"]), "embed.base_url");
    // Presence of --base-url (or $UG_EMBED_BASE_URL, or a persisted
    // embed.base_url) is the single switch between in-process (default)
    // and the legacy HTTP backend. --model applies to both: for local it
    // picks a fastembed catalog entry; for remote it's the model field
    // sent in the /v1/embeddings request.
    let want_remote = base_url.is_some();
    let (api_key, _) = config::resolve_pref_cfg(flag_value(args, &["--api-key"]), "embed.api_key");
    let (model, _) = config::resolve_pref_cfg(flag_value(args, &["--model"]), "embed.model");
    let cfg = EmbedderConfig::with_overrides(base_url, api_key, model, dim, None, None);
    let result = if want_remote {
        Embedder::remote(cfg)
    } else {
        Embedder::local(cfg)
    };
    let embedder = result.unwrap_or_else(|e| {
        eprintln!("failed to build embedder: {}", e);
        std::process::exit(1);
    });
    announce_embedder(&embedder, dim.is_some());
    embedder
}

/// One-line banner on stderr so the user can see which backend the
/// command is using before any progress output appears. Stderr so that
/// stdout-bound JSON from `semantic_search` / `hybrid_search` stays
/// clean for piping.
fn announce_embedder(embedder: &Embedder, dim_was_explicit: bool) {
    let cfg = embedder.config();
    let dim_label = if dim_was_explicit {
        format!("dim={}", cfg.dim)
    } else {
        format!("dim={} (auto-probe)", cfg.dim)
    };
    match embedder {
        Embedder::Local(_) => eprintln!(
            "{C_CYAN}▸{C_RESET} Embedder: {C_BOLD}{C_GREEN}local{C_RESET} (fastembed, in-process) — model={C_BOLD}{}{C_RESET}, {}",
            cfg.model, dim_label
        ),
        Embedder::Remote(_) => eprintln!(
            "{C_CYAN}▸{C_RESET} Embedder: {C_BOLD}{C_YELLOW}remote{C_RESET} (HTTP /v1/embeddings) — model={C_BOLD}{}{C_RESET}, base_url={}, {}",
            cfg.model, cfg.base_url, dim_label
        ),
    }
}

/// Like `embedder_from_args`, but used by `ug chat` where a chat model
/// is also in play. `--embedding-model` (or `$UG_EMBED_MODEL`) selects
/// the embeddings independently of `--chat-model` — `--model` has no
/// effect here, since with two services in the same command it's
/// ambiguous which one it would mean.
///
/// For the base-url / api-key, `--embedding-base-url` /
/// `--embedding-api-key` win when set, otherwise the shared
/// `--base-url` / `--api-key` apply (this matches the common case where
/// chat and embedding share a single OpenAI-compatible host), and
/// `$UG_EMBED_BASE_URL` / `$UG_EMBED_API_KEY` are the last fallback.
pub(crate) fn embedder_from_chat_args(args: &[String]) -> Embedder {
    let (dim_raw, _) = config::resolve_pref_cfg(flag_value(args, &["--embedding-dim"]), "embed.dim");
    let dim = dim_raw.and_then(|s| s.parse().ok());
    let base_url_flag = flag_value(args, &["--embedding-base-url"])
        .or_else(|| flag_value(args, &["--base-url"]));
    let (base_url, _) = config::resolve_pref_cfg(base_url_flag, "embed.base_url");
    let api_key_flag = flag_value(args, &["--embedding-api-key"])
        .or_else(|| flag_value(args, &["--api-key"]));
    let (api_key, _) = config::resolve_pref_cfg(api_key_flag, "embed.api_key");
    let (model, _) =
        config::resolve_pref_cfg(flag_value(args, &["--embedding-model"]), "embed.model");
    let want_remote = base_url.is_some();
    let cfg = EmbedderConfig::with_overrides(base_url, api_key, model, dim, None, None);
    let result = if want_remote {
        Embedder::remote(cfg)
    } else {
        Embedder::local(cfg)
    };
    let embedder = result.unwrap_or_else(|e| {
        eprintln!("failed to build embedder: {}", e);
        std::process::exit(1);
    });
    announce_embedder(&embedder, dim.is_some());
    embedder
}

pub(crate) fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

// ---------- Destination / store helpers ----------

/// Parse `--dest <kind>[,<kind>...]` into one or more `StoreSpec`s.
/// Defaults to `overgraph` when no `--dest` is supplied so existing
/// invocations keep working unchanged. CLI flags override env vars
/// (`UG_DEST`, `UG_NEO4J_*`).
fn store_specs_from_args(args: &[String], embedding_dim: u32) -> Vec<StoreSpec> {
    let dest = flag_value(args, &["--dest"])
        .or_else(|| std::env::var("UG_DEST").ok())
        .unwrap_or_else(|| "overgraph".to_string());

    // The OverGraph dir path. Read commands (semantic_search,
    // hybrid_search, traverse, chat) select a project by name via
    // -n/--name, resolved to ~/.ug/<name>/ugdb; ingest uses -o/--output
    // directly (which is also the JSON output file in some commands,
    // so -o always wins over the -n-derived path when both are
    // present).
    let og_path = flag_value(args, &["-n", "--name"])
        .map(|n| project::project_dir(&project::sanitize_name(&n)).join("ugdb").to_string_lossy().into_owned())
        .or_else(|| flag_value(args, &["-o", "--output"]))
        .or_else(|| std::env::var("UG_DB_PATH").ok())
        .unwrap_or_else(project::default_read_db_path);

    let neo4j_uri = flag_value(args, &["--neo4j-uri"]).or_else(|| std::env::var("UG_NEO4J_URI").ok());
    let neo4j_user = flag_value(args, &["--neo4j-user"])
        .or_else(|| std::env::var("UG_NEO4J_USER").ok())
        .unwrap_or_else(|| "neo4j".to_string());
    let neo4j_password = flag_value(args, &["--neo4j-password"])
        .or_else(|| std::env::var("UG_NEO4J_PASSWORD").ok())
        .unwrap_or_default();
    let neo4j_database = flag_value(args, &["--neo4j-database"])
        .or_else(|| std::env::var("UG_NEO4J_DATABASE").ok());

    let mut specs: Vec<StoreSpec> = Vec::new();
    for kind in dest.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match kind {
            "overgraph" | "og" => specs.push(StoreSpec::Overgraph {
                path: PathBuf::from(&og_path),
                embedding_dim,
            }),
            "neo4j" | "neo" => {
                let uri = neo4j_uri.clone().unwrap_or_else(|| {
                    eprintln!(
                        "Error: --dest neo4j requires --neo4j-uri (or UG_NEO4J_URI env var)"
                    );
                    std::process::exit(2);
                });
                if neo4j_password.is_empty() {
                    eprintln!(
                        "Error: --dest neo4j requires --neo4j-password (or UG_NEO4J_PASSWORD env var)"
                    );
                    std::process::exit(2);
                }
                specs.push(StoreSpec::Neo4j {
                    uri,
                    user: neo4j_user.clone(),
                    password: neo4j_password.clone(),
                    database: neo4j_database.clone(),
                    embedding_dim,
                });
            }
            other => {
                eprintln!(
                    "Error: unknown destination '{}' (expected: overgraph, neo4j)",
                    other
                );
                std::process::exit(2);
            }
        }
    }
    if specs.is_empty() {
        eprintln!("Error: --dest cannot be empty");
        std::process::exit(2);
    }
    specs
}

/// Read commands accept exactly one destination — the first parsed
/// spec wins, with a hard error on multi-spec inputs so users don't
/// accidentally fan out a query.
fn single_store_spec_from_args(args: &[String], embedding_dim: u32) -> StoreSpec {
    let specs = store_specs_from_args(args, embedding_dim);
    if specs.len() > 1 {
        eprintln!(
            "Error: this command accepts a single --dest, not a comma-separated list ({} given)",
            specs.len()
        );
        std::process::exit(2);
    }
    specs.into_iter().next().expect("at least one spec")
}

/// Banner indicating which backends a command is targeting.
fn announce_destinations(specs: &[StoreSpec]) {
    let names: Vec<&str> = specs.iter().map(|s| s.name()).collect();
    eprintln!(
        "{C_CYAN}▸{C_RESET} Destination(s): {C_BOLD}{}{C_RESET}",
        names.join(", ")
    );
}

/// Force-exit on panic so the process actually terminates. The local
/// (fastembed/ONNX) backend spawns rayon + ORT worker threads that are
/// not daemonized — a normal panic prints the message but then hangs
/// forever waiting for those threads, leaving Ctrl+C as the only way
/// out. Installing this hook keeps the default panic message but
/// forces a hard exit immediately after.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        std::process::exit(101);
    }));
}

// ---------- Commands ----------

fn run_index(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_index_help();
        return;
    }

    let path = flag_value(args, &["-i", "--input"])
        .or_else(|| {
            first_positional(
                args,
                &["-i", "--input", "-o", "--output", "-c", "--cache", "-n", "--name"],
            )
        })
        .unwrap_or_else(|| ".".to_string());
    let cache = flag_value(args, &["-c", "--cache"]);
    let project_dir = project::project_dir(&project::resolve_project_name(args, &path));
    let output = flag_value(args, &["-o", "--output"]).unwrap_or_else(|| {
        project_dir
            .join("indexed-tree.json")
            .to_string_lossy()
            .into_owned()
    });

    let result = match cache {
        Some(c) => index_with_cache(path, c),
        None => index(path),
    };
    write_file(&output, &result);
    println!(
        "{C_GREEN}✓{C_RESET} Generated index in {C_BOLD}{}{C_RESET}",
        output
    );
}

fn run_graph(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_help();
        return;
    }

    let project_dir = project::project_dir(&project::resolve_project_name(args, "."));
    let input = flag_value(args, &["-i", "--input"]).unwrap_or_else(|| {
        project_dir
            .join("indexed-tree.json")
            .to_string_lossy()
            .into_owned()
    });
    let output = flag_value(args, &["-o", "--output"])
        .unwrap_or_else(|| project_dir.join("graph.json").to_string_lossy().into_owned());

    let index_json = fs::read_to_string(&input).expect("Failed to read input");
    let result = build_graph(index_json);
    write_file(&output, &result);
    println!(
        "{C_GREEN}✓{C_RESET} Generated graph in {C_BOLD}{}{C_RESET}",
        output
    );
}

// ---------- Graph analysis (project-scoped, in-memory) ----------
//
// These commands read a project's graph.json — selected with
// `-n/--name`, else the cwd's project, else the most recently generated
// one — the same resolution the agent tools use. `-i/--input` still
// accepts an explicit graph.json for one-off files, and a legacy
// `<graph-file>` first positional is still honoured.
//
// Output: a readable report by default, raw JSON with `--json`, and
// `-o/--output <file>` writes that JSON to disk.

/// Value-taking flags shared by the graph-analysis commands, so
/// positionals can be told apart from flag values.
const GRAPH_VALUE_FLAGS: &[&str] = &[
    "-i",
    "--input",
    "-n",
    "--name",
    "-o",
    "--output",
    "-t",
    "--type",
    "--edge-type",
    "-f",
    "--file",
    "-l",
    "--limit",
    "-k",
    "--hops",
    "-d",
    "--direction",
    "--top",
    "--min-len",
    "--max-len",
    "--from",
    "--to",
];

/// Split an analysis command's arguments into (args used to locate the
/// graph, remaining positionals). A legacy `<graph-file>` first
/// positional — an existing `.json` file — is promoted to `-i` and
/// dropped from the positionals, so the pre-rename call style keeps
/// working.
fn analysis_input(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut load_args = args.to_vec();
    let mut pos = positionals(args, GRAPH_VALUE_FLAGS);
    if flag_value(args, &["-i", "--input"]).is_none() {
        if let Some(first) = pos.first().cloned() {
            if first.ends_with(".json") && Path::new(&first).is_file() {
                pos.remove(0);
                load_args.push("-i".to_string());
                load_args.push(first);
            }
        }
    }
    (load_args, pos)
}

/// Where a command's result should go.
enum Emit {
    Human,
    Json,
    File(String),
}

fn emit_mode(args: &[String]) -> Emit {
    if let Some(p) = flag_value(args, &["-o", "--output"]) {
        Emit::File(p)
    } else if has_flag(args, "--json") {
        Emit::Json
    } else {
        Emit::Human
    }
}

/// Write or print the raw JSON when `-o`/`--json` was given. Returns
/// true when the output was consumed, so the caller skips its
/// human-readable rendering.
fn emit_raw(args: &[String], json: &str, label: &str) -> bool {
    match emit_mode(args) {
        Emit::File(p) => {
            write_or_print(Some(&p), json, label);
            true
        }
        Emit::Json => {
            println!("{}", json);
            true
        }
        Emit::Human => false,
    }
}

/// Lowercased `-t/--type` values (node types for most commands).
fn type_filter(args: &[String], names: &[&str]) -> Vec<String> {
    multi_flag(args, names)
        .iter()
        .map(|t| t.to_lowercase())
        .collect()
}

fn limit_or(args: &[String], names: &[&str], default: usize) -> usize {
    flag_value(args, names)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Resolve a user-supplied node reference to a node id. Accepts an exact
/// nodeId, a repo-relative (or suffix-unique) file path, or a symbol
/// name ranked exact > prefix > substring. Ambiguity and misses print
/// candidates and exit — every downstream algorithm needs one id.
fn resolve_node_ref(graph: &GraphData, input: &str) -> String {
    if let Some(n) = graph.nodes.iter().find(|n| n.id == input) {
        return n.id.clone();
    }

    // File path: exact repo-relative match, else unique path suffix.
    let path = strip_file_id_prefix(input);
    let suffix = format!("/{}", path.trim_start_matches('/'));
    let mut file_hits: Vec<&GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.node_type, GraphNodeType::File))
        .filter(|n| {
            n.file.as_deref() == Some(path)
                || n.file.as_deref().map(|f| f.ends_with(&suffix)).unwrap_or(false)
        })
        .collect();
    file_hits.sort_by(|a, b| a.id.cmp(&b.id));
    file_hits.dedup_by(|a, b| a.id == b.id);
    if file_hits.len() == 1 {
        return file_hits[0].id.clone();
    }
    if file_hits.len() > 1 {
        exit_ambiguous(input, &file_hits);
    }

    // Symbol name.
    let q = input.to_lowercase();
    let mut hits: Vec<(u8, &GraphNode)> = Vec::new();
    for n in &graph.nodes {
        let nm = n.name.to_lowercase();
        let rank = if nm == q {
            0
        } else if nm.starts_with(&q) {
            1
        } else if nm.contains(&q) {
            2
        } else {
            3
        };
        if rank < 3 {
            hits.push((rank, n));
        }
    }
    if hits.is_empty() {
        eprintln!(
            "✗ Nothing in the graph matches '{}' — look it up with {C_CYAN}ug find_symbols{C_RESET}, or pass a node id directly.",
            input
        );
        std::process::exit(1);
    }
    let best = hits.iter().map(|(r, _)| *r).min().unwrap_or(0);
    let best_hits: Vec<&GraphNode> = hits
        .iter()
        .filter(|(r, _)| *r == best)
        .map(|(_, n)| *n)
        .collect();
    if best_hits.len() > 1 {
        exit_ambiguous(input, &best_hits);
    }
    best_hits[0].id.clone()
}

/// Print the candidates behind an ambiguous reference and exit — the
/// user picks one id and re-runs.
fn exit_ambiguous(input: &str, candidates: &[&GraphNode]) -> ! {
    eprintln!(
        "'{}' matches {} nodes — re-run with one of these ids:",
        input,
        candidates.len()
    );
    for n in candidates.iter().take(15) {
        eprintln!(
            "  {} {}  {}  id: {}",
            node_type_str(&n.node_type),
            n.name,
            node_loc(n),
            n.id
        );
    }
    if candidates.len() > 15 {
        eprintln!("  … and {} more", candidates.len() - 15);
    }
    std::process::exit(1);
}

/// One-line description of a node, used across the analysis reports.
fn node_line(n: &GraphNode) -> String {
    format!(
        "{} {C_BOLD}{}{C_RESET}  {C_DIM}{}{C_RESET}  id: {C_CYAN}{}{C_RESET}",
        node_type_str(&n.node_type),
        n.name,
        node_loc(n),
        n.id
    )
}

/// Does this node pass the `-t/--type` (node type) and `-f/--file`
/// (path prefix) filters?
fn node_passes(n: &GraphNode, types: &[String], file_prefix: Option<&str>) -> bool {
    if !types.is_empty() && !types.contains(&node_type_str(&n.node_type).to_lowercase()) {
        return false;
    }
    if let Some(p) = file_prefix {
        if !n.file.as_deref().unwrap_or("").starts_with(p) {
            return false;
        }
    }
    true
}

/// Which way edges are followed by `graph_bfs`.
#[derive(Clone, Copy, PartialEq)]
enum BfsDir {
    Out,
    In,
    Both,
}

fn bfs_dir_from_args(args: &[String]) -> BfsDir {
    match flag_value_or(args, &["-d", "--direction"], "out").to_lowercase().as_str() {
        "out" | "outbound" | "forward" => BfsDir::Out,
        "in" | "inbound" | "reverse" => BfsDir::In,
        "both" | "any" | "undirected" => BfsDir::Both,
        other => {
            eprintln!("Error: unknown --direction '{}' (expected: out, in, both)", other);
            std::process::exit(2);
        }
    }
}

fn bfs_dir_str(d: BfsDir) -> &'static str {
    match d {
        BfsDir::Out => "out",
        BfsDir::In => "in",
        BfsDir::Both => "both",
    }
}

/// K-hop BFS over the in-memory graph, honouring direction and an
/// optional edge-type allowlist. Returns hop distances per node id plus
/// the edges walked.
fn k_hop(
    graph: &GraphData,
    start: &str,
    k: u32,
    dir: BfsDir,
    edge_types: &[String],
) -> (std::collections::HashMap<String, u32>, Vec<GraphEdge>) {
    use std::collections::HashMap;

    let mut adj: HashMap<&str, Vec<(&str, &GraphEdge)>> = HashMap::new();
    for e in &graph.edges {
        if !edge_types.is_empty()
            && !edge_types.contains(&edge_type_str(&e.edge_type).to_lowercase())
        {
            continue;
        }
        if matches!(dir, BfsDir::Out | BfsDir::Both) {
            adj.entry(e.source.as_str()).or_default().push((e.target.as_str(), e));
        }
        if matches!(dir, BfsDir::In | BfsDir::Both) {
            adj.entry(e.target.as_str()).or_default().push((e.source.as_str(), e));
        }
    }

    let mut dist: HashMap<String, u32> = HashMap::new();
    let mut walked: Vec<GraphEdge> = Vec::new();
    dist.insert(start.to_string(), 0);
    let mut frontier: Vec<String> = vec![start.to_string()];

    for d in 1..=k {
        let mut next: Vec<String> = Vec::new();
        for id in &frontier {
            let Some(neighbors) = adj.get(id.as_str()) else {
                continue;
            };
            for (nbr, e) in neighbors {
                walked.push((*e).clone());
                if !dist.contains_key(*nbr) {
                    dist.insert((*nbr).to_string(), d);
                    next.push((*nbr).to_string());
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    walked.sort_by(|a, b| {
        (a.source.as_str(), a.target.as_str(), edge_type_str(&a.edge_type)).cmp(&(
            b.source.as_str(),
            b.target.as_str(),
            edge_type_str(&b.edge_type),
        ))
    });
    walked.dedup_by(|a, b| {
        a.source == b.source && a.target == b.target && edge_type_str(&a.edge_type) == edge_type_str(&b.edge_type)
    });
    (dist, walked)
}

fn run_graph_bfs(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_bfs_help();
        return;
    }
    let (load_args, pos) = analysis_input(args);
    if pos.is_empty() {
        eprintln!("Usage: ug graph_bfs <node-id-or-name> [k] [-k|--hops <n>] [-d|--direction out|in|both] [-t|--type <node-type>]... [--edge-type <type>]... [-l|--limit <n>] [-n|--name <project>]");
        std::process::exit(1);
    }
    // `k` is a flag, but the old positional form (`... <start> 2`) still works.
    let k: u32 = flag_value(args, &["-k", "--hops"])
        .or_else(|| pos.get(1).cloned())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let dir = bfs_dir_from_args(args);
    let edge_types = type_filter(args, &["--edge-type"]);
    let types = type_filter(args, &["-t", "--type"]);
    let file_prefix = flag_value(args, &["-f", "--file"]);
    let limit = limit_or(args, &["-l", "--limit"], 50);

    let (graph, _raw, _path) = load_agent_graph(&load_args);
    let start = resolve_node_ref(&graph, &pos[0]);
    let (dist, edges) = k_hop(&graph, &start, k, dir, &edge_types);

    // Hop order first, then type, then name — reads like an expanding ring.
    let mut reached: Vec<(u32, &GraphNode)> = graph
        .nodes
        .iter()
        .filter_map(|n| dist.get(&n.id).map(|d| (*d, n)))
        .filter(|(d, n)| *d == 0 || node_passes(n, &types, file_prefix.as_deref()))
        .collect();
    reached.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(node_type_str(&a.1.node_type).cmp(node_type_str(&b.1.node_type)))
            .then(a.1.name.cmp(&b.1.name))
    });

    let json = {
        let nodes: Vec<serde_json::Value> = reached
            .iter()
            .map(|(d, n)| {
                let mut v = serde_json::to_value(n).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(o) = v.as_object_mut() {
                    o.insert("distance".to_string(), serde_json::json!(d));
                }
                v
            })
            .collect();
        serde_json::json!({
            "start": start,
            "hops": k,
            "direction": bfs_dir_str(dir),
            "count": nodes.len(),
            "nodes": nodes,
            "edges": edges,
        })
        .to_string()
    };
    if emit_raw(args, &json, "BFS result") {
        return;
    }

    let start_node = graph.nodes.iter().find(|n| n.id == start);
    println!(
        "{C_BOLD}{}-hop BFS ({}bound){C_RESET} from {}",
        k,
        bfs_dir_str(dir),
        start_node.map(node_line).unwrap_or_else(|| start.clone())
    );
    println!(
        "{C_DIM}{} node(s) reached, {} edge(s) walked{}{C_RESET}",
        reached.len().saturating_sub(1),
        edges.len(),
        if edge_types.is_empty() {
            String::new()
        } else {
            format!(" · edge types: {}", edge_types.join(", "))
        }
    );
    println!();

    let mut shown = 0usize;
    let mut current_hop = u32::MAX;
    for (d, n) in reached.iter().filter(|(d, _)| *d > 0) {
        if shown >= limit {
            println!();
            println!(
                "{C_DIM}(+{} more — raise -l/--limit or narrow with -t/--type){C_RESET}",
                reached.len().saturating_sub(1) - shown
            );
            break;
        }
        if *d != current_hop {
            if current_hop != u32::MAX {
                println!();
            }
            println!("{C_BOLD}hop {}{C_RESET}", d);
            current_hop = *d;
        }
        println!("  {}", node_line(n));
        shown += 1;
    }
    if shown == 0 {
        println!("No neighbors within {} hop(s). Try -d both, a larger -k, or drop the filters.", k);
    }
    println!();
    println!("{C_DIM}Next:{C_RESET} {C_CYAN}ug get_code <id>{C_RESET} to read one · {C_CYAN}ug graph_path <a> <b>{C_RESET} to see how two connect");
}

fn run_graph_filter(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_filter_help();
        return;
    }
    let (load_args, pos) = analysis_input(args);
    let mut edge_types: Vec<String> = pos.iter().map(|s| s.to_lowercase()).collect();
    edge_types.extend(type_filter(args, &["-t", "--type", "--edge-type"]));
    let limit = limit_or(args, &["-l", "--limit"], 50);
    let file_prefix = flag_value(args, &["-f", "--file"]);
    let from_ref = flag_value(args, &["--from"]);
    let to_ref = flag_value(args, &["--to"]);

    let (graph, _raw, _path) = load_agent_graph(&load_args);
    let by_id = by_id_map(&graph);

    // No edge type given: report what's available instead of erroring —
    // that's the question a user without the type list is really asking.
    if edge_types.is_empty() && from_ref.is_none() && to_ref.is_none() {
        use std::collections::HashMap;
        let mut counts: HashMap<&'static str, usize> = HashMap::new();
        for e in &graph.edges {
            *counts.entry(edge_type_str(&e.edge_type)).or_insert(0) += 1;
        }
        let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        println!("{C_BOLD}Edge types in this graph{C_RESET} — {} edge(s) total", graph.edges.len());
        println!();
        for (t, c) in rows {
            println!("- {C_CYAN}{}{C_RESET}  {}", t, c);
        }
        println!();
        println!("Pass one or more to filter, e.g. {C_CYAN}ug graph_filter Calls Imports{C_RESET}");
        return;
    }

    let from_id = from_ref.map(|r| resolve_node_ref(&graph, &r));
    let to_id = to_ref.map(|r| resolve_node_ref(&graph, &r));

    let matched: Vec<&GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| {
            edge_types.is_empty()
                || edge_types.contains(&edge_type_str(&e.edge_type).to_lowercase())
        })
        .filter(|e| from_id.as_deref().map(|id| e.source == id).unwrap_or(true))
        .filter(|e| to_id.as_deref().map(|id| e.target == id).unwrap_or(true))
        .filter(|e| match &file_prefix {
            None => true,
            Some(p) => [&e.source, &e.target].iter().any(|id| {
                by_id
                    .get(id.as_str())
                    .and_then(|n| n.file.as_deref())
                    .map(|f| f.starts_with(p.as_str()))
                    .unwrap_or(false)
            }),
        })
        .collect();

    let json = serde_json::json!({ "count": matched.len(), "edges": matched }).to_string();
    if emit_raw(args, &json, "filtered edges") {
        return;
    }

    println!(
        "{C_BOLD}Edges{C_RESET} — {} match(es) of {}{}",
        matched.len(),
        graph.edges.len(),
        if matched.len() > limit {
            format!(", showing {}", limit)
        } else {
            String::new()
        }
    );
    println!();
    let label = |id: &str| -> String {
        match by_id.get(id) {
            Some(n) => format!("{} {C_BOLD}{}{C_RESET} {C_DIM}{}{C_RESET}", node_type_str(&n.node_type), n.name, node_loc(n)),
            None => format!("{C_DIM}{}{C_RESET}", id),
        }
    };
    for e in matched.iter().take(limit) {
        println!(
            "- {C_CYAN}{}{C_RESET}  {}  {C_DIM}→{C_RESET}  {}",
            edge_type_str(&e.edge_type),
            label(&e.source),
            label(&e.target)
        );
    }
    if matched.is_empty() {
        println!("No edges matched. Run {C_CYAN}ug graph_filter{C_RESET} with no arguments to see the available edge types.");
    }
}

fn run_graph_path(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_path_help();
        return;
    }
    let (load_args, pos) = analysis_input(args);
    if pos.len() < 2 {
        eprintln!("Usage: ug graph_path <source> <target> [--strict] [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, raw, _path) = load_agent_graph(&load_args);
    // The CLI resolves names/paths to ids before handing off; MCP and HTTP
    // pass ids directly.
    let source = resolve_node_ref(&graph, &pos[0]);
    let target = resolve_node_ref(&graph, &pos[1]);
    let strict = has_flag(args, "--strict");

    let result = agent_tools::shortest_path(&graph, &raw, &source, &target, strict);
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_shortest_path(&result, Render::Ansi, strict),
        "path result",
        true,
    );
}

/// Rows behind the centrality report: one per node, both scores joined.
fn centrality_rows<'a>(
    graph: &'a GraphData,
    centrality_json: &str,
    types: &[String],
    file_prefix: Option<&str>,
) -> Vec<(&'a GraphNode, f64, f64)> {
    let parsed: serde_json::Value =
        serde_json::from_str(centrality_json).unwrap_or_else(|_| serde_json::json!({}));
    let degree = parsed.get("degree_centrality").cloned().unwrap_or_else(|| serde_json::json!({}));
    let between = parsed
        .get("betweenness_centrality")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let score = |v: &serde_json::Value, id: &str| -> f64 {
        v.get(id).and_then(|x| x.as_f64()).unwrap_or(0.0)
    };
    graph
        .nodes
        .iter()
        .filter(|n| node_passes(n, types, file_prefix))
        .map(|n| (n, score(&degree, &n.id), score(&between, &n.id)))
        .collect()
}

fn run_graph_centrality(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_centrality_help();
        return;
    }
    let (load_args, _pos) = analysis_input(args);
    let types = type_filter(args, &["-t", "--type"]);
    let file_prefix = flag_value(args, &["-f", "--file"]);
    let top = limit_or(args, &["--top", "-l", "--limit"], 20);

    let (graph, raw, _path) = load_agent_graph(&load_args);
    let centrality = calculate_centrality(raw);

    // Raw output keeps the lib's shape so existing consumers of
    // analysis.json keep working.
    if emit_raw(args, &centrality, "centrality") {
        return;
    }

    let mut rows = centrality_rows(&graph, &centrality, &types, file_prefix.as_deref());

    println!("{C_BOLD}Centrality{C_RESET} — {} node(s) scored", rows.len());
    println!();
    println!("{C_BOLD}Top {} by degree{C_RESET} {C_DIM}(how connected){C_RESET}", top);
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (n, deg, _) in rows.iter().take(top) {
        println!("  {C_BOLD}{:.4}{C_RESET}  {}", deg, node_line(n));
    }
    println!();
    println!(
        "{C_BOLD}Top {} by betweenness{C_RESET} {C_DIM}(bridges between parts of the graph){C_RESET}",
        top
    );
    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    for (n, _, btw) in rows.iter().take(top) {
        println!("  {C_BOLD}{:.4}{C_RESET}  {}", btw, node_line(n));
    }
    println!();
    println!("{C_DIM}Next:{C_RESET} {C_CYAN}ug find_usages <id>{C_RESET} to see who depends on a hotspot.");
}

fn run_graph_cycles(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_cycles_help();
        return;
    }
    let (load_args, _pos) = analysis_input(args);
    let limit = limit_or(args, &["-l", "--limit"], 20);
    let min_len = limit_or(args, &["--min-len"], 0);
    let max_len = limit_or(args, &["--max-len"], usize::MAX);
    let file_prefix = flag_value(args, &["-f", "--file"]);

    let (graph, raw, _path) = load_agent_graph(&load_args);
    let by_id = by_id_map(&graph);
    let cycles_json = detect_cycles(raw);
    let parsed: serde_json::Value =
        serde_json::from_str(&cycles_json).unwrap_or_else(|_| serde_json::json!({}));
    let all: Vec<Vec<String>> = parsed
        .get("cycles")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();

    let cycles: Vec<&Vec<String>> = all
        .iter()
        .filter(|c| c.len() >= min_len && c.len() <= max_len)
        .filter(|c| match &file_prefix {
            None => true,
            Some(p) => c.iter().any(|id| {
                by_id
                    .get(id.as_str())
                    .and_then(|n| n.file.as_deref())
                    .map(|f| f.starts_with(p.as_str()))
                    .unwrap_or(false)
            }),
        })
        .collect();

    let json = serde_json::json!({
        "hasCycles": !cycles.is_empty(),
        "count": cycles.len(),
        "cycles": cycles,
    })
    .to_string();
    let consumed = emit_raw(args, &json, "cycle result");

    if !consumed {
        println!(
            "{C_BOLD}Cycles{C_RESET} — {} found{}",
            cycles.len(),
            if all.len() != cycles.len() {
                format!(" ({} before filters)", all.len())
            } else {
                String::new()
            }
        );
        println!();
        for (i, c) in cycles.iter().take(limit).enumerate() {
            println!("{C_BOLD}cycle {} ({} nodes){C_RESET}", i + 1, c.len());
            for id in c.iter() {
                match by_id.get(id.as_str()) {
                    Some(n) => println!("  ↻ {}", node_line(n)),
                    None => println!("  ↻ {C_DIM}{}{C_RESET}", id),
                }
            }
            println!();
        }
        if cycles.len() > limit {
            println!("{C_DIM}(+{} more — raise -l/--limit){C_RESET}", cycles.len() - limit);
        }
        if cycles.is_empty() {
            println!("{C_GREEN}✓{C_RESET} No cycles matched.");
        }
    }

    // CI use: non-zero exit when the graph has cycles.
    if has_flag(args, "--fail-on-cycle") && !cycles.is_empty() {
        std::process::exit(1);
    }
}

fn run_graph_analyze(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_analyze_help();
        return;
    }
    let (load_args, _pos) = analysis_input(args);
    let top = limit_or(args, &["--top", "-l", "--limit"], 10);
    let (graph, raw, graph_path) = load_agent_graph(&load_args);

    let centrality = calculate_centrality(raw.clone());
    let cycles = detect_cycles(raw);
    let cycle_count = serde_json::from_str::<serde_json::Value>(&cycles)
        .ok()
        .and_then(|v| v.get("cycles").and_then(|c| c.as_array()).map(|a| a.len()))
        .unwrap_or(0);

    // Written next to the graph by default so `ug serve` and the
    // visualization pick them up; `--no-write` makes this a pure report.
    if !has_flag(args, "--no-write") {
        let output_dir = flag_value(args, &["-o", "--output"]).unwrap_or_else(|| {
            graph_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .into_owned()
        });
        let _ = fs::create_dir_all(&output_dir);
        fs::write(format!("{}/analysis.json", output_dir), &centrality)
            .expect("Failed to write analysis.json");
        fs::write(format!("{}/cycles.json", output_dir), &cycles)
            .expect("Failed to write cycles.json");
        println!("{C_GREEN}✓{C_RESET} Wrote analysis to {C_BOLD}{}{C_RESET}:", output_dir);
        println!("  {C_CYAN}▸{C_RESET} analysis.json (centrality)");
        println!("  {C_CYAN}▸{C_RESET} cycles.json (cycle detection)");
        println!();
    }

    if has_flag(args, "--json") {
        println!(
            "{}",
            serde_json::json!({
                "graph": graph_path.to_string_lossy(),
                "nodes": graph.nodes.len(),
                "edges": graph.edges.len(),
                "cycles": cycle_count,
                "centrality": serde_json::from_str::<serde_json::Value>(&centrality).unwrap_or_default(),
            })
        );
        return;
    }

    let mut rows = centrality_rows(&graph, &centrality, &[], None);
    println!(
        "{C_BOLD}Graph analysis{C_RESET} — {} nodes, {} edges, {} cycle(s)",
        graph.nodes.len(),
        graph.edges.len(),
        cycle_count
    );
    println!("{C_DIM}graph: {}{C_RESET}", graph_path.display());
    println!();
    println!("{C_BOLD}Top {} by degree{C_RESET}", top);
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (n, deg, _) in rows.iter().take(top) {
        println!("  {C_BOLD}{:.4}{C_RESET}  {}", deg, node_line(n));
    }
    println!();
    println!("{C_BOLD}Top {} by betweenness{C_RESET}", top);
    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    for (n, _, btw) in rows.iter().take(top) {
        println!("  {C_BOLD}{:.4}{C_RESET}  {}", btw, node_line(n));
    }
    println!();
    println!("{C_DIM}Next:{C_RESET} {C_CYAN}ug graph_cycles{C_RESET} for the cycle detail · {C_CYAN}ug graph_centrality --top 50{C_RESET} for a longer ranking");
}

// ---------- Agent tools ----------
//
// The MCP server (node/cli.mjs) exposes five graph.json-backed tools that
// AI coding agents call to understand an indexed repo: find_symbols,
// file_outline, get_code, project_overview, graph_path. The commands
// below are those same tools callable by hand — same lookup logic over the
// same graph.json, no embeddings — so a human can run them to explore the
// repo the way an agent does, or to verify what an agent will see.

/// Flags-with-values shared by the agent-tool commands, so positional
/// arguments can be told apart from flag values.
const AGENT_VALUE_FLAGS: &[&str] = &[
    "-i",
    "--input",
    "-n",
    "--name",
    "-t",
    "--type",
    "--edge-type",
    "-f",
    "--file",
    "-l",
    "--limit",
    "-s",
    "--start",
    "-e",
    "--end",
    "-k",
    "--hops",
    "--max-chars",
];

/// Every non-flag positional, skipping flag/value pairs (multi-positional
/// sibling of `first_positional`).
fn positionals(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if value_flags.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            out.push(a.clone());
            i += 1;
        }
    }
    out
}

/// graph.json for the agent-tool commands: `-i/--input` wins, else the
/// `-n/--name` (or cwd-derived) project dir, else the most recently
/// updated project under ~/.ug — same fallback spirit as the db reads.
fn agent_graph_path(args: &[String]) -> PathBuf {
    if let Some(p) = flag_value(args, &["-i", "--input"]) {
        return PathBuf::from(p);
    }
    let p = project::project_dir(&project::resolve_project_name(args, ".")).join("graph.json");
    if p.exists() || flag_value(args, &["-n", "--name"]).is_some() {
        return p;
    }
    for (dir, _meta) in project::list_projects() {
        let candidate = dir.join("graph.json");
        if candidate.exists() {
            return candidate;
        }
    }
    p
}

fn load_agent_graph(args: &[String]) -> (GraphData, String, PathBuf) {
    let path = agent_graph_path(args);
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => {
            eprintln!(
                "graph.json not found at {} — run {C_CYAN}ug gen{C_RESET} for this project first.",
                path.display()
            );
            std::process::exit(1);
        }
    };
    match serde_json::from_str::<GraphData>(&raw) {
        Ok(graph) => (graph, raw, path),
        Err(e) => {
            eprintln!("Failed to parse {}: {}", path.display(), e);
            std::process::exit(1);
        }
    }
}

/// Repo root for reading source files: $UG_REPO_ROOT > project.json's
/// repoRoot (sibling of graph.json) > graph stats.repoRoot > cwd.
fn agent_repo_root(graph: &GraphData, graph_path: &Path) -> PathBuf {
    if let Ok(r) = std::env::var("UG_REPO_ROOT") {
        if !r.trim().is_empty() {
            return PathBuf::from(r);
        }
    }
    if let Some(dir) = graph_path.parent() {
        if let Some(meta) = project::read_meta(dir) {
            if !meta.repo_root.is_empty() {
                return PathBuf::from(meta.repo_root);
            }
        }
    }
    if let Some(stats) = &graph.stats {
        if !stats.repo_root.is_empty() {
            return PathBuf::from(&stats.repo_root);
        }
    }
    PathBuf::from(".")
}

fn print_find_symbols_help() {
    println!("  {C_CYAN}ug find_symbols{C_RESET}  {C_YELLOW}— exact-name symbol lookup (no embeddings){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug find_symbols <name-or-id>... [options]");
    println!();
    println!("  Accepts several names or nodeIds in one call (up to you; sections are separated) —");
    println!("  agents should batch related lookups instead of running the command repeatedly.");
    println!("  {C_CYAN}Direct nodeId lookup{C_RESET} (O(1)): if input contains ':' it's treated as a nodeId.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--node-type <type>{C_RESET}   Restrict to node type (repeatable; e.g. Function, Class, Interface)");
    println!("  {C_CYAN}--file-prefix <p>{C_RESET}    Only symbols under this file path prefix");
    println!("  {C_CYAN}-k, --limit <n>{C_RESET}      Max hits per query (default 20)");
    println!("  {C_CYAN}--include-docs{C_RESET}       Also match docstrings, not just names");
    println!("  {C_CYAN}-n, --name <project>{C_RESET} Project name (default: cwd basename)");
    println!("  {C_DIM}(-t/--type and -f/--file still parse as the old spellings){C_RESET}");
    println!();
    println!("{C_BOLD}Ranking:{C_RESET} exact > prefix > substring > docstring; ties go to the shorter name.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} resolveDb");
    println!("  {C_CYAN}ug find_symbols{C_RESET} loadConfig --node-type Function --file-prefix src/auth/");
    println!("  {C_CYAN}ug find_symbols{C_RESET} run_serve run_app run_gen   {C_YELLOW}# batch: three lookups, one call{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} 'function:src/auth.rs:42:login'  {C_YELLOW}# direct nodeId lookup (O(1)){C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} embedder --include-docs   {C_YELLOW}# what `ug graph_search` does{C_RESET}");
    println!();
    println!("  {C_DIM}`ug graph_search` is an alias for {C_RESET}{C_CYAN}find_symbols --include-docs{C_RESET}{C_DIM} — it was the same");
    println!("  scan, minus the ranking and batching.{C_RESET}");
}

fn print_file_outline_help() {
    println!("  {C_CYAN}ug file_outline{C_RESET}  {C_YELLOW}— list every indexed symbol in one file{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug file_outline <file-or-id>... [options]");
    println!();
    println!("  Accepts several file paths or File nodeIds in one call (up to you; sections are separated) —");
    println!("  agents should batch related lookups instead of running the command repeatedly.");
    println!("  {C_CYAN}Direct nodeId lookup{C_RESET} (O(1)): if input contains ':' it's treated as a nodeId.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} native/src/main.rs");
    println!("  {C_CYAN}ug file_outline{C_RESET} main.rs  {C_YELLOW}# unique basename works too{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} file:native/src/main.rs  {C_YELLOW}# File node ids work as-is{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} 'file:src/cli.mjs'  {C_YELLOW}# direct nodeId lookup (O(1)){C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} main.rs serve.rs cli.mjs   {C_YELLOW}# batch: outline several files at once{C_RESET}");
}

fn print_get_code_help() {
    println!("  {C_CYAN}ug get_code{C_RESET}  {C_YELLOW}— read full source for a node id or file/line range{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug get_code <node-id>... | -f|--file <file> [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-f, --file <file>{C_RESET}     Repo-relative file path (instead of node-id)");
    println!("  {C_CYAN}-s, --start <n>{C_RESET}      First line (1-based, with --file; default 1)");
    println!("  {C_CYAN}-e, --end <n>{C_RESET}        Last line inclusive (with --file; default EOF)");
    println!("  {C_CYAN}--max-chars <n>{C_RESET}      Character cap on output (default 20000)");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} \"function:native/src/main.rs:124:flag_value\"  {C_YELLOW}# id from find_symbols{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} <id1> <id2> <id3>   {C_YELLOW}# batch: several symbols in one call (--max-chars applies per symbol){C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} -f native/src/types.rs -s 180 -e 210");
    println!("  {C_CYAN}ug get_code{C_RESET} -f README.md  {C_YELLOW}# whole file{C_RESET}");
}

fn print_project_overview_help() {
    println!("  {C_CYAN}ug project_overview{C_RESET}  {C_YELLOW}— orient yourself in the codebase in one call{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug project_overview [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug project_overview{C_RESET}");
    println!();
    println!("Shows:");
    println!("  • Repo root and db location");
    println!("  • Node/edge counts by type");
    println!("  • Biggest files by symbol count");
    println!("  • Most depended-upon symbols (hotspots)");
}

/// Emit an agent-tool result: raw JSON when `--json`/`-o` was given,
/// otherwise the ANSI rendering. Exits non-zero when any item in the batch
/// failed, so a bad id in a script is still detectable.
fn emit_agent_result<T: serde::Serialize>(
    args: &[String],
    result: &T,
    render: impl FnOnce() -> String,
    label: &str,
    ok: bool,
) {
    let json = serde_json::to_string_pretty(result).unwrap_or_default();
    if !emit_raw(args, &json, label) {
        print!("{}", render());
    }
    if !ok {
        std::process::exit(1);
    }
}

/// Split bare positionals into (node ids, names/paths). The CLI takes
/// untagged arguments where MCP and HTTP have separate `node_id` / `name`
/// params, so it guesses using the indexer's id shape.
fn split_ids_and_names(pos: &[String]) -> (Vec<String>, Vec<String>) {
    pos.iter()
        .cloned()
        .partition(|s| looks_like_node_id(s))
}

fn run_find_symbols(args: &[String]) {
    run_find_symbols_with(args, false)
}

/// `ug graph_search` is `find_symbols` with docstring matching on — that was
/// the only behavioural difference between the two commands. It stays as a
/// separate entry point so the old invocation keeps its old default.
fn run_graph_search(args: &[String]) {
    // `--names-only` was graph_search's way of asking for name-only matching,
    // i.e. exactly what plain find_symbols does.
    run_find_symbols_with(args, !has_flag(args, "--names-only"))
}

fn run_find_symbols_with(args: &[String], include_docs: bool) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_find_symbols_help();
        return;
    }
    // Accept graph_search's legacy leading `<graph-file>` positional.
    let (load_args, queries) = analysis_input(args);
    if queries.is_empty() {
        eprintln!("Usage: ug find_symbols <name>... [--node-type <type>]... [--file-prefix <prefix>] [-k <n>] [--include-docs] [-n <project>]");
        std::process::exit(1);
    }
    let (node_id, name) = split_ids_and_names(&queries);
    let params = agent_tools::FindSymbolsParams {
        node_id,
        name,
        // `--node-type` is the canonical spelling; `-t/--type` still parses.
        node_types: multi_flag(args, &["--node-type", "-t", "--type"]),
        file_prefix: flag_value(args, &["--file-prefix", "-f", "--file"]),
        limit: flag_value(args, &["-k", "--limit", "-l"]).and_then(|s| s.parse().ok()),
        include_docs: include_docs || has_flag(args, "--include-docs"),
    };
    let (graph, _raw, _path) = load_agent_graph(&load_args);

    let result = agent_tools::find_symbols(&graph, &params);
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_find_symbols(&result, Render::Ansi),
        "find_symbols result",
        ok,
    );
}

fn run_file_outline(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_file_outline_help();
        return;
    }
    let files = positionals(args, AGENT_VALUE_FLAGS);
    if files.is_empty() {
        eprintln!("Usage: ug file_outline <file>... [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, _path) = load_agent_graph(args);

    // A `file:`-prefixed id is a File node id *and* a path — `file_outline`
    // resolves either, so both buckets end up in the same place.
    let (node_id, file) = files
        .into_iter()
        .partition(|s| looks_like_node_id(s) && !s.starts_with("file:"));
    let result = agent_tools::file_outline(&graph, &agent_tools::FileOutlineParams { node_id, file });
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_file_outline(&result, Render::Ansi),
        "file_outline result",
        ok,
    );
}

fn run_get_code(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_get_code_help();
        return;
    }
    let node_ids = positionals(args, AGENT_VALUE_FLAGS);
    let file_flag = flag_value(args, &["-f", "--file"]);
    if node_ids.is_empty() && file_flag.is_none() {
        eprintln!("Usage: ug get_code <node-id>... | -f|--file <file> [-s|--start <line>] [-e|--end <line>] [--max-chars <n>] [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);

    let params = agent_tools::GetCodeParams {
        node_id: node_ids,
        file: file_flag,
        start_line: flag_value(args, &["--start-line", "-s", "--start"])
            .and_then(|s| s.parse().ok()),
        end_line: flag_value(args, &["--end-line", "-e", "--end"]).and_then(|s| s.parse().ok()),
        max_chars: flag_value(args, &["--max-chars"]).and_then(|s| s.parse().ok()),
    };

    let result = agent_tools::get_code(&graph, &repo_root, &params);
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_get_code(&result, Render::Ansi),
        "get_code result",
        ok,
    );
}

fn run_project_overview(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_project_overview_help();
        return;
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);

    let result = agent_tools::project_overview(&graph, &repo_root, &graph_path);
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_project_overview(&result, Render::Ansi),
        "project_overview result",
        true,
    );
}

fn run_find_usages(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_find_usages_help();
        return;
    }
    let node_ids = positionals(args, AGENT_VALUE_FLAGS);
    if node_ids.is_empty() {
        eprintln!("Usage: ug find_usages <node-id>... [-k|--hops <n>] [-t|--edge-type <type>]... [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);

    let params = agent_tools::FindUsagesParams {
        node_id: node_ids,
        hops: flag_value(args, &["--hops", "-k"]).and_then(|s| s.parse().ok()),
        edge_types: multi_flag(args, &["--edge-type", "-t"]),
    };
    let result = agent_tools::find_usages(&graph, &repo_root, &params);
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_find_usages(&result, Render::Ansi),
        "find_usages result",
        ok,
    );
}

fn run_graph_schema(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_schema_help();
        return;
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);

    let result = agent_tools::graph_schema(&graph, &graph_path);
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_graph_schema(&result, Render::Ansi),
        "graph_schema result",
        true,
    );
}

fn print_find_usages_help() {
    println!("  {C_CYAN}ug find_usages{C_RESET}  {C_YELLOW}— who uses this symbol? (callers, importers, subclasses){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Follows edges {C_BOLD}inbound{C_RESET}: everything that calls / references / imports /");
    println!("  extends / implements the given node. The reverse of {C_CYAN}ug traverse{C_RESET}");
    println!("  (which walks outbound dependencies). Same logic as the MCP find_usages tool.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug find_usages <node-id>... [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-k, --hops <n>{C_RESET}         Transitive depth 1-3 (default 1 = direct users only)");
    println!("  {C_CYAN}-t, --edge-type <type>{C_RESET}  Restrict to edge type (repeatable; default: calls,");
    println!("                         references, imports, extends, implements — see {C_CYAN}ug graph_schema{C_RESET})");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}    Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug find_usages{C_RESET} \"function:native/src/main.rs:124:flag_value\"");
    println!("  {C_CYAN}ug find_usages{C_RESET} \"function:src/db.ts:42:connect\" -k 2 -t calls");
    println!("  {C_CYAN}ug find_usages{C_RESET} <id1> <id2>   {C_YELLOW}# batch: check several symbols before a refactor{C_RESET}");
}

fn print_graph_schema_help() {
    println!("  {C_CYAN}ug graph_schema{C_RESET}  {C_YELLOW}— node & edge types in this graph (metadata){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Lists the node types and edge types actually present in the project's");
    println!("  graph (with counts and what each edge type connects), plus the full");
    println!("  vocabulary indexers can emit. Check this before passing edge-type");
    println!("  filters to {C_CYAN}ug find_usages{C_RESET} / {C_CYAN}ug filter{C_RESET} — filtering on a type the graph");
    println!("  doesn't contain silently returns nothing.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_schema [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_schema{C_RESET}");
}

// full pipeline: index -> graph -> ingest -> search
fn run_gen(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_gen_help();
        return;
    }

    let start_total = std::time::Instant::now();

    let input = flag_value(args, &["-i", "--input"])
        .or_else(|| {
            first_positional(
                args,
                &[
                    "-i",
                    "--input",
                    "-c",
                    "--cache",
                    "-o",
                    "--output",
                    "-d",
                    "--db",
                    "-n",
                    "--name",
                    "--base-url",
                    "--api-key",
                    "--model",
                    "--embedding-dim",
                ],
            )
        })
        .unwrap_or_else(|| ".".to_string());
    let repo_root = input.clone();
    let cache = flag_value(args, &["-c", "--cache"]);
    let project_name = project::resolve_project_name(args, &input);
    let output_dir = flag_value(args, &["-o", "--output"])
        .unwrap_or_else(|| project::project_dir(&project_name).to_string_lossy().into_owned());
    let no_ingest = has_flag(args, "--no-ingest");
    let chain_serve = has_flag(args, "--serve");
    // Full precedence here: -d/--db flag → UG_DB_PATH env → <output-dir>/ugdb.
    // run_gen_ingest then pins the default OverGraph spec to this path.
    let db_path = flag_value(args, &["-d", "--db"])
        .or_else(|| std::env::var("UG_DB_PATH").ok())
        .unwrap_or_else(|| format!("{}/ugdb", output_dir));

    let pipeline_summary = if no_ingest {
        "index → graph → visualization"
    } else {
        "index → graph → visualization → ingest"
    };
    println!(
        "⚡ Full pipeline: {C_BOLD}{C_MAGENTA}{}{C_RESET}",
        pipeline_summary
    );

    let _ = fs::create_dir_all(&output_dir);

    let t0 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Indexing {C_YELLOW}{}{C_RESET}", input);
    let index_result = match cache {
        Some(c) => index_with_cache(input, c),
        None => index(input),
    };
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t0.elapsed()
    );

    let t1 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Building graph");
    let graph = build_graph(index_result.clone());
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t1.elapsed()
    );

    let (nodes_count, edges_count) = match serde_json::from_str::<serde_json::Value>(&graph) {
        Ok(v) => (
            v.get("nodes")
                .and_then(|n| n.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            v.get("edges")
                .and_then(|e| e.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        ),
        Err(_) => (0, 0),
    };
    println!("  nodes: {}", nodes_count);
    println!("  edges: {}", edges_count);

    let graph_path = format!("{}/graph.json", output_dir);
    fs::write(&graph_path, &graph).expect("Failed to write graph.json");
    fs::write(format!("{}/indexed-tree.json", output_dir), &index_result)
        .expect("Failed to write indexed-tree.json");

    let t2 = std::time::Instant::now();
    // index.html and ug-vis.bundle.js are embedded in `ug serve` (VIS_HTML /
    // VIS_BUNDLE) and served directly, so there's no need to write them here.
    println!("{C_CYAN}▸{C_RESET} Writing visualization README");
    fs::write(format!("{}/README.md", output_dir), VIS_MD).expect("Failed to write README.md");
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t2.elapsed()
    );

    let repo_root_abs = fs::canonicalize(&repo_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| repo_root.clone());
    let meta = project::ProjectMeta::new(&project_name, &repo_root_abs, nodes_count, edges_count);
    if let Err(e) = project::write_meta(Path::new(&output_dir), &meta) {
        eprintln!("⚠ failed to write project.json: {}", e);
    }

    println!("{C_BOLD}────────────────────────────────────────{C_RESET}");
    println!(
        "{C_GREEN}✓ Generated{C_RESET} project {C_BOLD}{}{C_RESET} in {C_BOLD}{}/{C_RESET}",
        project_name, output_dir
    );
    println!("  {C_GREEN}✓{C_RESET} graph.json");
    println!("  {C_GREEN}✓{C_RESET} indexed-tree.json");
    println!("  {C_GREEN}✓{C_RESET} README.md");
    println!("  {C_GREEN}✓{C_RESET} project.json");

    if no_ingest {
        println!("{C_YELLOW}⚠ Skipping db-ingest (--no-ingest){C_RESET}");
        if chain_serve {
            println!("Total time: {C_BOLD}{:?}{C_RESET}", start_total.elapsed());
            chain_to_serve(args, &graph_path, &db_path, true, &repo_root);
            return;
        }
        println!(
            "Run '{C_BOLD} ug serve -i {} {C_RESET}' and open {C_CYAN}http://127.0.0.1:8080{C_RESET}",
            graph_path
        );
        println!("Total time: {C_BOLD}{:?}{C_RESET}", start_total.elapsed());
        return;
    }

    println!();
    let t3 = std::time::Instant::now();
    println!(
        "{C_CYAN}▸{C_RESET} Ingesting graph data into DB {C_YELLOW}{}{C_RESET}",
        db_path
    );
    match run_gen_ingest(&graph, &db_path, args) {
        Ok((nodes_written, edges_written)) => {
            println!(
                "  {C_GREEN}✓ {} nodes, {} edges{C_RESET} embedded in {C_BOLD}{:?}{C_RESET}",
                nodes_written,
                edges_written,
                t3.elapsed()
            );
        }
        Err(e) => {
            eprintln!("⚠ db-ingest skipped — {}", e);
            eprintln!("  Re-run later once the embedding endpoint is up:");
            eprintln!("    ug ingest -i {} -o {}", graph_path, db_path);
        }
    }

    println!("────────────────────────────────────────");

    println!(
        "Run ' ug semantic_search \"hello\" -n {} ' to perform a semantic RAG query.",
        project_name
    );
    println!(
        "Run ' ug search \"hello\" -n {} ' to perform a hybrid graph + semantic RAG query.",
        project_name
    );
    println!("Total time: {:?}", start_total.elapsed());

    if chain_serve {
        chain_to_serve(args, &graph_path, &db_path, false, &repo_root);
    } else {
        println!(
            "Run '{C_BOLD} ug serve -i {} --repo-root {} {C_RESET}' and open {C_CYAN}http://127.0.0.1:8080{C_RESET} to view the graph.",
            graph_path,
            repo_root
        );
    }
}

/// Build a synthetic args vec for `serve` from the gen invocation and call
/// `serve::run_serve`. Inherits port/host/watch/repo-root and embedder flags
/// from the original invocation; sets `-i`/`-d` to the freshly generated
/// paths, and `--no-db` when the ingest step was skipped.
fn chain_to_serve(args: &[String], graph_path: &str, db_path: &str, no_db: bool, repo_root: &str) {
    let mut serve_args: Vec<String> = vec![
        "-i".to_string(),
        graph_path.to_string(),
        "-d".to_string(),
        db_path.to_string(),
        "--repo-root".to_string(),
        repo_root.to_string(),
    ];
    if no_db {
        serve_args.push("--no-db".to_string());
    }
    if has_flag(args, "--watch") {
        serve_args.push("--watch".to_string());
    }
    for &flag in &[
        "-p",
        "--port",
        "--host",
        "--repo-root",
        "--base-url",
        "--api-key",
        "--model",
        "--embedding-dim",
    ] {
        if let Some(v) = flag_value(args, &[flag]) {
            serve_args.push(flag.to_string());
            serve_args.push(v);
        }
    }
    println!();
    println!("────────────────────────────────────────");
    println!("Starting web server...");
    serve::run_serve(&serve_args);
}

// ingest graph data into one or more knowledge-store backends.
// Works against any `KnowledgeStore` impl (OverGraph, Neo4j, …).
async fn ingest_graph_with_progress(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<(usize, usize), String> {
    let nodes_count = graph.nodes.len();
    let edges_count = graph.edges.len();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let t0 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Building node texts ({})", nodes_count);
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let related = storage::collect_related_names(graph);
    let texts: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            storage::build_node_text(n, names)
        })
        .collect();
    println!(
        "\r{C_CYAN}▸{C_RESET} Building node texts: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t0.elapsed()
    );

    let t1 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Embedding nodes ({})", nodes_count);
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let total_nodes = texts.len();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(total_nodes);
    for (i, chunk) in texts.chunks(embedder.config().batch_size).enumerate() {
        let chunk_vec: Vec<String> = chunk.to_vec();
        let chunk_vectors = embedder
            .embed(&chunk_vec)
            .await
            .map_err(|e| format!("embedding failed: {}", e))?;
        vectors.extend(chunk_vectors);
        let processed = std::cmp::min((i + 1) * embedder.config().batch_size, total_nodes);
        let pct = processed as f32 / total_nodes as f32 * 100.0;
        print!(
            "\r{C_CYAN}▸{C_RESET} Embedding: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
            pct, processed, total_nodes
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!(
        "\r{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t1.elapsed()
    );

    if vectors.len() != graph.nodes.len() {
        return Err(format!(
            "embedder returned {} vectors for {} nodes",
            vectors.len(),
            graph.nodes.len()
        ));
    }

    let t2 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Writing nodes to Graph DB");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let node_rows: Vec<storage::NodeRow> = graph
        .nodes
        .iter()
        .zip(texts.into_iter())
        .zip(vectors.into_iter())
        .map(|((n, node_text), vector)| storage::NodeRow {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: format!("{:?}", n.node_type),
            description: n.docstring.clone().unwrap_or_default(),
            file: n.file.clone().unwrap_or_default(),
            start_line: n.start_line.unwrap_or(0),
            end_line: n.end_line.unwrap_or(0),
            last_update_at: now,
            node_text,
            vector,
        })
        .collect();

    let write_batch = 1000;
    let total = node_rows.len();
    for (i, batch) in node_rows.chunks(write_batch).enumerate() {
        store
            .upsert_nodes(batch)
            .await
            .map_err(|e| format!("upsert nodes: {}", e))?;
        let written = std::cmp::min((i + 1) * write_batch, total);
        let pct = written as f32 / total as f32 * 100.0;
        print!(
            "\r{C_CYAN}▸{C_RESET} Writing nodes: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
            pct, written, total
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!(
        "\r{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t2.elapsed()
    );

    let t3 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Writing edges to Graph DB");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let edge_rows: Vec<storage::EdgeRow> = graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            storage::EdgeRow {
                id,
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect();

    let total_edges = edge_rows.len();
    for (i, batch) in edge_rows.chunks(write_batch).enumerate() {
        store
            .upsert_edges(batch)
            .await
            .map_err(|e| format!("upsert edges: {}", e))?;
        let written = std::cmp::min((i + 1) * write_batch, total_edges);
        let pct = written as f32 / total_edges as f32 * 100.0;
        print!(
            "\r{C_CYAN}▸{C_RESET} Writing edges: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
            pct, written, total_edges
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!(
        "\r{C_CYAN}▸{C_RESET} Writing edges: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t3.elapsed()
    );

    Ok((nodes_count, edges_count))
}

fn run_gen_ingest(
    graph_json: &str,
    db_path: &str,
    args: &[String],
) -> Result<(usize, usize), String> {
    let graph: GraphData =
        serde_json::from_str(graph_json).map_err(|e| format!("parse graph: {}", e))?;
    let mut embedder = embedder_from_args(args);
    let dim_was_explicit = flag_value(args, &["--embedding-dim"]).is_some();
    let rt = tokio_runtime();
    rt.block_on(async {
        if !dim_was_explicit {
            let probed = embedder
                .probe_dim()
                .await
                .map_err(|e| format!("embedder dim probe: {}", e))?;
            if probed != embedder.config().dim {
                embedder.set_dim(probed);
            }
        }
        let dim = embedder.config().dim as u32;
        // `ug gen` accepts the same --dest / --neo4j-* flags as `ug
        // ingest`. When --dest is omitted we keep the OverGraph-only
        // behavior pointed at `db_path`.
        let mut specs = store_specs_from_args(args, dim);
        // gen already resolved the db path with full precedence
        // (-d/--db → UG_DB_PATH → <output-dir>/ugdb), so pin the
        // OverGraph-only default spec to it.
        if specs.len() == 1 {
            if let StoreSpec::Overgraph {
                path,
                embedding_dim: _,
            } = &mut specs[0]
            {
                *path = PathBuf::from(db_path);
            }
        }
        announce_destinations(&specs);
        ingest_with_specs(&specs, &embedder, &graph).await
    })
}

/// Open every spec, then dispatch to the right ingest path:
/// single-spec → progress-bar single ingest; multi-spec → fan-out
/// ingest (no per-store progress, but a one-line summary per backend).
async fn ingest_with_specs(
    specs: &[StoreSpec],
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<(usize, usize), String> {
    let mut stores: Vec<Box<dyn KnowledgeStore>> = Vec::with_capacity(specs.len());
    for spec in specs {
        let store = open_store(spec)
            .await
            .map_err(|e| format!("open {} store: {}", spec.name(), e))?;
        stores.push(store);
    }
    if stores.len() == 1 {
        let store = stores.into_iter().next().unwrap();
        ingest_graph_with_progress(store.as_ref(), embedder, graph).await
    } else {
        let set = StoreSet::new(stores);
        set.validate_dims().map_err(|e| format!("dim mismatch across destinations: {}", e))?;
        ingest_graph_multi_with_progress(&set, embedder, graph).await
    }
}

/// Multi-destination ingest with a single progress line per stage
/// (text-build, embed, write) — per-backend progress isn't useful when
/// fan-out is parallel.
async fn ingest_graph_multi_with_progress(
    set: &StoreSet,
    embedder: &Embedder,
    graph: &GraphData,
) -> Result<(usize, usize), String> {
    use storage::{collect_related_names, build_node_text};

    let nodes_count = graph.nodes.len();
    let edges_count = graph.edges.len();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let t0 = std::time::Instant::now();
    let related = collect_related_names(graph);
    let texts: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| {
            let names = related.get(&n.id).map(|v| v.as_slice()).unwrap_or(&[][..]);
            build_node_text(n, names)
        })
        .collect();
    println!(
        "{C_CYAN}▸{C_RESET} Building node texts: {C_GREEN}done{C_RESET} ({}) in {C_BOLD}{:?}{C_RESET}",
        nodes_count,
        t0.elapsed()
    );

    let t1 = std::time::Instant::now();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(nodes_count);
    for chunk in texts.chunks(embedder.config().batch_size) {
        let chunk_vec: Vec<String> = chunk.to_vec();
        let chunk_vectors = embedder
            .embed(&chunk_vec)
            .await
            .map_err(|e| format!("embedding failed: {}", e))?;
        vectors.extend(chunk_vectors);
    }
    println!(
        "{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t1.elapsed()
    );

    if vectors.len() != graph.nodes.len() {
        return Err(format!(
            "embedder returned {} vectors for {} nodes",
            vectors.len(),
            graph.nodes.len()
        ));
    }

    let node_rows: Vec<storage::NodeRow> = graph
        .nodes
        .iter()
        .zip(texts.into_iter())
        .zip(vectors.into_iter())
        .map(|((n, node_text), vector)| storage::NodeRow {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: format!("{:?}", n.node_type),
            description: n.docstring.clone().unwrap_or_default(),
            file: n.file.clone().unwrap_or_default(),
            start_line: n.start_line.unwrap_or(0),
            end_line: n.end_line.unwrap_or(0),
            last_update_at: now,
            node_text,
            vector,
        })
        .collect();
    let edge_rows: Vec<storage::EdgeRow> = graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            storage::EdgeRow {
                id,
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect();

    let t2 = std::time::Instant::now();
    set.upsert_nodes(&node_rows)
        .await
        .map_err(|e| format!("upsert nodes (fan-out): {}", e))?;
    println!(
        "{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}done{C_RESET} (×{} backends) in {C_BOLD}{:?}{C_RESET}",
        set.len(),
        t2.elapsed()
    );

    let t3 = std::time::Instant::now();
    set.upsert_edges(&edge_rows)
        .await
        .map_err(|e| format!("upsert edges (fan-out): {}", e))?;
    println!(
        "{C_CYAN}▸{C_RESET} Writing edges: {C_GREEN}done{C_RESET} (×{} backends) in {C_BOLD}{:?}{C_RESET}",
        set.len(),
        t3.elapsed()
    );

    Ok((nodes_count, edges_count))
}

/// `ug list` — enumerate project data dirs under `~/.ug` (or `$UG_HOME`).
fn run_list(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_list_help();
        return;
    }
    let projects = project::list_projects();
    let root = project::ug_home();
    if projects.is_empty() {
        println!(
            "No projects found in {C_BOLD}{}{C_RESET}. Run {C_CYAN}ug gen{C_RESET} in a repo to create one.",
            root.display()
        );
        return;
    }
    let cwd_name = project::derive_project_name(".");
    println!(
        "{C_BOLD}Projects in {}{C_RESET} ({}):\n",
        root.display(),
        projects.len()
    );
    println!(
        "  {C_BOLD}{:<24} {:>8} {:>8}  {:<19}  {}{C_RESET}",
        "NAME", "NODES", "EDGES", "UPDATED", "REPO"
    );
    for (_dir, meta) in &projects {
        let marker = if meta.name == cwd_name { "*" } else { " " };
        let updated = format_epoch(meta.updated_at);
        println!(
            "{C_GREEN}{}{C_RESET} {C_CYAN}{:<24}{C_RESET} {:>8} {:>8}  {:<19}  {}",
            marker, meta.name, meta.nodes, meta.edges, updated, meta.repo_root
        );
    }
    println!("\n{C_BOLD}*{C_RESET} matches the current directory. Serve them with {C_CYAN}ug serve{C_RESET}.");
}

/// `ug rm [<project>]` — delete a project's data directory under
/// `~/.ug` (or `$UG_HOME`). Prompts for confirmation unless `-f/--force`
/// (or `-y/--yes`) is given; an empty/EOF answer (e.g. non-interactive
/// stdin) is treated as "no" so this fails closed by default.
fn run_rm(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug rm{C_RESET} [<project>] [-n, --name <project>] [-f, --force | -y, --yes]");
        println!("  Delete a project's data directory under ~/.ug (or $UG_HOME).");
        println!("  Project defaults to the current directory's basename if omitted.");
        return;
    }

    let value_flags = ["-n", "--name"];
    let name_flag = flag_value(args, &["-n", "--name"]);
    let positional = first_positional(args, &value_flags);
    let project_name = name_flag
        .or(positional)
        .map(|n| project::sanitize_name(&n))
        .unwrap_or_else(|| project::derive_project_name("."));

    let dir = project::project_dir(&project_name);
    if !dir.exists() {
        eprintln!(
            "No project named {C_BOLD}{}{C_RESET} found at {}.",
            project_name,
            dir.display()
        );
        eprintln!("Run {C_CYAN}ug list{C_RESET} to see available projects.");
        std::process::exit(1);
    }

    println!("About to remove project {C_BOLD}{}{C_RESET}", project_name);
    println!("  path:  {}", dir.display());
    if let Some(meta) = project::read_meta(&dir) {
        println!("  repo:  {}", meta.repo_root);
        println!("  nodes: {}, edges: {}", meta.nodes, meta.edges);
    }

    let force = has_flag(args, "-f")
        || has_flag(args, "--force")
        || has_flag(args, "-y")
        || has_flag(args, "--yes");
    if !force {
        use std::io::Write;
        print!("Delete this project directory? This cannot be undone. [y/N] ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        let answer = input.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return;
        }
    }

    match project::remove_project_dir(&dir) {
        Ok(()) => println!(
            "{C_GREEN}✓{C_RESET} Removed {C_BOLD}{}{C_RESET} ({})",
            project_name,
            dir.display()
        ),
        Err(e) => {
            eprintln!("Failed to remove {}: {}", dir.display(), e);
            std::process::exit(1);
        }
    }
}

/// `ug uninstall` — deletes every indexed project under `ug_home()` (all
/// of `~/.ug` / `$UG_HOME`) and then removes the standalone install
/// itself: the `~/.local/share/ultragraph` dir the prebuilt installer
/// (see README's Install section, `curl ... install.sh`) unpacks into,
/// and the `~/.local/bin/ug` symlink it points at. The symlink is only
/// touched when it actually resolves into that install dir — never a
/// same-named file the user happens to have on their own PATH. A
/// from-source checkout has neither of those, so that half is silently
/// skipped and only project data is removed. Prompts for confirmation
/// unless `-f/--force` (or `-y/--yes`); empty/EOF input (e.g.
/// non-interactive stdin) reads as "no", same fail-closed default as `ug
/// rm`.
fn run_uninstall(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug uninstall{C_RESET} [-f, --force | -y, --yes]");
        println!(
            "  Delete ALL indexed projects under {} and uninstall ug itself",
            project::ug_home().display()
        );
        println!("  (the standalone install dir + `ug` symlink, if this is a prebuilt install).");
        return;
    }

    let home = dirs::home_dir();
    let install_dir = home
        .as_ref()
        .map(|h| h.join(".local").join("share").join("ultragraph"));
    let bin_symlink = home.as_ref().map(|h| h.join(".local").join("bin").join("ug"));

    let ug_home_dir = project::ug_home();
    let projects = project::list_projects();
    let install_dir_exists = install_dir.as_ref().is_some_and(|d| d.exists());
    let bin_symlink_is_ours = bin_symlink.as_ref().is_some_and(|p| {
        p.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
            && std::fs::read_link(p)
                .ok()
                .and_then(|target| install_dir.as_ref().map(|d| target.starts_with(d)))
                .unwrap_or(false)
    });

    println!("{C_BOLD}This will:{C_RESET}");
    if ug_home_dir.exists() {
        println!(
            "  - Delete {} indexed project(s) under {}",
            projects.len(),
            ug_home_dir.display()
        );
    }
    if install_dir_exists {
        println!(
            "  - Remove the installed app at {}",
            install_dir.as_ref().unwrap().display()
        );
    }
    if bin_symlink_is_ours {
        println!(
            "  - Remove the `ug` symlink at {}",
            bin_symlink.as_ref().unwrap().display()
        );
    }
    if !install_dir_exists && !bin_symlink_is_ours {
        println!(
            "  {C_YELLOW}(no standalone install found — looks like a from-source checkout, so only project data will be removed){C_RESET}"
        );
    }
    println!();
    println!("{C_BOLD}{C_YELLOW}This cannot be undone.{C_RESET}");

    let force = has_flag(args, "-f")
        || has_flag(args, "--force")
        || has_flag(args, "-y")
        || has_flag(args, "--yes");
    if !force {
        use std::io::Write;
        print!("Type 'yes' to confirm: ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        let answer = input.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return;
        }
    }

    if ug_home_dir.exists() {
        match std::fs::remove_dir_all(&ug_home_dir) {
            Ok(()) => println!(
                "{C_GREEN}✓{C_RESET} Removed project data at {}",
                ug_home_dir.display()
            ),
            Err(e) => eprintln!("Failed to remove {}: {}", ug_home_dir.display(), e),
        }
    }

    if bin_symlink_is_ours {
        let p = bin_symlink.unwrap();
        match std::fs::remove_file(&p) {
            Ok(()) => println!("{C_GREEN}✓{C_RESET} Removed symlink {}", p.display()),
            Err(e) => eprintln!("Failed to remove {}: {}", p.display(), e),
        }
    }

    if install_dir_exists {
        let d = install_dir.unwrap();
        match std::fs::remove_dir_all(&d) {
            Ok(()) => println!("{C_GREEN}✓{C_RESET} Removed {}", d.display()),
            Err(e) => eprintln!("Failed to remove {}: {}", d.display(), e),
        }
    }

    println!();
    println!("{C_BOLD}ug has been uninstalled.{C_RESET} Thanks for trying UltraGraph.");
}

/// GitHub repo the prebuilt release archives are published to. Must match
/// `REPO` in install.sh — `ug upgrade` is that script's self-update twin.
const UPGRADE_REPO: &str = "shoocstorm/ug";

/// Leading numeric triple of a `v1.2.3`-style tag; non-digit suffixes
/// (`-rc1`) and missing parts read as 0, so `v0.2` == `0.2.0`.
fn version_triple(v: &str) -> (u64, u64, u64) {
    let mut nums = v.trim().trim_start_matches('v').splitn(3, '.').map(|part| {
        part.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
    )
}

/// `ug upgrade` — self-update the standalone prebuilt install from the
/// latest GitHub release (or a pinned `vX.Y.Z`). Mirrors install.sh: it
/// looks up the release via the GitHub API, downloads the matching
/// `ultragraph-<os-arch>.tar.gz` asset, unpacks it into
/// `$UG_INSTALL_ROOT/.ug` (default `~/.local/share/ultragraph/.ug`), and
/// refreshes the `$UG_BIN_DIR/ug` symlink. The new tree is staged next to
/// the live one and swapped in with two renames, so a failed download or
/// extraction never leaves a half-written install — and replacing the
/// directory the running binary lives in is safe on Unix (the process
/// keeps its inode). From-source checkouts are refused unless `--force`,
/// which (re)installs the release to the standard location anyway.
fn run_upgrade(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug upgrade{C_RESET} [<version>] [--check] [-f, --force]");
        println!("  Check GitHub for a newer release and self-update the standalone install.");
        println!();
        println!("  {C_CYAN}<version>{C_RESET}    Pin a specific release tag (e.g. v0.2.0) instead of latest");
        println!("  {C_CYAN}--check{C_RESET}      Only report whether an update is available; install nothing");
        println!("  {C_CYAN}-f, --force{C_RESET}  Reinstall even when already up to date, and allow installing");
        println!("               the prebuilt release from a from-source checkout");
        return;
    }

    let check_only = has_flag(args, "--check");
    let force = has_flag(args, "-f") || has_flag(args, "--force");
    let pinned = first_positional(args, &[]);

    fn die(msg: &str) -> ! {
        eprintln!("{C_YELLOW}error:{C_RESET} {msg}");
        std::process::exit(1);
    }

    // Same OS/arch → asset mapping as install.sh. Windows ships a zip we
    // don't self-extract, so it gets the manual-download pointer too.
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("linux", "x86_64") => "linux-x64",
        (os, arch) => {
            eprintln!("`ug upgrade` has no self-installable archive for {os}/{arch}.");
            eprintln!(
                "Download a release manually: {C_CYAN}https://github.com/{UPGRADE_REPO}/releases/latest{C_RESET}"
            );
            std::process::exit(1);
        }
    };
    let archive = format!("ultragraph-{asset}.tar.gz");

    let current = env!("CARGO_PKG_VERSION");
    let release_url = match &pinned {
        Some(v) => {
            let tag = if v.starts_with('v') { v.clone() } else { format!("v{v}") };
            format!("https://api.github.com/repos/{UPGRADE_REPO}/releases/tags/{tag}")
        }
        None => format!("https://api.github.com/repos/{UPGRADE_REPO}/releases/latest"),
    };

    println!(
        "{C_CYAN}▸{C_RESET} Current version {C_BOLD}v{current}{C_RESET} — checking {}...",
        pinned.as_deref().unwrap_or("latest release")
    );

    let rt = tokio_runtime();
    let client = reqwest::Client::builder()
        .user_agent(concat!("ug/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|e| die(&format!("failed to build HTTP client: {e}")));

    let release: serde_json::Value = rt
        .block_on(async {
            client
                .get(&release_url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
        })
        .unwrap_or_else(|e: reqwest::Error| {
            die(&format!("release lookup failed ({release_url}): {e}"))
        });

    let tag = release["tag_name"].as_str().unwrap_or_default().to_string();
    if tag.is_empty() {
        die("release has no tag_name — unexpected GitHub API response");
    }
    let newer = version_triple(&tag) > version_triple(current);

    if check_only {
        if newer {
            println!(
                "{C_GREEN}▸{C_RESET} Update available: {C_BOLD}v{current}{C_RESET} → {C_BOLD}{tag}{C_RESET}"
            );
            println!("Run {C_CYAN}ug upgrade{C_RESET} to install it.");
        } else {
            println!("{C_GREEN}✓{C_RESET} Already up to date (v{current} is the latest release).");
        }
        return;
    }
    if !newer && pinned.is_none() && !force {
        println!("{C_GREEN}✓{C_RESET} Already up to date (v{current} is the latest release).");
        println!("{C_DIM}Pass --force to reinstall anyway.{C_RESET}");
        return;
    }

    let home = dirs::home_dir()
        .unwrap_or_else(|| die("cannot determine your home directory"));
    let install_root = std::env::var("UG_INSTALL_ROOT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("share").join("ultragraph"));
    let bin_dir = std::env::var("UG_BIN_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("bin"));
    let dot_ug = install_root.join(".ug");

    // Refuse to "upgrade" a from-source checkout: replacing
    // ~/.local/share/ultragraph wouldn't touch the binary being run, which
    // would just look like the upgrade silently didn't take.
    let exe = std::env::current_exe()
        .ok()
        .map(|e| fs::canonicalize(&e).unwrap_or(e));
    let canon_dot_ug = fs::canonicalize(&dot_ug).unwrap_or_else(|_| dot_ug.clone());
    let is_prebuilt = exe.as_ref().is_some_and(|e| e.starts_with(&canon_dot_ug));
    if !is_prebuilt && !force {
        eprintln!(
            "{C_YELLOW}This `ug` is not the prebuilt install{C_RESET} (running from {}).",
            exe.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_else(|| "<unknown>".into())
        );
        eprintln!(
            "`ug upgrade` manages the standalone install at {} — for a source checkout, `git pull` and rebuild instead.",
            dot_ug.display()
        );
        eprintln!(
            "Re-run with {C_CYAN}--force{C_RESET} to install {tag} to the standard location anyway."
        );
        std::process::exit(1);
    }

    let download_url = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(archive.as_str()))
        .and_then(|a| a["browser_download_url"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            die(&format!("no {archive} asset found on release {tag} — has it finished building?"))
        });

    println!("{C_CYAN}▸{C_RESET} Downloading {C_BOLD}{tag}{C_RESET} ({archive})...");
    let bytes = rt
        .block_on(async {
            use futures::StreamExt;
            use std::io::{IsTerminal, Write};
            let resp = client.get(&download_url).send().await?.error_for_status()?;
            let total = resp.content_length();
            let mut buf: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
            let mut stream = resp.bytes_stream();
            // Redraw only on whole-percent changes, and only on a real
            // terminal — piped output would otherwise collect every `\r`
            // frame as its own line.
            let tty = std::io::stdout().is_terminal();
            let mut last_pct: u64 = u64::MAX;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                buf.extend_from_slice(&chunk);
                if let Some(t) = total.filter(|&t| t > 0) {
                    let pct = buf.len() as u64 * 100 / t;
                    if tty && pct != last_pct {
                        last_pct = pct;
                        print!(
                            "\r  {:.1} / {:.1} MB ({pct}%)",
                            buf.len() as f64 / 1e6,
                            t as f64 / 1e6
                        );
                        let _ = std::io::stdout().flush();
                    }
                }
            }
            if tty && last_pct != u64::MAX {
                println!();
            } else {
                println!("  {:.1} MB downloaded", buf.len() as f64 / 1e6);
            }
            Ok::<_, reqwest::Error>(buf)
        })
        .unwrap_or_else(|e| die(&format!("download failed: {e}")));

    let pid = std::process::id();
    let tmp_archive = std::env::temp_dir().join(format!("ug-upgrade-{pid}.tar.gz"));
    fs::write(&tmp_archive, &bytes)
        .unwrap_or_else(|e| die(&format!("failed to write {}: {e}", tmp_archive.display())));
    drop(bytes);

    // Stage → swap: extract beside the live tree, then two renames. The
    // stage/backup dirs are pid-suffixed so a concurrent or crashed
    // upgrade can't collide with this one.
    let stage = install_root.join(format!(".ug.new-{pid}"));
    let backup = install_root.join(format!(".ug.old-{pid}"));
    let cleanup = |paths: &[&Path]| {
        for p in paths {
            if p.exists() {
                let _ = fs::remove_dir_all(p);
                let _ = fs::remove_file(p);
            }
        }
    };

    println!("{C_CYAN}▸{C_RESET} Installing to {}...", dot_ug.display());
    let _ = fs::remove_dir_all(&stage);
    if let Err(e) = fs::create_dir_all(&stage) {
        cleanup(&[&tmp_archive]);
        die(&format!("failed to create {}: {e}", stage.display()));
    }
    let tar_ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tmp_archive)
        .arg("-C")
        .arg(&stage)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    cleanup(&[&tmp_archive]);
    if !tar_ok || !stage.join("ug").exists() {
        cleanup(&[&stage]);
        die("failed to extract the release archive (is `tar` on your PATH?)");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for bin in ["ug", "ug-app"] {
            let p = stage.join(bin);
            if p.exists() {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o755));
            }
        }
    }

    if dot_ug.exists() {
        if let Err(e) = fs::rename(&dot_ug, &backup) {
            cleanup(&[&stage]);
            die(&format!("failed to move the old install aside: {e}"));
        }
    }
    if let Err(e) = fs::rename(&stage, &dot_ug) {
        // Put the old tree back so the existing install keeps working.
        if backup.exists() {
            let _ = fs::rename(&backup, &dot_ug);
        }
        cleanup(&[&stage]);
        die(&format!("failed to activate the new install: {e}"));
    }
    cleanup(&[&backup]);

    // Refresh the launcher symlink (`ln -sf` in install.sh). A regular
    // file at that path is the user's own — warn, never clobber it.
    #[cfg(unix)]
    {
        let link = bin_dir.join("ug");
        let link_is_file = link
            .symlink_metadata()
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        if link_is_file {
            eprintln!(
                "{C_YELLOW}⚠{C_RESET} {} exists and is a regular file — leaving it alone. The new binary is at {}",
                link.display(),
                dot_ug.join("ug").display()
            );
        } else {
            let _ = fs::create_dir_all(&bin_dir);
            if link.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&link);
            }
            if let Err(e) = std::os::unix::fs::symlink(dot_ug.join("ug"), &link) {
                eprintln!(
                    "{C_YELLOW}⚠{C_RESET} could not refresh symlink {}: {e}",
                    link.display()
                );
            }
        }
    }

    let confirmed = std::process::Command::new(dot_ug.join("ug"))
        .arg("-v")
        .env("UG_QUIET_LOGO", "1")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    println!();
    println!("{C_GREEN}✓{C_RESET} {C_BOLD}Upgraded to {tag}{C_RESET}");
    if let Some(v) = confirmed {
        println!("  {C_DIM}{v}{C_RESET}");
    }
    println!("  {C_DIM}(restart any running `ug serve` / MCP server to pick it up){C_RESET}");
}

/// Find a `node` executable when a bare PATH lookup fails. `ug mcp` is the
/// command MCP clients launch, and GUI clients (Claude Desktop, etc.) spawn
/// servers with a minimal PATH that usually misses Homebrew/nvm/volta
/// installs — so check the usual locations before giving up.
fn find_node_fallback() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if cfg!(unix) {
        candidates.push("/opt/homebrew/bin/node".into());
        candidates.push("/usr/local/bin/node".into());
        candidates.push("/usr/bin/node".into());
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".volta").join("bin").join("node"));
        candidates.push(home.join(".fnm").join("aliases").join("default").join("bin").join("node"));
        // nvm keeps one dir per version; the lexicographic max is good
        // enough for modern versions (v18/v20/v22 sort correctly).
        if let Ok(entries) = std::fs::read_dir(home.join(".nvm").join("versions").join("node")) {
            if let Some(latest) = entries.flatten().map(|e| e.path()).max() {
                candidates.push(latest.join("bin").join("node"));
            }
        }
    }
    #[cfg(windows)]
    {
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            candidates.push(std::path::PathBuf::from(pf).join("nodejs").join("node.exe"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// `ug mcp [install|uninstall <target>]` — there's no separate Rust MCP
/// implementation, so this forwards straight to the bundled `cli.mjs`
/// (sitting next to this binary in `.ug/` — see scripts/copy-wrappers.mjs).
/// Bare `ug mcp` becomes a long-running stdio JSON-RPC server: stdio is
/// inherited as-is so it can be wired into an MCP client directly, and the
/// startup logo is suppressed for that mode (see `is_mcp_server_mode` in
/// `main`). Client configs point at this command (not node+cli.mjs) exactly
/// so this wrapper can absorb environment problems like node missing from a
/// GUI client's minimal PATH.
fn run_mcp(args: &[String]) {
    let exe_path = std::env::current_exe()
        .ok()
        .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe));
    let cli_path = exe_path
        .as_ref()
        .and_then(|exe| exe.parent().map(|d| d.join("cli.mjs")));

    let cli_path = match cli_path {
        Some(p) if p.exists() => p,
        _ => {
            eprintln!("Couldn't find cli.mjs next to the `ug` binary — the MCP server/installer is Node-only.");
            eprintln!(
                "Run it directly instead: {C_CYAN}node <install-dir>/cli.mjs mcp {}{C_RESET}",
                args.join(" ")
            );
            std::process::exit(1);
        }
    };

    // UG_BIN tells cli.mjs where this binary lives, so `mcp install` writes
    // client configs that launch `ug mcp` directly instead of node+cli.mjs.
    let spawn = |node: &std::ffi::OsStr| {
        let mut cmd = std::process::Command::new(node);
        cmd.arg(&cli_path).arg("mcp").args(args);
        if let Some(exe) = &exe_path {
            cmd.env("UG_BIN", exe);
        }
        cmd.status()
    };

    let mut status = spawn(std::ffi::OsStr::new("node"));
    if matches!(&status, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
        match find_node_fallback() {
            Some(node) => status = spawn(node.as_os_str()),
            None => {
                eprintln!("`node` was not found on PATH or in the usual install locations (Homebrew, /usr/local, nvm, volta).");
                eprintln!("The MCP server runs on Node.js 20+ — install it, then retry.");
                std::process::exit(1);
            }
        }
    }

    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Failed to launch `node {}`: {}", cli_path.display(), e);
            std::process::exit(1);
        }
    }
}

/// `ug app` — launches the native desktop shell (Tauri) for the vis
/// layer. The webview just points at a `ug serve` URL, so this starts a
/// server first (in a background thread, in-process — no extra child
/// for it) and waits for it to answer before handing its URL to the
/// `ug-app` binary (built alongside `ug` — see native/src/bin/ug_app.rs).
/// All `ug serve` flags (`-i`, `--project`, `-p`, `--host`, etc.) pass
/// through untouched.
fn run_app(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_app_help();
        return;
    }

    let port: u16 = flag_value(args, &["-p", "--port"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let host = flag_value_or(args, &["--host"], "127.0.0.1");

    // `current_exe()` can return the invoked path rather than the resolved
    // one when `ug` is reached through a symlink (e.g. the installer's
    // `~/.local/bin/ug` -> `~/.local/share/ultragraph/.ug/ug`), which would
    // make us look for `ug-app` next to the symlink instead of next to the
    // real binary. Canonicalize first so we always look in the right dir.
    let app_path = std::env::current_exe().ok().and_then(|exe| {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        exe.parent().map(|d| {
            d.join(if cfg!(windows) { "ug-app.exe" } else { "ug-app" })
        })
    });
    let app_path = match app_path {
        Some(p) if p.exists() => p,
        _ => {
            eprintln!("Couldn't find the `ug-app` binary next to `ug` — the desktop shell wasn't bundled with this build.");
            eprintln!("Falling back to the browser instead: {C_CYAN}ug serve{C_RESET}, then open http://{host}:{port}");
            std::process::exit(1);
        }
    };

    let serve_args = args.to_vec();
    std::thread::spawn(move || {
        serve::run_serve(&serve_args);
    });

    let addr = format!("{host}:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if std::net::TcpStream::connect(&addr).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("Timed out waiting for `ug serve` to come up on {addr} — starting the app window anyway.");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let url = format!("http://{host}:{port}");
    println!("{C_CYAN}▸{C_RESET} Launching desktop app against {C_BOLD}{url}{C_RESET}");

    let status = std::process::Command::new(&app_path)
        .env("UG_APP_URL", &url)
        .status();

    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("Failed to launch {}: {}", app_path.display(), e);
            std::process::exit(1);
        }
    }
}

fn print_app_help() {
    println!("  {C_CYAN}ug app{C_RESET}  {C_YELLOW}— open the native desktop shell for the vis layer{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug app [serve options]");
    println!();
    println!("  Starts {C_CYAN}ug serve{C_RESET} (in-process, same as running it directly) and opens");
    println!("  a native window (Tauri) pointed at it — an alternative to opening");
    println!("  http://localhost:8080 in a browser tab. Accepts every {C_CYAN}ug serve{C_RESET}");
    println!("  flag (-i, --project, -p/--port, --host, --watch, --no-db, ...); see");
    println!("  {C_CYAN}ug serve -h{C_RESET} for the full list.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug app{C_RESET}                       {C_YELLOW}# all projects under ~/.ug{C_RESET}");
    println!("  {C_CYAN}ug app{C_RESET} --project myrepo -p 9000");
}

/// `ug config` — view and persist settings in `$UG_HOME/config.json`.
/// Persisted values sit below CLI flags and env vars in precedence, so
/// nothing here can silently hijack an explicit invocation; the
/// resolver prints a notice whenever a flag/env var overrides a saved
/// value.
fn run_config(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_config_help();
        return;
    }
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => run_config_list(),
        "path" => println!("{}", config::config_path().display()),
        "get" => {
            let Some(name) = args.get(1) else {
                eprintln!("Usage: ug config get <key>");
                std::process::exit(1);
            };
            let key = config_key_or_exit(name);
            match config::get(key.name) {
                Some(v) => println!("{}", v),
                None => {
                    eprintln!("{} is not set (run `ug config set {} <value>`)", key.name, key.name);
                    std::process::exit(1);
                }
            }
        }
        "set" => {
            let (Some(name), Some(value)) = (args.get(1), args.get(2)) else {
                eprintln!("Usage: ug config set <key> <value>");
                std::process::exit(1);
            };
            let key = config_key_or_exit(name);
            let path = config::config_path();
            let mut cfg = config::read_config_file(&path).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });
            if let Err(e) = config::value_set(&mut cfg, key, value) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            if let Err(e) = config::write_config_file(&path, &cfg) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            println!(
                "{C_GREEN}✓{C_RESET} {C_BOLD}{}{C_RESET} = {} → {}",
                key.name,
                config::display_value(key, value),
                path.display()
            );
            // A live env var still outranks what was just saved — say so
            // now rather than letting the next command surprise them.
            if let Some(env_key) = key.env {
                if std::env::var(env_key).map(|v| !v.trim().is_empty()).unwrap_or(false) {
                    println!(
                        "{C_YELLOW}▸ note:{C_RESET} ${} is set in your environment and overrides this value until unset"
                        , env_key
                    );
                }
            }
        }
        "unset" | "rm" => {
            let Some(name) = args.get(1) else {
                eprintln!("Usage: ug config unset <key>");
                std::process::exit(1);
            };
            let key = config_key_or_exit(name);
            let path = config::config_path();
            let mut cfg = config::read_config_file(&path).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });
            if !config::value_unset(&mut cfg, key) {
                println!("{} was not set — nothing to do", key.name);
                return;
            }
            if let Err(e) = config::write_config_file(&path, &cfg) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            println!("{C_GREEN}✓{C_RESET} unset {C_BOLD}{}{C_RESET}", key.name);
        }
        other => {
            eprintln!("Unknown config subcommand: {}", other);
            print_config_help();
            std::process::exit(1);
        }
    }
}

fn config_key_or_exit(name: &str) -> &'static config::ConfigKey {
    config::find_key(name).unwrap_or_else(|| {
        eprintln!("Unknown config key: {}", name);
        eprintln!("Known keys:");
        for k in config::CONFIG_KEYS {
            eprintln!("  {}", k.name);
        }
        std::process::exit(1);
    })
}

fn run_config_list() {
    let path = config::config_path();
    println!("{C_BOLD}UltraGraph config{C_RESET}  {C_DIM}{}{C_RESET}", path.display());
    println!("{C_DIM}precedence: CLI flag > env var > this file > built-in default{C_RESET}");
    println!();
    for key in config::CONFIG_KEYS {
        let saved = config::get(key.name);
        let value_label = match &saved {
            Some(v) => format!("{C_CYAN}{}{C_RESET}", config::display_value(key, v)),
            None => format!("{C_DIM}(not set){C_RESET}"),
        };
        // Flag an active env var: the saved value (or lack of one) is
        // not what commands will actually use right now.
        let env_note = key
            .env
            .filter(|e| std::env::var(e).map(|v| !v.trim().is_empty()).unwrap_or(false))
            .map(|e| format!("  {C_YELLOW}⚠ overridden by ${}{C_RESET}", e))
            .unwrap_or_default();
        let overrides = match key.env {
            Some(env) => format!("{} / ${}", key.flag, env),
            None => key.flag.to_string(),
        };
        println!("  {C_BOLD}{:<18}{C_RESET} {}{}", key.name, value_label, env_note);
        println!("  {C_DIM}{:<18} {} [{}]{C_RESET}", "", key.desc, overrides);
    }
    println!();
    println!("Run {C_CYAN}ug config set <key> <value>{C_RESET} to change, {C_CYAN}ug doctor{C_RESET} to see effective values.");
}

fn print_config_help() {
    println!("  {C_CYAN}ug config{C_RESET}  {C_YELLOW}— view and persist defaults (chat model, endpoints, …){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug config [list|get|set|unset|path] [<key>] [<value>]");
    println!();
    println!("  Saved to {C_CYAN}$UG_HOME/config.json{C_RESET} (default ~/.ug/config.json) and used by every");
    println!("  command as the fallback below CLI flags and env vars:");
    println!();
    println!("    {C_BOLD}CLI flag  >  env var  >  ug config  >  built-in default{C_RESET}");
    println!();
    println!("  A flag or env var that overrides a saved value prints a one-line notice.");
    println!();
    println!("{C_BOLD}Subcommands:{C_RESET}");
    println!("  {C_CYAN}list{C_RESET}               Show every key and its saved value (default)");
    println!("  {C_CYAN}get{C_RESET} <key>          Print one saved value");
    println!("  {C_CYAN}set{C_RESET} <key> <value>  Persist a value");
    println!("  {C_CYAN}unset{C_RESET} <key>        Remove a saved value");
    println!("  {C_CYAN}path{C_RESET}               Print the config file path");
    println!();
    println!("{C_BOLD}Keys:{C_RESET}");
    for key in config::CONFIG_KEYS {
        println!("  {C_CYAN}{:<18}{C_RESET} {}", key.name, key.desc);
    }
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_MAGENTA}ug config set{C_RESET} chat.model Qwen3.6-35B-A3B-MLX-8bit");
    println!("  {C_MAGENTA}ug config set{C_RESET} chat.base_url http://127.0.0.1:8000/v1");
    println!("  {C_MAGENTA}ug config get{C_RESET} chat.model");
    println!("  {C_MAGENTA}ug config unset{C_RESET} chat.model");
}

fn doctor_source_label(s: PrefSource) -> String {
    match s {
        PrefSource::Flag => "flag".to_string(),
        PrefSource::Env(name) => format!("env:{}", name),
        PrefSource::Config(key) => format!("config:{}", key),
        PrefSource::Default => "default".to_string(),
    }
}

/// `ug doctor` — print resolved project/db/embedder/chat configuration
/// and which tier (flag / env var / default) each value came from. Purely
/// read-only: resolves the same precedence chains the other commands use
/// but never builds an embedder/chat client or touches the network.
fn run_doctor(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_doctor_help();
        return;
    }
    println!("{C_BOLD}UltraGraph doctor{C_RESET}");
    println!();

    println!("{C_BOLD}Project{C_RESET}");
    let ug_home_from_env = std::env::var("UG_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some();
    println!(
        "  UG_HOME:      {C_CYAN}{}{C_RESET}  [{}]",
        project::ug_home().display(),
        if ug_home_from_env { "env:UG_HOME" } else { "default: ~/.ug" }
    );

    let name_flag = flag_value(args, &["-n", "--name"]);
    let project_name = name_flag
        .as_deref()
        .map(project::sanitize_name)
        .unwrap_or_else(|| project::derive_project_name("."));
    println!(
        "  project name: {C_CYAN}{}{C_RESET}  [{}]",
        project_name,
        if name_flag.is_some() { "flag:-n/--name" } else { "derived from cwd basename" }
    );

    let project_dir = project::project_dir(&project_name);
    let dir_status = if project_dir.exists() {
        format!("{C_GREEN}exists{C_RESET}")
    } else {
        format!("{C_YELLOW}not generated yet — run `ug gen`{C_RESET}")
    };
    println!("  project dir:  {} ({})", project_dir.display(), dir_status);

    let db_flag = flag_value(args, &["-d", "--db"]);
    let db_path = db_flag.clone().unwrap_or_else(project::default_read_db_path);
    let db_status = if std::path::Path::new(&db_path).exists() {
        format!("{C_GREEN}exists{C_RESET}")
    } else {
        format!("{C_YELLOW}missing — run `ug ingest`{C_RESET}")
    };
    println!(
        "  db path:      {} ({})  [{}]",
        db_path,
        db_status,
        if db_flag.is_some() { "flag:-d/--db" } else { "default: ~/.ug/<name>/ugdb → legacy ./.ug/ugdb" }
    );
    let cfg_path = config::config_path();
    println!(
        "  config file:  {} ({})",
        cfg_path.display(),
        if cfg_path.exists() {
            format!("{C_GREEN}exists{C_RESET} — manage with `ug config`")
        } else {
            format!("{C_YELLOW}none{C_RESET} — create with `ug config set <key> <value>`")
        }
    );
    println!();

    println!("{C_BOLD}Embeddings{C_RESET} (ingest / gen / semantic_search / search / serve)");
    let (base_url, base_src) =
        config::resolve_pref_cfg(flag_value(args, &["--base-url"]), "embed.base_url");
    let (_api_key, api_src) =
        config::resolve_pref_cfg(flag_value(args, &["--api-key"]), "embed.api_key");
    let (model, model_src) = config::resolve_pref_cfg(flag_value(args, &["--model"]), "embed.model");
    let backend = if base_url.is_some() {
        "remote (HTTP /v1/embeddings)"
    } else {
        "local (in-process ONNX)"
    };
    println!("  backend:      {C_CYAN}{}{C_RESET}  [{}]", backend, doctor_source_label(base_src));
    println!(
        "  model:        {}  [{}]",
        model.unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string()),
        doctor_source_label(model_src)
    );
    println!(
        "  base_url:     {}  [{}]",
        base_url.unwrap_or_else(|| format!("(n/a — {})", DEFAULT_EMBED_BASE_URL)),
        doctor_source_label(base_src)
    );
    println!("  api_key:      [{}]", doctor_source_label(api_src));
    println!();

    println!("{C_BOLD}Chat{C_RESET} (ug chat / POST /api/chat)");
    let chat_base_flag =
        flag_value(args, &["--chat-base-url"]).or_else(|| flag_value(args, &["--base-url"]));
    let (chat_base_url, chat_base_src) = config::resolve_pref_cfg(chat_base_flag, "chat.base_url");
    let chat_api_flag =
        flag_value(args, &["--chat-api-key"]).or_else(|| flag_value(args, &["--api-key"]));
    let (chat_api_key, chat_api_src) = config::resolve_pref_cfg(chat_api_flag, "chat.api_key");
    let (chat_model, chat_model_src) =
        config::resolve_pref_cfg(flag_value(args, &["--chat-model"]), "chat.model");
    let configured = chat_base_url.is_some() || chat_model.is_some();
    println!(
        "  base_url:     {}  [{}]",
        chat_base_url.unwrap_or_else(|| chat::DEFAULT_CHAT_BASE_URL.to_string()),
        doctor_source_label(chat_base_src)
    );
    println!(
        "  model:        {}  [{}]",
        chat_model.unwrap_or_else(|| chat::DEFAULT_CHAT_MODEL.to_string()),
        doctor_source_label(chat_model_src)
    );
    println!(
        "  api_key:      {}  [{}]",
        if chat_api_key.is_some() { "(set)" } else { "(default placeholder)" },
        doctor_source_label(chat_api_src)
    );
    println!(
        "  status:       {}",
        if configured {
            format!("{C_GREEN}configured{C_RESET} (base_url/model explicitly set)")
        } else {
            format!(
                "{C_YELLOW}not configured{C_RESET} — using sample defaults; run `ug config set chat.base_url <url>` (or pass --chat-base-url / $UG_CHAT_BASE_URL)"
            )
        }
    );
    println!();

    println!("{C_BOLD}Model cache{C_RESET} (ONNX weights for the local embedder)");
    println!("  {}", ultragraph::storage::embed_local::local_model_cache_dir().display());
    println!("  resolution: $UG_MODEL_CACHE → $XDG_CACHE_HOME/ug/models → platform cache dir → temp dir");
}

/// One HTTP endpoint `ug serve` registers, for `ug api`'s reference
/// listing. `cli_equivalent` is `Some("ug <cmd>")` when the exact same
/// data/action is also reachable as a plain CLI subcommand that works
/// without a server running at all — everything in this table is an
/// HTTP route, so it always requires `ug serve` to be up to hit it over
/// HTTP; this field instead tells the user whether *the underlying
/// capability* has a non-serve escape hatch.
struct ApiEntry {
    method: &'static str,
    path: &'static str,
    desc: &'static str,
    availability: &'static str,
    cli_equivalent: Option<&'static str>,
}

const API_ENDPOINTS: &[(&str, &[ApiEntry])] = &[
    (
        "Knowledge-base / project management",
        &[
            ApiEntry { method: "GET", path: "/api/projects", desc: "list discovered projects (or the single active one)", availability: "always", cli_equivalent: Some("ug list") },
            ApiEntry { method: "POST", path: "/api/projects/select", desc: "switch the server's active project", availability: "multi-project mode only", cli_equivalent: None },
            ApiEntry { method: "POST", path: "/api/projects/delete", desc: "delete a project's data directory", availability: "multi-project mode only", cli_equivalent: Some("ug rm") },
            ApiEntry { method: "POST", path: "/api/generate", desc: "spawn `ug gen` against a folder, returns a job id", availability: "multi-project mode only", cli_equivalent: Some("ug gen") },
            ApiEntry { method: "GET", path: "/api/generate/status", desc: "poll a generation job's progress/log", availability: "multi-project mode only", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/browse-dir", desc: "list subdirectories of a path (KB wizard folder picker)", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/capabilities", desc: "report db/embedder/chat readiness for the active project", availability: "always", cli_equivalent: Some("ug doctor (similar info)") },
            ApiEntry { method: "GET", path: "/api/config", desc: "persisted + effective settings with per-key source (flag/env/config/default)", availability: "always", cli_equivalent: Some("ug config list") },
            ApiEntry { method: "POST", path: "/api/config", desc: "persist settings to ~/.ug/config.json (chat changes apply immediately)", availability: "always", cli_equivalent: Some("ug config set") },
        ],
    ),
    (
        "Graph API (in-memory, active project)",
        &[
            ApiEntry { method: "GET", path: "/api/graph/stats", desc: "node/edge counts by type", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/graph/node/:id", desc: "fetch one node by id", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/graph/search", desc: "keyword search over graph nodes", availability: "always (empty if no project active)", cli_equivalent: Some("ug graph_search") },
            ApiEntry { method: "GET", path: "/api/graph/bfs/:id", desc: "k-hop BFS traversal from a node", availability: "always (empty if no project active)", cli_equivalent: Some("ug bfs") },
            ApiEntry { method: "GET", path: "/api/graph/path", desc: "shortest path between two nodes", availability: "always (empty if no project active)", cli_equivalent: Some("ug path") },
            ApiEntry { method: "GET", path: "/api/graph/filter", desc: "filter edges by type", availability: "always (empty if no project active)", cli_equivalent: Some("ug filter") },
            ApiEntry { method: "GET", path: "/api/graph/centrality", desc: "degree/betweenness centrality", availability: "always (empty if no project active)", cli_equivalent: Some("ug centrality") },
            ApiEntry { method: "GET", path: "/api/graph/cycles", desc: "detect cycles in the graph", availability: "always (empty if no project active)", cli_equivalent: Some("ug cycles") },
            ApiEntry { method: "GET", path: "/api/file", desc: "source file content for the preview panel", availability: "always (404 if file/project missing)", cli_equivalent: None },
        ],
    ),
    (
        "Agent tools (graph.json-backed — same names/params as the CLI and MCP)",
        &[
            ApiEntry { method: "GET", path: "/api/tools", desc: "list the agent tools and their paths (HTTP equivalent of MCP tools/list)", availability: "always", cli_equivalent: Some("ug help") },
            ApiEntry { method: "POST", path: "/api/tools/project_overview", desc: "stats, biggest files, most depended-upon symbols", availability: "always (empty if no project active)", cli_equivalent: Some("ug project_overview --json") },
            ApiEntry { method: "POST", path: "/api/tools/find_symbols", desc: "exact-name symbol lookup", availability: "always (empty if no project active)", cli_equivalent: Some("ug find_symbols --json") },
            ApiEntry { method: "POST", path: "/api/tools/file_outline", desc: "every indexed symbol in one file, in line order", availability: "always (empty if no project active)", cli_equivalent: Some("ug file_outline --json") },
            ApiEntry { method: "POST", path: "/api/tools/get_code", desc: "source for a node id or file/line range", availability: "always (empty if no project active)", cli_equivalent: Some("ug get_code --json") },
            ApiEntry { method: "POST", path: "/api/tools/find_usages", desc: "inbound callers/importers, with call sites", availability: "always (empty if no project active)", cli_equivalent: Some("ug find_usages --json") },
            ApiEntry { method: "POST", path: "/api/tools/shortest_path", desc: "shortest directed edge path between two node ids", availability: "always (empty if no project active)", cli_equivalent: Some("ug shortest_path --json") },
            ApiEntry { method: "POST", path: "/api/tools/graph_schema", desc: "node & edge types present, with counts", availability: "always (empty if no project active)", cli_equivalent: Some("ug graph_schema --json") },
        ],
    ),
    (
        "OverGraph search & chat (Phase 3 — needs a DB + embedder)",
        &[
            ApiEntry { method: "GET", path: "/api/db/node/:id", desc: "fetch one node from the OverGraph store", availability: "503 if no DB backend configured", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/db/traverse/:id", desc: "k-hop BFS over the OverGraph edges table", availability: "503 if no DB backend configured", cli_equivalent: Some("ug traverse") },
            ApiEntry { method: "POST", path: "/api/search/semantic", desc: "semantic vector search", availability: "503 if no DB + embedder configured", cli_equivalent: Some("ug semantic_search") },
            ApiEntry { method: "POST", path: "/api/search/hybrid", desc: "GraphRAG: semantic search → graph expansion → ranked context", availability: "503 if no DB + embedder configured", cli_equivalent: Some("ug search") },
            ApiEntry { method: "POST", path: "/api/chat", desc: "GraphRAG-grounded chat completion", availability: "503 if no DB + embedder + chat model configured", cli_equivalent: Some("ug chat") },
        ],
    ),
    (
        "UI & static assets",
        &[
            ApiEntry { method: "GET", path: "/", desc: "3D visualization UI (single-page app)", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/index.html", desc: "same as /", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/ug-vis.bundle.js", desc: "three.js/3d-force-graph JS bundle for the UI", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/favicon.svg", desc: "browser tab icon", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/healthz", desc: "liveness probe — always returns \"ok\"", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/graph.json", desc: "raw graph JSON for the active project", availability: "always (empty if no project active)", cli_equivalent: None },
        ],
    )    
];

/// `ug api` — reference listing of every HTTP endpoint `ug serve`
/// exposes, for users/agents who want to hit the REST API directly
/// instead of (or alongside) the CLI. Every row is an HTTP route, so
/// all of them require `ug serve` to be running to reach at all; the
/// "CLI equivalent" column instead flags which ones have a plain CLI
/// subcommand that does the same thing without a server.
fn run_api(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_api_help();
        return;
    }

    if has_flag(args, "--json") {
        let sections: Vec<serde_json::Value> = API_ENDPOINTS
            .iter()
            .map(|(section, entries)| {
                serde_json::json!({
                    "section": section,
                    "endpoints": entries.iter().map(|e| serde_json::json!({
                        "method": e.method,
                        "path": e.path,
                        "description": e.desc,
                        "availability": e.availability,
                        "cli_equivalent": e.cli_equivalent,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "requires_serve": true, "sections": sections }))
                .unwrap_or_default()
        );
        return;
    }

    println!("{C_BOLD}ug serve — HTTP API reference{C_RESET}");
    println!(
        "Every endpoint below is only reachable while {C_CYAN}ug serve{C_RESET} is running (default http://localhost:8080)."
    );
    println!(
        "{C_DIM}\"CLI equivalent\" marks endpoints whose capability is also available as a plain CLI command, no server needed.{C_RESET}"
    );
    println!();

    for (section, entries) in API_ENDPOINTS {
        println!("{C_BOLD}{}{C_RESET}", section);
        for e in *entries {
            let method_color = if e.method == "GET" { C_CYAN } else { C_MAGENTA };
            println!(
                "  {}{:<5}{C_RESET} {C_BOLD}{:<24}{C_RESET} {}",
                method_color, e.method, e.path, e.desc
            );
            let cli_note = match e.cli_equivalent {
                Some(cmd) => format!("{C_GREEN}CLI equivalent: {}{C_RESET}", cmd),
                None => format!("{C_DIM}serve-only (no CLI equivalent){C_RESET}"),
            };
            println!("        {C_YELLOW}{}{C_RESET}  ·  {}", e.availability, cli_note);
        }
        println!();
    }

    println!("Run {C_CYAN}ug api --json{C_RESET} for machine-readable output.");
}

/// Render epoch seconds as local-naive `YYYY-MM-DD HH:MM:SS` (UTC).
fn format_epoch(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    // Days-from-civil algorithm (Howard Hinnant) — avoids a chrono dep.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

fn run_ingest(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_ingest_help();
        return;
    }

    let graph_file = flag_value(args, &["-i", "--input"]).unwrap_or_else(|| {
        project::project_dir(&project::resolve_project_name(args, "."))
            .join("graph.json")
            .to_string_lossy()
            .into_owned()
    });

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph file");
    let graph: GraphData = serde_json::from_str(&graph_json).expect("Failed to parse graph JSON");
    let mut embedder = embedder_from_args(args);
    let dim_was_explicit = flag_value(args, &["--embedding-dim"]).is_some();
    let rt = tokio_runtime();

    let start_total = std::time::Instant::now();

    rt.block_on(async {
        if !dim_was_explicit {
            match embedder.probe_dim().await {
                Ok(probed) if probed != embedder.config().dim => embedder.set_dim(probed),
                Ok(_) => {}
                Err(e) => {
                    eprintln!("embedder dim probe failed: {}", e);
                    return;
                }
            }
        }
        let dim = embedder.config().dim as u32;
        let specs = store_specs_from_args(args, dim);
        announce_destinations(&specs);
        let dest_label: Vec<String> = specs.iter().map(|s| s.name().to_string()).collect();
        match ingest_with_specs(&specs, &embedder, &graph).await {
            Ok((nodes_written, edges_written)) => {
                println!("────────────────────────────────────────");
                println!(
                    "Ingested {} nodes, {} edges into [{}] in {:?}",
                    nodes_written,
                    edges_written,
                    dest_label.join(", "),
                    start_total.elapsed()
                );
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    });
}

// vector search on OverGraph (only)
fn run_semantic_search(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_semantic_search_help();
        return;
    }
    if args.is_empty() {
        eprintln!(
            "Usage: ug semantic_search <query> [-n|--name <project>] [-k|--limit <n>] \\
                 [--filter <sql>] [--base-url <url>] [--api-key <key>] [--model <name>] \\
                 [--embedding-dim <n>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let query = first_positional(
        args,
        &[
            "-n",
            "--name",
            "-k",
            "--limit",
            "--filter",
            "--base-url",
            "--api-key",
            "--model",
            "--embedding-dim",
            "-o",
            "--output",
            "--dest",
            "--neo4j-uri",
            "--neo4j-user",
            "--neo4j-password",
            "--neo4j-database",
        ],
    )
    .expect("missing query");
    let limit: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let filter = flag_value(args, &["--filter"]);
    let output_path = flag_value(args, &["-o", "--output"]);
    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();

    let result_json = rt.block_on(async {
        let dim = embedder.config().dim as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store(&spec)
            .await
            .unwrap_or_else(|e| panic!("failed to open {} store: {}", spec.name(), e));
        let hits = match filter.as_deref() {
            Some(f) => storage::semantic_search_w_where(store.as_ref(), &embedder, &query, limit, f)
                .await
                .expect("semantic_search_w_where failed"),
            None => storage_semantic_search(store.as_ref(), &embedder, &query, limit)
                .await
                .expect("semantic_search failed"),
        };

        let json: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
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
            })
            .collect();
        serde_json::to_string_pretty(&json).unwrap_or_default()
    });

    write_or_print(output_path.as_deref(), &result_json, "search result");
}

// graphRAG hybrid search: RRF seeds → PPR (default) or MMR rerank → snippet-attached context
fn run_hybrid_search(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_hybrid_search_help();
        return;
    }
    if args.is_empty() {
        eprintln!(
            "Usage: ug search <query> [-n|--name <project>] [-k|--limit <n>] \\
                 [--filter <sql>] [--direction <out|in|both>] \\
                 [-t|--edge-type <type>]... [--max-chars <n>] \\
                 [--no-snippets] [--repo-root <path>] \\
                 [--base-url <url>] [--api-key <key>] [--model <name>] [--embedding-dim <n>] \\
                 [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let value_flags = [
        "-n",
        "--name",
        "-k",
        "--limit",
        "--hops",
        "--filter",
        "--strategy",
        "--direction",
        "-t",
        "--edge-type",
        "--max-chars",
        "--mmr-lambda",
        "--repo-root",
        "--base-url",
        "--api-key",
        "--model",
        "--embedding-dim",
        "-o",
        "--output",
        "--dest",
        "--neo4j-uri",
        "--neo4j-user",
        "--neo4j-password",
        "--neo4j-database",
    ];
    let query = first_positional(args, &value_flags).expect("missing query");
    let k: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let hops: u32 = flag_value(args, &["--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let filter = flag_value(args, &["--filter"]);
    let strategy = flag_value(args, &["--strategy"])
        .map(|s| RankStrategy::from_str_lossy(&s))
        .unwrap_or(RankStrategy::Ppr);
    let direction = flag_value(args, &["--direction"])
        .map(|s| Direction::from_str_lossy(&s))
        .unwrap_or(Direction::Both);
    let edge_types = multi_flag(args, &["-t", "--edge-type"]);
    let max_chars: usize = flag_value(args, &["--max-chars"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(12_000);
    let mmr_lambda: f32 = flag_value(args, &["--mmr-lambda"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.6);
    let include_snippets = !has_flag(args, "--no-snippets");
    let repo_root: PathBuf = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let output_path = flag_value(args, &["-o", "--output"]);

    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();

    let result_json = rt.block_on(async {
        let dim = embedder.config().dim as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store(&spec)
            .await
            .unwrap_or_else(|e| panic!("failed to open {} store: {}", spec.name(), e));
        let mut opts = SearchKbOptions::new(&query, repo_root.as_path());
        opts.k = k;
        opts.hops = hops;
        opts.edge_types = if edge_types.is_empty() {
            None
        } else {
            Some(edge_types.as_slice())
        };
        opts.direction = direction;
        opts.max_chars = max_chars;
        opts.mmr_lambda = mmr_lambda;
        opts.where_clause = filter.as_deref();
        opts.include_snippets = include_snippets;
        opts.strategy = strategy;

        let result = storage_search_kb(store.as_ref(), &embedder, opts)
            .await
            .expect("hybrid_search failed");
        serde_json::to_string_pretty(&result).unwrap_or_default()
    });

    write_or_print(output_path.as_deref(), &result_json, "hybrid search result");
}

fn run_traverse(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_traverse_help();
        return;
    }
    if args.is_empty() {
        eprintln!(
            "Usage: ug traverse <start-node-id>... [-n|--name <project>] [-k|--hops <n>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let starts = positionals(
        args,
        &[
            "-n",
            "--name",
            "-k",
            "--hops",
            "-o",
            "--output",
            "--dest",
            "--neo4j-uri",
            "--neo4j-user",
            "--neo4j-password",
            "--neo4j-database",
        ],
    );
    if starts.is_empty() {
        eprintln!("missing start node id");
        std::process::exit(1);
    }
    let hops: u32 = flag_value(args, &["-k", "--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let output_path = flag_value(args, &["-o", "--output"]);

    // graph.json holds the same edges the store does and needs no db, so it
    // is the default — this is the same walk `find_usages` does. `--dest`
    // still routes to the store, which is how you verify what actually
    // landed in a destination (see docs/MULTI-DEST.md).
    if flag_value(args, &["--dest"]).is_none() {
        let (graph, _raw, _path) = load_agent_graph(args);
        let params = agent_tools::TraverseParams {
            node_id: starts,
            hops: Some(hops),
            edge_types: multi_flag(args, &["--edge-type", "-t"]),
            direction: flag_value(args, &["--direction", "-d"]),
        };
        let result = agent_tools::traverse(&graph, &params);
        let ok = result.ok();
        emit_agent_result(
            args,
            &result,
            || agent_tools::render_traverse(&result, Render::Ansi),
            "traverse result",
            ok,
        );
        return;
    }

    let rt = tokio_runtime();
    let json = rt.block_on(async {
        // Traversal doesn't need an embedder, but `single_store_spec_from_args`
        // wants the configured dim so the OverGraph sidecar validation works.
        // Read it from the existing meta file when possible; fall back to the
        // default. The Neo4j path persists its own dim independently.
        let dim = ultragraph::storage::DEFAULT_EMBEDDING_DIM as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store(&spec)
            .await
            .unwrap_or_else(|e| panic!("failed to open {} store: {}", spec.name(), e));
        let result = storage::traverse_filtered(store.as_ref(), &starts, hops, None, Direction::Outbound)
            .await
            .expect("traverse failed");
        let nodes_json: Vec<serde_json::Value> = result
            .nodes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "name": n.name,
                    "node_type": n.node_type,
                    "file": n.file,
                    "distance": result.distances.get(&n.id).copied().unwrap_or(0),
                })
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
        serde_json::to_string_pretty(&serde_json::json!({
            "nodes": nodes_json,
            "edges": edges_json,
        }))
        .unwrap_or_default()
    });

    write_or_print(output_path.as_deref(), &json, "traverse result");
}

// ---------- Chat (RAG + LLM) ----------

pub(crate) fn chat_client_from_args(args: &[String]) -> chat::ChatClient {
    let cfg = chat_config_from_args(args);
    eprintln!(
        "{C_CYAN}▸{C_RESET} Chat: model={C_BOLD}{}{C_RESET}, base_url={}, temperature={}, max_tokens={}",
        cfg.model, cfg.base_url, cfg.temperature, cfg.max_tokens
    );
    chat::ChatClient::new(cfg).unwrap_or_else(|e| {
        eprintln!("failed to build chat client: {}", e);
        std::process::exit(1);
    })
}

fn chat_config_from_args(args: &[String]) -> chat::ChatConfig {
    let base_url_flag = flag_value(args, &["--chat-base-url"])
        .or_else(|| flag_value(args, &["--base-url"]));
    let (base_url, _) = config::resolve_pref_cfg(base_url_flag, "chat.base_url");
    let api_key_flag = flag_value(args, &["--chat-api-key"])
        .or_else(|| flag_value(args, &["--api-key"]));
    let (api_key, _) = config::resolve_pref_cfg(api_key_flag, "chat.api_key");
    let (model, _) = config::resolve_pref_cfg(flag_value(args, &["--chat-model"]), "chat.model");
    let (temp_raw, _) =
        config::resolve_pref_cfg(flag_value(args, &["--temperature"]), "chat.temperature");
    let temperature = temp_raw.and_then(|s| s.parse().ok());
    let (max_tok_raw, _) =
        config::resolve_pref_cfg(flag_value(args, &["--max-tokens"]), "chat.max_tokens");
    let max_tokens = max_tok_raw.and_then(|s| s.parse().ok());
    let (timeout_raw, _) =
        config::resolve_pref_cfg(flag_value(args, &["--chat-timeout"]), "chat.timeout_secs");
    let timeout = timeout_raw.and_then(|s| s.parse().ok());
    chat::ChatConfig::with_overrides(base_url, api_key, model, temperature, max_tokens, timeout)
}

fn run_chat(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_chat_help();
        return;
    }

    // Value-bearing flags so the first non-flag positional becomes the
    // (optional) one-shot prompt — anything else drops us into REPL mode.
    let value_flags = [
        "-n",
        "--name",
        "-k",
        "--limit",
        "--hops",
        "--strategy",
        "--direction",
        "-t",
        "--edge-type",
        "--max-chars",
        "--repo-root",
        "--base-url",
        "--api-key",
        "--model",
        "--embedding-dim",
        "--embedding-model",
        "--embedding-base-url",
        "--embedding-api-key",
        "--chat-base-url",
        "--chat-api-key",
        "--chat-model",
        "--temperature",
        "--max-tokens",
        "--chat-timeout",
        "--system",
        "--filter",
        "-o",
        "--output",
        "--dest",
        "--neo4j-uri",
        "--neo4j-user",
        "--neo4j-password",
        "--neo4j-database",
    ];

    let oneshot_query = first_positional(args, &value_flags);
    let json_output = has_flag(args, "--json");
    let show_context = has_flag(args, "--show-context") || has_flag(args, "-v");
    let no_snippets = has_flag(args, "--no-snippets");

    let k: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let hops: u32 = flag_value(args, &["--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let max_chars: usize = flag_value(args, &["--max-chars"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(chat::DEFAULT_CTX_MAX_CHARS);
    let strategy = flag_value(args, &["--strategy"])
        .map(|s| RankStrategy::from_str_lossy(&s))
        .unwrap_or(RankStrategy::Ppr);
    let direction = flag_value(args, &["--direction"])
        .map(|s| Direction::from_str_lossy(&s))
        .unwrap_or(Direction::Both);
    let edge_types = multi_flag(args, &["-t", "--edge-type"]);
    let repo_root: PathBuf = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let system_prompt = flag_value(args, &["--system"]);
    let where_clause = flag_value(args, &["--filter"]);
    let output_path = flag_value(args, &["-o", "--output"]);

    let embedder = embedder_from_chat_args(args);
    let chat_client = chat_client_from_args(args);
    let rt = tokio_runtime();

    rt.block_on(async {
        let dim = embedder.config().dim as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store(&spec)
            .await
            .unwrap_or_else(|e| {
                eprintln!("failed to open {} store: {}", spec.name(), e);
                std::process::exit(1);
            });

        let edge_types_owned: Option<Vec<String>> = if edge_types.is_empty() {
            None
        } else {
            Some(edge_types)
        };
        let opts_factory = |q: &str| {
            let mut o = chat::ChatRagOptions::new();
            o.k = k;
            o.hops = hops;
            o.strategy = strategy;
            o.direction = direction;
            o.edge_types = edge_types_owned.as_deref();
            o.include_snippets = !no_snippets;
            o.max_context_chars = max_chars;
            o.where_clause = where_clause.as_deref();
            o.system_prompt = system_prompt.as_deref();
            let _ = q; // q reserved for future per-call overrides
            o
        };

        // Tokens stream to the terminal as they arrive unless the output
        // is structured (--json) or the user opts out (--no-stream).
        let no_stream = has_flag(args, "--no-stream");

        match oneshot_query {
            Some(q) => {
                if json_output || no_stream {
                    let outcome = match chat::run_chat_rag(
                        store.as_ref(),
                        &embedder,
                        &chat_client,
                        repo_root.as_path(),
                        &q,
                        &[],
                        opts_factory(&q),
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("chat failed: {}", e);
                            std::process::exit(1);
                        }
                    };

                    if json_output {
                        let body = chat_outcome_to_json(&q, &outcome);
                        let text = serde_json::to_string_pretty(&body).unwrap_or_default();
                        write_or_print(output_path.as_deref(), &text, "chat result");
                    } else {
                        print_chat_outcome(&q, &outcome, show_context);
                        if let Some(p) = output_path.as_deref() {
                            write_file(p, &outcome.answer);
                            println!("Wrote answer to {}", p);
                        }
                    }
                } else {
                    let outcome = match stream_chat_turn(
                        store.as_ref(),
                        &embedder,
                        &chat_client,
                        repo_root.as_path(),
                        &q,
                        &[],
                        opts_factory(&q),
                        show_context,
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("chat failed: {}", e);
                            std::process::exit(1);
                        }
                    };
                    if let Some(p) = output_path.as_deref() {
                        write_file(p, &outcome.answer);
                        println!("Wrote answer to {}", p);
                    }
                }
            }
            None => {
                if json_output {
                    eprintln!("Error: --json requires a one-shot prompt; cannot pair with REPL mode.");
                    std::process::exit(2);
                }
                run_chat_repl(
                    store.as_ref(),
                    &embedder,
                    &chat_client,
                    repo_root.as_path(),
                    opts_factory,
                    show_context,
                    no_stream,
                )
                .await;
            }
        }
    });
}

fn chat_outcome_to_json(query: &str, outcome: &chat::ChatRagOutcome) -> serde_json::Value {
    let citations: Vec<serde_json::Value> = outcome
        .context
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            serde_json::json!({
                "index": i + 1,
                "id": it.id,
                "name": it.name,
                "node_type": it.node_type,
                "file": it.file,
                "start_line": it.start_line,
                "end_line": it.end_line,
                "description": it.description,
                "distance": it.distance,
                "hop": it.hop,
                "snippet": it.snippet,
            })
        })
        .collect();
    serde_json::json!({
        "query": query,
        "answer": outcome.answer,
        "citations": citations,
        "seed_id": outcome.context.seed_id,
        "retrieval_ms": outcome.retrieval_ms,
        "completion_ms": outcome.completion_ms,
        "usage": outcome.usage,
    })
}

fn print_context_items(items: &[ultragraph::storage::ContextItem]) {
    println!("{C_BOLD}{C_MAGENTA}Retrieved context ({} items):{C_RESET}", items.len());
    for (i, it) in items.iter().enumerate() {
        let line_label = if it.start_line > 0 {
            format!(":{}-{}", it.start_line, it.end_line)
        } else {
            String::new()
        };
        println!(
            "  {C_CYAN}[#{}]{C_RESET} {C_BOLD}{}{C_RESET} {C_YELLOW}({}){C_RESET} {} {}{}",
            i + 1,
            it.name,
            it.node_type,
            if it.file.is_empty() { "<unknown>" } else { it.file.as_str() },
            line_label,
            if it.hop > 0 {
                format!(" {}hop={}{}", C_BLUE, it.hop, C_RESET)
            } else {
                String::new()
            }
        );
    }
    println!();
}

fn print_chat_meta(outcome: &chat::ChatRagOutcome) {
    println!(
        "{C_CYAN}▸{C_RESET} retrieval={}ms · completion={}ms · {} citation(s){}",
        outcome.retrieval_ms,
        outcome.completion_ms,
        outcome.context.items.len(),
        match &outcome.usage {
            Some(u) => format!(
                " · tokens prompt={} completion={} total={}",
                u.prompt_tokens.unwrap_or(0),
                u.completion_tokens.unwrap_or(0),
                u.total_tokens.unwrap_or(0),
            ),
            None => String::new(),
        }
    );
}

fn print_chat_outcome(query: &str, outcome: &chat::ChatRagOutcome, show_context: bool) {
    println!();
    println!("{C_BOLD}{C_CYAN}❯ Query:{C_RESET} {}", query);
    println!();
    if show_context {
        print_context_items(&outcome.context.items);
    }
    println!("{C_BOLD}{C_GREEN}Answer:{C_RESET}");
    println!("{}", outcome.answer.trim_end());
    println!();
    print_chat_meta(outcome);
}

/// One RAG turn with live token streaming to the terminal: a transient
/// "retrieving" line while search runs, the context list (when enabled)
/// as soon as it's ready, provider reasoning dimmed, then answer tokens
/// as they arrive. Falls back to a single chunk automatically when the
/// provider doesn't stream (handled in `run_chat_rag_stream`).
async fn stream_chat_turn(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat_client: &chat::ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    history: &[chat::ChatMessage],
    opts: chat::ChatRagOptions<'_>,
    show_context: bool,
) -> Result<chat::ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>> {
    use std::io::Write;

    println!();
    println!("{C_BOLD}{C_CYAN}❯ Query:{C_RESET} {}", query);
    println!();
    eprint!("{C_DIM}⣾ retrieving context…{C_RESET}");
    let _ = std::io::stderr().flush();

    let mut in_reasoning = false;
    let mut printed_answer_header = false;
    let outcome = chat::run_chat_rag_stream(
        store,
        embedder,
        chat_client,
        repo_root,
        query,
        history,
        opts,
        |ctx| {
            // Clear the transient retrieval line before real output.
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
            if show_context {
                print_context_items(&ctx.items);
            }
        },
        |d| {
            if let Some(r) = &d.reasoning {
                if !in_reasoning {
                    println!("{C_DIM}Reasoning:{C_RESET}");
                    print!("{C_DIM}");
                    in_reasoning = true;
                }
                print!("{}", r);
            }
            if let Some(c) = &d.content {
                if in_reasoning {
                    print!("{C_RESET}");
                    println!();
                    println!();
                    in_reasoning = false;
                }
                if !printed_answer_header {
                    println!("{C_BOLD}{C_GREEN}Answer:{C_RESET}");
                    printed_answer_header = true;
                }
                print!("{}", c);
            }
            let _ = std::io::stdout().flush();
        },
    )
    .await?;
    if in_reasoning {
        print!("{C_RESET}");
    }
    println!();
    println!();
    print_chat_meta(&outcome);
    Ok(outcome)
}

async fn run_chat_repl<'a, F>(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat_client: &chat::ChatClient,
    repo_root: &std::path::Path,
    mut opts_factory: F,
    show_context: bool,
    no_stream: bool,
) where
    F: for<'b> FnMut(&'b str) -> chat::ChatRagOptions<'a>,
{
    use std::io::{BufRead, Write};
    println!();
    println!("{C_BOLD}{C_MAGENTA}UltraGraph Chat — interactive RAG REPL{C_RESET}");
    println!("{C_CYAN}Type a question and press Enter. Commands: /quit /reset /context on|off /help{C_RESET}");
    println!();

    let mut history: Vec<chat::ChatMessage> = Vec::new();
    let mut show_ctx = show_context;
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();

    loop {
        print!("{C_BOLD}{C_GREEN}you ❯ {C_RESET}");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        match handle.read_line(&mut buf) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {}", e);
                break;
            }
        }
        let q = buf.trim();
        if q.is_empty() {
            continue;
        }
        match q {
            "/quit" | "/exit" | ":q" => break,
            "/reset" => {
                history.clear();
                println!("{C_YELLOW}(history cleared){C_RESET}");
                continue;
            }
            "/context on" => {
                show_ctx = true;
                println!("{C_YELLOW}(context display: on){C_RESET}");
                continue;
            }
            "/context off" => {
                show_ctx = false;
                println!("{C_YELLOW}(context display: off){C_RESET}");
                continue;
            }
            "/help" | "/?" => {
                println!("Commands: /quit, /reset, /context on|off, /help");
                continue;
            }
            _ => {}
        }

        let opts = opts_factory(q);
        let outcome = if no_stream {
            match chat::run_chat_rag(store, embedder, chat_client, repo_root, q, &history, opts)
                .await
            {
                Ok(o) => {
                    print_chat_outcome(q, &o, show_ctx);
                    o
                }
                Err(e) => {
                    eprintln!("{C_YELLOW}chat error:{C_RESET} {}", e);
                    continue;
                }
            }
        } else {
            match stream_chat_turn(
                store,
                embedder,
                chat_client,
                repo_root,
                q,
                &history,
                opts,
                show_ctx,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("{C_YELLOW}chat error:{C_RESET} {}", e);
                    continue;
                }
            }
        };

        // Keep the last 6 exchanges to bound prompt growth.
        history.push(chat::ChatMessage {
            role: "user".into(),
            content: q.to_string(),
        });
        history.push(chat::ChatMessage {
            role: "assistant".into(),
            content: outcome.answer.clone(),
        });
        let max_history = 12;
        if history.len() > max_history {
            let drop_n = history.len() - max_history;
            history.drain(0..drop_n);
        }
    }
}

// ---------- Help ----------

fn print_index_help() {
    println!("  {C_CYAN}ug index{C_RESET}  {C_YELLOW}— index a directory into a tree of code entities{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug index [<path>] [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <path>   Input directory (default: .)");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>  Output file (default: ~/.ug/<name>/indexed-tree.json)");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>    Project name (default: input dir basename)");
    println!("  {C_CYAN}-c, --cache{C_RESET} <dir>     Cache directory for incremental indexing");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug index{C_RESET} -i ./src -o index.json");
    println!("  {C_CYAN}ug index{C_RESET} -c ./cache -n myrepo");
}

fn print_graph_help() {
    println!("  {C_CYAN}ug graph{C_RESET}  {C_YELLOW}— build a graph from the indexed tree output{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph [<file>] [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>  Input index file (default: ~/.ug/<name>/indexed-tree.json)");
    println!("  {C_CYAN}-o, --output{C_RESET} <file> Output graph file (default: ~/.ug/<name>/graph.json)");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>   Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph{C_RESET} -i index.json -o graph.json");
    println!("  {C_CYAN}ug graph{C_RESET} (uses defaults)");
}

/// Options every graph-analysis command shares.
fn print_graph_common_options() {
    println!("  {C_CYAN}-n, --name{C_RESET} <project>  Project under ~/.ug (default: cwd's project, else most recent)");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>    Explicit graph.json (overrides --name)");
    println!("  {C_CYAN}--json{C_RESET}                Print the raw JSON result instead of a report");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>   Write the raw JSON to a file");
}

fn print_graph_analyze_help() {
    println!("  {C_CYAN}ug graph_analyze{C_RESET}  {C_YELLOW}— full graph analysis (centrality + cycles){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_analyze [options]        {C_DIM}(alias: analyze){C_RESET}");
    println!();
    println!("  Writes analysis.json + cycles.json next to the project's graph.json");
    println!("  (so {C_CYAN}ug serve{C_RESET} picks them up) and prints a ranked summary.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--top{C_RESET} <n>             Rows per ranking in the summary (default 10)");
    println!("  {C_CYAN}--no-write{C_RESET}            Report only — don't write analysis.json/cycles.json");
    println!("  {C_CYAN}-o, --output{C_RESET} <dir>    Output directory (default: the project dir)");
    println!("  {C_CYAN}-n, --name{C_RESET} <project>  Project under ~/.ug (default: cwd's project, else most recent)");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>    Explicit graph.json (overrides --name)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_analyze{C_RESET}");
    println!("  {C_CYAN}ug graph_analyze{C_RESET} -n my-repo --top 25");
    println!("  {C_CYAN}ug graph_analyze{C_RESET} --no-write --json");
}

fn print_graph_bfs_help() {
    println!("  {C_CYAN}ug graph_bfs{C_RESET}  {C_YELLOW}— K-hop breadth-first traversal from a node{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_bfs <node-id-or-name> [options]   {C_DIM}(alias: bfs){C_RESET}");
    println!();
    println!("  The start node takes a node id, a file path, or a symbol name —");
    println!("  ambiguous names list the candidate ids instead of guessing.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-k, --hops{C_RESET} <n>        Hop radius (default 1)");
    println!("  {C_CYAN}-d, --direction{C_RESET} <dir> out (callees/imports, default) | in (callers) | both");
    println!("  {C_CYAN}--edge-type{C_RESET} <type>    Only follow these edges (repeatable; see graph_schema)");
    println!("  {C_CYAN}-t, --type{C_RESET} <type>     Only show these node types (repeatable)");
    println!("  {C_CYAN}-f, --file{C_RESET} <prefix>   Only show nodes under this path prefix");
    println!("  {C_CYAN}-l, --limit{C_RESET} <n>       Max nodes printed (default 50)");
    print_graph_common_options();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_bfs{C_RESET} run_serve -k 2");
    println!("  {C_CYAN}ug graph_bfs{C_RESET} src/index.ts -k 2 --edge-type imports");
    println!("  {C_CYAN}ug graph_bfs{C_RESET} loadConfig -d in   {C_YELLOW}# what reaches this symbol{C_RESET}");
    println!("  {C_CYAN}ug graph_bfs{C_RESET} run_gen -n other-project -t Function");
}

fn print_graph_filter_help() {
    println!("  {C_CYAN}ug graph_filter{C_RESET}  {C_YELLOW}— list graph edges by type/endpoint{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_filter [<edge-type>...] [options]   {C_DIM}(alias: filter){C_RESET}");
    println!();
    println!("  With no arguments it prints every edge type in the graph with counts.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>   Edge type (repeatable; same as a positional)");
    println!("  {C_CYAN}--from{C_RESET} <node>         Only edges out of this node (id/name/file)");
    println!("  {C_CYAN}--to{C_RESET} <node>           Only edges into this node (id/name/file)");
    println!("  {C_CYAN}-f, --file{C_RESET} <prefix>   Only edges touching this path prefix");
    println!("  {C_CYAN}-l, --limit{C_RESET} <n>       Max edges printed (default 50)");
    print_graph_common_options();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_filter{C_RESET}                     {C_YELLOW}# what edge types exist?{C_RESET}");
    println!("  {C_CYAN}ug graph_filter{C_RESET} Calls Imports");
    println!("  {C_CYAN}ug graph_filter{C_RESET} Calls --from run_gen");
    println!("  {C_CYAN}ug graph_filter{C_RESET} Imports -f node/ -l 100");
}

fn print_graph_path_help() {
    println!("  {C_CYAN}ug graph_path{C_RESET}  {C_YELLOW}— how are two nodes connected?{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_path <source> <target> [options]   {C_DIM}(aliases: path, shortest_path){C_RESET}");
    println!();
    println!("  Source/target take a node id, a file path, or a symbol name. Edges are");
    println!("  directed (imports/calls/contains flow source→target); if no forward path");
    println!("  exists the reverse direction is tried and labeled as such.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--strict{C_RESET}              Don't retry the reverse direction");
    print_graph_common_options();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_path{C_RESET} run_gen run_ingest");
    println!("  {C_CYAN}ug graph_path{C_RESET} src/a.ts src/b.ts --strict");
    println!("  {C_CYAN}ug graph_path{C_RESET} file:src/a.ts file:src/b.ts -n my-repo");
}

fn print_graph_centrality_help() {
    println!("  {C_CYAN}ug graph_centrality{C_RESET}  {C_YELLOW}— degree & betweenness centrality{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_centrality [options]   {C_DIM}(alias: centrality){C_RESET}");
    println!();
    println!("  Degree = how connected a node is. Betweenness = how often it sits on");
    println!("  the shortest path between others (architectural bridges).");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--top{C_RESET} <n>             Rows per ranking (default 20)");
    println!("  {C_CYAN}-t, --type{C_RESET} <type>     Only rank these node types (repeatable)");
    println!("  {C_CYAN}-f, --file{C_RESET} <prefix>   Only rank nodes under this path prefix");
    print_graph_common_options();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_centrality{C_RESET} --top 30");
    println!("  {C_CYAN}ug graph_centrality{C_RESET} -t Function -f native/src/");
    println!("  {C_CYAN}ug graph_centrality{C_RESET} -n my-repo -o centrality.json");
}

fn print_graph_cycles_help() {
    println!("  {C_CYAN}ug graph_cycles{C_RESET}  {C_YELLOW}— detect dependency cycles{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_cycles [options]   {C_DIM}(alias: cycles){C_RESET}");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-l, --limit{C_RESET} <n>       Max cycles printed (default 20)");
    println!("  {C_CYAN}--min-len{C_RESET} <n>         Only cycles with at least n nodes");
    println!("  {C_CYAN}--max-len{C_RESET} <n>         Only cycles with at most n nodes");
    println!("  {C_CYAN}-f, --file{C_RESET} <prefix>   Only cycles touching this path prefix");
    println!("  {C_CYAN}--fail-on-cycle{C_RESET}       Exit 1 when any cycle matches (CI guard)");
    print_graph_common_options();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_cycles{C_RESET}");
    println!("  {C_CYAN}ug graph_cycles{C_RESET} --min-len 3 -f src/");
    println!("  {C_CYAN}ug graph_cycles{C_RESET} --fail-on-cycle --json   {C_YELLOW}# CI{C_RESET}");
}

fn print_ingest_help() {
    println!("  {C_CYAN}ug ingest{C_RESET}  {C_YELLOW}— embed graph nodes and write to one or more knowledge stores{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug ingest [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>  Graph JSON (default: ~/.ug/<name>/graph.json)");
    println!("  {C_CYAN}-o, --output{C_RESET} <dir>  OverGraph directory (default: ~/.ug/<name>/ugdb)");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>   Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Destinations (default: overgraph):{C_RESET}");
    println!("  {C_CYAN}--dest{C_RESET} <kind[,kind...]>   {C_BOLD}overgraph{C_RESET} | {C_BOLD}neo4j{C_RESET}. Comma-separated for fan-out ingest.");
    println!("                              Reads (semantic_search/search/traverse) accept");
    println!("                              exactly one --dest.");
    println!("  {C_CYAN}--neo4j-uri{C_RESET} <uri>      e.g. neo4j://localhost:7687 (env: UG_NEO4J_URI)");
    println!("  {C_CYAN}--neo4j-user{C_RESET} <user>    Default: neo4j (env: UG_NEO4J_USER)");
    println!("  {C_CYAN}--neo4j-password{C_RESET} <pw>  Required for --dest neo4j (env: UG_NEO4J_PASSWORD)");
    println!("  {C_CYAN}--neo4j-database{C_RESET} <db>  Default: neo4j (env: UG_NEO4J_DATABASE)");
    println!("  See {C_BOLD}docs/MULTI-DEST.md{C_RESET} for the GDS / APOC capability matrix and Neo4j schema.");
    println!();
    println!("{C_BOLD}Embedding (defaults to in-process, no service needed):{C_RESET}");
    println!("  {C_CYAN}--model{C_RESET} <name>      Model. For local: a fastembed alias (see below).");
    println!("                          For remote: the model field sent to /v1/embeddings.");
    println!("                          Default: bge-small-en-v1.5 (384d, ~130 MB download).");
    println!("  {C_CYAN}--base-url{C_RESET} <url>    {C_YELLOW}Switches to remote backend.{C_RESET} OpenAI-compatible");
    println!("                          /v1/embeddings endpoint (e.g. http://localhost:8000/v1).");
    println!("  {C_CYAN}--api-key{C_RESET} <key>     Bearer token for the remote endpoint (default: 1234).");
    println!("  {C_CYAN}--embedding-dim{C_RESET} <n>  Override vector dim. Auto-probed otherwise; persisted to");
    println!("                          <db>/ug-meta.json on first ingest.");
    println!();
    println!("{C_BOLD}Local model aliases:{C_RESET}");
    println!("  bge-small-en-v1.5 (default)  bge-base-en-v1.5  bge-large-en-v1.5");
    println!("  all-MiniLM-L6-v2  all-MiniLM-L12-v2  nomic-embed-text-v1.5");
    println!("  multilingual-e5-small/base/large  bge-small-zh-v1.5  jina-embeddings-v2-base-code");
    println!("  mxbai-embed-large-v1");
    println!("  Cache: $UG_MODEL_CACHE → $XDG_CACHE_HOME/ug/models → ~/Library/Caches/ug/models (macOS)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET}                                      {C_YELLOW}# local, default model, ~/.ug/<cwd>{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --model nomic-embed-text-v1.5             {C_YELLOW}# local, larger model{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --base-url https://api.openai.com/v1 \\");
    println!("            --api-key $OPENAI_API_KEY --model text-embedding-3-small  {C_YELLOW}# remote{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --dest neo4j \\");
    println!("            --neo4j-uri neo4j://localhost:7687 --neo4j-user neo4j \\");
    println!("            --neo4j-password $NEO4J_PASSWORD                           {C_YELLOW}# Neo4j only{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --dest overgraph,neo4j \\");
    println!("            --neo4j-uri neo4j://localhost:7687 \\");
    println!("            --neo4j-user neo4j --neo4j-password $NEO4J_PASSWORD        {C_YELLOW}# fan-out{C_RESET}");
}

fn print_semantic_search_help() {
    println!("  {C_CYAN}ug semantic_search{C_RESET}  {C_YELLOW}— semantic vector search (OverGraph, no graph context){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Search by {C_BOLD}meaning{C_RESET}: describe what the code does (\"oauth login flow\") and get");
    println!("  the closest symbols by embedding similarity. Needs an ingested db ({C_CYAN}ug gen{C_RESET})");
    println!("  and an embedding endpoint. If you already know the identifier's name, use");
    println!("  {C_CYAN}ug find_symbols{C_RESET} (exact, no embeddings); for search {C_BOLD}plus{C_RESET} related-code context,");
    println!("  use {C_CYAN}ug search{C_RESET}.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug semantic_search <query> [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>   Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>     Top-k results (default: 10)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>      Optional SQL WHERE clause");
    println!("  {C_CYAN}--base-url/--api-key/--model/--embedding-dim{C_RESET}  Embedding endpoint overrides");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>  Output file (optional, omit for stdout)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug semantic_search{C_RESET} \"oauth login flow\"");
}

fn print_hybrid_search_help() {
    println!(
        "  {C_BOLD}{C_YELLOW}★ ug search{C_RESET}  {C_YELLOW}— GraphRAG: semantic search → graph expansion → ranked context{C_RESET}"
    );
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  The most complete search: semantic seeds ({C_CYAN}semantic_search{C_RESET}) expanded along");
    println!("  graph edges, then ranked into one context bundle with source snippets —");
    println!("  what the MCP {C_BOLD}search{C_RESET} tool runs for agents. Best when you want to hand");
    println!("  code + its related code to an LLM, or answer \"where is X and what touches it\".");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug search <query> [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>    Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>      Final results (default: 8)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>       SQL WHERE clause for semantic seed filter");
    println!("  {C_CYAN}--direction{C_RESET} <dir>    outbound|inbound|both (default: both)");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>  Restrict expansion to edge type (repeatable)");
    println!("  {C_CYAN}--max-chars{C_RESET} <n>      Char budget for assembled context (default: 12000)");
    println!("  {C_CYAN}--no-snippets{C_RESET}        Skip reading source snippets from disk");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>   Repo root for snippet resolution (default: cwd)");
    println!("  {C_CYAN}--base-url/--api-key/--model/--embedding-dim{C_RESET}  Embedding endpoint overrides");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>  Output file (optional, omit for stdout)");
    println!();
    println!("{C_DIM}Ranking is Personalized PageRank over the edge graph, seeded by RRF");
    println!("(vector + full-text). Its tuning knobs (--strategy, --hops, --mmr-lambda,");
    println!("--ppr-*) still parse but are undocumented operator controls — the defaults");
    println!("are what you want. Backends without native PPR (Neo4j without GDS) fall back");
    println!("to MMR automatically.{C_RESET}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug search{C_RESET} \"oauth login flow\" -k 8");
}

fn print_traverse_help() {
    println!("  {C_CYAN}ug traverse{C_RESET}  {C_YELLOW}— K-hop BFS using the OverGraph edges table{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug traverse <node-id>... [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>    Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}-k, --hops{C_RESET} <n>       Max hops (default: 2)");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>  Output file (optional, omit for stdout)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug traverse{C_RESET} \"file:src/index.ts\"");
    println!("  {C_CYAN}ug traverse{C_RESET} <id1> <id2>   {C_YELLOW}# batch: one merged traversal from several seeds{C_RESET}");
}

fn print_list_help() {
    println!("  {C_BOLD}{C_GREEN}★ ug list{C_RESET}  {C_YELLOW}— list generated projects{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug list");
    println!();
    println!("  Lists every project under ~/.ug (or $UG_HOME), with node/edge counts");
    println!("  and last-updated time. The current directory's project is marked with {C_BOLD}*{C_RESET}.");
}

fn print_api_help() {
    println!("  {C_CYAN}ug api{C_RESET}  {C_YELLOW}— list every HTTP endpoint `ug serve` exposes{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug api [--json]");
    println!();
    println!("  Prints a reference table of every route registered by {C_CYAN}ug serve{C_RESET}'s");
    println!("  HTTP server: method, path, what it does, when it 503s/is empty, and");
    println!("  whether the same capability also exists as a plain CLI subcommand");
    println!("  that works without a server running.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--json{C_RESET}  Emit the same listing as machine-readable JSON");
}

fn print_doctor_help() {
    println!("  {C_CYAN}ug doctor{C_RESET}  {C_YELLOW}— show resolved config and where each value came from{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug doctor [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>  Project name to resolve (default: cwd basename)");
    println!("  {C_CYAN}-d, --db{C_RESET} <path>    DB path override to resolve against");
    println!("  {C_CYAN}--base-url/--api-key/--model{C_RESET}  Embedding flags, shown with resolution source");
    println!("  {C_CYAN}--chat-base-url/--chat-api-key/--chat-model{C_RESET}  Same, for chat");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug doctor{C_RESET}");
}

fn print_chat_help() {
    println!(
        "  {C_BOLD}{C_MAGENTA}💬 ug chat{C_RESET}  {C_YELLOW}— RAG-grounded chat against the knowledge graph{C_RESET}"
    );
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!(
        "  {C_CYAN}query{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}hybrid retrieval (PPR){C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}LLM completion{C_RESET}"
    );
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug chat [\"<one-shot prompt>\"] [options]");
    println!("  Omit the prompt to drop into an interactive REPL with conversational history.");
    println!();
    println!("{C_BOLD}Retrieval (matches `ug search`):{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>        Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>          Context items to retrieve (default: 8)");
    println!("  {C_CYAN}--direction{C_RESET} <dir>        outbound|inbound|both (default: both)");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>      Restrict expansion to edge type (repeatable)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>           Optional SQL WHERE clause for the seed filter");
    println!("  {C_CYAN}--max-chars{C_RESET} <n>          Context char budget (default: 12000)");
    println!("  {C_CYAN}--no-snippets{C_RESET}            Don't read source snippets from disk");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>       Repo root for snippet resolution (default: cwd)");
    println!();
    println!("{C_BOLD}Chat model:{C_RESET}");
    println!("  {C_CYAN}--chat-model{C_RESET} <name>      Chat completion model (e.g. gpt-4o-mini)");
    println!("  {C_CYAN}--base-url{C_RESET} <url>         OpenAI-compatible base URL (shared by chat + embeddings)");
    println!("  {C_CYAN}--api-key{C_RESET} <key>          Bearer token (shared by chat + embeddings)");
    println!("  {C_CYAN}--chat-base-url{C_RESET} <url>    Override base URL for chat only");
    println!("  {C_CYAN}--chat-api-key{C_RESET} <key>     Override bearer token for chat only");
    println!("  {C_CYAN}--temperature{C_RESET} <f>        Sampling temperature (default: 0.2)");
    println!("  {C_CYAN}--max-tokens{C_RESET} <n>         Max completion tokens (default: 1024)");
    println!("  {C_CYAN}--chat-timeout{C_RESET} <secs>    HTTP timeout for chat calls (default: 180)");
    println!("  {C_CYAN}--system{C_RESET} <text>          Override the default RAG system prompt");
    println!("  {C_DIM}Persist any of these once with `ug config set chat.model …` — flags/env vars still win.{C_RESET}");
    println!();
    println!("{C_BOLD}Embedding (for retrieval; in-process by default):{C_RESET}");
    println!("  {C_CYAN}--embedding-model{C_RESET} <name>   Embedding model (falls back to --model)");
    println!("  {C_CYAN}--embedding-base-url{C_RESET} <url> Override base URL for embeddings only");
    println!("  {C_CYAN}--embedding-api-key{C_RESET} <key>  Override bearer token for embeddings only");
    println!("  {C_CYAN}--embedding-dim{C_RESET} <n>        Vector dim override (auto-probed otherwise)");
    println!();
    println!("{C_BOLD}Output:{C_RESET}");
    println!("  {C_CYAN}--json{C_RESET}                   Emit a single JSON document (answer + citations)");
    println!("  {C_CYAN}--show-context, -v{C_RESET}       Print the retrieved citations alongside the answer");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>      Write the answer (or JSON) to a file");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_MAGENTA}ug chat{C_RESET} \"how does graph ingest work?\" \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("    --chat-model Qwen3.6-35B-A3B-MLX-8bit \\");
    println!("    --embedding-model Qwen3-Embedding-4B-4bit-DWQ");
    println!();
    println!("  {C_MAGENTA}ug chat{C_RESET} --json -v \\");
    println!("    \"explain the PPR seed pool logic\" \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --chat-model my-model");
    println!();
    println!("  {C_MAGENTA}ug chat{C_RESET} \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --chat-model my-model     {C_YELLOW}# interactive REPL{C_RESET}");
}

fn print_gen_help() {
    println!(
        "  {C_BOLD}{C_MAGENTA}⚡ ug gen{C_RESET}  {C_YELLOW}— end-to-end knowledge graph pipeline{C_RESET}"
    );
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!(
        "  {C_CYAN}index{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}graph{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}visualization{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}OverGraph ingest{C_RESET}"
    );
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug gen [<path>] [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <path>       Input directory (default: .)");
    println!(
        "  {C_CYAN}-c, --cache{C_RESET} <dir>        Cache directory for incremental indexing"
    );
    println!(
        "  {C_CYAN}-n, --name{C_RESET} <name>        Project name (default: input dir basename)"
    );
    println!(
        "  {C_CYAN}-o, --output{C_RESET} <dir>       Output directory (default: ~/.ug/<name>)"
    );
    println!(
        "  {C_CYAN}-d, --db{C_RESET} <dir>           OverGraph directory (default: <output-dir>/ugdb)"
    );
    println!("  {C_YELLOW}--no-ingest{C_RESET}              Skip the OverGraph ingest step");
    println!("  {C_GREEN}--serve{C_RESET}                  Chain into 'ug serve' on the generated outputs");
    println!(
        "                            (inherits -p/--port, --host, --watch, --repo-root, embedder flags)"
    );
    println!();
    println!("{C_BOLD}Embedding (in-process by default; --base-url switches to remote):{C_RESET}");
    println!("  {C_CYAN}--model{C_RESET} <name>           Local fastembed alias or remote model name");
    println!("                              (default: bge-small-en-v1.5, 384d).");
    println!("  {C_CYAN}--base-url{C_RESET} <url>         {C_YELLOW}Opt into remote{C_RESET} /v1/embeddings endpoint.");
    println!("  {C_CYAN}--api-key{C_RESET} <key>          Bearer token for the remote endpoint.");
    println!(
        "  {C_CYAN}--embedding-dim{C_RESET} <n>      Override vector dim (auto-probed otherwise)."
    );
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET}                              {C_YELLOW}# ~/.ug/<cwd-basename>/{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src -n myrepo           {C_YELLOW}# ~/.ug/myrepo/{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src --no-ingest --serve");
}

fn print_logo() {
    println!();
    println!(
        "   {C_YELLOW}✦{C_RESET} {C_DIM}──────────────────────────────────────────{C_RESET} {C_YELLOW}✦{C_RESET}"
    );
    println!();
    println!(
        "     {C_BOLD}{C_CYAN}●{C_RESET}{C_DIM}───{C_RESET}{C_BOLD}{C_MAGENTA}●{C_RESET}    {C_BOLD}U L T R A  G R A P H{C_RESET}"
    );
    println!("     {C_DIM}│   │{C_RESET}    {C_DIM}·  code intelligence  ·{C_RESET}");
    println!(
        "     {C_BOLD}{C_BLUE}●{C_RESET}{C_DIM}───{C_RESET}{C_BOLD}{C_GREEN}●{C_RESET}"
    );
    println!();
    println!("     {C_DIM}the knowledge graph for your codebase & docs{C_RESET}");
    println!();
    println!(
        "   {C_YELLOW}✦{C_RESET} {C_DIM}──────────────────────────────────────────{C_RESET} {C_YELLOW}✦{C_RESET}"
    );
    println!();
}

fn print_help() {
    println!();
    println!("Usage: {C_BOLD}ug <command>{C_RESET} [options]");
    println!();
    println!("{C_BOLD}Quick start:{C_RESET}");
    println!("  {C_CYAN}ug gen{C_RESET}     Index this directory, build the graph, and ingest it (→ ~/.ug/<name>/)");
    println!("  {C_CYAN}ug app{C_RESET}     Explore the graph in a native desktop window (starts the server for you)");
    println!("  {C_CYAN}ug{C_RESET}         Bare `ug` starts the server (visualization + REST API at http://localhost:8080)");
    println!("{C_BOLD}MCP (Claude Code / Claude Desktop / Cursor / Windsurf / VS Code / Gemini CLI / Codex CLI / Hermes Agent / opencode):{C_RESET}");
    println!("  {C_CYAN}ug mcp install{C_RESET}            Wire the MCP server into a client config (interactive picker; or name a target, e.g. `ug mcp install claude`)");

    println!();
    println!("{C_BOLD}Commands:{C_RESET}");
    println!(
        "  {C_BOLD}{C_MAGENTA}gen{C_RESET}              {C_BOLD}{C_MAGENTA}⚡ full pipeline: index → graph → visualization → ingest ⚡{C_RESET}"
    );
    println!("  {C_CYAN}serve{C_RESET}            Serve the visualization + graph API");
    println!("  {C_CYAN}app{C_RESET}              Open the native desktop shell (starts serve + a window)");
    println!("  {C_CYAN}api{C_RESET}              List every HTTP endpoint `ug serve` exposes");
    println!();
    println!("  {C_DIM}Retrieval (OverGraph-backed){C_RESET}");
    println!(
        "  {C_BOLD}{C_YELLOW}search{C_RESET}           {C_YELLOW}GraphRAG: semantic search → graph expansion → ranked context{C_RESET}"
    );
    println!("  {C_CYAN}semantic_search{C_RESET}  Search by meaning/concept (embeddings; use find_symbols for exact names)");
    println!("  {C_CYAN}traverse{C_RESET}         K-hop BFS over the OverGraph edges table");
    println!(
        "  {C_BOLD}{C_MAGENTA}chat{C_RESET}             {C_BOLD}{C_MAGENTA}💬 GraphRAG-grounded chat (one-shot or REPL){C_RESET}"
    );
    println!();
    println!("  {C_DIM}Pipeline steps (gen runs these for you){C_RESET}");
    println!("  {C_CYAN}index{C_RESET}            Index a directory");
    println!("  {C_CYAN}graph{C_RESET}            Build graph from index result");
    println!("  {C_CYAN}ingest{C_RESET}           Embed graph nodes and write to OverGraph");
    println!();
    println!("  {C_DIM}Graph analysis (offline, in-memory) — all take {C_RESET}{C_CYAN}-n <project>{C_RESET}{C_DIM}, {C_RESET}{C_CYAN}--json{C_RESET}{C_DIM}, {C_RESET}{C_CYAN}-o <file>{C_RESET}");
    println!("  {C_CYAN}graph_analyze{C_RESET}    Full analysis (centrality + cycles) → analysis.json/cycles.json");
    println!("  {C_CYAN}graph_bfs{C_RESET}        K-hop BFS from a node/name (--hops, -d in|out|both)");
    println!("  {C_CYAN}graph_filter{C_RESET}     List edges by type/endpoint (no args: edge types + counts)");
    println!("  {C_CYAN}graph_centrality{C_RESET} Rank nodes by degree/betweenness (--top, -t, -f)");
    println!("  {C_CYAN}graph_cycles{C_RESET}     Detect cycles (--min-len, --fail-on-cycle for CI)");
    println!("  {C_CYAN}graph_search{C_RESET}     Substring scan over names + docstrings (prefer find_symbols)");
    println!();
    println!("  {C_DIM}Agent tools — what AI coding agents use (via MCP) to understand a repo; run by hand to explore or verify{C_RESET}");
    println!("  {C_CYAN}project_overview{C_RESET} Orient in the codebase: stats, biggest files, most depended-upon symbols");
    println!("  {C_CYAN}find_symbols{C_RESET}      Exact-name symbol lookup (no embeddings) — returns ids for the tools below");
    println!("  {C_CYAN}file_outline{C_RESET}     List every indexed symbol in one file, in line order");
    println!("  {C_CYAN}get_code{C_RESET}         Read the source for a node id or file/line range");
    println!("  {C_CYAN}find_usages{C_RESET}      Who uses this symbol? (inbound callers/importers + call sites)");
    println!("  {C_CYAN}shortest_path{C_RESET}    How two symbols are connected (directed edge path)");
    println!("  {C_CYAN}graph_schema{C_RESET}     Node & edge types in this graph — what to pass to --edge-type filters");
    println!("  {C_DIM}  All accept {C_RESET}{C_CYAN}--json{C_RESET}{C_DIM} and take the same names/params as the MCP tools.{C_RESET}");
    println!();

    println!("  {C_DIM}Project management{C_RESET}");
    println!("  {C_BOLD}{C_GREEN}list_projects{C_RESET}  {C_GREEN}List generated projects under ~/.ug (or $UG_HOME){C_RESET}");
    println!("  {C_CYAN}rm{C_RESET}               Delete a project's data directory");
    println!("  {C_CYAN}upgrade{C_RESET}          Check GitHub for a new release and self-update (`--check` to only report)");
    println!("  {C_CYAN}uninstall{C_RESET}        Delete ALL indexed projects and uninstall ug itself");
    println!("  {C_CYAN}config{C_RESET}           View/persist defaults (chat model, endpoints, …) in ~/.ug/config.json");
    println!("  {C_CYAN}doctor{C_RESET}           Show resolved project/db/embedder/chat config");
    println!();
    println!("Run {C_CYAN}ug <command> -h{C_RESET} for that command's options and examples.");
}
// test change
