        import { ForceGraph3D, THREE, SpriteText }
            from './ug-vis.bundle.js';

        const config = {
            nodeRadius: { File: 10, Interface: 7, Function: 6, Class: 8, Dependency: 5, Config: 8, Route: 7, Constant: 5, Variable: 5, Default: 6 },
            // Two-family "ink" palette: warm oranges carry the structural
            // spine (folders, files, deps), steel blues carry code symbols —
            // mirroring the orange-trunk / blue-scatter reading of the
            // reference art (docs/node-diagram-ref.jpg).
            colorMap: {
                File: '#f26a1b',
                Folder: '#c2410c',
                Interface: '#8fb8dd',
                Function: '#5b8fc9',
                Class: '#2f5f96',
                Dependency: '#fb923c',
                Config: '#f59e0b',
                // Data, not behaviour — the palest blues in the code family,
                // so a wall of fields never out-shouts the functions on top
                // of them.
                Constant: '#a7c6e3',
                Variable: '#b8cfe6',
                // Routes are entry points, not internals — they take the
                // warm structural family rather than the code blues.
                Route: '#fbbf24',
                Default: '#7ba3c9'
            },
            relColorMap: {
                Exports: '#3a6ea5',
                Imports: '#5b8fc9',
                Extends: '#2f5f96',
                Implements: '#8fb8dd',
                Calls: '#9db5cc',
                Uses: '#c98d5e',
                Requires: '#94a3b8',
                DependsOn: '#fb923c',
                References: '#a4b8cc',
                Contains: '#e8712a',
                Overrides: '#6f9fd8'
            },
            getColor: g => config.colorMap[g] || config.colorMap.Default,
            getRelColor: r => r ? (config.relColorMap[r] || '#9aa7b4') : '#9aa7b4'
        };

        const state = {
            graph: { nodes: [], edges: [] },
            nodeFilters: new Set(),
            edgeFilters: new Set(),
            selectedNode: null,
            suggestionIndex: -1,
            currentSuggestions: [],
            capabilities: null,
            semMode: 'semantic',
            // Currently selected backend for search/traverse requests.
            // Initialized from /api/capabilities; null until that probe
            // completes — fetch helpers omit `dest` in that case so the
            // server falls back to its primary.
            semDest: null,
            semInFlight: false,
            chatInFlight: false,
            chatHistory: [],
            pathSource: null,
            pathMode: false,
            // Focus mode: isolate a node + its neighbourhood, dim the rest.
            focusNode: null,        // id of the focused node (null = off)
            focusSet: new Set(),    // node ids kept bright while focused
            focusIsolate: false,    // solo mode: hide the rest instead of dimming it
            // Solo mode (large graphs). Past SOLO_THRESHOLD elements the whole
            // graph is never drawn: `state.graph` stays complete for search,
            // filters and stats, while `state.view` holds the handful of nodes
            // actually handed to the renderer. Below the threshold the two are
            // the same object and nothing changes.
            soloOnly: false,        // forced solo: the canvas only ever shows a neighbourhood
            view: null,             // { nodes, edges } passed to Graph.graphData()
            viewIds: new Set(),     // ids currently in the view
            viewSeeds: new Set(),   // ids explicitly placed on the canvas
            viewExpanded: new Set(),// seeds whose neighbours are drawn too
            viewTruncated: 0,       // neighbours the render budget left out
            adj: null,              // Map<id, edge[]> over the full graph, built once
            _viewMerge: false,      // one-shot: next selection merges instead of replacing
            // Navigation history (visited node ids) + cursor for back/forward.
            history: [],
            historyIndex: -1,
            suppressHistory: false, // set while replaying history, to avoid re-recording
            suppressFocusReanchor: false, // set while neighbour-stepping, to keep the anchor
            neighborCursor: -1,     // index used by Tab/Shift+Tab neighbour stepping
            neighborOf: null,       // node id the cursor belongs to
            _labelDist: 340,        // world distance beyond which labels auto-hide
            showBoundary: true,     // dashed boundary box + corners + face labels
            autoSpin: false         // camera auto-rotation
        };

        // Canvas palette. Everything that has to sit *against* the 3D
        // background lives here, so the scene's ground tone and the colours
        // that recede into it can never drift apart.
        const CANVAS = {
            bg: '#0d0d10',
            fog: 0x0d0d10,
            label: 'rgba(214,219,230,0.92)',
            labelTour: 'rgba(253,186,116,0.98)',
            linkRecede: '#26262e',    // focus mode: outside the neighbourhood
            linkFar: '#1b1b21',       // tour: off the route entirely
            linkRouteDim: '#9c5f2c',  // tour: on the route, away from this stop
        };

        // The 3d-force-graph instance and its scene-decor handles.
        let Graph, width, height, selectionRing, boundaryCube, particleField;
        let _glowTex, _ringTex;
        // Highlight sets re-evaluated by the node/link style accessors.
        state.highlightNodes = new Set();
        state.highlightLinks = new Set();

        let rawData = null;

        function downloadJSON(data, filename) {
            const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url;
            a.download = filename;
            document.body.appendChild(a);
            a.click();
            document.body.removeChild(a);
            URL.revokeObjectURL(url);
        }

        function downloadIndex() {
            if (!rawData) return;
            downloadJSON(rawData, 'index.json');
        }

        function downloadGraph() {
            if (!rawData) return;
            const graph = { nodes: rawData.nodes, edges: rawData.edges };
            downloadJSON(graph, 'graph.json');
        }

        async function loadGraph() {
            const params = new URLSearchParams(window.location.search);
            const file = params.get('file') || 'graph.json';
            document.getElementById('loading').style.display = 'block';
            try {
                const response = await fetch(file);
                const data = await response.json();
                rawData = data;
                transformData(data);
                initialize();
                graphInitialized = true;
            } catch (err) {
                console.error('Failed to load graph:', err);
                document.getElementById('loading').innerHTML =
                    '<p style="color:#f87171">Failed to load graph.</p>';
            }
        }

