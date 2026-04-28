//! HTTP client for an OpenAI-compatible `/v1/embeddings` endpoint.
//!
//! Configured by default for the local Qwen3-Embedding model described in
//! docs/GRAPH-STORAGE.md. The model returns 1024-dimensional vectors. We
//! batch requests (default 32 inputs per call) to stay within the server's
//! per-request limits without paying a round-trip per node.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "openai/Qwen3-Embedding-0.6B-4bit-DWQ";
pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";
pub const DEFAULT_API_KEY: &str = "1234";
pub const EMBEDDING_DIM: usize = 1024;
pub const DEFAULT_BATCH_SIZE: usize = 32;

#[derive(Clone, Debug)]
pub struct EmbedderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub batch_size: usize,
    pub timeout_secs: u64,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: DEFAULT_API_KEY.to_string(),
            model: DEFAULT_MODEL.to_string(),
            batch_size: DEFAULT_BATCH_SIZE,
            timeout_secs: 120,
        }
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Debug)]
pub enum EmbedError {
    Http(reqwest::Error),
    BadStatus(u16, String),
    DimensionMismatch { expected: usize, got: usize },
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Http(e) => write!(f, "embedding http error: {}", e),
            EmbedError::BadStatus(code, body) => {
                write!(f, "embedding bad status {}: {}", code, body)
            }
            EmbedError::DimensionMismatch { expected, got } => {
                write!(f, "embedding dim mismatch: expected {}, got {}", expected, got)
            }
        }
    }
}

impl std::error::Error for EmbedError {}

pub struct Embedder {
    cfg: EmbedderConfig,
    client: reqwest::Client,
}

impl Embedder {
    pub fn new(cfg: EmbedderConfig) -> Result<Self, EmbedError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(EmbedError::Http)?;
        Ok(Self { cfg, client })
    }

    pub fn config(&self) -> &EmbedderConfig {
        &self.cfg
    }

    /// Embed a batch of texts. Returns one vector per input in the same
    /// order. Splits the input into sub-batches of `cfg.batch_size` so the
    /// server doesn't receive an unbounded payload.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(self.cfg.batch_size) {
            let chunk_vec: Vec<String> = chunk.to_vec();
            let req = EmbeddingRequest {
                model: &self.cfg.model,
                input: &chunk_vec,
            };

            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.cfg.api_key)
                .json(&req)
                .send()
                .await
                .map_err(EmbedError::Http)?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(EmbedError::BadStatus(status.as_u16(), body));
            }

            let parsed: EmbeddingResponse = resp.json().await.map_err(EmbedError::Http)?;

            // The server may return items out of order; sort by `index` so
            // each vector lines up with its input text.
            let mut items = parsed.data;
            items.sort_by_key(|i| i.index);
            for item in items {
                if item.embedding.len() != EMBEDDING_DIM {
                    return Err(EmbedError::DimensionMismatch {
                        expected: EMBEDDING_DIM,
                        got: item.embedding.len(),
                    });
                }
                out.push(item.embedding);
            }
        }

        Ok(out)
    }
}
