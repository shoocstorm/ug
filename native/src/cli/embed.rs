//! Building the embedder (and the tokio runtime it needs) from CLI flags,
//! env vars, and `~/.ug/config.json`, plus the banners that make the
//! resolved backend and token budget visible before work starts.

use ultragraph::limits::{BudgetSource, EmbedBudget};
use ultragraph::storage::{Embedder, EmbedderConfig};
use ultragraph::{C_BOLD, C_CYAN, C_GREEN, C_RESET, C_YELLOW};

use crate::config;

use super::args::flag_value;
use super::io::die;

/// Where a resolved config value came from: an explicit CLI flag, a
/// named env var, a key persisted in `~/.ug/config.json` (`ug config
/// set`), or none of those (caller applies its own default). `ug
/// doctor` reports this so the multi-tier fallback chain is inspectable
/// instead of implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefSource {
    Flag,
    Config(&'static str),
    Default,
}

pub(crate) fn embedder_from_args(args: &[String]) -> Embedder {
    match try_embedder_from_args(args) {
        Some(e) => e,
        None => die(1, "failed to build embedder (set --base-url, --api-key and/or --model, or run `ug config set embed.base_url …`)"),
    }
}

/// Build the embedder the CLI flags / config resolve to, or `None` when
/// they resolve to nothing usable.
///
/// `embedder_from_args` treats failure as fatal — the commands that
/// *need* embeddings (ingest, gen) have no honest fallback. Search has
/// one ([`crate::storage::name_search`]), so it calls this instead and
/// falls back to name matching when the answer is `None`.
pub(crate) fn try_embedder_from_args(args: &[String]) -> Option<Embedder> {
    let (dim_raw, _) = config::resolve_pref_cfg(flag_value(args, &["--embedding-dim"]), "embed.dim");
    let dim = dim_raw.and_then(|s| s.parse().ok());
    let (base_url, _) = config::resolve_pref_cfg(flag_value(args, &["--base-url"]), "embed.base_url");
    // Presence of --base-url (or $UG_EMBED_BASE_URL, or a persisted
    // embed.base_url) is the single switch between in-process (default)
    // and the legacy HTTP backend. --model applies to both: for local it
    // picks a fastembed catalog entry; for remote it's the model field
    // sent in the /v1/embeddings request.
    let want_remote = base_url.is_some();
    let (api_key, _) = config::resolve_pref_cfg(flag_value(args, &["--api-key"]), "embed.api_key");
    let (model, _) = config::resolve_pref_cfg(flag_value(args, &["--model"]), "embed.model");
    let cfg = EmbedderConfig::with_overrides(base_url, api_key, model, dim, None, None);
    let result = if want_remote {
        Embedder::remote(cfg)
    } else {
        Embedder::local(cfg)
    };
    let embedder = match result {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: embedder unavailable, falling back to name search: {}", e);
            return None;
        }
    };
    announce_embedder(&embedder, dim.is_some());
    Some(embedder)
}

/// Resolve how much of each node's description may be embedded, and say so.
///
/// The number comes from the model's token window unless `--section-cap`
/// (or a persisted `embed.section_cap`) pins it. Announcing it matters
/// because the alternative is invisible: text past the budget is dropped
/// with no marker in the output, and the user chose the model that decided
/// the number. Any mismatch between the two is printed as a warning.
pub(crate) fn budget_from_args(embedder: &Embedder, args: &[String]) -> EmbedBudget {
    let (raw, _) = config::resolve_pref_cfg(flag_value(args, &["--section-cap"]), "embed.section_cap");
    let override_chars = raw.and_then(|s| s.parse::<usize>().ok());
    let model = &embedder.config().model;
    let budget = EmbedBudget::resolve(model, override_chars);

    let window = match budget.window_tokens {
        Some(t) => format!("{} token window", t),
        None => "unknown window".to_string(),
    };
    let origin = match budget.source {
        BudgetSource::Flag => "pinned by --section-cap",
        BudgetSource::Auto => "derived from the model",
        BudgetSource::Default => "default — model window unknown",
    };
    eprintln!(
        "{C_CYAN}▸{C_RESET} Embedding budget: {C_BOLD}{}{C_RESET} chars per description ({}, {})",
        budget.description_chars, window, origin
    );
    for advice in [budget.advisory(model), budget.related_advisory()]
        .into_iter()
        .flatten()
    {
        eprintln!("{C_YELLOW}⚠{C_RESET}  {}", advice);
    }
    budget
}

/// One-line banner on stderr so the user can see which backend the
/// command is using before any progress output appears. Stderr so that
/// stdout-bound JSON from `hybrid_search` stays
/// clean for piping.
fn announce_embedder(embedder: &Embedder, dim_was_explicit: bool) {
    let cfg = embedder.config();
    let dim_label = if dim_was_explicit {
        format!("dim={}", cfg.dim)
    } else {
        format!("dim={} (auto-probe)", cfg.dim)
    };
    match embedder {
        Embedder::Local(_) => eprintln!(
            "{C_CYAN}▸{C_RESET} Embedder: {C_BOLD}{C_GREEN}local{C_RESET} (fastembed, in-process) — model={C_BOLD}{}{C_RESET}, {}",
            cfg.model, dim_label
        ),
        Embedder::Remote(_) => eprintln!(
            "{C_CYAN}▸{C_RESET} Embedder: {C_BOLD}{C_YELLOW}remote{C_RESET} (HTTP /v1/embeddings) — model={C_BOLD}{}{C_RESET}, base_url={}, {}",
            cfg.model, cfg.base_url, dim_label
        ),
    }
}

/// Like `embedder_from_args`, but used by `ug chat` where a chat model
/// is also in play. `--embedding-model` (or `$UG_EMBED_MODEL`) selects
/// the embeddings independently of `--chat-model` — `--model` has no
/// effect here, since with two services in the same command it's
/// ambiguous which one it would mean.
///
/// For the base-url / api-key, `--embedding-base-url` /
/// `--embedding-api-key` win when set, otherwise the shared
/// `--base-url` / `--api-key` apply (this matches the common case where
/// chat and embedding share a single OpenAI-compatible host), and
/// `$UG_EMBED_BASE_URL` / `$UG_EMBED_API_KEY` are the last fallback.
pub(crate) fn embedder_from_chat_args(args: &[String]) -> Embedder {
    let (dim_raw, _) = config::resolve_pref_cfg(flag_value(args, &["--embedding-dim"]), "embed.dim");
    let dim = dim_raw.and_then(|s| s.parse().ok());
    let base_url_flag = flag_value(args, &["--embedding-base-url"])
        .or_else(|| flag_value(args, &["--base-url"]));
    let (base_url, _) = config::resolve_pref_cfg(base_url_flag, "embed.base_url");
    let api_key_flag = flag_value(args, &["--embedding-api-key"])
        .or_else(|| flag_value(args, &["--api-key"]));
    let (api_key, _) = config::resolve_pref_cfg(api_key_flag, "embed.api_key");
    let (model, _) =
        config::resolve_pref_cfg(flag_value(args, &["--embedding-model"]), "embed.model");
    let want_remote = base_url.is_some();
    let cfg = EmbedderConfig::with_overrides(base_url, api_key, model, dim, None, None);
    let result = if want_remote {
        Embedder::remote(cfg)
    } else {
        Embedder::local(cfg)
    };
    let embedder = result.unwrap_or_else(|e| {
        eprintln!("failed to build embedder: {}", e);
        std::process::exit(1);
    });
    announce_embedder(&embedder, dim.is_some());
    embedder
}

pub(crate) fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| die(1, format!("failed to build tokio runtime: {e}")))
}
