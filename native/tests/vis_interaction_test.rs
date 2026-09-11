//! The info panel's checkable halves, run under `node`.
//!
//! `src/vis/js/14-interaction.js` is where every way of picking a node ends
//! up — the canvas, search, a chat citation, the catalog, a tour stop — and
//! `handleClick` turns the chosen node into the panel the reader acts on. At
//! 415 lines with 22 symbols depending on it, it is the most depended-upon
//! untested thing on the page, and four of its parts fail quietly:
//!
//! * `mdToHtml` renders a docstring into `innerHTML`. It is hand-rolled, so
//!   it is also the panel's injection surface: a `javascript:` link that
//!   survives it is a live one. Indexed repository content goes through here.
//! * `longFieldRow` / `chipRow` build the field rows, deciding when a value
//!   is too long to show inline and when a name list collapses. Both carry
//!   symbol names and file paths into `innerHTML`.
//! * `parseChunkText` reads the stored chunk text back into panel fields. A
//!   mis-parse shows the wrong prose under the right heading, silently.
//! * `findNodeByName` decides which chips link. Its server-mode branch has to
//!   memoise misses as well as hits, or the re-render it triggers queues the
//!   same name forever.
//!
//! `tests/js/interaction.mjs` lifts all four out of the shipped parts with a
//! string slice, so this cannot pass against a copy that has drifted from
//! what ships.
//!
//! Booting the real page to check this instead is a runaway CPU load — see
//! Agents.md §10r, which exists because it happened.
//!
//! **This needs `node` on `PATH`**, like `vis_ask_dispatch_test.rs`, and
//! fails loudly rather than skipping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run() -> (bool, String) {
    let native = repo_native();
    let harness = native.join("tests/js/interaction.mjs");
    let out = Command::new("node")
        .arg(&harness)
        // The panel is assembled from three parts: the click handler itself,
        // `escapeHtml`, and the field documentation its labels read.
        .arg(native.join("src/vis/js/14-interaction.js"))
        .arg(native.join("src/vis/js/17-info-drag.js"))
        .arg(native.join("src/vis/js/02-dialogs.js"))
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
fn the_info_panel_renders_and_resolves_as_specified() {
    let (ok, text) = run();
    assert!(ok, "info panel check failed:\n{text}");
    assert!(
        text.contains("the info panel renders and resolves as specified"),
        "the harness did not report its checks:\n{text}"
    );

    // The four parts are independently breakable, and the harness lifts each
    // by name. A rename upstream makes the lift throw, but a section that
    // silently stopped running would still exit 0 — so each is named here.
    for section in [
        "rendering a docstring as markdown",
        "reading a chunk back into fields",
        "building the panel fields",
        "resolving a chip name to a node",
    ] {
        assert!(
            text.contains(section),
            "the `{section}` checks must run:\n{text}"
        );
    }

    // A docstring is indexed repository content rendered into innerHTML. If
    // the scheme filter stops running, the panel executes what it indexes.
    assert!(
        text.contains("a javascript: link is neutralised")
            && text.contains("a scheme-relative link is neutralised"),
        "the link scheme checks must run:\n{text}"
    );

    // Memoising a miss is what stops the server-mode re-render from queueing
    // the same name forever. It is the one check here whose failure is a hang
    // rather than a wrong pixel.
    assert!(
        text.contains("a memoised miss is not asked for twice"),
        "the name-probe termination check must run:\n{text}"
    );
}
