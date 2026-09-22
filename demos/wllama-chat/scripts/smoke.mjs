// Headless end-to-end check: serve the demo, drive smoke.html in Chrome,
// wait for the page to POST its result back, print it, kill Chrome.
//
//   node scripts/smoke.mjs            # 1.2 MB model, ~10s
//   node scripts/smoke.mjs qwen       # real Qwen3 0.6B, 639 MB download
//   node scripts/smoke.mjs qwen --gpu # offload to WebGPU (needs a real GPU)
//   node scripts/smoke.mjs tiny --head  # watch it in a visible window
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { makeServer } from '../server.mjs';

const args = process.argv.slice(2);
const model = args.find((a) => !a.startsWith('-')) ?? 'tiny';
const headful = args.includes('--head');
const gpu = args.includes('--gpu');
const TIMEOUT_MS = Number(process.env.SMOKE_TIMEOUT ?? (model === 'qwen' ? 900_000 : 180_000));

const CHROME =
  process.env.CHROME_PATH ??
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

let resolveResult;
const result = new Promise((r) => (resolveResult = r));

const server = makeServer({
  routes: {
    '/__result': (req, res) => {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        res.writeHead(204).end();
        try {
          resolveResult(JSON.parse(body));
        } catch (e) {
          resolveResult({ ok: false, error: `bad payload: ${e.message}` });
        }
      });
    },
  },
});

await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;
const url = `http://127.0.0.1:${port}/smoke.html?model=${model}&gpu=${gpu ? 1 : 0}`;
console.log(`serving on ${port} · driving ${url}`);

const profile = await mkdtemp(join(tmpdir(), 'wllama-smoke-'));
const chrome = spawn(
  CHROME,
  [
    headful ? '--new-window' : '--headless=new',
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    '--disable-background-timer-throttling',
    ...(gpu ? ['--enable-unsafe-swiftshader'] : ['--disable-gpu', '--disable-software-rasterizer']),
    url,
  ],
  { stdio: ['ignore', 'pipe', 'pipe'] }
);
chrome.stderr.on('data', (d) => process.env.VERBOSE && process.stderr.write(d));

const timeout = new Promise((r) =>
  setTimeout(() => r({ ok: false, error: `timed out after ${TIMEOUT_MS / 1000}s` }), TIMEOUT_MS)
);

const payload = await Promise.race([result, timeout]);

chrome.kill('SIGKILL');
server.close();
await rm(profile, { recursive: true, force: true });

const { log = [], ...rest } = payload;
if (log.length) console.log('\n--- page log ---\n' + log.join('\n'));
console.log('\n--- result ---\n' + JSON.stringify(rest, null, 2));
console.log(payload.ok ? '\n✅ wllama works in the browser' : '\n❌ smoke test failed');
process.exit(payload.ok ? 0 : 1);
