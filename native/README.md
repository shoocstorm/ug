# UltraGraph-KB Native (Rust)

High-performance Graph-based knowledge base generator & visualizer built with Rust and tree-sitter.

> Full documentation for all commands, API routes, schemas, and architecture: **[docs/api-reference.md](../docs/api-reference.md)**

## Quick Start

```bash
cargo build --release
target/release/ug gen -i ./docs --no-ingest --serve
target/release/ug help
```

## CLI Commands

All commands are documented in full detail in [api-reference.md](../docs/api-reference.md). The most common entry points:

```bash
ug gen -i ../                     # Full pipeline: index + graph + ingest
ug serve                     # Serve with web UI
ug semantic_search "build a tree"           # Semantic vector search
ug search "build a tree"                    # Hybrid (keyword + vector) search
ug traverse file:src/index.ts               # K-hop BFS over graph edges
```

## Development

```bash
cargo test                    # Run tests
cargo build                   # Debug build
cargo build --release         # Release build
cargo llvm-cov --html         # test coverage
```

Output: `target/release/ug` (CLI + MCP + server) and `target/release/ug-app` (desktop shell).

<details>
<summary><b>Project Structure</b></summary>

```
native/
├── Cargo.toml
├── src/
│   ├── main.rs             # CLI binary
│   ├── lib.rs              # Library crate root
│   ├── project.rs          # ~/.ug/<project> folder resolution
│   ├── serve.rs             # `ug serve` — Axum web server + REST API
│   ├── chat.rs              # `ug chat` — RAG-grounded chat
│   ├── mcp/                 # MCP server + install/uninstall
│   ├── vis/                 # Embedded visualization HTML + JS
│   ├── indexer.rs          # Indexing entry-point
│   ├── indexer/             # Classifier, common, folder, languages, pdf, package_json
│   ├── graph.rs            # Graph building + BFS + analysis
│   ├── types.rs            # Data structures
│   └── storage/            # OverGraph + Neo4j backends, embedding, query, ingest
└── tests/                  # Integration tests (indexer, graph, search, storage, etc.)
```

</details>

<details>
<summary><b>Features</b></summary>

- **Indexer**: Parallel directory walking (respects .gitignore), incremental hashing (blake3), AST parsing via tree-sitter (TypeScript, Python, Java, Rust, Markdown/MDX), PDF extraction. Symbol extraction includes functions, classes, interfaces, signatures, docstrings, imports/exports, inheritance, type refs, calls. Folder hierarchy with classification, README detection, package.json parsing.
- **Graph**: File/Folder/Function/Class/Interface/Concept/Dependency/Config nodes. Contains/Imports/Exports/Extends/Implements/Calls/References edges. BFS traversal, centrality (degree + betweenness), cycle detection, shortest path, edge-type filtering.
- **Storage & GraphRAG**: OverGraph + Neo4j persistence. Vector search, FTS, RRF hybrid search, MMR reranking, graph expansion, code snippets, token-budgeted context assembly.

</details>

<details>
<summary><b>Dependencies</b></summary>

`tree-sitter` · `tree-sitter-typescript` · `tree-sitter-python` · `blake3` · `ignore` · `petgraph` · `rayon` · `overgraph` · `tokio` · `reqwest` · `axum` · `tauri`

</details>

## Performance

- Indexing: < 5s for 1,000-file repo
- BFS: < 100ms for 3-hop traversal
- Memory: < 500MB during indexing
