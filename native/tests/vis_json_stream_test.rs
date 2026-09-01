//! The local-mode streaming JSON parser, run under `node`.
//!
//! `src/vis/js/00-preamble.js` hand-rolls a JSON scanner so a graph past
//! V8's 536,870,888-character string ceiling can still be loaded in file
//! mode (P12.2). It is the one piece of the vis layer that reimplements
//! something the platform normally does, and its failure mode is a *wrong
//! graph* rather than an error: a chunk boundary landing inside a string, an
//! escape or a key silently loses or corrupts elements.
//!
//! The checks themselves are JS, in `tests/js/graph_json_stream.mjs`, because
//! the only reference worth comparing against is the platform's own
//! `JSON.parse` — every document is parsed both ways at chunk sizes from one
//! byte upward and compared for deep equality. This file is the wrapper that
//! puts them in the suite.
//!
//! **This needs `node` on `PATH`**, which CI's `ubuntu-latest` image and the
//! repo's own harnesses already assume. A missing `node` fails loudly rather
//! than skipping: a parser on the load path with no coverage at all is worse
//! than a red test that says why.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run(fixtures: &[PathBuf]) -> (bool, String) {
    let harness = repo_native().join("tests/js/graph_json_stream.mjs");
    let preamble = repo_native().join("src/vis/js/00-preamble.js");
    let mut cmd = Command::new("node");
    // The 500k fixture holds ~2.5 GB of parsed objects; node's default heap
    // is smaller than that and would die as an OOM rather than a diff.
    cmd.arg("--max-old-space-size=14336")
        .arg(&harness)
        .arg(&preamble);
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

/// Every seam, on documents small enough to split one byte at a time.
#[test]
fn the_streaming_graph_parser_matches_json_parse() {
    let (ok, text) = run(&[]);
    assert!(ok, "streaming parser check failed:\n{text}");
    assert!(
        text.contains("parses of") && text.contains("malformed documents rejected"),
        "the harness did not report both checks:\n{text}"
    );
}

/// The same comparison against real graphs, including one no single string
/// can hold. `#[ignore]` because the fixtures are a developer's `~/.ug`, not
/// something the suite can generate.
///
/// ```text
/// cargo nextest run -E 'test(the_streaming_graph_parser_reads_a_real_graph)' --run-ignored all
/// ```
#[test]
#[ignore]
fn the_streaming_graph_parser_reads_a_real_graph() {
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
    assert!(ok, "streaming parser check failed on a real graph:\n{text}");
    assert!(
        text.contains("identical to JSON.parse"),
        "no real graph was actually compared:\n{text}"
    );
}
