# UltraGraph-KB: Turbo Knowledge Base Indexer

A high-performance, local-first knowledge base generator that transforms codebases into a queryable Semantic Knowledge Graph.

## Overview

UltraGraph-KB implements all four phases of the UltraGraph knowledge base system:

- **Phase 1**: Native turbo indexer — saturates CPU cores to map codebases in milliseconds
- **Phase 2**: In-memory graph persistence with K-hop BFS traversal
- **Phase 3**: Semantic storage & enrichment (LanceDB + local embeddings)
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
              │  Nodes: File/Function/  │
              │    Class/Interface/     │
              │    Concept/Dependency   │
              │  Edges: Contains/       │
              │    Imports/Calls/       │
              │    Extends/References   │
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
         │  D3.js     │  │ • LanceDB tables │
         │ Interactive│  │ • Embeddings     │
         │  Force-    │  │   (1024-dim)     │
         │  directed  │  │ • Nodes + Edges  │
         │  graph     │  │   ingestion      │
         │            │  └────────-┬────────┘
         └────────────┘            │
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
| Markdown parsing | ✅ |
| NAPI-RS bridge | ✅ |
| Graph schema (Nodes/Edges) | ✅ |
| K-hop BFS traversal | ✅ |
| Graph analysis (centrality, cycles, shortest path) | ✅ |
| Vector search (LanceDB) | ✅ |
| Hybrid search (RRF: vector + FTS) | ✅ |
| MMR reranking | ✅ |
| GraphRAG retrieval (search → expand → rank → snippet) | ✅ |
| MCP server | ✅ |
| CLI commands | ✅ |
| D3.js visualization | ✅ |

## Tech Stack

- **Core Engine (Rust)**: File walking (`ignore`), AST Parsing (`tree-sitter`), Incremental Hashing (`blake3`), Graph (`petgraph`)
- **Bridge (NAPI-RS)**: Compiles Rust logic into a native `.node` module for Node.js
- **Storage**: LanceDB (vector + FTS), local OpenAI-compatible embedding endpoint
- **MCP**: `@modelcontextprotocol/sdk` stdio server with `zod` validation
- **CLI**: Node.js 20+ with native bindings

## Using Native APIs in External Node.js Apps

If you want to use the high-performance Rust-native APIs (exposed via `ultragraph-kb.node`) directly in your own Node.js application, you need to include the following files from this repository:

### Required Files
| File | Purpose |
|------|---------|
| `native/index.js` | Auto-generated NAPI-RS loader that detects your OS, architecture, and libc version to load the correct native binary. This is the entry point your app should require. |
| `native/ultragraph-kb.<platform>-<arch>.node` | Platform-specific pre-compiled native binary. Include at minimum the binary matching your target deployment environment (e.g., `ultragraph-kb.darwin-arm64.node` for macOS Apple Silicon). For cross-platform support, include all pre-built binaries for supported platforms. |

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
- `dbIngest` — LanceDB: Embed graph and write to LanceDB
- `dbHybridSearch` — LanceDB: End-to-end GraphRAG hybrid retrieval (vector + FTS + graph expansion)
- `dbSemanticSearch` — LanceDB: Pure vector search over embedded graph nodes
- `dbTraverse` — LanceDB: Graph traversal using edges table with edge-type filtering
- `pingEmbedder` — Probe embedding endpoint availability

## Installation

### Prerequisites

- Rust (latest stable)
- Node.js 20+
- `protoc` (`brew install protobuf` on macOS)

### Build from Source

```bash
npm run prebuild
```

This produces `native/ultragraph-kb.node` — the native Node.js module.

## Quick Start

See [docs/QUICKSTART.md](docs/QUICKSTART.md) for a step-by-step walkthrough.

```bash
# 1. Index a codebase
npm run index -- ./src -o out/indexed-tree.json

# 2. Build the graph
npm run graph -- out/indexed-tree.json -o out/graph.json

# 3. Visualize (or use all-in-one: npm run gen -- ./src -o out/)
npm start
# Open http://localhost:8080

# 4. Semantic search (requires embedding endpoint)
npm run ingest -- out/graph.json out/kg_db
npm run rag -- out/kg_db "how does auth work" -k 8

# 5. Manually check lance db data (via duckdb)
duckdb
INSTALL lance
load lance;
ATTACH 'native/out/kg_db' as db (type LANCE);
select * from db.main.nodes limit 10;
```


## All CLI Commands

Use `npm run <command> -- [args]` for all commands:

| npm script | Description |
|-----------|-------------|
| `npm run index -- <dir> -o <out>` | Index a directory |
| `npm run graph -- <index.json> -o <out>` | Build graph from index result |
| `npm run gen -- <dir> -o <out>` | Index + graph + visualization (all-in-one) |
| `npm run ingest -- <graph.json> <db_path>` | Embed graph into LanceDB |
| `npm run search -- <db> <query>` | Semantic vector search over LanceDB |
| `npm run rag -- <db> <query> -k <num>` | End-to-end GraphRAG retrieval |
| `npm start` | Serve visualization at http://localhost:8080 |
| `npm run mcp` | Start MCP server (requires `UG_DB_PATH`) |

Direct CLI commands (via `node src/cli.cjs <cmd>`):
- `bfs` - K-hop BFS traversal (in-memory graph)
- `search` - Keyword search over graph nodes
- `traverse` - K-hop BFS over LanceDB edges (with edge-type filter)
- `ping` - Probe embedding endpoint
- `help` - Show help

## MCP Server

```bash
# Configure via environment, then run:
UG_DB_PATH=./out/kg_db npm run mcp
```

Exposes three tools: `search_kb`, `traverse_kb`, `ping_embedder`.

## Tests

```bash
npm test                      # JS tests (21/21)
npm run prebuild && cd native && cargo test  # Rust tests (67 pass)
```

## Project Structure

```
ug/
├── native/
│   ├── src/
│   │   ├── lib.rs            # NAPI-RS entry point
│   │   ├── main.rs           # Rust CLI binary (ug)
│   │   ├── indexer.rs        # File scanning + AST parsing
│   │   ├── graph.rs          # Graph building + BFS + analysis
│   │   ├── types.rs          # Shared data structures
│   │   └── storage/          # LanceDB + embedding + GraphRAG
│   │       ├── db.rs         # LanceDB schemas + queries
│   │       ├── embed.rs      # Embedding HTTP client
│   │       ├── ingest.rs     # Embed + upsert pipeline
│   │       ├── query.rs      # search, traverse, RRF, MMR
│   │       ├── napi_bindings.rs  # NAPI async fns
│   │       └── text.rs       # Embedding text shaping
│   └── ultragraph-kb.node    # Built native module
├── src/
│   ├── cli.cjs               # JavaScript CLI
│   ├── vis/                  # D3.js visualization
│   ├── mcp/
│   │   └── mcp-server.mjs        # MCP stdio server
│   └── test/
│       └── test-runner.cjs   # Test suite (21 tests)
└── docs/                     # Design docs + quick start
```
