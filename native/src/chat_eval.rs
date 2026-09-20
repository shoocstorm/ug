//! What the Answer tab actually retrieves, measured.
//!
//! Stage 0 made the turn's evidence list honest — `ChatRagOutcome::citations`
//! is now everything the answer was allowed to cite, seed pack plus whatever
//! the model's own searches added (see [`crate::chat::CitationLedger`]). That
//! turned the ledger into an instrument: for a question whose answer node is
//! known, "did retrieval reach it" is a fact rather than an impression.
//!
//! Two measurements, because they answer different questions:
//!
//! * **`rag_eval_seed_recall`** — the first hybrid+PPR pass alone, no model.
//!   This is the number the pipeline diagram describes, and the ceiling on
//!   any answer that never re-searches. Fast, deterministic, no endpoint.
//! * **`rag_eval_agentic_recall`** — a whole turn with the toolbox. The
//!   difference between the two *is* how much the agentic loop is worth, and
//!   it is the only honest way to argue about changing retrieval: a change
//!   that lifts seed recall but is invisible end-to-end bought nothing.
//!
//! Both are `#[ignore]`d. They read this machine's own `~/.ug/<project>`
//! index, and the second needs a chat endpoint that answers:
//!
//! ```text
//! cargo nextest run --run-ignored all -E 'test(rag_eval_seed)' --no-capture
//! UG_EVAL_LLM=1 cargo nextest run --run-ignored all -E 'test(rag_eval)' --no-capture
//! ```
//!
//! Both run past nextest's default "something is stuck" ceiling, so
//! `.config/nextest.toml` carries an override scoped to `test(rag_eval)`.
//! Without it the run is terminated at 600 s and the measurement is lost.
//!
//! **Re-index before measuring.** `ug update <changed files>` then `ug ingest
//! -n ug`; grading retrieval against an index that predates the code being
//! asked about measures the staleness, not the retrieval (Agents.md §10).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::chat::{self, CitationLedger, ChatRagOptions};
use ultragraph::storage::{open_store, Embedder, KnowledgeStore, StoreSpec};

#[derive(Deserialize)]
struct Fixture {
    project: String,
    questions: Vec<Question>,
}

#[derive(Deserialize)]
struct Question {
    q: String,
    expect_any: Vec<String>,
}

fn fixture() -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rag_eval.json");
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&body).expect("rag_eval.json is valid JSON")
}

/// The indexed project this fixture grades against, or `None` when this
/// checkout has never indexed it — which is a reason to say so and stop, not
/// to fail someone's test run.
fn project_store_path(project: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let dir = home.join(".ug").join(project);
    // The store is the `ugdb` inside the project directory, not the
    // directory: `open_store` on the parent opens an empty one, which then
    // answers every query with nothing and grades as a total retrieval
    // collapse rather than as the misconfiguration it is.
    dir.join("ugdb").is_dir().then_some(dir)
}

/// Where the questions' answers live, for snippet hydration.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native/ has a parent")
        .to_path_buf()
}

struct Graded {
    question: String,
    /// 1-based rank of the first expected node in the citation list, or
    /// `None` for a miss. This is what the reader can click.
    rank: Option<usize>,
    /// Whether the answer text names an expected symbol at all.
    ///
    /// A separate question from `rank`, and the honest end-to-end one: only
    /// `search` registers into the ledger, so a turn that reached the answer
    /// through `get_code` or `find_usages` cites nothing for it and scores a
    /// miss above while having actually answered. Until every node-returning
    /// tool registers (Stage 2), the gap between these two columns *is* the
    /// provenance the answer is missing.
    named: bool,
    retrieved: usize,
    tool_calls: usize,
    /// The cap stopped it rather than it finishing — a miss under protest is
    /// a different problem from a miss it was content with.
    capped: bool,
    ms: u128,
}

/// The bare symbol name from a node id: `function:src/a.rs:Foo::bar` → `bar`.
fn leaf_name(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

fn grade(
    q: &Question,
    ids: &[String],
    answer: &str,
    tool_calls: usize,
    capped: bool,
    ms: u128,
) -> Graded {
    let rank = ids
        .iter()
        .position(|id| q.expect_any.iter().any(|want| want == id))
        .map(|i| i + 1);
    let named = q
        .expect_any
        .iter()
        .any(|want| answer.contains(leaf_name(want)));
    Graded {
        question: q.q.clone(),
        rank,
        named,
        retrieved: ids.len(),
        tool_calls,
        capped,
        ms,
    }
}

/// Hit rate and mean reciprocal rank, printed as the table this exists to
/// produce. Returns the hit rate so a caller can hold a floor against it.
fn report(label: &str, rows: &[Graded]) -> f32 {
    let hits = rows.iter().filter(|r| r.rank.is_some()).count();
    let named = rows.iter().filter(|r| r.named).count();
    let mrr: f32 = rows
        .iter()
        .filter_map(|r| r.rank)
        .map(|k| 1.0 / k as f32)
        .sum::<f32>()
        / rows.len() as f32;
    let total_ms: u128 = rows.iter().map(|r| r.ms).sum();
    let calls: usize = rows.iter().map(|r| r.tool_calls).sum();
    let capped = rows.iter().filter(|r| r.capped).count();

    println!("\n── {label} ───────────────────────────────────────────");
    for r in rows {
        let mark = match r.rank {
            Some(k) => format!("hit @{k}"),
            None => "MISS  ".to_string(),
        };
        let q: String = r.question.chars().take(52).collect();
        let says = if r.named { "names it" } else { "  —     " };
        let cap = if r.capped { "CAPPED" } else { "      " };
        println!(
            "  {mark:<8} {says} {cap} {:>3} cited {:>2} tools {:>6} ms  {q}",
            r.retrieved, r.tool_calls, r.ms
        );
    }
    let rate = hits as f32 / rows.len() as f32;
    println!(
        "  ── cited {hits}/{n} ({:.0}%) · named {named}/{n} ({:.0}%) · MRR {:.3} \
         · {calls} tool calls ({capped} capped) · {} ms total",
        rate * 100.0,
        named as f32 / rows.len() as f32 * 100.0,
        mrr,
        total_ms,
        n = rows.len(),
    );
    rate
}

async fn open_eval_store(dir: &PathBuf, dim: u32) -> Arc<dyn KnowledgeStore> {
    let spec = StoreSpec::Overgraph {
        path: dir.join("ugdb"),
        embedding_dim: dim,
    };
    Arc::from(
        open_store(&spec)
            .await
            .unwrap_or_else(|e| panic!("open {}: {e}", dir.display())),
    )
}

/// The embedder `ug chat` would build from this machine's config.
///
/// Not `EmbedderConfig::default()`: that skips the dim auto-probe, and a
/// store opened at the wrong dimension answers every query with nothing —
/// which grades as 0% recall and looks exactly like a retrieval collapse.
fn eval_embedder() -> Embedder {
    crate::cli::embed::embedder_from_chat_args(&[])
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// The first pass alone: what the pipeline retrieves from the user's own
/// wording, before the model says anything.
#[test]
#[ignore = "reads this machine's ~/.ug index; run deliberately"]
fn rag_eval_seed_recall() {
    let fx = fixture();
    let Some(dir) = project_store_path(&fx.project) else {
        eprintln!("note: ~/.ug/{} is not indexed — nothing to grade.", fx.project);
        return;
    };
    let embedder = eval_embedder();
    let dim = embedder.config().dim as u32;
    let root = repo_root();

    let rows = runtime().block_on(async {
        let store = open_eval_store(&dir, dim).await;
        let mut rows = Vec::new();
        for q in &fx.questions {
            let opts = ChatRagOptions::new();
            let t = std::time::Instant::now();
            let ctx = chat::retrieve_context(&*store, &embedder, &root, &q.q, &opts)
                .await
                .expect("retrieval");
            let ids: Vec<String> = ctx.items.iter().map(|i| i.id.clone()).collect();
            // No model in this half, so there is no answer text to check.
            rows.push(grade(q, &ids, "", 0, false, t.elapsed().as_millis()));
        }
        rows
    });

    let rate = report("seed retrieval only (no model)", &rows);
    // A floor, not a target. Measured 2026-09-20 against this repo's own
    // index; it exists so a retrieval change that quietly loses ground fails
    // here instead of being argued about.
    assert!(
        rate >= SEED_RECALL_FLOOR,
        "seed recall {rate:.2} fell below the recorded floor {SEED_RECALL_FLOOR:.2}"
    );
}

/// The whole turn: seed pack, then whatever the model went and fetched.
#[test]
#[ignore = "needs a live chat endpoint; set UG_EVAL_LLM=1"]
fn rag_eval_agentic_recall() {
    if std::env::var("UG_EVAL_LLM").is_err() {
        eprintln!("note: set UG_EVAL_LLM=1 to grade whole turns against the configured model.");
        return;
    }
    let fx = fixture();
    let Some(dir) = project_store_path(&fx.project) else {
        eprintln!("note: ~/.ug/{} is not indexed — nothing to grade.", fx.project);
        return;
    };
    let embedder = eval_embedder();
    let dim = embedder.config().dim as u32;
    let root = repo_root();
    // Same resolution `ug chat` uses, so what is graded here is what the
    // configured endpoint actually does.
    // A deliberating turn with four tool rounds runs well past the 180 s
    // default, and a timeout mid-run loses the whole measurement.
    let mut cfg = crate::cli::chat::chat_config_from_args(&[]);
    cfg.timeout_secs = 900;
    // The control for the deliberation change. `fast_client` declines to act
    // when a caller has set `extra_body` themselves, so putting the no-think
    // body here reproduces the old shipped behaviour exactly — tools attached,
    // deliberation off — without reverting the default to measure against it.
    if std::env::var("UG_EVAL_NOTHINK").is_ok() {
        println!("(control: deliberation off, as shipped before 2026-09-20)");
        cfg.extra_body = Some(chat::no_think_body());
    }
    let client = chat::ChatClient::new(cfg).expect("chat client");
    let graph_path = dir.join("graph.json");
    let raw = std::fs::read_to_string(&graph_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", graph_path.display()));
    let graph: ultragraph::types::GraphData =
        serde_json::from_str(&raw).expect("graph.json parses");
    let graph = Arc::new(graph);

    // Repeats, because one pass cannot see a one-question effect.
    //
    // Measured spread at n=12 on an unchanged build: cited 7/12 then 9/12,
    // 234 s then 298 s. That is ±2 questions and ±30 % wall clock from
    // sampling alone, so a single run distinguishes nothing smaller than the
    // deliberation fix itself. Averaging R passes shrinks it by √R; widening
    // the fixture shrinks it faster, and is the better fix when someone has
    // the ground truth to spare.
    let repeats: usize = std::env::var("UG_EVAL_REPEATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);

    let rows = runtime().block_on(async {
        let store = open_eval_store(&dir, dim).await;
        let embedder = Arc::new(eval_embedder());
        let mut rows = Vec::new();
        for (q, pass) in fx
            .questions
            .iter()
            .flat_map(|q| (1..=repeats).map(move |p| (q, p)))
        {
            let _ = pass;
            let ledger = Arc::new(Mutex::new(CitationLedger::new()));

            // The toolbox the UI gives it, wired to the same handles.
            let (tool_graph, tool_store, tool_embedder, tool_ledger) = (
                graph.clone(),
                store.clone(),
                embedder.clone(),
                ledger.clone(),
            );
            let (tool_gp, tool_root) = (graph_path.clone(), root.clone());
            let runner = move |name: &str, args: serde_json::Value| {
                let (graph, store, embedder, ledger) = (
                    tool_graph.clone(),
                    tool_store.clone(),
                    tool_embedder.clone(),
                    tool_ledger.clone(),
                );
                let (gp, root) = (tool_gp.clone(), tool_root.clone());
                let name = name.to_string();
                Box::pin(async move {
                    chat::run_chat_tool(
                        &name,
                        args,
                        &graph,
                        gp.as_path(),
                        root.as_path(),
                        &*store,
                        Some(&embedder),
                        &ledger,
                    )
                    .await
                }) as futures::future::BoxFuture<'static, Result<String, String>>
            };
            let toolbox = chat::ToolBox {
                schemas: crate::mcp::tools::openai_tool_schemas(),
                run: &runner,
                max_rounds: chat::DEFAULT_TOOL_ROUNDS,
                max_result_chars: 6_000,
            };

            let t = std::time::Instant::now();
            let outcome = chat::run_chat_rag(chat::ChatRagRequest {
                store: &*store,
                embedder: &embedder,
                chat: &client,
                repo_root: &root,
                query: &q.q,
                history: &[],
                opts: ChatRagOptions::new(),
                toolbox: Some(&toolbox),
                ledger: &ledger,
            })
            .await
            .expect("chat turn");
            let ids: Vec<String> = outcome.citations.iter().map(|i| i.id.clone()).collect();
            rows.push(grade(
                q,
                &ids,
                &outcome.answer,
                outcome.tool_calls,
                outcome.hit_round_cap,
                t.elapsed().as_millis(),
            ));
        }
        rows
    });

    let label = if repeats > 1 {
        format!("whole turn, {repeats} passes per question")
    } else {
        "whole turn (seed + what the model fetched)".to_string()
    };
    report(&label, &rows);
}

/// Measured 2026-09-20 on this repo's own index: 3/12 hit (25%), MRR 0.146.
///
/// The floor sits below the measurement on purpose — it is there to catch a
/// collapse, not to freeze a number that legitimately moves when the fixture
/// or the index does. Raise it when a change earns it, and say what the new
/// measurement was.
const SEED_RECALL_FLOOR: f32 = 0.20;

#[test]
#[ignore = "diagnostic: prints what the toolbox actually sends"]
fn rag_eval_toolbox_is_wired() {
    let schemas = crate::mcp::tools::openai_tool_schemas();
    println!("\nschemas sent: {}", schemas.len());
    for s in &schemas {
        println!("  {}", s["function"]["name"].as_str().unwrap_or("?"));
    }
    println!("\nSYSTEM_CORE in use for a tool turn:\n{}\n", chat::SYSTEM_CORE);
    assert!(!schemas.is_empty(), "no tools would be sent at all");
}
