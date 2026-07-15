# UltraGraph: High-Performance Knowledge Graph & RAG Engine

A high-performance, local-first knowledge base engine that transforms codebases and documents into an interactive, visualized, and queryable **Semantic Knowledge Graph**. Built with Rust and Node.js for maximum speed and flexibility.

## Install
```bash
curl -fsSL https://ultra-graph.web.app/install.sh | sh
```

## Run
```
ug app
ug serve
```

## ⚡ Overview

- **UltraGraph Introduction**: [https://ultra-graph.web.app](https://ultra-graph.web.app)

### 🎬 Demo

[![UltraGraph demo](https://img.youtube.com/vi/3K-L7NSw9vs/maxresdefault.jpg)](https://youtu.be/3K-L7NSw9vs)

> Click the thumbnail to watch the walkthrough on YouTube.

UltraGraph implements a complete four-phase pipeline for building and querying advanced knowledge bases:

- **Phase 1: Turbo Indexing** — Native multi-threaded indexer that maps codebases in milliseconds using `tree-sitter`.
- **Phase 2: Graph Synthesis** — Builds a rich symbol graph with structural analysis (centrality, cycle detection, shortest paths).
- **Phase 3: OverGraph Storage** — Persistent vector + FTS storage with incremental ingestion and **in-process ONNX embedding** out of the box.
- **Phase 4: GraphRAG Search** — State-of-the-art retrieval using **Personalized PageRank (PPR)** to combine semantic relevance with structural importance.

---

## 🏗️ Architecture

[![UltraGraph Architecture](docs/UG-Architecture.png)](https://ultra-graph.web.app/architecture.html)

> Click the diagram for an interactive view at [ultra-graph.web.app/architecture.html](https://ultra-graph.web.app/architecture.html).

---

## ✨ Features

| Category | Feature | Status |
| :--- | :--- | :--- |
| **Indexing** | Parallel multi-core file crawling (`.gitignore` aware) | ✅ |
| | Languages: **TypeScript, JavaScript, Python, Java, Rust, Markdown, PDF** | ✅ |
| | Incremental indexing with `blake3` hashing | ✅ |
| **Graph** | Folder hierarchy extraction & classification | ✅ |
| | Symbol extraction (Functions, Classes, Interfaces, Imports, Calls) | ✅ |
| | K-hop BFS, Shortest Path, Centrality, Cycle Detection | ✅ |
| **Storage** | **OverGraph**: Hybrid Vector + FTS storage | ✅ |
| | **Local ONNX embedding** via `fastembed-rs` (in-process, no service needed) | ✅ |
| | Optional remote OpenAI-compatible `/v1/embeddings` backend | ✅ |
| | Auto-probed embedding dim; persisted to `<db>/ug-meta.json` | ✅ |
| **Retrieval** | **GraphRAG**: Personalized PageRank (PPR) & MMR strategies | ✅ |
| | RRF (Reciprocal Rank Fusion) for hybrid search | ✅ |
| **Chat** | **`ug chat`**: RAG-grounded chat against any OpenAI-compatible LLM | ✅ |
| | One-shot + interactive REPL; per-turn citations; `--json` output | ✅ |
| | **`POST /api/chat`** in `ug serve` powers the web chat panel | ✅ |
| **Interface** | **Web UI**: Premium D3.js force-directed visualization | ✅ |
| | **Web Chat panel**: drop-in UI over `/api/chat` with citation jumps | ✅ |
| | **Desktop App**: `ug app` — native window (Tauri) over the same UI, no browser tab needed | ✅ |
| | **MCP Server**: Stdio-based server for LLM integration | ✅ |
| | **CLI**: Comprehensive toolkit for all phases | ✅ |

---

## 🧭 Ways to Use UltraGraph

**The standalone binary is the default path.** No Node.js required to run it:

```bash
ug gen        # index → graph → ingest this repo (→ ~/.ug/<name>/)
ug serve      # or run bare `ug`, no arguments, opens the server directly:
              # visualization + REST API at http://localhost:8080
```

That's it for the common case — `ug -h` lists every other command
(`chat`, `list`, `doctor`, individual pipeline stages, etc).

<details>
<summary><b>Advanced / other integration modes</b> — Node CLI, MCP server, embedding the native addon</summary>

| # | Way | How | Notes |
| :--- | :--- | :--- | :--- |
| 1 | **Node CLI** | `node .ug/cli.mjs gen`, `node .ug/cli.mjs list` | Same core pipeline via the JS wrapper. No `serve`/`chat` — those are Rust-binary-only. |
| 2 | **MCP server** | `node .ug/cli.mjs mcp install claude` | Node-only — there's no separate Rust MCP mode. See [MCP Server](#-mcp-server). |
| 3 | **Embed in your own Node/TS app** | `require('.ug/ug.node')` | Call the native functions directly, no CLI/subprocess. See [Native API Usage](#-native-api-usage). |

### Node CLI
```bash
node .ug/cli.mjs gen
node .ug/cli.mjs list
```

### MCP server
```bash
node .ug/cli.mjs mcp install claude   # or: claude-code, cursor, windsurf, vscode, gemini, codex, hermes, opencode
# Writes/merges the ultragraph entry into the target's own MCP config
# for you — no hand-edited absolute-path JSON needed.
```

### Embed the native addon
```js
const ug = require('/path/to/ug/.ug/ug.node'); // types: .ug/index.d.ts

const symbols = ug.index('./src');
const graph = ug.buildGraph(symbols);
const context = await ug.dbHybridSearch('./ugdb', JSON.stringify({
  query: 'how does authentication work?',
  k: 8,
}));
```
Prefer not to link the addon? Spawn the `ug` binary instead (`child_process`)
and parse its JSON output, or run `ug serve` and call its REST API.

</details>

---

## 🚀 Quick Start

### 1. Install

The one-liner above installs `ug` (+ the native addon and Node CLI it
ships alongside) to `~/.local/share/ultragraph/.ug/` and symlinks `ug`
onto `~/.local/bin`. Windows: download `ultragraph-windows-x64.zip`
from [Releases](https://github.com/shoocstorm/ug/releases/latest).
Building from source instead needs **Rust** (latest stable) and
**Node.js** 20+: `npm run build`.

No external embedding service is required either way — UltraGraph
ships an in-process **ONNX embedder** ([`fastembed-rs`](https://github.com/Anush008/fastembed-rs)).
Model weights download once on first use (~22–130 MB) and cache
locally. Pass `--base-url` to route to a remote OpenAI-compatible
endpoint instead.

### 2. Generate Your First Graph
The `gen` command runs the entire pipeline (index → graph → ingest → UI).
```bash
# Run the full pipeline on the current directory.
# Output goes to ~/.ug/<project-name>/ (project name = the directory's
# basename; override with -n/--name).
ug gen -i ./ --no-ingest                        # prebuilt install
npm run gen -- -i ./ --no-ingest                # built from source

# Index another repo under a custom project name
ug gen -i ~/code/other-repo -n other --no-ingest
```

### 3. Visualize
Open the interactive visualization in your browser:
```bash
ug             # bare `ug` — same as `ug serve` (prebuilt install)
ug serve       # equivalent, explicit form
npm start      # built from source
# Visit http://localhost:8080
```
Without `-i`, `ug serve` (and bare `ug`) runs in **multi-project mode**: it
discovers every project under `~/.ug` and the UI header gets a project
switcher, so you can flip between repos without restarting. With zero
projects generated yet, it shows the KB Manager wizard instead of erroring —
so `ug` alone is a safe first thing to run in any repo.

### Data layout

All generated data lives in one folder per project under `~/.ug`
(override the root with `UG_HOME`):

```
~/.ug/<project-name>/
├── graph.json          # the knowledge graph
├── indexed-tree.json   # raw symbol tree
├── ugdb/               # OverGraph vector + edge store
├── project.json        # name, repoRoot, node/edge counts, timestamps
└── README.md
```

`ug list` shows every project with counts and last-generated times;
`ug rm <project>` deletes one (prompts for confirmation unless
`-f/--force`/`-y/--yes` is given). The repo-local `.ug/` folder only
holds build artifacts (`ug` binary, `ug.node`), not data.

---

## 🛠️ Command Line Interface

The native `ug` binary is the primary CLI (the Node wrapper exposes the
same pipeline stages under `node node/cli.mjs <command>`, see
[Node CLI](#node-cli) above). `ug -h` lists every command; `ug <command>
-h` prints that command's full flags and examples.

| Command | Description |
| :--- | :--- |
| `ug gen` | Full pipeline: index → graph → visualization → OverGraph ingest |
| `ug serve` | Serve the visualization + REST API (multi-project by default) |
| `ug app` | Open the native desktop shell (Tauri) — starts `ug serve` and a window pointed at it |
| `ug index` / `ug graph` / `ug ingest` | The individual pipeline stages `gen` runs for you |
| `ug hybrid_search "<query>"` | GraphRAG: semantic search → graph expansion → ranked context |
| `ug semantic_search "<query>"` | Plain vector search, no graph expansion |
| `ug traverse <node-id>` | K-hop BFS over the stored OverGraph edges |
| `ug chat "<question>"` | RAG-grounded chat against an LLM, one-shot or REPL — see [RAG Chat](#-rag-chat-ug-chat) |
| `ug list` / `ug rm <project>` | List projects under `~/.ug`, or delete one |
| `ug doctor` | Print resolved project/db/embedder/chat config and where each value came from |
| `ug mcp install claude` | Wire the MCP server into a client's config (see [MCP Server](#-mcp-server)) |
| `ug uninstall` | Delete ALL indexed projects and the standalone install itself (prebuilt installs only) |

Every command that selects a project takes `-n/--name <project>`
(default: current directory's basename, else the most recently
generated project under `~/.ug`). Destructive commands (`rm`,
`uninstall`) prompt for confirmation unless `-f/--force`/`-y/--yes` is given.

### `ug doctor`

Config resolution has several fallback tiers (flag → env var → default,
plus project/db path lookup through `~/.ug`). `ug doctor` (or
`node node/cli.mjs doctor` for the MCP-server side) prints exactly what
got resolved and why, instead of you having to trace it through env vars:

```
$ ug doctor
Project
  UG_HOME:      /Users/you/.ug  [default: ~/.ug]
  project name: my-repo  [derived from cwd basename]
  project dir:  /Users/you/.ug/my-repo (exists)
  db path:      /Users/you/.ug/my-repo/ugdb (exists)  [default: ...]

Embeddings (ingest / gen / semantic_search / hybrid_search / serve)
  backend:      local (in-process ONNX)  [default]
  model:        BAAI/bge-small-en-v1.5  [default]
  ...

Chat (ug chat / POST /api/chat)
  status:       not configured — using sample defaults; run `ug config set chat.base_url <url>` ...
```

---

## ⚙️ Configuration

Repeating `--base-url`/`--api-key`/`--model`/`--chat-model` on every
invocation gets old fast. Persist your defaults once with `ug config`:

```bash
ug config set chat.model gpt-4o-mini
ug config set chat.base_url https://api.openai.com/v1
ug config set chat.api_key sk-...
ug config set embed.model text-embedding-3-small

ug config list              # every key, its saved value, and what can override it
ug config get chat.model
ug config unset chat.model
ug config path              # → ~/.ug/config.json (or $UG_HOME/config.json)
```

Values land in `$UG_HOME/config.json` (default `~/.ug/config.json`,
written with owner-only permissions since it may hold API keys) and are
picked up by every command — `ug chat`, `ug serve`'s `/api/chat`, the
embedder, and the npm MCP server.

Prefer clicking? The visualization has the same settings behind the
**⚙ gear** (top-right of the Knowledge Base Manager, and in the sidebar
header once a graph is open). It reads/writes the same file via
`GET`/`POST /api/config`, shows which tier (flag / env / saved /
default) currently wins for each key, and chat changes apply to the
running server immediately — no restart.

Known keys: `chat.model`, `chat.base_url`, `chat.api_key`,
`chat.temperature`, `chat.max_tokens`, `chat.timeout_secs`,
`embed.model`, `embed.base_url`, `embed.api_key`, `embed.dim`.

Precedence is always **CLI flag > env var > `ug config` > built-in
default**. An explicit flag or env var still wins over a saved value —
but never silently: the CLI prints a one-line notice when that happens,
e.g.

```
▸ note: CLI flag --chat-model overrides saved config chat.model = gpt-4o-mini (~/.ug/config.json)
```

### `.env` files

UltraGraph also loads a `.env` file from the current directory (both the
`ug` binary and `node cli.mjs` do this) for per-repo env-var defaults:

```bash
# .env in your repo root
UG_EMBED_BASE_URL=https://api.openai.com/v1
UG_EMBED_API_KEY=sk-...
UG_EMBED_MODEL=text-embedding-3-small
UG_CHAT_MODEL=gpt-4o-mini
```

A real env var of the same name still wins over `.env`, and both count
as the "env var" tier — above `ug config`, below CLI flags.

| Env var | Overrides |
| :--- | :--- |
| `UG_HOME` | Root of the `~/.ug` project data directory |
| `UG_DB_PATH` | OverGraph directory (MCP server; overrides project lookup) |
| `UG_PROJECT` | Project name under `~/.ug` (MCP server) |
| `UG_REPO_ROOT` | Repo root used to resolve snippet file paths |
| `UG_EMBED_BASE_URL` / `UG_EMBED_API_KEY` / `UG_EMBED_MODEL` | `--base-url` / `--api-key` / `--model` (embeddings) |
| `UG_CHAT_BASE_URL` / `UG_CHAT_API_KEY` / `UG_CHAT_MODEL` | `--chat-base-url` / `--chat-api-key` / `--chat-model` (`ug chat`) |
| `UG_MODEL_CACHE` | Local ONNX model cache directory |

Run `ug doctor` (or `node node/cli.mjs doctor`) any time to see which
tier actually won for each setting — see [`ug doctor`](#ug-doctor) above.

---

## 🧠 Embeddings

UltraGraph picks a backend based on a single flag: **omit `--base-url` for the local in-process embedder (default), or pass `--base-url` to use a remote OpenAI-compatible endpoint.** The same flags apply to `ingest`, `gen`, `semantic_search`, and `hybrid_search`. `--base-url`/`--api-key`/`--model` fall back to `$UG_EMBED_BASE_URL`/`$UG_EMBED_API_KEY`/`$UG_EMBED_MODEL` when omitted.

### Local backend (default) — in-process ONNX via `fastembed-rs`

No daemon, no Docker, no network. The first call downloads the ONNX weights into a user cache directory; every subsequent run loads from disk. Inference runs on CPU through the ORT runtime and is dispatched onto a blocking pool so it doesn't stall the async runtime.

```bash
# Default — bge-small-en-v1.5, 384-dim, ~130 MB on first run.
# Reads ~/.ug/<cwd-basename>/graph.json and writes its ugdb sibling.
ug ingest

# Pick a different model by alias
ug ingest --model nomic-embed-text-v1.5     # 768-dim, long-context
ug ingest --model jina-embeddings-v2-base-code   # 768-dim, code-aware
ug ingest --model mxbai-embed-large-v1      # 1024-dim, top-tier quality
```

**Supported aliases** (case-insensitive; vendor prefix optional):

| Alias | Dim | Notes |
| :--- | :--- | :--- |
| `bge-small-en-v1.5` *(default)* | 384 | Smallest viable, fastest |
| `bge-base-en-v1.5` | 768 | Balanced |
| `bge-large-en-v1.5` | 1024 | Highest quality in BGE family |
| `all-MiniLM-L6-v2` / `all-MiniLM-L12-v2` | 384 | Sentence-Transformers classics |
| `nomic-embed-text-v1.5` | 768 | Strong on long context |
| `multilingual-e5-small` / `base` / `large` | 384 / 768 / 1024 | Multilingual |
| `bge-small-zh-v1.5` | 512 | Chinese-heavy docs |
| `jina-embeddings-v2-base-code` | 768 | Code-aware |
| `mxbai-embed-large-v1` | 1024 | Top-tier quality |

**Model cache:** `$UG_MODEL_CACHE` → `$XDG_CACHE_HOME/ug/models` → platform default (`~/Library/Caches/ug/models` macOS, `~/.cache/ug/models` Linux, `%LOCALAPPDATA%\ug\models` Windows).

### Remote backend (opt-in) — OpenAI-compatible `/v1/embeddings`

Passing `--base-url` switches to the HTTP client. Works with OpenAI, [Ollama](https://ollama.ai/), `text-embeddings-inference`, vLLM, or any service exposing the OpenAI embeddings shape.

```bash
ug ingest \
  --base-url https://api.openai.com/v1 \
  --api-key  $OPENAI_API_KEY \
  --model    text-embedding-3-small
```

### Dimension handling

You don't need to know your model's dim. On first ingest, UltraGraph runs a one-shot probe, writes the discovered dim to `<db>/ug-meta.json`, and reuses it on every subsequent open. Override explicitly with `--embedding-dim <n>` if you need to pin it.

---

## 💬 RAG Chat (`ug chat`)

`ug chat` closes the loop: it retrieves graph-aware context via the same
GraphRAG pipeline that `hybrid_search` uses, then sends it to an
OpenAI-compatible chat model and prints the answer. Use it to verify
the *quality* of the indexed knowledge base end-to-end — not just that
retrieval works, but that a real LLM agent can actually answer
questions grounded in it.

### One-shot

```bash
ug chat "how does graph ingest work?" \
  --base-url http://127.0.0.1:8000/v1 \
  --api-key  12345 \
  --chat-model      Qwen3.6-35B-A3B-MLX-8bit \
  --embedding-model Qwen3-Embedding-4B-4bit-DWQ \
  --show-context
```

The answer is printed to stdout. Add `--json` to emit a single JSON
document containing the answer, citations, retrieval / completion
latencies and (when the server reports it) token usage — handy for
scripted regression testing.

### Interactive REPL

Omit the prompt to drop into a REPL with a 6-turn rolling history:

```bash
ug chat \
  --base-url http://127.0.0.1:8000/v1 \
  --chat-model my-chat-model
# you ❯ how does ingest work?
# Answer:
#   ...
# you ❯ /reset    # clear history
# you ❯ /context on   # show retrieved [#1], [#2], ...
# you ❯ /quit
```

### Key flags

| Flag | Description |
| :--- | :--- |
| `-n, --name <project>` | Project name (default: cwd basename, else the most recently generated project under `~/.ug`) |
| `--chat-model <name>` | Chat completion model (required for remote chat; falls back to `$UG_CHAT_MODEL`) |
| `--base-url` / `--api-key` | OpenAI-compatible endpoint, shared with embeddings (`--chat-base-url`/`--chat-api-key`/`--embedding-*` override each independently) |
| `--strategy ppr\|mmr`, `--hops`, `-k/--limit` | Retrieval tuning — same as `hybrid_search` |
| `--show-context, -v` / `--json` | Print citations alongside the answer, or emit one JSON document for scripting |

Run `ug chat -h` for the complete flag reference (temperature, max-tokens,
system prompt override, snippet/repo-root resolution, etc).

### Chat over HTTP (`POST /api/chat`)

`ug serve` exposes the same pipeline at `POST /api/chat`. Start the
server with chat enabled:

```bash
ug serve \
  --base-url http://127.0.0.1:8000/v1 --api-key 12345 \
  --chat-model Qwen3.6-35B-A3B-MLX-8bit
```

Then either use the built-in **Chat** panel in the web UI
(`http://127.0.0.1:8080`) — which surfaces clickable citations that
jump to the corresponding graph node — or call the API directly:

```bash
curl -s http://127.0.0.1:8080/api/chat \
  -H 'Content-Type: application/json' \
  -d '{
        "query": "explain the PPR seed pool logic",
        "k": 8,
        "hops": 2,
        "history": []
      }' | jq
```

Per-request overrides supported in the body: `chat_model`,
`chat_base_url`, `chat_api_key`, `temperature`, `max_tokens`,
`system_prompt`, `dest`, `edge_types`, `strategy`, `direction`,
`include_snippets`, `max_context_chars`, `where`.

`GET /api/capabilities` reports `chat_ready` plus the current
`chat.model` / `chat.base_url` so clients can disable their chat UI
gracefully when chat isn't configured.

---

## 🤖 MCP Server

Integrate UltraGraph directly into your AI Agent (Cursor, Claude Desktop, etc.).

### Tools Exposed
1.  **search_kb**: Graph-based RAG retrieval (PPR-based).
2.  **traverse_kb**: Structural walk from specific node IDs.
3.  **ping_embedder**: Verify embedding connectivity.

### Configuration
Set these environment variables before starting the server:
- `UG_PROJECT`: Project name under `~/.ug` — the db is `~/.ug/<project>/ugdb` and the repo root is read from that project's `project.json`. **Preferred.**
- `UG_DB_PATH`: Explicit OverGraph directory (overrides `UG_PROJECT`).
- `UG_HOME`: Override the `~/.ug` root.
- `UG_REPO_ROOT`: Root path for resolving snippet file paths (overrides `project.json`).
- `UG_EMBED_MODEL`: Override embedding model (local fastembed alias or remote model name).
- `UG_EMBED_BASE_URL`: **Set this to opt into the remote backend.** When unset, the MCP server uses the in-process ONNX embedder.
- `UG_EMBED_API_KEY`: Bearer token for the remote endpoint.
- `UG_MODEL_CACHE`: Override the local ONNX model cache directory.

When no env vars are set, the server uses `~/.ug/<cwd-basename>/ugdb` if it exists.

Before wiring up a client, run `node node/cli.mjs doctor` to see exactly
which db path, repo root, and embedder settings the MCP server will
resolve to (and which env var, if any, is driving each one).

```json
{
  "mcpServers": {
    "ultragraph": {
      "command": "node",
      "args": ["~/.ug/cli.mjs", "mcp"],
      "env": {
        "UG_PROJECT": "ug"
      }
    }
  }
}
```

---

## 🔌 Native API Usage

You can use the high-performance Rust core directly in your own Node.js or
TypeScript app by requiring the built native addon (`.ug/ug.node`) — no CLI,
no subprocess. Build it first with `npm run build`; TypeScript users get full
types for free from the generated `.ug/index.d.ts` sitting next to it.

```javascript
const { index, buildGraph, dbHybridSearch } = require('/path/to/ug/.ug/ug.node');

// Index a codebase
const symbols = index('./src');
const graph = buildGraph(symbols);

// Search with GraphRAG
const context = await dbHybridSearch('./ugdb', JSON.stringify({
  query: "how does authentication work?",
  k: 10
}));
```

Prefer not to link the addon into your process? Shell out to the `ug` binary
instead (`child_process.execFile`) and parse its JSON output, or run
`ug serve` and call its REST API (`/api/search/hybrid`, `/api/chat`, etc.)
over HTTP.

---

## 🧪 Testing

```bash
# Run JS test suite
npm test

# Run Native Rust tests
npm run build && cd native && cargo test
```

---

## 📚 Further Reading

Deep-dive docs for anyone extending UltraGraph:

| Doc | Covers |
| :--- | :--- |
| [`docs/GRAPH-STORAGE.md`](docs/GRAPH-STORAGE.md) | OverGraph data model, query functions, node/edge mapping |
| [`docs/EMBEDDING-BACKENDS.md`](docs/EMBEDDING-BACKENDS.md) | Local ONNX vs. remote embedder architecture, model aliases, failure modes |
| [`docs/WEB-SERVE.md`](docs/WEB-SERVE.md) | `ug serve`'s REST API, routes, logging, asset resolution |
| [`docs/MCP.md`](docs/MCP.md) | Full MCP tool reference, client setup (Claude Desktop, Cursor, opencode), troubleshooting |
| [`docs/MULTI-DEST.md`](docs/MULTI-DEST.md) | Neo4j backend: CLI flags, capability matrix, schema |
| [`native/README.md`](native/README.md) | Rust crate internals: CLI commands, project structure, extensibility |

## 📄 License
MIT
