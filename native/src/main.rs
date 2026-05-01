use std::env;
use std::fs;
use std::path::Path;
use ultragraph_kb::storage::{
    self, ingest_graph, semantic_search as storage_semantic_search, traverse as storage_traverse,
    Db, Embedder, EmbedderConfig,
};
use ultragraph_kb::types::GraphData;
use ultragraph_kb::{
    build_graph, calculate_centrality, detect_cycles, filter_edges_by_type, find_shortest_path,
    graph_keyword_search, index, index_with_cache, k_hop_bfs,
};

// Bundled visualization assets so `ug gen` can produce a self-contained
// output directory without needing the source tree at runtime.
const VIS_HTML: &str = include_str!("../../src/vis/visualization.html");
const VIS_MD: &str = include_str!("../../src/vis/visualization-how-to.md");

fn main() {
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
        "traverse" => run_traverse(cmd_args),
        "help" => print_help(),
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
fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if names.contains(&args[i].as_str()) && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn flag_value_or(args: &[String], names: &[&str], default: &str) -> String {
    flag_value(args, names).unwrap_or_else(|| default.to_string())
}

fn has_flag(args: &[String], flag: &str) -> bool {
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
            write_file(p, data);
            println!("Wrote {} to {}", label, p);
        }
        None => println!("{}", data),
    }
}

// ---------- Embedder / runtime helpers ----------

fn embedder_from_args(args: &[String]) -> Embedder {
    let cfg = EmbedderConfig::with_overrides(
        flag_value(args, &["--base-url"]),
        flag_value(args, &["--api-key"]),
        flag_value(args, &["--model"]),
        None,
        None,
    );
    Embedder::new(cfg).expect("failed to build embedder")
}

fn tokio_runtime() -> tokio::runtime::Runtime {
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
    println!("Generated index in {}", output);
}

fn run_graph(args: &[String]) {
    let input = flag_value_or(args, &["-i", "--input"], "ug-out/indexed-tree.json");
    let output = flag_value_or(args, &["-o", "--output"], "ug-out/graph.json");

    let index_json = fs::read_to_string(&input).expect("Failed to read input");
    let result = build_graph(index_json);
    write_file(&output, &result);
    println!("Generated graph in {}", output);
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

    println!("Analyzed graph:");
    println!("  - analysis.json (centrality)");
    println!("  - cycles.json (cycle detection)");
}

// full pipeline: ingest -> graph -> analyze -> search
fn run_gen(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_gen_help();
        return;
    }

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
    let db_path =
        flag_value(args, &["-d", "--db"]).unwrap_or_else(|| format!("{}/ug-db", output_dir));

    let pipeline_summary = if no_ingest {
        "index → graph → visualization"
    } else {
        "index → graph → visualization → LanceDB ingest"
    };
    println!("⚡ Full pipeline: {}", pipeline_summary);

    let _ = fs::create_dir_all(&output_dir);

    println!("▸ Indexing {}", input);
    let index_result = match cache {
        Some(c) => index_with_cache(input, c),
        None => index(input),
    };

    println!("▸ Building graph");
    let graph = build_graph(index_result.clone());

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

    println!("▸ Copying visualization assets");
    fs::write(format!("{}/index.html", output_dir), VIS_HTML).expect("Failed to write index.html");
    fs::write(format!("{}/README.md", output_dir), VIS_MD).expect("Failed to write README.md");

    println!("────────────────────────────────────────");
    println!("✓ Generated in {}/", output_dir);
    println!("  ✓ graph.json");
    println!("  ✓ indexed-tree.json");
    println!("  ✓ index.html (open in browser with HTTP server)");
    println!("  ✓ README.md");

    if no_ingest {
        println!("⚠ Skipping db-ingest (--no-ingest)");
        println!("Visit http://localhost:8080 to view the graph");
        return;
    }

    println!();
    println!("▸ Ingesting into {}", db_path);
    match run_gen_ingest(&graph, &db_path, args) {
        Ok((nodes_written, edges_written)) => {
            println!(
                "  ✓ {} nodes, {} edges embedded",
                nodes_written, edges_written
            );
        }
        Err(e) => {
            eprintln!("⚠ db-ingest skipped — {}", e);
            eprintln!("  Re-run later once the embedding endpoint is up:");
            eprintln!("    ug ingest -g {} -d {}", graph_path, db_path);
        }
    }

    println!("────────────────────────────────────────");
    println!("Visit http://localhost:8080 to view the graph");
    println!(
        "Run 'ug semantic_search \"hello\" -d {}' to perform a RAG query.",
        db_path
    );
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
        let stats = ingest_graph(&db, &embedder, &graph)
            .await
            .map_err(|e| format!("ingest: {}", e))?;
        Ok((stats.nodes_written, stats.edges_written))
    })
}

// ingest graph into LanceDB (only)
fn run_ingest(args: &[String]) {
    let graph_file = flag_value_or(args, &["-g", "--graph"], "ug-out/graph.json");
    let db_path = flag_value_or(args, &["-d", "--db"], "ug-out/ug-db");
    let create_indexes = has_flag(args, "--with-indexes");

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph file");
    let graph: GraphData = serde_json::from_str(&graph_json).expect("Failed to parse graph JSON");
    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();

    rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open lancedb");
        let stats = ingest_graph(&db, &embedder, &graph)
            .await
            .expect("ingest failed");
        println!(
            "Ingested {} nodes, {} edges into {}",
            stats.nodes_written, stats.edges_written, db_path
        );

        if create_indexes {
            // Indexes can fail on tiny tables (IvfPq needs a minimum number
            // of training rows). Surface the error but don't abort the
            // command - the table is still queryable without the indexes.
            if let Err(e) = db.try_create_vector_index().await {
                eprintln!("warning: vector index creation skipped: {}", e);
            } else {
                println!("Created vector index");
            }
            if let Err(e) = db.try_create_fts_index().await {
                eprintln!("warning: FTS index creation skipped: {}", e);
            } else {
                println!("Created FTS indexes (name, description)");
            }
        }
    });
}

// vector search on LanceDB (only)
fn run_semantic_search(args: &[String]) {
    if args.is_empty() {
        eprintln!(
            "Usage: ug semantic_search <query> [-d|--db <path>] [-k <limit>] [--filter <sql>] \\
                 [--base-url <url>] [--api-key <key>] [--model <name>] [-o|--output <file>]"
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
    let db_path = flag_value_or(args, &["-d", "--db"], "ug-out/ug-db");
    let limit: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let filter = flag_value(args, &["--filter"]);
    let output_path = flag_value(args, &["-o", "--output"]);
    let embedder = embedder_from_args(args);
    let rt = tokio_runtime();

    let result_json = rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open lancedb");
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

fn run_traverse(args: &[String]) {
    if args.is_empty() {
        eprintln!(
            "Usage: ug traverse <start-node-id> [-d|--db <path>] [-k|--hops <n>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let start = first_positional(args, &["-d", "--db", "-k", "--hops", "-o", "--output"])
        .expect("missing start node id");
    let db_path = flag_value_or(args, &["-d", "--db"], "ug-out/ug-db");
    let hops: u32 = flag_value(args, &["-k", "--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let output_path = flag_value(args, &["-o", "--output"]);

    let rt = tokio_runtime();
    let json = rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open lancedb");
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
    println!("gen [<path>]  Full pipeline: index → graph → visualization → LanceDB ingest");
    println!("  -i, --input <path>   Input directory (default: .)");
    println!("  -c, --cache <dir>    Cache directory for incremental indexing");
    println!("  -o, --output <dir>   Output directory (default: ug-out)");
    println!("  -d, --db <path>      LanceDB directory (default: <output>/ug-db)");
    println!("  --no-ingest          Skip the LanceDB ingest step");
    println!("  --base-url <url>     Embedding endpoint (default: http://localhost:8000/v1)");
    println!("  --api-key <key>      Embedding API key");
    println!("  --model <name>       Embedding model");
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
    println!("  search <graph> <keyword> Keyword search over graph nodes");
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
        "  gen [<path>]         Full pipeline: index → graph → visualization → LanceDB ingest"
    );
    println!("    -i, --input <path>  Input directory (default: .)");
    println!("    -c, --cache <dir>   Cache directory");
    println!("    -o, --output <dir>  Output directory (default: ug-out)");
    println!("    -d, --db <path>     LanceDB directory (default: <output>/ug-db)");
    println!("    --no-ingest         Skip the LanceDB ingest step");
    println!("    --base-url/--api-key/--model  Embedding endpoint overrides");
    println!();
    println!("  ingest               Embed graph nodes and write to LanceDB");
    println!("    -g, --graph <file>  Graph JSON (default: ug-out/graph.json)");
    println!("    -d, --db <path>    LanceDB directory (default: ug-out/ug-db)");
    println!("    --base-url <url>   Embedding endpoint (default: http://localhost:8000/v1)");
    println!("    --api-key <key>    Embedding API key (default: 1234)");
    println!(
        "    --model <name>     Embedding model (default: openai/Qwen3-Embedding-0.6B-4bit-DWQ)"
    );
    println!("    --with-indexes     Best-effort create vector + FTS indexes after ingest");
    println!();
    println!("  semantic_search <query>      Semantic vector search over the LanceDB nodes table");
    println!("    -d, --db <path>    LanceDB directory (default: ug-out/ug-db)");
    println!("    -k, --limit <n>    Top-k results (default: 10)");
    println!("    --filter <sql>     Optional SQL WHERE clause (hybrid search)");
    println!("    --base-url/--api-key/--model  Embedding endpoint overrides");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  traverse <node-id>   K-hop BFS using the LanceDB edges table");
    println!("    -d, --db <path>    LanceDB directory (default: ug-out/ug-db)");
    println!("    -k, --hops <n>     Max hops (default: 2)");
    println!("    -o, --output <file> Output file (optional)");
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
    println!("  ug gen -i ./lib -o ./ug-out");
    println!("  ug gen -i ./lib --no-ingest");
    println!("  ug ingest -g ug-out/graph.json -d ug-out/ug-db --with-indexes");
    println!("  ug semantic_search \"oauth login flow\" -d ug-out/ug-db -k 5");
    println!(
        "  ug semantic_search \"build a tree\" -d ug-out/ug-db --filter \"node_type = 'Function'\""
    );
    println!("  ug traverse file:src/index.ts -d ug-out/ug-db -k 2");
}
