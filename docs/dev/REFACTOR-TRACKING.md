# Refactor tracking — 2026-08-23

File-structure refactor of `native/src/`. Mark each batch `[x]` only after its
verify passes. If interrupted, resume at the first unchecked batch — earlier
batches are self-contained and already verified. Baseline: 888 tests
(607 lib + 281 integration), clippy clean. NOTE: `[[bin]] ug` has `test =
false` — verify with `cargo nextest run --lib` + `--test integration`, and
`cargo check --all-targets` (not `--bins`).

## Pre-done (uncommitted, from previous session)
- [x] cli/serve/chat/tour/project/config/mcp/assets promoted into the lib;
      `main.rs` is a shim; `extern crate self as ultragraph` binds old paths.
- [x] tests/all.rs → tests/integration.rs.

## Batches

### 1. CLI renames + stale doc fixes  — DONE (verified check --all-targets + nextest --lib 607/607)
- [ ] `cli/analysis.rs` → `cli/graph_algos.rs` (it is the CLI face of
      graph.rs algorithms, not the `analyze` tool).
- [ ] drop `_cmd` suffixes: `analyze_cmd.rs`→`analyze.rs`, `chat_cmd.rs`→
      `chat.rs`, `config_cmd.rs`→`config.rs`, `index_cmd.rs`→`index.rs`,
      `tour_cmd.rs`→`tour.rs` (safe now the lib owns the bare names).
- [ ] `cli/store.rs` → `cli/dest.rs` (it resolves `--dest`, not stores).
- [ ] dispatch `"mcp"` straight to `mcp::run`; drop the forwarder in
      `cli/connect.rs`.
- [ ] fix stale `storage/mod.rs` module map (`analyze` line → `query`, add
      missing submodules).
- [ ] fix `serve.rs` doc pointer `docs/SERVE.md` → `docs/WEB-SERVE.md`.
- Verify: `cargo check --all-targets` + `cargo nextest run --lib`.

### 2. graph.rs String-overloads  — DONE (also: centrality/cycles now return typed CentralityResult/CycleResult; traversal_test wrapper-parity test removed; count 887 = 607 lib + 280 integration + 14 ignored benches)
- [ ] delete the JSON-String variants (`calculate_centrality`, `detect_cycles`,
      `find_shortest_path`, `k_hop_bfs`) — AGENTS.md §9b footgun.
- [ ] rename `*_graph` variants to the canonical unsuffixed names
      (`calculate_centrality_graph` → `calculate_centrality`, etc.), fix
      callers (lib.rs re-exports, graph_algos.rs, serve.rs, integration tests).
- Verify: same as batch 1.

### 3. serve.rs (6.5k lines) → serve/ directory  — DONE (serve.rs is now the 911-line mod root; api.rs further split into api/chat_api/db_api/projects_api. Verified check --all-targets + nextest --lib 607/607)
Carve along the existing `// ---------- section ----------` markers:
- [x] `serve/mod.rs` — run_serve, ServeState, tracing, project switching.
- [x] `serve/registry.rs` — ProjectRegistry, open_serve_stores, contexts.
- [x] `serve/encoding.rs` — EncodedAsset, gzip/brotli, negotiation.
- [x] `serve/snapshot.rs` — GraphSnapshot, AdjIndex, slim columns/index.
- [x] `serve/nidx.rs` — binary slim index (old inline `mod nidx`).
- [x] `serve/gen_jobs.rs` — background `ug gen` jobs.
- [x] `serve/watch.rs` — Phase 1.5 file-watch reload.
- [x] `serve/host_guard.rs` — allowed-host middleware.
- [x] `serve/router.rs` — build_router + static handlers.
- [x] `serve/api.rs` — the `api_*` handlers (bulk).
- [x] router_tests.rs / nidx_tests.rs stay as siblings; `mod` lines updated.
- Verify: `cargo check --all-targets`, `cargo nextest run --lib`
  (router_tests + nidx_tests live there).

### 4. agent_tools.rs (5.6k lines) → agent_tools/ directory  — DONE
(mod.rs 840 = shared machinery + dispatch; 9 tool files; tests.rs sibling.
`shortest_path` exists where the plan guessed `search`; traverse/context kept
separate. `line_window`/`top_counts` bumped to pub(crate) for tests.rs.
Verified check --all-targets clean, nextest --lib 607/607, --test integration
280/280, clippy: 0 lints under agent_tools/.)
- [x] `mod.rs` — shared machinery: Render helpers, node/edge type strs,
      refs/Matchers, by_id_map, node_loc.
- [x] one file per tool family: find_symbols, file_outline, get_code,
      find_usages, traverse/context, project_overview, graph_schema,
      search.
- Verify: same as batch 1.

### 5. style module  — DONE
(new `src/style.rs` = C_* + `color` gate + `Render`; `lib.rs` keeps
`pub use style::*`, `agent_tools` keeps `pub use crate::style::Render`.
The ~30 `use ultragraph::{C_*}` call sites were left on the root re-export
on purpose — `ultragraph::C_*` IS the canonical root path, and rewriting
them to `::style::` fights the deliberate `extern crate self` binding.
Verified check --all-targets clean, nextest --lib 607/607, clippy 0 lints
in style.rs / lib.rs / agent_tools.)
- [x] new `style.rs`: C_* constants + `Render` enum (moved from lib.rs +
      agent_tools) + the color gate; lib.rs keeps `pub use style::*` so
      `ultragraph::C_*` / `agent_tools::Render` paths still resolve.
- Verify: same as batch 1.

### 6. types.rs split  — DONE
(The six algorithm results moved to graph.rs under an `Algorithm results`
banner; lib.rs re-exports them so `ultragraph::BfsResult` still resolves, and
the `ultragraph::types::X` call sites in cli/graph_algos.rs + 5 test files were
repointed. `ResolutionStats` deliberately STAYED in types.rs: it is a field of
`GraphData`, not a function's return shape, and moving it would have inverted
the types->graph dependency.)
- [x] algorithm results (BfsResult, PathResult, CentralityResult,
      CycleResult, FilteredEdgesResult, SearchResult) move into graph.rs
      beside their functions; removed from types.rs.
- [~] ResolutionStats — left in types.rs on purpose, see above.
- Verify: same as batch 1.

### 7. storage/embed dir + api table placement  — DONE
(`storage::embed_local` is now `storage::embed::local`; the one call site
(cli/doctor.rs) and two limits.rs doc refs were repointed, and
`storage::LocalEmbedder` still re-exports. The endpoint table is now
`serve/endpoints.rs` (pub(crate) ApiEntry + API_ENDPOINTS) and cli/api.rs is a
pure printer of it. `ug api` output verified byte-identical in a smoke run.)
- [x] `storage/embed.rs` + `embed_local.rs` -> `storage/embed/{mod,local}.rs`.
- [x] `cli/api.rs` endpoint table moves beside the router (`serve/`), the
      `ug api` command becomes a thin printer of it.
- Verify: same as batch 1.

### 8. `ug update` → `ug reindex` (user-visible; deferred)
- [ ] NOT done this pass — needs hook.rs messages, help, api.rs table,
      AGENTS.md, docs/API-REFERENCE.md §1.x, api-reference.html #tab-cli,
      index.html, possibly install.sh — full §7.2 sync. Do as its own change.

## Gate — batches 1-7 (passed 2026-08-23)
- [x] `cd native && cargo check --all-targets` — clean, no warnings
- [x] `cd native && cargo nextest run --lib` — 607/607
- [x] `cd native && cargo nextest run --test integration` — 280/280 (+14 ignored)
- [x] `cd native && cargo clippy --all-targets` — 53 unique diagnostics, all
      pre-existing and all in verbatim-moved code (doc_lazy_continuation,
      type_complexity, map_entry, for_kv_map...). The files this refactor
      actually authored — style.rs, serve/endpoints.rs, storage/embed/,
      lib.rs, types.rs — are lint-free.
      NOTE: the rtk hook rewrites `cargo`/`grep` and compacts their output;
      `--message-format json` must go through `rtk proxy "cargo clippy ..."`
      or you will silently read an empty lint list.
- [x] `./native/target/debug/ug help` smoke — plus `ug api`, which exercises
      the relocated endpoint table.
- [x] refresh ultragraph index — `ug gen -n ug` (4387 nodes / 11772 edges,
      497 stale pruned); `style.rs` + `serve/endpoints.rs` resolve.
- [x] AGENTS.md path references — one stale ref fixed (`serve.rs`
      `file_from_disk` -> `serve/db_api.rs`).

---

# Round 2 — comprehension pass (batches 9-12)

Batches 1-8 were about *file placement*. These are about what is still hard to
read once the files are in the right place. Found by surveying the post-refactor
tree: longest pure-code files, longest real functions, and duplicated dispatch.
Ordered cheapest-first so each lands independently; same verify loop as batch 1.

### 9. Delete the dead `__bench_probe_*` scaffolding  — DONE
(9 removed. Confirmed nothing referenced them first.)
- [x] `graph.rs:1955-1979` — nine `#[allow(dead_code)] fn __bench_probe_N() ->
      u32 { N }`, leftovers from the incremental-rebuild timing work. The only
      other hit repo-wide is `docs/ug-website/demo/graph.json`, which merely
      *indexed* them as symbols; nothing calls them. Free deletion.
- Note: the demo graph.json will still list them until it is regenerated. It
      is a committed fixture, not live data — leave it.
- Verify: `cargo check --all-targets` + `cargo nextest run --lib`.

### 10. graph.rs (2,028 lines) → graph/ directory  — DONE
(build.rs 1,343 / algos.rs ~700 / mod.rs 30. `build_graph_from_index` became
`pub(super)` so mod.rs can call it; build.rs exports nothing else public, so
mod.rs re-exports only `algos::*` and the lib.rs list is unchanged.)
Two disjoint halves sharing a file, with NO shared state across the seam:
- [x] `graph/build.rs` — construction: `FILE_RESOLVE_EXT_CANDIDATES`, the
      `MAX_*` caps, `QualifiedIndex`, `build_graph_from_index`,
      `resolve_qualified_import`, `build_file_indexes`,
      `resolve_import_to_file_id`, `lookup_with_extensions`, `lookup_basename`,
      `shared_prefix_len`, `symbol_node_ids`, `simple_name`,
      `parse_heading_level`, `dependency_root`, `edge_for_call`,
      `resolve_symbol`, `pick_best`, `dedupe_edges`.  (src lines 1-1292)
- [x] `graph/algos.rs` — querying: `EdgeAdj`, `find_shortest_path`,
      `k_hop_bfs`, `build_di_graph`, `filter_edges_by_type`,
      `graph_keyword_search`, `Csr`, `BrandesScratch`, `calculate_centrality`,
      `detect_cycles`, `detect_cycles_dfs`, + the `Algorithm results` structs
      moved here in batch 6.  (src lines 1293-1979 + 1981-2028)
- [x] `graph/mod.rs` — module doc explaining the build/query split, the two
      `mod` lines, and `pub use` of both so `crate::graph::X` and the
      `lib.rs` re-export list keep resolving unchanged.
- [x] `build_graph` (the JSON String entry point) stays in `mod.rs`: it spans
      both halves (parse -> build -> serialize).
- Why: `cli/graph_algos.rs` has for a while had a clearer name than the thing
      it fronts. This makes the lib side match.
- Verify: `cargo check --all-targets`, `cargo nextest run --lib` AND
      `--test integration` (graph_test/traversal_test/centrality_test all
      import these paths).

### 11. `build_graph_from_index` (739 lines) → a `GraphBuilder` struct  — DONE
(Landed as `GraphAccum` + free pass functions rather than a builder with
methods: `for x in &self.index...` while `self.nodes.push(..)` is a borrow
conflict, whereas `fn pass(index: &IndexResult, acc: &mut GraphAccum)` with a
`let GraphAccum { nodes, edges, .. } = acc;` destructure at the top has none
AND lets every pass body stay byte-identical to the original.

VERIFIED FAITHFUL by diffing the extracted bodies against
`git show HEAD:native/src/graph.rs`: 458 significant statements before, 458
after, in the same order, zero differences except the intended `route_ids`
relocation. NOTE: a graph.json hash comparison does NOT work as the proof here
— this repo indexes itself, so adding the new functions changes the output for
reasons unrelated to behaviour. Diff the statements, not the artifact.

Two things the extraction surfaced that the 739-line version hid:
  - `route_ids` never leaves pass 3 (it is a dedup guard read via the bool
    that `HashSet::insert` returns), so it is a local now, not shared state.
  - `resolution` is touched ONLY by `resolve_call_edges` — call resolution is
    the one step that can be confidently right, guessing, or give up.)
The single biggest real function left. It is already written as five labelled
sequential passes, so the seams are self-documented:
- [x] `struct GraphBuilder` owning the seven accumulators that thread through
      every pass: `nodes`, `edges`, `symbol_id_map`, `qualified`, `route_ids`,
      `constant_ids`, `resolution`.
- [x] one method per pass, renamed to say what it does rather than what order
      it runs in — the current labels are `Pass -1 / 0 / 1 / 2 / 3`, and a
      pass numbered -1 is the tell that they were prepended over time:
      - `Pass -1` -> `add_dependency_nodes`      (~16 lines)
      - `Pass 0`  -> `add_folder_nodes`          (~65)
      - `Pass 1`  -> `add_file_and_symbol_nodes` (~313)
      - `Pass 2`  -> `resolve_call_edges`        (~222)
      - `Pass 3`  -> `resolve_import_edges`      (~110)
- [x] `build_graph_from_index` becomes the ~10-line sequence of those calls,
      so the pipeline is readable at a glance and each pass is unit-testable.
- [x] keep pass ORDER and semantics byte-identical — pass 2 depends on the
      `symbol_id_map` pass 1 fills, pass 3 on the file index. This is a pure
      extraction; any behaviour change is a bug.
- Verify: as batch 10. `graph_test`/`indexer_test` cover the built graph;
      a diff of `ug gen` output before/after is the real proof.

### 12. De-duplicate the chat tool dispatcher  — DONE
(`chat::run_chat_tool` is now the single dispatcher; `cli/chat.rs`'s closure
and `serve/chat_api.rs::run_chat_tool` both delegate, supplying only their own
handles. Put in chat.rs beside `run_search_tool` and `run_tool_rounds`, which
is where the rest of the chat tool machinery already lives.
DRIFT FIXED: the CLI path now enforces `CHAT_TOOL_DENYLIST` too.)
`cli/chat.rs` (the closure at ~296) and `serve/chat_api.rs::run_chat_tool` are
structurally identical: `normalize_args` -> guard -> match
`search`/`semantic_search` -> `analyze` -> default arm doing
`reject_if_store_backed` + `IndexedSource::load` + `run_tool` + `ToolOutput`
unwrap. ~45 lines each, differing ONLY in where graph/store/repo_root come from.
- [x] They have already drifted: `serve/chat_api.rs:871` enforces
      `CHAT_TOOL_DENYLIST`, `cli/chat.rs` does not. Impact is a worse error
      message, not a hole — `run_tool`'s fallback arm rejects unknown names
      with "Expected one of: ...", and the denylist also filters
      `openai_tool_schemas()` so denied tools are never advertised. Fixing the
      drift is the point; the duplication is what caused it.
- [x] extract one dispatcher parameterized over its state source (graph +
      store + embedder + repo_root + graph_path), and have both call sites use
      it. Put it beside `chat::run_search_tool`, which both already share.
- Verify: as batch 10, plus `cargo nextest run --lib` covers
      `mcp::tools` denylist tests.

## Gate — batches 9-12  — PASSED 2026-08-23
- [x] `cargo check --all-targets` — clean, no warnings
- [x] `cargo nextest run --lib` — 607/607
- [x] `cargo nextest run --test integration` — 280/280 (+14 ignored)
- [x] `cargo clippy --all-targets` — 72 diagnostics, DOWN from the 106 baseline.
      The split briefly added 7 `needless_borrow` (the destructure makes
      `symbol_id_map` a `&mut`, so `&symbol_id_map` is a `&&mut`); fixed.
      Only remaining lint under graph/ is a pre-existing `too_many_arguments`
      on `graph_keyword_search`.
- [x] `ug help`, `ug api`, `ug gen -n ug` smoke — all fine
- [x] index refreshed; `GraphAccum`, `add_file_and_symbol_nodes` and both
      `run_chat_tool`s resolve
- [x] AGENTS.md / tracking-doc path references still correct

### Scope note — `cargo clippy --fix` ran wider than these batches
Fixing the 7 `needless_borrow` was done with `cargo clippy --fix --lib`, which
also applied safe mechanical fixes in 10 files OUTSIDE batches 9-12:
cli/{doctor,projects,scope,upgrade}.rs, indexer/{common,languages/python}.rs,
serve/{api,host_guard,nidx}.rs, storage/query.rs — 13 insertions, 15 deletions.
All semantics-preserving (`% n == 0` -> `is_multiple_of`, `.get(0)` -> `.first()`,
`.last()` -> `.next_back()`, closure -> fn reference, format-literal inlining).
The two that touched user-visible format strings (doctor.rs, projects.rs) were
checked by hand and produce byte-identical output. Revert them if these batches
need to stay surgical; they are unrelated to the refactor.
      `rtk proxy` or the hook hands you an empty list)
- [ ] `./native/target/debug/ug help` + `ug gen -n ug` smoke
- [ ] AGENTS.md / tracking-doc path references still correct

## Surveyed and deliberately NOT doing
- **The four language indexers** (java/typescript/rust/python, ~6k lines).
  Expected copy-paste; found the opposite. `indexer/languages.rs` defines a
  tight `LanguageIndexer` trait, stateless `&'static dyn` singletons, and a
  5-step "adding a new language" recipe in the module doc. The bulk is
  irreducible tree-sitter extraction. Leave it alone.
  (Only nit: `for_extension` is a hand-maintained if-else chain the module doc
  tells you to edit; it could iterate a slice. Not worth a batch.)
- **Transport param-mapping duplication.** Already consolidated — everything
  funnels through `run_tool`. Only `cli/agent.rs` + `cli/search.rs` build typed
  params directly, and that is the CLI flag-parsing path, which legitimately
  differs from a JSON transport.
