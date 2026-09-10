        // ─── Ask: one query, one stream ─────────────────────
        //
        // Every question this page can answer enters through `#ask-input`.
        // What used to be seven inputs — a keyword box, a semantic box, a
        // chat box, a tour box, the GQL box, the catalog filter and the
        // palette — is one bar whose *mode* is inferred from what you typed
        // and overridable from the strip under it.
        //
        // The four modes are the four retrievers the server already has:
        //
        //   names   `searchNodes`  — names, ids and docstrings. No DB.
        //   find    `fetchHybrid`  — dense + sparse fused, then a graph walk.
        //   answer  `runChatTurn`  — the same retrieval, written up, cited.
        //   tour    `startTour`    — the same retrieval, flown and narrated.
        //
        // Results from all of them land in the same one place — the block
        // for the question being asked now — and every block renders through
        // `renderHitRows`, so "why is this here" is answered the same way
        // whatever produced the row.

        const ASK_MODES = ['names', 'find', 'answer', 'tour'];

        // Above this length a query with no spaces is prose someone forgot to
        // space, not an identifier.
        const ASK_IDENT_MAX = 80;

        // What a raw input string means before the user overrides it. Pure —
        // `tests/js/ask_dispatch.mjs` runs this one directly.
        //
        // A prefix is an explicit instruction and wins. Otherwise: something
        // shaped like an identifier or a path is a name lookup, and anything
        // with a space in it is a question.
        function classifyAsk(raw) {
            const text = (raw || '').trim();
            if (!text) return { mode: null, query: '' };

            const prefix = text[0];
            if (prefix === '#') return { mode: 'names', query: text.slice(1).trim() };
            if (prefix === '?') return { mode: 'insights', query: text.slice(1).trim() };
            if (prefix === '>') return { mode: 'action', query: text.slice(1).trim() };

            const identish = text.length <= ASK_IDENT_MAX
                && /^[A-Za-z0-9_$.:#/\\@~+-]+$/.test(text);
            return { mode: identish ? 'names' : 'find', query: text };
        }

        // ── Wiring ──────────────────────────────────────────

        function askEl(id) { return document.getElementById(id); }

        function wireAsk() {
            const input = askEl('ask-input');
            const clear = askEl('ask-clear');
            const run = askEl('ask-run');

            state.askOverride = null;    // set by a click, cleared by an edit
            state.askCursor = -1;        // highlighted row in the live block
            state.askLive = null;        // the transient names block, if any

            input.addEventListener('input', () => {
                // An edit invalidates a mode the user picked for the previous
                // wording — the strip goes back to what the text implies.
                state.askOverride = null;
                clear.hidden = !input.value;
                autoGrowAsk(input);
                syncAskModes();
                askPreviewDebounced();
            });
            input.addEventListener('keydown', handleAskKey);
            input.addEventListener('focus', syncAskModes);

            clear.addEventListener('click', () => {
                input.value = '';
                clear.hidden = true;
                state.askOverride = null;
                askPreviewDebounced.cancel();
                dropAskLive();
                autoGrowAsk(input);
                syncAskModes();
                input.focus();
            });

            run.addEventListener('click', () => submitAsk());

            askEl('ask-modes').addEventListener('click', (e) => {
                const btn = e.target.closest('.ask-mode');
                if (!btn || btn.disabled) return;
                state.askOverride = btn.dataset.mode;
                syncAskModes();
                submitAsk();
            });

            const adv = askEl('ask-adv');
            const advToggle = askEl('ask-adv-toggle');
            advToggle.addEventListener('click', () => {
                const open = adv.hidden;
                adv.hidden = !open;
                advToggle.setAttribute('aria-expanded', String(open));
                advToggle.classList.toggle('open', open);
                // The retrieval disclosure is fetched once, on first reveal.
                if (open) loadChatSetup();
            });

            askEl('ask-reset').addEventListener('click', resetAsk);

            const plotAll = askEl('ask-plot-all');
            plotAll.addEventListener('click', () => lightUpNodes(state.askMatches || []));

            syncAskModes();
        }

        // A textarea so a long question wraps instead of scrolling sideways,
        // but it opens as one line and only grows to four.
        function autoGrowAsk(input) {
            input.style.height = 'auto';
            const max = 4 * 20 + 16;
            input.style.height = Math.min(input.scrollHeight, max) + 'px';
        }

        function handleAskKey(e) {
            if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                const rows = askLiveRows();
                if (!rows.length) return;
                e.preventDefault();
                moveAskCursor(e.key === 'ArrowDown' ? 1 : -1);
                return;
            }
            if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                // ⌘/Ctrl always means "write me an answer", whatever the bar
                // would otherwise have done with this text.
                if (e.metaKey || e.ctrlKey) { submitAsk('answer'); return; }
                const rows = askLiveRows();
                if (state.askCursor >= 0 && rows[state.askCursor]) {
                    rows[state.askCursor].click();
                    return;
                }
                submitAsk();
                return;
            }
            if (e.key === 'Escape') {
                const input = askEl('ask-input');
                if (!input.value) { input.blur(); return; }
                askEl('ask-clear').click();
            }
        }

        // ── The mode strip ──────────────────────────────────

        // What running Enter right now would do.
        function currentAskMode() {
            const { mode } = classifyAsk(askEl('ask-input').value);
            if (!mode) return null;
            if (state.askOverride && askModeReady(state.askOverride).ok) return state.askOverride;
            // A mode the server can't serve falls back to the one that always
            // works rather than failing at submit time.
            if (ASK_MODES.includes(mode) && !askModeReady(mode).ok) return 'names';
            return mode;
        }

        // Whether a mode can run, and why not. `names` reads the loaded graph
        // (or the server's node index) and is never unavailable.
        function askModeReady(mode) {
            const caps = state.capabilities || {};
            if (mode === 'names' || mode === 'insights' || mode === 'action') return { ok: true, why: '' };
            if (mode === 'answer') {
                if (!caps.chat_ready) {
                    return {
                        ok: false,
                        why: caps.search_ready
                            ? 'Answers need a language model — set one up in Settings.'
                            : 'Answers need embeddings and a language model.',
                    };
                }
                return { ok: true, why: '' };
            }
            // find and tour
            if (!caps.search_ready) {
                return { ok: false, why: caps.reason || 'Needs DB-backed retrieval — run ingest.' };
            }
            return { ok: true, why: '' };
        }

        // Reflect availability and the active mode onto the strip. Called on
        // every keystroke, so it touches four buttons and nothing else.
        function syncAskModes() {
            const strip = askEl('ask-modes');
            const has = !!askEl('ask-input').value.trim();
            strip.hidden = !has;
            const active = has ? currentAskMode() : null;
            strip.querySelectorAll('.ask-mode').forEach(btn => {
                const { ok, why } = askModeReady(btn.dataset.mode);
                btn.disabled = !ok;
                if (!ok) btn.title = why;
                btn.classList.toggle('active', btn.dataset.mode === active);
            });
        }

        // Whether a capability banner is up. An error with a home of its own
        // — a banner here, or the failed block in the stream — must not also
        // be written to the strip.
        function askCapsShown() {
            const caps = askEl('ask-caps');
            if (!caps) return false;
            return [...caps.querySelectorAll('.cap-banner')].some(el => !el.hidden);
        }

        function setAskStatus(text, isError) {
            const el = askEl('ask-status');
            el.textContent = text || '';
            el.classList.toggle('error', !!isError);
        }

        // ── Submitting ──────────────────────────────────────

        function submitAsk(forced) {
            const input = askEl('ask-input');
            const { mode, query } = classifyAsk(input.value);
            if (!mode || !query) return;
            const chosen = forced || (state.askOverride && askModeReady(state.askOverride).ok
                ? state.askOverride
                : mode);
            runAsk(chosen, query);
        }

        async function runAsk(mode, query) {
            if (!query) return;

            // The two prefixes that aren't retrieval at all: one is a preset
            // browser, the other is the palette's action list. Both already
            // have a home — send the query there rather than growing a third.
            if (mode === 'insights') {
                showPanel('browse');
                showSub('insights');
                const filter = askEl('ins-filter');
                if (filter) { filter.value = query; filter.dispatchEvent(new Event('input')); }
                return;
            }
            if (mode === 'action') {
                openPalette();
                const pi = askEl('palette-input');
                if (pi) { pi.value = '>' + query; renderPalette(pi.value); }
                return;
            }

            // An answer in flight owns the block it is writing into. Asking
            // again would clear that block out from under the stream, so the
            // next question waits rather than half-replacing this one.
            if (state.chatInFlight) {
                setAskStatus('Still answering — wait for that to finish.', true);
                return;
            }

            const ready = askModeReady(mode);
            if (!ready.ok) {
                // The reason is already on screen, under the mode it is about:
                // `probeCapabilities` raises a banner for every state that
                // turns a mode off, and that one carries the fix as a button.
                // Repeating it on the strip says the same thing twice.
                setAskStatus(askCapsShown() ? '' : ready.why, true);
                return;
            }

            hideAskOnboard();
            switch (mode) {
                case 'names':
                    // Recorded after the commit, so the count in the trail is
                    // the one the reader saw and not the previous keystroke's.
                    await commitAskNames(query);
                    recordAsk('names', query, { count: (state.askMatches || []).length });
                    return;
                case 'find': return runAskFind(query);
                case 'answer': return runChatTurn(query, askBlock('answer', query));
                case 'tour': {
                    const stops = clampInt(askEl('tour-stops').value, 2, 40, 8);
                    // A tour plays in the overlay and writes no block, so it
                    // clears the stream itself — otherwise the last answer
                    // sits under it looking like the tour's own result.
                    clearAskStream();
                    setAskStatus('');
                    recordAsk('tour', query, {});
                    return startTour(query, stops);
                }
            }
        }

        // ── Names ───────────────────────────────────────────
        //
        // Names is the one mode that answers while you type: the block at the
        // head of the stream is rebuilt on each pause and replaced wholesale,
        // so there is never a second results surface to reconcile. Committing
        // it (Enter, or the run button) just stops it being transient.

        const askPreviewDebounced = debounceTrailing(askPreview, SEARCH_DEBOUNCE_MS);
        let askPreviewToken = 0;

        async function askPreview() {
            // Typing while an answer streams must not take its block away —
            // the preview resumes on the next keystroke after it lands.
            if (state.chatInFlight) return;
            const { mode, query } = classifyAsk(askEl('ask-input').value);
            const effective = state.askOverride || mode;
            if (effective !== 'names' || !query) { dropAskLive(); return; }

            state.searchQuery = query;
            const token = ++askPreviewToken;
            const filters = state.nodeFilters && state.nodeFilters.size ? state.nodeFilters : null;
            // One request covers both the rows shown and the "light up every
            // match" set drawn from the rest.
            const found = await searchNodes(query, {
                limit: Math.max(50, SOLO_MAX_NODES),
                types: filters,
            });
            // A slower earlier keystroke must not repaint over a later one.
            if (token !== askPreviewToken) return;

            const block = ensureAskLive(query);
            const shown = found.nodes.slice(0, 50);
            block.meta(`${shown.length} of ${formatNumber(found.total)}`
                + (filters ? ` · ${filters.size} type filter(s)` : ''));
            block.body.innerHTML = '';
            if (!shown.length) {
                block.body.innerHTML = '<div class="ask-empty">No node name, id or docstring contains that.</div>';
                setAskMatches([]);
                return;
            }
            renderHitRows(block.body, shown);
            setAskMatches(found.nodes);
            state.askCursor = -1;
            writeUrlState();
        }

        function commitAskNames(query) {
            askPreviewDebounced.flush();
            const live = state.askLive;
            if (live) {
                live.classList.remove('live');
                state.askLive = null;
                state.askCursor = -1;
                return;
            }
            // Nothing was previewed (the mode was forced) — run it once.
            state.askOverride = 'names';
            return askPreview();
        }

        function ensureAskLive(query) {
            if (state.askLive) {
                const el = state.askLive;
                el.querySelector('.ask-block-q').textContent = query;
                return askBlockHandle(el);
            }
            const handle = askBlock('names', query);
            handle.el.classList.add('live');
            state.askLive = handle.el;
            return handle;
        }

        function dropAskLive() {
            state.searchQuery = '';
            if (!state.askLive) return;
            state.askLive.remove();
            state.askLive = null;
            state.askCursor = -1;
            setAskMatches([]);
        }

        function askLiveRows() {
            if (!state.askLive) return [];
            return [...state.askLive.querySelectorAll('.ask-row')];
        }

        // Down from nothing lands on the first row, not the last, and both
        // directions wrap. `askCursor` is -1 when nothing is highlighted.
        function moveAskCursor(dir) {
            const rows = askLiveRows();
            if (!rows.length) return;
            let next = state.askCursor + dir;
            if (next < 0) next = rows.length - 1;
            if (next >= rows.length) next = 0;
            state.askCursor = next;
            rows.forEach((r, i) => r.classList.toggle('active', i === state.askCursor));
            const el = rows[state.askCursor];
            if (el) el.scrollIntoView({ block: 'nearest' });
        }

        // ── Find ────────────────────────────────────────────

        async function runAskFind(query) {
            const k = clampInt(askEl('sem-k').value, 1, 50, 10);
            const hops = clampInt(askEl('chat-hops').value, 0, 4, 2);
            const block = askBlock('find', query);
            block.wait('Retrieving…');

            const t0 = performance.now();
            try {
                const hits = await fetchHybrid(query, k, hops);
                const ms = Math.round(performance.now() - t0);
                const destNote = hits.dest ? ` · from ${hits.dest}` : '';
                block.meta(`${hits.length} result${hits.length === 1 ? '' : 's'} · ${ms} ms${destNote}`);
                block.body.innerHTML = '';
                if (!hits.length) {
                    block.body.innerHTML = '<div class="ask-empty">Nothing retrieved. Try fewer words, '
                        + 'or a name — Names searches the index directly.</div>';
                    setAskMatches([]);
                    recordAsk('find', query, { count: 0 });
                    return;
                }
                renderHitRows(block.body, hits);
                setAskMatches(hits);
                recordAsk('find', query, { count: hits.length });
            } catch (err) {
                block.fail(`Retrieval failed — ${err.message || err}`);
                console.error(err);
            }
        }

        // ── The stream ──────────────────────────────────────

        function askBlockHandle(el) {
            const head = el.querySelector('.ask-block-head');
            const body = el.querySelector('.ask-block-body');
            return {
                el, body,
                meta(text) { head.querySelector('.ask-block-meta').textContent = text || ''; },
                wait(text) { body.innerHTML = ''; body.append(askWaitEl(text)); },
                fail(msg) {
                    el.classList.add('error');
                    body.innerHTML = '';
                    body.textContent = msg;
                },
            };
        }

        function askWaitEl(text) {
            const d = document.createElement('div');
            d.className = 'ask-wait';
            d.innerHTML = '<span class="tour-spinner"></span>';
            d.append(document.createTextNode(text));
            return d;
        }

        function askBlock(mode, query) {
            // One question, one block: opening a new one retires whatever the
            // last question left behind.
            clearAskStream();
            const stream = askEl('ask-stream');
            const el = document.createElement('div');
            el.className = `ask-block mode-${mode}`;

            const head = document.createElement('div');
            head.className = 'ask-block-head';
            const badge = document.createElement('span');
            badge.className = 'ask-block-mode';
            badge.textContent = mode;
            const q = document.createElement('span');
            q.className = 'ask-block-q';
            q.textContent = query;              // user text, never innerHTML
            q.title = query;
            const meta = document.createElement('span');
            meta.className = 'ask-block-meta';

            const keep = document.createElement('button');
            keep.type = 'button';
            keep.className = 'ask-block-keep';
            keep.title = 'Keep this in the trail';
            keep.textContent = '⌾';
            keep.addEventListener('click', () => {
                // An answer is worth keeping verbatim; anything else is worth
                // keeping as the view it produced.
                const answer = el._answer;
                if (answer) keepAnswer(query, answer.text, answer.cites);
                else keepCurrentView(query);
                keep.classList.add('kept');
                keep.title = 'Kept';
            });

            head.append(badge, q, meta, keep);

            const body = document.createElement('div');
            body.className = 'ask-block-body';

            el.append(head, body);
            stream.appendChild(el);
            el.scrollIntoView({ block: 'nearest' });
            return askBlockHandle(el);
        }

        // The stream holds the current question and nothing else. Each of
        // Names, Find, Answer and Tour used to append, so walking the strip
        // over one question left four stacked results and a column that only
        // ever got longer. Old blocks are *removed*, not hidden: a collapsed
        // section still in the DOM still costs layout and paint on every
        // reflow (AGENTS.md §9d).
        function clearAskStream() {
            askEl('ask-stream').innerHTML = '';
            state.askLive = null;
            state.askCursor = -1;
            setAskMatches([]);
        }

        function resetAsk() {
            state.chatHistory = [];
            // `searchQuery` is the slice of this column the URL carries; a
            // cleared stream must not leave `?q=` behind pointing at rows
            // that are gone.
            state.searchQuery = '';
            clearAskStream();
            setAskStatus('');
            showAskOnboard();
            writeUrlState();
        }

        function setAskMatches(hits) {
            const ids = (hits || [])
                .map(h => h.id)
                .filter(id => state.nodeById && state.nodeById.has(id));
            state.askMatches = ids;
            syncPlotAllButton(askEl('ask-plot-all'), ids);
        }

        // ── One row renderer for every retriever ────────────
        //
        // A row says what was found, where it lives, and — this is the part
        // that used to be buried in one pane nobody opened — *how* it was
        // reached. `matched_by`, `hop` and the score all travel on every
        // hybrid item and on every chat citation; showing them is the
        // difference between a list and an argument.

        const ASK_MATCH_TIP = {
            semantic: 'Dense vector match — the embedding of this node is close to your question.',
            keyword: 'Sparse/keyword match — the words themselves line up.',
            graph: 'Reached by walking the graph out from a seed that matched.',
        };

        function renderHitRows(container, hits, opts = {}) {
            const frag = document.createDocumentFragment();
            hits.forEach(h => {
                frag.appendChild(buildHitRow(h, opts));
            });
            container.appendChild(frag);
        }

        function buildHitRow(h, opts = {}) {
            const type = h.node_type || h.group || 'Default';
            const row = document.createElement('div');
            row.className = 'ask-row';

            const lineLabel = h.start_line
                ? `L${h.start_line}${h.end_line && h.end_line !== h.start_line ? '–' + h.end_line : ''}`
                : '';
            const meta = [type, h.file, lineLabel].filter(Boolean).join(' · ');

            row.innerHTML = `
                <div class="ask-row-head">
                    ${nodeIconSvg(type)}
                    <span class="ask-row-name">${escapeHtml(truncateName(h.name || h.id))}</span>
                    ${askProvenanceHtml(h)}
                </div>
                ${meta ? `<div class="ask-row-meta">${escapeHtml(meta)}</div>` : ''}
            `;
            row.querySelector('.ask-row-name').title = h.id || h.name || '';
            if (opts.index != null) row.dataset.cite = String(opts.index);

            row.addEventListener('click', (ev) => {
                const local = state.nodeById ? state.nodeById.get(h.id) : null;
                if (!local) {
                    // The store has it and the loaded graph doesn't. Say so
                    // rather than swallowing the click.
                    setAskStatus(`"${h.id}" isn't in the loaded graph.`, true);
                    return;
                }
                // ⌘/Ctrl adds to the canvas instead of replacing it.
                if (ev.metaKey || ev.ctrlKey) state._viewMerge = true;
                if (state.pathMode) exitPathMode();
                handleClick(null, local);
                focusNode(local);
            });
            return row;
        }

        // The provenance strip: how it was reached, how far, how close.
        function askProvenanceHtml(h) {
            const bits = [];
            const mech = h.matched_by || '';
            if (mech) {
                bits.push(`<span class="ask-match ask-match-${escapeHtml(mech)}"`
                    + ` title="${escapeHtml(ASK_MATCH_TIP[mech] || mech)}">${escapeHtml(mech)}</span>`);
            }
            if (h.hop != null && h.hop > 0) {
                bits.push(`<span class="ask-hop" title="Hops from the node that actually matched">`
                    + `${h.hop} hop${h.hop === 1 ? '' : 's'}</span>`);
            }
            const score = h.score != null ? h.score : h.distance;
            if (score != null && Number.isFinite(score)) {
                bits.push(`<span class="ask-score" title="Retrieval distance — smaller is closer">`
                    + `${score.toFixed(3)}</span>`);
            }
            return bits.join('');
        }

        // ── Onboarding slot ─────────────────────────────────
        // Filled in stage 4; for now it just gets out of the way once the
        // stream has something in it.

        function hideAskOnboard() {
            const el = askEl('ask-onboard');
            if (el) el.hidden = true;
        }

        function showAskOnboard() {
            const el = askEl('ask-onboard');
            if (el) el.hidden = false;
        }

        // A deep link's `?q=` is a name query. Put it back in the bar and
        // re-run the preview without stealing focus — the link may well have
        // landed the reader somewhere else on purpose.
        function restoreAskQuery(q) {
            const input = askEl('ask-input');
            if (!input) return;
            input.value = q;
            askEl('ask-clear').hidden = !q;
            autoGrowAsk(input);
            state.askOverride = 'names';
            syncAskModes();
            askPreview();
        }

        // Focus the bar, optionally seeded with a query. The palette, the
        // node menu and the keyboard map all route through this rather than
        // reaching for the element.
        function focusAsk(query, mode) {
            showPanel('ask');
            const input = askEl('ask-input');
            if (query != null) {
                input.value = query;
                askEl('ask-clear').hidden = !query;
                autoGrowAsk(input);
            }
            if (mode) state.askOverride = mode;
            syncAskModes();
            input.focus();
            input.setSelectionRange(input.value.length, input.value.length);
        }

        // ─── The opening ────────────────────────────────────
        //
        // An empty bar is the most common state this column is in, and the
        // question it has to answer is "what is this, and what should I ask
        // it?". Statistics do not answer that. So: what the base is, in one
        // line; what shape it has, from `project_overview` — a live endpoint
        // the page has never called; and a handful of questions built from
        // real names in this graph.
        //
        // Every suggestion is templated from data already retrieved. Opening
        // a knowledge base must not cost a model call.

        const ASK_SUGGEST_MAX = 5;

        // `probeCapabilities` runs again after an ingest, and the overview is
        // a whole-graph pass on the server. Fetch it once per project.
        let askOverviewFor = null;
        let askOverview = null;

        async function renderAskOrient() {
            const box = askEl('ask-orient');
            if (!box) return;

            const caps = state.capabilities || {};
            const proj = (caps.project && caps.project.name) || state.activeProject || '';
            const nodes = state.nodeCount || (state.graph && state.graph.nodes.length) || 0;

            box.innerHTML = '';
            const line = document.createElement('div');
            line.className = 'ask-orient-line';
            const title = document.createElement('span');
            title.className = 'ask-orient-name';
            title.textContent = proj || 'This knowledge base';
            const sub = document.createElement('span');
            sub.className = 'ask-orient-sub';
            sub.textContent = nodes ? `${formatNumber(nodes)} nodes` : '';
            line.append(title, sub);
            box.appendChild(line);

            let overview = askOverviewFor === proj ? askOverview : null;
            if (askOverviewFor !== proj) {
                askOverviewFor = proj;
                askOverview = null;
                try {
                    const res = await fetch('/api/tools/project_overview', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({}),
                    });
                    if (res.ok) askOverview = await res.json();
                } catch (err) {
                    // Orientation is a convenience. A server that cannot
                    // answer leaves the identity line standing, not an error.
                    console.warn('project overview unavailable:', err && err.message);
                }
                overview = askOverview;
            }
            if (overview) {
                const bits = [];
                const kind = { docs: 'Documents', code: 'Code', mixed: 'Code and documents' }[overview.kb_type];
                if (kind) bits.push(kind);
                if (overview.index && overview.index.files) {
                    bits.push(`${formatNumber(overview.index.files)} files`);
                }
                if (overview.languages && overview.languages.length) {
                    bits.push(overview.languages.slice(0, 3).map(l => l.name).join(', '));
                }
                if (bits.length) sub.textContent = bits.join(' · ');
            }
            renderAskSuggestions(overview);
        }

        // Questions worth asking, in this base's own vocabulary. Retrieval
        // answers questions; without it the best a starter can do is put a
        // real name in the bar, so the two cases get different chips rather
        // than one set that half works.
        function renderAskSuggestions(overview) {
            const box = askEl('ask-suggest');
            if (!box) return;
            box.innerHTML = '';

            const ready = askModeReady('find').ok;
            // `project_overview` is store-backed, so a published snapshot or a
            // `--no-db` serve gets nothing from it. The graph itself still
            // knows which nodes are busiest, and that is enough for a starter.
            const hubs = (overview && overview.hotspots && overview.hotspots.length
                ? overview.hotspots
                : topByDegree(5)).map(h => h.name).filter(Boolean);
            const files = (overview && overview.biggest_files ? overview.biggest_files : [])
                .map(f => f.name).filter(Boolean);
            const docs = overview && overview.kb_type === 'docs';

            let chips;
            if (!ready) {
                // Names is all there is until something is ingested. Offer the
                // busiest names rather than questions nothing can answer.
                chips = hubs.slice(0, ASK_SUGGEST_MAX).map(n => ({ text: n, mode: 'names' }));
            } else {
                chips = [];
                if (hubs[0]) chips.push({ text: `How does ${hubs[0]} work?` });
                if (hubs[1]) chips.push({ text: `What calls ${hubs[1]}?` });
                chips.push({ text: docs ? 'What does this collection cover?' : 'What are the entry points?' });
                if (files[0]) chips.push({ text: `Walk me through ${files[0]}` });
                if (!docs) chips.push({ text: 'What talks to the outside world?' });
                chips = chips.slice(0, ASK_SUGGEST_MAX);
            }
            if (!chips.length) return;

            const head = document.createElement('div');
            head.className = 'ask-onboard-head';
            head.textContent = ready ? 'Worth asking' : 'Worth looking at';
            box.appendChild(head);

            chips.forEach(c => {
                const chip = document.createElement('button');
                chip.type = 'button';
                chip.className = 'ask-chip';
                chip.textContent = c.text;
                chip.addEventListener('click', () => {
                    focusAsk(c.text, c.mode || null);
                    submitAsk(c.mode || undefined);
                });
                box.appendChild(chip);
            });
        }
