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
`embed.base_url`, `embed.api_key`, `embed.dim`.

## Precedence

Always **CLI flag > env var > `ug config` > built-in default**. An explicit flag
or env var still wins over a saved value — but never silently: the CLI prints a
one-line notice when that happens, e.g.

```
▸ note: CLI flag --chat-model overrides saved config chat.model = gpt-4o-mini (~/.ug/config.json)
```

## `.env` files

UltraGraph also loads a `.env` file from the current directory (both the `ug`
binary and `node cli.mjs` do this) for per-repo env-var defaults:

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

## `ug doctor`

Config resolution has several fallback tiers (flag → env var → default, plus
project/db path lookup through `~/.ug`). `ug doctor` (or `node node/cli.mjs
doctor` for the MCP-server side) prints exactly what got resolved and why:

```
$ ug doctor
Project
  UG_HOME:      /Users/you/.ug  [default: ~/.ug]
  project name: my-repo  [derived from cwd basename]
  project dir:  /Users/you/.ug/my-repo (exists)
  db path:      /Users/you/.ug/my-repo/ugdb (exists)  [default: ...]

Embeddings (ingest / gen / semantic_search / search / serve)
  backend:      local (in-process ONNX)  [default]
  model:        BAAI/bge-small-en-v1.5  [default]
  ...

Chat (ug chat / POST /api/chat)
  status:       not configured — using sample defaults; run `ug config set chat.base_url <url>` ...
```
