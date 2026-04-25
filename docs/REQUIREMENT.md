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
> 
