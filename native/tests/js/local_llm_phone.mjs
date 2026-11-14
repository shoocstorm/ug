// How the in-browser model loads on a phone, checked under `node`.
//
// iOS closes a Safari tab that grows past its memory limit, with no error:
// the page reloads. `js/28-local-llm.js` avoids that with a smaller file, a
// CPU-only load at a short window, a lean KV cache, and the build Safari can
// actually run. None of it can be seen failing from a desktop — the desktop
// path is a different branch of every function here — so the decisions are
// lifted out of the shipped part with a string slice (Agents.md §10r) and
// checked directly. They cannot pass against a copy that has drifted.
//
// argv: <28-local-llm.js>

import fs from 'node:fs';

const [partPath] = process.argv.slice(2);
if (!partPath) {
    console.error('usage: node local_llm_phone.mjs <28-local-llm.js>');
    process.exit(2);
}
const src = fs.readFileSync(partPath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

// The part's functions live at 8-space indent; the closing brace at that
// depth ends each one.
function lift(name) {
    const pad = ' '.repeat(8);
    const start = src.indexOf('\n' + pad + 'function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found — renamed?`);
    const end = src.indexOf('\n' + pad + '}', start);
    if (end < 0) throw new Error(`${name} has no closing brace`);
    return src.slice(start, end + pad.length + 2);
}

function liftCatalog() {
    const start = src.indexOf('const LLM_CATALOG = [');
    const end = src.indexOf('\n        ];', start);
    if (start < 0 || end < 0) throw new Error('LLM_CATALOG not found');
    return src.slice(start, end + 11);
}

const names = [
    'llmModelUrl', 'llmDetectDevice', 'llmDefaultBuildRuns', 'llmLoadPlan', 'llmStartLevel',
    'llmLoadParams', 'llmPlanText', 'llmListing', 'llmFailureKind', 'llmIsFatalLog', 'llmFitMessages',
];
const unit = new Function(
    liftCatalog() + '\n' + names.map(lift).join('\n') + `\nreturn { LLM_CATALOG, ${names.join(', ')} };`,
)();
const {
    LLM_CATALOG, llmModelUrl, llmDetectDevice, llmDefaultBuildRuns, llmLoadPlan, llmStartLevel,
    llmLoadParams, llmListing, llmFailureKind, llmIsFatalLog, llmFitMessages,
} = unit;

const IPHONE = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1';
const MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15';
const ANDROID = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Mobile Safari/537.36';

console.log('a phone is told apart');
{
    must('an iPhone is a phone', llmDetectDevice({ userAgent: IPHONE }).mobile && llmDetectDevice({ userAgent: IPHONE }).ios);
    must('an Android phone is a phone', llmDetectDevice({ userAgent: ANDROID }).mobile && !llmDetectDevice({ userAgent: ANDROID }).ios);
    must('a Mac is not', !llmDetectDevice({ userAgent: MAC, maxTouchPoints: 0 }).mobile);
    // iPadOS sends the Mac user agent; only the touch screen gives it away.
    must('an iPad claiming to be a Mac is', llmDetectDevice({ userAgent: MAC, maxTouchPoints: 5 }).ios);
    must('no navigator, no phone', !llmDetectDevice(undefined).mobile);
}

console.log('the build is chosen by compiling');
{
    const ok = { Suspending: function () {}, validate: () => true };
    must('JSPI and a Memory64 module that compiles: the default build', llmDefaultBuildRuns(ok));
    // WebKit: `new WebAssembly.Memory({address:'i64'})` succeeds and the
    // module still does not compile. This is the case wllama gets wrong.
    must('a Memory64 module that does not compile: compat', !llmDefaultBuildRuns({ Suspending: function () {}, validate: () => false }));
    must('no JSPI: compat', !llmDefaultBuildRuns({ validate: () => true }));
    must('a validate that throws: compat', !llmDefaultBuildRuns({ Suspending: 1, validate: () => { throw new Error('x'); } }));
    must('the probe is a real module on this runtime', typeof WebAssembly.validate(new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x04, 0x01])) === 'boolean');
}

console.log('a phone starts lighter and steps down');
{
    const prefs = { nCtx: 8192, gpu: true };
    const phone = { mobile: true, ios: true };
    const desk = { mobile: false, ios: false };
    must('a desktop starts at what the panel says', llmStartLevel(desk, undefined) === 0 && llmStartLevel(desk, 2) === 0);
    must('a phone starts at level 1', llmStartLevel(phone, undefined) === 1);
    must('a phone that failed at 1 starts at 2', llmStartLevel(phone, 2) === 2);
    must('and never past 2', llmStartLevel(phone, 9) === 2);

    const p0 = llmLoadPlan(prefs, 0, true);
    const p1 = llmLoadPlan(prefs, 1, true);
    const p2 = llmLoadPlan(prefs, 2, true);
    must('level 0 is the panel: its window, the GPU', p0.nCtx === 8192 && p0.gpu === true);
    must('level 0 without WebGPU is CPU', llmLoadPlan(prefs, 0, false).gpu === false);
    must('level 1 is CPU at 2048', p1.nCtx === 2048 && p1.gpu === false);
    must('level 2 is CPU at 1024', p2.nCtx === 1024 && p2.gpu === false);
    must('a smaller chosen window is kept', llmLoadPlan({ nCtx: 1024, gpu: true }, 1, true).nCtx === 1024);

    const d = llmLoadParams(p0, 6);
    must('a desktop load is what it always was',
        JSON.stringify(d) === JSON.stringify({ n_ctx: 8192, n_gpu_layers: 999, n_threads: 6, reasoning_format: 'none' }));
    const m = llmLoadParams(p1, 4);
    must('a phone load never touches the GPU', m.n_gpu_layers === 0 && m.n_ctx === 2048);
    must('a phone load halves the KV cache', m.cache_type_k === 'q8_0' && m.cache_type_v === 'q8_0' && m.flash_attn === true);
    must('a phone load keeps one sequence and small batches', m.n_parallel === 1 && m.n_batch === 256 && m.n_ubatch === 256);
}

console.log('a phone is offered only what it can hold');
{
    const phone = { mobile: true, ios: true };
    const desk = { mobile: false, ios: false };
    const offered = LLM_CATALOG.filter(e => llmListing(e, phone, false) === 'offer');
    must('a phone is offered something', offered.length > 0);
    must('nothing a phone is offered is over 450 MB', offered.every(e => e.bytes <= 450 * 1024 * 1024));
    must('exactly one of them is the pick', offered.filter(e => e.phonePick).length === 1);
    const big = LLM_CATALOG.find(e => e.id === 'qwen3-0.6b');
    must('the 639 MB file is hidden on a phone', llmListing(big, phone, false) === 'hide');
    must('unless it is already downloaded: then it can only be deleted', llmListing(big, phone, true) === 'tooBig');
    must('a GGUF added by hand is always offered', llmListing({ custom: true }, phone, false) === 'offer');
    const pick = LLM_CATALOG.find(e => e.phonePick);
    must('the phone file is not listed on a desktop', llmListing(pick, desk, false) === 'hide');
    must('a desktop still sees the rest', LLM_CATALOG.filter(e => llmListing(e, desk, false) === 'offer').length === LLM_CATALOG.length - 1);
    must('the phone file is pinned to a commit', /\/resolve\/[0-9a-f]{40}\/Qwen3-0\.6B-Q4_K_M\.gguf$/.test(llmModelUrl(pick)));
    must('the others still follow main', llmModelUrl(big).includes('/resolve/main/'));
}

console.log('a failure is read for what it is');
{
    must('an abort is memory', llmFailureKind('Aborted(OOM)') === 'memory');
    must('so is a heap that cannot grow', llmFailureKind('Cannot enlarge memory, requested 2147487744 bytes') === 'memory');
    // Before memory: asking for less of it does not help a build that cannot compile.
    must('a compile error is not memory', llmFailureKind('Aborted(CompileError: WebAssembly.Module doesn\'t parse at byte 12: Memory64 is not enabled)') === 'compile');
    must('a stalled load is a timeout', llmFailureKind('timed out starting the model (last log: load_tensors)') === 'timeout');
    must('a dropped download is the network', llmFailureKind('TypeError: Failed to fetch') === 'network');
    must('a dead worker is seen in the log', llmIsFatalLog('worker sent an error') && llmIsFatalLog('Aborted(). Build with -sASSERTIONS'));
    must('an ordinary line is not', !llmIsFatalLog('llama_model_loader: loaded meta data with 28 key-value pairs'));
}

console.log('a prompt is cut to the window');
{
    const sys = { role: 'system', content: 's'.repeat(300) };
    const size = (ms) => ms.reduce((n, m) => n + m.content.length + 12, 0);
    const short = [sys, { role: 'user', content: 'hi' }];
    must('a prompt that fits is left alone', JSON.stringify(llmFitMessages(short, 2048, 512)) === JSON.stringify(short));

    const long = [sys];
    for (let i = 0; i < 12; i++) long.push({ role: i % 2 ? 'assistant' : 'user', content: String(i).repeat(900) });
    const cut = llmFitMessages(long, 2048, 512);
    must('older turns go first', cut.length < long.length && cut[0] === sys && cut[cut.length - 1] === long[long.length - 1]);
    must('and it then fits', size(cut) <= (2048 - 512 - 48) * 3);

    // The demo sends two system messages: its instructions, then the
    // retrieved context. History goes before either of them does.
    const pack = { role: 'system', content: 'p'.repeat(2400) };
    const turn = [sys, pack, { role: 'user', content: 'a'.repeat(1500) }, { role: 'assistant', content: 'b'.repeat(1500) }, { role: 'user', content: 'q' }];
    const kept = llmFitMessages(turn, 2048, 512);
    must('the retrieved context outlives the history', kept.includes(sys) && kept.includes(pack) && kept[kept.length - 1].content === 'q' && kept.length < turn.length);

    const huge = [sys, { role: 'user', content: 'x'.repeat(20000) }];
    const tight = llmFitMessages(huge, 1024, 256);
    must('one oversized message is cut, not dropped', tight.length === 2 && tight[1].content.length < 20000 && tight[1].content.endsWith('…'));
    must('to the window', size(tight) <= (1024 - 256 - 48) * 3);
    must('the input is not modified', huge[1].content.length === 20000);
    // An assistant turn that is only tool calls has an empty content.
    must('a message with no text does not break it', llmFitMessages([sys, { role: 'assistant', content: '', tool_calls: [{}] }, { role: 'user', content: 'q' }], 2048, 512).length === 3);
}

console.log(failures ? `\n${failures} check(s) failed` : '\nthe phone load plan holds');
process.exit(failures ? 1 : 0);
