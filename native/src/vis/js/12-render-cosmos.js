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

        // Raise a near-black colour to something a flat renderer can actually
        // show. The walk's background tones (`linkFar` #1b1b21, `linkRecede`
        // #26262e) were chosen for a 3D scene where fog and depth separate an
        // edge from the backdrop; on a 2D plane they sit below the noise floor
        // and the graph looks like it has no edges at all.
        //
        // Only genuinely dark colours are touched, and they are lifted toward
        // the background's own hue rather than washed to grey, so "receded"
        // still reads as receded — just visibly so.
        function cosmosLift([r, g, b]) {
            const lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            if (lum >= 0.18) return [r, g, b];
            const k = Math.min(1, (0.18 - lum) * 3.2);
            const t = 0.42;   // the tone dark links are lifted toward
            return [r + (t - r) * k, g + (t - g) * k, b + (t * 1.06 - b) * k];
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

        // ─── Shape by node type ────────────────────────────────
        //
        // Shape is a second, redundant channel alongside colour, and it is the
        // one that survives everything colour does not: a node dimmed under a
        // focus filter, recoloured by a walk hop, or flared orange on selection
        // keeps its silhouette. Grouped by family rather than one shape per
        // type — seven silhouettes is a code to memorise, four is a glance.
        //
        //   square   containers            Folder, File
        //   diamond  declared types        Class, Interface
        //   hexagon  behaviour             Function
        //   circle   everything else       data, deps, config, routes
        function cosmosShapeFor(group) {
            const S = CosmosLib.PointShape;
            switch (group) {
                case 'Folder':
                case 'File':
                    return S.Square;
                case 'Class':
                case 'Interface':
                    return S.Diamond;
                case 'Function':
                    return S.Hexagon;
                default:
                    return S.Circle;
            }
        }

        // How much of the point the glyph may fill, per shape. The image is
        // drawn inside the point's quad, so a shape whose inscribed circle is
        // small clips the glyph at its corners: a diamond's is 0.71 of the half
        // width, against the ~0.75 the glyph already spans. Square and circle
        // contain the full quad and need no trim.
        function cosmosGlyphFit(group) {
            switch (group) {
                case 'Class':
                case 'Interface':
                    return 0.82;
                case 'Function':
                    return 0.94;
                default:
                    return 1;
            }
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
                shapes[i] = cosmosShapeFor(node.group);
                imageIdx[i] = cosmosImageIndex.get(cosmosImageKey(node)) ?? -1;
                sizes[i] = nodeRadiusFor(node);
                imageSizes[i] = sizes[i] * cosmosGlyphFit(node.group);
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
            // While a walk runs the overlay redraws every reached edge as a
            // straight, arrowed, hop-coloured strand (fxDrawWalkEdges). These
            // links are curved (`curvedLinks`), so leaving them on meant two
            // lines between the same two nodes, bowing apart — a cascade whose
            // columns are joined by a thicket rather than by legible strands.
            // So during a walk the overlay owns the edges and these stand down.
            //
            // Not blindly, though: the overlay stops after FX_MAX_FLOW_LINKS
            // strands, and an edge nothing draws is an edge that does not
            // exist as far as the diagram is concerned. It walks `cosmosEdges`
            // in this same order and counts the same way, so counting along
            // here identifies exactly the edges it will get to.
            const walkOwnsEdges = state.walkActive && state.walkEdgeKeys && state.walkEdgeKeys.size > 0;
            let overlayDrawn = 0;
            for (let i = 0; i < cosmosEdges.length; i++) {
                const e = cosmosEdges[i];
                const [r, g, b] = cosmosLift(cosmosRgb(linkColorFor(e)));
                linkColors[i * 4] = r; linkColors[i * 4 + 1] = g; linkColors[i * 4 + 2] = b;
                // The 3D renderer applies a flat 0.38 and lets fog and depth do
                // the rest. Flat on a plane there is neither, so opacity has to
                // carry the hierarchy itself: a walked or hovered edge is the
                // subject and reads at full strength, everything else is
                // context. Invisible is simply alpha 0 — cosmos.gl has no
                // per-link visibility accessor.
                const hot = state.highlightLinks.has(e) || linkParticlesFor(e) > 0;
                let alpha = linkVisibleFor(e) ? (hot ? 0.95 : 0.55) : 0;
                if (walkOwnsEdges && alpha > 0) {
                    const sId = e.source.id || e.source;
                    const tId = e.target.id || e.target;
                    if (state.walkEdgeKeys.has(walkEdgeKey(sId, tId))) {
                        if (++overlayDrawn <= FX_MAX_FLOW_LINKS) alpha = 0;
                    } else {
                        // A cross-link between two reached nodes: real, but not
                        // part of the traversal the cascade is laying out, and
                        // the columns are only readable if the lines between
                        // them are the ones that put them there. It comes back
                        // the moment the walk ends.
                        alpha = 0;
                    }
                }
                linkColors[i * 4 + 3] = alpha;
                linkWidths[i] = e.rel === 'Contains' ? 1.4 : 0.7;
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
            const { positions } = cosmosBuf;
            const changed = [];
            for (let i = 0; i < cosmosNodes.length; i++) {
                const hide = nodeVisibleFor(cosmosNodes[i]) ? 0 : 1;
                if (hide === cosmosHidden[i]) continue;
                cosmosHidden[i] = hide;
                changed.push(i);
            }
            if (!changed.length) return false;

            // Re-read the GPU before uploading anything.
            //
            // `cosmosBuf.positions` is only ever what was *last uploaded*. The
            // simulation and the user's dragging both move points afterwards,
            // and neither writes back here — so uploading this buffer to change
            // two nodes' visibility silently reset every other node to where it
            // used to be. Selecting a node after dragging one snapped the whole
            // graph back, which is what "the node moves when I select it" was.
            const live = cosmos.getPointPositions();
            if (live && live.length >= positions.length) {
                for (let k = 0; k < positions.length; k++) {
                    // Hidden points read back as NaN; skipping them keeps the
                    // last known-good spot, which is what they return to.
                    if (Number.isFinite(live[k])) positions[k] = live[k];
                }
            }

            for (const i of changed) {
                if (cosmosHidden[i]) {
                    positions[i * 2] = NaN;
                    positions[i * 2 + 1] = NaN;
                } else {
                    const n = cosmosNodes[i];
                    positions[i * 2] = +n.x || 0;
                    positions[i * 2 + 1] = +n.y || 0;
                }
            }
            cosmos.setPointPositions(positions);
            return true;
        }

        // Is there anything on the GPU to read or frame?
        //
        // Every cosmos.gl camera helper that fits a view — `fitView`,
        // `getPointPositions` — works by reading the position framebuffer back
        // off the GPU. With no points that framebuffer is never allocated, and
        // the readback dies inside luma.gl destructuring `device` off an
        // undefined texture ("Cannot destructure property 'device' of 'e'").
        // Thrown out of `mount`, that becomes "Could not start the cosmos
        // renderer" and the page never comes up at all.
        //
        // An empty view is not a corner case here: past SOLO_THRESHOLD
        // elements the page opens in solo mode, whose view holds nothing until
        // a node is picked (see applySoloMode). So *every* large graph mounts
        // through this path.
        function cosmosHasPoints() {
            return !!cosmos && cosmosNodes.length > 0;
        }

        // Read the GPU's positions back onto the node objects.
        //
        // The rest of the page treats `n.x` / `n.y` as the truth — extent
        // maths, framing, the tooltip and the walk all read them — but under
        // cosmos.gl the simulation runs on the GPU and those fields would
        // otherwise stay frozen at their seeded values. Absent (NaN) points are
        // skipped so hiding a node never destroys the position it comes back to.
        function cosmosSync() {
            if (!cosmos || !cosmos.isReady || !cosmosHasPoints()) return;
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

        // ─── The opening morph: galaxy → sunflower → force ─────
        //
        // The graph used to arrive by drifting in from one corner as the force
        // simulation cooled — which is not an animation, it is a layout being
        // computed in public. This replaces it with a deliberate one, using the
        // same mechanism cosmos.gl's own transition examples do: positions are
        // just a buffer, and `render(alpha, duration)` tweens the whole buffer
        // on the GPU. So the opening is three staged arrangements.
        //
        //   1. the galaxy, struck instantly and held for a beat
        //   2. the sunflower disc — the spiral arms unwind into an even field
        //   3. the folder islands — that field gathers into the shape of the
        //      codebase, which is the view worth landing on
        //
        // The simulation never runs during any of it, and does not take over at
        // the end: all three are computed arrangements, so each one arrives and
        // then holds perfectly still. Handing off to the force layout at the end
        // would undo stage 3 in public, which is the thing this replaced.

        const INTRO_HOLD_MS = 620;     // how long the galaxy stands before it moves
        const INTRO_MORPH_MS = 1000;   // one stage → the next

        // Pending timers for whatever animation is in flight — the opening's
        // stages, or a layout morph's write-back. Cleared on dispose and
        // whenever a new animation supersedes the last.
        let _cosmosAnimTimers = [];
        let _cosmosIntroDone = false;
        // A restyle that arrived mid-morph. Applying it there would upload
        // positions and re-render at zero duration, cancelling the transition
        // the intro is in the middle of — so it is deferred to the settle.
        let _cosmosRestylePending = false;
        // When the in-flight position transition finishes. Restyles arriving
        // before then are deferred — see cosmosSetLayout.
        let _cosmosMorphUntil = 0;

        function cosmosClearIntro() {
            _cosmosAnimTimers.forEach(clearTimeout);
            _cosmosAnimTimers = [];
        }

        // A small deterministic PRNG, so the opening is the same every load —
        // the intro is choreography, and choreography that reshuffles each time
        // reads as noise.
        function cosmosRand(seed) {
            let s = seed >>> 0;
            return () => {
                s = (s * 1664525 + 1013904223) >>> 0;
                return s / 4294967296;
            };
        }

        // A barred spiral: a dense core bulge, then log-spiral arms whose
        // angular scatter widens with radius. This is also a genuinely good
        // seed for the force layout — it is already roughly radial, so the
        // simulation has little left to undo.
        function cosmosGalaxyPositions(count) {
            const out = new Float32Array(count * 2);
            const rand = cosmosRand(0xc05a1);
            const cx = COSMOS_SPACE / 2, cy = COSMOS_SPACE / 2;
            const ARMS = 3;
            const SPAN = COSMOS_SPACE * 0.30;
            const TWIST = 2.6;
            for (let i = 0; i < count; i++) {
                const t = count > 1 ? i / (count - 1) : 0;
                // pow < 1 concentrates points toward the centre — the bulge.
                const r = SPAN * Math.pow(t, 0.62);
                const arm = (i % ARMS) * ((Math.PI * 2) / ARMS);
                // Scatter tightens near the core and frays at the rim, which is
                // what makes it read as arms rather than as three curves.
                const spread = 0.22 + 0.55 * t;
                const wobble = (rand() + rand() + rand() - 1.5) * spread;
                const theta = arm + (r / SPAN) * TWIST * Math.PI + wobble;
                const jitter = (rand() - 0.5) * SPAN * 0.06;
                out[i * 2] = cx + Math.cos(theta) * (r + jitter);
                out[i * 2 + 1] = cy + Math.sin(theta) * (r + jitter) * 0.82;  // slight tilt
            }
            return out;
        }

        // ─── The other layouts ─────────────────────────────────
        //
        // The 2D renderer's answer to the 3D one's six face projections: with
        // no camera to move, the thing worth switching is the *arrangement*.
        // Each of these is a pure function from node count (and, where it is
        // interesting, node type) to a position buffer, so any of them can be
        // morphed into with the same GPU transition the opening uses.
        //
        // Only `force` hands control to the simulation. The rest are static
        // arrangements, and the simulation is switched off while one is showing
        // — otherwise the forces pull it apart within a second of arriving.

        // Golden angle: the packing that makes a sunflower head even at every
        // radius, with no ring seams.
        const PHYLLOTAXIS_ANGLE = Math.PI * (3 - Math.sqrt(5));

        // Nodes grouped by type, in a stable order — the data-aware layouts all
        // want the same grouping, and it has to be deterministic or the
        // arrangement reshuffles every time you switch back to it.
        function cosmosGroups() {
            const by = new Map();
            for (let i = 0; i < cosmosNodes.length; i++) {
                const g = cosmosNodes[i].group || 'Default';
                const list = by.get(g);
                if (list) list.push(i); else by.set(g, [i]);
            }
            // Canonical type order (see NODE_TYPE_ORDER), not population. These
            // orderings are *spatial* — the ring a type sits on, its place in
            // the grid, where its island lands — so they should read the way a
            // codebase nests rather than reshuffling with the data. Ties broken
            // by name so it never depends on insertion order.
            return [...by.entries()].sort((a, b) =>
                nodeTypeRank(a[0]) - nodeTypeRank(b[0]) || a[0].localeCompare(b[0]));
        }

        // An even disc. Every node gets the same amount of room, so this is the
        // honest "here is how much there is" view.
        function cosmosSpiralPositions(count) {
            const out = new Float32Array(count * 2);
            const cx = COSMOS_SPACE / 2, cy = COSMOS_SPACE / 2;
            const R = COSMOS_SPACE * 0.30;
            for (let i = 0; i < count; i++) {
                const r = R * Math.sqrt((i + 0.5) / count);
                const theta = i * PHYLLOTAXIS_ANGLE;
                out[i * 2] = cx + Math.cos(theta) * r;
                out[i * 2 + 1] = cy + Math.sin(theta) * r;
            }
            return out;
        }

        // A lattice, ordered by type then name. Reading order becomes a sorted
        // index of the codebase — the one arrangement where position is a
        // lookup rather than a shape.
        function cosmosGridPositions(count) {
            const out = new Float32Array(count * 2);
            const order = [];
            for (const [, idx] of cosmosGroups()) {
                idx.sort((a, b) => String(cosmosNodes[a].name).localeCompare(String(cosmosNodes[b].name)));
                order.push(...idx);
            }
            const cols = Math.max(1, Math.ceil(Math.sqrt(count)));
            const rows = Math.max(1, Math.ceil(count / cols));
            const span = COSMOS_SPACE * 0.62;
            const step = span / Math.max(cols, rows);
            const x0 = COSMOS_SPACE / 2 - (cols - 1) * step / 2;
            const y0 = COSMOS_SPACE / 2 - (rows - 1) * step / 2;
            order.forEach((node, k) => {
                out[node * 2] = x0 + (k % cols) * step;
                out[node * 2 + 1] = y0 + Math.floor(k / cols) * step;
            });
            return out;
        }

        // One concentric ring per node type, commonest innermost. The ring a
        // node sits on *is* its type, so the composition of the codebase reads
        // straight off the radii.
        function cosmosRingPositions(count) {
            const out = new Float32Array(count * 2);
            const cx = COSMOS_SPACE / 2, cy = COSMOS_SPACE / 2;
            const groups = cosmosGroups();
            const inner = COSMOS_SPACE * 0.06;
            const step = (COSMOS_SPACE * 0.32 - inner) / Math.max(1, groups.length - 1 || 1);
            groups.forEach(([, idx], g) => {
                const r = inner + g * step;
                idx.forEach((node, k) => {
                    // Offset each ring so the rings don't line up into spokes.
                    const theta = (k / idx.length) * Math.PI * 2 + g * 0.618;
                    out[node * 2] = cx + Math.cos(theta) * r;
                    out[node * 2 + 1] = cy + Math.sin(theta) * r;
                });
            });
            return out;
        }

        // Pack a set of point indices into a disc — a sunflower again, so the
        // island has even density and no ring seams. Shared by the two
        // island-style layouts.
        function cosmosPackIsland(out, indices, gx, gy, rad) {
            const n = indices.length || 1;
            indices.forEach((node, k) => {
                const r = rad * Math.sqrt((k + 0.5) / n);
                const theta = k * PHYLLOTAXIS_ANGLE;
                out[node * 2] = gx + Math.cos(theta) * r;
                out[node * 2 + 1] = gy + Math.sin(theta) * r;
            });
        }

        // Each type as its own island, the islands laid around a ring and each
        // packed as a little sunflower sized to its population. The modular
        // read: how many kinds of thing there are, and how big each one is.
        function cosmosClusterPositions(count) {
            const out = new Float32Array(count * 2);
            const cx = COSMOS_SPACE / 2, cy = COSMOS_SPACE / 2;
            const groups = cosmosGroups();
            const ring = COSMOS_SPACE * 0.22;
            // Max, not groups[0] — these are ordered by type now, not by size,
            // so the first entry is the *smallest* group as often as not.
            const biggest = groups.reduce((m, g) => Math.max(m, g[1].length), 1);
            groups.forEach(([, idx], g) => {
                const a = (g / Math.max(1, groups.length)) * Math.PI * 2;
                // Area ∝ population, so a cluster's size is readable against
                // its neighbours rather than every blob being the same.
                const rad = COSMOS_SPACE * 0.075 * Math.sqrt(idx.length / biggest) + 20;
                cosmosPackIsland(out, idx, cx + Math.cos(a) * ring, cy + Math.sin(a) * ring, rad);
            });
            return out;
        }

        // ─── By-folder islands ─────────────────────────────────
        //
        // One named island per directory: the layout that says which parts of
        // the codebase hang together, straight off the folder structure.
        //
        // cosmos.gl offers a cluster *force* for this (`setPointClusters` +
        // `simulationCluster`), which is how its own example does it — but a
        // force has to converge, and converging in public is exactly the slow
        // arrival this whole sequence exists to avoid. Computed directly, the
        // islands are simply *there*, one morph away, and they hold still.
        //
        // Folders come from each node's `file` path. Nodes without one
        // (dependencies, say) go to an outer ring rather than being forced into
        // a folder they are not in.

        // Beyond this the labels collide and the islands stop being separable;
        // the tail joins the unfiled ring rather than becoming a fake "other".
        const MAX_FOLDER_CLUSTERS = 28;

        // Island names and their centres in space coords, published for the FX
        // overlay to label. Rebuilt whenever the folder layout is.
        let cosmosClusterNames = [];
        let cosmosClusterCentres = null;

        function cosmosFolderOf(n) {
            const f = n.file;
            if (!f) return null;
            const i = String(f).lastIndexOf('/');
            return i > 0 ? String(f).slice(0, i) : '/';
        }

        // Folders as [name, indices], biggest first, ties by name — so
        // switching away and back lands on exactly the same islands.
        function cosmosFolderGroups() {
            const by = new Map();
            const unfiled = [];
            for (let i = 0; i < cosmosNodes.length; i++) {
                const f = cosmosFolderOf(cosmosNodes[i]);
                if (!f) { unfiled.push(i); continue; }
                const list = by.get(f);
                if (list) list.push(i); else by.set(f, [i]);
            }
            const ranked = [...by.entries()]
                .sort((a, b) => b[1].length - a[1].length || a[0].localeCompare(b[0]));
            const kept = ranked.slice(0, MAX_FOLDER_CLUSTERS);
            // Everything past the cap joins the unfiled ring.
            ranked.slice(MAX_FOLDER_CLUSTERS).forEach(([, idx]) => unfiled.push(...idx));
            return { groups: kept, unfiled };
        }

        function cosmosFolderPositions(count) {
            const out = new Float32Array(count * 2);
            const cx = COSMOS_SPACE / 2, cy = COSMOS_SPACE / 2;
            const { groups, unfiled } = cosmosFolderGroups();
            cosmosClusterNames = groups.map(g => g[0]);
            cosmosClusterCentres = new Float32Array(groups.length * 2);

            const SPAN = COSMOS_SPACE * 0.26;
            const biggest = groups.length ? groups[0][1].length : 1;
            groups.forEach(([, idx], g) => {
                // Island centres on a phyllotaxis spiral rather than a ring:
                // with up to 28 folders a single ring leaves a hole in the
                // middle and crowds the rim. The biggest folder lands at the
                // centre, which is also where the eye starts.
                const theta = g * PHYLLOTAXIS_ANGLE;
                const rr = SPAN * Math.sqrt((g + 0.28) / Math.max(1, groups.length));
                const gx = cx + Math.cos(theta) * rr;
                const gy = cy + Math.sin(theta) * rr;
                const rad = COSMOS_SPACE * 0.055 * Math.sqrt(idx.length / biggest) + 14;
                cosmosPackIsland(out, idx, gx, gy, rad);
                cosmosClusterCentres[g * 2] = gx;
                cosmosClusterCentres[g * 2 + 1] = gy;
            });

            // The unfiled: a thin annulus outside everything, so they read as
            // context around the codebase rather than part of any folder.
            const ring = SPAN * 1.55;
            unfiled.forEach((node, k) => {
                const a = (k / Math.max(1, unfiled.length)) * Math.PI * 2;
                const jitter = ((k % 7) - 3) * (COSMOS_SPACE * 0.004);
                out[node * 2] = cx + Math.cos(a) * (ring + jitter);
                out[node * 2 + 1] = cy + Math.sin(a) * (ring + jitter);
            });
            return out;
        }

        // The catalogue the viewbar is built from.
        //
        //   kind 'static' — a position buffer, morphed into with the simulation
        //                   off, so it arrives and then holds still
        //   kind 'force'  — hand the arrangement back to the simulation
        const COSMOS_LAYOUTS = {
            force: { label: 'FORCE', kind: 'force' },
            galaxy: { label: 'GAL', kind: 'static', build: cosmosGalaxyPositions },
            spiral: { label: 'SUN', kind: 'static', build: cosmosSpiralPositions },
            grid: { label: 'GRID', kind: 'static', build: cosmosGridPositions },
            rings: { label: 'RING', kind: 'static', build: cosmosRingPositions },
            clusters: { label: 'CLUS', kind: 'static', build: cosmosClusterPositions },
            folders: { label: 'FOLDER', kind: 'static', build: cosmosFolderPositions },
        };

        const LAYOUT_MORPH_MS = 900;

        // Switch arrangement. Static layouts stop the simulation and morph the
        // position buffer; `force` hands it back and re-heats.
        function cosmosSetLayout(name, ms) {
            if (!cosmos || !cosmosBuf || !COSMOS_LAYOUTS[name]) return;
            cosmosClearIntro();
            _cosmosIntroDone = true;
            state.layout2d = name;
            // An empty view has no arrangement to make — and asking cosmos.gl
            // to fit a camera to nothing is fatal (see cosmosHasPoints). The
            // choice still sticks: `setData` re-applies it the moment the view
            // has nodes again, which in solo mode is the very next click.
            if (!cosmosHasPoints()) { syncLayoutButtons(); return; }
            const dur = ms == null ? LAYOUT_MORPH_MS : ms;
            // Anything that would re-render at zero duration has to wait until
            // this lands, or it cancels the transition mid-flight.
            _cosmosMorphUntil = performance.now() + dur;

            const spec = COSMOS_LAYOUTS[name];
            // Only the folder layout names its islands; anything else clears
            // the labels rather than leaving them floating over a new shape.
            if (name !== 'folders') cosmosClusterNames = [];

            if (spec.kind === 'force') {
                cosmos.setConfigPartial({ enableSimulation: true });
                _cosmosEarlyFit = false;
                _cosmosStartedAt = performance.now();
                // Enough heat to re-find a layout, short of throwing the
                // current arrangement away — you asked to switch layout, not
                // to reload the page.
                cosmos.start(0.7);
                syncLayoutButtons();
                return;
            }

            const pos = spec.build(cosmosNodes.length);
            cosmos.setConfigPartial({ enableSimulation: false });
            cosmosBuf.positions.set(pos);
            cosmosApplyVisibilityInto(cosmosBuf.positions);
            cosmos.setPointPositions(cosmosBuf.positions);
            cosmos.render(undefined, dur);
            cosmos.fitView(dur, 0.16);
            // The page treats n.x/n.y as the truth, and with the simulation off
            // there are no ticks to sync from — so publish the arrangement
            // once the tween has actually landed on it.
            _cosmosAnimTimers.push(setTimeout(() => {
                for (let i = 0; i < cosmosNodes.length; i++) {
                    cosmosNodes[i].x = pos[i * 2];
                    cosmosNodes[i].y = pos[i * 2 + 1];
                    cosmosNodes[i].z = 0;
                }
                // A restyle that arrived mid-morph was deferred rather than
                // allowed to cancel the transition; apply it now.
                if (_cosmosRestylePending) {
                    _cosmosRestylePending = false;
                    bumpGraphStyles();
                }
            }, dur));
            syncLayoutButtons();
        }

        // Prescribed positions from outside the renderer — the Graph Walk
        // cascade. Same mechanism as a static layout (positions are a buffer,
        // `render(alpha, ms)` tweens it on the GPU), but the geometry comes
        // from the caller rather than from COSMOS_LAYOUTS, and only the named
        // nodes move: everything else holds the spot it already had.
        function cosmosSetNodePositions(pos, ms) {
            if (!cosmos || !cosmosBuf || !cosmosHasPoints()) return;
            // A morph in flight would fight this one, and the opening's staged
            // timers would land on top of it.
            cosmosClearIntro();
            _cosmosIntroDone = true;
            const dur = Math.max(0, ms || 0);
            const buf = cosmosBuf.positions;

            // Re-read the GPU first. `cosmosBuf.positions` is only ever what
            // was last *uploaded*; the simulation and dragging both move points
            // afterwards without writing back, so starting from the stale
            // buffer would snap every untouched node to where it used to be.
            const live = cosmos.getPointPositions();
            if (live && live.length >= buf.length) {
                for (let k = 0; k < buf.length; k++) {
                    if (Number.isFinite(live[k])) buf[k] = live[k];
                }
            }
            for (let i = 0; i < cosmosNodes.length; i++) {
                const p = pos.get(cosmosNodes[i].id);
                if (!p) continue;
                buf[i * 2] = p.x;
                buf[i * 2 + 1] = p.y;
                // The page treats n.x/n.y as the truth — framing, the tooltip,
                // the overlay's strands and the next visibility change all read
                // them — and with the simulation off there are no ticks to sync
                // from. Published now rather than when the tween lands, because
                // an unhide arriving mid-morph reads them to decide where the
                // node comes back.
                cosmosNodes[i].x = p.x;
                cosmosNodes[i].y = p.y;
                cosmosNodes[i].z = 0;
            }
            // A prescribed arrangement and a running simulation are the same
            // contradiction the static layouts have: whoever writes the
            // positions last wins, every tick.
            cosmos.setConfigPartial({ enableSimulation: false });
            cosmosApplyVisibilityInto(buf);
            cosmos.setPointPositions(buf);
            cosmos.render(undefined, dur);

            // Restyles arriving mid-tween would upload at zero duration and
            // cancel it, so `restyle()` parks them in `_cosmosRestylePending`
            // — and something has to let them out again. Exiting a walk
            // restyles the instant it starts the morph home, and without this
            // the whole canvas would stay in walk colours until the next hover
            // happened to shake it loose.
            const flush = () => {
                if (!_cosmosRestylePending) return;
                _cosmosRestylePending = false;
                bumpGraphStyles();
            };
            if (dur) {
                _cosmosMorphUntil = performance.now() + dur;
                _cosmosAnimTimers.push(setTimeout(flush, dur));
            } else {
                flush();
            }
        }

        // Re-apply the NaN holes for hidden nodes over a freshly built layout,
        // which knows nothing about what is filtered out.
        function cosmosApplyVisibilityInto(positions) {
            if (!cosmosHidden) return;
            for (let i = 0; i < cosmosHidden.length; i++) {
                if (!cosmosHidden[i]) continue;
                positions[i * 2] = NaN;
                positions[i * 2 + 1] = NaN;
            }
        }

        // Someone who has asked the OS for less motion should get the graph,
        // not a performance.
        function cosmosReducedMotion() {
            return window.matchMedia
                && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
        }

        // Run the opening: galaxy → sunflower → folder islands. `land` is the
        // final stage, which is a normal layout switch like any other.
        function cosmosPlayIntro(land) {
            const n = cosmosNodes.length;
            cosmosClearIntro();
            if (!n || cosmosReducedMotion()) {
                land(0);
                // Neither of these paths runs the simulation, so no tick will
                // take the loading overlay down — without this it sits there
                // until mount's 4 s backstop, over a canvas that is already
                // finished (or, for an empty solo view, over the card
                // explaining how to put something on it).
                requestAnimationFrame(() => requestAnimationFrame(graphReveal));
                return;
            }

            const sunflower = cosmosSpiralPositions(n);
            cosmos.setPointPositions(cosmosGalaxyPositions(n));
            cosmos.render(undefined, 0);
            // Frame it immediately — the whole point is that something
            // deliberate is on screen from the first frame.
            cosmos.fitView(0, 0.24);
            // The simulation never runs here, so there are no ticks to release
            // the loading overlay — it has to be done explicitly or it would
            // sit over the whole animation.
            requestAnimationFrame(() => requestAnimationFrame(graphReveal));

            _cosmosAnimTimers.push(setTimeout(() => {
                cosmos.setPointPositions(sunflower);
                cosmos.render(undefined, INTRO_MORPH_MS);
                cosmos.fitView(INTRO_MORPH_MS, 0.16);
            }, INTRO_HOLD_MS));

            _cosmosAnimTimers.push(setTimeout(
                () => land(INTRO_MORPH_MS),
                INTRO_HOLD_MS + INTRO_MORPH_MS
            ));
        }

        // ─── Selection / highlight ─────────────────────────────

        // Selection marking, and *only* marking.
        //
        // cosmos.gl offers `highlightedPointIndices`, which greys out every
        // point not named in it. Using it here dimmed the graph a second time:
        // the shared style rules already express dimming as colour alpha
        // (nodeLightingFor — 0.95 for a focused neighbourhood, 0.06 for the
        // background), so a selected node's neighbours were multiplied down to
        // 0.95 × pointGreyoutOpacity and all but vanished. Exactly the nodes
        // you selected the node to look at.
        //
        // So dimming stays in one place — the alpha channel, shared with the 3D
        // renderer — and this sets only the rings, which add emphasis without
        // taking any away.
        function cosmosApplyHighlight() {
            const focusIdx = state.selectedNode
                ? cosmosIndexOf.get(state.selectedNode.id)
                : undefined;
            const outlined = [];
            for (let i = 0; i < cosmosNodes.length; i++) {
                if (cosmosNodes[i].isBoundary) outlined.push(i);
            }
            cosmos.setConfigPartial({
                focusedPointIndex: focusIdx,
                outlinedPointIndices: outlined.length ? outlined : undefined,
            });

            // Ask cosmos.gl to publish live positions for the handful of points
            // the overlay draws on top of. This is the cheap subset readback —
            // no full GPU stall — and it is what keeps the selection ring
            // centred on its node while the simulation moves it or the user
            // drags it. Reading n.x/n.y instead leaves the ring at wherever the
            // node last happened to be written back.
            const tracked = [];
            if (focusIdx !== undefined) tracked.push(focusIdx);
            state.highlightNodes.forEach(id => {
                const i = cosmosIndexOf.get(id);
                if (i !== undefined && i !== focusIdx) tracked.push(i);
            });
            cosmos.trackPointPositionsByIndices(tracked);
        }

        // Live position of a point, in space coords — the tracked value if we
        // asked for it, else the last synced one. Null when the point is absent.
        function cosmosLivePos(node) {
            const i = cosmosIndexOf.get(node.id);
            if (i === undefined) return null;
            const m = cosmos.getTrackedPointPositionsMap && cosmos.getTrackedPointPositionsMap();
            const p = m && m.get(i);
            if (p && Number.isFinite(p[0]) && Number.isFinite(p[1])) return p;
            return Number.isFinite(node.x) ? [node.x, node.y] : null;
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
            // No third dimension: no face projections and no orbit to spin.
            // Reported rather than silently ignored — the viewbar hides what
            // this cannot do. The bounding box survives the flattening, though
            // — the FX overlay draws it as a framed rectangle — so its toggle
            // stays live.
            caps: { threeD: false, faceViews: false, autoSpin: false, boundaryCube: true },
            // Instanced points and a GPU simulation, so the whole graph stays
            // on screen far longer than the 3D renderer manages. The threshold
            // follows `vis.solo_threshold` from config with SOLO_THRESHOLD as
            // the fallback.
            soloThreshold: visSoloThreshold(),

            async mount(el, view) {
                CosmosLib = await import('./cosmos-vis.bundle.js');
                cosmos = new CosmosLib.Graph(el, {
                    spaceSize: COSMOS_SPACE,
                    backgroundColor: CANVAS.bg,
                    // Held off until the opening morph finishes — the forces
                    // would otherwise dissolve the letters mid-transition.
                    // Re-enabled by the intro's settle step.
                    enableSimulation: false,
                    // With the simulation off, cosmos.gl auto-rescales incoming
                    // positions unless told not to. That would quietly rewrite
                    // the intro's carefully placed glyph and spiral coordinates.
                    rescalePositions: false,
                    transitionEasing: CosmosLib.TransitionEasing.CubicInOut,
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
                    // Occlusion culling assumes opaque points; ours carry their
                    // dimming in the alpha channel, so it would drop points
                    // that are meant to show through each other.
                    pointOcclusionCulling: false,
                    renderHoveredPointRing: true,
                    hoveredPointRingColor: '#f96716',
                    focusedPointRingColor: '#ff3d00',
                    outlinedPointRingColor: BOUNDARY_IN_COLOR,
                    hoveredPointCursor: 'pointer',
                    // Off, and deliberately. A point drag begins on mousedown
                    // over a node — which is also how every click on a node
                    // starts — so the smallest movement while picking one
                    // shoved it somewhere the layout never put it. The graph
                    // is a diagram of the code, not a canvas to arrange: a
                    // node that has moved is saying something false about
                    // where the layout placed it, and nothing offers to undo
                    // it. Panning and zooming still move the *view*.
                    enableDrag: false,
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
                    // Enough damping to settle promptly, not so much that the
                    // layout freezes before it has untangled.
                    simulationFriction: 0.55,
                    // **This is a tick count, and lower is faster.**
                    //
                    // cosmos.gl's own docs say the opposite ("use smaller
                    // values if you want the simulation to cool down slower"),
                    // but the maths is unambiguous: alpha decays by
                    // `1 - ALPHA_MIN^(1/decay)` per tick, so after `decay`
                    // ticks it has reached the floor. The default 5000 is
                    // therefore ~80 seconds of simulation at 60fps.
                    //
                    // The 3D renderer caps itself at 100 ticks (cooldownTicks)
                    // precisely so a layout is never a wait. This is the 2D
                    // equivalent: start(0.7) × 300 lands around 200 ticks.
                    simulationDecay: 300,
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
                    // A drag moves points on the GPU and writes back nowhere.
                    // Under a static layout there are no simulation ticks to
                    // pick that up either, so without this every position the
                    // page reads — framing, the tooltip, the walk, the next
                    // visibility change — would keep describing where the node
                    // used to be. Once per drag, not per frame: the overlay
                    // already follows the dragged node through tracked
                    // positions, so this only has to settle the bookkeeping.
                    onDragEnd: () => cosmosSync(),
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
                        if (!_cosmosEarlyFit && cosmosHasPoints()
                            && performance.now() - _cosmosStartedAt > 220) {
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
                        if (!state._didFit && cosmosHasPoints()) {
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
                cosmosApplyHighlight();
                _cosmosIntroDone = false;

                // The opening lands on the folder islands — a computed layout,
                // so it arrives and holds. cosmosSetLayout owns the rest:
                // marking the switcher, publishing positions, flushing any
                // restyle that was deferred during the morph.
                cosmosPlayIntro((ms) => {
                    _cosmosIntroDone = true;
                    _cosmosEarlyFit = true;
                    state._didFit = true;
                    cosmosSetLayout('folders', ms);
                });

                // If the engine never ticks (an empty solo view, say), don't
                // leave the loading overlay up forever.
                setTimeout(graphReveal, 4000);
            },

            setData(view) {
                if (!cosmos) return;
                // A view swap is not an arrival — solo mode changes this on
                // every click, and replaying the opening each time would be
                // absurd. Straight to the layout.
                cosmosClearIntro();
                cosmosBuild(view);
                cosmos.render(undefined, 0);
                _cosmosIntroDone = true;
                cosmosApplyHighlight();
                // A static arrangement has to be rebuilt for the new node set —
                // the layouts are functions of the view, so the old buffer
                // describes the wrong nodes. Instant, because a solo click is
                // navigation, not an arrival.
                if (state.layout2d && state.layout2d !== 'force') {
                    cosmosSetLayout(state.layout2d, 0);
                    return;
                }
                if (!cosmosHasPoints()) return;
                _cosmosEarlyFit = false;
                _cosmosStartedAt = performance.now();
                cosmos.setConfigPartial({ enableSimulation: true });
                cosmos.start(1);
            },

            restyle() {
                if (!cosmos || !cosmosBuf) return;
                if (!_cosmosIntroDone || performance.now() < _cosmosMorphUntil) {
                    _cosmosRestylePending = true;
                    return;
                }
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

            // Both fits are reachable with an empty canvas — Reset and the 1–6
            // keys are live in solo mode before a node has been picked — and
            // fitting a camera to no points is fatal (see cosmosHasPoints).
            frameAll(ms) { if (cosmosHasPoints()) cosmos.fitView(ms, 0.15); },

            // Every face projection collapses to the same 2D fit. The buttons
            // are hidden by caps, but the 1–6 keyboard shortcuts still land here.
            setView(_id, ms) { if (cosmosHasPoints()) cosmos.fitView(ms, 0.15); },

            // `opts.flat` is a 3D notion (which way the camera points); a plane
            // is already flat, so the fit is the same either way.
            frameNodes(ids, ms) {
                if (!cosmos) return;
                const idx = cosmosIndicesFor(ids);
                if (!idx.length) return;
                cosmos.fitViewByPointIndices(idx, ms, 0.2);
            },

            setNodePositions(pos, ms) { cosmosSetNodePositions(pos, ms); },

            // Everything here lives in cosmos.gl's simulation space, so a
            // layout computed around the origin has to be moved into it.
            space() {
                return { cx: COSMOS_SPACE / 2, cy: COSMOS_SPACE / 2, cz: 0, size: COSMOS_SPACE };
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

            // The 2D stand-in for the 3D renderer's face projections.
            layouts: COSMOS_LAYOUTS,
            setLayout(name, ms) { cosmosSetLayout(name, ms); },

            // The walk's ignition effects are drawn by the 2D FX overlay.
            emitPulse(node, colour, fromR, toR, growMs) {
                overlayEmitPulse(node, colour, fromR, toR, growMs);
            },

            emitSweep(spec) { overlayEmitSweep(spec); },

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
                // Pending intro steps would otherwise fire into a destroyed
                // instance a beat after the renderer was swapped away.
                cosmosClearIntro();
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
