# Specification: Interactive Force-Directed Relationship Graph

## Goal
Implement an interactive relationship graph using D3.js that visualizes nodes and edges. Nodes should be visually grouped by a specific attribute using color and spatial clustering.

## Technical Stack
- **Library:** D3.js (v7)
- **Environment:** Vanilla JS/HTML5
- **Rendering:** SVG
- **Server Integration:** `ug serve` APIs for enhanced functionality

## Data Structure
The implementation should expect a JSON object with the following format:
- `nodes`: Array of objects containing `id` (string) and `group` (string/int).
- `edges`: Array of objects containing `source` (id) and `target` (id).

## Core Requirements
1. **Physics Simulation:**
   - Use `d3.forceSimulation`.
   - Include `forceLink`, `forceManyBody` (repulsion), and `forceCenter`.
   - **Clustering:** Implement a `forceCollide` to prevent node overlap and a subtle `forceX`/`forceY` to pull nodes of the same `group` toward common centers.

2. **Visual Encoding:**
   - **Nodes:** Represented as circles. Fill color must be mapped to the `group` attribute using a categorical color scale (e.g., `d3.schemeCategory10`).
   - **Edges:** Represented as lines connecting nodes.
   - **Labels:** Add text labels to each node showing its `id`.

3. **Interactivity:**
   - **Drag & Drop:** Enable nodes to be draggable (update simulation alpha on drag).
   - **Zoom/Pan:** Implement `d3.zoom` on the SVG container.
   - **Hover Effects:** Highlight the hovered node and its immediate neighbors (optional but preferred).

4. **Responsive Design:**
   - The SVG should be responsive to the window size or container dimensions.

## Implementation Steps
1. Setup SVG container and g-elements for zooming.
2. Initialize the force simulation.
3. Create edge elements, then node elements (order matters for z-index).
4. Implement the `tick` function to update positions.
5. Add drag and zoom handlers.

## Server Integration Features

The visualization can leverage the `ug serve` APIs for enhanced functionality when running with the server.

### 1. Live Graph Health Indicator

Users can see if the loaded graph is current via the server's `/healthz` endpoint:

```js
fetch('/healthz') → show "Connected" / "Stale" badge in header
```

### 2. Node Detail Enrichment from DB

When a node is selected, fetch enriched data from `/api/db/node/${encodeURIComponent(id)}`. If available, hydrate the info panel with:

- `description` (from docstring / comment)
- `metrics` (cyclomatic complexity, LOC, etc.)
- `signature`
- File snippet / context

**Fallback:** If 503 (DB not available), show only the graph.json data.

### 3. Semantic Search Panel

Users can search by intent ("oauth login flow") not just node name via `/api/search/semantic`:

```js
const res = await fetch('/api/search/semantic', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ query: "user authentication flow", k: 10 })
});
```

Render results as clickable chips with distance scores.

### 4. Hybrid Search with Snippets

Return matching nodes with code snippets from source files via `/api/search/hybrid`:

- POST to `/api/search/hybrid` with `include_snippets: true`
- Render each hit as a collapsible card: node name + file excerpt + highlight

**Note:** Requires `--repo-root` set on the server so it can read source files.

## Feature Detection

The UI can probe API availability at load time via `api/capabilities`:

Use `caps` to show/hide UI elements (e.g., hide "Semantic" tab if server is unavailable or graph data in db is not ready).

## Graceful Fallthrough

For all server-dependent features:
1. Try the API call
2. If 503 or network error, fall back to client-side implementation
3. Log a debug message (can surface in dev mode)