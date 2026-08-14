# `ug` for AI Coding Agents — Strategy Analysis

> An outside-in analysis of how to make `ug` more useful to AI coding agents
> (Claude Code, Cursor, Copilot agents, etc.), and an honest verdict on hybrid
> search and natural-language semantic search for that audience.

| Field | Value |
| :--- | :--- |
| **Date** | 2026-08-14 |
| **Audience** | Product direction / design |
| **Method** | Codebase analysis + reasoning about agent workflows (not session telemetry) |

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

These are genuine differentiators that no amount of grep can replicate:

1. **Graph-structured blast radius.** `diff_impact`, `boundary_impact`, `find_usages` answer "what breaks if I change X?" — the single hardest question for an agent making a safe edit. Grep cannot do this.
2. **PPR graph expansion in `search`.** Personalized PageRank over edges is the real moat. It's structurally-aware ranking that neither grep nor a plain vector DB can reproduce.
3. **`code_query` presets + GQL.** One-call answers to "how many functions exceed 100 LOC and where" — replaces a loop of greps. The coverage-honesty (`NOT INDEXED` vs. a wrong zero) is exactly what an agent needs to avoid confident-but-wrong answers.
4. **Boundary detection.** Knowing which symbols are system entry/exit points (REST handlers, queue listeners) makes impact analysis mean something *beyond the repo.*
5. **Wildcards + batching + bare-name resolution.** Turns multi-call loops into one call (`find_usages 'validate_*'`). Token- and round-trip-efficient.
6. **Embedder-optional design.** The structural tools work on `graph.json` with zero external deps. An embedding failure is never a dead end.

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

Sharper framing: **for an agent, `find_symbols` + `traverse` + `find_usages` + `code_query` (all embedder-free) already cover 80%+ of needs.** `search` / `semantic_search` are the remaining 20%, and `semantic_search` (pure dense, no graph) is the least valuable of all — it's just a local vector DB, which agents increasingly have natively.

---

## Gaps & opportunities — ranked by leverage

### Tier 1: Highest leverage — fix the trust problem and own the edit-safety workflow

#### 1. The graph goes silently stale during editing — the #1 risk to agent trust

An agent edits 5 files, then asks `find_usages` / `diff_impact`. The answer is now wrong, and nothing warns it. `get_code` reads the live tree (good), but the structural tools the agent relies on for safety read the stale graph. If an agent can't trust blast-radius *after its own edits*, the killer feature is undermined at exactly the moment it matters most.

- `ug update <file>` exists but is **manual** — the agent must remember, every time.
- There's no staleness signal on tool output ("⚠ graph is 4 files behind the working tree for this result").
- This is the asymmetry that breaks trust: read-tools are live, structural-tools are stale, and they look identical.

**Opportunities:**

- **Automatic staleness detection** — track working-tree mtime/hash vs. indexed hash; emit a warning line on every structural tool result that touches changed files. Cheap to compute (the index already stores `file_hash` per node).
- **File-system watcher** — a `ug serve` background mode that incrementally re-indexes changed files (`ug update` + blake3 caching already exist; this is wiring). Or at minimum, a `ug watch` that an agent/shell hook/git hook triggers.
- **Git-hook integration** — `ug connect` could install a post-edit/post-commit hook that calls `ug update` on the changed paths, so the graph self-heals without the agent remembering.

> **Shipped (2026-08-14): git-hook integration.** `ug hook install` writes
> `post-commit`, `post-merge`, `post-checkout` and `post-rewrite` hooks that call
> back into `ug hook run`, which diffs the two commits the event moved between and
> runs `ug update` on those paths. `ug connect --hooks` installs them alongside the
> agent wiring. The hooks append to an existing hook of the same name behind
> `# >>> ug hook >>>` markers, honour `core.hooksPath`, never fail the git command,
> print one line and log the detail to `~/.ug/<project>/hook.log`;
> `UG_HOOK_DISABLE=1` skips one command. The remaining gap is the *uncommitted*
> edit burst — a working-tree watcher, not a git hook, is what closes that.
>
> Hook runs pass `--no-embed`: loading the embedding model costs more than the
> whole structural refresh (~1 s of a ~1.5 s run on this repo), so they write
> the graph, the facts and the keyword statistics and leave the changed nodes
> without vectors. Everything the safety story depends on — `find_usages`,
> `code_query`, `diff_impact` — stays exact; only vector recall lags, the store
> records the debt in `project.json`, and `ug ingest` backfills exactly the
> nodes owed. `ug hook status` and the `search`/`semantic_search` tools report
> it, so the degradation is never silent.

#### 2. A first-class "context pack" tool — collapse 5 tool calls into 1

When an agent works on symbol F, it typically assembles context through 4–6 round trips:

```
get_code F → find_usages F → traverse F → test_for F → read relevant doc
```

Each round trip is latency + tokens. A single curated call — `ug context <id>` — that returns a token-budgeted bundle (the symbol's code, its direct callers' call sites, its dependencies' signatures, the linked test, the linked doc section) would be **enormously** valuable. This is arguably the single highest-impact tool to add for agents.

- The **tour** feature is *almost* this, but wrapped in LLM narration (human-oriented). Agents want the **raw curated bundle**, not prose. Consider: `ug context` = tour's retrieval/assembly without the LLM step.
- Budget it with `maxChars` like `search` does. Rank sub-items by relevance.

#### 3. Symbol-level / semantic-diff impact — "you changed the signature of F; here's the precise blast radius"

`diff_impact` is file-level. An agent changing a function signature wants symbol-level precision: "callers of F, with their call-site arity, flagged if mismatched." The graph has `Calls` edges but **edges carry no properties** (no call-site line, no arg count) — this is the key limitation.

- **Edges have empty properties** (`build_edge_rows` writes `properties: String = ""`). Adding call-site line + arity to `Calls` edges at ingest would unlock signature-change analysis — one of the most common and error-prone agent tasks.
- A `ug verify <changed-files>` / `ug precommit` that combines: `diff_impact` + `diff_retest_scope` + `boundary_impact` + signature-mismatch detection into one "safety report." This makes `ug` the agent's **verification layer**, not just a search layer.

#### 4. Double down on "`ug` as the edit-safety net" as the product positioning for agents

The structural graph tools (`find_usages`, `impact`, `boundary_impact`, `retest_scope`) answer the question agents care about most: *"did I break anything, and what do I re-test?"* This is a stronger and more defensible positioning than "semantic code search" (crowded, commoditized by every agent platform's own embeddings). Lead with safety/verification in the agent skill text.

---

### Tier 2: Solid value — expand coverage and connectivity

#### 5. Language coverage gaps

Currently: TS/JS, Python, Java, Rust, Markdown, PDF. Missing: **Go, C/C++, C#, Ruby, PHP, Kotlin, Swift, Scala.** Agents work across all of these. Each missing language is a market where the agent falls back to grep. **Go and C# are likely the highest-ROI additions** (large agent-active ecosystems).

#### 6. "Find similar / duplicated code" tool

Agents refactor by finding repeated patterns. `duplicate_names` is name-based only. A structural-similarity or embedding-similarity tool ("functions structurally similar to F") would serve a real refactoring need. (This is actually one place where dense embeddings *are* useful for agents.)

#### 7. Stack-trace / error-to-graph resolution

Agents debug from stack traces constantly. A tool that ingests a stack trace, resolves each frame to a node, and shows the call path / blast radius would be high-value. `shortest_path` is the building block; a `ug explain_trace` wrapper would be the product.

#### 8. Documentation-to-symbol linkage

Concept nodes from Markdown exist, and there are `Imports` edges from doc links, but there's no "this doc section explains this symbol" link. Agents answering "how does X work" benefit enormously from linked prose. Consider: detect symbol references in prose (backticks, code refs) and emit `Documents` / `Explains` edges.

---

### Tier 3: De-emphasize for the agent use case

#### 9. The tour's LLM narration is human-oriented

The retrieval + assembly is great; the LLM-written 40-word narrations and camera-fly are for humans in the web UI. For agents, `--no-llm` (raw stops) is the useful mode, and that's essentially a **context pack** (see Tier 1 #2). Consider reframing: keep tour for the human/web surface, expose the raw assembly as `ug context` for agents.

#### 10. The chat REPL is redundant with the agent itself

`ug chat` runs *another* LLM with tool-calling. An AI coding agent *is* that loop — wrapping it in `ug`'s chat adds latency, cost, and a second model hop. The chat feature is valuable for **humans** using `ug` standalone (CLI, web UI), but an agent should call tools directly. Don't invest in chat-for-agents; invest in making the **tools** richer.

#### 11. Operator-level ranking knobs

`pprRestartProb`, `mmrLambda`, etc. — correctly removed from the agent tool schema. Agents don't tune these. Keep that discipline.

---

## Summary recommendations

| Priority | Action |
|---|---|
| **1 — Trust** | Add staleness detection/warning on every structural result; add `ug watch` / git-hook auto-refresh so the graph stays fresh during editing. |
| **2 — Context pack** | Add `ug context <id>` — curated, token-budgeted bundle (code + callers + deps + test + doc) in one call. |
| **3 — Edit safety** | Enrich `Calls` edges with call-site line/arity; add `ug verify` / `ug precommit` combining impact + retest + boundary + signature mismatch. |
| **4 — Positioning** | For agents, lead with "edit-safety / blast-radius verification" (the moat), not "semantic search" (commoditized). |
| **5 — Languages** | Add Go and C# next (largest agent-active ecosystems not yet covered). |
| **6 — De-emphasize** | Don't over-invest in dense semantic search for agents (supplementary, ~20% of queries); keep chat/tour for the human surface. |

---

## The one-sentence version

`ug`'s structural graph + PPR + blast-radius is a genuine moat that agents can't replicate with grep; the highest-leverage move is to make that graph **trustworthy during editing** (staleness), **cheaper to consume** (context pack), and **tied to change-safety** (semantic diff + verify) — while treating dense semantic search as a supplementary layer, not the centerpiece, for the agent audience.

---

## Caveat on confidence

These are observations from reading the codebase and reasoning about agent workflows, **not** from watching real agent sessions fail or succeed. The strongest validation would be **instrumenting actual agent sessions** to see:

- which tools get called,
- where round-trips pile up (the case for a context pack), and
- where stale results cause bad edits (the case for freshness).

That telemetry would tell you definitively which gap to close first.
