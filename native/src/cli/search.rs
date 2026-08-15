//! Retrieval commands backed by the knowledge store rather than
//! `graph.json`: `search` (hybrid, with `semantic_search` as a retired
//! alias for `--no-expand`) and `traverse`.

use std::path::PathBuf;

use ultragraph::agent_tools::{self, Render};
use ultragraph::storage::{
    self, name_search as storage_name_search, search_kb as storage_search_kb, Direction,
    RankStrategy, SearchKbOptions,
};
use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET, C_YELLOW};

use super::agent::{emit_agent_result, load_agent_graph, print_node_ref_help, print_wildcard_help};
use super::analysis::resolve_node_ref;
use super::args::{first_positional, flag_value, has_flag, multi_flag, positionals};
use super::embed::{tokio_runtime, try_embedder_from_args};
use super::io::{die, write_or_print};
use super::store::{open_store_or_exit, single_store_spec_from_args};

/// Retired subcommand, kept as an alias so muscle memory and existing
/// scripts keep working: `ug semantic_search <q>` is `ug search <q>
/// --no-expand`. It stays out of `ug --help`; `ug semantic_search -h`
/// says where it went.
pub(crate) fn run_semantic_search(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_semantic_search_help();
        return;
    }
    let mut forwarded = args.to_vec();
    if !has_flag(args, "--expand") {
        forwarded.push("--no-expand".to_string());
    }
    run_hybrid_search(&forwarded);
}

// graphRAG hybrid search: RRF seeds → PPR (default) or MMR rerank → snippet-attached context
pub(crate) fn run_hybrid_search(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_hybrid_search_help();
        return;
    }
    if args.is_empty() {
        eprintln!(
            "Usage: ug search <query> [-n|--name <project>] [-k|--limit <n>] \\
                 [--filter <sql>] [--no-expand] [--direction <out|in|both>] \\
                 [-t|--edge-type <type>]... [--max-chars <n>] \\
                 [--snippets] [--repo-root <path>] \\
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
        "--db",
        "-o",
        "--output",
        "--dest",
        "--neo4j-uri",
        "--neo4j-user",
        "--neo4j-password",
        "--neo4j-database",
    ];
    let query = first_positional(args, &value_flags).unwrap_or_else(|| die(2, "missing query argument"));
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
    let include_snippets = has_flag(args, "--snippets");
    // `--expand` is only meaningful as the way the `semantic_search` alias
    // is overridden back to the default, so it is accepted but unlisted.
    let expand = !has_flag(args, "--no-expand") || has_flag(args, "--expand");
    let repo_root: PathBuf = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let output_path = flag_value(args, &["-o", "--output"]);

    let embedder = try_embedder_from_args(args);
    let rt = tokio_runtime();

    let result_json = rt.block_on(async {
        let Some(embedder) = embedder else {
            // No embeddings, no hybrid ranking. Fall back to a name
            // substring match so `ug search foo` still answers when the
            // embedding backend is down or unconfigured.
            let dim = ultragraph::storage::DEFAULT_EMBEDDING_DIM;
            let spec = single_store_spec_from_args(args, dim as u32);
            let store = open_store_or_exit(&spec).await;
            let hits = storage_name_search(store.as_ref(), &query, k, filter.as_deref())
                .await
                .unwrap_or_else(|e| die(1, format!("name search failed: {e}")));
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
                        "matched_by": "name",
                    })
                })
                .collect();
            return serde_json::to_string_pretty(&json).unwrap_or_default();
        };
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
        opts.expand = expand;
        opts.strategy = strategy;

        let result = storage_search_kb(store.as_ref(), &embedder, opts)
            .await
            .unwrap_or_else(|e| die(1, format!("hybrid search failed: {e}")));
        serde_json::to_string_pretty(&result).unwrap_or_default()
    });

    write_or_print(output_path.as_deref(), &result_json, "hybrid search result");
}

pub(crate) fn run_traverse(args: &[String]) {
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
            "--db",
            // Without these, `-t calls` leaves "calls" behind as a seed.
            "-t",
            "--edge-type",
            "-d",
            "--direction",
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
    // landed in a destination (see docs/MULTI-STORAGE-DEST.md).
    if flag_value(args, &["--dest"]).is_none() {
        let (graph, _raw, _path) = load_agent_graph(args);
        // Accept a bare name or file path, not just an exact node id.
        // Typing `ug traverse run_serve` is what people try first, and being told
        // "no node with id 'run_serve'" when the symbol plainly exists is
        // a bad way to learn that ids come from somewhere else.
        //
        // A wildcard is left for the tool to expand: several seeds are one
        // merged walk here, so `traverse 'handle_*'` should visit every
        // handler rather than demand the caller pick one.
        let starts: Vec<String> = starts
            .iter()
            .map(|s| {
                if ultragraph::pattern::is_pattern(s) {
                    s.clone()
                } else {
                    resolve_node_ref(&graph, s)
                }
            })
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
            .unwrap_or_else(|e| die(1, format!("traverse failed: {e}")));
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

/// What `search` does when no embedder is available.
///
/// Printed by `search -h` because it is the thing a caller most needs to
/// know before deciding whether an embedding backend is worth configuring:
/// the command doesn't hard-fail, but what comes back is a plain name
/// match, and the difference is invisible unless you look at `matched_by`.
fn print_embedder_fallback_note() {
    println!("{C_BOLD}Without an embedder:{C_RESET}  {C_DIM}degrades, does not fail{C_RESET}");
    println!(
        "  If no embedder can be built, {C_CYAN}search{C_RESET} warns on stderr and returns"
    );
    println!(
        "  a {C_BOLD}name-substring match{C_RESET} instead, every hit tagged {C_CYAN}\"matched_by\": \"name\"{C_RESET}. No vector"
    );
    println!("  or keyword ranking, no graph expansion, no snippets: enough to locate a");
    println!("  symbol, not a substitute for GraphRAG.");
    println!();
    println!("  {C_YELLOW}The fallback covers a backend that cannot be built, not one that is down.{C_RESET}");
    println!("  {C_DIM}The default local ONNX model failing to load or download lands here. A remote");
    println!("  {C_CYAN}--base-url{C_RESET}{C_DIM} endpoint always builds, so an unreachable one fails the query outright.{C_RESET}");
    println!();
    println!("  {C_BOLD}Hard dependencies{C_RESET} {C_DIM}(no fallback anywhere):{C_RESET}");
    println!("  {C_DIM}·{C_RESET} {C_CYAN}ug chat{C_RESET} · {C_CYAN}ug tour{C_RESET} — exit if the embedder cannot be built");
    println!("  {C_DIM}·{C_RESET} the MCP {C_BOLD}search{C_RESET} tool and {C_CYAN}POST /api/search/*{C_RESET} — error / 503");
    println!("  {C_DIM}·{C_RESET} vectors must be {C_BOLD}in{C_RESET} the db: a {C_CYAN}--no-embed{C_RESET} run ingests without them, so the");
    println!("    semantic channel stays empty until {C_CYAN}ug ingest{C_RESET} catches up");
    println!();
    println!("  {C_DIM}Embeddings-free alternatives: {C_RESET}{C_CYAN}ug find_symbols{C_RESET}{C_DIM} (exact names, wildcards),");
    println!("  {C_RESET}{C_CYAN}ug analyze{C_RESET}{C_DIM} (statistics, blast radius), {C_RESET}{C_CYAN}ug traverse{C_RESET}{C_DIM} (edge walks).{C_RESET}");
    println!();
}

fn print_semantic_search_help() {
    println!("  {C_CYAN}ug semantic_search{C_RESET}  {C_YELLOW}— merged into {C_CYAN}ug search --no-expand{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  The two commands took the same arguments and differed only in whether");
    println!("  results were expanded along graph edges, so that is now a {C_BOLD}flag{C_RESET} rather than");
    println!("  a second command:");
    println!();
    println!("    {C_CYAN}ug search{C_RESET} \"oauth login flow\" {C_BOLD}--no-expand{C_RESET}   {C_YELLOW}# matching symbols only{C_RESET}");
    println!("    {C_CYAN}ug search{C_RESET} \"oauth login flow\"               {C_YELLOW}# + related code, ranked{C_RESET}");
    println!();
    println!("  This name still works and forwards to exactly that, so existing scripts");
    println!("  keep running. See {C_CYAN}ug search -h{C_RESET} for the full option list.");
    println!();
    println!("  {C_DIM}One behaviour change: seeds are now RRF-fused (vector + full-text) rather");
    println!("  than vector-only, which finds exact identifiers more reliably. Each hit's");
    println!("  {C_RESET}{C_CYAN}matched_by{C_RESET}{C_DIM} field says which channel found it.{C_RESET}");
    println!();
}

fn print_hybrid_search_help() {
    println!(
        "  {C_BOLD}{C_YELLOW}★ ug search{C_RESET}  {C_YELLOW}— GraphRAG: semantic search → graph expansion → ranked context{C_RESET}"
    );
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  The most complete search: semantic + keyword seeds expanded along graph");
    println!("  edges, then ranked into one context bundle with source snippets —");
    println!("  what the MCP {C_BOLD}search{C_RESET} tool runs for agents. Best when you want to hand");
    println!("  code + its related code to an LLM, or answer \"where is X and what touches it\".");
    println!();
    println!("  {C_BOLD}--no-expand{C_RESET} turns off the graph half and returns just what matched —");
    println!("  the old {C_CYAN}ug semantic_search{C_RESET}, which now forwards here.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug search <query> [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>    Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}--db{C_RESET} <dir>           OverGraph directory (default: the -n project's, else the active one)");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>      Final results (default: 8)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>       SQL WHERE clause for semantic seed filter");
    println!("  {C_CYAN}--no-expand{C_RESET}          Seeds only: no graph walk, no neighbors. Cheaper, and the");
    println!("                         {C_DIM}right shape for disambiguation or a filtered inventory{C_RESET}");
    println!("  {C_CYAN}--direction{C_RESET} <dir>    outbound|inbound|both (default: both)");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>  Restrict expansion to edge type (repeatable)");
    println!("  {C_CYAN}--max-chars{C_RESET} <n>      Char budget for assembled context (default: 12000)");
    println!("  {C_CYAN}--snippets{C_RESET}           Read source slices for each hit (off by default — lean ids+locations;");
    println!("                         {C_DIM}follow with get_code for any you want to read){C_RESET}");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>   Repo root for snippet resolution (default: cwd)");
    println!("  {C_CYAN}--base-url/--api-key/--model/--embedding-dim{C_RESET}  Embedding endpoint overrides");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>  Write the result JSON to a file (omit for stdout)");
    println!();
    println!("{C_DIM}Ranking is Personalized PageRank over the edge graph, seeded by RRF");
    println!("(vector + full-text). Its tuning knobs (--strategy, --hops, --mmr-lambda,");
    println!("--ppr-*) still parse but are undocumented operator controls — the defaults");
    println!("are what you want. Backends without native PPR (Neo4j without GDS) fall back");
    println!("to MMR automatically. The full-text half is a channel *inside* that fusion,");
    println!("not a standalone mode: it runs only on the embedder-backed path.{C_RESET}");
    println!();
    print_embedder_fallback_note();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug search{C_RESET} \"oauth login flow\" -k 8");
    println!("  {C_CYAN}ug search{C_RESET} \"oauth login flow\" {C_BOLD}--no-expand{C_RESET}    {C_YELLOW}# just the symbols that matched{C_RESET}");
}

fn print_traverse_help() {
    println!("  {C_CYAN}ug traverse{C_RESET}  {C_YELLOW}— K-hop BFS using the OverGraph edges table{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug traverse <symbol>... [options]");
    println!();
    print_node_ref_help();
    println!("  Several seeds make {C_BOLD}one{C_RESET} merged walk, not one walk each.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>       Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}--db{C_RESET} <dir>              OverGraph directory (default: the -n project's, else the active one)");
    println!("  {C_CYAN}-k, --hops{C_RESET} <n>          Max hops 1-5 (default: 2)");
    println!("  {C_CYAN}-d, --direction{C_RESET} <dir>   {C_CYAN}outbound{C_RESET} what I depend on (default) · {C_CYAN}inbound{C_RESET} who depends");
    println!("                          on me · {C_CYAN}both{C_RESET}");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <type>  Restrict to edge type (repeatable; see {C_CYAN}ug graph_schema{C_RESET})");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>     Write the result JSON to a file (omit for stdout)");
    println!();
    print_wildcard_help();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug traverse{C_RESET} \"file:src/index.ts\"");
    println!("  {C_CYAN}ug traverse{C_RESET} run_serve -k 1              {C_YELLOW}# direct dependencies only{C_RESET}");
    println!("  {C_CYAN}ug traverse{C_RESET} <id1> <id2>                 {C_YELLOW}# one merged walk from several seeds{C_RESET}");
    println!("  {C_CYAN}ug traverse{C_RESET} {C_BOLD}'handle_*'{C_RESET} -d inbound       {C_YELLOW}# what reaches any handler{C_RESET}");
    println!("  {C_CYAN}ug traverse{C_RESET} {C_BOLD}'src/auth/*.ts'{C_RESET} -t imports   {C_YELLOW}# the import graph of one directory{C_RESET}");
}
