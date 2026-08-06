# UltraGraph (ug) — Complete API & Architecture Reference

## 1. CLI Commands

`ug` uses a flat subcommand model (no clap derive — manual arg parsing). All commands listed below, with their flags and data dependencies.

### Legend

- **index.json** — `indexed-tree.json`, the output of `ug index` (list of `FileNode`s with parsed symbols)
- **graph.json** — the output of `ug graph` (list of `GraphNode`s + `GraphEdge`s)
- **ugdb/** — OverGraph embedded database directory (vector + adjacency store)
- **Neo4j** — remote Neo4j database (alternative backend)
- **config** — `~/.ug/config.json` (persisted preferences)
- **~/.ug/** — project home directory (`$UG_HOME`, defaults to `~/.ug`)
- **cache** — hash-based parse cache (`cache.json` + `indexed-tree.json` snapshot)

---

### 1.1 Pipeline Commands

| Command | Aliases | What it does | Data sources | Key flags |
|---------|---------|-------------|--------------|-----------|
| `ug gen` | — | **End-to-end pipeline**: index → graph → visualization → OverGraph ingest. The primary entry point. | Repo source → index.json → graph.json → ugdb/ | `-i <path>` input dir, `-o <dir>` output, `-n <name>` project name, `-c <dir>` cache, `--no-cache`, `--no-ingest`, `--no-prune`, `--serve`, `-d <dir>` db path, `--model`, `--base-url`, `--api-key`, `--embedding-dim` |
| `ug index` | — | Index a directory: parse source files into `FileNode`s with symbols, imports, exports. Writes `indexed-tree.json`. | Repo source → writes index.json | `-i <path>` input (default `.`), `-o <file>` output, `-n <name>`, `-c <dir>` cache |
| `ug graph` | — | Build graph from indexed tree: resolve cross-file imports, create `GraphData` with nodes + edges. Writes `graph.json`. | index.json → writes graph.json | `-i <file>` input index.json, `-o <file>` output graph.json, `-n <name>` |
| `ug ingest` | — | Embed graph nodes and write to one or more knowledge stores (OverGraph/Neo4j). Defaults resolve from active project (`ug active <name>`), else cwd basename; reads `~/.ug/<name>/graph.json`, writes `~/.ug/<name>/ugdb`. | graph.json → writes to ugdb/ or Neo4j | `-n <name>` project name, `-i <file>` input graph.json, `-o <dir>` output, `--dest <kind>` (overgraph\|neo4j, comma-separated), `--neo4j-*`, `--prune`, `--model`, `--base-url`, `--api-key`, `--embedding-dim` |

### 1.2 Graph Analysis Commands (graph.json-backed, offline, in-memory)

All accept: `-n <name>` (project), `-i <file>` (explicit graph.json), `--json` (raw JSON output), `-o <file>` (write JSON to file).

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug shortest_path` | — | Find shortest directed path between two symbols. | `<source>` `<target>` positionals, `--strict` (don't retry reverse direction) |
| `ug graph_centrality` | `centrality` | Rank nodes by degree & betweenness centrality. | `--top <n>` (default 20), `-t <type>` (repeatable), `-f <prefix>` |
| `ug graph_cycles` | `cycles` | Detect dependency cycles. | `-l <limit>` (default 20), `--min-len <n>`, `--max-len <n>`, `-f <prefix>`, `--fail-on-cycle` |

### 1.3 Agent Tools (graph.json-backed, same as MCP tools)

These accept the same params as their MCP counterparts and can output `--json`.

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug find_symbols` | — | Exact-name lookup (case-insensitive, ranked exact > prefix > substring). | `-n <name>`, `-t <type>` filter, `-f <file-prefix>`, `-l <limit>`, `--include-docs`, `--json`, `-o <file>` |
| `ug file_outline` | — | List indexed symbols in a file, in line order. | `<file>` positional, `-n <name>`, `--json` |
| `ug get_code` | — | Read source for a node id or file/line range. | `<node-id>` or `-f <file>`, `--start-line`, `--end-line`, `--max-chars`, `-n <name>` |
| `ug find_usages` | — | Find inbound references (callers/importers) to a symbol. | `<node-id>` positional(s), `--hops`, `--edge-type`, `-n <name>`, `--json` |
| `ug project_overview` | — | Orient in the codebase: stats, biggest files, most depended-upon symbols. | `-n <name>`, `--json` |
| `ug graph_schema` | — | Node & edge types with counts and connection info. | `-n <name>`, `--json` |

**Removed.** `graph_bfs`/`bfs`, `graph_filter`/`filter` and `graph_search` are gone, along with every pre-rename alias (`hybrid_search`, `search_kb`, `graph_path`, `path`, `list`, `find_symbol`, `reindex`, `update`, `centrality`, `cycles`, `code_query`). Each duplicated something else exactly, and duplicates drift — the two BFS commands had already diverged on whether a bare symbol name was accepted. Every command and tool now has exactly one name:

| Retired | Use instead |
|---------|-------------|
| `ug graph_bfs <name>` | `ug traverse <node-or-name>` — same graph.json walk, and it now resolves a bare name or file path too |
| `ug graph_filter` | `ug graph_schema` for the edge-type census (no database needed), or `ug query` for anything more |
| `ug graph_search <name>` | `ug find_symbols <name> --include-docs` — the flag was all `graph_search` set |

### 1.4 Retrieval Commands (ugdb/Neo4j-backed)

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug search` | — | **GraphRAG**: semantic search → graph expansion → PPR-ranked context with snippets. | `<query>` positional, `-k <limit>` (default 8), `--filter <sql>`, `--direction`, `-t <edge-type>`, `--max-chars`, `--no-snippets`, `-n <name>`, `--repo-root`, embedding overrides |
| `ug semantic_search` | — | Pure vector search over embeddings (no graph context). | `<query>` positional, `-k <limit>` (default 10), `--filter <sql>`, `-n <name>`, embedding overrides |
| `ug traverse` | — | K-hop BFS over the OverGraph edges table. | `<node-id>`... positionals, `-k <hops>` (default 2), `-n <name>` |
| `ug query` | — | **Whole-repo statistics**: counts, groups, distributions, blast radius. Read-only GQL over the stored facts. Needs the db but **no embedder**. | `<preset>` positional or `-p <preset>`, `-a k=v` (repeatable), `-g/--gql <query>`, `-k <limit>` (default 20), `-r/--range <window>` (`20` · `11-35` · `34-end`), `--list`, `-n <name>` |

### 1.5 Chat & Tour Commands

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug chat` | — | RAG-grounded chat: hybrid retrieval → LLM completion. One-shot (with prompt) or interactive REPL (no prompt). | `"<query>"` optional, `-k <limit>`, `--direction`, `-t <edge-type>`, `--filter`, `--max-chars`, `--no-snippets`, `--think`, `--no-tools`, `--max-tool-rounds`, `--chat-model`, `--chat-base-url`, `--chat-api-key`, `--temperature`, `--max-tokens`, `--chat-timeout`, `--system`, `--json`, `-v/--show-context`, embedding overrides, `-o <file>` |
| `ug tour` | — | Guided, narrated walkthrough — uses LLM to plan stops through the graph, flies the camera in the web UI. | `"<topic>"` optional, `-n <name>`, `--max-stops <n>` (default 8, max 25), `--no-llm`, `--chat-model`, `--chat-base-url`, `--chat-api-key`, `--temperature`, `--max-tokens`, tour-specific flags |

### 1.6 Project Management

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug serve` | (default) | Serve the visualization + REST API at `http://localhost:8080`. Runs by default with no args. | `-p <port>` (default 8080), `--host <ip>` (default 127.0.0.1), `--watch`, `--no-db`, `-i <graph.json>`, `-d <db>`, `--project <name>`, `--repo-root` |
| `ug app` | — | Open the native desktop shell (starts the server + a window). | Same as `ug serve` |
| `ug api` | — | List every HTTP endpoint `ug serve` exposes. | `--json` |
| `ug list` | `ls`, `list_projects` | List generated projects under `~/.ug` with stats. | — |
| `ug active` | — | View or set the active project (default for `ug mcp`). | Sets with `<name>` positional |
| `ug rm` | — | Delete a project's data directory. | `<name>` positional |
| `ug regen` | — | Re-run the pipeline for an existing project: reads `repoRoot` from its `project.json`, so no `-i` needed. Incremental. | `-n <name>`, plus every `ug gen` flag |
| `ug upgrade` | `update` | Check GitHub for a new release and self-update. | `--check` (report only, no update) |
| `ug uninstall` | — | Delete ALL indexed projects and uninstall ug itself. | — |
| `ug config` | — | View/persist defaults (chat model, endpoints, etc.) in `~/.ug/config.json`. | `set <key> <value>`, `get <key>`, `list` |
| `ug doctor` | — | Show resolved project/db/embedder/chat config with source for each value. | `-n <name>`, `-d <db>`, embedder and chat overrides |
| `ug mcp` | — | MCP server / install / uninstall (see §1.7). | |

### 1.7 MCP Subcommands

| Command | What it does | Key flags |
|---------|-------------|-----------|
| `ug mcp` | Run the stdio JSON-RPC MCP server (meant to be launched by an AI agent). | — |
| `ug connect <agent>` (alias `ug mcp install`) | Connect an AI agent: the CLI skill (`--cli`, recommended), the MCP server (`--mcp`), or both (`--both`); asks when not given. Agents: `claude`, `claude-desk`, `cursor`, `windsurf`, `vscode`, `gemini`, `codex`, `opencode`, `zed`, ... | `--project` (scope to this repo), `--global` (everywhere) |
| `ug disconnect <agent>` (alias `ug mcp uninstall`) | Remove the agent skill and the MCP server registration. | `--project`, `--global` |
| `ug mcp list` / `ls` | Print the tools this server advertises. | — |
| `ug mcp call <tool> <json>` | Invoke one tool directly from the command line. | `<tool>` name, `<json>` arguments |

### 1.8 Project Resolution & Active Project Fallback

Commands need to resolve a project name to read `graph.json` and/or write/load the OverGraph store (`ugdb`). The order varies by command group:

#### Resolution Order by Command Group

| Command Group | Resolution Order | Notes |
|--------------|------------------|------|
| `gen`, `index`, `graph` | `-n/--name` → derive from input path | Generate commands must use cwd when no `-n` (they create a new project) |
| `regen` | `-n` → **active project** → cwd basename | Rebuilds existing project; honors user's pinned active project |
| `ingest` | `-n` → **active project** → cwd basename | Reads graph.json and writes ugdb from the active project by default |
| Read commands<br/>(`semantic_search`, `search`, `traverse`, `chat`, `tour`, `query`, `graph_centrality`, `graph_cycles`) | `-n` → **active project** → cwd basename → most-recent | Now consistent with `regen`/`ingest` |
| `server`, `app`, `mcp` | `-n` → **active project** → cwd | Always opens the active project |
| Any command with `-i/--input` | `-i` wins | Bypasses all project logic |

#### Helper Functions

- `resolve_project_name(args, input)` → `-n` → `derive_project_name(input)` — for `gen`/`index`/`graph` (no active fallback)
- `resolve_active_project_name(args, input)` → `-n` → `get_active_project()` → `derive_project_name(input)` — for all other commands |

#### `default_read_db_path()` Fallback Chain

When no `-n/--name` flag is provided, the store/db path resolves through:

```
1. active project's ~/.ug/<active>/ugdb      ← honored first (UG_HOME/<name>/ugdb)
2. cwd-basename ugdb (if exists)
3. legacy ~/.ug/ugdb                          ← deprecated path
4. most-recently-updated project's ugdb       ← last resort
5. cwd-basename ugdb (always exists, may be empty)
```

This chain ensures that commands like `ug search"query"` or `ug chat"what is X"` run from an arbitrary directory will automatically pick the project the user previously pinned with `ug active <name>` rather than silently using the cwd or a random "most-recent" project.

---

## 2. HTTP API Routes

The HTTP server (`ug serve`) is built on **axum**. All routes listed below.

### 2.2 Graph Data & Health

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|-------------------|
| GET | `/graph.json` | Serve the full graph.json (gzip/brotli compressed) | graph.json on disk | No graph loaded |
| GET | `/healthz` | Health check — returns `"ok"` | — | — |

### 2.3 Project Management

| Method | Path | What it does | Data source |
|--------|------|-------------|--------------|
| GET | `/api/projects` | List all projects under ~/.ug with stats | `~/.ug/` directory scan |
| POST | `/api/projects/select` | Switch the active project at runtime | Body: `{ "project": "name" }` |
| POST | `/api/projects/delete` | Delete a project | Body: `{ "project": "name" }` |
| GET | `/api/projects/staleness` | Per-project staleness report (changed/deleted files vs graph.json mtime). Each entry has `isStale`, `changed`, `missing`, `files`, `kbKind`, `docNodes`, `codeNodes`, and `repoMissing` — when the repo root no longer exists the entry reports `repoMissing: true` with `isStale: false`, `kbKind: "unknown"` and zero counts rather than a misleading "N files deleted" (the index just freezes where it is). | graph.json mtime + file mtimes (skipped when the repo is gone) |

### 2.4 Generate/KB Manager

| Method | Path | What it does | Data source |
|--------|------|-------------|--------------|
| POST | `/api/generate` | Kick off `ug gen` as a background job. Body: `{ "inputDir", "projectName", "model", "baseUrl", "apiKey" }` | Spawns subprocess, streams log lines via SSE |
| GET | `/api/generate/status` | Check status of a gen job. Query: `?id=<uuid>` | In-memory `GenJobs` map |
| POST | `/api/ingest` | Re-embed an already-indexed project (the UI's "Ingest now" button). Body: `{ "name": "<project>" }` — defaults to the active project. Spawns `ug ingest`, poll with `/api/generate/status`. Reopens the active project's stores on success so the new vectors are live without a restart. A store that can't be opened (stale format or corrupt on-disk manifest) is wiped and rebuilt automatically. | Spawns subprocess |
| GET | `/api/browse-dir` | List files/dirs for the KB Manager file picker. Query: `?path=<path>` | Filesystem |

### 2.5 Graph API (graph.json-backed, read-only)

| Method | Path | What it does | Returns 503 when |
|--------|------|-------------|-------------------|
| GET | `/api/capabilities` | Server capabilities matrix: backends, models, features enabled | — |
| GET | `/api/config` | Read the active chat config (model, base_url, etc.) | No chat config |
| POST | `/api/config` | Update the persisted chat config | — |
| GET | `/api/graph/stats` | Node/edge counts, file stats | No graph |
| GET | `/api/graph/node/*id` | Get one node by id | No graph / not found |
| GET | `/api/graph/search` | Keyword search over node names (query params: `?q=`) | No graph |
| GET | `/api/graph/bfs/*id` | K-hop BFS from a node seed | No graph |
| GET | `/api/graph/path` | Shortest path between two nodes (query: `?source=&target=`) | No graph |
| GET | `/api/graph/filter` | Filter edges by type/endpoint | No graph |
| GET | `/api/graph/centrality` | Degree & betweenness centrality | No graph |
| GET | `/api/graph/cycles` | Detect dependency cycles | No graph |

### 2.6 Agent Tool API (same as MCP tools, over HTTP)

| Method | Path | What it does | Data source |
|--------|------|-------------|--------------|
| GET | `/api/tools` | List available agent tools with descriptions | MCP tool registry |
| GET | `/api/presets` | List `code_query` presets (name, category, description, params, source) | Preset registry |
| POST | `/api/tools/:tool` | Run one agent tool (same params as MCP). Accepts body JSON with optional `project` field. | graph.json |
| POST | `/api/tools/code_query` | Run a statistical query. Body: `{preset, args, gql, limit, range}`. Returns `columns` plus **only the requested window** of `rows`, with `from`/`to`/`rowsTotal` to page by, `rowsMatched`, `coverage`, `unindexed`, `warnings`, `truncated` and a rendered `text`. | ugdb (no embedder) |
| GET | `/api/presets` | Preset registry **plus** `properties` — the queryable property vocabulary, so the UI and the MCP capability manifest read the same list rather than each hardcoding one. | Preset registry |

### 2.7 File Content

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|------------------|
| GET | `/api/file` | Read source file content for the right-panel "Preview" tab. Query: `?file=<path>&startLine=&endLine=`. Reads the live file from disk first; when the repo path is gone (or the file was deleted) it falls back to the **indexed copy** captured in ugdb (`NodeRow.code`), so previews keep working with no repo on disk. The response includes `"source": "filesystem"\|"db"` and `"sliced"` (true when an exact symbol span was served, false when it fell back to the whole-file capture). | Filesystem, falling back to ugdb stored source | No project |

### 2.8 Database API (OverGraph/Neo4j-backed — Phase 3)

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|------------------|
| GET | `/api/db/node/*id` | Get a node by id from the knowledge store | ugdb or Neo4j | DB not opened |
| GET | `/api/db/traverse/*id` | K-hop BFS using the DB edges table | ugdb or Neo4j | DB not opened |
| POST | `/api/search/semantic` | Pure vector search — `{ "query", "k", "whereClause" }` | ugdb or Neo4j + embedder | DB or embedder unavailable |
| POST | `/api/search/hybrid` | GraphRAG search — `{ "query", "k", "hops", "edgeTypes", "direction", "maxChars", "mmrLambda", "whereClause", "includeSnippets", "strategy", ... }` | ugdb or Neo4j + embedder | DB or embedder unavailable |

### 2.9 Chat & Tour API

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|------------------|
| POST | `/api/chat` | RAG chat (non-streaming). Body: `{ "message", "history", "k", "direction", "edgeTypes", "repoRoot", "model", "baseUrl", "apiKey", "temperature", "maxTokens", "system", "tools", ... }` | DB + embedder + chat LLM | Chat endpoint not configured |
| POST | `/api/chat/stream` | RAG chat **streaming** (SSE). Same body as `/api/chat`. Streams `event: message` + `event: citations` + `event: done`. | DB + embedder + chat LLM | Chat endpoint not configured |
| GET | `/api/chat/config` | Get the server's default chat configuration | Config |
| POST | `/api/tour` | Guided tour (non-streaming). Body: `{ "topic", "projectName", "maxStops", "model", ... }` → plan → candidates → narration + links | DB + embedder + chat LLM | See tour opts |
| POST | `/api/tour/stream` | Guided tour **streaming** (SSE). Body same as `/api/tour`. Streams `event: stop` per stop as they arrive. | DB + embedder + chat LLM | — |

---

## 3. Agent / MCP Tools

### 3.1 Advertised MCP Tools (`tools/list`)

These 13 tools are advertised over MCP `tools/list` and also available via the CLI and HTTP `/api/tools/:tool`. Each tool accepts an optional `project` parameter (except `list_projects`).

| Tool | What it does | Data source | When it errors |
|------|-------------|-------------|----------------|
| `search` | **Primary KB search.** RRF (vector + FTS) → Personalized PageRank over edge graph → ranked context with snippets. | ugdb/Neo4j + embedder + graph.json | DB/embedder unavailable |
| `semantic_search` | Lightweight pure-vector lookup — no graph expansion, no snippets. Returns top-k nearest nodes with distance. | ugdb/Neo4j + embedder | DB/embedder unavailable |
| `traverse` | Walk graph N hops from seed node ids. Filters by edge type and direction. | graph.json (graph-backed, no DB needed) | graph.json missing/invalid |
| `find_usages` | Inbound references to a node (callers, importers, subclasses, etc.). Wrapper over traverse with direction=inbound + sensible defaults. Call-site lines come from each caller's stored source, with filesystem fallback. | graph.json (+ ugdb for call sites) | graph.json missing/invalid |
| `find_symbols` | Exact-name symbol lookup (case-insensitive, ranked exact > prefix > substring). Supports batch via array of names/ids. | graph.json | graph.json missing/invalid |
| `file_outline` | List every indexed symbol in one file, in line order. Accepts path or File node id. Supports batch via array. | graph.json | graph.json missing/invalid |
| `get_code` | Read source for a node id or file/line range. Works from stored source in DB (consistent with search) with filesystem fallback; a file/line range is cut out of the file's whole-file capture, so it needs no working tree either. | ugdb (preferred) + filesystem fallback | node/file captured in neither ugdb nor the working tree |
| `project_overview` | Orient in the codebase: repo root, node/edge counts, biggest files, most depended-upon symbols. | graph.json | graph.json missing/invalid |
| `shortest_path` | Find shortest directed edge path between two node ids. | graph.json | graph.json missing/invalid |
| `code_query` | **Whole-repo statistics**: counts, groups, distributions, blast radius. Takes a named `preset` or raw GQL. Read-only — mutations are rejected before write staging. Every answer reports property coverage, because aggregating over an unstored property returns `0` rather than an error. | ugdb (**no embedder**) | db missing or written by an older ug |
| `graph_schema` | **Capability manifest**: node & edge types with counts and connection shapes (from graph.json), plus queryable properties with live coverage and the `code_query` preset list (from the db). | graph.json + ugdb | graph.json missing/invalid (the db half degrades to a note) |
| `list_projects` | List every indexed project on this machine (name, repo path, graph size). | `~/.ug/` directory scan | — |
| `regen` | Re-run index → graph → embed pipeline — the whole of `ug gen`, which is why it is not called `reindex` (that names only the first stage). Incremental (content-hash cache). Graph tools refresh even if embedding fails. | Repo source → index.json → graph.json → ugdb/Neo4j | Repo root missing |

### 3.2 Unlisted Tools (hidden from agents, available via `ug mcp call`)

| Tool | What it does | Data source |
|------|-------------|-------------|
| `ping_embedder` | Ping the embedding endpoint to check connectivity | Embedder endpoint |

### 3.4 Chat Tool Denylist

`regen` and `list_projects` are excluded from the OpenAI-compatible tool schemas used by `ug chat` — the LLM should not be able to reindex or list projects mid-conversation.

### 3.5 OpenAI-compatible Tool Schemas

The `openai_tool_schemas()` function in `mcp/tools.rs` converts the MCP tool definitions into OpenAI `functions` format (with `parameters` instead of `inputSchema`, and with `project` removed). These are fed to the `/api/chat` LLM endpoint so the model can call tools like `find_symbols`, `get_code`, `search`, etc. mid-answer.

---

## 4. Storage Backends

### 4.1 Architecture

```
                    ┌─────────────────────────────────────┐
                    │         KnowledgeStore trait         │
                    │  (store.rs — pluggable abstraction)  │
                    └──────────┬──────────────────────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
     ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
     │  OverGraph   │  │    Neo4j     │  │   Future     │
     │  (db.rs)     │  │(backends/    │  │  backends    │
     │  embedded    │  │ neo4j.rs)    │  │              │
     │  engine      │  │ remote DB    │  │              │
     └──────────────┘  └──────────────┘  └──────────────┘
```

### 4.2 StoreSpec (destination selector)

Two variants, created from env vars or `--dest` flags:

| Variant | Connection | Environment variables |
|---------|-----------|----------------------|
| `StoreSpec::Overgraph { path, embedding_dim }` | Local embedded database engine | `UG_DEST=overgraph` (default) |
| `StoreSpec::Neo4j { uri, user, password, database, embedding_dim }` | Remote Neo4j via Bolt protocol | `UG_DEST=neo4j`, `UG_NEO4J_URI`, `UG_NEO4J_USER`, `UG_NEO4J_PASSWORD`, `UG_NEO4J_DATABASE` |

### 4.3 KnowledgeStore Trait

All backends implement this async trait:

| Method | What it does |
|--------|-------------|
| `embedding_dim()` | Return the vector dimension |
| `supports_native_ppr()` | Whether Personalized PageRank runs natively |
| `backend_name()` | `"overgraph"` or `"neo4j"` |
| `upsert_nodes(rows)` | Write/replace node rows (with embeddings) |
| `upsert_edges(rows)` | Write/replace edges |
| `vector_search(query, k, filter)` | Dense vector search (cosine distance) |
| `hybrid_search(query, sparse, text, k, filter)` | Dense + keyword fusion search |
| `traverse(start, max_hops, edge_types, direction)` | K-hop graph traversal |
| `execute_query(gql, params, limits)` | Run one **read-only** GQL statement for statistics. Default impl returns `Unsupported` — a backend without a query language must say so rather than answer approximately. Implemented on OverGraph; Neo4j inherits the default. |
| `ensure_query_indexes()` | Declare property indexes that make statistical queries cheap. Best-effort; default no-op. |
| `nodes_by_ids(ids)` | Bulk read nodes by id |
| `nodes_for_upsert(keys)` | Read back rows before upsert (for incremental ingest diffing) |
| `prune_nodes_absent_from(keep)` | Delete nodes not in the given set |
| `fetch_node(key)` | Get one node by id |
| `count_nodes()` | Total node count |
| `count_edges()` | Total edge count |
| `personalized_pagerank(seeds, direction, edge_types, ...)` | PPR computation |

### 4.4 OverGraph (db.rs)

The default embedded backend. Uses the `overgraph` crate's `DatabaseEngine`.

- **Storage location**: `<project-dir>/ugdb/` (alongside `graph.json`)
- **Vector index**: HNSW index over dense `f32` embeddings (cosine similarity)
- **Full-text index**: FNV-hashed sparse keyword vectors
- **Node keying**: Flat id space (`"function:src/main.rs:42:foo"`), mapped to internal `u64` ids
- **Edge storage**: Directed edges in adjacency table
- **Schema file**: `ug-meta.json` records the embedding dimension

### 4.5 Neo4j (backends/neo4j.rs)

An alternative remote backend. Connects via the Bolt protocol.

- **Connection**: env vars `UG_NEO4J_URI`, `UG_NEO4J_USER`, `UG_NEO4J_PASSWORD`, `UG_NEO4J_DATABASE`
- **Vector index**: Neo4j native vector index (label-based, cosine similarity)
- **Full-text**: Neo4j full-text search index
- **Node keying**: Label `GraphNode`, unique constraint on `id`
- **PPR**: Via the Graph Data Science (GDS) plugin (`gds.pageRank.stream`). Returns `Unsupported` when GDS is not installed.
- **Schema**: Auto-created on first connect (constraints, indexes)

### 4.6 StoreSet (fan-out)

When `--dest overgraph,neo4j` is specified, both backends are opened as a `StoreSet`. Writes fan out to all stores; reads pick exactly one store (the primary/first).

### 4.7 Other Storage Modules

| Module | Path | What it does |
|--------|------|-------------|
| `text.rs` | `storage/text.rs` | Builds the embedding text for each node (name + signature + docstring + related names + snippet) |
| `embed.rs` | `storage/embed.rs` | Remote embedder client (OpenAI-compatible `/v1/embeddings`), with auto-probe of dimension |
| `embed_local.rs` | `storage/embed_local.rs` | Local embedder via fastembed-rs (in-process, no external service) |
| `query.rs` | `storage/query.rs` | High-level search functions: `search_kb`, `semantic_search`, `semantic_search_w_where`, `rrf_search`, `mmr_rerank`, `traverse_filtered`, `traverse` |
| `ingest.rs` | `storage/ingest.rs` | Embedding + write pipeline: `ingest_graph`, `ingest_graph_multi`, `plan_incremental_ingest`, `prune_to_graph`, `reembed_nodes` |
| `ppr.rs` | `storage/ppr.rs` | Wrapper around `personalized_pagerank`: `run_ppr`, `default_edge_type_weights` |
| `source.rs` | `storage/source.rs` | Captures node source code from the filesystem for indexing in the DB |
| `types_registry.rs` | `storage/types_registry.rs` | Stable string ↔ u32 mapping for node types and edge types in OverGraph |
| `comments.rs` | `storage/comments.rs` | Extracts prose comments from source for embedding text |

---

## 5. Pipeline

### 5.1 Pipeline Flow

```
Source files
     │
     ▼
┌──────────┐    index.json (indexed-tree.json)
│   ug index│───→  { "files": [FileNode], "folders": [...],
│           │       "dependencies": [...], "stats": {...} }
└──────────┘
     │
     │  FileNode contains: path, hash, language, classification,
     │  symbols (id, name, kind, startLine, endLine, docstring,
     │  signature, imports, exports, extends, implements, calls, metrics)
     │
     ▼
┌──────────┐    graph.json
│  ug graph │───→  { "nodes": [GraphNode], "edges": [GraphEdge],
│           │       "stats": {...} }
└──────────┘
     │
     │  GraphNode contains: id, name, node_type, file, startLine, endLine,
     │  metrics, signature, docstring, imports, exports, extends,
     │  implements, calls, folder_meta
     │
     │  GraphEdge contains: source_id, target_id, edge_type
     │  Edge types: Calls, Extends, Implements, References, Contains,
     │              Imports, Exports, Requires, Uses, DependsOn
     │
     ▼
┌──────────┐    ugdb/ or Neo4j
│ ug ingest │───→  Nodes with dense embeddings + sparse keyword vectors
│           │      Edges in adjacency table
└──────────┘      NodeRow: { id, node_type, name, text, embedding,
                        file, start_line, end_line, code, file_hash,
                        description, docstring }
                  EdgeRow: { source, edge_type, target }
```

### 5.2 index.json Schema (`IndexResult`)

| Field | Type | Contents |
|-------|------|----------|
| `files` | `Vec<FileNode>` | One per source file. Contains all parsed symbols, imports, exports |
| `folders` | `Vec<FolderNode>` | File-system folder hierarchy with classification, README summaries |
| `dependencies` | `Vec<Dependency>` | From `package.json` (npm dependencies) |
| `stats` | `IndexStats` | totalFiles, cachedFiles, totalSymbols, totalFolders, totalLines, indexingTimeMs, lastIndexedAt, repoRoot |

### 5.3 graph.json Schema (`GraphData`)

| Field | Type | Contents |
|-------|------|----------|
| `nodes` | `Vec<GraphNode>` | All symbols + files + folders + dependency nodes. Id format: `{type}:{file}:{line}:{name}` e.g. `function:native/src/main.rs:35:main`, `file:native/src/serve.rs`, `folder:native/src/storage` |
| `edges` | `Vec<GraphEdge>` | Directed edges between nodes. Types: `Calls`, `Extends`, `Implements`, `References`, `Contains`, `Imports`, `Exports`, `Requires`, `Uses`, `DependsOn` |
| `stats` | `Option<IndexStats>` | Copied from index.json for self-contained graph |

### 5.4 Pipeline Details

1. **Index** (`indexer.rs`):
   - Discovers files with supported extensions via `scan_files()`
   - For each file: tree-sitter parse → symbol extraction → import/export resolution
   - Supported languages: Rust, TypeScript/TSX, JavaScript, Python, Java, Markdown, PDF, Word, Excel, PowerPoint
   - Cache: blake3 content hashes in `cache.json`; unchanged files reuse previous `FileNode`s
   - Output: `indexed-tree.json` (write-once, overwritten on re-index)

2. **Graph** (`graph.rs`):
   - Reads `IndexResult` (parsed in memory)
   - Pass 0: Creates folder nodes + folder hierarchy edges (`Contains`)
   - Pass 1: Creates file nodes + symbol nodes (Function, Class, Interface, Variable, Concept, Constant, Config, Dependency)
   - Resolves cross-file imports: joins the import path against source file's directory, tries extension permutations, falls back to basename matching
   - Creates edges: `Calls`, `Extends`, `Implements`, `References`, `Contains`, `Imports`, `Exports`, `Requires`, `Uses`, `DependsOn`
   - Output: `graph.json` (write-once, overwritten on re-graph)

3. **Ingest** (`storage/ingest.rs`):
   - Reads `GraphData`, builds embedding text for each node via `text::build_node_text()` (name + signature + docstring + related names + folder context + source snippet)
   - Generates sparse keyword vectors via `text.rs`
   - **Incremental**: diffs incoming vs stored nodes (by content hash), skips embedding for unchanged/reusable nodes
   - Embeds only new/changed nodes (in batches via the embedder)
   - Upserts nodes + edges to all configured backends
   - Optionally prunes stored nodes that no longer exist in the graph (`--prune`)
   - Output: embedded nodes and edges written to ugdb/ or Neo4j

### 5.5 Running the Pipeline

- **One-step**: `ug gen` runs index → graph → ingest sequentially
- **Step-by-step**: `ug index && ug graph && ug ingest`
- **With visualization**: `ug gen --serve` (or just `ug serve`)
- **MCP regen**: the `regen` MCP tool (formerly `reindex`) runs the full pipeline and reports results
