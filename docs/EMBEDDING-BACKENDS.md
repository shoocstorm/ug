# Embedding Backends

`ug` ships with two interchangeable embedding backends. The picker is a
single rule on the CLI (and the equivalent NAPI option):

- **No `--base-url`** → in-process, ONNX-based [`LocalEmbedder`].
  This is the default. No external service required.
- **`--base-url <url>`** → HTTP [`RemoteEmbedder`] against an
  OpenAI-compatible `/v1/embeddings` endpoint.

The same `--model` flag drives both backends. For local it picks an
entry from the fastembed model catalog (resolved via short alias); for
remote it is passed verbatim as the `model` field in the embedding
request body.

---

## Why bother with a local backend?

The previous default was a hosted Qwen3 endpoint at `localhost:8000`.
That worked, but it forced every contributor to:

1. Spin up an embedding server before any `ug ingest` could run.
2. Keep its model + dim aligned with whatever `ug` expected.
3. Manage GPU/memory for that sidecar.

Most users just want to index a repo and get a knowledge graph. Local
embedding removes the sidecar from the critical path.

We kept `--base-url` because:

- Hosted endpoints are dramatically faster on large repos (GPU > CPU
  ONNX by 5–20×).
- Some teams already operate a shared embedding service.
- Some embedding models (OpenAI `text-embedding-3-large`, Cohere, etc.)
  are remote-only.

---

## Architecture

```
                         ┌──────────────────────────┐
                         │      ingest_graph        │
                         │  query::semantic_search  │
                         │  query::search_kb        │
                         └────────────┬─────────────┘
                                      │ &Embedder
                                      ▼
                  ┌─────────────────────────────────────┐
                  │              Embedder               │  ← public façade
                  │           (enum dispatcher)         │     storage/embed.rs
                  └────────────┬───────────┬────────────┘
                               │           │
                  Embedder::Local         Embedder::Remote
                               │           │
                               ▼           ▼
                  ┌────────────────────┐  ┌─────────────────────────┐
                  │   LocalEmbedder    │  │     RemoteEmbedder      │
                  │ embed_local.rs     │  │      embed.rs           │
                  │                    │  │                         │
                  │ Arc<TextEmbedding> │  │   reqwest::Client       │
                  │     (fastembed)    │  │   /v1/embeddings POST   │
                  └────────┬───────────┘  └────────────┬────────────┘
                           │                           │
                           ▼                           ▼
                   ┌──────────────┐          ┌──────────────────┐
                   │ ort (ONNX)   │          │ user's hosted    │
                   │ + tokenizers │          │ embedding server │
                   │ ~/Library/   │          │ (any OpenAI-     │
                   │   Caches/ug  │          │  compatible API) │
                   └──────────────┘          └──────────────────┘
```

Key points:

- `Embedder` is a public enum with two variants. Every method
  (`embed`, `probe_dim`, `ping`, `config`, `set_dim`) routes through
  `match self`. Callers stay backend-agnostic.
- `LocalEmbedder` wraps fastembed's `TextEmbedding` in an `Arc` so the
  ONNX session can be shared across `tokio::task::spawn_blocking` calls
  without re-loading.
- `RemoteEmbedder` is the original HTTP client, unchanged, just renamed.

---

## Local backend (default)

### Selection

```bash
# Default — no flags needed.
ug ingest -i ugout/graph.json -o ugout/ugdb

# Pick a different local model.
ug ingest --model nomic-embed-text-v1.5
```

### Supported aliases

Aliases are case-insensitive. The vendor prefix (`BAAI/`, `nomic-ai/`,
`sentence-transformers/`, etc.) is stripped, so the full HuggingFace ID
and the short name resolve to the same variant.

| Alias                          | Family            | Dim  | Approx size | Notes |
|--------------------------------|-------------------|------|-------------|-------|
| `bge-small-en-v1.5` *(default)*| BAAI BGE          |  384 | ~130 MB     | Best size/quality trade-off |
| `bge-base-en-v1.5`             | BAAI BGE          |  768 | ~440 MB     | Higher quality, larger |
| `bge-large-en-v1.5`            | BAAI BGE          | 1024 | ~1.3 GB     | Heavyweight |
| `all-MiniLM-L6-v2`             | sentence-trans.   |  384 |  ~22 MB     | Smallest viable |
| `all-MiniLM-L12-v2`            | sentence-trans.   |  384 |  ~33 MB     | A bit better than L6 |
| `nomic-embed-text-v1.5`        | Nomic             |  768 | ~270 MB     | Strong on long context |
| `nomic-embed-text-v1`          | Nomic             |  768 | ~270 MB     | Older variant |
| `multilingual-e5-small`        | intfloat          |  384 | ~120 MB     | Multilingual |
| `multilingual-e5-base`         | intfloat          |  768 | ~280 MB     | Multilingual |
| `multilingual-e5-large`        | intfloat          | 1024 | ~560 MB     | Multilingual |
| `bge-small-zh-v1.5`            | BAAI BGE-zh       |  512 | ~100 MB     | Chinese-heavy content |
| `jina-embeddings-v2-base-code` | Jina              |  768 | ~320 MB     | Code-aware |
| `mxbai-embed-large-v1`         | mixedbread        | 1024 | ~700 MB     | Top-tier quality |

Unrecognized model names produce a clear error listing the supported
aliases.

### Model cache

ONNX weights and the tokenizer config are downloaded once and reused
across runs. Resolution order:

1. `$UG_MODEL_CACHE` (full path) — escape hatch for ops / CI.
2. `$XDG_CACHE_HOME/ug/models` — Linux convention.
3. `dirs::cache_dir()/ug/models` — platform default:
   - macOS: `~/Library/Caches/ug/models`
   - Linux: `~/.cache/ug/models`
   - Windows: `%LOCALAPPDATA%\ug\models`
4. `std::env::temp_dir()/ug/models` — last resort.

Each model lives in its own subdirectory. Deleting `~/Library/Caches/ug`
forces a fresh download.

### Threading

`fastembed::TextEmbedding::embed` is synchronous and CPU-bound. To keep
the tokio runtime responsive, `LocalEmbedder::embed` dispatches the
work onto the blocking pool:

```rust
tokio::task::spawn_blocking(move || {
    model.embed(texts, Some(batch_size))
}).await?
```

ONNX itself uses multiple threads internally; we don't add a second
parallel layer on top.

### Dimension handling

`LocalEmbedder::new` runs a one-shot embedding of the string `"ping"`
during construction and patches `cfg.dim` to match the model's actual
output. This means downstream code that reads `embedder.config().dim`
always agrees with reality — no separate `probe_dim` round-trip is
needed.

---

## Remote backend

### Selection

```bash
# OpenAI-style hosted service.
ug ingest --base-url https://api.openai.com/v1 \
          --api-key $OPENAI_API_KEY \
          --model text-embedding-3-small

# Local Ollama / vLLM / LM Studio / TEI exposing /v1/embeddings.
ug ingest --base-url http://localhost:8000/v1 \
          --model openai/Qwen3-Embedding-0.6B-4bit-DWQ
```

The remote backend is the legacy implementation (renamed from
`Embedder` → `RemoteEmbedder`); behavior is unchanged.

### Auto-dim probe

When `--embedding-dim` is **not** passed, `ug ingest` (and the NAPI
`db_ingest`) call `probe_dim()` once. This issues a single embedding
request, reads `data[0].embedding.len()`, and patches the embedder so
the per-batch dim validator agrees with the model's real output. The
discovered dim is then persisted to `<db>/ug-meta.json` on the first
ingest, and any later open with a mismatched dim returns
`DbError::DimMismatch` instead of silently corrupting the index.

---

## CLI / NAPI / serve integration

| Surface                  | Selection rule                                                  |
|--------------------------|-----------------------------------------------------------------|
| `ug ingest` / `ug gen`   | `--base-url` present → remote, else local                       |
| `ug serve`               | Same rule, applied during `embedder_from_args`                  |
| `ug semantic_search`     | Same                                                            |
| `ug hybrid_search`       | Same                                                            |
| NAPI `db_ingest`         | `embedderOptions.baseUrl` non-empty → remote, else local        |
| NAPI `db_*_search`       | Same                                                            |
| NAPI `pingEmbedder`      | Same                                                            |

Selection is **per call**. There is no global switch and no
configuration file — every entry point inspects its own arguments.

---

## Defaults

```rust
pub const DEFAULT_MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";  // remote-only
pub const DEFAULT_API_KEY: &str = "1234";
pub const DEFAULT_EMBEDDING_DIM: usize = 384;  // matches default model
pub const DEFAULT_BATCH_SIZE: usize = 32;
```

`DEFAULT_BASE_URL` is only consulted when the user has explicitly
opted into the remote backend (i.e. the field is set in
`EmbedderConfig`, not when it equals the literal default — `--base-url`
presence is the only switch).

`DEFAULT_EMBEDDING_DIM` doubles as the fallback for legacy databases
that pre-date the `<db>/ug-meta.json` sidecar. New ingests always write
the actual model dim to the sidecar.

---

## Tradeoffs

### Binary size

Adding `fastembed` (which pulls `ort`, `tokenizers`, `hf-hub`) increases
the `ug` binary by ~5–10 MB on macOS arm64. Acceptable for a CLI tool;
if you want a "thin remote-only" build, the recommended approach is to
gate `LocalEmbedder` and the `fastembed` dep behind a Cargo feature
`local-embed` and disable it. Not done today — flag as TODO if it
becomes painful.

### First-run latency

Local backend downloads model weights on first use (22 MB – 1.3 GB
depending on alias). Subsequent runs are instant. Show progress is
enabled by default so users see what's happening:

```
Downloading model.onnx ▰▰▰▰▰▰▰▱▱▱▱  62%  35.2 MB/56.8 MB  3.4 MB/s
```

### Throughput

Rule of thumb on a modern Mac (M-series):

| Backend                             | Throughput        | Notes                          |
|-------------------------------------|-------------------|--------------------------------|
| Local `bge-small-en-v1.5` (CPU ONNX)| ~150 nodes/s      | Fine up to ~100k nodes         |
| Local `bge-large-en-v1.5` (CPU ONNX)| ~25 nodes/s       | Painful past ~30k nodes        |
| Remote OpenAI `text-embedding-3-small`| ~500 nodes/s    | Network-bound; batched         |
| Remote local Ollama / TEI on GPU    | ~2000+ nodes/s    | GPU does the heavy lifting     |

If your repo has hundreds of thousands of nodes, prefer remote. The
local backend is for first-time setup, demos, offline work, and small-
to-medium repos.

### Cross-compilation

`ort` ships pre-built ONNX Runtime dylibs per target. For a normal
`cargo build` on each host machine, no extra steps are needed. For
cross-compilation (e.g. CI building Linux x64, macOS arm64, and
Windows from a single host), you'll need to fetch the right ORT
release per target — see `ort` docs.

---

## Failure modes

| Symptom                                                  | Cause / fix                                                            |
|----------------------------------------------------------|-----------------------------------------------------------------------|
| `unsupported local embedding model 'foo'`                | `--model` not in alias table. Use one of the listed aliases or pass `--base-url` to use a remote endpoint with that model. |
| `init local embedder: ...` (network error)               | First run can't reach HuggingFace. Set `$HF_ENDPOINT` to a mirror, or pre-download into `$UG_MODEL_CACHE`. |
| `embedding dim mismatch: expected 384, got 768`          | The model's true dim disagrees with what was persisted. Delete `<db>/ug-meta.json` (and the DB) for a fresh start, or pass `--embedding-dim` to match. |
| `embedding bad status 401: ...`                          | Remote endpoint rejected the API key. Pass `--api-key`. |
| `embedding bad status 404: ...` against an OpenAI URL    | `--model` not recognized by the remote provider. |
| Local backend hangs on first run                         | Likely downloading the model (no progress bar in some shells). Watch network. |

---

## Files

| File                                       | Role                                                |
|--------------------------------------------|-----------------------------------------------------|
| `native/src/storage/embed.rs`              | `Embedder` enum, `RemoteEmbedder`, `EmbedderConfig`, defaults |
| `native/src/storage/embed_local.rs`        | `LocalEmbedder`, alias resolver, cache dir picker   |
| `native/src/storage/mod.rs`                | Module wiring + public re-exports                   |
| `native/src/storage/napi_bindings.rs`      | NAPI `build_embedder`: branches on `baseUrl`        |
| `native/src/main.rs`                       | CLI `embedder_from_args`: branches on `--base-url`  |
| `native/Cargo.toml`                        | `fastembed = "4"`, `dirs = "5"`                     |

---

## Future work

- **Cargo feature gate.** Move local backend behind `--features local-embed`
  for users who want a thin remote-only build.
- **GPU execution providers.** `ort` supports CoreML / DirectML / CUDA
  via execution-provider configuration. Wire that through
  `InitOptions::with_execution_providers` for users who want to push
  the local path harder.
- **Model auto-download verification.** Pin SHA256 hashes per model
  alias and verify after download.
- **Per-call model override.** Today, `--model` is read once at
  construction. For workloads that mix model families, expose a
  per-call override.
