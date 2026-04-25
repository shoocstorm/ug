# Knowledge Graph Visualization

Interactive force-directed graph visualization built with D3.js v7.

## Quick Start

### Python HTTP Server (Recommended)

```bash
# From the output directory
cd out
python3 -m http.server 8080
```

Then open: http://localhost:8080/index.html

### Node.js HTTP Server

```bash
# Install serve globally or locally
npm install -g serve

# From the output directory
cd out
serve -p 8080
```

Then open: http://localhost:8080

### Using npx (No Install)

```bash
npx serve out -p 8080
```

### PHP Built-in Server

```bash
cd out
php -S localhost:8080
```

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
If loading local files fails, ensure you're using an HTTP server (not opening file:// directly).

### Large Graphs
For graphs with many nodes, the simulation may take time to stabilize. Wait a few seconds for layout to settle.