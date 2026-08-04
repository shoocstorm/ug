# `code_query` — whole-repo statistics & impact analysis

`code_query` answers counting, distribution and blast-radius questions over the
entire indexed graph in one call: *"how many methods are longer than 50
lines?"*, *"which folders are worst documented?"*, *"what breaks if I change
this file?"*. One MCP tool / CLI command / HTTP route; capability grows through
**presets** and **stored properties**, never through new tools.

The point is cost. Answered by grep-and-read, "how many methods are longer than
50 lines?" is ~500k tokens on a medium repo and impossible on a monorepo;
answered by looping `file_outline` it is ~40k tokens and 80 round-trips.
`code_query` answers it in one call and roughly 100 tokens, because ingest
already stored the facts a query engine can aggregate.

This doc covers the design, the fact layer, the query surface, and the
operational contract. For the tool schema see
[`docs/API-REFERENCE.md`](API-REFERENCE.md) (§CLI 1.4, HTTP 2.6, tools 3.1) and
[`docs/MCP-SERVE.md`](MCP-SERVE.md) (§10). For the Insights pane in the
visualization see [the viz README](../native/src/vis/README.md).

---

## The query language is OverGraph GQL — not a bespoke DSL

The original design considered four surfaces for statistics queries:

| Option | Verdict |
| --- | --- |
| **(a) Embed DuckDB/SQLite** | Powerful and familiar, but ~50 MB of binary bloat, and SQL is worst at the one question that matters most: transitive reachability needs recursive CTEs, which LLMs write badly. |
| **(b) ISO-GQL via `overgraph::execute_gql`** | **Shipped in overgraph 0.17 and covers the whole use case** — aggregation, implicit grouping, HAVING, variable-length paths, a planner and `explain`. This is what ug uses. |
| **(c) Bespoke JSON DSL** | ~1200 LOC of evaluator to reimplement, worse, what (b) already ships. |
| **(d) Fixed canned tools** | ~20 tokens per call, zero flexibility, unbounded tool-list growth. Kept only as **presets** over (b). |

**Decision: (b) as the query language, (d) as named presets over it, (a) as an
export escape hatch. Never build (c).**

Why this is the right call, verified against the engine rather than a roadmap:

- Aggregates `count · sum · avg · min · max · collect`, implicit Cypher-style
  grouping (`RETURN n.kind AS k, count(*) AS c`), HAVING via `WITH … WHERE`,
  and `ORDER BY` on an aggregate.
- Variable-length paths `*1..3`, `shortestPath` / `allShortestPaths` — the
  capability that makes Q3 (impact) a single statement instead of a recursive
  CTE.
- `CASE` for bucketing (histograms need no ingest-time materialisation),
  `EXISTS { … }` subqueries for set algebra ("symbols no test reaches" is
  `WHERE NOT EXISTS { … }`), `OPTIONAL MATCH`, string predicates
  (`STARTS WITH` / `ENDS WITH` / `CONTAINS`), and metadata functions
  (`elementKey(n)` is ug's string node id, `labels(n)` the node type).

Two operational gotchas that would otherwise make every preset look broken:

1. **`allow_full_scan` defaults to `false`.** A statistics query is a full scan
   by nature — "how many functions exceed 50 lines" has no bounded anchor.
   Every preset sets `allow_full_scan: true` explicitly, or it fails at
   planning.
2. **Caps truncate *and* one of them errors.** `max_rows` truncates silently;
   `max_frontier` on an unanchored traversal *errors* outright. Both are
   handled — truncation warns, and a frontier error gets a message explaining
   how to narrow the walk (anchor one end or reduce the bound). See
   [Silence and honesty](#silence-and-honesty).

### What running the queries changed

Five things were wrong in the design or the first implementation, and every one
was found by executing against the real index, not by reading:

1. **`count(DISTINCT elementKey(dep))`, not `count(*)`.** A variable-length
   match yields one row per *path*. `impact` on `storage/store.rs` reported
   **948** dependents with a plain count and **11** with the distinct one.
2. **`EXISTS { MATCH … WHERE … }` needs its own `RETURN` inside.** The design's
   example was a parse error.
3. **Caps do not only truncate — `max_frontier` *errors*.** `untested_symbols`
   at `*1..3` exceeded it outright on a 2280-node repo; at `*1..2` it answers
   in ~180 ms.
4. **`NOT x IN [...]` must be parenthesised** as `NOT (x IN [...])`, or the
   engine rejects the operands.
5. **Percentiles have to be computed in the render layer.** `percentileCont`
   parses (it is in the aggregate-name list) but does not lower — so it fails
   at execution, not at parse. Only `count · sum · avg · min · max · collect`
   work; percentiles are derived from a `collect()` column, bounded by
   `max_collect_items`.

Also settled along the way: `UnknownEdgeLabel` warnings are **suppressed for
presets and shown for raw GQL** (no single language emits every dependency edge
type, so a preset naming `Overrides` on a Rust graph is not a mistake; in a
query the caller wrote, the same warning almost always is a typo). The
full-scan notice is suppressed always — it is true of every statistical query
by construction, and echoing it would train readers to skip the line where the
real warnings live.

---

## The fact layer — Part A

A query can only answer what ingest stored. The facts below are derived once at
ingest time by `native/src/storage/facts.rs` and written as **plain sibling
properties** (`n.loc`, not `n.f_loc`) so GQL reads the way someone would write
it by hand.

| Property | Notes |
| --- | --- |
| `loc` | Code span, **inclusive** at both ends. Falls back to `end_line - start_line + 1` when `metrics` is absent, which is what gives Class/Interface nodes a size. |
| `code_lines` / `comment_lines` / `doc_lines` | Computed per file in `indexer/line_metrics.rs`, once, rather than in the five language extractors — so all languages gain them at once and "a comment" cannot mean different things per language. |
| `has_doc` / `has_comments` | Two separate booleans, deliberately. On this repo 329 functions carry prose but no doc comment; collapsing those into one "documented" figure hides exactly that finding. |
| `params` / `max_nesting` / `members` | `members` is only populated for languages whose class body encloses its members (Java, Python, TypeScript). A Rust struct's methods live in a separate `impl` block, so Rust types carry no `members` — the coverage line says so rather than ranking them all as memberless. |
| `folder` | Parent dir of `file`. |
| `is_test` | Prefers the indexer's file classification, keeps a path heuristic as fallback. |
| `in_degree` / `out_degree` | Computed once per ingest. `in_degree` moves when some *other* file starts calling a node, which is why incremental ingest must compare it. |
| `language` / `classification` | Stamped on every symbol in a file, not just the File node, so "group by language" is a scan and not a join. |
| `qualified_name` / `route` / `annotations` | Present for languages that resolve them (Java, TypeScript); reported `NOT INDEXED` where they never exist (e.g. `route` on a docs-only sample). |

Booleans are stored as **0/1 integers** — GQL has no boolean aggregate, so
`sum(n.has_doc) / count(*)` is the only way to ask "what fraction is
documented".

These are also written to `graph.json` under a schema version
(`GRAPH_SCHEMA_VERSION`); a store built before a property existed is rejected
with "run `ug regen`" rather than read as garbage.

### Queryable columns

`node_type` · `name` · `file` · `folder` · `language` · `classification` ·
`loc` · `code_lines` · `comment_lines` · `doc_lines` · `params` ·
`max_nesting` · `members` · `has_doc` · `has_comments` · `is_test` ·
`in_degree` · `out_degree` · `qualified_name` · `route` · `annotations` ·
`start_line` · `end_line`.

Three pairs are easy to confuse, and picking the wrong one changes the answer:

| Use | Not | Because |
|---|---|---|
| `code_lines` | `loc` | `loc` is a *span* — it counts blanks and comments. On this repo the longest function is 582 lines by span and 446 by code, a 23% gap. |
| `has_comments` | `has_doc` | `has_doc` is a doc-comment flag only. Of 1597 functions here, 828 carry prose but just 499 have a doc comment. |
| `is_test` | a path filter | `is_test` prefers the indexer's classification and catches test files that aren't named like one. |

---

## The query surface

One implementation (`native/src/code_query/`), three transports, matching the
`agent_tools` pattern:

| Surface | Entry point |
| --- | --- |
| CLI | `ug query <preset>` · `ug query --preset impact --target src/a.ts` · `ug query --gql "MATCH …"` · `ug query --list` |
| MCP | `code_query` tool — `{"preset": "long_functions"}` or `{"gql": "…"}` |
| HTTP | `POST /api/tools/code_query` · `GET /api/presets` |

`code_query` takes either a named `preset` or raw `gql` (mutually exclusive),
plus `args`, `limit`, and `range`. **Arguments are bound as GQL params, never
interpolated**; an undeclared argument is an error, not an ignored key.

`code_query` does **not** join the `tool_graph` arm — that arm is DB-free, and
aggregation needs stored properties. It has its own dispatch arm that opens the
store **without an embedder**, which preserves the useful half of the property:
statistics keep working when `search` cannot. `graph_schema` stays graph-only
and the store half (property coverage + preset list) is appended separately, so
the call still succeeds when there is no usable store.

### Paging without re-reading

A `range` is a **window over rows the query already produced** — the engine
computes the same thing either way, which is what keeps the totals honest
across pages. 1-based, inclusive at both ends, capped at 200 rows per window;
liberal spelling (`20` · `11-35` · `34-end` · `rows 11 to 35`) because every
rejected form is a round-trip to a caller who was already unambiguous.

```
rows 11–35 of 122 · 864 graph matches before grouping
next: rerun with range "36-55"
```

A window past the end reports how many rows exist rather than "no rows" — those
are different situations, and confusing them sends the caller off to debug a
query that works. Preset `LIMIT`s are high (200) so any window is reachable;
only the visible window is formatted, so this costs memory, not tokens.

### Presets

A preset is a **GQL string with a description**, loaded as data — not a match
arm. The list below is a seed file, not a set of functions, and
`graph_schema` / `GET /api/presets` advertise whatever is loaded. Built-ins:

Census · size · documentation · dead code · architecture · tests · risk —
e.g. `comment_coverage`, `comment_density`, `token_docs`,
`undercommented_complexity`, `long_functions`, `long_functions_by_code`,
`classes_by_members`, `language_breakdown`, `file_kinds`, `param_bloat`,
`god_classes`, `dead_code`, `orphan_files`, `untested_symbols`, `test_ratio`,
`impact`, `retest_scope`, `layering_violations`, `coupling_matrix`, `size_histogram`, …

Run `ug query --list` (or call `graph_schema`) for the current set — the
authoritative list is the code, and this paragraph will otherwise drift.

---

## Impact analysis — the `impact` preset

Reachability plus summarisation, shipped as a preset rather than a second tool:
the walk is expressible as a bounded variable-length GQL path, and what makes
it useful is the rollup and the caveats, both of which belong in the renderer.

1. **Resolve** target → node set. A File id expands through `Contains` to its
   symbols; a symbol id is used directly.
2. **Reverse-reach** over `Calls · References · Imports · Extends · Implements ·
   Overrides` (never `Contains` — it is pure structure and would drag in every
   sibling), bounded (`*1..N`, never unbounded), tracking hop distance.
3. **Roll up, not dump.** Direct dependents, transitive count by hop, affected
   files and folders, split test vs non-test, and — the question people
   *actually* mean — which `Route` nodes and exported symbols are reached.
4. **Report honestly.** ug's `Calls` edges are name-resolved heuristically
   (Java uses receiver types, TypeScript is best-effort). Dynamic dispatch, DI,
   reflection and string-keyed lookups are invisible to it. The report says so
   and lists `also_check`: files that *import* the target but whose
   symbol-level edge did not resolve. An impact report that implies
   completeness it does not have is worse than no report.

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

`count(DISTINCT elementKey(dep))` is what makes the transitive number honest —
a plain `count(*)` counts paths, and an impact report that quadrupled its real
dependents would not survive first contact with a user.

---

## The response envelope — silence and honesty

Every answer renders a table (or a bare count for a 1×1 result), leftover-id
samples, and — non-negotially — a **coverage line** and **cap warnings**:

```
Functions with loc > 50, excluding tests — 143 of 1162 (12.3%)

by folder                        count   avg loc   p90
  native/src/indexer/languages      31      88      210
  native/src/storage                27      74      166
  …                                                     (13 more groups)

samples: function:native/src/main.rs:36:main, function:…
coverage: loc 1191/1191 · is_test 1878/1891 · comment_lines NOT INDEXED (run `ug regen`)
⚠ Index may be stale: 3 changed files
```

The coverage line is the contract. `MATCH (n:Function) WHERE n.comment_lines >
3 RETURN count(*)` returns `Int(0)` on an index that has never recorded comment
lines — **no error, no engine warning** — and "no functions have long comments"
is a far worse outcome than a refusal. So `code_query` probes the properties
each query reads, reports their denominators, and flags any that are entirely
unpopulated as `NOT INDEXED`. Caps truncate silently, so the blast radius of an
impact query that hit `max_frontier` is a *lower bound* — that warning is
carried through too. Output is hard-capped at ~3k chars; `truncated` and
`rows_matched` (the denominator) are always reported.

This is the same honesty in two directions: the coverage contract is about
*properties that were never stored*; the cap warnings are about *rows dropped
mid-execution*. Both produce a confident wrong number, so both are surfaced.

---

## Why this stays cheap as capability grows

The rule this feature holds:

> **A new MCP tool is justified only when it needs a different resource or
> transport.** `get_code` reads the filesystem; `regen` mutates. Anything that
> is *a question about the graph* is a preset or a column — never a tool.

Every tool's JSON Schema sits in the tool list of every request, forever, used
or not. The old twelve tools cost ~4k tokens per request; one per statistics
question would push that past 7k as a permanent tax. Instead:

- **Presets are data, not code.** A preset is a record — name, description,
  params, GQL string — loaded from built-ins and, when shipped, from
  `<repo>/.ug/presets.toml` (team, in git) and `~/.ug/presets.toml` (personal),
  mtime-watched like `graph.json`. New capability: zero code, zero new tools,
  zero client restarts, and it travels with the repo. Because repo-supplied
  presets are executable input arriving with a cloned repo, every preset runs
  with `mode: ReadOnly` — a hostile preset cannot write, no matter what it
  contains.
- **Columns are a registry, not a match arm.** A fact is one row (name, type,
  extractor, coverage, `since` schema version). `graph_schema` renders the
  table, so agents discover the column the moment it exists.
- **The tool list is frozen.** Capability grows through data — presets in git,
  columns in a registry, facts from indexers. One durable decision rule
  replaces a growing tool list:

> **For any counting, aggregate, distribution or blast-radius question, call
> `graph_schema` then `code_query`. Never grep for a count. Never loop a
> per-file tool to build one.**

---

## Risks that remain

1. **Silent zero — the dominant risk.** Mitigated by the coverage contract, but
   it is why `graph_schema` lists property denominators and `code_query`
   renders `NOT INDEXED` instead of `0`. A regression here is worse than a
   crash.
2. **Stale index.** `staleness_note` propagates into the envelope — a
   precise-looking number implies freshness it may not have.
3. **Statistics depend on the store.** `code_query` opens the ugdb without an
   embedder, but it still needs the store to exist. Ingest writes nodes and
   properties even when embedding fails, so the degraded mode is *supported*,
   not an error.
4. **Upstream version velocity.** The 0.6 → 0.17 upgrade renamed eight public
   symbols. Pin exactly in `native/Cargo.toml`, read the changelog on every
   bump, and keep engine calls behind the `KnowledgeStore` trait
   (`native/src/storage/store.rs`) — the fact that all 14 migration errors
   landed in one file is evidence that boundary is working.
5. **`Concept` pollution.** Markdown headings are `Concept` nodes; statistics
   presets scope by `node_type` explicitly so "every code statistic" is not
   silently diluted by docs.

---

## Status

Shipped. Tracked from design through implementation in the session logs under
`docs/dev/` (2026-07-28 → 2026-07-29); the working state is: `code_query` +
`graph_schema` live on CLI / MCP / HTTP; 33 built-in presets; the fact layer
(comment/class metrics, file classification, schema versioning) lands on a
reindex; the Insights pane in the visualization is the fourth Discover subtab;
row ranges give paging without re-reading. **Remaining:** CSV/Parquet export
(`ug export --format csv|parquet`) as the SQL escape hatch, and repo-supplied
`.ug/presets.toml` (the `source` field in `GET /api/presets` already exists for
it).
