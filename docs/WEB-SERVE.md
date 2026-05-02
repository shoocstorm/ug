# `ug serve` — embedded web server

A self-contained Axum-based web server that serves the visualization UI plus a fast in-memory `graph.json` endpoint, with HTTP compression. Designed to grow into a read-only HTTP API over the same in-memory graph (and later, the OverGraph DB).

## Goals

- Replace the "spin up `python -m http.server`" step in the dev loop with a single `ug serve` command.
- Serve `visualization.html` and `graph.json` from one process so users only need the `ug` binary at runtime.
- Pre-load `graph.json` into memory and negotiate br/gzip compression so the UI loads fast even on remote machines or large graphs.
- Provide a clean foundation for read-only HTTP API endpoints (search, BFS, centrality, hybrid_search) without needing a second server.

## Non-goals

- No write endpoints. The server never mutates `graph.json` or the OverGraph DB.
- No multi-tenant / auth story. Default bind is `127.0.0.1` (loopback only); the operator opts into wider exposure.
- No SPA bundling, hot module reload, or build pipeline. The visualization stays a single embedded HTML file.

---

## Phase 1 — static + JSON (MVP)

**Command**

```
ug serve [-i <graph.json>] [-p <port>] [--host <addr>] [--no-open]
```

| Flag | Default | Notes |
|------|---------|-------|
| `-i, --input` | `ug-out/graph.json` | Path to the graph JSON to serve |
| `-p, --port`  | `8080` | TCP port |
| `--host`      | `127.0.0.1` | Bind address; pass `0.0.0.0` for LAN exposure |
| `--no-open`   | off | Reserved — auto-open is disabled in Phase 1 anyway |

**Routes**

| Method | Path | Response |
|--------|------|----------|
| GET | `/` | `visualization.html` (embedded via `include_str!`) |
| GET | `/index.html` | Same as `/` |
| GET | `/graph.json` | Cached bytes of the graph file, `Content-Type: application/json` |
| GET | `/healthz` | `ok` plain text |

**Implementation notes**

- `graph.json` is read once at startup into `axum::body::Bytes` and shared via `State`. Each request hands out a clone of the `Bytes` (cheap — refcount bump, not a copy).
- `tower_http::compression::CompressionLayer::new()` handles `Accept-Encoding` for br / gzip on the fly. JSON compresses ~10× so this is the main network win.
- Validate JSON parses at startup; print a warning if it doesn't, but still serve.
- Friendly bind errors: print the address and port if `bind` fails so the user knows when 8080 is occupied.

---

## Phase 1.5 — quality-of-life

These are small but worth doing once Phase 1 is in users' hands:

- `--watch` — poll the graph file's mtime every ~2s; re-read on change. Lets users `ug graph` in another terminal and refresh the browser.
- Pre-compress graph bytes at startup (br + gzip) and serve the right one based on `Accept-Encoding` to skip per-request CPU. Probably overkill for sub-100MB graphs but cheap to add.
- Auto-open browser (`--open` flag, `open` crate). Default off to avoid surprising users on remote sessions.
- Listening URL shown as a clickable link via OSC-8 escape codes in supported terminals.

---

## Phase 2 — read-only graph API (in-memory)

Goal: expose the same operations that today require `ug bfs` / `ug search_graph` / `ug centrality` over HTTP, backed by the same in-memory graph the server already holds.

| Method | Path | Maps to |
|--------|------|---------|
| GET | `/api/graph/stats` | node/edge counts, type breakdown |
| GET | `/api/graph/node/:id` | single node by id |
| GET | `/api/graph/search?q=&type=function&type=class` | `graph_keyword_search` |
| GET | `/api/graph/bfs/:id?k=2` | `k_hop_bfs` |
| GET | `/api/graph/path?source=&target=` | `find_shortest_path` |
| GET | `/api/graph/filter?type=Imports&type=Contains` | `filter_edges_by_type` |
| GET | `/api/graph/centrality` | `calculate_centrality` (cached after first call) |
| GET | `/api/graph/cycles` | `detect_cycles` (cached after first call) |

Implementation notes:

- The in-memory representation should switch from "raw JSON bytes" to a parsed `GraphData` (kept alongside the raw bytes) so handlers don't reparse per request.
- Centrality and cycles are expensive — compute lazily on first request and cache in `Arc<OnceLock<...>>`.
- All responses JSON; share a small `ApiError` type so 400/404/500 are consistent.
- Add a query parameter cap (e.g. `k <= 8`, result limit ≤ 1000) to keep handlers from being a DoS foot-gun if someone exposes the server.

---

## Phase 3 — DB-backed search API

Once Phase 2 is stable, layer in OverGraph (LanceDB) endpoints. These are async and need a `Db` handle and an `Embedder` in `State`.

| Method | Path | Maps to |
|--------|------|---------|
| POST | `/api/search/semantic` | `storage::semantic_search` (body: `{query, k, filter?}`) |
| POST | `/api/search/hybrid` | `storage::search_kb` (body: full `SearchKbOptions`) |
| GET | `/api/db/traverse/:id?k=2` | `storage::traverse` |
| GET | `/api/db/node/:id` | single node hydrate |

Implementation notes:

- Open the DB once at startup; `Db` is `Clone`able and async-safe.
- Embedder config flows in from the same `--base-url / --api-key / --model` flags as the other commands.
- Add `--db <path>` flag to `ug serve` (default `ug-out/ugdb`) — if missing, Phase 3 routes return 503 instead of crashing the server.
- Rate-limit hybrid_search per IP (it triggers an embedding call) — `tower_http::limit` or a small token bucket.

---

## Dependencies

Added in Phase 1:

```toml
axum = "0.7"
tower-http = { version = "0.5", features = ["compression-br", "compression-gzip"] }
# tokio already present; need the "net" feature for TcpListener
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net"] }
```

Phase 2 adds nothing new — it uses crates already in the workspace.

Phase 3 adds nothing new either — `overgraph`, `reqwest`, `serde_json` are already pulled in.

---

## Security / scope

- Default bind `127.0.0.1`. Document `--host 0.0.0.0` as opt-in only.
- No CORS layer in Phase 1 (same-origin). Add `tower_http::cors` only if/when we expose API endpoints meant to be hit from other origins.
- All endpoints read-only across all phases. If we ever add a write surface (e.g. annotation), it goes behind an explicit `--rw` flag and a token check, not a default-on route.

---

## Open questions

- Should `ug gen` optionally chain into `ug serve` (e.g. `ug gen --serve`)? Probably yes once Phase 1 is in — it removes one more user step.
- Worth a structured log layer (`tower_http::trace`) for the API phases, or is plain `println!` enough? Lean toward adding it in Phase 2 when there are >3 routes.
