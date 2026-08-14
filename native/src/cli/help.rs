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

/// Width of the command column. Sized to the longest name printed
/// (`project_overview`, `graph_centrality` — 16) plus a two-space gutter.
const CMD_W: usize = 18;

/// A command row: `  name              description`.
///
/// The name is padded *before* it is coloured. ANSI escapes are characters as
/// far as `{:<n}` is concerned, so colouring first pushes every row that
/// carries colour out of the column — which is how the old `update` line ended
/// up one space right of its neighbours.
fn cmd(name: &str, desc: &str) {
    println!("  {C_CYAN}{:<CMD_W$}{C_RESET}{}", name, desc);
}

/// A command row for the three commands worth reaching for first. Only the
/// name is emphasised — colouring the description too turned three rows into
/// three blocks of magenta and made the section harder to scan, not easier.
fn cmd_hi(name: &str, desc: &str) {
    println!("  {C_BOLD}{C_MAGENTA}{:<CMD_W$}{C_RESET}{}", name, desc);
}

/// A continuation line under a command row — indented into the description
/// column so it reads as part of the entry above, not as another command.
fn cont(text: &str) {
    println!("  {:<CMD_W$}{C_DIM}{}{C_RESET}", "", text);
}

/// A row in the closing `Conventions` block: same column as a command, but
/// the label is bold rather than cyan, so it never reads as a command name.
fn note(label: &str, text: &str) {
    println!("  {C_BOLD}{:<CMD_W$}{C_RESET}{}", label, text);
}

/// A section header, with an optional dim qualifier that applies to every
/// command under it. The qualifier is the group's *precondition* — stating it
/// once beats repeating "(needs the db)" on six rows.
fn group(title: &str, qualifier: &str) {
    println!();
    if qualifier.is_empty() {
        println!("{C_BOLD}{}{C_RESET}", title);
    } else {
        println!("{C_BOLD}{}{C_RESET}  {C_DIM}{}{C_RESET}", title, qualifier);
    }
}

pub(crate) fn print_help() {
    println!();
    println!(
        "Usage: {C_BOLD}ug <command>{C_RESET} [options]  {C_DIM}·{C_RESET}  {C_CYAN}ug <command> -h{C_RESET} {C_DIM}flags + examples{C_RESET}  {C_DIM}·{C_RESET}  {C_CYAN}ug -v{C_RESET} {C_DIM}version{C_RESET}"
    );

    group("Start here", "");
    cmd_hi("gen", "Index this directory into a graph at ~/.ug/<name>/");
    cont("run it again any time to refresh the project from its recorded root");
    cmd("connect", "Wire ug into an AI agent — Claude Code, Cursor, Codex, Gemini, …");
    cont("--cli the CLI skill (recommended) · --mcp the MCP server · --both");
    cmd("serve", "Web UI + REST API on localhost:8080 — also what bare `ug` starts");
    cmd("app", "The same, in a native desktop window");

    // `index` / `graph` / `ingest` are the stages `gen` runs and are still
    // dispatched, but they are not listed: they are internal seams, and
    // `gen --no-ingest` already covers the one reason an end user reached
    // for them. `ug api` and the docs still name them.
    group("Read the code", "from graph.json — no database needed");
    cmd("find_symbols", "Find symbols by name or wildcard — start here; gives the ids below");
    cmd("get_code", "Read a symbol's source, or a file and line range");
    cmd("file_outline", "Every indexed symbol in a file, in line order");
    cmd("find_usages", "Who calls or imports this? Callers, importers, call sites");
    cmd("shortest_path", "How two symbols are connected (directed edge path)");
    cmd("project_overview", "Orient: stats, biggest files, most depended-upon symbols");
    cmd("graph_schema", "Node & edge types in this graph — what --edge-type takes");
    cmd("graph_centrality", "Rank nodes by degree or betweenness");
    cmd("graph_cycles", "Detect dependency cycles (--fail-on-cycle for CI)");

    group("Search & analyse", "from the database `ug gen` builds");
    cmd_hi("query", "Statistics, distributions and blast radius, as read-only GQL");
    cont("39 named questions (ug query --list) or write your own");
    cmd_hi("search", "GraphRAG: semantic search → graph expansion → ranked context");
    cmd("semantic_search", "Search by meaning alone; find_symbols for exact names");
    cmd("traverse", "K-hop walk over the stored edges from a symbol");
    cmd("chat", "GraphRAG-grounded chat — one-shot, or an interactive REPL");
    cmd("tour", "Guided, narrated walkthrough; flies the camera in the web UI");

    group("Keep the graph current", "");
    cmd("update", "Refresh only the files you just changed — focused and incremental");
    cmd("hook", "Install git hooks: `update` runs on commit, merge, checkout, rebase");

    group("Manage projects", "");
    cmd("list", "Projects under ~/.ug: nodes, size on disk, how stale each one is");
    cmd("active", "The project commands default to when run outside an indexed repo");
    cmd("rename · rm", "Rename a project (aliases: rn, mv) · delete its data directory");
    cmd("doctor · config", "Show how every setting resolved · view and persist defaults");
    cmd("upgrade", "Check GitHub for a new release and self-update; --check only reports");
    cmd("uninstall", "Delete ALL indexed projects and remove ug itself");
    cmd("api", "List every HTTP endpoint `ug serve` exposes");
    cmd("mcp", "Run the MCP stdio server; `mcp call <tool> <json>` runs one by hand");
    cmd("disconnect", "Undo `connect` for an agent");

    // No blanket "these apply to every command" — `--json` and `-o` do not,
    // and an agent that believes a claim this page makes and then hits a
    // command without the flag has been told something false. Each row states
    // only what is true everywhere, and points at `-h` for the rest.
    group("Conventions", "");
    note("Projects", "-n <name> picks one; else the active project, else this directory.");
    cont("Each command prints which project it resolved to, and why, first.");
    note("Symbols", "A symbol argument takes an id, a plain name, or a quoted wildcard:");
    println!(
        "  {:<CMD_W$}{C_CYAN}'handle_*'{C_RESET}  {C_CYAN}'src/**/*.ts'{C_RESET}  {C_DIM}* ? [a-z] {{a,b}}; ** crosses dirs{C_RESET}",
        ""
    );
    note("Output", "--json for machine-readable output · -o <file> to write it to");
    cont("a file. Per command — check -h. --no-color · --no-logo · --no-banner");
    cont("strip decoration; all three are already off when output is piped.");
    println!();
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
