# UltraGraph (ug) — Complete API & Architecture Reference

## 1. CLI Commands

`ug` uses a flat subcommand model (no clap derive — manual arg parsing). All commands listed below, with their flags and data dependencies.

### Legend

- **index.json** — `indexed-tree.json`, the output of `ug index` (list of `FileNode`s with parsed symbols)
- **graph.json** — the output of `ug graph` (list of `GraphNode`s + `GraphEdge`s)
- **ugdb/** — OverGraph embedded database directory (vector + adjacency store)
- **Neo4j** — remote Neo4j database (alternative backend)
- **config** — `~/.ug/config.json` (persisted preferences)
- **~/.ug/** — project home directory (`$UG_HOME`, defaults to `~/.ug`)
- **cache** — hash-based parse cache (`cache.json` + `indexed-tree.json` snapshot)

---

### 1.1 Pipeline Commands

| Command | Aliases | What it does | Data sources | Key flags |
|---------|---------|-------------|--------------|-----------|
| `ug gen` | — | **End-to-end pipeline**: index → graph → visualization → OverGraph ingest. The primary entry point. With no input path named, re-runs the resolved project (`-n` → active → cwd basename) from the repo root recorded in its `project.json` — incremental, unchanged files are skipped. | Repo source → index.json → graph.json → ugdb/ | `-i <path>` input dir, `-o <dir>` output, `-n <name>` project name, `-c <dir>` cache, `--no-cache`, `--no-ingest`, `--with-embed`, `--no-prune`, `--serve`, `-d <dir>` db path, `--model`, `--base-url`, `--api-key`, `--embedding-dim` |
| `ug index` | — | Index a directory: parse source files into `FileNode`s with symbols, imports, exports. Writes `indexed-tree.json`. | Repo source → writes index.json | `-i <path>` input (default `.`), `-o <file>` output, `-n <name>`, `-c <dir>` cache |
| `ug graph` | — | Build graph from indexed tree: resolve cross-file imports, create `GraphData` with nodes + edges. Writes `graph.json`. | index.json → writes graph.json | `-i <file>` input index.json, `-o <file>` output graph.json, `-n <name>` |
| `ug ingest` | — | Embed graph nodes and write to one or more knowledge stores (OverGraph/Neo4j). Defaults resolve from active project (`ug active <name>`), else cwd basename; reads `~/.ug/<name>/graph.json`, writes `~/.ug/<name>/ugdb`. | graph.json → writes to ugdb/ or Neo4j | `-n <name>` project name, `-i <file>` input graph.json, `-o <dir>` output, `--dest <kind>` (overgraph\|neo4j, comma-separated), `--neo4j-*`, `--prune`, `--model`, `--base-url`, `--api-key`, `--embedding-dim` |
| `ug demo` | — | **Publish** a repo as a static web demo: index → graph → a folder any static host can serve. Stops after `build_graph` — no db, no vectors, no project registered under `~/.ug`. Writes `graph.json` next to the visualization page wrapped in a static stand-in for the server (`native/src/vis/demo-shim.js`), so the page works with no backend; semantic search, chat, tours, statistics and source preview are off and say so in the UI. Local absolute paths (repo root, home directory) are scrubbed out of the published graph. Warns when the graph exceeds the renderer's 100,000-element limit, past which the page opens in solo mode. | Repo source → writes `<out>/graph.json`, `index.html`, `threejs-vis.bundle.js`, `favicon.svg`, `demo.json`, `README.md` | `-i <path>` input (default `.`), `-o <dir>` output (default `docs/ug-website/demo` when that folder exists, else `./ug-demo`), `--label <text>`, `--source-url <url>`, `--site <url>` (base for the "Install ug" links, default `/`), `--page-only` |
| `ug demo --page-only` | — | Rebuild an already-published demo's `index.html` from this binary and leave `graph.json` alone. **What to run after editing `native/src/vis/`** — the published page is a copy and goes stale silently. `demo.json` carries a `visFingerprint` (hash of the assembled page + shim) so the `the_published_demo_page_is_not_stale` test catches the drift; a full re-publish would rewrite 2.7 MB of `graph.json` on every CSS tweak, which is why this path exists. | `<out>/demo.json` → rewrites `index.html`, `demo.json`, `threejs-vis.bundle.js`, `favicon.svg`, `README.md` | `-o <dir>`, `--label`, `--site` |

#### Embedding is opt-in — `--with-embed` vs `--no-ingest`

`ug gen` and `ug update` build **no vectors** unless `--with-embed` asks for
them: nothing structural reads a vector, and loading the embedding model is most
of a run's wall clock. Do not confuse that with `--no-ingest`, which skips the
database altogether — the difference decides whether `ug analyze` (statistics,
`diff_impact`, blast radius) can be trusted after the run:

| `ug gen` / `ug update` | Written to the OverGraph db | Current after the run | Answering from the *previous* ingest |
|---|---|---|---|
| *(default)* | nodes, edges, facts and keyword/BM25 statistics — **everything except the vectors**. No embedding model is loaded, which is most of a small run's wall clock. | the graph.json tools **and `ug analyze`** — statistics, `diff_impact`, `boundary_impact`, blast radius, `traverse --dest` | `search`, `chat` — they miss the changed nodes until the vectors are backfilled |
| `--with-embed` | all of the above **plus the vectors**. Loads the embedding model (local fastembed, or a remote `--base-url` endpoint). | everything, semantic search included | — |
| `--no-ingest` | **nothing** — no nodes, no edges, no vectors; the db is never opened. Only `graph.json` is rebuilt. | the graph.json tools only: `find_symbols`, `file_outline`, `get_code`, `find_usages`, `shortest_path`, `project_overview`, `graph_schema` | **everything the db backs** — `ug analyze` statistics and blast radius as well as `search`, `chat` |

`ug ingest -n <project>` catches the db up whenever you like; it embeds only the
nodes still owed a vector. A run without `--with-embed` records the debt in
`project.json` (`pendingVectorsSince`), which is what `ug hook status` and the
`search` warning line report.

`--no-embed`, the former opt-out, is still accepted for compatibility — it is
the default now, and it wins if combined with `--with-embed`.

### 1.2 Graph Analysis Commands (graph.json-backed, offline, in-memory)

All accept: `-n <name>` (project), `-i <file>` (explicit graph.json), `--json` (raw JSON output), `-o <file>` (write JSON to file).

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug shortest_path` | — | Find shortest directed path between two symbols. | `<source>` `<target>` positionals, `--strict` (don't retry reverse direction) |
| `ug graph_centrality` | — | Rank nodes by degree & betweenness centrality. | `--top <n>` (default 20), `-t <type>` (repeatable), `-f <prefix>` |
| `ug graph_cycles` | — | Detect dependency cycles. | `-l <limit>` (default 20), `--min-len <n>`, `--max-len <n>`, `-f <prefix>`, `--fail-on-cycle` |

### 1.3 Agent Tools (graph.json-backed, same as MCP tools)

These accept the same params as their MCP counterparts and can output `--json`.

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug context` | — | **Everything about one symbol in one budgeted call**: its source, callers with call sites, tests reaching it, dependencies and linked docs — each labelled with the role that put it there. Replaces `get_code` + `find_usages` + `traverse` + `analyze test_for`. | `<symbol>` positional (must resolve to exactly one), `--max-chars <n>` (default 12000), `--include <role>` (repeatable: `target`, `caller`, `test`, `dependency`, `doc`), `-n <name>`, `--json` |
| `ug find_symbols` | — | Symbol lookup by name, fragment (ranked exact > prefix > substring) or **wildcard**. | `--node-type <type>` (repeatable, wildcards ok), `--file-prefix <prefix-or-glob>`, `--boundary`, `-k <limit>`, `--include-docs`, `-n <name>`, `--json`, `-o <file>` |
| `ug file_outline` | — | List indexed symbols in a file, in line order. Takes a path **glob**. | `<file-or-glob>` positional(s), `-k/--max-files <n>` (default 20), `--ids`, `-n <name>`, `--json` |
| `ug get_code` | — | Read source for a symbol (id, name or wildcard), or a file/line range. | `<symbol>...` or `-f <file>`, `-s/--start-line`, `-e/--end-line`, `-r/--range <window>` (`11-35` · `34-end` · `20`, same dialect as `ug analyze --range`), `--max-chars`, `--no-doc`, `-n <name>` |
| `ug find_usages` | — | Find inbound references (callers/importers) to a symbol. | `<symbol>...` positional(s), `-k/--hops`, `-t/--edge-type`, `-n <name>`, `--json` |
| `ug project_overview` | — | Orient in the codebase: stats, biggest files, most depended-upon symbols. | `-n <name>`, `--json` |
| `ug graph_schema` | — | Node & edge types with counts and connection info. | `-n <name>`, `--json` |

#### Wildcards

Every place a symbol or file is named — `find_symbols` names, node types and
file filters, `file_outline` paths, and the symbol arguments of `get_code`,
`find_usages`, `traverse` and `shortest_path` — accepts the same shell-style
pattern. One matcher (`native/src/pattern.rs`) serves the CLI, HTTP and MCP,
so the dialect is identical on all three.

| Syntax | Matches |
|--------|---------|
| `*` | any run of characters (not `/` in a path) |
| `**` | any run of characters, `/` included (paths) |
| `?` | exactly one character |
| `[abc]` `[a-z]` | one character from the set or range |
| `[!ab]` | one character not in the set (`[^ab]` too) |
| `{a,b}` | either alternative, nestable |
| `\*` | a literal `*` |

Matching is case-insensitive and covers the **whole** name: `auth*` finds
`authorize`, `*auth*` finds `reauth`. In paths, `*` stops at `/` and `**/`
crosses directories (and also matches zero directories, so `src/**/*.rs`
finds `src/main.rs`). Quote patterns in a shell.

```bash
ug find_symbols 'handle_*'                       # every handler
ug find_symbols '*Controller' --node-type Class
ug find_symbols '*' --file-prefix 'src/auth/**' -k 100
ug find_symbols --boundary                       # every entry and exit point
                                                 # (no name needed — it is a listing)
ug file_outline 'src/**/*.{ts,tsx}' -k 40        # survey a subtree
ug find_usages 'validate_*'                      # blast radius of a family
ug traverse 'handle_*' -d inbound
ug get_code 'render_*' --no-doc
```

The symbol arguments of `get_code`, `find_usages` and `traverse` also take a
plain **name** — `ug find_usages connect` works without looking the id up
first. A name or pattern expands to at most 25 symbols there; going over the
cap is reported in the output, never silent. `shortest_path` endpoints must
resolve to exactly one node, and list the candidates when they don't.

### 1.4 Retrieval Commands (ugdb/Neo4j-backed)

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug search` | `semantic_search` (retired; forwards with `--no-expand`) | **GraphRAG**: semantic + keyword seeds → graph expansion → PPR-ranked context. Returns lean ids+locations by default; add `--snippets` to read source slices inline. `--no-expand` skips the graph half and returns only what matched. | `<query>` positional, `-k <limit>` (default 8), `--filter <sql>`, `--no-expand`, `--direction`, `-t <edge-type>`, `--max-chars`, `--snippets` (opt in; off by default), `-n <name>`, `--repo-root`, embedding overrides |
| `ug traverse` | — | K-hop BFS over the OverGraph edges table. | `<node-id>`... positionals, `-k <hops>` (default 2), `-n <name>` |
| `ug analyze` | — | **Whole-repo statistics**: counts, groups, distributions, blast radius. Read-only GQL over the stored facts. Needs the db but **no embedder**. The `diff_impact`/`diff_retest_scope` presets take a list of changed files (`-a files=a.ts,b.rs`); `test_for` takes a symbol (`-a symbol=<id>`). | `<preset>` positional or `-p <preset>`, `-a k=v` (repeatable), `-g/--gql <query>`, `-k <limit>` (default 20), `-r/--range <window>` (`20` · `11-35` · `34-end`), `--json` (machine envelope matching `POST /api/tools/analyze`), `--list`, `-n <name>` |

These read commands also accept `--db <dir>` to point at an explicit
OverGraph directory (default: the `-n` project's `ugdb`, else the active
project's), and `-o <file>` to write the result JSON to a file instead of
stdout.

### 1.5 Chat & Tour Commands

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug chat` | — | RAG-grounded chat: hybrid retrieval → LLM completion. One-shot (with prompt) or interactive REPL (no prompt). | `"<query>"` optional, `-k <limit>`, `--direction`, `-t <edge-type>`, `--filter`, `--max-chars`, `--no-snippets`, `--think`, `--no-tools`, `--max-tool-rounds`, `--chat-model`, `--chat-base-url`, `--chat-api-key`, `--temperature`, `--max-tokens`, `--chat-timeout`, `--system`, `--json`, `-v/--show-context`, embedding overrides, `-o <file>` |
| `ug tour` | — | Guided, narrated walkthrough — uses LLM to plan stops through the graph, flies the camera in the web UI. | `"<topic>"` optional, `-n <name>`, `--max-stops <n>` (default 8, max 25), `--no-llm`, `--chat-model`, `--chat-base-url`, `--chat-api-key`, `--temperature`, `--max-tokens`, tour-specific flags |

### 1.6 Project Management

| Command | Aliases | What it does | Key flags |
|---------|---------|-------------|-----------|
| `ug serve` | (default) | Serve the visualization + REST API at `http://localhost:8080`. Runs by default with no args. | `-p <port>` (default 8080), `--host <ip>` (default 127.0.0.1), `--watch`, `--no-db`, `--graph-mode <auto|local|server>`, `-i <graph.json>`, `-d <db>`, `--project <name>`, `--repo-root` |
| `ug app` | — | Open the native desktop shell (starts the server + a window). | Same as `ug serve` |
| `ug api` | — | List every HTTP endpoint `ug serve` exposes. | `--json` |
| `ug list` | `ls`, `list_projects` | List generated projects under `~/.ug`: node/edge counts, size on disk, `STATUS` (`fresh` · `N changed` · `no db` · `repo gone` · `no graph`), last-updated time and repo root, plus per-project follow-ups naming the command that resolves each one. `STATUS` reports whatever blocks you soonest, from the same scan as `GET /api/projects/staleness`. | `--quick` (skip the size walk and staleness scan), `--json` |
| `ug active` | — | View or set the active project (default for `ug mcp`). | Sets with `<name>` positional |
| `ug rename` | `rn`, `mv` | Rename a project's data directory and its `project.json` name; the active marker follows it. One positional renames the current (active, else cwd) project; two rename `<old> <new>`. | `<new>` or `<old> <new>` positionals, `-n <old>` |
| `ug rm` | — | Delete a project's data directory. | `<name>` positional |
| `ug update` | — | Refresh the graph for the files that just changed (focused re-run): re-index, re-resolve cross-file edges, re-ingest changed nodes (no vectors unless `--with-embed`). Built for a live editing session. A named path that no longer exists but *is* in the index is treated as a deletion; one the index never held is still an error. | `<file>...` positionals, `-n <name>`, `--with-embed`, `--no-ingest`, plus `ug gen` embedder flags |
| `ug hook` | — | Install git hooks (`post-commit`, `post-merge`, `post-checkout`, `post-rewrite`) that run `ug update` (without `--with-embed`) on the paths each event touched, so the graph never lags the working tree. Appends to an existing hook of the same name behind `# >>> ug hook >>>` markers, honours `core.hooksPath` (husky/lefthook), never fails the git command, and logs each run to `~/.ug/<project>/hook.log`. That log's header records the run's whole provenance — which hook fired, timestamp, project, repo root, data dir, `ug` binary and version, the files handed over, and the child's exit code — so a failed refresh can be diagnosed without reconstructing it from shell history. `UG_HOOK_DISABLE=1` skips it for one command. Because hook runs skip embedding, `status` reports how long vectors have been owed — `ug ingest -n <project>` backfills them. | `install` / `uninstall` / `status` (default) subcommands, `-n <name>` |
| `ug upgrade` | — | Check GitHub for a new release and self-update. The downloaded archive is verified against the release's published `.sha256` before it is unpacked and executed. | `--check` (report only, no update), `--allow-unverified` (install when a release publishes no checksum) |
| `ug uninstall` | — | Delete ALL indexed projects and uninstall ug itself. | — |
| `ug config` | — | View/persist defaults (chat model, endpoints, etc.) in `~/.ug/config.json`. | `set <key> <value>`, `get <key>`, `list` |
| `ug doctor` | — | Show resolved project/db/embedder/chat config with source for each value. | `-n <name>`, `-d <db>`, embedder and chat overrides |
| `ug mcp` | — | MCP server / install / uninstall (see §1.7). | |

### 1.7 MCP Subcommands

| Command | What it does | Key flags |
|---------|-------------|-----------|
| `ug mcp` | Run the stdio JSON-RPC MCP server (meant to be launched by an AI agent). | — |
| `ug connect <agent>` (alias `ug mcp install`) | Connect an AI agent: the CLI skill (`--cli`, recommended), the MCP server (`--mcp`), or both (`--both`); asks when not given. Agents: `claude`, `claude-desk`, `cursor`, `windsurf`, `vscode`, `gemini`, `codex`, `opencode`, `zed`, ... | `--project` (scope to this repo), `--global` (everywhere), `--hooks` (also install the git hooks — same as `ug hook install`) |
| `ug disconnect <agent>` (alias `ug mcp uninstall`) | Remove the agent skill and the MCP server registration. | `--project`, `--global` |
| `ug mcp list` / `ls` | Print the tools this server advertises. | — |
| `ug mcp call <tool> <json>` | Invoke one tool directly from the command line. | `<tool>` name, `<json>` arguments |

### 1.8 Project Resolution & Active Project Fallback

Commands need to resolve a project name to read `graph.json` and/or write/load the OverGraph store (`ugdb`). The order varies by command group:

#### Resolution Order by Command Group

| Command Group | Resolution Order | Notes |
|--------------|------------------|------|
| `gen`, `index`, `graph` | `-n/--name` → derive from input path | Generate commands must use cwd when no `-n` and no project matches (they create a new project); `gen` with no `-i` first checks for an existing project (see below) |
| `gen` (no `-i/--input`) | `-n` → **active project** → cwd basename | Re-runs the resolved project from the repo root recorded in its `project.json`; honors user's pinned active project |
| `ingest` | `-n` → **active project** → cwd basename | Reads graph.json and writes ugdb from the active project by default |
| Read commands<br/>(`search`, `traverse`, `chat`, `tour`, `analyze`, `graph_centrality`, `graph_cycles`) | `-n` → **active project** → cwd basename → most-recent | Now consistent with `gen`/`ingest` |
| `server`, `app`, `mcp` | `-n` → **active project** → cwd | Always opens the active project |
| Any command with `-i/--input` | `-i` wins | Bypasses all project logic |

#### Helper Functions

- `resolve_project_name(args, input)` → `-n` → `derive_project_name(input)` — for `gen` (with an explicit path)/`index`/`graph` (no active fallback)
- `resolve_active_project_name(args, input)` → `-n` → `get_active_project()` → `derive_project_name(input)` — for all other commands, and for `gen`'s re-run lookup when no path is named |

#### The Scope Banner

Every project-scoped command prints the project it resolved to — and which
link of the chain above produced it — as one line on **stderr** before it does
any work:

```
▸ project ug · ~/Documents/project/ug · data ~/.ug/ug · [active project]
```

It is on stderr, not stdout, so `--json`/`-o` output stays pipeable
(`ug find_symbols x --json | jq` is unaffected). A command that resolves the
same project twice — the agent tools read `graph.json` and then the `ugdb`
beside it — prints one line, not two. The `[…]` label is the rule that fired:
`-n/--name`, `--db`, `-i/--input`, `input path`, `active project`,
`current directory`, `legacy ./.ug/ugdb`, or **`most recently updated
project`** — the last being the one worth noticing, since it means the command
answered from a project that has nothing to do with the current directory.

Because `ug hook` captures the child's output into `~/.ug/<project>/hook.log`,
this line is also what makes a hook run auditable after the fact. Suppress it
with `--no-banner` or `UG_NO_BANNER=1`.

#### Staleness Warning

Riding the same channel, and suppressed by the same flag: every command that
answers from the **index** rather than the working tree warns when that index
is behind the tree, naming the files that drifted.

```
⚠ index is behind the tree · 2 changed of 418 indexed files · src/a.ts, src/b.rs
  Structural answers describe the last index. Refresh: ug update <file>... (fast) or ug gen -n ug.
```

This covers the graph.json readers (`find_symbols`, `file_outline`,
`get_code`, `find_usages`, `project_overview`, `graph_schema`,
`shortest_path`) and the db readers (`analyze`, `search`, `traverse`, `chat`,
`tour`). The commands that *write* the index — `gen`,
`ingest`, `update` — deliberately stay quiet, since warning that the index is
stale immediately before refreshing it is noise.

The reason it exists: `get_code` re-reads the live file and flags drift per
slice, but a structural answer has no such tell — a stale blast radius is
shaped exactly like a true one, and it is likeliest to be stale right after the
caller's own edits, which is exactly when it gets asked. Naming the drifted
files (rather than counting them) is what lets a caller tell "these are the
files I just edited, re-ask after refreshing" from "irrelevant, carry on".

Cost is one `stat` per indexed file, skipped entirely when the banner is off. A
project whose repo has vanished reports nothing: an index frozen against a
moved checkout is not drift anyone can fix with `ug update`. The MCP server
appends the equivalent note to its tool results (`Index may be stale`), since a
stderr line is invisible to an MCP client.

#### `default_read_db_path()` Fallback Chain

When no `-n/--name` flag is provided, the store/db path resolves through:

```
1. active project's ~/.ug/<active>/ugdb      ← honored first (UG_HOME/<name>/ugdb)
2. cwd-basename ugdb (if exists)
3. legacy ~/.ug/ugdb                          ← deprecated path
4. most-recently-updated project's ugdb       ← last resort
5. cwd-basename ugdb (always exists, may be empty)
```

This chain ensures that commands like `ug search"query"` or `ug chat"what is X"` run from an arbitrary directory will automatically pick the project the user previously pinned with `ug active <name>` rather than silently using the cwd or a random "most-recent" project.

---

## 2. HTTP API Routes

The HTTP server (`ug serve`) is built on **axum**. All routes listed below.

### 2.2 Graph Data & Health

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|-------------------|
| GET | `/graph.json` | Serve the full graph.json (gzip/brotli compressed) | graph.json on disk | No graph loaded |
| GET | `/indexed-tree.json` | Serve the indexed tree (per-file parse snapshot) | indexed-tree.json on disk | No graph loaded |
| GET | `/healthz` | Health check — returns `"ok"` | — | — |

### 2.3 Project Management

| Method | Path | What it does | Data source |
|--------|------|-------------|--------------|
| GET | `/api/projects` | List all projects under ~/.ug with stats | `~/.ug/` directory scan |
| POST | `/api/projects/select` | Switch the active project at runtime | Body: `{ "project": "name" }` |
| POST | `/api/projects/delete` | Delete a project | Body: `{ "project": "name" }` |
| GET | `/api/projects/staleness` | Per-project staleness report (changed/deleted files vs graph.json mtime). Each entry has `isStale`, `changed`, `missing`, `files`, `kbKind`, `docNodes`, `codeNodes`, and `repoMissing` — when the repo root no longer exists the entry reports `repoMissing: true` with `isStale: false`, `kbKind: "unknown"` and zero counts rather than a misleading "N files deleted" (the index just freezes where it is). | graph.json mtime + file mtimes (skipped when the repo is gone) |

### 2.4 Generate/KB Manager

| Method | Path | What it does | Data source |
|--------|------|-------------|--------------|
| POST | `/api/generate` | Kick off `ug gen` as a background job (multi-project mode only). Body: `{ "path": "<dir>", "name": "<project>"?, "no_ingest": false, "with_embed": false }`. **`with_embed` is opt-in**, mirroring the CLI: omitted, the job indexes structure into the db and builds no vectors — much faster — and `/api/ingest` backfills them later. `no_ingest` skips the db entirely. Returns `{ "jobId" }`. | Spawns `ug gen`, buffers its log lines |
| GET | `/api/generate/status` | Check status of a gen or ingest job. Query: `?job=<id>`. Returns `{ "status": "running"\|"done"\|"error", "log", "projectName", "error" }`. | In-memory `GenJobs` map |
| POST | `/api/ingest` | Re-embed an already-indexed project (the UI's "Ingest now" button). Body: `{ "name": "<project>" }` — defaults to the active project. Spawns `ug ingest`, poll with `/api/generate/status`. Reopens the active project's stores on success so the new vectors are live without a restart. A store that can't be opened (stale format or corrupt on-disk manifest) is wiped and rebuilt automatically. | Spawns subprocess |
| GET | `/api/browse-dir` | List subdirectories of a path (KB Manager folder picker). Query: `?path=<path>` | Filesystem |

### 2.5 Graph API (graph.json-backed, read-only)

| Method | Path | What it does | Returns 503 when |
|--------|------|-------------|-------------------|
| GET | `/api/capabilities` | Server capabilities matrix: backends, models, features enabled, the `graph` block below, and a `vis` block carrying resolved visualization prefs (`renderer`: auto/three/cosmos, `three_d_max_elements`, `solo_threshold`) — null when unset, so the page falls back to its built-in defaults | — |
| GET | `/api/config` | Persisted + effective settings, with per-key source (flag/env/config/default) and `kind` (`str`/`f32`/`u32`/`u64`/`enum`); `enum` keys carry their `choices` so the UI can render a dropdown | — |
| POST | `/api/config` | Persist settings to `~/.ug/config.json` — body `{ "set": {key:value}, "unset": [keys] }`. Chat changes apply immediately; embed changes after a server restart; vis and graph-server-mode changes on the next page load | — |
| GET | `/api/graph/stats` | Node/edge counts, file stats | No graph |
| GET | `/api/graph/nodes` | **Slim node index** — every node's `{id, name, type, file, startLine, endLine}` in columnar form with `node_type` and `file` dictionary-coded, plus undirected `deg`, sparse `boundary`, `catalogRoots` and the per-type counts. No edges. This is what the page loads instead of `graph.json` when `capabilities.graph.mode` is `server`; a node's index in these columns is its identity for every other server-mode request. Built once per snapshot | No graph |
| GET | `/api/graph/nodes.bin` | **The same index as a binary frame** — what the page actually loads in `server` mode; `/api/graph/nodes` is the fallback for an older client. Magic `UGNIDX\0\0`, `u32` version, a section table of `(kind, offset, len)`, then the columns as little-endian typed arrays, 4-byte aligned so the client can view them in place. Ids and names are front-coded (one `u8` of shared-prefix length per entry, restarting every 16) and travel with a `u32` FNV-1a hash column so the client builds its `id → index` table without decoding any of them. The tail section is JSON for everything that is not a column. Built once per snapshot | No graph |
| POST | `/api/graph/edges` | **Batch neighbourhood** — body `{ids:[node indices], scope:"incident"\|"induced"}` → columnar `{src, tgt, rel, relTypes, complete}`. `incident` returns every edge touching each id and lists them in `complete`; `induced` returns only edges with **both** ends in the set and `complete` is always empty, because it withholds the edges that leave the set. Server mode's one graph primitive: solo expansion, focus, the Related tab, walk and tour all reduce to this | No graph |
| POST | `/api/graph/nodes/hydrate` | **Batch node detail** — body `{ids:[node indices]}` → the full `GraphNode` for each: docstring, signature, metrics, imports/exports/calls/extends/implements, boundary detail. Exactly the fields `/api/graph/nodes` omits, fetched only for what the info panel is showing. Out-of-range indices are dropped, not fatal | No graph |
| GET | `/api/graph/node/*id` | Get one node by id | No graph / not found |
| GET | `/api/graph/search` | Keyword search over node **names, qualified ids and docstrings** (`?q=`, `?types=`, `?limit=` — default 200, cap 5,000). Results are **ranked** — by where the query appears in the name, then by name length — before `limit` is applied, so the cut keeps the best matches rather than the first ones in graph order. `count` is the number of matches, `returned` the number sent. This is the page's search box in `server` mode | No graph |
| GET | `/api/graph/traverse/*id` | K-hop BFS from a node seed | No graph |
| GET | `/api/graph/path` | Shortest path between two nodes (query: `?source=&target=`) | No graph |
| GET | `/api/graph/filter` | Filter edges by type/endpoint | No graph |
| GET | `/api/graph/centrality` | Degree & betweenness centrality | No graph |
| GET | `/api/graph/cycles` | Detect dependency cycles | No graph |

#### How the page gets its graph — `capabilities.graph`

`/api/capabilities` carries a `graph` block that decides this:

```json
"graph": { "mode": "server", "bytes": 346266017,
           "nodes": 161725, "edges": 745964, "threshold": 52428800 }
```

| `mode` | What the page does |
|---|---|
| `local` | Downloads `/graph.json` whole and answers everything in the browser. What every release before this did, and still the default for graphs under the threshold. |
| `server` | Loads `/api/graph/nodes.bin` instead — every node, no edges — and asks this server for edges, neighbourhoods, per-node detail and search. The page holds the index as typed-array columns rather than as node objects and builds a node only when something asks for one, so what it retains is set by what you touch, not by how big the graph is. Falls back to `/api/graph/nodes` (JSON) if the binary frame is unavailable. |

`ug serve --graph-mode <auto\|local\|server>` sets the policy; `auto` (the
default) resolves it per project by comparing `bytes` against `threshold`
(50 MB by default, configurable as `graph.server_mode_bytes`). The size is
`graph.json`'s identity bytes, not its compressed size,
because what costs the browser is the parse and the retained objects rather
than the transfer. Note the `threshold` in the payload is the *effective*
value — the configured `graph.server_mode_bytes`, or the 50 MB built-in when
unset.

**A missing `graph` block means `local`.** That is what a static host answers,
which is why the published demo keeps working with no shim change.

#### Visualization prefs — `capabilities.vis`

The page settles on a renderer and a solo threshold *before* its first render,
so the resolved values travel in `/api/capabilities` (the fetch it already makes
in that window) rather than in a second call. Both are `null` when unset, in
which case the page uses its built-ins:

```json
"vis": { "renderer": null, "three_d_max_elements": null, "solo_threshold": null }
```

- `renderer` — `auto` (three below `three_d_max_elements` elements, cosmos
  above), `three`, or `cosmos`. The page reads it below the session/`?r=`/
  `localStorage` choices but above the graph-size default, so it is a persisted
  preference rather than an override.
- `three_d_max_elements` — the 3D engine's whole-graph budget; the size `auto`
  switches over at. The page falls back to its built-in `THREE_D_MAX_ELEMENTS`
  (3,000) when null.
- `solo_threshold` — the 2D engine's solo ceiling; the 3D engine solos past
  `three_d_max_elements` instead. Both are editable from the settings panel
  (Visualization section) or `ug config set vis.renderer …` /
  `ug config set vis.three_d_max_elements …` /
  `ug config set vis.solo_threshold …`. The threshold above (in the `graph`
  block) comes from `graph.server_mode_bytes`.

### 2.6 Agent Tool API (same as MCP tools, over HTTP)

| Method | Path | What it does | Data source |
|--------|------|-------------|--------------|
| GET | `/api/tools` | List available agent tools (`name`, one-line `summary`, `path`, `method`, copyable example `body` each) — the HTTP equivalent of MCP `tools/list`. Includes the store-backed `analyze`, marked with a `note` that its preset catalog lives at `GET /api/presets` | MCP tool registry |
| GET | `/api/presets` | Preset registry **plus** `properties` — the queryable property vocabulary, so the UI and the MCP capability manifest read the same list rather than each hardcoding one (name, category, description, params, source) | Preset registry |
| POST | `/api/tools/:tool` | Run one agent tool (same params as MCP). Accepts body JSON with optional `project` field. | graph.json |
| POST | `/api/tools/analyze` | Run a statistical query. Body: `{preset, args, gql, limit, range}`. Accepts the optional `project` field like every other tool, resolves its store the same way, and applies the MCP-side arg normalization (stringified numbers/booleans coerced, paging `limit`/`range` lifted out of `args`, JSON-array `args` like `{"files": ["a.ts","b.rs"]}` joined to comma form). Returns `columns` plus **only the requested window** of `rows`, with `from`/`to`/`rowsTotal` to page by, `rowsMatched`, `coverage`, `unindexed`, `warnings`, `truncated` and a rendered `text`. | ugdb (no embedder) |

### 2.7 File Content

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|------------------|
| GET | `/api/file` | Read source file content for the right-panel "Preview" tab. Query: `?file=<path>&startLine=&endLine=`. Reads the live file from disk first; when the repo path is gone (or the file was deleted) it falls back to the **indexed copy** captured in ugdb (`NodeRow.code`), so previews keep working with no repo on disk. The response includes `"source": "filesystem"\|"db"` and `"sliced"` (true when an exact symbol span was served, false when it fell back to the whole-file capture). | Filesystem, falling back to ugdb stored source | No project |

### 2.8 Database API (OverGraph/Neo4j-backed — Phase 3)

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|------------------|
| GET | `/api/db/node/*id` | Get a node by id from the knowledge store | ugdb or Neo4j | DB not opened |
| GET | `/api/db/traverse/*id` | K-hop BFS using the DB edges table | ugdb or Neo4j | DB not opened |
| POST | `/api/search/semantic` | Pure vector search — `{ "query", "k", "whereClause" }` | ugdb or Neo4j + embedder | DB or embedder unavailable |
| POST | `/api/search/hybrid` | GraphRAG search — `{ "query", "k", "hops", "edgeTypes", "direction", "maxChars", "mmrLambda", "whereClause", "includeSnippets", "strategy", ... }` | ugdb or Neo4j + embedder | DB or embedder unavailable |

### 2.9 Chat & Tour API

| Method | Path | What it does | Data source | Returns 503 when |
|--------|------|-------------|--------------|------------------|
| POST | `/api/chat` | RAG chat. Body: `{ "message", "history", "k", "direction", "edgeTypes", "repoRoot", "model", "baseUrl", "apiKey", "temperature", "maxTokens", "system", "tools", "stream", ... }` — non-streaming by default; `"stream": true` switches to an SSE response (`event: context` / `delta` / `done` / terminal `error`) | DB + embedder + chat LLM | Chat endpoint not configured |
| GET | `/api/chat/config` | Get the server's default chat configuration | Config |
| POST | `/api/tour` | Guided tour. Body: `{ "topic", "projectName", "maxStops", "model", "stream", ... }` → plan → candidates → narration + links — non-streaming by default; `"stream": true` narrates itself over SSE (`event: progress` per phase, then `tour`, or terminal `error`) | DB + embedder + chat LLM | See tour opts |

---

## 3. Agent / MCP Tools

### 3.1 Advertised MCP Tools (`tools/list`)

These 13 tools are advertised over MCP `tools/list` and also available via the CLI and HTTP `/api/tools/:tool`. Each tool accepts an optional `project` parameter (except `list_projects`).

| Tool | What it does | Data source | When it errors |
|------|-------------|-------------|----------------|
| `search` | **Primary KB search.** RRF (vector + FTS) → Personalized PageRank over edge graph → ranked context with snippets. `expand: false` stops at the seeds — no graph walk, no neighbors — which is what the retired `semantic_search` tool did; that name still dispatches here. | ugdb/Neo4j + embedder + graph.json | DB/embedder unavailable |
| `traverse` | Walk graph N hops from seed symbols (id, name or wildcard). Filters by edge type and direction; several seeds make one merged walk. | graph.json (graph-backed, no DB needed) | graph.json missing/invalid |
| `find_usages` | Inbound references to a symbol (callers, importers, subclasses, etc.), by id, name or wildcard. Wrapper over traverse with direction=inbound + sensible defaults. Call-site lines come from each caller's stored source, with filesystem fallback. | graph.json (+ ugdb for call sites) | graph.json missing/invalid |
| `find_symbols` | Symbol lookup by name (case-insensitive, ranked exact > prefix > substring) or by wildcard pattern (whole-name match). Filters by node type, file path/glob, and `boundary` (system entry/exit points only — works with no name at all, which lists the whole surface). Supports batch via array of names/patterns/ids. | graph.json | graph.json missing/invalid |
| `file_outline` | List every indexed symbol in a file, in line order. Accepts a path, unique suffix, File node id, or path glob (up to `maxFiles` files). Supports batch via array. | graph.json | graph.json missing/invalid |
| `get_code` | Read source for a symbol (id, name or wildcard) or a file/line range. Works from stored source in DB (consistent with search) with filesystem fallback; a file/line range is cut out of the file's whole-file capture, so it needs no working tree either. | ugdb (preferred) + filesystem fallback | node/file captured in neither ugdb nor the working tree |
| `project_overview` | Orient in the codebase: repo root, node/edge counts, biggest files, most depended-upon symbols. | graph.json | graph.json missing/invalid |
| `context` | **Curated bundle for one symbol**: source + callers (with call sites) + tests + dependencies + linked docs, each item labelled by role, filled in priority order under one `maxChars` budget. Assembly over the existing tools, not new analysis — so it needs no embedder and no db beyond the stored source `get_code` and `find_usages` already use. Takes exactly one symbol; an ambiguous name is an error listing the candidates. | graph.json (+ ugdb for source & call sites) | graph.json missing/invalid, or the symbol resolves to zero/several nodes |
| `shortest_path` | Find shortest directed edge path between two symbols. Each endpoint (id, name or wildcard) must resolve to exactly one node. | graph.json | graph.json missing/invalid |
| `analyze` | **Whole-repo statistics**: counts, groups, distributions, blast radius. Takes a named `preset` or raw GQL. Read-only — mutations are rejected before write staging. Every answer reports property coverage, because aggregating over an unstored property returns `0` rather than an error. | ugdb (**no embedder**) | db missing or written by an older ug |
| `graph_schema` | **Capability manifest**: node & edge types with counts and connection shapes (from graph.json), plus queryable properties with live coverage and the `analyze` preset list (from the db). | graph.json + ugdb | graph.json missing/invalid (the db half degrades to a note) |
| `list_projects` | List every indexed project on this machine (name, repo path, graph size). | `~/.ug/` directory scan | — |
| `gen` | Re-run index → graph → embed pipeline for the current (or named) project, from its recorded repo root. Incremental (content-hash cache). Graph tools refresh even if embedding fails. | Repo source → index.json → graph.json → ugdb/Neo4j | Repo root missing |

### 3.2 Unlisted Tools (hidden from agents, available via `ug mcp call`)

| Tool | What it does | Data source |
|------|-------------|-------------|
| `ping_embedder` | Ping the embedding endpoint to check connectivity | Embedder endpoint |

### 3.4 Chat Tool Denylist

`gen` and `list_projects` are excluded from the OpenAI-compatible tool schemas used by `ug chat` — the LLM should not be able to reindex or list projects mid-conversation.

### 3.5 OpenAI-compatible Tool Schemas

The `openai_tool_schemas()` function in `mcp/tools.rs` converts the MCP tool definitions into OpenAI `functions` format (with `parameters` instead of `inputSchema`, and with `project` removed). These are fed to the `/api/chat` LLM endpoint so the model can call tools like `find_symbols`, `get_code`, `search`, etc. mid-answer.

---

## 4. Storage Backends

### 4.1 Architecture

```
                    ┌─────────────────────────────────────┐
                    │         KnowledgeStore trait         │
                    │  (store.rs — pluggable abstraction)  │
                    └──────────┬──────────────────────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
     ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
     │  OverGraph   │  │    Neo4j     │  │   Future     │
     │  (db.rs)     │  │(backends/    │  │  backends    │
     │  embedded    │  │ neo4j.rs)    │  │              │
     │  engine      │  │ remote DB    │  │              │
     └──────────────┘  └──────────────┘  └──────────────┘
```

### 4.2 StoreSpec (destination selector)

Two variants, created from env vars or `--dest` flags:

| Variant | Connection | Environment variables |
|---------|-----------|----------------------|
| `StoreSpec::Overgraph { path, embedding_dim }` | Local embedded database engine | `UG_DEST=overgraph` (default) |
| `StoreSpec::Neo4j { uri, user, password, database, embedding_dim }` | Remote Neo4j via Bolt protocol | `UG_DEST=neo4j`, `UG_NEO4J_URI`, `UG_NEO4J_USER`, `UG_NEO4J_PASSWORD`, `UG_NEO4J_DATABASE` |

### 4.3 KnowledgeStore Trait

All backends implement this async trait:

| Method | What it does |
|--------|-------------|
| `embedding_dim()` | Return the vector dimension |
| `supports_native_ppr()` | Whether Personalized PageRank runs natively |
| `backend_name()` | `"overgraph"` or `"neo4j"` |
| `upsert_nodes(rows)` | Write/replace node rows (with embeddings) |
| `upsert_edges(rows)` | Write/replace edges |
| `vector_search(query, k, filter)` | Dense vector search (cosine distance) |
| `hybrid_search(query, sparse, text, k, filter)` | Dense + keyword fusion search |
| `traverse(start, max_hops, edge_types, direction)` | K-hop graph traversal |
| `execute_query(gql, params, limits)` | Run one **read-only** GQL statement for statistics. Default impl returns `Unsupported` — a backend without a query language must say so rather than answer approximately. Implemented on OverGraph; Neo4j inherits the default. |
| `ensure_query_indexes()` | Declare property indexes that make statistical queries cheap. Best-effort; default no-op. |
| `nodes_by_ids(ids)` | Bulk read nodes by id |
| `nodes_for_upsert(keys)` | Read back rows before upsert (for incremental ingest diffing) |
| `prune_nodes_absent_from(keep)` | Delete nodes not in the given set |
| `fetch_node(key)` | Get one node by id |
| `count_nodes()` | Total node count |
| `count_edges()` | Total edge count |
| `personalized_pagerank(seeds, direction, edge_types, ...)` | PPR computation |

### 4.4 OverGraph (db.rs)

The default embedded backend. Uses the `overgraph` crate's `DatabaseEngine`.

- **Storage location**: `<project-dir>/ugdb/` (alongside `graph.json`)
- **Vector index**: HNSW index over dense `f32` embeddings (cosine similarity)
- **Full-text index**: FNV-hashed sparse keyword vectors
- **Node keying**: Flat id space (`"function:src/main.rs:42:foo"`), mapped to internal `u64` ids
- **Edge storage**: Directed edges in adjacency table
- **Schema file**: `ug-meta.json` records the embedding dimension

### 4.5 Neo4j (backends/neo4j.rs)

An alternative remote backend. Connects via the Bolt protocol.

- **Connection**: env vars `UG_NEO4J_URI`, `UG_NEO4J_USER`, `UG_NEO4J_PASSWORD`, `UG_NEO4J_DATABASE`
- **Vector index**: Neo4j native vector index (label-based, cosine similarity)
- **Full-text**: Neo4j full-text search index
- **Node keying**: Label `GraphNode`, unique constraint on `id`
- **PPR**: Via the Graph Data Science (GDS) plugin (`gds.pageRank.stream`). Returns `Unsupported` when GDS is not installed.
- **Schema**: Auto-created on first connect (constraints, indexes)

### 4.6 StoreSet (fan-out)

When `--dest overgraph,neo4j` is specified, both backends are opened as a `StoreSet`. Writes fan out to all stores; reads pick exactly one store (the primary/first).

### 4.7 Other Storage Modules

| Module | Path | What it does |
|--------|------|-------------|
| `text.rs` | `storage/text.rs` | Builds the embedding text for each node (name + signature + docstring + related names + snippet) |
| `embed/mod.rs` | `storage/embed/mod.rs` | Remote embedder client (OpenAI-compatible `/v1/embeddings`), with auto-probe of dimension |
| `embed/local.rs` | `storage/embed/local.rs` | Local embedder via fastembed-rs (in-process, no external service) |
| `query.rs` | `storage/query.rs` | High-level search functions: `search_kb`, `semantic_search`, `semantic_search_w_where`, `mmr_rerank`, `traverse_filtered`, `traverse` |
| `ingest.rs` | `storage/ingest.rs` | Embedding + write pipeline: `ingest_graph`, `plan_incremental_ingest`, `prune_to_graph` |
| `ppr.rs` | `storage/ppr.rs` | Wrapper around `personalized_pagerank`: `run_ppr`, `default_edge_type_weights` |
| `source.rs` | `storage/source.rs` | Captures node source code from the filesystem for indexing in the DB |
| `types_registry.rs` | `storage/types_registry.rs` | Stable string ↔ u32 mapping for node types and edge types in OverGraph |
| `comments.rs` | `storage/comments.rs` | Extracts prose comments from source for embedding text |

---

## 5. Pipeline

### 5.1 Pipeline Flow

```
Source files
     │
     ▼
┌──────────┐    index.json (indexed-tree.json)
│   ug index│───→  { "files": [FileNode], "folders": [...],
│           │       "dependencies": [...], "stats": {...} }
└──────────┘
     │
     │  FileNode contains: path, hash, language, classification,
     │  symbols (id, name, kind, startLine, endLine, docstring,
     │  signature, imports, exports, extends, implements, calls, metrics)
     │
     ▼
┌──────────┐    graph.json
│  ug graph │───→  { "nodes": [GraphNode], "edges": [GraphEdge],
│           │       "stats": {...} }
└──────────┘
     │
     │  GraphNode contains: id, name, node_type, file, startLine, endLine,
     │  metrics, signature, docstring, imports, exports, extends,
     │  implements, calls, folder_meta
     │
     │  GraphEdge contains: source_id, target_id, edge_type
     │  Edge types: Calls, Extends, Implements, References, Contains,
     │              Imports, Exports, Requires, Uses, DependsOn
     │
     ▼
┌──────────┐    ugdb/ or Neo4j
│ ug ingest │───→  Nodes with dense embeddings + sparse keyword vectors
│           │      Edges in adjacency table
└──────────┘      NodeRow: { id, node_type, name, text, embedding,
                        file, start_line, end_line, code, file_hash,
                        description, docstring }
                  EdgeRow: { source, edge_type, target }
```

### 5.2 index.json Schema (`IndexResult`)

| Field | Type | Contents |
|-------|------|----------|
| `files` | `Vec<FileNode>` | One per source file. Contains all parsed symbols, imports, exports |
| `folders` | `Vec<FolderNode>` | File-system folder hierarchy with classification, README summaries |
| `dependencies` | `Vec<Dependency>` | From `package.json` (npm dependencies) |
| `stats` | `IndexStats` | totalFiles, cachedFiles, totalSymbols, totalFolders, totalLines, indexingTimeMs, lastIndexedAt, repoRoot |

### 5.3 graph.json Schema (`GraphData`)

| Field | Type | Contents |
|-------|------|----------|
| `nodes` | `Vec<GraphNode>` | All symbols + files + folders + dependency nodes. Id format: `{type}:{file}:{line}:{name}` e.g. `function:native/src/main.rs:35:main`, `file:native/src/serve.rs`, `folder:native/src/storage` |
| `edges` | `Vec<GraphEdge>` | Directed edges between nodes. Types: `Calls`, `Extends`, `Implements`, `References`, `Contains`, `Imports`, `Exports`, `Requires`, `Uses`, `DependsOn` |
| `stats` | `Option<IndexStats>` | Copied from index.json for self-contained graph |

### 5.4 Pipeline Details

1. **Index** (`indexer.rs`):
   - Discovers files with supported extensions via `scan_files()`
   - For each file: tree-sitter parse → symbol extraction → import/export resolution
   - Supported languages: Rust, TypeScript/TSX, JavaScript, Python, Java, Markdown, PDF, Word, Excel, PowerPoint
   - Cache: blake3 content hashes in `cache.json`; unchanged files reuse previous `FileNode`s
   - Output: `indexed-tree.json` (write-once, overwritten on re-index)

2. **Graph** (`graph.rs`):
   - Reads `IndexResult` (parsed in memory)
   - Pass 0: Creates folder nodes + folder hierarchy edges (`Contains`)
   - Pass 1: Creates file nodes + symbol nodes (Function, Class, Interface, Variable, Concept, Constant, Config, Dependency)
   - Resolves cross-file imports: joins the import path against source file's directory, tries extension permutations, falls back to basename matching
   - Creates edges: `Calls`, `Extends`, `Implements`, `References`, `Contains`, `Imports`, `Exports`, `Requires`, `Uses`, `DependsOn`
   - Output: `graph.json` (write-once, overwritten on re-graph)

3. **Ingest** (`storage/ingest.rs`):
   - Reads `GraphData`, builds embedding text for each node via `text::build_node_text()` (name + signature + docstring + related names + folder context + source snippet)
   - Generates sparse keyword vectors via `text.rs`
   - **Incremental**: diffs incoming vs stored nodes (by content hash), skips embedding for unchanged/reusable nodes
   - Embeds only new/changed nodes (in batches via the embedder)
   - Upserts nodes + edges to all configured backends
   - Optionally prunes stored nodes that no longer exist in the graph (`--prune`)
   - Output: embedded nodes and edges written to ugdb/ or Neo4j

### 5.5 Running the Pipeline

- **One-step**: `ug gen` runs index → graph → ingest sequentially
- **Step-by-step**: `ug index && ug graph && ug ingest`
- **With visualization**: `ug gen --serve` (or just `ug serve`)
- **MCP gen**: the `gen` MCP tool runs the full pipeline and reports results
