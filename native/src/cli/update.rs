//! `ug update <file>...` — refresh the graph for the files that just changed.
//!
//! `ug gen` re-runs the whole pipeline, which is the right hammer for "the
//! repo drifted". `ug update` is the focused version an agent reaches for
//! after an edit burst: name the file(s) you touched, and it re-indexes,
//! re-resolves cross-file edges, re-embeds the changed nodes, and tells you
//! exactly what landed for those files.
//!
//! Two flags shorten the run, and they are not interchangeable:
//! `--no-embed` still ingests into the db — nodes, edges, facts and keyword
//! statistics all land, only the vectors are skipped (and no embedding model
//! is loaded, which is most of a small run's wall clock), so `ug analyze` and
//! blast radius stay exact. `--no-ingest` writes nothing to the db at all, so
//! everything it backs — `ug analyze` included — keeps answering from the
//! previous ingest. `skip_flags_help` in `gen` is the one place that
//! difference is spelled out for users.
//!
//! The cross-file edge graph is re-resolved over the whole repo on every run,
//! not spliced in place. That is the cost of correctness: an edge into the
//! changed file depends on names and receiver types the change may have
//! moved, and a partial re-resolution that left stale edges would be worse
//! than a full one. The parse cache (blake3 per file) and the ingest diff
//! (hash per node) keep unchanged work out of the hot path, so after a few
//! edits this is cheap — it never re-parses or re-embeds what did not move.

use std::fs;
use std::path::{Path, PathBuf};

use ultragraph::types::GraphData;
use ultragraph::{
    build_graph, index_with_cache, C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW,
};

use crate::project;

use super::args::{has_flag, positionals};
use super::gen::{resolve_gen_cache, run_gen_ingest};
use super::io::die;
use super::scope;

/// Value-carrying flags to skip when collecting the positional file list.
const VALUE_FLAGS: &[&str] = &[
    "-n",
    "--name",
    "-c",
    "--cache",
    "-d",
    "--db",
    "--base-url",
    "--api-key",
    "--model",
    "--embedding-dim",
    "--dest",
    "--neo4j-uri",
    "--neo4j-user",
    "--neo4j-password",
    "--neo4j-database",
];

/// `ug update <file>...` — refresh the index, graph and store for changed
/// files. See the module docs for what it does and does not splice.
pub(crate) fn run_update(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_update_help();
        return;
    }

    let targets = positionals(args, VALUE_FLAGS);
    if targets.is_empty() {
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  {C_BOLD}ug update{C_RESET} needs at least one file. Usage: \
             {C_CYAN}ug update <file>...{C_RESET}"
        );
        eprintln!("   Run {C_CYAN}ug update -h{C_RESET} for details.");
        std::process::exit(2);
    }

    // Same project resolution as every project-scoped command: -n/--name,
    // then the active project, then the cwd's basename.
    let name = project::resolve_active_project_name(args, ".");
    let dir = project::project_dir(&name);
    let Some(meta) = project::read_meta(&dir) else {
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  No generated project named {C_BOLD}{}{C_RESET} under {}.",
            name,
            project::ug_home().display()
        );
        eprintln!(
            "   Run {C_CYAN}ug gen -i <path>{C_RESET} to create it, or {C_CYAN}ug list{C_RESET} to see what exists."
        );
        std::process::exit(1);
    };

    if meta.repo_root.is_empty() || !Path::new(&meta.repo_root).exists() {
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  Project {C_BOLD}{}{C_RESET} points at {}, which no longer exists.",
            name,
            if meta.repo_root.is_empty() {
                "(no recorded path)"
            } else {
                &meta.repo_root
            }
        );
        eprintln!("   Re-run {C_CYAN}ug gen -i <path> -n {}{C_RESET} to repoint it.", name);
        std::process::exit(1);
    }

    scope::announce(
        &name,
        &dir,
        &meta.repo_root,
        scope::why_project(args, true),
    );

    // Canonicalise the repo root ONCE (Agents.md §9a): the recorded path can
    // carry a symlink (`/tmp` → `/private/tmp` on macOS), and comparing an
    // un-canonicalised root against a canonicalised file always reports
    // "escapes the repo" — the failure looks like a security denial, never a
    // bug.
    let repo_root = fs::canonicalize(&meta.repo_root).unwrap_or_else(|_| PathBuf::from(&meta.repo_root));
    let repo_root_str = repo_root.to_string_lossy().to_string();

    // Resolve each target to a repo-relative path, verifying it exists and
    // sits under the repo root. A path that does not resolve is an error
    // naming why, not a silent skip — an agent that asks to update a file it
    // mistyped needs to hear that, not believe the refresh happened.
    let mut rel_targets: Vec<String> = Vec::new();
    for raw in &targets {
        match resolve_target(raw, &repo_root, &meta.files) {
            Ok(rel) => {
                if !meta.files.is_empty() && !meta.files.iter().any(|f| f == &rel) {
                    eprintln!(
                        "{C_YELLOW}⚠{C_RESET}  {} is not in the index for {C_BOLD}{}{C_RESET} — \
                         it will be added if its extension is supported.",
                        rel, name
                    );
                }
                rel_targets.push(rel);
            }
            Err(e) => {
                eprintln!("{C_YELLOW}⚠{C_RESET}  {}", e);
                std::process::exit(1);
            }
        }
    }

    let start = std::time::Instant::now();
    let output_dir = dir.to_string_lossy().into_owned();
    let no_ingest = has_flag(args, "--no-ingest");

    println!(
        "{C_CYAN}▸{C_RESET} Updating {C_BOLD}{}{C_RESET} for {} file(s){}",
        name,
        rel_targets.len(),
        if no_ingest {
            " (graph.json only — nothing written to the db, --no-ingest)"
        } else if has_flag(args, "--no-embed") {
            " (db written without vectors, --no-embed)"
        } else {
            ""
        }
    );

    let cache = resolve_gen_cache(args, &output_dir);
    let index_result = match cache {
        Some(c) => index_with_cache(repo_root_str.clone(), c),
        None => ultragraph::index(repo_root_str.clone()),
    };

    let t = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Building graph");
    let graph_json = build_graph(index_result.clone());
    println!("  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}", t.elapsed());

    let parsed: Option<GraphData> = serde_json::from_str(&graph_json).ok();
    let (nodes_count, edges_count) = parsed
        .as_ref()
        .map(|g| (g.nodes.len(), g.edges.len()))
        .unwrap_or((0, 0));

    // Persist the rebuilt outputs so the next command — `ug find_symbols`,
    // the server, `ug gen` — reads the refreshed graph.
    let graph_path = format!("{}/graph.json", output_dir);
    fs::write(&graph_path, &graph_json)
        .unwrap_or_else(|e| die(1, format!("failed to write {graph_path}: {e}")));
    fs::write(format!("{}/indexed-tree.json", output_dir), &index_result)
        .unwrap_or_else(|e| die(1, format!("failed to write {output_dir}/indexed-tree.json: {e}")));
    let mut new_meta = project::ProjectMeta::new(&name, &repo_root_str, nodes_count, edges_count)
        .carrying_pending_vectors(Path::new(&output_dir));
    if let Some(g) = parsed.as_ref() {
        new_meta = new_meta.with_graph_index(g);
    }
    if let Err(e) = project::write_meta(Path::new(&output_dir), &new_meta) {
        eprintln!("⚠ failed to write project.json: {}", e);
    }

    // Per-target report: how many symbols the refreshed graph holds for each
    // file the caller named. Computed from the new graph so it reflects what
    // actually landed, not what the parse produced.
    println!();
    for rel in &rel_targets {
        let symbols = parsed
            .as_ref()
            .map(|g| {
                g.nodes
                    .iter()
                    .filter(|n| n.file.as_deref() == Some(rel.as_str()))
                    .count()
            })
            .unwrap_or(0);
        if symbols == 0 && !repo_root.join(rel).exists() {
            println!("  {C_GREEN}✓{C_RESET} {:<50} deleted — dropped from the index", rel);
        } else if symbols == 0 {
            println!(
                "  {C_YELLOW}·{C_RESET} {:<50} no indexed symbols (unsupported extension?)",
                rel
            );
        } else {
            println!("  {C_GREEN}✓{C_RESET} {:<50} {} symbol(s)", rel, symbols);
        }
    }

    let db_path = format!("{}/ugdb", output_dir);
    if no_ingest {
        println!();
        println!(
            "{C_YELLOW}⚠ Nothing written to the db (--no-ingest){C_RESET} — no nodes, no edges, no vectors."
        );
        println!(
            "  {C_DIM}Fresh: graph.json tools ({C_RESET}{C_CYAN}find_symbols{C_RESET}{C_DIM}, {C_RESET}{C_CYAN}find_usages{C_RESET}{C_DIM}, …). Answering from the previous"
        );
        println!(
            "  ingest: {C_RESET}{C_CYAN}ug analyze{C_RESET}{C_DIM} statistics and blast radius, {C_RESET}{C_CYAN}search{C_RESET}{C_DIM}, {C_RESET}{C_CYAN}chat{C_RESET}{C_DIM}."
        );
        println!(
            "  Use {C_RESET}{C_CYAN}--no-embed{C_RESET}{C_DIM} instead to keep the db current except for vectors.{C_RESET}"
        );
        println!("Updated {C_BOLD}{}{C_RESET} in {C_BOLD}{:?}{C_RESET}", name, start.elapsed());
        return;
    }

    println!();
    match run_gen_ingest(&graph_json, &db_path, args) {
        Ok(out) => {
            if out.vectors_skipped > 0 {
                println!(
                    "  {C_GREEN}✓{C_RESET} {} nodes, {} edges written; {C_YELLOW}{} awaiting vectors{C_RESET} {C_DIM}(--no-embed){C_RESET}",
                    out.nodes, out.edges, out.vectors_skipped
                );
                println!(
                    "  {C_DIM}Structure, statistics and blast radius are current. Run {C_RESET}{C_CYAN}ug ingest -n {}{C_RESET}{C_DIM} to catch semantic search up.{C_RESET}",
                    name
                );
            } else if out.embedding_error.is_some() {
                println!(
                    "  {C_YELLOW}⚠ {} nodes, {} edges{C_RESET} re-indexed {C_BOLD}without vectors{C_RESET}",
                    out.nodes, out.edges
                );
            } else {
                println!(
                    "  {C_GREEN}✓{C_RESET} {} nodes, {} edges embedded",
                    out.nodes, out.edges
                );
            }
            // The metadata above was written before the ingest ran, so the
            // pending mark is applied here, where the answer is known.
            project::set_pending_vectors(Path::new(&output_dir), out.vectors_skipped > 0);
            println!("Updated {C_BOLD}{}{C_RESET} in {C_BOLD}{:?}{C_RESET}", name, start.elapsed());
        }
        Err(e) => {
            eprintln!("{C_YELLOW}⚠ db-ingest skipped — {}{C_RESET}", e);
            println!(
                "Graph refreshed for {C_BOLD}{}{C_RESET} in {C_BOLD}{:?}{C_RESET} (ingest failed)",
                name,
                start.elapsed()
            );
        }
    }
}

/// Turn a target argument into a repo-relative path, checking it exists and
/// is inside the repo root.
///
/// Accepts three spellings an agent might use: an absolute path under the
/// repo, a path relative to the current directory, or an already
/// repo-relative path. Whichever resolves, the result is repo-relative so it
/// matches what the graph stores.
///
/// A path that no longer exists is a *deletion* when the index already knows
/// it (`indexed` holds `project.json`'s file list) — that is exactly what a
/// commit that removes a file looks like, and the refresh has to drop its
/// nodes. Anything else that does not exist is still an error: an agent that
/// mistypes a filename needs to hear so, not believe a refresh happened.
fn resolve_target(raw: &str, repo_root: &Path, indexed: &[String]) -> Result<String, String> {
    let candidate = Path::new(raw);
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        // Relative to cwd first — an agent's working directory is usually the
        // repo root, but not always.
        let from_cwd = std::env::current_dir().unwrap_or_default().join(candidate);
        if from_cwd.exists() {
            from_cwd
        } else {
            // Fall back to repo-root-relative.
            repo_root.join(candidate)
        }
    };

    let canon = match fs::canonicalize(&abs) {
        Ok(c) => c,
        Err(_) => {
            if let Some(rel) = deleted_target(raw, &abs, repo_root, indexed) {
                return Ok(rel);
            }
            return Err(format!(
                "{} does not exist (looked as {} and under repo root {}) — nothing to update.",
                raw,
                abs.display(),
                repo_root.display()
            ));
        }
    };

    if !canon.starts_with(repo_root) {
        return Err(format!(
            "{} is outside the project's repo root {} — `ug update` only refreshes files in the project.",
            raw,
            repo_root.display()
        ));
    }

    let rel = canon
        .strip_prefix(repo_root)
        .map_err(|e| format!("could not make {} repo-relative: {}", raw, e))?;
    Ok(rel.to_string_lossy().into_owned())
}

/// The repo-relative path for a target that is gone from disk but present in
/// the index — a deletion to be re-indexed away. `None` for anything the
/// index never held, which keeps a typo an error.
///
/// `abs` cannot be canonicalised (it does not exist), so the repo-relative
/// form is derived lexically, falling back to the argument as given when it
/// was already relative — the spelling `git diff --name-only` produces.
fn deleted_target(
    raw: &str,
    abs: &Path,
    repo_root: &Path,
    indexed: &[String],
) -> Option<String> {
    let rel = abs
        .strip_prefix(repo_root)
        .map(|p| p.to_string_lossy().into_owned())
        .ok()
        .or_else(|| Path::new(raw).is_relative().then(|| raw.to_string()))?;
    indexed.iter().any(|f| f == &rel).then_some(rel)
}

fn print_update_help() {
    println!("  {C_CYAN}ug update{C_RESET}  {C_YELLOW}— refresh the graph for the files that just changed{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  The focused counterpart to {C_CYAN}ug gen{C_RESET}: name the file(s) you edited and");
    println!("  this re-indexes, re-resolves cross-file edges, and re-embeds the changed");
    println!("  nodes. Incremental via the blake3 parse cache and the ingest diff, so");
    println!("  unchanged files are neither re-parsed nor re-embedded.");
    println!();
    println!("  Built for a live editing session: call it after an edit burst so the");
    println!("  structural and statistical tools ({C_CYAN}find_usages{C_RESET}, {C_CYAN}ug analyze{C_RESET}, …) reflect what");
    println!("  you just wrote. Cross-file edges are re-resolved over the whole graph on");
    println!("  each run — that is what keeps them correct.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug update <file>... [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <project>   Project to update (default: the active one)");
    println!("  {C_CYAN}--no-embed{C_RESET}              Ingest into the db, without vectors {C_DIM}(what git hooks use){C_RESET}");
    println!("  {C_CYAN}--no-ingest{C_RESET}             Write nothing to the db; refresh graph.json only");
    println!("      {C_DIM}The two are explained side by side below — they are often confused.{C_RESET}");
    println!("      {C_DIM}…plus every {C_RESET}{C_CYAN}ug gen{C_RESET}{C_DIM} embedder flag — see {C_RESET}{C_CYAN}ug gen -h{C_RESET}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug update{C_RESET} native/src/agent_tools.rs");
    println!("  {C_CYAN}ug update{C_RESET} src/a.ts src/b.ts -n myrepo");
    println!("  {C_CYAN}ug update{C_RESET} native/src/cli/mod.rs --no-embed   {C_DIM}# db current except vectors{C_RESET}");
    println!("  {C_CYAN}ug update{C_RESET} native/src/cli/mod.rs --no-ingest  {C_DIM}# graph.json only, db untouched{C_RESET}");
    println!();
    print!("{}", super::gen::skip_flags_help());
}
