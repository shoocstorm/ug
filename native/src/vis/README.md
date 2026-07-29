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
favicon.svg
```

`build.rs` concatenates them into `$OUT_DIR/visualization.html`, which
`main.rs` embeds with `include_str!`.

## Editing

Edit the parts. There is **no generated file in this directory** to edit by
mistake — that is deliberate. A generated copy sitting in the source tree
looks like every other file, so a change to it works when you test it and
then vanishes on the next `cargo build`, with nothing to explain why.

Adding a part is just adding a file: order comes from the numeric prefix,
so `04-settings.js` runs after `03-insights.js`. Nothing lists the parts,
which means nothing can drift out of sync with them.

## Two things that will bite you

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

## Checking your work

```bash
cargo test --test vis_assembly_test    # shape of the assembled page
cargo build --release                  # the </script> and placeholder guards
```

Headless Chrome **cannot boot this page** (no WebGL — `initialize()` never
runs and no `<canvas>` appears), so end-to-end browser testing needs a real
display. What does work headlessly is testing a pane in isolation: generate
a harness from this directory by marker, stub the graph integration, and
serve it on the API's origin. See the P2 section of
`docs/dev/PROGRESS-repo-stats-query.md` for how the Insights pane is
covered that way.
