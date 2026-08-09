        // ─── Sidebar UI ─────────────────────────────────────

        function wireSidebarSections() {
            document.querySelectorAll('.section').forEach(sec => {
                const header = sec.querySelector('.section-header');
                if (!header) return;
                header.addEventListener('click', () => sec.classList.toggle('collapsed'));
            });
        }

        function wireToggle() {
            const sidebar = document.getElementById('sidebar');
            const setCollapsed = (collapsed) => sidebar.classList.toggle('collapsed', collapsed);
            // In-header chevron hides the panel; floating launcher brings it back.
            document.getElementById('sidebar-collapse').addEventListener('click', () => setCollapsed(true));
            document.getElementById('sidebar-launcher').addEventListener('click', () => setCollapsed(false));
            // Keyboard: "[" toggles the panel (ignored while typing in a field).
            document.addEventListener('keydown', (e) => {
                if (e.key !== '[' || e.metaKey || e.ctrlKey || e.altKey) return;
                const t = e.target;
                if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
                setCollapsed(!sidebar.classList.contains('collapsed'));
            });
        }

        // Drag the right edge of the sidebar to resize it; width persists across reloads.
        function wireSidebarResize() {
            const handle = document.getElementById('sidebar-resize');
            const root = document.documentElement;
            const MIN = 300, MAX = 680;
            const apply = (px) => root.style.setProperty('--sidebar-width', px + 'px');
            const readWidth = () =>
                parseInt(getComputedStyle(root).getPropertyValue('--sidebar-width'), 10) || 384;

            const saved = parseInt(localStorage.getItem('ug-sidebar-width'), 10);
            if (saved >= MIN && saved <= MAX) apply(saved);

            let startX = 0, startW = 0;
            const onMove = (e) => apply(Math.min(MAX, Math.max(MIN, startW + (e.clientX - startX))));
            const onUp = () => {
                document.body.classList.remove('resizing');
                window.removeEventListener('mousemove', onMove);
                window.removeEventListener('mouseup', onUp);
                localStorage.setItem('ug-sidebar-width', readWidth());
            };
            handle.addEventListener('mousedown', (e) => {
                e.preventDefault();
                startX = e.clientX;
                startW = readWidth();
                document.body.classList.add('resizing');
                window.addEventListener('mousemove', onMove);
                window.addEventListener('mouseup', onUp);
            });
            // Double-click resets to the default width.
            handle.addEventListener('dblclick', () => {
                apply(384);
                localStorage.setItem('ug-sidebar-width', 384);
            });
        }

        // Drag the left edge of the details panel to resize it; width
        // persists across reloads, same contract as the sidebar.
        function wireInfoResize() {
            const handle = document.getElementById('info-resize');
            const root = document.documentElement;
            if (!handle) return;
            const MIN = 320, MAX = 900;
            const apply = (px) => root.style.setProperty('--info-width', px + 'px');
            const readWidth = () =>
                parseInt(getComputedStyle(root).getPropertyValue('--info-width'), 10) || 420;

            const saved = parseInt(localStorage.getItem('ug-info-width'), 10);
            if (saved >= MIN && saved <= MAX) apply(saved);

            let startX = 0, startW = 0;
            // The panel is anchored right, so it widens as the pointer moves left.
            const onMove = (e) => apply(Math.min(MAX, Math.max(MIN, startW - (e.clientX - startX))));
            const onUp = () => {
                document.body.classList.remove('resizing');
                window.removeEventListener('mousemove', onMove);
                window.removeEventListener('mouseup', onUp);
                localStorage.setItem('ug-info-width', readWidth());
            };
            handle.addEventListener('mousedown', (e) => {
                e.preventDefault();
                startX = e.clientX;
                startW = readWidth();
                document.body.classList.add('resizing');
                window.addEventListener('mousemove', onMove);
                window.addEventListener('mouseup', onUp);
            });
            handle.addEventListener('dblclick', () => {
                apply(420);
                localStorage.setItem('ug-info-width', 420);
            });
        }

        function wireToolTabs() {
            const tabs = document.querySelectorAll('.tool-tab');
            const panels = document.querySelectorAll('.tool-panel');
            tabs.forEach(tab => {
                tab.addEventListener('click', () => {
                    tabs.forEach(t => t.classList.remove('active'));
                    panels.forEach(p => p.classList.remove('active'));
                    tab.classList.add('active');
                    document.querySelector(`.tool-panel[data-tool="${tab.dataset.tool}"]`)
                        .classList.add('active');
                    if (tab.dataset.tool === 'centrality') showCentrality();
                });
            });

            document.getElementById('cycle-run').addEventListener('click', detectAndShowCycles);

            // Wired here rather than with an inline `onclick`. Everything on
            // this page lives in a `<script type="module">`, and module scope
            // is not global scope — an `onclick="downloadGraph()"` attribute
            // resolves against `window`, finds nothing, and throws
            // `ReferenceError: downloadGraph is not defined` on every click.
            // These were the last two inline handlers on the page.
            document.getElementById('download-index').addEventListener('click', downloadIndex);
            document.getElementById('download-graph').addEventListener('click', downloadGraph);
        }

        function wireFooter() {
            document.getElementById('reset').addEventListener('click', resetView);
        }

        function wireFilterActions() {
            document.getElementById('filter-clear').addEventListener('click', () => {
                state.nodeFilters.clear();
                state.edgeFilters.clear();
                document.querySelectorAll('#node-filter .filter-chip, #edge-filter .filter-chip')
                    .forEach(c => c.classList.remove('active'));
                applyFilters();
                refreshSuggestions(document.getElementById('search').value);
            });
        }

        // ─── Filter chips ───────────────────────────────────

        function buildNodeFilterChips() {
            const counts = {};
            state.graph.nodes.forEach(n => { counts[n.group] = (counts[n.group] || 0) + 1; });
            const container = document.getElementById('node-filter');
            container.innerHTML = '';
            Object.entries(counts).sort((a, b) => b[1] - a[1]).forEach(([type, count]) => {
                const chip = document.createElement('div');
                chip.className = 'filter-chip';
                chip.dataset.type = type;
                chip.innerHTML = `
                    ${nodeIconSvg(type)}
                    <span>${type}</span>
                    <span class="chip-count">${count}</span>
                `;
                chip.addEventListener('click', () => {
                    if (state.nodeFilters.has(type)) {
                        state.nodeFilters.delete(type);
                        chip.classList.remove('active');
                    } else {
                        state.nodeFilters.add(type);
                        chip.classList.add('active');
                    }
                    applyFilters();
                    refreshSuggestions(document.getElementById('search').value);
                });
                container.appendChild(chip);
            });

            // One extra chip, on its own axis: "show only what the outside
            // world can reach". Appended after the type chips (and only when
            // the graph has boundaries) so it reads as an additional filter
            // rather than as another node type.
            const boundaryCount = state.graph.nodes.filter(n => n.isBoundary).length;
            if (!boundaryCount) return;

            const chip = document.createElement('div');
            chip.className = 'filter-chip filter-chip-boundary';
            if (state.boundaryFilter) chip.classList.add('active');
            chip.title = 'Only system boundaries — HTTP endpoints, queue listeners, '
                + 'CLI commands, and outbound HTTP/DB/queue clients.';
            chip.innerHTML = `
                <span class="boundary-dot"></span>
                <span>boundaries</span>
                <span class="chip-count">${boundaryCount}</span>
            `;
            chip.addEventListener('click', () => {
                state.boundaryFilter = !state.boundaryFilter;
                chip.classList.toggle('active', state.boundaryFilter);
                applyFilters();
                refreshSuggestions(document.getElementById('search').value);
            });
            container.appendChild(chip);
        }

        function buildEdgeFilterChips() {
            const counts = {};
            state.graph.edges.forEach(e => {
                const r = e.rel || 'default';
                counts[r] = (counts[r] || 0) + 1;
            });
            const container = document.getElementById('edge-filter');
            container.innerHTML = '';
            Object.entries(counts).sort((a, b) => b[1] - a[1]).forEach(([type, count]) => {
                const chip = document.createElement('div');
                chip.className = 'filter-chip';
                chip.dataset.type = type;
                chip.innerHTML = `
                    <span class="chip-dot" style="background:${config.getRelColor(type)};color:${config.getRelColor(type)}"></span>
                    <span>${type}</span>
                    <span class="chip-count">${count}</span>
                `;
                chip.addEventListener('click', () => {
                    if (state.edgeFilters.has(type)) {
                        state.edgeFilters.delete(type);
                        chip.classList.remove('active');
                    } else {
                        state.edgeFilters.add(type);
                        chip.classList.add('active');
                    }
                    applyFilters();
                });
                container.appendChild(chip);
            });
        }


        function applyFilters() {
            // Boundary is a separate axis from node type — an endpoint is
            // still a Function — so it ANDs with the type chips rather than
            // joining them. Selecting "Function" and "boundaries only"
            // means both, which is the question someone actually has.
            const nodeMatches = n =>
                (state.nodeFilters.size === 0 || state.nodeFilters.has(n.group))
                && (!state.boundaryFilter || n.isBoundary);
            const edgeMatches = e =>
                state.edgeFilters.size === 0 || state.edgeFilters.has(e.rel);

            const byId = state.nodeById || new Map(state.graph.nodes.map(n => [n.id, n]));
            // Predicates consumed by the graph's nodeVisibility / linkVisibility
            // accessors, and by the solo view builder. Filtered-out elements
            // are hidden entirely rather than dimmed — WebGL skips them.
            state.nodeHidden = n => !nodeMatches(n);
            state.linkHidden = e => {
                const sourceNode = byId.get(e.source.id || e.source);
                const targetNode = byId.get(e.target.id || e.target);
                if (sourceNode && !nodeMatches(sourceNode)) return true;
                if (targetNode && !nodeMatches(targetNode)) return true;
                return !edgeMatches(e);
            };
            // In solo mode the filters decide what gets *pulled onto* the
            // canvas, not just what is hidden once there — otherwise a
            // filtered-out neighbour still spends render budget.
            if (state.soloOnly) rebuildSoloView();
            bumpGraphStyles();
            syncLegend();
            writeUrlState();
        }

        // ─── Focus mode (isolate a node's neighbourhood) ─────

        // Set of node ids directly connected to `id` (1-hop). Reads the
        // adjacency index rather than scanning every edge — this runs on every
        // selection and every Tab step.
        function neighborIdsOf(id) {
            const ids = new Set();
            edgesOf(id).forEach(e => {
                const other = otherEnd(e, id);
                if (other !== id) ids.add(other);
            });
            return ids;
        }

        // Anchor focus on a node: keep it + its neighbours bright, dim the rest.
        // Styling is applied by the caller's bumpGraphStyles().
        function enterFocus(d) {
            state.focusNode = d.id;
            const set = neighborIdsOf(d.id);
            set.add(d.id);
            state.focusSet = set;
            document.body.classList.add('focus-active');
            // Solo, if on, follows the new anchor rather than stranding the
            // view on the previous node's neighbourhood.
            syncSoloButton();
            writeUrlState();
        }

        // Drop the focus dimming. Does not touch the selection or history.
        function exitFocus() {
            state.focusNode = null;
            state.focusSet = new Set();
            document.body.classList.remove('focus-active');
            // Solo has nothing left to show — leaving it armed would blank the
            // canvas on the next focus.
            state.focusIsolate = false;
            syncSoloButton();
            writeUrlState();
        }

        // Solo mode: show *only* the focused node and what it connects to.
        // The sibling of the tour's solo toggle, for ordinary selection.
        function toggleFocusSolo() {
            // Falls back to the selection: clicking the button is a reasonable
            // way to focus a node you picked from search or the sidebar.
            if (!state.focusNode && state.selectedNode) enterFocus(state.selectedNode);
            if (!state.focusNode) return;
            state.focusIsolate = !state.focusIsolate;
            syncSoloButton();
            bumpGraphStyles();
            // Re-frame so the neighbourhood fills the view, and so the way back
            // out doesn't leave the camera buried inside the restored graph.
            if (state.focusIsolate) {
                frameNodeSet(state.focusSet, 700);
            } else if (state.selectedNode) {
                focusNode(state.selectedNode);
            }
            writeUrlState();
        }

        // Enabled only when there is something to solo; pressed state mirrors
        // `state.focusIsolate`. On a graph too large to draw whole the button
        // is locked on: solo isn't a mode you can leave there, and a control
        // that looks live but refuses to move is worse than one that reads as
        // fixed.
        function syncSoloButton() {
            const btn = document.getElementById('toggle-solo');
            if (!btn) return;
            if (state.soloOnly) {
                btn.disabled = true;
                btn.classList.add('active', 'locked');
                btn.setAttribute('aria-pressed', 'true');
                btn.title = 'This graph is too large to draw at once — solo mode is always on';
                return;
            }
            const armed = !!state.focusNode;
            btn.disabled = !armed;
            btn.classList.toggle('active', armed && state.focusIsolate);
            btn.setAttribute('aria-pressed', String(armed && state.focusIsolate));
        }

        // ─── Navigation history (back / forward / breadcrumb) ─

        function recordHistory(id) {
            // Stepping onto the same node we're already on is a no-op.
            if (state.history[state.historyIndex] === id) return;
            // Drop any forward entries before branching off.
            if (state.historyIndex < state.history.length - 1) {
                state.history = state.history.slice(0, state.historyIndex + 1);
            }
            state.history.push(id);
            state.historyIndex = state.history.length - 1;
        }

        function navHistory(delta) {
            const i = state.historyIndex + delta;
            if (i < 0 || i >= state.history.length) return;
            state.historyIndex = i;
            const node = state.nodeById.get(state.history[i]);
            if (!node) return;
            state.suppressHistory = true;   // replaying — don't re-record
            handleClick(null, node);        // re-anchors focus on the historical node
            focusNode(node);
            state.suppressHistory = false;
        }

        // Tab / Shift+Tab: step the selection through the focus anchor's
        // neighbours without moving the focus anchor itself.
        function cycleNeighbor(dir) {
            const anchorId = state.focusNode || (state.selectedNode && state.selectedNode.id);
            if (!anchorId) return;
            const list = [...neighborIdsOf(anchorId)]
                .map(id => state.nodeById.get(id))
                .filter(n => n && !(state.nodeHidden && state.nodeHidden(n)))
                .sort((a, b) => String(a.name || '').localeCompare(String(b.name || '')));
            if (!list.length) return;
            if (state.neighborOf !== anchorId) { state.neighborOf = anchorId; state.neighborCursor = -1; }
            state.neighborCursor = (state.neighborCursor + dir + list.length) % list.length;
            const next = list[state.neighborCursor];
            if (!next) return;
            // Select + fly, but keep the focus neighbourhood anchored on `anchorId`.
            state.suppressHistory = true;
            state.suppressFocusReanchor = true;
            handleClick(null, next);
            focusNode(next);
            state.suppressFocusReanchor = false;
            state.suppressHistory = false;
        }

        function updateNavbar() {
            const bar = document.getElementById('navbar');
            const crumbs = document.getElementById('nav-crumbs');
            const back = document.getElementById('nav-back');
            const fwd = document.getElementById('nav-forward');
            if (!bar) return;
            const active = state.historyIndex >= 0 && state.history.length > 0;
            bar.classList.toggle('visible', active);
            if (!active) { crumbs.innerHTML = ''; return; }
            back.disabled = state.historyIndex <= 0;
            fwd.disabled = state.historyIndex >= state.history.length - 1;

            const cur = state.historyIndex;
            const start = Math.max(0, cur - 3);
            let html = '';
            if (start > 0) html += '<span class="crumb-sep">…</span>';
            for (let i = start; i <= cur; i++) {
                const n = state.nodeById.get(state.history[i]);
                if (!n) continue;
                if (i > start || start > 0) html += '<span class="crumb-sep">›</span>';
                html += `<span class="crumb${i === cur ? ' current' : ''}" data-idx="${i}" title="${escapeHtml(n.name)}">
                    ${nodeIconSvg(n.group)}
                    <span class="name">${escapeHtml(truncateName(n.name))}</span>
                </span>`;
            }
            crumbs.innerHTML = html;
            crumbs.querySelectorAll('.crumb').forEach(el => {
                el.addEventListener('click', () => navHistory(parseInt(el.dataset.idx, 10) - state.historyIndex));
            });
        }

        function wireNav() {
            const back = document.getElementById('nav-back');
            const fwd = document.getElementById('nav-forward');
            const exit = document.getElementById('nav-exit');
            if (back) back.addEventListener('click', () => navHistory(-1));
            if (fwd) fwd.addEventListener('click', () => navHistory(1));
            if (exit) exit.addEventListener('click', () => { clearSelection(); frameGraph(700); });
        }

        // ─── Bottom view bar: projections + box / spin toggles ─

        function setActiveViewBtn(id) {
            document.querySelectorAll('#viewbar .vbtn').forEach(b =>
                b.classList.toggle('active', b.dataset.view === id));
        }

        // The single door the box goes in and out of — the viewbar toggle and
        // the tour's save/restore both come through here. Switching it on
        // rebuilds from the current layout rather than revealing whatever
        // cube was last built, which would enclose the wrong region.
        function applyBoundaryVisibility() {
            if (state.showBoundary) updateBoundaryCube();
            else disposeBoundaryCube();
        }

        function applyAutoSpin() {
            const controls = Graph && Graph.controls && Graph.controls();
            if (!controls) return;
            controls.autoRotate = state.autoSpin;
            controls.autoRotateSpeed = 1.6;
        }

        function wireViewbar() {
            document.querySelectorAll('#viewbar .vbtn').forEach(btn => {
                btn.addEventListener('click', () => {
                    setActiveViewBtn(btn.dataset.view);
                    setView(btn.dataset.view);
                });
            });

            const boxBtn = document.getElementById('toggle-box');
            if (boxBtn) boxBtn.addEventListener('click', () => {
                state.showBoundary = !state.showBoundary;
                boxBtn.classList.toggle('active', state.showBoundary);
                applyBoundaryVisibility();
            });

            const spinBtn = document.getElementById('toggle-spin');
            if (spinBtn) spinBtn.addEventListener('click', () => {
                state.autoSpin = !state.autoSpin;
                spinBtn.classList.toggle('active', state.autoSpin);
                spinBtn.classList.toggle('spinning', state.autoSpin);
                applyAutoSpin();
            });

            const soloBtn = document.getElementById('toggle-solo');
            if (soloBtn) soloBtn.addEventListener('click', toggleFocusSolo);
            // Starts inert: nothing is selected yet, so there is nothing to solo.
            syncSoloButton();

            const zIn = document.getElementById('zoom-in');
            const zOut = document.getElementById('zoom-out');
            if (zIn) zIn.addEventListener('click', () => zoomBy(0.8));
            if (zOut) zOut.addEventListener('click', () => zoomBy(1.25));
        }

        // ─── On-canvas legend (doubles as a type filter) ─────

        function buildLegend() {
            const body = document.getElementById('legend-body');
            const head = document.getElementById('legend-head');
            if (!body) return;
            const counts = new Map();
            state.graph.nodes.forEach(n => counts.set(n.group, (counts.get(n.group) || 0) + 1));
            const types = [...counts.keys()].sort((a, b) => counts.get(b) - counts.get(a));
            if (!types.length) { document.getElementById('legend').style.display = 'none'; return; }
            body.innerHTML = types.map(t => {
                return `<div class="legend-row" data-type="${escapeHtml(t)}" title="Filter to ${escapeHtml(t)}">
                    ${nodeIconSvg(t)}
                    <span class="name">${escapeHtml(t)}</span>
                    <span class="count">${counts.get(t)}</span>
                </div>`;
            }).join('');
            // The legend is the on-canvas key, so the dashed ring drawn on
            // boundary nodes needs an entry here or it is an unexplained
            // decoration. Appended below the types and marked as a separate
            // axis — it filters alongside them, not instead of them.
            const boundaryCount = state.graph.nodes.filter(n => n.isBoundary).length;
            if (boundaryCount) {
                body.insertAdjacentHTML('beforeend',
                    `<div class="legend-row legend-row-boundary" data-boundary="1"
                          title="Only system boundaries — HTTP endpoints, queue listeners, CLI commands, outbound clients">
                        <span class="legend-boundary-ring"></span>
                        <span class="name">boundary</span>
                        <span class="count">${boundaryCount}</span>
                    </div>`);
            }

            body.querySelectorAll('.legend-row').forEach(row => {
                if (row.dataset.boundary) {
                    row.addEventListener('click', () => {
                        state.boundaryFilter = !state.boundaryFilter;
                        document.querySelectorAll('#node-filter .filter-chip-boundary')
                            .forEach(c => c.classList.toggle('active', state.boundaryFilter));
                        applyFilters();
                        refreshSuggestions(document.getElementById('search').value);
                    });
                    return;
                }
                row.addEventListener('click', () => toggleTypeFilter(row.dataset.type));
            });
            if (head) head.addEventListener('click', () => {
                document.getElementById('legend').classList.toggle('is-collapsed');
            });
            syncLegend();
        }

        // Toggle a node-type filter, keeping the sidebar chips + legend in sync.
        function toggleTypeFilter(type) {
            if (state.nodeFilters.has(type)) state.nodeFilters.delete(type);
            else state.nodeFilters.add(type);
            // `[data-type]` only: the boundary chip filters on its own axis
            // and has no type, so an unscoped selector would silently clear
            // its highlight while `state.boundaryFilter` stayed on.
            document.querySelectorAll('#node-filter .filter-chip[data-type]').forEach(chip => {
                chip.classList.toggle('active', state.nodeFilters.has(chip.dataset.type));
            });
            applyFilters();
            refreshSuggestions(document.getElementById('search').value);
        }

        // Mute legend rows whose type is currently filtered out.
        function syncLegend() {
            const active = state.nodeFilters;
            document.querySelectorAll('.legend-row').forEach(row => {
                // Muting means "this type is filtered out". That reading does
                // not transfer to a boolean facet: the boundary row is never
                // excluded, it is either engaged or not, so it gets `active`
                // rather than the inverse of `muted`.
                if (row.dataset.boundary) {
                    row.classList.toggle('active', !!state.boundaryFilter);
                    return;
                }
                const on = active.size === 0 || active.has(row.dataset.type);
                row.classList.toggle('muted', !on);
            });
        }

        // Reflect what is actually on screen during an immersive mode. In a
        // walk the visible set is the reached frontier; on a tour it is the
        // route + its neighbourhood. Otherwise the counts revert to the whole
        // graph. Called throttled from the overlay loop so it tracks both
        // modes without each one having to hook in.
        function refreshModeLegend() {
            const body = document.getElementById('legend-body');
            if (!body) return;
            let ids = null, modeLabel = null;
            if (state.walkActive) { ids = state.walkReached; modeLabel = 'walk'; }
            else if (typeof tourState !== 'undefined' && tourState.active && tourState.routeIds.size) {
                ids = new Set([...tourState.routeIds, ...(tourState.nearIds || [])]);
                modeLabel = 'tour';
            }
            const rows = body.querySelectorAll('.legend-row');
            if (!ids) {
                const counts = new Map();
                state.graph.nodes.forEach(n => counts.set(n.group, (counts.get(n.group) || 0) + 1));
                rows.forEach(row => {
                    row.querySelector('.count').textContent = counts.get(row.dataset.type) || 0;
                    row.classList.remove('zero');
                });
                setLegendModeNote(null);
                return;
            }
            const counts = new Map();
            let total = 0;
            ids.forEach(id => {
                const n = state.nodeById && state.nodeById.get(id);
                if (!n) return;
                counts.set(n.group, (counts.get(n.group) || 0) + 1);
                total++;
            });
            rows.forEach(row => {
                const c = counts.get(row.dataset.type) || 0;
                row.querySelector('.count').textContent = c;
                row.classList.toggle('zero', c === 0);
            });
            setLegendModeNote(`${modeLabel} · ${total} node${total === 1 ? '' : 's'} on screen`);
        }

        function setLegendModeNote(text) {
            const el = document.getElementById('legend-mode-note');
            if (!el) return;
            el.textContent = text || '';
            el.style.display = text ? '' : 'none';
        }

        // ─── Orientation gizmo + distance-adaptive labels ────

        const GIZMO_AXES = [
            { v: [1, 0, 0], color: '#ff5d5d', label: 'X' },
            { v: [0, 1, 0], color: '#5dff8f', label: 'Y' },
            { v: [0, 0, 1], color: '#5d9dff', label: 'Z' },
        ];

        function startOverlayLoop() {
            const svg = document.getElementById('gizmo-svg');
            let frame = 0;
            const v = new THREE.Vector3();
            const tick = () => {
                requestAnimationFrame(tick);
                if (!Graph) return;
                const cam = Graph.camera();
                if (!cam) return;
                frame++;
                // Gizmo: rotate the world axes into the camera's view frame so the
                // little triad mirrors how the graph is oriented on screen.
                if (svg && frame % 2 === 0) {
                    cam.updateMatrixWorld();
                    const q = cam.quaternion.clone().invert();
                    const R = 26;
                    const drawn = GIZMO_AXES.map(a => {
                        v.set(a.v[0], a.v[1], a.v[2]).applyQuaternion(q);
                        return { color: a.color, label: a.label, x: v.x, y: v.y, z: v.z };
                    }).sort((a, b) => a.z - b.z); // painter's order: far axes first
                    let s = '';
                    for (const a of drawn) {
                        const x = (a.x * R).toFixed(1);
                        const y = (-a.y * R).toFixed(1);
                        const op = (0.4 + 0.6 * ((a.z + 1) / 2)).toFixed(2);
                        s += `<line x1="0" y1="0" x2="${x}" y2="${y}" stroke="${a.color}" stroke-width="2.4" stroke-linecap="round" opacity="${op}"/>`;
                        s += `<circle cx="${x}" cy="${y}" r="3.2" fill="${a.color}" opacity="${op}"/>`;
                        s += `<text x="${(a.x * R * 1.34).toFixed(1)}" y="${(-a.y * R * 1.34 + 3.4).toFixed(1)}" fill="${a.color}" font-size="9.5" font-family="JetBrains Mono, monospace" text-anchor="middle" opacity="${op}">${a.label}</text>`;
                    }
                    svg.innerHTML = s;
                }
                // Distance-adaptive labels: hide labels for nodes far from the
                // camera so a zoomed-out view stays clean and they reappear as you
                // move in. Throttled.
                if (frame % 8 === 0) updateAdaptiveLabels(cam);
                // Legend counts track the on-screen set during a walk or tour.
                if (frame % 12 === 0) refreshModeLegend();
            };
            requestAnimationFrame(tick);
        }

        function updateAdaptiveLabels(cam) {
            const px = cam.position.x, py = cam.position.y, pz = cam.position.z;
            const D = state._labelDist || 340;
            const D2 = D * D;
            const focusOn = !!state.focusNode;
            const tourOn = tourState.active && tourState.routeIds.size > 0;
            state.view.nodes.forEach(n => {
                const s = n.__nodeLabel;
                if (!s) return;
                // On a tour only the stops are named — the surrounding
                // neighbourhood stays present but anonymous.
                if (tourOn) {
                    s.visible = tourState.routeIds.has(n.id);
                    return;
                }
                if (focusOn) {
                    // While focused, always label the neighbourhood; hide the rest.
                    s.visible = state.focusSet.has(n.id);
                    return;
                }
                const dx = (n.x || 0) - px, dy = (n.y || 0) - py, dz = (n.z || 0) - pz;
                s.visible = (dx * dx + dy * dy + dz * dz) < D2;
            });
        }

