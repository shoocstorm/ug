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
| **P0a** — overgraph 0.6 → 0.17 migration | ✅ done, verified, **uncommitted** | 473 tests pass; full `ug gen` + search/traverse verified |
| **P0b** — widen `node_props`, survive embed failure | ✅ done, verified, **uncommitted** | 492 tests pass; degraded + recovery paths exercised for real |
| **P0c** — `code_query` tool, presets, envelope | ⬜ next | |
| **P1** — comment/class metrics, file facts (reindex) | ⬜ not started | |
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
