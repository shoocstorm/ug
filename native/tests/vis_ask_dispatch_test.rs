//! The Ask bar's checkable halves, run under `node`.
//!
//! Every question the page can be asked now enters through one input, and
//! `src/vis/js/25-ask.js` decides what happens to it. Three of its parts fail
//! quietly rather than loudly:
//!
//! * `classifyAsk` picks the mode from the raw text. Misclassify and typing a
//!   symbol name spends a model call, or a plain-language question searches
//!   for a node literally named "how does auth work" and finds nothing.
//! * `buildHitRow` / `askProvenanceHtml` build the row the reader acts on. It
//!   puts repository content — symbol names, file paths — through `innerHTML`,
//!   and it carries the `matched_by` / `hop` / score strip that is the whole
//!   difference between a list of results and a checkable answer.
//! * `askBlock` / `clearAskStream` decide what the column holds. Every mode
//!   writes through them, and when they appended instead of replacing, a walk
//!   along the mode strip left a block per click and a panel that only grew.
//!
//! `tests/js/ask_dispatch.mjs` lifts all three out of the shipped part with a
//! string slice, so this cannot pass against a copy that has drifted from
//! what ships.
//!
//! Booting the real page to check this instead is a runaway CPU load — see
//! Agents.md §10r, which exists because it happened.
//!
//! **This needs `node` on `PATH`**, like `vis_context_panel_test.rs`, and
//! fails loudly rather than skipping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run() -> (bool, String) {
    let native = repo_native();
    let harness = native.join("tests/js/ask_dispatch.mjs");
    let out = Command::new("node")
        .arg(&harness)
        .arg(native.join("src/vis/js/25-ask.js"))
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "could not run `node` (needed by this test): {e}\nharness: {}",
                harness.display()
            )
        });
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn the_ask_bar_classifies_and_renders_as_specified() {
    let (ok, text) = run();
    assert!(ok, "ask bar check failed:\n{text}");
    assert!(
        text.contains("the ask bar classifies and renders as specified"),
        "the harness did not report its checks:\n{text}"
    );
    // The halves are independently breakable, so all of them have to have run
    // — a harness that silently lifted only one would still exit 0.
    assert!(
        text.contains("classifying a query") && text.contains("rendering a result row"),
        "both halves must be exercised:\n{text}"
    );
    // A row puts indexed repository content into innerHTML. If these stop
    // running, the Ask column is an XSS sink.
    assert!(
        text.contains("escaping repository content"),
        "the escaping checks must run:\n{text}"
    );
    // Provenance is the point of the rewrite: a result that cannot say how it
    // was reached is the thing this replaced.
    assert!(
        text.contains("showing how a hit was reached"),
        "the provenance checks must run:\n{text}"
    );
    // One question, one block. Appending is how the column grew without bound.
    assert!(
        text.contains("keeping one question in the stream"),
        "the stream checks must run:\n{text}"
    );
}
