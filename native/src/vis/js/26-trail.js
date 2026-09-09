        // ─── The trail ──────────────────────────────────────
        //
        // What you found, kept across sessions. Two lists, one store:
        //
        //   recents  every query you ran, with its mode and what came back.
        //            Re-running one is free — the text goes back in the bar.
        //   kept     the things you chose to hold on to: a node, an answer,
        //            or a whole view (selection, focus, filters, renderer,
        //            query, and the solo set that was on the canvas).
        //
        // Per project, in browser storage, the way `tourHistoryKey`
        // (07-tour.js) and `walkHistoryKey` (18-walk.js) already are — node
        // ids only mean something inside one graph. Both of those keep their
        // own keys: they cache whole replay payloads, and sharing one quota
        // with the trail would let a large saved tour evict what you kept.

        const TRAIL_RECENTS_MAX = 20;
        const TRAIL_KEPT_MAX = 60;

        // Answers are stored so a recent can be re-read without spending the
        // model again, but a long one would eat the quota on its own.
        const TRAIL_ANSWER_CAP = 4000;

        function trailKey() {
            const p = state.capabilities && state.capabilities.project;
            return 'ug-trail:' + ((p && p.name) || 'default');
        }

        function emptyTrail() {
            return { recents: [], kept: [] };
        }

        function loadTrail() {
            try {
                const raw = localStorage.getItem(trailKey());
                const t = raw ? JSON.parse(raw) : null;
                if (!t || typeof t !== 'object') return emptyTrail();
                return {
                    recents: Array.isArray(t.recents) ? t.recents : [],
                    kept: Array.isArray(t.kept) ? t.kept : [],
                };
            } catch (e) {
                return emptyTrail();
            }
        }

        // Same shrink-then-drop contract the tour and walk histories use: a
        // quota failure must never leave the store half-written, and it must
        // never cost the user what they explicitly kept. Recents are the
        // disposable half, so they go first.
        function saveTrail(trail) {
            const t = {
                recents: trail.recents.slice(0, TRAIL_RECENTS_MAX),
                kept: trail.kept.slice(0, TRAIL_KEPT_MAX),
            };
            try {
                localStorage.setItem(trailKey(), JSON.stringify(t));
                return t;
            } catch (e) {
                const lite = {
                    recents: t.recents.slice(0, 5).map(r => ({ ...r, answer: null, cites: null })),
                    kept: t.kept,
                };
                try {
                    localStorage.setItem(trailKey(), JSON.stringify(lite));
                    return lite;
                } catch (e2) {
                    // Still over quota with only what was kept: drop the entry
                    // rather than leaving a corrupt one behind.
                    try { localStorage.removeItem(trailKey()); } catch (e3) { /* private mode */ }
                    return emptyTrail();
                }
            }
        }

        function trailId() {
            return Date.now().toString(36) + Math.random().toString(36).slice(2, 7);
        }

        // ── Recents ─────────────────────────────────────────

        // Called once per submitted query, after it has an outcome. `extra`
        // carries the answer and its citations for `answer` turns, so a
        // re-read costs nothing.
        function recordAsk(mode, query, extra) {
            const trail = loadTrail();
            // The same question asked twice is one entry, moved to the top.
            const rest = trail.recents.filter(r => !(r.mode === mode && r.query === query));
            const entry = {
                id: trailId(),
                at: Date.now(),
                mode,
                query,
                count: (extra && extra.count) != null ? extra.count : null,
                answer: extra && extra.answer ? extra.answer.slice(0, TRAIL_ANSWER_CAP) : null,
                cites: extra && extra.cites ? extra.cites.length : null,
                tourId: (extra && extra.tourId) || null,
            };
            trail.recents = [entry, ...rest];
            saveTrail(trail);
            renderTrailRecents();
            return entry;
        }

        // ── Kept ────────────────────────────────────────────

        function keepEntry(entry) {
            const trail = loadTrail();
            // Keeping the same thing twice is not two things.
            const rest = trail.kept.filter(k => k.key !== entry.key);
            trail.kept = [{ id: trailId(), at: Date.now(), ...entry }, ...rest];
            saveTrail(trail);
            renderKept();
            return true;
        }

        function dropKept(id) {
            const trail = loadTrail();
            trail.kept = trail.kept.filter(k => k.id !== id);
            saveTrail(trail);
            renderKept();
        }

        function isKept(key) {
            return loadTrail().kept.some(k => k.key === key);
        }

        // The selected node. `key` is the node id, so keeping it twice is a
        // no-op and the panel button can read its own state.
        function keepSelectedNode() {
            const n = state.selectedNode;
            if (!n) { setAskStatus('Select a node first — Keep holds on to what is open.', true); return; }
            if (isKept('node:' + n.id)) { dropKept(loadTrail().kept.find(k => k.key === 'node:' + n.id).id); }
            else keepEntry({ kind: 'node', key: 'node:' + n.id, label: n.name || n.id, nodeId: n.id, group: n.group });
            syncKeepButton();
        }

        // A whole view: what the URL already carries, plus the solo set, which
        // it does not. Restoring feeds the params back through the same
        // applier a deep link uses, so there is one restore path, not two.
        function keepCurrentView(label) {
            const params = urlStateParams();
            const solo = state.soloOnly && state.view && state.view.nodes
                ? state.view.nodes.map(n => n.id).slice(0, SOLO_MAX_NODES)
                : null;
            const search = params.toString();
            keepEntry({
                kind: 'view',
                key: 'view:' + search,
                label: label || viewLabel(),
                search,
                solo,
            });
        }

        // What to call a view nobody named.
        function viewLabel() {
            const bits = [];
            if (state.selectedNode) bits.push(truncateName(state.selectedNode.name));
            if (state.nodeFilters && state.nodeFilters.size) bits.push(`${state.nodeFilters.size} type filter(s)`);
            if (state.searchQuery) bits.push(`"${state.searchQuery}"`);
            return bits.length ? bits.join(' · ') : 'the whole graph';
        }

        function keepAnswer(query, answer, cites) {
            keepEntry({
                kind: 'answer',
                key: 'answer:' + query,
                label: query,
                answer: (answer || '').slice(0, TRAIL_ANSWER_CAP),
                cites: cites || [],
            });
        }

        // ── Restoring ───────────────────────────────────────

        function openKept(entry) {
            if (entry.kind === 'node') {
                const node = state.nodeById && state.nodeById.get(entry.nodeId);
                if (!node) { setAskStatus(`"${entry.nodeId}" isn't in the loaded graph.`, true); return; }
                handleClick(null, node);
                focusNode(node);
                return;
            }
            if (entry.kind === 'view') {
                applyUrlState(readUrlStateFrom(entry.search));
                // The solo set is the part the URL cannot carry: below the
                // threshold everything is drawn anyway, so this only matters
                // in solo mode.
                if (entry.solo && entry.solo.length && state.soloOnly) plotNodes(entry.solo);
                return;
            }
            if (entry.kind === 'answer') {
                const block = askBlock('answer', entry.label);
                block.meta('kept');
                setMarkdown(block.body, entry.answer || '(no answer)', entry.cites || []);
                if (entry.cites && entry.cites.length) block.body.appendChild(buildCitationBox(entry.cites));
                hideAskOnboard();
            }
        }

        // ── Rendering ───────────────────────────────────────

        function renderKept() {
            const box = document.getElementById('ask-kept-list');
            const wrap = document.getElementById('ask-kept');
            const count = document.getElementById('ask-kept-count');
            if (!box || !wrap) return;
            const items = loadTrail().kept;
            wrap.hidden = items.length === 0;
            count.textContent = items.length ? String(items.length) : '';
            box.innerHTML = '';
            items.forEach(entry => {
                const row = document.createElement('div');
                row.className = `trail-row kind-${entry.kind}`;

                const main = document.createElement('button');
                main.type = 'button';
                main.className = 'trail-main';
                main.title = entry.kind === 'node' ? `Go to ${entry.nodeId}` : entry.label;
                main.innerHTML = `<span class="trail-kind">${entry.kind}</span>`
                    + `<span class="trail-label"></span>`
                    + `<span class="trail-when">${escapeHtml(relativeTime(entry.at))}</span>`;
                main.querySelector('.trail-label').textContent = entry.label;
                main.addEventListener('click', () => openKept(entry));

                const drop = document.createElement('button');
                drop.type = 'button';
                drop.className = 'trail-drop';
                drop.title = 'Stop keeping this';
                drop.textContent = '✕';
                drop.addEventListener('click', (e) => {
                    e.stopPropagation();
                    dropKept(entry.id);
                    syncKeepButton();
                });

                row.append(main, drop);
                box.appendChild(row);
            });
        }

        function renderTrailRecents() {
            const box = document.getElementById('ask-recents');
            if (!box) return;
            const items = loadTrail().recents;
            box.innerHTML = '';
            if (!items.length) return;

            const head = document.createElement('div');
            head.className = 'ask-onboard-head';
            head.textContent = 'Where you were';
            box.appendChild(head);

            items.slice(0, 6).forEach(entry => {
                const row = document.createElement('button');
                row.type = 'button';
                row.className = 'trail-row trail-recent';
                const outcome = entry.count != null
                    ? `${entry.count} result${entry.count === 1 ? '' : 's'}`
                    : (entry.cites != null ? `${entry.cites} source${entry.cites === 1 ? '' : 's'}` : '');
                row.innerHTML = `<span class="trail-kind">${escapeHtml(entry.mode)}</span>`
                    + `<span class="trail-label"></span>`
                    + `<span class="trail-when">${escapeHtml(outcome || relativeTime(entry.at))}</span>`;
                row.querySelector('.trail-label').textContent = entry.query;
                row.title = entry.query;
                row.addEventListener('click', () => {
                    // A saved answer re-reads for free; anything else goes
                    // back in the bar and runs again.
                    if (entry.mode === 'answer' && entry.answer) {
                        focusAsk(entry.query);
                        openKept({ kind: 'answer', label: entry.query, answer: entry.answer, cites: [] });
                        return;
                    }
                    focusAsk(entry.query, entry.mode);
                    submitAsk(entry.mode);
                });
                box.appendChild(row);
            });
        }

        // The panel button reads its own state — a filled marker means this
        // node is already in the trail, and clicking it again takes it out.
        function syncKeepButton() {
            const btn = document.getElementById('keep-node-btn');
            if (!btn) return;
            const n = state.selectedNode;
            const on = !!n && isKept('node:' + n.id);
            btn.classList.toggle('active', on);
            btn.title = on ? 'Stop keeping this node' : 'Keep this node in the trail';
        }

        function wireTrail() {
            const keep = document.getElementById('keep-node-btn');
            if (keep) keep.addEventListener('click', keepSelectedNode);

            const clear = document.getElementById('ask-kept-clear');
            if (clear) clear.addEventListener('click', () => {
                const trail = loadTrail();
                trail.kept = [];
                saveTrail(trail);
                renderKept();
                syncKeepButton();
            });
        }
