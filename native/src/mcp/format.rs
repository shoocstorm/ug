//! Markdown formatters for the DB-backed MCP tools (`search`,
//! `semantic_search`). Ported verbatim in behaviour from the old
//! `node/cli.mjs` `formatRankedContext` / `formatSemanticHits` so MCP output
//! is byte-for-byte what agents were already trained against.
//!
//! The graph-backed tools don't need anything here — they render themselves
//! inside [`crate::agent_tools`] via `Render::Markdown`.

use ultragraph::storage::query::{RankedContext, SearchHit};

/// Long snippets blow up the prompt. Cap each item but indicate truncation so
/// the agent knows it can re-fetch the full slice via `get_code`.
pub(crate) const SNIPPET_PREVIEW_CHARS: usize = 1200;

struct SnippetPreview {
    text: String,
    truncated: bool,
    omitted: usize,
}

fn preview_snippet(snippet: &str) -> Option<SnippetPreview> {
    let trimmed = snippet.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    // Count by chars (not bytes) to match the JS `.length`/`.slice` semantics
    // and never split a multi-byte char.
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= SNIPPET_PREVIEW_CHARS {
        Some(SnippetPreview {
            text: trimmed.to_string(),
            truncated: false,
            omitted: 0,
        })
    } else {
        Some(SnippetPreview {
            text: chars[..SNIPPET_PREVIEW_CHARS].iter().collect(),
            truncated: true,
            omitted: chars.len() - SNIPPET_PREVIEW_CHARS,
        })
    }
}

/// `Type×N, Type×M` tally, most frequent first, insertion order breaking ties
/// — mirrors the JS `Map` iteration order.
fn summarize_node_types<'a, I: IntoIterator<Item = &'a str>>(types: I) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for t in types {
        if let Some(entry) = counts.iter_mut().find(|(name, _)| name == t) {
            entry.1 += 1;
        } else {
            counts.push((t.to_string(), 1));
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    counts
        .iter()
        .map(|(t, n)| format!("{}×{}", t, n))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn format_ranked_context(ctx: &RankedContext) -> String {
    let mut lines: Vec<String> = Vec::new();
    let items = &ctx.items;

    lines.push(format!("# Knowledge-base results for: {}", ctx.query));
    let mut meta = vec![
        format!("items={}", items.len()),
        format!("chars={}", ctx.total_chars),
    ];
    if let Some(seed) = &ctx.seed_id {
        meta.push(format!("seed={}", seed));
    }
    if !items.is_empty() {
        meta.push(format!(
            "types=[{}]",
            summarize_node_types(items.iter().map(|i| i.node_type.as_str()))
        ));
    }
    lines.push(meta.join("  •  "));
    lines.push(String::new());

    if items.is_empty() {
        lines.push("No matches. Try:".to_string());
        lines.push("- a broader query (drop qualifiers)".to_string());
        lines.push(
            "- semantic_search for a pure-vector pass with whereClause filters".to_string(),
        );
        lines.push("- ping_embedder to confirm the embedding endpoint is up".to_string());
        return lines.join("\n");
    }

    for (idx, it) in items.iter().enumerate() {
        let loc = if it.file.is_empty() {
            "(no file)".to_string()
        } else {
            format!("{}:{}-{}", it.file, it.start_line, it.end_line)
        };
        let score = format!("{:.3}", it.distance);
        lines.push(format!("## [{}] {} {}", idx + 1, it.node_type, it.name));
        lines.push(format!("- id: `{}`", it.id));
        lines.push(format!("- loc: {}", loc));
        lines.push(format!("- hop={}  •  score={}", it.hop, score));
        if !it.description.is_empty() {
            lines.push(format!("- desc: {}", it.description));
        }
        if let Some(snip) = it.snippet.as_deref().and_then(preview_snippet) {
            lines.push("```".to_string());
            lines.push(snip.text);
            lines.push("```".to_string());
            if snip.truncated {
                lines.push(format!(
                    "(snippet truncated — {} more chars; call get_code with id `{}` for the full source)",
                    snip.omitted, it.id
                ));
            }
        }
        lines.push(String::new());
    }

    let top_id = &items[0].id;
    lines.push("---".to_string());
    lines.push("Drill-down hints:".to_string());
    lines.push(format!(
        "- Walk neighbors:  traverse({{ nodeId: \"{}\", hops: 1 }})",
        top_id
    ));
    lines.push(format!(
        "- Find callers:    find_usages({{ nodeId: \"{}\" }})",
        top_id
    ));
    lines.push(
        "- Narrow search:   search({ query: \"...\", whereClause: \"node_type = 'Function'\" })"
            .to_string(),
    );
    lines.push(
        "- Read full file:  use the loc above (file:start-end) with your file-read tool"
            .to_string(),
    );

    lines.join("\n")
}

pub fn format_semantic_hits(query: &str, hits: &[SearchHit]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# Semantic search for: {}", query));
    let mut meta = vec![format!("hits={}", hits.len())];
    if !hits.is_empty() {
        meta.push(format!(
            "types=[{}]",
            summarize_node_types(hits.iter().map(|h| h.node.node_type.as_str()))
        ));
    }
    lines.push(meta.join("  •  "));
    lines.push(String::new());

    if hits.is_empty() {
        lines.push(
            "No matches. Loosen the whereClause or try search for graph-aware ranking.".to_string(),
        );
        return lines.join("\n");
    }

    for (idx, h) in hits.iter().enumerate() {
        let n = &h.node;
        let loc = if n.file.is_empty() {
            "(no file)".to_string()
        } else {
            format!("{}:{}-{}", n.file, n.start_line, n.end_line)
        };
        let score = format!("{:.3}", h.distance);
        lines.push(format!(
            "[{}] {} {}  •  id=`{}`  •  dist={}",
            idx + 1,
            n.node_type,
            n.name,
            n.id,
            score
        ));
        lines.push(format!("    {}", loc));
        if !n.description.is_empty() {
            lines.push(format!("    {}", n.description));
        }
    }

    lines.push(String::new());
    lines.push(format!(
        "Next: search({{ query: \"{}\" }}) for graph-ranked snippets, or traverse({{ nodeId: \"{}\" }}) to expand.",
        query, hits[0].node.id
    ));
    lines.join("\n")
}

/// Info row for `list_projects`. Kept here next to the formatter that consumes
/// it so the two stay in sync.
pub struct ProjectInfo {
    pub name: String,
    pub repo_root: String,
    pub nodes: Option<usize>,
    pub edges: Option<usize>,
}

pub fn format_project_list(projects: &[ProjectInfo], current_repo_root: &str, ug_home: &str) -> String {
    if projects.is_empty() {
        return format!(
            "No indexed projects under {} — run `ug gen` in a repo first.",
            ug_home
        );
    }
    let mut lines = vec![
        format!("# Indexed projects ({})", projects.len()),
        String::new(),
    ];
    for p in projects {
        let here = if p.repo_root == current_repo_root {
            "  ← current"
        } else {
            ""
        };
        let nodes = p.nodes.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
        let edges = p.edges.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
        lines.push(format!(
            "- **{}**  {}  ({} nodes, {} edges){}",
            p.name, p.repo_root, nodes, edges, here
        ));
    }
    lines.push(String::new());
    lines.push(
        "Pass project: '<name>' to any tool to query that project instead of the current one."
            .to_string(),
    );
    lines.join("\n")
}
