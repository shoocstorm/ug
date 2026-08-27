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
| **Status** | Rounds 1–4 landed (bar P11.7, deferred). Round 5 open: **P12.1 + P12.6 landed**, P12.2–P12.5 audited. Suite **912/912** |

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

**2026-08-27 · baseline `3e8f441` · 6 items, P12.1 and P12.6 landed · suite 912/912**

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
| P12.3 | Every hover repaints the whole view | ≥4.9 ms per pointer move at 40k/190k | ⬜ |
| P12.4 | Server-mode session memory has no ceiling | unmeasured; no eviction anywhere | ⬜ |
| P12.5 | P9.2 — index identity, now that a 500k fixture exists | would reach 18 MB from 58 | ⬜ |
| P12.6 | Every `vis.*` config key was read before it was loaded, then never re-read | `solo_threshold` 10,000,000 had no effect at all | ✅ |

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
### ⬜ P12.3 — every hover repaints the whole view

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

The fix is a dirty-set restyle: repaint the union of the previous and current
highlight sets when only the hover changed, and keep the full repaint for the
things that really do change every element — filters, walk, tour, focus,
theme. `cosmos.setPointColors` re-uploads the whole texture either way; that
is ~1 ms and not the part worth chasing.

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
| 2026-08-27 | P12.2 | file mode, `graph.json` > 512 MB | — | `RangeError: Invalid string length` | V8 caps a string at 536,870,888; wall at ~250,700 nodes |
| 2026-08-27 | P12.2 | file mode retained, neo4j / big500k | 319 MB / **837 MB** | — | vs 62 MB of heap for the same graph in server mode |
| 2026-08-27 | P12.3 | `cosmosPaint` per hover, 40k/190k | 4.92 ms | — | JS-loop bench with the style rules stubbed *cheaper* than real |

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
| WebAssembly anywhere in the vis layer | Audited 2026-08-27, no candidate found. Every hot loop is either a typed-array pass already at memory bandwidth, or string/`Map` work whose data would have to be copied across the wasm boundary to be touched. The one shape that would suit it — the 485k-entry `id → index` hash table — is already in a Worker and already costs 7 ms. The vis layer's measured costs are DOM ([P12.1](#p121)), a JSON parse ([P12.2](#p122)) and an un-dirtied repaint ([P12.3](#p123)); wasm addresses none of the three. |
| Raising `SOLO_MAX_NODES` above 1,500 | Not a performance question. cosmos.gl will instance far more and the GPU-side payload is ~40 B per point, so the budget is not what stops it — a couple of hundred thousand points is a solid wash whichever layout runs, and every hover, filter and restyle then pays full price for pixels nobody can read. Revisit only behind a *different* picture (density/aggregate overview), not by moving the number. |
| `GraphEdge` endpoints as `u32` node indices | Would reach ~6 MB against `Arc<str>`'s ~49 MB, but needs the node table wherever an edge is built or read — 41 construction sites, 144 reads, ten test files — and a seeded or two-pass `Deserialize`, because an edge would stop meaning anything on its own. Revisit only if 49 MB on a 500k-edge graph starts to matter. |
