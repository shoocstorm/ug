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
    "traverse",
    "find_usages",
    "find_symbols",
    "file_context",
    "get_code",
    "project_overview",
    "context",
    "shortest_path",
    "walk",
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

/// Retired tool names the dispatcher still answers.
///
/// `semantic_search` is now `search` with `expand: false` — the two took
/// the same arguments and differed only in whether graph expansion ran,
/// which made picking between them a coin flip an agent had to get right
/// on prompt text alone. The name stays callable (it is not advertised) so
/// existing agent configs, cached tool lists and transcripts keep working.
pub fn is_alias_tool(name: &str) -> bool {
    name == "semantic_search"
}

pub fn is_known_tool(canonical: &str) -> bool {
    TOOL_NAMES.contains(&canonical) || is_unlisted_tool(canonical) || is_alias_tool(canonical)
}

pub const CHAT_TOOL_DENYLIST: &[&str] = &["gen", "list_projects"];

/// Tools the chat and tour dispatchers answer from the open store, in their
/// own match arms. Everything else advertised falls through to
/// `agent_tools::run_tool`, which reads graph.json — so a tool that is in
/// neither place is one the model can call and nothing can run.
pub const STORE_BACKED_CHAT_TOOLS: &[&str] = &["search", "semantic_search", "analyze"];

/// Tools the chat dispatchers answer from `graph.json` **plus git**, in
/// their own match arm.
///
/// A third category, because `walk` is neither of the other two: it is not
/// store-backed (so it is not the store's category) and it is
/// not an `agent_tools` tool (so the graph.json fall-through cannot run
/// it). Naming the category is what keeps
/// `every_tool_offered_to_chat_can_be_dispatched` a real guard rather than
/// a list somebody widened until it passed.
pub const GIT_BACKED_CHAT_TOOLS: &[&str] = &["walk"];

/// Guard for the chat dispatchers' graph.json fall-through.
///
/// Reaching `agent_tools::run_tool` with a name that needs its own arm means
/// the tool is advertised but nothing runs it. Saying which arm is missing
/// beats that function's "Unknown agent tool", which sends the reader looking
/// for a missing *graph* tool — the wrong hunt, and how `analyze` stayed
/// broken in chat.
pub fn reject_if_not_graph_backed(name: &str) -> Result<(), String> {
    if STORE_BACKED_CHAT_TOOLS.contains(&name) {
        return Err(format!(
            "{} needs the indexed store, but this dispatcher has no arm for it.",
            name
        ));
    }
    if GIT_BACKED_CHAT_TOOLS.contains(&name) {
        return Err(format!(
            "{} needs git as well as the graph, but this dispatcher has no arm for it.",
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
/// The preset names, as a JSON Schema `enum` for `analyze.preset`.
///
/// A plain `"type": "string"` let the model write whatever a preset *might*
/// plausibly be called, and it duly did: a real transcript opens with
/// `{"preset": "boundary_kinds"}` — which is not a preset at all, it is a
/// graph *property* that was sitting in the same paragraph of the tool
/// description. An `enum` is the one part of a schema that constrained
/// decoding enforces, so under vLLM / llama.cpp grammars an invented name
/// stops being emittable rather than costing a round trip to find out.
fn preset_names() -> Vec<Value> {
    ultragraph::analyze::presets::all()
        .iter()
        .map(|p| Value::String(p.name.to_string()))
        .collect()
}

/// The preset catalogue: one `name(params) — what it answers` line each.
///
/// `Preset::description` is written for an agent choosing between presets and
/// was previously thrown away here, leaving the model to pick from 35 bare
/// names. The signature is in the same line as the description so "which
/// preset" and "what may I pass it" are one read, not two: the second
/// failure in that same transcript was `{"preset": "boundaries", "args":
/// {"kinds": [...]}}`, a parameter invented for a preset that declares none.
fn preset_catalog() -> String {
    ultragraph::analyze::presets::all()
        .iter()
        .map(|p| {
            let sig = if p.params.is_empty() {
                p.name.to_string()
            } else {
                let params: Vec<&str> = p.params.iter().map(|q| q.name).collect();
                format!("{}({})", p.name, params.join(", "))
            };
            format!("{} — {}", sig, p.description)
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    // The retired alias runs `search`'s code, so it needs `search`'s
    // coercion too — otherwise a stringified `"k": "10"` survives to
    // deserialization and fails there instead.
    let canonical = if is_alias_tool(tool) { "search" } else { tool };
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
            "description": "PRIMARY KNOWLEDGE-BASE SEARCH for this codebase. Use this whenever the user asks about anything that might exist in the indexed repository: how a feature works, where something is defined, what a symbol does, why some code exists, how modules connect, or to gather context before making a code change. Returns ranked code snippets with file:line locations, descriptions, and node IDs you can drill into via traverse / find_usages. Trigger phrases include: 'how does X work', 'where is X', 'what is X', 'find / show me code for X', 'explain X', 'is there a function that...', 'how is X implemented', 'before I change X look up...', 'context on X', or any question whose answer likely lives in the repo. Prefer calling this once with a focused natural-language query over guessing file paths. Two questions this is the WRONG tool for: a name you already know (use find_symbols — exact, no embeddings) and a family of symbols or files ('all the handlers', 'every *Controller', 'everything under src/auth/') — those are one find_symbols / file_context / find_usages call with a WILDCARD, which is exact and cheaper than ranking. Internals: RRF fuses vector + FTS hits to seed Personalized PageRank over the edge graph, so results combine semantic relevance with structural importance; set expand:false to skip that walk and get the matching nodes alone (this replaces the former semantic_search tool). Requires an embedder AND a database ingested with vectors: the FTS half is a channel inside that fusion, not a standalone mode, so unlike the `ug search` CLI (which quietly degrades to a name-substring match) this tool has NO fallback and returns an error instead. When it errors, switch to find_symbols / file_context / find_usages / traverse / analyze — none of those touch embeddings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query. Be specific — name the concept, function, or behavior you're after (e.g. 'how does the embedder probe its dim' beats 'embedder')." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many context items to return (default 8). Bump to 15-20 when surveying a subsystem; keep 5-8 when answering a focused question." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict the walk to these edge types (case-insensitive). Common: imports, calls, extends, implements, contains, references, instantiates, uses, overrides. Leave unset for the default mix." },
                    "direction": { "type": "string", "enum": ["outbound", "inbound", "both"], "description": "Edge direction during the walk (default 'both'). Use 'inbound' when you care about who depends on the seed; 'outbound' for what the seed depends on." },
                    "maxChars": { "type": "integer", "minimum": 100, "maximum": 200000, "description": "Approximate character budget for assembled context (default 60000). Lower it when you only need a sketch." },
                    "whereClause": { "type": "string", "description": "Optional SQL WHERE applied during seed search. Examples: \"node_type = 'Function'\", \"file LIKE 'src/auth/%'\"." },
                    "includeSnippets": { "type": "boolean", "description": "Read a source slice for each item (default false — returns lean ids+locations; set true when you want the code inline rather than a follow-up get_code)." },
                    "expand": { "type": "boolean", "description": "Whether results may include code the query did not match directly (default true). Leave it alone for normal questions — graph expansion is why this tool answers 'how does X work' better than grep. Set false to get ONLY the nodes that matched, no neighbors and no PPR: right for disambiguation ('which node do they mean?'), candidate generation before a traverse, and filtered inventory via whereClause. Cheaper, and the results are all seeds." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "traverse",
            "description": format!(
                "Walk the graph N hops from given seed symbols. The natural follow-up to search: take a node id you got back, expand outward to see what it imports, calls, contains, or extends. {refs} Several seeds make ONE merged walk, so a pattern like 'handle_*' traces everything reachable from a whole family in a single call. Filters by edge type and direction: 'outbound' is what the seed depends on, 'inbound' is who depends on the seed. PREFER find_usages for the inbound direction specifically — it is this same walk with call-site lines as evidence and a default edge set wide enough that constants and types are not invisible; prefer context when you want one symbol's code, callers, tests and deps before editing it. Output is grouped by hop, with an edge-type tally, and states what the edge-type and direction filters hid, so a narrowed walk cannot be mistaken for a complete one. Reads the structural graph directly — no database or embedding backend needed, so it keeps working when search does not.",
                refs = NODE_REF_FORMS
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": format!("Seed(s) — one value or an array of up to 10. {refs} Ids typically come from a prior search / find_symbols result. (`startNodeIds` is the deprecated legacy name for the same parameter.)", refs = NODE_REF_FORMS) },
                    "hops": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Hop radius (default 2). Use 1 for direct neighbors only." },
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Restrict to these edge types (case-insensitive; a comma-separated string is accepted too). Common: imports, calls, extends, implements, contains, references, instantiates, uses, overrides. See graph_schema for what this graph has. OMITTING THIS IS USUALLY RIGHT: a filter narrows the walk silently and the result still looks complete. ['calls'] alone misses a function passed as a value rather than called — languages record that as 'references' — so prefer ['calls','references'] when you mean \"what does this use\". The result reports how many edges the filter hid, and of which types." },
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
                    "edgeTypes": { "type": "array", "items": { "type": "string" }, "description": "Override the default ['calls', 'references', 'imports', 'extends', 'implements', 'overrides', 'instantiates', 'uses'] set if you only care about a subset (e.g. ['calls']). Narrowing it hides users rather than reporting none: drop 'instantiates'/'uses' and every constructed type and every read constant answers \"nobody uses this\". The full default is the safe answer for \"what breaks if I change X\". A comma-separated string is accepted too." }
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
            "name": "file_context",
            "description": format!(
                "EVERYTHING about ONE file in a single budgeted call — the symbols it declares with their doc clauses, the files that import it, what it imports, the test files that reach its symbols, the non-test files that depend on it (its blast radius), and its folder siblings. THE tool to call when you are handed a file rather than a symbol: a path from a diff, a stack trace, a traceback, or a file the user mentioned. Replaces the six calls you would otherwise spend assembling the same picture — an outline, plus find_usages, traverse, and the analyze impact / impact_summary / retest_scope presets — and does not repeat the outline the way a traverse from a File node does. Every entry says WHY it is there, so you can use the half you need and ignore the rest without a second call. Accepts a repo-relative path, a unique suffix (just the basename), a File node id ('file:native/src/main.rs'), ANY symbol id (it reports the file holding that symbol), a PATH GLOB ({wildcards}), or an ARRAY of up to 10 of those. ONE file returns the whole report; SEVERAL return each file's outline only and say so, because a budget split across several neighbourhoods would thin every one of them. Two things it deliberately does not do: it returns no source code (call get_code on the ids it hands you), and its `dependent` count excludes tests and stops at 3 hops (call analyze impact for the unfiltered figure).",
                wildcards = WILDCARD_SYNTAX
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Direct node id lookup — a File node id when you already have one, or any symbol id to get the file that holds it. Use instead of 'file' to skip the path lookup." },
                    "file": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": "Repo-relative path ('native/src/main.rs'), unique suffix ('main.rs'), File node id ('file:native/src/main.rs'), or a path glob ('src/**/*.ts'). One value gets the full report; several or a glob get outlines only." },
                    "maxChars": { "type": "integer", "minimum": 500, "description": "Total character budget for the report (default 60000). Roles are filled in priority order — outline, importer, import, test, dependent, sibling — so lowering this drops siblings and blast radius before it drops the outline. Whatever does not fit is reported as a count, never silently cut." },
                    "include": { "type": "array", "items": { "type": "string", "enum": ["outline", "importer", "import", "test", "dependent", "sibling"] }, "description": "Keep only these roles. Omit for all six. ['outline'] is the plain table of contents; ['test','dependent'] is the edit-safety pair to check before changing a file." },
                    "maxFiles": { "type": "integer", "minimum": 1, "maximum": 200, "description": "How many files a single glob may report (default 20). Beyond the cap the extra paths are listed by name instead of expanded, so nothing is hidden — raise this or narrow the glob." }
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
                    "nodeId": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 } ], "description": format!("What to read — reads exactly that symbol's line range. {refs} Ids come from find_symbols / search / file_context / traverse. Pass an array of up to 10 to read several in ONE call (per-symbol maxChars still applies).", refs = NODE_REF_FORMS) },
                    "file": { "type": "string", "description": "Repo-relative file path. Used when nodeId is not given (or to read outside any symbol)." },
                    "startLine": { "type": "integer", "minimum": 1, "description": "1-based first line (with file; default 1)." },
                    "endLine": { "type": "integer", "minimum": 1, "description": "1-based last line, inclusive (with file; default EOF)." },
                    "range": { "type": "string", "description": "The line window as one value, in the same dialect analyze uses for rows: \"11-35\" (closed, inclusive both ends), \"34-end\" (open), \"20\" (the first 20 lines). Use it to page through a long file — ask for the next window rather than re-reading from line 1 with a bigger endLine. startLine/endLine win if you send both." },
                    "maxChars": { "type": "integer", "minimum": 200, "maximum": 200000, "description": "Character cap on returned code, applied per symbol (default 60000). Output notes truncation." }
                }
            }
        },
        {
            "name": "project_overview",
            "description": "Orient yourself in the indexed codebase in one call: repo root, node/edge counts by type, the biggest files by symbol count, and the most depended-upon symbols (highest inbound degree, ignoring folder-containment edges). Call this FIRST in a new session, or when the user asks 'what is this project', 'how is it structured', 'where should I start'. The listed hotspot ids are good seeds for traverse / get_code.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "context",
            "description": format!(
                "EVERYTHING about ONE symbol in a single budgeted call — its source, its direct callers with call sites, the tests that reach it, what it depends on, and any prose linked to it. This is the tool to reach for when you are about to change a symbol, or need to understand one properly: it replaces the get_code → find_usages → traverse → analyze test_for → read-the-doc sequence you would otherwise run, in one round trip and one token budget. Every entry is labelled with the ROLE explaining why it is there ('target', 'caller', 'test', 'dependency', 'doc'), so you can ignore the half you do not need without asking again. Use include:['caller','test'] for the edit-safety half alone (who breaks, what re-verifies). Budget with maxChars — the roles are filled in priority order and what did not fit is reported as 'not shown', so a tight budget still returns the target and its callers rather than an arbitrary slice. Takes exactly ONE symbol: {refs} — a name matching several is an error listing the candidates, because a pack is a claim about one symbol's neighbourhood. Prefer this over get_code when the question is 'how does X work' or 'is it safe to change X'; prefer plain get_code when you only want the source text.",
                refs = NODE_REF_FORMS
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nodeId": { "type": "string", "description": format!("The one symbol to build the pack around. {refs} It must resolve to exactly one symbol; resolve an ambiguous name with find_symbols first.", refs = NODE_REF_FORMS) },
                    "maxChars": { "type": "integer", "minimum": 500, "description": "Total character budget for the whole pack (default 60000). Roles are filled in priority order — target, caller, test, dependency, doc — so lowering this drops docs and dependencies before it drops callers." },
                    "include": { "type": "array", "items": { "type": "string", "enum": ["target", "caller", "test", "dependency", "doc"] }, "description": "Keep only these roles. Omit for all five. ['caller','test'] is the edit-safety pair; ['target'] is just the source." }
                },
                "required": ["nodeId"]
            }
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
            "name": "walk",
            "description": "WHAT DID THIS CHANGE TOUCH? Maps a git diff onto the graph and returns the symbols it actually edited — innermost enclosing function or class per hunk, not just the file list — ordered by the call graph so callers come before the code they call, followed by the unchanged callers and tests that reach them. Use it to review a branch or a commit, to orient before continuing someone else's work, or to answer 'what did I just change and what does it affect' without reading the patch. This is what a diff cannot tell you: `git diff` orders by filename and stops at the file, while this names the symbols and follows the edges out of them. Every stop is labelled with its role — `changed` means the diff edited those lines, `caller`/`test` mean the code is UNCHANGED and is listed only because it reaches something that changed; never edit a caller believing the diff touched it. Needs git and graph.json; no database and no embedder. One hop out by design: for the full reachable set use analyze with `diff_impact` or `diff_retest_scope`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "spec": {
                        "type": "string",
                        "description": "What to walk. Omit (or \"working\") for uncommitted changes including untracked files — the default, and what you want after editing. \"staged\" for the index alone. A commit-ish (\"HEAD\", \"HEAD~2\", a sha) walks that commit against its parent. A range walks several: \"main..HEAD\" is every commit on this branch, \"main...HEAD\" is this branch against where it forked. Line numbers are exact for uncommitted work and for the most recent commit; walking an older revision maps its lines onto today's code and says so in a warning."
                    },
                    "max_stops": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 40,
                        "description": "Upper bound on stops (default 10). A large diff is summarised to the most-changed symbols rather than truncated arbitrarily."
                    },
                    "expand": {
                        "type": "boolean",
                        "description": "Include the unchanged callers and tests the change reaches (default true). Set false for only what the diff edited."
                    }
                }
            }
        },
        {
            "name": "analyze",
            "description": format!(
                "WHOLE-REPO STATISTICS over the indexed graph — counts, groups, distributions and blast radius. Use this for ANY question of the form 'how many', 'which are the biggest / longest / most depended-upon', 'what fraction', 'where is the worst X', 'what breaks if I change Y'. NEVER grep for a count and NEVER loop a per-file tool to build one: this answers in one call and ~100 tokens what reading the repo costs hundreds of thousands. Two ways to call it. (1) `preset` — a named question, the cheap path, e.g. {{\"preset\": \"long_functions\"}} or {{\"preset\": \"impact\", \"args\": {{\"target\": \"src/auth.ts\"}}}}. The catalogue is the `preset` parameter's own enum, with what each one answers — read it there and pick a name from it; a name that is not in that list is not a preset, however plausible it sounds. (2) `gql` — a raw OverGraph GQL (Cypher-shaped) query when no preset fits, e.g. \"MATCH (n:Function) WHERE n.loc > 50 AND n.is_test = 0 RETURN n.folder AS folder, count(*) AS c ORDER BY c DESC\"; the properties you can query are listed on that parameter. A *boundary* is where the system meets the outside world (a REST handler, a queue listener, a CLI command, an outbound HTTP or DB client); `boundary_impact` is the blast-radius question that matters before a change, because it reports which externally-visible contracts a change reaches rather than merely how many symbols move. Booleans are stored as 0/1 so they can be summed: documented fraction is sum(n.has_doc)/count(*). Read-only; it cannot modify the index. Every answer states its coverage denominators — treat a 'NOT INDEXED' warning as meaning the number is about nothing.",
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "preset": {
                        "type": "string",
                        "enum": preset_names(),
                        "description": format!(
                            "Name of a built-in question to run. Cheapest path — prefer this over writing GQL. Pick one of these exactly; the parenthesised names are the ONLY keys that preset accepts in `args`, and a preset shown without parentheses takes no `args` at all:\n{catalog}",
                            catalog = preset_catalog()
                        ),
                    },
                    "gql": { "type": "string", "description": "Raw OverGraph GQL, when no preset fits. Queryable node properties (these are PROPERTIES, not preset names — they are only valid inside a gql string): node_type, name, file, folder, loc, params, max_nesting, has_doc, is_test, in_degree, out_degree, qualified_name, route, annotations, start_line, end_line, boundary, boundary_in, boundary_out, boundary_kinds, boundary_protocols, boundary_detail — call graph_schema for their live population counts before relying on one. Aggregates: count, sum, avg, min, max, collect (no percentile — a collect() column is summarised as p50/p90/p99 in the output). Supports CASE, WITH … WHERE as HAVING, EXISTS { … } (needs its own RETURN clause inside), UNION, STARTS WITH / ENDS WITH / CONTAINS, and bounded variable-length paths. Every variable-length path needs a finite bound (*1..3, never *) and unanchored walks past 2 hops can exceed the traversal cap. Parenthesise negated membership: NOT (x IN [...])." },
                    "args": { "type": "object", "description": "Arguments for the chosen preset ONLY — the names inside that preset's parentheses in the `preset` catalogue, e.g. {\"target\": \"src/auth.ts\"} or {\"min_loc\": 100}. A preset listed there WITHOUT parentheses takes no arguments; omit this. An argument the preset does not declare is an error, not an ignored key. Paging is not a preset argument either: `limit` and `range` are top-level parameters, siblings of `preset`, never keys in here." },
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
            "description": "Refresh the index (index → graph → embed) for the current (or named) project — the same thing `ug gen` / `ug update` do in the CLI. Call it AFTER YOU EDIT FILES and before you ask any structural question about them: the structural tools (find_usages, traverse, analyze, shortest_path) answer from the index, so until you refresh, a blast radius describes the code as it was before your edit and looks exactly like a correct one. Also call it when a tool output carries an \"Index may be stale\" warning, or when results look outdated. Pass files: [\"src/a.ts\", \"src/b.rs\"] naming what you changed — the run reports how many symbols each of those files contributed, so you find out if one of them is not indexed at all; omit it to refresh everything. Incremental either way (unchanged files are skipped via content hashes), but embedding changed nodes needs the embedding backend, so it can take a while on big diffs; the structural tools are refreshed even if embedding fails.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Paths you just changed (repo-relative or absolute, inside the repo). Scopes the report, not the work — the refresh is incremental regardless. A path outside the repo is an error rather than a silent skip."
                    }
                }
            }
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
                    || GIT_BACKED_CHAT_TOOLS.contains(name)
                    || ultragraph::agent_tools::is_agent_tool(name),
                "'{}' is advertised to the chat model but no dispatcher answers it: \
                 add it to a store-backed match arm (and to STORE_BACKED_CHAT_TOOLS), \
                 to a git-backed arm (and to GIT_BACKED_CHAT_TOOLS), \
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
    fn the_preset_catalog_names_arguments_and_says_what_each_answers() {
        let cat = preset_catalog();
        assert!(cat.contains("long_functions(min_loc) — "), "{cat}");
        assert!(cat.contains("impact(target) — "), "{cat}");
        // A preset without parameters stays bare — no empty parens, because
        // `foo()` reads as "takes arguments, I just don't know which".
        assert!(!cat.contains("()"), "{cat}");
        assert!(cat.contains("repo_census — "), "{cat}");
        // The description is the half that lets a model choose; dropping it
        // is what left it picking from 35 bare names.
        for line in cat.lines() {
            assert!(line.contains(" — "), "every preset needs its one-liner: {line}");
        }
    }

    /// The `preset` slot must be closed, not free text.
    ///
    /// A real transcript opened with `{"preset": "boundary_kinds"}` — a graph
    /// *property* the tool description happened to list two sentences away —
    /// then `{"preset": "boundaries", "args": {"kinds": [...]}}` on a preset
    /// that declares no arguments. Two of four calls spent before the first
    /// real answer. An enum is the part of a schema constrained decoding
    /// actually enforces.
    #[test]
    fn the_preset_parameter_is_an_enum_of_real_presets() {
        let tools = tool_list();
        let analyze = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "analyze")
            .expect("analyze is advertised");
        let preset = &analyze["inputSchema"]["properties"]["preset"];
        let names: Vec<&str> = preset["enum"]
            .as_array()
            .expect("preset must be an enum, or the model may invent one")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            names.len(),
            ultragraph::analyze::presets::all().len(),
            "the enum is generated from the registry, so it cannot drift from it"
        );
        assert!(names.contains(&"boundaries") && names.contains(&"boundary_census"));
        // The name the model actually invented, and the namespace it came
        // from: a property is not a preset.
        assert!(!names.contains(&"boundary_kinds"), "{names:?}");

        // …and that namespace now lives on the parameter it belongs to.
        let gql = analyze["inputSchema"]["properties"]["gql"]["description"]
            .as_str()
            .unwrap();
        assert!(gql.contains("boundary_kinds"), "gql owns the property list");
        let desc = analyze["description"].as_str().unwrap();
        assert!(
            !desc.contains("boundary_kinds"),
            "the property list must not sit beside the preset list again"
        );
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

    /// The only names that work are the advertised ones. A near-miss must
    /// fail rather than quietly resolving — including `analyse`, whose other
    /// spelling is a real tool, and the singular of `find_symbols`.
    #[test]
    fn an_unadvertised_name_is_not_a_tool() {
        for miss in ["analyse", "find_symbol", "graph", "search_code", "outline"] {
            assert!(!is_known_tool(miss), "`{miss}` must not resolve");
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

    /// `semantic_search` folded into `search` as `expand: false`. It stays
    /// dispatchable so a cached tool list keeps working, but advertising it
    /// again would restore the ambiguity the merge removed.
    #[test]
    fn semantic_search_is_a_callable_alias_but_not_advertised() {
        assert!(is_known_tool("semantic_search"));
        assert!(is_alias_tool("semantic_search"));
        assert!(!TOOL_NAMES.contains(&"semantic_search"));
        let list = tool_list();
        let names: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"semantic_search"), "{names:?}");
    }

    /// The alias has no schema of its own, so it borrows `search`'s — without
    /// that, a model's stringified `"k": "10"` reaches serde and errors.
    #[test]
    fn the_alias_borrows_searchs_arg_coercion() {
        let mut args = json!({ "query": "oauth", "k": "10", "expand": "false" });
        normalize_args("semantic_search", &mut args);
        assert_eq!(args["k"], json!(10));
        assert_eq!(args["expand"], json!(false));
        assert_eq!(args["query"], json!("oauth"), "a real string stays a string");
    }

    /// The one knob the merge added. If it ever stops being a boolean the
    /// coercion above silently stops firing.
    #[test]
    fn search_advertises_expand_as_a_boolean() {
        let search = raw_tools()
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!("search"))
            .cloned()
            .expect("search is advertised");
        assert_eq!(
            search["inputSchema"]["properties"]["expand"]["type"],
            json!("boolean")
        );
    }
}
