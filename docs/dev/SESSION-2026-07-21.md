# Session summary — 2026-07-21

Branch `streamline-agent-surfaces`, 11 commits on top of `629cccb`.
22 files, +3952 / −1743.

Goal: make the CLI, HTTP API and MCP tools present **one** set of names,
params and behaviour to an AI agent — then fix what that exposed.

---

## 1. One implementation behind three transports

The seven graph-backed tools existed **twice**: Rust for the CLI, JavaScript
for MCP. `ug mcp call` carried a *third* copy of the whole dispatch. Three
implementations of the same lookups, free to drift — and drifting.

**`native/src/agent_tools.rs`** is now the single implementation. Each tool
takes a typed params struct and returns a typed result that both serializes
to JSON and renders to text through a `Render` enum (`Ansi` for terminals,
`Markdown` for MCP). Layout is shared; only emphasis markers differ, so the
surfaces agree by construction.

`agent_tools::run_tool` is the single dispatch. The napi bridge (MCP) and
`POST /api/tools/:name` both call it, so a new tool reaches all three
surfaces at once and none can be forgotten.

| | before | after |
|---|---|---|
| `native/src/main.rs` | — | −862 lines |
| `node/cli.mjs` | — | −948 lines |
| JS tool implementations | 12 functions | 0 |

Verified byte-identical payloads across HTTP == CLI == MCP.

## 2. Canonical names

| Now | Was |
|---|---|
| `search` | `search_kb` (MCP) / `hybrid_search` (CLI) |
| `semantic_search` | `semantic_search_kb` |
| `traverse` | `traverse_kb` |
| `shortest_path` | `graph_path` (CLI) |
| `list_projects` | `list` (CLI) |
| `find_symbols` | `find_symbol` |

The `_kb` suffix is gone. `tools/list` advertises only canonical names;
retained aliases (`search_kb`, `hybrid_search`, `graph_path`, `path`, `list`,
`find_symbol`, `graph_search`) still dispatch. Params accept both canonical
snake_case and legacy camelCase, and both scalar and array shapes.

## 3. Filled gaps

- **HTTP had no agent tools at all.** Added `GET /api/tools` (discovery,
  the HTTP equivalent of MCP `tools/list`) and `POST /api/tools/:name`.
- **Per-request project scoping** via an optional `project` field. It loads
  on demand but deliberately does *not* change the server's active project —
  a read must not reconfigure the UI for other clients.
- **`--json` on every agent tool** (previously only `graph_*` had it).
- **Call sites in `find_usages`** were MCP-only; ported to Rust, so the CLI
  gained them too. A per-file line cache means several callers in one file
  cost one read, where the JS version re-read per caller.

## 4. Merges and removals

**`graph_search` merged into `find_symbols`.** They were the same substring
scan over the same `graph.json`; the only difference was docstring matching.
Measured: `graph_search embedder` → 43, `find_symbol embedder` → 36,
`graph_search --names-only` → exactly the same 36. That difference is now
`include_docs` / `includeDocs`, and `ug graph_search` is an alias that turns
it on. Same counts before and after; docstring hits rank below every name hit.

> Breaking: `ug graph_search --json` now emits the `find_symbols` envelope
> (`{queries:[{items:[…]}]}`) instead of `{count, nodes:[…]}`.

**`ping_embedder` removed from `tools/list`** — an operator diagnostic, not
worth an agent's tool call, since `search` already surfaces embedding errors.
Still reachable via `ug doctor` and `ug mcp call ping_embedder`.

**`search`'s ranking knobs demoted**: 14 advertised params → 8. Dropped
`strategy`, `hops`, `mmrLambda`, `pprRestartProb`, `pprMaxIter`,
`pprSeedPool`, `pprEdgeWeights`. They still parse for operator debugging but
no longer cost description tokens on every `tools/list`.

> **MMR was not deleted.** `search_kb` falls back to it automatically when a
> store lacks native PPR (`query.rs:387`) — Neo4j without the GDS plugin.
> Removing the implementation would have broken that backend. Only the
> user-facing *choice* is gone.

## 5. `traverse` moved to graph.json

It was the last agent tool requiring a database, which made no sense beside
`find_usages` — inverse walks over the same edges, yet one hit OverGraph and
the other read `graph.json`. Ingest copies those edges into the store, so
both answer identically. A test asserts the two agree so they can't drift.

The store path stays: `ug traverse --dest <name>` and
`GET /api/db/traverse/:id?dest=` still query the destination, which is how
`docs/MULTI-DEST.md` says to verify what landed in OverGraph or Neo4j. Only
the default changed.

Payoff: on a `--no-db` server, `POST /api/tools/traverse` returns results
while `GET /api/db/traverse` returns 503.

## 6. Storage split, documented

Only **`search`, `semantic_search` and `chat`** need the database. Every
other tool reads `graph.json` alone, so they survive `ug gen --no-ingest` and
an unreachable embedder. Now a README table and a recovery hint in
`SKILL.md`.

---

## Bugs found and fixed

**Index cache never hit.** A cache hit needs a matching hash in `cache.json`
*and* the previous run's `FileNode` from `indexed-tree.json`.
`index_with_cache` wrote the first but never the second, so `prev_files` was
always empty and every "hit" re-parsed. The Rust tests passed only because
their helper wrote `indexed-tree.json` into the cache dir by hand, calling it
"the caller contract" — which the real callers don't honour. `ug index -i .
--cache .cache -o tree.json`, the example in `native/README.md`, could never
cache. Only `cli.mjs regenerateProject` worked, by accident: its cache dir and
output dir are the same path. The cache directory is now self-contained.

**`pdf_indexer_test`, 9/11 → 12/12.** Two failures, both fallout from the
`pdf-extract` → `liteparse`/PDFium migration the test file predates:
1. It expected `pdf_page:1`; the code emits `doc_page:1` — a deliberate
   rename, since the same indexer handles Word/Excel/PowerPoint.
2. `unicode.pdf` draws emoji through an embedded font with no usable Unicode
   mapping. PDFium extracts nothing (verified: `name="Page 1 (no text)"`).
   Split into two tests: a new `latin1.pdf` (base-14 Helvetica +
   WinAnsiEncoding, generated by `scripts/make-latin1-pdf.mjs`) covers the
   multi-byte round-trip, and `unicode.pdf` now pins graceful degradation —
   previously untested, and the common case for scanned PDFs.

**`max_nesting` was always 0.** `calculate_nesting` counted *declaration*
node kinds (`function_declaration`, …), but Rust's nodes are `function_item`
/ `struct_item`, absent from that list. It also measured the wrong thing —
how deeply the declaration sat, not the control-flow depth of its body.
Rewritten to walk control flow; now 392/788 symbols report non-zero nesting,
max 8. Regression test pins flat=0, one loop=1, `for>if>match`=3.

**Renderer markup leaks.** A test asserting no renderer emits the other
surface's markup caught two hardcoded backticks in error strings.

**`startNodeIds` rejected.** The zod transform emitted both it and `nodeId`,
which the Rust params struct rejected as a duplicate field.

---

## Stats made visible

Three datasets were written into `graph.json` and read by **nothing**:

- `stats` (`IndexStats`) — files, cached files, symbols, folders, lines,
  indexing duration, last-indexed timestamp.
- The repo-root folder node's `languageBreakdown` and `classification` —
  per-language file counts and the code/docs/mixed KB type.
- `metrics` (loc, params, max_nesting) on 785 symbol nodes.

No schema change was needed; they just had no reader. `project_overview` now
reports all three — so CLI, MCP and HTTP get them at once — and
`/api/graph/stats` gained `index`, `languages` and `kb_type` for the UI.

```
kind: mixed knowledge base

Index
- 59 file(s), 1225 symbol(s), 13 folder(s), 27719 line(s)
- built in 557 ms

Languages (56 files)
  rust×46, markdown×10

Largest symbols (lines of code · params · max nesting)
- Function build_graph_from_index  348 loc · 1p · depth 5  native/src/graph.rs:34-382
```

---

## Final state

- **12 MCP tools**, 8 of them `graph.json`-only.
- **Rust: 159 passed, 0 failed** — the whole suite is green for the first
  time this session.
- **JS: 26/26.**

## Not done

- **`capabilities.rs` registry.** `ug api`'s endpoint table and cli.mjs's
  ~350-line `MCP_TOOLS` array remain separate sources of truth for tool
  descriptions. The dispatch is unified, so this is documentation-drift risk
  only — but a single Rust table generating `ug help`, `ug api --json`,
  `tools/list` and `GET /api/tools` would close it.
- **`/api/graph/search`** still duplicates the `find_symbols` name+docstring
  scan. Left alone because the visualization UI consumes it and returns raw
  `GraphNode`s.
- **MMR deletion.** Blocked on whether Neo4j-without-GDS is a supported
  configuration. If not, `search_kb_mmr` + `mmr_rerank` (~60 lines) can go.
