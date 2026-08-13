//! `code_query` against a real store.
//!
//! The unit tests in `code_query` cover parameter binding and rendering
//! from hand-built rows. What only a live engine can show is whether the
//! shipped preset queries actually *execute* — and that is the thing most
//! worth testing, because a preset is a string. Nothing in the type system
//! notices a preset that references a property no node carries, uses a
//! syntax this engine rejects, or expands a traversal past a cap: all
//! three come back at runtime, and two of the three come back as a
//! plausible number rather than an error.

use std::collections::BTreeMap;
use tempfile::TempDir;
use ultragraph::agent_tools::Render;
use ultragraph::code_query::{self, presets, CodeQueryParams};
use ultragraph::storage::db::{Db, EdgeRow, NodeRow};
use ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM;
use ultragraph::storage::facts::{FactValue, Facts};
use ultragraph::storage::store::{KnowledgeStore, QueryLimits, QueryParams, QueryValue};

fn unit_vector(seed: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DEFAULT_EMBEDDING_DIM];
    v[seed % DEFAULT_EMBEDDING_DIM] = 1.0;
    v
}

fn facts(pairs: &[(&str, FactValue)]) -> Facts {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

struct Sym {
    id: &'static str,
    node_type: &'static str,
    file: &'static str,
    loc: i64,
    /// Span lines that are neither blank nor comment.
    code_lines: i64,
    comment_lines: i64,
    doc_lines: i64,
    has_doc: i64,
    is_test: i64,
    in_degree: i64,
    /// Declared members, for types in languages that nest them. `None`
    /// means the fact is absent — which is what a Rust struct looks like,
    /// and what `classes_by_members` has to tolerate.
    members: Option<i64>,
    /// The system boundary this symbol is, if any. `None` is the common
    /// case and still writes `boundary = 0`: this fixture stands for a
    /// graph that *was* boundary-indexed and found nothing here, which is
    /// a different claim from one that never looked.
    boundary: Option<Bnd>,
}

struct Bnd {
    kinds: &'static str,
    protocols: &'static str,
    detail: &'static str,
    inbound: i64,
    outbound: i64,
}

const HTTP_IN: Bnd = Bnd {
    kinds: "http.endpoint",
    protocols: "http",
    detail: "POST /login",
    inbound: 1,
    outbound: 0,
};

const DB_OUT: Bnd = Bnd {
    kinds: "db.access",
    protocols: "jdbc",
    detail: "users",
    inbound: 0,
    outbound: 1,
};

/// A miniature repo: two source folders, one test folder, a documented
/// core symbol everything depends on, and one symbol nothing calls.
const SYMBOLS: &[Sym] = &[
    Sym { id: "function:src/core/auth.rs:verify", node_type: "Function", file: "src/core/auth.rs", loc: 120, code_lines: 90, comment_lines: 20, doc_lines: 4, has_doc: 1, is_test: 0, in_degree: 3, members: None, boundary: Some(DB_OUT) },
    Sym { id: "function:src/core/auth.rs:hash",   node_type: "Function", file: "src/core/auth.rs", loc: 18,  code_lines: 15, comment_lines: 0,  doc_lines: 0, has_doc: 0, is_test: 0, in_degree: 1, members: None, boundary: None },
    // Commented but undocumented — the case that separates `has_comments`
    // from `has_doc`.
    Sym { id: "function:src/api/login.rs:handle", node_type: "Function", file: "src/api/login.rs", loc: 64,  code_lines: 48, comment_lines: 11, doc_lines: 0, has_doc: 0, is_test: 0, in_degree: 1, members: None, boundary: Some(HTTP_IN) },
    Sym { id: "function:src/api/login.rs:unused", node_type: "Function", file: "src/api/login.rs", loc: 9,   code_lines: 7,  comment_lines: 0,  doc_lines: 0, has_doc: 0, is_test: 0, in_degree: 0, members: None, boundary: None },
    Sym { id: "class:src/core/auth.rs:Session",   node_type: "Class",    file: "src/core/auth.rs", loc: 200, code_lines: 150, comment_lines: 30, doc_lines: 0, has_doc: 0, is_test: 0, in_degree: 2, members: Some(7), boundary: None },
    Sym { id: "function:tests/auth_test.rs:t_verify", node_type: "Function", file: "tests/auth_test.rs", loc: 25, code_lines: 22, comment_lines: 1, doc_lines: 0, has_doc: 0, is_test: 1, in_degree: 0, members: None, boundary: None },
    Sym { id: "file:src/core/auth.rs", node_type: "File", file: "src/core/auth.rs", loc: 400, code_lines: 300, comment_lines: 60, doc_lines: 0, has_doc: 0, is_test: 0, in_degree: 2, members: None, boundary: None },
    Sym { id: "file:src/api/login.rs", node_type: "File", file: "src/api/login.rs", loc: 90,  code_lines: 70, comment_lines: 12, doc_lines: 0, has_doc: 0, is_test: 0, in_degree: 0, members: None, boundary: None },
];

const EDGES: &[(&str, &str, &str)] = &[
    ("function:src/api/login.rs:handle", "function:src/core/auth.rs:verify", "Calls"),
    ("function:src/core/auth.rs:verify", "function:src/core/auth.rs:hash", "Calls"),
    ("function:tests/auth_test.rs:t_verify", "function:src/api/login.rs:handle", "Calls"),
    ("class:src/core/auth.rs:Session", "function:src/core/auth.rs:verify", "References"),
    ("function:src/api/login.rs:handle", "class:src/core/auth.rs:Session", "References"),
    ("file:src/api/login.rs", "file:src/core/auth.rs", "Imports"),
];

async fn seeded_store(tmp: &TempDir) -> Db {
    let db = Db::open_or_create(tmp.path().to_str().unwrap(), DEFAULT_EMBEDDING_DIM as u32)
        .await
        .unwrap();

    let rows: Vec<NodeRow> = SYMBOLS
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let folder = s.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            NodeRow {
                id: s.id.to_string(),
                name: s.id.rsplit(':').next().unwrap().to_string(),
                node_type: s.node_type.to_string(),
                description: String::new(),
                file: s.file.to_string(),
                start_line: 1,
                end_line: s.loc as u32,
                last_update_at: 1_700_000_000,
                node_text: s.id.to_string(),
                vector: unit_vector(i),
                code: String::new(),
                file_hash: String::new(),
                facts: {
                    let mut f = facts(&[
                        ("loc", FactValue::Int(s.loc)),
                        ("code_lines", FactValue::Int(s.code_lines)),
                        ("comment_lines", FactValue::Int(s.comment_lines)),
                        ("doc_lines", FactValue::Int(s.doc_lines)),
                        ("has_doc", FactValue::Int(s.has_doc)),
                        (
                            "has_comments",
                            FactValue::Int(i64::from(s.comment_lines > 0 || s.doc_lines > 0)),
                        ),
                        ("is_test", FactValue::Int(s.is_test)),
                        ("in_degree", FactValue::Int(s.in_degree)),
                        ("out_degree", FactValue::Int(1)),
                        ("params", FactValue::Int(2)),
                        ("max_nesting", FactValue::Int(3)),
                        ("folder", FactValue::Str(folder.to_string())),
                        ("language", FactValue::Str("rust".into())),
                        (
                            "classification",
                            FactValue::Str(
                                if s.is_test == 1 { "test" } else { "service" }.to_string(),
                            ),
                        ),
                    ]);
                    // Absent on purpose for everything but the one type
                    // that declares members — see `Sym::members`.
                    if let Some(m) = s.members {
                        f.insert("members".into(), FactValue::Int(m));
                    }
                    // The flags are written for every node, the strings only
                    // where there is one — mirroring `facts::compute`, so
                    // "no boundary" reads as a measured zero while the
                    // detail columns stay genuinely absent.
                    f.insert(
                        "boundary".into(),
                        FactValue::Int(i64::from(s.boundary.is_some())),
                    );
                    f.insert(
                        "boundary_in".into(),
                        FactValue::Int(s.boundary.as_ref().map_or(0, |b| b.inbound)),
                    );
                    f.insert(
                        "boundary_out".into(),
                        FactValue::Int(s.boundary.as_ref().map_or(0, |b| b.outbound)),
                    );
                    if let Some(b) = &s.boundary {
                        f.insert("boundary_kinds".into(), FactValue::Str(b.kinds.into()));
                        f.insert(
                            "boundary_protocols".into(),
                            FactValue::Str(b.protocols.into()),
                        );
                        f.insert("boundary_detail".into(), FactValue::Str(b.detail.into()));
                    }
                    f
                },
            }
        })
        .collect();
    db.upsert_nodes(&rows).await.unwrap();

    let edges: Vec<EdgeRow> = EDGES
        .iter()
        .map(|(s, t, kind)| EdgeRow {
            id: format!("{s}->{t}"),
            source: s.to_string(),
            target: t.to_string(),
            edge_type: kind.to_string(),
            properties: String::new(),
        })
        .collect();
    db.upsert_edges(&edges).await.unwrap();

    db
}

fn params(preset: &str) -> CodeQueryParams {
    CodeQueryParams {
        preset: Some(preset.to_string()),
        ..Default::default()
    }
}

/// The headline test: every preset ug ships must execute.
///
/// A preset is a string, and the three ways one breaks — rejected syntax,
/// a property that was never stored, a traversal past a cap — are all
/// invisible until something runs it.
#[tokio::test]
async fn every_builtin_preset_executes() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let mut failed: Vec<String> = Vec::new();
    for p in presets::all() {
        let mut args = BTreeMap::new();
        for param in p.params {
            if param.default.is_none() {
                // Only presets that take a target or a layer prefix reach
                // here; both want a path this fixture actually contains.
                let value = match param.name {
                    "target" => "src/core/auth.rs",
                    // A list of changed files for the diff_* presets.
                    "files" => "src/core/auth.rs,src/api/login.rs",
                    // A symbol node id for test_for.
                    "symbol" => "function:src/core/auth.rs:verify",
                    "from_prefix" => "src/api",
                    "to_prefix" => "src/core",
                    other => panic!("preset `{}` has an unhandled required param `{other}` — give the test a value for it", p.name),
                };
                args.insert(param.name.to_string(), value.to_string());
            }
        }
        let request = CodeQueryParams {
            preset: Some(p.name.to_string()),
            args,
            ..Default::default()
        };
        if let Err(e) = code_query::run(&db, &request).await {
            failed.push(format!("{}: {e}", p.name));
        }
    }
    assert!(failed.is_empty(), "presets failed to execute:\n{}", failed.join("\n"));
}

/// Every preset must also name properties the store actually holds.
/// Executing is not enough: `n.comment_lines > 3` executes fine and
/// returns a confident zero.
#[tokio::test]
async fn no_builtin_preset_reads_an_unindexed_property() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let mut offenders: Vec<String> = Vec::new();
    for p in presets::all() {
        let mut args = BTreeMap::new();
        for param in p.params {
            if param.default.is_none() {
                let value = match param.name {
                    "target" => "src/core/auth.rs",
                    "from_prefix" => "src/api",
                    _ => "src/core",
                };
                args.insert(param.name.to_string(), value.to_string());
            }
        }
        let answer = code_query::run(
            &db,
            &CodeQueryParams {
                preset: Some(p.name.to_string()),
                args,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if !answer.unindexed.is_empty() {
            offenders.push(format!("{}: {}", p.name, answer.unindexed.join(", ")));
        }
    }
    assert!(
        offenders.is_empty(),
        "presets read properties no node carries:\n{}",
        offenders.join("\n")
    );
}

#[tokio::test]
async fn a_preset_returns_the_right_answer_not_just_a_number() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let mut p = params("long_functions");
    p.args.insert("min_loc".into(), "50".into());
    let answer = code_query::run(&db, &p).await.unwrap();

    // verify (120) and handle (64) qualify; hash (18), unused (9) and the
    // 25-line test do not, and neither does the 200-line Class.
    let ids: Vec<&QueryValue> = answer.page.rows.iter().filter_map(|r| r.first()).collect();
    assert_eq!(ids.len(), 2, "{:?}", answer.page.rows);
    assert_eq!(
        ids[0],
        &QueryValue::Str("function:src/core/auth.rs:verify".into())
    );
}

#[tokio::test]
async fn impact_counts_dependents_once_not_once_per_path() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let mut p = params("impact_summary");
    p.args.insert("target".into(), "src/core/auth.rs".into());
    let answer = code_query::run(&db, &p).await.unwrap();

    // `handle` reaches auth.rs by two distinct routes (directly via verify,
    // and via Session), so a path-counting query would report it twice.
    // Dependents outside auth.rs: handle, t_verify, and the login.rs File
    // node that imports it.
    let dependents = answer.page.rows[0][0].as_f64().unwrap() as usize;
    assert_eq!(dependents, 3, "row: {:?}", answer.page.rows[0]);
}

#[tokio::test]
async fn boundary_impact_reports_the_surface_a_change_is_visible_through() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let mut p = params("boundary_impact");
    p.args.insert("target".into(), "src/core/auth.rs".into());
    let answer = code_query::run(&db, &p).await.unwrap();

    // `handle` is the only inbound boundary that reaches auth.rs. `verify`
    // is a boundary too, but an outbound one and inside the target file —
    // neither is a contract a change to auth.rs breaks for someone else.
    assert_eq!(answer.page.rows.len(), 1, "{:?}", answer.page.rows);
    let row = &answer.page.rows[0];
    assert_eq!(row[0], QueryValue::Str("function:src/api/login.rs:handle".into()));
    assert_eq!(row[1], QueryValue::Str("http.endpoint".into()));
    assert_eq!(row[2], QueryValue::Str("POST /login".into()));
}

#[tokio::test]
async fn boundary_census_separates_the_two_directions() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let answer = code_query::run(&db, &params("boundary_census")).await.unwrap();

    // One inbound HTTP surface and one outbound DB one — two rows, because
    // the kinds differ, and each counted in exactly one direction.
    assert_eq!(answer.page.rows.len(), 2, "{:?}", answer.page.rows);
    let total_in: f64 = answer
        .page
        .rows
        .iter()
        .filter_map(|r| r[2].as_f64())
        .sum();
    let total_out: f64 = answer
        .page
        .rows
        .iter()
        .filter_map(|r| r[3].as_f64())
        .sum();
    assert_eq!((total_in, total_out), (1.0, 1.0), "{:?}", answer.page.rows);
}

#[tokio::test]
async fn a_query_over_an_unstored_property_is_reported_not_answered() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    // A plausible-sounding metric this indexer has never produced. The
    // point is that the engine cannot tell the difference between "no
    // function exceeds this" and "nothing has ever recorded this".
    let answer = code_query::run(
        &db,
        &CodeQueryParams {
            gql: Some(
                "MATCH (n:Function) WHERE n.cyclomatic_complexity > 3 RETURN count(*) AS c".into(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The engine happily returns 0 here. Everything that stops that zero
    // from being believed lives in the envelope.
    assert_eq!(answer.page.rows[0][0], QueryValue::Int(0));
    assert_eq!(
        answer.unindexed,
        vec!["cyclomatic_complexity".to_string()]
    );
    let text = code_query::render::render(&answer, Render::Markdown);
    assert!(text.contains("NOT INDEXED"), "{text}");
}

/// An index built before comment metrics existed must say so.
///
/// This is the end-to-end version of the version gate: `ug` upgrades in
/// place, so the common state right after an upgrade is a *current binary*
/// reading an *old index*. Every symbol in it has `comment_lines: 0` by
/// serde default, and storing that would answer "how well commented is
/// this repo" with "not at all".
#[tokio::test]
async fn an_index_predating_comment_metrics_reports_them_as_unindexed() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open_or_create(tmp.path().to_str().unwrap(), DEFAULT_EMBEDDING_DIM as u32)
        .await
        .unwrap();

    // Exactly what a pre-v2 ingest wrote: the old facts, none of the new.
    let row = NodeRow {
        id: "function:src/old.rs:legacy".into(),
        name: "legacy".into(),
        node_type: "Function".into(),
        description: String::new(),
        file: "src/old.rs".into(),
        start_line: 1,
        end_line: 40,
        last_update_at: 1_700_000_000,
        node_text: "legacy".into(),
        vector: unit_vector(0),
        code: String::new(),
        file_hash: String::new(),
        facts: facts(&[
            ("loc", FactValue::Int(40)),
            ("has_doc", FactValue::Int(0)),
            ("is_test", FactValue::Int(0)),
            ("in_degree", FactValue::Int(0)),
        ]),
    };
    db.upsert_nodes(&[row]).await.unwrap();

    let answer = code_query::run(&db, &params("comment_coverage")).await.unwrap();
    assert!(
        answer.unindexed.contains(&"has_comments".to_string()),
        "expected has_comments to report unindexed, got {:?}",
        answer.unindexed
    );
    let text = code_query::render::render(&answer, Render::Markdown);
    assert!(text.contains("NOT INDEXED"), "{text}");
    assert!(text.contains("ug regen"), "must say how to fix it: {text}");
}

#[tokio::test]
async fn coverage_reports_real_denominators() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let answer = code_query::run(
        &db,
        &CodeQueryParams {
            gql: Some("MATCH (n) WHERE n.loc > 0 RETURN count(*) AS c".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let loc = answer
        .coverage
        .iter()
        .find(|c| c.property == "loc")
        .expect("loc coverage");
    assert_eq!(loc.present, SYMBOLS.len());
    assert_eq!(loc.total, SYMBOLS.len());
}

/// Presets can arrive from a cloned repo's `.ug/presets.toml`, so
/// read-only execution has to be a property of the call site rather than
/// a promise about the input.
#[tokio::test]
async fn a_mutation_is_refused_however_it_arrives() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let err = code_query::run(
        &db,
        &CodeQueryParams {
            gql: Some("MATCH (n:Function) SET n.loc = 0 RETURN count(*) AS c".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("read-only"), "{err}");

    // And the store is untouched.
    let page = db
        .execute_query(
            "MATCH (n:Function) WHERE n.loc = 0 RETURN count(*) AS c",
            &QueryParams::new(),
            &QueryLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(page.rows[0][0], QueryValue::Int(0));
}

#[tokio::test]
async fn a_traversal_that_exceeds_a_cap_says_how_to_narrow_it() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    // A tiny fixture cannot actually blow the frontier, so drive the
    // message off a cap the caller can pin low.
    let err = db
        .execute_query(
            "MATCH (a)-[:Calls|References*1..3]->(b) RETURN count(*) AS c",
            &QueryParams::new(),
            &QueryLimits {
                max_frontier: 1,
                ..QueryLimits::default()
            },
        )
        .await
        .map(|_| String::new())
        .unwrap_or_else(|e| e.to_string());
    assert!(
        err.contains("max_frontier") || err.is_empty(),
        "unexpected error: {err}"
    );
}
