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
            // Off the whole-graph histogram both loaders publish, not a fresh
            // pass over every node — see `nodeTypeCountsAll`.
            const counts = nodeTypeCountsAll();
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
            const boundaryCount = boundaryCountAll();
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
            // Counted by whichever loader ran — from the edge list in local
            // mode, off the index in server mode. Rebuilding them here from
            // `state.graph.edges` gave an empty chip row in server mode, which
            // reads as "this graph has no edge types" and, worse, leaves
            // `state.edgeFilters` empty so the edge filter matches everything.
            const counts = { ...(state.edgeTypeCounts || {}) };
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

            // `initialize()` sets this before anything can call in here, in both
            // modes. Rebuilding it as a fallback allocated a 500k-entry map to
            // paper over an invariant that was already broken.
            const byId = state.nodeById;
            // With nothing filtered these predicates cannot answer true, so say
            // so without looking anything up.
            //
            // Not a micro-optimisation: `linkHidden` resolves *both* endpoints
            // of every edge it is asked about, and `neighborsOf` asks about
            // every edge of the node being expanded. On a node of degree 8,680
            // — the real maximum in `~/.ug/neo4j` — that is 17k lookups to
            // evaluate a predicate whose answer is fixed. In server mode each
            // of those lookups also *builds* a node object, so the unfiltered
            // case was materialising the entire neighbourhood to decide it was
            // showing all of it.
            const noNodeFilter = state.nodeFilters.size === 0 && !state.boundaryFilter;
            const noEdgeFilter = state.edgeFilters.size === 0;
            // Read by `neighborsOf`, which has to skip the *lookup* and not
            // just the predicate: resolving a node only to ask a question whose
            // answer is fixed is what the short-circuit above cannot save.
            state.nodeFilterActive = !noNodeFilter;
            state.nodeHidden = noNodeFilter ? () => false : n => !nodeMatches(n);
            state.linkHidden = (noNodeFilter && noEdgeFilter) ? () => false : e => {
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
            // In server mode the neighbours may not have arrived yet, and this
            // runs inside the synchronous `handleClick` (18 call sites — not a
            // function to make async). So focus lands on the node alone and
            // widens a beat later, the same shape as `enrichFromDb`. The
            // guard in `edgesOf` has already started the fetch.
            if (state.graphMode === 'server' && !state.adjComplete.has(d.id)) {
                ensureEdges([d.id]).then(() => {
                    if (state.focusNode !== d.id) return;   // moved on since
                    const widened = neighborIdsOf(d.id);
                    widened.add(d.id);
                    state.focusSet = widened;
                    bumpGraphStyles();
                });
            }
            document.body.classList.add('focus-active');
            // Solo, if on, follows the new anchor rather than stranding the
            // view on the previous node's neighbourhood.
            syncSoloButton();
            // The trail rings the focused crumb, so it has to be re-read
            // whenever focus moves.
            updateNavbar();
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
            updateNavbar();
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
            // The bar and the title block share the top of the screen; the
            // class is what hands it over (see body.nav-active .header).
            document.body.classList.toggle('nav-active', active);
            // Nothing to exit *from* unless something is focused or selected —
            // an always-live button here reads as "this does something", and
            // then does nothing.
            const exit = document.getElementById('nav-exit');
            if (exit) exit.disabled = !state.focusNode && !state.selectedNode;
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
                // `current` is where you are in the *trail*; `selected` is what
                // is lit on the canvas. They are usually the same crumb — but
                // clicking a node off-trail, or clearing the selection, splits
                // them, and then the trail alone stops telling you which node
                // the details panel is actually describing.
                const cls = 'crumb'
                    + (i === cur ? ' current' : '')
                    + (state.selectedNode && state.selectedNode.id === state.history[i] ? ' selected' : '')
                    + (state.focusNode === state.history[i] ? ' focused' : '');
                html += `<span class="${cls}" data-idx="${i}" title="${escapeHtml(n.name)}">
                    ${nodeIconSvg(n.group)}
                    <span class="name">${escapeHtml(truncateName(n.name))}</span>
                </span>`;
            }
            crumbs.innerHTML = html;
            crumbs.querySelectorAll('.crumb').forEach(el => {
                el.addEventListener('click', () => navHistory(parseInt(el.dataset.idx, 10) - state.historyIndex));
            });
        }

        // Forget the trail. The bar hides itself once the history is empty, so
        // this is also how it is dismissed. Closing it drops the focus too —
        // the dimming is meaningless once the crumb trail it was anchored to
        // is gone. The selection is left as it is.
        function clearNavHistory() {
            state.history = [];
            state.historyIndex = -1;
            // `exitFocus` only flips the state — the dimming itself is paint,
            // applied on the next restyle, so both halves always travel together
            // (see `clearSelection`).
            exitFocus();
            bumpGraphStyles();
        }

        // Drag the history bar anywhere. It defaults to the very top, which is
        // where it is least in the way — but "least in the way" depends on what
        // is on the canvas underneath, and only the person looking at it knows
        // that. The position is remembered across reloads.
        const NAVBAR_POS_KEY = 'ug-navbar-pos';

        function placeNavbar(x, y) {
            const bar = document.getElementById('navbar');
            if (!bar) return;
            // Keep it reachable: a bar dragged off-screen (or parked before a
            // window resize) is one the user cannot get back.
            const w = bar.offsetWidth || 320;
            const h = bar.offsetHeight || 36;
            const cx = Math.min(Math.max(x, 4), Math.max(4, window.innerWidth - w - 4));
            const cy = Math.min(Math.max(y, 4), Math.max(4, window.innerHeight - h - 4));
            bar.classList.add('dragged');
            bar.style.left = cx + 'px';
            bar.style.top = cy + 'px';
            return { x: cx, y: cy };
        }

        function wireNavbarDrag() {
            const bar = document.getElementById('navbar');
            if (!bar) return;

            const saved = (() => {
                try { return JSON.parse(localStorage.getItem(NAVBAR_POS_KEY) || 'null'); }
                catch (err) { return null; }
            })();
            if (saved && Number.isFinite(saved.x) && Number.isFinite(saved.y)) {
                placeNavbar(saved.x, saved.y);
            }

            let dx = 0, dy = 0;
            const onMove = (e) => {
                const at = placeNavbar(e.clientX - dx, e.clientY - dy);
                if (at) { bar._pos = at; }
            };
            const onUp = () => {
                bar.classList.remove('dragging');
                window.removeEventListener('mousemove', onMove);
                window.removeEventListener('mouseup', onUp);
                try { localStorage.setItem(NAVBAR_POS_KEY, JSON.stringify(bar._pos || null)); }
                catch (err) { /* private mode */ }
            };
            bar.addEventListener('mousedown', (e) => {
                // Buttons and crumbs are targets, not handles.
                if (e.target.closest('button, .crumb')) return;
                e.preventDefault();
                const r = bar.getBoundingClientRect();
                dx = e.clientX - r.left;
                dy = e.clientY - r.top;
                bar._pos = { x: r.left, y: r.top };
                bar.classList.add('dragging');
                // Freeze it where it currently sits before the first move, so a
                // still-centred bar doesn't jump by half its width on grab.
                placeNavbar(r.left, r.top);
                window.addEventListener('mousemove', onMove);
                window.addEventListener('mouseup', onUp);
            });
            // A window that shrank can strand it off-screen.
            window.addEventListener('resize', () => {
                if (!bar.classList.contains('dragged')) return;
                const r = bar.getBoundingClientRect();
                placeNavbar(r.left, r.top);
            });
        }

        function wireNav() {
            const back = document.getElementById('nav-back');
            const fwd = document.getElementById('nav-forward');
            const exit = document.getElementById('nav-exit');
            const clear = document.getElementById('nav-clear');
            if (back) back.addEventListener('click', () => navHistory(-1));
            if (fwd) fwd.addEventListener('click', () => navHistory(1));
            if (exit) exit.addEventListener('click', () => { clearSelection(); frameGraph(700); });
            if (clear) clear.addEventListener('click', clearNavHistory);
            wireNavbarDrag();
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
            const r = activeRenderer();
            if (r) r.setBoundaryVisible(state.showBoundary);
        }

        function applyAutoSpin() {
            const r = activeRenderer();
            if (r) r.setAutoSpin(state.autoSpin);
        }

        function wireViewbar() {
            wireRendererToggle();
            document.querySelectorAll('#viewbar .vbtn').forEach(btn => {
                btn.addEventListener('click', () => {
                    setActiveViewBtn(btn.dataset.view);
                    setView(btn.dataset.view);
                });
            });

            document.querySelectorAll('#viewbar .lbtn').forEach(btn => {
                btn.addEventListener('click', () => setGraphLayout(btn.dataset.layout));
            });

            const labelBtn = document.getElementById('toggle-labels');
            if (labelBtn) labelBtn.addEventListener('click', toggleShowLabels);
            syncLabelButtons();

            // The reset button moved here from the sidebar footer — everything
            // it undoes happens on the canvas.
            const resetBtn = document.getElementById('reset');
            if (resetBtn) resetBtn.addEventListener('click', resetView);

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

        // Node name labels. The control appears twice — in the viewbar and on
        // the walk card, because a walk hides the viewbar outright — so the
        // state lives here and both buttons are rendered from it rather than
        // each keeping its own idea of it. They carry the same mark, too.
        function syncLabelButtons() {
            ['toggle-labels', 'walk-o-labels'].forEach(id => {
                const btn = document.getElementById(id);
                if (!btn) return;
                btn.classList.toggle('active', state.showLabels);
                btn.setAttribute('aria-pressed', String(state.showLabels));
            });
        }

        function setShowLabels(on) {
            state.showLabels = on;
            syncLabelButtons();
            // The 2D overlay re-reads state every frame; the 3D backend owns
            // sprite visibility and has to be told.
            bumpGraphStyles();
        }

        function toggleShowLabels() {
            setShowLabels(!state.showLabels);
        }

        // Mark which arrangement is showing. Called by the renderer whenever it
        // changes one, including the opening's hand-off to the force layout.
        function syncLayoutButtons() {
            document.querySelectorAll('#viewbar .lbtn').forEach(btn => {
                btn.classList.toggle('active', btn.dataset.layout === state.layout2d);
            });
        }

        // ─── On-canvas legend (doubles as a type filter) ─────

        function buildLegend() {
            const body = document.getElementById('legend-body');
            const head = document.getElementById('legend-head');
            if (!body) return;
            const counts = new Map(Object.entries(nodeTypeCountsAll()));
            // Canonical type order (NODE_TYPE_ORDER), matching the Rings layout
            // and the rest of the type orderings — so the legend reads top to
            // bottom the way the rings read inside out, and the two can be
            // compared without re-finding each type in a different place.
            const types = [...counts.keys()].sort((a, b) =>
                nodeTypeRank(a) - nodeTypeRank(b) || a.localeCompare(b));
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
            const boundaryCount = boundaryCountAll();
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
            wireLegendBulk();
            syncLegend();
        }

        // Toggle a node-type filter, keeping the sidebar chips + legend in sync.
        // ─── Bulk type filtering ─────────────────────────────
        //
        // An empty `state.nodeFilters` means "no filter — show everything",
        // which is the right default but leaves "show *nothing*" with no
        // representation at all. Rather than invert that meaning everywhere
        // (initialize, resetView, the chips, the walk's seed list all read
        // `size === 0` as "unfiltered"), one sentinel value carries it: a
        // string no node type can equal, so every existing `has(group)` test
        // answers false and the canvas empties. Nothing else needs to know.
        const NODE_FILTER_NONE = ' none';

        function presentNodeTypes() {
            return Object.keys(nodeTypeCountsAll());
        }

        // The types actually showing, resolving both special cases.
        function activeNodeTypes() {
            if (state.nodeFilters.has(NODE_FILTER_NONE)) return [];
            if (state.nodeFilters.size === 0) return presentNodeTypes();
            return presentNodeTypes().filter(t => state.nodeFilters.has(t));
        }

        // Apply a set of types, normalising the two ends so the state stays
        // canonical: everything selected is stored as "no filter", nothing
        // selected as the sentinel.
        function setNodeTypeFilter(types) {
            const present = presentNodeTypes();
            const keep = present.filter(t => types.includes(t));
            state.nodeFilters = keep.length === 0 ? new Set([NODE_FILTER_NONE])
                : keep.length === present.length ? new Set()
                : new Set(keep);
            document.querySelectorAll('#node-filter .filter-chip[data-type]').forEach(chip => {
                chip.classList.toggle('active', state.nodeFilters.has(chip.dataset.type));
            });
            applyFilters();
            refreshSuggestions(document.getElementById('search').value);
        }

        function wireLegendBulk() {
            const acts = [
                ['legend-all', () => presentNodeTypes()],
                ['legend-none', () => []],
                ['legend-invert', () => {
                    const on = new Set(activeNodeTypes());
                    return presentNodeTypes().filter(t => !on.has(t));
                }],
            ];
            acts.forEach(([id, pick]) => {
                const btn = document.getElementById(id);
                // buildLegend runs again on a project switch; a second listener
                // here would invert twice per click, which reads as nothing
                // happening at all.
                if (!btn || btn.dataset.wired === '1') return;
                btn.dataset.wired = '1';
                btn.addEventListener('click', (e) => {
                    // The whole header collapses the legend; these sit inside it.
                    e.stopPropagation();
                    setNodeTypeFilter(pick());
                });
            });
        }

        function toggleTypeFilter(type) {
            // Picking a type out of "show nothing" retires the sentinel —
            // otherwise it would linger and re-selecting every type by hand
            // would leave the set in a state that means both things at once.
            state.nodeFilters.delete(NODE_FILTER_NONE);
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
            // Solo mode never draws the whole graph — the renderer is handed
            // one neighbourhood at a time — so whole-graph totals here describe
            // a diagram that is not on screen. Note the empty set is used as-is
            // rather than falling through: a solo canvas with nothing picked
            // yet genuinely has zero of everything, and saying otherwise is the
            // same lie in its most misleading form.
            else if (state.soloOnly) {
                ids = state.viewIds || new Set();
                modeLabel = 'solo';
            }
            const rows = body.querySelectorAll('.legend-row');
            // The boundary row is a second axis, not a node type: it counts the
            // `isBoundary` flag, which any type can carry. It has no
            // `data-type`, so the type lookup below returns undefined for it —
            // which is why its count read 0 from the first refresh onward, no
            // matter how many boundary nodes the graph had. buildLegend set it
            // correctly and this function immediately overwrote it.
            const boundaryRow = body.querySelector('.legend-row[data-boundary]');
            const setCount = (row, c) => {
                row.querySelector('.count').textContent = c;
                row.classList.toggle('zero', c === 0);
            };

            if (!ids) {
                const counts = new Map(Object.entries(nodeTypeCountsAll()));
                const boundary = boundaryCountAll();
                rows.forEach(row => {
                    if (row === boundaryRow) return;
                    row.querySelector('.count').textContent = counts.get(row.dataset.type) || 0;
                    row.classList.remove('zero');
                });
                if (boundaryRow) setCount(boundaryRow, boundary);
                setLegendModeNote(null);
                return;
            }
            const counts = new Map();
            let total = 0;
            let boundary = 0;
            ids.forEach(id => {
                const n = state.nodeById && state.nodeById.get(id);
                if (!n) return;
                counts.set(n.group, (counts.get(n.group) || 0) + 1);
                if (n.isBoundary) boundary++;
                total++;
            });
            rows.forEach(row => {
                if (row === boundaryRow) return;
                setCount(row, counts.get(row.dataset.type) || 0);
            });
            if (boundaryRow) setCount(boundaryRow, boundary);
            setLegendModeNote(`${modeLabel} · ${total} node${total === 1 ? '' : 's'} on screen`);
        }

        function setLegendModeNote(text) {
            const el = document.getElementById('legend-mode-note');
            if (!el) return;
            el.textContent = text || '';
            el.style.display = text ? '' : 'none';
        }

