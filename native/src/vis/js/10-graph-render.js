        // ─── 3D force graph & rendering ──────────────────────

        // Selected node / hovered neighbours flare to a hot saturated orange —
        // the brightest ink on the page; everything else keeps its type colour.
        function nodeColorFor(n) {
            if (state.selectedNode && n.id === state.selectedNode.id) return '#ff3d00';
            if (state.highlightNodes.has(n.id)) return '#f96716';
            // On a tour the other stops burn amber so the route reads as one
            // chain; everything else keeps its type colour and gets lowlit
            // by opacity instead (see bumpGraphStyles).
            if (tourTier(n.id) === 'stop') return '#fb923c';
            return config.getColor(n.group);
        }
        function linkColorFor(e) {
            if (state.highlightLinks.has(e)) return '#f96716';
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

        // Visibility accessors handed to the graph. Filters own the base
        // answer; a tour in "solo" mode narrows it further to the route,
        // which turns the walk into a standalone diagram of the answer.
        function nodeVisibleFor(n) {
            if (tourState.active && tourState.isolate && !tourState.routeIds.has(n.id)) return false;
            return !(state.nodeHidden && state.nodeHidden(n));
        }

        function linkVisibleFor(e) {
            if (tourState.active && tourState.isolate) {
                const sId = e.source.id || e.source;
                const tId = e.target.id || e.target;
                if (!(tourState.routeIds.has(sId) && tourState.routeIds.has(tId))) return false;
            }
            return !(state.linkHidden && state.linkHidden(e));
        }

        // Particles crawl along highlighted links, and along the tour route so
        // the walk has visible direction of travel.
        function linkParticlesFor(e) {
            if (state.highlightLinks.has(e)) return 4;
            if (tourState.active && isTourRouteEdge(e)) return 2;
            return 0;
        }

        function nodeRadiusFor(n) {
            return (config.nodeRadius[n.group] || 6) * 0.8;
        }

        // A soft circular glow texture (white radial gradient → transparent),
        // tinted per-node via the sprite colour. Built once and shared.
        function glowTexture() {
            if (_glowTex) return _glowTex;
            const c = document.createElement('canvas');
            c.width = c.height = 128;
            const ctx = c.getContext('2d');
            const g = ctx.createRadialGradient(64, 64, 0, 64, 64, 64);
            g.addColorStop(0, 'rgba(255,255,255,1)');
            g.addColorStop(0.2, 'rgba(255,255,255,0.65)');
            g.addColorStop(0.55, 'rgba(255,255,255,0.18)');
            g.addColorStop(1, 'rgba(255,255,255,0)');
            ctx.fillStyle = g;
            ctx.fillRect(0, 0, 128, 128);
            _glowTex = new THREE.CanvasTexture(c);
            _glowTex.minFilter = THREE.LinearFilter;
            _glowTex.generateMipmaps = false;
            return _glowTex;
        }

        // A thin bright ring texture for the animated selection marker.
        function ringTexture() {
            if (_ringTex) return _ringTex;
            const c = document.createElement('canvas');
            c.width = c.height = 256;
            const ctx = c.getContext('2d');
            ctx.strokeStyle = 'rgba(255,255,255,1)';
            ctx.lineWidth = 10;
            ctx.beginPath();
            ctx.arc(128, 128, 104, 0, Math.PI * 2);
            ctx.stroke();
            // four short tick marks for a "targeting" feel
            ctx.lineWidth = 14;
            for (let a = 0; a < 4; a++) {
                const ang = a * Math.PI / 2;
                ctx.beginPath();
                ctx.arc(128, 128, 122, ang - 0.12, ang + 0.12);
                ctx.stroke();
            }
            _ringTex = new THREE.CanvasTexture(c);
            _ringTex.minFilter = THREE.LinearFilter;
            _ringTex.generateMipmaps = false;
            return _ringTex;
        }

        // Scene backdrop: a near-black ground with soft out-of-focus washes of
        // the two ink families (orange / steel blue). Dark makes the saturated
        // node colours glow instead of sitting flat, and gives the dimmed
        // rings of a tour somewhere to disappear into.
        function backgroundTexture() {
            const c = document.createElement('canvas');
            c.width = c.height = 1024;
            const ctx = c.getContext('2d');
            ctx.fillStyle = CANVAS.bg;
            ctx.fillRect(0, 0, 1024, 1024);
            const glow = (x, y, r, color) => {
                const g = ctx.createRadialGradient(x, y, 0, x, y, r);
                g.addColorStop(0, color);
                g.addColorStop(1, 'rgba(13,13,16,0)');
                ctx.fillStyle = g;
                ctx.fillRect(0, 0, 1024, 1024);
            };
            glow(790, 190, 620, 'rgba(233,100,28,0.16)');   // warm haze, top-right
            glow(170, 780, 640, 'rgba(58,110,165,0.20)');   // blue haze, bottom-left
            glow(320, 150, 460, 'rgba(91,143,201,0.12)');   // pale blue, top-left
            glow(880, 880, 520, 'rgba(194,65,12,0.12)');    // rust haze, bottom-right
            glow(512, 512, 760, 'rgba(120,130,150,0.10)');  // faint lift mid-frame
            const t = new THREE.CanvasTexture(c);
            t.colorSpace = THREE.SRGBColorSpace;
            t.minFilter = THREE.LinearFilter;
            t.generateMipmaps = false;
            return t;
        }

        // Custom node object: an unlit (self-illuminated) sphere so nodes stay
        // saturated from any camera angle, plus a soft tinted halo (a normal-
        // blend colour wash — additive glow would vanish against the white
        // paper), a translucent "membrane" shell on the larger cell-like
        // nodes, and an optional text label.
        function makeNodeObject(n) {
            const radius = nodeRadiusFor(n);
            const seg = state.perfMode ? 6 : 16;
            const group = new THREE.Group();

            const mat = new THREE.MeshBasicMaterial({ color: nodeColorFor(n), transparent: true, opacity: 0.95 });
            const core = new THREE.Mesh(new THREE.SphereGeometry(radius, seg, seg), mat);
            n.__nodeMat = mat;
            n.__nodeCore = core;
            n.__nodeRadius = radius;
            group.add(core);

            if (!state.perfMode) {
                // Soft tinted radial-gradient halo — reads as the out-of-focus
                // ink bleed around every node in the reference art.
                const halo = new THREE.Sprite(new THREE.SpriteMaterial({
                    map: glowTexture(),
                    color: config.getColor(n.group),
                    transparent: true,
                    opacity: 0.3,
                    depthWrite: false,
                }));
                n.__haloBase = radius * 4.5;
                halo.scale.setScalar(n.__haloBase);
                n.__nodeHalo = halo;
                group.add(halo);

                // Larger nodes get a translucent outer shell — the nucleus-
                // inside-a-membrane look the big cells in the reference have.
                if (radius >= 6) {
                    const shell = new THREE.Mesh(
                        new THREE.SphereGeometry(radius * 1.65, seg, seg),
                        new THREE.MeshBasicMaterial({
                            color: config.getColor(n.group),
                            transparent: true,
                            opacity: 0.14,
                            depthWrite: false,
                        }));
                    // Kept on the node so dimming (focus / tour) can fade the
                    // shell too — otherwise big nodes stay visible through it.
                    n.__nodeShell = shell;
                    n.__shellBase = 0.14;
                    group.add(shell);
                }
            }

            if (!state.skipLabels) {
                const label = truncateName(n.name);
                if (label) {
                    const s = new SpriteText(label);
                    s.color = CANVAS.label;
                    s.fontFace = 'JetBrains Mono, monospace';
                    s.textHeight = 3.5;
                    s.material.depthWrite = false;
                    // Sprite-text labels are NPOT canvas textures; skip mipmaps to
                    // avoid blurry text and unsupported-mipmap paths on some GPUs.
                    if (s.material.map) {
                        s.material.map.generateMipmaps = false;
                        s.material.map.minFilter = THREE.LinearFilter;
                        s.material.map.needsUpdate = true;
                    }
                    s.position.set(0, radius + 6, 0);
                    n.__nodeLabel = s;
                    group.add(s);
                }
            }
            return group;
        }

        // A d3-force that constrains nodes' Z extent to half of the Y range
        // so the graph reads as a box half as deep as it is tall. Nodes are
        // initialized with random Z offsets so they naturally spread into the
        // volume; the charge force handles inter-node spacing in 3D.
        function makeZBoundForce() {
            let nodes = [];
            const force = (alpha) => {
                let minY = Infinity, maxY = -Infinity;
                for (const n of nodes) {
                    const y = n.y || 0;
                    if (y < minY) minY = y;
                    if (y > maxY) maxY = y;
                }
                const maxZ = Math.max((maxY - minY) / 2, 15);
                for (const n of nodes) {
                    if (n.z > maxZ) { n.z = maxZ; if (n.vz > 0) n.vz = 0; }
                    else if (n.z < -maxZ) { n.z = -maxZ; if (n.vz < 0) n.vz = 0; }
                }
            };
            force.initialize = (n) => {
                nodes = n;
                for (const node of nodes) {
                    node.z = (Math.random() - 0.5) * 30;
                }
            };
            return force;
        }

        function createGraph() {
            const el = document.getElementById('graph-3d');
            window.addEventListener('mousemove', e => {
                // page coords position the tooltip; client coords feed the
                // pointerOverCanvas hit-test (elementFromPoint wants viewport
                // coordinates — identical here, but kept separate for safety).
                state._mouse = { x: e.pageX, y: e.pageY, cx: e.clientX, cy: e.clientY };
                // A hover struck on-canvas would otherwise stick (highlight +
                // tooltip) when the pointer crosses into a panel, because the
                // renderer never reports a leave for occluded pixels.
                if (state._hoverNode && !pointerOverCanvas()) handleNodeHover(null);
            });

            Graph = ForceGraph3D({ controlType: 'orbit' })(el)
                .backgroundColor(CANVAS.bg)
                .graphData({ nodes: state.graph.nodes, links: state.graph.edges })
                .nodeId('id')
                // Suppress the library's own hover tooltip. Its `nodeLabel`
                // accessor defaults to the `name` field, so leaving it unset
                // put a second, smaller box next to ours on every hover —
                // the same name, with none of the type or metrics. The
                // tooltip we want is `#tooltip`, built in handleNodeHover.
                // An empty label is how force-graph is told not to draw one:
                // it sets `display: none` whenever the content is falsy.
                .nodeLabel(() => '')
                .nodeVisibility(nodeVisibleFor)
                .nodeThreeObject(makeNodeObject)
                .linkColor(linkColorFor)
                // Strands read stronger on a dark ground than they did on
                // paper, so they're pulled back to keep nodes dominant.
                .linkOpacity(0.38)
                // Containment edges are the thick orange branches of the
                // reference art; everything else stays a fine strand.
                .linkWidth(e => e.rel === 'Contains' ? 1.1 : 0.45)
                .linkVisibility(linkVisibleFor)
                .linkDirectionalArrowLength(state.perfMode ? 0 : 3)
                .linkDirectionalArrowRelPos(1)
                .linkDirectionalParticles(linkParticlesFor)
                .linkDirectionalParticleWidth(1.6)
                .linkDirectionalParticleSpeed(0.012)
                .linkDirectionalParticleColor(() => '#ff3d00')
                .enableNodeDrag(true)
                .onNodeHover(handleNodeHover)
                .onNodeClick((n, evt) => handleNodeClick(evt, n))
                .onBackgroundClick(() => clearSelection())
                .width(width)
                .height(height);

            const charge = Graph.d3Force('charge');
            // smaller charge in perf mode so the layout doesn't explode outward and the camera doesn't have to fly miles away to fit it all.
            if (charge) charge.strength(state.perfMode ? -50 : -70);
            // shorter Link distance, means closer connected nodes
            const linkForce = Graph.d3Force('link');
            if (linkForce) linkForce.distance(state.perfMode ? 35 : 50);
            Graph.d3Force('zBound', makeZBoundForce());

            // Converge fast so the user isn't waiting for the layout to settle:
            // steeper alpha decay + extra velocity friction stop the slow outward
            // drift much sooner, and the cooldown cap guarantees the engine quits
            // within a bounded number of ticks regardless of graph size.
            Graph.d3AlphaDecay(state.perfMode ? 0.05 : 0.07);
            Graph.d3VelocityDecay(0.6);
            Graph.cooldownTicks(state.perfMode ? 80 : 100);

            // Subtle gradient backdrop (in-scene, so it sits behind the graph and
            // reads as blurred ambient washes instead of a flat white void).
            Graph.scene().background = backgroundTexture();

            // Fog doubles as depth-of-field: far nodes and strands melt into
            // the ground tone, so depth reads without needing bloom.
            Graph.scene().fog = new THREE.FogExp2(CANVAS.fog, 0.001);

            // Animated selection marker: a spinning, pulsing ring that sits on the
            // currently selected node (added to the scene, repositioned each frame).
            if (!state.perfMode) {
                selectionRing = new THREE.Sprite(new THREE.SpriteMaterial({
                    map: ringTexture(),
                    color: 0xff3d00,
                    transparent: true,
                    opacity: 0,
                    depthWrite: false,
                    depthTest: false,
                }));
                selectionRing.visible = false;
                selectionRing.renderOrder = 999;
                Graph.scene().add(selectionRing);
                startSelectionAnimation();
            }

            // Frame the bulk of the graph once the layout settles. Use frameGraph
            // (percentile-based) rather than zoomToFit so a few far-flung outlier
            // nodes don't force the camera miles away.
            const autoFrame = () => {
                // Recompute fog/label cues on every settle (cheap). The boundary
                // box must be rebuilt on every settle too — sizing it once from an
                // early snapshot leaves nodes outside it as the layout expands.
                applyDepthCues();
                if (!state.perfMode) { updateBoundaryCube(); updateParticleField(); }
                // Only fly the camera the first time.
                if (state._didFit) return;
                state._didFit = true;
                frameGraph(900);
            };
            // Mark the layout as truly settled only on a real engine stop (the
            // timeout fallbacks below may fire while nodes are still moving).
            Graph.onEngineStop(() => { state._boxSettled = true; autoFrame(); });
            // Fallbacks: settle may take a moment, so frame early too.
            setTimeout(autoFrame, 2500);
            setTimeout(autoFrame, 5000);

            // While the layout is still expanding, keep the boundary box enclosing
            // the cloud — nodes drift outward before the sim settles, so a box from
            // an early snapshot would leave them poking out. Throttled, and only
            // until the first real settle.
            if (!state.perfMode) {
                Graph.onEngineTick(() => {
                    if (state._boxSettled || !state.showBoundary) return;
                    const now = performance.now();
                    if (now - (state._lastBoxFit || 0) < 150) return;
                    state._lastBoxFit = now;
                    updateBoundaryCube();
                });
            }

            // Drives the orientation gizmo + distance-adaptive labels.
            startOverlayLoop();
        }

        // Centroid + a robust (90th-percentile) radius of the laid-out graph,
        // ignoring far-flung outliers. Shared by camera framing and depth cues.
        function computeExtent() {
            const nodes = state.graph.nodes.filter(n => Number.isFinite(n.x));
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

        // Fog and label-distance must track the graph's actual size. The fog was
        // originally a fixed exponential density tuned for a small graph, which
        // blacked out everything on a large one ("not enough light"). Scale the
        // density to the framing distance so depth reads consistently at any size.
        function applyDepthCues() {
            const ext = computeExtent();
            if (!ext) return;
            state._graphRadius = ext.radius;
            // Labels visible within ~half the graph radius of the camera; never
            // below the small-graph default so tiny graphs keep their labels.
            state._labelDist = Math.max(340, ext.radius * 0.5);
            const fog = Graph && Graph.scene().fog;
            if (fog) {
                // Exponential fade into the paper-white, keyed to size — this is
                // the depth-of-field of the reference art. Capped so small
                // graphs keep everything crisp.
                fog.density = Math.min(0.001, 0.16 / ext.radius);
            }
        }

        // The six face views (id → outward camera direction + the coordinate
        // plane that face lies in). Numbers match the digi labels on the boundary
        // box, so "view N" flies the camera to look straight at face N.
        const VIEWS = {
            '1': { dir: [0, 0, 1], plane: 'XY' },   // +Z front
            '2': { dir: [1, 0, 0], plane: 'YZ' },   // +X right
            '3': { dir: [0, 0, -1], plane: 'XY' },  // -Z back
            '4': { dir: [-1, 0, 0], plane: 'YZ' },  // -X left
            '5': { dir: [0, 1, 0], plane: 'XZ' },   // +Y top
            '6': { dir: [0, -1, 0], plane: 'XZ' },  // -Y bottom
        };

        // Snap the camera to a predefined view. id: '1'–'6' (face projections) or
        // '3d' (isometric).
        function setView(id, ms = 600) {
            if (!Graph) return;
            const ext = computeExtent();
            if (!ext) return;
            const { cx, cy, cz, radius } = ext;
            const fov = (Graph.camera().fov || 50) * Math.PI / 180;
            const aspect = (width && height) ? (width / height) : 1.6;
            const fitFov = aspect >= 1 ? fov : 2 * Math.atan(Math.tan(fov / 2) * aspect);
            const dist = (radius * 1.05) / Math.tan(fitFov / 2);
            const v = VIEWS[id];
            let pos;
            if (v) {
                pos = { x: cx + v.dir[0] * dist, y: cy + v.dir[1] * dist, z: cz + v.dir[2] * dist };
            } else {
                const d = dist / Math.sqrt(3);
                pos = { x: cx + d, y: cy + d, z: cz + d };
            }
            Graph.cameraPosition(pos, { x: cx, y: cy, z: cz }, ms);
        }

        // Frame the graph at the default 3D isometric view.
        function frameGraph(ms = 600) { setView('3d', ms); }

        // Scale the camera's orbit radius by `factor` (mirrors the mouse-wheel
        // dolly: <1 zooms in, >1 zooms out). Keeps the same look-at target so the
        // view direction is preserved; only the distance changes.
        function zoomBy(factor) {
            if (!Graph) return;
            const controls = Graph.controls && Graph.controls();
            const cam = Graph.camera && Graph.camera();
            if (!controls || !cam) return;
            const t = controls.target;
            const p = cam.position;
            const dx = p.x - t.x, dy = p.y - t.y, dz = p.z - t.z;
            const dist = Math.hypot(dx, dy, dz) || 1;
            // Floor so the camera can't cross the orbit target.
            const newDist = Math.max(dist * factor, Math.max(dist * 0.05, 2));
            const k = newDist / dist;
            Graph.cameraPosition(
                { x: t.x + dx * k, y: t.y + dy * k, z: t.z + dz * k },
                { x: t.x, y: t.y, z: t.z },
                180
            );
        }

        // Dashed wireframe cube showing the graph bounding box as spatial reference.
        function makeAxisLabel(text, hex) {
            const c = document.createElement('canvas');
            c.width = 64; c.height = 64;
            const ctx = c.getContext('2d');
            ctx.clearRect(0, 0, 64, 64);
            ctx.font = 'bold 44px Inter, sans-serif';
            ctx.textAlign = 'center';
            ctx.textBaseline = 'middle';
            ctx.shadowColor = 'rgba(255,255,255,0.9)';
            ctx.shadowBlur = 6;
            ctx.fillStyle = hex;
            ctx.fillText(text, 32, 34);
            ctx.shadowBlur = 0;
            ctx.fillText(text, 32, 34);
            const tex = new THREE.CanvasTexture(c);
            tex.minFilter = THREE.LinearFilter;
            tex.generateMipmaps = false;
            const mat = new THREE.SpriteMaterial({ map: tex, transparent: true, depthWrite: false, depthTest: false, sizeAttenuation: true });
            const sprite = new THREE.Sprite(mat);
            sprite.scale.setScalar(14);
            return sprite;
        }

        // Draw a single seven-segment digit into ctx within the box (x,y,bw,bh),
        // glowing in `hex`; off-segments are drawn very faintly for the classic
        // digital-display look.
        function drawSevenSeg(ctx, digit, x, y, bw, bh, hex) {
            const map = {
                0: 'abcdef', 1: 'bc', 2: 'abged', 3: 'abgcd', 4: 'fgbc',
                5: 'afgcd', 6: 'afgecd', 7: 'abc', 8: 'abcdefg', 9: 'abcdfg',
            };
            const on = new Set((map[digit] || '').split(''));
            const t = Math.min(bw, bh) * 0.16;     // segment thickness
            const g = t * 0.34;                     // gap between segments
            const midY = y + bh / 2;
            // [orientation, x, y, length] for each segment (h = horizontal, v = vertical)
            const segs = {
                a: ['h', x + g, y, bw - 2 * g],
                g: ['h', x + g, midY - t / 2, bw - 2 * g],
                d: ['h', x + g, y + bh - t, bw - 2 * g],
                f: ['v', x, y + g, bh / 2 - 1.5 * g],
                b: ['v', x + bw - t, y + g, bh / 2 - 1.5 * g],
                e: ['v', x, midY + g / 2, bh / 2 - 1.5 * g],
                c: ['v', x + bw - t, midY + g / 2, bh / 2 - 1.5 * g],
            };
            const cap = (px, py, len, horiz) => {
                // A tapered capsule so segments look like a real LCD font.
                const th = t, half = th / 2;
                ctx.beginPath();
                if (horiz) {
                    ctx.moveTo(px, py + half);
                    ctx.lineTo(px + half, py);
                    ctx.lineTo(px + len - half, py);
                    ctx.lineTo(px + len, py + half);
                    ctx.lineTo(px + len - half, py + th);
                    ctx.lineTo(px + half, py + th);
                } else {
                    ctx.moveTo(px + half, py);
                    ctx.lineTo(px + th, py + half);
                    ctx.lineTo(px + th, py + len - half);
                    ctx.lineTo(px + half, py + len);
                    ctx.lineTo(px, py + len - half);
                    ctx.lineTo(px, py + half);
                }
                ctx.closePath();
                ctx.fill();
            };
            for (const key of Object.keys(segs)) {
                const [orient, sx, sy, len] = segs[key];
                const lit = on.has(key);
                ctx.save();
                if (lit) {
                    ctx.fillStyle = hex;
                    ctx.shadowColor = hex;
                    ctx.shadowBlur = t * 1.4;
                } else {
                    ctx.fillStyle = 'rgba(71,85,105,0.12)';
                    ctx.shadowBlur = 0;
                }
                cap(sx, sy, len, orient === 'h');
                ctx.restore();
            }
        }

        // A stylish "digi" label sprite: a glowing seven-segment number inside a
        // rounded HUD frame, tinted to the face colour. Used to tag cube faces.
        function makeDigiLabel(num, hex) {
            const W = 150, H = 190;
            const c = document.createElement('canvas');
            c.width = W; c.height = H;
            const ctx = c.getContext('2d');
            // Rounded frame + faint panel.
            const r = 18, pad = 8;
            ctx.beginPath();
            ctx.roundRect(pad, pad, W - 2 * pad, H - 2 * pad, r);
            // Faint glass on the dark ground — a milky fill would read as a
            // bright rectangle floating in the scene.
            ctx.fillStyle = 'rgba(255,255,255,0.05)';
            ctx.fill();
            ctx.lineWidth = 4;
            ctx.strokeStyle = hex;
            ctx.shadowColor = hex;
            ctx.shadowBlur = 14;
            ctx.stroke();
            // The digit, centred with margin for the frame.
            drawSevenSeg(ctx, num, W * 0.34, H * 0.26, W * 0.32, H * 0.48, hex);
            const tex = new THREE.CanvasTexture(c);
            tex.colorSpace = THREE.SRGBColorSpace;
            tex.minFilter = THREE.LinearFilter;
            tex.generateMipmaps = false;
            const mat = new THREE.SpriteMaterial({ map: tex, transparent: true, depthWrite: false });
            const sprite = new THREE.Sprite(mat);
            sprite.userData.aspect = W / H;
            return sprite;
        }

        function updateBoundaryCube() {
            if (!Graph) return;
            const nodes = state.graph.nodes.filter(n => Number.isFinite(n.x));
            if (!nodes.length) return;
            let minX = Infinity, maxX = -Infinity;
            let minY = Infinity, maxY = -Infinity;
            let minZ = Infinity, maxZ = -Infinity;
            for (const n of nodes) {
                const x = n.x || 0, y = n.y || 0, z = n.z || 0;
                if (x < minX) minX = x; if (x > maxX) maxX = x;
                if (y < minY) minY = y; if (y > maxY) maxY = y;
                if (z < minZ) minZ = z; if (z > maxZ) maxZ = z;
            }
            // Pad the extents so nodes sitting on the surface (and their sphere
            // radius / labels) read as clearly *inside* the box rather than
            // straddling the boundary plane.
            const pad = Math.max(Math.hypot(maxX - minX, maxY - minY, maxZ - minZ) * 0.03, 14);
            minX -= pad; maxX += pad;
            minY -= pad; maxY += pad;
            minZ -= pad; maxZ += pad;
            const w = maxX - minX || 1;
            const h = maxY - minY || 1;
            const d = maxZ - minZ || 1;
            const cx = (minX + maxX) / 2;
            const cy = (minY + maxY) / 2;
            const cz = (minZ + maxZ) / 2;
            // Dispose the previous cube's GPU resources before discarding it —
            // this function now rebuilds repeatedly while the layout settles.
            if (boundaryCube) {
                Graph.scene().remove(boundaryCube);
                boundaryCube.traverse(o => {
                    if (o.geometry) o.geometry.dispose();
                    if (o.material) {
                        if (o.material.map) o.material.map.dispose();
                        o.material.dispose();
                    }
                });
                boundaryCube = null;
            }

            const group = new THREE.Group();

            // Dashed boundary cube. WebGL ignores LineDashedMaterial.linewidth, so
            // thin lines can't be thickened — instead each of the 12 edges is drawn
            // as a row of short cylinders ("dashes") that have real 3D thickness,
            // plus a sphere at each of the 8 corners so the box reads clearly.
            const accent = 0x8fa3b8; // quiet slate so the frame recedes behind the ink
            const diag = Math.hypot(w, h, d) || 1;
            const tubeR = Math.max(0.9, Math.min(6, diag * 0.0028));
            const cubeMat = new THREE.MeshBasicMaterial({
                color: accent, transparent: true, opacity: 0.22, depthWrite: false,
            });
            // Shared geometries: a unit-height cylinder (scaled per dash) and a
            // corner sphere — so the whole cube is a handful of geometries.
            const dashGeo = new THREE.CylinderGeometry(tubeR, tubeR, 1, 6);
            const cornerGeo = new THREE.SphereGeometry(tubeR * 2.1, 10, 10);

            const corners = [];
            for (const sx of [-1, 1]) for (const sy of [-1, 1]) for (const sz of [-1, 1]) {
                corners.push(new THREE.Vector3(cx + sx * w / 2, cy + sy * h / 2, cz + sz * d / 2));
            }
            // Edges join corners differing in exactly one axis bit (x=4, y=2, z=1).
            const edgePairs = [
                [0, 1], [2, 3], [4, 5], [6, 7], // z
                [0, 2], [1, 3], [4, 6], [5, 7], // y
                [0, 4], [1, 5], [2, 6], [3, 7], // x
            ];
            const up = new THREE.Vector3(0, 1, 0);
            const q = new THREE.Quaternion();
            const dir = new THREE.Vector3();
            for (const [ai, bi] of edgePairs) {
                const A = corners[ai], B = corners[bi];
                dir.subVectors(B, A);
                const len = dir.length() || 1;
                dir.normalize();
                q.setFromUnitVectors(up, dir);
                // ~30-world-unit cells, clamped so the dash count stays bounded.
                const nCells = Math.min(24, Math.max(6, Math.round(len / 30)));
                const cell = len / nCells;
                const dashLen = cell * 0.6; // 60% dash, 40% gap
                for (let i = 0; i < nCells; i++) {
                    const mid = i * cell + dashLen / 2;
                    const m = new THREE.Mesh(dashGeo, cubeMat);
                    m.position.copy(A).addScaledVector(dir, mid);
                    m.quaternion.copy(q);
                    m.scale.set(1, dashLen, 1);
                    group.add(m);
                }
            }
            for (const c of corners) {
                const s = new THREE.Mesh(cornerGeo, cubeMat);
                s.position.copy(c);
                group.add(s);
            }

            // Translucent coloured face planes, each tagged with a glowing
            // seven-segment "digi" number so the six boundary planes are
            // individually identifiable.
            // Numbered to match the bottom view bar: view N looks at face N.
            const hw = w / 2, hh = h / 2, hd = d / 2;
            const faceDefs = [
                { num: 1, hex: '#f97316', pos: [cx, cy, cz + hd], rot: [0, 0, 0], size: [w, h] }, // +Z  XY front
                { num: 2, hex: '#3a6ea5', pos: [cx + hw, cy, cz], rot: [0, Math.PI / 2, 0], size: [d, h] }, // +X  YZ right
                { num: 3, hex: '#c2410c', pos: [cx, cy, cz - hd], rot: [0, Math.PI, 0], size: [w, h] }, // -Z  XY back
                { num: 4, hex: '#5b8fc9', pos: [cx - hw, cy, cz], rot: [0, -Math.PI / 2, 0], size: [d, h] }, // -X  YZ left
                { num: 5, hex: '#f59e0b', pos: [cx, cy + hh, cz], rot: [-Math.PI / 2, 0, 0], size: [w, d] }, // +Y  XZ top
                { num: 6, hex: '#2f5f96', pos: [cx, cy - hh, cz], rot: [Math.PI / 2, 0, 0], size: [w, d] }, // -Y  XZ bottom
            ];
            const labelScale = Math.max(16, Math.min(150, Math.min(w, h, d) * 0.18));
            for (const f of faceDefs) {
                const plane = new THREE.Mesh(
                    new THREE.PlaneGeometry(f.size[0], f.size[1]),
                    new THREE.MeshBasicMaterial({
                        color: f.hex, transparent: true, opacity: 0.05,
                        side: THREE.DoubleSide, depthWrite: false,
                    })
                );
                plane.position.set(f.pos[0], f.pos[1], f.pos[2]);
                plane.rotation.set(f.rot[0], f.rot[1], f.rot[2]);
                group.add(plane);

                const label = makeDigiLabel(f.num, f.hex);
                label.position.set(f.pos[0], f.pos[1], f.pos[2]);
                const a = label.userData.aspect || 0.8;
                label.scale.set(labelScale * a, labelScale, 1);
                group.add(label);
            }

            // Axis indicator at the min corner
            const origin = new THREE.Vector3(minX, minY, minZ);
            const axisLen = Math.min(w, h, d) * 0.12;
            const specs = [
                { dir: [1, 0, 0], hex: '#ff4444', label: 'X' },
                { dir: [0, 1, 0], hex: '#44ff44', label: 'Y' },
                { dir: [0, 0, 1], hex: '#4488ff', label: 'Z' },
            ];
            for (const s of specs) {
                const dir = new THREE.Vector3(s.dir[0], s.dir[1], s.dir[2]);
                const end = origin.clone().add(dir.clone().multiplyScalar(axisLen));
                // Line segment
                const lineGeo = new THREE.BufferGeometry().setFromPoints([origin, end]);
                const lineMat = new THREE.LineBasicMaterial({ color: s.hex, transparent: true, opacity: 0.85 });
                group.add(new THREE.Line(lineGeo, lineMat));
                // Sphere at tip
                const sphere = new THREE.Mesh(
                    new THREE.SphereGeometry(axisLen * 0.06, 6, 6),
                    new THREE.MeshBasicMaterial({ color: s.hex })
                );
                sphere.position.copy(end);
                group.add(sphere);
                // Text label
                const label = makeAxisLabel(s.label, s.hex);
                label.position.copy(end).add(dir.clone().multiplyScalar(axisLen * 0.2));
                group.add(label);
            }
            group.visible = state.showBoundary;
            boundaryCube = group;
            Graph.scene().add(group);
        }

        // ─── Ambient particle field ─────────────────────────
        // Tiny ink flecks scattered around the settled node positions — the
        // "splatter" texture of the reference art. Three Points layers give
        // three fleck sizes: a fine dust layer, a mid layer, and a sparse
        // large layer whose soft texture reads as out-of-focus bokeh. All of
        // them inherit the scene fog, so distant flecks melt into the paper.
        let _dotTex;
        function dotTexture() {
            if (_dotTex) return _dotTex;
            const c = document.createElement('canvas');
            c.width = c.height = 64;
            const ctx = c.getContext('2d');
            const g = ctx.createRadialGradient(32, 32, 0, 32, 32, 32);
            g.addColorStop(0, 'rgba(255,255,255,1)');
            g.addColorStop(0.4, 'rgba(255,255,255,0.9)');
            g.addColorStop(0.75, 'rgba(255,255,255,0.25)');
            g.addColorStop(1, 'rgba(255,255,255,0)');
            ctx.fillStyle = g;
            ctx.fillRect(0, 0, 64, 64);
            _dotTex = new THREE.CanvasTexture(c);
            _dotTex.minFilter = THREE.LinearFilter;
            _dotTex.generateMipmaps = false;
            return _dotTex;
        }

        function updateParticleField() {
            if (!Graph || state.perfMode) return;
            const nodes = state.graph.nodes.filter(n => Number.isFinite(n.x));
            if (!nodes.length) return;
            // Rebuilt on every settle; dispose the old field's GPU resources.
            if (particleField) {
                Graph.scene().remove(particleField);
                particleField.traverse(o => {
                    if (o.geometry) o.geometry.dispose();
                    if (o.material) o.material.dispose();
                });
                particleField = null;
            }
            const group = new THREE.Group();
            // layers: [fleck size, share of total, opacity]
            const layers = [[1.6, 0.68, 0.75], [3.4, 0.24, 0.45], [7.5, 0.08, 0.2]];
            const total = Math.min(5000, Math.max(900, nodes.length * 10));
            const tmp = new THREE.Color();
            for (const [size, share, opacity] of layers) {
                const count = Math.max(1, Math.round(total * share));
                const pos = new Float32Array(count * 3);
                const col = new Float32Array(count * 3);
                for (let i = 0; i < count; i++) {
                    // Cluster each fleck around a random node so the splatter
                    // follows the graph's shape instead of filling the box.
                    const n = nodes[(Math.random() * nodes.length) | 0];
                    const r = (n.__nodeRadius || 5) * (2 + Math.pow(Math.random(), 2) * 16);
                    const th = Math.random() * Math.PI * 2;
                    const ph = Math.acos(2 * Math.random() - 1);
                    pos[i * 3] = n.x + r * Math.sin(ph) * Math.cos(th);
                    pos[i * 3 + 1] = n.y + r * Math.sin(ph) * Math.sin(th);
                    pos[i * 3 + 2] = (n.z || 0) + r * Math.cos(ph);
                    // Echo the anchor node's ink family, with a little jitter
                    // so the dust doesn't read as a flat colour.
                    tmp.set(config.getColor(n.group));
                    tmp.offsetHSL(0, (Math.random() - 0.5) * 0.1, (Math.random() - 0.35) * 0.25);
                    col[i * 3] = tmp.r; col[i * 3 + 1] = tmp.g; col[i * 3 + 2] = tmp.b;
                }
                const geo = new THREE.BufferGeometry();
                geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
                geo.setAttribute('color', new THREE.BufferAttribute(col, 3));
                group.add(new THREE.Points(geo, new THREE.PointsMaterial({
                    map: dotTexture(),
                    size,
                    vertexColors: true,
                    transparent: true,
                    opacity,
                    depthWrite: false,
                    sizeAttenuation: true,
                })));
            }
            particleField = group;
            Graph.scene().add(group);
        }

        // Continuous rAF loop driving the selected-node ring + halo pulse.
        function startSelectionAnimation() {
            const tick = () => {
                requestAnimationFrame(tick);
                const n = state.selectedNode;
                if (!selectionRing) return;
                if (!n || !Number.isFinite(n.x)) { selectionRing.visible = false; return; }
                const t = performance.now() * 0.001;
                const wave = Math.sin(t * 3.2);
                const pulse = 1 + 0.16 * wave;
                // Ring sized to a small multiple of the node radius so it stays
                // legible without swamping the neighbourhood at close range.
                const base = Math.max((n.__nodeRadius || 4) * 5, 26);
                selectionRing.position.set(n.x, n.y, n.z || 0);
                selectionRing.scale.setScalar(base * pulse);
                selectionRing.material.rotation += 0.03;
                selectionRing.material.opacity = 0.7 + 0.15 * wave;
                selectionRing.visible = true;
                // Breathe the selected node's own halo and core too (gently, so
                // the tinted halo doesn't swamp its neighbours at close range).
                if (n.__nodeHalo) {
                    n.__nodeHalo.scale.setScalar((n.__haloBase || base) * (1 + 0.2 * wave));
                }
                if (n.__nodeCore) {
                    n.__nodeCore.scale.setScalar(1.3 + 0.18 * wave);
                }
            };
            requestAnimationFrame(tick);
        }

        // Re-evaluate the style accessors after a selection / highlight / filter
        // change, without rebuilding the whole scene.
        function bumpGraphStyles() {
            if (!Graph) return;
            const focusOn = !!state.focusNode;
            // Custom node objects own their material, so recolour them directly.
            state.graph.nodes.forEach(n => {
                if (n.__nodeMat) n.__nodeMat.color.set(nodeColorFor(n));
                const sel = state.selectedNode && n.id === state.selectedNode.id;
                // On a tour, brightness is a four-ring gradient (this stop →
                // the rest of the route → its neighbours → everything else).
                // Otherwise focus mode's binary dim applies.
                const tier = tourTier(n.id);
                const dim = tier ? tier === 'far' : (focusOn && !state.focusSet.has(n.id));
                if (n.__nodeMat) {
                    n.__nodeMat.opacity = tier ? TOUR_TIER_OPACITY[tier] : (dim ? 0.06 : 0.95);
                }
                // Dimmed labels hide immediately; otherwise distance owns it (see
                // updateAdaptiveLabels), so we only force-hide here.
                if (dim && n.__nodeLabel) n.__nodeLabel.visible = false;
                // Tour stops get their names inked in warm orange so the route
                // is readable at a glance. Re-tinting rebuilds the sprite
                // texture, so only do it when the tint actually changes.
                if (n.__nodeLabel) {
                    const wantTint = tier === 'current' || tier === 'stop';
                    if (wantTint !== !!n.__labelTinted) {
                        n.__nodeLabel.color = wantTint ? CANVAS.labelTour : CANVAS.label;
                        n.__labelTinted = wantTint;
                    }
                }
                if (n.__nodeShell) {
                    const base = n.__shellBase || 0.14;
                    n.__nodeShell.material.opacity = dim ? 0
                        : (tier === 'near' ? base * 0.4 : base);
                }
                if (n.__nodeHalo) {
                    const hot = sel || state.highlightNodes.has(n.id);
                    n.__nodeHalo.material.opacity = dim ? 0
                        : (hot ? 0.75 : (tier === 'stop' ? 0.5 : (tier === 'near' ? 0.1 : 0.25)));
                    // Non-selected halos return to base size; the selected one is
                    // animated by the selection rAF loop.
                    if (!sel && n.__haloBase) n.__nodeHalo.scale.setScalar(n.__haloBase);
                }
                // Non-selected cores return to normal scale.
                if (!sel && n.__nodeCore) n.__nodeCore.scale.setScalar(1);
            });
            Graph.nodeVisibility(nodeVisibleFor)
                .linkColor(linkColorFor)
                .linkVisibility(linkVisibleFor)
                .linkDirectionalParticles(linkParticlesFor);
        }

        function truncateName(name) {
            const parts = String(name).split('/');
            const last = parts.pop();
            const short = last.split(':').pop();
            return short.length > 32 ? short.slice(0, 31) + '…' : short;
        }

