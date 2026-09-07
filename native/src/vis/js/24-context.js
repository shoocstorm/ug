        // ─── Context pack (info panel → Context tab) ────────────────────────
        //
        // `ug context` in the browser. One symbol's whole neighbourhood in one
        // budgeted call — its body, the callers that break if it changes, the
        // tests that re-verify it, what it leans on, any linked prose — each
        // item labelled with the role that put it there.
        //
        // The other four tabs are views of stored fields: read a row, print it.
        // This one is an *answer*, assembled server-side by the same code the
        // CLI and MCP run, and that difference is the whole reason it is worth
        // a tab. Related lists every edge touching a node; this says which of
        // them you have to read before you can safely change it.
        //
        // Three rules shape everything below:
        //
        //   1. **It is not free.** `context()` builds a map of every node and
        //      walks every edge (agent_tools/context.rs → find_usages.rs). On
        //      ~/.ug/big500k that is 485k nodes and 2.2M edges *per call*. So
        //      the fetch happens when the tab is opened and at no other time —
        //      never on selection, never on hover. §1a.
        //   2. **The budget is the feature.** A pack that quietly dropped its
        //      docs would be indistinguishable from a symbol with none. The
        //      header carries `used_chars / max_chars`, and `dropped` is
        //      rendered as "not shown: 4 dependency" exactly as the CLI does.
        //   3. **One renderer.** Copy asks the server for the same Markdown an
        //      MCP client gets (`"render": "markdown"` → render_context), so
        //      what the user pastes into their agent is what the agent tools
        //      emit. A second formatter written here would drift from it.

        // The pack is a claim about one symbol; the request that produced it is
        // keyed on everything that can change the answer. Pinned to the element
        // it rendered into as well, because re-selecting the already-selected
        // node rebuilds the panel and hands us a fresh, empty container while
        // the key still matches — the trap renderPreview documents at length.
        function contextKeyFor(node) {
            const roles = [...state.ctxRoles].sort().join(',');
            return `${node.id}|${state.ctxMaxChars}|${roles}`;
        }

        // The tab's shell, rendered synchronously with the rest of the panel so
        // switching to it never shows an empty box. The pack itself lands async.
        function buildContextShellHtml() {
            const chips = CTX_ROLE_ORDER.map(r => {
                const on = state.ctxRoles.size === 0 || state.ctxRoles.has(r);
                const { color, label, tip } = CTX_ROLE[r];
                return `<button class="ctx-chip${on ? ' active' : ''}" data-role="${r}" title="${escapeHtml(tip)}">
                    <span class="chip-dot" style="background:${color};color:${color}"></span>
                    <span>${escapeHtml(label)}</span>
                </button>`;
            }).join('');
            return `<div class="ctx-controls">
                    <div class="ctx-budget">
                        <label class="ctx-budget-label has-tip" for="ctx-budget"
                            title="The character budget the pack is fitted to — the same --max-chars ug context takes. Roles fill in priority order, so shrinking this sheds docs first, then dependencies; what did not fit is reported, never silently dropped.">Budget</label>
                        <input type="range" id="ctx-budget" min="500" max="40000" step="500" value="${state.ctxMaxChars}">
                        <output class="ctx-budget-out" id="ctx-budget-out">${state.ctxMaxChars.toLocaleString()}</output>
                    </div>
                    <div class="filter-chips ctx-chips" id="ctx-chips" title="Which roles to ask for — the tool's --include. None selected means every role.">${chips}</div>
                    <div class="ctx-actions">
                        <button class="ctx-act" id="ctx-paint" title="Colour this pack's members on the graph by their role, and push everything else back">Paint</button>
                        <button class="ctx-act" id="ctx-frame" title="Fly the camera to fit the whole pack">Frame</button>
                        <button class="ctx-act" id="ctx-copy" title="Copy the pack exactly as an agent receives it — rendered by the server, not reformatted here">Copy pack</button>
                    </div>
                </div>
                <div class="ctx-status" id="ctx-status"></div>
                <div class="ctx-body" id="ctx-body"></div>`;
        }

        // Called on every tab click and once per panel build. Fetches when the
        // Context tab is the active one, and tears the paint down when it is not.
        function syncContextTab(node) {
            if (state.infoTab === 'context') { renderContextPack(node); return; }
            // Guarded on there actually being a pack: this runs once per panel
            // build, and handleClick already ends in a bumpGraphStyles(). An
            // unconditional restyle here would double the cost of every node
            // selection — a whole-graph pass, on a graph that can hold 485k
            // nodes — to undo a paint that was not there.
            if (!state.ctxPack) return;
            clearContextPaint();
            bumpGraphStyles();
        }

        // Open the tab for whatever is selected. The palette entry, the `c` key
        // and the node menu's Context button all land here.
        function openContextTab() {
            const d = state.selectedNode;
            if (!d) {
                // Nothing selected is a real answer, not an error — say it
                // where the user is looking rather than doing nothing.
                const s = document.getElementById('ctx-status');
                if (s) { s.className = 'ctx-status error'; s.textContent = 'Select a node first.'; }
                return;
            }
            state.infoTab = 'context';
            handleClick(null, d);
        }

        function clearContextPaint() {
            state.ctxPack = null;
        }

        // ── The request ──────────────────────────────────────────────────────

        async function renderContextPack(node) {
            const body = document.getElementById('ctx-body');
            const status = document.getElementById('ctx-status');
            if (!body || !node) return;

            const key = contextKeyFor(node);
            if (state.ctxKey === key && state.ctxEl === body) {
                // Same question, same box, answer still in it. Repaint anyway:
                // leaving the tab cleared the pack, and coming back should put
                // it on the canvas without a second full-graph scan.
                paintContextPack();
                return;
            }
            state.ctxKey = key;
            state.ctxEl = body;

            const token = (state.ctxToken = (state.ctxToken || 0) + 1);
            status.className = 'ctx-status';
            status.textContent = 'Assembling…';
            body.innerHTML = '';
            // Drop the previous pack before asking for the next one. On a large
            // graph this call takes seconds, and leaving the old one painted
            // would light up the *previous* node's neighbourhood beside the new
            // node's panel — a canvas making a confident claim about the wrong
            // symbol, which is worse than one making none.
            if (state.ctxPack) { clearContextPaint(); bumpGraphStyles(); }

            const payload = { nodeId: node.id, maxChars: state.ctxMaxChars };
            if (state.ctxRoles.size) payload.include = [...state.ctxRoles];

            let data;
            try {
                const res = await fetch('/api/tools/context', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload),
                });
                if (!res.ok) throw new Error(await readErr(res));
                data = await res.json();
            } catch (err) {
                // Release the key so re-opening the tab retries rather than
                // inheriting the failure.
                state.ctxKey = null;
                state.ctxEl = null;
                if (state.ctxToken !== token) return;
                status.className = 'ctx-status error';
                status.textContent = `Couldn't build the context pack: ${err.message || err}`;
                return;
            }

            // A slower answer for a node we have since moved off must not
            // overwrite the current one.
            if (state.ctxToken !== token) return;
            if (!state.selectedNode || state.selectedNode.id !== node.id) return;

            // The tool answers 200 with an `error` string for an unresolvable
            // symbol — a bad reference is not a transport failure, and the
            // message names what to do about it, so show it as written.
            if (data.error) {
                state.ctxKey = null;
                state.ctxEl = null;
                status.className = 'ctx-status error';
                status.textContent = data.error;
                return;
            }

            state.ctxData = data;
            status.className = 'ctx-status';
            status.innerHTML = contextSummaryHtml(data);
            body.innerHTML = contextBodyHtml(data);
            wireContextRows(body);
            paintContextPack();
        }

        // ── Rendering ────────────────────────────────────────────────────────

        // The budget line, the per-role tally, and what did not fit. This is
        // the pack's honesty: an agent is told the same three things.
        function contextSummaryHtml(data) {
            const counts = new Map();
            (data.items || []).forEach(i => counts.set(i.role, (counts.get(i.role) || 0) + 1));
            const tally = CTX_ROLE_ORDER
                .filter(r => r !== 'target' && counts.get(r))
                .map(r => `<span class="ctx-tally" style="color:${CTX_ROLE[r].color}">${counts.get(r)} ${escapeHtml(CTX_ROLE[r].label)}</span>`)
                .join('');
            const used = data.used_chars || 0;
            const max = data.max_chars || state.ctxMaxChars;
            const pct = Math.max(0, Math.min(100, Math.round((used / max) * 100)));
            // Over budget is possible by design on very tight packs — the
            // renderer's fixed overhead is charged as an estimate. Say so
            // rather than clamping the bar and implying it fitted.
            const over = used > max;
            const dropped = (data.dropped || [])
                .map(d => `${d.count} ${escapeHtml(d.role)}`)
                .join(', ');
            const notes = (data.notes || []).map(n => `<div class="ctx-note">${escapeHtml(n)}</div>`).join('');
            return `<div class="ctx-meter" title="${used.toLocaleString()} of ${max.toLocaleString()} characters, the pack's rendering overhead included. A budget, not a guarantee: a very tight pack can run a couple of hundred characters over.">
                    <div class="ctx-meter-bar"><span style="width:${pct}%"${over ? ' class="over"' : ''}></span></div>
                    <span class="ctx-meter-txt${over ? ' over' : ''}">${used.toLocaleString()} / ${max.toLocaleString()} chars</span>
                </div>
                <div class="ctx-tallies">${tally}</div>
                ${dropped ? `<div class="ctx-dropped" title="Left out because the budget ran out or the per-role cap was hit. Raise the budget, or narrow the roles, and ask again.">not shown: ${dropped}</div>` : ''}
                ${notes}`;
        }

        function contextBodyHtml(data) {
            const items = data.items || [];
            let html = '';
            CTX_ROLE_ORDER.forEach(role => {
                const group = items.filter(i => i.role === role);
                if (!group.length) return;
                const { color, label, tip } = CTX_ROLE[role];
                const heading = role === 'target' ? 'target' : `${label}s (${group.length})`;
                html += `<div class="ctx-section" data-role="${role}">
                    <div class="ctx-head has-tip" title="${escapeHtml(tip)}">
                        <span class="chip-dot" style="background:${color};color:${color}"></span>
                        <span class="ctx-head-label">${escapeHtml(heading)}</span>
                    </div>
                    ${group.map(contextItemHtml).join('')}
                </div>`;
            });
            return html || '<div class="hier-empty">Nothing in the pack for these roles.</div>';
        }

        function contextItemHtml(item) {
            const loc = item.file
                ? `${item.file}${item.start_line ? ':' + item.start_line : ''}${item.end_line && item.end_line !== item.start_line ? '-' + item.end_line : ''}`
                : '';
            // `why` is the specific relationship the tool recorded — "this
            // —Calls→ target", "tested at 2 hops". It is the sentence the role
            // chip abbreviates, so it is shown, not just tooltipped.
            const why = item.why ? `<div class="ctx-why">${escapeHtml(item.why)}</div>` : '';
            // Call sites are the evidence that makes a caller actionable: a
            // name says something depends on this, a line says how.
            const sites = (item.call_sites || []).map(cs =>
                `<div class="ctx-site"><span class="ctx-site-line">${cs.line}</span><code>${escapeHtml(cs.text)}</code></div>`
            ).join('');
            const code = item.code
                ? `<pre class="ctx-code">${escapeHtml(item.code)}</pre>` : '';
            const cut = item.truncated_chars
                ? `<div class="ctx-cut" title="Trimmed to fit the budget. Raise it, or open the Source tab, for the rest.">${item.truncated_chars.toLocaleString()} chars trimmed</div>` : '';
            const doc = item.doc ? `<div class="ctx-doc">${escapeHtml(item.doc)}</div>` : '';
            return `<div class="ctx-item" data-id="${escapeHtml(item.id)}" title="${escapeHtml(item.id)} — click to select it">
                    <div class="ctx-item-head">
                        ${nodeIconSvg(item.node_type)}
                        <span class="name">${escapeHtml(truncateName(item.name))}</span>
                        ${loc ? `<span class="ctx-loc">${escapeHtml(loc)}</span>` : ''}
                    </div>
                    ${why}${doc}${sites}${code}${cut}
                </div>`;
        }

        // Every row navigates, the same way the Related list and chat citations
        // do — handleClick to select, focusNode to move the camera.
        function wireContextRows(scope) {
            scope.querySelectorAll('.ctx-item').forEach(row => {
                row.addEventListener('click', () => {
                    const t = state.nodeById && state.nodeById.get(row.dataset.id);
                    if (!t) return;
                    handleClick(null, t);
                    focusNode(t);
                });
            });
        }

        // ── The canvas ───────────────────────────────────────────────────────

        // Publish the pack as an id→role map for the style accessors in
        // 10-render-core.js. Built here rather than read out of `items` on
        // every style call: nodeColorFor runs once per node per restyle, and a
        // linear scan of the pack inside it would be O(nodes × pack).
        function paintContextPack() {
            const data = state.ctxData;
            if (!data || !data.items) return;
            const roleById = new Map();
            if (data.target) roleById.set(data.target.id, 'target');
            data.items.forEach(i => { if (i.id) roleById.set(i.id, i.role); });
            state.ctxPack = { targetId: data.target ? data.target.id : null, roleById };
            bumpGraphStyles();
        }

        function wireContextPanel() {
            // Delegated: the panel's innards are rebuilt on every selection, so
            // per-element listeners would have to be re-bound each time. The
            // info body outlives them all.
            const body = document.getElementById('info-body');
            if (!body) return;

            // Both the slider and the chips change the *question*, so both go
            // through the same refetch. Debounced, because a slider drag fires
            // continuously and each answer is a full graph scan.
            const refetch = debounceTrailing(() => {
                if (!state.selectedNode || state.infoTab !== 'context') return;
                renderContextPack(state.selectedNode);
            }, 260);

            body.addEventListener('input', (e) => {
                if (e.target.id !== 'ctx-budget') return;
                state.ctxMaxChars = clampInt(e.target.value, 500, 40000, 12000);
                const out = document.getElementById('ctx-budget-out');
                if (out) out.textContent = state.ctxMaxChars.toLocaleString();
                refetch();
            });

            body.addEventListener('click', (e) => {
                const chip = e.target.closest && e.target.closest('.ctx-chip');
                if (chip) {
                    const role = chip.dataset.role;
                    // The chips open as "all on" with an empty filter set, so
                    // the first click has to mean "only this one" rather than
                    // "everything except this one" — which is what toggling a
                    // member of an empty set would otherwise produce.
                    if (state.ctxRoles.size === 0) state.ctxRoles = new Set([role]);
                    else if (state.ctxRoles.has(role)) state.ctxRoles.delete(role);
                    else state.ctxRoles.add(role);
                    // Back to every role, which is what an empty set means to
                    // the tool as well.
                    if (state.ctxRoles.size === CTX_ROLE_ORDER.length) state.ctxRoles.clear();
                    document.querySelectorAll('.ctx-chip').forEach(c => {
                        c.classList.toggle('active', state.ctxRoles.size === 0 || state.ctxRoles.has(c.dataset.role));
                    });
                    refetch();
                    return;
                }
                const act = e.target.closest && e.target.closest('.ctx-act');
                if (!act) return;
                if (act.id === 'ctx-paint') {
                    state.ctxPaint = !state.ctxPaint;
                    act.classList.toggle('off', !state.ctxPaint);
                    bumpGraphStyles();
                } else if (act.id === 'ctx-frame') {
                    if (state.ctxPack) frameNodeSet([...state.ctxPack.roleById.keys()]);
                } else if (act.id === 'ctx-copy') {
                    copyContextPack(act);
                }
            });
        }

        // The server renders it. `render_context` is the one formatter for this
        // pack across the CLI, MCP and here — writing a second one in JS would
        // give the user a lookalike of what their agent sees rather than the
        // thing itself, and the two would drift on the first format change.
        async function copyContextPack(btn) {
            const node = state.selectedNode;
            if (!node) return;
            const label = btn.textContent;
            btn.textContent = 'Rendering…';
            try {
                const payload = { nodeId: node.id, maxChars: state.ctxMaxChars, render: 'markdown' };
                if (state.ctxRoles.size) payload.include = [...state.ctxRoles];
                const res = await fetch('/api/tools/context', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload),
                });
                if (!res.ok) throw new Error(await readErr(res));
                const data = await res.json();
                await navigator.clipboard.writeText(data.text || '');
                btn.classList.add('copied');
                btn.textContent = 'Copied';
            } catch (err) {
                btn.textContent = 'Copy failed';
                const s = document.getElementById('ctx-status');
                if (s) { s.className = 'ctx-status error'; s.textContent = `Couldn't render the pack: ${err.message || err}`; }
            }
            setTimeout(() => { btn.classList.remove('copied'); btn.textContent = label; }, 1600);
        }
