# UltraGraph: High-Performance Knowledge Graph & RAG Engine

A high-performance, local-first knowledge base engine that transforms codebases and documents into an interactive, visualized, and queryable **Semantic Knowledge Graph**. Built with Rust and Node.js for maximum speed and flexibility.

## ⚡ Overview

- **UltraGraph Introduction**: [https://ultra-graph.web.app](https://ultra-graph.web.app)

UltraGraph implements a complete four-phase pipeline for building and querying advanced knowledge bases:

- **Phase 1: Turbo Indexing** — Native multi-threaded indexer that maps codebases in milliseconds using `tree-sitter`.
- **Phase 2: Graph Synthesis** — Builds a rich symbol graph with structural analysis (centrality, cycle detection, shortest paths).
- **Phase 3: OverGraph Storage** — Persistent vector + FTS storage with incremental ingestion and local embedding support.
- **Phase 4: GraphRAG Search** — State-of-the-art retrieval using **Personalized PageRank (PPR)** to combine semantic relevance with structural importance.

---

## 🏗️ Architecture

```
                    ┌──────────────┐
                    │   Source     │
                    │   Codebase   │
                    │  (Dir Path)  │
                    └──────┬───────┘
                           │
                           ▼
              ┌─────────────────────────┐
              │   Phase 1: Indexing     │
              │  ────────────────────   │
              │  • File discovery       │
              │    (.gitignore aware)   │
              │  • tree-sitter AST      │
              │  • Symbol extraction    │
              │  • Incremental (blake3) │
              │  • Languages: TS/JS/    │
              │    Py/Java/MD           │
              └──────────┬──────────────┘
                         │
                         │ IndexResult (JSON)
                         │
                         ▼
              ┌─────────────────────────┐
              │   Phase 2: Graphing     │
              │  ────────────────────   │
              │  Nodes: Folder/File/    │
              │    Function/Class/      │
              │    Interface/Concept/   │
              │    Dependency           │
              │  Edges: Contains        │
              │    (folder→folder→file  │
              │    →symbol)/Imports/    │
              │    Calls/Extends/       │
              │    References           │
              │  Algos: BFS/Cycle/      │
              │    Centrality/Paths     │
              └────┬────────────┬───────┘
                   │            │
    GraphData      │            │ GraphData
    (JSON)         │            │
                   ▼            ▼
         ┌────────────┐  ┌──────────────────┐
         │            │  │ Phase 3: Graph   │
         │  VISUALIZE │  │ Storage          │
         │            │  │ ──────────────── │
         │  D3.js     │  │ • OverGraph table│
         │ Interactive│  │ • Embeddings     │
         │  Force-    │  │   (configurable) │
         │  directed  │  │ • Nodes + Edges  │
         │  graph     │  │   ingestion      │
         └────────────┘  └─────────┬────────┘
                                   │
                                   │ stored vectors
                                   ▼
                          ┌──────────────────┐
                          │ Phase 4: GraphRAG│
                          │ ──────────────── │
                          │ Hybrid Search:   │
                          │ • Vector-semantic│
                          │ • FTS-keyword    │
                          │ • RRF fusion     │
                          │ • MMR reranking  │
                          │ • K-hop expansion│
                          └────────┬─────────┘
                                   │
                                   ▼
                          ┌──────────────────┐
                          │   AI Agent via   │
                          │   MCP Server     │
                          │ ──────────────── │
                          │ Tools:           │
                          │ • search_kb      │
                          │ • traverse_kb    │
                          │ • ping_embedder  │
                          └──────────────────┘
```

---

## ✨ Features

| Category | Feature | Status |
| :--- | :--- | :--- |
| **Indexing** | Parallel multi-core file crawling (`.gitignore` aware) | ✅ |
| | Languages: **TypeScript, JavaScript, Python, Java, Markdown** | ✅ |
| | Incremental indexing with `blake3` hashing | ✅ |
| **Graph** | Folder hierarchy extraction & classification | ✅ |
| | Symbol extraction (Functions, Classes, Interfaces, Imports, Calls) | ✅ |
| | K-hop BFS, Shortest Path, Centrality, Cycle Detection | ✅ |
| **Storage** | **OverGraph**: Hybrid Vector + FTS storage (LanceDB-backed) | ✅ |
| | Support for local & remote OpenAI-compatible embedding endpoints | ✅ |
| **Retrieval** | **GraphRAG**: Personalized PageRank (PPR) & MMR strategies | ✅ |
| | RRF (Reciprocal Rank Fusion) for hybrid search | ✅ |
| **Interface** | **Web UI**: Premium D3.js force-directed visualization | ✅ |
| | **MCP Server**: Stdio-based server for LLM integration | ✅ |
| | **CLI**: Comprehensive toolkit for all phases | ✅ |

---

## 🚀 Quick Start

### 1. Prerequisites
- **Rust** (latest stable)
- **Node.js** 20+
- (Optional) A local embedding server (e.g., [Ollama](https://ollama.ai/) or `text-embeddings-inference`)

### 2. Build the Project
```bash
npm run build
```

### 3. Generate Your First Graph
The `gen` command runs the entire pipeline (index → graph → ingest → UI).
```bash
# Run the full pipeline on the current directory
npm run gen -- -i ./ -o ugout --no-ingest
```

### 4. Visualize
Open the interactive visualization in your browser:
```bash
npm start
# Visit http://localhost:8080
```

---

## 🛠️ Command Line Interface

UltraGraph provides a powerful CLI via `node node/cli.cjs` (or the native `ug` binary).

### Common Commands

| Command | Usage | Description |
| :--- | :--- | :--- |
| `gen` | `npm run gen -- [options]` | Full pipeline: Index + Graph + Ingest + UI |
| `index` | `npm run index -- -i <dir>` | Extract symbols from a directory |
| `graph` | `npm run graph -- -i <index.json>` | Build structural graph from index |
| `ingest` | `npm run ingest -- -i <graph.json>` | Embed and store in OverGraph |
| `rag` | `npm run rag -- <db> <query>` | Perform a GraphRAG retrieval |
| `traverse`| `npm run traverse -- <db> <id>` | K-hop traversal over stored edges |

### Advanced GraphRAG Options
When using `rag` or `db-rag`, you can tune the retrieval strategy:
- `--strategy ppr`: (Default) Uses Personalized PageRank seeded by semantic hits.
- `--strategy mmr`: Uses legacy seed-expansion with Maximal Marginal Relevance.
- `--restart-prob 0.15`: Teleport probability for PPR (higher = stays closer to seeds).
- `--direction outbound`: Restrict graph walk direction.

---

## 🤖 MCP Server

Integrate UltraGraph directly into your AI Agent (Cursor, Claude Desktop, etc.).

### Tools Exposed
1.  **search_kb**: Graph-based RAG retrieval (PPR-based).
2.  **traverse_kb**: Structural walk from specific node IDs.
3.  **ping_embedder**: Verify embedding connectivity.

### Configuration
Set these environment variables before starting the server:
- `UG_DB_PATH`: Path to your OverGraph directory (default: `./ugout/ugdb`).
- `UG_REPO_ROOT`: Root path for resolving snippet file paths.
- `UG_EMBED_MODEL`: Override embedding model name.
- `UG_EMBED_BASE_URL`: Override embedding endpoint base URL.
- `UG_EMBED_API_KEY`: Override embedding API key.

```bash
UG_DB_PATH=./ugout/ugdb 

{
  "mcpServers": {
    "ultragraph": {
      "command": "node",
      "args": ["/Users/aldrickwan/Documents/project/ug/ugout/mcp-server.mjs"],
      "env": {
        "UG_DB_PATH": "/Users/aldrickwan/Documents/project/ug/ugout/ugdb"
      }
    }
  }
}

```

---

## 🔌 Native API Usage

You can use the high-performance Rust core directly in your own Node.js apps via the `native/index.js` loader.

```javascript
const { index, buildGraph, dbHybridSearch } = require('./native/index.js');

// Index a codebase
const symbols = index('./src');

// Search with GraphRAG
const context = await dbHybridSearch('./ugdb', JSON.stringify({
  query: "how does authentication work?",
  k: 10
}));
```

---

## 🧪 Testing

```bash
# Run JS test suite
npm test

# Run Native Rust tests
npm run build && cd native && cargo test
```

## 📄 License
MIT
