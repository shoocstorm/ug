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

        // Adjacency over the *full* graph, built once. Before this, every
        // selection scanned all edges (neighborIdsOf) — the second-worst hot
        // path on a large repo after the restyle loop.
        function buildAdjacency() {
            const adj = new Map();
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
            return (state.adj && state.adj.get(id)) || [];
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
                for (const e of edgesOf(id)) {
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
            const total = Math.max(state.graph.nodes.length, state.graph.edges.length);
            const want = total > (limit || SOLO_THRESHOLD);
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
                const { ids, truncated } = soloViewIds(state.viewSeeds, state.viewExpanded);
                setSoloView(ids, truncated);
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
        function rebuildSoloView() {
            if (!state.soloOnly) return;
            const { ids, truncated } = soloViewIds(state.viewSeeds, state.viewExpanded);
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
        // particular name in mind. Read off the adjacency map rather than
        // `metrics.degree_centrality`, which only exists on enriched graphs.
        function topHubs(n) {
            if (!state.adj) return [];
            const ranked = [];
            state.adj.forEach((edges, id) => ranked.push([id, edges.length]));
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
