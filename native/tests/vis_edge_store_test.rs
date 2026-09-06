//! The local-mode edge store, run under `node`.
//!
//! `src/vis/js/02-dialogs.js` holds `graph.json`'s edges as typed columns plus
//! a CSR incidence index rather than as `{ source, target, rel }` objects in a
//! `Map` of per-node arrays — 254 MB against 40 MB on a 485k-node graph
//! (P12.25). Every reader still sees the old object shape, built per call.
//!
//! The failure mode is a *wrong graph* rather than an error. An off-by-one in
//! the prefix sum, a self-loop counted in one adjacency list twice, or a
//! dropped edge whose columns were never trimmed all produce a page that draws
//! neighbourhoods nobody has, with nothing on the console.
//!
//! So the check in `tests/js/edge_store.mjs` is equality against the shape the
//! store replaced, transcribed there from the code it replaced, over graphs
//! chosen for the ways a CSR build goes wrong — self-loops, parallel edges,
//! dropped endpoints, a hub, and the last node holding every edge. This file
//! is the wrapper that puts them in the suite.
//!
//! **This needs `node` on `PATH`**, for the same reason
//! `vis_json_stream_test.rs` does, and fails loudly rather than skipping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run(fixtures: &[PathBuf]) -> (bool, String) {
    let harness = repo_native().join("tests/js/edge_store.mjs");
    let dialogs = repo_native().join("src/vis/js/02-dialogs.js");
    let mut cmd = Command::new("node");
    // The 500k fixture parses to ~2.5 GB of objects before the store is built
    // from it, and the reference shape is built alongside the store.
    cmd.arg("--max-old-space-size=14336")
        .arg(&harness)
        .arg(&dialogs);
    for f in fixtures {
        cmd.arg(f);
    }
    let out = cmd.output().unwrap_or_else(|e| {
        panic!("could not run `node` (needed by this test): {e}\nharness: {}", harness.display())
    });
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The shapes a CSR build gets wrong, on graphs small enough to name.
#[test]
fn the_edge_store_matches_the_shape_it_replaces() {
    let (ok, text) = run(&[]);
    assert!(ok, "edge store check failed:\n{text}");
    assert!(
        text.contains("graphs checked against the object-and-Map shape"),
        "the harness did not report its checks:\n{text}"
    );
}

/// The same comparison against real graphs, edge for edge across every node's
/// adjacency. `#[ignore]` because the fixtures are a developer's `~/.ug`, not
/// something the suite can generate.
///
/// ```text
/// cargo nextest run -E 'test(the_edge_store_reads_a_real_graph)' --run-ignored all
/// ```
#[test]
#[ignore]
fn the_edge_store_reads_a_real_graph() {
    let home = std::env::var("UG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").expect("HOME")).join(".ug"));
    let fixtures: Vec<PathBuf> = ["neo4j", "big500k"]
        .iter()
        .map(|p| home.join(p).join("graph.json"))
        .filter(|p| p.exists())
        .collect();
    assert!(!fixtures.is_empty(), "no graph fixtures under {}", home.display());
    let (ok, text) = run(&fixtures);
    assert!(ok, "edge store check failed on a real graph:\n{text}");
    assert!(
        text.contains("identical to the object-and-Map shape"),
        "no real graph was actually compared:\n{text}"
    );
}
