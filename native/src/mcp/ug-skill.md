---
name: ug
description: >-
  Answer any question about an existing codebase or docs set — and check the
  safety of your own edits to it — by querying its `ug` (UltraGraph)
  knowledge graph from the CLI. Use this BEFORE grep, glob or reading files
  whenever the task is to understand code rather than open a file you have
  already located: how does X work, where is X defined, who calls or imports
  X, what breaks if I change X, how are A and B connected, where do I start
  in this repo. When the question is about ONE symbol, `ug context <symbol>`
  answers all of that in a single budgeted call — its source, its callers with
  call sites, the tests that reach it, its dependencies and its linked docs —
  instead of a get_code + find_usages + traverse + test_for sequence. Use it
  WHILE editing too: before changing a symbol, ask who
  depends on it (`ug context`, `ug find_usages`, `ug analyze boundary_impact`); after
  changing files, ask what that broke and what to re-test (`ug analyze
  diff_impact`, `ug analyze diff_retest_scope`) — run `ug update <file>...`
  on the files you just edited first, because git hooks only refresh the
  graph at commit boundaries and structural answers otherwise describe your
  code as it was before the edit. Also use for every count, fraction, ranking
  or distribution over the repo (how many functions are undocumented, which
  files are biggest, what is dead or untested) — one `ug analyze` replaces a
  loop of greps. Also use whenever a question spans a FAMILY of symbols or
  files — every handler, all the `*Controller` classes, each `test_*`,
  everything under `src/auth/` — because `ug` takes wildcards (`*` `?`
  `[abc]` `{a,b}`) wherever a symbol or file is named. Also use to find
  where a system meets the outside world (REST endpoints, queue listeners,
  CLI commands, scheduled jobs, outbound HTTP/DB/queue clients) and whether
  a change is visible beyond the repo. Also triggers on: ug, UltraGraph,
  `ug analyze`, blast radius, impact analysis, dead code, repo statistics,
  "which files depend on", "all the functions named like", "every file
  matching", system boundary, entry point, API surface, "what endpoints does
  this expose", "is this a breaking change", "what did my change break",
  "what should I re-test", "is it safe to change this", "did I miss a caller",
  "give me context on", "everything about this function", "walk me through this
  symbol", "what do I need to know before changing X", "what changed", "what
  did this commit do", "review this branch", "walk me through this diff",
  "summarise my changes", "what am I in the middle of".
---

# ug — codebase knowledge graph from the CLI

`ug` indexes a repo (code *and* prose docs) into a graph of symbols and edges.
Prefer it over `grep`/`Read` for anything relational, aggregate, or spanning a
family of symbols: those see text, `ug` sees the graph.

It is for editing work, not only reading — it answers *who depends on this?*
before you touch a symbol, and *what did I just break / what do I re-test?*
after, which grep cannot. Use it on your own edits, not just unfamiliar code.

## Editing workflow

```bash
# BEFORE touching a symbol — the whole picture in ONE call
ug context <symbol>                            # code + callers + tests + deps + docs

# Handed a FILE instead of a symbol? The same idea, one call
ug file_context <path>                         # outline + importers + tests + blast radius

# …or the individual questions, if you only need one
ug find_usages <symbol>                        # callers, importers, call sites
ug analyze boundary_impact --arg target=<file> # visible outside the system?

# AFTER an edit burst — blast radius + tests to re-run
ug analyze diff_impact --arg files=$(git diff --name-only | paste -sd, -)
ug analyze diff_retest_scope --arg files=a.ts,b.rs

# ORIENT in a change — yours, or someone else's
ug walk                                        # what is uncommitted right now
ug walk HEAD                                   # review the last commit
ug walk main...HEAD                            # everything this branch changed
```

Git hooks re-index only at commit boundaries, so across an edit burst the graph
describes your code as it was *before* you started. **So after you edit files,
run `ug update <file>...` before asking anything structural about them.** Every
structural command tells you when it answers from a stale index, on stderr:

```
⚠ index is behind the tree · 2 changed of 418 indexed files · src/a.ts, src/b.rs
  Refresh: ug update <file>... (fast) or ug gen -n ug.
```

If the named files are ones you just edited, refresh and ask again; otherwise
carry on. `ug get_code` needs none of this — it always reads the live working
tree, so a stale index shows up there as an unresolvable id, never silently
wrong source. If the MCP `ultragraph` tools are connected, the post-edit
refresh is the `gen` tool with `files: [...]` — same job as `ug update`, and it
reports each file's symbol count, so an unparseable file shows as `0 symbols`
rather than an empty answer you would have believed.

## `ug context` — one call instead of five

When you are about to change a symbol, or need to understand one properly, this
is the first thing to run. It returns the symbol's source, its direct callers
**with call sites**, the tests that reach it, what it depends on, and any prose
linked to it — replacing the sequence you would otherwise run by hand:

```bash
ug context run_gen                    # instead of: get_code + find_usages +
                                      # traverse + analyze test_for + read the doc
ug context run_gen --max-chars 4000                  # tighter budget
ug context run_gen --include caller --include test   # the edit-safety half only
```

Every entry is labelled with **why it is there** — `target`, `caller`, `test`,
`dependency`, `doc` — so you can drop the half you don't need without a second
call. Roles fill in that priority order, so shrinking the budget sheds docs and
dependencies before callers, and whatever didn't fit is reported (`not shown: 4
dependency`) rather than silently omitted.

It takes **exactly one symbol** — a pack is a claim about one neighbourhood, so
a name matching several is an error listing the candidates. Use plain `get_code`
when you only want source text; use `ug context` when the question is *how does
this work* or *is it safe to change this*.

Two rules turn a loop into one call:

> **1. For any count, fraction, ranking, distribution or blast-radius question,
> run one `ug analyze`.** "How many functions are over 100 lines, and where do
> they cluster?" is `ug analyze long_functions_by_folder` — one call.
>
> **2. For any question about a *family* of symbols or files, use a wildcard.**
> "Who calls any of the validators?" is `ug find_usages 'validate_*'` — one
> call, not one per validator.

## `ug file_context` — the same, for a file

Most of the time you are handed a *file*, not a symbol: a path out of `git
diff`, a frame in a stack trace, a file the user named. `ug file_context` is
`ug context` for that unit — one budgeted call instead of six.

```bash
ug file_context native/src/cli/gen.rs                       # the whole report
ug file_context gen.rs --include outline                    # just the table of contents
ug file_context gen.rs --include test --include dependent   # the edit-safety half
ug file_context 'native/src/storage/*.rs'                   # survey a directory
```

It returns, in budget priority order:

| role | what it answers |
|---|---|
| `outline` | every symbol the file declares, in line order, with its doc clause |
| `importer` | which files import this one |
| `import` | which files and third-party packages it reaches for |
| `test` | which test files reach its symbols — what re-verifies it |
| `dependent` | which non-test files reach its symbols — the blast radius |
| `sibling` | what else lives in the same folder |

It accepts a path, a unique basename, a `file:` id, **or any symbol id** — given
a symbol it reports the file holding it. Given **several** files (or a glob) it
returns each file's outline only, because a budget split across several
neighbourhoods would thin every one of them; `--max-chars` then applies per file.

Two things it deliberately will not do. It returns **no source** — call
`ug get_code <id>` on the ids it hands you. And its `dependent` count is
non-test symbols within 3 hops, so run `ug analyze impact --arg target=<path>`
when you want the unfiltered figure.

## `ug walk` — read a change, not a patch

`git diff` orders by filename and stops at the file. `ug walk` maps each hunk
onto the **innermost symbol that contains it** and orders the stops by the
call graph, so callers come before the code they call — then follows the edges
out to the unchanged callers and tests the change reaches.

```bash
ug walk                  # uncommitted, untracked files included
ug walk staged
ug walk HEAD             # one commit, against its parent
ug walk main...HEAD      # this branch, against where it forked
ug walk --commits        # list recent commits to pick from
```

**Read the role on every stop.** It is the difference between code you changed
and code that merely touches it:

| Role | Means |
|---|---|
| `changed` | The diff edited these exact lines. `+n/-n` is inside *this symbol*, not its file. |
| `caller` | **Unchanged.** It calls or references something that changed. |
| `test` | **Unchanged.** A test that reaches something that changed. |

Never edit a `caller` or `test` believing the diff touched it.

One hop out by design — for the full reachable set use `ug analyze diff_impact`
and `ug analyze diff_retest_scope`. Line numbers are exact for uncommitted work
and the most recent commit; walking an older revision maps that diff's lines
onto today's code and prints a warning naming the files where that is
approximate.

## Routing — which command do I run?

| Question | Command |
|---|---|
| Counts / fractions / rankings / what breaks? | `ug analyze <preset>` — catalog below |
| **Everything about one symbol at once** | **`ug context <symbol>`** — code + callers + tests + deps + docs |
| Safe to change this symbol? | `ug context <symbol>` (or just `ug find_usages <symbol>`) before you edit |
| **What did this change actually touch?** | **`ug walk`** (uncommitted) · `ug walk HEAD` · `ug walk main...HEAD` — the symbols a diff edited, in call-graph order |
| What did my edited files break? | `ug analyze diff_impact --arg files=...` (feed it `git diff --name-only`) |
| Which tests should I re-run? | `ug analyze diff_retest_scope --arg files=...` |
| Graph match my edits? | `ug update <file>...` — do this before asking structurally |
| Which test covers this symbol? | `ug analyze test_for --arg symbol=<id>` |
| Only have a concept, not a name | `ug search "concept"` or `ug find_symbols <word>`, feed the id in next |
| Know the name (or fuzzy) | `ug find_symbols foo` · `ug find_symbols 'handle_*'` |
| How does a vague concept work? | `ug search "concept" -k 8` |
| What's in this file, and what surrounds it? | `ug file_context path/f.rs` |
| What's in this subtree? | `ug file_context 'src/**/*.ts'` (outlines only) |
| Read the source | `ug get_code <symbol>` · `ug get_code -f file --range 10-60` |
| Who calls / imports / implements this? | `ug find_usages <symbol>` |
| What does this depend on? | `ug traverse <symbol> -k 1` (widen only if needed) |
| How are A and B connected? | `ug shortest_path A B` |
| Where do I start in this repo? | `ug project_overview`, `ug analyze where_to_start` |
| What does this service expose / talk to? | `ug analyze boundary_census` → `ug analyze boundaries` |
| Is this change visible outside the system? | `ug analyze boundary_impact --arg target=path/to/f.rs` |
| Every endpoint / listener / CLI command | `ug find_symbols --boundary` |
| Central symbols · dependency cycles | `ug graph_centrality` · `ug graph_cycles` |

**`context` vs `find_usages` vs `traverse`** — three tools walk the same
edges, and picking the widest one wastes a call while picking the narrowest
one gives an answer that looks complete and isn't:

| You want | Use | Why not the others |
|---|---|---|
| Everything about one symbol before editing it | `ug context <sym>` | `traverse` gives you ids without code, tests or docs |
| Who calls / imports / would break | `ug find_usages <sym>` | `traverse -d inbound` is the same walk minus the call-site evidence, and its default edge set is narrower |
| What this reaches, N hops out, one edge kind | `ug traverse <sym>` | the only one of the three that walks outbound past one hop |

**`-t/--edge-type` narrows silently.** An edge-type filter that matches
nothing returns an *empty walk*, not an error — which reads as "this symbol
has no dependencies". `-t calls` alone misses a function passed as a value
(recorded as `references`), so prefer `-t calls,references` or no filter at
all. Both `traverse` and `find_usages` report what their filters hid, under
the result: trust that line over the tally above it.

Node ids are `kind:file:name` — **no line number**, e.g.
`function:path/to/file.rs:symbol_name`. Anywhere an id is expected you may
instead pass a **bare symbol name** or **wildcard**, so a `find_symbols` round
trip is optional; `find_symbols`, `file_context`, `get_code`, `find_usages`
and `traverse` also take **several arguments in one call** — batch, don't loop:

```bash
ug find_symbols run_gen run_serve run_ingest
ug find_usages connect                     # no id lookup needed
```

**Confusable properties** — these decide whether a count answers the question
you actually asked:

- **`has_comments`** (any prose) vs **`has_doc`** (doc comments only) — much
  code is inline-only, so `has_doc` undercounts. `comment_coverage` uses the
  former, `doc_coverage` the latter.
- **`code_lines`**, not `loc`; `long_functions_by_code` is the true-code measure.
- **`members`** is populated only for nesting languages (Java, Python, TS) —
  check coverage before ranking on it.

## What needs an embedder — and what happens without one

`analyze`/`traverse` need the db but **no** embedder; everything else reads
`graph.json` and needs neither. `walk` is the one command that also needs
**git** — outside a working tree it says so, and nothing else is affected. Only `search` is embedding-backed, and it behaves
differently depending on how you call it:

| Surface | No embedder |
|---|---|
| `ug search` **(CLI)** | **Degrades, doesn't fail** — warns on stderr, returns a name-substring match tagged `"matched_by": "name"`: no vector/FTS ranking, no graph expansion, no snippets |
| MCP `search` tool | **Hard fail** — error, no fallback |
| `POST /api/search/hybrid` · `/api/search/semantic` · `/api/chat` | **503** |
| `ug chat` · `ug tour` | **Exit** — need an embedder *and* a chat model |

Two traps in that fallback. It covers an embedder that cannot be **built** — the
default local ONNX model failing to load or download — not one that is *down*: a
remote `--base-url` endpoint always builds, so an unreachable one fails the query
outright. And the keyword/FTS channel is **not** the fallback; it lives *inside*
the RRF fusion on the embedder-backed path, so losing the embedder loses it too.

Vectors must also be **in** the db, and embedding is **opt-in**: after any run
without `--with-embed` (the default, and what the git hooks do) the semantic
channel is empty for the changed nodes until `ug ingest` catches up, even with a
working embedder.

So an embedding problem is never a dead end — but degraded `search` output is a
name match wearing search's clothes. When you see `"matched_by": "name"`, reach
for `find_symbols` (exact, wildcards), `analyze` (statistics, blast radius) or
`traverse` (edge walks) instead of trusting the ranking.

## Wildcards — one call instead of a loop

`* ? [abc] [a-z] [!ab] {a,b}` work wherever a symbol or file is named. **Quote
them, always** — and do not conclude from one working command that you can
skip it, because an unquoted pattern fails three different ways depending on
your shell and your cwd:

| | unquoted `ug find_usages index_with_*` |
|---|---|
| bash, nothing on disk matches | **works** — the shell passes the pattern through |
| zsh, nothing matches | `zsh: no matches found` — `ug` never runs |
| either shell, files *do* match | **silently wrong** — `ug` receives `indexer indexer.rs` and answers about those |

The third is the one that costs you: it returns a confident, complete-looking
answer about whatever the filenames happened to resolve to. `ug` warns when
every argument names a file that exists, but the quotes are the actual fix.

A pattern matches the **whole** name
(`*auth*` finds `reauth`, `auth` doesn't); in paths `*` stops at `/`, `**/`
crosses directories. Id-taking commands cap a pattern at 25 symbols and say so
when they hit it — narrow, don't trust a capped answer.

```bash
ug find_symbols 'handle_*'                    # every handler
ug find_symbols '*Controller' --node-type Class
ug find_symbols '*' --file-prefix 'src/auth/**' -k 100   # a whole subtree
ug file_context 'src/**/*.{ts,tsx}' -k 40
ug find_usages 'validate_*'                   # blast radius of a family
ug traverse 'handle_*' -d inbound             # one merged walk
```

## `ug analyze`

**Presets** (args `[in brackets]` passed as `--arg key=value`, repeatable):

- **census** — repo_census, biggest_files, language_breakdown, file_kinds, where_to_start
- **size** — long_functions `[min_loc]`, long_functions_by_folder `[min_loc]`,
  long_functions_by_code `[min_loc]`, size_histogram, god_classes,
  classes_by_members, param_bloat `[min_params]`, deep_nesting `[min_depth]`
- **documentation** — comment_coverage, comment_density, doc_coverage,
  doc_coverage_by_folder, token_docs, undercommented_complexity,
  undocumented_hotspots
- **dead code** — dead_code, orphan_files, duplicate_names
- **architecture** — dependency_fanin, fanout_offenders `[min_fanout]`,
  coupling_matrix, layering_violations `[from_prefix, to_prefix]`, boundaries,
  boundary_census
- **tests** — test_ratio, untested_symbols, retest_scope `[target]`, test_for
  `[symbol]`, diff_retest_scope `[files]`
- **risk** — impact `[target]`, impact_summary `[target]`, boundary_impact
  `[target]`, diff_impact `[files]`, risky_symbols

The catalog moves between versions — re-read `ug analyze --list` on a mismatch.
`ug analyze --list` also shows full arg detail.

```bash
ug analyze long_functions --arg min_loc=150        # preset + arg (repeatable)
ug analyze --gql "MATCH (n:Function) WHERE n.params > 6 \
  RETURN n.folder AS f, count(*) AS c ORDER BY c DESC"
```

`--arg target=` takes a **repo-relative file path** — a concept string silently
returns nothing. When the user names a concept, resolve the path yourself
(`ug find_symbols queue`), never ask them for it.

Page with `--range 21-40` / `--range 34-end`, not a bigger `-k` — it windows
rows the query already produced, so totals never move. Each answer prints the
next range to ask for.

**Three ways to get a wrong zero** — all return `0`, not an error:

- Aggregate over a property nothing carries → read the `coverage:` footer; treat
  `NOT INDEXED` as "about nothing".
- `--arg target=` matches no indexed file (paths are repo-relative) →
  confirm with `ug file_context`.
- `-t/--edge-type` filter on a type this graph lacks → `ug graph_schema` first.

Reachability presets (`impact`, `boundary_impact`, `retest_scope`,
`untested_symbols`) can hit the traversal cap on hub files — the error names
the fix; fall back to `impact_summary` or `find_usages <id> -k 2`.

## Boundaries — what "what breaks" actually means

A **boundary** is where code meets something outside it: an HTTP endpoint,
queue listener, CLI command or scheduled job coming *in*; an HTTP, database or
queue client going *out*. These are contracts others depend on, and their
callers were never indexed — the call graph cannot show them.

So when asked "what breaks if I change X", run **both**:

```bash
ug analyze impact --arg target=src/orders/repo.java           # internal blast radius
ug analyze boundary_impact --arg target=src/orders/repo.java  # external contract
```

`impact` answering "41 dependents" is a refactor; `boundary_impact` answering
"two REST endpoints and a Kafka listener" is an API change — say so, because
that decides the version bump, migration or deprecation.

Boundaries also show up unbidden: `find_usages` prints a
`⊕ N of M user(s) are system boundaries` line, and agent-tool hits carry a
`boundary:` field like `in:http.endpoint GET /api/orders/{id}`.

Detection is heuristic, tuned to under-report rather than invent: Express
routes and chained axum routers are **not** detected (receiver can't be typed);
a service method that merely calls a repository isn't tagged as db access — the
repository is. Treat an empty result as "none found"; `ug graph_schema` shows
`NOT INDEXED` on a graph indexed before this existed.

## Hand-written GQL

Prefer a preset when one fits. By hand: booleans are `0`/`1` so they can be
summed (`sum(n.has_doc)` / `count(*)` is the documented fraction);
variable-length paths need a finite bound (`*1..3`, never `*`); unanchored
walks past 2 hops blow the cap; an `EXISTS { … }` subquery needs its own
`RETURN`. Docs are `Concept` nodes, so GQL over `:Concept` asks about prose.

## Let the CLI teach you — don't guess flags

```bash
ug --help          # every command, grouped by what it needs (db vs graph.json)
ug <command> -h    # flags AND worked examples — read it instead of guessing
ug analyze --list  # every built-in question, by category, with args
ug graph_schema    # node/edge types actually in THIS graph, + full vocabulary
```

Not indexed yet → `ug gen` at the repo root. Stale generally (line numbers
don't match) → `ug gen` again — both incremental, cheap. `ug list` shows what
exists: size and a `STATUS` of `fresh`, `N changed`, `no db`, or `repo gone`.

Commands default to the **cwd basename** (graph-backed) / **active project**
(db-backed); away from the root pass `-n <project>`. Every command prints the
project it resolved to on **stderr** — read that line.
`[most recently updated project]` means it guessed wrong; re-run with
`-n <project>`. The banner is on stderr, so `--json` stays parseable;
`--no-banner` / `UG_NO_BANNER=1` turns it off.

**Two kinds of drift** (both reported where they apply; `ug hook status`
reports both for your repo):

| Drift | Affects | What you see | Fix |
|---|---|---|---|
| **Index behind the tree** — files edited since last `gen`/`update` | every structural command | `⚠ index is behind the tree …` on stderr, naming files | `ug update <file>...` |
| **Vectors owed** — nodes ingested without `--with-embed` (the default, and what the hooks do) | `search`/`chat`/`tour` only | `⚠ Some nodes have no vectors yet …` | `ug ingest -n <project>` |

`ug get_code` sits outside both — it reads the live tree and flags drift per
slice, so source and line numbers are never silently wrong. Neither warning is
on stdout, so `--json` / `-o` stay parseable.

**Lean search by default.** `ug search` and the MCP `search` tool return ids +
locations without source slices. Add `--snippets` (CLI) or
`includeSnippets: true` (MCP) when you want code inline; otherwise follow a hit
with `get_code`.

**One search, one knob.** `search` expands hits along graph edges by default —
that is why it answers "how does X work" better than grep. `--no-expand` (CLI) /
`expand: false` (MCP) returns only what matched: cheaper, and the right shape for
disambiguation or a filtered inventory via `--filter`/`whereClause`. The old
`semantic_search` command and tool were exactly that, and still work as aliases.

**Embedding is opt-in, and "no vectors" is not "no database".** The difference
decides whether `ug analyze` is trustworthy:

| `ug gen` / `ug update` | Written to the db | Current | Behind |
|---|---|---|---|
| *(default)* | nodes + edges + facts + keyword stats, **no vectors** | graph.json tools **and** `ug analyze` — stats, `diff_impact`, blast radius | `search`/`chat` miss the changed nodes |
| `--with-embed` | all of the above **plus vectors** | everything | — |
| `--no-ingest` | **nothing** — db not opened | graph.json tools only | **everything db-backed**, incl. `analyze`, answers from the *previous* ingest |

The default is the fast path — counts and blast radius without loading an
embedding model (what the hooks do). Add `--with-embed` when you want semantic
search live at the end of the run, or let `ug ingest -n <project>` catch the db
up afterwards. Use `--no-ingest` only for a structure-only first pass.
*(The old `--no-embed` still works; it is the default now.)*

**Persistent connection for many calls.** Each `ug` CLI call is a fresh
process. For a session of dozens, `ug serve` keeps graph + db cached in one
process and answers the same tools over HTTP (`POST /api/tools/<name>`) — the
`/api/tools/analyze` envelope matches `ug query --json` field-for-field.

## Traps

- `ug search` for a name you already know → `find_symbols` (no embeddings);
  for a file's contents → `file_context`.
- Trusting `ug search` ranking without checking `matched_by` — `"name"` means the
  embedder was unavailable and you got a substring match, not GraphRAG.
- Looping over a family of symbols or files → one wildcard call.
- An unquoted `'*'` — the shell expands it before `ug` sees it. It may *look*
  fine (bash passes an unmatched pattern through); when real files match you
  get a confident answer about the wrong nodes. Quote every pattern.
- A wildcard that matches nothing is usually anchoring: patterns cover the whole
  name (`*auth*`, not `auth`), and `*` doesn't cross `/` in a path.
- Judging code from a snippet → `get_code` for the whole symbol.
- `dead_code` / `untested_symbols` are candidates only — reflection, dynamic
  dispatch and macros are invisible to the graph.