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
use crate::{C_BOLD, C_CYAN, C_DIM, C_RESET};

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// How a result renders to text. The *layout* is identical either way — only
/// the emphasis markers differ — so CLI and MCP output can't drift apart.
/// JSON output doesn't go through here; transports serialize the result
/// struct directly.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Render {
    /// ANSI escapes, for a terminal.
    Ansi,
    /// Markdown, for MCP clients (which render it in a chat transcript).
    Markdown,
}

impl Render {
    /// Wrap `s` in an SGR pair when colour is on, return it untouched when off.
    /// Called only from the `Render::Ansi` arm of each styling method, so the
    /// runtime gate ([`crate::color`]) covers every agent-tool and `analyze`
    /// renderer without each call site branching on it.
    fn ansi(self, open: &str, s: &str) -> String {
        if crate::color::enabled() {
            format!("{}{}{}", open, s, C_RESET)
        } else {
            s.to_string()
        }
    }

    pub(crate) fn bold(self, s: &str) -> String {
        match self {
            Render::Markdown => format!("**{}**", s),
            Render::Ansi => self.ansi(C_BOLD, s),
        }
    }

    pub(crate) fn dim(self, s: &str) -> String {
        match self {
            // Markdown has no "dim"; plain text keeps the line readable.
            Render::Markdown => s.to_string(),
            Render::Ansi => self.ansi(C_DIM, s),
        }
    }

    /// A node id, or anything else meant to be copied verbatim into a
    /// follow-up call.
    pub(crate) fn id(self, s: &str) -> String {
        match self {
            Render::Markdown => format!("`{}`", s),
            Render::Ansi => self.ansi(C_CYAN, s),
        }
    }

    pub(crate) fn heading(self, s: &str) -> String {
        match self {
            Render::Markdown => format!("## {}", s),
            Render::Ansi => self.ansi(C_BOLD, s),
        }
    }
}

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
// find_symbols
// ---------------------------------------------------------------------------

/// Params are canonical snake_case. The `alias` attributes accept the legacy
/// MCP camelCase spellings so existing agent calls keep working unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FindSymbolsParams {
    /// Direct node id lookup — O(1), skips the search entirely.
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Identifier, fragment, or wildcard pattern to match against node
    /// names. A pattern (`run_*`, `get?`, `[gs]et_code`, `{save,load}_*`)
    /// must match the whole name; a plain fragment keeps its
    /// exact > prefix > substring ranking.
    #[serde(deserialize_with = "de_one_or_many")]
    pub name: Vec<String>,
    /// Restrict to these node types (case-insensitive; wildcards allowed).
    #[serde(alias = "nodeTypes", deserialize_with = "de_one_or_many")]
    pub node_types: Vec<String>,
    /// Only symbols under this repo-relative path: a plain prefix
    /// (`src/auth/`) or a path glob (`src/**/*.ts`).
    #[serde(
        alias = "filePrefix",
        alias = "file",
        alias = "filePattern",
        alias = "file_pattern",
        alias = "path"
    )]
    pub file_prefix: Option<String>,
    #[serde(alias = "k")]
    pub limit: Option<usize>,
    /// Also match against docstrings, not just names. A docstring hit ranks
    /// below every name hit, since matching the identifier is the stronger
    /// signal.
    #[serde(alias = "includeDocs")]
    pub include_docs: bool,
    /// Keep only symbols that are system boundaries — REST handlers, queue
    /// listeners, CLI commands, outbound clients.
    ///
    /// The way to ask "what is this service's public surface" without
    /// knowing what any of it is called, which is exactly the position
    /// someone is in on their first day in an unfamiliar repo.
    #[serde(default)]
    pub boundary: bool,
}

const DEFAULT_SYMBOL_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct SymbolQueryResult {
    pub query: String,
    /// `"id"` for a direct lookup, `"name"` for a ranked name search,
    /// `"pattern"` for a wildcard match.
    pub kind: &'static str,
    pub total: usize,
    pub items: Vec<SymbolRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindSymbolsResult {
    pub queries: Vec<SymbolQueryResult>,
}

impl FindSymbolsResult {
    pub fn ok(&self) -> bool {
        self.queries.iter().all(|q| q.error.is_none())
    }
}

pub fn find_symbols(graph: &GraphData, p: &FindSymbolsParams) -> FindSymbolsResult {
    let limit = p.limit.unwrap_or(DEFAULT_SYMBOL_LIMIT);
    let mut queries = Vec::new();

    // A bad filter is the whole call's problem, not one query's — report it
    // once, against every query, instead of silently returning matches that
    // ignored the filter the caller asked for.
    let filters = type_matchers(&p.node_types).and_then(|types| {
        let file = match &p.file_prefix {
            Some(f) => Some(PathFilter::new(f)?),
            None => None,
        };
        Ok::<_, String>((types, file))
    });
    let (types, file_filter) = match filters {
        Ok(f) => f,
        Err(e) => {
            let queries = p
                .node_id
                .iter()
                .chain(p.name.iter())
                .map(|q| SymbolQueryResult {
                    query: q.clone(),
                    kind: "name",
                    total: 0,
                    items: vec![],
                    error: Some(e.clone()),
                })
                .collect();
            return FindSymbolsResult { queries };
        }
    };

    for id in &p.node_id {
        queries.push(match graph.nodes.iter().find(|n| n.id == *id) {
            Some(n) => SymbolQueryResult {
                query: id.clone(),
                kind: "id",
                total: 1,
                items: vec![SymbolRef::from_node(n)],
                error: None,
            },
            None => SymbolQueryResult {
                query: id.clone(),
                kind: "id",
                total: 0,
                items: vec![],
                error: Some(format!(
                    "No node with id '{}' — ids come from find_symbols, search or file_outline.",
                    id
                )),
            },
        });
    }

    for name in &p.name {
        let matcher = match Matcher::new(name, Mode::Name) {
            Ok(m) => m,
            Err(e) => {
                queries.push(SymbolQueryResult {
                    query: name.clone(),
                    kind: "pattern",
                    total: 0,
                    items: vec![],
                    error: Some(e),
                });
                continue;
            }
        };
        let glob = matcher.is_glob();
        let mut hits: Vec<(u8, &GraphNode)> = Vec::new();
        for n in &graph.nodes {
            if !type_allowed(&types, n) {
                continue;
            }
            if let Some(f) = &file_filter {
                if !f.matches(n.file.as_deref().unwrap_or("")) {
                    continue;
                }
            }
            if p.boundary && n.boundaries.is_empty() {
                continue;
            }
            // A wildcard is an explicit statement of what the name looks
            // like, so every hit is equally "exact" — there is no weaker
            // prefix/substring tier to fall back to. A literal keeps the
            // exact > prefix > substring ladder. Either way a docstring hit
            // ranks last: matching the identifier beats matching prose
            // about it.
            let rank = if glob {
                if matcher.matches(&n.name) {
                    0
                } else if p.include_docs && doc_hit(n, &matcher) {
                    3
                } else {
                    4
                }
            } else {
                literal_rank(&n.name, &matcher, p.include_docs, n)
            };
            if rank < 4 {
                hits.push((rank, n));
            }
        }
        // Literal queries tie-break on the shorter (closer) name. Pattern
        // queries are a listing rather than a ranked search, so they sort
        // alphabetically by name then file — stable and scannable, and it
        // does not shuffle `run_gen`/`run_index`/`run_serve` by length.
        if glob {
            hits.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.name.cmp(&b.1.name))
                    .then_with(|| a.1.file.cmp(&b.1.file))
            });
        } else {
            hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.len().cmp(&b.1.name.len())));
        }
        let total = hits.len();
        queries.push(SymbolQueryResult {
            query: name.clone(),
            kind: if glob { "pattern" } else { "name" },
            total,
            items: hits
                .iter()
                .take(limit)
                .map(|(_, n)| SymbolRef::from_node(n))
                .collect(),
            error: None,
        });
    }

    FindSymbolsResult { queries }
}

fn doc_hit(n: &GraphNode, matcher: &Matcher) -> bool {
    n.docstring
        .as_ref()
        .map(|d| matcher.matches_within(d))
        .unwrap_or(false)
}

/// exact > prefix > substring > docstring, for a literal query. `4` means
/// no match.
fn literal_rank(name: &str, matcher: &Matcher, include_docs: bool, n: &GraphNode) -> u8 {
    let Matcher::Literal(q) = matcher else {
        return 4;
    };
    let nm = name.to_lowercase();
    if nm == *q {
        0
    } else if nm.starts_with(q.as_str()) {
        1
    } else if nm.contains(q.as_str()) {
        2
    } else if include_docs && doc_hit(n, matcher) {
        3
    } else {
        4
    }
}

pub fn render_find_symbols(r: &FindSymbolsResult, style: Render) -> String {
    let mut out = String::new();
    for (i, q) in r.queries.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(e) = &q.error {
            line(&mut out, &format!("✗ {}", e));
            continue;
        }
        if q.kind == "id" {
            line(&mut out, &style.heading("Node by direct id lookup"));
            out.push('\n');
        } else {
            // Say when a result was truncated *and* how to see the rest —
            // a bare "showing 20" leaves the reader to guess the flag.
            let showing = if q.total > q.items.len() {
                format!(
                    ", showing {} — raise {} for the rest",
                    q.items.len(),
                    style.id("limit")
                )
            } else {
                String::new()
            };
            let what = if q.kind == "pattern" {
                "Symbols matching pattern"
            } else {
                "Symbols matching"
            };
            line(
                &mut out,
                &format!(
                    "{} — {} match(es){}",
                    style.heading(&format!("{} '{}'", what, q.query)),
                    q.total,
                    showing
                ),
            );
            out.push('\n');
        }
        if q.items.is_empty() {
            // Each suggestion is a different failure: too specific a string,
            // too narrow a filter, or the wrong tool entirely.
            let widen = if q.kind == "pattern" {
                format!(
                    "Wildcards match the whole name — wrap the pattern in {} to match anywhere ({}).",
                    style.id("*"),
                    style.id("*auth*")
                )
            } else {
                format!(
                    "Try a shorter fragment, or a wildcard ({} matches the whole name).",
                    style.id("*auth*")
                )
            };
            line(&mut out, &format!("No matches. {}", widen));
            line(
                &mut out,
                &format!(
                    "Also worth trying: drop the type/file filters, add {} to scan docstrings, or use {} for a concept-level query.",
                    style.id("include_docs"),
                    style.id("search")
                ),
            );
            continue;
        }
        for item in &q.items {
            item.render_bullet(&mut out, style);
        }
    }
    next_actions(
        &mut out,
        style,
        &[
            ("get_code <id>", "for source"),
            ("find_usages <id>", "for callers"),
            ("traverse <id>", "for dependencies"),
        ],
    );
    out
}

// ---------------------------------------------------------------------------
// file_outline
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileOutlineParams {
    /// Direct File node id lookup.
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Repo-relative path, unique suffix, `file:<path>` id, or a path glob
    /// (`src/**/*.ts`) that outlines every file it matches.
    #[serde(deserialize_with = "de_one_or_many")]
    pub file: Vec<String>,
    /// Cap on files outlined per glob (default 20). Ignored by the exact and
    /// suffix forms, which resolve to one file.
    #[serde(alias = "maxFiles", alias = "limit", alias = "k")]
    pub max_files: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOutlineEntry {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub symbols: Vec<SymbolRef>,
    /// Populated when a path matched more than one indexed file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOutlineResult {
    pub files: Vec<FileOutlineEntry>,
    /// Whether rendered lines append the full node id. Default `true` so
    /// MCP / HTTP / an interactive terminal keep copy-pasteable ids; the
    /// CLI flips this to `false` when stdout is piped (an agent can
    /// reconstruct `kind:file:name`, and `--ids` turns it back on).
    #[serde(skip)]
    pub show_ids: bool,
}

impl FileOutlineResult {
    pub fn ok(&self) -> bool {
        self.files.iter().all(|f| f.error.is_none())
    }
}

pub fn file_outline(graph: &GraphData, p: &FileOutlineParams) -> FileOutlineResult {
    let mut files = Vec::new();

    for id in &p.node_id {
        let entry = match graph.nodes.iter().find(|n| n.id == *id) {
            None => FileOutlineEntry {
                query: id.clone(),
                file: None,
                symbols: vec![],
                candidates: vec![],
                error: Some(format!(
                    "No node with id '{}' — ids come from find_symbols, search or file_outline.",
                    id
                )),
            },
            Some(n) if !matches!(n.node_type, GraphNodeType::File | GraphNodeType::Folder) => {
                FileOutlineEntry {
                    query: id.clone(),
                    file: None,
                    symbols: vec![],
                    candidates: vec![],
                    error: Some(format!(
                        "Node '{}' is a {}, not a File — file_outline needs a File node id.",
                        id,
                        node_type_str(&n.node_type)
                    )),
                }
            }
            Some(n) => match &n.file {
                Some(f) => outline_by_path(graph, id, f),
                None => FileOutlineEntry {
                    query: id.clone(),
                    file: None,
                    symbols: vec![],
                    candidates: vec![],
                    error: Some(format!("File node '{}' has no file path.", id)),
                },
            },
        };
        files.push(entry);
    }

    for f in &p.file {
        let path = strip_file_id_prefix(f);
        if pattern::is_pattern(path) {
            files.extend(outline_by_glob(graph, f, path, p.max_files.unwrap_or(DEFAULT_OUTLINE_FILES)));
        } else {
            files.push(outline_by_path(graph, f, path));
        }
    }

    FileOutlineResult { files, show_ids: true }
}

/// How many files one glob outlines before the rest are listed by name
/// instead. A whole-repo glob would otherwise dump every symbol in the
/// project into an agent's context.
const DEFAULT_OUTLINE_FILES: usize = 20;

/// Outline every indexed file matching a path glob.
///
/// Returns one entry per matched file (so each renders with its own heading
/// and the caller sees which files answered), plus a final error-shaped entry
/// naming the overflow when the glob matched more than `max_files`. Nothing
/// matching is one entry rather than silence — a glob that selects nothing is
/// almost always a mis-written pattern, and the message says so.
fn outline_by_glob(
    graph: &GraphData,
    query: &str,
    glob: &str,
    max_files: usize,
) -> Vec<FileOutlineEntry> {
    let pat = match Pattern::new(glob, Mode::Path) {
        Ok(p) => p,
        Err(e) => {
            return vec![FileOutlineEntry {
                query: query.to_string(),
                file: None,
                symbols: vec![],
                candidates: vec![],
                error: Some(e),
            }]
        }
    };

    let mut matched: Vec<String> = graph
        .nodes
        .iter()
        .filter_map(|n| n.file.as_ref())
        .filter(|f| pat.matches(f))
        .cloned()
        .collect();
    matched.sort();
    matched.dedup();

    if matched.is_empty() {
        return vec![FileOutlineEntry {
            query: query.to_string(),
            file: None,
            symbols: vec![],
            candidates: vec![],
            error: Some(format!(
                "No indexed file matches pattern '{}'. Paths are repo-relative, and '*' does not cross '/' — use '**/' for that (e.g. 'src/**/*.ts').",
                glob
            )),
        }];
    }

    let overflow: Vec<String> = matched.split_off(matched.len().min(max_files));
    let mut entries: Vec<FileOutlineEntry> = matched
        .iter()
        .map(|f| outline_by_path(graph, f, f))
        .collect();
    if !overflow.is_empty() {
        entries.push(FileOutlineEntry {
            query: query.to_string(),
            file: None,
            symbols: vec![],
            // The names are the useful part: the caller can outline exactly
            // the ones it wants without re-running a broader glob.
            candidates: overflow.iter().take(50).cloned().collect(),
            error: Some(format!(
                "'{}' matches {} more file(s) than the {}-file cap — outline them by name, narrow the pattern, or raise max_files.",
                glob,
                overflow.len(),
                max_files
            )),
        });
    }
    entries
}

/// Resolve `path` to one indexed file — exact repo-relative match first, then
/// a unique path suffix — and list its symbols in line order.
fn outline_by_path(graph: &GraphData, query: &str, path: &str) -> FileOutlineEntry {
    let mut resolved: Option<String> = graph
        .nodes
        .iter()
        .find(|n| n.file.as_deref() == Some(path))
        .map(|_| path.to_string());

    if resolved.is_none() {
        let suffix = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
        let mut matches: Vec<String> = graph
            .nodes
            .iter()
            .filter_map(|n| n.file.as_ref())
            .filter(|f| f.as_str() == path || f.ends_with(&suffix))
            .cloned()
            .collect();
        matches.sort();
        matches.dedup();
        if matches.len() > 1 {
            return FileOutlineEntry {
                query: query.to_string(),
                file: None,
                symbols: vec![],
                error: Some(format!(
                    "'{}' matches {} files — pass one of the candidates.",
                    path,
                    matches.len()
                )),
                candidates: matches,
            };
        }
        resolved = matches.into_iter().next();
    }

    let Some(resolved) = resolved else {
        return FileOutlineEntry {
            query: query.to_string(),
            file: None,
            symbols: vec![],
            candidates: vec![],
            error: Some(format!(
                "No indexed file matches '{}'. Pass a repo-relative path (project_overview lists the biggest files), or re-run ug gen if the file is new.",
                path
            )),
        };
    };

    let mut symbols: Vec<&GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| n.file.as_deref() == Some(resolved.as_str()))
        .filter(|n| !matches!(n.node_type, GraphNodeType::File | GraphNodeType::Folder))
        .collect();
    symbols.sort_by_key(|n| n.start_line.unwrap_or(0));

    FileOutlineEntry {
        query: query.to_string(),
        file: Some(resolved),
        symbols: symbols.iter().map(|n| SymbolRef::from_node(n)).collect(),
        candidates: vec![],
        error: None,
    }
}

pub fn render_file_outline(r: &FileOutlineResult, style: Render) -> String {
    let mut out = String::new();
    for (i, f) in r.files.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(e) = &f.error {
            line(&mut out, &format!("✗ {}", e));
            for c in &f.candidates {
                line(&mut out, &format!("- {}", c));
            }
            continue;
        }
        let path = f.file.as_deref().unwrap_or(&f.query);
        line(
            &mut out,
            &format!(
                "{} — {} symbol(s)",
                style.heading(&format!("Outline of {}", path)),
                f.symbols.len()
            ),
        );
        out.push('\n');
        for s in &f.symbols {
            let start = s.start_line.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
            let end = s.end_line.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
            // The id re-encodes `kind:path:name`, all of which the heading
            // (path) and this line (kind, name) already show — so it is
            // noise by default. `show_ids` puts it back for terminals and
            // for `--ids`.
            if r.show_ids {
                line(
                    &mut out,
                    &format!(
                        "- L{}-{}  {}  {}  id: {}",
                        start, end, s.node_type, style.bold(&s.name), style.id(&s.id)
                    ),
                );
            } else {
                line(
                    &mut out,
                    &format!("- L{}-{}  {}  {}", start, end, s.node_type, style.bold(&s.name)),
                );
            }
        }
    }
    next_actions(
        &mut out,
        style,
        &[
            ("get_code <id>", "to read one symbol"),
            ("get_code --file <path>", "for the whole file"),
        ],
    );
    out
}

// ---------------------------------------------------------------------------
// get_code
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCodeParams {
    /// Read exactly these symbols' line ranges.
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Repo-relative path, used when `node_id` is empty.
    pub file: Option<String>,
    #[serde(alias = "startLine", alias = "start")]
    pub start_line: Option<usize>,
    #[serde(alias = "endLine", alias = "end")]
    pub end_line: Option<usize>,
    /// The line window as one value — `"11-35"`, `"34-end"`, `"20"` — in the
    /// same dialect `analyze` uses for row windows, and parsed by the same
    /// code. `start_line`/`end_line` win when both are given, so the two
    /// spellings can never disagree about what was asked for.
    pub range: Option<String>,
    #[serde(alias = "maxChars")]
    pub max_chars: Option<usize>,
    /// Drop the leading doc-comment preview from each slice. Set by the
    /// CLI's `--no-doc` when the caller already saw it (e.g. via
    /// `find_symbols --include-docs`) and wants only the body.
    #[serde(default)]
    pub no_doc: bool,
}

const DEFAULT_MAX_CHARS: usize = 20_000;

#[derive(Debug, Clone, Serialize)]
pub struct CodeSlice {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Characters dropped to honour `max_chars`; 0 when nothing was cut.
    pub truncated_chars: usize,
    /// Set when the slice may not be what it claims. Two cases:
    /// - **live file, indexed copy disagrees** — the slice is current source
    ///   read from disk, but the node's recorded `start`/`end` came from an
    ///   older capture, so the span may point at the wrong lines.
    /// - **indexed copy, file changed** — the slice is the stale captured
    ///   text served because the repo is absent.
    /// Either way the code is still returned; the flag tells the caller not
    /// to trust line numbers as current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub use crate::storage::source::{IndexedSource, StoredSource};

/// Where a read tool gets source text from, in priority order.
///
/// `indexed` is the copy ingest captured into the project's store. It is
/// the answer that needs no repo — the whole point of the type — and it is
/// also the *consistent* answer, since it is what the node's description
/// and embedding were built from.
///
/// `repo_root` points at the working tree. [`get_code`] prefers it over the
/// captured copy whenever the file actually exists on disk: an agent
/// reading source in a live editing session needs the current lines, and a
/// capture is only ever as current as the last `ug gen`. The captured
/// copy stays the fallback for when the repo is absent (a moved checkout,
/// or a machine that only ever held the index), and the two are compared
/// so a drift between them is flagged rather than silently trusted.
///
/// It is allowed to point at nothing — a path that no longer exists simply
/// means the working-tree half yields nothing, not an error.
#[derive(Clone, Copy)]
pub struct SourceCtx<'a> {
    indexed: Option<&'a IndexedSource>,
    repo_root: &'a Path,
}

impl<'a> SourceCtx<'a> {
    /// The full context: indexed source + working tree, live file preferred.
    pub fn new(indexed: &'a IndexedSource, repo_root: &'a Path) -> Self {
        SourceCtx {
            indexed: Some(indexed),
            repo_root,
        }
    }

    /// Working tree only, for callers with no store at hand (tests, and
    /// the legacy path where a project has a graph but was never ingested).
    pub fn repo_only(repo_root: &'a Path) -> Self {
        SourceCtx {
            indexed: None,
            repo_root,
        }
    }

    pub fn repo_root(&self) -> &'a Path {
        self.repo_root
    }

    /// Captured source for one node id.
    pub fn node(&self, id: &str) -> Option<&'a StoredSource> {
        self.indexed?.node(id)
    }

    /// Captured source for a whole file, via its File node.
    ///
    /// The indexer emits one range-less node per file whose capture is the
    /// entire file (see [`capture_graph_code`]), which is what makes an
    /// arbitrary `get_code --file X --start 180 --end 210` answerable from
    /// the index instead of from disk.
    ///
    /// [`capture_graph_code`]: crate::storage::capture_graph_code
    pub fn file(&self, graph: &GraphData, file: &str) -> Option<&'a StoredSource> {
        let indexed = self.indexed?;
        whole_file_node_ids(graph, file)
            .into_iter()
            .find_map(|id| indexed.node(id))
    }
}

/// Ids of the range-less nodes that carry `file`'s whole-file capture.
///
/// Plural because a File node and a Config node can both cover one path;
/// callers take the first that actually has captured code.
pub fn whole_file_node_ids<'a>(graph: &'a GraphData, file: &str) -> Vec<&'a str> {
    graph
        .nodes
        .iter()
        .filter(|n| n.start_line.is_none() && n.file.as_deref() == Some(file))
        .map(|n| n.id.as_str())
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct GetCodeResult {
    pub slices: Vec<CodeSlice>,
}

impl GetCodeResult {
    pub fn ok(&self) -> bool {
        self.slices.iter().all(|s| s.error.is_none())
    }
}

/// Read source for nodes, or for a file/line range.
///
/// The working tree wins when it has the file — an agent reading source in
/// a live session wants current lines, not a capture — and the indexed copy
/// answers when the repo is not on this machine. Whichever copy is served,
/// a difference between the two is flagged on the slice (`stale`) rather
/// than silently trusted, so the caller knows whether a line range is the
/// current truth or the node's recorded span.
pub fn get_code(graph: &GraphData, src: SourceCtx, p: &GetCodeParams) -> GetCodeResult {
    let max_chars = p.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
    let mut slices = Vec::new();

    // Resolve the line window before reading anything: a malformed `range`
    // is the caller's mistake, and reporting it beats silently serving line
    // 1 to EOF as if that were what they asked for.
    let (start_line, end_line) = match line_window(p) {
        Ok(w) => w,
        Err(e) => return GetCodeResult { slices: vec![err_slice(p.file.as_deref().unwrap_or(""), e)] },
    };

    if p.node_id.is_empty() {
        let Some(file) = p.file.as_deref() else {
            return GetCodeResult {
                slices: vec![CodeSlice {
                    title: String::new(),
                    file: None,
                    start_line: None,
                    end_line: None,
                    total_lines: None,
                    doc: None,
                    code: None,
                    truncated_chars: 0,
                    stale: None,
                    error: Some("Pass node_id (one or more ids) or file.".into()),
                }],
            };
        };
        let file = strip_file_id_prefix(file);
        slices.push(read_slice(
            graph,
            src,
            file,
            start_line.unwrap_or(1),
            end_line.unwrap_or(usize::MAX),
            None,
            max_chars,
        ));
        return GetCodeResult { slices };
    }

    for id in &expand_node_refs(graph, &p.node_id, MAX_REF_EXPANSION) {
        let Some(n) = graph.nodes.iter().find(|n| n.id == *id) else {
            slices.push(err_slice(
                id,
                unresolved_ref_error(graph, id, MAX_REF_EXPANSION),
            ));
            continue;
        };
        let Some(f) = &n.file else {
            slices.push(err_slice(
                id,
                format!(
                    "Node '{}' ({}) has no source file.",
                    id,
                    node_type_str(&n.node_type)
                ),
            ));
            continue;
        };
        let start = n.start_line.unwrap_or(1) as usize;
        // No end line means "the whole file" (File nodes carry no range at
        // all), not "one line".
        let end = n.end_line.map(|v| v as usize).unwrap_or({
            if n.start_line.is_some() {
                start
            } else {
                usize::MAX
            }
        });
        // Live working tree first — current by definition — then the index
        // for when the repo is absent. The captured hash is passed through
        // so a live read that disagrees with what was indexed can be flagged.
        let indexed_hash = src.node(id).map(|s| s.file_hash.as_str());
        slices.push(
            live_slice(src.repo_root(), f, start, end, Some(n), max_chars, indexed_hash)
                .unwrap_or_else(|| match src.node(id) {
                    Some(stored) => {
                        stored_slice(stored, src.repo_root(), f, start, end, n, max_chars)
                    }
                    None => read_slice(graph, src, f, start, end, Some(n), max_chars),
                }),
        );
    }

    let mut result = GetCodeResult { slices };
    if p.no_doc {
        for s in &mut result.slices {
            s.doc = None;
        }
    }
    result
}

/// Build a slice from indexed source, flagging it when the file it came
/// from no longer hashes the same.
fn stored_slice(
    src: &StoredSource,
    repo_root: &Path,
    file: &str,
    start: usize,
    end: usize,
    node: &GraphNode,
    max_chars: usize,
) -> CodeSlice {
    let total_lines = src.code.lines().count();
    let (code, truncated_chars) = if src.code.len() > max_chars {
        let cut = src.code.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(src.code.len());
        (src.code[..cut].to_string(), src.code.len() - cut)
    } else {
        (src.code.clone(), 0)
    };
    let stale = stale_note(repo_root, file, &src.file_hash);
    CodeSlice {
        title: format!("{} {}", node_type_str(&node.node_type), node.name),
        file: Some(file.to_string()),
        start_line: Some(start),
        end_line: Some(if end == usize::MAX { total_lines } else { end }),
        total_lines: Some(total_lines),
        doc: node.docstring.clone(),
        code: Some(code),
        truncated_chars,
        stale,
        error: None,
    }
}

/// The `(start, end)` lines a `get_code` call asks for, from either spelling.
///
/// `range` is parsed by [`crate::analyze::range`] — the same parser behind
/// `analyze`'s row windows — so `--range 11-35` means the same shape of
/// thing in both commands and every spelling that works in one works in the
/// other (`11-35`, `11..35`, `34-end`, `34-`, `20`, `top 20`). What it does
/// *not* borrow is that module's `MAX_WINDOW` row cap: a line window is
/// bounded by `max_chars` instead, and silently truncating a 300-line
/// function at line 200 would be a wrong answer that looks right.
///
/// Explicit `start_line`/`end_line` win over `range`, so a caller that sets
/// both gets the more specific one rather than an arbitrary tiebreak.
fn line_window(p: &GetCodeParams) -> Result<(Option<usize>, Option<usize>), String> {
    let Some(raw) = p.range.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((p.start_line, p.end_line));
    };
    let w = crate::analyze::range::parse(raw).ok_or_else(|| {
        format!(
            "Could not read {:?} as a line range. Use a count (`20` = the first 20 lines), \
             a closed range (`11-35`), or an open one (`34-end`).",
            raw
        )
    })?;
    Ok((
        p.start_line.or(Some(w.start)),
        p.end_line.or(w.end),
    ))
}

fn err_slice(title: &str, error: String) -> CodeSlice {
    CodeSlice {
        title: title.to_string(),
        file: None,
        start_line: None,
        end_line: None,
        total_lines: None,
        doc: None,
        code: None,
        truncated_chars: 0,
        stale: None,
        error: Some(error),
    }
}

/// Slice `start..=end` out of a whole file, from the working tree when it
/// has it and from the index otherwise.
///
/// The working tree is tried first — for a file/line-range read the caller
/// almost always wants the *current* lines (an editing agent paging through
/// a symbol), and the index is only the fallback for when the repo is not
/// on this machine. The index also covers arbitrary ranges, since the file's
/// range-less node holds the entire text rather than one symbol's span.
fn read_slice(
    graph: &GraphData,
    src: SourceCtx,
    file: &str,
    start: usize,
    end: usize,
    node: Option<&GraphNode>,
    max_chars: usize,
) -> CodeSlice {
    let title = match node {
        Some(n) => format!("{} {}", node_type_str(&n.node_type), n.name),
        None => file.to_string(),
    };

    let indexed_hash = src.file(graph, file).map(|s| s.file_hash.as_str());
    if let Some(slice) = live_slice(src.repo_root(), file, start, end, node, max_chars, indexed_hash) {
        return slice;
    }

    // Repo absent (or file gone): the indexed copy is the only source left.
    match src.file(graph, file) {
        Some(s) => {
            let all: Vec<&str> = s.code.split('\n').collect();
            let from = start.max(1).min(all.len());
            let to = end.min(all.len()).max(from);
            let mut text = all[from - 1..to].join("\n");
            let char_count = text.chars().count();
            let mut truncated = 0;
            if char_count > max_chars {
                truncated = char_count - max_chars;
                text = text.chars().take(max_chars).collect();
            }
            CodeSlice {
                title,
                file: Some(file.to_string()),
                start_line: Some(from),
                end_line: Some(to),
                total_lines: Some(all.len()),
                doc: node.and_then(|n| n.docstring.clone()),
                code: Some(text),
                truncated_chars: truncated,
                // The working tree could not be read at all, so this capture
                // is the answer. `stale_note` only fires when the file *is*
                // on disk but no longer hashes the same — which here means
                // "readable but changed since capture", the signal worth carrying.
                stale: stale_note(src.repo_root(), file, &s.file_hash),
                error: None,
            }
        }
        None => err_slice(&title, unreadable_reason(src.repo_root(), file)),
    }
}

/// Slice `start..=end` out of the file as it currently sits on disk, or
/// `None` when the file is not readable from the working tree.
///
/// The live copy is current by definition — the reason it is preferred — but
/// a node's recorded `start`/`end` were captured at index time, so when the
/// live file disagrees with the indexed hash the slice carries a `stale`
/// note: the lines shown are real and current, but they came from a span that
/// may have moved. `indexed_hash` is `None` when nothing was captured, in
/// which case there is nothing to disagree with and no flag is set.
fn live_slice(
    repo_root: &Path,
    file: &str,
    start: usize,
    end: usize,
    node: Option<&GraphNode>,
    max_chars: usize,
    indexed_hash: Option<&str>,
) -> Option<CodeSlice> {
    let content = std::fs::read_to_string(repo_root.join(file)).ok()?;
    let all: Vec<&str> = content.split('\n').collect();
    let from = start.max(1).min(all.len());
    let to = end.min(all.len()).max(from);
    let mut text = all[from - 1..to].join("\n");
    let char_count = text.chars().count();
    let mut truncated = 0;
    if char_count > max_chars {
        truncated = char_count - max_chars;
        text = text.chars().take(max_chars).collect();
    }
    let title = match node {
        Some(n) => format!("{} {}", node_type_str(&n.node_type), n.name),
        None => file.to_string(),
    };
    // Only flag when there is an indexed copy to compare against, and only
    // when the live file actually differs from it. A matching hash means the
    // span and the source agree.
    let stale = indexed_hash.and_then(|h| {
        let live = blake3::hash(content.as_bytes()).to_hex();
        if live.as_str() == h {
            None
        } else {
            Some(format!(
                "{} has changed since indexing — showing the live working tree; \
                 the recorded span may be stale, re-run `ug gen` to refresh",
                file
            ))
        }
    });
    Some(CodeSlice {
        title,
        file: Some(file.to_string()),
        start_line: Some(from),
        end_line: Some(to),
        total_lines: Some(all.len()),
        doc: node.and_then(|n| n.docstring.clone()),
        code: Some(text),
        truncated_chars: truncated,
        stale,
        error: None,
    })
}

/// Why neither the index nor the working tree could produce `file`.
///
/// Distinguishes a deleted repo from a deleted file: the fix is different
/// (restore the path vs re-run `ug gen`), and "not found under repo root"
/// reads as a lie when the root itself is gone.
fn unreadable_reason(repo_root: &Path, file: &str) -> String {
    if !repo_root.exists() {
        format!(
            "{} was not captured in the index, and the repo path {} is no longer available — \
             re-run `ug gen` from a checkout to capture it",
            file,
            repo_root.display()
        )
    } else {
        format!(
            "{} not found under repo root {} — the index may be stale (re-run ug gen).",
            file,
            repo_root.display()
        )
    }
}

/// The warning to attach to indexed source whose file has since changed.
/// `None` when the file still matches, and also when it cannot be read at
/// all — a missing working tree is the expected case here, not a staleness
/// signal, so it must not raise a false alarm.
fn stale_note(repo_root: &Path, file: &str, file_hash: &str) -> Option<String> {
    match crate::storage::file_matches_hash(repo_root, file, file_hash) {
        Some(false) => Some(format!(
            "{} has changed since indexing — this is the indexed copy; re-run `ug gen` to refresh",
            file
        )),
        _ => None,
    }
}

pub fn render_get_code(r: &GetCodeResult, style: Render) -> String {
    let mut out = String::new();
    for (i, s) in r.slices.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(e) = &s.error {
            line(&mut out, &format!("✗ {}", e));
            continue;
        }
        line(
            &mut out,
            &format!(
                "{}  —  {}:{}-{}",
                style.bold(&s.title),
                s.file.as_deref().unwrap_or("?"),
                s.start_line.unwrap_or(0),
                s.end_line.unwrap_or(0)
            ),
        );
        if let Some(d) = &s.doc {
            line(&mut out, &style.dim(&format!("doc: {}", d)));
        }
        // Loud rather than dim: an agent acting on out-of-date source is
        // the failure this whole column exists to make visible.
        if let Some(why) = &s.stale {
            line(&mut out, &format!("⚠ {}", why));
        }
        out.push('\n');
        if style == Render::Markdown {
            line(&mut out, "```");
        }
        line(&mut out, s.code.as_deref().unwrap_or(""));
        if style == Render::Markdown {
            line(&mut out, "```");
        }
        if s.truncated_chars > 0 {
            out.push('\n');
            line(
                &mut out,
                &style.dim(&format!(
                    "(truncated — {} more chars; narrow the line range or raise max_chars)",
                    s.truncated_chars
                )),
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// find_usages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FindUsagesParams {
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Transitive depth, 1-3. Default 1 = direct users only.
    pub hops: Option<u32>,
    /// Defaults to [`USAGE_EDGE_TYPES`].
    #[serde(alias = "edgeTypes", deserialize_with = "de_one_or_many")]
    pub edge_types: Vec<String>,
}

/// A line inside a caller that mentions the subject by name. Heuristic —
/// the name could appear in a comment or string — but each hit is a
/// jumpable `file:line` and saves a `get_code` round-trip per caller.
#[derive(Debug, Clone, Serialize)]
pub struct CallSite {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// 1 = direct user of the subject; 2+ = reached transitively.
    pub depth: u32,
    /// Edge type connecting this user to `via_target`.
    pub via_edge: String,
    /// The node this user points at — the subject itself at depth 1.
    pub via_target: String,
    /// Populated for direct (depth 1) users only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSite>,
}

/// How many direct callers get a source scan, and how many matching lines
/// each contributes. Bounds the file reads on a hot symbol with hundreds of
/// callers.
const CALL_SITE_CALLER_CAP: usize = 20;
const CALL_SITE_PER_CALLER: usize = 3;
const CALL_SITE_TEXT_CHARS: usize = 160;

#[derive(Debug, Clone, Serialize)]
pub struct UsagesEntry {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<SymbolRef>,
    pub users: Vec<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindUsagesResult {
    pub hops: u32,
    pub edge_types: Vec<String>,
    pub nodes: Vec<UsagesEntry>,
}

impl FindUsagesResult {
    pub fn ok(&self) -> bool {
        self.nodes.iter().all(|n| n.error.is_none())
    }
}

/// Which direct users get their source scanned for call sites: depth-1,
/// file-bearing, capped.
///
/// Returned as indices so the scan and the pre-fetch that feeds it
/// ([`find_usages_source_ids`]) share one definition — a transport that
/// fetched a different set than the scan reads would silently produce
/// call-site-less results with no way to tell why.
fn call_site_candidates(users: &[Usage]) -> Vec<usize> {
    users
        .iter()
        .enumerate()
        .filter(|(_, u)| u.depth == 1 && u.symbol.file.is_some())
        .map(|(i, _)| i)
        .take(CALL_SITE_CALLER_CAP)
        .collect()
}

/// The caller's source, as lines, plus the 1-based file line its first
/// entry corresponds to.
///
/// Three sources, in order: the caller node's own captured span (exact, and
/// needs nothing on disk), the file's whole-file capture, then the working
/// tree. `files` caches the latter two so several callers in one file cost
/// a single lookup.
fn caller_lines(
    graph: &GraphData,
    src: SourceCtx,
    caller: &SymbolRef,
    file: &str,
    files: &mut HashMap<String, Option<Vec<String>>>,
) -> Option<(Vec<String>, usize)> {
    // The span capture is already exactly the caller's lines, so its first
    // line is the caller's start line and no clamping is needed.
    if let Some(stored) = src.node(&caller.id) {
        let lines: Vec<String> = stored.code.lines().map(|s| s.to_string()).collect();
        if !lines.is_empty() {
            return Some((lines, caller.start_line.unwrap_or(1) as usize));
        }
    }

    let whole = files.entry(file.to_string()).or_insert_with(|| {
        src.file(graph, file)
            .map(|s| s.code.clone())
            .or_else(|| std::fs::read_to_string(src.repo_root().join(file)).ok())
            .map(|c| c.split('\n').map(|s| s.to_string()).collect())
    });
    let whole = whole.as_ref()?;

    let from = caller.start_line.unwrap_or(1).saturating_sub(1) as usize;
    let to = caller
        .end_line
        .map(|e| e as usize)
        .unwrap_or(whole.len())
        .min(whole.len());
    if from >= to {
        return None;
    }
    Some((whole[from..to].to_vec(), from + 1))
}

/// Scan a caller's own source slice for lines mentioning `target_name`.
fn call_sites_for(
    graph: &GraphData,
    src: SourceCtx,
    caller: &SymbolRef,
    target_name: &str,
    files: &mut HashMap<String, Option<Vec<String>>>,
) -> Vec<CallSite> {
    if target_name.is_empty() {
        return vec![];
    }
    let Some(file) = caller.file.clone() else {
        return vec![];
    };
    let Some((lines, first_line)) = caller_lines(graph, src, caller, &file, files) else {
        return vec![];
    };

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.contains(target_name) {
            out.push(CallSite {
                line: first_line + i,
                text: line.trim().chars().take(CALL_SITE_TEXT_CHARS).collect(),
            });
            if out.len() >= CALL_SITE_PER_CALLER {
                break;
            }
        }
    }
    out
}

/// Node ids whose captured source a `find_usages` call will read.
///
/// Transports call this before the tool, fetch the ids from the project's
/// store, and pass the result back in — which is what lets the call-site
/// evidence come from the index rather than the working tree. The walk it
/// repeats is an in-memory pass over `graph.edges`; the alternative, a live
/// store handle inside the tool, would make a synchronous function block
/// inside whichever async runtime called it.
pub fn find_usages_source_ids(graph: &GraphData, p: &FindUsagesParams) -> Vec<String> {
    let walked = walk_usages(graph, p);
    let mut ids = Vec::new();
    for entry in &walked.nodes {
        for i in call_site_candidates(&entry.users) {
            let u = &entry.users[i];
            ids.push(u.symbol.id.clone());
            // The file's whole-file capture backs up any caller whose own
            // span was never captured.
            if let Some(f) = &u.symbol.file {
                ids.extend(whole_file_node_ids(graph, f).into_iter().map(String::from));
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

pub fn find_usages(
    graph: &GraphData,
    src: SourceCtx,
    p: &FindUsagesParams,
) -> FindUsagesResult {
    let mut result = walk_usages(graph, p);
    for entry in &mut result.nodes {
        // Evidence for direct users only: transitive ones don't mention the
        // subject by name, so scanning them would just produce noise.
        let Some(subject_name) = entry.subject.as_ref().map(|s| s.name.clone()) else {
            continue;
        };
        let mut files: HashMap<String, Option<Vec<String>>> = HashMap::new();
        for i in call_site_candidates(&entry.users) {
            let caller = entry.users[i].symbol.clone();
            entry.users[i].call_sites =
                call_sites_for(graph, src, &caller, &subject_name, &mut files);
        }
    }
    result
}

/// The inbound graph walk behind [`find_usages`], with no call sites
/// attached — the part that needs only `graph.json`.
fn walk_usages(graph: &GraphData, p: &FindUsagesParams) -> FindUsagesResult {
    let hops = p.hops.unwrap_or(1).clamp(1, 3);
    let edge_types: Vec<String> = if p.edge_types.is_empty() {
        USAGE_EDGE_TYPES.iter().map(|s| s.to_string()).collect()
    } else {
        p.edge_types.iter().map(|t| t.to_lowercase()).collect()
    };

    let by_id = by_id_map(graph);

    // Inbound adjacency, built once and shared across the batch: edges that
    // *end* at a node — their sources are its users.
    let mut inbound: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    for e in &graph.edges {
        let et = edge_type_str(&e.edge_type);
        if edge_types.contains(&et.to_lowercase()) {
            inbound
                .entry(e.target.as_str())
                .or_default()
                .push((e.source.as_str(), et));
        }
    }

    let mut nodes = Vec::new();
    for node_id in &expand_node_refs(graph, &p.node_id, MAX_REF_EXPANSION) {
        let Some(subject) = by_id.get(node_id.as_str()) else {
            nodes.push(UsagesEntry {
                query: node_id.clone(),
                subject: None,
                users: vec![],
                error: Some(unresolved_ref_error(graph, node_id, MAX_REF_EXPANSION)),
            });
            continue;
        };

        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(node_id.as_str());
        let mut users: Vec<Usage> = Vec::new();
        let mut frontier: Vec<&str> = vec![node_id.as_str()];
        for depth in 1..=hops {
            let mut next: Vec<&str> = Vec::new();
            for target in &frontier {
                let Some(sources) = inbound.get(target) else {
                    continue;
                };
                for (src, et) in sources {
                    if seen.insert(src) {
                        let symbol = by_id
                            .get(src)
                            .map(|n| SymbolRef::from_node(n))
                            .unwrap_or_else(|| SymbolRef {
                                id: (*src).to_string(),
                                name: "(unknown node)".into(),
                                node_type: "?".into(),
                                file: None,
                                start_line: None,
                                end_line: None,
                                doc: None,
                                boundary: None,
                            });
                        users.push(Usage {
                            symbol,
                            depth,
                            via_edge: (*et).to_string(),
                            via_target: (*target).to_string(),
                            call_sites: vec![],
                        });
                        next.push(src);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }

        nodes.push(UsagesEntry {
            query: node_id.clone(),
            subject: Some(SymbolRef::from_node(subject)),
            users,
            error: None,
        });
    }

    FindUsagesResult {
        hops,
        edge_types,
        nodes,
    }
}

/// "3 of 14 users are system boundaries: 2 http.endpoint, 1 mq.listener."
///
/// `None` when none of them are, so the line only appears when it changes
/// the reader's conclusion.
fn boundary_summary(users: &[Usage]) -> Option<String> {
    let hits: Vec<&str> = users
        .iter()
        .filter_map(|u| u.symbol.boundary.as_deref())
        .collect();
    if hits.is_empty() {
        return None;
    }

    // Count by kind, dropping the direction prefix and the detail — the
    // breakdown is meant to say what sort of contract is at stake, not to
    // re-list every route.
    let mut kinds: Vec<(&str, usize)> = Vec::new();
    for label in &hits {
        for part in label.split(", ") {
            let Some(kind) = part.split_once(':').map(|(_, k)| k) else {
                continue;
            };
            let kind = kind.split(' ').next().unwrap_or(kind);
            match kinds.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => kinds.push((kind, 1)),
            }
        }
    }
    kinds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let breakdown: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();

    Some(format!(
        "⊕ {} of {} user(s) are system boundaries: {}.",
        hits.len(),
        users.len(),
        breakdown.join(", ")
    ))
}

pub fn render_find_usages(r: &FindUsagesResult, style: Render) -> String {
    let mut out = String::new();
    let names: HashMap<&str, &str> = r
        .nodes
        .iter()
        .flat_map(|e| {
            e.users
                .iter()
                .map(|u| (u.symbol.id.as_str(), u.symbol.name.as_str()))
                .chain(e.subject.iter().map(|s| (s.id.as_str(), s.name.as_str())))
        })
        .collect();

    for (i, e) in r.nodes.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(err) = &e.error {
            line(&mut out, &format!("✗ {}", err));
            continue;
        }
        let subject = e.subject.as_ref().expect("subject set when error is none");
        line(
            &mut out,
            &format!(
                "{}  {}",
                style.heading(&format!("Usages of {} {}", subject.node_type, subject.name)),
                style.dim(&subject.loc())
            ),
        );
        line(
            &mut out,
            &style.dim(&format!(
                "hops={} · edges=[{}] · {} user(s)",
                r.hops,
                r.edge_types.join(", "),
                e.users.len()
            )),
        );
        // "What breaks if I change this" is the question this tool claims to
        // answer, and a user count alone cannot: eleven internal callers are
        // a refactor, one REST handler among them is an API change. Called
        // out above the list because it decides whether the list needs
        // reading at all.
        if let Some(summary) = boundary_summary(&e.users) {
            line(&mut out, &style.bold(&summary));
        }
        out.push('\n');

        if e.users.is_empty() {
            line(
                &mut out,
                &format!("Nothing points at this node via [{}].", r.edge_types.join(", ")),
            );
            line(
                &mut out,
                &format!(
                    "Try more hops, different edge types ({} lists what this graph has), or {} for outbound dependencies.",
                    style.id("graph_schema"),
                    style.id("traverse")
                ),
            );
            continue;
        }

        for u in &e.users {
            let via = if u.depth > 1 {
                let target = names.get(u.via_target.as_str()).copied().unwrap_or(&u.via_target);
                style.dim(&format!("—{}→ {} (hop {})", u.via_edge, target, u.depth))
            } else {
                style.dim(&format!("—{}→", u.via_edge))
            };
            line(
                &mut out,
                &format!(
                    "- {} {}  {} {}",
                    u.symbol.node_type,
                    style.bold(&u.symbol.name),
                    style.dim(&u.symbol.loc()),
                    via
                ),
            );
            line(&mut out, &format!("  id: {}", style.id(&u.symbol.id)));
            // The summary line above says how many users are boundaries;
            // this says which, so the reader does not have to re-derive it
            // from the names.
            if let Some(b) = &u.symbol.boundary {
                line(&mut out, &format!("  {}", style.bold(&format!("boundary: {}", b))));
            }
            for site in &u.call_sites {
                line(
                    &mut out,
                    &format!(
                        "    {}:{}  {}",
                        u.symbol.file.as_deref().unwrap_or("?"),
                        site.line,
                        style.id(&site.text)
                    ),
                );
            }
        }
        if e.users.iter().any(|u| !u.call_sites.is_empty()) {
            line(
                &mut out,
                &style.dim(
                    "(call-site lines matched by name — a hit inside a comment or string is possible)",
                ),
            );
        }
    }
    next_actions(
        &mut out,
        style,
        &[
            ("get_code <id>", "to read a caller"),
            ("find_usages <id> --hops 2", "for transitive users"),
        ],
    );
    out
}

// ---------------------------------------------------------------------------
// traverse
// ---------------------------------------------------------------------------

/// Which way edges are followed. `Outbound` = what the seed depends on,
/// `Inbound` = what depends on the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    #[default]
    Outbound,
    Inbound,
    Both,
}

impl Dir {
    fn from_str_lossy(s: &str) -> Dir {
        match s.to_lowercase().as_str() {
            "in" | "inbound" | "reverse" => Dir::Inbound,
            "both" | "all" => Dir::Both,
            _ => Dir::Outbound,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TraverseParams {
    #[serde(
        alias = "nodeId",
        alias = "nodeIds",
        // The MCP tool's original spelling, kept working.
        alias = "startNodeIds",
        deserialize_with = "de_one_or_many"
    )]
    pub node_id: Vec<String>,
    /// Hop radius, 1-5. Default 2.
    pub hops: Option<u32>,
    #[serde(alias = "edgeTypes", deserialize_with = "de_one_or_many")]
    pub edge_types: Vec<String>,
    pub direction: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraversedNode {
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// Hops from the nearest seed; 0 for the seeds themselves.
    pub distance: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraversedEdge {
    pub source: String,
    pub target: String,
    pub edge_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraverseResult {
    pub seeds: Vec<String>,
    pub hops: u32,
    pub direction: Dir,
    pub edge_types: Vec<String>,
    pub nodes: Vec<TraversedNode>,
    pub edges: Vec<TraversedEdge>,
    /// Seeds that named no node, as the caller wrote them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// One explanation per entry in `missing`, written where the graph is
    /// still in hand — a name that matches nothing, a pattern that matches
    /// nothing, and a pattern that matched too much are three different
    /// problems, and the renderer cannot tell them apart on its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl TraverseResult {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }
}

/// N-hop walk over graph.json from the given seeds.
///
/// The general form of [`find_usages`], which is the same walk pinned to
/// `Inbound` with a default edge-type set — so both now read the same
/// in-memory graph rather than one going to the database.
pub fn traverse(graph: &GraphData, p: &TraverseParams) -> TraverseResult {
    let hops = p.hops.unwrap_or(2).clamp(1, 5);
    let direction = p
        .direction
        .as_deref()
        .map(Dir::from_str_lossy)
        .unwrap_or(Dir::Outbound);
    let edge_filter: Vec<String> = p.edge_types.iter().map(|t| t.to_lowercase()).collect();

    let by_id = by_id_map(graph);

    // Adjacency built once, honouring the edge-type filter.
    let mut out_adj: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    let mut in_adj: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    for e in &graph.edges {
        let et = edge_type_str(&e.edge_type);
        if !edge_filter.is_empty() && !edge_filter.contains(&et.to_lowercase()) {
            continue;
        }
        out_adj
            .entry(e.source.as_str())
            .or_default()
            .push((e.target.as_str(), et));
        in_adj
            .entry(e.target.as_str())
            .or_default()
            .push((e.source.as_str(), et));
    }

    let mut missing = Vec::new();
    let mut distances: HashMap<&str, u32> = HashMap::new();
    let mut frontier: Vec<&str> = Vec::new();
    let mut seeds: Vec<String> = Vec::new();

    for id in &expand_node_refs(graph, &p.node_id, MAX_REF_EXPANSION) {
        match by_id.get(id.as_str()) {
            Some(n) => {
                seeds.push(id.clone());
                if distances.insert(n.id.as_str(), 0).is_none() {
                    frontier.push(n.id.as_str());
                }
            }
            None => missing.push(id.clone()),
        }
    }

    // Edges are collected as traversed, so the result only contains edges
    // that actually took part in the walk.
    let mut edges: Vec<TraversedEdge> = Vec::new();
    let mut seen_edges: HashSet<(&str, &str, &str)> = HashSet::new();

    // (adjacency, edge points away from the current node)
    let mut steps: Vec<(&HashMap<&str, Vec<(&str, &'static str)>>, bool)> = Vec::new();
    if matches!(direction, Dir::Outbound | Dir::Both) {
        steps.push((&out_adj, true));
    }
    if matches!(direction, Dir::Inbound | Dir::Both) {
        steps.push((&in_adj, false));
    }

    for depth in 1..=hops {
        let mut next: Vec<&str> = Vec::new();
        for node in &frontier {
            for (adj, forward) in &steps {
                let Some(neigh) = adj.get(*node) else { continue };
                for (other, et) in neigh {
                    let (src, tgt) = if *forward {
                        (*node, *other)
                    } else {
                        (*other, *node)
                    };
                    if seen_edges.insert((src, tgt, et)) {
                        edges.push(TraversedEdge {
                            source: src.to_string(),
                            target: tgt.to_string(),
                            edge_type: (*et).to_string(),
                        });
                    }
                    if !distances.contains_key(other) {
                        distances.insert(other, depth);
                        next.push(other);
                    }
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }

    let mut nodes: Vec<TraversedNode> = distances
        .iter()
        .filter_map(|(id, d)| {
            by_id.get(id).map(|n| TraversedNode {
                symbol: SymbolRef::from_node(n),
                distance: *d,
            })
        })
        .collect();
    // Nearest first, then stable by id so output doesn't shuffle run to run.
    nodes.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then(a.symbol.id.cmp(&b.symbol.id))
    });

    let notes = missing
        .iter()
        .map(|id| unresolved_ref_error(graph, id, MAX_REF_EXPANSION))
        .collect();

    TraverseResult {
        seeds,
        hops,
        direction,
        edge_types: edge_filter,
        nodes,
        edges,
        missing,
        notes,
    }
}

pub fn render_traverse(r: &TraverseResult, style: Render) -> String {
    let mut out = String::new();
    for note in &r.notes {
        line(&mut out, &format!("✗ {}", note));
    }
    if r.seeds.is_empty() {
        return out;
    }

    line(
        &mut out,
        &style.heading(&format!("Traversal from [{}]", r.seeds.join(", "))),
    );
    let filter = if r.edge_types.is_empty() {
        "all".to_string()
    } else {
        r.edge_types.join(", ")
    };
    line(
        &mut out,
        &style.dim(&format!(
            "hops={} · dir={:?} · edges=[{}] · {} node(s), {} edge(s)",
            r.hops,
            r.direction,
            filter,
            r.nodes.len(),
            r.edges.len()
        )),
    );

    let mut depth = None;
    for n in &r.nodes {
        if depth != Some(n.distance) {
            depth = Some(n.distance);
            out.push('\n');
            line(
                &mut out,
                &style.bold(&format!(
                    "hop={}  ({} node(s))",
                    n.distance,
                    r.nodes.iter().filter(|x| x.distance == n.distance).count()
                )),
            );
        }
        line(
            &mut out,
            &format!(
                "- {} {}  {}  id: {}",
                n.symbol.node_type,
                style.bold(&n.symbol.name),
                style.dim(&n.symbol.loc()),
                style.id(&n.symbol.id)
            ),
        );
        // A traversal is how someone maps unfamiliar territory, and a
        // boundary in the neighbourhood is the landmark worth stopping at.
        if let Some(b) = &n.symbol.boundary {
            line(&mut out, &format!("  {}", style.bold(&format!("boundary: {}", b))));
        }
    }

    // Edge-type tally: the shape of the neighbourhood in one line.
    if !r.edges.is_empty() {
        let mut tally: HashMap<&str, usize> = HashMap::new();
        for e in &r.edges {
            *tally.entry(e.edge_type.as_str()).or_insert(0) += 1;
        }
        let mut pairs: Vec<(&str, usize)> = tally.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        out.push('\n');
        line(
            &mut out,
            &style.dim(&format!(
                "edges: {}",
                pairs
                    .iter()
                    .map(|(t, c)| format!("{}×{}", t, c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        );
    }

    next_actions(
        &mut out,
        style,
        &[
            ("get_code <id>", "to read any node above"),
            ("find_usages <id>", "for the inbound direction"),
        ],
    );
    out
}

// ---------------------------------------------------------------------------
// project_overview
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TypeCount {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hotspot {
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// Inbound edges excluding `Contains`, i.e. "how much code depends on this".
    pub in_degree: usize,
}

/// What the indexer recorded about the last run. Written into graph.json by
/// `ug graph` and, until now, read by nothing.
#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    pub files: usize,
    pub cached_files: usize,
    pub symbols: usize,
    pub folders: usize,
    pub lines: u64,
    /// Unix seconds; 0 when the indexer didn't record it.
    pub indexed_at: u64,
    pub indexing_time_ms: u64,
}

/// A symbol carrying tree-sitter metrics, for the "where is the hairy code"
/// view. `metrics` is populated for most Function/Class nodes.
#[derive(Debug, Clone, Serialize)]
pub struct ComplexityEntry {
    #[serde(flatten)]
    pub symbol: SymbolRef,
    pub loc: u32,
    pub params: u32,
    pub max_nesting: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectOverviewResult {
    pub repo_root: String,
    pub graph_path: String,
    pub node_count: usize,
    pub edge_count: usize,
    /// `code`, `docs` or `mixed`, as classified for the repo root folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kb_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexSummary>,
    /// File counts per language, from the repo-root folder node.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<TypeCount>,
    pub node_types: Vec<TypeCount>,
    pub edge_types: Vec<TypeCount>,
    pub biggest_files: Vec<TypeCount>,
    pub hotspots: Vec<Hotspot>,
    /// Largest symbols by lines of code.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub complexity: Vec<ComplexityEntry>,
}

fn top_counts<K: ToString + Copy>(m: &HashMap<K, usize>, k: usize) -> Vec<TypeCount> {
    let mut v: Vec<(K, usize)> = m.iter().map(|(key, c)| (*key, *c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_string().cmp(&b.0.to_string())));
    v.truncate(k);
    v.into_iter()
        .map(|(name, count)| TypeCount {
            name: name.to_string(),
            count,
        })
        .collect()
}

pub fn project_overview(
    graph: &GraphData,
    repo_root: &Path,
    graph_path: &Path,
) -> ProjectOverviewResult {
    let mut node_types: HashMap<&'static str, usize> = HashMap::new();
    let mut symbols_per_file: HashMap<&str, usize> = HashMap::new();
    for n in &graph.nodes {
        *node_types.entry(node_type_str(&n.node_type)).or_insert(0) += 1;
        if let Some(f) = &n.file {
            if !matches!(n.node_type, GraphNodeType::File | GraphNodeType::Folder) {
                *symbols_per_file.entry(f.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut edge_types: HashMap<&'static str, usize> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for e in &graph.edges {
        *edge_types.entry(edge_type_str(&e.edge_type)).or_insert(0) += 1;
        // Contains is pure structure (folder→file→symbol); skipping it makes
        // inbound degree mean "how much code depends on this".
        if !matches!(e.edge_type, GraphEdgeType::Contains) {
            *in_degree.entry(e.target.as_str()).or_insert(0) += 1;
        }
    }

    let by_id = by_id_map(graph);
    let hotspots = top_counts(&in_degree, 12)
        .into_iter()
        .filter_map(|tc| {
            by_id.get(tc.name.as_str()).map(|n| Hotspot {
                symbol: SymbolRef::from_node(n),
                in_degree: tc.count,
            })
        })
        .collect();

    // The repo-root folder node (shallowest depth) carries the language
    // breakdown and the code/docs/mixed classification the indexer computed.
    let root_folder = graph
        .nodes
        .iter()
        .filter_map(|n| n.folder.as_ref())
        .min_by_key(|f| f.depth);
    let languages = root_folder
        .map(|f| {
            let mut v: Vec<TypeCount> = f
                .language_breakdown
                .iter()
                .map(|(name, count)| TypeCount {
                    name: name.clone(),
                    count: *count as usize,
                })
                .collect();
            v.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
            v
        })
        .unwrap_or_default();
    let kb_type = root_folder
        .and_then(|f| f.classification.as_ref())
        .map(|c| format!("{:?}", c).to_lowercase());

    let index = graph.stats.as_ref().map(|s| IndexSummary {
        files: s.total_files,
        cached_files: s.cached_files,
        symbols: s.total_symbols,
        folders: s.total_folders,
        lines: s.total_lines,
        indexed_at: s.last_indexed_at,
        indexing_time_ms: s.indexing_time_ms,
    });

    let mut complexity: Vec<ComplexityEntry> = graph
        .nodes
        .iter()
        .filter_map(|n| {
            n.metrics.as_ref().map(|m| ComplexityEntry {
                symbol: SymbolRef::from_node(n),
                loc: m.loc,
                params: m.params,
                max_nesting: m.max_nesting,
            })
        })
        .collect();
    complexity.sort_by(|a, b| {
        b.loc
            .cmp(&a.loc)
            .then(b.max_nesting.cmp(&a.max_nesting))
            .then(a.symbol.id.cmp(&b.symbol.id))
    });
    complexity.truncate(10);

    ProjectOverviewResult {
        repo_root: repo_root.display().to_string(),
        graph_path: graph_path.display().to_string(),
        node_count: graph.nodes.len(),
        edge_count: graph.edges.len(),
        kb_type,
        index,
        languages,
        node_types: top_counts(&node_types, 10),
        edge_types: top_counts(&edge_types, 10),
        biggest_files: top_counts(&symbols_per_file, 10),
        hotspots,
        complexity,
    }
}

pub fn render_project_overview(r: &ProjectOverviewResult, style: Render) -> String {
    let mut out = String::new();
    line(&mut out, &style.heading("Project overview"));
    line(&mut out, &style.dim(&format!("repo: {}", r.repo_root)));
    line(&mut out, &style.dim(&format!("graph: {}", r.graph_path)));
    if let Some(kb) = &r.kb_type {
        line(&mut out, &style.dim(&format!("kind: {} knowledge base", kb)));
    }
    out.push('\n');

    // What the indexer saw, as opposed to what the graph ended up with.
    if let Some(ix) = &r.index {
        line(&mut out, &style.bold("Index"));
        line(
            &mut out,
            &format!(
                "- {} file(s), {} symbol(s), {} folder(s), {} line(s)",
                ix.files, ix.symbols, ix.folders, ix.lines
            ),
        );
        let cached = if ix.cached_files > 0 {
            format!(", {} reused from cache", ix.cached_files)
        } else {
            String::new()
        };
        line(
            &mut out,
            &format!("- built in {} ms{}", ix.indexing_time_ms, cached),
        );
        if ix.indexed_at > 0 {
            line(
                &mut out,
                &style.dim(&format!("- last indexed at epoch {}", ix.indexed_at)),
            );
        }
        out.push('\n');
    }

    if !r.languages.is_empty() {
        let total: usize = r.languages.iter().map(|l| l.count).sum();
        line(&mut out, &style.bold(&format!("Languages ({} files)", total)));
        line(
            &mut out,
            &format!(
                "  {}",
                r.languages
                    .iter()
                    .map(|l| format!("{}×{}", l.name, l.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        out.push('\n');
    }

    line(&mut out, &style.bold(&format!("Nodes ({})", r.node_count)));
    for t in &r.node_types {
        line(&mut out, &format!("- {}: {}", t.name, t.count));
    }
    out.push('\n');

    line(&mut out, &style.bold(&format!("Edges ({})", r.edge_count)));
    for t in &r.edge_types {
        line(&mut out, &format!("- {}: {}", t.name, t.count));
    }
    out.push('\n');

    line(&mut out, &style.bold("Biggest files (by symbol count)"));
    for f in &r.biggest_files {
        line(&mut out, &format!("- {}  ({})", f.name, f.count));
    }
    out.push('\n');

    line(
        &mut out,
        &format!(
            "{} {}",
            style.bold("Most depended-upon symbols"),
            style.dim("(inbound edges, excluding containment)")
        ),
    );
    for h in &r.hotspots {
        // No `id:` suffix: it reconstructs `kind:path:name`, all three of
        // which this line (kind, name) and the loc() span (path) already
        // show. `find_symbols <name>` gives the exact id if one is needed.
        line(
            &mut out,
            &format!(
                "- {} {}  ←{}  {}",
                h.symbol.node_type,
                style.bold(&h.symbol.name),
                h.in_degree,
                style.dim(&h.symbol.loc())
            ),
        );
    }
    if !r.complexity.is_empty() {
        out.push('\n');
        line(
            &mut out,
            &format!(
                "{} {}",
                style.bold("Largest symbols"),
                style.dim("(lines of code · params · max nesting)")
            ),
        );
        for c in &r.complexity {
            line(
                &mut out,
                &format!(
                    "- {} {}  {} loc · {}p · depth {}  {}",
                    c.symbol.node_type,
                    style.bold(&c.symbol.name),
                    c.loc,
                    c.params,
                    c.max_nesting,
                    style.dim(&c.symbol.loc())
                ),
            );
        }
    }

    next_actions(
        &mut out,
        style,
        &[
            ("file_outline <file>", "on a big file"),
            ("get_code <id>", "on a hotspot"),
            ("search <query>", "for a concept"),
        ],
    );
    out
}

// ---------------------------------------------------------------------------
// graph_schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EdgeShape {
    /// `Function→Function`
    pub shape: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeTypeInfo {
    pub name: String,
    pub count: usize,
    pub shapes: Vec<EdgeShape>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphSchemaResult {
    pub graph_path: String,
    pub node_types: Vec<TypeCount>,
    pub edge_types: Vec<EdgeTypeInfo>,
    pub vocabulary: Vec<String>,
    /// Whether this graph predates cross-file call resolution
    /// (`GRAPH_SCHEMA_VERSION` 3).
    ///
    /// A stale graph does not fail — it answers. It answers `find_usages`
    /// with callers a name-match invented and `dead_code` with symbols whose
    /// only caller was dropped, and both look exactly like correct answers.
    /// This is the manifest tool, so saying so here is the cheapest place to
    /// stop a wrong number being quoted as a right one.
    pub stale_call_graph: bool,
    /// How many nodes are system boundaries, by kind.
    ///
    /// Empty on a graph indexed before boundaries existed *and* on one that
    /// genuinely has none — the same ambiguity the store's facts avoid by
    /// omission. Here the graph is in hand, so `stale_boundaries` says which
    /// it is rather than leaving the reader to guess.
    pub boundary_kinds: Vec<TypeCount>,
    /// Whether this graph predates boundary detection
    /// (`GRAPH_SCHEMA_VERSION` 4), i.e. an empty `boundary_kinds` means "not
    /// measured" rather than "none".
    pub stale_boundaries: bool,
}

pub fn graph_schema(graph: &GraphData, graph_path: &Path) -> GraphSchemaResult {
    let mut node_counts: HashMap<&'static str, usize> = HashMap::new();
    for n in &graph.nodes {
        *node_counts.entry(node_type_str(&n.node_type)).or_insert(0) += 1;
    }

    let by_id = by_id_map(graph);
    let mut edge_counts: HashMap<&'static str, usize> = HashMap::new();
    // Keyed by (edge type, source node type, target node type) so the reader
    // learns not just which types exist but what they connect.
    let mut edge_shapes: HashMap<(&'static str, &'static str, &'static str), usize> = HashMap::new();
    for e in &graph.edges {
        let et = edge_type_str(&e.edge_type);
        *edge_counts.entry(et).or_insert(0) += 1;
        let st = by_id
            .get(e.source.as_str())
            .map(|n| node_type_str(&n.node_type))
            .unwrap_or("?");
        let tt = by_id
            .get(e.target.as_str())
            .map(|n| node_type_str(&n.node_type))
            .unwrap_or("?");
        *edge_shapes.entry((et, st, tt)).or_insert(0) += 1;
    }

    let mut edge_types: Vec<EdgeTypeInfo> = edge_counts
        .iter()
        .map(|(name, count)| {
            let mut shapes: Vec<EdgeShape> = edge_shapes
                .iter()
                .filter(|((et, _, _), _)| et == name)
                .map(|((_, st, tt), c)| EdgeShape {
                    shape: format!("{}→{}", st, tt),
                    count: *c,
                })
                .collect();
            shapes.sort_by(|a, b| b.count.cmp(&a.count).then(a.shape.cmp(&b.shape)));
            shapes.truncate(4);
            EdgeTypeInfo {
                name: name.to_string(),
                count: *count,
                shapes,
            }
        })
        .collect();
    edge_types.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));

    // Counted by kind, not by node: a symbol that is both an endpoint and a
    // db client is one of each, and summing the column would double it.
    let mut boundary_counts: HashMap<&str, usize> = HashMap::new();
    for n in &graph.nodes {
        for b in &n.boundaries {
            *boundary_counts.entry(b.kind.as_str()).or_insert(0) += 1;
        }
    }
    let mut boundary_kinds: Vec<TypeCount> = boundary_counts
        .into_iter()
        .map(|(name, count)| TypeCount {
            name: name.to_string(),
            count,
        })
        .collect();
    boundary_kinds.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));

    let schema = graph.stats.as_ref().map(|s| s.graph_schema_version);
    GraphSchemaResult {
        graph_path: graph_path.display().to_string(),
        node_types: top_counts(&node_counts, usize::MAX),
        edge_types,
        vocabulary: EDGE_TYPE_VOCABULARY.iter().map(|s| s.to_string()).collect(),
        // < 5, not < 3: version 4 resolved cross-file calls but still lost
        // every Rust `mod`-qualified one, which is the same failure mode
        // wearing a newer version number.
        stale_call_graph: schema.map(|v| v < 5).unwrap_or(true),
        boundary_kinds,
        stale_boundaries: schema.map(|v| v < 4).unwrap_or(true),
    }
}

pub fn render_graph_schema(r: &GraphSchemaResult, style: Render) -> String {
    let mut out = String::new();
    line(
        &mut out,
        &format!(
            "{}  {}",
            style.heading("Graph schema"),
            style.dim(&r.graph_path)
        ),
    );
    out.push('\n');

    line(&mut out, &style.bold("Node types in this graph:"));
    for t in &r.node_types {
        line(&mut out, &format!("  {:<12} {}", t.name, t.count));
    }
    out.push('\n');

    line(
        &mut out,
        &format!(
            "{} {}",
            style.bold("System boundaries"),
            style.dim("(where this code meets the outside world)")
        ),
    );
    if r.stale_boundaries {
        line(
            &mut out,
            &format!(
                "  {} {}",
                style.dim("NOT INDEXED — this graph predates boundary detection; run"),
                style.id("ug gen")
            ),
        );
    } else if r.boundary_kinds.is_empty() {
        line(&mut out, &format!("  {}", style.dim("none detected")));
    } else {
        for t in &r.boundary_kinds {
            line(&mut out, &format!("  {:<16} {}", t.name, t.count));
        }
    }
    out.push('\n');

    line(
        &mut out,
        &format!(
            "{} {}",
            style.bold("Edge types in this graph"),
            style.dim("(source type → target type)")
        ),
    );
    for e in &r.edge_types {
        let shapes = e
            .shapes
            .iter()
            .map(|s| format!("{} ({})", s.shape, s.count))
            .collect::<Vec<_>>()
            .join(", ");
        line(
            &mut out,
            &format!("  {:<12} {:<6} {}", e.name, e.count, style.dim(&shapes)),
        );
    }
    out.push('\n');

    line(
        &mut out,
        &format!(
            "{} {}",
            style.bold("Full edge-type vocabulary"),
            style.dim("(what indexers can emit — pass these to edge_types filters)")
        ),
    );
    line(&mut out, &format!("  {}", r.vocabulary.join(", ")));
    out.push('\n');

    line(&mut out, &style.dim("Notes:"));
    line(
        &mut out,
        "  • Edges are directed: Calls A→B means A calls B; inbound edges on B are its callers.",
    );
    line(
        &mut out,
        "  • Contains is structure (Folder→File→Symbol) — exclude it when you mean \"depends on\".",
    );
    if r.stale_call_graph {
        line(
            &mut out,
            "  • This graph predates the current call resolution, so some Calls edges were matched",
        );
        line(
            &mut out,
            "    by name — pointing at a same-named symbol the call site never meant — and module-path",
        );
        line(
            &mut out,
            "    calls are missing. Treat find_usages, impact and dead_code as indicative,",
        );
        line(&mut out, "    and run \"ug gen\" before relying on them.");
    }
    out
}

// ---------------------------------------------------------------------------
// shortest_path
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ShortestPathParams {
    #[serde(alias = "sourceId", alias = "from")]
    pub source: String,
    #[serde(alias = "targetId", alias = "to")]
    pub target: String,
    /// Don't retry the reverse direction when no forward path exists.
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShortestPathResult {
    pub source: String,
    pub target: String,
    pub found: bool,
    /// True when no forward path existed and the reverse direction was used.
    pub reversed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
    pub path: Vec<String>,
    pub nodes: Vec<SymbolRef>,
}

/// Shortest directed path between two node ids.
///
/// `raw` is the graph.json text — [`crate::find_shortest_path`] parses it
/// itself. Edges are directed; unless `strict`, the reverse direction is
/// retried when no forward path exists and the result is flagged `reversed`.
pub fn shortest_path(
    graph: &GraphData,
    source: &str,
    target: &str,
    strict: bool,
) -> ShortestPathResult {
    // Straight onto the parsed graph. This used to take the graph.json *text*
    // as well and hand it to `crate::find_shortest_path`, which cloned the
    // whole string, re-parsed it into a second `GraphData`, rebuilt the
    // adjacency, serialised its answer to JSON — and then this function parsed
    // that back. The `!found` retry below did the entire thing a second time.
    // All of it to answer a question about the `graph` already in hand.
    let mut reversed = false;
    let mut result = crate::find_shortest_path_graph(graph, source, target);
    if !result.found && !strict {
        reversed = true;
        result = crate::find_shortest_path_graph(graph, target, source);
    }

    let by_id = by_id_map(graph);
    let hops = result
        .length
        .unwrap_or(result.path.len().saturating_sub(1) as u32);

    ShortestPathResult {
        source: source.to_string(),
        target: target.to_string(),
        found: result.found,
        reversed: result.found && reversed,
        length: if result.found { Some(hops) } else { None },
        nodes: result
            .path
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).map(|n| SymbolRef::from_node(n)))
            .collect(),
        path: result.path,
    }
}

pub fn render_shortest_path(r: &ShortestPathResult, style: Render, strict: bool) -> String {
    let mut out = String::new();
    if !r.found {
        line(
            &mut out,
            &format!(
                "No directed path between {} and {}{}.",
                style.id(&r.source),
                style.id(&r.target),
                if strict {
                    " (strict: reverse direction not tried)"
                } else {
                    " in either direction"
                }
            ),
        );
        line(
            &mut out,
            &format!(
                // Every hint names a command that exists today; pointing at
                // one that does not is worse than giving no hint at all.
                "They may be connected only through shared ancestors — try {} from each end.",
                style.id("traverse <symbol> -d both")
            ),
        );
        return out;
    }

    let hops = r.length.unwrap_or(0);
    if r.reversed {
        line(
            &mut out,
            &format!(
                "{} {} — {} hop(s)",
                style.heading(&format!("Path {} → {}", r.target, r.source)),
                style.dim("(reverse direction — no forward path existed)"),
                hops
            ),
        );
    } else {
        line(
            &mut out,
            &format!(
                "{} — {} hop(s)",
                style.heading(&format!("Path {} → {}", r.source, r.target)),
                hops
            ),
        );
    }
    out.push('\n');

    let by_id: HashMap<&str, &SymbolRef> = r.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for (i, id) in r.path.iter().enumerate() {
        let desc = match by_id.get(id.as_str()) {
            Some(n) => format!(
                "{} {}  {}  id: {}",
                n.node_type,
                style.bold(&n.name),
                style.dim(&n.loc()),
                style.id(&n.id)
            ),
            None => format!("(unknown node) id: {}", style.id(id)),
        };
        line(&mut out, &format!("{} {}", if i == 0 { "·" } else { "↓" }, desc));
    }
    next_actions(
        &mut out,
        style,
        &[(
            "get_code <id>",
            "on any id above to see the code that makes the link",
        )],
    );
    out
}

// ---------------------------------------------------------------------------
// context — the curated bundle
// ---------------------------------------------------------------------------

/// The roles a [`ContextItem`] can carry, in the order they are assembled,
/// budgeted and rendered.
///
/// The order is a priority claim, not a taxonomy. Working on a symbol, an
/// agent needs its body before anything else; then who breaks if it changes;
/// then what proves it still works; then what it leans on; then the prose.
/// When the budget runs out it runs out from the right.
pub const CONTEXT_ROLES: &[&str] = &["target", "caller", "test", "dependency", "doc"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContextParams {
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    #[serde(alias = "maxChars")]
    pub max_chars: Option<usize>,
    /// Keep only these roles. Empty means all of [`CONTEXT_ROLES`].
    #[serde(deserialize_with = "de_one_or_many")]
    pub include: Vec<String>,
}

/// Default budget for one pack.
///
/// Deliberately well under `get_code`'s 20k: the point of this tool is to be
/// cheaper than the five calls it replaces, and a pack that costs more than
/// `get_code` alone would be a worse deal dressed as a better one.
const CONTEXT_DEFAULT_MAX_CHARS: usize = 12_000;

/// Ceiling on the share of the budget the target's own body may take, so one
/// enormous function cannot crowd out every caller and test — the parts that
/// answer questions the body cannot.
const CONTEXT_TARGET_SHARE: f64 = 0.5;

const CONTEXT_CALLERS_CAP: usize = 12;
const CONTEXT_TESTS_CAP: usize = 8;
const CONTEXT_DEPS_CAP: usize = 15;
const CONTEXT_DOCS_CAP: usize = 5;

/// How much prose a `doc` item carries. Enough to tell whether the section is
/// the one you want; `get_code` on its id gives the rest.
const CONTEXT_DOC_PREVIEW_CHARS: usize = 400;

/// Rendering overhead every pack pays regardless of contents: the header, the
/// id line, the budget line, the section rules and the trailing hint.
///
/// Charged to the budget up front rather than ignored, because `max_chars`
/// has to bound what the caller actually receives. Counting only the payload
/// made a 500-char pack return 894 — a 79% overshoot, worst exactly when the
/// caller is being careful about tokens.
const CONTEXT_CHROME_RESERVE: usize = 260;

/// Per-item rendering overhead: the `- Type Name  file:line` bullet, the
/// `id:` line beneath it, and the indentation around the `why` and call-site
/// lines.
const CONTEXT_ITEM_CHROME: usize = 26;

#[derive(Debug, Clone, Serialize)]
pub struct ContextItem {
    /// One of [`CONTEXT_ROLES`] — why this item is in the pack.
    ///
    /// The label is the feature. A bundle of related symbols with no stated
    /// reason for each is a pile an agent has to re-derive; labelled, it can
    /// drop the half it does not need without a second round trip.
    pub role: &'static str,
    /// The specific relationship, e.g. `calls the target` or `tested at 2 hops`.
    pub why: String,
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// Present for `target` (its body) and `doc` (its prose). Callers,
    /// dependencies and tests travel as signatures plus evidence — their
    /// bodies are a `get_code` away and would blow the budget here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSite>,
    /// Characters cut from this item to stay inside the budget.
    #[serde(skip_serializing_if = "is_zero")]
    pub truncated_chars: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Items left out, per role — because the budget ran out or the per-role cap
/// was hit. Not distinguished, because the caller's move is the same either
/// way: raise `max_chars`, or narrow `include` and ask again.
#[derive(Debug, Clone, Serialize)]
pub struct ContextDropped {
    pub role: &'static str,
    pub count: usize,
}

/// What one item will cost the budget once rendered.
///
/// Counted from the [`SymbolRef`] that actually gets emitted rather than from
/// the node, because the preview fields ride along with it — a doc preview is
/// up to [`DOC_PREVIEW_CHARS`] per item, and 15 dependencies' worth of it is a
/// quarter of the default budget. Costing the node's bare name instead is how
/// a "12000 char" pack quietly returns 18000.
fn context_item_cost(symbol: &SymbolRef, why: &str, extra: usize) -> usize {
    CONTEXT_ITEM_CHROME
        + why.len()
        + symbol.id.len()
        + symbol.name.len()
        + symbol.node_type.len()
        + symbol.file.as_deref().map_or(0, str::len)
        + symbol.doc.as_deref().map_or(0, str::len)
        + symbol.boundary.as_deref().map_or(0, str::len)
        + extra
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextResult {
    /// The reference as the caller wrote it.
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub items: Vec<ContextItem>,
    pub max_chars: usize,
    /// Characters spent, chrome included — the number to shrink `max_chars`
    /// against.
    ///
    /// Close but not exact: assembly and rendering are separate steps (the
    /// JSON envelope has no chrome at all), so the renderer's fixed overhead
    /// is charged as an estimate rather than measured. At the default budget
    /// the pack lands under `max_chars`; at very small ones it can run over
    /// by a couple of hundred characters. Treat it as a budget, not a
    /// guarantee.
    pub used_chars: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<ContextDropped>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ContextResult {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    fn failed(query: &str, max_chars: usize, error: String) -> Self {
        ContextResult {
            query: query.to_string(),
            target: None,
            items: Vec::new(),
            max_chars,
            used_chars: 0,
            dropped: Vec::new(),
            notes: Vec::new(),
            error: Some(error),
        }
    }

    fn count(&self, role: &str) -> usize {
        self.items.iter().filter(|i| i.role == role).count()
    }
}

/// Running character budget for one pack.
struct ContextBudget {
    max: usize,
    used: usize,
}

impl ContextBudget {
    fn left(&self) -> usize {
        self.max.saturating_sub(self.used)
    }

    /// Spend up to `want`, returning what was actually granted.
    fn take(&mut self, want: usize) -> usize {
        let granted = want.min(self.left());
        self.used += granted;
        granted
    }
}

/// Everything an agent needs to change one symbol safely, in one call.
///
/// Replaces the 4–6 round trips an agent otherwise spends assembling the same
/// picture — `get_code` → `find_usages` → `traverse` → `test_for` → read the
/// doc — with a single token-budgeted bundle whose every entry says why it is
/// there. No new analysis: this is assembly and budgeting over the existing
/// graph tools, which is why it needs neither an embedder nor the store.
///
/// Composition, in budget priority order:
///
/// - **target** — the symbol's own source, capped at [`CONTEXT_TARGET_SHARE`]
///   of the budget, read through [`get_code`] so it inherits the live-file
///   preference and the stale-span flagging.
/// - **caller** — direct (1-hop) inbound users with their call sites, from
///   [`find_usages`]. Signatures and evidence, not bodies.
/// - **test** — inbound users within 2 hops that [`is_test_node`] recognises.
///   Classified before callers, so a test that calls the target is listed once,
///   as a test: "who breaks" and "what re-verifies" are different questions and
///   the same node answering both would read as two callers.
/// - **dependency** — 1-hop outbound neighbours, signatures only, minus
///   `Contains` (the file that holds the symbol is not something it depends on).
/// - **doc** — `Concept` nodes adjacent in either direction, i.e. prose the
///   indexer linked to this symbol, with a preview.
///
/// [`is_test_node`]: crate::storage::facts::is_test_node
pub fn context(graph: &GraphData, src: SourceCtx, p: &ContextParams) -> ContextResult {
    let max_chars = p.max_chars.unwrap_or(CONTEXT_DEFAULT_MAX_CHARS);
    let query = p.node_id.first().cloned().unwrap_or_default();

    if query.trim().is_empty() {
        return ContextResult::failed(
            &query,
            max_chars,
            "context needs a symbol — a node id, an exact name, or a wildcard that matches exactly one symbol.".to_string(),
        );
    }

    let wants = |role: &str| {
        p.include.is_empty() || p.include.iter().any(|r| r.eq_ignore_ascii_case(role))
    };

    // One symbol, one budget. A pack is a claim about *this* symbol's
    // neighbourhood; batching several would silently divide the budget between
    // them and produce a thin, misleading answer for each.
    let target_id = match resolve_single_ref(graph, &query) {
        Ok(id) => id,
        Err(e) => return ContextResult::failed(&query, max_chars, e),
    };
    let by_id = by_id_map(graph);
    let Some(target_node) = by_id.get(target_id.as_str()).copied() else {
        return ContextResult::failed(
            &query,
            max_chars,
            unresolved_ref_error(graph, &query, MAX_REF_EXPANSION),
        );
    };

    let mut out = ContextResult {
        query: query.clone(),
        target: Some(SymbolRef::from_node(target_node)),
        items: Vec::new(),
        max_chars,
        used_chars: 0,
        dropped: Vec::new(),
        notes: Vec::new(),
        error: None,
    };
    if p.node_id.len() > 1 {
        out.notes.push(format!(
            "context takes one symbol; using {} and ignoring the other {}.",
            query,
            p.node_id.len() - 1
        ));
    }
    // A misspelled role filters everything out, and an empty pack with no
    // explanation reads as "this symbol has no callers" — the confidently
    // wrong answer this whole tool exists to avoid. `--include callers` is the
    // obvious way to get this wrong, since every section heading is plural.
    let unknown: Vec<&str> = p
        .include
        .iter()
        .map(String::as_str)
        .filter(|r| !CONTEXT_ROLES.iter().any(|k| k.eq_ignore_ascii_case(r)))
        .collect();
    if !unknown.is_empty() {
        out.notes.push(format!(
            "unknown role(s) in include: {} — valid roles are {}. Nothing was kept for them.",
            unknown.join(", "),
            CONTEXT_ROLES.join(", ")
        ));
    }

    // Seeded with the chrome rather than deducted from `max`, so `used_chars`
    // reports what the caller will actually receive and can be compared
    // directly against the `max_chars` they asked for.
    let mut budget = ContextBudget {
        max: max_chars,
        used: CONTEXT_CHROME_RESERVE.min(max_chars),
    };
    let mut dropped: Vec<(&'static str, usize)> = Vec::new();

    // ── target ─────────────────────────────────────────────────────────
    if wants("target") {
        let share = ((max_chars as f64) * CONTEXT_TARGET_SHARE) as usize;
        let allowance = share.min(budget.left());
        let got = get_code(
            graph,
            src,
            &GetCodeParams {
                node_id: vec![target_id.clone()],
                file: None,
                start_line: None,
                end_line: None,
                range: None,
                max_chars: Some(allowance),
                no_doc: false,
            },
        );
        for slice in got.slices {
            if let Some(err) = slice.error {
                out.notes.push(format!("target source unavailable: {}", err));
                continue;
            }
            // `get_code` flags a span it could not trust; that warning is
            // about the whole pack, since every line number below was read
            // from the same index.
            if let Some(stale) = slice.stale {
                out.notes.push(stale);
            }
            let code = slice.code.unwrap_or_default();
            let why = "the symbol you asked about".to_string();
            let mut symbol = SymbolRef::from_node(target_node);
            // `get_code` returns the doc comment separately from the body, and
            // it is the single most useful piece of prose about the symbol.
            // Dropping it would make `context` strictly worse than the
            // `get_code` it is meant to replace — prefer the full comment
            // `get_code` recovered over the node's truncated preview.
            if let Some(doc) = slice.doc.filter(|d| !d.trim().is_empty()) {
                symbol.doc = Some(doc);
            }
            budget.take(context_item_cost(&symbol, &why, code.len()));
            out.items.push(ContextItem {
                role: "target",
                why,
                symbol,
                code: Some(code),
                call_sites: Vec::new(),
                truncated_chars: slice.truncated_chars,
            });
        }
    }

    // ── callers and tests: one inbound walk, split by role ──────────────
    if wants("caller") || wants("test") {
        let usages = find_usages(
            graph,
            src,
            &FindUsagesParams {
                node_id: vec![target_id.clone()],
                // 2, because `test_for` walks 1..2: a test usually reaches the
                // symbol directly or through one helper.
                hops: Some(2),
                edge_types: Vec::new(),
            },
        );
        let (mut callers, mut tests) = (0usize, 0usize);
        for entry in &usages.nodes {
            for user in &entry.users {
                let Some(node) = by_id.get(user.symbol.id.as_str()).copied() else {
                    continue;
                };
                // A `Concept` pointing at the symbol is prose about it, not
                // code that breaks when it changes. It is a real inbound edge,
                // so `find_usages` returns it — but here it belongs to the
                // `doc` role, and counting it as a caller would both overstate
                // the blast radius and list the same node twice.
                if matches!(node.node_type, GraphNodeType::Concept) {
                    continue;
                }
                let is_test = crate::storage::facts::is_test_node(node);
                // Depth-2 non-tests are dropped on purpose: they do not
                // mention the target by name, so they carry no evidence and
                // would pad the pack with plausible-looking noise.
                if !is_test && user.depth > 1 {
                    continue;
                }
                let (role, cap, seen) = if is_test {
                    ("test", CONTEXT_TESTS_CAP, &mut tests)
                } else {
                    ("caller", CONTEXT_CALLERS_CAP, &mut callers)
                };
                if !wants(role) {
                    continue;
                }
                if *seen >= cap {
                    dropped.push((role, 1));
                    continue;
                }
                // Arrow notation, matching `render_find_usages` — the reader
                // should not have to learn a second spelling of the same edge.
                let why = match (is_test, user.depth) {
                    (true, 1) => format!("test —{}→ target", user.via_edge),
                    (true, d) => format!("test, reaches target in {} hops", d),
                    (false, _) => format!("this —{}→ target", user.via_edge),
                };
                let cost = context_item_cost(
                    &user.symbol,
                    &why,
                    user.call_sites.iter().map(|c| c.text.len() + 8).sum(),
                );
                if budget.left() < cost {
                    dropped.push((role, 1));
                    continue;
                }
                budget.take(cost);
                *seen += 1;
                out.items.push(ContextItem {
                    role,
                    why,
                    symbol: user.symbol.clone(),
                    code: None,
                    call_sites: user.call_sites.clone(),
                    truncated_chars: 0,
                });
            }
        }
    }

    // ── dependencies and docs: one outbound/incident pass ───────────────
    if wants("dependency") || wants("doc") {
        let mut deps: Vec<(&GraphNode, &'static str)> = Vec::new();
        let mut docs: Vec<(&GraphNode, &'static str)> = Vec::new();
        let mut seen_ids: Vec<&str> = vec![target_id.as_str()];
        for e in &graph.edges {
            let et = edge_type_str(&e.edge_type);
            let other = if e.source == target_id {
                e.target.as_str()
            } else if e.target == target_id {
                // Inbound edges are the callers' business, except that a
                // Concept pointing *at* this symbol is documentation of it —
                // which is the direction doc links actually run.
                e.source.as_str()
            } else {
                continue;
            };
            let Some(node) = by_id.get(other).copied() else {
                continue;
            };
            if seen_ids.contains(&node.id.as_str()) {
                continue;
            }
            if matches!(node.node_type, GraphNodeType::Concept) {
                seen_ids.push(node.id.as_str());
                docs.push((node, et));
            } else if e.source == target_id
                // `Contains` is structure, not dependence: the file that holds
                // a function is not something the function relies on.
                && !matches!(e.edge_type, GraphEdgeType::Contains)
                && !matches!(node.node_type, GraphNodeType::Folder | GraphNodeType::File)
            {
                seen_ids.push(node.id.as_str());
                deps.push((node, et));
            }
        }

        if wants("dependency") {
            for (i, (node, et)) in deps.iter().enumerate() {
                let why = format!("target —{}→ this", et);
                let symbol = SymbolRef::from_node(node);
                let cost = context_item_cost(&symbol, &why, 0);
                if i >= CONTEXT_DEPS_CAP || budget.left() < cost {
                    dropped.push(("dependency", deps.len() - i));
                    break;
                }
                budget.take(cost);
                out.items.push(ContextItem {
                    role: "dependency",
                    why,
                    symbol,
                    code: None,
                    call_sites: Vec::new(),
                    truncated_chars: 0,
                });
            }
        }

        if wants("doc") {
            for (i, (node, _)) in docs.iter().enumerate() {
                let prose: String = node
                    .docstring
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(CONTEXT_DOC_PREVIEW_CHARS)
                    .collect();
                let why = "prose the indexer linked to this symbol".to_string();
                let symbol = SymbolRef::from_node(node);
                let cost = context_item_cost(&symbol, &why, prose.len());
                if i >= CONTEXT_DOCS_CAP || budget.left() < cost {
                    dropped.push(("doc", docs.len() - i));
                    break;
                }
                budget.take(cost);
                out.items.push(ContextItem {
                    role: "doc",
                    why,
                    symbol,
                    code: (!prose.is_empty()).then_some(prose),
                    call_sites: Vec::new(),
                    truncated_chars: 0,
                });
            }
        }
    }

    // Collapse the per-item drops into one count per role, in role order, so
    // the caller sees "6 dependency" rather than six identical lines.
    for role in CONTEXT_ROLES {
        let count: usize = dropped.iter().filter(|(r, _)| r == role).map(|(_, c)| c).sum();
        if count > 0 {
            out.dropped.push(ContextDropped { role, count });
        }
    }
    out.used_chars = budget.used;
    out
}

/// Ids whose captured source a `context` call reads: the target, plus the
/// direct users whose call sites get scanned.
pub fn context_source_ids(graph: &GraphData, p: &ContextParams) -> Vec<String> {
    let Some(query) = p.node_id.first() else {
        return Vec::new();
    };
    let Ok(target_id) = resolve_single_ref(graph, query) else {
        return Vec::new();
    };
    let mut ids = get_code_source_ids(
        graph,
        &GetCodeParams {
            node_id: vec![target_id.clone()],
            file: None,
            start_line: None,
            end_line: None,
            range: None,
            max_chars: None,
            no_doc: false,
        },
    );
    ids.extend(find_usages_source_ids(
        graph,
        &FindUsagesParams {
            node_id: vec![target_id],
            hops: Some(2),
            edge_types: Vec::new(),
        },
    ));
    ids.sort();
    ids.dedup();
    ids
}

/// Plural section heading for a role. Spelled out because appending `s`
/// yields "dependencys".
fn context_role_plural(role: &str) -> &str {
    match role {
        "caller" => "callers",
        "test" => "tests",
        "dependency" => "dependencies",
        "doc" => "docs",
        other => other,
    }
}

pub fn render_context(r: &ContextResult, style: Render) -> String {
    let mut out = String::new();
    if let Some(err) = &r.error {
        line(&mut out, err);
        return out;
    }

    if let Some(t) = &r.target {
        line(
            &mut out,
            &format!(
                "{} {} {}  {}",
                style.heading("Context for"),
                t.node_type,
                style.bold(&t.name),
                t.loc()
            ),
        );
        line(&mut out, &format!("id: {}", style.id(&t.id)));
    }

    let counts: Vec<String> = CONTEXT_ROLES
        .iter()
        .filter(|role| **role != "target")
        .filter_map(|role| {
            let n = r.count(role);
            (n > 0).then(|| format!("{} {}", n, role))
        })
        .collect();
    line(
        &mut out,
        &style.dim(&format!(
            "{} chars of {} budget{}{}",
            r.used_chars,
            r.max_chars,
            if counts.is_empty() {
                String::new()
            } else {
                format!(" · {}", counts.join(", "))
            },
            if r.dropped.is_empty() {
                String::new()
            } else {
                let d: Vec<String> = r
                    .dropped
                    .iter()
                    .map(|d| format!("{} {}", d.count, d.role))
                    .collect();
                format!(" · not shown: {}", d.join(", "))
            }
        )),
    );

    for note in &r.notes {
        line(&mut out, &format!("⚠ {}", note));
    }

    for role in CONTEXT_ROLES {
        let items: Vec<&ContextItem> = r.items.iter().filter(|i| i.role == *role).collect();
        if items.is_empty() {
            continue;
        }
        out.push('\n');
        line(
            &mut out,
            &style.bold(&match *role {
                "target" => "── target ──".to_string(),
                other => format!("── {} ({}) ──", context_role_plural(other), items.len()),
            }),
        );
        for item in items {
            if *role == "target" {
                if let Some(doc) = &item.symbol.doc {
                    line(&mut out, &style.dim(doc));
                }
                if let Some(code) = &item.code {
                    out.push_str(code);
                    if !code.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if item.truncated_chars > 0 {
                    line(
                        &mut out,
                        &style.dim(&format!(
                            "… {} more chars — get_code {} for the whole body",
                            item.truncated_chars, item.symbol.id
                        )),
                    );
                }
                continue;
            }
            item.symbol.render_bullet(&mut out, style);
            line(&mut out, &format!("  {}", style.dim(&item.why)));
            for cs in &item.call_sites {
                line(&mut out, &format!("    {}  {}", cs.line, cs.text));
            }
            if let Some(prose) = &item.code {
                line(&mut out, &format!("    {}", prose.replace('\n', "\n    ")));
            }
        }
    }

    out.push('\n');
    line(
        &mut out,
        &style.dim(
            "Next: get_code <id> for any item's full body · find_usages <id> --hops 2 for transitive users",
        ),
    );
    out
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GraphEdge, GraphNodeType};

    fn node(
        id: &str,
        name: &str,
        t: GraphNodeType,
        file: &str,
        lines: Option<(u32, u32)>,
    ) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: t,
            file: Some(file.to_string()),
            start_line: lines.map(|(s, _)| s),
            end_line: lines.map(|(_, e)| e),
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            ..Default::default()
        }
    }

    fn edge(source: &str, target: &str, edge_type: GraphEdgeType) -> GraphEdge {
        GraphEdge {
            source: source.to_string(),
            target: target.to_string(),
            edge_type,
        }
    }

    /// Two functions in one file, `caller` calling `callee`, plus the File
    /// node that contains them. The File node carries no line range, like a
    /// real one.
    fn fixture() -> GraphData {
        GraphData {
            nodes: vec![
                node("file:src/a.rs", "a.rs", GraphNodeType::File, "src/a.rs", None),
                node(
                    "function:src/a.rs:1:caller",
                    "caller",
                    GraphNodeType::Function,
                    "src/a.rs",
                    Some((1, 5)),
                ),
                node(
                    "function:src/a.rs:7:callee",
                    "callee",
                    GraphNodeType::Function,
                    "src/a.rs",
                    Some((7, 9)),
                ),
            ],
            edges: vec![
                edge(
                    "function:src/a.rs:1:caller",
                    "function:src/a.rs:7:callee",
                    GraphEdgeType::Calls,
                ),
                edge(
                    "file:src/a.rs",
                    "function:src/a.rs:1:caller",
                    GraphEdgeType::Contains,
                ),
            ],
            stats: None,
            resolution: None,
        }
    }

    /// A repo root that is guaranteed not to exist, so a test that passes
    /// can only have read from the index.
    const NO_REPO: &str = "/nonexistent-repo-root";

    /// One symbol with a neighbour of every role `context` reports:
    /// a plain caller, a caller that lives in a test file, an outbound
    /// dependency, an outbound `Contains` child (which is *not* a dependency),
    /// and a `Concept` node pointing at it (documentation).
    fn context_fixture() -> GraphData {
        let mut concept = node(
            "concept:docs/svc.md:Processing",
            "Processing",
            GraphNodeType::Concept,
            "docs/svc.md",
            Some((3, 20)),
        );
        concept.docstring = Some("How the processing pipeline fits together.".to_string());
        GraphData {
            nodes: vec![
                node("file:src/svc.rs", "svc.rs", GraphNodeType::File, "src/svc.rs", None),
                node(
                    "function:src/svc.rs:10:process",
                    "process",
                    GraphNodeType::Function,
                    "src/svc.rs",
                    Some((10, 20)),
                ),
                node(
                    "function:src/svc.rs:1:handler",
                    "handler",
                    GraphNodeType::Function,
                    "src/svc.rs",
                    Some((1, 5)),
                ),
                node(
                    "function:tests/svc_test.rs:1:process_roundtrips",
                    "process_roundtrips",
                    GraphNodeType::Function,
                    "tests/svc_test.rs",
                    Some((1, 8)),
                ),
                node(
                    "function:src/util.rs:1:normalize",
                    "normalize",
                    GraphNodeType::Function,
                    "src/util.rs",
                    Some((1, 4)),
                ),
                node(
                    "variable:src/svc.rs:12:tmp",
                    "tmp",
                    GraphNodeType::Variable,
                    "src/svc.rs",
                    Some((12, 12)),
                ),
                concept,
            ],
            edges: vec![
                edge(
                    "function:src/svc.rs:1:handler",
                    "function:src/svc.rs:10:process",
                    GraphEdgeType::Calls,
                ),
                edge(
                    "function:tests/svc_test.rs:1:process_roundtrips",
                    "function:src/svc.rs:10:process",
                    GraphEdgeType::Calls,
                ),
                edge(
                    "function:src/svc.rs:10:process",
                    "function:src/util.rs:1:normalize",
                    GraphEdgeType::Calls,
                ),
                // Structure, not dependence — must not show up as a dependency.
                edge(
                    "function:src/svc.rs:10:process",
                    "variable:src/svc.rs:12:tmp",
                    GraphEdgeType::Contains,
                ),
                edge(
                    "concept:docs/svc.md:Processing",
                    "function:src/svc.rs:10:process",
                    GraphEdgeType::References,
                ),
            ],
            stats: None,
            resolution: None,
        }
    }

    fn context_of(graph: &GraphData, src: &IndexedSource, p: ContextParams) -> ContextResult {
        context(graph, SourceCtx::new(src, Path::new(NO_REPO)), &p)
    }

    fn roles(r: &ContextResult, role: &str) -> Vec<String> {
        r.items
            .iter()
            .filter(|i| i.role == role)
            .map(|i| i.symbol.name.clone())
            .collect()
    }

    /// The pack's whole value proposition: one call returns every kind of
    /// neighbour, and each one says why it is there.
    ///
    /// The load-bearing assertion is the test/caller split. `process_roundtrips`
    /// calls the target exactly like `handler` does, so a naive walk lists it
    /// twice or calls it a caller; "who breaks" and "what re-verifies" are
    /// different questions and it belongs to the second.
    #[test]
    fn context_labels_every_neighbour_with_its_role() {
        let graph = context_fixture();
        let src = indexed(&[("function:src/svc.rs:10:process", "fn process() { normalize(); }")]);
        let r = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["process".to_string()],
                ..Default::default()
            },
        );

        assert!(r.ok(), "{:?}", r.error);
        assert_eq!(r.target.as_ref().expect("target").name, "process");
        assert_eq!(roles(&r, "target"), vec!["process"]);
        assert_eq!(roles(&r, "caller"), vec!["handler"]);
        assert_eq!(roles(&r, "test"), vec!["process_roundtrips"]);
        assert_eq!(roles(&r, "doc"), vec!["Processing"]);

        // `Contains` is structure: the variable the function holds is not
        // something it depends on.
        assert_eq!(roles(&r, "dependency"), vec!["normalize"]);

        // The target travels with its body; the rest travel as signatures.
        let target = r.items.iter().find(|i| i.role == "target").expect("target item");
        assert!(target.code.as_deref().unwrap_or_default().contains("fn process"));
        for item in r.items.iter().filter(|i| i.role == "caller" || i.role == "dependency") {
            assert!(item.code.is_none(), "{} carried a body", item.symbol.name);
        }

        // Every role label is one the schema advertises.
        for item in &r.items {
            assert!(CONTEXT_ROLES.contains(&item.role), "{}", item.role);
            assert!(!item.why.is_empty(), "{} has no why", item.symbol.name);
        }

        // The rendered form is what an agent actually reads, so assert on it
        // rather than only on the struct. Section headings are pluralised by
        // hand — appending `s` produced "dependencys" — and every neighbour
        // must arrive with an id the caller can paste into a follow-up call.
        let text = render_context(&r, Render::Markdown);
        for heading in ["── target ──", "── callers (1) ──", "── tests (1) ──",
                        "── dependencies (1) ──", "── docs (1) ──"] {
            assert!(text.contains(heading), "missing {heading} in:\n{text}");
        }
        assert!(text.contains("fn process"), "target body missing");
        assert!(text.contains("processing pipeline"), "doc prose missing");
        assert!(
            text.contains("function:src/util.rs:1:normalize"),
            "dependency id missing — the caller cannot follow up without it"
        );
    }

    /// A budget that binds drops from the bottom of the priority order and
    /// says what it dropped — the caller must never be left guessing whether
    /// "no tests" means "none exist" or "none fit".
    #[test]
    fn context_fills_by_priority_and_reports_what_did_not_fit() {
        let graph = context_fixture();
        let src = indexed(&[("function:src/svc.rs:10:process", "fn process() {}")]);
        // Chosen to bind partway down the priority order: enough for the
        // target and its caller, not enough to reach the docs at the bottom.
        const TIGHT: usize = 700;
        let tight = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["process".to_string()],
                max_chars: Some(TIGHT),
                ..Default::default()
            },
        );

        assert!(tight.ok());
        assert!(tight.used_chars <= TIGHT, "spent {}", tight.used_chars);
        // Docs are the first to go; the caller is not — that is the priority
        // order doing its job, and it is the whole reason the order exists.
        assert_eq!(roles(&tight, "caller"), vec!["handler"]);
        assert!(
            roles(&tight, "doc").is_empty(),
            "docs should not fit in {TIGHT}"
        );
        assert!(
            tight.dropped.iter().any(|d| d.role == "doc"),
            "a dropped doc must be reported: {:?}",
            tight.dropped
        );

        // The same call with room reports nothing dropped.
        let roomy = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["process".to_string()],
                ..Default::default()
            },
        );
        assert!(roomy.dropped.is_empty(), "{:?}", roomy.dropped);
        assert!(roomy.used_chars > tight.used_chars);
    }

    /// `include` is how an agent buys only the half it needs.
    #[test]
    fn context_include_keeps_only_the_named_roles() {
        let graph = context_fixture();
        let src = indexed(&[("function:src/svc.rs:10:process", "fn process() {}")]);
        let r = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["process".to_string()],
                include: vec!["caller".to_string(), "test".to_string()],
                ..Default::default()
            },
        );

        assert_eq!(roles(&r, "caller"), vec!["handler"]);
        assert_eq!(roles(&r, "test"), vec!["process_roundtrips"]);
        for absent in ["target", "dependency", "doc"] {
            assert!(roles(&r, absent).is_empty(), "{} should be filtered out", absent);
        }
        assert!(r.notes.is_empty(), "valid roles must not warn: {:?}", r.notes);

        // A misspelled role must say so. Every section heading is plural, so
        // `--include callers` is the natural mistake, and an unexplained empty
        // pack reads as "this symbol has no callers".
        let typo = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["process".to_string()],
                include: vec!["callers".to_string()],
                ..Default::default()
            },
        );
        assert!(typo.items.is_empty());
        assert!(
            typo.notes.iter().any(|n| n.contains("unknown role")),
            "{:?}",
            typo.notes
        );
    }

    /// A pack is a claim about one symbol's neighbourhood, so an ambiguous
    /// name has to fail loudly with the candidates rather than silently
    /// picking one and describing the wrong code.
    #[test]
    fn context_refuses_an_ambiguous_or_unknown_symbol() {
        let mut graph = context_fixture();
        graph.nodes.push(node(
            "function:src/other.rs:1:process",
            "process",
            GraphNodeType::Function,
            "src/other.rs",
            Some((1, 3)),
        ));
        let src = indexed(&[]);

        let ambiguous = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["process".to_string()],
                ..Default::default()
            },
        );
        assert!(!ambiguous.ok());
        let err = ambiguous.error.expect("error");
        assert!(err.contains("src/other.rs"), "{err}");
        assert!(ambiguous.items.is_empty());

        let unknown = context_of(
            &graph,
            &src,
            ContextParams {
                node_id: vec!["no_such_symbol".to_string()],
                ..Default::default()
            },
        );
        assert!(!unknown.ok());

        // An empty request is a usage error, not an empty pack.
        let blank = context_of(&graph, &src, ContextParams::default());
        assert!(!blank.ok());
    }

    fn indexed(entries: &[(&str, &str)]) -> IndexedSource {
        let mut out = IndexedSource::default();
        for (id, code) in entries {
            out.insert(
                *id,
                StoredSource {
                    code: (*code).to_string(),
                    file_hash: "deadbeef".into(),
                },
            );
        }
        out
    }

    /// The whole point of the capture: call-site evidence with no working
    /// tree anywhere. The line numbers must be the caller's real file lines,
    /// not offsets into its captured span.
    #[test]
    fn find_usages_reads_call_sites_from_the_index_with_no_repo() {
        let g = fixture();
        // `caller` spans lines 1-5; its capture is exactly those lines.
        let src = indexed(&[(
            "function:src/a.rs:1:caller",
            "fn caller() {\n    let x = 1;\n    callee(x)\n}\n\n",
        )]);
        let r = find_usages(
            &g,
            SourceCtx::new(&src, Path::new(NO_REPO)),
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        let sites = &r.nodes[0].users[0].call_sites;
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].line, 3, "third line of a span starting at line 1");
        assert_eq!(sites[0].text, "callee(x)");
    }

    /// A caller whose own span was never captured still gets call sites from
    /// the file's whole-file node — here the line offset comes from the
    /// caller's declared range, not from the start of the file.
    #[test]
    fn find_usages_falls_back_to_the_whole_file_capture() {
        let mut g = fixture();
        // Move `caller` to lines 3-5 so a file-relative scan is visible.
        g.nodes[1].start_line = Some(3);
        g.nodes[1].end_line = Some(5);
        let src = indexed(&[(
            "file:src/a.rs",
            "// header\n\nfn caller() {\n    callee()\n}\n\nfn callee() {}\n",
        )]);
        let r = find_usages(
            &g,
            SourceCtx::new(&src, Path::new(NO_REPO)),
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        let sites = &r.nodes[0].users[0].call_sites;
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].line, 4);
        assert_eq!(sites[0].text, "callee()");
    }

    /// Without an index and without a repo there is simply no evidence to
    /// show — but the structural answer still has to come back.
    #[test]
    fn find_usages_without_source_still_reports_users() {
        let g = fixture();
        let r = find_usages(
            &g,
            SourceCtx::repo_only(Path::new(NO_REPO)),
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert_eq!(r.nodes[0].users.len(), 1);
        assert!(r.nodes[0].users[0].call_sites.is_empty());
    }

    /// The pre-fetch and the scan have to agree on which nodes matter,
    /// otherwise a transport quietly fetches the wrong rows and call sites
    /// vanish with no error.
    #[test]
    fn find_usages_source_ids_cover_what_the_scan_reads() {
        let g = fixture();
        let ids = find_usages_source_ids(
            &g,
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert!(ids.contains(&"function:src/a.rs:1:caller".to_string()));
        assert!(
            ids.contains(&"file:src/a.rs".to_string()),
            "the file's whole-file node backs up an uncaptured caller"
        );
    }

    #[test]
    fn get_code_serves_a_node_from_the_index_with_no_repo() {
        let g = fixture();
        let src = indexed(&[("function:src/a.rs:7:callee", "fn callee() {\n    42\n}\n")]);
        let r = get_code(
            &g,
            SourceCtx::new(&src, Path::new(NO_REPO)),
            &GetCodeParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert!(r.ok());
        assert_eq!(r.slices[0].code.as_deref(), Some("fn callee() {\n    42\n}\n"));
        assert!(
            r.slices[0].stale.is_none(),
            "a missing repo is not a staleness signal"
        );
    }

    /// The file/line-range form, which has no node of its own to read: it
    /// slices the file's whole-file capture instead of the working tree.
    #[test]
    fn get_code_slices_a_range_out_of_the_whole_file_capture() {
        let g = fixture();
        let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\nfour\nfive\n")]);
        let r = get_code(
            &g,
            SourceCtx::new(&src, Path::new(NO_REPO)),
            &GetCodeParams {
                file: Some("src/a.rs".into()),
                start_line: Some(2),
                end_line: Some(4),
                ..Default::default()
            },
        );
        assert!(r.ok(), "{:?}", r.slices[0].error);
        assert_eq!(r.slices[0].code.as_deref(), Some("two\nthree\nfour"));
        assert_eq!(r.slices[0].total_lines, Some(6));
    }

    /// With nothing captured and no repo, `get_code` must say so rather than
    /// return an empty slice that reads as "this symbol has no code".
    #[test]
    fn get_code_reports_when_neither_index_nor_repo_has_the_file() {
        let g = fixture();
        let r = get_code(
            &g,
            SourceCtx::repo_only(Path::new(NO_REPO)),
            &GetCodeParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok());
        let err = r.slices[0].error.as_ref().unwrap();
        assert!(err.contains("not captured in the index"), "{}", err);
        assert!(err.contains(NO_REPO), "{}", err);
    }

    /// The whole point of P1: when the repo is on disk, `get_code` serves the
    /// *current* file content, not the stale capture. An editing agent that
    /// just changed the file must read back what it wrote.
    #[test]
    fn get_code_prefers_the_live_working_tree_over_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        // A real file on disk: the symbol body says `99` at lines 1-3.
        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(repo_root.join("src/a.rs"), "fn callee() {\n    99\n}\n").unwrap();

        // The node spans the same lines 1-3, so the span is in range for both
        // the live file and the stale capture below.
        let g = GraphData {
            nodes: vec![node(
                "function:src/a.rs:1:callee",
                "callee",
                GraphNodeType::Function,
                "src/a.rs",
                Some((1, 3)),
            )],
            edges: vec![],
            stats: None,
            resolution: None,
        };

        // Index holds a *different* (stale) capture of the same span — `42`.
        let src = indexed(&[(
            "function:src/a.rs:1:callee",
            "fn callee() {\n    42\n}\n",
        )]);

        let r = get_code(
            &g,
            SourceCtx::new(&src, repo_root),
            &GetCodeParams {
                node_id: vec!["function:src/a.rs:1:callee".into()],
                ..Default::default()
            },
        );
        assert!(r.ok(), "{:?}", r.slices[0].error);
        // 99 is the live edit; 42 is the stale capture. Live wins.
        assert!(
            r.slices[0].code.as_deref().unwrap().contains("99"),
            "live content not served: {:?}",
            r.slices[0].code
        );
        assert!(
            r.slices[0].stale.is_some(),
            "a live read that disagrees with the index must be flagged"
        );
    }

    /// When the live file still matches what was indexed, no staleness flag —
    /// the span and the source agree, so there is nothing to warn about.
    #[test]
    fn get_code_live_read_with_matching_hash_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let body = "fn callee() {\n    42\n}\n";
        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(repo_root.join("src/a.rs"), body).unwrap();
        let hash = blake3::hash(body.as_bytes()).to_hex().to_string();

        let mut src = IndexedSource::default();
        src.insert(
            "function:src/a.rs:7:callee",
            StoredSource { code: body.to_string(), file_hash: hash },
        );

        let r = get_code(
            &fixture(),
            SourceCtx::new(&src, repo_root),
            &GetCodeParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert!(r.ok());
        assert!(r.slices[0].stale.is_none(), "matching hash must not flag");
    }

    /// The file/line-range form reads the working tree too: an agent paging a
    /// file with `--range` wants current lines, and a changed file is flagged.
    #[test]
    fn get_code_file_range_reads_live_and_flags_drift() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(repo_root.join("src/a.rs"), "a\nb\nc\nd\ne\n").unwrap();

        // Index holds a stale, different capture so the drift is detectable.
        let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\n")]);

        let r = get_code(
            &fixture(),
            SourceCtx::new(&src, repo_root),
            &GetCodeParams {
                file: Some("src/a.rs".into()),
                start_line: Some(2),
                end_line: Some(4),
                ..Default::default()
            },
        );
        assert!(r.ok(), "{:?}", r.slices[0].error);
        // Live lines b/c/d, not the stale two/three/four.
        assert_eq!(r.slices[0].code.as_deref(), Some("b\nc\nd"));
        assert!(r.slices[0].stale.is_some(), "drift between live and index must flag");
    }

    #[test]
    fn get_code_source_ids_cover_nodes_and_their_files() {
        let g = fixture();
        let ids = get_code_source_ids(
            &g,
            &GetCodeParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert_eq!(
            ids,
            vec!["file:src/a.rs", "function:src/a.rs:7:callee"],
            "the node's span plus its file's capture as backup"
        );

        // The file form has no node id of its own to ask for.
        let ids = get_code_source_ids(
            &g,
            &GetCodeParams {
                file: Some("file:src/a.rs".into()),
                ..Default::default()
            },
        );
        assert_eq!(ids, vec!["file:src/a.rs"], "the file id prefix is stripped");
    }

    #[test]
    fn find_symbol_ranks_exact_then_prefix_then_substring() {
        // `call` (exact) > `caller` (prefix) > `do_call` (substring).
        let g = GraphData {
            nodes: vec![
                node("f:1:do_call", "do_call", GraphNodeType::Function, "a.rs", Some((1, 2))),
                node("f:2:caller", "caller", GraphNodeType::Function, "a.rs", Some((3, 4))),
                node("f:3:call", "call", GraphNodeType::Function, "a.rs", Some((5, 6))),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["call".into()],
                ..Default::default()
            },
        );
        assert_eq!(r.queries[0].total, 3);
        let order: Vec<&str> = r.queries[0].items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(order, vec!["call", "caller", "do_call"]);
    }

    #[test]
    /// `include_docs` widens the search to docstrings: the matches must be
    /// additive, and must rank below every name match.
    fn find_symbol_include_docs_ranks_below_name_hits() {
        let mut g = GraphData {
            nodes: vec![
                node("f:1:cache_get", "cache_get", GraphNodeType::Function, "a.rs", Some((1, 2))),
                node("f:2:drop_stale", "drop_stale", GraphNodeType::Function, "a.rs", Some((3, 4))),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        g.nodes[1].docstring = Some("Evicts entries from the cache.".into());

        // Name-only: the docstring mention is invisible.
        let names_only = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["cache".into()],
                ..Default::default()
            },
        );
        assert_eq!(names_only.queries[0].total, 1);
        assert_eq!(names_only.queries[0].items[0].name, "cache_get");

        // With docs: both, and the name hit still comes first.
        let with_docs = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["cache".into()],
                include_docs: true,
                ..Default::default()
            },
        );
        assert_eq!(with_docs.queries[0].total, 2);
        let order: Vec<&str> = with_docs.queries[0]
            .items
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(order, vec!["cache_get", "drop_stale"]);
    }

    #[test]
    fn find_symbol_honours_type_and_file_filters() {
        let g = fixture();
        let all = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["a".into()],
                ..Default::default()
            },
        );
        assert!(all.queries[0].total >= 3);

        let functions_only = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["a".into()],
                node_types: vec!["function".into()],
                ..Default::default()
            },
        );
        assert!(functions_only.queries[0]
            .items
            .iter()
            .all(|i| i.node_type == "Function"));

        let nothing = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["a".into()],
                file_prefix: Some("other/".into()),
                ..Default::default()
            },
        );
        assert_eq!(nothing.queries[0].total, 0);
    }

    #[test]
    fn find_symbol_respects_limit_but_reports_full_total() {
        let g = fixture();
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["call".into()],
                limit: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(r.queries[0].total, 2, "total counts every match");
        assert_eq!(r.queries[0].items.len(), 1, "items honour the limit");
    }

    #[test]
    fn find_symbol_direct_id_lookup() {
        let g = fixture();
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert_eq!(r.queries[0].kind, "id");
        assert_eq!(r.queries[0].items[0].name, "callee");
        assert!(r.ok());
    }

    #[test]
    fn find_symbol_reports_missing_id() {
        let g = fixture();
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                node_id: vec!["function:nope".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok());
        assert_eq!(r.queries[0].total, 0);
    }

    // -----------------------------------------------------------------
    // get_code line windows
    // -----------------------------------------------------------------

    fn window(range: Option<&str>, start: Option<usize>, end: Option<usize>) -> (Option<usize>, Option<usize>) {
        line_window(&GetCodeParams {
            range: range.map(String::from),
            start_line: start,
            end_line: end,
            ..Default::default()
        })
        .unwrap()
    }

    /// `--range` has to mean on `get_code` what it means on `ug analyze`, or
    /// the shared spelling is a trap. Same parser, so every phrasing that
    /// works there works here.
    #[test]
    fn range_accepts_the_same_spellings_as_analyze() {
        assert_eq!(window(Some("11-35"), None, None), (Some(11), Some(35)));
        assert_eq!(window(Some("11..35"), None, None), (Some(11), Some(35)));
        assert_eq!(window(Some("rows 11 to 35"), None, None), (Some(11), Some(35)));
        // Open-ended: no end bound, which the reader turns into EOF.
        assert_eq!(window(Some("34-end"), None, None), (Some(34), None));
        assert_eq!(window(Some("34-"), None, None), (Some(34), None));
        // A bare count is "the first N" — as in analyze, not "line N".
        assert_eq!(window(Some("20"), None, None), (Some(1), Some(20)));
    }

    #[test]
    fn explicit_start_end_win_over_range() {
        assert_eq!(window(Some("11-35"), Some(5), None), (Some(5), Some(35)));
        assert_eq!(window(Some("11-35"), None, Some(99)), (Some(11), Some(99)));
        assert_eq!(window(None, Some(5), Some(9)), (Some(5), Some(9)));
    }

    /// A window the parser cannot read is reported, not rounded down to
    /// "the whole file" — that would be a wrong answer that looks right.
    #[test]
    fn an_unreadable_range_is_an_error_with_the_input_in_it() {
        let e = line_window(&GetCodeParams {
            range: Some("banana".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(e.contains("banana"), "got: {e}");
        assert!(e.contains("34-end"), "the message shows the valid forms: {e}");

        // A backwards range is nonsense too, and analyze rejects it.
        assert!(line_window(&GetCodeParams {
            range: Some("35-11".into()),
            ..Default::default()
        })
        .is_err());
    }

    /// End to end through the tool, so the error reaches the caller as a
    /// slice rather than being swallowed.
    #[test]
    fn get_code_surfaces_a_bad_range_instead_of_reading_the_whole_file() {
        let g = fixture();
        let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\nfour\n")]);
        let r = get_code(
            &g,
            SourceCtx::new(&src, Path::new(NO_REPO)),
            &GetCodeParams {
                file: Some("src/a.rs".into()),
                range: Some("nope".into()),
                ..Default::default()
            },
        );
        assert!(!r.ok());
        assert!(r.slices[0].error.as_ref().unwrap().contains("nope"));
    }

    #[test]
    fn get_code_range_reads_exactly_that_window() {
        let g = fixture();
        let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\nfour\nfive\n")]);
        let r = get_code(
            &g,
            SourceCtx::new(&src, Path::new(NO_REPO)),
            &GetCodeParams {
                file: Some("src/a.rs".into()),
                range: Some("2-3".into()),
                ..Default::default()
            },
        );
        assert!(r.ok(), "{:?}", r.slices[0].error);
        assert_eq!(r.slices[0].code.as_deref(), Some("two\nthree"));
        assert_eq!(r.slices[0].start_line, Some(2));
        assert_eq!(r.slices[0].end_line, Some(3));
    }

    // -----------------------------------------------------------------
    // Wildcards
    // -----------------------------------------------------------------

    /// A graph with names and paths chosen so glob semantics are visible:
    /// three `run_*` functions across two directories, plus a name that
    /// only a substring search would reach.
    fn glob_fixture() -> GraphData {
        GraphData {
            nodes: vec![
                node("file:src/a.rs", "a.rs", GraphNodeType::File, "src/a.rs", None),
                node("file:src/deep/b.rs", "b.rs", GraphNodeType::File, "src/deep/b.rs", None),
                node("function:src/a.rs:1:run_gen", "run_gen", GraphNodeType::Function, "src/a.rs", Some((1, 5))),
                node("function:src/a.rs:7:run_serve", "run_serve", GraphNodeType::Function, "src/a.rs", Some((7, 9))),
                node("function:src/deep/b.rs:1:run_index", "run_index", GraphNodeType::Function, "src/deep/b.rs", Some((1, 4))),
                node("function:src/deep/b.rs:6:prerun_gen", "prerun_gen", GraphNodeType::Function, "src/deep/b.rs", Some((6, 8))),
                node("class:src/a.rs:20:Runner", "Runner", GraphNodeType::Class, "src/a.rs", Some((20, 30))),
            ],
            edges: vec![edge(
                "function:src/a.rs:1:run_gen",
                "function:src/deep/b.rs:1:run_index",
                GraphEdgeType::Calls,
            )],
            stats: None,
            resolution: None,
        }
    }

    fn names_of(q: &SymbolQueryResult) -> Vec<&str> {
        q.items.iter().map(|i| i.name.as_str()).collect()
    }

    /// The headline behaviour: a pattern matches the whole name, so
    /// `run_*` must not pick up `prerun_gen` the way a substring search
    /// would.
    #[test]
    fn find_symbols_wildcard_is_anchored_to_the_whole_name() {
        let g = glob_fixture();
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["run_*".into()],
                ..Default::default()
            },
        );
        assert_eq!(r.queries[0].kind, "pattern");
        assert_eq!(names_of(&r.queries[0]), vec!["run_gen", "run_index", "run_serve"]);
    }

    /// A plain fragment keeps the ranked substring behaviour it always had —
    /// wildcards are additive, not a replacement.
    #[test]
    fn find_symbols_literal_still_matches_substrings() {
        let g = glob_fixture();
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["run_gen".into()],
                ..Default::default()
            },
        );
        assert_eq!(r.queries[0].kind, "name");
        assert_eq!(
            names_of(&r.queries[0]),
            vec!["run_gen", "prerun_gen"],
            "exact first, then the substring hit"
        );
    }

    #[test]
    fn find_symbols_wildcard_honours_type_and_path_filters() {
        let g = glob_fixture();
        // `*` + a path glob is the "list this subtree" idiom.
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["*".into()],
                node_types: vec!["Function".into()],
                file_prefix: Some("src/deep/**".into()),
                ..Default::default()
            },
        );
        assert_eq!(names_of(&r.queries[0]), vec!["prerun_gen", "run_index"]);

        // A literal file filter keeps meaning "prefix", not "equals".
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["run_*".into()],
                file_prefix: Some("src/a.rs".into()),
                ..Default::default()
            },
        );
        assert_eq!(names_of(&r.queries[0]), vec!["run_gen", "run_serve"]);

        // A node-type filter may itself be a pattern.
        let r = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["*".into()],
                node_types: vec!["Cl*".into()],
                ..Default::default()
            },
        );
        assert_eq!(names_of(&r.queries[0]), vec!["Runner"]);
    }

    /// Two refs that overlap must not yield the same id twice.
    ///
    /// `Vec::dedup` only collapses *adjacent* duplicates, and these never
    /// are: each ref's expansion is appended whole, so a repeat of an
    /// earlier id lands with other ids between them. The visible symptom was
    /// `get_code run_gen 'run_*'` printing `run_gen`'s entire body twice.
    #[test]
    fn overlapping_refs_do_not_repeat_a_node_id() {
        let g = glob_fixture();
        let refs = vec!["run_gen".to_string(), "run_serve".to_string(), "run_*".to_string()];
        let out = expand_node_refs(&g, &refs, MAX_REF_EXPANSION);

        let mut sorted = out.clone();
        sorted.sort();
        let mut unique = sorted.clone();
        unique.dedup();
        assert_eq!(sorted, unique, "expansion contains duplicate ids: {out:?}");

        // First mention wins, so the caller's own ordering survives: the two
        // explicit refs stay in front of the ids the pattern added.
        assert_eq!(out[0], "function:src/a.rs:1:run_gen");
        assert_eq!(out[1], "function:src/a.rs:7:run_serve");
        assert!(out.contains(&"function:src/deep/b.rs:1:run_index".to_string()));
    }

    /// `*` in a path must not cross `/`, or every "this directory" query
    /// silently becomes a whole-subtree query.
    #[test]
    fn file_outline_glob_expands_to_every_matching_file() {
        let g = glob_fixture();
        let r = file_outline(
            &g,
            &FileOutlineParams {
                file: vec!["src/*.rs".into()],
                ..Default::default()
            },
        );
        assert!(r.ok());
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].file.as_deref(), Some("src/a.rs"));

        let deep = file_outline(
            &g,
            &FileOutlineParams {
                file: vec!["src/**/*.rs".into()],
                ..Default::default()
            },
        );
        let outlined: Vec<&str> = deep.files.iter().filter_map(|f| f.file.as_deref()).collect();
        assert_eq!(outlined, vec!["src/a.rs", "src/deep/b.rs"]);
    }

    /// Over the cap, the extra paths are named rather than dropped — a
    /// truncated answer that does not say so is the failure mode worth
    /// testing for.
    #[test]
    fn file_outline_glob_reports_the_files_it_did_not_expand() {
        let g = glob_fixture();
        let r = file_outline(
            &g,
            &FileOutlineParams {
                file: vec!["src/**/*.rs".into()],
                max_files: Some(1),
                ..Default::default()
            },
        );
        assert!(!r.ok(), "the overflow entry carries an error");
        assert_eq!(r.files[0].file.as_deref(), Some("src/a.rs"));
        let overflow = r.files.last().unwrap();
        assert_eq!(overflow.candidates, vec!["src/deep/b.rs".to_string()]);
        assert!(overflow.error.as_ref().unwrap().contains("max_files"));
    }

    #[test]
    fn file_outline_glob_matching_nothing_explains_itself() {
        let g = glob_fixture();
        let r = file_outline(
            &g,
            &FileOutlineParams {
                file: vec!["src/*.ts".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok());
        assert!(r.files[0].error.as_ref().unwrap().contains("**/"));
    }

    /// The id-taking tools accept a bare name. Before this, `find_usages
    /// callee` was an error telling the caller to go look the id up.
    #[test]
    fn id_taking_tools_accept_a_bare_name() {
        let g = fixture();
        let r = find_usages(
            &g,
            SourceCtx::repo_only(Path::new(NO_REPO)),
            &FindUsagesParams {
                node_id: vec!["callee".into()],
                ..Default::default()
            },
        );
        assert!(r.ok(), "a name resolves like an id");
        assert_eq!(r.nodes[0].users[0].symbol.name, "caller");
    }

    /// One pattern seeds one merged walk over every symbol it names.
    #[test]
    fn traverse_expands_a_pattern_into_several_seeds() {
        let g = glob_fixture();
        let r = traverse(
            &g,
            &TraverseParams {
                node_id: vec!["run_*".into()],
                hops: Some(1),
                ..Default::default()
            },
        );
        assert!(r.ok());
        assert_eq!(r.seeds.len(), 3, "three run_* functions seeded the walk");
    }

    /// The cap is reported, not silently applied.
    #[test]
    fn expansion_over_the_cap_is_reported() {
        let g = glob_fixture();
        let expanded = expand_node_refs(&g, &["run_*".to_string()], 2);
        assert_eq!(expanded.len(), 3, "two ids plus the pattern itself");
        assert_eq!(expanded[2], "run_*");
        let msg = unresolved_ref_error(&g, "run_*", 2);
        assert!(msg.contains("matches 3 symbols"), "got: {msg}");
        assert!(msg.contains("first 2"), "got: {msg}");
    }

    /// An endpoint that names several nodes has to be refused, not guessed:
    /// "is A connected to B" has a different answer per candidate.
    #[test]
    fn single_ref_resolution_refuses_ambiguity() {
        let g = glob_fixture();
        assert_eq!(
            resolve_single_ref(&g, "run_serve").unwrap(),
            "function:src/a.rs:7:run_serve"
        );
        let err = resolve_single_ref(&g, "run_*").unwrap_err();
        assert!(err.contains("matches 3 symbols"), "got: {err}");
        let err = resolve_single_ref(&g, "nope").unwrap_err();
        assert!(err.contains("No symbol named 'nope'"), "got: {err}");
    }

    /// The error text has to name the actual problem — a missing id, a name
    /// that matches nothing, and a mis-written pattern send the reader in
    /// three different directions.
    #[test]
    fn unresolved_refs_are_diagnosed_by_shape() {
        let g = glob_fixture();
        assert!(unresolved_ref_error(&g, "function:nope", 25).contains("No node with id"));
        assert!(unresolved_ref_error(&g, "zzz", 25).contains("No symbol named"));
        assert!(unresolved_ref_error(&g, "zzz_*", 25).contains("No symbol matches pattern"));
    }

    #[test]
    fn file_outline_resolves_suffix_and_orders_by_line() {
        let g = fixture();
        let r = file_outline(
            &g,
            &FileOutlineParams {
                file: vec!["a.rs".into()],
                ..Default::default()
            },
        );
        assert!(r.ok());
        let entry = &r.files[0];
        assert_eq!(entry.file.as_deref(), Some("src/a.rs"));
        // File/Folder nodes are excluded; symbols come back in line order.
        assert_eq!(entry.symbols.len(), 2);
        assert_eq!(entry.symbols[0].name, "caller");
        assert_eq!(entry.symbols[1].name, "callee");
    }

    #[test]
    fn file_outline_rejects_non_file_node_id() {
        let g = fixture();
        let r = file_outline(
            &g,
            &FileOutlineParams {
                node_id: vec!["function:src/a.rs:1:caller".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok());
        assert!(r.files[0].error.as_ref().unwrap().contains("not a File"));
    }

    #[test]
    fn find_usages_walks_inbound_and_skips_contains() {
        let g = fixture();
        let r = find_usages(
            &g,
            SourceCtx::repo_only(Path::new("/nonexistent")),
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:7:callee".into()],
                ..Default::default()
            },
        );
        assert!(r.ok());
        assert_eq!(r.nodes[0].users.len(), 1);
        assert_eq!(r.nodes[0].users[0].symbol.name, "caller");
        assert_eq!(r.nodes[0].users[0].via_edge, "Calls");
        assert_eq!(r.nodes[0].users[0].depth, 1);

        // The Contains edge into `caller` must not count as a usage.
        let r2 = find_usages(
            &g,
            SourceCtx::repo_only(Path::new("/nonexistent")),
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:1:caller".into()],
                ..Default::default()
            },
        );
        assert!(r2.nodes[0].users.is_empty());
    }

    #[test]
    fn traverse_respects_direction() {
        let g = fixture();
        let callee = "function:src/a.rs:7:callee";

        // Nothing downstream of callee...
        let out = traverse(
            &g,
            &TraverseParams {
                node_id: vec![callee.into()],
                hops: Some(2),
                ..Default::default()
            },
        );
        assert!(out.ok());
        assert_eq!(out.nodes.len(), 1, "only the seed itself");

        // ...but caller points at it.
        let inbound = traverse(
            &g,
            &TraverseParams {
                node_id: vec![callee.into()],
                hops: Some(1),
                direction: Some("inbound".into()),
                ..Default::default()
            },
        );
        let names: Vec<&str> = inbound.nodes.iter().map(|n| n.symbol.name.as_str()).collect();
        assert!(names.contains(&"caller"), "inbound must reach the caller");
        assert_eq!(
            inbound.nodes.iter().find(|n| n.symbol.name == "callee").unwrap().distance,
            0
        );
        assert_eq!(
            inbound.nodes.iter().find(|n| n.symbol.name == "caller").unwrap().distance,
            1
        );
    }

    #[test]
    fn traverse_filters_edge_types_and_reports_missing_seeds() {
        let g = fixture();
        // `Contains` only — the Calls edge must not be followed.
        let r = traverse(
            &g,
            &TraverseParams {
                node_id: vec!["file:src/a.rs".into(), "function:nope".into()],
                hops: Some(3),
                edge_types: vec!["contains".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok());
        assert_eq!(r.missing, vec!["function:nope".to_string()]);
        let names: Vec<&str> = r.nodes.iter().map(|n| n.symbol.name.as_str()).collect();
        assert!(names.contains(&"caller"), "Contains edge followed");
        assert!(
            !names.contains(&"callee"),
            "callee is only reachable via Calls, which was filtered out"
        );
    }

    /// `find_usages` is `traverse` pinned to inbound with a default edge set;
    /// on the same input the two must agree about who the users are.
    #[test]
    fn traverse_inbound_agrees_with_find_usages() {
        let g = fixture();
        let callee = "function:src/a.rs:7:callee";
        let usages = find_usages(
            &g,
            SourceCtx::repo_only(Path::new("/nonexistent")),
            &FindUsagesParams {
                node_id: vec![callee.into()],
                ..Default::default()
            },
        );
        let t = traverse(
            &g,
            &TraverseParams {
                node_id: vec![callee.into()],
                hops: Some(1),
                direction: Some("inbound".into()),
                edge_types: USAGE_EDGE_TYPES.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        );
        let mut a: Vec<&str> = usages.nodes[0].users.iter().map(|u| u.symbol.id.as_str()).collect();
        let mut b: Vec<&str> = t
            .nodes
            .iter()
            .filter(|n| n.distance > 0)
            .map(|n| n.symbol.id.as_str())
            .collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn project_overview_excludes_contains_from_in_degree() {
        let g = fixture();
        let r = project_overview(&g, Path::new("/repo"), Path::new("/repo/graph.json"));
        assert_eq!(r.node_count, 3);
        assert_eq!(r.edge_count, 2);
        // Only `callee` has a non-Contains inbound edge.
        assert_eq!(r.hotspots.len(), 1);
        assert_eq!(r.hotspots[0].symbol.name, "callee");
        assert_eq!(r.hotspots[0].in_degree, 1);
    }

    #[test]
    fn graph_schema_reports_edge_shapes() {
        let g = fixture();
        let r = graph_schema(&g, Path::new("/repo/graph.json"));
        let calls = r.edge_types.iter().find(|e| e.name == "Calls").unwrap();
        assert_eq!(calls.count, 1);
        assert_eq!(calls.shapes[0].shape, "Function→Function");
        assert!(r.vocabulary.contains(&"Contains".to_string()));
    }

    /// Every renderer, both styles. Markdown must never leak an ANSI escape
    /// and ANSI must never leak a Markdown backtick — the two surfaces share
    /// one layout, so a hardcoded marker in either direction shows up here.
    #[test]
    fn renderers_never_leak_the_other_surfaces_markup() {
        let g = fixture();
        let repo = Path::new("/repo");
        let gp = Path::new("/repo/graph.json");

        let symbols = find_symbols(
            &g,
            &FindSymbolsParams {
                name: vec!["caller".into(), "nothing-matches-this".into()],
                ..Default::default()
            },
        );
        let outline = file_outline(
            &g,
            &FileOutlineParams {
                file: vec!["a.rs".into(), "missing.rs".into()],
                ..Default::default()
            },
        );
        let usages = find_usages(
            &g,
            SourceCtx::repo_only(Path::new("/nonexistent")),
            &FindUsagesParams {
                node_id: vec![
                    "function:src/a.rs:7:callee".into(),
                    "function:src/a.rs:1:caller".into(),
                ],
                ..Default::default()
            },
        );
        let overview = project_overview(&g, repo, gp);
        let schema = graph_schema(&g, gp);
        let missing_path = ShortestPathResult {
            source: "a".into(),
            target: "b".into(),
            found: false,
            reversed: false,
            length: None,
            path: vec![],
            nodes: vec![],
        };

        let cases: Vec<(&str, Box<dyn Fn(Render) -> String>)> = vec![
            ("find_symbols", Box::new(move |s| render_find_symbols(&symbols, s))),
            ("file_outline", Box::new(move |s| render_file_outline(&outline, s))),
            ("find_usages", Box::new(move |s| render_find_usages(&usages, s))),
            (
                "project_overview",
                Box::new(move |s| render_project_overview(&overview, s)),
            ),
            ("graph_schema", Box::new(move |s| render_graph_schema(&schema, s))),
            (
                "shortest_path",
                Box::new(move |s| render_shortest_path(&missing_path, s, false)),
            ),
        ];

        for (name, render) in &cases {
            let md = render(Render::Markdown);
            assert!(
                !md.contains('\x1b'),
                "{} markdown output leaked an ANSI escape",
                name
            );
            let ansi = render(Render::Ansi);
            assert!(
                !ansi.contains('`'),
                "{} ANSI output leaked a markdown backtick",
                name
            );
        }
    }
}
