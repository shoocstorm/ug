//! `ug doctor` — report the resolved configuration and where each value
//! came from, so the multi-tier fallback chain is inspectable instead of
//! implicit.

use ultragraph::storage::{
    DEFAULT_BASE_URL as DEFAULT_EMBED_BASE_URL, DEFAULT_MODEL as DEFAULT_EMBED_MODEL,
};
use ultragraph::{C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW};

use crate::{chat, config, project};

use super::args::{flag_value, has_flag};
use super::embed::PrefSource;

fn doctor_source_label(s: PrefSource) -> String {
    match s {
        PrefSource::Flag => "flag".to_string(),
        PrefSource::Config(key) => format!("config:{}", key),
        PrefSource::Default => "default".to_string(),
    }
}

/// `ug doctor` — print resolved project/db/embedder/chat configuration
/// and which tier (flag / env var / default) each value came from. Purely
/// read-only: resolves the same precedence chains the other commands use
/// but never builds an embedder/chat client or touches the network.
pub(crate) fn run_doctor(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_doctor_help();
        return;
    }
    println!("{C_BOLD}UltraGraph doctor{C_RESET}");
    println!();

    println!("{C_BOLD}Project{C_RESET}");
    let ug_home_from_env = std::env::var("UG_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some();
    println!(
        "  UG_HOME:      {C_CYAN}{}{C_RESET}  [{}]",
        project::ug_home().display(),
        if ug_home_from_env { "env:UG_HOME" } else { "default: ~/.ug" }
    );

    let name_flag = flag_value(args, &["-n", "--name"]);
    let project_name = name_flag
        .as_deref()
        .map(project::sanitize_name)
        .unwrap_or_else(|| project::derive_project_name("."));
    println!(
        "  project name: {C_CYAN}{}{C_RESET}  [{}]",
        project_name,
        if name_flag.is_some() { "flag:-n/--name" } else { "derived from cwd basename" }
    );

    let project_dir = project::project_dir(&project_name);
    let dir_status = if project_dir.exists() {
        format!("{C_GREEN}exists{C_RESET}")
    } else {
        format!("{C_YELLOW}not generated yet — run `ug gen`{C_RESET}")
    };
    println!("  project dir:  {} ({})", project_dir.display(), dir_status);

    println!(
        "  active proj:  {}  [ug active — default for `ug mcp` when no $UG_PROJECT / cwd match]",
        project::get_active_project().unwrap_or_else(|| "(none)".to_string())
    );

    let db_flag = flag_value(args, &["-d", "--db"]);
    let db_path = db_flag.clone().unwrap_or_else(project::default_read_db_path);
    let db_status = if std::path::Path::new(&db_path).exists() {
        format!("{C_GREEN}exists{C_RESET}")
    } else {
        format!("{C_YELLOW}missing — run `ug ingest`{C_RESET}")
    };
    println!(
        "  db path:      {} ({})  [{}]",
        db_path,
        db_status,
        if db_flag.is_some() { "flag:-d/--db" } else { "default: ~/.ug/<name>/ugdb → legacy ./.ug/ugdb" }
    );
    let cfg_path = config::config_path();
    println!(
        "  config file:  {} ({})",
        cfg_path.display(),
        if cfg_path.exists() {
            format!("{C_GREEN}exists{C_RESET} — manage with `ug config`")
        } else {
            format!("{C_YELLOW}none{C_RESET} — create with `ug config set <key> <value>`")
        }
    );
    println!();

    println!("{C_BOLD}Embeddings{C_RESET} (ingest / gen / search / serve)");
    let (base_url, base_src) =
        config::resolve_pref_cfg(flag_value(args, &["--base-url"]), "embed.base_url");
    let (_api_key, api_src) =
        config::resolve_pref_cfg(flag_value(args, &["--api-key"]), "embed.api_key");
    let (model, model_src) = config::resolve_pref_cfg(flag_value(args, &["--model"]), "embed.model");
    let backend = if base_url.is_some() {
        "remote (HTTP /v1/embeddings)"
    } else {
        "local (in-process ONNX)"
    };
    println!("  backend:      {C_CYAN}{}{C_RESET}  [{}]", backend, doctor_source_label(base_src));
    println!(
        "  model:        {}  [{}]",
        model.unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string()),
        doctor_source_label(model_src)
    );
    println!(
        "  base_url:     {}  [{}]",
        base_url.unwrap_or_else(|| format!("(n/a — {})", DEFAULT_EMBED_BASE_URL)),
        doctor_source_label(base_src)
    );
    println!("  api_key:      [{}]", doctor_source_label(api_src));
    println!();

    println!("{C_BOLD}Chat{C_RESET} (ug chat / POST /api/chat)");
    let chat_base_flag =
        flag_value(args, &["--chat-base-url"]).or_else(|| flag_value(args, &["--base-url"]));
    let (chat_base_url, chat_base_src) = config::resolve_pref_cfg(chat_base_flag, "chat.base_url");
    let chat_api_flag =
        flag_value(args, &["--chat-api-key"]).or_else(|| flag_value(args, &["--api-key"]));
    let (chat_api_key, chat_api_src) = config::resolve_pref_cfg(chat_api_flag, "chat.api_key");
    let (chat_model, chat_model_src) =
        config::resolve_pref_cfg(flag_value(args, &["--chat-model"]), "chat.model");
    let configured = chat_base_url.is_some() || chat_model.is_some();
    println!(
        "  base_url:     {}  [{}]",
        chat_base_url.unwrap_or_else(|| chat::DEFAULT_CHAT_BASE_URL.to_string()),
        doctor_source_label(chat_base_src)
    );
    println!(
        "  model:        {}  [{}]",
        chat_model.unwrap_or_else(|| chat::DEFAULT_CHAT_MODEL.to_string()),
        doctor_source_label(chat_model_src)
    );
    println!(
        "  api_key:      {}  [{}]",
        if chat_api_key.is_some() { "(set)" } else { "(default placeholder)" },
        doctor_source_label(chat_api_src)
    );
    println!(
        "  status:       {}",
        if configured {
            format!("{C_GREEN}configured{C_RESET} (base_url/model explicitly set)")
        } else {
            format!(
                "{C_YELLOW}not configured{C_RESET} — using sample defaults; run `ug config set chat.base_url <url>` (or pass --chat-base-url / $UG_CHAT_BASE_URL)"
            )
        }
    );
    println!();

    println!("{C_BOLD}Model cache{C_RESET} (ONNX weights for the local embedder)");
    println!("  {}", ultragraph::storage::embed::local::local_model_cache_dir().display());
    println!("  resolution: $UG_MODEL_CACHE → $XDG_CACHE_HOME/ug/models → platform cache dir → temp dir");
}

fn print_doctor_help() {
    println!("  {C_CYAN}ug doctor{C_RESET}  {C_YELLOW}— show resolved config and where each value came from{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug doctor [options]");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>  Project name to resolve (default: cwd basename)");
    println!("  {C_CYAN}-d, --db{C_RESET} <path>    DB path override to resolve against");
    println!("  {C_CYAN}--base-url/--api-key/--model{C_RESET}  Embedding flags, shown with resolution source");
    println!("  {C_CYAN}--chat-base-url/--chat-api-key/--chat-model{C_RESET}  Same, for chat");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug doctor{C_RESET}");
}
