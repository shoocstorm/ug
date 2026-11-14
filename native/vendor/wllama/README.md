# wllama — vendored runtime

llama.cpp compiled to WebAssembly, so a model can be downloaded and run
*inside the browser tab* with no inference server anywhere. `ug serve` hands
these two files to the page at `/wllama/<version>/…`; nothing here is compiled
into the Rust, it is embedded verbatim by `assets.rs` and served as bytes.

| file | origin |
|---|---|
| `wllama.js` | `@wllama/wllama@3.6.1` → `esm/index.js` (unminified, self-contained: no imports, no source map) |
| `wllama.wasm` | `@wllama/wllama@3.6.1` → `esm/wasm/wllama.wasm` |
| `LICENCE` | the package's own MIT licence |
| `compat/wllama.js` | `@wllama/wllama-compat@3.6.1` → `wasm/wllama.js` (the worker script) |
| `compat/wllama.wasm` | `@wllama/wllama-compat@3.6.1` → `wasm/wllama.wasm` |

The `compat/` pair is the same runtime built without Memory64 and JSPI. It is
the only build Safari can run, so it is what an iPhone loads; other browsers
never fetch it. It adds about 15 MB to the binary. wllama's own fallback is to
fetch these two from jsDelivr, which the next section rules out — and a worker
cannot be started from another origin anyway.

Upstream: <https://github.com/ngxson/wllama> (MIT).

## Why vendored rather than fetched from a CDN

The visualization page has no external URLs at all — everything it needs
ships in the binary. Importing a module from jsDelivr at runtime would put
third-party code into the app's own origin, make the feature fail on a
network that blocks the CDN, and leave nothing to audit. 8.5 MB in the
binary buys pinned, offline-capable, reviewable bytes.

## Upgrading

```bash
npm pack @wllama/wllama@<new-version>
tar xzf wllama-wllama-<new-version>.tgz
cp package/esm/index.js       native/vendor/wllama/wllama.js
cp package/esm/wasm/wllama.wasm native/vendor/wllama/wllama.wasm
cp package/LICENCE            native/vendor/wllama/LICENCE

npm pack @wllama/wllama-compat@<new-version>
tar xzf wllama-wllama-compat-<new-version>.tgz
cp package/wasm/wllama.js    native/vendor/wllama/compat/wllama.js
cp package/wasm/wllama.wasm  native/vendor/wllama/compat/wllama.wasm
```

The two packages must be the same version: the page pairs the default
`wllama.js` with whichever wasm the browser can run.

Then bump `WLLAMA_VERSION` in `native/src/assets.rs` — it is the cache-busting
segment in the asset URL, so a new runtime that keeps the old version string
will be served out of a browser cache that holds the old bytes for a year.
The demo under `demos/wllama-chat/` pins the same version in its
`package.json`; keep the two in step so what the demo proves is what ug runs.
