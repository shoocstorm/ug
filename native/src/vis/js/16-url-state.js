        // ─── URL state sync (deep-linkable views) ─────────────
        // A small slice of `state` maps onto the query string, so a view is
        // shareable (`?p=&n=&tab=&nf=`) and survives a reload. The URL is
        // the source of truth on load; afterwards every mutation of a mapped
        // key rewrites it.
        //
        // Continuous changes (filters, focus, tab, query) use a debounced
        // `replaceState` so they don't litter the history stack. Landing on
        // a node uses `pushState`, which makes the browser Back button and
        // the in-app Back button the same path: the URL history *is* the
        // navigation history, replayed through the same `suppressHistory`
        // machinery that already exists for breadcrumb replay.
        //
        // This module owns `location`; nothing else should touch it.

        const URL_SYNC_MS = 250;
        let urlWriteTimer = null;

        // Decode `?p=&n=&tab=&focus=&nf=&ef=&q=&file=` into a plain object.
        // Unknown keys are ignored; a key that fails to parse is dropped
        // rather than fatal.
        function readUrlState() {
            const p = new URLSearchParams(window.location.search);
            const out = {};
            if (p.get('p')) out.p = p.get('p');
            if (p.get('n')) out.n = p.get('n');
            const f = p.get('focus');
            if (f === 'off' || f === 'dim' || f === 'solo') out.focus = f;
            if (p.get('tab')) out.tab = p.get('tab');
            if (p.get('q')) out.q = p.get('q');
            const nf = (p.get('nf') || '').split(',').filter(Boolean);
            const ef = (p.get('ef') || '').split(',').filter(Boolean);
            if (nf.length) out.nf = nf;
            if (ef.length) out.ef = ef;
            return out;
        }

        // The mapped subset of `state` as query params. `file` and `p` are
        // carried along even though they aren't mutations, so a single-file
        // serve (`?file=graph.json`) and a project deep link keep working
        // after the first URL rewrite.
        function urlStateParams() {
            const p = new URLSearchParams();
            if (state.graphFile) p.set('file', state.graphFile);
            if (state.activeProject) p.set('p', state.activeProject);
            const n = state.selectedNode && state.selectedNode.id;
            if (n) p.set('n', n);
            if (state.focusNode) {
                p.set('focus', state.focusIsolate ? 'solo' : 'dim');
            } else if (n) {
                p.set('focus', 'off');
            }
            const tab = state.activeTab
                ? (state.activeTab === 'discover' && state.discoverSub
                    ? `discover:${state.discoverSub}` : state.activeTab)
                : '';
            if (tab) p.set('tab', tab);
            if (state.nodeFilters && state.nodeFilters.size) {
                p.set('nf', [...state.nodeFilters].join(','));
            }
            if (state.edgeFilters && state.edgeFilters.size) {
                p.set('ef', [...state.edgeFilters].join(','));
            }
            if (state.searchQuery) p.set('q', state.searchQuery);
            return p;
        }

        function urlStateSearch() {
            const s = urlStateParams().toString();
            return s ? '?' + s : '';
        }

        // Continuous change: debounced replaceState. Guarded so it never
        // clobbers a deep link before the URL state has been applied.
        function writeUrlState() {
            clearTimeout(urlWriteTimer);
            urlWriteTimer = setTimeout(() => {
                if (!graphInitialized) return;
                window.history.replaceState(null, '', urlStateSearch());
            }, URL_SYNC_MS);
        }

        // Real navigation: a fresh history entry, so Back retraces it. Skipped
        // while a tour or walk is live — those select nodes programmatically
        // and would otherwise spam the history with every stop.
        function pushUrlState() {
            if (!graphInitialized) return;
            if (typeof tourState !== 'undefined' && tourState.active) return;
            if (typeof walkPlay !== 'undefined' && walkPlay.active) return;
            window.history.pushState(null, '', urlStateSearch());
        }

        // Drive the UI from a parsed URL snapshot. Runs once after a load,
        // and again on every `popstate`. Replaying suppresses the history
        // machinery so a Back press doesn't re-record what it just undid.
        function applyUrlState(parsed) {
            const u = parsed || readUrlState();
            if (u.nf) { state.nodeFilters.clear(); u.nf.forEach(x => state.nodeFilters.add(x)); }
            else state.nodeFilters.clear();
            if (u.ef) { state.edgeFilters.clear(); u.ef.forEach(x => state.edgeFilters.add(x)); }
            else state.edgeFilters.clear();
            refreshFilterChips();
            applyFilters();
            if (u.tab) setUiTab(u.tab);
            if (u.q) {
                const s = document.getElementById('search');
                if (s) { s.value = u.q; refreshSuggestions(u.q); }
            }
            if (u.n && state.nodeById && state.nodeById.has(u.n)) {
                const node = state.nodeById.get(u.n);
                const prev = state.suppressHistory;
                state.suppressHistory = true;
                handleClick(null, node);
                state.suppressHistory = prev;
                // This is the start of the trail, not a step back from it —
                // the breadcrumb should read as a single entry.
                state.history = [node.id];
                state.historyIndex = 0;
                updateNavbar();
                if (u.focus === 'solo') {
                    state.focusIsolate = true;
                    frameNodeSet(state.focusSet, 700);
                } else if (u.focus === 'off') {
                    exitFocus();
                }
                syncSoloButton();
                bumpGraphStyles();
            } else if (state.selectedNode) {
                // Backed onto an entry with no node: drop the selection the
                // way the in-app back button would.
                clearSelection();
                frameGraph(700);
            }
        }

        // Flip the sidebar chips' pressed state to match the filters, without
        // rebuilding them (they're already on the page).
        function refreshFilterChips() {
            document.querySelectorAll('#node-filter .filter-chip').forEach(chip =>
                chip.classList.toggle('active', state.nodeFilters.has(chip.dataset.type)));
            document.querySelectorAll('#edge-filter .filter-chip').forEach(chip =>
                chip.classList.toggle('active', state.edgeFilters.has(chip.dataset.type)));
        }

        // `tab` is `graph` / `catalog` / `discover` or `discover:<sub>`.
        function setUiTab(tab) {
            if (!tab) return;
            const [panel, sub] = tab.split(':');
            if (state.showPanelTab) state.showPanelTab(panel);
            else {
                document.querySelectorAll('.panel-tab').forEach(t =>
                    t.classList.toggle('active', t.dataset.tab === panel));
                document.querySelectorAll('.tab-pane').forEach(pa =>
                    pa.classList.toggle('active', pa.dataset.tab === panel));
                state.activeTab = panel;
            }
            if (sub && state.showDiscoverSub) state.showDiscoverSub(sub);
        }

        function wireUrlState() {
            // Browser Back/Forward: replay the URL snapshot instead of
            // leaving the app. What the snapshot carries (selected node,
            // focus, filters, tab) is exactly what the in-app back button
            // moves through, so the two controls agree.
            window.addEventListener('popstate', () => applyUrlState(readUrlState()));
            const copyBtn = document.getElementById('copy-link-btn');
            if (copyBtn) copyBtn.addEventListener('click', copyCurrentLink);
        }

        // Copy a link that reproduces the current view. Flushes the debounce
        // first so the URL matches the view exactly.
        async function copyCurrentLink() {
            clearTimeout(urlWriteTimer);
            window.history.replaceState(null, '', urlStateSearch());
            try {
                await navigator.clipboard.writeText(window.location.href);
                const btn = document.getElementById('copy-link-btn');
                if (btn) {
                    const t = btn.title;
                    btn.title = 'Copied!';
                    setTimeout(() => btn.title = t, 1500);
                }
            } catch {
                // Clipboard blocked — the URL bar already shows the link.
            }
        }

        wireUrlState();
