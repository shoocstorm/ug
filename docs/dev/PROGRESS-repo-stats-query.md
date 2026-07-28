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
| **P0b** — widen `node_props`, survive embed failure | ⬜ next | |
| **P0c** — `code_query` tool, presets, envelope | ⬜ blocked on P0b | |
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
