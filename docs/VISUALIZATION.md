# The visualization

The graph UI that `ug serve` opens, `ug gen` writes and `ug demo` publishes: one
self-contained page that draws an indexed repo as a graph you can fly, filter,
search, narrate and walk.

This document covers what it is, how it is put together, and — the largest piece
of it — the **two renderers** it can draw with and why there are two.

- **Source**: `native/src/vis/` (see its `README.md` for the editing rules)
- **Served by**: `native/src/serve.rs`
- **Published by**: `native/src/cli/demo.rs`

---

## 1. One page, two deployments

`ug gen` writes the page into a user's output directory and it is opened
straight from disk, so it cannot reference sibling assets — with one deliberate
exception, the renderer bundles (§3.3). Everything else is inlined.

That page is **generated**. Its source is the parts in `native/src/vis/`:

```
index.html          the shell: <head>, markup, {{CSS}} and {{JS}} placeholders
css/NN-name.css     17 stylesheet parts, concatenated in filename order
js/NN-name.js       21 script parts, concatenated in filename order
threejs-vis.bundle.js  the 3D renderer  (three.js + 3d-force-graph, vendored)
cosmos-vis.bundle.js   the 2D renderer  (cosmos.gl, vendored)
demo-shim.js        the static-hosting wrapper for the public demo
favicon.svg
```

`build.rs` concatenates them into `$OUT_DIR/visualization.html`, which
`assets.rs` embeds with `include_str!`. Two rules make that safe:

- **Order comes from the filename**, not from a manifest — a numeric prefix, so
  the order is visible in `ls` and nothing can drift out of sync with it. The
  prefixes must be unique and ascending (`vis_assembly_test`).
- **A literal `</script>` or `</style>` in a part fails the build.** It would
  close the block early and truncate the page with no error in the browser.

**The JS is one module, not many.** Every `js/*.js` part lands inside a single
`<script type="module">`, so they share one scope: a function in
`12-render-cosmos.js` calls one declared in `10-render-core.js` with no import.
Function declarations hoist and are fine anywhere; `const`/`let` are not, so
declarations go early and behaviour late.

### The two deployments

| | Served by | Gets your edit… |
|---|---|---|
| The app | `ug serve` / `ug gen`, from the binary | on the next `cargo build` |
| [The public demo](https://ultra-graph.web.app/demo/) | Firebase, from `docs/ug-website/demo/` | **only when re-published** |

The demo is a *copy* wrapped in `demo-shim.js`, and a copy cannot notice its
original changed — so `cli::demo::vis_fingerprint()` hashes the assembled page,
the shim **and both renderer bundles**, stamps it into `demo.json`, and
`the_published_demo_page_is_not_stale` fails until you run:

```bash
cargo run --bin ug -- demo --page-only
```

---

## 2. Routes

| Route | Serves |
|---|---|
| `/`, `/index.html` | the assembled page (`assets::VIS_HTML`) |
| `/threejs-vis.bundle.js` | 3D renderer bundle (`VIS_THREEJS_BUNDLE`) — loaded on demand |
| `/cosmos-vis.bundle.js` | 2D renderer bundle (`VIS_COSMOS_BUNDLE`) — loaded on demand |
| `/favicon.svg` | tab icon |
| `/graph.json` | the active project's graph |
| `/indexed-tree.json` | per-file parse snapshot |

Everything else the page uses is under `/api/` — and anything it fetches at
startup must also be answerable by `demo-shim.js`, or the public demo blocks on
a request a static host will never serve.

---

## 3. Two renderers

### 3.1 Why

The original renderer is **three.js + 3d-force-graph**: a 3D force graph where
every node is a `THREE.Group` of four or five objects (halo sprite, glyph
"sticker", membrane shell, boundary ring, text label) and the layout runs on CPU
`d3-force-3d`. It is expressive and it does not scale. A few thousand nodes is
~15k scene objects, and the restyle path walks every node on every hover.

**cosmos.gl** runs both the force simulation and the drawing in WebGL2 shaders —
one instanced draw call for all points — so it holds graphs the 3D renderer
cannot. What it costs is the third dimension: no z-axis, no orbit camera, and no
scene graph to add meshes to.

Neither strictly dominates, so both ship.

### 3.2 The seam

`10-render-core.js` owns everything that is **not** renderer-specific and
dispatches the rest. A backend is a plain object:

```js
{
  name, caps: { threeD, faceViews, autoSpin, boundaryCube },
  soloThreshold,
  mount(el, view), setData(view), restyle(), resize(w, h), dispose(),
  frameAll(ms), frameNodes(ids, ms), focusNode(n), setView(id, ms),
  flyToStop(stop, opts), frameRoute(ms), zoomBy(f),
  setAutoSpin(on), setBoundaryVisible(on), setLayout(name, ms),   // optional
  emitPulse(node, colour, fromR, toR, growMs), screenPos(node),
}
```

The rest of the page keeps calling `frameGraph()`, `setView()`, `focusNode()`,
`bumpGraphStyles()` and friends — those are now one-line dispatchers. Camera
moves that arrive before a backend has mounted are queued, not dropped, because
URL-state restore routinely beats the mount.

**`caps` is what keeps the pair honest.** A control a backend cannot honour is
*hidden*, not left dead: in 2D the six face projections, the 3D/ISO button, the
orientation gizmo and Spin all disappear, and the layout switcher (§5.5) takes
their place. A dead control reads as a bug in the graph rather than a property
of the renderer.

> **Trap worth knowing.** `applyRendererCaps()` hides things by setting the
> `hidden` property, and the UA's `[hidden] { display: none }` is the weakest
> rule there is. `#viewbar .vbtn { display: flex }` and `#axis-gizmo
> { display: flex }` both outrank it, so hiding silently did nothing until
> `css/16-overlay.css` added `[hidden]` rules at matching specificity. Anything
> else hidden this way needs the same check.

### 3.3 Loading

Both bundles are vendored and **imported lazily**, inside each backend's
`mount()`. A session downloads only the renderer it uses — the 3D bundle is
1.4 MB, the 2D one 681 KB.

The cosmos.gl bundle is built from npm rather than checked in from a CDN:

```bash
npm i @cosmos.gl/graph@3.4.1
# entry.js: export { Graph, PointShape, LinkStyle, TransitionEasing } from '@cosmos.gl/graph'
esbuild entry.js --bundle --format=esm --minify --outfile=native/src/vis/cosmos-vis.bundle.js
```

ESM with named exports, matching the 3D bundle's shape. Pin the version; re-vendoring
changes `vis_fingerprint()` and therefore requires a demo re-publish.

### 3.4 Which one runs

In order of authority: an explicit choice this session → `?r=` on the URL →
`localStorage` → **graph size**. Under `THREE_D_MAX_ELEMENTS` (3000
`max(nodes, edges)`) the default is 3D, where the extra dimension and in-scene
effects are still readable; above it, 2D.

The viewbar's `◫ 2D/3D` button switches at runtime: the old backend is disposed,
the mount element emptied, the new one mounted, and the current styling
re-applied.

> **Trap worth knowing.** `Graph._destructor()` releases the WebGL context but
> leaves its `<canvas>` in the DOM. Without `el.replaceChildren()` before mount,
> the new renderer comes up *underneath* a dead canvas still showing its last
> painted frame — which looks exactly like a freeze. The 3D backend also
> generation-stamps its rAF loops (`threeGen`) so they exit on dispose instead
> of running forever and fighting a later mount.

### 3.5 Solo mode is per renderer

Above a renderer's `soloThreshold` the whole graph is never drawn. `state.graph`
stays complete — search, filters, stats and path-finding all still see every
node — and the renderer is handed `state.view`, one neighbourhood at a time.

| Renderer | Threshold |
|---|---|
| 3D | `THREE_D_MAX_ELEMENTS` = 3 000 |
| 2D | `SOLO_THRESHOLD` = 1 000 000 |

`applySoloMode(backend.soloThreshold)` runs in `createGraph()` *before* mount, so
switching renderers re-decides: a 50k-node graph that renders whole in 2D drops
into solo when you switch to 3D, and lifts back out when you switch back.
Entering solo carries the current selection in as the first seed, so the switch
lands on the node you were looking at rather than a blank canvas.

Budgets: `SOLO_MAX_NODES` 1500 per view, `SOLO_MAX_NEIGHBORS` 300 per seed.
`demo.rs` mirrors `SOLO_THRESHOLD`, and `the_solo_threshold_matches_the_renderer`
fails if the two drift.

---

## 4. The shared style contract

A second renderer is only affordable if *"what should this node look like?"* has
one answer. `10-render-core.js` holds it; a backend decides how to **draw** that
answer, never what it is.

| Function | Answers |
|---|---|
| `nodeColorFor(n)` | selection orange → hover orange → walk hop colour → tour amber → type colour |
| `linkColorFor(e)` | hover direction → walk frontier → tour route → focus recede → relation colour |
| `nodeVisibleFor(n)` / `linkVisibleFor(e)` | walk / tour-isolate / focus-isolate / filters |
| `nodeLightingFor(n)` | `{ dim, opacity, tier }` — the one definition of "lowlit" |
| `linkParticlesFor(e)` | how many flow particles, and the single `state.lineFlow` switch |
| `nodeRadiusFor(n)` | type radius × 1.6 |

### Node types

`NODE_TYPE_ORDER` (`00-preamble.js`) is the canonical order — containers first,
then the symbols they contain, ending at functions:

```
Folder · File · Route · Config · Dependency · Class · Interface · Concept · Constant · Variable · Function
```

It drives the legend and every spatial layout. Before it existed, each of those
sorted by population, which put the largest type on the Rings layout's innermost
and shortest ring — the crowding was exactly inverted.

**Colour** is a two-family "ink" palette: warm oranges for the structural spine
(folders, files, deps), steel blues for code symbols. **Functions are the
deliberate exception**, in green outside both families — a palette exists to
group things, and the one type you most often need to pick out of a crowd should
not be grouped with anything.

**Shape** (2D only) is a second, redundant channel — it survives everything
colour does not, since a node dimmed by a filter, recoloured by a walk hop or
flared orange on selection keeps its silhouette:

| square | diamond | hexagon | circle |
|---|---|---|---|
| Folder, File | Class, Interface | Function | everything else |

Grouped by family rather than one shape per type: seven silhouettes is a code to
memorise, four is a glance.

---

## 5. The 2D renderer (`12-render-cosmos.js`)

Everything here is **index-addressed**: cosmos.gl has no node ids, only flat
`Float32Array`s, so this file owns the `id ↔ index` map and rebuilds it whenever
the view changes.

### 5.1 The sticker, split in two

cosmos.gl does **not** tint point images — the fragment shader blends them
*above* the shape colour (`mix(shapeColor, imageColor, imageColor.a)`). So a
node is drawn as two things:

- the **disc** is the point's own shape, coloured by `setPointColors` — dynamic,
  so it can flare orange on selection or take a walk hop's tint;
- the **image** carries only the dark glyph (from the shared `NODE_ICONS` paths)
  and the boundary ring, both of which are the same whatever colour the disc is.

Baking the disc into the image would freeze it per node type and lose every
dynamic colour the app depends on. One image per (type × boundary direction)
actually present in the graph; `cosmosGlyphFit()` trims the glyph for shapes
whose inscribed circle is small (a diamond's is 0.71 of the half-width).

### 5.2 Dim vs hide

Two different things, kept separate:

- **Dim** is colour alpha, straight from `nodeLightingFor`.
- **Hide** is a `NaN` position — cosmos.gl treats such a point as *absent*:
  excluded from the forces, from hover and from zoom targets, index preserved.
  That is exactly the isolate semantics a walk or a tour wants.

> **Trap worth knowing.** Dimming must happen in *one* place. Using
> `highlightedPointIndices` for selection greys out everything not named in it,
> which multiplied a selected node's neighbours down to ~0.11 opacity — the
> exact nodes you selected the node to look at. That config is no longer used;
> only the rings (`focusedPointIndex`, `outlinedPointIndices`) are, since rings
> add emphasis without taking any away.

### 5.3 Positions are the GPU's

The simulation and dragging both move points on the GPU, and neither writes back
to JS. But the rest of the page treats `n.x` / `n.y` as truth — extent maths,
framing, the tooltip, the walk all read them. So:

- `cosmosSync()` reads positions back on a throttle during ticks, on simulation
  end, and on `onDragEnd`.
- `cosmosApplyVisibility()` **re-reads the GPU before uploading**. Its buffer is
  only what was last uploaded, so writing it back to change two nodes' visibility
  used to reset every other node to where it had been — which is what "the node
  moves when I select it" was.
- The FX overlay uses `trackPointPositionsByIndices` for the selected node and
  its neighbours — the cheap subset readback — so the selection ring stays glued
  to its node through a drag or a running simulation.

### 5.4 Simulation

`simulationDecay` is **a tick count, and lower is faster**. cosmos.gl's own docs
say the opposite, but alpha decays by `1 - ALPHA_MIN^(1/decay)` per tick, so
after `decay` ticks it has reached the floor — the default 5000 is ~80 seconds at
60 fps. `ug` uses **300** (~5 s at `start(0.7)`), deliberately close to the 3D
renderer's `cooldownTicks(100)`, which exists for the same reason.

### 5.5 Layouts

With no camera to move, what is worth switching is the **arrangement**. Seven,
in the viewbar where the face projections sit in 3D:

| | | |
|---|---|---|
| **FD** | FOLDER | one named island per directory — the shape of the tree |
| **GX** | GAL | three spiral arms around a dense core |
| **SP** | SUN | phyllotaxis disc — equal room per node |
| **GD** | GRID | lattice ordered by type then name; position becomes a lookup |
| **RG** | RING | one concentric ring per node type, canonical order inside out |
| **CL** | CLUS | each type its own island, area ∝ population |
| **FS** | FORCE | hand the arrangement back to the simulation |

Six are **computed position buffers** — they arrive in one morph and then hold
perfectly still, with the simulation switched off (it would pull them apart
within a second). Only FORCE runs it.

`FOLDER` deserves a note: cosmos.gl offers a cluster *force* for this
(`setPointClusters` + `simulationCluster`), which is how its own example does it,
but a force has to converge and converging in public is the slow arrival this
design avoids. Computed directly, the islands are simply there. Folders come from
each node's `file` path, capped at `MAX_FOLDER_CLUSTERS` (28); nodes with no file
go to an outer ring rather than being forced into a folder they are not in.
Island centres sit on a phyllotaxis spiral so the biggest folder lands dead
centre, and the FX overlay labels each one.

### 5.6 The opening

Positions are just a buffer, and `render(alpha, duration)` tweens the whole
buffer on the GPU — so the graph *arrives* rather than being computed in public:

1. the **galaxy**, struck instantly and held ~620 ms
2. morph to the **sunflower** disc (~1 s) — the arms unwind into an even field
3. morph to the **folder islands** — the view worth landing on

The simulation never runs during any of it, and does not take over at the end;
handing off to the force layout would undo stage 3 in public. `prefers-reduced-motion`
skips straight to the layout.

> **Trap worth knowing.** A restyle arriving mid-morph re-renders at zero
> duration and cancels the transition, so restyles are deferred until
> `_cosmosMorphUntil` passes and then flushed. And with the simulation off,
> cosmos.gl auto-rescales incoming positions unless `rescalePositions: false` —
> which would quietly rewrite every layout's coordinates.

### 5.7 The FX overlay (`13-render-overlay.js`)

One 2D canvas over the WebGL one, `pointer-events: none`, carrying what
cosmos.gl cannot draw: **labels** (it renders no text at all), **halos** (a point
image cannot exceed its own quad), **link-flow particles** (the line shader has
no dash *offset* uniform, so a flowing strand cannot be faked), the **selection
ring**, the **walk ignition burst**, the **boundary frame** and the **folder
cluster labels**.

The rule that keeps it affordable: **never iterate every node per frame.**
Labels come from cosmos.gl's own density sampling (`getSampledPoints()`);
everything else is bounded by the hot set — selected, hovered, on the tour route,
or in the walk frontier.

Walked edges get a second pass here as glowing strands — a wide soft glow plus a
bright thin core, in the hop colour of the end being reached. A 1 px WebGL line
is still a thin dark thread on a dark ground, and during a walk those edges are
the *route*, not the scenery.

Dark links get help too: `cosmosLift()` raises only genuinely dark colours
(luminance < 0.18) toward the background's hue. The walk's `linkFar` / `linkRecede`
tones were chosen for a fogged 3D scene where depth separates an edge from the
backdrop; flat, they sit below the noise floor.

---

## 6. The 3D renderer (`11-render-three.js`)

The original, unchanged in behaviour and now behind the same seam. It owns
everything three-dimensional:

- **Sticker nodes** — a camera-facing glyph disc, a tinted radial halo, a
  translucent membrane shell on larger nodes, a dashed boundary ring, and a
  `SpriteText` label.
- **Orbit camera** with six face projections plus isometric, and a percentile-based
  framing that ignores far-flung outliers.
- **Fog as depth-of-field**, scaled to the graph's radius.
- **A dashed boundary cube** with seven-segment face labels and an XYZ triad
  (WebGL ignores line width, so each edge is a row of cylinders).
- **An ambient particle field** clustered around settled node positions.
- **Cinematic tour camera** — broadside to the direction of travel, aimed a touch
  toward the next stop.
- **A GLSL walk pulse** — fresnel shell, wireframe cage, plasma core, debris burst.

---

## 7. Modes layered on the canvas

| Mode | What it does | Owned by |
|---|---|---|
| **Focus** | anchor a node, dim or isolate everything outside its neighbourhood | `08-sidebar-nav.js` |
| **Solo** | above the renderer's threshold, draw one neighbourhood at a time | `16-solo-view.js` |
| **Guided tour** | LLM-planned route, narrated, camera flying stop to stop | `07-tour.js` + `/api/tour` |
| **Graph Walk** | animated BFS frontier, two-phase beat per hop, step-able | `18-walk.js` |

All four gate on shared state that the style rules read, so they compose rather
than conflict, and exiting returns the canvas to whatever was there before.

---

## 8. Chrome

- **Legend** — decodes colours and doubles as a type filter, ordered by
  `NODE_TYPE_ORDER`. Counts track **what is on screen**: the reached set during a
  walk, the route during a tour, the view in solo mode. A `boundary` row counts
  the `isBoundary` flag as a separate axis. Bulk ✓ / ⊘ / ◐ buttons select all,
  none and invert.
- **History bar** — breadcrumb trail of visited nodes at the top of the screen,
  draggable, with the selected node's crumb filled in the canvas's own selection
  orange. It replaces the title block while shown. A ✕ clears the trail, and an
  empty trail hides the bar.
- **Viewbar** — projections or layouts (per `caps`), zoom, Box, Spin, Solo,
  Names, renderer toggle, Reset.
- **Names** — node labels, off by default; one control in the viewbar and one on
  the walk card, both rendering from `state.showLabels`.
- **URL state** (`19-url-state.js`) — `?p=&n=&focus=&nf=&ef=&tab=&q=&r=`, applied
  on load, so a view is shareable and survives a reload. Node selection pushes a
  real history entry, which makes the browser's Back button and the in-app one
  the same button.
- **Command palette** (`20-palette.js`) — `⌘K` dispatches to nodes, insight
  presets, actions and recent tours; `?` opens a shortcut sheet generated from
  the same `KEYMAP` so the two cannot drift.
- **Loading** — a streaming progress bar driven by `Content-Length`, an honest
  failure card with Retry and Back-to-KBs, and a "Rendering graph…" phase held
  until the renderer's first painted frame. A backend that throws while mounting
  surfaces the error with a button to try the other renderer.

---

## 9. Verifying a change

```bash
cd native
cargo nextest run -E 'binary(vis_assembly_test) or test(demo)'   # part order, page shape, demo staleness
cargo build --release                                             # </script> + placeholder guards
cargo run --bin ug -- demo --page-only                            # required after any vis/ edit
./scripts/gen-demo.sh --preview                                   # serves docs/ug-website on :8081
```

`vis_assembly_test` checks that prefixes are unique and ascending, that no part
contains a literal closing tag, that the skeleton has exactly one of each
placeholder, and that **every part actually reached the output** — a file that
stops being picked up removes a feature silently.

A useful pre-flight that needs no cargo: assemble the parts the way `build.rs`
does and run `node --check` over the extracted module. It catches syntax errors
and duplicate top-level declarations, which the one-flat-scope design makes easy
to introduce.

**Browser testing.** The 3D page cannot boot headless — no WebGL means
`initialize()` never runs and no `<canvas>` appears — so it needs a real display.
cosmos.gl is WebGL2 and *does* boot under
`--use-gl=angle --use-angle=swiftshader`, so the 2D path is the one that can get
a real end-to-end canvas smoke test.

---

## 10. Backlog

1. **Responsive / small-screen layout** — the only `@media` queries are
   `prefers-reduced-motion`. With a fixed `--sidebar-width` (min 300 px) plus
   `--info-width` (min 320 px) both overlaying the canvas, 1280 px is already
   tight and a tablet is unusable. At minimum, collapse the sidebar to an overlay
   below ~1100 px.
2. **Accessibility pass** — apply the KB-manager card `role`/`tabIndex`/`keydown`
   pattern (`01-kb-manager.js`) to search results, catalog rows and insight
   results; add focus management to the palette.
3. **Empty / zero-result states** — copy for search, insights and catalog filters
   that match nothing.
4. **Light theme** — the palette is hard-committed to dark, but colours are
   centralised in `config.colorMap` / `config.relColorMap` / `CANVAS`, so a
   CSS-variable pass is cheaper here than in most codebases.
5. **Does 3D still earn its keep?** Every new visual feature now costs two
   implementations. Worth revisiting once the 2D renderer has been used in anger.

Per AGENTS.md §3a there are no users yet — a superseded idea belongs deleted,
not kept as an alias.
