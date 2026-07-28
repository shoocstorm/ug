//! BM25 keyword weighting, through the real store.
//!
//! `storage::sparse_stats` and `storage::text` unit-test the maths. What they
//! cannot cover is the plumbing, which is where this feature can break
//! silently: statistics are computed at ingest, persisted to a sidecar, read
//! back when the store is opened, and consulted again at query time. Any link
//! in that chain failing degrades keyword ranking back to raw term frequency
//! — with no error, and with search still returning plausible results.
//!
//! Runs without an embedding server: rows go in through `Db` with hand-built
//! dense vectors, exactly as in `storage_test.rs`.

use tempfile::TempDir;
use ultragraph::storage::db::{Db, NodeRow};
use ultragraph::storage::embed::DEFAULT_EMBEDDING_DIM;
use ultragraph::storage::sparse_stats::SparseStats;
use ultragraph::storage::store::KnowledgeStore;
use ultragraph::storage::text::{
    build_node_sparse_vector, build_sparse_keyword_vector, build_sparse_query_vector,
    MAX_SPARSE_DIMS,
};
use ultragraph::types::{GraphData, GraphNode, GraphNodeType};

fn unit_vector(seed: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DEFAULT_EMBEDDING_DIM];
    v[seed % DEFAULT_EMBEDDING_DIM] = 1.0;
    v
}

fn row(id: &str, node_text: &str, seed: usize) -> NodeRow {
    NodeRow {
        id: id.to_string(),
        name: id.to_string(),
        node_type: "Function".to_string(),
        description: String::new(),
        file: format!("src/{id}.ts"),
        start_line: 1,
        end_line: 9,
        last_update_at: 0,
        node_text: node_text.to_string(),
        vector: unit_vector(seed),
        code: String::new(),
        file_hash: String::new(),
        facts: Default::default(),
    }
}

fn node(id: &str, docstring: &str) -> GraphNode {
    GraphNode {
        id: id.to_string(),
        name: id.to_string(),
        node_type: GraphNodeType::Function,
        file: Some(format!("src/{id}.ts")),
        start_line: Some(1),
        end_line: Some(9),
        metrics: None,
        signature: None,
        docstring: Some(docstring.to_string()),
        imports: Vec::new(),
        exports: Vec::new(),
        extends: Vec::new(),
        implements: Vec::new(),
        calls: Vec::new(),
        folder: None,
        ..Default::default()
    }
}

/// The dimension a single-word query maps to.
fn dim_of(term: &str) -> u32 {
    build_sparse_keyword_vector(term)
        .first()
        .expect("term tokenizes")
        .0
}

fn weight_of(v: &[(u32, f32)], term: &str) -> Option<f32> {
    let d = dim_of(term);
    v.iter().find(|(x, _)| *x == d).map(|(_, w)| *w)
}

// ---- the plumbing chain --------------------------------------------------

#[tokio::test]
async fn ingest_writes_stats_that_a_reopened_store_picks_up() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let graph = GraphData {
        nodes: vec![
            node("alpha", "the parser reads the file"),
            node("beta", "the writer flushes the buffer"),
            node("gamma", "the quaternion rotates the mesh"),
        ],
        edges: vec![],
        stats: None,
    };
    let texts: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| n.docstring.clone().unwrap())
        .collect();

    {
        let db = Db::open(path).await.unwrap();
        assert!(
            db.sparse_stats().is_none(),
            "a fresh store has no statistics yet"
        );

        let stats = ultragraph::storage::refresh_sparse_stats(
            &[&db as &dyn KnowledgeStore],
            &texts,
            &Default::default(),
            &graph,
        );
        assert_eq!(stats.total_docs, 3);
        assert!(
            db.sparse_stats().is_some(),
            "refresh must install them on the store it was given"
        );
    }

    // The sidecar is the durable half: a new process opens the store and must
    // find the statistics without re-ingesting.
    assert!(
        SparseStats::path_in(tmp.path()).exists(),
        "sidecar written next to the store"
    );
    let reopened = Db::open(path).await.unwrap();
    let loaded = reopened.sparse_stats().expect("statistics reload on open");
    assert_eq!(loaded.total_docs, 3);
    assert_eq!(
        loaded.doc_freq(dim_of("the")),
        3,
        "'the' is in all three documents"
    );
    assert_eq!(
        loaded.doc_freq(dim_of("quaternion")),
        1,
        "a term in one document reads back as rare"
    );
}

#[tokio::test]
async fn stats_are_computed_over_the_whole_graph_not_the_changed_subset() {
    // Document frequency is a corpus-wide quantity. Deriving it from an
    // incremental delta would make every run disagree with the last, so
    // `refresh_sparse_stats` takes the full graph even when only a few rows
    // are being written.
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let graph = GraphData {
        nodes: (0..10)
            .map(|i| node(&format!("n{i}"), "shared term plus unique{i}"))
            .collect(),
        edges: vec![],
        stats: None,
    };
    let texts: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| n.docstring.clone().unwrap())
        .collect();

    let stats = ultragraph::storage::refresh_sparse_stats(
        &[&db as &dyn KnowledgeStore],
        &texts,
        &Default::default(),
        &graph,
    );
    assert_eq!(stats.total_docs, 10, "every node counted, not just new ones");
    assert_eq!(stats.doc_freq(dim_of("shared")), 10);
}

// ---- weighting behaviour -------------------------------------------------

#[test]
fn a_rare_query_term_outweighs_a_common_one() {
    let corpus = [
        "the parser reads the file",
        "the writer flushes the buffer",
        "the server accepts the request",
        "the quaternion rotates the mesh",
    ];
    let dims: Vec<Vec<u32>> = corpus
        .iter()
        .map(|d| {
            build_sparse_keyword_vector(d)
                .into_iter()
                .map(|(d, _)| d)
                .collect()
        })
        .collect();
    let stats = SparseStats::from_documents(dims.iter().map(|d| d.as_slice()));

    let weighted = build_sparse_query_vector("the quaternion", Some(&stats));
    let common = weight_of(&weighted, "the").expect("common term present");
    let rare = weight_of(&weighted, "quaternion").expect("rare term present");
    assert!(
        rare > common * 2.0,
        "IDF must separate them: rare={rare} common={common}"
    );

    // Every weight stays positive — OverGraph rejects negative weights on the
    // query side as well as the stored side, so a term appearing in every
    // document must still score above zero rather than cancel out.
    assert!(
        weighted.iter().all(|&(_, w)| w > 0.0),
        "no non-positive weights: {weighted:?}"
    );
}

#[test]
fn without_stats_every_query_term_weighs_the_same() {
    // The documented fallback for a store ingested before the sidecar
    // existed: previous behaviour, not an error.
    let flat = build_sparse_query_vector("the quaternion", None);
    assert_eq!(
        weight_of(&flat, "the"),
        weight_of(&flat, "quaternion"),
        "unweighted fallback treats every term alike"
    );
    assert!(!flat.is_empty());

    // An empty corpus is treated as "no information", not as a corpus where
    // everything is maximally rare.
    let empty = SparseStats::default();
    assert_eq!(
        build_sparse_query_vector("the quaternion", Some(&empty)),
        flat
    );
}

#[test]
fn document_weights_are_independent_of_the_corpus() {
    // This is what `b = 0` buys: the stored vector is a function of the node
    // alone, so adding documents can never invalidate vectors already
    // written. If this ever fails, incremental re-ingest is broken.
    let small = SparseStats::from_documents([[1u32, 2].as_slice()]);
    let large = SparseStats::from_documents([
        [1u32, 2].as_slice(),
        [1, 3].as_slice(),
        [1, 4].as_slice(),
        [1, 5].as_slice(),
    ]);

    let text = "parses the configuration file";
    let a = build_node_sparse_vector(text, "", Some(&small));
    let b = build_node_sparse_vector(text, "", Some(&large));
    let c = build_node_sparse_vector(text, "", None);
    assert_eq!(a, b, "corpus size must not move a stored weight");
    assert_eq!(a, c, "nor must the absence of statistics");
}

#[test]
fn the_dimension_cap_keeps_rare_terms_over_common_ones() {
    // The cap used to keep the heaviest raw frequencies — i.e. the most
    // repeated common words, exactly backwards.
    let mut text = String::new();
    for i in 0..MAX_SPARSE_DIMS + 200 {
        text.push_str(&format!("ident{i} "));
    }
    for _ in 0..50 {
        text.push_str("the ");
    }

    let corpus: Vec<String> = (0..20).map(|i| format!("the thing number {i}")).collect();
    let dims: Vec<Vec<u32>> = corpus
        .iter()
        .map(|d| {
            build_sparse_keyword_vector(d)
                .into_iter()
                .map(|(d, _)| d)
                .collect()
        })
        .collect();
    let stats = SparseStats::from_documents(dims.iter().map(|d| d.as_slice()));

    let ranked = build_node_sparse_vector(&text, "", Some(&stats));
    let unranked = build_node_sparse_vector(&text, "", None);

    assert_eq!(ranked.len(), MAX_SPARSE_DIMS, "cap still enforced");
    assert!(
        weight_of(&unranked, "the").is_some(),
        "raw frequency keeps the most repeated word"
    );
    assert!(
        weight_of(&ranked, "the").is_none(),
        "IDF-ranked truncation drops the term carrying no information"
    );
    assert!(
        ranked.windows(2).all(|w| w[0].0 < w[1].0),
        "OverGraph requires ascending dimension ids"
    );
}

// ---- ranking through the store ------------------------------------------

/// The dot product OverGraph accumulates: `Σ query_weight × stored_weight`
/// over the dimensions the two vectors share. Mirrors
/// `sparse_postings::accumulate_sparse_posting_scores` so a scoring claim can
/// be made exactly, without depending on how ranks happen to fuse.
fn sparse_score(query: &[(u32, f32)], doc: &[(u32, f32)]) -> f32 {
    query
        .iter()
        .filter_map(|&(qd, qw)| doc.iter().find(|(dd, _)| *dd == qd).map(|(_, dw)| qw * dw))
        .sum()
}

#[test]
fn idf_widens_the_gap_between_a_rare_match_and_a_common_one() {
    // The property IDF actually buys, stated as a ratio so it holds
    // regardless of the absolute weights.
    //
    // `padded` matches only the query's ubiquitous term, five times over.
    // `precise` matches its rare term once. Both score above zero either way
    // — saturation already caps the value of repetition, so the unweighted
    // vector is not wildly wrong — but only IDF knows that "the" carries
    // almost no information in this corpus, and it is that knowledge which
    // separates the two documents decisively.
    let padded = "the the the the the config";
    let precise = "the quaternion config";

    // A corpus where "the" is everywhere and "quaternion" is not. The point
    // needs more than the two candidates: document frequency is only
    // informative relative to a corpus.
    let mut corpus: Vec<String> = (0..9).map(|i| format!("the thing number {i}")).collect();
    corpus.push(padded.to_string());
    corpus.push(precise.to_string());
    let dims: Vec<Vec<u32>> = corpus
        .iter()
        .map(|d| build_sparse_keyword_vector(d).into_iter().map(|(d, _)| d).collect())
        .collect();
    let stats = SparseStats::from_documents(dims.iter().map(|d| d.as_slice()));
    assert_eq!(stats.doc_freq(dim_of("the")), 11, "'the' is in every document");
    assert_eq!(stats.doc_freq(dim_of("quaternion")), 1);

    let ratio = |q: &[(u32, f32)], p: &[(u32, f32)], r: &[(u32, f32)]| {
        sparse_score(q, r) / sparse_score(q, p)
    };

    let flat_q = build_sparse_query_vector("the quaternion", None);
    let flat = ratio(
        &flat_q,
        &build_node_sparse_vector(padded, "", None),
        &build_node_sparse_vector(precise, "", None),
    );

    let bm_q = build_sparse_query_vector("the quaternion", Some(&stats));
    let bm = ratio(
        &bm_q,
        &build_node_sparse_vector(padded, "", Some(&stats)),
        &build_node_sparse_vector(precise, "", Some(&stats)),
    );

    assert!(flat > 1.0, "the rare match already edges ahead: {flat}");
    assert!(
        bm > flat * 5.0,
        "IDF must make the separation decisive, not marginal: flat={flat} bm25={bm}"
    );
}

#[test]
fn repetition_still_counts_but_saturates() {
    // The document-side half, which applies with or without statistics.
    // Repeating a term keeps raising its weight, but with sharply
    // diminishing returns — so a document cannot win on keyword-stuffing
    // alone even before IDF is involved.
    let q = build_sparse_query_vector("config", None);
    let score = |text: &str| sparse_score(&q, &build_node_sparse_vector(text, "", None));

    let once = score("config");
    let twice = score("config config");
    let fifty = score(&"config ".repeat(50));

    assert!(twice > once, "more occurrences still score higher");
    assert!(fifty > twice);
    assert!(
        fifty < once * 3.0,
        "fifty occurrences must not be worth fifty: once={once} fifty={fifty}"
    );
}

#[tokio::test]
async fn hybrid_search_returns_the_row_carrying_the_query_terms() {
    // Store-level plumbing: statistics installed, sparse vectors written at
    // upsert, keyword leg wired into `hybrid_search`. Deliberately does not
    // assert an exact ordering — with three rows equidistant on the dense
    // leg, rank fusion breaks ties arbitrarily, and a test that depended on
    // that would be pinning noise rather than behaviour.
    let tmp = TempDir::new().unwrap();
    let db = Db::open(tmp.path().to_str().unwrap()).await.unwrap();

    let rows = vec![
        row("common_a", "the parser reads the file", 1),
        row("common_b", "the writer flushes the buffer", 2),
        row("rare", "the quaternion rotates the mesh", 3),
    ];
    let texts: Vec<String> = rows.iter().map(|r| r.node_text.clone()).collect();
    let graph = GraphData {
        nodes: rows.iter().map(|r| node(&r.id, &r.node_text)).collect(),
        edges: vec![],
        stats: None,
    };
    ultragraph::storage::refresh_sparse_stats(
        &[&db as &dyn KnowledgeStore],
        &texts,
        &Default::default(),
        &graph,
    );
    db.upsert_nodes(&rows).await.unwrap();

    let stats = db.sparse_stats().expect("stats installed");
    let hits = KnowledgeStore::hybrid_search(
        &db,
        unit_vector(1),
        build_sparse_query_vector("quaternion", Some(&stats)),
        "quaternion",
        3,
        None,
    )
    .await
    .unwrap();

    let ids: Vec<&str> = hits.iter().map(|(r, _)| r.id.as_str()).collect();
    assert!(ids.contains(&"rare"), "keyword leg found nothing: {ids:?}");

    // A term no document contains must not invent a keyword match: the only
    // signal left is the dense leg.
    let none = KnowledgeStore::hybrid_search(
        &db,
        unit_vector(1),
        build_sparse_query_vector("zzzznonexistent", Some(&stats)),
        "zzzznonexistent",
        3,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        none[0].0.id, "common_a",
        "with no keyword match, dense similarity decides"
    );
}

#[tokio::test]
async fn stored_vectors_survive_a_reopen_and_still_match() {
    // Sparse vectors are built at upsert from the row plus the statistics in
    // hand. They must persist — a reopened store that had to rebuild them
    // would be a silent performance and correctness trap.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap();
    let rows = vec![row("only", "the quaternion rotates the mesh", 1)];

    {
        let db = Db::open(path).await.unwrap();
        db.upsert_nodes(&rows).await.unwrap();
    }

    let db = Db::open(path).await.unwrap();
    let hits = KnowledgeStore::hybrid_search(
        &db,
        unit_vector(300),
        build_sparse_keyword_vector("quaternion"),
        "quaternion",
        3,
        None,
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 1, "the stored sparse vector is still searchable");
    assert_eq!(hits[0].0.id, "only");
}
