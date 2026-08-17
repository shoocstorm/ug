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
        function soloRequired(limit) {
            if (state.graphMode === 'server') return true;
            return Math.max(state.graph.nodes.length, state.edgeCount) > (limit || SOLO_THRESHOLD);
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
                state.adjPending = new Map();
                return;
            }
            state.adjCompleteAll = true;
            const push = (id, e) => {
                const list = adj.get(id);
                if (list) list.push(e);
                else adj.set(id, [e]);
            };
            state.graph.edges.forEach(e => {
                const s = e.source.id || e.source;
                const t = e.target.id || e.target;
                push(s, e);
                if (t !== s) push(t, e);
            });
            state.adj = adj;
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
            const idx = state.slimIndexOf;
            if (!idx) return;

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
                const i = idx.get(id);
                if (i !== undefined) want.push(i);
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
            const nodes = state.graph.nodes;
            const { src, tgt, rel, relTypes } = data;

            for (let k = 0; k < src.length; k++) {
                const s = nodes[src[k]];
                const t = nodes[tgt[k]];
                if (!s || !t) continue;
                // One object per edge, pushed into both endpoints' lists —
                // `setSoloView` dedupes by object identity (`seen.has(e)`), so
                // two objects for one edge would draw two strands.
                const edge = { source: s.id, target: t.id, rel: relTypes[rel[k]] || null };
                pushEdge(s.id, edge);
                if (t.id !== s.id) pushEdge(t.id, edge);
            }
            for (const id of data.complete || []) {
                const node = nodes[id];
                if (node) state.adjComplete.add(node.id);
            }
        }

        // Append without duplicating: a node's list is filled by several
        // fetches (its own incident query, plus induced queries from every
        // neighbourhood it appears in), and the same edge can arrive twice.
        function pushEdge(id, edge) {
            let list = state.adj.get(id);
            if (!list) { state.adj.set(id, [edge]); return; }
            for (const e of list) {
                if (e.source === edge.source && e.target === edge.target && e.rel === edge.rel) return;
            }
            list.push(edge);
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
                if (opts.filtered && state.nodeHidden) {
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
                    if (seen.has(e)) continue;   // every edge is in two adjacency lists
                    seen.add(e);
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
        // mounted: it leaves `state.view` correct for whoever mounts next.
        function applySoloMode(limit) {
            const want = soloRequired(limit);
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
            const total = state.graph.nodes.length;

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
            if (countEl) countEl.textContent = formatNumber(state.graph.nodes.length);

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
            const degree = state.degreeOf;
            if (!degree || !degree.size) return [];
            const ranked = [...degree.entries()];
            ranked.sort((a, b) => b[1] - a[1]);
            const out = [];
            for (const [id] of ranked) {
                const node = state.nodeById.get(id);
                if (node) out.push(node);
                if (out.length >= n) break;
            }
            return out;
        }

        // Put the cursor in the keyword search box. The button in the empty
        // state is useless if it focuses something the user cannot see, and
        // the box is four levels down: the sidebar, the Discover tab, its
        // Search sub-tab, the Search section, then the Keyword mode. The two
        // tab bars are opened by clicking their buttons rather than by
        // reaching into wirePanelTabs' closures.
        function focusSearchInput() {
            const sidebar = document.getElementById('sidebar');
            if (sidebar) sidebar.classList.remove('collapsed');
            const tab = document.querySelector('.panel-tab[data-tab="discover"]');
            if (tab && !tab.classList.contains('active')) tab.click();
            const sub = document.querySelector('.subtab[data-sub="search"]');
            if (sub && !sub.classList.contains('active')) sub.click();
            const section = document.getElementById('section-semantic');
            if (section) section.classList.remove('collapsed');
            if (state.semMode !== 'keyword') selectSemMode('keyword');
            const input = document.getElementById('search');
            if (input) { input.focus(); input.select(); }
        }

        // Shared by the two "light up … in graph" buttons (keyword + semantic).
        // Works in both modes: solo draws the set fresh; normal mode dims the
        // rest and frames it. Capped because thousands of lit nodes is noise.
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
