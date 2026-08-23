//! `chat_api.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::chat::{self, ChatClient, ChatConfig, ChatMessage, ChatRagOptions};
use crate::cli::args::flag_value;
use ultragraph::storage::{
    Direction, Embedder, KnowledgeStore, RankStrategy, DEFAULT_CONTEXT_CHARS,
};

use super::db_api::{embedder_or_503, pick_store};
use super::*;

// ---------- Phase 4 — Chat (/api/chat) ----------

/// Pull a default `ChatConfig` from CLI args, env vars, or the
/// persisted `~/.ug/config.json` (`ug config set chat.*`). Returns
/// `None` when no chat model is configured — the route then 503s with
/// a clear message rather than hitting a misconfigured endpoint.
///
/// Env-var fallbacks let `ug serve` be wrapped by `docker run -e
/// UG_CHAT_*` without rewriting the entrypoint.
pub(crate) fn build_chat_default_from_args(args: &[String]) -> Option<ChatConfig> {
    let (model, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--chat-model"]), "chat.model");
    // Chat borrows the embeddings endpoint/key when no chat-specific one
    // is given (the common single-host case): the chat.* chain resolves
    // first, then the embed.* chain (env/config only — its flags were
    // already folded in above).
    let chat_base_flag =
        flag_value(args, &["--chat-base-url"]).or_else(|| flag_value(args, &["--base-url"]));
    let base_url = crate::config::resolve_pref_cfg(chat_base_flag, "chat.base_url")
        .0
        .or_else(|| crate::config::resolve_pref_cfg(None, "embed.base_url").0);
    let chat_api_flag =
        flag_value(args, &["--chat-api-key"]).or_else(|| flag_value(args, &["--api-key"]));
    let api_key = crate::config::resolve_pref_cfg(chat_api_flag, "chat.api_key")
        .0
        .or_else(|| crate::config::resolve_pref_cfg(None, "embed.api_key").0);
    let (temp_raw, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--temperature"]), "chat.temperature");
    let temperature = temp_raw.and_then(|s| s.parse().ok());
    let (max_tok_raw, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--max-tokens"]), "chat.max_tokens");
    let max_tokens = max_tok_raw.and_then(|s| s.parse().ok());
    let (timeout_raw, _) =
        crate::config::resolve_pref_cfg(flag_value(args, &["--chat-timeout"]), "chat.timeout_secs");
    let timeout = timeout_raw.and_then(|s| s.parse().ok());

    // Require at least a chat model — without it we can't reasonably
    // pick one and the endpoint would 4xx every request.
    let model = model?;
    let cfg = ChatConfig::with_overrides(
        base_url,
        api_key,
        Some(model),
        temperature,
        max_tokens,
        timeout,
    );
    Some(cfg)
}

// ---------- Settings (/api/config) ----------

/// JSON view of every persistable config key for the settings UI:
/// saved value, effective value after flag precedence, and which
/// tier won. Secrets are masked — a raw API key never leaves the
/// server, only a short prefix for recognition.
fn config_payload(state: &ServeState) -> serde_json::Value {
    let args: &[String] = &state.serve_args;
    let keys: Vec<serde_json::Value> = crate::config::CONFIG_KEYS
        .iter()
        .map(|key| {
            let saved = crate::config::get(key.name);
            let flag_val = flag_value(args, &[key.flag]);
            let flag_active = flag_val.is_some();
            let (effective, source) = crate::config::resolve_pref_cfg(flag_val, key.name);
            let source_label = match source {
                crate::cli::embed::PrefSource::Flag => "flag",
                crate::cli::embed::PrefSource::Config(_) => "config",
                crate::cli::embed::PrefSource::Default => "default",
            };
            let mask = |v: &String| {
                if key.secret {
                    crate::config::display_value(key, v)
                } else {
                    v.clone()
                }
            };
            serde_json::json!({
                "name": key.name,
                "section": key.section,
                "desc": key.desc,
                "kind": match key.kind {
                    crate::config::Kind::Str => "str",
                    crate::config::Kind::F32 => "f32",
                    crate::config::Kind::U32 => "u32",
                    crate::config::Kind::U64 => "u64",
                    crate::config::Kind::Enum(_) => "enum",
                },
                "choices": match key.kind {
                    crate::config::Kind::Enum(allowed) => allowed.to_vec(),
                    _ => Vec::new(),
                },
                "secret": key.secret,
                "saved": saved.as_ref().map(&mask),
                "effective": effective.as_ref().map(&mask),
                "source": source_label,
                "flag": key.flag,
                "flag_active": flag_active,
                "default": crate::config::default_for(key),
            })
        })
        .collect();
    serde_json::json!({
        "path": crate::config::config_path().display().to_string(),
        "keys": keys,
        // Chat settings apply immediately (chat_default is rebuilt on
        // save); the embedder is constructed at startup, so embed.*
        // changes need a server restart to take effect here.
        "live_sections": ["chat"],
    })
}

pub(crate) async fn api_config_get(State(state): State<ServeState>) -> Response {
    ok_json(config_payload(&state).to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct ConfigPostBody {
    /// key → new value. Strings and numbers accepted; a blank string
    /// clears the key (same as listing it in `unset`).
    #[serde(default)]
    set: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    unset: Vec<String>,
}

/// Persist settings changes from the UI into `~/.ug/config.json`, then
/// refresh the in-process view so chat picks them up immediately.
/// Validation failures reject the whole request — the file is only
/// written when every change parses.
pub(crate) async fn api_config_post(
    State(state): State<ServeState>,
    Json(body): Json<ConfigPostBody>,
) -> Response {
    let path = crate::config::config_path();
    let mut cfg = match crate::config::read_config_file(&path) {
        Ok(c) => c,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    for (name, val) in &body.set {
        let Some(key) = crate::config::find_key(name) else {
            return err_json(
                StatusCode::BAD_REQUEST,
                &format!("unknown config key: {}", name),
            );
        };
        let raw = match val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    &format!("{} expects a string or number value", key.name),
                )
            }
        };
        if raw.trim().is_empty() {
            crate::config::value_unset(&mut cfg, key);
            continue;
        }
        if let Err(e) = crate::config::value_set(&mut cfg, key, raw.trim()) {
            return err_json(StatusCode::BAD_REQUEST, &e);
        }
    }
    for name in &body.unset {
        let Some(key) = crate::config::find_key(name) else {
            return err_json(
                StatusCode::BAD_REQUEST,
                &format!("unknown config key: {}", name),
            );
        };
        crate::config::value_unset(&mut cfg, key);
    }
    if let Err(e) = crate::config::write_config_file(&path, &cfg) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    crate::config::reload();

    let new_default = build_chat_default_from_args(&state.serve_args);
    match new_default.as_ref() {
        Some(c) => {
            tracing::info!(model = %c.model, base_url = %c.base_url, "chat config updated via /api/config")
        }
        None => tracing::info!("chat config cleared via /api/config (/api/chat will return 503)"),
    }
    *state.chat_default.write().expect("chat_default poisoned") = new_default;

    ok_json(config_payload(&state).to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct ChatBody {
    query: String,
    #[serde(default)]
    history: Option<Vec<ChatMessage>>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    hops: Option<u32>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    edge_types: Option<Vec<String>>,
    #[serde(default)]
    include_snippets: Option<bool>,
    #[serde(default)]
    max_context_chars: Option<usize>,
    #[serde(default, rename = "where")]
    where_clause: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    // Per-request chat overrides (UI surfaces these). All optional —
    // anything missing falls back to the default `ChatConfig`.
    #[serde(default)]
    chat_model: Option<String>,
    #[serde(default)]
    chat_base_url: Option<String>,
    #[serde(default)]
    chat_api_key: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Optional destination name; defaults to the primary backend.
    #[serde(default)]
    dest: Option<String>,
    /// `true` → respond as an SSE stream (`context` / `delta` / `done`
    /// / `error` events) instead of one JSON body. Providers that
    /// reject streaming still work: the server falls back to a plain
    /// completion and emits it as a single delta.
    #[serde(default)]
    stream: Option<bool>,
    /// Give the model the graph toolbox (search, outlines, call sites, …).
    /// On by default: a grounded answer beats a fluent one.
    #[serde(default)]
    tools: Option<bool>,
    /// Cap on tool-calling rounds before the model must answer.
    #[serde(default)]
    max_tool_rounds: Option<usize>,
    /// Let a reasoning model deliberate before answering. Off by default —
    /// the answer is grounded in retrieved context, and thinking is where a
    /// local model spends its minutes.
    #[serde(default)]
    think: Option<bool>,
}

/// Citation list shared by the JSON and SSE chat responses.
fn citations_json(items: &[ultragraph::storage::ContextItem]) -> Vec<serde_json::Value> {
    items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            serde_json::json!({
                "index": i + 1,
                "id": it.id,
                "name": it.name,
                "node_type": it.node_type,
                "file": it.file,
                "start_line": it.start_line,
                "end_line": it.end_line,
                "description": it.description,
                "distance": it.distance,
                "hop": it.hop,
                "snippet": it.snippet,
            })
        })
        .collect()
}

pub(crate) async fn api_chat(
    State(state): State<ServeState>,
    Json(body): Json<ChatBody>,
) -> Response {
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };

    // Merge defaults with per-request overrides. Without a default and
    // without an override we can't pick a model, so the route 503s.
    let chat_default = state
        .chat_default
        .read()
        .expect("chat_default poisoned")
        .clone();
    let chat_cfg = match merge_chat_cfg(&chat_default, &body) {
        Ok(c) => c,
        Err(ChatCfgError::NotConfigured) => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "chat not configured (start serve with --chat-model or pass `chat_model` in the request body)",
            )
        }
        Err(ChatCfgError::Invalid(msg)) => return err_json(StatusCode::BAD_REQUEST, &msg),
    };

    let chat_client = match ChatClient::new(chat_cfg) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("build chat client: {}", e),
            )
        }
    };

    if body.stream.unwrap_or(false) {
        return api_chat_stream(state, body, db, embedder, chat_client);
    }

    let k = body.k.unwrap_or(8).min(50).max(1);
    let hops = body.hops.unwrap_or(2).min(4);
    let strategy = body
        .strategy
        .as_deref()
        .map(RankStrategy::from_str_lossy)
        .unwrap_or(RankStrategy::Ppr);
    let direction = body
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    let include_snippets = body.include_snippets.unwrap_or(true);
    let max_context_chars = body
        .max_context_chars
        .unwrap_or(DEFAULT_CONTEXT_CHARS)
        .min(64_000);
    let edge_types_owned: Option<Vec<String>> = body.edge_types.filter(|v| !v.is_empty());
    let history_owned: Vec<ChatMessage> = body.history.unwrap_or_default();

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let mut opts = ChatRagOptions::new();
    opts.k = k;
    opts.hops = hops;
    opts.strategy = strategy;
    opts.direction = direction;
    opts.edge_types = edge_types_owned.as_deref();
    opts.include_snippets = include_snippets;
    opts.max_context_chars = max_context_chars;
    opts.where_clause = body.where_clause.as_deref();
    opts.system_prompt = body.system_prompt.as_deref();
    opts.fast = !body.think.unwrap_or(false);

    let dest_name = db.backend_name();
    let repo_root = state.repo_root();

    // The same toolbox the streaming path builds: `stream` picks how the
    // answer is delivered, not whether the model may consult the graph.
    let tool_state = state.clone();
    let tool_db = db.clone();
    let tool_embedder = Some(embedder.clone());
    let runner = move |name: &str, args: serde_json::Value| {
        let state = tool_state.clone();
        let db = tool_db.clone();
        let embedder = tool_embedder.clone();
        let name = name.to_string();
        Box::pin(async move { run_chat_tool(state, db, embedder, name, args).await })
            as futures::future::BoxFuture<'static, Result<String, String>>
    };
    let toolbox = body.tools.unwrap_or(true).then(|| chat::ToolBox {
        schemas: crate::mcp::tools::openai_tool_schemas(),
        run: &runner,
        max_rounds: body.max_tool_rounds.unwrap_or(4).min(8),
        max_result_chars: 6_000,
    });

    let outcome = chat::run_chat_rag(
        &*db,
        &embedder,
        &chat_client,
        repo_root.as_path(),
        &body.query,
        &history_owned,
        opts,
        toolbox.as_ref(),
    )
    .await;
    drop(_permit);

    match outcome {
        Ok(o) => {
            let citations = citations_json(&o.context.items);
            let body_json = serde_json::json!({
                "query": body.query,
                "answer": o.answer,
                "citations": citations,
                "seed_id": o.context.seed_id,
                "retrieval_ms": o.retrieval_ms,
                "completion_ms": o.completion_ms,
                "usage": o.usage,
                "dest": dest_name,
                "chat_model": chat_client.config().model.clone(),
            });
            ok_json(body_json.to_string())
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("chat: {}", e)),
    }
}

/// SSE variant of `/api/chat` (`"stream": true` in the body). Event
/// sequence the UI consumes:
///
/// ```text
/// event: context   data: {"citations":[…],"seed_id":…,"retrieval_ms":…}
/// event: delta     data: {"content":"…"} | {"reasoning":"…"}
/// event: done      data: {"answer":…,"usage":…,"completion_ms":…,…}
/// event: error     data: {"error":"…"}      (terminal, replaces done)
/// ```
///
/// The RAG turn runs in a spawned task feeding an unbounded channel, so
/// the response starts (and heartbeats) immediately while retrieval is
/// still working.
pub(crate) fn api_chat_stream(
    state: ServeState,
    body: ChatBody,
    db: Arc<dyn KnowledgeStore>,
    embedder: Arc<Embedder>,
    chat_client: ChatClient,
) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use futures::StreamExt;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    let repo_root = state.repo_root();
    let embed_lock = state.embed_lock.clone();

    tokio::spawn(async move {
        let dest_name = db.backend_name();
        let model = chat_client.config().model.clone();
        let endpoint = chat_client.config().base_url.clone();
        let emit = |name: &'static str, payload: serde_json::Value| {
            let _ = tx.send(SseEvent::default().event(name).data(payload.to_string()));
        };

        let _permit = match embed_lock.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                emit(
                    "error",
                    serde_json::json!({ "error": "embed semaphore closed" }),
                );
                return;
            }
        };

        let k = body.k.unwrap_or(8).min(50).max(1);
        let hops = body.hops.unwrap_or(2).min(4);
        let strategy = body
            .strategy
            .as_deref()
            .map(RankStrategy::from_str_lossy)
            .unwrap_or(RankStrategy::Ppr);
        let direction = body
            .direction
            .as_deref()
            .map(Direction::from_str_lossy)
            .unwrap_or(Direction::Both);
        let include_snippets = body.include_snippets.unwrap_or(true);
        let max_context_chars = body
            .max_context_chars
            .unwrap_or(DEFAULT_CONTEXT_CHARS)
            .min(64_000);
        let edge_types_owned: Option<Vec<String>> = body.edge_types.filter(|v| !v.is_empty());
        let history_owned: Vec<ChatMessage> = body.history.unwrap_or_default();

        let mut opts = ChatRagOptions::new();
        opts.k = k;
        opts.hops = hops;
        opts.strategy = strategy;
        opts.direction = direction;
        opts.edge_types = edge_types_owned.as_deref();
        opts.include_snippets = include_snippets;
        opts.max_context_chars = max_context_chars;
        opts.where_clause = body.where_clause.as_deref();
        opts.system_prompt = body.system_prompt.as_deref();
        opts.fast = !body.think.unwrap_or(false);

        emit("phase", serde_json::json!({ "phase": "retrieving" }));

        // Hand the model the graph toolbox so it can chase what retrieval
        // only pointed at — call sites, outlines, exact source, paths.
        let tool_state = state.clone();
        let tool_db = db.clone();
        let tool_embedder = Some(embedder.clone());
        let runner = move |name: &str, args: serde_json::Value| {
            let state = tool_state.clone();
            let db = tool_db.clone();
            let embedder = tool_embedder.clone();
            let name = name.to_string();
            Box::pin(async move { run_chat_tool(state, db, embedder, name, args).await })
                as futures::future::BoxFuture<'static, Result<String, String>>
        };
        let toolbox = if body.tools.unwrap_or(true) {
            Some(chat::ToolBox {
                schemas: crate::mcp::tools::openai_tool_schemas(),
                run: &runner,
                max_rounds: body.max_tool_rounds.unwrap_or(4).min(8),
                max_result_chars: 6_000,
            })
        } else {
            None
        };

        let t_ret = std::time::Instant::now();
        let emit_ctx = emit;
        let emit_tool = emit;
        let emit_delta = emit;
        let outcome = chat::run_chat_rag_stream(
            &*db,
            &embedder,
            &chat_client,
            repo_root.as_path(),
            &body.query,
            &history_owned,
            opts,
            toolbox.as_ref(),
            |ctx| {
                emit_ctx(
                    "context",
                    serde_json::json!({
                        "citations": citations_json(&ctx.items),
                        "seed_id": ctx.seed_id,
                        "retrieval_ms": t_ret.elapsed().as_millis() as u64,
                    }),
                );
            },
            |t: chat::ToolEvent| {
                emit_tool(
                    "tool",
                    serde_json::json!({
                        "name": t.name,
                        "args": t.args,
                        "args_json": t.args_json,
                        "state": if t.summary.is_some() { "done" } else { "start" },
                        "summary": t.summary,
                        "result": t.result,
                    }),
                );
            },
            |d| {
                let mut obj = serde_json::Map::new();
                if let Some(c) = d.content {
                    obj.insert("content".into(), serde_json::Value::String(c));
                }
                if let Some(r) = d.reasoning {
                    obj.insert("reasoning".into(), serde_json::Value::String(r));
                }
                if !obj.is_empty() {
                    emit_delta("delta", serde_json::Value::Object(obj));
                }
            },
        )
        .await;

        match outcome {
            Ok(o) => emit(
                "done",
                serde_json::json!({
                    "answer": o.answer,
                    "reasoning": if o.reasoning.is_empty() { None } else { Some(o.reasoning) },
                    "retrieval_ms": o.retrieval_ms,
                    "completion_ms": o.completion_ms,
                    "tool_calls": o.tool_calls,
                    "usage": o.usage,
                    "dest": dest_name,
                    "chat_model": model,
                }),
            ),
            Err(e) => {
                // Distinguish "your endpoint is down" from "the model erred":
                // only one of them is something the user can fix, and the UI
                // offers the fix when we say which it is.
                let unreachable = e
                    .downcast_ref::<chat::ChatError>()
                    .map(|c| c.is_unreachable())
                    .unwrap_or(false);
                emit(
                    "error",
                    serde_json::json!({
                        "error": format!("chat: {}", e),
                        "kind": if unreachable { "llm_unreachable" } else { "chat_failed" },
                        "endpoint": endpoint,
                    }),
                );
            }
        }
    });

    let stream =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Why a request couldn't be turned into a usable `ChatConfig`.
///
/// Two different failures with two different status codes: "nothing is
/// configured anywhere" is the server's problem (503), while "your
/// override is not something we'll send" is the caller's (400).
#[derive(Debug)]
pub(crate) enum ChatCfgError {
    /// No model from flags, config, or the request body.
    NotConfigured,
    /// The request's own endpoint override was rejected.
    Invalid(String),
}

/// Hosts a request-supplied chat endpoint may never point at.
///
/// Cloud instance-metadata services hand out live credentials to anything
/// that can issue a plain HTTP request from inside the network, so they are
/// the one SSRF target worth naming explicitly. Everything else — including
/// loopback and LAN addresses — stays reachable on purpose: pointing chat at
/// a local Ollama or an on-prem vLLM is the feature.
const BLOCKED_CHAT_HOSTS: &[&str] = &[
    "169.254.169.254",
    "fd00:ec2::254",
    "metadata.google.internal",
    "metadata",
];

/// Scheme + host + port of a chat endpoint, lowercased, for comparing a
/// request's override against the configured default. `None` when the URL
/// doesn't parse or names no host.
pub(crate) fn chat_origin(raw: &str) -> Option<(String, String, Option<u16>)> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let host = url
        .host_str()?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    Some((
        url.scheme().to_ascii_lowercase(),
        host,
        url.port_or_known_default(),
    ))
}

/// Decide the endpoint and credential a chat/tour request may actually use.
///
/// The rule that matters: **a request-supplied `base_url` never inherits the
/// server's stored API key**. `ug serve` has no authentication, so anything
/// that can reach the port — another local process, or a browser page that
/// got there via DNS rebinding — could otherwise post one JSON body naming
/// its own endpoint and have the server deliver the user's real API key to
/// it in an `Authorization` header. Overriding the endpoint is still allowed
/// (flipping models mid-session is the point); it just has to bring its own
/// key, or go keyless for a local server that wants none.
///
/// An override pointing at the *same* origin as the configured default is not
/// a redirection at all — that one keeps the stored key, so a UI that echoes
/// the current `base_url` back in the body keeps working.
pub(crate) fn resolve_chat_endpoint(
    default: &ChatConfig,
    override_url: Option<&str>,
    override_key: Option<&str>,
) -> Result<(String, String), ChatCfgError> {
    let stored_key = || default.api_key.clone();
    let Some(raw) = override_url.map(str::trim).filter(|s| !s.is_empty()) else {
        // No endpoint override: the stored key is going where it always goes.
        return Ok((
            default.base_url.clone(),
            override_key.map(str::to_string).unwrap_or_else(stored_key),
        ));
    };

    let Some((scheme, host, port)) = chat_origin(raw) else {
        return Err(ChatCfgError::Invalid(format!(
            "chat_base_url is not a valid absolute URL: {raw}"
        )));
    };
    if scheme != "http" && scheme != "https" {
        return Err(ChatCfgError::Invalid(format!(
            "chat_base_url must be http or https, got {scheme}"
        )));
    }
    if BLOCKED_CHAT_HOSTS.contains(&host.as_str()) {
        return Err(ChatCfgError::Invalid(format!(
            "chat_base_url host {host} is not allowed"
        )));
    }

    let same_origin =
        chat_origin(&default.base_url).is_some_and(|d| d == (scheme.clone(), host.clone(), port));
    let key = match override_key {
        Some(k) => k.to_string(),
        None if same_origin => stored_key(),
        None => {
            // The interesting case: redirected endpoint, no key of its own.
            // Send nothing rather than the stored secret.
            tracing::warn!(
                host = %host,
                "chat_base_url override points at a different origin than the configured \
                 endpoint — sending the request without the stored API key"
            );
            String::new()
        }
    };
    Ok((raw.to_string(), key))
}

/// Combine a default `ChatConfig` (from CLI/env at startup) with
/// per-request overrides. Errors when neither side provides a model, or
/// when the request's endpoint override is rejected by
/// [`resolve_chat_endpoint`].
fn merge_chat_cfg(
    default: &Option<ChatConfig>,
    body: &ChatBody,
) -> Result<ChatConfig, ChatCfgError> {
    let base_default = default.clone().unwrap_or_default();
    let model = body
        .chat_model
        .clone()
        .or_else(|| default.as_ref().map(|c| c.model.clone()))
        .ok_or(ChatCfgError::NotConfigured)?;
    let (base_url, api_key) = resolve_chat_endpoint(
        &base_default,
        body.chat_base_url.as_deref(),
        body.chat_api_key.as_deref(),
    )?;
    let temperature = body.temperature.unwrap_or(base_default.temperature);
    let max_tokens = body.max_tokens.unwrap_or(base_default.max_tokens);
    Ok(ChatConfig {
        extra_body: None,
        base_url,
        api_key,
        model,
        temperature,
        max_tokens,
        timeout_secs: base_default.timeout_secs,
    })
}

/// `GET /api/chat/config` — what the chat turn is actually made of.
///
/// The answer a model gives depends entirely on three things the UI
/// otherwise hides: the system prompt it was given, the tools it could
/// call, and how the context was retrieved. "Semantic search" is the
/// usual guess for the last one and it's wrong — so publish all three
/// rather than making people read the source to trust the output.
pub(crate) async fn api_chat_config(State(state): State<ServeState>) -> Response {
    use serde_json::json;

    let stores = state.stores();
    let primary = stores.as_ref().and_then(|s| s.get(&s.primary).cloned());
    let native_ppr = primary.as_ref().map(|p| p.supports_native_ppr());
    let backend = primary.as_ref().map(|p| p.backend_name());

    // PPR is the default; a backend without it silently ranks with MMR
    // instead, which changes the results enough to be worth naming.
    let effective = match native_ppr {
        Some(false) => "mmr",
        _ => "ppr",
    };
    let ranking = if effective == "ppr" {
        json!({
            "id": "ppr",
            "label": "Personalized PageRank over the graph",
            "detail": "The fused hits seed a PageRank run across the edge graph, so nodes that neighbour several good hits outrank a single lucky match.",
        })
    } else {
        json!({
            "id": "mmr",
            "label": "Maximal Marginal Relevance rerank",
            "detail": "This backend has no native PageRank, so results are reranked for relevance-vs-diversity instead of expanded through the graph.",
        })
    };

    let tools: Vec<serde_json::Value> = crate::mcp::tools::openai_tool_schemas()
        .into_iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name").cloned().unwrap_or_default(),
                "description": f.get("description").cloned().unwrap_or_default(),
                "parameters": f.get("parameters").cloned().unwrap_or_default(),
            }))
        })
        .collect();

    let body = json!({
        "system_prompt": chat::DEFAULT_SYSTEM_PROMPT,
        "tool_suffix": chat::TOOL_SYSTEM_SUFFIX,
        "tools_enabled_by_default": true,
        "tools": tools,
        "retrieval": {
            "summary": "Hybrid search (dense + keyword) seeds a graph ranking — not semantic-only.",
            "backend": backend,
            "strategy": effective,
            "stages": [
                {
                    "id": "hybrid",
                    "label": "Hybrid seed search — dense + keyword, fused with RRF",
                    "detail": "Your question is embedded and searched by vector similarity, and separately matched as keywords; the two rankings are merged by Reciprocal Rank Fusion so a hit either side can surface.",
                },
                ranking,
                {
                    "id": "budget",
                    "label": "Char-budgeted context pack",
                    "detail": "Top nodes are hydrated with their descriptions and source snippets, then trimmed to the context budget and numbered [#1], [#2] … for citation.",
                },
            ],
            "defaults": {
                "k": 8,
                "hops": 2,
                "direction": "both",
                "include_snippets": true,
                "max_context_chars": DEFAULT_CONTEXT_CHARS,
            },
        },
    });
    ok_json(body.to_string())
}

// ---------- Agent tools for chat & tour (`ToolBox`) ----------

/// Tools a chat/tour model may call, in OpenAI function-calling form.
///
/// The schemas come straight from the MCP registry, so the model behind
/// `/api/chat` sees exactly the toolbox an MCP client sees — one
/// description to maintain, not two. Tools that mutate or that only make
/// sense to an operator (`gen`, `list_projects`) are left out: a chat
/// turn should read the graph, not reshape it.
/// Run one tool against the server's live state.
///
/// The dispatch itself is [`chat::run_chat_tool`], shared with `ug chat`; this
/// only supplies the server's already-open graph snapshot and store handles.
async fn run_chat_tool(
    state: ServeState,
    db: Arc<dyn KnowledgeStore>,
    embedder: Option<Arc<Embedder>>,
    name: String,
    args: serde_json::Value,
) -> Result<String, String> {
    let snap = state.snapshot();
    let ctx = state.active();
    chat::run_chat_tool(
        &name,
        args,
        &snap.parsed,
        ctx.graph_path.as_path(),
        ctx.repo_root.as_path(),
        &*db,
        embedder.as_deref(),
    )
    .await
}

// ---------- Guided tour (/api/tour) ----------

#[derive(serde::Deserialize)]
pub(crate) struct TourBody {
    query: String,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    hops: Option<u32>,
    #[serde(default)]
    max_stops: Option<usize>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    edge_types: Option<Vec<String>>,
    #[serde(default)]
    include_snippets: Option<bool>,
    #[serde(default)]
    max_context_chars: Option<usize>,
    /// Cap on how many candidates may come from one file (0 = no cap).
    /// Keeps a single large file from swallowing the whole itinerary.
    #[serde(default)]
    max_per_file: Option<usize>,
    /// Attach the planning transcript (prompts + raw model reply + parsed
    /// plan) to the response. On by default so the UI can show the user
    /// the JSON the guide actually produced.
    #[serde(default)]
    include_debug: Option<bool>,
    #[serde(default, rename = "where")]
    where_clause: Option<String>,
    /// Skip the LLM guide and return a ranked itinerary from retrieval
    /// only. The route also degrades to this automatically when no chat
    /// model is configured, so a tour always works with just the DB.
    #[serde(default)]
    no_llm: Option<bool>,
    /// Stream planning progress as SSE instead of blocking until the tour
    /// is ready. Planning against a local model runs for minutes, so the
    /// UI wants a running account rather than a spinner.
    #[serde(default)]
    stream: Option<bool>,
    /// Let a reasoning model deliberate before planning. Off by default —
    /// thinking is where a local model spends its minutes, and a tour is a
    /// structured extraction, not a reasoning problem.
    #[serde(default)]
    think: Option<bool>,
    /// Let the guide research with the graph tools before routing.
    #[serde(default)]
    research: Option<bool>,
    /// Cap on research rounds.
    #[serde(default)]
    max_tool_rounds: Option<usize>,
    // Per-request chat overrides, same shape as /api/chat.
    #[serde(default)]
    chat_model: Option<String>,
    #[serde(default)]
    chat_base_url: Option<String>,
    #[serde(default)]
    chat_api_key: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    dest: Option<String>,
}

/// Merge the default `ChatConfig` with per-request overrides for a tour.
/// [`ChatCfgError::NotConfigured`] means no model could be resolved — the
/// caller then plans a narration-free ranked tour instead of erroring —
/// while `Invalid` is a rejected endpoint override and must surface as a
/// 400. Endpoint/credential handling is [`resolve_chat_endpoint`]'s, the
/// same as `/api/chat`: a tour body carries the identical override fields
/// and would otherwise be the second way to walk off with the stored key.
fn merge_tour_chat_cfg(
    default: &Option<ChatConfig>,
    body: &TourBody,
) -> Result<ChatConfig, ChatCfgError> {
    let base_default = default.clone().unwrap_or_default();
    let model = body
        .chat_model
        .clone()
        .or_else(|| default.as_ref().map(|c| c.model.clone()))
        .ok_or(ChatCfgError::NotConfigured)?;
    let (base_url, api_key) = resolve_chat_endpoint(
        &base_default,
        body.chat_base_url.as_deref(),
        body.chat_api_key.as_deref(),
    )?;
    let temperature = body.temperature.unwrap_or(base_default.temperature);
    let max_tokens = body.max_tokens.unwrap_or(base_default.max_tokens);
    Ok(ChatConfig {
        extra_body: None,
        base_url,
        api_key,
        model,
        temperature,
        max_tokens,
        timeout_secs: base_default.timeout_secs,
    })
}

/// Shape a `TourOptions` from a request body. `edge_types` is passed
/// separately because it has to outlive the borrow.
fn tour_opts_from_body<'a>(
    body: &'a TourBody,
    edge_types: Option<&'a [String]>,
) -> crate::tour::TourOptions<'a> {
    let mut opts = crate::tour::TourOptions::new();
    opts.k = body.k.unwrap_or(14).clamp(1, 80);
    opts.hops = body.hops.unwrap_or(2).min(4);
    opts.max_stops = body
        .max_stops
        .unwrap_or(crate::tour::DEFAULT_MAX_STOPS)
        .clamp(1, crate::tour::MAX_STOPS_LIMIT);
    opts.strategy = body
        .strategy
        .as_deref()
        .map(RankStrategy::from_str_lossy)
        .unwrap_or(RankStrategy::Ppr);
    opts.direction = body
        .direction
        .as_deref()
        .map(Direction::from_str_lossy)
        .unwrap_or(Direction::Both);
    opts.edge_types = edge_types;
    opts.include_snippets = body.include_snippets.unwrap_or(true);
    opts.max_context_chars = body
        .max_context_chars
        .unwrap_or(DEFAULT_CONTEXT_CHARS)
        .min(64_000);
    opts.where_clause = body.where_clause.as_deref();
    opts.max_per_file = body.max_per_file.unwrap_or(opts.max_per_file).min(20);
    opts.include_debug = body.include_debug.unwrap_or(true);
    opts.stream = body.stream.unwrap_or(false);
    opts.fast = !body.think.unwrap_or(false);
    opts.research = body.research.unwrap_or(false);
    opts
}

/// Attach the fields the route adds on top of a planned `Tour`.
fn tour_response_json(
    tour: &crate::tour::Tour,
    dest: &str,
    model: Option<&str>,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut v = serde_json::to_value(tour)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("dest".into(), serde_json::Value::String(dest.to_string()));
        if let Some(m) = model {
            obj.insert(
                "chat_model".into(),
                serde_json::Value::String(m.to_string()),
            );
        }
    }
    Ok(v)
}

/// SSE variant of `/api/tour` (`"stream": true` in the body). Planning a
/// tour against a local model is a minutes-long wait dominated by token
/// generation, so the route narrates itself:
///
/// ```text
/// event: progress  data: {"phase":"retrieved","candidates":14,…}
/// event: progress  data: {"phase":"writing","chars":812,…}
/// event: tour      data: {…the full Tour…}
/// event: error     data: {"error":"…"}      (terminal, replaces tour)
/// ```
pub(crate) fn api_tour_stream(
    state: ServeState,
    body: TourBody,
    db: Arc<dyn KnowledgeStore>,
    embedder: Arc<Embedder>,
    chat_cfg: Option<ChatConfig>,
    want_llm: bool,
) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use futures::StreamExt;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    let repo_root = state.repo_root();
    let embed_lock = state.embed_lock.clone();

    tokio::spawn(async move {
        let dest_name = db.backend_name();
        let emit = |name: &'static str, payload: serde_json::Value| {
            let _ = tx.send(SseEvent::default().event(name).data(payload.to_string()));
        };

        let _permit = match embed_lock.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                emit(
                    "error",
                    serde_json::json!({ "error": "embed semaphore closed" }),
                );
                return;
            }
        };

        let edge_types_owned: Option<Vec<String>> =
            body.edge_types.clone().filter(|v| !v.is_empty());
        let opts = tour_opts_from_body(&body, edge_types_owned.as_deref());

        // Same graph toolbox chat gets, so a tour can look past the nodes
        // retrieval happened to surface. Off unless asked for: every tool
        // round is another wait before the first stop.
        let tool_state = state.clone();
        let tool_db = db.clone();
        let tool_embedder = Some(embedder.clone());
        let runner = move |name: &str, args: serde_json::Value| {
            let state = tool_state.clone();
            let db = tool_db.clone();
            let embedder = tool_embedder.clone();
            let name = name.to_string();
            Box::pin(async move { run_chat_tool(state, db, embedder, name, args).await })
                as futures::future::BoxFuture<'static, Result<String, String>>
        };
        let toolbox = opts.research.then(|| chat::ToolBox {
            schemas: crate::mcp::tools::openai_tool_schemas(),
            run: &runner,
            max_rounds: body.max_tool_rounds.unwrap_or(3).min(8),
            max_result_chars: 4_000,
        });

        let mut used_model: Option<String> = None;
        let result = match chat_cfg {
            Some(cfg) => match ChatClient::new(cfg) {
                Ok(client) => {
                    used_model = Some(client.config().model.clone());
                    let emit_progress = emit;
                    let mut on_progress =
                        move |p: crate::tour::TourProgress| match serde_json::to_value(&p) {
                            Ok(v) => emit_progress("progress", v),
                            Err(e) => tracing::debug!(error = %e, "tour: progress encode failed"),
                        };
                    match crate::tour::plan_tour_with_progress(
                        &*db,
                        &embedder,
                        &client,
                        repo_root.as_path(),
                        &body.query,
                        opts.clone(),
                        toolbox.as_ref(),
                        &mut on_progress,
                    )
                    .await
                    {
                        Ok(t) => Ok(t),
                        Err(e) => {
                            tracing::warn!(error = %e, "tour guide LLM failed; falling back to ranked itinerary");
                            used_model = None;
                            let reason = e.to_string();
                            emit(
                                "progress",
                                serde_json::json!({ "phase": "fallback", "reason": reason }),
                            );
                            crate::tour::plan_tour_no_llm(
                                &*db,
                                &embedder,
                                repo_root.as_path(),
                                &body.query,
                                opts.clone(),
                            )
                            .await
                            .map(|mut t| {
                                t.warnings.push(format!(
                                    "The tour guide model was unreachable ({}); showing a ranked itinerary.",
                                    reason
                                ));
                                t
                            })
                        }
                    }
                }
                Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
            },
            None => {
                emit("progress", serde_json::json!({ "phase": "retrieving" }));
                crate::tour::plan_tour_no_llm(
                    &*db,
                    &embedder,
                    repo_root.as_path(),
                    &body.query,
                    opts.clone(),
                )
                .await
                .map(|mut t| {
                    if want_llm && !t.stops.is_empty() {
                        t.warnings.push(
                            "No chat model is configured, so this is a ranked itinerary rather than a narrated tour."
                                .to_string(),
                        );
                    }
                    t
                })
            }
        };

        match result {
            Ok(tour) => match tour_response_json(&tour, dest_name, used_model.as_deref()) {
                Ok(v) => emit("tour", v),
                Err(e) => emit(
                    "error",
                    serde_json::json!({ "error": format!("encode: {}", e) }),
                ),
            },
            Err(e) => emit(
                "error",
                serde_json::json!({ "error": format!("tour: {}", e) }),
            ),
        }
    });

    let stream =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `POST /api/tour` — plan a guided, narrated walkthrough for a question.
/// Retrieval is required (needs DB + embedder); the LLM guide is optional
/// (falls back to a ranked itinerary), so this route works whenever
/// semantic search does. Returns the full `Tour` (stops carry node ids the
/// UI flies the camera to).
pub(crate) async fn api_tour(
    State(state): State<ServeState>,
    Json(body): Json<TourBody>,
) -> Response {
    if body.query.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query is required");
    }
    let db = match pick_store(&state, body.dest.as_deref()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let embedder = match embedder_or_503(&state) {
        Ok(e) => e,
        Err(r) => return r,
    };

    let edge_types_owned: Option<Vec<String>> = body.edge_types.clone().filter(|v| !v.is_empty());

    // Decide the LLM path up front so we can fall back cleanly.
    let want_llm = !body.no_llm.unwrap_or(false);
    let chat_default = state
        .chat_default
        .read()
        .expect("chat_default poisoned")
        .clone();
    let chat_cfg = if want_llm {
        match merge_tour_chat_cfg(&chat_default, &body) {
            Ok(c) => Some(c),
            // No model anywhere is the documented fallback: plan the tour
            // without narration rather than failing the request.
            Err(ChatCfgError::NotConfigured) => None,
            // A rejected override is a caller error, not a reason to
            // silently narrate with a different endpoint than was asked for.
            Err(ChatCfgError::Invalid(msg)) => return err_json(StatusCode::BAD_REQUEST, &msg),
        }
    } else {
        None
    };

    if body.stream.unwrap_or(false) {
        return api_tour_stream(state, body, db, embedder, chat_cfg, want_llm);
    }

    let _permit = match state.embed_lock.acquire().await {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::SERVICE_UNAVAILABLE, "embed semaphore closed"),
    };

    let repo_root = state.repo_root();
    let dest_name = db.backend_name();

    let opts = tour_opts_from_body(&body, edge_types_owned.as_deref());

    let mut used_model: Option<String> = None;
    let result = match chat_cfg {
        Some(cfg) => match ChatClient::new(cfg) {
            Ok(client) => {
                used_model = Some(client.config().model.clone());
                match crate::tour::plan_tour(
                    &*db,
                    &embedder,
                    &client,
                    repo_root.as_path(),
                    &body.query,
                    opts.clone(),
                )
                .await
                {
                    Ok(t) => Ok(t),
                    Err(e) => {
                        // LLM unreachable/failed — still give a tour, but
                        // say why it isn't narrated.
                        tracing::warn!(error = %e, "tour guide LLM failed; falling back to ranked itinerary");
                        used_model = None;
                        let reason = e.to_string();
                        crate::tour::plan_tour_no_llm(
                            &*db,
                            &embedder,
                            repo_root.as_path(),
                            &body.query,
                            opts.clone(),
                        )
                        .await
                        .map(|mut t| {
                            t.warnings
                                .push(format!("The tour guide model was unreachable ({}); showing a ranked itinerary.", reason));
                            t
                        })
                    }
                }
            }
            Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
        },
        None => {
            let asked_for_llm = want_llm;
            crate::tour::plan_tour_no_llm(
                &*db,
                &embedder,
                repo_root.as_path(),
                &body.query,
                opts.clone(),
            )
            .await
            .map(|mut t| {
                if asked_for_llm && !t.stops.is_empty() {
                    t.warnings.push(
                        "No chat model is configured, so this is a ranked itinerary rather than a narrated tour."
                            .to_string(),
                    );
                }
                t
            })
        }
    };
    drop(_permit);

    match result {
        Ok(tour) => match tour_response_json(&tour, dest_name, used_model.as_deref()) {
            Ok(v) => ok_json(v.to_string()),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {}", e)),
        },
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("tour: {}", e)),
    }
}
