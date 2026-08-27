        // The renderer bundles (three.js, cosmos.gl) are imported lazily by
        // whichever backend mounts — see 10-render-core.js. Loading both up
        // front would cost every page 1.4 MB of a renderer it may never use.

        const config = {
            // Functions are deliberately large. They are the first-class
            // citizen of almost every language and the unit most code analysis
            // is actually about, so they outrank everything except the files
            // that contain them.
            nodeRadius: { File: 10, Interface: 7, Function: 9, Class: 8, Dependency: 5, Config: 8, Route: 7, Constant: 5, Variable: 5, Default: 6 },
            // Two-family "ink" palette: warm oranges carry the structural
            // spine (folders, files, deps), steel blues carry code symbols —
            // mirroring the orange-trunk / blue-scatter reading of the
            // reference art (docs/node-diagram-ref.jpg).
            //
            // Functions are the deliberate exception, in green outside both
            // families. A palette exists to group things, but the one type you
            // most often need to pick out of a crowd should not be grouped with
            // anything.
            colorMap: {
                File: '#f26a1b',
                Folder: '#c2410c',
                Interface: '#8fb8dd',
                // The one colour outside both ink families, and the only green
                // on the canvas. Functions are what most questions are about —
                // who calls this, what does this reach — so they are pulled out
                // of the steel-blue code family entirely rather than being one
                // more shade of it. Green also reads as "executable" next to
                // the passive blues of data. Paired with a hexagon in 2D, so it
                // is separable by shape as well as by hue.
                Function: '#8ac926',
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

        // The canonical order of node types: containers first, then the
        // symbols they contain, ending at functions.
        //
        // Before this there was no such thing — anything that needed an order
        // sorted by population, which put the *largest* type first. That is
        // wrong wherever the order is spatial: the Rings layout gave the
        // biggest group the innermost, shortest ring and the smallest group the
        // outermost, longest one, so the crowding was exactly inverted. This
        // reads outward the way a codebase nests, and lands the populous types
        // on the rings with the most room.
        //
        // Types absent from a graph are skipped; types not listed here sort
        // after everything named, alphabetically, so an unknown type is never
        // dropped.
        const NODE_TYPE_ORDER = [
            'Folder', 'File', 'Route', 'Config', 'Dependency',
            'Class', 'Interface', 'Concept',
            'Constant', 'Variable', 'Function',
        ];

        function nodeTypeRank(type) {
            const i = NODE_TYPE_ORDER.indexOf(type);
            return i === -1 ? NODE_TYPE_ORDER.length : i;
        }

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
            // Which 2D arrangement is showing. The 2D renderer's answer to the
            // 3D one's face projections — see COSMOS_LAYOUTS. The sunflower is
            // the default because it makes no claim: every node gets the same
            // room, so what you read off the opening screen is the graph's
            // size and its edges, not a grouping the arrangement chose for
            // you. The folder islands are one keystroke away when that
            // grouping *is* the question.
            layout2d: 'spiral',
            // Animated flow along the edges — the hover particles, the tour
            // route and the Graph Walk's travelling strands. Toggled from the
            // walk card, and honoured by both renderers.
            lineFlow: true,
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
            walkEdgeKeys: new Set(),// unordered "src|tgt" keys of walked edges
            // How the walked subgraph is *arranged* while a walk runs.
            //
            //   'flow'  — the nodes are re-laid-out as a directional cascade:
            //             one column per hop, marching the way the edges
            //             point, so the expansion reads as a flow diagram
            //             rather than as a hairball lighting up in place
            //   'graph' — leave every node where the force layout put it
            //
            // See computeWalkCascade in 18-walk.js.
            walkLayout: 'flow',
            walkCascadePos: null,      // Map<id, {x,y,z}> the cascade assigned
            walkPosSaved: null,     // Map<id, {x,y,z}> pre-walk positions, put back on exit
            walkLanes: []           // per-hop column bounds, for the on-canvas guides
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

        // Both re-read the file rather than keeping the parsed payload around.
        // Holding it meant the tab carried the graph twice for the whole
        // session — the untrimmed parse *and* the objects `transformData`
        // built from it — so that two buttons nobody presses on most visits
        // could re-serialize it. On a 162k-node repo that second copy is
        // hundreds of megabytes of permanently live objects. A download is
        // not a hot path; paying for the re-read at click time is the trade.
        async function downloadFromGraphFile(pick, filename) {
            try {
                const res = await fetch(state.graphFile);
                if (!res.ok) throw new Error(`Server answered ${res.status} ${res.statusText}`);
                downloadJSON(pick(await res.json()), filename);
            } catch (err) {
                // The old version read an in-memory copy and could not fail;
                // re-reading can, so say so somewhere rather than rejecting
                // into nothing.
                console.error(`Failed to download ${filename}:`, err);
            }
        }

        function downloadIndex() {
            return downloadFromGraphFile(d => d, 'index.json');
        }

        function downloadGraph() {
            return downloadFromGraphFile(d => ({ nodes: d.nodes, edges: d.edges }), 'graph.json');
        }

        // Trailing debounce, shared by every live search input (sidebar
        // search, walk seed, palette). In local mode a keystroke scans
        // in-memory arrays; in server mode it is an HTTP request to
        // /api/graph/search, so per-keystroke firing means a request per
        // character. `flush()` runs a pending call immediately — wired to
        // Enter, where the user has said "now" and the 200 ms wait would
        // race the pick.
        function debounceTrailing(fn, ms) {
            let timer = null, lastArgs = null;
            const debounced = (...args) => {
                lastArgs = args;
                if (timer !== null) clearTimeout(timer);
                timer = setTimeout(() => {
                    timer = null;
                    const a = lastArgs;
                    lastArgs = null;
                    fn(...a);
                }, ms);
            };
            debounced.flush = () => {
                if (timer === null) return;
                clearTimeout(timer);
                timer = null;
                const a = lastArgs;
                lastArgs = null;
                fn(...a);
            };
            // Drop a pending call without running it — for code that
            // replaces the input state outright (clear buttons, Escape).
            debounced.cancel = () => {
                if (timer === null) return;
                clearTimeout(timer);
                timer = null;
                lastArgs = null;
            };
            return debounced;
        }

        // How long a live search input waits for the typing to pause
        // before asking. Matches the URL-state sync's window.
        const SEARCH_DEBOUNCE_MS = 400;

        async function loadGraph() {
            const params = new URLSearchParams(window.location.search);
            const file = params.get('file') || 'graph.json';
            state.graphFile = file;
            const loading = document.getElementById('loading');
            graphConceal();
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
                // Which of the two graphs this page is getting. The server
                // decides from `graph.json`'s size and says so in
                // `capabilities.graph.mode`; `?gm=` overrides for testing. No
                // `graph` block — a static host, an older server, no server at
                // all — means local, which is what every release before this
                // did unconditionally.
                //
                // The branch lives here rather than at `loadGraph`'s six call
                // sites, and `?file=` still wins outright: an explicitly named
                // graph file is a request for that file.
                const gmOverride = params.get('gm');
                let mode = 'local';
                // `?file=` names a graph file to load outright. But the
                // URL-state sync used to write the *default* ('graph.json')
                // back into the URL, and this branch read that as an
                // explicit choice — pinning every reload of an interacted
                // page to local mode, whatever the server had resolved.
                // The default is not a choice; only a non-default file is.
                const fileParam = params.get('file');
                // Awaited unconditionally, even when `?file=` has already
                // settled the mode: this is also what populates
                // `state.capabilities`, and `initialize()` reads `vis.*` off it
                // to pick the renderer and decide solo mode. Fetching it only
                // on the default path left an explicitly-named graph file
                // deciding both against the built-in fallbacks. One cached
                // request either way.
                const caps = await getCapabilities();
                if (!fileParam || fileParam === 'graph.json') {
                    mode = (caps && caps.graph && caps.graph.mode) || 'local';
                }
                if (gmOverride === 'local' || gmOverride === 'server') mode = gmOverride;

                if (mode === 'server') {
                    setPhase('Loading node index…', 0);
                    // Remembered so every later server-mode response can be
                    // checked against the snapshot this page was built from.
                    state.graphToken = (caps && caps.graph && caps.graph.token) || null;
                    await loadNodeIndex(setPhase);
                    initialize();
                    graphInitialized = true;
                    applyUrlState(readUrlState());
                    return;
                }

                const response = await fetch(file);
                if (!response.ok) throw new Error(`Server answered ${response.status} ${response.statusText}`);

                // Drive the label through the phases with a real byte count
                // where we know how big the file is.
                //
                // The denominator is the *uncompressed* size, because the
                // numerator below counts decoded bytes: `getReader()` hands
                // back what the browser has already inflated, while
                // `Content-Length` describes the compressed body. Dividing one
                // by the other made the bar hit 100% about a tenth of the way
                // in and stay there.
                //
                // This used to request `Accept-Encoding: identity` to make the
                // two agree. That never worked — `Accept-Encoding` is a
                // forbidden header name, so `fetch` drops it and the response
                // arrived brotli-compressed regardless; all it did was
                // advertise an intent the browser ignored. The bar was broken
                // either way, and had it *not* been ignored it would have cost
                // a 10-20× larger download to fix a cosmetic problem.
                // `Content-Length` remains the fallback for a static host that
                // does not send ours.
                const length = parseInt(
                    response.headers.get('X-Uncompressed-Length')
                        || response.headers.get('Content-Length')
                        || '0',
                    10,
                );
                let data;
                if (response.body && length > 0) {
                    // Decoded as it arrives, so the payload exists once rather
                    // than three times over. Keeping every chunk, copying them
                    // into one `Uint8Array`, then decoding *that* to a string
                    // meant a 346 MB graph needed ~1 GB of transient buffers
                    // before `JSON.parse` had even started — on the repos big
                    // enough to show a progress bar, which is the whole set of
                    // repos this branch exists for. `text +=` builds a rope
                    // that is flattened once, at the parse.
                    const reader = response.body.getReader();
                    const decoder = new TextDecoder('utf-8');
                    let text = '';
                    let received = 0;
                    setPhase('Downloading…', 0);
                    for (;;) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        if (value) {
                            received += value.length;
                            text += decoder.decode(value, { stream: true });
                        }
                        const pct = Math.min(100, Math.round((received / length) * 100));
                        setPhase(`Downloading (${formatBytes(received)} of ${formatBytes(length)})…`, pct);
                    }
                    text += decoder.decode();
                    data = JSON.parse(text);
                } else {
                    data = await response.json();
                }

                setPhase('Building graph…', 100);
                transformData(data);
                // Released before `initialize()`, which is the peak: it builds
                // `nodeById`, the adjacency index and the filter/legend passes
                // on top of what `transformData` just allocated. `state.graph`
                // holds trimmed copies (and shares the sub-objects it kept), so
                // what goes here is the untrimmed shells, the fields the app
                // never reads, and every original edge — the parse is the one
                // copy nothing needs a second time.
                data = null;
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

        // ─── The server-mode node index ────────────────────
        //
        // Three ways to get the same index, in descending order of what they
        // cost the tab:
        //
        //   1. the binary frame, decoded in a Worker — nothing on the main
        //      thread but taking views over a buffer that was handed over
        //   2. the binary frame, decoded here — a Worker is unavailable
        //   3. the JSON index — the server is too old to serve the frame
        //
        // The fallbacks are not defensive padding: (3) is what an existing
        // deployment gets until it is upgraded, and it is the encoding the
        // binary frame is tested against.
        async function loadNodeIndex(setPhase) {
            try {
                const decoded = await fetchNodeIndexInWorker();
                if (decoded) {
                    setPhase('Building graph…', 100);
                    installNodeIndex(decodeNodeIndexFrame(decoded.buffer), decoded.slots);
                    return;
                }
            } catch (err) {
                // A worker that failed to *start* is worth one line; a worker
                // that failed to fetch is about to fail again below, loudly.
                console.warn('node index worker unavailable, decoding inline:', err && err.message);
            }

            const bin = await fetch('/api/graph/nodes.bin');
            if (bin.ok) {
                setPhase('Building graph…', 100);
                try {
                    transformSlimBinary(await bin.arrayBuffer());
                    return;
                } catch (err) {
                    console.warn('binary node index rejected, falling back to JSON:', err && err.message);
                }
            } else if (bin.status !== 404) {
                throw new Error(`Server answered ${bin.status} ${bin.statusText}`);
            }

            const res = await fetch('/api/graph/nodes');
            if (!res.ok) throw new Error(`Server answered ${res.status} ${res.statusText}`);
            setPhase('Building graph…', 100);
            transformSlim(await res.json());
        }

        // The worker's whole job: fetch the frame and build the `id → index`
        // hash table, then hand both buffers over as transferables.
        //
        // The table is the only part of the load that is real main-thread work
        // once the frame is binary — 500k probe-and-insert steps over a 4 MB
        // `Int32Array`. Everything else is a typed-array view, which is free.
        //
        // Inlined as a Blob URL because the page ships as a single assembled
        // HTML file (see build.rs): there is no sibling .js file for a Worker
        // to point at, and `ug gen` output is opened straight from disk.
        const NODE_INDEX_WORKER_SRC = `
self.onmessage = async (ev) => {
  try {
    const res = await fetch(ev.data.url);
    if (!res.ok) { self.postMessage({ ok: false, error: 'HTTP ' + res.status }); return; }
    const buffer = await res.arrayBuffer();
    const dv = new DataView(buffer);
    const count = dv.getUint32(12, true);
    let hashOff = -1, hashLen = 0, metaOff = -1, metaLen = 0;
    for (let slot = 0; slot < count; slot++) {
      const at = 16 + slot * 12;
      const kind = dv.getUint32(at, true);
      if (kind === 12) { hashOff = dv.getUint32(at + 4, true); hashLen = dv.getUint32(at + 8, true); }
      if (kind === 13) { metaOff = dv.getUint32(at + 4, true); metaLen = dv.getUint32(at + 8, true); }
    }
    if (hashOff < 0 || metaOff < 0) { self.postMessage({ ok: false, error: 'frame is missing a section' }); return; }
    const meta = JSON.parse(new TextDecoder('utf-8').decode(new Uint8Array(buffer, metaOff, metaLen)));
    const n = meta.n;
    const idHash = new Uint32Array(buffer, hashOff, hashLen / 4);
    let cap = 1024;
    while (cap < n * 2) cap *= 2;
    const mask = cap - 1;
    const slots = new Int32Array(cap).fill(-1);
    for (let i = 0; i < n; i++) {
      let k = idHash[i] & mask;
      while (slots[k] !== -1) k = (k + 1) & mask;
      slots[k] = i;
    }
    self.postMessage({ ok: true, buffer: buffer, slots: slots.buffer }, [buffer, slots.buffer]);
  } catch (e) {
    self.postMessage({ ok: false, error: String((e && e.message) || e) });
  }
};
`;

        // Resolves to `{ buffer, slots }`, or `null` if this environment has no
        // Worker to run it in (a headless harness, a locked-down embed) — which
        // is a reason to decode inline, not a reason to fail.
        function fetchNodeIndexInWorker() {
            if (typeof Worker !== 'function' || typeof Blob !== 'function' || !window.URL || !URL.createObjectURL) {
                return Promise.resolve(null);
            }
            return new Promise((resolve, reject) => {
                let url = null;
                let worker = null;
                const done = (fn, arg) => {
                    if (worker) worker.terminate();
                    if (url) URL.revokeObjectURL(url);
                    fn(arg);
                };
                try {
                    url = URL.createObjectURL(new Blob([NODE_INDEX_WORKER_SRC], { type: 'text/javascript' }));
                    worker = new Worker(url);
                } catch (err) {
                    if (url) URL.revokeObjectURL(url);
                    resolve(null);
                    return;
                }
                worker.onmessage = (ev) => {
                    const d = ev.data || {};
                    if (d.ok) done(resolve, { buffer: d.buffer, slots: new Int32Array(d.slots) });
                    else done(reject, new Error(d.error || 'node index worker failed'));
                };
                worker.onerror = (e) => done(reject, new Error(e.message || 'node index worker errored'));
                worker.postMessage({ url: new URL('/api/graph/nodes.bin', window.location.href).toString() });
            });
        }

        function formatBytes(n) {
            if (n < 1024) return `${n} B`;
            if (n < 1048576) return `${(n / 1024).toFixed(n < 10240 ? 1 : 0)} KB`;
            return `${(n / 1048576).toFixed(n < 10485760 ? 1 : 0)} MB`;
        }

