# Serve large graphs from the server instead of downloading them

## Context

`ug serve` hands the browser the entire `graph.json` and the page answers every
question from that local copy. On `~/.ug/neo4j` — 161,725 nodes / 745,964 edges,
**346 MB** on disk — that is a multi-hundred-megabyte JS heap, and it is why graph
walk, tour and plain node browsing all feel slow at once: GC pressure, not any one
slow function.

Earlier in this session three client-side leaks were fixed (a permanently retained
second copy of the parse, duplicated edge-endpoint strings, and a triple-buffered
download). That took the retained graph model from **591 MB → 295 MB**. It is not
enough — 295 MB is still the floor, because the page is *designed* to hold the whole
graph.

Measured target for the same repo: **66 MB**.

| | download | retained graph model |
|---|---|---|
| before this session | 346 MB raw / 15.8 MB gz | 591 MB |
| today | 346 MB raw / 15.8 MB gz | 295 MB |
| **after this plan** | **33.7 MB raw / 2.75 MB gz** | **~66 MB** |

Every figure above is measured, not estimated — the heap numbers by building the
actual client structures in V8 under `--expose-gc`.

Only one real project crosses the threshold, which is what keeps the risk bounded:

| project | graph.json | mode |
|---|---|---|
| neo4j | 346.3 MB | **server** |
| overgraph | 17.1 MB | local (unchanged) |
| hermes | 9.7 MB | local (unchanged) |
| ug | 4.3 MB | local (unchanged) |
| MusicBot | 0.9 MB | local (unchanged) |
| published demo | 2.9 MB | local (unchanged) |

**Scope decisions (already settled):** ship a slim node index rather than going
fully server-backed; port graph walk *and* tour in this project rather than gating
them; default the mode by byte size with a CLI flag and a URL override.

## Verified constraints

Checked against the source, not assumed.

| Fact | Where | Consequence |
|---|---|---|
| Edges are **68% of the payload** — 228 MB of 346 MB, almost all of it repeated endpoint id strings | measured over `~/.ug/neo4j/graph.json` | Dropping edges from the wire is the whole win. Nodes alone are 118 MB; the slim projection of them is 31 MB. |
| `state.adj` is built eagerly over every edge and read **synchronously** by everything downstream | `16-solo-view.js:40,56,70,88,121` | The seam is `edgesOf`. Make the Map a lazily-filled cache and the synchronous consumers never change. |
| Solo mode already means "the renderer only ever sees a neighbourhood" | `16-solo-view.js:1-23`, `SOLO_MAX_NODES=1500` | The view model needed here already exists and is proven. This plan changes *where the neighbourhood comes from*, not what a view is. |
| `applySoloMode`'s else-branch sets `state.view = state.graph` | `16-solo-view.js:194-195` | Impossible in server mode (no edges). Solo must be forced on and this branch made unreachable. |
| `demo-shim.js` already answers `/api/capabilities`, and 501s every other `/api/*` | `demo-shim.js:69-99,119-122` | Publishing the mode *in capabilities* means **the shim needs no change** and the demo keeps today's behaviour. Any other probe would need shim work (AGENTS.md:213). |
| `bootstrap()` calls `loadGraph()` from four places, `openProject()` from two more | `01-kb-manager.js:62,68,103,110,369,386` | The mode branch must live **inside** `loadGraph`, not at its call sites. |
| `AdjIndex` is **forward-only** (`out: Vec<Vec<usize>>`), lazily built per snapshot | `serve.rs:176-196` | `traverse`/`path` are outbound-only; solo's `neighborsOf` is undirected. Needs an inbound side. |
| `api_node` does a linear `nodes.iter().find()` per request although `adj.id_to_idx` exists | `serve.rs:2727` | O(V) per node fetch. A batch endpoint is required, not optional. |
| `api_stats` rescans all nodes **and** all edges on every call | `serve.rs:2682` | Fine once at boot; must not become a per-interaction call. |
| Handlers must iterate `snap.parsed` directly — routing through the lib's `String → String` helpers re-parsed the whole JSON per request | `docs/WEB-SERVE.md:104-110` | Recorded lesson. New handlers follow the same rule. |
| `/api/graph/*` has **no `?project=` scoping** and always reads the active project | `serve/router_tests.rs:364` | A project switch in flight is a correctness hazard once the browser depends on these routes. |
| `/api/graph/*` and `/api/capabilities` never call `refresh_snapshot_if_stale` | `serve.rs:1367-1414` | Without `--watch`, a regenerated graph is invisible to them. |
| Catalog cache is keyed on **object identity** `state.catalogTreeForGraph === state.graph`; `state.containsMaps` invalidated by reassignment | `15-tools-catalog.js:232,289`, `02-dialogs.js:333` | Mutating `state.graph` in place instead of reassigning silently serves a stale tree. |
| `the_solo_threshold_matches_the_renderer` parses `const SOLO_THRESHOLD` out of the JS as a **plain integer literal** by string splitting | `cli/demo.rs:725-743` | Do not rename or reformat that declaration. A *new* constant beside it is fine. |
| `the_published_demo_page_is_not_stale` fails on any `native/src/vis/` edit until the demo is re-published | `cli/demo.rs:683` | `cargo run --bin ug -- demo --page-only -o ../docs/ug-website/demo` is part of the commit, not follow-up. |
| Vis parts need unique, strictly ascending numeric prefixes; 00–20 are taken; every part's first >20-char line must reach the assembled page | `native/tests/vis_assembly_test.rs:57,104` | A new part must be `21-` or higher. |
| `SymbolRef` is already the light node projection the server returns everywhere else | `agent_tools.rs:492-517` | Reuse it for hydration/search responses rather than inventing a shape. |
| Snapshot LRU budget is 512 MiB and `approx_bytes()` estimates parsed at 3× identity | `serve.rs:262-273,316-323` | A 346 MB graph already scores ~1.4 GB. Adding a cached slim index and an inbound adjacency raises the server's own footprint — must be counted. |

## Design

### 1. The mode, and who decides it

`/api/capabilities` (`serve.rs:3066`) gains one block:

```json
"graph": { "bytes": 346266017, "nodes": 161725, "edges": 745964,
           "mode": "server", "token": "<mtime-or-hash>" }
```

`mode` is `server` when `bytes >= GRAPH_SERVER_MODE_BYTES` (50 MB), overridable by
`ug serve --graph-mode=local|server|auto` (parsed beside `--watch` at `serve.rs:868`).
**A missing `graph` block means local** — which is exactly what the static demo
returns, so `demo-shim.js` needs no change and the published demo is untouched.

Client: one memoized `graphCapabilities()` helper. `loadGraph` (`00-preamble.js:259`)
awaits it and branches; `probeCapabilities()` (`04-settings.js:34`) reuses the same
memoized promise instead of fetching twice. `?gm=local|server` is read from the query
string next to the existing `?file=` (`00-preamble.js:260`).

`token` is the snapshot identity. The slim index and every edge response carry it; a
mismatch means the graph was regenerated under the session, and the page says so
rather than silently mixing two graphs.

### 2. The slim index — `GET /api/graph/index`

**Columnar, with interned file paths.** Measured on neo4j:

| shape | raw | gzip | client heap |
|---|---|---|---|
| object-per-node | 35.5 MB | 1.93 MB | — |
| columnar, no file | 30.9 MB | 2.29 MB | 61 MB |
| **columnar + interned file + lines** | **33.7 MB** | **2.75 MB** | **66 MB** |

File paths cost almost nothing interned — 8,910 distinct paths across 161,725 nodes —
and including them keeps **search suggestions fully local and synchronous**
(`09-search.js:73-76` renders `file`, `group` and the line range in the meta row).
Columnar is chosen over object-per-node for parse CPU, not bytes: it avoids
constructing 161k × 6 key strings.

```jsonc
{ "v":1, "n":161725, "edgeCount":745964, "token":"…",
  "ids":[…N], "names":[…N],
  "types":[…9], "typeIdx":[…N],          // dictionary-coded
  "files":[…8910], "fileIdx":[…N],       // dictionary-coded; -1 = none
  "startLine":[…N], "endLine":[…N],      // 0 = absent
  "boundary":[12, 4471, …],              // SPARSE — only 170 nodes carry one
  "deg":[…N],                            // undirected degree; makes topHubs correct
  "catalogRoots":[…],                    // Folder/File with no Folder/File parent
  "stats":{…}, "languages":{…}, "nodeTypeCounts":{…}, "edgeTypeCounts":{…} }
```

Three consequences worth naming:

- **Dictionary coding is where the heap win lives.** 158,638 `file` values collapse to
  8,910 distinct strings — inline that is ~15 MB raw and ~32 MB of duplicate JS
  strings; coded it is ~0.8 MB and ~1.8 MB. This beats *today's local mode* too.
- **`boundary` is a sparse index list**, not a column: only 170 of 161,725 nodes
  carry one.
- **The array position is the node index, and that is load-bearing.** It is what lets
  edge endpoints travel as `int32` instead of 141-char ids (mean id length is 141
  chars). A 1,500-node neighbourhood's ~14,000 edges is ~4 MB as strings and ~200 KB
  as indices. This is the largest single lever in the project and it only exists
  because the index is positional.

`IndexStats` (`types.rs:454-477`) already serialises to the camelCase shape
`transformData` reads, so `renderIndexStats` / `renderLanguages` need no change.
`deg` and `catalogRoots` cost ~1 MB and remove two whole-graph round trips.

The client rehydrates this into exactly the node objects `transformData` builds today,
minus the heavy fields — so `state.graph.nodes`, `state.nodeById`, keyword search,
the palette, filter chips, the legend, every `nodeById.has()` presence check and every
deep link keep working **unchanged and synchronously**. `state.graph.edges` is `[]`.

Built lazily per snapshot in a `OnceLock`, like `centrality`/`cycles`
(`serve.rs:143-172`), and only when the graph is in server mode.

### 3. The client seam — `state.adj` becomes a cache

Today `buildAdjacency()` (`16-solo-view.js:40`) fills `Map<id, edge[]>` from every
edge, and `edgesOf` / `neighborsOf` / `soloViewIds` / `setSoloView` read it
**synchronously**. In server mode the *same Map* is filled on demand.

**The async boundary is `rebuildSoloView`, not `handleClick`.** `handleClick` has 18
call sites; `rebuildSoloView` has 4 (`16-solo-view.js:232,245`, `08-sidebar-nav.js:249`,
`07-tour.js:110`) plus `applySoloMode`'s inline call, and it returns nothing — its
only effect is a repaint. So it becomes `async` and every caller fires and forgets.
**No caller changes, and `soloViewIds` / `setSoloView` / `neighborsOf` / `edgesOf` /
`otherEnd` are untouched.**

```js
async function rebuildSoloView() {
    if (!state.soloOnly) return;
    await ensureEdges([...state.viewSeeds, ...state.viewExpanded]);  // complete lists
    const { ids, truncated } = soloViewIds(state.viewSeeds, state.viewExpanded);
    await ensureEdges(ids, 'induced');                                // cross-links
    setSoloView(ids, truncated);
}
```

**The cache needs three states, not two.** This is the sharp edge. `edgesOf` returns
`[]` for both "no edges" and "not fetched". Worse: `setSoloView` (`:130-140`) walks all
~1500 view ids looking for induced edges, so induced-scope results get merged into
*neighbours'* lists — making those lists **partial**. A later click on such a neighbour
would then render a partial neighbourhood with no error. So:

- `state.adj: Map<id, edge[]>` — what is known
- `state.adjComplete: Set<id>` — whose list is known **complete**

`ensureEdges` skips ids already complete; induced-only edges merge into `adj` but their
endpoints are **not** marked complete. `edgesOf` on an id not in `adjComplete` warns
once and schedules `ensureEdges([id])` → `rebuildSoloView()` — a cold miss costs a
beat, never a wrong picture. That guard is the safety net that makes a missed entry
point non-fatal.

Two follow-ups stay inside the synchronous `handleClick`, both modelled on the existing
`enrichFromDb` (`14-interaction.js:482-500`), which already re-renders panel sections
asynchronously: `enterFocus` (`:463`) and the Related tab (`:190-198`). The Related tab
must render "Loading related…" on a cold id — **never** its "isolated in the graph"
message (`:331`).

**Graph walk needs no server BFS** in principle — `computeWalk` (`18-walk.js:726-765`)
is a pure layer-by-layer BFS whose frontier per hop is exactly the batch to fetch. But
a 3-hop walk from a hub reaches tens of thousands of nodes, so the server gets a
`POST /api/graph/walk` that mirrors it layer-for-layer (including the `seenEdgeKeys`
dedupe and per-layer `tally`) and can cap. The JS version stays for local mode, with a
shared fixture asserting identical layers.

`findPath` (`15-tools-catalog.js:560`) is **directed-forward** — `:575` skips edges
where the current node is not the source — so the existing `GET /api/graph/path` is an
exact match. No undirected variant is needed.

### 4. Node hydration

`state.nodeById` holds slim nodes. `hydrateNodes(ids)` fills the heavy fields
(`docstring`, `signature`, `metrics`, `imports`, `calls`, `extends`, `implements`,
`boundaries`) **in place on the same objects**, so hydration is sticky and shared by
reference. The info panel, preview, hierarchy tab and catalog rows await it. Because
the objects are the same ones the renderer already holds, nothing needs re-wiring.

### 5. Server endpoints

Reuse before adding. Handlers iterate `snap.parsed` directly (`WEB-SERVE.md:104-110`).

| Endpoint | Why | Notes |
|---|---|---|
| `GET /api/graph/index` | the slim index | new; `OnceLock` per snapshot |
| `POST /api/graph/neighbourhood` | **the one primitive that unlocks solo expansion, walk, tour, Related tab, focus/Tab** | new; body `{ids[], edgeTypes?, cap?}` → `{token, edges:[{source,target,rel}]}` in the id form `state.adj` already stores |
| `POST /api/graph/nodes` | batch hydrate | new; replaces per-id `api_node` (`serve.rs:2727`), which is a linear O(V) scan today |
| `GET /api/graph/aggregates` | edge rel-type counts (`08-sidebar-nav.js:193`, `18-walk.js:642`), top-N by degree (`15-tools-catalog.js:12`, `topHubs` `16-solo-view.js:336`), Contains roots | new; **the only whole-graph aggregates the slim index cannot answer** — node-type counts, the boundary count, the legend and `refreshModeLegend` all still compute locally off `group`/`isBoundary`. Cached per snapshot |
| `POST /api/graph/walk` | graph walk | new; mirrors `computeWalk` layer-for-layer and can cap a hub blow-up |
| `GET /api/graph/path` | `findPath` | **exists and already matches** (`serve.rs:2859`) — `findPath` is directed-forward too |
| `GET /api/graph/centrality` | the centrality panel | **exists and is cached** (`serve.rs:2965`) |
| `GET /api/graph/cycles` | cycles panel | **exists**, already wired this session |

No Contains endpoint: `getContainsMaps` / `getContainsCounts` / `state.containsMaps`
(`02-dialogs.js:446-474`, reset at `:333`) are **deleted**, and parents/children derive
from `edgesOf(id).filter(e => e.rel === 'Contains')` — one index, one code path, both
modes. Only the catalog's *roots* need a whole-graph answer, and those ride in the
slim index. (111,375 nodes have more than one Contains parent, so nothing may assume a
single parent.)

`AdjIndex` (`serve.rs:176-196`) is rebuilt to hold **edge** indices, not neighbour
indices: `out[si].push(ti)` today throws `rel` away, which is why `api_traverse`
rescans all 745,964 edges per request (`:2829-2839`). With `out`/`inc` as
`Vec<Vec<u32>>` of edge indices that scan disappears and the batch endpoint is O(degree).
`api_node`'s linear scan is replaced by the `id_to_idx` map already sitting next to it.

New handlers route through `resolve_ctx` (`serve.rs:1473`), which already does
per-request project scoping **and** calls `refresh_snapshot_if_stale` — closing both
the project-switch hazard and the no-`--watch` staleness gap for free.

### 6. Forcing solo — the arithmetic fails silently

`applySoloMode:173` computes `Math.max(state.graph.nodes.length, state.graph.edges.length)`.
In server mode that is `max(161725, 0) = 161725`, which is **below** `SOLO_THRESHOLD`
(200000) — so `want = false` and it takes the `state.view = state.graph` branch,
handing the renderer 161,725 nodes and no edges. `initialize` (`03-insights.js:460`)
has the identical bug. One shared predicate fixes both:

```js
function soloRequired(limit) {
    if (state.graphMode === 'server') return true;   // no edges locally
    return Math.max(state.graph.nodes.length, state.edgeCount) > (limit || SOLO_THRESHOLD);
}
```

`state.edgeCount` replaces `state.graph.edges.length` at four sites
(`10-render-core.js:277`, `16-solo-view.js:173`, `03-insights.js:450`, `18-walk.js:640`);
missing one re-opens the hole. `SOLO_THRESHOLD` keeps its exact spelling and formatting
— `cli/demo.rs:725` parses it by string-splitting.

## Phases

**`--graph-mode` defaults to `local` through phases 0–6.** Server mode is opt-in
(`--graph-mode=server` / `?gm=server`) and gets progressively less broken; local mode
stays byte-identical throughout. The default flips to `auto` in phase 7. That is what
makes every phase independently shippable rather than a long red branch.

Every phase ends with `cargo nextest run`, `cargo build`, `ug help` green — **and any
phase touching `native/src/vis/` ends with `ug demo --page-only` re-run and committed**
(`cli/demo.rs:683` fails otherwise). That is per phase, not once at the end.

0. ✅ **DONE — Index by position.** `AdjIndex.out`/`inc` → `Vec<Vec<u32>>` of *edge* indices;
   delete `api_traverse`'s O(E) rescan (`:2829-2839`); `api_node` via `id_to_idx`;
   `repo_root` on `api_stats`. Server only, no behaviour change.
   *Verify:* `router_tests.rs` — **`sample_graph()` has `edges: vec![]`, so an
   edge-bearing fixture is needed first.** Preserve `api_traverse`'s *induced*-edge
   semantics at the frontier.
1. ✅ **DONE — Slim index + mode publication.** `GET /api/graph/nodes` (columnar, built in
   `spawn_blocking`, `OnceLock<EncodedAsset>` on `GraphSnapshot`, counted in
   `approx_bytes()`); `--graph-mode`; the capabilities `graph` block.
   *Verify:* `jq '.n, (.ids|length), (.files|length)'` = 161725 / 161725 / 8910.
2. ✅ **DONE — Client loads the slim index** — page loads, canvas empty. `getCapabilities()`
   memo, `loadGraph` branch, `transformSlim`, `soloRequired()`, `state.edgeCount` at
   the four sites, edge-type chips and `topHubs`/`renderStartHere` off `edgeTypeCounts`
   and `deg`. *Verify:* heap ≤ ~65 MB on neo4j with `?gm=server`; search rows show
   file + line meta; `?gm=local` unchanged.
3. ✅ **DONE — The edge cache** — canvas, focus, Related. `POST /api/graph/edges`
   (`incident`|`induced`), three-state cache, async `rebuildSoloView`, cold-miss guard,
   re-entrancy token so a stale response cannot call `setSoloView`.
   *Verify:* click a hub → neighbours have **cross-links, not just spokes**; then click
   a neighbour → its own *full* neighbourhood (the partial-cache regression test); a
   filter click makes **zero** requests.
4. ✅ **DONE — Contains** — delete `getContainsMaps`; catalog, hierarchy, tooltip off the cache.
   *Verify:* server-mode tree matches local-mode tree root-by-root. Bound or disable
   `setAllCatalogExpanded` (`15:306`) and `copyCatalogMarkdown` (`:523`) — both walk
   the whole tree.
5. ✅ **DONE — Hydration** — `POST /api/graph/nodes/hydrate`, folded into `enrichFromDb`
   (`14:482`). With file/lines in slim this is the **only** consumer. `showCentrality`
   → the cached endpoint, with a spinner (betweenness on this graph is O(V·E)).
6. ✅ **DONE — Walk, path, tour.** *Deviation:* no `POST /api/graph/walk`. `computeWalk`'s
   BFS frontier per hop is exactly the batch to fetch, so one `await ensureEdges(frontier)`
   per hop reuses `/api/graph/edges` — a second copy of the layer/tally semantics in Rust is
   the drift §3a warns about. Bounded by `WALK_MAX_FRONTIER` instead. `findPath` → existing
   `GET /api/graph/path`; `applyTourFocus` gets `enterFocus`'s async follow-up.
   *Verify:* identical layer id sets and tallies in both modes on a mid-size project.
7. ✅ **DONE — Default on + docs.** `--graph-mode=auto` at 50 MiB; `token` staleness handshake;
   `API-REFERENCE.md` §1.x/§2.x → `api-reference.html` `#tab-cli`/`#tab-http` (**add
   inside the existing pane** — a second `id="tab-http"` div silently breaks all five
   tabs), `WEB-SERVE.md`, `VISUALIZATION.md`, `cli/api.rs`, `router_tests.rs:359-365`
   doc comment, demo re-publish.

## Files touched

**Server:** `native/src/serve.rs` (`AdjIndex` :176, `GraphSnapshot` :143, `api_node`
:2727, `api_capabilities` :3066, `run_serve` :850, router :1264), new handlers beside
the existing `/api/graph/*` family; `native/src/cli/api.rs` (route catalog).

**Client:** `native/src/vis/js/00-preamble.js` (mode branch in `loadGraph`),
`02-dialogs.js` (`transformData` slim path, `getContainsMaps`), `03-insights.js`
(`initialize`), `16-solo-view.js` (`buildAdjacency` → cache, `edgesOf`, entry points),
`08-sidebar-nav.js` (aggregates, `neighborIdsOf`, `refreshModeLegend`),
`14-interaction.js` (Related tab, `findNodeByName`), `15-tools-catalog.js` (catalog,
centrality, `findPath`), `18-walk.js` (`computeWalk`), `07-tour.js` (`applyTourFocus`),
`04-settings.js` (memoized capabilities).

**No new `21-*.js` part.** `bootstrap()` is invoked on the last line of
`17-info-drag.js:49` — before parts 18–20 have evaluated — so top-level `const`/`let`
in a part numbered above 17 is in temporal-dead-zone for anything running
synchronously from boot. The new code goes in the parts that already own the state:
the edge cache in `16-solo-view.js` (it owns `state.adj`), the capabilities memo in
`04-settings.js` (it owns `probeCapabilities`), the slim loader beside `transformData`
in `02-dialogs.js`. This also sidesteps the ascending-prefix rule entirely.

**Docs:** `docs/VISUALIZATION.md`, `docs/WEB-SERVE.md`, `docs/API-REFERENCE.md`,
`docs/ug-website/api-reference.html`, and the re-published `docs/ug-website/demo/`.

## Verification

```bash
cd native
cargo nextest run                                   # 818 today, all must stay green
cargo build
cargo run --bin ug -- demo --page-only -o ../docs/ug-website/demo
```

End-to-end, the numbers this plan exists for:

```bash
ug serve --project neo4j --port 8124        # expect graph.mode = "server"
curl -s localhost:8124/api/capabilities | jq .graph
curl -so /dev/null -w '%{size_download}\n' localhost:8124/api/graph/index   # ~2.8 MB gz
```

Then in the browser on neo4j, with DevTools' memory profiler: confirm **no
`/graph.json` request**, a retained heap around **66 MB** rather than 295 MB, and
walk through click → expand → filter → search → light-up → focus/Tab → catalog →
hierarchy → find-path → walk → tour. On `ug` (4.3 MB) confirm the page is
byte-for-byte the experience it is today.

## Risks

- **The partial cache is the sharp edge.** A missed `ensureEdges`, or an induced-scope
  result wrongly marked complete, is a silently wrong picture rather than an error.
  Mitigated by the `adjComplete` third state and the cold-miss guard (§3); phase 3's
  verification steps exist specifically to catch it.
- **Two walk implementations.** JS for local mode, Rust for server mode — exactly the
  drift §3a warns about. Mitigated by making Rust authoritative and sharing a fixture
  that asserts identical layers; revisit deleting the JS one once server mode is default.
- **`agent_tools` must not be reused on hot paths.** `agent_tools::shortest_path`
  (`:3150`) re-parses the whole 346 MB JSON per request (up to twice, with the reversed
  retry) and `traverse` (`:2349`) rebuilds both adjacency maps over 745,964 edges per
  call. Only `SymbolRef`'s *shape* is reusable. This is the failure `WEB-SERVE.md:104-110`
  already records once.
- **Two code paths forever.** Local mode is not going away (the demo needs it, and
  small repos are better served by it), so every graph feature now has two
  implementations. Mitigated by making local mode *the same code* with a
  synchronous-resolving `ensureEdges` — one path, two data sources — rather than
  branching per feature.
- **Server footprint grows.** The slim index and inbound adjacency are new per-snapshot
  allocations against a 512 MiB LRU budget that already scores a 346 MB graph at
  ~1.4 GB (`serve.rs:262-273`). `approx_bytes()` must be updated or the budget will
  lie.
- **Snapshot skew.** `/api/graph/*` never refreshes without `--watch`; the `token`
  handshake turns a silent wrong answer into a "reload the page" message, but does not
  make the session self-heal.
- **Tour's "not in the loaded graph.json" checks.** `07-tour.js:879`, `06-chat.js:94`,
  `:743`, `:764` and `19-url-state.js:109` all test node presence and degrade quietly.
  The slim index is what keeps these honest — it is the reason this plan ships one
  rather than going fully server-backed. Any field they start reading beyond
  `{id,name,type,file,lines,boundary}` re-breaks them.

## Noted, deliberately out of scope

- **`EncodedAsset::new` gzip-9s *and* brotli-9s the whole 346 MB at snapshot load**
  (`serve.rs:789`) — tens of seconds of startup and ~1.4 GB peak. In server mode the
  browser never fetches `/graph.json`, so that work becomes pure waste (only the two
  download buttons still use it). Skipping pre-compression above the threshold is a
  real separate win; not taken here.
- **The LRU budget already lies.** `approx_bytes()` estimates parsed at 3× identity, so
  this graph scores ~1.4 GB against a 512 MiB default — two large projects thrash.
  Pre-existing; this plan must not make it worse (hence counting the slim index).
- **There is no JS test harness.** `vis_assembly_test.rs` is structural only, so
  phases 2–6 are verified by hand against `~/.ug/neo4j` plus the demo page. Adding real
  client-side regression cover is a separate decision.

