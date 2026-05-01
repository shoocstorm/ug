# MCP Server Setup Guide

UltraGraph-KB includes an MCP (Model Context Protocol) server that exposes GraphRAG retrieval tools to AI agents via stdio transport.

## What is MCP?

The Model Context Protocol (MCP) allows AI applications (like Claude Desktop, Cursor, and other AI agents) to connect to external data sources and tools. This MCP server provides three tools for querying your knowledge graph.

## Prerequisites

Before using the MCP server, ensure you have:

1. **Built the native module:**
   ```bash
   npm run prebuild
   ```

2. **Generated a knowledge graph and ingested it into LanceDB:**
   ```bash
   # Full pipeline (index + graph + visualization + ingest)
   npm run gen -- ./src -o ug-out/
   
   # Or step by step:
   npm run index -- ./src -o ug-out/indexed-tree.json
   npm run graph -- ug-out/indexed-tree.json -o ug-out/graph.json
   npm run ingest -- ug-out/graph.json ug-out/ug-db
   ```

3. **Running embedding endpoint** (for `search_kb` tool):
   - Local: `ollama serve` (with models like `llama3`, `nomic-embed-text`)
   - Or a remote OpenAI-compatible API

## Configuration

The MCP server uses environment variables for configuration:

| Variable | Description | Default |
|----------|-------------|---------|
| `UG_DB_PATH` | Path to LanceDB directory | `./ug-out/ug-db` |
| `UG_REPO_ROOT` | Root directory for resolving file paths in snippets | Current working directory |
| `UG_EMBED_BASE_URL` | Override embedding endpoint base URL | None (uses built-in default) |
| `UG_EMBED_API_KEY` | Override embedding API key | None |
| `UG_EMBED_MODEL` | Override embedding model name | None (uses built-in default) |

## Setting Up with AI Agents

### Claude Desktop

Edit your Claude Desktop configuration file:

**macOS:** `~/Library/Application Support/Claude/claude_desktop_config.json`
**Windows:** `%APPDATA%\Claude\claude_desktop_config.json`
**Linux:** `~/.config/Claude/claude_desktop_config.json`

Add the MCP server configuration:

```json
{
  "mcpServers": {
    "ultragraph-kb": {
      "command": "node",
      "args": ["/absolute/path/to/ug/src/mcp-server.mjs"],
      "env": {
        "UG_DB_PATH": "/absolute/path/to/ug/ug-out/ug-db",
        "UG_REPO_ROOT": "/absolute/path/to/your/project",
        "UG_EMBED_BASE_URL": "http://localhost:11434/v1",
        "UG_EMBED_MODEL": "nomic-embed-text"
      }
    }
  }
}
```

**Important:** Use absolute paths, not relative paths.

### Cursor

Cursor supports MCP servers via its configuration. Create or edit `.cursor/mcp.json` in your project root:

```json
{
  "mcpServers": {
    "ultragraph-kb": {
      "command": "node",
      "args": ["/absolute/path/to/ug/src/mcp-server.mjs"],
      "env": {
        "UG_DB_PATH": "/absolute/path/to/ug/ug-out/ug-db",
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
node /path/to/ug/src/mcp-server.mjs

# With environment variables
UG_DB_PATH=/path/to/ug-db UG_EMBED_BASE_URL=http://localhost:11434/v1 node /path/to/ug/src/mcp-server.mjs
```

## Available Tools

### 1. `search_kb` - GraphRAG Retrieval

Performs hybrid (vector + FTS) search with graph expansion and MMR reranking.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | ✅ | Natural-language query |
| `k` | integer (1-50) | ❌ | Number of context items to return (default: 8) |
| `hops` | integer (0-5) | ❌ | Graph expansion radius from seeds (default: 2) |
| `edgeTypes` | string[] | ❌ | Restrict expansion to edge types (e.g., `["imports", "calls"]`) |
| `direction` | string | ❌ | Edge direction: `"outbound"`, `"inbound"`, or `"both"` (default: `"both"`) |
| `maxChars` | integer (100-200000) | ❌ | Approximate character budget for assembled context |
| `mmrLambda` | number (0-1) | ❌ | MMR balance: 1 = max relevance, 0 = max diversity (default: 0.6) |
| `whereClause` | string | ❌ | Optional SQL WHERE clause for seed search |
| `includeSnippets` | boolean | ❌ | Read source code snippets (default: true) |

**Example usage in Claude:**
```
Use search_kb to find how authentication works in this codebase.
```

### 2. `traverse_kb` - Graph Traversal

Walk the graph N hops from given seed node IDs.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `startNodeIds` | string[] | ✅ | Seed node IDs to start traversal from |
| `hops` | integer (1-5) | ❌ | Hop radius (default: 2) |
| `edgeTypes` | string[] | ❌ | Filter by edge types |
| `direction` | string | ❌ | `"outbound"`, `"inbound"`, or `"both"` (default: `"outbound"`) |

**Example usage in Claude:**
```
Traverse the graph starting from node "func-123" with 2 hops.
```

### 3. `ping_embedder` - Health Check

Probe the embedding endpoint to verify connectivity.

**Parameters:** None

**Example usage:**
```
Ping the embedding endpoint to check if it's running.
```

## Testing the MCP Server

You can test the MCP server manually using the MCP inspector or by running it directly:

```bash
# Set environment variables
export UG_DB_PATH=./ug-out/ug-db
export UG_EMBED_BASE_URL=http://localhost:11434/v1
export UG_EMBED_MODEL=nomic-embed-text

# Run the server (it speaks MCP protocol over stdio)
node src/mcp-server.mjs
```

For a more interactive test, use the [MCP Inspector](https://github.com/modelcontextprotocol/inspector):

```bash
npx @modelcontextprotocol/inspector node src/mcp-server.mjs
```

## Troubleshooting

**"Cannot find module 'ultragraph-kb.node'"**
- Run `npm run prebuild` to build the native module

**"Database not found" errors**
- Ensure `UG_DB_PATH` points to a valid LanceDB directory
- Run `npm run ingest` to create the database

**"Embedding endpoint unreachable"**
- Verify your embedding endpoint is running
- Check `UG_EMBED_BASE_URL` is correct
- Use `ping_embedder` tool to test connectivity

**Tools not appearing in AI agent**
- Restart the AI agent application after configuring MCP
- Check the configuration file syntax (valid JSON)
- Use absolute paths in configuration
