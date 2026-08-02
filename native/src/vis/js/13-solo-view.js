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
        // is what makes the filters, the tour and "Plot all results" fall out
        // of the same code path.

        const SOLO_THRESHOLD = 10000;     // max(nodes, edges) above this → solo mode
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
            if (!Graph) return;
            Graph.graphData({ nodes, links: edges });
            // Let the existing settle-then-frame path (onEngineStop → autoFrame)
            // re-fit the camera around the new, differently sized view.
            state._didFit = false;
            state._boxSettled = false;
            updateSoloHud();
            bumpGraphStyles();
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

        // "Plot all results": draw a whole set at once, with only the edges
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

        // Fill in the guidance overlay and wire its shortcuts. Called once,
        // from initialize(), and only in solo mode.
        function setupSoloEmptyState() {
            const empty = document.getElementById('canvas-empty');
            if (!empty) return;

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

        // Shared by the two "Plot all results" buttons (keyword + semantic).
        // Hidden entirely outside solo mode: with the whole graph on screen
        // there is nothing to plot.
        function syncPlotAllButton(btn, ids) {
            if (!btn) return;
            if (!state.soloOnly || !ids.length) {
                btn.hidden = true;
                return;
            }
            const capped = Math.min(ids.length, SOLO_MAX_NODES);
            btn.hidden = false;
            btn.textContent = `⊞ Plot all results (${formatNumber(capped)})`;
            btn.title = capped < ids.length
                ? `Draw the first ${formatNumber(capped)} of ${formatNumber(ids.length)} matches (render budget)`
                : 'Draw every match on the canvas, with the links between them';
        }
