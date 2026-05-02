# Knowledge Graph Storage with OverGraph (Rust)

> **Migration note (2026-05-01):** Storage moved from LanceDB to **OverGraph** v0.6.0. See `MIGRATION-OVERGRAPH.md` for the rationale, API mapping, and §6 trade-offs. 

## Objective
Implement a knowledge graph storage system in Rust using OverGraph for persistence and local embedding generation. Use explicit embeddings.

## Tech Stack (Rust)

· `overgraph` crate v0.6.0 — pure Rust embedded graph DB with HNSW dense vectors, sparse posting-list indexes, native Personalized PageRank, and BFS traversal.
· Embedding endpoint:
```Local embedding model settings for testing:
Model: openai/Qwen3-Embedding-0.6B-4bit-DWQ
Base URL: http://localhost:8000/v1
API Key: 1234
```
· **Note**: LanceDB is huge (caused build time 1min -> 8min, causes binary size to be 20MB -> 300MB). 
· **Note**: OverGraph fixed the build time and binary size issue. 
· `serde`, `tokio`, `reqwest` (for HTTP embeddings).

## Data Model

OverGraph keys nodes by `(type_id: u32, key: String)` and edges by `(from_id, to_id, type_id: u32)`. The project's wire-format DTOs (`NodeRow`, `EdgeRow`) preserve the prior LanceDB-era shape so callers don't need to change. Translation between the two lives in `native/src/storage/db.rs`. Type-id constants are in `native/src/storage/types_registry.rs` and are **stable forever** (they're persisted on disk in OverGraph segments).

### Nodes (OverGraph `NodeRecord`)

| Project field | OverGraph location |
|---|---|
| `id: String` (e.g. `"function:src/foo.ts:1:foo"`) | `key: String` |
| `node_type: String` (`"Function"`, `"Class"`, …) | `type_id: u32` via `types_registry::node_type_to_id` |
| `name`, `description`, `file`, `node_text` | `props["name" \| "description" \| "file" \| "node_text"]` (all `PropValue::String`) |
| `start_line`, `end_line` | `props["start_line" \| "end_line"]` (`PropValue::UInt`) |
| `last_update_at: i64` | `props["last_update_at"]` (`PropValue::Int`) — also reflected in `NodeRecord.updated_at` (auto) |
| `vector: Vec<f32>` (1024-dim) | `dense_vector: Option<DenseVector>` |
| _(new)_ sparse keyword vector | `sparse_vector: Option<SparseVector>` — built at query time, see "Sparse keyword vectors" below |

### Edges (OverGraph `EdgeRecord`)

| Project field | OverGraph location |
|---|---|
| `id: String` (synthesized as `"<source>\|<edge_type>\|<target>"`) | not persisted — OverGraph allocates its own `u64` id |
| `source: String` | `from: u64` (resolved via `Db::lookup_id`) |
| `target: String` | `to: u64` |
| `edge_type: String` (`"Calls"`, `"Imports"`, …) | `type_id: u32` via `types_registry::edge_type_to_id` |
| `weight: f32` (new) | `weight: f32` — **baked at ingest** from `default_edge_type_weights` in `ppr.rs` |
| `properties: String` (JSON) | not used in v1 |

**Why the edge weight matters:** OverGraph PPR has no per-edge-type weight knob, only an `edge_type_filter`. So we encode the structural bias (Calls=1.0, Imports=0.7, Contains=0.3, …) into each edge's *weight* at upsert time. The `default_edge_type_weights()` table in `native/src/storage/ppr.rs` is the source of truth.

### Embedding Generation (Explicit)

1. Load embedding endpoint config once.
2. For each node, build `node_text = "{type}: {name}. {description}. Related: {list_of_related_names}"`.
   - For Folder nodes the `{name}` slot uses the full path (from `folder:<path>`), not the basename.
   - `{description}` priority: `folder.summary` → `docstring` → synthesized folder synopsis → empty.
3. Batch-encode: `texts → Vec<Vec<f32>>` (1024-dim).
4. Store vectors via `Db::upsert_nodes` → OverGraph `batch_upsert_nodes`.
5. Incremental: `reembed_nodes` re-runs steps 2–4 for a subset of ids.

### Sparse keyword vectors (replaces LanceDB FTS)

OverGraph has no built-in BM25. To preserve the keyword-search half of `rrf_search`, the project ships a deterministic tokenizer in `text::build_sparse_keyword_vector`:

- Lowercase alphanum tokens, length 2–32 chars.
- 32-bit FNV-1a hash of each token → dimension id.
- Term frequency as the weight.
- Sorted ascending by dimension id for OverGraph's canonicalization.

The same hash + tokenizer run at ingest (per node `node_text`) and at query time, so tokens collide deterministically. No IDF — fine for distinctive identifier queries; weaker for description-heavy queries. Upgrade path is SPLADE/BGE-M3 sparse embeddings; deferred to v2.

## OverGraph Setup

```rust
let opts = DbOptions {
    dense_vector: Some(DenseVectorConfig {
        dimension: 1024,
        metric: DenseMetric::Cosine,
        hnsw: HnswConfig::default(),  // m=16, ef_construction=200
    }),
    ..Default::default()
};
let engine = DatabaseEngine::open(path, &opts)?;
```

- Single dense vector space per DB (1024-dim, locked at open).
- HNSW indexes are built **per segment at flush time** automatically. 
- WAL mode: `WalSyncMode::GroupCommit` (default — 50ms fsync timer, ~20× write throughput vs. immediate).

## Query Functions (Rust async)

1. **Semantic Search** — `db::vector_search(db, query_vec, k, where_clause)`. Pure dense ANN over the HNSW index. `where_clause` parameter is currently ignored (see `MIGRATION-OVERGRAPH §6 Q1`); use OverGraph's `type_filter` directly when needed.

2. **Hybrid Search** — `db::hybrid_search(db, dense_vec, sparse_vec, k, where_clause)`. Native OverGraph `VectorSearchMode::Hybrid` with `FusionMode::ReciprocalRankFusion`. Replaces the manual RRF in the previous `query::rrf_search`.

3. **Graph Traversal** — `db::traverse_string_ids(db, start, max_hops, edge_type_ids, direction)`. Wraps OverGraph's `engine.traverse` and rehydrates string ids + edge records.

4. **Personalized PageRank** — `ppr::run_ppr(db, seeds, direction, edge_types, restart_prob, max_iter, max_results)`. Wraps OverGraph's native `engine.personalized_pagerank`. v1 ships with **uniform seed mass** (no per-seed weighting); the previous weighted-personalization-vector behavior is deferred (see `MIGRATION-OVERGRAPH §3.4`).

## Implementation Checklist

- [x] Add dependency: `overgraph = "0.6"`
- [x] Define `NodeRow` / `EdgeRow` DTOs (preserved from LanceDB era for wire-format stability)
- [x] Implement `types_registry` (string ↔ u32 mapping with stable IDs)
- [x] Implement `Db::open` with `DenseVectorConfig` (1024-dim cosine)
- [x] Implement `key_to_id` / `id_to_key` caches (project string id ↔ OverGraph u64)
- [x] Implement `build_node_text` (unchanged) + `build_sparse_keyword_vector` (new)
- [x] Implement `upsert_nodes` / `upsert_edges` (with edge-weight baking)
- [x] Implement `vector_search`, `hybrid_search`, `edges_from`, `edges_to`, `traverse_string_ids`, `nodes_by_ids`
- [x] Implement `run_ppr` (thin wrapper around `engine.personalized_pagerank`)
- [x] Storage smoke tests in `native/tests/storage_test.rs`

## Testing Criteria

- [x] Insert nodes, verify `vector_search` returns the seed first when querying with the same vector.
- [x] Verify `hybrid_search` combines dense + sparse via OverGraph's RRF.
- [x] Verify two-hop traversal returns reachable nodes with correct distances.
- [x] Verify `run_ppr` ranks the seed neighborhood above unconnected nodes.
- [x] Confirm vector dimension = 1024 (`EMBEDDING_DIM`).

## Performance (dev profile, ARM64 M-series, see `MIGRATION-OVERGRAPH §10`)

- Ingest 1K nodes + 5K edges: 64.8ms (target <2s).
- Hybrid search p50/p95: 5.5ms / 5.7ms over 100 queries (target <100ms).

## Deliverables

- Rust crate `ultragraph-kb` with `storage` module.
- Existing `ug` binary commands (`ug ingest`, `ug semantic_search`, `ug hybrid_search`, `ug traverse`) — unchanged signatures.
- NAPI surface (`db_ingest`, `db_semantic_search`, `db_hybrid_search`, `db_traverse`, `ping_embedder`) — unchanged JSON wire format.
