//! Rendering a [`QueryAnswer`] into the compact text an agent reads.
//!
//! This is where the token argument is actually won. The engine hands
//! back rows; a question that costs 500k tokens to answer by grep has to
//! come back under a few hundred, and it has to come back with enough
//! context that a *wrong* number cannot pass for a right one. So every
//! answer carries three things beyond the table:
//!
//! 1. **A headline with denominators** — "122 of 1477" beats "122".
//! 2. **Coverage** — which properties the query read, and how many nodes
//!    carry them. A statistic over a property 60% of nodes have is a
//!    confidently wrong answer and the caller cannot otherwise tell.
//! 3. **Truncation and engine warnings** — caps in OverGraph truncate
//!    silently, so a blast radius can be an under-report that reads as
//!    precise.

use super::{Coverage, QueryAnswer};
use crate::agent_tools::Render;
use crate::storage::store::QueryValue;

/// Hard ceiling on rendered output. A statistics answer that costs more
/// than this has stopped being cheaper than the thing it replaces.
const MAX_CHARS: usize = 3_000;

/// Longest string cell before it is elided from the middle — node ids are
/// the long values here, and their two informative ends are the file and
/// the symbol name.
const MAX_CELL: usize = 58;

/// Engine diagnostics that tell this particular reader nothing.
///
/// Two cases, and the second one depends on who wrote the query:
///
/// - **Full-scan notices**, always. A statistic is a full scan by nature —
///   there is no bounded anchor for "how many functions exceed 50 lines" —
///   so `code_query` opts into full scans deliberately. Echoing the
///   engine's note about it on every answer would train the reader to skip
///   the warning line, which is exactly where the load-bearing warnings
///   live.
/// - **Unknown label notices, for presets only.** ug's presets name every
///   dependency edge type on purpose, and no single language emits all of
///   them, so `Overrides` is legitimately absent from a Rust graph. In a
///   query the *caller* wrote, the same warning almost always means a
///   typo'd label silently matching nothing, which is worth saying.
fn is_expected_noise(w: &str, from_preset: bool) -> bool {
    let lower = w.to_ascii_lowercase();
    if lower.contains("full scan explicitly allowed") {
        return true;
    }
    from_preset && (lower.contains("unknownedgelabel") || lower.contains("unknownnodelabel"))
}

pub fn render(answer: &QueryAnswer, style: Render) -> String {
    let mut out = String::new();

    out.push_str(&style.heading(&answer.title));
    out.push('\n');
    if let Some(d) = &answer.description {
        out.push_str(&style.dim(d));
        out.push('\n');
    }

    let page = &answer.page;
    if page.rows.is_empty() {
        out.push_str("\nNo rows matched.\n");
        push_caveats(&mut out, answer, style);
        return out;
    }

    // A one-row, one-cell result is a bare count; a table would be four
    // lines of formatting around a single number.
    let scalar = page.rows.len() == 1 && page.rows[0].len() == 1;
    out.push('\n');
    if scalar {
        out.push_str(&format!(
            "{} = {}\n",
            page.columns.first().map(String::as_str).unwrap_or("value"),
            cell(&page.rows[0][0])
        ));
    } else {
        push_table(&mut out, answer, style);
    }

    // `rows_matched` counts what the engine matched before grouping and
    // LIMIT, which is the denominator a reader wants: "127 functions over
    // 50 lines, across 7 folders" rather than just "7 rows".
    if !scalar && page.rows_matched > page.rows.len() {
        out.push_str(&style.dim(&format!(
            "\n{} rows shown · {} graph matches before grouping\n",
            page.rows.len().min(answer.limit),
            page.rows_matched
        )));
    }

    push_samples(&mut out, answer, style);
    push_caveats(&mut out, answer, style);

    if out.chars().count() > MAX_CHARS {
        let keep: String = out.chars().take(MAX_CHARS).collect();
        return format!("{}\n… output truncated at {} chars\n", keep, MAX_CHARS);
    }
    out
}

fn push_table(out: &mut String, answer: &QueryAnswer, style: Render) {
    let page = &answer.page;
    let shown = page.rows.len().min(answer.limit);

    // A `collect()` column is a distribution, not a value. Rendering the
    // list would blow the budget for no information; percentiles are what
    // the caller wanted from it.
    let rendered: Vec<Vec<String>> = page.rows[..shown]
        .iter()
        .map(|r| r.iter().map(cell).collect())
        .collect();

    let cols = page.columns.len();
    let mut widths: Vec<usize> = page.columns.iter().map(|c| c.chars().count()).collect();
    for row in &rendered {
        for (i, c) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }

    // Right-align a column only when every cell in it is a number, so
    // mixed columns stay readable rather than ragged.
    let numeric: Vec<bool> = (0..cols)
        .map(|i| {
            page.rows[..shown]
                .iter()
                .filter_map(|r| r.get(i))
                .all(|v| matches!(v, QueryValue::Int(_) | QueryValue::Float(_)))
        })
        .collect();

    let header: Vec<String> = page
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| pad(c, widths[i], numeric.get(i).copied().unwrap_or(false)))
        .collect();
    out.push_str(&style.bold(header.join("  ").trim_end()));
    out.push('\n');

    for row in &rendered {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths[i], numeric.get(i).copied().unwrap_or(false)))
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }

    if page.rows.len() > shown {
        out.push_str(&style.dim(&format!(
            "… {} more row(s) — raise `limit` to see them\n",
            page.rows.len() - shown
        )));
    }
}

/// Node ids in the result, offered for the follow-up call.
///
/// The point of a statistics answer is usually the next action, and the
/// next action needs ids: `get_code`, `find_usages`, `traverse` all take
/// them. Only ids are worth repeating, which is why this looks for the
/// `id` column specifically rather than sampling arbitrary strings.
fn push_samples(out: &mut String, answer: &QueryAnswer, style: Render) {
    let page = &answer.page;
    let Some(ix) = page.columns.iter().position(|c| c == "id") else {
        return;
    };
    if page.rows.len() <= answer.limit {
        // Already fully listed in the table above.
        return;
    }
    let ids: Vec<String> = page
        .rows
        .iter()
        .skip(answer.limit)
        .filter_map(|r| r.get(ix))
        .filter_map(|v| match v {
            QueryValue::Str(s) => Some(style.id(s)),
            _ => None,
        })
        .take(5)
        .collect();
    if !ids.is_empty() {
        out.push_str(&format!("\nalso: {}\n", ids.join(" · ")));
    }
}

/// Everything that qualifies the number above it.
fn push_caveats(out: &mut String, answer: &QueryAnswer, style: Render) {
    // Unindexed properties first: this is the failure that turns a query
    // into a confident lie, so it does not get buried under coverage.
    if !answer.unindexed.is_empty() {
        out.push_str(&format!(
            "\n⚠ NOT INDEXED: {} — no node carries {}, so every predicate on \
             {} matched nothing. This answer is not about what you asked. \
             Run `ug reindex`; if the property still shows as absent, this \
             build's indexer does not produce it yet.\n",
            answer.unindexed.join(", "),
            if answer.unindexed.len() == 1 {
                "this property"
            } else {
                "these properties"
            },
            if answer.unindexed.len() == 1 {
                "it"
            } else {
                "them"
            },
        ));
    }

    if answer.page.truncated {
        out.push_str("\n⚠ The row cap was reached — this result is a LOWER BOUND, not a total.\n");
    }

    for w in &answer.page.warnings {
        if !is_expected_noise(w, answer.from_preset) {
            out.push_str(&format!("⚠ engine: {}\n", w));
        }
    }

    let populated: Vec<&Coverage> = answer.coverage.iter().filter(|c| !c.is_absent()).collect();
    if !populated.is_empty() {
        let parts: Vec<String> = populated
            .iter()
            .map(|c| {
                if c.present == c.total {
                    format!("{} {}/{}", c.property, c.present, c.total)
                } else {
                    // Partial population is the quiet version of the same
                    // problem, so it gets the percentage spelled out.
                    format!(
                        "{} {}/{} ({:.0}%)",
                        c.property,
                        c.present,
                        c.total,
                        100.0 * c.present as f64 / c.total.max(1) as f64
                    )
                }
            })
            .collect();
        out.push_str(&style.dim(&format!("\ncoverage: {}\n", parts.join(" · "))));
    }
}

fn pad(s: &str, width: usize, right_align: bool) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let fill = " ".repeat(width - len);
    if right_align {
        format!("{}{}", fill, s)
    } else {
        format!("{}{}", s, fill)
    }
}

fn cell(v: &QueryValue) -> String {
    match v {
        QueryValue::Null => "—".to_string(),
        QueryValue::Bool(b) => b.to_string(),
        QueryValue::Int(i) => i.to_string(),
        QueryValue::Float(f) => format!("{:.1}", f),
        QueryValue::Str(s) => elide(s),
        QueryValue::List(items) => distribution(items),
    }
}

/// Summarise a `collect()` column as a distribution.
///
/// GQL has no `percentileCont` this engine can lower, so percentiles are
/// computed here — which is also the only place they *can* be, since the
/// list is the thing that would otherwise cost thousands of tokens to
/// return.
fn distribution(items: &[QueryValue]) -> String {
    let mut nums: Vec<f64> = items.iter().filter_map(QueryValue::as_f64).collect();
    if nums.is_empty() {
        return format!("<{} items>", items.len());
    }
    nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    format!(
        "n={} p50={:.0} p90={:.0} p99={:.0} max={:.0}",
        nums.len(),
        percentile(&nums, 0.50),
        percentile(&nums, 0.90),
        percentile(&nums, 0.99),
        nums[nums.len() - 1]
    )
}

/// Nearest-rank percentile over an ascending slice.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Shorten from the middle: a node id's two ends (file and symbol name)
/// are the informative parts, and truncating from the right throws the
/// symbol name away.
fn elide(s: &str) -> String {
    let len = s.chars().count();
    if len <= MAX_CELL {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_CELL / 2 - 1).collect();
    let tail: String = s.chars().skip(len - (MAX_CELL / 2 - 2)).collect::<String>();
    format!("{}…{}", head, tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_query::QueryAnswer;
    use crate::storage::store::QueryPage;

    fn answer(page: QueryPage, coverage: Vec<Coverage>) -> QueryAnswer {
        let unindexed = coverage
            .iter()
            .filter(|c| c.is_absent())
            .map(|c| c.property.clone())
            .collect();
        QueryAnswer {
            title: "test".into(),
            description: None,
            page,
            coverage,
            unindexed,
            limit: 20,
            gql: "MATCH (n) RETURN count(*) AS c".into(),
            from_preset: false,
        }
    }

    fn page(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryPage {
        QueryPage {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows_matched: rows.len(),
            rows,
            warnings: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn a_single_cell_result_renders_as_a_bare_count() {
        let a = answer(page(&["c"], vec![vec![QueryValue::Int(122)]]), vec![]);
        let out = render(&a, Render::Markdown);
        assert!(out.contains("c = 122"), "{out}");
        assert!(!out.contains("---"), "no table scaffolding for one number");
    }

    #[test]
    fn an_unindexed_property_is_reported_loudly_not_as_a_zero() {
        let a = answer(
            page(&["c"], vec![vec![QueryValue::Int(0)]]),
            vec![Coverage {
                property: "comment_lines".into(),
                present: 0,
                total: 2280,
            }],
        );
        let out = render(&a, Render::Markdown);
        assert!(out.contains("NOT INDEXED"), "{out}");
        assert!(out.contains("comment_lines"), "{out}");
        // The whole point: the reader must not walk away with the zero.
        assert!(out.contains("not about what you asked"), "{out}");
    }

    #[test]
    fn partial_coverage_states_its_percentage() {
        let a = answer(
            page(&["c"], vec![vec![QueryValue::Int(5)]]),
            vec![Coverage {
                property: "loc".into(),
                present: 2181,
                total: 2280,
            }],
        );
        let out = render(&a, Render::Markdown);
        assert!(out.contains("loc 2181/2280 (96%)"), "{out}");
    }

    #[test]
    fn full_coverage_omits_the_percentage_as_noise() {
        let a = answer(
            page(&["c"], vec![vec![QueryValue::Int(5)]]),
            vec![Coverage {
                property: "loc".into(),
                present: 2280,
                total: 2280,
            }],
        );
        let out = render(&a, Render::Markdown);
        assert!(out.contains("loc 2280/2280"), "{out}");
        assert!(!out.contains("100%"), "{out}");
    }

    #[test]
    fn truncation_is_called_a_lower_bound() {
        let mut p = page(
            &["a", "b"],
            vec![vec![QueryValue::Int(1), QueryValue::Int(2)]],
        );
        p.truncated = true;
        let out = render(&answer(p, vec![]), Render::Markdown);
        assert!(out.contains("LOWER BOUND"), "{out}");
    }

    #[test]
    fn the_expected_full_scan_note_is_suppressed_but_real_warnings_are_not() {
        let mut p = page(
            &["a", "b"],
            vec![vec![QueryValue::Int(1), QueryValue::Int(2)]],
        );
        p.warnings = vec![
            "full scan explicitly allowed for unanchored graph pattern".into(),
            "UnknownEdgeLabel".into(),
        ];
        let out = render(&answer(p, vec![]), Render::Markdown);
        assert!(!out.contains("full scan"), "expected noise leaked: {out}");
        assert!(out.contains("UnknownEdgeLabel"), "{out}");
    }

    /// A preset naming `Overrides` on a Rust graph is doing the right
    /// thing; the same warning on a hand-written query usually means a
    /// typo that silently matched nothing.
    #[test]
    fn an_unknown_label_is_noise_from_a_preset_and_a_signal_from_raw_gql() {
        let mut p = page(
            &["a", "b"],
            vec![vec![QueryValue::Int(1), QueryValue::Int(2)]],
        );
        p.warnings = vec!["UnknownEdgeLabel".into()];

        let mut from_preset = answer(p.clone(), vec![]);
        from_preset.from_preset = true;
        assert!(!render(&from_preset, Render::Markdown).contains("UnknownEdgeLabel"));

        let hand_written = answer(p, vec![]);
        assert!(render(&hand_written, Render::Markdown).contains("UnknownEdgeLabel"));
    }

    #[test]
    fn a_collect_column_becomes_percentiles_instead_of_a_list() {
        let locs: Vec<QueryValue> = (1..=100).map(QueryValue::Int).collect();
        let a = answer(
            page(
                &["locs", "kind"],
                vec![vec![QueryValue::List(locs), QueryValue::Str("fn".into())]],
            ),
            vec![],
        );
        let out = render(&a, Render::Markdown);
        assert!(out.contains("n=100"), "{out}");
        assert!(out.contains("p50=50"), "{out}");
        assert!(out.contains("p90=90"), "{out}");
    }

    #[test]
    fn rows_beyond_the_limit_are_counted_not_dropped_silently() {
        let rows: Vec<Vec<QueryValue>> = (0..30)
            .map(|i| vec![QueryValue::Str(format!("f{i}")), QueryValue::Int(i)])
            .collect();
        let mut a = answer(page(&["id", "loc"], rows), vec![]);
        a.limit = 5;
        let out = render(&a, Render::Markdown);
        assert!(out.contains("25 more row(s)"), "{out}");
        assert!(
            out.contains("also:"),
            "leftover ids should be offered: {out}"
        );
    }

    #[test]
    fn output_is_capped() {
        let rows: Vec<Vec<QueryValue>> = (0..500)
            .map(|i| vec![QueryValue::Str("x".repeat(50)), QueryValue::Int(i)])
            .collect();
        let mut a = answer(page(&["id", "loc"], rows), vec![]);
        a.limit = 200;
        let out = render(&a, Render::Markdown);
        assert!(out.chars().count() <= MAX_CHARS + 80, "{}", out.len());
    }

    #[test]
    fn an_empty_result_still_reports_its_caveats() {
        let a = answer(
            page(&["c"], vec![]),
            vec![Coverage {
                property: "route".into(),
                present: 0,
                total: 2280,
            }],
        );
        let out = render(&a, Render::Markdown);
        assert!(out.contains("No rows matched"), "{out}");
        assert!(
            out.contains("NOT INDEXED"),
            "an empty result needs the why: {out}"
        );
    }

    #[test]
    fn long_ids_keep_both_informative_ends() {
        let id = "function:native/src/storage/backends/neo4j.rs:Neo4jStore::personalized_pagerank";
        let short = elide(id);
        assert!(short.starts_with("function:native/src/storage"), "{short}");
        assert!(short.ends_with("pagerank"), "{short}");
        assert!(short.chars().count() <= MAX_CELL);
    }

    #[test]
    fn percentiles_are_nearest_rank_and_survive_a_single_value() {
        assert_eq!(percentile(&[7.0], 0.5), 7.0);
        assert_eq!(percentile(&[7.0], 0.99), 7.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.5), 2.0);
    }
}
