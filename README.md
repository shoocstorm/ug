# UltraGraph-KB: Turbo Knowledge Base Indexer

A high-performance, local-first graph-based knowledge base generator that transforms codebases/documents into a visualized queryable Semantic Knowledge Graph.

## Overview

UltraGraph-KB implements all four phases of the UltraGraph knowledge base system:

- **Phase 1**: Native turbo indexer — saturates CPU cores to map codebases in milliseconds
- **Phase 2**: In-memory graph persistence with K-hop BFS traversal
- **Phase 3**: Semantic storage & enrichment (OverGraph + local embeddings)
- **Phase 4**: GraphRAG retrieval protocol with MCP server

## Core Flow

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
         │            │  │ Phase 3: RAG     │
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

**Data Flow:**
- `Input → [Index] → [Graph] → [Visualize]`
- `Input → [Index] → [Graph] → [RAG Store] → [Hybrid Search] → [AI Agent/MCP]`

### Features

| Feature | Status |
|---------|--------|
| Parallel file crawling | ✅ |
| .gitignore respect | ✅ |
| Incremental indexing (blake3) | ✅ |
| TypeScript AST parsing | ✅ |
| Python AST parsing | ✅ |
| Markdown parsing (heading sections w/ full body spans) | ✅ |
| Folder hierarchy extraction (parent/depth/children/README/classification) | ✅ |
| NAPI-RS bridge | ✅ |
| Graph schema (Nodes/Edges) | ✅ |
| Folder forest in graph (Contains: folder→folder→file→symbol) | ✅ |
| K-hop BFS traversal | ✅ |
| Graph analysis (centrality, cycles, shortest path) | ✅ |
| Vector search (OverGraph) | ✅ |
| Hybrid search (RRF: vector + FTS) | ✅ |
| MMR reranking | ✅ |
| GraphRAG retrieval (search → expand → rank → snippet) | ✅ |
| MCP server | ✅ |
| CLI commands | ✅ |
| D3.js visualization | ✅ |

## Tech Stack

- **Core Engine (Rust)**: File walking (`ignore`), AST Parsing (`tree-sitter`), Incremental Hashing (`blake3`), Graph (`petgraph`)
- **Bridge (NAPI-RS)**: Compiles Rust logic into a native `.node` module for Node.js
- **Storage**: OverGraph (vector + FTS), local OpenAI-compatible embedding endpoint
- **MCP**: `@modelcontextprotocol/sdk` stdio server with `zod` validation
- **CLI**: Node.js 20+ with native bindings

## Using Native APIs in External Node.js Apps

If you want to use the high-performance Rust-native APIs (exposed via `ultragraph.node`) directly in your own Node.js application, you need to include the following files from this repository:

### Required Files
| File | Purpose |
|------|---------|
| `native/index.js` | Auto-generated NAPI-RS loader that detects your OS, architecture, and libc version to load the correct native binary. This is the entry point your app should require. |
| `native/ultragraph.node` | Platform-specific pre-compiled native binary. Include at minimum the binary matching your target deployment environment (e.g., `ultragraph.darwin-arm64.node` for macOS Apple Silicon). For cross-platform support, include all pre-built binaries for supported platforms. |

### Usage
Require the loader in your Node.js app (adjust the path to match your project structure):
```javascript
const { 
  index, 
  buildGraph, 
  dbHybridSearch, 
  kHopBfs 
} = require('./path/to/native/index.js');
```

### Exposed Native APIs
The native module exports the following functions (see `native/index.js:579-591` for the full list):
- `index` / `indexWithCache` — Codebase indexing with incremental caching
- `buildGraph` — Build in-memory knowledge graph from index results
- `kHopBfs` — K-hop breadth-first graph traversal
- `findShortestPath` — Find shortest path between graph nodes
- `calculateCentrality` / `detectCycles` — Graph analysis utilities
- `filterEdgesByType` — Filter graph edges by type
- `graphKeywordSearch` — Graph-based: Keyword search over in-memory graph nodes
- `dbIngest` — OverGraph: Embed graph and write to OverGraph
- `dbHybridSearch` — OverGraph: End-to-end GraphRAG hybrid retrieval (vector + FTS + graph expansion)
- `dbSemanticSearch` — OverGraph: Pure vector search over embedded graph nodes
- `dbTraverse` — OverGraph: Graph traversal using edges table with edge-type filtering
- `pingEmbedder` — Probe embedding endpoint availability

## Installation

### Prerequisites

- Rust (latest stable)
- Node.js 20+

### Build from Source

```bash
npm run build
```

This produces `native/ultragraph.node` — the native Node.js module.

## Quick Start

See [docs/QUICKSTART.md](docs/QUICKSTART.md) for a step-by-step walkthrough.

```bash
# 1. Index a folder (generate indexed-tree.json)
npm run index -- -i ./ -o ugout/indexed-tree.json

# 2. Build the graph (generate graph.json)
npm run graph -- -i ugout/indexed-tree.json -o ugout/graph.json

# 3. Visualize (see how the graph looks like visually)
npm start
# Open http://localhost:8080

# 4. Ingest graph data into OverGraph to enable semantic search (requires embedding endpoint)
npm run ingest -- -i ugout/graph.json -o ugout/ugdb --model "Qwen3-Embedding-0.6B-4bit-DWQ" --base-url "http://127.0.0.1:8000/v1" --api-key "1234"

# or with other model with different embedding dimension other than 1024
npm run ingest -- -i ugout/graph.json -o ugout/ugdb --model "text-embedding-nomic-embed-text-v1.5" --base-url "http://127.0.0.1:1234/v1" --api-key "1234" --embedding-dim 768

# 5. Semantic search
npm run rag -- -i ugout/ugdb "how does auth work" -k 8 --model "text-embedding-nomic-embed-text-v1.5" --base-url "http://127.0.0.1:1234/v1" --api-key "1234" --embedding-dim 768

# 6. Manually check lance db data (via duckdb)
duckdb
INSTALL lance
load lance;
ATTACH 'ugout/ugdb' as db (type LANCE);
select * from db.main.nodes limit 10;
```


## All CLI Commands

Use `npm run <command> -- [args]` for all commands, or directly via `node node/cli.cjs <cmd> [args]`:

### npm scripts
| npm script | Description |
|-----------|-------------|
| `npm run gen -- [options]` | Index + graph + ingest + visualization (all-in-one) |
| `npm run index -- [options]` | Index a directory |
| `npm run graph -- [options]` | Build graph from index result |
| `npm run ingest -- <graph.json> <db>` | Embed graph into OverGraph |
| `npm run rag -- <db> <query> [options]` | End-to-end GraphRAG retrieval |
| `npm run traverse -- <db> <node-id> [options]` | K-hop BFS traversal over OverGraph edges |
| `npm start` | Serve visualization at http://localhost:8080 |
| `npm run mcp` | Start MCP server (requires `UG_DB_PATH`) |

### Direct CLI commands (via `node node/cli.cjs <cmd>`)
| Command | Short Flags | Description |
|---------|-------------|-------------|
| `index` | `-i` (--input), `-c` (--cache), `-o` (--output) | Index a directory with optional caching |
| `graph` | `-i` (--input), `-o` (--output) | Build graph from index result |
| `gen` | `-i` (--input), `-c` (--cache), `-o` (--output), `-d` (--db) | Full pipeline: index → graph → visualization → OverGraph ingest |
| `graph-search` | `-t` (--type), `-o` (--output) | Keyword search over in-memory graph nodes |
| `db-ingest` | `-b` (--base-url), `-a` (--api-key), `-m` (--model), `--embedding-dim` | Embed graph nodes and write to OverGraph. Dim is auto-probed from the endpoint when `--embedding-dim` is omitted; the chosen dim is persisted in `<db>/ug-meta.json`. |
| `db-traverse` | `-k` (--hops), `-e` (--edge-type) | K-hop BFS traversal over OverGraph edges |
| `db-rag` | `-k` (--limit), `-b` (--base-url), `-a` (--api-key), `-m` (--model), `--embedding-dim` | End-to-end GraphRAG hybrid retrieval |
| `ping` | `-b` (--base-url), `-a` (--api-key), `-m` (--model), `--embedding-dim` | Probe embedding endpoint |
| `help` | `-h` (--help) | Show help for commands |

## MCP Server

```bash
# Configure via environment, then run:
UG_DB_PATH=./ugout/ugdb npm run mcp
```

Exposes three tools: `search_kb`, `traverse_kb`, `ping_embedder`.

## Tests

```bash
npm test                      # JS tests (21/21)
npm run build && cd native && cargo test  # Rust tests (67 pass)
```

