# Design: whole-repo statistical queries for AI coding agents

Status: proposal · 2026-07-28

## The problem

An agent asked *"how many methods are longer than 50 lines?"* has two bad
options today:

1. **Grep + read.** `rg` for function keywords, then read every file to count
   lines. On this repo that is ~80 files / 40k lines ≈ 500k tokens and several
   minutes. On a real monorepo it is impossible.
2. **Loop over ug tools.** `file_outline` per file, 80 calls, each returning
   prose. Cheaper than reading source, still ~40k tokens and 80 round-trips.

Neither is acceptable, and both are *unnecessary*: `ug` already distilled the
repo into `graph.json` — 1891 nodes / 4374 edges / 674 KB for this project —
and `mcp/mod.rs:293` keeps it hot as an `Arc<GraphData>`, mtime-invalidated. A
full linear scan over that is microseconds. The answer to "how many methods are
longer than 50 lines" is a `filter().count()` away and costs **~30 tokens** to
return.

So the gap is not compute. It is three specific things:

| Gap | Symptom |
| --- | --- |
| **No aggregate query surface** | Every tool in `mcp/tools.rs:201` returns *rows* (symbol lists). None returns *counts, groups, or distributions*. |
| **Missing facts** | `metrics` is populated on **Function nodes only** (1162/1891 here; 385/552 in the Java sample). Classes have no `loc`. Nothing records comment density. File `language`/`classification` are computed by `indexer/classifier.rs` but never reach `GraphNode`. |
| **No reachability rollup** | `find_usages` gives 1-hop neighbours as a list. "What is the impact of changing file A" needs transitive reverse-reachability *summarised*, not dumped. |

Three sample questions, mapped:

| Question | Answerable today? |
| --- | --- |
| How many methods & classes have comments? | ❌ `docstring` covers doc-comments only (770/1891); inline comments are extracted at `storage/comments.rs:44` but only fed to the embedder, never counted. Class nodes carry no metrics. |
| How many methods longer than 50 lines? | ⚠️ Data exists (`metrics.loc`, or `endLine - startLine`) — but there is no way to ask. |
| If I modify file A, what is the impact? | ⚠️ Edges exist (`Calls` 1928, `References` 466, `Imports` 27, `Extends`, `Implements`, `Overrides`) — but no transitive rollup, and no way to say "excluding tests". |

---

## Design decision: what query surface?

| Option | Verdict |
| --- | --- |
| **(a) Embed DuckDB/SQLite** — project the graph into tables, agent writes SQL | Powerful and familiar, but ~50 MB of binary bloat on a tool that ships as a small static binary, and SQL is *worst* at the one question that matters most here: transitive reachability needs recursive CTEs, which LLMs write badly. |
| **(b) ISO-GQL via `overgraph::execute_gql`** | **Shipped in 0.17, and it covers this use case** — aggregation, implicit grouping, HAVING, variable-length paths, a planner and `explain`. See below. |
| **(c) Bespoke JSON DSL** — `from`/`where`/`group_by`/`aggregate` | ~1200 LOC of evaluator to reimplement, worse, what (b) already ships. Retain only the *idea* of a validated capability manifest. |
| **(d) Fixed canned tools** (`comment_stats`, `long_methods`, …) | ~20 tokens per call, zero flexibility, unbounded tool-list growth. |

**Recommendation: (b) as the query language, (d) as named presets over it, (a)
as an export escape hatch. Do not build (c).**

> **Correction (2026-07-28).** An earlier revision of this document argued that
> OverGraph's GQL did not exist and could not aggregate. That was wrong on both
> counts. It was based on the **pinned** version (`overgraph = "0.6"`,
> `native/Cargo.toml:40`) and on the local checkout at
> `~/Documents/project/overgraph`, which is still v0.6.0 and whose
> `docs/roadmap.md:44` lists GQL under "Next". **Upstream is at v0.17.0 and GQL
> shipped, with aggregation.** Everything below is the corrected analysis, and
> it changes the recommendation.

### What OverGraph 0.17 actually ships

Verified against the downloaded crate, not the roadmap:

| Capability | Evidence |
| --- | --- |
| `execute_gql(query, params, options)` + `explain_gql` | `engine/query.rs:111` |
| Queries **and** mutations **and** schema/index DDL | `GqlStatementBody::{Query,Mutation,Schema,Index}` — not read-only |
| Aggregates `count · sum · avg · min · max · collect` | `types.rs:1989` |
| Implicit Cypher-style grouping | `RETURN n.kind AS k, count(*) AS c` — `engine/tests/gql_execution.rs:13308` |
| HAVING, via `WITH … WHERE` | `WITH n.kind AS k, count(*) AS c WHERE c > 1` — `:13427` |
| `ORDER BY` on an aggregate | `ORDER BY count(*) DESC, k ASC` — `:13443` |
| Variable-length paths `*1..3` | `:5964`; parser tests `gql/parser.rs:3890` |
| `shortestPath` / `allShortestPaths` | `:6081`, `:6150` |
| Group-cardinality cap | `max_groups` in `GqlExecutionOptions` |

That is the entire feature set this design needs — including the one thing I
claimed a graph language could not do (aggregate) and the one thing SQL cannot
do cleanly (variable-length reachability, i.e. impact analysis). Building a
bespoke evaluator against this would mean reimplementing a planner-backed query
engine that is already a dependency.

### What still has to be built — and it is not a query language

Two of the original four objections survive the version correction, and both
are **ug-side gaps GQL cannot close for us**:

**1. The facts are not in the database.**
`node_props` (`storage/db.rs:462`) persists exactly ten properties:

```
name · node_type · description · file · start_line · end_line
last_update_at · node_text · code · file_hash
```

No `metrics` — so **no `loc`, no `params`, no `max_nesting`**. No `docstring`,
no `annotations`, no `qualified_name`, no `route`. `MATCH (f:Function) WHERE
f.loc > 50 RETURN count(*)` returns zero — not because the query is wrong, but
because the property was dropped at ingest. **Part A is therefore a hard
prerequisite for the GQL path**, where it was merely an enhancement for an
in-memory one: `graph.json` already carries these facts, the store does not.

**2. Two different stores.** GQL runs against the ugdb LSM store. `graph.json`
is a separate file, and today it is the one with full fidelity. "GQL over the
graph json data" is not a thing that can be done — the fix is to widen ingest so
the store carries what the JSON carries. That widening pays for itself
independently: `semantic_search`'s advertised `whereClause` (`mcp/tools.rs:228`)
is near-useless today because `NodeFilter` (`storage/store.rs:55`) supports
node-type filtering and nothing else.

And one genuine trade-off the version correction does not erase: `tool_graph`
(`mcp/mod.rs:420`) is deliberately DB-free so structural tools "keep working
when `search` does not", and `reindex`'s own contract says they stay fresh
**even if embedding fails**. Projects with a good `graph.json` and a missing or
stale `ugdb` are a real population, not a hypothetical. Moving statistics onto
GQL means they break in exactly that degraded mode — see Risk 10.

### The upgrade cost, measured

Bumping `overgraph = "0.6"` → `"0.17"` and running `cargo check --lib`:
**14 errors, every one in `src/storage/db.rs`.** No other file in the crate
touches the engine API. The breakage is one coherent rename:

| 0.6 | 0.17 |
| --- | --- |
| `NodeRecord` / `EdgeRecord` | `NodeView` / `EdgeView` |
| `type_id: u32` on `NodeInput` / `EdgeInput` | `label: &str` |
| `get_node_by_key(type_id, key)` | `get_node_by_key(label, key)` |
| `nodes_by_type` / `get_nodes_by_type` | `get_nodes` |
| `TraverseOptions.edge_type_filter` | `.edge_label_filter` |
| `PprOptions.edge_type_filter` | `.edge_label_filter` |
| `VectorSearchRequest.type_filter` | renamed |
| `NeighborEntry.edge_type_id` | label-based |

Note the direction: labels are **strings** now, so
`storage/types_registry.rs`'s numeric `node_type_to_id` / `edge_type_to_id`
mapping largely stops being necessary. The migration is a net simplification of
ug's storage layer, not just a port — a contained, mechanical half-day.

*(The bump was applied, checked, and reverted; the working tree is unchanged.)*

### The revised shape

Drop the bespoke DSL. Keep everything in this document that is **not** a query
language — none of it comes free with GQL, and it is where the actual product
value sits:

- **The preset registry** (Part E) — presets now hold **GQL strings**, which is
  strictly better than nested JSON predicates: readable in `.ug/presets.toml`,
  reviewable in a PR, writable by anyone who knows Cypher.
- **The response envelope** (Part B) — GQL returns rows; the entire token
  argument is about rendering aggregates + samples + **coverage** + staleness
  into a compact answer.
- **The capability manifest** (`graph_schema`) — now advertising *stored
  properties with coverage* plus available presets, so an agent writing GQL
  knows which properties exist, and how populated they are, before it queries.
- **The viz Insights pane** (Part G) — unchanged.
- **One MCP tool** (Part D) — `code_query`, taking either `preset` or raw `gql`.

The agent-ergonomics argument I made for a JSON IR over a query string was real
but is now clearly outweighed: one round-trip of parse-error debugging is worth
avoiding a hand-maintained query engine. It is also largely recoverable —
presets cover the common path at ~20 tokens, `explain_gql` gives structured
diagnostics, and the manifest prevents the most common failure, which is
querying a property that was never stored.

---

## Part A — Fact layer

Make the questions *answerable* before making them *askable*.

### A1. Derived columns — no reindex, ships immediately

Computed at query time from what is already in `graph.json`:

- `loc` → `metrics.loc` when present, else `end_line - start_line + 1`. This
  alone gives Class/Interface nodes a size, unblocking question 2 for every
  node type on **existing** indexes.
- `in_degree` / `out_degree` / `called_by` / `calls_out` — computed once per
  graph load and cached next to the `Arc<GraphData>` in `mcp/mod.rs:293`. This
  is what makes `where called_by = 0` (dead-code sweep) a scan instead of a
  join.
- `folder` → parent dir of `file`.
- `has_doc` → `docstring.is_some()`.

### A2. Comment metrics — requires reindex

Add to `SymbolMetrics` (`types.rs:135`):

```rust
pub comment_lines: u32,   // prose comment lines inside the symbol span
pub doc_lines: u32,       // lines in the leading doc comment
pub code_lines: u32,      // span minus blank and comment lines
```

Computed in **one** place — the shared indexer path (`indexer/common.rs`) —
using the existing scanner at `storage/comments.rs:44`, which is already
regex-free and handles `//`, `/* */` and `#`. Doing it there rather than in
each of the five language extractors means all languages gain it at once and
the definition of "a comment" cannot drift between them.

`code_lines` matters: `end_line - start_line` counts blank lines and the
signature, so "longer than 50 lines" measured on span overstates. Report both;
document which is which.

### A3. Metrics on Class / Interface

Each language extractor computes `SymbolMetrics` inline in its *function*
branch (`rust.rs:166`, `typescript.rs:252`, `java.rs:615`, `python.rs:137`).
Extend the class/interface branch with `loc`, `members`, `max_nesting`. Without
this, "how many classes …" silently answers about nothing.

### A4. File facts onto `GraphNode`

`FileClassification` (`types.rs:158` — Test, Config, Service, Component, …) and
`language` are computed by `indexer/classifier.rs` and **dropped** before the
graph is written. Add them to `GraphNode`. This is what unlocks:

- `is_test` — indispensable for impact analysis ("47 callers, 41 of them tests")
- per-language breakdowns
- excluding generated/vendor code from every statistic

### A5. Graph version stamp

Bump a `schema_version` in `graph.json`'s `stats` block. A graph written before
A2 has no comment data; querying `comment_lines` against it must return
**"not indexed — run `ug reindex`"**, never `0`. See risk 1 below.

---

## Part B — The query engine

New module `native/src/query_engine/` (`ast.rs`, `columns.rs`, `eval.rs`,
`render.rs`), operating in-memory on `&GraphData`, following the existing
one-implementation-three-transports pattern documented at `agent_tools.rs:1`.

### Query shape — GQL, not a bespoke DSL

The three questions in the brief, written in what OverGraph 0.17 already
executes. Nothing here needs an engine ug has to write:

```cypher
-- Q1: how many methods and classes have comments?  (needs Part A)
MATCH (n)
WHERE n.node_type IN ['Function', 'Class']
RETURN n.node_type          AS kind,
       count(*)             AS total,
       sum(n.has_comments)  AS documented
ORDER BY kind
```

```cypher
-- Q2: how many methods are longer than 50 lines, and where?
MATCH (n:Function)
WHERE n.loc > 50 AND n.is_test = false
WITH n.folder AS folder, count(*) AS c, avg(n.loc) AS avg_loc
WHERE c >= 3                       -- HAVING
RETURN folder, c, avg_loc
ORDER BY c DESC
LIMIT 20
```

```cypher
-- Q3: if I modify src/auth.ts, what is the impact?
MATCH p = (dependent)-[:Calls|References|Imports|Extends|Implements*1..3]->(target)
WHERE target.file = 'src/auth.ts'
RETURN dependent.file  AS file,
       dependent.is_test AS test,
       count(*)        AS refs
ORDER BY refs DESC
```

Variable-length paths make Q3 a single statement — the capability SQL would
need a recursive CTE for, and the reason a graph language is the right pick.

Variable-length paths make Q3 a single statement — the capability SQL would
need a recursive CTE for, and the reason a graph language is the right pick.

#### Verified against the full API reference

Read from `../overgraph/docs/api-reference.md` §GQL (line 3584). Three things
I had assumed were missing are in fact supported, which *simplifies* the plan:

| Assumed missing | Actually available |
| --- | --- |
| Bucketing helper | **`CASE`** — both generic (`CASE WHEN n.loc > 100 THEN …`) and simple form. Histograms need no ingest-time materialisation. |
| `ratio` | `sum(CASE WHEN n.has_comments THEN 1 ELSE 0 END) / count(*)`. Storing booleans as 0/1 is now an optimisation, not a requirement. |
| Set algebra (`except`) | **`EXISTS { … }` subqueries** plus `UNION` / `UNION ALL`. "Symbols no test reaches" is `WHERE NOT EXISTS { MATCH (t)-[:Calls*1..3]->(n) WHERE t.is_test }` — one statement, no render-layer differencing. |

Also available and worth using: `OPTIONAL MATCH`, `WITH DISTINCT`, `CALL { … }`,
string predicates (`STARTS WITH` / `ENDS WITH` / `CONTAINS`), scalar functions
(`coalesce`, `lower`, `trim`, `toString`, `toInteger`), arithmetic, and metadata
functions — `elementKey(n)` is ug's string node id, `labels(n)` the node type.

Genuinely still missing, and ug must supply it:

| Missing | Consequence |
| --- | --- |
| `p50`/`p90`/`p99` | Only `count · sum · avg · min · max · collect`. Compute percentiles in the render layer from `collect(n.loc)`, bounded by `max_collect_items` (65 536). |
| `UNWIND`, `FOREACH` | Not needed here. Noted so presets do not reach for them. |
| Unbounded `*` paths | Every variable-length path needs a finite upper bound ≤ `max_path_hops` (16). Impact analysis must always say `*1..N`. Paths are **relationship-simple** (no edge reused), which conveniently makes cycles safe. |

#### Two operational gotchas that would break every preset

Both from §Parameters and Options (line 4325), and neither is obvious:

**1. `allow_full_scan` defaults to `false`.** A statistics query is a full scan
by nature — "how many functions exceed 50 lines" has no bounded anchor. Every
preset ug ships must set `allow_full_scan: true`, or it fails at planning. This
single default would otherwise make the entire feature appear broken.

**2. Caps truncate; they do not error.** `max_groups` 65 536, `max_frontier`
65 536, `max_rows` 10 000, `max_paths_per_start` 4 096, `max_intermediate_
bindings` 65 536. On a large monorepo an impact query from a hot file can hit
`max_frontier` and return a **silently under-reported blast radius**.

That second point deserves promotion, because it is the same failure this design
already guards against from a different direction. The coverage contract was
about *properties that were never stored*; this is about *rows that were dropped
mid-execution*. Both produce a confident wrong number. So the response envelope
must carry **both**, read from `GqlExecutionResult`'s `caps` and `warnings`
fields (§Explain, Profile, and Stats, line 4611):

```
coverage: loc 1191/1191 · is_test 1878/1891 · comment_lines NOT INDEXED
⚠ traversal hit max_frontier (65536) — blast radius is a LOWER BOUND
```

#### Two capabilities worth exploiting

- **`CREATE PROPERTY INDEX` via GQL DDL.** ug can declare indexes on the hot
  statistic properties (`node_type`, `loc`, `file`, `is_test`) at the end of
  ingest, so presets hit indexes instead of scans.
- **`mode: ReadOnly`.** Rejects mutation statements before write staging. Running
  every preset in ReadOnly mode makes **Risk 8 disappear by construction** — a
  repo-supplied `.ug/presets.toml` from a cloned repository cannot mutate the
  store no matter what it contains. This is a much stronger guarantee than the
  "bounded evaluator" argument it replaces.

### Columns

Published through an extended `graph_schema` (already an MCP tool,
`mcp/tools.rs:318`) so discovery costs one call:

`id · name · node_type · qualified_name · file · folder · language ·
classification · is_test · is_generated · start_line · end_line · loc ·
code_lines · comment_lines · doc_lines · has_doc · has_comments · params ·
max_nesting · in_degree · out_degree · calls_out · called_by · annotations ·
route · extends · implements`

Unknown field → error listing valid columns with a Levenshtein suggestion. This
is the payoff over free-text SQL: a malformed query fails at validation with a
useful message instead of returning a plausible wrong number.

### Response shape — the actual token win

```
Functions with loc > 50, excluding tests — 143 of 1162 (12.3%)

by folder                        count   avg loc   p90
  native/src/indexer/languages      31      88      210
  native/src/storage                27      74      166
  …                                                     (13 more groups)

samples: function:native/src/main.rs:36:main, function:…
coverage: loc 1191/1191 · is_test 1878/1891 · comment_lines NOT INDEXED (run `ug reindex`)
⚠ Index may be stale: 3 changed files
```

Under 200 tokens for a question that costs 500k by grep. Defaults: `limit` 20,
output hard-capped ~3k chars, `truncated` + `total_groups` always reported.

The **`coverage` line is part of the contract, not a nicety.** A statistic
computed over a field populated for 60% of nodes is a confidently wrong answer,
and an agent has no way to know. Every response states denominators.

### Presets

An agent composing a DSL query spends ~300 reasoning tokens. A preset costs
~20. `code_query {"preset": "comment_coverage"}` expands server-side into the
full query. Starter set:

`comment_coverage` · `undocumented_public` · `long_functions` ·
`complexity_hotspots` · `param_bloat` · `god_classes` · `dead_code` ·
`orphan_files` · `dependency_fanin` · `fanout_offenders` · `coupling_matrix` ·
`layering_violations` · `test_ratio` · `untested_symbols` · `api_surface` ·
`untested_routes` · `annotation_census` · `size_histogram` · `impact` ·
`retest_scope` · `risky_symbols` · `where_to_start`

Presets are **data, not code** (Part E) — the list grows without a release, and
`graph_schema` advertises whatever is loaded. Note the naming: `code_query`, not
`code_stats`, because once impact analysis, dead-code sweeps and coupling
matrices ride the same engine, "stats" undersells it.

### B.4 — What this actually covers

The point of a general engine is that the three questions in the brief are a
thin slice. Everything below is one `code_query` call. **P1** marks queries that
need the Part A reindex; everything else runs against today's `graph.json`.

**Size, shape and complexity**

| Question | Query |
| --- | --- |
| How many methods are longer than 50 lines? | `where loc > 50`, `count` |
| Show me the length distribution | `group_by bucket(loc,[0,20,50,100,200])` |
| Which classes are god classes? | `node_type=Class`, `order_by loc desc` **P1** |
| Functions with more than 6 parameters? | `where params > 6` |
| Where is the deeply nested code? | `where max_nesting >= 4` |
| Average file size by language | `from files`, `group_by language`, `avg loc` |

**Documentation**

| Question | Query |
| --- | --- |
| How many methods & classes have comments? | `group_by node_type`, `ratio(has_comments)` **P1** |
| Which folders are worst documented? | `group_by folder`, `ratio(has_doc)`, `order_by asc` |
| Public API symbols with no docs | `where is_exported && !has_doc` |
| Docs that are token one-liners | `where doc_lines = 1` **P1** |
| Comment-to-code ratio by module | `group_by path_segment(file,1)`, `sum(comment_lines)/sum(code_lines)` **P1** |

**Dead code and waste**

| Question | Query |
| --- | --- |
| What is never called? | `where called_by = 0 && !is_test && !is_exported` |
| Which files does nothing import? | `from files`, `where in_degree = 0` |
| Exported symbols nobody uses | `where is_exported && called_by = 0` |
| Duplicate-looking names across modules | `group_by name`, `having count > 1` |

**Coupling and architecture** — mostly `from: "edges"`

| Question | Query |
| --- | --- |
| Module coupling matrix | `from edges`, `group_by [source.folder, target.folder]` |
| Does the UI layer reach the DB layer directly? | `from edges`, `where source.folder~'ui/' && target.folder~'db/'` |
| What is most depended upon? | `order_by in_degree desc` |
| Which functions touch everything? | `where out_degree > 20` |
| Files importing from >5 distinct folders | `group_by file`, `count_distinct target.folder`, `having > 5` |
| Cross-language boundaries | `from edges`, `where source.language != target.language` |

**Test posture**

| Question | Query |
| --- | --- |
| Test-to-source ratio per folder | `group_by folder`, `ratio(is_test)` |
| Which source symbols no test reaches? | `except`: all symbols − reachable-from tests |
| Folders with no tests at all | `group_by folder`, `having count(is_test)=0` |
| Which endpoints are untested? | `Route` nodes `except` reachable-from tests |

**API surface**

| Question | Query |
| --- | --- |
| List every HTTP route by method | `node_type=Route`, `group_by regex_extract(route,'^\w+')` |
| How big is the public surface per module? | `where is_exported`, `group_by folder` |
| How many `@Deprecated` / `@Transactional`? | `group_by annotations`, `count` |
| Which controllers are fattest? | `where annotations contains 'RestController'`, `order_by out_degree` |

**Change risk** — the reachability side

| Question | Query |
| --- | --- |
| If I modify file A, what breaks? | `from reachable_from(A, inbound)` |
| What must I re-test? | same, `where is_test` |
| Does this change touch an HTTP endpoint? | same, `where node_type = Route` |
| What is dangerous to touch? | `where in_degree > 20 && loc > 100 && !has_doc` |
| What do these two modules share? | `intersect` of two reachability sets |
| How big is the migration off this API? | `from reachable_from(X, inbound, edges=[calls])`, `count` |

**Onboarding and census**

| Question | Query |
| --- | --- |
| Where should I start reading? | `order_by in_degree desc`, `where has_doc` |
| What is this repo made of? | `group_by [language, node_type]` |
| Biggest files by symbol count | `group_by file`, `order_by count desc` |
| How do two repos compare? | same query, `project: "other"` — already supported on every tool |

Roughly forty distinct questions, one tool, one engine. That ratio is the whole
argument for the design.

---

## Part C — Impact analysis

Question 3 is a different kind — reachability plus summarisation. It ships as
the `impact` **preset** of `code_query`, not as a second tool: the walk is
already expressible as `from: {reachable_from: …}`, and what makes it useful is
the rollup and the caveats, both of which belong in the renderer.

1. **Resolve** target → node set. A File id expands through `Contains` to its
   symbols; a symbol id is used directly.
2. **Reverse-reach** over `Calls · References · Imports · Extends · Implements ·
   Overrides` (never `Contains` — it is pure structure and would drag in every
   sibling), tracking hop distance, deduping by node id.
3. **Roll up**, not dump: direct dependents, transitive count by hop, affected
   files and folders, split test vs non-test, and — the question people
   *actually* mean — **which `Route` nodes and exported symbols are reached**.
   "Does this change touch a public HTTP endpoint" is the decision being made.
4. **Report honestly.** ug's `Calls` edges are name-resolved heuristically
   (Java uses receiver types, TypeScript is best-effort). Dynamic dispatch, DI,
   reflection and string-keyed lookups are invisible to it. The report must say
   so, and list `also_check`: files that *import* the target but whose
   symbol-level edge did not resolve. An impact report that implies
   completeness it does not have is worse than no report — it converts an agent
   from cautious to confidently wrong.

```
Impact of native/src/storage/store.rs — 23 symbols

direct dependents      41   (18 src, 23 test)
transitive (≤3 hops)  156   (94 src, 62 test)
files affected         37   across 9 folders
routes reached          0
exported symbols        6   KnowledgeStore, StoreError, NodeFilter, …

top affected: storage/ingest.rs (22) · storage/db.rs (17) · mcp/mod.rs (9)
⚠ structural edges only — dynamic dispatch, DI and reflection are not tracked
also_check: 2 files import this module with unresolved symbol edges
```

---

## Part D — Surfaces

Mirroring `agent_tools.rs`'s three-transport pattern exactly:

| Surface | Entry point |
| --- | --- |
| CLI | `ug query '<json>'`, `ug query --preset long_functions`, `ug query --preset impact --target src/a.ts` — dispatch in `main.rs:77` |
| MCP | **one** new tool, `code_query`, in `mcp/tools.rs:201`; dispatch joins the `tool_graph` arm at `mcp/mod.rs:420` (graph-only — no DB or embedder, so it keeps working when `search` cannot) |
| HTTP | `POST /api/tools/code_query` for the viz layer |
| Core | `agent_tools::run_tool` (`agent_tools.rs:2332`) gains **one** arm |

`graph_schema` (existing tool, `mcp/tools.rs:318`) is extended rather than
duplicated: it becomes the runtime capability manifest — node types, edge types,
**queryable columns with coverage**, and **available presets**. That is the only
discovery call an agent needs before writing any query.

---

## Part E — Extensibility: growing capability without growing the tool list

> "Doesn't make sense to keep adding more & more MCP tools."

Correct, and the constraint is quantitative. **Every tool's JSON Schema sits in
the tool list of every single request, forever, used or not.** The current 12
tools cost roughly 4k tokens per request. Adding `impact_of`, `comment_stats`,
`dead_code`, `coupling_matrix`, `test_gaps`, `api_surface` … pushes that past 7k
— a permanent tax on every turn, to serve questions that appear in maybe one
conversation in twenty. Worse, tool schemas are **static and client-cached**: a
new capability means a new binary *and* a client restart *and* a skill-file
update. Nothing about that scales.

So the rule this design holds:

> **A new MCP tool is justified only when it needs a different resource or
> transport.** `get_code` earns its place (reads the filesystem and the store).
> `reindex` earns its place (mutates). Anything that is *a question about the
> graph* is a preset or a column — never a tool.

Five extension points, in increasing order of leverage.

### E1. Presets as data, not code

A preset is a record, not a match arm:

```toml
# .ug/presets.toml  — version-controlled, ships with the repo
[layering_violations]
description = "Edges from the UI layer straight into persistence"
params = ["ui_prefix", "db_prefix"]
query = """
{ "from": "edges",
  "where": {"and": [{"field":"source.folder","op":"prefix","value":"{{ui_prefix}}"},
                    {"field":"target.folder","op":"prefix","value":"{{db_prefix}}"}]},
  "group_by": ["source.file"], "aggregate": [{"fn":"count"}] }
"""
```

Loaded from `~/.ug/presets.toml` (personal) and `<repo>/.ug/presets.toml`
(team, in git), mtime-watched like `graph.json` already is. A team encodes its
own architecture rules once; every agent on the project discovers them through
`graph_schema` and calls them by name. **New capability: zero code, zero new
tools, zero client restarts, and it travels with the repo.**

This is also how the built-in preset list stays honest — the twenty-two presets
in Part B are a seed file, not twenty-two functions.

### E2. Columns as a registry, not a match arm

`columns.rs` holds a table, not a `match`:

```rust
Column {
    name: "comment_lines",
    ty: Int,
    extract: |n, _| n.metrics.map(|m| m.comment_lines),
    coverage: |g| g.nodes.iter().filter(|n| n.metrics.is_some()).count(),
    since: SchemaVersion(2),   // drives the "not indexed — run ug reindex" error
}
```

Adding a fact is one row. `graph_schema` renders the table, so agents discover
the column the moment it exists — no tool schema change, no doc change, no
skill-file edit. The `coverage` and `since` fields are what make the silent-zero
guarantee (Risk 1) systematic rather than remembered case-by-case.

### E3. A generic fact bag on nodes — the deepest hook

Add to `GraphNode`:

```rust
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub facts: BTreeMap<String, Value>,
```

Language extractors emit whatever their grammar makes cheap: Java →
`visibility`, `is_static`, `is_abstract`, `throws`; TypeScript → `is_async`,
`is_default_export`, `jsx`; Python → `decorators`, `is_generator`; Rust →
`is_unsafe`, `is_pub`, `derives`. Any key present in a loaded graph
**auto-registers as a queryable column** with coverage computed by scan.

The consequence is worth stating plainly: *a new language capability becomes
queryable the day it is indexed*, with no change to the engine, the tool, the
schema, or the docs. Someone adds `is_unsafe` to the Rust extractor on Monday;
on Tuesday an agent answers "how much unsafe code is in this crate, by module"
without a release. **The graph becomes the schema.**

The cost is discipline — an untyped bag invites junk keys and inconsistent
naming across languages. Mitigate with a registered-key list in `columns.rs`
that promotes known facts to typed columns with documentation, leaving unknown
keys queryable but marked `experimental` in `graph_schema`.

### E4 & E5 — both come free with GQL

These were extension points a bespoke DSL would have had to grow. GQL already
has them, so they need no design and no code:

- **Expressions** — `CASE` (generic and simple), arithmetic, `STARTS WITH` /
  `ENDS WITH` / `CONTAINS`, `coalesce`, `lower`, `trim`, `toString`,
  `toInteger`. Bucketing, naming-convention audits and derived dimensions are
  all expressible without an engine change.
- **Set algebra** — `EXISTS { … }` / `NOT EXISTS { … }`, `UNION`, `UNION ALL`,
  `OPTIONAL MATCH … WHERE x IS NULL`. "Untested symbols" is not a feature; it
  is `WHERE NOT EXISTS { MATCH (t)-[:Calls*1..3]->(n) WHERE t.is_test }`.

The only ingest-side reason left to materialise a derived value as a property is
**performance** — a stored `folder` or `loc_bucket` can carry a property index
where a query-time `CASE` cannot. Treat that as an optimisation, applied to hot
presets after measurement, not as a design requirement.

### What ties it together: runtime discovery

E1–E3 all fail without it. A capability the agent cannot *find* does not exist.
So `graph_schema` is the contract:

```
columns:  loc(int, 1191/1191) · comment_lines(int, NOT INDEXED) ·
          is_test(bool, 1878/1891) · is_unsafe(bool, 701/1891, experimental) · …
presets:  comment_coverage · impact · layering_violations(team) · …
```

One cheap call, always current, generated from data. The `ug-mcp` skill file
then needs exactly one durable instruction — *"for any counting, aggregate or
blast-radius question, call `graph_schema` then `code_query`; never grep and
never loop"* — and it stays correct as capability grows.

**Net effect: the tool list is frozen at 13. Capability grows through
data — presets in git, columns in a registry, facts from indexers.**

---

## Part F — Escape hatch for real SQL

`ug export --format csv|parquet --out ./ug-export/` writing `symbols`, `files`,
`edges`, `folders` tables. ~150 lines, no new runtime dependency. Agents with a
shell get DuckDB/pandas/SQLite for anything the DSL cannot express; humans get
BI tooling for free.

This is the honest answer to "a flexible powerful db" without embedding one. An
in-process `ug sql` behind `--features duckdb` stays available as a later
option, but the export covers the need at a fraction of the cost.

---

## Part G — Presets in the viz layer

Presets are only "data, not code" (E1) if a human can **see** them. A preset
file nobody can browse is a config format, not a feature — and the team member
most likely to write one is the least likely to read `presets.toml`.

### Placement

`visualization.html:7121` already has the right structure: the **Discover** tab
carries subtabs *Search · Tour · Chat*. Add a fourth — **Insights** — reusing
the existing `.subtab` markup, glider and `role="tab"` semantics rather than
inventing a panel idiom.

Inside it, the preset chips reuse the `#tour-examples` chip pattern
(`visualization.html:7175`) — already styled, already the established way this
UI offers "here are some things you could ask".

```
Insights                                          [ built-in | this repo ]

  Documentation      Complexity        Architecture      Risk
  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
  │ Comment      │  │ Long         │  │ Coupling     │  │ Dead code    │
  │ coverage     │  │ functions    │  │ matrix       │  │              │
  │ 61% ▓▓▓▓▓░░░ │  │ 143 symbols  │  │ 9 folders    │  │ 27 symbols   │
  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘
                                       ▲ from .ug/presets.toml
```

Grouped by category, each card showing its **headline number precomputed on
load** — a preset with a live result is an invitation; a preset with a name is a
menu item. Cards sourced from the repo's own `.ug/presets.toml` carry a badge,
which doubles as the Risk 8 provenance signal: a user can see at a glance that
`layering_violations` came from the working tree, not from `ug`.

### The part that justifies putting it here at all

A table of numbers in a 3D graph viewer is a wasted opportunity. **Clicking a
preset result selects those nodes in the graph.** "143 functions over 50 lines"
stops being a statistic and becomes a shape — you see instantly that 90 of them
cluster in one folder. Drilling into a group row filters the selection further;
the existing node-detail panel handles the leaf.

That is the one thing the viz layer can do that neither the CLI nor an agent
can, and it makes statistics *spatial*: **the answer to "where is the hairy
code" is a region, not a list.** Coupling matrices and blast radius get the same
treatment — `impact` lights up the affected subgraph directly, which is a far
better artifact than a file list.

### Wiring

| Piece | Detail |
| --- | --- |
| `GET /api/presets` | name, description, category, params, source (`builtin` \| `repo` \| `user`) — served from the same registry `graph_schema` reads, so UI and agents can never disagree about what exists |
| `POST /api/tools/code_query` | already in Part D; the UI is just another caller |
| Cards | render `headline` + `unit` from the query result envelope |
| Selection | result `sample` ids → existing graph selection API |
| Custom query | a collapsed "Advanced" JSON editor, schema-validated against the same column registry — and the natural home for a GQL box if OverGraph ships one |

No new backend concepts: the viz layer consumes exactly the endpoint and
registry the agent path already needs. That is the test for whether E1 was
designed right, and it passes.

---

## Risks

1. **Silent zero — the dominant risk.** If `comment_lines` is unindexed,
   "0 methods have comments" is far worse than an error. Any query touching an
   entirely-unpopulated column must *fail loudly*; partially-populated columns
   must report their denominator. This is why A5 and the `coverage` block are
   non-optional.
2. **Stale index.** `staleness_note` already exists (`mcp/mod.rs:407`) and must
   propagate here — a precise-looking number implies freshness it may not have.
3. **`Concept` pollution.** Markdown headings are `Concept` nodes (303 of 1891
   here). `from: "symbols"` must exclude `Concept`/`File`/`Folder` by default or
   every code statistic is wrong on any repo with docs.
4. **Double counting.** Multi-path reachability must dedupe by node id.
5. **`loc` ambiguity.** Span vs code lines differ by ~30%. Report both, name
   them distinctly, and document which the presets use.
6. **Scale.** In-memory scan is right up to ~10⁵ nodes (~50 MB graph). Beyond
   that, `nodes_by_type` paging already exists in overgraph 0.6
   (`engine/read.rs:2395`) as a fallback path. Not a v1 concern.
7. **Untyped fact bag drift (E3).** An open `facts` map invites five languages
   to spell the same concept five ways (`is_pub` / `visibility` / `exported`).
   Mitigate with a registered-key list that promotes known facts to typed,
   documented columns and marks the rest `experimental` in `graph_schema` —
   queryable, but clearly not a stable contract.
8. **Repo presets are executable input (E1) — largely solved by `ReadOnly`.**
   A `.ug/presets.toml` arrives with a cloned repo, and GQL *can* mutate. Run
   every preset with `mode: ReadOnly`, which rejects mutation statements before
   write staging: a hostile preset cannot write, no matter what it contains.
   What remains is resource use and misleading output, so also pin the caps
   explicitly rather than inheriting defaults, and have `graph_schema` label
   repo-supplied presets so a user can see `layering_violations` came from the
   working tree, not from `ug`.
9. **~~DSL complexity creep~~ — resolved by adopting GQL.** The failure mode of
   a home-grown DSL was growing it into a bad query language one feature at a
   time. Using OverGraph's removes the risk entirely; the discipline now applies
   to *presets*, which must stay declarative GQL and never become a macro
   language.
10. **Statistics become DB-dependent.** The largest cost of the GQL decision.
    `tool_graph` is DB-free today, and `reindex` explicitly keeps structural
    tools fresh when embedding fails — so a project can legitimately have a good
    `graph.json` and no usable `ugdb`. Three ways out, in preference order:
    **(a)** let ingest write nodes and properties even when embedding fails, so
    the store exists without vectors — the cleanest fix and useful well beyond
    this feature; **(b)** keep a small in-memory fallback for the handful of
    presets that need no traversal; **(c)** accept the dependency and report a
    clear "statistics need `ug reindex`" error. **(a) is the recommendation** —
    it turns a degraded mode into a supported one.
11. **Upstream version velocity.** 0.6 → 0.17 is eleven minor versions in a
    dependency that renames public API between them (this upgrade alone renames
    eight symbols). Pin exactly, read the changelog on every bump, and keep the
    `KnowledgeStore` trait (`storage/store.rs:220`) as the insulation layer it
    already is — the fact that all 14 errors landed in one file is evidence that
    boundary is working.

---

## Phasing

Reordered by the GQL decision: the upgrade and the ingest widening move to the
front, because everything else now depends on them.

**P0a — the OverGraph 0.17 migration.** Self-contained, independently
valuable, and everything else waits on it. Verified scope: 14 compile errors,
all in `native/src/storage/db.rs`.

1. Bump `native/Cargo.toml:40` to `overgraph = "0.17"` (pin exactly).
2. `NodeRecord`/`EdgeRecord` → `NodeView`/`EdgeView` at the import and in
   `NodeRow`/`EdgeRow` conversions.
3. `type_id: u32` → `label: &str` on `NodeInput` / `EdgeInput`, and
   `get_node_by_key(type_id, key)` → `(label, key)`. **This is the step that
   retires most of `storage/types_registry.rs`** — labels are strings now, so
   the numeric `node_type_to_id` / `edge_type_to_id` mapping and its
   round-tripping mostly disappear. Delete rather than adapt it.
4. `nodes_by_type` / `get_nodes_by_type` → `get_nodes`;
   `TraverseOptions.edge_type_filter` and `PprOptions.edge_type_filter` →
   `.edge_label_filter`; `VectorSearchRequest.type_filter` renamed;
   `NeighborEntry.edge_type_id` → label-based.
5. **Existing `ugdb` stores are stale after this** — node/edge type encoding
   changed. Bump the `ug-meta.json` sidecar (`storage/db.rs`, `DbMeta`) with a
   store-format version and refuse to open an older store with a "run
   `ug reindex`" message, rather than reading it as garbage.
6. Verify: `cargo test`, `cargo build --release`, `ug help`, then a full
   `ug gen` on this repo and a `search` that returns sensible hits.

*Do not fold anything else into this commit.* A pure dependency migration that
can be reverted in one step is worth more than a fast one.

**P0b — widen the store.** `node_props` (`storage/db.rs:462`) gains every
queryable fact: A1 derived columns (`loc`, `folder`, degrees, `has_doc`),
booleans as 0/1, and `is_test`. Declare `CREATE PROPERTY INDEX` on the hot ones
(`node_type`, `loc`, `file`, `is_test`) at the end of ingest. Plus Risk 10(a):
**ingest writes nodes and properties even when embedding fails**, so statistics
survive a missing embedder. Independently valuable — this is also what fixes
`semantic_search`'s broken `whereClause`.

**P0c — the layer that is actually the product.** `code_query` (one MCP tool,
`preset` or raw `gql`), the response envelope carrying **coverage *and* cap
warnings**, percentile helpers over `collect()`, built-in presets as GQL strings
executed with `allow_full_scan: true` and `mode: ReadOnly`, `graph_schema` as
capability manifest.

**P1 — requires reindex; answers question 1 properly.**
Comment/doc/code-line metrics (A2), class metrics (A3), file classification and
language on nodes (A4), schema version (A5) — written to **both** `graph.json`
and `node_props`.

**P2 — extensibility surface.**
Preset files holding GQL (E1), the property registry that feeds the manifest
(E2), the ingest-side fact bag (E3). E4/E5 are largely absorbed: expressions
become materialised properties at ingest, and set algebra becomes GQL's own
`OPTIONAL MATCH` or a two-query difference in the render layer.

**P2 also — Insights pane (Part G).** Depends only on `GET /api/presets` and
`code_query`, so it can land the moment E1 does. The Advanced editor is now a
**GQL box**, not a JSON editor — a strictly better power-user surface, and one
`explain_gql` can back with a real query plan.

**P3 — power users.** CSV/Parquet export; optional `ug sql`.

**Docs, shipped with P0:** `docs/mcp.md`, `docs/api-reference.md`, README, the
website slide deck, and — most importantly — the `ug-mcp` SKILL.md (both
`~/.claude/skills/ug-mcp/SKILL.md` and the copy `mcp/install.rs` distributes)
gains one durable decision rule:

> **For any counting, aggregate, distribution or blast-radius question, call
> `graph_schema` then `code_query`. Never grep for a count. Never loop a
> per-file tool to build one.**

That rule has to be phrased against the *engine*, not against a list of
presets — otherwise it goes stale the first time someone adds one, which is
exactly the failure mode this whole design exists to avoid.
