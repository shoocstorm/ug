        // ─── Solo view (large graphs) ───────────────────────
        //
        // Past SOLO_THRESHOLD elements a full render is not slow-but-usable,
        // it is unusable: the force simulation, one Three.js object per node
        // and the O(N) restyle on every hover all scale with what the
        // renderer was handed. Drawing the same hairball with cheaper pixels
        // (the old "perf mode") could not fix that, because the cost is in
        // the count, not the shading.
        //
        // So above the threshold the renderer is handed a *neighbourhood*
        // instead of a graph. `state.graph` stays whole — search, filters,
        // stats, centrality and path-finding all still see every node — and
        // `state.view` carries the few hundred that are actually drawn.
        //
        // The view is derived, never edited in place:
        //
        //   state.viewSeeds     ids explicitly put on the canvas
        //   state.viewExpanded  the subset of those whose neighbours come too
        //
        // Every interaction just changes those two sets and rebuilds, which
        // is what makes the filters, the tour and "light up … in graph" fall out
        // of the same code path.

        // max(nodes, edges) above this → solo mode.
        //
        // This is the 2D renderer's ceiling, and it is about legibility rather
        // than frame rate: cosmos.gl will happily instance a million points,
        // but a million points is not a picture of anything. Past a couple of
        // hundred thousand the canvas is a solid wash whichever layout is
        // running, and every interaction on it — hover, filter, restyle —
        // is paying full price for pixels nobody can read. A neighbourhood at
        // a time is both faster and more use.
        const SOLO_THRESHOLD = 200000;
        const SOLO_MAX_NODES = 1500;      // hard render budget for one view
        const SOLO_MAX_NEIGHBORS = 300;   // per-seed 1-hop cap, so a hub can't blow the budget

        // The `vis.solo_threshold` config key from ~/.ug/config.json, surfaced
        // here via /api/capabilities. Absent/invalid → SOLO_THRESHOLD. Only the
        // 2D engine consults this: the 3D engine keeps its own ceiling
        // (THREE_D_MAX_ELEMENTS), so a (mis)setting can never hand three.js
        // more than it can draw.
        function visSoloThreshold() {
            const raw = state.capabilities && state.capabilities.vis && state.capabilities.vis.solo_threshold;
            const n = parseInt(raw, 10);
            return Number.isFinite(n) && n > 0 ? n : SOLO_THRESHOLD;
        }

        // Whether the renderer must be handed neighbourhoods rather than the
        // whole graph. The two places that decide this — `initialize()` and
        // `applySoloMode()` — must agree, and both got it wrong for server mode
        // when they compared node and edge counts directly: there are no edges
        // locally in that mode, so `max(161725, 0)` reads as *under* the
        // threshold and the "draw everything" branch hands the renderer 162k
        // nodes with nothing connecting them.
        //
        // In server mode solo is not a threshold decision at all. It is the only
        // correct view, because the edges to draw anything else are on the
        // server.
        function soloRequired(limit, three) {
            if (state.graphMode === 'server') return true;
            const is3d = three === undefined ? state.renderer === 'three' : three;
            return soloElementCount(is3d) > (limit ?? visSoloThreshold());
        }

        // What "one element" costs — which is not the same question for the
        // two renderers, and answering it with one formula is how a real
        // difference got hidden.
        //
        // cosmos.gl uploads points and links as typed-array buffers and draws
        // each set in one instanced call, so what bounds it is the larger of
        // the two. three.js builds an object per node *and* an object per
        // link and submits a draw call for each; measured across four real
        // graphs on an M5 Max, its frame time is linear in that **total**:
        //
        //   ~/.ug/ug         4,648 + 12,385 → 17,077 draw calls   44.4 fps
        //   ~/.ug/hermes    11,550 + 13,038 → 24,425              26.4
        //   ~/.ug/MemOS     12,789 + 30,501 → 43,139              12.6
        //   ~/.ug/overgraph 15,143 + 43,831 → 58,000               9.6
        //
        // `max` predicted none of it. It puts `ug` at 12,385 and `hermes` at
        // 13,038 — within 5% of each other, for graphs that run at 44 and 26
        // fps. The sum is within 1% of the draw-call count every time.
        function soloElementCount(is3d) {
            const n = state.nodeCount || 0;
            const e = state.edgeCount || 0;
            return is3d ? n + e : Math.max(n, e);
        }

        // Short human phrase for *why* solo view is on. The graph title
        // shows it, because "solo view" with no reason reads as a bug
        // rather than a decision, and the reason is the one fact that
        // tells the user which setting to touch: server mode is the Graph
        // section's threshold; an engine ceiling is the Visualization
        // section's.
        function soloReasonText() {
            if (state.graphMode === 'server') return 'server mode: edges live on the server';
            const three = state.renderer === 'three';
            const limit = three ? threeDMaxElements() : visSoloThreshold();
            return `past the ${three ? "3D engine's element budget" : "2D engine's solo threshold"} of ${formatNumber(limit)} elements`;
        }

        // Adjacency over the *full* graph, built once. Before this, every
        // selection scanned all edges (neighborIdsOf) — the second-worst hot
        // path on a large repo after the restyle loop.
        // In server mode this starts empty and fills on demand — `state.adj`
        // becomes a cache of the neighbourhoods fetched so far rather than a
        // complete index. `state.adjComplete` is what tells the two apart, and
        // it is the single most important thing in this file:
        //
        //   in state.adj only        → *some* of this node's edges are here,
        //                              because a neighbour's fetch brought them
        //   in state.adjComplete     → *all* of this node's edges are here
        //
        // Without that distinction `edgesOf` cannot tell "this node has no
        // edges" from "nobody has asked yet", and both answer `[]`. The first
        // is a fact; the second is a wrong picture drawn with no error.
        // `state.adjCompleteAll` short-circuits it in local mode, where every
        // edge is known up front and every id is trivially complete.
        function buildAdjacency() {
            const adj = new Map();
            state.adjComplete = new Set();
            state.adjCompleteAll = false;
            if (state.graphMode === 'server') {
                state.adj = adj;
                state.adjKeys = new Map();
                state.adjPending = new Map();
                return;
            }
            state.adjKeys = null;   // local mode never re-pushes the same edge
            state.adjCompleteAll = true;
            // Local mode's adjacency *is* the edge store's CSR index, built
            // with the columns in `transformData`. This used to be a `Map` of
            // 485k arrays holding 4.5M references to 2.2M edge objects — 254 MB
            // on `~/.ug/big500k`, against 40 MB for the columns (P12.25).
            //
            // Left null rather than empty on purpose. `knownEdgesOf` is the
            // only reader, and a reader that is added later and forgets the
            // store should throw here rather than quietly answer "no edges" —
            // which is the distinction this whole file is built around, and
            // not one an empty `Map` can make.
            state.adj = null;
        }

        function edgesOf(id) {
            // A cold read in server mode is a bug in the caller — some entry
            // point forgot to `await ensureEdges` — and the damage is a node
            // drawn as isolated rather than an error anyone would notice. So
            // say so, and repair it: fetch the neighbourhood and redraw once it
            // lands. The answer is late instead of wrong.
            if (!state.adjCompleteAll && !state.adjComplete.has(id)) {
                if (!coldMissWarned.has(id)) {
                    coldMissWarned.add(id);
                    console.warn(`edgesOf(${id}) before its edges were fetched — repairing`);
                }
                ensureEdges([id]).then(() => { rebuildSoloView(); bumpGraphStyles(); });
            }
            return knownEdgesOf(id);
        }
        const coldMissWarned = new Set();

        // Whether a server-mode response describes the same graph the page
        // loaded. Server mode splits one graph across many requests and refers
        // to nodes by *position*, so a `ug gen` landing mid-session does not
        // give stale answers — it gives answers indexed into a different array.
        // Dropping the response and saying so is the only honest move; the
        // page cannot repair itself without reloading.
        function graphTokenMatches(token) {
            if (!token || !state.graphToken || token === state.graphToken) return true;
            if (!state.graphTokenWarned) {
                state.graphTokenWarned = true;
                console.warn('graph changed on the server — reload to see it');
                const chip = document.getElementById('view-count');
                if (chip) {
                    chip.hidden = false;
                    chip.innerHTML = '<span class="vc-count">Graph changed on disk</span>'
                        + '<span class="vc-note">reload the page to see the new index</span>';
                }
            }
            return false;
        }

        // What the cache holds for `id`, with no opinion about completeness.
        //
        // The distinction matters for exactly one caller: `setSoloView` walks
        // every id in the view looking for edges *between* them, and the
        // induced fetch has already supplied precisely those. The neighbours
        // are legitimately incomplete there — asking `edgesOf` would report a
        // cold miss on every one of them and re-enter the rebuild forever.
        function knownEdgesOf(id) {
            // Local mode: built from the columns, per call. Server mode: the
            // cache of what has been fetched so far.
            const store = state.edgeStore;
            if (store) {
                const node = state.nodeById && state.nodeById.get(id);
                return node ? store.edgesOfIndex(node._i) : EMPTY_LIST;
            }
            return (state.adj && state.adj.get(id)) || [];
        }

        // Fetch the edges around `ids` into `state.adj`, skipping whatever is
        // already known. Resolves immediately in local mode, where every edge
        // arrived with the graph.
        //
        // Two scopes, and the difference is the whole correctness story:
        //
        //   'incident' — every edge touching each id. Marks those ids complete.
        //   'induced'  — only edges with *both* ends in the set. Fills in the
        //                cross-links between nodes already on the canvas, and
        //                marks nothing complete, because it deliberately
        //                withheld the edges that leave the set.
        //
        // In-flight requests are shared through `state.adjPending` so a burst of
        // clicks on the same node makes one request, not one per click.
        async function ensureEdges(ids, scope = 'incident') {
            if (state.adjCompleteAll || state.graphMode !== 'server') return;
            const store = state.nodeStore;
            if (!store) return;

            // Deduped and sorted before the key is built: callers routinely
            // pass overlapping sets (`[...viewSeeds, ...viewExpanded]` names
            // most ids twice), and without this the same node is requested
            // twice under two different keys, defeating the in-flight sharing
            // immediately below.
            const want = [];
            const seen = new Set();
            for (const id of ids) {
                if (seen.has(id)) continue;
                seen.add(id);
                if (scope === 'incident' && state.adjComplete.has(id)) continue;
                const i = store.indexOf(id);
                if (i >= 0) want.push(i);
            }
            if (!want.length) return;
            want.sort((a, b) => a - b);

            const key = scope + ':' + want.join(',');
            let pending = state.adjPending.get(key);
            if (!pending) {
                pending = fetchEdges(want, scope).finally(() => state.adjPending.delete(key));
                state.adjPending.set(key, pending);
            }
            await pending;
        }

        async function fetchEdges(indices, scope) {
            const res = await fetch('/api/graph/edges', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ ids: indices, scope }),
            });
            if (!res.ok) throw new Error(await readErr(res));
            const data = await res.json();
            if (!graphTokenMatches(data.token)) return;
            // The wire speaks positions, and what the adjacency cache stores is
            // *ids* — so this resolves indices to ids, not to node objects.
            //
            // That distinction is worth a lot on a hub. A node of degree 8,680
            // brings back 8,680 edges, of which the canvas will draw at most
            // `SOLO_MAX_NEIGHBORS`; materialising a node object for every
            // endpoint built ~8.7k objects to throw ~8.4k of them away, and the
            // garbage showed up as a pause on the *next* await. `setSoloView`
            // materialises the few hundred that actually get drawn, through the
            // store, as it always did.
            //
            // The ids are memoised per response so that a hub appearing in
            // 8,680 edges yields one string rather than 8,680 copies of the
            // same 141 characters — the adjacency lists and the view compare
            // these, and identical strings compare by pointer first.
            const store = state.nodeStore;
            const { src, tgt, rel, relTypes } = data;
            const idMemo = new Map();
            const idOf = (i) => {
                let v = idMemo.get(i);
                if (v === undefined) {
                    v = i >= 0 && i < store.nodeCount ? store.idAt(i) : null;
                    idMemo.set(i, v);
                    // The store would otherwise have to hash this id back to
                    // the index we already have, the first time anything looks
                    // the node up.
                    if (v !== null) store.noteIndex(v, i);
                }
                return v;
            };

            for (let k = 0; k < src.length; k++) {
                const s = idOf(src[k]);
                const t = idOf(tgt[k]);
                if (!s || !t) continue;
                // One object per edge, pushed into both endpoints' lists —
                // `setSoloView` dedupes by object identity (`seen.has(e)`), so
                // two objects for one edge would draw two strands.
                const edge = { source: s, target: t, rel: relTypes[rel[k]] || null };
                // The dedupe key is built from the *wire indices*, not the ids.
                // Same identity, ~20 characters instead of ~290: on a node of
                // degree 8,680 that is 17k short strings to hash rather than
                // 17k long ones, which is most of what filling a hub's
                // adjacency costs.
                const key = src[k] + '|' + tgt[k] + '|' + rel[k];
                pushEdge(s, edge, key);
                if (t !== s) pushEdge(t, edge, key);
            }
            for (const i of data.complete || []) {
                const id = idOf(i);
                if (id) state.adjComplete.add(id);
            }
        }

        // Append without duplicating: a node's list is filled by several
        // fetches (its own incident query, plus induced queries from every
        // neighbourhood it appears in), and the same edge can arrive twice.
        //
        // Through a `Set` of keys, not a scan of the list. The scan was O(d) per
        // edge and therefore O(d²) to fill one node's adjacency — and `d` is not
        // small: the real `~/.ug/neo4j` graph has a node of degree 8,680, so one
        // click on it compared 141-character strings roughly 38 million times,
        // on the main thread, before anything was drawn.
        function pushEdge(id, edge, key) {
            let seen = state.adjKeys && state.adjKeys.get(id);
            if (!seen && state.adjKeys) {
                seen = new Set();
                state.adjKeys.set(id, seen);
            }
            if (seen) {
                if (seen.has(key)) return;
                seen.add(key);
            }
            const list = state.adj.get(id);
            if (list) list.push(edge);
            else state.adj.set(id, [edge]);
        }

        // The end of `e` that isn't `id`. Handles both shapes: edges the
        // renderer has claimed carry node objects, the pristine ones in
        // state.graph.edges carry ids.
        function otherEnd(e, id) {
            const s = e.source.id || e.source;
            return s === id ? (e.target.id || e.target) : s;
        }

        // Ids one hop from `id`. `filtered` applies the node/edge type chips,
        // so the render budget is spent on nodes the user can actually see.
        function neighborsOf(id, opts = {}) {
            const out = [];
            for (const e of edgesOf(id)) {
                if (opts.filtered && state.linkHidden && state.linkHidden(e)) continue;
                const other = otherEnd(e, id);
                if (other === id) continue;
                if (opts.filtered && state.nodeFilterActive && state.nodeHidden) {
                    const n = state.nodeById && state.nodeById.get(other);
                    if (n && state.nodeHidden(n)) continue;
                }
                out.push(other);
            }
            return out;
        }

        // Resolve seeds + expansions into the id set to draw. Seeds are kept
        // even when their own type is filtered out — you asked for that node
        // specifically, and silently dropping it reads as a broken click.
        function soloViewIds(seeds, expanded) {
            const ids = new Set();
            let truncated = 0;   // distinct nodes the budget left out
            seeds.forEach(id => {
                if (!state.nodeById.has(id)) return;
                if (ids.size < SOLO_MAX_NODES) ids.add(id);
                else truncated++;
            });
            expanded.forEach(id => {
                if (!ids.has(id)) return;
                const seen = new Set();
                let added = 0;
                for (const other of neighborsOf(id, { filtered: true })) {
                    // A pair can be joined by several edges, and neighbourhoods
                    // overlap; neither should spend the budget twice.
                    if (seen.has(other) || ids.has(other)) { seen.add(other); continue; }
                    seen.add(other);
                    if (added >= SOLO_MAX_NEIGHBORS || ids.size >= SOLO_MAX_NODES) {
                        truncated++;
                        continue;
                    }
                    ids.add(other);
                    added++;
                }
            });
            return { ids, truncated };
        }

        // Hand a set of ids to the renderer as a standalone little graph.
        //
        // The edges are *clones*: 3d-force-graph rewrites source/target into
        // node references on whatever array it is given, and state.graph.edges
        // has to stay in its id form so the next rebuild can read it again.
        function setSoloView(ids, truncated = 0) {
            const nodes = [];
            ids.forEach(id => {
                const n = state.nodeById.get(id);
                if (n) nodes.push(n);
            });

            const edges = [];
            const seen = new Set();
            ids.forEach(id => {
                // `knownEdgesOf`, not `edgesOf`: this wants the edges *between*
                // the view's nodes, which the induced fetch has already
                // supplied. Most of these ids are neighbours whose full lists
                // were deliberately not fetched, and demanding completeness
                // here would report a cold miss on every one of them.
                for (const e of knownEdgesOf(id)) {
                    // Every edge is in two adjacency lists. Server mode pushes
                    // one object into both, so identity is the key there;
                    // local mode builds a fresh object per read, so the key is
                    // the store's edge index. Both go in the same `Set` —
                    // a number and an object never collide.
                    const key = e._i === undefined ? e : e._i;
                    if (seen.has(key)) continue;
                    seen.add(key);
                    const s = e.source.id || e.source;
                    const t = e.target.id || e.target;
                    if (!ids.has(s) || !ids.has(t)) continue;
                    if (state.linkHidden && state.linkHidden(e)) continue;
                    edges.push({ source: s, target: t, rel: e.rel });
                }
            });

            state.view = { nodes, edges };
            state.viewIds = ids;
            state.viewTruncated = truncated;
            if (!activeRenderer()) return;
            setGraphData(state.view);
            // Let the existing settle-then-frame path (onEngineStop → autoFrame)
            // re-fit the camera around the new, differently sized view.
            state._didFit = false;
            state._boxSettled = false;
            updateSoloHud();
            // The legend counts what is on screen, and in solo mode that just
            // changed. The renderers' overlay loops re-read it on a throttle
            // too, but a click should not wait for the next tick.
            refreshModeLegend();
            bumpGraphStyles();
        }

        // Turn solo mode on or off for the element budget the *mounted*
        // renderer can actually draw whole.
        //
        // The threshold is not a property of the graph, it is a property of the
        // renderer: cosmos.gl instances a hundred thousand points happily,
        // three.js builds a Group of five objects per node and dies long
        // before that. So a graph that renders whole in 2D can be far past what
        // 3D can hold, and switching renderers has to re-decide — otherwise
        // 2D → 3D on a large graph hands three.js the entire hairball and the
        // tab stops responding.
        //
        // Returns true if the mode changed. Safe to call before a renderer is
        // mounted, and that safety is load-bearing rather than incidental:
        // `createGraph` mounts with `state.view` on the next statement, so
        // both branches below set it **synchronously** before returning.
        function applySoloMode(limit, three) {
            const want = soloRequired(limit, three);
            if (want === state.soloOnly) return false;
            state.soloOnly = want;
            document.body.classList.toggle('solo-only', want);

            if (want) {
                // Carry the selection in as the first seed, so a renderer
                // switch lands on the node you were already looking at rather
                // than on a blank canvas with no explanation.
                state.viewSeeds = new Set();
                state.viewExpanded = new Set();
                if (state.selectedNode && state.nodeById.has(state.selectedNode.id)) {
                    state.viewSeeds.add(state.selectedNode.id);
                    state.viewExpanded.add(state.selectedNode.id);
                }
                setupSoloEmptyState();
                // The view has to be right *now*, not when the rebuild lands.
                //
                // `createGraph` calls this and then, with no `await` between,
                // reads `state.view` to mount the renderer with — while
                // `rebuildSoloView` below is `async` and does not touch
                // `state.view` until its first microtask. So the renderer was
                // handed the graph solo mode had just decided it must not
                // draw: on `~/.ug/ug`, three.js mounted all 4,648 nodes and
                // 12,385 links against a 3,000-element budget, at 10 fps.
                //
                // And silently, because everything downstream reads
                // `state.view` and therefore saw the empty one it was left
                // with: `updateAdaptiveLabels` iterated nothing and left all
                // 4,648 name sprites visible, and `computeExtent` returned
                // null so `applyDepthCues` never recalibrated the fog — which
                // stayed at its mount default of 0.001 against a near-black
                // fog colour, and buried the graph in the dark at any camera
                // distance past a few hundred units.
                state.view = { nodes: [], edges: [] };
                state.viewIds = new Set();
                state.viewTruncated = 0;
                // Fire and forget, like every other `rebuildSoloView` caller:
                // this returns a boolean about the *mode*, and the view it
                // leaves behind is repainted whenever its edges land.
                rebuildSoloView();
            } else {
                // Back to the whole graph.
                state.view = state.graph;
                state.viewIds = new Set(state.graph.nodes.map(n => n.id));
                state.viewTruncated = 0;
                const chip = document.getElementById('view-count');
                if (chip) chip.hidden = true;
                const empty = document.getElementById('canvas-empty');
                if (empty) empty.hidden = true;
            }
            state._didFit = false;
            state._boxSettled = false;
            updateSoloHud();
            updateGraphTitle();
            refreshModeLegend();
            syncSoloButton();
            return true;
        }

        // Re-derive the view from the current seeds under the current filters.
        //
        // This is the async boundary for the whole server-mode design, and it
        // was chosen because it is the *narrow* one: four callers, none of which
        // uses a return value, against `handleClick`'s eighteen. So it became
        // async and every caller fires and forgets — `soloViewIds`,
        // `setSoloView`, `neighborsOf` and `edgesOf` stay synchronous and
        // unchanged, which is what keeps local mode identical.
        //
        // Two fetches, in this order, both bounded:
        //
        //   1. the seeds' *incident* edges — needed to know who the neighbours
        //      even are, bounded by seed degree
        //   2. the resulting set's *induced* edges — the cross-links between
        //      neighbours, without which the picture is a star rather than a
        //      neighbourhood. Bounded by SOLO_MAX_NODES.
        //
        // A monotonic token drops stale responses: clicking three nodes quickly
        // must leave the canvas showing the third, not whichever fetch happened
        // to finish last.
        let soloRebuildToken = 0;
        async function rebuildSoloView() {
            if (!state.soloOnly) return;
            const token = ++soloRebuildToken;

            await ensureEdges([...state.viewSeeds, ...state.viewExpanded]);
            if (token !== soloRebuildToken) return;

            const { ids, truncated } = soloViewIds(state.viewSeeds, state.viewExpanded);
            await ensureEdges([...ids], 'induced');
            if (token !== soloRebuildToken) return;

            setSoloView(ids, truncated);
        }

        // The single funnel every selection goes through (see handleClick).
        //
        //   plain pick                  → replace the canvas with that node
        //   ⌘/Ctrl-click, or a node     → add it, keeping what's already there
        //   already on the canvas
        function showInView(d) {
            if (!state.soloOnly || !d || !state.nodeById.has(d.id)) return;
            const merge = state._viewMerge || state.viewIds.has(d.id);
            state._viewMerge = false;
            if (!merge) {
                state.viewSeeds = new Set();
                state.viewExpanded = new Set();
            }
            state.viewSeeds.add(d.id);
            state.viewExpanded.add(d.id);
            rebuildSoloView();
        }

        // "light up … in graph": draw a whole set at once, with only the edges
        // *between* them. No 1-hop expansion — fifty hits each pulling in a
        // neighbourhood is the hairball this mode exists to avoid. Clicking
        // any of them afterwards expands that one.
        function plotNodes(ids) {
            if (!state.soloOnly) return;
            const wanted = Array.from(ids).filter(id => state.nodeById.has(id));
            if (!wanted.length) return;
            state.viewSeeds = new Set(wanted);
            state.viewExpanded = new Set();
            rebuildSoloView();
        }

        // "light up … in graph" — focus a whole result set at once. In solo
        // mode the nodes must be drawn first (none is on the canvas yet); below
        // the threshold they are all already drawn, so we just dim the rest and
        // frame the set. Capped because lighting thousands at once is noise,
        // not signal — the button label shows the capped count.
        function lightUpNodes(ids) {
            if (!state.nodeById) return;
            const found = Array.from(ids).map(id => state.nodeById.get(id)).filter(Boolean);
            if (!found.length) return;
            const capped = found.slice(0, SOLO_MAX_NODES);
            if (state.soloOnly) plotNodes(capped.map(n => n.id));
            state.focusNode = capped[0].id;
            state.focusSet = new Set(capped.map(n => n.id));
            document.body.classList.add('focus-active');
            // Without this the Solo button stays greyed out even though there
            // is now a focus to solo.
            syncSoloButton();
            bumpGraphStyles();
            focusNode(capped[0]);
        }

        // ─── Chrome: the empty state and the "what's on screen" chip ───

        function updateSoloHud() {
            if (!state.soloOnly) return;
            const shown = state.view ? state.view.nodes.length : 0;
            const total = state.nodeCount || 0;

            const chip = document.getElementById('view-count');
            if (chip) {
                chip.hidden = shown === 0;
                const parts = [`${formatNumber(shown)} of ${formatNumber(total)} nodes`];
                if (state.viewTruncated > 0) {
                    parts.push(`${formatNumber(state.viewTruncated)} more connected — narrow the type filters to see them`);
                }
                chip.innerHTML = `<span class="vc-count">${escapeHtml(parts[0])}</span>` +
                    (parts[1] ? `<span class="vc-note">${escapeHtml(parts[1])}</span>` : '');
            }

            const empty = document.getElementById('canvas-empty');
            if (empty) empty.hidden = shown > 0;
        }

        // Fill in the guidance overlay and wire its shortcuts. Called from
        // initialize(), and again by applySoloMode when a renderer switch drops
        // the page into solo mode — hence the guard: everything below is
        // derived from `state.graph`, which does not change, and re-running it
        // would stack a second set of click handlers on every button.
        function setupSoloEmptyState() {
            const empty = document.getElementById('canvas-empty');
            if (!empty || empty.dataset.wired === '1') return;
            empty.dataset.wired = '1';

            const countEl = empty.querySelector('.ce-count');
            if (countEl) countEl.textContent = formatNumber(state.nodeCount || 0);

            const search = empty.querySelector('.ce-search-btn');
            if (search) search.addEventListener('click', focusSearchInput);

            const hubs = empty.querySelector('.ce-hubs');
            const hubsLabel = empty.querySelector('.ce-hubs-label');
            if (hubs) {
                const top = topHubs(3);
                if (!top.length) {
                    hubs.hidden = true;
                    if (hubsLabel) hubsLabel.hidden = true;
                } else {
                    top.forEach(n => {
                        const b = document.createElement('button');
                        b.type = 'button';
                        b.className = 'ce-hub';
                        b.title = n.id;
                        b.innerHTML = `${nodeIconSvg(n.group)}<span>${escapeHtml(truncateName(n.name))}</span>`;
                        b.addEventListener('click', () => {
                            handleClick(null, n);
                            focusNode(n);
                        });
                        hubs.appendChild(b);
                    });
                }
            }
            empty.hidden = false;
        }

        // The most-connected nodes, as somewhere to start when you have no
        // particular name in mind. Read off `state.degreeOf` rather than
        // `metrics.degree_centrality`, which only exists on enriched graphs.
        //
        // Not off `state.adj`: in server mode that map is a *cache* of the
        // neighbourhoods fetched so far, so ranking it would rank whatever
        // happened to have been clicked — and rank it highest on the very first
        // screen, where nothing has been clicked at all.
        function topHubs(n) {
            return topByDegree(n);
        }

        // The empty-canvas card's "search for one" action. There is one bar
        // and one tab it lives on, so this is now a single call.
        function focusSearchInput() {
            const sidebar = document.getElementById('sidebar');
            if (sidebar) sidebar.classList.remove('collapsed');
            focusAsk(null, 'names');
        }

        // Behind every "light up … in graph" button. Works in both modes:
        // solo draws the set fresh; normal mode dims the rest and frames it.
        // Capped because thousands of lit nodes is noise.
        function syncPlotAllButton(btn, ids) {
            if (!btn) return;
            if (!ids.length) {
                btn.hidden = true;
                return;
            }
            const capped = Math.min(ids.length, SOLO_MAX_NODES);
            btn.hidden = false;
            btn.textContent = `⊞ light up ${formatNumber(capped)} in graph`;
            btn.title = capped < ids.length
                ? `Light up the first ${formatNumber(capped)} of ${formatNumber(ids.length)} matches`
                : 'Light up every match, dimming the rest of the graph';
        }
