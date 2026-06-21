use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use ultragraph::storage::{
    self, open_store, search_kb as storage_search_kb,
    semantic_search as storage_semantic_search, traverse as storage_traverse, Direction, Embedder,
    EmbedderConfig, KnowledgeStore, RankStrategy, SearchKbOptions, StoreSet, StoreSpec,
};
use ultragraph::types::GraphData;
use ultragraph::{
    build_graph, calculate_centrality, detect_cycles, filter_edges_by_type, find_shortest_path,
    graph_keyword_search, index, index_with_cache, k_hop_bfs, C_BLUE, C_BOLD, C_CYAN, C_GREEN,
    C_MAGENTA, C_RESET, C_YELLOW,
};

mod chat;
mod serve;

// Bundled visualization assets so `ug gen` can produce a self-contained
// output directory without needing the source tree at runtime.
pub(crate) const VIS_HTML: &str = include_str!("./vis/visualization.html");
pub(crate) const VIS_BUNDLE: &[u8] = include_bytes!("./vis/ug-vis.bundle.js");
const VIS_MD: &str = include_str!("../../README.md");

fn main() {
    install_panic_hook();
    // Load environment defaults from `.env` (in CWD or any parent
    // directory). Real env vars still win — `dotenvy::dotenv` does not
    // override values already set in the process environment. Quiet
    // when no `.env` is present.
    let _ = dotenvy::dotenv();
    print_logo();

    let args: Vec<String> = env::args().collect();

    if args.len() >= 2 && (args[1] == "-v" || args[1] == "--version") {
        println!("ug version {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.len() < 2 {
        print_help();
        return;
    }

    let cmd = &args[1];
    let cmd_args = &args[2..];

    match cmd.as_str() {
        "index" => run_index(cmd_args),
        "graph" => run_graph(cmd_args),
        "bfs" => run_bfs(cmd_args),
        "filter" => run_filter(cmd_args),
        "path" => run_path(cmd_args),
        "centrality" => run_centrality(cmd_args),
        "cycles" => run_cycles(cmd_args),
        "search_graph" => run_search_graph(cmd_args),
        "analyze" => run_analyze(cmd_args),
        "gen" => run_gen(cmd_args),
        "ingest" => run_ingest(cmd_args),
        "semantic_search" => run_semantic_search(cmd_args),
        "hybrid_search" => run_hybrid_search(cmd_args),
        "traverse" => run_traverse(cmd_args),
        "chat" => run_chat(cmd_args),
        "serve" => serve::run_serve(cmd_args),
        "help" => {
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

// ---------- Embedder / runtime helpers ----------

pub(crate) fn embedder_from_args(args: &[String]) -> Embedder {
    let dim = flag_value(args, &["--embedding-dim"]).and_then(|s| s.parse().ok());
    let base_url = flag_value(args, &["--base-url"]);
    // Presence of --base-url is the single switch between in-process
    // (default) and the legacy HTTP backend. --model applies to both:
    // for local it picks a fastembed catalog entry; for remote it's the
    // model field sent in the /v1/embeddings request.
    let want_remote = base_url.is_some();
    let cfg = EmbedderConfig::with_overrides(
        base_url,
        flag_value(args, &["--api-key"]),
        flag_value(args, &["--model"]),
        dim,
        None,
        None,
    );
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

/// Like `embedder_from_args`, but resolves the embedding model from
/// `--embedding-model` first (falling back to `--model` for backwards
/// compatibility with the rest of the CLI). Used by `ug chat` where
/// `--model` is reserved for the chat side and a separate
/// `--embedding-model` selects the embeddings.
///
/// For the base-url / api-key, `--embedding-base-url` /
/// `--embedding-api-key` win when set, otherwise the shared
/// `--base-url` / `--api-key` apply (this matches the common case where
/// chat and embedding share a single OpenAI-compatible host).
pub(crate) fn embedder_from_chat_args(args: &[String]) -> Embedder {
    let dim = flag_value(args, &["--embedding-dim"]).and_then(|s| s.parse().ok());
    let base_url = flag_value(args, &["--embedding-base-url"])
        .or_else(|| flag_value(args, &["--base-url"]));
    let api_key = flag_value(args, &["--embedding-api-key"])
        .or_else(|| flag_value(args, &["--api-key"]));
    let model = flag_value(args, &["--embedding-model"])
        .or_else(|| flag_value(args, &["--model"]));
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

    // The OverGraph dir path. Read commands use -d / --db; ingest uses
    // -o / --output (which is also the JSON output file in some
    // commands, so -d always wins when both are present).
    let og_path = flag_value(args, &["-d", "--db"])
        .or_else(|| flag_value(args, &["-o", "--output"]))
        .or_else(|| std::env::var("UG_DB_PATH").ok())
        .unwrap_or_else(|| ".ug/ugdb".to_string());

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
        .or_else(|| first_positional(args, &["-i", "--input", "-o", "--output", "-c", "--cache"]))
        .unwrap_or_else(|| ".".to_string());
    let cache = flag_value(args, &["-c", "--cache"]);
    let output = flag_value_or(args, &["-o", "--output"], ".ug/indexed-tree.json");

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

    let input = flag_value_or(args, &["-i", "--input"], ".ug/indexed-tree.json");
    let output = flag_value_or(args, &["-o", "--output"], ".ug/graph.json");

    let index_json = fs::read_to_string(&input).expect("Failed to read input");
    let result = build_graph(index_json);
    write_file(&output, &result);
    println!(
        "{C_GREEN}✓{C_RESET} Generated graph in {C_BOLD}{}{C_RESET}",
        output
    );
}

// simple breadth-first search on the graph (json)
fn run_bfs(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ug bfs <graph-file> <start-node-id> [k] [-o|--output <file>]");
        std::process::exit(1);
    }
    let graph_file = &args[0];
    let start_node = args[1].clone();
    let k: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let output_path = flag_value(args, &["-o", "--output"]);

    let graph_json = fs::read_to_string(graph_file).expect("Failed to read graph file");
    let result = k_hop_bfs(graph_json, start_node, k);
    write_or_print(output_path.as_deref(), &result, "BFS result");
}

// keyword-based in-memory graph search by loading the graph file into memory (json)
fn run_search_graph(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ug search_graph <graph-file> <keyword> [-t|--type <node-type>]... [-o|--output <file>]");
        std::process::exit(1);
    }
    let graph_file = &args[0];
    let keyword = first_positional(&args[1..], &["-t", "--type", "-o", "--output"]).unwrap_or_else(|| {
        eprintln!("Usage: ug search_graph <graph-file> <keyword> [-t|--type <node-type>]... [-o|--output <file>]");
        std::process::exit(1);
    });
    let node_types = multi_flag(args, &["-t", "--type"]);
    let output_path = flag_value(args, &["-o", "--output"]);

    let graph_json = fs::read_to_string(graph_file).expect("Failed to read graph");
    let types_opt = if node_types.is_empty() {
        None
    } else {
        Some(node_types)
    };
    let result = graph_keyword_search(graph_json, keyword, types_opt);
    write_or_print(output_path.as_deref(), &result, "search result");
}

fn run_filter(args: &[String]) {
    if args.len() < 2 {
        eprintln!(
            "Usage: ug filter <graph-file> <edge-type> [<edge-type>...] [-o|--output <file>]"
        );
        std::process::exit(1);
    }
    let graph_file = &args[0];
    let edge_types: Vec<String> = args[1..]
        .iter()
        .take_while(|s| !s.starts_with('-'))
        .map(|s| s.to_lowercase())
        .collect();
    let output_path = flag_value(args, &["-o", "--output"]);

    let graph_json = fs::read_to_string(graph_file).expect("Failed to read graph");
    let result = filter_edges_by_type(graph_json, edge_types);
    write_or_print(output_path.as_deref(), &result, "filtered edges");
}

fn run_path(args: &[String]) {
    if args.len() < 3 {
        eprintln!("Usage: ug path <graph-file> <source> <target> [-o|--output <file>]");
        std::process::exit(1);
    }
    let graph_file = &args[0];
    let source = args[1].clone();
    let target = args[2].clone();
    let output_path = flag_value(args, &["-o", "--output"]);

    let graph_json = fs::read_to_string(graph_file).expect("Failed to read graph");
    let result = find_shortest_path(graph_json, source, target);
    write_or_print(output_path.as_deref(), &result, "path result");
}

fn run_centrality(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ug centrality <graph-file> [-o|--output <file>]");
        std::process::exit(1);
    }
    let graph_file = &args[0];
    let output_path = flag_value(args, &["-o", "--output"]);

    let graph_json = fs::read_to_string(graph_file).expect("Failed to read graph");
    let result = calculate_centrality(graph_json);
    write_or_print(output_path.as_deref(), &result, "centrality");
}

fn run_cycles(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ug cycles <graph-file> [-o|--output <file>]");
        std::process::exit(1);
    }
    let graph_file = &args[0];
    let output_path = flag_value(args, &["-o", "--output"]);

    let graph_json = fs::read_to_string(graph_file).expect("Failed to read graph");
    let result = detect_cycles(graph_json);
    write_or_print(output_path.as_deref(), &result, "cycle result");
}

fn run_analyze(args: &[String]) {
    let input = flag_value_or(args, &["-i", "--input"], ".ug/graph.json");
    let output_dir = flag_value_or(args, &["-o", "--output"], ".ug");

    let graph_json = fs::read_to_string(&input).expect("Failed to read graph");
    let centrality = calculate_centrality(graph_json.clone());
    let cycles = detect_cycles(graph_json);

    let _ = fs::create_dir_all(&output_dir);
    fs::write(format!("{}/analysis.json", output_dir), &centrality)
        .expect("Failed to write analysis.json");
    fs::write(format!("{}/cycles.json", output_dir), &cycles).expect("Failed to write cycles.json");

    println!("{C_GREEN}✓{C_RESET} Analyzed graph:");
    println!("  {C_CYAN}▸{C_RESET} analysis.json (centrality)");
    println!("  {C_CYAN}▸{C_RESET} cycles.json (cycle detection)");
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
    let output_dir = flag_value_or(args, &["-o", "--output"], ".ug");
    let no_ingest = has_flag(args, "--no-ingest");
    let chain_serve = has_flag(args, "--serve");
    let db_path = flag_value(args, &["-o", "--output"]).unwrap_or_else(|| ".ug/ugdb".to_string());

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

    println!("{C_BOLD}────────────────────────────────────────{C_RESET}");
    println!(
        "{C_GREEN}✓ Generated{C_RESET} in {C_BOLD}{}/{C_RESET}",
        output_dir
    );
    println!("  {C_GREEN}✓{C_RESET} graph.json");
    println!("  {C_GREEN}✓{C_RESET} indexed-tree.json");
    println!("  {C_GREEN}✓{C_RESET} README.md");

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
        "Run ' ug semantic_search \"hello\" -d {} ' to perform a semantic RAG query.",
        db_path
    );
    println!(
        "Run ' ug hybrid_search \"hello\" -d {} ' to perform a hybrid graph + semantic RAG query.",
        db_path
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
        // If --dest wasn't specified the default OverGraph path may
        // not match the db_path the gen pipeline computed; respect
        // gen's path in that case.
        if specs.len() == 1 {
            if let StoreSpec::Overgraph {
                path,
                embedding_dim: _,
            } = &mut specs[0]
            {
                if path.as_os_str().is_empty()
                    || path.to_str() == Some(".ug/ugdb")
                {
                    *path = PathBuf::from(db_path);
                }
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

fn run_ingest(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_ingest_help();
        return;
    }

    let graph_file = flag_value_or(args, &["-i", "--input"], ".ug/graph.json");

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
    if args.is_empty() {
        eprintln!(
            "Usage: ug semantic_search <query> [-d|--db <path>] [-k|--limit <n>] \\
                 [--filter <sql>] [--base-url <url>] [--api-key <key>] [--model <name>] \\
                 [--embedding-dim <n>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let query = first_positional(
        args,
        &[
            "-d",
            "--db",
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
    if args.is_empty() {
        eprintln!(
            "Usage: ug hybrid_search <query> [-d|--db <path>] [-k|--limit <n>] [--hops <n>] \\
                 [--filter <sql>] [--strategy <ppr|mmr>] [--direction <out|in|both>] \\
                 [-t|--edge-type <type>]... [--max-chars <n>] [--mmr-lambda <f>] \\
                 [--no-snippets] [--repo-root <path>] \\
                 [--base-url <url>] [--api-key <key>] [--model <name>] [--embedding-dim <n>] \\
                 [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let value_flags = [
        "-d",
        "--db",
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
    if args.is_empty() {
        eprintln!(
            "Usage: ug traverse <start-node-id> [-d|--db <path>] [-k|--hops <n>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let start = first_positional(
        args,
        &[
            "-d",
            "--db",
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
    )
    .expect("missing start node id");
    let hops: u32 = flag_value(args, &["-k", "--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let output_path = flag_value(args, &["-o", "--output"]);

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
        let result = storage_traverse(store.as_ref(), &start, hops)
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
    let base_url = flag_value(args, &["--chat-base-url"])
        .or_else(|| flag_value(args, &["--base-url"]));
    let api_key = flag_value(args, &["--chat-api-key"])
        .or_else(|| flag_value(args, &["--api-key"]));
    let model = flag_value(args, &["--chat-model"]);
    let temperature = flag_value(args, &["--temperature"]).and_then(|s| s.parse().ok());
    let max_tokens = flag_value(args, &["--max-tokens"]).and_then(|s| s.parse().ok());
    let timeout = flag_value(args, &["--chat-timeout"]).and_then(|s| s.parse().ok());
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
        "-d",
        "--db",
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

        match oneshot_query {
            Some(q) => {
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

fn print_chat_outcome(query: &str, outcome: &chat::ChatRagOutcome, show_context: bool) {
    println!();
    println!("{C_BOLD}{C_CYAN}❯ Query:{C_RESET} {}", query);
    println!();
    if show_context {
        println!("{C_BOLD}{C_MAGENTA}Retrieved context ({} items):{C_RESET}", outcome.context.items.len());
        for (i, it) in outcome.context.items.iter().enumerate() {
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
    println!("{C_BOLD}{C_GREEN}Answer:{C_RESET}");
    println!("{}", outcome.answer.trim_end());
    println!();
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

async fn run_chat_repl<'a, F>(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat_client: &chat::ChatClient,
    repo_root: &std::path::Path,
    mut opts_factory: F,
    show_context: bool,
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
        let outcome = match chat::run_chat_rag(
            store,
            embedder,
            chat_client,
            repo_root,
            q,
            &history,
            opts,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{C_YELLOW}chat error:{C_RESET} {}", e);
                continue;
            }
        };

        print_chat_outcome(q, &outcome, show_ctx);

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
    println!("  {C_CYAN}-o, --output{C_RESET} <file>  Output file (default: .ug/indexed-tree.json)");
    println!("  {C_CYAN}-c, --cache{C_RESET} <dir>     Cache directory for incremental indexing");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug index{C_RESET} -i ./src -o index.json");
    println!("  {C_CYAN}ug index{C_RESET} -c ./cache -o ./.ug/indexed-tree.json");
}

fn print_graph_help() {
    println!("  {C_CYAN}ug graph{C_RESET}  {C_YELLOW}— build a graph from the indexed tree output{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph [<file>] [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>  Input index file (default: .ug/indexed-tree.json)");
    println!("  {C_CYAN}-o, --output{C_RESET} <file> Output graph file (default: .ug/graph.json)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph{C_RESET} -i index.json -o graph.json");
    println!("  {C_CYAN}ug graph{C_RESET} (uses defaults)");
}

fn print_ingest_help() {
    println!("  {C_CYAN}ug ingest{C_RESET}  {C_YELLOW}— embed graph nodes and write to one or more knowledge stores{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug ingest [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>  Graph JSON (default: .ug/graph.json)");
    println!("  {C_CYAN}-o, --output{C_RESET} <dir>  OverGraph directory (default: .ug/ugdb)");
    println!();
    println!("{C_BOLD}Destinations (default: overgraph):{C_RESET}");
    println!("  {C_CYAN}--dest{C_RESET} <kind[,kind...]>   {C_BOLD}overgraph{C_RESET} | {C_BOLD}neo4j{C_RESET}. Comma-separated for fan-out ingest.");
    println!("                              Reads (semantic_search/hybrid_search/traverse) accept");
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
    println!("  {C_CYAN}ug ingest{C_RESET} -i .ug/graph.json -o .ug/ugdb        {C_YELLOW}# local, default model{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --model nomic-embed-text-v1.5             {C_YELLOW}# local, larger model{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --base-url https://api.openai.com/v1 \\");
    println!("            --api-key $OPENAI_API_KEY --model text-embedding-3-small  {C_YELLOW}# remote{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --dest neo4j \\");
    println!("            --neo4j-uri neo4j://localhost:7687 --neo4j-user neo4j \\");
    println!("            --neo4j-password $NEO4J_PASSWORD                           {C_YELLOW}# Neo4j only{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --dest overgraph,neo4j \\");
    println!("            -o .ug/ugdb --neo4j-uri neo4j://localhost:7687 \\");
    println!("            --neo4j-user neo4j --neo4j-password $NEO4J_PASSWORD        {C_YELLOW}# fan-out{C_RESET}");
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
    println!("{C_BOLD}Retrieval (matches `ug hybrid_search`):{C_RESET}");
    println!("  {C_CYAN}-d, --db{C_RESET} <path>          OverGraph directory (default: .ug/ugdb)");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>          Context items to retrieve (default: 8)");
    println!("  {C_CYAN}--hops{C_RESET} <n>               Graph expansion hops (default: 2)");
    println!("  {C_CYAN}--strategy{C_RESET} <s>           ppr (default) or mmr");
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
    println!("    -d .ug/ugdb \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("    --chat-model Qwen3.6-35B-A3B-MLX-8bit \\");
    println!("    --embedding-model Qwen3-Embedding-4B-4bit-DWQ");
    println!();
    println!("  {C_MAGENTA}ug chat{C_RESET} --json -v \\");
    println!("    \"explain the PPR seed pool logic\" -d .ug/ugdb \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --chat-model my-model");
    println!();
    println!("  {C_MAGENTA}ug chat{C_RESET} -d .ug/ugdb \\");
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
        "  {C_CYAN}-o, --output{C_RESET} <dir>       Output/OverGraph directory (default: .ug)"
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
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src -o ./.ug");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src --no-ingest --serve");
}

fn print_logo() {
    println!();
    println!(
        "          {C_BOLD}{C_YELLOW}·{C_RESET}      {C_BOLD}{C_YELLOW}✦{C_RESET}      {C_BOLD}{C_CYAN}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_CYAN}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_CYAN}◉{C_RESET}      {C_BOLD}{C_YELLOW}✦{C_RESET}      {C_BOLD}{C_YELLOW}·{C_RESET}"
    );
    println!("                          {C_BOLD}{C_BLUE}╱ ╲ ╱ ╲ ╱ ╲{C_RESET}");
    println!(
        "             {C_BOLD}{C_YELLOW}·{C_RESET}        {C_BOLD}{C_MAGENTA}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_MAGENTA}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_MAGENTA}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_MAGENTA}◉{C_RESET}        {C_BOLD}{C_YELLOW}·{C_RESET}"
    );
    println!("                          {C_BOLD}{C_BLUE}╲ ╱ ╲ ╱ ╲ ╱{C_RESET}");
    println!(
        "          {C_BOLD}{C_YELLOW}·{C_RESET}      {C_BOLD}{C_YELLOW}✦{C_RESET}      {C_BOLD}{C_GREEN}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_GREEN}◉{C_RESET}{C_BOLD}{C_BLUE}───{C_RESET}{C_BOLD}{C_GREEN}◉{C_RESET}      {C_BOLD}{C_YELLOW}✦{C_RESET}      {C_BOLD}{C_YELLOW}·{C_RESET}"
    );
    println!();
    println!(
        "       {C_BOLD}{C_CYAN}━━━━━{C_RESET}  {C_BOLD}{C_MAGENTA}U{C_RESET} {C_BOLD}{C_CYAN}L{C_RESET} {C_BOLD}{C_MAGENTA}T{C_RESET} {C_BOLD}{C_CYAN}R{C_RESET} {C_BOLD}{C_MAGENTA}A{C_RESET}   {C_BOLD}{C_YELLOW}✦{C_RESET}   {C_BOLD}{C_GREEN}G{C_RESET} {C_BOLD}{C_CYAN}R{C_RESET} {C_BOLD}{C_GREEN}A{C_RESET} {C_BOLD}{C_CYAN}P{C_RESET} {C_BOLD}{C_GREEN}H{C_RESET}  {C_BOLD}{C_CYAN}━━━━━{C_RESET}"
    );
    println!();
    println!(
        "    {C_BOLD}{C_YELLOW}✦{C_RESET}  {C_BOLD}{C_CYAN}A luminous lattice where every concept finds its place,{C_RESET}"
    );
    println!(
        "    {C_BOLD}{C_YELLOW}✦{C_RESET}  {C_BOLD}{C_MAGENTA}a constellation woven from the very fabric of meaning —{C_RESET}"
    );
    println!(
        "    {C_BOLD}{C_YELLOW}✦{C_RESET}  {C_BOLD}{C_GREEN}the graph from which all other graphs eternally flow.{C_RESET}"
    );
    println!();
}

fn print_help() {
    println!("{C_BOLD}UltraGraph-KB CLI{C_RESET}");
    println!();
    println!("Usage: {C_BOLD}ug <command>{C_RESET} [options]");
    println!();
    println!("{C_BOLD}Commands:{C_RESET}");
    println!("  {C_CYAN}index{C_RESET} [<path>]        Index a directory");
    println!("    -i, --input <path>   Input directory (default: .)");
    println!("    -o, --output <file>  Output file (default: .ug/indexed-tree.json)");
    println!("    -c, --cache <dir>    Cache directory for incremental indexing");
    println!();
    println!("  {C_CYAN}graph{C_RESET} [<file>]        Build graph from index result");
    println!("    -i, --input <file>  Input index file (default: .ug/indexed-tree.json)");
    println!("    -o, --output <file> Output graph file (default: .ug/graph.json)");
    println!();
    println!("  {C_CYAN}bfs{C_RESET} <file> <node> [k]  K-hop BFS traversal");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  {C_CYAN}filter{C_RESET} <graph> <type>...  Filter edges by type");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  {C_CYAN}path{C_RESET} <graph> <src> <dst>  Find shortest path between nodes");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  {C_CYAN}centrality{C_RESET} <graph>     Calculate degree/betweenness centrality");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  {C_CYAN}cycles{C_RESET} <graph>        Detect cycles in graph");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  {C_CYAN}search_graph{C_RESET} <graph> <keyword>  Keyword search over graph nodes (in-memory)");
    println!("    -t, --type <type>   Restrict to node type (repeatable)");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!(
        "  {C_CYAN}analyze{C_RESET}              Run full graph analysis (centrality + cycles)"
    );
    println!("    -i, --input <file> Graph file (default: .ug/graph.json)");
    println!("    -o, --output <dir> Output directory (default: .ug)");
    println!();
    println!("  {C_BOLD}{C_MAGENTA}gen{C_RESET} [<path>]         {C_BOLD}{C_MAGENTA}⚡ Full pipeline: index → graph → visualization → OverGraph ingest ⚡{C_RESET}");
    println!("    -i, --input <path>  Input directory (default: .)");
    println!("    -c, --cache <dir>   Cache directory");
    println!("    -o, --output <dir>  Output/OverGraph directory (default: .ug)");
    println!("    --no-ingest         Skip the OverGraph ingest step");
    println!(
        "    --serve             Chain into 'ug serve' on the generated outputs after gen finishes"
    );
    println!("    --base-url/--api-key/--model/--embedding-dim  Embedding endpoint overrides");
    println!();
    println!("  {C_CYAN}ingest{C_RESET}               Embed graph nodes and write to OverGraph");
    println!("    -i, --input <file>  Graph JSON (default: .ug/graph.json)");
    println!("    -o, --output <dir> OverGraph directory (default: .ug/ugdb)");
    println!("    --model <name>     Local fastembed alias or remote model name");
    println!("                       (default: bge-small-en-v1.5, in-process, 384d)");
    println!("    --base-url <url>   Switches to remote /v1/embeddings endpoint");
    println!("    --api-key <key>    Bearer token for remote endpoint (default: 1234)");
    println!(
        "    --embedding-dim <n>  Vector dim override. Auto-probed otherwise; persisted to <db>/ug-meta.json on first ingest."
    );
    println!();
    println!("  {C_CYAN}semantic_search{C_RESET} <query>  Semantic vector search (OverGraph, no graph context)");
    println!("    -d, --db <path>    OverGraph directory (default: .ug/ugdb)");
    println!("    -k, --limit <n>    Top-k results (default: 10)");
    println!("    --filter <sql>     Optional SQL WHERE clause");
    println!("    --base-url/--api-key/--model/--embedding-dim  Embedding endpoint overrides");
    println!("    -o, --output <file> Output file (optional, omit for stdout)");
    println!();
    println!(
        "  {C_CYAN}hybrid_search{C_RESET} <query>   {C_YELLOW}GraphRAG: semantic search → graph expansion → ranked context{C_RESET}"
    );
    println!("    -d, --db <path>     OverGraph directory (default: .ug/ugdb)");
    println!("    -k, --limit <n>     Final results (default: 8)");
    println!("    --hops <n>          Graph expansion hops (default: 2)");
    println!("    --filter <sql>      SQL WHERE clause for semantic seed filter");
    println!("    --strategy <s>      ppr (default) or mmr (max marginal relevance)");
    println!("    --direction <dir>   outbound|inbound|both (default: both)");
    println!("    -t, --edge-type <t> Restrict expansion to edge type (repeatable)");
    println!("    --max-chars <n>     Char budget for assembled context (default: 12000)");
    println!("    --mmr-lambda <f>    MMR diversity/relevance balance 0..1 (default: 0.6)");
    println!("    --no-snippets       Skip reading source snippets from disk");
    println!("    --repo-root <path>  Repo root for snippet resolution (default: cwd)");
    println!("    --base-url/--api-key/--model/--embedding-dim  Embedding endpoint overrides");
    println!("    -o, --output <file> Output file (optional, omit for stdout)");
    println!();
    println!("  {C_CYAN}traverse{C_RESET} <node-id>   K-hop BFS using the OverGraph edges table");
    println!("    -d, --db <path>    OverGraph directory (default: .ug/ugdb)");
    println!("    -k, --hops <n>     Max hops (default: 2)");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!(
        "  {C_BOLD}{C_MAGENTA}chat{C_RESET} [<prompt>]      {C_BOLD}{C_MAGENTA}💬 GraphRAG-grounded chat (one-shot or REPL){C_RESET}"
    );
    println!("    -d, --db <path>       OverGraph directory (default: .ug/ugdb)");
    println!("    -k, --limit <n>       Retrieved context items (default: 8)");
    println!("    --hops <n>            Graph expansion hops (default: 2)");
    println!("    --strategy <s>        ppr (default) or mmr");
    println!("    --direction <dir>     outbound|inbound|both (default: both)");
    println!("    -t, --edge-type <t>   Restrict expansion edge type (repeatable)");
    println!("    --max-chars <n>       Context char budget (default: 12000)");
    println!("    --no-snippets         Don't include source snippets in context");
    println!("    --repo-root <path>    Repo root for snippet resolution (default: cwd)");
    println!("    --chat-model <name>   Chat completion model (required for remote chat)");
    println!("    --base-url <url>      OpenAI-compatible base URL (shared with embeddings)");
    println!("    --api-key <key>       Bearer token (shared with embeddings)");
    println!("    --chat-base-url/--chat-api-key  Override the chat endpoint only");
    println!("    --embedding-model <name>  Embedding model (falls back to --model)");
    println!("    --embedding-base-url/--embedding-api-key  Override the embedding endpoint only");
    println!("    --temperature <f>     Sampling temperature (default: 0.2)");
    println!("    --max-tokens <n>      Max completion tokens (default: 1024)");
    println!("    --system <text>       Override the default RAG system prompt");
    println!("    --show-context, -v    Print retrieved citations alongside the answer");
    println!("    --json                Emit a JSON document (answer + citations)");
    println!("    -o, --output <file>   Write answer (or JSON) to a file");
    println!();
    println!("  {C_CYAN}serve{C_RESET}                Serve the visualization + graph.json + read-only API (in-memory, pre-compressed)");
    println!("    -i, --input <file>  Graph JSON to serve (default: .ug/graph.json)");
    println!("    -d, --db <path>     OverGraph DB for /api/db + /api/search routes (default: .ug/ugdb)");
    println!("    --no-db             Don't open DB; routes return 503");
    println!("    -p, --port <n>      TCP port (default: 8080)");
    println!("    --host <addr>       Bind address (default: 127.0.0.1; use 0.0.0.0 for LAN)");
    println!("    --watch             Reload graph file when its mtime changes (~2s poll)");
    println!(
        "    --repo-root <path>  Repo root for hybrid-search snippet resolution (default: cwd)"
    );
    println!("    --base-url/--api-key/--model/--embedding-dim  Embedding endpoint overrides");
    println!("    --chat-model/--chat-base-url/--chat-api-key  Chat endpoint overrides (enables /api/chat)");
    println!("    {C_GREEN}API:{C_RESET} GET  /api/graph/{{stats, node/<id>, search?q=&types=, bfs/<id>?k=,");
    println!("                    path?source=&target=, filter?types=, centrality, cycles}}");
    println!("          GET  /api/db/{{node/<id>, traverse/<id>?k=&dir=&types=}}");
    println!("          POST /api/search/{{semantic, hybrid}}  body: JSON");
    println!("          POST /api/chat  body: {{query, k?, hops?, history?, chat_model?, ...}}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug index{C_RESET} -i ./src -o index.json");
    println!("  {C_CYAN}ug graph{C_RESET} -i index.json -o graph.json");
    println!("  {C_CYAN}ug bfs{C_RESET} graph.json file:src/index.ts 2");
    println!("  {C_CYAN}ug filter{C_RESET} graph.json Contains Imports");
    println!("  {C_CYAN}ug centrality{C_RESET} graph.json");
    println!("  {C_CYAN}ug cycles{C_RESET} graph.json");
    println!(
        "  {C_CYAN}ug search_graph{C_RESET} graph.json loadConfig --type function --type class"
    );
    println!("  {C_CYAN}ug analyze{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src -o ./.ug");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src --no-ingest --serve");
    println!("  {C_CYAN}ug ingest{C_RESET} -i .ug/graph.json -o .ug/ugdb");
    println!("  {C_CYAN}ug semantic_search{C_RESET} \"oauth login flow\" -d .ug/ugdb");
    println!("  {C_CYAN}ug hybrid_search{C_RESET} \"oauth login flow\" -d .ug/ugdb -k 8");
    println!("  {C_CYAN}ug traverse{C_RESET} \"file:src/index.ts\" -d .ug/ugdb");
    println!("  {C_MAGENTA}ug chat{C_RESET} \"how does ingest work?\" -d .ug/ugdb \\");
    println!("        --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("        --chat-model my-chat --embedding-model my-embed");
    println!("  {C_CYAN}ug serve{C_RESET} -i .ug/graph.json -p 8080");
}
