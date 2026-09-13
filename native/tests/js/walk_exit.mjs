// What a Graph Walk hands back when it ends, checked under `node`.
//
// A walk owns the canvas while it runs: `nodeLightingFor` reads
// `state.walkActive` first and shades every node by its hop, so *focus* mode —
// the 1-hop dimming anchored on a selected node — is inert for as long as the
// walk lasts. But focus can still be switched on during one: opening a node's
// details from the walk's node list goes through `handleClick`, and that
// anchors focus on it (`enterFocus`).
//
// The anchor is therefore invisible right up until the walk exits, and then it
// is the only thing left shading the graph. `exitWalk` has to drop it, or a
// walk ends with the graph dimmed around whichever row the reader last opened
// and nothing on screen saying why — and if solo was armed, with everything
// outside that one neighbourhood gone.
//
// The three functions are read out of the real parts with a string slice
// rather than transcribed, so this cannot pass against a copy that has drifted
// from what ships.
//
// Booting the real page to check this instead is a runaway CPU load — see
// Agents.md §10r.
//
// argv: <18-walk.js> <08-sidebar-nav.js> <10-render-core.js>

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [walkPath, navPath, corePath] = process.argv.slice(2);
const walkSrc = fs.readFileSync(walkPath, 'utf8');
const navSrc = fs.readFileSync(navPath, 'utf8');
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

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-walk-exit-'));
const write = (name, body) => {
    const p = path.join(tmp, name);
    fs.writeFileSync(p, body);
    return p;
};

// Everything the three lifted functions reach for that is not one of them.
// The stubs record rather than act: what matters here is the state the page is
// left in, not the paint that follows from it.
const STUB = `
    const calls = [];
    const bodyClasses = new Set();
    globalThis.document = {
        body: { classList: {
            add: c => bodyClasses.add(c),
            remove: c => bodyClasses.delete(c),
            contains: c => bodyClasses.has(c),
        } },
        getElementById: () => null,
    };
    const walkPlay = { active: false, playing: false, layers: null, index: -1, seedNode: null,
                       stepTimer: null, phaseTimer: null, streaming: -1, token: 0 };
    const state = {
        walkActive: false, walkRunning: false, walkSeed: null,
        walkReached: new Set(), walkColors: new Map(), walkEdgeKeys: new Set(),
        walkPosSaved: null, walkCascadePos: null, walkLanes: [],
        focusNode: null, focusSet: new Set(), focusIsolate: false,
        selectedNode: null, soloOnly: false, graphMode: 'client',
        adjComplete: new Set(), ctxPack: null, ctxPaint: false,
    };
    const tourState = { active: false, isolate: false, routeIds: new Set() };
    const TOUR_TIER_OPACITY = {};
    const tourTier = () => null;
    const ctxRoleOf = () => null;
    // enterFocus widens its set from the graph's edges; the neighbourhood
    // itself is not what is under test, so it is handed over directly.
    let neighbours = new Set();
    const neighborIdsOf = () => new Set(neighbours);
    const noop = name => (...a) => calls.push(name);
    const cancelWalkTimers = noop('cancelWalkTimers');
    const hideWalkOverlay = noop('hideWalkOverlay');
    const closeWalkNodes = noop('closeWalkNodes');
    const exitWalkImmersive = noop('exitWalkImmersive');
    const plotNodes = noop('plotNodes');
    const restoreWalkPositions = noop('restoreWalkPositions');
    const bumpGraphStyles = noop('bumpGraphStyles');
    const syncSoloButton = noop('syncSoloButton');
    const updateNavbar = noop('updateNavbar');
    const writeUrlState = noop('writeUrlState');
    const ensureEdges = () => Promise.resolve();
`;

const mod = write('walk-exit.mjs', `
    ${STUB}
    ${lift(walkSrc, 'exitWalk')}
    ${lift(walkSrc, 'walkTier')}
    ${lift(navSrc, 'enterFocus')}
    ${lift(navSrc, 'exitFocus')}
    ${lift(coreSrc, 'focusIsolateOn')}
    ${lift(coreSrc, 'nodeVisibleFor')}
    ${lift(coreSrc, 'nodeLightingFor')}
    export { exitWalk, enterFocus, nodeVisibleFor, nodeLightingFor,
             state, walkPlay, calls, bodyClasses, tourState };
    export const setNeighbours = ids => { neighbours = new Set(ids); };
`);
const W = await import(mod);

// A two-hop walk out of `seed`, with the reader having opened `mid` from the
// walk's node list — the state the page is actually in when Exit is pressed.
function startWalk() {
    // `showWalkOverlay` sets this; `exitWalk` reads it to decide whether there
    // is a walk to tear down at all.
    W.walkPlay.active = true;
    W.state.walkActive = true;
    W.state.walkRunning = true;
    W.state.walkSeed = 'seed';
    W.state.walkReached = new Set(['seed', 'mid', 'leaf']);
    W.state.walkColors = new Map([['seed', '#fff'], ['mid', '#fff'], ['leaf', '#fff']]);
    W.state.selectedNode = { id: 'mid', name: 'mid' };
    W.bodyClasses.add('walk-active');
    W.setNeighbours(['mid', 'seed']);
    W.enterFocus({ id: 'mid' });
}

console.log('anchoring focus during a walk');

startWalk();
must('the details panel anchors focus on the node it opened',
    W.state.focusNode === 'mid' && W.bodyClasses.has('focus-active'));
// The reason the anchor goes unnoticed: the walk's own shading answers first.
must('focus dimming is inert while the walk runs — the hop shading answers first',
    W.nodeLightingFor({ id: 'leaf' }).opacity === 0.96);
must('and focus isolation cannot hide a walked node',
    W.nodeVisibleFor({ id: 'leaf' }) === true);

console.log('exiting the walk');

W.state.focusIsolate = true;   // solo armed mid-walk, on a set that is about to be stale
W.exitWalk();

must('the walk is over', W.state.walkActive === false && !W.bodyClasses.has('walk-active'));
must('the focus anchor is dropped with it', W.state.focusNode === null);
must('so the graph comes back undimmed rather than lit around the last row opened',
    W.nodeLightingFor({ id: 'leaf' }).dim === false);
must('the dimming class goes too', !W.bodyClasses.has('focus-active'));
must('solo is disarmed, so nothing outside the old anchor is hidden',
    W.state.focusIsolate === false && W.nodeVisibleFor({ id: 'far' }) === true);
// Exiting a walk is not the same as clearing the selection: the node stays
// selected, its panel stays open, and the reader keeps exploring from there.
must('the selection survives the exit',
    W.state.selectedNode && W.state.selectedNode.id === 'mid');
must('the layout is handed back', W.calls.includes('restoreWalkPositions'));
must('and the canvas is restyled once the state is settled',
    W.calls.lastIndexOf('bumpGraphStyles') > W.calls.lastIndexOf('restoreWalkPositions'));

console.log('exiting with no walk running');

// The launcher calls exitWalk(true) before every run to tear down the previous
// walk. With nothing to tear down it must not reach into the page's state.
W.state.focusNode = 'kept';
W.state.focusSet = new Set(['kept']);
W.bodyClasses.add('focus-active');
W.exitWalk(true);
must('a no-op exit leaves an ordinary focus alone',
    W.state.focusNode === 'kept' && W.bodyClasses.has('focus-active'));

console.log(failures ? `\n${failures} check(s) failed` : '\na walk hands the graph back whole');
process.exit(failures ? 1 : 0);
