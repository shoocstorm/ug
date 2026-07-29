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

        async function probeCapabilities() {
            const semSection = document.getElementById('section-semantic');
            const chatSection = document.getElementById('section-chat');
            const tourSection = document.getElementById('section-tour');
            const tourDisabled = document.getElementById('section-tour-disabled');
            const chatDisabled = document.getElementById('section-chat-disabled');
            const disabledSection = document.getElementById('section-cap-disabled');
            const disabledMsg = document.getElementById('cap-disabled-msg');
            const chatBadgeRow = document.getElementById('chat-model-badge');
            const chatBadgePill = document.getElementById('chat-model-pill');
            const dot = document.querySelector('.sidebar-header .brand-dot');

            const showSection = (el) => el && el.classList.remove('cap-hidden');
            const hideSection = (el) => el && el.classList.add('cap-hidden');

            try {
                const res = await fetch('/api/capabilities');
                if (!res.ok) throw new Error('HTTP ' + res.status);
                const caps = await res.json();
                state.capabilities = caps;

                if (caps.search_ready) {
                    showSection(semSection);
                    hideSection(disabledSection);
                    if (dot) {
                        dot.classList.remove('cap-warn', 'cap-off');
                        dot.title = `DB ready · ${caps.db_node_count ?? '?'} nodes`;
                    }
                    renderDestSelector(caps);
                } else {
                    hideSection(semSection);
                    showSection(disabledSection);
                    const reason = caps.reason || 'DB-backed search unavailable.';
                    disabledMsg.textContent = reason;
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
                    // Only surface the disabled-chat banner when DB+embedder
                    // are otherwise ready — otherwise the generic search
                    // banner already covers it.
                    if (caps.search_ready) {
                        showSection(chatDisabled);
                    } else {
                        hideSection(chatDisabled);
                    }
                }
            } catch (err) {
                state.capabilities = { db_ready: false, embedder_ready: false, search_ready: false, chat_ready: false };
                hideSection(semSection);
                hideSection(chatSection);
                hideSection(tourSection);
                showSection(tourDisabled);
                markSubtabAvailability(state.capabilities);
                showSection(disabledSection);
                disabledMsg.textContent = 'Capabilities probe failed — server unreachable?';
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
            set('search', !!(caps && caps.search_ready));
            set('tour', !!(caps && caps.search_ready));
            set('chat', !!(caps && caps.chat_ready));
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
            const input = document.createElement('input');
            if (k.secret) {
                input.type = 'password';
                input.autocomplete = 'new-password';
            } else if (meta.num) {
                input.type = 'number';
                input.step = meta.num.step;
                if (meta.num.min !== undefined) input.min = meta.num.min;
                if (meta.num.max !== undefined) input.max = meta.num.max;
            } else {
                input.type = 'text';
                input.spellcheck = false;
            }
            // Secrets are never echoed back into the field — the server
            // only sends a masked preview, shown as the placeholder.
            if (!k.secret && k.saved != null) input.value = k.saved;
            if (k.secret) {
                input.placeholder = k.saved != null ? 'saved · ' + k.saved + ' — type to replace' : 'not set';
            } else {
                input.placeholder = k.default != null ? 'default: ' + k.default : 'not set';
            }
            wrap.appendChild(input);

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

            input.addEventListener('input', () => {
                if (settingsUi.unsets.has(k.name)) {
                    settingsUi.unsets.delete(k.name);
                    row.classList.remove('pending-unset');
                }
                const val = input.value.trim();
                const baseline = k.secret ? '' : (k.saved != null ? String(k.saved) : '');
                if (val !== baseline) settingsUi.edits.set(k.name, val);
                else settingsUi.edits.delete(k.name);
                row.classList.toggle('dirty', settingsUi.edits.has(k.name));
                updateSettingsFooter();
            });

            clearBtn.addEventListener('click', () => {
                if (settingsUi.unsets.has(k.name)) {
                    // Undo the pending clear.
                    settingsUi.unsets.delete(k.name);
                    row.classList.remove('pending-unset');
                    if (!k.secret && k.saved != null) input.value = k.saved;
                } else {
                    settingsUi.unsets.add(k.name);
                    settingsUi.edits.delete(k.name);
                    row.classList.remove('dirty');
                    row.classList.add('pending-unset');
                    if (!k.secret) input.value = k.saved != null ? k.saved : '';
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
            const touchedEmbed = Object.keys(set).concat(unset).some((n) => n.startsWith('embed.'));
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
                setSettingsStatus('ok', touchedEmbed
                    ? 'Saved ✓ — chat applies now; embedding changes take effect after a server restart'
                    : 'Saved ✓ — applied immediately');
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

