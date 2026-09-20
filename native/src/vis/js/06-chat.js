        // ─── Answer: RAG chat into an Ask block ────────────
        //
        // The question, the mode strip and the echo of what you asked all
        // live in the Ask column (`js/25-ask.js`); what is left here is the
        // turn itself — the request, the SSE reader, and the streaming
        // bubble it fills in.

        // Citations, collapsed by default — they're provenance, not the
        // answer, and a dozen of them push the reply off screen.
        function buildCitationBox(cites) {
            const box = document.createElement('details');
            box.className = 'chat-citations';
            box.open = !!state.chatCitesOpen;
            const sum = document.createElement('summary');
            // "19 sources" says nothing about what they are or why they are
            // here — and the number confuses when a tool returned 15 and the
            // list shows 19, because it is the union of everything the answer
            // could cite, not the last tool's result.
            sum.textContent = cites.length === 1
                ? '1 source the answer could cite — click to find it on the graph'
                : `${cites.length} sources the answer could cite — click one to find it on the graph`;
            box.appendChild(sum);
            // Remember the user's preference for the rest of the session.
            box.addEventListener('toggle', () => { state.chatCitesOpen = box.open; });
            cites.forEach(c => {
                const a = document.createElement('div');
                a.className = 'chat-cite';
                const lineLabel = c.start_line
                    ? `:${c.start_line}${c.end_line && c.end_line !== c.start_line ? '–' + c.end_line : ''}`
                    : '';
                a.innerHTML = nodeIconSvg(c.node_type || 'Default', 'cite-icon')
                    + escapeHtml(`[#${c.index}] ${c.name || c.id} · ${c.node_type || '?'} · ${c.file || '<unknown>'}${lineLabel}`)
                    // A citation is a retrieval hit that made it into the
                    // prompt, and it arrives with the same `matched_by`/`hop`
                    // every other hit has. Saying how each source was reached
                    // is what makes an answer checkable.
                    + askProvenanceHtml(c);
                a.title = c.description ? c.description.slice(0, 200) : (c.id || c.name || '');
                a.addEventListener('click', () => focusCitation(c));
                box.appendChild(a);
            });
            return box;
        }

        function focusCitation(c) {
            const local = state.nodeById ? state.nodeById.get(c.id) : null;
            if (local) {
                handleClick(null, local);
                focusNode(local);
            } else {
                setAskStatus(`"${c.id}" isn't in the loaded graph.`, true);
            }
        }

        // One answer turn, rendered into the Ask block that already carries
        // the question. `query` and `block` both come from `runAsk` — this
        // function reads no input element and owns no echo of its own.
        async function runChatTurn(query, block) {
            query = (query || '').trim();
            if (!query || state.chatInFlight) return;

            const k = clampInt(document.getElementById('chat-k').value, 1, 50, 8);
            const hops = clampInt(document.getElementById('chat-hops').value, 0, 4, 2);
            const modelOverride = document.getElementById('chat-model-override').value.trim();
            const systemOverride = document.getElementById('chat-system').value.trim();
            const temperatureRaw = document.getElementById('chat-temperature').value.trim();
            const maxTokensRaw = document.getElementById('chat-max-tokens').value.trim();

            const body = {
                query,
                k,
                hops,
                history: state.chatHistory.slice(-12),
            };
            if (modelOverride) body.chat_model = modelOverride;
            if (systemOverride) body.system_prompt = systemOverride;
            if (temperatureRaw) {
                const t = parseFloat(temperatureRaw);
                if (!Number.isNaN(t)) body.temperature = t;
            }
            if (maxTokensRaw) {
                const n = parseInt(maxTokensRaw, 10);
                if (!Number.isNaN(n)) body.max_tokens = n;
            }
            if (state.semDest) body.dest = state.semDest;

            body.stream = true;

            // The answer bubble exists from the first moment and fills in as
            // tokens arrive, with a live account of what the server is doing
            // above it — a chat that sits silent for a minute reads as broken.
            const turn = createChatTurn(block.body);
            const runBtn = document.getElementById('ask-run');

            state.chatInFlight = true;
            setAskStatus('Working out what to look up…');
            runBtn.disabled = true;

            const t0 = performance.now();
            let answer = '';
            try {
                const res = await fetch('/api/chat', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(body)
                });
                if (!res.ok) throw new Error(await readErr(res));

                let cites = [], done = null, streamErr = null;
                let streamErrKind = null, streamErrEndpoint = null;
                if (res.body && (res.headers.get('content-type') || '').includes('event-stream')) {
                    await readSseStream(res, (name, payload) => {
                        switch (name) {
                            case 'phase':
                                turn.phase(payload.phase === 'retrieving'
                                    ? 'Working out what to look up…' : payload.phase);
                                break;
                            case 'context':
                                cites = payload.citations || [];
                                turn.context(cites, payload.retrieval_ms);
                                break;
                            // A `search` the model ran mid-turn extended the
                            // evidence list. It arrives separately from
                            // `context` so a later source does not rewrite
                            // the progress line the reader is watching.
                            case 'citations':
                                cites = payload.citations || [];
                                turn.cites(cites);
                                break;
                            case 'tool':
                                turn.tool(payload);
                                break;
                            case 'delta':
                                if (payload.reasoning) turn.reasoning(payload.reasoning);
                                if (payload.content) { answer += payload.content; turn.append(payload.content); }
                                break;
                            case 'done':
                                done = payload;
                                break;
                            case 'error':
                                streamErr = payload.error || 'stream error';
                                streamErrKind = payload.kind || null;
                                streamErrEndpoint = payload.endpoint || null;
                                break;
                        }
                    });
                } else {
                    // Server without the streaming route — one JSON payload.
                    const data = await res.json();
                    answer = data.answer || '';
                    cites = data.citations || [];
                    turn.context(cites, data.retrieval_ms);
                    turn.append(answer);
                    done = data;
                }
                if (streamErr) {
                    // A dead endpoint isn't the user's mistake to decode —
                    // say what's wrong in plain words and offer the fix.
                    if (streamErrKind === 'llm_unreachable') {
                        // `turn.unreachable` puts this in the block, with the
                        // endpoint that did not answer. The strip only clears
                        // the progress line it was showing.
                        turn.unreachable(streamErrEndpoint);
                        setAskStatus('');
                        return;
                    }
                    throw new Error(streamErr);
                }

                const totalMs = Math.round(performance.now() - t0);
                answer = (answer || (done && done.answer) || '').trim();
                turn.finish(answer, cites, done, totalMs);

                state.chatHistory.push({ role: 'user', content: query });
                state.chatHistory.push({ role: 'assistant', content: answer });
                if (state.chatHistory.length > 24) {
                    state.chatHistory = state.chatHistory.slice(-24);
                }
                block.meta(`${cites.length} source${cites.length === 1 ? '' : 's'} · ${totalMs} ms`);
                setAskStatus('');
                // What an answer cited is a set worth seeing on the canvas,
                // the same way a Find result set is.
                setAskMatches(cites);
                // The finished text hangs off the block so its Keep button can
                // store the answer itself rather than the view it produced.
                block.el._answer = { text: answer, cites };
                recordAsk('answer', query, { answer, cites });
            } catch (err) {
                // Same again: the failure belongs to the block that failed.
                turn.fail(err.message || err);
                setAskStatus('');
                console.error(err);
            } finally {
                state.chatInFlight = false;
                runBtn.disabled = false;
            }
        }

        // ─── Chat setup inspector ───────────────────────────
        //
        // Three things decide what the model says — how context was
        // retrieved, what it was told to do, and what it could call. All
        // three are invisible by default, so publish them verbatim.

        async function loadChatSetup() {
            const box = document.getElementById('chat-setup-body');
            if (!box || box.dataset.loaded) return;
            try {
                const res = await fetch('/api/chat/config');
                if (!res.ok) throw new Error(await readErr(res));
                renderChatSetup(box, await res.json());
                box.dataset.loaded = '1';
            } catch (err) {
                box.textContent = `Couldn't load the chat setup: ${err.message || err}`;
            }
        }

        function renderChatSetup(box, cfg) {
            box.innerHTML = '';
            const section = (title) => {
                const h = document.createElement('div');
                h.className = 'setup-head';
                h.textContent = title;
                box.appendChild(h);
            };

            // ── retrieval ──
            const r = cfg.retrieval || {};
            section('How it decides what to look up');
            const sum = document.createElement('p');
            sum.className = 'setup-note';
            sum.textContent = r.summary || '';
            box.appendChild(sum);

            const steps = document.createElement('ol');
            steps.className = 'setup-steps';
            (r.stages || []).forEach(st => {
                const li = document.createElement('li');
                const b = document.createElement('b');
                b.textContent = st.label;
                const d = document.createElement('span');
                d.textContent = st.detail || '';
                li.append(b, d);
                steps.appendChild(li);
            });
            box.appendChild(steps);

            const facts = document.createElement('div');
            facts.className = 'setup-facts';
            const dflt = r.defaults || {};
            // Each chip says what the setting *is*; the tooltip says what it
            // does and which way it is wrong to move it. A row of nine bare
            // numbers is a row nobody can act on — "hops 2" and "tool rounds
            // 8" are equally opaque until something says what they bound.
            //
            // The copy lives here rather than in /api/chat/config because it
            // describes this panel's own summary of the defaults; the payload
            // already carries the full prose in `stages`.
            const ranked = (r.strategy || '').toLowerCase() === 'mmr'
                ? 'This backend has no native PageRank, so hits are reranked for relevance against '
                  + 'diversity instead of expanded through the graph.'
                : 'Personalized PageRank over the edge graph: a node neighbouring several good hits '
                  + 'outranks a single lucky match, which is what makes this more than vector search.';
            [
                ['store', r.backend || '—',
                 'Which graph store answered. `overgraph` is the embedded default; a Neo4j '
                 + 'destination reports itself here instead.'],
                ['ranking', (r.strategy || '').toUpperCase(), ranked],
                ['k', dflt.k,
                 'How many nodes one search keeps. Wider costs tokens and buries the good hits; '
                 + 'the model can ask for a different k on any call.'],
                ['hops', dflt.hops,
                 'How far the graph walk expands from each seed hit. 0 returns only what matched '
                 + 'the query directly — which is why a walk finds code your wording never named.'],
                ['context budget', dflt.max_context_chars ? `${Math.round(dflt.max_context_chars / 1000)}k chars` : '—',
                 'Ceiling on one assembled retrieval pack. Past it the lowest-ranked items are '
                 + 'dropped whole rather than a snippet being cut in half.'],
                // The three that decide whether a turn is agentic at all —
                // and the pair whose defaults were invisible while one of
                // them was silently switching the toolbox off.
                ['search before asking', dflt.seed === undefined ? null : (dflt.seed ? 'yes' : 'no'),
                 'Whether one hybrid retrieval runs on your exact wording before the model speaks. '
                 + 'Off while it has tools: it searches for itself, in the vocabulary the codebase '
                 + 'uses rather than yours, so a pre-pass is a second and worse-phrased copy of the '
                 + 'same neighbourhood. Turning tools off turns this back on.'],
                ['deliberates', dflt.think === undefined ? null : (dflt.think ? 'yes' : 'no'),
                 'Whether the model may think before answering. It follows the toolbox, because a '
                 + 'model given no room to deliberate answers from whatever it was handed: measured '
                 + 'over 12 questions, with this off it made zero tool calls.'],
                ['tool rounds', dflt.tool_rounds,
                 'Most rounds of tool calls before it must answer. Calls inside one round run '
                 + 'together; rounds are sequential, so this is also a latency ceiling. The answer '
                 + 'says when it stopped because it ran out rather than because it was done.'],
                ['per tool result', dflt.tool_result_chars ? `${Math.round(dflt.tool_result_chars / 1000)}k chars` : null,
                 'Ceiling on what one tool call may add to the prompt. Nothing caps the total '
                 + 'across rounds, so lower this for a model with a short context window rather '
                 + 'than trusting the cap to protect you.'],
            ].forEach(([k, v, why]) => {
                if (v == null) return;
                const chip = document.createElement('span');
                chip.innerHTML = '<b></b><i></i>';
                chip.querySelector('b').textContent = k;
                chip.querySelector('i').textContent = v;
                if (why) {
                    chip.title = `${k}: ${why}`;
                    chip.className = 'has-why';
                }
                facts.appendChild(chip);
            });
            box.appendChild(facts);

            // ── system prompt ──
            section('System prompt');
            const promptNote = document.createElement('p');
            promptNote.className = 'setup-note';
            promptNote.textContent = 'Sent with every question. The second half is appended only when tools are enabled.';
            box.appendChild(promptNote);
            box.appendChild(copyBlock('Base', cfg.system_prompt || ''));
            if (cfg.tool_suffix) box.appendChild(copyBlock('Appended when tools are on', cfg.tool_suffix.trim()));

            // ── tools ──
            const tools = cfg.tools || [];
            section(`Tools · ${tools.length}`);
            const toolNote = document.createElement('p');
            toolNote.className = 'setup-note';
            toolNote.textContent = tools.length
                ? 'The model chooses which to call, if any. Expand one to see its parameters.'
                : 'No tools are available to this server.';
            box.appendChild(toolNote);
            tools.forEach(t => {
                const d = document.createElement('details');
                d.className = 'setup-tool';
                const s2 = document.createElement('summary');
                s2.innerHTML = '<code></code><span></span>';
                s2.querySelector('code').textContent = t.name;
                // First sentence only — the full text is the model's to read.
                s2.querySelector('span').textContent =
                    String(t.description || '').split(/(?<=\.)\s/)[0].slice(0, 130);
                const pre = document.createElement('pre');
                pre.textContent = JSON.stringify(t.parameters || {}, null, 2);
                const desc = document.createElement('p');
                desc.className = 'setup-note';
                desc.textContent = t.description || '';
                d.append(s2, desc, pre);
                box.appendChild(d);
            });
        }

        // A labelled, copyable block of verbatim text.
        function copyBlock(label, text) {
            const wrap = document.createElement('div');
            wrap.className = 'ct-block';
            const head = document.createElement('div');
            head.className = 'ct-block-head';
            const l = document.createElement('span');
            l.textContent = label;
            const copy = document.createElement('button');
            copy.type = 'button';
            copy.className = 'ct-copy';
            copy.textContent = 'copy';
            copy.addEventListener('click', async (e) => {
                e.preventDefault();
                try {
                    await navigator.clipboard.writeText(text);
                    copy.textContent = 'copied';
                } catch (err) {
                    copy.textContent = 'failed';
                }
                setTimeout(() => { copy.textContent = 'copy'; }, 1200);
            });
            head.append(l, copy);
            const pre = document.createElement('pre');
            pre.textContent = text;
            wrap.append(head, pre);
            return wrap;
        }

        // ─── Minimal markdown renderer ──────────────────────
        //
        // Models answer in markdown, so rendering it is the difference
        // between a reply you can read and a wall of asterisks. Everything
        // is escaped before any markup is emitted — the input is model
        // output, which is untrusted by construction.

        function renderMarkdown(src) {
            const text = String(src || '').replace(/\r\n?/g, '\n');
            const blocks = [];
            // Pull fenced code out first so nothing inside it gets parsed.
            const fenced = text.replace(/(^|\n)([ \t]*)```([\w+-]*)\n([\s\S]*?)```/g,
                (_, lead, pad, lang, code) => {
                    // A fence under a list item is indented, and that indent
                    // belongs to the item, not to the code.
                    if (pad) code = code.replace(new RegExp('^' + pad, 'gm'), '');
                    blocks.push({ lang: lang || '', code: code.replace(/\n$/, '') });
                    return `${lead}${pad} CODE${blocks.length - 1} `;
                });

            const lines = fenced.split('\n');
            let html = '';
            const lists = [];         // open <ul>/<ol>, outermost first
            let para = [];
            let quote = [];

            const inline = (s) => {
                let out = escapeHtml(s);
                // `code` first: its content must not be re-parsed.
                const spans = [];
                out = out.replace(/`([^`]+)`/g, (_, c) => {
                    spans.push(c);
                    return ` IC${spans.length - 1} `;
                });
                out = out
                    .replace(/\*\*\*([^*]+)\*\*\*/g, '<strong><em>$1</em></strong>')
                    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
                    .replace(/(^|[\s(])\*([^*\n]+)\*/g, '$1<em>$2</em>')
                    .replace(/(^|[\s(])_([^_\n]+)_/g, '$1<em>$2</em>')
                    .replace(/~~([^~]+)~~/g, '<del>$1</del>');
                // Links: only http(s), and rendered inert (the graph UI has
                // no business opening arbitrary URLs on click-through).
                out = out.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
                    (_, label, href) => `<a href="${href}" target="_blank" rel="noreferrer noopener">${label}</a>`);
                // [#3] citation markers become clickable chips.
                out = out.replace(/\[#(\d+)\]/g, '<span class="md-cite" data-cite="$1">[#$1]</span>');
                return out.replace(/ IC(\d+) /g, (_, i) => `<code>${spans[+i]}</code>`);
            };

            const flushPara = () => {
                if (para.length) { html += `<p>${inline(para.join(' '))}</p>`; para = []; }
            };
            const flushList = () => {
                while (lists.length) {
                    const l = lists.pop();
                    html += (l.open ? '</li>' : '') + `</${l.tag}>`;
                }
            };
            const flushQuote = () => {
                if (quote.length) { html += `<blockquote>${inline(quote.join(' '))}</blockquote>`; quote = []; }
            };
            const flushAll = () => { flushPara(); flushList(); flushQuote(); };

            const codeHtml = (b) => `<pre class="md-code"${b.lang ? ` data-lang="${escapeHtml(b.lang)}"` : ''}>`
                + `<code>${escapeHtml(b.code)}</code></pre>`;

            // A pipe table is the one construct that needs its neighbour: the
            // row above the dashes is the header, so it can only be
            // recognised one line late. Models reach for tables constantly —
            // without this a comparison arrives as a wall of pipes.
            const cells = (line) => {
                let t = line.trim();
                if (t.startsWith('|')) t = t.slice(1);
                if (t.endsWith('|')) t = t.slice(0, -1);
                return t.split('|').map(c => c.trim());
            };
            const isDelimRow = (line) => line.includes('|') && line.includes('-')
                && cells(line).every(c => /^:?-+:?$/.test(c));
            const alignOf = (c) => {
                const l = c.startsWith(':'), r = c.endsWith(':');
                return l && r ? ' class="md-mid"' : r ? ' class="md-end"' : '';
            };

            for (let i = 0; i < lines.length; i++) {
                const line = lines[i].replace(/\s+$/, '');
                const open = lists.length ? lists[lists.length - 1] : null;

                // A fence indented under an item belongs to that item; one at
                // the margin ends the list it follows.
                const codeRef = line.match(/^(\s*) CODE(\d+) $/);
                if (codeRef) {
                    if (open && open.open && codeRef[1]) flushPara();
                    else flushAll();
                    html += codeHtml(blocks[+codeRef[2]]);
                    continue;
                }
                // A blank line ends a paragraph but not a list: models put one
                // between items, and closing there restarts the numbering.
                if (!line.trim()) { flushPara(); flushQuote(); continue; }

                const heading = line.match(/^(#{1,6})\s+(.*)$/);
                if (heading) {
                    flushAll();
                    const level = Math.min(6, heading[1].length);
                    html += `<h${level} class="md-h">${inline(heading[2])}</h${level}>`;
                    continue;
                }
                if (/^(-{3,}|\*{3,}|_{3,})$/.test(line.trim())) { flushAll(); html += '<hr>'; continue; }

                if (line.includes('|') && i + 1 < lines.length && isDelimRow(lines[i + 1])) {
                    flushAll();
                    const align = cells(lines[i + 1]).map(alignOf);
                    let t = '<div class="md-tablewrap"><table class="md-table"><thead><tr>';
                    cells(line).forEach((c, n) => { t += `<th${align[n] || ''}>${inline(c)}</th>`; });
                    t += '</tr></thead><tbody>';
                    let j = i + 2;
                    for (; j < lines.length; j++) {
                        const row = lines[j];
                        if (!row.trim() || !row.includes('|')) break;
                        t += '<tr>';
                        cells(row).forEach((c, n) => { t += `<td${align[n] || ''}>${inline(c)}</td>`; });
                        t += '</tr>';
                    }
                    html += t + '</tbody></table></div>';
                    i = j - 1;
                    continue;
                }

                const quoted = line.match(/^>\s?(.*)$/);
                if (quoted) { flushPara(); flushList(); quote.push(quoted[1]); continue; }
                flushQuote();

                const item = line.match(/^(\s*)(?:([-*+])|\d+[.)])\s+(.*)$/);
                if (item) {
                    flushPara();
                    const indent = item[1].replace(/\t/g, '    ').length;
                    const tag = item[2] ? 'ul' : 'ol';
                    // A deeper indent opens a child list *inside* the item
                    // that is still open — that is what makes a nested bullet
                    // nest rather than restart the list at the top.
                    while (lists.length && indent < lists[lists.length - 1].indent) {
                        const l = lists.pop();
                        html += (l.open ? '</li>' : '') + `</${l.tag}>`;
                    }
                    let top = lists[lists.length - 1];
                    if (!top || indent > top.indent) {
                        html += `<${tag} class="md-list">`;
                        lists.push({ tag, indent, open: false });
                    } else {
                        if (top.open) { html += '</li>'; top.open = false; }
                        if (top.tag !== tag) {
                            lists.pop();
                            html += `</${top.tag}><${tag} class="md-list">`;
                            lists.push({ tag, indent: top.indent, open: false });
                        }
                    }
                    top = lists[lists.length - 1];
                    // `- [ ] thing` is a checklist, not a bullet whose text
                    // happens to start with a bracket.
                    const task = item[3].match(/^\[([ xX])\]\s+(.*)$/);
                    html += task
                        ? `<li class="md-check"><span class="md-box${task[1] === ' ' ? '' : ' on'}"></span>${inline(task[2])}`
                        : `<li>${inline(item[3])}`;
                    top.open = true;
                    continue;
                }

                // An indented line under an open item is that item's own
                // continuation, not a paragraph that ends the list.
                if (open && open.open && /^\s/.test(line)) { html += ' ' + inline(line.trim()); continue; }

                flushList();
                para.push(line.trim());
            }
            flushAll();
            return html;
        }

        // The last point in `text` where a block certainly ended: a blank line
        // outside a fence, and not one sitting in the middle of a list. What
        // comes before it will not change however the answer continues, so a
        // streaming render can freeze it and stop re-parsing it.
        function stableEnd(text, from) {
            let fence = false, cut = from, pending = -1, i = from;
            for (;;) {
                const nl = text.indexOf('\n', i);
                if (nl < 0) break;            // the last line is still being written
                const line = text.slice(i, nl);
                i = nl + 1;
                if (/^\s*```/.test(line)) { fence = !fence; pending = -1; continue; }
                if (fence) continue;
                if (!line.trim()) { if (pending < 0) pending = i; continue; }
                if (pending >= 0) {
                    if (!/^\s*(?:[-*+]|\d+[.)])\s/.test(line)) cut = pending;
                    pending = -1;
                }
            }
            return cut;
        }

        // Render markdown into `el` and wire the [#N] chips to their citations.
        function setMarkdown(el, text, citations) {
            el.innerHTML = renderMarkdown(text);
            if (!citations || !citations.length) return;
            wireCitations(el, citations);
        }

        // Chips already wired are skipped: a streaming answer re-renders its
        // tail every frame, and the blocks above it must not collect a
        // listener per frame.
        function wireCitations(root, citations) {
            root.querySelectorAll('.md-cite:not([data-wired])').forEach(chip => {
                chip.dataset.wired = '1';
                const c = citations.find(x => String(x.index) === chip.dataset.cite);
                if (!c) { chip.classList.add('dead'); return; }
                chip.title = `${c.name || c.id} · ${c.file || ''}`;
                chip.addEventListener('click', () => focusCitation(c));
            });
        }

        // Once the answer lands, the individual tool rows fold into one
        // labelled group. They stay one click from the answer they produced,
        // without a wall of them sitting above every reply.
        // Both land *below* the answer, beside the sources: that's where
        // provenance belongs, and it's the part still on screen when a long
        // reply finishes — above the answer it just scrolls out of sight.
        function groupToolRows(el) {
            const rows = [...el.querySelectorAll(':scope > details.chat-tool')];
            if (!rows.length) {
                // Say so rather than showing nothing: "no tools" and "tools
                // hidden somewhere" look identical otherwise, and only one of
                // them tells you how the answer was reached.
                const none = document.createElement('div');
                none.className = 'chat-notools';
                none.textContent = 'Answered without querying the graph — it already had enough';
                el.appendChild(none);
                return;
            }
            const group = document.createElement('details');
            group.className = 'chat-tools';
            group.open = !!state.chatToolsOpen;
            group.addEventListener('toggle', () => { state.chatToolsOpen = group.open; });
            const sum = document.createElement('summary');
            // "2 tool calls" describes a mechanism; what the reader is
            // looking at is the model having gone and searched the graph on
            // its own, which is the whole difference from one-shot RAG.
            sum.textContent = rows.length === 1
                ? 'The model queried the graph once — see what it asked and got back'
                : `The model queried the graph ${rows.length} times — see what it asked and got back`;
            group.appendChild(sum);
            rows.forEach(r => { r.open = false; group.appendChild(r); });
            el.appendChild(group);
        }

        // A tool call, inspectable: the summary line is the toggle, the
        // arguments and the tool's own output sit inside. Answers that lean
        // on tools are only trustworthy if you can see what they read.
        function buildToolRow(t) {
            const row = document.createElement('details');
            row.className = 'chat-tool';
            const sum = document.createElement('summary');
            sum.innerHTML = '<span class="ct-mark"></span><span class="ct-name"></span>'
                + '<span class="ct-args"></span><span class="ct-sum"></span>';
            row.appendChild(sum);
            const pre = document.createElement('pre');
            pre.className = 'ct-detail';
            row.appendChild(pre);
            fillToolRow(row, t);
            return row;
        }

        function fillToolRow(row, t) {
            const done = t.state === 'done';
            row.classList.toggle('done', done);
            row.querySelector('.ct-mark').textContent = done ? '✓' : '▸';
            row.querySelector('.ct-name').textContent = t.name;
            row.querySelector('.ct-args').textContent = t.args || '';
            row.querySelector('.ct-sum').textContent = done ? (t.summary || 'done') : 'running…';

            // Parameters and response as separate, copyable blocks: checking an
            // answer against its evidence means reading the evidence verbatim,
            // not a paraphrase of it.
            const detail = row.querySelector('.ct-detail');
            detail.innerHTML = '';
            if (!done && !t.result) {
                const wait = document.createElement('div');
                wait.className = 'ct-wait';
                wait.textContent = 'waiting for the tool…';
                detail.appendChild(wait);
                return;
            }
            const block = (label, body, note) => {
                const wrap = document.createElement('div');
                wrap.className = 'ct-block';
                const head = document.createElement('div');
                head.className = 'ct-block-head';
                const l = document.createElement('span');
                l.textContent = label + (note ? ` · ${note}` : '');
                const copy = document.createElement('button');
                copy.type = 'button';
                copy.className = 'ct-copy';
                copy.textContent = 'copy';
                copy.addEventListener('click', async (e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    try {
                        await navigator.clipboard.writeText(body);
                        copy.textContent = 'copied';
                    } catch (err) {
                        copy.textContent = 'failed';
                    }
                    setTimeout(() => { copy.textContent = 'copy'; }, 1200);
                });
                head.append(l, copy);
                const pre = document.createElement('pre');
                pre.textContent = body;
                wrap.append(head, pre);
                return wrap;
            };
            const argsText = t.args_json && t.args_json !== '{}' ? t.args_json : '(no parameters)';
            detail.appendChild(block('Parameters sent', argsText));
            if (t.result != null) {
                const truncated = /result truncated at \d+ chars/.test(t.result);
                detail.appendChild(block(
                    'Response the model saw',
                    t.result,
                    truncated ? 'truncated' : `${t.result.length.toLocaleString()} chars`
                ));
            }
        }

        // What the turn cost, and what the same evidence costs read whole.
        //
        // The comparison is the narrow, defensible one: the files these
        // citations came from, opened in full. That is what an agent without
        // a graph pays once it has found them — not a strawman that reads the
        // repo, and not a claim about any other RAG system.
        function buildCostBox(done) {
            const cost = done.cost;
            const n = (v) => (v || 0).toLocaleString();
            const box = document.createElement('details');
            box.className = 'chat-cost';

            // Name the section before quoting a number at it. "~2,085 tokens
            // of evidence" was a statistic with no subject: it did not say it
            // was about token cost, and "evidence" did not say which of the
            // four quantities inside it meant. Lead with the exact figure the
            // endpoint charged, then what of it was retrieval, then the
            // comparison — each clause dropped when it does not apply.
            const sum = document.createElement('summary');
            const headline = ['Token cost'];
            const billedNow = (done.usage && done.usage.total_tokens) || 0;
            if (billedNow) headline.push(`${n(billedNow)} billed`);
            headline.push(`~${n(cost.sent_tokens)} of it retrieved`);
            if (cost.saved_ratio) {
                headline.push(`${cost.saved_ratio}× less than those files whole`);
            }
            sum.textContent = headline.join(' · ');
            box.appendChild(sum);

            // Where the bill went, as one proportion bar.
            //
            // An *emphasis* chart, not a categorical one: the story is that
            // the evidence is the small part, so it takes the page's accent
            // and everything else is de-emphasised grey. Three segments is
            // also the most this surface can honestly carry — the teal ramp's
            // adjacent steps measure ΔE 6.4 to normal vision, far under the
            // floor of 15, so a five-colour stack of them would be a stack
            // nobody could read. The itemised rows below are the legend and
            // the table view the contrast WARN obliges.
            const rounds0 = Math.max(done.tool_rounds || 0, 1);
            const evidence = cost.sent_tokens || 0;
            const fixed = (cost.fixed_tokens || 0) * rounds0;
            const billedTotal = (done.usage && done.usage.total_tokens) || 0;
            // What the endpoint charged beyond what we can attribute: the
            // conversation re-sent, and the gap between our estimate and its
            // tokenizer. Never negative — an estimate that overshoots is not
            // evidence of a negative quantity.
            const rest = Math.max(billedTotal - evidence - fixed, 0);
            const segs = [
                ['ev', 'This question', evidence],
                ['fx', rounds0 > 1 ? `Fixed overhead × ${rounds0}` : 'Fixed overhead', fixed],
                ['rest', 'Conversation re-sent', rest],
            ].filter(([, , v]) => v > 0);

            if (segs.length > 1) {
                const bar = document.createElement('div');
                bar.className = 'cost-bar';
                bar.setAttribute('role', 'img');
                bar.setAttribute('aria-label', segs
                    .map(([, label, v]) => `${label}: about ${n(v)} tokens`).join('; '));
                segs.forEach(([cls, label, v]) => {
                    const seg = document.createElement('span');
                    seg.className = `seg ${cls}`;
                    seg.style.flexGrow = String(v);
                    seg.title = `${label} — ~${n(v)} tokens`;
                    bar.appendChild(seg);
                });
                box.appendChild(bar);

                const key = document.createElement('div');
                key.className = 'cost-key';
                segs.forEach(([cls, label, v]) => {
                    const item = document.createElement('span');
                    item.className = 'key-item';
                    item.innerHTML = `<i class="dot ${cls}"></i>`;
                    item.appendChild(document.createTextNode(
                        `${label} ~${n(v)}`));
                    key.appendChild(item);
                });
                box.appendChild(key);
            }

            const row = (label, value, cls, key) => {
                const r = document.createElement('div');
                r.className = 'cost-row' + (cls ? ' ' + cls : '');
                const dot = key ? `<i class="dot ${key}"></i>` : '';
                r.innerHTML = `<span>${dot}${escapeHtml(label)}</span><span>${escapeHtml(value)}</span>`;
                box.appendChild(r);
            };
            const note = (text) => {
                const d = document.createElement('div');
                d.className = 'cost-note';
                d.textContent = text;
                box.appendChild(d);
            };

            // 1. What this question put in front of the model.
            // A pack of zero is not a small pack — with no seed pass there
            // was none, and a `~41` row read as a tiny retrieval instead.
            if (cost.context_tokens) row('Retrieved pack', `~${n(cost.context_tokens)}`, null, 'ev');
            row('Tool results', `~${n(cost.tool_tokens)}`, null, 'ev');
            row('Answer', `~${n(cost.answer_tokens)}`, null, 'ev');

            // 2. What every turn pays whatever you asked. Itemised because it
            // is usually *larger* than the evidence — the tool schemas alone
            // ran 9,200 tokens on this repo — and because a fixed cost paid
            // per round is the whole reason the billed total looks wrong.
            const rounds = Math.max(done.tool_rounds || 0, 1);
            if (cost.fixed_tokens) {
                row('System prompt', `~${n(cost.system_tokens)}`, 'sep', 'fx');
                if (cost.schema_tokens) row('Tool schemas', `~${n(cost.schema_tokens)}`, null, 'fx');
                if (rounds > 1) {
                    row(`Re-sent on each of ${rounds} rounds`,
                        `~${n(cost.fixed_tokens * rounds)}`, 'subtle');
                }
            }

            // 3. What the endpoint actually charged. A different question, and
            // always the larger one — which is exactly why it needs saying
            // rather than sitting unlabelled in the meta line as `tokens=`.
            const billed = done.usage && done.usage.total_tokens;
            if (billed) {
                row('Billed by the model', n(billed), 'sep');
                note('Counted by your endpoint, not estimated. The fixed cost above is paid again '
                    + 'every round, along with the conversation so far — that is most of the gap '
                    + 'between it and the evidence.');
            }

            // 4. The comparison — not part of this turn's bill at all.
            if (cost.whole_files) {
                row(`${cost.whole_files} cited file${cost.whole_files === 1 ? '' : 's'}, read whole`,
                    `~${n(cost.whole_file_tokens)}`, 'sep baseline');
                note('What the same evidence would cost opened in full — the alternative to '
                    + 'retrieval, not something this turn spent. Counts only the files these '
                    + 'citations came from.');
            }

            note('~ marks an estimate from character length; ug has no tokenizer for your endpoint.');
            return box;
        }

        // One streaming assistant turn: a status strip that narrates the
        // server's progress, the answer text as it arrives, and the citation
        // list once retrieval reports it.
        // `mount` is the Ask block's body — the bubble is appended there,
        // while the scrolling that keeps the newest text in view belongs to
        // the stream that owns every block.
        function createChatTurn(mount) {
            const list = mount;
            const scroller = document.getElementById('ask-stream');
            const el = document.createElement('div');
            el.className = 'chat-msg assistant streaming';

            const strip = document.createElement('div');
            strip.className = 'chat-progress';
            // What the strip says is the only account of the loop a reader
            // gets while it runs, and it described the pipeline that came
            // before this one: "Searching the graph…" on a turn that has not
            // searched, then "Writing the answer…" while it was still
            // deciding what to read.
            strip.innerHTML = '<span class="tour-spinner"></span><span class="cp-phase">Working out what to look up…</span>'
                + '<span class="cp-stats"></span>';
            const bodyEl = document.createElement('div');
            bodyEl.className = 'chat-body';
            const think = document.createElement('details');
            think.className = 'chat-think';
            think.hidden = true;
            think.innerHTML = '<summary>Model reasoning</summary><pre></pre>';

            // Markdown renders as it arrives, not only once the turn ends:
            // with a local model the raw text is what you read for the whole
            // minute the answer takes, and asterisks are not a reply.
            const tailEl = document.createElement('div');
            tailEl.className = 'md-tail';
            bodyEl.appendChild(tailEl);

            el.append(strip, think, bodyEl);
            list.appendChild(el);
            scroller.scrollTop = scroller.scrollHeight;

            const t0 = performance.now();
            let chars = 0, reasoningChars = 0, citeCount = 0, retrievalMs = null;
            const calls = new Map();   // in-flight tool rows, keyed by name+args
            let queries = 0;           // graph queries the model has run
            let raw = '';              // everything the model has sent
            let frozen = 0;            // raw[0..frozen) is rendered and left alone
            let frame = 0;             // pending render, if any
            let liveCites = [];
            const stats = () => {
                const secs = Math.max(0.001, (performance.now() - t0) / 1000);
                const tokens = Math.round((chars + reasoningChars) / 4);
                const bits = [];
                if (retrievalMs != null) bits.push(`${citeCount} sources · ${retrievalMs} ms`);
                if (tokens) bits.push(`${tokens.toLocaleString()} tokens · ${Math.round(tokens / secs)}/s`);
                strip.querySelector('.cp-stats').textContent = bits.join('  ·  ');
            };
            const nearBottom = () => scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60;

            // Re-parsing the whole answer on every delta is O(answer) per
            // token — quadratic over a long reply, and a full DOM rebuild
            // dozens of times a second (Agents.md §1a). Blocks that are
            // finished are rendered once and frozen; only the block still
            // being written is re-parsed, once per frame rather than once
            // per token. `md-tail` is `display: contents`, so the frozen
            // blocks stay direct children of `.chat-body`.
            const draw = () => {
                frame = 0;
                const stick = nearBottom();
                const cut = stableEnd(raw, frozen);
                if (cut > frozen) {
                    // Wired once, off-document, then moved in: a frozen block
                    // is never walked again, so the per-frame cost stays the
                    // size of the tail rather than the size of the answer.
                    const chunk = document.createElement('div');
                    chunk.innerHTML = renderMarkdown(raw.slice(frozen, cut));
                    if (liveCites.length) wireCitations(chunk, liveCites);
                    while (chunk.firstChild) bodyEl.insertBefore(chunk.firstChild, tailEl);
                    frozen = cut;
                }
                // The fence the model is still inside has no closing ``` yet;
                // render it as the code block it is about to be.
                const tail = raw.slice(frozen);
                const unclosed = (tail.match(/^\s*```/gm) || []).length % 2;
                tailEl.innerHTML = renderMarkdown(unclosed ? tail + '\n```' : tail);
                if (liveCites.length) wireCitations(tailEl, liveCites);
                if (stick) scroller.scrollTop = scroller.scrollHeight;
            };
            // Whatever replaces the body owns it from then on.
            const stopDraw = () => { if (frame) { cancelAnimationFrame(frame); frame = 0; } };

            return {
                phase(text) { strip.querySelector('.cp-phase').textContent = text; },
                context(cites, ms) {
                    citeCount = cites.length;
                    liveCites = cites || [];
                    retrievalMs = ms;
                    strip.querySelector('.cp-phase').textContent = cites.length
                        ? 'Reading what it found…'
                        : 'Working out what to look up…';
                    stats();
                },
                tool(t) {
                    if (t.state !== 'done') queries += 1;
                    strip.querySelector('.cp-phase').textContent = t.state === 'done'
                        ? `${t.name} → ${t.summary || 'done'}`
                        : `Querying the graph · ${t.name}${queries > 1 ? ` (${queries})` : ''}…`;
                    if (t.state === 'done') {
                        // Upgrade the row we opened when the call started, so
                        // the user can read exactly what was asked and returned.
                        const open = calls.get(t.name + '|' + t.args);
                        if (open) { fillToolRow(open, t); calls.delete(t.name + '|' + t.args); }
                        else el.insertBefore(buildToolRow(t), bodyEl);
                    } else {
                        const row = buildToolRow(t);
                        calls.set(t.name + '|' + t.args, row);
                        el.insertBefore(row, bodyEl);
                    }
                    if (nearBottom()) scroller.scrollTop = scroller.scrollHeight;
                },
                // More sources, same turn.
                cites(list) {
                    liveCites = list || [];
                    citeCount = liveCites.length;
                    stats();
                },
                reasoning(text) {
                    reasoningChars += text.length;
                    think.hidden = false;
                    think.querySelector('pre').textContent += text;
                    strip.querySelector('.cp-phase').textContent = 'Thinking…';
                    stats();
                },
                append(text) {
                    chars += text.length;
                    raw += text;
                    if (!frame) frame = requestAnimationFrame(draw);
                    stats();
                },
                finish(text, cites, done, totalMs) {
                    el.classList.remove('streaming');
                    stopDraw();
                    strip.remove();
                    // The streamed render guessed at a half-written tail; the
                    // finished text is authoritative, so render it whole.
                    if (text) setMarkdown(bodyEl, text, cites);
                    else bodyEl.textContent = '(no answer)';
                    // The summary goes first: it is the one line that says
                    // what the whole turn cost, and reading it should not mean
                    // scrolling past every tool call to reach it.
                    if (done && done.cost) el.appendChild(buildCostBox(done));
                    groupToolRows(el);
                    if (cites && cites.length) el.appendChild(buildCitationBox(cites));
                    const bits = [];
                    if (done && done.retrieval_ms != null) bits.push(`retrieval=${done.retrieval_ms}ms`);
                    if (done && done.completion_ms != null) bits.push(`completion=${done.completion_ms}ms`);
                    else if (totalMs != null) bits.push(`total=${totalMs}ms`);

                    if (done && done.tool_calls) {
                        bits.push(done.hit_round_cap
                            ? `tools=${done.tool_calls} in ${done.tool_rounds} rounds (hit the cap)`
                            : `tools=${done.tool_calls}`);
                    }
                    if (done && done.dest) bits.push(`dest=${done.dest}`);
                    if (done && done.chat_model) bits.push(`model=${done.chat_model}`);
                    if (bits.length) {
                        const m = document.createElement('div');
                        m.className = 'chat-meta';
                        m.textContent = bits.join(' · ');
                        el.appendChild(m);
                    }
                    scroller.scrollTop = scroller.scrollHeight;
                },
                fail(msg) {
                    el.classList.remove('streaming');
                    stopDraw();
                    el.classList.add('error');
                    strip.remove();
                    bodyEl.textContent = `Error: ${msg}`;
                },
                // Configured, but nothing answered at the other end.
                unreachable(endpoint) {
                    el.classList.remove('streaming');
                    stopDraw();
                    strip.remove();
                    bodyEl.innerHTML = '';
                    const box = document.createElement('div');
                    box.className = 'cap-banner warn';
                    const head = document.createElement('strong');
                    head.textContent = 'The model endpoint isn\'t responding.';
                    const p1 = document.createElement('span');
                    p1.textContent = endpoint
                        ? `Nothing answered at ${endpoint}. Start your local model server, or point UltraGraph somewhere else — everything else here keeps working in the meantime.`
                        : 'Start your local model server, or point UltraGraph somewhere else — everything else here keeps working in the meantime.';
                    const cta = document.createElement('button');
                    cta.type = 'button';
                    cta.className = 'cap-cta';
                    cta.textContent = 'Check model settings';
                    cta.addEventListener('click', () => openSettings());
                    box.append(head, p1, cta);
                    bodyEl.appendChild(box);
                },
            };
        }

