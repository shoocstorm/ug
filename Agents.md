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
- Run `cd native && cargo test` to execute all Rust tests
- Tests are in `native/tests/`: `indexer_test.rs`, `graph_test.rs`, `search_test.rs`,
  `storage_test.rs`, `rust_indexer_test.rs`, `pdf_indexer_test.rs`, `storage_bench.rs`,
  and the `#[ignore]`-gated `neo4j_smoke.rs` / `neo4j_write_smoke.rs`
- **Run these tests after every code change in the native folder**
- If adding new functionality, add corresponding test cases to the test files
- Ensure all tests pass before completing a phase

### Verification Checklist
```
1. cd native && cargo test              → all tests must pass
2. cd native && cargo build --release   → native module must build
3. ./native/target/release/ug help      → CLI works
```

## 7. Documentation & Website

**Significant changes must be reflected in both the docs and the ug website.**

`docs/ug-website/` is the public-facing promotional portal for ug — what end users see first. It is deployed to Firebase and contains:

| Page | What it presents |
|------|-----------------|
| `index.html` | Slide-deck landing page: product intro, demo video, problem/solution, features, MCP integration, install/upgrade instructions |
| `api-reference.html` | Full multi-tab API reference: CLI commands, HTTP API routes, MCP tools, storage backends, pipeline & schemas (generated from `docs/API-REFERENCE.md`) |
| `architecture.html` | Architecture diagram and component overview |

When making significant changes (new commands, new API routes, new MCP tools, storage changes, pipeline changes), update both:
1. The relevant markdown docs in `docs/` (especially `docs/API-REFERENCE.md`)
2. The corresponding `docs/ug-website/*.html` pages

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
