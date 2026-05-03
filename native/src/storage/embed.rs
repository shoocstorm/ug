//! HTTP client for an OpenAI-compatible `/v1/embeddings` endpoint.
//!
//! The default config targets the local Qwen3-Embedding model described
//! in docs/GRAPH-STORAGE.md (1024-dim cosine), but the dimension is a
//! runtime field on `EmbedderConfig` so other models (nomic 768-dim,
//! OpenAI 1536/3072, etc.) work without recompiling. We batch requests
//! (default 32 inputs per call) to stay within per-request limits.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "openai/Qwen3-Embedding-0.6B-4bit-DWQ";
pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";
pub const DEFAULT_API_KEY: &str = "1234";
pub const DEFAULT_EMBEDDING_DIM: usize = 1024;
/// Legacy alias preserved for existing tests/benches that use this as the
/// fixture size. New code should use `EmbedderConfig::dim` instead.
pub const EMBEDDING_DIM: usize = DEFAULT_EMBEDDING_DIM;
pub const DEFAULT_BATCH_SIZE: usize = 32;

#[derive(Clone, Debug)]
pub struct EmbedderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub dim: usize,
    pub batch_size: usize,
    pub timeout_secs: u64,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: DEFAULT_API_KEY.to_string(),
            model: DEFAULT_MODEL.to_string(),
            dim: DEFAULT_EMBEDDING_DIM,
            batch_size: DEFAULT_BATCH_SIZE,
            timeout_secs: 120,
        }
    }
}

impl EmbedderConfig {
    pub fn with_overrides(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        dim: Option<usize>,
        batch_size: Option<usize>,
        timeout_secs: Option<u64>,
    ) -> Self {
        let mut cfg = Self::default();
        if let Some(b) = base_url {
            cfg.base_url = b;
        }
        if let Some(a) = api_key {
            cfg.api_key = a;
        }
        if let Some(m) = model {
            cfg.model = m;
        }
        if let Some(d) = dim {
            cfg.dim = d;
        }
        if let Some(bs) = batch_size {
            cfg.batch_size = bs;
        }
        if let Some(t) = timeout_secs {
            cfg.timeout_secs = t;
        }
        cfg
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

    /// Override the configured embedding dimension. Used by the napi
    /// `db_ingest` path when the dim was not specified by the caller —
    /// we probe the endpoint and patch the embedder so its per-batch
    /// validator agrees with the model's actual output size.
    pub fn set_dim(&mut self, dim: usize) {
        self.cfg.dim = dim;
    }

    pub async fn ping(&self) -> Result<(), EmbedError> {
        self.probe_dim().await.map(|_| ())
    }

    /// Probe the endpoint with a single input and return the discovered
    /// embedding dimension. Useful for callers that want to detect the
    /// model's dim instead of pre-configuring it.
    pub async fn probe_dim(&self) -> Result<usize, EmbedError> {
        let probe = vec!["ping".to_string()];
        // Bypass the configured-dim validator inside `embed` so callers
        // can use `probe_dim` to discover an unknown dim. We re-issue a
        // minimal request here.
        let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
        let req = EmbeddingRequest {
            model: &self.cfg.model,
            input: &probe,
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
        let item = parsed.data.into_iter().next().ok_or(EmbedError::DimensionMismatch {
            expected: self.cfg.dim,
            got: 0,
        })?;
        Ok(item.embedding.len())
    }

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

            let mut items = parsed.data;
            items.sort_by_key(|i| i.index);
            for item in items {
                if item.embedding.len() != self.cfg.dim {
                    return Err(EmbedError::DimensionMismatch {
                        expected: self.cfg.dim,
                        got: item.embedding.len(),
                    });
                }
                out.push(item.embedding);
            }
        }

        Ok(out)
    }
}