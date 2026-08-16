        // ─── Renderer core: shared style rules + the backend seam ──────────
        //
        // The page has two renderers. `11-render-three.js` draws a 3D force
        // graph (three.js + 3d-force-graph); `12-render-cosmos.js` draws a 2D
        // GPU-simulated one (cosmos.gl). Everything in *this* file is the part
        // they share — what colour a node is, whether it is visible, how dim it
        // should be — plus the dispatchers the rest of the page calls.
        //
        // Keeping the style rules here is the whole point: a second renderer is
        // only affordable if "what should this node look like?" has one answer.
        // A backend decides how to *draw* that answer, never what it is.

        // Selected node / hovered neighbours flare to a hot saturated orange —
        // the brightest ink on the page; everything else keeps its type colour.
        function nodeColorFor(n) {
            if (state.selectedNode && n.id === state.selectedNode.id) return '#ff3d00';
            if (state.highlightNodes.has(n.id)) return '#f96716';
            // Graph Walk: reached nodes take their hop's colour on a
            // hot→cool gradient; unreached nodes keep their type colour and
            // are dimmed to near-invisible by bumpGraphStyles.
            if (state.walkActive && state.walkColors.has(n.id)) return state.walkColors.get(n.id);
            // On a tour the other stops burn amber so the route reads as one
            // chain; everything else keeps its type colour and gets lowlit
            // by opacity instead (see nodeOpacityFor).
            if (tourTier(n.id) === 'stop') return '#fb923c';
            return config.getColor(n.group);
        }

        function linkColorFor(e) {
            if (state.highlightLinks.has(e)) {
                return state.highlightLinkDir.get(e) === 'in' ? CANVAS.linkIn : CANVAS.linkOut;
            }
            // Graph Walk: walked edges glow in the frontier colour, everything
            // else recedes into the background so the expanding frontier is
            // the only thing the eye tracks.
            if (state.walkActive) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                const key = sId < tId ? sId + '|' + tId : tId + '|' + sId;
                if (state.walkEdgeKeys.has(key)) {
                    return state.walkColors.get(tId) || state.walkColors.get(sId) || '#f97316';
                }
                // Both endpoints reached but the BFS didn't walk this edge —
                // dim structural context between frontier nodes, rather than
                // the bright background tone used for everything unreached.
                if (state.walkReached.has(sId) && state.walkReached.has(tId)) return CANVAS.linkRecede;
                return CANVAS.linkFar;
            }
            // On a tour the route glows and everything else fades out, so the
            // path through the graph is the only thing the eye can follow.
            if (tourTier(e.source.id || e.source)) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (isTourRouteEdge(e)) return '#f97316';
                const cur = tourCurrentStop();
                if (cur && (sId === cur.node_id || tId === cur.node_id)) return config.getRelColor(e.rel);
                if (tourState.routeIds.has(sId) && tourState.routeIds.has(tId)) return CANVAS.linkRouteDim;
                return CANVAS.linkFar;
            }
            // In focus mode, links not wholly inside the focused neighbourhood
            // recede to a near-background tone so the local structure stands out.
            if (state.focusNode) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (!(state.focusSet.has(sId) && state.focusSet.has(tId))) return CANVAS.linkRecede;
            }
            return config.getRelColor(e.rel);
        }

        // Visibility accessors handed to the renderer. Filters own the base
        // answer; a tour in "solo" mode narrows it further to the route,
        // which turns the walk into a standalone diagram of the answer.
        // Is focus solo mode actually in force? Guarded rather than read raw:
        // an empty focus set would hide every node and leave a blank canvas,
        // and a tour owns isolation while it runs.
        function focusIsolateOn() {
            // In solo mode the *view* already holds nothing but the chosen
            // neighbourhood, so a second isolation layer has nothing to add and
            // plenty to break: it would hide everything outside the last
            // anchor's focus set, blanking a plotted result set.
            if (state.soloOnly) return false;
            return state.focusIsolate
                && !!state.focusNode
                && state.focusSet.size > 0
                && !tourState.active;
        }

        function nodeVisibleFor(n) {
            // Graph Walk isolates the canvas to the reached set so each new
            // frontier is the only thing on screen — the same "forced solo"
            // feel as the tour's isolate toggle, applied automatically for
            // every walk regardless of graph size.
            if (state.walkActive && !state.walkReached.has(n.id)) return false;
            if (tourState.active && tourState.isolate && !tourState.routeIds.has(n.id)) return false;
            if (focusIsolateOn() && !state.focusSet.has(n.id)) return false;
            return !(state.nodeHidden && state.nodeHidden(n));
        }

        function linkVisibleFor(e) {
            if (state.walkActive) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (!state.walkReached.has(sId) || !state.walkReached.has(tId)) return false;
            }
            if (tourState.active && tourState.isolate) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (!(tourState.routeIds.has(sId) && tourState.routeIds.has(tId))) return false;
            }
            if (focusIsolateOn()) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (!(state.focusSet.has(sId) && state.focusSet.has(tId))) return false;
            }
            return !(state.linkHidden && state.linkHidden(e));
        }

        // Particles crawl along highlighted links, and along the tour route so
        // the walk has visible direction of travel.
        function linkParticlesFor(e) {
            if (state.highlightLinks.has(e)) return 4;
            if (state.walkActive) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                const key = sId < tId ? sId + '|' + tId : tId + '|' + sId;
                return state.walkEdgeKeys.has(key) ? 3 : 0;
            }
            if (tourState.active && isTourRouteEdge(e)) return 2;
            return 0;
        }

        // Particles inherit their link's direction colour on hover. Elsewhere
        // (tour route, graph walk) there is only one flow to read, so the hot
        // orange stands.
        function linkParticleColorFor(e) {
            const dir = state.highlightLinkDir.get(e);
            if (!dir) return CANVAS.particleOut;
            return dir === 'in' ? CANVAS.particleIn : CANVAS.particleOut;
        }

        function nodeRadiusFor(n) {
            // Nodes are deliberately large — the sticker discs are the primary
            // visual, and a 5-unit disc drowns against edges that span hundreds
            // of units.
            return (config.nodeRadius[n.group] || 6) * 1.6;
        }

        // How lit a node is, as a single number both backends apply the same
        // way (three tints sprite materials, cosmos writes the colour alpha).
        // Extracted from what used to live inline in bumpGraphStyles so the two
        // renderers cannot disagree about what "dimmed" means.
        //
        // Returns { dim, opacity, tier }: `tier` is the tour ring (null when no
        // tour is running), `dim` is the binary "push this to the background".
        function nodeLightingFor(n) {
            const focusOn = !!state.focusNode;
            // On a tour, brightness is a four-ring gradient (this stop → the
            // rest of the route → its neighbours → everything else). Otherwise
            // focus mode's binary dim applies. Graph Walk has its own
            // three-state version (seed / reached / far).
            const tier = state.walkActive ? null : tourTier(n.id);
            if (state.walkActive) {
                const w = walkTier(n.id);
                // 'pending' = in the reached set but not yet ignited (its edges
                // are streaming toward it). Kept as a faint ghost so the eye has
                // a target to watch the particles flow into.
                const opacity = w === 'seed' ? 1.0
                    : w === 'reached' ? 0.96
                    : w === 'pending' ? 0.14
                    : 0.05;
                return { dim: w === 'far' || w === 'pending', opacity, tier: null };
            }
            const dim = tier ? tier === 'far' : (focusOn && !state.focusSet.has(n.id));
            const opacity = tier ? TOUR_TIER_OPACITY[tier] : (dim ? 0.06 : 0.95);
            return { dim, opacity, tier };
        }

        // Warm for a way in, cool violet for a way out — two directions the
        // eye can separate without reading a label.
        const BOUNDARY_IN_COLOR = '#fbbf24';
        const BOUNDARY_OUT_COLOR = '#a78bfa';

        function boundaryRingColor(n) {
            const inbound = (n.boundaries || []).some(b => b.direction === 'Inbound');
            return inbound ? BOUNDARY_IN_COLOR : BOUNDARY_OUT_COLOR;
        }

        function truncateName(name) {
            const parts = String(name).split('/');
            const last = parts.pop();
            const short = last.split(':').pop();
            return short.length > 32 ? short.slice(0, 31) + '…' : short;
        }

        // Centroid + a robust (90th-percentile) radius of the laid-out graph,
        // ignoring far-flung outliers. Shared by camera framing and depth cues.
        function computeExtent() {
            const nodes = state.view.nodes.filter(n => Number.isFinite(n.x));
            if (!nodes.length) return null;
            let cx = 0, cy = 0, cz = 0;
            nodes.forEach(n => { cx += n.x; cy += n.y; cz += n.z || 0; });
            cx /= nodes.length; cy /= nodes.length; cz /= nodes.length;
            const dists = nodes
                .map(n => Math.hypot(n.x - cx, n.y - cy, (n.z || 0) - cz))
                .sort((a, b) => a - b);
            const pct = dists[Math.floor(dists.length * 0.9)] || dists[dists.length - 1] || 120;
            return { cx, cy, cz, radius: Math.max(pct, 40) };
        }

        // The six face views (id → outward camera direction + the coordinate
        // plane that face lies in). Numbers match the digi labels on the boundary
        // box, so "view N" flies the camera to look straight at face N.
        // 3D-only: a 2D renderer reports `caps.faceViews === false` and the
        // viewbar hides the buttons rather than leaving them dead.
        const VIEWS = {
            '1': { dir: [0, 0, 1], plane: 'XY' },   // +Z front
            '2': { dir: [1, 0, 0], plane: 'YZ' },   // +X right
            '3': { dir: [0, 0, -1], plane: 'XY' },  // -Z back
            '4': { dir: [-1, 0, 0], plane: 'YZ' },  // -X left
            '5': { dir: [0, 1, 0], plane: 'XZ' },   // +Y top
            '6': { dir: [0, -1, 0], plane: 'XZ' },  // -Y bottom
        };

        // ─── The backend seam ──────────────────────────────────

        // name → factory. Each backend file registers itself at load time.
        const RENDERERS = {};
        // The mounted backend. Null until createGraph() resolves, which is why
        // every dispatcher below either guards or queues.
        let R = null;
        // Camera moves that arrived before the backend finished mounting —
        // URL state restore is the usual source, and dropping its framing
        // silently would look like the deep link half-worked.
        let _pendingRenderOps = [];

        // The mounted backend, or null while one is still coming up. Callers
        // that need a capability rather than a dispatcher reach for this.
        function activeRenderer() { return R; }

        function whenRendererReady(fn) {
            if (R) { fn(R); return; }
            _pendingRenderOps.push(fn);
        }

        function flushRenderOps() {
            const ops = _pendingRenderOps;
            _pendingRenderOps = [];
            ops.forEach(fn => { try { fn(R); } catch (err) { console.error(err); } });
        }

        // Which backend to use, in order of authority: an explicit choice made
        // this session, then `?r=` on the URL, then whatever was last chosen on
        // this machine, then the size-based default.
        //
        // The default follows the graph: a small repo (at or below
        // THREE_D_DEFAULT_MAX elements) opens in 3D, where the extra dimension
        // and the in-scene effects are still readable; past that the 2D engine
        // takes over, because the per-node object cost that makes 3D pleasant
        // on a small graph is the same cost that makes a big one unusable.
        const RENDERER_STORAGE_KEY = 'ug-renderer';
        const THREE_D_DEFAULT_MAX = 3000;  // max(nodes, edges) at or below this → default to 3D

        // The graph-size default: 3D under the threshold, 2D above it. Reads
        // the full `state.graph`, not the (possibly empty solo) view.
        function autoRendererName() {
            const nodes = state.graph && state.graph.nodes ? state.graph.nodes.length : 0;
            const edges = state.graph && state.graph.edges ? state.graph.edges.length : 0;
            return Math.max(nodes, edges) <= THREE_D_DEFAULT_MAX ? 'three' : 'cosmos';
        }

        function pickRendererName() {
            const candidates = [
                state.renderer,
                new URLSearchParams(window.location.search).get('r'),
                (() => { try { return localStorage.getItem(RENDERER_STORAGE_KEY); } catch (err) { return null; } })(),
                autoRendererName(),
            ];
            for (const c of candidates) if (c && RENDERERS[c]) return c;
            // Every named backend is missing — fall back to whatever registered.
            return Object.keys(RENDERERS)[0];
        }

        function rendererCaps() {
            return (R && R.caps) || { threeD: false, faceViews: false, autoSpin: false, boundaryCube: false };
        }

        // A backend that throws while mounting leaves the page in limbo: R is
        // still null, the loading overlay is up, and nothing on the canvas can
        // say why. Surface the failure in the overlay itself, with a way back
        // to the other renderer — the dead end this page used to hit silently.
        function otherRendererName(name) {
            return Object.keys(RENDERERS)[0] === name ? Object.keys(RENDERERS)[1] : Object.keys(RENDERERS)[0];
        }

        function mountFailed(name, err) {
            const loading = document.getElementById('loading');
            if (!loading) return;
            loading.style.display = 'block';
            loading.innerHTML = `
                <div class="load-error-card">
                    <div class="load-error-title">Could not start the ${name} renderer</div>
                    <div class="load-error-msg">${escapeHtml((err && err.message) || String(err))}</div>
                    <div class="load-error-file">${escapeHtml(name)}</div>
                    <div class="load-error-actions">
                        <button type="button" class="load-error-btn" id="load-renderer-fallback">Try the ${otherRendererName(name)} renderer</button>
                    </div>
                </div>`;
            const fallback = document.getElementById('load-renderer-fallback');
            if (fallback) fallback.addEventListener('click', () => {
                if (RENDERERS[otherRendererName(name)]) {
                    // Restore a spinner so a retry reads as work, not as
                    // a leftover error card the toggle forgot to clear.
                    loading.innerHTML = '<div class="loader"></div><p>Loading graph…</p>';
                    setRenderer(otherRendererName(name));
                }
            });
        }

        async function createGraph() {
            const el = document.getElementById('graph-3d');
            // A fresh graph (first load, retry, project switch) means a fresh
            // render: the loading overlay must stay up until this engine's
            // first painted frame.
            state._graphRevealed = false;
            if (R) { try { R.dispose(); } catch (err) { console.error(err); } R = null; }

            const name = pickRendererName();
            state.renderer = name;
            document.body.dataset.renderer = name;
            const backend = RENDERERS[name]();
            // A backend's teardown disposes its GPU context but not its
            // <canvas> — three's `_destructor()` leaves its element in #graph-3d,
            // so the new backend would mount *underneath* a dead frame that keeps
            // drawing its last painted scene on top. Everything here was owned by
            // the renderer just disposed, so drop it outright.
            el.replaceChildren();
            try {
                await backend.mount(el, state.view);
            } catch (err) {
                console.error('Renderer failed to mount:', err);
                mountFailed(name, err);
                return;
            }
            R = backend;
            applyRendererCaps();
            flushRenderOps();
        }

        // Swap renderers at runtime. Tears the old one down and re-mounts, then
        // restores the current styling — the graph data itself never moved.
        async function setRenderer(name) {
            if (!RENDERERS[name] || name === state.renderer) return;
            state.renderer = name;
            try { localStorage.setItem(RENDERER_STORAGE_KEY, name); } catch (err) { /* private mode */ }
            state._didFit = false;
            state._boxSettled = false;
            // The layout is about to be recomputed by a different engine, so
            // the overlay must come up blank rather than over a stale frame.
            const loading = document.getElementById('loading');
            if (loading) loading.style.display = 'block';
            await createGraph();
            bumpGraphStyles();
        }

        function wireRendererToggle() {
            const btn = document.getElementById('toggle-renderer');
            if (!btn) return;
            btn.addEventListener('click', () => {
                setRenderer(state.renderer === 'cosmos' ? 'three' : 'cosmos');
            });
        }

        // Hide the chrome a backend cannot honour, rather than leaving buttons
        // that do nothing. A dead control is worse than an absent one: it reads
        // as a bug in the graph rather than a property of the renderer.
        function applyRendererCaps() {
            const caps = rendererCaps();
            // Every projection button, the isometric one included: without a
            // camera there is no view to switch to, so "3D / ISO" is as
            // meaningless as face 4. Leaving it visible was the one control
            // that still claimed the 2D canvas had an orientation.
            document.querySelectorAll('#viewbar .vbtn').forEach(btn => {
                btn.hidden = !caps.faceViews;
            });
            const vsep = document.getElementById('vsep-views');
            if (vsep) vsep.hidden = !caps.faceViews;
            // The layout switcher is the mirror image: it stands in for the
            // face projections wherever there is no camera to aim.
            const hasLayouts = !!(R && R.setLayout);
            document.querySelectorAll('#viewbar .lbtn').forEach(btn => { btn.hidden = !hasLayouts; });
            const lsep = document.getElementById('vsep-layouts');
            if (lsep) lsep.hidden = !hasLayouts;
            syncLayoutButtons();
            const gizmo = document.getElementById('axis-gizmo');
            if (gizmo) gizmo.hidden = !caps.threeD;
            const spin = document.getElementById('toggle-spin');
            if (spin) spin.hidden = !caps.autoSpin;
            const box = document.getElementById('toggle-box');
            if (box) box.hidden = !caps.boundaryCube;
            const label = document.getElementById('renderer-label');
            if (label) label.textContent = caps.threeD ? '3D' : '2D';
            // The 3D backend draws its labels and effects in-scene; the 2D one
            // needs the overlay canvas. Exactly one of them is ever running.
            if (caps.threeD) overlayStop();
            else overlayStart();
        }

        // ─── Dispatchers ───────────────────────────────────────
        // These keep the names the rest of the page already calls, so adding a
        // backend did not mean touching every caller.

        // Re-evaluate the style accessors after a selection / highlight / filter
        // change, without rebuilding the whole scene.
        function bumpGraphStyles() { if (R) R.restyle(); }

        function setGraphData(view) { if (R) R.setData(view); }

        function resizeRenderer(w, h) { if (R) R.resize(w, h); }

        function frameGraph(ms = 600) { whenRendererReady(r => r.frameAll(ms)); }

        function setView(id, ms = 600) { whenRendererReady(r => r.setView(id, ms)); }

        function frameNodeSet(ids, ms = 700) { whenRendererReady(r => r.frameNodes(ids, ms)); }

        function focusNode(n) { if (n != null) whenRendererReady(r => r.focusNode(n)); }

        function zoomBy(factor) { if (R) R.zoomBy(factor); }

        // Arrangement, not viewpoint — only a backend without a camera offers
        // these (see COSMOS_LAYOUTS in 12-render-cosmos.js).
        function setGraphLayout(name) { if (R && R.setLayout) R.setLayout(name); }

        function flyToTourStop(stop, opts) { whenRendererReady(r => r.flyToStop(stop, opts || {})); }

        function frameTourRoute(ms) { whenRendererReady(r => r.frameRoute(ms || 1400)); }

        function emitWalkPulse(seedNode, colour, fromR, toR, growMs) {
            if (R) R.emitPulse(seedNode, colour, fromR, toR, growMs);
        }

        // Where a node sits on screen, in page pixels — the tooltip and the
        // overlay both need it, and only the backend knows the projection.
        function nodeScreenPos(n) { return R ? R.screenPos(n) : null; }

        // The loading overlay is owned by the render lifecycle: data is loaded
        // before the diagram exists, so the overlay stays up through the layout
        // + first paint and only comes down once a frame has actually been drawn.
        function graphReveal() {
            if (state._graphRevealed) return;
            state._graphRevealed = true;
            const loading = document.getElementById('loading');
            if (loading) loading.style.display = 'none';
        }
