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

        // Additive blending for links — richer where strands overlap, and at
        // high resolution the single biggest cost in every frame.
        //
        // A full redraw of the 161,725-node / 745,964-link neo4j index:
        //
        //   3400 × 2000    349 ms blended   →   149 ms unblended   (2.3×)
        //   1600 ×  913    164 ms blended   →   154 ms unblended   (1.06×)
        //
        // It is fill-rate bound, so it barely registers at a laptop window and
        // dominates on a large display — which is why an earlier round measured
        // it and dismissed it. It is also what sets INP on a selection: the
        // response to a click *is* a full redraw.
        //
        // Off is not free. Alpha is ignored at write time, so the dense link
        // mass reads flatter and focus dimming reads differently — about a
        // tenth of the pixels change. Hiding is unaffected: zero-alpha links
        // are still collapsed, so filters behave identically. Left on by
        // default and settable per install, because which side of that trade
        // is right depends on the display it is being read on.
        //
        // Mirrors `vis.link_blending` in native/src/config.rs.
        const LINK_BLENDING_DEFAULT = true;

        function visLinkBlending() {
            const raw = state.capabilities && state.capabilities.vis
                && state.capabilities.vis.link_blending;
            if (raw === 'off' || raw === false) return false;
            if (raw === 'on' || raw === true) return true;
            return LINK_BLENDING_DEFAULT;
        }

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
            // Buffer slot stamped on the edge itself, so a scoped restyle can
            // find it without a Map of 746k entries (~50 MB) that would exist
            // only to be read a dozen at a time on hover. Rewritten on every
            // build, and `cosmosPaintScoped` re-checks `cosmosEdges[i] === e`
            // before trusting it — a stale stamp from a previous view must
            // repaint nothing rather than repaint the wrong strand.
            for (let i = 0; i < cosmosEdges.length; i++) cosmosEdges[i].__ci = i;

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
            cosmosHitInvalidate();
            // A new point set: whatever was pushed described the old one.
            _hlFocus = -2;
            _hlOutlined = undefined;
        }

        // Refresh the colour/width buffers from the shared style rules. Alpha
        // is where the dimming lives: cosmos.gl has no per-node material to
        // fade, so "lowlit" is simply a smaller colour alpha.
        // One node's colour and alpha, written into the shared buffer. Split out
        // of `cosmosPaint` so a scoped restyle can repaint the handful of
        // entries a hover changed instead of all 161,725 of them — see the
        // `scope` argument on `restyle`.
        // Returns whether the entry's four floats actually moved.
        //
        // The comparison is made **through the buffer**, not against the
        // computed doubles. `colors` is a Float32Array, so `colors[k] !== r`
        // is true for almost every finite double even when nothing changed —
        // measured that way, a click appeared to alter 100.0% of both buffers,
        // which is exactly the wrong conclusion. Writing first and comparing
        // the stored values costs nothing and is exact.
        function cosmosPaintNode(i) {
            const { colors } = cosmosBuf;
            const n = cosmosNodes[i];
            const [r, g, b] = cosmosRgb(nodeColorFor(n));
            const { opacity } = nodeLightingFor(n);
            const k = i * 4;
            const o0 = colors[k], o1 = colors[k + 1], o2 = colors[k + 2], o3 = colors[k + 3];
            colors[k] = r; colors[k + 1] = g; colors[k + 2] = b; colors[k + 3] = opacity;
            return o0 !== colors[k] || o1 !== colors[k + 1]
                || o2 !== colors[k + 2] || o3 !== colors[k + 3];
        }

        // One link's colour and alpha, *before* the walk override below. The
        // 3D renderer applies a flat 0.38 and lets fog and depth do the rest.
        // Flat on a plane there is neither, so opacity has to carry the
        // hierarchy itself: a walked or hovered edge is the subject and reads
        // at full strength, everything else is context. Invisible is simply
        // alpha 0 — cosmos.gl has no per-link visibility accessor.
        function cosmosPaintLink(i) {
            const { linkColors } = cosmosBuf;
            const e = cosmosEdges[i];
            const [r, g, b] = cosmosLift(cosmosRgb(linkColorFor(e)));
            const hot = state.highlightLinks.has(e) || linkParticlesFor(e) > 0;
            const k = i * 4;
            const o0 = linkColors[k], o1 = linkColors[k + 1], o2 = linkColors[k + 2], o3 = linkColors[k + 3];
            linkColors[k] = r; linkColors[k + 1] = g; linkColors[k + 2] = b;
            linkColors[k + 3] = linkVisibleFor(e) ? (hot ? 0.95 : 0.55) : 0;
            return o0 !== linkColors[k] || o1 !== linkColors[k + 1]
                || o2 !== linkColors[k + 2] || o3 !== linkColors[k + 3];
        }

        // What the last full repaint actually moved. A whole-graph repaint is
        // not the same thing as a whole-graph *change*: selecting a second node
        // re-evaluates every style rule and moves **122 of 161,725 point
        // colours and 196 of 745,964 link colours** — 0.03% of the buffer, for
        // which the unconditional upload sent 17.5 MB. Collected only while it
        // is still small enough to be worth a partial upload; past that the
        // whole-buffer path wins anyway and the list would just be litter.
        const PAINT_TRACK_LIMIT = 4096;
        let _paintNodes = [];
        let _paintLinks = [];
        let _paintOver = false;      // more changed than it is worth tracking
        let _paintWidths = false;    // a link width moved, so widths need re-sending

        function cosmosPaint() {
            const { linkColors, linkWidths } = cosmosBuf;
            _paintNodes.length = 0;
            _paintLinks.length = 0;
            _paintOver = false;
            _paintWidths = false;
            for (let i = 0; i < cosmosNodes.length; i++) {
                if (!cosmosPaintNode(i)) continue;
                if (_paintNodes.length >= PAINT_TRACK_LIMIT) _paintOver = true;
                else _paintNodes.push(i);
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
                let moved = cosmosPaintLink(i);
                const e = cosmosEdges[i];
                if (walkOwnsEdges && linkColors[i * 4 + 3] > 0) {
                    const sId = e.source.id || e.source;
                    const tId = e.target.id || e.target;
                    const before = linkColors[i * 4 + 3];
                    if (state.walkEdgeKeys.has(walkEdgeKey(sId, tId))) {
                        if (++overlayDrawn <= FX_MAX_FLOW_LINKS) linkColors[i * 4 + 3] = 0;
                    } else {
                        // A cross-link between two reached nodes: real, but not
                        // part of the traversal the cascade is laying out, and
                        // the columns are only readable if the lines between
                        // them are the ones that put them there. It comes back
                        // the moment the walk ends.
                        linkColors[i * 4 + 3] = 0;
                    }
                    // The walk override runs after the paint, so whether it
                    // moved the alpha has to be folded in here.
                    if (before !== linkColors[i * 4 + 3]) moved = true;
                }
                if (moved) {
                    if (_paintLinks.length >= PAINT_TRACK_LIMIT) _paintOver = true;
                    else _paintLinks.push(i);
                }
                // Through the buffer, for the same reason the colours are:
                // `linkWidths` is a Float32Array and 1.4 is not representable,
                // so comparing against the double reported *every* width as
                // changed and defeated the tracking entirely.
                const before2 = linkWidths[i];
                linkWidths[i] = e.rel === 'Contains' ? 1.4 : 0.7;
                if (before2 !== linkWidths[i]) _paintWidths = true;
            }
        }

        // Repaint only what a caller says changed. `scope.nodes` is node ids,
        // `scope.links` is edge objects — the two highlight sets, before and
        // after, when a hover moves.
        //
        // The contract is narrow on purpose: **a scoped restyle asserts that
        // nothing outside these two lists changed appearance**, which is why it
        // also skips `cosmosApplyVisibility` (visibility is appearance, and a
        // hover cannot change it). Anything that touches filters, focus, a
        // tour, a walk or the theme must restyle unscoped.
        //
        // Link widths are structural (`Contains` or not), never style, so a
        // scoped pass leaves them and the buffer they were last written into.
        // Repaint just the scope, and report which slots it touched — the
        // upload below needs exactly that list, and walking the scope twice to
        // rediscover it would be silly.
        function cosmosPaintScoped(scope) {
            const nodes = [];
            const links = [];
            for (const id of scope.nodes) {
                const i = cosmosIndexOf.get(id);
                if (i !== undefined) { cosmosPaintNode(i); nodes.push(i); }
            }
            for (const e of scope.links) {
                const i = e && e.__ci;
                if (i !== undefined && cosmosEdges[i] === e) { cosmosPaintLink(i); links.push(i); }
            }
            return { nodes, links };
        }

        // ── Uploading a handful of colours instead of all of them ──────
        //
        // A hover changes `1 + degree` point colours and `degree` link
        // colours. Handing those to `setPointColors` / `setLinkColors` and
        // calling `render()` made cosmos.gl re-send **the whole buffer**:
        // 2.6 MB of point colours and 11.9 MB of link colours at 745,964
        // links. And `ga()`, the helper behind `updateColor`, does it twice
        // over — one `bufferSubData` of the full array, plus
        // `new Float32Array(t)` to keep as the transition's `previous`.
        //
        // In a trace of a few seconds of ordinary hovering on the neo4j graph,
        // that was **1,168 ms inside `bufferSubData`** — 56% of the whole
        // profile — under `updateColor ← create ← update ← render ← restyle`,
        // with the GPU itself idle (`GPUTask` totalled 27 ms). The matching
        // allocation showed up as the JS heap swinging **338 → 871 MB** across
        // the same five seconds. Both are per hover, and neither is anything
        // the picture needed.
        //
        // The colour arrays we hand cosmos.gl are used **by reference** — 
        // `updatePointColor` does `pointColors = inputPointColors`, and the
        // link path the same — and each entry is four floats at its own index,
        // in the order we built. So slot `i` is bytes `[i*16, i*16+16)` of the
        // GPU buffer, and writing only the slots that changed is not a
        // reinterpretation of anything: it is the same bytes, minus the
        // 14.5 MB either side of them.
        const COLOR_BYTES = 16;             // four float32 per entry
        // Merge two runs separated by fewer than this many entries: sending a
        // few unchanged colours costs less than another driver round trip.
        const UPLOAD_RUN_GAP = 16;
        // Past either of these the partial upload has stopped being a saving —
        // a hub node's 8,680 scattered edges is the case that finds it — and
        // the whole-buffer path is both simpler and faster. Falling back is
        // not a failure; it is the same upload we used to do unconditionally.
        const UPLOAD_MAX_RUNS = 192;
        const UPLOAD_MAX_FRACTION = 0.25;

        // Write `arr`'s entries at `indices` into `buf`. Returns false when it
        // judged the whole buffer cheaper, leaving the caller to do that.
        function cosmosWriteColors(buf, arr, indices) {
            if (!buf || buf.destroyed) return false;
            if (!indices.length) return true;

            const sorted = Int32Array.from(indices).sort();
            const runs = [];
            let start = sorted[0], end = sorted[0];
            for (let k = 1; k < sorted.length; k++) {
                const v = sorted[k];
                if (v === end) continue;
                if (v - end <= UPLOAD_RUN_GAP) { end = v; continue; }
                runs.push(start, end);
                start = end = v;
            }
            runs.push(start, end);

            if (runs.length / 2 > UPLOAD_MAX_RUNS) return false;
            let bytes = 0;
            for (let k = 0; k < runs.length; k += 2) bytes += (runs[k + 1] - runs[k] + 1) * COLOR_BYTES;
            if (bytes > arr.byteLength * UPLOAD_MAX_FRACTION) return false;

            for (let k = 0; k < runs.length; k += 2) {
                const s = runs[k], e = runs[k + 1];
                buf.write(arr.subarray(s * 4, (e + 1) * 4), s * COLOR_BYTES);
            }
            return true;
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
            cosmosHitInvalidate();
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
            cosmosHitInvalidate();
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
            cosmosMotion(dur);
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
                cosmosHitInvalidate();
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
            cosmosMotion(dur);
            cosmos.render(undefined, dur);
            cosmosHitInvalidate();

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
            // The whole opening, in one declaration: hold, spiral, land.
            cosmosMotion(INTRO_HOLD_MS + INTRO_MORPH_MS * 2);
            // The simulation never runs here, so there are no ticks to release
            // the loading overlay — it has to be done explicitly or it would
            // sit over the whole animation.
            requestAnimationFrame(() => requestAnimationFrame(graphReveal));

            _cosmosAnimTimers.push(setTimeout(() => {
                cosmos.setPointPositions(sunflower);
                cosmosMotion(INTRO_MORPH_MS);
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
        // What was last pushed to cosmos.gl, so a restyle that changes neither
        // can stay out of its config entirely. Reset on dispose and on build,
        // where the point set itself changes underneath both.
        let _hlFocus = -2;          // -2 is "nothing pushed yet"; undefined is a real value
        let _hlOutlined;

        function cosmosApplyHighlight() {
            const focusIdx = state.selectedNode
                ? cosmosIndexOf.get(state.selectedNode.id)
                : undefined;
            const outlined = [];
            for (let i = 0; i < cosmosNodes.length; i++) {
                if (cosmosNodes[i].isBoundary) outlined.push(i);
            }
            const outlinedArg = outlined.length ? outlined : undefined;

            // Only when it actually changed.
            //
            // `setConfigPartial` re-derives state from the config, and a new
            // `outlinedPointIndices` array — even one holding the same indices,
            // even an empty one where there was an empty one — makes
            // `updatePointStatus()` rewrite the whole point-status texture:
            // 161,725 points as a 403×403 rgba32float upload. In a trace of a
            // single selection that was **233 ms inside `texSubImage2D`**,
            // under `cosmosApplyHighlight ← restyle`. It also fired on every
            // hover, where neither value can have changed.
            //
            // Compared by content, because the array is rebuilt each call and
            // identity would always differ. It is boundary nodes, so it moves
            // when a filter or a walk moves it, and not otherwise.
            const sameOutlined = _hlOutlined === undefined
                ? outlinedArg === undefined
                : (outlinedArg !== undefined
                    && _hlOutlined.length === outlinedArg.length
                    && _hlOutlined.every((v, k) => v === outlinedArg[k]));
            if (focusIdx !== _hlFocus || !sameOutlined) {
                _hlFocus = focusIdx;
                // **Pass the array we pushed last time when the contents match.**
                // cosmos.gl compares `outlinedPointIndices` by *identity*, and a
                // difference there re-runs `updatePointStatus()` — a 403×403
                // rgba32float rewrite over all 161,725 points. Selecting a
                // different node changes `focusedPointIndex` but not the
                // boundary set, so handing over a freshly built array of the
                // same indices paid 2.6 MB per click for nothing: 355 ms of
                // `texSubImage2D` across a session of selections.
                if (!sameOutlined) _hlOutlined = outlinedArg;
                cosmos.setConfigPartial({
                    focusedPointIndex: focusIdx,
                    outlinedPointIndices: _hlOutlined,
                });
            }

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

        // The tracked-position map, read **once per overlay frame**.
        //
        // `getTrackedPointPositionsMap()` is a GPU→CPU readback
        // (`readPixelsToArrayWebGL`, which the driver services with a
        // `readPixels` into a pixel buffer and a `getBufferSubData` out of it)
        // whenever cosmos.gl's positions are not already up to date. It stalls
        // the pipeline: nothing else can proceed until the GPU has caught up
        // and handed the bytes back.
        //
        // The overlay asked for it once per *call site*, and it has two: once
        // per node it haloes (up to FX_MAX_HALOS) and **twice** per link it
        // draws (up to FX_MAX_FLOW_LINKS) — up to ~1,400 readbacks in a single
        // frame, for one map that cannot change inside that frame. Profiled on
        // a 161,725-node canvas, that was 45% of all time spent while the
        // pointer was moving. See P12.7 in docs/dev/PERF-TUNING-JOURNEY.md.
        // And it is not needed at all once the layout has settled. Tracking
        // exists so a ring stays glued to a node the *simulation* is still
        // moving; with the simulation stopped, `n.x`/`n.y` are exactly where
        // the points are — `onSimulationEnd` syncs them from the same
        // framebuffer. So a settled canvas skips the readback entirely and
        // `cosmosLivePos` falls through to the synced coordinates.
        let _cosmosPosMap = null;
        function cosmosPositionsThisFrame() {
            if (!_cosmosPosMap) {
                _cosmosPosMap = (!cosmos || !cosmos.isSimulationRunning) ? new Map()
                    : ((cosmos.getTrackedPointPositionsMap && cosmos.getTrackedPointPositionsMap()) || new Map());
            }
            return _cosmosPosMap;
        }

        // Called at the top of every overlay frame. The cache is only ever
        // valid *within* one frame — a running simulation moves points between
        // frames, and it is the overlay's job to say when one begins.
        function cosmosInvalidatePositions() { _cosmosPosMap = null; }

        // Live position of a point, in space coords — the tracked value if we
        // asked for it, else the last synced one. Null when the point is absent.
        function cosmosLivePos(node) {
            const i = cosmosIndexOf.get(node.id);
            if (i === undefined) return null;
            const p = cosmosPositionsThisFrame().get(i);
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

        // ─── Level of detail while the scene is moving ─────────
        //
        // Links are ~85% of every frame, and no link *setting* recovers it.
        // Measured on the 161,725-node / 745,964-link neo4j index, median
        // cost of one full redraw:
        //
        //   full (curved links, arrows, blending)     164–208 ms
        //   curvedLinkSegments 19 → 5                     170 ms
        //   linkBlending off                              154 ms
        //   arrows off                                    183 ms
        //   arrows off *and* straight links               130 ms
        //   **links not drawn at all**                     37 ms
        //
        // So the rule is: while something is moving, don't draw them. A camera
        // flight over this graph ran at **3.7 fps** with links and **120 fps**
        // without, and it lands on an identical picture either way — the links
        // come back the moment the motion stops. This is the whole of what
        // made animation feel broken at this size; it is not hover, and it was
        // never the restyle.
        //
        // Deadline-based rather than a nesting count: every animation declares
        // how long it will take, a later one extends the deadline, and one
        // timer puts the links back. Nothing has to pair its own begin with
        // its own end — which is what would eventually strand a graph with no
        // links in it, and would be a far worse bug than the one this fixes.
        //
        // Below MOTION_LINK_LIMIT this does nothing at all: a small graph
        // redraws inside a frame anyway, so links blinking out would be a
        // flicker bought for no gain.
        const MOTION_LINK_LIMIT = 60000;
        // A beat past the animation's own end — for the frame that lands on
        // the final position, and for a settle that overruns its nominal
        // duration.
        const MOTION_TAIL_MS = 120;

        let _motionUntil = 0;
        let _motionTimer = null;
        let _motionHiding = false;

        // "Something is about to move for `ms`." Safe from anywhere, including
        // with 0 — a continuous motion (a zoom gesture, a simulation tick)
        // just keeps re-arming the tail.
        function cosmosMotion(ms) {
            // Announced to the FX overlay first and unconditionally: it has to
            // redraw its labels, boundary and rings for the whole of any
            // motion, and unlike the link budget below that is true at every
            // graph size. This is the single call site for "something is
            // moving", which is why the overlay needs none of its own.
            overlayAnimateFor(ms);
            if (!cosmos || cosmosEdges.length < MOTION_LINK_LIMIT) return;
            const until = performance.now() + Math.max(0, ms || 0) + MOTION_TAIL_MS;
            if (until > _motionUntil) _motionUntil = until;
            if (!_motionHiding) {
                _motionHiding = true;
                // Config only — no render, no transition reset (setConfigPartial
                // just merges and re-reads state). The next frame, whoever asks
                // for it, draws points alone.
                cosmos.setConfigPartial({ renderLinks: false });
            }
            if (_motionTimer) clearTimeout(_motionTimer);
            _motionTimer = setTimeout(cosmosMotionEnd, Math.max(0, _motionUntil - performance.now()));
        }

        function cosmosMotionEnd() {
            if (_motionTimer) { clearTimeout(_motionTimer); _motionTimer = null; }
            _motionUntil = 0;
            if (!_motionHiding) return;
            _motionHiding = false;
            if (!cosmos) return;
            cosmos.setConfigPartial({ renderLinks: true });
            // The config change only flags; this is the frame that actually
            // puts the links back on screen. Same reason `restyle` renders —
            // cosmos.gl's frame loop does not upload or re-read on its own.
            cosmos.render(undefined, 0);
        }

        // ─── Hit testing on the CPU ────────────────────────────
        //
        // cosmos.gl picks the point under the pointer on the GPU: it redraws
        // every point into an offscreen index buffer, then reads a small
        // window of that buffer back with `readPixels` + `getBufferSubData`.
        // The readback cannot return until the GPU has finished the frame it
        // is queued behind, and on a canvas drawing 745,964 links that frame
        // is most of a tenth of a second. The cost is not work — through the
        // whole stall the CPU sits ~59% idle — so it does not show up as a
        // busy page. It shows up as a pointer skating ahead of its own
        // highlight.
        //
        // Measured on the 161,725-node neo4j index (P12.8):
        //
        //   pointer parked on a node    8.3 ms/frame,  0 of 481 over 16.7 ms
        //   pointer moving            115.3 ms/frame, 40 of  54 over
        //   moving, a quarter of the events  114.8 ms  ← per *move*, not per event
        //
        // So: stop asking the GPU. A uniform grid over the point positions
        // answers "what is under this pixel" in microseconds, and its cost
        // scales with the points *near the cursor*, not with the size of the
        // scene — which is the property the GPU pick did not have.
        //
        // What this deliberately does **not** do is take over cosmos.gl's
        // event plumbing. `Graph.onClick` decides point-vs-background by
        // reading `store.hoveredPoint`; the hover ring is drawn from
        // `store.hoveredPoint.index`; the cursor follows it too. So this
        // replaces the *picker* and still hands the result to cosmos.gl's own
        // `processHoverResult` — clicks, the ring, the cursor and the
        // mouseover/mouseout callbacks all behave exactly as before. Only the
        // readback is gone. See cosmosInstallCpuPicking.

        // Cells per axis. ~4 points per cell on a 160k graph, and the grid
        // costs the same 173 KB of Int32Array however the points are spread.
        const HIT_GRID_DIM = 208;
        // Screen-space forgiveness — how far outside its disc a point is still
        // caught. Not a taste call: it is cosmos.gl's own tolerance, and it
        // takes two constants to arrive at.
        //
        // cosmos.gl reads a **9×9 window** of the picking framebuffer around
        // the cursor and takes the nearest covered pixel in it, so it forgives
        // ±4 buffer pixels beyond whatever the point draws. And it renders
        // that buffer at **half resolution** (`Qu = 0.5` in the vendored
        // bundle, read back as `pickingFbo.width / screenSize[0]` = 0.5). So
        // one buffer pixel is two device pixels, and the real tolerance is
        // `8 / pixelRatio` CSS pixels — twice what the 9×9 window alone
        // suggests.
        //
        // Getting this wrong is not subtle. At 3 CSS px, agreement with the
        // GPU pick was 71%. At 4 device px it was 97%, and the pixels it still
        // missed were 8.8 px from a 2.4 px disc — comfortably inside
        // cosmos.gl's window and outside ours.
        //
        // It is a constant number of *screen* pixels at every zoom, which is
        // what cosmos.gl's window is. An earlier version capped it at one node
        // radius in space, meaning to bound the cell scan at a wide zoom; what
        // it actually did was clamp the tolerance back to about half its value
        // and silently undo the correction above. The scan is bounded by the
        // grid being finite (worst case: every cell, i.e. one pass over the
        // points — still microseconds, and far cheaper than the readback this
        // replaced).
        const HIT_PICK_WINDOW_PX = 4;      // half of cosmos.gl's 9×9 readback
        const HIT_PICK_BUFFER_SCALE = 0.5; // it renders that buffer at half res
        // How long a cached canvas rect is trusted. The hit test runs once per
        // frame, and `getBoundingClientRect` is the one part of it that can
        // force layout.
        const HIT_RECT_TTL = 500;

        let hitStart = null;    // Int32Array(cells + 1): CSR cell offsets
        let hitItems = null;    // Int32Array: point indices, cell by cell
        let hitCellOf = null;   // Int32Array scratch: point → cell, reused
        let hitOx = 0, hitOy = 0, hitCell = 1, hitMaxR = 0;
        let hitDirty = true;
        let hitRect = null, hitRectAt = 0;

        // Positions moved, the point set changed, or something was hidden.
        // Cheap: the grid is rebuilt on the next hit test, not here, so a
        // burst of changes costs one rebuild rather than one each.
        function cosmosHitInvalidate() { hitDirty = true; hitRect = null; }

        function hitCellIndex(x, y) {
            let cx = ((x - hitOx) / hitCell) | 0;
            let cy = ((y - hitOy) / hitCell) | 0;
            if (cx < 0) cx = 0; else if (cx >= HIT_GRID_DIM) cx = HIT_GRID_DIM - 1;
            if (cy < 0) cy = 0; else if (cy >= HIT_GRID_DIM) cy = HIT_GRID_DIM - 1;
            return cy * HIT_GRID_DIM + cx;
        }

        // Counting sort into a CSR grid: one pass to count, a prefix sum, one
        // pass to place. No per-cell arrays — 43k of them would be exactly the
        // allocation churn the rest of this file exists to avoid — and the
        // three typed arrays are reused across rebuilds, which matters because
        // a running simulation invalidates this every 400 ms (cosmosSync).
        function cosmosHitBuild() {
            hitDirty = false;
            hitStart = null;
            const n = cosmosNodes.length;
            if (!n || !cosmosBuf || !cosmosHidden) return;
            const sizes = cosmosBuf.sizes;

            let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
            let maxR = 0;
            for (let i = 0; i < n; i++) {
                // Hidden points are NaN in the position buffer, which is how
                // they are already excluded from the forces and from the GPU
                // pick. Leaving them out here keeps that true.
                if (cosmosHidden[i]) continue;
                const nd = cosmosNodes[i];
                const x = nd.x, y = nd.y;
                if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
                if (x < minX) minX = x;
                if (x > maxX) maxX = x;
                if (y < minY) minY = y;
                if (y > maxY) maxY = y;
                if (sizes[i] > maxR) maxR = sizes[i];
            }
            if (!Number.isFinite(minX)) return;   // nothing visible to hit

            const span = Math.max(maxX - minX, maxY - minY);
            hitOx = minX;
            hitOy = minY;
            // A degenerate span — one visible node, or a layout that stacked
            // them — would divide by zero. Falling back to a positive cell
            // puts everything in cell 0, which is correct and merely slow.
            hitCell = span > 0 ? span / HIT_GRID_DIM : 1;
            hitMaxR = maxR;

            const cells = HIT_GRID_DIM * HIT_GRID_DIM;
            if (!hitCellOf || hitCellOf.length < n) hitCellOf = new Int32Array(n);
            if (!hitItems || hitItems.length < n) hitItems = new Int32Array(n);
            const start = new Int32Array(cells + 1);
            const cellOf = hitCellOf;
            const items = hitItems;

            for (let i = 0; i < n; i++) {
                const nd = cosmosNodes[i];
                if (cosmosHidden[i] || !Number.isFinite(nd.x) || !Number.isFinite(nd.y)) {
                    cellOf[i] = -1;
                    continue;
                }
                const c = hitCellIndex(nd.x, nd.y);
                cellOf[i] = c;
                start[c + 1]++;
            }
            for (let c = 0; c < cells; c++) start[c + 1] += start[c];
            const cursor = start.slice(0, cells);
            for (let i = 0; i < n; i++) {
                const c = cellOf[i];
                if (c >= 0) items[cursor[c]++] = i;
            }
            hitStart = start;
        }

        // The canvas rect, cached. Read once per frame otherwise.
        function hitCanvasRect() {
            const now = performance.now();
            if (hitRect && now - hitRectAt < HIT_RECT_TTL) return hitRect;
            const canvas = cosmos && (cosmos.canvas || document.querySelector('#graph-3d canvas'));
            if (!canvas) return null;
            hitRect = canvas.getBoundingClientRect();
            hitRectAt = now;
            return hitRect;
        }

        // The point under a client-space pixel, in cosmos.gl's own pick shape
        // (`{index, position}`) so it can be handed straight to
        // `processHoverResult`. Null when the pointer is over empty canvas.
        function cosmosHitTest(clientX, clientY) {
            if (!cosmos || !cosmos.isReady || !cosmosBuf) return null;
            // Mid-morph the node objects and the GPU disagree *by design*: the
            // position buffer is being tweened and `n.x`/`n.y` hold one end of
            // it. Hovering against either lights up the wrong node, so during
            // a transition nothing is hovered — which is also what the old GPU
            // pick did (`findHoveredItem` bails while a transition is active).
            if (!_cosmosIntroDone || performance.now() < _cosmosMorphUntil) return null;
            if (hitDirty) cosmosHitBuild();
            if (!hitStart || !hitItems) return null;

            const rect = hitCanvasRect();
            if (!rect) return null;
            const sp = cosmos.screenToSpacePosition([clientX - rect.left, clientY - rect.top]);
            if (!sp || !Number.isFinite(sp[0]) || !Number.isFinite(sp[1])) return null;

            // One space unit is this many CSS pixels at the current zoom —
            // `scalePointsOnZoom` is on, so a node's radius is a constant in
            // space and the slop is what has to be converted.
            const scale = cosmos.spaceToScreenRadius(1) || 1;
            // `pixelRatio` is cosmos.gl's own config default — we never set it.
            const ratio = (window.devicePixelRatio || 2) * HIT_PICK_BUFFER_SCALE;
            const slopPx = HIT_PICK_WINDOW_PX / ratio;
            const slop = slopPx / scale;
            const reach = hitMaxR + slop;

            const dim = HIT_GRID_DIM;
            let x0 = Math.floor((sp[0] - reach - hitOx) / hitCell);
            let x1 = Math.floor((sp[0] + reach - hitOx) / hitCell);
            let y0 = Math.floor((sp[1] - reach - hitOy) / hitCell);
            let y1 = Math.floor((sp[1] + reach - hitOy) / hitCell);
            if (x1 < 0 || y1 < 0 || x0 >= dim || y0 >= dim) return null;
            if (x0 < 0) x0 = 0;
            if (y0 < 0) y0 = 0;
            if (x1 >= dim) x1 = dim - 1;
            if (y1 >= dim) y1 = dim - 1;

            const sizes = cosmosBuf.sizes;
            const px = sp[0], py = sp[1];
            let best = -1, bestD2 = Infinity;
            for (let cy = y0; cy <= y1; cy++) {
                const row = cy * dim;
                for (let cx = x0; cx <= x1; cx++) {
                    const c = row + cx;
                    const end = hitStart[c + 1];
                    for (let k = hitStart[c]; k < end; k++) {
                        const i = hitItems[k];
                        const nd = cosmosNodes[i];
                        const dx = nd.x - px, dy = nd.y - py;
                        const d2 = dx * dx + dy * dy;
                        if (d2 >= bestD2) continue;
                        // Inside the disc it draws, plus the slop.
                        const t = sizes[i] + slop;
                        if (d2 > t * t) continue;
                        // Nearest *centre* wins where two discs overlap.
                        //
                        // Reasoning said rim distance: cosmos.gl scans its
                        // readback window for the covered pixel nearest the
                        // cursor, which sounds like nearest surface. Measured
                        // against the GPU pick over 245 sampled pixels it is
                        // the other way round — centre 97.6% agreement, rim
                        // 95.5% — because the picking pass is depth-tested and
                        // the depth is not the rule the scan implies. Do not
                        // "fix" this back without re-running scratch/equal.
                        bestD2 = d2;
                        best = i;
                    }
                }
            }
            if (best < 0) return null;
            const hit = cosmosNodes[best];
            return { index: best, position: [hit.x, hit.y] };
        }

        // Swap cosmos.gl's GPU picker for the grid above.
        //
        // Two methods are replaced, on the *instance* rather than the
        // prototype, so nothing else sharing the class is touched:
        //
        //   findHoveredItem(force)   runs once per frame from `renderFrame`,
        //                            and synchronously on pointerdown so a
        //                            click knows what it landed on. Ours does
        //                            the grid lookup and hands the result to
        //                            cosmos.gl's own `processHoverResult` —
        //                            which is what sets `store.hoveredPoint`,
        //                            fires onPointMouseOver / onPointMouseOut
        //                            and updates the cursor.
        //   resolvePendingPick()     collects a readback we now never issue.
        //
        // Run every frame rather than only when the pointer moves, because
        // panning and zooming slide nodes under a stationary pointer and the
        // GPU pick re-ran for exactly that reason. It is affordable now:
        // `processHoverResult` fires callbacks only when the picked index
        // actually changes, so a repeat costs the lookup and nothing else.
        //
        // These are minified-bundle internals. This checks they are there and
        // leaves cosmos.gl's own picking alone if a re-vendor renames them —
        // slow hover is a far better failure than no hover. Re-check it when
        // bumping the pinned version (docs/VISUALIZATION.md §3.3).
        function cosmosInstallCpuPicking() {
            if (!cosmos
                || typeof cosmos.findHoveredItem !== 'function'
                || typeof cosmos.resolvePendingPick !== 'function'
                || typeof cosmos.processHoverResult !== 'function') {
                console.warn('[ug] cosmos.gl pick hooks not found — leaving GPU picking on');
                return false;
            }
            cosmos.resolvePendingPick = function () {};
            cosmos.findHoveredItem = function (force) {
                if (this._isDestroyed) return;
                if (!force && !this._isPointerOnCanvas) return;
                // cosmos.gl tracks the pointer in client coords on every
                // pointermove and pointerdown; `state._mouse` is our own copy
                // of the same thing, from the onMouseMove config handler.
                const m = state._mouse;
                const x = Number.isFinite(this._lastMouseX) ? this._lastMouseX : (m && m.cx);
                const y = Number.isFinite(this._lastMouseY) ? this._lastMouseY : (m && m.cy);
                if (!Number.isFinite(x) || !Number.isFinite(y)) return;
                this.processHoverResult(cosmosHitTest(x, y), undefined);
                // **Not optional, and not bookkeeping for its own sake.**
                //
                // `shouldKeepRendering()` — the predicate that decides whether
                // the rAF loop runs another frame — asks `hasPendingHoverWork()`,
                // which is "has the pointer moved more than 2px since the last
                // time hover was *checked*". Only the real `findHoveredItem`
                // advanced `_lastCheckedMouse*`, so leaving it behind pins that
                // answer at true for as long as the pointer is over the canvas,
                // and cosmos.gl redraws all 745,964 links every frame forever.
                // Measured: 8.3 ms/frame → 116 ms/frame while parked on a node,
                // with the CPU 95% idle — a renderer spinning on the GPU for a
                // frame nothing asked for.
                this._lastCheckedMouseX = this._lastMouseX;
                this._lastCheckedMouseY = this._lastMouseY;
                this._shouldForceHoverDetection = false;
                this._findHoveredItemExecutionCount = 0;
            };
            return true;
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
                    linkBlending: visLinkBlending(),
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
                    // Panning and zooming are the motion the user drives by
                    // hand, and where 3.7 fps is least forgivable. `cosmosMotion(0)`
                    // re-arms the tail while the gesture continues; the links
                    // land back a beat after it stops. Camera *flights* pass
                    // through here too and simply extend the deadline their
                    // caller already set.
                    onZoom: () => cosmosMotion(0),
                    onMouseMove: (_i, _p, event) => {
                        if (!event) return;
                        state._mouse = {
                            x: event.pageX, y: event.pageY,
                            cx: event.clientX, cy: event.clientY,
                        };
                    },
                    // Every tick moves every point and the links follow them —
                    // that is the frame this cannot afford. Re-armed rather than
                    // held, so a simulation that stops without firing
                    // `onSimulationEnd` still gets its links back.
                    onSimulationTick: () => {
                        cosmosMotion(0);
                        overlayInvalidate();
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
                            cosmosMotion(220);
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
                        cosmosMotionEnd();
                        cosmosSync();
                        state._boxSettled = true;
                        if (!state._didFit && cosmosHasPoints()) {
                            state._didFit = true;
                            cosmosMotion(350);
                            cosmos.fitView(350, 0.15);
                        }
                        refreshModeLegend();
                        graphReveal();
                    },
                });

                await cosmos.ready;
                cosmosInstallCpuPicking();
                cosmosBuild(view);
                // Snap the first upload into place. The default 800 ms
                // transition tweens every point from nothing on load, which
                // reads as the graph arriving late rather than arriving.
                cosmos.render(undefined, 0);
                cosmosApplyHighlight();
                _cosmosIntroDone = false;

                // The opening lands on whichever arrangement `state.layout2d`
                // names — a computed layout, so it arrives and holds. Read
                // rather than hard-coded: the default belongs with the rest of
                // the defaults (00-preamble.js), and a literal here silently
                // outranked it, so changing the documented default changed
                // nothing anyone could see. cosmosSetLayout owns the rest:
                // marking the switcher, publishing positions, flushing any
                // restyle that was deferred during the morph.
                cosmosPlayIntro((ms) => {
                    _cosmosIntroDone = true;
                    _cosmosEarlyFit = true;
                    state._didFit = true;
                    cosmosSetLayout(state.layout2d || 'spiral', ms);
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

            // `scope`, when given, names the only nodes and links whose
            // appearance can have changed — see `cosmosPaintScoped` for the
            // contract. Without it every node and link is repainted, which on
            // a graph drawn whole is 161,725 + 745,964 style evaluations per
            // pointer move: measured at 40 ms of a 68 ms restyle, four dropped
            // frames on every hover. See P12.3 in
            // docs/dev/PERF-TUNING-JOURNEY.md.
            //
            // A walk forces the full pass whatever the caller says: the walk
            // branch in `cosmosPaint` counts along the whole ordered edge list
            // to stay in step with the overlay's `FX_MAX_FLOW_LINKS` budget, so
            // one link's alpha there is not a function of that link alone.
            restyle(scope) {
                if (!cosmos || !cosmosBuf) return;
                if (!_cosmosIntroDone || performance.now() < _cosmosMorphUntil) {
                    _cosmosRestylePending = true;
                    return;
                }
                const scoped = scope && !state.walkActive;
                if (scoped) {
                    // The fast path: repaint the scope, push only those slots
                    // to the GPU, and let the frame we are already inside draw
                    // them. No whole-buffer upload, no `render()`, and so none
                    // of `update()` → `create()` → `updateColor()` → `ga()`.
                    const touched = cosmosPaintScoped(scope);
                    const pts = cosmos.points, lns = cosmos.lines;
                    if (pts && lns
                        && cosmosWriteColors(pts.targetColorBuffer, cosmosBuf.colors, touched.nodes)
                        && cosmosWriteColors(lns.targetColorBuffer, cosmosBuf.linkColors, touched.links)) {
                        cosmosApplyHighlight();
                        // We are normally called from inside cosmos.gl's own
                        // frame, ahead of its draw — but `handleNodeHover` has
                        // callers that are not, so ask for a frame rather than
                        // assume one.
                        cosmos.requestRender();
                        return;
                    }
                    // Too scattered to be worth it (a hub node's edges), or the
                    // buffers are not up yet. Fall through to the full upload —
                    // the colours are already painted, so nothing is lost.
                } else {
                    cosmosPaint();
                }

                // The unscoped path — a filter, a theme, a selection, a walk
                // step. It re-evaluates every style rule, but re-evaluating
                // everything is not the same as *changing* everything, and
                // until now it uploaded as though it were.
                //
                // `cosmosPaint` reports what actually moved. Selecting a second
                // node moves 122 point colours and 196 link colours out of
                // 907,689 entries; the first selection, which turns focus mode
                // on and dims the whole graph, moves essentially all of them.
                // The same code now takes both cases correctly.
                let visChanged = false;
                if (!scoped) {
                    visChanged = cosmosApplyVisibility();
                    const pts = cosmos.points, lns = cosmos.lines;
                    if (!_paintOver && !_paintWidths && !visChanged && pts && lns
                        && cosmosWriteColors(pts.targetColorBuffer, cosmosBuf.colors, _paintNodes)
                        && cosmosWriteColors(lns.targetColorBuffer, cosmosBuf.linkColors, _paintLinks)) {
                        cosmosApplyHighlight();
                        cosmos.requestRender();
                        return;
                    }
                }

                // Genuinely global. Write both colour buffers whole, rather
                // than going through `setPointColors` / `setLinkColors`:
                // cosmos.gl's `updateColor` sends the same bytes and *also*
                // keeps `new Float32Array(everything)` as the transition's
                // previous frame — 14.5 MB of allocation for a picture that is
                // already decided. Widths only when one moved.
                const pts = cosmos.points, lns = cosmos.lines;
                const buffersReady = pts && pts.targetColorBuffer && !pts.targetColorBuffer.destroyed
                    && lns && lns.targetColorBuffer && !lns.targetColorBuffer.destroyed;
                if (visChanged) {
                    // `cosmosApplyVisibility` uploaded positions, and
                    // `setPointPositions` flags colours as stale along with
                    // them — so `render()` below will send them anyway. Writing
                    // here too would send the same 14.5 MB twice.
                } else if (buffersReady) {
                    pts.targetColorBuffer.write(cosmosBuf.colors, 0);
                    lns.targetColorBuffer.write(cosmosBuf.linkColors, 0);
                } else {
                    cosmos.setPointColors(cosmosBuf.colors);
                    cosmos.setLinkColors(cosmosBuf.linkColors);
                }
                if (!scoped && _paintWidths) cosmos.setLinkWidths(cosmosBuf.linkWidths);
                cosmosApplyHighlight();
                // Snap rather than animate: a restyle is a response to a hover
                // or a filter, and an 800 ms colour tween reads as lag.
                //
                // **Always, including a scoped restyle.** P12.7 skipped this
                // for scoped restyles on the reasoning that a hover restyle
                // runs inside cosmos.gl's own frame (`restyle ← bumpGraphStyles
                // ← handleNodeHover ← onPointMouseOver ← processHoverResult ←
                // findHoveredItem ← renderFrame`) and ahead of that frame's
                // draw, so the draw would pick up the buffers the setters just
                // flagged. It does not. `setPointColors` only sets
                // `inputPointColors` and `isPointColorUpdateNeeded`; the
                // `bufferSubData` that actually uploads lives in `create()`,
                // which only `render()` reaches. `Points.draw()` calls
                // `updateColor()` only when the buffer does not exist yet.
                //
                // The result was silent: hovering a node with 8,680 links
                // changed **zero pixels** of canvas outside the tooltip, while
                // every buffer in memory held the right colours. Buffer
                // equality could not see it; a screenshot diff could. See
                // P12.8, and scratch/paint.mjs.
                //
                // And it costs nothing to be correct here. Measured on the
                // 161,725-node canvas, one full redraw is ~163 ms whether or
                // not the 14.5 MB of colour buffers are uploaded first
                // (160.5 ms with, 162.6 ms without). The redraw is the price;
                // the upload is noise beside it.
                cosmos.render(undefined, 0);
            },

            // The canvas is sized by CSS (100% of #graph-3d), so cosmos.gl
            // picks the new size up itself; nothing to push.
            resize() {},

            // Both fits are reachable with an empty canvas — Reset and the 1–6
            // keys are live in solo mode before a node has been picked — and
            // fitting a camera to no points is fatal (see cosmosHasPoints).
            frameAll(ms) { if (cosmosHasPoints()) { cosmosMotion(ms); cosmos.fitView(ms, 0.15); } },

            // Every face projection collapses to the same 2D fit. The buttons
            // are hidden by caps, but the 1–6 keyboard shortcuts still land here.
            setView(_id, ms) { if (cosmosHasPoints()) { cosmosMotion(ms); cosmos.fitView(ms, 0.15); } },

            // `opts.flat` is a 3D notion (which way the camera points); a plane
            // is already flat, so the fit is the same either way.
            frameNodes(ids, ms) {
                if (!cosmos) return;
                const idx = cosmosIndicesFor(ids);
                if (!idx.length) return;
                cosmosMotion(ms);
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
                requestAnimationFrame(() => {
                    cosmosMotion(800);
                    cosmos.zoomToPointByIndex(i, 800, 4);
                });
            },

            zoomBy(factor) {
                if (!cosmos) return;
                // The 3D renderer's factor scales an orbit *radius*, so <1 is
                // closer. A zoom level is the reciprocal of that.
                cosmosMotion(180);
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
                    cosmosMotion(opts.ms || 1100);
                    cosmos.fitViewByPointIndices(idx, opts.ms || 1100, 0.28);
                });
            },

            frameRoute(ms) {
                if (!cosmos || !tourState.data) return;
                const idx = cosmosIndicesFor(tourState.routeIds);
                if (idx.length < 2) return;
                cosmosMotion(ms || 1400);
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
                if (_motionTimer) clearTimeout(_motionTimer);
                _motionTimer = null;
                _motionUntil = 0;
                _motionHiding = false;
                _hlFocus = -2;
                _hlOutlined = undefined;
                hitStart = null;
                hitItems = null;
                hitCellOf = null;
                hitRect = null;
                hitDirty = true;
            },
        });
