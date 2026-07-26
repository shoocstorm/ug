//! End-to-end check that a markdown document's prose reaches the text that
//! gets embedded.
//!
//! The regression this guards: markdown headings used to be indexed with no
//! docstring, so a section embedded as `"Concept: <heading>. . Related: …"`
//! — the heading was the only retrieval signal, and the paragraph that
//! actually explained the section reached the dense index nowhere. This
//! walks the real pipeline (index → build_graph → build_texts) rather than
//! unit-testing the extractor, because the gap was in the seam between
//! those stages, not in any one of them.

use std::fs;
use tempfile::TempDir;
use std::collections::HashMap;
use ultragraph::limits::{BudgetSource, EmbedBudget};
use ultragraph::storage::source::CapturedCode;
use ultragraph::storage::ingest::{build_texts, capture_for_graph};
use ultragraph::{build_graph, index, GraphData};

const DOC: &str = "\
# Embedding Backends

Two backends ship in the box.

## Why bother with a local backend?

The previous default was a hosted endpoint. That forced every contributor to
spin up an embedding server before any ingest could run, and to manage GPU
memory for that sidecar.

```bash
cargo run -- gen --base-url http://localhost:8000
```

Most users just want to index a repo and get a knowledge graph.

## Hosted endpoints

See the [ingest guide](./ingest.md) for the flags.
";

/// Index `DOC` into a graph and return it alongside the per-node embedding
/// texts, in `graph.nodes` order.
fn texts_for_doc() -> (TempDir, GraphData, Vec<String>) {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("EMBEDDING-BACKENDS.md"), DOC).expect("write doc");

    let index_json = index(dir.path().to_string_lossy().to_string());
    let graph_json = build_graph(index_json);
    let graph: GraphData = serde_json::from_str(&graph_json).expect("graph parses");

    let captured = capture_for_graph(&graph);
    let texts = build_texts(&graph, &captured, &EmbedBudget::default());
    (dir, graph, texts)
}

fn text_for<'a>(graph: &GraphData, texts: &'a [String], name: &str) -> &'a str {
    let idx = graph
        .nodes
        .iter()
        .position(|n| n.name == name)
        .unwrap_or_else(|| panic!("no node named {name}"));
    &texts[idx]
}

#[test]
fn section_prose_reaches_the_embedding_text() {
    let (_dir, graph, texts) = texts_for_doc();
    let text = text_for(&graph, &texts, "Why bother with a local backend?");

    assert!(
        text.contains("spin up an embedding server"),
        "the paragraph is the description of a doc section: {text}"
    );
    assert!(
        text.contains("Most users just want to index a repo"),
        "prose after a fence is not lost: {text}"
    );
    assert!(
        !text.contains("cargo run"),
        "fenced code stays in the sparse channel: {text}"
    );
}

#[test]
fn a_parent_heading_does_not_absorb_its_children() {
    let (_dir, graph, texts) = texts_for_doc();
    let parent = text_for(&graph, &texts, "Embedding Backends");

    assert!(parent.contains("Two backends ship in the box"), "{parent}");
    assert!(
        !parent.contains("spin up an embedding server"),
        "child prose in the parent's vector would blur both and make an edit \
         to one subsection re-embed every ancestor: {parent}"
    );
}

#[test]
fn markdown_gets_no_mangled_comment_notes() {
    let (_dir, graph, texts) = texts_for_doc();

    // `#` opens a comment in the scanner storage::comments uses, which in
    // markdown means every heading line — and its string tracking trips on
    // apostrophes and backticks. Prose files opt out entirely.
    for (node, text) in graph.nodes.iter().zip(&texts) {
        assert!(
            !text.contains("Notes:"),
            "{} embedded comment noise: {text}",
            node.name
        );
    }
}

#[test]
fn link_targets_are_flattened_to_their_text() {
    let (_dir, graph, texts) = texts_for_doc();
    let text = text_for(&graph, &texts, "Hosted endpoints");

    assert!(text.contains("See the ingest guide for the flags"), "{text}");
    assert!(
        !text.contains("./ingest.md"),
        "a relative path is not query vocabulary; it is already an edge: {text}"
    );
}

// ---- the embedding budget, through the real pipeline ---------------------
//
// `EmbedBudget` is unit-tested in `limits`, but the property that matters is
// end-to-end: the indexer must keep the full prose while only the *embedded*
// text is trimmed. Getting that split wrong is what made the cap a re-index
// concern instead of a re-embed one, and nothing below the limits module
// would have noticed.

/// Index a document whose section prose is deliberately longer than any
/// budget under test, and return the graph plus a text-builder closure.
fn long_section_graph() -> (TempDir, GraphData, HashMap<String, CapturedCode>) {
    let dir = TempDir::new().expect("temp dir");
    let body = "The ingest pipeline reads every supported file and slices each symbol out. "
        .repeat(40);
    fs::write(
        dir.path().join("LONG.md"),
        format!("# Long section\n\n{}\n", body),
    )
    .expect("write doc");

    let graph_json = build_graph(index(dir.path().to_string_lossy().to_string()));
    let graph: GraphData = serde_json::from_str(&graph_json).expect("graph parses");
    let captured = capture_for_graph(&graph);
    (dir, graph, captured)
}

fn concept_text<'a>(graph: &GraphData, texts: &'a [String]) -> &'a str {
    let idx = graph
        .nodes
        .iter()
        .position(|n| n.name == "Long section")
        .expect("heading node");
    &texts[idx]
}

#[test]
fn the_indexer_keeps_full_prose_regardless_of_any_budget() {
    let (_dir, graph, _captured) = long_section_graph();
    let doc = graph
        .nodes
        .iter()
        .find(|n| n.name == "Long section")
        .and_then(|n| n.docstring.as_deref())
        .expect("section prose captured");

    // ~3 KB: well past every budget below, and kept in full at index time.
    assert!(doc.len() > 2_500, "indexer kept {} chars", doc.len());
    assert!(
        !doc.ends_with('…'),
        "the index-stage cap is 8 KB, so this must not be truncated yet"
    );
}

#[test]
fn a_tight_budget_trims_the_embedded_text_not_the_graph() {
    let (_dir, graph, captured) = long_section_graph();
    let budget = EmbedBudget {
        description_chars: 400,
        window_tokens: Some(512),
        source: BudgetSource::Flag,
    };
    let texts = build_texts(&graph, &captured, &budget);
    let text = concept_text(&graph, &texts);

    assert!(
        text.contains('…'),
        "trimming must be visible in the embedded text: {text}"
    );
    // The description is bounded by the budget; the rest of the text is the
    // heading, the split name and the Related: list.
    assert!(
        text.len() < 400 + 400,
        "embedded text should be near the budget, got {}",
        text.len()
    );
    // The graph still carries everything.
    let doc = graph
        .nodes
        .iter()
        .find(|n| n.name == "Long section")
        .and_then(|n| n.docstring.as_deref())
        .unwrap();
    assert!(doc.len() > text.len(), "the graph keeps more than the vector sees");
}

#[test]
fn a_generous_budget_embeds_more_of_the_same_graph() {
    let (_dir, graph, captured) = long_section_graph();
    let tight = build_texts(
        &graph,
        &captured,
        &EmbedBudget {
            description_chars: 400,
            window_tokens: Some(512),
            source: BudgetSource::Flag,
        },
    );
    let roomy = build_texts(
        &graph,
        &captured,
        &EmbedBudget {
            description_chars: 4_000,
            window_tokens: Some(8_192),
            source: BudgetSource::Flag,
        },
    );

    let a = concept_text(&graph, &tight).len();
    let b = concept_text(&graph, &roomy).len();
    assert!(b > a * 2, "a longer window must embed more: {a} vs {b}");

    // This is the whole point of moving the cap to embed stage: the same
    // indexed graph yields different embedded text, so switching to a
    // longer-window model needs a re-embed, not a re-index.
    assert!(!concept_text(&graph, &roomy).contains('…'), "4 KB fits the prose");
}

#[test]
fn the_default_budget_matches_the_model_derivation() {
    // `ug gen` resolves the budget from the loaded model; a caller that only
    // has `EmbedBudget::default()` must get the documented fallback rather
    // than an unbounded description.
    let (_dir, graph, captured) = long_section_graph();
    let texts = build_texts(&graph, &captured, &EmbedBudget::default());
    let text = concept_text(&graph, &texts);
    assert!(text.contains('…'), "the 1,500-char fallback still bounds it");

    let derived = EmbedBudget::resolve("bge-small-en-v1.5", None);
    let derived_texts = build_texts(&graph, &captured, &derived);
    assert!(
        concept_text(&graph, &derived_texts).len() < text.len(),
        "the 512-token derivation is tighter than the unknown-model fallback"
    );
}
