# UI/UX Enhancements — `ug serve` web UI

Scope: the HTML UI served by `ug serve` — `native/src/vis/index.html` plus
`native/src/vis/css/*.css` and `native/src/vis/js/*.js` (the `{{CSS}}` / `{{JS}}`
placeholders are filled at build time; `serve.rs` only routes `/index.html` and
`/ug-vis.bundle.js`).

These are the three changes with the highest ratio of user-visible payoff to
implementation cost, ranked. Each is grounded in what the code does today.

## Status: all three implemented

| # | Item | Landed in |
|---|------|-----------|
| 1 | Deep-linkable URL state | `js/16-url-state.js` (new), plus `writeUrlState()`/`pushUrlState()` hooks in `08-sidebar-nav.js`, `09-search.js`, `11-interaction.js`, `12-tools-catalog.js`; `p=` deep link in `01-kb-manager.js` bootstrap; Copy link button beside `#jump-btn` in `index.html` |
| 2 | ⌘K palette + `?` shortcut sheet | `js/17-palette.js` (new) — `KEYMAP` registry, palette overlays, capture-phase key bindings (`⌘K`, `?`, `r`, `t`); overlays + `<kbd class="btn-kbd">` hints in `index.html`; `css/15-enhance.css` (new) |
| 3 | Honest loading + start-here | `loadGraph()` rewrite in `js/00-preamble.js` (streaming progress via `Content-Length` reader, `response.ok`, failure card with Retry / Back-to-KBs); `renderStartHere()` in `12-tools-catalog.js` rendered by `initialize()` into the Graph tab. The overlay now stays up *through* the render phase too: `initialize()` switches it to an indeterminate "Rendering graph…" sweep, and `graphReveal()` in `js/10-graph-render.js` releases it on the engine's first painted frame (plus a 4 s fallback), so the force-layout + first WebGL paint is never a blank canvas. |

The prose below is the original design record. Where it cites a line number,
treat it as approximate — the implementation shifted them; the files named in
the status table above are the current truth.

---

## 1. Deep-linkable URL state (share a view, survive a reload)

### What happens today

The URL carries almost nothing. A grep across `js/` finds exactly two uses of
URL state:

- `js/00-preamble.js:121` — `?file=` picks which `graph.json` to fetch.
- `js/01-kb-manager.js:85-90` — `?kbOpen=1`, an internal flag used once to skip
  the KB manager after a project switch, then immediately stripped via
  `history.replaceState`.

Everything else lives only in the in-memory `state` object
(`js/00-preamble.js:39-70`): `selectedNode`, `focusNode`, `focusIsolate`,
`nodeFilters` / `edgeFilters`, `history` / `historyIndex`, `semMode`,
`semDest`, the active tab and sub-tab, the search query, the active tour.
`localStorage` is used for two things only — `ug-sidebar-width` and
`ug-info-width` (`js/08-sidebar-nav.js:35,44,72,82`).

Consequences:

- **Nothing is shareable.** The single most natural thing a user wants to do
  with this tool — "here's the node that answers your question, look" — is
  impossible. You can only tell someone: open the UI, pick the project, go to
  Discover → Search, type this, click the third result.
- **Reload is destructive.** A refresh (or an accidental ⌘R, or the server
  restarting during `ug gen`) drops you at the KB manager and loses the
  focus set, filters and navigation breadcrumb you built up.
- **Back/forward is fake.** `state.history` / `historyIndex`
  (`js/08-sidebar-nav.js:292+`) implements its own back/forward stack with
  dedicated buttons, but the browser's own Back button does something
  completely different (leaves the app). Two histories, one of which lies.

### Proposed

Make a small, explicit slice of `state` the URL, and treat the URL as the
source of truth on load.

```
/?p=<project>&n=<nodeId>&focus=solo&nf=Function,Class&ef=Calls&tab=discover:search&q=auth
```

Suggested keys — keep it short and stable, ignore unknown keys:

| Key     | Meaning                                | Source in `state`               |
|---------|----------------------------------------|---------------------------------|
| `p`     | project name (multi-project mode)      | KB manager selection            |
| `n`     | selected node id                       | `state.selectedNode`            |
| `focus` | `off` / `dim` / `solo`                 | `focusNode` + `focusIsolate`    |
| `nf`    | node-type filters (CSV)                | `state.nodeFilters`             |
| `ef`    | edge-type filters (CSV)                | `state.edgeFilters`             |
| `tab`   | `graph` / `discover:search` / `catalog`| active tab + sub-tab            |
| `q`     | last search query                      | search / semantic input         |
| `tour`  | saved tour id                          | tour history entry              |

Implementation sketch:

1. Add `js/16-url-state.js` with `readUrlState()` / `writeUrlState()` and a
   `SYNC_KEYS` list. One module owns the encoding; nothing else touches
   `location`.
2. Call `writeUrlState()` (debounced ~250 ms, `history.replaceState`) from the
   existing mutation points: `focusNode()` / `enterFocus()` / `clearFocus()`
   (`js/08-sidebar-nav.js:241-286`), the filter-chip handlers, tab switches,
   and `handleClick` (`js/11-interaction.js:67`).
3. Push a real history entry (`history.pushState`) on *node selection* only,
   and add a `popstate` listener that replays into the existing
   `state.history` machinery with `suppressHistory = true` — the flag already
   exists for exactly this "replaying, don't re-record" case
   (`js/00-preamble.js:66`). This makes the browser Back button and the in-app
   Back button the same button, which is what every user expects.
4. In `loadGraph()` (`js/00-preamble.js:120`), after `initialize()`, apply the
   parsed URL state; if `p=` is present, skip the KB manager the same way
   `kbOpen=1` already does.
5. Add a **Copy link** button next to `#jump-btn` in the node details header
   (`index.html:1064`) — it already sits beside a "Copy file path" button, so
   the affordance is established.

### Why this one is first

It is the smallest diff of the three, it needs no new visual design, and it
converts the UI from a personal viewer into something that can appear in a PR
description, a Slack thread, or an agent's answer. It also fixes the
back-button lie, which is a persistent low-grade papercut rather than a
one-time annoyance.

---

## 2. A command palette (⌘K) and a discoverable shortcut layer

### What happens today

There are real keyboard shortcuts, and they are genuinely good:

- `[` toggles the sidebar (`js/08-sidebar-nav.js:18-23`)
- `Tab` / `Shift+Tab` step the selection through the focus anchor's neighbours
  (`js/08-sidebar-nav.js:317+`)
- `Backspace` / `Shift+Backspace` back/forward, `Esc` exits focus
  (`index.html:446-459`)
- `↑↓` / `⏎` in search suggestions (`js/09-search.js:12`)
- A full transport layer during a tour: `Space`, `←`, `→`, `S` speed, `C` code,
  `W` warnings, `O` solo, `D` details, `J` plan JSON, `Esc` exit
  (`js/07-tour.js:1572-1599`)

None of this is discoverable. `grep -rin "shortcut\|hotkey" js/ index.html`
returns **zero matches** — there is no help overlay, no cheatsheet, no `?`
binding. The only surfacing is `title=` attributes on individual buttons, which
require you to already know where the button is and to hover it long enough.

Separately, navigation is deep: the sidebar is 3 tabs (Summary / Discover /
Catalog, `index.html:562-566`) and Discover is 4 more sub-tabs (Search / Tour /
Chat / Insights, `index.html:605-637`). Reaching "run the dead-code insight" is
Discover → Insights → filter → click. Reaching a specific function is Discover →
Search → type → arrow → enter.

### Proposed

**(a) `⌘K` / `Ctrl+K` command palette** — one input that dispatches to
everything already implemented:

- *Nodes* — reuse the existing suggestion engine in `js/09-search.js`, so the
  palette gets fuzzy node jump for free.
- *Insights presets* — the preset list already renders into `#ins-presets`
  (`js/03-insights.js:155`); expose the same array as palette commands.
- *Actions* — Start tour, Reset view (`resetView`, `js/11-interaction.js:1038`),
  Toggle solo/box/spin (`index.html:494-496`), Detect cycles, Download graph
  JSON, Open settings, Switch project, Collapse sidebar.
- *Recent tours* — `#tour-history-list` entries (`index.html:687`) replay
  without a model call, which makes them ideal palette items.

Prefixes keep it fast and typo-proof: `>` for actions, `#` for nodes, `?` for
insights, bare text searches everything.

**(b) `?` opens a shortcut sheet** — a single modal, generated from one
declarative table so it cannot drift from the bindings. The tour keys should
appear as a context section that highlights while a tour is active.

**(c) Show the binding where the action lives.** Add a `<kbd>` hint to the
primary buttons (`Start tour`, `Reset View`, `Exit focus`) rather than burying
it in `title`. The tour overlay already does this correctly in its tooltips
("Play / pause (space)") — extend that convention outward.

Implementation notes: the modal machinery already exists — `js/02-dialogs.js`
has overlay open/close with `Esc` handling at lines 164 and 194, and
`js/04-settings.js:449` does the same for the settings panel. Reuse that, don't
write a third one. Register a central `KEYMAP` array and have both the palette
and the `?` sheet read from it; the existing per-module `keydown` listeners can
stay, but new bindings should register through the map.

### Why this one is second

It costs more than #1 but it is what makes the existing depth *reachable*.
Right now the app has four powerful modes (search / tour / chat / insights) and
the cost of getting to any one of them is 2–4 clicks through nested tabs, which
biases every user toward whichever tab they landed on. A palette flattens that
to one keystroke and simultaneously fixes the shortcut-discoverability hole.

---

## 3. Honest loading, an honest failure state, and a "start here" for the first 10 seconds

### What happens today

**Loading is a black box with a lie at the end.** `loadGraph()`
(`js/00-preamble.js:120-136`) is:

```js
document.getElementById('loading').style.display = 'block';
try {
    const response = await fetch(file);
    const data = await response.json();
    ...
} catch (err) {
    console.error('Failed to load graph:', err);
    document.getElementById('loading').innerHTML =
        '<p style="color:#f87171">Failed to load graph.</p>';
}
```

Problems, in order of severity:

- **`response.ok` is never checked.** A 404 or a 500 whose body isn't JSON only
  fails at `.json()`; a 500 that *does* return a JSON error object is treated
  as a successful graph load and flows into `transformData` with garbage.
- **The error state is terminal.** No retry button, no reload, no indication of
  *why* — the actual reason goes to `console.error`, where no user will look.
  The only recovery is a manual refresh, which (per #1) also loses your place.
- **No progress.** A large repo's `graph.json` is a multi-megabyte single
  fetch behind a static "Loading graph..." (`index.html:1083-1086`) with no
  bytes-received, no node count, no sense of whether it's working or hung.

**The first view has no entry point.** After load, the user faces a force-graph
of every node at once. There is a `#health-badge` and an Index stat grid
(`index.html:962-979`), but nothing says *where to look*. Meanwhile the app
already computes exactly the answer: degree and betweenness centrality
(`#degree-centrality` / `#betweenness-centrality`, `index.html:1019-1021`) —
but it's buried in the **collapsed-by-default** Tools section
(`index.html:1004`), behind a tab, behind an accordion.

### Proposed

**(a) Fix the fetch.** Check `response.ok`, read the body once, and surface the
real reason:

```js
const response = await fetch(file);
if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
```

Render a proper failure card in `#loading`: the reason, the URL that failed,
a **Retry** button that re-calls `loadGraph()`, and a **Back to knowledge
bases** button that calls the existing `showKbManager()`. Two dead ends become
two live paths.

**(b) Stream the progress.** Use the `Content-Length` header plus a
`response.body.getReader()` loop to drive a real progress bar, then switch the
label through the phases that already exist as distinct steps —
*Downloading (4.2 MB)* → *Building graph* → *Laying out* — with the node/edge
counts filled in as soon as `transformData` knows them. The KB wizard already
streams live progress into `#kb-wizard-log` (`index.html:190`), so the visual
language for "this is working, here's what it's doing" is established; the
graph load should match it.

**(c) Give the first view a starting point.** On first load of a project,
surface the top 5 nodes by degree as an inline **"Start here"** strip above
the stat grid in the Summary tab — clicking one focuses it, which drops the
user straight into the focus/neighbour-stepping flow that is the app's best
interaction. This is a presentation change only: `showCentrality()` already
produces the data (`js/08-sidebar-nav.js:108`).

Consider also un-collapsing Tools, or promoting Centrality out of it — a
three-tab accordion is the wrong place for the one thing that answers "what is
this repo about?"

### Why this one is third

It is the least glamorous but it governs the first ten seconds, which is where
users decide whether the tool is solid. The `response.ok` bug in particular is
a correctness issue, not just polish: a server error currently renders as a
broken graph rather than an error message.

---

## Runners-up (worth logging, not in the top 3)

- **Responsive / small-screen layout.** There is exactly one `@media` query in
  the entire stylesheet set — `prefers-reduced-motion` in
  `css/07-kb-manager.css:566`. No breakpoints at all. With a fixed
  `--sidebar-width` (min 300 px) plus a `--info-width` (min 320 px) both
  overlaying the canvas, a laptop at 1280 px is already tight and a tablet is
  unusable. At minimum: collapse the sidebar to an overlay below ~1100 px.
- **Accessibility.** `aria-` appears 20 times in `index.html` but only 15 times
  across all 13 JS modules combined, and most dynamically rendered
  lists (search results, catalog rows, insight results) ship no roles, no
  `aria-selected`, and no focus management. The KB manager cards already do
  this correctly (`js/01-kb-manager.js:123-175`, with a comment explaining
  the `role`/`tabIndex`/`keydown` pattern) — that pattern should be applied to
  the other lists.
- **Light theme.** The palette is hard-committed to dark. Not urgent, but the
  colour config is already centralised in `config.colorMap` /
  `config.relColorMap` (`js/00-preamble.js:9-36`), so a CSS-variable pass would
  be cheaper here than in most codebases.
- **Empty and zero-result states.** Search, insights and catalog filters render
  nothing when they match nothing — no "no results, try X" copy.

---

## Suggested order of work

All three have landed (see status table at top). Backlog for the next pass,
from the runners-up above:

1. **Responsive / small-screen layout** — the only `@media` query is
   `prefers-reduced-motion` in `css/07-kb-manager.css`; collapse the sidebar to
   an overlay below ~1100 px.
2. **Accessibility pass** — apply the KB-manager card `role`/`tabIndex`/`keydown`
   pattern (`js/01-kb-manager.js`) to search results, catalog rows and insight
   results; add focus management to the new palette.
3. **Empty / zero-result states** — copy for search, insights and catalog
   filters that match nothing.
4. **Light theme** — CSS-variable pass over `config.colorMap` /
   `config.relColorMap` (`js/00-preamble.js`).

Per AGENTS.md §3a there are no users yet — a superseded idea in this doc should
be deleted outright, not kept as an alias.
