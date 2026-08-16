        // ─── Hover & selection ──────────────────────────────

        // Whether the pointer is genuinely over the 3D canvas (and not over
        // the sidebar, navbar, or any other panel stacked above it). The
        // renderer re-raycasts its *last known* canvas coordinates every
        // frame, so while the pointer is parked over a panel, camera flights
        // and layout ticks can slide a node under that stale position and
        // fire phantom hover events.
        function pointerOverCanvas() {
            const m = state._mouse;
            if (!m) return true;
            const el = document.elementFromPoint(m.cx ?? m.x, m.cy ?? m.y);
            return !!el && !!el.closest('#graph-3d');
        }

        function handleNodeHover(d, prev) {
            // Suppress hovers raycast from a stale pointer position while the
            // user is actually working in a panel (see pointerOverCanvas).
            if (d && !pointerOverCanvas()) d = null;
            // While a tour is playing the camera sweeps nodes under a parked
            // pointer every frame; tooltips there are noise, not intent. Same
            // while a plan is still being charted — nothing on screen is the
            // user's doing yet. Pausing hands hovering back to the user.
            if (d && tourState.active && (tourState.playing || !tourState.data)) d = null;
            // A walk owns the canvas outright, and for the same reason plus
            // one more. The camera reframes on every hop, so nodes sweep under
            // a parked pointer and hovers fire that the user never asked for —
            // but worse, a hover repaints the node and its neighbours in the
            // hot highlight orange, which is the one thing that can overwrite
            // a hop colour. The whole point of the reveal is that colour says
            // *how far out* a node is; a stray hover makes three hops look
            // like one, and drags a tooltip over the diagram while doing it.
            if (d && state.walkActive) d = null;
            state._hoverNode = d || null;
            const el = document.getElementById('graph-3d');
            if (el) el.style.cursor = d ? 'pointer' : null;
            const tooltip = document.getElementById('tooltip');

            // Recompute connected-node / link highlight sets. Scoped to the
            // view: the highlight sets are compared against the edge objects
            // the renderer holds, and off-screen edges have nothing to light up.
            state.highlightNodes.clear();
            state.highlightLinks.clear();
            state.highlightLinkDir.clear();
            let outCount = 0, inCount = 0;
            if (d) {
                state.highlightNodes.add(d.id);
                state.view.edges.forEach(e => {
                    const sId = e.source.id || e.source;
                    const tId = e.target.id || e.target;
                    if (sId === d.id) {
                        state.highlightNodes.add(tId);
                        state.highlightLinks.add(e);
                        state.highlightLinkDir.set(e, 'out');
                        outCount++;
                    } else if (tId === d.id) {
                        state.highlightNodes.add(sId);
                        state.highlightLinks.add(e);
                        state.highlightLinkDir.set(e, 'in');
                        inCount++;
                    }
                });
            }
            bumpGraphStyles();

            if (!d) {
                tooltip.classList.remove('visible');
                return;
            }

            tooltip.querySelector('.tooltip-title').textContent = truncateName(d.name);
            tooltip.querySelector('.tooltip-type').textContent =
                d.group + (d.startLine ? ` @L${d.startLine}` : '');

            const { directChildren, siblings } = getContainsCounts(d.id);
            const statParts = [];
            if (directChildren) statParts.push(`↳ ${directChildren} child${directChildren === 1 ? '' : 'ren'}`);
            if (siblings) statParts.push(`• ${siblings} sibling${siblings === 1 ? '' : 's'}`);
            tooltip.querySelector('.tooltip-stats').textContent = statParts.join('  ·  ');

            // The key for the two link colours now lit on the canvas. Without
            // it the split is decorative — you can see there are two families
            // of strand but not which way either one points. Colours come from
            // CANVAS so the swatch and the strand can never drift apart, and
            // an empty side is dropped rather than shown as a zero.
            const dirEl = tooltip.querySelector('.tooltip-dirs');
            const dirParts = [];
            if (outCount) dirParts.push(`<span style="color:${CANVAS.linkOut}">→ ${outCount} out</span>`);
            if (inCount) dirParts.push(`<span style="color:${CANVAS.linkIn}">← ${inCount} in</span>`);
            dirEl.innerHTML = dirParts.join('');

            const m = state._mouse || { x: width / 2, y: height / 2 };
            tooltip.style.left = (m.x + 15) + 'px';
            tooltip.style.top = (m.y + 15) + 'px';
            tooltip.classList.add('visible');
        }

        function handleClick(event, d) {
            // Every way of picking a node — the canvas, search, semantic hits,
            // chat citations, the catalog, insights, the breadcrumb, Tab
            // stepping, a tour stop — lands here, so solo mode only has to
            // hook this one place to keep the canvas in step with the selection.
            if (state.soloOnly) showInView(d);

            const info = document.getElementById('info');
            const body = document.getElementById('info-body');
            const jumpBtn = document.getElementById('jump-btn');
            const typeColor = config.getColor(d.group);

            // `<span class="info-label">` with the field's explanation attached.
            const fieldLabel = (key) => {
                const [label, tip] = FIELD_DOCS[key] || [key, ''];
                return `<span class="info-label has-tip" title="${escapeHtml(tip)}">${escapeHtml(label)}</span>`;
            };

            // A short, single-value field. `valueCls` adds a modifier to the
            // value span (used by the node-id row to lay out its copy button).
            const fieldRow = (key, valueHtml, valueCls) =>
                `<div class="info-row">${fieldLabel(key)}<span class="info-value${valueCls ? ' ' + valueCls : ''}">${valueHtml}</span></div>`;

            // A field whose value may be a paragraph rather than a token.
            // Markdown sections and PDF pages carry their whole prose as a
            // docstring, so an inline value would push everything below it —
            // tabs included — off the panel. Short values stay a plain row;
            // long ones collapse to a preview.
            const LONG_FIELD_CHARS = 180;
            function longFieldRow(key, value) {
                const text = String(value);
                if (text.length <= LONG_FIELD_CHARS) return fieldRow(key, escapeHtml(text));

                // Cut on a word boundary — a mid-word truncation reads as
                // corruption rather than as a preview.
                const flat = text.replace(/\s+/g, ' ').trim();
                let preview = flat.slice(0, 100);
                const lastSpace = preview.lastIndexOf(' ');
                if (lastSpace > 50) preview = preview.slice(0, lastSpace);

                return `<div class="info-row block">${fieldLabel(key)}`
                    + `<details class="info-collapse"><summary>`
                    + `<span class="info-preview">${escapeHtml(preview)}…</span>`
                    + `<span class="info-preview-count">${text.length.toLocaleString()} chars</span>`
                    + `</summary><div class="info-full">${escapeHtml(text)}</div></details></div>`;
            }

            // A list-valued field (calls, extends, implements) as chips.
            //
            // These were comma-joined into one `info-value`, which for a
            // function calling thirty things produced an unreadable wall that
            // pushed the tabs off screen — and gave no way to act on any of
            // it. Chips wrap, count themselves, collapse past a threshold,
            // and the ones that resolve to an indexed node navigate to it.
            const CHIPS_INLINE_MAX = 8;
            function chipRow(key, names) {
                const list = names.filter(Boolean).map(String);
                if (!list.length) return '';
                const chips = list.map(n => {
                    const hit = state.nodeById ? findNodeByName(n) : null;
                    const cls = hit ? 'info-chip linked' : 'info-chip';
                    const attr = hit ? ` data-id="${escapeHtml(hit.id)}" title="Go to ${escapeHtml(hit.id)}"`
                        : ' title="Not indexed as its own node — an external or unresolved name"';
                    return `<span class="${cls}"${attr}>${escapeHtml(n)}</span>`;
                }).join('');

                if (list.length <= CHIPS_INLINE_MAX) {
                    return `<div class="info-row block">${fieldLabel(key)}`
                        + `<div class="info-chips">${chips}</div></div>`;
                }
                const [label] = FIELD_DOCS[key] || [key];
                return `<div class="info-row block">${fieldLabel(key)}`
                    + `<details class="info-collapse"><summary>`
                    + `<span class="info-preview">${escapeHtml(list.slice(0, 3).join(', '))}…</span>`
                    + `<span class="info-preview-count">${list.length} ${label === 'Calls out to' ? 'calls' : 'names'}</span>`
                    + `</summary><div class="info-chips">${chips}</div></details></div>`;
            }

            // The title carries the node's type as its icon — the same glyph
            // used in the legend, so the panel identifies itself at a glance.
            const icon = document.getElementById('info-type-icon');
            if (icon) {
                icon.innerHTML = nodeIconSvg(d.group);
                icon.title = d.group || '';
            }
            document.getElementById('info-title').textContent = truncateName(d.name);

            // This node's neighbours, straight off the adjacency index. The
            // endpoints are resolved through nodeById rather than read off the
            // edge: force-graph rewrites source/target into node objects on
            // the arrays it is handed, and in solo mode the graph's own edges
            // are never handed to it, so they still carry plain ids.
            const related = [];
            edgesOf(d.id).forEach(e => {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                const otherId = sId === d.id ? tId : sId;
                const node = state.nodeById.get(otherId);
                if (!node) return;
                related.push({ node, rel: e.rel, dir: sId === d.id ? 'out' : 'in' });
            });

            // Related-tab filters. Edge chips count this node's neighbours
            // per edge type; the keyword box narrows by name. Both persist
            // across selections so tracing "save" callers keeps the filter,
            // but edge types are pruned to what this node has — a stale type
            // selection can't hide every row.
            const relCounts = {};
            related.forEach(({ rel }) => {
                const r = rel || 'other';
                relCounts[r] = (relCounts[r] || 0) + 1;
            });
            if (!state.relEdgeFilters) state.relEdgeFilters = new Set();
            [...state.relEdgeFilters].forEach(t => { if (!relCounts[t]) state.relEdgeFilters.delete(t); });

            // Direction filter, same persistence contract as the edge chips —
            // but it is never pruned: "in" staying selected on a node with no
            // incoming edges reads as an answer, not a stuck filter.
            const dirCounts = { all: related.length, in: 0, out: 0 };
            related.forEach(({ dir }) => { dirCounts[dir]++; });
            if (state.relDir !== 'in' && state.relDir !== 'out') state.relDir = 'all';

            let html = `
                ${sourceNote('fields', d)}
                ${fieldRow('id', `<span class="info-id" title="${escapeHtml(d.id)}">${escapeHtml(d.id)}</span><button class="info-copy-btn" data-copy="${escapeHtml(d.id)}" title="Copy node id" aria-label="Copy node id"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h8"/></svg></button>`, 'info-id-value')}
                ${fieldRow('name', escapeHtml(d.name))}
                <div class="info-row">${fieldLabel('type')}<span class="info-type-badge" style="background:${typeColor}20;color:${typeColor}">${nodeIconSvg(d.group, 'badge-icon')}${d.group}</span></div>
            `;
            // Directly under the heading: that a symbol is a contract with
            // something outside the system is the first thing worth knowing
            // about it, ahead of where it lives or how long it is.
            if (d.boundaries && d.boundaries.length) {
                const parts = d.boundaries.map(b => {
                    const dir = b.direction === 'Inbound' ? 'in' : 'out';
                    const detail = b.detail
                        ? `<span class="info-dim"> ${escapeHtml(b.detail)}</span>`
                        : '';
                    return `<span class="boundary-tag boundary-${dir}"`
                        + ` title="${escapeHtml(dir === 'in' ? 'A way into the system' : 'A way out of the system')}"`
                        + ` >${escapeHtml(b.kind)}</span>${detail}`;
                });
                html += fieldRow('boundary', parts.join('<span class="info-dim"> · </span>'));
            }
            if (d.file) html += fieldRow('file', escapeHtml(d.file));
            if (d.startLine) {
                // These formats aren't line-oriented — the indexer repurposes
                // the field as a page index, so labelling it "lines" would be
                // a straight lie.
                const isPaged = /\.(pdf|docx?|xlsx?|pptx?|odt|ods|odp|rtf)$/i.test(d.file || '');
                const span = d.endLine && d.endLine !== d.startLine ? `${d.startLine}–${d.endLine}` : `${d.startLine}`;
                const count = (d.endLine || d.startLine) - d.startLine + 1;
                html += fieldRow('lines', isPaged
                    ? `p.${span}`
                    : `${span}<span class="info-dim"> · ${count} line${count === 1 ? '' : 's'}</span>`);
            }
            if (d.metrics) {
                const m = d.metrics;
                const nest = m.maxNesting ?? m.max_nesting ?? '–';
                html += fieldRow('metrics',
                    `<span title="Lines of code in this symbol's body">LOC ${m.loc ?? '–'}</span>`
                    + `<span class="info-dim"> · </span><span title="Number of declared parameters">Params ${m.params ?? '–'}</span>`
                    + `<span class="info-dim"> · </span><span title="Deepest level of nested blocks — a rough complexity proxy">Nest ${nest}</span>`);
            }
            if (d.signature) {
                const params = d.signature.params ? d.signature.params.map(p => p.name + (p.type || p.param_type ? ':' + (p.type || p.param_type) : '')).join(', ') : '';
                const ret = d.signature.returnType || d.signature.return_type ? ' => ' + (d.signature.returnType || d.signature.return_type) : '';
                html += fieldRow('signature', `(${escapeHtml(params)})${escapeHtml(ret)}`);
            }
            if (d.docstring) html += longFieldRow('docstring', d.docstring);
            if (d.calls) html += chipRow('calls', d.calls);
            if (d.extends) html += chipRow('extends', d.extends);
            if (d.implements) html += chipRow('implements', d.implements);

            const activeTab = ['related', 'hierarchy', 'chunk'].includes(state.infoTab) ? state.infoTab : 'preview';
            const tabButton = (key, suffix = '') => {
                const [label, tip] = TAB_DOCS[key];
                return `<button class="info-tab${activeTab === key ? ' active' : ''}" data-view="${key}"`
                    + ` title="${escapeHtml(tip)}">${escapeHtml(label)}${suffix}</button>`;
            };
            html += `<div class="related-section">
                <div class="info-tabs">
                    ${tabButton('preview')}
                    ${tabButton('chunk')}
                    ${tabButton('hierarchy')}
                    ${tabButton('related', related.length ? '<span class="tab-count">' + related.length + '</span>' : '')}
                </div>
                <div class="info-view${activeTab === 'preview' ? ' active' : ''}" data-view="preview">
                    ${sourceNote('preview', d)}
                    <div id="preview-content"></div>
                </div>
                <div class="info-view${activeTab === 'chunk' ? ' active' : ''}" data-view="chunk">
                    ${sourceNote('chunk', d)}
                    <div id="chunk-content"></div>
                </div>
                <div class="info-view${activeTab === 'hierarchy' ? ' active' : ''}" data-view="hierarchy">
                    ${sourceNote('hierarchy', d)}
                    ${buildHierarchyHtml(d)}
                </div>
                <div class="info-view${activeTab === 'related' ? ' active' : ''}" data-view="related">
                    ${sourceNote('related', d)}`;
            if (related.length > 0) {
                // One chip per edge type, sorted heaviest first, each carrying
                // its count. Toggling narrows the list to the active types
                // (none active = all).
                const relEdgeChips = Object.keys(relCounts)
                    .sort((a, b) => relCounts[b] - relCounts[a])
                    .map(t => {
                        const color = config.getRelColor(t);
                        return `<button class="filter-chip${state.relEdgeFilters.has(t) ? ' active' : ''}" data-rel="${escapeHtml(t)}" title="${escapeHtml(EDGE_DOCS[t] || ('Edge type: ' + t))}">
                            <span class="chip-dot" style="background:${color};color:${color}"></span>
                            <span>${escapeHtml(t)}</span>
                            <span class="chip-count">${relCounts[t]}</span>
                        </button>`;
                    }).join('');
                // Direction segmented control. The arrows match the ones on
                // the rows themselves, so "→ Out" and a row's "→ calls" are
                // visibly the same claim.
                const dirBtn = (key, arrow, label, tip) =>
                    `<button class="rel-dir${state.relDir === key ? ' active' : ''}" data-dir="${key}" title="${escapeHtml(tip)}">
                        <span class="rel-dir-arrow">${arrow}</span>${label}<span class="chip-count">${dirCounts[key]}</span>
                    </button>`;
                html += `<div class="rel-filters">
                        <input class="rel-search" type="search" placeholder="Filter by name…" value="${escapeHtml(state.relKeyword || '')}" autocomplete="off">
                        <div class="rel-dir-switch" role="group" aria-label="Edge direction">
                            ${dirBtn('all', '↔', 'All', 'Every edge touching this node, in either direction')}
                            ${dirBtn('in', '←', 'In', 'Only edges pointing at this node — its callers, importers, containers')}
                            ${dirBtn('out', '→', 'Out', 'Only edges leaving this node — what it calls, imports, contains')}
                        </div>
                        <div class="filter-chips rel-edge-chips">${relEdgeChips}</div>
                    </div>
                    <div class="related-list" id="related-list"></div>
                    <div class="hier-empty" id="related-empty" style="display:none">No related nodes match the filters.</div>`;
            } else {
                html += `<div class="hier-empty">No edges touch this node — it is isolated in the graph.</div>`;
            }
            html += `</div>
            </div>`;

            body.innerHTML = html;
            // The node id is command fuel (ug get_code <id> …), so a one-click
            // copy beats drag-selecting a path-laden string.
            body.querySelectorAll('.info-copy-btn').forEach(btn => {
                btn.addEventListener('click', async () => {
                    try {
                        await navigator.clipboard.writeText(btn.dataset.copy);
                        btn.classList.add('copied');
                    } catch { /* clipboard blocked — select the id text as a fallback */ }
                    setTimeout(() => btn.classList.remove('copied'), 1500);
                });
            });
            // Related entries, hierarchy rows and resolvable chips all
            // navigate to their node. Factored out so the related list —
            // re-rendered by the filters below — can re-bind its rows.
            const wireNavTargets = scope => {
                scope.querySelectorAll('.related-item, .hier-row, .info-chip.linked').forEach(item => {
                    item.addEventListener('click', (ev) => {
                        const t = state.nodeById.get(item.dataset.id);
                        if (!t) return;
                        // Same modifier as the search results: keep what is on
                        // the canvas and add this neighbour to it.
                        if (ev.metaKey || ev.ctrlKey) state._viewMerge = true;
                        if (state.pathMode) {
                            state.pathSource = t.id;
                            const hint = info.querySelector('.path-hint');
                            if (hint) hint.querySelector('strong').textContent = truncateName(state.pathSource);
                            showPathResult('');
                        } else {
                            handleClick(null, t);
                            focusNode(t);
                        }
                    });
                });
            };
            wireNavTargets(body);

            // Related-tab filtering: keyword + per-edge chips. Both re-render
            // the list only, not the whole panel.
            const relList = body.querySelector('#related-list');
            if (relList) {
                const relItemHtml = ({ node, rel, dir }) => {
                    const arrow = dir === 'out' ? '→' : '←';
                    // The label carries the verb in the right voice for the
                    // direction ("contains" vs "contained by"), so a Contains
                    // edge is unambiguous without reading the tooltip. The
                    // tooltip still spells the full sentence + edge meaning.
                    const phrase = dir === 'out'
                        ? `This node ${edgeVerb(rel)} ${node.name}`
                        : `${node.name} ${edgeVerb(rel)} this node`;
                    const tip = `${phrase}. ${EDGE_DOCS[rel] || ''} Click to select it.`;
                    return `<div class="related-item" data-id="${node.id}" title="${escapeHtml(tip.trim())}">
                        ${nodeIconSvg(node.group)}
                        <span class="name">${escapeHtml(truncateName(node.name))}</span>
                        <span class="rel">${arrow} ${escapeHtml(edgeDirLabel(rel, dir))}</span>
                    </div>`;
                };
                const renderRelList = () => {
                    const q = (state.relKeyword || '').trim().toLowerCase();
                    const shown = related.filter(({ node, rel, dir }) => {
                        if (state.relDir !== 'all' && dir !== state.relDir) return false;
                        if (state.relEdgeFilters.size && !state.relEdgeFilters.has(rel || 'other')) return false;
                        if (q && !((node.name || '').toLowerCase().includes(q) || (node.id || '').toLowerCase().includes(q))) return false;
                        return true;
                    });
                    relList.innerHTML = shown.map(relItemHtml).join('');
                    const empty = body.querySelector('#related-empty');
                    if (empty) empty.style.display = shown.length ? 'none' : 'block';
                    wireNavTargets(relList);
                };
                const searchInput = body.querySelector('.rel-search');
                if (searchInput) searchInput.addEventListener('input', () => {
                    state.relKeyword = searchInput.value;
                    renderRelList();
                });
                body.querySelectorAll('.rel-dir-switch .rel-dir').forEach(btn => {
                    btn.addEventListener('click', () => {
                        state.relDir = btn.dataset.dir;
                        body.querySelectorAll('.rel-dir-switch .rel-dir')
                            .forEach(b => b.classList.toggle('active', b === btn));
                        renderRelList();
                    });
                });
                body.querySelectorAll('.rel-edge-chips .filter-chip').forEach(chip => {
                    chip.addEventListener('click', () => {
                        const t = chip.dataset.rel;
                        if (state.relEdgeFilters.has(t)) { state.relEdgeFilters.delete(t); chip.classList.remove('active'); }
                        else { state.relEdgeFilters.add(t); chip.classList.add('active'); }
                        renderRelList();
                    });
                });
                renderRelList();
            }

            // Preview tab: show the selected item's chunk content. The full text
            // (node_text) only arrives via the async DB enrichment, so render
            // immediately from any cached row, then again when the fetch lands.
            const cachedRow = state.dbRowCache ? state.dbRowCache.get(d.id) : null;
            renderPreview(d, cachedRow);
            renderChunk(d, cachedRow);

            body.querySelectorAll('.info-tab').forEach(tab => {
                tab.addEventListener('click', () => {
                    body.querySelectorAll('.info-tab').forEach(t => t.classList.remove('active'));
                    body.querySelectorAll('.info-view').forEach(v => v.classList.remove('active'));
                    tab.classList.add('active');
                    body.querySelector('.info-view[data-view="' + tab.dataset.view + '"]').classList.add('active');
                    state.infoTab = tab.dataset.view;
                });
            });

            if (d.file) {
                jumpBtn.style.display = 'flex';
                const target = d.startLine ? `${d.file}#L${d.startLine}` : d.file;
                jumpBtn.onclick = async () => {
                    await navigator.clipboard.writeText(target);
                    jumpBtn.title = 'Copied!';
                    setTimeout(() => jumpBtn.title = 'Copy file path', 1500);
                };
            } else {
                jumpBtn.style.display = 'none';
            }

            info.classList.add('visible');

            // Anchor focus on this node (unless we're mid neighbour-step, which
            // keeps the existing anchor) and record it in the nav history.
            if (!state.suppressFocusReanchor) enterFocus(d);
            if (!state.suppressHistory) recordHistory(d.id);

            state.selectedNode = d;
            // Keep the Walk pane's seed in sync with whatever the user just
            // selected, anywhere — so opening Walk always starts from the
            // node they were just looking at. No-op mid-walk (the running
            // walk owns its seed) and doesn't switch tabs.
            syncWalkSeed(d);
            bumpGraphStyles();
            updateNavbar();
            // A real selection is a navigation: push a history entry so the
            // browser Back button retraces it. Suppressed replays (breadcrumb,
            // popstate) skip this so they don't re-record.
            if (!state.suppressHistory) pushUrlState();

            enrichFromDb(d.id);
        }

        async function enrichFromDb(id) {
            if (!state.capabilities || !state.capabilities.db_ready) return;
            const requestedId = id;
            try {
                const res = await fetch(`/api/db/node/${encodeURIComponent(id)}`);
                // 404 means the node is in graph.json but never reached the
                // knowledge store (e.g. an embedder hasn't run on it). Sub an
                // empty row so renderChunk shows its "no indexed chunk" message
                // instead of leaving "Loading chunk…" pinned forever.
                const row = res.ok ? await res.json() : { node_text: '' };
                if (!state.dbRowCache) state.dbRowCache = new Map();
                state.dbRowCache.set(requestedId, row);
                if (!state.selectedNode || state.selectedNode.id !== requestedId) return;
                renderPreview(state.selectedNode, row);
                renderChunk(state.selectedNode, row);
            } catch (err) {
                // graceful fallthrough — graph.json data remains as-is
            }
        }

        // Fills the "Chunk" tab with what the knowledge base actually stores
        // for this node — the text that was embedded, broken back into the
        // parts `build_node_text` assembled it from. This is the honest
        // answer to "why did semantic search match this?", which the raw
        // source in Preview can't give.
        function renderChunk(node, row) {
            const container = document.getElementById('chunk-content');
            if (!container) return;
            container.innerHTML = '';

            const dbReady = !!(state.capabilities && state.capabilities.db_ready);
            if (!dbReady) {
                container.innerHTML = '<div class="hier-empty">No knowledge base attached — '
                    + 'run <code>ug serve</code> with an indexed project to see chunks.</div>';
                return;
            }
            if (!row) {
                container.innerHTML = '<div class="hier-empty">Loading chunk…</div>';
                return;
            }
            const text = (row.node_text || '').trim();
            if (!text) {
                // The node exists in graph.json but no vector was written for
                // it. The usual cause is project-wide: `ug gen --no-ingest`,
                // or the embedder was down during ingest. Offer the in-UI
                // trigger when search_ready is false (i.e. nothing has been
                // embedded); when search_ready is true this is a per-node
                // skip (over budget, filtered) and ingest won't change it.
                const frag = document.createDocumentFragment();
                const note = document.createElement('div');
                note.className = 'hier-empty';
                note.textContent = 'This node has no indexed chunk '
                    + '(it exists in graph.json but not in the vector store).';
                frag.appendChild(note);
                const caps = state.capabilities;
                if (caps && !caps.search_ready && caps.db_ready !== undefined) {
                    const cta = document.createElement('button');
                    cta.type = 'button';
                    cta.className = 'cap-cta';
                    cta.id = 'node-ingest-btn';
                    cta.textContent = 'Ingest now';
                    const status = document.createElement('div');
                    status.className = 'cap-ingest-status';
                    status.id = 'node-ingest-status';
                    frag.append(cta, status);
                    // If an ingest is already running (started from another
                    // banner) this freshly-rendered button joins it: disabled
                    // and showing the same progress.
                    ingestStateForFreshButton(cta, status);
                    cta.addEventListener('click', (e) => {
                        e.preventDefault();
                        triggerIngest({ btn: cta, status });
                    });
                }
                container.innerHTML = '';
                container.appendChild(frag);
                return;
            }

            const parts = parseChunkText(text);
            const frag = document.createDocumentFragment();

            const stats = document.createElement('div');
            stats.className = 'chunk-stats';
            const lines = (row.description || '').split('\n').length;
            stats.innerHTML = `<span title="Length of the whole embedded string. The dense vector is a function of exactly this text — nothing longer reached the model, nothing shorter was left out.">`
                + `<b>${text.length.toLocaleString()}</b> chars embedded</span>`
                + `<span title="Neighbour names folded in as context. Comes last in the embedded text, so it is the first field the model's token window truncates.">`
                + `<b>${parts.related.length}</b> related name${parts.related.length === 1 ? '' : 's'}</span>`
                + (row.start_line ? `<span title="Lines this node spans in its file. The text of those lines is under Captured source below, and read live in the Source tab.">`
                    + `<b>${(row.end_line || row.start_line) - row.start_line + 1}</b> source lines</span>` : '');
            frag.appendChild(stats);
            void lines;

            // What each part of the embedded text is, where it came from, and
            // what it does for retrieval. These sections are not stored
            // separately — they are `node_text` split back into the fields
            // `build_node_text` assembled it from, which is worth saying so
            // nobody goes looking for a "description column".
            const tooltips = {
                'Heading': 'The "Type: Name" prefix the embedding text opens with. Reconstructed '
                    + 'from node_text, not stored on its own. The name appears twice — exactly, and '
                    + 'split into words — so both an exact-symbol query and a prose query can reach it.',
                'Description': 'The node\'s docstring, or for a document section its prose. The main '
                    + 'body of what was embedded, and usually the only natural language the node has. '
                    + 'Trimmed to the description budget shown under Indexing limits.',
                'Signature': 'Parameters and return type, folded in so a query naming a type can '
                    + 'reach an otherwise undocumented symbol.',
                'Notes from comments': 'Prose lifted from inline comments inside the node\'s body, '
                    + 'after filtering out commented-out code, licence banners and tool directives. '
                    + 'A separate source from the docstring, with its own cap.',
                'Also indexed': 'Text present in the embedded string that did not match any known '
                    + 'field — usually a synthesized structural synopsis such as "defined in src/x.rs".',
                'Related names': 'Neighbour node names folded in as context. Comes last in the '
                    + 'embedded text, which makes it the first thing the model\'s token window '
                    + 'truncates. Names that resolve to an indexed node are clickable.',
            };

            const section = (label, value, cls) => {
                if (!value) return;
                const d = document.createElement('div');
                d.className = 'chunk-section' + (cls ? ' ' + cls : '');
                const l = document.createElement('div');
                l.className = 'chunk-label';
                l.textContent = label;
                l.title = tooltips[label] || '';
                const v = document.createElement('div');
                v.className = 'chunk-value';
                v.textContent = value;
                d.append(l, v);
                frag.appendChild(d);
            };

            // As `longFieldRow` in the detail panel: a document section's
            // description is now a whole paragraph, so it collapses rather
            // than burying the fields under it.
            const collapsibleSection = (label, value, cls) => {
                if (!value) return;
                if (value.length <= 180) return section(label, value, cls);
                const d = document.createElement('details');
                d.className = 'chunk-section chunk-collapse' + (cls ? ' ' + cls : '');
                const s = document.createElement('summary');
                const l = document.createElement('span');
                l.className = 'chunk-label';
                l.textContent = label;
                l.title = tooltips[label] || '';
                const count = document.createElement('span');
                count.className = 'chunk-collapse-count';
                count.textContent = `${value.length.toLocaleString()} chars`;
                s.append(l, count);
                const v = document.createElement('div');
                v.className = 'chunk-value';
                v.textContent = value;
                d.append(s, v);
                frag.appendChild(d);
            };

            section('Heading', parts.heading);
            collapsibleSection('Description', parts.description);
            section('Signature', parts.signature, 'mono');
            collapsibleSection('Notes from comments', parts.notes);
            if (parts.rest) section('Also indexed', parts.rest);

            if (parts.related.length) {
                const d = document.createElement('details');
                d.className = 'chunk-section chunk-collapse';
                const s = document.createElement('summary');
                const l = document.createElement('span');
                l.className = 'chunk-label';
                l.textContent = `Related names (${parts.related.length})`;
                l.title = tooltips['Related names'];
                s.title = tooltips['Related names'];
                s.appendChild(l);
                const list = document.createElement('div');
                list.className = 'chunk-related';
                parts.related.forEach(name => {
                    const chip = document.createElement('span');
                    chip.className = 'chunk-chip';
                    chip.textContent = name;
                    // These are the names the embedder saw; many are real
                    // nodes, so make the ones we can resolve clickable.
                    const hit = state.nodeById && findNodeByName(name);
                    if (hit) {
                        chip.classList.add('linked');
                        chip.title = hit.id;
                        chip.addEventListener('click', () => { handleClick(null, hit); focusNode(hit); });
                    }
                    list.appendChild(chip);
                });
                d.append(s, list);
                frag.appendChild(d);
            }

            const raw = document.createElement('details');
            raw.className = 'chunk-raw';
            const sum = document.createElement('summary');
            sum.textContent = 'Embedded text, verbatim';
            sum.title = 'The exact string that was sent to the embedding model, before it was '
                + 'split into the fields above. This is the node_text column — the dense vector '
                + 'is a function of precisely this text and nothing else.';
            const pre = document.createElement('pre');
            pre.textContent = text;
            raw.append(sum, pre);
            frag.appendChild(raw);

            const stored = renderStoredSource(row);
            if (stored) frag.appendChild(stored);

            const meta = renderChunkStorage(row);
            if (meta) frag.appendChild(meta);

            const limits = renderChunkLimits(row, parts, text);
            if (limits) frag.appendChild(limits);

            // The whole stored row, verbatim. Everything above is this object
            // reshaped for reading; this is the raw answer for "what does the
            // KB actually hold for this node?" — the same payload
            // /api/db/node/<id> returns.
            const rawJson = document.createElement('details');
            rawJson.className = 'chunk-raw';
            const rawJsonSum = document.createElement('summary');
            rawJsonSum.textContent = 'Raw node data (JSON)';
            rawJsonSum.title = 'The full stored row for this node, as /api/db/node/<id> returns it — '
                + 'every field the knowledge base holds, before the sections above reshape it.';
            const rawJsonPre = document.createElement('pre');
            rawJsonPre.className = 'chunk-json';
            rawJsonPre.textContent = JSON.stringify(row, null, 2);
            rawJson.append(rawJsonSum, rawJsonPre);
            frag.appendChild(rawJson);

            container.appendChild(frag);
        }

        // The `code` column: the exact source the store captured at index
        // time.
        //
        // Worth its own section rather than a line in the metadata, because
        // it is not the same text as the Preview tab. Preview reads the file
        // as it is on disk *now*; this is the snapshot an agent's snippet
        // reads return and the keyword index was built from. When the two
        // disagree the store is stale, and being able to see both is the
        // only way to tell that from the UI.
        function renderStoredSource(row) {
            const s = row.storage;
            if (!s || !s.code) return null;

            const details = document.createElement('details');
            details.className = 'chunk-raw';
            const summary = document.createElement('summary');
            const chars = Number(s.code_chars || 0).toLocaleString();
            summary.textContent = s.stale === true
                ? `Captured source — ${chars} chars, stale`
                : `Captured source — ${chars} chars`;
            summary.title = 'The source this node was indexed from, stored in the knowledge base. '
                + 'This is what a snippet read returns and what the keyword vector was built from. '
                + 'The Source tab shows the same span read live from disk when the repo is '
                + 'available, and the indexed copy otherwise — when the two differ, the index '
                + 'is behind.';
            details.appendChild(summary);

            const note = document.createElement('div');
            note.className = 'chunk-limit-note';
            note.textContent = s.stale === true
                ? 'What the store holds and what search returns. The file on disk has '
                    + 'changed since indexing, so Preview above will not match this.'
                : 'What the store holds and what search returns, captured at index time. '
                    + 'Preview above shows the same span live from disk when the repo is '
                    + 'available, and this copy when it is not.';
            details.appendChild(note);

            const pre = document.createElement('pre');
            pre.textContent = s.code_truncated
                ? s.code + `\n… truncated for display (${chars} chars stored)`
                : s.code;
            details.appendChild(pre);
            return details;
        }

        // The rest of the stored row: everything the vector store holds about
        // this node that isn't the embedded text itself. Collapsed, because
        // it answers follow-up questions ("is this copy stale?", "did the
        // keyword vector get truncated?") rather than the first one.
        const storageTooltips = {
            'Dense vector': 'Width of the semantic vector, set by the embedding model. A query is '
                + 'embedded by the same model and compared against this by cosine similarity. '
                + 'Changing model changes this width and requires a re-ingest.',
            'Keyword vector': 'How many distinct terms this node contributes to keyword search, '
                + 'BM25-weighted. Built from the embedded text plus the captured source at a '
                + 'discount, so code is findable by keyword without outvoting the description.',
            'Embedded text': 'Length of the node_text string the dense vector was computed from — '
                + 'the same text shown verbatim above.',
            'Captured source': 'Whether the store holds its own copy of the source. When it does '
                + 'not, snippet reads fall back to the working tree, which can disagree with what '
                + 'was indexed.',
            'Row updated': 'When this row last changed. Incremental ingest leaves untouched rows '
                + 'alone, so this marks the last real change rather than the last ug gen.',
            'File hash': 'blake3 of the whole file at index time. Compared against disk on every '
                + 'hydrate, which is what makes staleness detectable instead of silent.',
        };

        function renderChunkStorage(row) {
            const s = row.storage;
            if (!s) return null;

            const rows = [];
            const push = (label, value, note) => rows.push({ label, value, note, tip: storageTooltips[label] || '' });

            if (s.vector_dim) push('Dense vector', `${s.vector_dim} dims`);
            if (typeof s.sparse_dims === 'number') {
                const cap = capValue('sparse_dimensions');
                push('Keyword vector', `${s.sparse_dims.toLocaleString()} dims`,
                    cap && s.sparse_dims >= cap ? `at the ${cap.toLocaleString()}-dim cap — rarest terms dropped` : null);
            }
            push('Embedded text', `${Number(s.node_text_chars || 0).toLocaleString()} chars`);
            if (!s.code_chars) {
                push('Captured source', 'none — snippets read from disk');
            }
            if (s.last_update_at) {
                const d = new Date(s.last_update_at * 1000);
                push('Row updated', isNaN(d) ? String(s.last_update_at) : d.toLocaleString());
            }
            if (s.file_hash) {
                push('File hash', s.file_hash.slice(0, 12) + '…',
                    s.stale === true ? 'file changed since indexing — re-run ug gen'
                        : s.stale === false ? 'matches the file on disk' : 'file not readable now');
            }
            if (!rows.length) return null;

            const details = document.createElement('details');
            details.className = 'chunk-raw';
            const summary = document.createElement('summary');
            summary.textContent = s.stale === true ? 'Storage metadata — stale' : 'Storage metadata';
            summary.title = 'What the knowledge base holds about this node rather than in it: '
                + 'vector sizes, when the row last changed, and whether the file it came from '
                + 'still matches.';
            details.appendChild(summary);
            if (s.stale === true) details.open = true;

            const wrap = document.createElement('div');
            rows.forEach(r => {
                const el = document.createElement('div');
                el.className = 'chunk-limit' + (r.note && /cap|stale|changed/.test(r.note) ? ' hit' : '');
                const name = document.createElement('span');
                name.className = 'chunk-limit-name';
                name.textContent = r.label;
                name.title = r.tip;
                const val = document.createElement('span');
                val.className = 'chunk-limit-value';
                val.textContent = r.value;
                el.append(name, val);
                if (r.note) {
                    const note = document.createElement('div');
                    note.className = 'chunk-limit-effect';
                    note.textContent = r.note;
                    el.appendChild(note);
                }
                wrap.appendChild(el);
            });
            details.appendChild(wrap);
            return details;
        }

        // A published cap's value by id, or null when capabilities haven't
        // loaded (or the server is older than the limits block).
        function capValue(id) {
            const caps = state.capabilities && state.capabilities.limits && state.capabilities.limits.caps;
            const hit = Array.isArray(caps) && caps.find(c => c.id === id);
            return hit ? Number(hit.value) : null;
        }

        // The caps that shaped this chunk, from /api/capabilities.
        //
        // A chunk that looks cut off is almost always a cap doing its job,
        // not a bug — but nothing on screen said so, so the honest reading
        // was "the index is broken". This lists the caps that apply to this
        // node's file type and marks the ones that measurably bit it.
        function renderChunkLimits(row, parts, text) {
            const info = state.capabilities && state.capabilities.limits;
            if (!info || !Array.isArray(info.caps) || !info.caps.length) return null;

            const ext = String(row.file || '').split('.').pop().toLowerCase();
            const applies = c => !c.extensions || !c.extensions.length || c.extensions.includes(ext);
            const caps = info.caps.filter(applies);
            if (!caps.length) return null;

            // Whether a cap left a visible mark. Only claims a hit where the
            // evidence is in the chunk itself: a truncation ellipsis, or a
            // list that came back exactly full. The rest are reference.
            const truncated = s => /…\.?$/.test((s || '').trim());
            const hitOf = c => {
                if (c.id === 'markdown_section_text' || c.id === 'document_page_text') {
                    return truncated(parts.description);
                }
                if (c.id === 'related_names') return parts.related.length >= c.value;
                if (c.id === 'node_comments') return parts.notes.length >= c.value;
                if (c.id === 'document_page_name') return truncated(parts.heading);
                return false;
            };

            const hits = caps.filter(hitOf);
            const details = document.createElement('details');
            details.className = 'chunk-raw';
            const summary = document.createElement('summary');
            summary.textContent = hits.length
                ? `Indexing limits — ${hits.length} reached`
                : 'Indexing limits';
            summary.title = 'Caps that shaped what you see above. They decide what this node\'s '
                + 'vector can possibly match on, so a search miss is often a cap rather than a bad '
                + 'embedding. Ones marked "reached" measurably bit this node.';
            details.appendChild(summary);
            if (hits.length) details.open = true;

            const wrap = document.createElement('div');
            caps.forEach(c => {
                const rowEl = document.createElement('div');
                rowEl.className = 'chunk-limit' + (hitOf(c) ? ' hit' : '');
                const name = document.createElement('span');
                name.className = 'chunk-limit-name';
                name.textContent = c.label;
                name.title = `${STAGE_DOCS[c.stage] || ''} Defined at ${c.source}.`.trim();
                const val = document.createElement('span');
                val.className = 'chunk-limit-value';
                val.textContent = `${Number(c.value).toLocaleString()} ${c.unit}`;
                val.title = `${Number(c.value).toLocaleString()} ${c.unit} — from ${c.source}`;
                const eff = document.createElement('div');
                eff.className = 'chunk-limit-effect';
                eff.textContent = c.effect;
                rowEl.append(name, val, eff);
                wrap.appendChild(rowEl);
            });

            // The model's own window binds above every cap above, and applies
            // with no marker of any kind — worth stating even though we can
            // only estimate the token count (~4 chars/token for English).
            const note = document.createElement('div');
            note.className = 'chunk-limit-note';
            const win = info.embedder_token_window;
            const est = Math.round(text.length / 4);
            if (win) {
                const over = est > win;
                note.innerHTML = `Embedder <code>${escapeHtml(info.embedder_model || '')}</code> reads at most `
                    + `<b>${win.toLocaleString()}</b> tokens. This chunk is ~<b>${est.toLocaleString()}</b>`
                    + (over
                        ? ' — past the window, so the tail was dropped before embedding.'
                        : ', within the window.');
            } else {
                note.textContent = 'The embedding model also truncates at its own token window, '
                    + 'which is unknown for the model this server has open.';
            }
            wrap.appendChild(note);

            details.appendChild(wrap);
            return details;
        }

        // `build_node_text` writes "<Type>: <name>. <description>. Signature:
        // <sig>. Related: a, b, c" — split it back into its parts so the panel
        // can show them as fields instead of one run-on paragraph.
        function parseChunkText(text) {
            const out = { heading: '', description: '', signature: '', notes: '', related: [], rest: '' };
            let body = text;

            const relIdx = body.search(/(^|\.\s|\n)Related:\s/);
            if (relIdx >= 0) {
                const after = body.slice(relIdx).replace(/^[.\s]*Related:\s*/, '');
                out.related = after.split(',').map(s => s.trim()).filter(Boolean);
                body = body.slice(0, relIdx);
            }
            // Prose lifted from the symbol's own comments. Its own field
            // rather than part of the description: it comes from a different
            // source and is governed by its own cap.
            const notesIdx = body.search(/(^|\.\s|\n)Notes:\s/);
            if (notesIdx >= 0) {
                out.notes = body.slice(notesIdx).replace(/^[.\s]*Notes:\s*/, '').trim()
                    .replace(/\.$/, '');
                body = body.slice(0, notesIdx);
            }
            const sigIdx = body.search(/(^|\.\s|\n)Signature:\s/);
            if (sigIdx >= 0) {
                out.signature = body.slice(sigIdx).replace(/^[.\s]*Signature:\s*/, '').trim()
                    .replace(/\.$/, '');
                body = body.slice(0, sigIdx);
            }
            // The head is "<Type>: <name>." — the rest of the first block is
            // the node's description.
            const head = body.match(/^([^.\n]{0,80}?:\s*[^.\n]+?)\.\s*/);
            if (head) {
                out.heading = head[1].trim();
                body = body.slice(head[0].length);
            }
            out.description = body.trim().replace(/\.+$/, '');
            return out;
        }

        // Resolve a bare symbol name from a chunk's "Related:" list back to a
        // graph node, when one exists.
        function findNodeByName(name) {
            if (!state.nodeById) return null;
            if (state.nodeById.has(name)) return state.nodeById.get(name);
            if (!state._nameIndex) {
                state._nameIndex = new Map();
                state.graph.nodes.forEach(n => {
                    if (!state._nameIndex.has(n.name)) state._nameIndex.set(n.name, n);
                    const short = String(n.name).split('/').pop();
                    if (short && !state._nameIndex.has(short)) state._nameIndex.set(short, n);
                });
            }
            return state._nameIndex.get(name) || null;
        }

        // Fills the right-panel "Preview" tab with the *actual* source content of
        // the selected item: the whole file for a File node, or the
        // start..end line span for a chunk/symbol. (node_text in the DB is a
        // synthetic embedding string, not the source, so we read the file via
        // /api/file instead — which serves the live repo file, or the indexed
        // copy when the repo path is unavailable.) `row` is the DB hydrate that
        // carries the file path + line range; it may be null on the first
        // synchronous render.
        async function renderPreview(node, row) {
            const container = document.getElementById('preview-content');
            if (!container) return;

            const dbReady = !!(state.capabilities && state.capabilities.db_ready);
            const file = (row && row.file) || (node && node.file) || '';

            // Without the DB hydrate we don't know the file path (graph.json
            // doesn't carry it), so we can't locate the source yet.
            if (!file) {
                container.innerHTML = (dbReady && !row)
                    ? `<div class="preview-status">Loading preview…</div>`
                    : `<div class="preview-status">No source file recorded for this item.</div>`;
                return;
            }

            const isFileNode = (node && node.group === 'File') || (row && row.node_type === 'File');
            const start = isFileNode ? null : ((node && node.startLine) || (row && row.start_line) || null);
            const end = isFileNode ? null : ((node && node.endLine) || (row && row.end_line) || null);
            const isMd = /\.(md|markdown|mdx)$/i.test(file);

            // Selecting a node renders this panel twice: once synchronously from
            // whatever row is cached, then again when the DB hydrate lands —
            // because the hydrate can supply a file path or line range the
            // graph node doesn't carry. When it supplies nothing new, the
            // second render would refetch the identical span, so requests are
            // keyed on what actually determines the output and skipped when
            // that hasn't moved. The node id is in the key so two nodes
            // sharing a span still get their own header.
            //
            // The key alone can't say whether the answer it stands for is
            // still on screen: every selection rebuilds the whole panel, so
            // re-picking the node that is already selected hands us a fresh,
            // *empty* container while the key still matches — and the guard
            // would skip the only render that would fill it, leaving the tab
            // permanently blank. Pin the key to the element it was rendered
            // into, so it only suppresses a repeat into that same container.
            const key = `${(node && node.id) || ''}|${file}|${start || ''}|${end || ''}`;
            if (state.previewKey === key && state.previewEl === container) return;
            state.previewKey = key;
            state.previewEl = container;

            // Guard against a slow fetch for a previously-selected node landing
            // after the user has moved on.
            const token = (state.previewToken = (state.previewToken || 0) + 1);
            container.innerHTML = `<div class="preview-status">Loading preview…</div>`;

            let qs = `path=${encodeURIComponent(file)}`;
            if (start) { qs += `&start=${start}`; if (end) qs += `&end=${end}`; }

            let data = null;
            try {
                const res = await fetch(`/api/file?${qs}`);
                if (!res.ok) throw new Error('HTTP ' + res.status);
                data = await res.json();
            } catch (err) {
                // Release the key so the hydrate's render — or a re-selection —
                // can retry rather than inheriting a failure.
                state.previewKey = null;
                state.previewEl = null;
                if (state.previewToken !== token) return;
                const fallback = (row && row.description) || (node && node.docstring) || '';
                container.innerHTML = fallback
                    ? `<pre class="preview-code">${escapeHtml(fallback)}</pre>`
                    : `<div class="preview-status">Couldn't load <code>${escapeHtml(file)}</code>.</div>`;
                return;
            }

            // Selection moved on while we awaited — drop this result.
            if (state.previewToken !== token) return;
            if (!state.selectedNode || !node || state.selectedNode.id !== node.id) return;

            // Whether the copy shown came from the live repo or the index.
            // The endpoint reports which so the UI can say so rather than
            // imply a live read that never happened.
            const fromIndex = !!(data && data.source === 'db');
            const content = (data && typeof data.content === 'string') ? data.content : '';
            const lineLabel = start
                ? `L${start}${end && end !== start ? '–' + end : ''}`
                : (data && data.total_lines ? `${data.total_lines} lines` : '');
            // Says how much of the file is on screen. "Chunk" used to appear
            // here, which now collides with the Indexed tab's vocabulary —
            // this label is only ever about extent.
            const kindLabel = isFileNode ? 'Whole file' : 'Line span';
            const kindTip = isFileNode
                ? 'This node is the file itself, so the whole file is shown.'
                : 'Only the lines this node spans are shown, not the whole file.';
            const renderTip = isMd
                ? 'Rendered as Markdown. Toggle nothing — the raw text is under Captured source in the Indexed tab.'
                : '';
            const srcTitle = fromIndex
                ? 'Served from the index — the repo path for this file was unavailable when requested, so this is the source captured at index time.'
                : 'Path and line range read from disk for this view.';

            const meta = `<div class="preview-meta">
                <span title="${escapeHtml(kindTip)}">${kindLabel}</span>
                ${isMd ? `<span title="${escapeHtml(renderTip)}">Markdown</span>` : ''}
                ${fromIndex ? `<span class="preview-from-index" title="The repo was unavailable, so this is what the index captured — not the file as it is now.">Indexed copy</span>` : ''}
                <span class="src" title="${escapeHtml(srcTitle)}">${escapeHtml(file)}${lineLabel ? ' · ' + lineLabel : ''}</span>
            </div>`;

            if (!content) {
                container.innerHTML = meta + `<div class="preview-status">Empty content.</div>`;
                return;
            }

            container.innerHTML = isMd
                ? meta + `<div class="preview-md">${mdToHtml(content)}</div>`
                : meta + `<pre class="preview-code">${escapeHtml(content)}</pre>`;
        }

        // Compact, self-contained Markdown → HTML renderer. All text is HTML-escaped
        // before any inline markup is applied, and link targets are sanitised, so
        // chunk content can never inject raw HTML into the panel.
        function mdToHtml(src) {
            const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
            const inline = (s) => s
                .replace(/`([^`]+)`/g, (_, c) => `<code>${c}</code>`)
                .replace(/!\[([^\]]*)\]\([^)]*\)/g, (_, a) => `<em>${a || 'image'}</em>`)
                .replace(/\[([^\]]+)\]\(([^)\s]+)[^)]*\)/g, (_, t, u) => {
                    const safe = /^(https?:|\/|#|mailto:)/i.test(u) ? u : '#';
                    return `<a href="${safe}" target="_blank" rel="noopener noreferrer">${t}</a>`;
                })
                .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
                .replace(/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>');

            const lines = src.replace(/\r\n?/g, '\n').split('\n');
            const out = [];
            let i = 0, listType = null;
            const closeList = () => { if (listType) { out.push(`</${listType}>`); listType = null; } };

            while (i < lines.length) {
                const line = lines[i];

                const fence = line.match(/^```(\w*)\s*$/);
                if (fence) {
                    closeList();
                    const buf = [];
                    i++;
                    while (i < lines.length && !/^```\s*$/.test(lines[i])) { buf.push(lines[i]); i++; }
                    i++;
                    out.push(`<pre><code>${esc(buf.join('\n'))}</code></pre>`);
                    continue;
                }
                if (/^\s*([-*_])\1{2,}\s*$/.test(line)) { closeList(); out.push('<hr>'); i++; continue; }

                const h = line.match(/^(#{1,6})\s+(.*)$/);
                if (h) {
                    closeList();
                    const lvl = Math.min(h[1].length, 4);
                    out.push(`<h${lvl}>${inline(esc(h[2].trim()))}</h${lvl}>`);
                    i++; continue;
                }
                if (/^>\s?/.test(line)) {
                    closeList();
                    const buf = [];
                    while (i < lines.length && /^>\s?/.test(lines[i])) { buf.push(lines[i].replace(/^>\s?/, '')); i++; }
                    out.push(`<blockquote>${inline(esc(buf.join(' ')))}</blockquote>`);
                    continue;
                }
                const ul = line.match(/^\s*[-*+]\s+(.*)$/);
                if (ul) {
                    if (listType !== 'ul') { closeList(); out.push('<ul>'); listType = 'ul'; }
                    out.push(`<li>${inline(esc(ul[1]))}</li>`); i++; continue;
                }
                const ol = line.match(/^\s*\d+[.)]\s+(.*)$/);
                if (ol) {
                    if (listType !== 'ol') { closeList(); out.push('<ol>'); listType = 'ol'; }
                    out.push(`<li>${inline(esc(ol[1]))}</li>`); i++; continue;
                }
                if (/^\s*$/.test(line)) { closeList(); i++; continue; }

                closeList();
                const para = [line];
                i++;
                while (i < lines.length && !/^\s*$/.test(lines[i])
                    && !/^(#{1,6})\s/.test(lines[i]) && !/^```/.test(lines[i])
                    && !/^\s*[-*+]\s+/.test(lines[i]) && !/^\s*\d+[.)]\s+/.test(lines[i])
                    && !/^>\s?/.test(lines[i])) {
                    para.push(lines[i]); i++;
                }
                out.push(`<p>${inline(esc(para.join(' ')))}</p>`);
            }
            closeList();
            return out.join('\n');
        }

        function clearSelection() {
            state.selectedNode = null;
            exitFocus();
            bumpGraphStyles();
            document.getElementById('info').classList.remove('visible');
            // The history bar's Exit button is enabled off the selection, so it
            // has to be re-read here — otherwise it stays lit with nothing left
            // to exit.
            updateNavbar();
        }

        function resetView() {
            document.getElementById('search').value = '';
            document.getElementById('search-clear').classList.remove('visible');
            document.getElementById('search-suggestions').classList.remove('visible');
            state.searchQuery = '';
            document.getElementById('info').classList.remove('visible');
            state.nodeFilters.clear();
            state.edgeFilters.clear();
            document.querySelectorAll('.filter-chip').forEach(c => c.classList.remove('active'));
            // Reset means "back to how the page opened", and in solo mode that
            // is a bare canvas with the guidance overlay. Done before
            // applyFilters so the view is only rebuilt once.
            if (state.soloOnly) { state.viewSeeds = new Set(); state.viewExpanded = new Set(); }
            applyFilters();
            setActiveViewBtn('3d');
            frameGraph(600);
            state.selectedNode = null;
            exitFocus();
            state.history = [];
            state.historyIndex = -1;
            updateNavbar();
            bumpGraphStyles();
        }

