        // ─── Node context menu ──────────────────────────────
        //
        // Right-click a node for the short version of what the info panel
        // says. Selecting a node is a commitment: it re-anchors focus, moves
        // the camera, opens a panel over a third of the canvas and pushes a
        // history entry. But much of the time the question is only "what is
        // this one?", asked of three nodes in a row while reading a cluster —
        // and answering that should cost nothing that has to be undone.
        //
        // The node comes from `state._hoverNode` rather than from a library
        // right-click callback. The 3D backend has `onNodeRightClick` and
        // cosmos.gl has nothing like it, and one path that works on both
        // beats a more direct one that works on half the app. The pointer has
        // to be over the node for the menu to be about that node anyway, so
        // the hover the canvas already tracked is the same answer either
        // library would give — and it inherits hover's own suppressions, so
        // no card appears mid-walk or mid-tour, where the canvas is not the
        // user's to poke at.

        function wireNodeMenu() {
            const canvas = document.getElementById('graph-3d');
            const menu = document.getElementById('node-menu');
            if (!canvas || !menu) return;

            // Right-drag orbits the 3D camera and pans the 2D one, and a drag
            // ends with a `contextmenu` event exactly like a press does. Only
            // a press that stayed put is a request for the menu.
            let downX = 0, downY = 0;
            canvas.addEventListener('mousedown', e => {
                if (e.button !== 2) return;
                downX = e.clientX;
                downY = e.clientY;
            });

            canvas.addEventListener('contextmenu', e => {
                // The browser's own menu over a WebGL canvas offers Back,
                // Reload and Save Image As — nothing about the graph.
                e.preventDefault();
                if (Math.abs(e.clientX - downX) + Math.abs(e.clientY - downY) > 4) return;
                const node = state._hoverNode;
                if (!node) { hideNodeMenu(); return; }
                openNodeMenu(node, e.clientX, e.clientY);
            });

            // Anything else the user does dismisses it. A press outside is the
            // obvious one; a wheel is the necessary one, because zooming moves
            // the node out from under a card that is pinned to the viewport.
            document.addEventListener('mousedown', e => {
                if (!menu.hidden && !menu.contains(e.target)) hideNodeMenu();
            }, true);
            window.addEventListener('wheel', () => { if (!menu.hidden) hideNodeMenu(); }, { passive: true });

            // Capture phase, and it has to be: the global Escape handler in
            // 17-info-drag.js is bound on `document` and runs first otherwise,
            // and it clears the selection — so closing a card would also throw
            // away the node the user was reading about.
            document.addEventListener('keydown', e => {
                if (e.key !== 'Escape' || menu.hidden) return;
                e.stopPropagation();
                hideNodeMenu();
            }, true);
        }

        function openNodeMenu(node, x, y) {
            state._menuNode = node;
            state._menuAt = { x, y };
            renderNodeMenu(node);

            // The hover tooltip says a subset of what the card says, in the
            // same corner of the screen. Two boxes overlapping is worse than
            // either. (Moving onto the card takes the pointer off the canvas,
            // which suppresses the tooltip from then on — this only has to
            // deal with the frame where the pointer has not moved yet.)
            const tooltip = document.getElementById('tooltip');
            if (tooltip) tooltip.classList.remove('visible');

            // Server mode hands out a slim index: no docstring, metrics or
            // boundaries until a node is hydrated, and no edges until they are
            // asked for. Both fills re-render the card in place if it is still
            // showing the same node — the same late-but-right shape the info
            // panel uses, rather than a card that quietly reports "no metrics"
            // for a node that has them.
            if (state.graphMode === 'server' && node._slim !== false) {
                hydrateNodes([node.id])
                    .then(changed => { if (changed) refreshNodeMenu(node.id); })
                    .catch(err => console.error('node menu hydrate failed:', err));
            }
            if (!edgesKnownComplete(node.id)) {
                ensureEdges([node.id])
                    .then(() => refreshNodeMenu(node.id))
                    .catch(err => console.error('node menu edges failed:', err));
            }
        }

        function hideNodeMenu() {
            const menu = document.getElementById('node-menu');
            if (menu) menu.hidden = true;
            state._menuNode = null;
        }

        // Re-render only if the card is still open on that node — an async fill
        // that lands after the user has closed it, or moved on, has nothing to
        // say.
        function refreshNodeMenu(id) {
            const menu = document.getElementById('node-menu');
            if (!menu || menu.hidden) return;
            if (!state._menuNode || state._menuNode.id !== id) return;
            renderNodeMenu(state._menuNode);
        }

        // In and out degree, plus the edge types they run over. Read off the
        // adjacency index rather than the view's edges: the card is about the
        // node in the graph, not about how much of it happens to be drawn.
        function nodeMenuDegree(id) {
            let out = 0, inc = 0;
            const rels = new Map();
            knownEdgesOf(id).forEach(e => {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (sId === id) out++; else inc++;
                const rel = e.rel || 'other';
                rels.set(rel, (rels.get(rel) || 0) + 1);
            });
            return { out, inc, rels };
        }

        function renderNodeMenu(node) {
            const menu = document.getElementById('node-menu');
            if (!menu) return;
            const color = config.getColor(node.group);
            const row = (key, val) => `<div class="nm-row"><span class="nm-key">${key}</span>`
                + `<span class="nm-val">${val}</span></div>`;

            const rows = [];

            if (node.file) {
                // PDF and Office nodes carry a page number in `startLine` —
                // the info panel makes the same distinction, and labelling a
                // page as a line is a small straight lie.
                const isPaged = /\.(pdf|docx?|xlsx?|pptx?|odt|ods|odp|rtf)$/i.test(node.file);
                let where = escapeHtml(node.file);
                if (node.startLine) {
                    const span = node.endLine && node.endLine !== node.startLine
                        ? `${node.startLine}–${node.endLine}` : `${node.startLine}`;
                    where += `<span class="nm-dim">${isPaged ? ' · p.' : ':'}${span}</span>`;
                }
                rows.push(row('Where', where));
            }

            if (edgesKnownComplete(node.id)) {
                const { out, inc, rels } = nodeMenuDegree(node.id);
                if (out || inc) {
                    // Same two colours the hover highlight paints the strands
                    // in, so the card is a key for what is lit on the canvas
                    // behind it.
                    const parts = [];
                    if (out) parts.push(`<span style="color:${CANVAS.linkOut}">→ ${out} out</span>`);
                    if (inc) parts.push(`<span style="color:${CANVAS.linkIn}">← ${inc} in</span>`);
                    rows.push(row('Links', parts.join('<span class="nm-dim"> · </span>')));
                    // The two commonest edge types: "12 links" says how
                    // connected, this says connected *how*.
                    const top = [...rels.entries()].sort((a, b) => b[1] - a[1]).slice(0, 2)
                        .map(([rel, n]) => `<span style="color:${config.getRelColor(rel)}">${escapeHtml(rel)}</span>`
                            + `<span class="nm-dim"> ${n}</span>`);
                    if (top.length) rows.push(row('Via', top.join('<span class="nm-dim"> · </span>')));
                } else {
                    rows.push(row('Links', '<span class="nm-dim">none — isolated</span>'));
                }
            } else {
                rows.push(row('Links', '<span class="nm-dim">loading…</span>'));
            }

            if (node.metrics) {
                const m = node.metrics;
                const nest = m.maxNesting ?? m.max_nesting ?? '–';
                rows.push(row('Size', `LOC ${m.loc ?? '–'}<span class="nm-dim"> · </span>`
                    + `Params ${m.params ?? '–'}<span class="nm-dim"> · </span>Nest ${nest}`));
            }

            if (node.boundaries && node.boundaries.length) {
                const tags = node.boundaries.slice(0, 3).map(b => {
                    const dir = b.direction === 'Inbound' ? 'in' : 'out';
                    return `<span class="nm-boundary ${dir}">${escapeHtml(b.kind)}</span>`;
                }).join(' ');
                rows.push(row('Edge of', tags));
            }

            // One line of prose, when the node has any. Enough to tell a
            // documented node from an undocumented one and roughly what it is
            // about; the panel is where the whole thing lives.
            const doc = node.docstring
                ? `<div class="nm-doc">${escapeHtml(String(node.docstring).replace(/\s+/g, ' ').trim().slice(0, 180))}</div>`
                : '';

            menu.innerHTML = `
                <div class="nm-head">
                    ${nodeIconSvg(node.group)}
                    <span class="nm-name" title="${escapeHtml(node.name)}">${escapeHtml(truncateName(node.name))}</span>
                    <span class="nm-type" style="background:${color}20;color:${color}">${escapeHtml(node.group || '')}</span>
                </div>
                ${doc}
                <div class="nm-rows">${rows.join('')}</div>
                <div class="nm-acts">
                    <button class="nm-act" data-act="details" title="Open the full node panel">Details</button>
                    <button class="nm-act" data-act="zoom" title="Fly the camera to this node">Zoom to</button>
                    <button class="nm-act" data-act="copy" title="Copy the node id — command fuel for ug get_code">Copy id</button>
                </div>`;

            menu.querySelector('[data-act="details"]').addEventListener('click', () => {
                hideNodeMenu();
                handleClick(null, node);
            });
            menu.querySelector('[data-act="zoom"]').addEventListener('click', () => {
                hideNodeMenu();
                focusNode(node);
            });
            const copyBtn = menu.querySelector('[data-act="copy"]');
            copyBtn.addEventListener('click', async () => {
                try {
                    await navigator.clipboard.writeText(node.id);
                    copyBtn.classList.add('copied');
                    copyBtn.textContent = 'Copied';
                } catch { /* clipboard blocked — nothing useful to fall back to here */ }
                setTimeout(() => {
                    copyBtn.classList.remove('copied');
                    copyBtn.textContent = 'Copy id';
                }, 1200);
            });

            positionNodeMenu();
        }

        // Anchored at the pointer, nudged back inside the viewport. Re-run on
        // every render because an async fill changes the card's height, and a
        // card that grew downward off the bottom of the screen is the one the
        // user cannot read.
        function positionNodeMenu() {
            const menu = document.getElementById('node-menu');
            const at = state._menuAt;
            if (!menu || !at) return;
            menu.hidden = false;
            const pad = 10;
            const w = menu.offsetWidth;
            const h = menu.offsetHeight;
            const left = Math.min(at.x + 12, window.innerWidth - w - pad);
            const top = Math.min(at.y + 12, window.innerHeight - h - pad);
            menu.style.left = Math.max(pad, left) + 'px';
            menu.style.top = Math.max(pad, top) + 'px';
        }
