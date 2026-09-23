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

## How a turn works

**The model decides what to retrieve. Nothing is searched before it does.**

`ug chat` and `POST /api/chat` hand the model the same twelve graph tools an
MCP client gets, and let it deliberate before answering. A turn is:

1. **It reads your question.** No retrieval has run. It has the toolbox and
   the repository's shape, and decides what it needs — which for a question
   like "what did I just change" is no retrieval at all.
2. **It calls tools, several per round if it wants.** Calls within one round
   run concurrently; rounds are sequential, capped at 8.
3. **Everything it saw is citable.** Every node any tool returns gets a number
   in one `[#N]` run for the turn, so a symbol found by `find_usages` on round
   three is cited exactly like one from the first search.
4. **It answers**, citing those numbers. The reply says if it stopped because
   it ran out of rounds rather than because it was done.

Two defaults are load-bearing, and both were once the other way round:

| | Default | Why |
| :--- | :--- | :--- |
| **Deliberation** | **on** with tools | A model given no room to think answers from whatever it was handed. Measured over 12 questions: with deliberation off it made **zero** tool calls and its answer was identical to a plain retrieval, at a thousand times the cost. |
| **Pre-retrieval** | **off** with tools | Redundant once the model searches for itself — it asks in the codebase's vocabulary rather than yours, so the pre-pass arrives as a second, worse-phrased copy of the same neighbourhood. Removing it cost no measurable recall and saves ~10k tokens a turn. |

Both follow the toolbox: `--no-tools` (CLI) or `"tools": false` (HTTP) restores
the old behaviour — one hybrid pass, no deliberation, one completion. That is
the fast path, and it is an explicit trade rather than a hidden one. `--seed`
asks for the pre-pass back without giving up the tools.

Numbers, method and what did **not** work: [`dev/RAG-EVAL.md`](dev/RAG-EVAL.md).

### What a turn cost

Every answer reports its own token bill — `cost` in the JSON, a collapsible
box in the web UI:

| | |
| :--- | :--- |
| Retrieved pack / Tool results / Answer | what this question put in front of the model |
| System prompt / Tool schemas | fixed overhead — the same for every question, and **re-sent on every round** |
| Billed by the model | what your endpoint actually counted, across every round |
| *N* cited files, read whole | what the same evidence costs opened in full |

The fixed rows are worth looking at: on this repository the twelve tool schemas
measure **~9,200 tokens**, so a seven-round turn spends ~64k tokens restating
them. That is usually larger than the evidence and it is the bulk of the gap
between "sent" and "billed" — `--no-tools` is the only way not to pay it.

The last row is a comparison, not something the turn spent: it sizes the files
the citations came from, which is what an agent without a graph pays once it
has found them. It is not a claim about any other RAG system. Token figures
marked `~` are estimated from character length — `ug` has no tokenizer for an
arbitrary endpoint — while "billed" is exact.

### Budgets

| Setting | Default | Notes |
| :--- | ---: | :--- |
| Tool rounds per turn | 8 | `--max-tool-rounds`, ceiling 16 |
| Characters per tool result | 60,000 | Shared with the MCP tools' own `maxChars` |
| Request timeout | 900 s | A deliberating turn is several completions |

There is **no whole-turn budget**: several large results across several rounds
can exceed a small context window. Lower the per-result cap for a model with a
short window rather than assuming the cap protects you.

## Key flags

| Flag | Description |
| :--- | :--- |
| `-n, --name <project>` | Project name (default: active project set with `ug active <name>`, else cwd basename, else the most recently generated project under `~/.ug`) |
| `--chat-model <name>` | Chat completion model (required for remote chat; falls back to `$UG_CHAT_MODEL`) |
| `--base-url` / `--api-key` | OpenAI-compatible endpoint, shared with embeddings (`--chat-base-url`/`--chat-api-key`/`--embedding-*` override each independently) |
| `-k/--limit`, `--max-chars`, `--filter` | Retrieval tuning — same as `search` |
| `--show-context, -v` / `--json` | Print citations alongside the answer, or emit one JSON document for scripting |
| `--think` / `--no-tools` | Deliberation is already on when tools are attached; `--no-tools` drops the toolbox and takes the fast, single-completion path |
| `--seed` | Also run one hybrid retrieval on your wording before the model speaks (default: only with `--no-tools`) |
| `--max-tool-rounds <n>` | Cap tool-calling rounds (default 8, max 16) |

Run `ug chat -h` for the complete flag reference (temperature, max-tokens,
system prompt override, snippet/repo-root resolution, etc).

## No endpoint at all: run the model in the browser

Everything above assumes you have an OpenAI-compatible endpoint. If you do
not, `ug serve` can borrow one from the browser: open the UI, click the chip
icon in the sidebar header (or **Run one in this browser** on the banner that
says answers need a model), and pick a model. It downloads once into the
browser's own storage, runs there with llama.cpp compiled to WebAssembly, and
registers itself as this server's chat endpoint.

From then on `/api/chat`, `/api/tour` and `/api/walk` — and `ug chat` in a
terminal, as long as the tab stays open — are answered on your machine, with
the same prompts, tools and citations. Nothing is sent anywhere.

What to expect: SmolLM2 135M (138 MB) is there to prove the plumbing — it
downloads in seconds, answers instantly and talks nonsense; Qwen3 0.6B
answers at tens of tokens a second and narrates tours well but is weak at
multi-step tool use; Qwen3 1.7B is the usable middle. A model that cannot use the graph
toolbox — no tool-calling template, or simply too small to call one properly —
is never handed it: the turn falls back to seeded retrieval, which is the
`--no-tools` path and answers most questions better anyway at these sizes.

The window you pick decides the rest. ug's toolbox is ~9 900 tokens of JSON
schema on its own, so below 8192 tokens there is no room for it at all; the
panel labels each window with what it buys and caps the retrieval controls
(`k`, hops, results, tour stops) to what will actually fit. Retrieval budgets are clamped automatically to whatever context
window the model was loaded with, so a local model gets a smaller, tighter
context pack than a hosted one — see "Phase 5" in `docs/WEB-SERVE.md` for the
mechanics.

Closing the tab stops the model and hands chat back to whatever endpoint was
configured before.

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
`edge_types`, `direction`, `include_snippets`, `max_context_chars`, `where`,
plus the four that decide how the turn runs:

| Field | Default | Effect |
| :--- | :--- | :--- |
| `tools` | `true` | The graph toolbox. `false` takes the fast path: one hybrid pass, no deliberation |
| `seed` | `!tools` | Run one hybrid retrieval before the model speaks |
| `think` | — | Deliberation follows `tools`; an explicit `false` is ignored while tools are attached |
| `max_tool_rounds` | `8` | Ceiling 16 |

The response (and the SSE `done` event) carries `tool_calls`, `tool_rounds`,
`hit_round_cap` and `cost` alongside `answer` and `citations`. Streaming also
emits a `citations` event whenever a tool adds to the evidence list, so the
sources shown grow as the turn runs.

`GET /api/capabilities` reports `chat_ready` plus the current `chat.model` /
`chat.base_url` so clients can disable their chat UI gracefully when chat isn't
configured.
