# RAG Chat (`ug chat`)

`ug chat` closes the loop: it retrieves graph-aware context via the same
GraphRAG pipeline that `search` uses, then sends it to an OpenAI-compatible chat
model and prints the answer. Use it to verify the *quality* of the indexed
knowledge base end-to-end — not just that retrieval works, but that a real LLM
agent can actually answer questions grounded in it.

## One-shot

```bash
ug chat "how does graph ingest work?" \
  --base-url http://127.0.0.1:8000/v1 \
  --api-key  12345 \
  --chat-model      Qwen3.6-35B-A3B-MLX-8bit \
  --embedding-model Qwen3-Embedding-4B-4bit-DWQ \
  --show-context
```

The answer is printed to stdout. Add `--json` to emit a single JSON document
containing the answer, citations, retrieval / completion latencies and (when the
server reports it) token usage — handy for scripted regression testing.

## Interactive REPL

Omit the prompt to drop into a REPL with a 6-turn rolling history:

```bash
ug chat \
  --base-url http://127.0.0.1:8000/v1 \
  --chat-model my-chat-model
# you ❯ how does ingest work?
# Answer:
#   ...
# you ❯ /reset        # clear history
# you ❯ /context on   # show retrieved [#1], [#2], ...
# you ❯ /quit
```

## Key flags

| Flag | Description |
| :--- | :--- |
| `-n, --name <project>` | Project name (default: cwd basename, else the most recently generated project under `~/.ug`) |
| `--chat-model <name>` | Chat completion model (required for remote chat; falls back to `$UG_CHAT_MODEL`) |
| `--base-url` / `--api-key` | OpenAI-compatible endpoint, shared with embeddings (`--chat-base-url`/`--chat-api-key`/`--embedding-*` override each independently) |
| `-k/--limit`, `--max-chars`, `--filter` | Retrieval tuning — same as `search` |
| `--show-context, -v` / `--json` | Print citations alongside the answer, or emit one JSON document for scripting |

Run `ug chat -h` for the complete flag reference (temperature, max-tokens,
system prompt override, snippet/repo-root resolution, etc).

## Chat over HTTP (`POST /api/chat`)

`ug serve` exposes the same pipeline at `POST /api/chat`. Start the server with
chat enabled:

```bash
ug serve \
  --base-url http://127.0.0.1:8000/v1 --api-key 12345 \
  --chat-model Qwen3.6-35B-A3B-MLX-8bit
```

Then either use the built-in **Chat** panel in the web UI
(`http://127.0.0.1:8080`) — which surfaces clickable citations that jump to the
corresponding graph node — or call the API directly:

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

Per-request overrides supported in the body: `chat_model`, `chat_base_url`,
`chat_api_key`, `temperature`, `max_tokens`, `system_prompt`, `dest`,
`edge_types`, `direction`, `include_snippets`, `max_context_chars`, `where`.

`GET /api/capabilities` reports `chat_ready` plus the current `chat.model` /
`chat.base_url` so clients can disable their chat UI gracefully when chat isn't
configured.
