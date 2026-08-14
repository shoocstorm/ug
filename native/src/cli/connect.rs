//! `ug connect` / `ug disconnect` / `ug mcp` — wiring ug into an AI
//! coding agent, as a CLI skill, an MCP server, or both.

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

use crate::mcp;

use super::args::has_flag;

/// `ug mcp [...]` — the native MCP server and its install/uninstall/call/list
/// subcommands. Bare `ug mcp` becomes a long-running stdio JSON-RPC server
/// (stdio is the transport, so the startup logo is suppressed for that mode —
/// see `is_mcp_server_mode` in `main`). This replaces the old Node.js `cli.mjs`
/// server: every tool now runs the same Rust code the CLI and HTTP API use.
pub(crate) fn run_mcp(args: &[String]) {
    mcp::run(args);
}

/// `ug connect` — the front door for wiring ug into an AI agent.
///
/// The same code as `ug mcp install`, under the name that describes what it
/// now does: since the choice is CLI skill *or* MCP server, filing it under
/// `mcp` named one of the two answers. That spelling still works.
pub(crate) fn run_connect(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_connect_help();
        return;
    }
    mcp::install::run_mcp_install(args);
}

/// `ug disconnect` — undo `ug connect`, whichever way it wired things.
pub(crate) fn run_disconnect(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_connect_help();
        return;
    }
    mcp::install::run_mcp_uninstall(args);
}

fn print_connect_help() {
    println!("{}", connect_help_text());
}

/// Built as a string rather than printed line by line, so a test can read it:
/// this is the page that has to keep both spellings discoverable.
pub(crate) fn connect_help_text() -> String {
    let mut o = String::new();
    macro_rules! line {
        ($($arg:tt)*) => { o.push_str(&format!("{}\n", format_args!($($arg)*))) };
    }
    line!("  {C_BOLD}{C_GREEN}★ ug connect{C_RESET}  {C_YELLOW}— wire ug into an AI coding agent{C_RESET}");
    line!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    line!("");
    line!("{C_BOLD}Usage:{C_RESET}  ug connect [<agent>] [--cli|--mcp|--both] [--project|--global] [--hooks]");
    line!("        ug disconnect [<agent>]        {C_DIM}remove it again{C_RESET}");
    line!("        {C_DIM}(`ug mcp install` / `ug mcp uninstall` are the same commands){C_RESET}");
    line!("");
    line!("  No agent named? You get an interactive picker. Agents: {C_CYAN}claude{C_RESET},");
    line!("  {C_CYAN}claude-desk{C_RESET}, {C_CYAN}cursor{C_RESET}, {C_CYAN}windsurf{C_RESET}, {C_CYAN}vscode{C_RESET}, {C_CYAN}gemini{C_RESET}, {C_CYAN}codex{C_RESET}, {C_CYAN}hermes{C_RESET}, {C_CYAN}opencode{C_RESET}.");
    line!("");
    line!("{C_BOLD}Two ways to reach ug — connect asks, or pass one:{C_RESET}");
    line!("  {C_CYAN}--cli{C_RESET}      {C_BOLD}Recommended.{C_RESET} Installs the agent skill only; the agent runs");
    line!("             {C_CYAN}ug{C_RESET} itself. {C_CYAN}ug --help{C_RESET} and {C_CYAN}ug query --list{C_RESET} teach it the rest,");
    line!("             so it stays current with the binary and costs no idle context.");
    line!("  {C_CYAN}--mcp{C_RESET}      MCP server entry only — the agent calls tools over the protocol.");
    line!("  {C_CYAN}--both{C_RESET}     Both, and the agent chooses. It usually reaches for the");
    line!("             connected tools, so pick this only if you want that path.");
    line!("");
    line!("  {C_DIM}Whichever you pick, the other is removed — the point of choosing is not");
    line!("  to leave the agent two doors into the same graph.{C_RESET}");
    line!("");
    line!("{C_BOLD}Make it useful while the agent edits, not just while it reads:{C_RESET}");
    line!("  {C_CYAN}--hooks{C_RESET}    Also install the git hooks that re-index after every commit,");
    line!("             merge and rebase. That is what lets an agent trust {C_CYAN}find_usages{C_RESET}");
    line!("             and {C_CYAN}ug query diff_impact{C_RESET} {C_BOLD}about code it just wrote{C_RESET} — the moment");
    line!("             the answer matters most. It can still refresh on demand with");
    line!("             {C_CYAN}ug update <file>...{C_RESET} between commits.");
    line!("             {C_DIM}Same as running {C_RESET}{C_CYAN}ug hook install{C_RESET}{C_DIM}; see {C_RESET}{C_CYAN}ug hook -h{C_RESET}{C_DIM}.{C_RESET}");
    line!("");
    line!("{C_BOLD}Scope:{C_RESET}");
    line!("  {C_CYAN}--project{C_RESET}  this repo only    {C_CYAN}--global{C_RESET}  every project");
    line!("  {C_DIM}Asked when the agent supports both and neither flag is given.{C_RESET}");
    line!("");
    line!("{C_BOLD}Examples:{C_RESET}");
    line!("  {C_CYAN}ug connect{C_RESET}                        {C_DIM}# pick the agent and the way, interactively{C_RESET}");
    line!("  {C_CYAN}ug connect claude --cli --global{C_RESET}  {C_DIM}# the CLI skill, everywhere{C_RESET}");
    line!("  {C_CYAN}ug connect cursor --mcp --project{C_RESET} {C_DIM}# MCP server, this repo only{C_RESET}");
    line!("  {C_CYAN}ug disconnect claude{C_RESET}             {C_DIM}# remove skill and server entry{C_RESET}");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ug connect` is the promoted spelling of `ug mcp install`, not a second
    /// implementation — so its help has to teach the modes *and* keep the old
    /// spelling discoverable for anyone whose muscle memory or scripts have it.
    #[test]
    fn connect_help_teaches_the_modes_and_keeps_the_old_spelling() {
        let help = connect_help_text();
        for expected in [
            "--cli", "--mcp", "--both", "--hooks",
            "Recommended",
            "ug disconnect",
            "ug mcp install",
            "--project", "--global",
        ] {
            assert!(help.contains(expected), "`ug connect -h` is missing {expected}");
        }
    }
}
