//! `ug index` and `ug graph` — the two structural pipeline steps that
//! `ug gen` runs for you. Kept separately callable so a partial re-run
//! (re-parse without re-embedding) is possible.

use std::fs;

use ultragraph::{build_graph, index, index_with_cache, C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW};

use crate::project;

use super::args::{first_positional, flag_value, has_flag};
use super::io::{die, write_file};

pub(crate) fn run_index(args: &[String]) {
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

pub(crate) fn run_graph(args: &[String]) {
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

    let index_json = fs::read_to_string(&input)
        .unwrap_or_else(|e| die(1, format!("failed to read {input}: {e}")));
    let result = build_graph(index_json);
    write_file(&output, &result);
    println!(
        "{C_GREEN}✓{C_RESET} Generated graph in {C_BOLD}{}{C_RESET}",
        output
    );
}

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
