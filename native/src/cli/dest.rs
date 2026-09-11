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

    let neo4j_uri =
        flag_value(args, &["--neo4j-uri"]).or_else(|| std::env::var("UG_NEO4J_URI").ok());
    let neo4j_user = flag_value(args, &["--neo4j-user"])
        .or_else(|| std::env::var("UG_NEO4J_USER").ok())
        .unwrap_or_else(|| "neo4j".to_string());
    let neo4j_password = flag_value(args, &["--neo4j-password"])
        .or_else(|| std::env::var("UG_NEO4J_PASSWORD").ok())
        .unwrap_or_default();
    let neo4j_database =
        flag_value(args, &["--neo4j-database"]).or_else(|| std::env::var("UG_NEO4J_DATABASE").ok());

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
                    eprintln!("Error: --dest neo4j requires --neo4j-uri (or UG_NEO4J_URI env var)");
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

/// Warn the vector-ranked commands — `search`, `chat`, `tour` — when the
/// store they resolved to has no vectors in it.
///
/// Separate from [`single_store_spec_from_args`] on purpose: every DB-reading
/// command goes through that, but `analyze` and `traverse` rank with edges and
/// would only be nagged about something that does not affect their answer.
/// Same project-dir derivation as the staleness call above, and the same
/// silence for a non-local `--dest`, which has no `project.json` to read.
pub(crate) fn warn_if_no_vectors(spec: &StoreSpec) {
    if let StoreSpec::Overgraph { path, .. } = spec {
        if let Some(dir) = path.parent() {
            scope::announce_no_vectors(dir);
        }
    }
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
    /// How many of this graph's nodes have **no vector in the store** now
    /// that the run is over. Distinct from `embedding_error`: nothing went
    /// wrong, the vectors are simply owed — `ug ingest` backfills exactly
    /// these.
    ///
    /// This counts the *state of the index*, not what this run declined to
    /// do. A re-index over an unembedded store embeds nothing and writes
    /// nothing, and reporting that as "0 skipped" would let the caller
    /// announce an index that semantic search cannot use. See R4.2 in
    /// docs/dev/PERF-TUNING-JOURNEY.md.
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

#[cfg(test)]
mod tests {
    //! Which store a command writes to or reads from, and why.
    //!
    //! Three inputs can name it — `-n/--name`, `--db`, and the default — and
    //! they are tried in that order. Getting the order wrong does not fail:
    //! the command reads or writes a real store, just not the one the user
    //! named, and `ug gen -n other --db ./somewhere` would silently ingest
    //! into the wrong project.
    //!
    //! The error paths (`--dest neo4j` with no URI, an unknown destination,
    //! an empty `--dest`) all end in `std::process::exit`, so only the
    //! resolutions are reachable here.

    use super::*;
    use crate::project::UG_HOME_LOCK as ENV_GUARD;
    use tempfile::TempDir;

    const DIM: u32 = 384;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn og_path(spec: &StoreSpec) -> PathBuf {
        match spec {
            StoreSpec::Overgraph { path, .. } => path.clone(),
            other => panic!("expected an overgraph spec, got {other:?}"),
        }
    }

    /// Clear every env var these functions consult, so a developer's real
    /// shell cannot decide the result.
    fn clear_dest_env() {
        for k in [
            "UG_DEST",
            "UG_NEO4J_URI",
            "UG_NEO4J_USER",
            "UG_NEO4J_PASSWORD",
            "UG_NEO4J_DATABASE",
        ] {
            std::env::remove_var(k);
        }
    }

    #[tokio::test]
    async fn the_default_destination_is_a_local_overgraph_store() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();

        let specs = store_specs_from_args(&a(&[]), DIM);
        assert_eq!(specs.len(), 1);
        assert!(matches!(specs[0], StoreSpec::Overgraph { .. }));
        match &specs[0] {
            StoreSpec::Overgraph { embedding_dim, .. } => assert_eq!(*embedding_dim, DIM),
            _ => unreachable!(),
        }
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn a_project_name_resolves_to_that_projects_store() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();

        let path = og_path(&store_specs_from_args(&a(&["-n", "myrepo"]), DIM)[0]);
        assert!(
            path.ends_with("myrepo/ugdb"),
            "a named project points at its own store: {}",
            path.display()
        );
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn a_project_name_outranks_an_explicit_db_path() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();

        // Documented precedence, and the one worth pinning: given both, the
        // name wins. The opposite order would have `ug ingest -n a --db b`
        // write project a's graph into b's store.
        let path = og_path(&store_specs_from_args(&a(&["-n", "myrepo", "--db", "/tmp/other"]), DIM)[0]);
        assert!(
            path.ends_with("myrepo/ugdb"),
            "-n must outrank --db: {}",
            path.display()
        );
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn an_explicit_db_path_is_used_when_no_name_is_given() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();

        let path = og_path(&store_specs_from_args(&a(&["--db", "/tmp/chosen-store"]), DIM)[0]);
        assert_eq!(path, PathBuf::from("/tmp/chosen-store"));
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn a_project_name_is_sanitised_before_it_becomes_a_path() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();

        // The name reaches the filesystem, so a traversal in it must not.
        let path = og_path(&store_specs_from_args(&a(&["-n", "../escape"]), DIM)[0]);
        assert!(
            path.starts_with(tmp.path()),
            "a project name must not escape UG_HOME: {}",
            path.display()
        );
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn the_dest_env_var_stands_in_for_the_flag() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();
        std::env::set_var("UG_DEST", "og");

        // `og` is the short spelling, and the env var is the flagless way to
        // set it for a whole shell session.
        let specs = store_specs_from_args(&a(&[]), DIM);
        assert!(matches!(specs[0], StoreSpec::Overgraph { .. }));

        clear_dest_env();
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn an_explicit_dest_flag_beats_the_env_var() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();
        std::env::set_var("UG_DEST", "neo4j");

        // Without this, a `UG_DEST=neo4j` left in the environment would make
        // a plain `--dest overgraph` run fail on missing Neo4j credentials.
        let specs = store_specs_from_args(&a(&["--dest", "overgraph"]), DIM);
        assert!(matches!(specs[0], StoreSpec::Overgraph { .. }));

        clear_dest_env();
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn a_comma_separated_dest_fans_out_to_several_stores() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();

        // Writing both at once is the point of the list form; whitespace
        // around the entries is trimmed so a shell-quoted value still works.
        let specs = store_specs_from_args(
            &a(&[
                "--dest",
                "overgraph, neo4j",
                "--neo4j-uri",
                "bolt://localhost:7687",
                "--neo4j-password",
                "secret",
            ]),
            DIM,
        );
        assert_eq!(specs.len(), 2);
        assert!(matches!(specs[0], StoreSpec::Overgraph { .. }));
        match &specs[1] {
            StoreSpec::Neo4j { uri, user, database, .. } => {
                assert_eq!(uri, "bolt://localhost:7687");
                assert_eq!(user, "neo4j", "the user defaults rather than being required");
                assert_eq!(*database, None);
            }
            other => panic!("expected a neo4j spec, got {other:?}"),
        }
        std::env::remove_var("UG_HOME");
    }

    #[tokio::test]
    async fn neo4j_credentials_fall_back_to_the_environment() {
        let _guard = ENV_GUARD.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("UG_HOME", tmp.path());
        clear_dest_env();
        std::env::set_var("UG_NEO4J_URI", "bolt://env-host:7687");
        std::env::set_var("UG_NEO4J_USER", "env-user");
        std::env::set_var("UG_NEO4J_PASSWORD", "env-secret");
        std::env::set_var("UG_NEO4J_DATABASE", "env-db");

        // Credentials in the environment rather than in shell history is the
        // reason this fallback exists.
        let specs = store_specs_from_args(&a(&["--dest", "neo4j"]), DIM);
        match &specs[0] {
            StoreSpec::Neo4j { uri, user, password, database, .. } => {
                assert_eq!(uri, "bolt://env-host:7687");
                assert_eq!(user, "env-user");
                assert_eq!(password, "env-secret");
                assert_eq!(database.as_deref(), Some("env-db"));
            }
            other => panic!("expected a neo4j spec, got {other:?}"),
        }

        clear_dest_env();
        std::env::remove_var("UG_HOME");
    }
}
