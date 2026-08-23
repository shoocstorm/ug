//! Resolving `--dest` into `StoreSpec`s and opening the resulting stores.
//!
//! One command can fan out to several backends (`--dest overgraph,neo4j`);
//! read commands accept exactly one. `IngestOutcome` lives here because it
//! is what a write against those stores produces.

use std::path::{Path, PathBuf};

use ultragraph::storage::{self, KnowledgeStore, StoreSpec};
use ultragraph::{C_BOLD, C_CYAN, C_RESET};

use crate::project;

use super::args::flag_value;
use super::io::die;
use super::scope;

/// Parse `--dest <kind>[,<kind>...]` into one or more `StoreSpec`s.
/// Defaults to `overgraph` when no `--dest` is supplied so existing
/// invocations keep working unchanged. CLI flags override env vars
/// (`UG_DEST`, `UG_NEO4J_*`).
pub(crate) fn store_specs_from_args(args: &[String], embedding_dim: u32) -> Vec<StoreSpec> {
    let dest = flag_value(args, &["--dest"])
        .or_else(|| std::env::var("UG_DEST").ok())
        .unwrap_or_else(|| "overgraph".to_string());

    // The OverGraph dir path, and which rule produced it — the latter for the
    // scope banner below. Commands select a project by name via -n/--name,
    // resolved to ~/.ug/<name>/ugdb, which wins over the explicit --db path.
    // `-o` is reserved for the JSON output file on every read command, so it
    // is never a db dir here; callers that write a store (`gen`, `ingest`)
    // translate their destination flag to --db before handing args in.
    let (og_path, og_why) = if let Some(name) = flag_value(args, &["-n", "--name"]) {
        (
            project::project_dir(&project::sanitize_name(&name))
                .join("ugdb")
                .to_string_lossy()
                .into_owned(),
            "-n/--name",
        )
    } else if let Some(db) = flag_value(args, &["--db"]) {
        (db, "--db")
    } else {
        project::default_read_db_path_with_origin()
    };

    let neo4j_uri = flag_value(args, &["--neo4j-uri"]).or_else(|| std::env::var("UG_NEO4J_URI").ok());
    let neo4j_user = flag_value(args, &["--neo4j-user"])
        .or_else(|| std::env::var("UG_NEO4J_USER").ok())
        .unwrap_or_else(|| "neo4j".to_string());
    let neo4j_password = flag_value(args, &["--neo4j-password"])
        .or_else(|| std::env::var("UG_NEO4J_PASSWORD").ok())
        .unwrap_or_default();
    let neo4j_database = flag_value(args, &["--neo4j-database"])
        .or_else(|| std::env::var("UG_NEO4J_DATABASE").ok());

    let mut specs: Vec<StoreSpec> = Vec::new();
    for kind in dest.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match kind {
            "overgraph" | "og" => {
                // Announced here rather than at each of the ~10 call sites:
                // this is the one place that knows both the resolved path and
                // the rule that chose it. Only in the overgraph arm — a
                // `--dest neo4j` run never touches this path, and naming a
                // local project it isn't reading would be a lie.
                scope::announce_data("store", Path::new(&og_path), og_why);
                specs.push(StoreSpec::Overgraph {
                    path: PathBuf::from(&og_path),
                    embedding_dim,
                });
            }
            "neo4j" | "neo" => {
                let uri = neo4j_uri.clone().unwrap_or_else(|| {
                    eprintln!(
                        "Error: --dest neo4j requires --neo4j-uri (or UG_NEO4J_URI env var)"
                    );
                    std::process::exit(2);
                });
                if neo4j_password.is_empty() {
                    eprintln!(
                        "Error: --dest neo4j requires --neo4j-password (or UG_NEO4J_PASSWORD env var)"
                    );
                    std::process::exit(2);
                }
                specs.push(StoreSpec::Neo4j {
                    uri,
                    user: neo4j_user.clone(),
                    password: neo4j_password.clone(),
                    database: neo4j_database.clone(),
                    embedding_dim,
                });
            }
            other => {
                eprintln!(
                    "Error: unknown destination '{}' (expected: overgraph, neo4j)",
                    other
                );
                std::process::exit(2);
            }
        }
    }
    if specs.is_empty() {
        eprintln!("Error: --dest cannot be empty");
        std::process::exit(2);
    }
    specs
}

/// Read commands accept exactly one destination — the first parsed
/// spec wins, with a hard error on multi-spec inputs so users don't
/// accidentally fan out a query.
///
/// Also where the db-backed reads (`analyze`, `search`,
/// `traverse`, `chat`, `tour`) pick up the staleness warning. This function
/// rather than [`store_specs_from_args`] because that one is shared with the
/// commands that *write* the store — `ug gen` and `ug ingest` — and telling
/// them the index is behind the tree immediately before they refresh it would
/// be pure noise.
pub(crate) fn single_store_spec_from_args(args: &[String], embedding_dim: u32) -> StoreSpec {
    let specs = store_specs_from_args(args, embedding_dim);
    if specs.len() > 1 {
        eprintln!(
            "Error: this command accepts a single --dest, not a comma-separated list ({} given)",
            specs.len()
        );
        std::process::exit(2);
    }
    let spec = specs.into_iter().next().expect("at least one spec");
    // Only for a local project: the db dir's parent is the project dir that
    // holds project.json and graph.json. A `--dest neo4j` run, or a `--db`
    // pointed somewhere with no project.json beside it, has nothing to compare
    // against and `announce_staleness` returns without printing.
    if let StoreSpec::Overgraph { path, .. } = &spec {
        if let Some(dir) = path.parent() {
            scope::announce_staleness(dir);
        }
    }
    spec
}

/// What an ingest run actually produced.
///
/// Carries the degraded case explicitly rather than folding it into
/// `Err`: a run whose embedder died still wrote a complete structural
/// index, so it is neither a success nor a failure, and reporting it as
/// either misleads. The caller needs both facts to say something true.
pub(crate) struct IngestOutcome {
    pub(crate) nodes: usize,
    pub(crate) edges: usize,
    /// Set when nodes were written without vectors because embedding
    /// failed. Semantic search will miss them until the next run.
    pub(crate) embedding_error: Option<String>,
    /// How many nodes were written without vectors because the run did not
    /// ask for embedding (no `--with-embed`). Distinct from
    /// `embedding_error`: nothing went wrong, the vectors are simply owed —
    /// `ug ingest` backfills exactly these.
    pub(crate) vectors_skipped: usize,
}

/// Open a store, exiting cleanly on the one failure every user hits at
/// least once.
///
/// A store written by an older ug is an expected, actionable state after
/// an upgrade — not a bug. Reporting it through `panic!` buries a
/// perfectly good "run `ug gen`" message under a backtrace notice and
/// makes a routine migration look like a crash. Every other failure keeps
/// panicking, because it is one.
pub(crate) async fn open_store_or_exit(spec: &StoreSpec) -> Box<dyn KnowledgeStore> {
    match storage::open_store(spec).await {
        Ok(store) => store,
        Err(e @ storage::store::StoreError::StoreFormatMismatch { .. }) => {
            eprintln!("\n{C_BOLD}Index out of date{C_RESET}\n\n{}", e);
            std::process::exit(1);
        }
        Err(e) => die(1, format!("failed to open {} store: {}", spec.name(), e)),
    }
}

/// Banner indicating which backends a command is targeting.
pub(crate) fn announce_destinations(specs: &[StoreSpec]) {
    let names: Vec<&str> = specs.iter().map(|s| s.name()).collect();
    eprintln!(
        "{C_CYAN}▸{C_RESET} Destination(s): {C_BOLD}{}{C_RESET}",
        names.join(", ")
    );
}
