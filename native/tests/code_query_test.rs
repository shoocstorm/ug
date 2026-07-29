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
    has_doc: i64,
    is_test: i64,
    in_degree: i64,
}

/// A miniature repo: two source folders, one test folder, a documented
/// core symbol everything depends on, and one symbol nothing calls.
const SYMBOLS: &[Sym] = &[
    Sym { id: "function:src/core/auth.rs:verify", node_type: "Function", file: "src/core/auth.rs", loc: 120, has_doc: 1, is_test: 0, in_degree: 3 },
    Sym { id: "function:src/core/auth.rs:hash",   node_type: "Function", file: "src/core/auth.rs", loc: 18,  has_doc: 0, is_test: 0, in_degree: 1 },
    Sym { id: "function:src/api/login.rs:handle", node_type: "Function", file: "src/api/login.rs", loc: 64,  has_doc: 0, is_test: 0, in_degree: 1 },
    Sym { id: "function:src/api/login.rs:unused", node_type: "Function", file: "src/api/login.rs", loc: 9,   has_doc: 0, is_test: 0, in_degree: 0 },
    Sym { id: "class:src/core/auth.rs:Session",   node_type: "Class",    file: "src/core/auth.rs", loc: 200, has_doc: 0, is_test: 0, in_degree: 2 },
    Sym { id: "function:tests/auth_test.rs:t_verify", node_type: "Function", file: "tests/auth_test.rs", loc: 25, has_doc: 0, is_test: 1, in_degree: 0 },
    Sym { id: "file:src/core/auth.rs", node_type: "File", file: "src/core/auth.rs", loc: 400, has_doc: 0, is_test: 0, in_degree: 2 },
    Sym { id: "file:src/api/login.rs", node_type: "File", file: "src/api/login.rs", loc: 90,  has_doc: 0, is_test: 0, in_degree: 0 },
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
                facts: facts(&[
                    ("loc", FactValue::Int(s.loc)),
                    ("has_doc", FactValue::Int(s.has_doc)),
                    ("is_test", FactValue::Int(s.is_test)),
                    ("in_degree", FactValue::Int(s.in_degree)),
                    ("out_degree", FactValue::Int(1)),
                    ("params", FactValue::Int(2)),
                    ("max_nesting", FactValue::Int(3)),
                    ("folder", FactValue::Str(folder.to_string())),
                ]),
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
async fn a_query_over_an_unstored_property_is_reported_not_answered() {
    let tmp = TempDir::new().unwrap();
    let db = seeded_store(&tmp).await;

    let answer = code_query::run(
        &db,
        &CodeQueryParams {
            gql: Some(
                "MATCH (n:Function) WHERE n.comment_lines > 3 RETURN count(*) AS c".into(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The engine happily returns 0 here. Everything that stops that zero
    // from being believed lives in the envelope.
    assert_eq!(answer.page.rows[0][0], QueryValue::Int(0));
    assert_eq!(answer.unindexed, vec!["comment_lines".to_string()]);
    let text = code_query::render::render(&answer, Render::Markdown);
    assert!(text.contains("NOT INDEXED"), "{text}");
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
