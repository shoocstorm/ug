//! Whether a boundary node keeps its ring while the graph around it is dimmed,
//! checked under `node`.
//!
//! The 3D renderer fades `__boundaryRing` to zero with everything else. The 2D
//! one marks a boundary node twice and can fade neither mark, because both sit
//! outside the colour alpha this page dims with:
//!
//!   1. cosmos.gl's own outline ring, drawn from a *uniform* colour with a
//!      fixed alpha — `cosmosOutlinedIndices` decides who is in the set;
//!   2. the dashed rim baked into the node's glyph image, which the point
//!      shader composites as `max(shape.a, image.a)`, so the image's alpha
//!      wins outright — `cosmosGlyphIndexFor` decides which image is worn.
//!
//! The second is the visible one, and the one missed on the first attempt at
//! this. With either left alone the failure is the quiet kind: a focus, a tour,
//! a walk or a context pack dims the whole graph and every boundary node stays
//! marked at full strength, reading as "these are the relevant ones" — the
//! opposite of what the dimming is saying.
//!
//! The harness lifts both accessors, `cosmosBoundaryIndices`, `cosmosImageKey`
//! and the shared `nodeLightingFor` out of the shipped parts with a string
//! slice, so it cannot pass against a copy that has drifted from what ships.
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
    let harness = native.join("tests/js/boundary_rings.mjs");
    let out = Command::new("node")
        .arg(&harness)
        .arg(native.join("src/vis/js/12-render-cosmos.js"))
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
fn a_dimmed_boundary_node_loses_its_ring() {
    let (ok, text) = run();
    assert!(ok, "boundary ring check failed:\n{text}");
    assert!(
        text.contains("a dimmed boundary node loses its ring"),
        "the harness did not report its checks:\n{text}"
    );
    // Every mode that dims, because each one reaches `nodeLightingFor` by a
    // different branch and only focus was ever looked at by hand.
    assert!(
        text.contains("nothing dimming")
            && text.contains("focus anchored on a plain node")
            && text.contains("focus anchored on a boundary node")
            && text.contains("a tour running")
            && text.contains("a walk running")
            && text.contains("a context pack painted"),
        "every dimming mode must be exercised:\n{text}"
    );
    // Both marks, because fixing one and leaving the other is exactly how this
    // shipped the first time: the outline set was filtered, the baked rim was
    // not, and the graph looked unchanged.
    assert!(
        text.contains("the rim baked into the glyph"),
        "the glyph half must be exercised:\n{text}"
    );
    // The scan is cached per build; a stale cache would ring whichever nodes
    // happen to sit at the old indices after a view swap.
    assert!(
        text.contains("the boundary scan is cached"),
        "the cache check must run:\n{text}"
    );
}
