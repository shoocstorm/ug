# Knowledge Graph Visualization

Interactive force-directed graph visualization built with D3.js v7.

## Quick Start

### Using npx (No Install Required)

```bash
npx serve out -p 8080
```

Then open: http://localhost:8080

### Using npm start

```bash
npm start
```

Runs `npx serve out -p 8080` — open http://localhost:8080.

### Node.js HTTP Server (Global)

```bash
# Install serve globally or locally
npm install -g serve
serve out -p 8080
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

| Color | Type |
|-------|------|
| Cyan | File |
| Pink | Interface |
| Green | Function |
| Purple | Class |

## URL Parameters

- `?file=graph.json` - Load custom graph (default: graph.json)

## Troubleshooting

### CORS Error

If loading local files fails, ensure you're using an HTTP server (not opening `file://` directly).

### Large Graphs

For graphs with many nodes, the simulation may take time to stabilize. Wait a few seconds for layout to settle.
