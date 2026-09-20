# Answer-tab retrieval — a dated, repeatable measurement

> Copy this file to a new date and diff, the way `EVALUATION-*.md` does.
> A retrieval change that cannot point at two runs of this is an opinion.

## Why this exists

The Ask → Answer pipeline was argued about for months on the strength of its
diagram — hybrid seed search, PPR over the graph, a char-budgeted pack. What
nobody could say was **whether the pack contains the answer.** "More agentic"
and "better retrieval" are both unfalsifiable without a number, and Agents.md
§1a is explicit that a design whose cost you have not measured is not done.

Stage 0 of the agentic-RAG work made the number obtainable. `ChatRagOutcome::
citations` is now everything the answer was allowed to cite — the seed pack
*plus* whatever the model's own searches added (`chat::CitationLedger`). For a
question whose answer node is known, "did retrieval reach it" became a fact.

## The instrument

| Piece | Where |
| :--- | :--- |
| Questions + ground truth | `native/tests/fixtures/rag_eval.json` |
| Harness | `native/src/chat_eval.rs` (in-crate, `#[ignore]`d) |

Twelve questions of the kind actually typed into the Ask bar, each with an
`expect_any` set — nodes any of which answers it. Retrieval is graded on
**reaching** one of them, not on ordering them a particular way.

Two measurements, deliberately separate:

- **`rag_eval_seed_recall`** — the first hybrid+PPR pass alone, no model.
  Deterministic, ~200 ms for all twelve, needs no endpoint. This is the number
  the pipeline diagram describes, and the ceiling on any answer that never
  re-searches.
- **`rag_eval_agentic_recall`** — a whole turn with the toolbox, graded on the
  ledger. **The difference between the two is what the agentic loop is worth.**

```bash
cd native
cargo nextest run --lib --run-ignored all -E 'test(rag_eval_seed)' --no-capture
UG_EVAL_LLM=1 cargo nextest run --lib --run-ignored all -E 'test(rag_eval)' --no-capture
```

## Before you believe a run

**Re-index first.** Grading retrieval against an index that predates the code
being asked about measures staleness, not retrieval:

```bash
ug update <changed files…>   # structure
ug ingest -n ug              # vectors — `ug update` leaves them behind
```

`ug find_symbols <name>` shows whether a node is there at all. Two setup
mistakes both present as *0 % recall with 0 items returned*, which looks
identical to a retrieval collapse and is not one:

1. An embedder built without the dim auto-probe. The store opens at the wrong
   dimension and answers everything with nothing.
2. Pointing `StoreSpec::Overgraph` at `~/.ug/<project>` instead of
   `~/.ug/<project>/ugdb`. Opens an empty store, no error.

Both cost real time here; the harness now guards both.

## Runs

### 2026-09-20

Index: `~/.ug/ug`, ~5.7k nodes / ~15.5k edges, `BAAI/bge-small-en-v1.5` @ 384.
Model: `Qwen3.5-35B-A3B-4bit` on a local OpenAI-compatible endpoint.
k=8, hops=2, PPR, snippets on.

| # | Run | Cited | Named | Tools | Capped | Wall clock |
| :-: | :--- | :---: | :---: | :---: | :---: | :---: |
| 1 | Seed retrieval only, no model | 3/12 | — | — | — | 0.2 s |
| 2 | Whole turn, as shipped | 3/12 | — | 0 | — | 155 s |
| 3 | … + prompt split, + item provenance | 3/12 | — | 0 | — | 150 s |
| 4 | Control: deliberation off (= 2, re-run) | 3/12 | 8/12 | 0 | — | 118 s |
| 5 | Deliberation on, cap 4 | 8/12 | 9/12 | 28 | — | 214 s |
| 6 | … + tool nodes citable | 8/12 | 10/12 | 46 | 6/12 | 300 s |
| 7 | … + cap 8, concurrent calls in a round | 7/12 | 11/12 | 40 | 2/12 | 234 s |
| 8 | **Repeat of 7, nothing changed** | **9/12** | **9/12** | 52 | 3/12 | **298 s** |
| 9 | No pre-retrieval — the model searches for itself | 8/12 | 10/12 | 28 | 0/12 | 290 s |

**Cited** = an expected node is in the citation list. **Named** = the answer
text names an expected symbol.

#### Read run 8 before believing runs 5–7

Run 8 changed no code. It moved cited by +2, named by −2 and wall clock by
+64 s against run 7. **That is the measurement's own noise**, and it is larger
than every effect claimed between runs 5 and 7.

What survives it:

- **Deliberation is the whole result.** Cited 3/12 twice (runs 2, 4) against
  7–9/12 four times (runs 5–8, mean 8.0). That gap is far outside the spread
  and it reproduced in both directions.
- **The round cap was binding, and is not now.** 6/12 questions hit a cap of
  4; 2–3/12 hit a cap of 8. Mechanical and repeatable.

What does **not** survive it, and must not be claimed:

- Tool-node citability, the 8-round cap and within-round concurrency show no
  measurable effect on cited recall at this sample size. They are defensible
  on their own terms — a node an answer used should be citable; independent
  calls needn't serialise; a cap should not sit below ordinary usage — but
  they are design arguments, not measured wins.
- **Concurrency's latency win is not demonstrated.** Run 7 (234 s) versus run
  6 (300 s) looked like the win; run 8 at the same code says 298 s. Wall clock
  varies ±30 % between identical runs, so this fixture cannot see it. Timing a
  single round directly would.
- The monotone `named` rise 8→9→10→11 across runs 4–7 reads as a trend and
  then run 8 returns 9. One question per step is one question of noise.

#### Run 9 — removing the pre-pass

`ChatRagOptions::seed` now defaults off whenever a toolbox is attached: a
deliberating model calls `search` itself, in the codebase's vocabulary rather
than the user's, so the pre-pass was arriving as a second and worse-phrased
copy of the same neighbourhood. One observed turn before the change: 8 seed
items the model did not use, 15 from its own search, 19 sources listed.

**Cited recall landed on 8/12 — exactly the mean of runs 5–8.** Read against
run 8's spread that is *no measurable change*, which is the claim: removing it
cost nothing. It is not evidence that removing it helped.

What is a fact rather than a measurement: the turn no longer pays for the pack.
Measured by the cost instrument on one turn, `context_tokens` went 10,256 → 41
(the preface alone).

Two observations that are **not** claims — one run each, both inside the noise:
0/12 hit the round cap (previous runs 2–6), and tool calls fell to 28, the
lowest of any agentic run.

**The change exposed a provenance hole the pack was hiding.** One row reads
`MISS · names it · 0 cited · 5 tools` — a correct answer with no sources at
all. `analyze` returns a table with an `id` *column*, not the `id: <value>`
lines `cite_tool_nodes` keys on, so nothing registered. The seed pack used to
mask this with a floor of 8 citations that had nothing to do with the answer.
Fixing it means teaching `cite_tool_nodes` to read that column; keeping a small
seed purely as a citation floor would be reintroducing the pre-pass under
another name.

#### The instrument needs to be bigger before it can answer anything else

At n=12 the spread is ±2 questions. Nothing smaller than the deliberation fix
is visible, which means **the remaining levers cannot currently be evaluated**.
Two ways out, best first:

1. **More questions.** ~30 with ground truth would roughly halve the spread and
   costs only authoring time. The two setup traps above make a bad fixture look
   like a retrieval collapse, so verify each id with `ug find_symbols`.
2. **Repeat passes.** `UG_EVAL_REPEATS=3` runs each question three times and
   pools the rows; the spread shrinks by √R. Cheaper to write, far more
   expensive to run — a 3-pass agentic run is ~15 minutes.

## A second kind of question — the analytic probe set

The twelve fixture questions are all *"where is the code that does X"*, graded
by whether a known node id reached the citation list. **A whole class of real
question is not that shape**, and it is the class the tool schemas exist for:

> which methods are longer than 150 lines and have no comments/doc?

Its answer is a *set*, not a node, and reaching it means combining two
predicates no single preset expresses. That is where the loop was observed
running the same `analyze long_functions min_loc=150` call twice, because the
preset could only say half of what was asked and repeating it was the nearest
thing to progress. (Fixed since: an exact repeat is served from a memo and the
model is told why repeating cannot help.)

These ten probe the same shape. Every verification query below was run against
`~/.ug/ug` and its answer is recorded, so a run can be marked right or wrong
without re-deriving the truth — the §11a trap is a script that replays
production logic and drifts from it.

| # | Ask this | What it probes | Verify with | Answer here |
| :-: | :--- | :--- | :--- | ---: |
| 1 | Which functions are over 150 lines with no doc comment? | the original: two predicates, no preset | `WHERE n.loc >= 150 AND n.has_doc = 0 AND n.is_test = 0` | 23 |
| 2 | What's undocumented but called from more than five places? | magnitude + absence; must not confuse `in_degree` with callers of the file | `WHERE n.has_doc = 0 AND n.in_degree > 5 AND n.is_test = 0` | 94 |
| 3 | Which of our public entry points have no documentation? | must find `boundary` rather than guessing at "public" | `WHERE n.has_doc = 0 AND n.is_test = 0 AND n.boundary = 1` | 93 |
| 4 | Show me functions taking five or more parameters that nobody documented. | `param_bloat` exists but not with the doc half | `WHERE n.params >= 5 AND n.has_doc = 0` | 47 |
| 5 | Is there code that is long, deeply nested *and* has no comments at all? | three predicates; `undercommented_complexity` is close and its thresholds are wrong for this | `WHERE n.loc >= 100 AND n.max_nesting >= 4 AND n.has_comments = 0` | 2 |
| 6 | How many functions does nothing reference, excluding tests and entry points? | must exclude boundaries or "dead code" includes every handler | `WHERE n.in_degree = 0 AND n.is_test = 0 AND n.boundary = 0` | 339 |
| 7 | Which HTTP endpoints take more than three parameters? | **the empty answer.** None do — does it say so, or invent rows? | `WHERE n.boundary_kinds CONTAINS 'http.endpoint' AND n.params > 3` | 0 |
| 8 | Which CLI commands has nobody tested? | two tools: `boundaries kind=cli.command`, then test reachability | `boundaries --arg kind=cli.command` + `untested_symbols` | — |
| 9 | What would I have to re-test if I changed the indexer? | `retest_scope` / `impact` take a target — does it pass one, or scan? | `analyze retest_scope --arg target=native/src/indexer.rs` | — |
| 10 | Which folder has the worst doc coverage for its size? | a preset **does** answer this — does it find `doc_coverage_by_folder`, or write GQL? | `analyze doc_coverage_by_folder` | — |

### What to watch, beyond right or wrong

The answers are mostly reachable; the interesting variable is **what it cost to
get there**. Every turn now reports it:

| Signal | Where | Bad looks like |
| :--- | :--- | :--- |
| Queries and rounds | the badge, and `tool_calls` / `tool_rounds` | four calls for a one-call question |
| A repeat | tool row reads `repeat · ~N tokens` | it asked the same thing twice |
| Ran out of budget | `hit_round_cap`, and the meta line says so | it answered under protest |
| Sources | the citations list | **0 cited on a correct answer** — see the `analyze` provenance hole above |

Question 7 is the one to read most carefully. An empty result set is where a
model is most likely to fill the silence, and no amount of retrieval quality
helps if the answer is invented.

### Why these are not in the fixture

They cannot be graded by `expect_any`: the answer is a count or a set, and
there is no node id to reach for. Automating them needs a second grader —
"does the answer contain this number" is the cheap version and is brittle
against phrasing, "does the model's own GQL match the reference" is better and
is a different harness. Until one exists these are a **manual probe set**: run
them, read the badge and the tool rows, and record anything surprising here.

They are also the better argument for widening the fixture (above). Twelve
"where is X" questions measure one half of what the toolbox is for.

#### Caveats that still apply

- **`named` is a weak proxy and is inflated.** Several expected names are
  guessable from the question wording. `cited` cannot be faked that way.
- **`cited` under-counted the loop until run 6** — only `search` registered
  into the ledger. Runs before that understate tool-found evidence.
- ~~MRR is 0.183 — retrieval ranks the answer badly.~~ **Wrong reading.** MRR
  here is position in the *citation ledger*, which is insertion-ordered: the
  seed pack takes 1–8, so a node found on round three cannot rank better than
  9 however good retrieval was. Fair for run 1, meaningless for runs 5–8.
- Three independent ceilings each terminated a run mid-measurement before
  they were raised: the HTTP request timeout (180 s), the tool-round cap (4),
  and nextest's `slow-timeout` × `terminate-after` (600 s).
