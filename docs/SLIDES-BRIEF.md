# UltraGraph-KB — Slide Deck Brief

> **Purpose**: Feed this document to a slide-generation AI agent. It contains everything needed to produce a polished technical presentation about UltraGraph-KB: the elevator pitch, architecture, phase-by-phase deliverables, performance numbers, and concrete code/CLI examples.

---

## 1. Project Identity

- **Name**: UltraGraph-KB (CLI binary: `ug`)
- **One-liner**: A high-performance, **local-first** knowledge-base generator that turns codebases and documentation into a **queryable Semantic Knowledge Graph**.
- **Status**: All four core phases shipped; storage migrated to OverGraph (2026-05-01).
- **Target user**: AI coding agents (Claude, Javis Bot, …) that need *perfect context* from a repo — and the developers driving those agents.

### Core value proposition
1. **Seconds, not minutes** — saturates CPU cores during indexing.
2. **Zero-latency traversal** — embedded graph DB, no external server.
3. **Graph-aware retrieval** — Personalized PageRank, not naive vector search.
4. **Speaks MCP** — drop-in for any Model Context Protocol agent.

---

## 2. Hybrid Tech Stack (one-slide diagram)

```
┌───────────────────────────────────────────────────────────────┐
│                         UltraGraph-KB                          │
├───────────────────────────────────────────────────────────────┤
│  Interface (TypeScript)   ── Node 20+, pnpm, zod              │
│  ── CLI (src/cli.cjs)     ── MCP server (src/mcp-server.mjs)  │
├───────────────────────────────────────────────────────────────┤
│  NAPI-RS Bridge           ── native .node module              │
├───────────────────────────────────────────────────────────────┤
│  Core Engine (Rust)                                           │
│  ── ignore + rayon  (parallel crawler)                        │
│  ── tree-sitter      (TS / Python / Markdown AST)             │
│  ── blake3           (incremental hashing)                    │
│  ── axum + tower     (ug serve web API)                       │
├───────────────────────────────────────────────────────────────┤
│  Storage  ── OverGraph (embedded graph + vector + FTS)        │
│  Embedding ── local OpenAI-compatible endpoint                │
│              Qwen3-Embedding-0.6B-4bit-DWQ @ :8000            │
└───────────────────────────────────────────────────────────────┘
```

**Why hybrid?** Rust saturates cores for parsing/traversal; TypeScript owns the API + AI orchestration surface where flexibility matters more than raw speed.

---

## 3. The Four Phases (one slide each)

### Phase 1 — The Native "Turbo" Indexer ✅

*Goal: map a 1,000-file repo in seconds.*

- **Parallel crawler** — `rayon` over `ignore` (respects `.gitignore`).
- **Incremental indexing** — `blake3` content hash → only re-parse changed files.
- **AST extraction** — TypeScript / JavaScript, Python, Java, Rust, Markdown (CommonMark); **PDF** text via `pdf-extract` (one symbol per page).
- **NAPI bridge** — single `index(path)` → structured JSON of nodes + edges.

**Extended (1.1–1.4):**
- Import/Export, Extends/Implements, Calls, Type-references edges.
- JSDoc + Python docstrings, signatures (params/types/defaults), code metrics.
- File classification (test / entry-point / config / types / examples).
- Package dependencies from `package.json`.
- **Folder hierarchy** — derived from path math, with README detection, classification (Documentation / Source / Mixed), and language breakdown per folder.

### Phase 2 — Embedded Graph Persistence ✅

*Goal: zero-latency traversal, no external DB.*

- **Schema** — Nodes: `File · Folder · Symbol · Concept`. Edges: `Contains · Imports · Exports · Calls · Extends · Implements · References · DependsOn`.
- **K-Hop BFS** — in-memory, Rust-side.
- **Graph analytics** — edge-type filtering, shortest path, degree/betweenness centrality, cycle detection.
- **D3.js visualization** — force-directed (forceLink + forceManyBody + forceCenter + forceCollide + group-pull forceX/Y), drag/zoom/pan/hover, responsive SVG, vanilla JS — single embedded HTML file.

### Phase 3 — Semantic Storage & Enrichment ✅ (clustering deferred)

*Goal: add meaning to the structural map.*

- **Vector integration** — embed nodes with a local model into OverGraph.
- **Folder-aware embedding text** — pre-enrichment, folders carry a synthesized synopsis (classification + language breakdown + depth) so they have retrieval signal even before any LLM has written summaries.
- **Auto-created indexes** — vector + FTS.
- Deferred: LLM-written summaries + semantic clustering ("functional modules").

### Phase 4 — The GraphRAG Retrieval Protocol ✅

*Goal: provide the **Perfect Context** to the AI agent.*

The headline upgrade. The legacy spec was *vector top-1 → BFS expansion → MMR rerank*. We replaced it with **Personalized PageRank seeded by RRF**, HippoRAG-style.

```
            ┌─────────────────────────────────┐
            │ RRF seed pool (vector + FTS)    │   ← weighted candidates,
            │   top-16 hits → personalization │     NOT fixed BFS roots
            └────────────┬────────────────────┘
                         ▼
            ┌─────────────────────────────────┐
            │ Personalized PageRank           │
            │   random walk + restart α=0.15  │
            │   edge-type weights:            │
            │     Calls 1.0  Extends 0.9 …    │
            │     Contains 0.3                │
            └────────────┬────────────────────┘
                         ▼
            ┌─────────────────────────────────┐
            │ Token-budgeted assembly         │
            │   top-K by PPR · attach snippets│
            │   apply char budget             │
            └─────────────────────────────────┘

(Legacy MMR retained behind `strategy: "mmr"` for diversity-first callers.)
```

**Why we moved off single-seed + BFS** — three failure modes:
1. Stage-1 errors compound: a wrong top-1 seed taints the whole neighborhood.
2. Many queries ("how does auth work") have answers across 5+ nodes; there is no one seed.
3. MMR optimizes diversity, not relevance — the rerank step is a diversity heuristic.

**MCP server** — `src/mcp-server.mjs` exposes `search_kb`, `traverse_kb`, `ping_embedder` over stdio. Drop into any MCP agent (Claude, etc.).

---

## 4. New Relationship Cheat-Sheet (one slide table)

| Edge | Source → Target | What it surfaces |
|---|---|---|
| **CONTAINS** | Folder/File → Folder/File/Symbol | Structural hierarchy (folder forest + file→symbol + markdown heading nesting) |
| **IMPORTS** | File/Symbol → File/Symbol | Cross-file dependencies |
| **EXPORTS** | Symbol → File | Module exports |
| **CALLS** | Function → Function | Call graph |
| **EXTENDS** | Class → Class | Inheritance |
| **IMPLEMENTS** | Class → Interface | Interface impl |
| **TYPED_AS** | Symbol → Type/Symbol | Type relationships |
| **CONFIGURED_BY** | Symbol → Config | Config links |
| **DEPENDS_ON** | File → Package | NPM / external deps |

---

## 5. PPR Tuning Knobs (one slide)

Exposed through MCP `search_kb`, NAPI `dbHybridSearch`, and CLI `db-rag`.

| Parameter | Default | Effect |
|---|---|---|
| `pprRestartProb` (α) | 0.15 | Teleport probability. Higher → stay near seeds; lower → centrality dominates. |
| `pprMaxIter` | 100 | Power-iteration cap (L1 convergence, tol = 1e-4). |
| `pprSeedPool` | 16 | RRF hits feeding the personalization vector. Larger → more robust to a noisy top hit. |
| `pprEdgeWeights` | per-type | Override edge-type weights (case-insensitive). |

**Default edge weights**:
`calls=1.0 · extends=0.9 · implements=0.9 · imports=0.7 · requires=0.7 · exports=0.6 · uses=0.6 · references=0.5 · dependson=0.4 · contains=0.3`

**Scaling note**: PPR loads the full edges table per query (single-digit ms via sparse iteration at ≤100K edges). Past ~1M edges, switch to subgraph restriction (k-hop frontier from seeds) or precomputed PPR vectors.

---

## 6. `ug serve` — the embedded web API (one slide)

Single Axum process. Self-contained: `visualization.html` and `ug-vis.bundle.js` (three.js + 3d-force-graph) are `include_str!`/`include_bytes!`'d into the binary. `graph.json` is pre-loaded; bytes are pre-compressed (brotli-9 + gzip-9) once at startup. Default bind `127.0.0.1`.

| Phase | Routes |
|---|---|
| **1** static | `GET /`, `/index.html`, `/ug-vis.bundle.js`, `/graph.json`, `/healthz` |
| **2** read-only graph API | `/api/graph/stats · /node/*id · /search · /bfs · /path · /filter · /centrality · /cycles` |
| **3** DB-backed | `GET /api/db/node/*id · /api/db/traverse/*id`<br>`POST /api/search/semantic · /api/search/hybrid` |

**Compression payoff** — 31.6 MB graph, 41 614 nodes / 95 117 edges:

| encoding | bytes | ratio |
|---|---|---|
| identity | 31 625 670 | 1× |
| gzip-9 | 2 777 891 | ~11× |
| brotli-9 | 2 272 127 | ~14× |

Chain it from the gen pipeline:
```bash
ug gen -i ./src --serve --watch   # index → graph → viz → ingest → live server
```

---

## 7. Visualization (one slide)

Vanilla JS / HTML5 / SVG, **D3.js v7**, no build pipeline.

- **Physics** — `forceSimulation` with `forceLink`, `forceManyBody`, `forceCenter`, `forceCollide`, and subtle `forceX`/`forceY` pulling same-`group` nodes to common centers.
- **Encoding** — nodes = circles colored by `group` (`d3.schemeCategory10`); edges = lines; labels = node `id`.
- **Interactivity** — drag (with alpha update), `d3.zoom` for pan/zoom, hover highlights neighbors.
- **Server-aware UX** — feature-detects `ug serve` via `/api/capabilities`:
  - Live "Connected / Stale" badge from `/healthz`.
  - Node-detail enrichment from `/api/db/node/*id` (description, signature, metrics, snippet).
  - Semantic search panel (`POST /api/search/semantic`).
  - Hybrid search with collapsible snippet cards (`POST /api/search/hybrid`).
- **Graceful fallthrough** — every server-dependent feature falls back to client-only on 503 / network error.

---

## 8. Performance — the headline numbers (one slide)

On **MacBook Pro M5 Max · 18-core · 40-GPU · 128 GB**, indexing `~/.hermes/hermes-agent`:

| metric | before (LanceDB) | after (OverGraph, 2026-05-01) |
|---|---|---|
| Total wall time (full pipeline + embeddings) | ~5 min | **~1–2 min** |
| Indexing | — | 9.27 s |
| Graph build | — | 264 ms |
| Embeddings (41 619 nodes) | — | 618 s |
| Node write | — | 993 ms |
| Edge write (95 071) | — | 215 ms |
| Native binary | 300 MB | **10 MB** |
| DB on disk | 300 MB | **200 MB** |

**Spec targets** (REQUIREMENT.md):
- Ingestion < 5 s for 1 000-file repo.
- Query latency < 100 ms for a 3-hop traversal.
- Memory footprint < 500 MB during active indexing.

**OverGraph migration scoreboard**:
- `ppr.rs` 445 → **116 LOC** (single dependency replaces hand-rolled PPR + RRF).
- Ingest 1 K nodes + 5 K edges in **64.8 ms** (dev profile, ARM64).
- Hybrid search **p95 = 5.7 ms**.

---

## 9. CLI / API Surface (one slide)

**JS CLI** (`src/cli.cjs`):
- Indexing & graph: `index · graph · gen · bfs · graph-search`
- Storage & RAG: `db-ingest · db-semantic-search · db-traverse · db-rag · ping`
- Web: `serve`

**NAPI bindings** (`native/src/storage/napi_bindings.rs`):
- `dbIngest`, `dbHybridSearch`, `dbSemanticSearch`, `dbTraverse`, `pingEmbedder`
- Graph: `buildGraph`, `kHopBfs`, `filterEdgesByType`, `findShortestPath`, `calculateCentrality`, `detectCycles`, `graphKeywordSearch`

**MCP tools** (`src/mcp-server.mjs`):
- `search_kb` — hybrid PPR/MMR with snippets.
- `traverse_kb` — edge-typed BFS over the DB.
- `ping_embedder` — health-check the local embedding endpoint.

---

## 10. Suggested Slide Order (for the agent)

1. **Title** — UltraGraph-KB · *Perfect context, in seconds.*
2. **The problem** — naive RAG fails on codebases: a wrong top-1 vector hit poisons the whole answer.
3. **The architecture** — Rust engine + TS interface + embedded OverGraph (use the §2 diagram).
4. **Phase 1** — turbo indexer (numbers: parallel crawl, blake3 cache, AST → JSON).
5. **Phase 1 extended** — richer edges + docstrings + folder hierarchy (use §4 table).
6. **Phase 2** — embedded graph + D3 visualization (use §7).
7. **Phase 3** — semantic storage + folder-aware embedding text.
8. **Phase 4 — the headline** — why single-seed BFS fails (§3 *three failure modes*) → RRF + PPR + budgeted assembly (use the §3 ASCII diagram).
9. **PPR knobs** — §5 table.
10. **`ug serve`** — single binary, embedded assets, pre-compressed bytes (§6 table).
11. **Performance** — §8 before/after table.
12. **MCP integration** — drop-in for any MCP agent; one tool call away from "perfect context".
13. **Roadmap** — query-by-signature, query-by-pattern, semantic clustering / LLM summaries, scaling beyond 1 M edges.
14. **Close** — `ug gen -i ./src --serve` → open `http://localhost:8080`.

---

## 11. Tone & Visual Hints for the Slide Generator

- **Audience**: technical (engineers, AI-tooling builders). It's OK to show one line of Rust or one curl example per slide; no need to dumb down.
- **Voice**: matter-of-fact, numbers-forward, no marketing fluff. The product is fast and the slides should feel fast — short bullets, big numbers, monospace for identifiers.
- **Color**: dark theme; accent on graph edges (think D3 categorical palette). Use the **edge-type weight ladder** (Calls 1.0 → Contains 0.3) as a recurring visual motif — it's the soul of the retrieval story.
- **Diagrams to include**: the layered stack (§2), the RRF → PPR → assembly pipeline (§3 Phase 4), and a small node-graph thumbnail per slide footer for continuity.
- **Avoid**: stock "AI brain" imagery, gradients on text, three-column "Why us?" slides.

---

## 12. Concrete Examples to Quote Verbatim

**Full pipeline + serve**:
```bash
ug gen -i ~/.hermes/hermes-agent -o .ug/ugdb --serve
```

**Hybrid search via curl**:
```bash
curl -X POST -H "Content-Type: application/json" \
  http://localhost:8080/api/search/hybrid \
  -d '{"query":"oauth login flow","k":8,"strategy":"ppr"}'
```

**MCP tool call (conceptual)**:
```json
{ "name": "search_kb",
  "arguments": { "query": "how does auth work", "k": 8, "strategy": "ppr" } }
```

**Local embedding endpoint** (mentioned on a config slide):
```
Model:    openai/Qwen3-Embedding-0.6B-4bit-DWQ
Base URL: http://localhost:8000/v1
API Key:  1234
```

---

*Source documents distilled into this brief: `docs/REQUIREMENT.md`, `docs/PROGRESS.md`, `docs/VISUALIZATION.md`, `docs/WEB-SERVE.md`. Last refreshed 2026-05-13.*
