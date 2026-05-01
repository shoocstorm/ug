# UltraGraph-KB Native (Rust)

High-performance knowledge base indexer built with Rust, tree-sitter, and NAPI-RS.

## Quick Start

```bash
# Build
cd native && cargo build --release

# CLI
./target/release/ug --help

# Tests
cargo test
```

## CLI Commands

### `ug index <path>`

Index a directory and output JSON.

```bash
ug index -i ../src
ug index -i . --cache .cache -o out.json
```

Options:
- `-o, --output <file>` — Output file (default: `ug-out/indexed-tree.json`)
- `-c, --cache <dir>` — Cache directory for incremental indexing

### `ug graph`

Build graph from index result.

```bash
ug graph -i index.json -o graph.json
```

### `ug search_graph`

Keyword search over graph nodes.

```bash
ug search_graph ./ug-out/graph.json "Cache" -t function
```

### `ug bfs`

K-hop BFS traversal (in-memory graph).

```bash
ug bfs graph.json "file:src/index.ts" 2
```

### `ug gen`

Full pipeline: index + graph + analysis.

```bash
ug gen -i ./src -o ./ug-out
# Produces: graph.json, indexed-tree.json, analysis.json, cycles.json
```

### `ug ingest`

Embed graph nodes into LanceDB.

```bash
ug ingest -g ug-out/graph.json -d ug-out/ug-db --with-indexes
```

### `ug semantic_search`

Semantic vector search.

```bash
ug semantic_search "build a tree" -d ug-out/ug-db --filter "node_type = 'Function'"
```

### `ug traverse`

K-hop BFS over LanceDB edges.

```bash
ug traverse file:src/index.ts -d ug-out/ug-db -k 2
```

## Storage / GraphRAG (Phase 3+4)

End-to-end against a running embedding endpoint:

```bash
ug ingest -g ug-out/graph.json -d ug-out/ug-db --with-indexes
ug semantic_search "build a tree" -d ug-out/ug-db --filter "node_type = 'Function'"
ug traverse file:src/index.ts -d ug-out/ug-db -k 2
```

## Development

### Running Tests

```bash
cargo test              # All suites (67 tests)
cargo test --test indexer_test
cargo test --test graph_test
cargo test --test search_test
cargo test --test storage_test
```

### Building

```bash
cargo build              # Debug
cargo build --release   # Release (optimized)
```

Output:
- Library: `target/release/libultragraph_kb.rlib`
- NAPI: `target/release/ultragraph-kb.node`
- Binary: `target/release/ug`

### Project Structure

```
native/
├── Cargo.toml
├── src/
│   ├── main.rs             # CLI binary
│   ├── lib.rs              # Library exports
│   ├── indexer.rs          # Indexing entry-point + per-file pipeline
│   ├── indexer/
│   │   ├── classifier.rs   # File classification heuristics
│   │   ├── common.rs       # File walk, hashing, path normalization
│   │   ├── folder.rs       # Folder-node derivation from scanned paths
│   │   ├── languages.rs    # Per-language indexer registry (TS/Py/Java/MD)
│   │   ├── languages/      # Per-language tree-sitter extractors
│   │   └── package_json.rs # package.json dependency parsing
│   ├── graph.rs            # Graph building + BFS + analysis
│   ├── types.rs            # Data structures
│   └── storage/
│       ├── mod.rs
│       ├── db.rs           # LanceDB schemas + queries
│       ├── embed.rs        # Embedding HTTP client
│       ├── ingest.rs       # Embed + upsert pipeline
│       ├── query.rs        # search, traverse, RRF, MMR, snippets
│       ├── napi_bindings.rs   # NAPI async fns
│       └── text.rs         # Embedding text shaping (folder synopsis fallback)
└── tests/
    ├── indexer_test.rs     # 29 tests
    ├── graph_test.rs       # 13 tests
    ├── search_test.rs      # 13 tests
    └── storage_test.rs     # 12 tests
```

## Features

### Indexer
- Parallel directory walking (respects .gitignore)
- Incremental hashing (blake3)
- AST parsing (tree-sitter)
  - TypeScript/JavaScript
  - Python
  - Java
  - Markdown / MDX (heading sections carry full-body `end_line` spans for downstream summarization)
- Symbol extraction:
  - Functions, classes, interfaces
  - Function signatures (params, return types)
  - Docstrings (JSDoc @param/@returns)
  - Imports/exports
  - Inheritance (extends/implements)
  - Type references
  - Function calls
- File classification
- Folder hierarchy extraction:
  - Synthetic `.` root, every folder with `parent` / `depth` / `childFiles` / `childFolders`
  - Recursive `totalFiles` and `languageBreakdown` for character signal
  - README detection (`README.md` / `_index.md` / `index.md`)
  - Folder classification (Tests / Documentation / Components / Source / Mixed / …) via path-name + content fallback
  - `summary` slot reserved for the Semantic Enrichment phase
- Package.json dependency extraction

### Graph
- Node types: File, Folder, Function, Class, Interface, Concept (markdown headings), Dependency, Config
- Edge types: Contains, Imports, Exports, Extends, Implements, Calls, References
- Folder forest is wired with `Contains` edges (parent_folder → child_folder, folder → immediate_file), giving a clean `folder → folder → file → symbol` traversal chain that all query primitives (BFS, centrality, shortest-path, search) work over for free
- K-hop BFS traversal
- Centrality (degree + betweenness)
- Cycle detection
- Shortest path
- Edge-type filtering

### Storage & GraphRAG
- LanceDB persistence (nodes + edges tables)
- Vector search (1024-dim embeddings)
- FTS search (name + description)
- RRF hybrid search (vector + FTS fusion)
- MMR reranking (relevance vs. diversity)
- Graph expansion with direction + edge-type filter
- Code snippet extraction
- Token-budgeted context assembly
- Folder-aware embedding text: pre-enrichment, folder nodes embed with a synthesized synopsis ("`<classification>` folder, N typescript and M markdown files, depth D"); the storage layer prefers `folder.summary` once enrichment fills it
- `search_kb` — Phase 4 entry point

## Dependencies

- `tree-sitter` — AST parsing
- `tree-sitter-typescript` — TypeScript parser
- `tree-sitter-python` — Python parser
- `blake3` — Incremental hashing
- `ignore` — File walking
- `petgraph` — Graph algorithms
- `rayon` — Parallel processing
- `lancedb` — Vector database
- `arrow` / `arrow-array` / `arrow-schema` — LanceDB data format
- `tokio` — Async runtime
- `reqwest` — HTTP client (embeddings)
- `napi-rs` — Node.js bindings

## Extensibility

Adding a new language is a 5-step additive change:
1. Drop a new `languages/<name>.rs` implementing `LanguageIndexer`
2. Add `mod <name>;` in `languages.rs`
3. Register extensions in `for_extension`
4. Add exts to `common::SUPPORTED_EXTS`
5. Add `tree-sitter-<name>` to `Cargo.toml`

## Performance

- Target: < 5 seconds for 1,000-file repo
- Target: < 100ms for 3-hop BFS
- Memory: < 500MB during indexing
