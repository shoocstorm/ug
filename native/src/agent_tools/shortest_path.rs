//! `shortest_path` — agent tool.

use super::*;

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
/// Edges are directed; unless `strict`, the reverse direction is retried
/// when no forward path exists and the result is flagged `reversed`.
///
/// Runs straight on the parsed graph. This used to take the graph.json
/// *text* as well and hand it to the lib's shortest path, which cloned the
/// whole string, re-parsed it into a second `GraphData`, rebuilt the
/// adjacency, serialised its answer to JSON — and then this function parsed
/// that back. The `!found` retry below did the entire thing a second time.
/// All of it to answer a question about the `graph` already in hand.
pub fn shortest_path(
    graph: &GraphData,
    source: &str,
    target: &str,
    strict: bool,
) -> ShortestPathResult {
    let mut reversed = false;
    let mut result = crate::find_shortest_path(graph, source, target);
    if !result.found && !strict {
        reversed = true;
        result = crate::find_shortest_path(graph, target, source);
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
