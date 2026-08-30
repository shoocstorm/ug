        // ─── The performance HUD ───────────────────────────────
        //
        // What the canvas is costing, on the canvas, while you use it —
        // `vis.perf_hud`, off out of the box.
        //
        // It exists because of what P12.19 measured: the same graph walk runs
        // at 22.7 fps in `ug app` and 77–83 in Chrome, and in *both* the
        // page's own work is 1 ms a frame. A frame-rate number alone cannot
        // tell those apart, so this reports the two separately:
        //
        //   frame    the cadence the browser is handing us — rAF interval.
        //            Bounded by the display and by the engine's own frame
        //            policy (WebKit halves it in Low Power Mode), so a bad
        //            number here is not necessarily ours.
        //   overlay  the FX layer's own draw, timed around `overlayDraw()`.
        //            This one *is* ours, and on a walk or a tour it runs every
        //            frame for as long as the walk is on screen.
        //
        // Deliberately cheap: one rAF callback that stores two numbers, and a
        // DOM write twice a second (P5.2 — the gizmo that rewrote innerHTML at
        // 30 Hz was itself a perf item; a readout that costs what it reports
        // is worthless).
        const PERF_HUD_DEFAULT = 'off';   // mirrors vis.perf_hud in native/src/config.rs
        const PERF_HUD_TEXT_MS = 500;     // how often the numbers are rewritten
        const PERF_HUD_RING = 240;        // frames kept for the percentiles

        const perfHud = {
            on: false,
            el: null,
            raf: 0,
            rows: null,          // { key: <span> } for the value cells
            ivals: new Float32Array(PERF_HUD_RING),
            count: 0,            // total intervals recorded (ring is count % RING)
            prevFrame: 0,
            frames: 0,           // frames since the last text write
            draws: 0,            // overlay draws since the last text write
            drawMs: 0,           // their total cost
            drawMax: 0,
            windowStart: 0,
        };

        // Read by the overlay's tick on every frame, so it is a field lookup
        // and not a capability walk.
        function perfHudActive() { return perfHud.on; }

        // The FX overlay reports its own draw here. Called only while the HUD
        // is on — see `overlayStart`'s tick.
        function perfHudNoteDraw(ms) {
            perfHud.draws++;
            perfHud.drawMs += ms;
            if (ms > perfHud.drawMax) perfHud.drawMax = ms;
        }

        function perfHudConfigured() {
            const raw = state.capabilities && state.capabilities.vis
                && state.capabilities.vis.perf_hud;
            return String(raw || PERF_HUD_DEFAULT).toLowerCase() === 'on';
        }

        function perfHudBuild() {
            perfHud.el = document.getElementById('perf-hud');
            if (!perfHud.el || perfHud.rows) return;
            const keys = [
                ['frame', 'frame'],
                ['overlay', 'overlay'],
                ['drawn', 'drawn'],
                ['canvas', 'canvas'],
            ];
            perfHud.el.innerHTML = keys.map(([k, label]) =>
                `<div class="ph-row"><span class="ph-k">${label}</span><span class="ph-v" data-ph="${k}">—</span></div>`).join('');
            perfHud.rows = {};
            perfHud.el.querySelectorAll('[data-ph]').forEach(el => { perfHud.rows[el.dataset.ph] = el; });
        }

        // A percentile over the ring, which is unsorted and mostly full.
        function perfHudQuantile(p) {
            const n = Math.min(perfHud.count, PERF_HUD_RING);
            if (!n) return 0;
            const a = Array.prototype.slice.call(perfHud.ivals, 0, n).sort((x, y) => x - y);
            return a[Math.min(n - 1, Math.floor(n * p))];
        }

        function perfHudWrite(now) {
            const elapsed = now - perfHud.windowStart;
            const fps = elapsed > 0 ? (1000 * perfHud.frames / elapsed) : 0;
            const median = perfHudQuantile(0.5);
            const p95 = perfHudQuantile(0.95);
            const set = (k, text, cls) => {
                const el = perfHud.rows[k];
                if (!el) return;
                if (el.textContent !== text) el.textContent = text;
                if (cls !== undefined) el.className = 'ph-v' + (cls ? ' ' + cls : '');
            };

            // 55+ fps is smooth on any panel; below 24 the motion has stopped
            // reading as motion. The band between is the one worth noticing.
            const grade = fps >= 55 ? 'good' : (fps >= 24 ? 'warn' : 'bad');
            set('frame', `${fps.toFixed(0)} fps · ${median.toFixed(1)} ms · p95 ${p95.toFixed(1)}`, grade);

            if (perfHud.draws) {
                const mean = perfHud.drawMs / perfHud.draws;
                const rate = elapsed > 0 ? (1000 * perfHud.draws / elapsed) : 0;
                set('overlay', `${mean.toFixed(1)} ms · max ${perfHud.drawMax.toFixed(1)} · ${rate.toFixed(0)}/s`, '');
            } else {
                // Not drawing is the *good* state: the overlay gate (P12.10)
                // is holding, and the canvas is genuinely at rest.
                set('overlay', 'idle', '');
            }

            const nodes = (state.view && state.view.nodes) ? state.view.nodes.length : 0;
            const links = (state.view && state.view.edges) ? state.view.edges.length : 0;
            let drawn = `${formatNumber(nodes)} n · ${formatNumber(links)} l`;
            if (state.walkActive && state.walkEdgeKeys) drawn += ` · walk ${formatNumber(state.walkEdgeKeys.size)}`;
            else if (typeof tourState !== 'undefined' && tourState && tourState.active && tourState.routeEdges) drawn += ` · tour ${formatNumber(tourState.routeEdges.size)}`;
            set('drawn', drawn, '');

            // Device pixels, not CSS pixels: what the GPU is actually filling
            // is the number that explains a slow frame on a retina display —
            // 1400×868 at dpr 2 is 4.9 million pixels a frame. Heap only where
            // the engine reports one (Chromium; Safari and WKWebView do not).
            const dpr = window.devicePixelRatio || 1;
            // `width`/`height` are set by the renderer's first resize; the HUD
            // can be up before it, and `Math.round(NaN)` is a readout that
            // says NaN.
            const cw = Number.isFinite(width) ? width : 0;
            const ch = Number.isFinite(height) ? height : 0;
            let canvas = `${Math.round(cw * dpr)}×${Math.round(ch * dpr)} @${dpr}`;
            const mem = performance.memory && performance.memory.usedJSHeapSize;
            if (mem) canvas += ` · heap ${(mem / 1048576).toFixed(0)} MB`;
            const R_ = activeRenderer();
            if (R_ && R_.name) canvas += ` · ${R_.name}`;
            set('canvas', canvas, '');

            perfHud.windowStart = now;
            perfHud.frames = 0;
            perfHud.draws = 0;
            perfHud.drawMs = 0;
            perfHud.drawMax = 0;
        }

        function perfHudTick() {
            if (!perfHud.on) return;
            perfHud.raf = requestAnimationFrame(perfHudTick);
            const now = performance.now();
            if (perfHud.prevFrame) {
                perfHud.ivals[perfHud.count % PERF_HUD_RING] = now - perfHud.prevFrame;
                perfHud.count++;
                perfHud.frames++;
            }
            perfHud.prevFrame = now;
            if (now - perfHud.windowStart >= PERF_HUD_TEXT_MS) perfHudWrite(now);
        }

        function perfHudStart() {
            perfHudBuild();
            if (!perfHud.el || perfHud.on) return;
            perfHud.on = true;
            perfHud.el.hidden = false;
            perfHud.count = 0;
            perfHud.frames = 0;
            perfHud.draws = 0;
            perfHud.drawMs = 0;
            perfHud.drawMax = 0;
            perfHud.prevFrame = 0;
            perfHud.windowStart = performance.now();
            perfHud.raf = requestAnimationFrame(perfHudTick);
        }

        function perfHudStop() {
            perfHud.on = false;
            if (perfHud.raf) { cancelAnimationFrame(perfHud.raf); perfHud.raf = 0; }
            if (perfHud.el) perfHud.el.hidden = true;
        }

        // Called once capabilities have landed, and again whenever the setting
        // is saved — a readout you have to reload the page to see is a readout
        // you stop using.
        function perfHudSync(force) {
            const want = (force === undefined) ? perfHudConfigured() : !!force;
            if (want) perfHudStart(); else perfHudStop();
        }
