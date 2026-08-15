//! MCP tool registry: the JSON Schema advertised over `tools/list`, plus
//! the hidden tools the dispatcher honours.
//!
//! Every tool has exactly one name. Alternate spellings used to be
//! accepted so an agent holding a cached tool list would not break; with
//! nothing published yet they only bought a second name to document, test
//! and keep behaving identically, so they are gone.
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
    "analyze",
    "graph_schema",
    "list_projects",
    "gen",
];

/// Handled by the dispatcher but deliberately absent from `tools/list` —
/// operator diagnostics that would only waste an agent's tool call. Still
/// invocable through `ug mcp call` for debugging.
pub fn is_unlisted_tool(name: &str) -> bool {
    name == "ping_embedder"
}

pub fn is_known_tool(canonical: &str) -> bool {
    TOOL_NAMES.contains(&canonical) || is_unlisted_tool(canonical)
}

pub const CHAT_TOOL_DENYLIST: &[&str] = &["gen", "list_projects"];

/// Tools the chat and tour dispatchers answer from the open store, in their
/// own match arms. Everything else advertised falls through to
/// `agent_tools::run_tool`, which reads graph.json — so a tool that is in
/// neither place is one the model can call and nothing can run.
pub const STORE_BACKED_CHAT_TOOLS: &[&str] = &["search", "semantic_search", "analyze"];

/// Guard for the chat dispatchers' graph.json fall-through.
///
/// Reaching `agent_tools::run_tool` with a store-backed name means the tool is
/// advertised but has no arm to run it. Saying so beats that function's
/// "Unknown agent tool", which sends the reader looking for a missing *graph*
/// tool — the wrong hunt, and how `analyze` stayed broken in chat.
pub fn reject_if_store_backed(name: &str) -> Result<(), String> {
    if STORE_BACKED_CHAT_TOOLS.contains(&name) {
        return Err(format!(
            "{} needs the indexed store, but this dispatcher has no arm for it.",
            name
        ));
    }
    Ok(())
}

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

/// The wildcard dialect, interpolated into every tool description that
/// accepts one.
///
/// Models copy syntax from the description they are reading, so each tool has
/// to carry it — but it must be the *same* text everywhere, which is why it
/// comes from the matcher's own crate rather than being retyped here.
const WILDCARD_SYNTAX: &str = ultragraph::pattern::SYNTAX_SUMMARY;

/// The three shapes an id-taking parameter accepts, for the tools whose
/// `nodeId` no longer means "id only".
const NODE_REF_FORMS: &str = "Accepts a node id, a plain symbol name, or a wildcard pattern — a name or pattern expands to every symbol it matches (capped, and the cap is reported when hit), so you can act on a whole family without looking ids up first.";

/// Preset list for the tool description, each with the arguments it takes —
/// `long_functions(min_loc)`. Naming the arguments is what stops a model
/// inventing them, or borrowing `limit` from the wrong level.
fn preset_signatures() -> String {
    ultragraph::analyze::presets::all()
        .iter()
        .map(|p| {
            if p.params.is_empty() {
                p.name.to_string()
            } else {
                let params: Vec<&str> = p.params.iter().map(|q| q.name).collect();
                format!("{}({})", p.name, params.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Lift `analyze`'s own parameters out of `args`, where models keep
/// putting them. A misfiled `limit` is a well-formed intention expressed in
/// the wrong shape; rejecting it costs a round trip and teaches nothing.
/// An explicit top-level value always wins — that one was deliberate.
fn hoist_own_params(args: &mut Value) {
    let Some(obj) = args.as_object_mut() else { return };
    let Some(nested) = obj.get_mut("args").and_then(|v| v.as_object_mut()) else {
        return;
    };
    let mut lifted: Vec<(String, Value)> = Vec::new();
    for name in ultragraph::analyze::OWN_PARAMS {
        if let Some(v) = nested.remove(*name) {
            lifted.push(((*name).to_string(), v));
        }
    }
    for (name, value) in lifted {
        obj.entry(name).or_insert(value);
    }
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
    let canonical = tool;
    if canonical == "analyze" {
        hoist_own_params(args);
    }
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
            "description": "PRIMARY KNOWLEDGE-BASE SEARCH for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change. Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via traverse / find_usages. Trigger phrases include: 'how does X work', 'where is X', 'what is X', 'find / show me code for X', 'explain X', 'is there a function that...', 'how is X implemented', 'before I change X look up...', 'context on X', or any question whose answer likely lives in the repo. Prefer calling this once with a focused natural-language query over guessing file paths. Two questions this is the WRONG tool for: a name you already know (use find_symbols — exact, no embeddings) and a family of symbols or files ('all the handlers', 'every *Controller', 'everything under src/auth/') — those are one find_symbols / file_outline / find_usages call with a WILDCARD, which is exact and cheaper than ranking. Internals: RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query. Be specific — name the concept, function, or behavior you're after (e.g. 'how does the embedder probe its dim' beats 'embedder')." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many context items to return (default 8). Bump to 15-20 when surveying a subsystem; keep 5-8 when answering a focused question." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references, instantiates, uses, overrides. Leave unset for the default mix." },
                    "direction": { "type": "string", "enum": ["outbound", "inbound", "both"], "description": "Edge direction during the walk (default 'both'). Use 'inbound' when you care about who depends on the seed; 'outbound' for what the seed depends on." },
                    "maxChars": { "type": "integer", "minimum": 100, "maximum": 200000, "description": "Approximate character budget for assembled context (default ~16k). Lower it when you only need a sketch." },
                    "whereClause": { "type": "string", "description": "Optional SQL WHERE applied during seed search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\"." },
                    "includeSnippets": { "type": "boolean", "description": "Read a source slice for each item (default false — returns lean ids+locations; set true when you want the code inline rather than a follow-up get_code)." }
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
            "description": format!(
                "Walk the graph N hops from given seed symbols. The natural follow-up to search / semantic_search: take a node id you got back, expand outward to see what it imports, calls, contains, or extends. {refs} Several seeds make ONE merged walk, so a pattern like 'handle_*' traces everything reachable from a whole family in a single call. Filters by edge type and direction: 'outbound' is what the seed depends on, 'inbound' is who depends on the seed. Output is grouped by hop, with an edge-type tally, so the structure is easy to scan. Reads the structural graph directly — no database or embedding backend needed, so it keeps working when search does not.",
                refs = NODE_REF_FORMS
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": format!("Seed(s) — one value or an array of up to 10. {refs} Ids typically come from a prior search / find_symbols result. (`startNodeIds` is the deprecated legacy name for the same parameter.)", refs = NODE_REF_FORMS) },
                    "hops": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Hop radius (default 2). Use 1 for direct neighbors only." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references, instantiates, uses, overrides. See graph_schema for what this graph has." },
                    "direction": { "type": "string", "enum": ["outbound", "inbound", "both"], "description": "Edge direction (default 'outbound'). 'inbound' = who depends on me; 'outbound' = what I depend on; 'both' = either." }
                },
                "required": ["nodeId"]
            }
        },
        {
            "name": "find_usages",
            "description": format!(
                "Find inbound references to a symbol — callers of a function, importers of a module, subclasses of a class, or anything else pointing at it, with the call-site lines as evidence. Convenience wrapper over traverse with direction='inbound' and a sensible default edge-type set ['calls', 'references', 'imports', 'extends', 'implements', 'overrides', 'instantiates', 'uses']. Use this when the user asks 'who uses X', 'what calls X', 'where is X imported', 'what would break if I change X', or before a refactor. {refs} So the blast radius of a whole family is one call: {{\"nodeId\": \"validate_*\"}}. Batch-friendly: pass an ARRAY of up to 10 values to check them all in one call (e.g. every symbol a refactor touches).",
                refs = NODE_REF_FORMS
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": format!("What to look up usages for — one value or an array of up to 10 (batch related lookups into ONE call instead of several). {refs}", refs = NODE_REF_FORMS) },
                    "hops": { "type": "integer", "minimum": 1, "maximum": 3, "description": "How many hops out to walk (default 1 = direct callers only). Bump to 2 to catch transitive usages." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Override the default ['calls', 'references', 'imports', 'extends', 'implements'] set if you only care about a subset (e.g. ['calls'])." }
                },
                "required": ["nodeId"]
            }
        },
        {
            "name": "find_symbols",
            "description": format!(
                "NAME-BASED symbol lookup — no embeddings. Use this instead of search whenever you know (part of) an identifier: a function, class, interface or file the user named, a name from a stack trace, a symbol you are about to edit. Three ways to ask, all case-insensitive: (1) a plain fragment — ranked exact > prefix > substring, e.g. 'resolve' finds resolveDbAndRoot; (2) a WILDCARD pattern — {wildcards}, matched against the WHOLE name, e.g. 'handle_*' for every handler, '*Controller' for every controller class, '{{get,set}}_*' for accessors, '*' with filePrefix to list a whole directory; (3) a nodeId you already have, for O(1) lookup with no search at all. Returns id/type/file:line per hit — feed the id straight into get_code (source), find_usages (callers) or traverse (dependencies), all of which also accept the same names and patterns directly. Batch-friendly: pass an ARRAY of up to 10 names/patterns/ids to resolve them in ONE call. Set includeDocs to also scan docstring prose (matched anywhere, not whole-string); docstring hits rank below every name hit. A wildcard here is the cheap way to enumerate a family of symbols — prefer it over repeated calls or a grep.",
                wildcards = WILDCARD_SYNTAX
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Direct node id lookup — O(1) access when you already have the id from a prior search. Use instead of 'name' to skip the search step." },
                    "name": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": format!("Identifier, fragment, or wildcard pattern. A fragment ('resolve') is ranked exact > prefix > substring; a pattern ({wildcards}) must match the whole name, so use '*auth*' to match anywhere. Pass an array of up to 10 to resolve several in ONE call.", wildcards = WILDCARD_SYNTAX) },
                    "nodeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict to node types (case-insensitive, wildcards allowed). Common: Function, Class, Interface, Variable, File, Concept — call graph_schema for what this graph actually has." },
                    "filePrefix": { "type": "string", "description": "Only symbols under this repo-relative path. A plain string is a prefix ('src/auth/'); a glob is matched against the whole path ('src/**/*.ts'), where '*' stops at '/' and '**/' crosses directories. Combine with name '*' to list everything in a subtree." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Max hits per query (default 20). The result states the true total, so raise this rather than re-querying when it says more exist." },
                    "includeDocs": { "type": "boolean", "description": "Also match docstrings, not just names (default false). Use when the concept may be described in prose rather than named — e.g. \"cache invalidation\" when the function is called `drop_stale`. Docstring hits rank below all name hits." },
                    "boundary": { "type": "boolean", "description": "Keep only system boundaries — REST handlers, queue listeners, CLI commands, scheduled jobs, outbound HTTP/DB/queue clients (default false). Combine with name '*' (or omit name entirely) to list the whole surface a service exposes and consumes, which is the fastest way to orient in an unfamiliar repo. Each hit's `boundary` field names the kind and the surface, e.g. 'in:http.endpoint GET /api/orders/{id}'." }
                }
            }
        },
        {
            "name": "file_outline",
            "description": format!(
                "List every indexed symbol in a file, in line order — a structural table of contents. Use before opening or editing a file to know what's in it, or to map a file the user mentioned. Accepts a repo-relative path, a unique suffix (just the basename), a File node id ('file:native/src/main.rs'), a PATH GLOB, or an ARRAY of up to 10 of those to outline them all in one call. A glob ({wildcards}) is matched against the whole repo-relative path, where '*' stops at '/' and '**/' crosses directories — 'src/api/*.ts' outlines one directory, 'src/**/*.{{ts,tsx}}' a whole subtree, '**/test_*.py' every file following a naming convention. That is the cheap way to survey unfamiliar code: one call instead of one per file. Globs outline up to maxFiles files and then list the remaining paths by name. Returns name/type/line-range/id per symbol; ids feed get_code / find_usages / traverse.",
                wildcards = WILDCARD_SYNTAX
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Direct File node id lookup — O(1) access when you already have the File node id from a prior search. Use instead of 'file' to skip the file lookup step." },
                    "file": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Repo-relative path ('native/src/main.rs'), unique suffix ('main.rs'), File node id ('file:native/src/main.rs'), or a path glob ('src/**/*.ts'). Pass an array of up to 10 to outline several in ONE call." },
                    "maxFiles": { "type": "integer", "minimum": 1, "maximum": 200, "description": "How many files a single glob may outline (default 20). Beyond the cap the extra paths are listed by name instead of expanded, so nothing is hidden — raise this or narrow the glob." }
                }
            }
        },
        {
            "name": "get_code",
            "description": format!(
                "Read the full source for a symbol, or an arbitrary file/line range, from the indexed repo. THE follow-up to every other tool: search previews truncate at ~1200 chars and traverse/find_usages return no code at all — call this to see the real implementation before reasoning about it or editing it. {refs} So 'render_*' reads every renderer in one call. Or pass an ARRAY of up to 10 values, or file (+ optional startLine/endLine) for raw ranges. Reads from the index, so it works even when you have no direct file access (e.g. Claude Desktop) and flags any slice whose file changed since indexing.",
                refs = NODE_REF_FORMS
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": format!("What to read — reads exactly that symbol's line range. {refs} Ids come from find_symbols / search / file_outline / traverse. Pass an array of up to 10 to read several in ONE call (per-symbol maxChars still applies).", refs = NODE_REF_FORMS) },
                    "file": { "type": "string", "description": "Repo-relative file path. Used when nodeId is not given (or to read outside any symbol)." },
                    "startLine": { "type": "integer", "minimum": 1, "description": "1-based first line (with file; default 1)." },
                    "endLine": { "type": "integer", "minimum": 1, "description": "1-based last line, inclusive (with file; default EOF)." },
                    "range": { "type": "string", "description": "The line window as one value, in the same dialect analyze uses for rows: \"11-35\" (closed, inclusive both ends), \"34-end\" (open), \"20\" (the first 20 lines). Use it to page through a long file — ask for the next window rather than re-reading from line 1 with a bigger endLine. startLine/endLine win if you send both." },
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
            "description": "How are two symbols connected? Finds the shortest directed edge path between them — use it to answer 'does A reach B', 'how does the request get from the route to the db call', or to check whether an edit to A can affect B. Each endpoint takes a node id, an exact symbol name, or a wildcard, but must resolve to EXACTLY ONE node (the answer differs per candidate); when it doesn't, the error lists the ids to choose from. Edges are directed (imports/calls/contains flow source→target); if no forward path exists the reverse direction is tried and labeled as such.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sourceId": { "type": "string", "description": "Start point: a node id, an exact symbol name, or a wildcard matching exactly one symbol." },
                    "targetId": { "type": "string", "description": "End point: a node id, an exact symbol name, or a wildcard matching exactly one symbol." }
                },
                "required": ["sourceId", "targetId"]
            }
        },
        {
            "name": "analyze",
            "description": format!(
                "WHOLE-REPO STATISTICS over the indexed graph — counts, groups, distributions and blast radius. Use this for ANY question of the form 'how many', 'which are the biggest / longest / most depended-upon', 'what fraction', 'where is the worst X', 'what breaks if I change Y'. NEVER grep for a count and NEVER loop a per-file tool to build one: this answers in one call and ~100 tokens what reading the repo costs hundreds of thousands. Two ways to call it. (1) `preset` — a named question, the cheap path, e.g. {{\"preset\": \"long_functions\"}} or {{\"preset\": \"impact\", \"args\": {{\"target\": \"src/auth.ts\"}}}}. Available: {presets}. (2) `gql` — a raw OverGraph GQL (Cypher-shaped) query when no preset fits, e.g. \"MATCH (n:Function) WHERE n.loc > 50 AND n.is_test = 0 RETURN n.folder AS folder, count(*) AS c ORDER BY c DESC\". Queryable properties: node_type, name, file, folder, loc, params, max_nesting, has_doc, is_test, in_degree, out_degree, qualified_name, route, annotations, start_line, end_line, boundary, boundary_in, boundary_out, boundary_kinds, boundary_protocols, boundary_detail — call graph_schema for their live population counts before relying on one. A *boundary* is where the system meets the outside world (a REST handler, a queue listener, a CLI command, an outbound HTTP or DB client); `boundary_impact` is the blast-radius question that matters before a change, because it reports which externally-visible contracts a change reaches rather than merely how many symbols move. Booleans are stored as 0/1 so they can be summed: documented fraction is sum(n.has_doc)/count(*). Read-only; it cannot modify the index. Every answer states its coverage denominators — treat a 'NOT INDEXED' warning as meaning the number is about nothing.",
                presets = preset_signatures()
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "preset": { "type": "string", "description": "Name of a built-in question to run. Cheapest path — prefer this over writing GQL. See the description for the list, or call graph_schema." },
                    "gql": { "type": "string", "description": "Raw OverGraph GQL, when no preset fits. Aggregates: count, sum, avg, min, max, collect (no percentile — a collect() column is summarised as p50/p90/p99 in the output). Supports CASE, WITH … WHERE as HAVING, EXISTS { … } (needs its own RETURN clause inside), UNION, STARTS WITH / ENDS WITH / CONTAINS, and bounded variable-length paths. Every variable-length path needs a finite bound (*1..3, never *) and unanchored walks past 2 hops can exceed the traversal cap. Parenthesise negated membership: NOT (x IN [...])." },
                    "args": { "type": "object", "description": "Arguments for the chosen preset ONLY — the names in its signature above, e.g. {\"target\": \"src/auth.ts\"} or {\"min_loc\": 100}. Paging is not a preset argument: `limit` and `range` are top-level parameters, siblings of `preset`, never keys in here. An argument the preset does not declare is an error, not an ignored key." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "How many rows to display (default 20). Shorthand for range \"1-N\"." },
                    "range": { "type": "string", "description": "Which window of rows to show, 1-based and inclusive at both ends: \"20\" (top 20), \"11-35\", \"34-end\". Use this to page through a result you already ran instead of re-running with a bigger limit and re-reading rows you have seen — the window is applied to rows the query already produced, so every reported total stays the same. The output states which rows it is showing and names the exact range to ask for next." }
                }
            }
        },
        {
            "name": "graph_schema",
            "description": "The capability manifest for this project's graph, and the one call to make before any filtered or statistical query. Returns: node & edge types actually present, with counts and what each edge type connects (e.g. Calls: Function→Function); the full edge-type vocabulary indexers can emit; the properties analyze can filter and aggregate on, each with how many nodes actually carry it; and every available analyze preset. Filtering on a type the graph doesn't contain, or aggregating over a property nothing carries, returns a confident zero rather than an error — this call is how you avoid both. Edges are directed (Calls A→B means A calls B); Contains is pure structure (Folder→File→Symbol), exclude it when you mean 'depends on'.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_projects",
            "description": "List every indexed project on this machine (name, repo path, graph size). Every other tool accepts project: '<name>' to query one of these instead of the current project — use this to work across repos (e.g. a service in one repo calling an API defined in another) or when the user mentions a codebase that isn't the current directory.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "gen",
            "description": "Re-run the whole pipeline (index → graph → embed) for the current (or named) project — the same thing `ug gen` does in the CLI, which is why the tool shares its name. Call it when tool outputs carry an \"Index may be stale\" warning, when the user says results look outdated, or after you (or they) changed many files. Incremental — unchanged files are skipped via content hashes — but embedding changed nodes needs the embedding backend, so it can take a while on big diffs; the structural tools are refreshed even if embedding fails.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guards: `analyze` was advertised to the chat model but
    /// only the MCP dispatcher could run it, so the tab answered "Unknown
    /// agent tool 'analyze'" — a tool the model was told to call.
    #[test]
    fn every_tool_offered_to_chat_can_be_dispatched() {
        for name in TOOL_NAMES {
            if CHAT_TOOL_DENYLIST.contains(name) {
                continue;
            }
            assert!(
                STORE_BACKED_CHAT_TOOLS.contains(name)
                    || ultragraph::agent_tools::is_agent_tool(name),
                "'{}' is advertised to the chat model but no dispatcher answers it: \
                 add it to a store-backed match arm (and to STORE_BACKED_CHAT_TOOLS), \
                 to agent_tools::run_tool, or to CHAT_TOOL_DENYLIST",
                name
            );
        }
    }

    /// The denylist filters what is offered; it can only name real tools.
    #[test]
    fn advertised_schemas_match_the_canonical_list() {
        let offered: Vec<String> = openai_tool_schemas()
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(offered.iter().any(|n| n == "analyze"), "offered: {:?}", offered);
        for denied in CHAT_TOOL_DENYLIST {
            assert!(TOOL_NAMES.contains(denied), "'{}' is not a tool", denied);
            assert!(!offered.iter().any(|n| n == denied), "'{}' leaked into chat", denied);
        }
    }

    /// The reported failure: "`long_functions` does not take a `limit`
    /// parameter" — the model filed paging under `args`.
    #[test]
    fn lifts_paging_out_of_preset_args() {
        let mut args = json!({
            "preset": "long_functions",
            "args": { "min_loc": 100, "limit": 20, "range": "1-20" }
        });
        normalize_args("analyze", &mut args);
        assert_eq!(args["limit"], json!(20));
        assert_eq!(args["range"], json!("1-20"));
        assert_eq!(args["args"], json!({ "min_loc": 100 }));
    }

    /// A deliberate top-level value is not overwritten by a stray nested one.
    #[test]
    fn an_explicit_top_level_param_wins() {
        let mut args = json!({
            "preset": "dead_code",
            "limit": 50,
            "args": { "limit": 5 }
        });
        normalize_args("analyze", &mut args);
        assert_eq!(args["limit"], json!(50));
        assert_eq!(args["args"], json!({}));
    }

    /// Hoisting is only safe while no preset declares one of these names —
    /// if one ever did, its argument would be silently relocated.
    #[test]
    fn no_preset_shadows_a_query_parameter() {
        for p in ultragraph::analyze::presets::all() {
            for param in p.params {
                assert!(
                    !ultragraph::analyze::OWN_PARAMS.contains(&param.name),
                    "preset '{}' declares '{}', which hoist_own_params would steal",
                    p.name,
                    param.name
                );
            }
        }
    }

    /// Models invent argument names when the description only lists presets,
    /// so every preset that takes arguments must advertise them.
    #[test]
    fn preset_signatures_name_their_arguments() {
        let sigs = preset_signatures();
        assert!(sigs.contains("long_functions(min_loc)"), "{sigs}");
        assert!(sigs.contains("impact(target)"), "{sigs}");
        // A preset without parameters stays bare — no empty parens.
        assert!(sigs.contains("repo_census,") || sigs.ends_with("repo_census"), "{sigs}");
        assert!(!sigs.contains("()"), "{sigs}");
    }

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

    /// Coercion is driven by the tool's schema, so it only fires for a
    /// name that is actually advertised. `find_symbol` used to be an alias
    /// and is now nothing — its args come through untouched, which is the
    /// correct behaviour for an unknown tool.
    #[test]
    fn only_an_advertised_name_gets_its_args_coerced() {
        let mut args = json!({ "nodeId": "[\"a\",\"b\"]" });
        normalize_args("find_symbols", &mut args);
        assert_eq!(args["nodeId"], json!(["a", "b"]));

        let mut args = json!({ "nodeId": "[\"a\",\"b\"]" });
        normalize_args("find_symbol", &mut args);
        assert_eq!(args["nodeId"], json!("[\"a\",\"b\"]"), "no such tool, no schema, no rewrite");
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


    /// The denylist is matched against *canonical* names, so it had to be
    /// renamed alongside the tool — otherwise the chat model regains the
    /// ability to kick off a full re-index mid-answer.
    #[test]
    fn the_chat_denylist_tracks_the_rename() {
        assert!(CHAT_TOOL_DENYLIST.contains(&"gen"));
        let exposed: Vec<String> = openai_tool_schemas()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(!exposed.contains(&"gen".to_string()), "{exposed:?}");
    }

    /// With aliases gone, the only names that work are the advertised
    /// ones. A near-miss must fail rather than quietly resolving.
    #[test]
    fn an_unadvertised_name_is_not_a_tool() {
        for gone in ["reindex", "regen", "search_kb", "hybrid_search", "graph_path", "graph_search", "list"] {
            assert!(!is_known_tool(gone), "`{gone}` should no longer resolve");
        }
        for real in TOOL_NAMES {
            assert!(is_known_tool(real), "{real}");
        }
    }

    #[test]
    fn ping_embedder_known_but_unlisted() {
        assert!(is_known_tool("ping_embedder"));
        assert!(!TOOL_NAMES.contains(&"ping_embedder"));
        assert!(is_known_tool("search"));
        assert!(!is_known_tool("nonsense"));
    }
}
