//! `ug gen` — the one-command pipeline (index → graph → ingest), for both a
//! first run against a path and a re-run of an existing project (whose
//! recorded repo root is used when no path is named).
//!
//! Embedding is a fourth, optional stage: it is off unless `--with-embed`
//! asks for it, because a knowledge graph is structure and none of the
//! structural tools read a vector. See `wants_embeddings`.

use std::env;
use std::fs;
use std::path::Path;

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
use super::scope;
use super::dest::{IngestOutcome, announce_destinations, store_specs_from_args};

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

/// What `ug gen` should index when the caller named no path: the repo
/// root recorded in an existing project's `project.json`, else the cwd.
///
/// Re-running the pipeline and running it the first time are the same
/// operation, so one command covers both — there is no separate
/// regenerate. The project is resolved the way every project-scoped
/// command resolves it (`-n/--name` → active project → cwd basename),
/// and the resolved name rides along with the root — it may differ from
/// the root's basename (`ug gen -i /x/myrepo -n custom`), and deriving
/// it from the root would write the refreshed graph into the wrong
/// project dir.
///
/// When the resolved project's recorded root is gone, a name the user
/// pinned (`-n` or the active marker) has nothing safe to fall back to —
/// indexing the cwd into that project would silently re-point it at an
/// unrelated tree — so this stops and says how to repoint. A name that
/// was merely the cwd's basename falls back to `.`: the user is standing
/// in the tree they mean (most likely the repo moved here), which is
/// also exactly what gen did before the project existed.
fn resolve_gen_input(args: &[String]) -> (String, Option<String>) {
    let name = project::resolve_active_project_name(args, ".");
    let Some(meta) = project::read_meta(&project::project_dir(&name)) else {
        return (".".to_string(), None);
    };
    if !meta.repo_root.is_empty() && Path::new(&meta.repo_root).exists() {
        // No announcement here — `run_gen` prints the scope banner a few lines
        // later with this exact project and root, and two lines saying the
        // same thing is how output stops being read.
        return (meta.repo_root.clone(), Some(name));
    }
    if flag_value(args, &["-n", "--name"]).is_some() || project::get_active_project().is_some() {
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  Project {C_BOLD}{}{C_RESET} points at {}, which no longer exists.",
            name,
            if meta.repo_root.is_empty() {
                "(no recorded path)"
            } else {
                &meta.repo_root
            }
        );
        eprintln!(
            "   Re-run {C_CYAN}ug gen -i <path> -n {}{C_RESET} to repoint it.",
            name
        );
        std::process::exit(1);
    }
    (".".to_string(), None)
}

pub(crate) fn run_gen(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_gen_help();
        return;
    }

    let start_total = std::time::Instant::now();

    // An explicit -i/positional always wins. Without one, gen is also the
    // re-run command: the recorded repo root of the project this resolves
    // to (-n → active → cwd basename), else the cwd on a first run.
    let explicit_input = flag_value(args, &["-i", "--input"]).or_else(|| {
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
    });
    let named_input = explicit_input.is_some();
    let (input, pinned_project) = match explicit_input {
        Some(explicit) => (explicit, None),
        None => resolve_gen_input(args),
    };
    let repo_root = input.clone();
    let project_name = pinned_project
        .unwrap_or_else(|| project::resolve_project_name(args, &input));
    // Canonicalized once, here, so the banner, the containment checks and the
    // `repoRoot` written into project.json all name the same resolved tree
    // (Agents.md §9a). A path that does not exist yet keeps its raw form.
    let repo_root_abs = fs::canonicalize(&repo_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| repo_root.clone());
    let output_dir = flag_value(args, &["-o", "--output"])
        .unwrap_or_else(|| project::project_dir(&project_name).to_string_lossy().into_owned());
    scope::announce(
        &project_name,
        Path::new(&output_dir),
        &repo_root_abs,
        if flag_value(args, &["-n", "--name"]).is_some() {
            "-n/--name"
        } else if named_input {
            "input path"
        } else {
            scope::why_project(args, true)
        },
    );
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
    // index.html and the renderer bundles are embedded in `ug serve`
    // (VIS_HTML / VIS_THREEJS_BUNDLE / VIS_COSMOS_BUNDLE) and served directly, so
    // there's no need to write them here.
    println!("{C_CYAN}▸{C_RESET} Writing visualization README");
    fs::write(format!("{}/README.md", output_dir), crate::assets::VIS_MD)
        .unwrap_or_else(|e| die(1, format!("failed to write {output_dir}/README.md: {e}")));
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t2.elapsed()
    );

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
            "  {C_DIM}That is more than skipping embedding: {C_RESET}{C_CYAN}ug analyze{C_RESET}{C_DIM} statistics and blast radius"
        );
        println!(
            "  need the db too. {C_RESET}{C_DIM}Drop the flag to ingest without vectors, which is the default.{C_RESET}"
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
                "  {C_GREEN}✓ {} nodes, {} edges{C_RESET} written in {C_BOLD}{:?}{C_RESET}; {C_YELLOW}{} awaiting vectors{C_RESET} {C_DIM}(embedding is opt-in — --with-embed){C_RESET}",
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
                "Run '{C_BOLD}ug search \"hello\" -n {}{C_RESET}' for a hybrid graph + semantic RAG query.",
                project_name
            );
            println!(
                "Add {C_BOLD}--no-expand{C_RESET} for the matching symbols alone, without the graph walk."
            );
        }
        EmbeddingsOutcome::Missing | EmbeddingsOutcome::Failed => {
            // Structural-only commands — these work without vectors.
            println!(
                "Works now: {C_BOLD}ug find_symbols{C_RESET}, {C_BOLD}ug file_outline{C_RESET}, {C_BOLD}ug traverse{C_RESET}, {C_BOLD}ug analyze{C_RESET}."
            );
            println!(
                "Disabled until embeddings exist: {C_YELLOW}ug search{C_RESET}, chat, tours, the Indexed tab."
            );
            println!();
            println!("{C_BOLD}Next steps:{C_RESET}");
            println!(
                "  1. Start the server and click {C_BOLD}\"Ingest now\"{C_RESET} in any disabled tab:"
            );
            println!("       {C_CYAN}ug serve{C_RESET}  →  {C_CYAN}http://127.0.0.1:8080{C_RESET}");
            println!("  2. Or re-run ingest from another terminal:");
            println!("       {C_CYAN}ug ingest -n {}{C_RESET}", project_name);
            println!(
                "  3. Or ask for them up front next time: {C_CYAN}ug gen --with-embed{C_RESET}"
            );
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
        "{C_BOLD}What works now:{C_RESET}  the graph view, catalog, keyword search, {C_CYAN}ug analyze{C_RESET}, traverse."
    );
    println!(
        "{C_BOLD}Disabled:{C_RESET}       semantic search, chat, guided tours, the Indexed tab — they need vectors."
    );
    println!();
    println!("{C_BOLD}Get embeddings:{C_RESET}");
    println!(
        "  • Re-run with {C_CYAN}--with-embed{C_RESET} to build them as part of the pipeline."
    );
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

    // No vectors unless they were asked for. Loading the embedding model is
    // the single most expensive thing in an otherwise sub-second run, and
    // everything else — structure, facts, keyword statistics, pruning — is
    // what the graph is for, so only semantic search lags and `ug ingest`
    // backfills it. See `wants_embeddings`.
    if !wants_embeddings(args) {
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
        let specs = gen_specs(args, db_path, dim);
        announce_destinations(&specs);
        ingest_with_specs(&specs, &EmbedMode::Embed(&embedder), &graph, prune, &budget).await
    })
}

/// The one help block that explains the default, `--with-embed` and
/// `--no-ingest`.
///
/// Shared by `ug gen -h` and `ug update -h` rather than re-worded per
/// command, because the distinction is the thing people get wrong: "no
/// vectors" and "no database" both read as "skip the slow part", and only
/// one of them leaves `ug analyze` — statistics, `diff_impact`, blast
/// radius — answering from the previous ingest.
pub(crate) fn skip_flags_help() -> String {
    let mut o = String::new();
    macro_rules! line {
        ($($arg:tt)*) => { o.push_str(&format!("{}\n", format_args!($($arg)*))) };
    }
    line!("{C_BOLD}Vectors are opt-in — and \"no vectors\" is NOT \"no database\":{C_RESET}");
    line!("");
    line!("  {C_BOLD}(default){C_RESET}    {C_BOLD}Ingest into the db, without vectors.{C_RESET}");
    line!("               Nodes and edges {C_BOLD}are{C_RESET} written — facts and keyword statistics too.");
    line!("               Only the embedding is skipped, and no embedding model is");
    line!("               loaded, which is most of a small run's wall clock.");
    line!("               {C_GREEN}Current:{C_RESET} the graph.json tools {C_BOLD}and{C_RESET} {C_CYAN}ug analyze{C_RESET} — statistics,");
    line!("                        {C_CYAN}diff_impact{C_RESET}, blast radius, {C_CYAN}traverse --dest{C_RESET}.");
    line!("               {C_YELLOW}Behind:{C_RESET}  {C_CYAN}search{C_RESET} / {C_CYAN}chat{C_RESET} / {C_CYAN}tour{C_RESET} miss the changed");
    line!("                        nodes until the vectors are backfilled.");
    line!("");
    line!("  {C_CYAN}--with-embed{C_RESET} {C_BOLD}Also build the vectors, in the same run.{C_RESET}");
    line!("               Loads the embedding model and embeds every changed node,");
    line!("               so {C_CYAN}search{C_RESET}, {C_CYAN}chat{C_RESET} and {C_CYAN}tour{C_RESET} are live the moment the run ends.");
    line!("               Ask for it when you want semantic search now; otherwise");
    line!("               {C_CYAN}ug ingest -n <project>{C_RESET} backfills it later, at your convenience.");
    line!("");
    line!("  {C_CYAN}--no-ingest{C_RESET}  {C_BOLD}Write nothing to the db at all.{C_RESET}");
    line!("               No nodes, no edges, no vectors — the db is not opened.");
    line!("               Only graph.json is rebuilt.");
    line!("               {C_GREEN}Current:{C_RESET} the graph.json tools only — {C_CYAN}find_symbols{C_RESET},");
    line!("                        {C_CYAN}file_outline{C_RESET}, {C_CYAN}get_code{C_RESET}, {C_CYAN}find_usages{C_RESET}, {C_CYAN}shortest_path{C_RESET},");
    line!("                        {C_CYAN}project_overview{C_RESET}, {C_CYAN}graph_schema{C_RESET}.");
    line!("               {C_YELLOW}Behind:{C_RESET}  {C_BOLD}everything the db backs{C_RESET} — {C_CYAN}ug analyze{C_RESET} statistics and");
    line!("                        blast radius as well as {C_CYAN}search{C_RESET} / {C_CYAN}chat{C_RESET}");
    line!("                        keep answering from the {C_BOLD}previous{C_RESET} ingest.");
    line!("");
    line!("  Without {C_CYAN}--with-embed{C_RESET}, {C_CYAN}ug ingest -n <project>{C_RESET} catches the db up — it embeds");
    line!("  only the nodes still owed a vector.");
    line!("  {C_DIM}(The old {C_RESET}{C_CYAN}--no-embed{C_RESET}{C_DIM} is still accepted; it is the default now.){C_RESET}");
    o
}

/// The store specs for a gen-family ingest, pinned to the project's own db
/// path. Shared by both branches of `run_gen_ingest`.
///
/// gen already resolved the db path with full precedence (`-d/--db` →
/// `<output-dir>/ugdb`), so the flags that would make the store layer resolve
/// its own are dropped and `db_path` handed over as `--db`. Pinning it *into*
/// the arguments rather than overwriting the resulting specs is what lets the
/// store layer announce the project it is actually writing: `-n myproj -o
/// /elsewhere` resolves to `~/.ug/myproj/ugdb` there and `/elsewhere/ugdb`
/// here, and an override after the fact would have already printed the wrong
/// one. Neo4j flags pass through untouched, so a `--dest overgraph,neo4j`
/// fan-out still reaches both.
fn gen_specs(args: &[String], db_path: &str, dim: u32) -> Vec<StoreSpec> {
    let mut pinned: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--name" | "-d" | "--db" | "-o" | "--output" => i += 2,
            _ => {
                pinned.push(args[i].clone());
                i += 1;
            }
        }
    }
    pinned.push("--db".to_string());
    pinned.push(db_path.to_string());
    store_specs_from_args(&pinned, dim)
}

/// Whether this invocation should build vectors.
///
/// Embedding is **opt-in**. A knowledge graph is structure — nodes, edges,
/// facts, keyword statistics — and none of it needs a vector: `find_symbols`,
/// `find_usages`, `traverse`, `ug analyze`, blast radius and `diff_impact`
/// all read the graph, not the embedding space. Vectors buy `search`, `chat`
/// and tours, and they cost most of the wall clock of a run, so `--with-embed`
/// asks for them and `ug ingest -n <project>` backfills them afterwards.
///
/// `--no-embed` is still accepted. It was the old opt-out and is now the
/// default, so installed git hooks and scripts that pass it keep working and
/// keep meaning exactly what they meant; passing it alongside `--with-embed`
/// wins, on the rule that the flag naming a *skip* is the conservative one.
pub(crate) fn wants_embeddings(args: &[String]) -> bool {
    has_flag(args, "--with-embed") && !has_flag(args, "--no-embed")
}

/// The embedding dim and model to plan a vector-less run against.
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
    println!(
        "  {C_CYAN}-i, --input{C_RESET} <path>       Input directory (default: the resolved project's recorded"
    );
    println!(
        "                            repo root — {C_CYAN}-n{C_RESET} → active → cwd basename — else .)"
    );
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
    println!("  {C_GREEN}--with-embed{C_RESET}             {C_BOLD}Also build vectors{C_RESET} {C_DIM}(off by default).{C_RESET} See below.");
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
    println!(
        "  {C_MAGENTA}ug gen{C_RESET} -n myrepo                   {C_YELLOW}# re-run that project from its recorded root{C_RESET}"
    );
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src -n myrepo           {C_YELLOW}# ~/.ug/myrepo/{C_RESET}");
    println!(
        "  {C_MAGENTA}ug gen{C_RESET} --with-embed                 {C_YELLOW}# …and build vectors too (slower){C_RESET}"
    );
    println!("  {C_MAGENTA}ug gen{C_RESET} -i ./src --no-ingest --serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::UG_HOME_LOCK;

    fn isolated_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("UG_HOME", home.path());
        home
    }

    /// Embedding is opt-in. The whole point of the default is that a run
    /// that says nothing about vectors loads no embedding model, so this is
    /// the assertion that keeps `ug gen` and every git hook fast.
    #[test]
    fn vectors_are_opt_in() {
        let arg = |a: &str| vec![a.to_string()];
        assert!(!wants_embeddings(&[]), "a bare run builds no vectors");
        assert!(!wants_embeddings(&arg("--no-ingest")));
        assert!(wants_embeddings(&arg("--with-embed")));
        // The old opt-out is still accepted and still means "no vectors",
        // so an installed hook or a script that passes it keeps working.
        assert!(!wants_embeddings(&arg("--no-embed")));
        assert!(
            !wants_embeddings(&["--with-embed".to_string(), "--no-embed".to_string()]),
            "an explicit skip wins over an explicit ask"
        );
    }

    /// The help must name the flag that turns vectors on, or nobody finds it.
    #[test]
    fn the_help_names_the_opt_in() {
        let help = skip_flags_help();
        assert!(help.contains("--with-embed"));
        assert!(help.contains("ug ingest -n <project>"));
    }

    /// No existing project: gen indexes the cwd, exactly as it did before
    /// projects existed. This is the first-run path.
    #[test]
    fn without_a_project_gen_defaults_to_the_cwd() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let _home = isolated_home();
        let (input, pinned) = resolve_gen_input(&[]);
        assert_eq!(input, ".");
        assert!(pinned.is_none());
        std::env::remove_var("UG_HOME");
    }

    /// An existing project with a live recorded root is re-run from that
    /// root, with the project name pinned — even when it differs from the
    /// root's basename, so the refresh lands in the right project dir.
    #[test]
    fn an_existing_project_re_runs_from_its_recorded_root() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let _home = isolated_home();
        let repo = tempfile::tempdir().expect("repo");
        let dir = project::project_dir("custom");
        std::fs::create_dir_all(&dir).unwrap();
        project::write_meta(
            &dir,
            &project::ProjectMeta::new("custom", repo.path().to_str().unwrap(), 1, 1),
        )
        .unwrap();

        let args = ["-n".to_string(), "custom".to_string()];
        let (input, pinned) = resolve_gen_input(&args);
        assert_eq!(input, repo.path().to_string_lossy(), "the recorded root");
        assert_eq!(pinned.as_deref(), Some("custom"));

        std::env::remove_var("UG_HOME");
    }

    /// A project whose recorded root is gone, resolved only via the cwd's
    /// basename, falls back to the cwd: the user is standing in the tree
    /// they mean (most likely the repo moved here), which is also what gen
    /// did before the project existed.
    #[test]
    fn a_basename_matched_project_with_a_dead_root_falls_back_to_the_cwd() {
        let _guard = UG_HOME_LOCK.blocking_lock();
        let _home = isolated_home();
        let dir = project::project_dir(&project::derive_project_name("."));
        std::fs::create_dir_all(&dir).unwrap();
        project::write_meta(
            &dir,
            &project::ProjectMeta::new("gone", "/no/such/path/anymore", 1, 1),
        )
        .unwrap();

        let (input, pinned) = resolve_gen_input(&[]);
        assert_eq!(input, ".");
        assert!(pinned.is_none());
        std::env::remove_var("UG_HOME");
    }
}
