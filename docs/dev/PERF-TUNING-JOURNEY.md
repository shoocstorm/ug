# Performance Tuning Journey

> A phased, checkable plan for the performance work identified in the
> 2026-08-18 audit. Each item carries its evidence (a cited path), the fix,
> how to prove it worked, and its risk. Update the **Status** column as items
> land — this file is the shared ledger, not a one-shot report.

| Field | Value |
| :--- | :--- |
| **Opened** | 2026-08-18 |
| **Baseline commit** | `cdc9a2b` |
| **Version** | 0.1.15 |
| **Scope** | Rust core (`native/src`), web server, browser client |
| **Method** | Static audit of hot paths, then measured per item (see each item and the [Results log](#results-log)) |
| **Status** | Round 1 (15 items) landed 2026-08-18; Round 2 (P6–P9) landed 2026-08-20; **Round 3 opened 2026-08-24 — audited and measured, nothing landed** |

## Status legend

| Mark | Meaning |
| :--- | :--- |
| ⬜ | Open — not started |
| 🟡 | In progress |
| ✅ | Landed and verified |
| ⏭️ | Deferred — see the item's note for why |
| ❌ | Rejected — measurement did not support the hypothesis |

---

## Scoreboard

| # | Item | Phase | Est. impact | Risk | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| P1.1 | Brandes betweenness: index-based rewrite | 1 | **Very high** — measured 235× | Low | ✅ |
| P1.2 | `find_shortest_path`: predecessor array | 1 | High — measured 8.4× | Low | ✅ |
| P1.3 | `run_k_hop_bfs`: real BFS + incident-list induction | 1 | ~~High~~ → 1.0–2.1×, + correctness | Low | ✅ |
| P2.1 | Concurrent embedding batches | 2 | **Very high** — ~8× cold ingest | Low | ✅ |
| P2.2 | Stop reading + hashing every file twice | 2 | Medium — ~2× cold index I/O | Low | ✅ |
| P2.3 | Progress meter: drop the per-file mutex + flush | 2 | Medium (scales with cores) | Very low | ✅ |
| P3.1 | Lazy graph.json compression | 3 | **High** — measured 21× startup | Low | ✅ |
| P3.2 | Stop requesting `Accept-Encoding: identity` | 3 | ~~10–20× transfer~~ → correctness fix | Low | ✅ |
| P3.3 | Cache `/api/graph/stats`; `as_str()` over `{:?}` | 3 | Medium — measured 50× | Low | ✅ |
| P4.1 | MCP `shortest_path`: stop re-parsing the graph | 4 | High — + 346 MB freed | Low | ✅ |
| P4.2 | `mmr_rerank`: hoist relevance, cache norms | 4 | Medium — O(k²nd) → O(knd) | Low | ✅ |
| P4.3 | `read_snippet`: per-call file cache | 4 | Medium | Low | ✅ |
| P4.4 | `api_search`: bound the result set | 4 | Medium (safety) | Low | ✅ |
| P5.1 | Hover: index the *view* edges | 5 | **High** — O(E) → O(degree) per hover | Low | ✅ |
| P5.2 | Gizmo: stop rewriting `innerHTML` at 30 Hz | 5 | Low–medium | Very low | ✅ |

**Sequencing rationale.** Phase 1 first: the largest single win, entirely
self-contained in one file, in a well-tested area. Phase 2 next because it owns
the dominant wall clock of a cold ingest. Phases 3–4 are server-side and can
proceed in parallel with each other. Phase 5 is the only user-perceived
latency item that does not depend on anything else, so it can be pulled
forward if the felt slowness matters more than the throughput numbers.

**Hard dependency:** P4.1 needs the `find_shortest_path_graph` entry point
introduced by P1.2 — **satisfied as of 2026-08-18**, so P4.1 is unblocked.
Everything else is independent.

---

## Measurement

There is no profiler baseline yet. Establish one before Phase 1 so each phase
can be diffed rather than argued about.

### Fixtures

Real graphs already on disk, spanning three orders of magnitude:

| Fixture | `graph.json` | Notes |
| :--- | :--- | :--- |
| `~/.ug/java-oop-demo` | 108 KB | Smoke-test size |
| `~/.ug/ug` | 4.2 MB | 3,958 nodes / 10,899 edges |
| `~/.ug/overgraph` | 16 MB | Mid-size |
| `~/.ug/neo4j` | 330 MB | 161,725 nodes / 745,964 edges — the stress case |

### What is already known

From the 2026-08-16 investigation, carried forward so we do not re-derive it:

- **The server is not the bottleneck at scale.** `ug search` 0.24 s on the
  162k-node index vs 1.29 s on the 4k one; `/api/graph/cycles` over 746k edges
  in 0.33 s. PPR and the OverGraph vector index are *not* the problem.
- **The browser heap is.** The tab parses 330 MB into 161k node objects +
  746k edge objects, then builds `nodeById`, `state.adj` (~1.49M entries) and
  `_nameIndex` on top. Solo mode bounds what is *rendered*, not what is *held*.
- **Chat wall time is entirely local-LLM tokens**, not retrieval. Tool
  execution measured 0.1 s across 4 calls.

That prior finding is what makes Phase 3 (payload) and Phase 5 (client work per
interaction) the client-side priorities, and it is why nothing in this plan
chases the storage layer.

### Harness notes

- `cargo` root is `native/`; the suite runs via `cargo nextest run`
  (thread count lives in `.config/nextest.toml` — do not pass `-j`).
- Wall-clock micro-benches live at `native/tests/storage_bench.rs` and
  `native/tests/graph_bench.rs` (added by P1.1), both `#[ignore]`d, plain
  `Instant`, no Criterion. Run:
  `cargo test --release -p ultragraph --test graph_bench -- --ignored --nocapture`.
  Use `--release`; a debug build measures rustc's bounds checks, not the
  algorithm. `UG_BENCH_ALL=1` lifts the 20k-node fixture cap.
- **The RTK hook rewrites `cargo test` and filters its stdout**, which strips
  `println!` output — a bench appears to run and report nothing. Wrap the whole
  command in `rtk proxy "..."` to get raw output.
- For the browser side, measure heap in `node --expose-gc`: run the structure
  under test, drop the parsed payload, `gc()` three times, read
  `process.memoryUsage().heapUsed`. Node's V8 is the page's engine.
- The client-side timings that matter come from replaying the *web* path
  (POST to `/api/chat` with `stream:true`, timestamp each SSE event). The CLI
  does not reproduce it closely enough to trust.

### Baseline to capture before starting

```
# graph algorithms, per fixture
ug graph centrality   # currently: does not return on ~/.ug/neo4j
ug graph cycles
# server startup (first byte after launch)
time ug serve --once   # or equivalent; note peak RSS
# cold ingest
time ug gen <repo>     # note the embedding-call count from IngestStats
```

Record the numbers in the [Results log](#results-log) as they are taken.

---

## Phase 1 — Graph algorithms

**Theme:** everything in `graph.rs` is keyed by `String` and dequeues with
`Vec::remove(0)`, in code that already has dense node indices in hand. The fix
is the same shape three times: index-based collections, `VecDeque`, and no
per-edge `String` clone.

### ✅ P1.1 — Brandes betweenness: index-based rewrite

**Landed 2026-08-18.** Measured 41×–235× faster, widening with graph size. The
162k-node fixture, which previously never returned, now scores in **3.3 s**.
Jump to [what shipped](#p11--what-shipped) for the outcome, including a
correctness finding that changes what this endpoint returns.

**Where:** `native/src/graph.rs:1599-1673`

**Evidence.** Per source node, the inner loop rebuilds four
`HashMap<String, _>` over every node in the graph:

```rust
for node in &graph.nodes {          // V iterations
    for n in &graph.nodes {         // × V iterations
        pred.insert(n.id.clone(), vec![]);   // 4 String clones + a Vec alloc
        dist.insert(n.id.clone(), -1);
        sigma.insert(n.id.clone(), 0);
        delta.insert(n.id.clone(), 0.0);
    }
```

Four compounding problems in one loop:

1. `V²` string clones and map insertions just for initialization.
2. `queue.remove(0)` (`graph.rs:1621`) — `Vec` front-removal is O(n), making
   each BFS O(V²).
3. `graph.nodes[w_idx.index()].id.clone()` on **every edge traversal** — a heap
   allocation per edge visit, purely to key a map, when `NodeIndex` is already
   in hand.
4. `let node_ids: Vec<String> = ...id.clone()` (`graph.rs:1642`) — another V
   clones per source — followed by an O(V log V) sort that Brandes does not
   need. The BFS discovery order already gives the dependency-accumulation
   order for free.

At 15k nodes that is roughly a billion string allocations. The doc comment on
`calculate_centrality_graph` concedes "seconds on a 15k-node graph", and there
is **no size guard** on either caller — `/api/graph/centrality`
(`serve.rs:3477`) or `ug graph centrality` (`cli/analysis.rs:223`). On the
162k-node fixture it does not return.

**Fix.** Replace all four maps with `Vec<T>` indexed by node index; `VecDeque`
for the frontier; push visit order onto a `Vec<usize>` stack during BFS and pop
it for accumulation instead of sorting. Same algorithm, no behavioural change.

> **Correctness bugs found in the same pass.** Two, one masking the other:
>
> 1. **Stale distance read** (`graph.rs:1628`). `w_dist` was read into a local
>    *before* `dist[w]` was assigned four lines later, and never refreshed, so
>    the `v_dist + 1 == w_dist` test always compared against the initial `-1`.
>    σ never propagated and `pred` stayed empty for every node.
> 2. **Inverted accumulation** (`graph.rs:1656-1662`). Brandes is
>    `delta[v] += σv/σw · (1 + delta[w])` over `v ∈ pred[w]`; the code wrote
>    `delta[w] += σv/σw · (1 + delta[v])`.
>
> Because (1) left `pred` empty, the accumulation loop body never executed and
> **betweenness came out identically zero for every node of every graph** — it
> has never produced a usable number. Confirmed empirically before the rewrite
> (path, hub and diamond fixtures all returned all-zero betweenness; degree
> centrality was correct throughout).

<a id="p11--what-shipped"></a>
#### What shipped

- `Csr` — forward adjacency in compressed-sparse-row form over node indices,
  replacing the per-source petgraph walk. Parallel edges are collapsed, since
  two relationships between the same pair (`A Calls B` *and* `A Uses B`) are
  one hop and would otherwise double σ.
- `BrandesScratch` — `Vec`-indexed `dist`/`sigma`/`delta`/`pred`, allocated
  once per worker and reset with `fill` (a memset) per source. `pred`'s inner
  vectors are `clear`ed so they keep capacity; after the first few sources the
  traversal is allocation-free.
- BFS discovery order is kept on a stack and popped for accumulation, removing
  the O(V log V) sort *and* the O(V) id-clone that fed it, per source.
- `VecDeque` frontier, replacing `Vec::remove(0)`.
- Sources are scored across rayon's pool, **stride-partitioned into exactly
  `current_num_threads()` tasks**. Deliberately not `par_iter().fold()`: fold
  allocates one accumulator per split chunk and rayon picks that count, so an
  ~8 MB scratch at 162k nodes would be allocated an unbounded number of times.
  Striding also spreads the expensive sources (those inside a large connected
  component) that a contiguous split would pile onto one worker. Switching
  from fold to striding was itself a further ~2× on the mid-size fixtures.
- Dead `in_degree` / `out_degree` maps removed — computed on every call,
  never read.

#### Measured

Release build, M-series laptop, `cargo test --release --test graph_bench`.
Baseline is the implementation at `cdc9a2b`, preserved verbatim in
`tests/centrality_baseline/mod.rs` so the ratio stays reproducible.

| Synthetic (deg 3) | Baseline | Current | Speedup |
| :--- | ---: | ---: | ---: |
| n=200 | 18.1 ms | 0.4 ms | 41× |
| n=400 | 69.4 ms | 0.6 ms | 112× |
| n=800 | 277.2 ms | 1.7 ms | 160× |
| n=1600 | 1166.1 ms | 5.0 ms | 235× |

The ratio widens with size because the baseline was superlinear in
allocations, so 235× is a floor, not a ceiling, for the graphs that matter.

| Fixture | V | E | Current |
| :--- | ---: | ---: | ---: |
| `java-oop-demo` | 132 | 341 | 0.5 ms |
| `MusicBot` | 749 | 2,214 | 0.9 ms |
| `ug` | 4,007 | 11,061 | 3.7 ms |
| `hermes` | 10,734 | 11,753 | 9.3 ms |
| `overgraph` | 15,143 | 43,831 | 17.8 ms |
| `neo4j` | 161,725 | 745,964 | **3,266 ms** (previously never returned) |

Caveat on the comparison: the baseline was producing zeros, so this is "time
to produce nothing" against "time to produce correct scores". Its cost was
real regardless — the per-source map rebuild and sort ran in full.

#### Verification

- `tests/centrality_test.rs` — 9 new cases, every expected value derived on
  paper: path graph, hub, equal-length path splitting (pins σ), longer-detour
  rejection (pins shortest-path-only), directedness, degree, isolated node,
  two-node normalizer guard, dangling edge endpoint. Seven of the nine failed
  against the old implementation.
- Full suite: 836 passed, 0 failed.
- Downstream audit: betweenness is consumed only by the CLI's "Top N by
  betweenness" table (`cli/analysis.rs:196`) and `/api/graph/centrality`.
  Display only — no thresholds, no branching on the values. Nothing needed
  re-baselining, which is what dropped this item's risk from Medium to Low.

**Actual risk: Low.** The pre-change output was all zeros, so no consumer could
have depended on it. `ug graph centrality`'s betweenness column changes from an
arbitrary all-zero ordering to real bridge ranking — a fix, visible to users.

#### Follow-up this opened

The 162k-node fixture takes 3.3 s. That is fine for a CLI invocation and too
slow for `/api/graph/centrality` to serve on demand, even though the result is
memoized per snapshot in a `OnceLock` (`serve.rs:3477`) — the *first* caller
still waits. Worth a separate item: either warm it in the background at
snapshot load, or return 202 + poll. Not in scope here, and it was not a
problem before only because the endpoint returned zeros instantly.

---

### ✅ P1.2 — `find_shortest_path`: predecessor array

**Landed 2026-08-18. 8.4× at n=8000 (13.4 ms → 1.6 ms) for a caller that
already holds a `GraphData`; 2.8× even when both sides pay the JSON parse.**

New `find_shortest_path_graph(&GraphData, &str, &str) -> PathResult`, with the
`String` wrapper delegating to it — the `_graph` convention `calculate_
centrality_graph` and `detect_cycles_graph` already follow. BFS with a
`prev: Vec<Option<usize>>`, `VecDeque` frontier, `visited` marked on enqueue,
and one path reconstruction at the end, replacing a walk that cloned the whole
path per edge, dequeued with `Vec::remove(0)`, and re-queued a node once per
edge pointing at it.

**This unblocks P4.1**, which needed exactly this entry point.

**Where:** `native/src/graph.rs:1499-1526`

**Evidence.**

```rust
let (node_idx, path) = queue.remove(0);        // O(n) dequeue
...
for neighbor in di_graph.neighbors(node_idx) {
    let mut new_path = path.clone();           // full Vec<String> clone
    new_path.push(graph.nodes[neighbor.index()].id.clone());
    queue.push((neighbor, new_path));
}
```

Three separate blowups: O(n) front-removal; a full path clone per edge; and
`visited` marked on *dequeue* rather than enqueue, so a node is queued many
times over. Memory grows as O(E · path_len).

**Fix.** The correct implementation already exists in this repo —
`api_path` (`serve.rs:3368`) uses a `prev: Vec<Option<usize>>` predecessor
array with a single reconstruction at the end. Port it, and expose the result
as `find_shortest_path_graph(&GraphData, ...) -> PathResult` mirroring the
existing `calculate_centrality_graph` / `detect_cycles_graph` pattern. Keep the
`String`-in/`String`-out wrapper for back-compat.

**Verify.** Existing `graph_test.rs` path cases must pass unchanged; add a
timing case on `~/.ug/neo4j`.

**Risk:** Low — pure refactor, output identical. Note the two implementations
must agree: `api_path` is directed/forward-only, and the CLI should stay so.

---

### ✅ P1.3 — `run_k_hop_bfs`: real BFS + incident-list induction

**Landed 2026-08-18 — and the "High" impact estimate below was wrong.**
Measured 1.0×–2.1× on real fixtures (`neo4j` 1.2×, `hermes` 2.1×, the small
ones ~1.0×). Recorded rather than quietly restated, since the mis-scoring is
the useful part.

**Why the estimate missed.** The plan costed the O(V+E) result scan, which is
real and is gone. But *both* implementations must first build adjacency over
the whole graph, which is also O(V+E) — so removing one O(V+E) pass out of two
or three is a constant factor, not a complexity change. On a sparse synthetic
graph the two are within noise of each other; the new one builds two CSRs
(out-targets and incident-edges) where the old built one petgraph, which eats
most of what the faster induction wins back.

**What it is actually worth: the correctness fix.** The old walk used
`Vec::pop` — LIFO — and marked `visited` when a node came *off* the queue, so
it was a depth-first walk recording whatever distance it happened to arrive
with. On `A→B→C→D` plus a direct `A→D`, it descended the long branch and
recorded D at **distance 3**, then discarded the correct distance 1 because D
was already marked. Every consumer of `distances` was being told the wrong hop
count for any node reachable by more than one route — which is most of them in
a real call graph. `hop_distance_is_the_shortest_not_the_first_found` in
`tests/traversal_test.rs` pins it.

`run_k_hop_bfs` also became `pub k_hop_bfs_graph`, completing the `_graph`
family so callers holding a `GraphData` can skip the re-parse.

**Follow-up this exposed.** A 2-hop query on `neo4j` costs ~120 ms, nearly all
of it rebuilding adjacency from scratch. `serve.rs` already caches this per
snapshot (`snap.adj`, a `OnceLock`); the lib entry points rebuild per call
because a free function has nowhere to hang a cache. Giving them one is the
remaining win here, and is a bigger one than this item delivered.

**Where:** `native/src/graph.rs:1301-1338`

**Evidence.** `queue.pop()` is LIFO with `visited` marked on pop — so this is a
DFS wearing a BFS's name, and it can assign inflated distances. It then filters
`result_nodes` and `result_edges` by scanning the **entire** node and edge
lists, making a 1-hop query O(V + E).

**Fix.** `VecDeque` + mark-on-enqueue; induce the edge set from the visited
nodes' incident lists rather than scanning all edges. `api_traverse`
(`serve.rs:3303`) already does exactly this correctly — the lib version simply
never received the same treatment. Consider having both call one shared helper
so they cannot drift again.

**Verify.** Distances on a hand-built graph where DFS and BFS disagree (a
diamond with unequal branch lengths). Timing on `~/.ug/neo4j`.

**Risk:** Low — but distances *change* where the current DFS was over-counting,
which is a correctness fix. Same re-baselining caveat as P1.1, smaller blast
radius.

---

## Phase 2 — Ingest & indexing throughput

**Theme:** the cold path does everything correctly but strictly one at a time.

### ✅ P2.1 — Concurrent embedding batches

**Landed 2026-08-18.** `embed()` now keeps `concurrency` requests in flight
(default 8, `UG_EMBED_CONCURRENCY` to override; `1` restores sequential).

Two findings changed the shape of the fix:

1. **`cli::ingest` fed `embed()` one batch at a time** (`:132`, `:390`,
   chunked by `batch_size`), so making `embed` internally concurrent would
   have done nothing at all for `ug gen` — the win would have existed only on
   the `ingest_graph` path. Both loops now chunk by a new
   `EmbedderConfig::embed_chunk()` (`batch_size × concurrency`), which keeps
   the progress meter while giving `embed` enough work to saturate. Progress
   granularity coarsens from 32 to 256 nodes; a failure now discards up to a
   256-node chunk instead of 32, which the next run backfills either way.
2. **The idiomatic spelling broke unrelated code.** Written as
   `stream::iter(..).map(|chunk| self.embed_batch(&url, chunk))`, the closure
   is higher-ranked over the chunk lifetime and the returned future fails
   `Send` inference — surfacing as 12 errors about `TourOptions` and SSE
   senders at `serve.rs:5335`, nowhere near embedding. Confirmed self-inflicted
   by stashing and re-checking. Fixed by collecting the per-batch futures into
   a `Vec` first so each has a concrete lifetime; `buffered` still polls them
   lazily.

Error semantics stay **fail-fast** (`try_collect` drops the stream on first
`Err`, cancelling in-flight requests) — the open question the plan flagged,
resolved the smaller way, since every caller already degrades by writing nodes
without vectors.

`LocalEmbedder` deliberately untouched: it runs one `spawn_blocking` over
fastembed's own rayon pool, where a second concurrent call would contend
rather than help. It re-batches internally, so the larger chunks are safe.

**Verified by `tests/embed_concurrency_test.rs`** — six cases against a real
axum server on an ephemeral port that answers *later batches first*, so
completion order is roughly the reverse of request order. This property is the
reason the test exists rather than a combinator unit test: callers bind
`vectors[k]` to `plan.to_embed[k]` positionally, so a reordering bug would
attach every node's vector to a different node and produce an index that is
confidently wrong with nothing downstream able to notice.

**Where:** `native/src/storage/embed.rs:270-305`

**Evidence.**

```rust
for chunk in texts.chunks(self.cfg.batch_size) {   // 32 per batch
    let resp = self.client.post(&url)... .send().await?;
```

One HTTP round trip at a time. A cold ingest of 162k nodes is ~5,000
serialized requests; at 200 ms RTT that is ~17 minutes of near-pure idle wait.
This is almost certainly the dominant wall-clock cost of a cold remote ingest.

Secondary: `chunk.to_vec()` (line 271) clones every text for no reason — serde
serializes `&[String]` directly.

**Fix.** `futures` is already a dependency.
`stream::iter(...).map(...).buffered(N)` preserves output order and cuts this
~N×. Start at N=8. Make it configurable alongside `batch_size` in
`EmbedderConfig` so a rate-limited endpoint can dial it down.

**Open question — resolve before implementing.** Error semantics change: today
the first failing batch short-circuits and no later batch is issued. With
`buffered`, in-flight batches complete regardless. Decide whether to keep
fail-fast (drop the stream on first `Err`) or collect partial results. The
existing caller (`rows_from_plan`, `ingest.rs:303`) already degrades gracefully
by writing rows without vectors, so fail-fast is the smaller change.

**Also check:** whether the local backend (`embed_local.rs:73`) benefits.
fastembed batches internally on its own rayon pool, so concurrency there is
likely neutral-to-harmful — **measure before touching it.**

**Verify.** Time a cold `ug gen` against the remote endpoint, before/after,
with `IngestStats.embedding_calls` confirming the same number of calls were
made. Confirm vector order is preserved by diffing the resulting store.

**Risk:** Low, contingent on the error-semantics decision above.

---

### ✅ P2.2 — Stop reading + hashing every file twice

**Landed 2026-08-18.** `process_file` split into `process_file` (reads) and
`process_file_content` (takes bytes + hash); `index_with_cache`'s three phases
collapse into one parallel pass.

**Deviated from the plan above, deliberately.** The plan proposed handing
phase 1's contents to phase 3 and flagged the memory risk — peak RSS growing
by the sum of all source bytes. Instead the single pass reads, hashes, checks
the cache and parses inside one closure, so **contents drop the moment a
file's outcome is known** and what is held at once is bounded by rayon worker
count rather than repo size. That removes the memory concern instead of
measuring it. A cache hit reports only *that* it hit; a short sequential fold
drains `prev_files`, since moving ownership out of a map is not something a
parallel pass can do.

Binary documents keep the read-then-process shape — their extractor opens the
file itself, so there is nothing to hand it.

**Where:** `native/src/indexer.rs:374` and `native/src/indexer.rs:103-104`

**Evidence.** Phase 1 calls `compute_hash` (`indexer/common.rs:450` —
`fs::read` + `blake3::hash`) on every file. Phase 3 then calls `process_file`,
which does `fs::read_to_string` + `blake3::hash(content)` **again**
(`indexer.rs:103-104`) — and that second hash is immediately discarded, because
`indexer.rs:395` overwrites it with the phase-1 value.

On a cold run every file is read twice and hashed twice.

**Fix.** Have phase 1 return file contents alongside the hash, and give
`process_file` a variant taking `&str` content plus a precomputed hash. Keep
the existing signature as a thin wrapper — `process_file` is called from
`index()` (the no-cache path) and from tests.

**Watch:** phase 1 currently holds only paths and hashes; holding every file's
contents raises peak memory to the sum of all source bytes. On a large repo
that is real. Either cap it (stream contents only for the miss set, which
requires reordering phases 1 and 2) or accept it after measuring — the miss set
on a warm run is small, and the cold run is the one that already reads
everything anyway.

**Verify.** `indexer_test.rs` unchanged; time a cold `ug gen` on a large repo
and watch peak RSS alongside wall clock.

**Risk:** Low on correctness, medium on memory — measure RSS explicitly.

---

### ✅ P2.3 — Progress meter: drop the per-file mutex + flush

**Landed 2026-08-18.** The `Mutex` held across `print!` *and* a `stdout` flush
is replaced by an `AtomicUsize` of the last-printed whole percent, claimed with
`compare_exchange` — whichever worker wins repaints, the rest return without
touching stdout. Caps the run at 100 writes however many files there are, with
no lock on the hot path. Both call sites (`index` and `index_with_cache`)
updated; the explicit 100% line after the loop still guarantees a clean finish.

**Where:** `native/src/indexer.rs:250-253` and `native/src/indexer.rs:393-399`

**Evidence.**

```rust
.par_iter().enumerate().filter_map(|(i, file_path)| {
    let node = process_file(file_path, Some(&repo_root));
    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
    { let _g = print_mu.lock().unwrap(); print_index_progress(n, total_files); }
```

The mutex is held across a `print!` **and** a `stdout().flush()` — a syscall —
once per file. Every rayon worker serializes on it. On a many-core machine
indexing a large repo this becomes a real contention point, and the terminal
cannot render 50k updates/sec regardless.

**Fix.** Only print when the integer percentage actually changes: keep an
`AtomicUsize` of the last-printed percent and `compare_exchange` before taking
the lock. Caps output at 100 writes for the whole run. Costs nothing visually.

**Verify.** Wall-clock a cold index on a large repo, before/after, on a
high-core machine (the effect is invisible on 4 cores). Eyeball that the meter
still animates smoothly.

**Risk:** Very low.

---

## Phase 3 — Server startup & payload

**Theme:** the server does expensive work eagerly that is often never used, and
the client then declines the one expensive thing it did.

### ✅ P3.1 — Lazy graph.json compression

**Landed 2026-08-18. Measured on `~/.ug/neo4j` (346 MB): time-to-serve
6.66 s → 0.32 s (21×), resident 919 MB → 827 MB.**

`EncodedAsset`'s `gzip`/`brotli` became `OnceLock<Bytes>`, built on first
request; `asset_response` initialises only the encoding the client asked for,
so the other never exists. Startup now compresses nothing at all — the 6.3 s
it used to spend was entirely gzip-9 + brotli-9, and the parse it was hiding
turns out to be only ~0.3 s.

Two knock-ons, both improvements:

- `ProjectContext::approx_bytes` now charges `retained()` — identity plus
  whatever encodings actually exist — instead of assuming both. The LRU
  budget becomes an account of real memory rather than of memory we used to
  allocate unconditionally.
- The `ug serve ready` log lost its `gzip_bytes` / `brotli_bytes` fields,
  because by then nothing has been compressed. `encode_secs` is renamed
  `startup_secs`, which is now what it measures.

**Not done: parallelising the two compressors.** It became pointless — only
one encoding is ever built, so there is no second one to run alongside it.

**Follow-up.** The first client wanting an encoding pays for it on a runtime
worker. Bounded in practice: past `GRAPH_SERVER_MODE_BYTES` (50 MB) the page
uses the slim index and never fetches `graph.json`, so the big graphs skip it
entirely. Warming it in the background at snapshot load would remove even
that; not done here because it needs a runtime handle inside `load_snapshot`.

**Where:** `native/src/serve.rs:1072`, `native/src/serve.rs:111-144`

**Evidence.** `load_snapshot` unconditionally builds an `EncodedAsset`, which
runs **both** compressors at quality 9 over the whole file, serially, before
the server answers anything:

```rust
let encoded = EncodedAsset::new(raw_json.into_bytes(), "application/json; charset=utf-8");
```

On a 330 MB `graph.json` that is minutes of startup CPU, and it leaves four
resident copies (identity + gzip + brotli + parsed `GraphData`). In **server
mode `/graph.json` is never requested at all**, so the work is entirely wasted
there.

**Fix.** Make `gzip` and `brotli` `OnceLock<Bytes>` — exactly the pattern
`GraphSnapshot` already uses for `centrality`, `cycles` and `slim` on the same
struct (`serve.rs:159-163`). Skip them outright in server mode. When they *are*
needed, compress the two variants concurrently (`spawn_blocking` ×2, or rayon
`join`).

**Note:** `EncodedAsset::new` is also used for the JS bundles and favicon,
where eager compression at startup is correct and cheap. Keep that path; only
the graph snapshot needs laziness.

**Verify.** Time from process start to first successful `/healthz` on
`~/.ug/neo4j`, and peak RSS, before/after. Confirm a `/graph.json` request in
local mode still returns the correct `Content-Encoding` for each
`Accept-Encoding`.

**Risk:** Low. First `/graph.json` request in local mode now pays the
compression cost — acceptable, and it can be warmed in the background at
startup if that regression is felt.

---

### ✅ P3.2 — Stop requesting `Accept-Encoding: identity`

**Landed 2026-08-18 — but the premise in the plan below was wrong, and the
item is worth less than it was scored.** Recorded rather than quietly
rewritten, since the estimate is the thing that was mistaken.

The plan required checking whether the header took effect before doing any
work. It does not: `Accept-Encoding` is a forbidden header name, `fetch`
drops it, and measurement against a live server confirms the response was
*already* compressed —

```
content-length:          11,823,767   (brotli)
x-uncompressed-length:  346,266,017
content-encoding:        br
```

— a 29× ratio. **So there was no 10–20× transfer win to collect; the bytes
were never going over the wire uncompressed.** What was actually broken is
the progress bar: `getReader()` yields bytes the browser has already
inflated, so the numerator counted toward 346 MB while the denominator was
the 11.8 MB `Content-Length`. It reached 100% at 3.4% of the download and sat
there for the rest.

Fixed by serving `x-uncompressed-length` (new `UNCOMPRESSED_LENGTH` constant
in `serve.rs`) and reading it in `00-preamble.js`, falling back to
`Content-Length` for a static host that does not send it. The misleading
`Accept-Encoding` header is gone.

Impact reclassified from **High (10–20× transfer)** to a **correctness fix on
the loading UI**. Risk drops to Low with the premise settled.

**Knock-on:** editing `native/src/vis/js/` invalidated the published demo,
which is fingerprinted against the vis sources —
`cli::demo::tests::the_published_demo_page_is_not_stale` caught it. Refreshed
with `ug demo --page-only -o docs/ug-website/demo` (page only, 1.1 MB;
`graph.json` untouched). Note the test's own hint prints a bare
`--page-only`, which fails without `-o` unless run from a directory holding
`ug-demo/demo.json`.

**Where:** `native/src/vis/js/00-preamble.js:336`

**Evidence.**

```js
const response = await fetch(file, { headers: { 'Accept-Encoding': 'identity' } });
```

The comment says this keeps `Content-Length` exact so the progress bar is
honest. The cost is downloading a 330 MB JSON file **uncompressed** — JSON of
this shape typically brotlis 10–20×. The server already precomputed the brotli
(P3.1) and it is thrown away.

**Verify the premise first.** `Accept-Encoding` is a forbidden header name in
`fetch`; browsers may silently drop it, in which case the body is *already*
compressed and only the progress bar is lying. **Check the actual
`Content-Encoding` on the response before doing any work here** — the fix
differs completely depending on the answer.

**Fix (if the header does take effect).** Have the server emit the
uncompressed size in a custom header (`X-Uncompressed-Length`, exposed via
CORS) alongside the compressed body, and drive the progress bar from that. The
streaming reader at `00-preamble.js:353-368` already accumulates decoded text
and needs no change — only the denominator moves.

**Verify.** Network panel: transferred bytes and time-to-interactive on
`~/.ug/neo4j` in local mode. Progress bar must still reach exactly 100%.

**Risk:** Medium — depends entirely on the premise check. If the header is
already being ignored, this item collapses into "fix the progress-bar
denominator", which is cosmetic. Mark it ❌ in that case and say so.

---

### ✅ P3.3 — Cache `/api/graph/stats`; `as_str()` over `{:?}`

**Landed 2026-08-18. `/api/graph/stats` on the 162k-node graph: 19.8 ms
first call, 0.4 ms thereafter (50×).**

- `GraphNodeType::as_str` / `GraphEdgeType::as_str` return `&'static str`,
  replacing `format!("{:?}", ..)` across `serve.rs` (stats, slim index, edge
  cache, search, filter), `storage/ingest.rs`, `storage/text.rs` and
  `cli/ingest.rs`. The containers changed key type to `&'static str` rather
  than re-allocating on insert, and the search/filter paths now
  `eq_ignore_ascii_case` against the static name instead of building a
  lowercased `String` per element per request.
- `GraphSnapshot` gained `stats: OnceLock<String>` next to `centrality`,
  `cycles` and `slim`; `api_stats` is a memoised `render_stats`.

These names are wire format — stats keys, and the `node_type` / `edge_type`
columns on every store row — so `tests/enum_names_test.rs` asserts every
variant's `as_str` is byte-identical to its `Debug` spelling, plus that
`ALL` is not stale and that serde agrees.

**Where:** `native/src/serve.rs:3031`, `3035`, `3253`, `3460`
(and the same pattern at `344`, `386`, `390`, `3160`)

**Evidence.** `/api/graph/stats` rebuilds its type histogram from scratch on
every request, allocating a `String` via `Debug` formatting for each of ~162k
nodes and ~746k edges — nearly a million allocations per call, uncached, unlike
`centrality`/`cycles`/`slim` on the same snapshot. `/api/graph/search`
(`3253`) and `/api/graph/filter` (`3460`) do the same plus a `.to_lowercase()`
per element, per request.

**Fix.** Two independent changes, either useful alone:

1. Add `fn as_str(&self) -> &'static str` to the node-type and edge-type enums
   and match on that. Removes the allocation everywhere the pattern appears —
   including `storage/ingest.rs:109,171,523,608` and `storage/text.rs:147`,
   which are on the ingest hot path.
2. Add a `stats: OnceLock<String>` to `GraphSnapshot` next to its three
   existing lazy fields.

**Verify.** `wrk`/`hey` against `/api/graph/stats` on `~/.ug/neo4j`; requests
per second before/after. Assert the JSON body is byte-identical.

**Risk:** Low. `as_str` must reproduce the exact `Debug` spelling or wire
formats change — cover it with a round-trip test over every enum variant.

---

## Phase 4 — Request-path cleanup

Smaller, independent wins. P4.1 is gated on P1.2.

### ✅ P4.1 — MCP `shortest_path`: stop re-parsing the graph

**Landed 2026-08-18, and it reached further than planned.**

`agent_tools::shortest_path` now calls `find_shortest_path_graph` on the graph
it was already handed, and its `raw: &str` parameter is gone. That made
`run_tool`'s own `raw` parameter dead, and removing *that* is where the real
win turned out to be:

- **`mcp::CachedGraph` was retaining an `Arc<String>` of the entire
  `graph.json` — a second full copy of every cached project, 346 MB of it on
  the largest index — solely so it could be handed to `run_tool` for this one
  tool to re-parse.** It now stores `raw_len: usize` for cache accounting and
  drops the text after parsing.
- `cli::chat_cmd` likewise held an `Arc<String>` for the tool closure; dropped.
- `serve.rs` was cheaper (it borrowed `encoded.identity` rather than copying),
  but `GraphSnapshot::raw_json` had no callers left and is deleted.

So the item as scoped was "stop re-parsing per tool call"; what it actually
removed was a whole retained copy of the graph per cached MCP project.

**Follow-up:** `CachedGraph::approx_bytes` still multiplies by 4 (text + ~3×
parsed). With the text gone the honest figure is nearer 3×, but
over-estimating only makes the cache more conservative and the multiplier is
shared with `ug serve`, so it is left alone deliberately rather than
overlooked.

**Where:** `native/src/agent_tools.rs:3166-3177` — **depends on P1.2**

**Evidence.**

```rust
pub fn shortest_path(graph: &GraphData, raw: &str, ...) {
    let mut result = parse(crate::find_shortest_path(raw.to_string(), ...));
    if !result.found && !strict {
        result = parse(crate::find_shortest_path(raw.to_string(), ...));  // again
    }
```

`GraphData` is already parsed and sitting in the first parameter. Each call
clones the full `graph.json` *text*, re-parses it into a second `GraphData`,
rebuilds the petgraph, serializes the answer to JSON, and the caller parses
that back. The not-found path does it all a second time. Per MCP tool
invocation.

**Fix.** Call the `find_shortest_path_graph(&GraphData, ...)` entry point that
P1.2 introduces; delete the `raw` parameter.

**Verify.** Time the `shortest_path` MCP tool against `~/.ug/neo4j`.

**Risk:** Low. `raw` has exactly two call sites to update —
`cli/analysis.rs:175` and `agent_tools.rs:4140` — both of which already hold
the `&GraphData` they pass alongside it.

---

### ✅ P4.2 — `mmr_rerank`: hoist relevance, cache norms

**Landed 2026-08-18. O(k²·n·d) → O(k·n·d), with bit-identical output.**

Three caches, no changed arithmetic: relevance against the query is
loop-invariant and was recomputed every round; `cosine` recomputed both
vectors' norms on every call, so a candidate's own norm was recomputed
`k · picked` times; and the diversity term rescanned all of `picked` per
candidate per round where a running maximum extended by the newest pick
visits the same similarities.

**Deliberately *not* pre-normalizing**, which the plan suggested. It would
save a little more but changes float association and could flip a tie, and
this feeds search ranking. Caching the norms — the same sums, accumulated in
the same order — is free of that risk. `tests/rerank_snippet_test.rs` keeps a
copy of the naive implementation and asserts identical pick order across 4
`k` values × 5 `lambda` values, so "bit-identical" is checked, not claimed.

**Where:** `native/src/storage/query.rs:205-224`

**Evidence.** `cosine(&cand.node.vector, query_vec)` is recomputed inside every
one of the k selection rounds even though it is loop-invariant. `cosine`
(`query.rs:230`) also recomputes both vector norms on each call, and the
`picked` diversity term rescans all previously picked vectors per candidate.

**Fix.** Compute relevance once per candidate up front; pre-normalize every
vector so `cosine` becomes a plain dot product; keep a running per-candidate
max-similarity updated with only the newly picked vector each round (turns the
inner rescan from O(k) into O(1)).

**Verify.** Same ranking output on a fixed candidate set (assert exact order);
time with k=20 over 200 candidates.

**Risk:** Low. Floating-point association changes slightly with
pre-normalization — compare with a tolerance, and only assert *order* exactly.

---

### ✅ P4.3 — `read_snippet`: per-call file cache

**Landed 2026-08-18.** New `SnippetCache`, threaded through all three
snippet-attaching loops in `query.rs`. A file is read at most once per
request, and an unreadable path caches its failure so it is not retried per
hit. `read_snippet` and `snippet_for` keep their signatures; the line-slicing
is shared via a new `slice_lines`.

Per call, not global — as the plan required. A long-lived cache would serve a
snippet from before the user's last edit, which is the specific failure `ug`
exists to avoid.

**Where:** `native/src/storage/query.rs:360-392`

**Evidence.** Reads and line-scans the whole file per hit. A search returning
20 hits drawn from 5 files reads those 5 files 20 times.

**Fix.** Thread a small `HashMap<PathBuf, String>` through the `search_kb` call
that fills snippets, so each file is read at most once per request. Do not make
it a global cache — staleness against a live working tree is exactly the bug
`ug` exists to avoid.

**Verify.** Snippet output identical; count `fs::read_to_string` calls via a
temporary counter, or just time a 20-hit search.

**Risk:** Low.

---

### ✅ P4.4 — `api_search`: bound the result set

**Landed 2026-08-18.** `?limit=` (default 200, hard cap 5,000). The response
gained `returned`, `truncated` and `limit`; **`count` still means matches, not
returned**, so a caller can distinguish "200 of 162,000" from "200 of 200".

Checked before changing: nothing in `native/src/vis/js/` calls this endpoint —
it is listed in `cli/api.rs` and reachable by hand, so the contract change is
safe. The type filter also stopped allocating a lowercased `String` per node
per request, comparing `eq_ignore_ascii_case` against the static enum name
instead (P3.3's `as_str`).

**Where:** `native/src/serve.rs:3238-3277`

**Evidence.** No result limit. An empty `?q=` with no `?types=` serializes the
**entire** node set into the response — 162k full node objects on the stress
fixture.

**Fix.** Add a `limit` query param with a sane default (say 200) and a hard
cap, and return the pre-truncation `count` so the client can say "showing 200
of 162,000". Check `05-search-panel.js` / `09-search.js` for what the client
expects before changing the response shape.

**Verify.** `curl '/api/graph/search'` on `~/.ug/neo4j` returns promptly and
bounded. Search panel still behaves.

**Risk:** Low, but it is an **API contract change** — note it in
`docs/API-REFERENCE.md` and check the MCP surface does not depend on the
unbounded form.

---

## Phase 5 — Frontend interaction

### ✅ P5.1 — Hover: index the *view* edges

**Landed 2026-08-18. O(edges) → O(degree) per hover.**

**The fix prescribed below — "swap the scan for `edgesOf(d.id)`" — is wrong,
and would have broken highlighting outright.** Two reasons, both silent:

1. In solo mode `setSoloView` (`16-solo-view.js:297-313`) builds **fresh edge
   objects** (`{source, target, rel}`) rather than reusing the ones in
   `state.adj`, and the renderer matches `state.highlightLinks` by object
   identity. Adjacency edges would have highlighted nothing at all — no error,
   just a hover that stopped working on the large graphs this was meant to
   speed up.
2. The tooltip's "→ N out / ← N in" is documented as the key to the two link
   colours *lit on the canvas*. Full-graph degree would make that legend
   describe edges that are not on screen.

So the index is built over `state.view.edges` — what is actually drawn — and
cached against the array it came from, keyed on identity **and** length. All
three `state.view` assignment sites (`03-insights.js:462`,
`16-solo-view.js:316`, `16-solo-view.js:369`) assign whole objects and nothing
mutates `edges` in place, so identity is a sound cache key; the length check
covers an in-place append if one is ever added.

Result: same edge objects, same order within a node, same counts — a hover
after a view change pays one pass to build the index, every hover after it is
O(degree). Previously *every* hover was a full pass, with no throttle, and was
immediately followed by a full `restyle()`.

Self-loops stay counted once, as before (the index adds them to one list).

**Where:** `native/src/vis/js/14-interaction.js:48`

**Evidence.**

```js
state.view.edges.forEach(e => {
    const sId = e.source.id || e.source;
    ...
});
```

A full edge-list scan on every raycast hit, with no throttle, immediately
followed by `bumpGraphStyles()` → a full `R.restyle()`. The client **already
maintains an adjacency index** — `state.adj`, with `edgesOf(id)` /
`knownEdgesOf(id)` at `16-solo-view.js:94` — and this path simply does not use
it.

**Fix.** Swap the scan for `edgesOf(d.id)`. O(E) per hover becomes O(degree).

**Careful:** in server mode `edgesOf` warns and repairs on a cold miss. Hover
is a high-frequency caller and must not trigger a fetch storm — use
`knownEdgesOf` on the hover path and let the existing `ensureEdges` call sites
(`14-interaction.js:330`, `491`) own the fetching, as they already do for
click.

**Verify.** Chrome performance profile of a slow pointer sweep across a dense
region on `~/.ug/neo4j`; scripting time per hover before/after.

**Risk:** Low. Behaviour differs only where `state.adj` is incomplete, which is
the server-mode case the note above covers.

---

### ✅ P5.2 — Gizmo: stop rewriting `innerHTML` at 30 Hz

**Landed 2026-08-18.** The orientation triad now repaints only when the camera
quaternion has actually changed, compared component-wise against the last
painted orientation with a 1e-4 threshold — well under one screen pixel of
movement on a 26 px triad, so nothing visible is ever skipped. A parked camera
now costs nothing where it used to rebuild and reparse the markup every other
frame, forever.

Took the cheaper of the two options in the plan (skip when unchanged) rather
than rebuilding the SVG as persistent elements: it is a handful of lines
against a restructure, and an idle canvas is the case that matters.

Verified by extracting the assembled page's inline script and running
`node --check` on it as a module — the vis sources are fragments concatenated
by `build.rs`, so no individual file is standalone-parseable and per-file
linting would give a false failure.

**Where:** `native/src/vis/js/11-render-three.js:1216-1234`

**Evidence.** `svg.innerHTML = s` runs every other frame forever inside
`startOverlayLoop`, re-parsing the gizmo markup even when the camera has not
moved.

**Fix.** Build the three lines/circles/labels once as real SVG elements and
mutate their attributes; or skip the whole block when the camera quaternion is
unchanged since last frame (cheapest change, one comparison).

**Verify.** Performance profile with the camera parked: the gizmo work should
disappear from the flame chart entirely.

**Risk:** Very low.

---

---

# Round 2 — Large-graph client memory (500k nodes)

> Opened 2026-08-19. Round 1 (P1–P5) fixed the server and the per-interaction
> work. This round is about the one thing it deliberately left alone: **what the
> browser tab holds**. Round 1's own measurement note said "the browser heap is
> [the bottleneck]" and then went and optimised everything else, because that is
> where the cheap wins were. They are spent now.
>
> Scope is the 500k-node case — roughly 3× `~/.ug/neo4j`, which is the size a
> monorepo index reaches.

| Field | Value |
| :--- | :--- |
| **Opened** | 2026-08-19 |
| **Baseline commit** | `d5ed699` |
| **Scope** | `native/src/vis/js` (client), `native/src/serve.rs` (slim index wire) |
| **Method** | V8 heap measurement (`node --expose-gc`) on a synthetic 500k index built from the real `~/.ug/neo4j` distributions |
| **Status** | Phases 0–3 landed 2026-08-20; suite 881/881 |

## The measurement this round starts from

Synthetic 500k-node slim index, built from the real distributions in
`~/.ug/neo4j` (161,725 nodes — **avg id 141 chars**, avg name 42, 8,910 distinct
files scaled to 27k, max degree 8,680), run through the actual `transformSlim`
code in V8.

**Wire** (`build_slim_index`, `serve.rs`):

| payload | raw | gzip |
| :--- | ---: | ---: |
| current slim index | 99.7 MB | 20.1 MB |
| ↳ `ids` column alone | 68.5 MB | 11.8 MB |
| ↳ `names` column alone | 21.8 MB | 6.9 MB |
| everything else | 9.5 MB | 1.3 MB |

**Heap**, on the `transformSlim` path:

| stage | heap |
| :--- | ---: |
| response text | 103 MB |
| after `JSON.parse` | 236 MB |
| after `transformSlim` + `nodeById` | **379 MB (peak)** |
| steady state, payload freed | **331 MB (retained for the session)** |

Two facts fall out, and they set the whole plan:

1. **The renderer is not the problem.** Solo mode caps drawing at
   `SOLO_MAX_NODES` (1500) already. The cost is the 500k-object index the page
   keeps forever, plus three 500k-entry string-keyed `Map`s over it
   (`slimIndexOf`, `nodeById`, `degreeOf`).
2. **90% of the payload is string identity**, and it arrives as a JSON array of
   500k separate strings — which is both the download and the parse peak.

### Why the obvious fix is not the one taken

The cheapest thing to write would be "stop sending ids; make the wire index the
node's identity". It measures beautifully — 331 MB → **18 MB**. It is also
wrong for this codebase: **60+ call sites take a node id that came from a server
response** — `stop.node_id` in the tour, `c.id` in chat citations, `u.n` in the
URL state, semantic/hybrid hits, `/api/graph/path` results — and every one of
them speaks the real qualified id. Index identity means a translation layer at
eight protocol boundaries and a resolve round trip on every deep link.

So identity stays the qualified id, and the plan attacks the *representation*
instead: keep every id, but never as 500k separate JS strings. See
[P9.2](#p92--deferred-index-identity) for what was left on the table and why.

## Scoreboard

| # | Item | Phase | Est. impact | Risk | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| P6.1 | `transformSlim`: shared empty arrays, typed degrees | 0 | Medium — 331 → 215 MB | Very low | ✅ |
| P6.2 | `refreshSuggestions`: stop scanning + sorting 500k per keystroke | 0 | **High** — a stall per keystroke | Low | ✅ |
| P6.3 | `pushEdge`: O(degree²) → O(degree) dedupe | 0 | **High** — a hub click was seconds | Low | ✅ |
| P6.4 | Stop rebuilding `new Map(nodes.map(…))` per call | 0 | Medium | Very low | ✅ |
| P7.1 | Binary columnar node index (`/api/graph/nodes.bin`) | 1 | **High** — 99.7 → 36 MB wire, no JSON parse | Medium | ✅ |
| P7.2 | Front-coded id/name blobs | 1 | High — 2.6× on the dominant column | Low | ✅ |
| P8.1 | `NodeStore`: columnar typed arrays + lazy materialisation | 2 | **Very high** — the 331 MB | Medium | ✅ |
| P8.2 | Column-backed counts, search and name index | 2 | Medium | Low | ✅ |
| P9.1 | Decode the index in a Worker, transfer the buffers | 3 | Medium — removes the last main-thread block | Low | ✅ |
| P6.5 | Short-circuit the filter predicates when nothing is filtered | 0 | **High** — 17k lookups per hub expansion | Very low | ✅ |
| P8.3 | Rank `/api/graph/search` before applying `limit` | 2 | Correctness — the box's top hit | Low | ✅ |
| P9.2 | Index identity (drop ids from the wire entirely) | — | Would reach 18 MB | High | ⏭️ |

**Sequencing.** Phase 0 is independent of everything and lands first because two
of its four items are user-visible stalls, not memory. Phase 1 (wire) and Phase 2
(client store) are one change split in two — the binary frame is useless without
a reader and the store cannot be fed by the JSON index — so they land together
and are measured together. Phase 3 sits on top of a working Phase 1+2 and cannot
be done before them.

---

## Phase 0 — Client fixes with no protocol change

**Theme:** four independent things the client does per-node or per-edge that it
does not need to do at all. None of them touches the wire, so each is
individually revertible.

### P6.1 — `transformSlim`: shared empty arrays, typed degrees

**Where:** `native/src/vis/js/02-dialogs.js` (`transformSlim`)

**Evidence.** Every node is built with six *distinct* empty arrays
(`imports`, `exports`, `extends`, `implements`, `calls`, `boundaries`) as
placeholders until hydrate fills them. At 500k nodes that is **3 million array
objects** whose only job is to be empty. Alongside it `state.degreeOf` is a
`Map<string, number>` — 500k entries keyed by the same 141-char strings the
nodes already hold.

**Fix.** One frozen module-level `EMPTY` array shared by every placeholder slot
(hydrate *replaces* the slot rather than mutating it, so sharing is safe — this
is checked, not assumed). `degreeOf` becomes a `Uint32Array` indexed by wire
position, read through the index the store already has.

**Verify.** Heap measurement before/after on the 500k fixture.

**Risk:** Very low. The one hazard is a mutation of a placeholder array in
place; `hydrateNodes` assigns, and there is no other writer.

### P6.2 — `refreshSuggestions`: stop scanning + sorting 500k per keystroke

**Where:** `native/src/vis/js/09-search.js`

**Evidence.** Per keystroke: `state.graph.nodes.filter(…)` materialises a new
array (with an empty query, that is a **500k-element array of every node**),
then `.sort()` over the full match set with `toLowerCase()` called twice per
comparison. Measured 41 ms and ~3 MB of garbage per keystroke at 500k — and
that is the *cheap* case, a query matching few nodes. Fifty results are
displayed.

**Fix.** A bounded scan: walk the nodes once, keep only what the display needs
plus a capped match set for "light up … in graph", and never materialise the
unfiltered case at all (an empty query has always rendered nothing). Precompute
the lowercased name once per candidate instead of twice per comparison.

**Verify.** Time 5 keystroke scans on the 500k fixture; garbage per keystroke.

### P6.3 — `pushEdge`: O(degree²) → O(degree) dedupe

**Where:** `native/src/vis/js/16-solo-view.js`

**Evidence.**

```js
function pushEdge(id, edge) {
    let list = state.adj.get(id);
    if (!list) { state.adj.set(id, [edge]); return; }
    for (const e of list) {                       // linear scan, per edge
        if (e.source === edge.source && e.target === edge.target && e.rel === edge.rel) return;
    }
    list.push(edge);
}
```

The real `~/.ug/neo4j` graph has a **max degree of 8,680**; at 500k scale expect
~25k. Filling one hub's adjacency list is `d²/2` comparisons of 141-char
strings — hundreds of millions, on the main thread, for one click.

**Fix.** Carry a `Set` of `source\0target\0rel` keys alongside each list.

**Verify.** Time `ensureEdges` on the highest-degree node of `~/.ug/neo4j`.

### P6.4 — Stop rebuilding `new Map(nodes.map(…))` per call

**Where:** `03-insights.js`, `08-sidebar-nav.js`, `15-tools-catalog.js`

**Evidence.** Three sites build `new Map(state.graph.nodes.map(n => [n.id, n]))`
as a fallback when `state.nodeById` is missing. Each one allocates 500k
two-element arrays plus a 500k-entry map. `initialize()` sets `state.nodeById`
before any of them can run, so the fallback is dead weight that fires only if
the invariant is already broken.

**Fix.** Read `state.nodeById` and treat its absence as the bug it is.

---

## Phase 1 — The wire

### P7.1 — Binary columnar node index

**Where:** `native/src/serve.rs`, `native/src/vis/js/00-preamble.js`

**Evidence.** `/api/graph/nodes` is JSON, and its two largest members are arrays
of 500k strings. `JSON.parse` on that is 100 MB of text in and 236 MB of heap
out — the single largest allocation the page ever makes, on the main thread,
before anything is drawn.

**Fix.** A second representation of the same index, served at
`GET /api/graph/nodes.bin`: a magic header, a section table, and then columns as
raw little-endian typed arrays with the string columns front-coded (P7.2). The
client fetches it with `arrayBuffer()` and creates typed-array views **over the
buffer it already has** — no parse, no per-node allocation, and the string bytes
stay out of the JS heap entirely (they are `external` memory).

The JSON endpoint stays exactly as it is. It is what the API reference documents
and what the tests exercise, and keeping both is what makes the binary path
revertible with one line in `00-preamble.js`.

**Layout.** `UGNIDX\0` + `u32 version` + `u32 sectionCount`, then
`sectionCount × (u32 kind, u32 offset, u32 len)`, then the payload. Every
section is 4-byte aligned so a typed-array view can be taken without copying.
The tail section is a small JSON object for everything that is not a column —
dictionaries, counts, stats, languages, token.

**Risk:** Medium — a new binary format is a new place for an off-by-one. Bounded
by a round-trip test asserting the binary index and the JSON index describe
byte-identical graphs, and by the JSON path staying available.

### P7.2 — Front-coded id/name blobs

**Evidence.** Measured on the real `~/.ug/neo4j` ids: 21.8 MB raw, **8.4 MB**
front-coded with a 16-entry restart block (2.6×). Node ids are qualified names
(`path/to/file.rs::module::Symbol`), so consecutive ids share long prefixes.
Names: 6.5 MB → 3.3 MB.

**Fix.** Per entry, one `u8` of shared-prefix length with the previous entry,
then the suffix bytes; the entry's extent comes from the offsets column, so no
length field is needed. Restart (shared = 0) every 16 entries, which bounds
`idAt(i)` to 16 steps.

Alongside it the server sends a `u32` FNV-1a hash per id, so the client can
build its lookup table with pure integer work rather than decoding 500k strings
to hash them. `tests/` pins the Rust and JS hashes against each other.

---

## Phase 2 — The client store

### P8.1 — `NodeStore`: columnar typed arrays + lazy materialisation

**Where:** `native/src/vis/js/02-dialogs.js` (new store), and every reader of
`state.graph.nodes`

**Evidence.** The 331 MB. 500k node objects at ~130 bytes each, 500k id strings
at ~160 bytes each, 500k name strings, and three 500k-entry `Map`s over them.

**Fix.** In server mode `state.graph.nodes` **stops existing** (it becomes an
empty array). In its place, a `NodeStore` holding the columns as typed-array
views over the P7.1 buffer, an open-addressed `Int32Array` hash table over the
id hashes for `id → index`, and a `Map` pool of node objects **materialised only
when something asks for one**.

The critical design constraint: `state.nodeById` is read at 60+ call sites, most
of which take an id that came from a server response. So the store *is*
`state.nodeById` — it implements `get`/`has`/`size`/`keys`/`values`/`entries`/
`forEach`, and every one of those call sites is untouched. Only the sites that
*iterate all nodes* had to change, and there are 24 of them.

Materialised nodes are never evicted while they are in the view (the renderer
mutates `x`/`y` on the object and matches by identity, so a re-materialised node
would lose its position). The pool is bounded by what the user has touched, not
by graph size.

**Risk:** Medium. `state.graph.nodes` becoming empty in server mode is a silent
degradation at any site that still reads it — a legend that counts zero rather
than an error. Enumerated exhaustively rather than trusted:
`grep -rn 'state\.graph\.nodes'`.

### P8.2 — Column-backed counts, search and name index

The 24 whole-graph readers, by what they actually wanted:

- **Counts** (`buildNodeFilterChips`, `buildLegend`, `syncLegend`,
  `presentNodeTypes`) — already on the wire as `nodeTypeCounts`, and recomputed
  from 500k nodes anyway. Plus a new `boundaryCount`.
- **Search** (`refreshSuggestions`, the palette) — scans the type/name columns
  directly, materialising only the ≤50 rows displayed.
- **`_nameIndex`** (`findNodeByName`) — built from the name column on first use,
  storing indices rather than node objects.
- **`nodeCount`** — `state.nodeCount`.

---

## Phase 3 — Off the main thread

### P9.1 — Decode the index in a Worker

**Where:** `native/src/vis/js/00-preamble.js`, new worker source

**Evidence.** With Phase 1+2 the main-thread cost of loading the index is one
`arrayBuffer()` plus the hash-table build — the latter is 500k integer
insertions, tens of milliseconds, and it is the only remaining block.

**Fix.** Fetch and decode in a Worker created from a `Blob` URL (the vis is a
single assembled HTML file — there is no second script to point a `Worker` at),
and `postMessage` the buffers back as **transferables**, so nothing is copied.
The main thread receives an `ArrayBuffer` it can take views over directly.

**Risk:** Low, with a synchronous fallback if `Worker` or `Blob` URLs are
unavailable (they are not, in any browser this ships to, but a headless harness
is a different matter).

### P9.2 — Deferred: index identity

**Status: ⏭️ deferred, deliberately.** Dropping ids from the wire entirely and
making the wire index the node's identity measures 331 MB → **18 MB** and
99.7 MB → 9.5 MB. It is the biggest remaining win by a wide margin.

It is deferred because identity is not a client-side detail here. Eight
endpoints return real qualified ids (`/api/graph/search`, `/api/graph/path`,
`/api/graph/traverse`, `/api/graph/filter`, `/api/db/node`, `/api/chat`,
`/api/tour`, `/api/search/*`), the URL deep-link format is a real id, and 60+
client call sites pass ids around opaquely. Doing it properly means an
`id ↔ index` translation at every one of those boundaries plus a resolve
endpoint for cold deep links — a change with a much larger blast radius than
everything in this round combined, for a graph size nobody has hit yet.

Revisit when a real 500k index exists to test against.


---

---

## Round 2 — what shipped, and what it measures

### The fixture

Everything below is on a **485,175-node / 2,237,892-edge** graph, built by
replicating `~/.ug/neo4j` three times with suffixed ids — 806 MB of
`graph.json`, comfortably into server mode. It keeps the real distributions
that matter: avg id 141 chars, avg name 42, **max degree 8,680**.

Rebuild it with a streaming writer (holding two copies of an 806 MB document in
a `node` heap is its own experiment), then serve it:

```js
// mkgraph.js — 3x ~/.ug/neo4j into ~/.ug/scale500k/graph.json
const fs = require('fs');
const src = JSON.parse(fs.readFileSync(process.env.HOME + '/.ug/neo4j/graph.json', 'utf8'));
const out = fs.createWriteStream(process.env.HOME + '/.ug/scale500k/graph.json');
const w = s => new Promise(r => out.write(s) ? r() : out.once('drain', r));
const sfx = r => r === 0 ? '' : '#r' + r;
(async () => {
  await w('{"nodes":[');
  let first = true;
  for (let r = 0; r < 3; r++) for (const n of src.nodes) {
    const o = { id: n.id + sfx(r), name: n.name, node_type: n.node_type };
    if (n.folder) o.folder = n.folder;
    await w((first ? '' : ',') + JSON.stringify(o)); first = false;
  }
  await w('],"edges":['); first = true;
  for (let r = 0; r < 3; r++) for (const e of src.edges) {
    await w((first ? '' : ',') + JSON.stringify(
      { source: e.source + sfx(r), target: e.target + sfx(r), edge_type: e.edge_type }));
    first = false;
  }
  await w(']' + (src.stats ? ',"stats":' + JSON.stringify(src.stats) : '') + '}');
  out.end();
})();
```

```
node --max-old-space-size=12000 mkgraph.js
ug serve -i ~/.ug/scale500k/graph.json --no-db --port 8099 --graph-mode server
```

It is not left on disk between rounds — 806 MB, and it shows up in the project
switcher as a project nobody wants.

Two harnesses, because they answer different questions:

- **V8 heap** (`node --expose-gc`), with the store source sliced straight out of
  `02-dialogs.js` and evaluated, so it cannot drift from what ships. This is
  the controlled before/after on the index alone.
- **The real page in headless Chrome**, driven through a same-origin proxy that
  appends a probe to the assembled HTML and has it POST its findings back. This
  is the whole tab: renderer, adjacency cache, DOM and all. `HEAD` and this
  branch, same machine, same probe, n=8.

> **Do not time this code inside a `vm` context.** An early attempt measured
> `fnv1a32` at 9.7 µs per id and sent this round chasing a hash that was not
> slow; the same function in the harness's own realm is **0.48 µs**. The `vm`
> boundary defeats JIT specialisation by ~20×. Heap numbers from a `vm` are
> fine; timings are not.

### What the index costs the tab

| | HEAD | Round 2 |
| :--- | ---: | ---: |
| `/api/graph/nodes` identity bytes | 98.2 MB | — |
| `/api/graph/nodes.bin` identity bytes | — | **51.7 MB** |
| …the same, gzipped on the wire | 7.1 MB | 9.7 MB |
| response text on the JS heap | 97 MB | 0 |
| after parse | 226 MB | 0 |
| **peak while building the index** | **426 MB** | **58 MB** |
| **retained for the session** | **338 MB** | **58 MB** (3 MB heap + 55 MB external) |
| main-thread time to build it | 393 ms | 7 ms |

**The gzip row is a real regression and it is the deliberate trade.** Front
coding removes exactly the redundancy gzip was feeding on, so the frame
compresses worse even though it is half the size uncompressed. `ug serve` binds
to loopback and is documented as local-only, where 2.6 MB of extra transfer is
nothing and 47 MB of resident memory is everything. On a graph reached over a
network the JSON index is still there and still smaller compressed.

### What the whole tab costs

Chrome, `--enable-precise-memory-info`, median of 8 runs [min–max]:

| | HEAD | Round 2 | |
| :--- | ---: | ---: | ---: |
| `performance.memory.usedJSHeapSize` | 280 MB [275–291] | **95 MB** [87–122] | **2.9×** |
| `totalJSHeapSize` | 328 MB [328–333] | **132 MB** [103–133] | 2.5× |
| load → interactive | 2,485 ms [2,361–3,234] | **1,531 ms** [1,518–2,123] | 1.6× |
| cold click on the degree-8,680 hub | 69 ms | **38 ms** | 1.8× |
| keyword search | 77 ms | 82 ms | — |
| `state.graph.nodes.length` | 485,175 | **0** | |

Behaviour is identical where it should be: same 301 nodes and 312 edges in the
view, same 1,809 search matches, same top hit, same top hubs, zero console
errors. Local mode (`~/.ug/ug`, 4,221 nodes) is unchanged — same top hubs, same
counts, whole graph drawn.

**Search did not get faster, and that is the point.** 77 ms of scanning
485k nodes *on the main thread* became 82 ms of waiting for a server that
answers in under 1 ms — during which the page is responsive. The old number
was a frozen tab; the new one is not.

### The cold-click bimodality, and why it is not a regression

Measured immediately after load, the first click on the biggest hub was
*slower* than HEAD — 144 ms against 66 — and stubbornly bimodal: half the runs
came in at ~57 ms, half at ~145 ms. Three things were ruled out before the
cause was found:

- **Not the server.** `curl` against `/api/graph/edges` with the same 301-index
  induced body: 0.9 ms on both builds, eight runs each.
- **Not the store.** Phase timing put the gap in the induced round trip, whose
  client code is byte-identical between the builds — and a *plain `fetch` to
  the same endpoint*, written in the probe, showed the same 6 ms → 62 ms split.
- **Not the hash or the front coding.** See the `vm` note above.

It is a **one-off post-load GC** — the 51 MB `ArrayBuffer` is external memory,
and V8 collects shortly after the load burst. Whichever `await` follows absorbs
the pause. Inserting a 2.5 s idle before the first click removes it entirely
and the ordering inverts:

| cold click, per run (ms, sorted) | |
| :--- | :--- |
| HEAD | 67, 68, 68, 69, 69, 70, 146, 166 |
| Round 2 | 33, 38, 38, 38, 38, 38, 45, 86 |

So: **one** interaction in the first couple of seconds after load may absorb a
GC pause it would not have absorbed before; every interaction after that is
roughly twice as fast. Worth knowing, not worth trading 185 MB for.

### Bugs this found

Three, none of which any Rust-side test could have caught:

1. **The front-coded decoder lost its prefix on buffer growth.** `ensure()`
   reallocated the scratch buffer without copying what was in it — which is
   precisely the accumulated shared prefix the next record builds on. Ids that
   crossed a growth boundary came back with their head replaced by NUL bytes,
   hashed to something else, and became unfindable. **Nothing threw.** Caught by
   sweeping all 485,175 ids through `indexOf` and demanding each resolve to its
   own index.
   Now pinned by `the_client_decodes_every_id_this_encoder_writes`, which runs
   the real client decoder under `node` against a frame this encoder produced —
   and whose fixture is built specifically to force a growth mid-block, because
   the first version of that test passed against the broken decoder.
2. **Local mode was broken for a while and said nothing.** `transformData` has
   no `nodes` binding — its nodes live in a `nodeMap` — so the new counting
   pass threw a `ReferenceError` that `loadGraph`'s `catch` turned into the
   "Could not load the graph" card. Server mode was fine throughout, because it
   does not go through `transformData` at all. Only found by running the small
   graph through the browser harness.
3. **`select_nth_unstable(0)` panics on an empty slice** — reachable from
   `/api/graph/search` with any query that matches nothing, which is routine.

### Phase notes

**P6.1–P6.4** landed as planned. `EMPTY_LIST` is shared by every unhydrated
node; `degreeOf` is a `Uint32Array` column in server mode and `topByDegree` is
a partial selection rather than a sort of every node with an edge; `pushEdge`
dedupes through a `Set` keyed by **wire indices** (~20 characters, not the ~290
two ids cost); the three `new Map(nodes.map(…))` fallbacks are gone.

**P6.5 was not in the plan.** `applyFilters` builds `linkHidden`, which
resolves *both* endpoints of every edge it is asked about — and `neighborsOf`
asks about every edge of the node being expanded. With no filters active that
is 17,000 lookups on a hub to evaluate a predicate that cannot answer true, and
in server mode each lookup also *builds* a node. Both predicates now
short-circuit to `() => false` when nothing is filtered, and `neighborsOf`
skips the lookup as well as the predicate (`state.nodeFilterActive`) — skipping
only the predicate leaves the materialisation, which is the whole cost.
Worth 2.9 ms → 1.2 ms on `soloViewIds`, *better than HEAD*, and it helps local
mode too.

**P7.1/P7.2.** `build_slim_columns` is now the single source both encodings
serialise from, so `slim_index_encodings_describe_the_same_graph` can hold them
together. Front coding measured 2.6× on the real `~/.ug/neo4j` ids (21.8 MB →
8.4 MB at a 16-entry restart; 64 saves a further 0.6 MB and quadruples the
walk, so 16 it is).

**P8.1.** The `NodeStore` *is* `state.nodeById`, which is what kept this to 24
files instead of 60 — every call site that resolves a node id from a server
response (the tour's `stop.node_id`, chat citations, the URL state, the
catalog) is untouched. `fetchEdges` resolves endpoints to **ids, not node
objects**: a hub brings back 8,680 edges of which at most a few hundred are
drawn, and materialising every endpoint built ~8.7k objects to discard ~8.4k of
them. It memoises ids per response (so a hub appearing 8,680 times yields one
string, not 8,680 copies) and tells the store the index it already knows, so
the first lookup of each is a map hit rather than a hash probe.

**P8.2.** `searchNodes` is one implementation for the sidebar box, the command
palette and the walk's seed picker — a bounded scan locally, a request in
server mode. All three are now async with a monotonic token so a slow earlier
keystroke cannot repaint over a later one. `findNodeByName` resolves through
the server, memoising **misses as well as hits**, which is what makes the
re-render terminate.

**P8.3 was not in the plan, and is a behaviour fix.** `/api/graph/search`
truncated at `limit` in graph order. That was fine while the endpoint was only
reachable by hand; it is not fine now that it *is* the page's search box,
because "the 200 that happen to be first" is a different answer from "the best
200" — and it showed up immediately as a different top hit between the two
builds. Matches are now ranked (needle position in the name, then name length)
before the cut, and the endpoint matches qualified **ids** as well as names,
which is what the box has always done locally.

**P9.1.** The worker is created from a `Blob` URL because the page ships as one
assembled HTML file — there is no sibling `.js` for a `Worker` to point at, and
`ug gen` output is opened straight from disk. It fetches the frame and builds
the `id → index` table (500k probe-and-insert steps, the only real main-thread
work left once the frame is binary) and transfers both buffers back. The main
thread re-derives the table's capacity and **checks it against the one it was
handed** — a table sized for a different `n` would probe past its own entries
and answer "no such node" for real ids.


---

# Round 3 — the process, not the tab

> Rounds 1 and 2 moved the browser: 280 MB → 95 MB of JS heap, 2.5 s → 1.5 s to
> interactive. Nothing in either round looked at what `ug` itself costs while it
> does that. This round measures the two processes a user actually waits on —
> `ug gen` and `ug serve` — on the same `~/.ug/neo4j` fixture, and the finding
> is uniform: **the time is fine and the memory is not.**

| Field | Value |
| :--- | :--- |
| **Opened** | 2026-08-24 |
| **Baseline commit** | `1525e63` |
| **Version** | 0.1.16 |
| **Scope** | `native/src/cli`, `native/src/serve`, `native/src/graph`, `native/src/indexer` |
| **Method** | `/usr/bin/time -l` peak RSS and `ps` idle RSS on the real 161,725-node fixture; `curl` medians against a running `ug serve` |
| **Status** | Audited and measured; nothing landed |

## The measurement this round starts from

`~/.ug/neo4j` — 161,725 nodes, 745,964 edges, `graph.json` 330 MB,
`indexed-tree.json` 162 MB. Same fixture as Round 1.

**`ug gen -i <neo4j> --no-ingest --no-cache`:**

| | |
| :--- | ---: |
| wall clock | 5.17 s |
| CPU | 28.99 s user (5.6× parallel) |
| **peak RSS** | **3,378 MB** |
| graph.json written | 330 MB |

**`ug serve --project neo4j`, RSS after each first request:**

| stage | RSS |
| :--- | ---: |
| idle, graph parsed | **1,245 MB** |
| after `/api/graph/stats` | 1,253 MB |
| after `/api/graph/nodes.bin` | 1,297 MB |
| after `/api/graph/edges` (builds `AdjIndex`) | 1,336 MB |
| after `/api/graph/search` | 1,336 MB |
| after `/api/graph/centrality` | 1,569 MB |

**`ug serve` endpoint medians** (5 runs, `curl -w %{time_total}`):

| endpoint | median |
| :--- | ---: |
| `/api/graph/stats` | 0.34 ms |
| `/api/graph/traverse/<id>?k=2` | 0.36 ms |
| `/api/graph/edges` (2,000 ids → 90 KB) | 1.0 ms |
| `/api/graph/cycles` | 1.1 ms |
| `/api/graph/nodes.bin` | 3.2 ms |
| `/api/graph/centrality` | 9.6 ms |
| **`/api/graph/search?q=node`** | **23.3 ms** |

Three facts fall out, and they set the plan:

1. **`ug gen` peaks at 10× the file it writes.** 3,378 MB to produce 330 MB.
   That is the number that decides whether a large repo indexes at all, and
   most of it is the same data materialised three and four times over.
2. **`ug serve` holds 1,245 MB before anyone asks it anything**, of which
   ~252 MB is 1.49 million copies of 161,725 distinct strings and 346 MB is a
   buffer the page is explicitly told never to download.
3. **`/api/graph/search` is 20–70× slower than every other endpoint**, and
   Round 2 pointed the page's search box at it. The Round 2 write-up asserted
   this server "answers in under 1 ms"; that is true of an *empty* query
   (1.8 ms) and wrong by a factor of thirteen for a real one.

### Where the bytes actually are

Measured over the real `graph.json`, counting string content only:

| | content | in memory | interned |
| :--- | ---: | ---: | ---: |
| **edge endpoints** (`source` + `target`) | 192.4 MB | **~252 MB** | 6.0 MB |
| node `id` | 22.8 MB | 22.8 MB | — |
| node `file` | 15.1 MB | 15.1 MB | 0.8 MB |
| node `qualifiedName` | 10.6 MB | 10.6 MB | — |
| node `name` | 6.8 MB | 6.8 MB | — |
| node `calls` (369,605 elements) | 4.2 MB | 16.0 MB | — |
| node `docstring` | 2.3 MB | 2.3 MB | — |

`GraphEdge` is `{ source: String, target: String, edge_type }`. Across 745,964
edges that is 1,491,928 owned `String`s — 192 MB of text plus ~60 MB of headers
and allocator overhead — holding exactly **161,725 distinct values**, every one
of which is already sitting in `parsed.nodes[i].id`. This is the single largest
avoidable allocation in the process, and it is the server-side twin of the
client-side finding Round 2 built `NodeStore` for.

---

## Scoreboard

| # | Item | Phase | Est. impact | Risk | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| P10.1 | `ug gen`: stop parsing `graph.json` a third time, untyped | 0 | **Very high** — measured 3,378 → 2,250 MB | Very low | ⬜ |
| P10.2 | `/api/graph/search`: narrow to the previous prefix's matches | 0 | **Very high** — measured 23.3 → 1.5 ms | Low | ⬜ |
| P10.3 | `ug gen`: drop `index_result.clone()` and the second `IndexResult` | 1 | High — 162 MB clone + a full re-parse | Low | ⬜ |
| P10.4 | Intern `GraphEdge` endpoints to node indices | 2 | **Very high** — ~246 MB of `ug serve`'s 1,245 | Medium | ⬜ |
| P10.5 | Drop `encoded.identity` in server mode | 2 | **High** — 346 MB for a request that never comes | Low | ⬜ |
| P10.6 | `AdjIndex`: CSR rows, borrowed id table | 2 | Medium — 323k `Vec` allocations + 22.8 MB of re-cloned ids | Low | ⬜ |
| P10.7 | `dedupe_edges`: stop cloning 1.5M strings to key a map | 1 | Medium — ~210 MB of transient churn | Very low | ⬜ |
| P10.8 | `extract_return_type`: compile the regex once, borrow the source | 1 | Medium — a regex compile + a body copy per function | Very low | ⬜ |
| P10.9 | `/api/graph/search`: send what the caller reads | 2 | Medium — 133 KB → ~30 KB per keystroke | Low | ⬜ |
| P10.10 | `graph_keyword_search` / `filter_edges_by_type`: unreachable | 3 | Cleanup — no caller outside tests | Very low | ⬜ |

**Sequencing.** Phase 0 is two one-file changes with measured numbers already in
hand and no shape change to anything — land them first. Phase 1 is the rest of
the `ug gen` pipeline, which is self-contained in `cli/gen.rs` and
`graph/build.rs`. Phase 2 is the `ug serve` representation work; P10.4 is the
big one and P10.5/P10.6 are cheap once the edge representation is settled, so
they go together. P10.9 sits in Phase 2 only because it is server-side — it
depends on nothing and can be pulled forward at any point. Phase 3 is
bookkeeping.

**Not a dependency, but worth knowing:** P10.4 changes `GraphData`'s in-memory
shape, which `graph/algos.rs`, `serve/snapshot.rs` and every MCP tool read.
Doing P10.6 first would mean writing the `AdjIndex` twice.

---

## Phase 0 — Two measured wins, no shape change

### ⬜ P10.1 — `ug gen`: stop parsing `graph.json` a third time, untyped

**Where:** `native/src/cli/gen.rs:236`

**Evidence.** By the time control reaches this line, `graph.json`'s text has
already been parsed into a typed `GraphData` fifteen lines above:

```rust
let parsed_graph: Option<ultragraph::types::GraphData> = serde_json::from_str(&graph).ok();
```

and `GraphData` carries `pub resolution: Option<ResolutionStats>` — the exact
four numbers the next block wants. It parses the whole 330 MB document a second
time anyway, into `serde_json::Value`:

```rust
if let Ok(v) = serde_json::from_str::<serde_json::Value>(&graph) {
    if let Some(r) = v.get("resolution") {
        let n = |k: &str| r.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
```

An untyped `Value` is the most expensive representation serde has — every
object becomes a `Map<String, Value>`, every number a tagged enum — and this one
exists to read four `u32`s that are already in a struct on the stack. It is also
alive at the same time as `parsed_graph`, so it lands squarely on the peak.

**Fix.** Read them off `parsed_graph`:

```rust
if let Some(r) = parsed_graph.as_ref().and_then(|g| g.resolution.as_ref()) {
    let (q, t, b, d) = (
        r.resolved_qualified as u64,
        r.resolved_typed as u64,
        r.resolved_by_name as u64,
        r.dropped_unresolved as u64,
    );
```

**Measured**, `ug gen -i <neo4j> --no-ingest --no-cache`:

| | before | after | |
| :--- | ---: | ---: | ---: |
| peak RSS | 3,378 MB | **2,250 MB** | **1.50×** |
| wall clock | 5.17 s | **4.69 s** | 1.10× |

Output is byte-identical — the `calls:` line reads
`543808 resolved (0 by path, 540096 by receiver type, 3712 by name), 286079 unresolved`
on both builds.

**Prove it.** `/usr/bin/time -l ug gen -i <neo4j> --no-ingest --no-cache`, and
diff the stdout against the baseline run.

**Risk.** Very low. One expression replaced by another reading the same data
from a value that is already in scope; the `if let` arms are the same shape.

<a id="p102--prefix-narrowed-search"></a>
### ⬜ P10.2 — `/api/graph/search`: narrow to the previous prefix's matches

**Where:** `native/src/serve/api.rs:676-709`

**Evidence.** Every request walks all 161,725 nodes and, per node, allocates a
lowercased copy of the name (42 chars average) and of the qualified id (141
chars average) to run a substring test on:

```rust
let lower = n.name.to_lowercase();
let at = lower.find(&needle);
…
let id_match = !name_match && n.id.to_lowercase().contains(&needle);
let doc_match = n.docstring.as_ref().map(|d| d.to_lowercase().contains(&needle)).unwrap_or(false);
```

That is ~30 MB of text scanned and copied per request. Round 2's P8.2 made this
endpoint the page's search box in server mode and fires it per keystroke, so the
cost is paid once per character typed. Measured, `?limit=200`:

| query | median |
| :--- | ---: |
| `q=` (empty) | 1.8 ms |
| `q=n` | 17.1 ms |
| `q=node` | 23.3 ms |
| `q=getNode` | 23.2 ms |

The empty query is the control: it takes the same walk and pushes the same
`ranked` entries, and skips only the matching. So **21.5 of the 23.3 ms is the
match**, and none of it is the walk.

> **The obvious fix is the wrong one, and it was measured.** Replacing both
> `to_lowercase()` calls with an allocation-free ASCII case-insensitive scan
> made the endpoint **slower**: 23.3 ms → 33.0 ms. `str::find` is a tuned
> two-way/`memchr` search and a hand-rolled byte loop does not come close;
> the allocation was never the expensive half. Recorded in
> [Rejected / deferred](#rejected--deferred) so it is not re-proposed.

**Fix.** Stop rescanning the graph. A search box is typed one character at a
time, and substring containment is monotone under prefix: if `nod` is a prefix
of `node`, then every node matching `node` in some field already matched `nod`
in that field. So memoise the last `(needle, matching node indices)` on the
snapshot, and when the incoming needle starts with the memoised one, iterate
that index list instead of `parsed.nodes`.

The memo is one `Mutex<Option<(String, Vec<u32>)>>` on `GraphSnapshot`, bounded
by the node count (647 KB at its worst — a single-letter query matches almost
everything, because the ids are long paths). It is invalidated for free by
snapshot replacement, since it lives on the snapshot.

**Measured**, typing `nodepr` one character at a time:

| keystroke | matches | before | after | |
| :--- | ---: | ---: | ---: | ---: |
| `n` | 161,721 | 19.0 ms | 18.8 ms | — (nothing to narrow from) |
| `no` | 20,539 | 21.5 ms | **4.3 ms** | 5.0× |
| `nod` | 8,767 | 22.2 ms | **1.6 ms** | 13.9× |
| `node` | 8,704 | 23.3 ms | **1.5 ms** | 15.5× |
| `nodep` | 554 | 22.5 ms | **0.76 ms** | 29.6× |
| `nodepr` | 338 | 23.8 ms | **0.75 ms** | 31.7× |

`count` is identical on both builds for all six queries, so the ranking and the
truncation flag are unchanged.

**Prove it.** The `count` for a set of queries must match the baseline exactly —
that is the whole correctness argument, since the memo changes only which nodes
are *examined*. Add a test that issues a prefix chain (`n`, `no`, `nod`, `node`)
and a second that issues them in the reverse order, and assert both produce the
same counts as a fresh snapshot per query.

**Risk.** Low, with one sharp edge: the type filter runs *before* the match, so
the memo must record the matches of the **needle alone**, not of
`needle + types`, or a request with a narrower `?types=` will poison the memo
for the next request without one. Key the memo on the needle and apply the type
filter to the narrowed candidates, exactly as the un-narrowed path does.

The prototype cloned the memo `Vec` per request (647 KB at worst) to avoid
holding the lock across the scan. Hold an `Arc<Vec<u32>>` instead.

---

## Phase 1 — The rest of the `ug gen` pipeline

**Theme:** the pipeline is stitched together with JSON strings between stages
that both have the typed value in hand. Each seam costs a serialise, a parse,
and a full second copy of the data.

### ⬜ P10.3 — Drop `index_result.clone()` and the second `IndexResult`

**Where:** `native/src/cli/gen.rs:213`, `native/src/graph/mod.rs:20-28`,
`native/src/cli/update.rs:172`

**Evidence.** `index()` returns a **JSON string** (162 MB on this fixture), not
an `IndexResult`. `build_graph` takes that string **by value**, so the caller —
which needs it again at `gen.rs:259` to write `indexed-tree.json` — clones it:

```rust
let graph = build_graph(index_result.clone());   // +162 MB
```

and `build_graph` then parses it back into the struct it was serialised from:

```rust
pub fn build_graph(index_json: String) -> String {
    let index_result: crate::types::IndexResult = serde_json::from_str(&index_json)…;
    let graph = build::build_graph_from_index(&index_result);
    serde_json::to_string(&graph).unwrap_or_default()
}
```

So at the peak the process holds: the index JSON, a clone of the index JSON, an
`IndexResult` parsed from it, the `GraphData` built from that, and the 330 MB
graph JSON — five materialisations of two logical values.

**Fix.** Three independent steps, in increasing order of blast radius:

1. Write `indexed-tree.json` *before* `build_graph`, then move
   `index_result` in rather than cloning it. One line moved; kills 162 MB.
2. Give `build_graph` a borrowed sibling — `build_graph_from_index` is already
   `pub(crate)` and takes `&IndexResult`. `build_graph(String) -> String` stays
   as the library-facing wrapper.
3. Make `index()` return the typed `IndexResult` and serialise at the one place
   that writes the file. This is the real fix and the one that removes the seam
   rather than working around it; it touches `cli/index.rs`, `cli/update.rs`
   and `cli/demo.rs` as well.

**Prove it.** Peak RSS on the same `ug gen` command, cumulative with P10.1.
`graph.json` and `indexed-tree.json` must be byte-identical to the baseline.

**Risk.** Low for (1) and (2) — the ordering of two `fs::write` calls and a new
entry point beside an existing one. Medium for (3), which changes a public
signature; it is listed separately so it can be deferred without losing (1).

### ⬜ P10.7 — `dedupe_edges`: stop cloning 1.5M strings to key a map

**Where:** `native/src/graph/build.rs:1353-1364`

**Evidence.**

```rust
fn dedupe_edges(edges: &mut Vec<GraphEdge>) {
    let mut seen: HashMap<(String, String, GraphEdgeType), bool> = HashMap::new();
    edges.retain(|e| {
        let key = (e.source.clone(), e.target.clone(), e.edge_type.clone());
```

Two owned `String`s per edge, allocated to be hashed and dropped. On this
fixture that is 1,491,928 allocations totalling ~192 MB of copying, in a
function whose entire job is to decide a boolean. The `HashMap<_, bool>` is a
`HashSet` with a wasted byte and its padding per entry.

**Fix.** Decide the keep-set from a borrow, then apply it:

```rust
let mut seen: HashSet<(&str, &str, &GraphEdgeType)> = HashSet::with_capacity(edges.len());
let keep: Vec<bool> = edges.iter().map(|e| seen.insert((&e.source, &e.target, &e.edge_type))).collect();
let mut it = keep.into_iter();
edges.retain(|_| it.next().unwrap_or(true));
```

The borrow and the mutation are in separate statements, so this needs no
unsafe and no second `Vec<GraphEdge>`.

**Prove it.** Edge count after `build_graph` must be unchanged (745,964), and
`graph.json` byte-identical. Peak RSS on `ug gen`, and a `--release` timing of
`build_graph_from_index` alone in `native/tests/graph_bench.rs`.

**Risk.** Very low. Same predicate, same iteration order, no allocation.

### ⬜ P10.8 — `extract_return_type`: compile the regex once, borrow the source

**Where:** `native/src/indexer/common.rs:302-319`

**Evidence.** Called once per function node by `rust.rs:277`, `typescript.rs:435`
and `python.rs:317`. When the grammar has no `return_type` field to offer — a
Rust `fn` with no `-> T`, an unannotated Python `def`, which is the common case —
it falls through to:

```rust
let node_text = get_node_text(Some(*node), source)?;      // copies the whole function body
let return_re = regex::Regex::new(r"\)\s*:\s*([^\s{]+)").ok()?;   // compiles a regex
```

Both per function. `get_node_text` (`common.rs:74`) is
`String::from_utf8(source[start..end].to_vec())` — it copies the entire body of
every function in the repo onto the heap so a regex can look at its first line.

The same shape is in `typescript.rs:234` and `:275` and `python.rs:179` and
`:220`, compiled once per *file* rather than per function.
`markdown.rs:248` already does it correctly and is the model:

```rust
fn link_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(…).expect("…"))
}
```

**Fix.** A `static OnceLock<Regex>` per pattern, following `link_regex`. And
give `common.rs` a `node_str(node, source) -> Option<&str>` that returns
`std::str::from_utf8(&source[start..end]).ok()` — a borrow, no copy — for the
read-only callers. `extract_return_type` becomes allocation-free except for the
capture it actually returns.

**Prove it.** `ug gen --no-ingest --no-cache` wall clock and peak RSS on a
Python- or TypeScript-heavy repo (the neo4j fixture is Java, and `java.rs` does
not call `extract_return_type` at all — pick a fixture that exercises the path).
The graph must be byte-identical.

**Risk.** Very low. `expect` on a literal pattern is what `link_regex` already
does; a pattern that does not compile is a build-time bug, not a runtime one.

---

## Phase 2 — What `ug serve` holds

### ⬜ P10.4 — Intern `GraphEdge` endpoints to node indices

**Where:** `native/src/types.rs:745-749`

**Evidence.** See [Where the bytes actually are](#where-the-bytes-actually-are):
745,964 edges × 2 owned `String`s = ~252 MB resident, holding 161,725 distinct
values that `parsed.nodes[i].id` already owns. Interned to `u32` that is 6.0 MB —
**~246 MB, a fifth of `ug serve`'s 1,245 MB idle RSS**, and it comes off
`ug gen`'s peak as well.

It is also load-bearing for latency in three places that currently re-derive
what interning would make free: `build_adj` hashes 1.49M strings to build the
index (`snapshot.rs:115`), `api_edges` hashes two more per edge it returns
(`api.rs:480`), and `dedupe_edges` clones 1.49M of them (P10.7).

**Fix.** Keep `GraphEdge`'s **wire** shape exactly as it is — `graph.json` is a
published format that the MCP tools, the agent tools and every older client
read. Change only the in-memory representation, behind a serde adapter:
`GraphData` deserialises endpoints through the node-id table it is already
building, storing `src: u32, tgt: u32`, and serialises them back out as strings
by indexing `nodes`.

**Prove it.** `ug serve --project neo4j` idle RSS, which is a single `ps` read.
`graph.json` round-trips byte-identically — parse it, re-serialise it, `cmp`.
The full suite: this touches every reader of `e.source`.

**Risk.** Medium, and it is the reason this is Phase 2 rather than Phase 0. The
node-id table has to exist before the first edge is deserialised, which means
either a two-pass deserialiser or a `DeserializeSeed`. An edge naming a node
that does not exist is currently representable and would have to become either
an error or a sentinel — `build_adj` silently drops those today
(`snapshot.rs:115`'s `if let (Some(&si), Some(&ti))`), so the sentinel preserves
behaviour and an error does not.

### ⬜ P10.5 — Drop `encoded.identity` in server mode

**Where:** `native/src/serve/snapshot.rs:15`, `:406-431`,
`native/src/serve/encoding.rs:31-36`

**Evidence.** `GraphSnapshot` holds `encoded: EncodedAsset`, whose `identity:
Bytes` is the entire `graph.json` text — 346 MB here — for the life of the
snapshot, to serve `GET /graph.json`.

In server mode the page never asks for it. That is the entire point of server
mode: `snapshot.rs:139` puts the cutoff at 50 MB, and `00-preamble.js:374`
branches to `loadNodeIndex` and never touches the file. So on exactly the graphs
where this buffer is large, it is dead weight — and it is 28% of idle RSS.

Worse if anyone *does* request it: `EncodedAsset::brotli()` would compress 346 MB
and retain the result in a second `OnceLock`, forever.

**Fix.** When the resolved mode is `server`, don't retain the identity bytes.
`GET /graph.json` re-reads from disk and streams the file — `mtime` is already
on the snapshot to validate against, and this is a request the page is
documented not to make. `EncodedAsset` keeps its current form for the assets
where it earns its keep (the HTML and the renderer bundles, which are small,
hot, and have no file behind them).

Two consequences to handle: `GraphSnapshot::token()` reads
`self.encoded.identity.len()` — keep the length as a `usize` field. And
`ProjectContext::approx_bytes()` (`registry.rs:139`) is `identity * 3 +
retained`; with the identity gone the estimate needs a different basis.

**Prove it.** `ug serve --project neo4j` idle RSS. `curl -o - /graph.json | cmp`
against the file, in both modes, with and without `Accept-Encoding`.

**Risk.** Low. One request path changes from "serve a buffer" to "stream a
file", and `ug serve` already binds to loopback.

> **Note on the cache budget.** `snapshot_cache_budget()` is 512 MiB and
> `approx_bytes()` estimates this snapshot at 346 × 3 + 346 = **1,384 MB**
> against a measured 1,245 MB — the estimator is well calibrated and this is
> not a bug. But it means one neo4j project is already 2.7× the entire budget,
> and `evict_over_budget` never evicts the active project. On a graph this size
> the only way under the ceiling is to make the snapshot smaller, which is what
> P10.4 and P10.5 do — together they take the estimate to roughly 750 MB.

### ⬜ P10.6 — `AdjIndex`: CSR rows, borrowed id table

**Where:** `native/src/serve/snapshot.rs:85-125`

**Evidence.**

```rust
pub(crate) struct AdjIndex {
    pub(crate) id_to_idx: HashMap<String, usize>,
    pub(crate) out: Vec<Vec<u32>>,
    pub(crate) inc: Vec<Vec<u32>>,
}
```

Two problems, both measured at 39 MB combined on this fixture (the
`/api/graph/edges` step of the RSS table above):

1. `out` and `inc` are 323,450 separately-allocated `Vec`s — 24 bytes of header
   each plus whatever the doubling growth strategy overshot — to hold 1.49M
   `u32`s that are pushed in a single known pass. `graph/algos.rs:357` already
   has the right structure for this and says so in its own doc comment: *"one
   flat"* CSR. Two counting passes give exact capacity and one allocation.
2. `id_to_idx` clones every node id (`snapshot.rs:107`) — a third copy of the
   22.8 MB of id text, after `parsed.nodes` and `encoded.identity`.

**Fix.** Flatten `out`/`inc` to `offsets: Vec<u32>` + `flat: Vec<u32>`;
`incident()` keeps its signature and returns two slices chained. For the id
table, do what the client's `NodeStore` does (P9.1): a probe table of `u32`
indices, hashing through `nodes[i].id` rather than owning a key.

**Prove it.** RSS delta across the first `/api/graph/edges` request, and its
median latency (currently 1.0 ms for 2,000 ids). `/api/graph/traverse` and
`/api/graph/edges` results must be unchanged.

**Risk.** Low, and lower after P10.4 — with interned endpoints, `build_adj`
needs no id table at all for the edge walk. Do these in that order.

### ⬜ P10.9 — `/api/graph/search`: send what the caller reads

**Where:** `native/src/serve/api.rs:719-726`,
`native/src/vis/js/02-dialogs.js:807-845`

**Evidence.** The endpoint serialises whole `GraphNode` rows — metrics,
signature, docstring, imports, exports, calls, annotations, boundaries.
`?q=node&limit=200` is **133 KB**. The one caller reads two things out of each
row:

```js
for (const row of data.nodes || []) {
    if (boundaryOnly && !(row.boundaries && row.boundaries.length)) continue;
    const node = state.nodeById.get(row.id);
    if (node) nodes.push(node);
}
```

`row.id`, and whether `row.boundaries` is non-empty. Everything else is parsed
and dropped, per keystroke.

The client then **re-sorts by name**, reproducing the ranking the server did in
P8.3 — the same comparison, on the same 200 rows, twice.

**Fix.** A projection: `?fields=id` returns `{"ids": [...], "boundary": [...]}`
with the counts unchanged, and the full-row shape stays the default for the
hand-driven callers the endpoint was built for. Drop the client-side re-sort and
document that the server's order is the order — it already is, since P8.3.

**Prove it.** Response bytes at `limit=200`, and the rendered result list must
be identical (same 200 nodes, same order).

**Risk.** Low. Additive parameter; the existing shape is untouched.

---

## Phase 3 — Bookkeeping

### ⬜ P10.10 — `graph_keyword_search` / `filter_edges_by_type`: unreachable

**Where:** `native/src/graph/algos.rs:277-354`, exported at `native/src/lib.rs:44-45`

**Evidence.** Neither has a caller in `cli/`, `serve/`, `mcp/` or
`agent_tools/`. The only references outside their own definitions are
`native/tests/search_test.rs` and `native/tests/graph_test.rs` — they are
public API that nothing but its own tests uses. Both also carry the exact
defects the earlier rounds fixed elsewhere:

- `filter_edges_by_type` runs `format!("{:?}", e.edge_type)` **and** two
  `to_lowercase()` calls per (edge × requested type) — 4.5M allocations for
  three types over this fixture — the pattern P3.3 removed from
  `/api/graph/stats`.
- `graph_keyword_search` does the same `format!("{:?}")` per node
  (`algos.rs:328`), re-parses the whole graph from a JSON string like the
  `shortest_path` that P4.1 fixed, and `.cloned()`s an unbounded result set
  like the `api_search` that P4.4 bounded.

**Fix.** Decide which they are. If they are API, give them the P3.3/P4.1/P4.4
treatment and a caller. If they are not, delete them and their tests; the
capability exists at `/api/graph/search` and in the MCP tools, better done.

**Prove it.** `cargo build` after deletion, and the suite.

**Risk.** Very low either way. Worth resolving before anyone optimises them by
reflex.

### Smaller things found in the same pass, not worth their own item

- `load_snapshot` (`snapshot.rs:412-415`) does `fs::read` → `String::from_utf8`
  → `serde_json::from_str`, which validates 346 MB of UTF-8 twice.
  `serde_json::from_slice` does it once, and the bytes go to `EncodedAsset`
  either way.
- `api_edges` (`api.rs:509`) does `body.ids.clone()` to fill the `complete`
  field. `body` is owned and dead after this; move it.
- `project.json` for this fixture is **892 KB**, almost all of it the indexed
  file list. It is read whole for anything that wants the node count. Worth
  checking what `/api/projects` and the staleness poll actually deserialise
  before this grows further.
- `MCP`'s `evict_over_budget` (`mcp/mod.rs:457`) uses `Vec::remove(0)` — the
  pattern P1.1 removed from Brandes — but over an LRU list a handful of entries
  long. Correct as written; noted so it is not flagged again.

---

## Results log

Append one row per landed item. Keep the numbers, not just the verdict.

| Date | Item | Fixture | Before | After | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 2026-08-18 | P1.1 | synthetic n=1600 | 1166.1 ms | 5.0 ms | 235×; ratio widens with size |
| 2026-08-18 | P1.1 | `overgraph` (15k/44k) | — | 17.8 ms | baseline impractical to run |
| 2026-08-18 | P1.1 | `neo4j` (162k/746k) | never returned | 3,266 ms | first time this graph is scoreable |
| 2026-08-18 | P1.1 | — | all-zero output | correct | two bugs; see P1.1 §What shipped |
| 2026-08-18 | P3.1 | `neo4j` serve startup | 6.66 s | 0.32 s | 21×; graph-ready, not just `/healthz` |
| 2026-08-18 | P3.1 | `neo4j` idle RSS | 919 MB | 827 MB | unbuilt encodings cost nothing |
| 2026-08-18 | P3.2 | `neo4j` graph.json | already `br` | unchanged | premise wrong — no transfer win; progress bar fixed |
| 2026-08-18 | P3.3 | `/api/graph/stats` | 19.8 ms | 0.4 ms | 50×, memoised per snapshot |
| 2026-08-18 | P1.2 | synthetic n=8000 (parsed) | 13.4 ms | 1.6 ms | 8.4×; 2.8× when both parse JSON |
| 2026-08-18 | P1.3 | `neo4j` 2-hop traversal | 146.7 ms | 119.8 ms | 1.2× — estimate was wrong, see P1.3 |
| 2026-08-18 | P1.3 | — | wrong hop distances | correct | LIFO walk recorded first-found, not shortest |
| 2026-08-18 | P4.1 | MCP cached project | +346 MB retained | 0 | second full copy of graph.json, freed |
| 2026-08-18 | P4.2 | `mmr_rerank` | O(k²·n·d) | O(k·n·d) | output bit-identical, asserted vs reference |
| 2026-08-18 | P5.1 | hover, graph drawn whole | O(edges) each | O(degree) each | index built once per view change |
| 2026-08-20 | P6–P9 | 485k nodes, Chrome tab | 280 MB heap | 95 MB heap | 2.9×; median of 8, `usedJSHeapSize` |
| 2026-08-20 | P6–P9 | 485k nodes, index only (V8) | 426 MB peak / 338 MB held | 58 MB both | 7.3× peak; build 393 ms → 7 ms |
| 2026-08-20 | P7.1 | 485k node index, wire | 98.2 MB (7.1 MB gz) | 51.7 MB (9.7 MB gz) | half the bytes held, 2.6 MB more transferred |
| 2026-08-20 | P7.2 | `neo4j` ids, front-coded | 21.8 MB | 8.4 MB | 2.6×, 16-entry restart |
| 2026-08-20 | P6–P9 | 485k nodes, load → interactive | 2,485 ms | 1,531 ms | 1.6× |
| 2026-08-20 | P6.5 | hub expansion (`soloViewIds`) | 2.9 ms | 1.2 ms | beats HEAD; 17k needless lookups removed |
| 2026-08-20 | P6–P9 | cold click, degree-8,680 hub | 69 ms | 38 ms | 1.8×, once the post-load GC has settled |
| 2026-08-20 | P8.2 | keyword search, 485k nodes | 77 ms on the main thread | 82 ms, none of it on it | same wall clock, responsive tab |
| 2026-08-20 | — | client front-coding decoder | silently wrong ids | correct | scratch grew without copying; see §Bugs |
| 2026-08-24 | — | `ug gen` neo4j, peak RSS | 3,378 MB | — | baseline for Round 3; 10× the 330 MB it writes |
| 2026-08-24 | — | `ug serve` neo4j, idle RSS | 1,245 MB | — | baseline; before any request is served |
| 2026-08-24 | P10.1 | `ug gen` neo4j, peak RSS | 3,378 MB | 2,250 MB | 1.50×; one redundant `Value` parse, output identical |
| 2026-08-24 | P10.1 | `ug gen` neo4j, wall clock | 5.17 s | 4.69 s | 1.10× |
| 2026-08-24 | P10.2 | `/api/graph/search?q=node` | 23.3 ms | 1.5 ms | 15.5×; prefix-narrowed, `count` identical |
| 2026-08-24 | P10.2 | `/api/graph/search?q=nodepr` | 23.8 ms | 0.75 ms | 31.7×; widens as the query lengthens |
| 2026-08-24 | P10.2 | `/api/graph/search?q=n` | 19.0 ms | 18.8 ms | unchanged — the first keystroke has nothing to narrow from |
| 2026-08-24 | — | `graph.json` edge endpoints | 1.49M `String`s / ~252 MB | — | 161,725 distinct values; 6.0 MB interned (P10.4) |

---

## Rejected / deferred

Move items here rather than deleting them, with the measurement that killed
them — so the next audit does not re-propose them.

| Item | Why |
| :--- | :--- |
| Storage layer / PPR / vector index tuning | Measured 2026-08-16: not the bottleneck at 162k nodes. `/api/graph/cycles` over 746k edges runs in 0.33 s. Do not go looking here again. |
| Index identity for server mode (P9.2) | Would take the 485k index from 58 MB to ~18 MB and the wire from 51.7 MB to 9.5 MB — the biggest remaining win by far. Deferred because identity is not a client-side detail: eight endpoints return real qualified ids, the deep-link URL format is one, and 60+ client call sites pass them around opaquely. Revisit when a real 500k index exists to test against. |
| Chat retrieval latency | Measured 2026-08-16: wall time is entirely local-LLM tokens (~83 tok/s decode). Tool execution was 0.1 s across 4 calls. |
| Allocation-free lowercase scan in `api_search` | Measured 2026-08-24: **slower**, 23.3 ms → 33.0 ms on the 162k fixture. `to_lowercase()` feeds `str::find`, which is a tuned two-way/`memchr` search; a hand-rolled ASCII byte loop loses to it by more than the allocation costs. The scan was never the allocation — see [P10.2](#p102--prefix-narrowed-search). |
