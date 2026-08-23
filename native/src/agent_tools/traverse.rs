//! `traverse` — agent tool.

use super::*;

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
