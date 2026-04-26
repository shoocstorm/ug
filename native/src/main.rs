use std::env;
use std::fs;
use std::path::Path;
use ultragraph_kb::{index, index_with_cache, build_graph, k_hop_bfs, filter_edges_by_type, find_shortest_path, calculate_centrality, detect_cycles};

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
        "analyze" => run_analyze(cmd_args),
        "gen" => run_gen(cmd_args),
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
    println!("  analyze              Run full graph analysis (centrality + cycles)");
    println!("    -i, --input <file> Graph file (default: out/graph.json)");
    println!("    -o, --output <dir> Output directory (default: out)");
    println!();
    println!("  gen [<path>]         Generate graph + visualization");
    println!("    -i, --input <path>  Input directory (default: .)");
    println!("    -o, --output <dir> Output directory (default: out)");
    println!("    -c, --cache <dir>  Cache directory");
    println!();
    println!("Examples:");
    println!("  ug index -i ./src -o index.json");
    println!("  ug index ./src -o index.json");
    println!("  ug graph -i index.json -o graph.json");
    println!("  ug bfs graph.json file:src/index.ts 2");
    println!("  ug filter graph.json Contains Imports");
    println!("  ug centrality graph.json");
    println!("  ug cycles graph.json");
    println!("  ug analyze");
    println!("  ug gen -i ./lib -o ./out");
}