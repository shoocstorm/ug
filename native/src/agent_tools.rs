//! Graph.json-backed agent tools — one implementation, three transports.
//!
//! `ug find_symbol` (CLI), `POST /api/tools/find_symbol` (HTTP) and the MCP
//! `find_symbol` tool all land in the same function here. Each tool takes a
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

use crate::types::{GraphData, GraphEdgeType, GraphNode, GraphNodeType};
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
    fn bold(self, s: &str) -> String {
        match self {
            Render::Ansi => format!("{}{}{}", C_BOLD, s, C_RESET),
            Render::Markdown => format!("**{}**", s),
        }
    }

    fn dim(self, s: &str) -> String {
        match self {
            Render::Ansi => format!("{}{}{}", C_DIM, s, C_RESET),
            // Markdown has no "dim"; plain text keeps the line readable.
            Render::Markdown => s.to_string(),
        }
    }

    /// A node id, or anything else meant to be copied verbatim into a
    /// follow-up call.
    fn id(self, s: &str) -> String {
        match self {
            Render::Ansi => format!("{}{}{}", C_CYAN, s, C_RESET),
            Render::Markdown => format!("`{}`", s),
        }
    }

    fn heading(self, s: &str) -> String {
        match self {
            Render::Ansi => format!("{}{}{}", C_BOLD, s, C_RESET),
            Render::Markdown => format!("## {}", s),
        }
    }
}

/// Separator between sections of a batched call (one per node id / file /
/// name). Skipped before the first section.
fn section_break(out: &mut String, i: usize, style: Render) {
    if i > 0 {
        out.push('\n');
        out.push_str(&style.dim("────────────────────────────────────────"));
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
];

/// Default edge types for `find_usages` — dependency-ish edges only, no
/// `Contains` (structure), so results mean "code that uses this", not "the
/// folder that holds it".
pub const USAGE_EDGE_TYPES: &[&str] = &["calls", "references", "imports", "extends", "implements"];

/// `file:<path>` is how File node ids print, and users copy that straight
/// into file-taking params. Accept both forms.
pub fn strip_file_id_prefix(file: &str) -> &str {
    file.strip_prefix("file:").unwrap_or(file)
}

/// Node ids from the indexer are `<kind>:<path>:<line>:<name>`. The CLI takes
/// bare positionals, so it needs a heuristic to tell an id from a name.
pub fn looks_like_node_id(s: &str) -> bool {
    s.contains(':')
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
}

/// How much of a docstring travels in a list result. Full text is available
/// via `get_code`, so listings stay scannable.
const DOC_PREVIEW_CHARS: usize = 200;

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
// find_symbol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FindSymbolParams {
    /// Direct node id lookup — O(1), skips the search entirely.
    pub node_id: Vec<String>,
    /// Identifier (or fragment) to match against node names.
    pub name: Vec<String>,
    /// Restrict to these node types (case-insensitive).
    pub node_types: Vec<String>,
    /// Only symbols whose file path starts with this repo-relative prefix.
    pub file_prefix: Option<String>,
    pub limit: Option<usize>,
}

const DEFAULT_SYMBOL_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct SymbolQueryResult {
    pub query: String,
    /// `"id"` for a direct lookup, `"name"` for a ranked name search.
    pub kind: &'static str,
    pub total: usize,
    pub items: Vec<SymbolRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindSymbolResult {
    pub queries: Vec<SymbolQueryResult>,
}

impl FindSymbolResult {
    pub fn ok(&self) -> bool {
        self.queries.iter().all(|q| q.error.is_none())
    }
}

pub fn find_symbol(graph: &GraphData, p: &FindSymbolParams) -> FindSymbolResult {
    let limit = p.limit.unwrap_or(DEFAULT_SYMBOL_LIMIT);
    let types: Vec<String> = p.node_types.iter().map(|t| t.to_lowercase()).collect();
    let mut queries = Vec::new();

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
                    "No node with id '{}' — ids come from find_symbol, search or file_outline.",
                    id
                )),
            },
        });
    }

    for name in &p.name {
        let q = name.to_lowercase();
        let mut hits: Vec<(u8, &GraphNode)> = Vec::new();
        for n in &graph.nodes {
            if !types.is_empty() && !types.contains(&node_type_str(&n.node_type).to_lowercase()) {
                continue;
            }
            if let Some(prefix) = &p.file_prefix {
                if !n.file.as_deref().unwrap_or("").starts_with(prefix.as_str()) {
                    continue;
                }
            }
            let nm = n.name.to_lowercase();
            // exact > prefix > substring; ties broken by shorter (closer) name.
            let rank = if nm == q {
                0
            } else if nm.starts_with(&q) {
                1
            } else if nm.contains(&q) {
                2
            } else {
                3
            };
            if rank < 3 {
                hits.push((rank, n));
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.len().cmp(&b.1.name.len())));
        let total = hits.len();
        queries.push(SymbolQueryResult {
            query: name.clone(),
            kind: "name",
            total,
            items: hits
                .iter()
                .take(limit)
                .map(|(_, n)| SymbolRef::from_node(n))
                .collect(),
            error: None,
        });
    }

    FindSymbolResult { queries }
}

pub fn render_find_symbol(r: &FindSymbolResult, style: Render) -> String {
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
            let showing = if q.total > q.items.len() {
                format!(", showing {}", q.items.len())
            } else {
                String::new()
            };
            line(
                &mut out,
                &format!(
                    "{} — {} match(es){}",
                    style.heading(&format!("Symbols matching '{}'", q.query)),
                    q.total,
                    showing
                ),
            );
            out.push('\n');
        }
        if q.items.is_empty() {
            line(
                &mut out,
                &format!(
                    "No name matches. Try a shorter fragment, drop the type/file filters, or use {} for a concept-level query.",
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
    pub node_id: Vec<String>,
    /// Repo-relative path, unique suffix, or `file:<path>` id.
    pub file: Vec<String>,
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
                    "No node with id '{}' — ids come from find_symbol, search or file_outline.",
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
        files.push(outline_by_path(graph, f, strip_file_id_prefix(f)));
    }

    FileOutlineResult { files }
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
            line(
                &mut out,
                &format!(
                    "- L{}-{}  {}  {}  id: {}",
                    start,
                    end,
                    s.node_type,
                    style.bold(&s.name),
                    style.id(&s.id)
                ),
            );
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
    pub node_id: Vec<String>,
    /// Repo-relative path, used when `node_id` is empty.
    pub file: Option<String>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub max_chars: Option<usize>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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

pub fn get_code(graph: &GraphData, repo_root: &Path, p: &GetCodeParams) -> GetCodeResult {
    let max_chars = p.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
    let mut slices = Vec::new();

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
                    error: Some("Pass node_id (one or more ids) or file.".into()),
                }],
            };
        };
        let file = strip_file_id_prefix(file);
        slices.push(read_slice(
            repo_root,
            file,
            p.start_line.unwrap_or(1),
            p.end_line.unwrap_or(usize::MAX),
            None,
            max_chars,
        ));
        return GetCodeResult { slices };
    }

    for id in &p.node_id {
        let Some(n) = graph.nodes.iter().find(|n| n.id == *id) else {
            slices.push(err_slice(
                id,
                format!(
                    "No node with id '{}' — ids come from find_symbol, search or file_outline.",
                    id
                ),
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
        slices.push(read_slice(repo_root, f, start, end, Some(n), max_chars));
    }

    GetCodeResult { slices }
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
        error: Some(error),
    }
}

fn read_slice(
    repo_root: &Path,
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

    let content = match std::fs::read_to_string(repo_root.join(file)) {
        Ok(c) => c,
        Err(_) => {
            return err_slice(
                &title,
                format!(
                    "{} not found under repo root {} — the index may be stale (re-run ug gen).",
                    file,
                    repo_root.display()
                ),
            )
        }
    };

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

    CodeSlice {
        title,
        file: Some(file.to_string()),
        start_line: Some(from),
        end_line: Some(to),
        total_lines: Some(all.len()),
        doc: node.and_then(|n| n.docstring.clone()),
        code: Some(text),
        truncated_chars: truncated,
        error: None,
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
                "{}  —  {}:{}-{} (of {} lines)",
                style.bold(&s.title),
                s.file.as_deref().unwrap_or("?"),
                s.start_line.unwrap_or(0),
                s.end_line.unwrap_or(0),
                s.total_lines.unwrap_or(0)
            ),
        );
        if let Some(d) = &s.doc {
            line(&mut out, &style.dim(&format!("doc: {}", d)));
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
    pub node_id: Vec<String>,
    /// Transitive depth, 1-3. Default 1 = direct users only.
    pub hops: Option<u32>,
    /// Defaults to [`USAGE_EDGE_TYPES`].
    pub edge_types: Vec<String>,
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
}

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

pub fn find_usages(graph: &GraphData, p: &FindUsagesParams) -> FindUsagesResult {
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
    for node_id in &p.node_id {
        let Some(subject) = by_id.get(node_id.as_str()) else {
            nodes.push(UsagesEntry {
                query: node_id.clone(),
                subject: None,
                users: vec![],
                error: Some(format!(
                    "No node with id '{}' — ids come from find_symbol, search or file_outline.",
                    node_id
                )),
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
                            });
                        users.push(Usage {
                            symbol,
                            depth,
                            via_edge: (*et).to_string(),
                            via_target: (*target).to_string(),
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

#[derive(Debug, Clone, Serialize)]
pub struct ProjectOverviewResult {
    pub repo_root: String,
    pub graph_path: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub node_types: Vec<TypeCount>,
    pub edge_types: Vec<TypeCount>,
    pub biggest_files: Vec<TypeCount>,
    pub hotspots: Vec<Hotspot>,
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

    ProjectOverviewResult {
        repo_root: repo_root.display().to_string(),
        graph_path: graph_path.display().to_string(),
        node_count: graph.nodes.len(),
        edge_count: graph.edges.len(),
        node_types: top_counts(&node_types, 10),
        edge_types: top_counts(&edge_types, 10),
        biggest_files: top_counts(&symbols_per_file, 10),
        hotspots,
    }
}

pub fn render_project_overview(r: &ProjectOverviewResult, style: Render) -> String {
    let mut out = String::new();
    line(&mut out, &style.heading("Project overview"));
    line(&mut out, &style.dim(&format!("repo: {}", r.repo_root)));
    line(&mut out, &style.dim(&format!("graph: {}", r.graph_path)));
    out.push('\n');

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
        line(
            &mut out,
            &format!(
                "- {} {}  ←{}  {}  id: {}",
                h.symbol.node_type,
                style.bold(&h.symbol.name),
                h.in_degree,
                style.dim(&h.symbol.loc()),
                style.id(&h.symbol.id)
            ),
        );
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

    GraphSchemaResult {
        graph_path: graph_path.display().to_string(),
        node_types: top_counts(&node_counts, usize::MAX),
        edge_types,
        vocabulary: EDGE_TYPE_VOCABULARY.iter().map(|s| s.to_string()).collect(),
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
    out
}

// ---------------------------------------------------------------------------
// shortest_path
// ---------------------------------------------------------------------------

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
    raw: &str,
    source: &str,
    target: &str,
    strict: bool,
) -> ShortestPathResult {
    let parse = |json: String| -> crate::types::PathResult {
        serde_json::from_str(&json).unwrap_or(crate::types::PathResult {
            path: vec![],
            found: false,
            length: None,
        })
    };

    let mut reversed = false;
    let mut result = parse(crate::find_shortest_path(
        raw.to_string(),
        source.to_string(),
        target.to_string(),
    ));
    if !result.found && !strict {
        reversed = true;
        result = parse(crate::find_shortest_path(
            raw.to_string(),
            target.to_string(),
            source.to_string(),
        ));
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
                "They may be connected only through shared ancestors — try {} from each id.",
                style.id("graph_bfs <id> -d both")
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
        }
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
        };
        let r = find_symbol(
            &g,
            &FindSymbolParams {
                name: vec!["call".into()],
                ..Default::default()
            },
        );
        assert_eq!(r.queries[0].total, 3);
        let order: Vec<&str> = r.queries[0].items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(order, vec!["call", "caller", "do_call"]);
    }

    #[test]
    fn find_symbol_honours_type_and_file_filters() {
        let g = fixture();
        let all = find_symbol(
            &g,
            &FindSymbolParams {
                name: vec!["a".into()],
                ..Default::default()
            },
        );
        assert!(all.queries[0].total >= 3);

        let functions_only = find_symbol(
            &g,
            &FindSymbolParams {
                name: vec!["a".into()],
                node_types: vec!["function".into()],
                ..Default::default()
            },
        );
        assert!(functions_only.queries[0]
            .items
            .iter()
            .all(|i| i.node_type == "Function"));

        let nothing = find_symbol(
            &g,
            &FindSymbolParams {
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
        let r = find_symbol(
            &g,
            &FindSymbolParams {
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
        let r = find_symbol(
            &g,
            &FindSymbolParams {
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
        let r = find_symbol(
            &g,
            &FindSymbolParams {
                node_id: vec!["function:nope".into()],
                ..Default::default()
            },
        );
        assert!(!r.ok());
        assert_eq!(r.queries[0].total, 0);
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
            &FindUsagesParams {
                node_id: vec!["function:src/a.rs:1:caller".into()],
                ..Default::default()
            },
        );
        assert!(r2.nodes[0].users.is_empty());
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

        let symbols = find_symbol(
            &g,
            &FindSymbolParams {
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
            ("find_symbol", Box::new(move |s| render_find_symbol(&symbols, s))),
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
