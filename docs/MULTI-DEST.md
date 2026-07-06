# Multi-destination ingestion

UltraGraph can ingest the same knowledge graph into more than one
backend. Today two are supported:

- **OverGraph** (default) — file-based, embedded, no service required.
- **Neo4j** (5.13+) — connect via Bolt to an existing server. Optional
  GDS plugin enables native Personalized PageRank; without it, queries
  fall back to MMR automatically.

Backends sit behind a single `KnowledgeStore` trait, so adding a third
(LanceDB, Qdrant, Weaviate, …) is a matter of implementing one
interface — see `native/src/storage/store.rs`.

## Quick start

```bash
# OverGraph only (unchanged default)
ug ingest -i .ug/graph.json -o .ug/ugdb

# Neo4j only
ug ingest -i .ug/graph.json \
  --dest neo4j \
  --neo4j-uri neo4j://localhost:7687 \
  --neo4j-user neo4j \
  --neo4j-password $NEO4J_PASSWORD

# Fan-out: write to both in one pass
ug ingest -i .ug/graph.json \
  --dest overgraph,neo4j \
  -o .ug/ugdb \
  --neo4j-uri neo4j://localhost:7687 \
  --neo4j-user neo4j \
  --neo4j-password $NEO4J_PASSWORD

# Read from Neo4j
ug semantic_search "how does authentication work?" \
  --dest neo4j --neo4j-uri neo4j://localhost:7687 \
  --neo4j-user neo4j --neo4j-password $NEO4J_PASSWORD

# Same for hybrid_search and traverse
ug hybrid_search "loadConfig" --dest neo4j --neo4j-uri … --strategy ppr
ug traverse file:src/main.ts --dest neo4j --neo4j-uri …
```

## CLI flags

| Flag | Default | Notes |
|---|---|---|
| `--dest <kind[,kind...]>` | `overgraph` | Comma-separated list. Multi-dest is **ingest-only**; read commands accept exactly one. |
| `-o`, `--output <dir>` | `~/.ug/<name>/ugdb` | OverGraph data directory. Used as the OverGraph spec path. |
| `-d`, `--db <dir>` | (read commands) | Same as `-o` but typically used by reads. Honored as a fallback. |
| `--neo4j-uri <uri>` | — | `neo4j://host:port` or `bolt://host:port`. Required for `--dest neo4j`. |
| `--neo4j-user <user>` | `neo4j` | Bolt username. |
| `--neo4j-password <pw>` | — | Bolt password. Required for `--dest neo4j`. |
| `--neo4j-database <db>` | `neo4j` | Optional database name (Neo4j 4+ multi-database). |

## Environment variables

CLI flags take precedence; env vars provide defaults so the same
command works in `ug serve` and the MCP server without flag plumbing.

| Variable | Equivalent CLI flag |
|---|---|
| `UG_DEST` | `--dest` |
| `UG_DB_PATH` | `-o` / `-d` for OverGraph |
| `UG_NEO4J_URI` | `--neo4j-uri` |
| `UG_NEO4J_USER` | `--neo4j-user` |
| `UG_NEO4J_PASSWORD` | `--neo4j-password` |
| `UG_NEO4J_DATABASE` | `--neo4j-database` |

### `.env` file

The `ug` binary auto-loads a `.env` file from the current working
directory (or any parent) at startup, so you don't have to retype
connection params on every invocation. Copy `.env.example` to `.env`
and fill in the Neo4j fields:

```dotenv
# .env (gitignored)
UG_NEO4J_URI=neo4j://localhost:7687
UG_NEO4J_USER=neo4j
UG_NEO4J_PASSWORD=your-password
```

Then commands collapse to:

```bash
ug semantic_search "..." --dest neo4j
ug hybrid_search   "..." --dest neo4j
ug traverse <id>          --dest neo4j
ug ingest -i .ug/graph.json --dest overgraph,neo4j -o .ug/ugdb
```

For the MCP server (`node node/cli.mjs mcp`), Node 20+ supports the
same file natively:

```bash
node --env-file=.env node/cli.mjs mcp
```

Real environment variables always win over `.env` values, so CI /
deployment configs override safely.

## Capability matrix

| Capability | OverGraph | Neo4j (vanilla) | Neo4j + GDS | Neo4j + APOC |
|---|---|---|---|---|
| Vector search | ✅ HNSW | ✅ `db.index.vector` | ✅ | ✅ |
| Hybrid search (vector + keyword) | ✅ native | ✅ vector + full-text + RRF in app | ✅ | ✅ |
| K-hop traversal | ✅ | ✅ Cypher BFS | ✅ | ✅ |
| Personalized PageRank | ✅ native | ⚠️ falls back to MMR with a warning | ✅ via `gds.pageRank.stream` | ✅ |
| Type filter | ✅ | ✅ via secondary label | ✅ | ✅ |
| Sparse vectors | ✅ FNV-hashed tokens | ❌ N/A — emulated via full-text | ❌ | ❌ |
| APOC-accelerated upsert | n/a | ❌ falls back to per-type Cypher | n/a | ✅ `apoc.merge.relationship` |

`Neo4jStore::open` probes for both plugins on connect:

```cypher
SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'gds.' RETURN count(name)
SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'apoc.' RETURN count(name)
```

The probe result drives `supports_native_ppr()` and the choice of
upsert path. There is no error if either plugin is missing — only a
graceful degradation.

## Neo4j schema

Created (idempotently) on every `Neo4jStore::open`:

```cypher
CREATE CONSTRAINT ug_node_id_unique IF NOT EXISTS
  FOR (n:UgNode) REQUIRE n.id IS UNIQUE;

CREATE VECTOR INDEX ug_node_vec IF NOT EXISTS
  FOR (n:UgNode) ON (n.embedding)
  OPTIONS { indexConfig: {
    `vector.dimensions`: <embedding_dim>,
    `vector.similarity_function`: 'cosine'
  }};

CREATE FULLTEXT INDEX ug_node_text IF NOT EXISTS
  FOR (n:UgNode) ON EACH [n.name, n.description, n.node_text];
```

Each node carries the project's wire-format properties:

| Project field | Neo4j property |
|---|---|
| `id` | `n.id` (uniqueness-constrained) |
| `node_type` | dynamic label (`:Function`, `:Class`, …) plus `n.node_type` |
| `name`, `description`, `file`, `start_line`, `end_line`, `node_text`, `last_update_at` | properties on `:UgNode` |
| `vector: Vec<f32>` | `n.embedding` (`List<Float>`) |

Edges are typed relationships matching `GraphEdgeType` (`:Calls`,
`:Imports`, …) with `r.weight` baked from `default_edge_type_weights()`
at ingest. The weight is what drives Personalized PageRank's
structural bias on both backends.

The dim is persisted to a `:UgMeta { key: 'singleton' }` singleton node
the same way OverGraph uses `<db>/ug-meta.json` — opening with a
mismatched dim is rejected with `DimMismatch`.

## `ug serve` with multiple backends

`ug serve` follows the same `UG_DEST` convention as the ingest CLI —
list more than one backend and the visualization UI gets a
**destination selector** in the Semantic Search panel:

```bash
# Open the UI with both backends queryable
UG_DEST=overgraph,neo4j ug serve -i .ug/graph.json --db .ug/ugdb
```

- **Single backend** → a static pill labelled "overgraph · N nodes" /
  "neo4j · N nodes" appears above the search box, so the user always
  sees which DB they're hitting.
- **Multiple backends** → the pill becomes a dropdown. The first
  destination in `UG_DEST` is the default (marked `(default)` in the
  list). The selection is per-query — switch and re-run.
- Every search response now includes a `dest` field with the backend
  that actually served the result, surfaced in the result status line
  as "N results · 12 ms · from neo4j".

`/api/capabilities` reports the full set:

```json
{
  "db_ready": true,
  "search_ready": true,
  "db_node_count": 43,          // primary backend (back-compat)
  "primary": "overgraph",
  "destinations": [
    { "name": "overgraph", "primary": true,  "node_count": 43,  "supports_native_ppr": true  },
    { "name": "neo4j",     "primary": false, "node_count": 740, "supports_native_ppr": false }
  ]
}
```

Backends that failed to open at startup show up with an `error` field
on the destination row instead of `node_count`, so the operator can
see what's wrong without checking server logs. The UI lists them in a
disabled `unavailable` optgroup.

Per-request overrides go in the POST body / query string as `dest`:

```bash
curl -s -XPOST -H 'Content-Type: application/json' \
  -d '{"query":"loadConfig","k":5,"dest":"neo4j"}' \
  http://localhost:8080/api/search/semantic | jq '.dest, .count'

curl -s 'http://localhost:8080/api/db/traverse/file:src/main.ts?k=2&dest=neo4j' | jq '.dest'
```

## Fan-out semantics

`--dest overgraph,neo4j` opens both stores, validates that their
embedding dims agree (probed once from the embedder), embeds the graph
once, and fans the upserts out via `try_join_all`. **It is fail-fast**
— any backend's error aborts the whole ingest. No 2-phase commit; if
one backend ends up with partial data, re-run `ug ingest`. All
operations are idempotent (`MERGE` / OverGraph upsert).

Reads do NOT fan out. Each read picks one `--dest`. Comparing top-k
results across backends is on you (`diff <(jq -r '.items[].id' a) <(jq
-r '.items[].id' b)`).

## Personalized PageRank on Neo4j

When GDS is detected:

1. Resolve seed string ids → internal Neo4j node ids.
2. `CALL gds.graph.project($pname, 'UgNode', { types: $rel, properties: 'weight' })`.
3. `CALL gds.pageRank.stream($pname, { sourceNodes: $seeds, dampingFactor: $damp, maxIterations: $maxiter, relationshipWeightProperty: 'weight' })`.
4. `CALL gds.graph.drop($pname)`.

Per-call projection is wasteful but avoids cache-invalidation problems
across concurrent searches. If GDS isn't installed,
`personalized_pagerank` returns `StoreError::Unsupported` and
`query::search_kb` automatically falls back to MMR with one warning
log line per call.

## Known limitations

- **Direction filter on PPR** is a no-op on both backends today (matches
  the OverGraph behavior in `MIGRATION-OVERGRAPH §3.4`). The GDS
  projection uses `orientation: 'NATURAL'`.
- **Sparse vector parity.** Neo4j has no sparse vector type; hybrid
  recall on Neo4j may differ from OverGraph for queries dominated by
  rare identifier tokens. Acceptance bar in tests: ≥ 60% top-10 Jaccard.
- **APOC-free path performance.** Without APOC, `upsert_edges` runs one
  Cypher per distinct edge type (~10 round trips per batch instead of
  one). Install APOC if write throughput matters.
- **No fan-out on reads.** Each query targets exactly one backend.
- **No migration tooling.** To move data between backends, re-ingest
  from the original graph JSON.

## Testing against a local Neo4j

```bash
# 1. Start Neo4j (Community is fine; GDS optional)
docker run -d --name ug-neo4j-test -p 7687:7687 -p 7474:7474 \
  -e NEO4J_AUTH=neo4j/testpass \
  -e NEO4J_PLUGINS='["graph-data-science", "apoc"]' \
  neo4j:5.20

# 2. Run the smoke tests (use --test-threads=1 — schema setup races
#    when multiple suites open the same Neo4j concurrently)
cd native
cargo test --test neo4j_smoke -- --ignored --nocapture --test-threads=1
cargo test --test neo4j_write_smoke -- --ignored --nocapture --test-threads=1
```

Both suites are gated with `#[ignore]` so the default `cargo test` run
stays self-contained.
