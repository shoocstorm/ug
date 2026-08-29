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
        // It also *publishes* what it fetched. `state.capabilities` used to be
        // assigned only by `probeCapabilities()`, which runs at the very end of
        // `initialize()` — after the solo-mode and renderer decisions have
        // already been taken. So every `vis.*` config key was read against its
        // built-in fallback and then ignored for the rest of the session:
        // `vis.solo_threshold` raised to 10,000,000 still put a 162k-node graph
        // into solo view, because the decision compared against the hardcoded
        // 200,000. Nothing re-runs that decision once the real value lands.
        // See P12.6 in docs/dev/PERF-TUNING-JOURNEY.md.
        let capabilitiesPromise = null;
        function getCapabilities() {
            if (!capabilitiesPromise) {
                capabilitiesPromise = fetch('/api/capabilities')
                    .then(res => (res.ok ? res.json() : null))
                    .then(caps => {
                        if (caps) state.capabilities = caps;
                        return caps;
                    })
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
            data: null,          // last /api/config payload
            edits: new Map(),    // key name → new raw string value
            unsets: new Set(),   // key names marked "clear on save"
            invalid: new Map(),  // key name → validation error on the current edit
        };

        // Which settings sections the user has collapsed, per browser.
        const COLLAPSED_KEY = 'ug-settings-collapsed';

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
            graph: {
                title: 'Graph',
                sub: 'How the browser gets the graph — whole file or server mode',
                badge: ['reload', 'applies on reload'],
                icon: '<circle cx="5" cy="12" r="2"/><circle cx="19" cy="12" r="2"/><circle cx="12" cy="6" r="2"/><circle cx="12" cy="18" r="2"/><path d="M5 12h5"/><path d="M14 12h5"/><path d="m10 7 2 3 4-1"/><path d="m10 17 2-3 4 1"/>',
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
                hint: 'Three.js draws in 3D, with depth, effects and one styling pass per node — pleasant on a small repo, unusable past a few thousand nodes. Cosmos draws in 2D, on the GPU, and holds graphs far too big for 3D. Auto adjusts by graph size: picks three below the 3D element budget and cosmos above it.',
            },
            'vis.three_d_max_elements': {
                label: '3D element budget',
                hint: 'How much the 3D engine is asked to draw whole. Above this many nodes or edges, auto switches to the 2D engine — and if you force 3D, solo mode takes over instead of rendering the whole graph. Raised, three.js gets more on screen and slower hovers; lowered, the 2D engine becomes the default sooner.',
                num: { step: '100', min: '100', max: '1000000' },
            },
            'vis.solo_threshold': {
                label: 'Solo mode threshold (2D engine)',
                hint: 'Past this many nodes or edges the page never draws the whole graph — it opens in solo mode and shows one neighbourhood at a time. Lower it to isolate large repos earlier, higher to attempt whole-graph render for longer. Governs the 2D engine only; the 3D engine solos past its own element budget above.',
                num: { step: '1000', min: '1', max: '10000000' },
                unit: 'count',
            },
            'vis.link_blending': {
                label: 'Link blending (2D engine)',
                selectLabels: {
                    on: 'On — strands blend where they cross',
                    off: 'Off — faster on a large display',
                },
                hint: 'How overlapping links are drawn. On, they add together, so a dense bundle glows and depth reads through the tangle. Off, each strand is drawn opaque over the last — flatter, and the single biggest saving there is at high resolution: a full redraw of a 746k-link graph costs 349 ms blended and 149 ms unblended at 3400×2000, which is most of the delay between clicking a node and seeing the result. It halves the link draw at any size, but on a smaller window that draw is not what you are waiting on, so the interaction barely changes. Filtered-out links stay hidden either way.',
            },
            'vis.hover_delay_ms': {
                label: 'Hover delay',
                hint: 'How long the pointer has to rest on a node before it is hovered. Moving from one node to another crosses everything in between, and each crossing is a highlight recalculation and a redraw of the whole canvas — work for nodes you were only travelling over. A short wait collapses the whole journey into one hover, where you stopped. 0 hovers immediately.',
                num: { step: '10', min: '0', max: '2000' },
                unit: 'ms',
            },
            'graph.server_mode_bytes': {
                label: 'Server mode threshold (graph.json size)',
                hint: 'How the browser gets the graph. Past this size of graph.json the page no longer downloads the file — it loads a slim node index and asks this server for edges and neighbourhoods on demand (server mode). Below it the whole file is served and the browser renders everything itself (local mode). 50 MB out of the box; lower it to keep big files out of the browser tab, raise it for graphs you want fully client-side. A graph served this way always opens in solo view (Visualization section), whatever the solo threshold says — its edges live on the server.',
                num: { step: '1000000', min: '1024', max: '1000000000' },
                unit: 'bytes',
            },
        };

        // ── Unit-scaled thresholds ─────────────────────────
        // Byte and element-count thresholds are stored as raw integers but
        // typed as "50 MB" / "200 K": a unit dropdown sits next to the
        // number input, and every read/write converts through `factor`.
        // `opt` labels the dropdown, `short` labels prose like the
        // "default: 50 MB" placeholder.
        const SETTINGS_UNITS = {
            bytes: [
                { opt: 'B', short: 'B', factor: 1 },
                { opt: 'KB', short: 'KB', factor: 1024 },
                { opt: 'MB', short: 'MB', factor: 1024 * 1024 },
                { opt: 'GB', short: 'GB', factor: 1024 * 1024 * 1024 },
            ],
            count: [
                { opt: '×1', short: '', factor: 1 },
                { opt: '×1K', short: 'K', factor: 1000 },
                { opt: '×1M', short: 'M', factor: 1000000 },
            ],
        };

        // Largest unit the raw value still fills at least once — so a
        // saved 52428800 presents as "50 MB", and 200000 as "200 ×1K".
        function pickSettingsUnit(kind, raw) {
            let best = SETTINGS_UNITS[kind][0];
            for (const u of SETTINGS_UNITS[kind]) if (raw >= u.factor) best = u;
            return best;
        }

        // toPrecision(10) keeps the round-trip through Math.round exact
        // for any double-width integer, so re-displaying a saved value
        // never marks the row dirty on open.
        function scaledSettingsThreshold(raw, unit) {
            return Number((raw / unit.factor).toPrecision(10));
        }

        function formatSettingsThreshold(kind, raw) {
            if (!Number.isFinite(raw) || raw < 0) return String(raw);
            const u = pickSettingsUnit(kind, raw);
            const v = scaledSettingsThreshold(raw, u);
            return u.short ? v + ' ' + u.short : String(v);
        }

        // The same value as prose: 2 decimals, not 10. The input-field
        // formatter above keeps full precision so re-displaying a saved
        // value never marks the row dirty; notes want "4.57 MB", not
        // "4.571556091 MB".
        function formatSettingsThresholdHuman(kind, raw) {
            if (!Number.isFinite(raw) || raw < 0) return String(raw);
            const u = pickSettingsUnit(kind, raw);
            const r = Math.round(scaledSettingsThreshold(raw, u) * 100) / 100;
            return u.short ? r + ' ' + u.short : String(r);
        }

        // ── Numeric row validation ────────────────────────
        // The `<input type=number>` bounds never fire here — Save is a
        // button, not a form submit — so the rules in `meta.num` are
        // enforced by hand. `rawVal` is already in stored units (the
        // number shown × the unit factor), and these bounds are too, so
        // the comparison needs no rescaling. They mirror the server's
        // write-time validation; an error here means the POST would have
        // been rejected anyway, but the row says why at the keystroke
        // instead of after it.
        function settingsNumError(meta, rawVal) {
            if (!meta.num || rawVal === '') return null;
            const n = Number(rawVal);
            if (!Number.isFinite(n)) return 'Enter a number.';
            const num = meta.num;
            if (num.min !== undefined && n < Number(num.min)) {
                return 'Must be at least ' + (meta.unit
                    ? formatSettingsThreshold(meta.unit, Number(num.min))
                    : num.min) + '.';
            }
            if (num.max !== undefined && n > Number(num.max)) {
                return 'Must be at most ' + (meta.unit
                    ? formatSettingsThreshold(meta.unit, Number(num.max))
                    : num.max) + '.';
            }
            return null;
        }

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
            settingsUi.invalid.clear();
            const reloadBtn = document.getElementById('settings-reload');
            if (reloadBtn) reloadBtn.hidden = true;
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
                const head = document.createElement('button');
                head.type = 'button';
                head.className = 'settings-group-head';
                head.title = 'Click to collapse / expand';
                head.innerHTML =
                    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' + meta.icon + '</svg>' +
                    '<span class="settings-group-title">' + escapeHtml(meta.title) + '</span>' +
                    '<span class="settings-group-sub">' + escapeHtml(meta.sub) + '</span>' +
                    '<span class="settings-apply-badge ' + meta.badge[0] + '">' + escapeHtml(meta.badge[1]) + '</span>' +
                    '<span class="settings-caret" aria-hidden="true"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg></span>';
                group.appendChild(head);
                const rows = document.createElement('div');
                rows.className = 'settings-rows';
                for (const k of keys) rows.appendChild(renderSettingsRow(k));
                group.appendChild(rows);
                applySettingsCollapsed(group, sec);
                head.addEventListener('click', () => toggleSettingsCollapsed(group, sec));
                body.appendChild(group);
            }
        }

        // Per-section collapse state, remembered across opens so a fat modal
        // reopens on the sections the user actually uses. Stored in
        // localStorage (a browser preference, not a server setting).
        function collapsedSections() {
            try {
                const raw = localStorage.getItem(COLLAPSED_KEY);
                return raw ? new Set(JSON.parse(raw)) : new Set();
            } catch (err) { return new Set(); }
        }

        function applySettingsCollapsed(group, sec) {
            if (!collapsedSections().has(sec)) return;
            group.classList.add('collapsed');
            const rows = group.querySelector('.settings-rows');
            if (rows) rows.hidden = true;
        }

        function toggleSettingsCollapsed(group, sec) {
            const closed = !group.classList.contains('collapsed');
            group.classList.toggle('collapsed', closed);
            const rows = group.querySelector('.settings-rows');
            if (rows) rows.hidden = closed;
            const set = collapsedSections();
            if (closed) set.add(sec); else set.delete(sec);
            try { localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...set])); } catch (err) { /* private mode */ }
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
            // Unit-scaled fields get a unit dropdown beside the number;
            // the stored value stays the raw integer.
            const units = !isEnum && meta.unit ? SETTINGS_UNITS[meta.unit] : null;
            let unitSel = null;
            if (units) {
                unitSel = document.createElement('select');
                unitSel.className = 'settings-unit';
                unitSel.title = 'Unit';
                for (const u of units) {
                    const opt = document.createElement('option');
                    opt.value = String(u.factor);
                    opt.textContent = u.opt;
                    unitSel.appendChild(opt);
                }
            }
            // Set a unit field from a raw stored value (saved, default or
            // the clear-button restore): pick the unit that presents it
            // best and write the scaled number into the input.
            const setUnitField = (raw) => {
                if (Number.isFinite(raw) && raw >= 0) {
                    const u = pickSettingsUnit(meta.unit, raw);
                    unitSel.value = String(u.factor);
                    control.value = String(scaledSettingsThreshold(raw, u));
                } else {
                    control.value = '';
                }
            };
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
                    // With a unit dropdown the min/max would have to be
                    // rescaled per unit — skip them and let the server
                    // validate the raw value instead.
                    control.step = units ? 'any' : meta.num.step;
                    if (!units) {
                        if (meta.num.min !== undefined) control.min = meta.num.min;
                        if (meta.num.max !== undefined) control.max = meta.num.max;
                    }
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
                    control.placeholder = k.default != null
                        ? 'default: ' + (units ? formatSettingsThreshold(meta.unit, Number(k.default)) : k.default)
                        : 'not set';
                }
                if (units && !k.secret) {
                    if (k.saved != null) {
                        setUnitField(Number(k.saved));
                    } else if (k.default != null && Number.isFinite(Number(k.default))) {
                        // No saved value — pre-select the default's unit so a
                        // typed number pairs with a sensible unit out of the box.
                        unitSel.value = String(pickSettingsUnit(meta.unit, Number(k.default)).factor);
                    }
                }
            }
            wrap.appendChild(control);
            if (unitSel) wrap.appendChild(unitSel);

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

            // ── Live "what this page is doing now" notes ──────
            // Both thresholds govern decisions the page already made, so
            // each row shows the decision's outcome for *this* graph — and,
            // while the value is edited, the outcome the typed value would
            // have. This is what stops the two standing confusions: editing
            // a threshold that server mode (or the other engine) has made
            // irrelevant, and reading solo view as "the server-mode
            // threshold didn't work" — the two are independent settings,
            // and the notes cross-reference each other when both matter.
            const capsGraph = state.capabilities && state.capabilities.graph;
            let updateLiveNote = null;
            // The value the row is currently pointing at: the typed edit if
            // there is one, else the saved value, else the default (which is
            // also what a pending clear restores).
            const rowCutoff = () => {
                if (settingsUi.unsets.has(k.name)) return Number(k.default);
                const v = settingsUi.edits.get(k.name);
                if (v != null && v !== '' && Number.isFinite(Number(v))) return Number(v);
                return k.saved != null ? Number(k.saved) : Number(k.default);
            };
            if (k.name === 'vis.solo_threshold' && capsGraph) {
                const live = document.createElement('div');
                live.className = 'settings-row-note settings-live-note';
                row.appendChild(live);
                const elements = Math.max(capsGraph.nodes || 0, capsGraph.edges || 0);
                updateLiveNote = () => {
                    if (state.graphMode === 'server') {
                        live.classList.add('settings-force-note');
                        live.textContent = 'This graph is served in server mode — the page opens in solo view whatever this threshold says. Raise the server-mode threshold (Graph section) above the graph\'s size to make this setting matter.';
                        return;
                    }
                    live.classList.remove('settings-force-note');
                    const cutoff = rowCutoff();
                    const solo = Number.isFinite(cutoff) && elements > cutoff;
                    live.innerHTML = 'This page is ' + (state.soloOnly ? 'in solo view' : 'drawing the whole graph')
                        + '. This graph has ' + formatNumber(elements) + ' elements (nodes or edges, whichever is more)'
                        + ' — with a threshold of <strong>' + formatNumber(cutoff) + '</strong> it '
                        + (solo
                            ? '<strong class="mode-server">opens in solo view</strong> on the next reload'
                            : '<strong class="mode-local">draws the whole graph</strong> on the next reload')
                        + '.';
                };
                updateLiveNote();
            }
            if (k.name === 'graph.server_mode_bytes' && capsGraph && typeof capsGraph.bytes === 'number') {
                const live = document.createElement('div');
                live.className = 'settings-row-note settings-live-note';
                row.appendChild(live);
                const size = formatSettingsThresholdHuman('bytes', capsGraph.bytes);
                updateLiveNote = () => {
                    const c = rowCutoff();
                    const server = Number.isFinite(c) && capsGraph.bytes >= c;
                    const pageServer = state.graphMode === 'server';
                    let html = 'This graph is <strong>' + size + '</strong> — with a threshold of <strong>'
                        + (Number.isFinite(c) ? formatSettingsThresholdHuman('bytes', c) : '?')
                        + '</strong> it loads <strong class="' + (server ? 'mode-server' : 'mode-local') + '">'
                        + (server ? 'server' : 'local') + ' mode</strong>'
                        + (server
                            ? ' (a slim node index; edges arrive from this server on demand)'
                            : ' (the whole file, rendered in the browser)')
                        + ' on the next reload';
                    if (server !== pageServer) {
                        html += ' — this page now: ' + (pageServer ? 'server' : 'local') + '; reload to switch';
                    }
                    html += '.';
                    if (!server && state.soloOnly) {
                        html += ' The page still opens in solo view — that is the solo threshold (Visualization section), a separate setting.';
                    }
                    live.innerHTML = html;
                };
                updateLiveNote();
            }

            // Validation message slot, filled by onDirty. Only numeric
            // rows can populate it, so only they carry the element.
            let rowError = null;
            if (meta.num) {
                rowError = document.createElement('div');
                rowError.className = 'settings-row-error';
                row.appendChild(rowError);
            }

            const clearInvalid = () => {
                settingsUi.invalid.delete(k.name);
                row.classList.remove('invalid');
                if (rowError) rowError.textContent = '';
            };

            const onDirty = () => {
                if (settingsUi.unsets.has(k.name)) {
                    settingsUi.unsets.delete(k.name);
                    row.classList.remove('pending-unset');
                }
                let val = control.value.trim();
                if (units) {
                    // Raw integer = displayed number × selected factor.
                    const n = parseFloat(val);
                    val = val !== '' && Number.isFinite(n)
                        ? String(Math.round(n * Number(unitSel.value)))
                        : '';
                }
                // A value the server would reject is caught here, at the
                // row, with the rule stated — not after Save round-trips.
                const err = settingsNumError(meta, val);
                if (err && !settingsUi.unsets.has(k.name)) {
                    settingsUi.invalid.set(k.name, err);
                    row.classList.add('invalid');
                    if (rowError) rowError.textContent = err;
                } else {
                    clearInvalid();
                }
                // For a select the shown default *is* a legal choice, so an
                // unset row baseline is the default rather than '' — leaving
                // it untouched must not mark it dirty.
                const baseline = k.secret ? '' : (k.saved != null ? String(k.saved) : (isEnum ? String(k.default || '') : ''));
                if (val !== baseline) settingsUi.edits.set(k.name, val);
                else settingsUi.edits.delete(k.name);
                row.classList.toggle('dirty', settingsUi.edits.has(k.name));
                if (updateLiveNote) updateLiveNote();
                updateSettingsFooter();
            };
            control.addEventListener('input', onDirty);
            control.addEventListener('change', onDirty);
            if (unitSel) unitSel.addEventListener('change', onDirty);

            clearBtn.addEventListener('click', () => {
                if (settingsUi.unsets.has(k.name)) {
                    // Undo the pending clear.
                    settingsUi.unsets.delete(k.name);
                    row.classList.remove('pending-unset');
                    if (!k.secret && k.saved != null) {
                        if (units) setUnitField(Number(k.saved));
                        else control.value = k.saved;
                    }
                } else {
                    settingsUi.unsets.add(k.name);
                    settingsUi.edits.delete(k.name);
                    row.classList.remove('dirty');
                    row.classList.add('pending-unset');
                    if (!k.secret) {
                        if (units) setUnitField(k.saved != null ? Number(k.saved) : NaN);
                        else control.value = k.saved != null ? k.saved : (isEnum ? (k.default || '') : '');
                    }
                }
                // Whatever the clear restored is a saved value or a blank —
                // both legal by construction.
                clearInvalid();
                if (updateLiveNote) updateLiveNote();
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
            const invalid = settingsUi.invalid.size;
            // An invalid edit is still an edit — Revert stays available so
            // the user can back out of it — but Save waits until every rule
            // passes, and the status line says what is blocking.
            document.getElementById('settings-save').disabled = count === 0 || invalid > 0;
            document.getElementById('settings-revert').disabled = count === 0;
            const status = document.getElementById('settings-status');
            if (invalid > 0) {
                const [first] = settingsUi.invalid.values();
                setSettingsStatus('err', first);
            } else if (count > 0) {
                setSettingsStatus('', count + ' unsaved change' + (count === 1 ? '' : 's'));
            } else if (!status.classList.contains('ok') && !status.classList.contains('err')) {
                // Only clear neutral "N unsaved changes" text — keep a
                // just-shown save result visible.
                setSettingsStatus('', '');
            }
        }

        async function saveSettings() {
            if (settingsUi.invalid.size) return;   // footer keeps Save disabled; guard anyway
            const saveBtn = document.getElementById('settings-save');
            const set = {};
            for (const [name, val] of settingsUi.edits) set[name] = val;
            const unset = Array.from(settingsUi.unsets);
            const touched = Object.keys(set).concat(unset);
            const touchedEmbed = touched.some((n) => n.startsWith('embed.'));
            // Both a vis.* drawing pref and a graph.* delivery pref are read
            // once at page load — the honest next step after saving one is a
            // reload, so offer the button right there.
            const touchedReload = touched.some((n) => n.startsWith('vis.') || n.startsWith('graph.'));
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
                const reloadBtn = document.getElementById('settings-reload');
                if (reloadBtn) reloadBtn.hidden = !touchedReload;
                setSettingsStatus('ok', touchedReload
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
                settingsUi.invalid.clear();
                renderSettings();
                updateSettingsFooter();
                setSettingsStatus('', '');
            });
            document.getElementById('settings-save').addEventListener('click', saveSettings);
            const reloadBtn = document.getElementById('settings-reload');
            if (reloadBtn) reloadBtn.addEventListener('click', () => window.location.reload());
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

