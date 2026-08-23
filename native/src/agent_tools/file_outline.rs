//! `file_outline` — agent tool.

use super::*;

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
