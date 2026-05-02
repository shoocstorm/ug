This requirements file is optimized for a **High-Performance Hybrid Architecture**. It leverages **Rust** for the heavy-duty parsing and graph traversal (the "Engine") and **TypeScript** for the developer-facing API and AI orchestration (the "Interface").
# Technical Requirements: Project "UltraGraph-KB"
**Objective:** A high-performance, local-first knowledge base generator that transforms codebases and documentation into a queryable Semantic Knowledge Graph.
## 🏗️ The Hybrid Tech Stack
 * **Core Engine (Rust):** File walking (ignore crate), AST Parsing (tree-sitter), and Incremental Hashing (blake3).
 * **Bridge (NAPI-RS):** Compiles Rust logic into a native .node module for TypeScript.
 * **Storage (Embeddable):** * **Graph:** Oxigraph (SPARQL/RDF support) or SurrealDB (embedded mode).
   * **Vector:** LanceDB (Native NodeJS/Rust integration).
 * **Interface (TypeScript):** Node.js 20+, pnpm, zod for schema validation.
## 📋 Implementation Phases
### Phase 1: The Native "Turbo" Indexer (Rust)
*Goal: Saturate CPU cores to map the codebase in seconds, not minutes.*
 * [ ] **Parallel Crawler:** Use rayon to walk the directory, respecting .gitignore.
 * [ ] **Incremental Indexing:** Implement a cache using blake3 hashes. Only re-parse files where the hash has changed.
 * [ ] **AST Symbol Extraction:** * Extract: Function signatures, class hierarchies, imports/exports, and docstrings.
   * Supported Parsers: TypeScript, Python, and Markdown (CommonMark).
 * [ ] **NAPI-RS Bridge:** Expose a single index(path: string) function to TypeScript that returns a structured JSON of nodes and edges.
### Phase 2: Embedded Graph Persistence
*Goal: Zero-latency traversal without an external database server.*

* [ ] **Graph Schema:**
  * **Nodes:** File, Symbol (Function/Class), Concept (extracted from Docs).
  * **Edges:** DEPENDS_ON, CALLS, EXTENDS, REFERENCES.
* [ ] **In-Memory Querying:** Implement a Rust-side function to perform **K-Hop Breadth-First Search (BFS)** to find related context for a given symbol.
* [ ] **HTML Visualization Export:**
  * [ ] **Data Export:** Expose a function to serialize the graph as JSON with `nodes` (id, group) and `edges` (source, target).
  * [ ] **D3.js Integration:** Create a vanilla JS/HTML5 visualization module using D3.js v7.
  * [ ] **Force-Directed Rendering:**
    * Implement `d3.forceSimulation` with `forceLink`, `forceManyBody`, and `forceCenter`.
    * Add `forceCollide` to prevent node overlap and `forceX`/`forceY` for group clustering.
  * [ ] **Visual Encoding:**
    * Nodes as circles with fill color mapped to `group` via categorical scale.
    * Edges as lines connecting nodes.
    * Text labels showing node `id`.
  * [ ] **Interactivity:**
    * Drag & drop nodes with simulation alpha updates.
    * Zoom/pan via `d3.zoom`.
    * Hover effects highlighting node and neighbors.
  * [ ] **Responsive Design:** SVG adapts to window/container dimensions.
  * Refer to [Visualization](VISUALIZATION.md) for visualization details.


### Phase 3: Semantic Storage & Enrichment
*Goal: Add "meaning" to the structural map using local LLMs.*
 * [ ] **Vector Integration:** Generate embeddings for extracted graph nodes and edges and store them in LanceDB using a local embedding model.
```Local embedding model settings:
Model: openai/Qwen3-Embedding-0.6B-4bit-DWQ
Base URL: http://localhost:8000/v1
API Key: 1234
```
Refer to [Graph Storage](GRAPH-STORAGE.md) for graph storage details.

 * [ ] **Semantic Clustering:** Group related symbols into "Functional Modules"
   * Use qwen to generate high-level summaries for these clusters (e.g., "This module handles OAuth2 authentication").
```Local clustering model settings:
Model: openai/Qwen3.6-35B-A3B-MLX-8bit
Base URL: http://localhost:8000/v1
API Key: 1234
```

### Phase 4: The GraphRAG Retrieval Protocol
*Goal: Provide the "Perfect Context" to the AI Agent.*
 * [ ] **Hybrid Search Algorithm (PPR-based, HippoRAG-style):**
   1. **RRF Seed Search:** Reciprocal Rank Fusion of vector + FTS produces a *seed pool* (default 16 hits) — these are weighted candidates, **not** fixed entry points. RRF scores form the personalization mass.
   2. **Personalized PageRank over the edge graph:** edge-type-weighted random walk with restart (default α = 0.15). PPR scores combine seed proximity with structural centrality in a single graph-aware ranking — replaces the older "single seed → BFS expansion → MMR rerank" cascade. Edge-type weights default to Calls=1.0, Extends/Implements=0.9, Imports=0.7, Exports=0.6, References=0.5, DependsOn=0.4, Contains=0.3 and are caller-overridable.
   3. **Token-Budgeted Assembly:** hydrate top-K nodes by PPR score, attach code snippets, apply a character budget.
   4. *Legacy fallback:* MMR-based path retained behind `strategy: "mmr"` for callers that want diversity-first behavior.
 * [ ] **MCP Server Implementation:** Wrap the search logic in a **Model Context Protocol (MCP)** server so any AI agent (Claude, Javis Bot, etc.) can call search_kb(query).
## ⚡ Performance Targets
 * **Ingestion:** < 5 seconds for a 1,000-file repository.
 * **Query Latency:** < 100ms for a 3-hop graph traversal.
 * **Memory Footprint:** < 500MB during active indexing.
## 🛠️ Usage for Coding Agent
 1. **Initialize:** "Create a new monorepo with a /native folder for Rust and a /lib folder for TypeScript."
 2. **Step 1:** "Implement the NAPI-RS boilerplate in /native and a simple file-walking function."
 3. **Step 2:** "Integrate Tree-sitter in Rust to extract TypeScript interfaces."
 4. **Step 3:** "Build the TypeScript wrapper to store these symbols in LanceDB."
> **Note:** Since you are on **macOS**, ensure the Rust target is configured for aarch64-apple-darwin to take full advantage of your Mac's M-series performance.

## 🚀 Enhanced Extraction Plan (v2)

### Phase 1: The Native "Turbo" Indexer (Rust) - Extended

*Goal: Saturate CPU cores to map the codebase in seconds, not minutes.*
*Goal: Extract MORE relationships for richer graph connectivity.*

#### Current Implementation (keep)
* [x] **Parallel Crawler:** Use rayon to walk the directory, respecting .gitignore.
* [x] **Incremental Indexing:** Implement a cache using blake3 hashes. Only re-parse files where the hash has changed.
* [x] **AST Symbol Extraction:** * Extract: Function signatures, class hierarchies, imports/exports, and docstrings.
  * Supported Parsers: TypeScript, Python, and Markdown (CommonMark).
* [x] **NAPI-RS Bridge:** Expose a single index(path: string) function to TypeScript that returns a structured JSON of nodes and edges.

#### New: Additional Symbol Relationships (Phase 1.1)

* **[NEW] Import/Export Graph Edges** (High Impact)
  * Extract: `import ... from`, `export ... from`, `require()`, `export default`
  * Creates: IMPORTS, EXPORTS, REQUIRES edges between FILEs and SYMBOLs
  * Enables: Cross-file dependency analysis, dead code detection

* **[NEW] Inheritance Relationships** (High Impact)
  * Extract: `extends`, `implements`, `super()` calls
  * Creates: EXTENDS, IMPLEMENTS edges between CLASS nodes

* **[NEW] Type Reference Relationships** (Medium Impact)
  * Extract: Parameter types, return types, property types
  * Creates: TYPED_AS edges from Symbol → Type/Symbol
  * Enables: Type dependency analysis

* **[NEW] Function Call Relationships** (Medium Impact)
  * Extract: Nested function calls within function bodies
  * Creates: CALLS edges between Function nodes (within same file)
  * Enables: Call graph analysis

#### New: Semantic Metadata (Phase 1.2)

* **[NEW] Docstring Extraction** (High Value)
  * TypeScript: Extract JSDoc comments (`/** ... */`)
  * Python: Extract triple-quoted docstrings (`"""...""")
  * Store as: `Symbol.docstring` field
  * Enables: Semantic search, AI context generation

* **[NEW] Function Signature Details** (Medium Value)
  * Extract: Parameter names, types, default values, optional markers
  * Extract: Return type annotations
  * Store as: `Symbol.signature` JSON field
  * Enables: API signature search, parameter-based queries

* **[NEW] Code Metrics** (Low Value, High Insight)
  * Function LOC (lines of code)
  * Cyclomatic complexity proxy (branch count)
  * Parameter count
  * Nesting depth
  * Store as: `Symbol.metrics` field
  * Enables: Technical debt identification

#### New: File-Level Intelligence (Phase 1.3)

* **[NEW] File Classification** (High Value)
  * Detect: Test files (`*.test.ts`, `*.spec.ts`, `*_test.py`, `test_*.py`)
  * Detect: Entry points (`index.ts`, `main.ts`, `app.ts`, `server.ts`)
  * Detect: Config files (`.env`, `config.ts`, `settings.py`)
  * Detect: Types/Definitions (`.d.ts`, `types.py`, `interfaces.ts`)
  * Detect: Example/Demo files (`example/`, `demo/`, `samples/`)
  * Creates: FILE_TYPE nodes or metadata

* **[NEW] Package Dependencies** (Medium Value)
  * Extract: `package.json` dependencies, devDependencies
  * Creates: DEPENDS_ON edges to package nodes

* **[NEW] Configuration Relationships** (Medium Value)
  * Link: Config files to files that import them
  * Link: Environment variables to where they're used
  * Enables: Impact analysis for config changes

#### New: Folder Hierarchy (Phase 1.4) ✅

*Goal: Surface the structural narrative the directory tree itself tells — `src/components` vs `tests/components`, a `docs/2026/january/` knowledge-base layout, the `lib/` vs `app/` split.*

* [x] **Folder-Node Derivation** (High Value)
  * Derived from the scanned file set without re-parsing — pure path math
  * Synthetic `.` root anchors the forest; folders carry `parent`, `depth`, immediate `childFiles` / `childFolders`, recursive `totalFiles`, recursive `languageBreakdown`
  * README detection (`README.md` / `_index.md` / `index.md`) populates `folder.readme`
  * Folder classification: path-name heuristics (`tests/`, `docs/`, `components/`, …) with a content-driven fallback (all-markdown → Documentation, all-code → Source, else Mixed)
  * `folder.summary: Option<String>` reserved for the Semantic Enrichment phase
  * Cache-stable: recomputed each run from `scan_files`, so the forest stays correct under `index_with_cache`
  * Enables: hierarchy visualization, folder-scoped retrieval ("summarize what `src/auth/` contains"), and a coarser RAG context level above per-file/per-symbol nodes

### Phase 2: Enhanced Graph Schema

*Goal: Zero-latency traversal without an external database server.*

#### Current Implementation (keep)
* [x] **Graph Schema:**
  * **Nodes:** File, **Folder**, Symbol (Function/Class), Concept (extracted from Docs).
  * **Edges:** DEPENDS_ON, CALLS, EXTENDS, REFERENCES, **CONTAINS** (also wires the folder forest: parent_folder → child_folder, folder → immediate file).
* [x] **In-Memory Querying:** Implement a Rust-side function to perform **K-Hop Breadth-First Search (BFS)** to find related context for a given symbol.
* [x] **HTML Visualization Export:**
  * [x] **Data Export:** Expose a function to serialize the graph as JSON with `nodes` (id, group) and `edges` (source, target).
  * [x] **D3.js Integration:** Create a vanilla JS/HTML5 visualization module using D3.js v7.
  * [x] **Force-Directed Rendering:**
  * [x] **Visual Encoding:**
  * [x] **Interactivity:**
  * [x] **Responsive Design:** SVG adapts to window/container dimensions.

#### New: Enhanced Query Operations (Phase 2.1)

* **[NEW] Edge Type Filtering**
  * Allow queries to filter by edge type (e.g., "only IMPORTS edges")
  * Enable: Specific relationship analysis

* **[NEW] Path Finding**
  * Implement shortest path between two symbols
  * Enable: "How did function A reach function B?"

* **[NEW] Centrality Analysis**
  * Calculate degree centrality (most connected nodes)
  * Calculate betweenness centrality (bridge nodes)
  * Enable: Identify hub functions, critical dependencies

* **[NEW] Cycle Detection**
  * Detect circular dependencies in import graph
  * Enable: Identify potential design issues

### Phase 3: Semantic Enrichment (TypeScript) - Extended

*Goal: Add "meaning" to the structural map using local LLMs.*
* [x] **Vector Integration:** Embed extracted docstrings and code comments into LanceDB using a local model (e.g., all-MiniLM-L6-v2).
* [x] **Semantic Clustering:** * Group related symbols into "Functional Modules."
  * Use Ollama to generate high-level summaries for these clusters (e.g., "This module handles OAuth2 authentication").

#### New: Enhanced Semantic Features (Phase 3.1)

* **[NEW] Intent Classification**
  * Classify symbols by intent: API, UTILITY, DATA, CONFIG, TEST
  * Based on naming patterns + code structure
  * Enable: Faster context understanding

* **[NEW] Change Impact Prediction**
  * Given a symbol, predict which other symbols would be affected by changes
  * Based on call graph + import graph
  * Enable: Safer refactoring, better PR reviews

* **[NEW] Natural Language Summaries**
  * Use LLM to generate human-readable summaries of code modules
  * "This module provides user authentication via OAuth2"
  * Enable: Faster onboarding, documentation generation

### Phase 4: The GraphRAG Retrieval Protocol - Extended

*Goal: Provide the "Perfect Context" to the AI Agent.*

#### Why we moved off "single seed node → BFS"

The original spec (above) framed retrieval as **find one seed node, then BFS 2–3 hops, then rerank**. In practice that cascade has three failure modes:

1. **Stage-1 errors compound.** A wrong top-1 seed pulls expansion into the wrong neighborhood; MMR can't recover relevance from a tainted candidate set.
2. **Single entry-point assumption.** Many real queries ("how does auth work") have answers distributed across 5+ nodes — there is no one seed.
3. **MMR optimizes diversity, not relevance.** The "rerank" step is a diversity heuristic, not a true relevance scorer.

The replacement is **Personalized PageRank seeded by RRF** (HippoRAG-style):

* [x] **PPR-Based Hybrid Search Algorithm** *(implemented 2026-05-01, replaces seed+BFS+MMR by default)*:
  1. **RRF Seed Pool:** Vector + FTS via Reciprocal Rank Fusion produces a *weighted* seed set (default top-16). RRF scores feed the PPR personalization vector — no single-point-of-failure top-1 seed.
  2. **Personalized PageRank:** Random walk with restart over the edge graph. Edge types are *weighted* (Calls > Imports > Contains, etc.) rather than gated, so a strong Calls edge can outweigh a chain of weak Contains edges. Direction (`outbound` / `inbound` / `both`) and edge-type whitelists still supported as filters.
  3. **Token-Budgeted Context Assembly:** Hydrate top-K nodes by PPR score, attach code snippets, apply char budget.
  4. **Multi-seed by construction.** Disconnected components anchored by separate seeds each get their own ranked neighborhood.
  5. **Legacy MMR path retained** behind `strategy: "mmr"` for diversity-first callers.
* [x] **MCP Server Implementation:** Wrap the search logic in a **Model Context Protocol (MCP)** server so any AI agent (Claude, Javis Bot, etc.) can call search_kb(query).

**Tunable PPR parameters** (exposed through the MCP `search_kb` tool, NAPI `dbHybridSearch`, and CLI `db-rag`):

| Parameter | Default | Effect |
|-----------|---------|--------|
| `pprRestartProb` | 0.15 | Teleport probability. Higher = stay closer to seeds; lower = let centrality dominate. |
| `pprMaxIter` | 100 | Power-iteration cap. Convergence is L1 within `tol`=1e-4. |
| `pprSeedPool` | 16 | RRF hits feeding the personalization vector. Larger = more robust to a noisy top hit. |
| `pprEdgeWeights` | see below | Per-edge-type weight overrides (case-insensitive). |

Default edge-type weights: `calls=1.0, extends=0.9, implements=0.9, imports=0.7, requires=0.7, exports=0.6, uses=0.6, references=0.5, dependson=0.4, contains=0.3`.

**Scaling note:** PPR loads the full edges table per query (single-digit ms via sparse iteration at ≤100K edges). Past ~1M edges, a future enhancement would be subgraph restriction (k-hop frontier from seeds) or precomputed PPR vectors per node.

#### New: Enhanced Retrieval Features (Phase 4.1)

* **[NEW] Edge-Type-Aware Walk (implemented as PPR weights + whitelist)**
  * Walk along specific edge types only, *or* let all edges participate with type-specific weights.
  * Enable: More targeted context without losing centrality signal.

* **[NEW] Query by Signature**
  * "Find functions that take User as parameter and return Promise"
  * Enable: API discovery

* **[NEW] Query by Pattern**
  * "Find all error handling patterns (try/catch) in this module"
  * Enable: Code review assistance

---

## 📊 New Relationship Summary

| Edge Type | Source | Target | Purpose |
|-----------|--------|--------|---------|
| CONTAINS | Folder/File | Folder/File/Symbol | Structural hierarchy (folder forest + file→symbol nesting + markdown heading nesting) |
| IMPORTS | File/Symbol | File/Symbol | Cross-file dependencies |
| EXPORTS | Symbol | File | Module exports |
| CALLS | Function | Function | Call graph |
| EXTENDS | Class | Class | Inheritance |
| IMPLEMENTS | Class | Interface | Interface implementation |
| TYPED_AS | Symbol | Type/Symbol | Type relationships |
| CONFIGURED_BY | Symbol | Config | Configuration links |
| DEPENDS_ON | File | Package | NPM/External deps |

## 📋 Implementation Priority

**P0 - Must Have (Highest ROI):**
1. Import/Export extraction (cross-file relationships)
2. Docstring extraction (for semantic search)
3. Inheritance relationships (class hierarchies)

**P1 - Should Have:**
4. Type references (parameter/return types)
5. Function calls (call graph)
6. File classification (test/entry point detection)

**P2 - Nice to Have:**
7. Package dependencies
8. Code metrics
9. Configuration relationships
10. Path finding algorithms
> 

### Perf
With LanceDB, it took ~5min to import (with embeddings on MacBook Pro M5 Max 18-core 40-GPU 128GB):
▸ Ingesting into ug-out/ugdb
  ✓ 41619 nodes, 95071 edges embedded
index-tree.json - 20MB
graph.json - 40MB

With OverGraph, it took ~2min to import (with embeddings on MacBook Pro M5 Max 18-core 40-GPU 128GB):
- build time reduced from 5min to 1~2min.
- binary size reduced from 300MB to 10MB.
- db file size reduced from 300MB to 200MB.

```
ug-out/ug gen -i ~/.hermes/hermes-agent -o ug-out/ugdb 
⚡ Full pipeline: index → graph → visualization → ingest
▸ Indexing /Users/aldrickwan/.hermes/hermes-agent
  ✓ done in 9.267701s
▸ Building graph
  ✓ done in 264.158041ms
  nodes: 41619
  edges: 95071
▸ Copying visualization assets
  ✓ done in 229.041µs
────────────────────────────────────────
✓ Generated in ug-out/ugdb/
  ✓ graph.json
  ✓ indexed-tree.json
  ✓ index.html (open in browser with HTTP server)
  ✓ README.md

▸ Ingesting graph data into DB ug-out/ugdb
▸ Building node texts: 100.0% ✓ done in 46.680625ms
▸ Embedding: 100.0% (41619/41619) ✓ done in 618.098150209s
▸ Writing nodes: 100.0% (41619/41619) ✓ done in 993.175791ms
▸ Writing edges: 100.0% (95071/95071) ✓ done in 215.248042ms
  ✓ 41619 nodes, 95071 edges embedded in 625.374811959s
────────────────────────────────────────
Visit http://localhost:8080 to view the graph
Run 'ug semantic_search "hello" -o ug-out/ugdb' to perform a RAG query.
Total time: 635.004456709s
```

<br>

Refer to [IMPLEMENTATION PROGRESS](PROGRESS.md) for implementation progress.