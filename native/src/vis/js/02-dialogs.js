        // ─── Delete confirmation dialog ──────────────────────────
        // Custom dialog (not window.confirm) so the destructive-action
        // framing, stats, and in-dialog error/busy states match the rest
        // of the KB Manager instead of dropping to a native browser
        // prompt. `kbConfirmProject` holds whichever project the dialog
        // currently targets; the Cancel/Delete buttons are wired once in
        // `wireKbManager()` and just act on whatever's pending.
        let kbConfirmProject = null;

        function openDeleteConfirm(p) {
            kbConfirmProject = p;
            document.getElementById('kb-confirm-name').textContent = p.name;
            const stats = `${(p.nodes || 0).toLocaleString()} node${p.nodes === 1 ? '' : 's'} and ${(p.edges || 0).toLocaleString()} edge${p.edges === 1 ? '' : 's'}`;
            document.getElementById('kb-confirm-body').innerHTML =
                `This permanently deletes <b>${stats}</b> of indexed data` +
                (p.repoRoot ? ` for <code>${escapeHtml(p.repoRoot)}</code>` : '') +
                `. Your source files are untouched, but this cannot be undone.`;
            const errEl = document.getElementById('kb-confirm-error');
            errEl.hidden = true;
            errEl.textContent = '';
            setConfirmBusy(false);
            document.getElementById('kb-confirm-overlay').classList.add('visible');
            document.getElementById('kb-confirm-cancel').focus();
        }

        function closeDeleteConfirm() {
            document.getElementById('kb-confirm-overlay').classList.remove('visible');
            kbConfirmProject = null;
        }

        function setConfirmBusy(busy) {
            document.getElementById('kb-confirm-cancel').disabled = busy;
            document.getElementById('kb-confirm-delete-btn').disabled = busy;
            document.getElementById('kb-confirm-delete-label').textContent = busy ? 'Deleting…' : 'Delete forever';
        }

        // Delete the project currently shown in the confirm dialog via
        // POST /api/projects/delete (server-side mirror of `ug rm`). If
        // it was the active project, the server has already switched
        // active to another project (or the empty placeholder) — resync
        // via a full reload when the 3D scene is already initialized,
        // same as any other project switch.
        async function confirmDeleteActive() {
            if (!kbConfirmProject) return;
            const { name } = kbConfirmProject;
            const wasActive = kbCapsCache && kbCapsCache.active === name;
            setConfirmBusy(true);
            try {
                const res = await fetch('/api/projects/delete', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ name }),
                });
                if (!res.ok) {
                    const err = await res.json().catch(() => ({}));
                    throw new Error(err.error || `HTTP ${res.status}`);
                }
                closeDeleteConfirm();
                if (wasActive && graphInitialized) {
                    reloadOntoActiveProject();
                    return;
                }
                const listRes = await fetch('/api/projects', { cache: 'no-store' });
                kbCapsCache = await listRes.json();
                showKbManager(kbCapsCache);
            } catch (err) {
                console.error('failed to delete project:', err);
                setConfirmBusy(false);
                const errEl = document.getElementById('kb-confirm-error');
                errEl.textContent = err.message || 'Failed to delete project.';
                errEl.hidden = false;
            }
        }

        // Shared by the sidebar's "browse knowledge bases" icon and
        // clicking the brand/logo — both just reopen the same overlay.
        async function reopenKbManager() {
            if (!isMultiMode) return;
            document.getElementById('kb-manager').classList.add('visible');
            document.getElementById('kb-loading').hidden = false;
            document.getElementById('kb-list-view').hidden = true;
            document.getElementById('kb-wizard-view').hidden = true;
            setKbStatus('Refreshing…');
            try {
                const res = await fetch('/api/projects', { cache: 'no-store' });
                if (!res.ok) throw new Error(`HTTP ${res.status}`);
                kbCapsCache = await res.json();
                showKbManager(kbCapsCache);
            } catch (err) {
                console.error('failed to list projects:', err);
                hideKbManager();
            }
        }

        // Folder-browse dialog for the KB wizard's "Browse…" button — lets
        // the user click through directories server-side (via
        // GET /api/browse-dir) instead of only being able to type an
        // absolute path by hand. `kbBrowsePath` tracks whatever directory
        // is currently listed; "Select this folder" writes it back into
        // #kb-path-input and closes the dialog without touching anything
        // else in the wizard.
        let kbBrowsePath = null;

        function openFolderBrowser() {
            document.getElementById('kb-browse-overlay').classList.add('visible');
            const seed = document.getElementById('kb-path-input').value.trim();
            loadBrowseDir(seed || null);
        }

        function closeFolderBrowser() {
            document.getElementById('kb-browse-overlay').classList.remove('visible');
        }

        async function loadBrowseDir(path) {
            const listEl = document.getElementById('kb-browse-list');
            const pathEl = document.getElementById('kb-browse-path');
            const upBtn = document.getElementById('kb-browse-up');
            listEl.innerHTML = '<div class="kb-browse-loading">Loading…</div>';
            try {
                const qs = path ? `?path=${encodeURIComponent(path)}` : '';
                const res = await fetch(`/api/browse-dir${qs}`, { cache: 'no-store' });
                if (!res.ok) {
                    const err = await res.json().catch(() => ({}));
                    throw new Error(err.error || `HTTP ${res.status}`);
                }
                const data = await res.json();
                kbBrowsePath = data.path;
                pathEl.textContent = data.path;
                pathEl.scrollLeft = pathEl.scrollWidth;
                upBtn.disabled = !data.parent;
                upBtn.onclick = () => loadBrowseDir(data.parent);

                if (!data.entries.length) {
                    listEl.innerHTML = '<div class="kb-browse-empty">No subfolders here.</div>';
                    return;
                }
                listEl.innerHTML = data.entries.map(entry => `
                    <div class="kb-browse-row${entry.isRepo ? ' is-repo' : ''}" data-path="${escapeHtml(entry.path)}">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                            <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" />
                        </svg>
                        <span>${escapeHtml(entry.name)}</span>
                    </div>
                `).join('');
                listEl.querySelectorAll('.kb-browse-row').forEach(row => {
                    row.addEventListener('click', () => loadBrowseDir(row.dataset.path));
                });
            } catch (err) {
                console.error('failed to browse directory:', err);
                listEl.innerHTML = `<div class="kb-browse-empty">${escapeHtml(err.message || 'Failed to list directory.')}</div>`;
            }
        }

        function wireKbManager() {
            document.getElementById('kb-browse-btn').addEventListener('click', openFolderBrowser);
            document.getElementById('kb-browse-cancel').addEventListener('click', closeFolderBrowser);
            document.getElementById('kb-browse-select').addEventListener('click', () => {
                if (kbBrowsePath) document.getElementById('kb-path-input').value = kbBrowsePath;
                closeFolderBrowser();
            });
            document.getElementById('kb-browse-overlay').addEventListener('click', (e) => {
                if (e.target.id === 'kb-browse-overlay') closeFolderBrowser();
            });
            document.addEventListener('keydown', (e) => {
                if (e.key === 'Escape' && document.getElementById('kb-browse-overlay').classList.contains('visible')) {
                    closeFolderBrowser();
                }
            });


            document.getElementById('kb-wizard-back').addEventListener('click', () => {
                if (kbCapsCache) showKbList(kbCapsCache);
            });

            // What's New toggle
            document.getElementById('kb-whatsnew-toggle').addEventListener('click', (e) => {
                e.stopPropagation();
                document.getElementById('kb-whatsnew-body').parentElement.classList.toggle('collapsed');
            });
            document.getElementById('kb-whatsnew-body').parentElement.addEventListener('click', (e) => {
                if (e.target.closest('.kb-whatsnew-header') && !e.target.closest('.kb-whatsnew-toggle')) {
                    document.getElementById('kb-whatsnew-body').parentElement.classList.toggle('collapsed');
                }
            });

            document.getElementById('kb-open-btn').addEventListener('click', reopenKbManager);
            document.getElementById('brand-title').addEventListener('click', reopenKbManager);

            document.getElementById('kb-confirm-cancel').addEventListener('click', closeDeleteConfirm);
            document.getElementById('kb-confirm-delete-btn').addEventListener('click', confirmDeleteActive);
            document.getElementById('kb-confirm-overlay').addEventListener('click', (e) => {
                if (e.target.id === 'kb-confirm-overlay') closeDeleteConfirm();
            });
            document.addEventListener('keydown', (e) => {
                if (e.key === 'Escape' && document.getElementById('kb-confirm-overlay').classList.contains('visible')) {
                    closeDeleteConfirm();
                }
            });

            const form = document.getElementById('kb-wizard-form');
            form.addEventListener('submit', async (e) => {
                e.preventDefault();
                const path = document.getElementById('kb-path-input').value.trim();
                const name = document.getElementById('kb-name-input').value.trim();
                const noIngest = document.getElementById('kb-no-ingest').checked;
                if (!path) return;

                document.getElementById('kb-generate-btn').disabled = true;
                document.getElementById('kb-wizard-back').disabled = true;
                document.getElementById('kb-wizard-error').hidden = true;
                document.getElementById('kb-wizard-status').hidden = false;
                document.getElementById('kb-wizard-status-text').textContent = 'Starting…';
                document.getElementById('kb-wizard-log').textContent = '';

                try {
                    const res = await fetch('/api/generate', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ path, name: name || undefined, no_ingest: noIngest }),
                    });
                    if (!res.ok) {
                        const err = await res.json().catch(() => ({}));
                        throw new Error(err.error || `HTTP ${res.status}`);
                    }
                    const { jobId } = await res.json();
                    await pollGenJob(jobId);
                } catch (err) {
                    document.getElementById('kb-wizard-status').hidden = true;
                    document.getElementById('kb-wizard-error').hidden = false;
                    document.getElementById('kb-wizard-error').textContent = err.message || String(err);
                    document.getElementById('kb-generate-btn').disabled = false;
                    document.getElementById('kb-wizard-back').disabled = false;
                }
            });
        }

        function pollGenJob(jobId) {
            return new Promise((resolve, reject) => {
                const statusText = document.getElementById('kb-wizard-status-text');
                const logEl = document.getElementById('kb-wizard-log');
                const tick = async () => {
                    let job;
                    try {
                        const res = await fetch(`/api/generate/status?job=${encodeURIComponent(jobId)}`, { cache: 'no-store' });
                        job = await res.json();
                    } catch (err) {
                        setTimeout(tick, 1500);
                        return;
                    }
                    logEl.textContent = (job.log || []).join('\n');
                    logEl.scrollTop = logEl.scrollHeight;
                    if (job.status === 'running') {
                        statusText.textContent = 'Generating knowledge base…';
                        setTimeout(tick, 1000);
                    } else if (job.status === 'done') {
                        statusText.textContent = `Done — opening ${job.projectName}…`;
                        setTimeout(async () => {
                            await openProject(job.projectName, null);
                            resolve();
                        }, 500);
                    } else {
                        document.getElementById('kb-generate-btn').disabled = false;
                        document.getElementById('kb-wizard-back').disabled = false;
                        document.getElementById('kb-wizard-status').hidden = true;
                        document.getElementById('kb-wizard-error').hidden = false;
                        document.getElementById('kb-wizard-error').textContent = job.error || 'Generation failed.';
                        reject(new Error(job.error || 'generation failed'));
                    }
                };
                tick();
            });
        }

        function transformData(data) {
            const nodeMap = new Map();
            const cx = window.innerWidth / 2 || 800;
            const cy = window.innerHeight / 2 || 600;

            data.nodes.forEach((n, i) => {
                const angle = (i / data.nodes.length) * Math.PI * 2;
                const radius = 100 + Math.random() * 150;
                nodeMap.set(n.id, {
                    id: n.id,
                    name: n.name || n.id,
                    group: n.node_type || 'Default',
                    file: n.file || null,
                    startLine: n.startLine || n.start_line || null,
                    endLine: n.endLine || n.end_line || null,
                    docstring: n.docstring || null,
                    metrics: n.metrics || null,
                    signature: n.signature || null,
                    imports: n.imports || [],
                    exports: n.exports || [],
                    extends: n.extends || [],
                    implements: n.implements || [],
                    calls: n.calls || [],
                    x: cx + Math.cos(angle) * radius,
                    y: cy + Math.sin(angle) * radius
                });
            });

            const edges = data.edges
                .map(e => ({
                    source: e.source,
                    target: e.target,
                    rel: e.edge_type || e.rel || null
                }))
                .filter(e => e.source && e.target && nodeMap.has(e.source) && nodeMap.has(e.target));

            state.graph = { nodes: Array.from(nodeMap.values()), edges };
            state.containsMaps = null;
            state.stats = data.stats || null;
            state.catalogTree = null;
            state.catalogExpanded = null;
            state.catalogAutoExpanded = false;
        }

        function formatNumber(n) {
            if (n == null || isNaN(n)) return '—';
            if (n >= 1_000_000) return (n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1) + 'M';
            if (n >= 1_000) return (n / 1_000).toFixed(n >= 10_000 ? 0 : 1) + 'k';
            return String(n);
        }

        function formatDuration(ms) {
            if (ms == null || isNaN(ms)) return '—';
            if (ms < 1000) return `${ms} ms`;
            const s = ms / 1000;
            if (s < 60) return `${s.toFixed(s < 10 ? 2 : 1)} s`;
            const m = Math.floor(s / 60);
            const rem = Math.round(s - m * 60);
            return `${m}m ${rem}s`;
        }

        function formatRelativeTime(epochSec) {
            if (!epochSec) return '—';
            const ms = epochSec > 1e12 ? epochSec : epochSec * 1000;
            const diff = Date.now() - ms;
            if (diff < 0 || isNaN(diff)) return new Date(ms).toLocaleString();
            const s = Math.floor(diff / 1000);
            if (s < 60) return `${s}s ago`;
            const m = Math.floor(s / 60);
            if (m < 60) return `${m}m ago`;
            const h = Math.floor(m / 60);
            if (h < 24) return `${h}h ago`;
            const d = Math.floor(h / 24);
            if (d < 30) return `${d}d ago`;
            return new Date(ms).toLocaleDateString();
        }

        function renderIndexStats() {
            const stats = state.stats;
            const filesEl = document.getElementById('stat-files');
            const foldersEl = document.getElementById('stat-folders');
            const symbolsEl = document.getElementById('stat-symbols');
            const linesEl = document.getElementById('stat-lines');
            const metaEl = document.getElementById('index-meta');
            if (!stats) {
                filesEl.textContent = foldersEl.textContent =
                    symbolsEl.textContent = linesEl.textContent = '—';
                metaEl.innerHTML = '<div class="edge-breakdown-row"><span class="name">No index stats in graph.json</span></div>';
                return;
            }
            filesEl.textContent = formatNumber(stats.totalFiles);
            foldersEl.textContent = formatNumber(stats.totalFolders);
            symbolsEl.textContent = formatNumber(stats.totalSymbols);
            linesEl.textContent = formatNumber(stats.totalLines);

            const cached = stats.cachedFiles ?? 0;
            const total = stats.totalFiles ?? 0;
            const pct = total > 0 ? Math.round((cached / total) * 100) : 0;
            const rows = [
                { label: 'Cached', value: total > 0 ? `${cached} / ${total} (${pct}%)` : '—' },
                { label: 'Indexed in', value: formatDuration(stats.indexingTimeMs) },
                { label: 'Last run', value: formatRelativeTime(stats.lastIndexedAt) },
                { label: 'Repo', value: stats.repoRoot || '—' }
            ];
            metaEl.innerHTML = rows.map(r => `
                <div class="edge-breakdown-row">
                    <span class="name">${escapeHtml(r.label)}</span>
                    <span class="count" title="${escapeHtml(String(r.value))}">${escapeHtml(String(r.value))}</span>
                </div>
            `).join('');
        }

        function getContainsMaps() {
            if (state.containsMaps) return state.containsMaps;
            const childrenOf = new Map();
            const parentOf = new Map();
            state.graph.edges.forEach(e => {
                if (e.rel !== 'Contains') return;
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (!childrenOf.has(sId)) childrenOf.set(sId, []);
                childrenOf.get(sId).push(tId);
                if (!parentOf.has(tId)) parentOf.set(tId, []);
                parentOf.get(tId).push(sId);
            });
            state.containsMaps = { childrenOf, parentOf };
            return state.containsMaps;
        }

        function getContainsCounts(nodeId) {
            const { childrenOf, parentOf } = getContainsMaps();
            const directChildren = (childrenOf.get(nodeId) || []).length;
            const parents = parentOf.get(nodeId) || [];
            const siblingSet = new Set();
            parents.forEach(p => {
                (childrenOf.get(p) || []).forEach(c => {
                    if (c !== nodeId) siblingSet.add(c);
                });
            });
            return { directChildren, siblings: siblingSet.size, parents: parents.length };
        }

        // ─── Node type icons ────────────────────────────────
        //
        // A coloured dot says "these two differ"; an icon says how. Same
        // glyph everywhere a node type appears — legend, panel header,
        // related lists — so the shape becomes readable shorthand.
        const NODE_ICONS = {
            // ƒ — a function
            Function: '<path d="M8 20c0-9 .8-16 4.2-16 1.3 0 2 .6 2.4 1.3"/><path d="M6.5 10.5h7"/>',
            // braces — a class body
            Class: '<path d="M9.5 3.5C7 3.5 7.5 8 5.5 9.6c-.6.5-.9.7-1.5.9.6.2.9.4 1.5.9C7.5 13 7 17.5 9.5 17.5"/>'
                + '<path d="M14.5 3.5C17 3.5 16.5 8 18.5 9.6c.6.5.9.7 1.5.9-.6.2-.9.4-1.5.9-2 1.6-1.5 6.1-4 6.1"/>',
            // dashed diamond — a contract, not an implementation
            Interface: '<path d="M12 3.2 20.8 12 12 20.8 3.2 12z" stroke-dasharray="3.2 2.6"/>',
            // locked value
            Constant: '<rect x="5" y="10.5" width="14" height="9" rx="2"/><path d="M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5"/>',
            // page with a folded corner
            File: '<path d="M14 3.5H7.5A1.5 1.5 0 0 0 6 5v14a1.5 1.5 0 0 0 1.5 1.5h9A1.5 1.5 0 0 0 18 19V7.5z"/><path d="M14 3.5V7a.5.5 0 0 0 .5.5H18"/>',
            Folder: '<path d="M3.5 7.5A1.5 1.5 0 0 1 5 6h4l2 2.5h8a1.5 1.5 0 0 1 1.5 1.5v7.5A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z"/>',
            // sliders — configuration
            Config: '<path d="M5 8h8M17 8h2M5 16h2M11 16h8"/><circle cx="15" cy="8" r="2"/><circle cx="9" cy="16" r="2"/>',
            // an inbound arrow — a way in from outside the system
            Route: '<path d="M3 12h13"/><path d="M12 7.5 16.5 12 12 16.5"/><path d="M18.5 5.5v13"/>',
            // an idea, not a symbol
            Concept: '<path d="M9.5 18.5h5"/><path d="M10 21h4"/><path d="M12 3a6 6 0 0 1 3.4 10.9c-.6.4-.9 1-.9 1.6H9.5c0-.6-.3-1.2-.9-1.6A6 6 0 0 1 12 3z"/>',
        };

        // Inline SVG for a node type, tinted with that type's colour.
        function nodeIconSvg(group, cls) {
            const body = NODE_ICONS[group] || '<circle cx="12" cy="12" r="6.5"/>';
            const color = config.getColor(group);
            return `<svg class="node-icon${cls ? ' ' + cls : ''}" viewBox="0 0 24 24" fill="none"`
                + ` stroke="${color}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"`
                + ` aria-hidden="true">${body}</svg>`;
        }

        // What every field in the node panel means, in one place.
        //
        // Each entry is `[label, tip]`. The tip answers the three questions a
        // field can't answer by itself: where the value came from, what reads
        // it, and how it relates to the other fields. They are wordier than a
        // typical tooltip on purpose — the panel shows four different data
        // sources side by side, and the cost of guessing wrong is
        // misreading a search result.
        //
        // Keyed rather than inlined so the label and its explanation cannot
        // drift apart, and so renaming a field is a one-line change.
        const FIELD_DOCS = {
            id: ['Node id', 'The canonical key for this node, shaped <kind>:<file>:<name> '
                + '(with a #N suffix when a file declares the same name more than once). This one '
                + 'value is also the chunk id — the storage key of the embedded row — because ug '
                + 'has no separate chunk id: a node is a chunk. Pass it to ug get_code, find_usages '
                + 'or traverse.'],
            name: ['Name', 'The symbol or file name the indexer extracted. Names arrive already '
                + 'qualified where the language allows it (Db::upsert_nodes, not upsert_nodes). '
                + 'This is also the first thing in the embedded text, so it is searchable both '
                + 'exactly and as separate words.'],
            type: ['Node type', 'What kind of thing this is in the graph. Decided by the indexer '
                + 'from the symbol kind — Concept means a document section or page rather than '
                + 'code. Node type sets the colour in the graph and can be filtered on in search.'],
            file: ['File', 'Path the node was indexed from, relative to the repo root. Shared by '
                + 'every node declared in that file, and the key the Source tab reads from disk.'],
            lines: ['Line range', 'Where this node lives in its file. For PDF and Office pages '
                + 'this is the page number instead — those formats have no lines. The Source tab '
                + 'reads exactly this range; the range stored with the vector may differ if the '
                + 'file changed since indexing.'],
            metrics: ['Metrics', 'Measured by the indexer when the symbol was parsed. LOC is '
                + 'lines of code, Params the parameter count, Nest the deepest block nesting. '
                + 'Not part of the embedding — these describe shape, not meaning.'],
            signature: ['Signature', 'Parameters and return type as the parser saw them. Folded '
                + 'into the embedded text, so a query naming a parameter type can reach this '
                + 'node even when it has no doc comment.'],
            docstring: ['Docstring', 'The doc comment the indexer extracted — /** */, """ """, '
                + '/// or, for a document, the prose under the heading. This is the main body of '
                + 'the embedded text: for a code symbol it is the description, and for a '
                + 'document section it is essentially the whole node.'],
            calls: ['Calls out to', 'Function and method names invoked inside this node\'s body, '
                + 'as the parser saw them. Deliberately kept out of the embedded text — the '
                + 'Related names list already covers the neighbours, and call lists churn on '
                + 'every body edit. Names that resolve to an indexed node are clickable.'],
            extends: ['Extends', 'Base class or parent type. Becomes an Extends edge in the '
                + 'graph, so it is walkable in the Related tab and carries weight in '
                + 'graph-aware ranking.'],
            implements: ['Implements', 'Interfaces or traits this node implements. Becomes an '
                + 'Implements edge, walkable in the Related tab.'],
        };

        // What each edge type means. Edge type is not decoration — it carries
        // the weight graph-aware ranking walks with, so "Calls" and "Contains"
        // pull results with very different strength.
        const EDGE_DOCS = {
            Calls: 'A call site found in the body. The strongest signal in graph ranking.',
            Imports: 'A module or file dependency declared at the top of the file.',
            Contains: 'Structural containment — a folder, file or enclosing symbol.',
            Extends: 'Class or type inheritance.',
            Implements: 'An interface or trait implementation.',
            References: 'A mention that is neither a call nor an import — for a document, a link.',
            Exports: 'A re-export of this symbol.',
            DependsOn: 'A declared package dependency.',
        };

        // Present tense, subject-first, so the tooltip reads as a sentence
        // rather than as a schema name.
        function edgeVerb(rel) {
            switch (rel) {
                case 'Calls': return 'calls';
                case 'Imports': return 'imports';
                case 'Contains': return 'contains';
                case 'Extends': return 'extends';
                case 'Implements': return 'implements';
                case 'References': return 'references';
                case 'Exports': return 'exports';
                case 'DependsOn': return 'depends on';
                default: return 'links to';
            }
        }

        // Passive voice for an inbound edge — the inverse of [`edgeVerb`].
        // Used by the Related tab so the chip reads "contained by" vs
        // "contains" rather than a bare "Contains" + arrow, which leaves the
        // direction of a symmetric-looking label ambiguous.
        const EDGE_PASSIVE = {
            Calls: 'called by',
            Imports: 'imported by',
            Contains: 'contained by',
            Extends: 'extended by',
            Implements: 'implemented by',
            References: 'referenced by',
            Exports: 'exported by',
            DependsOn: 'depended on by',
        };
        function edgeDirLabel(rel, dir) {
            const r = rel || '';
            return dir === 'in'
                ? (EDGE_PASSIVE[r] || r.toLowerCase())
                : edgeVerb(rel);
        }

        // What a cap's stage means in practice: the cost of changing it.
        const STAGE_DOCS = {
            index: 'Applied while reading files, so changing it needs a full re-index.',
            embed: 'Applied while building the text that gets embedded, so changing it needs a '
                + 're-embed — ug gen picks that up on its own.',
            retrieve: 'Applied when answering a query, so changing it takes effect on the next '
                + 'search with no re-index at all.',
        };

        const TAB_DOCS = {
            preview: ['Source', 'The file as it is on disk right now, read live. This is the '
                + 'only tab that does not come from the index, so it is where you see changes '
                + 'made since the last ug gen.'],
            chunk: ['Indexed', 'What the knowledge base actually stores for this node: the text '
                + 'that was embedded, the source captured alongside it, and the caps that shaped '
                + 'both. The honest answer to "why did search return this?".'],
            hierarchy: ['Hierarchy', 'Containment only — the folder and file this node sits in, '
                + 'and the symbols declared inside it. Follows Contains edges, ignoring calls '
                + 'and imports.'],
            related: ['Related', 'Every edge touching this node in either direction — calls, '
                + 'imports, extends, implements, references. This is the neighbourhood that '
                + 'graph-aware search expands into when ranking results.'],
        };

        // Every panel section says where its data came from. The panel mixes
        // three very different sources — the graph file, the vector store and
        // the working tree — and "which of these am I looking at?" is a fair
        // question to be able to answer at a glance.
        function sourceNote(view, node) {
            const dbReady = !!(state.capabilities && state.capabilities.db_ready);
            const loc = node && node.file
                ? `${node.file}${node.startLine ? ':' + node.startLine : ''}`
                : '';
            const notes = {
                fields: {
                    label: dbReady ? 'graph.json + vector store' : 'graph.json',
                    detail: dbReady
                        ? 'What the indexer recorded about this node, from the loaded graph file, '
                          + 'hydrated with the stored row via /api/db/node. Describes the node as '
                          + 'it was indexed — compare with the Source tab to see what has changed '
                          + 'on disk since.'
                        : 'What the indexer recorded about this node, from the loaded graph file. '
                          + 'No knowledge base is attached, so store-only fields are unavailable.',
                },
                preview: {
                    label: 'working tree — live',
                    detail: loc
                        ? `Read from ${loc} via /api/file: the file as it is on disk right now. `
                          + 'This is the only tab not served from the index, so it is where edits '
                          + 'made since the last ug gen show up.'
                        : 'Read live from disk via /api/file — the file as it is now, not as indexed.',
                },
                chunk: {
                    label: 'vector store — as indexed',
                    detail: 'Everything the knowledge base holds for this node, from /api/db/node: '
                        + 'the text that was embedded (what semantic search matches against), the '
                        + 'source captured beside it (what snippet reads return), and the caps that '
                        + 'shaped both.',
                },
                hierarchy: {
                    label: 'graph.json · Contains edges',
                    detail: 'Containment only: the folder and file this node sits in, and the '
                        + 'symbols declared inside it. Calls and imports are in the Related tab.',
                },
                related: {
                    label: 'graph.json · all edges',
                    detail: 'Every edge touching this node in either direction — calls, imports, '
                        + 'extends, implements, references. This is the neighbourhood graph-aware '
                        + 'search expands into when ranking.',
                },
            };
            const n = notes[view];
            if (!n) return '';
            return `<div class="src-note" title="${escapeHtml(n.detail)}">`
                + `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                     <ellipse cx="12" cy="6" rx="8" ry="3"/><path d="M4 6v6c0 1.7 3.6 3 8 3s8-1.3 8-3V6"/>
                     <path d="M4 12v6c0 1.7 3.6 3 8 3s8-1.3 8-3v-6"/>
                   </svg>`
                + `<span class="src-label">${escapeHtml(n.label)}</span>`
                + `<span class="src-detail">${escapeHtml(n.detail)}</span></div>`;
        }

        // ─── Hierarchy tab ──────────────────────────────────
        // The selected node's containment context: ancestors up to two levels,
        // every sibling under its direct parent, and its own descendants down
        // to two levels. Long generations are windowed around the selection so
        // a file among hundreds of siblings stays readable.
        const HIER_SIBLING_CAP = 40;
        const HIER_CHILD_CAP = 40;
        const HIER_GRANDCHILD_CAP = 8;

        function buildHierarchyHtml(d) {
            const { childrenOf, parentOf } = getContainsMaps();
            const get = id => state.nodeById && state.nodeById.get(id);

            const sortNodes = ids => ids
                .map(get).filter(Boolean)
                .sort((a, b) => ((a.startLine ?? Infinity) - (b.startLine ?? Infinity))
                    || String(a.name).localeCompare(String(b.name)));

            const rowHtml = (n, depth, isSelected) => {
                const kids = (childrenOf.get(n.id) || []).length;
                const lines = n.startLine
                    ? `L${n.startLine}${n.endLine && n.endLine !== n.startLine ? '–' + n.endLine : ''}` : '';
                return `<div class="hier-row${isSelected ? ' selected' : ''}${kids ? ' has-children' : ''}"
                    data-id="${escapeHtml(n.id)}" style="padding-left:${8 + depth * 14}px">
                    ${nodeIconSvg(n.group)}
                    <span class="name" title="${escapeHtml(n.name)}">${escapeHtml(truncateName(n.name))}</span>
                    <span class="kind">${escapeHtml(n.group || '')}</span>
                    <span class="lines">${lines}</span>
                </div>`;
            };
            const moreHtml = (count, depth) =>
                `<div class="hier-more" style="padding-left:${8 + depth * 14}px">… ${count} more</div>`;

            // Ancestor chain, nearest last: [grandparent?, parent?]. Contains
            // is effectively a tree, so we follow the first parent.
            const chain = [];
            let cur = d.id;
            for (let i = 0; i < 2; i++) {
                const ps = parentOf.get(cur) || [];
                const p = ps.length ? get(ps[0]) : null;
                if (!p) break;
                chain.unshift(p);
                cur = p.id;
            }

            let html = '';
            let depth = 0;
            for (const a of chain) html += rowHtml(a, depth++, false);

            // The selected node's generation: all siblings under its direct
            // parent (just the node itself when it has no parent).
            const parent = chain[chain.length - 1];
            let generation = parent ? sortNodes(childrenOf.get(parent.id) || []) : [d];
            if (!generation.some(n => n.id === d.id)) generation = [d, ...generation];

            // Window an oversized generation around the selection.
            let shown = generation, hiddenSiblings = 0;
            if (generation.length > HIER_SIBLING_CAP) {
                const idx = generation.findIndex(n => n.id === d.id);
                const start = Math.max(0, Math.min(idx - HIER_SIBLING_CAP / 2, generation.length - HIER_SIBLING_CAP));
                shown = generation.slice(start, start + HIER_SIBLING_CAP);
                hiddenSiblings = generation.length - shown.length;
            }
            for (const n of shown) {
                const isSel = n.id === d.id;
                html += rowHtml(n, depth, isSel);
                if (!isSel) continue;
                // Descendants of the selected node only, two levels deep.
                const kids = sortNodes(childrenOf.get(n.id) || []);
                for (const k of kids.slice(0, HIER_CHILD_CAP)) {
                    html += rowHtml(k, depth + 1, false);
                    const gks = sortNodes(childrenOf.get(k.id) || []);
                    for (const g of gks.slice(0, HIER_GRANDCHILD_CAP)) html += rowHtml(g, depth + 2, false);
                    if (gks.length > HIER_GRANDCHILD_CAP) html += moreHtml(gks.length - HIER_GRANDCHILD_CAP, depth + 2);
                }
                if (kids.length > HIER_CHILD_CAP) html += moreHtml(kids.length - HIER_CHILD_CAP, depth + 1);
            }
            if (hiddenSiblings) html += moreHtml(hiddenSiblings, depth);

            const hasKids = (childrenOf.get(d.id) || []).length > 0;
            if (!chain.length && !hasKids && generation.length === 1) {
                return `<div class="hier-empty">No containment hierarchy for this node.</div>`;
            }
            return `<div class="hier-list">${html}</div>`;
        }

        // Placed above initialize() deliberately. `insState` and
        // INS_EXAMPLES are `const`, so they are in the temporal dead zone
        // until their declaration is evaluated — and initialize() calls
        // wireInsights(). The ordering is safe as written; keeping the
        // declarations ahead of their only caller means it stays safe if
        // initialize() ever moves earlier in module evaluation, where the
        // failure would be a ReferenceError that takes down the whole init
        // sequence and every subtab with it, not just this pane.
