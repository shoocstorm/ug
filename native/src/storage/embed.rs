//! Embedding backend dispatcher.
//!
//! Two backends sit behind a single [`Embedder`] enum so callers don't
//! care which one is in use:
//!
//! * [`LocalEmbedder`] — in-process ONNX inference via `fastembed-rs`.
//!   No external service required; the model is downloaded to a user
//!   cache on first use. This is the default.
//! * [`RemoteEmbedder`] — HTTP client for an OpenAI-compatible
//!   `/v1/embeddings` endpoint. Selected when the caller explicitly
//!   provides `--base-url`.
//!
//! The downstream API (`embed`, `probe_dim`, `ping`, `config`,
//! `set_dim`) is identical for both, so `ingest.rs` / `query.rs` /
//! `napi_bindings.rs` are agnostic to the backend.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::storage::embed_local::LocalEmbedder;

/// Default model. Resolved against fastembed's catalog for the local
/// backend, and passed verbatim as the `model` field for the remote
/// backend (OpenAI-compatible endpoints expect a model name).
pub const DEFAULT_MODEL: &str = "BAAI/bge-small-en-v1.5";
/// Only used when the user opts into the remote backend with
/// `--base-url`. The local backend ignores this entirely.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";
pub const DEFAULT_API_KEY: &str = "1234";
/// 384 matches `bge-small-en-v1.5` and `all-MiniLM-L6-v2`. Acts as the
/// fallback dim for legacy databases without a `ug-meta.json` sidecar.
pub const DEFAULT_EMBEDDING_DIM: usize = 384;
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
    /// In-process inference (model load, tokenizer, ONNX session) failed.
    Local(String),
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
            EmbedError::Local(msg) => write!(f, "local embedding error: {}", msg),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Public façade. The two variants share the same surface so callers
/// don't branch — `match self` only happens inside this enum.
pub enum Embedder {
    Local(LocalEmbedder),
    Remote(RemoteEmbedder),
}

impl Embedder {
    /// Default constructor — picks the **local** backend. Preserved so
    /// existing call sites keep compiling. Use `Embedder::remote` to
    /// opt into the HTTP backend.
    pub fn new(cfg: EmbedderConfig) -> Result<Self, EmbedError> {
        Self::local(cfg)
    }

    /// In-process embeddings via fastembed-rs. The model is downloaded
    /// (and cached) on first construction, which can take 30-60 s for
    /// a 22-130 MB model.
    pub fn local(cfg: EmbedderConfig) -> Result<Self, EmbedError> {
        LocalEmbedder::new(cfg).map(Self::Local)
    }

    /// HTTP backend against an OpenAI-compatible `/v1/embeddings`
    /// endpoint. Use this when `--base-url` is supplied.
    pub fn remote(cfg: EmbedderConfig) -> Result<Self, EmbedError> {
        RemoteEmbedder::new(cfg).map(Self::Remote)
    }

    pub fn config(&self) -> &EmbedderConfig {
        match self {
            Embedder::Local(e) => e.config(),
            Embedder::Remote(e) => e.config(),
        }
    }

    /// Override the configured embedding dimension. Used by the napi
    /// `db_ingest` path when the dim was not specified by the caller —
    /// we probe the endpoint and patch the embedder so its per-batch
    /// validator agrees with the model's actual output size.
    pub fn set_dim(&mut self, dim: usize) {
        match self {
            Embedder::Local(e) => e.set_dim(dim),
            Embedder::Remote(e) => e.set_dim(dim),
        }
    }

    pub async fn ping(&self) -> Result<(), EmbedError> {
        match self {
            Embedder::Local(e) => e.ping().await,
            Embedder::Remote(e) => e.ping().await,
        }
    }

    pub async fn probe_dim(&self) -> Result<usize, EmbedError> {
        match self {
            Embedder::Local(e) => e.probe_dim().await,
            Embedder::Remote(e) => e.probe_dim().await,
        }
    }

    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        match self {
            Embedder::Local(e) => e.embed(texts).await,
            Embedder::Remote(e) => e.embed(texts).await,
        }
    }
}

/// HTTP client for an OpenAI-compatible `/v1/embeddings` endpoint.
///
/// We batch requests (default 32 inputs per call) to stay within
/// per-request limits.
pub struct RemoteEmbedder {
    cfg: EmbedderConfig,
    client: reqwest::Client,
}

impl RemoteEmbedder {
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
