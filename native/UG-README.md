# UltraGraph-KB Native (Rust)

High-performance knowledge base indexer built with Rust, tree-sitter, and NAPI-RS.

## Quick Start

```bash
# Build the project
cd native
cargo build --release

# Run CLI
alias ug=./target/release/ug
./target/release/ug --help

# Run tests
cargo test
```

## CLI Commands

### `ug index <path>`

Index a directory and output JSON.

```bash
ug index -i ../src          # Index src folder
ug index -i . --cache .cache -o out.json # With incremental caching
```

Options:
- `-o, --output <file>` - Output file (default: `out/indexed-tree.json`)
- `-c, --cache <dir>` - Cache directory for incremental indexing

### `ug graph`

Build graph from index result.

```bash
ug graph
ug graph -i index.json -o graph.json   # From file
ug graph -i index.json                 # Positional args
```

Options:
- `-i, --input <file>` - Input index file
- `-o, --output <file>` - Output graph file

### `ug bfs`

K-hop BFS traversal on graph.

```bash
ug bfs graph.json "file:src/index.ts" 2  # 2-hop BFS
ug bfs graph.json node_id 1 -o result.json
```

Options:
- `-o, --output <file>` - Output file (optional)

### `ug gen`

Generate full output: indexing + graph building (combination of ug index + ug graph).

```bash
ug gen -i ./lib -o ./out              # Generate in ./out
ug gen                                 # Use defaults (current dir)
```

Options:
- `-i, --input <path>` - Input directory
- `-o, --output <dir>` - Output directory
- `-c, --cache <dir>` - Cache directory

## Development

### Running Tests

```bash
cargo test              # Run all tests
cargo test --test indexer_test   # Just indexer tests
cargo test --test graph_test     # Just graph tests
```

Test coverage:
- **31 tests total** (13 indexer + 18 graph)
- Tests use `tempfile` for isolated test directories

### Building

```bash
cargo build              # Debug build
cargo build --release   # Release build (optimized)
```

Output:
- Library: `target/release/libultragraph_kb.rlib`
- NAPI: `target/release/ultragraph_kb.node` (for Node.js)
- Binary: `target/release/ug` (CLI)

### Project Structure

```
native/
├── Cargo.toml          # Rust dependencies
├── src/
│   ├── main.rs        # CLI binary
│   ├── lib.rs         # Library exports
│   ├── indexer.rs     # File indexing logic
│   ├── graph.rs       # Graph building & BFS
│   └── types.rs       # Data structures
├── tests/
│   ├── indexer_test.rs  # Indexer tests (13)
│   └── graph_test.rs    # Graph tests (18)
└── target/            # Build output
```

## Features

### Indexer
- Parallel directory walking (respects .gitignore)
- Incremental hashing (blake3)
- AST parsing (tree-sitter)
  - TypeScript/JavaScript
  - Python
- Symbol extraction:
  - Functions, classes, interfaces
  - Function signatures (params, return types)
  - Docstrings (JSDoc)
  - Imports/exports
  - Inheritance (extends/implements)
  - Type references
- File classification (Component, Page, Hook, Util, Service, Config, Type, Constant, Context, Reducer, Test, Asset)
- Package.json dependency extraction

### Graph
- Node types: File, Function, Class, Interface
- Edge types: Contains, Imports, Extends, Implements, Calls, References
- K-hop BFS traversal
- Deduplication

## Dependencies

- `tree-sitter` - AST parsing
- `tree-sitter-typescript` - TypeScript parser
- `tree-sitter-python` - Python parser
- `blake3` - Incremental hashing
- `ignore` - File walking
- `petgraph` - Graph algorithms
- `rayon` - Parallel processing
- `regex` - Pattern matching
- `napi-rs` - Node.js bindings


## Extensibility
Support a new language, i.e.: Java.
Adding Java is now a 5-step additive change (documented in languages.rs):
  1. Drop languages/java.rs implementing LanguageIndexer.
  2. Add mod java; in languages.rs.
  3. Register the extensions in for_extension.
  4. Add the same exts to common::SUPPORTED_EXTS.
  5. Add tree-sitter-java to Cargo.toml.

## Performance

- Target: < 5 seconds for 1,000-file repo
- Target: < 100ms for 3-hop BFS
- Memory: < 500MB during indexing
