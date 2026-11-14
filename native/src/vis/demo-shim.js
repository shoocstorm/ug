/* ==========================================================================
   UltraGraph — static demo shim
   ==========================================================================

   `ug demo` publishes the *same* visualization page the local server serves,
   next to a `graph.json` snapshot and nothing else — no database, no vectors,
   no `ug serve` behind it. This file is the difference between those two
   deployments, and it is deliberately the *only* difference: nothing under
   `src/vis/js/` knows the demo exists, so the demo can never drift from the
   real app and the real app carries no demo branches.

   It is injected by `ug demo` as a plain `<script>` in `<head>`, above the
   page's own `<script type="module">`. Classic scripts run at parse time and
   modules are deferred, so the `fetch` patch below is installed before a
   single line of app code runs — which is the whole reason this can be a
   wrapper rather than a set of conditionals.

   Two jobs:

   1. **Be the server.** Answer the handful of endpoints the app probes at
      startup, and refuse the rest with one honest message. The app already
      degrades correctly when a backend is missing — that path exists for
      `ug gen --no-ingest` — so answering "not here" truthfully lands it in a
      state it was already designed for, instead of a wall of failed fetches.

   2. **Say so.** A degraded state the visitor cannot explain reads as a
      broken site. The badge, the intro card and the rewritten capability
      banners exist so every dark corner of the UI names the reason and
      points at the install.

   `ug demo` prepends a `window.UG_DEMO = {…}` manifest before this file, so
   everything project-specific (label, counts, where "Install ug" goes) is
   data rather than something to edit here.

   Do not put a literal closing script tag in this file — `ug demo` refuses
   to build if one appears, for the same reason `build.rs` refuses one in a
   `js/` part: it would truncate the page with no error in the browser.
   ========================================================================== */

(function () {
    'use strict';

    const DEMO = (window.UG_DEMO = Object.assign({
        label: 'a sample repository',
        project: 'demo',
        install: 'https://ultra-graph.web.app/#get-started',
        nodes: 0,
        edges: 0,
        // Where the in-browser model's runtime (wllama) lives in this folder,
        // as written by `ug demo`. Absent in demos published before the
        // feature — the model panel then stays hidden, exactly as it was.
        llm: null,
    }, window.UG_DEMO || {}));

    // One sentence, reused everywhere something is missing: the badge
    // tooltip, the capability probe's `reason` (which the search pane prints
    // verbatim), and the body of every refused request.
    const NOTE =
        'This is a published snapshot of an indexed repo — a graph file on a ' +
        'static host, with no ug server behind it. Anything that reads the ' +
        'local index (semantic search, guided tours, statistics, source ' +
        'preview) is off here. Install ug to get all of it on your own code.';

    /* ── 1. The static "server" ─────────────────────────────────────────── */

    const realFetch = window.fetch.bind(window);

    const jsonResponse = (body, status) => new Response(JSON.stringify(body), {
        status: status || 200,
        headers: { 'Content-Type': 'application/json' },
    });

    // POST bodies of the intercepted routes, as JSON (or null).
    function reqJson(init) {
        try {
            return init && init.body ? JSON.parse(init.body) : null;
        } catch (err) {
            return null;
        }
    }

    /* ── 1b. The in-browser model, without the server ─────────────────────
       *
       * `js/28-local-llm.js` runs a GGUF in this tab and hands it to the
       * *server* as its chat endpoint: attach, an SSE job feed, deltas and
       * results posted back, and a server-side RAG pipeline deciding what the
       * model is asked. A static host has none of that — so this section is
       * the whole loop, in this page:
       *
       *   /api/chat ──► keyword retrieval over graph.json ──► prompt
       *                                                        │ job
       *                                        fake EventSource ▼
       *                                            the tab's own wllama
       *                                                        │ deltas
       *        SSE `phase`/`context`/`delta`/`done` ◄── POST /jobs/:id/…
       *
       * The wire shapes mirror `serve/local_llm.rs` and `serve/chat_api.rs`
       * deliberately, so the page code that talks to the real server talks to
       * this unchanged — nothing under `src/vis/js/` knows the demo exists,
       * and it must stay that way.
       *
       * Honest limits, stated in the UI copy too: retrieval is keyword-only
       * (no vectors travel with a snapshot) and there are no graph tools (no
       * server to run them against). Answers cite `[#N]` from the pack, like
       * the real thing.
       */

    const LLM = {
        attached: null,          // what the page attached: {model,label,n_ctx,…}
        clients: new Map(),      // client_id -> fake event stream
        jobs: new Map(),         // job id -> {resolve, onDelta}
        nextJob: 1,
        index: null,             // lazy keyword index over graph.json
    };

    const DEMO_SYSTEM_PROMPT =
        'You are UltraGraph, a precise code assistant running inside a published ' +
        'static demo. You are given retrieved context items numbered [#1], [#2], ... ' +
        'produced by keyword search over the knowledge graph of this repository. ' +
        'Cite the supporting items inline using their bracketed numbers (e.g. ' +
        '"see [#2]"). Prefer concise, structured answers with code references and ' +
        'file paths when relevant. Answer the user\'s question using ONLY information ' +
        'present in those items when possible. If the answer is not in the context, ' +
        'say so plainly instead of guessing.';

    // Above the largest window the panel offers, so every option in the
    // context picker honestly reads "no graph tools": the demo has no server
    // to run tools against, at any window size. Mirrors `limits.tools_min_n_ctx`.
    const DEMO_TOOLS_MIN_N_CTX = 32768;

    function absUrl(path) {
        try {
            return new URL(path, window.location.href).href;
        } catch (err) {
            return path;
        }
    }

    // The prompt plan for an attached window — the toolless branch of
    // `LocalLlm::plan_prompt`, arithmetic included: the reserve for the
    // answer, the fixed overhead, the system prompt, and chars-per-token.
    // Tools never enter it, because there is nothing here for them to run.
    function demoPlan(nCtx) {
        const completion = Math.min(Math.max(Math.floor(nCtx / 4), 256), 4096);
        const budget = Math.max(0,
            nCtx - completion - 512 - Math.ceil(DEMO_SYSTEM_PROMPT.length / 3));
        const chars = Math.max(2000, budget * 3);
        return {
            completion_tokens: completion,
            context_chars: chars,
            // `UiLimits::from_context_chars`
            ui: {
                chat_k: Math.min(50, Math.max(2, Math.floor(chars / 900))),
                results: Math.min(50, Math.max(3, Math.floor(chars / 400))),
                hops: chars < 8000 ? 1 : 2,
                stops: Math.min(12, Math.max(3, Math.floor(chars / 1200))),
            },
        };
    }

    function llmStatus() {
        if (!DEMO.llm) return { supported: false };
        const a = LLM.attached;
        const plan = a ? demoPlan(a.n_ctx) : null;
        return {
            supported: true,
            runtime: {
                version: DEMO.llm.version,
                // Resolved against the page: wllama instantiates its worker
                // from a blob: URL, where a relative path has no useful base.
                js: absUrl(DEMO.llm.js),
                wasm: absUrl(DEMO.llm.wasm),
                // The build Safari runs. Absent in a demo published before
                // it shipped; the page then has only the default build.
                compat: DEMO.llm.compat ? {
                    worker: absUrl(DEMO.llm.compat.worker),
                    wasm: absUrl(DEMO.llm.compat.wasm),
                } : null,
            },
            limits: { tools_min_n_ctx: DEMO_TOOLS_MIN_N_CTX },
            attached: a ? {
                model: a.model,
                label: a.label,
                n_ctx: a.n_ctx,
                supports_tools: a.supports_tools,
                backend: a.backend,
                since: a.since,
                context_chars: plan.context_chars,
                tools: 0,
                tool_rounds: 0,
                ui_limits: plan.ui,
            } : null,
        };
    }

    // A one-line SSE block in the shape `readSseStream` parses.
    function demoSse(name, obj) {
        return 'event: ' + name + '\ndata: ' + JSON.stringify(obj) + '\n\n';
    }

    // ── keyword retrieval over the published graph ──

    const DEMO_STOPWORDS = new Set((
        'how does do did what where who whom why when which is are was were the a an of ' +
        'to in on for with without and or but it its this that these those can could ' +
        'should would shall will may might must i you he she it we they me him her us ' +
        'them my your our their be been being from at by as into about over under not ' +
        'no yes there here'
    ).split(' '));

    function demoTokens(query) {
        return String(query || '').toLowerCase().match(/[a-z0-9_]+/g) || [];
    }

    // The query as the retriever sees it: the tokens that carry meaning.
    function demoQueryTokens(query) {
        return demoTokens(query).filter(t => t.length >= 2 && !DEMO_STOPWORDS.has(t));
    }

    // Index graph.json once, on the first question. The page has already
    // parsed the same file for the canvas — a second parse is the cost of
    // the module boundary, and on a demo-sized graph it is a few MB.
    //
    // Docstrings are only indexed below this many nodes: a huge published
    // graph would otherwise double its own text again in lowercase copies,
    // and name+path matches still answer most questions there.
    const DEMO_MAX_DOC_NODES = 50000;

    async function demoIndex() {
        if (LLM.index) return LLM.index;
        const res = await realFetch('graph.json');
        if (!res.ok) throw new Error('graph.json could not be read: HTTP ' + res.status);
        const graph = await res.json();
        const withDocs = (graph.nodes || []).length <= DEMO_MAX_DOC_NODES;
        const nodes = [];
        for (const n of graph.nodes || []) {
            if (n.node_type === 'Folder') continue;
            const doc = withDocs ? String(n.docstring || '').trim() : '';
            nodes.push({
                id: n.id,
                name: String(n.name || ''),
                type: String(n.node_type || ''),
                file: String(n.file || ''),
                start: n.startLine || 0,
                end: n.endLine || 0,
                doc,
                lname: String(n.name || '').toLowerCase(),
                lq: String(n.qualifiedName || '').toLowerCase(),
                lfile: String(n.file || '').toLowerCase(),
                ldoc: doc.toLowerCase().slice(0, 2000),
            });
        }
        LLM.index = { nodes };
        return LLM.index;
    }

    // Score every node against the query tokens. Deterministic by
    // construction: scores are integers, ties break on id, no hashing.
    function demoRetrieve(index, query, k) {
        const tokens = demoQueryTokens(query);
        if (!tokens.length) return [];
        const hits = [];
        for (const n of index.nodes) {
            let score = 0;
            for (const t of tokens) {
                if (n.lname === t) score += 12;
                else if (n.lname.includes(t)) score += 6;
                else if (n.lq.includes(t)) score += 3;
                if (n.lfile.includes(t)) score += 2;
                if (n.ldoc && n.ldoc.includes(t)) score += 1;
            }
            if (score >= 4) hits.push({ n, score });
        }
        hits.sort((a, b) => (b.score - a.score) || (a.n.id < b.n.id ? -1 : a.n.id > b.n.id ? 1 : 0));
        return hits.slice(0, Math.max(1, k)).map(h => h.n);
    }

    // The pack the model reads, and the citations the reader clicks. Only
    // what is rendered is registered as a citation (§ the ledger rule: an
    // item the model was never shown must not appear in the source list).
    function demoRenderPack(nodes, maxChars) {
        const blocks = [];
        const citations = [];
        let used = 0;
        for (const n of nodes) {
            const loc = n.file + (n.start ? ':' + n.start + '-' + n.end : '');
            const desc = n.doc.slice(0, 400);
            const block = '[#' + (citations.length + 1) + '] ' + n.name
                + ' (' + n.type + ') — ' + loc + ' · keyword match\n'
                + (desc ? desc + '\n' : '');
            if (blocks.length && used + block.length > maxChars) break;
            used += block.length;
            blocks.push(block);
            citations.push({
                index: citations.length + 1,
                id: n.id,
                name: n.name,
                node_type: n.type,
                file: n.file,
                start_line: n.start || null,
                end_line: n.end || null,
                description: n.doc.slice(0, 200) || null,
                distance: null,
                hop: 0,
                snippet: null,
                matched_by: 'keyword',
            });
        }
        return { text: blocks.join(''), citations };
    }

    // Send one job to the tab's own model through the emulated event stream,
    // resolve with the result it posts back, and hand every posted delta to
    // `onDelta` as it arrives.
    function demoDispatchJob(request, onDelta) {
        return new Promise((resolve) => {
            const id = 'demo-job-' + (LLM.nextJob++);
            const client = LLM.attached && LLM.clients.get(LLM.attached.client_id);
            const settle = (payload) => {
                clearTimeout(timer);
                LLM.jobs.delete(id);
                resolve(payload);
            };
            // A generation that never ends holds the answer stream open; the
            // cancel also tells the tab to stop generating.
            const timer = setTimeout(() => {
                if (LLM.jobs.has(id)) {
                    client && client.emit('cancel', { id });
                    settle({ error: 'the model did not answer in time' });
                }
            }, 600000);
            if (!client) {
                settle({ error: 'the browser model is not connected' });
                return;
            }
            LLM.jobs.set(id, { resolve: settle, onDelta });
            client.emit('job', { id, request });
        });
    }

    // ── the routes ──

    const ROUTES = {
        // The header badge polls this. Answering it keeps the badge on a
        // deliberate "Live demo" state instead of flashing "Offline" — which
        // is what a visitor would read as "the site is down".
        '/healthz': () => new Response('ok', {
            status: 200,
            headers: { 'Content-Type': 'text/plain' },
        }),

        // `bootstrap()` shows the Knowledge Base Manager only for
        // `mode: "multi"`; anything else means one fixed graph and it goes
        // straight to `loadGraph()`. That is exactly the demo — one snapshot,
        // nothing to switch between — so this is the honest answer, not a
        // trick to skip a screen.
        '/api/projects': () => jsonResponse({ mode: 'single' }),

        // Every `false` here is true. The app's own capability handling then
        // drops the search pane to Keyword (which runs off graph.json, in the
        // browser, and genuinely works), and shows the banners rewritten
        // further down. The two exceptions: `chat_ready` and the `local_llm`
        // block, which light up the moment a visitor attaches a model in the
        // tab — that part of chat needs no server, because the tab *is* the
        // server (see 1b).
        '/api/capabilities': () => {
            const caps = {
                mode: 'demo',
                db_ready: false,
                embedder_ready: false,
                search_ready: false,
                chat_ready: false,
                reason: NOTE,
                project: { name: DEMO.project },
            };
            if (DEMO.llm) {
                caps.local_llm = llmStatus();
                if (LLM.attached) {
                    caps.chat_ready = true;
                    caps.chat = {
                        model: LLM.attached.model,
                        base_url: null,
                        in_browser: true,
                        label: LLM.attached.label,
                    };
                }
            }
            return jsonResponse(caps);
        },

        // What the model panel polls at boot. `supported: false` (a demo
        // published before the runtime shipped) keeps the panel's button
        // hidden, which is what the old 501 produced.
        '/api/llm/local/status': () => jsonResponse(llmStatus()),

        // The setup inspector under Ask. Same shape as the server's, saying
        // what this deployment actually does.
        '/api/chat/config': () => {
            const plan = LLM.attached ? demoPlan(LLM.attached.n_ctx) : null;
            return jsonResponse({
                retrieval: {
                    summary: 'Keyword search over the published graph snapshot, in this '
                        + 'tab — no server, no vector store. A model you download here '
                        + 'answers from what it finds.',
                    stages: [
                        {
                            label: 'Keyword match',
                            detail: 'Query tokens scored against node names, qualified names, '
                                + 'paths and docstrings in graph.json.',
                        },
                        {
                            label: 'Budgeted pack',
                            detail: 'Top items rendered as numbered [#N] context, sized to the '
                                + 'attached model\'s context window.',
                        },
                    ],
                    strategy: 'score',
                    backend: 'browser',
                    defaults: {
                        k: 8,
                        hops: 0,
                        max_context_chars: plan ? plan.context_chars : null,
                        seed: true,
                        think: false,
                        tool_rounds: 0,
                        tool_result_chars: 0,
                    },
                },
                system_prompt: DEMO_SYSTEM_PROMPT,
                tools: [],
            });
        },
    };

    // Routes whose path carries a parameter. Checked after the exact table.
    const PARAM_ROUTES = [
        // The page has a model loaded and offers it. Mirrors the server's
        // contract: the event stream must already be open (it is — the page
        // connects as soon as status says the feature exists).
        [/^\/api\/llm\/local\/attach$/, (m, init) => {
            const body = reqJson(init) || {};
            if (!DEMO.llm) return jsonResponse({ error: 'not published with this demo' }, 501);
            if (!body.client_id || !LLM.clients.has(body.client_id)) {
                return jsonResponse({
                    error: 'no event stream for this client_id — open /api/llm/local/events first',
                }, 400);
            }
            if (!body.n_ctx) return jsonResponse({ error: 'n_ctx must be non-zero' }, 400);
            LLM.attached = {
                model: String(body.model || 'model'),
                label: String(body.label || body.model || 'A model'),
                n_ctx: body.n_ctx,
                supports_tools: !!body.supports_tools,
                backend: String(body.backend || 'wasm'),
                client_id: body.client_id,
                since: Math.floor(Date.now() / 1000),
            };
            rewriteBanners();
            return jsonResponse(llmStatus());
        }],

        [/^\/api\/llm\/local\/detach$/, (m, init) => {
            const body = reqJson(init) || {};
            if (LLM.attached && (!body.client_id || body.client_id === LLM.attached.client_id)) {
                LLM.attached = null;
            }
            rewriteBanners();
            return jsonResponse(llmStatus());
        }],

        // Deltas and the final result of a generation, as the tab posts them.
        [/^\/api\/llm\/local\/jobs\/([^/]+)\/delta$/, (m, init) => {
            const job = LLM.jobs.get(m[1]);
            const body = reqJson(init) || {};
            if (job && job.onDelta && body.content) job.onDelta(body.content);
            return jsonResponse({ ok: !!job });
        }],
        [/^\/api\/llm\/local\/jobs\/([^/]+)\/result$/, (m, init) => {
            const job = LLM.jobs.get(m[1]);
            const body = reqJson(init) || {};
            if (job && job.resolve) job.resolve(body);
            return jsonResponse({ ok: !!job });
        }],

        // The ask pipeline, answered from this tab.
        [/^\/api\/chat$/, (m, init) => {
            const body = reqJson(init) || {};
            if (!String(body.query || '').trim()) {
                return jsonResponse({ error: 'query is required' }, 400);
            }
            if (!LLM.attached) {
                return jsonResponse({
                    error: 'No model is running in this tab. Click the model button in the '
                        + 'sidebar, download one, then ask again.',
                }, 503);
            }
            return demoChatResponse(body);
        }],
    ];

    // One /api/chat turn, streamed as SSE from a ReadableStream — the page's
    // own reader parses `event:`/`data:` blocks, so a synthetic body with
    // that shape is indistinguishable from the server's.
    function demoChatResponse(body) {
        const a = LLM.attached;
        const encoder = new TextEncoder();
        let closed = false;
        let sink = null;
        const stream = new ReadableStream({
            start(controller) { sink = controller; },
        });
        const send = (name, obj) => {
            if (closed || !sink) return;
            try {
                sink.enqueue(encoder.encode(demoSse(name, obj)));
            } catch (err) { closed = true; }
        };

        (async () => {
            const t0 = performance.now();
            send('phase', { phase: 'retrieving' });
            const plan = demoPlan(a.n_ctx);
            let citations = [];
            let packText = '';
            try {
                const index = await demoIndex();
                const k = Math.min(Math.max(body.k || 8, 1), plan.ui.chat_k);
                const nodes = demoRetrieve(index, body.query, k);
                const pack = demoRenderPack(nodes, plan.context_chars);
                citations = pack.citations;
                packText = pack.text;
            } catch (err) {
                // Retrieval failing must not take the turn with it: the model
                // can still answer, and will say the context is empty.
                console.warn('demo retrieval failed', err);
            }
            const retrievalMs = Math.round(performance.now() - t0);
            send('context', { citations, seed_id: null, retrieval_ms: retrievalMs });

            const preface = packText
                ? 'Retrieved context (cite as [#N]):\n\n' + packText.trimEnd() + '\n---'
                : 'No retrieved context was found for this query.';
            const messages = [
                { role: 'system', content: DEMO_SYSTEM_PROMPT },
                { role: 'system', content: preface },
            ];
            for (const h of (body.history || []).slice(-4)) {
                if (h && (h.role === 'user' || h.role === 'assistant') && h.content) {
                    messages.push({ role: h.role, content: String(h.content) });
                }
            }
            messages.push({ role: 'user', content: String(body.query) });

            const result = await demoDispatchJob(
                {
                    messages,
                    max_tokens: plan.completion_tokens,
                    temperature: typeof body.temperature === 'number' ? body.temperature : 0.3,
                },
                (content) => send('delta', { content }),
            );
            const completionMs = Math.round(performance.now() - t0) - retrievalMs;
            if (result && result.error) {
                send('error', { error: result.error });
            } else {
                send('done', {
                    answer: (result && result.content) || '',
                    reasoning: null,
                    retrieval_ms: retrievalMs,
                    completion_ms: completionMs,
                    tool_calls: 0,
                    tool_rounds: 0,
                    hit_round_cap: false,
                    cost: null,
                    usage: (result && result.usage) || null,
                    dest: null,
                    chat_model: a.label,
                });
            }
        })().catch((err) => {
            send('error', { error: String((err && err.message) || err) });
        }).finally(() => {
            closed = true;
            try { sink && sink.close(); } catch (e) { /* already closed */ }
        });

        return new Response(stream, {
            status: 200,
            headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' },
        });
    }

    window.fetch = function (input, init) {
        const raw = typeof input === 'string' ? input
            : (input && typeof input.url === 'string') ? input.url
                : String(input);

        // Resolved against the page so a relative request is matched by the
        // path it will actually hit. The demo is served from a subdirectory
        // (`/demo/`), so `graph.json` and `threejs-vis.bundle.js` resolve *under*
        // it while the app's own `/api/...` calls stay absolute — which is
        // precisely the line this needs to draw.
        let path;
        try {
            path = new URL(raw, window.location.href).pathname;
        } catch (err) {
            return realFetch(input, init);
        }

        const route = ROUTES[path];
        if (route) return Promise.resolve(route(init));

        for (const [re, handler] of PARAM_ROUTES) {
            const m = path.match(re);
            if (m) return Promise.resolve(handler(m, init));
        }

        // Everything else the server would have owned. 501 rather than 404:
        // the endpoint exists in `ug`, it is this deployment that cannot run
        // it, and each caller already has a failure path that says so.
        if (path === '/api' || path.startsWith('/api/')) {
            return Promise.resolve(jsonResponse({ error: NOTE, demo: true }, 501));
        }

        // Real files on the host — the graph snapshot, the bundle, the
        // wllama runtime. Untouched.
        return realFetch(input, init);
    };

    /* The tab's job feed. The real thing is an SSE stream the server pushes
       jobs down; here the "server" lives in the same page, so the stream is
       a listener registry the routes above dispatch into. Constructed before
       any attach (the page opens it as soon as status says the feature
       exists), kept for the life of the tab, and never reconnects because
       there is nothing to disconnect from.

       Wrapped rather than conditionally constructed so the app's own
       `new EventSource(...)` line is unchanged — same trick as the fetch
       patch above, and this is the only URL the app ever opens one for. */
    const RealEventSource = window.EventSource;
    window.EventSource = function (url, cfg) {
        const client = typeof url === 'string'
            ? (url.match(/[?&]client=([^&]+)/) || [])[1]
            : null;
        if (!client || url.indexOf('/api/llm/local/events') === -1) {
            return new RealEventSource(url, cfg);
        }
        const id = decodeURIComponent(client);
        const listeners = new Map();
        const on = (name, fn) => {
            if (!listeners.has(name)) listeners.set(name, new Set());
            listeners.get(name).add(fn);
        };
        const feed = {
            close() { LLM.clients.delete(id); },
            addEventListener(name, fn) { on(name, fn); },
            emit(name, data) {
                for (const fn of listeners.get(name) || []) {
                    try {
                        fn({ data: JSON.stringify(data) });
                    } catch (err) { console.warn('demo job listener failed', err); }
                }
            },
        };
        LLM.clients.set(id, feed);
        // Async so `addEventListener('open', …)` registered right after
        // construction is in place before it fires — the page re-attaches a
        // model on every open, which is exactly what should happen here.
        setTimeout(() => {
            for (const fn of listeners.get('open') || []) {
                try { fn({}); } catch (err) { /* listener's problem */ }
            }
        }, 0);
        return feed;
    };

    /* ── 2. Demo chrome ─────────────────────────────────────────────────── */

    // Scoped to `ug-demo-*` and injected here rather than added to
    // `src/vis/css/`, so the page `ug serve` ships carries none of it.
    const STYLE = `
        .ug-demo-badge {
            color: var(--accent);
            border-color: rgba(249, 115, 22, 0.35);
            text-decoration: none;
            cursor: pointer;
        }
        .ug-demo-badge::before {
            background: var(--accent);
            box-shadow: 0 0 8px var(--accent);
        }

        /* Tucked under the badge on the right. Every other edge is spoken
           for — the sidebar owns the left, the legend / viewbar / gizmo the
           bottom rail, and top centre carries the graph title overlay.

           z-index sits *below* the sidebar and the info panel (both 40) on
           purpose: the card is an introduction, and the moment the app has
           something real to show in that corner it should win. Combined with
           the dismiss-on-outside-click below, the card gets out of the way
           rather than having to be closed. */
        .ug-demo-card {
            position: fixed;
            top: 52px;
            right: 16px;
            z-index: 39;
            width: min(380px, calc(100vw - 32px));
            padding: 14px 16px 13px;
            background: rgba(15, 15, 17, 0.92);
            backdrop-filter: blur(24px);
            -webkit-backdrop-filter: blur(24px);
            border: 1px solid var(--border-soft);
            border-radius: 12px;
            box-shadow: 0 8px 40px rgba(0, 0, 0, 0.5);
            font-family: 'Inter', sans-serif;
            color: var(--text-muted);
            font-size: 11.5px;
            line-height: 1.6;
            opacity: 0;
            transition: opacity 0.4s ease, transform 0.4s ease;
        }
        .ug-demo-card.visible { opacity: 1; }
        .ug-demo-card.dismissed {
            opacity: 0;
            pointer-events: none;
            transform: translateY(-8px);
        }

        .ug-demo-kicker {
            font-family: 'JetBrains Mono', monospace;
            font-size: 9.5px;
            font-weight: 600;
            letter-spacing: 1.2px;
            text-transform: uppercase;
            color: var(--accent);
            margin-bottom: 6px;
        }
        .ug-demo-card h3 {
            margin: 0 0 6px;
            font-size: 14px;
            font-weight: 600;
            color: var(--text);
            line-height: 1.4;
        }
        .ug-demo-card p { margin: 0 0 9px; }
        .ug-demo-card strong { color: var(--text); font-weight: 600; }
        .ug-demo-lines { margin: 0 0 11px; padding: 0; list-style: none; }
        .ug-demo-lines li {
            display: flex;
            gap: 7px;
            align-items: baseline;
            padding: 1.5px 0;
        }
        .ug-demo-lines .mark { flex: none; width: 11px; font-size: 10px; }
        .ug-demo-lines .on .mark { color: var(--success); }
        .ug-demo-lines .off .mark { color: var(--text-dim); }
        .ug-demo-lines .off { color: var(--text-dim); }

        .ug-demo-actions { display: flex; align-items: center; gap: 8px; }
        .ug-demo-cta {
            display: inline-flex;
            align-items: center;
            gap: 5px;
            background: rgba(249, 115, 22, 0.14);
            color: var(--accent);
            border: none;
            border-radius: 6px;
            padding: 6px 12px;
            font-family: inherit;
            font-size: 11px;
            font-weight: 600;
            text-decoration: none;
            cursor: pointer;
            transition: background 0.15s;
        }
        .ug-demo-cta:hover { background: rgba(249, 115, 22, 0.24); }
        .ug-demo-dismiss {
            background: none;
            border: none;
            color: var(--text-dim);
            font-family: inherit;
            font-size: 11px;
            padding: 6px 4px;
            cursor: pointer;
        }
        .ug-demo-dismiss:hover { color: var(--text-muted); }

    `;

    // Remembered per origin: a visitor who has read the card once and comes
    // back to keep exploring should get the canvas, not the card again.
    const SEEN_KEY = 'ug-demo-card-seen';

    function el(html) {
        const t = document.createElement('template');
        t.innerHTML = html.trim();
        return t.content.firstElementChild;
    }

    function escapeText(s) {
        return String(s).replace(/[&<>"]/g, c => (
            { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]
        ));
    }

    function fmt(n) {
        return Number(n || 0).toLocaleString('en-US');
    }

    /* The header badge. `pollHealth()` owns this element and rewrites it on
       its own schedule, so this re-asserts after it rather than racing it.
       `paint` is idempotent — it returns when the badge already says what it
       should — which is what keeps the observer from re-triggering itself. */
    function claimBadge() {
        const badge = document.getElementById('health-badge');
        if (!badge) return;
        const LABEL = 'Live demo';
        const paint = () => {
            if (badge.textContent === LABEL && badge.classList.contains('ug-demo-badge')) return;
            badge.className = 'health-badge ug-demo-badge';
            badge.textContent = LABEL;
            badge.title = NOTE;
        };
        paint();
        new MutationObserver(paint).observe(badge, {
            childList: true,
            characterData: true,
            subtree: true,
            attributes: true,
            attributeFilter: ['class'],
        });
        badge.addEventListener('click', () => window.open(DEMO.install, '_blank', 'noopener'));
    }

    /* The capability banners. Each one currently offers "Ingest now", which
       is the right fix when a *local* project has no vectors and a dead
       button here — the endpoint behind it is one of the 501s above. Replace
       the copy with the reason that actually applies, and the button with the
       action that actually helps.

       The Ask bar's Names mode reads graph.json in the browser and genuinely
       works here; Find and Tour are the two that need the store, and they
       share one banner. Answer is the exception the banner has to get right
       twice: with no model it is off like the others, with a model attached
       it runs entirely in this tab — so the copy is written per state and
       rewritten on every attach and detach (both go through the routes
       above, which call back into here). */
    function bannerLink(text) {
        return `<a class="cap-cta" href="${escapeText(DEMO.install)}" `
            + `target="_blank" rel="noopener">${escapeText(text)}</a>`;
    }

    function rewriteBanners() {
        const chat = document.getElementById('chat-no-embeddings-msg');
        if (chat) {
            if (!DEMO.llm) {
                chat.innerHTML =
                    `<strong>Find, Answer and Tour need a local index.</strong>`
                    + `All three are planned against the vector store ug builds on your `
                    + `machine, and answers additionally need any OpenAI-compatible `
                    + `endpoint you point ug at — a local one is fine. Neither travels `
                    + `with a snapshot, so the bar here searches names only.`
                    + bannerLink('Install ug →');
            } else if (LLM.attached) {
                chat.innerHTML =
                    `<strong>Answers are running in this tab, on ${escapeText(LLM.attached.label)}.</strong>`
                    + `Each answer is grounded in keyword retrieval over the published `
                    + `graph and cites the nodes it used — click a citation to find it on `
                    + `the canvas. Find and Tour still need the local index, and stay off.`
                    + bannerLink('Install ug →');
            } else {
                chat.innerHTML =
                    `<strong>Find and Tour need a local index — Answer doesn't.</strong>`
                    + `Download a small model into this tab and it answers questions from `
                    + `the published graph itself, with citations. The download happens `
                    + `once, in your browser; nothing is sent anywhere.`
                    + `<button class="cap-cta" type="button" id="ug-demo-llm-cta">Run a model in this browser</button>`
                    + bannerLink('Install ug →');
                const cta = chat.querySelector('#ug-demo-llm-cta');
                if (cta) {
                    cta.addEventListener('click', () => {
                        const btn = document.getElementById('llm-open-btn');
                        if (btn) btn.click();
                    });
                }
            }
        }

        const ins = document.getElementById('ins-nodb-msg');
        if (ins) {
            ins.innerHTML =
                `<strong>Statistics need the indexed database.</strong>`
                + 'The preset questions and GQL read the structural DB `ug gen` builds. '
                + `The demo ships the graph alone, so the questions are listed here to `
                + `show what they ask — they cannot run.`
                + bannerLink('Install ug →');
        }
    }

    /* The intro card. Says what this is, what a visitor can actually do with
       it, and what needs the real thing — because the honest version of a
       demo is one where the missing half is named rather than hidden. */
    function introCard() {
        if (localStorage.getItem(SEEN_KEY) === '1') return;

        const card = el(`
            <div class="ug-demo-card" id="ug-demo-card" role="dialog" aria-label="About this demo">
                <div class="ug-demo-kicker">Live demo · read only</div>
                <h3>${escapeText(DEMO.label)}, as ug indexed it.</h3>
                <p><strong>${fmt(DEMO.nodes)} nodes</strong> and
                   <strong>${fmt(DEMO.edges)} edges</strong> — real output from
                   <code>ug gen</code>, published as a static snapshot.</p>
                <ul class="ug-demo-lines">
                    <li class="on"><span class="mark">✓</span><span>Fly the graph, filter by type, focus a node and walk its neighbourhood</span></li>
                    <li class="on"><span class="mark">✓</span><span>Keyword search, outlines, callers and imports — all read from the graph</span></li>
                    ${DEMO.llm ? '<li class="on"><span class="mark">✓</span><span>Ask questions with a small model downloaded into the tab — answers cite the graph</span></li>' : ''}
                    <li class="off"><span class="mark">·</span><span>Semantic search, guided tours, statistics and source preview need the local index</span></li>
                </ul>
                <div class="ug-demo-actions">
                    <a class="ug-demo-cta" href="${escapeText(DEMO.install)}" target="_blank" rel="noopener">Install ug →</a>
                    <button class="ug-demo-dismiss" type="button">Explore the graph</button>
                </div>
            </div>
        `);

        // Reaching for the graph *is* the dismissal — a visitor who has
        // started exploring has finished reading, and making them close a
        // card first would be the demo getting in its own way. Capture
        // phase, so it runs before whatever the click was aimed at; not
        // `once`, because a click *inside* the card would otherwise spend
        // the listener and leave the card un-dismissable from outside.
        const onOutside = (ev) => { if (!card.contains(ev.target)) dismiss(); };
        const dismiss = () => {
            document.removeEventListener('pointerdown', onOutside, true);
            card.classList.add('dismissed');
            try { localStorage.setItem(SEEN_KEY, '1'); } catch (err) { /* private mode */ }
            setTimeout(() => card.remove(), 450);
        };
        card.querySelector('.ug-demo-dismiss').addEventListener('click', dismiss);
        card.querySelector('.ug-demo-cta').addEventListener('click', dismiss);
        document.body.appendChild(card);

        // Held back until the loading overlay has had a chance to clear, so
        // the card lands on a graph rather than on a spinner. The
        // outside-click listener waits with it: armed any earlier, a click
        // during the load would dismiss a card that was never on screen —
        // and mark it seen, so it would never appear at all.
        setTimeout(() => {
            card.classList.add('visible');
            document.addEventListener('pointerdown', onOutside, true);
        }, 1200);
    }

    function start() {
        const style = document.createElement('style');
        style.textContent = STYLE;
        document.head.appendChild(style);
        claimBadge();
        rewriteBanners();
        introCard();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', start);
    } else {
        start();
    }
})();
