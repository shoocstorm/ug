//! The Context tab's pure halves, run under `node`.
//!
//! `src/vis/js/24-context.js` turns a `POST /api/tools/context` envelope into
//! the info panel's markup, and `src/vis/js/10-render-core.js` decides what the
//! pack tells the canvas. Both fail quietly rather than loudly: a renamed wire
//! field renders an empty pack, which looks exactly like a symbol with no
//! callers, and a mis-ordered style tier recolours the graph out from under a
//! running walk or tour.
//!
//! Neither needs a browser — they are pure functions of `state` and the
//! response. `tests/js/context_panel.mjs` lifts them out of the shipped parts
//! with a string slice (so it cannot pass against a copy that has drifted) and
//! exercises them, including the escaping of repository content, which reaches
//! the panel through `innerHTML`.
//!
//! Booting the real page to check this instead is a runaway CPU load — see
//! Agents.md §10r, which exists because it happened.
//!
//! **This needs `node` on `PATH`**, like `vis_edge_store_test.rs`, and fails
//! loudly rather than skipping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run(sample: Option<&Path>) -> (bool, String) {
    let native = repo_native();
    let harness = native.join("tests/js/context_panel.mjs");
    let mut cmd = Command::new("node");
    cmd.arg(&harness)
        .arg(native.join("src/vis/js/24-context.js"))
        .arg(native.join("src/vis/js/10-render-core.js"))
        .arg(native.join("src/vis/js/00-preamble.js"));
    if let Some(s) = sample {
        cmd.arg(s);
    }
    let out = cmd.output().unwrap_or_else(|e| {
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
fn the_context_panel_renders_and_paints_as_specified() {
    let (ok, text) = run(None);
    assert!(ok, "context panel check failed:\n{text}");
    assert!(
        text.contains("the context panel renders and paints as specified"),
        "the harness did not report its checks:\n{text}"
    );
    // The two halves are independently breakable, so both have to have run —
    // a harness that silently lifted only one would still exit 0.
    assert!(
        text.contains("rendering the envelope") && text.contains("painting the pack"),
        "both halves must be exercised:\n{text}"
    );
    // The pack carries source code and identifiers straight from the indexed
    // repo into innerHTML. If these stop running, the panel is an XSS sink.
    assert!(
        text.contains("escaping repository content"),
        "the escaping checks must run:\n{text}"
    );
}
