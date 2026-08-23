//! Graph.json-backed agent tools — one implementation, three transports.
//!
//! `ug find_symbols` (CLI), `POST /api/tools/find_symbols` (HTTP) and the MCP
//! `find_symbols` tool all land in the same function here. Each tool takes a
//! typed params struct and returns a typed result that both serializes to the
//! canonical JSON envelope and renders to text through [`Render`] — so the
//! three surfaces agree by construction instead of by discipline.
//!
//! Params use the canonical snake_case vocabulary; the transports are
//! responsible for mapping their own spelling onto it (MCP camelCase, CLI
//! kebab flags, HTTP snake_case query/body).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::pattern::{self, Mode, Pattern};
use crate::types::{BoundaryDirection, GraphData, GraphEdgeType, GraphNode, GraphNodeType};

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

// `Render` lives in `crate::style` beside the escape codes and the colour
// gate it drives; re-exported here because `agent_tools::Render` is the path
// every transport (CLI, HTTP, MCP) already imports it by.
pub use crate::style::Render;

/// Separator between sections of a batched call (one per node id / file /
/// name). Skipped before the first section. A blank line is enough — a
/// drawn rule costs ~40 bytes × N sections and carries no information.
fn section_break(out: &mut String, i: usize, _style: Render) {
    if i > 0 {
        out.push_str("\n\n");
    }
}

fn line(out: &mut String, s: &str) {
    out.push_str(s);
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------------

pub fn node_type_str(t: &GraphNodeType) -> &'static str {
    match t {
        GraphNodeType::File => "File",
        GraphNodeType::Folder => "Folder",
        GraphNodeType::Function => "Function",
        GraphNodeType::Class => "Class",
        GraphNodeType::Interface => "Interface",
        GraphNodeType::Concept => "Concept",
        GraphNodeType::Dependency => "Dependency",
        GraphNodeType::Config => "Config",
        GraphNodeType::Constant => "Constant",
        GraphNodeType::Variable => "Variable",
        GraphNodeType::Route => "Route",
    }
}

pub fn edge_type_str(t: &GraphEdgeType) -> &'static str {
    match t {
        GraphEdgeType::DependsOn => "DependsOn",
        GraphEdgeType::Calls => "Calls",
        GraphEdgeType::Extends => "Extends",
        GraphEdgeType::Implements => "Implements",
        GraphEdgeType::References => "References",
        GraphEdgeType::Contains => "Contains",
        GraphEdgeType::Imports => "Imports",
        GraphEdgeType::Exports => "Exports",
        GraphEdgeType::Requires => "Requires",
        GraphEdgeType::Uses => "Uses",
        GraphEdgeType::Overrides => "Overrides",
        GraphEdgeType::Instantiates => "Instantiates",
    }
}

/// Every edge type an indexer can emit — what the `edge_types` filters
/// accept. Surfaced by `graph_schema` so agents don't guess.
pub const EDGE_TYPE_VOCABULARY: &[&str] = &[
    "DependsOn",
    "Calls",
    "Extends",
    "Implements",
    "References",
    "Contains",
    "Imports",
    "Exports",
    "Requires",
    "Uses",
    "Overrides",
    "Instantiates",
];

/// Default edge types for `find_usages` — dependency-ish edges only, no
/// `Contains` (structure), so results mean "code that uses this", not "the
/// folder that holds it".
///
/// `overrides` is included because an override is the most useful answer
/// there is to "what depends on this method": for an interface or abstract
/// declaration it is the entirety of the implementing code.
///
/// `instantiates` and `uses` are here for the same reason. Constructing a
/// type is how most code depends on a type, and reading a constant is the
/// *only* way anything depends on one — leave them out and every constant in
/// the repo answers "who uses this" with silence.
pub const USAGE_EDGE_TYPES: &[&str] = &[
    "calls",
    "references",
    "imports",
    "extends",
    "implements",
    "overrides",
    "instantiates",
    "uses",
];

/// `file:<path>` is how File node ids print, and users copy that straight
/// into file-taking params. Accept both forms.
pub fn strip_file_id_prefix(file: &str) -> &str {
    file.strip_prefix("file:").unwrap_or(file)
}

/// Node ids from the indexer are `<kind>:<path>:<name>` (with a `#N` suffix
/// when a file declares that name more than once). The CLI takes bare
/// positionals, so it needs a heuristic to tell an id from a name.
///
/// A wildcard pattern is never an id, even one containing `:` — `*:*:login`
/// is a request to search ids, not to look one up.
pub fn looks_like_node_id(s: &str) -> bool {
    s.contains(':') && !pattern::is_pattern(s)
}

// ---------------------------------------------------------------------------
// Wildcard matching
// ---------------------------------------------------------------------------

/// One query string, compiled: a wildcard pattern when it contains
/// metacharacters, a plain literal otherwise.
///
/// The split is what keeps wildcards additive. A name with no `*`/`?`/`[`/`{`
/// takes exactly the path it always took — exact beats prefix beats substring
/// — so no existing call changes meaning; a name with one gets anchored glob
/// matching. Every param that accepts a name, a type or a path goes through
/// here, so the dialect is the same wherever an agent tries it.
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Lower-cased literal, compared by the caller's own rule (equality for
    /// types, exact/prefix/substring ranking for names, prefix for paths).
    Literal(String),
    Glob(Pattern),
}

impl Matcher {
    /// Compile `q`, treating it as a pattern only if it carries an unescaped
    /// metacharacter.
    pub fn new(q: &str, mode: Mode) -> Result<Matcher, String> {
        if pattern::is_pattern(q) {
            Ok(Matcher::Glob(Pattern::new(q, mode)?))
        } else {
            Ok(Matcher::Literal(pattern::unescape(q).to_lowercase()))
        }
    }

    pub fn is_glob(&self) -> bool {
        matches!(self, Matcher::Glob(_))
    }

    /// Whole-string match: glob semantics for a pattern, case-insensitive
    /// equality for a literal.
    pub fn matches(&self, s: &str) -> bool {
        match self {
            Matcher::Literal(lit) => s.to_lowercase() == *lit,
            Matcher::Glob(p) => p.matches(s),
        }
    }

    /// Match anywhere in `s` — glob for a pattern, substring for a literal.
    /// For prose (docstrings), where anchoring would be useless.
    pub fn matches_within(&self, s: &str) -> bool {
        match self {
            Matcher::Literal(lit) => s.to_lowercase().contains(lit.as_str()),
            Matcher::Glob(p) => Pattern::containing(p.as_str(), Mode::Name)
                .map(|c| c.matches(s))
                .unwrap_or(false),
        }
    }
}

/// A file filter: a path glob, or (with no metacharacters) the repo-relative
/// prefix `file_prefix` has always meant.
#[derive(Debug, Clone)]
pub struct PathFilter(Matcher);

impl PathFilter {
    pub fn new(spec: &str) -> Result<PathFilter, String> {
        Ok(PathFilter(Matcher::new(spec, Mode::Path)?))
    }

    pub fn matches(&self, path: &str) -> bool {
        match &self.0 {
            // Prefix, not equality: `src/auth/` has to keep selecting
            // everything beneath it.
            Matcher::Literal(prefix) => path.to_lowercase().starts_with(prefix.as_str()),
            Matcher::Glob(p) => p.matches(path),
        }
    }
}

/// Compile a `node_types` filter. An empty list means "any type"; a type
/// may itself be a pattern (`C*` for Class/Concept/Config/Constant).
fn type_matchers(specs: &[String]) -> Result<Vec<Matcher>, String> {
    specs
        .iter()
        .map(|t| Matcher::new(t, Mode::Name))
        .collect()
}

fn type_allowed(matchers: &[Matcher], n: &GraphNode) -> bool {
    matchers.is_empty() || matchers.iter().any(|m| m.matches(node_type_str(&n.node_type)))
}

// ---------------------------------------------------------------------------
// Node references
// ---------------------------------------------------------------------------

/// How many nodes one name or pattern may expand to in an id-taking tool.
///
/// These tools print a section per node, so an unbounded expansion of `*` is
/// a context bomb. The overflow is reported rather than dropped — see
/// [`unresolved_ref_error`].
pub const MAX_REF_EXPANSION: usize = 25;

/// Turn what the caller *wrote* into node ids.
///
/// Every id-taking tool (`get_code`, `find_usages`, `traverse`,
/// `shortest_path`) runs its `node_id` list through this first, so all three
/// of these work and mean the same thing everywhere:
///
/// - `function:src/auth.rs:42:login` — an id, used as-is (the fast path).
/// - `login` — a bare name, resolved to every symbol with that exact name.
/// - `login_*`, `src/auth/*.ts` — a wildcard, resolved to every symbol whose
///   name matches (or, when the pattern contains `/`, whose file matches).
///
/// Before this, anything but an id was an error telling the caller to go run
/// `find_symbols` and come back — a round trip per lookup for an agent, and a
/// dead end for a human who knows the function's name perfectly well.
///
/// A reference that resolves to nothing is passed through unchanged so the
/// caller's own "no such node" branch reports it; those branches use
/// [`unresolved_ref_error`], which explains what actually went wrong. A
/// reference whose expansion overflows `cap` is *also* passed through, after
/// its first `cap` ids, so the truncation is reported the same way instead of
/// silently shortening the answer.
pub fn expand_node_refs(graph: &GraphData, refs: &[String], cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in refs {
        if graph.nodes.iter().any(|n| n.id == *r) {
            out.push(r.clone());
            continue;
        }
        let matched = match_node_refs(graph, r);
        if matched.is_empty() {
            out.push(r.clone());
            continue;
        }
        out.extend(matched.iter().take(cap).cloned());
        if matched.len() > cap {
            out.push(r.clone());
        }
    }
    // Order-preserving dedup: keep the first mention of each id, drop later
    // ones. `Vec::dedup` was wrong here — it only collapses *adjacent*
    // duplicates, and the duplicates this produces never are. Two refs that
    // overlap (`pad` and `pa*`, or a name and the file that contains it)
    // interleave their expansions, so every tool downstream rendered the same
    // symbol twice and spent the caller's context on it.
    //
    // Sorting first would make `dedup` correct but is not an option:
    // `match_node_refs` orders by (name, id) and that is the order sections
    // print in, so sorting `out` would reorder the caller's own references.
    let mut seen: HashSet<String> = HashSet::with_capacity(out.len());
    out.retain(|id| seen.insert(id.clone()));
    out
}

/// Node ids a non-id reference names, in a stable order.
fn match_node_refs(graph: &GraphData, r: &str) -> Vec<String> {
    let name = match Matcher::new(r, Mode::Name) {
        Ok(m) => m,
        Err(_) => return vec![],
    };
    // A `/` means the caller is talking about paths, so match files too.
    let path = if r.contains('/') {
        PathFilter::new(r).ok()
    } else {
        None
    };

    let mut hits: Vec<&GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| {
            name.matches(&n.name)
                || path
                    .as_ref()
                    .zip(n.file.as_deref())
                    .map(|(p, f)| p.matches(f))
                    .unwrap_or(false)
        })
        .collect();
    hits.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    hits.into_iter().map(|n| n.id.clone()).collect()
}

/// Resolve one reference to exactly one node id, for the tools that take a
/// single endpoint rather than a list (`shortest_path`).
///
/// Ambiguity is an error here, not a silent pick: "is A connected to B" has
/// a different answer for each candidate B, so choosing one for the caller
/// would produce a confident answer to a question they did not ask.
pub fn resolve_single_ref(graph: &GraphData, r: &str) -> Result<String, String> {
    if graph.nodes.iter().any(|n| n.id == r) {
        return Ok(r.to_string());
    }
    let matched = match_node_refs(graph, r);
    match matched.len() {
        0 => Err(unresolved_ref_error(graph, r, MAX_REF_EXPANSION)),
        1 => Ok(matched.into_iter().next().unwrap()),
        n => Err(format!(
            "'{}' matches {} symbols — pass one id: {}{}",
            r,
            n,
            matched
                .iter()
                .take(5)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            if n > 5 { ", …" } else { "" }
        )),
    }
}

/// The message every id-taking tool shows for a reference it could not use —
/// one that named nothing, or one whose expansion hit the cap.
///
/// Written against the reference the caller actually passed: an id gets the
/// "where ids come from" pointer, a name that matches nothing suggests the
/// tools that find one, and a pattern is told how many symbols it hit and
/// what to do about it. A single "No node with id X" for all three sent
/// callers hunting for the wrong problem.
pub fn unresolved_ref_error(graph: &GraphData, r: &str, cap: usize) -> String {
    let matched = match_node_refs(graph, r);
    if matched.len() > cap {
        return format!(
            "'{}' matches {} symbols — only the first {} were expanded. Narrow the pattern, or run find_symbols '{}' to pick the ones you want.",
            r,
            matched.len(),
            cap,
            r
        );
    }
    if pattern::is_pattern(r) {
        return format!(
            "No symbol matches pattern '{}'. Patterns match the whole name — wrap it in '*' to match anywhere (e.g. '*{}*'), and use '**/' to cross directories in a path.",
            r,
            r.trim_matches('*')
        );
    }
    if looks_like_node_id(r) {
        return format!(
            "No node with id '{}' — ids come from find_symbols, search or file_outline. A plain name or a wildcard ('{}*') also works here.",
            r, r
        );
    }
    format!(
        "No symbol named '{}'. This parameter takes a node id, an exact name, or a wildcard — try '{}*' or run find_symbols '*{}*' to see what exists.",
        r, r, r
    )
}

pub fn by_id_map(graph: &GraphData) -> HashMap<&str, &GraphNode> {
    graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect()
}

/// `file:start-end`, or just the path for File nodes (which carry no line
/// range — printing `?-?` reads like an error).
pub fn node_loc(n: &GraphNode) -> String {
    match &n.file {
        Some(f) => match (n.start_line, n.end_line) {
            (None, None) => f.clone(),
            (s, e) => format!(
                "{}:{}-{}",
                f,
                s.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                e.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
            ),
        },
        None => "(no file)".into(),
    }
}

// ---------------------------------------------------------------------------
// Param decoding
// ---------------------------------------------------------------------------

/// Accept `"x"` or `["x", "y"]` for the same field.
///
/// The batch-friendly params (`node_id`, `name`, `file`) are documented to
/// take one value or an array, and callers rely on both — so normalise here
/// rather than making every transport branch on the shape.
fn de_one_or_many<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => vec![],
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

// ---------------------------------------------------------------------------
// Shared result pieces
// ---------------------------------------------------------------------------

/// The canonical shape of a node in any tool result. Every field an agent
/// needs to make a follow-up call, and nothing else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRef {
    pub id: String,
    pub name: String,
    pub node_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// One-line summary of the system boundaries this symbol is, e.g.
    /// `in:http.endpoint GET /api/orders/{id}`.
    ///
    /// Flattened to a string rather than carried as the structured
    /// [`crate::types::Boundary`] list: this rides on every node of every
    /// tool result, and a nested object per node would cost more tokens than
    /// the fact is worth. `None` — the common case — serializes to nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<String>,
}

/// How much of a docstring travels in a list result. Full text is available
/// via `get_code`, so listings stay scannable.
const DOC_PREVIEW_CHARS: usize = 200;

/// Compact `in:kind detail` label for a node's boundaries, or `None` when it
/// is not one.
///
/// Only the first two are named. A route-registration function can declare a
/// dozen, and spelling them all out on every row of a `traverse` result would
/// bury the graph in paths; the count is what tells the reader to go look.
fn boundary_label(n: &GraphNode) -> Option<String> {
    if n.boundaries.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = n
        .boundaries
        .iter()
        .take(2)
        .map(|b| {
            let dir = match b.direction {
                BoundaryDirection::Inbound => "in",
                BoundaryDirection::Outbound => "out",
            };
            match b.detail.as_deref().filter(|d| !d.is_empty()) {
                Some(d) => format!("{dir}:{} {d}", b.kind),
                None => format!("{dir}:{}", b.kind),
            }
        })
        .collect();
    if n.boundaries.len() > 2 {
        parts.push(format!("+{} more", n.boundaries.len() - 2));
    }
    Some(parts.join(", "))
}

impl SymbolRef {
    pub fn from_node(n: &GraphNode) -> Self {
        SymbolRef {
            id: n.id.clone(),
            name: n.name.clone(),
            node_type: node_type_str(&n.node_type).to_string(),
            file: n.file.clone(),
            start_line: n.start_line,
            end_line: n.end_line,
            doc: n.docstring.as_ref().map(|d| {
                let flat = d.replace('\n', " ");
                flat.chars().take(DOC_PREVIEW_CHARS).collect()
            }),
            boundary: boundary_label(n),
        }
    }

    fn loc(&self) -> String {
        match &self.file {
            Some(f) => match (self.start_line, self.end_line) {
                (None, None) => f.clone(),
                (s, e) => format!(
                    "{}:{}-{}",
                    f,
                    s.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                    e.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                ),
            },
            None => "(no file)".into(),
        }
    }

    /// `- Function foo  src/a.rs:10-20` + an `id:` line beneath.
    fn render_bullet(&self, out: &mut String, style: Render) {
        line(
            out,
            &format!(
                "- {} {}  {}",
                self.node_type,
                style.bold(&self.name),
                style.dim(&self.loc())
            ),
        );
        line(out, &format!("  id: {}", style.id(&self.id)));
        // Above the doc line: that this symbol is a contract with something
        // outside the repo changes what a reader does next more than its
        // description does.
        if let Some(b) = &self.boundary {
            line(out, &format!("  {}", style.bold(&format!("boundary: {}", b))));
        }
        if let Some(d) = &self.doc {
            line(out, &format!("  {}", style.dim(&format!("doc: {}", d))));
        }
    }
}

/// Standard "what to call next" footer, as `(command, why)` pairs. Agents
/// lean on these hints, so they live with the tool rather than being
/// re-invented in each transport. The command is styled per surface — cyan
/// in a terminal, backticks in Markdown — so neither leaks the other's
/// markup.
fn next_actions(out: &mut String, style: Render, hints: &[(&str, &str)]) {
    if hints.is_empty() {
        return;
    }
    let rendered: Vec<String> = hints
        .iter()
        .map(|(cmd, why)| {
            if why.is_empty() {
                style.id(cmd)
            } else {
                format!("{} {}", style.id(cmd), why)
            }
        })
        .collect();
    out.push('\n');
    line(
        out,
        &format!("{} {}", style.dim("Next:"), rendered.join(" · ")),
    );
}

// ---------------------------------------------------------------------------
// Tool modules
// ---------------------------------------------------------------------------

mod find_symbols;
mod file_outline;
mod get_code;
mod find_usages;
mod traverse;
mod project_overview;
mod graph_schema;
mod shortest_path;
mod context;

pub use find_symbols::*;
pub use file_outline::*;
pub use get_code::*;
pub use find_usages::*;
pub use traverse::*;
pub use project_overview::*;
pub use graph_schema::*;
pub use shortest_path::*;
pub use context::*;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The graph-backed tools [`run_tool`] dispatches: canonical name and its
/// one-line summary, in the order they're most useful to an agent.
///
/// One table rather than a name list beside a summary `match`, so a tool
/// cannot be advertised without a description or described without being
/// advertised — the two drifted apart in exactly that way before.
pub const AGENT_TOOLS: &[(&str, &str)] = &[
    ("project_overview", "Orient in the codebase: stats, biggest files, most depended-upon symbols."),
    ("context", "Everything about one symbol in a budgeted bundle: code, callers, tests, deps, docs."),
    ("find_symbols", "Symbol lookup by name or wildcard — returns node ids for the other tools."),
    ("file_outline", "Every indexed symbol in a file, in line order; takes a path glob."),
    ("get_code", "Read source for a symbol (id, name or wildcard), or a file/line range."),
    ("find_usages", "Who uses this symbol — inbound callers/importers, with call sites."),
    ("traverse", "N-hop walk from seed symbols, filtered by edge type and direction."),
    ("shortest_path", "Shortest directed edge path between two symbols."),
    ("graph_schema", "Node & edge types present in this graph, with counts."),
];

/// Store-backed tools `POST /api/tools/:tool` also dispatches, which
/// [`run_tool`] itself cannot answer — aggregation and reachability need the
/// indexed database rather than graph.json. Kept beside [`AGENT_TOOLS`] rather
/// than in it, so "can `run_tool` answer this?" stays true for one list while
/// the HTTP discovery still advertises everything an agent can call.
pub const STORE_BACKED_AGENT_TOOLS: &[(&str, &str)] = &[(
    "analyze",
    "Whole-repo statistics, distributions and blast radius — a named preset or a raw GQL query over the indexed store.",
)];

/// Does `run_tool` answer this name?
pub fn is_agent_tool(tool: &str) -> bool {
    AGENT_TOOLS.iter().any(|(name, _)| *name == tool)
}

/// A worked request body per tool, for `GET /api/tools` and `ug api`.
///
/// Discovery that lists parameter names still leaves a caller guessing what a
/// real call looks like; one copyable example per tool answers that in the
/// same round trip. Each shows the wildcard form, since that is the part
/// neither a schema nor a summary conveys.
pub fn tool_example(tool: &str) -> &'static str {
    match tool {
        "project_overview" => r#"{}"#,
        "context" => r#"{"node_id": "run_gen", "max_chars": 12000}"#,
        "find_symbols" => r#"{"name": "handle_*", "node_types": ["Function"], "file_prefix": "src/**"}"#,
        "file_outline" => r#"{"file": "src/**/*.ts", "max_files": 20}"#,
        "get_code" => r#"{"node_id": "render_*", "max_chars": 20000}"#,
        "find_usages" => r#"{"node_id": "validate_*", "hops": 1, "edge_types": ["calls"]}"#,
        "traverse" => r#"{"node_id": ["handle_*"], "hops": 2, "direction": "inbound"}"#,
        "shortest_path" => r#"{"source": "run_gen", "target": "run_ingest"}"#,
        "graph_schema" => r#"{}"#,
        "analyze" => r#"{"preset": "long_functions", "args": {"min_loc": 100}}"#,
        _ => "{}",
    }
}

/// One-line summary, for `ug api` / `GET /api/tools` discovery.
pub fn tool_summary(tool: &str) -> &'static str {
    AGENT_TOOLS
        .iter()
        .chain(STORE_BACKED_AGENT_TOOLS.iter())
        .find(|(name, _)| *name == tool)
        .map(|(_, summary)| *summary)
        .unwrap_or("")
}

/// What a tool call produced.
pub enum ToolOutput {
    Json(serde_json::Value),
    Text(String),
}

/// Run one graph-backed tool by canonical name.
///
/// The single dispatch behind every transport: the MCP server (`ug mcp`) and
/// the HTTP `/api/tools/:name` route both call this, so adding a tool or
/// changing one's shape can't leave a surface behind. `style` of `None`
/// returns the JSON envelope; `Some(_)` returns rendered text.
/// Node ids whose captured source `tool` will read, given these params.
///
/// The pre-fetch step every transport runs before [`run_tool`]: resolve the
/// ids here, load them with [`IndexedSource::load`], and the tool answers
/// from the index instead of the working tree. An empty result — including
/// for a tool that reads no source, or params that fail to parse — just
/// means the call has nothing to pre-fetch, and `run_tool` will report any
/// parameter problem itself.
pub fn source_node_ids(tool: &str, graph: &GraphData, params: &serde_json::Value) -> Vec<String> {
    match tool {
        "get_code" => serde_json::from_value::<GetCodeParams>(params.clone())
            .map(|p| get_code_source_ids(graph, &p))
            .unwrap_or_default(),
        "find_usages" => serde_json::from_value::<FindUsagesParams>(params.clone())
            .map(|p| find_usages_source_ids(graph, &p))
            .unwrap_or_default(),
        // A pack reads the target's body and its callers' call sites, so it
        // needs both pre-fetches — otherwise `ug context` would answer from
        // the working tree while `get_code` answered from the index.
        "context" => serde_json::from_value::<ContextParams>(params.clone())
            .map(|p| context_source_ids(graph, &p))
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// Node ids whose captured source a `get_code` call will read.
pub fn get_code_source_ids(graph: &GraphData, p: &GetCodeParams) -> Vec<String> {
    // Expand names/patterns the same way the tool will, or the pre-fetch
    // would miss exactly the sources a wildcard call is about to read.
    let mut ids = expand_node_refs(graph, &p.node_id, MAX_REF_EXPANSION);
    if p.node_id.is_empty() {
        // The file/range form reads the file's whole-file capture.
        if let Some(f) = p.file.as_deref() {
            let f = strip_file_id_prefix(f);
            ids.extend(whole_file_node_ids(graph, f).into_iter().map(String::from));
        }
    } else {
        // A node whose own span was never captured falls back to its
        // file's capture.
        for id in &p.node_id {
            if let Some(f) = graph
                .nodes
                .iter()
                .find(|n| n.id == *id)
                .and_then(|n| n.file.as_deref())
            {
                ids.extend(whole_file_node_ids(graph, f).into_iter().map(String::from));
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

pub fn run_tool(
    tool: &str,
    graph: &GraphData,
    src: SourceCtx,
    graph_path: &Path,
    params: serde_json::Value,
    style: Option<Render>,
) -> Result<ToolOutput, String> {
    fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, String> {
        serde_json::from_value(v).map_err(|e| format!("invalid params: {}", e))
    }
    fn out<T: Serialize>(
        result: T,
        style: Option<Render>,
        render: impl FnOnce(&T, Render) -> String,
    ) -> Result<ToolOutput, String> {
        Ok(match style {
            Some(s) => ToolOutput::Text(render(&result, s)),
            None => ToolOutput::Json(
                serde_json::to_value(&result).map_err(|e| format!("serialize result: {}", e))?,
            ),
        })
    }

    match tool {
        "find_symbols" => out(find_symbols(graph, &decode(params)?), style, render_find_symbols),
        "file_outline" => out(file_outline(graph, &decode(params)?), style, render_file_outline),
        "get_code" => out(
            get_code(graph, src, &decode(params)?),
            style,
            render_get_code,
        ),
        "find_usages" => out(
            find_usages(graph, src, &decode(params)?),
            style,
            render_find_usages,
        ),
        "project_overview" => out(
            project_overview(graph, src.repo_root(), graph_path),
            style,
            render_project_overview,
        ),
        "context" => out(context(graph, src, &decode(params)?), style, render_context),
        "traverse" => out(traverse(graph, &decode(params)?), style, render_traverse),
        "graph_schema" => out(graph_schema(graph, graph_path), style, render_graph_schema),
        "shortest_path" => {
            let p: ShortestPathParams = decode(params)?;
            if p.source.is_empty() || p.target.is_empty() {
                return Err(
                    "shortest_path needs both source and target — a node id, an exact symbol name, or a wildcard that matches exactly one symbol.".into(),
                );
            }
            // Endpoints may be written as names or patterns, like every
            // other id parameter; each must land on exactly one node.
            let source = resolve_single_ref(graph, &p.source)?;
            let target = resolve_single_ref(graph, &p.target)?;
            let result = shortest_path(graph, &source, &target, p.strict);
            Ok(match style {
                Some(s) => ToolOutput::Text(render_shortest_path(&result, s, p.strict)),
                None => ToolOutput::Json(
                    serde_json::to_value(&result).map_err(|e| format!("serialize result: {}", e))?,
                ),
            })
        }
        other => Err(format!(
            "Unknown agent tool '{}'. Expected one of: {}.",
            other,
            AGENT_TOOLS.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
        )),
    }
}
