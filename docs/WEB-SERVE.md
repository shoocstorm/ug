# `ug serve` — embedded web server

A self-contained Axum-based web server that serves the visualization UI plus a fast in-memory `graph.json` endpoint, with HTTP compression. Designed to grow into a read-only HTTP API over the same in-memory graph — and an OverGraph DB-backed semantic / hybrid-search surface.

## Goals

- Replace the "spin up `python -m http.server`" step in the dev loop with a single `ug serve` command.
- Serve `visualization.html`, `d3.v7.min.js`, and `graph.json` from one process so users only need the `ug` binary at runtime.
- Pre-load `graph.json` into memory and serve pre-compressed (br + gzip) bytes so the UI loads fast even on remote machines or large graphs.
- Provide a clean foundation for read-only HTTP API endpoints (search, BFS, centrality, semantic / hybrid search) without needing a second server.

## Non-goals

- No write endpoints. The server never mutates `graph.json` or the OverGraph DB.
- No multi-tenant / auth story. Default bind is `127.0.0.1` (loopback only); the operator opts into wider exposure.
- No SPA bundling, hot module reload, or build pipeline. The visualization stays a single embedded HTML file.

---

## Asset resolution at runtime

Worth being explicit, because it confuses people:

| File | Source | Resolved at |
|------|--------|-------------|
| `/` and `/index.html` | `native/src/vis/visualization.html` via `include_str!` | **compile time** — baked into the `ug` binary |
| `/d3.v7.min.js` | `native/src/vis/d3.v7.min.js` via `include_bytes!` | **compile time** — baked into the `ug` binary |
| `/graph.json` | `-i <path>` flag (default `ugout/graph.json`) | **startup** — read once, held in memory |
| `/api/db/*`, `/api/search/*` | OverGraph DB at `-d <path>` (default `ugout/ugdb`) | **startup** — opened once |
| `/api/chat` | OpenAI-compatible chat endpoint configured via `--chat-model` / `--chat-base-url` / `--chat-api-key` (or `UG_CHAT_*` env vars). Disabled when no `--chat-model` is set — route returns 503. | **startup** — config baked once; per-request body can override `chat_model` / `chat_base_url` / `chat_api_key` / `temperature` / `max_tokens` |

So:

- Editing `native/src/vis/*` does **not** affect a running `ug serve` until you `cargo build` again.
- The `index.html` / `d3.v7.min.js` files written into `ugout/` by `ug gen` are a *separate* copy meant for serving the directory with any static server (or `file://`). `ug serve` does not consult that copy.
- Path resolution for `--input` and `--db` is relative to the working directory where you invoked `ug serve`, not relative to the binary's location.

---

## Phase 1 — static + JSON  ✅ shipped

**Command**

```
ug serve [-i <graph.json>] [-p <port>] [--host <addr>]
```

| Flag | Default | Notes |
|------|---------|-------|
| `-i, --input` | `ugout/graph.json` | Path to the graph JSON to serve |
| `-p, --port`  | `8080` | TCP port |
| `--host`      | `127.0.0.1` | Bind address; pass `0.0.0.0` for LAN exposure |

**Routes**

| Method | Path | Response |
|--------|------|----------|
| GET | `/`, `/index.html` | `visualization.html` (embedded) |
| GET | `/d3.v7.min.js` | minified d3 v7.9.0 (embedded) |
| GET | `/graph.json` | bytes of the graph file |
| GET | `/healthz` | `ok` plain text |

---

## Phase 1.5 — quality-of-life  ✅ partial

| Item | Status |
|------|--------|
| `--watch` — poll graph file mtime, hot-reload snapshot | ✅ shipped |
| Pre-compress graph + HTML + d3 once at startup (br-9 + gzip-9), serve based on `Accept-Encoding` | ✅ shipped |
| Structured logging (`tracing` + `tower_http::trace`) — see [Logging](#logging) | ✅ shipped |
| `ug gen --serve` chain — gen pipeline flows directly into the server | ✅ shipped |
| Auto-open browser (`--open`), OSC-8 clickable URL | not shipped |

Pre-compression is done in-process (`flate2` for gzip, `brotli` crate for br). The `tower_http::compression::CompressionLayer` still wraps the router but no-ops on the pre-encoded statics — it kicks in only for dynamic `/api/*` JSON responses. Reload preserves the same `Arc<GraphSnapshot>` swap pattern, so handlers see consistent state across a single request.

Reference numbers on a 31.6 MB / 41 614-node / 95 117-edge graph (debug build, build-time pre-compression):

| encoding | bytes |
|---|---|
| identity | 31 625 670 |
| gzip-9   | 2 777 891 (~11×) |
| brotli-9 | 2 272 127 (~14×) |

---

## Phase 2 — read-only graph API (in-memory)  ✅ shipped

| Method | Path | Notes |
|--------|------|-------|
| GET | `/api/graph/stats` | node/edge counts + per-type breakdown |
| GET | `/api/graph/node/*id` | single node by id (wildcard accepts slashes/colons) |
| GET | `/api/graph/search?q=&types=function,class` | substring match on name + docstring |
| GET | `/api/graph/bfs/*id?k=2` | k-hop forward BFS, server caps `k` to 8 |
| GET | `/api/graph/path?source=&target=` | forward shortest path |
| GET | `/api/graph/filter?types=Imports,Calls` | edges by type |
| GET | `/api/graph/centrality` | cached after first call (`OnceLock`) |
| GET | `/api/graph/cycles` | cached after first call |

Implementation choices that mattered:

- Repeated query params don't merge to `Vec` in `serde_urlencoded`, so multi-value parameters use comma-separated form (`?types=function,class`) — also nicer in URLs.
- Handlers operate on the parsed `GraphData` directly, **not** on the lib's `String → String` functions. The first cut routed search/bfs/path/filter through the lib functions, which re-parsed the full 31 MB JSON inside the lib on every request and made `?q=hello` hang for seconds. Switching to direct iteration over `snap.parsed` dropped search to ~5 ms.
- BFS and path build a forward-adjacency index (`HashMap<id, idx>` + `Vec<Vec<idx>>`) lazily on first call via `OnceLock<AdjIndex>` per snapshot. Invalidated automatically on `--watch` reload.
- Centrality and cycles still use the lib functions but cache the result string in `OnceLock<String>` per snapshot — first call eats the parse + algorithm, subsequent calls are constant-time.
- Wildcard segments (`/*id`) are required because node ids contain slashes (`function:tools/foo.py:42:bar`). A plain `/:id` would 404.

---

## Phase 3 — DB-backed semantic / hybrid API  ✅ shipped

OverGraph endpoints. Async, need a `Db` handle and an `Embedder`.

| Method | Path | Maps to |
|--------|------|---------|
| GET  | `/api/db/node/*id` | `Db::fetch_node` — single-row hydrate |
| GET  | `/api/db/traverse/*id?k=&dir=&types=` | `storage::traverse_filtered` — BFS over the DB edge table |
| POST | `/api/search/semantic` | `storage::semantic_search[_w_where]` — single-shot vector search |
| POST | `/api/search/hybrid` | `storage::search_kb` — RRF / PPR / MMR + snippet attachment |

**`ug serve` flags added in Phase 3**

| Flag | Default | Notes |
|------|---------|-------|
| `-d, --db <path>` | `ugout/ugdb` | OverGraph directory; if open fails, Phase 3 routes return **503** but the server still starts |
| `--no-db` | off | Skip opening DB and embedder entirely (start server in Phase-1/2-only mode) |
| `--base-url <url>` | `http://localhost:8000/v1` | Embedding endpoint (same flag as other commands) |
| `--api-key <key>` | env / default | Embedding API key |
| `--model <name>` | default | Embedding model |
| `--embedding-dim <n>` | from `<db>/ug-meta.json` (or 1024 if absent) | Override the embedding dimension. Must match the DB's recorded dim. |
| `--repo-root <path>` | cwd | Repo root for snippet path resolution in hybrid_search |

**Request / response shape**

`POST /api/search/semantic`

```json
{ "query": "oauth login flow", "k": 10, "filter": null }
```

→ `{ "count": <n>, "hits": [{ "id", "name", "node_type", "file", "start_line", "end_line", "description", "distance" }] }`

`POST /api/search/hybrid` — body mirrors `SearchKbOptions` knobs:

```json
{
  "query": "oauth login flow",
  "k": 8,
  "hops": 2,
  "edge_types": null,
  "direction": "both",
  "strategy": "ppr",
  "max_chars": 12000,
  "mmr_lambda": 0.6,
  "where": null,
  "include_snippets": true
}
```

Quick smoke-test:

```bash
curl -v -X POST -H "Content-Type: application/json" \
  http://localhost:8080/api/search/hybrid \
  -d '{"query":"hi", "k":4, "strategy":"ppr"}'
```

→ `RankedContext` (already derives `Serialize` in storage layer).

`GET /api/db/traverse/*id?k=2&dir=outbound&types=Calls,Imports`

→ `{ "nodes": [...], "edges": [...], "distances": { "<id>": <hops> } }`

**`POST /api/chat`** — RAG-grounded chat against an OpenAI-compatible LLM.

Enabled when `ug serve` is started with `--chat-model` (or
`UG_CHAT_MODEL` is set). Internally runs the same `storage::search_kb`
hybrid retrieval used by `/api/search/hybrid`, then sends the assembled
context + the user query to the chat endpoint. Returns the assistant
answer plus structured citations (each numbered `[#N]` and clickable
in the web UI).

Request body:

```json
{
  "query": "explain the PPR seed pool logic",
  "k": 8,
  "hops": 2,
  "strategy": "ppr",
  "direction": "both",
  "include_snippets": true,
  "max_context_chars": 12000,
  "history": [
    { "role": "user", "content": "previous question" },
    { "role": "assistant", "content": "previous answer" }
  ],
  "chat_model": "Qwen3.6-35B-A3B-MLX-8bit",
  "chat_base_url": "http://127.0.0.1:8000/v1",
  "chat_api_key": "12345",
  "temperature": 0.2,
  "max_tokens": 1024,
  "system_prompt": null,
  "dest": null
}
```

Response shape:

```json
{
  "query": "...",
  "answer": "...",
  "citations": [
    { "index": 1, "id": "...", "name": "...", "node_type": "...",
      "file": "...", "start_line": 0, "end_line": 0,
      "description": "...", "distance": 0.0, "hop": 0, "snippet": "..." }
  ],
  "seed_id": "...",
  "retrieval_ms": 123,
  "completion_ms": 456,
  "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
  "dest": "overgraph",
  "chat_model": "Qwen3.6-35B-A3B-MLX-8bit"
}
```

503s when chat isn't configured (no startup `--chat-model` and no
per-request `chat_model` override). `GET /api/capabilities` exposes
`chat_ready` + the current `chat.model` / `chat.base_url` so clients
can disable their chat UI when the route isn't usable.

**Implementation notes**

- `Db` and `Embedder` aren't `Clone`, so wrap each in `Arc`. Held in `ServeState` as `Option<Arc<...>>` so a missing/broken DB doesn't kill the server.
- `Db::open` is async — happens inside the same `tokio::block_on` that owns the listener.
- `/api/db/node/*id` uses a new `Db::fetch_node(key) -> Result<Option<NodeRow>>` helper added to `storage::db`. The intuitive shape `traverse(id, 0)` doesn't work — OverGraph rejects depth=0 with `min_depth must be <= max_depth`. `fetch_node` does `lookup_id` → `engine.get_node` → `node_record_to_row` directly, no over-fetch.
- A process-wide `tokio::sync::Semaphore` (4 permits) gates `/api/search/*` so the embedding endpoint isn't hammered. Per-IP rate limiting is **not** included; if you bind `0.0.0.0`, put the server behind a reverse proxy.
- Phase 3 routes return **503** + ``{"error": "..."}`` when `db` or `embedder` is missing, including the failure reason from startup.
- `RankStrategy` and `Direction` from the JSON body are parsed via the storage layer's `from_str_lossy` — accepts lowercase strings (`"ppr"`, `"mmr"`, `"outbound"`, `"inbound"`, `"both"`).

---

## `ug gen --serve` chain  ✅ shipped

`ug gen` accepts `--serve` to flow directly into the server after the gen pipeline finishes — removes the "now run `ug serve -i ugout/graph.json`" hand-off step.

```bash
ug gen -i ./src --serve                      # full pipeline + serve on :8080
ug gen -i ./src --no-ingest --serve -p 9000  # skip ingest, serve at :9000 with --no-db
ug gen -i ./src --serve --watch              # also watches the graph.json it just wrote
```

Inherits from the original `gen` invocation: `-p`/`--port`, `--host`, `--watch`, `--repo-root`, and the embedder flags (`--base-url`, `--api-key`, `--model`). Sets `-i <generated_graph_path>` and `-d <db_path>` automatically. When `--no-ingest` is set, also passes `--no-db` so Phase 3 routes 503 cleanly instead of crashing on a missing DB.

Internally implemented by a `chain_to_serve()` helper in `main.rs` that builds a synthetic args vec and hands off to `serve::run_serve(...)` (which never returns).

---

## Logging

The server uses `tracing` + `tracing-subscriber` for structured logs. Initialized lazily at the top of `run_serve` via `try_init`, so chained `gen --serve` calls don't double-init.

**Default filter**

```
info,ultragraph=info,tower_http=info,hyper=warn,h2=warn,reqwest=warn,rustls=warn
```

**Override with `RUST_LOG`** — same precedence as any tracing-subscriber app:

```bash
RUST_LOG=warn ug serve …                                 # quiet, errors only
RUST_LOG=ug::serve::watch=debug ug serve … --watch       # only watch events
RUST_LOG=tower_http::trace=info,ug::serve=warn ug serve  # request traffic only
RUST_LOG=debug ug serve …                                 # everything
```

**What gets logged**

| Target | Level | When |
|--------|-------|------|
| `ug::serve` | INFO | DB open success, "ug serve ready" startup readout (one event with all fields) |
| `ug::serve` | WARN | DB open failed, Phase 3 disabled |
| `ug::serve` | ERROR | snapshot load failed, bind failed, server crashed |
| `ug::serve::watch` | INFO | graph reloaded after mtime change |
| `ug::serve::watch` | WARN | reload failed |
| `tower_http::trace::on_response` | INFO | one event per request with `latency`, `status`, `method`, `uri`, `version` |

Sample output:

```
2026-05-02T14:34:47.126Z INFO ug::serve: DB opened path=ugout/ugdb
2026-05-02T14:34:47.126Z INFO ug::serve: ug serve ready graph=ugout/graph.json nodes=41614 edges=95117 identity_bytes=31625670 gzip_bytes=2778217 brotli_bytes=2271798 encode_secs=9.27 addr=127.0.0.1:8765 db_api=true watch=true
2026-05-02T14:34:47.359Z INFO request: tower_http::trace::on_response: finished processing request latency=12 ms status=200 method=GET uri=/api/graph/stats version=HTTP/1.1
2026-05-02T14:35:00.111Z INFO ug::serve::watch: graph reloaded path=ugout/graph.json bytes=31625670 nodes=41614 edges=95117
```

---

## Dependencies

| Phase | Adds |
|-------|------|
| 1 | `axum = "0.7"`, `tower-http = "0.5"` (compression-br, compression-gzip), tokio `net` feature |
| 1.5 | `flate2 = "1"`, `brotli = "8"` (startup-time pre-compression at quality 9), `tracing = "0.1"`, `tracing-subscriber = "0.3"` (env-filter, fmt), `tower-http` `trace` feature |
| 2 | nothing — uses crates already in the workspace |
| 3 | nothing — `overgraph`, `reqwest`, `serde_json`, `tokio::sync` are already in the tree |

---

## Security / scope

- Default bind `127.0.0.1`. Document `--host 0.0.0.0` as opt-in only.
- No CORS layer (same-origin). Add `tower_http::cors` only if/when we expose API endpoints meant to be hit from other origins.
- All endpoints read-only across all phases. If we ever add a write surface (e.g. annotation), it goes behind an explicit `--rw` flag and a token check, not a default-on route.

---

## Open questions

- JSON-formatted log output (`tracing-subscriber`'s `fmt::json()`) for shipping logs to a collector — easy add behind a `--log-format=json` flag if needed.
- The `ug gen` `-o` flag currently double-binds to both `output_dir` and `db_path` (`flag_value(args, &["-o", "--output"])` is read twice with different defaults). When users pass a custom `-o`, both end up the same string. Pre-existing bug, unrelated to `ug serve`, but worth noting since `--serve` chains on `db_path`.
