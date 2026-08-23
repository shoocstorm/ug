//! `graph_schema` — agent tool.

use super::*;

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
