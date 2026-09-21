# Preset audit — 2026-09-21

## Round 1

Every `ug analyze` preset run against `~/.ug/ug` (5,820 nodes / 19,412 edges)
and read, not just executed. All 39 execute; that was never the problem. Six
of them answered a question other than the one their own description names,
and did it in the shape of a correct answer — a real table, real ids, real
numbers, nothing to notice.

This is the record of what was found, what was changed, and — at the end —
what was looked at and deliberately left alone.

**Method.** A shell loop over every no-argument preset —
`ug analyze <p> -n ug -k 12`, ANSI stripped — captured to a file before and
after, then diffed on the `rows 1–N of M` line. Numbers below are from that
pair of runs, on the same store, minutes apart. Presets taking a required
argument (`impact`, `retest_scope`, `test_for`, `layering_violations`, the
`diff_*` pair) were run separately and are unchanged by this audit.

---

## Summary

| # | Preset | Was | Now |
|---|---|---|---|
| 1 | `orphan_files` | 145 rows of 213 files (68% of the repo) | **7**, every one real |
| 2 | `is_test` (the fact, not a preset) | missed a file named `tests.rs` — 24 symbols | fixed; 9 presets were reading it |
| 3 | `where_to_start` + 8 more | test fixtures ranked as production code | `is_test = 0`, pinned by a test |
| 4 | `comment_density` | ordered by folder **size** | ordered by **density**, ratio shown |
| 5 | `doc_coverage_by_folder` | "least-documented" by raw **count** | by **fraction**, ratio shown |

---

## 1. `orphan_files` returned 68% of the repository

```
Files nothing imports or references.
  → 145 rows, of 213 indexed files
```

`MATCH (n:File) WHERE n.in_degree = 0`. A File node's `in_degree` counts only
**file-incident** edges — `Imports`, `References`, `Exports`, `DependsOn`
(Agents.md §11b). A module whose functions are called from thirty other files
still reads as zero. Broken down, the 145 were:

| | orphans | of | |
|---|---:|---:|---|
| test `.rs` | 39 | 39 | 100% — nothing imports a test. That is what a test is. |
| `.js` | 28 | 28 | 100% — the vis parts are concatenated by `build.rs`, never imported |
| `.md` / `.pdf` | 24 | 35 | documents are not imported by anything, ever |
| non-test `.rs` | 54 | 111 | 49% — Rust `mod` declarations mostly draw no edge |

So the preset was measuring *"the resolver drew no file-level edge"* and
presenting it as *"this file is dead"* — the same §9t failure the `dead_code`
audit fixed with `name_mentions`, in the one other preset that never got the
treatment.

**Fix.** A new stored fact, `external_in_degree`, written for File nodes:
dependency edges reaching into the file from another file, counting edges into
the File node *and into every symbol it holds*. Computed in the pass
`FactContext::new` already makes over the edge list, so it costs one borrowed
`HashMap` and no second traversal. Then three narrowings, each cutting a
category whose answer is structurally fixed rather than interesting:

- `external_in_degree = 0` — nothing outside reaches the file or its contents. **145 → 31**
- `is_test = 0` — a test file is never imported. **39 of the 145**
- not `documentation` — 19 of the remaining 31 were `.md`, burying 7 real rows

```
Code files nothing outside them reaches — no import, and no call into
any symbol they hold. Excludes tests and docs, which are never imported
by design.

file:native/src/chat_eval.rs             rust  408
file:native/build.rs                     rust  181
file:native/src/lib.rs                   rust   59
file:native/src/storage/mod.rs           rust   59
file:native/src/bin/ug_app.rs            rust   22
file:native/src/main.rs                  rust   11
file:native/src/storage/backends/mod.rs  rust    8
```

Five are entry points and module roots — correct, and self-evidently so from
the names. `chat_eval.rs` is a true positive: `grep` finds exactly one
reference to it in the tree, `mod chat_eval;` in `lib.rs`.

It is also **faster**: 1.01 s → 0.76 s wall, because a single File scan
replaced nothing (the correct-but-slow formulation, a correlated `NOT EXISTS`
subquery, measured **2.8 s** — see "Rejected" below).

The rewrite also gives it an `ORDER BY`. Five runs of the same binary over the
same store agreed, so the engine's scan order is deterministic — but
`LIMIT 200` with no ordering means a repo with more than 200 orphans drops
rows by store order rather than by anything the caller asked for, and a
re-ingest is free to move that order (§9c).

---

## 2. `is_test` never matched a file called `tests.rs`

Not a preset — the fact nine of them filter on.

`TEST_PATH_MARKERS` is a list of *anchored* substrings, and the anchoring is
load-bearing: an unanchored `test_` swept in `latest_version.rs` and
`fastest_path.ts`, which is why every marker carries a `/` or a `.`/`_`
delimiter. But the list had `"/tests/"` (needs a trailing slash) and
`"_tests."` (needs a leading underscore) and nothing for a file *named* for
tests with no prefix at all.

`native/src/agent_tools/tests.rs` matched neither. Of its 164 symbols, 140
were rescued by their `#[test]` / `#[tokio::test]` annotations — and the
other **24 were its un-annotated helpers**: `node`, `edge`, `fixture`. Those
are the ones with in-degrees in the forties, because every test in the file
calls them. `fixture` (in-degree 43, doc-commented) sat sixth in
`where_to_start`.

**Fix.** Four markers, anchored on `/` like the rest: `/test.`, `/tests.`,
`/spec.`, `/specs.`. The `/` is what keeps `latest.rs` and `manifest.rs` out —
the character before `test.` there is a letter, not a separator.

Verified against a case checked by hand first (§9o's corollary): symbols in
`agent_tools/tests.rs` reading as tests went **140 → 164**, exactly the 24
predicted, and repo-wide `is_test = 1` moved by the same 24.

---

## 3. Test scaffolding ranked as production code, in nine presets

A test fixture has a high in-degree (every test in the file calls it) and
often a doc comment. That is precisely what these presets rank on, so
fixtures land near the top of all of them.

`where_to_start` was the worst instance. Its description is *"the reading
order for a newcomer"*; five of its top twelve rows were test helpers —
`sample_graph`, `router_for`, `fixture`, `targets`, `EnvGuard::new` — and the
six underneath were 3-to-10-line delegating wrappers (`index` at 3 lines,
`project_dir` at 3, `die` at 4). Not one row was something to read.

**Fix.** `is_test = 0` added to nine presets, plus a substance floor on
`where_to_start`:

| preset | candidates before → after |
|---|---|
| `where_to_start` | 1,143 → 568 (`is_test = 0` **and** `loc >= 15`) |
| `size_histogram` | 4,120 → 2,319 |
| `duplicate_names` | 20 rows → **8** |
| `deep_nesting` | 107 → 101 |
| `param_bloat` | 49 → 45 |
| `god_classes` | 311 → 308 |
| `classes_by_members` | 107 → 106 |
| `risky_symbols` | 15 → 14 |
| `fanout_offenders` | unchanged (no test symbol has fan-out > 20) |

`loc >= 15` on `where_to_start` is the floor that removes the delegating
wrappers. `loc` is a *span*, so a three-line function carrying a twelve-line
doc comment still passes — which is the right call: someone wrote twelve lines
about it for a reason. The first page is now `facts::compute`,
`plan_incremental_ingest`, `range::parse`, `find_shortest_path`, `open_store`,
`file_context`.

`duplicate_names` is the biggest single jump. With tests included it returned
`run` (14), `node` (14), `args`, `argv`, `row`, `item` — per-test-module
locals, in a list whose stated purpose is finding duplication. Excluding them
leaves eight rows, and **all eight are the same function reimplemented once
per language extractor**: `collect_calls`, `extract_params`,
`record_type_refs`, `signature_type_refs`, `visit`, `push_call`,
`extract_calls`, `argument_count`. It also gained a `folders` column, which
separates the two cases the description names — one folder is a per-variant
family, several is real duplication.

`size_histogram` matters for a second reason: it is the distribution whose
tail `long_functions` lists, and the two were counting different populations.
A histogram that disagrees with its own tail-listing preset is worse than
either answer alone, because the reader assumes they match.

**Pinned by** `presets_about_production_code_exclude_tests`
(`native/tests/analyze_test.rs`). It names the fourteen presets that must
filter, asserts the clause is in the GQL *and* that no test symbol survives
into the rows. The list is explicit rather than "every preset", because
plenty of them are supposed to see tests — `test_ratio` counts them,
`untested_symbols` and `test_for` walk from them, `impact` must include them
in a blast radius, a census of the repo is a census of the repo. Naming the
ones that must not is the only version of this rule that is true.

Verified non-vacuous: removing the clause from `deep_nesting` and
`where_to_start` fails it on both assertions.

---

## 4. `comment_density` ranked by folder size

```
Comment-to-code line ratio per folder — where the prose actually is.
  → three raw sums, ORDER BY code_lines DESC
```

No ratio column, and ordered by how *big* each folder is. The top row was the
biggest folder, not the densest, so "where the prose actually is" was answered
with "wherever the code is". On this repo the two orders barely overlap:

| by size (was) | | by density (now) | |
|---|---:|---|---:|
| `vis/js` | 17% | `native` | 53.5% |
| `cli` | 17% | `graph` | 34.5% |
| `native/src` | 23% | `storage` | 31.6% |
| … | | `indexer` | 30.9% |

`graph`, `storage` and `indexer` — the three densest — sat 8th, 9th and 10th.
A reader taking the first rows as the answer got the opposite of the finding.

**Fix.** A `prose_pct` column, and `ORDER BY prose_pct DESC`.
`comment_lines` and `doc_lines` stay separate columns — the gap between "has
prose" and "has a doc comment" is what the whole documentation section exists
for — and `prose_pct` sums them only to rank.

---

## 5. `doc_coverage_by_folder` ranked by a raw count

```
Which folders are worst documented, least-documented first.
  → ORDER BY documented ASC
```

`documented` is a count, so the ranking was decided by how *small* a folder
is. `native` (5 symbols, 3 documented — **60%**) came third as "worst
documented"; `mcp` (123 symbols, 55 — **45%**) came eighth.

**Fix.** A `documented_pct` column and `ORDER BY documented_pct ASC, total
DESC`. The tie-break matters more than usual here: once the sort key is a
percentage, 0% covers four folders. The list now opens `vis/js` 0/759,
`vis` 0/7, `storage/embed` 38.9%, `storage/backends` 41.4%, `mcp` 44.7% —
and `native` has left the top ten.

---

## Rejected, with the number that killed it

Kept so the next audit does not re-propose them.

| Proposal | Why not |
|---|---|
| `orphan_files` as a correlated `NOT EXISTS` subquery over symbols | Correct — same 31 rows — and **2.8 s** against a 1.0 s baseline on a 5.8k-node graph, because it is O(files × edges) per query. On `big500k` (485k nodes) it is not runnable. The stored fact is O(edges) once at ingest. |
| Anchor that subquery through `Contains` to make it cheaper | **`Contains` cannot be written as a GQL relationship label at all** — see the note below. And the workaround form still measured 2.8 s. |
| `GRAPH_SCHEMA_VERSION` bump for `external_in_degree` | It derives from `n.file` and edge endpoints, both of which every `graph.json` has always carried, so an old graph gets the fact the moment this build re-ingests it. Bumping would mark every existing graph stale for a format change that did not happen. The real staleness — a *store* written by an older `ug` — already shows as `NOT INDEXED` in the coverage line. |
| `min_loc` as a **parameter** on `where_to_start` | Speculative configurability (§2). The floor exists to drop delegating wrappers, which is not a thing anyone wants to tune. A fixed, documented `15`. |
| `is_test = 0` on `dependency_fanin` | "The most depended-upon symbols in the repo" is a factual ranking, and a fixture called by forty tests genuinely is depended upon. Filtering would answer a different question. |
| `is_test = 0` on `comment_coverage` / `doc_coverage` / `repo_census` / `language_breakdown` / `biggest_files` / `coupling_matrix` | A census of the repo is a census of the repo. |

---

## Looked at, left alone

- **`god_classes` measures the wrong thing in Rust.** A struct's `loc` is its
  field block; its methods live in a separate `impl`. `Db` has 45 members and
  a `loc` of 23, so it does not appear, while a 193-line trait declaration
  tops the list. There is no better stored property — `members` is what
  `classes_by_members` is for — and inventing one is a larger change than
  this audit.
- **The coverage line cries wolf on the boundary presets.** `boundary_kinds 2%`
  is *correct and expected*: only 142 of 5,820 nodes are boundaries, and all
  142 carry it. But §11a tells a reader that a preset filtering on a 1%
  property "is answering about almost nothing", which is exactly wrong here.
  The denominator should arguably be the matched set, not the graph. That is a
  renderer change, not a preset one.
- **`coupling_matrix` counts parent-directory edges as coupling.**
  `native/src/cli → native/src` (492) is the largest row and is mostly a
  module talking to its own parent. Inherent to per-folder granularity.
- **`untested_symbols` is dominated by `vis/js`.** Correct — the frontend has
  no Rust tests reaching it — but it means the Rust answer starts around row
  20. A `--arg language=` filter would fix it and was not requested.
- **`docs/ANALYZE.md:120`** claims a store built before a property existed "is
  rejected with run `ug gen`". It is not; it answers with `NOT INDEXED`
  coverage, as `~/.ug/MemOS` did during this audit. Pre-existing, not touched.
- **`name_mentions` is missing from that file's queryable-columns list.**
  Added in this pass along with `external_in_degree`.

---

## One GQL trap worth knowing

`Contains` **cannot be written as a relationship label**:

```
MATCH (f:File)-[:Contains]->(s) RETURN count(*)
  → GQL parse error at line 1, column 18: expected relationship label after ':'
```

It is lexed as the `CONTAINS` string operator keyword, so the parser never
sees an identifier. Backticks are not accepted either (`unexpected character
'`'`), nor is it usable inside an alternation. The only way to match the
Folder→File→Symbol structure edge is:

```
MATCH (f)-[r]->(s) WHERE type(r) = 'Contains' RETURN count(*)   -- 4,985
```

The error message points at the colon and says nothing about keywords, so the
natural reading is "this label does not exist in this graph" — which is false,
and sends you to check the indexer.

---

# Round 2 — 2026-09-21

Same method, widened: the presets were run against **three** indexes rather
than one — `~/.ug/ug` (Rust + JS), `~/.ug/MemOS` (Python + TypeScript,
12,789 nodes) and `~/.ug/java-demo` (Java). Two of the ten findings below are
invisible on a Rust repo and were found only by pointing the same questions
at a Python one.

## Summary

| # | What | Was | Now |
|---|---|---|---|
| 1 | Python docstrings never extracted | `has_doc = 0` for **all 3,449** Python functions | 2,204 documented |
| 2 | `IMPACT_EDGES` missing `Instantiates`, `Uses` | `impact` on the constants file: **94** dependents | **419** — understated 4.5× |
| 3 | Four different definitions of "a dependency edge" | presets, the two architecture presets, `walk`, `in_degree` | one, in `types.rs`, with a sync test |
| 4 | `impact` / `diff_impact` `test_paths` | counted **paths**: 453 beside 27 dependents | a DISTINCT symbol count, bounded by `dependents` |
| 5 | `biggest_files` counted markdown headings | a README outranked source files | code symbols + `code_lines` |
| 6 | `language_breakdown` counted headings and prose | markdown: 645 "symbols", 29,953 "code lines" | code only; docs go to `file_kinds` |
| 7 | `test_ratio` | `functions` included tests; ordered by folder size | `source`/`tests`/`test_pct`, least-tested first |
| 8 | `doc_coverage`, `comment_coverage` | no percentage | ratio column |
| 9 | Coverage-line denominator | the **whole graph**, always | the node set the query matched |
| 10 | Three docs: "Rust carries no `members`" | false — 107 of 311 Rust types carry one | corrected |

---

## 1. Python docstrings were never extracted, and seven presets reported a confident zero

`python.rs` called the shared `common::extract_docstring`. That function scans
the **200 bytes before a node** for a `/** … */` JSDoc block. Python does not
put its documentation above the symbol — a docstring is the first statement
*inside* the body — so the call returned `None` for every Python symbol ever
indexed.

Measured on MemOS, before:

```
lang         fns  documented
python      3449           0
typescript  3327         407
```

Zero. Not low — zero. What that did to the presets:

- `doc_coverage` / `doc_coverage_by_folder` reported every Python folder at
  **0.0%**, twelve rows of them, which reads as a finished measurement.
- `undocumented_hotspots` listed every Python symbol in the repo.
- `token_docs` could never return a Python row.
- `risky_symbols` (`has_doc = 0`) treated all Python as undocumented.
- **`where_to_start` requires `has_doc = 1`, so on a pure-Python repo it
  returned nothing at all.** On MemOS it returned only TypeScript — its first
  row was `initTestLogger`.

`doc_lines` followed, because `annotate_line_metrics` derives it from
`sym.docstring`.

**Fix.** `extract_python_docstring` in `python.rs`, beside the Rust and Java
equivalents that already existed: `body: block` → first `expression_statement`
→ `string`, preferring the grammar's `string_content` child so the quote style
(`"""`, `'''`, `"`, `r"""`) does not have to be guessed. Only the first
statement counts — a string later in a body is an expression.

After:

```
python      3449        2204        (64%)
Class        691         431        (was 12)
```

`where_to_start` on MemOS now opens with `TextualMemoryItem`, `BaseLLM`,
`BaseConfig` — the core types — instead of a test logger.

The existing test `a_function_carries_its_signature_docstring_and_metrics` is
**named** for the docstring and never asserted one. It does now, alongside a
new test covering the four quote forms, a class docstring, a method docstring,
and a string that is not the first statement.

Java (`extract_javadoc`) and Rust (`extract_rust_docstring`) were checked and
are correct: `java-demo` reports 16/16 classes documented.

---

## 2 & 3. Four definitions of "a dependency edge", and two of them were missing edges

`IMPACT_EDGES` is documented in two files, under the same name, with
near-identical prose about why `Contains` is excluded. There were **four**
implementations and no two agreed:

| | Calls | References | Imports | Extends | Implements | Overrides | Instantiates | Uses |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| `analyze::presets::IMPACT_EDGES` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | — | — |
| `coupling_matrix` / `layering_violations` | ✓ | ✓ | ✓ | ✓ | ✓ | — | — | — |
| `walk::IMPACT_EDGES` | ✓ | ✓ | — | ✓ | ✓ | ✓ | ✓ | — |
| `facts::FactContext` (`in_degree`) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

So `ug analyze dead_code` and `ug analyze impact` disagreed about what a
dependency is, and `ug walk` disagreed with both on the same commit.

`Instantiates` is a **resolved call site** — `graph::edge_for_call` emits it
instead of `Calls` when the callee is a constructor. `Uses` is a symbol reading
a constant. Both are ordinary dependencies.

**What it cost.** On a densely-called file the aggregate barely moves:
`types.rs` went 1,307 → 1,314 dependents, and the first two files measured
moved by 0. That nearly ended the investigation. The file where it decides the
answer is the one whose consumers all arrive by `Uses`:

```
native/src/style.rs   (the repo's colour and style constants)
  impact_summary, as shipped:  94 dependents across 51 files
  impact_summary, corrected:  419 dependents across 65 files
```

**4.5× understated, in the preset whose entire job is telling you how much a
change can break.** 294 symbols in this repo have no inbound edge of any other
kind, so every reachability preset reported "nothing reaches this" for all of
them.

**Fix.** One definition in `types.rs`, as a slice for `walk` and a
`impact_edges_gql!()` macro for the preset strings (`concat!` only takes
literals). `impact_edges_agree` asserts the two forms list the same labels, and
`structural_edges_are_not_dependencies` pins `Contains`/`Exports`/`DependsOn`/
`Requires` out — `Exports` would make a file a dependent of the symbols it
declares, the same double-count `Contains` is excluded for (§11b).

`untested_symbols` keeps its narrower `Calls|References` at 2 hops: it is the
one preset with an *unanchored* variable-length walk, the cap is the documented
reason, and widening it changed the answer by **zero** (1,381 either way).

Cost of the wider set on the anchored 3-hop walk: 0.79 s before, 0.79 s after.

---

## 4. `test_paths` counted paths and sat beside a count of symbols

`impact` returns `count(DISTINCT elementKey(dep)) AS dependents` — with a
long comment explaining that a variable-length match yields one row per
*path*, so a plain `count(*)` reports routes rather than dependents, "the
difference between 948 and 11". The very next line was `sum(dep.is_test) AS
test_paths`, which is that same mistake.

```
file                                     dependents  test_paths
native/tests/incremental_ingest_test.rs          27         453
```

453 test paths against 27 dependents. The column name is technically honest
and the number is uninterpretable — and a non-test file showed 204, because
Rust's inline `#[cfg(test)] mod tests` puts test symbols in source files.

**Fix.** `count(DISTINCT CASE WHEN dep.is_test = 1 THEN elementKey(dep) END)
AS tests`, in both `impact` and `diff_impact`. Now bounded by `dependents` and
readable as a fraction of it: `34 dependents, 20 tests`.

---

## 5 & 6. Markdown headings counted as code symbols

`presets.rs` defines `CODE_TYPES` with this comment:

> Markdown headings are indexed as `Concept` nodes — so a statistic that
> forgets to exclude them is wrong on any project with docs.

Two census presets forgot.

**`biggest_files`** filtered `node_type <> 'File' AND <> 'Folder'`, which keeps
`Concept`. "Where the mass is" ranked `PERF-TUNING-JOURNEY.md` (90 headings)
and two READMEs among the source files. It also had no size column, so 163
one-line helpers outranked five 300-line functions. Now `CODE_TYPES` plus
`sum(n.code_lines)`.

**`language_breakdown`** had both columns wrong at once:

```
markdown       645       29953      ← 645 headings, and 29,953 lines of prose
```

`line_metrics::syntax_for("markdown")` returns `NONE`, so every non-blank line
of a document counts as `code_lines`. A language census that lists markdown
beside rust invites exactly the comparison those numbers cannot support.
Restricted to `CODE_TYPES`; documents are counted by `file_kinds`, which can
count them honestly.

---

## 7. `test_ratio` ordered by folder size

Two problems, and the column name was the smaller one: `functions` counted
*all* functions including the tests, so `native/tests` read "459 functions, 459
tests" and every other row needed a subtraction before it meant anything.

The ordering was the defect. `ORDER BY functions DESC` ranks by folder size, so
a preset named `test_ratio` put the biggest folder first and buried the
untested ones — `native/src/graph` (46 source functions, 0 co-located tests)
sat twelfth. Now `source` / `tests` / `test_pct`, least-tested first, and the
untested folders are the first four rows.

The denominator is all functions rather than `source`, because a folder that is
entirely tests divides by zero — OverGraph rejects the query rather than
returning a number, which is how this was found.

The description now says the preset counts where tests **live**, so a repo with
a top-level `tests/` directory shows zeros beside its source folders, and
points at `untested_symbols` for reachability.

---

## 8. Two coverage censuses with no coverage figure

`doc_coverage` and `comment_coverage` returned `total` and `documented` and
left the reader to divide. Same family as round 1's §4 and §5, milder because
there are only two columns. Both now carry the percentage.

---

## 9. The coverage line's denominator was the whole graph

`coverage_for` probes with `MATCH (n) RETURN count(*), count(n.prop)` —
unconditionally every node in the graph. So a preset that narrows to a node
subtype reported its own properties as barely indexed:

| preset | reported | truth |
|---|---|---|
| `orphan_files` | `external_in_degree 4%` | 100% of the File nodes it looks at |
| `classes_by_members` | `members 2%` | 34% of the types it looks at |

`classes_by_members` then told the reader, in its own description, to "check
coverage" — pointing at a number that was wrong by more than an order of
magnitude. `coverage_for`'s doc comment already says this warning is the one
that tells a caller to distrust a number, "so crying wolf teaches them to
ignore the real thing".

**Fix.** `probe_scope` narrows the probe to the node set the query matched, for
the two shapes presets use: a label on a single `n` pattern (`MATCH (n:File)`)
and a `node_type IN [...]` list. Everything else falls back to the whole graph,
as before.

**It narrows by node *type*, never by the query's value predicates**, and that
is the load-bearing part. Reusing the whole `WHERE` would look more precise and
would destroy the warning: `where_to_start` filters on `has_doc = 1`, so a
denominator built from its own predicate reports `has_doc 100%` whatever the
graph holds. It is also why `boundaries` still reads `boundary_kinds 2%` — it
selects with `n.boundary = 1`, and honouring that would turn a real
"boundaries were never indexed" into a confident 100%.

---

## 10. Three places claimed Rust carries no `members`

`facts.rs`, the `classes_by_members` description and `docs/ANALYZE.md` all
said a Rust struct's methods live in a separate `impl` block, so Rust types get
no `Contains` edges and no `members` fact.

They do:

```
MATCH (a)-[r]->(b) WHERE type(r) = 'Contains' AND a.name = 'Db'
  → Db::open, Db::open_or_create, Db::recorded_model, …
```

**107 of this repo's 311 Rust types carry a member count.** What is genuinely
absent is a type nobody wrote an `impl` for — `GraphNode`, `Symbol`, a request
body — which is a real answer about a real thing, still stored as *absent*
rather than `0`.

Combined with #9, this preset shipped a correct answer with a description and a
caveat that both told the caller to throw it away.

---

## Rejected, round 2

| Proposal | Why not |
|---|---|
| Widen `untested_symbols` to `Instantiates`/`Uses` | Changed the answer by zero (1,381 → 1,381) and it is the one unanchored variable-length walk, where the frontier cap is a documented failure. |
| Exclude parent/child folder pairs from `coupling_matrix` | 1,899 of 2,865 cross-folder edges are parent/child, which looked like noise — but `folder` is the *immediate* parent, so `native/src/cli → native/src` is the CLI depending on the top-level modules. That is real coupling, not structure. |
| A `tests_per_source` ratio for `test_ratio` | Divides by zero on a folder that is all tests, and OverGraph refuses the query. Percentage of all functions instead. |
| Reuse the query's full `WHERE` as the coverage denominator | Reports `has_doc 100%` for `where_to_start`, which filters on `has_doc`. The denominator has to be a node set, not a filtered set. |
| `MATCH (n:Class|Interface)` to make label-scoped coverage cover more presets | The engine has no label union; `:Class:Interface` parses as a conjunction and returns 0. |
