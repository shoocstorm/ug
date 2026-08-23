//! HTTP server for the visualization UI plus a read-only graph API.
//! See `docs/WEB-SERVE.md` for the full design (Phases 1, 1.5, 2, 3).
//!
//! Split into `serve/`, one file per concern; this is the module root:
//! the entry point (`run_serve`), the shared state (`ServeState`) and the
//! per-file test modules. The sections, in request-flow order:
//!
//!   - `registry`      - project registry, store opening, context building
//!   - `encoding`      - pre-built gzip/brotli assets + negotiation
//!   - `snapshot`      - GraphSnapshot, adjacency, slim JSON index
//!   - `nidx`          - the binary slim index served at `/api/graph/nodes.bin`
//!   - `gen_jobs`      - background `ug gen` jobs (KB Manager wizard)
//!   - `host_guard`    - DNS-rebinding allowlist middleware
//!   - `watch`         - Phase 1.5 file-watch reload
//!   - `router`        - `build_router` + static handlers
//!   - `api`           - graph.json-backed read API (Phase 2 + capabilities)
//!   - `projects_api`  - projects/generate/ingest/browse endpoints
//!   - `db_api`        - Phase 3 DB-backed handlers (semantic/hybrid/file)
//!   - `chat_api`      - Phase 4 chat, config and guided-tour endpoints

pub(crate) mod api;
pub(crate) mod chat_api;
pub(crate) mod db_api;
pub(crate) mod encoding;
pub(crate) mod endpoints;
pub(crate) mod gen_jobs;
pub(crate) mod host_guard;
pub(crate) mod nidx;
pub(crate) mod projects_api;
pub(crate) mod registry;
pub(crate) mod router;
pub(crate) mod snapshot;
pub(crate) mod watch;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio::sync::Semaphore;

use crate::chat::ChatConfig;
use crate::cli::args::{flag_value, flag_value_or, has_flag};
use crate::cli::embed::{embedder_from_args, tokio_runtime};
use crate::cli::io::die;
use ultragraph::storage::Embedder;
use ultragraph::{C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW};

// Items the entry point, the router and the test modules reach across
// files for. One import list here rather than twelve deep `super::super::`
// paths there.
pub(crate) use api::{err_json, ok_json};
pub(crate) use encoding::EncodedAsset;
pub(crate) use gen_jobs::GenJobs;
pub(crate) use registry::{
    build_project_context, snapshot_cache_budget, ProjectContext, ProjectRegistry, ServeMode,
    ServeStores,
};
pub(crate) use router::build_router;

// Items `run_serve` itself uses from the split-out sections.
use chat_api::build_chat_default_from_args;
use projects_api::StalenessCache;
use registry::{activate_project, build_placeholder_context};
pub(crate) use snapshot::{load_snapshot, GraphModePolicy, GraphSnapshot};
use watch::spawn_watch;

#[cfg(test)]
pub(crate) use api::rank_search_matches;
#[cfg(test)]
pub(crate) use nidx::{build_binary_index, fnv1a32, front_code, NIDX_BLOCK, NIDX_MAGIC};
#[cfg(test)]
pub(crate) use snapshot::build_slim_index;

#[derive(Clone)]
pub(crate) struct ServeState {
    registry: Arc<ProjectRegistry>,
    html: Arc<EncodedAsset>,
    bundle: Arc<EncodedAsset>,
    cosmos_bundle: Arc<EncodedAsset>,
    favicon: Arc<EncodedAsset>,
    /// `None` when the embedder couldn't be constructed (e.g. missing endpoint).
    /// Phase 3 search routes need it; `/api/db/*` routes don't.
    embedder: Option<Arc<Embedder>>,
    /// Default chat config from CLI flags / env vars / `~/.ug/config.json`.
    /// The `/api/chat` route also accepts per-request overrides
    /// (chat_model, base_url, …) so the UI can flip models without
    /// restarting the server. `None` when no chat model is configured
    /// anywhere; routes return 503 in that case. Behind a lock because
    /// `POST /api/config` rebuilds it when the user saves settings.
    chat_default: Arc<RwLock<Option<ChatConfig>>>,
    /// The args `ug serve` was started with, kept so `/api/config` can
    /// report which values are pinned by CLI flags and rebuild
    /// `chat_default` with the same flag precedence after a save.
    serve_args: Arc<Vec<String>>,
    /// Process-wide cap on concurrent embedding calls. Cheap insurance against
    /// hammering the embedding endpoint when many search requests land at once.
    embed_lock: Arc<Semaphore>,
    /// Background `ug gen` jobs kicked off from the KB Manager wizard.
    gen_jobs: Arc<GenJobs>,
    /// Last computed `/api/projects/staleness` payload, reused for
    /// [`STALENESS_TTL`] so N open tabs cost one filesystem scan, not N.
    staleness: Arc<RwLock<Option<StalenessCache>>>,
    /// Whether the browser is handed the whole `graph.json` or the slim index.
    /// The *policy* is per-server (`--graph-mode`); the mode it resolves to is
    /// per-project, because size is a property of the graph.
    graph_mode: GraphModePolicy,
}

impl ServeState {
    fn active(&self) -> Arc<ProjectContext> {
        self.registry.active_ctx()
    }

    fn snapshot(&self) -> Arc<GraphSnapshot> {
        self.active()
            .graph
            .read()
            .expect("graph state poisoned")
            .clone()
    }

    fn stores(&self) -> Option<Arc<ServeStores>> {
        self.active().stores.clone()
    }

    fn repo_root(&self) -> PathBuf {
        self.active().repo_root.clone()
    }

    fn db_unavailable_reason(&self) -> Option<String> {
        self.active().db_unavailable_reason.clone()
    }
}

// ---------- Tracing ----------

/// Initialize a global `tracing` subscriber. No-ops if one is already
/// installed (so chained calls from `ug gen --serve` are safe).
///
/// Default filter: `info` for our crate + tower_http, `warn` for the
/// noisy hyper/reqwest internals. Override with `RUST_LOG=...`.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,ultragraph=info,tower_http=info,hyper=warn,h2=warn,reqwest=warn,rustls=warn",
        )
    });
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .try_init();
}

// ---------- Entry point ----------

/// Which project a `--project`-less `ug serve` starts on, and why.
///
/// Same precedence as `ug mcp install`: the project the user explicitly
/// pinned with `ug active` wins, then the one matching the cwd basename,
/// then the most recently indexed one (`names` comes from
/// `list_projects`, sorted most-recent first). Without the active step,
/// serving from a subdirectory (`.../ug/native` → no `native` project)
/// silently landed on whatever happened to be indexed last, so the UI
/// disagreed with what `ug active` reported.
///
/// Pure so the precedence can be tested without racing other tests over
/// `$UG_HOME`. `names` must be non-empty.
fn pick_initial_project(
    names: &[String],
    active: Option<String>,
    cwd_name: String,
) -> (String, &'static str) {
    // A stale marker can name a project that isn't listed; ignore it.
    if let Some(name) = active.filter(|a| names.iter().any(|n| n == a)) {
        return (name, "active project");
    }
    if let Some(name) = names.iter().find(|n| **n == cwd_name) {
        return (name.clone(), "matches the current directory");
    }
    (names[0].clone(), "most recently indexed project")
}

pub fn run_serve(args: &[String]) {
    init_tracing();

    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_serve_help();
        return;
    }

    // Explicit -i/--input pins the server to one graph file (the
    // pre-multi-project behavior). Without it the server roots at
    // ug_home(), discovers every generated project, and lets the UI
    // switch between them at runtime.
    let input_flag = flag_value(args, &["-i", "--input"]);

    let port: u16 = flag_value(args, &["-p", "--port"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let host = flag_value_or(args, &["--host"], "127.0.0.1");
    let watch = has_flag(args, "--watch");
    let no_db = has_flag(args, "--no-db");
    // `flag_value` takes `--flag value`, never `--flag=value`. Every other flag
    // here has the same shape, but silently ignoring the `=` form would leave
    // the server in the opposite mode to the one asked for with nothing said —
    // so it is named as an error rather than skipped.
    if let Some(bad) = args.iter().find(|a| a.starts_with("--graph-mode=")) {
        die(
            2,
            format!("use `--graph-mode <value>`, not `{bad}` — this CLI takes flag values as a separate argument"),
        );
    }
    let graph_mode = match flag_value(args, &["--graph-mode"]) {
        Some(raw) => match GraphModePolicy::parse(&raw) {
            Some(p) => p,
            None => die(
                2,
                format!("--graph-mode must be auto, local or server (got {raw:?})"),
            ),
        },
        None => GraphModePolicy::Auto,
    };

    enum Startup {
        Single { graph_file: String },
        Multi { initial: String },
    }

    let startup = match input_flag {
        Some(graph_file) => Startup::Single { graph_file },
        None => {
            let projects = crate::project::list_projects();
            if projects.is_empty() {
                // Legacy repo-local layout: keep `ug serve` working in
                // repos generated before the ~/.ug move.
                if std::path::Path::new(".ug/graph.json").exists() {
                    tracing::warn!(
                        home = %crate::project::ug_home().display(),
                        "no projects found; serving legacy ./.ug/graph.json — run `ug gen` to migrate to ~/.ug"
                    );
                    Startup::Single {
                        graph_file: ".ug/graph.json".to_string(),
                    }
                } else {
                    // No projects and no legacy graph: start anyway with
                    // an empty placeholder project. The KB Manager screen
                    // (always shown first when `/api/projects` reports
                    // zero projects) presents the "generate from scratch"
                    // wizard; an empty sentinel `initial` name signals
                    // that below.
                    tracing::info!(
                        home = %crate::project::ug_home().display(),
                        "no projects found — starting in multi-project mode; use the KB Manager UI to generate one"
                    );
                    Startup::Multi {
                        initial: String::new(),
                    }
                }
            } else {
                let requested =
                    flag_value(args, &["--project"]).map(|n| crate::project::sanitize_name(&n));
                let initial = match requested {
                    Some(r) => {
                        if !projects.iter().any(|(_, m)| m.name == r) {
                            let names: Vec<&str> =
                                projects.iter().map(|(_, m)| m.name.as_str()).collect();
                            tracing::error!(
                                requested = %r,
                                available = %names.join(", "),
                                "--project not found"
                            );
                            std::process::exit(1);
                        }
                        r
                    }
                    None => {
                        let names: Vec<String> =
                            projects.iter().map(|(_, m)| m.name.clone()).collect();
                        let (initial, why) = pick_initial_project(
                            &names,
                            crate::project::get_active_project(),
                            crate::project::derive_project_name("."),
                        );
                        tracing::info!(project = %initial, reason = why, "initial project");
                        initial
                    }
                };
                Startup::Multi { initial }
            }
        }
    };

    let html = Arc::new(EncodedAsset::new(
        crate::assets::VIS_HTML.as_bytes().to_vec(),
        "text/html; charset=utf-8",
    ));
    let bundle = Arc::new(EncodedAsset::new(
        crate::assets::VIS_THREEJS_BUNDLE.to_vec(),
        "application/javascript; charset=utf-8",
    ));
    let cosmos_bundle = Arc::new(EncodedAsset::new(
        crate::assets::VIS_COSMOS_BUNDLE.to_vec(),
        "application/javascript; charset=utf-8",
    ));
    let favicon = Arc::new(EncodedAsset::new(
        crate::assets::VIS_FAVICON.to_vec(),
        "image/svg+xml",
    ));

    // Build embedder up-front (sync) — Phase 3 search routes need it.
    // Failure here is non-fatal: keep the rest of the server up and let
    // /api/search/* return 503.
    let (embedder_arc, embedder_err): (Option<Arc<Embedder>>, Option<String>) = if no_db {
        (None, Some("started with --no-db".to_string()))
    } else {
        match embedder_from_args(args) {
            e => (Some(Arc::new(e)), None),
        }
    };
    // `embedder_from_args` panics on construction failure today, so we don't
    // get a graceful error path for "endpoint config bogus" yet — but the
    // shape above is what we'd plug into if it returns Result later.
    let _ = embedder_err;

    let addr: SocketAddr = match format!("{}:{}", host, port).parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(host = %host, port, error = %e, "invalid bind address");
            std::process::exit(1);
        }
    };

    let rt = tokio_runtime();
    rt.block_on(async move {
        let t0 = std::time::Instant::now();

        let (mode, registry_seed) = match &startup {
            Startup::Single { .. } => (ServeMode::Single, None),
            Startup::Multi { initial } => (ServeMode::Multi, Some(initial.clone())),
        };
        let registry = Arc::new(ProjectRegistry {
            mode,
            no_db,
            active: RwLock::new(String::new()),
            loaded: RwLock::new(HashMap::new()),
            lru: RwLock::new(Vec::new()),
            cache_budget: snapshot_cache_budget(),
        });

        let initial_ctx = match &startup {
            Startup::Single { graph_file } => {
                let graph_path = std::fs::canonicalize(graph_file).unwrap_or_else(|e| {
                    tracing::error!(path = %graph_file, error = %e, "failed to resolve graph path");
                    std::process::exit(1);
                });
                // Default db: the graph file's sibling ugdb — keeps
                // `-i .ug/graph.json` finding `.ug/ugdb` like before.
                let db_path_raw = flag_value(args, &["-d", "--db"]).unwrap_or_else(|| {
                    graph_path
                        .parent()
                        .map(|p| p.join("ugdb"))
                        .unwrap_or_else(|| PathBuf::from("ugdb"))
                        .to_string_lossy()
                        .into_owned()
                });
                let db_path = std::fs::canonicalize(&db_path_raw).unwrap_or_else(|_| {
                    std::env::current_dir()
                        .map(|c| c.join(&db_path_raw))
                        .unwrap_or_else(|_| PathBuf::from(&db_path_raw))
                });
                let repo_root_override = flag_value(args, &["--repo-root"])
                    .map(PathBuf::from)
                    .map(|raw| {
                        // A repo root that no longer exists must not stop the
                        // server — the index serves content on its own, and
                        // every consumer of repo_root already tolerates a
                        // missing path. Canonicalize when possible so relative
                        // roots resolve; otherwise keep the raw path.
                        std::fs::canonicalize(&raw).unwrap_or(raw)
                    });
                let name = graph_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("single")
                    .to_string();
                let ctx =
                    build_project_context(&name, graph_path, db_path, repo_root_override, no_db)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::error!(error = %e, "failed to load graph snapshot");
                            std::process::exit(1);
                        });
                registry.insert_and_activate(ctx.clone());
                ctx
            }
            Startup::Multi { .. } => {
                let initial = registry_seed.expect("multi startup has initial project");
                if initial.is_empty() {
                    build_placeholder_context(&registry)
                } else {
                    activate_project(&registry, &initial).await.unwrap_or_else(|e| {
                        tracing::error!(project = %initial, error = %e, "failed to load initial project");
                        std::process::exit(1);
                    })
                }
            }
        };

        let (identity_size, nodes, edges) = {
            let snap = initial_ctx.graph.read().expect("graph state poisoned");
            (
                snap.encoded.identity.len(),
                snap.parsed.nodes.len(),
                snap.parsed.edges.len(),
            )
        };

        let chat_default = build_chat_default_from_args(args);
        if let Some(cfg) = chat_default.as_ref() {
            tracing::info!(
                model = %cfg.model,
                base_url = %cfg.base_url,
                "chat endpoint configured"
            );
        } else {
            tracing::info!("chat endpoint not configured (/api/chat will return 503)");
        }

        let state = ServeState {
            registry: registry.clone(),
            html,
            bundle,
            cosmos_bundle,
            favicon,
            embedder: embedder_arc,
            chat_default: Arc::new(RwLock::new(chat_default)),
            serve_args: Arc::new(args.to_vec()),
            embed_lock: Arc::new(Semaphore::new(4)),
            gen_jobs: Arc::new(GenJobs::new()),
            staleness: Arc::new(RwLock::new(None)),
            graph_mode,
        };

        let app = build_router(state.clone());

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(addr = %addr, error = %e, "bind failed");
                std::process::exit(1);
            }
        };

        let db_api_enabled = state.stores().is_some() && state.embedder.is_some();
        let db_unavailable = state.db_unavailable_reason();
        tracing::info!(
            mode = match mode { ServeMode::Single => "single", ServeMode::Multi => "multi" },
            project = %initial_ctx.name,
            graph = %initial_ctx.graph_path.display(),
            nodes,
            edges,
            identity_bytes = identity_size,
            // No gzip/brotli sizes here any more: nothing has been compressed
            // by the time the server is ready, which is the point of P3.1.
            startup_secs = t0.elapsed().as_secs_f32(),
            addr = %addr,
            db_api = db_api_enabled,
            db_unavailable_reason = db_unavailable.as_deref().unwrap_or(""),
            watch,
            "ug serve ready"
        );
        if !db_api_enabled {
            tracing::warn!(
                reason = db_unavailable.as_deref().unwrap_or("DB not opened"),
                "Phase 3 routes will 503"
            );
        }
        if watch {
            spawn_watch(state.clone());
        }

        tracing::info!("Open http://{}\n", addr);
        tracing::warn!(
            "ug serve is for local use: it binds to loopback by default and has \
             no authentication. Do not expose it to a network or run it on a \
             production server without a secured reverse proxy (auth + TLS + \
             network policy) in front."
        );

        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "server crashed");
            std::process::exit(1);
        }
    });
}

/// Extra hostnames this server will answer to, from `UG_ALLOWED_HOSTS`
/// (comma-separated). Needed only when `ug serve` sits behind a reverse
/// proxy that forwards a real domain in `Host`; the loopback and
pub fn print_serve_help() {
    println!("  {C_CYAN}ug serve{C_RESET}  {C_YELLOW}— serve visualization + graph API{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug serve [options]");
    println!();
    println!("  Without {C_CYAN}-i{C_RESET}, serves {C_BOLD}every{C_RESET} project under ~/.ug (or $UG_HOME) in");
    println!("  multi-project mode — the UI gets a project switcher, and");
    println!("  {C_CYAN}POST /api/projects/select{C_RESET} swaps the active project at runtime.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!(
        "  {C_CYAN}-i, --input{C_RESET} <file>   Graph JSON to serve (forces single-project mode)"
    );
    println!(
        "  {C_CYAN}--project{C_RESET} <name>     Initially active project in multi-project mode"
    );
    println!("                       (default: `ug active`, else cwd basename, else most recently generated)");
    println!(
        "  {C_CYAN}-d, --db{C_RESET} <path>      OverGraph DB for /api/db + /api/search routes"
    );
    println!("                       (default: per-project ugdb, or the graph file's sibling ugdb with -i)");
    println!("  {C_YELLOW}--no-db{C_RESET}            Don't open DB; routes return 503");
    println!("  {C_CYAN}-p, --port{C_RESET} <n>       TCP port (default: 8080)");
    println!("  {C_CYAN}--host{C_RESET} <addr>        Bind address (default: 127.0.0.1)");
    println!("  {C_GREEN}--watch{C_RESET}             Reload graph file when its mtime changes");
    println!(
        "  {C_CYAN}--graph-mode{C_RESET} <mode>  How the browser gets the graph: auto|local|server"
    );
    println!("                        auto = whole file under graph.server_mode_bytes");
    println!("                        (default 50 MB), slim index + server API above");
    println!("                       (default: auto — `server` at or above 50 MB of graph.json,");
    println!("                        where the page asks this server instead of downloading it)");
    println!(
        "  {C_CYAN}--repo-root{C_RESET} <path>   Repo root for hybrid-search snippet resolution"
    );
    println!(
        "  {C_CYAN}--base-url{C_RESET} <url>      Embedding/chat base URL (OpenAI-compatible)"
    );
    println!("  {C_CYAN}--api-key{C_RESET} <key>       Embedding/chat API key");
    println!(
        "  {C_CYAN}--model{C_RESET} <name>        Embedding model (fastembed alias for local)"
    );
    println!();
    println!("{C_BOLD}Security:{C_RESET}");
    println!("  {C_YELLOW}ug serve{C_RESET} is intended for {C_BOLD}local{C_RESET} use: it binds to 127.0.0.1 by default");
    println!(
        "  and the HTTP API has {C_BOLD}no authentication{C_RESET}. Do not run it on a production"
    );
    println!("  server or expose it to a network without a properly secured reverse proxy");
    println!("  (authentication + TLS + network policy) in front of it.");
    println!();
    println!("{C_BOLD}Chat (POST /api/chat):{C_RESET}");
    println!("  {C_CYAN}--chat-model{C_RESET} <name>     Chat completion model — required to enable /api/chat");
    println!("  {C_CYAN}--chat-base-url{C_RESET} <url>   Override base URL for chat (defaults to --base-url)");
    println!("  {C_CYAN}--chat-api-key{C_RESET} <key>    Override API key for chat (defaults to --api-key)");
    println!(
        "  {C_CYAN}--temperature{C_RESET} <f>       Default sampling temperature (default: 0.2)"
    );
    println!(
        "  {C_CYAN}--max-tokens{C_RESET} <n>        Default max completion tokens (default: 1024)"
    );
    println!(
        "  {C_CYAN}--chat-timeout{C_RESET} <secs>   HTTP timeout for chat calls (default: 180)"
    );
    println!("    Env fallbacks: UG_CHAT_MODEL, UG_CHAT_BASE_URL, UG_CHAT_API_KEY");
    println!();
    println!("{C_BOLD}API Endpoints:{C_RESET}");
    println!("  {C_CYAN}GET{C_RESET}  /api/projects              list projects + active selection");
    println!("  {C_CYAN}POST{C_RESET} /api/projects/select       body: {{ name }} — switch active project");
    println!("  {C_CYAN}POST{C_RESET} /api/projects/delete       body: {{ name }} — delete a project's data directory");
    println!(
        "  {C_CYAN}GET{C_RESET}  /api/graph/{{stats, node/<id>, search?q=&types=, bfs/<id>?k=,"
    );
    println!("           path?source=&target=, filter?types=, centrality, cycles}}");
    println!("  {C_CYAN}GET{C_RESET}  /api/db/{{node/<id>, traverse/<id>?k=&dir=&types=}}");
    println!("  {C_CYAN}POST{C_RESET} /api/search/{{semantic, hybrid}}  body: JSON");
    println!("  {C_CYAN}POST{C_RESET} /api/chat  body: {{ query, history?, k?, hops?, chat_model?, ... }}");
    println!(
        "  {C_CYAN}POST{C_RESET} /api/tour  body: {{ query, k?, hops?, max_stops?, no_llm?, ... }}"
    );
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET}                          {C_YELLOW}# all projects under ~/.ug{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} --project myrepo --watch");
    println!("  {C_CYAN}ug serve{C_RESET} -i path/to/graph.json -p 8080   {C_YELLOW}# single-project mode{C_RESET}");
    println!("  {C_CYAN}ug serve{C_RESET} \\");
    println!("           --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("           --chat-model Qwen3.6-35B-A3B-MLX-8bit");
}

#[cfg(test)]
mod router_tests;

#[cfg(test)]
mod nidx_tests;

#[cfg(test)]
mod tests {
    use super::chat_api::{resolve_chat_endpoint, ChatCfgError};
    use super::db_api::{slice_file_text, stored_source_for_file};
    use super::host_guard::{host_label, is_allowed_host};
    use super::pick_initial_project;
    use crate::chat::ChatConfig;

    fn cfg(base_url: &str, api_key: &str) -> ChatConfig {
        ChatConfig {
            base_url: base_url.into(),
            api_key: api_key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn request_supplied_endpoint_never_inherits_the_stored_key() {
        let default = cfg("https://api.openai.com/v1", "sk-real-secret");

        // The attack: point the endpoint elsewhere, send no key, collect the
        // server's key from the Authorization header. The key must not follow.
        let (url, key) =
            resolve_chat_endpoint(&default, Some("https://evil.tld/v1"), None).expect("allowed");
        assert_eq!(url, "https://evil.tld/v1");
        assert_eq!(key, "", "stored key leaked to a request-supplied endpoint");

        // A caller bringing its own key is fine — that's the legitimate
        // "flip to another provider" flow.
        let (_, key) =
            resolve_chat_endpoint(&default, Some("https://other.tld/v1"), Some("sk-theirs"))
                .expect("allowed");
        assert_eq!(key, "sk-theirs");

        // Same origin as the configured endpoint is not a redirection, so the
        // stored key still applies (the UI echoes base_url back in the body).
        let (_, key) = resolve_chat_endpoint(&default, Some("https://api.openai.com/v1/"), None)
            .expect("allowed");
        assert_eq!(key, "sk-real-secret");

        // No override at all: unchanged behaviour.
        let (url, key) = resolve_chat_endpoint(&default, None, None).expect("allowed");
        assert_eq!(
            (url.as_str(), key.as_str()),
            ("https://api.openai.com/v1", "sk-real-secret")
        );
    }

    #[test]
    fn chat_endpoint_rejects_metadata_and_non_http_targets() {
        let default = cfg("https://api.openai.com/v1", "sk-real-secret");
        for bad in [
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(
                matches!(
                    resolve_chat_endpoint(&default, Some(bad), None),
                    Err(ChatCfgError::Invalid(_))
                ),
                "{bad} should have been rejected"
            );
        }

        // A local model server is the common legitimate case and stays open.
        assert!(resolve_chat_endpoint(&default, Some("http://127.0.0.1:11434/v1"), None).is_ok());
    }

    #[test]
    fn host_guard_accepts_local_names_and_rejects_domains() {
        assert_eq!(host_label("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_label("[::1]:8080"), "::1");
        assert_eq!(host_label("::1"), "::1");
        assert_eq!(host_label("Evil.TLD:80"), "evil.tld");

        for ok in [
            "localhost",
            "localhost:8080",
            "127.0.0.1:8080",
            "[::1]:8080",
            "192.168.1.9:8080",
        ] {
            assert!(is_allowed_host(ok), "{ok} should be allowed");
        }
        // The rebinding case: attacker's domain, currently resolving to us.
        for bad in ["evil.tld", "evil.tld:8080", "ug.attacker.example", ""] {
            assert!(!is_allowed_host(bad), "{bad} should be rejected");
        }
    }
    use tempfile::TempDir;
    use ultragraph::storage::db::{Db, NodeRow};
    use ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM;
    use ultragraph::types::{GraphData, GraphNode, GraphNodeType};

    #[test]
    fn initial_project_prefers_the_active_one() {
        // list_projects order: most recently indexed first.
        let names: Vec<String> = ["dlab", "Ultra-Graph", "ug"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // `ug active Ultra-Graph` wins even from a subdir whose basename
        // (`native`) isn't a project, and even from the repo root, whose
        // basename (`ug`) is a different project.
        let (name, why) = pick_initial_project(&names, Some("Ultra-Graph".into()), "native".into());
        assert_eq!((name.as_str(), why), ("Ultra-Graph", "active project"));
        let (name, _) = pick_initial_project(&names, Some("Ultra-Graph".into()), "ug".into());
        assert_eq!(name, "Ultra-Graph");

        // No active project: the cwd match, else the most recent.
        let (name, why) = pick_initial_project(&names, None, "ug".into());
        assert_eq!(
            (name.as_str(), why),
            ("ug", "matches the current directory")
        );
        let (name, why) = pick_initial_project(&names, None, "native".into());
        assert_eq!(
            (name.as_str(), why),
            ("dlab", "most recently indexed project")
        );

        // A stale marker naming an unlisted project falls through.
        let (name, why) = pick_initial_project(&names, Some("deleted".into()), "native".into());
        assert_eq!(
            (name.as_str(), why),
            ("dlab", "most recently indexed project")
        );
    }

    fn graph_node(id: &str, file: &str, start: Option<u32>, end: Option<u32>) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: id.into(),
            node_type: GraphNodeType::Function,
            file: Some(file.into()),
            start_line: start,
            end_line: end,
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            ..Default::default()
        }
    }

    fn file_node(id: &str, file: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: file.into(),
            node_type: GraphNodeType::File,
            file: Some(file.into()),
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            ..Default::default()
        }
    }

    fn row(id: &str, node_type: &str, file: &str, start: u32, end: u32, code: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            name: id.into(),
            node_type: node_type.into(),
            description: String::new(),
            file: file.into(),
            start_line: start,
            end_line: end,
            last_update_at: 0,
            node_text: String::new(),
            vector: vec![0.0; DEFAULT_EMBEDDING_DIM],
            code: code.into(),
            file_hash: String::new(),
            facts: Default::default(),
        }
    }

    /// `/api/file`'s repo-independent fallback: with the repo path gone, the
    /// store's captured source is what the Preview tab serves. The resolver
    /// must find the exact span first, fall back to the whole-file capture,
    /// and skip rows with no captured code.
    #[tokio::test]
    async fn stored_file_fallback_resolves_span_then_whole_file() {
        let tmp = TempDir::new().unwrap();
        let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

        let sym = row(
            "function:src/a.rs:2:sym",
            "Function",
            "src/a.rs",
            2,
            3,
            "two\nthree\n",
        );
        let whole = row(
            "file:src/a.rs",
            "File",
            "src/a.rs",
            0,
            0,
            "one\ntwo\nthree\nfour\n",
        );
        db.upsert_nodes(&[sym.clone(), whole.clone()])
            .await
            .unwrap();

        let graph = GraphData {
            nodes: vec![
                graph_node("function:src/a.rs:2:sym", "src/a.rs", Some(2), Some(3)),
                file_node("file:src/a.rs", "src/a.rs"),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };

        // Exact span match → the symbol's captured slice, flagged as sliced.
        let got = stored_source_for_file(&graph, &db, "src/a.rs", Some(2), Some(3))
            .await
            .unwrap();
        assert_eq!((got.0.as_str(), got.1), ("two\nthree\n", true));

        // Whole-file request → the file node's captured content.
        let got = stored_source_for_file(&graph, &db, "src/a.rs", None, None)
            .await
            .unwrap();
        assert_eq!((got.0.as_str(), got.1), ("one\ntwo\nthree\nfour\n", false));

        // Span with no exact match falls back to the whole-file capture, and
        // reports it as unsliced.
        let got = stored_source_for_file(&graph, &db, "src/a.rs", Some(1), Some(1))
            .await
            .unwrap();
        assert_eq!((got.0.as_str(), got.1), ("one\ntwo\nthree\nfour\n", false));

        // Unknown file → None.
        assert_eq!(
            stored_source_for_file(&graph, &db, "src/absent.rs", None, None).await,
            None
        );

        // A row whose capture is empty (pre-column, or a binary file) is
        // skipped rather than served as blank.
        db.upsert_nodes(&[row(
            "function:src/b.rs:5:empty",
            "Function",
            "src/b.rs",
            5,
            9,
            "",
        )])
        .await
        .unwrap();
        let graph2 = GraphData {
            nodes: vec![graph_node(
                "function:src/b.rs:5:empty",
                "src/b.rs",
                Some(5),
                Some(9),
            )],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        assert_eq!(
            stored_source_for_file(&graph2, &db, "src/b.rs", Some(5), Some(9)).await,
            None
        );
    }

    /// A range request must return the same lines whether it was answered
    /// from the repo or from the index — the whole-file capture is cut down
    /// to the span rather than served entire, and `total_lines` stays the
    /// file's length either way.
    #[test]
    fn a_range_is_cut_out_of_a_whole_file_the_same_way_from_either_source() {
        let text = "one\ntwo\nthree\nfour\n".to_string();

        let (body, sliced, total) = slice_file_text(text.clone(), Some(2), Some(3));
        assert_eq!((body.as_str(), sliced, total), ("two\nthree", true, 4));

        // No range → the whole file, untouched.
        let (body, sliced, total) = slice_file_text(text.clone(), None, None);
        assert_eq!((body.as_str(), sliced, total), (text.as_str(), false, 4));

        // `end` omitted means the single `start` line.
        let (body, _, _) = slice_file_text(text.clone(), Some(4), None);
        assert_eq!(body, "four");

        // Out-of-range bounds clamp rather than panic — an index can lag the
        // file it describes.
        let (body, _, total) = slice_file_text(text.clone(), Some(3), Some(99));
        assert_eq!((body.as_str(), total), ("three\nfour", 4));
        let (body, _, _) = slice_file_text(text, Some(99), Some(120));
        assert_eq!(body, "");
    }
}
