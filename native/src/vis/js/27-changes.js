        // ─── Changes: walk a diff ───────────────────────────
        //
        // A tour is seeded by a question you type. A walk is seeded by a
        // *change*, which you cannot type — so this is a picker, not a bar.
        //
        // The whole panel is built around one rule: never make the user
        // guess, and never let them find out the answer by pressing the
        // expensive button.
        //
        //   /api/git/status   is git usable here, on what branch, is it dirty
        //   /api/git/commits  the dropdown's contents — this repo's own log
        //   /api/git/diff     what the selected spec touches, before walking it
        //   /api/walk         the walk, streamed into the tour overlay
        //
        // The first three are cheap and run on open / on change. The fourth
        // is the one that costs a model, and by the time it is enabled the
        // user has already seen the file list it will walk.
        //
        // git is a soft dependency end to end: `status` answers 200 with
        // `available: false` plus a code and a hint, and that hint is what
        // the banner says. A machine with no git loses this panel and
        // nothing else on the page.

        const changesState = {
            wired: false,
            loaded: false,       // commits fetched at least once
            repo: null,          // RepoStatus from /api/git/status
            spec: 'working',     // what would be walked right now
            previewToken: 0,     // drops responses for a spec the user left
            diff: null,          // the last preview, for the run button
            running: false,
        };

        /// Files listed in the preview before it turns into a wall.
        const CHG_PREVIEW_FILES = 40;

        function chgEl(id) { return document.getElementById(id); }

        function wireChanges() {
            if (changesState.wired) return;
            changesState.wired = true;

            const toggle = chgEl('changes-toggle');
            const panel = chgEl('changes-panel');
            if (!toggle || !panel) return;

            toggle.addEventListener('click', () => {
                const open = panel.hidden;
                panel.hidden = !open;
                toggle.setAttribute('aria-expanded', String(open));
                toggle.classList.toggle('open', open);
                // Probed on first open rather than at load: a page that
                // never opens this panel should not shell out to git.
                if (open && !changesState.loaded) loadChanges();
            });

            chgEl('chg-quick').addEventListener('click', (e) => {
                const btn = e.target.closest('.chg-chip');
                if (!btn || btn.disabled) return;
                selectSpec(btn.dataset.spec);
            });

            chgEl('chg-commit').addEventListener('change', (e) => {
                if (e.target.value) selectSpec(e.target.value);
            });

            // A typed ref is a deliberate override, so it wins over the
            // dropdown — but only once the user stops typing, or every
            // keystroke of `main...HEAD` is a git call.
            const ref = chgEl('chg-ref');
            const debounced = debounceTrailing(() => {
                const v = ref.value.trim();
                if (v) selectSpec(v);
            }, 400);
            ref.addEventListener('input', debounced);
            ref.addEventListener('keydown', (e) => {
                if (e.key !== 'Enter') return;
                e.preventDefault();
                debounced.cancel();
                const v = ref.value.trim();
                if (v) selectSpec(v, { run: true });
            });

            chgEl('chg-run').addEventListener('click', () => runWalk());
        }

        /// Probe the repository and fill the picker.
        async function loadChanges() {
            changesState.loaded = true;
            try {
                const res = await fetch('/api/git/status');
                const info = await res.json();
                if (!info.available) return showChangesUnavailable(info);
                changesState.repo = info.repo || {};
                applyRepoStatus(changesState.repo);
            } catch (err) {
                return showChangesUnavailable({
                    error: err.message || String(err),
                    hint: 'The server may have stopped — reload the page.',
                });
            }
            await loadCommits();
            // Open on the change the user is most likely to want: what they
            // have not committed, or the last commit when the tree is clean.
            selectSpec(changesState.repo.dirty ? 'working' : 'HEAD');
        }

        /// git is unusable here. Say why, and say what would fix it.
        function showChangesUnavailable(info) {
            const banner = chgEl('chg-unavailable');
            const body = chgEl('chg-body');
            if (!banner || !body) return;
            body.hidden = true;
            banner.hidden = false;
            const headline = {
                not_installed: 'Walking a change needs git.',
                not_a_repo: 'This project is not a git working tree.',
                no_commits: 'This repository has no commits yet.',
            }[info.code] || 'Changes are unavailable.';
            banner.innerHTML =
                `<strong>${escapeHtml(headline)}</strong> ` +
                escapeHtml(info.hint || info.error || '') +
                ' Everything else on this page is unaffected.';
        }

        function applyRepoStatus(repo) {
            const branch = chgEl('chg-branch');
            if (branch) {
                branch.textContent = repo.branch || '';
                branch.classList.toggle('dirty', !!repo.dirty);
                branch.title = repo.dirty
                    ? 'This working tree has uncommitted changes'
                    : 'This working tree is clean';
            }
            // "Uncommitted" and "Staged" are offered only when there is
            // something there: a chip that runs and reports nothing is a
            // dead end the panel already knew about.
            const quick = chgEl('chg-quick');
            const working = quick.querySelector('[data-spec="working"]');
            const staged = quick.querySelector('[data-spec="staged"]');
            if (working) {
                working.disabled = !repo.dirty;
                working.title = repo.dirty ? '' : 'Nothing uncommitted right now';
            }
            if (staged) {
                staged.disabled = !repo.staged;
                staged.title = repo.staged ? '' : 'Nothing staged right now';
            }
            // "vs main" — the question a reviewer actually asks. Only shown
            // when there is a default branch and we are not standing on it.
            const vs = chgEl('chg-vs-default');
            const def = repo.default_branch;
            if (vs && def && def !== repo.branch) {
                vs.hidden = false;
                vs.textContent = `vs ${def}`;
                vs.dataset.spec = `${def}...HEAD`;
                vs.title = `Everything this branch changed since it forked from ${def}`;
            } else if (vs) {
                vs.hidden = true;
            }
        }

        async function loadCommits() {
            const sel = chgEl('chg-commit');
            if (!sel) return;
            try {
                const res = await fetch('/api/git/commits?limit=30');
                if (!res.ok) throw new Error(await readErr(res));
                const { commits } = await res.json();
                sel.innerHTML = '';
                const blank = document.createElement('option');
                blank.value = '';
                blank.textContent = commits.length
                    ? `${commits.length} recent commits…`
                    : 'No commits';
                sel.appendChild(blank);
                (commits || []).forEach(c => {
                    const o = document.createElement('option');
                    o.value = c.sha;
                    o.textContent = commitLabel(c);
                    o.title = `${c.subject}\n${c.author} · ${c.date}`;
                    sel.appendChild(o);
                });
            } catch (err) {
                sel.innerHTML = '';
                const o = document.createElement('option');
                o.value = '';
                o.textContent = `Could not list commits: ${err.message || err}`;
                sel.appendChild(o);
            }
        }

        /// One dropdown row: hash, subject, age, size. Enough to recognise a
        /// commit without opening it — which is the whole job of a picker.
        function commitLabel(c) {
            const size = c.files
                ? `${c.files}f +${c.insertions}/-${c.deletions}`
                : '';
            const subject = c.subject.length > 46
                ? c.subject.slice(0, 45) + '…'
                : c.subject;
            return `${c.short}  ${subject}  · ${c.relative}${size ? ' · ' + size : ''}`;
        }

        /// Point the panel at `spec` and fetch what it touches.
        function selectSpec(spec, opts) {
            opts = opts || {};
            changesState.spec = spec;
            chgEl('chg-quick').querySelectorAll('.chg-chip').forEach(b => {
                b.classList.toggle('active', b.dataset.spec === spec);
            });
            // A chip and the dropdown are two views of the same choice; only
            // one can be showing a selection.
            const sel = chgEl('chg-commit');
            if (sel && sel.value !== spec) sel.value = [...sel.options].some(o => o.value === spec) ? spec : '';
            previewSpec(spec, opts.run);
        }

        async function previewSpec(spec, thenRun) {
            const token = ++changesState.previewToken;
            const preview = chgEl('chg-preview');
            const stat = chgEl('chg-stat');
            const run = chgEl('chg-run');
            changesState.diff = null;
            run.disabled = true;
            setChangesStatus('');
            preview.hidden = false;
            stat.textContent = 'Reading the diff…';
            chgEl('chg-files').innerHTML = '';

            let payload;
            try {
                const res = await fetch(`/api/git/diff?spec=${encodeURIComponent(spec)}`);
                payload = await res.json();
                if (!res.ok) throw new Error(payload.error || `HTTP ${res.status}`);
            } catch (err) {
                if (token !== changesState.previewToken) return;
                preview.hidden = true;
                setChangesStatus(err.message || String(err), true);
                return;
            }
            // The user moved on while this was in flight.
            if (token !== changesState.previewToken) return;

            const diff = payload.diff;
            changesState.diff = diff;
            renderDiffPreview(diff, payload.drifted || []);
            // An empty diff is not an error, but there is nothing to walk.
            run.disabled = !diff.files.length;
            if (!diff.files.length) {
                setChangesStatus('Nothing changed here — pick another change.');
            } else if (thenRun) {
                runWalk();
            }
        }

        function renderDiffPreview(diff, drifted) {
            const stat = chgEl('chg-stat');
            const files = chgEl('chg-files');
            if (!diff.files.length) {
                stat.innerHTML = `<span class="empty">${escapeHtml(diff.label)} — no changes</span>`;
                files.innerHTML = '';
                return;
            }
            const commits = diff.commits && diff.commits.length > 1
                ? ` · ${diff.commits.length} commits`
                : '';
            stat.innerHTML =
                `${escapeHtml(diff.label)} — ${diff.files.length} file${diff.files.length === 1 ? '' : 's'} ` +
                `<span class="add">+${diff.insertions}</span>/<span class="del">-${diff.deletions}</span>` +
                escapeHtml(commits);

            const drift = new Set(drifted);
            files.innerHTML = '';
            diff.files.slice(0, CHG_PREVIEW_FILES).forEach(f => {
                const row = document.createElement('div');
                row.className = 'chg-file' + (drift.has(f.path) ? ' drift' : '');
                const mark = document.createElement('span');
                mark.className = `mark ${f.status}`;
                mark.textContent = f.status.charAt(0).toUpperCase();
                mark.title = f.status;
                const path = document.createElement('span');
                path.className = 'path';
                // The inner span restores LTR inside the RTL ellipsis trick,
                // so a path is truncated at the front and still reads
                // forwards.
                const inner = document.createElement('span');
                inner.textContent = f.path;
                path.appendChild(inner);
                path.title = drift.has(f.path)
                    ? `${f.path} — changed again since this diff, so its stops may be approximate`
                    : f.path;
                const delta = document.createElement('span');
                delta.textContent = `+${f.added}/-${f.removed}`;
                row.append(mark, path, delta);
                files.appendChild(row);
            });
            if (diff.files.length > CHG_PREVIEW_FILES) {
                const more = document.createElement('div');
                more.className = 'chg-file';
                more.textContent = `… and ${diff.files.length - CHG_PREVIEW_FILES} more`;
                files.appendChild(more);
            }
        }

        function setChangesStatus(text, isError) {
            const el = chgEl('chg-status');
            if (!el) return;
            el.textContent = text || '';
            el.classList.toggle('error', !!isError);
        }

        // ── Running it ──────────────────────────────────────
        //
        // Shares the tour overlay wholesale: a walk *is* a tour whose stops
        // came from a diff, and the response is a tour object with `diff`
        // alongside it and a `change` on each stop. Two cinematic overlays
        // would be two sets of playback bugs.

        async function runWalk() {
            if (changesState.running) return;
            const spec = changesState.spec;
            const diff = changesState.diff;
            if (!diff || !diff.files.length) return;

            changesState.running = true;
            const run = chgEl('chg-run');
            run.disabled = true;
            setChangesStatus('Planning your walk…');

            beginTourSession();
            tourEl('tour-o-title').textContent = diff.label;
            const note = tourEl('tour-o-note');
            note.hidden = true;
            note.classList.remove('error');
            resetPlanProgress(diff.label);

            const body = {
                spec,
                stream: true,
                expand: chgEl('chg-expand').checked,
                max_stops: clampInt(askEl('tour-stops').value, 2, 40, 8),
            };

            const fail = (msg) => {
                stopPlanProgress();
                note.hidden = false;
                note.classList.add('error');
                note.textContent = `Walk failed: ${msg}`;
                setChangesStatus(`Walk failed: ${msg}`, true);
            };

            try {
                const res = await fetch('/api/walk', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(body),
                });
                if (!res.ok) throw new Error(await readErr(res));

                let walk = null, streamErr = null;
                if (res.body && (res.headers.get('content-type') || '').includes('event-stream')) {
                    await readSseStream(res, (name, payload) => {
                        if (!tourState.active) return;      // user bailed
                        if (name === 'progress') applyTourProgress(payload);
                        else if (name === 'walk') walk = payload;
                        else if (name === 'error') streamErr = payload.error || 'stream error';
                    });
                } else {
                    walk = await res.json();
                }
                if (!tourState.active) return;
                if (streamErr) throw new Error(streamErr);
                if (!walk) throw new Error('the server closed the stream without a walk');

                stopPlanProgress();
                if (!walk.stops || !walk.stops.length) {
                    note.hidden = false;
                    note.innerHTML = escapeHtml(walk.intro || 'Nothing in this change could be mapped onto the graph.');
                    setChangesStatus('');
                    return;
                }
                recordTourInHistory(walk, `⎇ ${diff.label}`, body.max_stops);
                setChangesStatus(
                    `${walk.fallback ? 'ranked itinerary' : 'guided walk'} ready · ` +
                    `${walk.stops.length} stop${walk.stops.length === 1 ? '' : 's'}`
                );
                if (tourState.streaming) upgradeToFinalTour(walk);
                else enterTourMode(walk);
            } catch (err) {
                fail(err.message || err);
                console.error(err);
            } finally {
                changesState.running = false;
                run.disabled = false;
            }
        }

        /// The `[changed +12/-3]` badge on a stop, or nothing on a tour.
        ///
        /// Exported as its own function because `renderStopMeta` is shared:
        /// a question-seeded tour has no `change` on any stop and must not
        /// grow an empty pill.
        function changeBadge(change) {
            if (!change) return null;
            const el = document.createElement('span');
            el.className = `tour-change ${change.role}`;
            el.textContent = change.role;
            el.title = {
                changed: 'The diff edited these lines',
                caller: 'Unchanged — it calls or references something that changed',
                test: 'Unchanged — a test that reaches something that changed',
            }[change.role] || '';
            if (change.added || change.removed) {
                const d = document.createElement('span');
                d.className = 'delta';
                d.textContent = `+${change.added}/-${change.removed}`;
                el.appendChild(d);
            }
            return el;
        }
