//! The catalogue of HTTP endpoints `ug serve` registers.
//!
//! It lives beside the router rather than under `cli/` because it describes
//! *this* module's surface: when a route is added to `router.rs`, the table
//! that has to change is the one next to it. `ug api` is only a printer of
//! it — see `cli/api.rs`.

/// One HTTP endpoint `ug serve` registers, for `ug api`'s reference
/// listing. `cli_equivalent` is `Some("ug <cmd>")` when the exact same
/// data/action is also reachable as a plain CLI subcommand that works
/// without a server running at all — everything in this table is an
/// HTTP route, so it always requires `ug serve` to be up to hit it over
/// HTTP; this field instead tells the user whether *the underlying
/// capability* has a non-serve escape hatch.
pub(crate) struct ApiEntry {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    pub(crate) desc: &'static str,
    pub(crate) availability: &'static str,
    pub(crate) cli_equivalent: Option<&'static str>,
}

pub(crate) const API_ENDPOINTS: &[(&str, &[ApiEntry])] = &[
    (
        "Knowledge-base / project management",
        &[
            ApiEntry { method: "GET", path: "/api/projects", desc: "list discovered projects (or the single active one)", availability: "always", cli_equivalent: Some("ug list") },
            ApiEntry { method: "GET", path: "/api/projects/staleness", desc: "per-project staleness report (changed/deleted files vs graph.json mtime)", availability: "always", cli_equivalent: Some("ug list (same scan)") },
            ApiEntry { method: "POST", path: "/api/projects/select", desc: "switch the server's active project", availability: "multi-project mode only", cli_equivalent: None },
            ApiEntry { method: "POST", path: "/api/projects/delete", desc: "delete a project's data directory", availability: "multi-project mode only", cli_equivalent: Some("ug remove") },
            ApiEntry { method: "POST", path: "/api/generate", desc: "spawn `ug gen` against a folder, returns a job id", availability: "multi-project mode only", cli_equivalent: Some("ug gen") },
            ApiEntry { method: "GET", path: "/api/generate/status", desc: "poll a generation job's progress/log", availability: "multi-project mode only", cli_equivalent: None },
            ApiEntry { method: "POST", path: "/api/ingest", desc: "re-embed an already-indexed project (UI Ingest now button); poll via /api/generate/status", availability: "always", cli_equivalent: Some("ug ingest") },
            ApiEntry { method: "GET", path: "/api/browse-dir", desc: "list subdirectories of a path (KB wizard folder picker)", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/capabilities", desc: "db/embedder/chat readiness, the indexing caps that shaped the store, and the resolved graph/vis prefs (server-mode threshold, renderer / solo threshold)", availability: "always", cli_equivalent: Some("ug doctor (similar info)") },
            ApiEntry { method: "GET", path: "/api/config", desc: "persisted + effective settings with per-key source (flag/env/config/default)", availability: "always", cli_equivalent: Some("ug config list") },
            ApiEntry { method: "POST", path: "/api/config", desc: "persist settings to ~/.ug/config.json (chat changes apply immediately)", availability: "always", cli_equivalent: Some("ug config set") },
        ],
    ),
    (
        "Graph API (in-memory, active project)",
        &[
            ApiEntry { method: "GET", path: "/api/graph/stats", desc: "node/edge counts by type", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/graph/nodes", desc: "slim node index (columnar: every node's id/name/type/file/lines, no edges) — what the page loads instead of graph.json on large graphs", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "POST", path: "/api/graph/edges", desc: "batch neighbourhood: edges around the given node indices (scope=incident|induced) — server mode's one graph primitive", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "POST", path: "/api/graph/nodes/hydrate", desc: "batch node detail: the docstring/signature/metrics/calls fields the slim index omits", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/graph/node/:id", desc: "fetch one node by id", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/graph/search", desc: "keyword search over graph nodes", availability: "always (empty if no project active)", cli_equivalent: Some("ug find_symbols") },
            ApiEntry { method: "GET", path: "/api/graph/traverse/:id", desc: "k-hop BFS traversal from a node", availability: "always (empty if no project active)", cli_equivalent: Some("ug traverse") },
            ApiEntry { method: "GET", path: "/api/graph/path", desc: "shortest path between two nodes", availability: "always (empty if no project active)", cli_equivalent: Some("ug shortest_path") },
            ApiEntry { method: "GET", path: "/api/graph/filter", desc: "filter edges by type", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/graph/centrality", desc: "degree/betweenness centrality", availability: "always (empty if no project active)", cli_equivalent: Some("ug graph_centrality") },
            ApiEntry { method: "GET", path: "/api/graph/cycles", desc: "detect cycles in the graph", availability: "always (empty if no project active)", cli_equivalent: Some("ug graph_cycles") },
            ApiEntry { method: "GET", path: "/api/file", desc: "source file content for the preview panel", availability: "always (404 if file/project missing)", cli_equivalent: None },
        ],
    ),
    (
        "Agent tools (graph.json-backed — same names/params as the CLI and MCP)",
        &[
            ApiEntry { method: "GET", path: "/api/tools", desc: "list the agent tools and their paths (HTTP equivalent of MCP tools/list)", availability: "always", cli_equivalent: Some("ug help") },
            ApiEntry { method: "GET", path: "/api/presets", desc: "analyze preset catalog plus the queryable property vocabulary", availability: "always", cli_equivalent: Some("ug analyze --list") },
            ApiEntry { method: "POST", path: "/api/tools/project_overview", desc: "stats, biggest files, most depended-upon symbols", availability: "always (empty if no project active)", cli_equivalent: Some("ug project_overview --json") },
            ApiEntry { method: "POST", path: "/api/tools/context", desc: "one symbol's whole neighbourhood: code, callers, tests, deps, docs — budgeted", availability: "always (empty if no project active)", cli_equivalent: Some("ug context --json") },
            ApiEntry { method: "POST", path: "/api/tools/find_symbols", desc: "symbol lookup by name or wildcard ('handle_*')", availability: "always (empty if no project active)", cli_equivalent: Some("ug find_symbols --json") },
            ApiEntry { method: "POST", path: "/api/tools/file_outline", desc: "every indexed symbol in a file, in line order; takes a path glob", availability: "always (empty if no project active)", cli_equivalent: Some("ug file_outline --json") },
            ApiEntry { method: "POST", path: "/api/tools/get_code", desc: "source for a symbol (id, name or wildcard), or a file/line range", availability: "always (empty if no project active)", cli_equivalent: Some("ug get_code --json") },
            ApiEntry { method: "POST", path: "/api/tools/find_usages", desc: "inbound callers/importers, with call sites", availability: "always (empty if no project active)", cli_equivalent: Some("ug find_usages --json") },
            ApiEntry { method: "POST", path: "/api/tools/shortest_path", desc: "shortest directed edge path between two symbols", availability: "always (empty if no project active)", cli_equivalent: Some("ug shortest_path --json") },
            ApiEntry { method: "POST", path: "/api/tools/graph_schema", desc: "node & edge types present, with counts", availability: "always (empty if no project active)", cli_equivalent: Some("ug graph_schema --json") },
            ApiEntry { method: "POST", path: "/api/tools/analyze", desc: "run a GQL (Cypher-like) query or built-in preset against the OverGraph store", availability: "503 if no DB backend configured", cli_equivalent: Some("ug analyze") },
        ],
    ),
    (
        "OverGraph search & chat (Phase 3 — needs a DB + embedder)",
        &[
            ApiEntry { method: "GET", path: "/api/db/node/:id", desc: "fetch one node from the OverGraph store", availability: "503 if no DB backend configured", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/api/db/traverse/:id", desc: "k-hop BFS over the OverGraph edges table", availability: "503 if no DB backend configured", cli_equivalent: Some("ug traverse") },
            ApiEntry { method: "POST", path: "/api/search/semantic", desc: "semantic vector search", availability: "503 if no DB + embedder configured", cli_equivalent: Some("ug search --no-expand") },
            ApiEntry { method: "POST", path: "/api/search/hybrid", desc: "GraphRAG: semantic search → graph expansion → ranked context", availability: "503 if no DB + embedder configured", cli_equivalent: Some("ug search") },
            ApiEntry { method: "POST", path: "/api/chat", desc: "GraphRAG-grounded chat completion (\"stream\": true in the body switches to SSE)", availability: "503 if no DB + embedder + chat model configured", cli_equivalent: Some("ug chat") },
            ApiEntry { method: "GET", path: "/api/chat/config", desc: "the server's default chat configuration", availability: "always", cli_equivalent: Some("ug config list (similar info)") },
            ApiEntry { method: "POST", path: "/api/tour", desc: "Guided, narrated walkthrough — ordered stops bound to node ids (\"stream\": true switches to SSE)", availability: "503 if no DB + embedder; LLM narration optional (ranked fallback)", cli_equivalent: Some("ug tour") },
        ],
    ),
    (
        "UI & static assets",
        &[
            ApiEntry { method: "GET", path: "/", desc: "graph visualization UI (single-page app)", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/index.html", desc: "same as /", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/threejs-vis.bundle.js", desc: "3D renderer bundle (three.js + 3d-force-graph), loaded on demand", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/cosmos-vis.bundle.js", desc: "2D renderer bundle (cosmos.gl), loaded on demand", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/favicon.svg", desc: "browser tab icon", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/healthz", desc: "liveness probe — always returns \"ok\"", availability: "always", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/graph.json", desc: "raw graph JSON for the active project", availability: "always (empty if no project active)", cli_equivalent: None },
            ApiEntry { method: "GET", path: "/indexed-tree.json", desc: "the indexed tree (per-file parse snapshot)", availability: "always (empty if no project active)", cli_equivalent: None },
        ],
    )    
];
