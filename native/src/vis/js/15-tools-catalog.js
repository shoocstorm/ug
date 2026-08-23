        // ─── Tools ──────────────────────────────────────────

        // "Start here" strip: the most connected nodes, so the first view of
        // a project has a concrete place to look (and clicking one drops
        // straight into the focus/neighbour-stepping flow). Degree is read
        // off the edge list, not the centrality metrics, so the strip works
        // even on graphs that were never enriched.
        function renderStartHere() {
            const box = document.getElementById('start-here');
            if (!box) return;
            // Both loaders can answer this — local mode from the map the edge
            // walk built, server mode from the degree column that came down with
            // the index. `topByDegree` is a partial selection rather than a
            // sort, so ranking 500k nodes to show five does not allocate an
            // array of all of them first.
            const top = topByDegree(5);
            if (top.length < 2) {
                box.closest('.section').style.display = 'none';
                return;
            }
            box.closest('.section').style.display = '';
            box.innerHTML = '';
            top.forEach(n => {
                const item = document.createElement('button');
                item.type = 'button';
                item.className = 'start-here-item';
                item.title = `Focus ${escapeHtml(n.name)} (${degreeOfNode(n)} edges)`;
                item.innerHTML = `
                    ${nodeIconSvg(n.group)}
                    <span class="name">${escapeHtml(truncateName(n.name))}</span>
                    <span class="deg">${degreeOfNode(n)}</span>
                `;
                item.addEventListener('click', () => { handleClick(null, n); focusNode(n); });
                box.appendChild(item);
            });
        }

        async function showCentrality() {
            const degEl = document.getElementById('degree-centrality');
            const betEl = document.getElementById('betweenness-centrality');
            degEl.innerHTML = ''; betEl.innerHTML = '';

            // In server mode `metrics` only exists on nodes that have been
            // hydrated, so ranking the local copies would rank whatever has
            // been clicked. The server computes this over the whole graph and
            // caches it per snapshot — betweenness is Brandes, so the first
            // call on a large repo is slow and the wait is announced rather
            // than left as a blank panel.
            if (state.graphMode === 'server') {
                degEl.innerHTML = '<div style="font-size:11px;color:var(--text-dim)">Computing centrality over the whole graph…</div>';
                try {
                    const res = await fetch('/api/graph/centrality');
                    if (!res.ok) throw new Error(await readErr(res));
                    const data = await res.json();
                    const toList = (obj) => Object.entries(obj || {})
                        .map(([id, value]) => ({ node: state.nodeById.get(id), value }))
                        .filter(e => e.node);
                    degEl.innerHTML = '';
                    renderCentrality(degEl, toList(data.degree_centrality), '#f97316');
                    renderCentrality(betEl, toList(data.betweenness_centrality), '#5b8fc9');
                } catch (err) {
                    degEl.innerHTML = `<div style="font-size:11px;color:var(--text-dim)">Centrality unavailable — ${escapeHtml(err.message || String(err))}</div>`;
                }
                return;
            }

            const degree = state.graph.nodes
                .filter(n => n.metrics && n.metrics.degree_centrality !== undefined)
                .map(n => ({ node: n, value: n.metrics.degree_centrality }));
            const betweenness = state.graph.nodes
                .filter(n => n.metrics && n.metrics.betweenness_centrality !== undefined)
                .map(n => ({ node: n, value: n.metrics.betweenness_centrality }));

            if (!degree.length && !betweenness.length) {
                degEl.innerHTML = '<div style="font-size:11px;color:var(--text-dim)">No centrality data — graph not enriched.</div>';
                return;
            }

            renderCentrality(degEl, degree, '#f97316');
            renderCentrality(betEl, betweenness, '#5b8fc9');
        }

        function renderCentrality(container, list, color) {
            list.sort((a, b) => b.value - a.value);
            const max = list[0]?.value || 1;
            list.slice(0, 6).forEach(({ node: n, value }) => {
                const item = document.createElement('div');
                item.className = 'centrality-item';
                const pct = (value / max) * 100;
                item.innerHTML = `
                    ${nodeIconSvg(n.group)}
                    <span class="name">${escapeHtml(truncateName(n.name))}</span>
                    <div class="bar" style="width:${pct}%;background:${color}"></div>
                    <span class="value">${value.toFixed(2)}</span>
                `;
                item.querySelector('.name').addEventListener('click', () => {
                    handleClick(null, n);
                    focusNode(n);
                });
                container.appendChild(item);
            });
        }

        // Cycle detection runs on the server (`GET /api/graph/cycles`), which
        // already owns a cached, iterative implementation over the same
        // graph.json.
        //
        // It used to run here, and could not: the DFS re-`filter`ed all of
        // `state.graph.edges` at every node it visited, so the work was
        // O(nodes × edges) — on a 162k-node / 746k-edge repo that is ~10¹¹
        // comparisons on the main thread, with a recursion depth that
        // overflows the stack long before it gets there. The server answers
        // the same graph in 0.33 s.
        //
        // On the published demo there is no server; the shim's 501 lands in
        // the catch and says so, which is the honest answer rather than a
        // frozen tab.
        async function detectAndShowCycles() {
            const status = document.getElementById('cycle-status');
            const text = document.getElementById('cycle-status-text');
            text.textContent = 'Computing…';
            status.className = 'cycle-status';

            try {
                const res = await fetch('/api/graph/cycles');
                if (!res.ok) throw new Error(await readErr(res));
                const data = await res.json();
                const count = (data.cycles || []).length;
                if (count) {
                    text.textContent = `${count} cycle(s) detected`;
                    status.className = 'cycle-status has-cycles';
                } else {
                    text.textContent = 'No cycles found';
                    status.className = 'cycle-status no-cycles';
                }
            } catch (err) {
                text.textContent = `Cycle detection unavailable — ${err.message || err}`;
                status.className = 'cycle-status';
            }
        }

        // ─── Catalog (repo TOC) ────────────────────────────

        // Discover's four modes (Search / Tour / Chat / Insights). The glider
        // is a single pill that slides to the active tab, so switching reads
        // as one movement rather than four separate highlights.
        function wireDiscoverSubtabs() {
            const bar = document.getElementById('discover-subtabs');
            const glider = document.getElementById('discover-glider');
            if (!bar || !glider) return;
            const tabs = [...bar.querySelectorAll('.subtab')];
            const panes = [...document.querySelectorAll('#pane-discover .subpane')];

            const moveGlider = () => {
                const active = bar.querySelector('.subtab.active');
                if (!active) return;
                glider.style.width = active.offsetWidth + 'px';
                glider.style.transform = `translateX(${active.offsetLeft - 4}px)`;
            };

            const activate = (name) => {
                // Leaving the Walk demo tears down its canvas state so the
                // next pane starts from a clean graph.
                if (state.discoverSub === 'walk' && name !== 'walk') exitWalk();
                tabs.forEach(t => t.classList.toggle('active', t.dataset.sub === name));
                panes.forEach(p => p.classList.toggle('active', p.dataset.sub === name));
                state.discoverSub = name;
                moveGlider();
                if (name === 'tour') renderTourHistory();
                if (name === 'walk') renderWalkHistory();
                // Presets load on first reveal, not at boot — the pane is one
                // of four and most sessions never open it.
                if (name === 'insights') loadInsights();
                writeUrlState();
            };

            tabs.forEach(t => t.addEventListener('click', () => activate(t.dataset.sub)));
            state.discoverSub = state.discoverSub || 'search';
            activate(state.discoverSub);
            // The bar is hidden until the Discover tab is shown, so its
            // widths only settle once it's on screen.
            window.addEventListener('resize', moveGlider);
            // Dragging the sidebar's edge resizes the bar without a window
            // resize event, so the pill would keep the width it was measured
            // at. Watch the bar itself instead of guessing when it changed.
            if (window.ResizeObserver) new ResizeObserver(moveGlider).observe(bar);
            state.syncDiscoverGlider = moveGlider;
            state.showDiscoverSub = activate;
        }

        // Switches between the three left-panel tabs (Catalog / Discover / Graph).
        function wirePanelTabs() {
            const tabs = document.querySelectorAll('.panel-tab');
            const panes = document.querySelectorAll('.tab-pane');
            const activate = (name) => {
                tabs.forEach(t => t.classList.toggle('active', t.dataset.tab === name));
                panes.forEach(p => p.classList.toggle('active', p.dataset.tab === name));
                state.activeTab = name;
                // Catalog renders lazily — make sure it's populated when revealed.
                if (name === 'catalog') renderCatalog();
                // The sub-tab glider can only measure itself once visible.
                if (name === 'discover' && state.syncDiscoverGlider) {
                    requestAnimationFrame(state.syncDiscoverGlider);
                    renderTourHistory();
                    renderWalkHistory();
                }
                writeUrlState();
            };
            tabs.forEach(tab => tab.addEventListener('click', () => activate(tab.dataset.tab)));
            // Exposed so the URL-state layer can switch panels on a deep link.
            state.showPanelTab = activate;
        }

        function wireCatalog() {
            const filter = document.getElementById('catalog-filter');
            const symbolsToggle = document.getElementById('catalog-symbols');
            const expandAll = document.getElementById('catalog-expand-all');
            const collapseAll = document.getElementById('catalog-collapse-all');
            const copyBtn = document.getElementById('catalog-copy');

            if (state.catalogIncludeSymbols == null) state.catalogIncludeSymbols = true;
            if (!state.catalogFilter) state.catalogFilter = '';
            symbolsToggle.checked = state.catalogIncludeSymbols;
            filter.value = state.catalogFilter;

            filter.addEventListener('input', () => {
                state.catalogFilter = filter.value.trim().toLowerCase();
                renderCatalog();
            });

            symbolsToggle.addEventListener('change', () => {
                state.catalogIncludeSymbols = symbolsToggle.checked;
                renderCatalog();
            });

            expandAll.addEventListener('click', () => setAllCatalogExpanded(true));
            collapseAll.addEventListener('click', () => setAllCatalogExpanded(false));
            copyBtn.addEventListener('click', copyCatalogMarkdown);

            // Catalog is the default tab, so build + render it up front.
            buildCatalogTree();
            renderCatalog();
        }

        // Builds a tree of node ids rooted at every Folder/File with no Contains
        // parent (or whose parent is missing from the graph). Cached on the
        // graph snapshot — invalidated whenever transformData reassigns
        // state.graph (containsMaps is reset there too).
        function buildCatalogTree() {
            if (state.catalogTree && state.catalogTreeForGraph === state.graph) {
                // Auto-expand is re-checked on the cached path too: in server
                // mode the first build runs before the roots' edges exist, so
                // the only chance to expand comes on a later call, once they
                // have landed. Returning early here left the tree collapsed
                // for the session.
                maybeAutoExpand(state.catalogTree);
                return state.catalogTree;
            }
            const { childrenOf, parentOf } = getContainsMaps();
            // Set by `initialize()` in both modes — a NodeStore in server mode,
            // a plain Map locally. Rebuilding one here as a fallback allocated a
            // 500k-entry map to cover an invariant that was already broken.
            const nodeById = state.nodeById;

            const isFolder = id => {
                const n = nodeById.get(id);
                return n && n.group === 'Folder';
            };
            const isFile = id => {
                const n = nodeById.get(id);
                return n && n.group === 'File';
            };

            // In server mode the roots came down with the slim index, computed
            // by the same Folder/File-parent test as below. Deriving them here
            // instead would ask every one of 162k nodes for its parents — a
            // whole-graph question the cache cannot answer, and 162k cold-miss
            // repairs if it tried.
            const roots = [];
            if (state.catalogRootIds) {
                roots.push(...state.catalogRootIds);
            } else {
                const seen = new Set();
                state.graph.nodes.forEach(n => {
                    if (n.group !== 'Folder' && n.group !== 'File') return;
                    const parents = parentOf.get(n.id) || [];
                    const hasContainerParent = parents.some(p => {
                        const pn = nodeById.get(p);
                        return pn && (pn.group === 'Folder' || pn.group === 'File');
                    });
                    if (!hasContainerParent && !seen.has(n.id)) {
                        seen.add(n.id);
                        roots.push(n.id);
                    }
                });
            }

            const sortIds = ids => {
                return ids.slice().sort((a, b) => {
                    const na = nodeById.get(a);
                    const nb = nodeById.get(b);
                    if (!na || !nb) return 0;
                    const rank = g => g === 'Folder' ? 0 : g === 'File' ? 1 : 2;
                    const ra = rank(na.group);
                    const rb = rank(nb.group);
                    if (ra !== rb) return ra - rb;
                    // Two symbols (e.g. nested headings) read best in source order.
                    if (ra === 2) {
                        const la = na.startLine == null ? Infinity : na.startLine;
                        const lb = nb.startLine == null ? Infinity : nb.startLine;
                        if (la !== lb) return la - lb;
                    }
                    return String(na.name).localeCompare(String(nb.name));
                });
            };

            // Build adjacency restricted to whatever the catalog should show.
            //
            // A node whose edges have not arrived answers "no children" for
            // now and *records itself*; `renderCatalog` fetches the whole
            // pass's worth in one request and renders again. Fetching per node
            // here instead — the obvious version — issues one request and one
            // full re-render per cold node, which on a tree with a couple of
            // hundred auto-expanded folders never settles.
            //
            // Batching this way converges in one round per level of depth
            // rather than one per node.
            const buildKids = (id) => {
                if (!edgesKnownComplete(id)) {
                    catalogColdIds.add(id);
                    return [];
                }
                const kids = childrenOf.get(id) || [];
                return sortIds(kids.filter(k => nodeById.has(k)));
            };

            state.catalogTree = { roots: sortIds(roots), buildKids, nodeById, isFolder, isFile };
            state.catalogTreeForGraph = state.graph;
            state.catalogExpanded = state.catalogExpanded || new Set();

            // Auto-expand top two levels on first build for any given graph.
            // Held back until the roots' edges are in: running it against a
            // cold cache would expand nothing and then latch
            // `catalogAutoExpanded`, so the tree would open collapsed and stay
            // that way for the session.
            maybeAutoExpand(state.catalogTree);
            return state.catalogTree;
        }

        // Open the top two levels, once per graph — but only once the roots'
        // edges are actually available. Running it against a cold cache would
        // expand nothing and still latch `catalogAutoExpanded`, leaving the
        // tree shut for the session.
        function maybeAutoExpand(tree) {
            if (state.catalogAutoExpanded) return;
            state.catalogExpanded = state.catalogExpanded || new Set();
            if (!tree.roots.every(edgesKnownComplete)) {
                // `buildKids` records cold ids; the flush at the end of
                // `renderCatalog` fetches them and we get called again.
                tree.roots.forEach(r => catalogColdIds.add(r));
                return;
            }
            state.catalogAutoExpanded = true;
            state.catalogExpanded.clear();
            tree.roots.forEach(r => {
                state.catalogExpanded.add(r);
                tree.buildKids(r).forEach(k => {
                    if (tree.isFolder(k)) state.catalogExpanded.add(k);
                });
            });
        }

        function setAllCatalogExpanded(expand) {
            const tree = buildCatalogTree();
            state.catalogExpanded = state.catalogExpanded || new Set();
            if (!expand) {
                state.catalogExpanded.clear();
                renderCatalog();
                return;
            }
            expandCatalogFrom(tree, tree.roots.slice());
        }

        // Expand-all, breadth-first and level-batched.
        //
        // Depth-first with `buildKids` per node was fine when every edge was
        // already local; in server mode it would ask the server for one node at
        // a time, thousands of times, and expand nothing while it waited. A
        // level is one request. `CATALOG_EXPAND_MAX` stops "expand all" on a
        // 162k-node repo from being a request to render 162k rows — the count
        // is reported rather than silently truncated.
        const CATALOG_EXPAND_MAX = 5000;
        async function expandCatalogFrom(tree, level) {
            let expanded = 0;
            let truncated = 0;
            while (level.length) {
                await ensureEdges(level);
                const next = [];
                for (const id of level) {
                    if (expanded >= CATALOG_EXPAND_MAX) { truncated++; continue; }
                    state.catalogExpanded.add(id);
                    expanded++;
                    for (const k of tree.buildKids(id)) next.push(k);
                }
                if (expanded >= CATALOG_EXPAND_MAX) { truncated += next.length; break; }
                level = next;
            }
            // Reported through `renderCatalog`, which owns the subtitle —
            // writing it here would be overwritten by the render on the very
            // next line.
            state.catalogExpandTruncated = truncated;
            renderCatalog();
        }

        function renderCatalog() {
            const body = document.getElementById('catalog-body');
            const subtitle = document.getElementById('catalog-subtitle');
            const stats = document.getElementById('catalog-stats');
            const tree = buildCatalogTree();
            const includeSymbols = state.catalogIncludeSymbols !== false;
            const filter = (state.catalogFilter || '').toLowerCase();
            const expanded = state.catalogExpanded;

            const repoLabel = (state.stats && state.stats.repoRoot)
                ? state.stats.repoRoot.split('/').filter(Boolean).pop() || state.stats.repoRoot
                : '';
            const capped = state.catalogExpandTruncated
                ? ` · expand stopped at ${formatNumber(CATALOG_EXPAND_MAX)}, `
                  + `${formatNumber(state.catalogExpandTruncated)} more below`
                : '';
            subtitle.textContent = (repoLabel ? `· ${repoLabel}` : '') + capped;
            subtitle.hidden = !subtitle.textContent;

            const counters = { folders: 0, files: 0, symbols: 0, shown: 0 };

            const matches = (node) => {
                if (!filter) return true;
                if (String(node.name || '').toLowerCase().includes(filter)) return true;
                if (node.file && String(node.file).toLowerCase().includes(filter)) return true;
                if (String(node.id || '').toLowerCase().includes(filter)) return true;
                return false;
            };

            // The catalog renders the whole Contains hierarchy through a single
            // recursive path. buildKids() returns *all* children of a node;
            // symbols are gated here by the "Symbols" toggle. Rendering symbols
            // through this same path (instead of a separate renderSymbol pass)
            // is what prevents every symbol from appearing twice.
            const isContainer = (id) => tree.isFolder(id) || tree.isFile(id);
            const childIds = (id) => {
                const kids = tree.buildKids(id);
                return includeSymbols ? kids : kids.filter(isContainer);
            };

            // Two-pass: first decide which subtrees to keep when filtering, then render.
            const keepSet = new Set();
            const matchSet = new Set();
            if (filter) {
                const visit = (id) => {
                    const n = tree.nodeById.get(id);
                    if (!n) return false;
                    let keep = false;
                    for (const k of childIds(id)) {
                        if (visit(k)) keep = true;
                    }
                    if (matches(n)) {
                        keep = true;
                        matchSet.add(id);
                    }
                    if (keep) keepSet.add(id);
                    return keep;
                };
                tree.roots.forEach(visit);
            }

            const html = [];
            const renderNode = (id, depth) => {
                const n = tree.nodeById.get(id);
                if (!n) return;
                if (filter && !keepSet.has(id)) return;

                const kids = childIds(id);
                const visibleKids = filter ? kids.filter(k => keepSet.has(k)) : kids;
                const hasChildren = visibleKids.length > 0;
                const isExpanded = expanded.has(id) || (filter && hasChildren);
                const color = config.getColor(n.group);

                if (n.group === 'Folder') counters.folders++;
                else if (n.group === 'File') counters.files++;
                else counters.symbols++;
                counters.shown++;

                const meta = [];
                if (visibleKids.length) {
                    meta.push(`${visibleKids.length} item${visibleKids.length === 1 ? '' : 's'}`);
                }
                if (n.startLine != null) {
                    let lineLabel = `L${n.startLine}`;
                    if (n.endLine != null && n.endLine !== n.startLine) {
                        lineLabel += `–${n.endLine}`;
                    }
                    meta.push(lineLabel);
                }

                const nameHtml = highlight(truncateName(n.name), filter);
                const cls = ['cat-node'];
                if (isExpanded) cls.push('expanded');
                if (matchSet.has(id) && filter) cls.push('match-self');

                html.push(`<div class="${cls.join(' ')}" data-id="${escapeHtml(id)}" data-leaf="${hasChildren ? '0' : '1'}" title="${escapeHtml(n.id)}">
                    <span class="cat-toggle${hasChildren ? '' : ' empty'}">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6"/></svg>
                    </span>
                    ${nodeIconSvg(n.group, 'cat-icon')}
                    <span class="cat-name">${nameHtml}</span>
                    <span class="cat-kind">${escapeHtml((n.group || '').toLowerCase())}</span>
                    ${meta.length ? `<span class="cat-meta">${escapeHtml(meta.join(' · '))}</span>` : ''}
                </div>`);

                html.push(`<div class="cat-children">`);
                if (hasChildren) {
                    visibleKids.forEach(k => renderNode(k, depth + 1));
                }
                html.push(`</div>`);
            };

            html.push(`<div class="catalog-tree">`);
            tree.roots.forEach(r => renderNode(r, 0));
            html.push(`</div>`);

            if (counters.shown === 0) {
                body.innerHTML = filter
                    ? `<div class="catalog-empty">No matches for <strong>${escapeHtml(filter)}</strong>.</div>`
                    : `<div class="catalog-empty">No folders or files in the graph. Re-run <code>ug index</code> to populate the catalog.</div>`;
            } else {
                body.innerHTML = html.join('');
                wireCatalogRows(body);
                // Re-apply the focused marker after a re-render (filter/expand etc.).
                if (state.catalogFocusedId) {
                    const r = body.querySelector(`.cat-node[data-id="${CSS.escape(state.catalogFocusedId)}"]`);
                    if (r) r.classList.add('focused');
                }
            }

            const chips = [];
            chips.push(`<span class="catalog-metric"><b>${counters.folders}</b><span>Folder${counters.folders === 1 ? '' : 's'}</span></span>`);
            chips.push(`<span class="catalog-metric"><b>${counters.files}</b><span>File${counters.files === 1 ? '' : 's'}</span></span>`);
            if (includeSymbols) {
                chips.push(`<span class="catalog-metric"><b>${counters.symbols}</b><span>Symbol${counters.symbols === 1 ? '' : 's'}</span></span>`);
            }
            stats.innerHTML = chips.join('');
            flushCatalogWarm();
        }

        // Nodes this render pass wanted children for but whose edges had not
        // arrived. Fetched as one batch, then one more render — see `buildKids`.
        const catalogColdIds = new Set();
        let catalogWarming = false;

        // One round per level of depth, driven here rather than by letting each
        // render schedule the next. Relying on the re-render to chain was
        // non-deterministic: whether the next level got recorded depended on
        // the order `maybeAutoExpand` and the flush happened to run in, and it
        // would sometimes stop one level down.
        //
        // `catalogWarming` also stops the `renderCatalog` at the end of each
        // round from re-entering this.
        const CATALOG_WARM_ROUNDS = 12;
        async function flushCatalogWarm() {
            if (catalogWarming || !catalogColdIds.size) return;
            catalogWarming = true;
            try {
                for (let round = 0; round < CATALOG_WARM_ROUNDS && catalogColdIds.size; round++) {
                    const ids = [...catalogColdIds];
                    catalogColdIds.clear();
                    await ensureEdges(ids);
                    renderCatalog();
                }
            } catch (err) {
                console.error('catalog warm failed:', err);
            } finally {
                catalogWarming = false;
            }
        }

        function wireCatalogRows(body) {
            body.querySelectorAll('.cat-node').forEach(row => {
                const toggle = row.querySelector('.cat-toggle');
                const isLeaf = row.dataset.leaf === '1';
                row.addEventListener('click', (e) => {
                    if (!isLeaf && (e.target === toggle || toggle.contains(e.target))) {
                        e.stopPropagation();
                        toggleCatalogRow(row);
                        return;
                    }
                    const id = row.dataset.id;
                    const node = state.nodeById ? state.nodeById.get(id) : null;
                    if (!node) return;
                    // Immersive focus: select + fly the camera, and pulse the row
                    // so the click reads as an immediate, juicy action — panel stays
                    // open so the tree remains a live navigation surface.
                    handleClick(null, node);
                    focusNode(node);
                    markCatalogFocused(row, id);
                });
            });
        }

        // Visual feedback for the catalog row whose node was just focused: a
        // persistent left-bar marker plus a one-shot pulse animation.
        function markCatalogFocused(row, id) {
            state.catalogFocusedId = id;
            const body = document.getElementById('catalog-body');
            if (body) body.querySelectorAll('.cat-node.focused').forEach(r => r.classList.remove('focused'));
            row.classList.add('focused');
            row.classList.remove('just-focused');
            // Force reflow so re-adding the class restarts the animation.
            void row.offsetWidth;
            row.classList.add('just-focused');
            row.addEventListener('animationend', () => row.classList.remove('just-focused'), { once: true });
        }

        function toggleCatalogRow(row) {
            const id = row.dataset.id;
            state.catalogExpanded = state.catalogExpanded || new Set();
            const isExpanded = row.classList.contains('expanded');
            if (isExpanded) {
                row.classList.remove('expanded');
                state.catalogExpanded.delete(id);
            } else {
                row.classList.add('expanded');
                state.catalogExpanded.add(id);
            }
        }

        function highlight(text, needle) {
            const safe = escapeHtml(text);
            if (!needle) return safe;
            const lower = safe.toLowerCase();
            const idx = lower.indexOf(needle);
            if (idx < 0) return safe;
            return safe.slice(0, idx) + '<mark>' + safe.slice(idx, idx + needle.length) + '</mark>' + safe.slice(idx + needle.length);
        }

        async function copyCatalogMarkdown() {
            const tree = buildCatalogTree();
            const includeSymbols = state.catalogIncludeSymbols !== false;
            const lines = [];
            const repo = state.stats && state.stats.repoRoot ? state.stats.repoRoot : 'Repository';
            lines.push(`# Catalog — ${repo}`, '');

            // The recursion below reads the Contains tree synchronously, so in
            // server mode the whole tree has to be resident first. Warm it
            // breadth-first, one request per level, capped the same way
            // expand-all is — a markdown dump of 162k lines is not a thing
            // anyone pastes anywhere.
            if (state.graphMode === 'server') {
                let level = tree.roots.slice();
                let warmed = 0;
                while (level.length && warmed < CATALOG_EXPAND_MAX) {
                    await ensureEdges(level);
                    warmed += level.length;
                    const next = [];
                    for (const id of level) for (const k of tree.buildKids(id)) next.push(k);
                    level = next;
                }
                if (level.length) lines.push(`_Truncated at ${formatNumber(CATALOG_EXPAND_MAX)} entries._`, '');
            }

            const walk = (id, depth) => {
                const n = tree.nodeById.get(id);
                if (!n) return;
                const indent = '  '.repeat(depth);
                const tag = n.group === 'Folder' ? '📁' : n.group === 'File' ? '📄' : '·';
                let extra = '';
                if (n.startLine != null) {
                    extra = ` _(L${n.startLine}${n.endLine != null && n.endLine !== n.startLine ? `–${n.endLine}` : ''})_`;
                }
                lines.push(`${indent}- ${tag} **${n.name}**${extra}`);
                // Single recursive walk — symbols are just children, gated by the
                // "Symbols" toggle (mirrors the on-screen tree; no duplication).
                tree.buildKids(id).forEach(k => {
                    const kn = tree.nodeById.get(k);
                    if (!includeSymbols && kn && kn.group !== 'Folder' && kn.group !== 'File') return;
                    walk(k, depth + 1);
                });
            };
            tree.roots.forEach(r => walk(r, 0));

            const text = lines.join('\n');
            const btn = document.getElementById('catalog-copy');
            const label = btn.querySelector('.catalog-copy-label');
            try {
                await navigator.clipboard.writeText(text);
                btn.classList.add('copied');
                if (label) label.textContent = 'Copied!';
                setTimeout(() => {
                    btn.classList.remove('copied');
                    if (label) label.textContent = 'Markdown';
                }, 1500);
            } catch (err) {
                if (label) label.textContent = 'Copy failed';
                setTimeout(() => { if (label) label.textContent = 'Markdown'; }, 1500);
            }
        }

        async function findPath(sourceId, targetId) {
            // The one traversal here that is genuinely unbounded — it can walk
            // the entire graph before deciding two nodes are unconnected — so
            // in server mode it goes to the server outright rather than pulling
            // the graph through the edge cache one frontier at a time.
            //
            // `GET /api/graph/path` is an exact match, not an approximation:
            // it is forward-only BFS, which is precisely what the loop below
            // does (see the `!== cur` guard).
            if (state.graphMode === 'server') {
                const qs = `?source=${encodeURIComponent(sourceId)}&target=${encodeURIComponent(targetId)}`;
                const res = await fetch(`/api/graph/path${qs}`);
                if (!res.ok) throw new Error(await readErr(res));
                const data = await res.json();
                if (!data.found) return { found: false };
                return {
                    found: true,
                    ids: data.path,
                    path: data.path.map(id => truncateName(id)),
                    hops: data.length,
                };
            }

            const queue = [[sourceId, [sourceId]]];
            const visited = new Set();
            let found = null;

            while (queue.length) {
                const [cur, path] = queue.shift();
                if (cur === targetId) { found = path; break; }
                if (visited.has(cur)) continue;
                visited.add(cur);
                // Outgoing edges only — the walk follows direction. Taken from
                // the adjacency index rather than re-scanning every edge per
                // BFS step, which is the difference between instant and
                // hopeless on a large graph.
                for (const edge of edgesOf(cur)) {
                    if ((edge.source.id || edge.source) !== cur) continue;
                    const next = edge.target.id || edge.target;
                    if (!visited.has(next)) queue.push([next, [...path, next]]);
                }
            }

            if (found) {
                // `path` is for display; `ids` is what can be drawn.
                return { found: true, ids: found, path: found.map(id => truncateName(id)), hops: found.length - 1 };
            } else {
                return { found: false };
            }
        }

