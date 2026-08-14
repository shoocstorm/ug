//! `ug gen` and `ug regen` — the one-command pipeline (index → graph →
//! ingest → embed), and re-running it over an existing project.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ultragraph::limits::EmbedBudget;
use ultragraph::storage::StoreSpec;
use ultragraph::types::GraphData;
use ultragraph::{
    build_graph, index, index_with_cache, C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET,
    C_YELLOW,
};

use crate::{project, serve};

use super::args::{first_positional, flag_value, has_flag};
use super::embed::{budget_from_args, embedder_from_args, tokio_runtime};
use super::ingest::{ingest_with_specs, EmbedMode};
use super::io::die;
use super::store::{IngestOutcome, announce_destinations, store_specs_from_args};

/// Decide which directory `ug gen` should use as its incremental parse
/// cache: `None` disables caching and forces a full re-parse.
///
/// Precedence: `--no-cache` → `-c/--cache` → the output dir (default).
/// A cache written by a different `ug` version is discarded rather than
/// trusted — `indexed-tree.json` holds parsed `FileNode`s, so an
/// indexer change between versions would otherwise keep serving nodes
/// in the old shape for every file whose content happened not to change.
pub(crate) fn resolve_gen_cache(args: &[String], output_dir: &str) -> Option<String> {
    if has_flag(args, "--no-cache") {
        return None;
    }
    if let Some(explicit) = flag_value(args, &["-c", "--cache"]) {
        return Some(explicit);
    }
    let this_version = env!("CARGO_PKG_VERSION");
    if let Some(meta) = project::read_meta(Path::new(output_dir)) {
        if !meta.ug_version.is_empty() && meta.ug_version != this_version {
            println!(
                "{C_YELLOW}▸{C_RESET} Index was built by ug {} (now {}) — re-parsing from scratch.",
                meta.ug_version, this_version
            );
            return None;
        }
    }
    Some(output_dir.to_string())
}

/// `ug regen [-n <project>]` — re-run the pipeline for a project that has
/// already been generated once.
///
/// This is `ug gen` with the input path remembered rather than retyped:
/// it reads `repoRoot` out of the project's `project.json` and hands off.
/// Everything else — the content-hash cache, the progress output, the
/// degraded-embedder path — is `gen`'s, because re-running the pipeline
/// and running it the first time are the same operation.
///
/// **On the name.** This indexes, rebuilds the graph *and* re-embeds into
/// the store, so `reindex` — which names only the first of the three —
/// would be a lie. `regen` says what it does and pairs with the `gen` it
/// repeats.
pub(crate) fn run_regen(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_regen_help();
        return;
    }

    // Same resolution order every project-scoped command uses: -n/--name,
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
        // The recorded path is how regen knows what to re-read; without a
        // usable one there is nothing to re-run against, and guessing the
        // cwd would silently re-index the wrong tree.
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

    println!(
        "{C_CYAN}▸{C_RESET} Regenerating {C_BOLD}{}{C_RESET} from {}",
        name, meta.repo_root
    );

    // Forward the caller's flags (embedder overrides, --no-ingest, …) and
    // add the two `gen` needs, unless they were already supplied.
    let mut forwarded: Vec<String> = args.to_vec();
    if flag_value(args, &["-i", "--input"]).is_none() {
        forwarded.push("-i".into());
        forwarded.push(meta.repo_root.clone());
    }
    if flag_value(args, &["-n", "--name"]).is_none() {
        forwarded.push("-n".into());
        forwarded.push(name);
    }
    run_gen(&forwarded);
}

fn print_regen_help() {
    println!("  {C_CYAN}ug regen{C_RESET}  {C_YELLOW}— re-run the pipeline for an existing project{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("  {C_CYAN}ug gen{C_RESET} with the input path remembered instead of retyped — it reads");
    println!("  the repo root from the project's own metadata. Incremental: unchanged");
    println!("  files are skipped via content hashes, so this is cheap after a few edits.");
    println!();
    println!("  Runs the whole pipeline (index → graph → embed), which is why it is");
    println!("  {C_BOLD}regen{C_RESET} and not {C_DIM}reindex{C_RESET}.");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug regen [-n <project>] [gen options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name <project>{C_RESET}   Project to regenerate (default: the active one)");
    println!("  {C_CYAN}--no-ingest{C_RESET}            {C_BOLD}Write nothing to the db.{C_RESET} graph.json only — no nodes,");
    println!("                         no edges, no vectors. See {C_CYAN}ug gen -h{C_RESET} for what that costs.");
    println!("  {C_CYAN}--no-embed{C_RESET}             {C_BOLD}Write the db, without vectors.{C_RESET} Nodes and edges land as");
    println!("                         usual; only embedding is skipped.");
    println!("      {C_DIM}…plus every {C_RESET}{C_CYAN}ug gen{C_RESET}{C_DIM} option — see {C_RESET}{C_CYAN}ug gen -h{C_RESET}");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  ug regen                    {C_DIM}# the active project{C_RESET}");
    println!("  ug regen -n myrepo");
    println!("  ug regen --no-embed         {C_DIM}# fast: db current except vectors{C_RESET}");
    println!("  ug regen --no-ingest        {C_DIM}# graph.json only, db untouched{C_RESET}");
}

pub(crate) fn run_gen(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_gen_help();
        return;
    }

    let start_total = std::time::Instant::now();

    let input = flag_value(args, &["-i", "--input"])
        .or_else(|| {
            first_positional(
                args,
                &[
                    "-i",
                    "--input",
                    "-c",
                    "--cache",
                    "-o",
                    "--output",
                    "-d",
                    "--db",
                    "-n",
                    "--name",
                    "--base-url",
                    "--api-key",
                    "--model",
                    "--embedding-dim",
                ],
            )
        })
        .unwrap_or_else(|| ".".to_string());
    let repo_root = input.clone();
    let project_name = project::resolve_project_name(args, &input);
    let output_dir = flag_value(args, &["-o", "--output"])
        .unwrap_or_else(|| project::project_dir(&project_name).to_string_lossy().into_owned());
    // Parse caching is on by default, keyed to the project dir — which
    // already holds the `indexed-tree.json` snapshot `index_with_cache`
    // needs to restore a cached file's nodes, so enabling it costs one
    // extra file (`cache.json`) and nothing else. Re-indexing an
    // unchanged repo is the common case (`ug gen` again, the KB
    // Manager's re-index button), and re-parsing every file for it is
    // pure waste: the cache only skips *per-file* tree-sitter work, and
    // cross-file resolution still runs fresh in `build_graph`, so it
    // cannot produce stale edges.
    //
    // It is bypassed when the snapshot was written by a different `ug`
    // build, since a parser or extractor change would otherwise be
    // silently masked by cached nodes in the old shape.
    let cache = resolve_gen_cache(args, &output_dir);
    let no_ingest = has_flag(args, "--no-ingest");
    let chain_serve = has_flag(args, "--serve");
    // Full precedence here: -d/--db flag → <output-dir>/ugdb.
    // run_gen_ingest then pins the default OverGraph spec to this path.
    let db_path = flag_value(args, &["-d", "--db"])
        .unwrap_or_else(|| format!("{}/ugdb", output_dir));

    let pipeline_summary = if no_ingest {
        "index → graph → visualization"
    } else {
        "index → graph → visualization → ingest"
    };
    println!(
        "⚡ Full pipeline: {C_BOLD}{C_MAGENTA}{}{C_RESET}",
        pipeline_summary
    );

    let _ = fs::create_dir_all(&output_dir);

    let t0 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Indexing {C_YELLOW}{}{C_RESET}", input);
    let index_result = match cache {
        Some(c) => index_with_cache(input, c),
        None => index(input),
    };
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t0.elapsed()
    );

    let t1 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Building graph");
    let graph = build_graph(index_result.clone());
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t1.elapsed()
    );

    // Parse once, typed. This used to be an untyped `serde_json::Value` parse
    // that existed only to `.len()` two arrays; the typed graph costs less and
    // also feeds the file index recorded in project.json below.
    let parsed_graph: Option<ultragraph::types::GraphData> = serde_json::from_str(&graph).ok();
    let (nodes_count, edges_count) = parsed_graph
        .as_ref()
        .map(|g| (g.nodes.len(), g.edges.len()))
        .unwrap_or((0, 0));
    println!("  nodes: {}", nodes_count);
    println!("  edges: {}", edges_count);

    // How every call site was resolved, or why it wasn't. Printed because a
    // call graph's quality is otherwise unmeasurable: a wrong edge and a
    // right edge look the same in a total, and the way this fails is the
    // confident wrong answer. `dropped` is mostly healthy — a call into the
    // standard library belongs there — but a jump in it between two runs of
    // the same repo means resolution regressed.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&graph) {
        if let Some(r) = v.get("resolution") {
            let n = |k: &str| r.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let (q, t, b, d) = (
                n("resolvedQualified"),
                n("resolvedTyped"),
                n("resolvedByName"),
                n("droppedUnresolved"),
            );
            println!(
                "  calls: {} resolved ({} by path, {} by receiver type, {} by name), {} unresolved",
                q + t + b,
                q,
                t,
                b,
                d
            );
        }
    }

    let graph_path = format!("{}/graph.json", output_dir);
    fs::write(&graph_path, &graph)
        .unwrap_or_else(|e| die(1, format!("failed to write {graph_path}: {e}")));
    fs::write(format!("{}/indexed-tree.json", output_dir), &index_result)
        .unwrap_or_else(|e| die(1, format!("failed to write {output_dir}/indexed-tree.json: {e}")));

    let t2 = std::time::Instant::now();
    // index.html and ug-vis.bundle.js are embedded in `ug serve` (VIS_HTML /
    // VIS_BUNDLE) and served directly, so there's no need to write them here.
    println!("{C_CYAN}▸{C_RESET} Writing visualization README");
    fs::write(format!("{}/README.md", output_dir), crate::assets::VIS_MD)
        .unwrap_or_else(|e| die(1, format!("failed to write {output_dir}/README.md: {e}")));
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t2.elapsed()
    );

    let repo_root_abs = fs::canonicalize(&repo_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| repo_root.clone());
    let mut meta =
        project::ProjectMeta::new(&project_name, &repo_root_abs, nodes_count, edges_count)
            .carrying_pending_vectors(Path::new(&output_dir));
    // Record the indexed file list so `/api/projects/staleness` can stat the
    // tree without re-reading and re-parsing graph.json on every poll.
    if let Some(g) = parsed_graph.as_ref() {
        meta = meta.with_graph_index(g);
    }
    if let Err(e) = project::write_meta(Path::new(&output_dir), &meta) {
        eprintln!("⚠ failed to write project.json: {}", e);
    }

    println!("{C_BOLD}────────────────────────────────────────{C_RESET}");
    println!(
        "{C_GREEN}✓ Generated{C_RESET} project {C_BOLD}{}{C_RESET} in {C_BOLD}{}/{C_RESET}",
        project_name, output_dir
    );
    println!("  {C_GREEN}✓{C_RESET} graph.json");
    println!("  {C_GREEN}✓{C_RESET} indexed-tree.json");
    println!("  {C_GREEN}✓{C_RESET} README.md");
    println!("  {C_GREEN}✓{C_RESET} project.json");

    if no_ingest {
        println!(
            "{C_YELLOW}⚠ Nothing written to the db (--no-ingest){C_RESET} — no nodes, no edges, no vectors."
        );
        println!(
            "  {C_DIM}That is more than skipping embedding: {C_RESET}{C_CYAN}ug query{C_RESET}{C_DIM} statistics and blast radius"
        );
        println!(
            "  need the db too. {C_RESET}{C_CYAN}--no-embed{C_RESET}{C_DIM} ingests without vectors if that is what you wanted.{C_RESET}"
        );
        // Make the path forward explicit before the serve line buries it.
        // Without this the user gets "Run 'ug serve'" and learns only after
        // the server starts that chat, tours, and the Indexed tab are dark.
        print_embeddings_missing_next_steps(&project_name, chain_serve);
        if chain_serve {
            println!("Total time: {C_BOLD}{:?}{C_RESET}", start_total.elapsed());
            chain_to_serve(args, &graph_path, &db_path, true, &repo_root);
            return;
        }
        println!("Total time: {C_BOLD}{:?}{C_RESET}", start_total.elapsed());
        return;
    }

    println!();
    let t3 = std::time::Instant::now();
    println!(
        "{C_CYAN}▸{C_RESET} Ingesting graph data into DB {C_YELLOW}{}{C_RESET}",
        db_path
    );
    let ingest_outcome = match run_gen_ingest(&graph, &db_path, args) {
        Ok(out) if out.vectors_skipped > 0 => {
            println!(
                "  {C_GREEN}✓ {} nodes, {} edges{C_RESET} written in {C_BOLD}{:?}{C_RESET}; {C_YELLOW}{} awaiting vectors{C_RESET} {C_DIM}(--no-embed){C_RESET}",
                out.nodes,
                out.edges,
                t3.elapsed(),
                out.vectors_skipped
            );
            project::set_pending_vectors(Path::new(&output_dir), true);
            EmbeddingsOutcome::Missing
        }
        Ok(out) if out.embedding_error.is_some() => {
            // Not a success. The index is real and queryable, but calling
            // it "embedded" would be false, and the user needs to know a
            // re-run is owed before semantic search is trustworthy.
            println!(
                "  {C_YELLOW}⚠ {} nodes, {} edges{C_RESET} indexed {C_BOLD}without vectors{C_RESET} in {C_BOLD}{:?}{C_RESET}",
                out.nodes,
                out.edges,
                t3.elapsed()
            );
            EmbeddingsOutcome::Missing
        }
        Ok(out) => {
            println!(
                "  {C_GREEN}✓ {} nodes, {} edges{C_RESET} embedded in {C_BOLD}{:?}{C_RESET}",
                out.nodes,
                out.edges,
                t3.elapsed()
            );
            project::set_pending_vectors(Path::new(&output_dir), false);
            EmbeddingsOutcome::Ready
        }
        Err(e) => {
            eprintln!("{C_YELLOW}⚠ db-ingest skipped — {}{C_RESET}", e);
            EmbeddingsOutcome::Failed
        }
    };

    println!("────────────────────────────────────────");

    match ingest_outcome {
        EmbeddingsOutcome::Ready => {
            // Only suggest these now that they actually work — suggesting
            // them after an ingest failure was actively misleading.
            println!(
                "Run '{C_BOLD}ug semantic_search \"hello\" -n {}{C_RESET}' for a semantic RAG query.",
                project_name
            );
            println!(
                "Run '{C_BOLD}ug search \"hello\" -n {}{C_RESET}' for a hybrid graph + semantic RAG query.",
                project_name
            );
        }
        EmbeddingsOutcome::Missing | EmbeddingsOutcome::Failed => {
            // Structural-only commands — these work without vectors.
            println!(
                "Works now: {C_BOLD}ug find_symbols{C_RESET}, {C_BOLD}ug file_outline{C_RESET}, {C_BOLD}ug traverse{C_RESET}, {C_BOLD}ug query{C_RESET}."
            );
            println!(
                "Disabled until embeddings exist: {C_YELLOW}ug search{C_RESET}, {C_YELLOW}ug semantic_search{C_RESET}, chat, tours, the Indexed tab."
            );
            println!();
            println!("{C_BOLD}Next steps:{C_RESET}");
            println!(
                "  1. Start the server and click {C_BOLD}\"Ingest now\"{C_RESET} in any disabled tab:"
            );
            println!("       {C_CYAN}ug serve{C_RESET}  →  {C_CYAN}http://127.0.0.1:8080{C_RESET}");
            println!("  2. Or re-run ingest from another terminal:");
            println!("       {C_CYAN}ug ingest -n {}{C_RESET}", project_name);
        }
    }
    println!("Total time: {C_BOLD}{:?}{C_RESET}", start_total.elapsed());

    if chain_serve {
        chain_to_serve(args, &graph_path, &db_path, false, &repo_root);
    } else {
        println!(
            "Run '{C_BOLD}ug serve{C_RESET}' and open {C_CYAN}http://127.0.0.1:8080{C_RESET} to view the graph."
        );
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum EmbeddingsOutcome {
    /// Vectors written — semantic search, chat, and tours work.
    Ready,
    /// `--no-ingest` or no vectors produced. Structure is in place; the
    /// vector-backed features are dark until a follow-up ingest runs.
    Missing,
    /// Ingest itself errored. Same UI impact as `Missing` from the user's
    /// point of view; tracked separately so the CLI can word the next
    /// steps honestly (it isn't known yet whether the structural DB is
    /// usable — depending on where ingest failed).
    Failed,
}

/// The "you don't have embeddings — here's how to get them" block,
/// printed by `ug gen` whenever ingest was skipped or didn't produce
/// vectors. Mirrored in the disabled tabs of the web UI, so the user
/// sees the same two paths (button or CLI) whichever surface they hit
/// first.
fn print_embeddings_missing_next_steps(project_name: &str, chain_serve: bool) {
    println!();
    println!(
        "{C_BOLD}What works now:{C_RESET}  the graph view, catalog, keyword search, {C_CYAN}ug query{C_RESET}, traverse."
    );
    println!(
        "{C_BOLD}Disabled:{C_RESET}       semantic search, chat, guided tours, the Indexed tab — they need vectors."
    );
    println!();
    println!("{C_BOLD}Get embeddings:{C_RESET}");
    if chain_serve {
        // Server is starting in-process; no need to suggest `ug serve`.
        println!(
            "  • The server is starting — open {C_CYAN}http://127.0.0.1:8080{C_RESET} and click {C_BOLD}\"Ingest now\"{C_RESET} in any disabled tab."
        );
    } else {
        println!(
            "  • Start the server and click {C_BOLD}\"Ingest now\"{C_RESET} in any disabled tab:"
        );
        println!("       {C_CYAN}ug serve{C_RESET}  →  {C_CYAN}http://127.0.0.1:8080{C_RESET}");
    }
    println!("  • Or run ingest from this terminal:");
    println!("       {C_CYAN}ug ingest -n {}{C_RESET}", project_name);
}

/// Build a synthetic args vec for `serve` from the gen invocation and call
/// `serve::run_serve`. Inherits port/host/watch/repo-root and embedder flags
/// from the original invocation; sets `-i`/`-d` to the freshly generated
/// paths, and `--no-db` when the ingest step was skipped.
fn chain_to_serve(args: &[String], graph_path: &str, db_path: &str, no_db: bool, repo_root: &str) {
    let mut serve_args: Vec<String> = vec![
        "-i".to_string(),
        graph_path.to_string(),
        "-d".to_string(),
        db_path.to_string(),
        "--repo-root".to_string(),
        repo_root.to_string(),
    ];
    if no_db {
        serve_args.push("--no-db".to_string());
    }
    if has_flag(args, "--watch") {
        serve_args.push("--watch".to_string());
    }
    for &flag in &[
        "-p",
        "--port",
        "--host",
        "--repo-root",
        "--base-url",
        "--api-key",
        "--model",
        "--embedding-dim",
    ] {
        if let Some(v) = flag_value(args, &[flag]) {
            serve_args.push(flag.to_string());
            serve_args.push(v);
        }
    }
    println!();
    println!("────────────────────────────────────────");
    println!("Starting web server...");
    serve::run_serve(&serve_args);
}

pub(crate) fn run_gen_ingest(
    graph_json: &str,
    db_path: &str,
    args: &[String],
) -> Result<IngestOutcome, String> {
    let graph: GraphData =
        serde_json::from_str(graph_json).map_err(|e| format!("parse graph: {}", e))?;
    // Ingest upserts, so a node dropped from the source would otherwise
    // linger in the store and keep surfacing in search. Pruning is on by
    // default because `ug gen` indexes a whole repo; `--no-prune` is for
    // the case where several inputs deliberately share one project dir.
    let prune = !has_flag(args, "--no-prune");

    // `--no-embed` is the fast path a git hook takes: it never builds an
    // embedder, because loading the local model is the single most expensive
    // thing in an otherwise sub-second incremental run. Everything else —
    // structure, facts, keyword statistics, pruning — still happens, so only
    // semantic search lags, and `ug ingest` backfills it.
    if has_flag(args, "--no-embed") {
        let (dim, model) = store_dim_and_model(db_path, args);
        let budget = EmbedBudget::resolve(&model, section_cap_override(args));
        let rt = tokio_runtime();
        return rt.block_on(async {
            let specs = gen_specs(args, db_path, dim);
            announce_destinations(&specs);
            ingest_with_specs(&specs, &EmbedMode::Skip(model), &graph, prune, &budget).await
        });
    }

    let mut embedder = embedder_from_args(args);
    let budget = budget_from_args(&embedder, args);
    let dim_was_explicit = flag_value(args, &["--embedding-dim"]).is_some();
    let rt = tokio_runtime();
    rt.block_on(async {
        if !dim_was_explicit {
            let probed = embedder
                .probe_dim()
                .await
                .map_err(|e| format!("embedder dim probe: {}", e))?;
            if probed != embedder.config().dim {
                embedder.set_dim(probed);
            }
        }
        let dim = embedder.config().dim as u32;
        // `ug gen` accepts the same --dest / --neo4j-* flags as `ug
        // ingest`. When --dest is omitted we keep the OverGraph-only
        // behavior pointed at `db_path`.
        let mut specs = store_specs_from_args(args, dim);
        // gen already resolved the db path with full precedence
        // (-d/--db → <output-dir>/ugdb), so pin every OverGraph spec to
        // it — including in a --dest overgraph,neo4j fan-out, where the
        // store layer's own resolution can no longer lean on -o.
        for spec in &mut specs {
            if let StoreSpec::Overgraph {
                path,
                embedding_dim: _,
            } = spec
            {
                *path = PathBuf::from(db_path);
            }
        }
        announce_destinations(&specs);
        ingest_with_specs(&specs, &EmbedMode::Embed(&embedder), &graph, prune, &budget).await
    })
}

/// The one help block that explains `--no-embed` vs `--no-ingest`.
///
/// Shared by `ug gen -h`, `ug regen -h` and `ug update -h` rather than
/// re-worded per command, because the difference is the thing people get
/// wrong: both read as "skip the slow part", and only one of them leaves
/// `ug query` — statistics, `diff_impact`, blast radius — answering from the
/// previous ingest.
pub(crate) fn skip_flags_help() -> String {
    let mut o = String::new();
    macro_rules! line {
        ($($arg:tt)*) => { o.push_str(&format!("{}\n", format_args!($($arg)*))) };
    }
    line!("{C_BOLD}The two skip flags are NOT the same thing:{C_RESET}");
    line!("");
    line!("  {C_CYAN}--no-embed{C_RESET}   {C_BOLD}Ingest into the db, without vectors.{C_RESET}");
    line!("               Nodes and edges {C_BOLD}are{C_RESET} written — facts and keyword statistics too.");
    line!("               Only the embedding is skipped, and no embedding model is");
    line!("               loaded, which is most of a small run's wall clock.");
    line!("               {C_GREEN}Current:{C_RESET} the graph.json tools {C_BOLD}and{C_RESET} {C_CYAN}ug query{C_RESET} — statistics,");
    line!("                        {C_CYAN}diff_impact{C_RESET}, blast radius, {C_CYAN}traverse --dest{C_RESET}.");
    line!("               {C_YELLOW}Behind:{C_RESET}  {C_CYAN}search{C_RESET} / {C_CYAN}semantic_search{C_RESET} / {C_CYAN}chat{C_RESET} miss the changed");
    line!("                        nodes until the vectors are backfilled.");
    line!("");
    line!("  {C_CYAN}--no-ingest{C_RESET}  {C_BOLD}Write nothing to the db at all.{C_RESET}");
    line!("               No nodes, no edges, no vectors — the db is not opened.");
    line!("               Only graph.json is rebuilt.");
    line!("               {C_GREEN}Current:{C_RESET} the graph.json tools only — {C_CYAN}find_symbols{C_RESET},");
    line!("                        {C_CYAN}file_outline{C_RESET}, {C_CYAN}get_code{C_RESET}, {C_CYAN}find_usages{C_RESET}, {C_CYAN}shortest_path{C_RESET},");
    line!("                        {C_CYAN}project_overview{C_RESET}, {C_CYAN}graph_schema{C_RESET}.");
    line!("               {C_YELLOW}Behind:{C_RESET}  {C_BOLD}everything the db backs{C_RESET} — {C_CYAN}ug query{C_RESET} statistics and");
    line!("                        blast radius as well as {C_CYAN}search{C_RESET} / {C_CYAN}semantic_search{C_RESET} / {C_CYAN}chat{C_RESET}");
    line!("                        keep answering from the {C_BOLD}previous{C_RESET} ingest.");
    line!("");
    line!("  Either way, {C_CYAN}ug ingest -n <project>{C_RESET} catches the db up — it embeds only");
    line!("  the nodes still owed a vector.");
    o
}

/// The store specs for a gen-family ingest, pinned to the project's own db
/// path. Shared by both branches of `run_gen_ingest`.
///
/// gen already resolved the db path with full precedence (`-d/--db` →
/// `<output-dir>/ugdb`), so every OverGraph spec is pinned to it — including
/// in a `--dest overgraph,neo4j` fan-out, where the store layer's own
/// resolution can no longer lean on `-o`.
fn gen_specs(args: &[String], db_path: &str, dim: u32) -> Vec<StoreSpec> {
    let mut specs = store_specs_from_args(args, dim);
    for spec in &mut specs {
        if let StoreSpec::Overgraph { path, embedding_dim: _ } = spec {
            *path = PathBuf::from(db_path);
        }
    }
    specs
}

/// The embedding dim and model to plan a `--no-embed` run against.
///
/// Both normally come from the loaded embedder, which is exactly what this
/// path refuses to load — so they come off the store instead. That is also
/// the *correct* source: opening an existing store with any other dim is a
/// hard error, and planning against any other model would invalidate every
/// vector already in it. Falls back to the configured defaults only when
/// there is no store yet, which is the first-ever ingest.
fn store_dim_and_model(db_path: &str, args: &[String]) -> (u32, String) {
    if let Some((dim, model)) = ultragraph::storage::recorded_dim_and_model(Path::new(db_path)) {
        if dim > 0 {
            return (dim, model.unwrap_or_else(|| configured_model(args)));
        }
    }
    let dim = flag_value(args, &["--embedding-dim"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(ultragraph::storage::DEFAULT_EMBEDDING_DIM as u32);
    (dim, configured_model(args))
}

/// The embedding model this invocation resolves to, without constructing an
/// embedder: `--model`, else a persisted `embed.model`, else the default.
fn configured_model(args: &[String]) -> String {
    crate::config::resolve_pref_cfg(flag_value(args, &["--model"]), "embed.model")
        .0
        .unwrap_or_else(|| ultragraph::storage::DEFAULT_MODEL.to_string())
}

/// `--section-cap` as a number, for the budget resolution that normally
/// happens inside `budget_from_args`.
fn section_cap_override(args: &[String]) -> Option<usize> {
    crate::config::resolve_pref_cfg(flag_value(args, &["--section-cap"]), "embed.section_cap")
        .0
        .and_then(|s| s.parse().ok())
}

fn print_gen_help() {
    println!(
        "  {C_BOLD}{C_MAGENTA}⚡ ug gen{C_RESET}  {C_YELLOW}— end-to-end knowledge graph pipeline{C_RESET}"
    );
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!(
        "  {C_CYAN}index{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}graph{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}visualization{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}OverGraph ingest{C_RESET}"
    );
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug gen [<path>] [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-i, --input{C_RESET} <path>       Input directory (default: .)");
    println!(
        "  {C_CYAN}-c, --cache{C_RESET} <dir>        Parse cache directory (default: the output dir)"
    );
    println!(
        "  {C_YELLOW}--no-cache{C_RESET}                Re-parse every file, ignoring the parse cache"
    );
    println!(
        "  {C_YELLOW}--no-prune{C_RESET}                Keep store rows for nodes no longer in the graph"
    );
    println!(
        "  {C_CYAN}-n, --name{C_RESET} <name>        Project name (default: input dir basename)"
    );
    println!(
        "  {C_CYAN}-o, --output{C_RESET} <dir>       Output directory (default: ~/.ug/<name>)"
    );
    println!(
        "  {C_CYAN}-d, --db{C_RESET} <dir>           OverGraph directory (default: <output-dir>/ugdb)"
    );
    println!("  {C_YELLOW}--no-ingest{C_RESET}              {C_BOLD}Skip the whole db step.{C_RESET} See below.");
    println!("  {C_YELLOW}--no-embed{C_RESET}               {C_BOLD}Ingest, minus the vectors.{C_RESET} See below.");
    println!("  {C_GREEN}--serve{C_RESET}                  Chain into 'ug serve' on the generated outputs");
    println!(
        "                            (inherits -p/--port, --host, --watch, --repo-root, embedder flags)"
    );
    println!();
    print!("{}", skip_flags_help());
    println!();
    println!("{C_BOLD}Embedding (in-process by default; --base-url switches to remote):{C_RESET}");
    println!("  {C_CYAN}--model{C_RESET} <name>           Local fastembed alias or remote model name");
    println!("                              (default: bge-small-en-v1.5, 384d).");
    println!("  {C_CYAN}--base-url{C_RESET} <url>         {C_YELLOW}Opt into remote{C_RESET} /v1/embeddings endpoint.");
    println!("  {C_CYAN}--api-key{C_RESET} <key>          Bearer token for the remote endpoint.");
    println!(
        "  {C_CYAN}--embedding-dim{C_RESET} <n>      Override vector dim (auto-probed otherwise)."
    );
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET}                              {C_YELLOW}# ~/.ug/<cwd-basename>/{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src -n myrepo           {C_YELLOW}# ~/.ug/myrepo/{C_RESET}");
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src --no-ingest --serve");
}
