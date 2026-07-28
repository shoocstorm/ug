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

use crate::types::{GraphData, GraphEdgeType, GraphNode, GraphNodeType};
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
}

impl FactContext {
    pub fn new(graph: &GraphData) -> Self {
        let mut in_degree: HashMap<String, u32> = HashMap::new();
        let mut out_degree: HashMap<String, u32> = HashMap::new();
        for e in &graph.edges {
            if matches!(e.edge_type, GraphEdgeType::Contains) {
                continue;
            }
            *in_degree.entry(e.target.clone()).or_insert(0) += 1;
            *out_degree.entry(e.source.clone()).or_insert(0) += 1;
        }
        Self {
            in_degree,
            out_degree,
        }
    }
}

/// Path segments that mark a file as test code.
///
/// Deliberately a path heuristic and not the indexer's
/// `FileClassification`: that classification is computed but never reaches
/// `GraphNode`, so it is not available here yet. When it lands (design doc
/// A4), `is_test` should read it and this list becomes the fallback.
const TEST_PATH_MARKERS: &[&str] = &[
    "/test/",
    "/tests/",
    "/__tests__/",
    "/spec/",
    "test_",
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
/// Prefers the indexer's `metrics.loc` where it exists and falls back to
/// the line range, which is what gives Class and Interface nodes a size at
/// all — the extractors only compute metrics for functions today.
///
/// This is a *span*, so it counts blank and comment lines. A separate
/// `code_lines` fact needs the comment scanner and lands with design doc
/// A2; until then, do not describe this number as "lines of code".
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
    }

    f.insert(
        "has_doc".into(),
        FactValue::from_bool(n.docstring.as_deref().is_some_and(|d| !d.trim().is_empty())),
    );

    // Folder and is_test are only meaningful for nodes that live in a
    // file. Folder nodes carry their own path in `file`, which would make
    // `folder` self-referential, so they are excluded.
    if !matches!(n.node_type, GraphNodeType::Folder) {
        if let Some(file) = n.file.as_deref().filter(|s| !s.is_empty()) {
            f.insert("folder".into(), FactValue::Str(folder_of(file).to_string()));
            f.insert("is_test".into(), FactValue::from_bool(looks_like_test(file)));
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

    fn ctx_of(edges: Vec<GraphEdge>) -> FactContext {
        FactContext::new(&GraphData {
            nodes: vec![],
            edges,
            stats: None,
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

    #[test]
    fn folder_nodes_get_no_self_referential_folder_fact() {
        let mut n = node("folder:src", Some("src"));
        n.node_type = GraphNodeType::Folder;
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("folder"));
        assert!(!f.contains_key("is_test"));
    }
}
