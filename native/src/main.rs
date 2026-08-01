use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use ultragraph::storage::{
    self, open_store, search_kb as storage_search_kb,
    semantic_search as storage_semantic_search, DEFAULT_CONTEXT_CHARS, Direction, Embedder,
    EmbedderConfig, KnowledgeStore, RankStrategy, SearchKbOptions, StoreSet, StoreSpec,
    DEFAULT_BASE_URL as DEFAULT_EMBED_BASE_URL, DEFAULT_MODEL as DEFAULT_EMBED_MODEL,
};
use ultragraph::agent_tools::{
    self, by_id_map, looks_like_node_id, node_loc, node_type_str,
    strip_file_id_prefix, Render,
};
use ultragraph::limits::{BudgetSource, EmbedBudget};
use ultragraph::types::{GraphData, GraphNode, GraphNodeType};
use ultragraph::{
    build_graph, calculate_centrality, detect_cycles, index, index_with_cache, C_BLUE, C_BOLD,
    C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW,
};

mod chat;
mod config;
mod mcp;
mod project;
mod serve;
mod tour;

// Bundled visualization assets so `ug gen` can produce a self-contained
// output directory without needing the source tree at runtime.
//
// The page is assembled by `build.rs` from `src/vis/index.html` +
// `src/vis/css/*` + `src/vis/js/*` — edit those, not the output. It lands
// in OUT_DIR rather than the source tree precisely so there is no
// generated copy sitting around to edit by mistake.
pub(crate) const VIS_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/visualization.html"));
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

    // `--no-logo` is consumed here rather than passed through, so no
    // subcommand's argument parser can mistake it for a positional.
    let mut args: Vec<String> = env::args().collect();
    let logo_flagged_off = args.iter().any(|a| a == "--no-logo" || a == "--quiet-logo");
    args.retain(|a| a != "--no-logo" && a != "--quiet-logo");
    let args = args;

    if !suppress_logo(&args, logo_flagged_off) {
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
        "regen" => run_regen(cmd_args),
        "serve" => serve::run_serve(cmd_args),
        "app" => run_app(cmd_args),
        "api" => run_api(cmd_args),
        // Pipeline steps `gen` runs for you.
        "index" => run_index(cmd_args),
        "graph" => run_graph(cmd_args),
        "ingest" => run_ingest(cmd_args),
        // Structural analysis. What is left here is what nothing else
        // can do: betweenness centrality needs all-pairs shortest paths,
        // and cycle detection needs an unbounded DFS — neither is
        // expressible as a query.
        "graph_centrality" => run_graph_centrality(cmd_args),
        "graph_cycles" => run_graph_cycles(cmd_args),
        // Agent tools (graph.json-backed, for AI coding agents). Names match
        // the MCP tools one-for-one.
        "find_symbols" => run_find_symbols(cmd_args),
        "file_outline" => run_file_outline(cmd_args),
        "get_code" => run_get_code(cmd_args),
        "find_usages" => run_find_usages(cmd_args),
        "project_overview" => run_project_overview(cmd_args),
        "shortest_path" => run_graph_path(cmd_args),
        "graph_schema" => run_graph_schema(cmd_args),
        "query" => run_code_query(cmd_args),
        // Retrieval (OverGraph-backed).
        "semantic_search" => run_semantic_search(cmd_args),
        "search" => run_hybrid_search(cmd_args),
        "traverse" => run_traverse(cmd_args),
        "chat" => run_chat(cmd_args),
        "tour" => run_tour(cmd_args),
        // Project management.
        // `list` is the command; `list_projects` stays because it is the MCP
        // tool's name, and the agent-tool commands are documented as taking
        // the same names as the tools.
        "list" | "ls" | "list_projects" => run_list(cmd_args),
        "active" => run_active(cmd_args),
        "rm" => run_rm(cmd_args),
        "uninstall" => run_uninstall(cmd_args),
        "upgrade" => run_upgrade(cmd_args),
        "config" => run_config(cmd_args),
        "doctor" => run_doctor(cmd_args),
        "connect" => run_connect(cmd_args),
        "disconnect" => run_disconnect(cmd_args),
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
    Config(&'static str),
    Default,
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

/// Resolve how much of each node's description may be embedded, and say so.
///
/// The number comes from the model's token window unless `--section-cap`
/// (or a persisted `embed.section_cap`) pins it. Announcing it matters
/// because the alternative is invisible: text past the budget is dropped
/// with no marker in the output, and the user chose the model that decided
/// the number. Any mismatch between the two is printed as a warning.
pub(crate) fn budget_from_args(embedder: &Embedder, args: &[String]) -> EmbedBudget {
    let (raw, _) = config::resolve_pref_cfg(flag_value(args, &["--section-cap"]), "embed.section_cap");
    let override_chars = raw.and_then(|s| s.parse::<usize>().ok());
    let model = &embedder.config().model;
    let budget = EmbedBudget::resolve(model, override_chars);

    let window = match budget.window_tokens {
        Some(t) => format!("{} token window", t),
        None => "unknown window".to_string(),
    };
    let origin = match budget.source {
        BudgetSource::Flag => "pinned by --section-cap",
        BudgetSource::Auto => "derived from the model",
        BudgetSource::Default => "default — model window unknown",
    };
    eprintln!(
        "{C_CYAN}▸{C_RESET} Embedding budget: {C_BOLD}{}{C_RESET} chars per description ({}, {})",
        budget.description_chars, window, origin
    );
    for advice in [budget.advisory(model), budget.related_advisory()]
        .into_iter()
        .flatten()
    {
        eprintln!("{C_YELLOW}⚠{C_RESET}  {}", advice);
    }
    budget
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

/// What an ingest run actually produced.
///
/// Carries the degraded case explicitly rather than folding it into
/// `Err`: a run whose embedder died still wrote a complete structural
/// index, so it is neither a success nor a failure, and reporting it as
/// either misleads. The caller needs both facts to say something true.
pub(crate) struct IngestOutcome {
    nodes: usize,
    edges: usize,
    /// Set when nodes were written without vectors because embedding
    /// failed. Semantic search will miss them until the next run.
    embedding_error: Option<String>,
}

/// Open a store, exiting cleanly on the one failure every user hits at
/// least once.
///
/// A store written by an older ug is an expected, actionable state after
/// an upgrade — not a bug. Reporting it through `panic!` buries a
/// perfectly good "run `ug regen`" message under a backtrace notice and
/// makes a routine migration look like a crash. Every other failure keeps
/// panicking, because it is one.
async fn open_store_or_exit(spec: &StoreSpec) -> Box<dyn KnowledgeStore> {
    match storage::open_store(spec).await {
        Ok(store) => store,
        Err(e @ storage::store::StoreError::StoreFormatMismatch { .. }) => {
            eprintln!("\n{C_BOLD}Index out of date{C_RESET}\n\n{}", e);
            std::process::exit(1);
        }
        Err(e) => panic!("failed to open {} store: {}", spec.name(), e),
    }
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

// ---------- Agent tools ----------
//
// The MCP server (`ug mcp`, see src/mcp/) exposes graph.json-backed tools that
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
    println!("  {C_CYAN}ug find_symbols{C_RESET} embedder --include-docs   {C_YELLOW}# also scan docstrings{C_RESET}");

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
    println!("  {C_CYAN}ug file_outline{C_RESET} 'file:native/src/main.rs'  {C_YELLOW}# direct nodeId lookup (O(1)){C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} main.rs serve.rs config.rs   {C_YELLOW}# batch: outline several files at once{C_RESET}");
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

/// `ug query` — whole-repo statistics, the CLI half of the `code_query`
/// MCP tool.
///
/// Store-backed rather than graph.json-backed, unlike its neighbours in
/// this section: aggregation and reachability need indexed properties.
/// It still needs no embedder, so the dim comes off the store's own
/// manifest instead of a model probe — statistics should not depend on
/// an embedding backend being reachable.
fn run_code_query(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_code_query_help();
        return;
    }
    if has_flag(args, "--list") || args.is_empty() {
        print_presets();
        return;
    }

    let params = match code_query_params_from_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
    };

    let rt = tokio_runtime();
    rt.block_on(async {
        let mut spec = single_store_spec_from_args(args, 0);
        if let StoreSpec::Overgraph { path, .. } = &spec {
            let dim = storage::db::stored_embedding_dim(path)
                .unwrap_or(storage::embed::DEFAULT_EMBEDDING_DIM as u32);
            spec.set_embedding_dim(dim);
        } else {
            // Neo4j has no local manifest to read, and no GQL support
            // behind this trait either — the error below says so.
            spec.set_embedding_dim(storage::embed::DEFAULT_EMBEDDING_DIM as u32);
        }
        let store = open_store_or_exit(&spec).await;

        match ultragraph::code_query::run(store.as_ref(), &params).await {
            Ok(answer) => {
                println!(
                    "{}",
                    ultragraph::code_query::render::render(&answer, Render::Ansi)
                );
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    });
}

/// Parse `ug query`'s flags into the shared params struct.
///
/// Split out from [`run_code_query`] so it can be tested: the positional
/// preset makes this the fiddliest argument parsing in the CLI, and the
/// failure it produces — a flag's *value* read as a preset name — surfaces
/// as a confusing "preset and gql together" error rather than anything
/// resembling its cause.
fn code_query_params_from_args(
    args: &[String],
) -> Result<ultragraph::code_query::CodeQueryParams, String> {
    let gql = flag_value(args, &["-g", "--gql"]);
    let preset = flag_value(args, &["-p", "--preset"]).or_else(|| {
        // Only infer a positional preset when no query was given —
        // otherwise a stray value would be read as a second query.
        if gql.is_some() {
            return None;
        }
        first_positional(
            args,
            &[
                "-p", "--preset", "-g", "--gql", "-a", "--arg", "-n", "--name", "-k", "--limit",
                "-r", "--range",
                // `-o` carries the store path on this command, as it does
                // on every other store-backed one. Leaving it out made
                // `ug query <preset> -o <path>` read the path as a second
                // preset.
                "-o", "--output",
                "--dest", "--neo4j-uri", "--neo4j-user", "--neo4j-password", "--neo4j-database",
            ],
        )
    });

    let mut query_args = std::collections::BTreeMap::new();
    for pair in multi_flag(args, &["-a", "--arg"]) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("--arg expects key=value, got '{}'", pair))?;
        query_args.insert(k.trim().to_string(), v.trim().to_string());
    }

    Ok(ultragraph::code_query::CodeQueryParams {
        preset,
        gql,
        args: query_args,
        limit: flag_value(args, &["-k", "--limit"]).and_then(|s| s.parse().ok()),
        range: flag_value(args, &["-r", "--range"]),
    })
}

fn print_presets() {
    println!("  {C_CYAN}ug query{C_RESET}  {C_YELLOW}— built-in questions{C_RESET}");
    println!();
    let mut category = "";
    for p in ultragraph::code_query::presets::all() {
        if p.category.as_str() != category {
            category = p.category.as_str();
            println!("{C_BOLD}{}{C_RESET}", category);
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
        println!("  {C_CYAN}{:<26}{C_RESET} {}", p.name, p.description);
        if !args.is_empty() {
            println!("  {:<26} {C_DIM}args: {}{C_RESET}", "", args.join(", "));
        }
    }
    println!();
    println!("  Run one:  {C_CYAN}ug query <preset> [--arg key=value]{C_RESET}");
    println!("  Raw GQL:  {C_CYAN}ug query --gql \"MATCH (n:Function) RETURN count(*) AS c\"{C_RESET}");
}

fn print_code_query_help() {
    println!("  {C_CYAN}ug query{C_RESET}  {C_YELLOW}— whole-repo statistics over the indexed graph{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Counts, groups, distributions and blast radius — the questions that");
    println!("  would otherwise mean grepping every file. Same engine as the");
    println!("  {C_CYAN}code_query{C_RESET} MCP tool. Read-only.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug query <preset> [options]");
    println!("        ug query --gql \"<query>\" [options]");
    println!("        ug query --list");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-p, --preset <name>{C_RESET}    Built-in question to run (also accepted as a positional)");
    println!("  {C_CYAN}-a, --arg <k=v>{C_RESET}        Preset argument, repeatable (e.g. --arg target=src/a.ts)");
    println!("  {C_CYAN}-g, --gql <query>{C_RESET}      Raw OverGraph GQL, when no preset fits");
    println!("  {C_CYAN}-k, --limit <n>{C_RESET}        Rows to display (default 20) — shorthand for --range 1-N");
    println!("  {C_CYAN}-r, --range <window>{C_RESET}   Which rows to show, 1-based and inclusive:");
    println!("                         {C_DIM}20 · 11-35 · 34-end{C_RESET} — page a result without re-reading it");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}   Project to query (default: the active one)");
    println!("      {C_CYAN}--list{C_RESET}             List every preset and exit");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_DIM}# how many functions are longer than 50 lines, and where{C_RESET}");
    println!("  ug query long_functions_by_folder");
    println!();
    println!("  {C_DIM}# raise the threshold{C_RESET}");
    println!("  ug query long_functions --arg min_loc=150");
    println!();
    println!("  {C_DIM}# what breaks if I change this file{C_RESET}");
    println!("  ug query impact --arg target=native/src/storage/store.rs");
    println!();
    println!("  {C_DIM}# page through a long result without re-reading what you saw{C_RESET}");
    println!("  ug query dead_code --range 21-40");
    println!();
    println!("  {C_DIM}# anything the presets don't cover{C_RESET}");
    println!("  ug query --gql \"MATCH (n:Function) WHERE n.params > 6 RETURN count(*) AS c\"");
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
    println!("  filters to {C_CYAN}ug find_usages{C_RESET} / {C_CYAN}ug traverse{C_RESET} — filtering on a type the graph");
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
/// Decide which directory `ug gen` should use as its incremental parse
/// cache: `None` disables caching and forces a full re-parse.
///
/// Precedence: `--no-cache` → `-c/--cache` → the output dir (default).
/// A cache written by a different `ug` version is discarded rather than
/// trusted — `indexed-tree.json` holds parsed `FileNode`s, so an
/// indexer change between versions would otherwise keep serving nodes
/// in the old shape for every file whose content happened not to change.
fn resolve_gen_cache(args: &[String], output_dir: &str) -> Option<String> {
    if has_flag(args, "--no-cache") {
        return None;
    }
    if let Some(explicit) = flag_value(args, &["-c", "--cache"]) {
        return Some(explicit);
    }
    let this_version = env!("CARGO_PKG_VERSION");
    if let Some(meta) = project::read_meta(Path::new(output_dir)) {
        if !meta.ug_version.is_empty() && meta.ug_version != this_version {
            println!(
                "{C_YELLOW}▸{C_RESET} Index was built by ug {} (now {}) — re-parsing from scratch.",
                meta.ug_version, this_version
            );
            return None;
        }
    }
    Some(output_dir.to_string())
}

/// `ug regen [-n <project>]` — re-run the pipeline for a project that has
/// already been generated once.
///
/// This is `ug gen` with the input path remembered rather than retyped:
/// it reads `repoRoot` out of the project's `project.json` and hands off.
/// Everything else — the content-hash cache, the progress output, the
/// degraded-embedder path — is `gen`'s, because re-running the pipeline
/// and running it the first time are the same operation.
///
/// **On the name.** This indexes, rebuilds the graph *and* re-embeds into
/// the store, so `reindex` — which names only the first of the three —
/// would be a lie. `regen` says what it does and pairs with the `gen` it
/// repeats.
fn run_regen(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_regen_help();
        return;
    }

    // Same resolution order every project-scoped command uses: -n/--name,
    // then the active project, then the cwd's basename.
    let name = flag_value(args, &["-n", "--name"])
        .map(|n| project::sanitize_name(&n))
        .or_else(project::get_active_project)
        .unwrap_or_else(|| project::derive_project_name("."));

    let dir = project::project_dir(&name);
    let Some(meta) = project::read_meta(&dir) else {
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  No generated project named {C_BOLD}{}{C_RESET} under {}.",
            name,
            project::ug_home().display()
        );
        eprintln!(
            "   Run {C_CYAN}ug gen -i <path>{C_RESET} to create it, or {C_CYAN}ug list{C_RESET} to see what exists."
        );
        std::process::exit(1);
    };

    if meta.repo_root.is_empty() || !Path::new(&meta.repo_root).exists() {
        // The recorded path is how regen knows what to re-read; without a
        // usable one there is nothing to re-run against, and guessing the
        // cwd would silently re-index the wrong tree.
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  Project {C_BOLD}{}{C_RESET} points at {}, which no longer exists.",
            name,
            if meta.repo_root.is_empty() {
                "(no recorded path)"
            } else {
                &meta.repo_root
            }
        );
        eprintln!("   Re-run {C_CYAN}ug gen -i <path> -n {}{C_RESET} to repoint it.", name);
        std::process::exit(1);
    }

    println!(
        "{C_CYAN}▸{C_RESET} Regenerating {C_BOLD}{}{C_RESET} from {}",
        name, meta.repo_root
    );

    // Forward the caller's flags (embedder overrides, --no-ingest, …) and
    // add the two `gen` needs, unless they were already supplied.
    let mut forwarded: Vec<String> = args.to_vec();
    if flag_value(args, &["-i", "--input"]).is_none() {
        forwarded.push("-i".into());
        forwarded.push(meta.repo_root.clone());
    }
    if flag_value(args, &["-n", "--name"]).is_none() {
        forwarded.push("-n".into());
        forwarded.push(name);
    }
    run_gen(&forwarded);
}

fn print_regen_help() {
    println!("  {C_CYAN}ug regen{C_RESET}  {C_YELLOW}— re-run the pipeline for an existing project{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  {C_CYAN}ug gen{C_RESET} with the input path remembered instead of retyped — it reads");
    println!("  the repo root from the project's own metadata. Incremental: unchanged");
    println!("  files are skipped via content hashes, so this is cheap after a few edits.");
    println!();
    println!("  Runs the whole pipeline (index → graph → embed), which is why it is");
    println!("  {C_BOLD}regen{C_RESET} and not {C_DIM}reindex{C_RESET} — though {C_CYAN}ug reindex{C_RESET} still works.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug regen [-n <project>] [gen options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}   Project to regenerate (default: the active one)");
    println!("  {C_CYAN}--no-ingest{C_RESET}            Rebuild graph.json only, skip embedding");
    println!("      {C_DIM}…plus every {C_RESET}{C_CYAN}ug gen{C_RESET}{C_DIM} option — see {C_RESET}{C_CYAN}ug gen -h{C_RESET}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  ug regen                    {C_DIM}# the active project{C_RESET}");
    println!("  ug regen -n myrepo");
    println!("  ug regen --no-ingest        {C_DIM}# structure only, no embedder needed{C_RESET}");
}

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
    let project_name = project::resolve_project_name(args, &input);
    let output_dir = flag_value(args, &["-o", "--output"])
        .unwrap_or_else(|| project::project_dir(&project_name).to_string_lossy().into_owned());
    // Parse caching is on by default, keyed to the project dir — which
    // already holds the `indexed-tree.json` snapshot `index_with_cache`
    // needs to restore a cached file's nodes, so enabling it costs one
    // extra file (`cache.json`) and nothing else. Re-indexing an
    // unchanged repo is the common case (`ug gen` again, the KB
    // Manager's re-index button), and re-parsing every file for it is
    // pure waste: the cache only skips *per-file* tree-sitter work, and
    // cross-file resolution still runs fresh in `build_graph`, so it
    // cannot produce stale edges.
    //
    // It is bypassed when the snapshot was written by a different `ug`
    // build, since a parser or extractor change would otherwise be
    // silently masked by cached nodes in the old shape.
    let cache = resolve_gen_cache(args, &output_dir);
    let no_ingest = has_flag(args, "--no-ingest");
    let chain_serve = has_flag(args, "--serve");
    // Full precedence here: -d/--db flag → <output-dir>/ugdb.
    // run_gen_ingest then pins the default OverGraph spec to this path.
    let db_path = flag_value(args, &["-d", "--db"])
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

    // How every call site was resolved, or why it wasn't. Printed because a
    // call graph's quality is otherwise unmeasurable: a wrong edge and a
    // right edge look the same in a total, and the way this fails is the
    // confident wrong answer. `dropped` is mostly healthy — a call into the
    // standard library belongs there — but a jump in it between two runs of
    // the same repo means resolution regressed.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&graph) {
        if let Some(r) = v.get("resolution") {
            let n = |k: &str| r.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let (q, t, b, d) = (
                n("resolvedQualified"),
                n("resolvedTyped"),
                n("resolvedByName"),
                n("droppedUnresolved"),
            );
            println!(
                "  calls: {} resolved ({} by path, {} by receiver type, {} by name), {} unresolved",
                q + t + b,
                q,
                t,
                b,
                d
            );
        }
    }

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
            "Run '{C_BOLD}ug serve{C_RESET}' and open {C_CYAN}http://127.0.0.1:8080{C_RESET}"
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
        Ok(out) if out.embedding_error.is_some() => {
            // Not a success. The index is real and queryable, but calling
            // it "embedded" would be false, and the user needs to know a
            // re-run is owed before semantic search is trustworthy.
            println!(
                "  {C_YELLOW}⚠ {} nodes, {} edges{C_RESET} indexed {C_BOLD}without vectors{C_RESET} in {C_BOLD}{:?}{C_RESET}",
                out.nodes,
                out.edges,
                t3.elapsed()
            );
            println!(
                "    Structure and statistics work now. Re-run once the embedder is up to enable semantic search:"
            );
            println!("      ug ingest -i {} -o {}", graph_path, db_path);
        }
        Ok(out) => {
            println!(
                "  {C_GREEN}✓ {} nodes, {} edges{C_RESET} embedded in {C_BOLD}{:?}{C_RESET}",
                out.nodes,
                out.edges,
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
            "Run '{C_BOLD}ug serve{C_RESET}' and open {C_CYAN}http://127.0.0.1:8080{C_RESET} to view the graph."
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
    prune: bool,
    budget: &EmbedBudget,
) -> Result<IngestOutcome, String> {
    let nodes_count = graph.nodes.len();
    let edges_count = graph.edges.len();

    let t0 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Building node texts ({})", nodes_count);
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Capture first: node texts fold in each node's own comments, and the
    // same captured source is written to the rows further down.
    let captured = storage::capture_for_graph(graph);
    let texts = storage::build_texts(graph, &captured, budget);
    // Corpus statistics for BM25 keyword weighting. Must land before the
    // upsert — they decide which terms survive each node's dimension cap.
    let stats = storage::refresh_sparse_stats(&[store], &texts, &captured, graph);
    println!(
        "\r{C_CYAN}▸{C_RESET} Building node texts: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET} — {} keyword terms tracked",
        t0.elapsed(),
        stats.terms()
    );

    // Diff against what's already stored so a re-index only pays for what
    // actually changed. On a first run everything lands in `to_embed` and
    // this costs one cheap miss per node.
    let tp = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Diffing against Graph DB ({})", nodes_count);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let model = embedder.config().model.clone();
    let plan = storage::plan_incremental_ingest(store, graph, &texts, false, &captured, Some(&model))
        .await
        .map_err(|e| format!("reading existing nodes: {}", e))?;
    let to_embed = plan.to_embed.len();
    println!(
        "\r{C_CYAN}▸{C_RESET} Diffing against Graph DB: {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET} — {} unchanged, {} moved, {} to embed",
        tp.elapsed(),
        plan.unchanged,
        plan.reusable.len(),
        to_embed
    );

    let t1 = std::time::Instant::now();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(to_embed);
    // A failed embed degrades rather than aborts. Everything else in this
    // pipeline is already computed — structure, source, and every derived
    // fact — and discarding it leaves the user with no index at all when
    // they could have had one that answers structural and statistical
    // questions. Nodes past the failure are written with no vector; the
    // next run sees a missing vector and backfills it.
    let mut embed_error: Option<String> = None;
    if to_embed == 0 {
        println!("{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}✓ skipped{C_RESET} (no node text changed)");
    } else {
        print!("{C_CYAN}▸{C_RESET} Embedding nodes ({})", to_embed);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        for (i, chunk) in plan.to_embed.chunks(embedder.config().batch_size).enumerate() {
            let chunk_vec: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            match embedder.embed(&chunk_vec).await {
                Ok(chunk_vectors) => vectors.extend(chunk_vectors),
                Err(e) => {
                    embed_error = Some(e.to_string());
                    break;
                }
            }
            let processed = std::cmp::min((i + 1) * embedder.config().batch_size, to_embed);
            let pct = processed as f32 / to_embed as f32 * 100.0;
            print!(
                "\r{C_CYAN}▸{C_RESET} Embedding: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
                pct, processed, to_embed
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        match &embed_error {
            None => println!(
                "\r{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
                t1.elapsed()
            ),
            Some(e) => {
                let done = vectors.len();
                println!(
                    "\r{C_YELLOW}⚠{C_RESET} Embedding failed after {}/{} — {}",
                    done, to_embed, e
                );
                println!(
                    "  Writing the remaining {} node(s) without vectors: structure and \
                     statistics still work, semantic search will miss them until the next run.",
                    to_embed - done
                );
                vectors.resize(to_embed, Vec::new());
            }
        }
    }

    let t2 = std::time::Instant::now();
    let node_rows = plan.finish(graph, vectors, &captured)?;

    let write_batch = 1000;
    let total = node_rows.len();
    if total == 0 {
        println!("{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}✓ skipped{C_RESET} (all {} already up to date)", nodes_count);
    } else {
        print!("{C_CYAN}▸{C_RESET} Writing nodes to Graph DB");
        let _ = std::io::Write::flush(&mut std::io::stdout());
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
            "\r{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET} ({}/{} changed)",
            t2.elapsed(),
            total,
            nodes_count
        );
    }

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

    if prune {
        let t4 = std::time::Instant::now();
        let removed = storage::prune_to_graph(store, graph)
            .await
            .map_err(|e| format!("prune stale nodes: {}", e))?;
        if removed > 0 {
            println!(
                "{C_CYAN}▸{C_RESET} Pruning stale nodes: {C_GREEN}✓ removed {}{C_RESET} in {C_BOLD}{:?}{C_RESET}",
                removed,
                t4.elapsed()
            );
        }
    }

    store.ensure_query_indexes();

    // Stamp the model last, so a run that died mid-write doesn't claim its
    // vectors all came from this one. Skipped entirely when embedding
    // failed: the stamp says "these vectors are current for this model",
    // which would be a lie about rows that have no vectors, and would stop
    // the next run from re-embedding them.
    if embed_error.is_none() {
        store.record_ingest_model(&model);
    }

    Ok(IngestOutcome {
        nodes: nodes_count,
        edges: edges_count,
        embedding_error: embed_error,
    })
}

fn run_gen_ingest(
    graph_json: &str,
    db_path: &str,
    args: &[String],
) -> Result<IngestOutcome, String> {
    let graph: GraphData =
        serde_json::from_str(graph_json).map_err(|e| format!("parse graph: {}", e))?;
    // Ingest upserts, so a node dropped from the source would otherwise
    // linger in the store and keep surfacing in search. Pruning is on by
    // default because `ug gen` indexes a whole repo; `--no-prune` is for
    // the case where several inputs deliberately share one project dir.
    let prune = !has_flag(args, "--no-prune");
    let mut embedder = embedder_from_args(args);
    let budget = budget_from_args(&embedder, args);
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
        // (-d/--db → <output-dir>/ugdb), so pin the
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
        ingest_with_specs(&specs, &embedder, &graph, prune, &budget).await
    })
}

/// Open every spec, then dispatch to the right ingest path:
/// single-spec → progress-bar single ingest; multi-spec → fan-out
/// ingest (no per-store progress, but a one-line summary per backend).
async fn ingest_with_specs(
    specs: &[StoreSpec],
    embedder: &Embedder,
    graph: &GraphData,
    prune: bool,
    budget: &EmbedBudget,
) -> Result<IngestOutcome, String> {
    // An index written by an older ug can't be opened, and this is the
    // command whose whole job is to replace it — so clear it first rather
    // than failing with "run ug gen" from inside ug gen.
    for path in storage::store::reset_stale_format_stores(specs)
        .map_err(|e| format!("clearing out-of-date store: {}", e))?
    {
        eprintln!(
            "{C_CYAN}▸{C_RESET} Rebuilding {} — it was written by an older ug",
            path.display()
        );
    }
    let mut stores: Vec<Box<dyn KnowledgeStore>> = Vec::with_capacity(specs.len());
    for spec in specs {
        let store = open_store(spec)
            .await
            .map_err(|e| format!("open {} store: {}", spec.name(), e))?;
        stores.push(store);
    }
    if stores.len() == 1 {
        let store = stores.into_iter().next().unwrap();
        ingest_graph_with_progress(store.as_ref(), embedder, graph, prune, budget).await
    } else {
        let set = StoreSet::new(stores);
        set.validate_dims().map_err(|e| format!("dim mismatch across destinations: {}", e))?;
        ingest_graph_multi_with_progress(&set, embedder, graph, prune, budget).await
    }
}

/// Multi-destination ingest with a single progress line per stage
/// (text-build, embed, write) — per-backend progress isn't useful when
/// fan-out is parallel.
async fn ingest_graph_multi_with_progress(
    set: &StoreSet,
    embedder: &Embedder,
    graph: &GraphData,
    prune: bool,
    budget: &EmbedBudget,
) -> Result<IngestOutcome, String> {

    let nodes_count = graph.nodes.len();
    let edges_count = graph.edges.len();

    let t0 = std::time::Instant::now();
    let captured = storage::capture_for_graph(graph);
    let texts = storage::build_texts(graph, &captured, budget);
    let refs: Vec<&dyn KnowledgeStore> = set.stores.iter().map(|s| s.as_ref()).collect();
    storage::refresh_sparse_stats(&refs, &texts, &captured, graph);
    println!(
        "{C_CYAN}▸{C_RESET} Building node texts: {C_GREEN}done{C_RESET} ({}) in {C_BOLD}{:?}{C_RESET}",
        nodes_count,
        t0.elapsed()
    );

    // Vector reuse is planned against the first destination, but every row
    // is still written to every destination — see `ingest_graph_multi`.
    let tp = std::time::Instant::now();
    let first = set
        .stores
        .first()
        .ok_or_else(|| "empty StoreSet".to_string())?;
    let model = embedder.config().model.clone();
    let plan =
        storage::plan_incremental_ingest(first.as_ref(), graph, &texts, true, &captured, Some(&model))
        .await
        .map_err(|e| format!("reading existing nodes: {}", e))?;
    let to_embed = plan.to_embed.len();
    println!(
        "{C_CYAN}▸{C_RESET} Diffing against {}: {C_GREEN}done{C_RESET} in {C_BOLD}{:?}{C_RESET} — {} reusable, {} to embed",
        first.backend_name(),
        tp.elapsed(),
        plan.reusable.len(),
        to_embed
    );

    let t1 = std::time::Instant::now();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(to_embed);
    // Degrades exactly as the single-destination path does — see the
    // comment there for why a dead embedder must not cost the whole index.
    let mut embed_error: Option<String> = None;
    for chunk in plan.to_embed.chunks(embedder.config().batch_size) {
        let chunk_vec: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        match embedder.embed(&chunk_vec).await {
            Ok(chunk_vectors) => vectors.extend(chunk_vectors),
            Err(e) => {
                embed_error = Some(e.to_string());
                break;
            }
        }
    }
    if let Some(e) = &embed_error {
        println!(
            "{C_YELLOW}⚠{C_RESET} Embedding failed after {}/{} — {}",
            vectors.len(),
            to_embed,
            e
        );
        println!(
            "  Writing the remaining node(s) without vectors: structure and statistics \
             still work, semantic search will miss them until the next run."
        );
        vectors.resize(to_embed, Vec::new());
    }
    println!(
        "{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}done{C_RESET} ({}) in {C_BOLD}{:?}{C_RESET}",
        to_embed,
        t1.elapsed()
    );

    let node_rows = plan.finish(graph, vectors, &captured)?;
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

    if prune {
        let t4 = std::time::Instant::now();
        // Every destination, not just the one the plan was built against.
        let mut removed = 0usize;
        for store in &set.stores {
            removed += storage::prune_to_graph(store.as_ref(), graph)
                .await
                .map_err(|e| format!("prune stale nodes: {}", e))?;
        }
        if removed > 0 {
            println!(
                "{C_CYAN}▸{C_RESET} Pruning stale nodes: {C_GREEN}done{C_RESET} (removed {}, ×{} backends) in {C_BOLD}{:?}{C_RESET}",
                removed,
                set.len(),
                t4.elapsed()
            );
        }
    }

    for store in &set.stores {
        store.ensure_query_indexes();
        if embed_error.is_none() {
            store.record_ingest_model(&model);
        }
    }

    Ok(IngestOutcome {
        nodes: nodes_count,
        edges: edges_count,
        embedding_error: embed_error,
    })
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
    let active = project::get_active_project();
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
        // `*` = matches the current directory; `→` = the active project
        // (`ug active`). When one project is both, `*` wins the leading slot
        // and the row still carries the active tag.
        let is_active = active.as_deref() == Some(meta.name.as_str());
        let marker = if meta.name == cwd_name { "*" } else { " " };
        let tag = if is_active {
            format!("  {C_YELLOW}← active{C_RESET}")
        } else {
            String::new()
        };
        let updated = format_epoch(meta.updated_at);
        println!(
            "{C_GREEN}{}{C_RESET} {C_CYAN}{:<24}{C_RESET} {:>8} {:>8}  {:<19}  {}{}",
            marker, meta.name, meta.nodes, meta.edges, updated, meta.repo_root, tag
        );
    }
    println!(
        "\n{C_BOLD}*{C_RESET} matches the current directory; {C_YELLOW}← active{C_RESET} is the default for {C_CYAN}ug mcp{C_RESET} and {C_CYAN}ug serve{C_RESET} (set with {C_CYAN}ug active <name>{C_RESET})."
    );
}

/// `ug active [<project>|--clear]` — view or set the persisted active
/// project. The active project is the default `ug mcp` resolves to when no
/// `UG_PROJECT` env var is set and the current directory isn't itself an
/// indexed project — so `ug mcp call <tool>` works from anywhere.
fn run_active(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug active{C_RESET} [<project> | --clear]");
        println!("  No args: show the current active project.");
        println!("  <project>: set it (must be an indexed project — see {C_CYAN}ug list{C_RESET}).");
        println!("  --clear: unset it.");
        return;
    }

    // Clear: `--clear`/`--unset`, or a bare `clear`/`none` positional.
    let positional = first_positional(args, &[]);
    let wants_clear = has_flag(args, "--clear")
        || has_flag(args, "--unset")
        || matches!(positional.as_deref(), Some("clear") | Some("none"));

    if wants_clear {
        match project::clear_active_project() {
            Ok(()) => println!("{C_GREEN}✓{C_RESET} Cleared the active project."),
            Err(e) => {
                eprintln!("Failed to clear active project: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    match positional {
        Some(name) => match project::set_active_project(&name) {
            Ok(set) => {
                println!("{C_GREEN}✓{C_RESET} Active project set to {C_CYAN}{}{C_RESET}.", set);
                if let Some(meta) = project::read_meta(&project::project_dir(&set)) {
                    if !meta.repo_root.is_empty() {
                        println!("  repo: {}", meta.repo_root);
                    }
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                eprintln!("Run {C_CYAN}ug list{C_RESET} to see available projects.");
                std::process::exit(1);
            }
        },
        None => match project::get_active_project() {
            Some(name) => {
                println!("Active project: {C_CYAN}{}{C_RESET}", name);
                if let Some(meta) = project::read_meta(&project::project_dir(&name)) {
                    if !meta.repo_root.is_empty() {
                        println!("  repo: {}", meta.repo_root);
                    }
                }
            }
            None => {
                println!("No active project set.");
                println!(
                    "Set one with {C_CYAN}ug active <name>{C_RESET} (see {C_CYAN}ug list{C_RESET})."
                );
            }
        },
    }
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

    // Captured before removal: get_active_project() validates the project
    // still has data, so it must be read while the dir still exists.
    let was_active = project::get_active_project().as_deref() == Some(project_name.as_str());

    match project::remove_project_dir(&dir) {
        Ok(()) => {
            // Drop the now-dangling active-project marker.
            if was_active {
                let _ = project::clear_active_project();
            }
            println!(
                "{C_GREEN}✓{C_RESET} Removed {C_BOLD}{}{C_RESET} ({})",
                project_name,
                dir.display()
            );
        }
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

/// `ug mcp [...]` — the native MCP server and its install/uninstall/call/list
/// subcommands. Bare `ug mcp` becomes a long-running stdio JSON-RPC server
/// (stdio is the transport, so the startup logo is suppressed for that mode —
/// see `is_mcp_server_mode` in `main`). This replaces the old Node.js `cli.mjs`
/// server: every tool now runs the same Rust code the CLI and HTTP API use.
fn run_mcp(args: &[String]) {
    mcp::run(args);
}

/// `ug connect` — the front door for wiring ug into an AI agent.
///
/// The same code as `ug mcp install`, under the name that describes what it
/// now does: since the choice is CLI skill *or* MCP server, filing it under
/// `mcp` named one of the two answers. That spelling still works.
fn run_connect(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_connect_help();
        return;
    }
    mcp::install::run_mcp_install(args);
}

/// `ug disconnect` — undo `ug connect`, whichever way it wired things.
fn run_disconnect(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_connect_help();
        return;
    }
    mcp::install::run_mcp_uninstall(args);
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
            // Env vars are no longer consulted for config keys — `ug config
            // set` is the way to persist. This block intentionally empty to
            // make that clear; remove when the dust settles.
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
    println!("{C_DIM}precedence: CLI flag > this file > built-in default{C_RESET}");
    println!();
    for key in config::CONFIG_KEYS {
        let saved = config::get(key.name);
        let value_label = match &saved {
            Some(v) => format!("{C_CYAN}{}{C_RESET}", config::display_value(key, v)),
            None => format!("{C_DIM}(not set){C_RESET}"),
        };
        let overrides = key.flag.to_string();
        println!("  {C_BOLD}{:<18}{C_RESET} {}", key.name, value_label);
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
    println!("  command as the fallback below CLI flags:");
    println!();
    println!("    {C_BOLD}CLI flag  >  ug config  >  built-in default{C_RESET}");
    println!();
    println!("  A flag that overrides a saved value prints a one-line notice.");
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

    println!(
        "  active proj:  {}  [{}]",
        project::get_active_project().unwrap_or_else(|| "(none)".to_string()),
        "ug active — default for `ug mcp` when no $UG_PROJECT / cwd match"
    );

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
            ApiEntry { method: "GET", path: "/api/capabilities", desc: "db/embedder/chat readiness, plus the indexing caps that shaped the store", availability: "always", cli_equivalent: Some("ug doctor (similar info)") },
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
            ApiEntry { method: "POST", path: "/api/tools/code_query", desc: "run a GQL (Cypher-like) query or built-in preset against the OverGraph store", availability: "503 if no DB backend configured", cli_equivalent: Some("ug query") },
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
            ApiEntry { method: "POST", path: "/api/tour", desc: "Guided, narrated walkthrough — ordered stops bound to node ids", availability: "503 if no DB + embedder; LLM narration optional (ranked fallback)", cli_equivalent: Some("ug tour") },
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
    let budget = budget_from_args(&embedder, args);
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
        // Opt-in here, unlike `ug gen`. `ug gen` owns a project and its
        // store should mirror the repo it indexed, so it prunes by
        // default. `ug ingest` just pushes a graph file at a destination,
        // and fanning several graphs into one store is a legitimate use —
        // pruning by default would make each ingest erase the last.
        let prune = has_flag(args, "--prune");
        match ingest_with_specs(&specs, &embedder, &graph, prune, &budget).await {
            Ok(out) => {
                println!("────────────────────────────────────────");
                println!(
                    "Ingested {} nodes, {} edges into [{}] in {:?}",
                    out.nodes,
                    out.edges,
                    dest_label.join(", "),
                    start_total.elapsed()
                );
                if let Some(e) = &out.embedding_error {
                    eprintln!(
                        "{C_YELLOW}⚠{C_RESET} Written without vectors — embedding failed: {}",
                        e
                    );
                    eprintln!(
                        "  Structure and statistics are queryable; re-run this command once the embedder is up."
                    );
                }
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
        let store = open_store_or_exit(&spec).await;
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
        let store = open_store_or_exit(&spec).await;
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
        // Accept a bare name or file path, not just an exact node id. The
        // retired `graph_bfs` did this and `traverse` did not, which was
        // the only reason to reach for the older command — typing
        // `ug traverse run_serve` is what people try first, and being told
        // "no node with id 'run_serve'" when the symbol plainly exists is
        // a bad way to learn that ids come from somewhere else.
        let starts: Vec<String> = starts
            .iter()
            .map(|s| resolve_node_ref(&graph, s))
            .collect();
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
        let store = open_store_or_exit(&spec).await;
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
    // Reasoning models spend most of the wall-clock deliberating; the
    // answer is grounded in retrieved context either way.
    let think = has_flag(args, "--think");
    // Tools are on by default: an answer that can check itself against the
    // graph beats one that can only paraphrase what retrieval happened to find.
    let no_tools = has_flag(args, "--no-tools");
    let max_tool_rounds: usize = flag_value(args, &["--max-tool-rounds"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .min(8);

    let k: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let hops: u32 = flag_value(args, &["--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let max_chars: usize = flag_value(args, &["--max-chars"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTEXT_CHARS);
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
        // Shared with the tool runner, which outlives any single turn.
        let store: std::sync::Arc<dyn KnowledgeStore> = std::sync::Arc::from(store);
        let embedder = std::sync::Arc::new(embedder);

        let edge_types_owned: Option<Vec<String>> = if edge_types.is_empty() {
            None
        } else {
            Some(edge_types)
        };

        // The same graph toolbox the UI and MCP clients get, so a terminal
        // answer is reached the same way as one in the browser. `--no-tools`
        // opts out; a project without a graph.json simply has none.
        let runner = if no_tools {
            None
        } else {
            Some(cli_tool_runner(args, store.clone(), embedder.clone()))
        };
        let toolbox = runner.as_ref().map(|run| chat::ToolBox {
            schemas: crate::mcp::tools::openai_tool_schemas(),
            run,
            max_rounds: max_tool_rounds,
            max_result_chars: 6_000,
        });

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
            o.fast = !think;
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
                        toolbox.as_ref(),
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
                        toolbox.as_ref(),
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
                    toolbox.as_ref(),
                    show_context,
                    no_stream,
                )
                .await;
            }
        }
    });
}

// ---------- Tour (guided, narrated graph walkthrough) ----------

fn run_tour(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_tour_help();
        return;
    }

    // Value-bearing flags so the first bare positional is the question.
    let value_flags = [
        "-n", "--name", "-k", "--limit", "--hops", "--max-stops", "--strategy", "--direction",
        "-t", "--edge-type", "--max-chars", "--max-per-file", "--repo-root", "--base-url",
        "--api-key", "--model",
        "--embedding-dim", "--embedding-model", "--embedding-base-url", "--embedding-api-key",
        "--chat-base-url", "--chat-api-key", "--chat-model", "--temperature", "--max-tokens",
        "--chat-timeout", "--filter", "-o", "--output", "--dest", "--neo4j-uri", "--neo4j-user",
        "--neo4j-password", "--neo4j-database",
    ];

    let query = match first_positional(args, &value_flags) {
        Some(q) => q,
        None => {
            eprintln!(
                "Usage: ug tour <question> [-k <n>] [--hops <n>] [--max-stops <n>] [--no-llm] [--json] [-o <file>]\n       (run `ug tour -h` for the full flag list)"
            );
            std::process::exit(1);
        }
    };

    let json_output = has_flag(args, "--json");
    let no_llm = has_flag(args, "--no-llm");
    let no_snippets = has_flag(args, "--no-snippets");
    // Print the guide's raw JSON plan alongside the itinerary — the CLI
    // twin of the web UI's "view plan JSON" panel.
    let show_plan = has_flag(args, "--show-plan");
    // Reasoning models spend most of a tour's wall-clock deliberating, so
    // the guide is asked not to unless the user wants it.
    let think = has_flag(args, "--think");
    let k: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(14);
    let hops: u32 = flag_value(args, &["--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let max_stops: usize = flag_value(args, &["--max-stops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(tour::DEFAULT_MAX_STOPS)
        .clamp(1, tour::MAX_STOPS_LIMIT);
    let max_chars: usize = flag_value(args, &["--max-chars"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTEXT_CHARS);
    // Candidates drawn from any one file, so a big file can't become the
    // whole tour. 0 disables the cap.
    let max_per_file: usize = flag_value(args, &["--max-per-file"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let strategy = flag_value(args, &["--strategy"])
        .map(|s| RankStrategy::from_str_lossy(&s))
        .unwrap_or(RankStrategy::Ppr);
    let direction = flag_value(args, &["--direction"])
        .map(|s| Direction::from_str_lossy(&s))
        .unwrap_or(Direction::Both);
    let edge_types = multi_flag(args, &["-t", "--edge-type"]);
    let where_clause = flag_value(args, &["--filter"]);
    let repo_root: PathBuf = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let output_path = flag_value(args, &["-o", "--output"]);

    let embedder = embedder_from_chat_args(args);
    let rt = tokio_runtime();

    rt.block_on(async {
        let dim = embedder.config().dim as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store_or_exit(&spec).await;

        let edge_types_owned: Option<Vec<String>> = if edge_types.is_empty() {
            None
        } else {
            Some(edge_types.clone())
        };
        let mut opts = tour::TourOptions::new();
        opts.k = k;
        opts.hops = hops;
        opts.max_stops = max_stops;
        opts.strategy = strategy;
        opts.direction = direction;
        opts.edge_types = edge_types_owned.as_deref();
        opts.include_snippets = !no_snippets;
        opts.max_context_chars = max_chars;
        opts.where_clause = where_clause.as_deref();
        opts.max_per_file = max_per_file;
        // The transcript is only worth carrying when something will print it.
        opts.include_debug = json_output || show_plan;
        opts.fast = !think;

        let result = if no_llm {
            eprintln!("{C_CYAN}▸{C_RESET} Planning tour (ranked, no LLM)…");
            tour::plan_tour_no_llm(store.as_ref(), &embedder, repo_root.as_path(), &query, opts.clone())
                .await
        } else {
            let chat_client = chat_client_from_args(args);
            eprintln!("{C_CYAN}▸{C_RESET} Planning tour for {C_BOLD}\u{201c}{}\u{201d}{C_RESET}…", query);
            // A local model can spend minutes on this; stream so the wait
            // has a visible pulse instead of a frozen terminal.
            opts.stream = true;
            let mut on_progress = tour_progress_printer();
            match tour::plan_tour_with_progress(
                store.as_ref(),
                &embedder,
                &chat_client,
                repo_root.as_path(),
                &query,
                opts.clone(),
                None,
                &mut on_progress,
            )
            .await
            {
                Ok(t) => Ok(t),
                Err(e) => {
                    eprintln!(
                        "{C_YELLOW}▸{C_RESET} tour guide (LLM) unavailable ({}); falling back to a ranked itinerary.",
                        e
                    );
                    tour::plan_tour_no_llm(
                        store.as_ref(),
                        &embedder,
                        repo_root.as_path(),
                        &query,
                        opts.clone(),
                    )
                    .await
                }
            }
        };

        let the_tour = match result {
            Ok(t) => t,
            Err(e) => {
                eprintln!("tour failed: {}", e);
                std::process::exit(1);
            }
        };

        if json_output {
            let text = serde_json::to_string_pretty(&the_tour).unwrap_or_default();
            write_or_print(output_path.as_deref(), &text, "tour");
        } else {
            print!("{}", render_tour(&the_tour, true));
            if show_plan {
                print!("{}", render_tour_plan(&the_tour, true));
            }
            if let Some(p) = output_path.as_deref() {
                write_file(p, &render_tour(&the_tour, false));
                println!("Wrote tour to {}", p);
            }
        }
    });
}

/// Word-wrap `text` to `width` columns, prefixing every line with `indent`.
fn wrap_indent(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    for (pi, para) in text.split('\n').enumerate() {
        if pi > 0 {
            out.push('\n');
        }
        let mut line_len = 0usize;
        let mut first_word = true;
        out.push_str(indent);
        for word in para.split_whitespace() {
            let wlen = word.chars().count();
            if !first_word && line_len + 1 + wlen > width {
                out.push('\n');
                out.push_str(indent);
                line_len = 0;
                first_word = true;
            }
            if !first_word {
                out.push(' ');
                line_len += 1;
            }
            out.push_str(word);
            line_len += wlen;
            first_word = false;
        }
    }
    out
}

/// The graph toolbox for `ug chat`, over this project's own graph and
/// store — the same tools the MCP server and `ug serve` expose, so an
/// answer in the terminal is reached the same way as one in the browser.
///
/// Returns the pieces the caller must keep alive: the runner closure is
/// borrowed by the `ToolBox`, so both have to outlive the chat turn.
fn cli_tool_runner(
    args: &[String],
    store: std::sync::Arc<dyn KnowledgeStore>,
    embedder: std::sync::Arc<Embedder>,
) -> impl Fn(&str, serde_json::Value) -> futures::future::BoxFuture<'static, Result<String, String>>
{
    let (graph, raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);
    let graph = std::sync::Arc::new(graph);
    let raw = std::sync::Arc::new(raw);

    move |name: &str, args: serde_json::Value| {
        let name = name.to_string();
        let graph = graph.clone();
        let raw = raw.clone();
        let repo_root = repo_root.clone();
        let graph_path = graph_path.clone();
        let store = store.clone();
        let embedder = embedder.clone();
        Box::pin(async move {
            let mut args = args;
            crate::mcp::tools::normalize_args(&name, &mut args);
            match name.as_str() {
                // The two search tools need the vector store; everything
                // else answers from the loaded graph.
                "search" | "semantic_search" => {
                    chat::run_search_tool(&name, &args, &*store, Some(&embedder), repo_root.as_path())
                        .await
                }
                // Statistics come from the store's indexed properties, not the
                // graph — the one advertised tool `run_tool` cannot answer.
                "code_query" => crate::mcp::run_code_query_json(&*store, &args).await,
                _ => {
                    crate::mcp::tools::reject_if_store_backed(&name)?;
                    let out = ultragraph::agent_tools::run_tool(
                        &name,
                        &graph,
                        &raw,
                        repo_root.as_path(),
                        graph_path.as_path(),
                        args,
                        Some(ultragraph::agent_tools::Render::Markdown),
                    )?;
                    Ok(match out {
                        ultragraph::agent_tools::ToolOutput::Text(t) => t,
                        ultragraph::agent_tools::ToolOutput::Json(v) => {
                            serde_json::to_string_pretty(&v).unwrap_or_default()
                        }
                    })
                }
            }
        }) as futures::future::BoxFuture<'static, Result<String, String>>
    }
}

/// A progress sink that keeps the terminal alive during a long plan.
/// Phase changes print a line; token counts rewrite one status line in
/// place (`\r`) so a five-minute completion doesn't scroll the screen.
fn tour_progress_printer() -> impl FnMut(tour::TourProgress) + Send {
    use std::io::Write;
    let mut writing = false;
    move |p| {
        let mut err = std::io::stderr();
        // Close off the in-place token line before printing anything else.
        let end_writing = |err: &mut std::io::Stderr, writing: &mut bool| {
            if *writing {
                let _ = writeln!(err);
                *writing = false;
            }
        };
        match p {
            tour::TourProgress::Retrieving => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · searching the graph…{C_RESET}");
            }
            tour::TourProgress::Retrieved { candidates, retrieval_ms } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(
                    err,
                    "{C_DIM}  · {} candidate(s) in {}ms{C_RESET}",
                    candidates, retrieval_ms
                );
            }
            tour::TourProgress::ReadingCode { items } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · read source for {} candidate(s){C_RESET}", items);
            }
            tour::TourProgress::Linking { edges } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · {} edge(s) between candidates{C_RESET}", edges);
            }
            tour::TourProgress::Planning { model, prompt_chars, candidates_shown, max_stops } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(
                    err,
                    "{C_DIM}  · asking {}{C_RESET}{C_DIM} for up to {} stop(s) from {} item(s) ({} char prompt){C_RESET}",
                    model, max_stops, candidates_shown, prompt_chars
                );
            }
            tour::TourProgress::Writing { chars, reasoning_chars, elapsed_ms } => {
                let secs = elapsed_ms as f64 / 1000.0;
                // ~4 chars/token is close enough for a progress read-out.
                let tokens = (chars + reasoning_chars) as f64 / 4.0;
                let rate = if secs > 0.0 { tokens / secs } else { 0.0 };
                let _ = write!(
                    err,
                    "\r{C_DIM}  · writing… ~{:.0} tokens · {:.0}/s · {:.0}s{C_RESET}\x1b[K",
                    tokens, rate, secs
                );
                let _ = err.flush();
                writing = true;
            }
            tour::TourProgress::Drafted { index, stop } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(
                    err,
                    "{C_GREEN}  ✓{C_RESET} {C_DIM}stop {} ready — {}{C_RESET}",
                    index + 1,
                    stop.title
                );
            }
            tour::TourProgress::Tool { name, args, summary, .. } => {
                end_writing(&mut err, &mut writing);
                match summary {
                    None => { let _ = writeln!(err, "{C_DIM}  ▸ {} {}{C_RESET}", name, args); }
                    Some(sum) => { let _ = writeln!(err, "{C_DIM}  ✓ {} — {}{C_RESET}", name, sum); }
                }
            }
            tour::TourProgress::Repairing { .. } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_YELLOW}  · reply unusable; asking again{C_RESET}");
            }
            tour::TourProgress::Assembling { stops } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · binding {} stop(s) to graph nodes{C_RESET}", stops);
            }
        }
    }
}

/// Render a `Tour` as a terminal itinerary. `color` toggles ANSI so the
/// same routine produces a clean plain-text file with `-o`.
fn render_tour(t: &tour::Tour, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let bold = c(C_BOLD);
    let reset = c(C_RESET);
    let cyan = c(C_CYAN);
    let dim = c(C_DIM);
    let green = c(C_GREEN);
    let yellow = c(C_YELLOW);
    let magenta = c(C_MAGENTA);

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("{bold}{cyan}❯ {}{reset}\n", t.title));
    if t.fallback {
        out.push_str(&format!(
            "{dim}  (ranked itinerary — no tour-guide LLM configured; pass --chat-model to narrate){reset}\n"
        ));
    }
    if !t.intro.is_empty() {
        out.push('\n');
        out.push_str(&wrap_indent(&t.intro, 76, "  "));
        out.push('\n');
    }

    if t.stops.is_empty() {
        out.push('\n');
        out.push_str(&format!("{yellow}  No stops on this tour.{reset}\n"));
        return out;
    }

    let total = t.stops.len();
    for (i, s) in t.stops.iter().enumerate() {
        out.push('\n');
        let loc = if s.start_line > 0 {
            format!(
                "{}:{}{}",
                if s.file.is_empty() { "<unknown>" } else { s.file.as_str() },
                s.start_line,
                if s.end_line > s.start_line { format!("-{}", s.end_line) } else { String::new() }
            )
        } else if !s.file.is_empty() {
            s.file.clone()
        } else {
            String::new()
        };
        // The graph edge we followed to get here, when there is one — the
        // itinerary should read as a walk, not a list.
        if let Some(link) = s.edge_from_prev.as_ref() {
            out.push_str(&format!(
                "{dim}  │  {} {}{reset}\n",
                if link.reverse { "\u{2190}" } else { "\u{2192}" },
                link.edge_type
            ));
        }
        out.push_str(&format!(
            "{green}  ●{reset} {dim}Stop {}/{}{reset} · {bold}{}{reset} {dim}({}){reset}\n",
            i + 1,
            total,
            s.title,
            s.node_type
        ));
        if !loc.is_empty() {
            out.push_str(&format!("{dim}     {}{reset}\n", loc));
        }
        if !s.narration.is_empty() {
            out.push_str(&wrap_indent(&s.narration, 74, "     "));
            out.push('\n');
        }
        if let Some(snip) = s.snippet.as_ref() {
            let snip = snip.trim_end_matches('\n');
            if !snip.is_empty() {
                for line in snip.lines().take(6) {
                    out.push_str(&format!("{dim}     │ {}{reset}\n", line));
                }
                if snip.lines().count() > 6 {
                    out.push_str(&format!("{dim}     │ …{reset}\n"));
                }
            }
        }
    }

    if !t.outro.is_empty() {
        out.push('\n');
        out.push_str(&format!("{magenta}  ✦{reset} "));
        // Continue the outro after the marker, wrapped and re-indented.
        let wrapped = wrap_indent(&t.outro, 74, "     ");
        out.push_str(wrapped.trim_start());
        out.push('\n');
    }

    if !t.warnings.is_empty() {
        out.push('\n');
        for w in &t.warnings {
            out.push_str(&format!("{yellow}  !{reset} {dim}{}{reset}\n", w));
        }
    }

    out.push('\n');
    let mut meta = format!("retrieval={}ms", t.retrieval_ms);
    if t.completion_ms > 0 {
        meta.push_str(&format!(" · guide={}ms", t.completion_ms));
    }
    meta.push_str(&format!(" · {} stop(s)", total));
    if !t.candidates.is_empty() {
        meta.push_str(&format!(" of {} candidate(s)", t.candidates.len()));
    }
    if let Some(u) = &t.usage {
        if let Some(tk) = u.total_tokens {
            meta.push_str(&format!(" · tokens={}", tk));
        }
    }
    out.push_str(&format!("{cyan}▸{reset} {dim}{}{reset}\n", meta));
    out
}

/// Pretty-print the guide's raw plan (`--show-plan`): the JSON object the
/// model produced, plus any refs we couldn't bind to a node.
fn render_tour_plan(t: &tour::Tour, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let bold = c(C_BOLD);
    let reset = c(C_RESET);
    let cyan = c(C_CYAN);
    let dim = c(C_DIM);
    let yellow = c(C_YELLOW);

    let mut out = String::new();
    let Some(d) = t.debug.as_ref() else {
        out.push_str(&format!(
            "\n{dim}  (no plan transcript — this itinerary didn't go through the tour guide){reset}\n"
        ));
        return out;
    };

    out.push_str(&format!("\n{bold}{cyan}❯ Guide plan (JSON){reset}\n"));
    if d.repaired {
        out.push_str(&format!(
            "{yellow}  !{reset} {dim}the first reply was unusable; this is the repaired one{reset}\n"
        ));
    }
    let body = match d.plan.as_ref() {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| d.raw_response.clone()),
        None => d.raw_response.clone(),
    };
    for line in body.lines() {
        out.push_str(&format!("{dim}  │ {reset}{}\n", line));
    }
    if !d.dropped.is_empty() {
        out.push('\n');
        for dr in &d.dropped {
            out.push_str(&format!(
                "{yellow}  !{reset} {dim}ref {} skipped — {}{reset}\n",
                dr.raw, dr.reason
            ));
        }
    }
    out
}

fn print_tour_help() {
    println!("  {C_CYAN}ug tour{C_RESET}  {C_YELLOW}— guided, narrated walk through the graph{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug tour <question> [options]");
    println!();
    println!("  GraphRAG picks the nodes that matter for your question, an LLM");
    println!("  \u{201c}tour guide\u{201d} orders them into a narrative and narrates each stop,");
    println!("  and the result is an ordered itinerary bound to real graph nodes.");
    println!("  In the web UI ({C_CYAN}ug serve{C_RESET}) the same tour flies the camera stop to stop.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>       Candidate nodes to retrieve (default: 14)");
    println!("  {C_CYAN}--hops{C_RESET} <n>            Graph expansion hops (default: 2)");
    println!("  {C_CYAN}--max-stops{C_RESET} <n>       Max stops on the tour (default: {}, max {})", tour::DEFAULT_MAX_STOPS, tour::MAX_STOPS_LIMIT);
    println!("  {C_CYAN}--max-per-file{C_RESET} <n>    Candidates kept per file, 0 = no cap (default: 2)");
    println!("  {C_YELLOW}--no-llm{C_RESET}             Skip the guide; emit a ranked itinerary from retrieval only");
    println!("  {C_CYAN}--no-snippets{C_RESET}         Omit code snippets from stops");
    println!("  {C_CYAN}--think{C_RESET}               Let a reasoning model deliberate (slower, rarely better)");
    println!("  {C_CYAN}--show-plan{C_RESET}           Print the raw JSON plan the guide produced");
    println!("  {C_CYAN}--strategy{C_RESET} <s>        Rank strategy (ppr|semantic|…, default: ppr)");
    println!("  {C_CYAN}--direction{C_RESET} <d>       Edge direction (out|in|both, default: both)");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>   Restrict expansion to an edge type (repeatable)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>        WHERE clause over node columns");
    println!("  {C_CYAN}-n, --name{C_RESET} <project>  Project under ~/.ug (default: cwd basename)");
    println!("  {C_CYAN}--json{C_RESET}                Emit the tour as JSON (node ids, timings, usage)");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>   Write the itinerary/JSON to a file");
    println!();
    println!("  Chat/embedding endpoint flags match {C_CYAN}ug chat{C_RESET}: {C_CYAN}--chat-model{C_RESET}, {C_CYAN}--base-url{C_RESET},");
    println!("  {C_CYAN}--api-key{C_RESET}, {C_CYAN}--temperature{C_RESET}, {C_CYAN}--max-tokens{C_RESET}, … (or persist via {C_CYAN}ug config set{C_RESET}).");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug tour{C_RESET} \"how does authentication work?\"");
    println!("  {C_CYAN}ug tour{C_RESET} \"the request lifecycle\" --max-stops 6 --hops 3");
    println!("  {C_CYAN}ug tour{C_RESET} \"how are nodes embedded?\" --show-plan");
    println!("  {C_CYAN}ug tour{C_RESET} \"error handling\" --no-llm --json -o tour.json");
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
        "{C_CYAN}▸{C_RESET} retrieval={}ms · completion={}ms · {} citation(s){}{}",
        outcome.retrieval_ms,
        outcome.completion_ms,
        outcome.context.items.len(),
        match outcome.tool_calls {
            0 => String::new(),
            n => format!(" · {} tool call(s)", n),
        },
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
    toolbox: Option<&chat::ToolBox<'_>>,
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
        toolbox,
        |ctx| {
            // Clear the transient retrieval line before real output.
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
            if show_context {
                print_context_items(&ctx.items);
            }
        },
        |t: chat::ToolEvent| {
            // One line per tool call so a long agentic turn is legible.
            match &t.summary {
                None => eprintln!("{C_DIM}  ▸ {} {}{C_RESET}", t.name, t.args),
                Some(sum) => eprintln!("{C_DIM}  ✓ {} — {}{C_RESET}", t.name, sum),
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
    toolbox: Option<&chat::ToolBox<'_>>,
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
            match chat::run_chat_rag(
                store, embedder, chat_client, repo_root, q, &history, opts, toolbox,
            )
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
                toolbox,
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
        history.push(chat::ChatMessage::new("user", q.to_string()));
        history.push(chat::ChatMessage::new("assistant", outcome.answer.clone()));
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
    println!(
        "  {C_YELLOW}--prune{C_RESET}             Delete stored nodes absent from this graph"
    );
    println!(
        "                          (off by default: several graphs may share one store)"
    );
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
    println!("{C_BOLD}Usage:{C_RESET}  ug list   {C_DIM}(aliases: ls, list_projects — the MCP tool's name){C_RESET}");
    println!();
    println!("  Lists every project under ~/.ug (or $UG_HOME), with node/edge counts");
    println!("  and last-updated time. The current directory's project is marked with {C_BOLD}*{C_RESET}.");
}

fn print_connect_help() {
    println!("{}", connect_help_text());
}

/// Built as a string rather than printed line by line, so a test can read it:
/// this is the page that has to keep both spellings discoverable.
fn connect_help_text() -> String {
    let mut o = String::new();
    macro_rules! line {
        ($($arg:tt)*) => { o.push_str(&format!("{}\n", format_args!($($arg)*))) };
    }
    line!("  {C_BOLD}{C_GREEN}★ ug connect{C_RESET}  {C_YELLOW}— wire ug into an AI coding agent{C_RESET}");
    line!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    line!("");
    line!("{C_BOLD}Usage:{C_RESET}  ug connect [<agent>] [--cli|--mcp|--both] [--project|--global]");
    line!("        ug disconnect [<agent>]        {C_DIM}remove it again{C_RESET}");
    line!("        {C_DIM}(`ug mcp install` / `ug mcp uninstall` are the same commands){C_RESET}");
    line!("");
    line!("  No agent named? You get an interactive picker. Agents: {C_CYAN}claude{C_RESET},");
    line!("  {C_CYAN}claude-desk{C_RESET}, {C_CYAN}cursor{C_RESET}, {C_CYAN}windsurf{C_RESET}, {C_CYAN}vscode{C_RESET}, {C_CYAN}gemini{C_RESET}, {C_CYAN}codex{C_RESET}, {C_CYAN}hermes{C_RESET}, {C_CYAN}opencode{C_RESET}.");
    line!("");
    line!("{C_BOLD}Two ways to reach ug — connect asks, or pass one:{C_RESET}");
    line!("  {C_CYAN}--cli{C_RESET}      {C_BOLD}Recommended.{C_RESET} Installs the agent skill only; the agent runs");
    line!("             {C_CYAN}ug{C_RESET} itself. {C_CYAN}ug --help{C_RESET} and {C_CYAN}ug query --list{C_RESET} teach it the rest,");
    line!("             so it stays current with the binary and costs no idle context.");
    line!("  {C_CYAN}--mcp{C_RESET}      MCP server entry only — the agent calls tools over the protocol.");
    line!("  {C_CYAN}--both{C_RESET}     Both, and the agent chooses. It usually reaches for the");
    line!("             connected tools, so pick this only if you want that path.");
    line!("");
    line!("  {C_DIM}Whichever you pick, the other is removed — the point of choosing is not");
    line!("  to leave the agent two doors into the same graph.{C_RESET}");
    line!("");
    line!("{C_BOLD}Scope:{C_RESET}");
    line!("  {C_CYAN}--project{C_RESET}  this repo only    {C_CYAN}--global{C_RESET}  every project");
    line!("  {C_DIM}Asked when the agent supports both and neither flag is given.{C_RESET}");
    line!("");
    line!("{C_BOLD}Examples:{C_RESET}");
    line!("  {C_CYAN}ug connect{C_RESET}                        {C_DIM}# pick the agent and the way, interactively{C_RESET}");
    line!("  {C_CYAN}ug connect claude --cli --global{C_RESET}  {C_DIM}# the CLI skill, everywhere{C_RESET}");
    line!("  {C_CYAN}ug connect cursor --mcp --project{C_RESET} {C_DIM}# MCP server, this repo only{C_RESET}");
    line!("  {C_CYAN}ug disconnect claude{C_RESET}             {C_DIM}# remove skill and server entry{C_RESET}");
    o
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
    println!("  {C_CYAN}--think{C_RESET}                  Let a reasoning model deliberate (slower, rarely better)");
    println!("  {C_CYAN}--no-tools{C_RESET}               Answer from retrieved context only — no graph tool calls");
    println!("  {C_CYAN}--max-tool-rounds{C_RESET} <n>    Cap tool-calling rounds (default: 4, max 8)");
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
        "  {C_CYAN}-c, --cache{C_RESET} <dir>        Parse cache directory (default: the output dir)"
    );
    println!(
        "  {C_YELLOW}--no-cache{C_RESET}                Re-parse every file, ignoring the parse cache"
    );
    println!(
        "  {C_YELLOW}--no-prune{C_RESET}                Keep store rows for nodes no longer in the graph"
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

/// Whether to skip the banner for this invocation.
///
/// The logo is decoration printed to **stdout**, which makes it worse than
/// noise for anything that reads output: it sat in front of the JSON from
/// `ug <tool> --json`, so piping to `jq` failed outright. Four ways it goes
/// away, in the order they are checked:
///
/// 1. **`--no-logo`** — explicit, works everywhere.
/// 2. **`UG_QUIET_LOGO`** — the pre-existing env contract, kept as-is.
/// 3. **stdout is not a terminal** — the one that matters in practice.
///    Pipes, redirects, CI and coding agents all land here and get clean
///    output with no flag to remember. A human at a terminal still sees
///    the banner, which is the only place it was ever doing any work.
/// 4. **stdio server modes** — bare `ug mcp` speaks JSON-RPC on stdout, so
///    a banner would corrupt the protocol stream outright; and `ug serve`'s
///    KB Manager wizard spawns `ug` as a subprocess whose output it streams
///    into a log viewer the banner would dominate.
fn suppress_logo(args: &[String], flagged_off: bool) -> bool {
    if flagged_off || std::env::var("UG_QUIET_LOGO").is_ok() {
        return true;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return true;
    }
    // Bare `ug mcp` (no install/uninstall subcommand) is the stdio server.
    args.get(1).map(String::as_str) == Some("mcp")
        && !matches!(
            args.get(2).map(String::as_str),
            Some("install") | Some("uninstall")
        )
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
    println!("{C_BOLD}Connect an AI agent (Claude Code / Claude Desktop / Cursor / Windsurf / VS Code / Gemini CLI / Codex CLI / Hermes Agent / opencode):{C_RESET}");
    println!("  {C_CYAN}ug connect{C_RESET}                Wire ug into an agent (interactive picker; or name one, e.g. `ug connect claude`)");
    println!("  {C_DIM}                          Asks how: {C_RESET}{C_CYAN}--cli{C_RESET}{C_DIM} teaches the agent this CLI (recommended), {C_RESET}{C_CYAN}--mcp{C_RESET}{C_DIM} wires the MCP server, {C_RESET}{C_CYAN}--both{C_RESET}{C_DIM} does both.{C_RESET}");

    println!();
    println!("{C_BOLD}Commands:{C_RESET}");
    println!(
        "  {C_BOLD}{C_MAGENTA}gen{C_RESET}              {C_BOLD}{C_MAGENTA}⚡ full pipeline: index → graph → visualization → ingest ⚡{C_RESET}"
    );
    println!("  {C_CYAN}regen{C_RESET}            Re-run that pipeline for an existing project (no -i needed; incremental)");
    println!("  {C_CYAN}serve{C_RESET}            Serve the visualization + graph API");
    println!("  {C_CYAN}app{C_RESET}              Open the native desktop shell (starts serve + a window)");
    println!("  {C_CYAN}api{C_RESET}              List every HTTP endpoint `ug serve` exposes");
    println!("  {C_CYAN}connect{C_RESET}          Wire ug into an AI agent — CLI skill and/or MCP server");
    println!("                   {C_DIM}(also spelled `ug mcp install`; undo with `ug disconnect`){C_RESET}");
    println!();
    println!("  {C_DIM}Retrieval & analysis (OverGraph-backed){C_RESET}");
    println!(
        "  {C_BOLD}{C_YELLOW}search{C_RESET}           {C_YELLOW}GraphRAG: semantic search → graph expansion → ranked context{C_RESET}"
    );
    println!(
        "  {C_BOLD}{C_MAGENTA}query{C_RESET}            {C_BOLD}{C_MAGENTA}📊 whole-repo statistics: counts, distributions, blast radius{C_RESET}"
    );
    println!("                   {C_DIM}33 named questions ({C_RESET}{C_CYAN}ug query --list{C_RESET}{C_DIM}) or write your own GQL{C_RESET}");
    println!("  {C_CYAN}semantic_search{C_RESET}  Search by meaning/concept (embeddings; use find_symbols for exact names)");
    println!("  {C_CYAN}traverse{C_RESET}         K-hop BFS over the OverGraph edges table");
    println!(
        "  {C_BOLD}{C_MAGENTA}chat{C_RESET}             {C_BOLD}{C_MAGENTA}💬 GraphRAG-grounded chat (one-shot or REPL){C_RESET}"
    );
    println!(
        "  {C_BOLD}{C_MAGENTA}tour{C_RESET}             {C_BOLD}{C_MAGENTA}🎬 guided, narrated walkthrough — flies the camera in the web UI{C_RESET}"
    );
    println!();
    // `index` / `graph` / `ingest` are the stages `gen` runs and are still
    // dispatched, but they are not listed: they are internal seams, and
    // `gen --no-ingest` already covers the one reason an end user reached
    // for them. `ug api` and the docs still name them.
    println!("  {C_DIM}Structural analysis (graph.json only — no database needed){C_RESET}");
    println!("  {C_CYAN}graph_centrality{C_RESET} Rank nodes by degree/betweenness (--top, -t, -f)");
    println!("                   {C_DIM}degree ranking is also {C_RESET}{C_CYAN}ug query dependency_fanin{C_RESET}{C_DIM}; betweenness is only here{C_RESET}");
    println!("  {C_CYAN}graph_cycles{C_RESET}     Detect dependency cycles (--min-len, --fail-on-cycle for CI)");
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
    println!("  {C_BOLD}{C_GREEN}list{C_RESET}             {C_GREEN}List generated projects under ~/.ug (or $UG_HOME){C_RESET}");
    println!("  {C_CYAN}active{C_RESET}           View/set the active project (default for `ug mcp` when no UG_PROJECT)");
    println!("  {C_CYAN}rm{C_RESET}               Delete a project's data directory");
    println!("  {C_CYAN}upgrade{C_RESET}          Check GitHub for a new release and self-update (`--check` to only report)");
    println!("  {C_CYAN}uninstall{C_RESET}        Delete ALL indexed projects and uninstall ug itself");
    println!("  {C_CYAN}config{C_RESET}           View/persist defaults (chat model, endpoints, …) in ~/.ug/config.json");
    println!("  {C_CYAN}doctor{C_RESET}           Show resolved project/db/embedder/chat config");
    println!();
    println!("{C_BOLD}Global flags:{C_RESET}");
    println!("  {C_CYAN}--no-logo{C_RESET}        Skip the banner. Already skipped automatically whenever stdout");
    println!("                   is not a terminal, so piped and captured output is clean.");
    println!("  {C_CYAN}-v, --version{C_RESET}    Print the version");
    println!();
    println!("Run {C_CYAN}ug <command> -h{C_RESET} for that command's options and examples.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn a_bare_preset_name_is_the_positional() {
        let p = code_query_params_from_args(&argv("long_functions")).unwrap();
        assert_eq!(p.preset.as_deref(), Some("long_functions"));
        assert!(p.gql.is_none());
    }

    /// The bug this test exists for: `-o` carries the store path, and
    /// leaving it out of the skip list made the path itself read as a
    /// second preset — surfacing as "preset and gql together", which
    /// points nowhere near the cause.
    #[test]
    fn a_flag_value_is_never_mistaken_for_the_positional_preset() {
        for line in [
            "repo_census -o /tmp/db",
            "repo_census --output /tmp/db",
            "repo_census -n myproject",
            "repo_census -k 5",
            "repo_census -a target=src/a.rs",
        ] {
            let p = code_query_params_from_args(&argv(line)).unwrap();
            assert_eq!(
                p.preset.as_deref(),
                Some("repo_census"),
                "parsed the wrong positional from `{line}`"
            );
        }
    }

    #[test]
    fn an_explicit_query_suppresses_positional_preset_inference() {
        let args = vec![
            "--gql".to_string(),
            "MATCH (n) RETURN count(*) AS c".to_string(),
            "-o".to_string(),
            "/tmp/db".to_string(),
        ];
        let p = code_query_params_from_args(&args).unwrap();
        assert!(p.preset.is_none(), "got preset {:?}", p.preset);
        assert!(p.gql.is_some());
    }

    /// Same failure as the `-o` bug: a flag whose value looks like a bare
    /// word gets read as the positional preset, and the error that surfaces
    /// points nowhere near the cause.
    #[test]
    fn a_range_value_is_not_mistaken_for_the_positional_preset() {
        for line in ["dead_code -r 11-35", "dead_code --range 34-end"] {
            let p = code_query_params_from_args(&argv(line)).unwrap();
            assert_eq!(p.preset.as_deref(), Some("dead_code"), "from `{line}`");
            assert!(p.range.is_some(), "from `{line}`");
        }
    }

    #[test]
    fn repeated_arg_flags_accumulate() {
        let p = code_query_params_from_args(&argv(
            "layering_violations -a from_prefix=src/ui -a to_prefix=src/db",
        ))
        .unwrap();
        assert_eq!(p.args["from_prefix"], "src/ui");
        assert_eq!(p.args["to_prefix"], "src/db");
    }

    /// The banner goes to stdout, so anything that *reads* output has to
    /// get it suppressed. The non-terminal case is the one that matters —
    /// it is why `ug <tool> --json | jq` works without a flag — but it
    /// cannot be asserted here, because the test harness's stdout is
    /// already not a terminal and would mask every other condition.
    #[test]
    fn the_logo_is_suppressed_by_the_flag_and_by_env() {
        assert!(suppress_logo(&argv("ug graph_schema"), true), "--no-logo");

        // Whatever the harness's stdout is, an explicit opt-out wins.
        std::env::set_var("UG_QUIET_LOGO", "1");
        assert!(suppress_logo(&argv("ug gen"), false), "UG_QUIET_LOGO");
        std::env::remove_var("UG_QUIET_LOGO");
    }

    /// Bare `ug mcp` speaks JSON-RPC on stdout — a banner there is not
    /// noise, it corrupts the protocol. `ug mcp install` is an ordinary
    /// interactive command and keeps it.
    #[test]
    fn the_stdio_server_mode_never_prints_a_banner() {
        assert!(suppress_logo(&argv("ug mcp"), false));
        assert!(suppress_logo(&argv("ug mcp call find_symbols"), false));
        // These are interactive; only a non-terminal stdout should silence
        // them, which is the condition this test cannot control.
        for line in [
            "ug mcp install claude",
            "ug mcp uninstall cursor",
            "ug connect claude",
            "ug disconnect cursor",
        ] {
            let args = argv(line);
            assert_eq!(
                suppress_logo(&args, false),
                !std::io::IsTerminal::is_terminal(&std::io::stdout()),
                "`{line}` should only be silenced by a non-terminal stdout"
            );
        }
    }

    /// `ug connect` is the promoted spelling of `ug mcp install`, not a second
    /// implementation — so its help has to teach the modes *and* keep the old
    /// spelling discoverable for anyone whose muscle memory or scripts have it.
    #[test]
    fn connect_help_teaches_the_modes_and_keeps_the_old_spelling() {
        let help = connect_help_text();
        for expected in [
            "--cli", "--mcp", "--both",
            "Recommended",
            "ug disconnect",
            "ug mcp install",
            "--project", "--global",
        ] {
            assert!(help.contains(expected), "`ug connect -h` is missing {expected}");
        }
    }

    #[test]
    fn a_malformed_arg_is_rejected_rather_than_dropped() {
        let err = code_query_params_from_args(&argv("impact -a target")).unwrap_err();
        assert!(err.contains("key=value"), "{err}");
    }
}
