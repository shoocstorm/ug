//! `analyze`: whole-repo statistical questions over the indexed graph.
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
//! tool, the `ug analyze` subcommand and `POST /api/tools/analyze` all
//! call [`run`] and render the same [`QueryAnswer`].

pub mod presets;
pub mod range;
pub mod render;

use crate::storage::store::{KnowledgeStore, QueryLimits, QueryPage, QueryParams, QueryValue};
use presets::{ParamValue, Preset};
use std::collections::BTreeMap;

/// The names [`AnalyzeParams`] owns, which no preset argument may shadow.
///
/// Callers — models especially — file these under `args` beside the preset's
/// own arguments, because that is where "the parameters for this call" look
/// like they live. Transports lift them back out and [`bind`] explains the
/// mistake when they don't; both need to agree on the list, and a preset that
/// declared one of these names would break the lift silently.
pub const OWN_PARAMS: &[&str] = &["preset", "gql", "limit", "range", "project"];

/// What the caller asked for: a preset by name, or raw GQL.
#[derive(Debug, Clone, Default)]
pub struct AnalyzeParams {
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
    /// Render a "by file" concentration summary above the table. The summary
    /// is computed over every matched row (not just the visible window), so
    /// it shows where the mass is — e.g. `dead_code`'s top rows are usually
    /// all route handlers in one file, and the summary makes that obvious
    /// before the reader scrolls the suspects.
    pub by_folder: bool,
}

/// Population of one property across the store.
#[derive(Debug, Clone)]
pub struct Coverage {
    pub property: String,
    pub present: usize,
    pub total: usize,
}

impl Coverage {
    /// A property no node carries, *in a store that has nodes*. Every
    /// predicate on it matched nothing, and the query still returned a
    /// number — this is the case the whole coverage contract exists to catch.
    ///
    /// The `total > 0` guard matters: `total` is `count(*)` over the store,
    /// so an un-ingested project makes `present == 0` for every property at
    /// once. Reporting that as "this property is not indexed" blames the
    /// schema for an empty index and sends the caller to `ug gen`, which
    /// does not ingest — see [`Coverage::index_is_empty`].
    pub fn is_absent(&self) -> bool {
        self.present == 0 && self.total > 0
    }

    /// The store answered the coverage probe with zero nodes: nothing has
    /// been ingested for this project yet.
    pub fn index_is_empty(&self) -> bool {
        self.total == 0
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
    /// The store holds no nodes at all — the project was generated with
    /// `--no-ingest`, or its ingest failed. Distinguished from `unindexed`
    /// because the remedy is different (`ug ingest`, not `ug gen`) and
    /// because every property looks absent in this state, which would
    /// otherwise be reported as a schema problem.
    pub empty_index: bool,
    /// Anchors the preset bound to values the store has no node for. The
    /// query matched nothing, but the empty answer is a caller typo (or paths
    /// ingest never indexed), not a real zero. Populated for `target` (one
    /// file), `files` (the diff_* presets — each missing path listed) and
    /// `symbol` (test_for) when the result is empty; raw GQL leaves it empty.
    pub target_not_indexed: Vec<String>,
    /// A `target` given as a symbol and answered as its file: the symbol as
    /// asked for, and the path actually queried.
    ///
    /// The `TARGET` presets anchor on `n.file`, but "what does a change to
    /// this reach" is a question people ask about a *function*. Silently
    /// answering a different question would be worse than the old refusal,
    /// so the substitution is carried out here and stated in the output.
    pub target_resolved_from: Option<(String, String)>,
    /// The window of rows to render. Every count reported alongside the
    /// table is over the *whole* result, not this window.
    pub window: range::RowRange,
    /// The GQL that ran, echoed for a preset so the caller can adapt it.
    pub gql: String,
    /// Whether the query came from the preset registry rather than the
    /// caller. Changes which engine warnings are worth repeating — see
    /// `render::is_expected_noise`.
    pub from_preset: bool,
    /// Whether to render the "by file" concentration summary. Copied from
    /// [`AnalyzeParams::by_folder`] so the renderer needs no extra arg.
    pub by_folder: bool,
    /// What to run next, from the preset that produced this answer.
    ///
    /// An answer is rarely the end of the question, and the useful follow-up
    /// is often one nobody would guess — see [`presets::Preset::next`]. Empty
    /// for raw GQL, which has no preset to ask.
    pub next: &'static [(&'static str, &'static str)],
}

use ultragraph::agent_tools::DEFAULT_ROWS as DEFAULT_LIMIT;

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
    "extension",
    "is_test",
    "in_degree",
    "out_degree",
    "name_mentions",
    "qualified_name",
    "route",
    "annotations",
    "boundary",
    "boundary_in",
    "boundary_out",
    "boundary_kinds",
    "boundary_protocols",
    "boundary_detail",
];

/// Resolve, execute and annotate one query.
pub async fn run(
    store: &dyn KnowledgeStore,
    params: &AnalyzeParams,
) -> Result<QueryAnswer, String> {
    let (title, description, gql, mut bound) = resolve(params)?;

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

    // A `target` that cannot be a path is almost certainly a symbol, and
    // "what does a change to `index_with_cache` reach" is the question
    // people actually bring to these presets. Resolve it to the file the
    // query can anchor on, before the query rather than after, so the answer
    // is about what was asked instead of a warning that it was not a file.
    let mut target_resolved_from = None;
    if let Some(QueryValue::Str(target)) = bound.get("target") {
        let target = target.clone();
        if !looks_like_path(&target) {
            if let Some(file) = symbol_target_file(store, &target, &limits).await {
                bound.insert("target".to_string(), QueryValue::Str(file.clone()));
                target_resolved_from = Some((target, file));
            }
        }
    }

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
    // Every entry carries the same `count(*)`, so any one of them answers
    // this. An empty `coverage` means the probe itself failed, which is a
    // different (already-reported) problem — not an empty index.
    let empty_index = !coverage.is_empty() && coverage.iter().all(|c| c.index_is_empty());

    // A target bound to a value the store has no node for is a caller typo,
    // not an answer. The presets that anchor on a path filter on `t.file`, so
    // zero rows from a typo'd path is indistinguishable from "nothing depends
    // on it" — probe once for the anchor, only on an empty result, so the
    // renderer can say which. Covers three shapes: `target` (one file),
    // `files` (the diff_* list — each missing path named) and `symbol`
    // (test_for — elementKey or name lookup).
    let target_not_indexed = if empty_index || !page.rows.is_empty() {
        Vec::new()
    } else {
        missing_anchors(store, &bound, &limits).await
    };

    Ok(QueryAnswer {
        // Looked up again rather than threaded through `resolve`, whose
        // four-tuple is already at the limit of what reads clearly.
        next: params
            .preset
            .as_deref()
            .and_then(presets::find)
            .map(|p| p.next)
            .unwrap_or(&[]),
        title,
        description,
        page,
        coverage,
        unindexed,
        empty_index,
        target_not_indexed,
        target_resolved_from,
        window,
        gql,
        from_preset: params.preset.is_some(),
        by_folder: params.by_folder,
    })
}

/// Which of the preset's anchor values the store has no node for. Runs only
/// when a target/files/symbol query came back empty, so each probe is one
/// cheap anchored query — never a full scan. A failure here costs nothing:
/// best-effort, returning empty (no caveat) rather than failing the answer.
async fn missing_anchors(
    store: &dyn KnowledgeStore,
    bound: &QueryParams,
    limits: &QueryLimits,
) -> Vec<String> {
    let mut missing = Vec::new();

    // Single file: `target` (impact, retest_scope, boundary_impact, …).
    if let Some(QueryValue::Str(target)) = bound.get("target") {
        // Not indexed means *nothing* carries the name — not merely that it
        // is not a file. A symbol target has already been rewritten to its
        // file by the time this runs, so anything still failing the file
        // probe gets one more chance as a symbol before being called a typo.
        // Reporting `index_with_cache` as "never ingested" when the function
        // is right there is worse than saying nothing: it is the one caveat
        // the tool tells callers to trust absolutely.
        if matches!(target_file_indexed(store, target, limits).await, Ok(false))
            && matches!(symbol_indexed(store, target, limits).await, Ok(false))
        {
            missing.push(target.clone());
        }
    }

    // List of files: `files` (diff_impact, diff_retest_scope). Probe each,
    // because "one of the five changed paths was a typo" is a different
    // diagnosis from "nothing depends on any of them".
    if let Some(QueryValue::List(items)) = bound.get("files") {
        for v in items {
            if let QueryValue::Str(f) = v {
                if matches!(target_file_indexed(store, f, limits).await, Ok(false)) {
                    missing.push(f.clone());
                }
            }
        }
    }

    // Symbol id: `symbol` (test_for). Match either the node key or the bare
    // name, since a caller may pass either — but only flag when neither
    // resolves, so a name that matches nothing reads as a typo, not "untested".
    if let Some(QueryValue::Str(symbol)) = bound.get("symbol") {
        if matches!(symbol_indexed(store, symbol, limits).await, Ok(false)) {
            missing.push(symbol.clone());
        }
    }

    missing
}

/// Whether any indexed node carries `file = $target`, the shape every
/// `TARGET` preset anchors on.
async fn target_file_indexed(
    store: &dyn KnowledgeStore,
    target: &str,
    limits: &QueryLimits,
) -> Result<bool, String> {
    let mut params = QueryParams::new();
    params.insert("target".to_string(), QueryValue::Str(target.to_string()));
    let probe = "MATCH (n) WHERE n.file = $target RETURN n.file AS file LIMIT 1";
    store
        .execute_query(probe, &params, limits)
        .await
        .map(|p| !p.rows.is_empty())
        .map_err(|e| e.to_string())
}

/// Whether any indexed node matches `symbol` — by elementKey (a node id) or
/// by bare name. `test_for` accepts both, and an empty result is a typo only
/// when neither resolves.
async fn symbol_indexed(
    store: &dyn KnowledgeStore,
    symbol: &str,
    limits: &QueryLimits,
) -> Result<bool, String> {
    let mut params = QueryParams::new();
    params.insert("symbol".to_string(), QueryValue::Str(symbol.to_string()));
    let probe = "MATCH (n) WHERE elementKey(n) = $symbol OR n.name = $symbol \
                 RETURN elementKey(n) AS id LIMIT 1";
    store
        .execute_query(probe, &params, limits)
        .await
        .map(|p| !p.rows.is_empty())
        .map_err(|e| e.to_string())
}

/// The file a symbol lives in, by node id or by bare name.
///
/// Only ever consulted for a `target` that cannot be a path, and only
/// returns a file when exactly one symbol answers to the name — two
/// functions called `parse` give no basis for picking one, and quietly
/// analysing whichever came first is how a blast radius becomes fiction.
async fn symbol_target_file(
    store: &dyn KnowledgeStore,
    target: &str,
    limits: &QueryLimits,
) -> Option<String> {
    let mut params = QueryParams::new();
    params.insert("target".to_string(), QueryValue::Str(target.to_string()));
    let probe = "MATCH (n) WHERE elementKey(n) = $target OR n.name = $target \
                 RETURN DISTINCT n.file AS file LIMIT 2";
    let page = store.execute_query(probe, &params, limits).await.ok()?;
    if page.rows.len() != 1 {
        return None;
    }
    match page.rows.first()?.first()? {
        QueryValue::Str(file) if !file.is_empty() => Some(file.clone()),
        _ => None,
    }
}

/// Whether `target` is shaped like a repo path rather than a symbol name.
///
/// A path has a directory separator or a file extension; `index_with_cache`
/// has neither. Wrong only in the harmless direction: a misjudged path
/// costs one extra probe that finds nothing.
fn looks_like_path(target: &str) -> bool {
    if target.contains('/') {
        return true;
    }
    match target.rsplit_once('.') {
        // An extension is short and alphanumeric. `Db.open` is not a path.
        Some((_, ext)) => {
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Turn the request into a query and its bound parameters.
fn resolve(
    params: &AnalyzeParams,
) -> Result<(String, Option<String>, String, QueryParams), String> {
    match (&params.preset, &params.gql) {
        (Some(_), Some(_)) => Err(
            "Pass either `preset` or `gql`, not both — a preset is already a query.".to_string(),
        ),
        (None, None) => Err(format!(
            "analyze needs a `preset` or a `gql` query.\n\nAvailable presets: {}",
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
                let value = if spec.list {
                    // A list parameter arrives as one comma- or
                    // newline-separated string (the natural shape for
                    // `--arg files=a.ts,b.rs` and for a model's JSON) and
                    // binds as a GQL list so `IN $files` works. An empty
                    // list is rejected: an `IN` over nothing matches
                    // nothing, and the empty answer would read as "no
                    // dependents" — the false zero the coverage contract
                    // exists to prevent.
                    let items: Vec<QueryValue> = raw
                        .split([',', '\n'])
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| QueryValue::Str(s.to_string()))
                        .collect();
                    if items.is_empty() {
                        return Err(format!(
                            "`{}` requires at least one path in `{}` — the value was empty.",
                            preset.name, spec.name
                        ));
                    }
                    QueryValue::List(items)
                } else {
                    match spec.default {
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
                    }
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
///
/// Shallow, but not blind to strings. A dot inside a quoted literal is
/// data, and the data this tool is pointed at is mostly *paths* — so
/// `WHERE n.file ENDS WITH "router_tests.rs"` used to probe a property
/// named `rs`, find nothing carrying it, and warn "NOT INDEXED: rs — this
/// answer is not about what you asked" over an answer that was entirely
/// correct. A false alarm on this particular warning is expensive: it is
/// the one that tells a caller to distrust a number, so crying wolf
/// teaches them to ignore the real thing.
fn referenced_properties(gql: &str) -> Vec<String> {
    let bytes = gql.as_bytes();
    let mut found: Vec<String> = Vec::new();
    // Which quote we are inside, if any. Backticks count: Cypher quotes
    // odd identifiers with them, and a dot in there is part of the name,
    // not an access.
    let mut quote: Option<u8> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            // A backslash escapes the next byte, including the closing
            // quote — `'` inside a single-quoted literal does not end it.
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if matches!(b, b'\'' | b'"' | b'`') {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b != b'.' {
            i += 1;
            continue;
        }

        // A dot inside `*1..3` or a decimal literal is not a property
        // access either. Requiring an identifier character on the left and
        // an alphabetic one on the right rules both out.
        let left_ok = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !left_ok {
            i += 1;
            continue;
        }
        let name: String = gql[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        i += 1;
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
            "\n\nanalyze is read-only. It answers questions about the graph; \
             it cannot modify the index (use `gen` for that).",
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
    } else if err.contains("IN requires a list") {
        // Observed: a model asked for `'cli.command' IN n.boundary_kinds`,
        // which is the right idea against the wrong shape. The multi-valued
        // boundary properties are comma-joined strings, not lists, and the
        // engine's own message says nothing about which property or what to
        // use instead.
        out.push_str(
            "\n\nA multi-valued property here is a comma-joined STRING, not a list \
— `boundary_kinds` and `boundary_protocols` both are. Use `CONTAINS` against it rather than `IN`: \
`WHERE n.boundary_kinds CONTAINS 'cli.command'`. (`IN` is for a literal list on the right: \
`WHERE n.file IN ['a.rs', 'b.rs']`.)\n\nFor boundaries specifically you do not need GQL at all — \
`analyze boundaries --arg kind=cli.command --arg direction=inbound` answers it directly.",
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
    /// The failure a model actually hits, and what it must be told.
    ///
    /// `'cli.command' IN n.boundary_kinds` is the right idea against the wrong
    /// shape: the multi-valued boundary properties are comma-joined strings.
    /// The engine says only "IN requires a list right-hand operand", which
    /// names neither the property nor the fix.
    #[test]
    fn an_in_against_a_joined_string_is_explained() {
        let out = explain_failure(
            "backend error: overgraph: invalid operation: graph row IN requires a list right-hand operand",
            "MATCH (n) WHERE 'cli.command' IN n.boundary_kinds RETURN n",
        );
        assert!(out.contains("CONTAINS"), "{out}");
        assert!(out.contains("boundary_kinds"), "{out}");
        // …and the answer that needs no GQL at all.
        assert!(out.contains("analyze boundaries --arg kind="), "{out}");
    }

    #[test]
    fn the_boundaries_preset_can_be_filtered() {
        let p = presets::find("boundaries").expect("boundaries exists");
        let names: Vec<&str> = p.params.iter().map(|q| q.name).collect();
        assert_eq!(names, vec!["kind", "direction"]);
        // Both optional: `analyze boundaries` on its own must keep working.
        assert!(p.params.iter().all(|q| q.default.is_some()), "filters must default to everything");
        // Substring, not equality — a symbol can carry several kinds, and `=`
        // silently drops every one that does.
        assert!(p.gql.contains("boundary_kinds CONTAINS $kind"), "{}", p.gql);
        assert!(p.gql.contains("$direction = ''"), "an unset direction must match both: {}", p.gql);
    }

    use super::*;

    fn params(preset: &str) -> AnalyzeParams {
        AnalyzeParams {
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

    /// A list parameter splits a comma-separated string into a GQL list, so
    /// `IN $files` works. Newlines split too (the shape `git diff --name-only`
    /// produces); blanks are dropped.
    #[test]
    fn a_list_parameter_splits_into_a_gql_list() {
        let mut p = params("diff_impact");
        p.args.insert("files".into(), "src/a.ts, src/b.rs\nc/ignored,,\n".into());
        let (_, _, _, bound) = resolve(&p).unwrap();
        match &bound["files"] {
            QueryValue::List(items) => {
                let names: Vec<&str> = items.iter().filter_map(|v| match v {
                    QueryValue::Str(s) => Some(s.as_str()),
                    _ => None,
                }).collect();
                assert_eq!(names, vec!["src/a.ts", "src/b.rs", "c/ignored"]);
            }
            other => panic!("expected a list, got {:?}", other),
        }
    }

    /// An empty list is rejected outright: `IN` over nothing matches nothing,
    /// and the empty answer would read as "no dependents" — the false zero
    /// the whole coverage contract exists to prevent.
    #[test]
    fn an_empty_list_parameter_is_rejected() {
        let mut p = params("diff_impact");
        p.args.insert("files".into(), ",,\n ".into());
        let err = resolve(&p).unwrap_err();
        assert!(err.contains("at least one path"), "{err}");
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
        let p = AnalyzeParams {
            preset: Some("repo_census".into()),
            gql: Some("MATCH (n) RETURN count(*)".into()),
            ..Default::default()
        };
        assert!(resolve(&p).unwrap_err().contains("not both"));
    }

    #[test]
    fn neither_preset_nor_gql_lists_the_presets() {
        let err = resolve(&AnalyzeParams::default()).unwrap_err();
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

    /// The bug: the scan walked every `.` in the raw query, so a *path* in
    /// a string literal read as a property access. `n.file = "router_tests.rs"`
    /// probed a property called `rs`, found nothing carrying it, and warned
    /// "NOT INDEXED: rs — this answer is not about what you asked" over an
    /// answer that was exactly what was asked.
    #[test]
    fn a_path_inside_a_string_literal_is_not_a_property() {
        let props = referenced_properties(
            r#"MATCH (n) WHERE n.file ENDS WITH "router_tests.rs" RETURN sum(n.is_test) AS t"#,
        );
        assert_eq!(props, vec!["file".to_string(), "is_test".to_string()], "{props:?}");

        // Single quotes are the same literal, and the presets use them.
        let props = referenced_properties(
            "MATCH (n) WHERE n.file IN ['src/a.ts', 'src/b.rs'] RETURN n.folder AS f",
        );
        assert_eq!(props, vec!["file".to_string(), "folder".to_string()], "{props:?}");
    }

    #[test]
    fn a_quote_escaped_inside_a_literal_does_not_end_it() {
        // If the `\'` were read as the closing quote, `don` would leave the
        // string and `t.rs` would register as a property named `rs`.
        let props = referenced_properties(
            r"MATCH (n) WHERE n.name = 'don\'t.rs' RETURN count(*) AS c",
        );
        assert_eq!(props, vec!["name".to_string()], "{props:?}");
    }

    #[test]
    fn a_backticked_identifier_hides_its_dots_too() {
        let props = referenced_properties(
            "MATCH (n) WHERE n.`odd.name` = 1 RETURN n.loc AS loc",
        );
        // `odd.name` contributes nothing; only the real accesses do.
        assert_eq!(props, vec!["loc".to_string()], "{props:?}");
    }

    /// The scan must still find everything it did before — skipping strings
    /// is a narrowing, and a narrowing that goes too far silently drops the
    /// coverage caveat this whole module exists to print.
    #[test]
    fn skipping_strings_does_not_lose_real_accesses() {
        let props = referenced_properties(
            r#"MATCH (dep)-[:Calls*1..3]->(t) WHERE t.file IN ["a.rs"] AND dep.is_test = 1
               RETURN dep.folder AS f, count(*) AS c"#,
        );
        assert!(props.contains(&"folder".to_string()), "{props:?}");
        assert!(props.contains(&"file".to_string()), "{props:?}");
        assert!(props.contains(&"is_test".to_string()), "{props:?}");
        assert!(!props.contains(&"rs".to_string()), "{props:?}");
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

    /// In an empty store every property has `present == 0`, but none of them
    /// is "absent" in the sense the caveat means — the index simply has no
    /// nodes. Without the `total > 0` guard, `unindexed` fills up with every
    /// property the query touched and reports a schema problem that isn't one.
    #[test]
    fn an_empty_store_makes_no_property_absent() {
        for property in ["node_type", "loc", "has_doc"] {
            let c = Coverage {
                property: property.into(),
                present: 0,
                total: 0,
            };
            assert!(
                !c.is_absent(),
                "{property}: an empty index is not a missing property"
            );
            assert!(c.index_is_empty(), "{property}: should report the empty index");
        }
    }
}
