# MCP Server Setup Guide

UltraGraph-KB includes an MCP (Model Context Protocol) server that exposes GraphRAG retrieval tools to AI agents via stdio transport.

## What is MCP?

The Model Context Protocol (MCP) allows AI applications (like Claude Desktop, Cursor, and other AI agents) to connect to external data sources and tools. This MCP server provides tools for querying your knowledge graph.

## Knowledge Base Types

UltraGraph supports indexing two types of knowledge bases (or a mixed combination):

1. **Documentation KB:** Markdown files, PDFs, Word documents, Excel spreadsheets, PowerPoint presentations
2. **Code KB:** Source code repositories (TypeScript, JavaScript, Python, Java, Rust, etc.)
3. **Mixed KB:** Both documentation and code in the same knowledge base

The type is automatically detected during indexing based on the content composition:
- **Docs:** Mostly `Document` and `Concept` nodes from markdown/PDF/documentation files
- **Code:** Mostly `Function`, `Class`, `Interface`, `Variable`, and other code structure nodes
- **Mixed:** Significant presence of both documentation and code nodes

The UI displays the detected KB type on each project card in the Knowledge Base Manager.

## Prerequisites

Before using the MCP server, ensure you have:

1. **Built the project:**
   ```bash
   cd native && cargo build --release
   ```

2. **Generated a knowledge graph and ingested it into OverGraph:**
   ```bash
   # Full pipeline (index + graph + visualization + ingest).
   # Output goes to ~/.ug/<project-name>/ by default.
   ug gen -i ./src

   # Or step by step:
   ug index -i ./src
   ug graph -i ~/.ug/src/indexed-tree.json
   ug ingest -n src   # reads ~/.ug/src/graph.json → writes ~/.ug/src/ugdb
   ```

3. **Embedding endpoint** (for `search` / `semantic_search`): not required by
   default — UltraGraph ships an in-process ONNX embedder (no external service).
   Set `UG_EMBED_BASE_URL` to opt into a remote OpenAI-compatible endpoint instead
   (e.g. `ollama serve` with `nomic-embed-text`).

**Index filtering:** UltraGraph automatically excludes build artifacts during indexing. The following patterns are ignored by default:
- `*.min.js`, `*.min.mjs`, `*.min.css` (minified files)
- `*.bundle.js`, `*.bundle.mjs`, `*.bundle.css` (bundled files)
- `dist/` directories

You can add custom ignore patterns via the `UG_IGNORE` environment variable (comma-separated, gitignore-style):
```bash
export UG_IGNORE="vendor/,*.generated.ts,node_modules/"
ug gen -i ./myproject
```

## Configuration

The MCP server uses environment variables for configuration:

| Variable | Description | Default |
|----------|-------------|---------|
| `UG_PROJECT` | Project name under `~/.ug` — db is `~/.ug/<project>/ugdb`, repo root read from `project.json`. **Preferred.** | `~/.ug/<cwd-basename>` if it exists, else `./ugdb` |
| `UG_HOME` | Override the `~/.ug` root | `~/.ug` |
| `UG_REPO_ROOT` | Root directory for resolving file paths in snippets | `project.json`'s `repoRoot`, else cwd |
| `UG_EMBED_MODEL` | Override embedding model (local fastembed alias or remote model name) | built-in default |
| `UG_EMBED_BASE_URL` | Set to opt into the remote embedding backend | unset — uses the in-process ONNX embedder |
| `UG_EMBED_API_KEY` | Bearer token for the remote embedding endpoint | none |
| `UG_MODEL_CACHE` | Override the local ONNX model cache directory | platform cache dir |
| `UG_DEST` | Knowledge store to read from: `overgraph` (default) or `neo4j` | `overgraph` |

These can also be set in a `.env` file in the directory you launch the
server from — a real environment variable of the same name always wins
over `.env`. Run `ug doctor` any time to print the fully
resolved db path, repo root, embedder, and destination config, along
with which env var (if any) drove each value.

## Setting Up with AI Agents

### The easy way

An agent can reach ug through the **`ug` CLI** (an agent skill teaches it; this
is the recommended path) or through the **MCP server** (tool calls over the
protocol). `ug connect` asks which you want, or takes the choice up front:

```bash
ug connect claude --cli   # skill only — the agent runs `ug` itself (recommended)
ug connect claude --mcp   # MCP server entry only
ug connect claude --both  # both, and the agent picks per question
```

Installing both leaves the choice to the agent, which usually reaches for the
connected tools; picking one removes the other, so the agent has a single door
into the graph. In a non-interactive shell with no flag, both are installed —
what scripted installs have always done.

```bash
ug connect                # No agent named: pick from an interactive list
ug connect claude         # Claude Code (project .mcp.json or global ~/.claude.json)
ug connect claude-desk    # Claude Desktop (global only)
ug connect cursor         # Cursor (.cursor/mcp.json — project or ~/.cursor/mcp.json)
ug connect windsurf       # Windsurf (global: ~/.codeium/windsurf/mcp_config.json)
ug connect vscode         # VS Code (.vscode/mcp.json — project or user-profile mcp.json)
ug connect gemini         # Gemini CLI (.gemini/settings.json — project or global)
ug connect codex          # Codex CLI (global: ~/.codex/config.toml)
ug connect hermes         # Hermes Agent (global: ~/.hermes/config.yaml)
ug connect opencode       # opencode (opencode.json — project or ~/.config/opencode/)
```

(`ug mcp install` is the same command under its original name, and still works.
The config paths above are where `--mcp`/`--both` write the server entry.)

For targets that support both a **project** config (in the current directory,
this repo only) and a **global** config (in your home dir, all projects),
you're asked which one to write — or pass `--project` / `--global` to skip
the question (required in non-interactive shells).

The written entry launches `ug mcp` via the absolute path of the `ug` binary,
with `UG_PROJECT` set to the current directory's project name, and is merged
into the target's config file preserving any other configured servers. The
server is a self-contained native binary — no Node.js runtime required.
Restart the app afterward. For any other MCP client, or to configure things
manually, see below.

To remove it again, `ug disconnect <agent>` (e.g. `ug disconnect cursor`; also
spelled `ug mcp uninstall`) — this removes the agent skill and strips just the
`ultragraph` entry from every scope the target supports (narrow it with
`--project`/`--global`), leaving any other servers, comments, and formatting
untouched. If there's nothing to remove, it's a no-op.

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
      "command": "/absolute/path/to/ug",
      "args": ["mcp"],
      "env": {
        "UG_PROJECT": "<project>",
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
      "command": "/absolute/path/to/ug",
      "args": ["mcp"],
      "env": {
        "UG_PROJECT": "<project>",
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
ug mcp

# With environment variables
UG_PROJECT=<project> UG_EMBED_BASE_URL=http://localhost:11434/v1 ug mcp
```

## Wildcards

Everywhere a symbol or file is named — `find_symbols` (`name`, `nodeTypes`,
`filePrefix`), `file_outline` (`file`), and the `nodeId` of `get_code`,
`find_usages`, `traverse` and `shortest_path` — accepts the same shell-style
pattern. One matcher serves the MCP tools, the HTTP API and the CLI, so a
pattern behaves identically wherever you use it.

| Syntax | Matches |
|--------|---------|
| `*` | any run of characters (not `/` in a path) |
| `**` | any run of characters, `/` included (paths) |
| `?` | exactly one character |
| `[abc]` `[a-z]` | one character from the set or range |
| `[!ab]` | one character not in the set |
| `{a,b}` | either alternative, nestable |
| `\*` | a literal `*` |

Matching is case-insensitive and covers the **whole** name: `auth*` matches
`authorize` but not `reauthorize` — use `*auth*` for that. In paths, `*` stops
at `/` and `**/` crosses directories (matching zero of them too, so
`src/**/*.rs` finds `src/main.rs`).

**Why it matters for an agent:** one call replaces a loop. `find_usages` with
`validate_*` gives the blast radius of a whole family; `file_outline` with
`src/**/*.ts` surveys a subtree; `find_symbols` with `*` plus `filePrefix`
enumerates a directory. Where a pattern names more symbols than a tool will
expand (25 for the `nodeId` parameters), the result says so — a truncated
answer is never silent.

The `nodeId` parameters also accept a plain **symbol name**, so
`{"nodeId": "connect"}` works without a `find_symbols` round trip first.

---

## Available Tools

### 1. `search` - Primary Knowledge-Base Search

**PRIMARY KNOWLEDGE-BASE SEARCH** for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change.

Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via `traverse` / `find_usages`.

**Trigger phrases:** "how does X work", "where is X", "what is X", "find/show me code for X", "explain X", "is there a function that...", "how is X implemented", "before I change X look up...", "context on X", or any question whose answer likely lives in the repo.

**Internals:** RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance.

**Requires an embedder and a database ingested with vectors — and unlike the CLI,
this tool has no fallback.** The FTS half is a channel inside the fusion, not a
standalone mode: the query is embedded before either channel runs. `ug search` on
the command line degrades to a name-substring match when no embedder can be built;
the MCP tool returns an error instead, deliberately, so an agent is never handed
substring hits it would read as ranked GraphRAG results. When it errors, switch to
`find_symbols` / `file_outline` / `find_usages` / `traverse` / `analyze` — none of
those touch embeddings. See
[docs/EMBEDDING-BACKENDS.md](EMBEDDING-BACKENDS.md#running-without-an-embedder).

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `analyze` | string | ✅ | Natural-language query. Be specific — name the concept, function, or behavior you're after. |
| `k` | integer (1-50) | ❌ | How many context items to return (default 8). Bump to 15-20 when surveying a subsystem. |
| `edgeTypes` | string[] | ❌ | Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references, instantiates, uses, overrides. |
| `direction` | string | ❌ | Edge direction during the walk (default 'both'). Use 'inbound' for who depends on the seed; 'outbound' for what the seed depends on. |
| `maxChars` | integer (100-200000) | ❌ | Approximate character budget for assembled context (default ~16k). |
| `whereClause` | string | ❌ | Optional SQL WHERE applied during seed search. Examples: `node_type = 'Function'`, `file LIKE 'src/auth/%'`. |
| `includeSnippets` | boolean | ❌ | Read a source slice for each item (default false — returns lean ids+locations; set true when you want the code inline rather than a follow-up `get_code`). |

> **Ranking is not tunable from the tool.** `strategy`, `hops`, `mmrLambda`,
> `pprRestartProb`, `pprMaxIter`, `pprSeedPool` and `pprEdgeWeights` used to be
> listed here. They are operator knobs — a wrong value degrades results
> silently, and the defaults are what an agent wants. They still parse (via
> `ug search` and `ug mcp call search`) but no longer appear in `tools/list`.
>
> In particular `strategy: 'mmr'` is not a choice any more: MMR survives only
> as the automatic fallback for backends without native PPR (Neo4j without the
> GDS plugin), which `search_kb` selects on its own.

**Example usage:**
```
search: { query: "how authentication works in this codebase", k: 10 }

search: { query: "where is the main entry point", k: 5, whereClause: "node_type = 'Function'" }

search: { query: "error handling", k: 8, edgeTypes: ["calls", "references"], direction: "both" }

search: { query: "database schema", k: 12, maxChars: 5000 }
```

---

### 2. `semantic_search` - Lightweight Vector Lookup

**Lightweight pure-vector lookup** over the knowledge base — no graph expansion, no snippet read, no PPR. Returns the top-k nearest nodes with id/name/type/file/lines/description/distance.

Use this when `search` would be overkill:
- Quick disambiguation ("which node is the user talking about?")
- Candidate generation before a deeper `traverse`
- Filtered lookups via `whereClause` (e.g. only Functions in a given folder)

Cheaper and faster than `search`. Switch to `search` when you need actual code snippets or graph-aware ranking.

Same hard dependency as `search`: an embedder plus vectors in the database, with
no fallback at the MCP layer. If you only need to match a **name**, use
`find_symbols` — exact, wildcard-capable, and needs no embeddings at all.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `analyze` | string | ✅ | Natural-language query. |
| `k` | integer (1-100) | ❌ | How many candidate nodes to return (default 10). |
| `whereClause` | string | ❌ | Optional SQL WHERE filter applied to the vector search. Examples: `node_type = 'Function'`, `file LIKE 'src/auth/%'`, `node_type IN ('Class','Interface')`. |

**Example usage:**
```
semantic_search: { query: "auth middleware", k: 5, whereClause: "node_type = 'Function'" }

semantic_search: { query: "User class", k: 3, whereClause: "node_type IN ('Class', 'Interface')" }

semantic_search: { query: "database connection", k: 10, whereClause: "file LIKE 'src/db/%'" }

semantic_search: { query: "API handler", k: 5 }
```

---

### 3. `traverse` - Graph Traversal

**Walk the graph N hops** from given seed symbols. The natural follow-up to `search` / `semantic_search`: take a node id you got back, expand outward to see what it imports, calls, contains, or extends.

Use `'outbound'` to see what the seed depends on; `'inbound'` to see who depends on the seed. Output groups edges by type so the structure is easy to scan. Several seeds make **one merged walk**, so a pattern traces a whole family at once.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `nodeId` | string \| string[] | ✅ | Seed(s): a node id, an exact symbol **name**, or a **wildcard** (see [Wildcards](#wildcards)) — one value or an array of up to 10. (`startNodeIds` is the deprecated legacy name, still accepted.) |
| `hops` | integer (1-5) | ❌ | Hop radius (default 2). Use 1 for direct neighbors only. |
| `edgeTypes` | string[] | ❌ | Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references, instantiates, uses, overrides. See `graph_schema` for what this graph has. |
| `direction` | string | ❌ | Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on. |

**Example usage:**
```
traverse: { nodeId: "func-123", hops: 2, edgeTypes: ["calls", "imports"] }

traverse: { nodeId: "class-456", hops: 1, direction: "outbound" }

traverse: { nodeId: ["func-789", "class-101"], hops: 2, direction: "both" }

traverse: { nodeId: "run_serve", hops: 1 }                 // by name

traverse: { nodeId: "handle_*", direction: "inbound" }     // one merged walk

traverse: { nodeId: "file-202", hops: 3, edgeTypes: ["contains", "imports"] }
```

---

### 4. `find_usages` - Find Inbound References

**Find inbound references** to a node — i.e. callers of a function, importers of a module, subclasses of a class, or anything else pointing at the node.

Convenience wrapper over `traverse` with `direction='inbound'` and a sensible default edge-type set: `['calls', 'references', 'imports', 'extends', 'implements', 'overrides', 'instantiates', 'uses']`.

Use this when the user asks "who uses X", "what calls X", "where is X imported", "what would break if I change X", or before a refactor.

Each direct caller carries the lines that mention the symbol (`file:line` plus the text), read from the source captured at ingest time — so the evidence comes back whether or not the repo is on this machine.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `nodeId` | string \| string[] | ✅ | What to look up usages for: a node id, an exact symbol **name**, or a **wildcard** (see [Wildcards](#wildcards)). Array of up to 10 batches related checks into one call. |
| `hops` | integer (1-3) | ❌ | How many hops out to walk (default 1 = direct callers only). Bump to 2 to catch transitive usages. |
| `edgeTypes` | string[] | ❌ | Override the default set if you only care about a subset (e.g. ['calls']). |

**Example usage:**
```
find_usages: { nodeId: "func-123", hops: 1 }

find_usages: { nodeId: "connect" }                     // by name, no id lookup first

find_usages: { nodeId: "validate_*" }                  // blast radius of a whole family

find_usages: { nodeId: "class-456", hops: 2 }

find_usages: { nodeId: "*Repository", edgeTypes: ["implements"] }
```

---

### 5. `find_symbols` - Symbol Lookup by Name or Wildcard

**Name-based lookup, no embeddings.** Use instead of `search` whenever you know (part of) an identifier: a function/class the user named, a symbol from a stack trace, something you are about to edit. Three ways to ask, all case-insensitive:

1. **a fragment** — ranked exact > prefix > substring (`resolve` finds `resolveDbAndRoot`);
2. **a wildcard pattern** — matched against the whole name (`handle_*`, `*Controller`, `{get,set}_*`); see [Wildcards](#wildcards);
3. **a nodeId** — O(1), no search at all.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `nodeId` | string \| string[] | ❌* | Direct node id lookup — O(1) when you already have the id from a prior search. |
| `name` | string \| string[] | ❌* | Identifier, fragment or wildcard pattern. Array of up to 10 resolves several in one call. |
| `nodeTypes` | string[] | ❌ | Restrict to node types (wildcards allowed): Function, Class, Interface, Variable, File, Concept. |
| `filePrefix` | string | ❌ | Only symbols under this repo-relative path: a prefix (`src/auth/`) or a glob (`src/**/*.ts`). |
| `limit` | integer (1-100) | ❌ | Max hits per query (default 20). The result states the true total. |
| `includeDocs` | boolean | ❌ | Also scan docstring prose (matched anywhere, not whole-string). Docstring hits rank below every name hit. |

*One of `nodeId` or `name` is required.

```
find_symbols: { name: "install_config" }
find_symbols: { nodeId: "function:native/src/mcp/install.rs:412:install_config" }
find_symbols: { name: "config", nodeTypes: ["Class"], filePrefix: "native/src/" }
find_symbols: { name: "handle_*" }                              // every handler
find_symbols: { name: "*Controller", nodeTypes: ["Class"] }
find_symbols: { name: "*", filePrefix: "src/auth/**", limit: 100 }  // a whole subtree
```

---

### 6. `file_outline` - File Table of Contents

**Every indexed symbol in a file, in line order.** Call before opening or editing a file. Accepts a repo-relative path, a unique suffix (just the basename works if unambiguous; ambiguous suffixes return the candidate list), a File node id, or a **path glob** that outlines every file it matches — one call to survey a directory or a subtree instead of one call per file.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `nodeId` | string \| string[] | ❌* | Direct File node id lookup — O(1) when you already have the File node id. |
| `file` | string \| string[] | ❌* | Repo-relative path (`native/src/main.rs`), unique suffix (`main.rs`), File node id, or glob (`src/**/*.ts`). Array of up to 10 outlines several in one call. |
| `maxFiles` | integer (1-200) | ❌ | Files a single glob may outline (default 20). Beyond the cap the remaining paths are listed by name, so nothing is hidden. |

*One of `nodeId` or `file` is required.

```
file_outline: { file: "native/src/mcp/install.rs" }
file_outline: { nodeId: "file:native/src/mcp/install.rs" }
file_outline: { file: "native/src/storage/*.rs" }          // one directory
file_outline: { file: "src/**/*.{ts,tsx}", maxFiles: 40 }  // a whole subtree
file_outline: { file: "**/test_*.py" }                     // by naming convention
```

---

### 7. `get_code` - Read Full Source

**Read the full source for a symbol, or a file/line range.** The follow-up to every other tool: search previews truncate at ~1200 chars and traversals return no code — call this to see the real implementation. Works even when the client has no file access (e.g. Claude Desktop), and even when the repo itself is not on the machine: a symbol reads its captured span, and a file/line range is cut out of the file's whole-file capture. The working tree is only a fallback for what ingest did not capture; a slice whose file changed since indexing comes back flagged.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `nodeId` | string \| string[] | ❌* | What to read: a node id from any prior result, an exact symbol **name**, or a **wildcard**. Array of up to 10 reads several in one call (per-symbol `maxChars`). |
| `file` | string | ❌* | Repo-relative path (when no nodeId). |
| `startLine` / `endLine` | integer | ❌ | 1-based inclusive range (with `file`; defaults to whole file). |
| `range` | string | ❌ | The window as one value, in the same dialect `analyze` uses for rows: `"11-35"`, `"34-end"`, `"20"` (the first 20 lines). `startLine`/`endLine` win if you send both. |
| `maxChars` | integer | ❌ | Character cap per symbol (default 20000). |

*One of `nodeId` or `file` is required.

```
get_code: { nodeId: "function:native/src/mcp/install.rs:412:install_config" }
get_code: { nodeId: "install_config" }        // by name
get_code: { nodeId: "render_*" }              // every renderer in one call
get_code: { file: "native/src/serve.rs", startLine: 100, endLine: 180 }
get_code: { file: "native/src/serve.rs", range: "100-180" }   // same thing
get_code: { file: "native/src/serve.rs", range: "181-260" }   // page on, don't re-read
```

---

### 8. `project_overview` - Orientation

**One-call orientation:** repo root, node/edge counts by type, biggest files by symbol count, and the most depended-upon symbols (highest inbound degree, containment edges excluded). Call it first in a new session, or for "what is this project / how is it structured".

**Parameters:** None

```
project_overview: {}
```

---

### 9. `shortest_path` - How Are Two Symbols Connected?

**Shortest directed edge path between two symbols.** Answers "does A reach B", "how does the route reach the db call", "can editing A affect B". If no forward path exists the reverse direction is tried and labeled.

Each endpoint takes a node id, an exact symbol name, or a wildcard — but must resolve to **exactly one** node, since the answer differs per candidate. When it doesn't, the error lists the ids to choose from.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `sourceId` | string | ✅ | Start point: node id, exact name, or a wildcard matching one symbol. |
| `targetId` | string | ✅ | End point: node id, exact name, or a wildcard matching one symbol. |

```
shortest_path: { sourceId: "file:native/src/mcp/install.rs", targetId: "function:native/src/main.rs:2874:run_mcp" }
```

---

### 10. `analyze` - Whole-Repo Statistics

**Counts, groups, distributions and blast radius over the indexed graph.** This is the tool for any question of the form "how many", "what fraction", "which are the biggest / longest / most depended-upon", "what does nothing call", "which folders depend on which", "what breaks if I change this file".

The point is cost. "How many methods are longer than 50 lines?" answered by grep-and-read is ~500k tokens on a medium repo and impossible on a monorepo; answered by looping `file_outline` it is ~40k tokens and 80 round trips. `analyze` answers it in one call and about 100 tokens, because ingest already stored the facts a query engine can aggregate.

Two ways to call it:

- **`preset`** — a named question. The cheap path (~20 tokens). Run `graph_schema` or `ug analyze --list` for the current set.
- **`gql`** — a raw OverGraph GQL query (Cypher-shaped), when no preset fits.

Read-only by construction: mutation statements are rejected before any write staging, so a query cannot modify the index.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `preset` | string | — | Name of a built-in question. |
| `gql` | string | — | Raw GQL, when no preset fits. Mutually exclusive with `preset`. |
| `args` | object | — | Arguments for a preset, e.g. `{"target": "src/auth.ts"}`. An undeclared argument is an error, not an ignored key. |
| `limit` | integer | — | Rows to display (default 20). Shorthand for `range: "1-N"`. |
| `range` | string | — | Which window of rows to show: `"20"`, `"11-35"`, `"34-end"`. 1-based, inclusive at both ends. |

**Paging without re-reading.** `range` is a window over rows the query *already produced* — it changes nothing about what the engine computes, so every reported total stays the same whichever window you ask for. That is what makes it cheap: the expensive part of paging is transferring rows you already have, not recomputing them. Each answer states which rows it is showing and names the exact range to ask for next:

```
rows 11–35 of 122 · 864 graph matches before grouping
next: rerun with range "36-55"
```

A window past the end says how many rows there actually are, rather than reporting "no rows" — those are different situations and confusing them sends you off to debug a query that is working.

```
analyze: { preset: "long_functions", args: { min_loc: 100 } }
analyze: { preset: "impact", args: { target: "native/src/storage/store.rs" } }
analyze: { gql: "MATCH (n:Function) WHERE n.params > 6 RETURN n.folder AS f, count(*) AS c ORDER BY c DESC" }
```

**Queryable properties:** `node_type` · `name` · `file` · `folder` · `language` · `classification` · `loc` · `code_lines` · `comment_lines` · `doc_lines` · `params` · `max_nesting` · `members` · `has_doc` · `has_comments` · `is_test` · `in_degree` · `out_degree` · `qualified_name` · `route` · `annotations` · `start_line` · `end_line`.

Three pairs are easy to confuse, and picking the wrong one changes the answer:

| Use | Not | Because |
|---|---|---|
| `code_lines` | `loc` | `loc` is a *span* — it counts blanks and comments. On this repo the longest function is 582 lines by span and 446 by code, a 23% gap. |
| `has_comments` | `has_doc` | `has_doc` is a doc-comment flag only. Of 1597 functions here, 828 carry prose but just 499 have a doc comment — the other 329 are explained entirely in inline comments. |
| `is_test` | a path filter | `is_test` prefers the indexer's file classification and falls back to a path heuristic, so it catches test files that aren't named like one. |

`members` counts declared members and is only populated for languages whose class body encloses them (Java, Python, TypeScript). A Rust struct's methods live in a separate `impl` block, so Rust types carry no `members` at all — the coverage line will say so rather than ranking them all as memberless.

**Every answer states its coverage, and this is not a nicety.** Aggregating over a property no node carries returns `0` — not an error, not a warning from the engine. `MATCH (n:Function) WHERE n.comment_lines > 3 RETURN count(*)` answers `0` on an index that has never recorded comment lines, and "no functions have long comments" is a far worse outcome than a refusal. So `analyze` probes the properties each query reads and reports their denominators, flagging any that are entirely unpopulated as `NOT INDEXED`. Call `graph_schema` first to see them all.

Writing GQL by hand, three things to know:

- Booleans are stored as `0`/`1` integers so they can be summed — the documented fraction is `sum(n.has_doc)` over `count(*)`.
- Every variable-length path needs a finite bound (`*1..3`, never `*`), and an *unanchored* walk past 2 hops can exceed the traversal cap and error. Anchor one end (`WHERE t.file = $target`) or reduce the bound.
- An `EXISTS { … }` subquery needs its own `RETURN` clause inside it, and negated membership must be parenthesised: `NOT (x IN [...])`.

Also available on the CLI as `ug analyze` and over HTTP as `POST /api/tools/analyze` (with `GET /api/presets` for the registry).

---

### 11. `graph_schema` - Capability Manifest

**Everything you need before filtering or aggregating**, in one cheap call:

- node and edge types this graph actually contains, with counts and what each edge type connects (e.g. `Calls: Function→Function (305)`);
- the full edge-type vocabulary indexers can emit;
- the properties `analyze` can filter on, **each with how many nodes actually carry it**;
- every available `analyze` preset, with its arguments.

Both halves exist for the same reason: filtering on a node or edge type the graph doesn't contain silently returns nothing, and aggregating over a property nothing carries silently returns zero. Neither is an error, so this call is how you avoid both.

**Parameters:** None

```
graph_schema: {}
```

---

### 12. `list_projects` - Enumerate Indexed Projects

**List every indexed project under `~/.ug`** with name, repo root, and node/edge counts. One server instance can query all of them: every other tool accepts an optional `project: '<name>'` parameter to target another project instead of the one the server was started for.

**Parameters:** None (ignores `project`)

```
list_projects: {}
search: { query: "auth flow", project: "other-repo" }
```

---

### 13. `gen` - Refresh a Stale Index

**Regenerate the index → graph → embeddings pipeline** for the current (or given) project. Incremental: a blake3 content cache skips files that haven't changed, so re-indexing after a few edits is fast.

Call this **after you edit files and before you ask anything structural about them** — `find_usages`, `traverse`, `shortest_path` and `analyze` all answer from the index, so until you refresh, a blast radius describes the code as it was before your edit and looks exactly like a correct one. Git hooks cover commit boundaries; this covers the edit burst in between. Also call it when tool outputs carry a staleness warning (see below).

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `project` | string | ❌ | Project to re-index (default: the current one). |
| `files` | string[] | ❌ | Paths you just changed, repo-relative or absolute. Scopes the **report**, not the work — the refresh is incremental either way. A path outside the repo is an error rather than a silent skip. |

```
gen: {}
gen: { files: ["src/auth.ts", "src/session.ts"] }
```

With `files`, the result appends a per-file line giving the symbol count each path contributed — see [Index Freshness](#index-freshness--staleness-warnings) for why `0 symbols` is the line that matters. The CLI equivalent is `ug update <file>...`.

---

### 14. `ping_embedder` - Health Check

**Probe the configured embedding endpoint.** Returns 'ok' on success or throws with the upstream error.

Call this when `search` / `semantic_search` fails with an embedding-related error, or as a one-off health check before kicking off a batch of queries.

**Parameters:** None

**Example usage:**
```
ping_embedder: {}
```

**When to use:**
- Before running a batch of search queries
- After `search` fails with an embedding-related error
- When troubleshooting "embedding endpoint unreachable" errors

## Sample Queries

Here are 20 common questions end users might ask when using the MCP tools. These examples demonstrate how to leverage `search` and `traverse` effectively.

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
search: { query: "How is authentication handled in this codebase?", k: 10 }

semantic_search: { query: "auth middleware", k: 5, whereClause: "node_type = 'Function'" }

# Name search
find_symbols: { name: "authenticateUser" }
# Direct nodeId lookup (O(1) when you already have the id)
find_symbols: { nodeId: "function:src/auth.ts:42:authenticateUser" }

# File path lookup
file_outline: { file: "src/auth.ts" }
# Direct nodeId lookup for File nodes (O(1))
file_outline: { nodeId: "file:src/auth.ts" }

traverse: { nodeId: "func-123", hops: 2, edgeTypes: ["calls", "imports"] }

find_usages: { nodeId: "func-123", hops: 1 }

ping_embedder: {}
```

## Index Freshness / Staleness Warnings

Every tool output is stamped with a staleness note when the index no longer matches the repo:

```
⚠ Index may be stale: 3 changed, 1 deleted of 214 indexed files since the last index (index built 2 day(s) ago).
Drifted: src/auth.ts, src/session.ts, src/db/pool.ts, src/old.ts (deleted)
This answer describes the last index, not the current tree. Call the gen tool with files: [...] naming what you changed (fast), or with no arguments to refresh everything.
```

The note names the drifted paths, not just a count, because the count alone cannot answer the question that decides what to do next: *are these the files I just edited?* If they are, the answer above describes the previous version of your own work.

Staleness is computed by comparing `graph.json`'s mtime against the current mtimes of the indexed files (once per project per server process). When you see the warning, call the `gen` tool — it's incremental either way, so unchanged files are skipped.

Pass `files` to scope the **report** (not the work): `gen: { files: ["src/auth.ts", "src/session.ts"] }` re-runs the same incremental pipeline and then tells you how many symbols each path you named contributed —

```
  src/auth.ts: 14 symbol(s)
  src/session.ts: 6 symbol(s)
  src/handler.go: 0 symbols — extension not indexed, so this file is invisible to every structural tool
```

That last line is the point: a repo in a language `ug` has no grammar for still indexes its Markdown and still answers questions, so without a per-file report an unindexed file is indistinguishable from one with no callers. Paths may be repo-relative or absolute; anything outside the repo root is an error rather than a silent skip.

The CLI equivalent is `ug update <file>...`. Both re-resolve cross-file edges over the whole graph on each run, which is what keeps them correct. `ug get_code` reads the *live* working tree by default and flags drift from the index, so line numbers stay current immediately after an edit even before a re-index.

**On the CLI, the same warning goes to stderr** before every structural command's output — `find_usages`, `traverse`, `shortest_path`, `analyze`, `search` and the rest — naming the drifted files and suppressed by `--no-banner` / `UG_NO_BANNER=1` along with the scope banner. stdout stays clean, so `--json` and `-o` remain parseable.

The web UI's Knowledge Base Manager runs the same check automatically on startup and every 2 minutes, showing a **⚠ Stale — Re-index** badge on affected project cards; clicking it re-runs the generation pipeline for that project.

## Testing the MCP Server

### Quick one-shot testing with `ug mcp list` / `ug mcp call`

The fastest way to poke at any tool — no JSON-RPC framing, no client config:

```bash
# Show every tool with a one-line description
ug mcp list

# Invoke any tool one-shot with its arguments as a JSON string
ug mcp call find_symbols '{"name":"run_mcp"}'
ug mcp call file_outline '{"file":"chat.rs"}'
ug mcp call list_projects '{}'
ug mcp call search '{"query":"how does auth work","k":8}'
ug mcp call gen '{}'
ug mcp call gen '{"files":["src/auth.ts","src/session.ts"]}'   # refresh + per-file report
```

`ug mcp call` resolves the same project/env configuration as the stdio server (`UG_PROJECT`, `.env`, …), so what you see is exactly what an agent would get. Pass `"project":"<name>"` inside the JSON to target another indexed project.

### Running the stdio server directly

You can also test the MCP server using the MCP inspector or by running it directly:

```bash
# Set environment variables
export UG_PROJECT=<project>
export UG_EMBED_BASE_URL=http://localhost:11434/v1
export UG_EMBED_MODEL=nomic-embed-text

# Run the server (it speaks MCP protocol over stdio)
ug mcp
```

For a more interactive test, use the [MCP Inspector](https://github.com/modelcontextprotocol/inspector):

```bash
npx @modelcontextprotocol/inspector ug mcp
```

Or invoke a single tool without a client via `ug mcp call <tool> '<json>'`
(e.g. `ug mcp call find_symbols '{"name":"run_mcp"}'`), and list the tools
with `ug mcp list`.

## Troubleshooting

**"graph.json not found" errors**
- Run `ug gen` for the project first to build its graph.json + ugdb

**"Database not found" errors**
- Ensure `UG_PROJECT` names a project with a valid OverGraph directory under `~/.ug`
- Run `ug gen` (or `ug ingest`) to create the database
- Run `ug doctor` to see exactly which db path got resolved and why

**"Embedding endpoint unreachable"**
- Only relevant if you opted into the remote backend via `UG_EMBED_BASE_URL`
- Verify that endpoint is running and `UG_EMBED_BASE_URL` is correct
- Use `ping_embedder` tool to test connectivity

**Tools not appearing in AI agent**
- Restart the AI agent application after configuring MCP
- Check the configuration file syntax (valid JSON)
- Use absolute paths in configuration
