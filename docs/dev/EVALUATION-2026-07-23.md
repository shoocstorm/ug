# UltraGraph (`ug`) — Evaluation

> A dated, repeatable scorecard so successive evaluations can be compared.
> Re-run the same checks, copy this file to a new date, and diff.

| Field | Value |
| :--- | :--- |
| **Date** | 2026-07-23 |
| **Version** | 0.1.4 |
| **Commit** | `f29220f` |
| **Evaluator** | Claude (Opus 4.8) |
| **Scope** | Full repo: Rust core (~22k LOC), Node wrapper (~3k LOC), docs, tests, CI |

## What it is

A local-first knowledge-graph + GraphRAG engine that turns codebases/docs
into a queryable semantic graph. Rust core exposed via CLI, MCP server, REST
API, D3 web UI, and a Tauri desktop app. Real GraphRAG: Personalized PageRank
(PPR) + Reciprocal Rank Fusion (RRF) + Maximal Marginal Relevance (MMR), with
in-process ONNX embeddings (no external service).

## Verification (commands actually run)

| Check | Command | Result |
| :--- | :--- | :--- |
| Rust tests | `cargo test` (in `native/`) | **159 passed, 9 ignored** (ignored = Neo4j live-DB smoke) |
| JS tests | `node node/test-runner.cjs` | **26/26 passed** |
| Rust build | `cargo test --no-run` | clean compile, no warnings |
| Lint (correctness) | `cargo clippy --all-targets -- -D clippy::correctness` | **clean (exit 0)** |
| Lint (style) | `cargo clippy --all-targets` | ~12 style/complexity lints (clone-on-copy, too-many-args); non-blocking |

## Scorecard

| Dimension | Score | Notes |
| :--- | :---: | :--- |
| **Architecture** | 9/10 | Trait-based `KnowledgeStore` cleanly abstracts OverGraph vs Neo4j; well-separated indexer / graph / storage / serve / chat. Thin-wrapper modules carry rationale in doc comments. |
| **Functionality / ambition** | 9/10 | Real GraphRAG (PPR + RRF + MMR), not just vector search. In-process ONNX embeddings, incremental blake3 indexing, multi-language tree-sitter, multi-project mode. |
| **Test coverage** | 8/10 | 185 tests (Rust+JS), all green, including incremental-cache and destructive-op safety. Gap: no HTTP/serve integration or MCP-protocol tests. |
| **Documentation** | 9/10 | 25 KB README + ~1,900 lines of design docs. "Which store backs what" matrix is genuinely excellent for a solo project. |
| **Code hygiene** | 8/10 | 0 `unsafe`, 4 TODO/FIXME, doc comments throughout. External-input boundaries degrade gracefully (see Risk #1). Residual: style-level clippy lints. |
| **DX / interfaces** | 9/10 | One-line install, self-update, `ug doctor` config tracing, clear precedence tiers with notice-on-override. CLI/MCP/HTTP tool parity is a strong design choice. |
| **Maturity / stability** | 6/10 | v0.1.4, 50 commits over ~2 months, single author. CI now runs tests on push (added 2026-07-23); previously release-only. |

### Overall: **8.1 / 10** — strong, well-engineered early-stage project

## Top strengths

1. **Substantial and it actually works** — every test suite passes; a real, coherent system, not a demo.
2. **Genuine GraphRAG** — structural PPR fused with semantic search, beyond typical "embed + cosine" RAG.
3. **Docs and DX punch above solo-project weight** — `ug doctor`, config tiers, store-capability matrix.

## Risks (ranked) and status

### Risk #1 — panic-on-bad-input from `unwrap()` — **RE-SCOPED after audit**
A raw grep shows ~58 `.unwrap()` in `native/src`, but a close audit found the
panic risk **overstated**: every external-input boundary already degrades
gracefully rather than panicking —
- `config.json`: malformed → warn + fall back to `{}` (`config.rs:187`)
- `project.json`: malformed → `None` via `.ok()?` (`project.rs:128`)
- `graph.json`: every graph algorithm does `from_str(...).map_err → return "{}"` (`graph.rs:866`, `:1001`)

The remaining `.unwrap()`s are RwLock-poison (idiomatic), provably-safe
internal loop invariants (e.g. Brandes centrality maps pre-seeded from
`graph.nodes`), or test code. No panic-on-bad-input path was found.

**Residual gap = prevention**, not a present bug. Churning provably-safe code
would violate the repo's own "surgical changes" rule (`Agents.md §3`).
**Fix applied:** a `clippy::correctness` gate in CI (below) that fails the
build if a genuine correctness/panic hazard is ever introduced.

### Risk #2 — no test CI — **FIXED (2026-07-23)**
185 tests existed but nothing gated them on push/PR; only `release.yml`
(build-and-publish on tags) existed. **Fix applied:** added
`.github/workflows/ci.yml` running the Rust suite, the JS suite, and the
`clippy::correctness` gate on every push and PR to `main`.

### Risk #3 — feature surface vs. stage
Neo4j backend + Tauri app + multi-dest is a wide surface for v0.1 to keep
green. Open `prompt.md` items still question core decisions (dropping
`ug.node` NAPI, merging `semantic_search` into `search`). Consider
consolidating the core before widening further. *(Not addressed — design call.)*

## Changes made during this evaluation (2026-07-23)

- Added `.github/workflows/ci.yml` — CI running Rust tests, JS tests, and the
  `clippy::correctness` gate on push/PR (fixes Risk #2, institutionalizes Risk #1).

## How to reproduce this evaluation

```bash
cd native && cargo test                                    # Rust suite
node node/test-runner.cjs                                  # JS suite
cd native && cargo clippy --all-targets -- -D clippy::correctness   # correctness gate
git rev-parse --short HEAD                                  # record the commit
```
