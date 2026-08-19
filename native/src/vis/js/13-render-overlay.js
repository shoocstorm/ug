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

        function overlayStart() {
            fxCanvas = document.getElementById('fx-overlay');
            if (!fxCanvas) return;
            fxCtx = fxCanvas.getContext('2d');
            fxCanvas.hidden = false;
            if (fxRunning) return;
            fxRunning = true;
            const tick = () => {
                if (!fxRunning) return;
                requestAnimationFrame(tick);
                overlayDraw();
            };
            requestAnimationFrame(tick);
        }

        function overlayStop() {
            fxRunning = false;
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
        function fxDrawFlow() {
            const ctx = fxCtx;
            const now = performance.now();
            let drawn = 0;
            ctx.save();
            ctx.globalCompositeOperation = 'lighter';
            for (const e of cosmosEdges) {
                const count = linkParticlesFor(e);
                if (!count) continue;
                if (++drawn > FX_MAX_FLOW_LINKS) break;
                const s = state.nodeById.get(e.source.id || e.source);
                const t = state.nodeById.get(e.target.id || e.target);
                if (!s || !t) continue;
                const a = fxPos(s), b = fxPos(t);
                if (!a || !b) continue;
                const [r, g, bl] = cosmosRgb(linkParticleColorFor(e));
                ctx.fillStyle = `rgba(${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(bl * 255)},0.95)`;
                for (let i = 0; i < count; i++) {
                    // Evenly spaced along the strand, all sliding source→target
                    // at the same rate, so direction of travel is readable.
                    const phase = ((now * 0.0004) + i / count) % 1;
                    const x = a.x + (b.x - a.x) * phase;
                    const y = a.y + (b.y - a.y) * phase;
                    ctx.beginPath();
                    ctx.arc(x, y, 1.7, 0, Math.PI * 2);
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
            // Additive strokes stack. Twenty of them read as a route; a
            // thousand read as a floodlight, and every node underneath is
            // gone. The glow is calibrated for a frontier of a few dozen
            // edges, so past that the whole layer is faded in proportion —
            // and the wide soft pass, which is most of the accumulated light,
            // is dropped entirely rather than merely dimmed.
            const bulk = Math.min(state.walkEdgeKeys.size, FX_MAX_FLOW_LINKS);
            const k = Math.max(0.16, Math.min(1, 90 / Math.max(1, bulk)));
            const wideGlow = k > 0.5;
            ctx.save();
            ctx.globalCompositeOperation = 'lighter';
            ctx.lineCap = 'round';
            let drawn = 0;
            for (const e of cosmosEdges) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                const key = sId < tId ? sId + '|' + tId : tId + '|' + sId;
                if (!state.walkEdgeKeys.has(key)) continue;
                if (++drawn > FX_MAX_FLOW_LINKS) break;
                const s = state.nodeById.get(sId);
                const t = state.nodeById.get(tId);
                if (!s || !t) continue;
                const a = fxPos(s), b = fxPos(t);
                if (!a || !b) continue;
                // The hop colour of the end being *reached*, so a strand
                // carries the temperature of the frontier it fed.
                const hex = state.walkColors.get(tId) || state.walkColors.get(sId) || '#f97316';
                const [r, g, bl] = cosmosRgb(hex);
                const rgb = `${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(bl * 255)}`;
                ctx.beginPath();
                ctx.moveTo(a.x, a.y);
                ctx.lineTo(b.x, b.y);
                if (wideGlow) {
                    ctx.strokeStyle = `rgba(${rgb},${(0.20 * k).toFixed(3)})`;
                    ctx.lineWidth = 5;
                    ctx.stroke();
                }
                ctx.strokeStyle = `rgba(${rgb},${(0.85 * k).toFixed(3)})`;
                ctx.lineWidth = 1.4;
                ctx.stroke();
                // Which way the relationship points, said by the line itself.
                // The flow particles say it too, but flow is a toggle (R on
                // the walk bar) and it is off in every still — and a cascade
                // that cannot tell a caller from a callee is a picture of
                // connectivity, not of dependency, which is the one thing a
                // walk is asked for. Note this is the *edge's* direction, not
                // the direction the traversal happened to follow: an inbound
                // walk crosses edges backwards, and drawing the head the way
                // the frontier moved would invert the meaning of the graph.
                fxArrowHead(ctx, a, b, fxRadius(t), rgb, k);
            }
            ctx.restore();
        }

        // A filled head pointing along a→b, set back from b by `inset` pixels
        // so it lands on the rim of the target disc rather than under it.
        //
        // `k` is the crowding factor the strands are drawn at, and the head
        // takes it too, in both size and alpha. It keeps a floor — a direction
        // mark that fades to nothing has stopped saying the one thing it is
        // for — but it must not keep full strength either: five hundred bright
        // wedges over threads faded to 0.14 would make the heads the diagram
        // and the edges the background.
        function fxArrowHead(ctx, a, b, inset, rgb, k) {
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const len = Math.hypot(dx, dy);
            // No direction to draw (a self-loop, or two nodes on top of each
            // other), or a strand so short the head would be most of it.
            if (!len || len < inset + 12) return;
            const ux = dx / len;
            const uy = dy / len;
            const tipX = b.x - ux * inset;
            const tipY = b.y - uy * inset;
            // Scaled to the strand so short hops don't get a head bigger than
            // the gap they cross, and clamped so long ones stay a mark rather
            // than a wedge.
            const size = Math.max(6, Math.min(11, len * 0.14)) * (0.7 + 0.3 * k);
            const wing = size * 0.5;
            const baseX = tipX - ux * size;
            const baseY = tipY - uy * size;
            ctx.beginPath();
            ctx.moveTo(tipX, tipY);
            ctx.lineTo(baseX - uy * wing, baseY + ux * wing);
            ctx.lineTo(baseX + uy * wing, baseY - ux * wing);
            ctx.closePath();
            ctx.fillStyle = `rgba(${rgb},${Math.max(0.3, 0.95 * k).toFixed(3)})`;
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
            for (let i = 0; i < 48; i++) {
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
                g.addColorStop(0.75, `rgba(${rgb},${(0.30 * env).toFixed(3)})`);
                g.addColorStop(1, `rgba(255,243,232,${(0.95 * env).toFixed(3)})`);
                ctx.fillStyle = g;
                ctx.beginPath();
                ctx.arc(at.x, at.y, r, 0, Math.PI * 2);
                ctx.fill();

                // A crisp leading edge on the wavefront. The gradient alone
                // reads as a glow that happens to be getting bigger; an actual
                // travelling circle reads as a front sweeping outward, which is
                // the thing the walk is trying to show.
                ctx.strokeStyle = `rgba(255,243,232,${(0.75 * env).toFixed(3)})`;
                ctx.lineWidth = Math.max(1, 2.2 * env);
                ctx.beginPath();
                ctx.arc(at.x, at.y, r, 0, Math.PI * 2);
                ctx.stroke();

                // The debris.
                ctx.fillStyle = `rgba(${rgb},${(0.95 * env).toFixed(3)})`;
                for (const s of p.sparks) {
                    const d = r * s.spd;
                    ctx.beginPath();
                    ctx.arc(at.x + s.dx * d, at.y + s.dy * d, 1.7, 0, Math.PI * 2);
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
            for (let i = 0; i < 34; i++) {
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
                    fxSweepBand(ctx, o[0], a[0], yTop, yBot, rgb, 0.16 * env, true);
                }
                // The front's own glow, then a crisp near-white leading edge —
                // the bar is what reads as a wave arriving rather than a
                // gradient that happens to be moving.
                const halo = Math.max(14, Math.abs(a[0] - o[0]) * 0.06);
                fxSweepBand(ctx, a[0] - halo, a[0] + halo, yTop, yBot, rgb, 0.5 * env, false);

                const edge = ctx.createLinearGradient(0, yTop, 0, yBot);
                edge.addColorStop(0, 'rgba(255,243,232,0)');
                edge.addColorStop(0.18, `rgba(255,243,232,${(0.85 * env).toFixed(3)})`);
                edge.addColorStop(0.82, `rgba(255,243,232,${(0.85 * env).toFixed(3)})`);
                edge.addColorStop(1, 'rgba(255,243,232,0)');
                ctx.strokeStyle = edge;
                ctx.lineWidth = Math.max(1.4, 2.4 * env);
                ctx.beginPath();
                ctx.moveTo(a[0], yTop);
                ctx.lineTo(a[0], yBot);
                ctx.stroke();

                // Debris riding the front, as short streaks pointing back the
                // way it came.
                ctx.strokeStyle = `rgba(${rgb},${(0.9 * env).toFixed(3)})`;
                ctx.lineWidth = 1.6;
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
