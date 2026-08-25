# `ug` for AI Coding Agents — Strategy Analysis

> An outside-in analysis of how to make `ug` more useful to AI coding agents
> (Claude Code, Cursor, Copilot agents, etc.), and an honest verdict on hybrid
> search and natural-language semantic search for that audience.

| Field | Value |
| :--- | :--- |
| **Date** | 2026-08-14 |
| **Revised** | 2026-08-15 — Tier 1.1 and 1.2 closed: staleness line on every structural result (CLI + MCP), `gen files:[...]` as the MCP mirror of `ug update`, and `ug context` / the `context` MCP tool as the curated pack |
| **Audience** | Product direction / design |
| **Method** | Codebase analysis + reasoning about agent workflows (not session telemetry) |

Each numbered gap carries a status: **Shipped** / **Partial** / **Open**. Claims
about the code are cited to a path so the next revision can re-check them
instead of re-deriving them.

---

## Framing: What AI coding agents actually need

An AI coding agent operates differently from a human explorer:

- It comes with **precise identifiers** (a name, file, error string, signature) far more often than a "vague concept."
- Its context window is **scarce and expensive** — every token costs latency at ~30 tok/s local endpoints.
- Its primary job is **making changes**, not browsing — so *"what will break?"* matters more than *"how does this work?"*
- It already has **grep, glob, Read, and a shell** — it will use those by default unless `ug` is clearly better.
- It trusts tools only when they're **always fresh and never silently wrong.**

This framing drives every recommendation below.

---

## What `ug` already does exceptionally well for agents

The agent-facing surface is deliberately small: **13 MCP tools** (`search`,
`traverse`, `find_usages`, `find_symbols`, `file_outline`,
`get_code`, `project_overview`, `context`, `shortest_path`, `analyze`,
`graph_schema`, `list_projects`, `gen` — `native/src/mcp/tools.rs`), each mirrored one-for-one
as a CLI subcommand (`native/src/cli/mod.rs::dispatch`), with the analytical
depth pushed into **39 `analyze` presets** rather than into more tools
(`native/src/analyze/presets.rs`). That ratio is the right one: an agent
pays context for every tool schema but nothing for a preset name it only uses
when it needs it.

Within that surface, these are genuine differentiators that no amount of grep
can replicate:

1. **Graph-structured blast radius.** `diff_impact`, `boundary_impact`, `find_usages` answer "what breaks if I change X?" — the single hardest question for an agent making a safe edit. Grep cannot do this.
2. **PPR graph expansion in `search`.** Personalized PageRank over edges is the real moat. It's structurally-aware ranking that neither grep nor a plain vector DB can reproduce.
3. **`analyze` presets + GQL.** One-call answers to "how many functions exceed 100 LOC and where" — replaces a loop of greps. The coverage-honesty (`NOT INDEXED` vs. a wrong zero) is exactly what an agent needs to avoid confident-but-wrong answers.
4. **Boundary detection.** Knowing which symbols are system entry/exit points (REST handlers, queue listeners) makes impact analysis mean something *beyond the repo.*
5. **Wildcards + batching + bare-name resolution.** Turns multi-call loops into one call (`find_usages 'validate_*'`). Token- and round-trip-efficient.
6. **Embedder-optional design.** Nothing an agent needs for safety requires a model. The tools split across two backing stores — `find_symbols`, `file_outline`, `get_code`, `find_usages`, `project_overview`, `graph_schema` and `shortest_path` read `graph.json` directly with zero external deps; `analyze`, `traverse` and `search` read the `ugdb` store — and **neither half needs vectors**, which is exactly what makes the `--no-embed` hook path viable. An embedding failure degrades ranking, never correctness.

---

## Direct questions: hybrid search & semantic search for agents

### Hybrid search (dense + sparse + PPR): the components are not equally valuable to agents

| Component | Value to an agent | Why |
|---|---|---|
| **PPR graph expansion** | **Essential — the moat** | Nothing else does it. Agents cannot replicate structurally-aware ranking. This is what makes `search` worth calling over grep. |
| **Sparse / BM25 keyword** | Moderate | Agents approximate this with `grep`, but `ug`'s tokenized identifier splitting (camelCase → `["build","sparse","keyword"]`) and ranking add real value. Least differentiated part. |
| **Dense / semantic embedding** | **Low–moderate — supplementary** | See below. |

### Natural-language semantic search — honest verdict: useful, not essential, and over-indexed for the agent use case

The uncomfortable truth: **agents rarely query in natural language internally.** They operate on identifiers, paths, and error strings. The "I have a vague concept, no name" case is maybe **15–20% of agent queries** — and it's the *human's* mental state (new to codebase), not the agent's. The agent is good at translating a vague user request into precise identifiers.

Scenarios where dense/semantic search genuinely helps an agent:

- "Find all authentication-related code" (fuzzy concept, no known name)
- Onboarding to a codebase with unknown naming conventions
- Bridging a user's natural-language ask to code identifiers
- "Find code similar to this function" (structural duplication — though embeddings are a weak proxy)

Scenarios where it's **redundant or inferior**:

- Agent knows the name → `find_symbols` (exact, fast, no embedder dependency)
- Agent needs callers → `find_usages` (graph walk)
- Agent has the file → `file_outline` / `get_code`
- The agent platform already has its own embeddings (Cursor, GitHub Copilot) → duplicate infrastructure

**Recommendation:** Treat the structural graph + PPR as the core value prop for agents. Treat dense/semantic search as an *enhancement layer* for the ~20% exploration case — keep it, don't let it be a single point of failure (the embedder-optional design is correct), but don't over-invest here for the agent audience. The dense half is far more valuable in the **human-facing** surface (chat, tour, web UI) than in the agent-facing one.

Sharper framing: **for an agent, `find_symbols` + `traverse` + `find_usages` + `analyze` (all embedder-free) already cover 80%+ of needs.** `search` / `semantic_search` are the remaining 20%, and `semantic_search` (pure dense, no graph) is the least valuable of all — it's just a local vector DB, which agents increasingly have natively.

The vector-less hook path is the first production evidence for this split, and
it points the same way. Git-hook refreshes deliberately skip the embedder
because loading the model costs more than the entire structural refresh, and the
result is a graph where every safety answer is exact and only vector recall
lags. That trade would be unacceptable if dense search were load-bearing for
agents. It isn't — which is why the debt is merely *reported*
(`vectors_note`, `ug hook status`, `ug list`) rather than blocking.

---

## Gaps & opportunities — ranked by leverage

### Tier 1: Highest leverage — fix the trust problem and own the edit-safety workflow

#### 1. The graph goes silently stale during editing — the #1 risk to agent trust — **Shipped**

An agent edits 5 files, then asks `find_usages` / `diff_impact`. The answer is now wrong, and nothing warns it. `get_code` reads the live tree (good), but the structural tools the agent relies on for safety read the stale graph. If an agent can't trust blast-radius *after its own edits*, the killer feature is undermined at exactly the moment it matters most.

This is the asymmetry that breaks trust: read-tools are live, structural-tools
are stale, and they look identical. All four pieces now exist — the last one,
the warning on the structural result itself, is what turns the other three from
plumbing into something the agent can act on.

**Shipped — self-healing across git events.** `ug hook install`
(`native/src/cli/hook.rs`) writes `post-commit`, `post-merge`, `post-checkout`
and `post-rewrite` hooks that call back into `ug hook run`, which diffs the two
commits the event moved between and runs `ug update` on those paths.
`ug connect --hooks` installs them alongside the agent wiring. The hooks append
to an existing hook of the same name behind `# >>> ug hook >>>` markers, honour
`core.hooksPath`, never fail the git command, print one line naming the project
they refreshed, and log the detail — hook, project, repo, data dir, binary
version, file list, exit code — to `~/.ug/<project>/hook.log`;
`UG_HOOK_DISABLE=1` skips one command.

Hook runs never pass `--with-embed` — and since the flag is opt-in, `ug gen`
and `ug update` do not either unless asked: loading the embedding model costs
more than the whole structural refresh (~1 s of a ~1.5 s run on this repo), so they write the
graph, the facts and the keyword statistics and leave the changed nodes without
vectors. Everything the safety story depends on — `find_usages`, `analyze`,
`diff_impact` — stays exact; only vector recall lags, the store records the debt
in `project.json` (`pendingVectorsSince`), and `ug ingest` backfills exactly the
nodes owed. `ug hook status`, `ug list` and the `search` / `semantic_search`
tools all report it (`native/src/mcp/mod.rs::vectors_note`), so the degradation
is never silent.

**Shipped — per-slice staleness on the read path.** `get_code` compares the live
file's blake3 against the `file_hash` the node was indexed with and attaches a
`stale` note to the returned slice
(`native/src/agent_tools/get_code.rs::stale_note`) — "the recorded span may be stale,
re-run `ug gen`". The read path is honest.

**Shipped — project-level staleness.** `project::staleness`
(`native/src/project.rs`) stats every file recorded in `project.json` against
`graph.json`'s mtime and separates *changed*, *missing* and *repo gone* — a
moved checkout is not "every file deleted". `ug list` renders it as a STATUS
column (`fresh` / `N changed` / `no db` / `repo gone` / `no graph`), and
`GET /api/projects/staleness` serves the same struct so the CLI and the KB
Manager cannot disagree.

**Shipped — the structural tools now say it, on both surfaces.** The MCP
server appended `staleness_note` to every structural result already
(`native/src/mcp/mod.rs::call_tool`); what was missing was the CLI, which is
the surface the bundled skill actually drives. `scope::announce_staleness`
(`native/src/cli/scope.rs`) closes it, riding the scope banner's channel and
contract: one line on **stderr**, deduplicated per project, suppressed by
`--no-banner` / `UG_NO_BANNER=1`, so `--json` and `-o` stay pipeable.

```
⚠ index is behind the tree · 2 changed of 418 indexed files · src/a.ts, src/b.rs
  Structural answers describe the last index. Refresh: ug update <file>... (fast) or ug gen -n ug.
```

Two hook points cover all thirteen commands: `load_agent_graph`
(`native/src/cli/agent.rs`) for the graph.json readers, and
`single_store_spec_from_args` (`native/src/cli/dest.rs`) for the db readers.
The latter rather than `store_specs_from_args` deliberately — that one is
shared with `gen` and `ingest`, and warning that the index is stale immediately
before refreshing it is noise.

Both notes now **name the drifted files** rather than only counting them
(`Staleness::changed_sample`, capped at four with `+N more`). The count alone
cannot answer the question that decides what the agent does next: *are these
the files I just edited?* If yes, the answer it is holding describes the
previous version of its own work; if no, it can proceed.

**Shipped — the uncommitted edit burst, via the cheap path.** The watcher is
still unbuilt; the second-best move landed instead, on both surfaces. The skill
(`native/src/mcp/ug-skill.md`) no longer claims the graph "is kept current by
git hooks" full stop — it says hooks cover *commit boundaries*, names the
edit-ask-edit-ask window as the gap, and makes `ug update <files>` an explicit
habit before any structural question. The MCP mirror is `gen` taking
`files: [...]` (`tool_gen` + `gen_targets`), which also closes a silent failure
the whole-repo `gen` hid: it reports symbols-per-named-file, so a `.go` file in
a repo with no Go grammar comes back `0 symbols — extension not indexed`
instead of as an empty structural answer the agent would have believed.

The habit is not load-bearing on its own, which is the point of shipping it
alongside the warning: an agent that forgets to refresh is told by the next
structural call that it forgot.

#### 1b. The *other* silent-wrongness: answering about the wrong project — **Shipped**

Staleness is one way a confident answer can be wrong; **scope** is the other,
and it was the less obvious of the two. Every project-scoped command resolves
its target through a fallback chain — `-n/--name` → the active project → the
cwd's basename → *the most recently updated project* — and that last link is a
trap: run `ug find_usages` from a directory that was never indexed and you get
a fluent, well-formed answer about some other repo. For an agent, which has no
peripheral vision and cannot notice that the file paths look unfamiliar, this
is worse than staleness: a stale answer is about the right code, a mis-scoped
one is about the wrong code entirely.

`native/src/cli/scope.rs` closes it. Every project-scoped command announces —
one line, on **stderr**, deduplicated per resolved data dir — which project it
picked, which repo that project indexes, where the data lives, and **which link
of the chain fired** (`-n/--name`, `active project`, `current directory`, `most
recently updated project`). stderr, not stdout, because `--json` / `-o` output
has to stay pipeable; `--no-banner` / `UG_NO_BANNER=1` turns it off.

The agent-facing lesson generalizes beyond this one banner: **a resolution rule
that can silently pick the wrong input must name the rule it followed.** The
same reasoning is why `ug hook` now logs the project, repo and binary version it
refreshed rather than "graph refresh failed", and why `ug hook status` prints
the repo the *project* records rather than the one the hook is installed in —
when those disagree, the hooks have been faithfully refreshing a graph of some
other tree.

Worth carrying further: the MCP server resolves through its own chain
(`UG_PROJECT` → cwd match → active → local `./ugdb`,
`native/src/mcp/mod.rs`), where a stderr banner is *not* visible to the agent.
Putting the resolved project name into `project_overview` — and into the
"NOT INDEXED" style coverage line the tools already emit — is the MCP-side
equivalent, and it is not there yet.

#### 2. A first-class "context pack" tool — collapse 5 tool calls into 1 — **Shipped**

When an agent works on symbol F, it used to assemble context through 4–6 round trips:

```
get_code F → find_usages F → traverse F → test_for F → read relevant doc
```

`context` (`native/src/agent_tools/context.rs::context`) is that sequence as one call,
on both surfaces — `ug context <symbol>` and the `context` MCP tool. As
predicted, it needed no new analysis: it is assembly and budgeting over
`get_code`, `find_usages`, a 1-hop edge scan and the shared `is_test_node`
predicate, which is why it landed cheaply and why it needs neither an embedder
nor anything from the db that `get_code` was not already reading.

The shape is the one this section argued for — **one call, one symbol, one
budget, every sub-item labelled by why it is there**:

```
Context for Function staleness  native/src/project.rs:380-470
1986 chars of 12000 budget · 3 caller, 2 test · not shown: 4 dependency

── callers (3) ──
- Function announce_staleness  native/src/cli/scope.rs:127-166
  this —Calls→ target
    137  let Some(stale) = project::staleness(data_dir, &meta) else {
```

Five roles — `target`, `caller`, `test`, `dependency`, `doc` — filled in that
order, which is a priority claim rather than a taxonomy: you need the body
first, then who breaks, then what re-verifies, then what it leans on, then the
prose. Shrinking `maxChars` therefore sheds docs and dependencies before
callers, and what did not fit is reported (`not shown: 4 dependency`) instead
of silently omitted — the same "never be quietly incomplete" rule as the
coverage line and the staleness note.

Three decisions worth recording, because each was a way the pack could have
been subtly wrong:

- **A test that calls the target is a `test`, not a `caller`.** *Who breaks* and
  *what re-verifies* are different questions; the same node answering both
  would read as two callers and overstate the blast radius. Test detection
  shares one definition with the `test_for` preset
  (`storage::facts::is_test_node`, extracted for this), so the pack and
  `analyze test_for` cannot disagree about what a test is.
- **A `Concept` pointing at the target is a `doc`, not a caller.** Prose that
  references a symbol is a real inbound edge, so `find_usages` returns it — but
  it is not code that breaks.
- **The budget charges rendering overhead.** Counting only the payload made a
  500-char pack return 894 characters — a 79% overshoot, worst exactly when the
  caller is being careful about tokens. Per-item chrome and a fixed header
  reserve are charged, which brings a tight pack to ~26% over and a
  default-sized one under. It remains a budget, not a guarantee, and says so.

`--include` narrows it: `ug context F --include caller --include test` buys the
edit-safety half alone. What is *not* here is doc linkage worth much — see #8,
still open: `doc` items only appear where the indexer already emitted a
`Concept` edge, which is rare. The role exists and is honest about being empty.

The **tour** feature remains the human-facing sibling: same assembly instinct,
wrapped in LLM narration. `context` is that retrieval without the LLM step,
which is what an agent wanted all along (see Tier 3 #9).

#### 3. Symbol-level / semantic-diff impact — "you changed the signature of F; here's the precise blast radius" — **Open**

`diff_impact` is file-level. An agent changing a function signature wants symbol-level precision: "callers of F, with their call-site arity, flagged if mismatched." The graph has `Calls` edges but **edges carry no properties** (no call-site line, no arg count) — this is the key limitation, and it is unchanged: `build_edge_rows` still writes `properties: String::new()` (`native/src/storage/ingest.rs:530`).

- Adding call-site line + arity to `Calls` edges at ingest would unlock signature-change analysis — one of the most common and error-prone agent tasks. It is also the only item on this list that requires touching the *indexers*, so it is the most expensive; everything else is assembly over data that already exists.
- A `ug verify <changed-files>` / `ug precommit` that combines: `diff_impact` + `diff_retest_scope` + `boundary_impact` + signature-mismatch detection into one "safety report." This makes `ug` the agent's **verification layer**, not just a search layer. Worth splitting from the edge work: the first three presets exist today, so a `ug verify` that reports impact + retest + boundary is shippable now and gains signature-mismatch later.
- The natural home for it is the git-hook path that already exists: `ug hook run` knows the exact file set that changed between two commits, which is the same input `ug verify` wants.

#### 4. Double down on "`ug` as the edit-safety net" as the product positioning for agents — **Shipped**

The structural graph tools (`find_usages`, `impact`, `boundary_impact`, `retest_scope`) answer the question agents care about most: *"did I break anything, and what do I re-test?"* This is a stronger and more defensible positioning than "semantic code search" (crowded, commoditized by every agent platform's own embeddings).

This has landed in the agent-facing text. The bundled skill
(`native/src/mcp/ug-skill.md`) now leads with "answer any question about an
existing codebase **— and check the safety of your own edits to it**", tells the
agent to use `ug` *while* editing in both directions (ask who depends on a
symbol before changing it; ask what broke and what to re-test after), and
triggers on the safety phrasings directly: *"is this a breaking change"*,
*"what did my change break"*, *"what should I re-test"*, *"is it safe to change
this"*, *"did I miss a caller"*. The MCP server description
(`native/src/mcp/mod.rs`) carries the same framing.

The copy gap noted in the previous revision is closed. The skill no longer
promises the graph "is kept current by git hooks" — a claim true at commit
boundaries and false during an uncommitted edit burst. It now scopes the claim
to commit boundaries, names the burst as the gap, makes `ug update <files>` an
explicit pre-question habit, and shows the warning the agent will see if it
forgets.

---

### Tier 2: Solid value — expand coverage and connectivity

#### 5. Language coverage gaps — **Open**

Unchanged since the first draft. The indexed extension list
(`native/src/indexer/common.rs`) is `ts, tsx, js, jsx, py, java, rs, md, mdx,
markdown, pdf` — five tree-sitter grammars (TypeScript/TSX covering JS/JSX,
Python, Java, Rust, Markdown) plus a pure-Rust PDF text path. Office formats
were dropped with the pdfium backend and are not coming back cheaply.

Missing: **Go, C/C++, C#, Ruby, PHP, Kotlin, Swift, Scala.** Agents work across
all of these. Each missing language is a market where the agent falls back to
grep — and, worse, where it falls back *silently*, because a repo with no
indexable files still produces a graph and answers questions about the
Markdown in it. **Go and C# are likely the highest-ROI additions** (large
agent-active ecosystems). A cheap partial mitigation ahead of any new grammar:
have `project_overview` state the indexed-language coverage of the repo, so an
agent working in a Go service learns in one call that `ug` can only see its
docs.

#### 6. "Find similar / duplicated code" tool — **Open**

Agents refactor by finding repeated patterns. `duplicate_names` (a preset, name-based only) is what exists. A structural-similarity or embedding-similarity tool ("functions structurally similar to F") would serve a real refactoring need. (This is actually one place where dense embeddings *are* useful for agents.)

#### 7. Stack-trace / error-to-graph resolution — **Open**

Agents debug from stack traces constantly. A tool that ingests a stack trace, resolves each frame to a node, and shows the call path / blast radius would be high-value. `shortest_path` is the building block; a `ug explain_trace` wrapper would be the product.

#### 8. Documentation-to-symbol linkage — **Open**

Concept nodes from Markdown exist, and there are `Imports` edges from doc links, but there's no "this doc section explains this symbol" link. Agents answering "how does X work" benefit enormously from linked prose. Consider: detect symbol references in prose (backticks, code refs) and emit `Documents` / `Explains` edges.

**This got more valuable when #2 shipped.** `ug context` has a `doc` role and
populates it from `Concept` nodes adjacent to the target — but on this repo that
is 366 Concept nodes and almost no edges reaching a code symbol, so the role is
nearly always empty. The consumer now exists and is wired; the edges are the
only missing half. Emitting `Documents` edges from backticked symbol references
in prose would light up the one part of the context pack that is currently
honest-but-blank, which makes this the cheapest remaining upgrade to a shipped
feature rather than a speculative new one.

---

### Tier 3: De-emphasize for the agent use case

#### 9. The tour's LLM narration is human-oriented

The retrieval + assembly is great; the LLM-written 40-word narrations and camera-fly are for humans in the web UI. For agents, `--no-llm` (raw stops) is the useful mode, and that's essentially a **context pack** (see Tier 1 #2). That reframing has now happened: `ug context` is the raw assembly for agents, and tour keeps the narration for the human/web surface. The two are separate code paths rather than one with a flag — tour walks a *route* between several symbols for a reader with no prior knowledge, `context` describes *one* symbol's neighbourhood for a caller about to edit it — but if a third consumer appears, tour's retrieval is the place to look for shared machinery.

#### 10. The chat REPL is redundant with the agent itself

`ug chat` runs *another* LLM with tool-calling. An AI coding agent *is* that loop — wrapping it in `ug`'s chat adds latency, cost, and a second model hop. The chat feature is valuable for **humans** using `ug` standalone (CLI, web UI), but an agent should call tools directly. Don't invest in chat-for-agents; invest in making the **tools** richer.

#### 11. Operator-level ranking knobs

`pprRestartProb`, `mmrLambda`, etc. — correctly removed from the agent tool schema. Agents don't tune these. Keep that discipline.

---

## Summary recommendations

| Priority | Action | Status |
|---|---|---|
| **1 — Trust** | Keep the graph fresh and say so when it isn't. | **Shipped** — git hooks auto-refresh on commit/merge/checkout/rewrite; `get_code` flags stale slices; `ug list` + `/api/projects/staleness` report project drift; scope banner names the resolved project; **every structural result now carries a staleness line naming the drifted files**, on stderr (CLI) and appended to the text (MCP); the skill and the `gen` tool both make `ug update <files>` the post-edit habit. **Still open:** the working-tree watcher, which would make the habit unnecessary rather than merely backstopped. |
| **2 — Context pack** | Add `ug context <id>` — curated, token-budgeted bundle (code + callers + deps + test + doc) in one call. | **Shipped** — `ug context` + the `context` MCP tool. Five labelled roles filled in priority order under one `maxChars`; `--include` narrows it. Assembly over existing tools, so no embedder and no new analysis. |
| **3 — Edit safety** | `ug verify` combining impact + retest + boundary; then enrich `Calls` edges with call-site line/arity for signature-mismatch detection. | **Open** — split: `ug verify` is shippable from existing presets; edge properties need indexer work (`ingest.rs:530` still writes `""`). |
| **4 — Positioning** | For agents, lead with "edit-safety / blast-radius verification" (the moat), not "semantic search" (commoditized). | **Shipped** — `ug-skill.md` and the MCP server description both lead with edit safety and trigger on the safety phrasings. |
| **5 — Languages** | Add Go and C# next (largest agent-active ecosystems not yet covered). | **Open** — still 5 grammars + PDF. Interim: report language coverage in `project_overview`. |
| **6 — De-emphasize** | Don't over-invest in dense semantic search for agents (supplementary, ~20% of queries); keep chat/tour for the human surface. | **Holding** — the `--no-embed` hook path proves the point: the structural half is what the safety story needs, vectors are the part that can lag without breaking anything. |

**If only one thing ships next:** `ug verify` (#3). Tier 1 now has one item
left, and its cheap half is shippable today: `diff_impact` +
`diff_retest_scope` + `boundary_impact` combined into a single "safety report"
over the file set `ug hook run` already computes. The expensive half — call-site
line and arity on `Calls` edges, for signature-mismatch detection — is the only
item on this list that requires touching the indexers, and it is what turns
that report from file-level into symbol-level.

---

## The one-sentence version

`ug`'s structural graph + PPR + blast-radius is a genuine moat that agents can't replicate with grep; the highest-leverage move is to make that graph **trustworthy during editing** (staleness), **cheaper to consume** (context pack), and **tied to change-safety** (semantic diff + verify) — while treating dense semantic search as a supplementary layer, not the centerpiece, for the agent audience.

Since the first draft, two of those three have closed. **Trustworthy during
editing:** the graph self-heals across git events, the read path admits when a
slice is stale, projects report their own drift, every command names the project
it resolved, and every structural answer now says when it is behind the tree and
which files moved. **Cheaper to consume:** `ug context` collapses the five-call
assembly into one labelled, budgeted bundle.

What remains is the third — **tied to change-safety**: `ug verify` (shippable
today from existing presets), the `Calls`-edge properties that would raise it
from file-level to signature-level, and the working-tree watcher that would make
the `ug update` habit unnecessary rather than merely backstopped by a warning.

---

## Caveat on confidence

These are observations from reading the codebase and reasoning about agent workflows, **not** from watching real agent sessions fail or succeed. The strongest validation would be **instrumenting actual agent sessions** to see:

- which tools get called,
- where round-trips pile up (the case for a context pack), and
- where stale results cause bad edits (the case for freshness).

That telemetry would tell you definitively which gap to close first. Note that
the trust work shipped so far was driven the same way this doc was — by reading
the code and reasoning about failure modes — so the ordering of what remains is
still an argument, not a measurement.
