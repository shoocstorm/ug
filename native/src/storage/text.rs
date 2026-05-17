//! Text shaping for node embeddings.
//!
//! The format follows the spec in docs/GRAPH-STORAGE.md:
//! `"{type}: {name}. {description}. Related: {list_of_related_names}"`
//!
//! `description` falls back to the docstring; `related` is the union of
//! neighbour node names reachable via any edge (in either direction). We
//! cap related names so a hub node like `index.ts` doesn't blow the
//! embedding context.
//!
//! Folder nodes carry no docstring at index time. Pre-enrichment we
//! synthesize a description from the folder's classification and language
//! breakdown so the embedding still has retrieval signal; once the
//! Semantic Enrichment phase fills `folder.summary` we prefer that.

use crate::types::{GraphData, GraphNode, GraphNodeFolderMeta, GraphNodeType};
use std::collections::HashMap;

/// Cap on related-name fan-out per node. Embedding context is bounded; a
/// hub node with thousands of edges would otherwise dominate every query.
const MAX_RELATED: usize = 24;

pub fn build_node_text(node: &GraphNode, related_names: &[String]) -> String {
    let kind = format!("{:?}", node.node_type);

    // For folders, prefer the full path over the basename so the embedding
    // text disambiguates same-named folders (`tests/components` vs
    // `src/components`). Other node types already encode location elsewhere.
    let name = match (&node.node_type, node.folder.as_ref()) {
        (GraphNodeType::Folder, Some(_)) => folder_path_from_id(&node.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| node.name.clone()),
        _ => node.name.clone(),
    };

    let description = node_description(node);

    let related = if related_names.is_empty() {
        String::new()
    } else {
        related_names.join(", ")
    };

    format!("{}: {}. {}. Related: {}", kind, name, description, related)
}

/// Per-node description used inside the embedding text. Falls through:
/// 1. `folder.summary` for folder nodes once enrichment fills it
/// 2. `docstring` for any node that has one
/// 3. synthesized folder synopsis from classification + breakdown + counts
/// 4. empty string for everything else (matches old behaviour)
fn node_description(node: &GraphNode) -> String {
    if let Some(meta) = &node.folder {
        if let Some(summary) = meta.summary.as_ref() {
            let trimmed = summary.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    if let Some(doc) = &node.docstring {
        let trimmed = doc.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if matches!(node.node_type, GraphNodeType::Folder) {
        if let Some(meta) = &node.folder {
            return synthesize_folder_synopsis(meta);
        }
    }

    String::new()
}

/// Build a one-line description from a folder's structural metadata. Used
/// pre-enrichment so the folder node still carries retrieval signal.
/// Example output: "components folder, 8 typescript and 2 markdown files
/// (depth 2)".
fn synthesize_folder_synopsis(meta: &GraphNodeFolderMeta) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(class) = meta.classification.as_ref() {
        parts.push(format!("{} folder", classification_label(class)));
    } else if meta.depth == 0 {
        parts.push("project root".to_string());
    } else {
        parts.push("folder".to_string());
    }

    if meta.total_files > 0 {
        parts.push(format_breakdown(meta));
    }

    parts.push(format!("depth {}", meta.depth));

    parts.join(", ")
}

fn classification_label(class: &crate::types::FolderClassification) -> &'static str {
    use crate::types::FolderClassification::*;
    match class {
        Source => "source",
        Tests => "tests",
        Documentation => "documentation",
        Examples => "examples",
        Config => "config",
        Assets => "assets",
        Components => "components",
        Pages => "pages",
        Hooks => "hooks",
        Services => "services",
        Contexts => "contexts",
        Reducers => "reducers",
        Utils => "utils",
        Types => "types",
        Mixed => "mixed",
    }
}

/// Format the language breakdown like "8 typescript and 2 markdown files".
/// When the breakdown is empty (extension we don't recognise), falls back to
/// just the file count.
fn format_breakdown(meta: &GraphNodeFolderMeta) -> String {
    if meta.language_breakdown.is_empty() {
        return format!("{} files", meta.total_files);
    }
    let mut entries: Vec<(&String, &u32)> = meta.language_breakdown.iter().collect();
    // Largest-first so the dominant language leads. Stable on ties via name.
    entries.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let labelled: Vec<String> = entries
        .iter()
        .map(|(lang, count)| format!("{} {}", count, lang))
        .collect();
    format!("{} files", labelled.join(" and "))
}

/// Strip the `folder:` prefix from a folder node ID. Returns `None` if the ID
/// doesn't carry that prefix - shouldn't happen for a Folder node, but the
/// caller falls back to the basename in that case.
fn folder_path_from_id(id: &str) -> Option<&str> {
    id.strip_prefix("folder:")
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

/// Build a sparse keyword vector for OverGraph's hybrid search. The
/// dimension space is the FNV-1a hash of each lowercase alphanumeric
/// token; the weight is term frequency. The same hash + tokenizer are
/// used at ingest and at query time so tokens collide deterministically.
///
/// This is the v1 FTS replacement (MIGRATION-OVERGRAPH §3.3 Option 1).
/// It has no IDF — common words count as much as rare ones — but for
/// code symbol queries (mostly distinctive identifiers) it gives
/// roughly BM25-equivalent recall with zero extra dependencies.
pub fn build_sparse_keyword_vector(text: &str) -> Vec<(u32, f32)> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut weights: HashMap<u32, f32> = HashMap::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for c in ch.to_lowercase() {
                buf.push(c);
            }
        } else if !buf.is_empty() {
            emit_token(&buf, &mut weights);
            buf.clear();
        }
    }
    if !buf.is_empty() {
        emit_token(&buf, &mut weights);
    }
    // Sort by dimension id so output is deterministic and matches the
    // canonical form OverGraph expects for sparse vectors.
    let mut out: Vec<(u32, f32)> = weights.into_iter().collect();
    out.sort_unstable_by_key(|&(dim, _)| dim);
    out
}

fn emit_token(token: &str, out: &mut HashMap<u32, f32>) {
    // 2-char minimum filters out one-letter accidental tokens from
    // ident splits. 32-char cap stops pathological URLs / hashes from
    // dominating the vector.
    let len = token.len();
    if !(2..=32).contains(&len) {
        return;
    }
    let dim = fnv1a_u32(token.as_bytes());
    *out.entry(dim).or_insert(0.0) += 1.0;
}

/// Reciprocal Rank Fusion of two ranked lists. The standard RRF
/// constant `c = 60` (Cormack et al., 2009) — small enough to keep
/// rank-1 hits dominant, large enough that ties don't collapse to zero.
/// Output is sorted by fused score descending; output length is
/// capped at `k`. Both backends use this for hybrid search:
/// OverGraph's native fusion plus Neo4j's app-side fusion of vector +
/// full-text results.
pub fn reciprocal_rank_fusion(
    left: Vec<(crate::storage::db::NodeRow, f32)>,
    right: Vec<(crate::storage::db::NodeRow, f32)>,
    k: usize,
) -> Vec<(crate::storage::db::NodeRow, f32)> {
    const C: f32 = 60.0;
    let mut scored: HashMap<String, (crate::storage::db::NodeRow, f32)> = HashMap::new();
    for (rank, (row, _)) in left.into_iter().enumerate() {
        let s = 1.0 / (C + rank as f32 + 1.0);
        let id = row.id.clone();
        scored.entry(id).or_insert((row, 0.0)).1 += s;
    }
    for (rank, (row, _)) in right.into_iter().enumerate() {
        let s = 1.0 / (C + rank as f32 + 1.0);
        let id = row.id.clone();
        scored.entry(id).or_insert((row, 0.0)).1 += s;
    }
    let mut out: Vec<(crate::storage::db::NodeRow, f32)> = scored.into_values().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    out
}

/// FNV-1a 32-bit hash. Mirrors the algorithm OverGraph uses internally
/// (its `fnv1a` is 64-bit but the principle is identical) so test
/// fixtures can predict dimension ids.
fn fnv1a_u32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod sparse_tests {
    use super::*;

    #[test]
    fn tokenizer_lowercases_and_splits() {
        let v = build_sparse_keyword_vector("Hello, World! Hello.");
        // "hello" twice, "world" once → 2 distinct dimensions
        assert_eq!(v.len(), 2);
        let total: f32 = v.iter().map(|(_, w)| *w).sum();
        assert_eq!(total, 3.0);
    }

    #[test]
    fn empty_text_yields_empty_vector() {
        assert!(build_sparse_keyword_vector("").is_empty());
        assert!(build_sparse_keyword_vector("!!!").is_empty());
    }

    #[test]
    fn deterministic_dim_ids() {
        let a = build_sparse_keyword_vector("foo bar");
        let b = build_sparse_keyword_vector("foo bar");
        assert_eq!(a, b);
    }
}
