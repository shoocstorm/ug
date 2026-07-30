//! `code_query`: whole-repo statistical questions over the indexed graph.
//!
//! An agent asked "how many methods are longer than 50 lines?" has, without
//! this, two bad options: grep every file and count (≈500k tokens on this
//! repo, impossible on a monorepo), or loop a per-file tool 80 times. Both
//! are unnecessary — ingest already distilled the repo into properties a
//! query engine can aggregate, and the answer is a `count(*)` that costs
//! about thirty tokens to return.
//!
//! The query language is OverGraph's GQL, executed by
//! [`KnowledgeStore::execute_query`]. What this module adds is everything
//! that is *not* a query language:
//!
//! - **Presets** ([`presets`]) — named questions, so the common path costs
//!   ~20 tokens instead of ~300 reasoning ones.
//! - **Coverage** — the denominator behind every statistic. See
//!   [`coverage_for`]; this is the module's most important job.
//! - **Cap warnings** — the engine truncates rather than erroring, so a
//!   blast radius can be a silent under-report that reads as precise.
//!
//! One implementation, three transports, matching `agent_tools`: the MCP
//! tool, the `ug query` subcommand and `POST /api/tools/code_query` all
//! call [`run`] and render the same [`QueryAnswer`].

pub mod presets;
pub mod range;
pub mod render;

use crate::storage::store::{KnowledgeStore, QueryLimits, QueryPage, QueryParams, QueryValue};
use presets::{ParamValue, Preset};
use std::collections::BTreeMap;

/// The names [`CodeQueryParams`] owns, which no preset argument may shadow.
///
/// Callers — models especially — file these under `args` beside the preset's
/// own arguments, because that is where "the parameters for this call" look
/// like they live. Transports lift them back out and [`bind`] explains the
/// mistake when they don't; both need to agree on the list, and a preset that
/// declared one of these names would break the lift silently.
pub const OWN_PARAMS: &[&str] = &["preset", "gql", "limit", "range", "project"];

/// What the caller asked for: a preset by name, or raw GQL.
#[derive(Debug, Clone, Default)]
pub struct CodeQueryParams {
    pub preset: Option<String>,
    pub gql: Option<String>,
    /// Preset arguments, as strings — they arrive from JSON tool args and
    /// CLI flags alike, and are coerced against the preset's declared
    /// parameter types before binding.
    pub args: BTreeMap<String, String>,
    /// Rows to render. Does not change what the engine computes, so the
    /// reported totals stay honest when this truncates the table.
    pub limit: Option<usize>,
    /// Which window of rows to render — `"11-35"`, `"34-end"`, `"top 10"`.
    /// Overrides [`Self::limit`] when both are given. See [`range`].
    pub range: Option<String>,
}

/// Population of one property across the store.
#[derive(Debug, Clone)]
pub struct Coverage {
    pub property: String,
    pub present: usize,
    pub total: usize,
}

impl Coverage {
    /// A property no node carries. Every predicate on it matched nothing,
    /// and the query still returned a number — this is the case the whole
    /// coverage contract exists to catch.
    pub fn is_absent(&self) -> bool {
        self.present == 0
    }
}

/// A rendered-ready answer: the rows, plus everything needed to know
/// whether to believe them.
#[derive(Debug, Clone)]
pub struct QueryAnswer {
    /// Preset name, or `"gql"` for a raw query.
    pub title: String,
    pub description: Option<String>,
    pub page: QueryPage,
    pub coverage: Vec<Coverage>,
    /// Properties the query referenced that no node carries.
    pub unindexed: Vec<String>,
    /// The window of rows to render. Every count reported alongside the
    /// table is over the *whole* result, not this window.
    pub window: range::RowRange,
    /// The GQL that ran, echoed for a preset so the caller can adapt it.
    pub gql: String,
    /// Whether the query came from the preset registry rather than the
    /// caller. Changes which engine warnings are worth repeating — see
    /// `render::is_expected_noise`.
    pub from_preset: bool,
}

const DEFAULT_LIMIT: usize = 20;

/// Every property a query may filter or aggregate on, for the capability
/// manifest.
///
/// The manifest reports each of these with a live count rather than as a
/// bare list, because "this build can write `params`" and "this index
/// contains `params`" are different claims and only the second one makes
/// a query meaningful. A name here that no node carries reports as NOT
/// INDEXED, which is exactly the signal a caller needs.
///
/// Fixed columns come first, then the derived facts from
/// [`crate::storage::facts`]. Adding a fact there means adding it here.
pub const QUERYABLE_PROPERTIES: &[&str] = &[
    "node_type",
    "name",
    "file",
    "start_line",
    "end_line",
    "language",
    "classification",
    "loc",
    "code_lines",
    "comment_lines",
    "doc_lines",
    "params",
    "max_nesting",
    "members",
    "has_doc",
    "has_comments",
    "folder",
    "is_test",
    "in_degree",
    "out_degree",
    "qualified_name",
    "route",
    "annotations",
];

/// Resolve, execute and annotate one query.
pub async fn run(
    store: &dyn KnowledgeStore,
    params: &CodeQueryParams,
) -> Result<QueryAnswer, String> {
    let (title, description, gql, bound) = resolve(params)?;

    // Resolve the window before touching the store: a malformed range is
    // the caller's mistake and should not cost a query to discover.
    let window = match params.range.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => range::parse(raw).ok_or_else(|| {
            format!(
                "Could not read {:?} as a row range. Use a count (`20`), a closed \
                 window (`11-35`), or an open one (`34-end`). Rows are 1-based and \
                 both ends are inclusive.",
                raw
            )
        })?,
        None => range::RowRange::first(
            params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, range::MAX_WINDOW),
        ),
    };

    let limits = QueryLimits::default();

    let page = store
        .execute_query(&gql, &bound, &limits)
        .await
        .map_err(|e| explain_failure(&e.to_string(), &gql))?;

    let coverage = coverage_for(store, &gql, &limits).await;
    let unindexed = coverage
        .iter()
        .filter(|c| c.is_absent())
        .map(|c| c.property.clone())
        .collect();

    Ok(QueryAnswer {
        title,
        description,
        page,
        coverage,
        unindexed,
        window,
        gql,
        from_preset: params.preset.is_some(),
    })
}

/// Turn the request into a query and its bound parameters.
fn resolve(
    params: &CodeQueryParams,
) -> Result<(String, Option<String>, String, QueryParams), String> {
    match (&params.preset, &params.gql) {
        (Some(_), Some(_)) => Err(
            "Pass either `preset` or `gql`, not both — a preset is already a query.".to_string(),
        ),
        (None, None) => Err(format!(
            "code_query needs a `preset` or a `gql` query.\n\nAvailable presets: {}",
            preset_names().join(", ")
        )),
        (Some(name), None) => {
            let preset = presets::find(name).ok_or_else(|| unknown_preset(name))?;
            let bound = bind(preset, &params.args)?;
            Ok((
                preset.name.to_string(),
                Some(preset.description.to_string()),
                preset.gql.to_string(),
                bound,
            ))
        }
        (None, Some(q)) if q.trim().is_empty() => {
            Err("`gql` was empty — pass a query or use a preset.".to_string())
        }
        (None, Some(q)) => {
            // Raw GQL takes its parameters as plain strings. Typed preset
            // coercion has no schema to work from here, and guessing that
            // "50" means the integer 50 would silently change `>` from a
            // string comparison to a numeric one.
            let bound = params
                .args
                .iter()
                .map(|(k, v)| (k.clone(), QueryValue::Str(v.clone())))
                .collect();
            Ok(("gql".to_string(), None, q.trim().to_string(), bound))
        }
    }
}

/// Coerce the caller's string arguments against the preset's declared
/// parameters, filling defaults and rejecting anything undeclared.
fn bind(preset: &Preset, args: &BTreeMap<String, String>) -> Result<QueryParams, String> {
    let mut bound = QueryParams::new();

    for spec in preset.params {
        match args.get(spec.name) {
            Some(raw) => {
                let value = match spec.default {
                    // The declared default fixes the type: a parameter
                    // that defaults to an integer must bind as one, or
                    // `n.loc > '50'` compares a number against a string.
                    Some(ParamValue::Int(_)) => {
                        QueryValue::Int(raw.trim().parse().map_err(|_| {
                            format!(
                                "`{}` expects a number for `{}`, got {:?}.",
                                preset.name, spec.name, raw
                            )
                        })?)
                    }
                    _ => QueryValue::Str(raw.trim().to_string()),
                };
                bound.insert(spec.name.to_string(), value);
            }
            None => match spec.default {
                Some(ParamValue::Int(i)) => {
                    bound.insert(spec.name.to_string(), QueryValue::Int(i));
                }
                Some(ParamValue::Str(s)) => {
                    bound.insert(spec.name.to_string(), QueryValue::Str(s.to_string()));
                }
                None => {
                    return Err(format!(
                        "`{}` requires the `{}` parameter — {}",
                        preset.name, spec.name, spec.description
                    ));
                }
            },
        }
    }

    // An argument the preset does not take is almost always a typo for one
    // it does, and silently ignoring it produces an answer to a different
    // question than the one asked.
    for key in args.keys() {
        if !preset.params.iter().any(|p| p.name == key.as_str()) {
            // The commonest miss is not a typo but a level confusion: the
            // query's own parameters filed under `args` alongside the
            // preset's. Say where it belongs, or the reader's only option is
            // to drop it and answer a narrower question than was asked.
            if OWN_PARAMS.contains(&key.as_str()) {
                return Err(format!(
                    "`{}` is a parameter of the query itself, not of the `{}` preset — \
                     pass it alongside the preset, not inside `args`: \
                     `--{} <value>` on the CLI, or {{\"preset\": \"{}\", \"{}\": …}} as a tool argument.",
                    key, preset.name, key, preset.name, key
                ));
            }
            let accepted: Vec<&str> = preset.params.iter().map(|p| p.name).collect();
            return Err(format!(
                "`{}` does not take a `{}` parameter. Accepted: {}",
                preset.name,
                key,
                if accepted.is_empty() {
                    "(none)".to_string()
                } else {
                    accepted.join(", ")
                }
            ));
        }
    }

    Ok(bound)
}

/// How populated is each property this query touched?
///
/// The dominant failure of a statistics tool is not a wrong query, it is a
/// right query over a property nothing carries: `n.comment_lines > 3`
/// returns `0` with no error and no warning, and "0 methods have long
/// comments" is far worse than a refusal. So every answer states its
/// denominators.
///
/// Best-effort by design — a store that cannot answer the coverage probe
/// still returns the statistic, just without the caveat. Failing the whole
/// call because the *caveat* could not be computed would be the wrong
/// trade.
pub async fn coverage_for(
    store: &dyn KnowledgeStore,
    gql: &str,
    limits: &QueryLimits,
) -> Vec<Coverage> {
    let props = referenced_properties(gql);
    if props.is_empty() {
        return Vec::new();
    }

    // One query for all of them: `count(expr)` skips nulls, so
    // `count(n.loc)` is exactly "how many nodes carry loc".
    let projections: Vec<String> = props
        .iter()
        .enumerate()
        .map(|(i, p)| format!("count(n.{}) AS c{}", p, i))
        .collect();
    let probe = format!(
        "MATCH (n) RETURN count(*) AS total, {}",
        projections.join(", ")
    );

    let Ok(page) = store
        .execute_query(&probe, &QueryParams::new(), limits)
        .await
    else {
        return Vec::new();
    };
    let Some(row) = page.rows.first() else {
        return Vec::new();
    };
    let total = row.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;

    props
        .into_iter()
        .enumerate()
        .filter_map(|(i, property)| {
            let present = row.get(i + 1)?.as_f64()? as usize;
            Some(Coverage {
                property,
                present,
                total,
            })
        })
        .collect()
}

/// Property names a query reads, as `<binding>.<name>`.
///
/// A deliberately shallow scan: it wants the names to *probe*, and a name
/// that turns out not to be a stored property simply reports as absent,
/// which is the same thing the caller needed to know anyway.
fn referenced_properties(gql: &str) -> Vec<String> {
    let bytes = gql.as_bytes();
    let mut found: Vec<String> = Vec::new();

    for (i, _) in gql.match_indices('.') {
        // A dot inside `*1..3`, a decimal literal, or a quoted path is not
        // a property access. Requiring an identifier character on the left
        // and an alphabetic one on the right rules all three out.
        let left_ok = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !left_ok {
            continue;
        }
        let name: String = gql[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }
        if !found.contains(&name) {
            found.push(name);
        }
    }
    found
}

/// Turn an engine error into something the caller can act on.
fn explain_failure(err: &str, gql: &str) -> String {
    let mut out = format!("Query failed: {}", err);
    if err.contains("ReadOnlyViolation") {
        out.push_str(
            "\n\ncode_query is read-only. It answers questions about the graph; \
             it cannot modify the index (use `regen` for that).",
        );
    } else if err.contains("max_frontier") || err.contains("exceeded configured cap") {
        // Worth an explicit hand-hold: this is the one failure mode whose
        // cause is the *shape* of the traversal rather than anything
        // wrong with the query, and the fix is never obvious from the
        // engine's message.
        out.push_str(
            "\n\nThe traversal expanded too far to complete. This happens when a \
             variable-length path has nothing to anchor it — it starts from every \
             matching node at once. Narrow it: reduce the hop bound (`*1..2` rather \
             than `*1..3`), list fewer edge labels, or anchor one end to a specific \
             file with `WHERE t.file = $target`.",
        );
    } else if err.contains("parse error") {
        out.push_str(&format!(
            "\n\nThe query that failed:\n{}\n\n\
             This is OverGraph GQL (Cypher-shaped). Note two things it is strict about: \
             an `EXISTS {{ … }}` subquery needs its own RETURN clause, and `NOT x IN [...]` \
             must be parenthesised as `NOT (x IN [...])`.",
            gql
        ));
    }
    out
}

fn preset_names() -> Vec<&'static str> {
    presets::all().iter().map(|p| p.name).collect()
}

fn unknown_preset(name: &str) -> String {
    // Nearest match by shared prefix — cheap, and enough to catch the
    // realistic error, which is a half-remembered name rather than a
    // random string.
    let mut best: Option<(usize, &str)> = None;
    for candidate in preset_names() {
        let shared = name
            .chars()
            .zip(candidate.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if shared >= 3 && best.map(|(n, _)| shared > n).unwrap_or(true) {
            best = Some((shared, candidate));
        }
    }
    match best {
        Some((_, suggestion)) => format!(
            "Unknown preset `{}`. Did you mean `{}`?\n\nAll presets: {}",
            name,
            suggestion,
            preset_names().join(", ")
        ),
        None => format!(
            "Unknown preset `{}`.\n\nAvailable presets: {}",
            name,
            preset_names().join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(preset: &str) -> CodeQueryParams {
        CodeQueryParams {
            preset: Some(preset.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_preset_resolves_to_its_query_with_defaults_bound() {
        let (title, _, gql, bound) = resolve(&params("long_functions")).unwrap();
        assert_eq!(title, "long_functions");
        assert!(gql.contains("$min_loc"));
        assert_eq!(bound["min_loc"], QueryValue::Int(50));
    }

    #[test]
    fn an_integer_parameter_binds_as_an_integer_not_a_string() {
        let mut p = params("long_functions");
        p.args.insert("min_loc".into(), "120".into());
        let (_, _, _, bound) = resolve(&p).unwrap();
        // Binding "120" as a string would make `n.loc > $min_loc` compare
        // a number against text, which does not mean what it looks like.
        assert_eq!(bound["min_loc"], QueryValue::Int(120));
    }

    #[test]
    fn a_non_numeric_value_for_a_numeric_parameter_is_rejected() {
        let mut p = params("long_functions");
        p.args.insert("min_loc".into(), "fifty".into());
        let err = resolve(&p).unwrap_err();
        assert!(err.contains("expects a number"), "{err}");
    }

    #[test]
    fn a_required_parameter_cannot_be_defaulted() {
        let err = resolve(&params("impact")).unwrap_err();
        assert!(err.contains("requires the `target` parameter"), "{err}");
    }

    /// A misfiled `limit` must be told where it belongs. Answering only
    /// "long_functions does not take a limit" leaves dropping it as the
    /// reader's best move, which silently narrows the question.
    #[test]
    fn a_misfiled_query_parameter_says_where_it_goes() {
        let mut p = params("long_functions");
        p.args.insert("limit".into(), "20".into());
        let err = resolve(&p).unwrap_err();
        assert!(err.contains("parameter of the query itself"), "{err}");
        assert!(err.contains("not inside `args`"), "{err}");
    }

    #[test]
    fn an_undeclared_argument_is_an_error_not_a_silent_no_op() {
        let mut p = params("long_functions");
        p.args.insert("min_lines".into(), "50".into());
        let err = resolve(&p).unwrap_err();
        assert!(err.contains("does not take a `min_lines`"), "{err}");
        assert!(err.contains("min_loc"), "should list what it does take");
    }

    #[test]
    fn a_misspelled_preset_gets_a_suggestion() {
        let err = resolve(&params("long_function")).unwrap_err();
        assert!(err.contains("Did you mean `long_functions`"), "{err}");
    }

    #[test]
    fn preset_and_gql_together_are_rejected() {
        let p = CodeQueryParams {
            preset: Some("repo_census".into()),
            gql: Some("MATCH (n) RETURN count(*)".into()),
            ..Default::default()
        };
        assert!(resolve(&p).unwrap_err().contains("not both"));
    }

    #[test]
    fn neither_preset_nor_gql_lists_the_presets() {
        let err = resolve(&CodeQueryParams::default()).unwrap_err();
        assert!(err.contains("repo_census"), "{err}");
    }

    #[test]
    fn property_scan_finds_reads_and_ignores_path_bounds_and_decimals() {
        let props = referenced_properties(
            "MATCH (a)-[:Calls*1..3]->(b) WHERE a.loc > 2.5 AND b.is_test = 0 \
             RETURN a.folder AS f, count(*) AS c",
        );
        assert!(props.contains(&"loc".to_string()));
        assert!(props.contains(&"is_test".to_string()));
        assert!(props.contains(&"folder".to_string()));
        // `*1..3` and `2.5` must not register as property reads.
        assert!(!props
            .iter()
            .any(|p| p.starts_with(|c: char| c.is_numeric())));
        assert_eq!(props.len(), 3, "{props:?}");
    }

    #[test]
    fn property_scan_deduplicates() {
        let props = referenced_properties("MATCH (n) WHERE n.loc > 1 RETURN n.loc AS loc");
        assert_eq!(props, vec!["loc".to_string()]);
    }

    #[test]
    fn absent_coverage_is_zero_present_not_a_missing_entry() {
        let c = Coverage {
            property: "comment_lines".into(),
            present: 0,
            total: 2280,
        };
        assert!(c.is_absent());
        let c = Coverage {
            property: "loc".into(),
            present: 2181,
            total: 2280,
        };
        assert!(!c.is_absent());
    }
}
