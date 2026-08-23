        // ─── Search ─────────────────────────────────────────

        function wireSearch() {
            const input = document.getElementById('search');
            const clear = document.getElementById('search-clear');

            input.addEventListener('input', e => {
                clear.classList.toggle('visible', !!e.target.value);
                refreshSuggestions(e.target.value);
            });
            input.addEventListener('focus', () => refreshSuggestions(input.value));
            input.addEventListener('keydown', handleSearchKey);

            clear.addEventListener('click', () => {
                input.value = '';
                clear.classList.remove('visible');
                refreshSuggestions('');
                input.focus();
            });

            // Light up the whole match set at once: solo mode draws them fresh,
            // normal mode dims the rest and frames the set. Capped so thousands
            // of matches stay legible.
            const plotAll = document.getElementById('search-plot-all');
            if (plotAll) plotAll.addEventListener('click', () => {
                const matches = state.searchMatches || [];
                if (!matches.length) return;
                lightUpNodes(matches.slice(0, SOLO_MAX_NODES).map(n => n.id));
            });
        }

        // The suggestion box, off the shared `searchNodes` (02-dialogs.js).
        //
        // Async because in server mode the answer comes from the server — the
        // page has no name column it can afford to scan. A monotonic token
        // drops stale responses, so typing quickly leaves the box showing the
        // last query rather than whichever request finished last.
        let suggestToken = 0;
        async function refreshSuggestions(query) {
            const container = document.getElementById('search-suggestions');
            const meta = document.getElementById('search-meta-count');
            const q = query.trim().toLowerCase();
            state.searchQuery = q;
            const filterActive = state.nodeFilters.size > 0;
            const token = ++suggestToken;

            // Fifty are shown; "light up … in graph" draws from the rest, so the
            // request covers both in one pass instead of two.
            const found = await searchNodes(q, {
                limit: q ? Math.max(50, SOLO_MAX_NODES) : 50,
                types: filterActive ? state.nodeFilters : null,
            });
            if (token !== suggestToken) return;

            const sorted = found.nodes;
            const display = sorted.slice(0, 50);
            state.currentSuggestions = display;
            state.searchMatches = q ? sorted : [];
            state.suggestionIndex = -1;

            // Query-driven, like the Semantic/Hybrid panes: render cards only
            // when there is something to search for. A bare focus no longer
            // dumps fifty nodes into the pane — the meta line still reports
            // the total. Filter-chip clicks call this with the current (often
            // empty) query, so they update the count without surfacing a list.
            container.innerHTML = '';
            if (q) {
                if (display.length === 0) {
                    container.innerHTML = '<div class="suggestion-empty">No matching nodes</div>';
                } else {
                    display.forEach((n, i) => {
                        const lineLabel = n.startLine
                            ? `L${n.startLine}${n.endLine && n.endLine !== n.startLine ? '–' + n.endLine : ''}`
                            : '';
                        const metaParts = [n.file, n.group, lineLabel].filter(Boolean);
                        const item = document.createElement('div');
                        item.className = 'suggestion-item';
                        item.dataset.index = i;
                        item.innerHTML = `
                            <div class="suggestion-head">
                                ${nodeIconSvg(n.group)}
                                <span class="suggestion-name">${escapeHtml(truncateName(n.name))}</span>
                            </div>
                            ${metaParts.length ? `<div class="suggestion-meta">${escapeHtml(metaParts.join(' · '))}</div>` : ''}
                        `;
                        item.addEventListener('mousedown', evt => {
                            evt.preventDefault();
                            selectSearchResult(n, evt);
                        });
                        container.appendChild(item);
                    });
                }
            }

            const total = found.total;
            if (q || filterActive) {
                meta.textContent = `${Math.min(display.length, total)} of ${total}` +
                    (filterActive ? ` · ${state.nodeFilters.size} type filter(s)` : '');
            } else {
                meta.textContent = `${state.nodeCount || 0} nodes`;
            }

            syncPlotAllButton(document.getElementById('search-plot-all'), state.searchMatches);
            writeUrlState();
        }

        function handleSearchKey(e) {
            const items = document.querySelectorAll('.suggestion-item');
            if (e.key === 'ArrowDown') {
                e.preventDefault();
                state.suggestionIndex = Math.min(state.suggestionIndex + 1, items.length - 1);
                updateSuggestionHighlight();
            } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                state.suggestionIndex = Math.max(state.suggestionIndex - 1, 0);
                updateSuggestionHighlight();
            } else if (e.key === 'Enter') {
                e.preventDefault();
                const idx = state.suggestionIndex >= 0 ? state.suggestionIndex : 0;
                if (state.currentSuggestions[idx]) {
                    selectSearchResult(state.currentSuggestions[idx], e);
                }
            } else if (e.key === 'Escape') {
                document.getElementById('search').value = '';
                document.getElementById('search-clear').classList.remove('visible');
                refreshSuggestions('');
                document.getElementById('search').blur();
            }
        }

        function updateSuggestionHighlight() {
            const items = document.querySelectorAll('.suggestion-item');
            items.forEach((it, i) => it.classList.toggle('active', i === state.suggestionIndex));
            const el = items[state.suggestionIndex];
            if (el) el.scrollIntoView({ block: 'nearest' });
        }

        // `evt` is optional: with ⌘/Ctrl held the pick is added to whatever is
        // already on the canvas instead of replacing it (solo mode only —
        // below the threshold everything is drawn anyway).
        function selectSearchResult(n, evt) {
            document.getElementById('search').value = n.name;
            if (state.pathMode) exitPathMode();
            if (evt && (evt.metaKey || evt.ctrlKey)) state._viewMerge = true;
            handleClick(null, n);
            focusNode(n);
        }

        // focusNode() is a renderer dispatcher — see 10-render-core.js. How a
        // node is centred depends on whether there is a camera to fly.

