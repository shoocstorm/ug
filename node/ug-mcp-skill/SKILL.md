---
name: ug-mcp
description: |
  UltraGraph MCP tools — efficient codebase/documentation KB search through a
  semantic knowledge graph. Use when the session has ultragraph MCP tools
  (search, find_symbol, traverse, etc.) connected.
---

# UltraGraph MCP Tool Guide

## Available Tools

| Tool | Cost | Batch? | What it does |
|------|------|--------|-------------|
| `project_overview` | cheap | — | One-shot repo orientation: node counts, biggest files, hotspot symbols |
| `graph_schema` | cheap | — | Node/edge types actually in this graph + full vocabulary. Check before filtering. |
| `find_symbol` | cheap | ✅ `nodeId[]` OR `name[]` | Direct nodeId lookup (O(1)) OR exact/substring name lookup → nodeId(s) |
| `file_outline` | cheap | ✅ `nodeId[]` OR `file[]` | Direct nodeId lookup (O(1) OR file path → symbols in file |
| `get_code` | cheap | ✅ `nodeId[]` | Read full source for nodeId(s) or a file:line range |
| `find_usages` | cheap | ✅ `nodeId[]` | Callers/importers of symbol(s), with the matching call-site lines |
| `shortest_path` | cheap | — | Shortest directed path between two nodeIds |
| `list_projects` | cheap | — | List all indexed repos on this machine |
| `ping_embedder` | cheap | — | Embedding endpoint health check |
| `semantic_search` | medium | — | Pure vector lookup → candidate nodeIds (no code snippets) |
| `traverse` | medium | ✅ `nodeId[]` | N-hop graph walk from nodeId(s) — deps or dependents |
| `search` | expensive | — | Full GraphRAG: PPR-ranked snippets with code context |
| `reindex` | expensive | — | Re-run index→graph→embed pipeline |

**Cheap tools**: no DB round-trip or cheap name-only scan.  
**Medium tools**: single DB query (embedding or graph hop).  
**Expensive tools**: multi-pass (embed → rank → expand → snippet-read).

**Batch?**: those params accept a single string OR an array of up to 10.
**Always batch related lookups into ONE call** — one batched call is one
round trip; N separate calls cost N context re-reads. A bad id in a batch
doesn't fail the call — it returns an inline `✗` section.

## Quick-Reference Decision Tree

```
What do you need?
│
├─ "Where is X defined or used?" (know the name)
│  → find_symbol({name})
│
├─ "How does feature X work?" (vague concept)
│  → search({query, k:5}) → get_code on best hit
│
├─ "What files are in module X?"
│  → file_outline({file: "path/to/module"})
│
├─ "Read this function's source"
│  → get_code({nodeId})  (use nodeId from find_symbol/search/file_outline)
│
├─ "Who calls / imports this?"
│  → find_usages({nodeId})
│
├─ "What does this function depend on?"
│  → traverse({nodeId, direction:"outbound", hops:1})
│
├─ "What would break if I change X?"
│  → graph_schema → find_usages({nodeId, edgeTypes:["calls"]})
│
├─ "How does A connect to B?"
│  → shortest_path({sourceId, targetId})
│
└─ "Is the graph up to date?" (stale warning)
   → reindex
```

## Concrete ug Codebase Scenarios

### Scenario 1: "How does the indexer pipeline work?"

A new contributor asks how files get from disk to the knowledge graph.

```
# Step 1: get oriented to the indexing subsystem
search({ query: "main indexing pipeline entrypoint", k: 5 })

# Step 2: read the entrypoint function found above
get_code({ nodeId: "function:native/src/indexer/mod.rs:42:index_project" })

# Step 3: see what index_project depends on (what it calls + imports)
traverse({ nodeId: "function:native/src/indexer/mod.rs:42:index_project", direction: "outbound", hops: 1 })

# Step 4: if LanguageIndexer appears, drill into how language dispatch works
find_symbol({ name: "for_extension" })
get_code({ nodeId: "function:native/src/indexer/languages.rs:55:for_extension" })
```

### Scenario 2: "What would break if I rename LanguageIndexer?"

Before doing the rename, check every place that touches the trait.

```
# Step 1: find every implementer
find_usages({ nodeId: "trait:native/src/indexer/languages.rs:32:LanguageIndexer" })

# Step 2: also find direct references (imports, type annotations)
find_usages({ nodeId: "trait:native/src/indexer/languages.rs:32:LanguageIndexer", edgeTypes: ["references", "imports"] })

# Step 3: check the graph schema to make sure we're not missing edge types
graph_schema({})
```

### Scenario 3: "Add a new language indexer (e.g. Go)"

You need to understand the existing pattern before implementing.

```
# Step 1: orient — how are existing indexers structured?
file_outline({ file: "native/src/indexer/languages.rs" })

# Step 2: read an existing indexer as a template (e.g. Python, the simplest)
get_code({ file: "native/src/indexer/languages/python.rs" })

# Step 3: also read the Rust indexer (a compiled language, closer to Go)
get_code({ file: "native/src/indexer/languages/rust.rs" })

# Step 4: check where indexers are registered
find_usages({ nodeId: "trait:native/src/indexer/languages.rs:32:LanguageIndexer", edgeTypes: ["references"] })
```

### Scenario 4: "Where are all the tests and how do I run them?"

```
# Step 1: find test files in the native crate
file_outline({ file: "native/tests" })

# Step 2: read the main test file to understand patterns
get_code({ file: "native/tests/graph_test.rs" })

# Step 3: find CI test commands
search({ query: "test commands scripts", k: 3 })
```

### Scenario 5: "The MCP server is failing to start — trace the startup"

```
# Step 1: find the MCP server entrypoint
find_symbol({ name: "mcp" })
# or use a broader search
search({ query: "MCP server start initialization", k: 5 })

# Step 2: read the main function
get_code({ nodeId: "function:native/src/main.rs:10:main" })

# Step 3: see startup dependencies
traverse({ nodeId: "function:native/src/main.rs:10:main", direction: "outbound", hops: 2 })
```

### Scenario 6: "Find symbols that have no callers (potentially dead code)"

```
# Step 1: find a hotspot symbol
project_overview({})

# Step 2: check if anyone calls it
find_usages({ nodeId: "function:native/src/indexer/languages.rs:55:for_extension", edgeTypes: ["calls"] })

# Step 3: if no callers, verify with broader edge types
find_usages({ nodeId: "function:native/src/indexer/languages.rs:55:for_extension" })
```

### Scenario 7: "Trace a data flow — how does a Rust file get from disk to the graph?"

```
# Step 1: find the file walking / discovery code
search({ query: "file discovery walker finds files in directory", k: 5 })

# Step 2: find the indexing function that processes a single file
find_symbol({ name: "index_file" })
get_code({ nodeId: "function:native/src/indexer/mod.rs:120:index_file" })

# Step 3: trace what index_file calls
traverse({ nodeId: "function:native/src/indexer/mod.rs:120:index_file", direction: "outbound", hops: 1 })
```

### Scenario 8: "Check which files import a deprecated crate"

```
# Step 1: find all imports of the crate
search({ query: "use old_crate_name", k: 15 })

# Step 2: read each file that imports it to plan the migration
# (use batched get_code with the nodeIds from step 1)
```

## Tool Selection Rules

| When you... | Use this | Why |
|-------------|----------|-----|
| ...have nodeId already | `find_symbol({nodeId})` or `file_outline({nodeId})` | O(1) direct lookup — skips search |
| ...know the identifier(s) | `find_symbol({name})` (array for several) | Cheapest — no embedding, no DB |
| ...need file structure | `file_outline({file})` (array for several) | Single file scan, no DB |
| ...want to read source | `get_code` (array for several) | Direct file read |
| ...have a vague concept | `search` | Full GraphRAG — but limit `k: 5` first |
| ...have nodeId, want context | `traverse` with `hops: 1` | Cheap single-hop walk |
| ...want callers of symbol(s) | `find_usages` (array for several) | Pre-configured inbound walk |
| ...want to connect 2 symbols | `shortest_path` | Targeted path query |
| ...want to filter by edge/node type | `graph_schema` first | Absent types silently match nothing |
| ...get stale warnings | `reindex` | Refreshes the graph |
| ...get embed errors | `ping_embedder` | Diagnose connectivity |

## Anti-Patterns

- **Don't** `search` for a known name → use `find_symbol({name})` (50-100x cheaper tokens).
  Example: instead of `search("the for_extension function")`, do `find_symbol({name:"for_extension"})`.
- **Don't** `find_symbol({name})` when you have the nodeId → use `find_symbol({nodeId})` (O(1) vs O(n) scan).
  Example: you already have `function:native/src/main.rs:10:main` from a prior result — use that, don't re-search.
- **Don't** `search` for file structure → use `file_outline({file})`.
  Example: instead of `search("what functions are in languages.rs")`, do `file_outline({file:"native/src/indexer/languages.rs"})`.
- **Don't** make N separate calls for N related items → pass an array (up to 10) in ONE call.
  Example: instead of 3 `find_symbol` calls, do `find_symbol({name:["for_extension","LanguageIndexer","index_file"]})`.
- **Don't** pass `edgeTypes`/`nodeTypes` filters blind → `graph_schema` first.
  Example: if you filter by `edgeTypes:["inherits"]` but the graph only has `extends`, you get zero results with no error.
- **Don't** `traverse` at `hops: 3` by default → start at `hops: 1`, expand if needed.
- **Don't** `search` with `k: 20` on first try → start at `k: 5`, then expand with `get_code` on promising hits.
- **Don't** read code from search snippets alone → use `get_code` for full context.
  Example: search shows 20 lines of `for_extension` — still call `get_code` to see the whole function.
- **Don't** embed-health-check with `search` → use `ping_embedder`.
