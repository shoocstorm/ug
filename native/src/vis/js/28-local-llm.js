        // ─── A model running in this browser (wllama) ──────────────────
        //
        // The server has no model of its own unless you gave it one. This
        // panel is the other way in: download a GGUF once, run it here with
        // llama.cpp-compiled-to-wasm, and hand it to the server as its chat
        // endpoint (`POST /api/llm/local/attach`). From then on Answer,
        // Tours and Walks are generated in this tab — same prompts, same
        // tools, same citations, no API key.
        //
        // The server pushes each generation down an SSE stream and we post
        // the tokens back (`serve/local_llm.rs` has the other half). Two
        // things follow from that and both are visible in the code below:
        //
        //   * This tab is the provider. Closing it stops the model, so the
        //     stream is reconnected on every `open` and the model re-attached
        //     — a reconnect the server saw as a disconnect would otherwise
        //     leave chat pointing at nothing.
        //   * A browser model is small. `max_tokens` and the retrieval budget
        //     are clamped server-side from the context window we report here,
        //     which is why `n_ctx` is part of the attach payload rather than
        //     a local detail.

        const LLM_PREFS_KEY = 'ug.localLlm.v1';

        // Curated so that every entry actually works in a browser: an
        // instruction-tuned GGUF with a chat template (tools need one), and
        // under the 2 GB single-file ceiling wasm imposes — see the note in
        // the "any GGUF" section of the panel.
        const LLM_CATALOG = [
            {
                id: 'smollm2-135m',
                name: 'SmolLM2 135M',
                repo: 'unsloth/SmolLM2-135M-Instruct-GGUF',
                file: 'SmolLM2-135M-Instruct-Q8_0.gguf',
                quant: 'Q8_0',
                bytes: 138 * 1024 * 1024,
                tools: false,
                sanity: true,
                blurb: 'Downloads in seconds and answers instantly — there to prove the plumbing works, not to be useful. Expect confident nonsense.',
            },
            {
                id: 'qwen3-0.6b',
                name: 'Qwen3 0.6B',
                repo: 'Qwen/Qwen3-0.6B-GGUF',
                file: 'Qwen3-0.6B-Q8_0.gguf',
                quant: 'Q8_0',
                bytes: 610 * 1024 * 1024,
                // It has a tool-calling template and cannot use it: given the
                // toolbox it writes `<search>{...}</search>` into the answer
                // instead of calling anything. It answers better from ug's own
                // retrieval, so that is what it gets — overridable under
                // Advanced.
                tools: false,
                blurb: 'Quickest to download and the lightest to run. Narrates tours well. Answers from ug\u2019s retrieval rather than driving the graph tools itself — at this size that is the better trade.',
            },
            {
                id: 'qwen3-1.7b-q4',
                name: 'Qwen3 1.7B',
                repo: 'unsloth/Qwen3-1.7B-GGUF',
                file: 'Qwen3-1.7B-Q4_K_M.gguf',
                quant: 'Q4_K_M',
                bytes: 1056 * 1024 * 1024,
                tools: true,
                recommended: true,
                blurb: 'The balance point: follows the graph tools properly and still answers at a readable speed on a laptop.',
            },
            {
                id: 'qwen3-1.7b-q8',
                name: 'Qwen3 1.7B',
                repo: 'unsloth/Qwen3-1.7B-GGUF',
                file: 'Qwen3-1.7B-Q8_0.gguf',
                quant: 'Q8_0',
                bytes: 1749 * 1024 * 1024,
                tools: true,
                blurb: 'Same model, barely quantized — the best answers that still fit in a browser tab, at roughly half the speed.',
            },
        ];

        // Every Qwen3 up to 4B has 28 layers and 8 KV heads of 128, so one
        // token of context costs the same 112 KiB whatever the size. Worth
        // showing: at 4096 tokens that is another half a gigabyte of tab, on
        // top of the weights, and it is the usual reason a load fails.
        const LLM_KV_BYTES_PER_TOKEN = 112 * 1024;

        const llm = {
            caps: null,
            runtime: null,
            wllama: null,
            models: null,
            cached: new Map(),
            entry: null,
            clientId: null,
            events: null,
            attached: false,
            busy: false,
            phase: 'off',
            detail: '',
            queue: Promise.resolve(),
            running: null,
            cancelled: new Set(),
            custom: [],
            prefs: { nCtx: 8192, gpu: true, think: false, auto: true, last: null, tools: {} },
        };

        function llmSupported() {
            return !!(llm.caps && llm.caps.supported && llm.caps.runtime);
        }

        function llmEl(id) {
            return document.getElementById(id);
        }

        function llmBytes(n) {
            if (!n && n !== 0) return '—';
            if (n >= 1024 * 1024 * 1024) return (n / 1024 / 1024 / 1024).toFixed(2) + ' GB';
            return Math.round(n / 1024 / 1024) + ' MB';
        }

        function llmModelUrl(entry) {
            return `https://huggingface.co/${entry.repo}/resolve/main/${entry.file}`;
        }

        function llmLoadPrefs() {
            try {
                const raw = localStorage.getItem(LLM_PREFS_KEY);
                if (raw) {
                    const saved = JSON.parse(raw);
                    Object.assign(llm.prefs, saved.prefs || {});
                    llm.custom = Array.isArray(saved.custom) ? saved.custom : [];
                }
            } catch (_) { /* a browser with storage off still gets the feature */ }
        }

        function llmSavePrefs() {
            try {
                localStorage.setItem(LLM_PREFS_KEY, JSON.stringify({
                    prefs: llm.prefs,
                    custom: llm.custom,
                }));
            } catch (_) { /* ignore */ }
        }

        /// Whether this model should be offered ug's graph tools. The catalog
        /// sets the default — some models have no tool template, others have
        /// one they cannot use — and the checkbox under Advanced overrides it.
        function llmToolsWanted(entry) {
            if (!entry) return false;
            const saved = llm.prefs.tools && llm.prefs.tools[entry.id];
            return saved === undefined ? entry.tools !== false : !!saved;
        }

        function llmEntries() {
            return LLM_CATALOG.concat(llm.custom);
        }

        function llmFindEntry(id) {
            return llmEntries().find(e => e.id === id) || null;
        }

        // ── the panel ──────────────────────────────────────────────────

        function llmOverlayEl() {
            return llmEl('llm-overlay');
        }

        function llmOpen() {
            llmOverlayEl().classList.add('visible');
            llmRender();
            llmRefreshCache();
        }

        function llmClose() {
            llmOverlayEl().classList.remove('visible');
        }

        function llmSetPhase(phase, detail) {
            llm.phase = phase;
            llm.detail = detail || '';
            // A `ug gen` page opened from disk, or the published demo, has no
            // server to attach to. Offering the button there would be an
            // invitation to a dead end.
            const button = llmEl('llm-open-btn');
            if (button) {
                button.hidden = phase === 'unsupported';
                button.classList.toggle('live', llm.attached);
                button.title = llm.attached
                    ? `${llm.entry ? llm.entry.name : 'A model'} is running in this browser — click to manage it`
                    : 'Run a language model in this browser';
            }
            llmRenderStatus();
            llmRenderGuidance();
            llmApplyInputCaps();
            llmSyncBadge();
        }

        function llmProgress(fraction, label) {
            const wrap = llmEl('llm-progress');
            if (!wrap) return;
            if (fraction === null) {
                wrap.hidden = true;
                return;
            }
            wrap.hidden = false;
            llmEl('llm-bar-fill').style.width = `${Math.round(fraction * 100)}%`;
            llmEl('llm-progress-label').textContent = label || '';
            llmEl('llm-progress-pct').textContent = `${Math.round(fraction * 100)}%`;
        }

        function llmRenderStatus() {
            const dot = llmEl('llm-status-dot');
            const text = llmEl('llm-status-text');
            const stop = llmEl('llm-stop');
            const facts = llmEl('llm-facts');
            if (!dot || !text) return;

            const label = llm.entry ? `${llm.entry.name} · ${llm.entry.quant}` : '';
            const states = {
                off: ['', 'No model running here. Pick one below — it downloads once.'],
                unsupported: ['warn', 'This page can’t run a model — open UltraGraph over http, not as a file.'],
                downloading: ['busy', `Downloading ${label}…`],
                loading: ['busy', `Starting ${label}…`],
                live: ['live', `${label} is answering for this project`],
                working: ['live', `${label} — generating…`],
                error: ['error', llm.detail || 'Something went wrong'],
            };
            const [kind, fallback] = states[llm.phase] || states.off;
            dot.className = `llm-dot ${kind}`;
            text.textContent = llm.phase === 'working' && llm.running
                ? `${label} — ${llm.running.tokens} tokens · ${llmRate()} tok/s`
                : (llm.detail && llm.phase !== 'error' ? llm.detail : fallback);
            stop.hidden = !(llm.attached || llm.busy);
            stop.textContent = llm.busy && !llm.attached ? 'Cancel' : 'Stop using it';

            if (facts) {
                if (llm.attached && llm.wllama) {
                    const gpu = llm.backend === 'webgpu';
                    facts.hidden = false;
                    const tools = llm.caps && llm.caps.attached && llm.caps.attached.tools;
                    facts.innerHTML = [
                        ['runs on', gpu ? 'the GPU (WebGPU)' : `the CPU · ${llm.threads || '?'} threads`],
                        ['context window', `${llm.prefs.nCtx} tokens`],
                        ['graph tools', tools ? `${tools} of them` : 'off — answers use ug\u2019s retrieval'],
                        ['answered here', llm.served ? `${llm.served} this session` : 'nothing yet'],
                    ].map(([k, v]) => `<span>${escapeHtml(k)} <b>${escapeHtml(String(v))}</b></span>`).join('');
                } else {
                    facts.hidden = true;
                }
            }
        }

        function llmRate() {
            if (!llm.running || !llm.running.tokens) return '0.0';
            const secs = (performance.now() - llm.running.started) / 1000;
            return secs > 0 ? (llm.running.tokens / secs).toFixed(1) : '0.0';
        }

        function llmRenderEnv() {
            const host = llmEl('llm-env');
            if (!host) return;
            const isolated = typeof crossOriginIsolated !== 'undefined' && crossOriginIsolated;
            const gpu = 'gpu' in navigator;
            const opfs = !!(navigator.storage && navigator.storage.getDirectory);
            const threads = navigator.hardwareConcurrency || 1;

            const chip = (ok, text, title) =>
                `<span class="llm-chip ${ok ? 'ok' : 'off'}" title="${escapeHtml(title)}">${ok ? '✓' : '✗'} ${escapeHtml(text)}</span>`;

            let html = [
                chip(gpu, 'WebGPU', gpu
                    ? 'Layers can be offloaded to the GPU — several times faster.'
                    : 'No WebGPU here, so the model runs on the CPU.'),
                chip(isolated, isolated ? `${threads} threads` : 'single thread', isolated
                    ? 'Cross-origin isolated, so the wasm can use every core.'
                    : 'Without SharedArrayBuffer the model runs on one core — around nine times slower.'),
                chip(opfs, 'model storage', opfs
                    ? 'Downloaded models are kept in this browser’s private storage and reused.'
                    : 'No OPFS: the model would have to be downloaded again every time.'),
            ].join('');

            // Counted from the models we can see rather than from
            // `storage.estimate().usage`, which Chrome updates lazily and
            // which read 0 MB with a model downloaded and running.
            const stored = [...llm.cached.values()].reduce((a, b) => a + b, 0);
            const quota = llm.storage && llm.storage.quota;
            html += `<span class="llm-chip" title="Models kept in this browser's private storage, and what it will let you keep">`
                + `${stored ? llmBytes(stored) + ' of models stored' : 'nothing downloaded yet'}`
                + `${quota ? ' · ' + llmBytes(quota) + ' available' : ''}</span>`;
            host.innerHTML = html;
        }

        function llmRenderModels() {
            const host = llmEl('llm-models');
            if (!host) return;
            host.innerHTML = '';
            for (const entry of llmEntries()) {
                const url = llmModelUrl(entry);
                const cachedBytes = llm.cached.get(url);
                const isCurrent = llm.entry && llm.entry.id === entry.id;
                const li = document.createElement('li');
                li.className = 'llm-model' + (isCurrent && llm.attached ? ' active' : '');

                const badges = [];
                if (entry.recommended) badges.push('<span class="llm-badge rec">recommended</span>');
                if (entry.sanity) badges.push('<span class="llm-badge">try this first</span>');
                if (cachedBytes) badges.push(`<span class="llm-badge cached">downloaded</span>`);
                if (isCurrent && llm.attached) badges.push('<span class="llm-badge live">running</span>');
                if (entry.custom) badges.push('<span class="llm-badge">yours</span>');

                li.innerHTML = `
                    <div class="llm-model-head">
                        <span class="llm-model-name">${escapeHtml(entry.name)}</span>
                        <span class="llm-model-quant">${escapeHtml(entry.quant || '')}</span>
                        ${badges.join('')}
                        <span class="llm-model-size">${cachedBytes ? llmBytes(cachedBytes) : '≈ ' + llmBytes(entry.bytes)}</span>
                    </div>
                    <div class="llm-model-blurb">${escapeHtml(entry.blurb || entry.repo + '/' + entry.file)}</div>
                    <div class="llm-model-actions"></div>`;

                const actions = li.querySelector('.llm-model-actions');
                const button = (label, cls, fn, disabled) => {
                    const b = document.createElement('button');
                    b.type = 'button';
                    b.className = `settings-btn ${cls}`;
                    b.textContent = label;
                    b.disabled = !!disabled;
                    b.addEventListener('click', fn);
                    actions.appendChild(b);
                    return b;
                };

                if (isCurrent && llm.attached) {
                    button('Stop using it', 'ghost', () => llmStop());
                } else {
                    button(
                        cachedBytes ? 'Use this model' : `Download & use · ${llmBytes(entry.bytes)}`,
                        'primary',
                        () => llmStart(entry),
                        llm.busy || !llmSupported(),
                    );
                }
                if (cachedBytes && !(isCurrent && llm.attached)) {
                    button('Delete', 'ghost danger', () => llmDelete(entry), llm.busy);
                }
                if (entry.custom) {
                    button('Remove', 'ghost', () => {
                        llm.custom = llm.custom.filter(e => e.id !== entry.id);
                        llmSavePrefs();
                        llmRenderModels();
                    }, llm.busy);
                }
                host.appendChild(li);
            }
        }

        /// The window is the one setting with a consequence people meet the
        /// hard way: ug's graph toolbox is ~10 000 tokens of JSON schema, so
        /// below a certain window there is no room for it and answers fall
        /// back to seeded retrieval. The server publishes that threshold.
        function llmRenderCtxHint() {
            const hint = llmEl('llm-ctx-hint');
            if (!hint) return;
            const kv = llm.prefs.nCtx * LLM_KV_BYTES_PER_TOKEN;
            const min = llmToolsMinCtx();
            const tools = llm.prefs.nCtx >= min
                ? 'The model can drive the graph tools itself at this size.'
                : `Below ${min} tokens there is no room for the graph tools, so answers are built from `
                  + `ug's own retrieval instead — still grounded and cited, just not self-directed.`;
            hint.textContent =
                `${tools} Costs about ${llmBytes(kv)} of memory on top of the model itself. `
                + `Changing it takes effect the next time a model starts.`;
        }

        /// The retrieval controls in the Ask column, and the budget key each
        /// one is bounded by.
        const LLM_CAPPED_INPUTS = [
            ['chat-k', 'chat_k'],
            ['sem-k', 'results'],
            ['chat-hops', 'hops'],
            ['tour-stops', 'stops'],
        ];

        /// Hold the retrieval knobs to what the running model's window can
        /// actually carry.
        ///
        /// The server clamps to the same numbers, so this is not what makes
        /// the answer fit — it is what stops the panel from showing `k=8`
        /// while the turn quietly uses 3. Originals are remembered on the
        /// element so stopping the model gives them straight back.
        function llmApplyInputCaps() {
            const limits = llm.caps && llm.caps.attached && llm.caps.attached.ui_limits;
            for (const [id, key] of LLM_CAPPED_INPUTS) {
                const input = llmEl(id);
                if (!input) continue;
                if (input.dataset.llmMax === undefined) {
                    input.dataset.llmMax = input.max || '';
                    input.dataset.llmTitle = input.title || '';
                }
                const cap = limits && limits[key];
                if (cap == null) {
                    input.max = input.dataset.llmMax;
                    input.title = input.dataset.llmTitle;
                    input.classList.remove('llm-capped');
                    continue;
                }
                input.max = String(cap);
                if (Number(input.value) > cap) input.value = String(cap);
                input.classList.add('llm-capped');
                input.title = `Capped at ${cap} — that is what fits in the `
                    + `${llm.prefs.nCtx}-token window of ${llm.entry ? llm.entry.name : 'the local model'}. `
                    + 'Raise the context window in the model panel for more.';
            }
        }

        function llmToolsMinCtx() {
            return (llm.caps && llm.caps.limits && llm.caps.limits.tools_min_n_ctx) || 8192;
        }

        /// Annotate the window picker with what each size buys.
        function llmRenderCtxOptions() {
            const select = llmEl('llm-ctx');
            if (!select) return;
            const min = llmToolsMinCtx();
            for (const opt of select.options) {
                const n = Number(opt.value);
                const gb = llmBytes(n * LLM_KV_BYTES_PER_TOKEN);
                opt.textContent = `${n} tokens · ${gb} memory · ${n >= min ? 'graph tools on' : 'no graph tools'}`;
            }
        }

        /// The panel says different things before and after a model is up:
        /// what this will cost you, then what to do with it.
        function llmRenderGuidance() {
            const steps = llmEl('llm-steps');
            if (steps) steps.hidden = llm.attached || llm.busy;
            const next = llmEl('llm-goto-ask');
            if (next) next.hidden = !llm.attached;
            const foot = llmEl('llm-foot-status');
            if (foot) {
                foot.textContent = llm.attached
                    ? 'Answer, Tours and Walks are now generated in this tab.'
                    : 'While a model runs here, Answer, Tours and Walks are generated in this tab.';
            }
        }

        function llmRender() {
            llmRenderStatus();
            llmRenderGuidance();
            llmRenderCtxOptions();
            llmRenderEnv();
            llmRenderModels();
            llmRenderCtxHint();
            const ctx = llmEl('llm-ctx');
            if (ctx) ctx.value = String(llm.prefs.nCtx);
            const gpu = llmEl('llm-gpu');
            if (gpu) gpu.checked = !!llm.prefs.gpu;
            const think = llmEl('llm-think');
            if (think) think.checked = !!llm.prefs.think;
            const auto = llmEl('llm-auto');
            if (auto) auto.checked = !!llm.prefs.auto;
            const tools = llmEl('llm-tools');
            if (tools) {
                const entry = llm.entry || llmFindEntry(llm.prefs.last);
                tools.checked = llmToolsWanted(entry);
                tools.disabled = !entry;
                const min = llmToolsMinCtx();
                const hint = llmEl('llm-tools-hint');
                if (hint) {
                    hint.textContent = llm.prefs.nCtx < min
                        ? `Off whatever this says while the window is under ${min} tokens — there is no room for the schemas.`
                        : 'A model that is bad at tool calling answers better without them: ug retrieves for it instead.';
                }
            }
        }

        /// Keep the two always-visible affordances honest: the dot on the
        /// sidebar button and the model pill in the Ask column.
        function llmSyncBadge() {
            const dot = llmEl('llm-btn-dot');
            if (dot) {
                dot.hidden = !(llm.attached || llm.busy);
                dot.className = 'llm-btn-dot' + (llm.phase === 'working' ? ' working' : llm.busy ? ' busy' : '');
            }
            const row = llmEl('chat-model-badge');
            const pill = llmEl('chat-model-pill');
            if (row && pill && llm.attached && llm.entry) {
                row.hidden = false;
                pill.hidden = false;
                pill.classList.add('in-browser');
                pill.textContent = `in this browser · ${llm.entry.name}`;
                pill.title = 'This project’s answers are generated in this tab. Click to manage.';
            } else if (pill) {
                pill.classList.remove('in-browser');
            }
        }

        async function llmRefreshCache() {
            if (!llm.models) return;
            llm.cached.clear();
            try {
                for (const m of await llm.models.getModels()) {
                    if (m.size > 0) llm.cached.set(m.url, m.size);
                }
            } catch (err) {
                console.warn('local model cache listing failed', err);
            }
            try {
                if (navigator.storage && navigator.storage.estimate) {
                    llm.storage = await navigator.storage.estimate();
                }
            } catch (_) { /* optional */ }
            llmRenderModels();
            llmRenderEnv();
        }

        // ── loading a model ────────────────────────────────────────────

        async function llmRuntime() {
            if (llm.runtime) return llm.runtime;
            // Served from the ug binary itself (`/wllama/<version>/…`), so
            // this works on a machine that cannot reach a CDN.
            llm.runtime = await import(llm.caps.runtime.js);
            return llm.runtime;
        }

        async function llmStart(entry) {
            if (llm.busy) return;
            if (!llmSupported()) {
                llmSetPhase('unsupported');
                return;
            }
            llm.busy = true;
            llm.entry = entry;
            llm.served = 0;
            llmSetPhase('downloading', '');
            llmRenderModels();

            const started = performance.now();
            try {
                const { Wllama, LoggerWithoutDebug } = await llmRuntime();
                if (llm.wllama) {
                    await llm.wllama.exit().catch(() => {});
                    llm.wllama = null;
                }
                const wllama = new Wllama(
                    { default: llm.caps.runtime.wasm },
                    { parallelDownloads: 3, logger: LoggerWithoutDebug },
                );

                const useGpu = !!llm.prefs.gpu && 'gpu' in navigator;
                await wllama.loadModelFromUrl(llmModelUrl(entry), {
                    n_ctx: llm.prefs.nCtx,
                    n_gpu_layers: useGpu ? 999 : 0,
                    // Keep `<think>` in the text we stream so this file can
                    // fold it away itself; the default parser routes it to a
                    // field the stream does not carry.
                    reasoning_format: 'none',
                    progressCallback: ({ loaded, total }) => {
                        const secs = (performance.now() - started) / 1000;
                        const rate = secs > 0 ? loaded / secs : 0;
                        llmProgress(total ? loaded / total : 0,
                            `${llmBytes(loaded)} of ${llmBytes(total)} · ${llmBytes(rate)}/s`);
                    },
                });

                llmProgress(1, 'Starting the model…');
                llmSetPhase('loading');
                llm.wllama = wllama;
                llm.threads = wllama.getNumThreads ? wllama.getNumThreads() : null;
                llm.backend = useGpu && wllama.isSupportWebGPU && wllama.isSupportWebGPU() ? 'webgpu' : 'wasm';
                entry.hasTemplate = !!(wllama.getChatTemplate && wllama.getChatTemplate());

                await llmAttach();
                llm.prefs.last = entry.id;
                llmSavePrefs();
                llmProgress(null);
                llmSetPhase('live');
            } catch (err) {
                console.error('local model failed to start', err);
                llmProgress(null);
                llm.entry = null;
                llmSetPhase('error', llmReadableError(err));
            } finally {
                llm.busy = false;
                llmRenderModels();
                llmRefreshCache();
            }
        }

        function llmReadableError(err) {
            const msg = String((err && err.message) || err || 'unknown error');
            if (/out of memory|Cannot allocate|OOM/i.test(msg)) {
                return 'This browser ran out of memory. Try a smaller model, or a smaller context window under Advanced.';
            }
            if (/fetch|network|Failed to load/i.test(msg)) {
                return 'The download failed — check the connection, then try again. Partial downloads are resumed.';
            }
            if (/context size|exceeds the available context|kv_cache_full|n_ctx/i.test(msg)) {
                return `That prompt did not fit this model's ${llm.prefs.nCtx}-token window. `
                    + 'Raise the context window under Advanced in the model panel (bigger window, more memory), '
                    + 'or ask a narrower question.';
            }
            return msg;
        }

        async function llmStop() {
            if (llm.running) llm.running.ctrl.abort();
            try {
                await fetch('/api/llm/local/detach', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ client_id: llm.clientId }),
                });
            } catch (_) { /* the server drops it when the stream goes anyway */ }
            llm.attached = false;
            if (llm.wllama) {
                await llm.wllama.exit().catch(() => {});
                llm.wllama = null;
            }
            llm.entry = null;
            llm.prefs.last = null;
            llmSavePrefs();
            llmSetPhase('off');
            llmRenderModels();
            if (typeof probeCapabilities === 'function') probeCapabilities(true);
        }

        async function llmDelete(entry) {
            if (!llm.models) return;
            const url = llmModelUrl(entry);
            try {
                for (const m of await llm.models.getModels()) {
                    if (m.url === url) await m.remove();
                }
            } catch (err) {
                console.warn('could not delete model', err);
            }
            llmRefreshCache();
        }

        async function llmAttach() {
            const res = await fetch('/api/llm/local/attach', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    client_id: llm.clientId,
                    model: llm.entry.id,
                    label: `${llm.entry.name} ${llm.entry.quant || ''}`.trim(),
                    n_ctx: llm.prefs.nCtx,
                    supports_tools: llmToolsWanted(llm.entry) && llm.entry.hasTemplate !== false,
                    backend: llm.backend,
                }),
            });
            if (!res.ok) throw new Error(`the server refused the model (HTTP ${res.status})`);
            // The reply is the status payload, including the budget this
            // window bought — the panel and the retrieval controls both read
            // their numbers from it rather than guessing.
            llm.caps = await res.json().catch(() => llm.caps);
            llm.attached = true;
            // Banners, the model pill and the greyed-out modes all key off
            // capabilities, and chat only just became possible.
            if (typeof probeCapabilities === 'function') probeCapabilities(true);
        }

        // ── answering what the server asks ─────────────────────────────

        function llmConnect() {
            if (llm.events) llm.events.close();
            const src = new EventSource(`/api/llm/local/events?client=${encodeURIComponent(llm.clientId)}`);
            llm.events = src;

            src.addEventListener('open', () => {
                // A reconnect looks like a fresh tab to the server, which
                // detached the model when the old stream dropped. Put it back.
                if (llm.wllama && !llm.attached) {
                    llmAttach().then(() => llmSetPhase('live')).catch(err => console.warn('re-attach failed', err));
                }
            });
            src.addEventListener('job', (e) => {
                let job;
                try {
                    job = JSON.parse(e.data);
                } catch (err) {
                    return;
                }
                llm.queue = llm.queue.then(() => llmRunJob(job)).catch(err => console.warn('job failed', err));
            });
            src.addEventListener('cancel', (e) => {
                try {
                    const { id } = JSON.parse(e.data);
                    llm.cancelled.add(id);
                    if (llm.running && llm.running.id === id) llm.running.ctrl.abort();
                } catch (_) { /* ignore */ }
            });
            src.addEventListener('error', () => {
                // EventSource reconnects on its own; the model is only
                // considered attached again once `open` says so.
                llm.attached = false;
                if (llm.wllama) llmSetPhase('loading', 'Reconnecting to the server…');
            });
        }

        /// Messages arrive in OpenAI shape. wllama wants `content` to be a
        /// string (except on an assistant turn that is only tool calls), and
        /// anything else it does not recognise is dropped rather than risked.
        function llmMessages(raw) {
            return (raw || []).map(m => {
                const out = { role: m.role };
                if (m.role === 'assistant' && Array.isArray(m.tool_calls) && m.tool_calls.length) {
                    out.tool_calls = m.tool_calls;
                    out.content = m.content || '';
                } else {
                    out.content = typeof m.content === 'string' ? m.content : (m.content ? String(m.content) : '');
                }
                if (m.role === 'tool' && m.tool_call_id) out.tool_call_id = m.tool_call_id;
                return out;
            });
        }

        /// Strip `<think>…</think>` from a stream, incrementally.
        ///
        /// The model emits it inline (we load with `reasoning_format: 'none'`
        /// on purpose) and the server would otherwise show it as the answer.
        /// Returns the visible part of this piece.
        function llmThinkFilter() {
            let inside = false;
            let carry = '';
            return (piece) => {
                let text = carry + piece;
                carry = '';
                let out = '';
                while (text) {
                    if (inside) {
                        const end = text.indexOf('</think>');
                        if (end === -1) {
                            // Keep a tail that might be a split closing tag.
                            carry = text.slice(-8);
                            return out;
                        }
                        text = text.slice(end + 8);
                        inside = false;
                        continue;
                    }
                    const start = text.indexOf('<think>');
                    if (start === -1) {
                        const tail = text.slice(-7);
                        if (/<t?h?i?n?k?$/.test(tail)) {
                            carry = tail;
                            out += text.slice(0, text.length - tail.length);
                        } else {
                            out += text;
                        }
                        return out;
                    }
                    out += text.slice(0, start);
                    text = text.slice(start + 7);
                    inside = true;
                }
                return out;
            };
        }

        async function llmRunJob(job) {
            if (llm.cancelled.has(job.id)) {
                llm.cancelled.delete(job.id);
                return;
            }
            if (!llm.wllama) {
                await llmPost(`/api/llm/local/jobs/${job.id}/result`, {
                    error: 'the browser model was stopped before this request ran',
                });
                return;
            }

            const req = job.request || {};
            const wantsTools = Array.isArray(req.tools) && req.tools.length > 0;
            const ctrl = new AbortController();
            llm.running = { id: job.id, ctrl, tokens: 0, started: performance.now() };
            llmSetPhase('working');

            const params = {
                messages: llmMessages(req.messages),
                max_tokens: req.max_tokens || 512,
                temp: typeof req.temperature === 'number' ? req.temperature : 0.3,
                abortSignal: ctrl.signal,
                chat_template_kwargs: { enable_thinking: !!llm.prefs.think },
            };

            let content = '';
            let toolCalls = [];
            let finish = 'stop';
            let usage = null;

            try {
                if (wantsTools) {
                    // One shot, not streamed: tool calls arrive whole, and a
                    // half-assembled function argument is not something to
                    // reconstruct from deltas.
                    params.tools = req.tools;
                    if (req.tool_choice) params.tool_choice = req.tool_choice;
                    const res = await llm.wllama.createChatCompletion(params);
                    const choice = (res.choices || [])[0] || {};
                    content = (choice.message && choice.message.content) || '';
                    toolCalls = (choice.message && choice.message.tool_calls) || [];
                    finish = choice.finish_reason || 'stop';
                    usage = res.usage || null;
                    const visible = llmThinkFilter()(content);
                    if (visible) await llmPost(`/api/llm/local/jobs/${job.id}/delta`, { content: visible });
                    content = visible;
                } else {
                    const stream = await llm.wllama.createChatCompletion({ ...params, stream: true });
                    const strip = llmThinkFilter();
                    let pending = '';
                    let lastFlush = performance.now();
                    for await (const chunk of stream) {
                        const choice = (chunk.choices || [])[0] || {};
                        const piece = choice.delta && choice.delta.content;
                        if (chunk.usage) usage = chunk.usage;
                        if (choice.finish_reason) finish = choice.finish_reason;
                        if (!piece) continue;
                        llm.running.tokens++;
                        const visible = strip(piece);
                        if (!visible) continue;
                        content += visible;
                        pending += visible;
                        // Batched: a POST per token would spend more time in
                        // HTTP than in inference.
                        if (performance.now() - lastFlush > 120) {
                            await llmPost(`/api/llm/local/jobs/${job.id}/delta`, { content: pending });
                            pending = '';
                            lastFlush = performance.now();
                            llmRenderStatus();
                        }
                    }
                    if (pending) await llmPost(`/api/llm/local/jobs/${job.id}/delta`, { content: pending });
                }

                await llmPost(`/api/llm/local/jobs/${job.id}/result`, {
                    content,
                    tool_calls: toolCalls,
                    finish_reason: finish,
                    usage,
                });
                llm.served = (llm.served || 0) + 1;
            } catch (err) {
                const aborted = ctrl.signal.aborted || /abort/i.test(String(err && err.message));
                await llmPost(`/api/llm/local/jobs/${job.id}/result`, {
                    error: aborted ? 'the answer was cancelled' : llmReadableError(err),
                });
                if (!aborted) console.error('local generation failed', err);
            } finally {
                llm.cancelled.delete(job.id);
                llm.running = null;
                llmSetPhase(llm.attached ? 'live' : 'off');
            }
        }

        function llmPost(url, body) {
            return fetch(url, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body),
            }).catch(err => console.warn('posting to the server failed', err));
        }

        // ── capabilities + boot ────────────────────────────────────────

        /// Called from `probeCapabilities` whenever the server's answer
        /// lands, including the re-probe that follows an attach.
        function llmOnCapabilities(caps) {
            llm.caps = (caps && caps.local_llm) || null;
            const serverSide = caps && caps.chat && caps.chat.in_browser;
            // Another tab is serving. Say so rather than offering a second
            // model that would silently take over the first one's job.
            if (serverSide && !llm.attached && !llm.busy) {
                llmSetPhase('live', `${(caps.chat.label || 'A model')} is running in another tab`);
                llm.foreign = true;
            } else if (!serverSide && llm.foreign) {
                llm.foreign = false;
                llmSetPhase('off');
            }
            if (!llmSupported() && !llm.busy) llmSetPhase('unsupported');
            if (llmOverlayEl() && llmOverlayEl().classList.contains('visible')) llmRender();
            llmApplyInputCaps();
            llmSyncBadge();
            llmMaybeAutoStart();
        }

        let llmAutoStarted = false;
        function llmMaybeAutoStart() {
            if (llmAutoStarted || !llm.prefs.auto || !llm.prefs.last) return;
            if (!llmSupported() || llm.busy || llm.attached || llm.foreign) return;
            const entry = llmFindEntry(llm.prefs.last);
            if (!entry) return;
            llmAutoStarted = true;
            // Only when the weights are already here: a page load must never
            // start a gigabyte-sized download on its own.
            llmRefreshCache().then(() => {
                if (llm.cached.has(llmModelUrl(entry))) llmStart(entry);
            });
        }

        function llmAddCustom() {
            const repo = llmEl('llm-custom-repo').value.trim().replace(/^https?:\/\/huggingface\.co\//, '').replace(/\/+$/, '');
            const file = llmEl('llm-custom-file').value.trim();
            if (!repo || !file) return;
            if (!/^[\w.-]+\/[\w.-]+$/.test(repo) || !file.endsWith('.gguf')) {
                llmSetPhase('error', 'That does not look like a Hugging Face repo and a .gguf file.');
                return;
            }
            const entry = {
                id: `hf:${repo}/${file}`,
                name: repo.split('/').pop().replace(/-GGUF$/i, ''),
                repo,
                file,
                quant: (file.match(/(IQ\d\w*|Q\d[_\w]*|BF16|F16)/i) || [''])[0],
                bytes: 0,
                tools: true,
                custom: true,
                blurb: `${repo} · ${file}`,
            };
            if (!llmFindEntry(entry.id)) {
                llm.custom.push(entry);
                llmSavePrefs();
            }
            llmEl('llm-custom-repo').value = '';
            llmEl('llm-custom-file').value = '';
            llmRenderModels();
        }

        (function initLocalLlm() {
            llmLoadPrefs();
            llm.clientId = `tab-${Math.random().toString(36).slice(2, 10)}`;

            const overlay = llmOverlayEl();
            if (!overlay) return;

            llmEl('llm-open-btn').addEventListener('click', llmOpen);
            llmEl('llm-close').addEventListener('click', llmClose);
            document.querySelectorAll('[data-open-llm]').forEach(b => b.addEventListener('click', llmOpen));
            const pill = llmEl('chat-model-pill');
            if (pill) pill.addEventListener('click', () => { if (llm.attached || llmSupported()) llmOpen(); });
            overlay.addEventListener('click', (e) => { if (e.target === overlay) llmClose(); });
            document.addEventListener('keydown', (e) => {
                if (e.key === 'Escape' && overlay.classList.contains('visible')) llmClose();
            });

            llmEl('llm-stop').addEventListener('click', () => {
                if (llm.busy && llm.running) llm.running.ctrl.abort();
                llmStop();
            });
            llmEl('llm-custom-add').addEventListener('click', llmAddCustom);
            llmEl('llm-goto-ask').addEventListener('click', () => {
                llmClose();
                const input = document.getElementById('ask-input');
                if (input) input.focus();
            });
            llmEl('llm-ctx').addEventListener('change', (e) => {
                llm.prefs.nCtx = Number(e.target.value) || 4096;
                llmSavePrefs();
                llmRenderCtxHint();
            });
            llmEl('llm-tools').addEventListener('change', async (e) => {
                const entry = llm.entry || llmFindEntry(llm.prefs.last);
                if (!entry) return;
                llm.prefs.tools = llm.prefs.tools || {};
                llm.prefs.tools[entry.id] = e.target.checked;
                llmSavePrefs();
                // Attaching again is cheap and takes effect on the next
                // question — no reload, no re-download.
                if (llm.attached) {
                    await llmAttach().catch(err => console.warn('re-attach failed', err));
                    llmRender();
                }
            });
            for (const [id, key] of [['llm-gpu', 'gpu'], ['llm-think', 'think'], ['llm-auto', 'auto']]) {
                llmEl(id).addEventListener('change', (e) => {
                    llm.prefs[key] = e.target.checked;
                    llmSavePrefs();
                });
            }

            // Closing the tab drops the SSE stream, which is what the server
            // watches — but a beacon makes the handover immediate rather than
            // waiting on a keepalive to fail.
            window.addEventListener('pagehide', () => {
                if (!llm.attached) return;
                try {
                    navigator.sendBeacon(
                        '/api/llm/local/detach',
                        new Blob([JSON.stringify({ client_id: llm.clientId })], { type: 'application/json' }),
                    );
                } catch (_) { /* the stream drop covers it */ }
            });

            // The model list needs the runtime's cache manager, which is only
            // worth importing where the server says the feature exists. The
            // capabilities probe calls back into `llmOnCapabilities`.
            fetch('/api/llm/local/status')
                .then(r => (r.ok ? r.json() : null))
                .then(async (status) => {
                    if (!status || !status.supported) {
                        llmSetPhase('unsupported');
                        return;
                    }
                    llm.caps = status;
                    const { ModelManager } = await llmRuntime();
                    llm.models = new ModelManager();
                    llmConnect();
                    await llmRefreshCache();
                    llmSetPhase('off');
                    llmMaybeAutoStart();
                })
                .catch(() => llmSetPhase('unsupported'));
        })();
