        // ─── Knowledge Base Manager (startup landing + wizard) ──
        // Always shown first when the server is in multi-project mode:
        // discovers ~/.ug projects via /api/projects, lets the user open
        // one (activates it, then loads the graph) or generate a new one
        // from a folder path via /api/generate — a background job on the
        // server, polled through /api/generate/status. In single-project
        // mode (`ug serve -i <graph.json>`) this is skipped entirely.

        let kbCapsCache = null;
        // Whether `initialize()` has already wired up the 3D scene, event
        // listeners, etc. in this page session. Re-running it in place (by
        // calling loadGraph() a second time) would double-attach every
        // listener and leak the old three.js scene, so once this is true,
        // switching projects goes through a full page reload instead.
        let graphInitialized = false;
        // Whether the server is in multi-project mode — gates the KB
        // Manager reopen affordances (icon button + clicking the brand).
        let isMultiMode = false;
        // Per-card accent, cycled by position — the same categorical
        // palette the legend / view-bar face labels already use
        // elsewhere in the app, so the manager reads as part of the
        // same product rather than a bolted-on landing page.
        const KB_PALETTE = ['#f97316', '#5b8fc9', '#fb923c', '#8fb8dd', '#f59e0b', '#c2410c'];

        function setKbStatus(text) {
            const el = document.getElementById('kb-status-text');
            if (el) el.textContent = text;
        }

        // Compact "updated" label for the card footer — the absolute
        // timestamp is long enough to get ellipsed next to the stale note,
        // so the exact time moves into the row's tooltip instead.
        function kbRelativeTime(epochSeconds) {
            const mins = Math.round((Date.now() / 1000 - epochSeconds) / 60);
            if (mins < 1) return 'just now';
            if (mins < 60) return `${mins}m ago`;
            const hours = Math.round(mins / 60);
            if (hours < 24) return `${hours}h ago`;
            const days = Math.round(hours / 24);
            if (days < 30) return `${days}d ago`;
            const months = Math.round(days / 30);
            return months < 12 ? `${months}mo ago` : `${Math.round(months / 12)}y ago`;
        }

        function kbCardIconSvg() {
            return `<svg class="kb-card-icon" viewBox="0 0 24 24" fill="none">
                <circle cx="12" cy="5" r="2.2" fill="currentColor" />
                <circle cx="5" cy="18" r="2.2" fill="currentColor" />
                <circle cx="19" cy="18" r="2.2" fill="currentColor" />
                <path d="M12 7.2 6.3 16.2 M12 7.2 17.7 16.2 M7 18 H17" stroke="currentColor" stroke-width="1.4" opacity="0.6" />
            </svg>`;
        }

        async function bootstrap() {
            wireKbManager();
            let caps;
            try {
                const res = await fetch('/api/projects', { cache: 'no-store' });
                if (!res.ok) throw new Error(`HTTP ${res.status}`);
                caps = await res.json();
            } catch (err) {
                // Static serve, or an older server without the endpoint —
                // fall back to the pre-KB-manager behavior.
                loadGraph();
                return;
            }
            if (caps.mode !== 'multi') {
                loadGraph();
                return;
            }
            isMultiMode = true;
            kbCapsCache = caps;
            startStalenessPolling();
            document.getElementById('kb-open-btn').hidden = false;
            document.getElementById('brand-title').classList.add('brand-clickable');
            document.getElementById('brand-title').title = 'Browse knowledge bases';

            // `openProject` marks the URL before reloading to switch
            // projects mid-session (see the `graphInitialized` reload
            // path). On a genuine fresh navigation this is absent and the
            // manager always shows first, per the always-show-on-startup
            // design; on a switch-triggered reload it's present, so skip
            // straight to the project the user just picked instead of
            // making them pick it again from the manager.
            const params = new URLSearchParams(window.location.search);
            // A deep link (`?p=<project>`) opens that project straight away
            // instead of the manager. If it isn't the server's active one,
            // switch the server over first so the graph that comes back is
            // the one the link pointed at.
            const p = params.get('p');
            if (p) {
                state.activeProject = p;
                if (caps.active !== p) {
                    try {
                        await fetch('/api/projects/select', {
                            method: 'POST',
                            headers: { 'Content-Type': 'application/json' },
                            body: JSON.stringify({ name: p }),
                        });
                    } catch (e) {
                        // Deep link to a project we can't reach — fall back to
                        // whatever the server has active.
                    }
                }
                loadGraph();
                return;
            }
            if (params.get('kbOpen') === '1') {
                params.delete('kbOpen');
                const rest = params.toString();
                window.history.replaceState(null, '', window.location.pathname + (rest ? `?${rest}` : ''));
                loadGraph();
                return;
            }

            showKbManager(caps);
        }

        function showKbManager(caps) {
            document.getElementById('kb-manager').classList.add('visible');
            if (!caps.projects.length) {
                showKbWizard({ canGoBack: false });
            } else {
                showKbList(caps);
            }
        }

        function hideKbManager() {
            document.getElementById('kb-manager').classList.remove('visible');
        }

        function showKbList(caps) {
            document.getElementById('kb-loading').hidden = true;
            document.getElementById('kb-wizard-view').hidden = true;
            document.getElementById('kb-list-view').hidden = false;

            const count = caps.projects.length;
            setKbStatus(`${count} knowledge base${count === 1 ? '' : 's'} indexed`);

            const grid = document.getElementById('kb-grid');
            grid.innerHTML = '';
            caps.projects.forEach((p, i) => {
                // A plain div (not <button>) because it hosts a real nested
                // delete <button> — nested buttons are invalid HTML and
                // browsers mis-parse them. role/tabIndex + a keydown handler
                // restore the button-like keyboard behavior a native
                // <button> would have given for free.
                const card = document.createElement('div');
                card.className = 'kb-card' + (p.name === caps.active ? ' active' : '');
                card.setAttribute('role', 'button');
                card.tabIndex = 0;
                card.style.setProperty('--card-accent', KB_PALETTE[i % KB_PALETTE.length]);
                card.style.setProperty('--i', i);
                const updatedExact = p.updatedAt ? new Date(p.updatedAt * 1000).toLocaleString() : '—';
                const updated = p.updatedAt ? kbRelativeTime(p.updatedAt) : '—';
                // KB kind (docs / code / mixed) is computed server-side from
                // the graph's node composition and stamped onto the cached
                // project by refreshStaleness(). Until the first staleness
                // poll lands, no badge is shown rather than a wrong guess.
                const kbType = p.kbKind || '';
                const kbTypeLabel = { docs: 'Docs', code: 'Code', mixed: 'Mixed' }[kbType] || '';
                const isActive = p.name === caps.active;
                const staleInfo = p.staleInfo || 'Files changed';

                card.innerHTML = `
                    <div class="kb-card-top">
                        ${kbCardIconSvg()}
                        <div class="kb-card-name" title="${escapeHtml(p.name)}">${escapeHtml(p.name)}</div>
                        <div class="kb-card-actions">
                            ${p.stale ? `
                            <button type="button" class="kb-card-action kb-card-reindex" title="Index is stale — ${escapeHtml(staleInfo)} since last indexing. Click to re-index." aria-label="Re-index ${escapeHtml(p.name)}">
                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 11A8 8 0 0 0 6.3 5.7L3 9m1 4a8 8 0 0 0 13.7 5.3L21 15"/><path d="M3 4v5h5M21 20v-5h-5"/></svg>
                            </button>` : ''}
                            <button type="button" class="kb-card-action kb-card-delete" title="Delete knowledge base" aria-label="Delete ${escapeHtml(p.name)}">
                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2m2 0-.7 12.1A2 2 0 0 1 14.3 21H9.7a2 2 0 0 1-2-1.9L7 7"/></svg>
                            </button>
                        </div>
                    </div>
                    ${isActive || kbType ? `<div class="kb-card-flags">
                        ${isActive ? '<span class="kb-card-badge is-active">Active</span>' : ''}
                        ${kbType ? `<span class="kb-card-badge kind-${kbType}">${kbTypeLabel}</span>` : ''}
                    </div>` : ''}
                    <div class="kb-card-stats">
                        <div class="kb-card-metric"><b>${(p.nodes || 0).toLocaleString()}</b><span>Nodes</span></div>
                        <div class="kb-card-metric"><b>${(p.edges || 0).toLocaleString()}</b><span>Edges</span></div>
                    </div>
                    <div class="kb-card-path" title="${escapeHtml(p.repoRoot || '')}">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
                        <span>${escapeHtml(p.repoRoot || '')}</span>
                    </div>
                    ${p.repoMissing ? `<div class="kb-card-repo-missing" title="The source repo path was unavailable on the last staleness check. Everything shown is served from the index; re-indexing needs the repo restored.">repo unavailable — index only</div>` : ''}
                    <div class="kb-card-footer">
                        <span class="kb-card-updated" title="Last indexed ${escapeHtml(updatedExact)}">Updated ${escapeHtml(updated)}</span>
                        ${p.stale ? `<span class="kb-card-stale-note" title="Index is stale — click the re-index button to refresh.">${escapeHtml(staleInfo)}</span>` : ''}
                    </div>
                `;
                card.addEventListener('click', () => openProject(p.name, caps.active));
                card.addEventListener('keydown', (e) => {
                    // Enter/Space on a nested action button bubbles up here —
                    // without this guard the card would also open the project.
                    if (e.target !== card) return;
                    if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        openProject(p.name, caps.active);
                    }
                });
                card.querySelector('.kb-card-delete').addEventListener('click', (e) => {
                    e.stopPropagation();
                    openDeleteConfirm(p);
                });
                const reindexBtn = card.querySelector('.kb-card-reindex');
                if (reindexBtn) {
                    reindexBtn.addEventListener('click', (e) => {
                        e.stopPropagation();
                        reindexProject(p);
                    });
                }
                grid.appendChild(card);
            });

            const addTile = document.createElement('button');
            addTile.type = 'button';
            addTile.className = 'kb-add-tile';
            addTile.style.setProperty('--i', count);
            addTile.innerHTML = `
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                    <path d="M12 5v14M5 12h14" />
                </svg>
                New Knowledge Base
            `;
            addTile.addEventListener('click', () => showKbWizard({ canGoBack: true }));
            grid.appendChild(addTile);
        }

        // ─── Index staleness (auto-detected on start + every 2 min) ──
        // Polls /api/projects/staleness, stamps `stale`/`staleInfo` onto the
        // cached project list, and re-renders the KB grid when the manager
        // is visible so the ⚠ badge appears/disappears without a reload.
        const STALENESS_POLL_MS = 2 * 60 * 1000;
        let stalenessTimer = null;

        async function refreshStaleness() {
            if (!isMultiMode || !kbCapsCache) return;
            let data;
            try {
                const res = await fetch('/api/projects/staleness', { cache: 'no-store' });
                if (!res.ok) return;
                data = await res.json();
            } catch {
                return; // transient network error — next tick will retry
            }
            const byName = new Map((data.projects || []).map(s => [s.name, s]));
            let changed = false;
            for (const p of kbCapsCache.projects) {
                const s = byName.get(p.name);
                const stale = !!(s && s.isStale);
                // A repo root that no longer exists is not "N files deleted" —
                // the whole tree is gone and the index is serving from itself.
                const repoMissing = !!(s && s.repoMissing);
                const info = s && stale
                    ? [s.changed ? `${s.changed} changed` : '', s.missing ? `${s.missing} deleted` : '']
                        .filter(Boolean).join(', ')
                    : '';
                const kind = (s && s.kbKind) || p.kbKind || '';
                if (p.stale !== stale || p.staleInfo !== info || p.kbKind !== kind
                    || p.repoMissing !== repoMissing) {
                    p.stale = stale;
                    p.staleInfo = info;
                    p.kbKind = kind;
                    p.repoMissing = repoMissing;
                    changed = true;
                }
            }
            // Re-render only when something moved and the list is on screen —
            // re-rendering mid-wizard would blow away the generation log.
            const managerVisible = document.getElementById('kb-manager').classList.contains('visible');
            const listVisible = !document.getElementById('kb-list-view').hidden;
            if (changed && managerVisible && listVisible) showKbList(kbCapsCache);
        }

        function startStalenessPolling() {
            if (stalenessTimer) return;
            refreshStaleness();
            stalenessTimer = setInterval(refreshStaleness, STALENESS_POLL_MS);
        }

        // Re-index a stale project by reusing the wizard's /api/generate job
        // flow against the project's repo root. `ug gen` re-parses every
        // file, but its ingest stage is incremental: it diffs against the
        // graph DB and only re-embeds nodes whose text actually changed, so
        // the dominant cost scales with the edit, not the repo. Progress
        // streams into the wizard view.
        async function reindexProject(p) {
            if (!p.repoRoot) {
                alert(`Cannot re-index "${p.name}": no repo root recorded in project.json.`);
                return;
            }
            showKbWizard({ canGoBack: true });
            document.getElementById('kb-path-input').value = p.repoRoot;
            document.getElementById('kb-name-input').value = p.name;
            setKbStatus(`Re-indexing ${p.name}…`);
            document.getElementById('kb-generate-btn').disabled = true;
            document.getElementById('kb-wizard-back').disabled = true;
            document.getElementById('kb-wizard-form').hidden = true;
            document.getElementById('kb-wizard-status').hidden = false;
            document.getElementById('kb-wizard-status-text').textContent = `Re-indexing ${p.name}…`;
            document.getElementById('kb-wizard-log').textContent = '';
            try {
                const res = await fetch('/api/generate', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ path: p.repoRoot, name: p.name }),
                });
                if (!res.ok) {
                    const err = await res.json().catch(() => ({}));
                    throw new Error(err.error || `HTTP ${res.status}`);
                }
                const { jobId } = await res.json();
                await pollGenJob(jobId);
            } catch (err) {
                document.getElementById('kb-wizard-status').hidden = true;
                document.getElementById('kb-wizard-form').hidden = false;
                document.getElementById('kb-wizard-error').hidden = false;
                document.getElementById('kb-wizard-error').textContent = err.message || String(err);
                document.getElementById('kb-generate-btn').disabled = false;
                document.getElementById('kb-wizard-back').disabled = false;
            }
        }

        function showKbWizard({ canGoBack }) {
            document.getElementById('kb-loading').hidden = true;
            document.getElementById('kb-list-view').hidden = true;
            document.getElementById('kb-wizard-view').hidden = false;
            document.getElementById('kb-wizard-back').hidden = !canGoBack;
            document.getElementById('kb-wizard-form').hidden = false;
            document.getElementById('kb-wizard-status').hidden = true;
            document.getElementById('kb-wizard-error').hidden = true;
            document.getElementById('kb-generate-btn').disabled = false;
            document.getElementById('kb-wizard-back').disabled = false;
            setKbStatus(canGoBack ? 'Adding a new knowledge base' : 'No knowledge bases yet');
        }

        // Reload onto whatever project the server now has active, marking
        // the URL so `bootstrap()` opens it directly instead of showing
        // the KB Manager again — a plain `location.reload()` would land
        // back on the manager (a reload is a fresh "startup" too, and the
        // manager always shows first then), forcing a second click to
        // actually open the project the user just switched to.
        function reloadOntoActiveProject() {
            const url = new URL(window.location.href);
            // `p` is the deep-link project key — the reloaded page opens that
            // project directly (see `bootstrap`). Fall back to the old
            // `kbOpen` marker when no project is known so the reload still
            // skips the manager.
            if (state.activeProject) url.searchParams.set('p', state.activeProject);
            else url.searchParams.set('kbOpen', '1');
            window.location.href = url.toString();
        }

        // Open `name` (already-discovered project) in the explorer. If the
        // 3D scene hasn't been initialized yet in this page session (the
        // very first pick from the initial bootstrap screen), it's safe to
        // load in place; otherwise reload the page so `initialize()` only
        // ever runs once per load — see the `graphInitialized` comment.
        async function openProject(name, activeName) {
            hideKbManager();
            state.activeProject = name;
            if (name === activeName) {
                if (!graphInitialized) loadGraph();
                return;
            }
            graphConceal();
            try {
                const res = await fetch('/api/projects/select', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ name }),
                });
                if (!res.ok) {
                    const err = await res.json().catch(() => ({}));
                    throw new Error(err.error || `HTTP ${res.status}`);
                }
                if (graphInitialized) {
                    reloadOntoActiveProject();
                } else {
                    loadGraph();
                }
            } catch (err) {
                console.error('failed to open project:', err);
                document.getElementById('loading').innerHTML =
                    `<p style="color:#f87171">Failed to open project: ${escapeHtml(err.message)}</p>`;
            }
        }

