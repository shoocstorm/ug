// Manual end-to-end check for the in-browser model (serve/local_llm.rs +
// vis/js/28-local-llm.js). Nothing else covers the browser half: the Rust
// tests drive the hub and the routes, but only this actually downloads a
// GGUF, runs it in a tab and reads the answer back out of ug's own chat.
//
//   cargo build --bin ug        # the binary this drives
//   node scripts/local-llm-e2e.mjs
//
// It needs Chrome, a project called `ug` under ~/.ug with vectors, and about
// 640 MB of download the first time. `KEEP_PROFILE=<dir>` reuses a Chrome
// profile so the model stays cached between runs — with it the whole check
// takes under a minute.
//
// Why CDP rather than clicking through a headless page: the page's script is
// one ES module, so nothing it declares is reachable from `Runtime.evaluate`.
// Everything here therefore goes through the DOM, the way a person would —
// which is also what makes it a UI test and not just an API one.
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const PORT = Number(process.env.UG_PORT ?? 8123);
const CDP = 9333;
const UG = process.env.UG_BIN ?? new URL('../native/target/debug/ug', import.meta.url).pathname;
const CHROME = process.env.CHROME_PATH ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PROFILE = process.env.KEEP_PROFILE || (await mkdtemp(join(tmpdir(), 'ug-llm-')));
// Which card to click. `MODEL='SmolLM2 135M'` is the 138 MB sanity model and
// makes a full run of this take about a minute from cold.
const MODEL = process.env.MODEL ?? 'Qwen3 0.6B';
// Context window to run it with. The graph toolbox only fits above a
// threshold the server publishes, so this decides which chat path is tested.
const NCTX = process.env.NCTX ?? '';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log('·', ...a);

async function waitFor(fn, label, ms = 60000) {
  const t0 = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (_) {}
    if (Date.now() - t0 > ms) throw new Error(`timed out waiting for ${label}`);
    await sleep(500);
  }
}

// ---- server -------------------------------------------------------------
const server = spawn(UG, ['serve', '-p', String(PORT), '--project', 'ug'], {
  stdio: ['ignore', 'pipe', 'pipe'],
});
let serverLog = '';
server.stdout.on('data', (d) => (serverLog += d));
server.stderr.on('data', (d) => (serverLog += d));

// ---- chrome -------------------------------------------------------------
let chrome;
let ws;
let nextId = 1;
const pending = new Map();

async function cdp(method, params = {}) {
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise((res, rej) => {
    pending.set(id, { res, rej });
    setTimeout(() => rej(new Error(`${method} timed out`)), 120000);
  });
}

async function evaluate(expression, awaitPromise = false) {
  const r = await cdp('Runtime.evaluate', {
    expression,
    awaitPromise,
    returnByValue: true,
  });
  if (r.exceptionDetails) {
    throw new Error(r.exceptionDetails.exception?.description || 'evaluate threw');
  }
  return r.result.value;
}

async function main() {
  await waitFor(
    () => fetch(`http://127.0.0.1:${PORT}/healthz`).then((r) => r.ok),
    'ug serve'
  );
  log(`ug serve up on ${PORT}`);

  const caps = await fetch(`http://127.0.0.1:${PORT}/api/capabilities`).then((r) => r.json());
  log('capabilities:', JSON.stringify({
    db: caps.db_ready, embedder: caps.embedder_ready, search: caps.search_ready,
    chat: caps.chat, local_llm: caps.local_llm,
  }));

  chrome = spawn(CHROME, [
    '--headless=new',
    `--remote-debugging-port=${CDP}`,
    `--user-data-dir=${PROFILE}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-dev-shm-usage',
    '--disable-gpu',
    '--disable-software-rasterizer',
    `http://127.0.0.1:${PORT}/`,
  ], { stdio: ['ignore', 'pipe', 'pipe'] });
  let chromeLog = '';
  chrome.stderr.on('data', (d) => (chromeLog += d));

  const target = await waitFor(async () => {
    const list = await fetch(`http://127.0.0.1:${CDP}/json/list`).then((r) => r.json());
    return list.find((t) => t.type === 'page' && t.url.includes(`:${PORT}`));
  }, 'a chrome page target');

  ws = new WebSocket(target.webSocketDebuggerUrl);
  ws.addEventListener('message', (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      const { res, rej } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? rej(new Error(JSON.stringify(msg.error))) : res(msg.result);
    }
  });
  await new Promise((r) => ws.addEventListener('open', r));
  await cdp('Runtime.enable');
  await cdp('Page.enable');
  log('attached to the page');

  // The target appears before the document is parsed, so an evaluate here
  // can land in the initial empty document — where `crossOriginIsolated` is
  // false and every id is null. Wait for the real page.
  await waitFor(
    () => evaluate(`document.readyState === 'complete' && !!document.getElementById('llm-overlay')`),
    'the page to finish loading'
  );

  // 1. isolation + panel exist
  const env = await evaluate(`JSON.stringify({
    isolated: crossOriginIsolated,
    sab: typeof SharedArrayBuffer !== 'undefined',
    panel: !!document.getElementById('llm-overlay'),
    button: !!document.getElementById('llm-open-btn'),
    threads: navigator.hardwareConcurrency,
  })`);
  log('page env:', env);
  const envObj = JSON.parse(env);
  if (!envObj.isolated || !envObj.panel) throw new Error('page is not set up for a local model');

  // 2. open the panel and check it rendered the catalog
  await evaluate(`document.getElementById('llm-open-btn').click()`);
  await sleep(1200);
  const models = await evaluate(
    `[...document.querySelectorAll('#llm-models .llm-model-name')].map(e => e.textContent).join(' | ')`
  );
  log('catalog:', models);
  log('env chips:', await evaluate(`document.getElementById('llm-env').textContent.trim().replace(/\\s+/g,' ')`));

  // 3. click "Download & use" on the 0.6B
  if (NCTX) {
    await evaluate(`(() => {
      const sel = document.getElementById('llm-ctx');
      sel.value = ${JSON.stringify(NCTX)};
      sel.dispatchEvent(new Event('change'));
    })()`);
    log('context window set to', NCTX);
  }
  log('card buttons:', await evaluate(`(() => {
    const cards = [...document.querySelectorAll('#llm-models .llm-model')];
    return cards.map(c => c.querySelector('.llm-model-name').textContent + ' => ' +
      [...c.querySelectorAll('button')].map(b => b.textContent + (b.disabled ? '(disabled)' : '')).join(', ')).join(' | ');
  })()`));
  log('clicked:', await evaluate(`(() => {
    const cards = [...document.querySelectorAll('#llm-models .llm-model')];
    const card = cards.find(c => c.textContent.includes(${JSON.stringify(MODEL)}));
    const btn = [...card.querySelectorAll('button')].find(b => /Download|Use this/.test(b.textContent));
    if (!btn) return 'already running (auto-start)';
    btn.click();
    return btn.textContent;
  })()`));
  log(`clicked ${MODEL}; waiting for the model to come up…`);

  const status = await waitFor(async () => {
    const s = await evaluate(`JSON.stringify({
      text: document.getElementById('llm-status-text').textContent,
      pct: document.getElementById('llm-progress-pct').textContent,
      facts: document.getElementById('llm-facts').textContent,
      dot: document.getElementById('llm-status-dot').className,
    })`);
    const o = JSON.parse(s);
    process.stdout.write(`\r   ${o.text} ${o.pct}            `);
    return o.dot.includes('live') ? o : null;
  }, 'the model to attach', 900000);
  console.log('');
  log('panel says:', status.text, '·', status.facts);

  // 4. the server should now report the browser as its chat endpoint
  const caps2 = await fetch(`http://127.0.0.1:${PORT}/api/capabilities`).then((r) => r.json());
  log('server chat:', JSON.stringify(caps2.chat), 'attached:', JSON.stringify(caps2.local_llm.attached));

  // 5. ask the bridge directly, non-streaming
  const t0 = Date.now();
  const completion = await fetch(`http://127.0.0.1:${PORT}/api/llm/local/v1/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model: 'whatever',
      messages: [{ role: 'user', content: 'In one sentence: what is a call graph?' }],
      max_tokens: 96,
      temperature: 0.2,
    }),
  }).then((r) => r.json());
  log(`non-streamed answer in ${((Date.now() - t0) / 1000).toFixed(1)}s:`);
  console.log('   ', JSON.stringify(completion.choices?.[0]?.message?.content));

  // 6. and streaming, the way ChatClient does it
  const t1 = Date.now();
  const res = await fetch(`http://127.0.0.1:${PORT}/api/llm/local/v1/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      messages: [{ role: 'user', content: 'Name three things a code graph is good for.' }],
      max_tokens: 96,
      stream: true,
    }),
  });
  let chunks = 0;
  let text = '';
  let raw = '';
  const decoder = new TextDecoder();
  for await (const buf of res.body) {
    const chunkText = decoder.decode(buf, { stream: true });
    raw += chunkText;
    for (const line of chunkText.split('\n')) {
      if (!line.startsWith('data: ')) continue;
      const payload = line.slice(6).trim();
      if (payload === '[DONE]') continue;
      try {
        const j = JSON.parse(payload);
        const d = j.choices?.[0]?.delta?.content;
        if (d) { text += d; chunks++; }
      } catch (_) {}
    }
  }
  log(`streamed ${chunks} chunks in ${((Date.now() - t1) / 1000).toFixed(1)}s:`);
  console.log('   ', JSON.stringify(text.slice(0, 200)));
  if (!chunks) console.log('   RAW SSE:', JSON.stringify(raw.slice(0, 1200)));

  // 7. the real thing: ug's own agentic chat, answered by the tab
  if (caps2.search_ready) {
    const t2 = Date.now();
    const answer = await fetch(`http://127.0.0.1:${PORT}/api/chat`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      // Tools on — the default, and the path that used to send a 10 000
      // token toolbox to a 4 000 token model.
      body: JSON.stringify({ query: 'What does build_router do?', k: 4 }),
    }).then((r) => r.json());
    log(`/api/chat in ${((Date.now() - t2) / 1000).toFixed(1)}s (tools=${answer.tool_calls ?? '?'}, model=${answer.chat_model ?? '?'}):`);
    console.log('   ', JSON.stringify(String(answer.answer || answer.error || '').slice(0, 400)));
  } else {
    log('/api/chat skipped: this project has no vectors (search_ready=false)');
  }

  // 7b. a tour: the other thing the model narrates, and the one whose
  // planning prompt is auto-scaled and so needs the hard cap to hold.
  if (caps2.search_ready) {
    const t3 = Date.now();
    const tour = await fetch(`http://127.0.0.1:${PORT}/api/tour`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ query: 'how does serving work', max_stops: 3, k: 8 }),
    }).then((r) => r.json());
    const stops = tour.stops || [];
    log(`/api/tour in ${((Date.now() - t3) / 1000).toFixed(1)}s: ${stops.length} stops, model=${tour.model || '?'}`);
    console.log('    intro:', JSON.stringify(String(tour.intro || tour.error || '').slice(0, 200)));
    if (stops[0]) console.log('    stop 1:', stops[0].name, '—', JSON.stringify(String(stops[0].narration || '').slice(0, 160)));
  }

  // 8. tear down from the UI and check chat went back to the user's endpoint
  await evaluate(`document.getElementById('llm-stop').click()`);
  await sleep(2000);
  const caps3 = await fetch(`http://127.0.0.1:${PORT}/api/capabilities`).then((r) => r.json());
  log('after stopping:', JSON.stringify(caps3.chat), JSON.stringify(caps3.local_llm.attached));
  console.log('\n✅ e2e passed');
}

try {
  await main();
} catch (e) {
  console.error('\n❌', e.message);
  console.error('--- server log tail ---\n' + serverLog.split('\n').slice(-25).join('\n'));
  process.exitCode = 1;
} finally {
  try { ws?.close(); } catch (_) {}
  chrome?.kill('SIGKILL');
  server.kill('SIGKILL');
  if (!process.env.KEEP_PROFILE) await rm(PROFILE, { recursive: true, force: true });
}
