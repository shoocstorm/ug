#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────────────
// make-demo-video.mjs — record the ug demo video from the published demo page.
//
//   node scripts/make-demo-video.mjs
//     [--out docs/ug-website/img/ug-demo.mp4]   output file
//     [--fps 30] [--size 1920x1080]             capture settings
//     [--quick]                                 short smoke-run (no search/walk)
//     [--keep-frames]                           keep the JPEG frames + ffconcat
//
// What it does, in order:
//   1. serves docs/ug-website/demo/ on a local port. index.html is served with
//      a small export shim injected into the page's main module script, just
//      before its closing tag — that puts `window.__ugv` (state, frameGraph,
//      frameNodeSet, handleClick, runWalk, …) at the driver's disposal without
//      touching the source parts.
//   2. launches headless Chrome with the real GPU (`--use-angle=metal
//      --enable-gpu`) and connects to the page target over the debugging
//      WebSocket (node's native WebSocket — no dependencies).
//   3. records a `Page.screencast` while it plays a fixed storyboard:
//        S1  terminal overlay types `ug gen` (numbers read live from demo.json)
//        S2  graph bloom + slow dolly out      (frameGraph long ease)
//        S3  fly to a hub function, select it, hover its neighbourhood
//        S4  type in the Ask bar, arrow through live results
//        S5  graph walk — inbound hops from build_router ("who depends on this")
//        S6  pull back to the full map, title card, fade to black
//   4. writes the frames, builds an ffconcat with per-frame durations taken
//      from the screencast timestamps (so dropped frames hold instead of
//      speeding up), and encodes h264/yuv420p/faststart.
//   5. kills Chrome, checks for stragglers, removes the frames.
//
// The storyboard is data (SCENES / BEATS below) — retime or repoint nodes
// without touching the plumbing.
// ─────────────────────────────────────────────────────────────────────────────

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DEMO_DIR = path.join(ROOT, 'docs', 'ug-website', 'demo');
const CHROME = process.env.UGV_CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

// ── CLI ─────────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
const arg = (name, dflt) => {
    const i = argv.indexOf(`--${name}`);
    if (i < 0) return dflt;
    const v = argv[i + 1];
    return v && !v.startsWith('--') ? v : true;
};
const QUICK = argv.includes('--quick');
const DIAG = argv.includes('--diagnose');
const KEEP_FRAMES = argv.includes('--keep-frames');
const FPS = parseInt(arg('fps', '30'), 10);
const [W, H] = String(arg('size', '1920x1080')).split('x').map(n => parseInt(n, 10));
const OUT = path.resolve(ROOT, String(arg('out', 'docs/ug-website/img/ug-demo.mp4')));

if (!fs.existsSync(DEMO_DIR)) {
    console.error(`demo not found at ${DEMO_DIR} — run ./scripts/gen-demo.sh first`);
    process.exit(1);
}

// ── Storyboard ──────────────────────────────────────────────────────────────
// Node ids are stable ids in the demo's graph.json (function:<file>:<name>).
const NODE_HUB = 'function:indexer/common.rs:get_node_text';    // hottest function (85 deg)
const NODE_WALK_SEED = 'function:serve/router.rs:build_router'; // the server's route table

const demoJson = JSON.parse(fs.readFileSync(path.join(DEMO_DIR, 'demo.json'), 'utf8'));
const stats = demoJson.nodes != null ? demoJson : null;
const NODES = stats ? stats.nodes.toLocaleString('en-US') : '?';
const EDGES = stats ? stats.edges.toLocaleString('en-US') : '?';
const FILES = stats ? stats.files : '?';
const LINES = stats ? stats.lines.toLocaleString('en-US') : '?';
const LANGS = (() => {
    // honest language split out of graph.json folder 0 (same numbers the
    // landing page shows), so the terminal never drifts from the snapshot
    try {
        const g = JSON.parse(fs.readFileSync(path.join(DEMO_DIR, 'graph.json'), 'utf8'));
        const root = g.nodes.find(n => n.id === 'folder:.');
        const b = root?.folder?.languageBreakdown || {};
        return Object.entries(b).sort((a, c) => c[1] - a[1])
            .map(([k, v]) => `${v} ${k}`).join(' · ');
    } catch { return `${FILES} files`; }
})();

const TERMINAL_LINES = [
    { t: 'cmd', text: 'ug gen' },
    { t: 'row', text: `  reading ${FILES} files · ${LINES} lines` },
    { t: 'row', text: `  ✓ parsed ${LANGS}` },
    { t: 'row', text: `  ✓ ${NODES} symbols · ${EDGES} edges` },
    { t: 'row', text: '  ✓ graph written — the map is live' },
];

// Beats in milliseconds; scaled down ~2.5x in --quick.
const B = QUICK ? {
    dolly: 3000, fly: 900, settle: 700, typeMs: 1500, hover: 1500,
    searchType: 70, searchHold: 1200, walk: 5000, outro: 2200, titleHold: 1600,
} : {
    dolly: 7500, fly: 1600, settle: 1400, typeMs: 2600, hover: 3400,
    searchType: 150, searchHold: 2600, walk: 11500, outro: 4200, titleHold: 3400,
};

const log = (m) => console.log(`[ugv] ${m}`);

// ── 1. Static server with the export shim injected ──────────────────────────
const SHIM = `
;(function () {
    try {
        window.__ugv = {
            state: state, frameGraph: frameGraph, frameNodeSet: frameNodeSet,
            focusNode: focusNode, handleClick: handleClick,
            clearSelection: clearSelection, exitFocus: exitFocus,
            exitWalk: exitWalk, runWalk: runWalk,
            node: function (id) { return state.nodeById && state.nodeById.get(id); },
            ready: function () { return !!(state.nodeById && state.nodeById.size > 100); },
        };
    } catch (err) { window.__ugv = { error: String(err) }; }
})();`;

function injectShim(html) {
    // Inject into the module script that owns the graph UI — located by a
    // function only that module defines, then its closing tag.
    const anchor = html.indexOf('function frameGraph');
    if (anchor < 0) throw new Error('main module script not found in demo index.html');
    const close = html.indexOf('</script>', anchor);
    if (close < 0) throw new Error('main module closing tag not found');
    return html.slice(0, close) + SHIM + html.slice(close);
}

const MIME = {
    '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8',
    '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8',
    '.json': 'application/json', '.png': 'image/png', '.jpg': 'image/jpeg',
    '.svg': 'image/svg+xml', '.wasm': 'application/wasm', '.ico': 'image/x-icon',
};

function startServer() {
    return new Promise((resolve) => {
        const srv = http.createServer((req, res) => {
            const url = decodeURIComponent(req.url.split('?')[0]);
            let p = path.join(DEMO_DIR, url === '/' ? 'index.html' : url.slice(1));
            if (!p.startsWith(DEMO_DIR)) { res.writeHead(403); res.end(); return; }
            if (fs.statSync(p, { throwIfNoEntry: false })?.isDirectory()) p = path.join(p, 'index.html');
            if (!fs.existsSync(p)) { res.writeHead(404); res.end('nope'); return; }
            let body = fs.readFileSync(p);
            if (p.endsWith('index.html')) body = injectShim(body.toString('utf8'));
            res.writeHead(200, { 'content-type': MIME[path.extname(p)] || 'application/octet-stream' });
            res.end(body);
        });
        srv.listen(0, '127.0.0.1', () => resolve({ srv, port: srv.address().port }));
    });
}

// ── 2. Chrome + a minimal CDP client (native WebSocket, no deps) ────────────
function freePort() {
    const s = http.createServer(() => {});
    return new Promise((r) => { s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => r(p)); }); });
}

async function launchChrome(debugPort, userDataDir) {
    const flags = [
        '--headless', '--no-first-run', '--no-default-browser-check',
        `--remote-debugging-port=${debugPort}`,
        `--user-data-dir=${userDataDir}`,
        `--window-size=${W},${H}`, '--hide-scrollbars', '--mute-audio',
        '--force-device-scale-factor=1',
        '--disable-background-timer-throttling', '--disable-renderer-backgrounding',
        '--disable-backgrounding-occluded-windows',
        // a hidden document suspends rAF outright — the walk played into the
        // void while every later scene sat frozen. Occlusion tracking on a
        // headless window can spontaneously mark the page hidden mid-run.
        '--disable-features=CalculateNativeWinOcclusion',
        '--use-angle=metal', '--enable-gpu', '--ignore-gpu-blocklist',
        'about:blank',
    ];
    const proc = spawn(CHROME, flags, { stdio: ['ignore', 'ignore', 'ignore'] });
    for (let i = 0; i < 60; i++) {
        await new Promise(r => setTimeout(r, 250));
        try {
            const v = await (await fetch(`http://127.0.0.1:${debugPort}/json/version`)).json();
            if (v.webSocketDebuggerUrl) return { proc };
        } catch { /* not up yet */ }
    }
    throw new Error('chrome debugging port never came up');
}

class Cdp {
    constructor(ws) { this.ws = ws; this.id = 0; this.pending = new Map(); this.handlers = new Map(); }
    static async connect(url) {
        const ws = new WebSocket(url);
        await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('ws connect failed')); });
        const c = new Cdp(ws);
        ws.onmessage = (ev) => {
            const msg = JSON.parse(ev.data);
            if (msg.id && c.pending.has(msg.id)) {
                const { res, rej } = c.pending.get(msg.id); c.pending.delete(msg.id);
                msg.error ? rej(new Error(`${msg.error.message} ${msg.error.data || ''}`)) : res(msg.result);
            } else if (msg.method) {
                for (const fn of c.handlers.get(msg.method) || []) fn(msg.params);
            }
        };
        ws.onclose = () => { for (const [, rej] of c.pending) rej(new Error('cdp socket closed')); };
        return c;
    }
    on(method, fn) {
        if (!this.handlers.has(method)) this.handlers.set(method, []);
        this.handlers.get(method).push(fn);
    }
    send(method, params = {}) {
        const id = ++this.id;
        return new Promise((res, rej) => {
            this.pending.set(id, { res, rej });
            this.ws.send(JSON.stringify({ id, method, params }));
        });
    }
    async eval(expression, { awaitPromise = false } = {}) {
        const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise });
        if (r.exceptionDetails) throw new Error(`page eval failed: ${r.exceptionDetails.exception?.description || r.exceptionDetails.text} :: in <${expression.slice(0, 160)}>`);
        return r.result?.value;
    }
}

const wait = (ms) => new Promise(r => setTimeout(r, ms));

async function waitFor(cdp, expr, timeoutMs, label) {
    const t0 = Date.now();
    while (Date.now() - t0 < timeoutMs) {
        try { if (await cdp.eval(expr)) return Date.now() - t0; } catch { /* page mid-load */ }
        await wait(250);
    }
    throw new Error(`timeout waiting for ${label}`);
}

// ── 3. In-page overlay UI (terminal, captions, title card) ──────────────────
// Injected once via Runtime.evaluate. All ids prefixed ugv; no backticks in
// the injected source (it travels inside a JS string literal).
const OVERLAY_BOOT = `
(() => {
    if (window.__ugvRun) return 'ok';
    const style = document.createElement('style');
    style.textContent = [
        '#ugv-term{position:fixed;left:50%;top:50%;transform:translate(-50%,-50%);width:min(660px,84vw);background:rgba(5,8,12,.93);border:1px solid rgba(122,236,182,.16);border-radius:14px;padding:20px 26px 24px;font:500 15px/1.8 ui-monospace,"SF Mono",Menlo,monospace;color:#dfe9e4;box-shadow:0 30px 90px rgba(0,0,0,.6);z-index:2147483100;transition:opacity .85s ease}',
        '#ugv-term .bar{display:flex;gap:8px;margin:-4px -12px 16px}#ugv-term .bar i{width:11px;height:11px;border-radius:50%;background:#28323c}',
        '#ugv-term .p{color:#7aecb6}#ugv-term .ok{color:#7aecb6}#ugv-term .dim{color:#8fa3ad}',
        '#ugv-term .row{opacity:0;transform:translateY(4px);transition:opacity .45s ease,transform .45s ease;white-space:pre}',
        '#ugv-term .row.in{opacity:1;transform:none}',
        '#ugv-cursor{display:inline-block;width:9px;height:17px;background:#7aecb6;vertical-align:-3px;margin-left:1px;animation:ugvblink 1.1s step-end infinite}',
        '@keyframes ugvblink{50%{opacity:0}}',
        '#ugv-cap{position:fixed;left:56px;bottom:52px;max-width:60vw;font:500 19px/1.45 ui-monospace,"SF Mono",Menlo,monospace;color:#eef7f2;text-shadow:0 2px 16px rgba(0,0,0,.85),0 0 40px rgba(0,0,0,.5);opacity:0;transform:translateY(10px);transition:opacity .7s ease,transform .7s ease;z-index:2147483100;letter-spacing:.02em}',
        '#ugv-cap.in{opacity:1;transform:none}',
        '#ugv-cap b{color:#7aecb6;font-weight:600}',
        '#ugv-title{position:fixed;inset:0;display:flex;flex-direction:column;align-items:center;justify-content:center;z-index:2147483100;opacity:0;transition:opacity 1.25s ease;background:radial-gradient(ellipse at center,rgba(4,7,10,.3) 0%,rgba(4,7,10,.82) 100%);pointer-events:none}',
        '#ugv-title.in{opacity:1}',
        '#ugv-title h1{font:700 104px/1 ui-monospace,"SF Mono",Menlo,monospace;color:#f2fbf7;margin:0;letter-spacing:-.04em;text-shadow:0 6px 40px rgba(0,0,0,.6)}',
        '#ugv-title h1 em{color:#7aecb6;font-style:normal}',
        '#ugv-title p{font:500 21px/1.5 ui-monospace,Menlo,monospace;color:#bfe6d4;margin:10px 0 0}',
        '#ugv-title code{font:500 15px/1 ui-monospace,Menlo,monospace;color:#7aecb6;background:rgba(7,12,16,.88);border:1px solid rgba(122,236,182,.25);padding:13px 20px;border-radius:10px;margin-top:30px}',
        '#ugv-title span{font:500 13px/1 ui-monospace,Menlo,monospace;color:#7d95a0;margin-top:20px;letter-spacing:.05em}',
        '#ugv-blackout{position:fixed;inset:0;background:#04060a;z-index:2147483200;opacity:0;transition:opacity 1.5s ease;pointer-events:none}',
        '#ugv-blackout.in{opacity:1}',
        // Always-on tally while recording. Beyond looking the part, it keeps a
        // compositor animation alive for the whole capture: once the graph
        // canvas goes idle it stops committing frames, rAF starves, and the
        // screencast receives nothing — the walk and the outro never wake it
        // on their own (see AGENTS.md §9h). step-end: cheapest possible beat.
        '#ugv-rec{position:fixed;right:28px;top:22px;display:flex;align-items:center;gap:9px;padding:8px 15px;border-radius:999px;background:rgba(6,10,14,.74);border:1px solid rgba(255,99,99,.28);font:600 12px/1 ui-monospace,Menlo,monospace;color:#ff9b9b;letter-spacing:.16em;z-index:2147483100;pointer-events:none}',
        '#ugv-rec i{width:9px;height:9px;border-radius:50%;background:#ff5d5d;animation:ugvrec 1.2s step-end infinite}',
        '@keyframes ugvrec{50%{opacity:.22}}',
    ].join('');
    document.head.appendChild(style);
    const term = document.createElement('div'); term.id = 'ugv-term';
    term.innerHTML = '<div class="bar"><i></i><i></i><i></i></div>' +
        '<div><span class="p">' + (window.__ugvPrompt || '>') + ' </span><span id="ugv-term-type"></span><span id="ugv-cursor"></span></div>' +
        '<div id="ugv-term-out"></div>';
    const cap = document.createElement('div'); cap.id = 'ugv-cap';
    const title = document.createElement('div'); title.id = 'ugv-title';
    title.innerHTML = '<h1>u<em>g</em></h1><p>the right context, in seconds.</p>' +
        '<code>curl -fsSL https://ultra-graph.web.app/install.sh | sh</code>' +
        '<span>github.com/shoocstorm/ug</span>';
    const black = document.createElement('div'); black.id = 'ugv-blackout';
    const rec = document.createElement('div'); rec.id = 'ugv-rec'; rec.style.display = 'none';
    rec.innerHTML = '<i></i>REC';
    document.body.append(term, cap, title, black, rec);
    window.__ugvRun = {
        populate(rows) {
            const out = document.getElementById('ugv-term-out'); out.innerHTML = '';
            rows.slice(1).forEach(r => {
                const d = document.createElement('div'); d.className = 'row';
                d.textContent = r.text.replace(/^\\s*\\u2713/, '');
                if (/\\u2713/.test(r.text)) { d.classList.add('ok'); d.textContent = '\\u2713' + d.textContent; }
                else d.classList.add('dim');
                out.appendChild(d);
            });
            return rows[0].text;
        },
        term(cmd, cps) {
            return new Promise(resolve => {
                const el = document.getElementById('ugv-term-type');
                let i = 0;
                const tick = () => {
                    if (i <= cmd.length) { el.textContent = cmd.slice(0, i++); setTimeout(tick, cps); }
                    else setTimeout(reveal, 430);
                };
                let r = 0;
                const reveal = () => {
                    const rows = document.querySelectorAll('#ugv-term-out .row');
                    if (r >= rows.length) { setTimeout(resolve, 700); return; }
                    rows[r++].classList.add('in');
                    setTimeout(reveal, 430);
                };
                tick();
            });
        },
        cap(html) { const c = document.getElementById('ugv-cap'); c.innerHTML = html; c.classList.add('in'); return true; },
        capHide() { document.getElementById('ugv-cap').classList.remove('in'); return true; },
        fadeTerm() { const t = document.getElementById('ugv-term'); t.style.opacity = '0'; setTimeout(() => t.remove(), 950); return true; },
        fadeCover() {
            const c = document.getElementById('__ugv_cover');
            if (c) { c.style.transition = 'opacity .95s ease'; c.style.opacity = '0'; setTimeout(() => c.remove(), 1000); }
            return true;
        },
        title() { document.getElementById('ugv-title').classList.add('in'); return true; },
        blackout() { document.getElementById('ugv-blackout').classList.add('in'); return true; },
        rec(on) { const r = document.getElementById('ugv-rec'); r.style.display = on ? 'flex' : 'none'; return true; },
    };
    return 'ok';
})()`;

// Black cover from the first moment of the document — no white flash, and the
// graph bloom stays hidden behind it while the terminal plays.
const COVER_BOOT = `
(() => {
    const mount = () => {
        if (document.getElementById('__ugv_cover')) return;
        const d = document.createElement('div');
        d.id = '__ugv_cover';
        d.style.cssText = 'position:fixed;inset:0;background:#04060a;z-index:2147483000';
        (document.body || document.documentElement).appendChild(d);
    };
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', mount, { once: true });
    else mount();
    window.__visLog = [[document.visibilityState, Math.round(performance.now())]];
    for (const t of ['visibilitychange', 'blur', 'focus']) {
        window.addEventListener(t, () => window.__visLog.push([t, document.visibilityState, Math.round(performance.now())]));
    }
})()`;

// ── 4. Input helpers ────────────────────────────────────────────────────────
async function mouse(cdp, type, x, y, opts = {}) {
    await cdp.send('Input.dispatchMouseEvent', { type, x, y, button: 'left', pointerType: 'mouse', ...opts });
}
async function clickAt(cdp, x, y) {
    await mouse(cdp, 'mousePressed', x, y, { clickCount: 1 });
    await mouse(cdp, 'mouseReleased', x, y, { clickCount: 1 });
}
function keyCode(ch) {
    if (ch === '\r') return { key: 'Enter', code: 'Enter', keyCode: 13 };
    if (ch === '\x1b') return { key: 'Escape', code: 'Escape', keyCode: 27 };
    if (ch === '\x1b[B') return { key: 'ArrowDown', code: 'ArrowDown', keyCode: 40 };
    const upper = ch.toUpperCase();
    return { key: ch, code: `Key${upper}`, keyCode: upper.charCodeAt(0) };
}
async function keyTap(cdp, ch, mods = {}) {
    const k = keyCode(ch);
    const base = { key: k.key, code: k.code, windowsVirtualKeyCode: k.keyCode, nativeVirtualKeyCode: k.keyCode, ...mods };
    if (ch.length === 1 && !mods.raw) {
        await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', text: ch, ...base, unmodifiedText: ch });
        await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
    } else {
        await cdp.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', ...base });
        await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
    }
}

// ── 5. The storyboard ───────────────────────────────────────────────────────
async function playScenes(cdp, url) {
    const marks = [];
    const mark = (name) => marks.push({ name, at: Date.now() });
    const t0 = Date.now();

    // Watchdog: if the document ever goes hidden (rAF then starves and the
    // capture freezes), log it and force the page back to active.
    const hiddenAt = [];
    const visPoll = setInterval(async () => {
        try {
            if (await cdp.eval('document.hidden')) {
                hiddenAt.push(((Date.now() - t0) / 1000).toFixed(1) + 's');
                log(`page went HIDDEN at +${hiddenAt[hiddenAt.length - 1]} — forcing active`);
                await cdp.send('Page.setWebLifecycleState', { state: 'active' }).catch(() => {});
            }
        } catch { /* between contexts */ }
    }, 250);

    // Start the screencast first so nothing is missed, then navigate.
    await cdp.send('Page.startScreencast', { format: 'jpeg', quality: 90, maxWidth: W, maxHeight: H, everyFrame: true });
    await cdp.send('Page.navigate', { url });
    mark('navigate');

    // Wait for the module + graph, plus a layout settle so the bloom is mostly
    // done behind the cover (we reveal mid-motion, not mid-chaos).
    const readyMs = await waitFor(cdp, 'window.__ugv && __ugv.ready()', 60000, 'graph ready');
    log(`graph ready in ${(readyMs / 1000).toFixed(1)}s (${(await cdp.eval('window.__ugv.state.nodeById.size'))} nodes)`);
    const gpu = await cdp.eval(`(() => { const c = document.createElement('canvas'); const g = c.getContext('webgl2') || c.getContext('webgl'); if (!g) return 'NO_WEBGL'; const d = g.getExtension('WEBGL_debug_renderer_info'); return d ? String(g.getParameter(d.UNMASKED_RENDERER_WEBGL)) : String(g.getParameter(g.RENDERER)); })()`);
    log(`webgl renderer: ${gpu}`);
    if (/swift/i.test(String(gpu))) log('WARNING: software renderer — frames will be slow; check GPU flags');

    await cdp.eval(OVERLAY_BOOT);
    await cdp.eval(`window.__ugvPrompt = ${JSON.stringify('❯')}`);
    const cmd = await cdp.eval(`__ugvRun.populate(${JSON.stringify(TERMINAL_LINES)})`);

    // S1 — terminal (graph still hidden behind the cover)
    mark('s1-terminal');
    await cdp.eval(`__ugvRun.term(${JSON.stringify(cmd)}, ${QUICK ? 40 : 52})`, { awaitPromise: true });
    await wait(QUICK ? 250 : 600);

    // S2 — reveal + dolly out. Snap tight on the hub *behind* the cover, then
    // fade both overlays away as one slow frameGraph pull starts.
    await cdp.eval(`__ugv.frameNodeSet([${JSON.stringify(NODE_HUB)}], 1)`);
    await wait(250);
    mark('s2-reveal');
    await cdp.eval('__ugvRun.rec(true); __ugvRun.fadeTerm(); __ugvRun.fadeCover()');
    await cdp.eval(`__ugv.frameGraph(${B.dolly})`);
    await cdp.eval(`__ugvRun.cap(${JSON.stringify(`${NODES} nodes · ${EDGES} edges — <b>one codebase, mapped</b>`)})`);
    await wait(B.dolly * 0.55);
    await cdp.eval('__ugvRun.capHide()');
    await wait(B.dolly * 0.45);

    // S3 — fly to the hottest function, select it, hover its neighbourhood
    mark('s3-select');
    await cdp.eval(`__ugv.frameNodeSet([${JSON.stringify(NODE_HUB)}], ${B.fly})`);
    await wait(B.fly + 200);
    await cdp.eval(`__ugv.handleClick(null, __ugv.node(${JSON.stringify(NODE_HUB)}))`);
    await cdp.eval(`__ugvRun.cap('every symbol, every dependency — <b>indexed</b>')`);
    await wait(B.settle);
    // Real pointer passes over the canvas centre — the node is centred there,
    // so the hover tooltip + neighbourhood highlight land on target.
    for (const [dx, dy] of [[0, 0], [30, 22], [-24, 30], [0, 0]]) {
        await mouse(cdp, 'mouseMoved', W / 2 + dx, H / 2 + dy);
        await wait(B.hover / 4);
    }
    await cdp.eval('__ugvRun.capHide()');

    if (!QUICK) {
        // S4 — the Ask bar: real click, real keystrokes, live graph highlights
        mark('s4-search');
        await cdp.eval('__ugv.exitFocus(); __ugv.clearSelection()');
        await cdp.eval('__ugv.frameGraph(1700)');
        await wait(1900);
        const rect = await cdp.eval(`(() => { const r = document.getElementById('ask-input').getBoundingClientRect(); return JSON.stringify({ x: r.left + Math.min(r.width / 2, 240), y: r.top + r.height / 2 }); })()`);
        const { x, y } = JSON.parse(rect);
        await clickAt(cdp, x, y);
        await wait(350);
        for (const ch of 'walk') { await keyTap(cdp, ch); await wait(B.searchType); }
        await wait(B.searchHold * 0.5);
        await keyTap(cdp, '\x1b[B'); await wait(420);   // arrow through results
        await keyTap(cdp, '\x1b[B'); await wait(B.searchHold * 0.5);
        await cdp.eval(`__ugvRun.cap('find anything — <b>in seconds</b>')`);
        await wait(900);
        await cdp.eval('__ugvRun.capHide()');
        await cdp.eval(`(() => { const c = document.getElementById('ask-clear'); if (c) c.click(); const i = document.getElementById('ask-input'); if (i) i.blur(); return true; })()`);
        await wait(500);

        // S5 — the walk: who depends on build_router, hop by hop
        mark('s5-walk');
        await cdp.eval(`(() => { const s = __ugv.state; s.walkSeed = ${JSON.stringify(NODE_WALK_SEED)}; s.walkHops = 3; s.walkDir = 'inbound'; s.walkSpeed = 0.5; __ugv.runWalk(); })()`, { awaitPromise: false });
        await cdp.eval(`__ugvRun.cap('trace impact — <b>who depends on this?</b>')`);
        if (DIAG) {
            await wait(B.walk * 0.45);
            const probe = await cdp.eval(`(async () => {
                const raf = await Promise.race([
                    new Promise(r => requestAnimationFrame(() => r('alive@' + Math.round(performance.now())))),
                    new Promise(r => setTimeout(() => r('STARVED'), 2500)),
                ]);
                const s = window.__ugv && __ugv.state;
                return { raf: raf, vis: document.visibilityState, hidden: document.hidden,
                    walkRunning: !!(s && s.walkRunning), walkActive: !!(s && s.walkActive) };
            })()`, { awaitPromise: true });
            log(`probe: ${JSON.stringify(probe)}`);
            const shot = await cdp.send('Page.captureScreenshot', { format: 'jpeg', quality: 80 });
            fs.writeFileSync('/var/folders/2q/lt4cn61n79ncvf61rdmhbtqr0000gn/T/opencode/diag-shot.jpg', Buffer.from(shot.data, 'base64'));
            log('manual screenshot saved');
        }
        await wait(DIAG ? B.walk * 0.4 : B.walk);
        await cdp.eval('__ugvRun.capHide()');
        await cdp.eval('__ugv.exitWalk(true); __ugv.exitFocus(); __ugv.clearSelection()');
        await wait(700);
    }

    // S6 — pull back to the whole map, title card, blackout
    mark('s6-outro');
    await cdp.eval('__ugvRun.rec(false)');
    await cdp.eval(`__ugv.frameGraph(${B.outro})`);
    await wait(B.outro * 0.45);
    await cdp.eval('__ugvRun.title()');
    await wait(B.titleHold);
    await cdp.eval('__ugvRun.blackout()');
    await wait(1700);
    mark('end');
    clearInterval(visPoll);
    if (hiddenAt.length) log(`hidden events: ${hiddenAt.join(', ')}`);
    const vis = await cdp.eval('JSON.stringify(window.__visLog || [])').catch(() => '[]');
    if (vis !== '[["visible",0]]') log(`visibility log: ${vis}`);

    await cdp.send('Page.stopScreencast');
    return marks;
}

// ── 6. Frames → ffconcat → mp4 ─────────────────────────────────────────────
function assemble(frames, framesDir, listPath) {
    frames.forEach((f, i) => {
        fs.writeFileSync(path.join(framesDir, `f${String(i).padStart(6, '0')}.jpg`), Buffer.from(f.data, 'base64'));
    });
    const lines = ['ffconcat version 1.0'];
    for (let i = 0; i < frames.length; i++) {
        const dur = i + 1 < frames.length
            ? Math.min(0.4, Math.max(1 / 120, frames[i + 1].ts - frames[i].ts))
            : 1 / FPS;
        lines.push(`file 'f${String(i).padStart(6, '0')}.jpg'`, `duration ${dur.toFixed(4)}`);
    }
    lines.push(`file 'f${String(frames.length - 1).padStart(6, '0')}.jpg'`);
    fs.writeFileSync(listPath, lines.join('\n'));
    fs.mkdirSync(path.dirname(OUT), { recursive: true });
    execSync(
        `ffmpeg -y -v error -f concat -safe 0 -i ${JSON.stringify(listPath)} ` +
        `-vf fps=${FPS},scale=${W}:${H},format=yuv420p -c:v libx264 -preset slow -crf 24 ` +
        `-movflags +faststart -an ${JSON.stringify(OUT)}`,
        { cwd: framesDir, stdio: 'inherit' },
    );
}

// ── 7. Run ──────────────────────────────────────────────────────────────────
const HARD_MS = QUICK ? 180_000 : 420_000;
let chromeProc = null, srv = null, userDataDir = null, done = false;

function killAll(code) {
    if (done) return; done = true;
    try { chromeProc?.kill(); } catch { /* already gone */ }
    if (userDataDir) { try { execSync(`pkill -9 -f ${JSON.stringify(userDataDir)} || true`); } catch { /* none */ } }
    try { srv?.close(); } catch { /* already closed */ }
    if (code != null) process.exitCode = code;
}
const watchdog = setTimeout(() => { console.error('[ugv] HARD TIMEOUT — killing everything'); killAll(1); }, HARD_MS);
process.on('SIGINT', () => killAll(130));

try {
    const { srv: s, port: httpPort } = await startServer(); srv = s;
    const debugPort = await freePort();
    userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ugv-chrome-'));
    log(`serving demo at http://127.0.0.1:${httpPort} · chrome debug :${debugPort}`);
    const { proc } = await launchChrome(debugPort, userDataDir); chromeProc = proc;

    const targets = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
    const page = targets.find(t => t.type === 'page');
    if (!page?.webSocketDebuggerUrl) throw new Error('no page target');
    const cdp = await Cdp.connect(page.webSocketDebuggerUrl);
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    // Preventive: keep the headless page focused and active — a hidden
    // document starves rAF and freezes the capture.
    await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: true }).catch(() => {});
    await cdp.send('Page.setWebLifecycleState', { state: 'active' }).catch(() => {});
    // Pin the viewport — --window-size is the outer window and lands 87px
    // short in headless, which would give odd-sized frames (h264 wants even).
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: W, height: H, deviceScaleFactor: 1, mobile: false });
    await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: COVER_BOOT });

    const frames = [];
    const capStart = Date.now();
    cdp.on('Page.screencastFrame', (p) => {
        frames.push({ ts: p.metadata?.timestamp ?? frames.length / FPS, data: p.data, recv: Date.now() - capStart });
        cdp.send('Page.screencastFrameAck', { sessionId: p.sessionId }).catch(() => {});
    });

    const url = `http://127.0.0.1:${httpPort}/?r=cosmos`;
    const marks = await playScenes(cdp, url);
    await wait(400); // drain in-flight frames

    log(`captured ${frames.length} frames`);
    if (frames.length < (QUICK ? 60 : 400)) throw new Error('too few frames captured — something went wrong');
    try {
        const vis = await cdp.eval('JSON.stringify(window.__visLog || [])');
        if (vis && vis !== '[["visible",0]]') log(`visibility: ${vis}`);
    } catch { /* page may be gone */ }
    const tsSpan = frames.length > 1 ? frames[frames.length - 1].ts - frames[0].ts : 0;
    const recvSpan = frames.length > 1 ? (frames[frames.length - 1].recv - frames[0].recv) / 1000 : 0;
    log(`ts span ${tsSpan.toFixed(1)}s · arrival span ${recvSpan.toFixed(1)}s`);
    const gaps = [];
    for (let i = 1; i < frames.length; i++) {
        const g = frames[i].recv - frames[i - 1].recv;
        if (g > 700) gaps.push([frames[i - 1].recv / 1000, frames[i].recv / 1000, g]);
    }
    if (gaps.length) log(`arrival gaps >0.7s: ${gaps.slice(0, 12).map(([a, b]) => `${a.toFixed(1)}→${b.toFixed(1)}`).join(', ')}${gaps.length > 12 ? ' …' : ''}`);

    const framesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ugv-frames-'));
    const listPath = path.join(framesDir, 'list.ffconcat');
    assemble(frames, framesDir, listPath);

    const probe = execSync(
        `ffprobe -v error -show_entries format=duration,size -of default=nw=1 ${JSON.stringify(OUT)}`,
    ).toString().trim();
    log(`encoded ${OUT}`);
    console.log(probe);
    for (const m of marks) console.log(`  mark ${m.name} +${((m.at - marks[0].at) / 1000).toFixed(1)}s`);

    if (!KEEP_FRAMES) fs.rmSync(framesDir, { recursive: true, force: true });
    else log(`frames kept at ${framesDir}`);

    done = true;
    clearTimeout(watchdog);
    killAll(0);
    // straggler check — a dead driver must not leave a live browser (§10r)
    const left = (() => { try { return execSync(`pgrep -f ${JSON.stringify(userDataDir)} || true`).toString().trim(); } catch { return ''; } })();
    if (left) { execSync(`pkill -9 -f ${JSON.stringify(userDataDir)} || true`); log('killed chrome stragglers'); }
    process.exit(0);
} catch (err) {
    console.error(`[ugv] FAILED: ${err.message}`);
    clearTimeout(watchdog);
    killAll(1);
    process.exit(1);
}
