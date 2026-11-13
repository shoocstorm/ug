// The static demo's in-browser model loop, checked under `node`.
//
// `ug demo` ships a page whose model panel normally talks to a server:
// /api/llm/local/{status,attach,events,jobs/…} and /api/chat. On a static
// host all of those are answered by `native/src/vis/demo-shim.js`, which
// also bridges the tab's own wllama into the SSE job feed. Two things can
// break with no browser anywhere to say so:
//
//   * the wire shapes drift from what `js/28-local-llm.js` and `js/06-chat.js`
//     parse (they are written for the real server, and only work here by
//     mirroring it exactly), or
//   * the arithmetic stops fitting the attached model's window — a pack that
//     overruns n_ctx is a `kv_cache_full` error a visitor can do nothing
//     about.
//
// So this file checks both, without booting the graph page (Agents.md §10r):
// the pure functions are lifted out of the real shim with a string slice —
// they cannot pass against a copy that has drifted — and then the whole shim
// is evaluated against a stub DOM and driven through one full turn:
// status → events → attach → /api/chat → job → deltas → result → SSE out.
//
// argv: <demo-shim.js>

import fs from 'node:fs';
import assert from 'node:assert/strict';

const [shimPath] = process.argv.slice(2);
if (!shimPath) {
    console.error('usage: node demo_llm.mjs <demo-shim.js>');
    process.exit(2);
}
const src = fs.readFileSync(shimPath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

// ── lift the pure half ────────────────────────────────────────────────

// The shim's functions live at 4-space indent inside its IIFE; the closing
// brace at that depth ends each one.
function lift(source, name, indent = 4) {
    const pad = ' '.repeat(indent);
    const start = source.indexOf('\n' + pad + 'function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found at indent ${indent} — renamed?`);
    const end = source.indexOf('\n' + pad + '}', start);
    if (end < 0) throw new Error(`${name} has no closing brace at indent ${indent}`);
    return source.slice(start, end + pad.length + 2);
}

// A const whose initializer spans lines. Everything belonging to the
// initializer is indented deeper than the declaration; the initializer is
// over at the first line back at declaration depth that *starts a new
// thing* — a line at depth that closes one (`)`, `]`, `}`) still belongs
// to it.
function liftConst(source, name, indent = 4) {
    const pad = ' '.repeat(indent);
    const start = source.indexOf('\n' + pad + 'const ' + name + ' =');
    if (start < 0) throw new Error(`${name} not found`);
    let nl = source.indexOf('\n', start + 1);   // end of the `const NAME =` line
    for (;;) {
        const next = source.indexOf('\n', nl + 1);
        if (next < 0) throw new Error(`${name} has no terminating line`);
        const line = source.slice(nl + 1, next);
        const atDepth = line.startsWith(pad) && !line.startsWith(pad + ' ');
        const closes = /^\s*[)\]}]/.test(line);
        if (atDepth && !closes) {
            return source.slice(start + 1, nl + 1);
        }
        nl = next;
    }
}

const lifted = [
    liftConst(src, 'DEMO_SYSTEM_PROMPT'),
    liftConst(src, 'DEMO_STOPWORDS'),
    lift(src, 'demoTokens'),
    lift(src, 'demoQueryTokens'),
    lift(src, 'demoPlan'),
    lift(src, 'demoSse'),
    lift(src, 'demoRetrieve'),
    lift(src, 'demoRenderPack'),
].join('\n');
const unit = new Function(lifted + '\nreturn { demoPlan, demoSse, demoRetrieve, demoRenderPack, demoQueryTokens, DEMO_SYSTEM_PROMPT };')();
const { demoPlan, demoSse, demoRetrieve, demoRenderPack, demoQueryTokens, DEMO_SYSTEM_PROMPT } = unit;

// ── the plan has to fit the window it is planning for ─────────────────
//
// The whole point of the arithmetic (local_llm.rs §11q): system + pack +
// overhead + answer tokens ≤ n_ctx, or the generation dies with
// kv_cache_full on the visitor's machine.
{
    let fits = true, clamped = true;
    for (const nCtx of [2048, 4096, 8192, 16384, 32768, 65536]) {
        const p = demoPlan(nCtx);
        const promptTokens = Math.ceil((DEMO_SYSTEM_PROMPT.length + p.context_chars) / 3);
        if (promptTokens + p.completion_tokens + 512 > nCtx) fits = false;
        if (p.completion_tokens !== Math.min(Math.max(Math.floor(nCtx / 4), 256), 4096)) clamped = false;
    }
    must('plan: system + pack + overhead + answer fits every window', fits);
    must('plan: completion is n_ctx/4 clamped to [256, 4096]', clamped);
    must('plan: ui limits follow the char budget', demoPlan(8192).ui.chat_k === Math.floor(demoPlan(8192).context_chars / 900));
    must('plan: a tiny window still gets a usable pack', demoPlan(2048).context_chars >= 2000);
}

// ── retrieval ─────────────────────────────────────────────────────────
const mkNode = (over) => Object.assign({
    id: '', name: '', type: 'Function', file: '', start: 0, end: 0, doc: '',
    lname: '', lq: '', lfile: '', ldoc: '',
}, over);
const idx = (order) => ({
    nodes: order.map(n => n),
});
const nFoo = mkNode({ id: 'function:a.rs:foo', name: 'renderNode', file: 'a.rs', lname: 'rendernode', lq: 'crate::rendernode', lfile: 'a.rs', ldoc: 'render a node' });
const nBar = mkNode({ id: 'function:b.rs:bar', name: 'renderNode', file: 'b.rs', lname: 'rendernode', lq: 'crate::rendernode', lfile: 'b.rs', ldoc: '' });
const nDoc = mkNode({ id: 'concept:chat.md', name: 'Chat', type: 'Concept', lname: 'chat', lq: 'chat.md', lfile: 'chat.md', ldoc: 'how rendering works: the renderer' });

{
    must('tokens: stopwords and one-letter tokens drop out',
        JSON.stringify(demoQueryTokens('how does the a render node?')) === JSON.stringify(['render', 'node']));
    const one = demoRetrieve(idx([nFoo, nBar, nDoc]), 'render node', 5);
    const two = demoRetrieve(idx([nDoc, nBar, nFoo]), 'render node', 5);
    must('retrieve: ties break on id, not input order', JSON.stringify(one.map(n => n.id)) === JSON.stringify(two.map(n => n.id)));
    must('retrieve: name matches outrank doc matches', one[0].id === 'function:a.rs:foo');
    must('retrieve: k is respected', demoRetrieve(idx([nFoo, nBar, nDoc]), 'render node', 1).length === 1);
    must('retrieve: no meaningful tokens, no hits', demoRetrieve(idx([nFoo]), 'how does the', 5).length === 0);
}

// ── the pack ──────────────────────────────────────────────────────────
{
    const { text, citations } = demoRenderPack([nFoo, nBar, nDoc], 10000);
    must('pack: items are numbered [#1]… in order', text.startsWith('[#1] renderNode (Function) — a.rs') && text.includes('[#2] renderNode (Function) — b.rs'));
    must('pack: says how each item was reached', text.includes('keyword match'));
    must('pack: only rendered items become citations', citations.length === 3 && citations[0].index === 1 && citations[2].id === 'concept:chat.md');
    must('pack: citations carry matched_by for the provenance chip', citations.every(c => c.matched_by === 'keyword'));

    // A budget that admits one item must admit that item — and a budget that
    // admits several must never overshoot (Agents.md §10o: a cap that can
    // only reject is not a cap).
    const tight = demoRenderPack([nFoo, nBar, nDoc], 60);
    must('pack: a small budget keeps the first item', tight.citations.length === 1);
    const mid = demoRenderPack([nFoo, nBar, nDoc], 200);
    const chars = mid.text.length;
    must('pack: a multi-item pack fits inside the budget', mid.citations.length >= 1 && chars <= 200,
        `text was ${chars} chars for a 200 budget`);
}

// ── the SSE wire format readSseStream parses ──────────────────────────
{
    const block = demoSse('delta', { content: 'hi\nthere' });
    must('sse: block shape is event:/data: plus a blank line', /^event: delta\ndata: \{.*\}\n\n$/.test(block));
    let parsed;
    try { parsed = JSON.parse(block.slice(block.indexOf('data: ') + 6).trim()); } catch (e) { parsed = null; }
    must('sse: the data line is one JSON payload', parsed && parsed.content === 'hi\nthere');
}

// ── the whole loop, against the real shim ─────────────────────────────
//
// The shim only touches window/document at its edges, and every DOM probe
// is null-guarded, so a stub window is enough to run it for real. Node's
// own fetch Response/ReadableStream stand in for the browser's.
{
    const graphFixture = {
        nodes: [
            { id: 'function:a.rs:render', node_type: 'Function', name: 'render', file: 'a.rs', startLine: 1, endLine: 9, qualifiedName: 'crate::render', docstring: 'Render the graph.' },
            { id: 'folder:.', node_type: 'Folder', name: '.', file: '.' },
        ],
        edges: [],
    };
    const win = {
        location: { href: 'https://x.test/demo/' },
        // The "real" fetch the shim keeps for static files: graph.json.
        fetch: () => Promise.resolve(new Response(JSON.stringify(graphFixture), { status: 200, headers: { 'Content-Type': 'application/json' } })),
    };
    const document = {
        readyState: 'loading',
        addEventListener: () => {},
        getElementById: () => null,
        querySelectorAll: () => [],
        createElement: () => ({ style: {}, classList: { add() {}, remove() {}, toggle() {} }, appendChild() {}, addEventListener() {}, querySelector: () => ({ addEventListener() {} }), set innerHTML(v) {}, set textContent(v) {} }),
        head: { appendChild() {} },
        body: { appendChild() {} },
    };
    const localStorage = { getItem: () => null, setItem: () => {} };
    globalThis.window = win;
    globalThis.document = document;
    globalThis.localStorage = localStorage;

    win.UG_DEMO = { llm: { version: '3.6.1', js: 'wllama/3.6.1/wllama.js', wasm: 'wllama/3.6.1/wllama.wasm' } };
    new Function('window', 'document', 'localStorage', src)(win, document, localStorage);

    const out = [];
    const step = (label, cond) => out.push([label, cond]);
    const fetch = (u, i) => win.fetch(u, i);

    // status: supported, runtime resolved against the page
    const st0 = await (await fetch('/api/llm/local/status')).json();
    step('status: supported with runtime resolved absolute',
        st0.supported === true && st0.runtime.js === 'https://x.test/demo/wllama/3.6.1/wllama.js');

    // the job feed
    const es = new win.EventSource('/api/llm/local/events?client=c1');
    const jobs = [];
    es.addEventListener('job', (e) => jobs.push(JSON.parse(e.data)));
    await new Promise((r) => es.addEventListener('open', r));

    // attach → capabilities flip
    const st1 = await (await fetch('/api/llm/local/attach', {
        method: 'POST',
        body: JSON.stringify({ client_id: 'c1', model: 'smollm2-135m', label: 'SmolLM2 135M', n_ctx: 4096, supports_tools: false, backend: 'wasm' }),
    })).json();
    step('attach: replies with the status payload, tools honestly off',
        st1.attached && st1.attached.label === 'SmolLM2 135M' && st1.attached.tools === 0 && st1.attached.ui_limits.chat_k >= 2);
    const caps = await (await fetch('/api/capabilities')).json();
    step('capabilities: chat becomes ready, in-browser, when a model attaches',
        caps.chat_ready === true && caps.chat.in_browser === true && caps.search_ready === false);
    const capsPre = await (await fetch('/api/llm/local/status')).json();
    step('status: carries the ui caps the panel clamps inputs with', typeof capsPre.attached.context_chars === 'number');

    // one full /api/chat turn with a scripted "model"
    const gotJob = new Promise((r) => es.addEventListener('job', (e) => r(JSON.parse(e.data))));
    const chatP = fetch('/api/chat', {
        method: 'POST',
        body: JSON.stringify({ query: 'how do I render the graph?', k: 5, history: [], stream: true }),
    });
    const job = await gotJob;
    const req = job.request;
    step('job: messages are system, pack, user', req.messages.length === 3
        && req.messages[0].role === 'system' && req.messages[2].role === 'user');
    step('job: the pack is a numbered context block',
        req.messages[1].content.startsWith('Retrieved context (cite as [#N]):')
        && req.messages[1].content.includes('[#1] render (Function) — a.rs:1-9 · keyword match'));
    step('job: max_tokens follows the window plan', req.max_tokens === Math.min(Math.max(Math.floor(4096 / 4), 256), 4096));

    await fetch(`/api/llm/local/jobs/${job.id}/delta`, { method: 'POST', body: JSON.stringify({ content: 'It calls ' }) });
    await fetch(`/api/llm/local/jobs/${job.id}/delta`, { method: 'POST', body: JSON.stringify({ content: 'render(). [#1]' }) });
    await fetch(`/api/llm/local/jobs/${job.id}/result`, { method: 'POST', body: JSON.stringify({ content: 'It calls render(). [#1]', finish_reason: 'stop' }) });

    const res = await chatP;
    step('chat: replies as an SSE stream',
        (res.headers.get('content-type') || '').includes('text/event-stream'));
    const text = await res.text();
    const names = [...text.matchAll(/^event: (\w+)$/gm)].map((m) => m[1]);
    step('chat: phases arrive in order', names.join(',') === 'phase,context,delta,delta,done');
    const done = JSON.parse(text.slice(text.indexOf('event: done')).split('\n')[1].slice(6));
    step('chat: done carries the answer and the model label',
        done.answer === 'It calls render(). [#1]' && done.chat_model === 'SmolLM2 135M');
    const ctx = JSON.parse(text.slice(text.indexOf('event: context')).split('\n')[1].slice(6));
    step('chat: citations cite only rendered items, keyword-matched',
        ctx.citations.length === 1 && ctx.citations[0].id === 'function:a.rs:render' && ctx.citations[0].matched_by === 'keyword');

    // detach → chat is off again, and a turn without a model says why
    await fetch('/api/llm/local/detach', { method: 'POST', body: JSON.stringify({ client_id: 'c1' }) });
    const caps2 = await (await fetch('/api/capabilities')).json();
    step('detach: chat readiness goes away', caps2.chat_ready === false);
    const no = await fetch('/api/chat', { method: 'POST', body: JSON.stringify({ query: 'hi', stream: true }) });
    step('chat: without a model the reply is a 503 that says the fix',
        no.status === 503 && ((await no.json()).error || '').includes('model'));

    for (const [label, cond] of out) must(label, cond);
}

console.log(failures ? `\n${failures} FAILURES` : '\nall ok');
process.exit(failures ? 1 : 0);
