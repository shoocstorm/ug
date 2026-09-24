// What freezing the graph buys while an in-browser model generates.
//
//   cargo build --bin ug
//   KEEP_PROFILE=/tmp/ug-bench node scripts/local-llm-bench.mjs
//
// Runs the same generation with the freeze on and off, three times each, and
// reports CPU-milliseconds and wall-milliseconds *per generated character* —
// not per answer: a model at temperature 0 through a GPU backend still varies
// its answer length by a third between runs, and per-answer numbers measure
// that instead of the change.
//
// It deliberately launches Chrome with a **visible window**. Under
// `--headless=new` there is no compositing, the idle page costs 3% of a core,
// and the thing being measured does not happen — the first version of this
// script reported a 39% win that repeated runs turned into -2%. See
// Agents.md §11s.
import { spawn, execSync } from 'node:child_process';

const PORT = 8141, CDP = 9341;
const UG = process.env.UG_BIN ?? new URL('../native/target/debug/ug', import.meta.url).pathname;
const CHROME = process.env.CHROME_PATH ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PROFILE = process.env.KEEP_PROFILE ?? '/tmp/ug-local-llm-bench';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(fn, label, ms = 180000) {
  const t0 = Date.now();
  for (;;) {
    try { const v = await fn(); if (v) return v; } catch (_) {}
    if (Date.now() - t0 > ms) throw new Error(`timeout: ${label}`);
    await sleep(400);
  }
}

/** Total CPU seconds burned by every process under this Chrome profile. */
function browserCpuSeconds() {
  const out = execSync(
    `ps -Ao pid,time,command | grep -F -- "--user-data-dir=${PROFILE}" | grep -v grep || true`,
    { encoding: 'utf8' }
  );
  let total = 0;
  for (const line of out.trim().split('\n')) {
    if (!line) continue;
    const m = line.trim().match(/^\d+\s+([\d:.]+)/);
    if (!m) continue;
    const parts = m[1].split(':').map(Number);
    total += parts.length === 3
      ? parts[0] * 3600 + parts[1] * 60 + parts[2]
      : parts[0] * 60 + parts[1];
  }
  return total;
}

const server = spawn(UG, ['serve', '-p', String(PORT), '--project', 'ug'], { stdio: 'ignore' });
let chrome, ws, nextId = 1;
const pending = new Map();
const cdp = (method, params = {}) => {
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise((res, rej) => {
    pending.set(id, { res, rej });
    setTimeout(() => rej(new Error(method + ' timeout')), 120000);
  });
};
const evaluate = async (expression) =>
  (await cdp('Runtime.evaluate', { expression, returnByValue: true })).result.value;

async function generate(tokens) {
  const t0 = Date.now();
  const cpu0 = browserCpuSeconds();
  // Sampled mid-generation below; the class is the page's own record of
  // whether the renderer is stopped.
  const res = fetch(`http://127.0.0.1:${PORT}/api/llm/local/v1/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      messages: [{ role: 'user', content: 'Write a paragraph about graphs.' }],
      max_tokens: tokens,
      seed: 42,
      // Deterministic: the two arms must generate the same text, or the
      // comparison is between answer lengths rather than between renderers.
      temperature: 0,
    }),
  }).then((r) => r.json());
  // Sampled while the answer is still streaming — after it resolves the
  // graph is already back.
  await sleep(450);
  const sample = await evaluate(`document.body.classList.contains('llm-graph-paused')`);
  const frozen = sample;
  const res2 = await res;
  const secs = (Date.now() - t0) / 1000;
  const cpu = browserCpuSeconds() - cpu0;
  const text = res2.choices?.[0]?.message?.content ?? '';
  return { secs, cpu, chars: text.length, sawFrozen: frozen };
}

try {
  await waitFor(() => fetch(`http://127.0.0.1:${PORT}/healthz`).then((r) => r.ok), 'server');
  chrome = spawn(CHROME, [
    '--window-position=40,40', `--remote-debugging-port=${CDP}`, `--user-data-dir=${PROFILE}`,
    '--no-first-run', '--disable-dev-shm-usage', '--window-size=1600,1000',
    `http://127.0.0.1:${PORT}/`,
  ], { stdio: 'ignore' });

  const target = await waitFor(async () => {
    const l = await fetch(`http://127.0.0.1:${CDP}/json/list`).then((r) => r.json());
    return l.find((t) => t.type === 'page' && t.url.includes(`:${PORT}`));
  }, 'target');
  ws = new WebSocket(target.webSocketDebuggerUrl);
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { res, rej } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result);
    }
  });
  await new Promise((r) => ws.addEventListener('open', r));
  await cdp('Runtime.enable');
  await waitFor(() => evaluate(`document.readyState === 'complete' && !!document.getElementById('llm-overlay')`), 'page');

  // Into a project so the graph is actually rendering.
  await evaluate(`(() => {
    const card = [...document.querySelectorAll('#kb-manager .kb-card')].find(c => c.classList.contains('active'))
      || document.querySelector('#kb-manager .kb-card');
    if (card) card.click();
  })()`);
  await sleep(8000);
  console.log('renderer:', await evaluate(`document.body.dataset.renderer`));

  // Baseline: what the page costs with nothing else happening.
  const idle0 = browserCpuSeconds();
  await sleep(6000);
  console.log(`idle page: ${(browserCpuSeconds() - idle0).toFixed(1)} CPU-seconds over 6s wall`);

  await evaluate(`document.getElementById('llm-open-btn').click()`);
  await sleep(1500);
  await evaluate(`document.getElementById('llm-advanced').open = true`);
  await evaluate(`(() => {
    const card = [...document.querySelectorAll('#llm-models .llm-model')].find(c => c.textContent.includes('SmolLM2 135M'));
    const btn = [...card.querySelectorAll('button')].find(b => /Download|Use this/.test(b.textContent));
    if (btn) btn.click();
  })()`);
  await waitFor(() => evaluate(`document.getElementById('llm-status-dot').className.includes('live')`), 'model', 600000);
  await evaluate(`document.getElementById('llm-close').click()`);
  await sleep(1500);

  const setFreeze = async (on) => {
    await evaluate(`(() => {
      document.getElementById('llm-open-btn').click();
      const box = document.getElementById('llm-pause-graph');
      if (box.checked !== ${on}) { box.click(); }
      document.getElementById('llm-close').click();
      return box.checked;
    })()`);
    await sleep(800);
  };

  const rows = [];
  for (const freeze of [true, false, true, false, true, false]) {
    await setFreeze(freeze);
    const r = await generate(320);
    rows.push({ freeze, ...r });
    console.log(
      `freeze=${String(freeze).padEnd(5)} ${r.secs.toFixed(1)}s wall · ` +
      `${r.cpu.toFixed(1)} CPU-seconds · ${r.chars} chars · ` +
      `${(1000 * r.cpu / Math.max(1, r.chars)).toFixed(2)} CPU-ms/char · graph ${r.sawFrozen ? 'FROZEN' : 'live'} mid-answer`
    );
    await sleep(2000);
  }

  const avg = (f) => {
    const xs = rows.filter((r) => r.freeze === f);
    const chars = xs.reduce((a, b) => a + b.chars, 0);
    return {
      secs: xs.reduce((a, b) => a + b.secs, 0) / xs.length,
      cpu: xs.reduce((a, b) => a + b.cpu, 0) / xs.length,
      cpuPerChar: 1000 * xs.reduce((a, b) => a + b.cpu, 0) / Math.max(1, chars),
      msPerChar: 1000 * xs.reduce((a, b) => a + b.secs, 0) / Math.max(1, chars),
    };
  };
  const on = avg(true), off = avg(false);
  console.log(`\nfrozen:  ${on.secs.toFixed(1)}s wall, ${on.cpu.toFixed(1)} CPU-s`);
  console.log(`live:    ${off.secs.toFixed(1)}s wall, ${off.cpu.toFixed(1)} CPU-s`);
  console.log(`per character — frozen ${on.cpuPerChar.toFixed(2)} CPU-ms / ${on.msPerChar.toFixed(2)} ms wall`);
  console.log(`per character — live   ${off.cpuPerChar.toFixed(2)} CPU-ms / ${off.msPerChar.toFixed(2)} ms wall`);
  console.log(`saved: ${(100 * (off.cpuPerChar - on.cpuPerChar) / off.cpuPerChar).toFixed(0)}% of the CPU per character, ` +
    `${(100 * (off.msPerChar - on.msPerChar) / off.msPerChar).toFixed(0)}% of the wall clock`);
} finally {
  try { ws?.close(); } catch (_) {}
  chrome?.kill('SIGKILL');
  server.kill('SIGKILL');
}
