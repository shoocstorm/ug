        // ─── Renderer backend: 2D GPU force graph (cosmos.gl) ──────────────
        //
        // The scale renderer. cosmos.gl runs both the force simulation and the
        // drawing in WebGL2 shaders, so it holds graphs that the 3D backend
        // cannot: there is one instanced draw call for every point rather than
        // a THREE.Group of five objects each.
        //
        // What it costs is the third dimension — there is no z-axis, no orbit
        // camera and no scene to add meshes to. The face views, the boundary
        // cube and the orientation gizmo are reported unsupported through
        // `caps` and the viewbar hides them.
        //
        // Everything here is index-addressed: cosmos.gl has no node ids, only
        // positions in flat Float32Arrays, so this file owns the id ↔ index
        // map and rebuilds it whenever the view changes.

        let CosmosLib = null;      // { Graph, PointShape, LinkStyle }
        let cosmos = null;         // the mounted cosmos.gl Graph
        let cosmosNodes = [];      // point index → node object
        let cosmosEdges = [];      // link index → edge object
        let cosmosIndexOf = new Map();   // node id → point index
        let cosmosBuf = null;      // the live typed arrays handed to the GPU
        let cosmosHidden = null;   // Uint8Array: visibility as last uploaded
        let cosmosImageIndex = new Map();  // image key → atlas index
        let _cosmosSyncAt = 0;
        // The opening fit: one early frame so the graph arrives already framed,
        // then the settle fit tidies it once the layout stops.
        let _cosmosEarlyFit = false;
        let _cosmosStartedAt = 0;

        // cosmos.gl's simulation space. Positions outside [0, spaceSize] are
        // trouble (the library's own docs flag >4096 as an iOS crash), and the
        // circle seeding in transformData lands well inside it.
        const COSMOS_SPACE = 4096;

        // ─── Colour plumbing ───────────────────────────────────

        // '#rgb' | '#rrggbb' | 'rgb(…)' | 'rgba(…)' → [r, g, b] in 0..1.
        // cosmos.gl takes colour channels as floats, and every colour the style
        // rules produce is one of these three spellings.
        const _rgbCache = new Map();
        function cosmosRgb(css) {
            const hit = _rgbCache.get(css);
            if (hit) return hit;
            let out = [1, 1, 1];
            const s = String(css).trim();
            if (s[0] === '#') {
                const h = s.slice(1);
                const full = h.length === 3 ? h[0] + h[0] + h[1] + h[1] + h[2] + h[2] : h;
                const v = parseInt(full.slice(0, 6), 16);
                if (Number.isFinite(v)) out = [((v >> 16) & 255) / 255, ((v >> 8) & 255) / 255, (v & 255) / 255];
            } else {
                const m = s.match(/rgba?\(([^)]+)\)/);
                if (m) {
                    const p = m[1].split(',').map(x => parseFloat(x));
                    out = [(p[0] || 0) / 255, (p[1] || 0) / 255, (p[2] || 0) / 255];
                }
            }
            _rgbCache.set(css, out);
            return out;
        }

        // ─── Glyph atlas ───────────────────────────────────────

        // cosmos.gl does not tint point images — the fragment shader blends
        // them *above* the shape colour (`mix(shapeColor, imageColor, alpha)`).
        // So the sticker is split in two: the disc is the point's own Circle
        // shape, which keeps its per-node colour and can flare orange on
        // selection or take a walk hop's tint; the image carries only the dark
        // glyph and the boundary ring, both of which are the same whatever
        // colour the disc happens to be.
        //
        // That split is what makes a per-node dynamic colour possible at all —
        // baking the disc into the image would freeze it per node type.
        function cosmosImageKey(n) {
            if (!n.isBoundary) return n.group;
            const inbound = (n.boundaries || []).some(b => b.direction === 'Inbound');
            return n.group + (inbound ? '|in' : '|out');
        }

        function cosmosGlyphImage(key) {
            const [group, dir] = key.split('|');
            const S = 128;
            const c = document.createElement('canvas');
            c.width = c.height = S;
            const ctx = c.getContext('2d');

            // The glyph, drawn dark so it reads as a punch-through on whatever
            // colour the disc underneath happens to be. NODE_ICONS bodies are
            // authored in a 24-unit box, so centre and scale it to ~75% of the
            // point, leaving a margin for the boundary ring.
            ctx.save();
            ctx.translate(S / 2, S / 2);
            ctx.scale(S / 32, S / 32);
            ctx.translate(-12, -12);
            const body = NODE_ICONS[group] || '<circle cx="12" cy="12" r="6.5"/>';
            ctx.strokeStyle = 'rgba(24,24,30,0.95)';
            ctx.lineWidth = 2.2;
            ctx.lineCap = 'round';
            ctx.lineJoin = 'round';
            // The Interface glyph is a dashed diamond; Path2D doesn't carry
            // the dasharray attribute, so re-apply it here.
            if (body.includes('stroke-dasharray="3.2 2.6"')) ctx.setLineDash([3.2, 2.6]);
            ctx.stroke(new Path2D(body));
            ctx.restore();

            // System boundary: a dashed rim. In 3D this ring floats outside the
            // sticker; a point image cannot exceed its own quad, so here it
            // sits on the disc's edge instead — still a dashed ring, still not
            // the selection marker, still orthogonal to the node's type colour.
            if (dir) {
                ctx.strokeStyle = dir === 'in' ? BOUNDARY_IN_COLOR : BOUNDARY_OUT_COLOR;
                ctx.lineWidth = 6;
                ctx.setLineDash([8, 6]);
                ctx.lineCap = 'round';
                ctx.beginPath();
                ctx.arc(S / 2, S / 2, S / 2 - 5, 0, Math.PI * 2);
                ctx.stroke();
            }
            return ctx.getImageData(0, 0, S, S);
        }

        // Build one image per (type × boundary direction) actually present in
        // the view. Types the graph doesn't contain cost nothing.
        function cosmosBuildAtlas(nodes) {
            const keys = [];
            cosmosImageIndex = new Map();
            for (const n of nodes) {
                const k = cosmosImageKey(n);
                if (cosmosImageIndex.has(k)) continue;
                cosmosImageIndex.set(k, keys.length);
                keys.push(k);
            }
            cosmos.setImageData(keys.map(cosmosGlyphImage));
        }

        // ─── Buffers ───────────────────────────────────────────

        function cosmosBuild(view) {
            const nodes = view.nodes;
            const edges = view.edges;
            const n = nodes.length;

            cosmosNodes = nodes;
            cosmosEdges = edges;
            cosmosIndexOf = new Map();
            for (let i = 0; i < n; i++) cosmosIndexOf.set(nodes[i].id, i);

            const positions = new Float32Array(n * 2);
            const colors = new Float32Array(n * 4);
            const sizes = new Float32Array(n);
            const shapes = new Float32Array(n);
            const imageIdx = new Float32Array(n);
            const imageSizes = new Float32Array(n);
            cosmosHidden = new Uint8Array(n);

            // Links reference points by index. Endpoints are ids here (the raw
            // view) or node objects (once a layout has run over them), so
            // accept both — the same `.id || value` shape the style rules use.
            const src = [];
            for (const e of edges) {
                const s = cosmosIndexOf.get(e.source.id || e.source);
                const t = cosmosIndexOf.get(e.target.id || e.target);
                // transformData already drops edges with a missing endpoint;
                // this catches the solo view's freshly cloned ones.
                src.push(s === undefined || t === undefined ? -1 : s, s === undefined || t === undefined ? -1 : t);
            }
            const keep = [];
            for (let i = 0; i < edges.length; i++) if (src[i * 2] >= 0) keep.push(i);
            const links = new Float32Array(keep.length * 2);
            keep.forEach((ei, i) => { links[i * 2] = src[ei * 2]; links[i * 2 + 1] = src[ei * 2 + 1]; });
            cosmosEdges = keep.map(i => edges[i]);

            cosmosBuf = {
                positions, colors, sizes, shapes, imageIdx, imageSizes, links,
                linkColors: new Float32Array(cosmosEdges.length * 4),
                linkWidths: new Float32Array(cosmosEdges.length),
            };

            cosmosBuildAtlas(nodes);
            for (let i = 0; i < n; i++) {
                const node = nodes[i];
                positions[i * 2] = +node.x || 0;
                positions[i * 2 + 1] = +node.y || 0;
                shapes[i] = CosmosLib.PointShape.Circle;
                imageIdx[i] = cosmosImageIndex.get(cosmosImageKey(node)) ?? -1;
                sizes[i] = nodeRadiusFor(node);
                imageSizes[i] = sizes[i];
            }
            cosmosPaint();

            cosmos.setPointPositions(positions);
            cosmos.setPointShapes(shapes);
            cosmos.setPointImageIndices(imageIdx);
            cosmos.setPointImageSizes(imageSizes);
            cosmos.setPointSizes(sizes);
            cosmos.setPointColors(colors);
            cosmos.setLinks(links);
            cosmos.setLinkColors(cosmosBuf.linkColors);
            cosmos.setLinkWidths(cosmosBuf.linkWidths);
        }

        // Refresh the colour/width buffers from the shared style rules. Alpha
        // is where the dimming lives: cosmos.gl has no per-node material to
        // fade, so "lowlit" is simply a smaller colour alpha.
        function cosmosPaint() {
            const { colors, linkColors, linkWidths } = cosmosBuf;
            for (let i = 0; i < cosmosNodes.length; i++) {
                const n = cosmosNodes[i];
                const [r, g, b] = cosmosRgb(nodeColorFor(n));
                const { opacity } = nodeLightingFor(n);
                colors[i * 4] = r; colors[i * 4 + 1] = g; colors[i * 4 + 2] = b;
                colors[i * 4 + 3] = opacity;
            }
            for (let i = 0; i < cosmosEdges.length; i++) {
                const e = cosmosEdges[i];
                const [r, g, b] = cosmosRgb(linkColorFor(e));
                linkColors[i * 4] = r; linkColors[i * 4 + 1] = g; linkColors[i * 4 + 2] = b;
                // Strands read stronger on a dark ground, so they're pulled
                // back to keep nodes dominant — the same 0.38 the 3D renderer
                // applies globally, carried per link so invisibility is just
                // alpha 0 (cosmos.gl has no per-link visibility accessor).
                linkColors[i * 4 + 3] = linkVisibleFor(e) ? 0.38 : 0;
                linkWidths[i] = e.rel === 'Contains' ? 1.1 : 0.45;
            }
        }

        // Hidden points are moved to NaN, which cosmos.gl treats as *absent*:
        // excluded from the forces, from hover and from zoom targets. That is
        // exactly the isolate semantics a walk or a tour wants, and it is why
        // dimming (alpha) and hiding (NaN) are kept separate here.
        //
        // Only rewritten when visibility actually changed. Uploading positions
        // on every restyle would fight the running simulation, snapping every
        // node back to its last synced position on each hover.
        function cosmosApplyVisibility() {
            let changed = false;
            const { positions } = cosmosBuf;
            for (let i = 0; i < cosmosNodes.length; i++) {
                const n = cosmosNodes[i];
                const hide = nodeVisibleFor(n) ? 0 : 1;
                if (hide === cosmosHidden[i]) continue;
                cosmosHidden[i] = hide;
                changed = true;
                if (hide) {
                    positions[i * 2] = NaN;
                    positions[i * 2 + 1] = NaN;
                } else {
                    // Back to its last known-good spot — cosmosSync keeps
                    // n.x/n.y current for everything still on screen.
                    positions[i * 2] = +n.x || 0;
                    positions[i * 2 + 1] = +n.y || 0;
                }
            }
            if (changed) cosmos.setPointPositions(positions);
            return changed;
        }

        // Read the GPU's positions back onto the node objects.
        //
        // The rest of the page treats `n.x` / `n.y` as the truth — extent
        // maths, framing, the tooltip and the walk all read them — but under
        // cosmos.gl the simulation runs on the GPU and those fields would
        // otherwise stay frozen at their seeded values. Absent (NaN) points are
        // skipped so hiding a node never destroys the position it comes back to.
        function cosmosSync() {
            if (!cosmos || !cosmos.isReady) return;
            const p = cosmos.getPointPositions();
            if (!p || !p.length) return;
            for (let i = 0; i < cosmosNodes.length; i++) {
                const x = p[i * 2], y = p[i * 2 + 1];
                if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
                cosmosNodes[i].x = x;
                cosmosNodes[i].y = y;
                cosmosNodes[i].z = 0;
            }
        }

        // ─── Selection / highlight ─────────────────────────────

        // In cosmos.gl selection is configuration, not a method call: name the
        // indices and the shader greys out everything else. That replaces the
        // per-node material walk the 3D backend has to do.
        function cosmosApplyHighlight() {
            const focusIdx = state.selectedNode
                ? cosmosIndexOf.get(state.selectedNode.id)
                : undefined;
            const hot = [];
            state.highlightNodes.forEach(id => {
                const i = cosmosIndexOf.get(id);
                if (i !== undefined) hot.push(i);
            });
            if (focusIdx !== undefined) hot.push(focusIdx);
            const outlined = [];
            for (let i = 0; i < cosmosNodes.length; i++) {
                if (cosmosNodes[i].isBoundary) outlined.push(i);
            }
            cosmos.setConfigPartial({
                focusedPointIndex: focusIdx,
                // An empty array greys out *everything*, which is not what "no
                // hover" means — undefined is how the highlight is cleared.
                highlightedPointIndices: hot.length ? hot : undefined,
                outlinedPointIndices: outlined.length ? outlined : undefined,
            });
        }

        function cosmosIndicesFor(ids) {
            const out = [];
            ids.forEach(id => {
                const i = cosmosIndexOf.get(id);
                if (i !== undefined && !cosmosHidden[i]) out.push(i);
            });
            return out;
        }

        // ─── The backend ───────────────────────────────────────

        RENDERERS.cosmos = () => ({
            name: 'cosmos',
            // No third dimension, so no face projections, no orbit to spin and
            // no cube to draw. Reported rather than silently ignored: the
            // viewbar hides what this cannot do.
            // No third dimension: no face projections and no orbit to spin.
            // The bounding box survives the flattening, though — the FX overlay
            // draws it as a framed rectangle — so its toggle stays live.
            caps: { threeD: false, faceViews: false, autoSpin: false, boundaryCube: true },

            async mount(el, view) {
                CosmosLib = await import('./cosmos-vis.bundle.js');
                cosmos = new CosmosLib.Graph(el, {
                    spaceSize: COSMOS_SPACE,
                    backgroundColor: CANVAS.bg,
                    pointDefaultSize: 6,
                    pointOpacity: 1,
                    // The disc is the point's own shape; the glyph rides on top
                    // as an image. See cosmosGlyphImage.
                    pointDefaultShape: 0,
                    scalePointsOnZoom: true,
                    linkWidthScale: 1,
                    linkDefaultArrows: true,
                    linkArrowsSizeScale: 0.8,
                    curvedLinks: true,
                    linkGreyoutOpacity: 0.05,
                    pointGreyoutOpacity: 0.12,
                    renderHoveredPointRing: true,
                    hoveredPointRingColor: '#f96716',
                    focusedPointRingColor: '#ff3d00',
                    outlinedPointRingColor: BOUNDARY_IN_COLOR,
                    hoveredPointCursor: 'pointer',
                    enableDrag: true,
                    enableZoom: true,
                    fitViewOnInit: false,
                    // Reproducible layouts: the same graph settles the same way
                    // twice, which matters for a screenshot and for a demo.
                    randomSeed: 'ug',
                    // Tuned against the 3D renderer's d3-force settings:
                    // charge -70 / link distance 50 / velocityDecay 0.6.
                    simulationLinkDistance: 50,
                    simulationLinkSpring: 1,
                    simulationRepulsion: 0.6,
                    simulationGravity: 0.12,
                    simulationCenter: 0.1,
                    simulationFriction: 0.25,
                    // Higher cools *faster* (the config's wording is inverted:
                    // "smaller values ... cool down slower"). The 3D renderer
                    // caps itself at 100 ticks so a layout is never a wait;
                    // this is the 2D equivalent of that impatience.
                    simulationDecay: 25000,
                    onPointClick: (index, _pos, event) => {
                        const n = cosmosNodes[index];
                        if (n) handleNodeClick(event, n);
                    },
                    onPointMouseOver: (index) => {
                        const n = cosmosNodes[index];
                        if (n && pointerOverCanvas()) handleNodeHover(n);
                    },
                    onPointMouseOut: () => handleNodeHover(null),
                    onBackgroundClick: () => clearSelection(),
                    onMouseMove: (_i, _p, event) => {
                        if (!event) return;
                        state._mouse = {
                            x: event.pageX, y: event.pageY,
                            cx: event.clientX, cy: event.clientY,
                        };
                    },
                    onSimulationTick: () => {
                        if (!state._graphRevealed) {
                            requestAnimationFrame(() => requestAnimationFrame(graphReveal));
                        }
                        // Frame the graph as soon as it has a shape, rather than
                        // waiting for it to stop moving. Fitting only on settle
                        // is most of why the opening felt slow: the layout was
                        // already legible long before, but it was still a small
                        // knot off in a corner with the camera parked wide.
                        if (!_cosmosEarlyFit && performance.now() - _cosmosStartedAt > 220) {
                            _cosmosEarlyFit = true;
                            cosmos.fitView(220, 0.15);
                        }
                        // Throttled: a full position readback is a GPU stall,
                        // and nothing off-canvas needs per-frame accuracy.
                        const now = performance.now();
                        if (now - _cosmosSyncAt < 400) return;
                        _cosmosSyncAt = now;
                        cosmosSync();
                    },
                    onSimulationEnd: () => {
                        cosmosSync();
                        state._boxSettled = true;
                        if (!state._didFit) {
                            state._didFit = true;
                            cosmos.fitView(350, 0.15);
                        }
                        refreshModeLegend();
                        graphReveal();
                    },
                });

                await cosmos.ready;
                cosmosBuild(view);
                // Snap the first upload into place. The default 800 ms
                // transition tweens every point from nothing on load, which
                // reads as the graph arriving late rather than arriving.
                cosmos.render(undefined, 0);
                _cosmosEarlyFit = false;
                _cosmosStartedAt = performance.now();
                cosmos.start(1);
                cosmosApplyHighlight();
                // If the engine never ticks (an empty solo view, say), don't
                // leave the loading overlay up forever.
                setTimeout(graphReveal, 4000);
            },

            setData(view) {
                if (!cosmos) return;
                cosmosBuild(view);
                cosmos.render(undefined, 0);
                _cosmosEarlyFit = false;
                _cosmosStartedAt = performance.now();
                cosmos.start(1);
                cosmosApplyHighlight();
            },

            restyle() {
                if (!cosmos || !cosmosBuf) return;
                cosmosPaint();
                cosmos.setPointColors(cosmosBuf.colors);
                cosmos.setLinkColors(cosmosBuf.linkColors);
                cosmos.setLinkWidths(cosmosBuf.linkWidths);
                cosmosApplyVisibility();
                cosmosApplyHighlight();
                // Snap rather than animate: a restyle is a response to a hover
                // or a filter, and an 800 ms colour tween reads as lag.
                cosmos.render(undefined, 0);
            },

            // The canvas is sized by CSS (100% of #graph-3d), so cosmos.gl
            // picks the new size up itself; nothing to push.
            resize() {},

            frameAll(ms) { if (cosmos) cosmos.fitView(ms, 0.15); },

            // Every face projection collapses to the same 2D fit. The buttons
            // are hidden by caps, but the 1–6 keyboard shortcuts still land here.
            setView(_id, ms) { if (cosmos) cosmos.fitView(ms, 0.15); },

            frameNodes(ids, ms) {
                if (!cosmos) return;
                const idx = cosmosIndicesFor(ids);
                if (!idx.length) return;
                cosmos.fitViewByPointIndices(idx, ms, 0.2);
            },

            focusNode(n) {
                if (!cosmos) return;
                const i = cosmosIndexOf.get(n.id);
                if (i === undefined || cosmosHidden[i]) return;
                // Wait one frame so any panel toggles (info open/close) commit
                // their layout before the view moves.
                requestAnimationFrame(() => cosmos.zoomToPointByIndex(i, 800, 4));
            },

            zoomBy(factor) {
                if (!cosmos) return;
                // The 3D renderer's factor scales an orbit *radius*, so <1 is
                // closer. A zoom level is the reciprocal of that.
                cosmos.setZoomLevel(cosmos.getZoomLevel() / factor, 180);
            },

            // No camera to place broadside, so the 2D reading of "fly to this
            // stop" is: frame the stop with its neighbourhood and the hop on
            // either side, so where we came from and where we're going are both
            // on screen.
            flyToStop(stop, opts) {
                if (!cosmos) return;
                const ids = new Set([stop.node_id]);
                if (opts.prev) ids.add(opts.prev.node_id);
                if (opts.next) ids.add(opts.next.node_id);
                tourState.nearIds.forEach(id => ids.add(id));
                const idx = cosmosIndicesFor(ids);
                if (!idx.length) return;
                requestAnimationFrame(() => {
                    cosmos.fitViewByPointIndices(idx, opts.ms || 1100, 0.28);
                });
            },

            frameRoute(ms) {
                if (!cosmos || !tourState.data) return;
                const idx = cosmosIndicesFor(tourState.routeIds);
                if (idx.length < 2) return;
                cosmos.fitViewByPointIndices(idx, ms || 1400, 0.2);
            },

            // Both are 3D-only affordances; caps already hide their controls.
            setAutoSpin() {},
            setBoundaryVisible() {},

            // The walk's ignition burst is drawn by the 2D FX overlay.
            emitPulse(node, colour, fromR, toR, growMs) {
                overlayEmitPulse(node, colour, fromR, toR, growMs);
            },

            screenPos(n) {
                if (!cosmos || !n || !Number.isFinite(n.x)) return null;
                const p = cosmos.spaceToScreenPosition([n.x, n.y]);
                if (!p) return null;
                const canvas = cosmos.canvas || document.querySelector('#graph-3d canvas');
                const rect = canvas ? canvas.getBoundingClientRect() : { left: 0, top: 0 };
                return {
                    x: p[0] + rect.left + window.scrollX,
                    y: p[1] + rect.top + window.scrollY,
                };
            },

            dispose() {
                if (cosmos) {
                    try { cosmos.destroy(); } catch (err) { console.error(err); }
                }
                cosmos = null;
                cosmosBuf = null;
                cosmosNodes = [];
                cosmosEdges = [];
                cosmosIndexOf = new Map();
            },
        });
