//! Structural graph analysis over a project's `graph.json`:
//! `shortest_path`, `graph_centrality`, `graph_cycles`.
//!
//! What is left here is what nothing else can do — betweenness needs
//! all-pairs shortest paths and cycle detection needs an unbounded DFS,
//! and neither is expressible as a query. The node-reference resolver
//! (name / path / wildcard / id → one node id) also lives here, since
//! these are the commands that need a single unambiguous id.
//!
//! Each of these reads a project's `graph.json` — selected with
//! `-n/--name`, else the cwd's project, else the most recently generated
//! one — the same resolution the agent tools use. `-i/--input` still
//! accepts an explicit `graph.json` for one-off files, and a legacy
//! `<graph-file>` first positional is still honoured.
//!
//! Output: a readable report by default, raw JSON with `--json`, and
//! `-o/--output <file>` writes that JSON to disk.

use ultragraph::agent_tools::{
    self, by_id_map, node_loc, node_type_str, strip_file_id_prefix, Render,
};
use ultragraph::types::{GraphData, GraphNode, GraphNodeType};
use ultragraph::CentralityResult;
use ultragraph::{
    calculate_centrality, detect_cycles, C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW,
};

use super::agent::{emit_agent_result, load_agent_graph, print_wildcard_help};
use super::args::{analysis_input, emit_raw, flag_value, has_flag, limit_or, type_filter};

/// Resolve a user-supplied node reference to a node id. Accepts an exact
/// nodeId, a repo-relative (or suffix-unique) file path, a wildcard pattern,
/// or a symbol name ranked exact > prefix > substring. Ambiguity and misses
/// print candidates and exit — every downstream algorithm needs one id.
pub(crate) fn resolve_node_ref(graph: &GraphData, input: &str) -> String {
    if let Some(n) = graph.nodes.iter().find(|n| n.id == input) {
        return n.id.clone();
    }

    // A pattern has no ranking tiers to fall back through: it either picks
    // out one node or the user has to say which one they meant.
    if ultragraph::pattern::is_pattern(input) {
        match agent_tools::resolve_single_ref(graph, input) {
            Ok(id) => return id,
            Err(e) => {
                eprintln!("✗ {}", e);
                std::process::exit(1);
            }
        }
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

pub(crate) fn run_graph_path(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_path_help();
        return;
    }
    let (load_args, pos) = analysis_input(args);
    if pos.len() < 2 {
        eprintln!("Usage: ug shortest_path <source> <target> [--strict] [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, _path) = load_agent_graph(&load_args);
    // The CLI resolves names/paths to ids before handing off; MCP and HTTP
    // pass ids directly.
    let source = resolve_node_ref(&graph, &pos[0]);
    let target = resolve_node_ref(&graph, &pos[1]);
    let strict = has_flag(args, "--strict");

    let result = agent_tools::shortest_path(&graph, &source, &target, strict);
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
    centrality: &CentralityResult,
    types: &[String],
    file_prefix: Option<&str>,
) -> Vec<(&'a GraphNode, f64, f64)> {
    graph
        .nodes
        .iter()
        .filter(|n| node_passes(n, types, file_prefix))
        .map(|n| {
            (
                n,
                centrality.degree_centrality.get(&n.id).copied().unwrap_or(0.0),
                centrality.betweenness_centrality.get(&n.id).copied().unwrap_or(0.0),
            )
        })
        .collect()
}

pub(crate) fn run_graph_centrality(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_centrality_help();
        return;
    }
    let (load_args, _pos) = analysis_input(args);
    let types = type_filter(args, &["-t", "--type"]);
    let file_prefix = flag_value(args, &["-f", "--file"]);
    let top = limit_or(args, &["--top", "-l", "--limit"], 20);

    let (graph, _raw, _path) = load_agent_graph(&load_args);
    let centrality = calculate_centrality(&graph);

    // Raw output keeps the lib's shape so existing consumers of
    // analysis.json keep working.
    if emit_raw(args, &serde_json::to_string(&centrality).unwrap_or_default(), "centrality") {
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

pub(crate) fn run_graph_cycles(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_cycles_help();
        return;
    }
    let (load_args, _pos) = analysis_input(args);
    let limit = limit_or(args, &["-l", "--limit"], 20);
    let min_len = limit_or(args, &["--min-len"], 0);
    let max_len = limit_or(args, &["--max-len"], usize::MAX);
    let file_prefix = flag_value(args, &["-f", "--file"]);

    let (graph, _raw, _path) = load_agent_graph(&load_args);
    let by_id = by_id_map(&graph);
    let all = detect_cycles(&graph).cycles;

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

/// Options every graph-analysis command shares.
fn print_graph_common_options() {
    println!("  {C_CYAN}-n, --name{C_RESET} <project>  Project under ~/.ug (default: cwd's project, else most recent)");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>    Explicit graph.json (overrides --name)");
    println!("  {C_CYAN}--json{C_RESET}                Print the raw JSON result instead of a report");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>   Write the raw JSON to a file");
}

fn print_graph_path_help() {
    println!("  {C_CYAN}ug shortest_path{C_RESET}  {C_YELLOW}— how are two nodes connected?{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug shortest_path <source> <target> [options]");
    println!();
    println!("  Source/target take a node id, a file path, a symbol name, or a wildcard —");
    println!("  but each has to land on {C_BOLD}exactly one{C_RESET} node, since \"is A connected to B\"");
    println!("  has a different answer for every candidate. Ambiguity lists the ids to pick from.");
    println!();
    println!("  Edges are directed (imports/calls/contains flow source→target); if no forward");
    println!("  path exists the reverse direction is tried and labeled as such.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--strict{C_RESET}              Don't retry the reverse direction");
    print_graph_common_options();
    println!();
    print_wildcard_help();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug shortest_path{C_RESET} run_gen run_ingest");
    println!("  {C_CYAN}ug shortest_path{C_RESET} src/a.ts src/b.ts --strict");
    println!("  {C_CYAN}ug shortest_path{C_RESET} file:src/a.ts file:src/b.ts -n my-repo");
    println!("  {C_CYAN}ug shortest_path{C_RESET} {C_BOLD}'*Controller'{C_RESET} save_user   {C_YELLOW}# ok when one class matches{C_RESET}");
}

fn print_graph_centrality_help() {
    println!("  {C_CYAN}ug graph_centrality{C_RESET}  {C_YELLOW}— degree & betweenness centrality{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_centrality [options]");
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
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_cycles [options]");
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
