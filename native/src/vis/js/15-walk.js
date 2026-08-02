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
            const updateSugHighlight = () => {
                sugBox.querySelectorAll('.walk-seed-sug').forEach((it, i) =>
                    it.classList.toggle('active', i === sugIndex));
            };

            input.addEventListener('input', refreshSeedSuggestions);
            input.addEventListener('focus', refreshSeedSuggestions);
            input.addEventListener('blur', () => setTimeout(() => sugBox.classList.remove('open'), 140));
            input.addEventListener('keydown', e => {
                const items = sugBox.querySelectorAll('.walk-seed-sug[data-index]');
                if (e.key === 'ArrowDown') { e.preventDefault(); sugIndex = Math.min(sugIndex + 1, items.length - 1); updateSugHighlight(); }
                else if (e.key === 'ArrowUp') { e.preventDefault(); sugIndex = Math.max(sugIndex - 1, 0); updateSugHighlight(); }
                else if (e.key === 'Enter') { e.preventDefault(); const pick = items[sugIndex] || items[0]; if (pick) pick.dispatchEvent(new Event('mousedown')); }
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
            if (!box || !state.graph.edges.length) return;
            const counts = new Map();
            state.graph.edges.forEach(e => {
                const r = e.rel || 'other';
                counts.set(r, (counts.get(r) || 0) + 1);
            });
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
                        if (!seenThisLayer.has(neighbour)) { seenThisLayer.add(neighbour); next.push(neighbour); }
                    }
                }
                if (!next.length) break;
                next.forEach(id => { reached.add(id); dist.set(id, h); });
                layers.push({ hop: h, ids: next, edges, tally });
                frontier = next;
            }
            return { layers, dist, reached };
        }

        // Launch from the sidebar: validate, compute, record, play.
        function runWalk() {
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
            const { layers, reached } = computeWalk(seedId, state.walkHops, state.walkDir, state.walkEdgeTypes);
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

        function playWalk(seedNode, layers, totalEdges) {
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

            enterWalkImmersive();
            buildWalkSegments(layers.length);
            restoreWalkPosition();
            showWalkOverlay();

            // Hop 0 — the seed, ignited immediately. A small establishing
            // pulse marks the origin the rest of the walk radiates from.
            setWalkStateToHop(0);
            walkPlay.index = 0;
            emitWalkPulse(seedNode, walkColorForHop(0), 4, 44, 320);
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
            }
            bumpGraphStyles();
            frameNodeSet(reached, walkFrameMs());
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
            }
            bumpGraphStyles();
            const fromR = layerReachRadius(walkPlay.seedNode, h - 1);
            const toR = layerReachRadius(walkPlay.seedNode, h);
            emitWalkPulse(walkPlay.seedNode, colour, fromR, toR, walkIgniteMs());
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
            bumpGraphStyles();
            frameNodeSet(state.walkReached, walkFrameMs());
            pingPipelineBox();
            walkPlay.index = h;
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
            if (wasActive) {
                exitWalkImmersive();
                // Keep the walked neighbourhood on the canvas as an ordinary
                // selection the user can keep exploring.
                if (state.soloOnly && reached.size) plotNodes(Array.from(reached));
                bumpGraphStyles();
            }
            if (!quiet) {
                const status = document.getElementById('walk-status');
                if (status) status.textContent = '';
            }
        }

        // ── Overlay UI ───────────────────────────────────────

        function showWalkOverlay() {
            walkPlay.active = true;
            const ovl = walkEl('walk-overlay');
            if (ovl) ovl.classList.add('visible');
            const seedEl = walkEl('walk-o-seed');
            if (seedEl && walkPlay.seedNode) {
                seedEl.textContent = truncateName(walkPlay.seedNode.name);
                seedEl.title = walkPlay.seedNode.id;
            }
            buildWalkSegments(walkPlay.layers.length);
        }
        function hideWalkOverlay() {
            const ovl = walkEl('walk-overlay');
            if (ovl) ovl.classList.remove('visible');
        }

        function buildWalkSegments(count) {
            const bar = walkEl('walk-o-progress');
            if (!bar) return;
            bar.innerHTML = '';
            for (let h = 0; h < count; h++) {
                const seg = document.createElement('div');
                seg.className = 'walk-seg';
                seg.dataset.hop = h;
                seg.title = h === 0 ? 'Seed' : `Hop ${h}`;
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
            const last = layers.length - 1;

            // Counter.
            const counter = walkEl('walk-o-counter');
            if (counter) counter.textContent = last > 0 ? `${idx}/${last}` : '';

            // Segments — done up to index, active for streaming/index.
            const bar = walkEl('walk-o-progress');
            if (bar) {
                const focus = walkPlay.streaming > 0 ? walkPlay.streaming : idx;
                bar.querySelectorAll('.walk-seg').forEach(seg => {
                    const h = parseInt(seg.dataset.hop, 10);
                    seg.classList.toggle('done', h < focus || (h === focus && walkPlay.streaming < 0));
                    seg.classList.toggle('active', h === focus);
                    seg.style.setProperty('--seg',
                        (h === focus && walkPlay.streaming > 0) ? '50%' : (h < focus ? '100%' : '0%'));
                });
            }

            // Hop title + phase tag handled by setOverlayPhase; edges for current layer.
            const hopEl = walkEl('walk-o-hop');
            if (hopEl) {
                hopEl.textContent = idx === 0 ? 'Seed' : `Hop ${idx}`;
            }
            const edgesEl = walkEl('walk-o-edges');
            if (edgesEl) {
                const tally = idx === 0 ? {} : (layers[idx].tally || {});
                const entries = Object.entries(tally).sort((a, b) => b[1] - a[1]);
                edgesEl.innerHTML = idx === 0
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

            // Totals up to the current index.
            const totals = walkEl('walk-o-totals');
            if (totals) {
                const reachedSoFar = layers.reduce((a, l, i) => i <= idx ? a + l.ids.length : a, 0);
                const edgesSoFar = layers.reduce((a, l, i) => i <= idx ? a + l.edges.length : a, 0);
                totals.innerHTML =
                    `<span><b>${reachedSoFar}</b> nodes</span>`
                    + `<span><b>${edgesSoFar}</b> edges</span>`
                    + `<span><b>${idx}</b> of <b>${last}</b> hops</span>`;
            }

            // Transport disabled state at the ends.
            const prev = walkEl('walk-o-prev');
            const next = walkEl('walk-o-next');
            if (prev) prev.disabled = idx <= 0 && walkPlay.streaming < 0;
            if (next) next.disabled = idx >= last && walkPlay.streaming < 0;
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
            if (r.autoSpin) {
                state.autoSpin = true;
                if (typeof applyAutoSpin === 'function') applyAutoSpin();
                if (typeof syncSpinButton === 'function') syncSpinButton();
            }
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
        function replayWalkFromHistory(id) {
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
                const r = computeWalk(entry.seedId, entry.hops, entry.dir,
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

        function emitWalkPulse(seedNode, colour, fromR, toR, growMs) {
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
