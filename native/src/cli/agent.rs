//! The agent tools, callable by hand.
//!
//! The MCP server (`ug mcp`, see `src/mcp/`) exposes `graph.json`-backed
//! tools that AI coding agents call to understand an indexed repo. The
//! commands here are those same tools — same lookup logic over the same
//! `graph.json`, no embeddings — so a human can explore the repo the way
//! an agent does, or verify what an agent will see. Command names match
//! the tool names one-for-one.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ultragraph::agent_tools::{self, looks_like_node_id, Render};
use ultragraph::storage::{self, StoreSpec};
use ultragraph::types::GraphData;
use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET, C_YELLOW};

use crate::project;

use super::args::{
    AGENT_VALUE_FLAGS, analysis_input, emit_raw, flag_value, has_flag, multi_flag, positionals,
    split_ids_and_names,
};
use super::embed::tokio_runtime;
use super::scope;

/// graph.json for the agent-tool commands: `-i/--input` wins, else the
/// `-n/--name`, active, or cwd-derived project dir, else the most recently
/// updated project under ~/.ug — same fallback spirit as the db reads.
///
/// Returns the rule that fired alongside the path, for the scope banner:
/// falling through to "most recently updated project" is how a question about
/// this repo gets answered from another one, and it should never be silent.
fn agent_graph_path(args: &[String]) -> (PathBuf, &'static str) {
    if let Some(p) = flag_value(args, &["-i", "--input"]) {
        return (PathBuf::from(p), "-i/--input");
    }
    let p =
        project::project_dir(&project::resolve_active_project_name(args, ".")).join("graph.json");
    if p.exists() || flag_value(args, &["-n", "--name"]).is_some() {
        return (p, scope::why_project(args, true));
    }
    for (dir, _meta) in project::list_projects() {
        let candidate = dir.join("graph.json");
        if candidate.exists() {
            return (candidate, "most recently updated project");
        }
    }
    (p, scope::why_project(args, true))
}

pub(crate) fn load_agent_graph(args: &[String]) -> (GraphData, String, PathBuf) {
    let (path, why) = agent_graph_path(args);
    // Before the read, so a "graph.json not found" failure below still says
    // which project it was looking for and why it looked there.
    scope::announce_data("graph", &path, why);
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => {
            eprintln!(
                "graph.json not found at {} — run {C_CYAN}ug gen{C_RESET} for this project first.",
                path.display()
            );
            std::process::exit(1);
        }
    };
    match serde_json::from_str::<GraphData>(&raw) {
        Ok(graph) => (graph, raw, path),
        Err(e) => {
            eprintln!("Failed to parse {}: {}", path.display(), e);
            std::process::exit(1);
        }
    }
}

/// Repo root for reading source files: $UG_REPO_ROOT > project.json's
/// repoRoot (sibling of graph.json) > graph stats.repoRoot > cwd.
pub(crate) fn agent_repo_root(graph: &GraphData, graph_path: &Path) -> PathBuf {
    if let Ok(r) = std::env::var("UG_REPO_ROOT") {
        if !r.trim().is_empty() {
            return PathBuf::from(r);
        }
    }
    if let Some(dir) = graph_path.parent() {
        if let Some(meta) = project::read_meta(dir) {
            if !meta.repo_root.is_empty() {
                return PathBuf::from(meta.repo_root);
            }
        }
    }
    if let Some(stats) = &graph.stats {
        if !stats.repo_root.is_empty() {
            return PathBuf::from(&stats.repo_root);
        }
    }
    PathBuf::from(".")
}

/// Load the source the index captured for `ids`, from the store that sits
/// beside this project's graph.json.
///
/// The reason the agent-tool commands work with the repo gone: `ug gen`
/// wrote every node's span into `~/.ug/<project>/ugdb`, so the source is
/// project data, not something to go find on disk. Pinned to the graph's
/// own sibling `ugdb` rather than resolved from `--dest`/`-o` — the code
/// must come from the same project as the graph, and a graph loaded via
/// `-i` from anywhere else simply has no store to read.
///
/// Silent on every failure: a project that was never ingested, a store from
/// an older ug, a `--dest neo4j` run. Each yields an empty result and the
/// tools fall back to the working tree, which is what they did before the
/// store captured anything.
fn indexed_source(graph_path: &Path, ids: &[String]) -> agent_tools::IndexedSource {
    if ids.is_empty() {
        return agent_tools::IndexedSource::default();
    }
    let Some(db_path) = graph_path.parent().map(|d| d.join("ugdb")) else {
        return agent_tools::IndexedSource::default();
    };
    if !db_path.exists() {
        return agent_tools::IndexedSource::default();
    }
    let spec = StoreSpec::Overgraph {
        embedding_dim: storage::db::stored_embedding_dim(&db_path)
            .unwrap_or(storage::embed::DEFAULT_EMBEDDING_DIM as u32),
        path: db_path,
    };
    tokio_runtime().block_on(async {
        match storage::open_store(&spec).await {
            Ok(store) => agent_tools::IndexedSource::load(store.as_ref(), ids).await,
            Err(_) => agent_tools::IndexedSource::default(),
        }
    })
}

/// The wildcard dialect, printed by every command that accepts one.
///
/// One block in one place because the matcher is one implementation
/// (`ultragraph::pattern`): someone who learns `*` on `find_symbols` will try
/// it on `file_outline` and `find_usages`, and it has to be the same there.
/// The quoting note leads because an unquoted `*` is expanded by the shell
/// before `ug` ever sees it — the first thing that bites a new user.
pub(crate) fn print_wildcard_help() {
    println!("{C_BOLD}Wildcards:{C_RESET}  {C_YELLOW}quote them — an unquoted * is expanded by your shell{C_RESET}");
    println!("  {C_CYAN}*{C_RESET}      any run of characters        {C_CYAN}?{C_RESET}      exactly one character");
    println!("  {C_CYAN}[abc]{C_RESET}  one of these characters      {C_CYAN}[a-z]{C_RESET}  one from the range");
    println!("  {C_CYAN}[!ab]{C_RESET}  any character except these   {C_CYAN}{{a,b}}{C_RESET} either alternative");
    println!("  {C_CYAN}\\*{C_RESET}     a literal asterisk");
    println!("  A pattern must match the {C_BOLD}whole{C_RESET} name: {C_CYAN}auth*{C_RESET} finds authorize, {C_CYAN}*auth*{C_RESET} finds reauth.");
    println!("  In paths, {C_CYAN}*{C_RESET} stops at {C_CYAN}/{C_RESET} and {C_CYAN}**/{C_RESET} crosses directories: {C_CYAN}src/**/*.ts{C_RESET}.");
}

/// The three shapes every id-taking command accepts, printed where they
/// apply. Agents were sending an id-only lookup, failing, and round-tripping
/// through `find_symbols`; humans just knew the function's name.
pub(crate) fn print_node_ref_help() {
    println!("{C_BOLD}Accepts, for each argument:{C_RESET}");
    println!("  a node {C_CYAN}id{C_RESET} from any tool  ·  an exact symbol {C_CYAN}name{C_RESET}  ·  a {C_CYAN}wildcard{C_RESET} pattern");
    println!("  A name or pattern expands to every symbol it matches (up to {} of them).", agent_tools::MAX_REF_EXPANSION);
}

fn print_find_symbols_help() {
    println!("  {C_CYAN}ug find_symbols{C_RESET}  {C_YELLOW}— symbol lookup by name or wildcard (no embeddings){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug find_symbols <name-or-pattern-or-id>... [options]");
    println!();
    println!("  Takes several arguments in one call (sections are separated) — batch related");
    println!("  lookups instead of running the command repeatedly.");
    println!("  {C_CYAN}Direct nodeId lookup{C_RESET} (O(1)): an argument containing ':' is treated as a nodeId.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--node-type <type>{C_RESET}   Restrict to node type (repeatable, wildcards ok; e.g. Function,");
    println!("                       Class, Interface, Variable, File — {C_CYAN}ug graph_schema{C_RESET} lists them)");
    println!("  {C_CYAN}--file-prefix <p>{C_RESET}    Only symbols under this path: a prefix ({C_CYAN}src/auth/{C_RESET}) or a");
    println!("                       glob ({C_CYAN}src/**/*.ts{C_RESET})");
    println!("  {C_CYAN}-k, --limit <n>{C_RESET}      Max hits per query (default 20)");
    println!("  {C_CYAN}--boundary{C_RESET}           Only system boundaries — REST handlers, queue listeners,");
    println!("                       CLI commands, outbound HTTP/DB clients. Works with no");
    println!("                       name at all, which lists the whole public surface.");
    println!("  {C_CYAN}--include-docs{C_RESET}       Also match docstrings, not just names");
    println!("  {C_CYAN}-n, --name <project>{C_RESET} Project name (default: cwd basename)");
    println!("  {C_CYAN}--json{C_RESET}               Machine-readable output");
    println!("  {C_DIM}(-t/--type and -f/--file still parse as the old spellings){C_RESET}");
    println!();
    print_wildcard_help();
    println!();
    println!("{C_BOLD}Ranking:{C_RESET}");
    println!("  Plain text:  exact > prefix > substring > docstring; ties go to the shorter name.");
    println!("  A pattern:   every match is equal (you said what the name looks like), listed A-Z.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} resolveDb                    {C_YELLOW}# a name you know{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} run_serve run_app run_gen    {C_YELLOW}# batch: three lookups, one call{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}'handle_*'{C_RESET}                  {C_YELLOW}# every handler{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}'*Controller'{C_RESET} --node-type Class");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}'test_?'{C_RESET}                    {C_YELLOW}# test_1 … test_9, not test_10{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}'{{get,set}}_*'{C_RESET}               {C_YELLOW}# accessors of both kinds{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}'*'{C_RESET} --file-prefix {C_BOLD}'src/auth/**'{C_RESET} -k 100");
    println!("                                            {C_YELLOW}# everything in one subtree{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} 'function:src/auth.rs:42:login'  {C_YELLOW}# direct nodeId lookup{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} cache --include-docs         {C_YELLOW}# also scan docstrings{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}--boundary{C_RESET}                  {C_YELLOW}# every entry and exit point{C_RESET}");
    println!("  {C_CYAN}ug find_symbols{C_RESET} {C_BOLD}--boundary{C_RESET} --file-prefix {C_BOLD}'src/api/**'{C_RESET}");
    println!();
    println!("{C_BOLD}Next:{C_RESET} feed an id from the output into {C_CYAN}ug get_code{C_RESET}, {C_CYAN}ug find_usages{C_RESET} or {C_CYAN}ug traverse{C_RESET}");
    println!("      — those take the same names and patterns directly, too.");
}

fn print_file_outline_help() {
    println!("  {C_CYAN}ug file_outline{C_RESET}  {C_YELLOW}— list every indexed symbol in one file{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug file_outline <file-or-glob-or-id>... [options]");
    println!();
    println!("{C_BOLD}Accepts, for each argument:{C_RESET}");
    println!("  a repo-relative {C_CYAN}path{C_RESET} ({C_CYAN}native/src/main.rs{C_RESET})  ·  a unique {C_CYAN}suffix{C_RESET} ({C_CYAN}main.rs{C_RESET})");
    println!("  a File node {C_CYAN}id{C_RESET} ({C_CYAN}file:native/src/main.rs{C_RESET})  ·  a path {C_CYAN}glob{C_RESET} ({C_CYAN}src/**/*.ts{C_RESET})");
    println!("  Batch several in one call rather than running the command repeatedly.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-k, --max-files <n>{C_RESET}   Files a single glob may outline (default 20). Over the cap,");
    println!("                        the extra paths are listed by name instead of expanded.");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!("  {C_CYAN}--ids{C_RESET}                Show the full node id on each line (on by default in a");
    println!("                        terminal; off when piped, since kind:file:name reconstructs it)");
    println!("  {C_CYAN}--json{C_RESET}               Machine-readable output");
    println!();
    print_wildcard_help();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} native/src/main.rs");
    println!("  {C_CYAN}ug file_outline{C_RESET} main.rs                   {C_YELLOW}# unique basename works too{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} main.rs serve.rs config.rs  {C_YELLOW}# batch: several files at once{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} {C_BOLD}'native/src/storage/*.rs'{C_RESET}  {C_YELLOW}# every file in one directory{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} {C_BOLD}'src/**/*.{{ts,tsx}}'{C_RESET} -k 40  {C_YELLOW}# a whole subtree, recursively{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} {C_BOLD}'**/test_*.py'{C_RESET}            {C_YELLOW}# by naming convention, anywhere{C_RESET}");
    println!("  {C_CYAN}ug file_outline{C_RESET} native/src/main.rs --ids   {C_YELLOW}# force ids on when piped{C_RESET}");
}

fn print_get_code_help() {
    println!("  {C_CYAN}ug get_code{C_RESET}  {C_YELLOW}— read full source for a node id or file/line range{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug get_code <symbol>... | -f|--file <file> [options]");
    println!();
    print_node_ref_help();
    println!("  Or read raw lines instead, with {C_CYAN}--file{C_RESET} and a line window.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-f, --file <file>{C_RESET}     Repo-relative file path (instead of a symbol)");
    println!("  {C_CYAN}-s, --start <n>{C_RESET}       First line (1-based, with --file; default 1)");
    println!("  {C_CYAN}-e, --end <n>{C_RESET}         Last line inclusive (with --file; default EOF)");
    println!("  {C_CYAN}-r, --range <window>{C_RESET}  Both at once (with --file), same dialect as {C_CYAN}ug query --range{C_RESET}:");
    println!("                        {C_CYAN}11-35{C_RESET} · {C_CYAN}34-end{C_RESET} · {C_CYAN}20{C_RESET} (the first 20) · {C_CYAN}11..35{C_RESET} · {C_CYAN}rows 11 to 35{C_RESET}");
    println!("                        {C_DIM}-s/-e win if you give both spellings{C_RESET}");
    println!("  {C_CYAN}--max-chars <n>{C_RESET}       Character cap per symbol (default 20000)");
    println!("  {C_CYAN}--no-doc{C_RESET}              Drop the leading doc-comment preview (the body only)");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!("  {C_CYAN}--json{C_RESET}                Machine-readable output");
    println!();
    print_wildcard_help();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} flag_value                    {C_YELLOW}# by name{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} \"function:native/src/main.rs:124:flag_value\"  {C_YELLOW}# by id{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} <id1> <id2> <id3>            {C_YELLOW}# batch (--max-chars applies per symbol){C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} {C_BOLD}'render_*'{C_RESET} --no-doc          {C_YELLOW}# every renderer's body{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} -f native/src/types.rs --range 180-210");
    println!("  {C_CYAN}ug get_code{C_RESET} -f native/src/types.rs -r 400-end   {C_YELLOW}# to EOF{C_RESET}");
    println!("  {C_CYAN}ug get_code{C_RESET} -f README.md                 {C_YELLOW}# whole file{C_RESET}");
    println!();
    println!("{C_DIM}Source comes from the index, so this works with the repo absent; a file that");
    println!("changed since indexing is served with a staleness warning.{C_RESET}");
}

fn print_project_overview_help() {
    println!("  {C_CYAN}ug project_overview{C_RESET}  {C_YELLOW}— orient yourself in the codebase in one call{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug project_overview [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug project_overview{C_RESET}");
    println!();
    println!("Shows:");
    println!("  • Repo root and db location");
    println!("  • Node/edge counts by type");
    println!("  • Biggest files by symbol count");
    println!("  • Most depended-upon symbols (hotspots)");
}

/// Emit an agent-tool result: raw JSON when `--json`/`-o` was given,
/// otherwise the ANSI rendering. Exits non-zero when any item in the batch
/// failed, so a bad id in a script is still detectable.
pub(crate) fn emit_agent_result<T: serde::Serialize>(
    args: &[String],
    result: &T,
    render: impl FnOnce() -> String,
    label: &str,
    ok: bool,
) {
    let json = serde_json::to_string_pretty(result).unwrap_or_default();
    if !emit_raw(args, &json, label) {
        print!("{}", render());
    }
    if !ok {
        std::process::exit(1);
    }
}

pub(crate) fn run_find_symbols(args: &[String]) {
    run_find_symbols_with(args, false)
}

fn run_find_symbols_with(args: &[String], include_docs: bool) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_find_symbols_help();
        return;
    }
    // Accept graph_search's legacy leading `<graph-file>` positional.
    let (load_args, queries) = analysis_input(args);
    let boundary = has_flag(args, "--boundary");
    if queries.is_empty() && !boundary {
        eprintln!("Usage: ug find_symbols <name>... [--node-type <type>]... [--file-prefix <prefix>] [--boundary] [-k <n>] [--include-docs] [-n <project>]");
        std::process::exit(1);
    }
    let (node_id, mut name) = split_ids_and_names(&queries);
    // `--boundary` on its own is a listing, not a search: "show me this
    // service's public surface" is a question someone asks precisely because
    // they do not yet know what any of it is called.
    if name.is_empty() && node_id.is_empty() {
        name.push("*".to_string());
    }
    let params = agent_tools::FindSymbolsParams {
        node_id,
        name,
        // `--node-type` is the canonical spelling; `-t/--type` still parses.
        node_types: multi_flag(args, &["--node-type", "-t", "--type"]),
        file_prefix: flag_value(args, &["--file-prefix", "-f", "--file"]),
        limit: flag_value(args, &["-k", "--limit", "-l"]).and_then(|s| s.parse().ok()),
        include_docs: include_docs || has_flag(args, "--include-docs"),
        boundary,
    };
    let (graph, _raw, _path) = load_agent_graph(&load_args);

    let result = agent_tools::find_symbols(&graph, &params);
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_find_symbols(&result, Render::Ansi),
        "find_symbols result",
        ok,
    );
}

pub(crate) fn run_file_outline(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_file_outline_help();
        return;
    }
    let files = positionals(args, AGENT_VALUE_FLAGS);
    if files.is_empty() {
        eprintln!("Usage: ug file_outline <file>... [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, _path) = load_agent_graph(args);

    // A `file:`-prefixed id is a File node id *and* a path — `file_outline`
    // resolves either, so both buckets end up in the same place.
    let (node_id, file) = files
        .into_iter()
        .partition(|s| looks_like_node_id(s) && !s.starts_with("file:"));
    let mut result = agent_tools::file_outline(
        &graph,
        &agent_tools::FileOutlineParams {
            node_id,
            file,
            max_files: flag_value(args, &["--max-files", "-k", "--limit"])
                .and_then(|s| s.parse().ok()),
        },
    );
    // Show ids in a terminal (humans want them); hide them when piped since
    // an agent can reconstruct `kind:file:name`. `--ids` forces them on.
    result.show_ids = has_flag(args, "--ids") || ultragraph::color::enabled();
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_file_outline(&result, Render::Ansi),
        "file_outline result",
        ok,
    );
}

pub(crate) fn run_get_code(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_get_code_help();
        return;
    }
    let node_ids = positionals(args, AGENT_VALUE_FLAGS);
    let file_flag = flag_value(args, &["-f", "--file"]);
    if node_ids.is_empty() && file_flag.is_none() {
        eprintln!("Usage: ug get_code <node-id>... | -f|--file <file> [-s|--start <line>] [-e|--end <line>] [--range <window>] [--max-chars <n>] [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);

    // `--range 11-35` is one flag for what `-s 11 -e 35` says in two. Both
    // are handed to the tool as written: resolving the window there is what
    // gives MCP and HTTP the same flag, in the same dialect `ug query` uses.
    let params = agent_tools::GetCodeParams {
        node_id: node_ids,
        file: file_flag,
        start_line: flag_value(args, &["--start-line", "-s", "--start"])
            .and_then(|s| s.parse().ok()),
        end_line: flag_value(args, &["--end-line", "-e", "--end"]).and_then(|s| s.parse().ok()),
        range: flag_value(args, &["--range", "-r"]),
        max_chars: flag_value(args, &["--max-chars"]).and_then(|s| s.parse().ok()),
        no_doc: has_flag(args, "--no-doc"),
    };

    let indexed = indexed_source(&graph_path, &agent_tools::get_code_source_ids(&graph, &params));
    let result = agent_tools::get_code(
        &graph,
        agent_tools::SourceCtx::new(&indexed, &repo_root),
        &params,
    );
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_get_code(&result, Render::Ansi),
        "get_code result",
        ok,
    );
}

pub(crate) fn run_project_overview(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_project_overview_help();
        return;
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);

    let result = agent_tools::project_overview(&graph, &repo_root, &graph_path);
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_project_overview(&result, Render::Ansi),
        "project_overview result",
        true,
    );
}

pub(crate) fn run_find_usages(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_find_usages_help();
        return;
    }
    let node_ids = positionals(args, AGENT_VALUE_FLAGS);
    if node_ids.is_empty() {
        eprintln!("Usage: ug find_usages <node-id>... [-k|--hops <n>] [-t|--edge-type <type>]... [-n|--name <project>]");
        std::process::exit(1);
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);

    let params = agent_tools::FindUsagesParams {
        node_id: node_ids,
        hops: flag_value(args, &["--hops", "-k"]).and_then(|s| s.parse().ok()),
        edge_types: multi_flag(args, &["--edge-type", "-t"]),
    };
    let indexed = indexed_source(
        &graph_path,
        &agent_tools::find_usages_source_ids(&graph, &params),
    );
    let result = agent_tools::find_usages(
        &graph,
        agent_tools::SourceCtx::new(&indexed, &repo_root),
        &params,
    );
    let ok = result.ok();
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_find_usages(&result, Render::Ansi),
        "find_usages result",
        ok,
    );
}

pub(crate) fn run_graph_schema(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_graph_schema_help();
        return;
    }
    let (graph, _raw, graph_path) = load_agent_graph(args);

    let result = agent_tools::graph_schema(&graph, &graph_path);
    emit_agent_result(
        args,
        &result,
        || agent_tools::render_graph_schema(&result, Render::Ansi),
        "graph_schema result",
        true,
    );
}

fn print_find_usages_help() {
    println!("  {C_CYAN}ug find_usages{C_RESET}  {C_YELLOW}— who uses this symbol? (callers, importers, subclasses){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Follows edges {C_BOLD}inbound{C_RESET}: everything that calls / references / imports /");
    println!("  extends / implements the given node. The reverse of {C_CYAN}ug traverse{C_RESET}");
    println!("  (which walks outbound dependencies). Same logic as the MCP find_usages tool.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug find_usages <symbol>... [options]");
    println!();
    print_node_ref_help();
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-k, --hops <n>{C_RESET}         Transitive depth 1-3 (default 1 = direct users only)");
    println!("  {C_CYAN}-t, --edge-type <type>{C_RESET}  Restrict to edge type (repeatable; default: calls,");
    println!("                         references, imports, extends, implements — see {C_CYAN}ug graph_schema{C_RESET})");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}    Project name (default: cwd basename)");
    println!("  {C_CYAN}--json{C_RESET}                 Machine-readable output");
    println!();
    print_wildcard_help();
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug find_usages{C_RESET} connect                   {C_YELLOW}# who calls it, with call sites{C_RESET}");
    println!("  {C_CYAN}ug find_usages{C_RESET} \"function:src/db.ts:42:connect\" -k 2 -t calls");
    println!("  {C_CYAN}ug find_usages{C_RESET} <id1> <id2>               {C_YELLOW}# batch: several symbols at once{C_RESET}");
    println!("  {C_CYAN}ug find_usages{C_RESET} {C_BOLD}'validate_*'{C_RESET}             {C_YELLOW}# blast radius of a whole family{C_RESET}");
    println!("  {C_CYAN}ug find_usages{C_RESET} {C_BOLD}'*Repository'{C_RESET} -t implements  {C_YELLOW}# who implements each one{C_RESET}");
    println!();
    println!("{C_DIM}Nothing found is a real answer here — it means no indexed edge points at the");
    println!("symbol. Check {C_RESET}{C_CYAN}ug graph_schema{C_RESET}{C_DIM} for which edge types this graph actually has.{C_RESET}");
}

fn print_graph_schema_help() {
    println!("  {C_CYAN}ug graph_schema{C_RESET}  {C_YELLOW}— node & edge types in this graph (metadata){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  Lists the node types and edge types actually present in the project's");
    println!("  graph (with counts and what each edge type connects), plus the full");
    println!("  vocabulary indexers can emit. Check this before passing edge-type");
    println!("  filters to {C_CYAN}ug find_usages{C_RESET} / {C_CYAN}ug traverse{C_RESET} — filtering on a type the graph");
    println!("  doesn't contain silently returns nothing.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug graph_schema [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}  Project name (default: cwd basename)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug graph_schema{C_RESET}");
}
