//! Chat completion client + RAG orchestrator.
//!
//! Sits on top of the existing GraphRAG retrieval pipeline (`storage::search_kb`)
//! and the chat side of an OpenAI-compatible endpoint. Shared by:
//!
//! * `ug chat …` CLI command (one-shot or REPL mode)
//! * `POST /api/chat` in `ug serve` (used by the visualization UI)
//!
//! The module is intentionally backend-agnostic: any service exposing
//! `POST <base>/chat/completions` with the OpenAI v1 wire format works
//! (OpenAI, vLLM, llama.cpp, Ollama via the openai-compat shim, MLX
//! server, etc).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ultragraph::types::GraphData;
use ultragraph::storage::{
    search_kb as storage_search_kb, ContextItem, DEFAULT_CONTEXT_CHARS, Direction, Embedder,
    KnowledgeStore, RankStrategy, RankedContext, SearchKbOptions,
};

/// Default chat model. Picked so the CLI works as soon as the user
/// points `--base-url` at any OpenAI-compatible chat endpoint; the
/// caller almost always wants to override this with `--chat-model`.
pub const DEFAULT_CHAT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_CHAT_BASE_URL: &str = "http://127.0.0.1:8000/v1";
pub const DEFAULT_CHAT_API_KEY: &str = "1234";
pub const DEFAULT_TEMPERATURE: f32 = 0.2;
pub const DEFAULT_MAX_TOKENS: u32 = 32768;
/// Long enough for a deliberating turn that calls tools.
///
/// 180 s was sized for one completion from a pack. A turn that deliberates and
/// calls four tools is several completions plus the tool work between them,
/// and it blew through 180 s mid-measurement (docs/dev/RAG-EVAL.md,
/// 2026-09-20) — the request died, so the whole turn died, after two minutes
/// of work the user had already waited for.
///
/// The cost of the larger number is bounded: a *dead* endpoint fails on
/// connect and is reported immediately (`ChatError::is_unreachable`), so this
/// only governs an endpoint that accepted the request and is still thinking.
/// Waiting on that is what the user asked for.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// How many tool rounds a turn may spend before it must answer.
///
/// Was 4, chosen before anything counted how many a turn actually uses.
/// Measured 2026-09-20 (docs/dev/RAG-EVAL.md): **half the questions hit the
/// cap** — the loop was not finishing, it was being cut off, and an answer
/// written under protest looks exactly like one the model was happy with.
///
/// The ceiling is what stops a confused model looping forever, so it stays;
/// it is just no longer set below what an ordinary question needs. Rounds are
/// sequential — each is a completion the user waits through — so this is a
/// real latency ceiling too, which is why the calls *within* a round now run
/// together (`run_round_calls`).
pub const DEFAULT_TOOL_ROUNDS: usize = 8;

/// Hard ceiling on `max_tool_rounds`, whatever a request asks for.
pub const MAX_TOOL_ROUNDS: usize = 16;

/// How much of one tool's output may reach the prompt.
///
/// Was 6 000, which is a page and a half: an `analyze boundaries` listing 142
/// surfaces was cut off at `(result truncated at 6000 chars)` mid-table, and a
/// model handed half a table either answers from half a table or spends
/// another round paging for the rest. Both are worse than the tokens.
///
/// **This is a per-call cap, not a per-turn one.** Nothing yet bounds the sum,
/// so a turn that makes several large calls across several rounds can put far
/// more than this in front of the model — 8 rounds x a few calls x 60 k chars
/// is ~120 k tokens, which fits a 262 k-token window and does not fit a 32 k
/// one. A whole-turn budget is the missing piece (docs/dev/RAG-EVAL.md);
/// until it exists, lower this for a small-context model rather than assuming
/// the cap protects you.
pub const DEFAULT_TOOL_RESULT_CHARS: usize = ultragraph::agent_tools::DEFAULT_MAX_CHARS;


#[derive(Clone, Debug)]
pub struct ChatConfig {
    /// Extra top-level fields merged into the request body. Providers
    /// disagree on how you ask a reasoning model to skip deliberation
    /// (`chat_template_kwargs.enable_thinking`, `reasoning_effort`, …),
    /// so callers that care pass whatever their endpoint understands and
    /// fall back if it 400s.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            extra_body: None,
            base_url: DEFAULT_CHAT_BASE_URL.to_string(),
            api_key: DEFAULT_CHAT_API_KEY.to_string(),
            model: DEFAULT_CHAT_MODEL.to_string(),
            temperature: DEFAULT_TEMPERATURE,
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl ChatConfig {
    /// Apply optional CLI/API overrides on top of defaults. A `None`
    /// keeps the existing default — mirrors `EmbedderConfig::with_overrides`.
    pub fn with_overrides(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        timeout_secs: Option<u64>,
    ) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = base_url {
            cfg.base_url = v;
        }
        if let Some(v) = api_key {
            cfg.api_key = v;
        }
        if let Some(v) = model {
            cfg.model = v;
        }
        if let Some(v) = temperature {
            cfg.temperature = v;
        }
        if let Some(v) = max_tokens {
            cfg.max_tokens = v;
        }
        if let Some(v) = timeout_secs {
            cfg.timeout_secs = v;
        }
        cfg
    }
}

/// Request fields that ask a reasoning model to answer without
/// deliberating first.
///
/// Thinking is a property of the chat template, not the prompt: telling a
/// Qwen3-class model "don't think out loud" in the system prompt changes
/// nothing, and it will spend tens of thousands of tokens — minutes, on a
/// local box — reasoning before the first useful character. Providers
/// spell the off switch differently, so send all the common ones;
/// anything unrecognised is ignored, or triggers the retry in
/// `ChatClient::post_chat`.
pub fn no_think_body() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    // vLLM / SGLang / llama.cpp-server pass this through to the template.
    m.insert(
        "chat_template_kwargs".into(),
        serde_json::json!({ "enable_thinking": false }),
    );
    // OpenAI o-series, newer llama.cpp and LM Studio builds.
    m.insert("reasoning_effort".into(), serde_json::json!("low"));
    m
}

/// The same client with deliberation switched off. `None` when the caller
/// already set `extra_body` themselves — an explicit choice always wins.
pub fn fast_client(chat: &ChatClient) -> Option<ChatClient> {
    if chat.config().extra_body.is_some() {
        return None;
    }
    let mut cfg = chat.config().clone();
    cfg.extra_body = Some(no_think_body());
    ChatClient::new(cfg).ok()
}

/// One message on the wire. `tool_calls` / `tool_call_id` are only set on
/// the assistant and tool turns of a function-calling exchange; they stay
/// absent otherwise so plain chat requests are byte-identical to before.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
            ..Default::default()
        }
    }
}

/// One assistant turn as the provider returned it.
#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

/// A tool invocation the model asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    pub function: ToolCallFunction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallFunction {
    #[serde(default)]
    pub name: String,
    /// JSON-encoded arguments. Models emit this as a *string*, not an object.
    #[serde(default)]
    pub arguments: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    stream: bool,
    /// OpenAI function-calling tool list. Omitted entirely when empty so
    /// endpoints that don't support tools see the request they always saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [Value]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    #[serde(default)]
    pub total_tokens: Option<u32>,
}

#[derive(Debug)]
pub enum ChatError {
    Http(reqwest::Error),
    BadStatus(u16, String),
    EmptyChoices,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Http(e) => write!(f, "chat http error: {}", e),
            ChatError::BadStatus(code, body) => {
                write!(f, "chat bad status {}: {}", code, body)
            }
            ChatError::EmptyChoices => write!(f, "chat response had no choices"),
        }
    }
}

impl std::error::Error for ChatError {}

impl ChatError {
    /// Whether this is "the endpoint isn't answering" rather than "the
    /// model said no". Callers use it to offer configuration instead of
    /// printing a transport error at someone who can't act on it.
    pub fn is_unreachable(&self) -> bool {
        match self {
            ChatError::Http(e) => e.is_connect() || e.is_timeout() || e.is_request(),
            // 404 on the completions path is the other classic symptom of a
            // base URL pointing at something that isn't an OpenAI-style API.
            ChatError::BadStatus(code, _) => *code == 404,
            ChatError::EmptyChoices => false,
        }
    }
}

/// Minimal client for OpenAI-compatible `/v1/chat/completions`.
pub struct ChatClient {
    cfg: ChatConfig,
    client: reqwest::Client,
}

impl ChatClient {
    pub fn new(cfg: ChatConfig) -> Result<Self, ChatError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(ChatError::Http)?;
        Ok(Self { cfg, client })
    }

    pub fn config(&self) -> &ChatConfig {
        &self.cfg
    }

    /// Serialize a request, folding in `extra_body` when present.
    fn request_body(&self, req: &ChatRequest<'_>) -> serde_json::Value {
        let mut v = serde_json::to_value(req).unwrap_or_else(|_| serde_json::json!({}));
        if let (Some(extra), Some(obj)) = (self.cfg.extra_body.as_ref(), v.as_object_mut()) {
            for (k, val) in extra {
                obj.insert(k.clone(), val.clone());
            }
        }
        v
    }

    /// POST a request body, retrying once without `extra_body` if the
    /// endpoint rejects it. The extras are optimisations (see
    /// [`no_think_body`]), never requirements, so a provider that refuses
    /// unknown fields must still get a working request.
    ///
    /// [`no_think_body`]: no_think_body
    async fn post_chat(
        &self,
        url: &str,
        req: &ChatRequest<'_>,
    ) -> Result<reqwest::Response, ChatError> {
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.cfg.api_key)
            .json(&self.request_body(req))
            .send()
            .await
            .map_err(ChatError::Http)?;
        if resp.status().is_client_error() && self.cfg.extra_body.is_some() {
            tracing::debug!(
                status = resp.status().as_u16(),
                "chat: endpoint rejected the request; retrying without extra_body"
            );
            return self
                .client
                .post(url)
                .bearer_auth(&self.cfg.api_key)
                .json(&serde_json::to_value(req).unwrap_or_else(|_| serde_json::json!({})))
                .send()
                .await
                .map_err(ChatError::Http);
        }
        Ok(resp)
    }

    /// Non-streaming round-trip. See [`complete_with_reason`] when the
    /// caller needs to know *why* the model stopped (e.g. to tell a
    /// truncated reply apart from a badly formatted one).
    ///
    /// [`complete_with_reason`]: ChatClient::complete_with_reason
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
    ) -> Result<(String, Option<Usage>), ChatError> {
        let (text, usage, _) = self.complete_with_reason(messages).await?;
        Ok((text, usage))
    }

    /// As [`complete`], plus the provider's `finish_reason` (`"stop"`,
    /// `"length"`, …) when it sends one.
    ///
    /// [`complete`]: ChatClient::complete
    pub async fn complete_with_reason(
        &self,
        messages: &[ChatMessage],
    ) -> Result<(String, Option<Usage>, Option<String>), ChatError> {
        let out = self.complete_raw(messages, None).await?;
        Ok((out.content, out.usage, out.finish_reason))
    }

    /// The full assistant turn, including any tools it wants called. `tools`
    /// is the OpenAI function-calling schema list; pass `None` for a plain
    /// completion.
    pub async fn complete_raw(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<Completion, ChatError> {
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let req = ChatRequest {
            model: &self.cfg.model,
            messages,
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
            stream: false,
            tools,
            tool_choice: tools.map(|_| "auto"),
        };

        let resp = self.post_chat(&url, &req).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChatError::BadStatus(status.as_u16(), body));
        }

        let parsed: ChatResponse = resp.json().await.map_err(ChatError::Http)?;
        let choice = parsed.choices.into_iter().next().ok_or(ChatError::EmptyChoices)?;
        Ok(Completion {
            content: choice.message.content.unwrap_or_default(),
            reasoning: choice.message.reasoning_content.unwrap_or_default(),
            tool_calls: choice.message.tool_calls.unwrap_or_default(),
            usage: parsed.usage,
            finish_reason: choice.finish_reason,
        })
    }

    /// Streaming round-trip (`stream: true`, SSE wire format). Calls
    /// `on_delta` for every incremental piece as it arrives and returns
    /// the fully accumulated `(content, reasoning, usage)` at the end.
    ///
    /// Provider quirks handled here so callers don't have to:
    /// * a 200 with a plain JSON body (provider silently ignored
    ///   `stream: true`) is accepted and emitted as one big delta;
    /// * `delta.reasoning_content` / `delta.reasoning` (DeepSeek-R1 /
    ///   OpenRouter style) are surfaced separately from `delta.content`;
    /// * a non-2xx status comes back as `ChatError::BadStatus` — callers
    ///   fall back to the non-streaming `complete()` on that.
    pub async fn complete_stream<F>(
        &self,
        messages: &[ChatMessage],
        mut on_delta: F,
    ) -> Result<(String, String, Option<Usage>), ChatError>
    where
        F: FnMut(StreamDelta),
    {
        use futures::StreamExt;

        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let req = ChatRequest {
            model: &self.cfg.model,
            messages,
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
            stream: true,
            tools: None,
            tool_choice: None,
        };

        let resp = self.post_chat(&url, &req).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChatError::BadStatus(status.as_u16(), body));
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);
        if !is_sse {
            // Provider ignored `stream: true` and sent one JSON body.
            let parsed: ChatResponse = resp.json().await.map_err(ChatError::Http)?;
            let choice = parsed
                .choices
                .into_iter()
                .next()
                .ok_or(ChatError::EmptyChoices)?;
            let text = choice.message.content.unwrap_or_default();
            on_delta(StreamDelta {
                content: Some(text.clone()),
                finish_reason: choice.finish_reason,
                usage: parsed.usage.clone(),
                ..Default::default()
            });
            return Ok((text, String::new(), parsed.usage));
        }

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage: Option<Usage> = None;
        let mut buf = String::new();
        let mut body = resp.bytes_stream();
        'outer: while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(ChatError::Http)?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                match parse_sse_line(line.trim_end()) {
                    SseLine::Done => break 'outer,
                    SseLine::Skip => {}
                    SseLine::Delta(d) => {
                        if let Some(c) = &d.content {
                            content.push_str(c);
                        }
                        if let Some(r) = &d.reasoning {
                            reasoning.push_str(r);
                        }
                        if d.usage.is_some() {
                            usage = d.usage.clone();
                        }
                        on_delta(d);
                    }
                }
            }
        }
        Ok((content, reasoning, usage))
    }
}

/// One incremental piece of a streaming completion.
#[derive(Debug, Default, Clone)]
pub struct StreamDelta {
    pub content: Option<String>,
    /// Chain-of-thought text some providers stream separately
    /// (`delta.reasoning_content` / `delta.reasoning`).
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

enum SseLine {
    Delta(StreamDelta),
    Done,
    Skip,
}

// SSE `delta` payloads, OpenAI wire format plus the two common
// reasoning-field dialects.
#[derive(Deserialize)]
struct StreamResp {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDeltaMsg>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDeltaMsg {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

/// Parse one SSE line: `data: [DONE]` ends the stream, `data: {json}`
/// carries a delta, everything else (blank lines, comments, `event:`
/// fields, unparseable payloads) is skipped — mid-stream garbage
/// shouldn't kill an otherwise fine completion.
fn parse_sse_line(line: &str) -> SseLine {
    let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
        return SseLine::Skip;
    };
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    let Ok(parsed) = serde_json::from_str::<StreamResp>(payload) else {
        return SseLine::Skip;
    };
    let mut out = StreamDelta {
        usage: parsed.usage,
        ..Default::default()
    };
    if let Some(choice) = parsed.choices.into_iter().next() {
        out.finish_reason = choice.finish_reason;
        if let Some(delta) = choice.delta {
            out.content = delta.content.filter(|s| !s.is_empty());
            out.reasoning = delta
                .reasoning_content
                .or(delta.reasoning)
                .filter(|s| !s.is_empty());
        }
    }
    if out.content.is_none()
        && out.reasoning.is_none()
        && out.finish_reason.is_none()
        && out.usage.is_none()
    {
        return SseLine::Skip;
    }
    SseLine::Delta(out)
}

// ---------- Prompt assembly ----------

/// System prompt used by both the CLI and `ug serve`. Tells the model
/// to ground itself in the retrieved context and cite by `[#N]`.
/// The prompt for a turn that **can** go looking.
///
/// What it was given and how to cite it — and nothing telling it to stay put.
pub const SYSTEM_CORE: &str = "You are UltraGraph, a precise code/knowledge assistant. \
You are given retrieved context items numbered [#1], [#2], ... drawn from a knowledge graph + vector \
store over the user's repository. Cite the supporting items inline using their bracketed numbers \
(e.g. \"see [#2]\"). Prefer concise, structured answers with code references and file paths when \
relevant.";

/// The prompt for a turn that **cannot**: [`SYSTEM_CORE`] plus the two
/// closed-book sentences.
///
/// Those two sentences are right when there are no tools and actively wrong
/// when there are, and they are the ones the model obeys: they arrive first
/// and they are unambiguous, while [`TOOL_SYSTEM_SUFFIX`] arrives after and
/// asks for a judgment call. Measured over 12 questions
/// (`docs/dev/RAG-EVAL.md`, 2026-09-20): with these included the model made
/// **zero** tool calls across all twelve and answered each from the first
/// pack, which was wrong three times in four. "Say so plainly instead of
/// guessing" is a virtue when looking is impossible and a failure when it is
/// one call away.
///
/// `prompt_starts_from_the_same_core` pins the two to each other.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are UltraGraph, a precise code/knowledge assistant. \
You are given retrieved context items numbered [#1], [#2], ... drawn from a knowledge graph + vector \
store over the user's repository. Cite the supporting items inline using their bracketed numbers \
(e.g. \"see [#2]\"). Prefer concise, structured answers with code references and file paths when \
relevant. Answer the user's question using ONLY information present in those items when possible. \
If the answer is not in the context, say so plainly instead of guessing.";

/// Appended to the system prompt when the model has the graph toolbox.
///
/// The base prompt tells it to answer from the retrieved items — which on
/// its own reads as "don't go looking", and the model duly doesn't. This
/// says the opposite where it matters: retrieval is a starting point, and
/// anything specific should be checked against the graph itself.
pub const TOOL_SYSTEM_SUFFIX: &str = "\n\nYou ALSO have tools over the same knowledge graph, and \
the retrieved items above are only a starting point — they are the neighbourhood of the question, \
not the whole answer. Call tools before answering whenever the question asks for something the \
items don't already show completely:\n\
- `find_usages` for \"what calls / uses X\" — the items rarely contain every call site.\n\
- `get_code` to read a symbol's exact source before describing or quoting it.\n\
- `file_context` to see everything a file declares, plus what imports and tests it.\n\
- `find_symbols` to resolve a name you were given into a real node id.\n\
- `search` to widen the net when the items look thin or off-topic.\n\
- `shortest_path` / `traverse` to show how two things connect.\n\n\
The items above were retrieved from the user's wording alone, and what they are is the \
NEIGHBOURHOOD of the question — the right file, the right module, the functions next to the answer. \
Being about the right area is not the same as containing the answer, and mistaking one for the other \
is the single most common way this goes wrong: eight plausible items from exactly the right file, \
none of which is the thing being asked about. A pack that looks good is not evidence that it is.\n\n\
So before answering, name to yourself the specific symbol, file or behaviour the question is about, \
and check that one of the items above actually IS that thing. If none of them is — even when they all \
look relevant — do not answer from the nearest miss. REWRITE the query in the vocabulary the codebase \
uses and call `search` again, or go straight to `find_symbols` with a wildcard for the name you expect \
the thing to have. Each item says how it was reached (`semantic`/`keyword` match, and how many hops \
out it is); items that are all several hops out are a neighbourhood, not an answer. Searching two or \
three times with better wording is normal and expected.\n\n\
Pass arguments as real JSON, not JSON inside a string: `\"nodeId\": \"function:src/a.rs:1:foo\"` for \
one id, `\"nodeId\": [\"id1\", \"id2\"]` for several. Never `\"nodeId\": \"[\\\"id1\\\"]\"`.\n\n\
Prefer one or two well-aimed calls over guessing. A `search` result is numbered in the SAME [#N] run \
as the items above — it continues the list rather than restarting it, so cite what a search found by \
its own number exactly as you cite the items above. The other graph tools do the same: every node they \
print carries its own [#N] beside its id, and a node you used to answer should be cited by that number \
rather than merely described. If the items already answer the question completely, just answer.";

/// One `[#n]` block: the header line, the description, the snippet.
fn render_item(n: usize, item: &ContextItem) -> String {
    let file = if item.file.is_empty() { "<unknown>" } else { item.file.as_str() };
    let where_ = if item.start_line > 0 && item.end_line >= item.start_line {
        format!("{}:{}-{}", file, item.start_line, item.end_line)
    } else {
        file.to_string()
    };
    // How this item was reached, not just that it was. A pack of eight
    // neighbours-of-neighbours looks identical to a pack of eight direct
    // matches unless it says so, and the model is asked to tell them apart.
    let how = match (item.matched_by.as_str(), item.hop) {
        ("", 0) => String::new(),
        ("", h) => format!(" · {h} hop(s) out"),
        (m, 0) => format!(" · {m} match"),
        (m, h) => format!(" · {m}, {h} hop(s) out"),
    };
    let header = format!("[#{}] {} ({}) — {}{}", n, item.name, item.node_type, where_, how);

    let mut block = String::with_capacity(header.len() + 256);
    block.push_str(&header);
    block.push('\n');
    if !item.description.is_empty() {
        block.push_str(item.description.trim());
        block.push('\n');
    }
    if let Some(snippet) = item.snippet.as_ref() {
        if !snippet.is_empty() {
            block.push_str("```\n");
            block.push_str(snippet.trim_end_matches('\n'));
            block.push_str("\n```\n");
        }
    }
    block.push('\n');
    block
}

/// The one `[#N]` namespace a turn has.
///
/// Numbering used to restart at `[#1]` on every call, and a turn renders at
/// least twice the moment the model takes the suffix's advice and searches
/// again: once for the seed pack, once per `search` result. Both blocks then
/// claimed `[#1]`, `[#2]`, `[#3]`; the model cited a number that meant two
/// different nodes, and the citation the reader clicked was whichever one the
/// seed pack happened to have put there. **The failure got worse the better
/// the model behaved** — only a turn that re-searched could hit it.
///
/// The ledger hands each node a number once per turn and remembers it, so a
/// re-search extends the evidence list instead of overwriting it, and what
/// the reader is shown is everything the answer was actually allowed to cite.
#[derive(Default)]
pub struct CitationLedger {
    items: Vec<ContextItem>,
    /// node id → zero-based position in `items`.
    seen: HashMap<String, usize>,
}

impl CitationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything cited so far, in `[#1]`, `[#2]`, … order.
    pub fn items(&self) -> &[ContextItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Start a new turn. The REPL carries history across turns but rebuilds
    /// the context block each time, so numbers have to restart with it —
    /// otherwise turn two opens at `[#9]` with `[#1]`..`[#8]` nowhere in
    /// sight.
    pub fn reset(&mut self) {
        self.items.clear();
        self.seen.clear();
    }

    /// Give a node this turn's next number without rendering a block for it.
    ///
    /// For a node a tool has already displayed in its own format: the number
    /// is what makes it citable, and re-printing the node underneath the
    /// tool's own output would just say everything twice.
    pub fn number_for(&mut self, item: &ContextItem) -> usize {
        match self.seen.get(&item.id) {
            Some(&i) => i + 1,
            None => {
                self.seen.insert(item.id.clone(), self.items.len());
                self.items.push(item.clone());
                self.items.len()
            }
        }
    }

    /// Render items for the prompt under this turn's numbering.
    ///
    /// `max_chars` is a soft cap across the assembled block — once exceeded
    /// the remaining items are dropped, because head-truncation would split
    /// a snippet mid-token and the model handles that worse than simply not
    /// being shown the lowest-ranked items.
    ///
    /// A node already cited keeps the number it has — the model gets told
    /// "this is [#3] again" rather than a second name for the same thing,
    /// which is also what stops a re-search that returns the same neighbourhood
    /// from doubling the citation list.
    ///
    /// **Only what is rendered is registered.** `max_chars` drops the tail,
    /// and an item the model was never shown must not turn up in the list of
    /// sources the reader is told the answer rests on.
    pub fn render(&mut self, items: &[ContextItem], max_chars: usize) -> String {
        let mut out = String::with_capacity(items.len() * 256);
        for item in items {
            let existing = self.seen.get(&item.id).copied();
            let n = existing.unwrap_or(self.items.len());
            let block = render_item(n + 1, item);
            if !out.is_empty() && out.len() + block.len() > max_chars {
                break;
            }
            if existing.is_none() {
                self.seen.insert(item.id.clone(), n);
                self.items.push(item.clone());
            }
            out.push_str(&block);
        }
        out
    }
}

/// Build the standard prompt (system + RAG context + user query).
///
/// The seed pack registers into `ledger` first, so it owns `[#1]` upward and
/// anything a tool retrieves later continues the same run of numbers.
pub fn build_rag_messages(
    query: &str,
    context: &RankedContext,
    history: &[ChatMessage],
    system_prompt: Option<&str>,
    ctx_max_chars: usize,
    ledger: &mut CitationLedger,
    no_seed: bool,
) -> Vec<ChatMessage> {
    let system = system_prompt.unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let mut msgs: Vec<ChatMessage> = Vec::with_capacity(history.len() + 3);

    msgs.push(ChatMessage::new("system", system));

    let rendered = ledger.render(&context.items, ctx_max_chars);
    let preface = if rendered.is_empty() {
        // Two different empty states, and the model must not confuse them:
        // "we looked and found nothing" is a fact about the repository,
        // "we have not looked yet" is an instruction to go and look. The
        // caller says which by whether it asked for a seed pass at all.
        if no_seed {
            "No context has been retrieved yet — that is deliberate, not a finding. \
             Search for what you need before answering; do not report the repository as empty."
                .to_string()
        } else {
            "No retrieved context was found for this query.".to_string()
        }
    } else {
        format!(
            "Retrieved context (cite as [#N]):\n\n{}\n---",
            rendered.trim_end()
        )
    };
    msgs.push(ChatMessage::new("system", preface));

    // Prior turns (already in role/content shape).
    for m in history {
        msgs.push(m.clone());
    }

    msgs.push(ChatMessage::new("user", query.to_string()));

    msgs
}

// ---------- Orchestrator ----------

/// One pass of "retrieve → prompt → answer". Used by both the CLI and
/// the HTTP layer so the behaviour is identical regardless of entry
/// point. Returns the answer text, the retrieval result, and timing /
/// usage info so callers can surface latency and token counts.
/// What a turn spent, and what the same evidence would have cost read whole.
///
/// The comparison is deliberately narrow, because a broad one would not be
/// true. `whole_file_chars` is the size on disk of exactly the files this
/// turn's citations came from — it is what an agent pays when it locates the
/// right files and opens them, which is the realistic alternative to this
/// pipeline, not a strawman that reads the repo. It is **not** a claim about
/// any particular other RAG system, and it says nothing about answer quality.
///
/// Files the index knows but the working tree no longer has are skipped, so
/// `files` is what was actually measured rather than what was cited.
#[derive(Clone, Debug, Default)]
pub struct TurnCost {
    /// The retrieved pack put into the prompt.
    pub context_chars: usize,
    /// The system prompt, tool suffix included.
    ///
    /// Fixed overhead — the same for every question — and re-sent on every
    /// round, which is most of why the endpoint's billed total dwarfs the
    /// evidence. Unlabelled it just looked like the estimate being wrong.
    pub system_chars: usize,
    /// The tool schemas as sent, when a toolbox was attached. The largest
    /// single fixed cost of an agentic turn and the one nothing showed.
    pub schema_chars: usize,
    /// Everything the tools returned, after clipping.
    pub tool_chars: usize,
    /// The answer itself.
    pub answer_chars: usize,
    /// Distinct files behind the citations, that exist on disk.
    pub files: usize,
    /// Those files, whole.
    pub whole_file_chars: u64,
}

impl TurnCost {
    /// What reached the model on this turn's behalf: pack plus tool output.
    /// Excludes the system prompt, the history and the tool schemas, which
    /// are the same whatever the question is.
    pub fn sent_chars(&self) -> usize {
        self.context_chars + self.tool_chars
    }
}

/// The turn's token bill, as the UI and the CLI both report it.
///
/// Chars are facts; tokens are an estimate from one shared constant
/// (`limits::est_tokens`) because `ug` has no tokenizer for an arbitrary
/// endpoint. `whole_files` is what the same evidence costs read whole — see
/// `chat::TurnCost` for exactly what that claims and what it does not.
pub fn cost_json(c: &TurnCost) -> serde_json::Value {
    let est = crate::limits::est_tokens;
    let sent = c.sent_chars();
    let whole = c.whole_file_chars as usize;
    serde_json::json!({
        "context_tokens": est(c.context_chars),
        "tool_tokens": est(c.tool_chars),
        "answer_tokens": est(c.answer_chars),
        "sent_tokens": est(sent),
        // Fixed per turn and re-sent every round: the same for every question,
        // and together usually larger than the evidence.
        "system_tokens": est(c.system_chars),
        "schema_tokens": est(c.schema_chars),
        "fixed_tokens": est(c.system_chars + c.schema_chars),
        "whole_files": c.files,
        "whole_file_tokens": est(whole),
        // Only when there is something to compare: no files measured means
        // no claim, rather than a division that invents one.
        "saved_ratio": if sent > 0 && whole > 0 {
            Some(((whole as f64 / sent as f64) * 10.0).round() / 10.0)
        } else {
            None
        },
        "estimated": true,
    })
}

pub struct ChatRagOutcome {
    /// How many tool calls the model made getting to this answer.
    pub tool_calls: usize,
    /// How many tool rounds it used, and whether it ran out. `Some(true)`
    /// means the cap stopped it — an answer written under protest.
    pub tool_rounds: usize,
    pub hit_round_cap: bool,
    /// What this turn cost, and the read-it-whole comparison.
    pub cost: TurnCost,
    pub answer: String,
    /// Everything the answer was allowed to cite, in `[#1]`, `[#2]`, … order:
    /// the seed pack plus whatever the model's own searches added. `context`
    /// below is the *first* retrieval alone — it still carries `seed_id` and
    /// the retrieval timing, but it is not the evidence list once a turn has
    /// searched again.
    pub citations: Vec<ContextItem>,
    /// Separately-streamed chain-of-thought text, when the provider
    /// sends one (`reasoning_content`). Empty for providers that inline
    /// it in `answer` as `<think>` tags — callers handle those.
    pub reasoning: String,
    pub context: RankedContext,
    pub retrieval_ms: u128,
    pub completion_ms: u128,
    pub usage: Option<Usage>,
}

/// A tool the model may call, plus the code that runs it.
///
/// The schemas come from the MCP registry (`mcp::tools`) so an agent
/// talking to `ug` over MCP and the model behind `/api/chat` see exactly
/// the same toolbox — one place to describe a tool, two ways to reach it.
pub struct ToolBox<'a> {
    /// OpenAI function-calling schemas, as sent in `tools`.
    pub schemas: Vec<Value>,
    /// Runs one call. Errors come back as text for the model to read;
    /// a failed tool call should teach it, not abort the turn.
    pub run: &'a (dyn Fn(&str, Value) -> futures::future::BoxFuture<'static, Result<String, String>>
             + Send
             + Sync),
    /// Cap on tool rounds, so a confused model can't loop forever.
    pub max_rounds: usize,
    /// Cap on how much one tool's output may add to the prompt.
    pub max_result_chars: usize,
}

/// Run the `search` tool (or its `semantic_search` alias) for a chat toolbox.
///
/// The two embedding-backed tools, once, for every transport: `ug chat` and
/// `/api/chat` both offer the model the same schemas and both hold an open
/// store, so both were carrying their own copy of this — the arrangement that
/// let `analyze` work over MCP and fail in chat. Graph-backed tools stay
/// with their caller, which is where the graph lives.
/// Run one chat-advertised tool and return its Markdown.
///
/// `ug chat` and `POST /api/chat` both land here, so a tool call answers the
/// same way in the terminal and the browser. They differ only in where the
/// graph and store come from, which is why those arrive as parameters.
///
/// The order matters: coerce the model's stringified arrays/numbers first,
/// then refuse anything chat is not allowed to reach, and only then dispatch.
/// The two search tools and `analyze` need the vector store, so they are wired
/// to the caller's already-open handles; everything else answers from the
/// loaded graph via `agent_tools::run_tool`.
#[allow(clippy::too_many_arguments)]
pub async fn run_chat_tool(
    name: &str,
    args: Value,
    graph: &ultragraph::types::GraphData,
    graph_path: &std::path::Path,
    repo_root: &std::path::Path,
    store: &dyn KnowledgeStore,
    embedder: Option<&Embedder>,
    ledger: &Mutex<CitationLedger>,
) -> Result<String, String> {
    use ultragraph::agent_tools;

    let mut args = args;
    crate::mcp::tools::normalize_args(name, &mut args);
    if crate::mcp::tools::CHAT_TOOL_DENYLIST.contains(&name) {
        return Err(format!("{} is not available from chat", name));
    }

    match name {
        "search" | "semantic_search" => {
            run_search_tool(name, &args, store, embedder, repo_root, ledger).await
        }
        // Statistics come from the store's indexed properties, not the graph —
        // the one thing `agent_tools::run_tool` cannot answer.
        "analyze" => crate::mcp::run_analyze_json(store, &args).await,
        // Reads graph.json and git; needs neither the store nor the
        // embedder, so it answers "what did I just change" even on a
        // project that was never ingested.
        "walk" => crate::mcp::run_walk_tool(&args, graph, repo_root).await,
        _ => {
            crate::mcp::tools::reject_if_not_graph_backed(name)?;
            // Chat already holds this project's store open, so the source
            // pre-fetch is one lookup rather than another open.
            let indexed = agent_tools::IndexedSource::load(
                store,
                &agent_tools::source_node_ids(name, graph, &args),
            )
            .await;
            let out = agent_tools::run_tool(
                name,
                graph,
                agent_tools::SourceCtx::new(&indexed, repo_root),
                graph_path,
                args,
                Some(agent_tools::Render::Markdown),
            )?;
            let text = match out {
                agent_tools::ToolOutput::Text(t) => t,
                agent_tools::ToolOutput::Json(v) => {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                }
            };
            Ok(cite_tool_nodes(&text, graph, ledger, name))
        }
    }
}

/// Make the nodes a tool showed citable.
///
/// Every graph-backed tool prints `id: <node id>` for each node it returns,
/// and those were the one kind of evidence an answer could use and not cite:
/// only `search` registered into the ledger, so an answer reached through
/// `find_usages` named its symbol while pointing at eight unrelated sources.
/// Measured 2026-09-20 (docs/dev/RAG-EVAL.md): one question in twelve answered
/// correctly with the answer's own subject absent from its citation list.
///
/// Each id that resolves against the graph takes this turn's next number,
/// appended to the line the model is already reading. An id that does not
/// resolve is left exactly as it was — a tool may print ids for nodes this
/// graph snapshot no longer has, and inventing a citation for one is worse
/// than omitting it.
///
/// Cost: one pass over the graph's nodes per tool call that printed any id,
/// and none at all for a tool that printed none (`analyze`, `graph_schema`).
/// Not one pass per id — that is the shape that would hurt on a 500k-node
/// graph (Agents.md §1a).
fn cite_tool_nodes(md: &str, graph: &GraphData, ledger: &Mutex<CitationLedger>, how: &str) -> String {
    let wanted: HashSet<&str> = md
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("id: "))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect();
    if wanted.is_empty() {
        return md.to_string();
    }

    let mut numbers: HashMap<&str, usize> = HashMap::new();
    {
        let mut l = ledger.lock().expect("citation ledger poisoned");
        for node in graph.nodes.iter().filter(|n| wanted.contains(n.id.as_str())) {
            let item = ContextItem {
                id: node.id.clone(),
                name: node.name.clone(),
                node_type: node.node_type.as_str().to_string(),
                file: node.file.clone().unwrap_or_default(),
                start_line: node.start_line.unwrap_or(0),
                end_line: node.end_line.unwrap_or(0),
                description: node.docstring.clone().unwrap_or_default(),
                distance: 0.0,
                hop: 0,
                snippet: None,
                // How it was reached is the tool's name: more use to a reader
                // than "semantic", and true.
                matched_by: how.to_string(),
            };
            numbers.insert(node.id.as_str(), l.number_for(&item));
        }
    }
    if numbers.is_empty() {
        return md.to_string();
    }

    let mut out = String::with_capacity(md.len() + numbers.len() * 8);
    for line in md.lines() {
        out.push_str(line);
        if let Some(id) = line.trim_start().strip_prefix("id: ") {
            if let Some(n) = numbers.get(id.trim()) {
                out.push_str(&format!("  [#{n}]"));
            }
        }
        out.push('\n');
    }
    out
}

pub async fn run_search_tool(
    name: &str,
    args: &Value,
    store: &dyn KnowledgeStore,
    embedder: Option<&Embedder>,
    repo_root: &std::path::Path,
    ledger: &Mutex<CitationLedger>,
) -> Result<String, String> {
    let embedder = embedder.ok_or("no embedder configured — semantic tools are offline")?;
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or_default().trim();
    if query.is_empty() {
        return Err("query is required".into());
    }
    let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 25) as usize;

    let mut opts = SearchKbOptions::new(query, repo_root);
    opts.k = k;
    opts.hops = args.get("hops").and_then(|v| v.as_u64()).unwrap_or(2).min(4) as u32;
    // `semantic_search` is the retired alias for `search` with expansion
    // off; an explicit `expand` still wins, so a model that names the old
    // tool but asks for expansion gets it.
    opts.expand = args
        .get("expand")
        .and_then(|v| v.as_bool())
        .unwrap_or(name != "semantic_search");
    opts.include_snippets = true;
    // Retrieve to the same budget the result is allowed to occupy. These were
    // both 6 000 and had to move together: raising only the clip leaves the
    // *retrieval* trimming items the clip would have let through, and raising
    // only the clip's ceiling changes nothing for this tool at all.
    opts.max_chars = DEFAULT_TOOL_RESULT_CHARS;
    let ctx = storage_search_kb(store, embedder, opts)
        .await
        .map_err(|e| e.to_string())?;
    // Numbered by the turn's ledger, not from [#1] again: this block and the
    // seed pack share one conversation, so they have to share one namespace.
    // The lock is held across the render and nothing else — a guard alive
    // across an `await` would make this future `!Send`.
    let rendered = {
        let mut l = ledger.lock().expect("citation ledger poisoned");
        l.render(&ctx.items, DEFAULT_TOOL_RESULT_CHARS)
    };
    Ok(rendered)
}

/// What happened during a tool-calling exchange, for progress reporting.
#[derive(Clone, Debug)]
pub struct ToolEvent {
    pub name: String,
    /// Compact one-line rendering of the arguments.
    pub args: String,
    /// The arguments as sent, pretty-printed.
    pub args_json: String,
    /// `None` while the call is running, `Some(summary)` once it returned.
    pub summary: Option<String>,
    /// What the tool returned — the same text the model was handed, so a
    /// user can check the answer against its evidence.
    pub result: Option<String>,
}

/// What a tool-calling exchange left behind for the final turn.
pub struct ToolRounds {
    /// The conversation to send for the final answer.
    pub messages: Vec<ChatMessage>,
    /// What the rounds cost.
    pub usage: Option<Usage>,
    /// How many tools actually ran.
    pub calls: usize,
    /// How many rounds it used. Equal to `max_rounds` means the loop was cut
    /// off rather than finished — which reads identically to "it was done"
    /// unless the two are counted separately.
    pub rounds: usize,
    /// Characters every tool result added to the prompt, after clipping.
    pub result_chars: usize,
    /// The answer a round produced *instead* of calling a tool, when one
    /// did. See [`run_tool_rounds`] — the caller must use this rather than
    /// asking for the same answer a second time.
    pub answer: Option<Completion>,
}

/// Run the model's tool calls until it answers in prose.
///
/// Tool rounds are deliberately non-streaming: partial `tool_calls` deltas
/// are the messiest part of the OpenAI wire format, and the rounds produce
/// no user-visible text anyway. The caller streams the *final* answer.
/// Returns the messages to send for that final turn, plus the usage the
/// rounds cost.
///
/// **A round that answers instead of calling a tool has already written the
/// whole answer**, and it comes back in [`ToolRounds::answer`]. This used to
/// be thrown away so the caller could redo it streamed, which made every
/// tool-less turn generate its answer twice — the single largest avoidable
/// cost in a chat turn, and pure decode time on a local endpoint (§5). One
/// measured turn against a 161k-node graph: 17.1 s writing the draft, then
/// 9.8 s streaming the same answer again. Reusing the draft ends the turn at
/// the moment the old code was only starting to show its first token.
pub async fn run_tool_rounds<F>(
    chat: &ChatClient,
    toolbox: &ToolBox<'_>,
    mut messages: Vec<ChatMessage>,
    mut on_event: F,
) -> Result<ToolRounds, ChatError>
where
    F: FnMut(ToolEvent),
{
    let mut usage: Option<Usage> = None;
    let mut calls = 0usize;
    let mut rounds = 0usize;
    let mut result_chars = 0usize;
    for _ in 0..toolbox.max_rounds {
        rounds += 1;
        let out = chat.complete_raw(&messages, Some(&toolbox.schemas)).await?;
        usage = merge_usage(usage, out.usage.clone());
        if out.tool_calls.is_empty() {
            // The model answered instead of calling a tool. Hand that answer
            // back rather than paying to generate it again; an empty one is
            // no answer at all, so that still falls through to the caller.
            let answer = (!out.content.trim().is_empty()).then_some(out);
            // It stopped on its own: the round that answers is not one it spent.
            rounds -= 1;
            return Ok(ToolRounds { messages, usage, calls, rounds, result_chars, answer });
        }

        // Record the assistant turn verbatim; providers reject tool results
        // that don't follow the call that asked for them.
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: out.content.clone(),
            tool_calls: Some(out.tool_calls.clone()),
            ..Default::default()
        });

        // A round's calls are independent of each other — the model asked for
        // all of them before seeing any answer — so they run together. This
        // is not speculative: measured 2026-09-20, a round averages ~2.3 calls
        // and the worst turn made 9, each one a graph query the user waits
        // through in series (docs/dev/RAG-EVAL.md).
        let prepared: Vec<(Value, String, String)> = out
            .tool_calls
            .iter()
            .map(|call| {
                let args: Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(Value::Object(Default::default()));
                let arg_line = compact_args(&args);
                let args_json = serde_json::to_string_pretty(&args).unwrap_or_default();
                (args, arg_line, args_json)
            })
            .collect();

        // Every call announced before any of them runs: the progress feed is
        // what tells the user why the wait is long, and it should show the
        // whole round rather than revealing it one call at a time.
        for (call, (_, arg_line, args_json)) in out.tool_calls.iter().zip(&prepared) {
            on_event(ToolEvent {
                name: call.function.name.clone(),
                args: arg_line.clone(),
                args_json: args_json.clone(),
                summary: None,
                result: None,
            });
        }

        let results = run_round_calls(
            toolbox.run,
            out.tool_calls
                .iter()
                .zip(&prepared)
                .map(|(call, (args, ..))| (call.function.name.clone(), args.clone()))
                .collect(),
        )
        .await;

        // Recorded in the order the model asked, not the order they finished:
        // providers reject tool results that do not line up with the calls.
        for ((call, (_, arg_line, args_json)), result) in
            out.tool_calls.iter().zip(&prepared).zip(results)
        {
            calls += 1;
            let (text, summary) = match result {
                Ok(t) => {
                    // Tokens, not lines: what this result costs is what the
                    // reader is deciding about, and a line of a table and a
                    // line of source are not the same price.
                    let tokens = crate::limits::est_tokens(t.chars().count());
                    (t, format!("~{} tokens", fmt_thousands(tokens)))
                }
                Err(e) => (format!("Tool error: {}", e), format!("failed: {}", e)),
            };
            let text = clip_tool_result(&text, toolbox.max_result_chars);
            result_chars += text.chars().count();
            on_event(ToolEvent {
                name: call.function.name.clone(),
                args: arg_line.clone(),
                args_json: args_json.clone(),
                summary: Some(summary),
                result: Some(text.clone()),
            });
            messages.push(ChatMessage {
                role: "tool".into(),
                content: text,
                tool_call_id: Some(call.id.clone()),
                name: Some(call.function.name.clone()),
                ..Default::default()
            });
        }
    }
    // Out of rounds: tell the model to answer with what it has.
    messages.push(ChatMessage::new(
        "user",
        "You have used all available tool calls. Answer now with what you have.",
    ));
    Ok(ToolRounds { messages, usage, calls, rounds, result_chars, answer: None })
}

/// Run one round's tool calls concurrently, returning their results **in the
/// order they were asked for**.
///
/// The ordering is not a nicety: a provider rejects a `tool` message whose
/// `tool_call_id` does not follow the assistant turn that requested it, so
/// completion order cannot be allowed to leak into the transcript.
pub async fn run_round_calls(
    run: &(dyn Fn(&str, Value) -> futures::future::BoxFuture<'static, Result<String, String>>
          + Send
          + Sync),
    calls: Vec<(String, Value)>,
) -> Vec<Result<String, String>> {
    futures::future::join_all(calls.into_iter().map(|(name, args)| run(&name, args))).await
}

/// Assemble what the turn spent, alongside the read-it-whole comparison.
#[allow(clippy::too_many_arguments)]
fn turn_cost(
    answer: &str,
    context_chars: usize,
    tool_chars: usize,
    system_chars: usize,
    schema_chars: usize,
    ledger: &Mutex<CitationLedger>,
    repo_root: &std::path::Path,
) -> TurnCost {
    let (files, whole_file_chars) = {
        let l = ledger.lock().expect("citation ledger poisoned");
        whole_file_cost(l.items(), repo_root)
    };
    TurnCost {
        context_chars,
        system_chars,
        schema_chars,
        tool_chars,
        answer_chars: answer.chars().count(),
        files,
        whole_file_chars,
    }
}

/// Size on disk of the distinct files this turn's citations came from.
///
/// The honest baseline for "what did this save": an agent without a graph
/// still has to find the right files, and then it opens them. Files the index
/// lists but the tree no longer has are skipped rather than guessed at, so
/// the count returned is what was actually measured.
fn whole_file_cost(items: &[ContextItem], repo_root: &std::path::Path) -> (usize, u64) {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut bytes = 0u64;
    let mut files = 0usize;
    for item in items {
        if item.file.is_empty() || !seen.insert(item.file.as_str()) {
            continue;
        }
        if let Ok(md) = std::fs::metadata(repo_root.join(&item.file)) {
            if md.is_file() {
                bytes += md.len();
                files += 1;
            }
        }
    }
    (files, bytes)
}

/// `12345` → `12,345`. Big token counts are unreadable without it.
fn fmt_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One-line rendering of tool arguments for the progress feed.
fn compact_args(args: &Value) -> String {
    let Some(obj) = args.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let val: String = val.chars().take(40).collect();
            format!("{}={}", k, val)
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

/// Keep one tool result from eating the whole context window.
fn clip_tool_result(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{}\n… (result truncated at {} chars)", head, max_chars)
}

fn merge_usage(a: Option<Usage>, b: Option<Usage>) -> Option<Usage> {
    match (a, b) {
        (None, x) => x,
        (x, None) => x,
        (Some(x), Some(y)) => {
            let add = |l: Option<u32>, r: Option<u32>| match (l, r) {
                (None, v) => v,
                (v, None) => v,
                (Some(l), Some(r)) => Some(l + r),
            };
            Some(Usage {
                prompt_tokens: add(x.prompt_tokens, y.prompt_tokens),
                completion_tokens: add(x.completion_tokens, y.completion_tokens),
                total_tokens: add(x.total_tokens, y.total_tokens),
            })
        }
    }
}

/// Per-request RAG knobs. Mirrors the subset of `SearchKbOptions` that
/// makes sense to expose to a chat caller (we hide the PPR-tuning
/// fields behind defaults).
#[derive(Clone, Debug)]
pub struct ChatRagOptions<'a> {
    pub k: usize,
    pub hops: u32,
    pub strategy: RankStrategy,
    pub direction: Direction,
    pub edge_types: Option<&'a [String]>,
    pub include_snippets: bool,
    pub max_context_chars: usize,
    pub where_clause: Option<&'a str>,
    pub system_prompt: Option<&'a str>,
    /// Answer without deliberating (see [`no_think_body`]). On by default:
    /// the answer is grounded in retrieved context, so the wall-clock cost
    /// of a chain of thought rarely buys anything.
    ///
    /// [`no_think_body`]: no_think_body
    pub fast: bool,
    /// Run one hybrid retrieval on the user's wording before the model speaks.
    ///
    /// Necessary when the model has no tools — it is the only evidence there
    /// will be. **Redundant when it does**: a deliberating model calls
    /// `search` itself, with its own wording, and the seed pack then arrives
    /// as a second, worse-phrased set of the same neighbourhood. One observed
    /// turn: 8 seed items the model did not use, 15 from its own search, 19
    /// sources listed, and ~10k tokens of pack paid for either way.
    ///
    /// Defaulted from the toolbox by both transports rather than here, since
    /// this struct does not know whether one was attached.
    pub seed: bool,
}

impl<'a> ChatRagOptions<'a> {
    pub fn new() -> Self {
        Self {
            k: 8,
            hops: 2,
            strategy: RankStrategy::Ppr,
            direction: Direction::Both,
            edge_types: None,
            include_snippets: true,
            max_context_chars: DEFAULT_CONTEXT_CHARS,
            where_clause: None,
            system_prompt: None,
            fast: true,
            seed: true,
        }
    }
}

impl<'a> Default for ChatRagOptions<'a> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ultragraph::storage::ContextItem;

    fn fake_item(idx: usize, snippet: Option<&str>) -> ContextItem {
        ContextItem {
            id: format!("file:src/a{}.rs", idx),
            name: format!("fn_{}", idx),
            node_type: "Function".into(),
            file: format!("src/a{}.rs", idx),
            start_line: 10,
            end_line: 15,
            description: format!("describes fn_{}", idx),
            distance: 0.1 * idx as f32,
            hop: idx as u32,
            snippet: snippet.map(|s| s.to_string()),
            matched_by: "semantic".into(),
        }
    }

    #[test]
    fn a_pack_numbers_items_and_includes_snippets() {
        let items = vec![
            fake_item(1, Some("fn fn_1() {}")),
            fake_item(2, None),
        ];
        let out = CitationLedger::new().render(&items, 10_000);
        assert!(out.contains("[#1]"));
        assert!(out.contains("[#2]"));
        assert!(out.contains("fn_1"));
        assert!(out.contains("fn fn_1() {}"));
        // Header includes the line range.
        assert!(out.contains(":10-15"));
    }

    #[test]
    fn a_pack_truncates_at_char_budget() {
        let big_snippet: String = "x".repeat(5_000);
        let items = vec![
            fake_item(1, Some(&big_snippet)),
            fake_item(2, Some(&big_snippet)),
            fake_item(3, Some(&big_snippet)),
        ];
        let out = CitationLedger::new().render(&items, 6_000);
        // Should fit the first item but stop before the third.
        assert!(out.contains("[#1]"));
        assert!(!out.contains("[#3]"), "third item should be dropped");
    }

    // ---------- the turn's [#N] namespace ----------
    //
    // The bug these pin: numbering restarted at [#1] on every render, so a
    // turn that took the tool suffix's advice and searched again produced a
    // second block also numbered [#1], [#2], [#3]. The model then cited a
    // number that meant two different nodes, and the citation the reader
    // clicked resolved against whichever one the seed pack had put there.

    // ---------- a round's calls run together ----------

    #[tokio::test]
    async fn a_rounds_calls_run_concurrently_and_come_back_in_order() {
        use std::time::{Duration, Instant};
        // Each call sleeps; in series this is 300 ms, together it is ~100 ms.
        let run = |name: &str, _args: Value| {
            let name = name.to_string();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(name)
            }) as futures::future::BoxFuture<'static, Result<String, String>>
        };
        let calls = vec![
            ("first".to_string(), Value::Null),
            ("second".to_string(), Value::Null),
            ("third".to_string(), Value::Null),
        ];
        let t = Instant::now();
        let out = run_round_calls(&run, calls).await;
        let elapsed = t.elapsed();

        // Asked-for order, not completion order: a provider rejects a `tool`
        // message whose id does not follow the call that requested it.
        assert_eq!(
            out,
            vec![
                Ok("first".to_string()),
                Ok("second".to_string()),
                Ok("third".to_string())
            ]
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "three 100ms calls took {elapsed:?} — they ran in series"
        );
    }

    #[tokio::test]
    async fn one_failed_call_does_not_lose_the_others() {
        // A tool error is something the model reads and learns from; it must
        // not take the round's other results down with it.
        let run = |name: &str, _args: Value| {
            let name = name.to_string();
            Box::pin(async move {
                if name == "bad" {
                    Err("no such node".to_string())
                } else {
                    Ok(name)
                }
            }) as futures::future::BoxFuture<'static, Result<String, String>>
        };
        let out = run_round_calls(
            &run,
            vec![
                ("good".to_string(), Value::Null),
                ("bad".to_string(), Value::Null),
                ("also_good".to_string(), Value::Null),
            ],
        )
        .await;
        assert_eq!(out[0], Ok("good".to_string()));
        assert_eq!(out[1], Err("no such node".to_string()));
        assert_eq!(out[2], Ok("also_good".to_string()));
    }

    // ---------- a tool's nodes are evidence too ----------

    fn fake_graph() -> ultragraph::types::GraphData {
        let node = ultragraph::types::GraphNode {
            id: "function:src/a.rs:clip".into(),
            name: "clip".into(),
            node_type: ultragraph::types::GraphNodeType::Function,
            file: Some("src/a.rs".into()),
            start_line: Some(10),
            end_line: Some(20),
            docstring: Some("Clips a result.".into()),
            ..Default::default()
        };
        ultragraph::types::GraphData {
            nodes: vec![node],
            edges: Vec::new(),
            stats: None,
            resolution: None,
        }
    }

    #[test]
    fn a_tool_node_gets_a_citable_number() {
        let ledger = Mutex::new(CitationLedger::new());
        let md = "- Function clip  src/a.rs:10-20\n  id: function:src/a.rs:clip\n";
        let out = cite_tool_nodes(md, &fake_graph(), &ledger, "find_usages");
        assert!(out.contains("id: function:src/a.rs:clip  [#1]"), "{out}");
        let l = ledger.lock().unwrap();
        assert_eq!(l.len(), 1);
        // How it was reached is the tool that reached it.
        assert_eq!(l.items()[0].matched_by, "find_usages");
    }

    #[test]
    fn a_tool_node_continues_the_packs_numbering() {
        let ledger = Mutex::new(CitationLedger::new());
        ledger.lock().unwrap().render(&[fake_item(1, None), fake_item(2, None)], 10_000);
        let out = cite_tool_nodes(
            "  id: function:src/a.rs:clip\n",
            &fake_graph(),
            &ledger,
            "get_code",
        );
        assert!(out.contains("[#3]"), "the pack owns [#1] and [#2]:\n{out}");
    }

    #[test]
    fn an_id_the_graph_does_not_have_is_left_alone() {
        // A tool may print an id this snapshot no longer carries. Inventing a
        // citation for it is worse than omitting it.
        let ledger = Mutex::new(CitationLedger::new());
        let md = "  id: function:src/gone.rs:vanished\n";
        let out = cite_tool_nodes(md, &fake_graph(), &ledger, "find_usages");
        assert_eq!(out, md);
        assert_eq!(ledger.lock().unwrap().len(), 0);
    }

    #[test]
    fn output_with_no_ids_is_returned_untouched() {
        // `analyze` and `graph_schema` print none, and must not cost a pass
        // over the graph to discover that.
        let ledger = Mutex::new(CitationLedger::new());
        let md = "| folder | count |\n| --- | --- |\n| src | 12 |\n";
        assert_eq!(cite_tool_nodes(md, &fake_graph(), &ledger, "analyze"), md);
        assert_eq!(ledger.lock().unwrap().len(), 0);
    }

    #[test]
    fn the_same_node_from_two_tools_keeps_one_number() {
        let ledger = Mutex::new(CitationLedger::new());
        let md = "  id: function:src/a.rs:clip\n";
        let a = cite_tool_nodes(md, &fake_graph(), &ledger, "find_usages");
        let b = cite_tool_nodes(md, &fake_graph(), &ledger, "get_code");
        assert!(a.contains("[#1]") && b.contains("[#1]"), "{a}{b}");
        assert_eq!(ledger.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_empty_pack_says_which_kind_of_empty_it_is() {
        // "We looked and found nothing" is a fact about the repository and
        // invites the model to say so. "We have not looked yet" is an
        // instruction. The same empty pack has to read as one or the other.
        let ctx = RankedContext {
            query: "q".into(),
            items: vec![],
            total_chars: 0,
            seed_id: None,
        };
        let looked =
            build_rag_messages("q", &ctx, &[], None, 10_000, &mut CitationLedger::new(), false);
        assert!(looked[1].content.starts_with("No retrieved context"), "{}", looked[1].content);

        let not_yet =
            build_rag_messages("q", &ctx, &[], None, 10_000, &mut CitationLedger::new(), true);
        assert!(not_yet[1].content.contains("deliberate, not a finding"), "{}", not_yet[1].content);
        assert!(
            not_yet[1].content.contains("do not report the repository as empty"),
            "{}",
            not_yet[1].content
        );
    }

    #[test]
    fn prompt_starts_from_the_same_core() {
        // Two prompts, one for a turn that can go looking and one for a turn
        // that cannot. They must differ ONLY by the closed-book tail, or the
        // next edit lands in one of them and silently not the other.
        assert!(
            DEFAULT_SYSTEM_PROMPT.starts_with(SYSTEM_CORE),
            "the closed-book prompt is the core plus a tail, not a second prompt"
        );
        let tail = &DEFAULT_SYSTEM_PROMPT[SYSTEM_CORE.len()..];
        assert!(tail.contains("ONLY information present") && tail.contains("say so plainly"));
        // …and the core must carry neither: with tools in hand, both are the
        // wrong instruction and the model obeys them over the suffix.
        assert!(!SYSTEM_CORE.contains("ONLY information present"), "{SYSTEM_CORE}");
        assert!(!SYSTEM_CORE.contains("say so plainly"), "{SYSTEM_CORE}");
        // Citing is in the half that always applies.
        assert!(SYSTEM_CORE.contains("[#2]"));
    }

    #[test]
    fn an_item_says_how_it_was_reached() {
        // "Are these any good?" is the judgment the suffix asks for, and it
        // is unanswerable from name and path alone.
        let mut near = fake_item(1, None);
        near.hop = 0;
        near.matched_by = "keyword".into();
        let out = CitationLedger::new().render(&[near], 10_000);
        assert!(out.contains("· keyword match"), "{out}");

        let mut far = fake_item(2, None);
        far.hop = 3;
        far.matched_by = "semantic".into();
        let out = CitationLedger::new().render(&[far], 10_000);
        assert!(out.contains("· semantic, 3 hop(s) out"), "{out}");
    }

    #[test]
    fn a_second_render_continues_the_numbering() {
        let mut ledger = CitationLedger::new();
        let seed = ledger.render(&[fake_item(1, None), fake_item(2, None)], 10_000);
        assert!(seed.contains("[#1]") && seed.contains("[#2]"), "{seed}");

        // What a `search` tool call renders mid-turn.
        let found = ledger.render(&[fake_item(3, None)], 10_000);
        assert!(found.contains("[#3]"), "a re-search must not restart at [#1]:\n{found}");
        assert!(!found.contains("[#1]"), "{found}");
        assert_eq!(ledger.len(), 3);
    }

    #[test]
    fn a_node_cited_twice_keeps_its_first_number() {
        let mut ledger = CitationLedger::new();
        ledger.render(&[fake_item(1, None), fake_item(2, None)], 10_000);
        // A re-search returns the same neighbourhood plus one new node.
        let again = ledger.render(&[fake_item(2, None), fake_item(9, None)], 10_000);
        assert!(again.contains("[#2]"), "the repeat keeps its number:\n{again}");
        assert!(again.contains("[#3]"), "the new node gets the next one:\n{again}");
        assert_eq!(ledger.len(), 3, "a repeat must not lengthen the evidence list");
    }

    #[test]
    fn only_what_is_rendered_is_cited() {
        // An item dropped by the budget was never shown to the model, so
        // listing it as a source tells the reader the answer rests on
        // something it could not have read.
        let big: String = "x".repeat(2_000);
        let mut ledger = CitationLedger::new();
        let out = ledger.render(
            &[fake_item(1, Some(&big)), fake_item(2, Some(&big)), fake_item(3, Some(&big))],
            6_000,
        );
        assert!(out.contains("[#2]") && !out.contains("[#3]"), "{out}");
        assert_eq!(ledger.len(), 2, "the dropped item must not be cited");
    }

    #[test]
    fn a_reset_starts_the_numbering_again() {
        let mut ledger = CitationLedger::new();
        ledger.render(&[fake_item(1, None), fake_item(2, None)], 10_000);
        ledger.reset();
        let next = ledger.render(&[fake_item(7, None)], 10_000);
        assert!(next.contains("[#1]"), "a new turn opens at [#1]:\n{next}");
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn the_seed_pack_takes_the_first_numbers() {
        // build_rag_messages must register before any tool runs, or a tool
        // that renders first would take [#1] out from under the pack the
        // user is shown.
        let ctx = RankedContext {
            query: "q".into(),
            items: vec![fake_item(1, None), fake_item(2, None)],
            total_chars: 0,
            seed_id: None,
        };
        let mut ledger = CitationLedger::new();
        let msgs = build_rag_messages("q", &ctx, &[], None, 10_000, &mut ledger, false);
        assert!(msgs[1].content.contains("[#1]") && msgs[1].content.contains("[#2]"));
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.items()[0].id, fake_item(1, None).id);
    }

    #[test]
    fn the_tool_suffix_tells_the_model_the_numbering_is_shared() {
        // The suffix is what makes a searched-for node citable at all: a
        // model told to describe tool findings "in prose" will not cite them.
        assert!(
            TOOL_SYSTEM_SUFFIX.contains("SAME [#N] run"),
            "the suffix must say search results continue the numbering"
        );
    }

    #[test]
    fn build_rag_messages_carries_history_and_system() {
        let ctx = RankedContext {
            query: "q".into(),
            items: vec![fake_item(1, None)],
            total_chars: 0,
            seed_id: Some("seed".into()),
        };
        let history = vec![
            ChatMessage::new("user", "prev?"),
            ChatMessage::new("assistant", "prev!"),
        ];
        let msgs = build_rag_messages(
            "now?",
            &ctx,
            &history,
            Some("CUSTOM"),
            10_000,
            &mut CitationLedger::new(),
            false,
        );

        // [system, system(context), user(prev), assistant(prev), user(now)]
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "CUSTOM");
        assert!(msgs[1].content.contains("[#1]"));
        assert_eq!(msgs[2].content, "prev?");
        assert_eq!(msgs[3].content, "prev!");
        assert_eq!(msgs[4].role, "user");
        assert_eq!(msgs[4].content, "now?");
    }

    #[test]
    fn sse_line_parses_content_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"hel"},"finish_reason":null}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta(d) => assert_eq!(d.content.as_deref(), Some("hel")),
            _ => panic!("expected delta"),
        }
    }

    #[test]
    fn sse_line_parses_reasoning_dialects() {
        for field in ["reasoning_content", "reasoning"] {
            let line = format!(r#"data: {{"choices":[{{"delta":{{"{}":"hmm"}}}}]}}"#, field);
            match parse_sse_line(&line) {
                SseLine::Delta(d) => assert_eq!(d.reasoning.as_deref(), Some("hmm"), "{}", field),
                _ => panic!("expected delta for {}", field),
            }
        }
    }

    #[test]
    fn sse_line_done_and_noise() {
        assert!(matches!(parse_sse_line("data: [DONE]"), SseLine::Done));
        assert!(matches!(parse_sse_line(""), SseLine::Skip));
        assert!(matches!(parse_sse_line(": keep-alive"), SseLine::Skip));
        assert!(matches!(parse_sse_line("event: message"), SseLine::Skip));
        assert!(matches!(parse_sse_line("data: {not json"), SseLine::Skip));
        // Empty delta object → nothing to report.
        assert!(matches!(
            parse_sse_line(r#"data: {"choices":[{"delta":{}}]}"#),
            SseLine::Skip
        ));
    }

    #[test]
    fn sse_line_captures_finish_and_usage() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"total_tokens":42}}"#;
        match parse_sse_line(line) {
            SseLine::Delta(d) => {
                assert_eq!(d.finish_reason.as_deref(), Some("stop"));
                assert_eq!(d.usage.unwrap().total_tokens, Some(42));
            }
            _ => panic!("expected delta"),
        }
    }

    #[test]
    fn build_rag_messages_handles_empty_context() {
        let ctx = RankedContext {
            query: "q".into(),
            items: vec![],
            total_chars: 0,
            seed_id: None,
        };
        let msgs =
            build_rag_messages("hello", &ctx, &[], None, 10_000, &mut CitationLedger::new(), false);
        assert_eq!(msgs[0].content, DEFAULT_SYSTEM_PROMPT);
        assert!(msgs[1].content.starts_with("No retrieved context"));
    }

    // ---- ChatConfig::with_overrides -------------------------------------

    #[test]
    fn with_overrides_of_nothing_is_the_default_config() {
        let cfg = ChatConfig::with_overrides(None, None, None, None, None, None);
        let d = ChatConfig::default();
        assert_eq!(cfg.base_url, d.base_url);
        assert_eq!(cfg.api_key, d.api_key);
        assert_eq!(cfg.model, d.model);
        assert_eq!(cfg.temperature, d.temperature);
        assert_eq!(cfg.max_tokens, d.max_tokens);
        assert_eq!(cfg.timeout_secs, d.timeout_secs);
        assert!(cfg.extra_body.is_none());
    }

    #[test]
    fn each_override_is_applied_independently() {
        // A setter wired to the wrong field is invisible until someone's
        // `--model` silently changes their base URL, so check one at a time
        // against everything else staying default.
        let d = ChatConfig::default();

        let c = ChatConfig::with_overrides(Some("http://x/v1".into()), None, None, None, None, None);
        assert_eq!(c.base_url, "http://x/v1");
        assert_eq!(c.model, d.model);

        let c = ChatConfig::with_overrides(None, Some("sk-abc".into()), None, None, None, None);
        assert_eq!(c.api_key, "sk-abc");
        assert_eq!(c.base_url, d.base_url);

        let c = ChatConfig::with_overrides(None, None, Some("qwen3".into()), None, None, None);
        assert_eq!(c.model, "qwen3");

        let c = ChatConfig::with_overrides(None, None, None, Some(0.9), None, None);
        assert_eq!(c.temperature, 0.9);
        assert_eq!(c.max_tokens, d.max_tokens);

        let c = ChatConfig::with_overrides(None, None, None, None, Some(256), None);
        assert_eq!(c.max_tokens, 256);

        let c = ChatConfig::with_overrides(None, None, None, None, None, Some(5));
        assert_eq!(c.timeout_secs, 5);
        assert_eq!(c.temperature, d.temperature);
    }

    #[test]
    fn zero_valued_overrides_are_honoured_rather_than_ignored() {
        // `Some(0)` is a deliberate choice (a greedy temperature, a hard
        // timeout); treating it as "unset" would silently ignore the flag.
        let c = ChatConfig::with_overrides(None, None, None, Some(0.0), Some(0), Some(0));
        assert_eq!(c.temperature, 0.0);
        assert_eq!(c.max_tokens, 0);
        assert_eq!(c.timeout_secs, 0);
    }

    #[test]
    fn an_empty_string_override_still_replaces_the_default() {
        // Passing `Some("")` is how a caller clears an API key for a local
        // endpoint that rejects one.
        let c = ChatConfig::with_overrides(None, Some(String::new()), None, None, None, None);
        assert_eq!(c.api_key, "");
    }

    // ---- no_think_body / fast_client -------------------------------------

    #[test]
    fn no_think_body_sends_every_spelling_of_the_off_switch() {
        // Providers disagree on the field name and ignore what they don't
        // recognise, so sending all of them is the point. Dropping one
        // silently reintroduces minutes of deliberation on that provider.
        let m = no_think_body();
        assert_eq!(
            m.get("chat_template_kwargs"),
            Some(&serde_json::json!({ "enable_thinking": false }))
        );
        assert_eq!(m.get("reasoning_effort"), Some(&serde_json::json!("low")));
    }

    #[test]
    fn fast_client_switches_deliberation_off_and_keeps_everything_else() {
        let base = ChatConfig::with_overrides(
            Some("http://x/v1".into()),
            Some("k".into()),
            Some("m".into()),
            Some(0.3),
            Some(99),
            Some(7),
        );
        let client = ChatClient::new(base).expect("client");
        let fast = fast_client(&client).expect("a client with no extra_body gets a fast twin");

        assert_eq!(fast.config().extra_body.as_ref(), Some(&no_think_body()));
        assert_eq!(fast.config().base_url, "http://x/v1");
        assert_eq!(fast.config().model, "m");
        assert_eq!(fast.config().max_tokens, 99);
        assert_eq!(fast.config().timeout_secs, 7);
        // The original is untouched.
        assert!(client.config().extra_body.is_none());
    }

    #[test]
    fn an_explicit_extra_body_is_never_overridden() {
        // The caller already told the provider how to behave; replacing that
        // with our guess would override an explicit choice.
        let mut cfg = ChatConfig::default();
        let mut custom = serde_json::Map::new();
        custom.insert("reasoning_effort".into(), serde_json::json!("high"));
        cfg.extra_body = Some(custom);

        let client = ChatClient::new(cfg).expect("client");
        assert!(fast_client(&client).is_none());
    }

    // ---- compact_args ----------------------------------------------------

    #[test]
    fn compact_args_renders_sorted_key_value_pairs() {
        // Sorted so the same call always prints the same way — an unsorted
        // map iteration makes the progress feed reshuffle between runs.
        let args = serde_json::json!({ "query": "auth", "k": 5, "deep": true });
        assert_eq!(compact_args(&args), "deep=true k=5 query=auth");
    }

    #[test]
    fn compact_args_unquotes_strings_but_not_other_values() {
        let args = serde_json::json!({ "s": "plain", "n": 1.5, "arr": ["a"], "nul": null });
        let out = compact_args(&args);
        assert!(out.contains("s=plain"), "{out}");
        assert!(out.contains("n=1.5"), "{out}");
        assert!(out.contains(r#"arr=["a"]"#), "{out}");
        assert!(out.contains("nul=null"), "{out}");
    }

    #[test]
    fn compact_args_clips_each_value_to_forty_chars() {
        let args = serde_json::json!({ "q": "x".repeat(100) });
        let out = compact_args(&args);
        assert_eq!(out, format!("q={}", "x".repeat(40)));
    }

    #[test]
    fn compact_args_of_a_non_object_is_empty() {
        // Tool arguments arrive as whatever the model emitted, which is not
        // always the object the schema asked for.
        assert_eq!(compact_args(&serde_json::json!([1, 2])), "");
        assert_eq!(compact_args(&serde_json::json!("bare")), "");
        assert_eq!(compact_args(&serde_json::json!(null)), "");
        assert_eq!(compact_args(&serde_json::json!({})), "");
    }

    // ---- clip_tool_result ------------------------------------------------

    #[test]
    fn a_short_result_is_returned_untouched() {
        assert_eq!(clip_tool_result("small", 100), "small");
        // Exactly at the limit is still untouched — the comparison is `<=`.
        assert_eq!(clip_tool_result("abcde", 5), "abcde");
    }

    #[test]
    fn the_default_budget_fits_a_whole_boundary_listing() {
        // The number that set this: `analyze boundaries` over ~142 surfaces
        // was cut at 6 000 chars mid-table. A model handed half a table either
        // answers from half a table or spends a round paging for the rest.
        let a_full_listing = 142 * 120; // ~120 chars per row, id + kinds + file
        assert!(
            DEFAULT_TOOL_RESULT_CHARS > a_full_listing,
            "a full listing is ~{a_full_listing} chars and the cap is {DEFAULT_TOOL_RESULT_CHARS}"
        );
    }

    #[test]
    fn an_oversized_result_is_clipped_and_says_so() {
        let out = clip_tool_result(&"a".repeat(100), 10);
        assert!(out.starts_with(&"a".repeat(10)));
        assert!(
            out.contains("truncated at 10 chars"),
            "the model has to know it saw a partial result: {out}"
        );
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        // A byte-based cap would split a multi-byte character and produce
        // invalid output; `é` is two bytes and must count as one.
        let out = clip_tool_result(&"é".repeat(20), 5);
        let head: String = out.chars().take(5).collect();
        assert_eq!(head, "ééééé");
        assert_eq!(clip_tool_result(&"é".repeat(5), 5), "é".repeat(5));
    }

    #[test]
    fn clipping_to_zero_keeps_only_the_notice() {
        let out = clip_tool_result("anything", 0);
        assert!(out.starts_with('\n'), "{out:?}");
        assert!(out.contains("truncated at 0 chars"));
    }

    // ---- merge_usage -----------------------------------------------------

    fn usage(p: Option<u32>, c: Option<u32>, t: Option<u32>) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: t,
        }
    }

    #[test]
    fn merging_usage_sums_each_field() {
        // A tool-calling turn reports usage per request; the caller wants
        // the total for the whole exchange.
        let m = merge_usage(
            Some(usage(Some(10), Some(5), Some(15))),
            Some(usage(Some(3), Some(7), Some(10))),
        )
        .unwrap();
        assert_eq!(m.prompt_tokens, Some(13));
        assert_eq!(m.completion_tokens, Some(12));
        assert_eq!(m.total_tokens, Some(25));
    }

    #[test]
    fn merging_with_none_returns_the_other_side() {
        let u = usage(Some(1), Some(2), Some(3));
        assert_eq!(
            merge_usage(None, Some(u.clone())).unwrap().total_tokens,
            Some(3)
        );
        assert_eq!(merge_usage(Some(u), None).unwrap().total_tokens, Some(3));
        assert!(merge_usage(None, None).is_none());
    }

    #[test]
    fn a_field_missing_on_one_side_is_carried_from_the_other() {
        // Providers omit fields inconsistently. Treating a missing count as
        // zero would be fine; dropping the side that *has* it would not.
        let m = merge_usage(
            Some(usage(Some(10), None, None)),
            Some(usage(None, Some(4), None)),
        )
        .unwrap();
        assert_eq!(m.prompt_tokens, Some(10));
        assert_eq!(m.completion_tokens, Some(4));
        assert_eq!(m.total_tokens, None);
    }

    // ---- ChatError::is_unreachable ---------------------------------------

    #[test]
    fn a_404_reads_as_unreachable_because_it_usually_is_a_wrong_base_url() {
        assert!(ChatError::BadStatus(404, "Not Found".into()).is_unreachable());
    }

    #[test]
    fn a_model_side_refusal_is_not_unreachable() {
        // These mean the endpoint answered. Offering "configure your
        // endpoint" here would send the user to fix the one thing that
        // demonstrably works.
        for code in [400, 401, 403, 422, 429, 500, 502, 503] {
            assert!(
                !ChatError::BadStatus(code, "x".into()).is_unreachable(),
                "status {code}"
            );
        }
        assert!(!ChatError::EmptyChoices.is_unreachable());
    }

    #[test]
    fn chat_errors_render_with_their_detail() {
        // These strings are what the user actually sees when a chat fails.
        assert_eq!(
            ChatError::BadStatus(500, "boom".into()).to_string(),
            "chat bad status 500: boom"
        );
        assert_eq!(
            ChatError::EmptyChoices.to_string(),
            "chat response had no choices"
        );
    }

    // ---- ChatRagOptions --------------------------------------------------

    #[test]
    fn rag_options_default_to_grounded_and_fast() {
        let o = ChatRagOptions::default();
        assert_eq!(o.k, 8);
        assert_eq!(o.hops, 2);
        assert!(matches!(o.strategy, RankStrategy::Ppr));
        assert!(matches!(o.direction, Direction::Both));
        assert!(o.include_snippets, "snippets are what ground the answer");
        // On by default: the answer is grounded in retrieved context, so a
        // chain of thought rarely buys anything and costs wall-clock time.
        assert!(o.fast);
        assert!(o.edge_types.is_none());
        assert!(o.where_clause.is_none());
        assert!(o.system_prompt.is_none());
        assert_eq!(o.max_context_chars, DEFAULT_CONTEXT_CHARS);
    }

    #[test]
    fn rag_options_new_and_default_agree() {
        let (a, b) = (ChatRagOptions::new(), ChatRagOptions::default());
        assert_eq!(a.k, b.k);
        assert_eq!(a.hops, b.hops);
        assert_eq!(a.fast, b.fast);
        assert_eq!(a.max_context_chars, b.max_context_chars);
    }
}

/// The retrieval half of a RAG turn, shared by the streaming and
/// non-streaming paths.
pub async fn retrieve_context(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    repo_root: &std::path::Path,
    query: &str,
    opts: &ChatRagOptions<'_>,
) -> Result<RankedContext, Box<dyn std::error::Error + Send + Sync>> {
    let mut search_opts = SearchKbOptions::new(query, repo_root);
    search_opts.k = opts.k;
    search_opts.hops = opts.hops;
    search_opts.strategy = opts.strategy;
    search_opts.direction = opts.direction;
    search_opts.edge_types = opts.edge_types;
    search_opts.include_snippets = opts.include_snippets;
    search_opts.max_chars = opts.max_context_chars;
    search_opts.where_clause = opts.where_clause;
    storage_search_kb(store, embedder, search_opts).await
}

/// Everything a RAG turn needs before the answer goes anywhere.
///
/// [`run_chat_rag`] and [`run_chat_rag_stream`] take exactly this set and
/// differ only in how the answer comes back, so it is one struct rather than
/// eight positional arguments repeated in two signatures and at every call
/// site. Streaming is a transport choice: a field added here reaches both
/// paths, which is what keeps them from drifting.
pub struct ChatRagRequest<'a> {
    /// Where retrieval reads from.
    pub store: &'a dyn KnowledgeStore,
    /// Embeds the query for the dense half of the hybrid search.
    pub embedder: &'a Embedder,
    /// The provider that writes the answer.
    pub chat: &'a ChatClient,
    /// Forwarded to retrieval so it can resolve relative source paths when
    /// building snippets.
    pub repo_root: &'a std::path::Path,
    /// The question being asked this turn.
    pub query: &'a str,
    /// Prior turns, oldest first.
    pub history: &'a [ChatMessage],
    /// Retrieval and prompt knobs.
    pub opts: ChatRagOptions<'a>,
    /// Graph tools the model may call. `None` withholds them — which is a
    /// capability decision, never a consequence of how the caller
    /// transports the answer.
    pub toolbox: Option<&'a ToolBox<'a>>,
    /// The turn's `[#N]` numbering, shared with whatever the toolbox's
    /// `search` runs through. The caller owns it because the caller built
    /// the tool runner: both sides have to write into the same one or the
    /// numbers mean two different things (see [`CitationLedger`]).
    pub ledger: &'a Mutex<CitationLedger>,
}

/// Single-turn RAG: retrieve from the store, then ask the provider to
/// answer.
pub async fn run_chat_rag(
    req: ChatRagRequest<'_>,
) -> Result<ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let ChatRagRequest {
        store,
        embedder,
        chat,
        repo_root,
        query,
        history,
        opts,
        toolbox,
        ledger,
    } = req;
    let t_ret = std::time::Instant::now();
    // Skipping the seed is not "no retrieval" — it hands the retrieval to the
    // model, which searches in the codebase's own vocabulary instead of the
    // user's. See `ChatRagOptions::seed`.
    let context = if opts.seed {
        retrieve_context(store, embedder, repo_root, query, &opts).await?
    } else {
        RankedContext { query: query.to_string(), items: Vec::new(), total_chars: 0, seed_id: None }
    };
    let retrieval_ms = t_ret.elapsed().as_millis();

    // A model that can go looking must not be told to stay put. An explicit
    // system prompt from the caller always wins — that one was deliberate.
    let system = opts
        .system_prompt
        .or(Some(if toolbox.is_some() { SYSTEM_CORE } else { DEFAULT_SYSTEM_PROMPT }));
    let mut messages = {
        let mut l = ledger.lock().expect("citation ledger poisoned");
        build_rag_messages(
            query,
            &context,
            history,
            system,
            opts.max_context_chars,
            &mut l,
            !opts.seed,
        )
    };
    // The retrieved pack is the second system message; its size is the part
    // of the prompt this question is responsible for.
    //
    // Only when there *is* one. With no seed pass that message is the
    // "nothing retrieved yet, go and look" preface — fixed prompt text, the
    // same for every question. Counting it as a retrieved pack reported
    // "Retrieved pack ~41" on a turn that retrieved nothing, which reads as a
    // tiny retrieval rather than as none.
    let preface_chars = messages.get(1).map(|m| m.content.chars().count()).unwrap_or(0);
    let (context_chars, preface_overhead) = if context.items.is_empty() {
        (0, preface_chars)
    } else {
        (preface_chars, 0)
    };

    // Deliberation is what makes a model with tools actually use them.
    //
    // Measured on the same question through this path (docs/dev/RAG-EVAL.md,
    // 2026-09-20): `fast` on → 0 tool calls, `fast` off → 4. Across the whole
    // 12-question set, `fast` on produced 0 tool calls and an answer identical
    // to seed-only retrieval. The UI sends `think: false` by default, so the
    // agentic layer was inert in the shipped configuration and nothing said so.
    //
    // **This is a choice the model makes, not a capability the template
    // removes.** A direct probe of the endpoint with `enable_thinking: false`
    // still returns tool calls, so the tools are offered and callable either
    // way. What changes is willingness: handed eight plausible-looking items
    // and no room to deliberate, it answers from them. Given room, it checks.
    // Worth re-testing when the prompt or the pack changes — the lever may
    // move.
    //
    // Fast mode therefore applies to a turn that has nothing to call. A caller
    // who wants the latency back turns the toolbox off, which is at least an
    // honest trade rather than an invisible one.
    let fast = (opts.fast && toolbox.is_none())
        .then(|| fast_client(chat))
        .flatten();
    let chat = fast.as_ref().unwrap_or(chat);

    let t_cmp = std::time::Instant::now();
    let mut tool_usage = None;
    let mut tool_calls = 0;
    let mut tool_rounds = 0;
    let mut tool_chars = 0usize;
    let mut schema_chars = 0usize;
    // Read once the suffix is on it: the toolbox nearly doubles it.
    let mut system_chars =
        messages.first().map(|m| m.content.chars().count()).unwrap_or(0) + preface_overhead;
    let mut hit_round_cap = false;
    let mut drafted = None;
    if let Some(tb) = toolbox {
        if let Some(sys) = messages.first_mut().filter(|m| m.role == "system") {
            sys.content.push_str(TOOL_SYSTEM_SUFFIX);
        }
        schema_chars = serde_json::to_string(&tb.schemas).map(|s| s.chars().count()).unwrap_or(0);
        system_chars =
            messages.first().map(|m| m.content.chars().count()).unwrap_or(0) + preface_overhead;
        // No progress feed here: nothing is watching a non-streamed turn.
        let rounds = run_tool_rounds(chat, tb, messages, |_| {}).await?;
        messages = rounds.messages;
        tool_usage = rounds.usage;
        tool_calls = rounds.calls;
        tool_rounds = rounds.rounds;
        tool_chars = rounds.result_chars;
        hit_round_cap = tb.max_rounds > 0 && rounds.rounds >= tb.max_rounds;
        drafted = rounds.answer;
    }

    // A round that answered instead of calling a tool already wrote this
    // answer, and its cost is in `tool_usage`.
    let (answer, usage) = match drafted {
        Some(d) => (d.content, None),
        None => chat.complete(&messages).await?,
    };
    let completion_ms = t_cmp.elapsed().as_millis();
    let cost = turn_cost(&answer, context_chars, tool_chars, system_chars, schema_chars, ledger, repo_root);

    Ok(ChatRagOutcome {
        answer,
        reasoning: String::new(),
        citations: ledger.lock().expect("citation ledger poisoned").items().to_vec(),
        context,
        retrieval_ms,
        completion_ms,
        usage: merge_usage(tool_usage, usage),
        tool_calls,
        tool_rounds,
        hit_round_cap,
        cost,
    })
}

/// Streaming variant of `run_chat_rag`. `on_context` fires once after
/// retrieval (so callers can surface citations before the first token);
/// `on_delta` fires per streamed chunk. Falls back to the non-streaming
/// `complete()` when the provider rejects `stream: true` (4xx/5xx on
/// the streaming request), emitting the whole answer as one delta — so
/// callers get streaming when the provider supports it and identical
/// behaviour when it doesn't.
pub async fn run_chat_rag_stream<C, F, T>(
    req: ChatRagRequest<'_>,
    mut on_context: C,
    on_tool: T,
    mut on_delta: F,
) -> Result<ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>>
where
    C: FnMut(&RankedContext),
    T: FnMut(ToolEvent),
    F: FnMut(StreamDelta),
{
    let ChatRagRequest {
        store,
        embedder,
        chat,
        repo_root,
        query,
        history,
        opts,
        toolbox,
        ledger,
    } = req;
    let t_ret = std::time::Instant::now();
    // Skipping the seed is not "no retrieval" — it hands the retrieval to the
    // model, which searches in the codebase's own vocabulary instead of the
    // user's. See `ChatRagOptions::seed`.
    let context = if opts.seed {
        retrieve_context(store, embedder, repo_root, query, &opts).await?
    } else {
        RankedContext { query: query.to_string(), items: Vec::new(), total_chars: 0, seed_id: None }
    };
    let retrieval_ms = t_ret.elapsed().as_millis();
    on_context(&context);

    // A model that can go looking must not be told to stay put. An explicit
    // system prompt from the caller always wins — that one was deliberate.
    let system = opts
        .system_prompt
        .or(Some(if toolbox.is_some() { SYSTEM_CORE } else { DEFAULT_SYSTEM_PROMPT }));
    let mut messages = {
        let mut l = ledger.lock().expect("citation ledger poisoned");
        build_rag_messages(
            query,
            &context,
            history,
            system,
            opts.max_context_chars,
            &mut l,
            !opts.seed,
        )
    };
    // The retrieved pack is the second system message; its size is the part
    // of the prompt this question is responsible for.
    //
    // Only when there *is* one. With no seed pass that message is the
    // "nothing retrieved yet, go and look" preface — fixed prompt text, the
    // same for every question. Counting it as a retrieved pack reported
    // "Retrieved pack ~41" on a turn that retrieved nothing, which reads as a
    // tiny retrieval rather than as none.
    let preface_chars = messages.get(1).map(|m| m.content.chars().count()).unwrap_or(0);
    let (context_chars, preface_overhead) = if context.items.is_empty() {
        (0, preface_chars)
    } else {
        (preface_chars, 0)
    };

    // Deliberation is not a luxury for a model holding tools. `fast_client`
    // sends `enable_thinking: false`, and on a Qwen3-class template that does
    // not merely shorten the reasoning — it stops tool calls being emitted at
    // all. Measured (docs/dev/RAG-EVAL.md, 2026-09-20): 0 tool calls across 12
    // questions with it on, 4 on the same question with it off. The UI sends
    // `think: false` by default, so the entire agentic layer was off in the
    // shipped configuration and nothing said so.
    //
    // Fast mode therefore applies to a turn that has nothing to call. A caller
    // who wants the latency back turns the toolbox off, which is at least an
    // honest trade rather than an invisible one.
    let fast = (opts.fast && toolbox.is_none())
        .then(|| fast_client(chat))
        .flatten();
    let chat = fast.as_ref().unwrap_or(chat);

    let t_cmp = std::time::Instant::now();
    // Let the model dig through the graph first — retrieval gives it a
    // starting neighbourhood, the tools let it follow the threads it finds.
    let mut tool_usage = None;
    let mut tool_calls = 0;
    let mut tool_rounds = 0;
    let mut tool_chars = 0usize;
    let mut schema_chars = 0usize;
    // Read once the suffix is on it: the toolbox nearly doubles it.
    let mut system_chars =
        messages.first().map(|m| m.content.chars().count()).unwrap_or(0) + preface_overhead;
    let mut hit_round_cap = false;
    let mut drafted = None;
    if let Some(tb) = toolbox {
        // Tell it the tools exist, and when they're worth using.
        if let Some(sys) = messages.first_mut().filter(|m| m.role == "system") {
            sys.content.push_str(TOOL_SYSTEM_SUFFIX);
        }
        schema_chars = serde_json::to_string(&tb.schemas).map(|s| s.chars().count()).unwrap_or(0);
        system_chars =
            messages.first().map(|m| m.content.chars().count()).unwrap_or(0) + preface_overhead;
    }
    if let Some(tb) = toolbox {
        let rounds = run_tool_rounds(chat, tb, messages, on_tool).await?;
        messages = rounds.messages;
        tool_usage = rounds.usage;
        tool_calls = rounds.calls;
        tool_rounds = rounds.rounds;
        tool_chars = rounds.result_chars;
        hit_round_cap = tb.max_rounds > 0 && rounds.rounds >= tb.max_rounds;
        drafted = rounds.answer;
    }

    // A round that answered instead of calling a tool already wrote the whole
    // answer. Re-asking for it streamed would double the turn's decode time
    // for nothing — the second copy would not even start arriving until after
    // the first one finished. Deliver the draft as one delta instead, which
    // is what the non-streaming fallback below already does.
    if let Some(d) = drafted {
        on_delta(StreamDelta {
            content: Some(d.content.clone()),
            reasoning: (!d.reasoning.is_empty()).then(|| d.reasoning.clone()),
            ..Default::default()
        });
        let cost = turn_cost(&d.content, context_chars, tool_chars, system_chars, schema_chars, ledger, repo_root);
        return Ok(ChatRagOutcome {
            answer: d.content,
            reasoning: d.reasoning,
            citations: ledger.lock().expect("citation ledger poisoned").items().to_vec(),
            context,
            retrieval_ms,
            completion_ms: t_cmp.elapsed().as_millis(),
            usage: tool_usage,
            tool_calls,
            tool_rounds,
            hit_round_cap,
            cost,
        });
    }

    let (answer, reasoning, usage) = match chat.complete_stream(&messages, &mut on_delta).await {
        Ok(out) => out,
        Err(ChatError::BadStatus(code, body)) => {
            // Provider refused the streaming request — retry plain.
            tracing::debug!(code, body = %body, "stream refused; falling back to non-streaming");
            let (answer, usage) = chat.complete(&messages).await?;
            on_delta(StreamDelta {
                content: Some(answer.clone()),
                usage: usage.clone(),
                ..Default::default()
            });
            (answer, String::new(), usage)
        }
        Err(e) => return Err(Box::new(e)),
    };
    let completion_ms = t_cmp.elapsed().as_millis();
    let cost = turn_cost(&answer, context_chars, tool_chars, system_chars, schema_chars, ledger, repo_root);

    Ok(ChatRagOutcome {
        answer,
        reasoning,
        citations: ledger.lock().expect("citation ledger poisoned").items().to_vec(),
        context,
        retrieval_ms,
        completion_ms,
        usage: merge_usage(tool_usage, usage),
        tool_calls,
        tool_rounds,
        hit_round_cap,
        cost,
    })
}
