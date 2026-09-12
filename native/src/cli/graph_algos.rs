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
use super::io::{CliError, CliResult};
use super::args::{analysis_input, emit_raw, flag_value, has_flag, limit_or, type_filter};

/// Resolve a user-supplied node reference to a node id. Accepts an exact
/// nodeId, a repo-relative (or suffix-unique) file path, a wildcard pattern,
/// or a symbol name ranked exact > prefix > substring. Ambiguity and misses
/// print candidates and exit — every downstream algorithm needs one id.
pub(crate) fn resolve_node_ref(graph: &GraphData, input: &str) -> Result<String, CliError> {
    if let Some(n) = graph.nodes.iter().find(|n| n.id == input) {
        return Ok(n.id.clone());
    }

    // A pattern has no ranking tiers to fall back through: it either picks
    // out one node or the user has to say which one they meant.
    if ultragraph::pattern::is_pattern(input) {
        return agent_tools::resolve_single_ref(graph, input)
            .map_err(|e| CliError::new(format!("✗ {}", e)));
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
        return Ok(file_hits[0].id.clone());
    }
    if file_hits.len() > 1 {
        return Err(ambiguous(input, &file_hits));
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
        return Err(CliError::new(format!(
            "✗ Nothing in the graph matches '{}' — look it up with {C_CYAN}ug find_symbols{C_RESET}, or pass a node id directly.",
            input
        )));
    }
    let best = hits.iter().map(|(r, _)| *r).min().unwrap_or(0);
    let best_hits: Vec<&GraphNode> = hits
        .iter()
        .filter(|(r, _)| *r == best)
        .map(|(_, n)| *n)
        .collect();
    if best_hits.len() > 1 {
        return Err(ambiguous(input, &best_hits));
    }
    Ok(best_hits[0].id.clone())
}

/// The candidates behind an ambiguous reference, as an error the user can
/// act on: they pick one id and re-run.
///
/// Built as a message rather than printed and exited, so the caller decides
/// what happens next — and so the list itself is checkable.
fn ambiguous(input: &str, candidates: &[&GraphNode]) -> CliError {
    let mut msg = format!(
        "'{}' matches {} nodes — re-run with one of these ids:",
        input,
        candidates.len()
    );
    for n in candidates.iter().take(15) {
        msg.push_str(&format!(
            "\n  {} {}  {}  id: {}",
            node_type_str(&n.node_type),
            n.name,
            node_loc(n),
            n.id
        ));
    }
    if candidates.len() > 15 {
        msg.push_str(&format!("\n  … and {} more", candidates.len() - 15));
    }
    CliError::new(msg)
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
    // Compared against the static name rather than a lowercased clone of it.
    let nt = node_type_str(&n.node_type);
    if !types.is_empty() && !types.iter().any(|t| t.eq_ignore_ascii_case(nt)) {
        return false;
    }
    if let Some(p) = file_prefix {
        if !n.file.as_deref().unwrap_or("").starts_with(p) {
            return false;
        }
    }
    true
}

pub(crate) fn run_graph_path(args: &[String]) -> CliResult {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_path_help();
        return Ok(());
    }
    let (load_args, pos) = analysis_input(args);
    if pos.len() < 2 {
        return Err(CliError::usage(
            "Usage: ug shortest_path <source> <target> [--strict] [-n|--name <project>]",
        ));
    }
    let (graph, _raw, _path) = load_agent_graph(&load_args)?;
    // The CLI resolves names/paths to ids before handing off; MCP and HTTP
    // pass ids directly.
    let source = resolve_node_ref(&graph, &pos[0])?;
    let target = resolve_node_ref(&graph, &pos[1])?;
    let strict = has_flag(args, "--strict");

    let result = agent_tools::shortest_path(&graph, &source, &target, strict);
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_shortest_path(&result, Render::Ansi, strict),
        "path result",
        true,
    )
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

pub(crate) fn run_graph_centrality(args: &[String]) -> CliResult {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_centrality_help();
        return Ok(());
    }
    let (load_args, _pos) = analysis_input(args);
    let types = type_filter(args, &["-t", "--type"]);
    let file_prefix = flag_value(args, &["-f", "--file"]);
    let top = limit_or(args, &["--top", "-l", "--limit"], 20);

    let (graph, _raw, _path) = load_agent_graph(&load_args)?;
    let centrality = calculate_centrality(&graph);

    // Raw output keeps the lib's shape so existing consumers of
    // analysis.json keep working.
    if emit_raw(args, &serde_json::to_string(&centrality).unwrap_or_default(), "centrality") {
        return Ok(());
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
    Ok(())
}

pub(crate) fn run_graph_cycles(args: &[String]) -> CliResult {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_cycles_help();
        return Ok(());
    }
    let (load_args, _pos) = analysis_input(args);
    let limit = limit_or(args, &["-l", "--limit"], 20);
    let min_len = limit_or(args, &["--min-len"], 0);
    let max_len = limit_or(args, &["--max-len"], usize::MAX);
    let file_prefix = flag_value(args, &["-f", "--file"]);

    let (graph, _raw, _path) = load_agent_graph(&load_args)?;
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

    // CI use: non-zero exit when the graph has cycles. The report above has
    // already listed them, so the error carries no message of its own.
    if has_flag(args, "--fail-on-cycle") && !cycles.is_empty() {
        return Err(CliError { code: 1, message: String::new() });
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    //! Turning what a user typed into one node, and filtering what gets
    //! scored.
    //!
    //! `shortest_path`, `graph_centrality` and `graph_cycles` all need a
    //! single unambiguous node id, so `resolve_node_ref` is what stands
    //! between `ug shortest_path parse other` and an answer about the wrong
    //! `parse`. Resolving to the wrong node does not fail — it reports a real
    //! path between two real nodes, neither of which the user asked about.
    //!
    //! Only the success paths are reachable from a test: every failure here
    //! ends in `std::process::exit`, including the ambiguity report.

    use super::*;

    pub(super) fn node(id: &str, name: &str, ty: GraphNodeType, file: Option<&str>) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: ty,
            file: file.map(str::to_string),
            start_line: Some(1),
            end_line: Some(9),
            ..Default::default()
        }
    }

    pub(super) fn graph(nodes: Vec<GraphNode>) -> GraphData {
        GraphData { nodes, edges: Vec::new(), stats: None, resolution: None }
    }

    /// `resolve_node_ref` for a reference expected to resolve.
    fn resolved(g: &GraphData, input: &str) -> String {
        resolve_node_ref(g, input).unwrap_or_else(|e| panic!("{input} should resolve: {e}"))
    }

    pub(super) fn sample() -> GraphData {
        graph(vec![
            node("file:src/auth/login.ts", "login.ts", GraphNodeType::File, Some("src/auth/login.ts")),
            node("file:src/db/query.ts", "query.ts", GraphNodeType::File, Some("src/db/query.ts")),
            node("function:src/auth/login.ts:signIn", "signIn", GraphNodeType::Function, Some("src/auth/login.ts")),
            node("function:src/db/query.ts:signInternal", "signInternal", GraphNodeType::Function, Some("src/db/query.ts")),
            node("class:src/db/query.ts:QueryBuilder", "QueryBuilder", GraphNodeType::Class, Some("src/db/query.ts")),
        ])
    }

    // ── resolving a reference ───────────────────────────────────────────────

    #[test]
    fn an_exact_node_id_resolves_to_itself() {
        let g = sample();
        // Checked first, before any name or path matching, so an id that also
        // happens to look like a name cannot be re-interpreted.
        assert_eq!(
            resolved(&g, "function:src/auth/login.ts:signIn"),
            "function:src/auth/login.ts:signIn"
        );
    }

    #[test]
    fn a_full_repo_relative_path_resolves_to_its_file_node() {
        let g = sample();
        assert_eq!(resolved(&g, "src/auth/login.ts"), "file:src/auth/login.ts");
    }

    #[test]
    fn a_path_suffix_is_enough_when_it_is_unique() {
        let g = sample();
        // Typing the whole repo-relative path is the thing this exists to
        // avoid; a basename that names one file is an answer.
        assert_eq!(resolved(&g, "login.ts"), "file:src/auth/login.ts");
        assert_eq!(resolved(&g, "auth/login.ts"), "file:src/auth/login.ts");
    }

    #[test]
    fn an_exact_name_beats_a_prefix_match() {
        let g = sample();
        // "signIn" is also a prefix of "signInternal". Without the ranking
        // tiers this would be ambiguous and exit; with them the exact match
        // is the answer and the command runs.
        assert_eq!(
            resolved(&g, "signIn"),
            "function:src/auth/login.ts:signIn"
        );
    }

    #[test]
    fn a_name_match_is_case_insensitive() {
        let g = sample();
        assert_eq!(
            resolved(&g, "querybuilder"),
            "class:src/db/query.ts:QueryBuilder"
        );
    }

    #[test]
    fn a_unique_prefix_resolves_without_the_rest_of_the_name() {
        let g = sample();
        assert_eq!(
            resolved(&g, "signInter"),
            "function:src/db/query.ts:signInternal"
        );
    }

    #[test]
    fn a_substring_resolves_when_nothing_matches_better() {
        let g = sample();
        // Rank 2: not exact, not a prefix. Only reached because no node
        // scores higher, which is what keeps a substring from stealing a
        // reference that had an exact match available.
        assert_eq!(
            resolved(&g, "Builder"),
            "class:src/db/query.ts:QueryBuilder"
        );
    }

    #[test]
    fn a_file_reference_is_not_answered_by_a_symbol_in_that_file() {
        // Paths are tried before symbol names, so a path-shaped input
        // resolves to the File node rather than to something declared in it.
        let g = sample();
        assert_eq!(resolved(&g, "query.ts"), "file:src/db/query.ts");
    }

    // ── which nodes get scored ──────────────────────────────────────────────

    #[test]
    fn no_filters_pass_everything() {
        let n = node("x", "x", GraphNodeType::Function, Some("src/a.rs"));
        assert!(node_passes(&n, &[], None));
    }

    #[test]
    fn the_type_filter_ignores_case() {
        let n = node("x", "x", GraphNodeType::Function, Some("src/a.rs"));
        // The flag is typed by hand, so "function", "Function" and "FUNCTION"
        // all have to mean the same thing.
        for spelling in ["function", "Function", "FUNCTION"] {
            assert!(node_passes(&n, &[spelling.to_string()], None), "{spelling}");
        }
        assert!(!node_passes(&n, &["class".to_string()], None));
    }

    #[test]
    fn several_types_are_an_or() {
        let n = node("x", "x", GraphNodeType::Class, None);
        let types = vec!["function".to_string(), "class".to_string()];
        assert!(node_passes(&n, &types, None));
    }

    #[test]
    fn the_file_filter_is_a_path_prefix() {
        let n = node("x", "x", GraphNodeType::Function, Some("src/auth/login.ts"));
        assert!(node_passes(&n, &[], Some("src/auth")));
        assert!(node_passes(&n, &[], Some("src/")));
        assert!(!node_passes(&n, &[], Some("src/db")));
    }

    #[test]
    fn a_node_with_no_file_fails_a_file_filter_rather_than_passing_it() {
        // Folder nodes carry no file. Letting them through a `-f src/auth`
        // filter would put unrelated rows in a filtered report.
        let n = node("folder:src", "src", GraphNodeType::Folder, None);
        assert!(!node_passes(&n, &[], Some("src")));
        assert!(node_passes(&n, &[], None), "but no filter still passes it");
    }

    #[test]
    fn both_filters_must_pass() {
        let n = node("x", "x", GraphNodeType::Function, Some("src/auth/login.ts"));
        assert!(node_passes(&n, &["function".to_string()], Some("src/auth")));
        assert!(!node_passes(&n, &["class".to_string()], Some("src/auth")));
        assert!(!node_passes(&n, &["function".to_string()], Some("src/db")));
    }

    // ── scoring rows ────────────────────────────────────────────────────────

    #[test]
    fn a_node_the_centrality_pass_never_scored_reads_as_zero() {
        let g = sample();
        let empty = CentralityResult {
            degree_centrality: Default::default(),
            betweenness_centrality: Default::default(),
        };
        let rows = centrality_rows(&g, &empty, &[], None);

        // Absent is zero, not skipped: an isolated node belongs in the report
        // at the bottom, and dropping it would misstate how many were scored.
        assert_eq!(rows.len(), g.nodes.len());
        assert!(rows.iter().all(|(_, d, b)| *d == 0.0 && *b == 0.0));
    }

    #[test]
    fn filters_reach_the_scored_rows() {
        let g = sample();
        let empty = CentralityResult {
            degree_centrality: Default::default(),
            betweenness_centrality: Default::default(),
        };
        let rows = centrality_rows(&g, &empty, &["function".to_string()], None);
        assert_eq!(rows.len(), 2, "only the two functions are scored");

        let scoped = centrality_rows(&g, &empty, &[], Some("src/db"));
        assert_eq!(scoped.len(), 3, "the file, the function and the class under src/db");
    }
}

#[cfg(test)]
mod resolution_error_tests {
    //! The refusals, which used to be unreachable.
    //!
    //! Until `resolve_node_ref` returned a `Result`, every one of these ended
    //! in `std::process::exit` — so a test that exercised one took the test
    //! runner down with it, and none of them was ever checked. They are the
    //! paths a user actually meets: a typo, and a name that means two things.
    //!
    //! The ambiguity list is the part worth pinning. It is not a diagnostic,
    //! it is the answer: the user reads it, picks one id, and re-runs.

    use super::tests::{graph, node, sample};
    use super::*;

    fn err(g: &GraphData, input: &str) -> CliError {
        match resolve_node_ref(g, input) {
            Ok(id) => panic!("{input} unexpectedly resolved to {id}"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_name_that_matches_nothing_says_where_to_look_it_up() {
        let g = sample();
        let e = err(&g, "nosuchsymbol");
        assert!(e.message.contains("nosuchsymbol"), "{e}");
        assert!(
            e.message.contains("find_symbols"),
            "the error has to name the way out: {e}"
        );
        assert_eq!(e.code, 1);
    }

    #[test]
    fn an_ambiguous_name_lists_the_ids_to_choose_between() {
        // Two functions named `handler` in different files. Picking one would
        // answer a question about the wrong symbol and look correct doing it.
        let g = graph(vec![
            node("function:src/a.rs:handler", "handler", GraphNodeType::Function, Some("src/a.rs")),
            node("function:src/b.rs:handler", "handler", GraphNodeType::Function, Some("src/b.rs")),
        ]);
        let e = err(&g, "handler");

        assert!(e.message.contains("matches 2 nodes"), "{e}");
        assert!(e.message.contains("function:src/a.rs:handler"), "{e}");
        assert!(e.message.contains("function:src/b.rs:handler"), "{e}");
        assert!(
            e.message.contains("re-run with one of these ids"),
            "the list is an instruction, not a diagnostic: {e}"
        );
    }

    #[test]
    fn an_ambiguous_file_path_lists_the_files_it_could_mean() {
        let g = graph(vec![
            node("file:src/a/config.ts", "config.ts", GraphNodeType::File, Some("src/a/config.ts")),
            node("file:src/b/config.ts", "config.ts", GraphNodeType::File, Some("src/b/config.ts")),
        ]);
        let e = err(&g, "config.ts");
        assert!(e.message.contains("matches 2 nodes"), "{e}");
        assert!(e.message.contains("src/a/config.ts"), "{e}");
        assert!(e.message.contains("src/b/config.ts"), "{e}");
    }

    #[test]
    fn a_long_candidate_list_is_truncated_with_a_count_of_the_rest() {
        // Twenty identically-named symbols is a wall of text, and the point
        // of the list is that the user can read it.
        let nodes: Vec<GraphNode> = (0..20)
            .map(|i| {
                node(
                    &format!("function:src/f{i}.rs:dup"),
                    "dup",
                    GraphNodeType::Function,
                    Some(&format!("src/f{i}.rs")),
                )
            })
            .collect();
        let e = err(&graph(nodes), "dup");

        assert!(e.message.contains("matches 20 nodes"), "{e}");
        assert_eq!(
            e.message.matches("  id: ").count(),
            15,
            "at most fifteen are listed: {e}"
        );
        assert!(e.message.contains("… and 5 more"), "{e}");
    }

    #[test]
    fn a_pattern_that_matches_several_is_refused_rather_than_narrowed() {
        // A wildcard has no ranking tiers to fall through — it either picks
        // out one node or the user has to say which they meant.
        let g = graph(vec![
            node("function:src/a.rs:handleGet", "handleGet", GraphNodeType::Function, Some("src/a.rs")),
            node("function:src/a.rs:handlePut", "handlePut", GraphNodeType::Function, Some("src/a.rs")),
        ]);
        let e = err(&g, "handle*");
        assert!(!e.message.is_empty(), "a refusal has to say something");
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_refused() {
        let g = sample();
        assert!(!err(&g, "nosuch*").message.is_empty());
    }

    #[test]
    fn a_pattern_matching_exactly_one_symbol_still_resolves() {
        // The refusals above must not make the useful case an error too.
        // `signIn*` would match `signInternal` as well, so the pattern has to
        // be one that genuinely names a single symbol.
        let g = sample();
        assert_eq!(
            resolve_node_ref(&g, "QueryB*").expect("one match resolves"),
            "class:src/db/query.ts:QueryBuilder"
        );
    }
}

#[cfg(test)]
mod command_tests {
    //! The three commands, driven end to end.
    //!
    //! Reachable at all only because they return `CliResult` now. What they
    //! print is checked by the renderer tests; what matters here is that a
    //! bad invocation comes back as a failure with the right exit code
    //! instead of ending the process.

    use super::tests::node;
    use super::*;
    use crate::project::EnvGuard;

    /// A `~/.ug` holding one project whose graph has a two-symbol call edge.
    fn project(env: &mut EnvGuard) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tmp");
        let ug_home = tmp.path().join("ug_home");
        let dir = ug_home.join("p");
        std::fs::create_dir_all(&dir).expect("dir");

        let g = GraphData {
            nodes: vec![
                node("function:src/a.rs:caller", "caller", GraphNodeType::Function, Some("src/a.rs")),
                node("function:src/a.rs:callee", "callee", GraphNodeType::Function, Some("src/a.rs")),
            ],
            edges: vec![crate::types::GraphEdge {
                source: "function:src/a.rs:caller".into(),
                target: "function:src/a.rs:callee".into(),
                edge_type: crate::types::GraphEdgeType::Calls,
            }],
            stats: None,
            resolution: None,
        };
        std::fs::write(dir.join("graph.json"), serde_json::to_string(&g).unwrap()).expect("graph");
        let meta = crate::project::ProjectMeta::new("p", tmp.path().to_str().unwrap(), 2, 1);
        crate::project::write_meta(&dir, &meta).expect("meta");
        env.set("UG_HOME", &ug_home);
        tmp
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shortest_path_needs_two_endpoints_and_says_so() {
        let mut env = EnvGuard::new();
        let _g = project(&mut env);
        let e = run_graph_path(&args(&["--json", "onlyone"])).expect_err("usage error");
        assert_eq!(e.code, 2, "a usage error is exit 2, not 1");
        assert!(e.message.contains("Usage: ug shortest_path"), "{e}");
    }

    #[test]
    fn shortest_path_reports_an_unresolvable_endpoint() {
        let mut env = EnvGuard::new();
        let _g = project(&mut env);
        let e = run_graph_path(&args(&["--json", "caller", "nosuchthing"]))
            .expect_err("the target does not resolve");
        assert!(e.message.contains("nosuchthing"), "{e}");
    }

    #[test]
    fn shortest_path_between_two_real_symbols_succeeds() {
        let mut env = EnvGuard::new();
        let _g = project(&mut env);
        run_graph_path(&args(&["--json", "caller", "callee"])).expect("a real path");
    }

    #[test]
    fn every_command_prints_its_help_without_touching_a_project() {
        // `-h` is answered before any project resolution, so it works in a
        // directory with no index at all — which is where someone reaching
        // for it usually is.
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("UG_HOME", tmp.path().join("empty"));

        run_graph_path(&args(&["-h"])).expect("path help");
        run_graph_centrality(&args(&["--help"])).expect("centrality help");
        run_graph_cycles(&args(&["-h"])).expect("cycles help");
    }

    #[test]
    fn a_missing_graph_is_reported_rather_than_ending_the_process() {
        // The whole point of the conversion: this used to call exit(1), so
        // no test could reach it.
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("UG_HOME", tmp.path().join("empty"));

        let e = run_graph_centrality(&args(&["--json", "-n", "nothing-here"]))
            .expect_err("no graph to read");
        assert!(e.message.contains("graph.json"), "{e}");
        assert!(e.message.contains("ug gen"), "the error says how to fix it: {e}");
    }

    #[test]
    fn fail_on_cycle_is_quiet_when_the_graph_is_acyclic() {
        // The fixture's one edge makes no cycle, so the CI flag must not
        // fail a clean graph.
        let mut env = EnvGuard::new();
        let _g = project(&mut env);
        run_graph_cycles(&args(&["-n", "p", "--fail-on-cycle"])).expect("no cycles");
    }

    #[test]
    fn fail_on_cycle_fails_when_there_is_one() {
        // A non-zero exit is the entire feature, and the message is empty
        // because the report above it already listed the cycles.
        let mut env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let ug_home = tmp.path().join("ug_home");
        let dir = ug_home.join("cyc");
        std::fs::create_dir_all(&dir).unwrap();
        let edge = |s: &str, t: &str| crate::types::GraphEdge {
            source: s.into(),
            target: t.into(),
            edge_type: crate::types::GraphEdgeType::Calls,
        };
        let g = GraphData {
            nodes: vec![
                node("function:src/a.rs:a", "a", GraphNodeType::Function, Some("src/a.rs")),
                node("function:src/b.rs:b", "b", GraphNodeType::Function, Some("src/b.rs")),
            ],
            edges: vec![
                edge("function:src/a.rs:a", "function:src/b.rs:b"),
                edge("function:src/b.rs:b", "function:src/a.rs:a"),
            ],
            stats: None,
            resolution: None,
        };
        std::fs::write(dir.join("graph.json"), serde_json::to_string(&g).unwrap()).unwrap();
        let meta = crate::project::ProjectMeta::new("cyc", tmp.path().to_str().unwrap(), 2, 2);
        crate::project::write_meta(&dir, &meta).unwrap();
        env.set("UG_HOME", &ug_home);

        let e = run_graph_cycles(&args(&["-n", "cyc", "--fail-on-cycle"]))
            .expect_err("the graph has a cycle");
        assert_eq!(e.code, 1);
        assert!(
            e.message.is_empty(),
            "the printed report is the message; repeating it would double it"
        );
    }
}
