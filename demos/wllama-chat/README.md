# wllama chat demo

Proof that [wllama](https://github.com/ngxson/wllama) — llama.cpp compiled to
WebAssembly — can download a GGUF from Hugging Face, host it **inside the
browser tab**, and serve chat completions with no backend and no API key.

This is the feasibility spike for putting a local LLM behind `ug`'s chat UI.

Pipeline: `HF GGUF → OPFS cache → WASM worker → OpenAI-shaped streaming API`.

## Run it

```bash
cd demos/wllama-chat
npm install           # pulls @wllama/wllama (ESM + the 8 MB wllama.wasm)
npx serve .           # serve.json adds the COOP/COEP headers wllama needs
# → http://localhost:3000
```

`npx serve` is enough; the repo also ships a zero-dependency server with the
same headers if you'd rather not hit the network for a CLI:

```bash
npm start             # node server.mjs, PORT=8080 by default
```

Then: pick **Qwen3 0.6B**, hit **Download & load** (639 MB, once — it is cached
in OPFS, so the second load is instant), and type in the box.

> Use Chrome or Edge. The headers above enable `SharedArrayBuffer`, which is
> what gets you multi-threaded WASM; without cross-origin isolation wllama
> silently falls back to a single thread.

## Verify it without a browser

`scripts/smoke.mjs` serves the page, drives it in headless Chrome, and waits
for the page to POST its result back:

```bash
npm run smoke              # TinyStories 260K — 1.2 MB, finishes in ~10s
node scripts/smoke.mjs qwen  # the real thing: 639 MB download, then chat
```

Measured on this machine (M-series, CPU/WASM backend, 9 threads):

| model | download | load | generation |
|---|---|---|---|
| TinyStories 260K | 1.2 MB | 2.7 s | 634 tok/s |
| Qwen3 0.6B Q8_0 | 639 MB | 8.3 s (cached: no re-download) | 46 tok/s |

Qwen3's reply through `createChatCompletion`, thinking disabled:

> WebAssembly is a binary format for executing high-performance applications on the web.

## What's where

| file | what it does |
|---|---|
| `index.html` | markup + the import map that points `@wllama/wllama` at `node_modules` |
| `app.js` | the whole demo: model catalog, OPFS cache state, load w/ progress, streaming chat |
| `styles.css` | dark UI |
| `server.mjs` | zero-dep static server that sets COOP/COEP |
| `smoke.html` + `scripts/smoke.mjs` | headless end-to-end check |
| `serve.json` | same headers, for `npx serve` |

## Notes for the `ug` integration

- **Cross-origin isolation is a hard requirement for multi-thread.** `ug serve`
  would have to send `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp` on the app shell. Everything
  cross-origin the page then loads must pass CORS — Hugging Face does.
- **The `.wasm` path must be absolute.** wllama runs in a worker created from a
  `blob:` URL, where a relative path has no useful base; hence
  `new URL('./…/wllama.wasm', import.meta.url).href`.
- **Models live in OPFS**, keyed by URL, listed through `ModelManager.getModels()`
  and evictable with `model.remove()` — so `ug` can show/manage them.
- **`reasoning_format: 'none'` at load time** keeps Qwen3's `<think>…</think>`
  in the streamed content so the UI can fold it; the default parser routes it
  to a field the stream chunks don't carry.
- **Thinking is a template kwarg**: `chat_template_kwargs: { enable_thinking: false }`.
  Qwen3 still emits an empty `<think></think>` pair — render around it.
- The API is OpenAI-shaped (`createChatCompletion`, `stream: true`,
  `abortSignal`), so the swap between hosted and in-browser inference is small.
