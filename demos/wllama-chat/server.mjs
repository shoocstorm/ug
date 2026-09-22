// Zero-dependency static server with the COOP/COEP headers wllama needs for
// SharedArrayBuffer (multi-thread WASM). `npx serve` works too — see README.
import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('.', import.meta.url));

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.wasm': 'application/wasm',
  '.map': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
};

const CROSS_ORIGIN_ISOLATION = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Cross-Origin-Embedder-Policy': 'require-corp',
  'Cross-Origin-Resource-Policy': 'cross-origin',
};

/**
 * @param {{root?: string, routes?: Record<string, (req, res) => void>}} opts
 */
export function makeServer({ root = ROOT, routes = {} } = {}) {
  return createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost');
    const route = routes[url.pathname];
    if (route) return route(req, res);

    let pathname = decodeURIComponent(url.pathname);
    if (pathname.endsWith('/')) pathname += 'index.html';
    const file = join(root, normalize(pathname).replace(/^(\.\.[/\\])+/, ''));

    try {
      const info = await stat(file);
      if (!info.isFile()) throw new Error('not a file');
      res.writeHead(200, {
        ...CROSS_ORIGIN_ISOLATION,
        'Content-Type': MIME[extname(file)] ?? 'application/octet-stream',
        'Content-Length': info.size,
        'Cache-Control': 'no-cache',
      });
      createReadStream(file).pipe(res);
    } catch {
      res.writeHead(404, { 'Content-Type': 'text/plain' });
      res.end('404 ' + pathname);
    }
  });
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const port = Number(process.env.PORT ?? 8080);
  makeServer().listen(port, () => {
    console.log(`wllama demo  →  http://localhost:${port}`);
    console.log('(cross-origin isolated: SharedArrayBuffer / multi-thread enabled)');
  });
}
