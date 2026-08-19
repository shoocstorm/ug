        // ─── Health polling ─────────────────────────────────

        async function pollHealth() {
            const badge = document.getElementById('health-badge');
            if (!badge) return;
            try {
                const r = await fetch('/healthz', { cache: 'no-store' });
                if (r.ok) {
                    badge.classList.remove('stale');
                    badge.classList.add('connected');
                    badge.textContent = 'Connected';
                    badge.title = 'Server reachable';
                } else {
                    badge.classList.remove('connected');
                    badge.classList.add('stale');
                    badge.textContent = `HTTP ${r.status}`;
                    badge.title = `Server returned ${r.status}`;
                }
            } catch (err) {
                badge.classList.remove('connected');
                badge.classList.add('stale');
                badge.textContent = 'Offline';
                badge.title = 'Server unreachable';
            }
        }

        function startHealthPolling() {
            pollHealth();
            setInterval(pollHealth, 3500000);
        }

        // ─── Capabilities probe ─────────────────────────────

        // `/api/capabilities`, fetched at most once per page load.
        //
        // It used to be read only here, from `initialize()`. It is now also
        // read by `loadGraph`, *before* the graph exists, because its `graph`
        // block is what decides whether the graph arrives as `graph.json` or as
        // the slim index. Two fetches would be two chances to disagree with
        // each other, so both callers share this promise.
        //
        // Never rejects: every caller's honest fallback for "no answer" is the
        // same as its fallback for "no server", and on a static host there
        // genuinely is no server. `null` says so.
        let capabilitiesPromise = null;
        function getCapabilities() {
            if (!capabilitiesPromise) {
                capabilitiesPromise = fetch('/api/capabilities')
                    .then(res => (res.ok ? res.json() : null))
                    .catch(() => null);
            }
            return capabilitiesPromise;
        }

        async function probeCapabilities() {
            const chatSection = document.getElementById('section-chat');
            const tourSection = document.getElementById('section-tour');
            const tourDisabled = document.getElementById('section-tour-disabled');
            const chatDisabled = document.getElementById('section-chat-disabled');
            const chatBadgeRow = document.getElementById('chat-model-badge');
            const chatBadgePill = document.getElementById('chat-model-pill');
            const dot = document.querySelector('.sidebar-header .brand-dot');

            const showSection = (el) => el && el.classList.remove('cap-hidden');
            const hideSection = (el) => el && el.classList.add('cap-hidden');

            // The Search pane stays open — Keyword works off graph.json. Only
            // the DB-backed Semantic/Hybrid tabs gate on search readiness:
            // when off they're disabled with an inline note, and an active
            // vector tab falls back to Keyword.
            const semVectorTabs = document.querySelectorAll('.sem-mode[data-mode="semantic"], .sem-mode[data-mode="hybrid"]');
            const semCapNote = document.getElementById('sem-cap-note');
            const setVectorSearch = (ok, reason) => {
                semVectorTabs.forEach(b => { b.disabled = !ok; b.title = ok ? '' : reason; });
                if (semCapNote) { semCapNote.hidden = ok; if (!ok) semCapNote.textContent = reason; }
                if (!ok && state.semMode && state.semMode !== 'keyword') selectSemMode('keyword');
            };

            try {
                const caps = await getCapabilities();
                if (!caps) throw new Error('capabilities unavailable');
                state.capabilities = caps;

                if (caps.search_ready) {
                    setVectorSearch(true, '');
                    if (dot) {
                        dot.classList.remove('cap-warn', 'cap-off');
                        dot.title = `DB ready · ${caps.db_node_count ?? '?'} nodes`;
                    }
                    renderDestSelector(caps);
                } else {
                    const reason = caps.reason || 'DB-backed search unavailable.';
                    setVectorSearch(false, reason);
                    if (dot) {
                        const partial = caps.db_ready && caps.embedder_ready;
                        dot.classList.toggle('cap-warn', partial);
                        dot.classList.toggle('cap-off', !partial);
                        dot.title = reason;
                    }
                }

                // A tour needs retrieval (DB + embedder); LLM narration is
                // optional — the server falls back to a ranked itinerary — so
                // gate the launcher on search readiness, not chat.
                if (caps.search_ready) {
                    showSection(tourSection);
                    hideSection(tourDisabled);
                } else {
                    hideSection(tourSection);
                    showSection(tourDisabled);
                }
                // Tours degrade gracefully without a model; say so where the
                // user is about to start one, not in a banner elsewhere.
                const tourNoLlm = document.getElementById('tour-nollm-msg');
                if (tourNoLlm) {
                    tourNoLlm.classList.toggle('cap-hidden', !(caps.search_ready && !caps.chat_ready));
                }
                // ── Insights tab ──
                // Insights presets run /api/tools/analyze, which is
                // store-backed: the preset list itself is static, but
                // running one needs the structural DB. The banner offers
                // an Ingest button — `ug ingest` writes the structural
                // nodes even on embedder failure, which is all Insights
                // needs (vectors are Chat/Tour/Semantic only).
                const insNoDb = document.getElementById('ins-nodb-msg');
                if (insNoDb) insNoDb.classList.toggle('cap-hidden', !!caps.db_ready);

                markSubtabAvailability(caps);
                // History survives page loads, and the key is per project, so
                // it can only be read once capabilities name the project.
                renderTourHistory();

                if (caps.chat_ready) {
                    showSection(chatSection);
                    hideSection(chatDisabled);
                    if (caps.chat && caps.chat.model && chatBadgeRow && chatBadgePill) {
                        chatBadgeRow.hidden = false;
                        chatBadgePill.hidden = false;
                        chatBadgePill.textContent = caps.chat.model;
                        chatBadgePill.title = `Chat model: ${caps.chat.model}\nBase URL: ${caps.chat.base_url || '?'}`;
                    }
                } else {
                    hideSection(chatSection);
                    // Two failure modes, surfaced in the disabled banner:
                    //   • !search_ready → no vectors. Offer the Ingest button.
                    //   •  search_ready → vectors exist, just no chat model.
                    // Without this split, the no-vectors case showed nothing
                    // at all and the user had no path forward from the UI.
                    const noEmb = document.getElementById('chat-no-embeddings-msg');
                    const noLlm = document.getElementById('chat-disabled-msg');
                    if (caps.search_ready) {
                        if (noEmb) noEmb.hidden = true;
                        if (noLlm) noLlm.hidden = false;
                        showSection(chatDisabled);
                    } else {
                        if (noEmb) noEmb.hidden = false;
                        if (noLlm) noLlm.hidden = true;
                        showSection(chatDisabled);
                    }
                }
            } catch (err) {
                state.capabilities = { db_ready: false, embedder_ready: false, search_ready: false, chat_ready: false };
                setVectorSearch(false, 'Capabilities probe failed — server unreachable?');
                hideSection(chatSection);
                hideSection(tourSection);
                showSection(tourDisabled);
                const insNoDb = document.getElementById('ins-nodb-msg');
                if (insNoDb) insNoDb.classList.remove('cap-hidden');
                markSubtabAvailability(state.capabilities);
                if (dot) {
                    dot.classList.add('cap-off');
                    dot.title = 'Capabilities probe failed';
                }
                console.warn('capabilities probe failed:', err);
            }
        }

        // Dot a Discover sub-tab whose backend isn't available. The tab stays
        // clickable on purpose — its pane explains what's missing, which beats
        // a mode that silently vanishes.
        function markSubtabAvailability(caps) {
            const set = (sub, ok) => {
                const el = document.querySelector(`.subtab[data-sub="${sub}"]`);
                if (!el) return;
                el.classList.toggle('unavailable', !ok);
                el.title = ok ? '' : 'Not available with the current server setup';
            };
            // Keyword search (part of the Search subtab) works off graph.json
            // with no DB, so the subtab is never marked unavailable; the
            // Semantic/Hybrid tabs within it carry their own disabled state.
            set('search', true);
            set('tour', !!(caps && caps.search_ready));
            set('chat', !!(caps && caps.chat_ready));
        }

        // ─── Trigger embedding/ingestion from the UI ─────────────
        //
        // When `capabilities.search_ready` is false the graph is loaded
        // but no vectors have been written (`ug gen --no-ingest`, or the
        // embedder was down last run). The disabled banners in Chat/Tour
        // plus the indexed-tab empty state each surface an "Ingest now"
        // button; this is the shared runner.
        //
        // Every ingest button drives the *same* underlying job — they all
        // target the active project's store — so one click runs once and
        // every button mirrors the shared state: all disable, all show the
        // same progress, all show the same error.
        //
        // Reuses the gen-job tracker (`/api/generate/status`) so the
        // polling shape matches the KB Manager wizard, and re-probes
        // capabilities on success so the banner retires itself the
        // moment the freshly-embedded store is live.

        let ingestRunning = false;
        let ingestStatusHtml = '';

        // Every "Ingest now" button + its sibling status slot currently
        // in the DOM. Queried live so a re-rendered indexed tab (which
        // recreates its button on each node selection) drops the stale
        // instance automatically instead of leaking it.
        function ingestPairs() {
            const pairs = [];
            document.querySelectorAll('[id$="-ingest-btn"]').forEach(btn => {
                const status = document.getElementById(btn.id.replace(/-ingest-btn$/, '-ingest-status'));
                if (status) pairs.push({ btn, status });
            });
            return pairs;
        }

        function setAllIngestDisabled(disabled) {
            ingestPairs().forEach(({ btn }) => { btn.disabled = disabled; });
        }

        function setAllIngestStatus(html, isError) {
            ingestStatusHtml = html;
            ingestPairs().forEach(({ status }) => {
                status.classList.toggle('error', !!isError);
                status.innerHTML = html;
                status.hidden = false;
            });
        }

        // A button rendered while an ingest is already running should show
        // the shared state immediately instead of waiting for the next
        // poll tick.
        function ingestStateForFreshButton(btn, status) {
            if (!ingestRunning) return;
            btn.disabled = true;
            status.classList.remove('error');
            status.innerHTML = ingestStatusHtml;
            status.hidden = false;
        }

        // Drive a single shared ingest. The clicked button only seeds the
        // run — progress is mirrored to every ingest button on the page.
        function triggerIngest(opts) {
            const btn = opts.btn;
            const status = opts.status;
            const project = opts.project || (state.capabilities && state.capabilities.project && state.capabilities.project.name);
            if (!btn || !status) return;
            if (ingestRunning) return;
            ingestRunning = true;
            setAllIngestDisabled(true);
            setAllIngestStatus('<span class="tour-spinner"></span>Starting ingest…');

            let jobId = null;
            fetch('/api/ingest', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(project ? { name: project } : {}),
            }).then(r => {
                if (!r.ok) return r.json().catch(() => ({})).then(e => { throw new Error(e.error || `HTTP ${r.status}`); });
                return r.json();
            }).then(data => {
                jobId = data.jobId;
                pollIngestJob(jobId);
            }).catch(err => {
                finishIngest(true, `Couldn't start ingest: ${err.message || err}`);
            });
        }

        function pollIngestJob(jobId) {
            const tick = async () => {
                let job;
                try {
                    const res = await fetch(`/api/generate/status?job=${encodeURIComponent(jobId)}`, { cache: 'no-store' });
                    job = await res.json();
                } catch (err) {
                    // Network blip — keep polling rather than failing the
                    // whole run, which is still going on the server.
                    setTimeout(tick, 1500);
                    return;
                }
                const lastLine = (job.log || []).slice(-1)[0] || '';
                if (job.status === 'running') {
                    setAllIngestStatus('<span class="tour-spinner"></span>'
                        + escapeHtml(lastLine || 'Ingesting…'));
                    setTimeout(tick, 1000);
                } else if (job.status === 'done') {
                    setAllIngestStatus('<span style="color: var(--success, #4ade80)">✓ Ingest complete — refreshing…</span>');
                    // Give the server a beat to swap the stores, then
                    // re-probe; probeCapabilities hides the banner once
                    // search_ready flips true.
                    setTimeout(async () => {
                        await probeCapabilities();
                        finishIngest(false);
                    }, 600);
                } else {
                    finishIngest(true, job.error || 'Ingest failed.');
                }
            };
            tick();
        }

        function finishIngest(isError, msg) {
            ingestRunning = false;
            setAllIngestDisabled(false);
            if (isError) {
                setAllIngestStatus(escapeHtml(msg || 'Ingest failed.'), true);
            }
        }

        // Wire every "Ingest now" button on the page to its sibling
        // status slot. Called once after the DOM is built; the chat/tour/
        // insights buttons are static in the disabled banners, so a single
        // pass at init covers them. The indexed-tab button is created
        // per-node in JS and wires its own click to triggerIngest.
        function wireIngestButtons() {
            document.querySelectorAll('[id$="-ingest-btn"]').forEach(btn => {
                if (btn.dataset.wired === '1') return;
                btn.dataset.wired = '1';
                // Sibling status slot: same prefix, `-status` suffix.
                const prefix = btn.id.replace(/-ingest-btn$/, '');
                const status = document.getElementById(prefix + '-ingest-status');
                btn.addEventListener('click', (e) => {
                    e.preventDefault();
                    triggerIngest({ btn, status });
                });
            });
        }

        // ─── Settings modal (view + persist ~/.ug/config.json) ─────
        // Backed by GET/POST /api/config. Precedence: CLI flag > env
        // var > saved config > default. Rows carry a source chip and an
        // override note when a flag/env var outranks the saved value,
        // so nothing about the resolution is silent.

        const settingsUi = {
            data: null,        // last /api/config payload
            edits: new Map(),  // key name → new raw string value
            unsets: new Set(), // key names marked "clear on save"
        };

        const SETTINGS_SECTIONS = {
            chat: {
                title: 'Chat',
                sub: 'GraphRAG answers — ug chat & this UI',
                badge: ['live', 'applies instantly'],
                icon: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>',
            },
            embed: {
                title: 'Embeddings',
                sub: 'Vector search & ingest',
                badge: ['restart', 'after restart'],
                icon: '<path d="m12 2 8.5 5v10L12 22l-8.5-5V7L12 2z"/><path d="M12 22v-10"/><path d="m3.5 7 8.5 5 8.5-5"/>',
            },
            vis: {
                title: 'Visualization',
                sub: 'How the graph is drawn — engine & solo mode',
                badge: ['reload', 'applies on reload'],
                icon: '<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z"/><circle cx="12" cy="12" r="3"/>',
            },
        };

        const SETTINGS_FIELDS = {
            'chat.model': { label: 'Model', hint: 'Any model your endpoint serves — e.g. gpt-4o-mini, or a local MLX / vLLM / Ollama model name.' },
            'chat.base_url': { label: 'Endpoint URL', hint: 'OpenAI-compatible base URL, usually ending in /v1.' },
            'chat.api_key': { label: 'API key', hint: 'Stored locally in config.json with owner-only file permissions; never sent to the browser.' },
            'chat.temperature': { label: 'Temperature', hint: '0 = deterministic, higher = more creative.', num: { step: '0.05', min: '0', max: '2' } },
            'chat.max_tokens': { label: 'Max tokens', hint: 'Cap on completion length.', num: { step: '1', min: '1' } },
            'chat.timeout_secs': { label: 'Timeout (seconds)', hint: 'HTTP timeout per completion call.', num: { step: '1', min: '1' } },
            'embed.model': { label: 'Model', hint: 'Local fastembed alias (bge-small-en-v1.5, nomic-embed-text-v1.5, …) or a remote model name.' },
            'embed.base_url': { label: 'Endpoint URL', hint: 'Leave unset to embed locally in-process (no network needed).' },
            'embed.api_key': { label: 'API key', hint: 'Only needed for a remote embeddings endpoint.' },
            'embed.dim': { label: 'Dimension', hint: 'Auto-probed on first ingest — set only to pin it explicitly.', num: { step: '1', min: '1' } },
            'vis.renderer': {
                label: 'Rendering engine',
                selectLabels: {
                    auto: 'Auto (recommended)',
                    three: 'Three.js — 3D',
                    cosmos: 'Cosmos — 2D (scale)',
                },
                hint: 'Three.js draws in 3D, with depth, effects and one styling pass per node — pleasant on a small repo, unusable past a few thousand nodes. Cosmos draws in 2D, on the GPU, and holds graphs far too big for 3D. Auto adjusts by graph size: picks three below 3,000 elements and cosmos above it — the point (THREE_D_MAX_ELEMENTS) where 3D stops being readable and the 2D engine takes over.',
            },
            'vis.solo_threshold': {
                label: 'Solo mode threshold',
                hint: 'Past this many nodes or edges the page never draws the whole graph — it opens in solo mode and shows one neighbourhood at a time. Lower it to isolate large repos earlier, higher to attempt whole-graph render for longer. The 3D engine keeps its own hard ceiling of 3,000 elements, so this threshold governs the 2D engine.',
                num: { step: '1', min: '1', max: '10000000' },
            },
        };

        function settingsOverlayEl() {
            return document.getElementById('settings-overlay');
        }

        function openSettings() {
            settingsOverlayEl().classList.add('visible');
            loadSettings();
        }

        function closeSettings() {
            settingsOverlayEl().classList.remove('visible');
        }

        async function loadSettings() {
            const body = document.getElementById('settings-body');
            settingsUi.edits.clear();
            settingsUi.unsets.clear();
            setSettingsStatus('', '');
            body.innerHTML = '<div class="settings-loading">Loading configuration…</div>';
            try {
                const res = await fetch('/api/config');
                if (!res.ok) throw new Error('HTTP ' + res.status);
                settingsUi.data = await res.json();
                renderSettings();
            } catch (err) {
                body.innerHTML = '<div class="settings-error">Couldn’t load configuration — ' + escapeHtml(String(err.message || err)) + '</div>';
            }
            updateSettingsFooter();
        }

        function renderSettings() {
            const body = document.getElementById('settings-body');
            body.innerHTML = '';
            const data = settingsUi.data;
            if (!data) return;
            const pathEl = document.getElementById('settings-path');
            if (pathEl) {
                pathEl.textContent = data.path;
                pathEl.title = data.path;
            }
            const bySection = {};
            for (const k of data.keys) (bySection[k.section] ||= []).push(k);
            for (const [sec, meta] of Object.entries(SETTINGS_SECTIONS)) {
                const keys = bySection[sec];
                if (!keys || !keys.length) continue;
                const group = document.createElement('div');
                group.className = 'settings-group';
                const head = document.createElement('div');
                head.className = 'settings-group-head';
                head.innerHTML =
                    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' + meta.icon + '</svg>' +
                    '<span class="settings-group-title">' + escapeHtml(meta.title) + '</span>' +
                    '<span class="settings-group-sub">' + escapeHtml(meta.sub) + '</span>' +
                    '<span class="settings-apply-badge ' + meta.badge[0] + '">' + escapeHtml(meta.badge[1]) + '</span>';
                group.appendChild(head);
                const rows = document.createElement('div');
                rows.className = 'settings-rows';
                for (const k of keys) rows.appendChild(renderSettingsRow(k));
                group.appendChild(rows);
                body.appendChild(group);
            }
        }

        function renderSettingsRow(k) {
            const meta = SETTINGS_FIELDS[k.name] || { label: k.name, hint: k.desc };
            const row = document.createElement('div');
            row.className = 'settings-row';
            row.dataset.key = k.name;

            const head = document.createElement('div');
            head.className = 'settings-row-head';
            const label = document.createElement('label');
            label.textContent = meta.label;
            const keySpan = document.createElement('span');
            keySpan.className = 'settings-key';
            keySpan.textContent = k.name;
            const chip = document.createElement('span');
            chip.className = 'settings-chip';
            if (k.source === 'flag') {
                chip.classList.add('chip-flag');
                chip.textContent = k.flag;
                chip.title = 'This server was started with ' + k.flag + ', which outranks the saved value.';
            } else if (k.source === 'env') {
                chip.classList.add('chip-env');
                chip.textContent = '$' + k.env;
                chip.title = '$' + k.env + ' is set in the server’s environment and outranks the saved value.';
            } else if (k.source === 'config') {
                chip.classList.add('chip-config');
                chip.textContent = 'saved';
                chip.title = 'Using the value saved in ' + (settingsUi.data ? settingsUi.data.path : 'config.json');
            } else {
                chip.classList.add('chip-default');
                chip.textContent = 'default';
                chip.title = 'No saved value — using the built-in default.';
            }
            head.append(label, keySpan, chip);
            row.appendChild(head);

            const wrap = document.createElement('div');
            wrap.className = 'settings-input-wrap';

            // Enum keys (e.g. the rendering engine) are a `<select>` over the
            // backend-declared choices; everything else keeps the text/number/
            // password `<input>`. Both share the value/dirty/clear handling below.
            const isEnum = k.kind === 'enum' && Array.isArray(k.choices) && k.choices.length > 0;
            let control;
            if (isEnum) {
                control = document.createElement('select');
                const labels = (meta.selectLabels) || {};
                const baselineOption = k.saved != null ? String(k.saved) : String(k.default || '');
                for (const c of k.choices) {
                    const opt = document.createElement('option');
                    opt.value = c;
                    opt.textContent = labels[c] || c;
                    if (c === baselineOption) opt.selected = true;
                    control.appendChild(opt);
                }
            } else {
                control = document.createElement('input');
                if (k.secret) {
                    control.type = 'password';
                    control.autocomplete = 'new-password';
                } else if (meta.num) {
                    control.type = 'number';
                    control.step = meta.num.step;
                    if (meta.num.min !== undefined) control.min = meta.num.min;
                    if (meta.num.max !== undefined) control.max = meta.num.max;
                } else {
                    control.type = 'text';
                    control.spellcheck = false;
                }
                // Secrets are never echoed back into the field — the server
                // only sends a masked preview, shown as the placeholder.
                if (!k.secret && k.saved != null) control.value = k.saved;
                if (k.secret) {
                    control.placeholder = k.saved != null ? 'saved · ' + k.saved + ' — type to replace' : 'not set';
                } else {
                    control.placeholder = k.default != null ? 'default: ' + k.default : 'not set';
                }
            }
            wrap.appendChild(control);

            const clearBtn = document.createElement('button');
            clearBtn.type = 'button';
            clearBtn.className = 'settings-clear-btn';
            clearBtn.title = 'Clear the saved value (back to default)';
            clearBtn.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2m2 0-.7 12.1A2 2 0 0 1 14.3 21H9.7a2 2 0 0 1-2-1.9L7 7"/></svg>';
            clearBtn.hidden = k.saved == null;
            wrap.appendChild(clearBtn);
            row.appendChild(wrap);

            const note = document.createElement('div');
            note.className = 'settings-row-note';
            if (k.source === 'flag' || k.source === 'env') {
                note.classList.add('override');
                const src = k.source === 'flag' ? 'CLI flag <code>' + escapeHtml(k.flag) + '</code>' : 'env var <code>$' + escapeHtml(k.env) + '</code>';
                const eff = k.effective != null ? ' — currently using <code>' + escapeHtml(k.effective) + '</code>' : '';
                note.innerHTML = 'Overridden by ' + src + eff + '. Saving still updates the file for every other run.';
            } else {
                note.textContent = meta.hint || k.desc || '';
            }
            row.appendChild(note);

            const onDirty = () => {
                if (settingsUi.unsets.has(k.name)) {
                    settingsUi.unsets.delete(k.name);
                    row.classList.remove('pending-unset');
                }
                const val = control.value.trim();
                // For a select the shown default *is* a legal choice, so an
                // unset row baseline is the default rather than '' — leaving
                // it untouched must not mark it dirty.
                const baseline = k.secret ? '' : (k.saved != null ? String(k.saved) : (isEnum ? String(k.default || '') : ''));
                if (val !== baseline) settingsUi.edits.set(k.name, val);
                else settingsUi.edits.delete(k.name);
                row.classList.toggle('dirty', settingsUi.edits.has(k.name));
                updateSettingsFooter();
            };
            control.addEventListener('input', onDirty);
            control.addEventListener('change', onDirty);

            clearBtn.addEventListener('click', () => {
                if (settingsUi.unsets.has(k.name)) {
                    // Undo the pending clear.
                    settingsUi.unsets.delete(k.name);
                    row.classList.remove('pending-unset');
                    if (!k.secret && k.saved != null) control.value = k.saved;
                } else {
                    settingsUi.unsets.add(k.name);
                    settingsUi.edits.delete(k.name);
                    row.classList.remove('dirty');
                    row.classList.add('pending-unset');
                    if (!k.secret) control.value = k.saved != null ? k.saved : (isEnum ? (k.default || '') : '');
                }
                updateSettingsFooter();
            });

            return row;
        }

        function setSettingsStatus(kind, text) {
            const el = document.getElementById('settings-status');
            el.className = 'settings-status' + (kind ? ' ' + kind : '');
            el.textContent = text;
        }

        function updateSettingsFooter() {
            const count = settingsUi.edits.size + settingsUi.unsets.size;
            document.getElementById('settings-save').disabled = count === 0;
            document.getElementById('settings-revert').disabled = count === 0;
            const status = document.getElementById('settings-status');
            if (count > 0) {
                setSettingsStatus('', count + ' unsaved change' + (count === 1 ? '' : 's'));
            } else if (!status.classList.contains('ok') && !status.classList.contains('err')) {
                // Only clear neutral "N unsaved changes" text — keep a
                // just-shown save result visible.
                setSettingsStatus('', '');
            }
        }

        async function saveSettings() {
            const saveBtn = document.getElementById('settings-save');
            const set = {};
            for (const [name, val] of settingsUi.edits) set[name] = val;
            const unset = Array.from(settingsUi.unsets);
            const touched = Object.keys(set).concat(unset);
            const touchedEmbed = touched.some((n) => n.startsWith('embed.'));
            const touchedVis = touched.some((n) => n.startsWith('vis.'));
            saveBtn.disabled = true;
            setSettingsStatus('', 'Saving…');
            try {
                const res = await fetch('/api/config', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ set, unset }),
                });
                if (!res.ok) {
                    const err = await res.json().catch(() => ({}));
                    throw new Error(err.error || 'HTTP ' + res.status);
                }
                settingsUi.data = await res.json();
                settingsUi.edits.clear();
                settingsUi.unsets.clear();
                renderSettings();
                updateSettingsFooter();
                setSettingsStatus('ok', touchedVis
                    ? 'Saved ✓ — the page picks this up on the next reload'
                    : (touchedEmbed
                        ? 'Saved ✓ — chat applies now; embedding changes take effect after a server restart'
                        : 'Saved ✓ — applied immediately'));
                // Chat readiness / model pill may have just changed.
                probeCapabilities();
            } catch (err) {
                setSettingsStatus('err', 'Save failed: ' + String(err.message || err));
                updateSettingsFooter();
                saveBtn.disabled = settingsUi.edits.size + settingsUi.unsets.size === 0;
            }
        }

        (function initSettingsUi() {
            const overlay = settingsOverlayEl();
            // Any "set up a model" call-to-action opens the same settings panel
            // the header button does — the model is editable at runtime, so
            // "restart the server" was never the right advice.
            document.querySelectorAll('[data-open-settings]').forEach(btn => {
                btn.addEventListener('click', openSettings);
            });
            document.getElementById('settings-open-btn').addEventListener('click', openSettings);
            document.getElementById('kb-settings-btn').addEventListener('click', openSettings);
            document.getElementById('settings-close').addEventListener('click', closeSettings);
            document.getElementById('settings-revert').addEventListener('click', () => {
                settingsUi.edits.clear();
                settingsUi.unsets.clear();
                renderSettings();
                updateSettingsFooter();
                setSettingsStatus('', '');
            });
            document.getElementById('settings-save').addEventListener('click', saveSettings);
            overlay.addEventListener('click', (e) => {
                if (e.target === overlay) closeSettings();
            });
            document.addEventListener('keydown', (e) => {
                if (e.key === 'Escape' && overlay.classList.contains('visible')) closeSettings();
            });
        })();

        // ─── Destination selector ──────────────────────────
        // Populated from /api/capabilities. The user always sees which
        // backend they're querying; with >1 backend they can switch
        // before each search.

        function renderDestSelector(caps) {
            const row = document.getElementById('sem-dest-row');
            const select = document.getElementById('sem-dest');
            const badge = document.getElementById('sem-dest-badge');
            if (!row || !select || !badge) return;

            // Only count the destinations that actually opened
            // (skip ones with an `error` field — those failed at startup).
            const all = Array.isArray(caps.destinations) ? caps.destinations : [];
            const ready = all.filter(d => !d.error);
            if (ready.length === 0) {
                row.hidden = true;
                return;
            }
            row.hidden = false;

            const primary = caps.primary || (ready.find(d => d.primary) || ready[0]).name;
            // Preserve the user's prior choice if it still exists across
            // a capabilities refresh.
            const desired = state.semDest && ready.some(d => d.name === state.semDest)
                ? state.semDest
                : primary;
            state.semDest = desired;

            if (ready.length === 1) {
                // Single-backend serve: static badge so the user still
                // knows which DB they're hitting, but no dropdown noise.
                select.hidden = true;
                badge.hidden = false;
                badge.textContent = formatDestLabel(ready[0]);
                badge.title = badgeTitle(ready[0]);
            } else {
                badge.hidden = true;
                select.hidden = false;
                select.innerHTML = '';
                for (const d of ready) {
                    const opt = document.createElement('option');
                    opt.value = d.name;
                    opt.textContent = formatDestLabel(d) + (d.primary ? '  (default)' : '');
                    opt.title = badgeTitle(d);
                    if (d.name === desired) opt.selected = true;
                    select.appendChild(opt);
                }
                // Also report any backends that failed to open so the
                // operator sees them without having to check logs.
                const failed = all.filter(d => d.error);
                if (failed.length) {
                    const group = document.createElement('optgroup');
                    group.label = 'unavailable';
                    for (const d of failed) {
                        const opt = document.createElement('option');
                        opt.value = d.name;
                        opt.disabled = true;
                        opt.textContent = `${d.name} — ${d.error}`;
                        group.appendChild(opt);
                    }
                    select.appendChild(group);
                }
                select.onchange = () => {
                    state.semDest = select.value;
                };
            }
        }

        function formatDestLabel(d) {
            const count = (d.node_count != null) ? `· ${d.node_count.toLocaleString()} nodes` : '';
            return `${d.name} ${count}`.trim();
        }

        function badgeTitle(d) {
            const parts = [`Backend: ${d.name}`];
            if (d.node_count != null) parts.push(`${d.node_count.toLocaleString()} nodes`);
            parts.push(`PPR: ${d.supports_native_ppr ? 'native' : 'MMR fallback'}`);
            return parts.join(' · ');
        }

