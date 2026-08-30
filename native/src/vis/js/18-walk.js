        // ─── Graph Walk (Discover → Walk) ────────────────────
        //
        // An animated, step-able BFS frontier. Pick a seed and the walk takes
        // over the canvas in a floating playback card (like a Guided Tour):
        // the sidebar collapses, chrome dims, and transport controls
        // (prev / play / next + speed) drive the reveal hop by hop. Each hop
        // is a two-phase beat — edges stream toward the new frontier, then
        // the frontier ignites — and a wavefront sphere grows from the seed
        // to enclose the reached set. Recent walks are kept for replay.
        //
        // Mutually exclusive with tour/focus styling: every render accessor
        // it touches is gated on `state.walkActive`, and exiting returns the
        // canvas to whatever the user had before.

        // Hot core cooling outward into the structural blues — the seed is
        // the hottest point and the wave loses energy as it radiates.
        const WALK_HOP_COLORS = ['#ff3d00', '#fb923c', '#f5b041', '#a3b8d4', '#5b8fc9', '#2f5f96'];
        // Base (1×) beat timing, in ms. Each hop is a two-phase beat: edges
        // ignite and stream toward the new frontier, then the frontier nodes
        // ignite. Both phases scale by 1/walkSpeed.
        const WALK_EDGE_IGNITE_MS = 440;
        const WALK_HOP_DWELL_MS = 1000;
        const WALK_SPEEDS = [0.5, 1, 2];
        const WALK_POS_KEY = 'ug-walk-pos';
        const WALK_HISTORY_MAX = 6;
        const walkEl = (id) => document.getElementById(id);

        function walkColorForHop(h) {
            return WALK_HOP_COLORS[Math.min(h, WALK_HOP_COLORS.length - 1)];
        }
        const walkIgniteMs = () => WALK_EDGE_IGNITE_MS / state.walkSpeed;
        const walkDwellMs = () => WALK_HOP_DWELL_MS / state.walkSpeed;
        const walkFrameMs = () => Math.max(380, Math.min(1300, 950 / state.walkSpeed));

        // tier helper consulted by bumpGraphStyles (10-graph-render.js).
        //   seed    — the starting node
        //   reached — ignited, lit in its hop colour
        //   pending — in the reached set but not yet ignited (edges streaming
        //             toward it); rendered as a faint ghost target
        //   far     — not reached (hidden by nodeVisibleFor during a walk)
        function walkTier(id) {
            if (!state.walkActive) return null;
            if (id === state.walkSeed) return 'seed';
            if (state.walkColors.has(id)) return 'reached';
            if (state.walkReached.has(id)) return 'pending';
            return 'far';
        }

        // Unordered edge key, matching the one used in linkColorFor /
        // linkParticlesFor. Survives the renderer rewriting source/target
        // into node objects, and the cloned edges solo mode rebuilds.
        function walkEdgeKey(sId, tId) {
            return sId < tId ? sId + '|' + tId : tId + '|' + sId;
        }

        const delay = (ms) => new Promise(r => setTimeout(r, ms));

        // ─── The cascade: laying the walk out so it reads ────
        //
        // A walk used to light nodes up wherever the force layout had already
        // dropped them. That shows you *which* nodes were reached and nothing
        // else: hop 3 can sit left of hop 1, a caller can sit downstream of the
        // thing it calls, and the frontier arrives as a scatter of sparks
        // rather than as an expansion. Every question a walk is actually asked
        // — how far does this reach, what does it fan out into, which way does
        // the dependency point — is a question about *arrangement*, and the
        // arrangement was the one thing the walk did not control.
        //
        // So while a walk runs the reached subgraph is re-laid-out from
        // scratch, on three rules:
        //
        //   1. **One column per hop, marching the way the edges point.**
        //      Distance from the seed is horizontal distance on screen, so
        //      "three hops out" is something you can see rather than count.
        //
        //   2. **Following an edge forward goes right; following one backward
        //      goes left.** So an arrow always points the way you are reading.
        //      An outbound walk grows rightward, an inbound walk grows leftward
        //      into the seed, and a `both` walk splits into two wings around
        //      it: everything the seed reaches on the right, everything that
        //      reaches the seed on the left. That one rule is what turns "who
        //      calls this / what does this call" into a picture.
        //
        //   3. **Inside a column, sit next to your parent.** Each node wants
        //      the average height of whatever reached it; collisions are
        //      resolved by pushing down, then the column is slid back so it
        //      stays centred on where its parents wanted it. Ties break by
        //      relation, so a file's contained symbols read as one tight block
        //      distinct from the things it calls, and then by the canonical
        //      type order.
        //
        // The whole cascade is computed once, up front, for every hop — the
        // reveal is temporal, never geometric. Nodes do not shuffle as the walk
        // advances; they arrive at a place that was already theirs.

        const WALK_COL_GAP = 320;    // clear space between one hop band and the next
        const WALK_LANE_GAP = 108;   // between the sub-lanes of one wide band
        const WALK_ROW_GAP = 78;     // between two ordinary nodes in a column
        // Containment is the one relation where the child is *part of* the
        // parent rather than something it reached, so contained siblings are
        // stacked as a compact bracket instead of a fan — a shape you can pick
        // out of a column without reading a single label.
        const WALK_ROW_TIGHT = 0.6;
        // How tall a single lane may get before the band wraps into another
        // one. Deliberately short: a hop of two hundred nodes stacked in one
        // line is a mile-high hairline you have to scroll, and the thing that
        // makes a cascade readable is that it is *wider* than it is tall. A
        // wide hop therefore grows sideways, into a block of lanes, and the
        // band it occupies grows with it — see the x cursor in placeWalkColumn.
        const WALK_COL_MAX_H = 900;
        const WALK_ROW_MIN = 0.5;    // how far row spacing may be squeezed first
        const WALK_ROW_PAD = 8;      // clear space between two node discs
        const WALK_LAYOUT_KEY = 'ug-walk-layout';
        const WALK_RESTORE_MS = 950; // the morph back to the graph's own layout

        // Vertical banding inside a column. Structure first, then type
        // hierarchy, then behaviour, then data, then module wiring — the same
        // order in every column, so the bands read as horizontal stripes
        // running the length of the cascade.
        const WALK_REL_BAND = {
            Contains: 0,
            Extends: 1, Implements: 1, Overrides: 1,
            Calls: 2,
            Uses: 3, References: 3,
            Imports: 4, Exports: 4,
            Requires: 5, DependsOn: 5,
        };
        const walkRelBand = (rel) => {
            const b = WALK_REL_BAND[rel];
            return b === undefined ? 6 : b;
        };

        function walkCascadeOn() { return state.walkLayout === 'flow'; }

        // Guides are revealed with their hop, not all at once — the cascade
        // should still be a reveal, not a diagram that was there all along.
        function walkLaneRevealed(lane) {
            return lane.hop <= Math.max(walkPlay.index, walkPlay.streaming);
        }

        // Build the cascade for a whole computed walk.
        // Returns { pos, lanes } — positions by node id, and one record per
        // hop column for the on-canvas guides.
        function computeWalkCascade(layers, seedId) {
            const pos = new Map();
            const laneBy = new Map();
            if (!layers || !layers.length) return { pos, lanes: [] };

            // Which layer each reached node landed in. Every edge a layer
            // carries joins that layer to the one before it (computeWalk only
            // records an edge when it reaches a node for the first time), so
            // this is enough to orient every edge.
            const hopOf = new Map([[seedId, 0]]);
            layers.forEach(l => l.ids.forEach(id => hopOf.set(id, l.hop)));

            // id → the edges that reached it, each with the end it came from,
            // its relation, and whether it was followed forward (+1, the parent
            // points at this node) or backward (−1, this node points at the
            // parent).
            const inbound = new Map();
            for (let h = 1; h < layers.length; h++) {
                for (const e of layers[h].edges) {
                    const sh = hopOf.get(e.source);
                    const th = hopOf.get(e.target);
                    let rec = null;
                    if (th === h && sh === h - 1) rec = { child: e.target, parent: e.source, rel: e.rel, sign: 1 };
                    else if (sh === h && th === h - 1) rec = { child: e.source, parent: e.target, rel: e.rel, sign: -1 };
                    if (!rec) continue;
                    const list = inbound.get(rec.child);
                    if (list) list.push(rec); else inbound.set(rec.child, [rec]);
                }
            }

            // Which wing a node belongs to. The first hop picks its side from
            // the direction its edge was followed; everything past that
            // inherits from its parent, so a wing never doubles back on itself
            // and "further from the seed" always means "further out".
            const wing = new Map([[seedId, 0]]);
            for (let h = 1; h < layers.length; h++) {
                for (const id of layers[h].ids) {
                    let w = 0;
                    for (const r of (inbound.get(id) || [])) {
                        const pw = wing.get(r.parent);
                        w = pw ? pw : r.sign;
                        if (w) break;
                    }
                    wing.set(id, w || 1);
                }
            }

            pos.set(seedId, { x: 0, y: 0, z: 0 });
            laneBy.set('0:0', {
                hop: 0, sign: 0, x: 0, x0: 0, x1: 0, top: 0, bottom: 0, count: 1,
                label: 'SEED', color: walkColorForHop(0),
            });

            // How far out each wing has been built. Hop bands are laid down
            // end to end rather than at fixed multiples of a gap, because a
            // band is as wide as its fan-out needs: pinning hop 3 to 3×gap
            // would drop it on top of a hop 2 that had spread into six lanes.
            const cursor = { '-1': 0, '1': 0 };
            for (let h = 1; h < layers.length; h++) {
                for (const sign of [-1, 1]) {
                    const ids = layers[h].ids.filter(id => wing.get(id) === sign);
                    if (!ids.length) continue;
                    placeWalkColumn(ids, h, sign, inbound, pos, laneBy, cursor);
                }
            }

            const lanes = [...laneBy.values()].sort((a, b) => a.x - b.x);
            anchorWalkCascade(pos, lanes, seedId);
            return { pos, lanes };
        }

        // Move the cascade from the origin, where it was convenient to build
        // it, to wherever the mounted backend can actually draw it.
        //
        //   • a backend with a bounded space (2D) gets it centred in that
        //     space, scaled down if it does not fit — coordinates outside
        //     cosmos.gl's `spaceSize` are the documented iOS crash, and they
        //     also throw off its own screen projection
        //   • a backend without one (3D) gets it centred on the seed's current
        //     position, so the diagram unfolds where the seed already was
        //     rather than teleporting the walk to the origin
        //
        // The lane records carry the same transform: they describe the same
        // geometry and are drawn in the same coordinates.
        function anchorWalkCascade(pos, lanes, seedId) {
            const space = rendererSpace();
            let ox = 0, oy = 0, oz = 0, k = 1;
            if (space) {
                let mnx = Infinity, mxx = -Infinity, mny = Infinity, mxy = -Infinity;
                pos.forEach(p => {
                    if (p.x < mnx) mnx = p.x;
                    if (p.x > mxx) mxx = p.x;
                    if (p.y < mny) mny = p.y;
                    if (p.y > mxy) mxy = p.y;
                });
                if (!Number.isFinite(mnx)) return;
                const extent = Math.max(mxx - mnx, mxy - mny, 1);
                // Floored rather than unbounded: shrinking far enough to fit a
                // truly enormous walk would close the gaps between node discs,
                // and a cascade you cannot read the nodes of is not a saving.
                k = Math.min(1, Math.max(0.7, (space.size * 0.9) / extent));
                ox = space.cx - ((mnx + mxx) / 2) * k;
                oy = space.cy - ((mny + mxy) / 2) * k;
            } else {
                const seed = state.nodeById && state.nodeById.get(seedId);
                if (!seed || !Number.isFinite(seed.x)) return;
                ox = seed.x; oy = seed.y; oz = seed.z || 0;
            }
            if (k === 1 && !ox && !oy && !oz) return;
            pos.forEach(p => { p.x = p.x * k + ox; p.y = p.y * k + oy; p.z = p.z * k + oz; });
            lanes.forEach(l => {
                l.x = l.x * k + ox;
                l.x0 = l.x0 * k + ox;
                l.x1 = l.x1 * k + ox;
                l.top = l.top * k + oy;
                l.bottom = l.bottom * k + oy;
                l.scale = k;
            });
        }

        // One hop band of one wing.
        function placeWalkColumn(ids, hop, sign, inbound, pos, laneBy, cursor) {
            const items = ids.map(id => {
                const ins = (inbound.get(id) || []).filter(r => pos.has(r.parent));
                let want = 0;
                let band = 9;
                for (const r of ins) {
                    want += pos.get(r.parent).y;
                    band = Math.min(band, walkRelBand(r.rel));
                }
                const node = state.nodeById && state.nodeById.get(id);
                return {
                    id,
                    // The barycentre of everything that reached it — a node
                    // pulled at from two places belongs between them.
                    want: ins.length ? want / ins.length : 0,
                    parentKey: ins.length ? ins[0].parent : '',
                    band: band === 9 ? 6 : band,
                    tight: band === 0,
                    rank: node ? nodeTypeRank(node.group) : 99,
                    name: node ? node.name : id,
                };
            });

            const cmp = (a, b) =>
                a.want - b.want
                || (a.parentKey < b.parentKey ? -1 : a.parentKey > b.parentKey ? 1 : 0)
                || a.band - b.band
                || a.rank - b.rank
                || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
            items.sort(cmp);

            // Squeeze the row spacing before wrapping — a column of twenty is
            // still one readable run at tighter spacing, whereas splitting it
            // costs an edge crossing for every node in the second lane.
            let natural = 0;
            items.forEach(it => { natural += WALK_ROW_GAP * (it.tight ? WALK_ROW_TIGHT : 1); });
            const squeeze = natural > WALK_COL_MAX_H
                ? Math.max(WALK_ROW_MIN, WALK_COL_MAX_H / natural)
                : 1;
            const gap = WALK_ROW_GAP * squeeze;
            // A slot never closes below the discs it has to hold. Squeezing is
            // a way to fit more of a column on screen; it is not a licence to
            // overlap nodes, which is the one thing that would make the
            // arrangement less readable than the layout it replaced.
            let used = 0;
            items.forEach(it => {
                const node = state.nodeById && state.nodeById.get(it.id);
                const floor = (node ? nodeRadiusFor(node) : 9) * 2 + WALK_ROW_PAD;
                it.h = Math.max(gap * (it.tight ? WALK_ROW_TIGHT : 1), floor);
                used += it.h;
            });

            const laneCount = Math.max(1, Math.ceil(used / WALK_COL_MAX_H));
            const perLane = Math.ceil(items.length / laneCount);
            // The band starts a clear gap past wherever the previous hop
            // finished, and the cursor moves to its last lane.
            const base = cursor[sign] + sign * WALK_COL_GAP;
            let top = -Infinity, bottom = Infinity, placed = 0, lastX = base;
            for (let l = 0; l < laneCount; l++) {
                const slice = items.slice(l * perLane, (l + 1) * perLane);
                if (!slice.length) continue;
                lastX = base + sign * l * WALK_LANE_GAP;
                const span = layWalkColumn(slice, lastX, pos);
                top = Math.max(top, span.top);
                bottom = Math.min(bottom, span.bottom);
                placed += slice.length;
            }
            cursor[sign] = lastX;
            laneBy.set(sign + ':' + hop, {
                hop, sign,
                x: (base + lastX) / 2,
                x0: Math.min(base, lastX),
                x1: Math.max(base, lastX),
                top, bottom, count: placed,
                label: 'HOP ' + hop,
                color: walkColorForHop(hop),
            });
        }

        // Place one lane's worth of nodes on a vertical line.
        //
        // Each node is put at the height its parents want; where that would
        // overlap the node above, it is pushed down just far enough. Pushing
        // only ever moves things one way, so the run as a whole drifts — and
        // then the whole run is slid back by the average drift, which keeps the
        // column centred on its parents instead of hanging off the bottom of
        // them. (The priority method from layered graph drawing, in about
        // fifteen lines: it is not optimal, and for a fan-out from one parent
        // it is exactly optimal, which is the case that matters here.)
        function layWalkColumn(slice, x, pos) {
            const ys = [];
            let cursor = -Infinity;
            for (let i = 0; i < slice.length; i++) {
                const need = i === 0 ? 0 : (slice[i - 1].h + slice[i].h) / 2;
                const y = Math.max(slice[i].want, cursor + need);
                ys.push(y);
                cursor = y;
            }
            let drift = 0;
            for (let i = 0; i < slice.length; i++) drift += ys[i] - slice[i].want;
            drift /= slice.length;
            for (let i = 0; i < slice.length; i++) {
                pos.set(slice[i].id, { x, y: ys[i] - drift, z: 0 });
            }
            return { top: ys[ys.length - 1] - drift, bottom: ys[0] - drift };
        }

        // ── Putting the cascade on the canvas, and taking it off ──

        // Snapshot where the graph's own layout had these nodes, so exiting the
        // walk can hand them all back. Only ever fills gaps: in solo mode the
        // view grows as the walk advances, and a node's *pre-walk* position is
        // the one from the first time we saw it, not from the last.
        function rememberWalkPositions(ids) {
            if (!state.walkPosSaved) state.walkPosSaved = new Map();
            ids.forEach(id => {
                if (state.walkPosSaved.has(id)) return;
                const n = state.nodeById && state.nodeById.get(id);
                if (n && Number.isFinite(n.x)) {
                    state.walkPosSaved.set(id, { x: n.x, y: n.y, z: n.z || 0 });
                }
            });
        }

        // Struck instantly rather than morphed: at the moment a walk starts the
        // canvas has already isolated to the seed, so there is nothing on
        // screen to watch move. Animating an arrangement of invisible nodes
        // costs a second of the user's attention and buys nothing.
        function applyWalkCascade(ms) {
            if (!walkCascadeOn() || !state.walkCascadePos || !nodePositionsSupported()) return;
            setNodePositions(state.walkCascadePos, ms || 0, {});
        }

        // The reverse, and this one *is* worth animating: everything is visible
        // again, so the cascade visibly relaxes back into the graph it was cut
        // out of — which is the one moment that shows you where the walk was.
        function restoreWalkPositions() {
            const saved = state.walkPosSaved;
            state.walkPosSaved = null;
            state.walkCascadePos = null;
            state.walkLanes = [];
            if (!saved || !saved.size || !nodePositionsSupported()) return;
            setNodePositions(saved, WALK_RESTORE_MS, { release: true });
        }

        // How a hop announces itself.
        //
        // Off the cascade the frontier really is a shell — hop 2 is everything
        // at distance 2, in every direction — so a sphere expanding from the
        // seed touches all of it at once, and that is the effect.
        //
        // On the cascade the frontier is a *line*: one column, off to one side.
        // A sphere grown from the seed until it reached that column would have
        // engulfed hop 1, the near edge of hop 3 and most of the canvas on the
        // way, describing a geometry the diagram no longer has. So the wave
        // becomes a curtain travelling along the flow axis — from the column it
        // is leaving to the column about to light up, arriving on the beat. A
        // `both` walk gets one per wing, and watching the two set off in
        // opposite directions is the clearest statement the walk makes about
        // what "inbound" and "outbound" mean.
        function emitWalkBurst(h, colour, ms) {
            const seed = walkPlay.seedNode;
            const lanes = state.walkLanes || [];
            if (!walkCascadeOn() || !state.walkCascadePos || !lanes.length) {
                const fromR = h === 0 ? 4 : layerReachRadius(seed, h - 1);
                const toR = h === 0 ? 44 : layerReachRadius(seed, h);
                emitWalkPulse(seed, colour, fromR, toR, ms);
                return;
            }
            const seedPos = state.walkCascadePos.get(state.walkSeed);
            // Hop 0 is one node, not a column: there is nothing for a front to
            // travel between, and a compact flash is what "this is where it
            // starts" looks like.
            if (h === 0 || !seedPos) {
                emitWalkPulse(seedPos ? { ...seedPos } : seed, colour, 4, 44, ms);
                return;
            }

            const here = lanes.filter(l => l.hop === h);
            if (!here.length) { emitWalkPulse({ ...seedPos }, colour, 4, 44, ms); return; }

            // The curtain spans everything revealed so far, so it reads as a
            // front crossing the whole diagram rather than a bar the height of
            // one column.
            let top = -Infinity, bottom = Infinity;
            for (const l of lanes) {
                if (l.hop > h) continue;
                top = Math.max(top, l.top);
                bottom = Math.min(bottom, l.bottom);
            }
            const pad = Math.max((top - bottom) * 0.06, 50);
            const k = (lanes[0] && lanes[0].scale) || 1;

            let swept = false;
            for (const lane of here) {
                const prev = lanes.find(p => p.hop === h - 1 && (p.sign === lane.sign || p.sign === 0));
                const fromX = prev ? (prev.sign === 0 ? prev.x : prev.x1) : seedPos.x;
                const dir = lane.x1 >= fromX ? 1 : -1;
                // Overshoot the column a little: the front should wash *over*
                // the nodes as they ignite, not stop short at the first lane.
                const toX = lane.x1 + dir * WALK_COL_GAP * 0.3 * k;
                swept = emitWalkSweep({
                    colour,
                    fromX, toX,
                    top: top + pad,
                    bottom: bottom - pad,
                    z: seedPos.z || 0,
                    growMs: ms,
                }) || swept;
            }
            // A backend with no sweep still has to mark the beat.
            if (!swept) emitWalkPulse({ ...seedPos }, colour, 4, 44, ms);
        }

        // ── Playback state ───────────────────────────────────
        // Render-relevant sets live on `state` (read by the graph accessors);
        // this object holds the control state the overlay drives.
        const walkPlay = {
            active: false, playing: false, index: -1, streaming: -1,
            layers: null, totalEdges: 0, seedNode: null,
            token: 0, phaseTimer: null, stepTimer: null, restore: null,
        };

        function cancelWalkTimers() {
            walkPlay.token++;   // any pending scheduled callback now bails
            if (walkPlay.phaseTimer) { clearTimeout(walkPlay.phaseTimer); walkPlay.phaseTimer = null; }
            if (walkPlay.stepTimer) { clearTimeout(walkPlay.stepTimer); walkPlay.stepTimer = null; }
            walkPlay.streaming = -1;
        }

        function wireWalk() {
            const root = document.getElementById('pane-walk');
            if (!root) return;

            // Seed input: live filter over the loaded graph.
            const input = document.getElementById('walk-seed-input');
            const sugBox = document.getElementById('walk-seed-suggestions');
            let sugIndex = -1;

            // Same shared search the sidebar box uses — see `searchNodes` in
            // 02-dialogs.js. Async, with a token, because in server mode the
            // ranking happens on the server; debounced because in that mode
            // every keystroke is a round-trip. Focus is a single event and
            // stays immediate.
            let seedToken = 0;
            const refreshSeedSuggestions = async () => {
                const q = input.value.trim().toLowerCase();
                sugIndex = -1;
                if (!q) { sugBox.classList.remove('open'); sugBox.innerHTML = ''; return; }
                const filterActive = state.nodeFilters.size > 0;
                const token = ++seedToken;
                const found = await searchNodes(q, {
                    limit: 8,
                    types: filterActive ? state.nodeFilters : null,
                });
                if (token !== seedToken) return;
                const hits = found.nodes;
                sugBox.innerHTML = '';
                if (!hits.length) {
                    sugBox.innerHTML = '<div class="walk-seed-sug" style="cursor:default;color:var(--text-dim)">No matching nodes</div>';
                    sugBox.classList.add('open');
                    return;
                }
                hits.forEach((n, i) => {
                    const row = document.createElement('div');
                    row.className = 'walk-seed-sug';
                    row.dataset.index = i;
                    const meta = [n.group, n.file].filter(Boolean).join(' · ');
                    row.innerHTML = nodeIconSvg(n.group)
                        + `<span class="nm">${escapeHtml(truncateName(n.name))}</span>`
                        + `<span class="mt">${escapeHtml(meta)}</span>`;
                    row.addEventListener('mousedown', e => { e.preventDefault(); selectWalkSeed(n); });
                    sugBox.appendChild(row);
                });
                sugBox.classList.add('open');
            };
            const updateSugHighlight = () => {
                sugBox.querySelectorAll('.walk-seed-sug').forEach((it, i) =>
                    it.classList.toggle('active', i === sugIndex));
            };

            const refreshSeedDebounced = debounceTrailing(refreshSeedSuggestions, SEARCH_DEBOUNCE_MS);
            input.addEventListener('input', refreshSeedDebounced);
            input.addEventListener('focus', refreshSeedSuggestions);
            input.addEventListener('blur', () => setTimeout(() => sugBox.classList.remove('open'), 140));
            input.addEventListener('keydown', e => {
                const items = sugBox.querySelectorAll('.walk-seed-sug[data-index]');
                if (e.key === 'ArrowDown') { e.preventDefault(); sugIndex = Math.min(sugIndex + 1, items.length - 1); updateSugHighlight(); }
                else if (e.key === 'ArrowUp') { e.preventDefault(); sugIndex = Math.max(sugIndex - 1, 0); updateSugHighlight(); }
                else if (e.key === 'Enter') {
                    e.preventDefault();
                    // Pick what is in the box now — a pending debounced
                    // refresh would otherwise race the pick with a
                    // stale-prefix list. The mousedown dispatch below is
                    // async-safe: it runs only once the (flushed) list has
                    // the row.
                    refreshSeedDebounced.flush();
                    const pick = items[sugIndex] || items[0];
                    if (pick) pick.dispatchEvent(new Event('mousedown'));
                }
                else if (e.key === 'Escape') { sugBox.classList.remove('open'); input.blur(); }
            });

            // Hops / direction (sidebar launcher controls).
            root.querySelectorAll('.walk-hop-btn').forEach(btn => btn.addEventListener('click', () => {
                if (state.walkRunning) return;
                state.walkHops = parseInt(btn.dataset.hops, 10);
                root.querySelectorAll('.walk-hop-btn').forEach(b => b.classList.toggle('active', b === btn));
            }));
            root.querySelectorAll('.walk-dir-btn').forEach(btn => btn.addEventListener('click', () => {
                if (state.walkRunning) return;
                state.walkDir = btn.dataset.dir;
                root.querySelectorAll('.walk-dir-btn').forEach(b => b.classList.toggle('active', b === btn));
            }));
            // Sidebar speed buttons set the default for the next walk.
            root.querySelectorAll('.walk-speed-btn').forEach(btn => btn.addEventListener('click', () => {
                setWalkSpeed(parseFloat(btn.dataset.speed) || 1);
            }));
            // Arrangement. Remembered across sessions: whether you want a walk
            // rearranged into a diagram or lit up in place is a reading
            // preference, not something to re-decide every time.
            try {
                const saved = localStorage.getItem(WALK_LAYOUT_KEY);
                if (saved === 'flow' || saved === 'graph') state.walkLayout = saved;
            } catch (e) { /* private mode */ }
            root.querySelectorAll('.walk-arr-btn').forEach(btn => btn.addEventListener('click', () => {
                setWalkLayout(btn.dataset.arr);
            }));
            syncWalkLayoutButtons();

            renderWalkEdgeTypes();

            // Launcher + history actions.
            document.getElementById('walk-run').addEventListener('click', runWalk);
            const clearHist = document.getElementById('walk-history-clear');
            if (clearHist) clearHist.addEventListener('click', () => { clearWalkHistory(); });

            // Overlay transport.
            const bind = (id, fn) => { const el = walkEl(id); if (el) el.addEventListener('click', fn); };
            bind('walk-o-prev', prevHop);
            bind('walk-o-next', nextHop);
            bind('walk-o-play', togglePlayWalk);
            bind('walk-o-exit', () => exitWalk());
            bind('walk-o-close', () => exitWalk());
            bind('walk-o-speed', () => cycleWalkSpeed());
            bind('walk-o-flow', toggleWalkFlow);
            bind('walk-o-layout', toggleWalkLayout);
            bind('walk-o-info', toggleWalkInfo);
            bind('walk-o-labels', toggleShowLabels);

            wireWalkDrag();

            // Keyboard transport — only when a walk is live and the user
            // isn't typing in an input.
            document.addEventListener('keydown', e => {
                if (!walkPlay.active) return;
                const t = e.target;
                if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
                if (e.key === 'Escape') { exitWalk(); return; }
                if (e.key === 'ArrowLeft') { e.preventDefault(); prevHop(); }
                else if (e.key === 'ArrowRight') { e.preventDefault(); nextHop(); }
                else if (e.key === ' ' || e.key === 'Spacebar') { e.preventDefault(); togglePlayWalk(); }
                else if (e.key === 's' || e.key === 'S') { e.preventDefault(); cycleWalkSpeed(); }
                else if (e.key === 'r' || e.key === 'R') { e.preventDefault(); toggleWalkFlow(); }
                else if (e.key === 'f' || e.key === 'F') { e.preventDefault(); toggleWalkLayout(); }
                else if (e.key === 'd' || e.key === 'D') { e.preventDefault(); toggleWalkInfo(); }
                else if (e.key === 'l' || e.key === 'L') { e.preventDefault(); toggleShowLabels(); }
                else if (e.key === 'n' || e.key === 'N') { e.preventDefault(); toggleWalkNodes(); }
            });

            // Seed follows the current selection (see syncWalkSeed, wired
            // into handleClick). At init, adopt whatever is already selected.
            if (state.selectedNode) syncWalkSeed(state.selectedNode);
            renderWalkHistory();
        }

        function setWalkSpeed(s) {
            state.walkSpeed = s;
            const ovl = walkEl('walk-o-speed');
            if (ovl) ovl.textContent = s + '×';
            document.querySelectorAll('.walk-speed-btn').forEach(b =>
                b.classList.toggle('active', parseFloat(b.dataset.speed) === s));
        }
        function cycleWalkSpeed() {
            const idx = WALK_SPEEDS.indexOf(state.walkSpeed);
            setWalkSpeed(WALK_SPEEDS[(idx + 1) % WALK_SPEEDS.length]);
        }

        function renderWalkEdgeTypes() {
            const box = document.getElementById('walk-edge-types');
            // Off `state.edgeTypeCounts`, which both loaders populate — counted
            // from the edge list locally, read off the index in server mode
            // where there is no local edge list to count.
            const counts = new Map(Object.entries(state.edgeTypeCounts || {}));
            if (!box || !counts.size) return;
            const types = [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([t]) => t);
            box.innerHTML = '';
            const all = document.createElement('button');
            all.type = 'button';
            all.className = 'walk-et active';
            all.textContent = 'all edges';
            all.addEventListener('click', () => {
                state.walkEdgeTypes = null;
                box.querySelectorAll('.walk-et').forEach(b => b.classList.remove('active'));
                all.classList.add('active');
            });
            box.appendChild(all);
            types.forEach(t => {
                const chip = document.createElement('button');
                chip.type = 'button';
                chip.className = 'walk-et';
                chip.textContent = t;
                chip.title = `${counts.get(t)} edge${counts.get(t) === 1 ? '' : 's'} of this type`;
                chip.addEventListener('click', () => {
                    if (!state.walkEdgeTypes) state.walkEdgeTypes = new Set();
                    if (state.walkEdgeTypes.has(t)) { state.walkEdgeTypes.delete(t); chip.classList.remove('active'); }
                    else { state.walkEdgeTypes.add(t); chip.classList.add('active'); }
                    const anyExplicit = state.walkEdgeTypes.size > 0;
                    all.classList.toggle('active', !anyExplicit);
                    if (!anyExplicit) state.walkEdgeTypes = null;
                });
                box.appendChild(chip);
            });
        }

        function selectWalkSeed(n) {
            if (!n) return;
            state.walkSeed = n.id;
            const input = document.getElementById('walk-seed-input');
            const sugBox = document.getElementById('walk-seed-suggestions');
            if (input) { input.value = ''; input.placeholder = 'Seed selected — change it any time'; }
            if (sugBox) sugBox.classList.remove('open');
            renderWalkSeedChip(n);
            const status = document.getElementById('walk-status');
            if (status) { status.classList.remove('error'); status.textContent = `Seed: ${n.id}`; }
        }

        function renderWalkSeedChip(n) {
            const chipHost = document.getElementById('walk-seed-chip');
            if (!chipHost) return;
            chipHost.innerHTML = `<span class="walk-seed-chip">`
                + nodeIconSvg(n.group)
                + `<span class="nm" title="${escapeHtml(n.id)}">${escapeHtml(truncateName(n.name))}</span>`
                + `<button type="button" class="x" title="Clear seed" aria-label="Clear seed">✕</button>`
                + `</span>`;
            chipHost.querySelector('.x').addEventListener('click', clearWalkSeed);
        }

        // Called from handleClick on every node selection. Mirrors the
        // selection into the Walk pane's seed so opening Walk always starts
        // from the node the user was just looking at. Skipped while a walk is
        // running and does NOT switch tabs or touch the seed input's typing.
        function syncWalkSeed(node) {
            if (!node || state.walkRunning) return;
            state.walkSeed = node.id;
            renderWalkSeedChip(node);
            if (state.discoverSub === 'walk' && !state.walkActive) {
                const status = document.getElementById('walk-status');
                if (status) { status.classList.remove('error'); status.textContent = `Seed: ${node.id}`; }
            }
        }

        function clearWalkSeed() {
            state.walkSeed = null;
            const input = document.getElementById('walk-seed-input');
            const chipHost = document.getElementById('walk-seed-chip');
            if (input) input.placeholder = 'Type a function, class, file…';
            if (chipHost) chipHost.innerHTML = '';
            if (input) input.focus();
        }

        // ── The walk itself (BFS over the loaded graph) ──────
        // Same semantics as the `traverse` MCP tool and /api/db/traverse:
        // respects direction and an optional edge-type set, returns one
        // layer per hop so the player can reveal them sequentially.
        // Deliberately *not* a second BFS on the server.
        //
        // The plan called for a `POST /api/graph/walk` mirroring this loop, to
        // cap a hub blow-up. But this is a pure traversal over `edgesOf`, and
        // its frontier at each hop is exactly the batch of nodes whose edges
        // are needed next — so one `await ensureEdges(frontier)` per hop makes
        // it work in server mode with the endpoint that already exists, at one
        // request per hop. Reimplementing it in Rust would mean two copies of
        // the layer/tally/edge-key semantics the player animation depends on,
        // which is the drift AGENTS.md §3a is about. The blow-up is bounded
        // instead by WALK_MAX_FRONTIER below.
        //
        // Async in both modes; `ensureEdges` resolves immediately when every
        // edge is already local.
        const WALK_MAX_FRONTIER = 4000;
        async function computeWalk(seedId, maxHops, dir, edgeTypes) {
            const layers = [{ hop: 0, ids: [seedId], edges: [], tally: {} }];
            const dist = new Map([[seedId, 0]]);
            const reached = new Set([seedId]);
            const seenEdgeKeys = new Set();
            let frontier = [seedId];
            const allow = edgeTypes && edgeTypes.size ? edgeTypes : null;

            for (let h = 1; h <= maxHops; h++) {
                const next = [];
                const edges = [];
                const tally = {};
                const seenThisLayer = new Set();
                // The whole frontier's edges in one request, before the layer
                // is walked. Per node would be one request each — thousands of
                // them on a wide hop.
                await ensureEdges(frontier);
                for (const cur of frontier) {
                    for (const e of edgesOf(cur)) {
                        if (allow && !allow.has(e.rel)) continue;
                        const s = e.source.id || e.source;
                        const t = e.target.id || e.target;
                        let neighbour = null;
                        if (dir === 'outbound' && s === cur) neighbour = t;
                        else if (dir === 'inbound' && t === cur) neighbour = s;
                        else if (dir === 'both') neighbour = s === cur ? t : (t === cur ? s : null);
                        if (neighbour == null || neighbour === cur) continue;
                        if (reached.has(neighbour)) continue;
                        const ek = walkEdgeKey(s, t);
                        if (!seenEdgeKeys.has(ek)) {
                            seenEdgeKeys.add(ek);
                            edges.push({ source: s, target: t, rel: e.rel });
                            const r = e.rel || 'other';
                            tally[r] = (tally[r] || 0) + 1;
                        }
                        if (!seenThisLayer.has(neighbour)) { seenThisLayer.add(neighbour); next.push(neighbour); }
                    }
                }
                if (!next.length) break;
                next.forEach(id => { reached.add(id); dist.set(id, h); });
                // A hub three hops out reaches tens of thousands. The layers
                // are already recorded; stopping here bounds the *next* fetch
                // rather than truncating what has been found, and the walk
                // reports the hop it stopped at.
                if (next.length > WALK_MAX_FRONTIER) {
                    layers.push({ hop: h, ids: next, edges, tally });
                    return { layers, dist, reached, stoppedAtHop: h };
                }
                layers.push({ hop: h, ids: next, edges, tally });
                frontier = next;
            }
            return { layers, dist, reached };
        }

        // Launch from the sidebar: validate, compute, record, play.
        async function runWalk() {
            const status = document.getElementById('walk-status');
            const seedId = state.walkSeed;
            if (!seedId) {
                if (status) { status.classList.add('error'); status.textContent = 'Pick a seed node first.'; }
                return;
            }
            const seedNode = state.nodeById && state.nodeById.get(seedId);
            if (!seedNode) {
                if (status) { status.classList.add('error'); status.textContent = 'Seed node not in loaded graph.'; }
                return;
            }
            exitWalk(true);
            const { layers, reached } = await computeWalk(seedId, state.walkHops, state.walkDir, state.walkEdgeTypes);
            const totalEdges = layers.reduce((a, l) => a + l.edges.length, 0);
            if (layers.length <= 1) {
                if (status) {
                    status.classList.remove('error');
                    const dirHint = state.walkDir === 'inbound' ? 'inbound references' : `${state.walkDir} edges`;
                    status.textContent = `No ${dirHint} from this node — try a different direction.`;
                }
                return;
            }
            recordWalkInHistory({
                seedId, seedName: seedNode.name, seedGroup: seedNode.group,
                hops: state.walkHops, dir: state.walkDir,
                edgeTypes: state.walkEdgeTypes ? [...state.walkEdgeTypes] : null,
                layers, totalEdges, nodeCount: reached.size,
            });
            playWalk(seedNode, layers, totalEdges);
        }

        // ── Playback state machine ──────────────────────────
        //
        // The walk is a media-player: auto-play with a play/pause, plus prev
        // / next to step hop by hop. Forward steps are the two-phase animated
        // beat; backward steps rewind instantly. A token invalidates stale
        // scheduled callbacks whenever the user intervenes or the walk exits.

        // Everything here is one transaction as far as the canvas is
        // concerned — the seed selection, the cascade, and the walk's own
        // first paint all land on the same frame — so the restyles they would
        // each do are held and issued once. See `coalesceRestyles`.
        function playWalk(seedNode, layers, totalEdges) {
            coalesceRestyles(() => playWalkInner(seedNode, layers, totalEdges));
        }

        function playWalkInner(seedNode, layers, totalEdges) {
            cancelWalkTimers();
            walkPlay.layers = layers;
            walkPlay.totalEdges = totalEdges;
            walkPlay.seedNode = seedNode;
            walkPlay.index = -1;
            walkPlay.streaming = -1;

            state.walkActive = true;
            state.walkRunning = true;
            state.walkSeed = seedNode.id;
            state.selectedNode = seedNode;
            document.body.classList.add('walk-active');

            // The cascade is computed for the whole walk before the first hop
            // is drawn — see computeWalkCascade. It is applied here, while
            // everything but the seed is still hidden, so the arrangement is
            // simply the one the walk was always in.
            state.walkPosSaved = null;
            state.walkCascadePos = null;
            state.walkLanes = [];
            if (walkCascadeOn() && nodePositionsSupported()) {
                const reachedIds = [];
                layers.forEach(l => l.ids.forEach(id => reachedIds.push(id)));
                rememberWalkPositions(reachedIds);
                const flow = computeWalkCascade(layers, seedNode.id);
                state.walkCascadePos = flow.pos;
                state.walkLanes = flow.lanes;
                applyWalkCascade(0);
            }

            enterWalkImmersive();
            // Pre-fill the details panel with the seed now, so the info
            // button on the card has content to toggle. Suppress history
            // (a walk start isn't a navigation) and focus re-anchoring (the
            // walk owns the camera); the immersive entry then hides it again.
            state.suppressHistory = true;
            state.suppressFocusReanchor = true;
            handleClick(null, seedNode);
            state.suppressFocusReanchor = false;
            state.suppressHistory = false;
            const walkInfoEl = document.getElementById('info');
            if (walkInfoEl) walkInfoEl.classList.remove('visible');
            buildWalkSegments(layers.length - 1);
            restoreWalkPosition();
            showWalkOverlay();

            // Hop 0 — the seed, ignited immediately. A small establishing
            // pulse marks the origin the rest of the walk radiates from.
            setWalkStateToHop(0);
            walkPlay.index = 0;
            emitWalkBurst(0, walkColorForHop(0), 320);
            setOverlayPhase('ignite', 0);
            updateWalkOverlay();
            setWalkPlaying(true);
            // Kick off auto-play onto hop 1.
            scheduleAutoAdvance();
        }

        // Rebuild render state to "everything up to hop `target` ignited",
        // with nothing beyond. Used for hop 0 setup, rewinds, and jumps.
        function setWalkStateToHop(target) {
            const layers = walkPlay.layers;
            // Before the restyle, not after it. The hop counter is what the
            // canvas reads to decide how much of the walk has been revealed
            // (walkLaneRevealed), so leaving it to the caller to set once this
            // returns left the guides showing the *previous* hop until some
            // unrelated restyle happened along.
            walkPlay.index = target;
            walkPlay.streaming = -1;
            const reached = new Set([state.walkSeed]);
            const colors = new Map([[state.walkSeed, walkColorForHop(0)]]);
            const edgeKeys = new Set();
            for (let h = 1; h <= target; h++) {
                const colour = walkColorForHop(h);
                layers[h].ids.forEach(id => { reached.add(id); colors.set(id, colour); });
                layers[h].edges.forEach(e => edgeKeys.add(walkEdgeKey(e.source, e.target)));
            }
            state.walkReached = reached;
            state.walkColors = colors;
            state.walkEdgeKeys = edgeKeys;
            if (state.soloOnly) {
                plotNodes(Array.from(reached));
                state._didFit = true;
                state._boxSettled = true;
                // Handing the renderer a new view re-runs its own layout over
                // it, which would undo the cascade a hop at a time.
                applyWalkCascade(0);
            }
            bumpGraphStyles();
            frameWalk(reached);
        }

        // Framing during a walk. The cascade is a planar diagram, so it is
        // read straight on — the usual three-quarter framing foreshortens the
        // hop columns until they no longer line up, which is most of what the
        // arrangement was for.
        function frameWalk(ids, ms) {
            frameNodeSet(ids, ms || walkFrameMs(), { flat: walkCascadeOn() && !!state.walkCascadePos });
        }

        // Forward, animated: phase 1 streams edges toward ghost frontier
        // nodes, phase 2 (after walkIgniteMs) ignites them.
        function advanceToHop(h) {
            const layers = walkPlay.layers;
            if (!layers || h < 1 || h >= layers.length) return;
            cancelWalkTimers();
            const myTok = walkPlay.token;
            const layer = layers[h];
            const colour = walkColorForHop(h);
            walkPlay.streaming = h;

            // Phase 1 — ghosts + streaming edges, and the wavefront sets off.
            // The sphere grows from the previous frontier to this one over
            // exactly the stream window, so it arrives as the nodes ignite.
            layer.ids.forEach(id => state.walkReached.add(id));
            layer.edges.forEach(e => state.walkEdgeKeys.add(walkEdgeKey(e.source, e.target)));
            if (state.soloOnly) {
                plotNodes(Array.from(state.walkReached));
                state._didFit = true;
                state._boxSettled = true;
                applyWalkCascade(0);
            }
            bumpGraphStyles();
            // Open the frame at the *start* of the stream, over the same
            // window the wavefront takes to cross. The camera used to widen
            // only once the frontier had ignited, which was fine for a sphere
            // — you watched it swell past the edge of the view — but a front
            // travelling to a column you cannot see yet is a front travelling
            // to nowhere. Now the view opens as the front sets off, and both
            // arrive together.
            frameWalk(state.walkReached, walkIgniteMs());
            emitWalkBurst(h, colour, walkIgniteMs());
            setOverlayPhase('stream', h);
            updateWalkOverlay();

            walkPlay.phaseTimer = setTimeout(() => {
                if (walkPlay.token !== myTok) return;
                igniteHop(h, colour);
            }, walkIgniteMs());
        }

        function igniteHop(h, colour) {
            if (walkPlay.phaseTimer) { clearTimeout(walkPlay.phaseTimer); walkPlay.phaseTimer = null; }
            walkPlay.streaming = -1;
            const layer = walkPlay.layers[h];
            colour = colour || walkColorForHop(h);
            // The wavefront (started in advanceToHop) arrives here as these
            // nodes light up — no new pulse; the synchronisation is the point.
            layer.ids.forEach(id => state.walkColors.set(id, colour));
            // Ahead of the restyle: see setWalkStateToHop.
            walkPlay.index = h;
            bumpGraphStyles();
            frameWalk(state.walkReached);
            pingPipelineBox();
            setOverlayPhase('ignite', h);
            updateWalkOverlay();
            scheduleAutoAdvance();
        }

        function scheduleAutoAdvance() {
            if (walkPlay.stepTimer) { clearTimeout(walkPlay.stepTimer); walkPlay.stepTimer = null; }
            if (!walkPlay.playing) return;
            const last = walkPlay.layers.length - 1;
            if (walkPlay.index >= last) { setWalkPlaying(false); return; }
            const myTok = walkPlay.token;
            const nextH = walkPlay.index + 1;
            walkPlay.stepTimer = setTimeout(() => {
                if (walkPlay.token !== myTok) return;
                advanceToHop(nextH);
            }, walkDwellMs());
        }

        function nextHop() {
            if (!walkPlay.active) return;
            // Manual nav pauses auto-advance (matches the tour).
            setWalkPlaying(false);
            // If a stream is mid-flight, pressing next completes it now.
            if (walkPlay.streaming > 0) { igniteHop(walkPlay.streaming); return; }
            if (walkPlay.index < walkPlay.layers.length - 1) advanceToHop(walkPlay.index + 1);
        }

        function prevHop() {
            if (!walkPlay.active) return;
            setWalkPlaying(false);
            const cur = walkPlay.streaming > 0 ? walkPlay.streaming : walkPlay.index;
            if (cur <= 0) return;
            cancelWalkTimers();
            setWalkStateToHop(cur - 1);
            walkPlay.index = cur - 1;
            setOverlayPhase('ignite', walkPlay.index);
            updateWalkOverlay();
        }

        function jumpToHop(h) {
            if (!walkPlay.active) return;
            setWalkPlaying(false);
            cancelWalkTimers();
            h = Math.max(0, Math.min(h, walkPlay.layers.length - 1));
            setWalkStateToHop(h);
            walkPlay.index = h;
            setOverlayPhase('ignite', h);
            updateWalkOverlay();
        }

        function togglePlayWalk() {
            if (!walkPlay.active) return;
            if (walkPlay.playing) { setWalkPlaying(false); return; }   // pause; let current phase finish
            setWalkPlaying(true);
            if (walkPlay.streaming >= 0) return;                        // a stream is in flight — it'll carry on
            const last = walkPlay.layers.length - 1;
            if (walkPlay.index < last) { advanceToHop(walkPlay.index + 1); return; }
            // At the end — replay from the top.
            cancelWalkTimers();
            setWalkStateToHop(0);
            walkPlay.index = 0;
            updateWalkOverlay();
            advanceToHop(1);
        }

        function setWalkPlaying(p) {
            walkPlay.playing = p;
            const btn = walkEl('walk-o-play');
            const icon = walkEl('walk-o-play-icon');
            if (icon) {
                icon.innerHTML = p
                    ? '<path d="M6 5h4v14H6zM14 5h4v14h-4z"/>'        // ❚❚
                    : '<path d="M8 5.2c0-.9 1-1.4 1.7-.9l9 6.8c.6.4.6 1.4 0 1.8l-9 6.8c-.7.5-1.7 0-1.7-.9z"/>'; // ▶
            }
            if (btn) btn.setAttribute('aria-label', p ? 'Pause' : 'Play');
        }

        // Edge-flow animation on/off during a walk.
        //
        // This control used to drive auto-rotate, which only ever meant
        // anything to the 3D renderer — on a 2D plane there is nothing to
        // rotate, so the button sat there doing nothing. What a walk actually
        // has worth switching off is the motion *along the edges*: the
        // travelling strands are the whole point when you are following a
        // frontier, and a distraction when you are reading one.
        function syncWalkFlowButton() {
            const btn = walkEl('walk-o-flow');
            if (!btn) return;
            btn.classList.toggle('active', state.lineFlow);
            btn.setAttribute('aria-pressed', String(state.lineFlow));
        }
        function setWalkFlow(on) {
            state.lineFlow = on;
            syncWalkFlowButton();
            // The particle counts are read from the style rules, so the change
            // only reaches the canvas on a restyle.
            bumpGraphStyles();
        }
        function toggleWalkFlow() {
            setWalkFlow(!state.lineFlow);
        }

        // Cascade on/off, live. Switching mid-walk does not disturb the reveal
        // — the hop you are on stays the hop you are on; only the geometry
        // under it changes, and it changes by morphing so you can see which
        // node went where. That is the comparison the toggle is *for*: the
        // diagram tells you the shape of the reach, the graph's own layout
        // tells you where that reach lives in the codebase.
        function setWalkLayout(name) {
            const next = name === 'flow' ? 'flow' : 'graph';
            const changed = next !== state.walkLayout;
            state.walkLayout = next;
            try { localStorage.setItem(WALK_LAYOUT_KEY, next); } catch (e) { /* private mode */ }
            syncWalkLayoutButtons();
            if (!changed || !walkPlay.active || !walkPlay.layers || !nodePositionsSupported()) return;

            if (next === 'flow') {
                const reachedIds = [];
                walkPlay.layers.forEach(l => l.ids.forEach(id => reachedIds.push(id)));
                rememberWalkPositions(reachedIds);
                const flow = computeWalkCascade(walkPlay.layers, state.walkSeed);
                state.walkCascadePos = flow.pos;
                state.walkLanes = flow.lanes;
                setNodePositions(flow.pos, WALK_RESTORE_MS, {});
            } else {
                state.walkCascadePos = null;
                state.walkLanes = [];
                // `walkPosSaved` is kept: exitWalk still has to hand every
                // node back, and a second identical restore costs nothing.
                if (state.walkPosSaved && state.walkPosSaved.size) {
                    setNodePositions(state.walkPosSaved, WALK_RESTORE_MS, {});
                }
            }
            bumpGraphStyles();
            // After the morph, not before it. Framing reads the nodes' current
            // coordinates, and at the moment a morph starts those are still the
            // arrangement being left behind — so an immediate fit frames the
            // old shape and then watches the new one shrink inside it.
            setTimeout(() => {
                if (walkPlay.active) frameWalk(state.walkReached);
            }, WALK_RESTORE_MS);
        }

        function toggleWalkLayout() {
            setWalkLayout(walkCascadeOn() ? 'graph' : 'flow');
        }

        function syncWalkLayoutButtons() {
            const on = walkCascadeOn();
            document.querySelectorAll('.walk-arr-btn').forEach(b =>
                b.classList.toggle('active', (b.dataset.arr === 'flow') === on));
            const btn = walkEl('walk-o-layout');
            if (btn) {
                btn.classList.toggle('active', on);
                btn.setAttribute('aria-pressed', String(on));
            }
        }

        // Show/hide the (already populated) details panel while a walk is live.
        function toggleWalkInfo() {
            const info = document.getElementById('info');
            if (!info) return;
            const show = !info.classList.contains('visible');
            info.classList.toggle('visible', show);
            const btn = walkEl('walk-o-info');
            if (btn) btn.classList.toggle('active', show);
        }

        function exitWalk(quiet) {
            const wasActive = walkPlay.active;
            cancelWalkTimers();
            walkPlay.active = false;
            walkPlay.playing = false;
            walkPlay.layers = null;
            walkPlay.index = -1;
            walkPlay.seedNode = null;
            state.walkActive = false;
            state.walkRunning = false;
            const reached = state.walkReached;
            state.walkReached = new Set();
            state.walkColors = new Map();
            state.walkEdgeKeys = new Set();
            const runBtn = document.getElementById('walk-run');
            if (runBtn) runBtn.disabled = false;
            document.body.classList.remove('walk-active');
            hideWalkOverlay();
            // The list describes a walk. Without one it is a panel of rows
            // pointing at a canvas that no longer holds them.
            closeWalkNodes();
            if (wasActive) {
                exitWalkImmersive();
                // Keep the walked neighbourhood on the canvas as an ordinary
                // selection the user can keep exploring.
                if (state.soloOnly && reached.size) plotNodes(Array.from(reached));
                // …but hand the geometry back. A walk borrows the layout; it
                // does not get to keep it, or every walk would leave the graph
                // rearranged behind it.
                restoreWalkPositions();
                bumpGraphStyles();
            } else {
                state.walkPosSaved = null;
                state.walkCascadePos = null;
                state.walkLanes = [];
            }
            if (!quiet) {
                const status = document.getElementById('walk-status');
                if (status) status.textContent = '';
            }
        }

        // ── Overlay UI ───────────────────────────────────────

        function showWalkOverlay() {
            walkPlay.active = true;
            // A filter typed against the last walk's reach is a filter against
            // a set that no longer exists.
            resetWalkNodes();
            const ovl = walkEl('walk-overlay');
            if (ovl) ovl.classList.add('visible');
            const seedEl = walkEl('walk-o-seed');
            if (seedEl && walkPlay.seedNode) {
                seedEl.textContent = truncateName(walkPlay.seedNode.name);
                seedEl.title = walkPlay.seedNode.id;
            }
            buildWalkSegments(walkPlay.layers.length - 1);
            syncWalkLayoutButtons();
            // A backend without a position seam cannot hold a prescribed
            // arrangement, so the control would be a lie rather than a choice.
            const layoutBtn = walkEl('walk-o-layout');
            if (layoutBtn) layoutBtn.hidden = !nodePositionsSupported();
        }
        function hideWalkOverlay() {
            const ovl = walkEl('walk-overlay');
            if (ovl) ovl.classList.remove('visible');
        }

        // The progress track: a dot for the seed, then one segment per hop.
        //
        // The seed used to be a segment like any other, which made a
        // three-hop walk read as a four-step one and — worse — lit a whole
        // segment before the walk had gone anywhere. It is not a step the walk
        // takes; it is where the walk starts. So it gets an origin dot at the
        // head of the track and the segments are the hops, which is what the
        // counter beside them has always said.
        function buildWalkSegments(hops) {
            const bar = walkEl('walk-o-progress');
            if (!bar) return;
            bar.innerHTML = '';
            const dot = document.createElement('button');
            dot.type = 'button';
            dot.className = 'walk-seed-dot';
            dot.title = 'Back to the seed';
            dot.setAttribute('aria-label', 'Seed');
            dot.addEventListener('click', () => jumpToHop(0));
            bar.appendChild(dot);
            for (let h = 1; h <= hops; h++) {
                const seg = document.createElement('div');
                seg.className = 'walk-seg';
                seg.dataset.hop = h;
                seg.title = `Hop ${h}`;
                seg.addEventListener('click', () => jumpToHop(h));
                bar.appendChild(seg);
            }
        }

        function setOverlayPhase(phase, hop) {
            const el = walkEl('walk-o-phase');
            if (!el) return;
            el.dataset.phase = phase;
            el.textContent = phase === 'stream' ? 'streaming'
                : phase === 'ignite' ? (hop === 0 ? 'seed' : 'ignited')
                : 'done';
        }

        function updateWalkOverlay() {
            const layers = walkPlay.layers;
            if (!layers) return;
            const idx = walkPlay.index;
            const streaming = walkPlay.streaming;
            const last = layers.length - 1;
            // The hop the card is describing. While edges are in flight that
            // is the hop *arriving*, not the one behind it: the canvas has
            // already revealed its nodes as ghosts and is streaming its edges,
            // so a card still reading "Hop 1" was describing the beat before
            // the one you were watching.
            const cur = streaming > 0 ? streaming : idx;

            // Counter.
            const counter = walkEl('walk-o-counter');
            if (counter) counter.textContent = last > 0 ? `${cur}/${last}` : '';

            // The track: origin dot, then one segment per hop.
            const bar = walkEl('walk-o-progress');
            if (bar) {
                const dot = bar.querySelector('.walk-seed-dot');
                if (dot) {
                    // Lit and beating while the walk is still sitting on the
                    // seed; filled but quiet once it has set off.
                    dot.classList.toggle('active', idx === 0 && streaming < 0);
                    dot.classList.toggle('done', idx > 0 || streaming > 0);
                }
                bar.querySelectorAll('.walk-seg').forEach(seg => {
                    const h = parseInt(seg.dataset.hop, 10);
                    const inFlight = streaming === h;
                    // An ignited hop is a *full* segment. The old rule left
                    // the hop you had just arrived at on an empty track,
                    // because the fill was only painted while a segment was
                    // mid-stream.
                    const done = h <= idx;
                    seg.classList.toggle('done', done);
                    seg.classList.toggle('streaming', inFlight);
                    // Exactly one segment is ever the current step — the hop
                    // in flight if there is one, otherwise the last ignited.
                    seg.classList.toggle('active', h === cur);
                    seg.style.setProperty('--seg', done ? '100%' : (inFlight ? '55%' : '0%'));
                });
            }

            // Hop title + phase tag handled by setOverlayPhase; edges for current layer.
            const hopEl = walkEl('walk-o-hop');
            if (hopEl) {
                hopEl.textContent = cur === 0 ? 'Seed' : `Hop ${cur}`;
            }
            const edgesEl = walkEl('walk-o-edges');
            if (edgesEl) {
                const tally = cur === 0 ? {} : (layers[cur].tally || {});
                const entries = Object.entries(tally).sort((a, b) => b[1] - a[1]);
                edgesEl.innerHTML = cur === 0
                    ? `<span class="walk-o-empty">the starting node</span>`
                    : entries.length
                        ? entries.map(([r, c]) => `<span class="walk-o-edge">${escapeHtml(r)} ×${c}</span>`).join('')
                        : `<span class="walk-o-empty">no edges</span>`;
            }

            // Legend — one dot per hop that exists.
            const legend = walkEl('walk-o-legend');
            if (legend) {
                legend.innerHTML = '';
                for (let h = 0; h < layers.length; h++) {
                    const c = layers[h].ids.length;
                    const el = document.createElement('span');
                    el.className = 'walk-o-leg';
                    el.innerHTML = `<span class="d" style="background:${walkColorForHop(h)};color:${walkColorForHop(h)}"></span>`
                        + `<span>${h === 0 ? 'seed' : 'hop ' + h}</span>`
                        + `<span class="n">${c}</span>`;
                    legend.appendChild(el);
                }
            }

            // Totals for everything on the canvas, in-flight hop included —
            // its nodes are already drawn (as ghosts) and its edges are
            // already streaming, so counting them keeps the numbers in step
            // with the segment beside them.
            const totals = walkEl('walk-o-totals');
            if (totals) {
                const reachedSoFar = layers.reduce((a, l, i) => i <= cur ? a + l.ids.length : a, 0);
                const edgesSoFar = layers.reduce((a, l, i) => i <= cur ? a + l.edges.length : a, 0);
                totals.innerHTML =
                    `<span><b>${reachedSoFar}</b> nodes</span>`
                    + `<span><b>${edgesSoFar}</b> edges</span>`
                    + `<span><b>${cur}</b> of <b>${last}</b> hops</span>`;
            }

            // Transport disabled state at the ends. Keyed to the *committed*
            // hop, not `cur`: while the last hop is still streaming, Next is
            // what completes it, so it has to stay live.
            const prev = walkEl('walk-o-prev');
            const next = walkEl('walk-o-next');
            if (prev) prev.disabled = idx <= 0 && streaming < 0;
            if (next) next.disabled = idx >= last && streaming < 0;

            // The reached-nodes list marks which hops are still only planned,
            // and a hop change is precisely what moves that line. No-op when
            // the panel is closed.
            refreshWalkNodes();
        }

        // ── Immersive: collapse sidebar / details, restore on exit ──
        // Mirrors the tour's enterImmersive / exitImmersive so the canvas
        // owns the screen while a walk plays.

        function enterWalkImmersive() {
            if (walkPlay.restore) return;
            const sidebar = document.getElementById('sidebar');
            const info = document.getElementById('info');
            walkPlay.restore = {
                sidebarCollapsed: sidebar ? sidebar.classList.contains('collapsed') : null,
                infoVisible: info ? info.classList.contains('visible') : null,
                autoSpin: state.autoSpin,
                showBoundary: state.showBoundary,
            };
            if (sidebar) sidebar.classList.add('collapsed');
            if (info) info.classList.remove('visible');
            if (state.autoSpin) {
                state.autoSpin = false;
                if (typeof applyAutoSpin === 'function') applyAutoSpin();
                if (typeof syncSpinButton === 'function') syncSpinButton();
            }
            syncWalkFlowButton();
            if (state.showBoundary) {
                state.showBoundary = false;
                if (typeof applyBoundaryVisibility === 'function') applyBoundaryVisibility();
            }
            if (typeof handleNodeHover === 'function') handleNodeHover(null);
        }

        function exitWalkImmersive() {
            const r = walkPlay.restore;
            walkPlay.restore = null;
            if (!r) return;
            const sidebar = document.getElementById('sidebar');
            const info = document.getElementById('info');
            if (sidebar && r.sidebarCollapsed === false) sidebar.classList.remove('collapsed');
            if (info && r.infoVisible === false) info.classList.remove('visible');
            // Restore the pre-walk spin state exactly — a walk may have turned
            // auto-rotate on at the end, and it must not leak out of the walk.
            state.autoSpin = !!r.autoSpin;
            if (typeof applyAutoSpin === 'function') applyAutoSpin();
            if (typeof syncSpinButton === 'function') syncSpinButton();
            syncWalkFlowButton();
            if (r.showBoundary) {
                state.showBoundary = true;
                if (typeof applyBoundaryVisibility === 'function') applyBoundaryVisibility();
            }
        }

        // ── Dragging the playback card ───────────────────────

        function clampWalkPosition(left, top) {
            const overlay = walkEl('walk-overlay');
            const rect = overlay.getBoundingClientRect();
            const margin = 8;
            return {
                left: Math.min(Math.max(margin, left), Math.max(margin, window.innerWidth - rect.width - margin)),
                top: Math.min(Math.max(margin, top), Math.max(margin, window.innerHeight - rect.height - margin)),
            };
        }
        function placeWalkCard(left, top) {
            const overlay = walkEl('walk-overlay');
            if (!overlay) return;
            const p = clampWalkPosition(left, top);
            overlay.classList.add('dragged');
            overlay.style.left = p.left + 'px';
            overlay.style.top = p.top + 'px';
        }
        function restoreWalkPosition() {
            const overlay = walkEl('walk-overlay');
            if (!overlay) return;
            let saved = null;
            try { saved = JSON.parse(localStorage.getItem(WALK_POS_KEY) || 'null'); } catch (e) { saved = null; }
            if (!saved || typeof saved.left !== 'number') return;
            overlay.classList.add('dragged');
            placeWalkCard(saved.left, saved.top);
        }
        function wireWalkDrag() {
            const overlay = walkEl('walk-overlay');
            const handle = walkEl('walk-o-drag');
            if (!overlay || !handle) return;
            let dx = 0, dy = 0, moved = false;
            const onMove = (e) => { moved = true; placeWalkCard(e.clientX - dx, e.clientY - dy); };
            const onUp = () => {
                overlay.classList.remove('dragging');
                window.removeEventListener('pointermove', onMove);
                window.removeEventListener('pointerup', onUp);
                if (!moved) return;
                try {
                    localStorage.setItem(WALK_POS_KEY, JSON.stringify({
                        left: parseFloat(overlay.style.left) || 0,
                        top: parseFloat(overlay.style.top) || 0,
                    }));
                } catch (e) { /* private mode */ }
            };
            handle.addEventListener('pointerdown', (e) => {
                if (e.target.closest('button')) return;
                e.preventDefault();
                const rect = overlay.getBoundingClientRect();
                dx = e.clientX - rect.left;
                dy = e.clientY - rect.top;
                moved = false;
                placeWalkCard(rect.left, rect.top);
                overlay.classList.add('dragging');
                window.addEventListener('pointermove', onMove);
                window.addEventListener('pointerup', onUp);
            });
        }

        // ── History ──────────────────────────────────────────
        // The last few walks are kept verbatim (seed + computed layers) so a
        // walk can be replayed later without re-running the BFS. Scoped per
        // project, since node ids only mean something inside one graph.

        function walkHistoryKey() {
            const p = state.capabilities && state.capabilities.project;
            return 'ug-walk-history:' + ((p && p.name) || 'default');
        }
        function loadWalkHistory() {
            try {
                const raw = localStorage.getItem(walkHistoryKey());
                const list = raw ? JSON.parse(raw) : [];
                return Array.isArray(list) ? list : [];
            } catch (e) { return []; }
        }
        function saveWalkHistory(list) {
            try { localStorage.setItem(walkHistoryKey(), JSON.stringify(list.slice(0, WALK_HISTORY_MAX))); }
            catch (e) {
                // Layers are the bulk; if quota bites, shed them (replay then
                // falls back to recompute) before dropping entries outright.
                const lite = list.map(e => ({ ...e, layers: null }));
                try { localStorage.setItem(walkHistoryKey(), JSON.stringify(lite.slice(0, WALK_HISTORY_MAX))); }
                catch (e2) { try { localStorage.removeItem(walkHistoryKey()); } catch (e3) { /* private mode */ } }
            }
            return list.slice(0, WALK_HISTORY_MAX);
        }
        function recordWalkInHistory(rec) {
            const entry = {
                id: 'walk-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 7),
                ts: Date.now(),
                ...rec,
            };
            // Same seed + params replaces the older run rather than stacking.
            const rest = loadWalkHistory().filter(e =>
                !(e.seedId === entry.seedId && e.hops === entry.hops && e.dir === entry.dir));
            const saved = saveWalkHistory([entry, ...rest]);
            renderWalkHistory(saved);
        }
        function removeWalkFromHistory(id) {
            renderWalkHistory(saveWalkHistory(loadWalkHistory().filter(e => e.id !== id)));
        }
        function clearWalkHistory() {
            renderWalkHistory(saveWalkHistory([]));
        }
        function renderWalkHistory(list) {
            const box = walkEl('walk-history-list');
            const wrap = walkEl('walk-history');
            if (!box || !wrap) return;
            const items = list || loadWalkHistory();
            wrap.hidden = items.length === 0;
            box.innerHTML = '';
            items.forEach(entry => {
                const row = document.createElement('div');
                row.className = 'whist-row';

                const main = document.createElement('button');
                main.type = 'button';
                main.className = 'whist-main';
                main.title = `Replay walk from ${entry.seedName || entry.seedId}`;
                main.innerHTML =
                    `<span class="whist-title"></span>
                     <span class="whist-meta">
                        <span class="whist-stops">${entry.nodeCount || 0} nodes</span>
                        <span class="whist-dot">·</span>
                        <span class="whist-dir">${escapeHtml(entry.dir || 'out')}</span>
                        <span class="whist-dot">·</span>
                        <span>${entry.hops}h</span>
                        <span class="whist-dot">·</span>
                        <span>${escapeHtml(relativeTime(entry.ts))}</span>
                     </span>`;
                main.querySelector('.whist-title').textContent = entry.seedName || entry.seedId;
                main.addEventListener('click', () => replayWalkFromHistory(entry.id));

                const del = document.createElement('button');
                del.type = 'button';
                del.className = 'whist-icon danger';
                del.title = 'Remove from history';
                del.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" '
                    + 'stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18"/></svg>';
                del.addEventListener('click', () => removeWalkFromHistory(entry.id));

                row.append(main, del);
                box.appendChild(row);
            });
        }
        // Re-run a saved walk. If the stored layers survived, play them
        // verbatim; otherwise recompute from the seed (graph may have grown).
        async function replayWalkFromHistory(id) {
            const entry = loadWalkHistory().find(e => e.id === id);
            if (!entry) return;
            const seedNode = state.nodeById && state.nodeById.get(entry.seedId);
            const status = document.getElementById('walk-status');
            if (!seedNode) {
                if (status) { status.classList.add('error'); status.textContent = `“${entry.seedName || entry.seedId}” is no longer in this graph.`; }
                return;
            }
            exitWalk(true);
            applyWalkParams(entry);
            let layers = entry.layers, totalEdges = entry.totalEdges || 0;
            if (!layers || !layers.length) {
                const r = await computeWalk(entry.seedId, entry.hops, entry.dir,
                    entry.edgeTypes ? new Set(entry.edgeTypes) : null);
                layers = r.layers;
                totalEdges = layers.reduce((a, l) => a + l.edges.length, 0);
            }
            if (status) { status.classList.remove('error'); status.textContent = `Replaying walk · ${entry.nodeCount || layers.reduce((a, l) => a + l.ids.length, 0)} nodes`; }
            playWalk(seedNode, layers, totalEdges);
        }
        // Reflect a replayed walk's params back into the launcher controls.
        function applyWalkParams(entry) {
            state.walkHops = entry.hops;
            state.walkDir = entry.dir;
            state.walkEdgeTypes = entry.edgeTypes && entry.edgeTypes.length ? new Set(entry.edgeTypes) : null;
            document.querySelectorAll('.walk-hop-btn').forEach(b =>
                b.classList.toggle('active', parseInt(b.dataset.hops, 10) === entry.hops));
            document.querySelectorAll('.walk-dir-btn').forEach(b =>
                b.classList.toggle('active', b.dataset.dir === entry.dir));
            document.querySelectorAll('.walk-et').forEach(b => b.classList.remove('active'));
            // Edge-type chips can't be perfectly restored for a graph whose
            // types changed; "all" stays selected when no specifics were stored.
            if (!state.walkEdgeTypes) {
                const all = document.querySelector('.walk-et');
                if (all) all.classList.add('active');
            }
        }

        // ── Canvas effects ───────────────────────────────────

        // Farthest node of hops 0..`uptoHop` from the seed, in world units.
        // Read off the layer ids directly (not state.walkReached) so the
        // radius reflects the *intended* frontier regardless of render state.
        function layerReachRadius(seedNode, uptoHop) {
            const layers = walkPlay.layers;
            if (!layers || !seedNode) return 0;
            let max = 0;
            for (let h = 0; h <= uptoHop && h < layers.length; h++) {
                for (const id of layers[h].ids) {
                    const n = state.nodeById && state.nodeById.get(id);
                    if (!n || !Number.isFinite(n.x)) continue;
                    const dx = (n.x || 0) - (seedNode.x || 0);
                    const dy = (n.y || 0) - (seedNode.y || 0);
                    const dz = (n.z || 0) - (seedNode.z || 0);
                    const d = Math.sqrt(dx * dx + dy * dy + dz * dz);
                    if (d > max) max = d;
                }
            }
            return max;
        }
        // The wavefront burst — a composite "energy explosion" that radiates
        // from the seed to the new frontier. Timed to the two-phase beat: it
        // starts at the previous frontier's radius when the stream phase
        // opens and arrives at the new nodes exactly as they ignite.
        //
        // The burst itself is drawn by the renderer — it is a volumetric
        // effect in 3D and a flat one in 2D — so it is reached through
        // emitWalkPulse() in 10-render-core.js.

        // Pulse the pipeline diagram's GRAPH WALK box in sync with each
        // ignited frontier — ties the canvas animation to the explainer.
        function pingPipelineBox() {
            const box = document.getElementById('walk-pipe-graph');
            if (!box) return;
            box.classList.remove('pulse');
            void box.offsetWidth;
            box.classList.add('pulse');
            setTimeout(() => box.classList.remove('pulse'), walkDwellMs());
        }
