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
            fxDrawWalkEdges();
            fxDrawFlow();
            const hot = fxHotNodes();
            fxDrawHalos(hot);
            fxDrawSelection();
            fxDrawPulses();
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
                ctx.strokeStyle = `rgba(${rgb},0.20)`;
                ctx.lineWidth = 5;
                ctx.stroke();
                ctx.strokeStyle = `rgba(${rgb},0.85)`;
                ctx.lineWidth = 1.4;
                ctx.stroke();
            }
            ctx.restore();
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
                const at = fxPos(p.node);
                if (!at) return true;
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
