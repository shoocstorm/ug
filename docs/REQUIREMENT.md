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
### Phase 3: Semantic Enrichment (TypeScript)
*Goal: Add "meaning" to the structural map using local LLMs.*
 * [ ] **Vector Integration:** Embed extracted docstrings and code comments into LanceDB using a local model (e.g., all-MiniLM-L6-v2).
 * [ ] **Semantic Clustering:** * Group related symbols into "Functional Modules."
   * Use Ollama to generate high-level summaries for these clusters (e.g., "This module handles OAuth2 authentication").
### Phase 4: The GraphRAG Retrieval Protocol
*Goal: Provide the "Perfect Context" to the AI Agent.*
 * [ ] **Hybrid Search Algorithm:**
   1. **Keyword/Vector Search:** Locate the "Seed Node" in the KB.
   2. **Graph Expansion:** Walk the graph 2-3 hops from the Seed Node to pull in relevant dependencies and documentation.
   3. **Context Ranking:** Use a re-ranker to ensure the most relevant code snippets appear first in the prompt.
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

### Phase 2: Enhanced Graph Schema

*Goal: Zero-latency traversal without an external database server.*

#### Current Implementation (keep)
* [x] **Graph Schema:**
  * **Nodes:** File, Symbol (Function/Class), Concept (extracted from Docs).
  * **Edges:** DEPENDS_ON, CALLS, EXTENDS, REFERENCES.
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
* [x] **Hybrid Search Algorithm:**
  1. **Keyword/Vector Search:** Locate the "Seed Node" in the KB.
  2. **Graph Expansion:** Walk the graph 2-3 hops from the Seed Node to pull in relevant dependencies and documentation.
  3. **Context Ranking:** Use a re-ranker to ensure the most relevant code snippets appear first in the prompt.
* [x] **MCP Server Implementation:** Wrap the search logic in a **Model Context Protocol (MCP)** server so any AI agent (Claude, Javis Bot, etc.) can call search_kb(query).

#### New: Enhanced Retrieval Features (Phase 4.1)

* **[NEW] Edge-Type-Aware Expansion**
  * Only expand along specific edge types (e.g., only CALLED_BY edges)
  * Enable: More targeted context

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
