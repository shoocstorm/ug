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
```

Then bump `WLLAMA_VERSION` in `native/src/assets.rs` — it is the cache-busting
segment in the asset URL, so a new runtime that keeps the old version string
will be served out of a browser cache that holds the old bytes for a year.
The demo under `demos/wllama-chat/` pins the same version in its
`package.json`; keep the two in step so what the demo proves is what ug runs.
