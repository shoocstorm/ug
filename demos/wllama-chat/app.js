// wllama demo — download a GGUF from Hugging Face, host it in-browser, chat with it.
// Everything below runs client-side: no server-side inference, no API key.

import { Wllama, ModelManager, WllamaAbortError } from '@wllama/wllama';

// The .wasm ships with the npm package; resolve it against this module's URL so
// the worker (which lives on a blob: URL) gets an absolute path.
const WASM_PATHS = {
  default: new URL(
    './node_modules/@wllama/wllama/esm/wasm/wllama.wasm',
    import.meta.url
  ).href,
};

const HF = (repo, file) => `https://huggingface.co/${repo}/resolve/main/${file}`;

const MODELS = {
  'qwen3-0.6b': {
    name: 'Qwen3 0.6B',
    size: '639 MB',
    repo: 'Qwen/Qwen3-0.6B-GGUF',
    file: 'Qwen3-0.6B-Q8_0.gguf',
    note: 'Smaller model optimized for small devices. Accuracy may be worse.',
    thinking: true,
    n_ctx: 4096,
  },
  'stories-260k': {
    name: 'TinyStories 260K',
    size: '1.2 MB',
    repo: 'ggml-org/models',
    file: 'tinyllamas/stories260K.gguf',
    note: 'Smoke test only — downloads in a second, talks nonsense.',
    thinking: false,
    n_ctx: 1024,
  },
};
for (const [id, m] of Object.entries(MODELS)) {
  m.id = id;
  m.url = HF(m.repo, m.file);
}

const $ = (id) => document.getElementById(id);
const el = {
  caps: $('caps'),
  list: $('modelList'),
  loadBtn: $('loadBtn'),
  unloadBtn: $('unloadBtn'),
  evictBtn: $('evictBtn'),
  progressWrap: $('progressWrap'),
  progressFill: $('progressFill'),
  progressLabel: $('progressLabel'),
  progressPct: $('progressPct'),
  info: $('modelInfo'),
  thread: $('thread'),
  empty: $('emptyState'),
  input: $('input'),
  sendBtn: $('sendBtn'),
  stopBtn: $('stopBtn'),
  thinkWrap: $('thinkToggleWrap'),
  think: $('thinkToggle'),
  temp: $('temp'),
  tempOut: $('tempOut'),
  maxTokens: $('maxTokens'),
  maxTokensOut: $('maxTokensOut'),
  stats: $('stats'),
  status: $('status'),
};

const state = {
  selected: 'qwen3-0.6b',
  wllama: null,
  loaded: null, // model def currently in memory
  cached: new Map(), // url -> bytes
  messages: [],
  abort: null,
};

const modelManager = new ModelManager();

const mb = (n) => `${(n / 1024 / 1024).toFixed(1)} MB`;
const setStatus = (s) => (el.status.textContent = s);

// ── capability badges ──────────────────────────────────────
function renderCaps() {
  const caps = [
    ['WebGPU', 'gpu' in navigator],
    ['SharedArrayBuffer', typeof SharedArrayBuffer !== 'undefined'],
    ['OPFS', !!navigator.storage?.getDirectory],
    [`${navigator.hardwareConcurrency || '?'} threads`, true],
  ];
  el.caps.innerHTML = caps
    .map(([label, on]) => `<span class="cap ${on ? 'on' : ''}">${on ? '✓' : '✗'} ${label}</span>`)
    .join('');
}

// ── model picker ───────────────────────────────────────────
function renderModels() {
  el.list.innerHTML = '';
  for (const m of Object.values(MODELS)) {
    const li = document.createElement('li');
    li.className = 'model-card' + (m.id === state.selected ? ' selected' : '');
    const cachedBytes = state.cached.get(m.url);
    li.innerHTML = `
      <div class="row">
        <span class="name">${m.name}</span>
        ${cachedBytes ? `<span class="badge">cached · ${mb(cachedBytes)}</span>` : `<span class="size">${m.size}</span>`}
      </div>
      <div class="note">${m.note}</div>`;
    li.onclick = () => {
      if (state.loaded && state.loaded.id !== m.id) {
        setStatus(`Unload ${state.loaded.name} before switching models.`);
        return;
      }
      state.selected = m.id;
      renderModels();
      syncButtons();
    };
    el.list.appendChild(li);
  }
}

async function refreshCache() {
  state.cached.clear();
  try {
    for (const m of await modelManager.getModels()) {
      if (m.size > 0) state.cached.set(m.url, m.size);
    }
  } catch (e) {
    console.warn('cache listing failed', e);
  }
  renderModels();
  syncButtons();
}

function syncButtons() {
  const m = MODELS[state.selected];
  const busy = !!state.abort || el.loadBtn.dataset.busy === '1';
  const isLoaded = state.loaded?.id === m.id;
  el.loadBtn.hidden = isLoaded;
  el.loadBtn.disabled = busy;
  el.loadBtn.textContent = state.cached.has(m.url) ? 'Load from cache' : 'Download & load';
  el.unloadBtn.hidden = !isLoaded;
  el.evictBtn.hidden = !state.cached.has(m.url) || isLoaded;
  el.sendBtn.disabled = !state.loaded || busy;
  el.input.disabled = !state.loaded;
  el.input.placeholder = state.loaded
    ? `Message ${state.loaded.name}…  (Enter to send, Shift+Enter for newline)`
    : 'Load a model first…';
  el.thinkWrap.hidden = !state.loaded?.thinking;
}

// ── loading ────────────────────────────────────────────────
async function loadModel() {
  const m = MODELS[state.selected];
  el.loadBtn.dataset.busy = '1';
  syncButtons();
  el.progressWrap.hidden = false;
  el.info.hidden = true;
  setProgress(0, 0, 'Connecting…');

  const started = performance.now();
  try {
    const wllama = new Wllama(WASM_PATHS, { parallelDownloads: 3 });
    await wllama.loadModelFromUrl(m.url, {
      n_ctx: m.n_ctx,
      // keep <think> blocks in the raw stream so the UI can fold them itself
      reasoning_format: 'none',
      progressCallback: ({ loaded, total }) => {
        const pct = total ? loaded / total : 0;
        const secs = (performance.now() - started) / 1000;
        const speed = secs > 0 ? loaded / secs : 0;
        setProgress(
          pct,
          pct,
          `${mb(loaded)} / ${mb(total)} · ${mb(speed)}/s`
        );
      },
    });

    setProgress(1, 1, 'Warming up…');
    state.wllama = wllama;
    state.loaded = m;
    renderInfo(wllama);
    el.progressWrap.hidden = true;
    setStatus(`${m.name} loaded in ${((performance.now() - started) / 1000).toFixed(1)}s.`);
    el.empty.remove?.();
    el.input.focus();
  } catch (e) {
    console.error(e);
    el.progressWrap.hidden = true;
    addMessage('error', `Load failed: ${e.message}`);
    setStatus('Load failed.');
  } finally {
    el.loadBtn.dataset.busy = '0';
    await refreshCache();
  }
}

function setProgress(fill, pct, label) {
  el.progressFill.style.width = `${Math.round(fill * 100)}%`;
  el.progressPct.textContent = `${Math.round(pct * 100)}%`;
  el.progressLabel.textContent = label;
}

function renderInfo(wllama) {
  const i = wllama.getLoadedContextInfo();
  const rows = {
    backend: wllama.isSupportWebGPU() ? 'WebGPU' : 'WASM (CPU)',
    threads: wllama.isMultithread?.() ? `${wllama.getNumThreads?.() ?? '?'} (multi)` : 'single',
    context: `${i.n_ctx} / ${i.n_ctx_train}`,
    layers: i.n_layer,
    n_embd: i.n_embd,
    vocab: i.n_vocab,
    arch: i.metadata?.['general.architecture'] ?? '—',
    libllama: Wllama.getLibllamaVersion(),
  };
  el.info.innerHTML = Object.entries(rows)
    .map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`)
    .join('');
  el.info.hidden = false;
}

async function unloadModel() {
  await state.wllama?.exit();
  state.wllama = null;
  state.loaded = null;
  state.messages = [];
  el.info.hidden = true;
  setStatus('Model unloaded.');
  syncButtons();
}

async function evictModel() {
  const m = MODELS[state.selected];
  for (const model of await modelManager.getModels()) {
    if (model.url === m.url) await model.remove();
  }
  setStatus(`${m.name} removed from cache.`);
  await refreshCache();
}

// ── chat ───────────────────────────────────────────────────
function addMessage(role, text) {
  el.empty?.remove?.();
  const div = document.createElement('div');
  div.className = `msg ${role}`;
  div.textContent = text;
  el.thread.appendChild(div);
  el.thread.scrollTop = el.thread.scrollHeight;
  return div;
}

/** Split `<think>…</think>` out of a streamed reply and fold it into a <details>. */
function renderAssistant(node, raw) {
  const open = raw.indexOf('<think>');
  if (open === -1) {
    node.textContent = raw;
  } else {
    const close = raw.indexOf('</think>');
    const thinking = raw.slice(open + 7, close === -1 ? raw.length : close).trim();
    const answer = close === -1 ? '' : raw.slice(close + 8);
    node.innerHTML = '';
    // With `enable_thinking: false` Qwen3 still emits an empty <think></think>.
    if (thinking) {
      const d = document.createElement('details');
      d.className = 'think';
      d.open = close === -1; // keep it open while the model is still thinking
      d.innerHTML = `<summary>${close === -1 ? 'thinking…' : 'thought process'}</summary>`;
      const body = document.createElement('div');
      body.className = 'body';
      body.textContent = thinking;
      d.appendChild(body);
      node.appendChild(d);
    }
    node.appendChild(document.createTextNode(answer.replace(/^\n+/, '')));
  }
  const atBottom =
    el.thread.scrollHeight - el.thread.scrollTop - el.thread.clientHeight < 80;
  if (atBottom) el.thread.scrollTop = el.thread.scrollHeight;
}

async function send() {
  const text = el.input.value.trim();
  if (!text || !state.wllama || state.abort) return;

  el.input.value = '';
  addMessage('user', text);
  state.messages.push({ role: 'user', content: text });

  const node = addMessage('assistant', '');
  node.innerHTML = '<span class="caret">▍</span>';

  state.abort = new AbortController();
  el.stopBtn.hidden = false;
  syncButtons();

  const m = state.loaded;
  const started = performance.now();
  let reply = '';
  let tokens = 0;

  try {
    const stream = await state.wllama.createChatCompletion({
      messages: state.messages,
      stream: true,
      abortSignal: state.abort.signal,
      max_tokens: Number(el.maxTokens.value),
      temp: Number(el.temp.value),
      top_p: 0.95,
      top_k: 40,
      ...(m.thinking
        ? { chat_template_kwargs: { enable_thinking: el.think.checked } }
        : {}),
    });

    for await (const chunk of stream) {
      const delta = chunk.choices?.[0]?.delta?.content;
      if (!delta) continue;
      reply += delta;
      tokens++;
      renderAssistant(node, reply);
      const secs = (performance.now() - started) / 1000;
      el.stats.textContent = `${tokens} tok · ${(tokens / secs).toFixed(1)} tok/s`;
    }

    state.messages.push({ role: 'assistant', content: reply });
    setStatus(
      `Generated ${tokens} tokens in ${((performance.now() - started) / 1000).toFixed(1)}s.`
    );
  } catch (e) {
    if (e instanceof WllamaAbortError || e?.name === 'AbortError') {
      state.messages.push({ role: 'assistant', content: reply });
      setStatus('Stopped.');
    } else {
      console.error(e);
      addMessage('error', `${e.name}: ${e.message}`);
      setStatus('Generation failed.');
    }
  } finally {
    if (!reply) node.remove();
    state.abort = null;
    el.stopBtn.hidden = true;
    syncButtons();
    el.input.focus();
  }
}

// ── wiring ─────────────────────────────────────────────────
el.loadBtn.onclick = loadModel;
el.unloadBtn.onclick = unloadModel;
el.evictBtn.onclick = evictModel;
el.sendBtn.onclick = send;
el.stopBtn.onclick = () => state.abort?.abort();
el.input.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    send();
  }
});
el.temp.oninput = () => (el.tempOut.value = Number(el.temp.value).toFixed(2));
el.maxTokens.oninput = () => (el.maxTokensOut.value = el.maxTokens.value);

renderCaps();
renderModels();
syncButtons();
refreshCache();
setStatus(`wllama ready · libllama ${Wllama.getLibllamaVersion()}`);
