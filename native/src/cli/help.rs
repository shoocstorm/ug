//! The top-level `ug help` output and the startup logo.
//!
//! Per-command help lives beside the command it documents; what is here
//! is the index that points at those.

use ultragraph::{C_BLUE, C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW};

/// Whether to skip the banner for this invocation.
///
/// The logo is decoration printed to **stdout**, which makes it worse than
/// noise for anything that reads output: it sat in front of the JSON from
/// `ug <tool> --json`, so piping to `jq` failed outright. Four ways it goes
/// away, in the order they are checked:
///
/// 1. **`--no-logo`** — explicit, works everywhere.
/// 2. **`UG_QUIET_LOGO`** — the pre-existing env contract, kept as-is.
/// 3. **stdout is not a terminal** — the one that matters in practice.
///    Pipes, redirects, CI and coding agents all land here and get clean
///    output with no flag to remember. A human at a terminal still sees
///    the banner, which is the only place it was ever doing any work.
/// 4. **stdio server modes** — bare `ug mcp` speaks JSON-RPC on stdout, so
///    a banner would corrupt the protocol stream outright; and `ug serve`'s
///    KB Manager wizard spawns `ug` as a subprocess whose output it streams
///    into a log viewer the banner would dominate.
pub(crate) fn suppress_logo(args: &[String], flagged_off: bool) -> bool {
    if flagged_off || std::env::var("UG_QUIET_LOGO").is_ok() {
        return true;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return true;
    }
    // Bare `ug mcp` (no install/uninstall subcommand) is the stdio server.
    args.get(1).map(String::as_str) == Some("mcp")
        && !matches!(
            args.get(2).map(String::as_str),
            Some("install") | Some("uninstall")
        )
}

pub(crate) fn print_logo() {
    println!();
    println!(
        "   {C_YELLOW}✦{C_RESET} {C_DIM}──────────────────────────────────────────{C_RESET} {C_YELLOW}✦{C_RESET}"
    );
    println!();
    println!(
        "     {C_BOLD}{C_CYAN}●{C_RESET}{C_DIM}───{C_RESET}{C_BOLD}{C_MAGENTA}●{C_RESET}    {C_BOLD}U L T R A  G R A P H{C_RESET}"
    );
    println!("     {C_DIM}│   │{C_RESET}    {C_DIM}·  code intelligence  ·{C_RESET}");
    println!(
        "     {C_BOLD}{C_BLUE}●{C_RESET}{C_DIM}───{C_RESET}{C_BOLD}{C_GREEN}●{C_RESET}"
    );
    println!();
    println!("     {C_DIM}the knowledge graph for your codebase & docs{C_RESET}");
    println!();
    println!(
        "   {C_YELLOW}✦{C_RESET} {C_DIM}──────────────────────────────────────────{C_RESET} {C_YELLOW}✦{C_RESET}"
    );
    println!();
}

pub(crate) fn print_help() {
    println!();
    println!("Usage: {C_BOLD}ug <command>{C_RESET} [options]");
    println!();
    println!("{C_BOLD}Quick start:{C_RESET}");
    println!("  {C_CYAN}ug gen{C_RESET}     Index this directory, build the graph, and ingest it (→ ~/.ug/<name>/)");
    println!("  {C_CYAN}ug app{C_RESET}     Explore the graph in a native desktop window (starts the server for you)");
    println!("  {C_CYAN}ug{C_RESET}         Bare `ug` starts the server (visualization + REST API at http://localhost:8080)");
    println!("{C_BOLD}Connect an AI agent (Claude Code / Claude Desktop / Cursor / Windsurf / VS Code / Gemini CLI / Codex CLI / Hermes Agent / opencode):{C_RESET}");
    println!("  {C_CYAN}ug connect{C_RESET}                Wire ug into an agent (interactive picker; or name one, e.g. `ug connect claude`)");
    println!("  {C_DIM}                          Asks how: {C_RESET}{C_CYAN}--cli{C_RESET}{C_DIM} teaches the agent this CLI (recommended), {C_RESET}{C_CYAN}--mcp{C_RESET}{C_DIM} wires the MCP server, {C_RESET}{C_CYAN}--both{C_RESET}{C_DIM} does both.{C_RESET}");

    println!();
    println!("{C_BOLD}Commands:{C_RESET}");
    println!(
        "  {C_BOLD}{C_MAGENTA}gen{C_RESET}              {C_BOLD}{C_MAGENTA}⚡ full pipeline: index → graph → visualization → ingest ⚡{C_RESET}"
    );
    println!("                   {C_DIM}re-run an existing project from its recorded root (incremental){C_RESET}");
    println!("  {C_CYAN}update{C_RESET}            Refresh the graph for the files that just changed (focused re-run)");
    println!("  {C_CYAN}hook{C_RESET}             Install git hooks that run `update` for you — the graph self-heals");
    println!("  {C_CYAN}serve{C_RESET}            Serve the visualization + graph API");
    println!("  {C_CYAN}app{C_RESET}              Open the native desktop shell (starts serve + a window)");
    println!("  {C_CYAN}api{C_RESET}              List every HTTP endpoint `ug serve` exposes");
    println!("  {C_CYAN}connect{C_RESET}          Wire ug into an AI agent — CLI skill and/or MCP server");
    println!("                   {C_DIM}(also spelled `ug mcp install`; undo with `ug disconnect`){C_RESET}");
    println!();
    println!("  {C_DIM}Retrieval & analysis (OverGraph-backed){C_RESET}");
    println!(
        "  {C_BOLD}{C_YELLOW}search{C_RESET}           {C_YELLOW}GraphRAG: semantic search → graph expansion → ranked context{C_RESET}"
    );
    println!(
        "  {C_BOLD}{C_MAGENTA}query{C_RESET}            {C_BOLD}{C_MAGENTA}📊 whole-repo statistics: counts, distributions, blast radius{C_RESET}"
    );
    println!("                   {C_DIM}39 named questions ({C_RESET}{C_CYAN}ug query --list{C_RESET}{C_DIM}) or write your own GQL{C_RESET}");
    println!("  {C_CYAN}semantic_search{C_RESET}  Search by meaning/concept (embeddings; use find_symbols for exact names)");
    println!("  {C_CYAN}traverse{C_RESET}         K-hop BFS over the OverGraph edges table");
    println!(
        "  {C_BOLD}{C_MAGENTA}chat{C_RESET}             {C_BOLD}{C_MAGENTA}💬 GraphRAG-grounded chat (one-shot or REPL){C_RESET}"
    );
    println!(
        "  {C_BOLD}{C_MAGENTA}tour{C_RESET}             {C_BOLD}{C_MAGENTA}🎬 guided, narrated walkthrough — flies the camera in the web UI{C_RESET}"
    );
    println!();
    // `index` / `graph` / `ingest` are the stages `gen` runs and are still
    // dispatched, but they are not listed: they are internal seams, and
    // `gen --no-ingest` already covers the one reason an end user reached
    // for them. `ug api` and the docs still name them.
    println!("  {C_DIM}Structural analysis (graph.json only — no database needed){C_RESET}");
    println!("  {C_CYAN}graph_centrality{C_RESET} Rank nodes by degree/betweenness (--top, -t, -f)");
    println!("                   {C_DIM}degree ranking is also {C_RESET}{C_CYAN}ug query dependency_fanin{C_RESET}{C_DIM}; betweenness is only here{C_RESET}");
    println!("  {C_CYAN}graph_cycles{C_RESET}     Detect dependency cycles (--min-len, --fail-on-cycle for CI)");
    println!();
    println!("  {C_DIM}Agent tools — what AI coding agents use (via MCP) to understand a repo; run by hand to explore or verify{C_RESET}");
    println!("  {C_CYAN}project_overview{C_RESET} Orient in the codebase: stats, biggest files, most depended-upon symbols");
    println!("  {C_CYAN}find_symbols{C_RESET}     Name lookup, no embeddings — returns the ids the tools below take");
    println!("  {C_CYAN}file_outline{C_RESET}     List every indexed symbol in a file, in line order");
    println!("  {C_CYAN}get_code{C_RESET}         Read the source for a symbol, or a file/line range");
    println!("  {C_CYAN}find_usages{C_RESET}      Who uses this symbol? (inbound callers/importers + call sites)");
    println!("  {C_CYAN}shortest_path{C_RESET}    How two symbols are connected (directed edge path)");
    println!("  {C_CYAN}graph_schema{C_RESET}     Node & edge types in this graph — what to pass to --edge-type filters");
    println!("  {C_DIM}  All accept {C_RESET}{C_CYAN}--json{C_RESET}{C_DIM} and take the same names/params as the MCP tools.{C_RESET}");
    println!();
    println!("  {C_BOLD}Wildcards{C_RESET} work anywhere a symbol or file is named — quote them:");
    println!("    {C_CYAN}ug find_symbols{C_RESET} 'handle_*'          {C_DIM}every handler{C_RESET}");
    println!("    {C_CYAN}ug find_usages{C_RESET}  'validate_*'        {C_DIM}blast radius of a family{C_RESET}");
    println!("    {C_CYAN}ug file_outline{C_RESET} 'src/**/*.ts'       {C_DIM}outline a whole subtree{C_RESET}");
    println!("  {C_DIM}  * ? [abc] [a-z] [!ab] {{a,b}} — whole-name match; ** crosses directories.{C_RESET}");
    println!("  {C_DIM}  These tools also take a plain symbol name, not just an id.{C_RESET}");
    println!();

    println!("  {C_DIM}Project management{C_RESET}");
    println!("  {C_BOLD}{C_GREEN}list{C_RESET}             {C_GREEN}List generated projects under ~/.ug (or $UG_HOME){C_RESET}");
    println!("  {C_CYAN}active{C_RESET}           View/set the active project (default for `ug mcp` when no UG_PROJECT)");
    println!("  {C_CYAN}rename{C_RESET}           Rename a project (aliases: rn, mv) — defaults to the active one");
    println!("  {C_CYAN}rm{C_RESET}               Delete a project's data directory");
    println!("  {C_CYAN}upgrade{C_RESET}          Check GitHub for a new release and self-update (`--check` to only report)");
    println!("  {C_CYAN}uninstall{C_RESET}        Delete ALL indexed projects and uninstall ug itself");
    println!("  {C_CYAN}config{C_RESET}           View/persist defaults (chat model, endpoints, …) in ~/.ug/config.json");
    println!("  {C_CYAN}doctor{C_RESET}           Show resolved project/db/embedder/chat config");
    println!();
    println!("{C_BOLD}Global flags:{C_RESET}");
    println!("  {C_CYAN}--no-logo{C_RESET}        Skip the banner. Already skipped automatically whenever stdout");
    println!("                   is not a terminal, so piped and captured output is clean.");
    println!("  {C_CYAN}--no-color{C_RESET}       Force colour off (agent-tool + query output). Also off automatically");
    println!("                   when piped or when the NO_COLOR env var is set; this flag makes it explicit.");
    println!("  {C_CYAN}--no-banner{C_RESET}      Skip the `▸ project <name> · <repo> · [<how it resolved>]` line every");
    println!("                   project-scoped command prints. It goes to stderr, so it never touches");
    println!("                   piped output; UG_NO_BANNER=1 does the same.");
    println!("  {C_CYAN}-v, --version{C_RESET}    Print the version");
    println!();
    println!("Run {C_CYAN}ug <command> -h{C_RESET} for that command's options and examples.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// The banner goes to stdout, so anything that *reads* output has to
    /// get it suppressed. The non-terminal case is the one that matters —
    /// it is why `ug <tool> --json | jq` works without a flag — but it
    /// cannot be asserted here, because the test harness's stdout is
    /// already not a terminal and would mask every other condition.
    #[test]
    fn the_logo_is_suppressed_by_the_flag_and_by_env() {
        assert!(suppress_logo(&argv("ug graph_schema"), true), "--no-logo");

        // Whatever the harness's stdout is, an explicit opt-out wins.
        std::env::set_var("UG_QUIET_LOGO", "1");
        assert!(suppress_logo(&argv("ug gen"), false), "UG_QUIET_LOGO");
        std::env::remove_var("UG_QUIET_LOGO");
    }

    /// Bare `ug mcp` speaks JSON-RPC on stdout — a banner there is not
    /// noise, it corrupts the protocol. `ug mcp install` is an ordinary
    /// interactive command and keeps it.
    #[test]
    fn the_stdio_server_mode_never_prints_a_banner() {
        assert!(suppress_logo(&argv("ug mcp"), false));
        assert!(suppress_logo(&argv("ug mcp call find_symbols"), false));
        // These are interactive; only a non-terminal stdout should silence
        // them, which is the condition this test cannot control.
        for line in [
            "ug mcp install claude",
            "ug mcp uninstall cursor",
            "ug connect claude",
            "ug disconnect cursor",
        ] {
            let args = argv(line);
            assert_eq!(
                suppress_logo(&args, false),
                !std::io::IsTerminal::is_terminal(&std::io::stdout()),
                "`{line}` should only be silenced by a non-terminal stdout"
            );
        }
    }
}
