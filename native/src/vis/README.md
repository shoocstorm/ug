# The visualization page

`ug serve` and `ug gen` ship **one self-contained HTML file** — it gets
written into a user's output directory and opened straight from disk, so it
cannot reference sibling assets. That file is *generated*. Its source is
the parts in this directory.

```
index.html          the shell: <head>, markup, {{CSS}} and {{JS}} placeholders
css/NN-name.css     stylesheet parts, concatenated in filename order
js/NN-name.js       script parts, concatenated in filename order
ug-vis.bundle.js    Three.js + 3d-force-graph, vendored (do not hand-edit)
demo-shim.js        the static-hosting wrapper for the public demo — see below
favicon.svg
```

`build.rs` concatenates them into `$OUT_DIR/visualization.html`, which
`main.rs` embeds with `include_str!`. Note what `build.rs` globs: `css/` and
`js/` only. `demo-shim.js` sits at this level precisely so it is *not*
concatenated into every build of the page.

## This page has two deployments

Editing anything here changes both:

| | Served by | Gets your edit… |
|---|---|---|
| The app | `ug serve` / `ug gen`, from the binary | on the next `cargo build` |
| [The public demo](https://ultra-graph.web.app/demo/) | Firebase, from `docs/ug-website/demo/` | **only when you re-publish it** |

The demo is a *copy* of this page, wrapped in `demo-shim.js` and pointed at a
static `graph.json`. A copy has no way to notice its original changed — so
your edit lands in the app and the public demo keeps serving the old page,
with nothing anywhere to say so. That is why a test watches for it
(`the_published_demo_page_is_not_stale`), and why the fix is one cheap
command:

```bash
cargo run --bin ug -- demo --page-only   # rewrites the page, leaves graph.json alone
```

## Editing

Edit the parts. There is **no generated file in this directory** to edit by
mistake — that is deliberate. A generated copy sitting in the source tree
looks like every other file, so a change to it works when you test it and
then vanishes on the next `cargo build`, with nothing to explain why.

Adding a part is just adding a file: order comes from the numeric prefix,
so `04-settings.js` runs after `03-insights.js`. Nothing lists the parts,
which means nothing can drift out of sync with them.

## Three things that will bite you

**The JS is one module, not many.** Every `js/*.js` part is concatenated
into a single `<script type="module">`, so they share one scope — a
function in `10-graph-render.js` can call one declared in `00-preamble.js`
with no import. That is why the split was safe to do at all: it changes no
semantics. But it also means **order matters for anything evaluated at load
time**. Function declarations hoist and are fine anywhere; `const` and `let`
are not, and a part that runs code at module level can only see `const`s
declared in an earlier part. When in doubt, put declarations early and
behaviour late.

**A literal `</script>` or `</style>` in a part will truncate the page.**
The browser reports nothing — the rest of the file is reinterpreted as
markup and the app half-loads. `build.rs` fails the build if it finds one;
split the literal (`'<\/scr' + 'ipt>'`) if you genuinely need it. This is
not hypothetical: the JS already contains a `<script type="module">` string
that only works because it has no closing tag.

**A new startup request breaks the public demo, not the app.** Everything a
`js/` part fetches goes through `demo-shim.js`, which answers `/healthz`,
`/api/projects` and `/api/capabilities` locally and refuses every other
`/api/*` with one honest message. That works because it is a *prefix* rule —
add a fetch to some path outside `/api/`, or make the page block on a new
endpoint at startup, and the demo will reach a static host that has no such
route. Nothing under `js/` should ever branch on "am I the demo": keeping the
demo's whole behaviour in the shim is what stops the two from drifting. Teach
the shim instead.

## Checking your work

```bash
cargo nextest run -E 'binary(vis_assembly_test) or test(demo)'
cargo build --release                  # the </script> and placeholder guards
```

That covers the shape of the assembled page, the `</script>` and placeholder
guards, and whether the published demo still matches this directory.

Headless Chrome **cannot boot this page** (no WebGL — `initialize()` never
runs and no `<canvas>` appears), so end-to-end browser testing needs a real
display. What does work headlessly is testing a pane in isolation: generate
a harness from this directory by marker, stub the graph integration, and
serve it on the API's origin. See the `analyze` feature doc
(`docs/ANALYZE.md`) — its Status section notes how the Insights pane is
covered that way.
