# Quick Start Guide

Get UltraGraph-KB up and running in under 5 minutes.

## What Is It

UltraGraph-KB transforms a codebase into a **semantic knowledge graph** — a structured map of files, functions, classes, and their relationships. You can query it, visualize it, and use it to power AI-assisted development (GraphRAG).

## Prerequisites

| Tool | Version |
|------|---------|
| Node.js | 20+ |
| Rust | latest stable |
| protoc | any (needed by OverGraph internals) |

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

This produces `native/ultragraph.node` — the native binary that powers everything.

## Workflow at a Glance

```
1. Index   → scan files, extract symbols
2. Graph   → build node/edge relationships
3. Query   → search, traverse, visualize
```

## Step-by-Step

### 1. Index a Codebase

```bash
node src/cli.cjs index ./src -o ugout/indexed-tree.json
```

| Flag | Default | Description |
|------|---------|-------------|
| `-i` / `--input` | `.` | Directory to scan |
| `-o` / `--output` | `ugout/indexed-tree.json` | Output JSON file |
| `-c` / `--cache` | none | Enable incremental caching |

**With cache** (faster re-indexing):

```bash
node src/cli.cjs index ./src -c ./.ug-cache
```

### 2. Build the Graph

```bash
node src/cli.cjs graph ugout/indexed-tree.json -o ugout/graph.json
```

Or use `gen` to do index + graph + visualization + OverGraph ingest in one command:

```bash
node src/cli.cjs gen -i ./src -o ./ugout
```

### 3. View the Visualization

```bash
node src/cli.cjs gen -i ./src -o ./ugout
npx serve ugout -p 8080
# Open http://localhost:8080 in browser
```

### 4. Query the Graph

**Keyword search:**

```bash
node src/cli.cjs search ugout/graph.json "authenticate" --type Function
```

**K-hop BFS:**

```bash
node src/cli.cjs bfs ugout/graph.json "file:src/auth.ts" 2
```

**Find shortest path:**

```bash
# Via Rust CLI binary (faster):
./native/target/release/ug path ugout/graph.json "file:src/auth.ts" "function:src/handler.ts:42:handleLogin"
```

## Phase 3+4: Semantic Storage & GraphRAG

These features require a running local embedding endpoint. Default config:

| Setting | Value |
|---------|-------|
| Model | `openai/Qwen3-Embedding-0.6B-4bit-DWQ` |
| Base URL | `http://localhost:8000/v1` |
| API Key | `1234` |
| Embedding dim | auto-probed from the endpoint (default 1024). Override with `--embedding-dim <n>`. |

The dim that a DB was created with is persisted in `<db>/ug-meta.json`. Re-opening
the DB with a different dim returns an `embedding dim mismatch` error rather than
silently mixing vectors of different sizes — to switch models, ingest into a fresh
db path (or `rm -rf <db>` first).

### Check Embedding Connectivity

```bash
node src/cli.cjs ping
```

### Ingest Graph into OverGraph

```bash
node src/cli.cjs ingest ugout/graph.json ugout/ugdb
```

### GraphRAG Retrieval (End-to-End)

Combines seed search → graph expansion → MMR reranking → snippet extraction:

```bash
node src/cli.cjs rag ugout/ugdb "how does authentication work" -k 8
```

### Traverse with Edge Filters

```bash
node src/cli.cjs traverse ugout/ugdb "file:src/index.ts" -k 2 --edge-type Contains --direction outbound
```

## All CLI Commands

```
gen           Index + graph + visualization + ingest (all-in-one)
index         Index a directory
graph         Build graph from index result
bfs           K-hop BFS traversal (in-memory graph)
graph-search  Keyword search over graph nodes
ingest        Embed graph into OverGraph
traverse      K-hop BFS over OverGraph edges (with type filter)
rag           End-to-end GraphRAG retrieval
ping          Probe embedding endpoint
help          Show help for commands
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
│   │   └── storage/       # OverGraph + embedding + GraphRAG
│   ├── Cargo.toml
│── ugout/ultragraph.node # Built native module
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
node src/cli.cjs index ./src -c ./.ug-cache -o ugout/indexed-tree.json
node src/cli.cjs graph ugout/indexed-tree.json -o ugout/graph.json
```

Second run only re-parses files whose blake3 hash changed.

### Full Graph Analysis

```bash
./native/target/release/ug gen -i ./src -o ./ugout
# Produces: graph.json, indexed-tree.json, analysis.json, cycles.json
```

### GraphRAG for an AI Agent

`gen` already runs ingest at the end (skip with `--no-ingest` if your embedding endpoint isn't up):

```bash
# 1. One command does indexing + graph + visualization + OverGraph ingest
node src/cli.cjs gen -i ./my-project -o ./ugout

# 2. Query with context retrieval
node src/cli.cjs rag ugout/ugdb "explain the auth flow" -k 10
```

## Troubleshooting

| Issue | Fix |
|-------|-----|
| `MODULE_NOT_FOUND: ultragraph.node` | Run `cd native && cargo build --release` |
| `protoc not found` | `brew install protobuf` |
| `embedder connect failed` | Ensure embedding endpoint is running at `localhost:8000` |
| Markdown files not indexed | Known tree-sitter version conflict (see README.md) |
