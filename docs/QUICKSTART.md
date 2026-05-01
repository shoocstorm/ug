# Quick Start Guide

Get UltraGraph-KB up and running in under 5 minutes.

## What Is It

UltraGraph-KB transforms a codebase into a **semantic knowledge graph** — a structured map of files, functions, classes, and their relationships. You can query it, visualize it, and use it to power AI-assisted development (GraphRAG).

## Prerequisites

| Tool | Version |
|------|---------|
| Node.js | 20+ |
| Rust | latest stable |
| protoc | any (needed by LanceDB internals) |

On macOS:

```bash
brew install protobuf
```

Verify:

```bash
node -v          # v20 or later
rustc --version  # rustc 1.x
protoc --version # libprotoc x.x
```

## Installation

```bash
git clone <repo-url>
cd ug
cd native && cargo build --release && cd ..
```

This produces `native/ultragraph-kb.node` — the native binary that powers everything.

## Workflow at a Glance

```
1. Index   → scan files, extract symbols
2. Graph   → build node/edge relationships
3. Query   → search, traverse, visualize
```

## Step-by-Step

### 1. Index a Codebase

```bash
node src/cli.cjs index ./src -o out/indexed-tree.json
```

| Flag | Default | Description |
|------|---------|-------------|
| `-i` / `--input` | `.` | Directory to scan |
| `-o` / `--output` | `out/indexed-tree.json` | Output JSON file |
| `-c` / `--cache` | none | Enable incremental caching |

**With cache** (faster re-indexing):

```bash
node src/cli.cjs index ./src -c ./.ug-cache
```

### 2. Build the Graph

```bash
node src/cli.cjs graph out/indexed-tree.json -o out/graph.json
```

Or use `gen` to do index + graph + visualization in one command:

```bash
node src/cli.cjs gen -i ./src -o ./out
```

### 3. View the Visualization

```bash
node src/cli.cjs gen -i ./src -o ./out
npx serve out -p 8080
# Open http://localhost:8080 in browser
```

### 4. Query the Graph

**Keyword search:**

```bash
node src/cli.cjs search out/graph.json "authenticate" --type Function
```

**K-hop BFS:**

```bash
node src/cli.cjs bfs out/graph.json "file:src/auth.ts" 2
```

**Find shortest path:**

```bash
# Via Rust CLI binary (faster):
./native/target/release/ug path out/graph.json "file:src/auth.ts" "function:src/handler.ts:42:handleLogin"
```

## Phase 3+4: Semantic Storage & GraphRAG

These features require a running local embedding endpoint. Default config:

| Setting | Value |
|---------|-------|
| Model | `openai/Qwen3-Embedding-0.6B-4bit-DWQ` |
| Base URL | `http://localhost:8000/v1` |
| API Key | `1234` |

### Check Embedding Connectivity

```bash
node src/cli.cjs ping
```

### Ingest Graph into LanceDB

```bash
node src/cli.cjs ingest out/graph.json out/kg_db
```

### Semantic (Vector) Search

```bash
node src/cli.cjs vsearch out/kg_db "oauth login flow" -k 5
```

With SQL filter:

```bash
node src/cli.cjs vsearch out/kg_db "build a tree" --filter "node_type = 'Function'"
```

### GraphRAG Retrieval (End-to-End)

Combines seed search → graph expansion → MMR reranking → snippet extraction:

```bash
node src/cli.cjs rag out/kg_db "how does authentication work" -k 8
```

### Traverse with Edge Filters

```bash
node src/cli.cjs traverse out/kg_db "file:src/index.ts" -k 2 --edge-type Contains --direction outbound
```

## All CLI Commands

```
index      Index a directory
graph      Build graph from index result
gen        Index + graph + visualization (all-in-one)
bfs        K-hop BFS traversal (in-memory graph)
search     Keyword search over graph nodes
ingest     Embed graph into LanceDB
vsearch    Semantic vector search over LanceDB
traverse   K-hop BFS over LanceDB edges (with type filter)
rag        End-to-end GraphRAG retrieval
ping       Probe embedding endpoint
help       Show help for commands
```

## Tests

```bash
npm test
```

Run Rust tests:

```bash
cd native && cargo test
```

## Project Structure

```
ug/
├── native/
│   ├── src/
│   │   ├── lib.rs         # NAPI-RS entry point
│   │   ├── main.rs        # Rust CLI binary
│   │   ├── indexer.rs     # File scanning + AST parsing
│   │   ├── graph.rs       # Graph building + BFS
│   │   ├── types.rs       # Shared data structures
│   │   └── storage/       # LanceDB + embedding + GraphRAG
│   ├── Cargo.toml
│   └── ultragraph-kb.node # Built native module
├── src/
│   ├── cli.cjs            # JavaScript CLI
│   ├── vis/               # D3.js visualization
│   └── test/
│       └── test-runner.cjs
└── docs/                  # Design docs
```

## Common Patterns

### Incremental Re-Index (Only Changed Files)

```bash
node src/cli.cjs index ./src -c ./.ug-cache -o out/indexed-tree.json
node src/cli.cjs graph out/indexed-tree.json -o out/graph.json
```

Second run only re-parses files whose blake3 hash changed.

### Full Graph Analysis

```bash
./native/target/release/ug gen -i ./src -o ./out
# Produces: graph.json, indexed-tree.json, analysis.json, cycles.json
```

### GraphRAG for an AI Agent

```bash
# 1. Index and build graph
node src/cli.cjs gen -i ./my-project -o ./out

# 2. Embed into LanceDB
node src/cli.cjs ingest out/graph.json out/kg_db

# 3. Query with context retrieval
node src/cli.cjs rag out/kg_db "explain the auth flow" -k 10
```

## Troubleshooting

| Issue | Fix |
|-------|-----|
| `MODULE_NOT_FOUND: ultragraph-kb.node` | Run `cd native && cargo build --release` |
| `protoc not found` | `brew install protobuf` |
| `embedder connect failed` | Ensure embedding endpoint is running at `localhost:8000` |
| Markdown files not indexed | Known tree-sitter version conflict (see README.md) |
