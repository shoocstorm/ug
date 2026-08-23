//! `project_overview` — agent tool.

use super::*;

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

pub(crate) fn top_counts<K: ToString + Copy>(m: &HashMap<K, usize>, k: usize) -> Vec<TypeCount> {
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
