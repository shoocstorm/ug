        // ─── Graph Walk (Discover → Walk) ────────────────────
        //
        // An animated BFS frontier. Pick a seed, choose a hop radius and a
        // direction, and watch the walk light up on the canvas one hop at a
        // time — each frontier a different colour, wavefronts rippling out
        // from the seed, edges carrying flowing particles in the direction
        // of travel. This is the same N-hop walk that powers the `graph`
        // channel of hybrid search and the `traverse` MCP tool (see the
        // pipeline card rendered in the markup).
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
        const WALK_EDGE_IGNITE_MS = 440;   // phase 1 (stream) → phase 2 (ignite)
        const WALK_HOP_DWELL_MS = 1000;    // after ignition, before the next hop
        const WALK_MAX_HOPS = 4;

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

        function delay(ms) { return new Promise(r => setTimeout(r, ms)); }

        function wireWalk() {
            const root = document.getElementById('pane-walk');
            if (!root) return;

            // Seed input: live filter over the loaded graph.
            const input = document.getElementById('walk-seed-input');
            const sugBox = document.getElementById('walk-seed-suggestions');
            let sugIndex = -1;

            const refreshSeedSuggestions = () => {
                const q = input.value.trim().toLowerCase();
                sugIndex = -1;
                if (!q) { sugBox.classList.remove('open'); sugBox.innerHTML = ''; return; }
                const filterActive = state.nodeFilters.size > 0;
                const hits = state.graph.nodes
                    .filter(n => !filterActive || state.nodeFilters.has(n.group))
                    .filter(n => n.name.toLowerCase().includes(q) || n.id.toLowerCase().includes(q))
                    .sort((a, b) => {
                        const ai = a.name.toLowerCase().indexOf(q);
                        const bi = b.name.toLowerCase().indexOf(q);
                        if (ai !== bi) return ai - bi;
                        return a.name.length - b.name.length;
                    })
                    .slice(0, 8);
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

            input.addEventListener('input', refreshSeedSuggestions);
            input.addEventListener('focus', refreshSeedSuggestions);
            input.addEventListener('blur', () => setTimeout(() => sugBox.classList.remove('open'), 140));
            input.addEventListener('keydown', e => {
                const items = sugBox.querySelectorAll('.walk-seed-sug[data-index]');
                if (e.key === 'ArrowDown') {
                    e.preventDefault();
                    sugIndex = Math.min(sugIndex + 1, items.length - 1);
                    updateSugHighlight();
                } else if (e.key === 'ArrowUp') {
                    e.preventDefault();
                    sugIndex = Math.max(sugIndex - 1, 0);
                    updateSugHighlight();
                } else if (e.key === 'Enter') {
                    e.preventDefault();
                    const pick = items[sugIndex] || items[0];
                    if (pick) pick.dispatchEvent(new Event('mousedown'));
                } else if (e.key === 'Escape') {
                    sugBox.classList.remove('open');
                    input.blur();
                }
            });
            const updateSugHighlight = () => {
                sugBox.querySelectorAll('.walk-seed-sug').forEach((it, i) =>
                    it.classList.toggle('active', i === sugIndex));
            };

            // Hops stepper.
            root.querySelectorAll('.walk-hop-btn').forEach(btn => {
                btn.addEventListener('click', () => {
                    if (state.walkRunning) return;
                    state.walkHops = parseInt(btn.dataset.hops, 10);
                    root.querySelectorAll('.walk-hop-btn').forEach(b =>
                        b.classList.toggle('active', b === btn));
                });
            });

            // Direction segmented control.
            root.querySelectorAll('.walk-dir-btn').forEach(btn => {
                btn.addEventListener('click', () => {
                    if (state.walkRunning) return;
                    state.walkDir = btn.dataset.dir;
                    root.querySelectorAll('.walk-dir-btn').forEach(b =>
                        b.classList.toggle('active', b === btn));
                });
            });

            // Speed control — scales the two-phase beat so a fast reader
            // can race through and someone watching the structure can
            // slow it to half speed.
            root.querySelectorAll('.walk-speed-btn').forEach(btn => {
                btn.addEventListener('click', () => {
                    state.walkSpeed = parseFloat(btn.dataset.speed) || 1;
                    root.querySelectorAll('.walk-speed-btn').forEach(b =>
                        b.classList.toggle('active', b === btn));
                });
            });

            // Edge-type chips toggle (built once the graph is in hand).
            renderWalkEdgeTypes();

            // Run / exit.
            document.getElementById('walk-run').addEventListener('click', runWalk);
            document.getElementById('walk-exit').addEventListener('click', () => exitWalk());

            // Esc exits a running walk, like path mode.
            document.addEventListener('keydown', e => {
                if (e.key === 'Escape' && state.walkActive) exitWalk();
            });

            // Seed follows the current selection (see syncWalkSeed, wired
            // into handleClick). At init, adopt whatever is already selected.
            if (state.selectedNode) syncWalkSeed(state.selectedNode);
        }

        // Called from handleClick on every node selection. Mirrors the
        // selection into the Walk pane's seed and chip so that opening Walk
        // always starts from the node the user was just looking at. Skipped
        // while a walk is running (the walk owns its seed) and deliberately
        // does NOT switch tabs or touch the seed input's in-progress typing.
        function syncWalkSeed(node) {
            if (!node || state.walkRunning) return;
            state.walkSeed = node.id;
            const chipHost = document.getElementById('walk-seed-chip');
            if (chipHost) {
                chipHost.innerHTML = `<span class="walk-seed-chip">`
                    + nodeIconSvg(node.group)
                    + `<span class="nm" title="${escapeHtml(node.id)}">${escapeHtml(truncateName(node.name))}</span>`
                    + `<button type="button" class="x" title="Clear seed" aria-label="Clear seed">✕</button>`
                    + `</span>`;
                chipHost.querySelector('.x').addEventListener('click', clearWalkSeed);
            }
            // Only narrate when the user is actually looking at Walk.
            if (state.discoverSub === 'walk' && !state.walkActive) {
                const status = document.getElementById('walk-status');
                if (status) { status.classList.remove('error'); status.textContent = `Seed: ${node.id}`; }
            }
        }

        function renderWalkEdgeTypes() {
            const box = document.getElementById('walk-edge-types');
            if (!box || !state.graph.edges.length) return;
            const counts = new Map();
            state.graph.edges.forEach(e => {
                const r = e.rel || 'other';
                counts.set(r, (counts.get(r) || 0) + 1);
            });
            const types = [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([t]) => t);
            box.innerHTML = '';
            // "All" toggle — default on, clears the explicit selection.
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
                    if (state.walkEdgeTypes.has(t)) {
                        state.walkEdgeTypes.delete(t);
                        chip.classList.remove('active');
                    } else {
                        state.walkEdgeTypes.add(t);
                        chip.classList.add('active');
                    }
                    const anyExplicit = state.walkEdgeTypes.size > 0;
                    all.classList.toggle('active', !anyExplicit);
                    // Choosing specifics means "only these"; clearing all of
                    // them returns to the default mix.
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
            const chipHost = document.getElementById('walk-seed-chip');
            if (input) { input.value = ''; input.placeholder = 'Seed selected — change it any time'; }
            if (sugBox) sugBox.classList.remove('open');
            if (chipHost) {
                chipHost.innerHTML = `<span class="walk-seed-chip">`
                    + nodeIconSvg(n.group)
                    + `<span class="nm" title="${escapeHtml(n.id)}">${escapeHtml(truncateName(n.name))}</span>`
                    + `<button type="button" class="x" title="Clear seed" aria-label="Clear seed">✕</button>`
                    + `</span>`;
                chipHost.querySelector('.x').addEventListener('click', clearWalkSeed);
            }
            const status = document.getElementById('walk-status');
            if (status) { status.classList.remove('error'); status.textContent = `Seed: ${n.id}`; }
        }

        function clearWalkSeed() {
            state.walkSeed = null;
            const input = document.getElementById('walk-seed-input');
            const chipHost = document.getElementById('walk-seed-chip');
            if (input) input.placeholder = 'Type a function, class, file…';
            if (chipHost) chipHost.innerHTML = '';
            if (input) input.focus();
        }

        // ── The walk itself ──────────────────────────────────
        //
        // BFS over the loaded graph (`state.graph` via the adjacency index
        // built in 13-solo-view.js), respecting direction and an optional
        // edge-type set — the same semantics as the `traverse` MCP tool and
        // the `/api/db/traverse` route. Returns one layer per hop so the
        // animation can reveal them sequentially.
        function computeWalk(seedId, maxHops, dir, edgeTypes) {
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
                        if (!seenThisLayer.has(neighbour)) {
                            seenThisLayer.add(neighbour);
                            next.push(neighbour);
                        }
                    }
                }
                if (!next.length) break;
                next.forEach(id => { reached.add(id); dist.set(id, h); });
                layers.push({ hop: h, ids: next, edges, tally });
                frontier = next;
            }
            return { layers, dist, reached };
        }

        async function runWalk() {
            const status = document.getElementById('walk-status');
            const runBtn = document.getElementById('walk-run');
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
            if (state.walkRunning) return;

            // Cancel any prior walk and reset its visuals.
            exitWalk(true);

            state.walkRunning = true;
            state.walkActive = true;
            if (runBtn) runBtn.disabled = true;
            document.body.classList.add('walk-active');

            // Preflight the full walk up front so the readout can show what's
            // coming; the animation reveals it layer by layer.
            const { layers, reached } = computeWalk(
                seedId, state.walkHops, state.walkDir, state.walkEdgeTypes);
            const totalEdges = layers.reduce((a, l) => a + l.edges.length, 0);

            // Seed = hop 0. Seed is also the selection so the ring marker
            // lands on it.
            state.walkReached = new Set([seedId]);
            state.walkColors = new Map([[seedId, walkColorForHop(0)]]);
            state.walkEdgeKeys = new Set();
            state.selectedNode = seedNode;

            if (status) {
                status.classList.remove('error');
                status.textContent = `Walking ${state.walkHops} hop${state.walkHops === 1 ? '' : 's'} ${state.walkDir} from ${truncateName(seedId)}…`;
            }
            setWalkProgress(0, Math.max(1, layers.length - 1), 'seed');

            // Solo: draw the seed alone first, then let each frontier add to
            // the canvas as it's reached. Below the threshold the whole graph
            // is already drawn and the walk just recolours it (unreached nodes
            // are hidden by nodeVisibleFor — the canvas is force-isolated to
            // the reached set for the duration of the walk).
            if (state.soloOnly) {
                plotNodes([seedId]);
                // Suppress the settle handler's own reframe so focusNode /
                // frameNodeSet own the camera for every step.
                state._didFit = true;
                state._boxSettled = true;
            }
            bumpGraphStyles();
            focusNode(seedNode);
            emitWalkPulse(seedNode, walkColorForHop(0));
            updateWalkReadout(layers, 0, totalEdges);

            const myToken = (state._walkToken = (state._walkToken || 0) + 1);
            const totalHops = layers.length - 1;

            for (let h = 1; h < layers.length; h++) {
                const layer = layers[h];
                const colour = walkColorForHop(h);

                // ── Phase 1 — edge ignite ────────────────────────────
                // Add the frontier to the reached set (so it is drawn, not
                // hidden by isolation) but NOT yet to walkColors — it renders
                // as a faint "pending" ghost. Light the connecting edges so
                // particles stream outward from the previous frontier toward
                // those ghosts. This beat is what makes the walk legible: the
                // eye follows the flow before the nodes ignite.
                layer.ids.forEach(id => state.walkReached.add(id));
                layer.edges.forEach(e => state.walkEdgeKeys.add(walkEdgeKey(e.source, e.target)));
                if (state.soloOnly) {
                    plotNodes(Array.from(state.walkReached));
                    state._didFit = true;
                    state._boxSettled = true;
                }
                bumpGraphStyles();
                setWalkProgress(h - 1, totalHops, 'stream');
                if (status) status.textContent = `Hop ${h}/${totalHops}: streaming outward…`;

                await delay(walkIgniteMs());
                if (myToken !== state._walkToken) return; // cancelled

                // ── Phase 2 — frontier ignite ────────────────────────
                layer.ids.forEach(id => state.walkColors.set(id, colour));
                bumpGraphStyles();
                frameNodeSet(state.walkReached, walkFrameMs());
                emitWalkPulse(seedNode, colour);
                pingPipelineBox();
                updateWalkReadout(layers, h, totalEdges);
                setWalkProgress(h, totalHops, 'ignite');
                if (status) status.textContent =
                    `Hop ${h}/${totalHops}: +${layer.ids.length} node${layer.ids.length === 1 ? '' : 's'} reached`;

                // Dwell on the ignited frontier before the next beat starts.
                await delay(walkDwellMs());
                if (myToken !== state._walkToken) return; // cancelled
            }

            state.walkRunning = false;
            if (runBtn) runBtn.disabled = false;
            setWalkProgress(totalHops, totalHops, 'done');
            if (status) {
                status.textContent = `Reached ${reached.size} node${reached.size === 1 ? '' : 's'} · ${totalEdges} edge${totalEdges === 1 ? '' : 's'} across ${totalHops} hop${totalHops === 1 ? '' : 's'}`;
            }
        }

        // Progress bar above the readout: fills hop by hop, and its colour
        // hints which phase is live (streaming vs ignited).
        function setWalkProgress(done, total, phase) {
            const wrap = document.getElementById('walk-progress');
            const bar = document.getElementById('walk-progress-bar');
            const label = document.getElementById('walk-progress-label');
            if (!wrap || !bar) return;
            wrap.hidden = false;
            const pct = total ? Math.round((done / total) * 100) : 0;
            bar.style.width = pct + '%';
            bar.dataset.phase = phase || '';
            if (label) label.textContent = total ? `${done}/${total}` : '';
        }

        function exitWalk(quiet) {
            // Invalidate any in-flight reveal loop.
            state._walkToken = (state._walkToken || 0) + 1;
            const wasActive = state.walkActive;
            state.walkActive = false;
            state.walkRunning = false;
            const runBtn = document.getElementById('walk-run');
            if (runBtn) runBtn.disabled = false;
            document.body.classList.remove('walk-active');

            const reached = state.walkReached;
            state.walkReached = new Set();
            state.walkColors = new Map();
            state.walkEdgeKeys = new Set();

            // Hand the result back to a normal solo view: keep the reached
            // set on the canvas as an ordinary neighbourhood the user can
            // keep poking at, rather than clearing it.
            if (wasActive && state.soloOnly && reached.size) {
                plotNodes(Array.from(reached));
            }
            if (wasActive) bumpGraphStyles();

            if (!quiet) {
                const status = document.getElementById('walk-status');
                if (status) status.textContent = '';
                const readout = document.getElementById('walk-readout');
                if (readout) readout.classList.remove('open');
            }
            const progress = document.getElementById('walk-progress');
            if (progress) progress.hidden = true;
        }

        // Farthest reached node from the seed, in world units — read live off
        // the rendered positions so the wavefront always tracks what is on
        // screen (in solo mode the force sim may still be settling a frontier
        // when a pulse fires; the next hop's pulse catches up as it spreads).
        function walkReachRadius(seedNode) {
            let max = 0;
            state.walkReached.forEach(id => {
                const n = state.nodeById && state.nodeById.get(id);
                if (!n || !Number.isFinite(n.x)) return;
                const dx = (n.x || 0) - (seedNode.x || 0);
                const dy = (n.y || 0) - (seedNode.y || 0);
                const dz = (n.z || 0) - (seedNode.z || 0);
                const d = Math.sqrt(dx * dx + dy * dy + dz * dz);
                if (d > max) max = d;
            });
            return max;
        }

        // A "wavefront" sphere centred on the seed. Its final radius encloses
        // every node reached so far — so each hop the ring grows outward to
        // cover the new frontier, which is exactly the hop/walk concept made
        // visible. It grows to that boundary, holds it briefly so the reach
        // reads as a shell (not a flash), then fades. Self-disposing.
        function emitWalkPulse(seedNode, colour) {
            if (!Graph || !seedNode || !Number.isFinite(seedNode.x)) return;
            const baseR = 8;
            const finalR = Math.max(walkReachRadius(seedNode) + 22, 50);
            const targetScale = finalR / baseR;
            const geo = new THREE.IcosahedronGeometry(baseR, 1);
            const mat = new THREE.MeshBasicMaterial({
                color: colour, wireframe: true, transparent: true,
                opacity: 0.85, depthWrite: false,
            });
            const mesh = new THREE.Mesh(geo, mat);
            mesh.position.set(seedNode.x || 0, seedNode.y || 0, seedNode.z || 0);
            mesh.renderOrder = 500;
            Graph.scene().add(mesh);

            const start = performance.now();
            const dur = Math.min(1050, Math.max(520, walkDwellMs()));
            const growFrac = 0.6;   // expand over 60%, fade over the rest
            const step = () => {
                const t = (performance.now() - start) / dur;
                if (t >= 1 || !state.walkActive) {
                    Graph.scene().remove(mesh);
                    geo.dispose(); mat.dispose();
                    return;
                }
                const g = t < growFrac ? t / growFrac : 1;
                const e = 1 - Math.pow(1 - g, 3);             // ease-out grow
                mesh.scale.setScalar(1 + (targetScale - 1) * e);
                mat.opacity = t < growFrac
                    ? 0.85
                    : 0.85 * (1 - (t - growFrac) / (1 - growFrac));
                requestAnimationFrame(step);
            };
            requestAnimationFrame(step);
        }

        // ── Readout: hop legend + per-hop breakdown + running totals ──
        function updateWalkReadout(layers, upto, totalEdges) {
            const readout = document.getElementById('walk-readout');
            if (!readout) return;
            readout.classList.add('open');

            // Legend — one dot per hop that will fire.
            const legend = document.getElementById('walk-legend');
            if (legend) {
                legend.innerHTML = '';
                for (let h = 0; h < layers.length; h++) {
                    const count = layers[h].ids.length;
                    const el = document.createElement('span');
                    el.className = 'walk-leg';
                    el.innerHTML = `<span class="dot" style="background:${walkColorForHop(h)};color:${walkColorForHop(h)}"></span>`
                        + `<span>Hop ${h}</span>`
                        + `<span class="n">${count}</span>`;
                    legend.appendChild(el);
                }
            }

            // Per-hop layers.
            const box = document.getElementById('walk-layers');
            if (box) {
                box.innerHTML = '';
                layers.forEach(layer => {
                    const row = document.createElement('div');
                    row.className = 'walk-layer'
                        + (layer.hop <= upto ? ' revealed' : '')
                        + (layer.hop === upto ? ' current' : '');
                    const colour = walkColorForHop(layer.hop);
                    const tally = Object.entries(layer.tally)
                        .sort((a, b) => b[1] - a[1]);
                    const tags = layer.hop === 0
                        ? `<div class="walk-layer-empty">starting point</div>`
                        : tally.length
                            ? `<div class="walk-layer-edges">${
                                tally.map(([r, c]) => `<span class="walk-et-tag">${escapeHtml(r)} ×${c}</span>`).join('')
                              }</div>`
                            : `<div class="walk-layer-empty">no edges</div>`;
                    row.innerHTML = `<div class="walk-layer-bar" style="background:${colour}"></div>`
                        + `<div class="walk-layer-body">`
                        + `<div class="walk-layer-head">`
                        + `<span class="walk-layer-hop">Hop ${layer.hop}</span>`
                        + `<span class="walk-layer-count">+${layer.ids.length}</span>`
                        + `</div>${tags}</div>`;
                    box.appendChild(row);
                });
            }

            // Totals.
            const totals = document.getElementById('walk-totals');
            if (totals) {
                const reachedSoFar = layers.reduce((a, l, i) => i <= upto ? a + l.ids.length : a, 0);
                const edgesSoFar = layers.reduce((a, l, i) => i <= upto ? a + l.edges.length : a, 0);
                totals.innerHTML =
                    `<span><b>${reachedSoFar}</b> nodes</span>`
                    + `<span><b>${edgesSoFar}</b> edges</span>`
                    + `<span><b>${upto}</b> of <b>${layers.length - 1}</b> hops</span>`;
            }
        }

        // Pulse the pipeline diagram's GRAPH WALK box in sync with each
        // frontier reveal — ties the canvas animation to the explainer.
        function pingPipelineBox() {
            const box = document.getElementById('walk-pipe-graph');
            if (!box) return;
            box.classList.remove('pulse');
            // Force a reflow so the animation restarts on every hop.
            void box.offsetWidth;
            box.classList.add('pulse');
            setTimeout(() => box.classList.remove('pulse'), walkDwellMs());
        }
