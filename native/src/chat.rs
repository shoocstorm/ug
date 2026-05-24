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

use ultragraph::storage::{
    search_kb as storage_search_kb, ContextItem, Direction, Embedder, KnowledgeStore,
    RankStrategy, RankedContext, SearchKbOptions,
};

/// Default chat model. Picked so the CLI works as soon as the user
/// points `--base-url` at any OpenAI-compatible chat endpoint; the
/// caller almost always wants to override this with `--chat-model`.
pub const DEFAULT_CHAT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_CHAT_BASE_URL: &str = "http://127.0.0.1:8000/v1";
pub const DEFAULT_CHAT_API_KEY: &str = "1234";
pub const DEFAULT_TEMPERATURE: f32 = 0.2;
pub const DEFAULT_MAX_TOKENS: u32 = 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Reasonable cap on per-chunk character budget for `assemble_context`.
/// Keeps the prompt under ~12k chars (~3-4k tokens) which fits inside
/// the context window of the smallest deployable chat models.
pub const DEFAULT_CTX_MAX_CHARS: usize = 12_000;

#[derive(Clone, Debug)]
pub struct ChatConfig {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    stream: bool,
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

    /// Single non-streaming round-trip. Returns the assistant text and
    /// (when the server reports it) token-usage stats.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
    ) -> Result<(String, Option<Usage>), ChatError> {
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
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&req)
            .send()
            .await
            .map_err(ChatError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChatError::BadStatus(status.as_u16(), body));
        }

        let parsed: ChatResponse = resp.json().await.map_err(ChatError::Http)?;
        let choice = parsed.choices.into_iter().next().ok_or(ChatError::EmptyChoices)?;
        let text = choice.message.content.unwrap_or_default();
        let _ = choice.finish_reason; // currently ignored; surfaced via logs upstream if needed
        Ok((text, parsed.usage))
    }
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

    msgs.push(ChatMessage {
        role: "system".into(),
        content: system.into(),
    });

    let rendered = render_context(&context.items, ctx_max_chars);
    let preface = if rendered.is_empty() {
        "No retrieved context was found for this query.".to_string()
    } else {
        format!(
            "Retrieved context (cite as [#N]):\n\n{}\n---",
            rendered.trim_end()
        )
    };
    msgs.push(ChatMessage {
        role: "system".into(),
        content: preface,
    });

    // Prior turns (already in role/content shape).
    for m in history {
        msgs.push(m.clone());
    }

    msgs.push(ChatMessage {
        role: "user".into(),
        content: query.to_string(),
    });

    msgs
}

// ---------- Orchestrator ----------

/// One pass of "retrieve → prompt → answer". Used by both the CLI and
/// the HTTP layer so the behaviour is identical regardless of entry
/// point. Returns the answer text, the retrieval result, and timing /
/// usage info so callers can surface latency and token counts.
pub struct ChatRagOutcome {
    pub answer: String,
    pub context: RankedContext,
    pub retrieval_ms: u128,
    pub completion_ms: u128,
    pub usage: Option<Usage>,
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
            max_context_chars: DEFAULT_CTX_MAX_CHARS,
            where_clause: None,
            system_prompt: None,
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
            ChatMessage { role: "user".into(), content: "prev?".into() },
            ChatMessage { role: "assistant".into(), content: "prev!".into() },
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
}

/// Single-turn RAG: retrieve from `store`, then ask `chat` to answer
/// `query`. `repo_root` is forwarded to the retrieval pipeline so it
/// can resolve relative source paths when building snippets.
pub async fn run_chat_rag(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat: &ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    history: &[ChatMessage],
    opts: ChatRagOptions<'_>,
) -> Result<ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let t_ret = std::time::Instant::now();
    let mut search_opts = SearchKbOptions::new(query, repo_root);
    search_opts.k = opts.k;
    search_opts.hops = opts.hops;
    search_opts.strategy = opts.strategy;
    search_opts.direction = opts.direction;
    search_opts.edge_types = opts.edge_types;
    search_opts.include_snippets = opts.include_snippets;
    search_opts.max_chars = opts.max_context_chars;
    search_opts.where_clause = opts.where_clause;

    let context = storage_search_kb(store, embedder, search_opts).await?;
    let retrieval_ms = t_ret.elapsed().as_millis();

    let messages = build_rag_messages(
        query,
        &context,
        history,
        opts.system_prompt,
        opts.max_context_chars,
    );

    let t_cmp = std::time::Instant::now();
    let (answer, usage) = chat.complete(&messages).await?;
    let completion_ms = t_cmp.elapsed().as_millis();

    Ok(ChatRagOutcome {
        answer,
        context,
        retrieval_ms,
        completion_ms,
        usage,
    })
}
