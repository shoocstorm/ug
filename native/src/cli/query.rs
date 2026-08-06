//! `ug query` — whole-repo statistics, the CLI half of the `code_query`
//! MCP tool.
//!
//! Store-backed rather than `graph.json`-backed, unlike the other agent
//! tools: aggregation and reachability need indexed properties.

use ultragraph::agent_tools::Render;
use ultragraph::storage::{self, StoreSpec};
use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET, C_YELLOW};

use super::args::{first_positional, flag_value, has_flag, multi_flag};
use super::embed::tokio_runtime;
use super::store::{open_store_or_exit, single_store_spec_from_args};

/// `ug query` — whole-repo statistics, the CLI half of the `code_query`
/// MCP tool.
///
/// Store-backed rather than graph.json-backed, unlike its neighbours in
/// this section: aggregation and reachability need indexed properties.
/// It still needs no embedder, so the dim comes off the store's own
/// manifest instead of a model probe — statistics should not depend on
/// an embedding backend being reachable.
pub(crate) fn run_code_query(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_code_query_help();
        return;
    }
    if has_flag(args, "--list") || args.is_empty() {
        print_presets(has_flag(args, "--terse"));
        return;
    }

    let params = match code_query_params_from_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
    };

    let rt = tokio_runtime();
    rt.block_on(async {
        let mut spec = single_store_spec_from_args(args, 0);
        if let StoreSpec::Overgraph { path, .. } = &spec {
            let dim = storage::db::stored_embedding_dim(path)
                .unwrap_or(storage::embed::DEFAULT_EMBEDDING_DIM as u32);
            spec.set_embedding_dim(dim);
        } else {
            // Neo4j has no local manifest to read, and no GQL support
            // behind this trait either — the error below says so.
            spec.set_embedding_dim(storage::embed::DEFAULT_EMBEDDING_DIM as u32);
        }
        let store = open_store_or_exit(&spec).await;

        match ultragraph::code_query::run(store.as_ref(), &params).await {
            Ok(answer) => {
                println!(
                    "{}",
                    ultragraph::code_query::render::render(&answer, Render::Ansi)
                );
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    });
}

/// Parse `ug query`'s flags into the shared params struct.
///
/// Split out from [`run_code_query`] so it can be tested: the positional
/// preset makes this the fiddliest argument parsing in the CLI, and the
/// failure it produces — a flag's *value* read as a preset name — surfaces
/// as a confusing "preset and gql together" error rather than anything
/// resembling its cause.
pub(crate) fn code_query_params_from_args(
    args: &[String],
) -> Result<ultragraph::code_query::CodeQueryParams, String> {
    let gql = flag_value(args, &["-g", "--gql"]);
    let preset = flag_value(args, &["-p", "--preset"]).or_else(|| {
        // Only infer a positional preset when no query was given —
        // otherwise a stray value would be read as a second query.
        if gql.is_some() {
            return None;
        }
        first_positional(
            args,
            &[
                "-p", "--preset", "-g", "--gql", "-a", "--arg", "-n", "--name", "-k", "--limit",
                "-r", "--range",
                // `-o` carries the store path on this command, as it does
                // on every other store-backed one. Leaving it out made
                // `ug query <preset> -o <path>` read the path as a second
                // preset.
                "-o", "--output",
                "--dest", "--neo4j-uri", "--neo4j-user", "--neo4j-password", "--neo4j-database",
            ],
        )
    });

    let mut query_args = std::collections::BTreeMap::new();
    for pair in multi_flag(args, &["-a", "--arg"]) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("--arg expects key=value, got '{}'", pair))?;
        query_args.insert(k.trim().to_string(), v.trim().to_string());
    }

    Ok(ultragraph::code_query::CodeQueryParams {
        preset,
        gql,
        args: query_args,
        limit: flag_value(args, &["-k", "--limit"]).and_then(|s| s.parse().ok()),
        range: flag_value(args, &["-r", "--range"]),
        by_folder: has_flag(args, "--by-folder"),
    })
}

fn print_presets(terse: bool) {
    println!("  {C_CYAN}ug query{C_RESET}  {C_YELLOW}— built-in questions{C_RESET}");
    println!();
    let mut category = "";
    for p in ultragraph::code_query::presets::all() {
        if p.category.as_str() != category {
            category = p.category.as_str();
            println!("{C_BOLD}{}{C_RESET}", category);
        }
        let args: Vec<String> = p
            .params
            .iter()
            .map(|q| {
                if q.default.is_none() {
                    format!("{}=<required>", q.name)
                } else {
                    q.name.to_string()
                }
            })
            .collect();
        if terse {
            // Names + args only — for an agent that already knows the
            // catalog and wants to scan it cheaply.
            let tail = if args.is_empty() {
                String::new()
            } else {
                format!("  {C_DIM}args: {}{C_RESET}", args.join(", "))
            };
            println!("  {C_CYAN}{:<26}{C_RESET}{}", p.name, tail);
        } else {
            println!("  {C_CYAN}{:<26}{C_RESET} {}", p.name, p.description);
            if !args.is_empty() {
                println!("  {:<26} {C_DIM}args: {}{C_RESET}", "", args.join(", "));
            }
        }
    }
    println!();
    println!("  Run one:  {C_CYAN}ug query <preset> [--arg key=value]{C_RESET}");
    println!("  Raw GQL:  {C_CYAN}ug query --gql \"MATCH (n:Function) RETURN count(*) AS c\"{C_RESET}");
}

fn print_code_query_help() {
    println!("  {C_CYAN}ug query{C_RESET}  {C_YELLOW}— whole-repo statistics over the indexed graph{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Counts, groups, distributions and blast radius — the questions that");
    println!("  would otherwise mean grepping every file. Same engine as the");
    println!("  {C_CYAN}code_query{C_RESET} MCP tool. Read-only.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug query <preset> [options]");
    println!("        ug query --gql \"<query>\" [options]");
    println!("        ug query --list");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-p, --preset <name>{C_RESET}    Built-in question to run (also accepted as a positional)");
    println!("  {C_CYAN}-a, --arg <k=v>{C_RESET}        Preset argument, repeatable (e.g. --arg target=src/a.ts)");
    println!("  {C_CYAN}-g, --gql <query>{C_RESET}      Raw OverGraph GQL, when no preset fits");
    println!("  {C_CYAN}-k, --limit <n>{C_RESET}        Rows to display (default 20) — shorthand for --range 1-N");
    println!("  {C_CYAN}-r, --range <window>{C_RESET}   Which rows to show, 1-based and inclusive:");
    println!("                         {C_DIM}20 · 11-35 · 34-end{C_RESET} — page a result without re-reading it");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}   Project to query (default: the active one)");
    println!("      {C_CYAN}--list{C_RESET}             List every preset and exit");
    println!("      {C_CYAN}--terse{C_RESET}            With --list: names + args only, no description prose");
    println!("      {C_CYAN}--by-folder{C_RESET}        Print a \"by file\" concentration summary above the table");
    println!("                         {C_DIM}(surfaces dynamic-dispatch piles in dead_code / untested_symbols){C_RESET}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_DIM}# how many functions are longer than 50 lines, and where{C_RESET}");
    println!("  ug query long_functions_by_folder");
    println!();
    println!("  {C_DIM}# raise the threshold{C_RESET}");
    println!("  ug query long_functions --arg min_loc=150");
    println!();
    println!("  {C_DIM}# what breaks if I change this file{C_RESET}");
    println!("  ug query impact --arg target=native/src/storage/store.rs");
    println!();
    println!("  {C_DIM}# page through a long result without re-reading what you saw{C_RESET}");
    println!("  ug query dead_code --range 21-40");
    println!();
    println!("  {C_DIM}# anything the presets don't cover{C_RESET}");
    println!("  ug query --gql \"MATCH (n:Function) WHERE n.params > 6 RETURN count(*) AS c\"");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn a_bare_preset_name_is_the_positional() {
        let p = code_query_params_from_args(&argv("long_functions")).unwrap();
        assert_eq!(p.preset.as_deref(), Some("long_functions"));
        assert!(p.gql.is_none());
    }

    /// The bug this test exists for: `-o` carries the store path, and
    /// leaving it out of the skip list made the path itself read as a
    /// second preset — surfacing as "preset and gql together", which
    /// points nowhere near the cause.
    #[test]
    fn a_flag_value_is_never_mistaken_for_the_positional_preset() {
        for line in [
            "repo_census -o /tmp/db",
            "repo_census --output /tmp/db",
            "repo_census -n myproject",
            "repo_census -k 5",
            "repo_census -a target=src/a.rs",
        ] {
            let p = code_query_params_from_args(&argv(line)).unwrap();
            assert_eq!(
                p.preset.as_deref(),
                Some("repo_census"),
                "parsed the wrong positional from `{line}`"
            );
        }
    }

    #[test]
    fn an_explicit_query_suppresses_positional_preset_inference() {
        let args = vec![
            "--gql".to_string(),
            "MATCH (n) RETURN count(*) AS c".to_string(),
            "-o".to_string(),
            "/tmp/db".to_string(),
        ];
        let p = code_query_params_from_args(&args).unwrap();
        assert!(p.preset.is_none(), "got preset {:?}", p.preset);
        assert!(p.gql.is_some());
    }

    /// Same failure as the `-o` bug: a flag whose value looks like a bare
    /// word gets read as the positional preset, and the error that surfaces
    /// points nowhere near the cause.
    #[test]
    fn a_range_value_is_not_mistaken_for_the_positional_preset() {
        for line in ["dead_code -r 11-35", "dead_code --range 34-end"] {
            let p = code_query_params_from_args(&argv(line)).unwrap();
            assert_eq!(p.preset.as_deref(), Some("dead_code"), "from `{line}`");
            assert!(p.range.is_some(), "from `{line}`");
        }
    }

    #[test]
    fn repeated_arg_flags_accumulate() {
        let p = code_query_params_from_args(&argv(
            "layering_violations -a from_prefix=src/ui -a to_prefix=src/db",
        ))
        .unwrap();
        assert_eq!(p.args["from_prefix"], "src/ui");
        assert_eq!(p.args["to_prefix"], "src/db");
    }

    #[test]
    fn a_malformed_arg_is_rejected_rather_than_dropped() {
        let err = code_query_params_from_args(&argv("impact -a target")).unwrap_err();
        assert!(err.contains("key=value"), "{err}");
    }
}
