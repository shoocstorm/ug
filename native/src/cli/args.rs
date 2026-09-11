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

#[cfg(test)]
mod tests {
    //! The hand-rolled parser every subcommand is built on.
    //!
    //! `ug` parses its own arguments rather than pulling in a parser crate,
    //! so a bug here is a bug in every command at once — and it does not
    //! surface as a parse error. A flag value collected as a positional is
    //! searched for as if it were a symbol name; a positional swallowed as a
    //! flag value simply disappears. Both produce a confident answer to a
    //! question nobody asked.

    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── flag values ─────────────────────────────────────────────────────────

    #[test]
    fn a_flag_value_is_the_token_after_the_flag() {
        assert_eq!(flag_value(&a(&["-n", "myrepo"]), &["-n", "--name"]).as_deref(), Some("myrepo"));
        assert_eq!(
            flag_value(&a(&["--name", "myrepo"]), &["-n", "--name"]).as_deref(),
            Some("myrepo")
        );
    }

    #[test]
    fn the_first_spelling_present_wins() {
        // Both spellings of one flag given: first occurrence in the argv
        // order, not first in the names list.
        let args = a(&["--name", "second", "-n", "first"]);
        assert_eq!(flag_value(&args, &["-n", "--name"]).as_deref(), Some("second"));
    }

    #[test]
    fn a_trailing_flag_has_no_value() {
        // `ug find_symbols -n` with nothing after it. Returning the flag name
        // itself, or panicking on the index, are both worse than None.
        assert_eq!(flag_value(&a(&["-n"]), &["-n", "--name"]), None);
        assert_eq!(flag_value(&a(&[]), &["-n"]), None);
    }

    #[test]
    fn an_absent_flag_falls_back_to_the_default() {
        assert_eq!(flag_value_or(&a(&[]), &["-o"], "out.json"), "out.json");
        assert_eq!(flag_value_or(&a(&["-o", "given.json"]), &["-o"], "out.json"), "given.json");
    }

    #[test]
    fn has_flag_is_an_exact_match_not_a_prefix() {
        assert!(has_flag(&a(&["--json"]), "--json"));
        assert!(!has_flag(&a(&["--jsonl"]), "--json"), "a longer flag is a different flag");
        assert!(!has_flag(&a(&["json"]), "--json"));
    }

    #[test]
    fn a_repeatable_flag_collects_every_value() {
        let args = a(&["-t", "function", "-t", "class", "--type", "file"]);
        assert_eq!(multi_flag(&args, &["-t", "--type"]), vec!["function", "class", "file"]);
    }

    #[test]
    fn a_repeatable_flag_with_no_value_ends_the_scan_cleanly() {
        assert_eq!(multi_flag(&a(&["-t", "function", "-t"]), &["-t"]), vec!["function"]);
        assert!(multi_flag(&a(&[]), &["-t"]).is_empty());
    }

    #[test]
    fn a_type_filter_is_lowercased_for_comparison() {
        // Typed by hand, so the case a user chose must not decide the match.
        assert_eq!(type_filter(&a(&["-t", "Function"]), &["-t"]), vec!["function"]);
    }

    #[test]
    fn a_limit_falls_back_when_it_is_absent_or_not_a_number() {
        assert_eq!(limit_or(&a(&[]), &["-l"], 20), 20);
        assert_eq!(limit_or(&a(&["-l", "5"]), &["-l"], 20), 5);
        // A typo takes the default rather than failing the command — the
        // limit is a convenience, not the question being asked.
        assert_eq!(limit_or(&a(&["-l", "lots"]), &["-l"], 20), 20);
        assert_eq!(limit_or(&a(&["-l", "-3"]), &["-l"], 20), 20, "a usize cannot be negative");
    }

    // ── positionals ─────────────────────────────────────────────────────────

    #[test]
    fn a_flags_value_is_not_mistaken_for_a_positional() {
        // The bug this guards: `--node-type Function` leaving "Function"
        // behind as a positional, which is then looked up as a symbol name.
        let args = a(&["--node-type", "Function", "signIn"]);
        assert_eq!(positionals(&args, AGENT_VALUE_FLAGS), vec!["signIn"]);
        assert_eq!(first_positional(&args, AGENT_VALUE_FLAGS).as_deref(), Some("signIn"));
    }

    #[test]
    fn a_value_less_flag_is_skipped_without_eating_the_next_token() {
        // `--json` takes no value, so the token after it is still a
        // positional. Treating every flag as value-taking loses it.
        let args = a(&["--json", "signIn"]);
        assert_eq!(positionals(&args, AGENT_VALUE_FLAGS), vec!["signIn"]);
    }

    #[test]
    fn several_positionals_come_back_in_order() {
        let args = a(&["alpha", "-n", "proj", "bravo", "--json", "charlie"]);
        assert_eq!(positionals(&args, AGENT_VALUE_FLAGS), vec!["alpha", "bravo", "charlie"]);
        assert_eq!(first_positional(&args, AGENT_VALUE_FLAGS).as_deref(), Some("alpha"));
    }

    #[test]
    fn no_positionals_is_none_rather_than_an_empty_string() {
        assert_eq!(first_positional(&a(&["-n", "proj"]), AGENT_VALUE_FLAGS), None);
        assert!(positionals(&a(&["--json"]), AGENT_VALUE_FLAGS).is_empty());
    }

    #[test]
    fn every_agent_value_flag_actually_consumes_its_value() {
        // A flag missing from the table is the silent half of this bug, so
        // the table is checked as a whole rather than one entry at a time.
        for flag in AGENT_VALUE_FLAGS {
            let args = a(&[flag, "VALUE", "realPositional"]);
            assert_eq!(
                positionals(&args, AGENT_VALUE_FLAGS),
                vec!["realPositional"],
                "{flag} did not consume its value"
            );
        }
    }

    // ── ids versus names ────────────────────────────────────────────────────

    #[test]
    fn node_ids_are_split_from_plain_names() {
        // The CLI takes untagged arguments where MCP has separate params, so
        // it has to guess from the id shape.
        let pos = a(&["function:src/a.rs:1:alpha", "bravo", "file:src/b.rs"]);
        let (ids, names) = split_ids_and_names(&pos);
        assert_eq!(ids, vec!["function:src/a.rs:1:alpha", "file:src/b.rs"]);
        assert_eq!(names, vec!["bravo"]);
    }

    #[test]
    fn a_list_of_plain_names_yields_no_ids() {
        let (ids, names) = split_ids_and_names(&a(&["alpha", "bravo"]));
        assert!(ids.is_empty());
        assert_eq!(names, vec!["alpha", "bravo"]);
    }

    #[test]
    fn splitting_nothing_yields_two_empty_lists() {
        let (ids, names) = split_ids_and_names(&[]);
        assert!(ids.is_empty() && names.is_empty());
    }

    // ── promoting a graph file to -i ────────────────────────────────────────

    #[test]
    fn a_leading_json_file_is_promoted_to_an_input_flag() {
        // `ug graph_cycles ./graph.json` has to work without the flag, and
        // the promoted path must not also remain a positional — it would be
        // looked up as a symbol name.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("graph.json");
        std::fs::write(&path, "{}").unwrap();
        let p = path.to_string_lossy().to_string();

        let (load_args, pos) = analysis_input(&a(&[&p, "extra"]));
        assert_eq!(flag_value(&load_args, &["-i"]).as_deref(), Some(p.as_str()));
        assert_eq!(pos, vec!["extra"], "the promoted path is no longer a positional");
    }

    #[test]
    fn a_json_name_that_is_not_a_file_stays_a_positional() {
        // Only an existing file is promoted. A symbol that happens to end in
        // ".json" is still a symbol.
        let (load_args, pos) = analysis_input(&a(&["not_a_real_file.json"]));
        assert_eq!(flag_value(&load_args, &["-i"]), None);
        assert_eq!(pos, vec!["not_a_real_file.json"]);
    }

    #[test]
    fn an_explicit_input_flag_stops_any_promotion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("graph.json");
        std::fs::write(&path, "{}").unwrap();
        let p = path.to_string_lossy().to_string();

        let (load_args, pos) = analysis_input(&a(&["-i", "chosen.json", &p]));
        assert_eq!(
            flag_value(&load_args, &["-i"]).as_deref(),
            Some("chosen.json"),
            "an explicit -i is not overridden by a positional"
        );
        assert_eq!(pos, vec![p], "and the positional is left alone");
    }
}
