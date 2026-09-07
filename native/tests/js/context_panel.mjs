// The Context tab's pure halves, run under `node`.
//
// Two things in `src/vis/js/` decide whether a context pack is shown
// correctly, and neither needs a browser to be wrong:
//
//   1. `24-context.js` turns the `/api/tools/context` envelope into the
//      panel's markup. Get a field name wrong and the tab renders an empty
//      pack — which looks exactly like a symbol with no callers.
//   2. `10-render-core.js`'s context tier decides what the canvas is told.
//      Get its precedence wrong and it recolours a graph out from under a
//      running walk or tour.
//
// Both are pure functions of `state` and the response, so both are checkable
// here. The functions are read out of the real parts rather than transcribed,
// so this cannot pass against a copy that has drifted from what ships.
//
// argv: <24-context.js> <10-render-core.js> <00-preamble.js> [<a real /api/tools/context response>]

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [ctxPath, corePath, preamblePath, samplePath] = process.argv.slice(2);
const ctxSrc = fs.readFileSync(ctxPath, 'utf8');
const coreSrc = fs.readFileSync(corePath, 'utf8');
const preSrc = fs.readFileSync(preamblePath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

// Lift one `function name(...) { ... }` out of a concatenated part. The parts
// are indented to a fixed depth inside the page's module, so the closing brace
// at that depth ends the function.
function lift(src, name) {
    const start = src.indexOf('\n        function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found — has it been renamed?`);
    const end = src.indexOf('\n        }', start);
    if (end < 0) throw new Error(`${name} has no closing brace at part depth`);
    return src.slice(start, end + 10);
}
function liftConst(src, decl) {
    const start = src.indexOf('\n        const ' + decl);
    if (start < 0) throw new Error(`${decl} not found`);
    const end = src.indexOf('\n        };', start);
    return src.slice(start, end + 11);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-ctx-'));
const write = (name, body) => {
    const p = path.join(tmp, name);
    fs.writeFileSync(p, body);
    return p;
};

const ROLE_TABLE = liftConst(preSrc, 'CTX_ROLE = {');
const ROLE_ORDER = preSrc.match(/const CTX_ROLE_ORDER = .*;/)[0];

// ── 1. Rendering ─────────────────────────────────────────────────────────────

const renderMod = write('render.mjs', `
    const escapeHtml = t => String(t).replace(/[&<>"']/g, c =>
        ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
    const truncateName = n => String(n);
    const nodeIconSvg = g => '<svg class="node-icon" data-g="' + g + '"></svg>';
    ${ROLE_TABLE}
    ${ROLE_ORDER}
    ${lift(ctxSrc, 'contextSummaryHtml')}
    ${lift(ctxSrc, 'contextBodyHtml')}
    ${lift(ctxSrc, 'contextItemHtml')}
    const state = { ctxMaxChars: 12000 };
    export { contextSummaryHtml, contextBodyHtml, CTX_ROLE, CTX_ROLE_ORDER };
`);
const R = await import(renderMod);

// A pack shaped exactly like the wire format: SymbolRef is flattened into the
// item, so id/name/node_type/file/start_line sit beside role and why.
const pack = {
    query: 'staleness',
    target: { id: 'fn:p.rs:staleness', name: 'staleness', node_type: 'Function', file: 'p.rs', start_line: 434, end_line: 528 },
    max_chars: 4000,
    used_chars: 3936,
    items: [
        { role: 'target', why: 'the symbol you asked about', id: 'fn:p.rs:staleness', name: 'staleness', node_type: 'Function', file: 'p.rs', start_line: 434, end_line: 528, code: 'fn staleness() {}', truncated_chars: 1707 },
        { role: 'caller', why: 'this —Calls→ target', id: 'fn:s.rs:announce', name: 'announce', node_type: 'Function', file: 's.rs', start_line: 127, call_sites: [{ line: 137, text: 'let Some(stale) = staleness(dir)' }] },
        { role: 'caller', why: 'this —Calls→ target', id: 'fn:l.rs:run_list', name: 'run_list', node_type: 'Function', file: 'l.rs', start_line: 84, call_sites: [{ line: 84, text: 'project::staleness(&dir, &meta)' }] },
        { role: 'test', why: 'tested at 1 hop', id: 'fn:t.rs:sep', name: 'staleness_separates', node_type: 'Function', file: 't.rs', start_line: 882, call_sites: [{ line: 901, text: 'let fresh = staleness(...)' }] },
        { role: 'dependency', why: 'target —Calls→ this', id: 'fn:p.rs:meta', name: 'read_meta', node_type: 'Function', file: 'p.rs' },
        { role: 'doc', why: 'prose linked to the target', id: 'concept:d.md:Staleness', name: 'Staleness', node_type: 'Concept', file: 'd.md', doc: 'How the index drifts.' },
    ],
    dropped: [{ role: 'caller', count: 1 }, { role: 'test', count: 3 }],
    notes: [],
};

console.log('rendering the envelope');
const summary = R.contextSummaryHtml(pack);
must('the budget meter is drawn', /ctx-meter-bar/.test(summary));
must('used and max chars are both stated', summary.includes('3,936 / 4,000 chars'));
must('each role is tallied', (summary.match(/ctx-tally/g) || []).length === 4);
must('what did not fit is reported', summary.includes('not shown: 1 caller, 3 test'));

const body = R.contextBodyHtml(pack);
for (const role of R.CTX_ROLE_ORDER) {
    must(`the ${role} section is rendered`, body.includes(`data-role="${role}"`));
}
must('sections appear in budget-priority order',
    R.CTX_ROLE_ORDER.map(r => body.indexOf(`data-role="${r}"`)).every((v, i, a) => i === 0 || v > a[i - 1]));
must('a role is headed with its count', body.includes('callers (2)'));
must("each item states why it is here", (body.match(/ctx-why/g) || []).length === 6);
must('call sites carry their line numbers', body.includes('>137<') && body.includes('>901<'));
must('the target body is rendered as code', /ctx-code/.test(body));
must('trimmed characters are reported', body.includes('1,707 chars trimmed'));
must('every row carries its node id for navigation',
    (body.match(/class="ctx-item" data-id="/g) || []).length === 6);
must('a doc item shows its prose', body.includes('How the index drifts.'));

console.log('an empty pack');
const empty = R.contextBodyHtml({ items: [], max_chars: 500, used_chars: 260 });
must('says so rather than rendering nothing', /hier-empty/.test(empty));

// Item text is source code and identifiers from the indexed repo. It reaches
// the panel through innerHTML, so anything that is not escaped here executes.
console.log('escaping repository content');
const hostile = {
    max_chars: 100, used_chars: 10,
    items: [{
        role: 'caller', why: '<img src=x onerror=alert(1)>', id: 'a"b',
        name: '<b>pwn</b>', node_type: 'Function',
        call_sites: [{ line: 1, text: '</scr' + 'ipt><scr' + 'ipt>alert(1)' }],
        code: '<svg onload=alert(1)>',
    }],
};
const ev = R.contextBodyHtml(hostile);
must('a name carrying markup is escaped', !ev.includes('<b>pwn</b>'));
must('a why carrying markup is escaped', !ev.includes('<img src=x'));
must('a call site carrying a closing tag is escaped', !ev.includes('</scr' + 'ipt>'));
must('code carrying markup is escaped', !ev.includes('<svg onload'));
must('a quote in an id cannot break out of the attribute', !ev.includes('data-id="a"b"'));

// ── 2. The canvas tier ───────────────────────────────────────────────────────

const styleMod = write('style.mjs', `
    ${ROLE_TABLE}
    const TOUR_TIER_OPACITY = { stop: 1, route: 0.8, near: 0.4, far: 0.06 };
    let state, tourState;
    const config = { getColor: () => '#type', getRelColor: () => '#rel', nodeRadius: {} };
    const CANVAS = { linkFar: '#1b1b21', linkRecede: '#26262e', linkRouteDim: '#9c5f2c',
                     linkIn: '#22d3ee', linkOut: '#f96716' };
    const tourTier = () => (tourState.active ? 'stop' : null);
    const walkTier = () => 'far';
    const isTourRouteEdge = () => false;
    const tourCurrentStop = () => null;
    const focusIsolateOn = () => false;
    ${lift(coreSrc, 'ctxRoleOf')}
    ${lift(coreSrc, 'nodeColorFor')}
    ${lift(coreSrc, 'linkColorFor')}
    ${lift(coreSrc, 'nodeLightingFor')}
    export function setEnv(s, t) { state = s; tourState = t; }
    export { nodeColorFor, linkColorFor, nodeLightingFor, CTX_ROLE };
`);
const S = await import(styleMod);

const roleById = new Map([['T', 'target'], ['C', 'caller'], ['E', 'test'], ['D', 'dependency'], ['P', 'doc']]);
const baseState = () => ({
    selectedNode: null, highlightNodes: new Set(), highlightLinks: new Set(),
    highlightLinkDir: new Map(), walkActive: false, walkColors: new Map(),
    walkReached: new Set(), walkEdgeKeys: new Set(), focusNode: null,
    focusSet: new Set(), ctxPack: { targetId: 'T', roleById }, ctxPaint: true,
});
const noTour = { active: false, isolate: false, routeIds: new Set() };
const edge = (s, t) => ({ source: s, target: t, rel: 'Calls' });

console.log('painting the pack');
S.setEnv(baseState(), noTour);
for (const [id, role] of roleById) {
    must(`a ${role} takes its role colour`, S.nodeColorFor({ id, group: 'Function' }) === S.CTX_ROLE[role].color);
}
must('a node outside the pack keeps its type colour', S.nodeColorFor({ id: 'X', group: 'Function' }) === '#type');
must('a pack member is fully lit', S.nodeLightingFor({ id: 'C' }).opacity === 1.0 && S.nodeLightingFor({ id: 'C' }).dim === false);
must('everything else recedes', S.nodeLightingFor({ id: 'X' }).dim === true);
must('an edge inside the pack keeps its relationship colour', S.linkColorFor(edge('T', 'C')) === '#rel');
must('an edge leaving the pack recedes', S.linkColorFor(edge('T', 'X')) === '#1b1b21');

console.log('the paint toggle');
const off = baseState(); off.ctxPaint = false;
S.setEnv(off, noTour);
must('paint off restores the type colour', S.nodeColorFor({ id: 'C', group: 'Function' }) === '#type');
must('paint off restores normal lighting', S.nodeLightingFor({ id: 'X' }).opacity === 0.95);
must('paint off restores link colours', S.linkColorFor(edge('T', 'X')) !== '#1b1b21');

// A walk or a tour owns the canvas outright while it runs. A pack left painted
// underneath would recolour a running animation from a panel nobody is looking
// at — the pack's own tier has to yield.
console.log('yielding to walk and tour');
const walking = baseState(); walking.walkActive = true;
S.setEnv(walking, noTour);
must('a running walk keeps the canvas', S.nodeColorFor({ id: 'C', group: 'Function' }) !== S.CTX_ROLE.caller.color);
must('a running walk keeps its own lighting', S.nodeLightingFor({ id: 'C' }).tier === null);
S.setEnv(baseState(), { active: true, isolate: false, routeIds: new Set() });
must('a running tour keeps the canvas', S.nodeColorFor({ id: 'C', group: 'Function' }) === '#fb923c');
must('a running tour keeps its own lighting', S.nodeLightingFor({ id: 'X' }).tier === 'stop');

console.log('no pack');
const none = baseState(); none.ctxPack = null;
S.setEnv(none, noTour);
must('nothing is recoloured', S.nodeColorFor({ id: 'C', group: 'Function' }) === '#type');
must('nothing is dimmed', S.nodeLightingFor({ id: 'X' }).opacity === 0.95);
must('no link is pushed back', S.linkColorFor(edge('T', 'X')) !== '#1b1b21');

// ── 3. A real response, when one was passed ──────────────────────────────────

if (samplePath) {
    console.log('a real /api/tools/context response');
    const real = JSON.parse(fs.readFileSync(samplePath, 'utf8'));
    must('the response carries a target', !!real.target);
    must('the response carries items', Array.isArray(real.items) && real.items.length > 0);
    const html = R.contextBodyHtml(real);
    must('every item in it renders a row',
        (html.match(/class="ctx-item"/g) || []).length === real.items.length);
    must('every role in it is one the panel knows',
        real.items.every(i => R.CTX_ROLE_ORDER.includes(i.role)));
    console.log('  a real response rendered every item');
}

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failures === 0
    ? 'the context panel renders and paints as specified'
    : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
