        // ─── Chat (RAG) panel ──────────────────────────────

        function wireChatPanel() {
            const input = document.getElementById('chat-input');
            const sendBtn = document.getElementById('chat-send');
            const resetBtn = document.getElementById('chat-reset');
            const settings = document.getElementById('chat-settings');
            const toggle = document.getElementById('chat-settings-toggle');

            sendBtn.addEventListener('click', runChatTurn);
            resetBtn.addEventListener('click', resetChat);
            input.addEventListener('keydown', e => {
                if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                    e.preventDefault();
                    runChatTurn();
                }
            });
            toggle.addEventListener('click', () => {
                const open = settings.classList.toggle('open');
                toggle.textContent = (open ? '▾' : '▸') + ' Advanced';
            });
        }

        function resetChat() {
            state.chatHistory = [];
            const list = document.getElementById('chat-messages');
            list.innerHTML = '';
            const status = document.getElementById('chat-status');
            status.classList.remove('error');
            status.textContent = '';
        }

        function appendChatMessage(role, text, opts = {}) {
            const list = document.getElementById('chat-messages');
            const el = document.createElement('div');
            el.className = `chat-msg ${role}${opts.pending ? ' pending' : ''}${opts.error ? ' error' : ''}`;
            el.textContent = text;
            if (opts.citations && opts.citations.length) {
                const cites = document.createElement('div');
                cites.className = 'chat-citations';
                opts.citations.forEach(c => {
                    const a = document.createElement('div');
                    a.className = 'chat-cite';
                    const lineLabel = c.start_line
                        ? `:${c.start_line}${c.end_line && c.end_line !== c.start_line ? '–' + c.end_line : ''}`
                        : '';
                    const fileLabel = c.file || '<unknown>';
                    a.innerHTML = nodeIconSvg(c.node_type || 'Default', 'cite-icon')
                        + escapeHtml(`[#${c.index}] ${c.name || c.id} · ${c.node_type || '?'} · ${fileLabel}${lineLabel}`);
                    a.title = c.description ? c.description.slice(0, 200) : (c.id || c.name || '');
                    a.addEventListener('click', () => focusCitation(c));
                    cites.appendChild(a);
                });
                el.appendChild(cites);
            }
            if (opts.meta) {
                const m = document.createElement('div');
                m.className = 'chat-meta';
                m.textContent = opts.meta;
                el.appendChild(m);
            }
            list.appendChild(el);
            list.scrollTop = list.scrollHeight;
            return el;
        }

        // Citations, collapsed by default — they're provenance, not the
        // answer, and a dozen of them push the reply off screen.
        function buildCitationBox(cites) {
            const box = document.createElement('details');
            box.className = 'chat-citations';
            box.open = !!state.chatCitesOpen;
            const sum = document.createElement('summary');
            sum.textContent = `${cites.length} source${cites.length === 1 ? '' : 's'}`;
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
                    + escapeHtml(`[#${c.index}] ${c.name || c.id} · ${c.node_type || '?'} · ${c.file || '<unknown>'}${lineLabel}`);
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
                const status = document.getElementById('chat-status');
                status.classList.remove('error');
                status.textContent = `Node "${c.id}" not in loaded graph.json.`;
            }
        }

        async function runChatTurn() {
            const inputEl = document.getElementById('chat-input');
            const sendBtn = document.getElementById('chat-send');
            const statusEl = document.getElementById('chat-status');
            const query = inputEl.value.trim();

            statusEl.classList.remove('error');
            if (!query) {
                statusEl.textContent = 'Enter a question to send.';
                return;
            }
            if (state.chatInFlight) return;

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

            appendChatMessage('user', query);
            inputEl.value = '';
            // The answer bubble exists from the first moment and fills in as
            // tokens arrive, with a live account of what the server is doing
            // above it — a chat that sits silent for a minute reads as broken.
            const turn = createChatTurn();

            state.chatInFlight = true;
            sendBtn.disabled = true;
            statusEl.textContent = 'Retrieving context…';

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
                                    ? 'Searching the graph…' : payload.phase);
                                break;
                            case 'context':
                                cites = payload.citations || [];
                                turn.context(cites, payload.retrieval_ms);
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
                    turn.append(answer);
                    done = data;
                }
                if (streamErr) {
                    // A dead endpoint isn't the user's mistake to decode —
                    // say what's wrong in plain words and offer the fix.
                    if (streamErrKind === 'llm_unreachable') {
                        turn.unreachable(streamErrEndpoint);
                        statusEl.classList.add('error');
                        statusEl.textContent = 'No answer — the model endpoint isn\'t responding.';
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
                statusEl.textContent = `${cites.length} citation${cites.length === 1 ? '' : 's'} · ${totalMs} ms total`;
            } catch (err) {
                turn.fail(err.message || err);
                statusEl.classList.add('error');
                statusEl.textContent = `Chat failed: ${err.message || err}`;
                console.error(err);
            } finally {
                state.chatInFlight = false;
                sendBtn.disabled = false;
                inputEl.focus();
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
            section('Retrieval');
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
            [
                ['store', r.backend || '—'],
                ['ranking', (r.strategy || '').toUpperCase()],
                ['k', dflt.k],
                ['hops', dflt.hops],
                ['context budget', dflt.max_context_chars ? `${Math.round(dflt.max_context_chars / 1000)}k chars` : '—'],
            ].forEach(([k, v]) => {
                if (v == null) return;
                const chip = document.createElement('span');
                chip.innerHTML = '<b></b><i></i>';
                chip.querySelector('b').textContent = k;
                chip.querySelector('i').textContent = v;
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
            const fenced = text.replace(/```([\w+-]*)\n([\s\S]*?)```/g, (_, lang, code) => {
                blocks.push({ lang: lang || '', code: code.replace(/\n$/, '') });
                return ` CODE${blocks.length - 1} `;
            });

            const lines = fenced.split('\n');
            let html = '';
            let list = null;          // 'ul' | 'ol'
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
            const flushList = () => { if (list) { html += `</${list}>`; list = null; } };
            const flushQuote = () => {
                if (quote.length) { html += `<blockquote>${inline(quote.join(' '))}</blockquote>`; quote = []; }
            };
            const flushAll = () => { flushPara(); flushList(); flushQuote(); };

            for (const raw of lines) {
                const line = raw.replace(/\s+$/, '');
                const codeRef = line.match(/^ CODE(\d+) $/);
                if (codeRef) {
                    flushAll();
                    const b = blocks[+codeRef[1]];
                    html += `<pre class="md-code"${b.lang ? ` data-lang="${escapeHtml(b.lang)}"` : ''}>`
                        + `<code>${escapeHtml(b.code)}</code></pre>`;
                    continue;
                }
                if (!line.trim()) { flushAll(); continue; }

                const heading = line.match(/^(#{1,6})\s+(.*)$/);
                if (heading) {
                    flushAll();
                    const level = Math.min(6, heading[1].length);
                    html += `<h${level} class="md-h">${inline(heading[2])}</h${level}>`;
                    continue;
                }
                if (/^(-{3,}|\*{3,}|_{3,})$/.test(line.trim())) { flushAll(); html += '<hr>'; continue; }

                const quoted = line.match(/^>\s?(.*)$/);
                if (quoted) { flushPara(); flushList(); quote.push(quoted[1]); continue; }
                flushQuote();

                const bullet = line.match(/^\s*[-*+]\s+(.*)$/);
                const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/);
                if (bullet || numbered) {
                    flushPara();
                    const want = bullet ? 'ul' : 'ol';
                    if (list !== want) { flushList(); html += `<${want} class="md-list">`; list = want; }
                    html += `<li>${inline((bullet || numbered)[1])}</li>`;
                    continue;
                }
                flushList();
                para.push(line.trim());
            }
            flushAll();
            return html;
        }

        // Render markdown into `el` and wire the [#N] chips to their citations.
        function setMarkdown(el, text, citations) {
            el.innerHTML = renderMarkdown(text);
            if (!citations || !citations.length) return;
            el.querySelectorAll('.md-cite').forEach(chip => {
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
                none.textContent = 'No tool calls — answered from the retrieved sources alone';
                el.appendChild(none);
                return;
            }
            const group = document.createElement('details');
            group.className = 'chat-tools';
            group.open = !!state.chatToolsOpen;
            group.addEventListener('toggle', () => { state.chatToolsOpen = group.open; });
            const sum = document.createElement('summary');
            sum.textContent = `${rows.length} tool call${rows.length === 1 ? '' : 's'} — inspect parameters and responses`;
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

        // One streaming assistant turn: a status strip that narrates the
        // server's progress, the answer text as it arrives, and the citation
        // list once retrieval reports it.
        function createChatTurn() {
            const list = document.getElementById('chat-messages');
            const el = document.createElement('div');
            el.className = 'chat-msg assistant streaming';

            const strip = document.createElement('div');
            strip.className = 'chat-progress';
            strip.innerHTML = '<span class="tour-spinner"></span><span class="cp-phase">Searching the graph…</span>'
                + '<span class="cp-stats"></span>';
            const bodyEl = document.createElement('div');
            bodyEl.className = 'chat-body';
            const think = document.createElement('details');
            think.className = 'chat-think';
            think.hidden = true;
            think.innerHTML = '<summary>Model reasoning</summary><pre></pre>';

            el.append(strip, think, bodyEl);
            list.appendChild(el);
            list.scrollTop = list.scrollHeight;

            const t0 = performance.now();
            let chars = 0, reasoningChars = 0, citeCount = 0, retrievalMs = null;
            const calls = new Map();   // in-flight tool rows, keyed by name+args
            const stats = () => {
                const secs = Math.max(0.001, (performance.now() - t0) / 1000);
                const tokens = Math.round((chars + reasoningChars) / 4);
                const bits = [];
                if (retrievalMs != null) bits.push(`${citeCount} sources · ${retrievalMs} ms`);
                if (tokens) bits.push(`${tokens.toLocaleString()} tokens · ${Math.round(tokens / secs)}/s`);
                strip.querySelector('.cp-stats').textContent = bits.join('  ·  ');
            };
            const nearBottom = () => list.scrollHeight - list.scrollTop - list.clientHeight < 60;

            return {
                phase(text) { strip.querySelector('.cp-phase').textContent = text; },
                context(cites, ms) {
                    citeCount = cites.length;
                    retrievalMs = ms;
                    strip.querySelector('.cp-phase').textContent = 'Writing the answer…';
                    stats();
                },
                tool(t) {
                    strip.querySelector('.cp-phase').textContent =
                        t.state === 'done' ? `${t.name} → ${t.summary || 'done'}` : `Calling ${t.name}…`;
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
                    if (nearBottom()) list.scrollTop = list.scrollHeight;
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
                    const stick = nearBottom();
                    bodyEl.textContent += text;
                    stats();
                    if (stick) list.scrollTop = list.scrollHeight;
                },
                finish(text, cites, done, totalMs) {
                    el.classList.remove('streaming');
                    strip.remove();
                    // Streaming shows raw text (markdown can't be parsed
                    // half-written); the finished answer gets rendered.
                    if (text) setMarkdown(bodyEl, text, cites);
                    else bodyEl.textContent = '(no answer)';
                    groupToolRows(el);
                    if (cites && cites.length) el.appendChild(buildCitationBox(cites));
                    const bits = [];
                    if (done && done.retrieval_ms != null) bits.push(`retrieval=${done.retrieval_ms}ms`);
                    if (done && done.completion_ms != null) bits.push(`completion=${done.completion_ms}ms`);
                    else if (totalMs != null) bits.push(`total=${totalMs}ms`);
                    if (done && done.usage && done.usage.total_tokens) bits.push(`tokens=${done.usage.total_tokens}`);
                    if (done && done.tool_calls) bits.push(`tools=${done.tool_calls}`);
                    if (done && done.dest) bits.push(`dest=${done.dest}`);
                    if (done && done.chat_model) bits.push(`model=${done.chat_model}`);
                    if (bits.length) {
                        const m = document.createElement('div');
                        m.className = 'chat-meta';
                        m.textContent = bits.join(' · ');
                        el.appendChild(m);
                    }
                    list.scrollTop = list.scrollHeight;
                },
                fail(msg) {
                    el.classList.remove('streaming');
                    el.classList.add('error');
                    strip.remove();
                    bodyEl.textContent = `Error: ${msg}`;
                },
                // Configured, but nothing answered at the other end.
                unreachable(endpoint) {
                    el.classList.remove('streaming');
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

        function renderSemanticHits(container, hits, mode) {
            container.innerHTML = '';
            hits.forEach(h => {
                const card = document.createElement('div');
                card.className = 'sem-hit';
                const scoreText = h.score != null && Number.isFinite(h.score)
                    ? h.score.toFixed(3)
                    : '';
                const lineLabel = h.start_line
                    ? `L${h.start_line}${h.end_line && h.end_line !== h.start_line ? '–' + h.end_line : ''}`
                    : '';
                const meta = [h.node_type, h.file, lineLabel].filter(Boolean).join(' · ');
                // Only the hybrid pipeline tags how each item was reached;
                // pure-semantic hits have no `matched_by`.
                const mech = (mode === 'hybrid' && h.matched_by) ? h.matched_by : '';
                const mechBadge = mech
                    ? `<span class="sem-match sem-match-${mech}" title="How this result was reached: ${mech === 'semantic' ? 'dense vector match' : mech === 'keyword' ? 'sparse/keyword match' : 'graph walk from a seed'}">${mech}</span>`
                    : '';

                card.innerHTML = `
                    <div class="sem-hit-head">
                        ${nodeIconSvg(h.node_type || 'Default')}
                        <span class="name" title="${escapeHtml(h.id || h.name)}">${escapeHtml(truncateName(h.name || h.id))}</span>
                        ${mechBadge}
                        <span class="score">${escapeHtml(scoreText)}</span>
                    </div>
                    ${meta ? `<div class="sem-hit-meta" title="${escapeHtml(meta)}">${escapeHtml(meta)}</div>` : ''}
                `;
                card.querySelector('.sem-hit-head').addEventListener('click', (ev) => {
                    const local = state.nodeById ? state.nodeById.get(h.id) : null;
                    if (local) {
                        // ⌘/Ctrl adds to the canvas instead of replacing it.
                        if (ev.metaKey || ev.ctrlKey) state._viewMerge = true;
                        handleClick(null, local);
                        focusNode(local);
                    } else {
                        // DB has the node but graph.json doesn't — surface a small notice.
                        const status = document.getElementById('sem-status');
                        status.classList.remove('error');
                        status.textContent = `Node "${h.id}" not in loaded graph.json.`;
                    }
                });
                container.appendChild(card);
            });

            // Offer the whole hit set in one go: solo mode draws them fresh,
            // normal mode dims the rest and frames them. Hits the loaded graph
            // doesn't contain can't be drawn, so they don't count.
            state.semMatches = hits
                .map(h => h.id)
                .filter(id => state.nodeById && state.nodeById.has(id));
            syncPlotAllButton(document.getElementById('sem-plot-all'), state.semMatches);
        }

