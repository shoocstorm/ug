use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use ultragraph_kb::storage::{
    self, search_kb as storage_search_kb, semantic_search as storage_semantic_search,
    traverse as storage_traverse, Db, Direction, Embedder, EmbedderConfig, RankStrategy,
    SearchKbOptions,
};
use ultragraph_kb::types::GraphData;
use ultragraph_kb::{
    build_graph, calculate_centrality, detect_cycles, filter_edges_by_type, find_shortest_path,
    graph_keyword_search, index, index_with_cache, k_hop_bfs, C_BLUE, C_BOLD, C_CYAN, C_GREEN,
    C_MAGENTA, C_RESET, C_YELLOW,
};

mod serve;

// Bundled visualization assets so `ug gen` can produce a self-contained
// output directory without needing the source tree at runtime.
pub(crate) const VIS_HTML: &str = include_str!("./vis/visualization.html");
pub(crate) const VIS_D3: &[u8] = include_bytes!("./vis/d3.v7.min.js");
const VIS_MD: &str = include_str!("../../README.md");

fn main() {
    print_logo();

    let args: Vec<String> = env::args().collect();

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
    let cfg = EmbedderConfig::with_overrides(
        flag_value(args, &["--base-url"]),
        flag_value(args, &["--api-key"]),
        flag_value(args, &["--model"]),
        None,
        None,
    );
    Embedder::new(cfg).expect("failed to build embedder")
}

pub(crate) fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

// ---------- Commands ----------

fn run_index(args: &[String]) {
    let path = flag_value(args, &["-i", "--input"])
        .or_else(|| first_positional(args, &["-i", "--input", "-o", "--output", "-c", "--cache"]))
        .unwrap_or_else(|| ".".to_string());
    let cache = flag_value(args, &["-c", "--cache"]);
    let output = flag_value_or(args, &["-o", "--output"], "ug-out/indexed-tree.json");

    let result = match cache {
        Some(c) => index_with_cache(path, c),
        None => index(path),
    };
    write_file(&output, &result);
    println!("{C_GREEN}✓{C_RESET} Generated index in {C_BOLD}{}{C_RESET}", output);
}

fn run_graph(args: &[String]) {
    let input = flag_value_or(args, &["-i", "--input"], "ug-out/indexed-tree.json");
    let output = flag_value_or(args, &["-o", "--output"], "ug-out/graph.json");

    let index_json = fs::read_to_string(&input).expect("Failed to read input");
    let result = build_graph(index_json);
    write_file(&output, &result);
    println!("{C_GREEN}✓{C_RESET} Generated graph in {C_BOLD}{}{C_RESET}", output);
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
    let input = flag_value_or(args, &["-i", "--input"], "ug-out/graph.json");
    let output_dir = flag_value_or(args, &["-o", "--output"], "ug-out");

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
                ],
            )
        })
        .unwrap_or_else(|| ".".to_string());
    let cache = flag_value(args, &["-c", "--cache"]);
    let output_dir = flag_value_or(args, &["-o", "--output"], "ug-out");
    let no_ingest = has_flag(args, "--no-ingest");
    let chain_serve = has_flag(args, "--serve");
    let db_path =
        flag_value(args, &["-o", "--output"]).unwrap_or_else(|| "ug-out/ugdb".to_string());

    let pipeline_summary = if no_ingest {
        "index → graph → visualization"
    } else {
        "index → graph → visualization → ingest"
    };
    println!("⚡ Full pipeline: {C_BOLD}{C_MAGENTA}{}{C_RESET}", pipeline_summary);

    let _ = fs::create_dir_all(&output_dir);

    let t0 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Indexing {C_YELLOW}{}{C_RESET}", input);
    let index_result = match cache {
        Some(c) => index_with_cache(input, c),
        None => index(input),
    };
    println!("  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}", t0.elapsed());

    let t1 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Building graph");
    let graph = build_graph(index_result.clone());
    println!("  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}", t1.elapsed());

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
     println!("{C_CYAN}▸{C_RESET} Copying visualization assets");
     fs::write(format!("{}/index.html", output_dir), VIS_HTML).expect("Failed to write index.html");
     fs::write(format!("{}/d3.v7.min.js", output_dir), VIS_D3)
         .expect("Failed to write d3.v7.min.js");
     fs::write(format!("{}/README.md", output_dir), VIS_MD).expect("Failed to write README.md");
     println!("  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}", t2.elapsed());
 
     println!("{C_BOLD}────────────────────────────────────────{C_RESET}");
     println!("{C_GREEN}✓ Generated{C_RESET} in {C_BOLD}{}/{C_RESET}", output_dir);
     println!("  {C_GREEN}✓{C_RESET} graph.json");
     println!("  {C_GREEN}✓{C_RESET} indexed-tree.json");
     println!("  {C_GREEN}✓{C_RESET} index.html (open in browser with HTTP server)");
     println!("  {C_GREEN}✓{C_RESET} d3.v7.min.js");
     println!("  {C_GREEN}✓{C_RESET} README.md");

    if no_ingest {
        println!("{C_YELLOW}⚠ Skipping db-ingest (--no-ingest){C_RESET}");
        if chain_serve {
            println!("Total time: {C_BOLD}{:?}{C_RESET}", start_total.elapsed());
            chain_to_serve(args, &graph_path, &db_path, true);
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
    println!("{C_CYAN}▸{C_RESET} Ingesting graph data into DB {C_YELLOW}{}{C_RESET}", db_path);
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
        "Run ' ug serve -i {} ' and open http://127.0.0.1:8080 to view the graph.",
        graph_path
    );
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
        chain_to_serve(args, &graph_path, &db_path, false);
    }
}

/// Build a synthetic args vec for `serve` from the gen invocation and call
/// `serve::run_serve`. Inherits port/host/watch/repo-root and embedder flags
/// from the original invocation; sets `-i`/`-d` to the freshly generated
/// paths, and `--no-db` when the ingest step was skipped.
fn chain_to_serve(args: &[String], graph_path: &str, db_path: &str, no_db: bool) {
    let mut serve_args: Vec<String> = vec![
        "-i".to_string(),
        graph_path.to_string(),
        "-d".to_string(),
        db_path.to_string(),
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

// ingest graph data into graph db
async fn ingest_graph_with_progress(
    db: &Db,
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
        db.upsert_nodes(batch)
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
        db.upsert_edges(batch)
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
    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();
    rt.block_on(async {
        let db = Db::open(db_path)
            .await
            .map_err(|e| format!("open db: {}", e))?;
        ingest_graph_with_progress(&db, &embedder, &graph).await
    })
}

fn run_ingest(args: &[String]) {
    let graph_file = flag_value_or(args, &["-i", "--input"], "ug-out/graph.json");
    let db_path = flag_value_or(args, &["-o", "--output"], "ug-out/ugdb");

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph file");
    let graph: GraphData = serde_json::from_str(&graph_json).expect("Failed to parse graph JSON");
    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();

    let start_total = std::time::Instant::now();

    rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open overgraph");
        match ingest_graph_with_progress(&db, &embedder, &graph).await {
            Ok((nodes_written, edges_written)) => {
                println!("────────────────────────────────────────");
                println!(
                    "Ingested {} nodes, {} edges into {} in {:?}",
                    nodes_written,
                    edges_written,
                    db_path,
                    start_total.elapsed()
                );
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    });
}

// vector search on OverGraph (only)
fn run_semantic_search(args: &[String]) {
    if args.is_empty() {
        eprintln!(
            "Usage: ug semantic_search <query> [-d|--db <path>] [-k|--limit <n>] \\
                 [--filter <sql>] [--base-url <url>] [--api-key <key>] [--model <name>] [-o|--output <file>]"
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
            "-o",
            "--output",
        ],
    )
    .expect("missing query");
    let db_path = flag_value_or(args, &["-d", "--db"], "ug-out/ugdb");
    let limit: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let filter = flag_value(args, &["--filter"]);
    let output_path = flag_value(args, &["-o", "--output"]);
    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();

    let result_json = rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open OverGraph");
        let hits = match filter.as_deref() {
            Some(f) => storage::semantic_search_w_where(&db, &embedder, &query, limit, f)
                .await
                .expect("semantic_search_w_where failed"),
            None => storage_semantic_search(&db, &embedder, &query, limit)
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
                 [--base-url <url>] [--api-key <key>] [--model <name>] [-o|--output <file>]"
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
        "-o",
        "--output",
    ];
    let query = first_positional(args, &value_flags).expect("missing query");
    let db_path = flag_value_or(args, &["-d", "--db"], "ug-out/ugdb");
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
        let db = Db::open(&db_path).await.expect("failed to open OverGraph");
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

        let result = storage_search_kb(&db, &embedder, opts)
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

    let start = first_positional(args, &["-d", "--db", "-k", "--hops", "-o", "--output"])
        .expect("missing start node id");
    let db_path = flag_value_or(args, &["-d", "--db"], "ug-out/ugdb");
    let hops: u32 = flag_value(args, &["-k", "--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let output_path = flag_value(args, &["-o", "--output"]);

    let rt = tokio_runtime();
    let json = rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open OverGraph");
        let result = storage_traverse(&db, &start, hops)
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

// ---------- Help ----------

fn print_gen_help() {
    println!("gen [<path>]  Full pipeline: index → graph → visualization → OverGraph ingest");
    println!("  -i, --input <path>   Input directory (default: .)");
    println!("  -c, --cache <dir>    Cache directory for incremental indexing");
    println!("  -o, --output <dir>   Output/OverGraph directory (default: ug-out)");
    println!("  --no-ingest          Skip the OverGraph ingest step");
    println!("  --serve              After gen, chain into ' ug serve ' on the generated outputs");
    println!(
        "                       (inherits -p/--port, --host, --watch, --repo-root, embedder flags)"
    );
    println!("  --base-url <url>     Embedding endpoint (default: http://localhost:8000/v1)");
    println!("  --api-key <key>      Embedding API key");
    println!("  --model <name>       Embedding model");
}

fn print_logo() {
    println!(
        "{C_BOLD}{C_CYAN}  _   _ {C_MAGENTA} _ {C_YELLOW} _             {C_GREEN} _____                 _     {C_RESET}"
    );
    println!(
        "{C_BOLD}{C_CYAN} | | | |{C_MAGENTA}| |_ {C_YELLOW}___ ___ ___  {C_GREEN}|   __|___ ___ ___ ___| |_   {C_RESET}"
    );
    println!(
        "{C_BOLD}{C_CYAN} | | | |{C_MAGENTA}|  _|{C_YELLOW}  _| .'| . | {C_GREEN}|  |  |  _| .'| . |   |  _|  {C_RESET}"
    );
    println!(
        "{C_BOLD}{C_CYAN} |_____|{C_MAGENTA}|_| {C_YELLOW}|_| |__,|_  | {C_GREEN}|_____|_| |__,|  _|_|_|_|    {C_RESET}"
    );
    println!(
        "{C_BOLD}{C_YELLOW}                       |___| {C_GREEN}              |_|            {C_RESET}"
    );
    println!();
    println!(
        "        {C_BOLD}{C_BLUE}⊂{C_RESET}{C_BOLD}{C_MAGENTA}ヽ{C_RESET}{C_BOLD}{C_BLUE}({C_RESET}{C_BOLD}{C_CYAN}◕{C_RESET}{C_BOLD}{C_MAGENTA}‿{C_RESET}{C_BOLD}{C_CYAN}◕{C_RESET}{C_BOLD}{C_BLUE}){C_RESET}{C_BOLD}{C_MAGENTA}ﾉ{C_RESET}{C_BOLD}{C_BLUE}⊃{C_RESET}  {C_BOLD}{C_YELLOW}✨ UltraGraph: Ultra-fast Knowledge Graph ✨{C_RESET}"
    );
    println!();
}

fn print_help() {
    println!("UltraGraph-KB CLI");
    println!();
    println!("Usage: ug <command> [options]");
    println!();
    println!("Commands:");
    println!("  index [<path>]        Index a directory");
    println!("    -i, --input <path>   Input directory (default: .)");
    println!("    -o, --output <file>  Output file (default: ug-out/indexed-tree.json)");
    println!("    -c, --cache <dir>    Cache directory for incremental indexing");
    println!();
    println!("  graph [<file>]        Build graph from index result");
    println!("    -i, --input <file>  Input index file (default: ug-out/indexed-tree.json)");
    println!("    -o, --output <file> Output graph file (default: ug-out/graph.json)");
    println!();
    println!("  bfs <file> <node> [k] K-hop BFS traversal");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  filter <graph> <type>... Filter edges by type");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  path <graph> <src> <dst> Find shortest path between nodes");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  centrality <graph>     Calculate degree/betweenness centrality");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  cycles <graph>        Detect cycles in graph");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  search_graph <graph> <keyword> Keyword search over graph nodes (in-memory, for small graphs)");
    println!(
        "    -t, --type <type>   Restrict to node type (repeatable, e.g. function/class/file)"
    );
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  analyze              Run full graph analysis (centrality + cycles)");
    println!("    -i, --input <file> Graph file (default: ug-out/graph.json)");
    println!("    -o, --output <dir> Output directory (default: ug-out)");
    println!();
    println!(
        "  gen [<path>]         Full pipeline: index → graph → visualization → OverGraph ingest"
    );
    println!("    -i, --input <path>  Input directory (default: .)");
    println!("    -c, --cache <dir>   Cache directory");
    println!("    -o, --output <dir>  Output/OverGraph directory (default: ug-out)");
    println!("    --no-ingest         Skip the OverGraph ingest step");
    println!("    --serve             Chain into ' ug serve ' on the generated outputs after gen finishes");
    println!("    --base-url/--api-key/--model  Embedding endpoint overrides");
    println!();
    println!("  ingest               Embed graph nodes and write to OverGraph");
    println!("    -i, --input <file>  Graph JSON (default: ug-out/graph.json)");
    println!("    -o, --output <dir> OverGraph directory (default: ug-out/ugdb)");
    println!("    --base-url <url>   Embedding endpoint (default: http://localhost:8000/v1)");
    println!("    --api-key <key>    Embedding API key (default: 1234)");
    println!(
        "    --model <name>     Embedding model (default: openai/Qwen3-Embedding-0.6B-4bit-DWQ)"
    );
    println!();
    println!("  semantic_search <query>      Semantic vector search (OverGraph, no graph context)");
    println!("    -d, --db <path>    OverGraph directory (default: ug-out/ugdb)");
    println!("    -k, --limit <n>    Top-k results (default: 10)");
    println!("    --filter <sql>     Optional SQL WHERE clause");
    println!("    --base-url/--api-key/--model  Embedding endpoint overrides");
    println!("    -o, --output <file> Output file (optional, omit for stdout)");
    println!();
    println!(
        "  hybrid_search <query>        GraphRAG: semantic search → graph expansion → ranked context"
    );
    println!("    -d, --db <path>     OverGraph directory (default: ug-out/ugdb)");
    println!("    -k, --limit <n>     Final results (default: 8)");
    println!("    --hops <n>          Graph expansion hops (default: 2)");
    println!("    --filter <sql>      SQL WHERE clause for semantic seed filter");
    println!("    --strategy <s>      ppr (default, personalizedPageRank) or mmr (max marginal relevance)");
    println!("    --direction <dir>   outbound|inbound|both (default: both)");
    println!("    -t, --edge-type <t> Restrict expansion to edge type (repeatable)");
    println!("    --max-chars <n>     Char budget for assembled context (default: 12000)");
    println!("    --mmr-lambda <f>    MMR diversity/relevance balance 0..1 (default: 0.6)");
    println!("    --no-snippets       Skip reading source snippets from disk");
    println!("    --repo-root <path>  Repo root for snippet resolution (default: cwd)");
    println!("    --base-url/--api-key/--model  Embedding endpoint overrides");
    println!("    -o, --output <file> Output file (optional, omit for stdout)");
    println!();
    println!("  traverse <node-id>   K-hop BFS using the OverGraph edges table");
    println!("    -d, --db <path>    OverGraph directory (default: ug-out/ugdb)");
    println!("    -k, --hops <n>     Max hops (default: 2)");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  serve                Serve the visualization + graph.json + read-only API (in-memory, pre-compressed gzip/br)");
    println!("    -i, --input <file>  Graph JSON to serve (default: ug-out/graph.json)");
    println!("    -p, --port <n>      TCP port (default: 8080)");
    println!("    --host <addr>       Bind address (default: 127.0.0.1; use 0.0.0.0 for LAN)");
    println!("    --watch             Reload graph file when its mtime changes (~2s poll)");
    println!("    -d, --db <path>     OverGraph DB for /api/db + /api/search routes (default: ug-out/ugdb)");
    println!("    --no-db             Don't open DB; Phase 3 routes return 503");
    println!(
        "    --repo-root <path>  Repo root for hybrid-search snippet resolution (default: cwd)"
    );
    println!(
        "    --base-url/--api-key/--model  Embedding endpoint overrides (same as ingest/search)"
    );
    println!("    API: GET  /api/graph/{{stats, node/<id>, search?q=&types=, bfs/<id>?k=,");
    println!(
        "                           path?source=&target=, filter?types=, centrality, cycles}}"
    );
    println!("         GET  /api/db/{{node/<id>, traverse/<id>?k=&dir=&types=}}");
    println!("         POST /api/search/{{semantic, hybrid}}  body: JSON");
    println!();
    println!("Examples:");
    println!("  ug index -i ./src -o index.json");
    println!("  ug graph -i index.json -o graph.json");
    println!("  ug bfs graph.json file:src/index.ts 2");
    println!("  ug filter graph.json Contains Imports");
    println!("  ug centrality graph.json");
    println!("  ug cycles graph.json");
    println!("  ug search_graph graph.json loadConfig --type function --type class");
    println!("  ug analyze");
    println!("  ug gen -i ./src -o ./ug-out");
    println!("  ug gen -i ./src --no-ingest --serve");
    println!("  ug ingest -i ug-out/graph.json -o ug-out/ugdb");
    println!("  ug semantic_search \"oauth login flow\" -d ug-out/ugdb");
    println!("  ug hybrid_search \"oauth login flow\" -d ug-out/ugdb -k 8");
    println!("  ug hybrid_search \"build a tree\" -d ug-out/ugdb --strategy mmr");
    println!("  ug traverse \"file:src/index.ts\" -d ug-out/ugdb");
    println!("  ug serve -i ug-out/graph.json -p 8080");
}
