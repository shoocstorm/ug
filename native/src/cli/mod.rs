//! The `ug` command-line interface.
//!
//! One module per command group, each owning its `run_*` entry point *and*
//! the `print_*_help` that documents it — help text is the largest part of
//! this CLI and keeping it beside the flag parsing it describes is the only
//! way the two stay in step. Shared machinery lives in [`args`] (flag and
//! positional parsing), [`io`] (writing results, exiting with a message),
//! [`embed`] (embedder + runtime construction) and [`dest`] (resolving
//! `--dest` into stores).
//!
//! [`run`] is the whole entry point: everything below `fn main` starts here.

pub(crate) mod agent;
pub(crate) mod graph_algos;
pub(crate) mod api;
pub(crate) mod app;
pub(crate) mod analyze;
pub(crate) mod args;
pub(crate) mod chat;
pub(crate) mod config;
pub(crate) mod connect;
pub(crate) mod demo;
pub(crate) mod doctor;
pub(crate) mod embed;
pub(crate) mod gen;
pub(crate) mod help;
pub(crate) mod hook;
pub(crate) mod index;
pub(crate) mod ingest;
pub(crate) mod io;
pub(crate) mod projects;
pub(crate) mod scope;
pub(crate) mod search;
pub(crate) mod dest;
pub(crate) mod tour;
pub(crate) mod update;
pub(crate) mod upgrade;

use std::env;

use ultragraph::{C_BOLD, C_CYAN, C_RESET};

use crate::{mcp, serve};

/// Parse the process arguments and run the requested command.
///
/// Everything that has to happen before any subcommand sees an argument —
/// the colour gate, `.env` loading, the global flags that no subcommand's
/// parser should ever see as a positional — happens here, once.
pub fn run() {
    io::install_panic_hook();

    // Colour gate, resolved once before any command runs. `Render::Ansi`
    // output (the agent-tool commands and `ug analyze`) consults this so a
    // non-tty consumer — a pipe, an LLM, a log — gets plain text without
    // every format string branching. `--no-color` and the `NO_COLOR` env
    // var (https://no-color.org) force it off; otherwise it follows the
    // terminal. Human-facing banners keep their colour in a terminal
    // regardless.
    let raw_args: Vec<String> = env::args().collect();
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let color_off = args::has_flag(&raw_args, "--no-color")
        || env::var_os("NO_COLOR").is_some()
        || !tty;
    ultragraph::color::set(!color_off);

    // Progress meters follow the terminal, not the colour gate — see
    // [`ultragraph::progress`]. A `\r` repaint is a meter on a terminal and
    // ~100 duplicate lines anywhere else, so a pipe gets only each phase's
    // final line. `UG_PROGRESS=1` opts a pipe back in: the KB Manager's
    // wizard reads the frames to drive its live log viewer.
    ultragraph::progress::set(tty || env::var_os("UG_PROGRESS").is_some());

    // Load environment defaults from `.env` (in CWD or any parent
    // directory). Real env vars still win — `dotenvy::dotenv` does not
    // override values already set in the process environment. Quiet
    // when no `.env` is present.
    let _ = dotenvy::dotenv();

    // `--no-logo` is consumed here rather than passed through, so no
    // subcommand's argument parser can mistake it for a positional. Same for
    // `--no-banner`, which silences the "which project am I working against"
    // line every project-scoped command prints to stderr (see [`scope`]).
    let mut argv: Vec<String> = raw_args;
    let logo_flagged_off = argv.iter().any(|a| a == "--no-logo" || a == "--quiet-logo");
    if argv.iter().any(|a| a == "--no-banner") {
        scope::silence();
    }
    argv.retain(|a| {
        a != "--no-logo" && a != "--quiet-logo" && a != "--no-color" && a != "--no-banner"
    });
    let argv = argv;

    if !help::suppress_logo(&argv, logo_flagged_off) {
        help::print_logo();
    }

    if argv.len() >= 2 && (argv[1] == "-v" || argv[1] == "--version") {
        println!("ug version {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if argv.len() < 2 {
        // No subcommand: just start the server. `ug serve` is safe even
        // with zero generated projects — it shows the KB Manager wizard
        // instead of erroring — so this removes the old "run gen, then
        // remember to run serve" two-step for the common case.
        eprintln!(
            "{C_CYAN}▸{C_RESET} No command given — starting {C_BOLD}ug serve{C_RESET}. Run {C_CYAN}ug help{C_RESET} for other commands."
        );
        serve::run_serve(&[]);
        return;
    }

    dispatch(&argv[1], &argv[2..]);
}

/// Map a subcommand name to its entry point.
fn dispatch(cmd: &str, cmd_args: &[String]) {
    match cmd {
        // Primary entry points.
        "gen" => gen::run_gen(cmd_args),
        "update" => update::run_update(cmd_args),
        "hook" => hook::run_hook(cmd_args),
        "serve" => serve::run_serve(cmd_args),
        "app" => app::run_app(cmd_args),
        "api" => api::run_api(cmd_args),
        "demo" => demo::run_demo(cmd_args),
        // Pipeline steps `gen` runs for you.
        "index" => index::run_index(cmd_args),
        "graph" => index::run_graph(cmd_args),
        "ingest" => ingest::run_ingest(cmd_args),
        // Structural analysis. What is left here is what nothing else
        // can do: betweenness centrality needs all-pairs shortest paths,
        // and cycle detection needs an unbounded DFS — neither is
        // expressible as a query.
        "graph_centrality" => graph_algos::run_graph_centrality(cmd_args),
        "graph_cycles" => graph_algos::run_graph_cycles(cmd_args),
        // Agent tools (graph.json-backed, for AI coding agents). Names match
        // the MCP tools one-for-one.
        "context" => agent::run_context(cmd_args),
        "find_symbols" => agent::run_find_symbols(cmd_args),
        "file_outline" => agent::run_file_outline(cmd_args),
        "get_code" => agent::run_get_code(cmd_args),
        "find_usages" => agent::run_find_usages(cmd_args),
        "project_overview" => agent::run_project_overview(cmd_args),
        "shortest_path" => graph_algos::run_graph_path(cmd_args),
        "graph_schema" => agent::run_graph_schema(cmd_args),
        "analyze" => analyze::run_analyze(cmd_args),
        // Retrieval (OverGraph-backed).
        "semantic_search" => search::run_semantic_search(cmd_args),
        "search" => search::run_hybrid_search(cmd_args),
        "traverse" => search::run_traverse(cmd_args),
        "chat" => chat::run_chat(cmd_args),
        "tour" => tour::run_tour(cmd_args),
        // Project management.
        // `list` is the command; `list_projects` stays because it is the MCP
        // tool's name, and the agent-tool commands are documented as taking
        // the same names as the tools.
        "list" | "ls" | "list_projects" => projects::run_list(cmd_args),
        "active" => projects::run_active(cmd_args),
        "rename" | "rn" | "mv" => projects::run_rename(cmd_args),
        "remove" => projects::run_remove(cmd_args),
        "uninstall" => projects::run_uninstall(cmd_args),
        "upgrade" => upgrade::run_upgrade(cmd_args),
        "config" => config::run_config(cmd_args),
        "doctor" => doctor::run_doctor(cmd_args),
        "connect" => connect::run_connect(cmd_args),
        "disconnect" => connect::run_disconnect(cmd_args),
        // The MCP server itself: a primary entry point, dispatched straight
        // to the module rather than forwarded through `connect`.
        "mcp" => mcp::run(cmd_args),
        "help" | "-h" | "--help" => help::print_help(),
        _ => {
            eprintln!("Unknown command: {}", cmd);
            help::print_help();
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod help_drift_tests {
    //! Every flag a command reads is a flag its own `-h` has to name.
    //!
    //! Per-command help lives beside the command it documents, which is the
    //! right place for it and also the reason it drifts: adding a flag is one
    //! edit and documenting it is another, and nothing fails when only the
    //! first happens. The flag then works and is undiscoverable, which is
    //! indistinguishable from it not existing.
    //!
    //! This reads the flags straight out of each command's own `has_flag` /
    //! `flag_value` calls, so it cannot go stale the way a transcribed list
    //! would.

    use std::collections::BTreeSet;

    fn source(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli").join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
    }

    /// Every `-x` / `--long` this file passes to `has_flag` or `flag_value`.
    fn flags_read(src: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        // Both helpers take flag names as string literals, so the literals
        // inside a `has_flag(` / `flag_value(` call are the flag vocabulary.
        for (open, close) in [("has_flag(", ')'), ("flag_value(", ')')] {
            let mut rest = src;
            while let Some(i) = rest.find(open) {
                rest = &rest[i + open.len()..];
                let Some(end) = rest.find(close) else { break };
                let call = &rest[..end];
                let mut chars = call.char_indices();
                while let Some((qi, c)) = chars.next() {
                    if c != '"' {
                        continue;
                    }
                    let after = &call[qi + 1..];
                    let Some(qe) = after.find('"') else { break };
                    let lit = &after[..qe];
                    if lit.starts_with('-') && lit.len() > 1 {
                        out.insert(lit.to_string());
                    }
                    // Skip past the closing quote.
                    for (ni, _) in chars.by_ref() {
                        if ni > qi + qe {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// Flags that are real but deliberately undocumented, with the reason.
    /// Adding a name here is how you say "not worth a help line"; the test
    /// fails until either that or a help line happens.
    fn excused(flag: &str) -> bool {
        matches!(
            flag,
            // Universally accepted, documented once in `ug help` rather than
            // repeated under every command.
            "-h" | "--help" | "--no-logo"
                // Legacy spellings kept working for installed hooks and old
                // scripts; documenting them would advertise them.
                | "--no-embed" | "--no-expand" | "-o" | "--output"
        )
    }

    #[track_caller]
    fn assert_documented(file: &str, help_marker: &str) {
        let src = source(file);
        let help_start = src
            .find(help_marker)
            .unwrap_or_else(|| panic!("{file}: no `{help_marker}` — has it been renamed?"));
        let help = &src[help_start..];

        let missing: Vec<String> = flags_read(&src)
            .into_iter()
            .filter(|f| !excused(f))
            .filter(|f| !help.contains(f.as_str()))
            .collect();

        assert!(
            missing.is_empty(),
            "{file} reads {missing:?} but its help never mentions them — \
             document them, or excuse them in `excused()` with a reason"
        );
    }

    #[test]
    fn the_tour_command_documents_the_flags_it_reads() {
        assert_documented("tour.rs", "fn print_tour_help");
    }

    #[test]
    fn the_ingest_command_documents_the_flags_it_reads() {
        assert_documented("ingest.rs", "fn print_ingest_help");
    }

    #[test]
    fn the_update_command_documents_the_flags_it_reads() {
        assert_documented("update.rs", "fn print_update_help");
    }

    #[test]
    fn the_hook_command_documents_the_flags_it_reads() {
        assert_documented("hook.rs", "fn hook_help_text");
    }

    #[test]
    fn a_flag_that_is_read_but_undocumented_is_actually_caught() {
        // The guard above only means something if it can fail. This is the
        // shape it is looking for: a flag in the code, absent from the help.
        let src = r#"
            fn run(args: &[String]) {
                if has_flag(args, "--secret-mode") { return; }
            }
            fn print_x_help() {
                println!("  --documented   does a thing");
            }
        "#;
        let flags = flags_read(src);
        assert!(flags.contains("--secret-mode"), "{flags:?}");
        let help = &src[src.find("fn print_x_help").unwrap()..];
        assert!(!help.contains("--secret-mode"), "the drift this test detects");
    }

    #[test]
    fn the_extractor_finds_both_call_shapes() {
        let src = r#"
            has_flag(args, "--alpha");
            flag_value(args, &["-b", "--bravo"]);
        "#;
        let flags = flags_read(src);
        assert!(flags.contains("--alpha"), "{flags:?}");
        assert!(flags.contains("-b") && flags.contains("--bravo"), "{flags:?}");
    }

    #[test]
    fn the_extractor_ignores_strings_that_are_not_flags() {
        // A project name or a path in the same call is not a flag.
        let flags = flags_read(r#"flag_value(args, &["-n", "--name"]); foo("myrepo");"#);
        assert!(!flags.contains("myrepo"), "{flags:?}");
        assert_eq!(flags.len(), 2, "{flags:?}");
    }
}
