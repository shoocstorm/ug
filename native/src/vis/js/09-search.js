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
        }

        function refreshSuggestions(query) {
            const container = document.getElementById('search-suggestions');
            const meta = document.getElementById('search-meta-count');
            const q = query.trim().toLowerCase();
            const filterActive = state.nodeFilters.size > 0;

            const candidates = state.graph.nodes.filter(n => {
                if (filterActive && !state.nodeFilters.has(n.group)) return false;
                if (!q) return true;
                return n.name.toLowerCase().includes(q) || n.id.toLowerCase().includes(q);
            });

            const sorted = q
                ? candidates.sort((a, b) => {
                    const ai = a.name.toLowerCase().indexOf(q);
                    const bi = b.name.toLowerCase().indexOf(q);
                    if (ai !== bi) return ai - bi;
                    return a.name.length - b.name.length;
                })
                : candidates;

            const display = sorted.slice(0, 50);
            state.currentSuggestions = display;
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
                            selectSearchResult(n);
                        });
                        container.appendChild(item);
                    });
                }
            }

            const total = candidates.length;
            if (q || filterActive) {
                meta.textContent = `${Math.min(display.length, total)} of ${total}` +
                    (filterActive ? ` · ${state.nodeFilters.size} type filter(s)` : '');
            } else {
                meta.textContent = `${state.graph.nodes.length} nodes`;
            }
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
                    selectSearchResult(state.currentSuggestions[idx]);
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

        function selectSearchResult(n) {
            document.getElementById('search').value = n.name;
            if (state.pathMode) exitPathMode();
            handleClick(null, n);
            focusNode(n);
        }

        function focusNode(n) {
            if (!Graph || n == null) return;
            // Wait one frame so any panel toggles (info open/close) commit their
            // layout before the camera flies.
            requestAnimationFrame(() => {
                // Centre the camera on the node at a consistent, comfortable
                // distance. Isolation is conveyed by the focus dimming, not by the
                // camera — fitting a scattered neighbourhood tends to either
                // over-zoom (tiny clusters bloom out) or under-zoom (sparse, empty
                // view), so we keep the framing simple and predictable.
                const x = +n.x || 0, y = +n.y || 0, z = +n.z || 0;
                // Total camera-to-node distance (the (d,d,d) offset has magnitude
                // sqrt(3)*d). Pulled well back so a generous slice of the
                // surrounding neighbourhood stays in frame on focus.
                const d = 480 / Math.sqrt(3);
                Graph.cameraPosition(
                    { x: x + d, y: y + d, z: z + d },
                    { x, y, z },
                    800);
            });
        }

