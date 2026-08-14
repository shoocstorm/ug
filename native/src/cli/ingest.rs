//! `ug ingest` — writing a built `graph.json` into one or more knowledge
//! stores, with embeddings and progress reporting. Also the ingest half
//! of `ug gen`.

use std::fs;

use ultragraph::limits::EmbedBudget;
use ultragraph::storage::{
    self, open_store, Embedder, KnowledgeStore, StoreError, StoreSet, StoreSpec,
};
use ultragraph::types::GraphData;
use ultragraph::{C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW};

use crate::project;

use super::args::{flag_value, has_flag};
use super::embed::{budget_from_args, embedder_from_args, tokio_runtime};
use super::io::die;
use super::scope;
use super::store::{IngestOutcome, announce_destinations, store_specs_from_args};

/// What an ingest run should do about vectors.
///
/// The skipping half exists because loading the local embedding model costs
/// more than everything else in a small incremental ingest put together —
/// well over a second against roughly 300 ms for the structural work. A run
/// triggered by a git hook wants the structure now and can let the vectors
/// arrive later, so it writes the changed nodes with no vector and leaves the
/// model stamp alone. The incremental planner then sees a vector of the wrong
/// width on those rows and re-embeds exactly them on the next `ug ingest` —
/// the same backfill path a failed embedder already takes.
pub(crate) enum EmbedMode<'a> {
    /// Embed the changed nodes with this embedder.
    Embed(&'a Embedder),
    /// Skip embedding. Carries the model name the *store* was written with,
    /// which the planner still needs: it decides whether the vectors already
    /// in the store may be carried forward.
    Skip(String),
}

impl EmbedMode<'_> {
    /// The model the plan should be made against — the live embedder's, or
    /// the one recorded on the store when we are not loading an embedder.
    fn model(&self) -> &str {
        match self {
            EmbedMode::Embed(e) => &e.config().model,
            EmbedMode::Skip(model) => model,
        }
    }

    fn embedder(&self) -> Option<&Embedder> {
        match self {
            EmbedMode::Embed(e) => Some(e),
            EmbedMode::Skip(_) => None,
        }
    }
}

// ingest graph data into one or more knowledge-store backends.
// Works against any `KnowledgeStore` impl (OverGraph, Neo4j, …).
async fn ingest_graph_with_progress(
    store: &dyn KnowledgeStore,
    embed_mode: &EmbedMode<'_>,
    graph: &GraphData,
    prune: bool,
    budget: &EmbedBudget,
) -> Result<IngestOutcome, String> {
    let nodes_count = graph.nodes.len();
    let edges_count = graph.edges.len();

    let t0 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Building node texts ({})", nodes_count);
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Capture first: node texts fold in each node's own comments, and the
    // same captured source is written to the rows further down.
    let captured = storage::capture_for_graph(graph);
    let texts = storage::build_texts(graph, &captured, budget);
    // Corpus statistics for BM25 keyword weighting. Must land before the
    // upsert — they decide which terms survive each node's dimension cap.
    let stats = storage::refresh_sparse_stats(&[store], &texts, &captured, graph);
    println!(
        "\r{C_CYAN}▸{C_RESET} Building node texts: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET} — {} keyword terms tracked",
        t0.elapsed(),
        stats.terms()
    );

    // Diff against what's already stored so a re-index only pays for what
    // actually changed. On a first run everything lands in `to_embed` and
    // this costs one cheap miss per node.
    let tp = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Diffing against Graph DB ({})", nodes_count);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let model = embed_mode.model().to_string();
    let plan = storage::plan_incremental_ingest(store, graph, &texts, false, &captured, Some(&model))
        .await
        .map_err(|e| format!("reading existing nodes: {}", e))?;
    let to_embed = plan.to_embed.len();
    println!(
        "\r{C_CYAN}▸{C_RESET} Diffing against Graph DB: {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET} — {} unchanged, {} moved, {} to embed",
        tp.elapsed(),
        plan.unchanged,
        plan.reusable.len(),
        to_embed
    );

    let t1 = std::time::Instant::now();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(to_embed);
    // A failed embed degrades rather than aborts. Everything else in this
    // pipeline is already computed — structure, source, and every derived
    // fact — and discarding it leaves the user with no index at all when
    // they could have had one that answers structural and statistical
    // questions. Nodes past the failure are written with no vector; the
    // next run sees a missing vector and backfills it.
    let mut embed_error: Option<String> = None;
    let mut vectors_skipped = 0usize;
    if to_embed == 0 {
        println!("{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}✓ skipped{C_RESET} (no node text changed)");
    } else if embed_mode.embedder().is_none() {
        // Deliberate, so it is not an `embed_error`: nothing failed, and
        // calling it a failure would put a warning on every commit.
        vectors_skipped = to_embed;
        vectors.resize(to_embed, Vec::new());
        println!(
            "{C_CYAN}▸{C_RESET} Embedding: {C_YELLOW}✓ skipped (--no-embed){C_RESET} — {} node(s) written without vectors",
            to_embed
        );
    } else {
        let embedder = embed_mode.embedder().expect("checked above");
        print!("{C_CYAN}▸{C_RESET} Embedding nodes ({})", to_embed);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        for (i, chunk) in plan.to_embed.chunks(embedder.config().batch_size).enumerate() {
            let chunk_vec: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            match embedder.embed(&chunk_vec).await {
                Ok(chunk_vectors) => vectors.extend(chunk_vectors),
                Err(e) => {
                    embed_error = Some(e.to_string());
                    break;
                }
            }
            let processed = std::cmp::min((i + 1) * embedder.config().batch_size, to_embed);
            let pct = processed as f32 / to_embed as f32 * 100.0;
            print!(
                "\r{C_CYAN}▸{C_RESET} Embedding: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
                pct, processed, to_embed
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        match &embed_error {
            None => println!(
                "\r{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
                t1.elapsed()
            ),
            Some(e) => {
                let done = vectors.len();
                println!(
                    "\r{C_YELLOW}⚠{C_RESET} Embedding failed after {}/{} — {}",
                    done, to_embed, e
                );
                println!(
                    "  Writing the remaining {} node(s) without vectors: structure and \
                     statistics still work, semantic search will miss them until the next run.",
                    to_embed - done
                );
                vectors.resize(to_embed, Vec::new());
            }
        }
    }

    let t2 = std::time::Instant::now();
    let node_rows = plan.finish(graph, vectors, &captured)?;

    let write_batch = 1000;
    let total = node_rows.len();
    if total == 0 {
        println!("{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}✓ skipped{C_RESET} (all {} already up to date)", nodes_count);
    } else {
        print!("{C_CYAN}▸{C_RESET} Writing nodes to Graph DB");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        for (i, batch) in node_rows.chunks(write_batch).enumerate() {
            store
                .upsert_nodes(batch)
                .await
                .map_err(|e| format!("upsert nodes: {}", e))?;
            let written = std::cmp::min((i + 1) * write_batch, total);
            let pct = written as f32 / total as f32 * 100.0;
            print!(
                "\r{C_CYAN}▸{C_RESET} Writing nodes: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
                pct, written, total
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        println!(
            "\r{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET} ({}/{} changed)",
            t2.elapsed(),
            total,
            nodes_count
        );
    }

    let t3 = std::time::Instant::now();
    print!("{C_CYAN}▸{C_RESET} Writing edges to Graph DB");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let edge_rows: Vec<storage::EdgeRow> = graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            storage::EdgeRow {
                id,
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect();

    let total_edges = edge_rows.len();
    for (i, batch) in edge_rows.chunks(write_batch).enumerate() {
        store
            .upsert_edges(batch)
            .await
            .map_err(|e| format!("upsert edges: {}", e))?;
        let written = std::cmp::min((i + 1) * write_batch, total_edges);
        let pct = written as f32 / total_edges as f32 * 100.0;
        print!(
            "\r{C_CYAN}▸{C_RESET} Writing edges: {C_YELLOW}{:>6.1}%{C_RESET} ({}/{})",
            pct, written, total_edges
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!(
        "\r{C_CYAN}▸{C_RESET} Writing edges: {C_GREEN}100.0% ✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t3.elapsed()
    );

    if prune {
        let t4 = std::time::Instant::now();
        let removed = storage::prune_to_graph(store, graph)
            .await
            .map_err(|e| format!("prune stale nodes: {}", e))?;
        if removed > 0 {
            println!(
                "{C_CYAN}▸{C_RESET} Pruning stale nodes: {C_GREEN}✓ removed {}{C_RESET} in {C_BOLD}{:?}{C_RESET}",
                removed,
                t4.elapsed()
            );
        }
    }

    store.ensure_query_indexes();

    // Stamp the model last, so a run that died mid-write doesn't claim its
    // vectors all came from this one. Skipped entirely when embedding
    // failed — or was deliberately skipped: the stamp says "these vectors
    // are current for this model", which would be a lie about rows that
    // have no vectors, and would stop the next run from re-embedding them.
    if embed_error.is_none() && vectors_skipped == 0 {
        store.record_ingest_model(&model);
    }

    Ok(IngestOutcome {
        nodes: nodes_count,
        edges: edges_count,
        embedding_error: embed_error,
        vectors_skipped,
    })
}

/// Open every spec, then dispatch to the right ingest path:
/// single-spec → progress-bar single ingest; multi-spec → fan-out
/// ingest (no per-store progress, but a one-line summary per backend).
pub(crate) async fn ingest_with_specs(
    specs: &[StoreSpec],
    embed_mode: &EmbedMode<'_>,
    graph: &GraphData,
    prune: bool,
    budget: &EmbedBudget,
) -> Result<IngestOutcome, String> {
    // An index written by an older ug can't be opened, and this is the
    // command whose whole job is to replace it — so clear it first rather
    // than failing with "run ug gen" from inside ug gen.
    for path in storage::store::reset_stale_format_stores(specs)
        .map_err(|e| format!("clearing out-of-date store: {}", e))?
    {
        eprintln!(
            "{C_CYAN}▸{C_RESET} Rebuilding {} — it was written by an older ug",
            path.display()
        );
    }
    let mut stores: Vec<Box<dyn KnowledgeStore>> = Vec::with_capacity(specs.len());
    for spec in specs {
        let store = match open_store(spec).await {
            Ok(s) => s,
            Err(StoreError::Corrupt(msg)) => {
                // The store's on-disk data can't be parsed (corrupt
                // manifest/WAL/record). This command's whole job is to
                // rebuild the store, so wipe it and try again — same deal
                // as `reset_stale_format_stores` above.
                if let StoreSpec::Overgraph { path, .. } = spec {
                    eprintln!(
                        "{C_CYAN}▸{C_RESET} Rebuilding {} — store on disk is corrupt: {}",
                        path.display(),
                        msg
                    );
                    std::fs::remove_dir_all(path)
                        .map_err(|e| format!("clearing corrupt store {}: {}", path.display(), e))?;
                    open_store(spec)
                        .await
                        .map_err(|e| format!("open {} store after reset: {}", spec.name(), e))?
                } else {
                    return Err(format!("open {} store: {}", spec.name(), msg));
                }
            }
            Err(e) => return Err(format!("open {} store: {}", spec.name(), e)),
        };
        stores.push(store);
    }
    if stores.len() == 1 {
        let store = stores.into_iter().next().unwrap();
        ingest_graph_with_progress(store.as_ref(), embed_mode, graph, prune, budget).await
    } else {
        let set = StoreSet::new(stores);
        set.validate_dims().map_err(|e| format!("dim mismatch across destinations: {}", e))?;
        ingest_graph_multi_with_progress(&set, embed_mode, graph, prune, budget).await
    }
}

/// Multi-destination ingest with a single progress line per stage
/// (text-build, embed, write) — per-backend progress isn't useful when
/// fan-out is parallel.
async fn ingest_graph_multi_with_progress(
    set: &StoreSet,
    embed_mode: &EmbedMode<'_>,
    graph: &GraphData,
    prune: bool,
    budget: &EmbedBudget,
) -> Result<IngestOutcome, String> {

    let nodes_count = graph.nodes.len();
    let edges_count = graph.edges.len();

    let t0 = std::time::Instant::now();
    let captured = storage::capture_for_graph(graph);
    let texts = storage::build_texts(graph, &captured, budget);
    let refs: Vec<&dyn KnowledgeStore> = set.stores.iter().map(|s| s.as_ref()).collect();
    storage::refresh_sparse_stats(&refs, &texts, &captured, graph);
    println!(
        "{C_CYAN}▸{C_RESET} Building node texts: {C_GREEN}done{C_RESET} ({}) in {C_BOLD}{:?}{C_RESET}",
        nodes_count,
        t0.elapsed()
    );

    // Vector reuse is planned against the first destination, but every row
    // is still written to every destination.
    let tp = std::time::Instant::now();
    let first = set
        .stores
        .first()
        .ok_or_else(|| "empty StoreSet".to_string())?;
    let model = embed_mode.model().to_string();
    let plan =
        storage::plan_incremental_ingest(first.as_ref(), graph, &texts, true, &captured, Some(&model))
        .await
        .map_err(|e| format!("reading existing nodes: {}", e))?;
    let to_embed = plan.to_embed.len();
    println!(
        "{C_CYAN}▸{C_RESET} Diffing against {}: {C_GREEN}done{C_RESET} in {C_BOLD}{:?}{C_RESET} — {} reusable, {} to embed",
        first.backend_name(),
        tp.elapsed(),
        plan.reusable.len(),
        to_embed
    );

    let t1 = std::time::Instant::now();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(to_embed);
    // Degrades exactly as the single-destination path does — see the
    // comment there for why a dead embedder must not cost the whole index.
    let mut embed_error: Option<String> = None;
    let mut vectors_skipped = 0usize;
    match embed_mode.embedder() {
        None => {
            vectors_skipped = to_embed;
            vectors.resize(to_embed, Vec::new());
        }
        Some(embedder) => {
            for chunk in plan.to_embed.chunks(embedder.config().batch_size) {
                let chunk_vec: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
                match embedder.embed(&chunk_vec).await {
                    Ok(chunk_vectors) => vectors.extend(chunk_vectors),
                    Err(e) => {
                        embed_error = Some(e.to_string());
                        break;
                    }
                }
            }
        }
    }
    if let Some(e) = &embed_error {
        println!(
            "{C_YELLOW}⚠{C_RESET} Embedding failed after {}/{} — {}",
            vectors.len(),
            to_embed,
            e
        );
        println!(
            "  Writing the remaining node(s) without vectors: structure and statistics \
             still work, semantic search will miss them until the next run."
        );
        vectors.resize(to_embed, Vec::new());
    }
    println!(
        "{C_CYAN}▸{C_RESET} Embedding: {C_GREEN}done{C_RESET} ({}) in {C_BOLD}{:?}{C_RESET}",
        to_embed,
        t1.elapsed()
    );

    let node_rows = plan.finish(graph, vectors, &captured)?;
    let edge_rows: Vec<storage::EdgeRow> = graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = format!("{:?}", e.edge_type);
            let id = format!("{}|{}|{}", e.source, edge_type, e.target);
            storage::EdgeRow {
                id,
                source: e.source.clone(),
                target: e.target.clone(),
                edge_type,
                properties: String::new(),
            }
        })
        .collect();

    let t2 = std::time::Instant::now();
    set.upsert_nodes(&node_rows)
        .await
        .map_err(|e| format!("upsert nodes (fan-out): {}", e))?;
    println!(
        "{C_CYAN}▸{C_RESET} Writing nodes: {C_GREEN}done{C_RESET} (×{} backends) in {C_BOLD}{:?}{C_RESET}",
        set.len(),
        t2.elapsed()
    );

    let t3 = std::time::Instant::now();
    set.upsert_edges(&edge_rows)
        .await
        .map_err(|e| format!("upsert edges (fan-out): {}", e))?;
    println!(
        "{C_CYAN}▸{C_RESET} Writing edges: {C_GREEN}done{C_RESET} (×{} backends) in {C_BOLD}{:?}{C_RESET}",
        set.len(),
        t3.elapsed()
    );

    if prune {
        let t4 = std::time::Instant::now();
        // Every destination, not just the one the plan was built against.
        let mut removed = 0usize;
        for store in &set.stores {
            removed += storage::prune_to_graph(store.as_ref(), graph)
                .await
                .map_err(|e| format!("prune stale nodes: {}", e))?;
        }
        if removed > 0 {
            println!(
                "{C_CYAN}▸{C_RESET} Pruning stale nodes: {C_GREEN}done{C_RESET} (removed {}, ×{} backends) in {C_BOLD}{:?}{C_RESET}",
                removed,
                set.len(),
                t4.elapsed()
            );
        }
    }

    for store in &set.stores {
        store.ensure_query_indexes();
        if embed_error.is_none() && vectors_skipped == 0 {
            store.record_ingest_model(&model);
        }
    }

    Ok(IngestOutcome {
        nodes: nodes_count,
        edges: edges_count,
        embedding_error: embed_error,
        vectors_skipped,
    })
}

pub(crate) fn run_ingest(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_ingest_help();
        return;
    }

    // The project this ingest targets: -n/--name, else the active project
    // (`ug active`), else the cwd basename. Resolved once so the input
    // graph and the output store land in the same project directory.
    let project_name = project::resolve_active_project_name(args, ".");
    // Announced before the store layer gets a look-in: `spec_args` below
    // rewrites the resolution into an explicit `-n`, which would make the
    // store layer's banner report a flag the user never typed.
    let project_dir = project::project_dir(&project_name);
    scope::announce(
        &project_name,
        &project_dir,
        &project::read_meta(&project_dir)
            .map(|m| m.repo_root)
            .unwrap_or_default(),
        scope::why_project(args, true),
    );

    let graph_file = flag_value(args, &["-i", "--input"]).unwrap_or_else(|| {
        project::project_dir(&project_name)
            .join("graph.json")
            .to_string_lossy()
            .into_owned()
    });

    // Pin the OverGraph destination to the same project when no explicit
    // -o was given, so an ingest never writes into a *different* project's
    // store than the one it read the graph from (the default_read_db_path
    // fallback would otherwise do exactly that). store_specs_from_args
    // prefers -n over the db dir flag, so appending it only when -o is
    // absent keeps the explicit -o override intact.
    //
    // `-o/--output` is the store dir on this write command, but on the
    // read commands it is the JSON output file — so store_specs_from_args
    // no longer treats it as a db path. It reads `--db` instead, which is
    // what ingest translates its destination flag to.
    let mut spec_args: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-o" | "--output" => spec_args.push("--db".to_string()),
            _ => spec_args.push(a.clone()),
        }
    }
    if flag_value(args, &["-o", "--output"]).is_none()
        && flag_value(args, &["-n", "--name"]).is_none()
    {
        spec_args.push("-n".to_string());
        spec_args.push(project_name.clone());
    }

    let graph_json = fs::read_to_string(&graph_file)
        .unwrap_or_else(|e| die(1, format!("failed to read {graph_file}: {e}")));
    let graph: GraphData = serde_json::from_str(&graph_json)
        .unwrap_or_else(|e| die(1, format!("failed to parse {graph_file}: {e}")));
    let mut embedder = embedder_from_args(args);
    let budget = budget_from_args(&embedder, args);
    let dim_was_explicit = flag_value(args, &["--embedding-dim"]).is_some();
    let rt = tokio_runtime();

    let start_total = std::time::Instant::now();

    rt.block_on(async {
        if !dim_was_explicit {
            match embedder.probe_dim().await {
                Ok(probed) if probed != embedder.config().dim => embedder.set_dim(probed),
                Ok(_) => {}
                Err(e) => {
                    eprintln!("embedder dim probe failed: {}", e);
                    return;
                }
            }
        }
        let dim = embedder.config().dim as u32;
        let specs = store_specs_from_args(&spec_args, dim);
        announce_destinations(&specs);
        let dest_label: Vec<String> = specs.iter().map(|s| s.name().to_string()).collect();
        // Opt-in here, unlike `ug gen`. `ug gen` owns a project and its
        // store should mirror the repo it indexed, so it prunes by
        // default. `ug ingest` just pushes a graph file at a destination,
        // and fanning several graphs into one store is a legitimate use —
        // pruning by default would make each ingest erase the last.
        let prune = has_flag(args, "--prune");
        match ingest_with_specs(&specs, &EmbedMode::Embed(&embedder), &graph, prune, &budget).await {
            Ok(out) => {
                println!("────────────────────────────────────────");
                println!(
                    "Ingested {} nodes, {} edges into [{}] in {:?}",
                    out.nodes,
                    out.edges,
                    dest_label.join(", "),
                    start_total.elapsed()
                );
                // This is the command that pays off `--no-embed`, so it is
                // also the one that clears the debt.
                if out.embedding_error.is_none() {
                    project::set_pending_vectors(
                        &project::project_dir(&project_name),
                        false,
                    );
                }
                if let Some(e) = &out.embedding_error {
                    eprintln!(
                        "{C_YELLOW}⚠{C_RESET} Written without vectors — embedding failed: {}",
                        e
                    );
                    eprintln!(
                        "  Structure and statistics are queryable; re-run this command once the embedder is up."
                    );
                }
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    });
}

fn print_ingest_help() {
    println!("  {C_CYAN}ug ingest{C_RESET}  {C_YELLOW}— embed graph nodes and write to one or more knowledge stores{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug ingest -n <project> [options]");
    println!();
    println!("  Re-embeds an already-indexed project by name — reads");
    println!("  {C_CYAN}~/.ug/<project>/graph.json{C_RESET} and writes to {C_CYAN}~/.ug/<project>/ugdb{C_RESET}.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>   Project name (default: cwd basename)");
    println!("  {C_CYAN}-i, --input{C_RESET} <file>  Graph JSON (default: ~/.ug/<name>/graph.json)");
    println!("  {C_CYAN}-o, --output{C_RESET} <dir>  OverGraph directory (default: ~/.ug/<name>/ugdb)");
    println!(
        "  {C_YELLOW}--prune{C_RESET}             Delete stored nodes absent from this graph"
    );
    println!(
        "                          (off by default: several graphs may share one store)"
    );
    println!();
    println!("{C_BOLD}Destinations (default: overgraph):{C_RESET}");
    println!("  {C_CYAN}--dest{C_RESET} <kind[,kind...]>   {C_BOLD}overgraph{C_RESET} | {C_BOLD}neo4j{C_RESET}. Comma-separated for fan-out ingest.");
    println!("                              Reads (semantic_search/search/traverse) accept");
    println!("                              exactly one --dest.");
    println!("  {C_CYAN}--neo4j-uri{C_RESET} <uri>      e.g. neo4j://localhost:7687 (env: UG_NEO4J_URI)");
    println!("  {C_CYAN}--neo4j-user{C_RESET} <user>    Default: neo4j (env: UG_NEO4J_USER)");
    println!("  {C_CYAN}--neo4j-password{C_RESET} <pw>  Required for --dest neo4j (env: UG_NEO4J_PASSWORD)");
    println!("  {C_CYAN}--neo4j-database{C_RESET} <db>  Default: neo4j (env: UG_NEO4J_DATABASE)");
    println!("  See {C_BOLD}docs/MULTI-STORAGE-DEST.md{C_RESET} for the GDS / APOC capability matrix and Neo4j schema.");
    println!();
    println!("{C_BOLD}Embedding (defaults to in-process, no service needed):{C_RESET}");
    println!("  {C_CYAN}--model{C_RESET} <name>      Model. For local: a fastembed alias (see below).");
    println!("                          For remote: the model field sent to /v1/embeddings.");
    println!("                          Default: bge-small-en-v1.5 (384d, ~130 MB download).");
    println!("  {C_CYAN}--base-url{C_RESET} <url>    {C_YELLOW}Switches to remote backend.{C_RESET} OpenAI-compatible");
    println!("                          /v1/embeddings endpoint (e.g. http://localhost:8000/v1).");
    println!("  {C_CYAN}--api-key{C_RESET} <key>     Bearer token for the remote endpoint (default: 1234).");
    println!("  {C_CYAN}--embedding-dim{C_RESET} <n>  Override vector dim. Auto-probed otherwise; persisted to");
    println!("                          <db>/ug-meta.json on first ingest.");
    println!();
    println!("{C_BOLD}Local model aliases:{C_RESET}");
    println!("  bge-small-en-v1.5 (default)  bge-base-en-v1.5  bge-large-en-v1.5");
    println!("  all-MiniLM-L6-v2  all-MiniLM-L12-v2  nomic-embed-text-v1.5");
    println!("  multilingual-e5-small/base/large  bge-small-zh-v1.5  jina-embeddings-v2-base-code");
    println!("  mxbai-embed-large-v1");
    println!("  Cache: $UG_MODEL_CACHE → $XDG_CACHE_HOME/ug/models → ~/Library/Caches/ug/models (macOS)");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} -n mb2                                     {C_YELLOW}# re-embed project by name{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET}                                      {C_YELLOW}# local, default model, ~/.ug/<cwd>{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --model nomic-embed-text-v1.5             {C_YELLOW}# local, larger model{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --base-url https://api.openai.com/v1 \\");
    println!("            --api-key $OPENAI_API_KEY --model text-embedding-3-small  {C_YELLOW}# remote{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --dest neo4j \\");
    println!("            --neo4j-uri neo4j://localhost:7687 --neo4j-user neo4j \\");
    println!("            --neo4j-password $NEO4J_PASSWORD                           {C_YELLOW}# Neo4j only{C_RESET}");
    println!("  {C_CYAN}ug ingest{C_RESET} --dest overgraph,neo4j \\");
    println!("            --neo4j-uri neo4j://localhost:7687 \\");
    println!("            --neo4j-user neo4j --neo4j-password $NEO4J_PASSWORD        {C_YELLOW}# fan-out{C_RESET}");
}
