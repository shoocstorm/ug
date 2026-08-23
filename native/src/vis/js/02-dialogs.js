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
                    // The server already switched active away from the deleted
                    // project, so point any post-reload deep link at the new
                    // active one rather than the one that just vanished.
                    try {
                        const listRes = await fetch('/api/projects', { cache: 'no-store' });
                        kbCapsCache = await listRes.json();
                        state.activeProject = kbCapsCache && kbCapsCache.active ? kbCapsCache.active : null;
                    } catch (e) { /* keep whatever activeProject already was */ }
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
                // Embedding is opt-in, exactly as on the CLI: the default run
                // writes structure and no vectors, and "Ingest now" backfills.
                const withEmbed = document.getElementById('kb-with-embed').checked;
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
                        body: JSON.stringify({ path, name: name || undefined, no_ingest: noIngest, with_embed: withEmbed }),
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
                    // Named `isBoundary`, not `boundary`: `state.showBoundary`
                    // is already the dashed bounding-box overlay, and two
                    // unrelated "boundary" concepts one word apart would be
                    // read as one by the next person here.
                    isBoundary: !!(n.boundaries && n.boundaries.length),
                    boundaries: n.boundaries || [],
                    x: cx + Math.cos(angle) * radius,
                    y: cy + Math.sin(angle) * radius
                });
            });

            // Endpoints are carried over as the *node's own* id string rather
            // than the one JSON.parse made for this edge. graph.json spells
            // every endpoint out in full, so a large repo arrives with a fresh
            // string per endpoint: 1.5M of them on a 746k-edge graph, where
            // only 162k distinct ids exist. Pointing at the node's copy leaves
            // the duplicates collectable along with the rest of the parsed
            // payload instead of pinning ~130 MB for the life of the tab.
            // The lookup is the same one the old `.has()` filter did.
            const edges = [];
            for (const e of data.edges) {
                const s = nodeMap.get(e.source);
                const t = nodeMap.get(e.target);
                if (!s || !t) continue;
                edges.push({ source: s.id, target: t.id, rel: e.edge_type || e.rel || null });
            }

            state.graph = { nodes: Array.from(nodeMap.values()), edges };
            state.stats = data.stats || null;
            // The repo-root folder node (shallowest depth) carries the
            // per-language file counts the indexer computed — the same fact
            // the CLI's overview prints as `rust×69, markdown×23, …`.
            state.languages = null;
            let rootDepth = Infinity;
            data.nodes.forEach(n => {
                const f = n.folder;
                if (!f || typeof f.depth !== 'number' || f.depth >= rootDepth) return;
                if (f.languageBreakdown && Object.keys(f.languageBreakdown).length) {
                    rootDepth = f.depth;
                    state.languages = f.languageBreakdown;
                }
            });
            state.catalogTree = null;
            state.catalogExpanded = null;
            state.catalogAutoExpanded = false;
            state.graphMode = 'local';
            // Null in local mode, and that is what every `if (state.nodeStore)`
            // branch keys off: the columns only exist when the server sent them.
            state.nodeStore = null;
            state.nodeCount = state.graph.nodes.length;
            state.edgeCount = edges.length;
            // Counted once here rather than four times over in the chips, the
            // legend, `presentNodeTypes` and `syncLegend` — server mode gets the
            // same two facts off the wire, so both modes answer from one place.
            state.nodeTypeCounts = {};
            state.boundaryCount = 0;
            for (const n of state.graph.nodes) {
                state.nodeTypeCounts[n.group] = (state.nodeTypeCounts[n.group] || 0) + 1;
                if (n.isBoundary) state.boundaryCount++;
            }
            // Degree, for the "start here" strip and the solo empty state's top
            // hubs. Both used to compute this themselves — one by scanning every
            // edge, one by ranking the whole adjacency map — for a top-5 list.
            state.degreeOf = new Map();
            for (const e of edges) {
                state.degreeOf.set(e.source, (state.degreeOf.get(e.source) || 0) + 1);
                if (e.target !== e.source) {
                    state.degreeOf.set(e.target, (state.degreeOf.get(e.target) || 0) + 1);
                }
            }
            state.edgeTypeCounts = {};
            for (const e of edges) {
                const r = e.rel || 'other';
                state.edgeTypeCounts[r] = (state.edgeTypeCounts[r] || 0) + 1;
            }
            state.catalogRootIds = null;   // local mode derives these from Contains
        }

        // ─── Server mode: the node index ───────────────────
        //
        // In server mode the page is handed every node's *identity* and nothing
        // else; edges, docstrings, metrics and boundaries arrive per
        // neighbourhood, on demand. On a 500k-node graph that index is the
        // largest thing the tab ever holds, so how it is held is the whole
        // memory story — see §Round 2 of docs/dev/PERF-TUNING-JOURNEY.md.
        //
        // What it used to be: 500k node objects, each with six distinct empty
        // arrays as placeholders, plus three 500k-entry Maps keyed by the same
        // 141-character id strings. Measured 379 MB peak, 331 MB retained.
        //
        // What it is now: typed-array columns viewed straight over the response
        // buffer, front-coded id/name blobs that never become JS strings, and
        // node objects materialised only for what the user actually touches.

        // One frozen array shared by every unhydrated node's imports / exports /
        // extends / implements / calls / boundaries.
        //
        // At 500k nodes the old six-arrays-per-node cost 3 million array objects
        // whose only job was to be empty. Sharing is safe because `hydrateNodes`
        // *assigns* these slots rather than mutating them — nothing anywhere
        // pushes into a node's list — and freezing turns a future mistake about
        // that into a thrown error rather than a graph where every node suddenly
        // imports the same thing.
        const EMPTY_LIST = Object.freeze([]);

        // FNV-1a, 32-bit, over the UTF-8 bytes. Must stay byte-identical to
        // `fnv1a32` in serve.rs — `nidx_hash_matches_the_client` pins them
        // together, because a divergence is not an error, it is a lookup table
        // that quietly answers "no such node" for every id.
        const NIDX_ENCODER = typeof TextEncoder !== 'undefined' ? new TextEncoder() : null;
        function fnv1a32(str) {
            // ASCII fast path. For a string with no code unit above 127 the
            // UTF-8 bytes *are* the char codes, so this is the same hash — and
            // it avoids `TextEncoder.encode`, which allocates a fresh
            // `Uint8Array` per call. That allocation was the single most
            // expensive thing in an id lookup: 11 µs against ~1 µs, on a path
            // the solo view walks once per node it puts on the canvas.
            //
            // The first non-ASCII code unit bails to the encoder and starts
            // over. Node ids are file paths and symbol names, so that is rare
            // — and it has to be correct rather than close, which is what
            // `the_client_decodes_every_id_this_encoder_writes` checks with a
            // deliberately non-ASCII id.
            let h = 2166136261;
            for (let i = 0; i < str.length; i++) {
                const c = str.charCodeAt(i);
                if (c > 127) return fnv1a32Utf8(str);
                h ^= c;
                h = Math.imul(h, 16777619);
            }
            return h >>> 0;
        }

        function fnv1a32Utf8(str) {
            let h = 2166136261;
            if (NIDX_ENCODER) {
                const bytes = NIDX_ENCODER.encode(str);
                for (let i = 0; i < bytes.length; i++) {
                    h ^= bytes[i];
                    h = Math.imul(h, 16777619);
                }
                return h >>> 0;
            }
            // No TextEncoder at all (no browser this ships to, but a harness
            // might): encode the code points by hand rather than hash the
            // wrong bytes and silently lose every non-ASCII id.
            for (const ch of str) {
                let cp = ch.codePointAt(0);
                const buf = cp < 0x80 ? [cp]
                    : cp < 0x800 ? [0xc0 | (cp >> 6), 0x80 | (cp & 0x3f)]
                    : cp < 0x10000 ? [0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f)]
                    : [0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f)];
                for (const byte of buf) {
                    h ^= byte;
                    h = Math.imul(h, 16777619);
                }
            }
            return h >>> 0;
        }

        // A string column that is already an array of JS strings — the JSON
        // index's shape, and the fallback when the binary frame is unavailable.
        function arrayStringColumn(arr) {
            return { at: i => (arr[i] == null ? '' : arr[i]), length: arr.length };
        }

        // A front-coded string column: record `i` is `blob[off[i]..off[i+1]]`,
        // one byte of shared-prefix length followed by the suffix, restarting
        // every `block` entries. See `front_code` in serve.rs.
        //
        // The point is that the bytes stay bytes: 500k ids cost ~26 MB of
        // ArrayBuffer here against ~80 MB of individually-allocated JS strings,
        // and none of it is on the JS heap where the GC has to walk it.
        function frontCodedColumn(blob, off, block) {
            const decoder = new TextDecoder('utf-8');
            const n = off.length - 1;
            // One scratch buffer, grown on demand. Reconstruction is at most
            // `block` records, so this never sees a surprise size.
            let scratch = new Uint8Array(256);
            // Grows by *copying* — the bytes already in it are the shared
            // prefix the next record builds on, so a bare reallocation silently
            // returns an id with its head cut off. Which is not an error
            // anywhere: it is a lookup that answers "no such node".
            const ensure = (need) => {
                if (scratch.length < need) {
                    let cap = scratch.length;
                    while (cap < need) cap *= 2;
                    const grown = new Uint8Array(cap);
                    grown.set(scratch);
                    scratch = grown;
                }
            };
            return {
                length: n,
                at(i) {
                    if (i < 0 || i >= n) return '';
                    const first = i - (i % block);
                    let len = 0;
                    for (let k = first; k <= i; k++) {
                        const s = off[k];
                        const e = off[k + 1];
                        const shared = blob[s];
                        const suffix = e - s - 1;
                        ensure(shared + suffix);
                        // `shared` bytes of what we already built stay put;
                        // everything after them comes from this record.
                        scratch.set(blob.subarray(s + 1, e), shared);
                        len = shared + suffix;
                    }
                    return decoder.decode(scratch.subarray(0, len));
                },
            };
        }

        // How many node objects may sit in the pool before the ones nobody is
        // looking at are dropped. Generous on purpose: a node costs ~130 bytes,
        // so this is ~6 MB, and the alternative — evicting something the
        // renderer is holding — loses its position and breaks identity.
        const NODE_POOL_SOFT_CAP = 50000;

        // The `id → index` memo. Bounded for the same reason and cleared
        // wholesale rather than evicted one at a time: it is a pure cache of a
        // pure function, so throwing all of it away costs one re-probe.
        const NODE_INDEX_CACHE_CAP = 100000;

        // Everything `state.nodeById` has ever been asked for, over the columns.
        //
        // This object *is* `state.nodeById`. That is deliberate and it is what
        // kept this change to 24 files instead of 60: the Map surface
        // (`get`/`has`/`size`/`keys`/`values`/`entries`/`forEach`) is
        // implemented here, so every call site that looks a node up by id —
        // the tour's `stop.node_id`, chat citations, the URL state, the
        // catalog, the renderers — is untouched and does not know the nodes it
        // gets back were built a microsecond ago.
        function makeNodeStore(ix, prebuiltSlots) {
            const n = ix.n;
            const byIndex = new Map();   // index → node object
            const byId = new Map();      // node.id → the same object
            const idxCache = new Map();  // id → index (or -1), memoised

            // Open addressing, linear probing, at ≤50% load. `slots` holds node
            // indices; `idHash` is the per-node hash the server sent, so
            // building this is pure integer work — no id is ever decoded here.
            //
            // The worker builds it when there is one (see
            // `fetchNodeIndexInWorker`), which is the last piece of the load
            // that would otherwise block the main thread. The capacity check is
            // not paranoia: a table sized for a different `n` would probe past
            // its own entries and answer "no such node" for real ids.
            let cap = 1024;
            while (cap < n * 2) cap *= 2;
            const mask = cap - 1;
            const idHash = ix.idHash;
            let slots;
            if (prebuiltSlots && prebuiltSlots.length === cap) {
                slots = prebuiltSlots;
            } else {
                slots = new Int32Array(cap).fill(-1);
                for (let i = 0; i < n; i++) {
                    let k = idHash[i] & mask;
                    while (slots[k] !== -1) k = (k + 1) & mask;
                    slots[k] = i;
                }
            }

            const cx = (typeof window !== 'undefined' && window.innerWidth / 2) || 800;
            const cy = (typeof window !== 'undefined' && window.innerHeight / 2) || 600;

            function indexOf(id) {
                if (typeof id !== 'string') return -1;
                const memo = idxCache.get(id);
                if (memo !== undefined) return memo;
                const h = fnv1a32(id);
                let k = h & mask;
                let found = -1;
                while (slots[k] !== -1) {
                    const cand = slots[k];
                    // The hash check is what keeps this cheap: a full id is
                    // decoded only on a hash collision, which at 32 bits over
                    // 500k entries is a handful of nodes in the whole graph.
                    if (idHash[cand] === h && ix.idCol.at(cand) === id) { found = cand; break; }
                    k = (k + 1) & mask;
                }
                // Misses are memoised too. `findNodeByName` and the catalog ask
                // about strings that are not ids at all, repeatedly.
                if (idxCache.size >= NODE_INDEX_CACHE_CAP) idxCache.clear();
                idxCache.set(id, found);
                return found;
            }

            function nodeAt(i) {
                if (i < 0 || i >= n) return undefined;
                const held = byIndex.get(i);
                if (held) return held;
                if (byIndex.size >= NODE_POOL_SOFT_CAP) trimPool();
                const id = ix.idCol.at(i);
                const fi = ix.fileIdx[i];
                const angle = (i / n) * Math.PI * 2;
                const radius = 100 + Math.random() * 150;
                // Property order is fixed and matches `transformData`'s node
                // literal — do not reorder, it is what keeps the hidden class
                // shared with locally-built nodes.
                const node = {
                    id,
                    name: ix.nameCol.at(i) || id,
                    group: ix.typeNames[ix.typeIdx[i]] || 'Default',
                    file: fi >= 0 ? ix.fileNames[fi] : null,
                    startLine: ix.startLine[i] || null,
                    endLine: ix.endLine[i] || null,
                    docstring: null,
                    metrics: null,
                    signature: null,
                    imports: EMPTY_LIST,
                    exports: EMPTY_LIST,
                    extends: EMPTY_LIST,
                    implements: EMPTY_LIST,
                    calls: EMPTY_LIST,
                    isBoundary: !!ix.boundary[i],
                    boundaries: EMPTY_LIST,
                    x: cx + Math.cos(angle) * radius,
                    y: cy + Math.sin(angle) * radius,
                    // Everything above `id`/`name`/`group`/`file`/lines/boundary
                    // is a placeholder until `hydrateNodes` fills it from the
                    // server. `_slim` says which is which, so a panel can tell
                    // "no docstring" from "not fetched yet".
                    _slim: true,
                    // Its position in the columns. Saves the round trip back
                    // through the hash table everywhere the server protocol
                    // wants an index — hydrate, the edge fetches, the catalog.
                    _i: i,
                };
                byIndex.set(i, node);
                byId.set(id, node);
                return node;
            }

            // Drop what nothing is looking at. A node currently on the canvas
            // keeps its object: the renderers mutate `x`/`y` on it and match
            // highlight sets by object identity, so re-materialising one mid-view
            // would reset its position and silently break highlighting. A
            // hydrated node is kept too — throwing it away would re-fetch it.
            function trimPool() {
                const keep = state.viewIds instanceof Set ? state.viewIds : null;
                const selected = state.selectedNode && state.selectedNode.id;
                for (const [i, node] of byIndex) {
                    if (node._slim === false) continue;
                    if (node.id === selected) continue;
                    if (keep && keep.has(node.id)) continue;
                    byIndex.delete(i);
                    byId.delete(node.id);
                }
            }

            const store = {
                // ── the columns, for whoever wants them without an object ──
                columns: ix,
                nodeCount: n,
                indexOf,
                nodeAt,
                // Record an `id → index` a caller already knows, so the next
                // `get`/`has` for it is a map hit rather than a hash probe.
                // `fetchEdges` knows the index of every endpoint it decodes.
                noteIndex(id, i) {
                    if (idxCache.size >= NODE_INDEX_CACHE_CAP) idxCache.clear();
                    idxCache.set(id, i);
                },
                idAt: i => ix.idCol.at(i),
                nameAt: i => ix.nameCol.at(i),
                groupAt: i => ix.typeNames[ix.typeIdx[i]] || 'Default',
                degAt: i => ix.deg[i] || 0,
                isBoundaryAt: i => !!ix.boundary[i],

                // ── the Map surface `state.nodeById` is read through ──
                get(id) {
                    const held = byId.get(id);
                    if (held) return held;
                    const i = indexOf(id);
                    return i >= 0 ? nodeAt(i) : undefined;
                },
                has(id) {
                    return byId.has(id) || indexOf(id) >= 0;
                },
                get size() { return n; },
                // Materialise-as-you-go, so a stray whole-graph iteration is
                // merely slow rather than a lie. Nothing in the app does this —
                // `grep -n 'state.graph.nodes'` is the check — and anything that
                // starts to should use the columns instead.
                *keys() { for (let i = 0; i < n; i++) yield ix.idCol.at(i); },
                *values() { for (let i = 0; i < n; i++) yield nodeAt(i); },
                *entries() { for (let i = 0; i < n; i++) yield [ix.idCol.at(i), nodeAt(i)]; },
                forEach(fn) { for (let i = 0; i < n; i++) fn(nodeAt(i), ix.idCol.at(i), store); },
                [Symbol.iterator]() { return store.entries(); },
            };
            return store;
        }

        // The most-connected nodes, in either mode. Server mode reads the degree
        // column (a `Uint32Array`, no id strings involved); local mode ranks the
        // `degreeOf` map the edge walk filled in.
        //
        // A partial selection rather than a sort: ranking 500k entries to show
        // five was the old shape, and it allocated an array of every node with
        // any edge at all to do it.
        function topByDegree(k) {
            const store = state.nodeStore;
            if (store) {
                const deg = store.columns.deg;
                const n = store.nodeCount;
                const best = [];   // indices, ascending by degree, at most k
                let floor = 0;
                for (let i = 0; i < n; i++) {
                    const d = deg[i];
                    if (d <= floor && best.length >= k) continue;
                    let at = best.length;
                    while (at > 0 && deg[best[at - 1]] < d) at--;
                    best.splice(at, 0, i);
                    if (best.length > k) best.pop();
                    if (best.length >= k) floor = deg[best[best.length - 1]];
                }
                return best.map(i => store.nodeAt(i)).filter(Boolean);
            }
            const degree = state.degreeOf;
            if (!degree || !degree.size) return [];
            return [...degree.entries()]
                .sort((a, b) => b[1] - a[1])
                .slice(0, k)
                .map(([id]) => state.nodeById && state.nodeById.get(id))
                .filter(Boolean);
        }

        // Undirected degree of one node, in either mode. Server mode reads the
        // column through the node's own wire index; local mode reads the map the
        // edge walk built.
        function degreeOfNode(node) {
            if (!node) return 0;
            if (state.nodeStore && typeof node._i === 'number') return state.nodeStore.degAt(node._i);
            return (state.degreeOf && state.degreeOf.get(node.id)) || 0;
        }

        // ─── Name search, in both modes ────────────────────
        //
        // One implementation for the three places that search by name — the
        // sidebar's suggestion box, the command palette and the Graph Walk's
        // seed picker. All three used to hold their own copy of the same
        // `filter().sort().slice()` over every node in the graph.
        //
        // Locally that is a scan, bounded so the sort never sees more than
        // `SEARCH_SCAN_CAP` candidates. In server mode it is a request:
        // `/api/graph/search` has the real strings, and the client has a
        // front-coded name column that would have to be decoded half a million
        // times per keystroke to answer the same question badly.
        //
        // Always a promise, in both modes, so the callers have one shape.

        // How many matches are ranked before the cut. The display shows fifty;
        // "light up … in graph" takes at most SOLO_MAX_NODES. Anything past
        // this cannot reach the screen, and collecting it is what made an empty
        // query allocate an array of every node in the graph.
        const SEARCH_SCAN_CAP = 4000;

        async function searchNodes(query, opts = {}) {
            const q = (query || '').trim().toLowerCase();
            const limit = opts.limit || 50;
            const types = opts.types && opts.types.size ? opts.types : null;
            const boundaryOnly = !!opts.boundary;

            if (state.nodeStore) return searchNodesOnServer(q, limit, types, boundaryOnly);

            const cap = Math.max(limit, SEARCH_SCAN_CAP);
            const scored = [];
            let total = 0;
            for (const n of state.graph.nodes) {
                if (types && !types.has(n.group)) continue;
                if (boundaryOnly && !n.isBoundary) continue;
                let rank = 0;
                if (q) {
                    const ln = n.name.toLowerCase();
                    rank = ln.indexOf(q);
                    if (rank < 0) {
                        if (!n.id.toLowerCase().includes(q)) continue;
                        // Matched on the qualified id rather than the name:
                        // still a hit, but it sorts after every name match.
                        rank = Number.MAX_SAFE_INTEGER;
                    }
                }
                total++;
                // The lowercased name is computed once here, not twice per
                // comparison inside the sort, which is where the old version
                // spent most of a keystroke.
                if (scored.length < cap) scored.push({ n, rank, len: n.name.length });
            }
            if (q) scored.sort((a, b) => (a.rank - b.rank) || (a.len - b.len));
            return {
                nodes: scored.slice(0, limit).map(s => s.n),
                total,
                truncated: total > scored.length,
            };
        }

        async function searchNodesOnServer(q, limit, types, boundaryOnly) {
            if (!q) {
                // Nothing is displayed for an empty query in any of the three
                // callers — only the count, which the index already carries.
                const counts = nodeTypeCountsAll();
                let total = 0;
                if (types) types.forEach(t => { total += counts[t] || 0; });
                else total = state.nodeCount || 0;
                return { nodes: [], total, truncated: false };
            }
            const qs = new URLSearchParams({ q, limit: String(limit) });
            if (types) qs.set('types', [...types].join(','));
            let data;
            try {
                const res = await fetch(`/api/graph/search?${qs}`);
                if (!res.ok) throw new Error(await readErr(res));
                data = await res.json();
            } catch (err) {
                console.warn('node search failed:', err && err.message);
                return { nodes: [], total: 0, truncated: false };
            }
            // Hits come back as whole node rows; what the canvas and the panels
            // need is *the* node object for each id, so they go through the
            // store. A hit already on screen keeps its position and identity.
            const nodes = [];
            for (const row of data.nodes || []) {
                if (boundaryOnly && !(row.boundaries && row.boundaries.length)) continue;
                const node = state.nodeById.get(row.id);
                if (node) nodes.push(node);
            }
            nodes.sort((a, b) => {
                const ai = a.name.toLowerCase().indexOf(q);
                const bi = b.name.toLowerCase().indexOf(q);
                const ar = ai < 0 ? Number.MAX_SAFE_INTEGER : ai;
                const br = bi < 0 ? Number.MAX_SAFE_INTEGER : bi;
                return (ar - br) || (a.name.length - b.name.length);
            });
            return { nodes, total: data.count || nodes.length, truncated: !!data.truncated };
        }

        // Node-type histogram over the *whole* graph.
        //
        // Server mode reads what the index already carried; local mode counts.
        // Four call sites used to recount this from every node on every call —
        // the filter chips, the legend, `presentNodeTypes` and `syncLegend` —
        // which on a large graph is four full passes to draw a row of numbers
        // that cannot have changed.
        function nodeTypeCountsAll() {
            if (state.nodeTypeCounts) return state.nodeTypeCounts;
            const counts = {};
            state.graph.nodes.forEach(n => { counts[n.group] = (counts[n.group] || 0) + 1; });
            state.nodeTypeCounts = counts;
            return counts;
        }

        function boundaryCountAll() {
            if (typeof state.boundaryCount === 'number') return state.boundaryCount;
            state.boundaryCount = state.graph.nodes.filter(n => n.isBoundary).length;
            return state.boundaryCount;
        }

        // Install a decoded index (from either encoding) as the page's graph.
        //
        // `state.graph.nodes` is deliberately left **empty** in server mode.
        // Every whole-graph reader was moved onto the columns or onto the
        // counts above; anything that still reaches for it is a bug, and an
        // empty array makes it show up as a zero rather than as 500k
        // materialised objects.
        function installNodeIndex(ix, prebuiltSlots) {
            const store = makeNodeStore(ix, prebuiltSlots);
            state.graph = { nodes: [], edges: [] };
            state.nodeStore = store;
            state.nodeById = store;
            state.nodeCount = ix.n;
            state.graphMode = 'server';
            state.edgeCount = ix.edgeCount || 0;
            state.stats = ix.stats || null;
            state.languages = ix.languages || null;
            state.edgeTypeCounts = ix.edgeTypeCounts || {};
            state.nodeTypeCounts = ix.nodeTypeCounts || {};
            state.boundaryCount = ix.boundaryCount || 0;
            // Roots are the one place a bounded set of ids is worth decoding up
            // front: the catalog opens on them, and there are thousands, not
            // hundreds of thousands.
            state.catalogRootIds = Array.from(ix.catalogRoots || [], i => ix.idCol.at(i));
            state.degreeOf = null;   // server mode ranks off the degree column
            state.catalogTree = null;
            state.catalogExpanded = null;
            state.catalogAutoExpanded = false;
        }

        // The JSON index (`/api/graph/nodes`), kept as the fallback for a server
        // too old to serve the binary frame — and as the thing the binary frame
        // is tested against.
        function transformSlim(payload) {
            const n = payload.n;
            const boundary = new Uint8Array(n);
            for (const i of payload.boundary || []) boundary[i] = 1;
            const ids = payload.ids || [];
            const idHash = new Uint32Array(n);
            for (let i = 0; i < n; i++) idHash[i] = fnv1a32(ids[i] || '');
            installNodeIndex({
                n,
                edgeCount: payload.edgeCount || 0,
                idCol: arrayStringColumn(ids),
                nameCol: arrayStringColumn(payload.names || []),
                idHash,
                typeNames: payload.types || [],
                typeIdx: Uint8Array.from(payload.typeIdx || []),
                fileNames: payload.files || [],
                fileIdx: Int32Array.from(payload.fileIdx || []),
                startLine: Uint32Array.from(payload.startLine || []),
                endLine: Uint32Array.from(payload.endLine || []),
                deg: Uint32Array.from(payload.deg || []),
                boundary,
                boundaryCount: (payload.boundary || []).length,
                catalogRoots: payload.catalogRoots || [],
                nodeTypeCounts: payload.nodeTypeCounts || {},
                edgeTypeCounts: payload.edgeTypeCounts || {},
                stats: payload.stats || null,
                languages: payload.languages || null,
            });
        }

        // ─── The binary frame (`/api/graph/nodes.bin`) ─────
        //
        // Layout is defined by `build_binary_index` in serve.rs. Decoding is
        // taking views over the buffer that arrived: no copy, no parse, and the
        // id/name bytes are never turned into JS strings.

        const NIDX_MAGIC = 'UGNIDX\0\0';
        const NIDX_KIND = {
            TYPE_IDX: 1, FILE_IDX: 2, START_LINE: 3, END_LINE: 4, DEG: 5,
            BOUNDARY: 6, CATALOG_ROOTS: 7, ID_BLOB: 8, ID_OFF: 9,
            NAME_BLOB: 10, NAME_OFF: 11, ID_HASH: 12, META: 13,
        };

        // Frame → the `ix` shape `installNodeIndex` wants. Throws on anything
        // it does not recognise so `loadGraph` can fall back to the JSON index
        // rather than render a graph decoded from garbage.
        function decodeNodeIndexFrame(buffer) {
            const bytes = new Uint8Array(buffer);
            if (bytes.length < 16) throw new Error('node index frame is truncated');
            for (let i = 0; i < 8; i++) {
                if (bytes[i] !== NIDX_MAGIC.charCodeAt(i)) throw new Error('not a node index frame');
            }
            const head = new DataView(buffer);
            const version = head.getUint32(8, true);
            if (version !== 1) throw new Error(`node index version ${version} is not supported`);
            const count = head.getUint32(12, true);
            const sec = new Map();
            for (let slot = 0; slot < count; slot++) {
                const at = 16 + slot * 12;
                const kind = head.getUint32(at, true);
                const off = head.getUint32(at + 4, true);
                const len = head.getUint32(at + 8, true);
                if (off + len > bytes.length) throw new Error(`node index section ${kind} overruns the frame`);
                sec.set(kind, [off, len]);
            }
            const need = (kind) => {
                const s = sec.get(kind);
                if (!s) throw new Error(`node index is missing section ${kind}`);
                return s;
            };
            const u8 = (kind) => { const [o, l] = need(kind); return bytes.subarray(o, o + l); };
            // Views, not copies — which is why every section is 4-byte aligned
            // on the server side.
            const u32 = (kind) => { const [o, l] = need(kind); return new Uint32Array(buffer, o, l / 4); };
            const i32 = (kind) => { const [o, l] = need(kind); return new Int32Array(buffer, o, l / 4); };

            const meta = JSON.parse(new TextDecoder('utf-8').decode(u8(NIDX_KIND.META)));
            const n = meta.n;
            const block = meta.block || 16;
            return {
                n,
                edgeCount: meta.edgeCount || 0,
                idCol: frontCodedColumn(u8(NIDX_KIND.ID_BLOB), u32(NIDX_KIND.ID_OFF), block),
                nameCol: frontCodedColumn(u8(NIDX_KIND.NAME_BLOB), u32(NIDX_KIND.NAME_OFF), block),
                idHash: u32(NIDX_KIND.ID_HASH),
                typeNames: meta.types || [],
                typeIdx: u8(NIDX_KIND.TYPE_IDX),
                fileNames: meta.files || [],
                fileIdx: i32(NIDX_KIND.FILE_IDX),
                startLine: u32(NIDX_KIND.START_LINE),
                endLine: u32(NIDX_KIND.END_LINE),
                deg: u32(NIDX_KIND.DEG),
                boundary: u8(NIDX_KIND.BOUNDARY),
                boundaryCount: meta.boundaryCount || 0,
                catalogRoots: u32(NIDX_KIND.CATALOG_ROOTS),
                nodeTypeCounts: meta.nodeTypeCounts || {},
                edgeTypeCounts: meta.edgeTypeCounts || {},
                stats: meta.stats || null,
                languages: meta.languages || null,
            };
        }

        function transformSlimBinary(buffer) {
            installNodeIndex(decodeNodeIndexFrame(buffer));
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
                { label: 'Cached', title: 'Files whose content was unchanged since the last index and were reused from cache instead of re-parsed. Re-runs of a warm index show a high number here.', value: total > 0 ? `${cached} / ${total} (${pct}%)` : '—' },
                { label: 'Indexed in', value: formatDuration(stats.indexingTimeMs) },
                { label: 'Last run', value: formatRelativeTime(stats.lastIndexedAt) },
                { label: 'Repo', value: stats.repoRoot || '—' }
            ];
            metaEl.innerHTML = rows.map(r => `
                <div class="edge-breakdown-row">
                    <span class="name" title="${escapeHtml(r.title || '')}">${escapeHtml(r.label)}</span>
                    <span class="count" title="${escapeHtml(String(r.value))}">${escapeHtml(String(r.value))}</span>
                </div>
            `).join('');

            renderLanguages();
        }

        function renderLanguages() {
            const el = document.getElementById('index-languages');
            if (!el) return;
            const label = document.getElementById('languages-label');
            const langs = state.languages;
            const entries = langs
                ? Object.entries(langs).sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(b[0]))
                : [];
            if (!entries.length) {
                el.innerHTML = '<div class="edge-breakdown-row"><span class="name">No language data in graph.json</span></div>';
                if (label) label.textContent = 'Languages';
                return;
            }
            const total = entries.reduce((s, [, c]) => s + c, 0);
            if (label) label.textContent = `Languages (${total} files)`;
            el.innerHTML = entries.map(([name, count]) => `
                <div class="edge-breakdown-row" title="${escapeHtml(name)}">
                    <span class="name">${escapeHtml(name)}</span>
                    <span class="count">${count}</span>
                </div>
            `).join('');
        }

        // The Contains hierarchy, read off the adjacency index one node at a
        // time.
        //
        // This used to be two whole-graph Maps built by scanning every edge —
        // 273,100 Contains edges on a large repo, ~30 MB of Maps, for questions
        // that are always about one node. It was also a *second* index over
        // information `state.adj` already holds, which is one index too many:
        // in server mode there is no local edge list to scan, so the maps came
        // out empty and every hierarchy view silently reported nothing.
        //
        // `known` reads only what the cache already has and never reports a
        // miss — for walking *other* nodes' children (siblings, grandchildren)
        // where incompleteness is expected and self-healing.
        function containsChildrenOf(id, known) {
            const out = [];
            for (const e of (known ? knownEdgesOf(id) : edgesOf(id))) {
                if (e.rel !== 'Contains') continue;
                if ((e.source.id || e.source) === id) out.push(e.target.id || e.target);
            }
            return out;
        }

        function containsParentsOf(id, known) {
            const out = [];
            for (const e of (known ? knownEdgesOf(id) : edgesOf(id))) {
                if (e.rel !== 'Contains') continue;
                if ((e.target.id || e.target) === id) out.push(e.source.id || e.source);
            }
            return out;
        }

        /// Whether `id`'s edge list is known to be whole.
        function edgesKnownComplete(id) {
            return state.adjCompleteAll || (state.adjComplete && state.adjComplete.has(id));
        }

        // Lazy stand-ins for the two Maps this used to materialise, so callers
        // that already spoke `childrenOf.get(id)` keep working unchanged.
        function getContainsMaps() {
            return {
                childrenOf: { get: (id) => containsChildrenOf(id, true) },
                parentOf: { get: (id) => containsParentsOf(id, true) },
            };
        }

        function getContainsCounts(nodeId) {
            const directChildren = containsChildrenOf(nodeId).length;
            const parents = containsParentsOf(nodeId);
            // Siblings are a two-hop question — the parents' *other* children —
            // so they need the parents' edges as well. Reporting 0 while those
            // are still in flight would read as "an only child", which is a
            // different and wrong answer, so report null and let the caller
            // leave the line out until it is true.
            let siblings = null;
            if (parents.every(edgesKnownComplete)) {
                const set = new Set();
                parents.forEach(p => {
                    containsChildrenOf(p, true).forEach(c => { if (c !== nodeId) set.add(c); });
                });
                siblings = set.size;
            } else {
                ensureEdges(parents);
            }
            return { directChildren, siblings, parents: parents.length };
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
            // an open box — a value slot that can be refilled
            Variable: '<rect x="4.5" y="9.5" width="15" height="10" rx="2"/><path d="M8.5 9.5V7.5a3.5 3.5 0 0 1 7 0"/>',
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
            boundary: ['System boundary', 'Where this code meets something outside the system: an '
                + 'HTTP endpoint, a queue listener, a CLI command or a scheduled job coming in; an '
                + 'HTTP, database or queue client going out. Detected from framework annotations, '
                + 'decorators, attributes and client call sites, so it is a strong signal rather '
                + 'than a certainty. It matters most before a change — a boundary is a contract '
                + 'something outside the repo already depends on, and the call graph cannot show '
                + 'you its callers because they were never indexed.'],
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
            preview: ['Source', 'The file as it is on disk right now, read live — or, when the '
                + 'repo path is unavailable, the source the index captured at index time. This '
                + 'is the only tab that does not come from the index when the repo is present, '
                + 'so it is where you see changes made since the last ug gen.'],
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
                    label: 'working tree — live (index fallback)',
                    detail: loc
                        ? `Read from ${loc} via /api/file: the file as it is on disk right now, `
                          + 'or the source the index captured when the repo path is unavailable. '
                          + 'When the repo is present this is the only tab not served from the '
                          + 'index, so it is where edits made since the last ug gen show up.'
                        : 'Read via /api/file — the file as it is now when the repo is available, '
                          + 'the indexed copy when it is not.',
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
