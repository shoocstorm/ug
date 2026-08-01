        // ─── Search panel: Keyword / Semantic / Hybrid ──────
        //
        // Keyword is client-side over the loaded graph.json and always
        // available; Semantic and Hybrid are DB-backed and get disabled by
        // the capabilities probe when the store isn't queryable. The three
        // share one tab strip; the body swaps between the keyword UI and
        // the vector UI.

        function selectSemMode(mode) {
            state.semMode = mode;
            document.querySelectorAll('.sem-mode').forEach(b => {
                b.classList.toggle('active', b.dataset.mode === mode);
            });
            const isKeyword = mode === 'keyword';
            const kw = document.querySelector('.sem-ui[data-ui="keyword"]');
            const vec = document.querySelector('.sem-ui[data-ui="vector"]');
            if (kw) kw.hidden = !isKeyword;
            if (vec) vec.hidden = isKeyword;
        }

        function wireSemanticPanel() {
            state.semMode = 'keyword';
            document.querySelectorAll('.sem-mode').forEach(btn => {
                btn.addEventListener('click', () => {
                    if (btn.disabled) return;
                    selectSemMode(btn.dataset.mode);
                });
            });

            const input = document.getElementById('sem-input');
            const exec = document.getElementById('sem-exec');
            exec.addEventListener('click', runSemanticSearch);
            input.addEventListener('keydown', e => {
                if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                    e.preventDefault();
                    runSemanticSearch();
                }
            });
        }

        function wireFindPathBtn() {
            const btn = document.getElementById('find-path-btn');
            btn.addEventListener('click', () => {
                if (!state.selectedNode) return;
                state.pathSource = state.selectedNode.id;
                state.pathMode = true;
                document.body.classList.add('path-mode', 'has-source');
                const info = document.getElementById('info');
                const hint = info.querySelector('.path-hint') || createPathHint();
                hint.innerHTML = `<span>Find path from <strong>${escapeHtml(truncateName(state.pathSource))}</strong></span>
                    <button class="path-cancel-btn" title="Cancel">✕</button>`;
                hint.classList.add('visible');
                showPathResult('');

                hint.querySelector('.path-cancel-btn').addEventListener('click', (e) => {
                    e.stopPropagation();
                    exitPathMode();
                });
            });
        }

        function handleNodeClick(event, d) {
            if (state.pathMode && state.pathSource) {
                event.stopPropagation();
                runFindPathTo(d.id);
                return;
            }
            // Poking around the graph mid-tour means the user wants to look at
            // something; don't yank the camera away on the next auto-advance.
            if (tourState.active && tourState.playing) pauseTour();
            handleClick(event, d);
        }

        function createPathHint() {
            const info = document.getElementById('info');
            const hint = document.createElement('div');
            hint.className = 'path-hint';
            info.querySelector('.drag-handle').insertAdjacentElement('afterend', hint);
            return hint;
        }

        function showPathResult(text, found) {
            const info = document.getElementById('info');
            let result = info.querySelector('.info-path-result');
            if (!result) {
                result = document.createElement('div');
                result.className = 'info-path-result';
                const hint = info.querySelector('.path-hint');
                if (hint) hint.insertAdjacentElement('afterend', result);
                else info.querySelector('.drag-handle').insertAdjacentElement('afterend', result);
            }
            if (!text) {
                result.classList.remove('visible');
                return;
            }
            result.textContent = text;
            result.classList.add('visible');
            result.classList.toggle('found', found === true);
            result.classList.toggle('not-found', found === false);
        }

        function exitPathMode() {
            state.pathSource = null;
            state.pathMode = false;
            document.body.classList.remove('path-mode', 'has-source');
            const info = document.getElementById('info');
            const hint = info.querySelector('.path-hint');
            if (hint) hint.classList.remove('visible');
            showPathResult('');
        }

        function runFindPathTo(targetId) {
            if (!state.pathSource || !targetId) return;
            const result = findPath(state.pathSource, targetId);
            const info = document.getElementById('info');
            const hint = info.querySelector('.path-hint');
            if (result.found) {
                showPathResult(`${result.hops} hop(s): ${result.path.join(' → ')}`, true);
                if (hint) hint.textContent = 'Path found! Click "Find Path" to find another.';
            } else {
                showPathResult('No path found from ' + truncateName(state.pathSource) + ' to ' + truncateName(targetId), false);
            }
        }

        async function runSemanticSearch() {
            const input = document.getElementById('sem-input');
            const exec = document.getElementById('sem-exec');
            const statusEl = document.getElementById('sem-status');
            const resultsEl = document.getElementById('sem-results');
            const query = input.value.trim();

            statusEl.classList.remove('error');
            if (!query) {
                statusEl.textContent = 'Enter a query to search.';
                return;
            }
            if (state.semInFlight) return;

            const k = clampInt(document.getElementById('sem-k').value, 1, 50, 10);
            state.semInFlight = true;
            exec.disabled = true;
            statusEl.textContent = state.semMode === 'hybrid'
                ? 'Running hybrid search…'
                : 'Running semantic search…';
            resultsEl.innerHTML = '';

            const t0 = performance.now();
            try {
                const hits = state.semMode === 'hybrid'
                    ? await fetchHybrid(query, k)
                    : await fetchSemantic(query, k);
                const ms = Math.round(performance.now() - t0);
                const destNote = hits.dest ? ` · from ${hits.dest}` : '';
                if (!hits.length) {
                    resultsEl.innerHTML = '<div class="sem-empty">No results.</div>';
                    statusEl.textContent = `0 results · ${ms} ms${destNote}`;
                } else {
                    renderSemanticHits(resultsEl, hits, state.semMode);
                    statusEl.textContent = `${hits.length} result${hits.length === 1 ? '' : 's'} · ${ms} ms${destNote}`;
                }
            } catch (err) {
                statusEl.classList.add('error');
                statusEl.textContent = `Search failed: ${err.message || err}`;
                resultsEl.innerHTML = '';
                console.error(err);
            } finally {
                state.semInFlight = false;
                exec.disabled = false;
            }
        }

        async function fetchSemantic(query, k) {
            const body = { query, k };
            if (state.semDest) body.dest = state.semDest;
            const res = await fetch('/api/search/semantic', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body)
            });
            if (!res.ok) throw new Error(await readErr(res));
            const data = await res.json();
            const hits = (data.hits || []).map(h => ({
                id: h.id,
                name: h.name,
                node_type: h.node_type,
                file: h.file,
                start_line: h.start_line,
                end_line: h.end_line,
                description: h.description,
                score: h.distance,
                snippet: null
            }));
            // Stash the server-reported dest so the status line can
            // surface "results from <backend>" — important when serve
            // is configured with multiple destinations.
            hits.dest = data.dest || state.semDest;
            return hits;
        }

        async function fetchHybrid(query, k) {
            const body = { query, k, include_snippets: true };
            if (state.semDest) body.dest = state.semDest;
            const res = await fetch('/api/search/hybrid', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body)
            });
            if (!res.ok) throw new Error(await readErr(res));
            const data = await res.json();
            // search_kb returns RankedContext { items: [ContextItem { id, name,
            // node_type, file, start_line, end_line, description, distance,
            // hop, snippet }] } — flat, no nested node wrapper.
            const items = data.items || [];
            const hits = items.map(it => ({
                id: it.id,
                name: it.name,
                node_type: it.node_type,
                file: it.file,
                start_line: it.start_line,
                end_line: it.end_line,
                description: it.description,
                score: it.distance,
                snippet: it.snippet || null
            }));
            hits.dest = data.dest || state.semDest;
            return hits;
        }

        async function readErr(res) {
            try {
                const j = await res.json();
                return j.error || `HTTP ${res.status}`;
            } catch {
                return `HTTP ${res.status}`;
            }
        }

        function clampInt(v, lo, hi, fallback) {
            const n = parseInt(v, 10);
            if (Number.isNaN(n)) return fallback;
            return Math.max(lo, Math.min(hi, n));
        }

