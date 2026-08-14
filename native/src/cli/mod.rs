//! The `ug` command-line interface.
//!
//! One module per command group, each owning its `run_*` entry point *and*
//! the `print_*_help` that documents it — help text is the largest part of
//! this CLI and keeping it beside the flag parsing it describes is the only
//! way the two stay in step. Shared machinery lives in [`args`] (flag and
//! positional parsing), [`io`] (writing results, exiting with a message),
//! [`embed`] (embedder + runtime construction) and [`store`] (resolving
//! `--dest` into stores).
//!
//! [`run`] is the whole entry point: everything below `fn main` starts here.

pub(crate) mod agent;
pub(crate) mod analysis;
pub(crate) mod api;
pub(crate) mod app;
pub(crate) mod args;
pub(crate) mod chat_cmd;
pub(crate) mod config_cmd;
pub(crate) mod connect;
pub(crate) mod doctor;
pub(crate) mod embed;
pub(crate) mod gen;
pub(crate) mod help;
pub(crate) mod hook;
pub(crate) mod index_cmd;
pub(crate) mod ingest;
pub(crate) mod io;
pub(crate) mod projects;
pub(crate) mod query;
pub(crate) mod search;
pub(crate) mod store;
pub(crate) mod tour_cmd;
pub(crate) mod update;
pub(crate) mod upgrade;

use std::env;

use ultragraph::{C_BOLD, C_CYAN, C_RESET};

use crate::serve;

/// Parse the process arguments and run the requested command.
///
/// Everything that has to happen before any subcommand sees an argument —
/// the colour gate, `.env` loading, the global flags that no subcommand's
/// parser should ever see as a positional — happens here, once.
pub(crate) fn run() {
    io::install_panic_hook();

    // Colour gate, resolved once before any command runs. `Render::Ansi`
    // output (the agent-tool commands and `ug query`) consults this so a
    // non-tty consumer — a pipe, an LLM, a log — gets plain text without
    // every format string branching. `--no-color` and the `NO_COLOR` env
    // var (https://no-color.org) force it off; otherwise it follows the
    // terminal. Human-facing banners keep their colour in a terminal
    // regardless.
    let raw_args: Vec<String> = env::args().collect();
    let color_off = args::has_flag(&raw_args, "--no-color")
        || env::var_os("NO_COLOR").is_some()
        || !std::io::IsTerminal::is_terminal(&std::io::stdout());
    ultragraph::color::set(!color_off);

    // Load environment defaults from `.env` (in CWD or any parent
    // directory). Real env vars still win — `dotenvy::dotenv` does not
    // override values already set in the process environment. Quiet
    // when no `.env` is present.
    let _ = dotenvy::dotenv();

    // `--no-logo` is consumed here rather than passed through, so no
    // subcommand's argument parser can mistake it for a positional.
    let mut argv: Vec<String> = raw_args;
    let logo_flagged_off = argv.iter().any(|a| a == "--no-logo" || a == "--quiet-logo");
    argv.retain(|a| a != "--no-logo" && a != "--quiet-logo" && a != "--no-color");
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
        "regen" => gen::run_regen(cmd_args),
        "update" => update::run_update(cmd_args),
        "hook" => hook::run_hook(cmd_args),
        "serve" => serve::run_serve(cmd_args),
        "app" => app::run_app(cmd_args),
        "api" => api::run_api(cmd_args),
        // Pipeline steps `gen` runs for you.
        "index" => index_cmd::run_index(cmd_args),
        "graph" => index_cmd::run_graph(cmd_args),
        "ingest" => ingest::run_ingest(cmd_args),
        // Structural analysis. What is left here is what nothing else
        // can do: betweenness centrality needs all-pairs shortest paths,
        // and cycle detection needs an unbounded DFS — neither is
        // expressible as a query.
        "graph_centrality" => analysis::run_graph_centrality(cmd_args),
        "graph_cycles" => analysis::run_graph_cycles(cmd_args),
        // Agent tools (graph.json-backed, for AI coding agents). Names match
        // the MCP tools one-for-one.
        "find_symbols" => agent::run_find_symbols(cmd_args),
        "file_outline" => agent::run_file_outline(cmd_args),
        "get_code" => agent::run_get_code(cmd_args),
        "find_usages" => agent::run_find_usages(cmd_args),
        "project_overview" => agent::run_project_overview(cmd_args),
        "shortest_path" => analysis::run_graph_path(cmd_args),
        "graph_schema" => agent::run_graph_schema(cmd_args),
        "query" | "code_query" => query::run_code_query(cmd_args),
        // Retrieval (OverGraph-backed).
        "semantic_search" => search::run_semantic_search(cmd_args),
        "search" => search::run_hybrid_search(cmd_args),
        "traverse" => search::run_traverse(cmd_args),
        "chat" => chat_cmd::run_chat(cmd_args),
        "tour" => tour_cmd::run_tour(cmd_args),
        // Project management.
        // `list` is the command; `list_projects` stays because it is the MCP
        // tool's name, and the agent-tool commands are documented as taking
        // the same names as the tools.
        "list" | "ls" | "list_projects" => projects::run_list(cmd_args),
        "active" => projects::run_active(cmd_args),
        "rename" | "rn" | "mv" => projects::run_rename(cmd_args),
        "rm" => projects::run_rm(cmd_args),
        "uninstall" => projects::run_uninstall(cmd_args),
        "upgrade" => upgrade::run_upgrade(cmd_args),
        "config" => config_cmd::run_config(cmd_args),
        "doctor" => doctor::run_doctor(cmd_args),
        "connect" => connect::run_connect(cmd_args),
        "disconnect" => connect::run_disconnect(cmd_args),
        "mcp" => connect::run_mcp(cmd_args),
        "help" | "-h" | "--help" => help::print_help(),
        _ => {
            eprintln!("Unknown command: {}", cmd);
            help::print_help();
            std::process::exit(1);
        }
    }
}
