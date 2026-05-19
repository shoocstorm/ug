# Project Progress Tracking

## UltraGraph-KB

### Current Phase: All core phases implemented ✅

### Latest milestone (2026-05-19): PDF indexing ✅
- New `indexer/pdf.rs` extracts text per page via the `pdf-extract` crate (pure Rust, no native deps).
- One `Symbol` per page, `kind: "heading_1"` → maps to `Concept` graph node with a `Contains` edge from the file (same shape as markdown headings, zero new graph-layer code).
- Page text lands in `docstring` (8 KB cap, char-boundary-safe truncation) so semantic search can rank PDF pages alongside code symbols.
- Empty / image-only pages emit a `Page N (no text)` stub. Garbage PDFs are silently skipped.
- Extension match is now case-insensitive — `.PDF` / `.Pdf` from scanners no longer get dropped.
- 11 integration tests + 3 unit tests cover happy path, mixed PDF/markdown directories, Unicode survival, graph round-trip, malformed input, and uppercase extensions.

### Milestone (2026-05-15): Multi-destination ingestion (Neo4j) ✅
- New `KnowledgeStore` trait abstracts the storage layer; OverGraph and Neo4j both implement it.
- `--dest overgraph,neo4j` fans ingest out to both backends in one pass.
- Neo4j 5.13+ supported via `neo4rs`; vector + full-text indexes auto-created.
- Personalized PageRank uses `gds.pageRank.stream` when GDS is detected; falls back to MMR otherwise.
- See `docs/MULTI-DEST.md` for the user-facing surface and `docs/MULTI-DEST-PLAN.md` for the design rationale.

---

## Phase 1: The Native "Turbo" Indexer (Rust) ✅ COMPLETED
- [x] Parallel Crawler: Use rayon to walk the directory, respecting .gitignore
- [x] Incremental Indexing: Implement a cache using blake3 hashes
- [x] AST Symbol Extraction: Extract Function, Class, Interface, Docstrings
- [x] NAPI-RS Bridge: Expose index() function returning JSON

**Implemented in:**
- `native/src/lib.rs` — index(), index_with_cache()
- `native/src/indexer.rs` — file walking, tree-sitter AST parsing, symbol extraction
- `native/Cargo.toml` — tree-sitter, blake3, ignore, rayon

---

## Phase 1.1: Additional Symbol Relationships ✅ COMPLETED
- [x] Import/Export Graph Edges
- [x] Inheritance Relationships (extends, implements)
- [x] Type Reference Relationships
- [x] Function Call Relationships
- [x] Package Dependencies (package.json)
- [x] File Classification

**Edge Types:** Contains, Imports, Exports, Extends, Implements, Calls, References, DependsOn

---

## Phase 1.2: Semantic Metadata ✅ COMPLETED
- [x] Docstring Extraction (JSDoc @param/@returns)
- [x] Function Signature Details (params, types, defaults, return types)
- [x] Code Metrics (LOC, param count, nesting depth)
- [x] Markdown heading section spans (`end_line` covers from heading through the line before the next same-or-shallower heading) — gives Semantic Enrichment the full body of each heading symbol

---

## Phase 1.3: Folder Hierarchy ✅ COMPLETED
- [x] FolderNode derivation from the scanned file set (no parsing — pure path math)
- [x] Synthetic `.` root anchors the forest; every folder carries `parent`, `depth`, immediate `childFiles` / `childFolders`, recursive `totalFiles`, recursive `languageBreakdown`
- [x] README detection (`README.md` / `_index.md` / `index.md` variants) populates `folder.readme`
- [x] Folder classification: path-name heuristics (`tests/`, `docs/`, `components/`, …) with content-driven fallback (all-markdown → Documentation, all-code → Source, else Mixed)
- [x] `folder.summary: Option<String>` reserved for the Semantic Enrichment phase to fill later
- [x] Cache-stable: folders are recomputed each run from `scan_files`, so the forest is correct in both `index()` and `index_with_cache()`

**Implemented in:**
- `native/src/indexer/folder.rs` — `extract_folders()`
- `native/src/indexer.rs` — wires the call into both index entry points
- `native/src/types.rs` — `FolderNode`, `FolderClassification`, `IndexResult.folders`, `IndexStats.totalFolders`

---

## Phase 2: Embedded Graph Persistence ✅ COMPLETED
- [x] Graph Schema: Nodes (File, Folder, Symbol, Concept) + Edges (all types above)
- [x] Folder forest in the graph: `Contains` edges parent_folder→child_folder and folder→immediate_file (only when the file resolved into a graph node)
- [x] In-Memory Querying: K-Hop BFS
- [x] Graph Analysis: centrality, cycle detection, shortest path, edge-type filtering
- [x] HTML Visualization Export: D3.js v7 force-directed graph

**Implemented in:**
- `native/src/graph.rs` — GraphNode, GraphEdge, GraphData, BfsResult, build_graph(), k_hop_bfs()
- `native/src/graph.rs` — filter_edges_by_type(), find_shortest_path(), calculate_centrality(), detect_cycles()
- `native/src/types.rs` — `GraphNodeType::Folder`, `GraphNodeFolderMeta` (depth, parent, classification, readme, totalFiles, languageBreakdown, summary)
- `src/vis/visualization.html` — Interactive D3.js visualization

**NAPI exports:**
- `buildGraph`, `kHopBfs`, `filterEdgesByType`, `findShortestPath`, `calculateCentrality`, `detectCycles`, `graphKeywordSearch`

---

## Phase 3: Semantic Storage & Enrichment ✅ COMPLETED
- [x] Vector Integration: Embed graph nodes into OverGraph via local OpenAI-compatible endpoint
- [x] Auto index creation (vector + FTS)
- [x] Folder-aware embedding text: pre-enrichment, folder nodes get a synthesized synopsis from classification + language breakdown + depth so they carry retrieval signal even before LLM summaries arrive; post-enrichment, `folder.summary` (or `docstring`) takes over
- [ ] Semantic Clustering: deferred
- [ ] Semantic Enrichment (LLM-written `summary` for folder + symbol nodes): deferred

**Implemented in:**
- `native/src/storage/text.rs` — `build_node_text`, `collect_related_names`, folder-synopsis fallback
- `native/src/storage/embed.rs` — `Embedder` HTTP client + `Embedder::ping`
- `native/src/storage/db.rs` — OverGraph schemas, upsert, vector_search, edges_from/to, fts_search, nodes_by_ids
- `native/src/storage/ingest.rs` — `ingest_graph`, `reembed_nodes`
- `native/src/storage/query.rs` — `semantic_search`, `semantic_search_w_where`, `rrf_search`, `mmr_rerank`, `traverse`, `traverse_filtered`, `read_snippet`, `search_kb`
- `native/src/storage/napi_bindings.rs` — NAPI surface for storage (async)
- `native/Cargo.toml` — arrow, tokio, futures, reqwest, lancedb, lance-index

**Build prerequisite:** `protoc` (`brew install protobuf` on macOS)

---

## Phase 4: The GraphRAG Retrieval Protocol ✅ COMPLETED

### Core deliverables (per REQUIREMENT.md)
- [x] **Hybrid Search Algorithm (PPR-first as of 2026-05-01):**
  1. Seed search via RRF (Reciprocal Rank Fusion of vector + FTS) — RRF scores feed the personalization vector instead of being used as fixed BFS roots
  2. **Personalized PageRank** over the edge graph (HippoRAG-style): edge-type-weighted random walk with restart, replaces both BFS expansion and MMR rerank with a single graph-aware ranking. Multi-seed by construction; central neighbors surface naturally
  3. Snippet extraction + token-budgeted context assembly
  4. Legacy MMR path retained behind `strategy: "mmr"` for callers who want diversity-first behavior
- [x] **MCP Server Implementation:**
  - `src/mcp-server.mjs` — stdio MCP server using `@modelcontextprotocol/sdk` + `zod`
  - Tools: `search_kb` (uses `dbHybridSearch`), `traverse_kb` (uses `dbTraverse`), `ping_embedder`
  - Configurable via env: `UG_DB_PATH`, `UG_REPO_ROOT`, `UG_EMBED_BASE_URL`, `UG_EMBED_API_KEY`, `UG_EMBED_MODEL`
  - Run via `node src/mcp-server.mjs`

### NAPI bindings (`native/src/storage/napi_bindings.rs`)
- `dbIngest(graphJson, dbPath, embedderOptions?) -> Promise<string>`
- `dbHybridSearch(dbPath, optionsJson, embedderOptions?) -> Promise<string>` — `optionsJson` accepts `strategy: "ppr"|"mmr"`, `pprRestartProb`, `pprMaxIter`, `pprSeedPool`, `pprEdgeWeights` (in addition to the existing `query`, `k`, `hops`, `edgeTypes`, `direction`, `maxChars`, `mmrLambda`, `whereClause`, `includeSnippets`)
- `dbSemanticSearch(dbPath, query, k, whereClause?, embedderOptions?) -> Promise<string>`
- `dbTraverse(dbPath, startNodeIds, hops, edgeTypes?, direction?) -> Promise<string>`
- `pingEmbedder(embedderOptions?) -> Promise<string>`

### JS CLI commands (`src/cli.cjs`)
- `index`, `graph`, `gen` — indexing + graph building + visualization
- `bfs`, `graph-search` — in-memory graph operations
- `db-ingest`, `db-semantic-search`, `db-traverse`, `db-rag`, `ping` — OverGraph + GraphRAG operations

### Tests
- Rust: **68 tests pass** across 7 suites after the OverGraph migration (`indexer_test`: 29, `graph_test`: 13, `search_test`: 13, `storage_test`: 7 (rewritten), plus `storage_bench`: 2 ignored, plus 4 unit tests in `text::sparse_tests` + `types_registry::tests`).
- JS: **21/21 tests pass** covering indexing, graph ops, ingest, semantic search, GraphRAG retrieval, edge-filtered traversal.

### Still open (Phase 4.1 — "nice-to-haves" from REQUIREMENT.md)
- [ ] Query by signature ("functions taking User and returning Promise")
- [ ] Query by pattern (try/catch detection from AST)

---

## Storage Migration: LanceDB → OverGraph (2026-05-01) ✅

End-to-end migration on branch `migrate/overgraph`. See `docs/MIGRATION-OVERGRAPH.md` for the full plan, run log, and open-question resolutions.

**Highlights:**
- Replaced LanceDB + manual PPR + manual RRF with a single OverGraph 0.6.0 dependency.
- Native PPR: `native/src/storage/ppr.rs` shrank from 445 → 116 LOC.
- Native hybrid search: `query::rrf_search` collapsed into one `vector_search(mode=Hybrid, fusion_mode=RRF)` call.
- New deterministic sparse keyword tokenizer (`text::build_sparse_keyword_vector`) replaces OverGraph's BM25 FTS for the keyword channel of hybrid search.
- Storage NAPI surface (`db_ingest`, `db_semantic_search`, `db_hybrid_search`, `db_traverse`, `ping_embedder`) — wire-compatible, no caller changes needed.
- Bench (dev profile, ARM64): ingest 1K nodes + 5K edges in 64.8ms; hybrid search p95 = 5.7ms.

**Outstanding:**
- Release-mode bin link error to resolve (LTO + napi cdylib + new dep tree).
- End-to-end CLI verification with a live embedding endpoint.
- Decision on whether to ship behind a `storage-overgraph` Cargo feature flag (OverGraph retained as fallback) or merge as the only backend.

## Last Updated: 2026-05-01
