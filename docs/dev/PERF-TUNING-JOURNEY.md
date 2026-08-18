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
| **Method** | Static audit of hot paths; no profiler run yet (see [Measurement](#measurement)) |

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
| P5.1 | Hover: use the adjacency index already built | 5 | **High** — perceived responsiveness | Low | ⬜ |
| P5.2 | Gizmo: stop rewriting `innerHTML` at 30 Hz | 5 | Low–medium | Very low | ⬜ |

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

### ⬜ P5.1 — Hover: use the adjacency index already built

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

### ⬜ P5.2 — Gizmo: stop rewriting `innerHTML` at 30 Hz

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

---

## Rejected / deferred

Move items here rather than deleting them, with the measurement that killed
them — so the next audit does not re-propose them.

| Item | Why |
| :--- | :--- |
| Storage layer / PPR / vector index tuning | Measured 2026-08-16: not the bottleneck at 162k nodes. `/api/graph/cycles` over 746k edges runs in 0.33 s. Do not go looking here again. |
| Chat retrieval latency | Measured 2026-08-16: wall time is entirely local-LLM tokens (~83 tok/s decode). Tool execution was 0.1 s across 4 calls. |
