---
name: ug
description: >-
  Answer any question about an existing codebase or docs set by querying its
  `ug` (UltraGraph) knowledge graph from the CLI. Use this BEFORE grep, glob
  or reading files whenever the task is to understand code rather than edit a
  file you have already located: how does X work, where is X defined, who
  calls or imports X, what breaks if I change X, how are A and B connected,
  where do I start in this repo. Also use for every count, fraction, ranking
  or distribution over the repo (how many functions are undocumented, which
  files are biggest, what is dead or untested) — one `ug query` replaces a
  loop of greps. Also use whenever a question spans a FAMILY of symbols or
  files rather than one — every handler, all the `*Controller` classes, each
  `test_*`, everything under `src/auth/` — because `ug` takes wildcards
  (`*` `?` `[abc]` `{a,b}`) wherever a symbol or file is named, so one call
  replaces a loop. Also triggers on: ug, UltraGraph, `ug query`, blast
  radius, impact analysis, dead code, repo statistics, "which files depend
  on", "all the functions named like", "every file matching".
---

# ug — codebase knowledge graph from the CLI

`ug` indexes a repo (code *and* prose docs) into a graph of symbols and edges.
Prefer it over `grep`/`Read` for anything relational or aggregate: those see
text, `ug` sees the graph.

Two rules turn a loop into one call — reach for them before iterating:

> **1. For any count, fraction, ranking, distribution or blast-radius question,
> run one `ug query`.** Never grep for a count; never loop a per-file command to
> build one. "How many functions are over 100 lines, and where do they cluster?"
> is `ug query long_functions_by_folder` — one call.
>
> **2. For any question about a *family* of symbols or files, use a wildcard.**
> Every command that names a symbol or file takes `* ? [abc] {a,b}`. "Who calls
> any of the validators?" is `ug find_usages 'validate_*'` — one call, not one
> per validator. See [Wildcards](#wildcards--one-call-instead-of-a-loop).

If the `ultragraph` MCP tools are also connected you don't need them: same
engine, same parameter names, richer `--help`.

## Routing — which command do I run?

| Question | Command |
|---|---|
| How many / what fraction / biggest / what breaks? | `ug query <preset>` — catalog below |
| You only have a concept, not a name or path | `ug search "concept"` or `ug find_symbols <word>`, then feed the id/path into the command you wanted |
| Where is `foo`? (you know the name) | `ug find_symbols foo` |
| Every symbol in a family / naming convention | `ug find_symbols 'handle_*'` — see Wildcards |
| How does <vague concept> work? | `ug search "concept" -k 8` |
| What's in this file? | `ug file_outline path/to/f.rs` |
| What's in this directory / subtree? | `ug file_outline 'src/**/*.ts'` |
| Read the source | `ug get_code <symbol>` · `ug get_code -f file --range 10-60` |
| Who calls / imports / implements this? | `ug find_usages <symbol>` |
| What does this depend on? | `ug traverse <symbol> -k 1` (widen only if needed) |
| How are A and B connected? | `ug shortest_path A B` |
| Where do I start in this repo? | `ug project_overview`, `ug query where_to_start` |
| Central symbols · dependency cycles | `ug graph_centrality` · `ug graph_cycles` |

**Confusable properties** — these decide whether a count answers the question
you actually asked:

- **`has_comments`** (any prose — doc *or* inline) vs **`has_doc`** (doc comments
  only). Much code is explained inline only, so `has_doc` undercounts.
  `comment_coverage` uses `has_comments`; `doc_coverage` uses `has_doc`.
- **`code_lines`**, not `loc` — a span including blanks and comments. Use
  `long_functions_by_code` for true code length.
- **`members`** is populated only for languages that nest members in the type
  body (Java, Python, TS) — check coverage before ranking on it.

Only `search` and `semantic_search` need an embedder; `query` and `traverse`
need the db. Everything else reads `graph.json` — so an embedding error is
never a dead end.

Node ids are `kind:file:name` — **no line number**, e.g.
`function:path/to/file.rs:symbol_name`. Anywhere an id is expected you may
instead pass a **bare symbol name** or a **wildcard**, so a `find_symbols`
round trip is optional, not a prerequisite. `find_symbols`, `file_outline`,
`get_code`, `find_usages` and `traverse` also take **several arguments in one
call** — batch instead of looping:

```bash
ug find_symbols run_gen run_serve run_ingest
ug find_usages connect                     # no id lookup needed
```

## Wildcards — one call instead of a loop

`* ? [abc] [a-z] [!ab] {a,b}` work wherever a symbol or file is named:
`find_symbols` (name, `--node-type`, `--file-prefix`), `file_outline`, and the
symbol arguments of `get_code`, `find_usages`, `traverse`, `shortest_path`.
**Quote them** — the shell expands `*` otherwise.

```bash
ug find_symbols 'handle_*'                    # every handler
ug find_symbols '*Controller' --node-type Class
ug find_symbols '*' --file-prefix 'src/auth/**' -k 100   # a whole subtree
ug file_outline 'src/**/*.{ts,tsx}' -k 40     # survey many files at once
ug find_usages 'validate_*'                   # blast radius of a family
ug traverse 'handle_*' -d inbound             # one merged walk
```

A pattern matches the **whole** name (`auth*` finds `authorize`, `*auth*`
finds `reauth`); in paths `*` stops at `/` and `**/` crosses directories. In
the id-taking commands a pattern expands to at most 25 symbols and says so
when it hits that — narrow the pattern rather than trusting a capped answer.
`shortest_path` needs each endpoint to match exactly one symbol.

## `ug query`

### Preset catalog — pick directly, no `--list` needed

- **census** — `repo_census`, `biggest_files`, `language_breakdown`, `file_kinds`, `where_to_start`
- **size** — `long_functions` `[min_loc]`, `long_functions_by_folder` `[min_loc]`, `long_functions_by_code` `[min_loc]`, `size_histogram`, `god_classes`, `classes_by_members`, `param_bloat` `[min_params]`, `deep_nesting` `[min_depth]`
- **documentation** — `comment_coverage`, `comment_density`, `doc_coverage`, `doc_coverage_by_folder`, `token_docs`, `undercommented_complexity`, `undocumented_hotspots`
- **dead code** — `dead_code`, `orphan_files`, `duplicate_names`
- **architecture** — `dependency_fanin`, `fanout_offenders` `[min_fanout]`, `coupling_matrix`, `layering_violations` `[from_prefix, to_prefix]`
- **tests** — `test_ratio`, `untested_symbols`, `retest_scope` `[target]`
- **risk** — `impact` `[target]`, `impact_summary` `[target]`, `risky_symbols`

Args in `[brackets]` are passed `--arg key=value` (repeatable). The catalog
moves between versions — re-read `ug query --list` if `ug --version` mismatches.

### Usage

```bash
ug query long_functions --arg min_loc=150            # preset + arg (repeatable)
ug query impact --arg target=path/to/file.rs
ug query --gql "MATCH (n:Function) WHERE n.params > 6 \
  RETURN n.folder AS f, count(*) AS c ORDER BY c DESC"
```

**`--arg target=` takes a repo-relative file path** — a concept string
silently returns nothing. When the user names a concept rather than a path,
look the path up yourself — never ask them for it:

```bash
ug find_symbols queue          # → …:src/…/queue/AbstractQueue.java:…
ug query impact --arg target=<resolved path>
```

**Page with `--range 21-40` / `--range 34-end`, not a bigger `-k`.** It windows
rows the query already produced, so totals never move and you never re-read
what you've seen. Each answer prints the next range to ask for.

**Three ways to get a wrong zero** — all return `0`, not an error:

- Aggregate over a property nothing carries → read the `coverage:` footer; treat `NOT INDEXED` as "about nothing".
- `--arg target=` path matches no indexed file (paths are repo-relative) → confirm with `ug file_outline`.
- `-t/--edge-type` filter on a type this graph lacks → `ug graph_schema` first.

**Reachability presets can hit the traversal cap** on hub files (`impact`,
`retest_scope`, `untested_symbols` walk variable-length paths). The error names
the fix; fall back to `impact_summary` or `find_usages <id> -k 2`.

### Hand-written GQL

Prefer a preset when one fits. When writing GQL by hand: booleans are `0`/`1`
so they can be summed (`sum(n.has_doc)` over `count(*)` is the documented
fraction); variable-length paths need a finite bound (`*1..3`, never `*`), and
unanchored walks past 2 hops blow the cap; an `EXISTS { … }` subquery needs its
own `RETURN`. Docs are `Concept` nodes, so GQL over `:Concept` asks about the
prose rather than the code.

## Let the CLI teach you — don't guess flags

```bash
ug --help          # every command, grouped by what it needs (db vs graph.json)
ug <command> -h    # flags AND worked examples — cheap; read it instead of guessing
ug query --list    # every built-in question, by category, with args (full detail)
ug graph_schema    # node/edge types actually in THIS graph, + the full vocabulary
```

`query --list` and `graph_schema` change per repo/version — re-read them rather
than remembering them.

Not indexed yet → `ug gen` at the repo root. Stale (line numbers don't match the
file) → `ug regen`; it's incremental, so cheap. `ug list` shows what exists.
Graph-backed commands default to the **cwd basename**, db-backed ones to the
**active project** — away from the repo root, pass `-n <project>`.

## Traps

- `ug search` for a name you already know → `find_symbols` (no embeddings).
  For a file's contents → `file_outline`.
- Looping a command over a family of symbols or files → one wildcard call.
- An unquoted `'*'` — the shell expands it before `ug` sees it.
- A wildcard that matches nothing is usually anchoring: patterns must cover the
  whole name (`*auth*`, not `auth`), and `*` does not cross `/` in a path.
- Judging code from a search snippet → `get_code` for the whole symbol.
- Reporting `dead_code` / `untested_symbols` as fact → candidates only;
  reflection, dynamic dispatch and macros are invisible to the graph.
