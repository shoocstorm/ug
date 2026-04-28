//! Text shaping for node embeddings.
//!
//! The format follows the spec in docs/GRAPH-STORAGE.md:
//! `"{type}: {name}. {description}. Related: {list_of_related_names}"`
//!
//! `description` falls back to the docstring; `related` is the union of
//! neighbour node names reachable via any edge (in either direction). We
//! cap related names so a hub node like `index.ts` doesn't blow the
//! embedding context.

use crate::types::{GraphData, GraphNode};
use std::collections::HashMap;

/// Cap on related-name fan-out per node. Embedding context is bounded; a
/// hub node with thousands of edges would otherwise dominate every query.
const MAX_RELATED: usize = 24;

pub fn build_node_text(node: &GraphNode, related_names: &[String]) -> String {
    let kind = format!("{:?}", node.node_type);
    let description = node
        .docstring
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();

    let related = if related_names.is_empty() {
        String::new()
    } else {
        related_names.join(", ")
    };

    format!(
        "{}: {}. {}. Related: {}",
        kind, node.name, description, related
    )
}

/// Build a `node_id -> [neighbour names]` map by walking every edge in
/// `graph`. Both endpoints of an edge contribute to each other so the
/// embedded text reflects bidirectional context.
pub fn collect_related_names(graph: &GraphData) -> HashMap<String, Vec<String>> {
    let id_to_name: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect();

    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for edge in &graph.edges {
        if let Some(target_name) = id_to_name.get(edge.target.as_str()) {
            out.entry(edge.source.clone())
                .or_default()
                .push(target_name.to_string());
        }
        if let Some(source_name) = id_to_name.get(edge.source.as_str()) {
            out.entry(edge.target.clone())
                .or_default()
                .push(source_name.to_string());
        }
    }

    for v in out.values_mut() {
        v.sort();
        v.dedup();
        if v.len() > MAX_RELATED {
            v.truncate(MAX_RELATED);
        }
    }

    out
}
