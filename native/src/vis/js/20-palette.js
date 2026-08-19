        // ─── Command palette (⌘K) + shortcut sheet (?) ──────
        // One input that dispatches to everything the app already does, plus
        // a single declarative registry of the global bindings so the `?`
        // sheet can't drift from the keys. New global bindings register here;
        // per-mode keys (tour / walk transport) stay in their own modules and
        // are listed statically so the sheet is complete.

        const KEYMAP = [
            { group: 'Global', keys: '⌘K / Ctrl+K', action: 'Command palette' },
            { group: 'Global', keys: '?', action: 'Keyboard shortcuts' },
            { group: 'Global', keys: 't', action: 'Go to the guided tour' },
            { group: 'Global', keys: 'r', action: 'Reset view' },
            { group: 'Global', keys: '[', action: 'Toggle sidebar' },
            { group: 'Global', keys: 'Backspace / Shift+Backspace', action: 'Back / forward' },
            { group: 'Global', keys: 'Tab / Shift+Tab', action: 'Step to next / previous neighbour' },
            { group: 'Global', keys: 'Esc', action: 'Exit focus / close' },
            { group: 'Global', keys: '1–6', action: 'Face projections · 0 = 3D' },
            { group: 'Global', keys: 'Right-click a node', action: 'Summary card — details, zoom, copy id' },
            { group: 'Guided tour', keys: 'Space', action: 'Play / pause' },
            { group: 'Guided tour', keys: '← / →', action: 'Previous / next stop' },
            { group: 'Guided tour', keys: 'S', action: 'Playback speed' },
            { group: 'Guided tour', keys: 'C / W', action: 'Code / warnings' },
            { group: 'Guided tour', keys: 'O / D / J', action: 'Solo / details / plan JSON' },
            { group: 'Guided tour', keys: 'Esc', action: 'Exit the tour' },
            { group: 'Graph walk', keys: 'Space', action: 'Play / pause' },
            { group: 'Graph walk', keys: '← / →', action: 'Previous / next hop' },
            { group: 'Graph walk', keys: 'S / R / D', action: 'Speed / edge flow / details' },
            { group: 'Graph walk', keys: 'F / L', action: 'Cascade layout / labels' },
            { group: 'Graph walk', keys: 'N', action: 'Reached nodes — list every hop' },
            { group: 'Graph walk', keys: 'Esc', action: 'Exit the walk' },
        ];

        function openPalette() {
            document.getElementById('palette-overlay').classList.add('visible');
            const input = document.getElementById('palette-input');
            // Presets load on first reveal; if they're already here, refresh
            // is a no-op.
            loadInsights();
            input.value = '';
            renderPalette('');
            input.focus();
        }

        function closePalette() {
            document.getElementById('palette-overlay').classList.remove('visible');
        }

        function paletteVisible() {
            return document.getElementById('palette-overlay').classList.contains('visible');
        }

        function openShortcuts() {
            renderShortcuts();
            document.getElementById('shortcuts-overlay').classList.add('visible');
        }

        function closeShortcuts() {
            document.getElementById('shortcuts-overlay').classList.remove('visible');
        }

        function shortcutsVisible() {
            return document.getElementById('shortcuts-overlay').classList.contains('visible');
        }

        // ─── Palette items ───────────────────────────────────
        // Three sources, each filtered by its prefix so a keystroke scopes
        // the search instead of widening it.

        const PALETTE_ACTIONS = [
            { name: 'Start a guided tour', run: () => { showPanel('discover'); showSub('tour'); const i = document.getElementById('tour-input'); if (i) i.focus(); } },
            { name: 'Go to search', run: () => { showPanel('discover'); showSub('search'); const i = document.getElementById('search'); if (i) i.focus(); } },
            { name: 'Go to chat', run: () => { showPanel('discover'); showSub('chat'); const i = document.getElementById('chat-input'); if (i) i.focus(); } },
            { name: 'Go to insights', run: () => { showPanel('discover'); showSub('insights'); } },
            { name: 'Toggle solo (focus isolate)', run: toggleFocusSolo },
            { name: 'Toggle boundary box', run: () => { const b = document.getElementById('toggle-box'); if (b) b.click(); } },
            { name: 'Toggle auto-spin', run: () => { const b = document.getElementById('toggle-spin'); if (b) b.click(); } },
            { name: 'Detect cycles', run: () => { showPanel('graph'); detectAndShowCycles(); } },
            { name: 'Reset view', run: resetView },
            { name: 'Download graph JSON', run: downloadGraph },
            { name: 'Download index JSON', run: downloadIndex },
            { name: 'Open settings', run: openSettings },
            { name: 'Copy link to this view', run: copyCurrentLink },
            { name: 'Switch project', run: reopenKbManager },
            { name: 'Collapse sidebar', run: () => { const b = document.getElementById('sidebar-collapse'); if (b) b.click(); } },
        ];

        function buildPaletteItems(raw) {
            const q = (raw || '').trim().toLowerCase();
            let prefix = null;
            if (q.startsWith('>')) prefix = 'action';
            else if (q.startsWith('#')) prefix = 'node';
            else if (q.startsWith('?')) prefix = 'insight';
            const query = prefix ? q.slice(1).trim() : q;

            const items = [];
            if (!prefix || prefix === 'node') {
                let nodes = state.graph.nodes;
                if (query) {
                    nodes = nodes.filter(n =>
                        (n.name || '').toLowerCase().includes(query) ||
                        (n.id || '').toLowerCase().includes(query));
                    nodes = [...nodes].sort((a, b) =>
                        (a.name.toLowerCase().indexOf(query) - b.name.toLowerCase().indexOf(query)) ||
                        a.name.localeCompare(b.name));
                }
                nodes.slice(0, 25).forEach(n => items.push({
                    kind: 'node',
                    label: truncateName(n.name),
                    group: n.group,
                    sub: [n.group, n.file].filter(Boolean).join(' · '),
                    run: () => { handleClick(null, n); focusNode(n); },
                }));
            }
            if (!prefix || prefix === 'action') {
                const actions = query
                    ? PALETTE_ACTIONS.filter(a => a.name.toLowerCase().includes(query))
                    : PALETTE_ACTIONS;
                actions.forEach(a => items.push({
                    kind: 'action',
                    label: a.name,
                    sub: 'action',
                    run: a.run,
                }));
                // Saved tours replay without a model call — ideal palette items.
                loadTourHistory().slice(0, 5).forEach(e => items.push({
                    kind: 'tour',
                    label: `Replay: ${e.title || e.query}`,
                    sub: `${e.stops} stop${e.stops === 1 ? '' : 's'} · tour`,
                    run: () => { showPanel('discover'); showSub('tour'); replayTourFromHistory(e.id); },
                }));
            }
            if (!prefix || prefix === 'insight') {
                const presets = query
                    ? insState.presets.filter(p => `${p.name} ${p.description || ''}`.toLowerCase().includes(query))
                    : insState.presets;
                presets.slice(0, 20).forEach(p => items.push({
                    kind: 'insight',
                    label: p.name,
                    sub: p.description || 'insight',
                    run: () => { showPanel('discover'); showSub('insights'); choosePreset(p); },
                }));
            }
            return items;
        }

        let paletteCursor = -1;
        function renderPalette(raw) {
            const box = document.getElementById('palette-results');
            const empty = document.getElementById('palette-empty');
            const items = buildPaletteItems(raw);
            paletteCursor = -1;
            empty.hidden = items.length > 0;
            box.innerHTML = '';
            if (!items.length) return;
            items.forEach((it, i) => {
                const row = document.createElement('div');
                row.className = `palette-item kind-${it.kind}`;
                row.dataset.index = i;
                row.innerHTML = `
                    <span class="palette-item-icon">${it.kind === 'node' ? nodeIconSvg(it.group || 'Default') : it.kind === 'action' ? '›' : it.kind === 'tour' ? '↻' : '?'}</span>
                    <span class="palette-item-label">${escapeHtml(it.label)}</span>
                    <span class="palette-item-sub">${escapeHtml(it.sub)}</span>
                `;
                row.addEventListener('mousedown', evt => {
                    evt.preventDefault();
                    runPaletteItem(it);
                });
                box.appendChild(row);
            });
        }

        function runPaletteItem(it) {
            closePalette();
            it.run();
        }

        function paletteMove(dir) {
            const rows = document.querySelectorAll('#palette-results .palette-item');
            if (!rows.length) return;
            paletteCursor = (paletteCursor + dir + rows.length) % rows.length;
            rows.forEach((r, i) => r.classList.toggle('active', i === paletteCursor));
            rows[paletteCursor].scrollIntoView({ block: 'nearest' });
        }

        function showPanel(name) {
            if (state.showPanelTab) state.showPanelTab(name);
        }
        function showSub(name) {
            if (state.showDiscoverSub) state.showDiscoverSub(name);
        }

        function gotoTour() {
            showPanel('discover');
            showSub('tour');
            const input = document.getElementById('tour-input');
            if (input) input.focus();
        }

        // ─── Shortcut sheet ─────────────────────────────────
        function renderShortcuts() {
            const body = document.getElementById('shortcuts-body');
            const groups = {};
            KEYMAP.forEach(k => {
                (groups[k.group] = groups[k.group] || []).push(k);
            });
            body.innerHTML = Object.entries(groups).map(([group, rows]) => `
                <div class="shortcuts-group">
                    <div class="shortcuts-group-title">${escapeHtml(group)}</div>
                    ${rows.map(r => `
                        <div class="shortcuts-row">
                            <span class="shortcuts-keys">${escapeHtml(r.keys)}</span>
                            <span class="shortcuts-action">${escapeHtml(r.action)}</span>
                        </div>`).join('')}
                </div>`).join('');
        }

        function wirePalette() {
            const input = document.getElementById('palette-input');
            input.addEventListener('input', () => renderPalette(input.value));
            input.addEventListener('keydown', (e) => {
                if (e.key === 'ArrowDown') { e.preventDefault(); paletteMove(1); }
                else if (e.key === 'ArrowUp') { e.preventDefault(); paletteMove(-1); }
                else if (e.key === 'Enter') {
                    e.preventDefault();
                    const items = buildPaletteItems(input.value);
                    const idx = paletteCursor >= 0 ? paletteCursor : 0;
                    if (items[idx]) runPaletteItem(items[idx]);
                } else if (e.key === 'Escape') {
                    e.preventDefault();
                    closePalette();
                }
            });
            document.getElementById('shortcuts-close').addEventListener('click', closeShortcuts);
            document.getElementById('shortcuts-overlay').addEventListener('click', (e) => {
                if (e.target === document.getElementById('shortcuts-overlay')) closeShortcuts();
            });

            // Always-visible triggers (sidebar header) + the Start-here "Try"
            // buttons all point at the same three features, so the palette,
            // the shortcut sheet and the share link each have a click path.
            document.getElementById('palette-open-btn').addEventListener('click', openPalette);
            document.getElementById('shortcuts-open-btn').addEventListener('click', openShortcuts);
            document.getElementById('try-palette').addEventListener('click', openPalette);
            document.getElementById('try-shortcuts').addEventListener('click', openShortcuts);
            document.getElementById('try-copy-link').addEventListener('click', copyCurrentLink);
        }

        // Global bindings, capture phase so they beat the older bubble-phase
        // handlers when they own the key (e.g. closing the palette over the
        // info panel's Esc).
        document.addEventListener('keydown', (e) => {
            if ((e.metaKey || e.ctrlKey) && (e.key === 'k' || e.key === 'K')) {
                e.preventDefault();
                e.stopImmediatePropagation();
                if (paletteVisible()) closePalette(); else openPalette();
                return;
            }
            if (paletteVisible()) return; // the palette input owns its keys
            if (shortcutsVisible()) {
                if (e.key === 'Escape') { e.preventDefault(); e.stopImmediatePropagation(); closeShortcuts(); }
                return;
            }
            const t = e.target;
            const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable);
            if (typing) return;
            if (e.key === '?') {
                e.preventDefault();
                e.stopImmediatePropagation();
                openShortcuts();
                return;
            }
            // The walk and tour own R/Space etc. while they're live; don't
            // yank those keys out from under them.
            const immersive = (typeof walkPlay !== 'undefined' && walkPlay.active) ||
                (typeof tourState !== 'undefined' && tourState.active);
            if ((e.key === 'r' || e.key === 'R') && !immersive) {
                e.preventDefault();
                e.stopImmediatePropagation();
                resetView();
                return;
            }
            if ((e.key === 't' || e.key === 'T') && !immersive) {
                e.preventDefault();
                e.stopImmediatePropagation();
                gotoTour();
                return;
            }
        }, true);

        wirePalette();
