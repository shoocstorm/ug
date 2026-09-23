# UltraGraph

A local-first engine that turns codebases and documents into an interactive,
queryable **Semantic Knowledge Graph**. Built with Rust speed.

- **Intro**: [ultra-graph.web.app](https://ultra-graph.web.app)
- **Demo**: [Click to watch demo video](https://youtu.be/9Je4T8h1YX8)

[![UltraGraph demo](docs/UG-Hybrid-Search.png)](https://youtu.be/9Je4T8h1YX8)

## Install

```bash
curl -fsSL https://ultra-graph.web.app/install.sh | sh
```

Windows: download `ultragraph-windows-x64.zip` from
[Releases](https://github.com/shoocstorm/ug/releases/latest).

From source (Rust required): `cd native && cargo build --release`

## Quick Start

```bash
ug gen        # index this repo → ~/.ug/<name>/
ug            # open the web UI at :8080
```

`ug gen` runs the full pipeline on the current directory — index, graph,
and database. Output goes to `~/.ug/<project-name>/`.

`ug` (or `ug serve`) starts a web UI with visualization, search, and a REST
API. With zero projects it shows a setup wizard.

```bash
ug gen -i ~/code/other-repo -n other    # index another repo
ug gen --with-embed                     # include vector embeddings
ug -h                                   # all commands
```

Embeddings are **opt-in**: by default `ug gen` skips the embedding model
(most of the wall clock), so everything structural works immediately.
Only `search`, `chat`, and tours need vectors. Use `--with-embed` to build
them in the same run.

## Architecture

[![UltraGraph Architecture](docs/UG-Architecture.png)](https://ultra-graph.web.app/architecture.html)

A four-phase pipeline ([interactive view](https://ultra-graph.web.app/architecture.html)):

1. **Indexing** — parallel `tree-sitter` indexer, incremental via `blake3`
2. **Graph** — symbol graph with structural analysis (centrality, cycles, shortest paths)
3. **Storage** — OverGraph vector + full-text store with local ONNX embedding
4. **Search** — GraphRAG: Personalized PageRank fusing semantic + structural relevance

## Features

| Area | What |
| :--- | :--- |
| **Languages** | TypeScript, JavaScript, Python, Java, Rust, Markdown, PDF |
| **Graph** | Functions, Classes, Interfaces, Imports, Calls — with cross-file call resolution |
| **Search** | Semantic + keyword + graph expansion (GraphRAG) |
| **Chat** | RAG-grounded chat against any OpenAI-compatible LLM — or against a model running in your browser tab, downloaded from the UI, no key and no server |
| **Changes** | `ug walk` — narrated walkthrough of a git diff in call-graph order |
| **Interfaces** | Web UI, desktop app (Tauri), MCP server, CLI |

## Key Commands

| Command | What it does |
| :--- | :--- |
| `ug gen` | Full pipeline: index → graph → database |
| `ug update <file>...` | Refresh graph for changed files only |
| `ug hook install` | Auto-refresh graph on git commits |
| `ug serve` / `ug app` | Web UI + REST API (desktop shell with `app`) |
| `ug search "<query>"` | GraphRAG search |
| `ug chat "<question>"` | RAG-grounded chat |
| `ug walk [<rev>]` | Narrated walkthrough of a git diff |
| `ug context <symbol>` | Everything about one symbol in one call |
| `ug analyze <preset>` | Whole-repo statistics and blast radius |
| `ug list` / `ug remove` | Manage projects under `~/.ug` |

Run `ug -h` for the full list, or `ug <command> -h` for flags.

## Configuration

No LLM to point it at? Open the web UI, click the chip icon in the sidebar
header, and pick a small model: it downloads once into the browser, runs
there (llama.cpp compiled to WebAssembly) and becomes this server's chat
endpoint for answers, tours and walks. See [docs/CHAT.md](docs/CHAT.md).

```bash
ug config set chat.model gpt-4o-mini
ug config set chat.base_url https://api.openai.com/v1
ug config list
```

Precedence: **CLI flag > env var > `ug config` > default**.
Run `ug doctor` to see resolved values. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Embeddings

```bash
ug ingest                                  # local ONNX (default, no config needed)
ug ingest --base-url https://api.openai.com/v1 --api-key $KEY \
          --model text-embedding-3-small   # remote
```

No external service required — ships with a local ONNX embedder. Weights
download once on first use (~22–130 MB). See [docs/EMBEDDING-BACKENDS.md](docs/EMBEDDING-BACKENDS.md).

## AI Agent Integration

Connect an AI agent to UltraGraph:

```bash
ug connect [agent]     # interactive picker (claude, cursor, windsurf, etc.)
```

This sets up either a **CLI skill** (recommended) or an **MCP server**.
Add `--hooks` to install git hooks that auto-refresh the graph on commits.
See [docs/MCP-SERVE.md](docs/MCP-SERVE.md).

## Data Layout

```
~/.ug/<project>/
├── graph.json          # knowledge graph
├── ugdb/               # vector + edge store
└── project.json        # metadata
```

## Further Reading

| Doc | Covers |
| :--- | :--- |
| [`docs/API-REFERENCE.md`](docs/API-REFERENCE.md) | Complete CLI, HTTP API, MCP tools |
| [`docs/CHAT.md`](docs/CHAT.md) | `ug chat` flags, REPL, HTTP API |
| [`docs/EMBEDDING-BACKENDS.md`](docs/EMBEDDING-BACKENDS.md) | Local vs remote embedder |
| [`docs/MCP-SERVE.md`](docs/MCP-SERVE.md) | MCP tools, client setup |
| [`docs/ANALYZE.md`](docs/ANALYZE.md) | Whole-repo statistics |
| [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) | Config keys, env vars |
| [`docs/WEB-SERVE.md`](docs/WEB-SERVE.md) | REST API routes |

## License

MIT
