//! `ug files` — the CLI door to [`crate::files`]: flags in, the shared table
//! or JSON envelope out. The MCP `files` tool and `POST /api/tools/files` are
//! the other two doors to the same code.

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RED, C_RESET, C_YELLOW};

use crate::files::{self as core, FileQuery, RawQuery, DEFAULT_ROWS};
use crate::project::{self, ProjectMeta};

use super::args::{flag_value, has_flag, multi_flag, positionals};
use super::io::{write_or_print, CliError, CliResult};
use super::scope;

const VALUE_FLAGS: &[&str] = &[
    "-n", "--name", "-g", "--glob", "-e", "--ext", "--lang", "--kind", "--status", "--sort",
    "-k", "--limit", "-r", "--range", "-o", "--output",
];

/// The flags, as the one query every transport builds.
pub(crate) fn query_from_args(args: &[String]) -> Result<FileQuery, CliError> {
    let mut patterns = positionals(args, VALUE_FLAGS);
    patterns.extend(multi_flag(args, &["-g", "--glob"]));
    FileQuery::new(&RawQuery {
        patterns,
        exts: multi_flag(args, &["-e", "--ext"]),
        langs: multi_flag(args, &["--lang"]),
        kind: flag_value(args, &["--kind"]),
        states: multi_flag(args, &["--status"]),
        sort: flag_value(args, &["--sort"]),
        limit: flag_value(args, &["-k", "--limit"]),
        range: flag_value(args, &["-r", "--range"]),
    })
    .map_err(CliError::usage)
}

/// How the project to list was chosen: `-n`, the active project, the
/// directory — and, as the agent tools do, the most recently updated project
/// when none of those has one.
fn resolve(args: &[String]) -> Result<(std::path::PathBuf, ProjectMeta, &'static str), CliError> {
    let name = project::resolve_active_project_name(args, ".");
    let dir = project::project_dir(&name);
    if let Some(meta) = project::read_meta(&dir) {
        return Ok((dir, meta, scope::why_project(args, true)));
    }
    if flag_value(args, &["-n", "--name"]).is_some() {
        return Err(CliError::new(format!(
            "No project named {name:?} — {C_CYAN}ug list{C_RESET} shows the ones that exist."
        )));
    }
    project::list_projects()
        .into_iter()
        .next()
        .map(|(dir, meta)| (dir, meta, "most recently updated project"))
        .ok_or_else(|| CliError::new(format!("No projects yet. Run {C_CYAN}ug gen{C_RESET} in a repo to index one.")))
}

pub(crate) fn run_files(args: &[String]) -> CliResult {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_files_help();
        return Ok(());
    }
    let json = has_flag(args, "--json");
    let query = query_from_args(args)?;
    let (dir, meta, why) = resolve(args)?;
    scope::announce(&meta.name, &dir, &meta.repo_root, why);

    let answer = core::run(&dir, &meta, &query, !json);
    let out = if json {
        core::to_json(&answer).to_string()
    } else {
        // A result, like `ug analyze`'s table: plain text through a pipe or
        // with NO_COLOR (the colour gate in `cli::run`); the help keeps its colour.
        core::render(&answer, ultragraph::color::enabled(), &|from, to| next_command(args, from, to))
    };
    let output = flag_value(args, &["-o", "--output"]);
    write_or_print(output.as_deref(), out.trim_end(), "file list");
    Ok(())
}

/// Quote an argument for the `next:` line only when a shell would mangle it.
fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./,=:@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The command that shows the next window: the same arguments, with the
/// window replaced — so the filters the user typed carry over.
fn next_command(args: &[String], next_from: usize, next_to: usize) -> String {
    let mut words = vec!["ug".to_string(), "files".to_string()];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-k" | "--limit" | "-r" | "--range") {
            i += 2;
            continue;
        }
        words.push(shell_word(a));
        i += 1;
    }
    words.push("--range".into());
    words.push(format!("{next_from}-{next_to}"));
    words.join(" ")
}

fn print_files_help() {
    println!("  {C_CYAN}ug files{C_RESET}  {C_YELLOW}— the files a project's index holds, and whether each is current{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  One row per indexed file: language, docs or code, size, last modified,");
    println!("  and its status against the index — {C_GREEN}fresh{C_RESET}, {C_YELLOW}changed{C_RESET} since (re-run ug gen),");
    println!("  or {C_RED}missing{C_RESET}. The same check {C_CYAN}ug list{C_RESET} counts with. Read-only and cheap:");
    println!("  one stat per file, no graph parse.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug files [<pattern>...] [options]");
    println!();
    println!("{C_BOLD}Filters:{C_RESET}  {C_DIM}(all of them must hold; several patterns are alternatives){C_RESET}");
    println!("  {C_CYAN}<pattern>, -g, --glob <p>{C_RESET}  Wildcard on the path, repeatable. Without a {C_CYAN}/{C_RESET} it also");
    println!("                             matches the file name: {C_DIM}'*.ts' · 'src/**/*.rs' · '*{{fare,refund}}*'{C_RESET}");
    println!("  {C_CYAN}-e, --ext <ext>{C_RESET}            Extension, repeatable or comma-separated: {C_DIM}--ext ts,md{C_RESET}");
    println!("      {C_CYAN}--lang <name>{C_RESET}          Language: typescript, python, java, rust, markdown, pdf");
    println!("      {C_CYAN}--kind <k>{C_RESET}             code or docs");
    println!("      {C_CYAN}--status <s>{C_RESET}           fresh, changed or missing (comma-separated for several)");
    println!();
    println!("{C_BOLD}Output:{C_RESET}");
    println!("      {C_CYAN}--sort <by>{C_RESET}            path (default), size (largest first), modified (newest first)");
    println!("  {C_CYAN}-k, --limit <n>{C_RESET}            Rows to show (default {DEFAULT_ROWS}) — shorthand for --range 1-N");
    println!("  {C_CYAN}-r, --range <window>{C_RESET}       Which rows, 1-based and inclusive: {C_DIM}20 · 51-100 · 34-end{C_RESET}");
    println!("      {C_CYAN}--json{C_RESET}                 Machine-readable: every match unless -k/--range is given,");
    println!("                             with per-status counts over all matches");
    println!("  {C_CYAN}-o, --output <file>{C_RESET}        Write the result to a file");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}       Project to list (default: the active one)");
    println!();
    println!("  Wildcards: {}", ultragraph::pattern::SYNTAX_SUMMARY);
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  ug files                                {C_DIM}# the first {DEFAULT_ROWS} files{C_RESET}");
    println!("  ug files --range 51-100                 {C_DIM}# the next page{C_RESET}");
    println!("  ug files '*.md' --kind docs             {C_DIM}# documentation only{C_RESET}");
    println!("  ug files 'src/**' --ext ts,tsx --sort size");
    println!("  ug files --status changed,missing       {C_DIM}# what ug gen would refresh{C_RESET}");
    println!("  ug files -n my-repo --json | jq '.files[].path'");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_build_the_shared_query_and_bad_values_are_usage_errors() {
        assert!(query_from_args(&a(&["*.ts", "-g", "src/**", "--ext", "ts,md", "--kind", "code", "--status", "fresh", "--sort", "size", "-k", "5"])).is_ok());
        for bad in [&["--kind", "binary"][..], &["--status", "stale"], &["--sort", "random"], &["-k", "0"], &["--range", "banana"]] {
            assert_eq!(query_from_args(&a(bad)).unwrap_err().code, 2, "{bad:?}");
        }
    }

    #[test]
    fn the_next_command_keeps_the_filters_and_replaces_the_window() {
        let args = a(&["-n", "tidewater", "*.ts", "--kind", "code", "-k", "20"]);
        assert_eq!(next_command(&args, 21, 40), "ug files -n tidewater '*.ts' --kind code --range 21-40");
        assert_eq!(next_command(&a(&["--range", "51-100"]), 101, 150), "ug files --range 101-150");
    }
}
