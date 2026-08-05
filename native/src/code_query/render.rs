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

    let total = page.rows.len();
    let (from, to) = answer.window.slice(total);

    // A "by file" concentration summary, computed over every matched row
    // (not just the visible window). `dead_code` / `untested_symbols` tend
    // to surface dozens of route handlers from one file as the "top" rows;
    // this summary shows that concentration up front so a reader does not
    // mistake a dynamic-dispatch pile for the real suspects.
    if answer.by_folder {
        push_by_file(&mut out, answer, style);
    }

    if from >= to {
        // The window starts past the last row. Saying "no rows" here would
        // be indistinguishable from "the query matched nothing", which is a
        // completely different situation and would send the caller off to
        // debug a query that is working.
        out.push_str(&format!(
            "\nThis result has {} row(s); the requested window starts at row {}.\n",
            total, answer.window.start
        ));
        push_caveats(&mut out, answer, style);
        return out;
    }

    // A one-row, one-cell result is a bare count; a table would be four
    // lines of formatting around a single number.
    let scalar = total == 1 && page.rows[0].len() == 1;
    out.push('\n');
    if scalar {
        out.push_str(&format!(
            "{} = {}\n",
            page.columns.first().map(String::as_str).unwrap_or("value"),
            cell(&page.rows[0][0])
        ));
    } else {
        push_table(&mut out, answer, style, from, to);
    }

    // Two different denominators, and conflating them would mislead:
    // `rows_matched` is what the graph matched before grouping, `total` is
    // how many result rows exist, and the window is what you are looking at.
    if !scalar {
        let mut parts = vec![format!("rows {}–{} of {}", from + 1, to, total)];
        if page.rows_matched > total {
            parts.push(format!("{} graph matches before grouping", page.rows_matched));
        }
        out.push_str(&style.dim(&format!("\n{}\n", parts.join(" · "))));

        if to < total {
            // A full, runnable command — `range` is a `ug query` flag, not a
            // preset argument, and a bare `range "26-45"` left callers
            // guessing the syntax. For a preset we can name it; for raw GQL
            // the query is too long to embed, so point at the flag instead.
            let next_from = to + 1;
            let next_to = (to + 20).min(total);
            let cap = if answer.window.is_capped(total) {
                " (window capped at 200 rows)"
            } else {
                ""
            };
            let hint = if answer.from_preset {
                format!(
                    "next: ug query {} --range {}-{}{}",
                    answer.title, next_from, next_to, cap
                )
            } else {
                format!(
                    "next: re-run with --range {}-{}{} (your --gql stays the same)",
                    next_from, next_to, cap
                )
            };
            out.push_str(&style.dim(&format!("{}\n", hint)));
        }
    }

    push_caveats(&mut out, answer, style);

    if out.chars().count() > MAX_CHARS {
        let keep: String = out.chars().take(MAX_CHARS).collect();
        return format!("{}\n… output truncated at {} chars\n", keep, MAX_CHARS);
    }
    out
}

fn push_by_file(out: &mut String, answer: &QueryAnswer, style: Render) {
    // Extract the file from the first column. Most list presets return a
    // node id (`kind:file:name`) there; the file is the `/`-bearing segment.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for row in &answer.page.rows {
        if let Some(file) = row.first().and_then(|v| file_of(&cell(v))) {
            *counts.entry(file).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.push_str(&style.dim("by file (all matches):"));
    out.push('\n');
    for (file, n) in ranked.into_iter().take(15) {
        out.push_str(&style.dim(&format!("  {:>4}  {}", n, file)));
        out.push('\n');
    }
    out.push('\n');
}

/// Pull a repo-relative file path out of a first-column cell. Recognises a
/// node id (`kind:file:name` — the `/`-bearing segment) and a bare path;
/// returns `None` for anything that does not look like a file, so a preset
/// whose first column is a metric or a name is simply left ungrouped.
fn file_of(cell: &str) -> Option<String> {
    cell.split(':')
        .find(|seg| seg.contains('/') && seg.contains('.'))
        .map(|s| s.to_string())
        .or_else(|| {
            if cell.contains('/') && cell.contains('.') {
                Some(cell.to_string())
            } else {
                None
            }
        })
}

fn push_table(out: &mut String, answer: &QueryAnswer, style: Render, from: usize, to: usize) {
    let page = &answer.page;
    let visible = &page.rows[from..to];

    // A `collect()` column is a distribution, not a value. Rendering the
    // list would blow the budget for no information; percentiles are what
    // the caller wanted from it.
    let rendered: Vec<Vec<String>> = visible
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
            visible
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

}

// A previous revision also printed a handful of node ids from beyond the
// visible rows, as a nudge toward the next call. Row ranges replaced it:
// "next: ug query dead_code --range 21-40" says the same thing precisely,
// costs a line instead of five ids, and unlike a sample it does not leave
// the reader guessing which rows it skipped — or, crucially, where the
// `--range` flag goes.

/// Everything that qualifies the number above it.
fn push_caveats(out: &mut String, answer: &QueryAnswer, style: Render) {
    // Unindexed properties first: this is the failure that turns a query
    // into a confident lie, so it does not get buried under coverage.
    if !answer.unindexed.is_empty() {
        out.push_str(&format!(
            "\n⚠ NOT INDEXED: {} — no node carries {}, so every predicate on \
             {} matched nothing. This answer is not about what you asked. \
             Run `ug regen`; if the property still shows as absent, this \
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
        // Percentages only: the raw `present/total` counts are scale, which
        // a reader reasons about from the rows above, not from the caveat
        // line. Spelling the percentage keeps the same trust signal at a
        // fraction of the width.
        let parts: Vec<String> = populated
            .iter()
            .map(|c| {
                format!("{} {:.0}%", c.property, 100.0 * c.present as f64 / c.total.max(1) as f64)
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
            window: crate::code_query::range::RowRange::first(20),
            gql: "MATCH (n) RETURN count(*) AS c".into(),
            from_preset: false,
            by_folder: false,
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
        assert!(out.contains("loc 96%"), "{out}");
        // Raw counts are gone — the percentage is what the reader uses.
        assert!(!out.contains("2181/2280"), "{out}");
    }

    #[test]
    fn full_coverage_shows_100_percent() {
        let a = answer(
            page(&["c"], vec![vec![QueryValue::Int(5)]]),
            vec![Coverage {
                property: "loc".into(),
                present: 2280,
                total: 2280,
            }],
        );
        let out = render(&a, Render::Markdown);
        assert!(out.contains("loc 100%"), "{out}");
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

    fn numbered_rows(n: usize) -> QueryPage {
        page(
            &["id", "loc"],
            (0..n)
                .map(|i| vec![QueryValue::Str(format!("f{i}")), QueryValue::Int(i as i64)])
                .collect(),
        )
    }

    #[test]
    fn the_window_is_always_stated_so_the_reader_knows_which_rows_these_are() {
        let mut a = answer(numbered_rows(30), vec![]);
        a.window = crate::code_query::range::RowRange::first(5);
        let out = render(&a, Render::Markdown);
        assert!(out.contains("rows 1–5 of 30"), "{out}");
        assert!(out.contains("f0") && out.contains("f4"), "{out}");
        assert!(!out.contains("f5"), "row 6 is outside the window: {out}");
    }

    #[test]
    fn a_mid_result_window_shows_exactly_that_slice() {
        let mut a = answer(numbered_rows(122), vec![]);
        a.window = crate::code_query::range::parse("11-35").unwrap();
        let out = render(&a, Render::Markdown);
        assert!(out.contains("rows 11–35 of 122"), "{out}");
        // Rows are 1-based, so row 11 is `f10`.
        assert!(out.contains("f10"), "{out}");
        assert!(out.contains("f34"), "{out}");
        assert!(!out.contains("f9"), "row 10 is before the window: {out}");
        assert!(!out.contains("f35"), "row 36 is after the window: {out}");
    }

    #[test]
    fn a_window_that_reaches_the_end_offers_no_next_page() {
        let mut a = answer(numbered_rows(30), vec![]);
        a.window = crate::code_query::range::parse("21-end").unwrap();
        let out = render(&a, Render::Markdown);
        assert!(out.contains("rows 21–30 of 30"), "{out}");
        assert!(!out.contains("next:"), "nothing left to fetch: {out}");
    }

    #[test]
    fn a_partial_window_names_the_exact_range_to_ask_for_next() {
        let mut a = answer(numbered_rows(122), vec![]);
        a.window = crate::code_query::range::parse("11-35").unwrap();
        // Default `answer()` is `from_preset: false`, so the hint points at
        // the `--range` flag rather than embedding a (raw GQL) command.
        let out = render(&a, Render::Markdown);
        assert!(out.contains("--range 36-55"), "{out}");
    }

    #[test]
    fn a_partial_preset_window_shows_a_runnable_next_command() {
        let mut a = answer(numbered_rows(122), vec![]);
        a.title = "dead_code".into();
        a.from_preset = true;
        a.window = crate::code_query::range::parse("11-35").unwrap();
        let out = render(&a, Render::Markdown);
        // The hint is a full command an agent can copy-paste: the preset
        // name and the `--range` flag, not a bare `range "36-55"`.
        assert!(out.contains("next: ug query dead_code --range 36-55"), "{out}");
    }

    /// Distinct from "the query matched nothing", which would send the
    /// caller off to debug a query that is working perfectly.
    #[test]
    fn a_window_past_the_end_says_how_many_rows_there_actually_are() {
        let mut a = answer(numbered_rows(12), vec![]);
        a.window = crate::code_query::range::parse("50-60").unwrap();
        let out = render(&a, Render::Markdown);
        assert!(out.contains("has 12 row(s)"), "{out}");
        assert!(out.contains("starts at row 50"), "{out}");
        assert!(!out.contains("No rows matched"), "wrong diagnosis: {out}");
    }

    #[test]
    fn output_is_capped() {
        let rows: Vec<Vec<QueryValue>> = (0..500)
            .map(|i| vec![QueryValue::Str("x".repeat(50)), QueryValue::Int(i)])
            .collect();
        let mut a = answer(page(&["id", "loc"], rows), vec![]);
        a.window = crate::code_query::range::parse("1-end").unwrap();
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
