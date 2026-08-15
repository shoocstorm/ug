        // The renderer bundles (three.js, cosmos.gl) are imported lazily by
        // whichever backend mounts — see 10-render-core.js. Loading both up
        // front would cost every page 1.4 MB of a renderer it may never use.

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
            // Which renderer backend draws the canvas: 'three' (3D force
            // graph) or 'cosmos' (2D, GPU-simulated). Resolved against the
            // registered backends in pickRendererName() — see 10-render-core.js.
            renderer: null,
            // Node name labels. Off by default — past a few hundred nodes a
            // full set of names is a wall of text rather than a map, and the
            // shape of the graph is what the canvas is for. Toggled from the
            // viewbar; honoured by both renderers.
            showLabels: false,
            nodeFilters: new Set(),
            edgeFilters: new Set(),
            // "Only system boundaries." A separate axis from nodeFilters
            // because boundary-ness is orthogonal to node type — and named
            // nothing like `showBoundary`, which is the bounding-box overlay.
            boundaryFilter: false,
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
            view: null,             // { nodes, edges } handed to the renderer
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
            // Off by default: the cube and its face digits are orientation
            // furniture, and they cut across the graph on every first look.
            // The viewbar's Box button brings them back.
            showBoundary: false,    // dashed boundary box + corners + face labels
            autoSpin: false,        // camera auto-rotation
            // Graph Walk demo (Discover → Walk). A BFS frontier that lights
            // up hop by hop on the canvas. Mutually exclusive with tour/focus
            // styling — `walkActive` gates every render accessor it touches.
            walkActive: false,
            walkRunning: false,     // true while the hop-by-hop reveal is in flight
            walkSeed: null,         // seed node id
            walkHops: 3,            // hop radius
            walkDir: 'outbound',    // 'outbound' | 'inbound' | 'both'
            walkSpeed: 1,           // 0.5 | 1 | 2 — scales the per-hop reveal pace
            walkEdgeTypes: null,    // Set of edge rels to follow, or null = all
            walkReached: new Set(), // node ids reached so far (progressive reveal)
            walkColors: new Map(),  // id → hop hex colour (drives nodeColorFor)
            walkEdgeKeys: new Set() // unordered "src|tgt" keys of walked edges
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
            // Hover highlight, split by direction. Which end of an edge you
            // are on is the whole question when reading a call graph, and the
            // particles alone can't answer it — they crawl source→target
            // either way, and from most camera angles that reads as motion,
            // not as direction. So outgoing keeps the warm highlight the rest
            // of the app uses for "this node", and incoming takes a cyan that
            // no other ink on the canvas occupies (the code family is a much
            // duller steel blue, the structural family is orange).
            linkOut: '#f96716',
            linkIn: '#22d3ee',
            particleOut: '#ff3d00',
            particleIn: '#67e8f9',
        };

        // Canvas size, shared by the renderer backends and the resize handler.
        let width, height;
        // Highlight sets re-evaluated by the node/link style accessors.
        state.highlightNodes = new Set();
        state.highlightLinks = new Set();
        // edge object → 'in' | 'out', relative to the hovered node. Kept
        // alongside the set rather than replacing it so the plain "is this
        // link hot?" question stays a single lookup.
        state.highlightLinkDir = new Map();

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
            state.graphFile = file;
            const loading = document.getElementById('loading');
            loading.style.display = 'block';
            loading.innerHTML = `
                <div class="loader"></div>
                <p class="load-phase" id="load-phase">Connecting…</p>
                <div class="load-progress" id="load-progress" hidden><div class="load-progress-bar" id="load-progress-bar"></div></div>
            `;
            const setPhase = (text, pct) => {
                const phase = document.getElementById('load-phase');
                if (phase) phase.textContent = text;
                const bar = document.getElementById('load-progress');
                if (bar) bar.hidden = false;
                const fill = document.getElementById('load-progress-bar');
                if (fill) fill.style.width = (pct == null ? 24 : pct) + '%';
            };
            // A dead graph load used to be a terminal wall of text no one
            // could act on. Turn the two dead ends into live paths: retry the
            // same graph, or drop back to the knowledge-base manager.
            const showFailure = (message) => {
                loading.innerHTML = `
                    <div class="load-error-card">
                        <div class="load-error-title">Could not load the graph</div>
                        <div class="load-error-msg">${escapeHtml(message)}</div>
                        <div class="load-error-file">${escapeHtml(file)}</div>
                        <div class="load-error-actions">
                            <button type="button" class="load-error-btn" id="load-retry">Retry</button>
                            ${isMultiMode ? '<button type="button" class="load-error-btn" id="load-back-kb">Back to knowledge bases</button>' : ''}
                        </div>
                    </div>`;
                document.getElementById('load-retry').addEventListener('click', loadGraph);
                const back = document.getElementById('load-back-kb');
                if (back) back.addEventListener('click', () => {
                    loading.style.display = 'none';
                    hideKbManager();
                    showKbManager(kbCapsCache || { mode: 'multi', projects: [], active: null });
                });
            };

            try {
                const response = await fetch(file, { headers: { 'Accept-Encoding': 'identity' } });
                if (!response.ok) throw new Error(`Server answered ${response.status} ${response.statusText}`);

                // Drive the label through the phases with a real byte count
                // where we know how big the file is. Identity encoding keeps
                // `Content-Length` exact, so the bar is honest, not decorative.
                const length = parseInt(response.headers.get('Content-Length') || '0', 10);
                let data;
                if (response.body && length > 0) {
                    const reader = response.body.getReader();
                    const chunks = [];
                    let received = 0;
                    setPhase('Downloading…', 0);
                    for (;;) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        if (value) { chunks.push(value); received += value.length; }
                        const pct = Math.min(100, Math.round((received / length) * 100));
                        setPhase(`Downloading (${formatBytes(received)} of ${formatBytes(length)})…`, pct);
                    }
                    const all = new Uint8Array(received);
                    let off = 0;
                    for (const c of chunks) { all.set(c, off); off += c.length; }
                    data = JSON.parse(new TextDecoder('utf-8').decode(all));
                } else {
                    data = await response.json();
                }

                setPhase('Building graph…', 100);
                rawData = data;
                transformData(data);
                initialize();
                graphInitialized = true;
                // The URL may carry a view worth restoring (a shared link, or
                // a refreshed deep link): apply it now that the graph exists.
                // The loading overlay stays up until the renderer's first
                // painted frame (graphReveal in 10-graph-render.js) — hiding
                // it here would leave a blank canvas while the layout spins up.
                applyUrlState(readUrlState());
            } catch (err) {
                console.error('Failed to load graph:', err);
                showFailure(err.message || String(err));
            }
        }

        function formatBytes(n) {
            if (n < 1024) return `${n} B`;
            if (n < 1048576) return `${(n / 1024).toFixed(n < 10240 ? 1 : 0)} KB`;
            return `${(n / 1048576).toFixed(n < 10485760 ? 1 : 0)} MB`;
        }

