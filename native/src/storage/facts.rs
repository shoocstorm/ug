//! Queryable per-node facts, derived once at ingest and stored as
//! properties.
//!
//! Everything a whole-repo statistical question needs — "how many methods
//! are longer than 50 lines", "which symbols does nothing call" — has to be
//! a *stored property*, because that is the only thing a query language can
//! filter and aggregate on. `graph.json` carries these facts today and the
//! store dropped them, which is why the store could answer "find me
//! something like X" but not "how many X are there".
//!
//! Two rules shape what belongs here:
//!
//! 1. **Derivable per node, once.** A fact that needs the whole graph
//!    (degrees) is fine because it is computed once into a [`FactContext`];
//!    a fact that needs a second query at read time is not.
//! 2. **Booleans are stored as `0`/`1` integers.** GQL has no boolean
//!    aggregate, so "what fraction has docs" is `sum(has_doc) / count(*)`.
//!    Storing `true`/`false` would make the most common shape of question
//!    impossible to express.

use crate::types::{FileClassification, GraphData, GraphEdgeType, GraphNode, GraphNodeType};
use std::collections::{BTreeMap, HashMap};

/// A stored fact value, in the small set of shapes every backend can hold.
///
/// Deliberately not `overgraph::PropValue`: `NodeRow` is the
/// backend-portable DTO and Neo4j has to be able to write these too.
#[derive(Debug, Clone, PartialEq)]
pub enum FactValue {
    Int(i64),
    Str(String),
}

impl FactValue {
    /// Booleans enter the store as 0/1 so they can be summed. See the
    /// module note.
    pub fn from_bool(b: bool) -> Self {
        FactValue::Int(if b { 1 } else { 0 })
    }
}

/// Facts attached to one node, keyed by property name.
pub type Facts = BTreeMap<String, FactValue>;

/// Graph-wide context needed by facts that are not local to a node.
///
/// Built once per ingest so `compute` stays O(1) per node.
pub struct FactContext {
    /// Inbound edges excluding `Contains`, i.e. "how much code depends on
    /// this". `Contains` is pure structure (folder→file→symbol) and would
    /// give every symbol an in-degree of 1 for free, drowning the signal.
    in_degree: HashMap<String, u32>,
    /// Outbound edges, same exclusion.
    out_degree: HashMap<String, u32>,
    /// Outbound `Contains` edges from a type to the members it declares.
    ///
    /// The one place `Contains` is the signal rather than the noise. Only
    /// meaningful for languages whose class body encloses its members —
    /// Java has 451 such edges in the bundled sample; Rust has none,
    /// because `impl` blocks sit outside the struct they extend. See
    /// [`compute`] for how that asymmetry is kept honest.
    members: HashMap<String, u32>,
    /// Whether this graph was written by a build that records comment
    /// metrics. A graph older than that answers "how many functions have
    /// comments" with zero, which is worse than refusing.
    has_line_metrics: bool,
}

impl FactContext {
    pub fn new(graph: &GraphData) -> Self {
        let mut in_degree: HashMap<String, u32> = HashMap::new();
        let mut out_degree: HashMap<String, u32> = HashMap::new();
        let mut members: HashMap<String, u32> = HashMap::new();
        for e in &graph.edges {
            if matches!(e.edge_type, GraphEdgeType::Contains) {
                *members.entry(e.source.clone()).or_insert(0) += 1;
                continue;
            }
            *in_degree.entry(e.target.clone()).or_insert(0) += 1;
            *out_degree.entry(e.source.clone()).or_insert(0) += 1;
        }
        let has_line_metrics = graph
            .stats
            .as_ref()
            .map(|s| s.graph_schema_version >= 2)
            .unwrap_or(false);
        Self {
            in_degree,
            out_degree,
            members,
            has_line_metrics,
        }
    }
}

/// Path segments that mark a file as test code.
///
/// Now the *fallback*, not the rule: the indexer's `FileClassification`
/// reaches `GraphNode` and takes precedence where it exists (see
/// [`compute`]). This still matters for graphs written before that landed,
/// which is the only case left — every `FileClassification` variant is a
/// decision, so a node that has one never reaches here.
///
/// Every marker is anchored — on a `/` (start of a path segment) or on the
/// `.`/`_` that delimits a filename suffix. An unanchored `test_` matched
/// mid-word and swept in production code: `latest_version.rs`,
/// `greatest_hits.rs`, `fastest_path.ts`, `contest_rules.py` all contain
/// `test_` and all read as test code. Anchoring to `/test_` keeps the real
/// case (`src/test_thing.py`, where the marker starts a segment) and drops
/// the false ones.
const TEST_PATH_MARKERS: &[&str] = &[
    "/test/",
    "/tests/",
    "/__tests__/",
    "/spec/",
    "/test_",
    "_test.",
    ".test.",
    ".spec.",
    "_spec.",
];

fn looks_like_test(file: &str) -> bool {
    // Leading separator so a top-level `tests/` directory matches the same
    // `/tests/` marker as a nested one, without a second set of patterns.
    let probe = format!("/{}", file);
    let lower = probe.to_ascii_lowercase();
    TEST_PATH_MARKERS.iter().any(|m| lower.contains(m))
}

/// Stable lowercase name for a file classification.
///
/// Spelled out rather than derived from `Debug`, so a rename of the enum
/// variant cannot silently change a stored property that queries and
/// saved presets filter on.
fn classification_str(c: &FileClassification) -> &'static str {
    match c {
        FileClassification::Component => "component",
        FileClassification::Page => "page",
        FileClassification::Hook => "hook",
        FileClassification::Util => "util",
        FileClassification::Service => "service",
        FileClassification::Config => "config",
        FileClassification::Type => "type",
        FileClassification::Constant => "constant",
        FileClassification::Context => "context",
        FileClassification::Reducer => "reducer",
        FileClassification::Test => "test",
        FileClassification::Asset => "asset",
        FileClassification::Documentation => "documentation",
    }
}

/// Parent directory of a repo-relative file path, `""` for a file at the
/// repo root. Used to group statistics by module without a query-time
/// string function.
fn folder_of(file: &str) -> &str {
    match file.rfind('/') {
        Some(ix) => &file[..ix],
        None => "",
    }
}

/// Lines the node spans, inclusive.
///
/// Prefers the indexer's `metrics.loc` and falls back to the line range.
/// Both are inclusive of their first and last line, so a Function (which
/// has metrics) and a Concept (which may not) are comparable.
///
/// This is a *span*: it counts blank and comment lines. The `code_lines`
/// fact is the one to use when you mean "lines of code" — on commented
/// code the two differ by roughly 30%.
fn span_loc(n: &GraphNode) -> Option<u32> {
    if let Some(m) = &n.metrics {
        return Some(m.loc);
    }
    match (n.start_line, n.end_line) {
        (Some(s), Some(e)) if e >= s => Some(e - s + 1),
        _ => None,
    }
}

/// Derive every stored fact for one node.
pub fn compute(n: &GraphNode, ctx: &FactContext) -> Facts {
    let mut f = Facts::new();

    if let Some(loc) = span_loc(n) {
        f.insert("loc".into(), FactValue::Int(loc as i64));
    }
    if let Some(m) = &n.metrics {
        f.insert("params".into(), FactValue::Int(m.params as i64));
        f.insert("max_nesting".into(), FactValue::Int(m.max_nesting as i64));

        // Only when the graph is new enough to actually carry them.
        // Writing `comment_lines = 0` from a graph indexed before the
        // metric existed would produce the exact failure this design
        // exists to prevent: a confident zero that reads as a measurement.
        // Omitted, the property shows up as NOT INDEXED in every answer's
        // coverage line, which tells the caller to reindex.
        if ctx.has_line_metrics {
            f.insert("comment_lines".into(), FactValue::Int(m.comment_lines as i64));
            f.insert("doc_lines".into(), FactValue::Int(m.doc_lines as i64));
            f.insert("code_lines".into(), FactValue::Int(m.code_lines as i64));
            f.insert(
                "has_comments".into(),
                FactValue::from_bool(m.comment_lines > 0 || m.doc_lines > 0),
            );
        }
    }

    f.insert(
        "has_doc".into(),
        FactValue::from_bool(n.docstring.as_deref().is_some_and(|d| !d.trim().is_empty())),
    );

    if let Some(lang) = n.language.as_deref().filter(|s| !s.is_empty()) {
        f.insert("language".into(), FactValue::Str(lang.to_string()));
    }
    if let Some(c) = &n.classification {
        f.insert(
            "classification".into(),
            FactValue::Str(classification_str(c).to_string()),
        );
    }

    // Members are only recorded where the graph genuinely has them. A
    // Rust struct's methods live in a separate `impl` block, so it has no
    // `Contains` edges and gets no `members` fact — absent rather than a
    // zero that would rank every Rust type as memberless. The coverage
    // line makes the partial population visible.
    if matches!(n.node_type, GraphNodeType::Class | GraphNodeType::Interface) {
        if let Some(count) = ctx.members.get(&n.id).copied().filter(|c| *c > 0) {
            f.insert("members".into(), FactValue::Int(count as i64));
        }
    }

    // Folder and is_test are only meaningful for nodes that live in a
    // file. Folder nodes carry their own path in `file`, which would make
    // `folder` self-referential, so they are excluded.
    if !matches!(n.node_type, GraphNodeType::Folder) {
        if let Some(file) = n.file.as_deref().filter(|s| !s.is_empty()) {
            f.insert("folder".into(), FactValue::Str(folder_of(file).to_string()));
            // The indexer's classification is authoritative where it
            // exists — it saw the file's contents, not just its name. The
            // path heuristic stays as the fallback for graphs written
            // before the classification reached the node, and for files
            // the classifier had no opinion about.
            // `Some(_)` used to fall through to the path heuristic, which
            // made this comment a lie in the negative direction: a file the
            // classifier positively identified as `Util` or `Service` could
            // still be relabelled a test by its *name*. `FileClassification`
            // has no "unknown" variant — every variant is a decision — so
            // `Some(c)` is exactly "the classifier had an opinion" and its
            // answer stands either way. `None` remains the genuine fallback
            // for graphs written before classification reached the node.
            let is_test = match &n.classification {
                Some(c) => *c == FileClassification::Test,
                None => looks_like_test(file),
            };
            f.insert("is_test".into(), FactValue::from_bool(is_test));
        }
    }

    f.insert(
        "in_degree".into(),
        FactValue::Int(ctx.in_degree.get(&n.id).copied().unwrap_or(0) as i64),
    );
    f.insert(
        "out_degree".into(),
        FactValue::Int(ctx.out_degree.get(&n.id).copied().unwrap_or(0) as i64),
    );

    if let Some(q) = n.qualified_name.as_deref().filter(|s| !s.is_empty()) {
        f.insert("qualified_name".into(), FactValue::Str(q.to_string()));
    }
    if let Some(r) = n.route.as_deref().filter(|s| !s.is_empty()) {
        f.insert("route".into(), FactValue::Str(r.to_string()));
    }
    if !n.annotations.is_empty() {
        // Joined rather than nested: the store holds scalars, and the
        // shape queries actually want is "does this contain X".
        let names: Vec<&str> = n.annotations.iter().map(|a| a.name.as_str()).collect();
        f.insert("annotations".into(), FactValue::Str(names.join(",")));
    }

    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Annotation, GraphEdge, SymbolMetrics};

    fn node(id: &str, file: Option<&str>) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: id.to_string(),
            node_type: GraphNodeType::Function,
            file: file.map(String::from),
            ..Default::default()
        }
    }

    /// A context from a graph with **no** stats block, i.e. one whose
    /// schema version is unknown and therefore pre-v2.
    fn ctx_of(edges: Vec<GraphEdge>) -> FactContext {
        FactContext::new(&GraphData {
            nodes: vec![],
            edges,
            stats: None,
            resolution: None,
        })
    }

    /// A context from a graph stamped with the given schema version.
    fn ctx_at_version(version: u32, edges: Vec<GraphEdge>) -> FactContext {
        FactContext::new(&GraphData {
            nodes: vec![],
            edges,
            stats: Some(crate::types::IndexStats {
                graph_schema_version: version,
                total_files: 0,
                cached_files: 0,
                total_symbols: 0,
                total_folders: 0,
                total_lines: 0,
                indexing_time_ms: 0,
                last_indexed_at: 0,
                repo_root: String::new(),
            }),
            resolution: None,
        })
    }

    fn with_line_metrics(comment: u32, doc: u32, code: u32) -> Option<SymbolMetrics> {
        Some(SymbolMetrics {
            loc: 40,
            params: 1,
            max_nesting: 2,
            comment_lines: comment,
            doc_lines: doc,
            code_lines: code,
        })
    }

    fn edge(source: &str, target: &str, edge_type: GraphEdgeType) -> GraphEdge {
        GraphEdge {
            source: source.into(),
            target: target.into(),
            edge_type,
        }
    }

    #[test]
    fn loc_prefers_metrics_but_falls_back_to_the_line_span() {
        let mut n = node("f", Some("src/a.rs"));
        n.start_line = Some(10);
        n.end_line = Some(20);
        let f = compute(&n, &ctx_of(vec![]));
        // 10..=20 inclusive is 11 lines, not 10.
        assert_eq!(f["loc"], FactValue::Int(11));

        n.metrics = Some(SymbolMetrics {
            loc: 7,
            params: 2,
            max_nesting: 1,
            ..Default::default()
        });
        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["loc"], FactValue::Int(7), "metrics win over the span");
        assert_eq!(f["params"], FactValue::Int(2));
        assert_eq!(f["max_nesting"], FactValue::Int(1));
    }

    /// Class nodes carry no metrics, so without the span fallback every
    /// "how big are the classes" question would return nothing.
    #[test]
    fn nodes_without_metrics_still_get_a_size() {
        let mut n = node("C", Some("src/a.rs"));
        n.node_type = GraphNodeType::Class;
        n.start_line = Some(1);
        n.end_line = Some(50);
        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["loc"], FactValue::Int(50));
    }

    #[test]
    fn a_node_with_no_line_range_has_no_loc() {
        let f = compute(&node("f", Some("src/a.rs")), &ctx_of(vec![]));
        assert!(!f.contains_key("loc"), "absent, not zero");
    }

    #[test]
    fn booleans_are_stored_as_ints_so_they_can_be_summed() {
        let mut n = node("f", Some("src/a.rs"));
        assert_eq!(compute(&n, &ctx_of(vec![]))["has_doc"], FactValue::Int(0));
        n.docstring = Some("what it does".into());
        assert_eq!(compute(&n, &ctx_of(vec![]))["has_doc"], FactValue::Int(1));
    }

    #[test]
    fn whitespace_only_docstrings_do_not_count_as_documentation() {
        let mut n = node("f", Some("src/a.rs"));
        n.docstring = Some("   \n  ".into());
        assert_eq!(compute(&n, &ctx_of(vec![]))["has_doc"], FactValue::Int(0));
    }

    #[test]
    fn test_files_are_detected_at_any_depth_including_the_repo_root() {
        for path in [
            "tests/foo.rs",
            "native/tests/foo.rs",
            "src/__tests__/a.ts",
            "src/a.test.ts",
            "src/a_test.go",
            "src/test_thing.py",
        ] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f["is_test"], FactValue::Int(1), "{path} should read as test");
        }
        for path in ["src/latest.rs", "src/contest.ts", "src/a.rs"] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(
                f["is_test"],
                FactValue::Int(0),
                "{path} should not read as test"
            );
        }
    }

    #[test]
    fn folder_is_the_parent_dir_and_empty_at_the_repo_root() {
        let f = compute(&node("f", Some("a/b/c.rs")), &ctx_of(vec![]));
        assert_eq!(f["folder"], FactValue::Str("a/b".into()));
        let f = compute(&node("f", Some("c.rs")), &ctx_of(vec![]));
        assert_eq!(f["folder"], FactValue::Str("".into()));
    }

    /// `Contains` is folder→file→symbol structure. Counting it would give
    /// every symbol in the repo an in-degree of at least 1 and make
    /// "what does nothing depend on" answer "nothing".
    #[test]
    fn degrees_ignore_contains_edges() {
        let ctx = ctx_of(vec![
            edge("file:a", "f", GraphEdgeType::Contains),
            edge("caller", "f", GraphEdgeType::Calls),
            edge("other", "f", GraphEdgeType::References),
            edge("f", "callee", GraphEdgeType::Calls),
        ]);
        let f = compute(&node("f", Some("src/a.rs")), &ctx);
        assert_eq!(f["in_degree"], FactValue::Int(2), "Calls + References only");
        assert_eq!(f["out_degree"], FactValue::Int(1));
    }

    #[test]
    fn unreferenced_nodes_report_zero_rather_than_nothing() {
        let f = compute(&node("f", Some("src/a.rs")), &ctx_of(vec![]));
        // Absent would make `where in_degree = 0` (dead-code sweeps) miss
        // exactly the nodes it is looking for.
        assert_eq!(f["in_degree"], FactValue::Int(0));
        assert_eq!(f["out_degree"], FactValue::Int(0));
    }

    #[test]
    fn optional_java_facts_appear_only_when_present() {
        let mut n = node("f", Some("src/A.java"));
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("qualified_name"));
        assert!(!f.contains_key("route"));
        assert!(!f.contains_key("annotations"));

        n.qualified_name = Some("com.x.A#f".into());
        n.route = Some("GET /a".into());
        n.annotations = vec![
            Annotation {
                name: "Test".into(),
                args: None,
            },
            Annotation {
                name: "Override".into(),
                args: None,
            },
        ];
        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["qualified_name"], FactValue::Str("com.x.A#f".into()));
        assert_eq!(f["route"], FactValue::Str("GET /a".into()));
        assert_eq!(f["annotations"], FactValue::Str("Test,Override".into()));
    }

    /// The single most important behaviour in this module.
    ///
    /// A graph indexed before comment metrics existed has `comment_lines:
    /// 0` on every symbol, because that is what `#[serde(default)]` does.
    /// Storing that would answer "how many functions have comments" with a
    /// confident, wrong zero. Omitting it makes the property report as NOT
    /// INDEXED in every answer's coverage line instead.
    #[test]
    fn line_metrics_are_omitted_on_a_graph_too_old_to_have_them() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(0, 0, 0);

        let old = compute(&n, &ctx_of(vec![]));
        for key in ["comment_lines", "doc_lines", "code_lines", "has_comments"] {
            assert!(
                !old.contains_key(key),
                "{key} must be absent, not zero, on a pre-v2 graph"
            );
        }
        // Facts that always existed are unaffected.
        assert_eq!(old["params"], FactValue::Int(1));
    }

    #[test]
    fn line_metrics_are_stored_on_a_current_graph() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(6, 3, 22);

        let f = compute(&n, &ctx_at_version(2, vec![]));
        assert_eq!(f["comment_lines"], FactValue::Int(6));
        assert_eq!(f["doc_lines"], FactValue::Int(3));
        assert_eq!(f["code_lines"], FactValue::Int(22));
        assert_eq!(f["has_comments"], FactValue::Int(1));
    }

    /// `has_doc` and `has_comments` measure different things, and the gap
    /// between them is usually the finding: a function explained entirely
    /// in inline comments is undocumented by one measure and commented by
    /// the other.
    #[test]
    fn inline_comments_count_as_commented_but_not_as_documented() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(9, 0, 30);

        let f = compute(&n, &ctx_at_version(2, vec![]));
        assert_eq!(f["has_comments"], FactValue::Int(1));
        assert_eq!(f["has_doc"], FactValue::Int(0), "no doc comment");
    }

    #[test]
    fn a_symbol_with_no_prose_at_all_reports_neither() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(0, 0, 30);

        let f = compute(&n, &ctx_at_version(2, vec![]));
        assert_eq!(f["has_comments"], FactValue::Int(0));
        assert_eq!(f["has_doc"], FactValue::Int(0));
    }

    #[test]
    fn language_and_classification_reach_the_store() {
        let mut n = node("f", Some("src/a.rs"));
        n.language = Some("rust".into());
        n.classification = Some(FileClassification::Service);

        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["language"], FactValue::Str("rust".into()));
        assert_eq!(f["classification"], FactValue::Str("service".into()));
    }

    /// The classifier saw the file's contents; the path heuristic only saw
    /// its name. Where they disagree, the classifier wins.
    #[test]
    fn classification_outranks_the_path_heuristic_for_is_test() {
        // A path that looks nothing like a test, classified as one.
        let mut n = node("f", Some("src/checkout.rs"));
        n.classification = Some(FileClassification::Test);
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));

        // No classification at all: fall back to the path, as before.
        let n = node("f", Some("tests/checkout.rs"));
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));

        // The direction this used to get wrong. "Where they disagree the
        // classifier wins" has to hold both ways: a file the classifier
        // positively called `Util` is not a test, however test-like its name
        // reads. Previously the `Some(_)` arm fell through to the path
        // heuristic, so this returned 1 and the file dropped out of every
        // statistic that filters `is_test = 0`.
        let mut n = node("f", Some("src/utils/test_helpers.rs"));
        n.classification = Some(FileClassification::Util);
        assert_eq!(
            compute(&n, &ctx_of(vec![]))["is_test"],
            FactValue::Int(0),
            "a classified non-test must not be relabelled by its path"
        );

        // Same in the other direction: a test-shaped classification on a
        // test-shaped path still agrees.
        let mut n = node("f", Some("tests/checkout.rs"));
        n.classification = Some(FileClassification::Test);
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));
    }

    /// `test_` was matched anywhere in the path, so ordinary words ending in
    /// "test" swept real code into the test bucket. Markers are anchored now.
    #[test]
    fn test_marker_does_not_match_mid_word() {
        for path in [
            "src/config/latest_version.rs",
            "src/greatest_hits.rs",
            "src/fastest_path.ts",
            "src/protest_form.tsx",
            "src/contest_rules.py",
        ] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(
                f["is_test"],
                FactValue::Int(0),
                "{path} is production code, not a test"
            );
        }

        // The real case the marker exists for still matches: `test_` at the
        // start of a filename.
        for path in ["src/test_thing.py", "test_top_level.py"] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f["is_test"], FactValue::Int(1), "{path} should read as test");
        }
    }

    /// A Rust struct's methods live in a separate `impl` block, so it has
    /// no `Contains` edges. Reporting `members: 0` would rank every Rust
    /// type as memberless against Java types that genuinely nest theirs.
    #[test]
    fn members_is_absent_rather_than_zero_when_the_language_does_not_nest() {
        let mut n = node("class:src/a.rs:S", Some("src/a.rs"));
        n.node_type = GraphNodeType::Class;
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("members"));

        let ctx = ctx_of(vec![
            edge("class:src/a.rs:S", "fn:one", GraphEdgeType::Contains),
            edge("class:src/a.rs:S", "fn:two", GraphEdgeType::Contains),
        ]);
        assert_eq!(compute(&n, &ctx)["members"], FactValue::Int(2));
    }

    #[test]
    fn only_types_get_a_members_fact() {
        let ctx = ctx_of(vec![edge("f", "g", GraphEdgeType::Contains)]);
        // A File also has Contains edges, but "members" is a property of a
        // type, and a File already reports its symbol count another way.
        let f = compute(&node("f", Some("src/a.rs")), &ctx);
        assert!(!f.contains_key("members"), "Function nodes have no members");
    }

    #[test]
    fn folder_nodes_get_no_self_referential_folder_fact() {
        let mut n = node("folder:src", Some("src"));
        n.node_type = GraphNodeType::Folder;
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("folder"));
        assert!(!f.contains_key("is_test"));
    }
}
