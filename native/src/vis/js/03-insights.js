        // ─── Insights (analyze in the browser) ──────────

        // Worked examples for the GQL console. Each one exists to teach a
        // different capability rather than to be a useful query in itself —
        // the fastest way to learn what the language can express is to run
        // something that already works and edit it.
        const INS_EXAMPLES = [
            {
                label: 'group + having',
                gql: "MATCH (n:Function)\nWHERE n.code_lines > 40 AND n.is_test = 0\nWITH n.folder AS folder, count(*) AS fns, avg(n.code_lines) AS avg_lines\nWHERE fns >= 3\nRETURN folder, fns, avg_lines\nORDER BY fns DESC"
            },
            {
                label: 'buckets (CASE)',
                gql: "MATCH (n:Function)\nRETURN CASE\n         WHEN n.code_lines > 200 THEN 'e. 200+'\n         WHEN n.code_lines > 100 THEN 'd. 101-200'\n         WHEN n.code_lines > 50  THEN 'c. 51-100'\n         WHEN n.code_lines > 20  THEN 'b. 21-50'\n         ELSE 'a. 0-20'\n       END AS bucket,\n       count(*) AS functions\nORDER BY bucket ASC"
            },
            {
                label: 'ratio (sum ÷ count)',
                gql: "// Booleans are stored as 0/1 so they can be summed —\n// this is how you ask for a fraction.\nMATCH (n)\nWHERE n.node_type IN ['Function', 'Class']\nRETURN n.node_type AS kind,\n       count(*) AS total,\n       sum(n.has_comments) AS commented,\n       sum(n.has_doc) AS with_doc_comment\nORDER BY total DESC"
            },
            {
                label: 'blast radius',
                gql: "// Variable-length paths need a finite bound, and\n// count(DISTINCT …) — a plain count() counts PATHS, not\n// dependents, which over-reports by an order of magnitude.\nMATCH (dep)-[:Calls|References|Imports*1..3]->(t)\nWHERE t.file = 'native/src/storage/store.rs'\n  AND dep.file <> t.file\nRETURN dep.file AS file,\n       count(DISTINCT elementKey(dep)) AS dependents\nORDER BY dependents DESC"
            },
            {
                label: 'nothing reaches it',
                gql: "// An EXISTS subquery needs its own RETURN clause inside.\nMATCH (n:Function)\nWHERE n.is_test = 0 AND n.in_degree > 0\n  AND NOT EXISTS {\n        MATCH (t)-[:Calls|References*1..2]->(n)\n        WHERE t.is_test = 1 RETURN t\n      }\nRETURN elementKey(n) AS id, n.in_degree AS depended_on_by\nORDER BY depended_on_by DESC"
            },
            {
                label: 'edges between folders',
                gql: "MATCH (a)-[:Calls|References|Imports]->(b)\nWHERE a.folder <> b.folder\nRETURN a.folder AS from_folder, b.folder AS to_folder, count(*) AS edges\nORDER BY edges DESC"
            },
            {
                label: 'layer violation',
                gql: "MATCH (a)-[:Calls|References|Imports]->(b)\nWHERE a.folder STARTS WITH 'native/src/mcp'\n  AND b.folder STARTS WITH 'native/src/storage'\nRETURN a.file AS from_file, b.file AS to_file, count(*) AS edges\nORDER BY edges DESC"
            },
            {
                label: 'distribution',
                gql: "// A collect() column comes back summarised as\n// p50 / p90 / p99 rather than as thousands of numbers.\nMATCH (n:Function)\nWHERE n.code_lines IS NOT NULL AND n.is_test = 0\nRETURN n.language AS language, collect(n.code_lines) AS spread"
            },
        ];

        const insState = {
            presets: [],
            properties: [],
            category: null,
            active: null,      // the preset being configured / shown
            lastRun: null,     // { preset|gql, args } so the pager can re-run
            from: 1,
            pageSize: 20,
        };

        const insEl = id => document.getElementById(id);
        const insEsc = s => String(s ?? '').replace(/[&<>"]/g,
            c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

        async function loadInsights() {
            if (insState.presets.length) return;
            try {
                const res = await fetch('/api/presets', { cache: 'no-store' });
                const data = await res.json();
                insState.presets = data.presets || [];
                insState.properties = data.properties || [];
            } catch (e) {
                insEl('ins-presets').innerHTML =
                    `<div class="ins-empty">Could not load presets: ${insEsc(e.message)}</div>`;
                return;
            }
            renderInsCategories();
            renderInsPresets();
        }

        function renderInsCategories() {
            const cats = [...new Set(insState.presets.map(p => p.category))];
            const bar = insEl('ins-cats');
            bar.innerHTML = '';
            const mk = (label, value) => {
                const b = document.createElement('button');
                b.type = 'button';
                b.className = 'ins-cat' + (insState.category === value ? ' active' : '');
                b.textContent = label;
                b.addEventListener('click', () => {
                    insState.category = insState.category === value ? null : value;
                    renderInsCategories();
                    renderInsPresets();
                });
                bar.appendChild(b);
            };
            mk(`all ${insState.presets.length}`, null);
            cats.forEach(c => mk(c, c));
        }

        function renderInsPresets() {
            const q = (insEl('ins-filter').value || '').trim().toLowerCase();
            const list = insState.presets.filter(p => {
                if (insState.category && p.category !== insState.category) return false;
                if (!q) return true;
                return (p.name + ' ' + p.description + ' ' + p.category).toLowerCase().includes(q);
            });

            const box = insEl('ins-presets');
            box.innerHTML = '';
            if (!list.length) {
                box.innerHTML = '<div class="ins-empty">No question matches that filter.</div>';
                return;
            }
            list.forEach(p => {
                const required = (p.params || []).filter(a => a.required);
                const b = document.createElement('button');
                b.type = 'button';
                b.className = 'ins-preset' + (insState.active === p.name ? ' active' : '');
                b.innerHTML =
                    `<span class="n">${insEsc(p.name)}</span>` +
                    `<span class="d">${insEsc(p.description)}</span>` +
                    (required.length
                        ? `<span class="needs">needs ${required.map(a => insEsc(a.name)).join(', ')}</span>`
                        : '');
                b.addEventListener('click', () => choosePreset(p));
                box.appendChild(b);
            });
        }

        // Selecting a preset runs it immediately unless it needs an argument
        // no one has supplied yet — in that case the form appears and the
        // run waits, rather than firing a query that is going to fail.
        function choosePreset(p) {
            insState.active = p.name;
            renderInsPresets();

            const params = p.params || [];
            const section = insEl('ins-args-section');
            const box = insEl('ins-args');
            box.innerHTML = '';
            section.hidden = params.length === 0;

            params.forEach(a => {
                const wrap = document.createElement('div');
                wrap.className = 'ins-arg';
                wrap.innerHTML =
                    `<label for="ins-arg-${insEsc(a.name)}">${insEsc(a.name)}` +
                    (a.required ? ' *' : '') + `</label>` +
                    `<input id="ins-arg-${insEsc(a.name)}" data-arg="${insEsc(a.name)}"` +
                    ` placeholder="${insEsc(a.required ? 'required' : 'default')}">` +
                    `<div class="hint">${insEsc(a.description)}</div>`;
                box.appendChild(wrap);
            });

            if (params.length) {
                const run = document.createElement('button');
                run.type = 'button';
                run.className = 'ins-run';
                run.textContent = 'Run';
                run.addEventListener('click', () => runInsPreset(p));
                box.appendChild(run);
                box.querySelectorAll('input').forEach(i => {
                    i.addEventListener('keydown', ev => {
                        if (ev.key === 'Enter') { ev.preventDefault(); runInsPreset(p); }
                    });
                });
            }

            const missing = params.some(a => a.required);
            if (missing) {
                const first = box.querySelector('input');
                if (first) first.focus();
            } else {
                runInsPreset(p);
            }
        }

        function runInsPreset(p) {
            const args = {};
            document.querySelectorAll('#ins-args input[data-arg]').forEach(i => {
                if (i.value.trim()) args[i.dataset.arg] = i.value.trim();
            });
            const missing = (p.params || []).filter(a => a.required && !args[a.name]);
            if (missing.length) {
                showInsResult(p.name, { error: `${p.name} needs ${missing.map(m => m.name).join(', ')}.` });
                return;
            }
            insState.from = 1;
            insState.lastRun = { preset: p.name, args, label: p.name };
            executeInsQuery(true);
        }

        function runInsGql() {
            const gql = insEl('ins-gql').value.trim();
            if (!gql) return;
            insState.active = null;
            renderInsPresets();
            insEl('ins-args-section').hidden = true;
            insState.from = 1;
            insState.lastRun = { gql, label: 'Custom query' };
            executeInsQuery(true);
        }

        async function executeInsQuery(scroll) {
            const run = insState.lastRun;
            if (!run) return;
            const section = insEl('ins-result-section');
            section.hidden = false;
            // A fresh run (not a page change) brings the result to the top so
            // the answer is visible without scrolling — the preset list above
            // can be long, and a first-time user shouldn't have to hunt.
            if (scroll) {
                section.classList.remove('collapsed');
                section.scrollIntoView({ behavior: 'smooth', block: 'start' });
            }
            insEl('ins-result-label').textContent = run.label;
            insEl('ins-result').innerHTML = '<div class="ins-empty">Running…</div>';

            const to = insState.from + insState.pageSize - 1;
            const body = {
                range: `${insState.from}-${to}`,
                ...(run.preset ? { preset: run.preset, args: run.args } : { gql: run.gql }),
            };
            try {
                const res = await fetch('/api/tools/analyze', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(body),
                });
                const data = await res.json();
                showInsResult(run.label, res.ok ? data : { error: data.error || `HTTP ${res.status}` });
            } catch (e) {
                showInsResult(run.label, { error: e.message });
            }
        }

        function showInsResult(label, data) {
            const box = insEl('ins-result');
            insEl('ins-result-section').hidden = false;
            insEl('ins-result-label').textContent = label;

            if (data.error) {
                box.innerHTML = `<div class="ins-err">${insEsc(data.error)}</div>`;
                return;
            }

            const cols = data.columns || [];
            const rows = data.rows || [];
            box.innerHTML = '';

            if (!rows.length) {
                box.innerHTML = '<div class="ins-empty">No rows matched.</div>';
            } else {
                const idCol = cols.indexOf('id');
                const numeric = cols.map((_, i) => rows.every(r => typeof r[i] === 'number'));

                const wrap = document.createElement('div');
                wrap.className = 'ins-table-wrap';
                const table = document.createElement('table');
                table.className = 'ins-table';
                table.innerHTML = '<thead><tr>' +
                    cols.map((c, i) => `<th${numeric[i] ? ' style="text-align:right"' : ''}>${insEsc(c)}</th>`).join('') +
                    '</tr></thead>';
                const tbody = document.createElement('tbody');

                rows.forEach(r => {
                    const tr = document.createElement('tr');
                    tr.innerHTML = cols.map((_, i) => {
                        const v = r[i];
                        const cls = [numeric[i] ? 'num' : '', i === idCol ? 'id' : ''].filter(Boolean).join(' ');
                        const text = v === null || v === undefined ? '—'
                            : typeof v === 'number' ? (Number.isInteger(v) ? v : v.toFixed(1))
                            : Array.isArray(v) ? `${v.length} values`
                            : String(v);
                        return `<td class="${cls}" title="${insEsc(text)}">${insEsc(text)}</td>`;
                    }).join('');
                    // The payoff for putting this in a graph viewer at all:
                    // a row is a place, not just a number.
                    if (idCol >= 0 && typeof r[idCol] === 'string') {
                        tr.classList.add('clickable');
                        tr.addEventListener('click', () => insFocusOne(r[idCol]));
                    }
                    tbody.appendChild(tr);
                });
                table.appendChild(tbody);
                wrap.appendChild(table);
                box.appendChild(wrap);
            }

            // Pager. Three numbers, and they mean different things:
            // `from`/`to` is the window, `rowsTotal` is how many rows the
            // result has, `rowsMatched` is how many graph elements matched
            // before grouping collapsed them. Showing only one would
            // mislead about which.
            const total = data.rowsTotal ?? rows.length;
            const shownFrom = rows.length ? (data.from ?? insState.from) : 0;
            const shownTo = rows.length ? (data.to ?? insState.from + rows.length - 1) : 0;
            const pager = document.createElement('div');
            pager.className = 'ins-pager';
            const prev = document.createElement('button');
            prev.type = 'button';
            prev.textContent = '← prev';
            prev.disabled = insState.from <= 1;
            prev.addEventListener('click', () => {
                insState.from = Math.max(1, insState.from - insState.pageSize);
                executeInsQuery();
            });
            const next = document.createElement('button');
            next.type = 'button';
            next.textContent = 'next →';
            next.disabled = shownTo >= total;
            next.addEventListener('click', () => {
                insState.from += insState.pageSize;
                executeInsQuery();
            });
            const range = document.createElement('span');
            range.className = 'range';
            range.textContent = rows.length
                ? `rows ${shownFrom}–${shownTo} of ${total}` +
                  (data.rowsMatched > total ? ` · ${data.rowsMatched} graph matches` : '')
                : `nothing at row ${insState.from} — this result has ${total} row(s)`;
            pager.append(prev, next, range);

            const ids = (data.rows || [])
                .map(r => r[(data.columns || []).indexOf('id')])
                .filter(v => typeof v === 'string');
            if (ids.length > 1) {
                const all = document.createElement('button');
                all.type = 'button';
                all.textContent = `light up ${ids.length} in graph`;
                all.addEventListener('click', () => lightUpNodes(ids));
                pager.appendChild(all);
            }
            box.appendChild(pager);

            // Coverage is part of the answer, not a footnote: a statistic over
            // a property most nodes lack is a confidently wrong number, and
            // nothing else on screen would reveal it.
            if ((data.unindexed || []).length) {
                const w = document.createElement('div');
                w.className = 'ins-warn';
                w.innerHTML = `<strong>Not indexed:</strong> ${data.unindexed.map(insEsc).join(', ')} — ` +
                    `no node carries ${data.unindexed.length === 1 ? 'this property' : 'these properties'}, ` +
                    `so every predicate on ${data.unindexed.length === 1 ? 'it' : 'them'} matched nothing. ` +
                    `This result is not about what you asked. Run <code>ug gen</code>.`;
                box.appendChild(w);
            }
            if (data.truncated) {
                const w = document.createElement('div');
                w.className = 'ins-warn';
                w.textContent = 'The row cap was reached — this result is a lower bound, not a total.';
                box.appendChild(w);
            }
            const populated = (data.coverage || []).filter(c => c.present > 0);
            if (populated.length) {
                const m = document.createElement('div');
                m.className = 'ins-meta';
                m.textContent = 'coverage: ' + populated.map(c =>
                    c.present === c.total
                        ? `${c.property} ${c.present}/${c.total}`
                        : `${c.property} ${c.present}/${c.total} (${Math.round(100 * c.present / c.total)}%)`
                ).join(' · ');
                box.appendChild(m);
            }
        }

        // One result row → the node it names.
        function insFocusOne(id) {
            const node = state.nodeById && state.nodeById.get(id);
            if (!node) return;
            exitFocus();
            handleClick(null, node);
            focusNode(node);
        }

        // "What can I query?" — the properties, with how many nodes actually
        // carry each. Computed by the same coverage probe the MCP capability
        // manifest uses, so the two can never disagree.
        async function showInsSchema() {
            const box = insEl('ins-schema-box');
            if (!box.hidden) { box.hidden = true; return; }
            box.hidden = false;
            box.innerHTML = 'Reading the index…';
            const props = insState.properties;
            if (!props.length) { box.textContent = 'Property list unavailable.'; return; }
            try {
                const res = await fetch('/api/tools/analyze', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        gql: 'MATCH (n) RETURN ' + props.map(p => `n.${p}`).join(', '),
                        limit: 1,
                    }),
                });
                const data = await res.json();
                if (!res.ok) { box.innerHTML = `<span class="absent">${insEsc(data.error || 'failed')}</span>`; return; }
                const cov = data.coverage || [];
                box.innerHTML =
                    '<div style="margin-bottom:6px">Properties you can filter and aggregate on, ' +
                    'with how many nodes carry each:</div>' +
                    cov.map(c => c.present === 0
                        ? `<div><b>${insEsc(c.property)}</b> — <span class="absent">not indexed</span></div>`
                        : `<div><b>${insEsc(c.property)}</b> ${c.present}/${c.total}</div>`
                    ).join('');
            } catch (e) {
                box.innerHTML = `<span class="absent">${insEsc(e.message)}</span>`;
            }
        }

        function wireInsights() {
            const filter = insEl('ins-filter');
            if (!filter) return;
            filter.addEventListener('input', renderInsPresets);

            // The GQL console lives at the bottom of the pane; this surfaces it
            // from the opening blurb so writing a custom query is one click,
            // not a scroll-and-hunt.
            const jump = insEl('ins-jump-gql');
            if (jump) jump.addEventListener('click', () => {
                const gqlSection = insEl('ins-gql-section');
                if (gqlSection) {
                    gqlSection.classList.remove('collapsed');
                    gqlSection.scrollIntoView({ behavior: 'smooth', block: 'start' });
                }
                const gql = insEl('ins-gql');
                if (gql) gql.focus();
            });

            const ex = insEl('ins-examples');
            INS_EXAMPLES.forEach(e => {
                const b = document.createElement('button');
                b.type = 'button';
                b.className = 'ins-example';
                b.textContent = e.label;
                b.addEventListener('click', () => {
                    insEl('ins-gql').value = e.gql;
                    insEl('ins-gql').focus();
                });
                ex.appendChild(b);
            });

            insEl('ins-gql-run').addEventListener('click', runInsGql);
            insEl('ins-gql').addEventListener('keydown', ev => {
                // Ctrl/Cmd+Enter runs, plain Enter keeps writing — a query is
                // several lines more often than it is one.
                if ((ev.metaKey || ev.ctrlKey) && ev.key === 'Enter') { ev.preventDefault(); runInsGql(); }
            });
            insEl('ins-schema').addEventListener('click', showInsSchema);
        }


        function initialize() {
            const container = document.getElementById('container');
            width = container.clientWidth;
            height = container.clientHeight;

            const nodeCount = state.graph.nodes.length;
            // Not `state.graph.edges.length` — in server mode that array is
            // empty by design and the real count came down with the index.
            const edgeCount = state.edgeCount || 0;
            state.nodeById = new Map(state.graph.nodes.map(n => [n.id, n]));
            buildAdjacency();

            // Past the threshold the whole graph is never drawn. `state.graph`
            // still holds every node — search, filters, stats and presence
            // checks read it — but the renderer is handed `state.view`, which
            // starts empty and only ever holds one neighbourhood at a time.
            // Below the threshold, in local mode, the two are the same object.
            state.soloOnly = soloRequired();
            state.view = state.soloOnly ? { nodes: [], edges: [] } : state.graph;
            document.body.classList.toggle('solo-only', state.soloOnly);

            document.getElementById('graph-title').textContent =
                `${nodeCount} nodes, ${edgeCount} edges` +
                (state.soloOnly ? ' · solo mode' : '');

            buildNodeFilterChips();
            buildEdgeFilterChips();
            buildLegend();
            renderIndexStats();
            renderStartHere();
            createGraph();
            wireNav();
            wireViewbar();

            // The data is loaded but the diagram isn't drawn yet: the force
            // layout + first WebGL paint happen over the next frames. Keep the
            // loading overlay up (it's released by the renderer on its first
            // painted frame) so there's no blank canvas with no feedback.
            const lp = document.getElementById('load-phase');
            if (lp) lp.textContent = 'Rendering graph…';
            const prog = document.getElementById('load-progress');
            if (prog) prog.classList.add('indeterminate');
            const fill = document.getElementById('load-progress-bar');
            if (fill) fill.style.width = '';

            document.getElementById('sidebar').classList.remove('pending');
            document.getElementById('sidebar-launcher').classList.remove('pending');
            wireSidebarSections();
            wireSearch();
            wireToggle();
            wireSidebarResize();
            wireInfoResize();
            wireToolTabs();
            wireFilterActions();
            wireSemanticPanel();
            wireChatPanel();
            wireTourPanel();
            wireFindPathBtn();
            wireInsights();
            wireWalk();
            wireDiscoverSubtabs();
            wirePanelTabs();
            wireCatalog();
            wireIngestButtons();
            probeCapabilities();
            startHealthPolling();
            // Solo mode opens on an empty canvas, so it needs something there
            // to explain itself and to give the user a first move.
            if (state.soloOnly) { setupSoloEmptyState(); updateSoloHud(); }

            document.getElementById('info-close').addEventListener('click', () => {
                document.getElementById('info').classList.remove('visible');
                exitPathMode();
                state.selectedNode = null;
                exitFocus();
                bumpGraphStyles();
            });
        }

