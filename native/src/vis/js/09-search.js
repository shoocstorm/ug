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
            input.addEventListener('blur', () => setTimeout(() => {
                document.getElementById('search-suggestions').classList.remove('visible');
            }, 200));

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
            const searchInput = document.getElementById('search');
            const q = query.trim().toLowerCase();
            const filterActive = state.nodeFilters.size > 0;
            const searchFocused = document.activeElement === searchInput;

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

            // Only render/open the dropdown when the user is actively searching.
            // Filter-chip clicks must not pop the suggestion list — that re-flows
            // the sidebar and feels like the screen jumping.
            if (searchFocused) {
                container.innerHTML = '';
                if (display.length === 0) {
                    container.innerHTML = '<div class="suggestion-empty">No matching nodes</div>';
                } else {
                    display.forEach((n, i) => {
                        const item = document.createElement('div');
                        item.className = 'suggestion-item';
                        item.dataset.index = i;
                        const color = config.getColor(n.group);
                        item.innerHTML = `
                            <span class="suggestion-dot" style="background:${color};color:${color}"></span>
                            <span class="suggestion-name">${escapeHtml(truncateName(n.name))}</span>
                            <span class="suggestion-type">${n.group}</span>
                        `;
                        item.addEventListener('mousedown', evt => {
                            evt.preventDefault();
                            selectSearchResult(n);
                        });
                        container.appendChild(item);
                    });
                }
                container.classList.add('visible');
            } else {
                container.classList.remove('visible');
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
                document.getElementById('search-suggestions').classList.remove('visible');
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
            document.getElementById('search-suggestions').classList.remove('visible');
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

