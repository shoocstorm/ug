# Plan: Multi-destination ingestion (OverGraph + Neo4j)

## Context

Today UltraGraph ingests into a single, file-based graph DB — **OverGraph** — accessed through a hard-coded `Db` struct that wraps `overgraph::DatabaseEngine`. The full read/write surface (`upsert_nodes`, `upsert_edges`, `vector_search`, `hybrid_search`, `traverse`, `personalized_pagerank`) is OverGraph-specific. Every CLI/NAPI/server call site (`Db::open` × 6 in `main.rs`/`serve.rs`/`napi_bindings.rs`) opens an OverGraph instance directly.

The user wants UltraGraph to also push the same knowledge graph into **Neo4j** so it can sit alongside the rest of an organization's graph tooling (Bloom, GDS, Cypher dashboards, downstream apps). Neo4j 5.11+ has a native vector index, full-text indexes, mature traversal via Cypher, and (via the GDS plugin) Personalized PageRank — so feature parity is achievable without giving up OverGraph for users who like the local-first story.

The goal is a **pluggable storage layer**: refactor today's OverGraph code behind a `KnowledgeStore` trait, add a Neo4j implementation, and let the user pick one — or fan out to both — at ingest time. The wire formats (`NodeRow`, `EdgeRow`, `RankedContext`) and CLI/NAPI signatures stay the same so downstream tooling (TS bindings, MCP server, web UI) doesn't change.

## Goals

1. **Pluggable backends.** New backends drop in by implementing one trait.
2. **Neo4j as the second backend.** Feature parity for ingest, semantic, hybrid, traverse, and PPR (when GDS is detected at runtime).
3. **Fan-out ingest.** `ug ingest --dest overgraph,neo4j` writes to both in one pass; reads still pick one destination.
4. **Backwards-compatible defaults.** No `--dest` flag → OverGraph. Existing `.ug/ugdb/` directories keep working unchanged.

## Non-goals (deferred)

- Migration tooling between OverGraph ↔ Neo4j.
- Bidirectional sync / drift detection across destinations.
- Multi-destination fan-out on the **read** path (only ingest).
- Any change to the TypeScript layer, MCP server, or web UI — the NAPI surface stays wire-compatible.
- Sparse-vector hybrid on Neo4j (Neo4j has no sparse vector type; we emulate hybrid via vector + full-text + RRF instead).

## Architecture

### Trait shape

A single async trait owns the storage contract. `&dyn KnowledgeStore` flows through `query.rs`, `ingest.rs`, `ppr.rs`, `serve.rs`, and the NAPI bindings.

```rust
// native/src/storage/store.rs (new)
#[async_trait::async_trait]
pub trait KnowledgeStore: Send + Sync {
    fn embedding_dim(&self) -> u32;
    fn supports_native_ppr(&self) -> bool;

    async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<(), StoreError>;
    async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<(), StoreError>;

    async fn vector_search(
        &self,
        query: Vec<f32>,
        k: usize,
        filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError>;

    /// `query_text` is needed by Neo4j's full-text index;
    /// the OverGraph impl ignores it (uses the sparse vector instead).
    async fn hybrid_search(
        &self,
        query: Vec<f32>,
        sparse: Vec<(u32, f32)>,
        query_text: &str,
        k: usize,
        filter: Option<&NodeFilter>,
    ) -> Result<Vec<(NodeRow, f32)>, StoreError>;

    async fn traverse(
        &self,
        start: &str,
        max_hops: u32,
        edge_types: Option<&[String]>,
        direction: Direction,
    ) -> Result<TraversalPage, StoreError>;

    async fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<NodeRow>, StoreError>;
    async fn fetch_node(&self, key: &str) -> Result<Option<NodeRow>, StoreError>;
    async fn count_nodes(&self) -> Result<usize, StoreError>;

    /// Returns `Err(StoreError::Unsupported)` when the backend can't run native PPR
    /// (e.g. Neo4j without the GDS plugin). Callers should check `supports_native_ppr`
    /// first and fall back to MMR.
    async fn personalized_pagerank(
        &self,
        seeds: &[String],
        direction: Direction,
        edge_types: Option<&[String]>,
        restart_prob: f32,
        max_iter: usize,
        max_results: Option<usize>,
    ) -> Result<Vec<(String, f32)>, StoreError>;
}
```

`NodeFilter` is a small portable type filter (`Option<Vec<String>>` of allowed `node_type` values plus a future slot for property predicates). It replaces the current `where_clause: Option<&str>` plumbing, which was already a TODO no-op (see `db.rs:451`, `MIGRATION-OVERGRAPH §6 Q1`). Existing CLI `--filter "node_type = 'Function'"` strings get parsed once into `NodeFilter` so back-compat holds.

`StoreError` is a new error enum that subsumes the current `DbError` and adds Neo4j-specific variants (`Bolt`, `Auth`, `Unsupported`).

### Backend impls

- **`OvergraphStore`** — moves today's `Db` into `storage::backends::overgraph::OvergraphStore`. Implements `KnowledgeStore` by calling the existing OverGraph engine code. `supports_native_ppr() = true`.
- **`Neo4jStore`** — new `storage::backends::neo4j::Neo4jStore` using the [`neo4rs = "0.8"`](https://crates.io/crates/neo4rs) async Bolt driver. On connect, runs `CALL gds.list()` once; if it returns rows, sets `gds_available = true` and `supports_native_ppr() = true`; otherwise `false` and PPR routes through the MMR fallback in `query::search_kb`.

### Storage selection

`StoreSpec` is the parsed form of a destination (kind + per-kind config), constructed from CLI flags or env vars. `open_store(spec)` builds the right `Box<dyn KnowledgeStore>`.

```rust
pub enum StoreSpec {
    Overgraph { path: PathBuf, embedding_dim: u32 },
    Neo4j { uri: String, user: String, password: String, database: Option<String>, embedding_dim: u32 },
}

pub async fn open_store(spec: &StoreSpec) -> Result<Box<dyn KnowledgeStore>, StoreError>;
```

### Multi-destination fan-out (ingest only)

`StoreSet` wraps `Vec<Box<dyn KnowledgeStore>>` and exposes ingest-only methods (`upsert_nodes`, `upsert_edges`) that fan out via `futures::future::try_join_all`. Reads stay single-destination — `ug rag`, `ug semantic_search`, `ug traverse` take exactly one `--dest`.

Embedding dim must agree across destinations in a fan-out (probed once, validated against each store). If the user wants different dims per store, they re-run ingest separately.

Failure semantics: if any destination errors during fan-out, the whole ingest fails fast; the user re-runs after fixing connectivity. No partial-rollback; users are expected to re-run ingest, which is idempotent (upsert by key).

## Neo4j schema

| Project concept | Neo4j mapping |
|---|---|
| Node id (`"function:src/foo.ts:1:foo"`) | `:UgNode { id: "..." }` (constraint: `UNIQUE`) plus a label per `node_type` (`:Function`, `:Class`, …) for fast type filtering |
| Node `name` / `description` / `file` / `start_line` / `end_line` / `last_update_at` / `node_text` | properties on `:UgNode` |
| Node `vector: Vec<f32>` | `embedding: List<Float>` property; indexed via `db.index.vector.createNodeIndex('ug_node_vec', 'UgNode', 'embedding', $dim, 'cosine')` (one-time, idempotent on connect) |
| Edge `source` / `target` / `edge_type` | `(a:UgNode {id:$src})-[r:`<EdgeType>`]->(b:UgNode {id:$tgt})` with `r.weight = $weight` baked from `default_edge_type_weights()` |
| Full-text channel for hybrid search | `db.index.fulltext.createNodeIndex('ug_node_text', ['UgNode'], ['name','description','node_text'])` (one-time, idempotent) |

Indexes are created inside `Neo4jStore::open` with `IF NOT EXISTS` semantics (Neo4j 5.x). Embedding dim is read from / written to Neo4j as a `:UgMeta { embedding_dim: $dim }` singleton node — same role as `ug-meta.json` for OverGraph.

### Operation mapping

| Trait method | Neo4j Cypher (sketch) |
|---|---|
| `upsert_nodes` | `UNWIND $rows AS r MERGE (n:UgNode {id: r.id}) SET n += r.props, n.embedding = r.vector, n:` + dynamic label apoc-style (or `CALL apoc.create.addLabels`); fall back to per-type batches if APOC is absent |
| `upsert_edges` | `UNWIND $rows AS r MATCH (a:UgNode {id:r.src}), (b:UgNode {id:r.tgt}) CALL apoc.merge.relationship(a, r.edge_type, {}, {weight:r.weight}, b) YIELD rel RETURN count(*)`; APOC-free fallback uses one Cypher per distinct edge type |
| `vector_search` | `CALL db.index.vector.queryNodes('ug_node_vec', $k, $vec) YIELD node, score RETURN node, score` |
| `hybrid_search` | Two queries (vector + `db.index.fulltext.queryNodes('ug_node_text', $text, {limit:$k})`); merge with the same RRF helper used by OverGraph (extract `query::reciprocal_rank_fusion` to a shared util) |
| `traverse` | `MATCH p = (s:UgNode {id:$start})-[r:` + `\|`-joined edge types + `*1..$hops]->(n) RETURN n, length(p) AS depth, [rel IN r \| {src:startNode(rel).id, tgt:endNode(rel).id, type:type(rel)}] AS edges`; direction toggles `->` / `<-` / `-` |
| `personalized_pagerank` | `CALL gds.pageRank.stream('ug-graph', { sourceNodes:$seedIds, dampingFactor:$damping, maxIterations:$maxIter, relationshipWeightProperty:'weight' }) YIELD nodeId, score`; graph projected lazily on first call via `gds.graph.project.cypher` and cached |

**APOC is optional.** If `CALL apoc.help('apoc')` fails on connect, `Neo4jStore` flips a flag and uses the slower Cypher-only fallback for dynamic label / dynamic relationship type. We log it once; behavior is identical, throughput drops ~2–3×.

## File-by-file changes

| Path | Action | Why |
|---|---|---|
| `native/Cargo.toml` | + `neo4rs = "0.8"`, + `async-trait = "0.1"` | Neo4j Bolt driver + trait async sugar |
| `native/src/storage/store.rs` | **New** — defines `KnowledgeStore` trait, `NodeFilter`, `TraversalPage`, `StoreError`, `StoreSpec`, `open_store`, `StoreSet` | The abstraction itself |
| `native/src/storage/backends/mod.rs` | **New** — `pub mod overgraph; pub mod neo4j;` | Backend module root |
| `native/src/storage/backends/overgraph.rs` | **Move** today's `db.rs` body here as `OvergraphStore`; implement `KnowledgeStore`. Free functions (`vector_search`, `hybrid_search`, `traverse_string_ids`, `nodes_by_ids`) become trait methods | Adapt existing OverGraph code to the trait |
| `native/src/storage/backends/neo4j.rs` | **New** — `Neo4jStore` with the schema setup, GDS/APOC capability detection, and the Cypher in the table above | The Neo4j impl |
| `native/src/storage/db.rs` | **Slim down** to re-export `NodeRow`, `EdgeRow`, `DbError` (kept as alias to `StoreError` for back-compat). The `Db` type becomes a `pub type Db = dyn KnowledgeStore;` alias plus `Db::open` / `Db::open_or_create` shims that build an `OvergraphStore` (so `Db::open` keeps working). | Preserve external API while routing through the trait |
| `native/src/storage/ingest.rs` | Take `&dyn KnowledgeStore` instead of `&Db`; add `ingest_graph_multi(stores: &StoreSet, …)` | Fan-out path |
| `native/src/storage/query.rs` | Take `&dyn KnowledgeStore`. In `search_kb`, when `strategy == Ppr` and `!store.supports_native_ppr()`, log once and fall back to `search_kb_mmr` automatically | Trait through-routing + graceful PPR degradation |
| `native/src/storage/ppr.rs` | Generalize `run_ppr` to take `&dyn KnowledgeStore`; OverGraph-specific code moves to `backends/overgraph.rs` | The wrapper now dispatches via the trait |
| `native/src/storage/text.rs` | Extract `reciprocal_rank_fusion(left: Vec<(NodeRow, f32)>, right: Vec<(NodeRow, f32)>, k: usize, c: f32) -> Vec<(NodeRow, f32)>` (Neo4j hybrid uses it) | Shared RRF helper |
| `native/src/storage/napi_bindings.rs` | Parse `dest` field from options JSON → `StoreSpec` → `open_store`; route through trait | NAPI surface gains optional `dest` field |
| `native/src/storage/mod.rs` | Re-export `KnowledgeStore`, `StoreSpec`, `StoreSet`, `open_store`, `StoreError` | Crate-level visibility |
| `native/src/main.rs` | Add `--dest`, `--neo4j-uri`, `--neo4j-user`, `--neo4j-password`, `--neo4j-database` flag parsing; thread through `run_ingest`, `run_semantic_search`, `run_hybrid_search` (`run_rag`), `run_traverse`, `run_gen_ingest`. `--dest` takes a comma-separated list **only on `ingest`/`gen`**; reads accept exactly one. | CLI surface |
| `native/src/serve.rs` | Replace `Db::open` with `open_store(spec)` resolved from env (`UG_DEST`, `UG_NEO4J_*` mirroring MCP env conventions) | Web server picks the right backend |
| `node/cli.cjs` | Surface the new flags in `--help`; pass-through unchanged otherwise (it just shells out to the binary or NAPI) | CLI ergonomics |
| `node/mcp-server.mjs` | Add `UG_DEST` / `UG_NEO4J_*` env handling; pass into the JSON `embedderOptions` (rename to `storeOptions` or add a sibling `destOptions` field) | MCP server picks backend |
| `native/tests/store_trait_test.rs` | **New** — runs the same fixture (50 nodes, 100 edges, fixed query set) against both backends, asserts identical NodeRow shape returned and similar top-k overlap (≥80% Jaccard for vector search) | Cross-backend correctness |
| `native/tests/storage_test.rs` | Existing OverGraph tests stay; rewrite them to construct an `OvergraphStore` via the trait | Confirms refactor didn't regress |
| `docs/MULTI-DEST.md` | **New** — user-facing doc covering the `--dest` flag, Neo4j setup, GDS/APOC capability matrix, fan-out semantics, env vars | The deliverable docs file |
| `docs/GRAPH-STORAGE.md` | Add a top section: "OverGraph is one of two supported backends; see `MULTI-DEST.md` for Neo4j" | Cross-link without duplicating |
| `CHANGELOG.md` | Note the new `--dest` flag, Neo4j support, and that defaults are unchanged | Release notes |

## Configuration & CLI surface

```bash
# OverGraph (default — unchanged)
ug ingest -i .ug/graph.json -o .ug/ugdb

# Neo4j (single dest)
ug ingest -i .ug/graph.json \
  --dest neo4j \
  --neo4j-uri bolt://localhost:7687 \
  --neo4j-user neo4j \
  --neo4j-password $NEO4J_PASSWORD

# Fan-out: write to both
ug ingest -i .ug/graph.json \
  --dest overgraph,neo4j \
  -o .ug/ugdb \
  --neo4j-uri bolt://localhost:7687 \
  --neo4j-user neo4j \
  --neo4j-password $NEO4J_PASSWORD

# Read from Neo4j
ug rag --dest neo4j --neo4j-uri … "how does authentication work?"
```

Env-var equivalents (used by `serve` and MCP, also accepted by CLI as fallbacks): `UG_DEST`, `UG_DB_PATH`, `UG_NEO4J_URI`, `UG_NEO4J_USER`, `UG_NEO4J_PASSWORD`, `UG_NEO4J_DATABASE`.

## Phased implementation

Each phase ends with a green build + the listed verification command. Don't merge phases; if one regresses, stop and fix before moving on.

1. **Phase 1 — Trait extraction (no behavior change).** Add `store.rs` with the trait. Move today's `db.rs` body into `backends/overgraph.rs` and implement `KnowledgeStore` for it. Slim `db.rs` to re-exports + `Db::open` shim. Route `query.rs`, `ingest.rs`, `ppr.rs`, `napi_bindings.rs`, `serve.rs` through `&dyn KnowledgeStore`. Verify: `cargo test -p ultragraph` (all 68 existing tests pass), `ug gen -i ./native/src -o /tmp/ug-trait` produces a working OverGraph DB and `ug rag` returns results.
2. **Phase 2 — Neo4j read-only impl + capability probes.** Add `neo4rs` dep. Implement `Neo4jStore::open` (connect, probe APOC + GDS, ensure indexes). Implement `vector_search`, `hybrid_search`, `traverse`, `nodes_by_ids`, `fetch_node`, `count_nodes`. PPR returns `StoreError::Unsupported` when GDS missing. Verify: `cargo test -p ultragraph neo4j_smoke` against a Docker-launched Neo4j fixture (`docker run -d -p 7687:7687 -e NEO4J_AUTH=neo4j/test neo4j:5.20`).
3. **Phase 3 — Neo4j write path.** Implement `upsert_nodes`, `upsert_edges` (APOC + APOC-free paths). Wire `--dest neo4j` into `main.rs::run_ingest`. Verify: `ug ingest --dest neo4j …` against the fixture; `MATCH (n:UgNode) RETURN count(n)` matches input node count; `ug semantic_search --dest neo4j` returns hits.
4. **Phase 4 — Fan-out + StoreSet.** Implement `StoreSet`, parse comma-separated `--dest`, fan out in `ingest_graph_multi`. Verify: `ug ingest --dest overgraph,neo4j …`; both backends report identical `count_nodes`; `ug rag --dest overgraph` and `ug rag --dest neo4j` against the same query produce overlapping top-10 (≥6 shared ids).
5. **Phase 5 — PPR via GDS.** Implement `Neo4jStore::personalized_pagerank` using `gds.pageRank.stream` with cached graph projection. Verify: when GDS is present, `ug rag --strategy ppr --dest neo4j` returns ranked results; when GDS is absent (run against `neo4j:5.20-community` *without* installing GDS), the same call falls back to MMR with a single warning log line.
6. **Phase 6 — Docs + serve/MCP env wiring.** Write `docs/MULTI-DEST.md`, update `GRAPH-STORAGE.md`, `CHANGELOG.md`, `node/mcp-server.mjs` env handling, `serve.rs` env handling. Verify: MCP `search_kb` works against Neo4j with only env vars set; web UI `npm start` against Neo4j.

## Verification (end-to-end, run after Phase 4)

```bash
# 0. Build
cd native && cargo build --release && cd ..

# 1. Index a small repo
npm run gen -- -i ./native/src -o .ug --no-ingest

# 2. Spin up Neo4j (separate terminal, or skip if you have one running)
docker run -d --name ug-neo4j-test -p 7687:7687 -p 7474:7474 \
  -e NEO4J_AUTH=neo4j/testpass \
  -e NEO4J_PLUGINS='["graph-data-science", "apoc"]' \
  neo4j:5.20

# 3. Fan-out ingest
ug ingest -i .ug/graph.json -o .ug/ugdb \
  --dest overgraph,neo4j \
  --neo4j-uri bolt://localhost:7687 \
  --neo4j-user neo4j --neo4j-password testpass

# 4. Read from each backend, expect overlap
ug rag --dest overgraph "tree-sitter parser"  > /tmp/og.json
ug rag --dest neo4j     "tree-sitter parser"  > /tmp/n4.json
diff <(jq -r '.items[].id' /tmp/og.json | sort) \
     <(jq -r '.items[].id' /tmp/n4.json | sort)
# >= 60% Jaccard on the top-10 ids is the acceptance bar

# 5. Cross-backend trait test
cd native && cargo test --test store_trait_test -- --nocapture
```

## Critical files to read before implementation

- `native/src/storage/db.rs` — the file being split / abstracted.
- `native/src/storage/query.rs` — the read-side composition (`search_kb`, `rrf_search`, `traverse_filtered`); their `&Db` parameters become `&dyn KnowledgeStore`.
- `native/src/storage/ppr.rs` — the PPR wrapper; the trait method maps onto its existing signature 1:1.
- `native/src/storage/napi_bindings.rs` — every NAPI entry point opens a DB; all six must route through `open_store`.
- `native/src/main.rs:780` (`run_ingest`) and `native/src/main.rs:835` (`run_semantic_search`) — the CLI entry points that need new flags.
- `native/src/serve.rs:286` — the web server's single `Db::open` site.
- `docs/MIGRATION-OVERGRAPH.md` §6 Q1 — context on why `where_clause` is currently a no-op (we replace it with `NodeFilter` in this refactor).
- `docs/REQUIREMENT.md` Phase 4 — the requirements PPR was built against (informs the Neo4j PPR equivalence bar).

## Known risks & open follow-ups

- **GDS licensing.** GDS Community Edition supports `pageRank.stream` with personalization since v2.0; Aura DS / Enterprise have it too. We assume the user installs GDS via `NEO4J_PLUGINS='["graph-data-science"]'` (it's a one-line install). If only GDS Enterprise features are needed later (large graphs, parallel writes), document it in `MULTI-DEST.md`.
- **Direction parameter on PPR.** OverGraph's wrapper currently *ignores* direction (see `ppr.rs:83`). Neo4j's `gds.pageRank.stream` walks the projected graph's edges as defined at projection time — we project with `orientation: 'NATURAL'` by default and document that direction filter on the PPR strategy is a no-op on both backends. Tracked as a follow-up matching `MIGRATION-OVERGRAPH §3.4 v1.1`.
- **Sparse vector parity.** Neo4j has no sparse vector type. Hybrid recall on Neo4j may differ from OverGraph for queries dominated by rare identifier tokens. Acceptance bar in §Verification step 4 (≥60% Jaccard) accounts for this. If recall regresses worse than that, the next step is a SPLADE-style sparse embedding layer (deferred).
- **APOC-free path performance.** Without APOC the `upsert_edges` path runs one Cypher per distinct edge type (~10 round trips per batch instead of one). Measure during Phase 3; if it's painful, document the APOC requirement in `MULTI-DEST.md` rather than optimizing.
- **Fan-out partial failure.** If one backend errors mid-ingest, the other may have partial data. Idempotent re-ingest is the recovery path. We don't ship a transactional 2PC.
