//! What a Graph Walk hands back when it ends, checked under `node`.
//!
//! Focus mode (the 1-hop dimming anchored on a selected node) is inert while a
//! walk runs — `nodeLightingFor` reads `state.walkActive` first and shades
//! every node by its hop. But focus can still be *switched on* mid-walk:
//! opening a node's details from the walk's node list goes through
//! `handleClick`, which anchors focus on it.
//!
//! So the anchor is invisible until the walk exits, and then it is the only
//! thing shading the graph. That failure is the quiet kind: the walk played
//! correctly, every node on screen is real, and the graph simply comes back
//! dimmed around a node the reader did not choose to dim — with solo armed,
//! with most of it gone.
//!
//! The harness lifts `exitWalk`, `enterFocus`/`exitFocus` and the renderer's
//! lighting accessors out of the shipped parts with a string slice, so it
//! cannot pass against a copy that has drifted from what ships.
//!
//! **This needs `node` on `PATH`**, like `vis_ask_dispatch_test.rs`, and fails
//! loudly rather than skipping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run() -> (bool, String) {
    let native = repo_native();
    let harness = native.join("tests/js/walk_exit.mjs");
    let out = Command::new("node")
        .arg(&harness)
        .arg(native.join("src/vis/js/18-walk.js"))
        .arg(native.join("src/vis/js/08-sidebar-nav.js"))
        .arg(native.join("src/vis/js/10-render-core.js"))
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
fn a_walk_hands_the_graph_back_whole() {
    let (ok, text) = run();
    assert!(ok, "walk exit check failed:\n{text}");
    assert!(
        text.contains("a walk hands the graph back whole"),
        "the harness did not report its checks:\n{text}"
    );
    // Three independently breakable halves: that focus can be anchored during
    // a walk at all, that exiting drops it, and that a no-op exit (the
    // launcher's tear-down before every run) leaves an ordinary focus alone.
    assert!(
        text.contains("anchoring focus during a walk")
            && text.contains("exiting the walk")
            && text.contains("exiting with no walk running"),
        "every half must be exercised:\n{text}"
    );
    // The selection is deliberately *not* cleared — a walk ends where the
    // reader keeps exploring.
    assert!(
        text.contains("the selection survives the exit"),
        "the selection check must run:\n{text}"
    );
}
