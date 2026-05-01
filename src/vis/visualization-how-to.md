# Knowledge Graph Visualization

Interactive force-directed graph visualization built with D3.js v7.

## Quick Start

### Using npx (No Install Required)

```bash
npx serve ug-out -p 8080

or

python3 -m http.server 8080 --directory ug-out
```

Then open: http://localhost:8080

## Features

- **Force-directed layout**: Physics-based node positioning
- **Interactive**: Drag nodes, zoom, pan
- **Search**: Filter nodes by name
- **Hover**: Highlight connected nodes
- **Click**: View node details
- **Export**: Save as SVG

## Node Types

Colors come from `colorMap` in `visualization.html` (see the `config` block near the top of the `<script>`). Node types not listed below render with the default slate color.

| Color | Type | Notes |
|-------|------|-------|
| Cyan | File | Source files (TS/JS/Py/Java/MD) |
| Pink | Interface | TypeScript / Java interfaces |
| Green | Function | Functions, methods, variables |
| Purple | Class | Classes |
| Orange | Dependency | npm / package.json deps |
| Gold | Config | Files classified as `config` |
| Slate (default) | Folder, Concept | Folder hierarchy nodes (`folder:<path>`) and markdown heading concepts. Add explicit entries to `colorMap` in `visualization.html` to give them their own colors. |

## URL Parameters

- `?file=graph.json` - Load custom graph (default: graph.json)

## Troubleshooting

### CORS Error

If loading local files fails, ensure you're using an HTTP server (not opening `file://` directly).

### Large Graphs

For graphs with many nodes, the simulation may take time to stabilize. Wait a few seconds for layout to settle.
