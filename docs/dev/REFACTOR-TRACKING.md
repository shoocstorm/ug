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

## Final gate (all batches)
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
