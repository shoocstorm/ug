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
    /// Edges the caller's `edge_types` filter removed, that the walk would
    /// otherwise have followed, tallied by type.
    ///
    /// A filter narrows silently: the edges it drops never reach the result,
    /// so a filtered walk and a complete one are indistinguishable in the
    /// output. That is how `edge_types: ["calls"]` reports a function as
    /// having no dependency on one it passes as a value — Rust records that
    /// as `references`, and the answer still looks whole. Counting what was
    /// hidden is the only way a caller can tell the difference.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hidden_by_edge_type: BTreeMap<String, usize>,
    /// Edges pointing the other way from an expanded node, tallied by type.
    /// Empty when `direction` is `Both`, since then nothing is hidden.
    ///
    /// The same blind spot as above, one axis over: `outbound` answers "what
    /// does this reach" and says nothing about what reaches it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hidden_by_direction: BTreeMap<String, usize>,
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
    let edge_filter: Vec<String> = normalize_edge_filter(&p.edge_types);

    let by_id = by_id_map(graph);

    // Adjacency built once, honouring the edge-type filter.
    let mut out_adj: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    let mut in_adj: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    for e in &graph.edges {
        let et = edge_type_str(&e.edge_type);
        // `eq_ignore_ascii_case` against the static name: `et` is a
        // `&'static str` and the filter is already lowercased, so the
        // `to_lowercase()` here allocated a `String` per edge purely to
        // compare it. See P11.12 in docs/dev/PERF-TUNING-JOURNEY.md.
        if !edge_filter.is_empty() && !edge_filter.iter().any(|t| t.eq_ignore_ascii_case(et)) {
            continue;
        }
        out_adj
            .entry(&*e.source)
            .or_default()
            .push((&*e.target, et));
        in_adj
            .entry(&*e.target)
            .or_default()
            .push((&*e.source, et));
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

    // What the knobs hid.
    //
    // A second pass over the edge list, rather than a parallel "suppressed"
    // adjacency built alongside the real one: the walk has to finish before
    // anything can be said about which dropped edges were even relevant, and
    // holding every dropped edge until then is what would cost memory — on a
    // large repo `edge_types: ["calls"]` drops most of three quarters of a
    // million edges. This pass allocates nothing per edge and runs only when
    // a knob was actually set, so an unfiltered `both` walk pays for none of
    // it.
    let mut hidden_by_edge_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut hidden_by_direction: BTreeMap<String, usize> = BTreeMap::new();
    let filtering = !edge_filter.is_empty();
    let directional = direction != Dir::Both;
    if filtering || directional {
        // A node at the hop limit is never expanded, so edges leaving it were
        // not hidden by a knob — the walk had already stopped. Counting them
        // would report the hop bound as if it were a filter.
        let expanded = |id: &str| distances.get(id).is_some_and(|d| *d < hops);
        for e in &graph.edges {
            let et = edge_type_str(&e.edge_type);
            let passes = !filtering || edge_filter.iter().any(|t| t.eq_ignore_ascii_case(et));
            let out_reach = matches!(direction, Dir::Outbound | Dir::Both) && expanded(&e.source);
            let in_reach = matches!(direction, Dir::Inbound | Dir::Both) && expanded(&e.target);

            if !passes {
                if out_reach || in_reach {
                    *hidden_by_edge_type.entry(et.to_string()).or_insert(0) += 1;
                }
                // Already accounted for; counting it under direction too
                // would report one edge as two separate omissions.
                continue;
            }
            // Passed the filter and still absent: the walk only looks one
            // way, and this edge points the other.
            if directional && !out_reach && !in_reach {
                let other_way = match direction {
                    Dir::Outbound => expanded(&e.target),
                    Dir::Inbound => expanded(&e.source),
                    Dir::Both => false,
                };
                if other_way {
                    *hidden_by_direction.entry(et.to_string()).or_insert(0) += 1;
                }
            }
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
        hidden_by_edge_type,
        hidden_by_direction,
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

    // What the knobs hid, said out loud. An edge-type filter that quietly
    // drops a real dependency is worse than no filter at all, because the
    // result still looks complete — see `TraverseResult::hidden_by_edge_type`.
    if !r.hidden_by_edge_type.is_empty() {
        line(
            &mut out,
            &style.dim(&format!(
                "{} hidden by edge-type filter: {}  ·  drop it to see all",
                edge_count(&r.hidden_by_edge_type),
                tally_str(&r.hidden_by_edge_type)
            )),
        );
    }
    if !r.hidden_by_direction.is_empty() {
        line(
            &mut out,
            &style.dim(&format!(
                "{} hidden by dir={:?}: {}  ·  --direction both to see both ways",
                edge_count(&r.hidden_by_direction),
                r.direction,
                tally_str(&r.hidden_by_direction)
            )),
        );
    }

    next_actions_styled(&mut out, style, &traverse_next_actions(r, style));
    out
}

fn edge_count(tally: &BTreeMap<String, usize>) -> String {
    let n: usize = tally.values().sum();
    format!("{} edge{}", n, if n == 1 { "" } else { "s" })
}

fn tally_str(tally: &BTreeMap<String, usize>) -> String {
    tally
        .iter()
        .map(|(t, c)| format!("{}×{}", t, c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What to suggest *next*, given what this walk actually did.
///
/// A tool description is read once, cold, before the agent has a task in
/// hand; the previous result is the last thing it saw. So the reliable place
/// to say "another tool answers this better" is here, in the output, with the
/// arguments already filled in — a command that can be run beats the name of
/// one that would have to be assembled.
fn traverse_next_actions(r: &TraverseResult, style: Render) -> Vec<(String, &'static str)> {
    let seed = r.seeds.first().map(String::as_str).unwrap_or("<id>");
    let quoted = format!("'{}'", seed);
    let mut hints: Vec<(String, &'static str)> = Vec::new();

    // An empty walk is empty for every possible reason at once, and the
    // caller cannot tell which from the result. The knobs are the likeliest.
    if r.nodes.len() <= r.seeds.len() {
        if !r.hidden_by_edge_type.is_empty() {
            hints.push((
                style.cmd("traverse", &quoted),
                "— nothing matched the edge-type filter; this is the same walk unfiltered",
            ));
        } else if !r.seeds.is_empty() {
            hints.push((
                style.cmd("find_symbols", &format!("'{}*'", seed)),
                "— the seed resolved but has no edges; check the name is the one you meant",
            ));
        }
        return hints;
    }

    // `find_usages` is this walk pinned inbound, plus call-site lines and a
    // default edge set wide enough that constants and types aren't silent.
    if r.direction == Dir::Inbound {
        hints.push((
            style.cmd("find_usages", &quoted),
            "— the same inbound walk, with call-site lines as evidence",
        ));
    }

    // One seed, one hop, outbound: `context` answers the question this was
    // probably standing in for, in one call.
    if r.hops == 1 && r.seeds.len() == 1 && r.direction == Dir::Outbound {
        hints.push((
            style.cmd("context", &quoted),
            "— code, callers, tests and deps for this symbol in one budgeted call",
        ));
    }

    if r.nodes.len() > 200 {
        hints.push((
            style.cmd("traverse", &format!("{} -k 1", quoted)),
            "— this neighbourhood is large; narrow it, or rank it with graph_centrality",
        ));
    }

    hints.push((style.cmd("get_code", "<id>"), "to read any node above"));
    if r.direction != Dir::Inbound {
        hints.push((
            style.cmd("find_usages", "<id>"),
            "for the inbound direction",
        ));
    }
    hints
}
