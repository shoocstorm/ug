//! Tests for the agent tools.

use super::*;
use crate::types::{GraphEdge, GraphNodeType};

fn node(
    id: &str,
    name: &str,
    t: GraphNodeType,
    file: &str,
    lines: Option<(u32, u32)>,
) -> GraphNode {
    GraphNode {
        id: id.to_string(),
        name: name.to_string(),
        node_type: t,
        file: Some(file.to_string()),
        start_line: lines.map(|(s, _)| s),
        end_line: lines.map(|(_, e)| e),
        metrics: None,
        signature: None,
        docstring: None,
        imports: vec![],
        exports: vec![],
        extends: vec![],
        implements: vec![],
        calls: vec![],
        folder: None,
        ..Default::default()
    }
}

fn edge(source: &str, target: &str, edge_type: GraphEdgeType) -> GraphEdge {
    GraphEdge {
        source: source.into(),
        target: target.into(),
        edge_type,
    }
}

/// Two functions in one file, `caller` calling `callee`, plus the File
/// node that contains them. The File node carries no line range, like a
/// real one.
fn fixture() -> GraphData {
    GraphData {
        nodes: vec![
            node("file:src/a.rs", "a.rs", GraphNodeType::File, "src/a.rs", None),
            node(
                "function:src/a.rs:1:caller",
                "caller",
                GraphNodeType::Function,
                "src/a.rs",
                Some((1, 5)),
            ),
            node(
                "function:src/a.rs:7:callee",
                "callee",
                GraphNodeType::Function,
                "src/a.rs",
                Some((7, 9)),
            ),
        ],
        edges: vec![
            edge(
                "function:src/a.rs:1:caller",
                "function:src/a.rs:7:callee",
                GraphEdgeType::Calls,
            ),
            edge(
                "file:src/a.rs",
                "function:src/a.rs:1:caller",
                GraphEdgeType::Contains,
            ),
        ],
        stats: None,
        resolution: None,
    }
}

/// A repo root that is guaranteed not to exist, so a test that passes
/// can only have read from the index.
const NO_REPO: &str = "/nonexistent-repo-root";

/// One symbol with a neighbour of every role `context` reports:
/// a plain caller, a caller that lives in a test file, an outbound
/// dependency, an outbound `Contains` child (which is *not* a dependency),
/// and a `Concept` node pointing at it (documentation).
fn context_fixture() -> GraphData {
    let mut concept = node(
        "concept:docs/svc.md:Processing",
        "Processing",
        GraphNodeType::Concept,
        "docs/svc.md",
        Some((3, 20)),
    );
    concept.docstring = Some("How the processing pipeline fits together.".to_string());
    GraphData {
        nodes: vec![
            node("file:src/svc.rs", "svc.rs", GraphNodeType::File, "src/svc.rs", None),
            node(
                "function:src/svc.rs:10:process",
                "process",
                GraphNodeType::Function,
                "src/svc.rs",
                Some((10, 20)),
            ),
            node(
                "function:src/svc.rs:1:handler",
                "handler",
                GraphNodeType::Function,
                "src/svc.rs",
                Some((1, 5)),
            ),
            node(
                "function:tests/svc_test.rs:1:process_roundtrips",
                "process_roundtrips",
                GraphNodeType::Function,
                "tests/svc_test.rs",
                Some((1, 8)),
            ),
            node(
                "function:src/util.rs:1:normalize",
                "normalize",
                GraphNodeType::Function,
                "src/util.rs",
                Some((1, 4)),
            ),
            node(
                "variable:src/svc.rs:12:tmp",
                "tmp",
                GraphNodeType::Variable,
                "src/svc.rs",
                Some((12, 12)),
            ),
            concept,
        ],
        edges: vec![
            edge(
                "function:src/svc.rs:1:handler",
                "function:src/svc.rs:10:process",
                GraphEdgeType::Calls,
            ),
            edge(
                "function:tests/svc_test.rs:1:process_roundtrips",
                "function:src/svc.rs:10:process",
                GraphEdgeType::Calls,
            ),
            edge(
                "function:src/svc.rs:10:process",
                "function:src/util.rs:1:normalize",
                GraphEdgeType::Calls,
            ),
            // Structure, not dependence — must not show up as a dependency.
            edge(
                "function:src/svc.rs:10:process",
                "variable:src/svc.rs:12:tmp",
                GraphEdgeType::Contains,
            ),
            edge(
                "concept:docs/svc.md:Processing",
                "function:src/svc.rs:10:process",
                GraphEdgeType::References,
            ),
        ],
        stats: None,
        resolution: None,
    }
}

fn context_of(graph: &GraphData, src: &IndexedSource, p: ContextParams) -> ContextResult {
    context(graph, SourceCtx::new(src, Path::new(NO_REPO)), &p)
}

fn roles(r: &ContextResult, role: &str) -> Vec<String> {
    r.items
        .iter()
        .filter(|i| i.role == role)
        .map(|i| i.symbol.name.clone())
        .collect()
}

/// The pack's whole value proposition: one call returns every kind of
/// neighbour, and each one says why it is there.
///
/// The load-bearing assertion is the test/caller split. `process_roundtrips`
/// calls the target exactly like `handler` does, so a naive walk lists it
/// twice or calls it a caller; "who breaks" and "what re-verifies" are
/// different questions and it belongs to the second.
#[test]
fn context_labels_every_neighbour_with_its_role() {
    let graph = context_fixture();
    let src = indexed(&[("function:src/svc.rs:10:process", "fn process() { normalize(); }")]);
    let r = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["process".to_string()],
            ..Default::default()
        },
    );

    assert!(r.ok(), "{:?}", r.error);
    assert_eq!(r.target.as_ref().expect("target").name, "process");
    assert_eq!(roles(&r, "target"), vec!["process"]);
    assert_eq!(roles(&r, "caller"), vec!["handler"]);
    assert_eq!(roles(&r, "test"), vec!["process_roundtrips"]);
    assert_eq!(roles(&r, "doc"), vec!["Processing"]);

    // `Contains` is structure: the variable the function holds is not
    // something it depends on.
    assert_eq!(roles(&r, "dependency"), vec!["normalize"]);

    // The target travels with its body; the rest travel as signatures.
    let target = r.items.iter().find(|i| i.role == "target").expect("target item");
    assert!(target.code.as_deref().unwrap_or_default().contains("fn process"));
    for item in r.items.iter().filter(|i| i.role == "caller" || i.role == "dependency") {
        assert!(item.code.is_none(), "{} carried a body", item.symbol.name);
    }

    // Every role label is one the schema advertises.
    for item in &r.items {
        assert!(CONTEXT_ROLES.contains(&item.role), "{}", item.role);
        assert!(!item.why.is_empty(), "{} has no why", item.symbol.name);
    }

    // The rendered form is what an agent actually reads, so assert on it
    // rather than only on the struct. Section headings are pluralised by
    // hand — appending `s` produced "dependencys" — and every neighbour
    // must arrive with an id the caller can paste into a follow-up call.
    let text = render_context(&r, Render::Markdown);
    for heading in ["── target ──", "── callers (1) ──", "── tests (1) ──",
                    "── dependencies (1) ──", "── docs (1) ──"] {
        assert!(text.contains(heading), "missing {heading} in:\n{text}");
    }
    assert!(text.contains("fn process"), "target body missing");
    assert!(text.contains("processing pipeline"), "doc prose missing");
    assert!(
        text.contains("function:src/util.rs:1:normalize"),
        "dependency id missing — the caller cannot follow up without it"
    );
}

/// A budget that binds drops from the bottom of the priority order and
/// says what it dropped — the caller must never be left guessing whether
/// "no tests" means "none exist" or "none fit".
#[test]
fn context_fills_by_priority_and_reports_what_did_not_fit() {
    let graph = context_fixture();
    let src = indexed(&[("function:src/svc.rs:10:process", "fn process() {}")]);
    // Chosen to bind partway down the priority order: enough for the
    // target and its caller, not enough to reach the docs at the bottom.
    const TIGHT: usize = 700;
    let tight = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["process".to_string()],
            max_chars: Some(TIGHT),
            ..Default::default()
        },
    );

    assert!(tight.ok());
    assert!(tight.used_chars <= TIGHT, "spent {}", tight.used_chars);
    // Docs are the first to go; the caller is not — that is the priority
    // order doing its job, and it is the whole reason the order exists.
    assert_eq!(roles(&tight, "caller"), vec!["handler"]);
    assert!(
        roles(&tight, "doc").is_empty(),
        "docs should not fit in {TIGHT}"
    );
    assert!(
        tight.dropped.iter().any(|d| d.role == "doc"),
        "a dropped doc must be reported: {:?}",
        tight.dropped
    );

    // The same call with room reports nothing dropped.
    let roomy = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["process".to_string()],
            ..Default::default()
        },
    );
    assert!(roomy.dropped.is_empty(), "{:?}", roomy.dropped);
    assert!(roomy.used_chars > tight.used_chars);
}

/// `include` is how an agent buys only the half it needs.
#[test]
fn context_include_keeps_only_the_named_roles() {
    let graph = context_fixture();
    let src = indexed(&[("function:src/svc.rs:10:process", "fn process() {}")]);
    let r = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["process".to_string()],
            include: vec!["caller".to_string(), "test".to_string()],
            ..Default::default()
        },
    );

    assert_eq!(roles(&r, "caller"), vec!["handler"]);
    assert_eq!(roles(&r, "test"), vec!["process_roundtrips"]);
    for absent in ["target", "dependency", "doc"] {
        assert!(roles(&r, absent).is_empty(), "{} should be filtered out", absent);
    }
    assert!(r.notes.is_empty(), "valid roles must not warn: {:?}", r.notes);

    // A misspelled role must say so. Every section heading is plural, so
    // `--include callers` is the natural mistake, and an unexplained empty
    // pack reads as "this symbol has no callers".
    let typo = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["process".to_string()],
            include: vec!["callers".to_string()],
            ..Default::default()
        },
    );
    assert!(typo.items.is_empty());
    assert!(
        typo.notes.iter().any(|n| n.contains("unknown role")),
        "{:?}",
        typo.notes
    );
}

/// A pack is a claim about one symbol's neighbourhood, so an ambiguous
/// name has to fail loudly with the candidates rather than silently
/// picking one and describing the wrong code.
#[test]
fn context_refuses_an_ambiguous_or_unknown_symbol() {
    let mut graph = context_fixture();
    graph.nodes.push(node(
        "function:src/other.rs:1:process",
        "process",
        GraphNodeType::Function,
        "src/other.rs",
        Some((1, 3)),
    ));
    let src = indexed(&[]);

    let ambiguous = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["process".to_string()],
            ..Default::default()
        },
    );
    assert!(!ambiguous.ok());
    let err = ambiguous.error.expect("error");
    assert!(err.contains("src/other.rs"), "{err}");
    assert!(ambiguous.items.is_empty());

    let unknown = context_of(
        &graph,
        &src,
        ContextParams {
            node_id: vec!["no_such_symbol".to_string()],
            ..Default::default()
        },
    );
    assert!(!unknown.ok());

    // An empty request is a usage error, not an empty pack.
    let blank = context_of(&graph, &src, ContextParams::default());
    assert!(!blank.ok());
}

fn indexed(entries: &[(&str, &str)]) -> IndexedSource {
    let mut out = IndexedSource::default();
    for (id, code) in entries {
        out.insert(
            *id,
            StoredSource {
                code: (*code).to_string(),
                file_hash: "deadbeef".into(),
            },
        );
    }
    out
}

/// The whole point of the capture: call-site evidence with no working
/// tree anywhere. The line numbers must be the caller's real file lines,
/// not offsets into its captured span.
#[test]
fn find_usages_reads_call_sites_from_the_index_with_no_repo() {
    let g = fixture();
    // `caller` spans lines 1-5; its capture is exactly those lines.
    let src = indexed(&[(
        "function:src/a.rs:1:caller",
        "fn caller() {\n    let x = 1;\n    callee(x)\n}\n\n",
    )]);
    let r = find_usages(
        &g,
        SourceCtx::new(&src, Path::new(NO_REPO)),
        &FindUsagesParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    let sites = &r.nodes[0].users[0].call_sites;
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].line, 3, "third line of a span starting at line 1");
    assert_eq!(sites[0].text, "callee(x)");
}

/// A caller whose own span was never captured still gets call sites from
/// the file's whole-file node — here the line offset comes from the
/// caller's declared range, not from the start of the file.
#[test]
fn find_usages_falls_back_to_the_whole_file_capture() {
    let mut g = fixture();
    // Move `caller` to lines 3-5 so a file-relative scan is visible.
    g.nodes[1].start_line = Some(3);
    g.nodes[1].end_line = Some(5);
    let src = indexed(&[(
        "file:src/a.rs",
        "// header\n\nfn caller() {\n    callee()\n}\n\nfn callee() {}\n",
    )]);
    let r = find_usages(
        &g,
        SourceCtx::new(&src, Path::new(NO_REPO)),
        &FindUsagesParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    let sites = &r.nodes[0].users[0].call_sites;
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].line, 4);
    assert_eq!(sites[0].text, "callee()");
}

/// Without an index and without a repo there is simply no evidence to
/// show — but the structural answer still has to come back.
#[test]
fn find_usages_without_source_still_reports_users() {
    let g = fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.nodes[0].users.len(), 1);
    assert!(r.nodes[0].users[0].call_sites.is_empty());
}

/// The pre-fetch and the scan have to agree on which nodes matter,
/// otherwise a transport quietly fetches the wrong rows and call sites
/// vanish with no error.
#[test]
fn find_usages_source_ids_cover_what_the_scan_reads() {
    let g = fixture();
    let ids = find_usages_source_ids(
        &g,
        &FindUsagesParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert!(ids.contains(&"function:src/a.rs:1:caller".to_string()));
    assert!(
        ids.contains(&"file:src/a.rs".to_string()),
        "the file's whole-file node backs up an uncaptured caller"
    );
}

#[test]
fn get_code_serves_a_node_from_the_index_with_no_repo() {
    let g = fixture();
    let src = indexed(&[("function:src/a.rs:7:callee", "fn callee() {\n    42\n}\n")]);
    let r = get_code(
        &g,
        SourceCtx::new(&src, Path::new(NO_REPO)),
        &GetCodeParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert!(r.ok());
    assert_eq!(r.slices[0].code.as_deref(), Some("fn callee() {\n    42\n}\n"));
    assert!(
        r.slices[0].stale.is_none(),
        "a missing repo is not a staleness signal"
    );
}

/// The file/line-range form, which has no node of its own to read: it
/// slices the file's whole-file capture instead of the working tree.
#[test]
fn get_code_slices_a_range_out_of_the_whole_file_capture() {
    let g = fixture();
    let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\nfour\nfive\n")]);
    let r = get_code(
        &g,
        SourceCtx::new(&src, Path::new(NO_REPO)),
        &GetCodeParams {
            file: Some("src/a.rs".into()),
            start_line: Some(2),
            end_line: Some(4),
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    assert_eq!(r.slices[0].code.as_deref(), Some("two\nthree\nfour"));
    assert_eq!(r.slices[0].total_lines, Some(6));
}

/// With nothing captured and no repo, `get_code` must say so rather than
/// return an empty slice that reads as "this symbol has no code".
#[test]
fn get_code_reports_when_neither_index_nor_repo_has_the_file() {
    let g = fixture();
    let r = get_code(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &GetCodeParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert!(!r.ok());
    let err = r.slices[0].error.as_ref().unwrap();
    assert!(err.contains("not captured in the index"), "{}", err);
    assert!(err.contains(NO_REPO), "{}", err);
}

/// The whole point of P1: when the repo is on disk, `get_code` serves the
/// *current* file content, not the stale capture. An editing agent that
/// just changed the file must read back what it wrote.
#[test]
fn get_code_prefers_the_live_working_tree_over_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path();
    // A real file on disk: the symbol body says `99` at lines 1-3.
    std::fs::create_dir_all(repo_root.join("src")).unwrap();
    std::fs::write(repo_root.join("src/a.rs"), "fn callee() {\n    99\n}\n").unwrap();

    // The node spans the same lines 1-3, so the span is in range for both
    // the live file and the stale capture below.
    let g = GraphData {
        nodes: vec![node(
            "function:src/a.rs:1:callee",
            "callee",
            GraphNodeType::Function,
            "src/a.rs",
            Some((1, 3)),
        )],
        edges: vec![],
        stats: None,
        resolution: None,
    };

    // Index holds a *different* (stale) capture of the same span — `42`.
    let src = indexed(&[(
        "function:src/a.rs:1:callee",
        "fn callee() {\n    42\n}\n",
    )]);

    let r = get_code(
        &g,
        SourceCtx::new(&src, repo_root),
        &GetCodeParams {
            node_id: vec!["function:src/a.rs:1:callee".into()],
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    // 99 is the live edit; 42 is the stale capture. Live wins.
    assert!(
        r.slices[0].code.as_deref().unwrap().contains("99"),
        "live content not served: {:?}",
        r.slices[0].code
    );
    assert!(
        r.slices[0].stale.is_some(),
        "a live read that disagrees with the index must be flagged"
    );
}

/// When the live file still matches what was indexed, no staleness flag —
/// the span and the source agree, so there is nothing to warn about.
#[test]
fn get_code_live_read_with_matching_hash_is_not_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path();
    let body = "fn callee() {\n    42\n}\n";
    std::fs::create_dir_all(repo_root.join("src")).unwrap();
    std::fs::write(repo_root.join("src/a.rs"), body).unwrap();
    let hash = blake3::hash(body.as_bytes()).to_hex().to_string();

    let mut src = IndexedSource::default();
    src.insert(
        "function:src/a.rs:7:callee",
        StoredSource { code: body.to_string(), file_hash: hash },
    );

    let r = get_code(
        &fixture(),
        SourceCtx::new(&src, repo_root),
        &GetCodeParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert!(r.ok());
    assert!(r.slices[0].stale.is_none(), "matching hash must not flag");
}

/// The file/line-range form reads the working tree too: an agent paging a
/// file with `--range` wants current lines, and a changed file is flagged.
#[test]
fn get_code_file_range_reads_live_and_flags_drift() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path();
    std::fs::create_dir_all(repo_root.join("src")).unwrap();
    std::fs::write(repo_root.join("src/a.rs"), "a\nb\nc\nd\ne\n").unwrap();

    // Index holds a stale, different capture so the drift is detectable.
    let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\n")]);

    let r = get_code(
        &fixture(),
        SourceCtx::new(&src, repo_root),
        &GetCodeParams {
            file: Some("src/a.rs".into()),
            start_line: Some(2),
            end_line: Some(4),
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    // Live lines b/c/d, not the stale two/three/four.
    assert_eq!(r.slices[0].code.as_deref(), Some("b\nc\nd"));
    assert!(r.slices[0].stale.is_some(), "drift between live and index must flag");
}

#[test]
fn get_code_source_ids_cover_nodes_and_their_files() {
    let g = fixture();
    let ids = get_code_source_ids(
        &g,
        &GetCodeParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert_eq!(
        ids,
        vec!["file:src/a.rs", "function:src/a.rs:7:callee"],
        "the node's span plus its file's capture as backup"
    );

    // The file form has no node id of its own to ask for.
    let ids = get_code_source_ids(
        &g,
        &GetCodeParams {
            file: Some("file:src/a.rs".into()),
            ..Default::default()
        },
    );
    assert_eq!(ids, vec!["file:src/a.rs"], "the file id prefix is stripped");
}

#[test]
fn find_symbol_ranks_exact_then_prefix_then_substring() {
    // `call` (exact) > `caller` (prefix) > `do_call` (substring).
    let g = GraphData {
        nodes: vec![
            node("f:1:do_call", "do_call", GraphNodeType::Function, "a.rs", Some((1, 2))),
            node("f:2:caller", "caller", GraphNodeType::Function, "a.rs", Some((3, 4))),
            node("f:3:call", "call", GraphNodeType::Function, "a.rs", Some((5, 6))),
        ],
        edges: vec![],
        stats: None,
        resolution: None,
    };
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["call".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.queries[0].total, 3);
    let order: Vec<&str> = r.queries[0].items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(order, vec!["call", "caller", "do_call"]);
}

#[test]
/// `include_docs` widens the search to docstrings: the matches must be
/// additive, and must rank below every name match.
fn find_symbol_include_docs_ranks_below_name_hits() {
    let mut g = GraphData {
        nodes: vec![
            node("f:1:cache_get", "cache_get", GraphNodeType::Function, "a.rs", Some((1, 2))),
            node("f:2:drop_stale", "drop_stale", GraphNodeType::Function, "a.rs", Some((3, 4))),
        ],
        edges: vec![],
        stats: None,
        resolution: None,
    };
    g.nodes[1].docstring = Some("Evicts entries from the cache.".into());

    // Name-only: the docstring mention is invisible.
    let names_only = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["cache".into()],
            ..Default::default()
        },
    );
    assert_eq!(names_only.queries[0].total, 1);
    assert_eq!(names_only.queries[0].items[0].name, "cache_get");

    // With docs: both, and the name hit still comes first.
    let with_docs = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["cache".into()],
            include_docs: true,
            ..Default::default()
        },
    );
    assert_eq!(with_docs.queries[0].total, 2);
    let order: Vec<&str> = with_docs.queries[0]
        .items
        .iter()
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(order, vec!["cache_get", "drop_stale"]);
}

#[test]
fn find_symbol_honours_type_and_file_filters() {
    let g = fixture();
    let all = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["a".into()],
            ..Default::default()
        },
    );
    assert!(all.queries[0].total >= 3);

    let functions_only = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["a".into()],
            node_types: vec!["function".into()],
            ..Default::default()
        },
    );
    assert!(functions_only.queries[0]
        .items
        .iter()
        .all(|i| i.node_type == "Function"));

    let nothing = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["a".into()],
            file_prefix: Some("other/".into()),
            ..Default::default()
        },
    );
    assert_eq!(nothing.queries[0].total, 0);
}

#[test]
fn find_symbol_respects_limit_but_reports_full_total() {
    let g = fixture();
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["call".into()],
            limit: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(r.queries[0].total, 2, "total counts every match");
    assert_eq!(r.queries[0].items.len(), 1, "items honour the limit");
}

#[test]
fn find_symbol_direct_id_lookup() {
    let g = fixture();
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.queries[0].kind, "id");
    assert_eq!(r.queries[0].items[0].name, "callee");
    assert!(r.ok());
}

#[test]
fn find_symbol_reports_missing_id() {
    let g = fixture();
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            node_id: vec!["function:nope".into()],
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert_eq!(r.queries[0].total, 0);
}

// -----------------------------------------------------------------
// get_code line windows
// -----------------------------------------------------------------

fn window(range: Option<&str>, start: Option<usize>, end: Option<usize>) -> (Option<usize>, Option<usize>) {
    line_window(&GetCodeParams {
        range: range.map(String::from),
        start_line: start,
        end_line: end,
        ..Default::default()
    })
    .unwrap()
}

/// `--range` has to mean on `get_code` what it means on `ug analyze`, or
/// the shared spelling is a trap. Same parser, so every phrasing that
/// works there works here.
#[test]
fn range_accepts_the_same_spellings_as_analyze() {
    assert_eq!(window(Some("11-35"), None, None), (Some(11), Some(35)));
    assert_eq!(window(Some("11..35"), None, None), (Some(11), Some(35)));
    assert_eq!(window(Some("rows 11 to 35"), None, None), (Some(11), Some(35)));
    // Open-ended: no end bound, which the reader turns into EOF.
    assert_eq!(window(Some("34-end"), None, None), (Some(34), None));
    assert_eq!(window(Some("34-"), None, None), (Some(34), None));
    // A bare count is "the first N" — as in analyze, not "line N".
    assert_eq!(window(Some("20"), None, None), (Some(1), Some(20)));
}

#[test]
fn explicit_start_end_win_over_range() {
    assert_eq!(window(Some("11-35"), Some(5), None), (Some(5), Some(35)));
    assert_eq!(window(Some("11-35"), None, Some(99)), (Some(11), Some(99)));
    assert_eq!(window(None, Some(5), Some(9)), (Some(5), Some(9)));
}

/// A window the parser cannot read is reported, not rounded down to
/// "the whole file" — that would be a wrong answer that looks right.
#[test]
fn an_unreadable_range_is_an_error_with_the_input_in_it() {
    let e = line_window(&GetCodeParams {
        range: Some("banana".into()),
        ..Default::default()
    })
    .unwrap_err();
    assert!(e.contains("banana"), "got: {e}");
    assert!(e.contains("34-end"), "the message shows the valid forms: {e}");

    // A backwards range is nonsense too, and analyze rejects it.
    assert!(line_window(&GetCodeParams {
        range: Some("35-11".into()),
        ..Default::default()
    })
    .is_err());
}

/// End to end through the tool, so the error reaches the caller as a
/// slice rather than being swallowed.
#[test]
fn get_code_surfaces_a_bad_range_instead_of_reading_the_whole_file() {
    let g = fixture();
    let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\nfour\n")]);
    let r = get_code(
        &g,
        SourceCtx::new(&src, Path::new(NO_REPO)),
        &GetCodeParams {
            file: Some("src/a.rs".into()),
            range: Some("nope".into()),
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert!(r.slices[0].error.as_ref().unwrap().contains("nope"));
}

#[test]
fn get_code_range_reads_exactly_that_window() {
    let g = fixture();
    let src = indexed(&[("file:src/a.rs", "one\ntwo\nthree\nfour\nfive\n")]);
    let r = get_code(
        &g,
        SourceCtx::new(&src, Path::new(NO_REPO)),
        &GetCodeParams {
            file: Some("src/a.rs".into()),
            range: Some("2-3".into()),
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    assert_eq!(r.slices[0].code.as_deref(), Some("two\nthree"));
    assert_eq!(r.slices[0].start_line, Some(2));
    assert_eq!(r.slices[0].end_line, Some(3));
}

// -----------------------------------------------------------------
// Wildcards
// -----------------------------------------------------------------

/// A graph with names and paths chosen so glob semantics are visible:
/// three `run_*` functions across two directories, plus a name that
/// only a substring search would reach.
fn glob_fixture() -> GraphData {
    GraphData {
        nodes: vec![
            node("file:src/a.rs", "a.rs", GraphNodeType::File, "src/a.rs", None),
            node("file:src/deep/b.rs", "b.rs", GraphNodeType::File, "src/deep/b.rs", None),
            node("function:src/a.rs:1:run_gen", "run_gen", GraphNodeType::Function, "src/a.rs", Some((1, 5))),
            node("function:src/a.rs:7:run_serve", "run_serve", GraphNodeType::Function, "src/a.rs", Some((7, 9))),
            node("function:src/deep/b.rs:1:run_index", "run_index", GraphNodeType::Function, "src/deep/b.rs", Some((1, 4))),
            node("function:src/deep/b.rs:6:prerun_gen", "prerun_gen", GraphNodeType::Function, "src/deep/b.rs", Some((6, 8))),
            node("class:src/a.rs:20:Runner", "Runner", GraphNodeType::Class, "src/a.rs", Some((20, 30))),
        ],
        edges: vec![edge(
            "function:src/a.rs:1:run_gen",
            "function:src/deep/b.rs:1:run_index",
            GraphEdgeType::Calls,
        )],
        stats: None,
        resolution: None,
    }
}

fn names_of(q: &SymbolQueryResult) -> Vec<&str> {
    q.items.iter().map(|i| i.name.as_str()).collect()
}

/// The headline behaviour: a pattern matches the whole name, so
/// `run_*` must not pick up `prerun_gen` the way a substring search
/// would.
#[test]
fn find_symbols_wildcard_is_anchored_to_the_whole_name() {
    let g = glob_fixture();
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["run_*".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.queries[0].kind, "pattern");
    assert_eq!(names_of(&r.queries[0]), vec!["run_gen", "run_index", "run_serve"]);
}

/// A plain fragment keeps the ranked substring behaviour it always had —
/// wildcards are additive, not a replacement.
#[test]
fn find_symbols_literal_still_matches_substrings() {
    let g = glob_fixture();
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["run_gen".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.queries[0].kind, "name");
    assert_eq!(
        names_of(&r.queries[0]),
        vec!["run_gen", "prerun_gen"],
        "exact first, then the substring hit"
    );
}

#[test]
fn find_symbols_wildcard_honours_type_and_path_filters() {
    let g = glob_fixture();
    // `*` + a path glob is the "list this subtree" idiom.
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["*".into()],
            node_types: vec!["Function".into()],
            file_prefix: Some("src/deep/**".into()),
            ..Default::default()
        },
    );
    assert_eq!(names_of(&r.queries[0]), vec!["prerun_gen", "run_index"]);

    // A literal file filter keeps meaning "prefix", not "equals".
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["run_*".into()],
            file_prefix: Some("src/a.rs".into()),
            ..Default::default()
        },
    );
    assert_eq!(names_of(&r.queries[0]), vec!["run_gen", "run_serve"]);

    // A node-type filter may itself be a pattern.
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["*".into()],
            node_types: vec!["Cl*".into()],
            ..Default::default()
        },
    );
    assert_eq!(names_of(&r.queries[0]), vec!["Runner"]);
}

/// Two refs that overlap must not yield the same id twice.
///
/// `Vec::dedup` only collapses *adjacent* duplicates, and these never
/// are: each ref's expansion is appended whole, so a repeat of an
/// earlier id lands with other ids between them. The visible symptom was
/// `get_code run_gen 'run_*'` printing `run_gen`'s entire body twice.
#[test]
fn overlapping_refs_do_not_repeat_a_node_id() {
    let g = glob_fixture();
    let refs = vec!["run_gen".to_string(), "run_serve".to_string(), "run_*".to_string()];
    let out = expand_node_refs(&g, &refs, MAX_REF_EXPANSION);

    let mut sorted = out.clone();
    sorted.sort();
    let mut unique = sorted.clone();
    unique.dedup();
    assert_eq!(sorted, unique, "expansion contains duplicate ids: {out:?}");

    // First mention wins, so the caller's own ordering survives: the two
    // explicit refs stay in front of the ids the pattern added.
    assert_eq!(out[0], "function:src/a.rs:1:run_gen");
    assert_eq!(out[1], "function:src/a.rs:7:run_serve");
    assert!(out.contains(&"function:src/deep/b.rs:1:run_index".to_string()));
}

/// `*` in a path must not cross `/`, or every "this directory" query
/// silently becomes a whole-subtree query.
#[test]
fn file_outline_glob_expands_to_every_matching_file() {
    let g = glob_fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["src/*.rs".into()],
            ..Default::default()
        },
    );
    assert!(r.ok());
    assert_eq!(r.files.len(), 1);
    assert_eq!(r.files[0].file.as_deref(), Some("src/a.rs"));

    let deep = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["src/**/*.rs".into()],
            ..Default::default()
        },
    );
    let outlined: Vec<&str> = deep.files.iter().filter_map(|f| f.file.as_deref()).collect();
    assert_eq!(outlined, vec!["src/a.rs", "src/deep/b.rs"]);
}

/// Over the cap, the extra paths are named rather than dropped — a
/// truncated answer that does not say so is the failure mode worth
/// testing for.
#[test]
fn file_outline_glob_reports_the_files_it_did_not_expand() {
    let g = glob_fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["src/**/*.rs".into()],
            max_files: Some(1),
            ..Default::default()
        },
    );
    assert!(!r.ok(), "the overflow entry carries an error");
    assert_eq!(r.files[0].file.as_deref(), Some("src/a.rs"));
    let overflow = r.files.last().unwrap();
    assert_eq!(overflow.candidates, vec!["src/deep/b.rs".to_string()]);
    assert!(overflow.error.as_ref().unwrap().contains("max_files"));
}

#[test]
fn file_outline_glob_matching_nothing_explains_itself() {
    let g = glob_fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["src/*.ts".into()],
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert!(r.files[0].error.as_ref().unwrap().contains("**/"));
}

/// The id-taking tools accept a bare name. Before this, `find_usages
/// callee` was an error telling the caller to go look the id up.
#[test]
fn id_taking_tools_accept_a_bare_name() {
    let g = fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams {
            node_id: vec!["callee".into()],
            ..Default::default()
        },
    );
    assert!(r.ok(), "a name resolves like an id");
    assert_eq!(r.nodes[0].users[0].symbol.name, "caller");
}

/// One pattern seeds one merged walk over every symbol it names.
#[test]
fn traverse_expands_a_pattern_into_several_seeds() {
    let g = glob_fixture();
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["run_*".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    assert!(r.ok());
    assert_eq!(r.seeds.len(), 3, "three run_* functions seeded the walk");
}

/// The cap is reported, not silently applied.
#[test]
fn expansion_over_the_cap_is_reported() {
    let g = glob_fixture();
    let expanded = expand_node_refs(&g, &["run_*".to_string()], 2);
    assert_eq!(expanded.len(), 3, "two ids plus the pattern itself");
    assert_eq!(expanded[2], "run_*");
    let msg = unresolved_ref_error(&g, "run_*", 2);
    assert!(msg.contains("matches 3 symbols"), "got: {msg}");
    assert!(msg.contains("first 2"), "got: {msg}");
}

/// An endpoint that names several nodes has to be refused, not guessed:
/// "is A connected to B" has a different answer per candidate.
#[test]
fn single_ref_resolution_refuses_ambiguity() {
    let g = glob_fixture();
    assert_eq!(
        resolve_single_ref(&g, "run_serve").unwrap(),
        "function:src/a.rs:7:run_serve"
    );
    let err = resolve_single_ref(&g, "run_*").unwrap_err();
    assert!(err.contains("matches 3 symbols"), "got: {err}");
    let err = resolve_single_ref(&g, "nope").unwrap_err();
    assert!(err.contains("No symbol named 'nope'"), "got: {err}");
}

/// The error text has to name the actual problem — a missing id, a name
/// that matches nothing, and a mis-written pattern send the reader in
/// three different directions.
#[test]
fn unresolved_refs_are_diagnosed_by_shape() {
    let g = glob_fixture();
    assert!(unresolved_ref_error(&g, "function:nope", 25).contains("No node with id"));
    assert!(unresolved_ref_error(&g, "zzz", 25).contains("No symbol named"));
    assert!(unresolved_ref_error(&g, "zzz_*", 25).contains("No symbol matches pattern"));
}

#[test]
fn file_outline_resolves_suffix_and_orders_by_line() {
    let g = fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["a.rs".into()],
            ..Default::default()
        },
    );
    assert!(r.ok());
    let entry = &r.files[0];
    assert_eq!(entry.file.as_deref(), Some("src/a.rs"));
    // File/Folder nodes are excluded; symbols come back in line order.
    assert_eq!(entry.symbols.len(), 2);
    assert_eq!(entry.symbols[0].name, "caller");
    assert_eq!(entry.symbols[1].name, "callee");
}

#[test]
fn file_outline_rejects_non_file_node_id() {
    let g = fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams {
            node_id: vec!["function:src/a.rs:1:caller".into()],
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert!(r.files[0].error.as_ref().unwrap().contains("not a File"));
}

#[test]
fn find_usages_walks_inbound_and_skips_contains() {
    let g = fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new("/nonexistent")),
        &FindUsagesParams {
            node_id: vec!["function:src/a.rs:7:callee".into()],
            ..Default::default()
        },
    );
    assert!(r.ok());
    assert_eq!(r.nodes[0].users.len(), 1);
    assert_eq!(r.nodes[0].users[0].symbol.name, "caller");
    assert_eq!(r.nodes[0].users[0].via_edge, "Calls");
    assert_eq!(r.nodes[0].users[0].depth, 1);

    // The Contains edge into `caller` must not count as a usage.
    let r2 = find_usages(
        &g,
        SourceCtx::repo_only(Path::new("/nonexistent")),
        &FindUsagesParams {
            node_id: vec!["function:src/a.rs:1:caller".into()],
            ..Default::default()
        },
    );
    assert!(r2.nodes[0].users.is_empty());
}

#[test]
fn traverse_respects_direction() {
    let g = fixture();
    let callee = "function:src/a.rs:7:callee";

    // Nothing downstream of callee...
    let out = traverse(
        &g,
        &TraverseParams {
            node_id: vec![callee.into()],
            hops: Some(2),
            ..Default::default()
        },
    );
    assert!(out.ok());
    assert_eq!(out.nodes.len(), 1, "only the seed itself");

    // ...but caller points at it.
    let inbound = traverse(
        &g,
        &TraverseParams {
            node_id: vec![callee.into()],
            hops: Some(1),
            direction: Some("inbound".into()),
            ..Default::default()
        },
    );
    let names: Vec<&str> = inbound.nodes.iter().map(|n| n.symbol.name.as_str()).collect();
    assert!(names.contains(&"caller"), "inbound must reach the caller");
    assert_eq!(
        inbound.nodes.iter().find(|n| n.symbol.name == "callee").unwrap().distance,
        0
    );
    assert_eq!(
        inbound.nodes.iter().find(|n| n.symbol.name == "caller").unwrap().distance,
        1
    );
}

#[test]
fn traverse_filters_edge_types_and_reports_missing_seeds() {
    let g = fixture();
    // `Contains` only — the Calls edge must not be followed.
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["file:src/a.rs".into(), "function:nope".into()],
            hops: Some(3),
            edge_types: vec!["contains".into()],
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert_eq!(r.missing, vec!["function:nope".to_string()]);
    let names: Vec<&str> = r.nodes.iter().map(|n| n.symbol.name.as_str()).collect();
    assert!(names.contains(&"caller"), "Contains edge followed");
    assert!(
        !names.contains(&"callee"),
        "callee is only reachable via Calls, which was filtered out"
    );
}

/// `find_usages` is `traverse` pinned to inbound with a default edge set;
/// on the same input the two must agree about who the users are.
#[test]
fn traverse_inbound_agrees_with_find_usages() {
    let g = fixture();
    let callee = "function:src/a.rs:7:callee";
    let usages = find_usages(
        &g,
        SourceCtx::repo_only(Path::new("/nonexistent")),
        &FindUsagesParams {
            node_id: vec![callee.into()],
            ..Default::default()
        },
    );
    let t = traverse(
        &g,
        &TraverseParams {
            node_id: vec![callee.into()],
            hops: Some(1),
            direction: Some("inbound".into()),
            edge_types: USAGE_EDGE_TYPES.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
    );
    let mut a: Vec<&str> = usages.nodes[0].users.iter().map(|u| u.symbol.id.as_str()).collect();
    let mut b: Vec<&str> = t
        .nodes
        .iter()
        .filter(|n| n.distance > 0)
        .map(|n| n.symbol.id.as_str())
        .collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn project_overview_excludes_contains_from_in_degree() {
    let g = fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/repo/graph.json"));
    assert_eq!(r.node_count, 3);
    assert_eq!(r.edge_count, 2);
    // Only `callee` has a non-Contains inbound edge.
    assert_eq!(r.hotspots.len(), 1);
    assert_eq!(r.hotspots[0].symbol.name, "callee");
    assert_eq!(r.hotspots[0].in_degree, 1);
}

#[test]
fn graph_schema_reports_edge_shapes() {
    let g = fixture();
    let r = graph_schema(&g, Path::new("/repo/graph.json"));
    let calls = r.edge_types.iter().find(|e| e.name == "Calls").unwrap();
    assert_eq!(calls.count, 1);
    assert_eq!(calls.shapes[0].shape, "Function→Function");
    assert!(r.vocabulary.contains(&"Contains".to_string()));
}

/// Every renderer, both styles. Markdown must never leak an ANSI escape
/// and ANSI must never leak a Markdown backtick — the two surfaces share
/// one layout, so a hardcoded marker in either direction shows up here.
#[test]
fn renderers_never_leak_the_other_surfaces_markup() {
    let g = fixture();
    let repo = Path::new("/repo");
    let gp = Path::new("/repo/graph.json");

    let symbols = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["caller".into(), "nothing-matches-this".into()],
            ..Default::default()
        },
    );
    let outline = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["a.rs".into(), "missing.rs".into()],
            ..Default::default()
        },
    );
    let usages = find_usages(
        &g,
        SourceCtx::repo_only(Path::new("/nonexistent")),
        &FindUsagesParams {
            node_id: vec![
                "function:src/a.rs:7:callee".into(),
                "function:src/a.rs:1:caller".into(),
            ],
            ..Default::default()
        },
    );
    let overview = project_overview(&g, repo, gp);
    let schema = graph_schema(&g, gp);
    let missing_path = ShortestPathResult {
        source: "a".into(),
        target: "b".into(),
        found: false,
        reversed: false,
        length: None,
        path: vec![],
        nodes: vec![],
    };

    let cases: Vec<(&str, Box<dyn Fn(Render) -> String>)> = vec![
        ("find_symbols", Box::new(move |s| render_find_symbols(&symbols, s))),
        ("file_outline", Box::new(move |s| render_file_outline(&outline, s))),
        ("find_usages", Box::new(move |s| render_find_usages(&usages, s))),
        (
            "project_overview",
            Box::new(move |s| render_project_overview(&overview, s)),
        ),
        ("graph_schema", Box::new(move |s| render_graph_schema(&schema, s))),
        (
            "shortest_path",
            Box::new(move |s| render_shortest_path(&missing_path, s, false)),
        ),
    ];

    for (name, render) in &cases {
        let md = render(Render::Markdown);
        assert!(
            !md.contains('\x1b'),
            "{} markdown output leaked an ANSI escape",
            name
        );
        let ansi = render(Render::Ansi);
        assert!(
            !ansi.contains('`'),
            "{} ANSI output leaked a markdown backtick",
            name
        );
    }
}

// ── shortest_path ───────────────────────────────────────────────────────────
//
// Edges are directed, and the reverse-direction retry is the part that has to
// stay honest: an answer that walked backwards is still useful ("these two are
// connected"), but reporting it as a forward path would tell an agent that a
// change to the source can reach the target when the dependency runs the other
// way. So every reversed result is flagged, and `strict` turns the retry off.

#[test]
fn a_forward_path_is_found_and_not_flagged_as_reversed() {
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:1:caller",
        "function:src/a.rs:7:callee",
        false,
    );
    assert!(r.found);
    assert!(!r.reversed, "this path runs the way the edge points");
    assert_eq!(r.length, Some(1));
    assert_eq!(
        r.path,
        vec!["function:src/a.rs:1:caller", "function:src/a.rs:7:callee"]
    );
}

#[test]
fn a_backwards_query_is_answered_but_flagged() {
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:7:callee",
        "function:src/a.rs:1:caller",
        false,
    );
    assert!(r.found);
    assert!(
        r.reversed,
        "the walk went against the edges and must say so"
    );
    // The path is reported in the direction it was actually walked.
    assert_eq!(
        r.path,
        vec!["function:src/a.rs:1:caller", "function:src/a.rs:7:callee"]
    );
}

#[test]
fn strict_refuses_to_walk_backwards() {
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:7:callee",
        "function:src/a.rs:1:caller",
        true,
    );
    assert!(!r.found, "strict asks a directed question and takes no for an answer");
    assert!(!r.reversed);
    assert_eq!(r.length, None, "no path means no length, not zero");
    assert!(r.path.is_empty());
}

#[test]
fn every_node_on_the_path_is_resolved_to_a_symbol() {
    // The ids alone are not enough for an agent to act on — it needs the
    // name, type and location to decide what to read next.
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:1:caller",
        "function:src/a.rs:7:callee",
        false,
    );
    assert_eq!(r.nodes.len(), r.path.len());
    assert_eq!(r.nodes[0].name, "caller");
    assert_eq!(r.nodes[1].name, "callee");
}

#[test]
fn an_unknown_endpoint_finds_nothing_rather_than_panicking() {
    let g = fixture();
    let r = shortest_path(&g, "function:src/a.rs:1:caller", "nope:does:not:exist", false);
    assert!(!r.found);
    assert!(r.nodes.is_empty());
}

#[test]
fn a_node_to_itself_is_a_zero_hop_path() {
    let g = fixture();
    let id = "function:src/a.rs:1:caller";
    let r = shortest_path(&g, id, id, false);
    assert!(r.found);
    assert_eq!(r.length, Some(0));
    assert_eq!(r.path, vec![id]);
}

#[test]
fn rendering_a_found_path_names_both_ends_and_the_hop_count() {
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:1:caller",
        "function:src/a.rs:7:callee",
        false,
    );
    let out = render_shortest_path(&r, Render::Markdown, false);
    assert!(out.contains("caller"), "{out}");
    assert!(out.contains("callee"), "{out}");
    assert!(out.contains("1 hop(s)"), "{out}");
    assert!(out.contains("get_code"), "the next action must be named: {out}");
}

#[test]
fn rendering_a_reversed_path_says_it_went_backwards() {
    // Silently rendering this like a forward path is the misreading the
    // flag exists to prevent.
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:7:callee",
        "function:src/a.rs:1:caller",
        false,
    );
    let out = render_shortest_path(&r, Render::Markdown, false);
    assert!(out.contains("reverse direction"), "{out}");
    assert!(out.contains("no forward path existed"), "{out}");
}

#[test]
fn rendering_a_miss_explains_which_question_was_asked() {
    let g = fixture();
    let miss = shortest_path(&g, "function:src/a.rs:7:callee", "function:src/a.rs:1:caller", true);

    let strict_out = render_shortest_path(&miss, Render::Markdown, true);
    assert!(
        strict_out.contains("reverse direction not tried"),
        "a strict miss is a narrower claim than a plain one: {strict_out}"
    );

    let loose_out = render_shortest_path(&miss, Render::Markdown, false);
    assert!(loose_out.contains("in either direction"), "{loose_out}");
    // The hint has to name a command that exists.
    assert!(loose_out.contains("traverse"), "{loose_out}");
}

#[test]
fn both_render_styles_produce_the_same_facts() {
    let g = fixture();
    let r = shortest_path(
        &g,
        "function:src/a.rs:1:caller",
        "function:src/a.rs:7:callee",
        false,
    );
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_shortest_path(&r, style, false);
        assert!(out.contains("caller") && out.contains("callee"), "{out}");
        assert!(out.contains("1 hop(s)"), "{out}");
    }
}

// ── project_overview ────────────────────────────────────────────────────────
//
// The orientation call an agent makes first in an unfamiliar repo. Two of its
// counts are opinionated rather than mechanical, and both would look correct
// if they broke: `Contains` is excluded from inbound degree so a hotspot means
// "much code depends on this" rather than "this file is big", and File and
// Folder nodes are excluded from the per-file symbol tally so a file does not
// count itself.

/// `fixture()` plus a second file, a `References` edge into the callee from
/// outside, and metrics on both functions — enough for every tally to have
/// something to get wrong.
fn overview_fixture() -> GraphData {
    let mut g = fixture();
    g.nodes.push(node(
        "file:src/b.rs",
        "b.rs",
        GraphNodeType::File,
        "src/b.rs",
        None,
    ));
    let mut other = node(
        "function:src/b.rs:1:other",
        "other",
        GraphNodeType::Function,
        "src/b.rs",
        Some((1, 3)),
    );
    other.metrics = Some(crate::types::SymbolMetrics {
        loc: 3,
        params: 1,
        max_nesting: 1,
        ..Default::default()
    });
    g.nodes.push(other);

    if let Some(caller) = g
        .nodes
        .iter_mut()
        .find(|n| n.id == "function:src/a.rs:1:caller")
    {
        caller.metrics = Some(crate::types::SymbolMetrics {
            loc: 40,
            params: 3,
            max_nesting: 4,
            ..Default::default()
        });
    }

    g.edges.push(edge(
        "function:src/b.rs:1:other",
        "function:src/a.rs:7:callee",
        GraphEdgeType::References,
    ));
    g.edges.push(edge(
        "file:src/b.rs",
        "function:src/b.rs:1:other",
        GraphEdgeType::Contains,
    ));
    g
}

#[test]
fn an_overview_counts_the_whole_graph() {
    let g = overview_fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/repo/graph.json"));
    assert_eq!(r.node_count, g.nodes.len());
    assert_eq!(r.edge_count, g.edges.len());
    assert_eq!(r.repo_root, "/repo");
    assert_eq!(r.graph_path, "/repo/graph.json");
}

#[test]
fn node_and_edge_types_are_tallied_biggest_first() {
    let g = overview_fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));

    let functions = r.node_types.iter().find(|t| t.name == "Function").expect("functions");
    assert_eq!(functions.count, 3);
    let files = r.node_types.iter().find(|t| t.name == "File").expect("files");
    assert_eq!(files.count, 2);
    assert!(
        r.node_types[0].count >= r.node_types[1].count,
        "tallies are ordered by count"
    );

    let contains = r.edge_types.iter().find(|t| t.name == "Contains").expect("contains");
    assert_eq!(contains.count, 2);
}

#[test]
fn structural_containment_does_not_make_something_a_hotspot() {
    // A file `Contains` every symbol in it, so counting those edges would
    // rank files by size and call it dependency. The callee here is reached
    // by a real Calls and a real References edge; the file nodes are reached
    // only by Contains and must not appear.
    let g = overview_fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));

    let top = r.hotspots.first().expect("a hotspot");
    assert_eq!(top.symbol.id, "function:src/a.rs:7:callee");
    assert_eq!(top.in_degree, 2, "one Calls plus one References");
    assert!(
        !r.hotspots.iter().any(|h| h.symbol.id.starts_with("file:")),
        "a file reached only by Contains is not depended upon"
    );
}

#[test]
fn a_file_does_not_count_itself_among_its_symbols() {
    // File and Folder nodes carry a `file` too, so including them would add
    // one phantom symbol to every file in the repo.
    let g = overview_fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));

    let a = r.biggest_files.iter().find(|t| t.name == "src/a.rs").expect("a.rs");
    assert_eq!(a.count, 2, "caller and callee, not the File node as well");
    let b = r.biggest_files.iter().find(|t| t.name == "src/b.rs").expect("b.rs");
    assert_eq!(b.count, 1);
}

#[test]
fn the_complexity_list_leads_with_the_longest_symbol() {
    let g = overview_fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));

    assert_eq!(r.complexity.len(), 2, "only symbols carrying metrics appear");
    assert_eq!(r.complexity[0].symbol.name, "caller");
    assert_eq!(r.complexity[0].loc, 40);
    assert_eq!(r.complexity[1].symbol.name, "other");
}

#[test]
fn an_overview_of_an_empty_graph_reports_zero_rather_than_failing() {
    let empty = GraphData {
        nodes: Vec::new(),
        edges: Vec::new(),
        stats: None,
        resolution: None,
    };
    let r = project_overview(&empty, Path::new("/repo"), Path::new("/g.json"));
    assert_eq!((r.node_count, r.edge_count), (0, 0));
    assert!(r.hotspots.is_empty() && r.complexity.is_empty());
    assert!(r.index.is_none(), "no stats means no index summary, not a zeroed one");
}

#[test]
fn rendering_an_overview_names_the_repo_and_its_hotspots() {
    let g = overview_fixture();
    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_project_overview(&r, style);
        assert!(out.contains("Project overview"), "{out}");
        assert!(out.contains("/repo"), "{out}");
        assert!(out.contains("callee"), "the top hotspot is the point: {out}");
    }
}

// ── traverse: the knobs, and what happens at their edges ────────────────────
//
// The existing cases cover direction, edge-type filtering and pattern seeds.
// What was left are the bounds and the degenerate inputs — the places where a
// wrong answer is a plausible one. A hop count that silently became 5 walks a
// far larger neighbourhood than asked for and returns a believable result; a
// `both` direction that behaves as `outbound` simply omits every caller.

/// A three-hop chain plus a branch, so hop bounds have something to bite on.
///   root → mid → leaf
///   root → side
fn chain_fixture() -> GraphData {
    let f = |id: &str, name: &str| node(id, name, GraphNodeType::Function, "src/c.rs", Some((1, 2)));
    GraphData {
        nodes: vec![
            f("fn:root", "root"),
            f("fn:mid", "mid"),
            f("fn:leaf", "leaf"),
            f("fn:side", "side"),
        ],
        edges: vec![
            edge("fn:root", "fn:mid", GraphEdgeType::Calls),
            edge("fn:mid", "fn:leaf", GraphEdgeType::Calls),
            edge("fn:root", "fn:side", GraphEdgeType::Calls),
        ],
        stats: None,
        resolution: None,
    }
}

fn walked(g: &GraphData, p: TraverseParams) -> Vec<String> {
    let mut names: Vec<String> = traverse(g, &p)
        .nodes
        .iter()
        .map(|n| n.symbol.name.clone())
        .collect();
    names.sort();
    names
}

#[test]
fn hops_bound_how_far_the_walk_reaches() {
    let g = chain_fixture();
    let from_root = |hops| {
        walked(
            &g,
            TraverseParams {
                node_id: vec!["fn:root".into()],
                hops: Some(hops),
                ..Default::default()
            },
        )
    };
    assert_eq!(from_root(1), vec!["mid", "root", "side"]);
    assert_eq!(from_root(2), vec!["leaf", "mid", "root", "side"]);
}

#[test]
fn a_hop_count_outside_its_range_is_clamped_not_rejected() {
    let g = chain_fixture();
    // Zero would otherwise return the seed alone, which reads as "nothing
    // depends on this". The floor of 1 means the answer is always a walk.
    let zero = walked(
        &g,
        TraverseParams {
            node_id: vec!["fn:root".into()],
            hops: Some(0),
            ..Default::default()
        },
    );
    assert!(zero.contains(&"mid".to_string()), "hops=0 is clamped up to 1: {zero:?}");

    // And the ceiling holds, so a huge number cannot walk an entire repo.
    let huge = walked(
        &g,
        TraverseParams {
            node_id: vec!["fn:root".into()],
            hops: Some(9_999),
            ..Default::default()
        },
    );
    assert_eq!(huge.len(), 4, "the whole chain, and no error: {huge:?}");
}

#[test]
fn the_default_direction_is_outbound() {
    let g = chain_fixture();
    // Not `both`. Asking what a symbol depends on is the common question,
    // and `find_usages` is the pinned-inbound tool for the other one.
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["fn:mid".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    let names: Vec<&str> = r.nodes.iter().map(|n| n.symbol.name.as_str()).collect();
    assert!(names.contains(&"leaf"), "outbound reaches what mid calls");
    assert!(!names.contains(&"root"), "and not what calls mid: {names:?}");
}

#[test]
fn both_directions_reach_callers_and_callees_at_once() {
    let g = chain_fixture();
    let names = walked(
        &g,
        TraverseParams {
            node_id: vec!["fn:mid".into()],
            hops: Some(1),
            direction: Some("both".into()),
            ..Default::default()
        },
    );
    assert_eq!(names, vec!["leaf", "mid", "root"]);
}

#[test]
fn an_unrecognised_direction_falls_back_to_outbound() {
    let g = chain_fixture();
    let typo = walked(
        &g,
        TraverseParams {
            node_id: vec!["fn:mid".into()],
            hops: Some(1),
            direction: Some("sideways".into()),
            ..Default::default()
        },
    );
    let default = walked(
        &g,
        TraverseParams {
            node_id: vec!["fn:mid".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(typo, default);
}

#[test]
fn several_seeds_make_one_merged_walk() {
    // Not one walk per seed concatenated: a node reachable from two seeds
    // appears once, at its distance from the nearest of them.
    let g = chain_fixture();
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["fn:root".into(), "fn:mid".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    let mids: Vec<_> = r.nodes.iter().filter(|n| n.symbol.name == "mid").collect();
    assert_eq!(mids.len(), 1, "no duplicate rows for a shared node");
    assert_eq!(mids[0].distance, 0, "mid is a seed, so distance 0 wins");
    let leaf = r.nodes.iter().find(|n| n.symbol.name == "leaf").expect("leaf");
    assert_eq!(leaf.distance, 1, "one hop from the nearest seed, not two from root");
}

#[test]
fn a_seed_that_names_nothing_is_reported_with_a_reason() {
    let g = chain_fixture();
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["fn:root".into(), "nosuchthing".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    assert!(!r.ok(), "a partially-resolved walk is not a clean result");
    assert_eq!(r.missing, vec!["nosuchthing"]);
    assert_eq!(r.notes.len(), r.missing.len(), "one explanation per miss");
    // The walk still runs for the seeds that did resolve.
    assert!(r.nodes.iter().any(|n| n.symbol.name == "root"));
}

#[test]
fn no_seeds_at_all_is_an_empty_walk_rather_than_the_whole_graph() {
    let g = chain_fixture();
    let r = traverse(&g, &TraverseParams { hops: Some(2), ..Default::default() });
    assert!(r.nodes.is_empty(), "an unseeded walk must not fan out");
    assert!(r.edges.is_empty());
}

#[test]
fn a_cycle_terminates_and_reports_each_node_once() {
    let f = |id: &str, name: &str| node(id, name, GraphNodeType::Function, "src/c.rs", Some((1, 2)));
    let g = GraphData {
        nodes: vec![f("fn:a", "a"), f("fn:b", "b")],
        edges: vec![
            edge("fn:a", "fn:b", GraphEdgeType::Calls),
            edge("fn:b", "fn:a", GraphEdgeType::Calls),
        ],
        stats: None,
        resolution: None,
    };
    let names = walked(
        &g,
        TraverseParams {
            node_id: vec!["fn:a".into()],
            hops: Some(5),
            ..Default::default()
        },
    );
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn the_edges_reported_are_those_among_the_nodes_reached() {
    let g = chain_fixture();
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["fn:root".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    // root→mid and root→side were walked; mid→leaf was not, because leaf is
    // outside the hop radius and an edge to a node not in the result would
    // draw a line to nothing.
    assert_eq!(r.edges.len(), 2, "{:?}", r.edges);
    assert!(r.edges.iter().all(|e| e.source == "fn:root"));
}

#[test]
fn rendering_a_walk_names_the_seed_and_what_it_reached() {
    let g = chain_fixture();
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["fn:root".into()],
            hops: Some(2),
            ..Default::default()
        },
    );
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_traverse(&r, style);
        assert!(out.contains("root"), "{out}");
        assert!(out.contains("leaf"), "{out}");
    }
}

#[test]
fn rendering_a_walk_with_a_missing_seed_explains_the_miss() {
    let g = chain_fixture();
    let r = traverse(
        &g,
        &TraverseParams {
            node_id: vec!["nosuchthing".into()],
            hops: Some(1),
            ..Default::default()
        },
    );
    let out = render_traverse(&r, Render::Markdown);
    assert!(out.contains("nosuchthing"), "the miss is named: {out}");
}

// ── the shared helpers every tool is built from ─────────────────────────────
//
// Small enough to look obviously right, and wrong in ways that surface as a
// tool answering the wrong question rather than as an error: an id mistaken
// for a name gets searched for, a `file:` prefix left on a path matches
// nothing, and a tool missing from the discovery tables is invisible to every
// agent that reads them.

#[test]
fn an_id_is_told_from_a_name_by_its_colons() {
    // The CLI takes bare positionals where MCP has separate params, so this
    // heuristic decides which lookup runs.
    assert!(looks_like_node_id("function:src/a.rs:1:caller"));
    assert!(looks_like_node_id("file:src/a.rs"));
    assert!(!looks_like_node_id("caller"), "a bare name is not an id");
    assert!(!looks_like_node_id("src/a.rs"), "nor is a path");
}

#[test]
fn a_wildcard_is_never_an_id_even_with_colons() {
    // `*:*:login` asks to search ids, not to look one up. Treating it as an
    // id would look for a node literally named that and find nothing.
    assert!(!looks_like_node_id("*:*:login"));
    assert!(!looks_like_node_id("function:src/*.rs:handler"));
}

#[test]
fn a_file_id_prefix_is_stripped_but_a_bare_path_is_untouched() {
    assert_eq!(strip_file_id_prefix("file:src/a.rs"), "src/a.rs");
    assert_eq!(strip_file_id_prefix("src/a.rs"), "src/a.rs");
    // Only the leading occurrence, and only at the start.
    assert_eq!(strip_file_id_prefix("src/file:weird.rs"), "src/file:weird.rs");
}

#[test]
fn a_location_degrades_field_by_field() {
    let with_lines = node("x", "x", GraphNodeType::Function, "src/a.rs", Some((3, 9)));
    assert_eq!(node_loc(&with_lines), "src/a.rs:3-9");

    // A File node carries no range; printing `src/a.rs:?-?` for one would be
    // noise on every file in a result.
    let no_lines = node("f", "a.rs", GraphNodeType::File, "src/a.rs", None);
    assert_eq!(node_loc(&no_lines), "src/a.rs");

    let mut no_file = node("x", "x", GraphNodeType::Function, "src/a.rs", Some((1, 2)));
    no_file.file = None;
    assert_eq!(node_loc(&no_file), "(no file)");
}

#[test]
fn a_half_known_range_says_which_half_is_missing() {
    let mut partial = node("x", "x", GraphNodeType::Function, "src/a.rs", Some((3, 9)));
    partial.end_line = None;
    assert_eq!(node_loc(&partial), "src/a.rs:3-?");
}

#[test]
fn node_and_edge_types_render_as_their_canonical_names() {
    // These strings are compared against user input by the CLI's `-t` filter
    // and printed into every result, so they are a contract, not a debug
    // format.
    assert_eq!(node_type_str(&GraphNodeType::Function), "Function");
    assert_eq!(node_type_str(&GraphNodeType::File), "File");
    assert_eq!(edge_type_str(&GraphEdgeType::Calls), "Calls");
    assert_eq!(edge_type_str(&GraphEdgeType::Contains), "Contains");
}

#[test]
fn every_advertised_tool_carries_a_summary_and_an_example() {
    // `ug api` and GET /api/tools publish both. A tool added to the table
    // without either shows up in discovery as a blank row, which reads as a
    // broken endpoint rather than a missing doc string.
    for (name, summary) in AGENT_TOOLS.iter().chain(STORE_BACKED_AGENT_TOOLS.iter()) {
        assert!(!summary.is_empty(), "{name} has no summary");
        let example = tool_example(name);
        assert!(
            example.starts_with('{') && example.ends_with('}'),
            "{name}'s example is not a JSON object: {example}"
        );
        serde_json::from_str::<serde_json::Value>(example)
            .unwrap_or_else(|e| panic!("{name}'s example does not parse: {e}"));
        assert_eq!(tool_summary(name), *summary, "{name} summary lookup disagrees");
    }
}

#[test]
fn only_graph_backed_tools_claim_run_tool_can_answer_them() {
    // The split is the point of the two tables: `analyze` is advertised to
    // agents but needs the store, so `run_tool` must not claim it.
    for (name, _) in AGENT_TOOLS {
        assert!(is_agent_tool(name), "{name} should be run_tool-dispatchable");
    }
    for (name, _) in STORE_BACKED_AGENT_TOOLS {
        assert!(
            !is_agent_tool(name),
            "{name} needs the store, so run_tool cannot answer it"
        );
    }
    assert!(!is_agent_tool("not_a_tool"));
    assert_eq!(tool_summary("not_a_tool"), "", "an unknown tool has no summary");
}

#[test]
fn a_batch_param_accepts_one_value_or_many() {
    // Documented to take either, and callers use both, so the transports do
    // not branch on the shape.
    let one: TraverseParams =
        serde_json::from_value(serde_json::json!({ "node_id": "fn:a" })).expect("one");
    assert_eq!(one.node_id, vec!["fn:a"]);

    let many: TraverseParams =
        serde_json::from_value(serde_json::json!({ "node_id": ["fn:a", "fn:b"] })).expect("many");
    assert_eq!(many.node_id, vec!["fn:a", "fn:b"]);

    let absent: TraverseParams = serde_json::from_value(serde_json::json!({})).expect("absent");
    assert!(absent.node_id.is_empty());
}

#[test]
fn the_mcp_spelling_of_a_seed_param_still_works() {
    // `startNodeIds` was the original MCP name and is kept as an alias;
    // dropping it would break every agent built against the old schema.
    for key in ["node_id", "nodeId", "nodeIds", "startNodeIds"] {
        let p: TraverseParams =
            serde_json::from_value(serde_json::json!({ key: "fn:a" })).unwrap_or_else(|e| panic!("{key}: {e}"));
        assert_eq!(p.node_id, vec!["fn:a"], "{key}");
    }
}

// ── get_code: the line window, and the answers that are not code ────────────
//
// The live-vs-index preference is already covered above. What was left is
// everything around it: the `range` dialect, what a bad window does, and the
// several ways a slice comes back as an explanation rather than source. Each
// of those is a case where returning *something* plausible — line 1 to EOF,
// say — would be worse than saying no.

/// A repo on disk with one four-line file, and a graph naming it.
fn disk_repo() -> (tempfile::TempDir, GraphData) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    let g = GraphData {
        nodes: vec![
            node("file:src/a.rs", "a.rs", GraphNodeType::File, "src/a.rs", None),
            node(
                "function:src/a.rs:2:mid",
                "mid",
                GraphNodeType::Function,
                "src/a.rs",
                Some((2, 3)),
            ),
        ],
        edges: vec![],
        stats: None,
        resolution: None,
    };
    (dir, g)
}

fn code_for(repo: &Path, g: &GraphData, p: GetCodeParams) -> GetCodeResult {
    get_code(g, SourceCtx::repo_only(repo), &p)
}

#[test]
fn a_file_and_line_range_reads_exactly_those_lines() {
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/a.rs".into()),
            start_line: Some(2),
            end_line: Some(3),
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    let code = r.slices[0].code.as_deref().unwrap();
    assert!(code.contains("two") && code.contains("three"), "{code:?}");
    assert!(!code.contains("one") && !code.contains("four"), "{code:?}");
}

#[test]
fn the_range_dialect_means_the_same_as_explicit_line_numbers() {
    let (dir, g) = disk_repo();
    let explicit = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/a.rs".into()),
            start_line: Some(2),
            end_line: Some(3),
            ..Default::default()
        },
    );
    let via_range = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/a.rs".into()),
            range: Some("2-3".into()),
            ..Default::default()
        },
    );
    assert_eq!(via_range.slices[0].code, explicit.slices[0].code);
}

#[test]
fn a_malformed_range_is_reported_rather_than_silently_ignored() {
    // Ignoring it would serve line 1 to EOF as though that were the request,
    // and an agent would summarise the wrong part of the file.
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/a.rs".into()),
            range: Some("not-a-range".into()),
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert!(r.slices[0].error.is_some(), "{:?}", r.slices[0]);
}

#[test]
fn asking_for_neither_a_node_nor_a_file_says_which_to_pass() {
    let (dir, g) = disk_repo();
    let r = code_for(dir.path(), &g, GetCodeParams::default());
    assert!(!r.ok());
    let err = r.slices[0].error.as_deref().unwrap();
    assert!(err.contains("node_id") && err.contains("file"), "{err}");
}

#[test]
fn a_file_that_is_not_there_explains_itself() {
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/missing.rs".into()),
            ..Default::default()
        },
    );
    assert!(!r.ok());
    assert!(r.slices[0].error.is_some(), "{:?}", r.slices[0]);
}

#[test]
fn a_file_id_is_accepted_where_a_path_is() {
    // The other tools hand back `file:src/a.rs` ids, so get_code has to take
    // one without the caller stripping the prefix first.
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("file:src/a.rs".into()),
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    assert!(r.slices[0].code.as_deref().unwrap().contains("one"));
}

#[test]
fn several_ids_come_back_as_several_slices_in_order() {
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            node_id: vec!["file:src/a.rs".into(), "function:src/a.rs:2:mid".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.slices.len(), 2, "one slice per id, batched into one call");
}

#[test]
fn a_char_budget_truncates_and_says_how_much_it_dropped() {
    // An agent that cannot tell a truncated body from a short one will
    // describe a function by its first line and call that the whole thing.
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/a.rs".into()),
            max_chars: Some(5),
            ..Default::default()
        },
    );
    assert!(r.ok(), "{:?}", r.slices[0].error);
    assert!(
        r.slices[0].truncated_chars > 0,
        "truncation must be reported: {:?}",
        r.slices[0]
    );
}

#[test]
fn a_slice_reports_the_files_length_in_newline_separated_segments() {
    // The total is how a caller knows whether to ask for the next window,
    // and it counts `split('\n')` segments rather than visible lines. A file
    // ending in a newline therefore reports one more than it looks like it
    // has: "one\ntwo\nthree\nfour\n" is four lines and five segments.
    //
    // That is the implementation the slicing needs — joining all the
    // segments reproduces the file byte for byte, where `.lines()` would
    // silently drop the trailing newline on the way back. The cost is that
    // the last segment is empty, so a caller paging to the reported end gets
    // a blank final window rather than an error.
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/a.rs".into()),
            start_line: Some(1),
            end_line: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(r.slices[0].total_lines, Some(5));
}

#[test]
fn rendering_code_names_the_file_and_the_lines() {
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            node_id: vec!["function:src/a.rs:2:mid".into()],
            ..Default::default()
        },
    );
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_get_code(&r, style);
        assert!(out.contains("src/a.rs"), "{out}");
    }
}

#[test]
fn rendering_a_failed_slice_shows_the_reason() {
    let (dir, g) = disk_repo();
    let r = code_for(
        dir.path(),
        &g,
        GetCodeParams {
            file: Some("src/missing.rs".into()),
            ..Default::default()
        },
    );
    let out = render_get_code(&r, Render::Markdown);
    assert!(out.contains("missing.rs"), "the failure names the file: {out}");
}

// ── find_usages: depth, filters, and the boundary summary ───────────────────
//
// The tool a caller reaches for before changing a symbol, so its bounds are
// the ones a refactor is planned against. Under-reporting reads as "nothing
// depends on this", which is the most expensive wrong answer in the toolbox.

fn boundary(kind: &str, dir: crate::types::BoundaryDirection, detail: Option<&str>) -> crate::types::Boundary {
    crate::types::Boundary {
        kind: kind.into(),
        direction: dir,
        protocol: "http".into(),
        detail: detail.map(str::to_string),
        source: "test".into(),
    }
}

/// root ← mid ← leaf, so transitive depth has something to reach.
fn users_fixture() -> GraphData {
    let f = |id: &str, name: &str| node(id, name, GraphNodeType::Function, "src/u.rs", Some((1, 2)));
    GraphData {
        nodes: vec![f("fn:root", "root"), f("fn:mid", "mid"), f("fn:leaf", "leaf")],
        edges: vec![
            edge("fn:mid", "fn:root", GraphEdgeType::Calls),
            edge("fn:leaf", "fn:mid", GraphEdgeType::Calls),
        ],
        stats: None,
        resolution: None,
    }
}

fn users_of(g: &GraphData, p: FindUsagesParams) -> Vec<String> {
    let r = find_usages(g, SourceCtx::repo_only(Path::new(NO_REPO)), &p);
    let mut names: Vec<String> = r.nodes[0]
        .users
        .iter()
        .map(|u| u.symbol.name.clone())
        .collect();
    names.sort();
    names
}

#[test]
fn the_default_depth_is_direct_users_only() {
    let g = users_fixture();
    assert_eq!(
        users_of(&g, FindUsagesParams { node_id: vec!["fn:root".into()], ..Default::default() }),
        vec!["mid"],
        "leaf calls mid, not root"
    );
}

#[test]
fn more_hops_reach_transitive_users() {
    let g = users_fixture();
    assert_eq!(
        users_of(
            &g,
            FindUsagesParams {
                node_id: vec!["fn:root".into()],
                hops: Some(2),
                ..Default::default()
            }
        ),
        vec!["leaf", "mid"]
    );
}

#[test]
fn depth_is_recorded_per_user_so_direct_and_indirect_are_told_apart() {
    let g = users_fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams {
            node_id: vec!["fn:root".into()],
            hops: Some(2),
            ..Default::default()
        },
    );
    let depth = |name: &str| {
        r.nodes[0]
            .users
            .iter()
            .find(|u| u.symbol.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .depth
    };
    assert_eq!(depth("mid"), 1);
    assert_eq!(depth("leaf"), 2);
}

#[test]
fn an_edge_type_filter_narrows_which_users_count() {
    let g = users_fixture();
    // Only `Contains` edges — there are none, so nothing uses root by that
    // relationship. An empty answer here is correct, not a failure.
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams {
            node_id: vec!["fn:root".into()],
            edge_types: vec!["contains".into()],
            ..Default::default()
        },
    );
    assert!(r.nodes[0].users.is_empty());
}

#[test]
fn a_symbol_nothing_uses_reports_no_users_rather_than_an_error() {
    let g = users_fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams { node_id: vec!["fn:leaf".into()], ..Default::default() },
    );
    assert!(r.ok(), "resolving fine and finding nothing is a clean result");
    assert!(r.nodes[0].users.is_empty());
}

#[test]
fn a_boundary_user_is_summarised_by_kind() {
    // "Who calls this" and "does changing it escape the system" are different
    // questions, and the summary is what answers the second one without
    // re-listing every route.
    use crate::types::BoundaryDirection;
    let mut g = users_fixture();
    if let Some(mid) = g.nodes.iter_mut().find(|n| n.id == "fn:mid") {
        mid.boundaries = vec![boundary("http.endpoint", BoundaryDirection::Inbound, Some("GET /x"))];
    }
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams { node_id: vec!["fn:root".into()], ..Default::default() },
    );
    let out = render_find_usages(&r, Render::Markdown);
    assert!(out.contains("system boundaries"), "{out}");
    assert!(out.contains("http.endpoint"), "the kind is what is at stake: {out}");
}

#[test]
fn no_boundary_users_means_no_summary_line() {
    let g = users_fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams { node_id: vec!["fn:root".into()], ..Default::default() },
    );
    let out = render_find_usages(&r, Render::Markdown);
    assert!(
        !out.contains("system boundaries"),
        "an empty breakdown must not print a header: {out}"
    );
}

#[test]
fn rendering_usages_names_the_subject_and_its_callers() {
    let g = users_fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams { node_id: vec!["fn:root".into()], ..Default::default() },
    );
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_find_usages(&r, style);
        assert!(out.contains("root") && out.contains("mid"), "{out}");
    }
}

#[test]
fn a_seed_that_resolves_to_nothing_is_not_a_clean_result() {
    let g = users_fixture();
    let r = find_usages(
        &g,
        SourceCtx::repo_only(Path::new(NO_REPO)),
        &FindUsagesParams { node_id: vec!["nosuchthing".into()], ..Default::default() },
    );
    assert!(!r.ok());
    let out = render_find_usages(&r, Render::Markdown);
    assert!(out.contains("nosuchthing"), "{out}");
}

// ── project_overview: the parts that only appear on a real index ────────────

#[test]
fn an_overview_reports_the_index_summary_when_the_graph_carries_one() {
    use crate::types::IndexStats;
    let mut g = overview_fixture();
    g.stats = Some(IndexStats {
        graph_schema_version: crate::types::GRAPH_SCHEMA_VERSION,
        total_files: 2,
        cached_files: 1,
        total_symbols: 3,
        total_folders: 1,
        total_lines: 120,
        indexing_time_ms: 45,
        last_indexed_at: 1_700_000_000,
        repo_root: "/repo".into(),
    });
    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));

    let idx = r.index.expect("the summary follows the stats");
    assert_eq!((idx.files, idx.cached_files, idx.symbols), (2, 1, 3));
    assert_eq!(idx.lines, 120);
}

#[test]
fn the_language_breakdown_comes_from_the_shallowest_folder() {
    // The repo-root folder node is the one the indexer put the whole-repo
    // breakdown on. Reading a deeper folder's would describe a subdirectory
    // and call it the project.
    use crate::types::{FolderClassification, GraphNodeFolderMeta};
    use std::collections::BTreeMap;

    let folder = |id: &str, depth: u32, langs: &[(&str, u32)]| {
        let mut n = node(id, id, GraphNodeType::Folder, "src", None);
        n.folder = Some(GraphNodeFolderMeta {
            depth,
            parent: None,
            classification: Some(FolderClassification::Source),
            readme: None,
            total_files: 2,
            language_breakdown: langs
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect::<BTreeMap<_, _>>(),
            summary: None,
        });
        n
    };

    let mut g = overview_fixture();
    g.nodes.push(folder("folder:src/deep", 3, &[("python", 99)]));
    g.nodes.push(folder("folder:.", 0, &[("rust", 10), ("typescript", 4)]));

    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));
    let names: Vec<&str> = r.languages.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, vec!["rust", "typescript"], "biggest first, from depth 0");
    assert!(
        !names.contains(&"python"),
        "a deep folder's breakdown is not the project's: {names:?}"
    );
    assert_eq!(r.kb_type.as_deref(), Some("source"));
}

// ── file_outline ────────────────────────────────────────────────────────────

#[test]
fn an_outline_lists_a_files_symbols_in_line_order() {
    let g = fixture();
    let r = file_outline(&g, &FileOutlineParams { file: vec!["src/a.rs".into()], ..Default::default() });
    assert!(r.ok(), "{:?}", r.files[0].error);
    let names: Vec<&str> = r.files[0].symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["caller", "callee"], "line order, not id order");
}

#[test]
fn an_outline_accepts_a_bare_basename() {
    // Typing the repo-relative path is what the suffix match exists to avoid.
    let g = fixture();
    let r = file_outline(&g, &FileOutlineParams { file: vec!["a.rs".into()], ..Default::default() });
    assert!(r.ok(), "{:?}", r.files[0].error);
    assert_eq!(r.files[0].symbols.len(), 2);
}

#[test]
fn an_outline_accepts_a_file_node_id() {
    let g = fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams { file: vec!["file:src/a.rs".into()], ..Default::default() },
    );
    assert!(r.ok(), "{:?}", r.files[0].error);
}

#[test]
fn an_outline_of_an_unknown_file_explains_itself() {
    let g = fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams { file: vec!["src/nothing.rs".into()], ..Default::default() },
    );
    assert!(!r.ok());
    assert!(r.files[0].error.is_some(), "{:?}", r.files[0]);
}

#[test]
fn several_files_are_outlined_in_one_call() {
    let g = overview_fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams {
            file: vec!["src/a.rs".into(), "src/b.rs".into()],
            ..Default::default()
        },
    );
    assert_eq!(r.files.len(), 2, "batched rather than one call per file");
}

#[test]
fn rendering_an_outline_names_the_file_and_its_symbols() {
    let g = fixture();
    let r = file_outline(&g, &FileOutlineParams { file: vec!["src/a.rs".into()], ..Default::default() });
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_file_outline(&r, style);
        assert!(out.contains("src/a.rs"), "{out}");
        assert!(out.contains("caller") && out.contains("callee"), "{out}");
    }
}

#[test]
fn rendering_an_outline_miss_shows_the_reason() {
    let g = fixture();
    let r = file_outline(
        &g,
        &FileOutlineParams { file: vec!["src/nothing.rs".into()], ..Default::default() },
    );
    let out = render_file_outline(&r, Render::Markdown);
    assert!(out.contains("nothing.rs"), "{out}");
}

// ── find_symbols: the boundary filter ───────────────────────────────────────

#[test]
fn the_boundary_filter_keeps_only_the_systems_edges() {
    // "What does this service expose and consume" is the fastest way to
    // orient in an unfamiliar repo, and it is this filter plus `name: "*"`.
    // Without the filter the same query returns every symbol there is.
    use crate::types::BoundaryDirection;
    let mut g = users_fixture();
    if let Some(mid) = g.nodes.iter_mut().find(|n| n.id == "fn:mid") {
        mid.boundaries = vec![boundary("http.endpoint", BoundaryDirection::Inbound, Some("GET /x"))];
    }

    let all = find_symbols(
        &g,
        &FindSymbolsParams { name: vec!["*".into()], ..Default::default() },
    );
    assert!(all.queries[0].items.len() > 1, "the unfiltered query sees everything");

    let edges = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["*".into()],
            boundary: true,
            ..Default::default()
        },
    );
    let names: Vec<&str> = edges.queries[0].items.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["mid"], "only the symbol carrying a boundary");
}

#[test]
fn a_boundary_hit_says_which_surface_it_is() {
    // The kind and the route are the answer; a bare list of names would not
    // tell a reader what the service actually exposes.
    use crate::types::BoundaryDirection;
    let mut g = users_fixture();
    if let Some(mid) = g.nodes.iter_mut().find(|n| n.id == "fn:mid") {
        mid.boundaries = vec![boundary("http.endpoint", BoundaryDirection::Inbound, Some("GET /orders"))];
    }
    let r = find_symbols(
        &g,
        &FindSymbolsParams { name: vec!["mid".into()], ..Default::default() },
    );
    let label = r.queries[0].items[0].boundary.as_deref().expect("a boundary label");
    assert!(label.contains("in:"), "direction is part of the label: {label}");
    assert!(label.contains("http.endpoint"), "{label}");
    assert!(label.contains("GET /orders"), "{label}");
}

#[test]
fn a_repo_with_no_boundaries_returns_nothing_rather_than_everything() {
    // The failure that would look like success: a filter that silently
    // stopped applying turns "show me the service surface" into "show me the
    // whole repo".
    let g = users_fixture();
    let r = find_symbols(
        &g,
        &FindSymbolsParams {
            name: vec!["*".into()],
            boundary: true,
            ..Default::default()
        },
    );
    assert!(r.queries[0].items.is_empty());
}

#[test]
fn rendering_a_full_overview_shows_every_section_it_has_data_for() {
    // The render drops a section rather than printing an empty one, so the
    // sections only appear on a graph carrying stats and a root folder — the
    // shape a real `ug gen` produces, and the one the terse fixtures above
    // never reach.
    use crate::types::{FolderClassification, GraphNodeFolderMeta, IndexStats};
    use std::collections::BTreeMap;

    let mut g = overview_fixture();
    g.stats = Some(IndexStats {
        graph_schema_version: crate::types::GRAPH_SCHEMA_VERSION,
        total_files: 2,
        cached_files: 1,
        total_symbols: 3,
        total_folders: 1,
        total_lines: 120,
        indexing_time_ms: 45,
        last_indexed_at: 1_700_000_000,
        repo_root: "/repo".into(),
    });
    let mut root = node("folder:.", ".", GraphNodeType::Folder, "", None);
    root.folder = Some(GraphNodeFolderMeta {
        depth: 0,
        parent: None,
        classification: Some(FolderClassification::Source),
        readme: None,
        total_files: 2,
        language_breakdown: BTreeMap::from([("rust".to_string(), 10u32)]),
        summary: None,
    });
    g.nodes.push(root);

    let r = project_overview(&g, Path::new("/repo"), Path::new("/g.json"));
    for style in [Render::Ansi, Render::Markdown] {
        let out = render_project_overview(&r, style);
        assert!(out.contains("rust"), "the language breakdown: {out}");
        assert!(out.contains("caller"), "the complexity list: {out}");
        assert!(out.contains("callee"), "the hotspots: {out}");
        assert!(out.contains("src/a.rs"), "the biggest files: {out}");
    }
}

#[test]
fn the_graph_schema_reports_the_types_actually_present() {
    // The call an agent makes before any filtered query, because filtering on
    // a type the graph does not contain returns a confident zero rather than
    // an error.
    let g = overview_fixture();
    let r = graph_schema(&g, Path::new("/g.json"));
    let names: Vec<&str> = r.node_types.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"Function"), "{names:?}");
    assert!(names.contains(&"File"), "{names:?}");

    let edges: Vec<&str> = r.edge_types.iter().map(|t| t.name.as_str()).collect();
    assert!(edges.contains(&"Calls"), "{edges:?}");
    assert!(edges.contains(&"Contains"), "{edges:?}");

    for style in [Render::Ansi, Render::Markdown] {
        let out = render_graph_schema(&r, style);
        assert!(out.contains("Function"), "{out}");
    }
}

#[test]
fn an_empty_graphs_schema_is_empty_rather_than_a_default_list() {
    // Reporting the vocabulary a graph *could* hold, on a graph that holds
    // nothing, is how a filter comes back with a confident zero.
    let empty = GraphData {
        nodes: Vec::new(),
        edges: Vec::new(),
        stats: None,
        resolution: None,
    };
    let r = graph_schema(&empty, Path::new("/g.json"));
    assert!(r.node_types.is_empty());
    assert!(r.edge_types.is_empty());
}
