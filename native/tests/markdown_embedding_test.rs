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
use ultragraph::limits::EmbedBudget;
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
