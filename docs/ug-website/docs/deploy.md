# Deploying the UltraGraph website

The site is static: hand-written HTML at `docs/ug-website/`, plus one
*generated* folder, `demo/`. `firebase.json` sets `"public": "."`, so the
deploy publishes that directory wholesale — there is no build step, and
nothing new has to be registered anywhere to be served.

## Host locally

```bash
cd docs/ug-website
python3 -m http.server        # http://localhost:8000
# or: npx serve               # http://localhost:3000
```

Serve from `docs/ug-website`, **not** from `demo/`. The demo's "Install ug"
links point at the site root (`/#get-started`), and the YouTube embed on the
landing page will not stream from a plain `file://`.

## Deploy

```bash
cd docs/ug-website            # required: "public" is "." relative to this file
firebase login                # once
firebase deploy --only hosting:ultra-graph
```

Deploying publishes to a live public site. Edit and commit freely; deploy
only when asked.

---

# The live demo (`demo/`)

`https://ultra-graph.web.app/demo/` is a real indexed repo a visitor can fly
before installing anything. It is the same visualization page `ug serve`
serves, next to a `graph.json` snapshot and a static stand-in for the server
— no database, no vectors, no backend.

## Regenerate it

```bash
./scripts/gen-demo.sh              # from the repo root
./scripts/gen-demo.sh --preview    # …and then serve the site at :8000
```

The script builds `ug` from the working tree first, on purpose: the
visualization page is **embedded in the binary** (`build.rs` assembles
`native/src/vis/` into it), so publishing with a stale `ug` silently ships
that build's page and an edit under `native/src/vis/` appears to do nothing.

Overrides, if you want a different demo:

```bash
DEMO_INPUT=native/src/indexer DEMO_LABEL="The indexer" ./scripts/gen-demo.sh
UG=/usr/local/bin/ug ./scripts/gen-demo.sh          # skip the build
```

Or call the command directly — `ug demo -h` lists every flag:

```bash
ug demo -i ~/code/myrepo -o /tmp/myrepo-demo \
        --label "MyRepo" --source-url https://github.com/me/myrepo
```

## Deployment is automatic

`demo/` sits inside the published folder, so `firebase deploy` picks it up
with everything else — there is nothing to wire up. Two things keep it that
way:

- **Commit the generated files.** The deploy publishes the working tree, so
  whatever is in `docs/ug-website/demo/` at deploy time is what goes live.
  Committing them means any checkout can deploy the same demo.
- **Don't add `demo/` to `.gitignore` or to `firebase.json`'s `ignore` list.**
  That list drops `**/.*`, `docs/**`, `**/*.md` and `node_modules` — the
  demo's `README.md` is ignored by the markdown rule (it is a note for
  whoever opens the directory, not part of the page); everything else in
  `demo/` ships.

## What is in `demo/`

| File | |
|---|---|
| `graph.json` | the indexed graph — what the page draws (~2.7 MB, gzipped in transit) |
| `index.html` | the visualization, with the demo shim injected |
| `ug-vis.bundle.js` | the renderer (Three.js + 3d-force-graph) |
| `favicon.svg` | so the folder also works served standalone |
| `demo.json` | label, node/edge counts, `ug` version, generation time |
| `README.md` | a "this is generated" note; not deployed |

All of it is rewritten on every run. Do not hand-edit anything in there.

## Things that will bite you

**Refresh the counts on the landing page.** `index.html`'s `#demo` section
hardcodes the node/edge/file/line figures in `.demo-facts`. They come from
the generator's output (and `demo/demo.json`) — update them in the same
commit, or the page advertises a graph that no longer exists.

**Watch for the solo-mode warning.** The renderer draws a graph in full up to
10,000 elements and switches to *solo mode* above that — an empty canvas
asking the visitor to pick a node. Correct for a real repo, a poor first
impression for a demo. `ug demo` prints a warning when the graph it just
built crosses the line; if it does, point `DEMO_INPUT` at a subtree. This is
why the default is `native/src` (~2.9k nodes / 8.7k edges) and not the whole
checkout (~10.3k edges, just over).

**A published graph is public.** It carries file paths, symbol names and
docstrings from whatever was indexed. `ug demo` rewrites local absolute paths
out — the repo root and your home directory — but nothing else. Only point it
at code you are willing to publish.

**What is deliberately missing.** Semantic and hybrid search, chat, guided
tours, statistics/GQL and source preview all read the index `ug gen` builds
locally, and none of it can be published. The demo shim answers those
endpoints with one honest message and the UI names the reason where each
feature would have appeared. Keyword search, filters, focus, walk, outlines,
callers and imports all work — they read `graph.json` in the browser.

**Where the shim lives.** `native/src/vis/demo-shim.js`, injected by
`ug demo`. Nothing under `native/src/vis/js/` knows the demo exists, which is
what stops the demo and the real app from drifting apart. If a startup
endpoint is added to the app, teach the shim to answer it there.
