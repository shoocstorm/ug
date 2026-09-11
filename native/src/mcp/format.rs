//! Markdown formatters for the DB-backed MCP tools (`search`, and the
//! store-backed project listing). Ported in behaviour from the old
//! `node/cli.mjs` `formatRankedContext` so MCP output stays what agents
//! were already trained against.
//!
//! Both halves of `search` render through [`format_ranked_context`]: the
//! expanded pass and the `expand: false` pass differ by one line per item,
//! not by output format.
//!
//! The graph-backed tools don't need anything here — they render themselves
//! inside [`crate::agent_tools`] via `Render::Markdown`.

use ultragraph::storage::query::RankedContext;

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

/// Render a search result. `expanded` says whether graph expansion ran:
/// when it didn't, every item is a seed, so the per-item `hop` line is a
/// constant 0 and gets dropped rather than printed as noise.
pub fn format_ranked_context(ctx: &RankedContext, expanded: bool) -> String {
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
        if expanded {
            lines.push("- a whereClause to filter rather than a longer query".to_string());
        } else {
            lines.push("- expand: true, so graph neighbors of a near-miss still land".to_string());
        }
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
        if expanded {
            lines.push(format!("- hop={}  •  score={}", it.hop, score));
        } else {
            lines.push(format!("- score={}  •  via={}", score, it.matched_by));
        }
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

#[cfg(test)]
mod tests {
    //! What an agent actually reads back from `search`.
    //!
    //! These formatters are the MCP surface: an agent never sees a
    //! `RankedContext`, it sees this markdown. Three things in it are load
    //! bearing rather than cosmetic — the `[N]` numbers the agent cites, the
    //! ids it feeds to the next tool call, and the truncation notice that
    //! tells it a snippet is incomplete. Dropping any of them still produces
    //! plausible output, and the agent answers from a partial file without
    //! knowing it.

    use super::*;
    use ultragraph::storage::ContextItem;

    fn item(name: &str, node_type: &str) -> ContextItem {
        ContextItem {
            id: format!("function:src/{name}.rs:{name}"),
            name: name.into(),
            node_type: node_type.into(),
            file: format!("src/{name}.rs"),
            start_line: 10,
            end_line: 42,
            description: String::new(),
            distance: 0.25,
            hop: 0,
            snippet: None,
            matched_by: "semantic".into(),
        }
    }

    fn ctx(items: Vec<ContextItem>) -> RankedContext {
        RankedContext {
            query: "how does auth work".into(),
            total_chars: 1234,
            seed_id: items.first().map(|i| i.id.clone()),
            items,
        }
    }

    // ── snippet previews ────────────────────────────────────────────────────

    #[test]
    fn an_empty_snippet_is_no_snippet() {
        assert!(preview_snippet("").is_none());
        assert!(preview_snippet("   \n\t ").is_none(), "whitespace is not content");
    }

    #[test]
    fn a_short_snippet_passes_through_untruncated() {
        let p = preview_snippet("fn main() {}").expect("some");
        assert_eq!(p.text, "fn main() {}");
        assert!(!p.truncated);
        assert_eq!(p.omitted, 0);
    }

    #[test]
    fn a_snippet_at_the_cap_is_not_truncated() {
        let s = "x".repeat(SNIPPET_PREVIEW_CHARS);
        let p = preview_snippet(&s).expect("some");
        assert!(!p.truncated, "the cap is inclusive");
        assert_eq!(p.text.chars().count(), SNIPPET_PREVIEW_CHARS);
    }

    #[test]
    fn one_char_past_the_cap_truncates_and_counts_what_is_missing() {
        let s = "x".repeat(SNIPPET_PREVIEW_CHARS + 1);
        let p = preview_snippet(&s).expect("some");
        assert!(p.truncated);
        assert_eq!(p.text.chars().count(), SNIPPET_PREVIEW_CHARS);
        assert_eq!(p.omitted, 1, "the count is what to tell the agent it is missing");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // A byte slice here would split a multi-byte char and produce invalid
        // UTF-8, or panic. Source comments carry accents and CJK routinely.
        let s = "é".repeat(SNIPPET_PREVIEW_CHARS + 10);
        let p = preview_snippet(&s).expect("some");
        assert_eq!(p.text.chars().count(), SNIPPET_PREVIEW_CHARS);
        assert_eq!(p.omitted, 10);
        assert!(p.text.chars().all(|c| c == 'é'), "no mangled char at the cut");
    }

    // ── the type tally ──────────────────────────────────────────────────────

    #[test]
    fn node_types_are_tallied_most_frequent_first() {
        let out = summarize_node_types(["Function", "Class", "Function", "Function", "Class"]);
        assert_eq!(out, "Function×3, Class×2");
    }

    #[test]
    fn a_tie_keeps_the_order_the_types_first_appeared_in() {
        // Deliberate: it mirrors the JS Map iteration this was ported from,
        // and a stable order is what keeps the output diffable.
        assert_eq!(summarize_node_types(["Class", "Function"]), "Class×1, Function×1");
        assert_eq!(summarize_node_types(["Function", "Class"]), "Function×1, Class×1");
    }

    #[test]
    fn no_types_tally_to_nothing() {
        assert_eq!(summarize_node_types(Vec::<&str>::new()), "");
    }

    // ── the search result an agent reads ────────────────────────────────────

    #[test]
    fn results_are_numbered_from_one_for_citation() {
        let out = format_ranked_context(&ctx(vec![item("alpha", "Function"), item("bravo", "Class")]), true);
        assert!(out.contains("## [1] Function alpha"), "{out}");
        assert!(out.contains("## [2] Class bravo"), "{out}");
    }

    #[test]
    fn every_item_carries_the_id_the_next_tool_call_needs() {
        // The whole point of the format: an agent reads a hit here and feeds
        // its id straight to get_code / find_usages / traverse.
        let out = format_ranked_context(&ctx(vec![item("alpha", "Function")]), true);
        assert!(out.contains("- id: `function:src/alpha.rs:alpha`"), "{out}");
        assert!(out.contains("- loc: src/alpha.rs:10-42"), "{out}");
    }

    #[test]
    fn an_item_with_no_file_says_so_rather_than_printing_a_bare_range() {
        let mut it = item("alpha", "Concept");
        it.file = String::new();
        let out = format_ranked_context(&ctx(vec![it]), true);
        assert!(out.contains("- loc: (no file)"), "{out}");
        assert!(!out.contains(":10-42"), "a range with no file is meaningless: {out}");
    }

    #[test]
    fn an_unexpanded_search_reports_provenance_instead_of_a_constant_hop() {
        // With no graph expansion every item is a seed, so `hop` is always 0
        // and printing it is noise. What is worth the line instead is which
        // channel found the hit.
        let items = vec![item("alpha", "Function")];
        let expanded = format_ranked_context(&ctx(items.clone()), true);
        let seeds_only = format_ranked_context(&ctx(items), false);

        assert!(expanded.contains("- hop=0"), "{expanded}");
        assert!(!seeds_only.contains("hop="), "{seeds_only}");
        assert!(seeds_only.contains("via=semantic"), "{seeds_only}");
    }

    #[test]
    fn a_truncated_snippet_tells_the_agent_how_to_get_the_rest() {
        let mut it = item("alpha", "Function");
        it.snippet = Some("x".repeat(SNIPPET_PREVIEW_CHARS + 500));
        let out = format_ranked_context(&ctx(vec![it]), true);

        // Without this the agent answers from a partial function body and has
        // no way to know it was partial.
        assert!(out.contains("snippet truncated — 500 more chars"), "{out}");
        assert!(out.contains("get_code with id `function:src/alpha.rs:alpha`"), "{out}");
    }

    #[test]
    fn a_short_snippet_is_fenced_with_no_truncation_notice() {
        let mut it = item("alpha", "Function");
        it.snippet = Some("fn alpha() {}".into());
        let out = format_ranked_context(&ctx(vec![it]), true);
        assert!(out.contains("```\nfn alpha() {}\n```"), "{out}");
        assert!(!out.contains("truncated"), "{out}");
    }

    #[test]
    fn the_header_summarises_what_came_back() {
        let out = format_ranked_context(&ctx(vec![item("a", "Function"), item("b", "Function")]), true);
        assert!(out.starts_with("# Knowledge-base results for: how does auth work"), "{out}");
        assert!(out.contains("items=2"), "{out}");
        assert!(out.contains("chars=1234"), "{out}");
        assert!(out.contains("types=[Function×2]"), "{out}");
    }

    #[test]
    fn empty_results_suggest_the_fix_that_fits_the_mode() {
        // The advice differs by mode, and giving the wrong one sends the
        // agent to a knob it already has set.
        let empty = RankedContext {
            query: "nothing".into(),
            items: Vec::new(),
            total_chars: 0,
            seed_id: None,
        };
        let expanded = format_ranked_context(&empty, true);
        assert!(expanded.contains("whereClause"), "{expanded}");
        assert!(!expanded.contains("expand: true"), "already expanded: {expanded}");

        let seeds_only = format_ranked_context(&empty, false);
        assert!(seeds_only.contains("expand: true"), "{seeds_only}");

        // No drill-down hints either way — there is nothing to drill into.
        assert!(!expanded.contains("Drill-down"), "{expanded}");
    }

    #[test]
    fn the_drill_down_hints_point_at_the_top_hit() {
        let out = format_ranked_context(&ctx(vec![item("alpha", "Function"), item("bravo", "Class")]), true);
        assert!(out.contains(r#"traverse({ nodeId: "function:src/alpha.rs:alpha", hops: 1 })"#), "{out}");
        assert!(out.contains(r#"find_usages({ nodeId: "function:src/alpha.rs:alpha" })"#), "{out}");
        assert!(!out.contains("bravo.rs:bravo\", hops"), "the hints name the best hit, not the last");
    }

    // ── the project list ────────────────────────────────────────────────────

    #[test]
    fn no_projects_says_what_to_run_and_where_it_looked() {
        let out = format_project_list(&[], "/repo", "/home/u/.ug");
        assert!(out.contains("/home/u/.ug"), "naming the directory is half the answer: {out}");
        assert!(out.contains("ug gen"), "{out}");
    }

    #[test]
    fn the_current_repo_is_marked_in_the_list() {
        let projects = vec![
            ProjectInfo { name: "here".into(), repo_root: "/repo/a".into(), nodes: Some(10), edges: Some(20) },
            ProjectInfo { name: "other".into(), repo_root: "/repo/b".into(), nodes: Some(1), edges: Some(2) },
        ];
        let out = format_project_list(&projects, "/repo/a", "/home/u/.ug");

        assert!(out.contains("# Indexed projects (2)"), "{out}");
        let here_line = out.lines().find(|l| l.contains("**here**")).expect("the row");
        assert!(here_line.ends_with("← current"), "{here_line}");
        let other_line = out.lines().find(|l| l.contains("**other**")).expect("the row");
        assert!(!other_line.contains("current"), "{other_line}");
    }

    #[test]
    fn a_project_with_no_counts_shows_a_question_mark_not_a_zero() {
        // A project whose metadata predates the counts has an unknown size.
        // Printing 0 would read as an empty index.
        let projects = vec![ProjectInfo {
            name: "old".into(),
            repo_root: "/repo".into(),
            nodes: None,
            edges: None,
        }];
        let out = format_project_list(&projects, "/elsewhere", "/home/u/.ug");
        assert!(out.contains("(? nodes, ? edges)"), "{out}");
    }
}
