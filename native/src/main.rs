use std::env;
use std::fs;
use std::path::Path;
use ultragraph_kb::storage::{
    self, ingest_graph, semantic_search as storage_semantic_search,
    traverse as storage_traverse, Db, Embedder, EmbedderConfig,
};
use ultragraph_kb::types::GraphData;
use ultragraph_kb::{
    build_graph, calculate_centrality, detect_cycles, filter_edges_by_type, find_shortest_path,
    index, index_with_cache, k_hop_bfs, search_by_keyword,
};

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
        "search" => run_search(cmd_args),
        "analyze" => run_analyze(cmd_args),
        "gen" => run_gen(cmd_args),
        "ingest" => run_ingest(cmd_args),
        "vsearch" => run_vsearch(cmd_args),
        "traverse" => run_traverse(cmd_args),
        "help" => {
            if let Some(_c) = cmd_args.first() {
                eprintln!("TODO: print command help");
            } else {
                print_help();
            }
        }
        _ => {
            eprintln!("Unknown command: {}", cmd);
            print_help();
            std::process::exit(1);
        }
    }
}

fn run_index(args: &[String]) {
    let mut path = ".".to_string();
    let mut cache_path: Option<String> = None;
    let mut output = "out/indexed-tree.json".to_string();

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        if arg == "-i" || arg == "--input" {
            if i + 1 < argc {
                path = args[i + 1].clone();
            }
            i += 2;
        } else if arg == "-o" || arg == "--output" {
            if i + 1 < argc {
                output = args[i + 1].clone();
            }
            i += 2;
        } else if arg == "-c" || arg == "--cache" {
            if i + 1 < argc {
                cache_path = Some(args[i + 1].clone());
            }
            i += 2;
        } else {
            path = arg;
            i += 1;
        }
    }

    let result = if let Some(ref cache) = cache_path {
        index_with_cache(path.clone(), cache.clone())
    } else {
        index(path)
    };

    if let Some(parent) = Path::new(&output).parent() {
        let _ = fs::create_dir_all(parent);
    }
    
    fs::write(&output, &result).expect("Failed to write output");
    println!("Generated index in {}", output);
}

fn run_graph(args: &[String]) {
    let mut input = "out/indexed-tree.json".to_string();
    let mut output = "out/graph.json".to_string();

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        if arg == "-i" || arg == "--input" {
            if i + 1 < argc {
                input = args[i + 1].clone();
            }
            i += 2;
        } else if arg == "-o" || arg == "--output" {
            if i + 1 < argc {
                output = args[i + 1].clone();
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    let index_json = fs::read_to_string(&input).expect("Failed to read input");
    let result = build_graph(index_json);

    if let Some(parent) = Path::new(&output).parent() {
        let _ = fs::create_dir_all(parent);
    }
    
    fs::write(&output, &result).expect("Failed to write output");
    println!("Generated graph in {}", output);
}

fn run_bfs(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ug bfs <graph-file> <start-node-id> [k] [-o|--output <file>]");
        std::process::exit(1);
    }

    let graph_file = args[0].clone();
    let start_node = args[1].clone();
    let k = if args.len() > 2 { args[2].parse().unwrap_or(1) } else { 1 };
    
    let output_path = if args.contains(&"-o".to_string()) || args.contains(&"--output".to_string()) {
        let idx = args.iter().position(|a| a == "-o" || a == "--output");
        idx.and_then(|i| args.get(i + 1).map(|s| s.clone()))
    } else {
        None
    };

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph file");
    let result = k_hop_bfs(graph_json, start_node, k as u32);

    if let Some(path) = output_path {
        fs::write(&path, &result).expect("Failed to write output");
        println!("Wrote BFS result to {}", path);
    } else {
        println!("{}", result);
    }
}

fn run_gen(args: &[String]) {
    let mut input = ".".to_string();
    let mut cache_path: Option<String> = None;
    let mut output_dir = "out".to_string();

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        if arg == "-i" || arg == "--input" {
            if i + 1 < argc {
                input = args[i + 1].clone();
            }
            i += 2;
        } else if arg == "-c" || arg == "--cache" {
            if i + 1 < argc {
                cache_path = Some(args[i + 1].clone());
            }
            i += 2;
        } else if arg == "-o" || arg == "--output" {
            if i + 1 < argc {
                output_dir = args[i + 1].clone();
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    let index_result = if let Some(ref cache) = cache_path {
        index_with_cache(input.clone(), cache.clone())
    } else {
        index(input)
    };

    let graph = build_graph(index_result.clone());
    let analysis = calculate_centrality(graph.clone());
    let cycles = detect_cycles(graph.clone());

    let _ = fs::create_dir_all(&output_dir);
    
    fs::write(format!("{}/graph.json", output_dir), &graph).expect("Failed to write graph.json");
    fs::write(format!("{}/indexed-tree.json", output_dir), &index_result).expect("Failed to write indexed-tree.json");
    fs::write(format!("{}/analysis.json", output_dir), &analysis).expect("Failed to write analysis.json");
    fs::write(format!("{}/cycles.json", output_dir), &cycles).expect("Failed to write cycles.json");

    println!("Generated in {}/:", output_dir);
    println!("  - graph.json");
    println!("  - indexed-tree.json");
    println!("  - analysis.json");
    println!("  - cycles.json");
}

fn run_filter(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ug filter <graph-file> <edge-type> [<edge-type>...] [-o|--output <file>]");
        std::process::exit(1);
    }

    let graph_file = args[0].clone();
    let edge_types: Vec<String> = args[1..].iter().take_while(|s| !s.starts_with('-')).cloned().collect();
    
    let output_path = args.iter().position(|a| a == "-o" || a == "--output")
        .and_then(|i| args.get(i + 1).map(|s| s.clone()));

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph");
    let edge_types_str: Vec<String> = edge_types.iter().map(|s| s.to_lowercase()).collect();
    let result = filter_edges_by_type(graph_json, edge_types_str);

    if let Some(path) = output_path {
        fs::write(&path, &result).expect("Failed to write output");
        println!("Wrote filtered edges to {}", path);
    } else {
        println!("{}", result);
    }
}

fn run_path(args: &[String]) {
    if args.len() < 3 {
        eprintln!("Usage: ug path <graph-file> <source> <target> [-o|--output <file>]");
        std::process::exit(1);
    }

    let graph_file = args[0].clone();
    let source = args[1].clone();
    let target = args[2].clone();
    
    let output_path = args.iter().position(|a| a == "-o" || a == "--output")
        .and_then(|i| args.get(i + 1).map(|s| s.clone()));

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph");
    let result = find_shortest_path(graph_json, source, target);

    if let Some(path) = output_path {
        fs::write(&path, &result).expect("Failed to write output");
        println!("Wrote path result to {}", path);
    } else {
        println!("{}", result);
    }
}

fn run_centrality(args: &[String]) {
    if args.len() < 1 {
        eprintln!("Usage: ug centrality <graph-file> [-o|--output <file>]");
        std::process::exit(1);
    }

    let graph_file = args[0].clone();
    let output_path = args.iter().position(|a| a == "-o" || a == "--output")
        .and_then(|i| args.get(i + 1).map(|s| s.clone()));

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph");
    let result = calculate_centrality(graph_json);

    if let Some(path) = output_path {
        fs::write(&path, &result).expect("Failed to write output");
        println!("Wrote centrality to {}", path);
    } else {
        println!("{}", result);
    }
}

fn run_cycles(args: &[String]) {
    if args.len() < 1 {
        eprintln!("Usage: ug cycles <graph-file> [-o|--output <file>]");
        std::process::exit(1);
    }

    let graph_file = args[0].clone();
    let output_path = args.iter().position(|a| a == "-o" || a == "--output")
        .and_then(|i| args.get(i + 1).map(|s| s.clone()));

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph");
    let result = detect_cycles(graph_json);

    if let Some(path) = output_path {
        fs::write(&path, &result).expect("Failed to write output");
        println!("Wrote cycle result to {}", path);
    } else {
        println!("{}", result);
    }
}

fn run_search(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ug search <graph-file> <keyword> [-t|--type <node-type>]... [-o|--output <file>]");
        std::process::exit(1);
    }

    let graph_file = args[0].clone();
    let mut keyword: Option<String> = None;
    let mut node_types: Vec<String> = Vec::new();
    let mut output_path: Option<String> = None;

    let mut i = 1;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        if arg == "-t" || arg == "--type" {
            if i + 1 < argc {
                node_types.push(args[i + 1].clone());
            }
            i += 2;
        } else if arg == "-o" || arg == "--output" {
            if i + 1 < argc {
                output_path = Some(args[i + 1].clone());
            }
            i += 2;
        } else if keyword.is_none() {
            keyword = Some(arg);
            i += 1;
        } else {
            i += 1;
        }
    }

    let kw = match keyword {
        Some(k) => k,
        None => {
            eprintln!("Usage: ug search <graph-file> <keyword> [-t|--type <node-type>]... [-o|--output <file>]");
            std::process::exit(1);
        }
    };

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph");
    let types_opt = if node_types.is_empty() { None } else { Some(node_types) };
    let result = search_by_keyword(graph_json, kw, types_opt);

    if let Some(path) = output_path {
        fs::write(&path, &result).expect("Failed to write output");
        println!("Wrote search result to {}", path);
    } else {
        println!("{}", result);
    }
}

fn run_analyze(args: &[String]) {
    let mut input = "out/graph.json".to_string();
    let mut output_dir = "out".to_string();

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        if arg == "-i" || arg == "--input" {
            if i + 1 < argc {
                input = args[i + 1].clone();
            }
            i += 2;
        } else if arg == "-o" || arg == "--output" {
            if i + 1 < argc {
                output_dir = args[i + 1].clone();
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    let graph_json = fs::read_to_string(&input).expect("Failed to read graph");
    let centrality = calculate_centrality(graph_json.clone());
    let cycles = detect_cycles(graph_json.clone());

    let _ = fs::create_dir_all(&output_dir);
    
    fs::write(format!("{}/analysis.json", output_dir), &centrality).expect("Failed to write analysis.json");
    fs::write(format!("{}/cycles.json", output_dir), &cycles).expect("Failed to write cycles.json");

    println!("Analyzed graph:");
    println!("  - analysis.json (centrality)");
    println!("  - cycles.json (cycle detection)");
}

fn build_embedder_from_args(
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Embedder {
    let mut cfg = EmbedderConfig::default();
    if let Some(b) = base_url {
        cfg.base_url = b;
    }
    if let Some(k) = api_key {
        cfg.api_key = k;
    }
    if let Some(m) = model {
        cfg.model = m;
    }
    Embedder::new(cfg).expect("failed to build embedder")
}

fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

fn run_ingest(args: &[String]) {
    let mut graph_file = "out/graph.json".to_string();
    let mut db_path = "out/kg_db".to_string();
    let mut base_url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut model: Option<String> = None;
    let mut create_indexes = false;

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        match arg.as_str() {
            "-g" | "--graph" => {
                if i + 1 < argc {
                    graph_file = args[i + 1].clone();
                }
                i += 2;
            }
            "-d" | "--db" => {
                if i + 1 < argc {
                    db_path = args[i + 1].clone();
                }
                i += 2;
            }
            "--base-url" => {
                if i + 1 < argc {
                    base_url = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "--api-key" => {
                if i + 1 < argc {
                    api_key = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "--model" => {
                if i + 1 < argc {
                    model = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "--with-indexes" => {
                create_indexes = true;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let graph_json = fs::read_to_string(&graph_file).expect("Failed to read graph file");
    let graph: GraphData = serde_json::from_str(&graph_json).expect("Failed to parse graph JSON");
    let embedder = build_embedder_from_args(base_url, api_key, model);
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

fn run_vsearch(args: &[String]) {
    if args.is_empty() {
        eprintln!(
            "Usage: ug vsearch <query> [-d|--db <path>] [-k <limit>] [--filter <sql>] \\
                 [--base-url <url>] [--api-key <key>] [--model <name>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let mut query: Option<String> = None;
    let mut db_path = "out/kg_db".to_string();
    let mut limit: usize = 10;
    let mut filter: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut model: Option<String> = None;
    let mut output_path: Option<String> = None;

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        match arg.as_str() {
            "-d" | "--db" => {
                if i + 1 < argc {
                    db_path = args[i + 1].clone();
                }
                i += 2;
            }
            "-k" | "--limit" => {
                if i + 1 < argc {
                    limit = args[i + 1].parse().unwrap_or(10);
                }
                i += 2;
            }
            "--filter" => {
                if i + 1 < argc {
                    filter = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "--base-url" => {
                if i + 1 < argc {
                    base_url = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "--api-key" => {
                if i + 1 < argc {
                    api_key = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "--model" => {
                if i + 1 < argc {
                    model = Some(args[i + 1].clone());
                }
                i += 2;
            }
            "-o" | "--output" => {
                if i + 1 < argc {
                    output_path = Some(args[i + 1].clone());
                }
                i += 2;
            }
            _ => {
                if query.is_none() {
                    query = Some(arg);
                }
                i += 1;
            }
        }
    }

    let q = query.expect("missing query");
    let embedder = build_embedder_from_args(base_url, api_key, model);
    let rt = tokio_runtime();

    let result_json = rt.block_on(async {
        let db = Db::open(&db_path).await.expect("failed to open lancedb");
        let hits = match filter.as_deref() {
            Some(f) => storage::hybrid_search(&db, &embedder, &q, limit, f)
                .await
                .expect("hybrid_search failed"),
            None => storage_semantic_search(&db, &embedder, &q, limit)
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

    if let Some(p) = output_path {
        fs::write(&p, &result_json).expect("Failed to write output");
        println!("Wrote search result to {}", p);
    } else {
        println!("{}", result_json);
    }
}

fn run_traverse(args: &[String]) {
    if args.is_empty() {
        eprintln!(
            "Usage: ug traverse <start-node-id> [-d|--db <path>] [-k|--hops <n>] [-o|--output <file>]"
        );
        std::process::exit(1);
    }

    let mut start_id: Option<String> = None;
    let mut db_path = "out/kg_db".to_string();
    let mut hops: u32 = 2;
    let mut output_path: Option<String> = None;

    let mut i = 0;
    let argc = args.len();
    while i < argc {
        let arg = args[i].clone();
        match arg.as_str() {
            "-d" | "--db" => {
                if i + 1 < argc {
                    db_path = args[i + 1].clone();
                }
                i += 2;
            }
            "-k" | "--hops" => {
                if i + 1 < argc {
                    hops = args[i + 1].parse().unwrap_or(2);
                }
                i += 2;
            }
            "-o" | "--output" => {
                if i + 1 < argc {
                    output_path = Some(args[i + 1].clone());
                }
                i += 2;
            }
            _ => {
                if start_id.is_none() {
                    start_id = Some(arg);
                }
                i += 1;
            }
        }
    }

    let start = start_id.expect("missing start node id");
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

    if let Some(p) = output_path {
        fs::write(&p, &json).expect("Failed to write output");
        println!("Wrote traverse result to {}", p);
    } else {
        println!("{}", json);
    }
}

fn print_help() {
    println!("UltraGraph-KB CLI");
    println!();
    println!("Usage: ug <command> [options]");
    println!();
    println!("Commands:");
    println!("  index [<path>]        Index a directory");
    println!("    -i, --input <path>   Input directory (default: .)");
    println!("    -o, --output <file>  Output file (default: out/indexed-tree.json)");
    println!("    -c, --cache <dir>    Cache directory for incremental indexing");
    println!();
    println!("  graph [<file>]        Build graph from index result");
    println!("    -i, --input <file>  Input index file (default: out/indexed-tree.json)");
    println!("    -o, --output <file> Output graph file (default: out/graph.json)");
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
    println!("    -t, --type <type>   Restrict to node type (repeatable, e.g. function/class/file)");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  analyze              Run full graph analysis (centrality + cycles)");
    println!("    -i, --input <file> Graph file (default: out/graph.json)");
    println!("    -o, --output <dir> Output directory (default: out)");
    println!();
    println!("  gen [<path>]         Generate graph + visualization");
    println!("    -i, --input <path>  Input directory (default: .)");
    println!("    -o, --output <dir> Output directory (default: out)");
    println!("    -c, --cache <dir>  Cache directory");
    println!();
    println!("  ingest               Embed graph nodes and write to LanceDB");
    println!("    -g, --graph <file>  Graph JSON (default: out/graph.json)");
    println!("    -d, --db <path>    LanceDB directory (default: out/kg_db)");
    println!("    --base-url <url>   Embedding endpoint (default: http://localhost:8000/v1)");
    println!("    --api-key <key>    Embedding API key (default: 1234)");
    println!("    --model <name>     Embedding model (default: openai/Qwen3-Embedding-0.6B-4bit-DWQ)");
    println!("    --with-indexes     Best-effort create vector + FTS indexes after ingest");
    println!();
    println!("  vsearch <query>      Semantic vector search over the LanceDB nodes table");
    println!("    -d, --db <path>    LanceDB directory (default: out/kg_db)");
    println!("    -k, --limit <n>    Top-k results (default: 10)");
    println!("    --filter <sql>     Optional SQL WHERE clause (hybrid search)");
    println!("    --base-url/--api-key/--model  Embedding endpoint overrides");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("  traverse <node-id>   K-hop BFS using the LanceDB edges table");
    println!("    -d, --db <path>    LanceDB directory (default: out/kg_db)");
    println!("    -k, --hops <n>     Max hops (default: 2)");
    println!("    -o, --output <file> Output file (optional)");
    println!();
    println!("Examples:");
    println!("  ug index -i ./src -o index.json");
    println!("  ug index ./src -o index.json");
    println!("  ug graph -i index.json -o graph.json");
    println!("  ug bfs graph.json file:src/index.ts 2");
    println!("  ug filter graph.json Contains Imports");
    println!("  ug centrality graph.json");
    println!("  ug cycles graph.json");
    println!("  ug search graph.json loadConfig --type function --type class");
    println!("  ug analyze");
    println!("  ug gen -i ./lib -o ./out");
    println!("  ug ingest -g out/graph.json -d out/kg_db --with-indexes");
    println!("  ug vsearch \"oauth login flow\" -d out/kg_db -k 5");
    println!("  ug vsearch \"build a tree\" -d out/kg_db --filter \"node_type = 'Function'\"");
    println!("  ug traverse file:src/index.ts -d out/kg_db -k 2");
}