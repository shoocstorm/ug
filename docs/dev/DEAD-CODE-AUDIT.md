# Dead-code audit — 2026-09-20

> **Updated 2026-09-21.** §5 records the indexer work that followed. The
> candidate list this audit was built from went from 462 rows to 24 without
> losing one of the eight real findings. §1–§4 are left exactly as written:
> their numbers are what motivated the change, and §5 supersedes the counts
> in §3.

What nothing in this repository references any more, checked exhaustively
rather than sampled, plus what the check revealed about the `dead_code`
preset that was supposed to answer it.

Index at the time of the audit: 212 files, 5 725 nodes, 15 454 edges.

---

## 1. The answer

### Confirmed dead — nothing names these but their own definition

Every one verified by hand: the identifier appears exactly once across all
`.rs`, `.js`, `.ts` and `.html` in the repo, on its own declaration line.

- **`native/src/types.rs:277` — `struct TypeRef`** *(4 loc)*
  Never constructed, never a field type, never a parameter. `GraphTypeRef`
  below is a byte-for-byte duplicate of it.
- **`native/src/types.rs:795` — `struct GraphTypeRef`** *(4 loc)*
  Identical shape to `TypeRef` (`name: String, generic: Option<String>`).
  The pair looks like one survived a rename and the other survived the
  revert.
- **`native/src/storage/db.rs:617` — `Db::try_create_vector_index`** *(3 loc)*
- **`native/src/storage/db.rs:712` — `Db::try_create_fts_index`** *(3 loc)*
  Both `pub async fn`, so the compiler cannot warn. `ensure_fact_indexes`
  is what actually declares indexes now.
- **`native/src/storage/ingest.rs:726` — `upsert_only_nodes`** *(6 loc)*
  Already carries `#[allow(dead_code)]` and a comment saying "helper kept
  for backwards compat". Nothing has called it since.
- **`native/src/serve/registry.rs:94` — `ServeStores::primary_store`** *(5 loc)*
  Already carries `#[allow(dead_code)]` and "reserved for future routes".
  `pick_store` covers every handler.
- **`native/src/analyze/presets.rs:37` — `pub const CODE_TYPES`** *(1 loc)*
  Documents the node types that count as code, and no preset uses it —
  including `dead_code`, which had the narrower list inlined and so could
  not see its own unused constant. A `pub const` in a `pub mod`, invisible
  to both rustc and the old preset.
- **`native/src/vis/js/10-render-core.js:625` — `nodeScreenPos`** *(1 loc)*
  A one-line wrapper over `R.screenPos(n)`. No caller.

Two more are unreferenced but deliberately so, and should stay:

- `native/src/vis/js/02-dialogs.js:915` — `[Symbol.iterator]`, part of the
  Map-shaped surface `NodeStore` presents. Called by `for…of`/spread, which
  no static reference can show.
- `native/build.rs:main`, `native/src/bin/ug_app.rs:main` — entry points.

### Documentation nothing links to

Six markdown files that no other tracked file names, by path or by
basename:

- `docs/dev/EVALUATION-2026-07-23.md`
- `docs/dev/SESSION-2026-07-21.md`
- `docs/dev/SESSION-2026-07-26.md`
- `docs/dev/SESSION-2026-07-27.md`
- `docs/ug-website/docs/SLIDES-BRIEF.md`
- `docs/ug-website/docs/slide_style_guide.md`

Dated session notes are arguably meant to be unlinked. The two
`ug-website` briefs are not obviously in that category.

### Clean

- **Rust symbols.** `cargo check --all-targets` is warning-free. No
  `dead_code`, `unused_variables` or `unused_imports` lint fires anywhere.
- **Cargo dependencies.** Every entry in `native/Cargo.toml` is named in
  at least one `.rs` file. No unused crates.
- **JS.** Every function in `native/src/vis/js/*.js` has a caller except
  `nodeScreenPos`. The `12-render-cosmos.js` position builders
  (`cosmosGridPositions` and friends) are reached through the layout table,
  `wirePalette`/`openContextTab`/`cycleNeighbor` from wiring and command
  entries.
- **Files.** `orphan_files` reports 144; all are false positives —
  integration-test targets that Cargo discovers by directory, `mod`
  declarations the Rust indexer does not draw `Imports` edges for, and the
  `vis/js/*.js` files `build.rs` concatenates by glob.

### Method

1. `ug gen`, then every node with zero non-`Contains` in-degree — 2 070 of
   5 725.
2. For each, count occurrences of its short name as a whole word across all
   tracked code. Exactly one occurrence means the definition line and
   nothing else.
3. `cargo check --all-targets` for the compiler's own view of private and
   `pub(crate)` items.
4. Every dependency name against the source; every markdown basename
   against every tracked file.

Step 2 is deliberately conservative: a name that collides with a live
symbol elsewhere is cleared. That hid exactly one real item —
`[Symbol.iterator]`, whose short name `iterator` appears in prose.

---

## 2. What was wrong with the `dead_code` preset

The old query:

```gql
MATCH (n) WHERE n.in_degree = 0 AND n.is_test = 0
  AND n.node_type IN ['Function', 'Class', 'Interface']
RETURN elementKey(n) AS id, n.loc AS loc ORDER BY loc DESC LIMIT 200
```

**462 rows. 8 of them dead. 1.7% of the list was worth reading**, and
because rows are ordered by `loc`, the eight real ones — all under 7 lines
— sat at the very bottom, past a 200-row display cap they could not even
reach.

Three distinct defects:

### 2.1 `in_degree = 0` means "the resolver drew no edge", not "nothing uses this"

This is the big one: 454 of the 462 rows. The preset's own comment
acknowledged dynamic dispatch in the abstract, but the false-positive rate
it produces in practice is not a caveat, it is the entire result. Measured
breakdown of the 441 non-dead rows in the `Function`/`Class`/`Interface`
set:

| mechanism | rows |
|---|---|
| Rust free fn or type, referenced but unresolved | 158 |
| Rust inherent method, called through `.` | 141 |
| JS member or function-table reference | 76 |
| Rust trait-impl method, dispatched through the trait | 55 |
| boundary entry point (nothing *should* call it) | 3 |

Concretely: `serde`'s `deserialize_with = "de_edges_interned"` is a string,
`.route("/api/graph/search", get(api_search))` passes a function as a
value, `impl Display for StoreError { fn fmt }` is reached through
`Display`, and `R.mount(...)` in the vis layer is a member call on an
object literal. All four have an in-degree of zero. All four are live.

### 2.2 `is_test = 0` was reading a stale fact, not a wrong one

The first run of this audit returned 96 `#[tokio::test]` functions from
`src/**/*_tests.rs`. `is_test_node` handles `tokio::test` correctly — but
facts are written at ingest, and an incremental `ug gen` had left the
untouched files carrying facts from a build that predated the fix. The
symptom was a preset producing rows it should never have produced; the
cause was upstream of the preset, and a full re-ingest cleared it. Worth
recording because "the preset is wrong" and "the facts are old" look
identical from the output.

### 2.3 The node-type list could not see a dead constant

`Constant` and `Variable` were excluded, so a `pub const` nothing
references was invisible. `CODE_TYPES` — the repo's own documented list of
"node types that are code", which *does* include `Constant` — was sitting
unused three hundred lines above the preset that needed it.

---

## 3. The refinement

New in `facts.rs`: **`name_mentions`**, a stored per-node fact counting how
many times a symbol's short name is written down anywhere else in the
graph *before* resolution — every callee name, implemented trait, imported
item, parameter and return type, and every word of prose, attributed to the
node that wrote it and skipping the node's own name.

It is the question `in_degree` was standing in for. A trait method reached
through `dyn` has no inbound edge, but the call site still wrote `fmt`; a
`serde` payload is never called, but somebody's signature still says
`Json<SearchArgs>`.

The preset is now:

```gql
MATCH (n) WHERE n.in_degree = 0 AND n.name_mentions = 0 AND n.is_test = 0
  AND n.node_type IN ['Function', 'Class', 'Interface', 'Constant']
RETURN elementKey(n) AS id, n.loc AS loc ORDER BY loc DESC LIMIT 200
```

Verified against the hand-checked list above:

| | old | new |
|---|---|---|
| rows | 462 | **111** |
| confirmed dead among them | 8 | **8** |
| precision | 1.7% | **7.2%** |
| rows to read before seeing all 8 | 462 (capped at 200 — unreachable) | 111 |

4.2× fewer candidates, nothing lost, and the whole list now fits inside the
display cap. `CODE_TYPES` is found because `Constant` joined the list.

**Why both conjuncts.** `name_mentions` alone clears any symbol whose short
name collides with a live one; `in_degree` alone is the 1.7%.

**Why `Variable` is still excluded.** It is JS module state
(`perfHud`, `insState`, `changesState`), which this indexer draws no `Uses`
edges for. It added 78 rows and not one true positive.

### What still gets through

The remaining 103 false positives are all the same shape: a name written
down in a form the fact cannot see.

- `serde` attribute strings — `default = "default_hybrid_k"` is a string
  literal in an attribute, not a call or a type.
- Cross-file constant references — `QUERYABLE_PROPERTIES` is used from two
  other files and the Rust indexer draws no `Uses` edge across a file
  boundary, so both signals miss it.
- JS event wiring — `closeSettings`, `gotoTour` and friends are attached to
  handlers rather than called.
- Struct literals for types the indexer resolved as `Instantiates` from a
  different spelling (`RustIndexer`, `JavaIndexer`, …).

Closing those means teaching the indexer to record attribute-string and
cross-file constant references. Until then the preset says *candidates*,
and means it.

### Known one-directional error

`name_mentions` is keyed by short name, not by node id, because an
unresolved mention is only ever a name — that is what makes it unresolved.
Two symbols sharing a short name share a count. The error only ever hides a
dead symbol behind a live one, never the reverse, which is the right way to
be wrong for a list someone has to read. It cost exactly one true positive
here: `[Symbol.iterator]`, cleared by the word `iterator` in a doc comment.

---

## 4. Reproducing

```bash
ug gen                        # facts are written at ingest; stale facts lie
ug analyze dead_code --range 1-111
```

The `coverage:` line must show `name_mentions` near 100%. If it reports
`NOT INDEXED`, the store predates this fact and every predicate on it
matched nothing — re-run `ug gen` rather than believing the empty answer.

---

## 5. Closing the gaps in the indexer

§2.1 blamed `in_degree = 0` for being a weak signal and §3 worked around it
with `name_mentions`. Both were true, but neither asked the better question:
*why* was a call site invisible in the first place? Every false positive in
§3 was a name written down in a position the AST walk never visited. Seven of
those positions are now visited.

### What was invisible, and why

| # | Gap | Why the walk missed it | Fix |
|---|---|---|---|
| 1 | **Calls inside macros** | A macro's arguments are a `token_tree` of loose tokens, not code. `println!("{}", render(n))` contains no `call_expression`, so the call to `render` was not in the tree at all. | Walk the token stream against the source: an identifier followed by `(` is a call, one preceded by `.` is an untypable method call and stays dropped. |
| 2 | **Inline format captures** | Since Rust 2021 `format!("{WIDTH}")` reads `WIDTH`, and `{:<CMD_W$}` reads `CMD_W`. String literals were skipped wholesale as prose. | Parse `{…}` placeholders; record only `SCREAMING_CASE` names, because a placeholder far more often names a local. |
| 3 | **Types in type position** | A data type is never called — it is a parameter, a return, a field. Nothing in the pipeline read type positions, so every `serde` payload, params struct and response DTO had an in-degree of zero. | New `Symbol::type_refs`, from parameters, return types, struct fields, enum payloads, constant types and alias targets, resolved to `References`. |
| 4 | **`serde` attribute strings** | `#[serde(default = "default_hybrid_k")]` names a function inside a string literal, on a *field* rather than on the item. Not a call, not a value, not a type. | Read `key = "path"` pairs from item- and field-level attributes for the eight `serde` keys that take a function. |
| 5 | **Glob-imported constants** | `use super::*` leaves `resolve_path` nothing to compose, so it re-roots the name under the *reading* module and matches nothing. | Offer the glob candidate too, exactly as value references already did. |
| 6 | **Module-scope code** | A browser bundle does its whole wiring at top level — `wirePalette();`, a listener calling `gotoTour()`, `run: openContextTab` in a command table. None of it is inside a function, so there was no symbol to hang it on. | New `FileNode::module_refs`, resolved to edges out of the **File** node, which is whose code it is. |
| 7 | **Associated constants** | `Kind::ALL` was registered as `crate::types::ALL`, sharing no path suffix with the spelling every reader uses — and leaving two `ALL` constants indistinguishable. | Qualify with the owning `impl`. Display names and node ids are unchanged. |

Also closed on the way: cross-file constant reads (`uses` got the
`by_path_suffix` fallback that value references already had), types named in
local bindings and `static`s inside functions, and `key: handler` entries in
object literals inside functions.

### What it did to the graph

| | before | after |
|---|---|---|
| edges | 15 454 | **18 783** (+21.5%) |
| `Calls` | 6 632 | **7 483** |
| `References` | 1 273 | **3 506** (×2.8) |
| `Uses` | 892 | **985** |
| symbols with no inbound edge | 2 070 | **1 871** |

Correctness was sampled rather than assumed: 120 random `Calls` / `References`
/ `Uses` edges were checked against the source of the symbol they leave.
Every one names its target. (Two apparent misses were an aliased import,
`use … as ENV_GUARD` — a correct edge the text check cannot see.)

Indexing cost: the extract phase went from 47 ms to 117 ms on this repo;
total `ug gen` is unchanged at ~2.5 s, which is dominated by everything else.
A full re-index is byte-identical run to run.

### What it did to `dead_code`

| | old indexer | new indexer |
|---|---|---|
| `in_degree = 0` alone | 462 | 292 |
| **`+ name_mentions = 0`** | 111 | **24** |

Still the same 8 findings — recall never moved. Precision went from 1.7% to
33%, and the list is now short enough to read in full.

Both halves were needed and neither subsumes the other: the indexer work
alone gets to 292, the preset work alone to 111.

### One thing that got worse first

Writing §1 of this document *erased its own findings*. Markdown headings are
indexed as `Concept` nodes whose docstring is the section body, so recording
"`TypeRef` is dead, nothing references it" put that name in a docstring,
lifted its `name_mentions` to 1, and dropped it out of `dead_code`. Every
symbol on the list vanished on the commit that wrote the list down.

`name_mentions` now counts mentions from code symbols only. Prose *about* a
codebase is not a use of it. The same trap still bites one name at a time in
code — a doc comment naming a dead symbol revives it — which is intended, and
is why an example in a doc comment should never use a real symbol's name.

### What still gets through

The remaining 16 false positives are three shapes:

- **Cross-file bare value references in the JS bundle** — `lanes.filter(walkLaneRevealed)`
  and `{ run: downloadGraph }` where the target is declared in another file of
  the concatenated bundle. Resolving these means matching a bare name across
  files, which `resolve_call_edges` deliberately refuses for value references:
  it would let a loop variable named `call` reference any function in the repo
  spelled the same way. That trade is still the right one; it is recorded here
  rather than changed.
- **Methods on receivers the local type environment could not infer** —
  `Staleness::kb_kind`, `LineRange::overlaps`, `Neo4jStore::db_name`.
- **A genuinely ambiguous name** — two different `delay` functions in the vis
  layer, where resolving to either would be a guess.

The honest floor is roughly this: the indexer can see every name that is
*written*, and cannot see a name that is *computed*. Everything above was the
first category.
