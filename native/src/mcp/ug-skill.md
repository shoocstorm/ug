---
name: ug
description: |
  Use the `ug` CLI (UltraGraph) to answer questions about a codebase or docs
  set from its knowledge graph instead of grepping and reading files. Use for
  "where is X / who calls X / what breaks if I change X", for any count,
  fraction, distribution or ranking over a repo, and for orienting in an
  unfamiliar codebase. Trigger on: ug, UltraGraph, `ug query`, blast radius,
  dead code, repo statistics, "which files depend on".
---

# ug — codebase knowledge graph from the CLI

`ug` indexes a repo (code *and* prose docs) into a graph of symbols and edges.
Prefer it over `grep`/`Read` for anything relational or aggregate: those see
text, `ug` sees the graph.

> **For any count, fraction, ranking, distribution or blast-radius question, run
> one `ug query`.** Never grep for a count; never loop a per-file command to
> build one. "How many functions are over 100 lines, and where do they cluster?"
> is `ug query long_functions_by_folder` — one call.

If the `ultragraph` MCP tools are also connected you don't need them: same
engine, same parameter names, richer `--help`. Paths in examples below are from
`ug`'s own repo — they show an argument's shape; substitute your own.

## Let the CLI teach you — don't guess flags

```bash
ug --help          # every command, grouped by what it needs (db vs graph.json)
ug <command> -h    # flags AND worked examples — cheap; read it instead of guessing
ug query --list    # all 33 built-in questions, by category, with their args
ug graph_schema    # node/edge types actually in THIS graph, + the full vocabulary
```

The last two change per repo — re-read them rather than remembering them.

Not indexed yet → `ug gen` at the repo root. Stale (line numbers don't match the
file) → `ug regen`; it's incremental, so cheap. `ug list` shows what
exists. Graph-backed commands default to the **cwd basename**, db-backed ones to
the **active project** — away from the repo root, pass `-n <project>`.

## Routing

| Question | Command |
|---|---|
| How many / what fraction / biggest / what breaks? | `ug query <preset>` — below |
| Where is `foo`? (you know the name) | `ug find_symbols foo` |
| How does <vague concept> work? | `ug search "concept" -k 8` |
| What's in this file? | `ug file_outline path/to/f.rs` |
| Read the source | `ug get_code <id>` · `ug get_code -f file -s 10 -e 60` |
| Who calls / imports / implements this? | `ug find_usages <id>` |
| What does this depend on? | `ug traverse <id> -k 1` (widen only if needed) |
| How are A and B connected? | `ug shortest_path A B` |
| Where do I start in this repo? | `ug project_overview`, `ug query where_to_start` |
| Central symbols · dependency cycles | `ug graph_centrality` · `ug graph_cycles` |

Only `search` and `semantic_search` need an embedder; `query` and `traverse`
need the db. Everything else reads `graph.json` — so an embedding error is never
a dead end.

Node ids are `kind:file:name` — **no line number**, e.g.
`function:native/src/main.rs:run_code_query`. `find_usages` and `get_code` need
a real id (from `find_symbols`/`file_outline`/`search`); the others also take a
bare name or path. `find_symbols`, `file_outline`, `get_code`, `find_usages` and
`traverse` take **several arguments in one call** — batch instead of looping:

```bash
ug find_symbols run_gen run_serve run_ingest
```

## `ug query`

```bash
ug query long_functions --arg min_loc=150            # preset + arg (repeatable)
ug query impact --arg target=native/src/storage/store.rs
ug query --gql "MATCH (n:Function) WHERE n.params > 6 \
  RETURN n.folder AS f, count(*) AS c ORDER BY c DESC"
```

Check `--list` before writing GQL — presets cover census, size, documentation,
dead code, architecture, tests and risk, and are the cheaper path.

**Page with `--range 21-40` / `--range 34-end`, not a bigger `-k`.** It windows
rows the query already produced, so totals never move and you never re-read what
you've seen. Each answer prints the next range to ask for.

**Three ways to get a wrong zero.** Aggregating over a property nothing carries
returns `0`, not an error — read the `coverage:` footer, and treat `NOT INDEXED`
as "this number is about nothing". A `--arg target=` path matching no indexed
file also returns `0`; paths are repo-relative, confirm with `ug file_outline`.
And filtering `-t/--edge-type` on a type this graph lacks silently matches
nothing — `ug graph_schema` first.

**Reachability presets can hit the traversal cap** on hub files (`impact`,
`retest_scope`, `untested_symbols` walk variable-length paths). The error names
the fix; fall back to `impact_summary` or `find_usages <id> -k 2`.

Confusable properties: **`code_lines`** not `loc` (a span, includes blanks and
comments); **`has_comments`** not `has_doc` (which only sees doc comments — much
code is explained inline only); **`members`** is populated only for languages
that nest members in the type body, so check its coverage before ranking on it.

Hand-written GQL: booleans are `0`/`1` so they can be summed (`sum(n.has_doc)`
over `count(*)` is the documented fraction); variable-length paths need a finite
bound (`*1..3`, never `*`), and unanchored walks past 2 hops blow the cap; an
`EXISTS { … }` subquery needs its own `RETURN`. Docs are `Concept` nodes, so GQL
over `:Concept` asks about the prose rather than the code.

## Traps

- `ug search` for a name you already know → `find_symbols` (exact, no embeddings).
  For a file's contents → `file_outline`.
- Judging code from a search snippet → `get_code` for the whole symbol.
- Reporting `dead_code` / `untested_symbols` as fact → candidates only;
  reflection, dynamic dispatch and macros are invisible to the graph.
