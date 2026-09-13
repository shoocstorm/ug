//! The Changes panel, checked under `node`.
//!
//! The panel that launches a walk decides two things a screenshot would not
//! catch:
//!
//! 1. **What a stop's badge says.** A `caller` is *unchanged* code that
//!    merely reaches the diff. If it renders like a `changed` stop — or
//!    renders with no badge — the reader concludes the diff edited it, which
//!    is the exact misreading a diff walk exists to prevent. That failure is
//!    invisible: the walk still plays, the camera still flies, every stop is
//!    real code.
//!
//! 2. **What the preview puts on screen.** It writes commit subjects and
//!    file paths into the DOM, so it is also where a repository's own
//!    content reaches `innerHTML`.
//!
//! The harness lifts both functions out of the shipped part with a string
//! slice, so it cannot pass against a copy that has drifted from what ships.
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
    let harness = native.join("tests/js/changes_panel.mjs");
    let out = Command::new("node")
        .arg(&harness)
        .arg(native.join("src/vis/js/27-changes.js"))
        .arg(native.join("src/vis/index.html"))
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
fn the_changes_panel_labels_and_previews_as_specified() {
    let (ok, text) = run();
    assert!(ok, "changes panel check failed:\n{text}");
    assert!(
        text.contains("the changes panel labels and previews as specified"),
        "the harness did not report its checks:\n{text}"
    );
    // Three independently breakable halves. A harness that silently lifted
    // only one would still exit 0.
    assert!(
        text.contains("labelling a walk stop")
            && text.contains("listing a commit")
            && text.contains("previewing a diff")
            && text.contains("reaching for elements that exist"),
        "every half must be exercised:\n{text}"
    );
    // The preview puts repository content into the DOM. If these stop
    // running, the launcher is an XSS sink.
    assert!(
        text.contains("escaping repository content"),
        "the escaping checks must run:\n{text}"
    );
}
