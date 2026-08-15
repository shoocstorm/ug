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
    }, window.UG_DEMO || {}));

    // One sentence, reused everywhere something is missing: the badge
    // tooltip, the capability probe's `reason` (which the search pane prints
    // verbatim), and the body of every refused request.
    const NOTE =
        'This is a published snapshot of an indexed repo — a graph file on a ' +
        'static host, with no ug server behind it. Anything that reads the ' +
        'local index (semantic search, chat, guided tours, statistics, source ' +
        'preview) is off here. Install ug to get all of it on your own code.';

    /* ── 1. The static "server" ─────────────────────────────────────────── */

    const realFetch = window.fetch.bind(window);

    const jsonResponse = (body, status) => new Response(JSON.stringify(body), {
        status: status || 200,
        headers: { 'Content-Type': 'application/json' },
    });

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
        // hides Chat and Tour, drops the search pane to Keyword (which runs
        // off graph.json, in the browser, and genuinely works), and shows the
        // banners rewritten further down.
        '/api/capabilities': () => jsonResponse({
            mode: 'demo',
            db_ready: false,
            embedder_ready: false,
            search_ready: false,
            chat_ready: false,
            reason: NOTE,
            project: { name: DEMO.project },
        }),
    };

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
        if (route) return Promise.resolve(route());

        // Everything else the server would have owned. 501 rather than 404:
        // the endpoint exists in `ug`, it is this deployment that cannot run
        // it, and each caller already has a failure path that says so.
        if (path === '/api' || path.startsWith('/api/')) {
            return Promise.resolve(jsonResponse({ error: NOTE, demo: true }, 501));
        }

        // Real files on the host — the graph snapshot, the bundle. Untouched.
        return realFetch(input, init);
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

    /* The three capability banners. Each one currently offers "Ingest now",
       which is the right fix when a *local* project has no vectors and a dead
       button here — the endpoint behind it is one of the 501s above. Replace
       the copy with the reason that actually applies, and the button with the
       action that actually helps. */
    const BANNERS = {
        'tour-disabled-msg': [
            'Guided tours need a local index.',
            'A tour is planned against the vector store ug builds on your machine, ' +
            'then flown as a camera path across this same canvas. There is no store ' +
            'behind a published snapshot, so the launcher stays closed here.',
        ],
        'chat-no-embeddings-msg': [
            'Chat needs a local index and a model.',
            'Chat answers from embeddings ug writes next to the graph, through any ' +
            'OpenAI-compatible endpoint you point it at — a local one is fine. ' +
            'Neither travels with a snapshot.',
        ],
        'ins-nodb-msg': [
            'Statistics need the indexed database.',
            'The preset questions and GQL read the structural DB `ug gen` builds. ' +
            'The demo ships the graph alone, so the questions are listed here to ' +
            'show what they ask — they cannot run.',
        ],
    };

    function rewriteBanners() {
        for (const [id, [title, body]] of Object.entries(BANNERS)) {
            const node = document.getElementById(id);
            if (!node) continue;
            node.innerHTML =
                `<strong>${escapeText(title)}</strong>${escapeText(body)}` +
                `<a class="cap-cta" href="${escapeText(DEMO.install)}" ` +
                `target="_blank" rel="noopener">Install ug →</a>`;
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
                    <li class="off"><span class="mark">·</span><span>Semantic search, chat, guided tours, statistics and source preview need the local index</span></li>
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
