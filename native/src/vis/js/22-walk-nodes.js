        // ─── Graph Walk: the reach, as a list ───────────────
        //
        // The cascade answers "what shape is this reach" — three hundred discs
        // in hop columns, which is the right answer to how far and how wide,
        // and no answer at all to *which* nodes. Past a couple of dozen the
        // canvas cannot be read node by node: the labels collide, the discs
        // are five pixels wide, and the only way to inspect one was to find it
        // with the pointer.
        //
        // So: the same set, as rows you can filter. It reads `walkPlay.layers`
        // — the whole plan, including hops the reveal has not reached yet,
        // because the plan is computed up front and knowing what is coming is
        // most of why someone opens this mid-walk. Rows from an unrevealed hop
        // say so and do not offer to fly to a node that is not on the canvas.

        // The row cap. A three-hop walk off a hub reaches tens of thousands of
        // nodes, and every row is DOM: at ~200 bytes of layout each, rendering
        // them all is tens of megabytes and a list that janks on every
        // keystroke. The cap is on *rendered* rows, applied after filtering,
        // so narrowing the filter always reaches the rest — and the banner
        // above the list says so whenever it bites, because a silently
        // truncated list is a wrong answer rather than a partial one.
        const WALK_NODES_MAX = 1000;

        // Filter state lives here rather than in the DOM so it survives the
        // re-render each hop triggers while the panel is open.
        const wnFilters = { text: '', hop: 'all', types: new Set() };
        let wnActiveId = null;

        function wnEl(id) { return document.getElementById(id); }

        function wireWalkNodes() {
            const btn = wnEl('walk-o-list');
            const panel = wnEl('walk-nodes');
            if (!btn || !panel) return;

            btn.addEventListener('click', toggleWalkNodes);
            wnEl('wn-close').addEventListener('click', closeWalkNodes);

            const search = wnEl('wn-search');
            search.addEventListener('input', () => {
                wnFilters.text = search.value.trim().toLowerCase();
                renderWalkNodes({ keepScroll: false });
            });
            // Esc inside the box clears the filter before it closes anything —
            // the same two-stage escape the search field in the sidebar has.
            search.addEventListener('keydown', e => {
                if (e.key !== 'Escape' || !search.value) return;
                e.stopPropagation();
                search.value = '';
                wnFilters.text = '';
                renderWalkNodes({ keepScroll: false });
            });

            // Capture phase: the walk's own Escape handler exits the walk, and
            // closing a panel must not tear down the thing it is describing.
            document.addEventListener('keydown', e => {
                if (e.key !== 'Escape' || panel.hidden) return;
                e.stopPropagation();
                closeWalkNodes();
            }, true);
        }

        function toggleWalkNodes() {
            const panel = wnEl('walk-nodes');
            if (!panel) return;
            if (panel.hidden) openWalkNodes(); else closeWalkNodes();
        }

        function openWalkNodes() {
            const panel = wnEl('walk-nodes');
            if (!panel || !walkPlay.layers) return;
            panel.hidden = false;
            const btn = wnEl('walk-o-list');
            if (btn) { btn.classList.add('active'); btn.setAttribute('aria-pressed', 'true'); }
            renderWalkNodes({ keepScroll: false });
            const search = wnEl('wn-search');
            if (search) search.focus();
        }

        function closeWalkNodes() {
            const panel = wnEl('walk-nodes');
            if (!panel) return;
            panel.hidden = true;
            const btn = wnEl('walk-o-list');
            if (btn) { btn.classList.remove('active'); btn.setAttribute('aria-pressed', 'false'); }
        }

        // A new walk means a new set of nodes, so the filters from the last
        // one are answers to a question nobody is asking any more. Called from
        // showWalkOverlay.
        function resetWalkNodes() {
            wnFilters.text = '';
            wnFilters.hop = 'all';
            wnFilters.types.clear();
            wnActiveId = null;
            const search = wnEl('wn-search');
            if (search) search.value = '';
        }

        // Called from updateWalkOverlay on every hop change: the panel lists
        // hops the walk has yet to reveal, and which of them those are is
        // exactly what a hop changes.
        function refreshWalkNodes() {
            const panel = wnEl('walk-nodes');
            if (!panel || panel.hidden) return;
            if (!walkPlay.layers) { closeWalkNodes(); return; }
            renderWalkNodes({ keepScroll: true });
        }

        // Every planned node, flattened, each tagged with the hop that reaches
        // it. Built fresh per render: it is bounded by the walk, and a walk
        // that is still revealing changes which hops count as reached.
        function wnCollect() {
            const rows = [];
            const seen = new Set();
            for (const layer of walkPlay.layers || []) {
                const revealed = state.walkReached && layer.ids.some(id => state.walkReached.has(id));
                for (const id of layer.ids) {
                    // A node can only be reached once — but the seed is also
                    // layer 0's only id, and a malformed plan should not put a
                    // node in the list twice.
                    if (seen.has(id)) continue;
                    seen.add(id);
                    const node = state.nodeById && state.nodeById.get(id);
                    if (!node) continue;
                    rows.push({
                        node,
                        hop: layer.hop,
                        // Per node, not per layer: during the streaming phase a
                        // hop's nodes are already in `walkReached` as ghosts,
                        // and those *are* on the canvas to fly to.
                        pending: !(state.walkReached && state.walkReached.has(id)) && !revealed,
                    });
                }
            }
            return rows;
        }

        function wnMatches(row) {
            if (wnFilters.hop !== 'all' && row.hop !== wnFilters.hop) return false;
            if (wnFilters.types.size && !wnFilters.types.has(row.node.group)) return false;
            if (!wnFilters.text) return true;
            const n = row.node;
            return (n.name || '').toLowerCase().includes(wnFilters.text)
                || (n.group || '').toLowerCase().includes(wnFilters.text)
                || (n.file || '').toLowerCase().includes(wnFilters.text)
                || (n.id || '').toLowerCase().includes(wnFilters.text);
        }

        function renderWalkNodes(opts) {
            const panel = wnEl('walk-nodes');
            if (!panel || panel.hidden || !walkPlay.layers) return;
            const list = wnEl('wn-list');
            const scroll = (opts && opts.keepScroll) ? list.scrollTop : 0;

            const all = wnCollect();
            const matched = all.filter(wnMatches);

            // ── Header: the whole reach, before any filter.
            const hops = walkPlay.layers.length - 1;
            wnEl('wn-sub').textContent =
                `${all.length.toLocaleString()} node${all.length === 1 ? '' : 's'} · `
                + `${hops} hop${hops === 1 ? '' : 's'}`;

            // ── Hop chips. Counts are of the unfiltered set: a chip that
            // recounted itself under the current filter could read "0" for a
            // hop that is full, which is a filter reporting on itself.
            //
            // One pass for both numbers a hop needs — how many nodes it holds,
            // and whether any of them are on the canvas yet. Counting inside
            // the chip loop instead would rescan every node per hop, which on
            // the 3,162-node walk this cap exists for is six full sweeps.
            const byHop = new Map();
            const revealedHops = new Set();
            all.forEach(r => {
                byHop.set(r.hop, (byHop.get(r.hop) || 0) + 1);
                if (!r.pending) revealedHops.add(r.hop);
            });
            const hopBox = wnEl('wn-hops');
            hopBox.innerHTML = `<button class="wn-chip${wnFilters.hop === 'all' ? ' active' : ''}" data-hop="all" type="button">All`
                + `<span class="wn-count">${all.length.toLocaleString()}</span></button>`
                + [...byHop.keys()].sort((a, b) => a - b).map(h => {
                    const pending = !revealedHops.has(h);
                    const colour = walkColorForHop(h);
                    return `<button class="wn-chip${wnFilters.hop === h ? ' active' : ''}${pending ? ' pending' : ''}"`
                        + ` data-hop="${h}" type="button"`
                        + ` title="${pending ? 'Planned — the walk has not revealed this hop yet' : ''}">`
                        + `<span class="wn-dot" style="background:${colour}"></span>`
                        + `${h === 0 ? 'Seed' : 'Hop ' + h}`
                        + `<span class="wn-count">${byHop.get(h).toLocaleString()}</span></button>`;
                }).join('');
            hopBox.querySelectorAll('.wn-chip').forEach(chip => {
                chip.addEventListener('click', () => {
                    const raw = chip.dataset.hop;
                    wnFilters.hop = raw === 'all' ? 'all' : parseInt(raw, 10);
                    renderWalkNodes({ keepScroll: false });
                });
            });

            // ── Type chips, over whatever the hop filter leaves. These narrow
            // within a hop far more often than across the whole walk.
            const inHop = all.filter(r => wnFilters.hop === 'all' || r.hop === wnFilters.hop);
            const byType = new Map();
            inHop.forEach(r => byType.set(r.node.group, (byType.get(r.node.group) || 0) + 1));
            const typeBox = wnEl('wn-types');
            typeBox.innerHTML = [...byType.entries()]
                .sort((a, b) => b[1] - a[1])
                .map(([type, n]) => {
                    const colour = config.getColor(type);
                    return `<button class="wn-chip${wnFilters.types.has(type) ? ' active' : ''}" data-type="${escapeHtml(type)}" type="button">`
                        + `<span class="wn-dot" style="background:${colour}"></span>${escapeHtml(type)}`
                        + `<span class="wn-count">${n.toLocaleString()}</span></button>`;
                }).join('');
            typeBox.querySelectorAll('.wn-chip').forEach(chip => {
                chip.addEventListener('click', () => {
                    const t = chip.dataset.type;
                    if (wnFilters.types.has(t)) wnFilters.types.delete(t); else wnFilters.types.add(t);
                    renderWalkNodes({ keepScroll: false });
                });
            });

            // ── The cap, and saying so.
            const capped = matched.length > WALK_NODES_MAX;
            const shown = capped ? matched.slice(0, WALK_NODES_MAX) : matched;
            const note = wnEl('wn-note');
            note.hidden = !capped;
            if (capped) {
                note.textContent = `Showing the first ${WALK_NODES_MAX.toLocaleString()} of `
                    + `${matched.length.toLocaleString()} matching nodes — narrow the filter to reach the rest.`;
            }

            if (!shown.length) {
                list.innerHTML = `<div class="wn-empty">${all.length
                    ? 'No nodes match these filters.'
                    : 'This walk has not reached anything yet.'}</div>`;
                return;
            }

            // ── Rows, grouped by hop. Within a hop: type first, then name, so
            // the kinds of thing a hop pulled in read as blocks rather than as
            // an alphabetical shuffle of everything at once.
            shown.sort((a, b) => a.hop - b.hop
                || String(a.node.group).localeCompare(String(b.node.group))
                || String(a.node.name).localeCompare(String(b.node.name)));

            // How much of each hop survived the filter and the cap, counted
            // once rather than re-scanned per group header.
            const shownPerHop = new Map();
            shown.forEach(r => shownPerHop.set(r.hop, (shownPerHop.get(r.hop) || 0) + 1));

            const parts = [];
            let lastHop = null;
            for (const row of shown) {
                if (row.hop !== lastHop) {
                    lastHop = row.hop;
                    const total = byHop.get(row.hop) || 0;
                    const here = shownPerHop.get(row.hop) || 0;
                    parts.push(`<div class="wn-group">`
                        + `<span class="wn-dot" style="background:${walkColorForHop(row.hop)}"></span>`
                        + `${row.hop === 0 ? 'Seed' : 'Hop ' + row.hop}`
                        + `<span class="wn-gcount">${here === total ? here.toLocaleString()
                            : here.toLocaleString() + ' of ' + total.toLocaleString()}</span></div>`);
                }
                const n = row.node;
                const colour = config.getColor(n.group);
                const where = n.file
                    ? escapeHtml(n.file + (n.startLine ? ':' + n.startLine : ''))
                    : '';
                const deg = edgesKnownComplete(n.id) ? knownEdgesOf(n.id).length : null;
                parts.push(`<button class="wn-row${row.pending ? ' pending' : ''}${wnActiveId === n.id ? ' active' : ''}"`
                    + ` data-id="${escapeHtml(n.id)}" type="button"`
                    + ` title="${escapeHtml(row.pending ? 'Planned — not on the canvas yet' : n.id)}">`
                    + nodeIconSvg(n.group)
                    + `<span class="wn-main">`
                    + `<span class="wn-name">${escapeHtml(truncateName(n.name))}</span>`
                    + (where ? `<span class="wn-where">${where}</span>` : '')
                    + `</span>`
                    + (deg === null ? '' : `<span class="wn-deg" title="Edges touching this node">${deg}</span>`)
                    + `<span class="wn-type" style="background:${colour}20;color:${colour}">${escapeHtml(n.group || '')}</span>`
                    + `</button>`);
            }
            list.innerHTML = parts.join('');

            // A row flies the camera to its node and opens the summary card on
            // it — the same card a click on the canvas gives, which is what
            // makes this a way *into* the walk rather than a report about it.
            list.querySelectorAll('.wn-row:not(.pending)').forEach(el => {
                el.addEventListener('click', ev => {
                    const node = state.nodeById.get(el.dataset.id);
                    if (!node) return;
                    wnActiveId = node.id;
                    list.querySelectorAll('.wn-row.active').forEach(r => r.classList.remove('active'));
                    el.classList.add('active');
                    focusNode(node);
                    openNodeMenuAt(node, ev);
                });
            });
            list.scrollTop = scroll;
        }
