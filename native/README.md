# UltraGraph-KB Native (Rust)

High-performance Graph-based knowledge base generator built with Rust, tree-sitter, and NAPI-RS.

## Quick Start

```bash
# Build
npm run build

# Quick Generation (and visualization)
.ug/ug gen -i ./docs --no-ingest --serve

# More CLI cmds
.ug/ug help
```

## CLI Commands

### `ug gen`

Full pipeline: index + graph + ingest.

```bash
ug gen -i ../ -o ../.ug
# Produces: indexed-tree.json, graph.json and ingest graph.json into OverGraph db for semantic/hybrid search and RAG.
# Add --no-ingest to skip the ingest step if you do not have an embedding endpoint running. (see ug gen --help for more info about the embedding endpoint configuration)
# Adding --serve will start a http server at localhost:8080 to serve the graph.json in an visualized html page.
```

### `ug index`

Scan & index a directory and output an indexed-tree.json file.

```bash
ug index -i ../docs
ug index -i . --cache .cache -o indexed-tree.json # cache speeds up re-indexing
```

Options:
- `-o, --output <file>` — Output file (default: `.ug/indexed-tree.json`)
- `-c, --cache <dir>` — Cache directory for incremental indexing

### `ug graph`

Build graph.json from the indexed-tree.json.

```bash
ug graph -i indexed-tree.json -o graph.json
```

### `ug search_graph`

Keyword search over graph nodes from graph.json.

```bash
ug search_graph graph.json "visualization" -t Concept
```

### `ug bfs`

K-hop BFS traversal over in-memory graph.json.

```bash
ug bfs graph.json "file:docs/VISUALIZATION-FEATURES.md" 2
```

### `ug ingest`

Embed graph nodes into OverGraph.

```bash
ug ingest -i graph.json -o ./ugdb
```

### `ug semantic_search`

Semantic vector search.

```bash
ug semantic_search "build a tree" -d ./ugdb --filter "node_type = 'Function'"
```

### `ug hybrid_search`

Hybrid (keyword + vector) search over OverGraph, with RRF and PPR/MMR.

```bash
ug hybrid_search "build a tree" -d ./ugdb -k 2
```

### `ug traverse`

K-hop BFS over OverGraph edges.

```bash
ug traverse file:src/index.ts -d ./ugdb -k 2
```

### `ug serve`

Serve the OverGraph database with a web UI.

```bash
ug serve -d ./ -p 8080

ug serve -d ./ --repo-root ~/Documents/project/aldrickbot
```

## Storage / GraphRAG (Phase 3+4)

End-to-end against a running embedding endpoint:

```bash
ug ingest -i graph.json -o ./ugdb
ug semantic_search "build a tree" -d ./ugdb --filter "node_type = 'Function'"
ug traverse file:src/index.ts -d ./ugdb -k 2
```

## Development

### Running Tests

```bash
cargo test
```

### Building

```bash
cargo build              # Debug
cargo build --release   # Release (optimized)
```

Output:
- Library: `target/release/libultragraph.rlib`
- NAPI: `target/release/ug.node`
- Binary: `target/release/ug`

### Project Structure

```
native/
├── Cargo.toml
├── src/
│   ├── main.rs             # CLI binary
│   ├── lib.rs              # Library / NAPI exports
│   ├── project.rs          # ~/.ug/<project> folder resolution (mirrors node/cli.mjs)
│   ├── serve.rs             # `ug serve` — Axum web server + REST API
│   ├── chat.rs              # `ug chat` — RAG-grounded chat against an OpenAI-compatible LLM
│   ├── vis/                 # Embedded visualization HTML + JS bundle
│   ├── indexer.rs          # Indexing entry-point + per-file pipeline
│   ├── indexer/
│   │   ├── classifier.rs   # File classification heuristics
│   │   ├── common.rs       # File walk, hashing, path normalization
│   │   ├── folder.rs       # Folder-node derivation from scanned paths
│   │   ├── languages.rs    # Per-language indexer registry (TS/Py/Java/Rust/MD)
│   │   ├── languages/      # Per-language tree-sitter extractors (ts, py, java, rust, md)
│   │   ├── pdf.rs          # PDF text extractor (pdf-extract, one Symbol per page)
│   │   └── package_json.rs # package.json dependency parsing
│   ├── graph.rs            # Graph building + BFS + analysis
│   ├── types.rs            # Data structures
│   └── storage/
│       ├── mod.rs
│       ├── db.rs             # OverGraph schemas + queries
│       ├── embed.rs          # Remote embedding HTTP client
│       ├── embed_local.rs    # In-process ONNX embedder (fastembed)
│       ├── ingest.rs         # Embed + upsert pipeline
│       ├── query.rs          # search, traverse, RRF, MMR, snippets
│       ├── ppr.rs            # Personalized PageRank
│       ├── store.rs          # `KnowledgeStore` trait (multi-destination)
│       ├── types_registry.rs # Stable string↔u32 type-id mapping
│       ├── napi_bindings.rs  # NAPI async fns
│       ├── text.rs           # Embedding text shaping (folder synopsis fallback)
│       └── backends/
│           └── neo4j.rs      # Neo4j `KnowledgeStore` implementation
└── tests/
    ├── indexer_test.rs        # 13 tests
    ├── graph_test.rs          # 29 tests
    ├── search_test.rs         # 13 tests
    ├── storage_test.rs        # 7 tests
    ├── rust_indexer_test.rs   # 17 tests
    ├── pdf_indexer_test.rs    # 11 tests
    ├── storage_bench.rs       # 2 tests, #[ignore] by default
    ├── neo4j_smoke.rs         # 4 tests, #[ignore] — needs a running Neo4j
    └── neo4j_write_smoke.rs   # 3 tests, #[ignore] — needs a running Neo4j
```

## Features

### Indexer
- Parallel directory walking (respects .gitignore)
- Incremental hashing (blake3)
- AST parsing (tree-sitter)
  - TypeScript/JavaScript
  - Python
  - Java
  - **Rust** (`function_item` / `struct_item` / `enum_item` / `trait_item` / `type_item` /
    `const_item` / `static_item` / `macro_definition`; `impl` block methods get qualified
    as `Type::method` with `implements: [Trait]` on `impl Trait for Type` methods;
    `use` declarations expand brace-groups and `as` aliases into per-import records;
    `///` and `//!` doc-comment runs collapse into `docstring`)
  - Markdown / MDX (heading sections carry full-body `end_line` spans for downstream summarization)
- Binary document parsing
  - **PDF** via `pdf-extract` (pure-Rust, no native deps): one `Symbol` per page,
    `kind: "heading_1"` so pages map cleanly to `Concept` graph nodes with a
    `Contains` edge from the file. Page text → `docstring` (capped at 8 KB) so
    semantic search can rank it. Empty / image-only pages emit a `(no text)`
    stub. Extension match is case-insensitive (`.PDF`, `.Pdf` all work).
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
- OverGraph persistence (nodes + edges tables)
- Vector search (embedding dimension is configurable per DB; default 1024, auto-probed at ingest)
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
- `overgraph` — Graph and Vector database
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
