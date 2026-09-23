//! `local_llm.rs` — the browser tab as an inference endpoint.
//!
//! The page can download a GGUF and run it with wllama (llama.cpp compiled to
//! WebAssembly). Everything that asks ug a question, though — `/api/chat`,
//! `/api/tour`, `/api/walk`, `ug chat` in a terminal — goes through
//! [`ChatClient`], which speaks HTTP to an OpenAI-compatible endpoint. Rather
//! than teach each of those about a second kind of model, this module *is*
//! such an endpoint: `POST /api/llm/local/v1/chat/completions` looks exactly
//! like any other provider and is answered by the tab.
//!
//! ```text
//!   run_chat_rag ──► ChatClient ──HTTP──► /api/llm/local/v1/chat/completions
//!                                              │  job                    ▲
//!                                              ▼                         │
//!                                      SSE /events  ──►  browser  ──POST─┘
//!                                                         wllama      deltas
//! ```
//!
//! So the server keeps every prompt, tool round and citation ledger it already
//! had, and only the token generation moves into the tab. Two consequences
//! worth stating, because both are load-bearing:
//!
//! 1. **The tab is the provider, so losing the tab is losing the provider.**
//!    The SSE stream is the liveness signal: when it drops, the attachment is
//!    torn down and every in-flight job fails immediately with a message that
//!    says the tab went away — rather than each caller waiting out its own
//!    timeout against something that is never going to answer.
//! 2. **A browser model's context window is small.** A hosted model takes the
//!    60 kB of retrieved context ug packs by default; a 4k-token window takes
//!    about a tenth of that. So every turn is sized against the attached
//!    window first ([`LocalLlm::plan_prompt`]), and the chat and tour routes
//!    clamp to the character budget it returns, because the alternative is a
//!    `kv_cache_full` error the user can do nothing about.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::chat::ChatConfig;

use super::*;

/// How long a single generation may take before the caller gives up.
///
/// Generous on purpose: a 4B model on a CPU-only laptop answers at a few
/// tokens a second, and the failure this guards against is a wedged tab, which
/// the SSE liveness check catches in seconds anyway.
const JOB_TIMEOUT: Duration = Duration::from_secs(900);

/// Hard ceiling on a single generation, whatever the window allows.
///
/// `ChatConfig` defaults to 32768 `max_tokens` and a tour asks for all of it;
/// that is a fine ceiling for a hosted model and a promise of a twelve-minute
/// wait at the ~45 tok/s a 0.6B manages on a laptop CPU.
const MAX_COMPLETION_TOKENS: u32 = 4096;

/// Floor, so a tiny window still gets a usable answer rather than one
/// sentence.
const MIN_COMPLETION_TOKENS: u32 = 256;

/// Tokens set aside for the system prompt and the question itself when
/// turning a context window into a retrieval budget.
const PROMPT_OVERHEAD_TOKENS: u32 = 512;

/// Characters per token, conservatively. Code and JSON tokenize worse than
/// prose, and overshooting the window is a hard error while undershooting only
/// costs a little context.
const CHARS_PER_TOKEN: usize = 3;

/// The graph toolbox is written for a capable agent: twelve tools, ~35 kB of
/// JSON Schema, about 9 900 tokens before the conversation starts. That is
/// more than twice a 4k-token window — which is why "chat bad status 502:
/// request (10183 tokens) exceeds the available context size (4096)" was the
/// first thing a browser model did. Two levers, in this order: fewer tools,
/// and shorter descriptions.
///
/// These five are what a question about code actually needs: find it, read
/// it, see who calls it, get everything about one symbol, and orient.
const COMPACT_TOOLS: &[&str] = &[
    "search",
    "get_code",
    "find_usages",
    "context",
    "project_overview",
];

/// Description budgets for the compact schemas. The prose exists to teach an
/// agent the subtleties; a 1.7B model is not going to read four hundred words
/// of it, and every one of those words displaces the code it is meant to be
/// reasoning about.
const COMPACT_DESC_CHARS: usize = 200;
const COMPACT_PROP_DESC_CHARS: usize = 90;

/// When even the compact five do not fit, one tool still might: finding code
/// is the one thing a model cannot do from the prompt alone.
const MINIMAL_TOOLS: &[&str] = &["search", "get_code"];

/// Below this, a toolbox is affordable but pointless: the model can call a
/// tool and then has no room for what it returns.
const MIN_TOOL_RESULT_TOKENS: u32 = 1_500;

/// The same floor for the two-tool tier. A single search result of ~2 700
/// characters is a real answer's worth of context; demanding the full
/// allowance would throw the tools away over a few hundred tokens.
const MIN_MINIMAL_RESULT_TOKENS: u32 = 900;

/// Tool rounds a browser model may spend. Each round's result stays in the
/// window for the rest of the turn, so the ceiling is a memory budget, not
/// just patience. (`DEFAULT_TOOL_ROUNDS` is 8.)
const BROWSER_TOOL_ROUNDS: usize = 3;

// ---------- State ----------

/// A model the page has loaded and offered to the server.
#[derive(Clone, Debug)]
pub(crate) struct Attached {
    /// Catalog id, e.g. `qwen3-0.6b`, or `hf:owner/repo/file.gguf` for a model
    /// the user pasted in themselves.
    pub(crate) model: String,
    /// What to show a human — "Qwen3 0.6B".
    pub(crate) label: String,
    /// The context window the page actually loaded the model with.
    pub(crate) n_ctx: u32,
    /// Whether the page reported that this model has a tool-calling template.
    pub(crate) supports_tools: bool,
    /// Backend the page ended up on — "webgpu" or "wasm".
    pub(crate) backend: String,
    /// The browser connection serving it. Jobs go to this one client, so two
    /// open tabs cannot both answer the same request.
    client_id: String,
    /// Unix seconds, for the UI's "running since".
    pub(crate) since: u64,
}

impl Attached {
    /// How many tokens this model may spend on one answer.
    ///
    /// A quarter of the window: enough for a chat turn or a tour plan, and it
    /// leaves three quarters for the context those are built from. The same
    /// reserve is subtracted again in [`LocalLlm::plan_prompt`], which decides
    /// the retrieval budget — a reserve that is not the budget actually handed
    /// to the model is how a prompt ends up one token too long for its window.
    ///
    /// The retrieval budget itself deliberately lives *only* there. It used to
    /// have a second implementation here as well, and the two had already
    /// drifted: this one never subtracted the system prompt, so it advertised
    /// a window larger than the one a turn actually gets. Nothing but its own
    /// tests ever called it (§11l).
    pub(crate) fn completion_tokens(&self) -> u32 {
        (self.n_ctx / 4).clamp(MIN_COMPLETION_TOKENS, MAX_COMPLETION_TOKENS)
    }
}

/// One generation the tab has been asked to run.
struct JobSlot {
    events: UnboundedSender<JobEvent>,
    client_id: String,
}

/// What the tab reports back about a job, in the order the caller sees it.
#[derive(Debug)]
enum JobEvent {
    Delta { content: String },
    Done(Box<Completion>),
    Failed(String),
}

#[derive(Debug, Default)]
struct Completion {
    content: String,
    tool_calls: Vec<Value>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

#[derive(Default)]
struct Inner {
    attached: Option<Attached>,
    /// Connected browser event streams, keyed by the id the page generated.
    clients: HashMap<String, UnboundedSender<SseEvent>>,
    jobs: HashMap<String, JobSlot>,
    next_job: u64,
    /// The chat config that was in force before the tab attached, restored on
    /// detach so turning the feature off puts the user's own endpoint back.
    displaced: Option<Option<ChatConfig>>,
}

/// The hub. One per server; cloned around inside [`ServeState`] as an `Arc`.
pub(crate) struct LocalLlm {
    inner: Mutex<Inner>,
    /// Port the server is actually listening on, filled in after `bind`. The
    /// bridge URL has to be one *this process* can reach, so it is always
    /// loopback regardless of `--host`.
    port: OnceLock<u16>,
    /// The `-p/--port` value, used until the listener confirms the real one
    /// (and in tests, which never bind).
    configured_port: u16,
}

/// Why a completion could not be handed to a browser.
#[derive(Debug)]
pub(crate) enum DispatchError {
    /// No tab has offered a model.
    NotAttached,
    /// A model is attached but its stream has gone — the tab was closed or
    /// reloaded and we have not noticed the teardown yet.
    ClientGone,
}

impl LocalLlm {
    pub(crate) fn new(configured_port: u16) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            port: OnceLock::new(),
            configured_port,
        }
    }

    /// Record the address the listener actually bound. Called once, after
    /// `bind` — with `-p 0` the configured port is a lie and the bridge URL
    /// built from it would point at nothing.
    pub(crate) fn set_port(&self, port: u16) {
        let _ = self.port.set(port);
    }

    fn base_url(&self) -> String {
        let port = self.port.get().copied().unwrap_or(self.configured_port);
        format!("http://127.0.0.1:{port}/api/llm/local/v1")
    }

    /// Is this `ChatConfig` the browser bridge rather than a real endpoint?
    /// Used by the capabilities route to label the model, and by detach to
    /// avoid restoring over a config the user changed in the meantime.
    pub(crate) fn owns(&self, cfg: &ChatConfig) -> bool {
        cfg.base_url == self.base_url()
    }

    pub(crate) fn attached(&self) -> Option<Attached> {
        self.lock().attached.clone()
    }


    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The `ChatConfig` that routes this server's chat through the tab.
    fn bridge_config(&self, attached: &Attached) -> ChatConfig {
        ChatConfig::with_overrides(
            Some(self.base_url()),
            // The bridge is loopback-only and guarded by the same Host check
            // as every other route; there is no secret to carry.
            Some(String::new()),
            Some(attached.model.clone()),
            None,
            Some(attached.completion_tokens()),
            Some(JOB_TIMEOUT.as_secs()),
        )
    }

    /// Hand a request to the tab. Returns the job id and the stream of what
    /// comes back.
    fn dispatch(&self, request: Value) -> Result<(String, UnboundedReceiver<JobEvent>), DispatchError> {
        let mut inner = self.lock();
        let attached = inner.attached.clone().ok_or(DispatchError::NotAttached)?;
        let client = inner
            .clients
            .get(&attached.client_id)
            .cloned()
            .ok_or(DispatchError::ClientGone)?;

        inner.next_job += 1;
        let id = format!("j{}", inner.next_job);
        let (tx, rx) = mpsc::unbounded_channel();
        inner.jobs.insert(
            id.clone(),
            JobSlot {
                events: tx,
                client_id: attached.client_id.clone(),
            },
        );
        drop(inner);

        let event = SseEvent::default()
            .event("job")
            .data(json!({ "id": id, "request": request }).to_string());
        if client.send(event).is_err() {
            // The receiver is gone: the stream task ended between our lookup
            // and this send. Clean up rather than leaving a job nobody runs.
            self.lock().jobs.remove(&id);
            return Err(DispatchError::ClientGone);
        }
        Ok((id, rx))
    }

    fn push(&self, job: &str, event: JobEvent) -> bool {
        let inner = self.lock();
        match inner.jobs.get(job) {
            Some(slot) => slot.events.send(event).is_ok(),
            None => false,
        }
    }

    /// Drop a finished or abandoned job, telling the tab to stop if it is
    /// still working on it.
    fn retire(&self, job: &str, cancel: bool) {
        let mut inner = self.lock();
        let Some(slot) = inner.jobs.remove(job) else {
            return;
        };
        if !cancel {
            return;
        }
        if let Some(client) = inner.clients.get(&slot.client_id) {
            let _ = client.send(
                SseEvent::default()
                    .event("cancel")
                    .data(json!({ "id": job }).to_string()),
            );
        }
    }

    /// Register a browser event stream.
    fn connect(&self, client_id: String) -> UnboundedReceiver<SseEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().clients.insert(client_id, tx);
        rx
    }

    /// Push one event to a connected tab, if it is still there.
    fn notify(&self, client_id: &str, event: SseEvent) {
        if let Some(tx) = self.lock().clients.get(client_id) {
            let _ = tx.send(event);
        }
    }

    /// Tear down a browser event stream: fail its jobs and, if it was the one
    /// serving the attached model, detach.
    ///
    /// Returns the chat config to restore, if any.
    fn disconnect(&self, client_id: &str) -> Option<Option<ChatConfig>> {
        let mut inner = self.lock();
        inner.clients.remove(client_id);

        let orphaned: Vec<String> = inner
            .jobs
            .iter()
            .filter(|(_, slot)| slot.client_id == client_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in orphaned {
            if let Some(slot) = inner.jobs.remove(&id) {
                let _ = slot.events.send(JobEvent::Failed(
                    "the browser tab running the model went away".to_string(),
                ));
            }
        }

        match inner.attached.as_ref() {
            Some(a) if a.client_id == client_id => {
                inner.attached = None;
                inner.displaced.take()
            }
            _ => None,
        }
    }
}

/// Apply a browser model's window to a tour's budgets.
///
/// Both context knobs, because they answer different questions:
/// `max_context_chars` is the retrieval budget, and `context_hard_cap` is what
/// stops the planner's auto-scaling from growing the prompt back past the
/// window (see `TourOptions::context_hard_cap`). The stop count matters for
/// the same reason — a longer itinerary is a longer prompt *and* a longer
/// answer.
pub(crate) fn clamp_tour_opts(plan: &PromptPlan, opts: &mut crate::tour::TourOptions<'_>) {
    let Some(cap) = plan.context_chars else { return };
    opts.max_context_chars = opts.max_context_chars.min(cap);
    opts.context_hard_cap = Some(match opts.context_hard_cap {
        Some(existing) => existing.min(cap),
        None => cap,
    });
    if let Some(ui) = plan.ui {
        // Every stop costs a menu entry in the planning prompt and a
        // narration in the answer, so the itinerary is bounded by the window
        // just as the pack is.
        opts.max_stops = opts.max_stops.min(ui.stops);
        opts.k = opts.k.min(ui.chat_k);
        opts.hops = opts.hops.min(ui.hops);
    }
}

/// What a turn may spend, given the window of the model that will answer it.
///
/// Every field is `None` when no browser model is attached, meaning "leave
/// the caller's own numbers alone" — a hosted model needs none of this.
pub(crate) struct PromptPlan {
    /// Tool schemas to offer, or `None` for a toolless turn. A toolless turn
    /// is not a crippled one: chat falls back to seeded retrieval, which is
    /// the `--no-tools` path and answers most questions perfectly well.
    pub(crate) schemas: Option<Vec<Value>>,
    /// Ceiling on the retrieved context pack.
    pub(crate) context_chars: Option<usize>,
    /// Ceiling on one tool result reaching the prompt. The default is 60 000
    /// characters — twenty thousand tokens, or five times a 4k window.
    pub(crate) tool_result_chars: Option<usize>,
    /// Ceiling on tool rounds, because every round's result stays in the
    /// window until the turn ends.
    pub(crate) tool_rounds: Option<usize>,
    /// Ceilings for the retrieval knobs the UI exposes. `None` when nothing
    /// is attached — the user's own numbers stand.
    pub(crate) ui: Option<UiLimits>,
}

/// What the retrieval controls may be set to before the answer stops fitting.
///
/// The server clamps to these anyway; publishing them is what lets the panel
/// cap the inputs themselves, so the number on screen is the number that will
/// be used rather than one silently ignored.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UiLimits {
    /// `k` — retrieved context items per answer.
    pub(crate) chat_k: usize,
    /// Results shown by Find.
    pub(crate) results: usize,
    /// Graph expansion hops out from the seeds.
    pub(crate) hops: u32,
    /// Stops in a tour.
    pub(crate) stops: usize,
}

impl UiLimits {
    /// Derived from the character budget, using what each unit actually costs
    /// in a prompt: ~900 characters for a context item with its snippet,
    /// ~400 for a search hit, ~1 200 for a tour stop and the menu entry
    /// behind it. Rounded down, then floored so the controls never collapse
    /// to something unusable.
    fn from_context_chars(chars: usize) -> Self {
        Self {
            chat_k: (chars / 900).clamp(2, 50),
            results: (chars / 400).clamp(3, 50),
            // Every hop multiplies the candidate set, and the pack is what
            // pays for it.
            hops: if chars < 8_000 { 1 } else { 2 },
            stops: (chars / 1_200).clamp(3, 12),
        }
    }

    pub(crate) fn to_json(self) -> Value {
        json!({
            "chat_k": self.chat_k,
            "results": self.results,
            "hops": self.hops,
            "stops": self.stops,
        })
    }
}

impl PromptPlan {
    /// The plan for a server with no browser model attached: whatever the
    /// caller asked for.
    fn unconstrained(want_tools: bool) -> Self {
        Self {
            schemas: want_tools.then(crate::mcp::tools::openai_tool_schemas),
            context_chars: None,
            tool_result_chars: None,
            tool_rounds: None,
            ui: None,
        }
    }
}

/// Rough token cost of a JSON payload once it is on the wire.
fn tokens_of_json(value: &[Value]) -> u32 {
    let chars: usize = value.iter().map(|v| v.to_string().len()).sum();
    (chars / CHARS_PER_TOKEN) as u32
}

fn tokens_of_text(text: &str) -> u32 {
    (text.len() / CHARS_PER_TOKEN) as u32
}

/// Keep the first sentence (or `max` characters) of a description.
fn shorten(text: &str, max: usize) -> String {
    let first = text.split_once(". ").map_or(text, |(head, _)| head);
    let first = first.trim();
    if first.len() <= max {
        return first.to_string();
    }
    let cut = first
        .char_indices()
        .take_while(|(i, _)| *i <= max)
        .map(|(i, _)| i)
        .last()
        .unwrap_or(0);
    format!("{}…", first[..cut].trim_end())
}

/// The toolbox, cut down to what fits in a browser model's window.
fn compact_tool_schemas() -> Vec<Value> {
    trimmed_tool_schemas(COMPACT_TOOLS)
}

fn minimal_tool_schemas() -> Vec<Value> {
    trimmed_tool_schemas(MINIMAL_TOOLS)
}

fn trimmed_tool_schemas(keep: &[&str]) -> Vec<Value> {
    crate::mcp::tools::openai_tool_schemas()
        .into_iter()
        .filter(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|n| keep.contains(&n))
        })
        .map(|mut t| {
            let Some(f) = t.get_mut("function").and_then(Value::as_object_mut) else {
                return t;
            };
            if let Some(Value::String(d)) = f.get_mut("description") {
                *d = shorten(d, COMPACT_DESC_CHARS);
            }
            if let Some(props) = f
                .get_mut("parameters")
                .and_then(|p| p.get_mut("properties"))
                .and_then(Value::as_object_mut)
            {
                for (_, prop) in props.iter_mut() {
                    if let Some(Value::String(d)) = prop.get_mut("description") {
                        *d = shorten(d, COMPACT_PROP_DESC_CHARS);
                    }
                }
            }
            t
        })
        .collect()
}

impl LocalLlm {
    /// Fit a turn into the attached model's context window.
    ///
    /// The arithmetic is the whole point of this function: a window has to
    /// hold the system prompt, the tool schemas, the question, everything the
    /// tools return *and* the answer, all at once. Only the schemas are big
    /// enough to make the sum impossible on their own, so they are what gives
    /// way first.
    pub(crate) fn plan_prompt(&self, want_tools: bool) -> PromptPlan {
        let Some(a) = self.attached() else {
            return PromptPlan::unconstrained(want_tools);
        };

        let system = tokens_of_text(crate::chat::DEFAULT_SYSTEM_PROMPT);
        // The question, the chat scaffolding and any history.
        let budget = a
            .n_ctx
            .saturating_sub(a.completion_tokens())
            .saturating_sub(PROMPT_OVERHEAD_TOKENS)
            .saturating_sub(system);

        // A GGUF with no tool-calling chat template cannot drive the toolbox
        // at any window size, and a turn with tools on does no seed
        // retrieval — so offering it tools would answer from nothing.
        if want_tools && a.supports_tools {
            let suffix = tokens_of_text(crate::chat::TOOL_SYSTEM_SUFFIX);
            // Full first: a 32k-window model can afford the real toolbox, and
            // the compact one is a loss of capability, not a free win.
            for (schemas, floor) in [
                (
                    crate::mcp::tools::openai_tool_schemas(),
                    MIN_TOOL_RESULT_TOKENS,
                ),
                (compact_tool_schemas(), MIN_TOOL_RESULT_TOKENS),
                (minimal_tool_schemas(), MIN_MINIMAL_RESULT_TOKENS),
            ] {
                let left = budget
                    .saturating_sub(tokens_of_json(&schemas))
                    .saturating_sub(suffix);
                if left >= floor {
                    // Split what is left across the rounds, since each
                    // result stays in the window for the rest of the turn.
                    let per_round = left as usize / BROWSER_TOOL_ROUNDS;
                    let chars = per_round * CHARS_PER_TOKEN;
                    return PromptPlan {
                        schemas: Some(schemas),
                        // With tools on, chat does no seed retrieval; the
                        // pack budget still bounds anything that does land.
                        context_chars: Some(chars),
                        tool_result_chars: Some(chars),
                        tool_rounds: Some(BROWSER_TOOL_ROUNDS),
                        ui: Some(UiLimits::from_context_chars(chars)),
                    };
                }
            }
            tracing::debug!(
                n_ctx = a.n_ctx,
                "in-browser model has no room for the graph toolbox; answering from seeded retrieval instead"
            );
        }

        let chars = (budget as usize * CHARS_PER_TOKEN).max(2_000);
        PromptPlan {
            schemas: None,
            context_chars: Some(chars),
            tool_result_chars: None,
            tool_rounds: None,
            ui: Some(UiLimits::from_context_chars(chars)),
        }
    }
}

// ---------- Routes ----------

#[derive(Deserialize)]
pub(crate) struct AttachBody {
    client_id: String,
    model: String,
    #[serde(default)]
    label: Option<String>,
    n_ctx: u32,
    #[serde(default)]
    supports_tools: bool,
    #[serde(default)]
    backend: Option<String>,
}

/// `POST /api/llm/local/attach` — the page has a model loaded and ready.
///
/// Installs the bridge as the server's chat endpoint, remembering what it
/// displaced so `detach` can put it back.
pub(crate) async fn api_local_llm_attach(
    State(state): State<ServeState>,
    Json(body): Json<AttachBody>,
) -> Response {
    if body.n_ctx == 0 {
        return err_json(StatusCode::BAD_REQUEST, "n_ctx must be non-zero");
    }
    let hub = state.local_llm.clone();

    {
        let inner_has_client = hub.lock().clients.contains_key(&body.client_id);
        if !inner_has_client {
            return err_json(
                StatusCode::BAD_REQUEST,
                "no event stream for this client_id — open /api/llm/local/events first",
            );
        }
    }

    let label = body.label.unwrap_or_else(|| body.model.clone());
    let attached = Attached {
        model: body.model.clone(),
        label,
        n_ctx: body.n_ctx,
        supports_tools: body.supports_tools,
        backend: body.backend.unwrap_or_else(|| "wasm".to_string()),
        client_id: body.client_id,
        since: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };

    let cfg = hub.bridge_config(&attached);
    {
        let mut chat = state.chat_default.write().expect("chat_default poisoned");
        let mut inner = hub.lock();
        // Only the *first* attach displaces something: re-attaching (a page
        // reload, a model switch) must not record the bridge as the thing to
        // restore, or detaching would hand chat back to itself.
        if inner.displaced.is_none() {
            let previous = chat.clone();
            let previous_is_bridge = previous.as_ref().is_some_and(|c| hub.owns(c));
            inner.displaced = Some(if previous_is_bridge { None } else { previous });
        }
        inner.attached = Some(attached.clone());
        *chat = Some(cfg);
    }

    tracing::info!(
        model = %attached.model,
        n_ctx = attached.n_ctx,
        backend = %attached.backend,
        "in-browser model attached; chat, tours and walks now run in the tab"
    );
    ok_json(status_payload(&state).to_string())
}

#[derive(Deserialize)]
pub(crate) struct DetachBody {
    #[serde(default)]
    client_id: Option<String>,
}

/// `POST /api/llm/local/detach` — the user switched the model off (or unloaded
/// it). Restores whatever chat endpoint was configured before.
pub(crate) async fn api_local_llm_detach(
    State(state): State<ServeState>,
    Json(body): Json<DetachBody>,
) -> Response {
    let hub = state.local_llm.clone();
    let restore = {
        let mut inner = hub.lock();
        match inner.attached.as_ref() {
            Some(a) if body.client_id.as_deref().is_none_or(|c| c == a.client_id) => {
                inner.attached = None;
                inner.displaced.take()
            }
            _ => None,
        }
    };
    if let Some(previous) = restore {
        restore_chat_default(&state, previous);
    }
    ok_json(status_payload(&state).to_string())
}

/// `GET /api/llm/local/status` — what the UI polls to render its panel.
pub(crate) async fn api_local_llm_status(State(state): State<ServeState>) -> Response {
    ok_json(status_payload(&state).to_string())
}

/// The shape both `/status` and `/api/capabilities` embed.
pub(crate) fn status_payload(state: &ServeState) -> Value {
    let hub = &state.local_llm;
    let attached = hub.attached();
    json!({
        "supported": true,
        "runtime": {
            "version": crate::assets::WLLAMA_VERSION,
            "js": format!("/wllama/{}/wllama.js", crate::assets::WLLAMA_VERSION),
            "wasm": format!("/wllama/{}/wllama.wasm", crate::assets::WLLAMA_VERSION),
        },
        // What the window the page chose actually buys. The page shows this
        // next to the context-window picker so the trade is visible *before* a
        // 10 000-token prompt meets a 4 000-token model.
        "limits": { "tools_min_n_ctx": tools_min_n_ctx() },
        "attached": attached.as_ref().map(|a| {
            let plan = hub.plan_prompt(true);
            json!({
                "model": a.model,
                "label": a.label,
                "n_ctx": a.n_ctx,
                "supports_tools": a.supports_tools,
                "backend": a.backend,
                "since": a.since,
                "context_chars": plan.context_chars,
                "tools": plan.schemas.as_ref().map_or(0, Vec::len),
                "tool_rounds": plan.tool_rounds,
                "ui_limits": plan.ui.map(UiLimits::to_json),
            })
        }),
    })
}

/// Smallest context window that leaves room for a toolbox *and* for what the
/// tools return. Probed rather than hardcoded: it moves when a tool's schema
/// does, and a stale number here is a promise the server then breaks.
fn tools_min_n_ctx() -> u32 {
    let probe = LocalLlm::new(0);
    for n_ctx in [2048u32, 4096, 8192, 16_384, 32_768] {
        probe.lock().attached = Some(Attached {
            model: String::new(),
            label: String::new(),
            n_ctx,
            supports_tools: true,
            backend: String::new(),
            client_id: String::new(),
            since: 0,
        });
        if probe.plan_prompt(true).schemas.is_some() {
            return n_ctx;
        }
    }
    u32::MAX
}

/// Put back the chat config the bridge displaced — unless the user changed it
/// while the tab was serving, in which case theirs wins.
fn restore_chat_default(state: &ServeState, previous: Option<ChatConfig>) {
    let hub = &state.local_llm;
    let mut chat = state.chat_default.write().expect("chat_default poisoned");
    let current_is_bridge = chat.as_ref().is_some_and(|c| hub.owns(c));
    if current_is_bridge {
        *chat = previous;
    }
}

/// `POST /api/config` rebuilds `chat_default` from flags and the config file,
/// which would quietly evict a tab that is serving. Re-apply the bridge over
/// the rebuilt config and keep the rebuilt one as what detach restores.
pub(crate) fn reapply_after_config_rebuild(state: &ServeState, rebuilt: Option<ChatConfig>) {
    let hub = state.local_llm.clone();
    let mut chat = state.chat_default.write().expect("chat_default poisoned");
    let mut inner = hub.lock();
    match inner.attached.clone() {
        Some(a) => {
            inner.displaced = Some(rebuilt);
            *chat = Some(hub.bridge_config(&a));
        }
        None => *chat = rebuilt,
    }
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    client: String,
}

/// `GET /api/llm/local/events` — the tab's job feed, and its heartbeat.
pub(crate) async fn api_local_llm_events(
    State(state): State<ServeState>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Response {
    use futures::StreamExt;

    let hub = state.local_llm.clone();
    let client_id = q.client.clone();
    let mut rx = hub.connect(client_id.clone());
    hub.notify(
        &client_id,
        SseEvent::default()
            .event("hello")
            .data(json!({ "client": client_id }).to_string()),
    );

    // Dropped when the browser disconnects — which is the only reliable
    // signal that the tab is gone, and so the only place the attachment can
    // be torn down from.
    struct Disconnect {
        state: ServeState,
        client_id: String,
    }
    impl Drop for Disconnect {
        fn drop(&mut self) {
            if let Some(previous) = self.state.local_llm.disconnect(&self.client_id) {
                restore_chat_default(&self.state, previous);
                tracing::info!(
                    client = %self.client_id,
                    "in-browser model detached (tab closed); chat restored to its previous endpoint"
                );
            }
        }
    }
    let guard = Disconnect {
        state: state.clone(),
        client_id: client_id.clone(),
    };

    // The guard lives inside the poll closure, so it is dropped exactly when
    // the response body is — which is when the browser has gone.
    let stream = futures::stream::poll_fn(move |cx| {
        let _hold = &guard;
        rx.poll_recv(cx)
    })
    .map(Ok::<_, std::convert::Infallible>);

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct DeltaBody {
    #[serde(default)]
    content: String,
}

/// `POST /api/llm/local/jobs/:id/delta` — tokens, as they are produced.
pub(crate) async fn api_local_llm_delta(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    Json(body): Json<DeltaBody>,
) -> Response {
    let delivered = state.local_llm.push(
        &id,
        JobEvent::Delta {
            content: body.content,
        },
    );
    // A job the caller already abandoned is not an error worth shouting
    // about, but the tab should learn to stop sending.
    ok_json(json!({ "ok": delivered }).to_string())
}

#[derive(Deserialize)]
pub(crate) struct ResultBody {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<Value>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// `POST /api/llm/local/jobs/:id/result` — the turn is over, one way or another.
pub(crate) async fn api_local_llm_result(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    Json(body): Json<ResultBody>,
) -> Response {
    let event = match body.error {
        Some(e) => JobEvent::Failed(e),
        None => JobEvent::Done(Box::new(Completion {
            content: body.content,
            tool_calls: body.tool_calls,
            finish_reason: body.finish_reason,
            usage: body.usage,
        })),
    };
    let delivered = state.local_llm.push(&id, event);
    ok_json(json!({ "ok": delivered }).to_string())
}

// ---------- The OpenAI-compatible endpoint ----------

/// `POST /api/llm/local/v1/chat/completions`.
///
/// Deliberately boring: whatever ug's own chat client sends to a hosted
/// provider, it sends here, and gets the same wire format back.
pub(crate) async fn api_local_llm_completions(
    State(state): State<ServeState>,
    Json(body): Json<Value>,
) -> Response {
    let stream = body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let attached = state.local_llm.attached();
    let model = attached
        .as_ref()
        .map(|a| a.model.clone())
        .unwrap_or_else(|| "local".to_string());
    let completion_cap = attached
        .as_ref()
        .map(Attached::completion_tokens)
        .unwrap_or(MAX_COMPLETION_TOKENS);

    let request = browser_request(&body, completion_cap);
    let (id, rx) = match state.local_llm.dispatch(request) {
        Ok(v) => v,
        Err(DispatchError::NotAttached) => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "no in-browser model is attached — open the UltraGraph tab and start one",
            )
        }
        Err(DispatchError::ClientGone) => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "the browser tab running the model is not connected any more",
            )
        }
    };

    if stream {
        stream_response(state, id, rx, model)
    } else {
        collect_response(state, id, rx, model).await
    }
}

/// The subset of an OpenAI request the page needs, with the ceilings a
/// browser-hosted model has to live under applied.
fn browser_request(body: &Value, completion_cap: u32) -> Value {
    let mut out = json!({
        "messages": body.get("messages").cloned().unwrap_or(json!([])),
    });
    let obj = out.as_object_mut().expect("just built");
    if let Some(t) = body.get("temperature").and_then(Value::as_f64) {
        obj.insert("temperature".into(), json!(t));
    }
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(completion_cap as u64)
        .min(completion_cap as u64);
    obj.insert("max_tokens".into(), json!(max_tokens));
    if let Some(tools) = body.get("tools").filter(|t| !t.is_null()) {
        obj.insert("tools".into(), tools.clone());
        if let Some(choice) = body.get("tool_choice") {
            obj.insert("tool_choice".into(), choice.clone());
        }
    }
    out
}

fn chatcmpl_id(job: &str) -> String {
    format!("chatcmpl-local-{job}")
}

fn created_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Non-streaming: wait for the tab to finish, then answer in one body.
async fn collect_response(
    state: ServeState,
    id: String,
    mut rx: UnboundedReceiver<JobEvent>,
    model: String,
) -> Response {
    let hub = state.local_llm.clone();
    let outcome = tokio::time::timeout(JOB_TIMEOUT, async {
        let mut buffered = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                JobEvent::Delta { content } => buffered.push_str(&content),
                JobEvent::Done(mut done) => {
                    if done.content.is_empty() {
                        done.content = buffered;
                    }
                    return Ok(*done);
                }
                JobEvent::Failed(e) => return Err(e),
            }
        }
        Err("the browser stopped answering this request".to_string())
    })
    .await;
    hub.retire(&id, true);

    match outcome {
        Ok(Ok(done)) => {
            let mut message = json!({ "role": "assistant", "content": done.content });
            if !done.tool_calls.is_empty() {
                message["tool_calls"] = json!(done.tool_calls);
            }
            ok_json(
                json!({
                    "id": chatcmpl_id(&id),
                    "object": "chat.completion",
                    "created": created_now(),
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": done.finish_reason.unwrap_or_else(|| "stop".into()),
                    }],
                    "usage": done.usage.unwrap_or(json!({})),
                })
                .to_string(),
            )
        }
        Ok(Err(e)) => err_json(StatusCode::BAD_GATEWAY, &e),
        Err(_) => err_json(
            StatusCode::GATEWAY_TIMEOUT,
            "the in-browser model did not finish in time",
        ),
    }
}

/// Streaming: translate the tab's deltas into OpenAI SSE chunks.
fn stream_response(
    state: ServeState,
    id: String,
    mut rx: UnboundedReceiver<JobEvent>,
    model: String,
) -> Response {
    let (tx, out_rx) = mpsc::unbounded_channel::<Result<SseEvent, std::convert::Infallible>>();
    let hub = state.local_llm.clone();
    let job = id.clone();

    tokio::spawn(async move {
        let send = |value: Value| {
            let _ = tx.send(Ok(SseEvent::default().data(value.to_string())));
        };
        let chunk = |delta: Value, finish: Option<&str>| {
            json!({
                "id": chatcmpl_id(&job),
                "object": "chat.completion.chunk",
                "created": created_now(),
                "model": model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        };

        send(chunk(json!({ "role": "assistant" }), None));

        let result = tokio::time::timeout(JOB_TIMEOUT, async {
            while let Some(event) = rx.recv().await {
                match event {
                    JobEvent::Delta { content } => {
                        if !content.is_empty() {
                            send(chunk(json!({ "content": content }), None));
                        }
                    }
                    JobEvent::Done(done) => return Ok(*done),
                    JobEvent::Failed(e) => return Err(e),
                }
            }
            Err("the browser stopped answering this request".to_string())
        })
        .await;

        match result {
            Ok(Ok(done)) => {
                let mut delta = json!({});
                if !done.tool_calls.is_empty() {
                    delta["tool_calls"] = json!(done.tool_calls);
                }
                let mut final_chunk = chunk(
                    delta,
                    Some(done.finish_reason.as_deref().unwrap_or("stop")),
                );
                if let Some(usage) = done.usage {
                    final_chunk["usage"] = usage;
                }
                send(final_chunk);
            }
            // A failure mid-stream cannot become an HTTP status any more —
            // the 200 went out with the first chunk. `error` is what an
            // OpenAI-compatible client is told, and ug's own client surfaces
            // the partial answer it already has.
            Ok(Err(e)) => send(json!({ "error": { "message": e } })),
            Err(_) => send(json!({
                "error": { "message": "the in-browser model did not finish in time" }
            })),
        }
        let _ = tx.send(Ok(SseEvent::default().data("[DONE]")));
        hub.retire(&job, true);
    });

    let mut out_rx = out_rx;
    let stream = futures::stream::poll_fn(move |cx| out_rx.poll_recv(cx));
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[cfg(test)]
#[path = "local_llm_tests.rs"]
mod local_llm_tests;
