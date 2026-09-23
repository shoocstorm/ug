// Whether a boundary node keeps its ring while the graph around it is dimmed,
// checked under `node`.
//
// The 2D renderer marks a boundary node twice, and *neither* mark can be
// faded by the alpha the rest of the page dims with:
//
//   1. cosmos.gl's own outline ring, drawn from a uniform colour with a fixed
//      alpha — `cosmosOutlinedIndices` decides who is in the set;
//   2. the dashed rim baked into the node's glyph image, which the point
//      shader composites with `max(shape.a, image.a)` so the image's alpha
//      wins outright — `cosmosGlyphIndexFor` decides which image is worn.
//
// The second is the one that is actually visible, and the one that was missed
// first time round. With either left alone, a focus, a tour, a walk or a
// context pack dims the whole graph and every boundary node stays marked at
// full strength, reading as "these are the relevant ones" — the opposite of
// what the dimming is saying.
//
// Both functions are read out of the shipped parts with a string slice rather
// than transcribed, so this cannot pass against a copy that has drifted from
// what ships. Booting the real page to check it instead is a runaway CPU load
// — see Agents.md §10r.
//
// argv: <12-render-cosmos.js> <10-render-core.js>

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [cosmosPath, corePath] = process.argv.slice(2);
const cosmosSrc = fs.readFileSync(cosmosPath, 'utf8');
const coreSrc = fs.readFileSync(corePath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

// Lift one `function name(...) { ... }` out of a concatenated part. The parts
// are indented to a fixed depth inside the page's module, so the closing brace
// at that depth ends the function.
function lift(source, name, indent = 8) {
    const pad = ' '.repeat(indent);
    const start = source.indexOf('\n' + pad + 'function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found at indent ${indent} — has it been renamed?`);
    const end = source.indexOf('\n' + pad + '}', start);
    if (end < 0) throw new Error(`${name} has no closing brace at indent ${indent}`);
    return source.slice(start, end + pad.length + 2);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-boundary-rings-'));
const modPath = path.join(tmp, 'boundary-rings.mjs');

// Everything the two lifted functions reach for that is not one of them.
const STUB = `
    let cosmosNodes = [];
    let _hlBoundary = null;
    const state = {
        walkActive: false, walkReached: new Set(),
        focusNode: null, focusSet: new Set(),
        ctxPack: null, ctxPaint: false,
    };
    const tourState = { active: false };
    const TOUR_TIER_OPACITY = { current: 1, stop: 0.95, near: 0.5, far: 0.06 };
    let tier = () => null;
    const tourTier = id => tier(id);
    let walkOf = () => 'far';
    const walkTier = id => walkOf(id);
    let packRole = () => null;
    const ctxRoleOf = id => packRole(id);
    const BOUNDARY_IN_COLOR = '#fbbf24';
    const BOUNDARY_OUT_COLOR = '#a78bfa';
    let cosmosImageIndex = new Map();
    let cosmosBuf = null;
`;

fs.writeFileSync(modPath, `
    ${STUB}
    ${lift(coreSrc, 'nodeLightingFor')}
    ${lift(cosmosSrc, 'cosmosBoundaryIndices')}
    ${lift(cosmosSrc, 'cosmosOutlinedIndices')}
    ${lift(cosmosSrc, 'cosmosImageKey')}
    ${lift(cosmosSrc, 'cosmosGlyphIndexFor')}
    ${lift(cosmosSrc, 'cosmosPaintGlyphs')}
    export { cosmosOutlinedIndices, cosmosPaintGlyphs, cosmosGlyphIndexFor, state };
    export const setNodes = ns => {
        cosmosNodes = ns;
        _hlBoundary = null;
        // The atlas, as cosmosBuildAtlas builds it: one entry per key the view
        // needs, plus the ringless variant of every boundary node's type.
        cosmosImageIndex = new Map();
        const add = k => { if (!cosmosImageIndex.has(k)) cosmosImageIndex.set(k, cosmosImageIndex.size); };
        for (const n of ns) { add(cosmosImageKey(n)); if (n.isBoundary) add(n.group); }
        cosmosBuf = { imageIdx: new Float32Array(ns.length).fill(-1) };
    };
    export const glyphs = () => Array.from(cosmosBuf.imageIdx);
    export const atlasKeys = () => Array.from(cosmosImageIndex.keys());
    export const boundaryIndices = () => cosmosBoundaryIndices();
    export const setTiers = (t, w, p) => {
        if (t) tier = t;
        if (w) walkOf = w;
        if (p) packRole = p;
    };
`);
const B = await import(modPath);

// Five nodes; three of them boundaries, at indices 1, 2 and 4 — deliberately
// not contiguous, so an implementation that returned a count or a prefix
// rather than the actual indices fails.
const NODES = [
    { id: 'plain-a', group: 'Function' },
    { id: 'route', group: 'Function', isBoundary: true, boundaries: [{ direction: 'Inbound' }] },
    { id: 'cli', group: 'Function', isBoundary: true, boundaries: [{ direction: 'Outbound' }] },
    { id: 'plain-b', group: 'Class' },
    // The only node of its type, and a boundary — so `Method` reaches the
    // atlas solely through the ringless variant `cosmosBuildAtlas` adds for it.
    { id: 'main', group: 'Method', isBoundary: true, boundaries: [{ direction: 'Inbound' }] },
];
B.setNodes(NODES);

const reset = () => {
    B.state.walkActive = false;
    B.state.focusNode = null;
    B.state.focusSet = new Set();
    B.state.ctxPack = null;
    B.state.ctxPaint = false;
    B.setTiers(() => null, () => 'far', () => null);
};

console.log('a dimmed boundary node loses its ring');

console.log('nothing dimming');
reset();
must('every boundary node is outlined', JSON.stringify(B.cosmosOutlinedIndices()) === '[1,2,4]');
must('and only the boundary nodes are', B.cosmosOutlinedIndices().every(i => NODES[i].isBoundary));

console.log('focus anchored on a plain node');
reset();
B.state.focusNode = 'plain-a';
B.state.focusSet = new Set(['plain-a', 'route']);
must('a boundary node inside the focus set keeps its ring',
    JSON.stringify(B.cosmosOutlinedIndices()) === '[1]');

console.log('focus anchored on a boundary node');
reset();
B.state.focusNode = 'main';
B.state.focusSet = new Set(['main']);
must('the selected boundary node keeps its own ring',
    JSON.stringify(B.cosmosOutlinedIndices()) === '[4]');

console.log('a tour running');
reset();
B.setTiers(id => (id === 'cli' ? 'current' : 'far'), null, null);
must('only the stop on the route is outlined',
    JSON.stringify(B.cosmosOutlinedIndices()) === '[2]');

console.log('a walk running');
reset();
B.state.walkActive = true;
B.setTiers(null, id => (id === 'route' ? 'seed' : id === 'main' ? 'reached' : 'far'), null);
must('reached boundary nodes are outlined, unreached ones are not',
    JSON.stringify(B.cosmosOutlinedIndices()) === '[1,4]');
must('a pending node is dim, so it has no ring',
    (B.setTiers(null, id => (id === 'route' ? 'pending' : 'far'), null),
        B.cosmosOutlinedIndices().length === 0));

console.log('a context pack painted');
reset();
B.state.ctxPack = { roleById: new Map() };
B.state.ctxPaint = true;
B.setTiers(null, null, id => (id === 'cli' ? 'seed' : null));
must('only pack members are outlined',
    JSON.stringify(B.cosmosOutlinedIndices()) === '[2]');

console.log('the rim baked into the glyph');
reset();
must('the atlas carries a ringless variant for a boundary-only type',
    B.atlasKeys().includes('Method'));
B.cosmosPaintGlyphs();
const lit = B.glyphs();
must('an undimmed boundary node wears its ringed glyph',
    lit[1] === B.atlasKeys().indexOf('Function|in')
    && lit[2] === B.atlasKeys().indexOf('Function|out')
    && lit[4] === B.atlasKeys().indexOf('Method|in'));

B.state.focusNode = 'route';
B.state.focusSet = new Set(['route']);
must('the repaint reports that a glyph moved', B.cosmosPaintGlyphs() === true);
const dimmed = B.glyphs();
must('a dimmed boundary node drops to the ringless glyph of its type',
    dimmed[2] === B.atlasKeys().indexOf('Function')
    && dimmed[4] === B.atlasKeys().indexOf('Method'));
must('the one in the focus set keeps its rim',
    dimmed[1] === B.atlasKeys().indexOf('Function|in'));
must('a second repaint with nothing changed reports no move',
    B.cosmosPaintGlyphs() === false);

reset();
must('leaving focus puts every rim back', B.cosmosPaintGlyphs() === true
    && JSON.stringify(B.glyphs()) === JSON.stringify(lit));

console.log('the boundary scan is cached');
reset();
must('the same array is handed back rather than rebuilt',
    B.boundaryIndices() === B.boundaryIndices());
must('and a new point set drops it', (B.setNodes([{ id: 'only', isBoundary: true }]),
    JSON.stringify(B.boundaryIndices()) === '[0]'));

fs.rmSync(tmp, { recursive: true, force: true });
if (failures) {
    console.error(`${failures} check(s) failed`);
    process.exit(1);
}
console.log('all checks passed');
