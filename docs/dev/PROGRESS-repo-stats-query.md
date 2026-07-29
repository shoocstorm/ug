# Progress: repo-stats query feature

Live status tracker for the work specified in
[`DESIGN-repo-stats-query.md`](DESIGN-repo-stats-query.md). This work spans
multiple sessions — **read this file first, update it as you go.**

Reference for the storage engine's API: see `Agents.md` §8 (never read the
7.7k-line `../overgraph/docs/api-reference.md` whole; use its ToC).

---

## Status

| Phase | State | Notes |
|-------|-------|-------|
| **P0a** — overgraph 0.6 → 0.17 migration | ✅ done, committed `ca7b9ac` | 473 tests pass; full `ug gen` + search/traverse verified |
| **P0b** — widen `node_props`, survive embed failure | ✅ done, committed `5e182a6` | 492 tests pass; degraded + recovery paths exercised for real |
| **P0c** — `code_query` tool, presets, envelope | ✅ done, verified, **uncommitted** | 529 tests pass; 25 presets run against the live index and all three transports driven end to end |
| **P1** — comment/class metrics, file facts (reindex) | ✅ done, verified, **uncommitted** | 554 tests pass; full `ug gen` reindex, then all 33 presets run against it |
| **P2** — preset files, Insights viz pane | ⬜ not started | |
| **P3** — CSV/Parquet export | ⬜ not started | |

---

## P0a — overgraph 0.17 migration ✅

All six steps done. Baseline was 14 compile errors, all in
`native/src/storage/db.rs`; final change touched five files.

- [x] 1. `native/Cargo.toml` → `overgraph = "0.17"`
- [x] 2. `NodeRecord`/`EdgeRecord` → `NodeView`/`EdgeView`
- [x] 3. `type_id: u32` → `label: &str` throughout;
       `types_registry.rs` rewritten from a u32 map into a **label
       canonicalizer** (`node_label`, `edge_label`, `ALL_NODE_LABELS`).
       It could not be deleted outright: edge-type filters still arrive
       from users in arbitrary case and the engine matches labels
       exactly, and the prune/lookup sweeps still need a label inventory.
- [x] 4. `get_nodes`, `.edge_label_filter`, `label_filter`,
       `NeighborEntry.label`, `NodeKeyQuery`. `count_nodes` now uses
       `count_nodes_by_labels` — precise instead of the old approximation.
- [x] 5. `STORE_FORMAT_VERSION = 2` in `DbMeta`
- [x] 6. Verified: 473 tests pass (21 neo4j-gated ignored) ·
       `cargo build --release` · `ug help` · full `ug gen` rebuilt
       2235 nodes / 5193 edges · `semantic_search`, `search`, and
       `traverse -e contains` all return correct results

### Two things the verification caught

Both would have shipped broken without running the real binary:

1. **The guard was unrecoverable.** `ug gen` hit the same store-format
   rejection as every read path, so the error told users to run the
   command that was itself blocked. Fixed with
   `db::reset_if_stale_format` + `store::reset_stale_format_stores`,
   called only from `ingest_with_specs` — ingest is the one caller
   entitled to delete a store, because it is about to replace it. The
   deletion is deliberately narrow: manifest present **and** recording a
   different format. A missing manifest never deletes anything.
2. **The guard panicked.** An expected post-upgrade state was surfacing
   as a crash with a backtrace notice. `open_store_or_exit` in `main.rs`
   now exits cleanly for this one error and keeps panicking for the rest.

Also worth knowing: an earlier version of `check_store_format` treated
"data on disk, no manifest" as a v1 store. That broke two `bm25_test`
reopen tests, because `Db::open` never writes a manifest — so a store
created through it looked ancient on the second open. The check is now
keyed on the manifest alone.

---

## P0b — widen `node_props` + survive embedding failure ✅

New module `native/src/storage/facts.rs` derives per-node facts once at
ingest; they are written as **plain sibling properties** (`n.loc`, not
`n.f_loc`) so GQL reads the way someone would write it by hand.

Facts today: `loc` · `params` · `max_nesting` · `has_doc` · `folder` ·
`is_test` · `in_degree` · `out_degree` · `qualified_name` · `route` ·
`annotations`. Booleans are stored as **0/1 integers** — GQL has no boolean
aggregate, so `sum(has_doc)/count(*)` is the only way to ask "what fraction
is documented".

- [x] `NodeRow.facts`, round-tripped via `RESERVED_PROPS` (anything not a
      fixed column is a fact). A fact can never shadow a column.
- [x] `loc` falls back to the line span when `metrics` is absent — this is
      what gives Class/Interface nodes a size at all, since the extractors
      only compute metrics for functions.
- [x] Property indexes (`ensure_query_indexes`, no-op default on the trait
      so Neo4j is unaffected): equality on `node_type`/`is_test`/`folder`/
      `has_doc`, range on `loc`/`in_degree`/`out_degree`/`params`/
      `max_nesting`. Best-effort — a missing index costs speed, never
      correctness.
- [x] Embedding failure degrades instead of aborting: rows are written with
      **no** dense vector (`dense_vector: None`), so the node stays out of
      the HNSW index while every property stays queryable. A wrong-*width*
      vector is still an error.
- [x] `record_ingest_model` is skipped on a degraded run — the stamp claims
      "these vectors are current for this model", which would be a lie about
      rows that have none, and would stop the next run re-embedding them.
- [x] Both progress paths in `main.rs` (single- and multi-destination) and
      both `ingest.rs` paths degrade identically.
- [x] `IngestOutcome` replaces the `(nodes, edges)` tuple so a degraded run
      is neither reported as success nor as failure.

### Two subtleties worth not re-deriving

1. **`stored_row_matches` must compare facts.** `in_degree` is not a
   property of its own node — it moves when some *other* file starts
   calling it. Without the comparison, an incremental re-ingest would
   freeze degrees at first-ingest values and every derived statistic would
   drift further from the truth on each run, while the node looked current.
2. **Test seeds must compute real facts.** `seed()` in
   `incremental_ingest_test.rs` and `stored()` in `ingest.rs` simulate "what
   a previous ingest wrote". Leaving their facts empty made every node look
   changed and silently turned nine "nothing was rewritten" assertions into
   tests of the wrong thing. The suite caught this.

### Verified

492 tests pass (21 neo4j-gated ignored), 19 new. New `tests/facts_test.rs`
covers round-trip, column shadowing, persistence without a vector, wrong-width
rejection, backfill, and index idempotency.

End to end on this repo: a second `ug gen` reports **2279 unchanged, 0 to
embed** — proof the facts round-trip byte-identically, since any mismatch
would rewrite every node. Degradation exercised against a dead endpoint
(`--base-url http://127.0.0.1:9`): 6 nodes + 6 edges indexed without vectors,
honest summary, then a normal `ug gen` backfilled all 6 and semantic search
returned correct hits.

---

## P0c — `code_query` ✅

New module `native/src/code_query/` (`mod.rs` · `presets.rs` · `render.rs`),
plus `KnowledgeStore::execute_query` and its OverGraph implementation. One
implementation, three transports, matching `agent_tools`.

- [x] **Store trait.** Portable `QueryValue` / `QueryPage` / `QueryLimits` /
      `QueryParams` in `store.rs`; `execute_query` defaults to
      `Unsupported` so Neo4j says so rather than answering approximately.
      OverGraph lowers `GqlValue` → `QueryValue`, flattening nodes to their
      **string key** (a statistics answer wants an id it can feed to
      `get_code`, not a hydrated node carrying `code`).
- [x] **Options pinned, not inherited.** `mode: ReadOnly`,
      `allow_full_scan: true`, and every cap set explicitly — a cap the
      caller never chose is one it cannot warn about.
- [x] **25 built-in presets** across census / size / documentation / dead
      code / architecture / tests / risk. Parameters are **bound as GQL
      params**, never interpolated.
- [x] **The envelope**: table (or a bare count for a 1×1 result),
      leftover-id samples, `rows_matched` as the denominator, percentiles
      computed from a `collect()` column, cap-truncation notice, engine
      warnings, and the coverage line. Capped at 3k chars.
- [x] **`graph_schema` is now the capability manifest** — the store half
      (property coverage + preset list) is appended in `mcp/mod.rs`, so
      `agent_tools::graph_schema` stays graph-only and the call still
      succeeds when there is no usable store.
- [x] **No embedder.** `db::stored_embedding_dim` reads the dim off the
      sidecar, so statistics never start an embedding backend. Both the MCP
      and CLI paths use it.
- [x] Surfaces: MCP `code_query` · `ug query` (+ `--list`) · `POST
      /api/tools/code_query` · `GET /api/presets`.
- [x] Docs: `docs/mcp.md` (§10, §11 rewritten), `docs/api-reference.md`
      (CLI 1.4, HTTP 2.6, tools 3.1, trait 4.3), README, and the `ug-mcp`
      SKILL.md — source **and** the installed `~/.claude/skills/` copy.

### What running the queries changed

Five things were wrong in the design doc or in the first implementation, and
every one of them was found by executing against the real index, not by
reading:

1. **`count(DISTINCT elementKey(dep))`, not `count(*)`.** A variable-length
   match yields one row per *path*. `impact` on `storage/store.rs` reported
   **948** dependents with a plain count and **11** with the distinct one.
   Design Risk 4 named this; it is far bigger than "dedupe by node id"
   suggests.
2. **`EXISTS { MATCH … WHERE … }` needs its own `RETURN` inside.** The
   design's example is a parse error.
3. **Caps do not only truncate — `max_frontier` *errors*.** The design says
   caps truncate silently, which is true of `max_rows` but not of the
   traversal frontier. `untested_symbols` at `*1..3` exceeded it outright on
   this 2280-node repo; at `*1..2` it answers in ~180ms. Both behaviours are
   handled: truncation warns, and the frontier error gets a message saying
   how to narrow the walk.
4. **`NOT x IN [...]` must be parenthesised** as `NOT (x IN [...])`, or the
   engine rejects the operands.
5. **Percentiles do have to be computed in the render layer.** `percentileCont`
   parses (it is in the aggregate-name list) but does not lower — so it fails
   at execution, not at parse. The design's conclusion was right; its reason
   was not.

### The silent zero, demonstrated

`MATCH (n:Function) WHERE n.comment_lines > 3 RETURN count(*)` returns
`Int(0)` — no error, no engine warning. That is the whole reason the
coverage contract is non-optional, and it is now covered by a test that
asserts on the *rendered* output, not just the coverage struct.

### Two judgement calls worth knowing

- **`UnknownEdgeLabel` is suppressed for presets and shown for raw GQL.**
  ug's presets name every dependency edge type deliberately and no single
  language emits all of them, so `Overrides` is legitimately absent from a
  Rust graph. In a query the caller wrote, the same warning almost always
  means a typo'd label matching nothing. The full-scan notice is suppressed
  always — it is true of every statistical query by construction, and
  echoing it would train readers to skip the line where the real warnings
  live.
- **`code_query` does not join the `tool_graph` arm**, contrary to design
  Part D. That arm is DB-free, and aggregation needs stored properties.
  It has its own dispatch arm that opens the store without an embedder,
  which preserves the useful half of the property: it keeps working when
  `search` does not.

---

## P1 — the fact layer ✅

The design's question 1 — *"how many methods & classes have comments?"* —
is now answerable, and the answer is more interesting than a single number:

```
kind       total  commented  with_doc_comment
Function    1597        828               499
Class        211        101                25
```

329 functions carry prose but no doc comment. Collapsing those two into one
"documented" figure would have hidden the finding, which is why
`has_comments` and `has_doc` are separate properties rather than one.

- [x] **A2 — line metrics**, in `indexer/line_metrics.rs`, computed once
      per file in `process_file` rather than in the five extractors.
      Adds `comment_lines` · `doc_lines` · `code_lines`.
- [x] **A3 — metrics on types.** Handled centrally by the same pass:
      any symbol with no `metrics` gets one, `loc` filled from its span.
      That covers Class and Interface without touching four extractors,
      and covers any node type added later for free.
- [x] **A4 — `language` and `classification` on `GraphNode`**, stamped
      onto every symbol in a file rather than just the File node, so
      "group by language" is a scan and not a join. `is_test` now prefers
      the classifier and keeps the path heuristic as fallback.
- [x] **A5 — `graph_schema_version` in `IndexStats`** (`GRAPH_SCHEMA_VERSION
      = 2`), plus `INDEXER_VERSION` 2 → 3 to discard the content cache.
- [x] 8 new presets: `comment_coverage` · `comment_density` · `token_docs` ·
      `undercommented_complexity` · `language_breakdown` · `file_kinds` ·
      `long_functions_by_code` · `classes_by_members`. 33 total.

### The version gate, which is the whole point of A5

`ug` upgrades in place, so the ordinary state right after an upgrade is a
**current binary reading an old index**. Every symbol in that index has
`comment_lines: 0` — not because it has no comments, but because
`#[serde(default)]` filled it in. Storing that would answer "how well
commented is this repo" with "not at all".

So `facts::compute` writes the line metrics **only** when the graph is
stamped v2 or later. Verified against the bundled v1 Java graph, where
`comment_coverage` renders:

```
kind       total  commented  with_doc_comment
Function     552  —                        12
⚠ NOT INDEXED: has_comments — … Run `ug reindex`
```

`sum()` over an absent property returns null, which renders as `—` rather
than `0`. Between that and the warning, there is no reading of this output
that produces a wrong number.

### Two judgement calls

- **`loc` is now inclusive everywhere.** The extractors computed
  `end - start` while the span fallback computed `end - start + 1`, so
  `loc` meant subtly different things for a Function and a Class and the
  two were never comparable. Both are inclusive now. Existing numbers shift
  by one; that is the fix, not a regression.
- **`members` is absent, not zero, where the language does not nest.**
  Rust `impl` blocks sit outside the struct, so a Rust type has no
  `Contains` edges — the bundled Java sample has 451, this repo has none.
  Writing `members: 0` would rank every Rust type as memberless against
  Java types that genuinely declare members. Omitted, coverage reports
  `members 71/749 (9%)` on Java and `NOT INDEXED` here, and
  `classes_by_members` says so in its own description.

### Verified

554 tests pass (21 neo4j-gated ignored), 20 new. Beyond the suite: a full
`ug gen` of this repo (2445 nodes / 5732 edges) with every new property
landing — `code_lines`/`comment_lines`/`doc_lines`/`has_comments` at
2337/2445, `language` 2431/2445, `classification` 433/2445 — then all **33**
presets run against that store, none failing. `members` verified separately
on the Java sample (`BotConfig` 71).

`classification` covers only 18% here: the classifier was written for
JS/TS project shapes and has little to say about a Rust tree. That is
honest rather than broken — `file_kinds` reports its own 18% coverage — but
it means the classification-based half of A4 is mostly untested against
data on this repo.

---

## Decisions already settled (do not relitigate)

- **No bespoke JSON query DSL.** Use OverGraph GQL — it ships aggregation,
  `CASE`, `EXISTS {}`, `UNION`, and bounded variable-length paths.
- **One new MCP tool** (`code_query`), ever. Capability grows through presets
  (GQL strings) and stored properties, discovered at runtime via `graph_schema`.
- **Ingest must write nodes/properties even when embedding fails**, so stats
  survive a missing embedder.
- Presets run with `allow_full_scan: true` (stats are full scans by nature and
  the default is `false`) and `mode: ReadOnly` (makes repo-supplied presets
  safe by construction).
- Responses carry **coverage** (property population) **and cap warnings**
  (`caps`/`warnings` off the result) — caps truncate silently, so a blast
  radius can be an under-report that looks precise.

---

## Log

- **2026-07-28** — Design doc finalised after correcting an earlier wrong
  conclusion (GQL *does* exist and *does* aggregate; I had checked pinned 0.6.0
  and a stale local checkout). P0a implemented and verified end to end.
  **Not yet committed** — the tree is on `main`, so this wants a branch.
  Files touched: `native/Cargo.toml`, `Cargo.lock`, `src/main.rs`,
  `src/storage/{db,store,ingest,types_registry}.rs`, plus `Agents.md` §8
  (OverGraph API reference pointer) and these two `docs/dev/` files.
  Note the working tree also carries unrelated staged changes
  (`.github/workflows/ci.yml`) that are **not** part of this work — commit
  P0a with explicit paths, not `git add -A`.
- **2026-07-28** — P0a committed as `ca7b9ac`. P0b implemented and verified;
  uncommitted. New files: `native/src/storage/facts.rs`,
  `native/tests/facts_test.rs`. Modified: `storage/{db,store,ingest,mod}.rs`,
  `storage/backends/neo4j.rs`, `src/main.rs`, and five test files that build
  `NodeRow` literals.
- **2026-07-29** — P0b committed as `5e182a6`. **P0c implemented and
  verified; uncommitted.** 529 tests pass (21 neo4j-gated ignored), 37 new.
  New files: `native/src/code_query/{mod,presets,render}.rs`,
  `native/tests/code_query_test.rs`. Modified: `storage/{store,db}.rs`,
  `src/lib.rs`, `src/agent_tools.rs` (Render helpers → `pub(crate)`),
  `src/main.rs`, `src/serve.rs`, `src/mcp/{mod,tools,ug-mcp-skill}.rs|md`,
  `docs/mcp.md`, `docs/api-reference.md`, `README.md`.
  Verified beyond the suite: all 25 presets run against this repo's live
  index (`~/.ug/UG/ugdb`, 2280 nodes); `code_query` and `graph_schema`
  driven through the real stdio MCP server; `ug query` run from the release
  binary; `/api/presets` and `POST /api/tools/code_query` curled against a
  live `ug serve`.
  Java facts verified too: `docs/samples/java.graph.json` ingested into a
  temp store with a dead embedder (which re-exercised P0b's degraded path —
  749 nodes, 2728 edges, no vectors). `qualified_name` 643/749,
  `annotations` 107/749 (`Override` 94 · `Test` 12 · `FunctionalInterface`
  1), `route` correctly `NOT INDEXED` — that sample has no HTTP routes.
  One bug found and fixed by that run: `-o` was missing from `ug query`'s
  positional-skip list, so `ug query <preset> -o <path>` read the path as a
  second preset and failed with "preset and gql together", which points
  nowhere near the cause. Argument parsing is now a testable function with
  a regression test (main.rs had no test module before this).
  **Still to do before release:** the website slide deck — the one doc
  surface from the design's P0 list not yet touched.
- **2026-07-29** — **P1 implemented and verified; uncommitted.** 554 tests
  pass, 20 new. New file: `native/src/indexer/line_metrics.rs`. Modified:
  `src/types.rs` (SymbolMetrics + GraphNode + IndexStats),
  `src/indexer.rs`, all four language extractors (`..Default::default()`
  and the inclusive `loc`), `src/graph.rs`, `src/storage/facts.rs`,
  `src/code_query/{mod,presets}.rs`, `tests/code_query_test.rs`,
  `docs/mcp.md`, and the `ug-mcp` skill (source + installed copy).
  Note for whoever commits: `INDEXER_VERSION` 2 → 3 means the first
  `ug gen` after this lands re-indexes every file rather than hitting the
  content cache. That is intended — without it the new metrics would appear
  only on files someone happened to edit, and every repo-wide statistic
  would be computed over an arbitrary subset.
  Test project `~/.ug/UGTEST` was created for verification and can be
  deleted (`ug rm UGTEST`).
