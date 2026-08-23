## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 3a. Breaking changes are cheap right now

**`ug` has no users yet. Do not carry compatibility for people who don't
exist.**

When something is renamed or superseded, delete the old thing outright:

- No aliases for the old spelling. No deprecation shims that print "use X
  instead". No `#[deprecated]`, no re-exports, no "kept for back-compat".
- No parameter kept accepting an old shape "just in case".
- Rename freely: CLI subcommands, MCP tool names, JSON fields, stored
  property names, on-disk formats.

Each alias is a second name to document, test, and keep behaving
identically — and duplicates drift. `graph_bfs` and `traverse` did the same
graph walk and had already diverged on whether a bare symbol name was
accepted; that divergence is what a compatibility alias buys you.

Store formats are the one place to still be deliberate: bump
`STORE_FORMAT_VERSION` / `GRAPH_SCHEMA_VERSION` so an existing index is
*rejected with a "run `ug gen`" message* rather than read as garbage. That
is not backward compatibility — it is refusing to answer from data this
build cannot read, which stays correct however old the index is.

Revisit this section at the first real release. Until then, prefer one name
per thing and a clean deletion over a graceful migration path.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. LLM Features Must Feel Fast

**Most of the wall-clock is the model writing tokens. Budget them like money.**

`ug` is routinely pointed at a local endpoint doing ~30 tokens/second. At that rate a
feature is fast or slow depending almost entirely on how many tokens it asks for and how
long the user waits before seeing anything. Four rules, in order of impact:

1. **Turn thinking off unless the task needs it.** A reasoning model spends tens of
   thousands of tokens deliberating *before* its first useful character — the guided tour
   took over ten minutes this way. Prompt instructions do NOT stop it: thinking lives in
   the chat template. Send the real switches instead — `chat::no_think_body()`
   (`chat_template_kwargs.enable_thinking:false` + `reasoning_effort:"low"`), which
   `ChatClient` merges via `ChatConfig::extra_body` and retries without on a 4xx. Same
   tour, same model: **10+ min → 9.4 s**. Give users `--think` to opt back in.
2. **Show progress from the first second.** Stream the completion and report phases
   (`TourProgress` → SSE `event: progress`, or the CLI's in-place status line): what was
   retrieved, which model is writing, tokens so far, rate, elapsed. A silent wait feels
   broken; a narrated one feels like work being done.
3. **Deliver partial results as they arrive.** Don't wait for the whole reply to be
   parseable. `StopScanner` pulls each finished stop out of the still-streaming JSON so the
   tour starts walking on stop 1 — roughly half the wait removed on every tour.
4. **Ask for less.** Shorter prompts (clip snippets and descriptions; only send code for
   the top candidates) and shorter outputs (cap narration length explicitly) cut generation
   time directly. The tour's prompt went 10.8k → 6.9k chars with no loss of quality.

Verify with a real slow endpoint, not a mock: mocks answer instantly and hide exactly the
problem you're trying to fix.

**One toolbox, two doors.** When a model needs to reach into the graph, give it the MCP
tools rather than a parallel set: `mcp::tools::tool_list()` is the single source of schemas
and descriptions, converted to OpenAI `function` shape for `/api/chat`. Descriptions are
load-bearing prompt text — writing a second copy means two things to keep true. Run tool
rounds non-streaming (partial `tool_calls` deltas are the ugliest corner of the wire
format), stream only the final answer, and show every call to the user as it happens.

## 6. Test Before Commit

**Always verify changes with tests before marking a task complete.**

### Rust Tests (Native Code)
- Run `cd native && cargo nextest run` to execute all Rust tests
- **Use `cargo nextest run`, not `cargo test`.** `cargo test -- --test-threads=N`
  only parallelizes within a single test binary, so the big ones serialize
  against each other and most of the machine sits idle. nextest schedules every
  test across all binaries in one pool, one process per test.
  Measured on the 18-core dev machine, warm, same 809 tests: `cargo test`
  12.3s, `cargo test -- --test-threads=15` 12.4s (i.e. the flag buys nothing),
  `cargo nextest run` 5.0s.
  Thread count is already set in `native/.config/nextest.toml`
  (`test-threads = -3` — all logical CPUs minus 3, so 15 on this 18-core
  machine), so **do not pass `-j` yourself**; the config is the one place to
  change it.
- Install once with `cargo install cargo-nextest --locked`. If nextest is
  genuinely unavailable, `cargo test -- --test-threads=15` is the fallback.
- Tests are in `native/tests/`: `indexer_test.rs`, `graph_test.rs`, `search_test.rs`,
  `storage_test.rs`, `rust_indexer_test.rs`, `pdf_indexer_test.rs`, `storage_bench.rs`,
  and the `#[ignore]`-gated `neo4j_smoke.rs` / `neo4j_write_smoke.rs`. They compile
  into **one** binary via `tests/integration.rs` — a new file there needs a `mod` line in
  that harness or it is silently not built. See its module comment for why.
- **Run these tests after every code change in the native folder**
- If adding new functionality, add corresponding test cases to the test files
- Ensure all tests pass before completing a phase

### Running Less Than Everything

A full `cargo nextest run` after touching `src/` is ~2 minutes, and almost none
of that is testing — the 885 tests execute in 4.3 seconds. The rest is linking
and, on macOS, `syspolicyd`/`XprotectService` scanning each freshly linked
binary. So the way to a fast loop is to relink fewer binaries, not to run fewer
tests:

```bash
cargo check --bins                 # 0.2s warm — does it compile? use this while editing
cargo nextest run --lib            # unit tests: CLI/serve/mcp/tour/project + the library
cargo nextest run --test integration   # only the integration tests
cargo nextest run -E 'test(/^search_test::/)'   # one former test file
```

`--lib` covers more than the name suggests. `cli`, `serve`, `chat`, `tour`, and
`project` are modules of the *library* (`src/main.rs` is a shim over
`ultragraph::cli::run`), so their unit tests are in the lib harness and no
`--test` flag reaches them. **After any change under `src/vis/` (html/css/js)**
the two that must pass are both there:

```bash
cargo nextest run --lib \
  -E 'test(the_published_demo_page_is_not_stale) + test(the_solo_threshold_matches_the_renderer)'
```

If you are on macOS and have not added your terminal's host app (VS Code,
Ghostty, Terminal) to System Settings → Privacy & Security → Developer Tools,
do that once — it exempts everything the app spawns from the Gatekeeper
assessment that dominates the numbers above. **Quit and relaunch the app after
granting it**: TCC resolves against the responsible process, so an app that was
already running keeps the pre-grant decision for every shell under it. The grant
follows the app, not the directory — build from two terminals and both need it.

`native/target/.metadata_never_index` keeps Spotlight out of the build tree.
Without it `mds`/`mds_stores`/`corespotlightd` re-index every `.rcgu.o` object
file cargo rewrites. `cargo clean` deletes the marker along with `target/`, so
`touch native/target/.metadata_never_index` again afterwards.

Do not bother shrinking the binaries to reduce scan time: `strip -S` on the test
harness takes it from 58.3 MB to 56.5 MB. Only 3% is debug info — `debug =
"line-tables-only"` plus `debug = false` for dependencies (see `Cargo.toml`)
already squeezed that dry, and the rest is genuine statically linked code.

### Verification Checklist
```
1. cd native && cargo nextest run   → all tests must pass
2. cd native && cargo build         → native module must build
3. ./native/target/debug/ug help    → CLI works
```

`cargo build` builds the `ug` binary only. The `ug-app` desktop shell needs
`--features app`, which pulls in Tauri and ~110 more crates; CI's clippy job
runs `--all-features`, so leaving it out locally cannot hide a break in it.
Touch that binary or `build.rs` and you should run the feature-enabled build
yourself before pushing.

The first test run on a fresh checkout downloads the ONNX runtime and the
embedding model (several hundred MB, ~9 minutes on a home connection) the
first time a test reaches the embedder. It is a one-time cost cached under
`~/.cache`; a warm run finishes in seconds. Do not read that first run
as a slow test suite.

nextest does not run doctests — it cannot, by design. That costs us nothing
today: every ` ``` ` block in `native/src/` is fenced as `text`, so there are
no doctests to skip. Add a real doctest and you have to run `cargo test --doc`
alongside nextest to see it.

## 7. Documentation & Website

**The website lives in this repo at `docs/ug-website/`. Updating it is part of
the change, not a follow-up task.**

It used to be the separate `shoocstorm/AI-Ultra-Graph-RAG-for-AI-Agent`
checkout, symlinked in. That repo is superseded — edit the files here.

### 7.1 What is in `docs/ug-website/`

| File | What it presents | Anchors you will edit |
|------|-----------------|----------------------|
| `index.html` | Slide-deck landing page | `#what` `#how` `#demo` `#features` `#agents` `#showcase` `#get-started` |
| `api-reference.html` | Multi-tab API reference, mirroring `docs/API-REFERENCE.md` | `#tab-cli` `#tab-http` `#tab-mcp` `#tab-storage` `#tab-pipeline` |
| `architecture.html` | Architecture diagram, in two tabbed layouts of the *same* pipeline | `#tab-horizontal` (default) `#tab-vertical` |
| `install.sh` | The script behind the landing page's `curl \| sh` install | — |
| `img/UG-*.png` | Screenshots used by the hero background and `#showcase` | — |
| `404.html`, `favicon.svg` | Static assets served at the site root | — |
| `firebase.json`, `.firebaserc` | Hosting config — see 7.3 | — |
| `demo/` | **Generated** — the live demo at `/demo/`. See 7.4 | — |
| `docs/deploy.md` | How to deploy, and how to regenerate `demo/` | — |

Everything except `demo/` is a hand-written static page. **There is no build
step and no generator** — nothing regenerates them from the markdown, which is
exactly why they go stale unless you edit them in the same pass.

### 7.2 The standing trigger — apply without being asked

If a change lands in the left column, the right column is part of that same
change. Do not wait to be told, and do not file it as future work.

| You changed… | Update, in the same commit |
|---|---|
| A CLI subcommand, flag, or its help text | `docs/API-REFERENCE.md` §1.x → `api-reference.html` `#tab-cli`. If it is a headline capability, also `index.html` `#features` |
| An HTTP route, its params, or its response shape | `docs/API-REFERENCE.md` §2.x → `#tab-http` |
| An MCP tool — name, schema, or **description text** | `docs/API-REFERENCE.md` §3.1 → `#tab-mcp` → `index.html` `#agents` |
| A storage backend, `StoreSpec`, or `KnowledgeStore` method | `docs/API-REFERENCE.md` §4.x → `#tab-storage` |
| The pipeline, `index.json`, or `graph.json` schema | `docs/API-REFERENCE.md` §5.x → `#tab-pipeline` |
| Install or upgrade steps | `index.html` `#get-started` **and** `docs/ug-website/install.sh` — they must agree |
| Architecture or component boundaries | `architecture.html` — **both tabs**, `#tab-horizontal` and `#tab-vertical`, which draw the same pipeline at different densities |
| A user-visible UI feature worth showing off | `index.html` `#showcase` (reuse an existing `img/UG-*.png` unless the feature is genuinely new) |
| Anything under `native/src/vis/` (css, js, the shell, the shim) | `ug demo --page-only` — the live demo ships a *copy* of that page and keeps serving the old one until you re-publish it. `the_published_demo_page_is_not_stale` fails until you do. See 7.4 |
| The indexer, or the `graph.json` shape | `./scripts/gen-demo.sh` — a full re-publish, so the demo's snapshot is one this build could actually have produced. The landing page's counts follow `demo/demo.json` on their own |
| An endpoint the page calls **at startup**, or any fetch outside `/api/` | `native/src/vis/demo-shim.js` must answer it — on a static host there is no such route, and the demo blocks on a request nothing will serve |

Renames and deletions count. Per §3a there are no aliases, so a command that
disappeared from the CLI must disappear from the website too — a page
documenting a command that no longer exists is worse than no page.

While editing the HTML:

- Match the surrounding markup and class names. Do not reformat, re-indent, or
  "modernize" a page you are only adding a row to (§3).
- **Keep the `#tab-*` ids and `data-tab` values stable** — the tab bar's JS
  pairs them by name. **Exactly one `.tab-content` per id.** To extend a tab,
  add the section *inside* its existing pane; opening a second
  `<div class="tab-content" id="tab-cli">` looks harmless but silently breaks
  every tab — the later div wins the id lookup, so the earlier one keeps its
  hardcoded `active` and shows under all five tabs. This has already shipped
  once.
- In `architecture.html`, the two tabs reuse the same class names (`.card`,
  `.tag`, `.chip`, `.bullet`, `.icon`, `.code`, …) at different sizes, so every
  rule is scoped under `.view-h` or `.view-v`. **Never unscope one** — it
  silently restyles the other tab. Add new rules inside the matching block.
- Every command string on the page must be copy-paste runnable. Verify it
  against actual `ug help <cmd>` output, not from memory.
- `favicon.svg` is referenced from the site root (`/favicon.svg`); screenshots
  are referenced relatively (`img/UG-Query.png`, and `url('img/UG-Graph.png')`
  in the inline `<style>` block). Keep new screenshots in `img/`.
- Screenshots are still multi-MB each. Reuse what is in `img/`, and compress
  anything new — a 33 MB animated GIF was already removed once for this reason.
  Two are currently unreferenced (`UG-Guided-Tour.png`,
  `UG-Semantic-Search.png`); prefer wiring one of those in over adding a file.

### 7.3 Deploying to Firebase

`firebase.json` sets `"public": "."`, so the deploy **must run from inside the
website folder**:

```bash
cd docs/ug-website
firebase login          # once
firebase deploy --only hosting:ultra-graph
```

Do not hoist `firebase.json` / `.firebaserc` to the repo root or rewrite
`public` to a path — the pages reference assets at the site root, and repointing
`public` silently changes what `/favicon.svg` and `install.sh` resolve to.
`.firebase/` (the local deploy cache) is gitignored.

Deploying publishes to a live public site. Edit and commit freely; **deploy only
when asked.**

### 7.4 The live demo (`docs/ug-website/demo/`)

The one generated directory under the website: a real indexed repo a visitor
can fly before installing anything. `ug demo` writes it — `graph.json` plus the
visualization page, wrapped in a static stand-in for the server. No database,
no vectors, no backend. It deploys with the site because `"public": "."`
publishes the whole folder.

**Editing `native/src/vis/` changes two deployments, not one.** The app gets
your edit on the next `cargo build`; the public demo is a *copy* of that page
and gets it only when re-published. A copy cannot notice its original moved,
so this fails with no symptom: the build is fine, the tests are fine, `ug
serve` is fine, and the live demo quietly keeps serving the old renderer.

You do not have to remember this. A test does:

```
the_published_demo_page_is_not_stale   (native/src/cli/demo.rs)
```

It compares a hash of the assembled page + shim against the one stamped in
`demo/demo.json`, and prints the fix. It runs in CI too — the demo is
committed, so `cargo test` on every PR sees the same mismatch you would.

The fix is cheap, and is the one to reach for after a vis edit:

```bash
cargo run --bin ug -- demo --page-only   # rewrites the page; graph.json untouched
./scripts/gen-demo.sh                    # full re-publish: re-indexes, new snapshot
```

Prefer `--page-only`. A full re-publish rewrites `graph.json` — 2.7 MB, and
`stats.lastIndexedAt` moves every run, so it churns the repo on every CSS
tweak. Re-publish when the *snapshot* should move (indexer or graph-schema
changes, or the demo has drifted from the code it shows), not when the page did.

Full notes — overrides, what ships, the failure modes — live in
`docs/ug-website/docs/deploy.md`. Four more things that decide whether an edit
here lands:

- **The page is embedded in the binary.** `build.rs` assembles `native/src/vis/`
  into `ug`, so regenerating with a stale `ug` silently republishes that build's
  page and your edit appears to do nothing. `gen-demo.sh` builds first for
  exactly this reason — do not "optimize" that away.
- **The demo's behaviour lives in one file**, `native/src/vis/demo-shim.js`.
  Nothing under `native/src/vis/js/` knows the demo exists, and it must stay
  that way — that is what stops the demo and the real app from drifting. The
  shim answers `/healthz`, `/api/projects` and `/api/capabilities` and refuses
  every other `/api/*`; a fetch to a path outside that prefix, or a new
  blocking startup request, reaches a static host with no such route. Teach the
  shim, never branch on "am I the demo" inside `js/`.
- **Commit the generated files.** The deploy publishes the working tree.
- **The landing page's counts need no maintenance.** `index.html` reads
  `demo/demo.json` at load time to fill `.demo-facts`; the numbers in the
  markup are just the fallback for a failed fetch. Do not re-hardcode them —
  they were hardcoded first, and went stale within a day.

## 8. OverGraph — the storage engine

`ug`'s graph store is [OverGraph](https://overgraph.io), an embedded Rust graph
database. It is a sibling checkout in this VS Code workspace
(`ug.code-workspace` → `../overgraph`), so its source is directly readable.

**Full API reference: `/Users/aldrickwan/Documents/project/overgraph/docs/API-REFERENCE.md`**

It is ~7,700 lines / 314 KB — **never read it whole.** It opens with a
`## Table of Contents` (line 15) linking every method. Read the ToC, then jump
to the one section you need. Landmarks as of v0.17.0:

| Section | Line |
|---------|------|
| Table of Contents | 15 |
| Data Model (nodes, edges, `PropValue`, labels) | ~27 |
| Property Index Management | ~66 |
| Queries (node / edge / graph-row / pipeline) | 2759 |
| **GQL** (Cypher-style query language) | 3584 |
| — Read Syntax | 3613 |
| — Parameters and Options (**caps, `allow_full_scan`**) | 4325 |
| — Explain, Profile, and Stats | 4611 |
| — Current Limits (what GQL rejects) | 5113 |
| Traversal (`neighbors`, `traverse`, `shortest_path`) | ~5169+ |

Two things that bite immediately when writing GQL against it:

- **`allow_full_scan` defaults to `false`.** Any query without a bounded anchor
  — which is most aggregate/statistics queries — fails until you opt in.
- **Every execution is capped** (`max_groups`, `max_frontier`, `max_path_hops`,
  `max_rows`, …). Caps truncate rather than error, so results can be silently
  partial. Read `caps` and `warnings` off the result and surface them.

**Version discipline:** the crate moves fast and renames public API between
minor versions (0.6 → 0.17 renamed eight symbols and changed node/edge type ids
from `u32` to string labels). Pin exactly in `native/Cargo.toml`, read the
changelog on every bump, and keep engine calls behind the `KnowledgeStore`
trait (`native/src/storage/store.rs`) so upgrades stay confined to
`native/src/storage/db.rs`.

## 9. Two bugs this codebase keeps re-introducing

Both are invisible in review, silent at runtime, and have each already shipped
here more than once. Check for them by reflex.

### 9a. Canonicalize *both* sides of a path comparison

`canonicalize` resolves symlinks. Comparing a resolved path against an
unresolved one always fails, and the failure looks like a correct security
denial or a correct "not found" — never like a bug.

```rust
// WRONG — root still contains a symlink component
let canon = std::fs::canonicalize(root.join(rel)).ok()?;
if !canon.starts_with(root) { return forbidden(); }

// RIGHT — resolve the root once, then compare
let root = std::fs::canonicalize(root).unwrap_or(root);
```

macOS makes this fire constantly: `/tmp` → `/private/tmp`, `/var` →
`/private/var`, so every `TempDir` and anything under `/var/folders/…` trips
it. It has bitten `ug` at least three times — `indexer.rs` (`strip_repo_root`
stripped nothing, leaving absolute paths in qualified names; see the comment at
`indexer.rs:202`), and `serve.rs` `file_from_disk` (every file preview 403'd as
"path escapes repo root" when `repo_root` came from a non-canonical source).

Rules:
- Canonicalize at the point the path is **stored**, so the invariant holds for
  every later reader, not at each comparison site.
- A path that fails to canonicalize (doesn't exist yet) keeps its raw form —
  `unwrap_or(raw)`, never `unwrap()`.
- `read_link` returns the symlink's literal target, which may be relative.
  Resolve it before comparing it to anything.
- Any check of the form "is A inside B" needs a test with a symlinked path.
  A `TempDir` on macOS is that test for free.

### 9b. No unbounded work inside `async fn`

An `async fn` body runs on a tokio worker thread. Anything that doesn't `.await`
holds that worker until it returns, stalling every other in-flight request —
this is not "slow", it is a server-wide stall.

Push to `tokio::task::spawn_blocking` when the work is:
- **filesystem traversal or IO whose size scales with user data** — reading
  `graph.json`, one `stat` per indexed file, `read_dir` (which can hang on a
  network mount);
- **whole-graph CPU** — `serde_json::from_str` on a multi-MB graph, centrality,
  cycle detection, the index → graph pipeline;
- anything you cannot bound by a small constant.

A single `canonicalize` or `metadata` call is fine inline — the threshold is
*unbounded*, not *touches the filesystem*.

```rust
// WRONG — 16 MB read + parse + one stat per file, on a runtime worker
async fn handler(...) -> Response {
    let s = std::fs::read_to_string(&graph)?;
    let v: Value = serde_json::from_str(&s)?;
    for f in files { std::fs::metadata(f); }
}

// RIGHT
let body = tokio::task::spawn_blocking(move || scan(...)).await?;
```

Prefer deleting the work over moving it: `/api/projects/staleness` was parsing
every project's `graph.json` on a 2-minute poll to recover a file list that
`ug gen` could just record in `project.json`. `spawn_blocking` fixed the stall;
persisting the list removed the work. Also cache results that several clients
poll for — N open tabs should cost one scan, not N.

And note the CPU-bound helpers take `&GraphData`, not `String`
(`calculate_centrality_graph`, `detect_cycles_graph`). The `String` overloads
re-parse the graph from scratch; never call those when a parsed graph is in
scope.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
