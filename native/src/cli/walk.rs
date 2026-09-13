//! `ug walk` — a guided walkthrough of a *change*.
//!
//! The sibling of `ug tour`: same guide, same overlay, same renderer, but
//! seeded from a git diff instead of a question. It shares `tour.rs`'s
//! itinerary printer so a walk and a tour read identically in a terminal —
//! the only addition is the `[changed +12/-3]` badge on each stop.
//!
//! git is a soft dependency (see [`crate::git`]): every failure here is a
//! state of the user's machine, so it is reported with the sentence that
//! fixes it rather than as a stack trace.

use std::path::PathBuf;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW};

use crate::git::{self, Commit, DiffSummary, GitError, RevSpec};
use crate::walk;

use super::agent::{agent_repo_root, load_agent_graph};
use super::args::{first_positional, flag_value, has_flag};
use super::chat::chat_client_from_args;
use super::embed::tokio_runtime;
use super::io::{write_file, write_or_print, CliError, CliResult};
use super::tour::{render_tour, render_tour_plan, tour_progress_printer};

/// Flags that take a value, so the first bare positional is the revision
/// and not the argument of the flag before it.
const VALUE_FLAGS: [&str; 20] = [
    "-n", "--name", "-i", "--input", "--max-stops", "--repo-root", "-o", "--output", "--commits",
    "--base-url", "--api-key", "--model", "--chat-base-url", "--chat-api-key", "--chat-model",
    "--temperature", "--max-tokens", "--chat-timeout", "--ring", "--spec",
];

pub(crate) fn run_walk(args: &[String]) -> CliResult {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_walk_help();
        return Ok(());
    }

    let repo_root_flag: Option<PathBuf> = flag_value(args, &["--repo-root"]).map(PathBuf::from);

    // `--commits` is a listing, not a walk: it answers "what can I walk"
    // and exits. It runs before the graph is loaded because it needs git
    // and nothing else.
    if has_flag(args, "--commits") {
        let limit = flag_value(args, &["--commits"])
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(git::DEFAULT_COMMIT_LIMIT);
        let dir = repo_root_flag
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        return list_commits(&dir, limit);
    }

    let spec = RevSpec::parse(
        &flag_value(args, &["--spec"])
            .or_else(|| first_positional(args, &VALUE_FLAGS))
            .unwrap_or_default(),
    );

    let json_output = has_flag(args, "--json");
    let show_plan = has_flag(args, "--show-plan");
    let output_path = flag_value(args, &["-o", "--output"]);

    let mut opts = walk::WalkOptions::new();
    if let Some(n) = flag_value(args, &["--max-stops"]).and_then(|s| s.parse::<usize>().ok()) {
        opts.max_stops = n.clamp(1, crate::tour::MAX_STOPS_LIMIT);
    }
    opts.expand = !has_flag(args, "--no-expand");
    opts.include_snippets = !has_flag(args, "--no-snippets");
    opts.include_debug = json_output || show_plan;
    opts.think = has_flag(args, "--think");

    // The graph, and the root it was indexed from. A walk reads graph.json
    // and never the vector store, so there is no `--dest`, no embedder and
    // no reason for this to fail on a project that was never ingested.
    let (graph, _raw, graph_path) = load_agent_graph(args)?;
    let repo_root = repo_root_flag.unwrap_or_else(|| agent_repo_root(&graph, &graph_path));

    let diff = walk::resolve_diff(&repo_root, &spec).map_err(git_failed)?;
    let drifted = walk::drifted_files(&repo_root, &spec, &diff);

    eprintln!(
        "{C_CYAN}▸{C_RESET} Walking {C_BOLD}{}{C_RESET} — {} file(s), {C_GREEN}+{}{C_RESET}/{C_YELLOW}-{}{C_RESET}",
        diff.label,
        diff.files.len(),
        diff.insertions,
        diff.deletions
    );
    if diff.is_empty() {
        // Not an error: "nothing changed" is a true and useful answer, and
        // exiting non-zero would break `ug walk && …` in a hook.
        println!("\n{C_DIM}  Nothing changed in {}.{C_RESET}\n", diff.label);
        return Ok(());
    }

    let no_llm = has_flag(args, "--no-llm");
    let rt = tokio_runtime();
    let planned = rt.block_on(async {
        let chat = (!no_llm).then(|| chat_client_from_args(args));
        let mut on_progress = tour_progress_printer();
        if no_llm {
            eprintln!("{C_CYAN}▸{C_RESET} Planning walk (ranked, no LLM)…");
        }
        opts.stream = chat.is_some();

        let first = walk::plan_walk(
            &graph,
            &repo_root,
            diff.clone(),
            &drifted,
            chat.as_ref(),
            &opts,
            &mut on_progress,
        )
        .await;

        match first {
            Ok(w) => Ok(w),
            // The guide is optional, so an unreachable model degrades to the
            // ranked itinerary rather than losing the walk.
            Err(e) if chat.is_some() => {
                eprintln!(
                    "{C_YELLOW}▸{C_RESET} tour guide (LLM) unavailable ({}); falling back to a ranked itinerary.",
                    e
                );
                let mut quiet = |_| {};
                let mut fallback_opts = opts.clone();
                fallback_opts.stream = false;
                walk::plan_walk(&graph, &repo_root, diff, &drifted, None, &fallback_opts, &mut quiet)
                    .await
                    .map(|mut w| {
                        w.tour.warnings.push(format!(
                            "The tour guide model was unreachable ({}); showing a ranked itinerary.",
                            e
                        ));
                        w
                    })
            }
            Err(e) => Err(e),
        }
    });

    let planned = planned.map_err(|e| CliError::new(format!("walk failed: {}", e)))?;

    if json_output {
        let payload = walk_json(&planned);
        let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
        write_or_print(output_path.as_deref(), &text, "walk");
        return Ok(());
    }

    print!("{}", render_diff_header(&planned.diff, true));
    print!("{}", render_tour(&planned.tour, true));
    if show_plan {
        print!("{}", render_tour_plan(&planned.tour, true));
    }
    if let Some(p) = output_path.as_deref() {
        let plain = render_diff_header(&planned.diff, false) + &render_tour(&planned.tour, false);
        write_file(p, &plain);
        println!("Wrote walk to {}", p);
    }
    Ok(())
}

/// The JSON envelope: the tour, plus the diff it was planned from.
///
/// The diff rides alongside rather than inside `Tour` so the tour shape
/// stays the same whichever seeded it — a consumer that already parses a
/// tour parses a walk unchanged, and finds the change metadata in a field
/// it can ignore.
pub(crate) fn walk_json(w: &walk::Walk) -> serde_json::Value {
    let mut v = serde_json::to_value(&w.tour).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "diff".into(),
            serde_json::to_value(&w.diff).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "unmapped".into(),
            serde_json::to_value(&w.unmapped).unwrap_or(serde_json::Value::Null),
        );
    }
    v
}

/// Turn a git failure into a CLI failure that says what to do next.
///
/// `hint()` is appended rather than replacing the message: "git is not
/// installed" alone leaves the user wondering what broke, and the hint
/// alone leaves them wondering why.
fn git_failed(e: GitError) -> CliError {
    CliError::new(format!("{}\n  {C_DIM}{}{C_RESET}", e, e.hint()))
}

/// `ug walk --commits` — the terminal's version of the web UI's picker.
fn list_commits(dir: &std::path::Path, limit: usize) -> CliResult {
    let commits = git::recent_commits(dir, limit, None).map_err(git_failed)?;
    if commits.is_empty() {
        println!("{C_DIM}  No commits yet.{C_RESET}");
        return Ok(());
    }
    println!(
        "\n{C_BOLD}{C_CYAN}❯ Recent commits{C_RESET} {C_DIM}(walk one with `ug walk <hash>`){C_RESET}\n"
    );
    for c in &commits {
        println!(
            "  {C_YELLOW}{}{C_RESET}  {}{C_DIM}  · {} · {}{C_RESET}",
            c.short,
            truncate(&c.subject, 64),
            c.relative,
            stat_line(c)
        );
    }
    println!();
    Ok(())
}

fn stat_line(c: &Commit) -> String {
    if c.files == 0 {
        return "no file changes".to_string();
    }
    format!(
        "{} file{}, +{}/-{}",
        c.files,
        if c.files == 1 { "" } else { "s" },
        c.insertions,
        c.deletions
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head)
}

/// The change the walk is about, printed above the itinerary.
///
/// Deliberately not folded into `render_tour`: a tour has no diff, and the
/// two renderers staying separate is what lets the itinerary printer stay
/// the single one both commands share.
fn render_diff_header(d: &DiffSummary, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let (bold, dim, reset) = (c(C_BOLD), c(C_DIM), c(C_RESET));
    let (green, yellow, cyan, magenta) = (c(C_GREEN), c(C_YELLOW), c(C_CYAN), c(C_MAGENTA));

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("{bold}{magenta}◆ {}{reset}\n", d.label));
    out.push_str(&format!(
        "{dim}  {} file(s) · {reset}{green}+{}{reset}{dim} / {reset}{yellow}-{}{reset}\n",
        d.files.len(),
        d.insertions,
        d.deletions
    ));

    // The file list is the change at a glance; past a dozen it is a wall,
    // and the itinerary below is the better summary anyway.
    const SHOW_FILES: usize = 12;
    for f in d.files.iter().take(SHOW_FILES) {
        let mark = match f.status {
            git::ChangeStatus::Added => format!("{green}A{reset}"),
            git::ChangeStatus::Deleted => format!("{yellow}D{reset}"),
            git::ChangeStatus::Renamed => format!("{cyan}R{reset}"),
            git::ChangeStatus::Modified => format!("{dim}M{reset}"),
        };
        out.push_str(&format!(
            "  {} {dim}{}{reset} {dim}(+{}/-{}){reset}\n",
            mark, f.path, f.added, f.removed
        ));
    }
    if d.files.len() > SHOW_FILES {
        out.push_str(&format!(
            "  {dim}… and {} more file(s){reset}\n",
            d.files.len() - SHOW_FILES
        ));
    }
    if d.commits.len() > 1 {
        out.push_str(&format!(
            "{dim}  across {} commits, {} … {}{reset}\n",
            d.commits.len(),
            d.commits.last().map(|c| c.short.as_str()).unwrap_or(""),
            d.commits.first().map(|c| c.short.as_str()).unwrap_or("")
        ));
    }
    out
}

pub(crate) fn print_walk_help() {
    println!(
        r#"{C_BOLD}ug walk{C_RESET} — a guided walkthrough of a change

Seeds a narrated walkthrough from a git diff instead of a question: the stops
are the symbols the change actually touched, ordered by the call graph rather
than by filename, followed by the callers and tests the change reaches.

Reads graph.json only — no vector store, no embedder — so it works on any
generated project. The language model is optional: without one you get the
ordered itinerary, without narration.

{C_BOLD}USAGE{C_RESET}
  ug walk [<rev>] [flags]
  ug walk --commits [<n>]

{C_BOLD}WHAT TO WALK{C_RESET} {C_DIM}(the positional argument){C_RESET}
  {C_DIM}(nothing){C_RESET}        your uncommitted changes — staged and unstaged
  staged            just what is staged
  {C_CYAN}<hash>{C_RESET} / HEAD~2   one commit, against its parent
  main..HEAD        every commit on this branch since main
  main...HEAD       this branch against where it forked from main

{C_BOLD}FLAGS{C_RESET}
  --commits [<n>]   List recent commits and exit (default {commits}) — the picker
  --spec <rev>      Same as the positional, for callers that build an argv
  --max-stops <n>   Upper bound on stops (default {stops}, max {limit})
  --no-expand       Only the changed symbols; skip the callers and tests
  --no-snippets     Omit source snippets from the itinerary
  --no-llm          Skip the guide; print the ranked itinerary
  --think           Let the guide deliberate (slower, occasionally better)
  --show-plan       Also print the raw JSON plan the guide produced
  --json            Machine-readable: the tour, plus the diff it came from
  -o, --output <f>  Write the result to a file as well as printing it
  -n, --name <p>    Project to walk (default: cwd, else most recent)
  -i, --input <f>   graph.json to read instead of the project's
  --repo-root <d>   Where the working tree is (default: the indexed root)

{C_BOLD}MODEL{C_RESET} {C_DIM}(same flags as `ug chat` / `ug tour`){C_RESET}
  --chat-model <m>, --chat-base-url <u>, --chat-api-key <k>,
  --temperature <t>, --max-tokens <n>, --chat-timeout <secs>
  {C_DIM}(--model, --base-url and --api-key are accepted as short spellings){C_RESET}

{C_BOLD}EXAMPLES{C_RESET}
  {C_DIM}# What am I in the middle of changing?{C_RESET}
  ug walk

  {C_DIM}# Review the last commit{C_RESET}
  ug walk HEAD

  {C_DIM}# Walk a whole branch, the way a reviewer would read it{C_RESET}
  ug walk main...HEAD --max-stops 12

  {C_DIM}# Pick a commit to walk{C_RESET}
  ug walk --commits 20

  {C_DIM}# Just the changed symbols, no model, as JSON{C_RESET}
  ug walk HEAD~1 --no-expand --no-llm --json

{C_BOLD}REQUIRES GIT{C_RESET}
  This is the one command that shells out to git. Without git — or outside a
  working tree — it says so and every other ug command is unaffected.
"#,
        commits = git::DEFAULT_COMMIT_LIMIT,
        stops = crate::tour::DEFAULT_MAX_STOPS,
        limit = crate::tour::MAX_STOPS_LIMIT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{ChangeStatus, FileChange, Hunk, LineRange};

    fn diff_with(files: Vec<FileChange>) -> DiffSummary {
        DiffSummary {
            spec: "HEAD".into(),
            label: "commit abc1234 — do a thing".into(),
            insertions: files.iter().map(|f| f.added).sum(),
            deletions: files.iter().map(|f| f.removed).sum(),
            files,
            commits: vec![],
            truncated: false,
        }
    }

    fn file(path: &str, status: ChangeStatus, added: u32, removed: u32) -> FileChange {
        FileChange {
            path: path.into(),
            old_path: None,
            status,
            added,
            removed,
            hunks: vec![Hunk {
                range: LineRange { start: 1, end: 2 },
                added,
                removed,
            }],
            binary: false,
        }
    }

    #[test]
    fn the_header_names_the_change_and_its_files() {
        let d = diff_with(vec![
            file("src/a.rs", ChangeStatus::Modified, 4, 1),
            file("src/b.rs", ChangeStatus::Added, 9, 0),
        ]);
        let out = render_diff_header(&d, false);
        assert!(out.contains("commit abc1234"), "{out}");
        assert!(out.contains("src/a.rs"), "{out}");
        assert!(out.contains("+13"), "{out}");
        assert!(out.contains("-1"), "{out}");
        // Plain mode must be plain: the file is written with `-o`.
        assert!(!out.contains('\u{1b}'), "ANSI leaked into the plain form");
    }

    #[test]
    fn a_long_file_list_is_summarised_not_dumped() {
        let files: Vec<FileChange> = (0..30)
            .map(|i| file(&format!("src/f{i}.rs"), ChangeStatus::Modified, 1, 0))
            .collect();
        let out = render_diff_header(&diff_with(files), false);
        assert!(out.contains("and 18 more file(s)"), "{out}");
    }

    #[test]
    fn a_revision_is_read_from_the_positional_not_from_a_flag_value() {
        // The trap this guards: `--max-stops 12 HEAD~1` must walk HEAD~1,
        // not the string "12".
        let args: Vec<String> = ["--max-stops", "12", "HEAD~1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = first_positional(&args, &VALUE_FLAGS).unwrap_or_default();
        assert_eq!(RevSpec::parse(&got), RevSpec::Commit("HEAD~1".into()));
    }

    #[test]
    fn no_positional_means_the_working_tree() {
        let args: Vec<String> = ["--no-expand"].iter().map(|s| s.to_string()).collect();
        let got = first_positional(&args, &VALUE_FLAGS).unwrap_or_default();
        assert_eq!(RevSpec::parse(&got), RevSpec::Working);
    }

    #[test]
    fn commit_stats_read_as_prose() {
        let c = Commit {
            sha: "a".into(),
            short: "a".into(),
            subject: "s".into(),
            author: "x".into(),
            relative: "now".into(),
            date: "d".into(),
            files: 1,
            insertions: 2,
            deletions: 3,
        };
        assert_eq!(stat_line(&c), "1 file, +2/-3");
        let empty = Commit { files: 0, ..c };
        assert_eq!(stat_line(&empty), "no file changes");
    }
}
