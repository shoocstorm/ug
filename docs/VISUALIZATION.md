# Specification: Interactive Force-Directed Relationship Graph

## Goal
Implement an interactive relationship graph using D3.js that visualizes nodes and edges. Nodes should be visually grouped by a specific attribute using color and spatial clustering.

## Technical Stack
- **Library:** D3.js (v7)
- **Environment:** Vanilla JS/HTML5
- **Rendering:** SVG

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
