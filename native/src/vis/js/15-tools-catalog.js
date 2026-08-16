        // ─── Tools ──────────────────────────────────────────

        // "Start here" strip: the most connected nodes, so the first view of
        // a project has a concrete place to look (and clicking one drops
        // straight into the focus/neighbour-stepping flow). Degree is read
        // off the edge list, not the centrality metrics, so the strip works
        // even on graphs that were never enriched.
        function renderStartHere() {
            const box = document.getElementById('start-here');
            if (!box) return;
            const degree = new Map();
            state.graph.edges.forEach(e => {
                const s = e.source.id || e.source;
                const t = e.target.id || e.target;
                degree.set(s, (degree.get(s) || 0) + 1);
                degree.set(t, (degree.get(t) || 0) + 1);
            });
            const top = [...degree.entries()]
                .sort((a, b) => b[1] - a[1])
                .slice(0, 5)
                .map(([id]) => state.nodeById.get(id))
                .filter(Boolean);
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
                item.title = `Focus ${escapeHtml(n.name)} (${degree.get(n.id)} edges)`;
                item.innerHTML = `
                    ${nodeIconSvg(n.group)}
                    <span class="name">${escapeHtml(truncateName(n.name))}</span>
                    <span class="deg">${degree.get(n.id)}</span>
                `;
                item.addEventListener('click', () => { handleClick(null, n); focusNode(n); });
                box.appendChild(item);
            });
        }

        function showCentrality() {
            const degEl = document.getElementById('degree-centrality');
            const betEl = document.getElementById('betweenness-centrality');
            degEl.innerHTML = ''; betEl.innerHTML = '';

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
                return state.catalogTree;
            }
            const { childrenOf, parentOf } = getContainsMaps();
            const nodeById = state.nodeById || new Map(state.graph.nodes.map(n => [n.id, n]));
            state.nodeById = nodeById;

            const isFolder = id => {
                const n = nodeById.get(id);
                return n && n.group === 'Folder';
            };
            const isFile = id => {
                const n = nodeById.get(id);
                return n && n.group === 'File';
            };

            const roots = [];
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
            const buildKids = (id) => {
                const kids = childrenOf.get(id) || [];
                return sortIds(kids.filter(k => nodeById.has(k)));
            };

            state.catalogTree = { roots: sortIds(roots), buildKids, nodeById, isFolder, isFile };
            state.catalogTreeForGraph = state.graph;
            state.catalogExpanded = state.catalogExpanded || new Set();

            // Auto-expand top two levels on first build for any given graph.
            if (!state.catalogAutoExpanded) {
                state.catalogAutoExpanded = true;
                state.catalogExpanded.clear();
                state.catalogTree.roots.forEach(r => {
                    state.catalogExpanded.add(r);
                    state.catalogTree.buildKids(r).forEach(k => {
                        if (state.catalogTree.isFolder(k)) state.catalogExpanded.add(k);
                    });
                });
            }
            return state.catalogTree;
        }

        function setAllCatalogExpanded(expand) {
            const tree = buildCatalogTree();
            state.catalogExpanded = state.catalogExpanded || new Set();
            if (!expand) {
                state.catalogExpanded.clear();
                renderCatalog();
                return;
            }
            const stack = tree.roots.slice();
            while (stack.length) {
                const id = stack.pop();
                state.catalogExpanded.add(id);
                tree.buildKids(id).forEach(k => stack.push(k));
            }
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
            subtitle.textContent = repoLabel ? `· ${repoLabel}` : '';

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

        function findPath(sourceId, targetId) {
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

