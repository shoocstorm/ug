# MCP Server Setup Guide

UltraGraph-KB includes an MCP (Model Context Protocol) server that exposes GraphRAG retrieval tools to AI agents via stdio transport.

## What is MCP?

The Model Context Protocol (MCP) allows AI applications (like Claude Desktop, Cursor, and other AI agents) to connect to external data sources and tools. This MCP server provides five tools for querying your knowledge graph.

## Prerequisites

Before using the MCP server, ensure you have:

1. **Built the project:**
   ```bash
   npm run build
   ```

2. **Generated a knowledge graph and ingested it into OverGraph:**
   ```bash
   # Full pipeline (index + graph + visualization + ingest).
   # Output goes to ~/.ug/<project-name>/ by default.
   npm run gen -- -i ./src

   # Or step by step:
   npm run index -- -i ./src
   npm run graph -- -i ~/.ug/src/indexed-tree.json
   npm run ingest -- -i ~/.ug/src/graph.json -o ~/.ug/src/ugdb
   ```

3. **Embedding endpoint** (for `search_kb` / `semantic_search_kb`): not required by
   default — UltraGraph ships an in-process ONNX embedder (no external service).
   Set `UG_EMBED_BASE_URL` to opt into a remote OpenAI-compatible endpoint instead
   (e.g. `ollama serve` with `nomic-embed-text`).

## Configuration

The MCP server uses environment variables for configuration:

| Variable | Description | Default |
|----------|-------------|---------|
| `UG_PROJECT` | Project name under `~/.ug` — db is `~/.ug/<project>/ugdb`, repo root read from `project.json`. **Preferred.** | none |
| `UG_DB_PATH` | Explicit OverGraph directory (overrides `UG_PROJECT`) | `~/.ug/<cwd-basename>/ugdb` if it exists, else `./ugdb` |
| `UG_HOME` | Override the `~/.ug` root | `~/.ug` |
| `UG_REPO_ROOT` | Root directory for resolving file paths in snippets | `project.json`'s `repoRoot`, else cwd |
| `UG_EMBED_MODEL` | Override embedding model (local fastembed alias or remote model name) | built-in default |
| `UG_EMBED_BASE_URL` | Set to opt into the remote embedding backend | unset — uses the in-process ONNX embedder |
| `UG_EMBED_API_KEY` | Bearer token for the remote embedding endpoint | none |
| `UG_MODEL_CACHE` | Override the local ONNX model cache directory | platform cache dir |
| `UG_DEST` | Knowledge store to read from: `overgraph` (default) or `neo4j` | `overgraph` |

## Setting Up with AI Agents

### The easy way

```bash
node .ug/cli.mjs mcp install claude     # Claude Desktop
node .ug/cli.mjs mcp install cursor     # Cursor (.cursor/mcp.json in cwd)
node .ug/cli.mjs mcp install opencode   # opencode (opencode.json in cwd)
```

This writes (or merges into, preserving any other configured servers) the
target's config file with the correct absolute path to this `cli.mjs` and
`UG_PROJECT` set to the current directory's project name. Restart the app
afterward. For any other MCP client, or to configure things manually, see below.

### Claude Desktop (manual)

Edit your Claude Desktop configuration file:

**macOS:** `~/Library/Application Support/Claude/claude_desktop_config.json`
**Windows:** `%APPDATA%\Claude\claude_desktop_config.json`
**Linux:** `~/.config/Claude/claude_desktop_config.json`

Add the MCP server configuration:

```json
{
  "mcpServers": {
    "ultragraph": {
      "command": "node",
      "args": ["/absolute/path/to/ug/.ug/cli.mjs", "mcp"],
      "env": {
        "UG_DB_PATH": "/absolute/path/to/.ug/<project>/ugdb",
        "UG_REPO_ROOT": "/absolute/path/to/your/project",
        "UG_EMBED_BASE_URL": "http://localhost:11434/v1",
        "UG_EMBED_MODEL": "nomic-embed-text"
      }
    }
  }
}
```

**Important:** Use absolute paths, not relative paths.

### Cursor (manual)

Cursor supports MCP servers via its configuration. Create or edit `.cursor/mcp.json` in your project root:

```json
{
  "mcpServers": {
    "ultragraph": {
      "command": "node",
      "args": ["/absolute/path/to/ug/.ug/cli.mjs", "mcp"],
      "env": {
        "UG_DB_PATH": "/absolute/path/to/.ug/<project>/ugdb",
        "UG_REPO_ROOT": "/absolute/path/to/your/project"
      }
    }
  }
}
```

### Other MCP-Compatible Clients

For any MCP client that supports stdio transport, use:

```bash
# Command to start the server
node /path/to/ug/.ug/cli.mjs mcp

# With environment variables
UG_DB_PATH=/path/to/ug-db UG_EMBED_BASE_URL=http://localhost:11434/v1 node /path/to/ug/.ug/cli.mjs mcp
```

## Available Tools

### 1. `search_kb` - Primary Knowledge-Base Search

**PRIMARY KNOWLEDGE-BASE SEARCH** for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change.

Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via `traverse_kb` / `find_usages`.

**Trigger phrases:** "how does X work", "where is X", "what is X", "find/show me code for X", "explain X", "is there a function that...", "how is X implemented", "before I change X look up...", "context on X", or any question whose answer likely lives in the repo.

**Internals:** RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance. Pass `strategy='mmr'` for the legacy diversity-first BFS+MMR cascade.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | ✅ | Natural-language query. Be specific — name the concept, function, or behavior you're after. |
| `k` | integer (1-50) | ❌ | How many context items to return (default 8). Bump to 15-20 when surveying a subsystem. |
| `hops` | integer (0-5) | ❌ | MMR-only: graph expansion radius from each seed (default 2). Ignored under PPR. |
| `edgeTypes` | string[] | ❌ | Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. |
| `direction` | string | ❌ | Edge direction during the walk (default 'both'). Use 'inbound' for who depends on the seed; 'outbound' for what the seed depends on. |
| `maxChars` | integer (100-200000) | ❌ | Approximate character budget for assembled context (default ~16k). |
| `mmrLambda` | number (0-1) | ❌ | MMR balance (only when strategy='mmr'): 1 = max relevance, 0 = max diversity (default 0.6). |
| `whereClause` | string | ❌ | Optional SQL WHERE applied during seed search. Examples: `node_type = 'Function'`, `file LIKE 'src/auth/%'`. |
| `includeSnippets` | boolean | ❌ | Read source slice for each item (default true). Set false when you only need IDs and locations. |
| `strategy` | string | ❌ | Ranking strategy. 'ppr' (default) = Personalized PageRank seeded by RRF. 'mmr' = legacy seed+BFS+MMR. |
| `pprRestartProb` | number (0.01-0.99) | ❌ | PPR teleport probability (default 0.15). Higher = stay closer to seeds. |
| `pprMaxIter` | integer (1-200) | ❌ | PPR power-iteration cap (default 30). |
| `pprSeedPool` | integer (1-200) | ❌ | How many RRF hits feed the personalization vector (default 16). |
| `pprEdgeWeights` | object | ❌ | Override edge-type weights, e.g. `{ calls: 1.0, imports: 0.7 }`. |

**Example usage:**
```
search_kb: { query: "how authentication works in this codebase", k: 10 }

search_kb: { query: "where is the main entry point", k: 5, whereClause: "node_type = 'Function'" }

search_kb: { query: "payment processing logic", k: 15, strategy: "mmr", hops: 3 }

search_kb: { query: "error handling", k: 8, edgeTypes: ["calls", "references"], direction: "both" }

search_kb: { query: "database schema", k: 12, maxChars: 5000, pprRestartProb: 0.3 }
```

---

### 2. `semantic_search_kb` - Lightweight Vector Lookup

**Lightweight pure-vector lookup** over the knowledge base — no graph expansion, no snippet read, no PPR. Returns the top-k nearest nodes with id/name/type/file/lines/description/distance.

Use this when `search_kb` would be overkill:
- Quick disambiguation ("which node is the user talking about?")
- Candidate generation before a deeper `traverse_kb`
- Filtered lookups via `whereClause` (e.g. only Functions in a given folder)

Cheaper and faster than `search_kb`. Switch to `search_kb` when you need actual code snippets or graph-aware ranking.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | ✅ | Natural-language query. |
| `k` | integer (1-100) | ❌ | How many candidate nodes to return (default 10). |
| `whereClause` | string | ❌ | Optional SQL WHERE filter applied to the vector search. Examples: `node_type = 'Function'`, `file LIKE 'src/auth/%'`, `node_type IN ('Class','Interface')`. |

**Example usage:**
```
semantic_search_kb: { query: "auth middleware", k: 5, whereClause: "node_type = 'Function'" }

semantic_search_kb: { query: "User class", k: 3, whereClause: "node_type IN ('Class', 'Interface')" }

semantic_search_kb: { query: "database connection", k: 10, whereClause: "file LIKE 'src/db/%'" }

semantic_search_kb: { query: "API handler", k: 5 }
```

---

### 3. `traverse_kb` - Graph Traversal

**Walk the graph N hops** from given seed node ids. The natural follow-up to `search_kb` / `semantic_search_kb`: take a node id you got back, expand outward to see what it imports, calls, contains, or extends.

Use `'outbound'` to see what the seed depends on; `'inbound'` to see who depends on the seed. Output groups edges by type so the structure is easy to scan.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `startNodeIds` | string[] | ✅ | Seed node ids — typically copied from a prior search result. |
| `hops` | integer (1-5) | ❌ | Hop radius (default 2). Use 1 for direct neighbors only. |
| `edgeTypes` | string[] | ❌ | Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. |
| `direction` | string | ❌ | Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on. |

**Example usage:**
```
traverse_kb: { startNodeIds: ["func-123"], hops: 2, edgeTypes: ["calls", "imports"] }

traverse_kb: { startNodeIds: ["class-456"], hops: 1, direction: "outbound" }

traverse_kb: { startNodeIds: ["func-789", "class-101"], hops: 2, direction: "both" }

traverse_kb: { startNodeIds: ["file-202"], hops: 3, edgeTypes: ["contains", "imports"] }
```

---

### 4. `find_usages` - Find Inbound References

**Find inbound references** to a node — i.e. callers of a function, importers of a module, subclasses of a class, or anything else pointing at the node.

Convenience wrapper over `traverse_kb` with `direction='inbound'` and a sensible default edge-type set: `['calls', 'references', 'imports', 'extends', 'implements']`.

Use this when the user asks "who uses X", "what calls X", "where is X imported", "what would break if I change X", or before a refactor.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `nodeId` | string | ✅ | The node id to look up usages for. Get this from search_kb or semantic_search_kb. |
| `hops` | integer (1-3) | ❌ | How many hops out to walk (default 1 = direct callers only). Bump to 2 to catch transitive usages. |
| `edgeTypes` | string[] | ❌ | Override the default set if you only care about a subset (e.g. ['calls']). |

**Example usage:**
```
find_usages: { nodeId: "func-123", hops: 1 }

find_usages: { nodeId: "class-456", hops: 2 }

find_usages: { nodeId: "func-789", edgeTypes: ["calls"] }

find_usages: { nodeId: "file-101", hops: 1, edgeTypes: ["imports"] }
```

---

### 5. `ping_embedder` - Health Check

**Probe the configured embedding endpoint.** Returns 'ok' on success or throws with the upstream error.

Call this when `search_kb` / `semantic_search_kb` fails with an embedding-related error, or as a one-off health check before kicking off a batch of queries.

**Parameters:** None

**Example usage:**
```
ping_embedder: {}
```

**When to use:**
- Before running a batch of search queries
- After `search_kb` fails with an embedding-related error
- When troubleshooting "embedding endpoint unreachable" errors

## Sample Queries

Here are 20 common questions end users might ask when using the MCP tools. These examples demonstrate how to leverage `search_kb` and `traverse_kb` effectively.

### General Code Understanding
1. "How is authentication handled in this codebase?"
2. "What's the overall architecture of this project?"
3. "Explain how the caching layer works"
4. "What database models exist and how are they related?"

### Finding Specific Functions/Classes
5. "Where is the main entry point defined?"
6. "Find all functions that handle payment processing"
7. "Show me the error handling logic"
8. "Where is the configuration loaded from?"

### Understanding Relationships
9. "What does this function call and who calls it?"
10. "Show me the dependency graph for the API router"
11. "Which files import the auth module?"
12. "What's the call stack for the login function?"

### Debugging & Investigation
13. "Why is this API endpoint returning 500 errors?"
14. "Find all places where this exception is caught"
15. "What validation happens before saving to the database?"
16. "Trace the data flow from request to response"

### Feature Discovery
17. "How do I add a new API route?"
18. "What's the pattern for creating background jobs?"
19. "Where are the React components defined?"
20. "How are environment variables configured and used?"

### Example Tool Calls

```claude
search_kb: { query: "How is authentication handled in this codebase?", k: 10 }

semantic_search_kb: { query: "auth middleware", k: 5, whereClause: "node_type = 'Function'" }

traverse_kb: { startNodeIds: ["func-123"], hops: 2, edgeTypes: ["calls", "imports"] }

find_usages: { nodeId: "func-123", hops: 1 }

ping_embedder: {}
```

## Testing the MCP Server

You can test the MCP server manually using the MCP inspector or by running it directly:

```bash
# Set environment variables
export UG_DB_PATH=~/.ug/<project>/ugdb
export UG_EMBED_BASE_URL=http://localhost:11434/v1
export UG_EMBED_MODEL=nomic-embed-text

# Run the server (it speaks MCP protocol over stdio)
node node/cli.mjs mcp
```

For a more interactive test, use the [MCP Inspector](https://github.com/modelcontextprotocol/inspector):

```bash
npx @modelcontextprotocol/inspector node node/cli.mjs mcp
```

## Troubleshooting

**"Cannot find module 'ug.node'"**
- Run `npm run build` to build the native addon

**"Database not found" errors**
- Ensure `UG_DB_PATH` (or `UG_PROJECT`) points to a valid OverGraph directory
- Run `npm run gen` (or `npm run ingest`) to create the database

**"Embedding endpoint unreachable"**
- Only relevant if you opted into the remote backend via `UG_EMBED_BASE_URL`
- Verify that endpoint is running and `UG_EMBED_BASE_URL` is correct
- Use `ping_embedder` tool to test connectivity

**Tools not appearing in AI agent**
- Restart the AI agent application after configuring MCP
- Check the configuration file syntax (valid JSON)
- Use absolute paths in configuration
