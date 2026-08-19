# Configuration

Persist defaults once with `ug config` instead of repeating
`--base-url`/`--api-key`/`--model`/`--chat-model` on every invocation:

```bash
ug config set chat.model gpt-4o-mini
ug config set chat.base_url https://api.openai.com/v1
ug config set chat.api_key sk-...
ug config set embed.model text-embedding-3-small

ug config list          # every key, its saved value, and what can override it
ug config get chat.model
ug config unset chat.model
ug config path          # → ~/.ug/config.json (or $UG_HOME/config.json)
```

Values land in `$UG_HOME/config.json` (default `~/.ug/config.json`, written with
owner-only permissions since it may hold API keys) and are picked up by every
command — `ug chat`, `ug serve`'s `/api/chat`, the embedder, and the npm MCP
server.

The visualization exposes the same settings behind the **⚙ gear** (top-right of
the Knowledge Base Manager, and in the sidebar header once a graph is open). It
reads/writes the same file via `GET`/`POST /api/config`, shows which tier
(flag / env / saved / default) currently wins for each key, and chat changes
apply to the running server immediately — no restart.

**Known keys:** `chat.model`, `chat.base_url`, `chat.api_key`,
`chat.temperature`, `chat.max_tokens`, `chat.timeout_secs`, `embed.model`,
`embed.base_url`, `embed.api_key`, `embed.dim`, `vis.renderer`,
`vis.three_d_max_elements`, `vis.solo_threshold`, `graph.server_mode_bytes`.

Visualization keys (`vis.*`) shape how the web graph is drawn. They are read at
page load (the settings panel's Visualization section marks them "applies on
reload"):

- `vis.renderer` — `auto` (default), `three`, or `cosmos`. `auto` renders with
  three.js below `vis.three_d_max_elements` elements and cosmos above it; the
  other two force that engine regardless of size. A per-browser `ug-renderer`
  choice from the canvas's engine toggle still outranks this, as does
  `?r=three|cosmos` on the URL.
- `vis.three_d_max_elements` — nodes/edges the 3D engine is asked to draw
  whole, and the element budget `auto` switches on. Defaults to 3,000. Above it
  `auto` picks the 2D engine; forcing `three` past it hands the 3D engine one
  neighbourhood at a time via solo mode rather than the whole graph.
- `vis.solo_threshold` — nodes/edges past which the page never draws the whole
  graph, opening in solo mode (one neighbourhood at a time) instead. Defaults
  to 200,000. Governs the 2D engine; the 3D engine solos past its own
  `vis.three_d_max_elements` above.

Graph keys (`graph.*`) decide *how the browser gets the graph*, not how it is
drawn — that is the local/server mode split:

- `graph.server_mode_bytes` — `graph.json`'s size, in bytes, past which the
  browser no longer downloads the file. Below it the whole file is served and
  the browser renders everything in the tab (local mode). At or above it the
  page loads a slim node index (every node, no edges) and asks the server for
  edges, neighbourhoods and per-node detail on demand (server mode). Defaults
  to 50 MB (52 428 800). The `--graph-mode` flag sets the *policy* — `auto`
  (default, resolves per graph against this threshold), `local`, or `server` —
  while this key sets the cutoff `auto` uses.

## Precedence

Always **CLI flag > env var > `ug config` > built-in default**. An explicit flag
or env var still wins over a saved value — but never silently: the CLI prints a
one-line notice when that happens, e.g.

```
▸ note: CLI flag --chat-model overrides saved config chat.model = gpt-4o-mini (~/.ug/config.json)
```

## `.env` files

UltraGraph also loads a `.env` file from the current directory (the `ug`
binary does this, including when launched as an MCP server) for per-repo
env-var defaults:

```bash
# .env in your repo root
UG_EMBED_BASE_URL=https://api.openai.com/v1
UG_EMBED_API_KEY=sk-...
UG_EMBED_MODEL=text-embedding-3-small
UG_CHAT_MODEL=gpt-4o-mini
```

A real env var of the same name still wins over `.env`, and both count as the
"env var" tier — above `ug config`, below CLI flags.

## Environment variables

| Env var | Overrides |
| :--- | :--- |
| `UG_HOME` | Root of the `~/.ug` project data directory |
| `UG_PROJECT` | Project name under `~/.ug` (MCP server) |
| `UG_REPO_ROOT` | Repo root used to resolve snippet file paths |
| `UG_EMBED_BASE_URL` / `UG_EMBED_API_KEY` / `UG_EMBED_MODEL` | `--base-url` / `--api-key` / `--model` (embeddings) |
| `UG_CHAT_BASE_URL` / `UG_CHAT_API_KEY` / `UG_CHAT_MODEL` | `--chat-base-url` / `--chat-api-key` / `--chat-model` (`ug chat`) |
| `UG_MODEL_CACHE` | Local ONNX model cache directory |
| `UG_ALLOWED_HOSTS` | Extra hostnames `ug serve` will answer to (comma-separated). Only needed behind a reverse proxy — loopback names and bare IPs are always accepted. See [WEB-SERVE.md](WEB-SERVE.md#security--scope) |
| `UG_BROWSE_ROOTS` | Extra directory roots `ug serve`'s `/api/browse-dir` and `/api/generate` may read (colon-separated). Defaults to `~`, `$UG_HOME`, and the server's working directory |

## `ug doctor`

Config resolution has several fallback tiers (flag → env var → default, plus
project/db path lookup through `~/.ug`). `ug doctor` prints exactly what got
resolved and why (the same resolution the MCP server uses):

```
$ ug doctor
Project
  UG_HOME:      /Users/you/.ug  [default: ~/.ug]
  project name: my-repo  [derived from cwd basename]
  project dir:  /Users/you/.ug/my-repo (exists)
  db path:      /Users/you/.ug/my-repo/ugdb (exists)  [default: ...]

Embeddings (ingest / gen / search / serve)
  backend:      local (in-process ONNX)  [default]
  model:        BAAI/bge-small-en-v1.5  [default]
  ...

Chat (ug chat / POST /api/chat)
  status:       not configured — using sample defaults; run `ug config set chat.base_url <url>` ...
```
