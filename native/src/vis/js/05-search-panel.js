        // ─── Path mode + the hybrid retrieval call ──────────
        //
        // What is left here after the Ask column took the query inputs
        // (`js/25-ask.js`): "find a path between these two nodes", which is
        // a two-click gesture on the canvas rather than a query, and the
        // `fetchHybrid` wrapper the Ask column and the palette both call.

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
            // A walk in progress owns the canvas — selecting through it would
            // rebuild the solo view and tear down the animation mid-flight.
            // But "what *is* that one?" is the question a reveal provokes on
            // every hop, and until now the canvas answered it with nothing:
            // the click was swallowed, and hover is suppressed during a walk
            // too (see handleNodeHover). So the click opens the summary card
            // instead — it reads state and changes none, which is exactly what
            // this guard is protecting.
            if (state.walkActive) {
                if (event) event.stopPropagation();
                // Same reasoning as the tour below: someone reading a node is
                // not someone who wants the next hop to reframe the camera out
                // from under them. Pausing is one keystroke to undo.
                if (walkPlay.playing) setWalkPlaying(false);
                openNodeMenuAt(d, event);
                return;
            }
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

        async function runFindPathTo(targetId) {
            if (!state.pathSource || !targetId) return;
            showPathResult('Searching for a path…');
            let result;
            try {
                result = await findPath(state.pathSource, targetId);
            } catch (err) {
                showPathResult(`Path search failed — ${err.message || err}`, false);
                return;
            }
            const info = document.getElementById('info');
            const hint = info.querySelector('.path-hint');
            if (result.found) {
                showPathResult(`${result.hops} hop(s): ${result.path.join(' → ')}`, true);
                if (hint) hint.textContent = 'Path found! Click "Find Path" to find another.';
                // A path nobody can see is only half an answer: in solo mode
                // most of its hops are not on the canvas yet.
                if (state.soloOnly) plotNodes(result.ids);
            } else {
                showPathResult('No path found from ' + truncateName(state.pathSource) + ' to ' + truncateName(targetId), false);
            }
        }

        // `search_kb` returns RankedContext { items: [ContextItem { …,
        // matched_by: "semantic" | "keyword" | "graph", hop, distance }] } —
        // flat. `matched_by` and `hop` are the provenance the rows render;
        // they cost nothing to carry and everything to re-derive.
        async function fetchHybrid(query, k, hops) {
            // Snippets are not shown in the rows, so skip the capture and
            // the transfer.
            const body = { query, k, include_snippets: false };
            if (hops != null) body.hops = hops;
            if (state.semDest) body.dest = state.semDest;
            const res = await fetch('/api/search/hybrid', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body)
            });
            if (!res.ok) throw new Error(await readErr(res));
            const data = await res.json();
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
                hop: it.hop,
                matched_by: it.matched_by || '',
                snippet: it.snippet || null
            }));
            // Stash the server-reported dest so the block's meta line can say
            // "results from <backend>" — it matters when serve is configured
            // with more than one destination.
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

