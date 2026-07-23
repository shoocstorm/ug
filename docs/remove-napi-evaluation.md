# Evaluation: Remove the `ug.node` NAPI Layer

## Current Architecture

```
CLI (ug <tool>)     ──→  Rust main.rs ──→ directly calls Rust impls
HTTP (ug serve)     ──→  Rust axum     ──→ directly calls Rust impls
MCP (ug mcp)        ──→  run_mcp() spawns ──→ node cli.mjs mcp ──→ ug.<napi_fn>() ──→ Rust NAPI bridge
```

The NAPI layer is a one-way bridge: **Rust has no dependency on NAPI/Node**, but the MCP server is entirely Node.js and depends on NAPI to talk to Rust. The Rust binary (`ug`) with `ug serve` already serves all the same tools via HTTP without touching NAPI.

---

## What the NAPI Layer Covers

| Component | Lines | File | What it does |
|---|---|---|---|
| `agent_tools_napi.rs` | 146 | `native/src/agent_tools_napi.rs` | Caches graph.json, calls `agent_tools::run_tool()`, serializes result |
| `storage/napi_bindings.rs` | 484 | `native/src/storage/napi_bindings.rs` | Opens store, runs queries (5 async fns: db_ingest, db_hybrid_search, db_semantic_search, db_traverse, ping_embedder) |
| `graph.rs` NAPI exports | ~50 | `native/src/graph.rs` | Thin sync wrappers: build_graph, k_hop_bfs, filter_edges_by_type, graph_keyword_search, find_shortest_path, calculate_centrality, detect_cycles |
| `indexer.rs` NAPI exports | ~20 | `native/src/indexer.rs` | Thin sync wrappers: index, index_with_cache |
| napi deps + build | — | `native/Cargo.toml` | `napi`, `napi-derive`, `napi-build` (3 crates) |
| napi-rs CLI | — | `native/package.json` | `@napi-rs/cli` devDependency |

### Total NAPI surface: **16 functions** (11 sync, 5 async)

---

## What `cli.mjs` Does *Beyond* Calling NAPI

| Module | Lines | Difficulty to port | Notes |
|---|---|---|---|
| MCP stdio server (protocol) | ~50 | **Easy** | Uses `@modelcontextprotocol/sdk` — 2 request handlers (ListTools, CallTool) + StdioServerTransport. Trivial JSON-RPC over stdio. |
| MCP tool definitions (JSON Schema) | ~320 | **Easy** | 12 tool schemas with descriptions. Can derive from Rust structs via `schemars`. |
| `callTool` dispatch | ~70 | **Easy** | Maps tool names → Rust function calls. Already mirrors `agent_tools::run_tool()`. |
| `formatRankedContext` + `formatSemanticHits` | ~175 | **Medium** | Markdown rendering of search results with drill-down hints. Pure string formatting. |
| `stalenessNote` + staleness checking | ~50 | **Easy** | File mtime comparison — reads graph.json, stats each indexed file, compares timestamps. Trivial in Rust. |
| `projectCtx` / project resolution | ~180 | **Medium** | Resolves UG_PROJECT env → ~/.ug dir → graph.json/ugdb paths. Most of this already exists in `native/src/project.rs` — just need to add the env-var driven resolution. |
| `regenerateProject` (reindex) | ~30 | **Easy** | Calls index → buildGraph → dbIngest sequentially. All already exist as Rust fns. |
| `listProjects` + `formatProjectList` | ~40 | **Easy** | Already exists in `native/src/project.rs::list_projects()`. |
| `loadDotEnv` (.env loader) | ~20 | **Easy** | Already handled by `dotenvy::dotenv()` in Rust main.rs. |
| CLI command wrappers (index, graph, etc.) | ~300 | **Easy** | Already exist as Rust subcommands (`ug index`, `ug graph`, etc.). |
| MCP install/uninstall (9 clients) | ~350 | **Hard** | Config file manipulation for 9 different MCP clients across 3 formats (JSON, TOML, YAML). Includes interactive prompts. |
| Agent skill file install | ~70 | **Medium** | Copies SKILL.md to various agent rule directories. Straightforward file copy. |

---

## What Gets Removed

### Rust side
| File | Purpose |
|---|---|
| `native/Cargo.toml`: napi, napi-derive, napi-build | 3 dependencies |
| `native/Cargo.toml`: `crate-type = ["cdylib", "rlib"]` → `["rlib"]` | Remove cdylib |
| `native/build.rs`: `napi_build::setup()` | NAPI build setup call |
| `native/src/lib.rs`: `pub mod agent_tools_napi;` | NAPI-only module |
| `native/src/agent_tools_napi.rs` | 146 lines of NAPI bridge |
| `native/src/storage/napi_bindings.rs` | 484 lines of NAPI bridge |
| NAPI exports in `native/src/graph.rs` | 7 functions |
| NAPI exports in `native/src/indexer.rs` | 2 functions |

### Node.js side
| File | Purpose |
|---|---|
| `node/cli.mjs` | 2151 lines — the entire Node.js CLI + MCP server |
| `node/test-runner.cjs` | 799 lines — tests that require `ug.node` |
| `node/package.json` | Dependencies: @modelcontextprotocol/sdk, chalk, zod, yaml |
| `node/ug-mcp-skill/SKILL.md` | Agent skill file |
| `native/package.json` | @napi-rs/cli build dependency |
| `native/scripts/copy-bin.mjs` | Binary copy script (replaced by simpler `cargo build --release`) |

### Root
| File | Purpose |
|---|---|
| `package.json` | npm scripts, chalk/zod/sdk dependencies |
| `scripts/copy-wrappers.mjs` | esbuild bundler for cli.mjs |
| `scripts/release.sh` | Version bump script (simplified — one less manifest) |

### Build pipeline (before → after)

```
Before:
  cargo build --release                → libultragraph.dylib, ug, ug-app
  napi build --release --output-dir .ug → ug.node (from cdylib)
  node scripts/copy-bin.mjs release     → copies ug and ug-app to .ug/
  node scripts/copy-wrappers.mjs        → esbuild bundles cli.mjs → .ug/cli.mjs
                                         → copies ug-mcp-skill → .ug/ug-mcp-skill

After:
  cargo build --release                → ug, ug-app
  (optional: simple cp to .ug/)
```

---

## Approach: Full Rust MCP Server

### What needs to be written

#### 1. MCP stdio server module (~200 lines)
**File: `native/src/mcp.rs`** (new module)

A raw stdio JSON-RPC server implementing the MCP protocol. No SDK needed — the protocol is well-defined:

- Read `Content-Length: N\r\n\r\n` + JSON body from stdin
- Dispatch `{"jsonrpc":"2.0","method":"tools/list",...}` and `{"jsonrpc":"2.0","method":"tools/call",...}`
- Write `Content-Length: N\r\n\r\n` + JSON response to stdout
- Log diagnostics (connect/disconnect/errors) via stderr

Key functions:
```rust
pub fn run_mcp_server(args: &[String])
```

The MCP spec is simple enough to implement directly — the SDK overhead is unnecessary when you control both ends.

#### 2. MCP tool schema definitions (~150 lines)
**Add to `native/src/mcp.rs`** or create `native/src/mcp/tools.rs`

Define each tool's JSON Schema. Use `schemars::schema_for` on the existing parameter structs from `agent_tools.rs`, or define them manually (they're relatively stable).

The 12 tools are:
- `search` — SearchKbOptions (from storage/query.rs — mimics CLI's hybrid search)
- `semantic_search` — query, k, whereClause
- `find_symbols` — FindSymbolsParams (from agent_tools.rs)
- `file_outline` — FileOutlineParams (from agent_tools.rs)
- `get_code` — GetCodeParams (from agent_tools.rs)
- `find_usages` — FindUsagesParams (from agent_tools.rs)
- `traverse` — TraverseParams (from agent_tools.rs)
- `shortest_path` — ShortestPathParams (from agent_tools.rs)
- `project_overview` — no params
- `graph_schema` — no params
- `list_projects` — no params
- `reindex` — no params

Each schema includes descriptions, types, defaults, and which params are required (matching the existing zod schemas in `cli.mjs` lines 416-722).

Optionally hidden: `ping_embedder` (for operator diagnostics).

#### 3. Tool dispatch + call implementations (~200 lines)
**Add to `native/src/mcp.rs`** or `native/src/mcp/dispatch.rs`

```rust
async fn handle_call(tool: &str, params: serde_json::Value, state: &McpState) -> Result<String, String>
```

For each tool:
- **Graph-backed tools** (`find_symbols`, `file_outline`, `get_code`, `find_usages`, `traverse`, `shortest_path`, `project_overview`, `graph_schema`): delegate to `agent_tools::run_tool()`. Load graph from `state.graph_path`, pass graph/raw/repo_root. Already fully implemented.
- **DB-backed tools** (`search`, `semantic_search`): open store from `state.db_path`, build embedder from config, call `storage::query::search_kb()` / `storage::query::semantic_search()`. Already fully implemented — just need to strip the NAPI error wrapper.
- **`list_projects`**: call `crate::project::list_projects()`, format output.
- **`reindex`**: call index → buildGraph → ingest sequentially. All exist as library functions.
- **`ping_embedder`**: build embedder, call `embedder.ping()`.

#### 4. State management + project resolution (~80 lines)
**Add to `native/src/mcp.rs`**

```rust
struct McpState {
    db_path: PathBuf,
    repo_root: PathBuf,
    graph_path: PathBuf,
    embedder_config: EmbedderConfig,
    dest_config: DestConfig,
}

fn resolve_mcp_state() -> McpState
```

Resolution order (mirroring `cli.mjs` `resolveDbAndRoot()` + `projectCtx()`):
1. `UG_PROJECT` env var → `~/.ug/<name>/ugdb` + `project.json.repoRoot`
2. CWD-derived project → `~/.ug/<cwd-basename>/ugdb` if it exists
3. `./ugdb` fallback (legacy)
- `UG_REPO_ROOT` env var overrides `repoRoot` from project.json
- Embedder config: `UG_EMBED_BASE_URL` / `UG_EMBED_API_KEY` / `UG_EMBED_MODEL` env vars → `ug config get embed.*` → defaults
- Destination config: `UG_DEST` / `UG_NEO4J_URI` / `UG_NEO4J_USER` / `UG_NEO4J_PASSWORD` / `UG_NEO4J_DATABASE` env vars (already handled by `serve.rs`)

#### 5. Staleness checking (~60 lines)
**Add to `native/src/mcp.rs`**

```rust
fn index_staleness(db_path: &Path) -> Option<StalenessInfo>
fn staleness_note(db_path: &Path) -> String
```

Logic (matching `cli.mjs` lines 929-972):
1. Load `graph.json` from `dirname(db_path)/`
2. Read all node `.file` values (skip Folder nodes)
3. `stat()` each file relative to `repo_root`, compare mtime to graph.json's mtime
4. Return count of changed + missing files
5. Append warning note to tool outputs when stale

#### 6. Output formatting for DB-backed tools (~120 lines)
**Add to `native/src/mcp/formatters.rs`** or reuse from existing code

`format_ranked_context(ctx: &RankedContext) -> String`:
- Header with query, item count, seed id, types
- Per-item: type + name, id, location, hop/distance, description, snippet (truncated)
- Drill-down hints footer

`format_semantic_hits(query: &str, hits: &[SemanticHit]) -> String`:
- Simpler list format with name, id, distance
- "Next: search / traverse" hint

These formatters currently live in `cli.mjs` lines 770-861. Port to Rust string formatting.

#### 7. MCP install/uninstall (~350 lines)
**File: `native/src/mcp_install.rs`** (new module)

```rust
pub fn run_mcp_install(target: &str, scope: Option<&str>)
pub fn run_mcp_uninstall(target: &str, scope: Option<&str>)
pub fn list_targets()
```

For each of the 9 MCP client targets, write/remove the `ultragraph` server entry in the appropriate config file:

| Target | Format | Config locations |
|---|---|---|
| `claude` | JSON | `.mcp.json` (project), `~/.claude.json` (global) |
| `claude-desk` | JSON | `~/Library/Application Support/Claude/claude_desktop_config.json` (Darwin), `%APPDATA%/Claude/claude_desktop_config.json` (Windows), `~/.config/Claude/claude_desktop_config.json` (Linux) |
| `cursor` | JSON | `~/.cursor/mcp.json` (global), also project? |
| `windsurf` | JSON | `~/.codeium/windsurf/mcp_config.json` |
| `vscode` | JSON | `.vscode/mcp.json` (project), `~/Library/Application Support/Code/User/mcp.json` (global/Darwin) |
| `gemini` | JSON | `.gemini/settings.json` (project), `~/.gemini/settings.json` (global) |
| `codex` | TOML | `~/.codex/config.toml` |
| `hermes` | YAML | `~/.hermes/config.yaml` |
| `opencode` | JSON | `opencode.json` (project), `~/.config/opencode/opencode.json` (global) |

**Required Rust crates:** `serde_json` (already present), `toml` (new), `serde_yaml` (new). Optional: `inquire` for interactive prompts.

The generated server entry is always: `{ command: "ug" or "/path/to/ug", args: ["mcp"], env: { UG_PROJECT: "<project-name>" } }`.

#### 8. Remove NAPI + cdylib from build
**Files to modify:**

`native/Cargo.toml`:
```diff
 [lib]
-crate-type = ["cdylib", "rlib"]
+crate-type = ["rlib"]

 [dependencies]
-napi = { version = "3", features = ["napi6", "tokio_rt"] }
-napi-build = "1"
-napi-derive = "3"
+# these deps removed

 [build-dependencies]
-napi-build = "1"
 tauri-build = "2"
```

`native/build.rs`:
```diff
 fn main() {
-    napi_build::setup();
     tauri_build::build();
 }
```

`native/src/lib.rs`:
```diff
-pub mod agent_tools_napi;
```

#### 9. Remove Node.js files and build scripts
Files to delete:
- `node/cli.mjs` (entire directory is optional — verify nothing else depends on it)
- `node/test-runner.cjs`
- `node/package.json`
- `node/ug-mcp-skill/`
- `native/package.json`
- `native/scripts/copy-bin.mjs`
- `scripts/copy-wrappers.mjs`
- `scripts/release.sh` (simplify to not touch npm manifests)

Root `package.json`: keep if needed for other tooling, or remove if `ug` is the only interface.

#### 10. Update `run_mcp` in `main.rs`
```rust
// Before (lines 2910-2960): finds cli.mjs next to binary, spawns node
fn run_mcp(args: &[String]) {
    // ... 50 lines of Node.js process spawning
}

// After: directly enter the MCP server loop (or dispatch to install subcommands)
fn run_mcp(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("install") => mcp_install::run_mcp_install(args.get(1).copied(), ...),
        Some("uninstall") => mcp_install::run_mcp_uninstall(args.get(1).copied(), ...),
        Some("list" | "ls") => mcp_install::list_targets(),
        _ => runtime.block_on(mcp::run_mcp_server(args)),
    }
}
```

#### 11. Tests
- Port `node/test-runner.cjs` tests to Rust integration tests in `native/tests/`
- The NAPI-based tests (calling `ug.index()`, `ug.buildGraph()`, etc. from Node.js) should become Rust tests calling the same library functions directly
- Most test scenarios already have equivalents in `native/src/agent_tools.rs::tests`

---

## Files to Create (New)

| File | Est. lines | Purpose |
|---|---|---|
| `native/src/mcp.rs` | ~400 | MCP stdio server + state + dispatch + staleness + formatting |
| `native/src/mcp_install.rs` | ~400 | MCP install/uninstall for 9 clients |
| `native/tests/mcp_test.rs` | ~200 | MCP server tests |

**Total new Rust:** ~1000 lines

## Files to Modify

| File | Change |
|---|---|
| `native/Cargo.toml` | Remove napi deps, change crate-type, add `toml` + `serde_yaml` + optional `inquire` |
| `native/build.rs` | Remove `napi_build::setup()` |
| `native/src/lib.rs` | Remove `pub mod agent_tools_napi` |
| `native/src/main.rs` | Rewrite `run_mcp()` to not spawn node |
| `native/src/storage/napi_bindings.rs` | Delete entire file |
| `native/src/graph.rs` | Remove #[napi] annotations (keep the functions as library fns) |
| `native/src/indexer.rs` | Remove #[napi] annotations (keep the functions as library fns) |

## Files to Delete

| File | Lines |
|---|---|
| `node/cli.mjs` | 2151 |
| `node/test-runner.cjs` | 799 |
| `node/package.json` | ~15 |
| `node/ug-mcp-skill/SKILL.md` | ~80 |
| `native/package.json` | 25 |
| `native/scripts/copy-bin.mjs` | 40 |
| `native/src/agent_tools_napi.rs` | 146 |
| `native/src/storage/napi_bindings.rs` | 484 |
| `scripts/copy-wrappers.mjs` | 38 |
| `scripts/release.sh` | 149 |
| `package.json` (root, optional) | 38 |

**Total deleted:** ~4000 lines

---

## Impact Summary

| Dimension | Assessment |
|---|---|
| **Feasibility** | **High** — all computation already lives in Rust. The NAPI wrappers are pure serialization bridges. |
| **Risk** | **Low** — no algorithm changes, only transport replacement. |
| **Effort** | ~**1000–1200 lines** new Rust, **~4000 lines** deleted, **~10 deps** removed |
| **Build complexity** | **↓↓ Reduced** — one `cargo build`, no npm, no napi-rs, no esbuild, no copy scripts |
| **Binary size** | **↓ Smaller** — no bundled JS runtime or esbuild output |
| **Install complexity** | **↓↓ Greatly reduced** — no Node.js requirement, single binary |
| **Startup time** | **↓ Faster** — no Node.js process spawn, no V8 init |
| **Maintenance** | **↓ Simpler** — all code in one language, one build system, no version drift |
| **User-facing change** | **None** — `ug mcp` still works identically. `ug mcp install` still works. |
| **Breaking change** | Anyone importing `ug.node` directly from JS (e.g. `require('../.ug/ug.node')`). Check if downstream consumers do this. |

---

## Implementation Order

1. **Prep:** Add `mcp.rs` module to `native/src/lib.rs`. Add `toml` and `serde_yaml` to Cargo.toml deps.
2. **MCP server core:** Implement `mcp.rs` with stdio JSON-RPC, project resolution, state management, tool dispatch.
3. **Tool dispatch:** Wire up graph-backed tools (call `agent_tools::run_tool()`), DB-backed tools (call `storage::query` fns), list_projects, reindex, ping_embedder.
4. **Formatters:** Port `formatRankedContext` and `formatSemanticHits` to Rust.
5. **Staleness:** Implement staleness checking + warning note.
6. **MCP install/uninstall:** Implement `mcp_install.rs` for all 9 clients.
7. **Rewrite `run_mcp`:** Change `main.rs` to call native MCP server instead of spawning node.
8. **Remove NAPI:** Strip napi deps from Cargo.toml, remove cdylib, remove napi_build from build.rs, remove `agent_tools_napi.rs` and `napi_bindings.rs`.
9. **Clean up:** Delete node/ files, scripts, native/package.json.
10. **Tests:** Port NAPI-dependent tests to Rust integration tests. Run `cargo test`.
11. **Verify:** `cargo build --release && ./target/release/ug mcp` should start the server without Node.js.
