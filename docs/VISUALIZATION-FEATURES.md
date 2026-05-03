# Visualization UI — Feature Ideas

> What could we build in `visualization.html` by leveraging the `ug serve` APIs?

This document maps the WEB-SERVE APIs to potential UI features. Each item includes rationale, API integration point, and complexity estimate.

---

## Current State

`visualization.html` (the embedded UI):
- Loads `graph.json` via fetch on startup
- Performs client-side search via `String.includes`
- Runs BFS, path, centrality, cycle detection in-browser
- Shows basic node details in a slide-out panel
- No integration with DB layer or semantic search

Leveraging the server APIs unlocks: faster queries on large graphs, DB-backed enriched metadata, semantic search, and server-side algorithms.

---

## High-Impact / Low-Effort

### 1. [x] Server-side search with `/api/graph/search`

**Why:** Client-side search on 40k+ nodes scans the full array in JS. The server does substring match + type filtering in ~5ms.

**Change:** Replace local `state.graph` filter with:

```js
// search via server when API available
const res = await fetch(`/api/graph/search?q=${encodeURIComponent(q)}&types=${types}`);
const { hits } = await res.json();
```

**Fallthrough:** If server returns 404 or error, continue using client-side filtering.

---

### 2. [x] Live graph health indicator

**Why:** Users want to know if the loaded graph is current. Server already has `/healthz`.

**Change:** On load, `fetch('/healthz')` → show "Connected" / "Stale" badge in header.

---

### 3. [x] Node detail enrichment from DB via `/api/db/node/*id`

**Why:** `graph.json` has minimal node data. OverGraph DB has full `NodeRow` with docstrings, metrics, signatures.

**Change:** When a node is selected, call `/api/db/node/${encodeURIComponent(id)}`. If 200, hydrate the info panel with DB fields:

- `description` (from docstring / comment)
- `metrics` (cyclomatic complexity, LOC, etc.)
- `signature`
- File snippet / context

**Fallthrough:** If 503 (DB not available), show only the graph.json data.

---

## Medium Effort

### 4. Interactive k-hop neighborhood via `/api/graph/bfs` or `/api/db/traverse`

**Why:** Currently, clicking "Expand" on a node in the info panel runs client-side BFS. Server can do it faster and with DB-backed edges.

**Change:** Add "Explore > 1 hop →" button in the node info panel:

- Default: use `/api/graph/bfs/${id}?k=2` (in-memory edges)
- With DB available: use `/api/db/traverse/${id}?k=2&dir=outbound` (full edge table)

**Result:** Highlight reachable nodes in the visualization without loading the full graph.

---

### 5. Semantic search panel via `/api/search/semantic`

**Why:** Users often search by intent ("oauth login flow") not node name.

**Change:** Add a "Semantic" tab next to "Search" in the sidebar:

```js
const res = await fetch('/api/search/semantic', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ query: "user authentication flow", k: 10 })
});
```

**UI:** Render results as clickable chips with distance scores.

---

### 6. Hybrid search with snippets via `/api/search/hybrid`

**Why:** Return not just matching nodes but code snippets from the source files.

**Change:** Add a "Hybrid" tab or expand the semantic results:

- POST to `/api/search/hybrid` with `include_snippets: true`
- Render each hit as a collapsible card: node name + file excerpt + highlight

**Note:** Requires `--repo-root` set on the server so it can read source files.

---

### 7. Centrality pre-computed via `/api/graph/centrality`

**Why:** Server caches centrality (degree + betweenness) after first call. Reuse instead of running in-browser.

**Change:** In the "Centrality" tool panel, fetch `/api/graph/centrality` once on expand, cache in JS, then render.

---

### 8. Cycle detection via `/api/graph/cycles`

**Why:** Server computes cycles and caches. Use for graphs too large to run client-side.

**Change:** Same pattern as centrality — fetch once, render the result list.

---

## Higher Effort

### 9. Collaborative / shared view

**Why:** Multiple users viewing the same graph could see each other's selections.

**Implementation:**
- Add WebSocket support to `ug serve` (future phase)
- Broadcast `selection_changed` events
- Show presence indicators in the UI

**API:** Not yet available. Requires server-side websocket layer.

---

### 10. Annotation layer

**Why:** Let users add notes or labels to nodes, persisted in OverGraph.

**Implementation:**
- New server route: `POST /api/annotation` (requires `--rw` flag)
- Store in a separate edge table or node property
- Render annotations as badges on nodes

**API:** Not yet available.

---

### 11. Graph diff view

**Why:** After re-indexing, see what changed: added / removed / renamed nodes.

**Implementation:**
- Server generates diff between two graph snapshots
- New route: `GET /api/graph/diff?from=<timestamp>&to=<timestamp>`
- UI: Color-code added (green), removed (red), moved (yellow) nodes

**API:** Not yet available.

---

## Feature Matrix

| Feature | API Endpoint | UI Location | Complexity |
|---------|--------------|------------|------------|
| Server-side search | `/api/graph/search` | Search tab | Low |
| Live health indicator | `/healthz` | Header | Low |
| DB node enrichment | `/api/db/node/*id` | Info panel | Low |
| K-hop exploration | `/api/graph/bfs` or `/api/db/traverse` | Info panel | Medium |
| Semantic search | `/api/search/semantic` | New tab | Medium |
| Hybrid search + snippets | `/api/search/hybrid` | Expand results | Medium |
| Precomputed centrality | `/api/graph/centrality` | Tools > Centrality | Medium |
| Cycle detection | `/api/graph/cycles` | Tools > Cycles | Medium |
| Collaborative view | (future WebSocket) | — | High |
| Annotation layer | (future `POST /api/annotation`) | — | High |
| Graph diff | (future `/api/graph/diff`) | — | High |

---

## Implementation Notes

### Feature detection

The UI can probe API availability at load time:

```js
async function detectCapabilities() {
  const caps = { db: false, semantic: false };
  try {
    const r = await fetch('/api/db/node/test');
    caps.db = r.status !== 503;
  } catch {}
  try {
    const r = await fetch('/api/search/semantic', { method: 'OPTIONS' });
    caps.semantic = r.status !== 404 && r.status !== 405;
  } catch {}
  return caps;
}
```

Use `caps` to show / hide UI elements (e.g., hide "Semantic" tab if unavailable).

### Graceful fallthrough

For all server-dependent features:
1. Try the API call
2. If 503 or network error, fall back to client-side implementation
3. Log a debug message (can surface in dev mode)

### Performance

- Pre-compressed responses: Server already sends brotli/gzip for `/graph.json`. The UI receives compressed bytes automatically.
- Caching: `/api/graph/centrality` and `/api/graph/cycles` are cached server-side per snapshot. First call is slow (~seconds), subsequent calls are instant. Consider showing a spinner on first load.

### Server flags relevant to UI

- `--watch`: When the server reloads the graph, the UI should re-fetch `/graph.json` (or the client can poll `/healthz`).
- `--db`: If not provided, DB-dependent routes 503. UI should hide DB features gracefully.
- `--base-url`, `--model`: For semantic search; show model name in UI if connected.

---

## Open Questions

- Should the UI try to auto-update when the server's `--watch` triggers a reload? (Would need `/healthz` polling or Server-Sent Events.)
- How to surface server-side errors to the UI? (Propagate error messages from API responses into a toast / log panel.)
- Authentication? (WEB-SERVE has no auth; if exposing to LAN, consider adding `--api-key` to UI and send `Authorization: Bearer <key>` header.)