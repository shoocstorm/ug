---
name: ug-mcp
description: |
  UltraGraph MCP tools — efficient codebase/documentation KB search through a
  semantic knowledge graph. Use when the session has ultragraph MCP tools
  (search_kb, find_symbol, traverse_kb, etc.) connected.
---

# UltraGraph MCP Tool Guide

## Available Tools

| Tool | Cost | Batch? | What it does |
|------|------|--------|-------------|
| `project_overview` | cheap | — | One-shot repo orientation: node counts, biggest files, hotspot symbols |
| `graph_schema` | cheap | — | Node/edge types actually in this graph + full vocabulary. Check before filtering. |
| `find_symbol` | cheap | ✅ `name[]` | Exact/substring name lookup → nodeId(s). No DB query. |
| `file_outline` | cheap | ✅ `file[]` | List all symbols in a file (line order) → nodeIds |
| `get_code` | cheap | ✅ `nodeId[]` | Read full source for nodeId(s) or a file:line range |
| `list_projects` | cheap | — | List all indexed repos on this machine |
| `ping_embedder` | cheap | — | Embedding endpoint health check |
| `semantic_search_kb` | medium | — | Pure vector lookup → candidate nodeIds (no code snippets) |
| `traverse_kb` | medium | ✅ `startNodeIds[]` | N-hop graph walk from nodeId(s) — deps or dependents |
| `find_usages` | medium | ✅ `nodeId[]` | Callers/importers of symbol(s) (convenience wrapper over traverse_kb) |
| `shortest_path` | medium | — | Shortest directed path between two nodeIds |
| `search_kb` | expensive | — | Full GraphRAG: PPR-ranked snippets with code context |
| `reindex` | expensive | — | Re-run index→graph→embed pipeline |

**Cheap tools**: no DB round-trip or cheap name-only scan (Tier 1).  
**Medium tools**: single DB query (embedding or graph hop).  
**Expensive tools**: multi-pass (embed → rank → expand → snippet-read).

**Batch? column**: those parameters accept a single string OR an array of up
to 10. **Always batch related lookups into ONE call** — one batched call is
one round trip; N separate calls cost N context re-reads. A bad id in a batch
doesn't fail the call: it comes back as an inline `✗` section, the rest
return normally.

## Efficient Workflows

### 1. First time in a repo
```
project_overview: {}
  → identifies key files and hotspot symbols
file_outline: { file: ["src/main.rs", "src/serve.rs", "src/db.rs"] }
  → ONE call outlines all key files → nodeIds of important functions
get_code: { nodeId: ["id-1", "id-2", "id-3"] }
  → ONE call reads the most important few
```

### 2. You know the name(s) (function, class, file)
```
find_symbol: { name: ["authenticate", "validateToken", "refreshSession"] }
  → ONE call resolves every symbol you're about to work on → nodeIds
get_code: { nodeId: ["func-123", "func-456"] }
  → read the implementations together
find_usages: { nodeId: "func-123" }
  → who calls it (only if needed)
```

### 3. You know the concept but not the name
```
search_kb: { query: "how auth tokens are validated", k: 5 }
  → returns ranked snippets + nodeIds
get_code: { nodeId: "func-xyz" }
  → expand the best match
```

### 4. Debugging / tracing a call chain
```
find_symbol: { name: "loginHandler" }   → nodeId
find_usages: { nodeId: "handle-login" } → find callers
traverse_kb: { startNodeIds: ["handle-login"], direction: "outbound", edgeTypes: ["calls"] }
  → what does it call?
shortest_path: { sourceId: "route-login", targetId: "db-query" }
  → how does request flow through?
```

### 5. Pre-refactor impact check
```
graph_schema: {}
  → confirm which edge types this graph actually has (filtering on an
    absent type silently returns nothing)
find_usages: { nodeId: ["func-1", "func-2", "func-3"], edgeTypes: ["calls"] }
  → ONE call: everything that would break for every symbol the refactor touches
```

### 6. Lightweight disambiguation (before expensive search)
```
semantic_search_kb: { query: "auth middleware", k: 5, whereClause: "node_type = 'Function'" }
  → quick candidate list → pick the right nodeId → get_code
```

### 7. Multi-project queries
```
list_projects: {}
  → all indexed projects
search_kb: { query: "user schema", project: "other-repo" }
  → query across repos
```

## Tool Selection Rules

| When you... | Use this | Why |
|-------------|----------|-----|
| ...know the identifier(s) | `find_symbol` (array for several) | Cheapest — no embedding, no DB |
| ...need file structure | `file_outline` (array for several) | Single file scan, no DB |
| ...want to read source | `get_code` (array for several) | Direct file read |
| ...have a vague concept | `search_kb` | Full GraphRAG — but limit `k: 5` first |
| ...have nodeId, want context | `traverse_kb` with `hops: 1` | Cheap single-hop walk |
| ...want callers of symbol(s) | `find_usages` (array for several) | Pre-configured inbound walk |
| ...want to connect 2 symbols | `shortest_path` | Targeted path query |
| ...want to filter by edge/node type | `graph_schema` first | Absent types silently match nothing |
| ...get stale warnings | `reindex` | Refreshes the graph |
| ...get embed errors | `ping_embedder` | Diagnose connectivity |

## Anti-Patterns

- **Don't** `search_kb` for a known name → use `find_symbol` (50-100x cheaper tokens)
- **Don't** `search_kb` for file structure → use `file_outline`
- **Don't** make N separate `find_symbol`/`get_code`/`file_outline`/`find_usages`
  calls for N related items → pass an array (up to 10) in ONE call; each extra
  call re-reads the whole conversation context
- **Don't** pass `edgeTypes`/`nodeTypes` filters blind → `graph_schema` first;
  a type the graph doesn't contain returns nothing, which looks like "no usages"
- **Don't** `traverse_kb` at `hops: 3` by default → start at `hops: 1`
- **Don't** `search_kb` with `k: 20` on first try → start at `k: 5`, then expand
- **Don't** read code from search snippets alone → use `get_code` for full context
- **Don't** embed-health-check with `search_kb` → use `ping_embedder`
