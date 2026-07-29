//! MCP tool registry: the JSON Schema advertised over `tools/list`, plus the
//! name aliases and hidden tools the dispatcher honours. Ported from the
//! `MCP_TOOLS` / `TOOL_ALIASES` tables that used to live in `node/cli.mjs`.
//!
//! The schemas are hand-written (rather than derived from the param structs)
//! because the descriptions are load-bearing prompt text tuned for agents —
//! `schemars` output would lose them.

use serde_json::{json, Value};

/// Canonical tool names, in the order `tools/list` advertises them.
pub const TOOL_NAMES: &[&str] = &[
    "search",
    "semantic_search",
    "traverse",
    "find_usages",
    "find_symbols",
    "file_outline",
    "get_code",
    "project_overview",
    "shortest_path",
    "code_query",
    "graph_schema",
    "list_projects",
    "regen",
];

/// Pre-rename spellings. `tools/list` advertises only the canonical set, but an
/// agent may have an old name cached from an earlier session, so keep accepting
/// them — same aliases the CLI honours for its subcommands.
pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "search_kb" | "hybrid_search" => "search",
        "graph_path" | "path" => "shortest_path",
        "list" => "list_projects",
        "find_symbol" => "find_symbols",
        // graph_search was find_symbols over names *and* docstrings; the
        // docstring half comes back via `alias_defaults`.
        "graph_search" => "find_symbols",
        // `reindex` named only the first of the three stages it runs
        // (index → graph → embed). `regen` says what it does and pairs
        // with the `ug gen` it repeats; the old name keeps working because
        // agents cache tool lists between sessions.
        "reindex" => "regen",
        other => other,
    }
}

/// Legacy names that implied a non-default param value. Merged under the
/// caller's args (caller wins).
pub fn alias_defaults(raw_name: &str) -> Option<Value> {
    match raw_name {
        "graph_search" => Some(json!({ "includeDocs": true })),
        _ => None,
    }
}

/// Handled by the dispatcher but deliberately absent from `tools/list` —
/// operator diagnostics that would only waste an agent's tool call. Still
/// invocable through `ug mcp call` for debugging.
pub fn is_unlisted_tool(name: &str) -> bool {
    name == "ping_embedder"
}

pub fn is_known_tool(canonical: &str) -> bool {
    TOOL_NAMES.contains(&canonical) || is_unlisted_tool(canonical)
}

pub const CHAT_TOOL_DENYLIST: &[&str] = &["regen", "list_projects"];

pub fn openai_tool_schemas() -> Vec<serde_json::Value> {
    let listed = tool_list();
    listed
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| !CHAT_TOOL_DENYLIST.contains(&n))
                        .unwrap_or(false)
                })
                .map(|t| {
                    // MCP calls it `inputSchema`; OpenAI wants
                    // `function.parameters`. Same JSON Schema either way.
                    let mut params = t.get("inputSchema").cloned().unwrap_or_else(
                        || json!({ "type": "object", "properties": {} }),
                    );
                    // `project` is an MCP nicety — the server already knows
                    // which project it serves, and letting the model pick
                    // another one mid-answer just invites confusion.
                    if let Some(props) = params
                        .get_mut("properties")
                        .and_then(|p| p.as_object_mut())
                    {
                        props.remove("project");
                    }
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.get("name").cloned().unwrap_or_default(),
                            "description": t.get("description").cloned().unwrap_or_default(),
                            "parameters": params,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Coerce arguments that a model stringified back into real JSON.
///
/// Models routinely send `"nodeId": "[\"function:…\"]"` — a JSON array
/// *encoded as a string* — instead of `"nodeId": ["function:…"]`. Union
/// types in our schemas (`string | array`) make that especially tempting,
/// and the result is a lookup for a node whose id literally contains
/// brackets and quotes. Same story for `"hops": "2"`.
///
/// So before dispatch, re-read each argument against what its schema
/// says it should be. Rejecting a well-meant call over quoting teaches
/// the model nothing and costs the user a round-trip.
pub fn normalize_args(tool: &str, args: &mut Value) {
    let canonical = canonical_tool_name(tool);
    let schema = raw_tools();
    let Some(props) = schema
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["name"] == json!(canonical)))
        .and_then(|t| t["inputSchema"]["properties"].as_object())
    else {
        return;
    };
    let Some(obj) = args.as_object_mut() else {
        return;
    };

    for (key, value) in obj.iter_mut() {
        let Some(spec) = props.get(key) else { continue };
        let Some(text) = value.as_str() else { continue };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Only rewrite when the schema says this field isn't a plain
        // string, so an id that happens to look numeric stays a string.
        if !accepts_non_string(spec) {
            continue;
        }
        let looks_encoded = trimmed.starts_with('[') || trimmed.starts_with('{');
        let parsed = if looks_encoded {
            serde_json::from_str::<Value>(trimmed).ok()
        } else if accepts_kind(spec, "integer") || accepts_kind(spec, "number") {
            trimmed.parse::<i64>().ok().map(Value::from)
        } else if accepts_kind(spec, "boolean") {
            match trimmed {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            }
        } else {
            None
        };
        if let Some(p) = parsed {
            if !p.is_string() {
                *value = p;
            }
        }
    }
}

/// Does this property schema admit something other than a plain string?
fn accepts_non_string(spec: &Value) -> bool {
    ["array", "object", "integer", "number", "boolean"]
        .iter()
        .any(|k| accepts_kind(spec, k))
}

/// Whether `spec` allows `kind`, looking through a `oneOf` union.
fn accepts_kind(spec: &Value, kind: &str) -> bool {
    if spec["type"] == json!(kind) {
        return true;
    }
    spec["oneOf"]
        .as_array()
        .map(|alts| alts.iter().any(|a| a["type"] == json!(kind)))
        .unwrap_or(false)
}

/// The `tools/list` payload: every advertised tool's JSON Schema, with an
/// optional `project` property injected into all but `list_projects`.
pub fn tool_list() -> Value {
    let mut tools = raw_tools();
    let project_prop = json!({
        "type": "string",
        "description": "Optional: name of another indexed project to query (see list_projects). Default: the project this server was started for.",
    });
    for t in tools.as_array_mut().expect("raw_tools is an array") {
        if t["name"] == json!("list_projects") {
            continue;
        }
        t["inputSchema"]["properties"]["project"] = project_prop.clone();
    }
    tools
}

fn raw_tools() -> Value {
    json!([
        {
            "name": "search",
            "description": "PRIMARY KNOWLEDGE-BASE SEARCH for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change. Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via traverse / find_usages. Trigger phrases include: 'how does X work', 'where is X', 'what is X', 'find / show me code for X', 'explain X', 'is there a function that...', 'how is X implemented', 'before I change X look up...', 'context on X', or any question whose answer likely lives in the repo. Prefer calling this once with a focused natural-language query over guessing file paths. Internals: RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query. Be specific — name the concept, function, or behavior you're after (e.g. 'how does the embedder probe its dim' beats 'embedder')." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many context items to return (default 8). Bump to 15-20 when surveying a subsystem; keep 5-8 when answering a focused question." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. Leave unset for the default mix." },
                    "direction": { "type": "string", "enum": ["outbound", "inbound", "both"], "description": "Edge direction during the walk (default 'both'). Use 'inbound' when you care about who depends on the seed; 'outbound' for what the seed depends on." },
                    "maxChars": { "type": "integer", "minimum": 100, "maximum": 200000, "description": "Approximate character budget for assembled context (default ~16k). Lower it when you only need a sketch." },
                    "whereClause": { "type": "string", "description": "Optional SQL WHERE applied during seed search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\"." },
                    "includeSnippets": { "type": "boolean", "description": "Read source slice for each item (default true). Set false when you only need IDs and locations for a follow-up traversal." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "semantic_search",
            "description": "Lightweight pure-vector lookup over the knowledge base — no graph expansion, no snippet read, no PPR. Returns the top-k nearest nodes with id/name/type/file/lines/description/distance. Use this when search would be overkill: (a) quick disambiguation ('which node is the user talking about?'), (b) candidate generation before a deeper traverse, (c) filtered lookups via whereClause (e.g. only Functions in a given folder). Cheaper and faster than search. Switch to search when you need actual code snippets or graph-aware ranking.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 100, "description": "How many candidate nodes to return (default 10)." },
                    "whereClause": { "type": "string", "description": "Optional SQL WHERE filter applied to the vector search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\", \"node_type IN ('Class','Interface')\"." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "traverse",
            "description": "Walk the graph N hops from given seed node ids. The natural follow-up to search / semantic_search: take a node id you got back, expand outward to see what it imports, calls, contains, or extends. Filters by edge type and direction. Use 'outbound' to see what the seed depends on; 'inbound' to see who depends on the seed. Output is grouped by hop, with an edge-type tally, so the structure is easy to scan. Reads the structural graph directly — no database or embedding backend needed, so it keeps working when search does not.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Seed node id(s) — one id or an array of up to 10, typically copied from a prior search / find_symbols result. (`startNodeIds` is the deprecated legacy name for the same parameter.)" },
                    "hops": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Hop radius (default 2). Use 1 for direct neighbors only." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references. See graph_schema for what this graph has." },
                    "direction": { "type": "string", "enum": ["outbound", "inbound", "both"], "description": "Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on; 'both' = either." }
                },
                "required": ["nodeId"]
            }
        },
        {
            "name": "find_usages",
            "description": "Find inbound references to a node — i.e. callers of a function, importers of a module, subclasses of a class, or anything else pointing at the node. Convenience wrapper over traverse with direction='inbound' and a sensible default edge-type set ['calls', 'references', 'imports', 'extends', 'implements']. Use this when the user asks 'who uses X', 'what calls X', 'where is X imported', 'what would break if I change X', or before a refactor. Batch-friendly: pass an ARRAY of up to 10 nodeIds to check them all in one call (e.g. every symbol a refactor touches).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "The node id (or an array of up to 10 ids — batch related lookups into ONE call instead of several) to look up usages for. Get ids from search or find_symbols." },
                    "hops": { "type": "integer", "minimum": 1, "maximum": 3, "description": "How many hops out to walk (default 1 = direct callers only). Bump to 2 to catch transitive usages." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Override the default ['calls', 'references', 'imports', 'extends', 'implements'] set if you only care about a subset (e.g. ['calls'])." }
                },
                "required": ["nodeId"]
            }
        },
        {
            "name": "find_symbols",
            "description": "EXACT-NAME symbol lookup — no embeddings, no fuzziness beyond substring. Use this instead of search whenever you already know (part of) an identifier: a function, class, interface, or file the user named, an id you saw in a stack trace, a symbol you are about to edit. Direct nodeId lookup is also supported: if you already have a nodeId from a prior search, pass it for O(1) direct access instead of re-searching. Matches case-insensitively against node names, ranked exact > prefix > substring. Returns id/type/file:line for each hit — feed the id straight into get_code (source), find_usages (callers), or traverse (dependencies). Cheaper and more precise than vector search for known names; fall back to search when you only know the concept, not the name. Batch-friendly: pass an ARRAY of up to 10 names/nodeIds to resolve them all in one call. Set includeDocs to also match docstring text — a keyword scan that finds symbols described by a word they do not contain in their name. Docstring hits rank below every name hit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Direct node id lookup — O(1) access when you already have the id from a prior search. Use instead of 'name' to skip the search step." },
                    "name": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Identifier to look up, e.g. 'resolveDbAndRoot' or a fragment like 'resolve'. Pass an array of up to 10 names to resolve several symbols in ONE call (e.g. every function you're about to edit)." },
                    "nodeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict to node types (case-insensitive). Common: Function, Class, Interface, File, Concept." },
                    "filePrefix": { "type": "string", "description": "Only symbols whose file path starts with this repo-relative prefix, e.g. 'src/auth/'." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Max hits to return (default 20)." },
                    "includeDocs": { "type": "boolean", "description": "Also match docstrings, not just names (default false). Use when the concept may be described in prose rather than named — e.g. \"cache invalidation\" when the function is called `drop_stale`. Docstring hits rank below all name hits." }
                }
            }
        },
        {
            "name": "file_outline",
            "description": "List every indexed symbol in one file, in line order — a structural table of contents. Use before opening or editing a file to know what's in it, or to map a file the user mentioned. Direct nodeId lookup is also supported: if you already have a File node id from a prior search, pass it for O(1) direct access. Accepts a repo-relative path or a unique suffix (e.g. just the basename), a File node id ('file:native/src/main.rs'), or an ARRAY of up to 10 files/ids to outline them all in one call. Returns name/type/line-range/id per symbol; ids feed get_code / find_usages / traverse.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Direct File node id lookup — O(1) access when you already have the File node id from a prior search. Use instead of 'file' to skip the file lookup step." },
                    "file": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Repo-relative path ('native/src/main.rs'), unique suffix ('main.rs'), or a File node id ('file:native/src/main.rs'). Pass an array of up to 10 files to outline several in ONE call." }
                }
            }
        },
        {
            "name": "get_code",
            "description": "Read the full source for a node id or an arbitrary file/line range from the indexed repo. THE follow-up to every other tool: search previews truncate at ~1200 chars and traverse/find_usages return no code at all — call this to see the real implementation before reasoning about it or editing it. Pass a nodeId from any prior result — or an ARRAY of up to 10 ids to read several symbols in one call instead of several calls — or file (+ optional startLine/endLine) for raw ranges. Reads from the indexed repo root, so it works even when you have no direct file access (e.g. Claude Desktop).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Node id from find_symbols / search / file_outline / traverse — reads exactly that symbol's line range. Pass an array of up to 10 ids to read several symbols in ONE call (per-symbol maxChars still applies)." },
                    "file": { "type": "string", "description": "Repo-relative file path. Used when nodeId is not given (or to read outside any symbol)." },
                    "startLine": { "type": "integer", "minimum": 1, "description": "1-based first line (with file; default 1)." },
                    "endLine": { "type": "integer", "minimum": 1, "description": "1-based last line, inclusive (with file; default EOF)." },
                    "maxChars": { "type": "integer", "minimum": 200, "maximum": 200000, "description": "Character cap on returned code (default 20000). Output notes truncation." }
                }
            }
        },
        {
            "name": "project_overview",
            "description": "Orient yourself in the indexed codebase in one call: repo root, node/edge counts by type, the biggest files by symbol count, and the most depended-upon symbols (highest inbound degree, ignoring folder-containment edges). Call this FIRST in a new session, or when the user asks 'what is this project', 'how is it structured', 'where should I start'. The listed hotspot ids are good seeds for traverse / get_code.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "shortest_path",
            "description": "How are two symbols connected? Finds the shortest directed edge path between two node ids — use it to answer 'does A reach B', 'how does the request get from the route to the db call', or to check whether an edit to A can affect B. Edges are directed (imports/calls/contains flow source→target); if no forward path exists the reverse direction is tried and labeled as such. Get ids from find_symbols or search first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sourceId": { "type": "string", "description": "Start node id." },
                    "targetId": { "type": "string", "description": "End node id." }
                },
                "required": ["sourceId", "targetId"]
            }
        },
        {
            "name": "code_query",
            "description": format!(
                "WHOLE-REPO STATISTICS over the indexed graph — counts, groups, distributions and blast radius. Use this for ANY question of the form 'how many', 'which are the biggest / longest / most depended-upon', 'what fraction', 'where is the worst X', 'what breaks if I change Y'. NEVER grep for a count and NEVER loop a per-file tool to build one: this answers in one call and ~100 tokens what reading the repo costs hundreds of thousands. Two ways to call it. (1) `preset` — a named question, the cheap path, e.g. {{\"preset\": \"long_functions\"}} or {{\"preset\": \"impact\", \"args\": {{\"target\": \"src/auth.ts\"}}}}. Available: {presets}. (2) `gql` — a raw OverGraph GQL (Cypher-shaped) query when no preset fits, e.g. \"MATCH (n:Function) WHERE n.loc > 50 AND n.is_test = 0 RETURN n.folder AS folder, count(*) AS c ORDER BY c DESC\". Queryable properties: node_type, name, file, folder, loc, params, max_nesting, has_doc, is_test, in_degree, out_degree, qualified_name, route, annotations, start_line, end_line — call graph_schema for their live population counts before relying on one. Booleans are stored as 0/1 so they can be summed: documented fraction is sum(n.has_doc)/count(*). Read-only; it cannot modify the index. Every answer states its coverage denominators — treat a 'NOT INDEXED' warning as meaning the number is about nothing.",
                presets = ultragraph::code_query::presets::all()
                    .iter()
                    .map(|p| p.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "preset": { "type": "string", "description": "Name of a built-in question to run. Cheapest path — prefer this over writing GQL. See the description for the list, or call graph_schema." },
                    "gql": { "type": "string", "description": "Raw OverGraph GQL, when no preset fits. Aggregates: count, sum, avg, min, max, collect (no percentile — a collect() column is summarised as p50/p90/p99 in the output). Supports CASE, WITH … WHERE as HAVING, EXISTS { … } (needs its own RETURN clause inside), UNION, STARTS WITH / ENDS WITH / CONTAINS, and bounded variable-length paths. Every variable-length path needs a finite bound (*1..3, never *) and unanchored walks past 2 hops can exceed the traversal cap. Parenthesise negated membership: NOT (x IN [...])." },
                    "args": { "type": "object", "description": "Arguments for a preset, e.g. {\"target\": \"src/auth.ts\"} or {\"min_loc\": 100}. An argument the preset does not declare is an error, not an ignored key." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "How many rows to display (default 20). Shorthand for range \"1-N\"." },
                    "range": { "type": "string", "description": "Which window of rows to show, 1-based and inclusive at both ends: \"20\" (top 20), \"11-35\", \"34-end\". Use this to page through a result you already ran instead of re-running with a bigger limit and re-reading rows you have seen — the window is applied to rows the query already produced, so every reported total stays the same. The output states which rows it is showing and names the exact range to ask for next." }
                }
            }
        },
        {
            "name": "graph_schema",
            "description": "The capability manifest for this project's graph, and the one call to make before any filtered or statistical query. Returns: node & edge types actually present, with counts and what each edge type connects (e.g. Calls: Function→Function); the full edge-type vocabulary indexers can emit; the properties code_query can filter and aggregate on, each with how many nodes actually carry it; and every available code_query preset. Filtering on a type the graph doesn't contain, or aggregating over a property nothing carries, returns a confident zero rather than an error — this call is how you avoid both. Edges are directed (Calls A→B means A calls B); Contains is pure structure (Folder→File→Symbol), exclude it when you mean 'depends on'.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_projects",
            "description": "List every indexed project on this machine (name, repo path, graph size). Every other tool accepts project: '<name>' to query one of these instead of the current project — use this to work across repos (e.g. a service in one repo calling an API defined in another) or when the user mentions a codebase that isn't the current directory.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "regen",
            "description": "Re-run the whole pipeline (index → graph → embed) for the current (or named) project — the same thing `ug gen` does, which is why it is `regen`. Call it when tool outputs carry an \"Index may be stale\" warning, when the user says results look outdated, or after you (or they) changed many files. Incremental — unchanged files are skipped via content hashes — but embedding changed nodes needs the embedding backend, so it can take a while on big diffs; the structural tools are refreshed even if embedding fails. Accepts its former name `reindex` too.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_a_stringified_id_array() {
        // The exact shape models keep sending for a `string | array` param.
        let mut args = json!({
            "nodeId": "[\"function:native/src/storage/embed.rs:265:RemoteEmbedder::embed\"]"
        });
        normalize_args("find_usages", &mut args);
        assert_eq!(
            args["nodeId"],
            json!(["function:native/src/storage/embed.rs:265:RemoteEmbedder::embed"])
        );
    }

    #[test]
    fn leaves_a_plain_id_alone() {
        let mut args = json!({ "nodeId": "function:src/a.rs:1:foo" });
        normalize_args("get_code", &mut args);
        assert_eq!(args["nodeId"], json!("function:src/a.rs:1:foo"));
    }

    #[test]
    fn coerces_stringified_numbers_only_where_the_schema_wants_one() {
        let mut args = json!({ "nodeId": "42", "hops": "2" });
        normalize_args("find_usages", &mut args);
        // `hops` is an integer in the schema…
        assert_eq!(args["hops"], json!(2));
        // …but a numeric-looking id is still an id.
        assert_eq!(args["nodeId"], json!("42"));
    }

    #[test]
    fn unknown_tools_and_params_pass_through_untouched() {
        let mut args = json!({ "nodeId": "[\"x\"]" });
        normalize_args("not_a_tool", &mut args);
        assert_eq!(args["nodeId"], json!("[\"x\"]"));

        let mut args2 = json!({ "mystery": "[1,2]" });
        normalize_args("find_usages", &mut args2);
        assert_eq!(args2["mystery"], json!("[1,2]"), "no schema, no rewrite");
    }

    #[test]
    fn aliased_tool_names_resolve_to_the_same_schema() {
        let mut args = json!({ "nodeId": "[\"a\",\"b\"]" });
        normalize_args("find_symbol", &mut args);   // alias of find_symbols
        assert_eq!(args["nodeId"], json!(["a", "b"]));
    }

    #[test]
    fn advertises_every_named_tool() {
        let list = tool_list();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), TOOL_NAMES.len());
        let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
    }

    #[test]
    fn injects_project_everywhere_but_list_projects() {
        let list = tool_list();
        for t in list.as_array().unwrap() {
            let has_project = t["inputSchema"]["properties"].get("project").is_some();
            if t["name"] == "list_projects" {
                assert!(!has_project, "list_projects must not take a project arg");
            } else {
                assert!(has_project, "{} should take a project arg", t["name"]);
            }
        }
    }

    /// Renaming an advertised tool breaks any agent holding a cached tool
    /// list, so the old name has to keep resolving. This is the check that
    /// the alias was not forgotten when the rename happened.
    #[test]
    fn reindex_still_resolves_after_the_rename_to_regen() {
        assert_eq!(canonical_tool_name("reindex"), "regen");
        assert!(is_known_tool("regen"));
        assert!(TOOL_NAMES.contains(&"regen"));
        assert!(!TOOL_NAMES.contains(&"reindex"), "only the new name is advertised");
    }

    /// The denylist is matched against *canonical* names, so it had to be
    /// renamed alongside the tool — otherwise the chat model regains the
    /// ability to kick off a full re-index mid-answer.
    #[test]
    fn the_chat_denylist_tracks_the_rename() {
        assert!(CHAT_TOOL_DENYLIST.contains(&"regen"));
        let exposed: Vec<String> = openai_tool_schemas()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(!exposed.contains(&"regen".to_string()), "{exposed:?}");
    }

    #[test]
    fn aliases_map_to_canonical_names() {
        assert_eq!(canonical_tool_name("search_kb"), "search");
        assert_eq!(canonical_tool_name("hybrid_search"), "search");
        assert_eq!(canonical_tool_name("graph_path"), "shortest_path");
        assert_eq!(canonical_tool_name("list"), "list_projects");
        assert_eq!(canonical_tool_name("graph_search"), "find_symbols");
        assert_eq!(canonical_tool_name("get_code"), "get_code");
    }

    #[test]
    fn graph_search_alias_defaults_include_docs() {
        assert_eq!(alias_defaults("graph_search"), Some(json!({ "includeDocs": true })));
        assert_eq!(alias_defaults("find_symbols"), None);
    }

    #[test]
    fn ping_embedder_known_but_unlisted() {
        assert!(is_known_tool("ping_embedder"));
        assert!(!TOOL_NAMES.contains(&"ping_embedder"));
        assert!(is_known_tool("search"));
        assert!(!is_known_tool("nonsense"));
    }
}
