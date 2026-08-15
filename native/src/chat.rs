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

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ultragraph::storage::{
    search_kb as storage_search_kb, semantic_search as storage_semantic_search, ContextItem,
    DEFAULT_CONTEXT_CHARS, Direction, Embedder, KnowledgeStore, RankStrategy, RankedContext,
    SearchKbOptions,
};

/// Default chat model. Picked so the CLI works as soon as the user
/// points `--base-url` at any OpenAI-compatible chat endpoint; the
/// caller almost always wants to override this with `--chat-model`.
pub const DEFAULT_CHAT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_CHAT_BASE_URL: &str = "http://127.0.0.1:8000/v1";
pub const DEFAULT_CHAT_API_KEY: &str = "1234";
pub const DEFAULT_TEMPERATURE: f32 = 0.2;
pub const DEFAULT_MAX_TOKENS: u32 = 32768;
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;


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
    #[allow(dead_code)]
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
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are UltraGraph, a precise code/knowledge assistant. \
You are given retrieved context items numbered [#1], [#2], ... drawn from a knowledge graph + vector \
store over the user's repository. Answer the user's question using ONLY information present in those \
items when possible. Cite the supporting items inline using their bracketed numbers (e.g. \"see [#2]\"). \
If the answer is not in the context, say so plainly instead of guessing. Prefer concise, structured \
answers with code references and file paths when relevant.";

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
- `file_outline` to see everything a file declares.\n\
- `find_symbols` to resolve a name you were given into a real node id.\n\
- `search` / `semantic_search` to widen the net when the items look thin or off-topic.\n\
- `shortest_path` / `traverse` to show how two things connect.\n\n\
The items above were retrieved from the user's wording alone. If they look thin, off-topic, or miss \
the part being asked about, REWRITE the query in the vocabulary the codebase actually uses and call \
`search` again — swap plain words for likely symbol or file names, drop filler, try a synonym, or \
split a compound question into separate searches. Searching two or three times with better wording \
is normal and expected; answering from a poor first pass is not.\n\n\
Pass arguments as real JSON, not JSON inside a string: `\"nodeId\": \"function:src/a.rs:1:foo\"` for \
one id, `\"nodeId\": [\"id1\", \"id2\"]` for several. Never `\"nodeId\": \"[\\\"id1\\\"]\"`.\n\n\
Prefer one or two well-aimed calls over guessing. Cite retrieved items with [#N] as usual; describe \
tool findings in prose. If the items already answer the question completely, just answer.";

/// Render a retrieval pack into a single prompt string. Each item is
/// labelled `[#i]` so the model can cite it; the answerer can then map
/// `[#i]` back to a `ContextItem` for the final citation list.
///
/// `max_chars` is a soft cap applied across the whole assembled block —
/// once exceeded the remaining items are dropped (head-truncation
/// would split snippets mid-token, which the model handles worse than
/// just omitting the lowest-ranked items).
pub fn render_context(items: &[ContextItem], max_chars: usize) -> String {
    let mut out = String::with_capacity(items.len() * 256);
    for (i, item) in items.iter().enumerate() {
        let header = if item.start_line > 0 && item.end_line >= item.start_line {
            format!(
                "[#{}] {} ({}) — {}:{}-{}",
                i + 1,
                item.name,
                item.node_type,
                if item.file.is_empty() { "<unknown>" } else { item.file.as_str() },
                item.start_line,
                item.end_line
            )
        } else {
            format!(
                "[#{}] {} ({}) — {}",
                i + 1,
                item.name,
                item.node_type,
                if item.file.is_empty() { "<unknown>" } else { item.file.as_str() }
            )
        };

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

        if !out.is_empty() && out.len() + block.len() > max_chars {
            break;
        }
        out.push_str(&block);
    }
    out
}

/// Build the standard prompt (system + RAG context + user query).
pub fn build_rag_messages(
    query: &str,
    context: &RankedContext,
    history: &[ChatMessage],
    system_prompt: Option<&str>,
    ctx_max_chars: usize,
) -> Vec<ChatMessage> {
    let system = system_prompt.unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let mut msgs: Vec<ChatMessage> = Vec::with_capacity(history.len() + 3);

    msgs.push(ChatMessage::new("system", system));

    let rendered = render_context(&context.items, ctx_max_chars);
    let preface = if rendered.is_empty() {
        "No retrieved context was found for this query.".to_string()
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
pub struct ChatRagOutcome {
    /// How many tool calls the model made getting to this answer.
    pub tool_calls: usize,
    pub answer: String,
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

/// Run `search` / `semantic_search` for a chat toolbox.
///
/// The two embedding-backed tools, once, for every transport: `ug chat` and
/// `/api/chat` both offer the model the same schemas and both hold an open
/// store, so both were carrying their own copy of this — the arrangement that
/// let `analyze` work over MCP and fail in chat. Graph-backed tools stay
/// with their caller, which is where the graph lives.
pub async fn run_search_tool(
    name: &str,
    args: &Value,
    store: &dyn KnowledgeStore,
    embedder: Option<&Embedder>,
    repo_root: &std::path::Path,
) -> Result<String, String> {
    let embedder = embedder.ok_or("no embedder configured — semantic tools are offline")?;
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or_default().trim();
    if query.is_empty() {
        return Err("query is required".into());
    }
    let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 25) as usize;

    if name == "semantic_search" {
        let hits = storage_semantic_search(store, embedder, query, k)
            .await
            .map_err(|e| e.to_string())?;
        if hits.is_empty() {
            return Ok("No matches.".into());
        }
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} ({}) — {}:{} · distance {:.3}\n",
                i + 1,
                h.node.name,
                h.node.node_type,
                h.node.file,
                h.node.start_line,
                h.distance
            ));
        }
        return Ok(out);
    }

    let mut opts = SearchKbOptions::new(query, repo_root);
    opts.k = k;
    opts.hops = args.get("hops").and_then(|v| v.as_u64()).unwrap_or(2).min(4) as u32;
    opts.include_snippets = true;
    opts.max_chars = 6_000;
    let ctx = storage_search_kb(store, embedder, opts)
        .await
        .map_err(|e| e.to_string())?;
    Ok(render_context(&ctx.items, 6_000))
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

/// Run the model's tool calls until it answers in prose.
///
/// Tool rounds are deliberately non-streaming: partial `tool_calls` deltas
/// are the messiest part of the OpenAI wire format, and the rounds produce
/// no user-visible text anyway. The caller streams the *final* answer.
/// Returns the messages to send for that final turn, plus the usage the
/// rounds cost.
pub async fn run_tool_rounds<F>(
    chat: &ChatClient,
    toolbox: &ToolBox<'_>,
    mut messages: Vec<ChatMessage>,
    mut on_event: F,
) -> Result<(Vec<ChatMessage>, Option<Usage>, usize), ChatError>
where
    F: FnMut(ToolEvent),
{
    let mut usage: Option<Usage> = None;
    let mut calls = 0usize;
    for _ in 0..toolbox.max_rounds {
        let out = chat.complete_raw(&messages, Some(&toolbox.schemas)).await?;
        usage = merge_usage(usage, out.usage.clone());
        if out.tool_calls.is_empty() {
            // The model answered instead of calling a tool. Drop that draft
            // and let the caller redo it streamed — the context it built up
            // (the tool results) is what matters.
            return Ok((messages, usage, calls));
        }

        // Record the assistant turn verbatim; providers reject tool results
        // that don't follow the call that asked for them.
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: out.content.clone(),
            tool_calls: Some(out.tool_calls.clone()),
            ..Default::default()
        });

        for call in &out.tool_calls {
            let args: Value = serde_json::from_str(&call.function.arguments)
                .unwrap_or(Value::Object(Default::default()));
            let arg_line = compact_args(&args);
            let args_json = serde_json::to_string_pretty(&args).unwrap_or_default();
            on_event(ToolEvent {
                name: call.function.name.clone(),
                args: arg_line.clone(),
                args_json: args_json.clone(),
                summary: None,
                result: None,
            });

            let result = (toolbox.run)(&call.function.name, args).await;
            calls += 1;
            let (text, summary) = match result {
                Ok(t) => {
                    let lines = t.lines().count();
                    (t, format!("{} line(s)", lines))
                }
                Err(e) => (format!("Tool error: {}", e), format!("failed: {}", e)),
            };
            let text = clip_tool_result(&text, toolbox.max_result_chars);
            on_event(ToolEvent {
                name: call.function.name.clone(),
                args: arg_line,
                args_json,
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
    Ok((messages, usage, calls))
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
    fn render_context_numbers_items_and_includes_snippets() {
        let items = vec![
            fake_item(1, Some("fn fn_1() {}")),
            fake_item(2, None),
        ];
        let out = render_context(&items, 10_000);
        assert!(out.contains("[#1]"));
        assert!(out.contains("[#2]"));
        assert!(out.contains("fn_1"));
        assert!(out.contains("fn fn_1() {}"));
        // Header includes the line range.
        assert!(out.contains(":10-15"));
    }

    #[test]
    fn render_context_truncates_at_char_budget() {
        let big_snippet: String = "x".repeat(5_000);
        let items = vec![
            fake_item(1, Some(&big_snippet)),
            fake_item(2, Some(&big_snippet)),
            fake_item(3, Some(&big_snippet)),
        ];
        let out = render_context(&items, 6_000);
        // Should fit the first item but stop before the third.
        assert!(out.contains("[#1]"));
        assert!(!out.contains("[#3]"), "third item should be dropped");
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
        let msgs = build_rag_messages("now?", &ctx, &history, Some("CUSTOM"), 10_000);

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
        let msgs = build_rag_messages("hello", &ctx, &[], None, 10_000);
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

/// Single-turn RAG: retrieve from `store`, then ask `chat` to answer
/// `analyze`. `repo_root` is forwarded to the retrieval pipeline so it
/// can resolve relative source paths when building snippets.
///
/// `toolbox` is threaded through exactly as in [`run_chat_rag_stream`]:
/// whether the caller wants the answer streamed is a transport choice and
/// must not decide whether the model may consult the graph.
pub async fn run_chat_rag(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat: &ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    history: &[ChatMessage],
    opts: ChatRagOptions<'_>,
    toolbox: Option<&ToolBox<'_>>,
) -> Result<ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let t_ret = std::time::Instant::now();
    let context = retrieve_context(store, embedder, repo_root, query, &opts).await?;
    let retrieval_ms = t_ret.elapsed().as_millis();

    let mut messages = build_rag_messages(
        query,
        &context,
        history,
        opts.system_prompt,
        opts.max_context_chars,
    );

    let fast = opts.fast.then(|| fast_client(chat)).flatten();
    let chat = fast.as_ref().unwrap_or(chat);

    let t_cmp = std::time::Instant::now();
    let mut tool_usage = None;
    let mut tool_calls = 0;
    if let Some(tb) = toolbox {
        if let Some(sys) = messages.first_mut().filter(|m| m.role == "system") {
            sys.content.push_str(TOOL_SYSTEM_SUFFIX);
        }
        // No progress feed here: nothing is watching a non-streamed turn.
        let (msgs, usage, calls) = run_tool_rounds(chat, tb, messages, |_| {}).await?;
        messages = msgs;
        tool_usage = usage;
        tool_calls = calls;
    }

    let (answer, usage) = chat.complete(&messages).await?;
    let completion_ms = t_cmp.elapsed().as_millis();

    Ok(ChatRagOutcome {
        answer,
        reasoning: String::new(),
        context,
        retrieval_ms,
        completion_ms,
        usage: merge_usage(tool_usage, usage),
        tool_calls,
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
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat: &ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    history: &[ChatMessage],
    opts: ChatRagOptions<'_>,
    toolbox: Option<&ToolBox<'_>>,
    mut on_context: C,
    mut on_tool: T,
    mut on_delta: F,
) -> Result<ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>>
where
    C: FnMut(&RankedContext),
    T: FnMut(ToolEvent),
    F: FnMut(StreamDelta),
{
    let t_ret = std::time::Instant::now();
    let context = retrieve_context(store, embedder, repo_root, query, &opts).await?;
    let retrieval_ms = t_ret.elapsed().as_millis();
    on_context(&context);

    let mut messages = build_rag_messages(
        query,
        &context,
        history,
        opts.system_prompt,
        opts.max_context_chars,
    );

    let fast = opts.fast.then(|| fast_client(chat)).flatten();
    let chat = fast.as_ref().unwrap_or(chat);

    let t_cmp = std::time::Instant::now();
    // Let the model dig through the graph first — retrieval gives it a
    // starting neighbourhood, the tools let it follow the threads it finds.
    let mut tool_usage = None;
    let mut tool_calls = 0;
    if toolbox.is_some() {
        // Tell it the tools exist, and when they're worth using.
        if let Some(sys) = messages.first_mut().filter(|m| m.role == "system") {
            sys.content.push_str(TOOL_SYSTEM_SUFFIX);
        }
    }
    if let Some(tb) = toolbox {
        let (msgs, usage, calls) =
            run_tool_rounds(chat, tb, messages, |e| on_tool(e)).await?;
        messages = msgs;
        tool_usage = usage;
        tool_calls = calls;
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

    Ok(ChatRagOutcome {
        answer,
        reasoning,
        context,
        retrieval_ms,
        completion_ms,
        usage: merge_usage(tool_usage, usage),
        tool_calls,
    })
}
