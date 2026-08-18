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
//! `set_dim`) is identical for both, so `ingest.rs` / `query.rs` are
//! agnostic to the backend.

use futures::stream::{self, StreamExt, TryStreamExt};
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
pub const DEFAULT_BATCH_SIZE: usize = 32;
/// How many embedding requests the remote backend keeps in flight.
///
/// A cold ingest of a large repo is thousands of batches, and issued one at
/// a time the whole run is round-trip latency — at 200 ms RTT, 5,000 batches
/// is ~17 minutes of waiting on a socket. Eight is deliberately modest: it is
/// a real speedup against a local inference server without looking like a
/// denial-of-service to a shared or metered endpoint.
///
/// Override with `UG_EMBED_CONCURRENCY` when the endpoint is rate limited
/// (set it to `1` to restore the old strictly-sequential behaviour).
pub const DEFAULT_EMBED_CONCURRENCY: usize = 8;

/// `UG_EMBED_CONCURRENCY`, or [`DEFAULT_EMBED_CONCURRENCY`] when unset or
/// unparseable. Zero is treated as one — no concurrency setting should be
/// able to stall the pipeline outright.
fn env_concurrency() -> usize {
    std::env::var("UG_EMBED_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.max(1))
        .unwrap_or(DEFAULT_EMBED_CONCURRENCY)
}

#[derive(Clone, Debug)]
pub struct EmbedderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub dim: usize,
    pub batch_size: usize,
    /// Requests in flight at once. Remote backend only — the local one runs
    /// in a single `spawn_blocking` over fastembed's own thread pool, where a
    /// second concurrent call would contend rather than help.
    pub concurrency: usize,
    pub timeout_secs: u64,
}

impl EmbedderConfig {
    /// How many texts to hand [`Embedder::embed`] in one call so it has
    /// enough work to fill `concurrency` requests.
    ///
    /// Callers that chunk for their own reasons (a progress meter) should
    /// chunk by this rather than by `batch_size`: feeding one `batch_size`
    /// at a time serializes the pipeline no matter what `concurrency` says.
    pub fn embed_chunk(&self) -> usize {
        self.batch_size.max(1) * self.concurrency.max(1)
    }
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: DEFAULT_API_KEY.to_string(),
            model: DEFAULT_MODEL.to_string(),
            dim: DEFAULT_EMBEDDING_DIM,
            batch_size: DEFAULT_BATCH_SIZE,
            concurrency: env_concurrency(),
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

    /// Override the configured embedding dimension. Used by the reindex
    /// ingest path when the dim was not specified by the caller —
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

    /// One batch, one request. `input` borrows the caller's slice — the
    /// request type takes `&[String]`, so there is nothing to clone.
    async fn embed_batch(&self, url: &str, chunk: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let req = EmbeddingRequest {
            model: &self.cfg.model,
            input: chunk,
        };

        let resp = self
            .client
            .post(url)
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

        // The endpoint may answer out of order; `index` is what puts a
        // vector back against its input.
        let mut items = parsed.data;
        items.sort_by_key(|i| i.index);

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if item.embedding.len() != self.cfg.dim {
                return Err(EmbedError::DimensionMismatch {
                    expected: self.cfg.dim,
                    got: item.embedding.len(),
                });
            }
            out.push(item.embedding);
        }
        Ok(out)
    }

    /// Embed every text, `concurrency` batches in flight at a time.
    ///
    /// `buffered` yields results in **input order** regardless of which
    /// request finishes first, so the returned vectors line up with `texts`
    /// exactly as they did when this was a sequential loop — callers index
    /// the two together and would silently mis-assign every vector otherwise.
    ///
    /// Errors are fail-fast, matching the previous behaviour: `try_collect`
    /// abandons the stream on the first `Err`, which drops the in-flight
    /// requests and cancels them. The caller (`rows_from_plan`, and the two
    /// loops in `cli::ingest`) already degrades by writing the remaining
    /// nodes without vectors, so failing early costs nothing and avoids
    /// hammering an endpoint that has started refusing.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
        let concurrency = self.cfg.concurrency.max(1);

        // The per-batch futures are built eagerly into a `Vec` rather than
        // lazily inside `.map(|chunk| ...)`. A closure there would be
        // higher-ranked over the chunk's lifetime, and the opaque future this
        // function returns then fails `Send` inference — not here, but at
        // every `tokio::spawn` that transitively awaits an ingest, with the
        // useless "implementation of `Send` is not general enough" pointing
        // at unrelated code in `serve.rs`. Building them up front gives each
        // one a concrete lifetime tied to this call and the problem vanishes.
        // Nothing is polled until `buffered` polls it, so this stays lazy in
        // the way that matters.
        let pending: Vec<_> = texts
            .chunks(self.cfg.batch_size.max(1))
            .map(|chunk| self.embed_batch(&url, chunk))
            .collect();

        let batches: Vec<Vec<Vec<f32>>> = stream::iter(pending)
            .buffered(concurrency)
            .try_collect()
            .await?;

        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in batches {
            out.extend(batch);
        }
        Ok(out)
    }
}
