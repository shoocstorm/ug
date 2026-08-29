        // ─── The 2D FX overlay ─────────────────────────────────────────────
        //
        // Everything the cosmos.gl backend cannot draw itself, painted on one
        // 2D canvas sitting over its WebGL one: labels, halos, the selection
        // marker, link-flow particles, the Graph Walk ignition burst and the
        // boundary frame.
        //
        // Why an overlay rather than more shader work: cosmos.gl renders no
        // text at all, has no dash *offset* uniform (so a flowing link cannot
        // be faked with a marching dash pattern), and draws point images inside
        // the point's own quad (so a halo cannot extend past its node). Each of
        // those is a hard limit of the library, not a gap worth patching in a
        // vendored bundle.
        //
        // This file is deliberately cosmos-specific — it reads `cosmos`
        // directly from 12-render-cosmos.js. The 3D backend draws all of this
        // in-scene and never starts the loop.
        //
        // The one rule that keeps it affordable: **never iterate every node per
        // frame.** Labels come from cosmos.gl's own density sampling, and every
        // other layer is bounded by the "hot" set — what is selected, hovered,
        // on the tour route, or in the walk frontier.

        let fxCanvas = null, fxCtx = null, fxRunning = false, fxFrame = 0;
        let fxPulses = [];
        let fxSweeps = [];

        // Above this, an effect stops being read as a set and starts being
        // read as noise — and stops being cheap. Hover on a hub node is the
        // case that finds this.
        const FX_MAX_HALOS = 400;
        const FX_MAX_FLOW_LINKS = 600;

        // ── When this canvas is allowed to be still ─────────────
        //
        // This loop used to call `overlayDraw()` on every frame of the tab's
        // life. On a settled graph with nothing selected and nothing hovered
        // that is a full-canvas `clearRect`, a label pass and a full-viewport
        // texture upload to the GPU, sixty-plus times a second, producing a
        // pixel-identical frame each time. Measured on the 161,725-node canvas
        // sitting untouched: **58.8% of a CPU core** — renderer 36%,
        // gpu-process 23% — for a picture that was not changing. Gated, the
        // same idle canvas costs **7.8%**. See P12.10.
        //
        // Two conditions let it draw. `overlayLive()` is "something animates
        // by itself, frame to frame". `fxDirty` is "something changed" — set
        // by `overlayInvalidate()` from wherever the drawn content can move.
        // `fxLiveUntil` covers timed motion (a camera flight, a layout morph,
        // a pan) where neither of the other two fires per frame.
        //
        // The one frame that must not be missed is the one *after* the last
        // live frame — the canvas still holds a selection ring that is now
        // gone. `fxWasLive` buys exactly that frame.
        let fxDirty = true;
        let fxWasLive = false;
        let fxLiveUntil = 0;

        function overlayLive() {
            if (fxPulses.length || fxSweeps.length) return true;
            // The ring spins and breathes; the flow particles march.
            if (state.selectedNode || state.highlightNodes.size) return true;
            if (state.walkActive) return true;
            if (typeof tourState !== 'undefined' && tourState && tourState.active) return true;
            return performance.now() < fxLiveUntil;
        }

        // "The drawn content moved, once." Cheap enough to call liberally —
        // it costs one boolean and at most one frame.
        function overlayInvalidate() { fxDirty = true; }

        // "The drawn content will keep moving for `ms`." Every camera flight,
        // layout morph and pan already announces itself to `cosmosMotion`,
        // which forwards to here — so this needs no call sites of its own.
        function overlayAnimateFor(ms) {
            const until = performance.now() + Math.max(0, ms || 0) + 150;
            if (until > fxLiveUntil) fxLiveUntil = until;
        }

        function overlayStart() {
            fxCanvas = document.getElementById('fx-overlay');
            if (!fxCanvas) return;
            fxCtx = fxCanvas.getContext('2d');
            fxCanvas.hidden = false;
            overlayInvalidate();
            if (fxRunning) return;
            fxRunning = true;
            const tick = () => {
                if (!fxRunning) return;
                requestAnimationFrame(tick);
                const live = overlayLive();
                if (!live && !fxDirty && !fxWasLive) return;
                fxWasLive = live;
                fxDirty = false;
                overlayDraw();
            };
            requestAnimationFrame(tick);
        }

        function overlayStop() {
            fxRunning = false;
            _fxWalkEdges = EMPTY_EDGES;
            _fxWalkEdgesFor = null;
            _fxWalkEdgesSize = -1;
            fxLiveUntil = 0;
            fxWasLive = false;
            fxPulses = [];
            fxSweeps = [];
            if (fxCanvas) {
                fxCanvas.hidden = true;
                if (fxCtx) fxCtx.clearRect(0, 0, fxCanvas.width, fxCanvas.height);
            }
        }

        // Canvas pixels track the container and the display's pixel ratio, so
        // text stays crisp on a retina screen instead of being upscaled.
        function overlayResize() {
            const dpr = Math.min(window.devicePixelRatio || 1, 2);
            const w = Math.max(1, Math.round(width * dpr));
            const h = Math.max(1, Math.round(height * dpr));
            if (fxCanvas.width !== w || fxCanvas.height !== h) {
                fxCanvas.width = w;
                fxCanvas.height = h;
                fxCanvas.style.width = width + 'px';
                fxCanvas.style.height = height + 'px';
            }
            fxCtx.setTransform(dpr, 0, 0, dpr, 0, 0);
        }

        // Node → canvas-local pixels. The backend's screenPos() answers in page
        // coordinates because the tooltip needs those; the overlay is aligned
        // with the canvas, so the offsets come back off again here.
        function fxPos(n) {
            if (!cosmos || !n) return null;
            // Live position where one is being tracked (the selected node and
            // its highlighted neighbours), so the ring and halos stay glued to
            // their nodes through a drag or a running simulation. Everything
            // else falls back to the last synced n.x/n.y.
            const sp = cosmosLivePos(n);
            if (!sp) return null;
            const p = cosmos.spaceToScreenPosition(sp);
            return p ? { x: p[0], y: p[1] } : null;
        }

        function fxRadius(n) {
            const r = cosmos ? cosmos.spaceToScreenRadius(nodeRadiusFor(n)) : 0;
            return Math.max(r || 0, 2);
        }

        // Pull a segment's ends back to the rims of the two discs it joins,
        // as {ax, ay, bx, by} — or null when the discs overlap and there is no
        // strand left to draw.
        //
        // This overlay is a canvas *over* cosmos.gl's, so anything drawn here
        // is drawn on top of every node: there is no z-order between the two
        // layers to hide a line behind a disc. Trimming the ends gives the
        // picture a single draw list would — edges first, nodes over them —
        // and it is the ends that matter, because that is where a strand meets
        // the one thing it must not cross out.
        function fxTrimSegment(a, b, ra, rb) {
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const len = Math.hypot(dx, dy);
            // Leave a little strand rather than trimming to nothing: two
            // touching discs still need the line between them to exist.
            if (!len || len <= ra + rb + 2) return null;
            const ux = dx / len;
            const uy = dy / len;
            return {
                ax: a.x + ux * ra, ay: a.y + uy * ra,
                bx: b.x - ux * rb, by: b.y - uy * rb,
            };
        }

        // The bounded set every layer except labels works from.
        function fxHotNodes() {
            const ids = new Set();
            if (state.selectedNode) ids.add(state.selectedNode.id);
            state.highlightNodes.forEach(id => ids.add(id));
            if (tourState.active) tourState.routeIds.forEach(id => ids.add(id));
            if (state.walkActive) {
                // The frontier, not the whole reached set — a walk over a big
                // graph reaches thousands, and the newest hop is the thing the
                // eye is following.
                state.walkColors.forEach((_c, id) => ids.add(id));
            }
            const out = [];
            for (const id of ids) {
                const n = state.nodeById && state.nodeById.get(id);
                if (n && Number.isFinite(n.x)) out.push(n);
                if (out.length >= FX_MAX_HALOS) break;
            }
            return out;
        }

        function overlayDraw() {
            if (!fxCanvas || !fxCtx || !cosmos || !cosmos.isReady) return;
            // One frame, one GPU readback. Everything below asks `cosmosLivePos`
            // for positions — once per halo and twice per flow link — and each
            // of those used to stall the pipeline on its own. See P12.7.
            cosmosInvalidatePositions();
            overlayResize();
            fxCtx.clearRect(0, 0, width, height);
            fxFrame++;

            if (state.showBoundary) fxDrawBoundary();
            fxDrawClusterLabels();
            fxDrawWalkLanes();
            fxDrawWalkEdges();
            fxDrawFlow();
            const hot = fxHotNodes();
            fxDrawHalos(hot);
            fxDrawSelection();
            fxDrawPulses();
            fxDrawSweeps();
            fxDrawLabels();

            // The legend counts what is actually on the canvas — during a walk
            // that is the reached set, during a tour the route — so it has to
            // be re-read as those grow, not once at the end.
            //
            // The 3D renderer does this from its own rAF loop. Here it was only
            // wired to onSimulationEnd, which never fires at all under a static
            // layout, so the counts sat frozen at the whole-graph totals for
            // the entire walk. Throttled to every 12th frame, same as 3D.
            if (fxFrame % 12 === 0) refreshModeLegend();
        }

        // ── Halos ──────────────────────────────────────────────
        // A port of the 3D renderer's glowTexture() gradient stops, painted
        // straight into 2D. Only the hot set gets one: the ambient halo every
        // node carries in 3D would mean a full-graph pass per frame, which is
        // the cost this renderer exists to avoid.
        function fxDrawHalos(hot) {
            const ctx = fxCtx;
            ctx.save();
            ctx.globalCompositeOperation = 'lighter';
            for (const n of hot) {
                const p = fxPos(n);
                if (!p) continue;
                const { dim } = nodeLightingFor(n);
                if (dim) continue;
                const sel = state.selectedNode && n.id === state.selectedNode.id;
                const isHot = sel || state.highlightNodes.has(n.id);
                // A whisper of breathing on the selected node only. In 3D the
                // halo is a depth cue you read past; flat on a 2D plane the
                // same amplitude reads as the node throbbing at you.
                const wave = sel ? 1 + 0.04 * Math.sin(performance.now() * 0.0026) : 1;
                const r = fxRadius(n) * 1.45 * wave;
                if (r < 3) continue;
                const [cr, cg, cb] = cosmosRgb(config.getColor(n.group));
                // Pulled well back from the 3D values. There the halo competes
                // with fog, depth and a dark backdrop; here it sits directly on
                // the disc, so the same alpha blows out the glyph underneath.
                // A halo should say "this one" from the corner of your eye and
                // otherwise stay out of the way of the glyph it surrounds.
                const a = isHot ? 0.22 : (tourTier(n.id) === 'stop' ? 0.16 : 0.09);
                const g = ctx.createRadialGradient(p.x, p.y, 0, p.x, p.y, r);
                const rgb = `${Math.round(cr * 255)},${Math.round(cg * 255)},${Math.round(cb * 255)}`;
                g.addColorStop(0, `rgba(${rgb},${(a).toFixed(3)})`);
                g.addColorStop(0.2, `rgba(${rgb},${(a * 0.65).toFixed(3)})`);
                g.addColorStop(0.55, `rgba(${rgb},${(a * 0.18).toFixed(3)})`);
                g.addColorStop(1, `rgba(${rgb},0)`);
                ctx.fillStyle = g;
                ctx.beginPath();
                ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
                ctx.fill();
            }
            ctx.restore();
        }

        // ── Selection marker ───────────────────────────────────
        // The spinning, pulsing ring with four tick marks — the same marker the
        // 3D renderer builds as a sprite, drawn directly.
        function fxDrawSelection() {
            const n = state.selectedNode;
            if (!n) return;
            const p = fxPos(n);
            if (!p) return;
            const t = performance.now() * 0.001;
            const wave = Math.sin(t * 3.2);
            // Close to the disc: the ring marks the node, it does not enclose
            // its neighbourhood.
            const r = Math.max(fxRadius(n) * 1.28, 9) * (1 + 0.028 * wave);
            const ctx = fxCtx;
            ctx.save();
            ctx.translate(p.x, p.y);
            ctx.rotate(t * 0.32);
            ctx.strokeStyle = `rgba(255,61,0,${(0.7 + 0.15 * wave).toFixed(3)})`;
            ctx.lineWidth = Math.max(1.5, r * 0.055);
            ctx.beginPath();
            ctx.arc(0, 0, r, 0, Math.PI * 2);
            ctx.stroke();
            ctx.lineWidth = Math.max(2, r * 0.075);
            for (let a = 0; a < 4; a++) {
                const ang = a * Math.PI / 2;
                ctx.beginPath();
                ctx.arc(0, 0, r * 1.17, ang - 0.12, ang + 0.12);
                ctx.stroke();
            }
            ctx.restore();
        }

        // ── Link flow particles ────────────────────────────────
        // cosmos.gl's dash pattern has no offset uniform, so a flowing strand
        // cannot be faked in the link shader. These are the 3D renderer's
        // directional particles, drawn as dots marching along the on-screen
        // segment. Only hot links have any (hover 4, walk 3, tour route 2), so
        // the loop is bounded by the interaction, not by the graph.
        // cosmos.gl's curved-link geometry, mirrored here so the overlay can
        // ride the path its shader draws rather than the chord underneath it.
        // Both constants are the library's own defaults, which is what our
        // config leaves them at: a control point half a link-length off the
        // midpoint, pulled back by a rational-quadratic weight of 0.8.
        const FX_CURVE_H = 0.5;
        const FX_CURVE_W = 0.8;
        // Scratch, because this is evaluated once per particle per frame and
        // an allocation there is thousands a second for nothing.
        const fxPt = [0, 0];

        // The same conic the shader computes:
        //   P(t) = ((1-t)²A + 2(1-t)t·w·C + t²B) / ((1-t)² + 2(1-t)t·w + t²)
        function fxConicPoint(ax, ay, bx, by, cx, cy, t) {
            const mt = 1 - t;
            const w2 = 2 * mt * t * FX_CURVE_W;
            const d = mt * mt + w2 + t * t;
            fxPt[0] = (mt * mt * ax + w2 * cx + t * t * bx) / d;
            fxPt[1] = (mt * mt * ay + w2 * cy + t * t * by) / d;
            return fxPt;
        }

        // Which strands can be carrying particles at all.
        //
        // This loop used to run over `cosmosEdges` — **all 745,964 of them,
        // every frame** — asking `linkParticlesFor` about each one in order to
        // find the handful that answer yes. With a node selected and the
        // pointer away, that is an O(edges) scan per frame producing nothing,
        // for as long as the selection lasts: profiled at **853 ms of a 6 s
        // window inside `linkParticlesFor` alone**, and 29% of a core against
        // 1.4% with nothing selected. See P12.12.
        //
        // `linkParticlesFor` answers from one of three sources. Two of them —
        // a walk and a tour — decide by node-pair key and by route membership
        // rather than by edge identity, so they still have to be asked; both
        // are transient, and both are already redrawing the canvas for other
        // reasons. The third is a hover or a selection, it is exactly
        // `state.highlightLinks`, and it is the one that persists.
        function fxFlowCandidates() {
            if (!state.lineFlow) return EMPTY_EDGES;
            if (state.walkActive) return fxWalkEdges();
            if (typeof tourState !== 'undefined' && tourState && tourState.active) return cosmosEdges;
            return state.highlightLinks;
        }
        const EMPTY_EDGES = [];

        // The walk's edges as objects, in `cosmosEdges` order.
        //
        // Both the layer below and the flow loop above used to find them by
        // scanning **all 745,964 edges every frame**, building a `"a|b"` key
        // string for each one to test membership against `walkEdgeKeys`. In a
        // trace of a walk on the neo4j graph that was 3,073 ms inside
        // `fxDrawWalkEdges` and 2,478 ms inside `linkParticlesFor` — 46% and
        // 37% of a profile whose frames were 267 ms each. Twenty-two frames in
        // seven seconds, about 3 fps, with the GPU idle the whole time: none of
        // it was drawing, all of it was looking. The discarded key strings are
        // also why the same seven seconds logged 342 minor GCs.
        //
        // The set only changes when the walk takes a hop, so the list is built
        // there and reused for every frame in between.
        //
        // Order is `cosmosEdges` order and must stay that way: `cosmosPaint`'s
        // walk branch counts along the same list to decide which strands the
        // overlay will get to within `FX_MAX_FLOW_LINKS`, and the two have to
        // agree about which those are.
        //
        // Cached against the Set *and* its size — a hop grows the same Set
        // rather than replacing it, so identity alone would never notice.
        let _fxWalkEdges = EMPTY_EDGES;
        let _fxWalkEdgesFor = null;
        let _fxWalkEdgesSize = -1;

        function fxWalkEdges() {
            const keys = state.walkEdgeKeys;
            if (!keys || !keys.size) return EMPTY_EDGES;
            if (_fxWalkEdgesFor === keys && _fxWalkEdgesSize === keys.size) return _fxWalkEdges;
            const out = [];
            for (const e of cosmosEdges) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (keys.has(walkEdgeKey(sId, tId))) out.push(e);
            }
            _fxWalkEdges = out;
            _fxWalkEdgesFor = keys;
            _fxWalkEdgesSize = keys.size;
            return out;
        }

        function fxDrawFlow() {
            const ctx = fxCtx;
            const now = performance.now();
            let drawn = 0;
            // Follow the arc while cosmos.gl is drawing arcs — but not during a
            // walk, where the overlay has taken the edges over and draws them
            // as straight strands (see fxDrawWalkEdges). A dot sliding along
            // the chord of a curve it is meant to be travelling sits off its
            // own strand, furthest adrift at the midpoint, which is exactly
            // where the eye is following it.
            const curved = !state.walkActive
                && !!(cosmos.config && cosmos.config.curvedLinks);
            // Every particle is two arcs when it glows and one when it does
            // not, and there are up to three per strand — so on a wide
            // frontier the bloom alone is thousands of paths a frame. It is
            // the flourish, so it is what goes.
            const hot = state.walkActive
                ? (state.walkEdgeKeys ? state.walkEdgeKeys.size : 0)
                : state.highlightLinks.size;
            const glowDots = hot <= 160;
            ctx.save();
            ctx.globalCompositeOperation = 'lighter';
            for (const e of fxFlowCandidates()) {
                const count = linkParticlesFor(e);
                if (!count) continue;
                if (++drawn > FX_MAX_FLOW_LINKS) break;
                const s = state.nodeById.get(e.source.id || e.source);
                const t = state.nodeById.get(e.target.id || e.target);
                if (!s || !t) continue;
                const a = fxPos(s), b = fxPos(t);
                if (!a || !b) continue;
                // Rim to rim, like the strands they run along: a dot drawn
                // over a node disc reads as something sitting on the node
                // rather than as something arriving at it. During a walk every
                // particle-bearing edge is a walk strand, and those end at an
                // arrowhead — so the dots stop where it begins rather than
                // riding over the top of it.
                //
                // Trimmed in curve *parameter* rather than in pixels: on an arc
                // there is no straight segment left to shorten, and over this
                // bow the two agree to within a dot's width. Screen radii over
                // the screen chord, so it holds at every zoom.
                const chord = Math.hypot(b.x - a.x, b.y - a.y);
                if (chord < 2) continue;
                const t0 = fxRadius(s) / chord;
                const t1 = 1 - (fxRadius(t) + (state.walkActive ? 9 : 0)) / chord;
                if (t1 - t0 < 0.06) continue;

                // The control point in *space* coordinates. It has to be: the
                // screen projection can mirror y, and a perpendicular measured
                // after that mirror bows the arc the opposite way from the one
                // the shader drew.
                let sp = null, tp = null, cx = 0, cy = 0;
                if (curved) {
                    sp = cosmosLivePos(s);
                    tp = cosmosLivePos(t);
                    if (!sp || !tp) continue;
                    const dx = tp[0] - sp[0];
                    const dy = tp[1] - sp[1];
                    // normalize(perp) · linkDist · h collapses to perp · h.
                    cx = (sp[0] + tp[0]) / 2 - dy * FX_CURVE_H;
                    cy = (sp[1] + tp[1]) / 2 + dx * FX_CURVE_H;
                }

                const [r, g, bl] = cosmosRgb(linkParticleColorFor(e));
                const rgb = `${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(bl * 255)}`;
                for (let i = 0; i < count; i++) {
                    // Evenly spaced along the strand, all sliding source→target
                    // at the same rate, so direction of travel is readable.
                    // Unhurried on purpose: at the old rate a frontier of them
                    // read as static, which is a busier picture than motion.
                    const phase = ((now * 0.00026) + i / count) % 1;
                    const u = t0 + (t1 - t0) * phase;
                    let x, y;
                    if (curved) {
                        const scr = cosmos.spaceToScreenPosition(
                            fxConicPoint(sp[0], sp[1], tp[0], tp[1], cx, cy, u));
                        if (!scr) continue;
                        x = scr[0];
                        y = scr[1];
                    } else {
                        x = a.x + (b.x - a.x) * u;
                        y = a.y + (b.y - a.y) * u;
                    }
                    // Fade in off the source and out into the target rather
                    // than appearing and vanishing at full strength. The pop at
                    // each end was the part that read as an animation looping
                    // rather than as something flowing.
                    const env = Math.min(1, Math.min(phase, 1 - phase) / 0.16);
                    if (env <= 0.02) continue;
                    // Two passes: a wide, nearly transparent bloom and a small
                    // core. A single hard dot is a pixel travelling; this is a
                    // light travelling, which is what the strand is made of.
                    if (glowDots) {
                        ctx.fillStyle = `rgba(${rgb},${(0.14 * env).toFixed(3)})`;
                        ctx.beginPath();
                        ctx.arc(x, y, 3, 0, Math.PI * 2);
                        ctx.fill();
                    }
                    ctx.fillStyle = `rgba(${rgb},${(0.62 * env).toFixed(3)})`;
                    ctx.beginPath();
                    ctx.arc(x, y, 1.25, 0, Math.PI * 2);
                    ctx.fill();
                }
            }
            ctx.restore();
        }

        // ── Graph Walk: the hop lanes ──────────────────────────
        //
        // The cascade arranges the walk into one column per hop, marching the
        // way the edges point (see computeWalkCascade). Drawn on its own
        // that is a suggestive shape; with the columns named it is a diagram
        // you can read distances off.
        //
        // So each revealed hop gets a banded lane, a heading with its
        // population, and a chevron on the gap to the next one — the arrow is
        // the part that says *which way this is going*, which is the whole
        // claim the arrangement is making. Everything is drawn behind the
        // strands and the nodes, at low alpha: these are gridlines, not ink.
        //
        // At most a handful of lanes, so this is free.
        function fxDrawWalkLanes() {
            const lanes = state.walkActive && state.walkLanes ? state.walkLanes : [];
            if (!lanes.length) return;
            const shown = lanes.filter(walkLaneRevealed);
            if (!shown.length) return;

            // One vertical span shared by every lane, so the bands line up
            // instead of each stopping wherever its own column happens to.
            let top = -Infinity, bottom = Infinity;
            for (const l of shown) {
                if (l.top > top) top = l.top;
                if (l.bottom < bottom) bottom = l.bottom;
            }
            const pad = Math.max((top - bottom) * 0.06, 50);
            const a = cosmos.spaceToScreenPosition([0, top + pad]);
            const b = cosmos.spaceToScreenPosition([0, bottom - pad]);
            if (!a || !b) return;
            const yTop = Math.min(a[1], b[1]);
            const yBot = Math.max(a[1], b[1]);

            const ctx = fxCtx;
            ctx.save();
            ctx.font = '600 10.5px "JetBrains Mono", monospace';
            ctx.textAlign = 'center';
            ctx.textBaseline = 'bottom';
            for (const lane of shown) {
                const p = cosmos.spaceToScreenPosition([lane.x, 0]);
                if (!p) continue;
                const x = p[0];
                // A wide hop spreads into a block of lanes, and the band has to
                // cover the block — a fixed-width stripe on the centre would
                // leave the outer lanes sitting outside their own hop.
                const halfW = Math.max(
                    cosmos.spaceToScreenRadius(
                        (lane.x1 - lane.x0) / 2 + WALK_COL_GAP * 0.32 * (lane.scale || 1)) || 0,
                    8
                );
                if (x < -halfW * 2 || x > width + halfW * 2) continue;
                const [r, g, bl] = cosmosRgb(lane.color);
                const rgb = `${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(bl * 255)}`;

                // The band: a wash that fades out top and bottom, so it reads
                // as a lane rather than as a rectangle drawn over the graph.
                const grad = ctx.createLinearGradient(0, yTop, 0, yBot);
                grad.addColorStop(0, `rgba(${rgb},0)`);
                grad.addColorStop(0.16, `rgba(${rgb},0.055)`);
                grad.addColorStop(0.84, `rgba(${rgb},0.055)`);
                grad.addColorStop(1, `rgba(${rgb},0)`);
                ctx.fillStyle = grad;
                ctx.fillRect(x - halfW, yTop, halfW * 2, yBot - yTop);

                // The rule. A single-lane hop gets it down its axis; a hop that
                // spread into a block gets its two edges instead, because a
                // line through the middle of a block of nodes is a line through
                // the middle of a block of nodes.
                ctx.strokeStyle = `rgba(${rgb},0.22)`;
                ctx.lineWidth = 1;
                ctx.beginPath();
                if (lane.x1 - lane.x0 < 1) {
                    ctx.moveTo(x, yTop);
                    ctx.lineTo(x, yBot);
                } else {
                    ctx.moveTo(x - halfW, yTop); ctx.lineTo(x - halfW, yBot);
                    ctx.moveTo(x + halfW, yTop); ctx.lineTo(x + halfW, yBot);
                }
                ctx.stroke();

                const label = lane.label + (lane.count > 1 ? '  ×' + lane.count : '');
                const ly = Math.max(yTop - 7, 16);
                ctx.strokeStyle = 'rgba(13,13,16,0.9)';
                ctx.lineWidth = 3.5;
                ctx.strokeText(label, x, ly);
                ctx.fillStyle = `rgba(${rgb},0.9)`;
                ctx.fillText(label, x, ly);
            }

            // Direction chevrons, one per gap, pointing the way the walk went.
            // Drawn between the lanes rather than on them, because what they
            // describe is the step, not the column.
            const mid = (yTop + yBot) / 2;
            ctx.lineCap = 'round';
            ctx.lineJoin = 'round';
            for (let i = 1; i < shown.length; i++) {
                const prev = shown[i - 1], cur = shown[i];
                const pa = cosmos.spaceToScreenPosition([prev.x, 0]);
                const pb = cosmos.spaceToScreenPosition([cur.x, 0]);
                if (!pa || !pb) continue;
                const cx = (pa[0] + pb[0]) / 2;
                if (cx < 0 || cx > width) continue;
                const dir = pb[0] >= pa[0] ? 1 : -1;
                const s = Math.min(9, Math.abs(pb[0] - pa[0]) * 0.12);
                if (s < 3) continue;
                const [r, g, bl] = cosmosRgb(cur.color);
                ctx.strokeStyle = `rgba(${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(bl * 255)},0.45)`;
                ctx.lineWidth = 2;
                ctx.beginPath();
                ctx.moveTo(cx - dir * s * 0.6, mid - s);
                ctx.lineTo(cx + dir * s * 0.6, mid);
                ctx.lineTo(cx - dir * s * 0.6, mid + s);
                ctx.stroke();
            }
            ctx.restore();
        }

        // ── Graph Walk: the travelled edges ────────────────────
        //
        // The edges the walk actually crossed, drawn as glowing strands in
        // their hop colour. The renderer draws them too, but a 1px WebGL line
        // at 0.95 alpha is still a thin dark thread on a dark ground — and
        // these are the single most important thing on screen during a walk,
        // because they are the *route*, not the scenery.
        //
        // Two passes: a wide soft glow, then a bright thin core. That is what
        // makes a line read as lit rather than merely coloured, and it is the
        // flat equivalent of the additive bloom the 3D walk gets for free.
        // Bounded by the walked set, which is the frontier — not the graph.
        function fxDrawWalkEdges() {
            if (!state.walkActive || !state.walkEdgeKeys || !state.walkEdgeKeys.size) return;
            const ctx = fxCtx;
            // How crowded the frontier is, as one factor every part of a
            // strand answers to. Twenty strands read as a route; a thousand
            // read as a floodlight with the graph lost inside it, so the layer
            // thins as it fills.
            const bulk = Math.min(state.walkEdgeKeys.size, FX_MAX_FLOW_LINKS);
            const k = Math.max(0.16, Math.min(1, 90 / Math.max(1, bulk)));
            // Sparse enough that a few overlapping strands are still
            // countable. Two things ride on it, and both for the same reason:
            // the bloom is the pass that stacks into a white sheet, and the
            // per-strand gradient is the pass that costs — one
            // `createLinearGradient` each, sixty times a second, exactly when
            // there are most of them.
            const sparse = k > 0.45;
            ctx.save();
            ctx.lineCap = 'round';
            let drawn = 0;
            for (const e of fxWalkEdges()) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (++drawn > FX_MAX_FLOW_LINKS) break;
                const s = state.nodeById.get(sId);
                const t = state.nodeById.get(tId);
                if (!s || !t) continue;
                const a = fxPos(s), b = fxPos(t);
                if (!a || !b) continue;
                const seg = fxTrimSegment(a, b, fxRadius(s), fxRadius(t));
                if (!seg) continue;
                // The hop colour of the end being *reached*, so a strand
                // carries the temperature of the frontier it fed.
                const hex = state.walkColors.get(tId) || state.walkColors.get(sId) || '#f97316';
                const [r, g, bl] = cosmosRgb(hex);
                const rgb = `${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(bl * 255)}`;

                // The head is measured before anything is drawn, because the
                // strand has to stop where it starts. Running the line under
                // the head left its darker edges showing through a shape that
                // is meant to read as solid — the seam you see on a cheap
                // diagram, and the reason arrowheads look pasted on.
                const head = fxArrowSpec(a, b, fxRadius(t), k);
                const endX = head ? head.baseX : seg.bx;
                const endY = head ? head.baseY : seg.by;

                // Alpha along the strand: nearly gone where it leaves, full
                // where it arrives. A flat line states a connection; a line
                // that gathers toward its target says which end the walk was
                // moving to, quietly enough that the arrowhead is still the
                // thing that answers it outright.
                const core = Math.min(0.66, 0.24 + 0.42 * k);
                let stroke;
                if (sparse) {
                    stroke = ctx.createLinearGradient(seg.ax, seg.ay, endX, endY);
                    stroke.addColorStop(0, `rgba(${rgb},${(core * 0.22).toFixed(3)})`);
                    stroke.addColorStop(1, `rgba(${rgb},${core.toFixed(3)})`);
                } else {
                    // Crowded: the taper is invisible under five hundred
                    // overlapping strands anyway, so it is dropped rather than
                    // paid for. At the *average* of the alphas it replaces,
                    // not the peak — a flat strand at the gradient's arrival
                    // strength lays down three times the ink over the whole
                    // length, which is a haze across the columns.
                    stroke = `rgba(${rgb},${(core * 0.55).toFixed(3)})`;
                }

                ctx.beginPath();
                ctx.moveTo(seg.ax, seg.ay);
                ctx.lineTo(endX, endY);
                if (sparse) {
                    // Additive, and only this pass: light that sums is what
                    // makes a handful of strands glow, and what makes a
                    // thousand of them a white sheet.
                    ctx.globalCompositeOperation = 'lighter';
                    ctx.strokeStyle = `rgba(${rgb},${(0.09 * k).toFixed(3)})`;
                    ctx.lineWidth = 6;
                    ctx.stroke();
                    ctx.globalCompositeOperation = 'source-over';
                }
                ctx.strokeStyle = stroke;
                ctx.lineWidth = 1.1;
                ctx.stroke();

                // Which way the relationship points, said without the
                // animation. The flow particles say it too, but flow is a
                // toggle (R on the walk bar) and it is off in every still —
                // and a cascade that cannot tell a caller from a callee is a
                // picture of connectivity, not of dependency, which is the one
                // thing a walk is asked for. Note this is the *edge's*
                // direction, not the direction the traversal happened to
                // follow: an inbound walk crosses edges backwards, and drawing
                // the head the way the frontier moved would invert the meaning
                // of the graph.
                if (head) fxArrowHead(ctx, head, `rgba(${rgb},${core.toFixed(3)})`);
            }
            ctx.restore();
        }

        // Where a head pointing along a→b would sit: tip set back from `b` by
        // `inset` pixels so it lands on the rim of the target disc, base one
        // head-length behind it. Returns null when there is nothing to point
        // — a self-loop, two nodes on top of each other, or a strand so short
        // the head would be most of it.
        //
        // Separate from the drawing because the strand needs the base point
        // before anything is painted: the line stops there, so the head sits
        // on the diagram rather than on top of the line.
        //
        // `k` is the crowding factor the strands are drawn at, and the head
        // takes it too. Sized down as the frontier fills, because five hundred
        // wedges over threads this fine would make the heads the diagram and
        // the edges the background.
        function fxArrowSpec(a, b, inset, k) {
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const len = Math.hypot(dx, dy);
            const size = Math.max(5, Math.min(8.5, len * 0.11)) * (0.72 + 0.28 * k);
            if (!len || len < inset + size + 6) return null;
            const ux = dx / len;
            const uy = dy / len;
            const tipX = b.x - ux * inset;
            const tipY = b.y - uy * inset;
            return {
                tipX, tipY,
                baseX: tipX - ux * size,
                baseY: tipY - uy * size,
                // Perpendicular half-width. A narrow head reads as a direction
                // mark; a wide one reads as a shape in its own right, which at
                // several hundred of them is a texture over the diagram.
                wx: -uy * size * 0.4,
                wy: ux * size * 0.4,
                ux, uy, size,
            };
        }

        // The head itself: a dart rather than a triangle. The concave base
        // gives it a soft edge where it meets the strand, so the join reads as
        // a taper instead of as two shapes butted together.
        function fxArrowHead(ctx, h, fill) {
            ctx.beginPath();
            ctx.moveTo(h.tipX, h.tipY);
            ctx.lineTo(h.baseX + h.wx, h.baseY + h.wy);
            ctx.quadraticCurveTo(
                h.baseX + h.ux * h.size * 0.34, h.baseY + h.uy * h.size * 0.34,
                h.baseX - h.wx, h.baseY - h.wy);
            ctx.closePath();
            ctx.fillStyle = fill;
            ctx.fill();
        }

        // ── Graph Walk ignition ────────────────────────────────
        // The 3D burst is a fresnel shell, a wireframe cage and a 96-point
        // debris cloud. Flat, that reads as an expanding ring with a spark
        // burst — same beat, same colour, same easing.
        function overlayEmitPulse(seedNode, colour, fromR, toR, growMs) {
            if (!seedNode || !Number.isFinite(seedNode.x)) return;
            const grow = Math.max(160, growMs || 420);
            const sparks = [];
            for (let i = 0; i < 30; i++) {
                const ang = Math.random() * Math.PI * 2;
                sparks.push({ dx: Math.cos(ang), dy: Math.sin(ang), spd: 0.55 + Math.random() * 0.7 });
            }
            // The hop pulses arrive with real radii measured off the layout
            // (layerReachRadius), but the *seed* pulse is called with the
            // literal 4 → 44 that suited the 3D scene's units. In this space a
            // graph spans thousands, so that burst was about one percent of the
            // view — present, and completely invisible. Anything that small is
            // treated as "a flash at the seed" and given a share of the graph's
            // own extent, so a wavefront always sweeps a distance you can see.
            const ext = computeExtent();
            const R = ext ? ext.radius : 500;
            const from = Math.max(fromR || 0, 6);
            const to = Math.max((toR || 0) + 18, from + 24, R * 0.16);
            fxPulses.push({
                node: seedNode,
                // Where it went off, in space coords. The cascade fires each
                // hop's burst from the centre of the column that is igniting,
                // which is a bare position and not a node at all — and fxPos()
                // can only answer for something in the point index.
                at: [seedNode.x, seedNode.y],
                colour,
                fromR: from,
                toR: to,
                grow,
                fade: Math.min(640, Math.max(240, grow * 0.9)),
                t0: performance.now(),
                sparks,
            });
        }

        function fxDrawPulses() {
            if (!fxPulses.length) return;
            const ctx = fxCtx;
            const now = performance.now();
            ctx.save();
            ctx.globalCompositeOperation = 'lighter';
            fxPulses = fxPulses.filter(p => {
                const t = now - p.t0;
                if (t >= p.grow + p.fade || !state.walkActive) return false;
                const pr = Math.min(1, t / p.grow);
                // Ease-out: decelerate at the frontier.
                const e = 1 - Math.pow(1 - pr, 3);
                // Envelope: quick ramp-in over the first 25%, then fade.
                const env = t <= p.grow
                    ? Math.min(1, pr / 0.25)
                    : Math.max(0, 1 - (t - p.grow) / p.fade);
                let at = fxPos(p.node);
                if (!at) {
                    const sp = cosmos.spaceToScreenPosition(p.at);
                    if (!sp) return true;
                    at = { x: sp[0], y: sp[1] };
                }
                const spaceR = p.fromR + (p.toR - p.fromR) * e;
                const r = cosmos.spaceToScreenRadius(spaceR) || 0;
                if (r <= 0) return true;
                const [cr, cg, cb] = cosmosRgb(p.colour);
                const rgb = `${Math.round(cr * 255)},${Math.round(cg * 255)},${Math.round(cb * 255)}`;

                // The shell: a bright rim with a hot falloff inward.
                const g = ctx.createRadialGradient(at.x, at.y, r * 0.55, at.x, at.y, r);
                g.addColorStop(0, `rgba(${rgb},0)`);
                g.addColorStop(0.75, `rgba(${rgb},${(0.18 * env).toFixed(3)})`);
                // Tinted, not white. A near-white rim on a hop-coloured shell
                // blows out to a flashbulb over a dense column and takes the
                // colour — the one thing the ring is carrying — with it.
                g.addColorStop(1, `rgba(${rgb},${(0.55 * env).toFixed(3)})`);
                ctx.fillStyle = g;
                ctx.beginPath();
                ctx.arc(at.x, at.y, r, 0, Math.PI * 2);
                ctx.fill();

                // A crisp leading edge on the wavefront. The gradient alone
                // reads as a glow that happens to be getting bigger; an actual
                // travelling circle reads as a front sweeping outward, which is
                // the thing the walk is trying to show.
                ctx.strokeStyle = `rgba(255,238,222,${(0.4 * env).toFixed(3)})`;
                ctx.lineWidth = Math.max(0.8, 1.5 * env);
                ctx.beginPath();
                ctx.arc(at.x, at.y, r, 0, Math.PI * 2);
                ctx.stroke();

                // The debris. Fine and dim: it is the texture on the front,
                // not a second event happening alongside it.
                ctx.fillStyle = `rgba(${rgb},${(0.45 * env).toFixed(3)})`;
                for (const s of p.sparks) {
                    const d = r * s.spd;
                    ctx.beginPath();
                    ctx.arc(at.x + s.dx * d, at.y + s.dy * d, 1.15, 0, Math.PI * 2);
                    ctx.fill();
                }
                return true;
            });
            ctx.restore();
        }

        // ── Graph Walk: the travelling wavefront ───────────────
        //
        // The flat version of the 3D curtain (threeEmitSweep), and the
        // cascade's replacement for the ignition ring. A ring is the right
        // shape for a frontier that is a shell; once the walk is laid out as
        // columns the frontier is a *line*, and a ring big enough to reach it
        // has already swallowed everything nearer. So the front travels along
        // the flow instead: a bright vertical bar sweeping from the column it
        // left to the column about to ignite, with a lit wake behind it.
        function overlayEmitSweep(spec) {
            if (!spec || !Number.isFinite(spec.fromX) || !Number.isFinite(spec.toX)) return;
            const grow = Math.max(160, spec.growMs || 420);
            const sparks = [];
            for (let i = 0; i < 22; i++) {
                sparks.push({
                    at: Math.random(),               // where across the curtain
                    lag: Math.random() * 0.22,       // trails the front
                });
            }
            fxSweeps.push({
                ...spec,
                grow,
                fade: Math.min(560, Math.max(220, grow * 0.75)),
                t0: performance.now(),
                sparks,
            });
        }

        // Vertical falloff, drawn as a stack of slices. A single fillRect with
        // a horizontal gradient is one call but leaves the curtain with hard
        // ends; the graph is additive here, so the taper cannot be painted on
        // afterwards and has to be baked into the alpha as it goes down.
        const FX_SWEEP_SLICES = 12;
        function fxSweepBand(ctx, x0, x1, yTop, yBot, rgb, alpha, wake) {
            const h = (yBot - yTop) / FX_SWEEP_SLICES;
            for (let i = 0; i < FX_SWEEP_SLICES; i++) {
                // Smoothstep in from both ends over the outer sixth.
                const u = (i + 0.5) / FX_SWEEP_SLICES;
                const d = Math.min(u, 1 - u) / 0.16;
                const ends = d >= 1 ? 1 : d * d * (3 - 2 * d);
                const a = alpha * ends;
                if (a < 0.004) continue;
                const g = ctx.createLinearGradient(x0, 0, x1, 0);
                if (wake) {
                    g.addColorStop(0, `rgba(${rgb},0)`);
                    g.addColorStop(0.72, `rgba(${rgb},${(a * 0.45).toFixed(3)})`);
                    g.addColorStop(1, `rgba(${rgb},${a.toFixed(3)})`);
                } else {
                    g.addColorStop(0, `rgba(${rgb},0)`);
                    g.addColorStop(0.5, `rgba(${rgb},${a.toFixed(3)})`);
                    g.addColorStop(1, `rgba(${rgb},0)`);
                }
                ctx.fillStyle = g;
                ctx.fillRect(Math.min(x0, x1), yTop + i * h, Math.abs(x1 - x0), h + 1);
            }
        }

        function fxDrawSweeps() {
            if (!fxSweeps.length) return;
            const ctx = fxCtx;
            const now = performance.now();
            ctx.save();
            ctx.globalCompositeOperation = 'lighter';
            fxSweeps = fxSweeps.filter(s => {
                const t = now - s.t0;
                if (t >= s.grow + s.fade || !state.walkActive) return false;
                const p = Math.min(1, t / s.grow);
                const e = 1 - Math.pow(1 - p, 3);
                const env = t <= s.grow
                    ? Math.min(1, p / 0.22)
                    : Math.max(0, 1 - (t - s.grow) / s.fade);
                const x = s.fromX + (s.toX - s.fromX) * e;
                const a = cosmos.spaceToScreenPosition([x, s.top]);
                const b = cosmos.spaceToScreenPosition([x, s.bottom]);
                const o = cosmos.spaceToScreenPosition([s.fromX, s.top]);
                if (!a || !b || !o) return true;
                const yTop = Math.min(a[1], b[1]);
                const yBot = Math.max(a[1], b[1]);
                if (yBot - yTop < 2) return true;
                const [cr, cg, cb] = cosmosRgb(s.colour);
                const rgb = `${Math.round(cr * 255)},${Math.round(cg * 255)},${Math.round(cb * 255)}`;

                // The wake: everything the front has already passed over.
                if (Math.abs(a[0] - o[0]) > 2) {
                    fxSweepBand(ctx, o[0], a[0], yTop, yBot, rgb, 0.10 * env, true);
                }
                // The front's own glow, then a crisp near-white leading edge —
                // the bar is what reads as a wave arriving rather than a
                // gradient that happens to be moving.
                // Wider and dimmer than it was: the same amount of light
                // spread over more of the sweep reads as a front arriving
                // rather than as a strip light being switched on.
                const halo = Math.max(20, Math.abs(a[0] - o[0]) * 0.09);
                fxSweepBand(ctx, a[0] - halo, a[0] + halo, yTop, yBot, rgb, 0.3 * env, false);

                const edge = ctx.createLinearGradient(0, yTop, 0, yBot);
                edge.addColorStop(0, 'rgba(255,238,222,0)');
                edge.addColorStop(0.18, `rgba(255,238,222,${(0.5 * env).toFixed(3)})`);
                edge.addColorStop(0.82, `rgba(255,238,222,${(0.5 * env).toFixed(3)})`);
                edge.addColorStop(1, 'rgba(255,238,222,0)');
                ctx.strokeStyle = edge;
                ctx.lineWidth = Math.max(1, 1.6 * env);
                ctx.beginPath();
                ctx.moveTo(a[0], yTop);
                ctx.lineTo(a[0], yBot);
                ctx.stroke();

                // Debris riding the front, as short streaks pointing back the
                // way it came.
                ctx.strokeStyle = `rgba(${rgb},${(0.42 * env).toFixed(3)})`;
                ctx.lineWidth = 1.1;
                ctx.lineCap = 'round';
                const dir = s.toX >= s.fromX ? 1 : -1;
                ctx.beginPath();
                for (const sp of s.sparks) {
                    const bp = Math.max(0, e - sp.lag);
                    const sx = cosmos.spaceToScreenPosition(
                        [s.fromX + (s.toX - s.fromX) * bp, s.bottom + (s.top - s.bottom) * sp.at]);
                    if (!sx) continue;
                    ctx.moveTo(sx[0] - dir * 7, sx[1]);
                    ctx.lineTo(sx[0], sx[1]);
                }
                ctx.stroke();
                return true;
            });
            ctx.restore();
        }

        // ── Labels ─────────────────────────────────────────────
        // cosmos.gl renders no text, so this replaces both the SpriteText
        // labels and the distance-adaptive visibility rule. Density sampling
        // does the job zoom-distance did in 3D: getSampledPoints() returns a
        // spread-out subset (one per ~100 screen px), so a zoomed-out view
        // stays legible and names appear as you move in.
        function fxDrawLabels() {
            if (!state.showLabels) return;
            const ctx = fxCtx;
            const tourOn = tourState.active && tourState.routeIds.size > 0;
            const focusOn = !!state.focusNode;

            let nodes;
            if (tourOn) {
                // On a tour only the stops are named — the surrounding
                // neighbourhood stays present but anonymous.
                nodes = [];
                tourState.routeIds.forEach(id => {
                    const n = state.nodeById.get(id);
                    if (n) nodes.push(n);
                });
            } else if (focusOn) {
                nodes = [];
                state.focusSet.forEach(id => {
                    const n = state.nodeById.get(id);
                    if (n) nodes.push(n);
                });
            } else {
                const sampled = cosmos.getSampledPoints();
                nodes = (sampled && sampled.indices ? sampled.indices : [])
                    .map(i => cosmosNodes[i])
                    .filter(Boolean);
            }

            ctx.save();
            ctx.font = '11px "JetBrains Mono", monospace';
            ctx.textAlign = 'center';
            ctx.textBaseline = 'bottom';
            for (const n of nodes) {
                const { dim, tier } = nodeLightingFor(n);
                if (dim) continue;
                if (cosmosHidden && cosmosHidden[cosmosIndexOf.get(n.id)]) continue;
                const p = fxPos(n);
                if (!p || p.x < -80 || p.y < -20 || p.x > width + 80 || p.y > height + 20) continue;
                const label = truncateName(n.name);
                if (!label) continue;
                // Tour stops get their names inked in warm orange so the route
                // is readable at a glance.
                ctx.fillStyle = (tier === 'current' || tier === 'stop')
                    ? CANVAS.labelTour : CANVAS.label;
                // A dark rim keeps text legible where it crosses a bright node.
                ctx.strokeStyle = 'rgba(13,13,16,0.85)';
                ctx.lineWidth = 3;
                const y = p.y - fxRadius(n) - 4;
                ctx.strokeText(label, p.x, y);
                ctx.fillText(label, p.x, y);
            }
            ctx.restore();
        }

        // ── Folder cluster labels ──────────────────────────────
        // The by-folder layout is only legible if the islands are named. The
        // centres are computed by the layout itself, so they are exact and
        // fixed rather than a centroid chased frame by frame. At most
        // MAX_FOLDER_CLUSTERS of them, so this is cheap.
        function fxDrawClusterLabels() {
            if (state.layout2d !== 'folders' || !cosmosClusterNames.length) return;
            const pos = cosmosClusterCentres;
            if (!pos || !pos.length) return;
            const ctx = fxCtx;
            ctx.save();
            ctx.font = '600 12px "JetBrains Mono", monospace';
            ctx.textAlign = 'center';
            ctx.textBaseline = 'middle';
            for (let i = 0; i < cosmosClusterNames.length; i++) {
                const x = pos[i * 2], y = pos[i * 2 + 1];
                if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
                const p = cosmos.spaceToScreenPosition([x, y]);
                if (!p || p[0] < 0 || p[1] < 0 || p[0] > width || p[1] > height) continue;
                // The last path segment: the full path is what makes folders
                // distinct, but the leaf is what makes them recognisable.
                const full = cosmosClusterNames[i];
                const leaf = full.split('/').filter(Boolean).pop() || full;
                ctx.strokeStyle = 'rgba(13,13,16,0.9)';
                ctx.lineWidth = 3.5;
                ctx.strokeText(leaf, p[0], p[1]);
                ctx.fillStyle = 'rgba(233,233,240,0.92)';
                ctx.fillText(leaf, p[0], p[1]);
            }
            ctx.restore();
        }

        // ── Boundary frame ─────────────────────────────────────
        // The 3D renderer's dashed cube, flattened: in 2D a bounding *box* has
        // four edges and four labelled sides, so the six numbered faces become
        // the four the plane actually has. Reuses drawSevenSeg from the 3D
        // backend — it was already a plain canvas2D routine.
        function fxDrawBoundary() {
            const ext = computeExtent();
            if (!ext || !cosmos) return;
            const r = ext.radius * 1.08;
            const a = cosmos.spaceToScreenPosition([ext.cx - r, ext.cy - r]);
            const b = cosmos.spaceToScreenPosition([ext.cx + r, ext.cy + r]);
            if (!a || !b) return;
            const x0 = Math.min(a[0], b[0]), x1 = Math.max(a[0], b[0]);
            const y0 = Math.min(a[1], b[1]), y1 = Math.max(a[1], b[1]);
            const ctx = fxCtx;
            ctx.save();
            ctx.strokeStyle = 'rgba(143,163,184,0.35)';
            ctx.lineWidth = 1.4;
            ctx.setLineDash([10, 7]);
            ctx.strokeRect(x0, y0, x1 - x0, y1 - y0);
            ctx.setLineDash([]);
            // Corner ticks, so the frame reads as a measured box rather than a
            // plain border.
            ctx.strokeStyle = 'rgba(143,163,184,0.6)';
            ctx.lineWidth = 2;
            const k = Math.min(18, (x1 - x0) * 0.05);
            for (const [cx, cy, sx, sy] of [[x0, y0, 1, 1], [x1, y0, -1, 1], [x0, y1, 1, -1], [x1, y1, -1, -1]]) {
                ctx.beginPath();
                ctx.moveTo(cx + sx * k, cy);
                ctx.lineTo(cx, cy);
                ctx.lineTo(cx, cy + sy * k);
                ctx.stroke();
            }
            // Side numbers, matching the viewbar's colour coding for 1–4.
            const sides = [
                { num: 1, hex: '#f97316', x: (x0 + x1) / 2, y: y1 + 6 },
                { num: 2, hex: '#3a6ea5', x: x1 + 6, y: (y0 + y1) / 2 },
                { num: 3, hex: '#c2410c', x: (x0 + x1) / 2, y: y0 - 34 },
                { num: 4, hex: '#5b8fc9', x: x0 - 34, y: (y0 + y1) / 2 },
            ];
            for (const s of sides) {
                drawSevenSeg(ctx, s.num, s.x - 9, s.y, 18, 28, s.hex);
            }
            ctx.restore();
        }
