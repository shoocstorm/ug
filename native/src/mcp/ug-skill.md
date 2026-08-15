---
name: ug
description: >-
  Answer any question about an existing codebase or docs set — and check the
  safety of your own edits to it — by querying its `ug` (UltraGraph)
  knowledge graph from the CLI. Use this BEFORE grep, glob or reading files
  whenever the task is to understand code rather than open a file you have
  already located: how does X work, where is X defined, who calls or imports
  X, what breaks if I change X, how are A and B connected, where do I start
  in this repo. Use it WHILE editing too, in both directions: before changing
  a symbol, ask who depends on it (`ug find_usages`, `ug analyze
  boundary_impact`); after changing files, ask what that broke and what to
  re-test (`ug analyze diff_impact`, `ug analyze diff_retest_scope`) — run `ug
  update <file>...` on the files you just edited first, because git hooks only
  refresh the graph at commit boundaries and structural answers otherwise
  describe your code as it was before the edit. Also use for every count, fraction, ranking
  or distribution over the repo (how many functions are undocumented, which
  files are biggest, what is dead or untested) — one `ug analyze` replaces a
  loop of greps. Also use whenever a question spans a FAMILY of symbols or
  files rather than one — every handler, all the `*Controller` classes, each
  `test_*`, everything under `src/auth/` — because `ug` takes wildcards
  (`*` `?` `[abc]` `{a,b}`) wherever a symbol or file is named, so one call
  replaces a loop. Also use to find where a system meets the outside world —
  its REST endpoints, queue listeners, CLI commands, scheduled jobs and
  outbound HTTP/DB/queue clients — and to ask whether a change is visible
  beyond the repo. Also triggers on: ug, UltraGraph, `ug analyze`, blast
  radius, impact analysis, dead code, repo statistics, "which files depend
  on", "all the functions named like", "every file matching", system
  boundary, entry point, API surface, "what endpoints does this expose",
  "is this a breaking change", "what did my change break", "what should I
  re-test", "is it safe to change this", "did I miss a caller".
---

# ug — codebase knowledge graph from the CLI

`ug` indexes a repo (code *and* prose docs) into a graph of symbols and edges.
Prefer it over `grep`/`Read` for anything relational or aggregate: those see
text, `ug` sees the graph.

**It is for editing work, not only for reading.** The graph answers the two
questions that decide whether a change is safe — *who depends on this?* before
you touch it, and *what did I just break, and what do I re-test?* after — and
grep cannot answer either. Use it on your own edits, not just on unfamiliar
code.

## Editing a codebase with `ug`

```bash
# 1. BEFORE you change a symbol — who is downstream of it?
ug find_usages <symbol>                       # callers, importers, call sites
ug analyze boundary_impact --arg target=<file>  # is the change visible outside the system?

# 2. AFTER an edit burst — what did it reach, and what covers it?
git diff --name-only | tr '\n' ',' | sed 's/,$//'      # the changed-file list
ug analyze diff_impact --arg files=a.ts,b.rs             # blast radius of those files
ug analyze diff_retest_scope --arg files=a.ts,b.rs       # the tests to re-run
```

**The graph keeps up with you — at commit boundaries.** `ug hook install` puts
git hooks in the repo, so every commit, merge, checkout and rebase re-indexes
the paths it touched. What the hooks do *not* cover is the window you spend
most of your time in: edit, ask, edit, ask, commit once at the end. Across that
burst the index describes the code as it was before you started.

**So make this a habit: after you edit files, run `ug update` on them before
you ask anything structural about them.**

```bash
ug update src/a.ts src/b.rs   # the files you just edited — focused and fast
ug gen                        # whole project, incremental; after a big or messy change
ug hook status                # are the hooks installed here? are vectors owed?
```

It costs a fraction of a second and it is the difference between a real blast
radius and a stale one. You are not relied on to remember, though — **every
structural command tells you when it is answering from a stale index**, on
stderr, naming the files that drifted:

```
⚠ index is behind the tree · 2 changed of 418 indexed files · src/a.ts, src/b.rs
  Structural answers describe the last index. Refresh: ug update <file>... (fast) or ug gen -n ug.
```

If you see that line and the named files are ones you just edited, the answer
you are reading is about the previous version of your own work — refresh and
ask again. If they are files you do not care about, carry on. `ug get_code` is
the exception that needs none of this: it always reads the live working tree,
so a stale graph shows up there as an id that no longer resolves, never as
silently wrong source.

Two rules turn a loop into one call — reach for them before iterating:

> **1. For any count, fraction, ranking, distribution or blast-radius question,
> run one `ug analyze`.** Never grep for a count; never loop a per-file command to
> build one. "How many functions are over 100 lines, and where do they cluster?"
> is `ug analyze long_functions_by_folder` — one call.
>
> **2. For any question about a *family* of symbols or files, use a wildcard.**
> Every command that names a symbol or file takes `* ? [abc] {a,b}`. "Who calls
> any of the validators?" is `ug find_usages 'validate_*'` — one call, not one
> per validator. See [Wildcards](#wildcards--one-call-instead-of-a-loop).

If the `ultragraph` MCP tools are also connected you don't need them: same
engine, same parameter names, richer `--help`. If you are using them *instead*
of the CLI, the post-edit refresh above is the `gen` tool with
`files: ["src/a.ts", "src/b.rs"]` — same job as `ug update`, and it reports how
many symbols each file you named contributed, so a file `ug` cannot parse shows
up as `0 symbols` rather than as an empty answer you would have believed.

## Routing — which command do I run?

| Question | Command |
|---|---|
| How many / what fraction / biggest / what breaks? | `ug analyze <preset>` — catalog below |
| Is it safe to change this symbol? | `ug find_usages <symbol>` before you edit |
| What breaks across the files I just changed? | `ug analyze diff_impact --arg files=...` (feed it `git diff --name-only`) |
| Which tests should I re-run for my changes? | `ug analyze diff_retest_scope --arg files=...` |
| I just edited files — make the graph match | `ug update <file>...` — do this **before** asking anything structural about them (git hooks only fire on commit) |
| Which test covers this symbol? | `ug analyze test_for --arg symbol=<id>` |
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
| Where do I start in this repo? | `ug project_overview`, `ug analyze where_to_start` |
| What does this service expose / talk to? | `ug analyze boundary_census`, then `ug analyze boundaries` |
| Is this change visible outside the system? | `ug analyze boundary_impact --arg target=path/to/f.rs` |
| Every endpoint / listener / CLI command | `ug find_symbols --boundary` |
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

Only `search` and `semantic_search` need an embedder; `analyze` and `traverse`
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

## `ug analyze`

### Preset catalog — pick directly, no `--list` needed

- **census** — `repo_census`, `biggest_files`, `language_breakdown`, `file_kinds`, `where_to_start`
- **size** — `long_functions` `[min_loc]`, `long_functions_by_folder` `[min_loc]`, `long_functions_by_code` `[min_loc]`, `size_histogram`, `god_classes`, `classes_by_members`, `param_bloat` `[min_params]`, `deep_nesting` `[min_depth]`
- **documentation** — `comment_coverage`, `comment_density`, `doc_coverage`, `doc_coverage_by_folder`, `token_docs`, `undercommented_complexity`, `undocumented_hotspots`
- **dead code** — `dead_code`, `orphan_files`, `duplicate_names`
- **architecture** — `dependency_fanin`, `fanout_offenders` `[min_fanout]`, `coupling_matrix`, `layering_violations` `[from_prefix, to_prefix]`, `boundaries`, `boundary_census`
- **tests** — `test_ratio`, `untested_symbols`, `retest_scope` `[target]`, `test_for` `[symbol]`, `diff_retest_scope` `[files]`
- **risk** — `impact` `[target]`, `impact_summary` `[target]`, `boundary_impact` `[target]`, `diff_impact` `[files]`, `risky_symbols`

Args in `[brackets]` are passed `--arg key=value` (repeatable). The catalog
moves between versions — re-read `ug analyze --list` if `ug --version` mismatches.

### Usage

```bash
ug analyze long_functions --arg min_loc=150            # preset + arg (repeatable)
ug analyze impact --arg target=path/to/file.rs
ug analyze --gql "MATCH (n:Function) WHERE n.params > 6 \
  RETURN n.folder AS f, count(*) AS c ORDER BY c DESC"
```

**`--arg target=` takes a repo-relative file path** — a concept string
silently returns nothing. When the user names a concept rather than a path,
look the path up yourself — never ask them for it:

```bash
ug find_symbols queue          # → …:src/…/queue/AbstractQueue.java:…
ug analyze impact --arg target=<resolved path>
```

**Page with `--range 21-40` / `--range 34-end`, not a bigger `-k`.** It windows
rows the query already produced, so totals never move and you never re-read
what you've seen. Each answer prints the next range to ask for.

**Three ways to get a wrong zero** — all return `0`, not an error:

- Aggregate over a property nothing carries → read the `coverage:` footer; treat `NOT INDEXED` as "about nothing".
- `--arg target=` path matches no indexed file (paths are repo-relative) → confirm with `ug file_outline`.
- `-t/--edge-type` filter on a type this graph lacks → `ug graph_schema` first.

**Reachability presets can hit the traversal cap** on hub files (`impact`,
`boundary_impact`, `retest_scope`, `untested_symbols` walk variable-length
paths). The error names the fix; fall back to `impact_summary` or
`find_usages <id> -k 2`.

### System boundaries — what "what breaks" actually means

A **boundary** is where the code meets something outside it: an HTTP endpoint,
a queue listener, a CLI command or a scheduled job coming *in*; an HTTP,
database or queue client going *out*. These are the contracts other teams and
other systems depend on, and the call graph cannot show you their callers —
those callers were never indexed.

So when asked "what breaks if I change X", run **both**:

```bash
ug analyze impact --arg target=src/orders/repo.java           # how much code moves
ug analyze boundary_impact --arg target=src/orders/repo.java  # what is visible outside
```

`impact` answering "41 dependents" is a refactor. `boundary_impact` answering
"two REST endpoints and a Kafka listener" is an API change — say so, because
that is what decides whether the change needs a version bump, a migration or
a deprecation notice.

Boundaries also show up without being asked for: `find_usages` prints a
`⊕ N of M user(s) are system boundaries` line, and every agent-tool hit
carries a `boundary:` field like `in:http.endpoint GET /api/orders/{id}`.

Detection is heuristic — annotations, decorators, attributes and client call
sites. It is tuned to under-report rather than invent: Express routes and
chained axum routers are **not** detected (the receiver cannot be typed), and
a service method that merely calls a repository is not tagged as database
access — the repository is. Treat an empty result as "none found", and check
`ug graph_schema` for the boundary counts; on a graph indexed before this
existed it says `NOT INDEXED` rather than reporting zero.

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
ug analyze --list    # every built-in question, by category, with args (full detail)
ug graph_schema    # node/edge types actually in THIS graph, + the full vocabulary
```

`query --list` and `graph_schema` change per repo/version — re-read them rather
than remembering them.

Not indexed yet → `ug gen` at the repo root. Stale after *your* edits → `ug
update <the files you changed>`; stale generally (line numbers don't match the
file) → `ug gen` again, which re-runs an existing project from its recorded
root. Both are incremental, so cheap. `ug list` shows what exists — with each
project's size, and a `STATUS` telling you whether it is `fresh`, `N changed`
(the index is behind the tree), `no db` (`analyze`/`search`/`chat` cannot read
it), or `repo gone`.

Graph-backed commands default to the **cwd basename**, db-backed ones to the
**active project** — away from the repo root, pass `-n <project>`. You never
have to infer which one answered: every command prints the project it resolved
to on **stderr** before doing any work, with the rule that picked it —

```
▸ project ug · ~/Documents/project/ug · data ~/.ug/ug · [active project]
```

Read that line. `[most recently updated project]` means the command fell all
the way through the chain and answered from a project unrelated to the
directory you are in — re-run it with `-n <project>`. The banner is on stderr,
so `--json` output stays parseable; `--no-banner` turns it off.

**Freshness, in detail** (see [Editing a codebase with `ug`](#editing-a-codebase-with-ug)
for the workflow). Two independent kinds of drift, each reported where it
applies, so you never have to guess whether an answer is stale:

| Drift | Who is affected | What you see | Fix |
|---|---|---|---|
| **Index behind the tree** — files edited since the last `gen`/`update` | every structural command: `find_usages`, `traverse`, `shortest_path`, `ug analyze`, `search` | `⚠ index is behind the tree …` on stderr, naming the drifted files | `ug update <file>...` |
| **Vectors owed** — nodes indexed with `--no-embed` (what the git hooks do) | `search` / `semantic_search` ranking only; structure and blast radius stay exact | `⚠ Some nodes have no vectors yet …` | `ug ingest -n <project>` |

`ug get_code` sits outside both: it reads the *live* working tree and flags
drift per slice, so source and line numbers are never silently wrong. The
structural tools cannot do that — they answer from the indexed graph and a
stale blast radius looks exactly like a true one — which is why they announce
the drift instead. `ug hook status` reports both kinds for the repo you are in.

Neither warning appears on stdout, so `--json` and `-o` stay parseable;
`--no-banner` (or `UG_NO_BANNER=1`) silences them along with the scope banner.

**Lean search by default.** `ug search` and the MCP `search` tool return ids +
locations without source slices. Add `--snippets` (CLI) or `includeSnippets:
true` (MCP) when you want the code inline; otherwise follow a hit with
`get_code`.

**`--no-embed` vs `--no-ingest` — they are not the same flag, and the
difference decides whether `ug analyze` is trustworthy.** Both are accepted by
`ug gen` and `ug update`:

| Flag | Written to the OverGraph db | What is current | What is behind |
|---|---|---|---|
| `--no-embed` | nodes + edges + facts + keyword stats, **no vectors** | graph.json tools **and `ug analyze`** — statistics, `diff_impact`, blast radius | `search` / `semantic_search` / `chat` miss the changed nodes |
| `--no-ingest` | **nothing at all** — the db is not opened | graph.json tools only: `find_symbols`, `file_outline`, `get_code`, `find_usages`, `shortest_path`, `project_overview` | **everything db-backed**, including `ug analyze` statistics and blast radius, answers from the *previous* ingest |

So: use `--no-embed` when you want speed but still need counts and blast
radius (this is what the git hooks do); use `--no-ingest` only for a
structure-only first pass where no embedder is available. `ug ingest -n
<project>` catches the db up either way — it embeds only the nodes still owed
a vector.

**Persistent connection for many calls.** Each `ug` CLI call is a fresh process
that reloads the graph; for a session of dozens of calls, `ug serve` keeps the
graph + db cached in one process and answers the same tools over HTTP
(`POST /api/tools/<name>`) — the `/api/tools/analyze` envelope matches `ug
query --json` field-for-field.

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
