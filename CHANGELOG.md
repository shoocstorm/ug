# Changelog

## Unreleased — branch `migrate/overgraph`

### ⚠️ Breaking: storage backend changed from LanceDB to OverGraph

The `ug-out/ugdb/` directory format is **not forward-compatible**. Existing databases must be deleted and re-ingested:

```bash
rm -rf ug-out/ugdb
ug ingest -g ug-out/graph.json -d ug-out/ugdb
```

Opening an old LanceDB-formatted directory with the new build will fail at OverGraph's manifest parser — the error is unambiguous.

The CLI surface, NAPI surface, and JSON wire formats are unchanged. The only user-visible behavior change at the command level is that `ug ingest --with-indexes` is now a no-op (OverGraph builds indexes per segment automatically) and prints a deprecation note.

See `docs/MIGRATION-OVERGRAPH.md` for the full migration plan, run log, and open-question resolutions.

### Changed
- `native/src/storage/db.rs`: rewritten against `overgraph` 0.6.0. `Db` now wraps `DatabaseEngine` with a project-string-id ↔ OverGraph-u64 cache.
- `native/src/storage/ppr.rs`: 445 LOC → 116 LOC. Now a thin wrapper around `engine.personalized_pagerank` (uniform seed mass — see migration doc §3.4).
- `native/src/storage/query.rs::rrf_search`: replaced manual fusion with native `VectorSearchMode::Hybrid` + `FusionMode::ReciprocalRankFusion`.
- `native/src/storage/query.rs::traverse_filtered`: replaced BFS loop with `engine.traverse` per seed + merge.
- `native/src/storage/text.rs`: added `build_sparse_keyword_vector` (FNV-1a hashed tokens) replacing LanceDB BM25 FTS for the keyword channel.

### Added
- `native/src/storage/types_registry.rs`: stable u32 IDs for node and edge types. **Once assigned an ID must never change** (persisted on disk in OverGraph segments).
- `native/tests/storage_bench.rs`: ignored micro-benchmarks for ingest + hybrid search latency.

### Removed
- `lancedb`, `lance-index`, `arrow`, `arrow-array`, `arrow-schema` dependencies.
- The build-time `protoc` requirement (came in via `lance-encoding`).
- In-process Personalized PageRank power-iteration loop (now native to OverGraph).
- Manual Reciprocal Rank Fusion in `query::rrf_search`.

### Performance (dev profile, ARM64 M-series)
- Ingest 1K nodes + 5K edges: 64.8ms.
- Hybrid search p50/p95: 5.5ms / 5.7ms over 100 queries.

### Known issues
- Release-mode build of the `ug` binary fails to link due to LTO + napi cdylib interaction with the new dep tree. The lib and dev-profile bin compile cleanly. Fix likely needs `lto = false` or `codegen-units = 1` in the release profile — tracked as a follow-up.
- The `where_clause` parameter on `db::vector_search` / `db::hybrid_search` is currently ignored (the LanceDB SQL surface is gone; OverGraph's `type_filter` is more limited). Tracked as `MIGRATION-OVERGRAPH §6 Q1`.
- PPR seeds are uniform — the previous weighted personalization vector based on RRF scores is deferred. Tracked as `MIGRATION-OVERGRAPH §6 Q2`.
