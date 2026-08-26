# Performance Tuning Journey

> The shared ledger for performance work on `ug`. One section per round, each
> with the baseline it started from, what landed with its measured number, and
> what the round taught that would otherwise be re-learned. Items are numbered
> `P<group>.<n>` continuously across rounds — Round 1 opened at P1, Round 4
> closed at P11.
>
> **Two standing rules.** Nothing lands without a measurement, and nothing is
> deleted when it fails — it moves to [Rejected / deferred](#rejected--deferred)
> with the number that killed it, so the next audit does not re-propose it.

| Field | Value |
| :--- | :--- |
| **Opened** | 2026-08-18 |
| **Version** | 0.1.16 |
| **Primary fixture** | `~/.ug/neo4j` — 161,725 nodes / 745,964 edges / 330 MB `graph.json` |
| **Status** | Rounds 1–3 landed; Round 4 complete bar P11.7 (deferred). Suite **908/908** |

**Status marks:** ✅ landed and verified · ⬜ open · ⏭️ deferred · ❌ rejected by measurement

---

## Where things stand

Against the state before Round 1. On `~/.ug/neo4j` except the two browser
rows, which are Round 2's synthetic 485k-node index (~3× neo4j).

| Surface | Before | Now | |
| :--- | ---: | ---: | ---: |
| `ug gen` **cold** (default, with ingest) | 33.87 s / 6,117 MB | **20.12 s / 5,123 MB** | **1.68× / 1.19×** |
| `ug gen` **warm** (re-index, nothing changed) | 46.01 s / 6,826 MB | **8.05 s / 2,633 MB** | **5.7× / 2.6×** |
| `ug gen --no-ingest` | 5.17 s / 3,378 MB | **4.18 s / 1,161 MB** | 1.24× / **2.91×** |
| `ug serve` idle RSS | 1,245 MB | **730 MB** | **1.70×** |
| `ug serve` startup to graph-ready | 6.66 s | **0.62 s** | 10.7× |
| Browser tab, 485k nodes | 280 MB heap | **95 MB** | **2.9×** |
| …load → interactive | 2,485 ms | **1,531 ms** | 1.6× |
| `/api/graph/search` per keystroke | 23.8 ms / 133 KB | **0.44 ms / 24.8 KB** | **54× / 5.4×** |
| `ug graph centrality` | never returned | **3.3 s** | — |

**The largest remaining number** is `upsert_nodes` at 7.5 s — 37% of a cold
ingest, and inside OverGraph 0.17 rather than this crate.

> ⚠️ **`ug gen` duplicates every edge in the store on each re-index.** Found
> while verifying P11.6 and **not fixed** — it is a correctness bug in the
> write path, not a performance one. See
> [Edges are appended, never replaced](#edges-are-appended-never-replaced).

---

## Fixtures and harness

| Fixture | `graph.json` | Notes |
| :--- | :--- | :--- |
| `~/.ug/java-demo` | 108 KB | Smoke-test size |
| `~/.ug/ug` | 4.6 MB | ~4,400 nodes — the local-mode case |
| `~/.ug/overgraph` | 16 MB | Mid-size |
| `~/.ug/neo4j` | 330 MB | 161,725 nodes / 745,964 edges — the stress case |

- `cargo` root is `native/`; the suite runs via `cargo nextest run` (thread
  count lives in `.config/nextest.toml` — do not pass `-j`).
- **The RTK hook rewrites `cargo test` and filters its stdout**, which strips
  `println!` — a bench appears to run and report nothing. Wrap the whole
  command in `rtk proxy "..."` for raw output. The same filter mangles long
  `grep` output.
- Micro-benches live at `native/tests/graph_bench.rs` and
  `storage_bench.rs`, both `#[ignore]`d, plain `Instant`, no Criterion. Use
  `--release`; a debug build measures rustc's bounds checks. `UG_BENCH_ALL=1`
  lifts the 20k-node fixture cap.
- Browser heap: `node --expose-gc`, run the structure, drop the payload,
  `gc()` three times, read `heapUsed`. Node's V8 is the page's engine. Tab
  totals via Chrome `--enable-precise-memory-info`, median of 8.
- Peak RSS: `/usr/bin/time -l`. Phase attribution: 0.2 s `ps` sampling, or
  temporary `Instant` probes reverted before landing.
- `ug demo --page-only` must be re-run from the repo root after any
  `native/src/vis/` change, or `the_published_demo_page_is_not_stale` fails.

---

## Standing conclusions

Carried forward so they are not re-derived. Each was measured once and has
held since.

- **The storage query layer is not the bottleneck.** `/api/graph/cycles` over
  746k edges runs in 0.33 s; PPR and the vector index are fine. The *write*
  path is another matter entirely — see Round 4.
- **Chat wall time is entirely local-LLM tokens** (~83 tok/s decode). Tool
  execution measured 0.1 s across 4 calls.
- **On macOS, freeing memory does not return it.** Dropping a buffer after
  allocating it leaves RSS where it was. Every memory win in Rounds 3–4 had to
  be *not allocating*, never *allocating then releasing*. This cost two
  separate false starts (P10.4, P10.5) before it was understood.
- **`str::find` beats a hand-rolled scan.** `to_lowercase()` + `find` is a
  tuned two-way/`memchr` search; the allocation is not the expensive half.
- **`ug gen` output is not reproducible** — see
  [P11.10](#p1110--the-indexs-content-is-not-reproducible-either). Correctness
  checks compare the node map, edge multiset and `resolution`, not bytes.

---

# Round 1 — Rust hot paths

**2026-08-18 · baseline `cdc9a2b` · 15 items, all landed · suite 871/871**

Static audit of hot paths in `native/src`, then measured per item. The theme
was code keyed by `String` with `Vec::remove(0)` dequeues, in functions that
already had dense indices in hand.

| # | Item | Measured | |
| :--- | :--- | :--- | :--- |
| P1.1 | Brandes betweenness: index-based rewrite | **235×** (1166 ms → 5.0 ms at n=1600); 162k fixture went from never returning to 3.3 s | ✅ |
| P1.2 | `find_shortest_path`: predecessor array | 8.4× | ✅ |
| P1.3 | `run_k_hop_bfs`: real BFS + incident-list induction | 1.2× — the estimate was wrong | ✅ |
| P2.1 | Concurrent embedding batches | ~8× cold ingest | ✅ |
| P2.2 | Stop reading + hashing every file twice | ~2× cold index I/O | ✅ |
| P2.3 | Progress meter: drop the per-file mutex + flush | scales with cores | ✅ |
| P3.1 | Lazy `graph.json` compression | **21×** startup (6.66 s → 0.32 s), −92 MB idle | ✅ |
| P3.2 | Stop requesting `Accept-Encoding: identity` | no transfer win — the premise was wrong; landed as a progress-bar fix | ✅ |
| P3.3 | Cache `/api/graph/stats`; `as_str()` over `{:?}` | **50×** (19.8 ms → 0.4 ms) | ✅ |
| P4.1 | MCP `shortest_path`: stop re-parsing the graph | +346 MB freed | ✅ |
| P4.2 | `mmr_rerank`: hoist relevance, cache norms | O(k²nd) → O(knd), output bit-identical | ✅ |
| P4.3 | `read_snippet`: per-call file cache | medium | ✅ |
| P4.4 | `api_search`: bound the result set | safety | ✅ |
| P5.1 | Hover: index the *view* edges | O(edges) → O(degree) per hover | ✅ |
| P5.2 | Gizmo: stop rewriting `innerHTML` at 30 Hz | low–medium | ✅ |

### What it taught

**Three correctness bugs surfaced, two of them severe**, and all three were
found by rewriting hot code rather than by any test:

- **Betweenness centrality had never produced a usable number.** A stale
  distance read left `pred` empty for every node, so the accumulation loop body
  never executed and the endpoint returned identically zero for every node of
  every graph. An inverted accumulation term was hiding behind it. Confirmed
  empirically on path, hub and diamond fixtures before the rewrite.
- **`run_k_hop_bfs` reported wrong hop distances** — a LIFO walk recorded
  first-found rather than shortest.
- **P3.2's premise was wrong.** `graph.json` was already served `br`; there was
  no transfer win to have. The item still landed, as a progress-bar fix.

**An estimate being wrong is a result.** P1.3 was rated "High" and measured
1.2×. Recording that is what stops the next audit from re-proposing it.

---

# Round 2 — Large-graph client memory

**2026-08-20 · baseline `d5ed699` · 12 items · suite 881/881**

Scope was the 500k-node case — roughly 3× `~/.ug/neo4j`, the size a monorepo
index reaches. Measured in V8 (`node --expose-gc`) against a synthetic index
built from the real `~/.ug/neo4j` distributions: avg id **141 chars**, avg name
42, 8,910 files scaled to 27k, max degree 8,680.

**The baseline:** a 99.7 MB slim index became 236 MB after `JSON.parse` and
**331 MB retained for the session**. Two facts set the plan — the renderer was
never the problem (solo mode already caps drawing at 1,500 nodes), and **90% of
the payload was string identity**, arriving as a JSON array of 500k separate
strings.

| # | Item | Measured | |
| :--- | :--- | :--- | :--- |
| P6.1 | `transformSlim`: shared empty arrays, typed degrees | 331 → 215 MB | ✅ |
| P6.2 | `refreshSuggestions`: stop scanning + sorting 500k per keystroke | a stall per keystroke | ✅ |
| P6.3 | `pushEdge`: O(degree²) → O(degree) dedupe | a hub click was seconds | ✅ |
| P6.4 | Stop rebuilding `new Map(nodes.map(…))` per call | medium | ✅ |
| P6.5 | Short-circuit the filter predicates when nothing is filtered | 2.9 → 1.2 ms, **beats HEAD** | ✅ |
| P7.1 | Binary columnar node index (`/api/graph/nodes.bin`) | 98.2 → 51.7 MB wire, no JSON parse | ✅ |
| P7.2 | Front-coded id/name blobs | **2.6×** on the dominant column (21.8 → 8.4 MB) | ✅ |
| P8.1 | `NodeStore`: columnar typed arrays + lazy materialisation | the 331 MB | ✅ |
| P8.2 | Column-backed counts, search and name index | medium | ✅ |
| P8.3 | Rank `/api/graph/search` before applying `limit` | correctness — the box's top hit | ✅ |
| P9.1 | Decode the index in a Worker, transfer the buffers | 393 ms → 7 ms main-thread | ✅ |
| P9.2 | Index identity (drop ids from the wire entirely) | would reach 18 MB | ⏭️ |

**Result:** tab heap **280 → 95 MB (2.9×)**, load → interactive **2,485 →
1,531 ms**, cold hub click 69 → 38 ms. The index itself went from 426 MB peak /
338 MB held to **58 MB both**, built in 7 ms instead of 393.

### What it taught

- **Search did not get faster, and that was the point.** 77 ms of scanning
  485k nodes *on the main thread* became 82 ms of waiting for a server that
  answers in under 1 ms. The old number was a frozen tab; the new one is not.
  (Round 3 later found the server was answering in 23 ms, not 1 — see P10.2.)
- **The gzip regression was the deliberate trade.** Front coding removes
  exactly the redundancy gzip fed on, so the frame compresses worse (7.1 → 9.7
  MB) even though it is half the size uncompressed. `ug serve` is loopback-only;
  2.6 MB of transfer is nothing and 47 MB of resident memory is everything.
- **A post-load GC can look like a regression.** The first click after load was
  bimodal — half the runs 57 ms, half 145 — because the 51 MB `ArrayBuffer` is
  external memory that V8 collects shortly after the load burst. Inserting a
  2.5 s idle removed it entirely and inverted the ordering. Not the server, not
  the store, not the front coding: all three were ruled out first.
- **A silently wrong decoder is worse than a crash.** The front-coded decoder's
  scratch buffer reallocated without copying the accumulated shared prefix, so
  ids crossing a growth boundary came back with their head replaced by NUL
  bytes and became unfindable. **Nothing threw.** Caught only by sweeping all
  485,175 ids through `indexOf` and demanding each resolve to its own index —
  and the first version of that test passed against the broken decoder, so the
  fixture now forces a growth mid-block.
- **Local mode broke and said nothing** for a while: `transformData` has no
  `nodes` binding, so a new counting pass threw a `ReferenceError` that
  `loadGraph`'s `catch` turned into a generic "could not load" card. Server
  mode was fine throughout. Only found by running the *small* graph through the
  browser harness.

**Why identity stayed as qualified ids (P9.2).** Dropping them from the wire
measures beautifully — 331 MB → 18 MB — and is wrong for this codebase: 60+
client call sites take a node id that came from a server response, and eight
protocol boundaries speak the real qualified id. The plan attacked the
*representation* instead: keep every id, never as 500k separate JS strings.

---

# Round 3 — The process, not the tab

**2026-08-25 · baseline `1525e63` · 10 items, all landed · suite 891/891**

Rounds 1–2 moved the browser. Nothing had measured what `ug` itself costs while
doing it. The finding was uniform: **the time is fine and the memory is not.**

**The baseline:** `ug gen --no-ingest` peaked at **3,378 MB** to produce a
330 MB file. `ug serve` held **1,245 MB idle**, before any request. Every
endpoint answered under 10 ms except `/api/graph/search` at **23.3 ms** — which
Round 2 had just made the page's search box, firing per keystroke.

**Where the bytes were.** `GraphEdge` held `source` and `target` as owned
`String`s: 745,964 edges × 2 = **1.49 million allocations, ~252 MB**, holding
exactly **161,725 distinct values** already owned by `nodes[i].id`. Plus a
346 MB `graph.json` buffer retained for a request that, in server mode, the
page is routed away from making.

| # | Item | Measured | |
| :--- | :--- | :--- | :--- |
| P10.1 | `ug gen`: stop parsing `graph.json` a third time, untyped | 3,378 → 2,250 MB from deleting one line | ✅ |
| P10.2 | `/api/graph/search`: narrow to the previous prefix's matches | 23.3 → 1.5 ms; **54×** at six characters | ✅ |
| P10.3 | Typed `ug gen` pipeline — no JSON seams between stages | 2,250 → 1,618 MB, and 1.9× wall | ✅ |
| P10.4 | Intern `GraphEdge` endpoints as `Arc<str>` | serve 916 → 730 MB; gen 1,458 → 1,161 MB | ✅ |
| P10.5 | Drop the retained `graph.json` buffer; mmap the parse | 1,238 → 916 MB | ✅ |
| P10.6 | `AdjIndex`: CSR rows + resolved endpoint columns | 323k `Vec` allocations gone; no string hash per edge served | ✅ |
| P10.7 | `dedupe_edges`: stop cloning 1.5M strings to key a map | ~192 MB of transient churn | ✅ |
| P10.8 | `extract_return_type`: compile the regex once, borrow the source | a regex compile + a body copy *per function* | ✅ |
| P10.9 | `/api/graph/search`: send what the caller reads (`?fields=id`) | 133 → 24.8 KB per keystroke | ✅ |
| P10.10 | `graph_keyword_search` / `filter_edges_by_type` defects | `format!("{:?}")` per node removed | ✅ |

**Result:** `ug gen --no-ingest` **3,378 → 1,161 MB (2.91×)**, `ug serve` idle
**1,245 → 730 MB (1.70×)**, search **54×**. Startup regressed 0.31 → 0.62 s,
taken deliberately.

### What it taught

- **When you intern is the whole item.** Three placements were measured:
  none (1,458 MB peak), after the fact in `dedupe_edges` (1,456 MB, **+1.6 s**),
  and as each edge is made or read (**1,161 MB**). Interning afterwards is
  worthless — every duplicate has already been allocated, and freeing them does
  not return the memory. The serve side interns as it *parses*; the builder
  interns at *push* time.
- **P10.5 was measured wrong twice before it was measured right.** Dropping the
  retained buffer changed idle RSS by 7 MB, because the bytes were still read
  first and freed after. `from_reader` avoided the allocation but cost
  0.31 → **1.33 s** of startup. `memmap2` — already in the tree via fastembed —
  got the memory at 0.48 s.
- **The obvious search fix was the wrong one, and it was measured.** Replacing
  `to_lowercase()` with an allocation-free ASCII scan made the endpoint
  **slower**: 23.3 → 33.0 ms. Narrowing the candidate set is what worked, and
  it fits how the endpoint is actually used: a search box is typed one
  character at a time, and substring containment is monotone under prefix.
- **`Arc<str>` over `u32` indices, deliberately.** Indices would reach ~6 MB
  against `Arc<str>`'s ~49 MB, but need the node table wherever an edge is
  built or read — 41 construction sites, 144 reads, ten test files — and a
  seeded deserializer, because an edge would stop meaning anything on its own.
- **The startup regression is the right trade.** 0.31 s of startup for 515 MB
  of resident memory, on a process that then runs for hours.

<a id="graphjson-is-not-reproducible"></a>
### `graph.json` is not reproducible, and it is not this round's bug

Two runs of the **unmodified** pre-Round-3 binary produce different
`graph.json` bytes — same node ids, same edge set, same `resolution`, but the
*order* of the edge list and of some nodes' `imports` arrays varies.

This cost real time, because the first two samples said the opposite:

| binary | runs compared | equal? |
| :--- | :--- | :--- |
| HEAD | h1 vs h2 | ✅ |
| HEAD | h1 vs h3, h4, h5 | ❌ |

Two agreeing samples of a coin are not evidence that it has one face. Round 3's
changes were briefly suspected — `dedupe_edges` was reverted and re-measured to
rule it out — before five samples of HEAD settled it.

The mechanism is `HashMap` iteration order reaching an ordered output: Rust's
`RandomState` seeds per map from a per-thread counter, so iteration order
depends on how many maps that thread built first, which depends on how rayon
distributed files — which is timing-dependent. Making the pipeline faster
changed the timing and made a latent nondeterminism easy to hit.
[P11.10](#p1110--the-indexs-content-is-not-reproducible-either) is the stronger
version of this, and the fix.

---

# Round 4 — The ingest nobody measured

**2026-08-25 · Phase 0 landed, P11.4–P11.10 open · suite 899/899**

Rounds 1–3 all measured `ug gen --no-ingest`. That flag turns off **the
default**: without it, `ug gen` also writes the graph into the OverGraph store,
and that step is **87% of the command's wall clock and five sixths of its peak
memory**.

| `ug gen -i <neo4j> --no-cache` | |
| :--- | ---: |
| wall clock | **33.87 s** |
| …of which ingest | **29.62 s (87%)** |
| CPU | 56.4 s user on 18 cores — **1.7× average parallelism** |
| **peak RSS** | **6,117 MB** |
| the same command `--no-ingest` | 4.18 s / 1,161 MB |

### Where the 29.6 s went

Six of these nine phases had no timing at all before this round; two still
print nothing at runtime.

| phase | time | % | parallel? |
| :--- | ---: | ---: | :--- |
| `upsert_nodes` | 10.42 s | 35% | inside OverGraph |
| `refresh_sparse_stats` | 7.23 s | 24% | **no — pure CPU, one thread** |
| *store open + commit (untimed)* | *4.71 s* | *16%* | — |
| `upsert_edges` | 2.32 s | 8% | inside OverGraph |
| `ensure_query_indexes` | 1.69 s | 6% | **silent — no output at all** |
| `build_texts` | 1.24 s | 4% | no |
| `prune_to_graph` | 0.94 s | 3% | **silent unless it removed something** |
| `plan.finish` | 0.44 s | 2% | no |
| `capture_for_graph` | 0.42 s | 1% | no |
| diff against the DB | 0.21 s | 1% | — |

There is **no `rayon` anywhere in `native/src/storage/`.** The indexer
parallelises; everything after it does not.

**Where the 6.1 GB went**, sampled every 200 ms: 1,077 MB after the graph build
(Round 3's peak), 1,585 MB after node texts, then **6,026 MB by the tail of
`upsert_nodes` + `upsert_edges`**. Five of the six gigabytes arrive in the
store-write path, and two contributors are ours rather than OverGraph's — both
row sets are materialised in full before the first batch is written.

## What landed

| # | Item | Measured | |
| :--- | :--- | :--- | :--- |
| P11.1 | `refresh_sparse_stats`: use the other 17 cores | **7.23 → 0.93 s (7.8×)**, three lines | ✅ |
| P11.2 | The incremental path could never hit without `--with-embed` | warm re-index rewrote 161,725 nodes → **0** | ✅ |
| P11.3 | `edge_rows`: build per batch, not all 745,964 up front | **−518 MB** | ✅ |
| P11.4 | `node_rows`: stream instead of materialising every row | node write **10.5 → 7.5 s**, −703 MB | ✅ |
| P11.5 | `capture_graph_code`: parallel, not a serial loop over 8,910 files | folded into the phase below | ✅ |
| P11.6 | Skip the edge write when the edge set is unchanged | warm **12.7 → 8.1 s**, −1.7 GB | ✅ |
| P11.7 | `build_texts`: parallel | **deferred — see below** | ⏭️ |
| P11.8 | Two silent phases, plus the untimed store open and commit | the 4.71 s mystery is **4.23 s of commit** | ✅ |
| P11.9 | `graph_id_set`: 161,725 id clones to build a prune set | ~23 MB of transient copies | ✅ |
| P11.10 | The index's content was not reproducible | **6/6 runs identical**, both fixtures | ✅ |

| `ug gen -i <neo4j> --no-cache` | before Round 4 | now | |
| :--- | ---: | ---: | ---: |
| **cold** — first index | 33.87 s / 6,117 MB | **20.12 s / 5,123 MB** | **1.68× / 1.19×** |
| **warm** — re-index, nothing changed | 46.01 s / 6,826 MB | **8.05 s / 2,633 MB** | **5.7× / 2.6×** |

Every phase of an ingest now has a name and a number:

```
▸ Opening the store:      ✓ done in    20 ms
▸ Building node texts:    ✓ done in  1.39 s
▸ Diffing against the DB: ✓ done in   148 ms
▸ Writing nodes:          ✓ done in  7.50 s
▸ Writing edges:          ✓ done in  1.78 s
▸ Pruning stale nodes:    ✓ nothing stale in 611 ms
▸ Building query indexes: ✓ done in  1.77 s
▸ Committing:             ✓ done in  4.23 s
```

`graph.json` is unchanged throughout: node map, edge multiset and `resolution`
all equal to a pre-Round-3 build.

<a id="edges-are-appended-never-replaced"></a>
### ⚠️ Found, not fixed: edges are appended, never replaced

**Every `ug gen` over an already-ingested store adds a complete duplicate copy
of the edge set.** Three runs over an unchanged repo:

| run | store edge counts | `graph.json` says |
| :--- | :--- | :--- |
| 1 | 3 Calls / 5 Contains | 3 / 5 |
| 2 | 6 Calls / 10 Contains | 3 / 5 |
| 3 | 9 Calls / 15 Contains | 3 / 5 |

`EdgeRow` carries an `id` (`source|type|target`) built for all 745,964 edges,
and `Db::upsert_edges` **never passes it to the engine** — OverGraph's
`EdgeInput` has no identity field at all (`from`, `to`, `label`, `props`,
`weight`, `valid_from`, `valid_to`), so `batch_upsert_edges` has nothing to
deduplicate on and appends. The node prune removes *nodes*; nothing removes
edges.

Confirmed pre-existing: a control where **both** runs wrote edges, with no
P11.6 skip involved, duplicates identically. On the neo4j fixture that is
+745,964 edges per re-index.

**P11.6 masks this in the common case and that is a hazard worth stating
plainly.** An unchanged re-index no longer duplicates, because it no longer
writes — but any run that *does* change something still doubles down. The
change is strictly an improvement over rewriting every time; it is not a fix.

The fix needs a design decision this round did not have room for: either track
the returned edge ids so the old ones can be deleted, or delete the graph's
edges before rewriting, or get edge identity into OverGraph's `EdgeInput`.
Until then `EdgeRow::id` is ~216 MB of string built per ingest and discarded.

### ⏭️ P11.7 deferred — `build_texts` parallelism changes retrieval semantics

`build_texts` threads a `&mut HashSet` (`seen_banner`) through the whole fold
so a licence header is indexed once rather than on every node. Two routes to
parallelising it, and both were rejected on inspection:

- **Per-file banner sets** would parallelise cleanly but change *attribution*:
  a header shared by 1,000 files would be indexed 1,000 times instead of once.
  That is a retrieval-quality change, not a performance one.
- **Extract in parallel, dedupe serially** does not decompose.
  `extract_prose_comments` interleaves the dedup with a running character
  budget and an early `break`, so a line skipped as a banner does not consume
  budget — the dedup decides *which* lines are kept and where the cut falls.

The phase is now 1.39 s of a 20.1 s command. Not worth changing what the index
contains to save part of it.

### What this round taught

- **A validation can be blocked by a bug in a different item.** P11.1 could not
  be checked by re-running `ug gen` and comparing, because P11.10 meant the
  term count was already unstable. Its test computes the serial and parallel
  forms over the same inputs **in one process** — the check the instability
  cannot reach. Fixing P11.10 first, as planned, was right.
- **A proxy metric survives only as long as its assumption.** P11.2's fix broke
  the "did this run skip embedding" check, which had been correct only while
  every run rewrote everything. It briefly made `ug gen` announce a vectorless
  index as search-ready. `IngestPlan::vectorless_kept` reports the state of the
  index instead of the delta of the run.
- **The store rejects a mis-sized vector at upsert**, so the case the old width
  check defended against cannot be constructed through the store's API — which
  is why `vector_is_reusable` is a pure function with unit tests.
- **Java had already fixed P11.10, in one extractor.** `java.rs` sorted its
  imports with a comment naming the exact failure; the other four languages
  never got the same treatment. The fix now lives in one shared helper so a
  sixth language cannot miss it.
- **Two independent causes wore the same symptom.** Making `graph.json`
  reproducible did *not* make the keyword index reproducible: the second cause
  was `build_node_sparse_vector` draining a `HashMap` and then
  `sort_unstable_by` + `truncate` over heavily tied scores, so a different
  subset of tied terms survived each run. Fixed by breaking ties on dimension.
- **Verifying a perf change found a correctness bug.** The edge duplication
  above was invisible until P11.6 required checking that the store still agreed
  with `graph.json`.


---

## Results log

One row per landed item or baseline. Keep the numbers, not just the verdict.

| Date | Item | Fixture | Before | After | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 2026-08-18 | P1.1 | synthetic n=1600 | 1166.1 ms | 5.0 ms | 235×; ratio widens with size |
| 2026-08-18 | P1.1 | `neo4j` (162k/746k) | never returned | 3,266 ms | first time this graph is scoreable |
| 2026-08-18 | P1.1 | — | all-zero output | correct | two bugs; betweenness had never worked |
| 2026-08-18 | P1.2 | synthetic n=8000 (parsed) | 13.4 ms | 1.6 ms | 8.4×; 2.8× when both parse JSON |
| 2026-08-18 | P1.3 | `neo4j` 2-hop traversal | 146.7 ms | 119.8 ms | 1.2× — estimate was wrong |
| 2026-08-18 | P1.3 | — | wrong hop distances | correct | LIFO walk recorded first-found, not shortest |
| 2026-08-18 | P3.1 | `neo4j` serve startup | 6.66 s | 0.32 s | 21×; graph-ready, not just `/healthz` |
| 2026-08-18 | P3.1 | `neo4j` idle RSS | 919 MB | 827 MB | unbuilt encodings cost nothing |
| 2026-08-18 | P3.2 | `neo4j` graph.json | already `br` | unchanged | premise wrong — no transfer win; progress bar fixed |
| 2026-08-18 | P3.3 | `/api/graph/stats` | 19.8 ms | 0.4 ms | 50×, memoised per snapshot |
| 2026-08-18 | P4.1 | MCP cached project | +346 MB retained | 0 | second full copy of graph.json, freed |
| 2026-08-18 | P4.2 | `mmr_rerank` | O(k²·n·d) | O(k·n·d) | output bit-identical, asserted vs reference |
| 2026-08-18 | P5.1 | hover, graph drawn whole | O(edges) each | O(degree) each | index built once per view change |
| 2026-08-20 | P6–P9 | 485k nodes, Chrome tab | 280 MB heap | 95 MB heap | 2.9×; median of 8, `usedJSHeapSize` |
| 2026-08-20 | P6–P9 | 485k nodes, index only (V8) | 426 MB peak / 338 MB held | 58 MB both | 7.3× peak; build 393 ms → 7 ms |
| 2026-08-20 | P6–P9 | 485k nodes, load → interactive | 2,485 ms | 1,531 ms | 1.6× |
| 2026-08-20 | P6–P9 | cold click, degree-8,680 hub | 69 ms | 38 ms | 1.8×, once the post-load GC has settled |
| 2026-08-20 | P6.5 | hub expansion (`soloViewIds`) | 2.9 ms | 1.2 ms | beats HEAD; 17k needless lookups removed |
| 2026-08-20 | P7.1 | 485k node index, wire | 98.2 MB (7.1 MB gz) | 51.7 MB (9.7 MB gz) | half the bytes held, 2.6 MB more transferred |
| 2026-08-20 | P7.2 | `neo4j` ids, front-coded | 21.8 MB | 8.4 MB | 2.6×, 16-entry restart |
| 2026-08-20 | P8.2 | keyword search, 485k nodes | 77 ms on the main thread | 82 ms, none of it on it | same wall clock, responsive tab |
| 2026-08-20 | — | client front-coding decoder | silently wrong ids | correct | scratch grew without copying |
| 2026-08-24 | — | `ug gen --no-ingest` neo4j | 3,378 MB / 5.17 s | — | Round 3 baseline; 10× the 330 MB it writes |
| 2026-08-24 | — | `ug serve` neo4j, idle RSS | 1,245 MB | — | baseline; before any request is served |
| 2026-08-24 | — | `graph.json` edge endpoints | 1.49M `String`s / ~252 MB | — | 161,725 distinct values; ~49 MB interned |
| 2026-08-25 | P10.1+P10.3 | `ug gen` neo4j, peak RSS | 3,378 MB | 1,618 MB | 2.09×; typed pipeline, no JSON seams |
| 2026-08-25 | P10.7+P10.8 | `ug gen` neo4j, wall clock | 4.14 s | 2.72 s | cumulative; 1.90× on HEAD |
| 2026-08-25 | P10.4 | `ug serve` neo4j, idle RSS | 916 MB | 730 MB | interned at parse; `Arc<str>`, not indices |
| 2026-08-25 | P10.4 | `ug gen` neo4j, peak RSS | 1,458 MB | 1,161 MB | interning at *push* time, not after |
| 2026-08-25 | P10.5 | `ug serve` neo4j, idle RSS | 1,238 MB | 916 MB | mmap; dropping the buffer alone was worth 7 MB |
| 2026-08-25 | P10.9 | `/api/graph/search?limit=200` | 133.0 KB | 24.8 KB | 5.4×; `?fields=id` |
| 2026-08-25 | **Round 3** | `ug gen --no-ingest` neo4j | 3,378 MB / 5.17 s | **1,161 MB / 4.18 s** | **2.91× peak**, 1.24× wall; output equal |
| 2026-08-25 | **Round 3** | `ug serve` neo4j idle | 1,245 MB | **730 MB** | **1.70×**; startup 0.31 s → 0.62 s |
| 2026-08-25 | **Round 3** | `/api/graph/search?q=nodepr` | 23.8 ms | **0.44 ms** | **54×**; `count` identical |
| 2026-08-25 | — | `ug gen` reproducibility | non-deterministic | unchanged | pre-existing at HEAD; 5 samples. Not fixed |
| 2026-08-25 | — | `ug gen` **default** (with ingest) | 33.87 s / 6,117 MB | — | Round 4 baseline; ingest is 87% of the command |
| 2026-08-25 | — | …of which `upsert_nodes` | 10.42 s | — | 35% of ingest; inside OverGraph 0.17 |
| 2026-08-25 | — | …untimed store open + commit | 4.71 s | — | 16% of ingest with no phase name |
| 2026-08-25 | P11.1 | `refresh_sparse_stats` | 7.23 s | 0.93 s | **7.8×**; equality with the serial pass pinned in-process |
| 2026-08-25 | P11.1 | "Building node texts" phase | 8.93 s | 2.62 s | 3.4× |
| 2026-08-25 | P11.3 | `ug gen` cold, peak RSS | 6,117 MB | 5,599 MB | 518 MB; rows built per batch |
| 2026-08-25 | P11.2 | warm re-index, nodes rewritten | 161,725 | **0** | `0 unchanged` → `161725 unchanged` |
| 2026-08-25 | P11.2 | — | reported "embedded", cleared `pendingVectors` | reports 161,725 owed | bug the change opened; `vectorless_kept` closes it |
| 2026-08-25 | P11.10 | sparse-index terms, 5 runs, same binary | 84,481–84,519 | — | the index *content* is nondeterministic, not just its order |
| 2026-08-25 | **Round 4 Ph0** | `ug gen` **cold** | 33.87 s / 6,117 MB | **27.07 s / 5,743 MB** | **1.25×** wall |
| 2026-08-25 | **Round 4 Ph0** | `ug gen` **warm** | 46.01 s / 6,826 MB | **16.63 s / 4,369 MB** | **2.77× wall, 1.56× peak**; graph.json equal |
| 2026-08-26 | P11.10 | `graph.json`, 6 runs, ug repo | 6 distinct | **1** | reproducible; four extractors were missing Java's sort |
| 2026-08-26 | P11.10 | sparse-index terms, 4 runs | 9,340–9,364 | **9,331 ×4** | second cause: tied scores truncated in HashMap order |
| 2026-08-26 | P11.4 | `Writing nodes` phase | 10.54 s | 7.50 s | streamed; −703 MB peak, and *faster* |
| 2026-08-26 | P11.8 | the untimed 4.71 s | unnamed | **4.23 s commit** + 0.02 s open | every ingest phase now has a name |
| 2026-08-26 | P11.6 | warm re-index | 12.68 s / 4,346 MB | **8.05 s / 2,633 MB** | edge write skipped; commit 4.25 s → 0.03 s |
| 2026-08-26 | **Round 4** | `ug gen` **cold** | 33.87 s / 6,117 MB | **20.12 s / 5,123 MB** | **1.68× / 1.19×** |
| 2026-08-26 | **Round 4** | `ug gen` **warm** | 46.01 s / 6,826 MB | **8.05 s / 2,633 MB** | **5.7× / 2.6×**; graph.json equal |
| 2026-08-26 | — | store edge count over 3 re-indexes | 5 → 10 → 15 | unchanged | **bug, not fixed**: edges append, never replace |

---

## Rejected / deferred

Move items here rather than deleting them, with the measurement that killed
them — so the next audit does not re-propose them.

| Item | Why |
| :--- | :--- |
| Storage **query** layer / PPR / vector index tuning | Measured 2026-08-16: not the bottleneck at 162k nodes. `/api/graph/cycles` over 746k edges runs in 0.33 s. (The *write* path is a different story — see Round 4.) |
| Chat retrieval latency | Measured 2026-08-16: wall time is entirely local-LLM tokens (~83 tok/s decode). Tool execution was 0.1 s across 4 calls. |
| Index identity for server mode (P9.2) | Would take the 485k index from 58 MB to ~18 MB and the wire from 51.7 to 9.5 MB — the biggest remaining client win. Deferred because identity is not a client-side detail: eight endpoints return real qualified ids, the deep-link URL format is one, and 60+ client call sites pass them around opaquely. Revisit when a real 500k index exists to test against. |
| Allocation-free lowercase scan in `api_search` | Measured 2026-08-24: **slower**, 23.3 → 33.0 ms. `to_lowercase()` feeds `str::find`, a tuned two-way/`memchr` search; a hand-rolled ASCII byte loop loses by more than the allocation costs. The scan was never the allocation. |
| Interning edge endpoints *after* the build | Measured 2026-08-25: **no change in peak** (1,458 → 1,456 MB) for **+1.6 s**. Every duplicate is already allocated by then, and freeing them does not return the memory. Interning has to happen as each edge is made or read. |
| `serde_json::from_reader` for the snapshot parse | Measured 2026-08-25: avoids the 346 MB buffer, but startup **0.31 → 1.33 s** — four times HEAD, giving back most of what P3.1 bought. `memmap2` gets the same memory at 0.48 s. |
| `build_texts` parallelism (P11.7) | Deferred 2026-08-26. `seen_banner` is order-dependent shared state: per-file sets change *attribution* (a header shared by 1,000 files would be indexed 1,000 times), and extract-then-dedupe does not decompose because `extract_prose_comments` interleaves the dedup with a character budget and an early `break`. 1.39 s of a 20.1 s command — not worth changing what the index contains. |
| `GraphEdge` endpoints as `u32` node indices | Would reach ~6 MB against `Arc<str>`'s ~49 MB, but needs the node table wherever an edge is built or read — 41 construction sites, 144 reads, ten test files — and a seeded or two-pass `Deserialize`, because an edge would stop meaning anything on its own. Revisit only if 49 MB on a 500k-edge graph starts to matter. |
