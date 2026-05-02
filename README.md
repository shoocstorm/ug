# UltraGraph-KB: Turbo Knowledge Base Indexer

A high-performance, local-first knowledge base generator that transforms codebases into a queryable Semantic Knowledge Graph.

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
         │  D3.js     │  │ • OverGraph tables │
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

If you want to use the high-performance Rust-native APIs (exposed via `ultragraph-kb.node`) directly in your own Node.js application, you need to include the following files from this repository:

### Required Files
| File | Purpose |
|------|---------|
| `native/index.js` | Auto-generated NAPI-RS loader that detects your OS, architecture, and libc version to load the correct native binary. This is the entry point your app should require. |
| `native/ultragraph-kb.node` | Platform-specific pre-compiled native binary. Include at minimum the binary matching your target deployment environment (e.g., `ultragraph-kb.darwin-arm64.node` for macOS Apple Silicon). For cross-platform support, include all pre-built binaries for supported platforms. |

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
- `protoc` (`brew install protobuf` on macOS)

### Build from Source

```bash
npm run prebuild
```

This produces `native/ultragraph-kb.node` — the native Node.js module.

## Quick Start

See [docs/QUICKSTART.md](docs/QUICKSTART.md) for a step-by-step walkthrough.

```bash
# 1. Index a folder
npm run index -- ./src -o ug-out/indexed-tree.json

# 2. Build the graph
npm run graph -- ug-out/indexed-tree.json -o ug-out/graph.json

# 3. Visualize (or use all-in-one: npm run gen -- ./src -o ug-out/)
npm start
# Open http://localhost:8080

# 4. Semantic search (requires embedding endpoint)
npm run ingest -- ug-out/graph.json ug-out/ugdb
npm run rag -- ug-out/ugdb "how does auth work" -k 8

# 5. Manually check lance db data (via duckdb)
duckdb
INSTALL lance
load lance;
ATTACH 'ug-out/ugdb' as db (type LANCE);
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
| `db-ingest` | `-b` (--base-url), `-a` (--api-key), `-m` (--model) | Embed graph nodes and write to OverGraph |
| `db-traverse` | `-k` (--hops), `-e` (--edge-type) | K-hop BFS traversal over OverGraph edges |
| `db-rag` | `-k` (--limit), `-b` (--base-url), `-a` (--api-key), `-m` (--model) | End-to-end GraphRAG hybrid retrieval |
| `ping` | `-b` (--base-url), `-a` (--api-key), `-m` (--model) | Probe embedding endpoint |
| `help` | `-h` (--help) | Show help for commands |

### Examples
```bash
# Index a folder
node node/cli.cjs index -i ./src -c ./cache -o ug-out/indexed-tree.json

# Build graph from index result
node node/cli.cjs graph -i ./src -o ug-out/graph.json

# keyword based graph search with type filter
node node/cli.cjs graph-search ug-out/graph.json "auth" -t Function -t Class

# DB ingest with custom embedder
node node/cli.cjs db-ingest graph.json ./ugdb -b http://localhost:11434/v1 -m llama3

# Traverse with edge-type filter
node node/cli.cjs db-traverse ./ugdb "node-123" -k 3 -e Calls -e Imports

# RAG search
node node/cli.cjs db-rag ./ugdb "how does auth work" -k 8

# Get help for a command
node node/cli.cjs gen -h
```

## MCP Server

```bash
# Configure via environment, then run:
UG_DB_PATH=./ug-out/ugdb npm run mcp
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
│   ├── node/
│   │   ├── lib.rs            # NAPI-RS entry point
│   │   ├── main.rs           # Rust CLI binary (ug)
│   │   ├── indexer.rs        # File scanning + AST parsing
│   │   ├── graph.rs          # Graph building + BFS + analysis
│   │   ├── types.rs          # Shared data structures
│   │   └── storage/          # OverGraph + embedding + GraphRAG
│   │       ├── db.rs         # OverGraph schemas + queries
│   │       ├── embed.rs      # Embedding HTTP client
│   │       ├── ingest.rs     # Embed + upsert pipeline
│   │       ├── query.rs      # search, traverse, RRF, MMR
│   │       ├── napi_bindings.rs  # NAPI async fns
│   │       └── text.rs       # Embedding text shaping
└── ug-out/ultragraph-kb.node    # Built native module
├── node/
│   ├── cli.cjs               # JavaScript CLI
│   ├── vis/                  # D3.js visualization
│   └── mcp-server.mjs        # MCP stdio server
│   └── test/
│       └── test-runner.cjs   # Test suite (21 tests)
└── docs/                     # Design docs + quick start
```
