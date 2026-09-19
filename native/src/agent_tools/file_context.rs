//! `file_context` — agent tool.
//!
//! The file-level counterpart to [`context`]. `context` answers "what is this
//! symbol and what breaks if I change it"; this answers the same question for
//! the unit an agent is actually handed — a path from a diff, a stack trace or
//! an `@`-mention.
//!
//! Replaces the command formerly called `file_outline`, whose entire output is
//! the `outline` role here. Two commands would have meant two MCP tool
//! descriptions for one question, and a tool description is prompt text the
//! caller pays for on every request.

use super::*;

/// The roles a [`FileContextItem`] can carry, in the order they are
/// assembled, budgeted and rendered.
///
/// The order is a priority claim, not a taxonomy. Handed a file, an agent
/// needs to know what is in it before anything else; then who would notice if
/// it changed; then what it leans on; then what re-verifies it; then the wider
/// blast radius; then what sits beside it. When the budget runs out it runs
/// out from the right.
pub const FILE_CONTEXT_ROLES: &[&str] =
    &["outline", "importer", "import", "test", "dependent", "sibling"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileContextParams {
    /// Direct File node id lookup.
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Repo-relative path, unique path suffix, `file:<path>` id, or a path
    /// glob (`src/**/*.ts`).
    #[serde(deserialize_with = "de_one_or_many")]
    pub file: Vec<String>,
    #[serde(alias = "maxChars")]
    pub max_chars: Option<usize>,
    /// Keep only these roles. Empty means all of [`FILE_CONTEXT_ROLES`].
    #[serde(deserialize_with = "de_one_or_many")]
    pub include: Vec<String>,
    /// Cap on files reported per glob (default 20).
    #[serde(alias = "maxFiles", alias = "limit", alias = "k")]
    pub max_files: Option<usize>,
}

/// Default budget for one file's context.
///
/// Lower than `context`'s 12k because nothing here carries source: the
/// measured six-call workaround this replaces —  `file_outline` +
/// `find_usages` + `traverse` + three `analyze` presets — came to roughly
/// 8,200 characters, most of it `traverse` repeating the outline.
const FILE_CONTEXT_DEFAULT_MAX_CHARS: usize = 8_000;

/// Rendering overhead every report pays: the heading, the id line, the facts
/// line, the budget line, six section rules and the trailing hint. See
/// [`Budget::new`] for why it is charged up front.
const FILE_CONTEXT_CHROME_RESERVE: usize = 320;

/// Ceiling on the share of the budget the outline may take, so a 400-symbol
/// file cannot crowd out every importer and test — the parts that answer
/// questions the outline cannot.
const OUTLINE_SHARE: f64 = 0.5;

const IMPORTER_CAP: usize = 15;
const IMPORT_CAP: usize = 15;
const TEST_CAP: usize = 10;
const DEPENDENT_CAP: usize = 15;
const SIBLING_CAP: usize = 24;

/// How far inbound the dependent walk goes. Matches `analyze impact`, so the
/// two tools do not answer "what is the blast radius" with different numbers.
const DEPENDENT_HOPS: u32 = 3;

/// How far inbound the test walk goes. Matches `context`'s `test` role: a
/// test reaching a symbol through one helper still counts as covering it,
/// three hops of indirection does not.
const TEST_HOPS: u32 = 2;

/// Per-row overhead of an outline line — `  L12-34  Function  ` plus the
/// newline. Deliberately not [`ITEM_CHROME`]: an outline row is one line, not
/// the two-line `render_bullet` that constant exists to charge for, and using
/// it here would invent 26 phantom characters per symbol — over 1,500 on a
/// large file, a fifth of the default budget.
const OUTLINE_ROW_CHROME: usize = 14;

/// How much of a symbol's doc comment rides on its outline row. One short
/// clause is enough to tell whether this is the symbol you want; the outline
/// is a table of contents, not a summary.
const OUTLINE_DOC_CHARS: usize = 90;

/// How many files one glob reports before the rest are listed by name.
const DEFAULT_CONTEXT_FILES: usize = 20;

/// What the graph knows about the file itself, as opposed to its
/// neighbourhood.
#[derive(Debug, Clone, Serialize)]
pub struct FileFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    /// Lines in the file. Absent on a graph written before schema version 7,
    /// which is why it is an `Option` rather than a `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub is_test: bool,
    pub symbols: usize,
    /// Raw import specifiers the indexer read from the file, resolved or not.
    #[serde(skip_serializing_if = "is_zero")]
    pub import_specifiers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileContextItem {
    /// One of [`FILE_CONTEXT_ROLES`] — why this item is in the report.
    pub role: &'static str,
    /// The specific relationship, e.g. `imports this file`.
    pub why: String,
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// For the roles that aggregate a file's symbols rather than naming one —
    /// `test` and `dependent` — how many symbols this file contributes.
    #[serde(skip_serializing_if = "is_zero")]
    pub symbols: usize,
    /// Up to two named symbols behind an aggregate row, so a `test` or
    /// `dependent` entry is a lead and not just a count.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileContextEntry {
    /// The reference as the caller wrote it.
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<FileFacts>,
    pub items: Vec<FileContextItem>,
    /// Populated when a path matched more than one indexed file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FileContextEntry {
    fn failed(query: &str, error: String, candidates: Vec<String>) -> Self {
        FileContextEntry {
            query: query.to_string(),
            file: None,
            id: None,
            facts: None,
            items: Vec::new(),
            candidates,
            error: Some(error),
        }
    }

    fn count(&self, role: &str) -> usize {
        self.items.iter().filter(|i| i.role == role).count()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileContextResult {
    pub files: Vec<FileContextEntry>,
    pub max_chars: usize,
    /// Characters spent, chrome included. Close but not exact, for the reason
    /// [`ContextResult::used_chars`] gives: assembly and rendering are
    /// separate steps, so the renderer's fixed overhead is charged as an
    /// estimate. Treat it as a budget, not a guarantee.
    pub used_chars: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<DroppedRole>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Whether rendered outline rows append the full node id — `false` when
    /// the CLI is piped, as in the outline this role replaces.
    #[serde(skip)]
    pub show_ids: bool,
}

impl FileContextResult {
    pub fn ok(&self) -> bool {
        self.files.iter().all(|f| f.error.is_none())
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// What a caller's file reference turned out to mean.
enum FileRef {
    One(String),
    /// A suffix matching several indexed files — the caller picks.
    Many(Vec<String>),
    None,
}

/// Resolve `path` to one indexed file: exact repo-relative match first, then a
/// unique path suffix, so `context.rs` works when only one file ends that way.
fn resolve_file(graph: &GraphData, path: &str) -> FileRef {
    if graph.nodes.iter().any(|n| n.file.as_deref() == Some(path)) {
        return FileRef::One(path.to_string());
    }
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
    match matches.len() {
        0 => FileRef::None,
        1 => FileRef::One(matches.remove(0)),
        _ => FileRef::Many(matches),
    }
}

/// Every indexed file path matching a glob, sorted.
fn files_matching_glob(graph: &GraphData, glob: &str) -> Result<Vec<String>, String> {
    let pat = Pattern::new(glob, Mode::Path)?;
    let mut matched: Vec<String> = graph
        .nodes
        .iter()
        .filter_map(|n| n.file.as_ref())
        .filter(|f| pat.matches(f))
        .cloned()
        .collect();
    matched.sort();
    matched.dedup();
    Ok(matched)
}

/// The symbols a file declares, in line order.
///
/// Excludes the file's own node — a config file is a `Config` node rather than
/// a `File` one, so filtering on node type alone would list a config file as a
/// symbol inside itself.
fn symbols_in_file<'g>(graph: &'g GraphData, path: &str, file_id: &str) -> Vec<&'g GraphNode> {
    let mut symbols: Vec<&GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| n.file.as_deref() == Some(path))
        .filter(|n| n.id != file_id)
        .filter(|n| !matches!(n.node_type, GraphNodeType::File | GraphNodeType::Folder))
        .collect();
    symbols.sort_by_key(|n| n.start_line.unwrap_or(0));
    symbols
}

/// A node that stands for a whole file rather than a symbol inside one.
///
/// These are skipped by the inbound walk: a `File`-to-symbol `References`
/// edge is the import relationship, which the `importer` role already reports
/// by name, and counting it again would inflate the blast radius with the
/// thing the reader just read one section earlier.
fn is_file_level(n: &GraphNode) -> bool {
    matches!(
        n.node_type,
        GraphNodeType::File | GraphNodeType::Folder | GraphNodeType::Config
    )
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Everything an agent needs to work in one file, in one call.
///
/// Replaces the six round trips an agent otherwise spends assembling the same
/// picture — `file_outline` → `find_usages` → `traverse` → `analyze impact` →
/// `analyze impact_summary` → `analyze retest_scope` — with a single
/// token-budgeted report whose every entry says why it is there. On this
/// repository those six calls came to ~8,200 characters, of which `traverse`
/// alone was 5,200 and its first hop was a verbatim copy of the outline.
///
/// No new analysis: this is assembly and budgeting over facts already in
/// `graph.json`, which is why it needs neither the store nor an embedder.
/// The `analyze` presets are the specification for the two walking roles, not
/// the implementation — they are async over an open store, and every agent
/// tool must answer from `graph.json` alone.
///
/// Composition, in budget priority order:
///
/// - **outline** — the symbols the file declares, in line order, capped at
///   [`OUTLINE_SHARE`] of the budget.
/// - **importer** — files whose `Imports` edge points here. A real stored
///   edge, not an inference.
/// - **import** — the files and third-party dependencies this one reaches for.
/// - **test** — test files reaching this file's symbols within [`TEST_HOPS`],
///   by the same [`is_test_node`] definition `context` uses.
/// - **dependent** — non-test files reaching them within [`DEPENDENT_HOPS`],
///   grouped by file: the blast radius of changing this file.
/// - **sibling** — what else lives in the same folder.
///
/// Handed several files it reports the outline of each and says so. A budget
/// split across several neighbourhoods would thin every one of them, which is
/// the same reason `context` takes exactly one symbol.
///
/// [`is_test_node`]: crate::storage::facts::is_test_node
pub fn file_context(graph: &GraphData, p: &FileContextParams) -> FileContextResult {
    let max_chars = p
        .max_chars
        .unwrap_or(FILE_CONTEXT_DEFAULT_MAX_CHARS)
        .max(1);
    let mut notes: Vec<String> = Vec::new();

    // An unrecognised role would otherwise silently empty the report, so it
    // is named rather than ignored.
    let include: Vec<&'static str> = if p.include.is_empty() {
        FILE_CONTEXT_ROLES.to_vec()
    } else {
        let mut kept: Vec<&'static str> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for want in &p.include {
            match FILE_CONTEXT_ROLES
                .iter()
                .find(|r| r.eq_ignore_ascii_case(want))
            {
                Some(r) => kept.push(r),
                None => unknown.push(want.clone()),
            }
        }
        if !unknown.is_empty() {
            notes.push(format!(
                "unknown role(s) ignored: {} — include takes {}.",
                unknown.join(", "),
                FILE_CONTEXT_ROLES.join(", ")
            ));
        }
        if kept.is_empty() {
            FILE_CONTEXT_ROLES.to_vec()
        } else {
            kept
        }
    };
    let wants = |role: &str| include.iter().any(|r| *r == role);

    let ok_entry = |query: &str, path: &str| FileContextEntry {
        query: query.to_string(),
        file: Some(path.to_string()),
        id: Some(format!("file:{}", path)),
        facts: None,
        items: Vec::new(),
        candidates: Vec::new(),
        error: None,
    };

    let mut entries: Vec<FileContextEntry> = Vec::new();

    // A node id — any node id. A symbol resolves to the file that holds it,
    // the same coercion `analyze` applies to a non-path `target`: an agent
    // holding a symbol id and wanting its file should not have to look the
    // path up first.
    for id in &p.node_id {
        match graph.nodes.iter().find(|n| n.id == *id) {
            None => entries.push(FileContextEntry::failed(
                id,
                format!(
                    "No node with id '{}' — ids come from find_symbols, search or file_context.",
                    id
                ),
                vec![],
            )),
            Some(n) => match n.file.as_deref() {
                None => entries.push(FileContextEntry::failed(
                    id,
                    format!("Node '{}' has no file path.", id),
                    vec![],
                )),
                Some(f) => {
                    if !is_file_level(n) {
                        notes.push(format!(
                            "'{}' is a {}, not a file — reporting {}, the file that holds it.",
                            n.name,
                            node_type_str(&n.node_type),
                            f
                        ));
                    }
                    entries.push(ok_entry(id, f));
                }
            },
        }
    }

    let max_files = p.max_files.unwrap_or(DEFAULT_CONTEXT_FILES).max(1);
    for f in &p.file {
        let path = strip_file_id_prefix(f);
        if pattern::is_pattern(path) {
            match files_matching_glob(graph, path) {
                Err(e) => entries.push(FileContextEntry::failed(f, e, vec![])),
                Ok(matched) if matched.is_empty() => entries.push(FileContextEntry::failed(
                    f,
                    format!(
                        "No indexed file matches pattern '{}'. Paths are repo-relative, and '*' does not cross '/' — use '**/' for that (e.g. 'src/**/*.ts').",
                        path
                    ),
                    vec![],
                )),
                Ok(mut matched) => {
                    let overflow: Vec<String> = matched.split_off(matched.len().min(max_files));
                    for m in &matched {
                        entries.push(ok_entry(m, m));
                    }
                    if !overflow.is_empty() {
                        entries.push(FileContextEntry::failed(
                            f,
                            format!(
                                "'{}' matches {} more file(s) than the {}-file cap — name them, narrow the pattern, or raise max_files.",
                                path,
                                overflow.len(),
                                max_files
                            ),
                            // The names are the useful part: the caller can
                            // ask about exactly the ones it wants without
                            // re-running a broader glob.
                            overflow.iter().take(50).cloned().collect(),
                        ));
                    }
                }
            }
        } else {
            match resolve_file(graph, path) {
                FileRef::One(r) => entries.push(ok_entry(f, &r)),
                FileRef::Many(c) => entries.push(FileContextEntry::failed(
                    f,
                    format!("'{}' matches {} files — pass one of the candidates.", path, c.len()),
                    c,
                )),
                FileRef::None => entries.push(FileContextEntry::failed(
                    f,
                    format!(
                        "No indexed file matches '{}'. Pass a repo-relative path (project_overview lists the biggest files), or re-run ug gen if the file is new.",
                        path
                    ),
                    vec![],
                )),
            }
        }
    }

    if entries.is_empty() {
        entries.push(FileContextEntry::failed(
            "",
            "file_context needs a file — a repo-relative path, a unique path suffix, a file:<path> id, or a path glob.".into(),
            vec![],
        ));
    }

    // One file gets the whole report; several get the outline of each. The
    // budget is a claim about one neighbourhood, and dividing it across
    // several would thin every answer — the same reason `context` takes
    // exactly one symbol.
    let resolved_count = entries.iter().filter(|e| e.error.is_none()).count();
    let full = resolved_count == 1;
    if resolved_count > 1 {
        notes.push(format!(
            "{} files — outline only, and max_chars is per file. Pass one file for its importers, tests and blast radius.",
            resolved_count
        ));
    }

    // A budget is a claim about one file, so a batch gets it once per file.
    // Sharing a single budget across a survey gave the first file everything
    // and the rest a bare header — and the unbounded outline this replaces had
    // no per-call ceiling at all, only the `max_files` cap.
    let effective_max = if full {
        max_chars
    } else {
        max_chars.saturating_mul(resolved_count.max(1))
    };
    let mut budget = Budget::new(effective_max, FILE_CONTEXT_CHROME_RESERVE);
    let mut dropped: HashMap<&'static str, usize> = HashMap::new();
    let by_id = by_id_map(graph);


    // Adjacency for the two walking roles, built once and only when they are
    // actually wanted — on a large graph this is the expensive part of the
    // call, and `--include outline` should not pay for it.
    let in_adj: HashMap<&str, Vec<&str>> = if full && (wants("test") || wants("dependent")) {
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in &graph.edges {
            let et = edge_type_str(&e.edge_type);
            if !USAGE_EDGE_TYPES.iter().any(|t| t.eq_ignore_ascii_case(et)) {
                continue;
            }
            adj.entry(&*e.target).or_default().push(&*e.source);
        }
        adj
    } else {
        HashMap::new()
    };

    for (i, entry) in entries.iter_mut().enumerate() {
        if entry.error.is_some() {
            continue;
        }
        if i > 0 {
            budget.take(FILE_CONTEXT_CHROME_RESERVE);
        }
        let Some(path) = entry.file.clone() else { continue };
        let file_id = format!("file:{}", path);
        let file_node = by_id.get(file_id.as_str()).copied();
        let symbols = symbols_in_file(graph, &path, &file_id);

        entry.facts = Some(FileFacts {
            language: file_node.and_then(|n| n.language.clone()),
            classification: file_node
                .and_then(|n| n.classification.as_ref())
                .map(|c| crate::storage::facts::classification_str(c).to_string()),
            lines: file_node.and_then(|n| match (n.start_line, n.end_line) {
                (Some(s), Some(e)) if e >= s => Some(e - s + 1),
                _ => None,
            }),
            folder: Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .filter(|s| !s.is_empty()),
            is_test: file_node.is_some_and(crate::storage::facts::is_test_node),
            symbols: symbols.len(),
            import_specifiers: file_node.map_or(0, |n| n.imports.len()),
        });

        // ── outline ──────────────────────────────────────────────────────
        if wants("outline") {
            let allowance = if full {
                ((max_chars as f64 * OUTLINE_SHARE) as usize).min(budget.left())
            } else {
                // Outline-only, and one file's worth of budget — capped by
                // what is actually left so an early overrun cannot be repaid
                // by a later file.
                max_chars
                    .saturating_sub(FILE_CONTEXT_CHROME_RESERVE)
                    .min(budget.left())
            };
            let mut spent = 0usize;
            let mut shown = 0usize;
            for s in &symbols {
                let symbol = SymbolRef::from_node(s);
                let cost = outline_row_cost(&symbol);
                if spent + cost > allowance {
                    break;
                }
                spent += cost;
                shown += 1;
                budget.take(cost);
                entry.items.push(FileContextItem {
                    role: "outline",
                    why: String::new(),
                    symbol,
                    symbols: 0,
                    examples: vec![],
                });
            }
            if shown < symbols.len() {
                *dropped.entry("outline").or_default() += symbols.len() - shown;
            }
        }

        if !full {
            continue;
        }

        // ── importer / import ────────────────────────────────────────────
        if wants("importer") {
            let mut importers: Vec<&GraphNode> = graph
                .edges
                .iter()
                .filter(|e| &*e.target == file_id.as_str())
                .filter(|e| matches!(e.edge_type, GraphEdgeType::Imports))
                .filter_map(|e| by_id.get(&*e.source).copied())
                .collect();
            importers.sort_by(|a, b| a.name.cmp(&b.name));
            importers.dedup_by(|a, b| a.id == b.id);
            push_file_items(
                entry,
                &mut budget,
                &mut dropped,
                "importer",
                IMPORTER_CAP,
                importers.into_iter().map(|n| (n, "imports this file".to_string(), 0, vec![])).collect(),
            );
        }

        if wants("import") {
            let mut imports: Vec<(&GraphNode, String, usize, Vec<String>)> = graph
                .edges
                .iter()
                .filter(|e| &*e.source == file_id.as_str())
                .filter_map(|e| match e.edge_type {
                    GraphEdgeType::Imports => by_id
                        .get(&*e.target)
                        .copied()
                        .map(|n| (n, "this file imports it".to_string(), 0, vec![])),
                    GraphEdgeType::DependsOn => by_id
                        .get(&*e.target)
                        .copied()
                        .map(|n| (n, "third-party dependency".to_string(), 0, vec![])),
                    _ => None,
                })
                .collect();
            imports.sort_by(|a, b| a.0.name.cmp(&b.0.name));
            imports.dedup_by(|a, b| a.0.id == b.0.id);
            push_file_items(entry, &mut budget, &mut dropped, "import", IMPORT_CAP, imports);
        }

        // ── test / dependent ─────────────────────────────────────────────
        if wants("test") || wants("dependent") {
            let (tests, dependents) =
                inbound_by_file(&by_id, &in_adj, &symbols, &path);
            if wants("test") {
                push_file_items(entry, &mut budget, &mut dropped, "test", TEST_CAP, group_items(&by_id, tests, "reach this file"));
            }
            if wants("dependent") {
                push_file_items(entry, &mut budget, &mut dropped, "dependent", DEPENDENT_CAP, group_items(&by_id, dependents, "depend on this file"));
            }
        }

        // ── sibling ──────────────────────────────────────────────────────
        if wants("sibling") {
            let dir = Path::new(&path).parent().map(|p| p.to_string_lossy().to_string());
            let mut siblings: Vec<&GraphNode> = graph
                .nodes
                .iter()
                .filter(|n| is_file_level(n) && !matches!(n.node_type, GraphNodeType::Folder))
                .filter(|n| n.id != file_id)
                .filter(|n| {
                    n.file.as_deref().and_then(|f| {
                        Path::new(f).parent().map(|p| p.to_string_lossy().to_string())
                    }) == dir
                })
                .collect();
            siblings.sort_by(|a, b| a.name.cmp(&b.name));
            push_file_items(
                entry,
                &mut budget,
                &mut dropped,
                "sibling",
                SIBLING_CAP,
                siblings.into_iter().map(|n| (n, String::new(), 0, vec![])).collect(),
            );
        }
    }

    let dropped: Vec<DroppedRole> = FILE_CONTEXT_ROLES
        .iter()
        .filter_map(|role| {
            dropped
                .get(*role)
                .copied()
                .filter(|c| *c > 0)
                .map(|count| DroppedRole { role, count })
        })
        .collect();

    FileContextResult {
        files: entries,
        max_chars: effective_max,
        used_chars: budget.used(),
        dropped,
        notes,
        show_ids: true,
    }
}

/// What one outline row costs once rendered. See [`OUTLINE_ROW_CHROME`] for
/// why this is not [`item_cost`].
fn outline_row_cost(symbol: &SymbolRef) -> usize {
    OUTLINE_ROW_CHROME
        + symbol.name.len()
        + symbol.node_type.len()
        + symbol.id.len()
        + symbol
            .doc
            .as_deref()
            .map_or(0, |d| d.chars().take(OUTLINE_DOC_CHARS).count())
}

/// Spend the budget on one role's items, in order, reporting the remainder.
///
/// Breaks rather than continues on exhaustion: these roles are already sorted
/// by how much they matter, so skipping ahead to a cheaper item would reorder
/// the answer to save a few characters.
fn push_file_items(
    entry: &mut FileContextEntry,
    budget: &mut Budget,
    dropped: &mut HashMap<&'static str, usize>,
    role: &'static str,
    cap: usize,
    items: Vec<(&GraphNode, String, usize, Vec<String>)>,
) {
    let total = items.len();
    let mut shown = 0usize;
    for (node, why, symbols, examples) in items {
        let symbol = SymbolRef::from_node(node);
        let extra: usize = examples.iter().map(|e| e.len() + 4).sum();
        let cost = item_cost(&symbol, &why, extra);
        if shown >= cap || budget.left() < cost {
            break;
        }
        budget.take(cost);
        shown += 1;
        entry.items.push(FileContextItem { role, why, symbol, symbols, examples });
    }
    if shown < total {
        *dropped.entry(role).or_default() += total - shown;
    }
}

/// Files reaching into the subject, ranked: `(path, symbol count, up to two
/// example symbol names)`.
type FileHits = Vec<(String, usize, Vec<String>)>;

/// Walk inbound from every symbol the file declares and split what reaches it
/// into tests and non-test dependents, grouped by the file each user lives in.
///
/// Seeded with the file's *symbols*, never its File node: a File node's
/// `in_degree` counts only file-incident edges, so a function here being
/// called from ten other files contributes nothing to it. One hop from the
/// File node is the wrong answer to "what depends on this file", and looks
/// like the right one.
fn inbound_by_file<'g>(
    by_id: &HashMap<&'g str, &'g GraphNode>,
    in_adj: &HashMap<&'g str, Vec<&'g str>>,
    symbols: &[&'g GraphNode],
    own_path: &str,
) -> (FileHits, FileHits) {
    let mut depth: HashMap<&str, u32> = HashMap::new();
    let mut frontier: Vec<&str> = Vec::new();
    for s in symbols {
        if depth.insert(s.id.as_str(), 0).is_none() {
            frontier.push(s.id.as_str());
        }
    }

    for d in 1..=DEPENDENT_HOPS {
        let mut next: Vec<&str> = Vec::new();
        for cur in &frontier {
            for src in in_adj.get(*cur).into_iter().flatten() {
                if by_id.get(*src).is_some_and(|n| is_file_level(n)) {
                    continue;
                }
                if depth.contains_key(*src) {
                    continue;
                }
                depth.insert(src, d);
                next.push(src);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    // Sorted before grouping: a `HashMap` walk is in no particular order, and
    // the example names below would come out different on every run — the
    // reproducibility trap this repo has already been bitten by.
    let mut reached: Vec<(&str, u32)> = depth
        .into_iter()
        .filter(|(_, d)| *d > 0)
        .collect();
    reached.sort_unstable();

    let mut tests: HashMap<&str, (usize, Vec<String>)> = HashMap::new();
    let mut deps: HashMap<&str, (usize, Vec<String>)> = HashMap::new();
    for (id, d) in reached {
        let Some(n) = by_id.get(id) else { continue };
        let Some(f) = n.file.as_deref() else { continue };
        if f == own_path {
            continue;
        }
        // Tests are classified first, as in `context`: "what re-verifies this"
        // and "what breaks if I change it" are different questions, and one
        // node answering both would read as two dependents. A test further
        // away than `TEST_HOPS` is dropped rather than recorded as a
        // dependent — test code is not blast radius.
        let bucket = if crate::storage::facts::is_test_node(n) {
            if d > TEST_HOPS {
                continue;
            }
            &mut tests
        } else {
            &mut deps
        };
        let e = bucket.entry(f).or_insert((0, Vec::new()));
        e.0 += 1;
        if e.1.len() < 2 {
            e.1.push(n.name.clone());
        }
    }

    (rank_hits(tests), rank_hits(deps))
}

/// Heaviest first, then by path — so the same graph always renders the same
/// order.
fn rank_hits(hits: HashMap<&str, (usize, Vec<String>)>) -> FileHits {
    let mut out: FileHits = hits
        .into_iter()
        .map(|(f, (count, examples))| (f.to_string(), count, examples))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Turn ranked per-file hits into renderable items.
fn group_items<'g>(
    by_id: &HashMap<&'g str, &'g GraphNode>,
    hits: FileHits,
    verb: &str,
) -> Vec<(&'g GraphNode, String, usize, Vec<String>)> {
    hits.into_iter()
        .filter_map(|(path, count, examples)| {
            let id = format!("file:{}", path);
            // Looked up by id rather than by scanning for a matching `file`
            // property with a `File` node type: a config file is a `Config`
            // node, and a type filter would drop it from the answer.
            by_id.get(id.as_str()).copied().map(|n| {
                (
                    n,
                    format!("{} symbol(s) here {}", count, verb),
                    count,
                    examples,
                )
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Plural name and the one-clause gloss that says what a section is *for*.
///
/// The gloss is the feature, as the role label is in `context`: a reader who
/// knows that "dependents" means non-test files within three hops can trust
/// the number, and a reader who does not would have to guess.
fn role_heading(role: &'static str) -> (&'static str, &'static str) {
    match role {
        "outline" => ("outline", "what this file declares"),
        "importer" => ("importers", "who imports this file"),
        "import" => ("imports", "what this file reaches for"),
        "test" => ("tests", "what re-verifies this file"),
        "dependent" => (
            "dependents",
            "blast radius — non-test symbols only, within 3 hops",
        ),
        "sibling" => ("siblings", "same folder"),
        other => (other, ""),
    }
}

/// `4812 chars of 8000 budget · 26 outline, 1 importer · not shown: 6 sibling`
fn budget_line(r: &FileContextResult, style: Render) -> String {
    let counts: Vec<String> = FILE_CONTEXT_ROLES
        .iter()
        .filter_map(|role| {
            let n: usize = r.files.iter().map(|f| f.count(role)).sum();
            (n > 0).then(|| format!("{} {}", n, role))
        })
        .collect();
    style.dim(&format!(
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
    ))
}

/// `rust · context · 670 lines · 26 symbols · native/src/agent_tools`
fn facts_line(f: &FileFacts, style: Render) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(lang) = &f.language {
        parts.push(lang.clone());
    }
    if let Some(kind) = &f.classification {
        parts.push(kind.clone());
    }
    if f.is_test {
        parts.push("test".to_string());
    }
    match f.lines {
        Some(n) => parts.push(format!("{} lines", n)),
        // Absent rather than zero on a graph older than schema version 7 —
        // saying "0 lines" would be a measurement of nothing.
        None => parts.push("lines not indexed — run ug gen".to_string()),
    }
    parts.push(format!("{} symbol(s)", f.symbols));
    if let Some(folder) = &f.folder {
        parts.push(folder.clone());
    }
    style.dim(&parts.join(" · "))
}

pub fn render_file_context(r: &FileContextResult, style: Render) -> String {
    let mut out = String::new();
    let single = r.files.len() == 1;

    if !single {
        line(&mut out, &budget_line(r, style));
        for note in &r.notes {
            line(&mut out, &format!("⚠ {}", note));
        }
        out.push('\n');
    }

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
            &format!("{} {}", style.heading("Context for File"), style.bold(path)),
        );
        if let Some(id) = &f.id {
            line(&mut out, &format!("id: {}", style.id(id)));
        }
        if let Some(facts) = &f.facts {
            line(&mut out, &facts_line(facts, style));
        }
        if single {
            line(&mut out, &budget_line(r, style));
            for note in &r.notes {
                line(&mut out, &format!("⚠ {}", note));
            }
        }

        for role in FILE_CONTEXT_ROLES {
            let items: Vec<&FileContextItem> =
                f.items.iter().filter(|i| i.role == *role).collect();
            if items.is_empty() {
                continue;
            }
            let (plural, gloss) = role_heading(*role);
            let mut heading = format!("── {} ({}) ──", plural, items.len());
            if *role == "import" {
                if let Some(n) = f.facts.as_ref().map(|x| x.import_specifiers).filter(|n| *n > 0) {
                    heading = format!("── {} ({} of {} specifier(s)) ──", plural, items.len(), n);
                }
            }
            out.push('\n');
            line(&mut out, &style.bold(&heading));
            if !gloss.is_empty() {
                line(&mut out, &style.dim(gloss));
            }

            for item in items {
                if *role == "outline" {
                    render_outline_row(&mut out, item, r.show_ids, style);
                    continue;
                }
                render_file_item(&mut out, item, style);
            }
        }
    }

    // Nothing resolved, so there is no id to follow up on — the hints would
    // point at a report that does not exist.
    if r.files.iter().all(|f| f.error.is_some()) {
        return out;
    }

    next_actions_styled(
        &mut out,
        style,
        &[
            (style.id("get_code <id>"), "to read one symbol"),
            (
                style.cmd("context", "<id>"),
                "for one symbol's own neighbourhood",
            ),
            (
                style.cmd("analyze impact", "--arg target=<path>"),
                "for the blast radius with tests included",
            ),
        ],
    );
    out
}

/// `- File src/api.rs  935 lines` + its id, relationship and examples.
///
/// Not the shared [`SymbolRef::render_bullet`]: a File node's name *is* its
/// path, so that renderer prints the path twice — once as the name and again
/// as the location.
fn render_file_item(out: &mut String, item: &FileContextItem, style: Render) {
    let s = &item.symbol;
    let span = match (s.start_line, s.end_line) {
        (Some(a), Some(b)) if b >= a => {
            format!("  {}", style.dim(&format!("{} lines", b - a + 1)))
        }
        // A Dependency node is a package, not a file, and has no span.
        _ => String::new(),
    };
    line(out, &format!("- {} {}{}", s.node_type, style.bold(&s.name), span));
    line(out, &format!("  id: {}", style.id(&s.id)));
    if !item.why.is_empty() {
        line(out, &format!("  {}", style.dim(&item.why)));
    }
    if !item.examples.is_empty() {
        line(out, &format!("    {}", style.dim(&item.examples.join(" · "))));
    }
}

/// `- L12-34  Function  name — the first clause of its doc  id: …`
fn render_outline_row(out: &mut String, item: &FileContextItem, show_ids: bool, style: Render) {
    let s = &item.symbol;
    let start = s.start_line.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    let end = s.end_line.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    let mut row = format!(
        "- L{}-{}  {}  {}",
        start,
        end,
        s.node_type,
        style.bold(&s.name)
    );
    // One clause of prose is what turns a table of contents into a summary —
    // the whole reason this role exists rather than the bare outline it
    // replaces.
    if let Some(doc) = s.doc.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        let clause: String = doc.chars().take(OUTLINE_DOC_CHARS).collect();
        let clause = clause.trim_end();
        let ellipsis = if doc.chars().count() > OUTLINE_DOC_CHARS { "…" } else { "" };
        row.push_str(&style.dim(&format!(" — {}{}", clause, ellipsis)));
    }
    // The id re-encodes `kind:path:name`, all of which the heading (path) and
    // this row (kind, name) already show — so it is noise when piped.
    if show_ids {
        row.push_str(&format!("  id: {}", style.id(&s.id)));
    }
    line(out, &row);
}
