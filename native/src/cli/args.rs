//! Argument parsing shared by every subcommand.
//!
//! `ug` hand-rolls its CLI parsing rather than pulling in a parser crate,
//! so these few primitives — flag lookup, positional extraction, the
//! `--json`/`-o` output mode — are what every `run_*` function is built
//! from. They all take the raw `&[String]` tail of `env::args()`.

use std::path::Path;

use ultragraph::agent_tools::looks_like_node_id;

use super::io::write_or_print;

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
pub(crate) fn multi_flag(args: &[String], names: &[&str]) -> Vec<String> {
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
pub(crate) fn first_positional(args: &[String], value_flags: &[&str]) -> Option<String> {
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
    // Long spellings of the agent-tool filters. Without them, the value
    // after the flag (`--node-type Function`) is collected as a positional
    // and searched for as if it were a symbol name.
    "--node-type",
    "--file-prefix",
    "--max-files",
];

/// Split an analysis command's arguments into (args used to locate the
/// graph, remaining positionals). A first positional that is an existing
/// `.json` file is promoted to `-i` and dropped from the positionals, so
/// naming a graph file directly works without the flag.
pub(crate) fn analysis_input(args: &[String]) -> (Vec<String>, Vec<String>) {
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
pub(crate) enum Emit {
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
pub(crate) fn emit_raw(args: &[String], json: &str, label: &str) -> bool {
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
pub(crate) fn type_filter(args: &[String], names: &[&str]) -> Vec<String> {
    multi_flag(args, names)
        .iter()
        .map(|t| t.to_lowercase())
        .collect()
}

pub(crate) fn limit_or(args: &[String], names: &[&str], default: usize) -> usize {
    flag_value(args, names)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Flags-with-values shared by the agent-tool commands, so positional
/// arguments can be told apart from flag values.
pub(crate) const AGENT_VALUE_FLAGS: &[&str] = &[
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
    "--max-files",
    "--include",
    "--range",
    "-r",
    "--node-type",
    "--file-prefix",
    "--start-line",
    "--end-line",
    "--direction",
    "-d",
];

/// Every non-flag positional, skipping flag/value pairs (multi-positional
/// sibling of `first_positional`).
pub(crate) fn positionals(args: &[String], value_flags: &[&str]) -> Vec<String> {
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

/// Split bare positionals into (node ids, names/paths). The CLI takes
/// untagged arguments where MCP and HTTP have separate `node_id` / `name`
/// params, so it guesses using the indexer's id shape.
pub(crate) fn split_ids_and_names(pos: &[String]) -> (Vec<String>, Vec<String>) {
    pos.iter()
        .cloned()
        .partition(|s| looks_like_node_id(s))
}
