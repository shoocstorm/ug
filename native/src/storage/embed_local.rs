//! In-process embedding backend using `fastembed-rs`.
//!
//! On first construction the model weights are downloaded from
//! HuggingFace into a user cache directory and reused across runs.
//! Inference is CPU-bound and synchronous, so [`LocalEmbedder::embed`]
//! is dispatched onto the tokio blocking pool rather than running on
//! the main runtime.

use std::path::PathBuf;
use std::sync::Arc;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::storage::embed::{EmbedError, EmbedderConfig};

pub struct LocalEmbedder {
    /// Wrapped in `Arc` so we can clone across `spawn_blocking`
    /// boundaries without re-loading the ONNX session.
    model: Arc<TextEmbedding>,
    cfg: EmbedderConfig,
}

impl LocalEmbedder {
    /// Loads the model named by `cfg.model`, runs a one-shot embedding
    /// to discover its true output dimension, and patches `cfg.dim`
    /// before returning. This means downstream code that reads
    /// `embedder.config().dim` always agrees with the model that's
    /// actually loaded — no separate probe round-trip required.
    pub fn new(mut cfg: EmbedderConfig) -> Result<Self, EmbedError> {
        let model_enum = resolve_model(&cfg.model)?;
        let cache = local_model_cache_dir();
        let init = InitOptions::new(model_enum)
            .with_cache_dir(cache)
            .with_show_download_progress(true);
        let model = TextEmbedding::try_new(init)
            .map_err(|e| EmbedError::Local(format!("init local embedder: {}", e)))?;

        let probe = model
            .embed(vec!["ping".to_string()], Some(1))
            .map_err(|e| EmbedError::Local(format!("probe local embedder: {}", e)))?;
        let dim = probe.first().map(|v| v.len()).unwrap_or(0);
        if dim == 0 {
            return Err(EmbedError::Local(
                "local embedder returned an empty vector during dim probe".into(),
            ));
        }
        cfg.dim = dim;

        Ok(Self {
            model: Arc::new(model),
            cfg,
        })
    }

    pub fn config(&self) -> &EmbedderConfig {
        &self.cfg
    }

    pub fn set_dim(&mut self, dim: usize) {
        self.cfg.dim = dim;
    }

    pub async fn ping(&self) -> Result<(), EmbedError> {
        // Model loaded successfully in `new`, dim was probed there.
        // No remote service to ping — being constructed is the proof.
        Ok(())
    }

    pub async fn probe_dim(&self) -> Result<usize, EmbedError> {
        Ok(self.cfg.dim)
    }

    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.model.clone();
        let batch = self.cfg.batch_size;
        let dim = self.cfg.dim;
        let texts: Vec<String> = texts.to_vec();

        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, EmbedError> {
            let out = model
                .embed(texts, Some(batch))
                .map_err(|e| EmbedError::Local(format!("embed: {}", e)))?;
            for v in &out {
                if v.len() != dim {
                    return Err(EmbedError::DimensionMismatch {
                        expected: dim,
                        got: v.len(),
                    });
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| EmbedError::Local(format!("blocking task panicked: {}", e)))?
    }
}

/// Where downloaded ONNX weights live. Resolution order:
/// 1. `UG_MODEL_CACHE` env var (full path) — escape hatch for ops.
/// 2. `XDG_CACHE_HOME/ug/models` — Linux convention.
/// 3. Platform default via `dirs::cache_dir()`:
///    - macOS:   `~/Library/Caches/ug/models`
///    - Linux:   `~/.cache/ug/models`
///    - Windows: `%LOCALAPPDATA%\ug\models`
/// 4. Final fallback: `std::env::temp_dir()/ug/models`.
pub fn local_model_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("UG_MODEL_CACHE") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(p).join("ug").join("models");
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ug")
        .join("models")
}

/// Map a user-supplied model name to a fastembed `EmbeddingModel`
/// variant. Accepts either the full HuggingFace identifier
/// (`BAAI/bge-small-en-v1.5`) or a short alias (`bge-small`). Match is
/// case-insensitive and the vendor prefix is stripped, so
/// `BAAI/bge-small-en-v1.5`, `bge-small-en-v1.5`, `bge-small-en`, and
/// `bge-small` all resolve to the same variant.
fn resolve_model(name: &str) -> Result<EmbeddingModel, EmbedError> {
    let lowered = name.trim().to_ascii_lowercase();
    let canon = lowered.rsplit('/').next().unwrap_or(&lowered);
    let variant = match canon {
        // bge-en family (BAAI). 384/768/1024 dim respectively.
        "bge-small-en-v1.5" | "bge-small-en" | "bge-small" => EmbeddingModel::BGESmallENV15,
        "bge-base-en-v1.5" | "bge-base-en" | "bge-base" => EmbeddingModel::BGEBaseENV15,
        "bge-large-en-v1.5" | "bge-large-en" | "bge-large" => EmbeddingModel::BGELargeENV15,

        // sentence-transformers MiniLM. 384 dim, smallest viable model.
        "all-minilm-l6-v2" | "all-minilm" | "minilm" | "minilm-l6" => EmbeddingModel::AllMiniLML6V2,
        "all-minilm-l12-v2" | "minilm-l12" => EmbeddingModel::AllMiniLML12V2,

        // nomic. 768 dim, strong on long context.
        "nomic-embed-text-v1.5" | "nomic-embed" | "nomic" => EmbeddingModel::NomicEmbedTextV15,
        "nomic-embed-text-v1" => EmbeddingModel::NomicEmbedTextV1,

        // intfloat multilingual e5. 384/768/1024 dim.
        "multilingual-e5-small" | "e5-small" => EmbeddingModel::MultilingualE5Small,
        "multilingual-e5-base" | "e5-base" => EmbeddingModel::MultilingualE5Base,
        "multilingual-e5-large" | "e5-large" => EmbeddingModel::MultilingualE5Large,

        // bge-zh for Chinese-heavy codebases / docs.
        "bge-small-zh-v1.5" | "bge-small-zh" => EmbeddingModel::BGESmallZHV15,

        // Code-aware: jina v2 base code (768 dim).
        "jina-embeddings-v2-base-code" | "jina-code" => EmbeddingModel::JinaEmbeddingsV2BaseCode,

        // mixedbread mxbai (1024 dim, top-tier quality).
        "mxbai-embed-large-v1" | "mxbai-large" | "mxbai" => EmbeddingModel::MxbaiEmbedLargeV1,

        _ => {
            return Err(EmbedError::Local(format!(
                "unsupported local embedding model '{name}'. Supported aliases:\n  \
                 - bge-small-en-v1.5 (default, 384d)\n  \
                 - bge-base-en-v1.5  (768d)\n  \
                 - bge-large-en-v1.5 (1024d)\n  \
                 - all-MiniLM-L6-v2  (384d)\n  \
                 - all-MiniLM-L12-v2 (384d)\n  \
                 - nomic-embed-text-v1.5 (768d)\n  \
                 - multilingual-e5-small/base/large\n  \
                 - bge-small-zh-v1.5\n  \
                 - jina-embeddings-v2-base-code (768d, code-aware)\n  \
                 - mxbai-embed-large-v1 (1024d)\n\
                 Or pass --base-url to use a remote OpenAI-compatible endpoint."
            )));
        }
    };
    Ok(variant)
}
