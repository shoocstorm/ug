# Performance Tuning Journey

> The shared ledger for performance work on `ug`. One section per round, each
> with the baseline it started from, what landed with its measured number, and
> what the round taught that would otherwise be re-learned. Items are numbered
> `P<group>.<n>` continuously across rounds — Round 1 opened at P1, Round 5
> opened at P12.
>
> **Two standing rules.** Nothing lands without a measurement, and nothing is
> deleted when it fails — it moves to [Rejected / deferred](#rejected--deferred)
> with the number that killed it, so the next audit does not re-propose it.

| Field | Value |
| :--- | :--- |
| **Opened** | 2026-08-18 |
| **Version** | 0.1.16 |
| **Primary fixture** | `~/.ug/neo4j` — 161,725 nodes / 745,964 edges / 330 MB `graph.json` |
| **Status** | Rounds 1–4 landed (bar P11.7, deferred). Round 5: **P12.1, P12.3, P12.6, P12.7, P12.8, P12.9, P12.10, P12.11, P12.12, P12.13 landed** (P12.7 half-reverted — see its note); P12.2, P12.4, P12.5 audited. Suite **912/912** |

**Status marks:** ✅ landed and verified · ⬜ open · ⏭️ deferred · ❌ rejected by measurement

---

## Where things stand

Against the state before Round 1. On `~/.ug/neo4j` except the two browser
rows, which are Round 2's synthetic 485k-node index (~3× neo4j).

| Surface | Before | Now | |
| :--- | ---: | ---: | ---: |
| `ug gen` **cold** (default, with ingest) | 33.87 s / 6,158 MB | **20.23 s / 4,997 MB** | **1.67× / 1.23×** |
| `ug gen` **warm** (re-index, nothing changed) | 46.01 s / 6,826 MB | **8.05 s / 2,633 MB** | **5.7× / 2.6×** |
| `ug gen --no-ingest` | 5.17 s / 3,378 MB | **4.18 s / 1,161 MB** | 1.24× / **2.91×** |
| `ug serve` idle RSS | 1,245 MB | **730 MB** | **1.70×** |
| `ug serve` startup to graph-ready | 6.66 s | **0.62 s** | 10.7× |
| Browser tab, 485k nodes | 280 MB heap | **62 MB** | **4.5×** |
| …load → interactive | 2,485 ms | **1,105 ms** | 2.2× |
| Browser **renderer process**, 485k | 5,367 MB | **314 MB** | **17.1×** |
| `/api/graph/search` per keystroke | 23.8 ms / 133 KB | **0.44 ms / 24.8 KB** | **54× / 5.4×** |
| `ug graph centrality` | never returned | **3.3 s** | — |

**The largest remaining number** is `upsert_nodes` at 7.5 s — 37% of a cold
ingest, and inside OverGraph 0.17 rather than this crate.

> **Fixed 2026-08-26 (P11.13):** `ug gen` used to duplicate every edge in the
> store on each re-index, and never delete one that had gone. See
> [Edges are appended, never replaced](#edges-are-appended-never-replaced).

---

## Fixtures and harness

| Fixture | `graph.json` | Notes |
| :--- | :--- | :--- |
| `~/.ug/java-demo` | 108 KB | Smoke-test size |
| `~/.ug/ug` | 4.6 MB | ~4,400 nodes — the local-mode case |
| `~/.ug/overgraph` | 16 MB | Mid-size |
| `~/.ug/neo4j` | 330 MB | 161,725 nodes / 745,964 edges — the stress case |
| `~/.ug/big500k` | 1,049 MB | 485,175 / 2,237,892 — `~/.ug/neo4j` tripled with a 2-char id shard prefix, so string lengths and degrees stay real and only the count scales. The 500k target, and what P9.2/P12.5 were waiting for |

- `cargo` root is `native/`; the suite runs via `cargo nextest run` (thread
  count lives in `.config/nextest.toml` — do not pass `-j`).
- **The RTK hook rewrites `cargo test` and filters its stdout**, which strips
  `println!` — a bench appears to run and report nothing. Wrap the whole
  command in `rtk proxy "..."` for raw output. The same filter mangles long
  `grep` output.
- **Every number in this file is `target/release/ug`.** A debug build is
  3–5× slower on this pipeline and *unevenly* so, which makes it worse than
  merely slow for comparisons: `[profile.dev.package."*"] opt-level = 2`
  optimizes every dependency and leaves `ultragraph` itself at opt-level 0, so
  phases that are our code run 3–5.4× slower while phases that are purely
  OverGraph run at **1.0×**. Measured on the same input: `Building query
  indexes` 2,105 ms debug vs 2,082 ms release; `Writing nodes` 1,382 vs 258.
  A debug profile therefore doesn't scale the phase table, it reshapes it.
- Micro-benches live at `native/tests/graph_bench.rs` and
  `storage_bench.rs`, both `#[ignore]`d, plain `Instant`, no Criterion. Use
  `--release`; a debug build measures rustc's bounds checks. `UG_BENCH_ALL=1`
  lifts the 20k-node fixture cap.
- Browser heap: `node --expose-gc`, run the structure, drop the payload,
  `gc()` three times, read `heapUsed`. Node's V8 is the page's engine. Tab
  totals via Chrome `--enable-precise-memory-info`, median of 8.
- **`usedJSHeapSize` is not the tab.** Blink's DOM is outside the JS heap, so
  a page of 9M elements reads 341 MB of heap inside a 5,367 MB renderer
  process. Take `Performance.getMetrics` → `Nodes` and `JSEventListeners`
  alongside it, and the RSS of the process matching `--type=renderer`.
- The browser harness is: headless Chrome over CDP (`WebSocket` + `fetch`, no
  puppeteer, `--enable-unsafe-swiftshader`), behind a small reverse proxy in
  front of `ug serve` that injects `window.__ug = { state, … }` before the
  page's last `</script>` — module scope is unreachable from
  `Runtime.evaluate` otherwise. The same proxy can `String.replace` a patch
  into the served page, which is how a client-side fix gets a number before
  anyone rebuilds. Wait for the `#loading` element to *exist* before reading
  its `display`, or the probe reports a load time of 0 ms.
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
- **The corollary: allocation *churn* does not show up in peak RSS at all.**
  Millions of short-lived allocations that are freed promptly never raise the
  high-water mark, so removing them never lowers it. P11.11 and P11.12 together
  removed ~4.5M `String` allocations and moved peak RSS by **15 MB against a
  113 MB run-to-run spread** — nothing. They are worth 0.32 s of CPU, and that
  is the only claim they should carry. Peak RSS is set by the largest
  *simultaneous live set*; churn is a CPU cost wearing a memory-shaped mask.
- **Units: every RSS figure here is MB (10⁶), from `/usr/bin/time -l`'s
  `maximum resident set size`, which is bytes on macOS.** `ps -o rss=` reports
  KB, so the 0.2 s sampler's numbers are MiB unless converted — mixing the two
  silently inflated a reported win by 2× once. Take a median of ≥5; the spread
  on a 5 GB workload is ~100 MB.
- **`str::find` beats a hand-rolled scan.** `to_lowercase()` + `find` is a
  tuned two-way/`memchr` search; the allocation is not the expensive half.
- **`ug gen` output is reproducible as of P11.10**, and was not before it —
  see [P11.10](#p1110--the-indexs-content-is-not-reproducible-either) for the
  two independent causes. Correctness checks still compare the node map, edge
  multiset and `resolution` rather than bytes.
- **The GPU is not the constraint in the vis layer and never has been.** Solo
  mode caps what any renderer is handed at 1,500 nodes past 200k elements, and
  the full GPU-side payload below that is ~40 B per point and ~28 B per link.
  Every cost measured in the browser has been CPU, JS heap or DOM — see
  [Round 5](#round-5--the-vis-layer).

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
| **peak RSS** | **6,158 MB** |
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
| P11.7 | `build_texts`: parallel | **deferred — 3.3% of the command, 1.7% reachable** | ⏭️ |
| P11.8 | Two silent phases, plus the untimed store open and commit | the 4.71 s mystery is **4.23 s of commit** | ✅ |
| P11.9 | `graph_id_set`: 161,725 id clones to build a prune set | ~23 MB of transient copies | ✅ |
| P11.10 | The index's content was not reproducible | **6/6 runs identical**, both fixtures | ✅ |
| P11.11 | `collect_related_names`: borrow instead of 4 `String`s per edge | ~3M allocations gone | ✅ |
| P11.12 | Sweep for the same pattern everywhere (`FactContext`, two traversals) | with P11.11: **−0.32 s (1.6%)**, and **no** measurable memory change | ✅ |
| P11.13 | Edges: `edge_uniqueness` + an edge prune | **correctness** — the store now matches `graph.json` after any re-index | ✅ |

| `ug gen -i <neo4j> --no-cache` | before Round 4 | now | |
| :--- | ---: | ---: | ---: |
| **cold** — first index | 33.87 s / 6,158 MB | **20.23 s / 4,997 MB** | **1.67× / 1.23×** |
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
### ✅ P11.13 — edges are appended, never replaced

Found while verifying P11.6. Three runs over an **unchanged** repo:

| run | store | `graph.json` says |
| :--- | :--- | :--- |
| 1 | 3 Calls / 5 Contains | 3 / 5 |
| 2 | 6 Calls / 10 Contains | 3 / 5 |
| 3 | 9 Calls / 15 Contains | 3 / 5 |

**Two bugs wearing one symptom**, and fixing the first exposed the second.

**1. Nothing identified an edge.** `NodeInput` carries a `key` the engine
dedupes on; `EdgeInput` has no such field, so every `batch_upsert_edges`
*created*. The `EdgeRow::id` we build for all 745,964 edges was never passed to
the engine at all.

The fix was not the workaround it looked like. OverGraph supports edge identity
— `DbOptions::edge_uniqueness`, **default `false`** — and
`plan_batch_upsert_edges` honours it properly, resolving a whole batch against
existing triples in one `find_existing_edges_batch` rather than a lookup per
edge. Reading the engine source settled that; the API reference only documents
the flag against the *singular* `upsert_edge`.

`DbOptions` is persisted into the manifest on **first open** and later opens
ignore what they pass, so an existing store keeps duplicating forever however
new the binary is — hence `STORE_FORMAT_VERSION` 3 → **4**. A v3 store is also
already wrong (it holds one copy of the edge set per `ug gen` that ever ran),
so this is the quiet-wrongness case the version gate exists for.

**2. Nothing deleted an edge that had gone.** `edge_uniqueness` stops an edge
being written twice; it does nothing about one that no longer exists. Deleting
a *node* takes its edges with it via tombstoning — but a call removed from a
body leaves both endpoints alive, so nothing tombstoned it and the store kept
answering `find_usages` with a caller that no longer calls. `prune_edges_to_graph`
is the edge counterpart of the node prune, gated on P11.6's digest so an
unchanged run does not enumerate 746k edges to delete none.

**Verified** by walking a fixture through every state, comparing the store
against `graph.json` at each:

| | store | `graph.json` |
| :--- | :--- | :--- |
| two functions, one call | Calls 1 / Contains 4 | Calls 1 / Contains 4 |
| add a function (edges grow) | Calls 3 / Contains 5 | Calls 3 / Contains 5 |
| delete it *and* a call (edges shrink) | Contains 4 | Contains 4 |
| restore the call | Calls 1 / Contains 4 | Calls 1 / Contains 4 |

Six *forced* rewrites of the same graph leave the live count at exactly 5 and
the store at 72 KB — no growth at all.

**One observation, not a regression.** At the 746k-edge scale, repeatedly
forcing the edge write (stripping the digest) grows the store on disk —
937 MB → 2.7 GB over five. The live edge set is correct throughout; this is
segment and WAL churn awaiting compaction, and the small-fixture run above
shows compaction keeping up when the data is small. It also cannot happen in
normal use: P11.6 skips the edge write entirely when nothing changed. Worth
knowing before anyone bypasses the digest in a loop.

### ⏭️ P11.7 deferred — measured, not argued

**Status: not done, and the case for doing it got weaker.**

The first deferral was on design grounds without measuring the split inside the
phase. That was the wrong order. Instrumented, the "Building node texts" phase
on the neo4j fixture breaks down as — before, and after [P11.11](#p1111) landed
inside it:

| part | at deferral | now | shared state? |
| :--- | ---: | ---: | :--- |
| `collect_related_names` | 371 ms | **230 ms** | no — P11.11 took 141 ms out |
| `extract_prose_comments` | 258 ms | 250 ms | **yes — `seen_banner`** |
| `build_node_text_with_comments` | 220 ms | 196 ms | no |
| **`build_texts` total** | **849 ms** | **677 ms** | |
| phase total (incl. capture + sparse stats, already parallel) | 1.51 s | **1.32 s** | |

So `build_texts` is now **677 ms of a 20.4 s command — 3.3%** — and 250 ms of
that is genuinely blocked by `seen_banner`. Parallelising everything that
*isn't* blocked is worth ~350 ms at the theoretical limit: **1.7%**, for a
change that touches how the embedding text is built.

That settles it on arithmetic rather than on the design argument, which was
also true but weaker:

- **Per-file banner sets** parallelise cleanly but change *attribution* — a
  header shared by 1,000 files would be indexed 1,000 times instead of once.
  A retrieval-quality change, not a performance one.
- **Extract in parallel, dedupe serially** does not decompose.
  `extract_prose_comments` interleaves the dedup with a running character
  budget and an early `break`, so a line skipped as a banner does not consume
  budget: the dedup decides *which* lines are kept and where the cut falls.
  Exact equivalence needs an unbounded first pass, which trades the time for
  memory in a round spent buying memory back.

Not worth changing what the index contains, or holding, for 1.7%. Revisit only
if `build_texts` ever becomes a large share of the command again — which
P11.11 made *less* likely, not more.

<a id="p1111"></a>
### ✅ P11.11 / P11.12 — the clone-to-key sweep, and a corrected claim

Measuring P11.7 turned up that the biggest piece of that phase was not the one
under discussion. `collect_related_names` walked all 745,964 edges calling
`edge.source.to_string()` to key a map — a ~141-character id allocated only to
be hashed against an entry that already exists — twice per edge, plus two more
for the values. ~3 million allocations.

A scan for the same shape across `native/src` then found three more:

| site | what it did | per |
| :--- | :--- | :--- |
| `storage/facts.rs` `FactContext::new` | `e.source.to_string()` / `e.target.to_string()` to key three degree maps — **and it is built twice per ingest** | ~1.5M allocations per call |
| `agent_tools/traverse.rs` | `et.to_lowercase()` to compare a `&'static str` against an already-lowercased filter | one `String` per edge |
| `agent_tools/find_usages.rs` | the same | one `String` per edge |
| `cli/graph_algos.rs` | `node_type_str(..).to_lowercase()` against a lowercased list | one `String` per node |

All now borrow, or compare with `eq_ignore_ascii_case`.

**The result is smaller than first reported, and the correction is the point.**
Measured properly — five runs each side, medians, one consistent unit:

| | before | after | |
| :--- | ---: | ---: | ---: |
| `ug gen` cold, wall | 20.55 s | **20.23 s** | −0.32 s (**1.6%**) |
| `ug gen` cold, peak RSS | 5,012 MB | 4,997 MB | −15 MB — **noise** |

The time win is real: the gap exceeds the run-to-run spread on the before side
(0.14 s). The memory win is not: 15 MB against a 113 MB spread.

Two earlier claims for P11.11 — **−470 MB**, then **−245 MB** — were both
wrong, and wrong for two different reasons worth recording:

1. **Mixed units.** The "before" came from `time -l` bytes ÷ 10⁶ (MB) and the
   "after" from `ps` KB ÷ 1024 (MiB). Comparing them inflated the gap ~2×.
2. **Single samples against a ~100 MB spread.** Even in consistent units, one
   run per side cannot resolve a difference this size.

The underlying reason there is no memory win at all is the corollary in
[Standing conclusions](#standing-conclusions): ~4.5M allocations that are
freed promptly never raise the high-water mark, so removing them never lowers
it. Churn is a CPU cost, not a memory one.

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

# Round 5 — The vis layer

**2026-08-27 · baseline `3e8f441` · 8 items, P12.1 / P12.3 / P12.6 / P12.7 landed · suite 912/912**

Scope was the browser: WebGL, wasm, worker threads, GPU-friendly data
structures, and what stands between `ug` and a 500k-node graph. Round 2 took
the tab's **JS heap** from 280 MB to 95 MB at that size and concluded the
renderer was never the problem. Both halves of that are still true. Both
missed what the tab actually costs.

**The fixture the deferred items were waiting for now exists:**
`~/.ug/big500k` — 485,175 nodes / 2,237,892 edges / 1,049 MB, built by
tripling `~/.ug/neo4j` with a 2-char shard prefix on every id, so the real
string-length and degree distributions are preserved and only the count
scales. Same dimensions as Round 2's synthetic index, so its numbers compare.

## The four questions, answered

**WebGL — yes, twice, and it is not the constraint.** `12-render-cosmos.js`
drives cosmos.gl: force simulation *and* drawing in WebGL2 shaders, one
instanced draw call for all points, and the data handed to it is already a
struct-of-arrays of `Float32Array` columns (`positions`, `colors`, `sizes`,
`shapes`, `imageIdx`, `imageSizes`, `links`, `linkColors`, `linkWidths`).
`11-render-three.js` is the opposite by design — `makeNodeObject` builds a
`THREE.Group` of 2–4 `Sprite`s per node with a **per-node `SpriteMaterial`**,
so nothing instances and nothing shares. That is why `THREE_D_MAX_ELEMENTS`
is 3,000, and it is the right trade for the graphs it draws.

**wasm — none, and the audit found no candidate.** Every hot loop is either a
typed-array pass that is already at memory bandwidth, or string/`Map` work
whose data would have to be copied across the wasm boundary to be operated on.
The one shape that would suit it — building the 485k-entry `id → index` hash
table — is already in a Worker and already costs 7 ms. Filed under
[Rejected / deferred](#rejected--deferred) so the next audit does not
re-propose it.

**Worker threads — one, and it is the right one.** `NODE_INDEX_WORKER_SRC`
fetches the binary node frame and builds the hash table off-thread, then hands
both buffers over as transferables (P9.1). Layout does not need a worker: it
runs on the GPU in cosmos and is bounded to 3,000 elements in three. The
remaining main-thread work is DOM and JSON — and a worker cannot help with
JSON, because the parsed result would have to be structured-cloned back.
That is an argument for making the data binary, not for adding a thread; see
P12.2.

**GPU-friendly data — yes, and it never gets to matter.** Past 200,000
elements `soloRequired()` hands the renderer a *neighbourhood* of at most
`SOLO_MAX_NODES` = 1,500. Even at the top of the full-draw band the entire
GPU-side payload is ~40 B per point and ~28 B per link — 7 MB at 40k nodes and
200k links. **VRAM has never been the limit and is not on the path to 500k.**

## What the tab actually costs

`ug serve --graph-mode server`, boot to settled, **nothing clicked**, canvas
empty. `heap` is `performance.memory.usedJSHeapSize`; `renderer` is the RSS of
the Chrome renderer process holding the page.

| fixture | renderer | heap | DOM nodes | listeners | catalog rows | edges in `state.adj` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `neo4j` 161,725 | **1,958 MB** | 118 MB | 3,057,743 | 139,578 | 139,290 | 919,575 |
| `big500k` 485,175 | **5,367 MB** | 341 MB | 9,166,074 | 418,158 | 417,870 | 2,758,725 |

**The JS heap is not the tab.** Blink's DOM lives outside it, so the number
Round 2 optimised reads 118 MB while the process holds 1,958 MB. Every
browser figure in this file before today measured the 6%.

| # | Item | Measured | |
| :--- | :--- | :--- | :--- |
| P12.1 | The catalog renders the whole repository into the DOM at boot | **1,958 → 245 MB** on neo4j; **5,367 → 314 MB** at 485k | ✅ |
| P12.2 | File mode dies at ~250k nodes, and says "Invalid string length" | hard wall, reproduced in Chrome | ⬜ |
| P12.3 | Every hover repaints the whole view | **71.2 → 27.3 ms** per hover at 161k/746k | ✅ |
| P12.4 | Server-mode session memory has no ceiling | unmeasured; no eviction anywhere | ⬜ |
| P12.5 | P9.2 — index identity, now that a 500k fixture exists | would reach 18 MB from 58 | ⬜ |
| P12.6 | Every `vis.*` config key was read before it was loaded, then never re-read | `solo_threshold` 10,000,000 had no effect at all | ✅ |
| P12.7 | A GPU readback per drawn element per frame, and a synchronous render per hover | our share of CPU while the pointer moves: **31% → 3.3%** | ⚠️ half-reverted |
| P12.8 | Hover asks the GPU what is under the cursor, and waits for the whole scene | sweep wall **−27%**, CPU **−41%**, p95 frame **−49%**; 97.6% pick agreement | ✅ |
| P12.9 | Every animation redraws 745,964 links per frame | **3–4 fps → 120 fps** on morphs and camera flights | ✅ |
| P12.10 | The page never goes idle — on either screen | idle CPU **50.4% → 3%** (landing), **58.8% → 7%** (graph) | ✅ |
| P12.11 | Every hover re-uploads *and* re-allocates all 14.5 MB of colour | heap swing per 60 moves **909 → 34 MB**; sweep CPU **10×** | ✅ |
| P12.12 | A selected node keeps the page working forever | parked with a selection **28.8% → 3.6%** of a core; click **539 → 291 ms** | ✅ |
| P12.13 | A whole-graph repaint uploaded as though everything changed | repeat selection CPU **−35%**, heap growth **21 MB → 0** | ✅ |

<a id="p121"></a>
### ✅ P12.1 — the catalog renders the whole repository into the DOM at boot

`renderCatalog`'s `renderNode` recurses into `visibleKids` **unconditionally**
— it never consults `isExpanded`. The entire Contains hierarchy is serialised
into `body.innerHTML` on every render and shown or hidden with CSS
(`.cat-node.expanded + .cat-children`), because `toggleCatalogRow` only adds
and removes a class and never re-renders. So the whole tree *has* to be in the
DOM for expansion to work at all.

That is 417,870 rows and 9.17M DOM elements at 485k nodes, on a panel that is
not open, before anything is clicked.

It costs a second time. `buildKids` records every id whose edges have not
arrived, and `flushCatalogWarm` fetches them a level at a time —
`CATALOG_WARM_ROUNDS` = 12. Because the render walks every node, the warm
walks every node too: 12 escalating requests ending in an 851 KB body of ids,
which pull **2,758,725 edge objects** and the same number of `adjKeys`
strings into `state.adj`. Server mode exists precisely so the client does not
hold edges. It holds most of them before the user has done anything.

**Landed as three changes**, all in `15-tools-catalog.js`:

- `renderNode` descends only into a row that is actually expanded. Under a
  filter `isExpanded` is already unconditional, so a filtered view still
  renders its whole matched subtree and keeps collapsing by CSS alone.
- `toggleCatalogRow` re-renders, because the subtree is no longer sitting in
  the DOM behind a CSS rule. It restores `#catalog-body`'s `scrollTop` across
  the render — that is the scroll container, and replacing its children would
  otherwise throw the row you just clicked off screen. Skipped while a filter
  is running, where re-rendering would re-expand the row just clicked shut.
- The footer chips stop being a tally of rendered rows. See below.

| | neo4j 161,725 | | big500k 485,175 | |
| :--- | ---: | ---: | ---: | ---: |
| | before | after | before | after |
| renderer RSS | 1,958 MB | **245 MB** | 5,367 MB | **314 MB** |
| JS heap | 118 MB | **26 MB** | 341 MB | **62 MB** |
| DOM nodes | 3,057,743 | **5,206** | 9,166,074 | **8,441** |
| event listeners | 139,578 | **365** | 418,158 | **519** |
| catalog rows | 139,290 | **77** | 417,870 | **231** |
| edges in `state.adj` | 919,575 | **382** | 2,758,725 | **1,146** |

**8.0× and 17.1×**, and the rows that remain are exactly the two levels
`maybeAutoExpand` opens.

**The chips had to change with it.** They were `counters.folders/files/symbols`
incremented once per rendered row, which was only ever right because every row
was rendered; collapsing a folder would now make the repository look smaller.
They are the whole graph's type census when unfiltered (`state.nodeTypeCounts`,
carried by both modes, and free of edges) and the kept set when filtered. Both
count **distinct nodes**, where the old tally counted a node once per Contains
parent — 4,592 symbols reported for 4,224 on `~/.ug/ug`. Rendered with
`toLocaleString()` rather than `formatNumber`, because "449k Symbols" throws
away the fact the chip exists to state.

**Verified through the DOM, not by reading the diff.** Every row below is a
real event dispatched at the page:

| | `~/.ug/ug` (local) | `~/.ug/neo4j` (server) |
| :--- | :--- | :--- |
| rows at boot | 21 | 77 |
| expand a collapsed folder | 21 → 31, row marked expanded | 77 → 79 |
| …its children resolve past the cold miss | 10 of 10 get toggles | 2 of 2 |
| collapse it again | back to 21 | back to 77 |
| `scrollTop` across a toggle | 40 → 40 | 40 → 40 |
| filter, then clear it | 838 rows → 31 | 42,371 → 79 |
| chips follow the filter | 20/59/691, then 25/192/4,224 | 1,057/1,841/24,595, then 3,128/8,910/149,687 |
| Collapse all / Expand all | 1 row / **4,809** | 1 / 21,558 |
| console errors | none | none |

The 4,809 that "Expand all" reaches on `~/.ug/ug` is exactly what the page
used to render at boot — the whole tree is still reachable, it is just no
longer the default. On neo4j "Expand all" is bounded by `CATALOG_EXPAND_MAX`
(5,000 expanded nodes) rather than by the repo, so it lands at 21,558 rows
instead of 139,290.

**A cold expand costs no extra round trip.** `buildKids` runs for every
*rendered* row, including collapsed ones, so the tree is always warmed one
level ahead of what is open — 77 rows pull 382 edges on neo4j. A child whose
own edges have not landed renders as a leaf, `flushCatalogWarm` fetches it, and
the re-render gives it its toggle.

**The lesson is the measurement, not the bug.** A JS-heap-only reading
declared this tab healthy at 118 MB while the process held 1,958 MB. Chrome's
`Performance.getMetrics` (`Nodes`, `JSEventListeners`) and the renderer
process's RSS are the honest numbers for anything that builds DOM.

<a id="p122"></a>
### ⬜ P12.2 — file mode dies at ~250k nodes, and blames the file

`loadGraph`'s local-mode branch accumulates the response into one string
(`text += decoder.decode(…)`) and then `JSON.parse`s it. V8's maximum string
length is **536,870,888 characters**; the concatenation throws
`RangeError: Invalid string length` at 535.8 MB. `loadGraph`'s `catch` turns
that into the generic failure card. Confirmed against a real page, not
inferred:

```
Could not load the graph
Invalid string length
graph.json
```

At neo4j's density (2,141 bytes of `graph.json` per node) the wall is
**~250,700 nodes** — half way to the target. The `Response.json()` fallback
hits the same ceiling; so would a streaming parser, one step later, because
the parsed result is the real cost:

| file mode, retained after load | nodes / edges | held |
| :--- | :--- | ---: |
| `~/.ug/neo4j` | 161,725 / 745,964 | **319 MB** (1,151 MB peak during parse) |
| `~/.ug/big500k` | 485,175 / 2,237,892 | **837 MB** |

Server mode is unaffected — `graph.server_mode_bytes` sends anything large
down the columnar path, and 485k costs 62 MB of heap there. But three routes
reach file mode regardless of size: `--graph-mode local`, `?gm=local`, and —
the one that matters — **a page with no server at all**, where the
capabilities probe 404s and `mode` falls back to `'local'` unconditionally.

Two fixes, and they are not alternatives:

1. **Say what happened.** A graph past the ceiling should name the wall and
   point at `ug serve`, not report "Invalid string length" against
   `graph.json` as though the file were corrupt. Small, and correct whatever
   else is decided.
2. **Give the static artifact the columnar path.** `nodes.bin` already exists;
   the missing half is edges. A CSR blob — `Uint32Array` offsets plus endpoint
   indices — is ~20 MB for 2.2M edges against 837 MB of JS objects, and it is
   a transferable, so it decodes in the Worker that already exists. This is
   also the only route by which a worker helps the load path at all.

<a id="p123"></a>
### ✅ P12.3 — every hover repaints the whole view

`onHover` → `bumpGraphStyles()` → `R.restyle()` → `cosmosPaint()`, which walks
every node *and every link* in the view rebuilding the colour and width
buffers, then `cosmosApplyVisibility()` walks every node again. There is no
throttle and no dirty set. A hover changes at most `1 + degree` nodes and
`degree` links.

Solo mode hides this above 200,000 elements — the view is 1,500 nodes. Below
it the whole graph is the view. A JS-loop bench (node, with the style rules
*stubbed cheaper* than the real ones):

| view | per restyle |
| :--- | ---: |
| 4,432 nodes / 11,977 links (`~/.ug/ug`) | 0.35 ms |
| 11,550 / 13,038 (`~/.ug/hermes`) | 0.58 ms |
| 40,000 / 190,000 | **4.92 ms** |
| 161,725 / 200,000 | **9.01 ms** |

The real `nodeColorFor` / `linkColorFor` are long conditional chains over
selection, walk, tour and focus state, and `nodeLightingFor`,
`linkVisibleFor` and `linkParticlesFor` run alongside them, so Chrome's number
will be several times these. At 60 Hz a pointer move has 16 ms.

**Attributed before touching anything, and the guess was half wrong.**
Instrumented on the 161,725-node / 745,964-link canvas, one restyle splits into
`paint` **40 ms (59%)**, `render` **27 ms (40%)**, and everything else — the
three buffer setters, `cosmosApplyVisibility`, `cosmosApplyHighlight` — under
1 ms combined. The setters were never a cost: they only flag the buffers dirty,
and the upload happens inside `render`.

**What landed.** `restyle(scope)` takes an optional
`{ nodes: <ids>, links: <edge objects> }` naming the only things whose
appearance can have changed. `cosmosPaint` split into `cosmosPaintNode(i)` and
`cosmosPaintLink(i)`, so a scoped pass writes exactly what the full pass would.
`onHover` passes the union of the **previous and current** highlight sets — the
set leaving needs reverting as much as the set arriving. Link buffer slots are
stamped on the edge (`__ci`); a Map would cost ~50 MB at 746k edges to serve a
dozen lookups per hover.

Three things a scoped pass skips, and one it refuses:

- `setLinkWidths` — widths are structural (`Contains` or not), never style.
- `cosmosApplyVisibility` — visibility *is* appearance, and the scope is a
  promise that nothing outside it changed appearance.
- A **walk** forces the full pass whatever the caller says: its branch counts
  along the whole ordered edge list to stay in step with the overlay's
  `FX_MAX_FLOW_LINKS` budget, so one link's alpha there is not a function of
  that link alone.

The 3D backend ignores the hint — bounded to 3,000 elements, so scoping it
would buy a branch rather than time.

| per hover, median of 21 | neo4j 161,725 / 745,964 | `~/.ug/ug` 4,459 / 12,033 |
| :--- | ---: | ---: |
| before | 71.2 ms | — |
| **after** | **27.3 ms** | 1.2 ms |
| full restyle (filters, tour, theme) | 73.8 ms — unchanged by design | 1.7 ms |

**2.6×**, on the real GPU (headless Chrome with `--use-angle=metal`, an M5 Max
— not swiftshader, which doubles it).

**Correctness is an equality test, not an eyeball.** Drive N real hovers
through the scoped path, snapshot `colors` / `linkColors` / `linkWidths`, then
force one full repaint of the same state and count differing floats. **Zero**,
on both fixtures, after 1 hover and after 40.

That test had to be broken on purpose before it was worth trusting:

| injected bug | caught? |
| :--- | :--- |
| scoped pass skips links entirely | **yes** — 16 floats after 1 hover, 40 after 40 |
| scope omits the *leaving* highlight set | **only after 40** — 1,800 node + 2,256 link floats, and **0 after one hover** |
| *(the test's own first version)* built a scope but never moved the highlight sets | compared two identical no-op paints and reported 0 |

A single-hover test passes against a renderer that never un-highlights
anything, because there is nothing yet to leave behind.

**Where the remaining 27 ms is, and why it stops here.** It is
`cosmos.render()`. cosmos.gl re-uploads a whole buffer when one entry changes —
`subData` appears **zero** times in the vendored bundle, so there is no
partial-update path through its public API. At 745,964 links that is an 11.9 MB
link-colour texture plus 2.6 MB of point colours on every hover, however few
entries moved. Going below this means vendoring a partial upload into
cosmos.gl, which is a change to the library rather than to `ug`; it is in
[Rejected / deferred](#rejected--deferred) with this number rather than
attempted here.

<a id="p124"></a>
### ⬜ P12.4 — server-mode session memory has no ceiling

Nothing in server mode evicts. `state.adj` (edge objects), `state.adjKeys` (a
`Set` of `source|target|rel` strings per node), `state.adjComplete` and
`state.adjPending` only ever grow, for the life of the tab. `NodeStore`'s
object pool *does* trim at `NODE_POOL_SOFT_CAP` = 50,000 — but `trimPool`
skips any node that has been hydrated (`node._slim === false`), so every node
whose detail panel was ever opened is kept forever with its docstring,
metrics, signature and lists.

None of this is measured yet. P12.1 currently masks it — the catalog fills
`state.adj` with most of the graph before a session has a chance to. Worth
measuring *after* P12.1 lands, by walking a scripted session of a few hundred
hub expansions and sampling the renderer RSS, so the shape of the growth is
known before anything is capped.

<a id="p125"></a>
### ⬜ P12.5 — P9.2, unblocked

Deferred in Round 2 with "revisit when a real 500k index exists to test
against". One now does. The item is unchanged — index identity would take the
485k index from 58 MB to ~18 MB and the wire from 51.7 to 9.5 MB — and so is
the objection: eight endpoints speak real qualified ids and 60+ client call
sites pass them around opaquely. It is now measurable rather than estimated,
which is the only thing that had to change.

Sequenced last deliberately. It is worth ~40 MB of heap; P12.1 is worth
1.7 GB of process.

<a id="p126"></a>
### ✅ P12.6 — every `vis.*` config key was read before it was loaded

Reported from a real session: `vis.solo_threshold` raised to 10,000,000, and a
162k-node graph still opened in solo view saying "too many to draw at once".

`state.capabilities` was assigned in exactly one place — inside
`probeCapabilities()`, which runs at the **end** of `initialize()`. But
`initialize()` decides solo mode at its top and calls `createGraph()` (which
picks the renderer) well before that. Both read `vis.*` off
`state.capabilities`, so both ran against `undefined` and fell back to the
hardcoded constants: `SOLO_THRESHOLD` 200,000 and `THREE_D_MAX_ELEMENTS` 3,000.
`max(161725, 745964) > 200000`, so solo engaged. `applySoloMode` early-returns
when the answer has not changed, so the real value arriving a moment later
changed nothing for the rest of the session. `vis.renderer` was equally dead;
it only *looked* to work because `auto` and the size-based default agree.

**The tell was a state that disagreed with its own inputs.** Evaluated in the
page after load, `soloRequired()` returned `false` while `state.soloOnly` was
`true` — the predicate was never wrong, the moment it ran was.

Two changes. `getCapabilities()` — the single fetch-and-cache point — now
assigns `state.capabilities` itself, so there is one place that can be late
rather than two. And `loadGraph` awaits it unconditionally, including on the
`?file=<non-default>` path that had already settled the mode without it: one
cached request is cheaper than an ordering rule nobody can see.

| `~/.ug/neo4j`, 161,725 / 745,964 | before | after |
| :--- | :--- | :--- |
| `vis.soloThreshold` = 10,000,000 | solo view, `state.view` empty | **whole graph**, `state.view` 161,725 / 745,964 |
| default config, forced local | solo view | solo view — the 200,000 fallback still engages |
| default config, auto | server mode → solo | unchanged |

**What full draw actually costs at 161k**, now that it is reachable: renderer
**958 MB**, heap 356 MB, load 2.0 s — and a restyle median of **62 ms**, which
is four dropped frames on every pointer move. That is [P12.3](#p123) arriving
in practice rather than in theory: `cosmosPaint` walks all 161,725 nodes and
745,964 links on every hover. The 62 ms was measured under software WebGL
(swiftshader), so treat it as an upper bound on the GPU half; the JS half alone
benched at ~9 ms with *cheaper-than-real* style rules. Either way it is over a
frame, and raising the threshold is what makes it the dominant interaction cost.

<a id="p127"></a>
### ⚠️ P12.7 — a GPU readback per element per frame, and a render per hover

> **Half of this was wrong, and shipped a silent visual regression.** The
> readback fix is sound and stands. The second half — skipping `render()` on a
> scoped restyle — stopped hover recolouring reaching the GPU at all. Kept here
> as written, with the correction below, because *why* it was wrong is the
> useful part. See [P12.8](#p128).

P12.3 made the hover repaint cheap and the canvas still felt slow, so this time
the measurement was a **CPU profile of real pointer input** (`Input.dispatchMouseEvent`
over the canvas, `Profiler` around it, aggregated by *call path* rather than by
self time). Self time alone had been pointing at the wrong things all round.

| call path, while the pointer moves | before |
| :--- | ---: |
| `getBufferSubData ← takePickResult ← renderFrame` | 33.0% |
| `readPixels ← getTrackedPositionsMap ← cosmosLivePos` | 16.3% |
| `bufferSubData ← updateColor ← update ← render ← restyle` | 12.3% |
| `_createAdjacencyLists ← update ← render ← restyle` | 2.6% |
| *(idle)* | 26.5% |

Two of those were ours.

**The overlay read the GPU once per drawn element, every frame.**
`getTrackedPointPositionsMap()` is a `readPixelsToArrayWebGL` — a pipeline
stall. `cosmosLivePos` called it *per call*, and the overlay calls that once
per halo and **twice per flow link**: up to ~1,400 stalls in one frame for a
map that cannot change within it. Now read once per frame, invalidated by
`overlayDraw`. And skipped entirely when the simulation is stopped, which is
the normal state — tracking exists to follow points the simulation is still
moving, and `n.x`/`n.y` are exact once `onSimulationEnd` has synced them.

**A scoped restyle forced a synchronous render.** ~~The call path says a hover
restyle runs *inside* cosmos.gl's own frame
(`restyle ← bumpGraphStyles ← handleNodeHover ← onPointMouseOver ← processHoverResult
← resolvePendingPick ← renderFrame`), so the next frame applies whatever the
setters flagged.~~ **Wrong.** The stack is right and the conclusion does not
follow. `setPointColors` sets `inputPointColors` and a dirty flag; the
`bufferSubData` that uploads lives in `create()`, which only `render()`
reaches. `Points.draw()` calls `updateColor()` only if the buffer does
not exist yet. The frame loop uploads nothing.

So from P12.7 until P12.8, hovering a node on the 2D renderer recoloured
**nothing** — a node with 8,680 links changed zero pixels of canvas outside the
tooltip. Reverted in P12.8, where the measurement also shows it was never worth
anything: one full redraw of this scene costs ~163 ms whether or not the
14.5 MB of colour buffers are uploaded first (160.5 ms with, 162.6 ms without).

| | before | after |
| :--- | ---: | ---: |
| *(idle)* — CPU with nothing to do | 26.5% | **59.3%** |
| everything the page itself does | ~31% | **3.3%** |
| `restyle` and everything under it | 14.9% | **~0.0%** |
| cosmos.gl's pick readback | 33.0% | 34.8% — untouched |

Buffer equality against a full repaint is still exactly zero after 1 and 40
hovers, on both fixtures — which is exactly how the regression got through.
**Buffer equality cannot see a buffer that never leaves the CPU.** The test
that catches it is a screenshot diff (`scratch/paint.mjs`): park the pointer on
a high-degree node, screenshot a patch of canvas *away from the tooltip*, and
count changed pixels. 0 before the fix, 329 after.

<a id="p128"></a>
### ✅ P12.8 — hit-testing on the CPU, so hover stops asking the GPU

cosmos.gl picks the point under the pointer on the GPU: it redraws every point
into an offscreen index buffer, then reads a 9×9 window of it back with
`readPixels` + `getBufferSubData`. That readback cannot return until the GPU
has finished the frame it is queued behind, and on a canvas drawing 745,964
links that frame is most of a fifth of a second. It is a *stall*, not work —
the CPU is idle through it — which is why it never showed up as a busy page,
only as a pointer skating ahead of its own highlight.

**What it is not.** Four hypotheses were measured and rejected before this one:
our restyle (already ~0.0% after P12.3), the colour uploads (112 → 116 ms — no
change), `curvedLinks: false` (−9%), `linkDefaultArrows: false` (−6%).

**The replacement.** A uniform grid over the point positions, built by counting
sort into three reused `Int32Array`s (CSR: `cellStart`, `items`, and a scratch
`cellOf`) — no per-cell arrays, because 43k of them would be exactly the
allocation churn the rest of the renderer exists to avoid. 208 cells per axis,
~4 points per cell at 160k, 173 KB regardless of how the points are spread.
Rebuilt lazily, invalidated wherever `n.x`/`n.y` or the hidden set change
(`cosmosSync`, `cosmosBuild`, `cosmosApplyVisibility`, both layout paths).
Query cost scales with the points *near the cursor* — the property the GPU pick
did not have.

**What it deliberately does not do is take over cosmos.gl's event plumbing.**
`Graph.onClick` decides point-vs-background by reading `store.hoveredPoint`;
the hover ring is drawn from `store.hoveredPoint.index`; the cursor follows it
too. So `findHoveredItem` is replaced *on the instance* and still hands its
result to cosmos.gl's own `processHoverResult`. Clicks, the ring, the cursor,
`onPointMouseOver`/`onPointMouseOut` and the tooltip all behave as before —
only the readback is gone. `resolvePendingPick` becomes a no-op. Both names are
checked before patching: a re-vendor that renames them leaves cosmos.gl's own
picking on, because slow hover is a much better failure than no hover.

**Two things had to be measured rather than reasoned about, and both came out
against the reasoning.**

*Tolerance.* cosmos.gl reads a **9×9** window of the picking framebuffer, so it
already forgives ±4 *device* pixels on top of whatever the point draws.
Matching that number — rather than picking a nice-looking 3 CSS px — is the
difference between 71% and 99% agreement with the GPU pick.

*Tie-breaking.* Where two discs overlap, the readback scan takes the covered
pixel nearest the cursor, which sounds like "nearest surface". It is not: the
picking pass is depth-tested. Ranking by distance to the rim scored **95.5%**;
ranking by distance to the **centre** scored **97.6%**. The code says so, and
says not to change it back without re-running the harness.

| CPU pick vs GPU pick, 245 sampled canvas pixels | |
| :--- | ---: |
| agreement | **97.6%** |
| the GPU found a node and we did not | **0** |
| we found a node and the GPU did not | **0** |
| both found one, different node (overlapping discs) | 6 of 71 |

> The rate is **zoom-dependent** — 95–98% across cameras, because how many
> discs overlap under a pixel is a property of the view, not of the algorithm.
> Pin the camera (`fitView(0, …)` and settle) before comparing two runs, or the
> number moves on its own. Verified against a control: forcing
> [P12.11](#p1211)'s partial upload off gives a bit-identical result
> (95.1%, same 5 disagreements, same single boundary pixel), which is what
> confirms colours have nothing to do with picking.

**And the patch introduced a bug of its own, which the same harness caught.**
`shouldKeepRendering()` — the predicate deciding whether the rAF loop runs
another frame — asks `hasPendingHoverWork()`: "has the pointer moved more than
2 px since hover was last *checked*". Only the real `findHoveredItem` advanced
`_lastCheckedMouse*`. Leaving that behind pinned the answer at *true* for as
long as the pointer was over the canvas, and cosmos.gl redrew all 745,964 links
every frame, forever: **8.3 → 116 ms per frame while parked on a node, with the
CPU 95% idle.** Replacing a method means inheriting its bookkeeping, not just
its return value.

Same-session A/B on the 161,725-node / 745,964-link canvas, 120 pointer moves
25 ms apart (absolute frame costs drift between sessions with GPU clock state —
the ratios hold, the milliseconds do not travel):

| | GPU picking | CPU picking |
| :--- | ---: | ---: |
| wall clock for the sweep | 30,780 ms | **22,483 ms** (−27%) |
| CPU spent | 15,266 ms | **8,979 ms** (−41%) |
| p95 frame | 533.5 ms | **274.3 ms** (−49%) |
| median frame | 8.5 ms | 8.4 ms |
| parked on a node | 8.3 ms, 7 frames over 16.7 | **8.3 ms, 0 over** |
| picking framebuffer ever allocated | yes | **never** |

`scratch/interact.mjs` checks the four paths that run through
`store.hoveredPoint` — hover + tooltip + pointer cursor, click-to-select,
pointer-off-canvas clears, background click clears the selection. All pass.

<a id="p129"></a>
### ✅ P12.9 — don't draw the links while the picture is moving

With picking off the critical path, what was left was not hover at all. The
user's own words: *"it is the animation slow, not the hover / click"*. They
were right, and the number is brutal.

| median frame during an animation, 161,725 / 745,964 | fps |
| :--- | ---: |
| layout morph (grid) | 249 ms — **4.0 fps** |
| layout morph (rings) | 299 ms — **3.3 fps** |
| camera flight (`fitView`) | 337 ms — **3.0 fps** |

One full redraw of this scene is ~163–208 ms, and **links are ~85% of it**. No
link *setting* recovers that:

| one full redraw | median |
| :--- | ---: |
| full — curved links, arrows, blending | 164–208 ms |
| `curvedLinkSegments` 19 → 5 | 170 ms |
| `linkBlending: false` | 154 ms |
| `linkDefaultArrows: false` | 183 ms |
| arrows off **and** straight links | 130 ms |
| **`renderLinks: false`** | **37 ms** |

So the rule is: **while something is moving, don't draw them.** This is the
"draw fewer links" lever, spent where it costs nothing — the picture the
animation *lands* on is byte-for-byte what it always was, because the links
come straight back when the motion stops. Nothing is permanently simplified;
no threshold changes what a settled graph looks like.

Deadline-based, not a nesting count: every animation declares how long it will
take, a later one extends the deadline, and one timer restores. Nothing has to
pair its own begin with its own end — which is what would eventually strand a
graph with no links in it, a far worse bug than the one being fixed. Wired into
every motion the renderer knows about: the opening morph, both layout paths,
the walk's prescribed positions, every camera flight (`frameAll`, `setView`,
`frameNodes`, `focusNode`, `zoomBy`, `flyToStop`, `frameRoute`), the simulation
ticks, and — via `onZoom` — the user's own pan and zoom, which is where 3 fps
was least forgivable.

| | before | after |
| :--- | ---: | ---: |
| layout morph (grid) | 249 ms — 4.0 fps | **8.3 ms — 120 fps** |
| layout morph (rings) | 299 ms — 3.3 fps | **8.4 ms — 119 fps** |
| camera flight | 337 ms — 3.0 fps | **9.0 ms — 111 fps** |

Below `MOTION_LINK_LIMIT` (60,000 links) it does nothing at all — a small graph
redraws inside a frame anyway, and links blinking out would be a flicker bought
for nothing. Verified on both sides of the line, sampling `renderLinks` on
every frame:

| | frames with links hidden | links back at the end |
| :--- | ---: | :--- |
| neo4j (745,964 links) — layout morph | 138 of 290 | yes |
| neo4j — camera flight | 83 of 224 | yes |
| neo4j — **settled, nothing happening** | **0 of 317** | yes |
| `~/.ug/ug` (12,033 links) — morph, flight, idle | **0** | yes |

It is also neutral for hover, by construction: a pointer sweep triggers no
motion at all (`motionCalls: 0` across 120 moves), so P12.8's numbers stand
unchanged.

<a id="p1210"></a>
### ✅ P12.10 — the page never went idle

Reported, not audited: *"why CPU / GPU usage keep high even if I did not do
anything but just staying at the project list landing screen? they go down
until I closed the chrome tab."* The process named was Chrome's shared
`--type=gpu-process`.

Both screens were burning half a core doing nothing, for two unrelated
reasons. Measured as **CPU-time deltas across the whole browser process tree**
over 20 s of no input (`ps -o time=`, walking the launched browser's children —
`%CPU` on macOS is a lifetime average and useless here):

| idle, 20 s | before | after |
| :--- | ---: | ---: |
| **landing screen** (KB manager) | **50.4%** of a core — gpu 34%, renderer 16% | **2.8–3.8%** |
| **graph view**, settled, nothing selected | **58.8%** of a core — renderer 36%, gpu 23% | **6.0–7.5%** |

#### The graph view: an rAF loop with no off switch

`overlayStart()`'s tick called `overlayDraw()` on every frame for the life of
the tab. On a settled canvas with nothing selected and nothing hovered that is
a full-canvas `clearRect`, a label pass and a full-viewport texture upload,
sixty-plus times a second, producing a pixel-identical frame each time.

It now draws when something can have changed. `overlayLive()` covers what
animates by itself — pulses, sweeps, a selection ring, flow particles, a walk,
a tour. `overlayInvalidate()` covers discrete changes (restyle, boundary
toggle, view swap, window resize). `overlayAnimateFor(ms)` covers timed motion,
and needs no call sites of its own: `cosmosMotion(ms)` from [P12.9](#p129) is
already the single place every camera flight, layout morph and pan announces
itself, so it forwards there.

The frame that must not be missed is the one *after* the last live frame — the
canvas still holds a selection ring that is now gone. `fxWasLive` buys exactly
that frame.

#### The landing screen: 22 infinite animations, and a spinner nobody can see

`document.getAnimations()` reported **22 running**: 18 × `kb-twinkle` on the
SVG constellation, the terminal cursor, the status dot, a stale-card box-shadow
pulse, and the `#loading` spinner — which was turning **behind** the opaque
manager, because on this screen `loadGraph()` never runs and `graphReveal()`
never fires, so the overlay sat there saying "Loading graph…" for the life of
the page.

The bisect is the interesting part, because it says the intuition is wrong:

| | running | CPU |
| :--- | ---: | ---: |
| baseline | 22 | 50.4% |
| minus the 18 twinkling circles | 4 | 44.6% |
| minus one stale-card pulse | 21 | 34.3% |
| **all animations off** | 0 | **1.0%** |
| only the status dot — a single 6 px circle | 1 | **17.3%** |
| only the terminal cursor (`step-end`) | 1 | **4.5%** |

**The cost is per *frame*, not per animation.** One smooth infinite animation
anywhere keeps Chrome compositing at the display's refresh rate forever, and
each of those frames re-rasterises the full-viewport stack. It scales with
window area (700×500: 3.8%, 1600×1000: 16.1%) and `will-change` does not touch
it (17.2% vs 16.1%) — it is not a layer-promotion problem. A `step-end`
animation is ~4× cheaper because it only produces frames at its steps.

Three fixes, and the user found the third:

1. The ambient animations are now struck once and still — varied resting
   opacities on the constellation, a lit status dot, a resting halo on the
   stale card. The blinking cursor stays: it is `step-end`, it is the one
   animation that means something, and it is the cheapest.
2. `#loading` is hidden when the manager opens, and `loadGraph()` puts it back.
3. **`body.kb-open #container { visibility: hidden }`.** *"why the id=container
   div has to be shown when I was on landing screen?"* — it does not. It is
   100vw × 100vh of four stacked radial gradients with
   `background-attachment: fixed`, holding the canvas, the FX overlay and a
   `backdrop-filter: blur(24px)` sidebar, all fully painted underneath an
   opaque overlay. `visibility`, not `display`: the manager also opens *over* a
   live graph, and collapsing a mounted WebGL canvas's box resizes it to
   nothing and back through cosmos.gl's `ResizeObserver`.

#### And it found a real bug in P12.8's pick tolerance

Re-running the pick-agreement harness after all this showed one pixel where the
GPU found a node and we did not — **8.76 px from a disc of radius 2.44 px**,
far outside the ±4 px tolerance P12.8 had reasoned its way to. Two things were
wrong:

* cosmos.gl renders its picking framebuffer at **half resolution** (`Qu = 0.5`
  in the bundle; confirmed at runtime as `pickingFbo.width / screenSize[0]`
  = 0.5). Its 9×9 window is therefore ±4 *buffer* pixels = **8 / pixelRatio CSS
  pixels**, twice what P12.8 assumed.
* and the slop was capped at `hitMaxR` — meant to bound the cell scan at a wide
  zoom, but what it actually did was clamp the corrected tolerance straight
  back down. Fixing the derivation alone changed *nothing*, which is what
  exposed the cap.

Agreement went 97.1% (1 miss) → **97.6% with zero misses in either direction**;
the six remaining are overlapping discs where both answers are a node under the
cursor.

#### Verified in pixels

A canvas allowed to sit still fails silently — a missed invalidation looks
exactly like a correct idle frame from the inside. Eight screenshot-diff
assertions (`scratch/overlaycheck.mjs`), all passing:

| | changed pixels | want |
| :--- | ---: | :--- |
| idle canvas over 1.2 s | 0 | stable |
| hover paints | 36,776 | > 0 |
| selection ring keeps animating | 3,076 | > 0 |
| settles back after the hover leaves | 0 | stable |
| …and the highlight is actually gone | 43,541 | > 0 |
| zoom repaints the overlay | 311,931 | > 0 |
| …and settles again | 0 | stable |
| window resize repaints | 331,685 | > 0 |

Plus `scratch/kbflow.mjs` for the `#container` path that could strand an
invisible graph: deep link → container painted; manager opened over the live
graph → hidden, canvas still 1600×913 (no resize storm); project re-picked →
container painted, canvas byte-identical.

<a id="p1211"></a>
### ✅ P12.11 — 14.5 MB uploaded, and 14.5 MB allocated, per hover

Handed a Chrome trace of a few seconds of ordinary hovering on the neo4j
graph. It reads as a single column:

```
Animation frame fired                              2,064.8 ms  99.2%
  renderFrame                                      1,534.7 ms  73.7%
    cosmos.findHoveredItem → processHoverResult    1,531.1 ms  73.5%
      onPointMouseOver → handleNodeHover           1,346.2 ms  64.7%
        bumpGraphStyles → restyle                  1,345.0 ms  64.6%
          render → update → create → updateColor   1,171.7 ms  56.3%
            ga → write → bufferSubData             1,168.2 ms  56.1%
```

**56% of the profile in `bufferSubData`, with the GPU idle** — `GPUTask`
totalled 27.5 ms across the whole 5.1 s trace. And the same trace's counters
show the JS heap swinging **338.6 → 871.0 MB**.

Both come from the same place. A hover changes `1 + degree` point colours and
`degree` link colours; `setPointColors` / `setLinkColors` + `render()` re-sent
**the whole buffer** — 2.6 MB of point colours and 11.9 MB of link colours at
745,964 links. And `ga()`, the helper behind `updateColor`, pays it twice: one
`bufferSubData` of the full array, then `new Float32Array(t)` to keep as the
transition's `previous`. 14.5 MB uploaded and 14.5 MB allocated, per hover.

**The arrays are used by reference.** `updatePointColor` does
`pointColors = inputPointColors`; the link path the same. No copy, no reorder —
entry `i` is bytes `[i*16, i*16+16)` of the GPU buffer, in the order we built
it. So a scoped restyle now writes only the slots it painted, straight into
`points.targetColorBuffer` / `lines.targetColorBuffer`, and skips `render()`
entirely: no `update()`, no `create()`, no `updateColor()`, no `ga()`.

Scattered indices are coalesced into runs (gap ≤ 16 entries — sending a few
unchanged colours beats another driver round trip), and the whole thing falls
back to the old full upload past 192 runs or a quarter of the buffer. A hub
node's 8,680 edges is the case that finds that, and falling back there is not a
failure: it is the upload we used to do unconditionally.

| 60 pointer moves, 25 landing on a node | full upload | partial |
| :--- | ---: | ---: |
| wall clock | 13.0 s | **6.4 s** |
| JS heap swing | **909 MB** | **34 MB** |
| peak heap | 1,210 MB | 336 MB |
| CPU over a 120-move sweep | 3,603 ms | **356 ms** |
| p95 frame | 116.6 ms | 100.0 ms |

**Verified two ways, because "the same bytes minus the ones either side" is a
claim about equivalence.** Reading the GPU buffers back after 16 landed hovers
and comparing float for float against the arrays the page believes it uploaded:
**0 differing** of 646,900 point floats and 2,983,856 link floats. And the
rendered picture: with the FX overlay hidden — its flow particles are
time-based anddiffer in phase between runs — a hover under the partial path and the
same hover under the full path are **pixel-identical, 0 of 591,300 differing,
max channel delta 0**.

> This also retires the "[partial GPU buffer uploads](#rejected--deferred)"
> entry, which had been rejected twice: once because cosmos.gl exposes no
> partial-update API (true — but luma.gl's `Buffer.write(data, byteOffset)`
> underneath it does), and once because [P12.9](#p129) measured the upload as
> free beside the 163 ms draw. That second measurement was taken on a
> **headless** browser and it does not hold on the user's machine, where the
> same upload costs 39 ms and half a gigabyte of allocation. Two rejections,
> both reasoned from a real measurement, both wrong about the machine that
> matters.

<a id="p1212"></a>
### ✅ P12.12 — selecting a node left the page working forever

A second trace, of **one** node selection. Aggregated by call path, three costs
fell out, and the largest was not the click at all.

| for one selection | |
| :--- | ---: |
| `bufferSubData ← ga ← updateColor ← create ← update ← render ← restyle ← bumpGraphStyles ← handleClick` | 532 ms |
| `texSubImage2D ← updatePointStatus ← updateStateFromConfig ← setConfigPartial ← cosmosApplyHighlight ← restyle` | 233 ms |
| `linkParticlesFor ← fxDrawFlow ← overlayDraw ← tick` | 131 ms — *and rising for as long as the node stays selected* |

#### The overlay scanned every edge, every frame, to find at most 600

`fxDrawFlow` ran `for (const e of cosmosEdges)` — **all 745,964** — calling
`linkParticlesFor` on each to find the handful carrying particles. Its
`FX_MAX_FLOW_LINKS` break only helps if the flowing edges come early in the
array; with a selection and the pointer away, it scanned the entire list every
frame and drew nothing.

`linkParticlesFor` answers from one of three sources. A walk decides by
node-pair key and a tour by route membership, so those still have to be asked —
both are transient, and both redraw for other reasons anyway. The third is a
hover or a selection, it is exactly `state.highlightLinks`, and it is the one
that persists. Iterating that instead:

| parked, a node selected, pointer off the canvas | before | after |
| :--- | ---: | ---: |
| CPU | **28.8%** of a core, indefinitely | **3.6%** |
| `linkParticlesFor` over a 6 s profile | **853 ms** (13.1%) | — |
| the same page with nothing selected | 1.4% | 1.8% |

This is [P5.1](#p51) again — "hover used to scan the whole view edge list on
every raycast hit" — in a different loop. An O(edges) pass to find an O(degree)
set is a shape worth grepping for.

#### A config setter that rewrote a 403×403 texture

`cosmosApplyHighlight` pushed `focusedPointIndex` and `outlinedPointIndices`
through `setConfigPartial` on **every** restyle, including every hover, where
neither can have changed. A fresh `outlinedPointIndices` array — even one
holding the same indices — makes `updateStateFromConfig` call
`updatePointStatus()`, which rewrites the whole point-status texture: 161,725
points as a 403×403 `rgba32float` upload, 233 ms in the trace.

Now compared by content and skipped when unchanged, with the memo reset
wherever the point set is rebuilt. `texSubImage2D` disappears from the profile
entirely.

| one click | before | after |
| :--- | ---: | ---: |
| CPU over the 1.2 s after mousedown | 539 ms | **291 ms** |
| heap | +14 MB | +9 MB |
| `texSubImage2D` | 233 ms | **gone** |

What is left in the click is a full repaint — 41 ms of `cosmosPaint` /
`linkColorFor` and a 102 ms `bufferSubData` — and that one is honest: selecting
a node re-dims every other node and link in the graph, so every colour really
does change. [P12.11](#p1211)'s partial upload deliberately does not apply.

#### Verified

`scratch/selcheck.mjs`, in pixels and in state:

| | |
| :--- | ---: |
| hover paints | 40,723 px |
| **flow particles still march** (two frames 420 ms apart) | 7,978 px |
| selection sets the focus ring | index 17,893 = the clicked node |
| selection ring keeps animating | 8,041 px |
| deselecting clears the focus ring | `undefined` |
| and the canvas returns to the unselected picture | **0 px** |

<a id="p1213"></a>
### ✅ P12.13 — a whole-graph *repaint* is not a whole-graph *change*

A third trace, again of one selection, with [P12.12](#p1212) in. The overlay
work and the texture upload are gone from it; what is left is a single column:

```
bufferSubData ← write ← ga ← updateColor ← create ← update ← render
              ← restyle ← bumpGraphStyles ← handleClick ← onPointClick
                                                     548.6 ms  (8.5%)
```

`handleClick` calls `bumpGraphStyles()` with no scope, so the restyle
re-evaluates every style rule and re-uploads all 17.5 MB. The question is how
much of that actually needed to move. Instrumented, per click:

| a click changes | point colours | link colours |
| :--- | ---: | ---: |
| **first selection** — `enterFocus` dims the whole graph | 161,665 of 161,725 | 745,775 of 745,964 |
| **moving the selection to another node** | **122** (0.1%) | **196** (0.03%) |

So the first selection genuinely is global, and every one after it uploads
17.5 MB to change about three hundred entries.

`cosmosPaint` now reports what it actually moved — it is already writing every
entry, so it costs one comparison per entry to know — and `restyle` takes the
partial upload when the list is short, the whole-buffer path when it is not.
The same code covers both cases without knowing anything about selection.

Two smaller things fell out of the same edit. The whole-buffer path now writes
the two colour buffers directly rather than through `setPointColors` /
`setLinkColors`, because cosmos.gl's `updateColor` sends the same bytes *and*
keeps `new Float32Array(everything)` as the transition's previous frame — 14.5 MB
of allocation for a picture already decided. And link widths are re-sent only
when a width moved, which is approximately never.

Same build, same machine, partial path forced off vs on:

| | before | after |
| :--- | ---: | ---: |
| first selection, CPU | 270 ms | 272 ms — *unchanged, and correctly so* |
| **second selection, CPU** | 222 ms | **148 ms** |
| **third selection, CPU** | 263 ms | **157 ms** |
| heap growth per repeat selection | **+21 MB / +19 MB** | **0 MB** |

#### Two float32 traps, one of which I shipped into the measurement

Comparing a computed `double` against a slot in a `Float32Array` is *almost
always unequal* — 1.4 is not representable, and neither is most of a colour
channel. Measured that way, a click appeared to change **100.0%** of both
buffers, which is precisely the conclusion that would have stopped this work.
The fix is to write first and compare the stored values, which is exact and
free. I then made the identical mistake a second time in the link-width check,
where it silently forced every restyle down the slow path — caught only because
the instrumentation printed `widths: true` on a click that cannot change a
width.

#### And a real regression, caught by instrumenting rather than by testing

Restructuring `restyle` around the new fast path, I **deleted the
`cosmosPaint()` call from the unscoped branch**. The page then uploaded stale
colours on every filter, theme and selection — the [§9f](#p127) failure again,
and the third time this round that a rendering change was wrong in a way no
buffer-level check could see. It was not a screenshot that caught it either: it
was a counter reading `n: 4096` while the paint's own change count read `0`,
which is only possible if the paint never ran.

#### Verified

`scratch/selverify.mjs` — pixels *and* a float-for-float GPU readback at every
step:

| | |
| :--- | ---: |
| first selection dims the graph | 119,315 px |
| GPU matches the arrays after selecting A | 0 / 0 of 3.6M floats |
| moving the selection repaints | 3,372 px |
| GPU matches the arrays after selecting B | 0 / 0 |
| deselecting returns to the undimmed picture | 2,931 px of chrome only |
| GPU matches the arrays after deselecting | 0 / 0 |

Every one of those numbers is **identical** with the partial path forced off,
which is the assertion that matters: the two paths draw the same picture.

#### What is left, and it is a product question

The **first** selection still uploads 17.5 MB, because clicking a node calls
`enterFocus` and dimming the entire graph really does change every colour.
Measured by taking the dimming out: a click then changes **1–2 point colours
and 0 links**, and costs 186 ms instead of 399 ms.

No upload cleverness reaches it. Handing the dimming to cosmos.gl's own greyout
looks like the answer and is not — see the Rejected table: its link greyout is
driven by an `rgba32float` status texture over *all* links, freshly allocated
and uploaded at 11.9 MB per change, which is the buffer it would have replaced.

So the only remaining lever is what a click *means*: whether selecting a node
should dim the rest of the graph automatically, or whether that should be
something the user asks for — the way the solo/isolate toggle beside it already
is. That is a design decision, and the numbers above are what it costs, not an
argument for making it.

### What the round taught

- **The JS heap is not the tab.** 118 MB of heap sat inside a 1,958 MB
  renderer process. Every browser number in this file before today measured
  the 6% and called it the tab. `Performance.getMetrics` → `Nodes` and
  `JSEventListeners`, plus the renderer process's RSS, are the honest ones.
- **Two features were each individually reasonable.** Server mode fetches
  edges on demand. The catalog warms cold ids in batches rather than one at a
  time. Composed, they fetch most of the graph at boot. Neither file is wrong
  on its own reading, which is why nothing caught it.
- **The cheap diagnostic was not the fix.** Setting `CATALOG_WARM_ROUNDS = 0`
  reproduced the whole win (240 MB / 307 MB) and would have shipped a broken
  catalog. It was worth doing as a *bisect* — it proved which of the two
  suspects held the memory before either was touched.
- **A wall can look like a corrupt file.** The 512 MB string ceiling surfaces
  as "Invalid string length" attributed to `graph.json`. The first instinct on
  seeing that card is to re-run `ug gen`, which cannot help.
- **Equal buffers are not an equal picture.** P12.3's equality harness was a
  good test and it certified a change that drew nothing: it compared what was
  in memory, and the bug was that memory never reached the GPU. A rendering
  change needs at least one assertion made of pixels.
- **Replacing a method means inheriting its bookkeeping.** The CPU picker
  returned the right answer and still cost 14× the frame time, because the
  method it replaced also advanced the counters that let the render loop go
  idle. Read what the original *writes*, not only what it returns.
- **The user was right about which thing was slow.** Three rounds of hover work
  were correct and none of it was the complaint. "The animation is laggy" named
  a 3 fps camera flight that no amount of hover tuning would have found,
  because the harness only ever drove the pointer. Then *"why is CPU high when
  I'm doing nothing"* named a third thing again — and *"why does #container
  have to be shown on the landing screen"* was the fix, handed over.
- **Nobody had measured the page doing nothing.** Every number in this file up
  to P12.9 was taken while something was happening: a hover, a sweep, a morph.
  The idle state was half a core on both screens and it had never been looked
  at. Benchmark the rest state too — it is the state the app spends most of its
  life in.
- **`Float32Array` comparisons are a measurement trap of their own.** A
  computed double almost never equals the float32 it was stored as, so
  "did this entry change?" answered by comparing against the input says *yes,
  always*. It reported 100% of a buffer changing on every click — an answer
  that looks decisive and would have closed the investigation. Write into the
  array first, then compare the stored values.
- **The same bug, in a different loop.** `fxDrawFlow` scanning every edge each
  frame to find the few with particles is P5.1 from Round 1, which was the same
  scan on hover. Fixing a shape once does not fix it everywhere it occurs;
  "O(everything) to find O(a few)" is worth grepping for, not just remembering.
- **Look past the transaction to the state it leaves behind.** One click cost
  539 ms — and left the page burning 29% of a core for as long as the node
  stayed selected. The steady state *after* an interaction deserves its own
  measurement, because it is unbounded and the click is not.
- **A rejection is only as good as the machine it was measured on.** Partial
  colour uploads were rejected twice on real measurements, and the second one —
  "the upload is free beside the draw", 160.5 ms vs 162.6 ms — was taken on a
  headless browser and was simply not true of the user's. On theirs the same
  upload was 56% of the profile. Headless on the real GPU is close enough for
  *ratios* between two configurations; it is not close enough to conclude that
  something costs nothing.
- **`%CPU` from `ps` is a lifetime average.** On a Chrome process that has been
  up for 22 hours it says almost nothing about now. Take CPU-*time* deltas
  (`ps -o time=`) over a fixed window, across the whole process tree — the GPU
  process is shared and does not belong to the tab you think it does.

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
| 2026-08-25 | — | `ug gen` **default** (with ingest) | 33.87 s / 6,158 MB | — | Round 4 baseline; ingest is 87% of the command |
| 2026-08-25 | — | …of which `upsert_nodes` | 10.42 s | — | 35% of ingest; inside OverGraph 0.17 |
| 2026-08-25 | — | …untimed store open + commit | 4.71 s | — | 16% of ingest with no phase name |
| 2026-08-25 | P11.1 | `refresh_sparse_stats` | 7.23 s | 0.93 s | **7.8×**; equality with the serial pass pinned in-process |
| 2026-08-25 | P11.1 | "Building node texts" phase | 8.93 s | 2.62 s | 3.4× |
| 2026-08-25 | P11.3 | `ug gen` cold, peak RSS | 6,158 MB | 5,599 MB | 518 MB; rows built per batch |
| 2026-08-25 | P11.2 | warm re-index, nodes rewritten | 161,725 | **0** | `0 unchanged` → `161725 unchanged` |
| 2026-08-25 | P11.2 | — | reported "embedded", cleared `pendingVectors` | reports 161,725 owed | bug the change opened; `vectorless_kept` closes it |
| 2026-08-25 | P11.10 | sparse-index terms, 5 runs, same binary | 84,481–84,519 | — | the index *content* is nondeterministic, not just its order |
| 2026-08-25 | **Round 4 Ph0** | `ug gen` **cold** | 33.87 s / 6,158 MB | **27.07 s / 5,743 MB** | **1.25×** wall |
| 2026-08-25 | **Round 4 Ph0** | `ug gen` **warm** | 46.01 s / 6,826 MB | **16.63 s / 4,369 MB** | **2.77× wall, 1.56× peak**; graph.json equal |
| 2026-08-26 | P11.10 | `graph.json`, 6 runs, ug repo | 6 distinct | **1** | reproducible; four extractors were missing Java's sort |
| 2026-08-26 | P11.10 | sparse-index terms, 4 runs | 9,340–9,364 | **9,331 ×4** | second cause: tied scores truncated in HashMap order |
| 2026-08-26 | P11.4 | `Writing nodes` phase | 10.54 s | 7.50 s | streamed; −703 MB peak, and *faster* |
| 2026-08-26 | P11.8 | the untimed 4.71 s | unnamed | **4.23 s commit** + 0.02 s open | every ingest phase now has a name |
| 2026-08-26 | P11.6 | warm re-index | 12.68 s / 4,346 MB | **8.05 s / 2,633 MB** | edge write skipped; commit 4.25 s → 0.03 s |
| 2026-08-26 | **Round 4** | `ug gen` **cold** | 33.87 s / 6,158 MB | **20.23 s / 4,997 MB** | **1.67× / 1.23×**; medians of 5 |
| 2026-08-26 | **Round 4** | `ug gen` **warm** | 46.01 s / 6,826 MB | **8.05 s / 2,633 MB** | **5.7× / 2.6×**; graph.json equal |
| 2026-08-26 | — | store edge count over 3 re-indexes | 5 → 10 → 15 | unchanged | **bug, not fixed**: edges append, never replace |
| 2026-08-26 | P11.7 | "Building node texts", split | 371 / 258 / 220 ms | 230 / 250 / 196 ms | related-names / prose-comments / text-build. Phase 1.51 → 1.32 s; deferred at 3.3% of the command |
| 2026-08-26 | P11.11+P11.12 | `ug gen` cold, wall | 20.55 s | **20.23 s** | −0.32 s (1.6%); medians of 5, gap > before-spread |
| 2026-08-26 | P11.11+P11.12 | `ug gen` cold, peak RSS | 5,012 MB | 4,997 MB | **−15 MB = noise** (113 MB spread). Churn does not set the peak |
| 2026-08-26 | — | *retracted* | "−470 MB", then "−245 MB" | 15 MB | mixed MB/MiB, then single samples vs a 100 MB spread |
| 2026-08-26 | P11.13 | store edge count over 3 re-indexes | 5 → 10 → 15 | **5 → 5 → 5** | `edge_uniqueness` (OverGraph default is off); STORE_FORMAT 3 → 4 |
| 2026-08-26 | P11.13 | a call deleted from a body | stale edge kept forever | removed | `prune_edges_to_graph`; both endpoints survive, so nothing tombstoned it |
| 2026-08-26 | P11.13 | `ug gen` cold, neo4j | 20.23 s / 4,997 MB | 18.99 s / 5,318 MB | edge identity is a batch lookup, not per-edge |
| 2026-08-26 | — | 6 *forced* edge rewrites, small fixture | — | 5 live edges, 72 KB, flat | growth seen at 746k scale is segment churn, not live duplicates |
| 2026-08-27 | — | fixture `~/.ug/big500k` | — | 485,175 / 2,237,892 / 1,049 MB | neo4j tripled; the 500k target finally testable |
| 2026-08-27 | — | `ug serve` server mode, neo4j, boot | 118 MB heap | — | …inside a **1,958 MB** renderer process. The heap was never the tab |
| 2026-08-27 | — | …DOM at boot, nothing clicked | 3,057,743 nodes / 139,578 listeners | — | 139,290 catalog rows, panel closed |
| 2026-08-27 | — | …edges pulled into `state.adj` at boot | 919,575 | — | in the mode whose purpose is not holding edges |
| 2026-08-27 | P12.1 | neo4j, renderer RSS | 1,958 MB | **245 MB** | **8.0×** landed; DOM 3,057,743 → 5,206, rows 139,290 → 77 |
| 2026-08-27 | P12.1 | big500k, renderer RSS | 5,367 MB | **314 MB** | **17.1×** landed; DOM 9,166,074 → 8,441, heap 341 → 62 MB |
| 2026-08-27 | P12.1 | edges in `state.adj` at boot, big500k | 2,758,725 | **1,146** | the tree is warmed one level ahead of what is open, no further |
| 2026-08-27 | P12.1 | `~/.ug/ug`, rows at boot / on Expand all | 4,809 / 4,809 | **21** / 4,809 | the whole tree is still reachable, just no longer the default |
| 2026-08-27 | P12.1 | catalog chips, `~/.ug/ug` symbols | 4,592 | **4,224** | the old tally counted a node once per Contains parent |
| 2026-08-27 | P12.1 | expand / collapse / scroll / filter | — | all verified through dispatched DOM events | `scrollTop` 40 → 40 across a toggle; no console errors |
| 2026-08-27 | P12.6 | `vis.soloThreshold` = 10,000,000, neo4j | solo view, `state.view` empty | **whole graph**, 161,725 / 745,964 | caps were assigned after the decision that reads them |
| 2026-08-27 | P12.6 | default config, forced local | solo | solo | the 200,000 fallback still engages — only the config path changed |
| 2026-08-27 | P12.3 | neo4j **full draw**, restyle per hover | — | **62 ms** median (software WebGL) | 4 dropped frames per pointer move; renderer 958 MB, heap 356 MB |
| 2026-08-27 | P12.3 | restyle attribution, 161k/746k | — | paint 40 ms (59%), render 27 ms (40%), rest <1 ms | the buffer setters only flag dirty; the upload is inside `render` |
| 2026-08-27 | P12.3 | hover restyle, neo4j, **real GPU (Metal)** | 71.2 ms | **27.3 ms** | **2.6×**; full restyle unchanged at 73.8 ms, by design |
| 2026-08-27 | P12.3 | hover restyle, `~/.ug/ug` 4,459/12,033 | — | 1.2 ms | small graphs were never the problem; no regression |
| 2026-08-27 | P12.3 | scoped vs full buffers, 1 and 40 hovers | — | **0 differing floats** ×3 buffers, both fixtures | and it catches injected bugs — see the table in P12.3 |
| 2026-08-27 | — | a one-hover equality test | passes against a renderer that never un-highlights | — | the leaving set only exists from the second hover on |
| 2026-08-27 | P12.7 | `cosmosLivePos` GPU readbacks per frame | up to ~1,400 | **0 while settled**, else 1 | once per halo and *twice* per flow link, for a map fixed within the frame |
| 2026-08-27 | P12.7 | page's own CPU share while the pointer moves | ~31% | **3.3%** | idle 26.5% → 59.3%; `restyle` and below 14.9% → ~0.0% |
| 2026-08-27 | P12.8 | frame time, pointer **parked** on a node | — | **8.3 ms**, 0 frames over 16.7 ms | a stationary hover is already perfectly smooth |
| 2026-08-27 | P12.8 | frame time, pointer **moving** | — | **115.3 ms** | unchanged at 4× fewer events, so per-move and a stall, not work |
| 2026-08-27 | P12.8 | skip both colour uploads on hover | 112 ms | 116 ms | **rejected** — the uploads are not the stall |
| 2026-08-27 | P12.8 | `curvedLinks:false` + `linkDefaultArrows:false` | 111.8 ms | 87.8 ms | 21% for a real visual downgrade; **not the answer** |
| 2026-08-27 | P12.2 | file mode, `graph.json` > 512 MB | — | `RangeError: Invalid string length` | V8 caps a string at 536,870,888; wall at ~250,700 nodes |
| 2026-08-27 | P12.2 | file mode retained, neo4j / big500k | 319 MB / **837 MB** | — | vs 62 MB of heap for the same graph in server mode |
| 2026-08-27 | P12.3 | `cosmosPaint` per hover, 40k/190k | 4.92 ms | — | JS-loop bench with the style rules stubbed *cheaper* than real |
| 2026-08-28 | P12.7 | hover repaint on the 2D renderer, **pixels** | 0 changed | **329 changed** | the scoped-restyle render skip uploaded nothing; buffer equality could not see it |
| 2026-08-28 | P12.8 | CPU hit test vs GPU pick, 245 sampled pixels | — | **97.6% agree, 0 misses either way** | the 6 disagreements are overlapping discs |
| 2026-08-28 | P12.8 | pick tolerance: 3 CSS px vs cosmos's 9×9 window (±4 device px) | 71% agreement | **99%** | the number was measurable, not a taste call |
| 2026-08-28 | P12.8 | tie-break: rim distance vs centre distance | 95.5% | **97.6%** | reasoning said rim; the picking pass is depth-tested |
| 2026-08-28 | P12.8 | frame time parked on a node, after replacing `findHoveredItem` | 8.3 ms | 116 ms → **8.3 ms** | `_lastCheckedMouse*` gates `shouldKeepRendering()`; inherit the bookkeeping |
| 2026-08-28 | P12.8 | pointer sweep, 120 moves — wall / CPU / p95 | 30,780 / 15,266 / 533.5 ms | **22,483 / 8,979 / 274.3 ms** | same-session A/B; ratios travel, milliseconds do not |
| 2026-08-28 | P12.9 | one full redraw, 161,725 / 745,964 | 164–208 ms | **37 ms** with `renderLinks:false` | links are ~85% of every frame |
| 2026-08-28 | P12.9 | `curvedLinkSegments` 19→5 / `linkBlending:false` / arrows off | 164 ms | 170 / 154 / 183 ms | **all rejected** — no link setting recovers the cost |
| 2026-08-28 | P12.9 | layout morph / camera flight | 4.0 / 3.0 fps | **120 / 111 fps** | links hidden only while moving; settled picture identical |
| 2026-08-28 | P12.9 | frames with links hidden while settled | — | **0 of 317** | and 0 of everything on a 12,033-link graph |
| 2026-08-28 | P12.10 | landing screen, 20 s idle, whole browser tree | **50.4%** of a core | **2.8–3.8%** | 22 infinite CSS animations; gpu-process was 34% of it |
| 2026-08-28 | P12.10 | graph view, settled, nothing selected | **58.8%** of a core | **6.0–7.5%** | the FX overlay redrew every frame forever |
| 2026-08-28 | P12.10 | one 6px status dot, alone | — | **17.3%** of a core | the cost is per *frame*: one smooth animation pins the compositor |
| 2026-08-28 | P12.10 | the same dot, `will-change: transform,opacity` | 16.1% | 17.2% | **rejected** — not a layer-promotion problem |
| 2026-08-28 | P12.10 | a `step-end` animation vs a smooth one | 17.3% | **4.5%** | stepped animations only produce frames at their steps |
| 2026-08-28 | P12.10 | same animation, 700×500 vs 1600×1000 window | 3.8% | 16.1% | scales with area — a full-viewport raster per frame |
| 2026-08-28 | P12.10 | cosmos.gl pick tolerance, corrected for the half-res picking buffer | 97.1%, 1 miss | **97.6%, 0 misses** | `Qu = 0.5`, so the 9×9 window is ±8 CSS px, not ±4 |
| 2026-08-28 | P12.10 | overlay repaint correctness | — | **8 of 8 pixel assertions** | idle stable, hover/zoom/resize repaint, ring still animates |
| 2026-08-29 | P12.11 | user's Chrome trace, 5.1 s of hovering | `bufferSubData` **1,168 ms (56%)** | — | GPU idle throughout (`GPUTask` 27.5 ms total) |
| 2026-08-29 | P12.11 | JS heap over 60 pointer moves (25 landing) | swing **909 MB**, peak 1,210 MB | swing **34 MB**, peak 336 MB | `ga()` allocates a full copy per hover as the transition's `previous` |
| 2026-08-29 | P12.11 | wall clock, same 60 moves | 13.0 s | **6.4 s** | 2.0× |
| 2026-08-29 | P12.11 | CPU over a 120-move sweep | 3,603 ms | **356 ms** | 10× |
| 2026-08-29 | P12.11 | GPU buffers vs the arrays, after 16 hovers | — | **0 differing** of 646,900 + 2,983,856 floats | read back with `readSyncWebGL` |
| 2026-08-29 | P12.11 | rendered hover, partial vs full upload | — | **0 of 591,300 pixels differ** | with the FX overlay hidden — its flow particles differ by phase |
| 2026-08-29 | P12.12 | parked with a node selected, pointer away | **28.8%** of a core | **3.6%** | `fxDrawFlow` scanned all 745,964 edges every frame |
| 2026-08-29 | P12.12 | `linkParticlesFor` over a 6 s profile | **853 ms (13.1%)** | — | an O(edges) pass to find an O(degree) set |
| 2026-08-29 | P12.12 | one click, CPU over the following 1.2 s | 539 ms | **291 ms** | `texSubImage2D` (233 ms) gone entirely |
| 2026-08-29 | P12.12 | `setConfigPartial` on every restyle | 403×403 texture rewritten | skipped when unchanged | a fresh array of the same indices counted as a change |
| 2026-08-29 | P12.12 | selection behaviour | — | **6 of 6 pixel/state assertions** | flow still marches, ring follows, deselect returns 0 px |
| 2026-08-29 | P12.13 | what a click actually changes, moving the selection | assumed all | **122 point + 196 link colours** of 907,689 | the first selection really is global — `enterFocus` dims everything |
| 2026-08-29 | P12.13 | repeat selection, CPU / heap | 222–263 ms, **+19–21 MB** | **148–157 ms, 0 MB** | partial upload; widths only when a width moved |
| 2026-08-29 | P12.13 | comparing a double against a `Float32Array` slot | reported **100.0%** of the buffer changed | write first, compare stored | made the same mistake twice, once in the fix itself |
| 2026-08-29 | P12.13 | selection correctness | — | **6 of 6, identical with the fast path off** | pixels *and* a float-for-float GPU readback at each step |

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
| `build_texts` parallelism (P11.7) | Deferred 2026-08-26, on arithmetic. Instrumented, `build_texts` is 677 ms of a 20.4 s command (3.3%) and 250 ms of that is blocked by `seen_banner`; parallelising the rest is worth ~350 ms at the limit (1.7%). P11.11 landed *inside* this phase and shrank it further, so the case is weaker now than at deferral. The design objection stands too — per-file banner sets change *attribution*, and extract-then-dedupe does not decompose because `extract_prose_comments` interleaves the dedup with a character budget and an early `break`. Measuring the split first found [P11.11](#p1111), which was the larger piece all along. |
| Moving focus dimming into cosmos.gl's own greyout | Investigated 2026-08-29, after [P12.13](#p1213) left the *first* selection as the last global upload. The idea: stop expressing focus dimming as alpha in our colour buffers and hand cosmos.gl `highlightedPointIndices` / `highlightedLinkIndices` instead, which are lists proportional to the focus set rather than to the graph. Dead on the link side: `Lines.updateLinkStatus` builds `new Float32Array(ceil(sqrt(linksNumber))² × 4)` and uploads it as an `rgba32float` texture on every change — **11.9 MB at 745,964 links**, allocated fresh each time. That is the same cost as the 11.9 MB colour buffer it would replace, plus a second dimming mechanism to maintain alongside the alpha one that the tour's four-tier gradient and the walk's four states still need. (The earlier objection recorded in `cosmosApplyHighlight` — that it double-dims — is real but secondary; the arithmetic is what kills it.) |
| ~~Partial GPU buffer uploads in cosmos.gl~~ | **Un-rejected and landed as [P12.11](#p1211) on 2026-08-29.** Rejected twice, wrongly. First on 2026-08-27, because cosmos.gl exposes no partial-update API — true of cosmos.gl, but luma.gl's `Buffer.write(data, byteOffset)` underneath it takes a byte offset. Then again on 2026-08-28, because [P12.9](#p129) measured the upload as free beside the 163 ms draw (160.5 ms with it, 162.6 ms without) — a real measurement, taken on a **headless** browser, that does not hold on the machine that matters: a user's trace put the same upload at 56% of the profile and 909 MB of allocation per sixty hovers. Kept here as a reminder that a rejection is only as good as the machine it was measured on. |
| Level-of-detail links on a *settled* graph | Considered 2026-08-28 as the answer to slow animation, and unnecessary once measured. Motion-only LoD ([P12.9](#p129)) recovers 3 fps → 120 fps while leaving the still picture exactly as it was, so there is no case for permanently dropping, capping or fading links — and every version of that changes what the graph *says* about the code. |
| WebAssembly anywhere in the vis layer | Audited 2026-08-27, no candidate found. Every hot loop is either a typed-array pass already at memory bandwidth, or string/`Map` work whose data would have to be copied across the wasm boundary to be touched. The one shape that would suit it — the 485k-entry `id → index` hash table — is already in a Worker and already costs 7 ms. The vis layer's measured costs are DOM ([P12.1](#p121)), a JSON parse ([P12.2](#p122)) and an un-dirtied repaint ([P12.3](#p123)); wasm addresses none of the three. |
| Raising `SOLO_MAX_NODES` above 1,500 | Not a performance question. cosmos.gl will instance far more and the GPU-side payload is ~40 B per point, so the budget is not what stops it — a couple of hundred thousand points is a solid wash whichever layout runs, and every hover, filter and restyle then pays full price for pixels nobody can read. Revisit only behind a *different* picture (density/aggregate overview), not by moving the number. |
| `GraphEdge` endpoints as `u32` node indices | Would reach ~6 MB against `Arc<str>`'s ~49 MB, but needs the node table wherever an edge is built or read — 41 construction sites, 144 reads, ten test files — and a seeded or two-pass `Deserialize`, because an edge would stop meaning anything on its own. Revisit only if 49 MB on a 500k-edge graph starts to matter. |
