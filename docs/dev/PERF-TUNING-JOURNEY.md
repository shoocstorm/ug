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
| **Status** | Rounds 1–3 landed; Round 4 Phase 0 landed. Suite **899/899**. P11.4–P11.10 open |

**Status marks:** ✅ landed and verified · ⬜ open · ⏭️ deferred · ❌ rejected by measurement

---

## Where things stand

Against the state before Round 1. On `~/.ug/neo4j` except the two browser
rows, which are Round 2's synthetic 485k-node index (~3× neo4j).

| Surface | Before | Now | |
| :--- | ---: | ---: | ---: |
| `ug gen` **cold** (default, with ingest) | 33.87 s / 6,117 MB | **27.07 s / 5,743 MB** | 1.25× |
| `ug gen` **warm** (re-index, nothing changed) | 46.01 s / 6,826 MB | **16.63 s / 4,369 MB** | **2.77× / 1.56×** |
| `ug gen --no-ingest` | 5.17 s / 3,378 MB | **4.18 s / 1,161 MB** | 1.24× / **2.91×** |
| `ug serve` idle RSS | 1,245 MB | **730 MB** | **1.70×** |
| `ug serve` startup to graph-ready | 6.66 s | **0.62 s** | 10.7× |
| Browser tab, 485k nodes | 280 MB heap | **95 MB** | **2.9×** |
| …load → interactive | 2,485 ms | **1,531 ms** | 1.6× |
| `/api/graph/search` per keystroke | 23.8 ms / 133 KB | **0.44 ms / 24.8 KB** | **54× / 5.4×** |
| `ug graph centrality` | never returned | **3.3 s** | — |

**The largest remaining numbers**, all in the ingest path: `upsert_nodes` at
10.5 s (inside OverGraph 0.17, 46% of a cold ingest) and a ~5.7 GB peak that
[P11.4](#p114--node_rows-materialised-in-full-each-carrying-a-copy-of-its-source)
is aimed at.

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

## Landed — Phase 0

| # | Item | Measured | |
| :--- | :--- | :--- | :--- |
| P11.1 | `refresh_sparse_stats`: use the other 17 cores | **7.23 → 0.93 s (7.8×)**, three lines | ✅ |
| P11.2 | The incremental path can never hit without `--with-embed` | warm re-index 46.0 → 16.6 s, −2.46 GB | ✅ |
| P11.3 | `edge_rows`: build per batch, not all 745,964 up front | **−518 MB**, estimate said ~550 | ✅ |

| `ug gen` | before | after | |
| :--- | ---: | ---: | ---: |
| **cold** — first index | 33.87 s / 6,117 MB | **27.07 s / 5,743 MB** | 1.25× |
| **warm** — re-index, nothing changed | 46.01 s / 6,826 MB | **16.63 s / 4,369 MB** | **2.77× / 1.56×** |
| nodes rewritten on a warm run | 161,725 | **0** | |

`graph.json` unchanged: node map, edge multiset and `resolution` all equal.

### P11.2 — the item, and the bug it opened

**A second `ug gen` over an unchanged repo did no incremental work at all.** It
reported `0 unchanged, 0 moved, 161725 to embed` — zero unchanged out of
161,725 byte-for-byte identical nodes.

Two lines a hundred apart that could not both hold. With embedding off — the
default — rows are written with an empty vector on purpose. The reuse gate
required a vector of exactly the store's width. 0 ≠ 384, forever, so
`stored_row_matches` (which compares facts, code and file hash with real care)
was unreachable on the default path.

The gate now distinguishes *no vector* from *wrong-width vector*, via a named
`VectorPlan` enum rather than a bool — it would have sat next to an existing
`always_write: bool` in a seven-argument function.

**Then the fix opened a worse bug, in the direction that was flagged as
dangerous.** `ug gen` decides what to tell the user by branching on "how many
nodes did *this run* write without a vector". That was a fine proxy only while
every run rewrote everything. Once a vector-less row can be recognised as
unchanged, a warm re-index embeds nothing *and writes nothing*, so the count
reads zero and the success branch fires:

```
✓ 161725 nodes, 745964 edges embedded in 12.69s
```

Nothing was embedded. Worse than the wrong sentence: that branch clears
`pendingVectors` and returns `EmbeddingsOutcome::Ready`, so the project would
have recorded a vectorless index as search-ready, and the ingest model would
have been stamped onto it.

Fixed by reporting **the state of the index** rather than the delta of the run:
`IngestPlan` carries `vectorless_kept`, and both ingest paths report
`vectors_skipped + vectorless_kept`. The warm run now says `161725 awaiting
vectors` and keeps its `pendingVectorsSince` stamp.

### What Phase 0 taught

- **A validation can be blocked by a bug in a different item.** P11.1 cannot be
  checked by re-running `ug gen` and comparing, because P11.10 means the term
  count is already unstable — 84,481 to 84,519 across five runs of the same
  binary. The test computes both the serial and parallel forms over the same
  inputs **in one process** and compares `total_docs`, `terms()` and every
  dimension's document frequency. That is the check the instability cannot
  reach.
- **The store rejects a mis-sized vector at upsert** (`BadVector { got: 7,
  want: 384 }`), so the case the width check defends against cannot be
  constructed through the store's API at all — it only arises from a store
  whose dim changed under rows already written. That is why `vector_is_reusable`
  is a pure function with unit tests rather than an integration test.
- **A proxy metric survives only as long as its assumption.** `vectors_skipped`
  was correct for exactly as long as every run rewrote everything.

## Open

### ⬜ P11.4 — `node_rows` materialised in full, each carrying a copy of its source
<a id="p114--node_rows-materialised-in-full-each-carrying-a-copy-of-its-source"></a>

**Where:** `native/src/storage/ingest.rs` (`IngestPlan::finish`),
`native/src/cli/ingest.rs` (the node write loop).

`plan.finish` returns a `Vec<NodeRow>` for **every** node before the first
batch is written, and `NodeRow.code` is the symbol's whole body — already in
memory once, in `captured`, which is held across the phase. So every symbol's
source exists twice at the peak, and nested symbols mean a class's copy
contains its methods' copies again. `node_text` is likewise already in `texts`.

Same shape as P11.3, and the RSS curve says it is the larger half: 1,585 MB
before the phase, 3,009 MB by the time the node writes finish.

**Fix.** Produce rows per batch. The awkward part is that `reusable` and
`to_embed` are two lists that together cover the node set, so batching has to
walk both — an iterator or a `finish_chunk(range)` rather than a plain
`chunks()`.

**Prove it.** Peak RSS, and row-for-row equality of what reaches
`upsert_nodes`. **Risk:** low. Do it after P11.2, which changes which bucket
most nodes land in.

### ⬜ P11.5 — `capture_graph_code`: the repo's second full read and blake3

**Where:** `native/src/storage/source.rs`.

Every source file in the repo, read and blake3-hashed — for the **second time
in the same `ug gen` run**. `indexer::process_file` already did exactly this
and stored the result as `FileNode::hash`. **P2.2 in Round 1 was "stop reading
+ hashing every file twice"** and fixed the duplicate *inside the indexer*;
this one is three stages later, so it was never in that item's blast radius.
It is also a serial loop over 8,910 independent files.

**Fix.** Two independent halves: `par_iter` over the per-file work, and carry
`FileNode::hash` forward so the rehash goes away (the read stays — the slices
need the content).

**Prove it.** Phase timing, and `file_hash` equality for every node — they hash
the same bytes, so they must be identical. **Risk:** low.

### ⬜ P11.6 — Edges are rewritten in full on every run, always

**Where:** `native/src/cli/ingest.rs` (the edge write loop).

There is no incremental plan for edges. Nodes get a careful diff; edges get an
unconditional upsert of all 745,964 rows every run — including the run where
the node diff correctly reports everything unchanged. In the P11.2 measurement
the run that skipped the node write entirely still spent 3.39 s here.

**Fix.** An edge is `(source, target, type)` with no derived state — no vector,
no facts, no captured code — so "has the edge set changed" is a set comparison.
A digest of the sorted triples, stored alongside the ingest model: equal
digest, skip the phase.

**Prove it.** Ingest twice, assert the second writes no edges and `traverse`
answers identically; then delete one edge and assert it does write.
**Risk:** medium — a stale skip leaves the store disagreeing with `graph.json`,
which is a failure mode nothing else here has. A prune must invalidate it.

### ⬜ P11.7 — `build_texts`: serial, and it runs before the diff

**Where:** `native/src/storage/ingest.rs`.

The order is capture → build texts → **then** diff, so 1.24 s of text building
is paid in full even on a run that will conclude nothing changed. It has to be,
in part — the diff compares `prev.node_text == *text`, so the text is the diff
key — but that argues for making it cheap, not for leaving it serial.

Not the one-liner P11.1 was: `seen_banner` is a `&mut HashSet` threaded through
the whole fold, deliberately, so a licence header is indexed once per file
rather than once per node. The natural shape is per-file parallelism with a
per-file banner set, which is closer to what the dedup means anyway.

**Prove it.** Texts byte-identical to the serial build, including which node
won each banner. **Risk:** low-to-medium, entirely because of `seen_banner`.

### ⬜ P11.8 — Two phases the progress meter never mentions

**Where:** `native/src/cli/ingest.rs`, after the edge write.

`prune_to_graph` is timed but only *prints* when it removed something, so a
first ingest reports nothing despite a full scan. `ensure_query_indexes()` has
no timing and no output ever. Measured **0.94 s** and **1.69 s** — 2.6 s of a
29.6 s command the user watches in silence. A further **4.71 s** is inside
`ingest_with_specs` with no phase name at all: store open,
`reset_stale_format_stores`, and the commit that happens on drop.

**Fix.** Print both unconditionally in the established `▸ … ✓ done in {:?}`
shape — "nothing stale" is a useful thing to have said. Time the store open and
the commit; the commit especially, because it is where a large ingest appears
to hang. **Risk:** very low. It is output.

### ⬜ P11.9 — `graph_id_set`: 161,725 id clones to build a prune set

**Where:** `native/src/storage/ingest.rs`.

~23 MB of transient copies of ids that `graph.nodes` already owns, to build a
set the callee only reads. `prune_nodes_absent_from` takes `&HashSet<String>`,
so this is a signature question rather than a body one. **Risk:** very low,
bounded by whether the trait method can change shape.

<a id="p1110--the-indexs-content-is-not-reproducible-either"></a>
### ⬜ P11.10 — The index's *content* is not reproducible either

Extends [the Round 3 finding](#graphjson-is-not-reproducible), which is about
ordering. The keyword-term count over five runs of the same binary against the
same unchanged repo:

```
84,481   84,488   84,515   84,518   84,519
```

Round 3 established that `graph.json`'s **order** varies and its content was
stable. This is worse: the number of distinct terms in the sparse index differs
by run, so the **BM25 statistics and the stored sparse vectors are not the same
index twice**. Two developers indexing the same commit get keyword search that
ranks differently.

The likely path is the same `HashMap` iteration order — `seen_banner` in
`build_texts` decides which node a file-level comment is attributed to, and a
different attribution changes which terms that node contributes.

**Fix.** Ordered containers where an ordered output is derived, plus a test
that indexes the same fixture twice and compares `SparseStats`, not just the
graph bytes.

**Risk:** low to fix, and **it should be fixed before P11.7 lands** — a
parallel pass over data whose serial result is already unstable is much harder
to validate and much easier to blame. P11.1 already had to work around it.

### Smaller things found in the same pass

- `upsert_nodes` is **10.42 s**, the largest single phase, and it is inside
  OverGraph 0.17 rather than this crate. The one lever on our side is
  `write_batch = 1000`, never swept. Worth one experiment at 200 / 1,000 /
  5,000 / 20,000 before concluding the cost is the store's.
- `plan_incremental_ingest` builds its `NodeKey` list with an id clone and a
  type `to_string()` per node per chunk — the same shape as P11.9.
- `EdgeRow::id` is `source|type|target`, a third copy of both ids (~290 chars)
  per edge. If the store can key an edge on the triple rather than a
  concatenation, that is another ~216 MB off P11.3's number.
- `graph_keyword_search` and `filter_edges_by_type` still have no caller
  outside their own ~20 tests (P10.10 fixed their defects but not this).
  Deleting public API is a semver decision, not a performance one — but the
  capability exists at `/api/graph/search` and in the MCP tools, done better.

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
| `GraphEdge` endpoints as `u32` node indices | Would reach ~6 MB against `Arc<str>`'s ~49 MB, but needs the node table wherever an edge is built or read — 41 construction sites, 144 reads, ten test files — and a seeded or two-pass `Deserialize`, because an edge would stop meaning anything on its own. Revisit only if 49 MB on a 500k-edge graph starts to matter. |
