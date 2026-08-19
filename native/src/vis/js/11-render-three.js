        // ─── Renderer backend: 3D force graph (three.js + 3d-force-graph) ──
        //
        // The original renderer, now behind the seam in 10-render-core.js. It
        // owns everything three-dimensional: the orbit camera, the six face
        // projections, the dashed boundary cube, the orientation gizmo, fog as
        // depth-of-field, and the sprite "sticker" nodes.
        //
        // Nothing here decides what a node should look like — that lives in
        // 10-render-core.js and is shared with the 2D backend.

        // Bound by mount(), from the vendored bundle. Loaded lazily so a page
        // running the 2D renderer never pays for 1.4 MB of three.js.
        let ForceGraph3D, THREE, SpriteText;

        // The 3d-force-graph instance and its scene-decor handles.
        let Graph, selectionRing, boundaryCube, particleField;
        let _glowTex, _ringTex, _boundaryRingTex, _dotTex;

        // Generation counter for the rAF loops this file starts. Each mount
        // captures the current value; dispose() bumps it, so loops from a dead
        // mount exit on their next frame — otherwise they run forever and,
        // once the renderer mounts again, two loops fight over the same shared
        // selectionRing / gizmo state.
        let threeGen = 0;

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

        // A dashed halo ring marking a system boundary — a node the outside
        // world can reach, or that reaches out.
        //
        // A *ring* rather than a colour, because colour is already the node's
        // type channel and boundary-ness is orthogonal to type: an endpoint is
        // still a Function, and recolouring it would trade one fact for
        // another. Dashes rather than the solid ring with tick marks, so it
        // never reads as the selection marker.
        function boundaryRingTexture() {
            if (_boundaryRingTex) return _boundaryRingTex;
            const c = document.createElement('canvas');
            c.width = c.height = 256;
            const ctx = c.getContext('2d');
            ctx.strokeStyle = 'rgba(255,255,255,1)';
            ctx.lineWidth = 11;
            ctx.setLineDash([16, 11]);
            ctx.lineCap = 'round';
            ctx.beginPath();
            ctx.arc(128, 128, 110, 0, Math.PI * 2);
            ctx.stroke();
            _boundaryRingTex = new THREE.CanvasTexture(c);
            _boundaryRingTex.minFilter = THREE.LinearFilter;
            _boundaryRingTex.generateMipmaps = false;
            return _boundaryRingTex;
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

        // One glyph texture per node type, drawn on a transparent 256px canvas
        // and tinted per node by the sprite's colour. Cached because the graph
        // can hold tens of thousands of nodes of a single type.
        //
        // The disc body is painted *bright* (near-white) so that after the
        // sprite's colour multiply it reads as a vivid saturated sticker that
        // pops off the dark background; the glyph is drawn dark so it stays a
        // crisp dark cutout against that bright disc. (White glyph on a white
        // disc would vanish; a grey disc would sink into the dark scene.)
        const _nodeIconTex = new Map();
        function nodeIconTexture(group) {
            if (_nodeIconTex.has(group)) return _nodeIconTex.get(group);
            const c = document.createElement('canvas');
            c.width = c.height = 256;
            const ctx = c.getContext('2d');
            ctx.translate(128, 128);
            ctx.scale(8, 8);
            // Crisp sticker disc — a solid body with a thin brighter rim.
            ctx.beginPath();
            ctx.arc(0, 0, 11.5, 0, Math.PI * 2);
            ctx.fillStyle = 'rgba(240,240,244,0.97)';
            ctx.fill();
            ctx.lineWidth = 0.7;
            ctx.strokeStyle = 'rgba(255,255,255,0.9)';
            ctx.stroke();
            // The glyph, drawn dark so it reads as a punch-through on the
            // bright disc after tinting.
            const body = NODE_ICONS[group] || '<circle cx="12" cy="12" r="6.5"/>';
            ctx.strokeStyle = 'rgba(24,24,30,0.95)';
            ctx.lineWidth = 2.6;
            ctx.lineCap = 'round';
            ctx.lineJoin = 'round';
            // The Interface glyph is a dashed diamond; Path2D doesn't carry
            // the dasharray attribute, so re-apply it here.
            if (body.includes('stroke-dasharray="3.2 2.6"')) ctx.setLineDash([3.2, 2.6]);
            ctx.stroke(new Path2D(body));
            const tex = new THREE.CanvasTexture(c);
            tex.colorSpace = THREE.SRGBColorSpace;
            tex.minFilter = THREE.LinearFilter;
            tex.generateMipmaps = false;
            _nodeIconTex.set(group, tex);
            return tex;
        }

        // Custom node object: a camera-facing "sticker" disc — the node type's
        // glyph (ƒ, braces, folder…) tinted with the node's colour — so every
        // node is recognisable at a glance rather than reading as a bare
        // sphere. A soft tinted halo sits behind it (a normal-blend colour
        // wash — additive glow would vanish against the white paper), a
        // translucent "membrane" shell still wraps the larger cell-like nodes,
        // and an optional text label floats above.
        function makeNodeObject(n) {
            const radius = nodeRadiusFor(n);
            const seg = 16;   // sphere tesselation for the larger nodes' shell
            const group = new THREE.Group();

            // Soft tinted radial-gradient halo — reads as the out-of-focus
            // ink bleed around every node in the reference art. Rendered below
            // the sticker (renderOrder) so the glow never washes over the glyph.
            const halo = new THREE.Sprite(new THREE.SpriteMaterial({
                map: glowTexture(),
                color: config.getColor(n.group),
                transparent: true,
                opacity: 0.3,
                depthWrite: false,
            }));
            halo.renderOrder = 0;
            // A modest glow now — the sticker carries the visual weight, so the
            // halo should read as a soft rim, not a big fuzzy disc around it.
            n.__haloBase = radius * 2.6;
            halo.scale.setScalar(n.__haloBase);
            n.__nodeHalo = halo;
            group.add(halo);

            const mat = new THREE.SpriteMaterial({
                map: nodeIconTexture(n.group),
                color: nodeColorFor(n),
                transparent: true,
                opacity: 0.95,
                depthWrite: false,
            });
            const core = new THREE.Sprite(mat);
            core.renderOrder = 2;
            // Sprite scale is full width, so double the radius for a disc that
            // sits where the sphere used to. Kept as a base so the selection
            // pulse can breathe around it without losing the size.
            n.__coreScale = radius * 2;
            core.scale.setScalar(n.__coreScale);
            n.__nodeMat = mat;
            n.__nodeCore = core;
            n.__nodeRadius = radius;
            group.add(core);

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
                shell.renderOrder = 1;
                // Kept on the node so dimming (focus / tour) can fade the
                // shell too — otherwise big nodes stay visible through it.
                n.__nodeShell = shell;
                n.__shellBase = 0.14;
                group.add(shell);
            }

            // System boundary: a dashed ring outside the sticker, keeping the
            // node's own type colour and glyph intact.
            if (n.isBoundary) {
                const ring = new THREE.Sprite(new THREE.SpriteMaterial({
                    map: boundaryRingTexture(),
                    color: boundaryRingColor(n),
                    transparent: true,
                    opacity: 0.85,
                    depthWrite: false,
                }));
                // Above the halo, below the sticker: the ring frames the
                // node rather than sitting on top of its glyph.
                ring.renderOrder = 1;
                n.__boundaryRingBase = radius * 3.1;
                ring.scale.setScalar(n.__boundaryRingBase);
                n.__boundaryRing = ring;
                group.add(ring);
            }

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

        function threeMount(el, view) {
            window.addEventListener('mousemove', threeTrackMouse);

            Graph = ForceGraph3D({ controlType: 'orbit' })(el)
                .backgroundColor(CANVAS.bg)
                // The *view*, not the graph: in solo mode this starts empty and
                // only ever holds a neighbourhood (see 16-solo-view.js). Below
                // the threshold `state.view` is `state.graph` itself.
                .graphData({ nodes: view.nodes, links: view.edges })
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
                .linkDirectionalArrowLength(3)
                .linkDirectionalArrowRelPos(1)
                .linkDirectionalParticles(linkParticlesFor)
                .linkDirectionalParticleWidth(1.6)
                .linkDirectionalParticleSpeed(0.012)
                .linkDirectionalParticleColor(linkParticleColorFor)
                .enableNodeDrag(true)
                .onNodeHover(handleNodeHover)
                .onNodeClick((n, evt) => handleNodeClick(evt, n))
                .onBackgroundClick(() => clearSelection())
                .width(width)
                .height(height);

            const charge = Graph.d3Force('charge');
            if (charge) charge.strength(-70);
            // shorter Link distance, means closer connected nodes
            const linkForce = Graph.d3Force('link');
            if (linkForce) linkForce.distance(50);
            Graph.d3Force('zBound', makeZBoundForce());

            // Converge fast so the user isn't waiting for the layout to settle:
            // steeper alpha decay + extra velocity friction stop the slow outward
            // drift much sooner, and the cooldown cap guarantees the engine quits
            // within a bounded number of ticks regardless of graph size.
            Graph.d3AlphaDecay(0.07);
            Graph.d3VelocityDecay(0.6);
            Graph.cooldownTicks(100);

            // Subtle gradient backdrop (in-scene, so it sits behind the graph and
            // reads as blurred ambient washes instead of a flat white void).
            Graph.scene().background = backgroundTexture();

            // Fog doubles as depth-of-field: far nodes and strands melt into
            // the ground tone, so depth reads without needing bloom.
            Graph.scene().fog = new THREE.FogExp2(CANVAS.fog, 0.001);

            // Animated selection marker: a spinning, pulsing ring that sits on the
            // currently selected node (added to the scene, repositioned each frame).
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
            threeGen += 1;
            startSelectionAnimation();

            // Frame the bulk of the graph once the layout settles. Use frameGraph
            // (percentile-based) rather than zoomToFit so a few far-flung outlier
            // nodes don't force the camera miles away.
            const autoFrame = () => {
                // Recompute fog/label cues on every settle (cheap). The boundary
                // box must be rebuilt on every settle too — sizing it once from an
                // early snapshot leaves nodes outside it as the layout expands.
                applyDepthCues();
                updateBoundaryCube();
                updateParticleField();
                // Only fly the camera the first time — or the first time after
                // solo mode swapped the view out from under it.
                if (state._didFit) return;
                state._didFit = true;
                threeSetView('3d', 900);
            };
            // Mark the layout as truly settled only on a real engine stop (the
            // timeout fallbacks below may fire while nodes are still moving).
            Graph.onEngineStop(() => { state._boxSettled = true; autoFrame(); });
            // Fallbacks: settle may take a moment, so frame early too.
            setTimeout(autoFrame, 2500);
            setTimeout(autoFrame, 5000);
            // If the engine somehow never ticks (e.g. an empty solo view), don't
            // leave the loading overlay up forever — release it regardless.
            setTimeout(graphReveal, 4000);

            // While the layout is still expanding, keep the boundary box enclosing
            // the cloud — nodes drift outward before the sim settles, so a box from
            // an early snapshot would leave them poking out. Throttled, and only
            // until the first real settle.
            Graph.onEngineTick(() => {
                // First tick → the layout is spinning up and a frame is about
                // to be painted. Release the loading overlay on the frame
                // *after* the first paint (double rAF) so it never hides into
                // a blank canvas mid-shader-compile.
                if (!state._graphRevealed) {
                    requestAnimationFrame(() => requestAnimationFrame(graphReveal));
                }
                if (state._boxSettled || !state.showBoundary) return;
                const now = performance.now();
                if (now - (state._lastBoxFit || 0) < 150) return;
                state._lastBoxFit = now;
                updateBoundaryCube();
            });

            // Drives the orientation gizmo + distance-adaptive labels.
            startOverlayLoop();
        }

        // page coords position the tooltip; client coords feed the
        // pointerOverCanvas hit-test (elementFromPoint wants viewport
        // coordinates — identical here, but kept separate for safety).
        function threeTrackMouse(e) {
            state._mouse = { x: e.pageX, y: e.pageY, cx: e.clientX, cy: e.clientY };
            // A hover struck on-canvas would otherwise stick (highlight +
            // tooltip) when the pointer crosses into a panel, because the
            // renderer never reports a leave for occluded pixels.
            if (state._hoverNode && !pointerOverCanvas()) handleNodeHover(null);
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

        // Snap the camera to a predefined view. id: '1'–'6' (face projections) or
        // '3d' (isometric).
        function threeSetView(id, ms = 600) {
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

        // Fit the camera around an arbitrary set of node ids — used when solo
        // mode leaves only a neighbourhood on screen. (`threeFrameRoute` does the
        // same for a tour route, with its own route-specific guards.)
        //
        // `opts.flat` swaps the three-quarter offset for a straight-on one and
        // fits the actual bounding box rather than a bounding sphere. A
        // prescribed planar arrangement — the Graph Walk cascade — is a
        // *diagram*, and a diagram read from an angle is a diagram with
        // foreshortening: the hop columns stop lining up and the whole point
        // of laying them out in a row is lost.
        function threeFrameNodes(ids, ms = 700, opts) {
            if (!Graph) return;
            const pts = [];
            ids.forEach(id => {
                const n = state.nodeById && state.nodeById.get(id);
                if (n) pts.push({ x: +n.x || 0, y: +n.y || 0, z: +n.z || 0 });
            });
            if (!pts.length) return;
            if (opts && opts.flat) { threeFrameFlat(pts, ms); return; }
            const centre = pts.reduce(
                (a, p) => ({ x: a.x + p.x / pts.length, y: a.y + p.y / pts.length, z: a.z + p.z / pts.length }),
                { x: 0, y: 0, z: 0 }
            );
            let radius = 0;
            pts.forEach(p => {
                const dx = p.x - centre.x, dy = p.y - centre.y, dz = p.z - centre.z;
                radius = Math.max(radius, Math.sqrt(dx * dx + dy * dy + dz * dz));
            });
            // Clamped so a lone node isn't framed from inside it, and a sprawling
            // neighbourhood doesn't push the camera out past the whole graph.
            const d = Math.max(300, Math.min(1500, radius * 2 + 200)) / Math.sqrt(3);
            Graph.cameraPosition(
                { x: centre.x + d, y: centre.y + d * 0.8, z: centre.z + d },
                centre,
                ms
            );
        }

        // Straight-on fit of a bounding box: the camera sits on the +Z axis
        // through the box's centre, far enough back that the box fits both the
        // vertical and the horizontal field of view. A cascade is much wider
        // than it is tall, so fitting a bounding *sphere* here would park the
        // camera a long way past what the height needs.
        function threeFrameFlat(pts, ms) {
            let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity, sumZ = 0;
            for (const p of pts) {
                if (p.x < minX) minX = p.x;
                if (p.x > maxX) maxX = p.x;
                if (p.y < minY) minY = p.y;
                if (p.y > maxY) maxY = p.y;
                sumZ += p.z;
            }
            const centre = { x: (minX + maxX) / 2, y: (minY + maxY) / 2, z: sumZ / pts.length };
            const fov = ((Graph.camera().fov || 50) * Math.PI) / 180;
            const aspect = (width && height) ? width / height : 1.6;
            // Half-extents plus a margin, so nodes at the rim keep their labels
            // and halos inside the frame.
            // Vertical margin runs wider than horizontal: the walk's hop
            // headings sit above the top of the node box, and a fit computed
            // from node positions alone would frame them out.
            //
            // The floor on the half-extents is what keeps a *small* set
            // sensible. A walk sitting on its seed is one node, and fitting a
            // box that size puts the camera inside the sticker — the node
            // fills the screen and the diagram it belongs to is nowhere. The
            // floor frames a lone node at roughly the distance a plain focus
            // would, so stepping back to the seed reads as zooming to it
            // rather than falling into it.
            const halfW = Math.max((maxX - minX) / 2, 150) * 1.14;
            const halfH = Math.max((maxY - minY) / 2, 150) * 1.32;
            const dist = Math.max(
                halfH / Math.tan(fov / 2),
                halfW / (Math.tan(fov / 2) * aspect),
                200
            );
            Graph.cameraPosition({ x: centre.x, y: centre.y, z: centre.z + dist }, centre, ms);
        }

        // ── Prescribed positions ───────────────────────────────
        //
        // Somebody outside the renderer has computed where the nodes go (the
        // Graph Walk cascade) and this makes it stick.
        //
        // The mechanism is d3-force's own pinning: `fx/fy/fz` are read *after*
        // every force has had its say, so a pinned node cannot be pushed
        // anywhere no matter what the simulation is doing. Every node in the
        // view is pinned, not just the moved ones — the alternative is a
        // prescribed arrangement floating in a graph that is still settling
        // around it, which reads as the layout being fought over.
        //
        // The awkward part is that three-forcegraph only copies node
        // coordinates onto the scene objects *while the engine is running*
        // (`tickFrame` guards the whole sync on `engineRunning`). So a morph
        // has to keep the engine alive for its duration, which is what the
        // reheats below are for. With every node pinned, a reheat cannot
        // disturb anything: alpha only matters to nodes that are free to move.
        let _threePosAnim = null;
        let _threeUnpinTimer = null;

        function threeCancelPosAnim() {
            if (_threePosAnim) { cancelAnimationFrame(_threePosAnim); _threePosAnim = null; }
            if (_threeUnpinTimer) { clearTimeout(_threeUnpinTimer); _threeUnpinTimer = null; }
        }

        function threeSetNodePositions(pos, ms, opts) {
            if (!Graph) return;
            threeCancelPosAnim();
            const o = opts || {};
            const nodes = (Graph.graphData() || {}).nodes || [];
            const items = [];
            for (const n of nodes) {
                const p = pos.get(n.id);
                const x0 = +n.x || 0, y0 = +n.y || 0, z0 = +n.z || 0;
                items.push({
                    n, x0, y0, z0,
                    x1: p ? p.x : x0,
                    y1: p ? p.y : y0,
                    z1: p ? (p.z || 0) : z0,
                });
            }
            if (!items.length) return;

            const apply = (k) => {
                const e = k >= 1 ? 1 : 1 - Math.pow(1 - k, 3);
                for (const it of items) {
                    const x = it.x0 + (it.x1 - it.x0) * e;
                    const y = it.y0 + (it.y1 - it.y0) * e;
                    const z = it.z0 + (it.z1 - it.z0) * e;
                    it.n.x = x; it.n.y = y; it.n.z = z;
                    it.n.fx = x; it.n.fy = y; it.n.fz = z;
                }
            };

            // Unpinning is deliberately late. The reheats leave alpha high, and
            // releasing a whole graph into a hot simulation would re-settle it
            // — the walk would end by shuffling the graph the user came back
            // to. A couple of seconds pinned is all it takes for alpha to decay
            // and the engine to stop, and after that a release moves nothing.
            const release = () => {
                if (!o.release) return;
                _threeUnpinTimer = setTimeout(() => {
                    _threeUnpinTimer = null;
                    for (const it of items) {
                        it.n.fx = undefined; it.n.fy = undefined; it.n.fz = undefined;
                    }
                }, 2200);
            };

            const dur = Math.max(0, ms || 0);
            if (!dur) {
                apply(1);
                // One tick's worth of engine, purely so the scene objects pick
                // the new coordinates up — nothing can move while pinned.
                Graph.d3ReheatSimulation();
                release();
                return;
            }

            const t0 = performance.now();
            apply(0);
            Graph.d3ReheatSimulation();
            let lastHeat = t0;
            const step = () => {
                const now = performance.now();
                const k = Math.min(1, (now - t0) / dur);
                apply(k);
                // cooldownTicks(100) is ~1.6s of frames; a longer morph would
                // otherwise freeze half way through when the engine quits.
                if (now - lastHeat > 900) { lastHeat = now; Graph.d3ReheatSimulation(); }
                if (k < 1) { _threePosAnim = requestAnimationFrame(step); return; }
                _threePosAnim = null;
                release();
            };
            _threePosAnim = requestAnimationFrame(step);
        }

        // Centre the camera on the node at a consistent, comfortable distance.
        // Isolation is conveyed by the focus dimming, not by the camera —
        // fitting a scattered neighbourhood tends to either over-zoom (tiny
        // clusters bloom out) or under-zoom (sparse, empty view), so we keep the
        // framing simple and predictable.
        function threeFocusNode(n) {
            if (!Graph) return;
            // Wait one frame so any panel toggles (info open/close) commit their
            // layout before the camera flies.
            requestAnimationFrame(() => {
                const x = +n.x || 0, y = +n.y || 0, z = +n.z || 0;
                // Total camera-to-node distance (the (d,d,d) offset has magnitude
                // sqrt(3)*d). Pulled well back so a generous slice of the
                // surrounding neighbourhood stays in frame on focus.
                const d = 480 / Math.sqrt(3);
                Graph.cameraPosition(
                    { x: x + d, y: y + d, z: z + d },
                    { x, y, z },
                    800);
            });
        }

        // Scale the camera's orbit radius by `factor` (mirrors the mouse-wheel
        // dolly: <1 zooms in, >1 zooms out). Keeps the same look-at target so the
        // view direction is preserved; only the distance changes.
        function threeZoomBy(factor) {
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

        // ─── Tour camera ───────────────────────────────────────

        function tourVec(n) { return { x: +n.x || 0, y: +n.y || 0, z: +n.z || 0 }; }
        function vSub(a, b) { return { x: a.x - b.x, y: a.y - b.y, z: a.z - b.z }; }
        function vAdd(a, b) { return { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }; }
        function vScale(a, s) { return { x: a.x * s, y: a.y * s, z: a.z * s }; }
        function vLen(a) { return Math.sqrt(a.x * a.x + a.y * a.y + a.z * a.z); }
        function vNorm(a) { const l = vLen(a) || 1; return vScale(a, 1 / l); }
        function vCross(a, b) {
            return {
                x: a.y * b.z - a.z * b.y,
                y: a.z * b.x - a.x * b.z,
                z: a.x * b.y - a.y * b.x,
            };
        }
        function vLerp(a, b, t) { return vAdd(a, vScale(vSub(b, a), t)); }

        // Fly to a stop with framing that tells the story: the camera sits
        // broadside to the direction of travel (so the hop we just took is
        // visible, not hidden behind us), pulled back far enough to hold the
        // stop's neighbourhood, and aimed a touch toward the next stop so
        // where we're heading is already on screen.
        function threeFlyToStop(stop, opts) {
            opts = opts || {};
            const node = state.nodeById ? state.nodeById.get(stop.node_id) : null;
            if (!node || !Graph) return;
            const prevNode = opts.prev && state.nodeById ? state.nodeById.get(opts.prev.node_id) : null;
            const nextNode = opts.next && state.nodeById ? state.nodeById.get(opts.next.node_id) : null;

            const p = tourVec(node);
            // Everything we'd like in frame, to pick a distance from.
            const context = [];
            if (prevNode) context.push(tourVec(prevNode));
            if (nextNode) context.push(tourVec(nextNode));
            tourState.nearIds.forEach(id => {
                const nn = state.nodeById && state.nodeById.get(id);
                if (nn) context.push(tourVec(nn));
            });
            let spread = 0;
            context.forEach(c => { spread = Math.max(spread, vLen(vSub(c, p))); });
            const dist = Math.max(230, Math.min(900, 200 + spread * 1.35));

            const travel = prevNode ? vSub(p, tourVec(prevNode))
                : (nextNode ? vSub(tourVec(nextNode), p) : null);
            let dir;
            if (travel && vLen(travel) > 1e-3) {
                const t = vNorm(travel);
                let up = { x: 0, y: 1, z: 0 };
                let side = vCross(t, up);
                // Travelling straight up/down: pick another reference axis so
                // the cross product doesn't collapse.
                if (vLen(side) < 0.2) { up = { x: 0, y: 0, z: 1 }; side = vCross(t, up); }
                side = vNorm(side);
                dir = vNorm(vAdd(vScale(side, 1), vAdd(vScale(up, 0.5), vScale(t, -0.3))));
            } else {
                const d = 1 / Math.sqrt(3);
                dir = { x: d, y: d, z: d };
            }
            const look = nextNode ? vLerp(p, tourVec(nextNode), 0.16) : p;
            const cam = vAdd(p, vScale(dir, dist));
            // One frame of slack so any panel toggle commits its layout first.
            requestAnimationFrame(() => {
                Graph.cameraPosition(cam, look, opts.ms || 1100);
            });
        }

        // Frame every stop on the route, so the ending shot is the shape of
        // the answer rather than one more close-up.
        function threeFrameRoute(ms) {
            if (!Graph || !tourState.data) return;
            const pts = [];
            tourState.routeIds.forEach(id => {
                const n = state.nodeById && state.nodeById.get(id);
                if (n) pts.push(tourVec(n));
            });
            if (pts.length < 2) return;
            const c = pts.reduce((a, p) => vAdd(a, p), { x: 0, y: 0, z: 0 });
            const centre = vScale(c, 1 / pts.length);
            let radius = 0;
            pts.forEach(p => { radius = Math.max(radius, vLen(vSub(p, centre))); });
            const d = Math.max(300, Math.min(1500, radius * 2 + 200)) / Math.sqrt(3);
            Graph.cameraPosition(
                { x: centre.x + d, y: centre.y + d * 0.8, z: centre.z + d },
                centre,
                ms || 1400);
        }

        // ─── Boundary cube ─────────────────────────────────────

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

        // Drops the cube and frees its GPU resources. It is ~30 meshes plus
        // six label textures, and it is hidden by default, so it is built on
        // demand rather than kept around invisible.
        function disposeBoundaryCube() {
            if (!boundaryCube || !Graph) return;
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

        function updateBoundaryCube() {
            if (!Graph) return;
            // Nothing keeps the box in sync while it is off, so there is no
            // point building it — setBoundaryVisible() rebuilds from the
            // current layout at the moment it is switched on.
            if (!state.showBoundary) { disposeBoundaryCube(); return; }
            const nodes = state.view.nodes.filter(n => Number.isFinite(n.x));
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
            disposeBoundaryCube();

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
            if (!Graph) return;
            const nodes = state.view.nodes.filter(n => Number.isFinite(n.x));
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
            const gen = threeGen;
            const tick = () => {
                if (gen !== threeGen) return;
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
                    n.__nodeCore.scale.setScalar((n.__coreScale || 1) * (1.3 + 0.18 * wave));
                }
            };
            requestAnimationFrame(tick);
        }

        // ─── Orientation gizmo + distance-adaptive labels ──────

        const GIZMO_AXES = [
            { v: [1, 0, 0], color: '#ff5d5d', label: 'X' },
            { v: [0, 1, 0], color: '#5dff8f', label: 'Y' },
            { v: [0, 0, 1], color: '#5d9dff', label: 'Z' },
        ];

        // Whether the camera has turned enough since the last gizmo repaint to
        // be worth redrawing. The threshold is well below one screen pixel of
        // movement on a 26px triad, so nothing visible is ever skipped.
        //
        // Declared outside the loop so the state survives, and reset whenever
        // the renderer is remounted (`gz` is re-created with the closure).
        function gizmoMovedFactory() {
            let last = null;
            return function gizmoMoved(cam) {
                const q = cam.quaternion;
                if (last
                    && Math.abs(q.x - last.x) < 1e-4
                    && Math.abs(q.y - last.y) < 1e-4
                    && Math.abs(q.z - last.z) < 1e-4
                    && Math.abs(q.w - last.w) < 1e-4) {
                    return false;
                }
                last = { x: q.x, y: q.y, z: q.z, w: q.w };
                return true;
            };
        }

        function startOverlayLoop() {
            const gen = threeGen;
            const svg = document.getElementById('gizmo-svg');
            let frame = 0;
            const v = new THREE.Vector3();
            // Last orientation the gizmo was drawn for. Rebuilding its markup
            // is a string concat plus an `innerHTML` parse — cheap once, but
            // it ran 30 times a second forever, including while the camera sat
            // perfectly still, which is most of the time anyone spends reading
            // the graph. Comparing the quaternion first makes an idle canvas
            // cost nothing.
            const gizmoMoved = gizmoMovedFactory();
            const tick = () => {
                if (gen !== threeGen) return;
                requestAnimationFrame(tick);
                if (!Graph) return;
                const cam = Graph.camera();
                if (!cam) return;
                frame++;
                // Gizmo: rotate the world axes into the camera's view frame so the
                // little triad mirrors how the graph is oriented on screen.
                if (svg && frame % 2 === 0 && gizmoMoved(cam)) {
                    cam.updateMatrixWorld();
                    const q = cam.quaternion.clone().invert();
                    const R = 26;
                    const drawn = GIZMO_AXES.map(a => {
                        v.set(a.v[0], a.v[1], a.v[2]).applyQuaternion(q);
                        return { color: a.color, label: a.label, x: v.x, y: v.y, z: v.z };
                    }).sort((a, b) => a.z - b.z); // painter's order: far axes first
                    let s = '';
                    for (const a of drawn) {
                        const x = (a.x * R).toFixed(1);
                        const y = (-a.y * R).toFixed(1);
                        const op = (0.4 + 0.6 * ((a.z + 1) / 2)).toFixed(2);
                        s += `<line x1="0" y1="0" x2="${x}" y2="${y}" stroke="${a.color}" stroke-width="2.4" stroke-linecap="round" opacity="${op}"/>`;
                        s += `<circle cx="${x}" cy="${y}" r="3.2" fill="${a.color}" opacity="${op}"/>`;
                        s += `<text x="${(a.x * R * 1.34).toFixed(1)}" y="${(-a.y * R * 1.34 + 3.4).toFixed(1)}" fill="${a.color}" font-size="9.5" font-family="JetBrains Mono, monospace" text-anchor="middle" opacity="${op}">${a.label}</text>`;
                    }
                    svg.innerHTML = s;
                }
                // Distance-adaptive labels: hide labels for nodes far from the
                // camera so a zoomed-out view stays clean and they reappear as you
                // move in. Throttled.
                if (frame % 8 === 0) updateAdaptiveLabels(cam);
                // Legend counts track the on-screen set during a walk or tour.
                if (frame % 12 === 0) refreshModeLegend();
            };
            requestAnimationFrame(tick);
        }

        function updateAdaptiveLabels(cam) {
            // The viewbar's Names toggle wins over every distance rule below.
            if (!state.showLabels) {
                state.view.nodes.forEach(n => { if (n.__nodeLabel) n.__nodeLabel.visible = false; });
                return;
            }
            const px = cam.position.x, py = cam.position.y, pz = cam.position.z;
            const D = state._labelDist || 340;
            const D2 = D * D;
            const focusOn = !!state.focusNode;
            const tourOn = tourState.active && tourState.routeIds.size > 0;
            state.view.nodes.forEach(n => {
                const s = n.__nodeLabel;
                if (!s) return;
                // On a tour only the stops are named — the surrounding
                // neighbourhood stays present but anonymous.
                if (tourOn) {
                    s.visible = tourState.routeIds.has(n.id);
                    return;
                }
                if (focusOn) {
                    // While focused, always label the neighbourhood; hide the rest.
                    s.visible = state.focusSet.has(n.id);
                    return;
                }
                const dx = (n.x || 0) - px, dy = (n.y || 0) - py, dz = (n.z || 0) - pz;
                s.visible = (dx * dx + dy * dy + dz * dz) < D2;
            });
        }

        // ─── Graph Walk: hop lane markers ──────────────────────
        //
        // The cascade puts one column of nodes per hop, marching the way the
        // edges point. That is only self-explanatory once you already know it,
        // so each revealed column gets a rule down its axis and a heading —
        // the same guides the 2D overlay paints (fxDrawWalkLanes), built here
        // as scene objects because there is no overlay running in 3D.
        //
        // Rebuilt only when the revealed set changes, not on every restyle: a
        // hover must not cost a sprite-texture upload.
        let walkLaneGroup = null;
        let _walkLaneSig = '';

        function disposeWalkLanes() {
            if (!walkLaneGroup || !Graph) { walkLaneGroup = null; _walkLaneSig = ''; return; }
            Graph.scene().remove(walkLaneGroup);
            walkLaneGroup.traverse(o => {
                if (o.geometry) o.geometry.dispose();
                if (o.material) {
                    if (o.material.map) o.material.map.dispose();
                    o.material.dispose();
                }
            });
            walkLaneGroup = null;
            _walkLaneSig = '';
        }

        function threeUpdateWalkLanes() {
            if (!Graph) return;
            const lanes = (state.walkActive && state.walkLanes) ? state.walkLanes : [];
            const shown = lanes.filter(walkLaneRevealed);
            const sig = shown.map(l => l.hop + ':' + l.sign + ':' + l.count).join(',');
            if (sig === _walkLaneSig) return;
            disposeWalkLanes();
            _walkLaneSig = sig;
            if (!shown.length) return;

            // One vertical span for every column, so the rules line up rather
            // than each ending wherever its own column happens to.
            let top = -Infinity, bottom = Infinity;
            for (const l of shown) {
                if (l.top > top) top = l.top;
                if (l.bottom < bottom) bottom = l.bottom;
            }
            const pad = Math.max((top - bottom) * 0.06, 50);
            top += pad; bottom -= pad;

            const group = new THREE.Group();
            for (const lane of shown) {
                const colour = new THREE.Color(lane.color);
                const mat = new THREE.LineBasicMaterial({
                    color: colour, transparent: true, opacity: 0.16, depthWrite: false, fog: false,
                });
                // A hop that fanned out into a block of lanes is bracketed on
                // both edges; one that fits in a single line gets its axis.
                const pad = WALK_COL_GAP * 0.32 * (lane.scale || 1);
                const rules = lane.x1 - lane.x0 < 1
                    ? [lane.x]
                    : [lane.x0 - pad * lane.sign, lane.x1 + pad * lane.sign];
                for (const rx of rules) {
                    const geo = new THREE.BufferGeometry().setFromPoints([
                        new THREE.Vector3(rx, bottom, 0),
                        new THREE.Vector3(rx, top, 0),
                    ]);
                    group.add(new THREE.Line(geo, mat));
                }

                const text = new SpriteText(lane.label + (lane.count > 1 ? '  ×' + lane.count : ''));
                text.color = lane.color;
                text.fontFace = 'JetBrains Mono, monospace';
                text.fontWeight = '600';
                text.textHeight = Math.max(14, (top - bottom) * 0.026);
                text.material.depthWrite = false;
                text.material.fog = false;
                if (text.material.map) {
                    text.material.map.generateMipmaps = false;
                    text.material.map.minFilter = THREE.LinearFilter;
                    text.material.map.needsUpdate = true;
                }
                text.position.set(lane.x, top + text.textHeight * 0.9, 0);
                group.add(text);
            }
            group.renderOrder = -1;
            walkLaneGroup = group;
            Graph.scene().add(group);
        }

        // ─── Graph Walk ignition pulse ─────────────────────────
        //
        // Four additive-blended layers bloom against the dark canvas:
        //   • a fresnel-rim glow shell — bright silhouette, hot core (the
        //     "energy bubble"; the part that reads as a real blast),
        //   • a wireframe cage slightly larger — geometric structure / edge,
        //   • a soft plasma core — a luminous centre,
        //   • a radial particle burst — the explosion of debris outward.
        // All self-disposing (geometry + materials freed once the burst ends).
        const _fresnelShell = () => `
            varying vec3 vNormal; varying vec3 vView;
            void main() {
                vec4 mv = modelViewMatrix * vec4(position, 1.0);
                vNormal = normalize(normalMatrix * normal);
                vView = normalize(-mv.xyz);
                gl_Position = projectionMatrix * mv;
            }`;
        const _fresnelFrag = () => `
            varying vec3 vNormal; varying vec3 vView;
            uniform vec3 uColor; uniform vec3 uCore; uniform float uPower; uniform float uOpacity;
            void main() {
                float rim = 1.0 - max(0.0, dot(normalize(vNormal), normalize(vView)));
                rim = pow(rim, uPower);
                vec3 c = mix(uCore, uColor, rim);
                gl_FragColor = vec4(c, rim * uOpacity);
            }`;

        function threeEmitPulse(seedNode, colour, fromR, toR, growMs) {
            if (!Graph || !seedNode || !Number.isFinite(seedNode.x)) return;
            const scene = Graph.scene();
            const fromRr = Math.max(fromR || 0, 6);
            const toRr = Math.max((toR || 0) + 18, fromRr + 24);
            const grow = Math.max(160, growMs || 420);
            const fade = Math.min(640, Math.max(240, grow * 0.9));
            const total = grow + fade;
            const t0 = performance.now();

            const group = new THREE.Group();
            group.position.set(seedNode.x || 0, seedNode.y || 0, seedNode.z || 0);
            scene.add(group);
            const disposables = [];
            const addDispose = (o) => { if (o.geometry) disposables.push(o.geometry); if (o.material) disposables.push(o.material); };

            // Fresnel rim shell — the energy bubble.
            const shellMat = new THREE.ShaderMaterial({
                transparent: true, depthWrite: false, blending: THREE.AdditiveBlending, fog: false,
                uniforms: {
                    uColor: { value: new THREE.Color(colour) },
                    uCore: { value: new THREE.Color('#fff3e8') },
                    uPower: { value: 2.6 },
                    uOpacity: { value: 0 },
                },
                vertexShader: _fresnelShell(),
                fragmentShader: _fresnelFrag(),
            });
            const shell = new THREE.Mesh(new THREE.IcosahedronGeometry(1, 4), shellMat);
            shell.renderOrder = 500;
            group.add(shell); addDispose(shell);

            // Wireframe cage — structure / edge shimmer.
            const cageMat = new THREE.MeshBasicMaterial({
                color: new THREE.Color(colour), wireframe: true, transparent: true,
                opacity: 0, depthWrite: false, blending: THREE.AdditiveBlending, fog: false,
            });
            const cage = new THREE.Mesh(new THREE.IcosahedronGeometry(1, 1), cageMat);
            cage.renderOrder = 501;
            group.add(cage); addDispose(cage);

            // Plasma core — luminous centre (kept faint so it doesn't white out).
            const coreMat = new THREE.MeshBasicMaterial({
                color: new THREE.Color(colour), transparent: true, opacity: 0,
                depthWrite: false, blending: THREE.AdditiveBlending, fog: false,
            });
            const core = new THREE.Mesh(new THREE.IcosahedronGeometry(1, 3), coreMat);
            core.renderOrder = 499;
            group.add(core); addDispose(core);

            // Radial particle burst — debris exploding outward to the frontier.
            const N = 96;
            const positions = new Float32Array(N * 3);
            const dirs = [];
            for (let i = 0; i < N; i++) {
                const u = Math.random(), v = Math.random();
                const theta = 2 * Math.PI * u, phi = Math.acos(2 * v - 1);
                dirs.push({
                    dx: Math.sin(phi) * Math.cos(theta),
                    dy: Math.sin(phi) * Math.sin(theta),
                    dz: Math.cos(phi),
                    spd: 0.55 + Math.random() * 0.7,   // some reach past the shell
                });
            }
            const pGeo = new THREE.BufferGeometry();
            pGeo.setAttribute('position', new THREE.BufferAttribute(positions, 3));
            const pMat = new THREE.PointsMaterial({
                color: new THREE.Color(colour), size: 3.4, transparent: true, opacity: 0,
                depthWrite: false, blending: THREE.AdditiveBlending, sizeAttenuation: true, fog: false,
            });
            const points = new THREE.Points(pGeo, pMat);
            points.renderOrder = 502;
            group.add(points); addDispose({ geometry: pGeo, material: pMat });

            const step = () => {
                const t = performance.now() - t0;
                if (t >= total || !state.walkActive) {
                    scene.remove(group);
                    disposables.forEach(d => d.dispose());
                    return;
                }
                const p = Math.min(1, t / grow);           // grow progress
                const e = 1 - Math.pow(1 - p, 3);           // ease-out: decelerate at the frontier
                const r = fromRr + (toRr - fromRr) * e;
                const rs = Math.max(0.1, r);
                shell.scale.setScalar(rs);
                cage.scale.setScalar(Math.max(0.1, r * 1.07));
                core.scale.setScalar(Math.max(0.1, r * 0.62));
                // Envelope: quick ramp-in over the first 25% of the grow, hold,
                // then fade out across `fade`. The burst flashes to life, sweeps
                // outward, and dissolves — reads as an explosion, not a fade.
                const fadeIn = Math.min(1, p / 0.25);
                const env = t <= grow ? fadeIn : Math.max(0, 1 - (t - grow) / fade);
                shellMat.uniforms.uOpacity.value = 0.95 * env;
                cageMat.opacity = 0.5 * env;
                coreMat.opacity = 0.14 * env;
                pMat.opacity = 0.95 * env;
                // Particles fly outward, each on its own speed curve.
                const arr = pGeo.attributes.position.array;
                for (let i = 0; i < N; i++) {
                    const d = dirs[i];
                    const dist = r * d.spd;
                    arr[i * 3] = d.dx * dist;
                    arr[i * 3 + 1] = d.dy * dist;
                    arr[i * 3 + 2] = d.dz * dist;
                }
                pGeo.attributes.position.needsUpdate = true;
                // Slow counter-rotation for shimmer on the structural layers.
                cage.rotation.y += 0.012; cage.rotation.x += 0.007;
                shell.rotation.y -= 0.005;
                requestAnimationFrame(step);
            };
            requestAnimationFrame(step);
        }

        // ─── Graph Walk: the travelling wavefront ──────────────
        //
        // The cascade's answer to the ignition sphere.
        //
        // A sphere is the right shape for a frontier that really is a shell:
        // hop 2 is everything at radius 2, in every direction, so a wave
        // expanding from the seed touches all of it at once. Laid out as
        // columns that is no longer true — hop 2 is a *line*, and a sphere
        // grown from the seed until it reaches that line has already swallowed
        // hop 1, hop 3's near edge and most of the canvas on the way. It
        // describes a geometry the diagram no longer has.
        //
        // So in a cascade the wavefront is a curtain: a bright vertical front
        // that travels along the flow axis from the column it left to the
        // column that is about to ignite, dragging a lit wake behind it and a
        // spray of debris riding along. Same beat, same colour, same timing —
        // it arrives exactly as the frontier lights up — but now the motion
        // and the layout agree about which way the walk is going.
        const _sweepVert = () => `
            varying vec3 vPos;
            void main() {
                vPos = position;
                gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
            }`;
        // uWake 0 → the front itself; 1 → the lit wake dragged behind it. Both
        // taper at the top and bottom so the curtain has ends rather than
        // being a rectangle laid over the graph, and both taper across their
        // travel axis: a flat bar of additive white is a light source, not a
        // wavefront, and it erases everything it passes over. Only the very
        // centre of the front goes hot; the rest keeps the hop's own colour,
        // so the wave still reads as belonging to the frontier it announces.
        const _sweepFrag = () => `
            varying vec3 vPos;
            uniform vec3 uColor; uniform vec3 uCore;
            uniform float uOpacity; uniform float uWake;
            void main() {
                float ends = 1.0 - smoothstep(0.34, 0.5, abs(vPos.y));
                float bar = 1.0 - smoothstep(0.0, 0.5, abs(vPos.x));
                float wake = pow(vPos.x + 0.5, 2.4);
                float body = mix(bar, wake, uWake);
                float hot = mix(pow(bar, 3.0), 0.0, uWake);
                vec3 c = mix(uColor, uCore, hot);
                gl_FragColor = vec4(c, ends * body * uOpacity);
            }`;

        function threeEmitSweep(spec) {
            if (!Graph || !spec || !Number.isFinite(spec.fromX) || !Number.isFinite(spec.toX)) return;
            const scene = Graph.scene();
            const height = Math.max(spec.top - spec.bottom, 40);
            const cy = (spec.top + spec.bottom) / 2;
            const cz = spec.z || 0;
            const dir = spec.toX >= spec.fromX ? 1 : -1;
            const span = Math.abs(spec.toX - spec.fromX) || 1;
            // Some body in z so the curtain still reads as a sheet when the
            // camera is orbited off the diagram's plane.
            const depth = Math.max(height * 0.12, 60);
            const grow = Math.max(160, spec.growMs || 420);
            const fade = Math.min(560, Math.max(220, grow * 0.75));
            const total = grow + fade;
            const t0 = performance.now();

            const group = new THREE.Group();
            scene.add(group);
            const disposables = [];

            const makeCurtain = (wake) => {
                const mat = new THREE.ShaderMaterial({
                    transparent: true, depthWrite: false, blending: THREE.AdditiveBlending,
                    fog: false, side: THREE.DoubleSide,
                    uniforms: {
                        uColor: { value: new THREE.Color(spec.colour) },
                        uCore: { value: new THREE.Color('#fff3e8') },
                        uOpacity: { value: 0 },
                        uWake: { value: wake },
                    },
                    vertexShader: _sweepVert(),
                    fragmentShader: _sweepFrag(),
                });
                const mesh = new THREE.Mesh(new THREE.BoxGeometry(1, 1, 1), mat);
                mesh.renderOrder = 500;
                group.add(mesh);
                disposables.push(mesh.geometry, mat);
                return { mesh, mat };
            };
            const wake = makeCurtain(1);
            const front = makeCurtain(0);
            // The wake is oriented so its local +x is the direction of travel;
            // the shader's `along` ramp then brightens toward the front.
            wake.mesh.scale.set(1, height, depth);
            wake.mesh.rotation.y = dir > 0 ? 0 : Math.PI;
            // Wide enough for the soft profile to have somewhere to fall off.
            front.mesh.scale.set(Math.max(span * 0.05, 18), height, depth * 1.02);

            // Debris riding the front, scattered across the curtain rather
            // than exploding from a point — it is a wave passing, not a blast.
            const N = 72;
            const positions = new Float32Array(N * 3);
            const bits = [];
            for (let i = 0; i < N; i++) {
                bits.push({
                    y: cy + (Math.random() - 0.5) * height * 0.92,
                    z: cz + (Math.random() - 0.5) * depth,
                    lag: Math.random() * 0.22,               // trails the front
                    jitter: (Math.random() - 0.5) * span * 0.05,
                });
            }
            const pGeo = new THREE.BufferGeometry();
            pGeo.setAttribute('position', new THREE.BufferAttribute(positions, 3));
            const pMat = new THREE.PointsMaterial({
                color: new THREE.Color(spec.colour), size: 3.2, transparent: true, opacity: 0,
                depthWrite: false, blending: THREE.AdditiveBlending, sizeAttenuation: true, fog: false,
            });
            const points = new THREE.Points(pGeo, pMat);
            points.renderOrder = 502;
            group.add(points);
            disposables.push(pGeo, pMat);

            const step = () => {
                const t = performance.now() - t0;
                if (t >= total || !state.walkActive) {
                    scene.remove(group);
                    disposables.forEach(d => d.dispose());
                    return;
                }
                const p = Math.min(1, t / grow);
                const e = 1 - Math.pow(1 - p, 3);   // decelerate into the frontier
                const x = spec.fromX + (spec.toX - spec.fromX) * e;
                const travelled = Math.max(Math.abs(x - spec.fromX), 1);

                wake.mesh.scale.x = travelled;
                wake.mesh.position.set(spec.fromX + dir * travelled / 2, cy, cz);
                front.mesh.position.set(x, cy, cz);

                const fadeIn = Math.min(1, p / 0.22);
                const env = t <= grow ? fadeIn : Math.max(0, 1 - (t - grow) / fade);
                front.mat.uniforms.uOpacity.value = 0.72 * env;
                wake.mat.uniforms.uOpacity.value = 0.26 * env;
                pMat.opacity = 0.85 * env;

                const arr = pGeo.attributes.position.array;
                for (let i = 0; i < N; i++) {
                    const b = bits[i];
                    const bp = Math.max(0, e - b.lag);
                    arr[i * 3] = spec.fromX + (spec.toX - spec.fromX) * bp + b.jitter;
                    arr[i * 3 + 1] = b.y;
                    arr[i * 3 + 2] = b.z;
                }
                pGeo.attributes.position.needsUpdate = true;
                requestAnimationFrame(step);
            };
            requestAnimationFrame(step);
        }

        // ─── Restyle ───────────────────────────────────────────

        // Custom node objects own their material, so recolour them directly.
        // Only what is on screen: in solo mode that is the difference between a
        // few hundred nodes per hover and a hundred thousand.
        function threeRestyle() {
            if (!Graph) return;
            threeUpdateWalkLanes();
            state.view.nodes.forEach(n => {
                if (n.__nodeMat) n.__nodeMat.color.set(nodeColorFor(n));
                const sel = state.selectedNode && n.id === state.selectedNode.id;
                const { dim, opacity: op, tier } = nodeLightingFor(n);
                if (n.__nodeMat) n.__nodeMat.opacity = op;
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
                // The boundary ring fades with everything else. Left at full
                // opacity it would survive a tour or a focus filter and read
                // as "these are the relevant nodes", which is the opposite of
                // what dimming is saying.
                if (n.__boundaryRing) {
                    n.__boundaryRing.material.opacity = dim ? 0 : (tier === 'near' ? 0.35 : 0.85);
                }
                // Non-selected cores return to normal scale.
                if (!sel && n.__nodeCore) n.__nodeCore.scale.setScalar(n.__coreScale || 1);
            });
            Graph.nodeVisibility(nodeVisibleFor)
                .linkColor(linkColorFor)
                .linkVisibility(linkVisibleFor)
                // Re-set alongside the particle count: the colour accessor is
                // read when a link's particles are (re)initialised, so a hover
                // that only changed direction would otherwise keep the last
                // hover's tint.
                .linkDirectionalParticleColor(linkParticleColorFor)
                .linkDirectionalParticles(linkParticlesFor);
        }

        // ─── The backend ───────────────────────────────────────

        RENDERERS.three = () => ({
            name: 'three',
            caps: { threeD: true, faceViews: true, autoSpin: true, boundaryCube: true },
            // Past this it is handed one neighbourhood at a time — a Group of
            // five objects per node does not scale, which is what solo mode
            // was built for in the first place.
            soloThreshold: THREE_D_MAX_ELEMENTS,

            async mount(el, view) {
                ({ ForceGraph3D, THREE, SpriteText } = await import('./threejs-vis.bundle.js'));
                threeMount(el, view);
            },

            setData(view) {
                if (Graph) Graph.graphData({ nodes: view.nodes, links: view.edges });
            },

            restyle() { threeRestyle(); },

            resize(w, h) { if (Graph) Graph.width(w).height(h); },

            frameAll(ms) { threeSetView('3d', ms); },
            setView(id, ms) { threeSetView(id, ms); },
            frameNodes(ids, ms, opts) { threeFrameNodes(ids, ms, opts); },
            setNodePositions(pos, ms, opts) { threeSetNodePositions(pos, ms, opts); },
            focusNode(n) { threeFocusNode(n); },
            zoomBy(f) { threeZoomBy(f); },
            flyToStop(stop, opts) { threeFlyToStop(stop, opts); },
            frameRoute(ms) { threeFrameRoute(ms); },

            setAutoSpin(on) {
                const controls = Graph && Graph.controls && Graph.controls();
                if (!controls) return;
                controls.autoRotate = on;
                controls.autoRotateSpeed = 1.6;
            },

            setBoundaryVisible(on) {
                if (on) updateBoundaryCube();
                else disposeBoundaryCube();
            },

            emitPulse(node, colour, fromR, toR, growMs) {
                threeEmitPulse(node, colour, fromR, toR, growMs);
            },

            emitSweep(spec) { threeEmitSweep(spec); },

            // Project a node into page pixels via the live camera.
            screenPos(n) {
                if (!Graph || !n || !Number.isFinite(n.x)) return null;
                const c = Graph.graph2ScreenCoords
                    ? Graph.graph2ScreenCoords(n.x, n.y, n.z || 0)
                    : null;
                if (!c) return null;
                const rect = Graph.renderer().domElement.getBoundingClientRect();
                return { x: c.x + rect.left + window.scrollX, y: c.y + rect.top + window.scrollY };
            },

            dispose() {
                window.removeEventListener('mousemove', threeTrackMouse);
                threeGen += 1;
                threeCancelPosAnim();
                disposeWalkLanes();
                disposeBoundaryCube();
                if (Graph) {
                    try { Graph._destructor && Graph._destructor(); } catch (err) { console.error(err); }
                }
                Graph = null;
                selectionRing = null;
                particleField = null;
            },
        });
