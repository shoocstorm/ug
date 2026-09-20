//! The Answer tab's markdown, checked under `node`.
//!
//! An answer arrives as markdown and is read as HTML, and two parts of
//! `src/vis/js/06-chat.js` stand between them:
//!
//! 1. **`renderMarkdown`** — the parser. Whatever it does not understand
//!    reaches the reader as punctuation: a comparison table as a wall of
//!    pipes, a nested bullet as a list that restarts at the top. It is also
//!    where *model output* — untrusted by construction — goes through
//!    `innerHTML`, so a table cell and a list item have to escape exactly as
//!    carefully as a paragraph does.
//! 2. **`stableEnd`** — where a half-written answer may be frozen. The
//!    streamed render freezes finished blocks and re-parses only the tail;
//!    freeze inside a fence or halfway down a list and what you read while
//!    the answer streams disagrees with what stands when it lands.
//!
//! The harness lifts both out of the shipped part with a string slice, so it
//! cannot pass against a copy that has drifted from what ships.
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
    let harness = native.join("tests/js/chat_markdown.mjs");
    let out = Command::new("node")
        .arg(&harness)
        .arg(native.join("src/vis/js/06-chat.js"))
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
fn an_answer_renders_markdown_as_specified() {
    let (ok, text) = run();
    assert!(ok, "answer markdown check failed:\n{text}");
    assert!(
        text.contains("the answer renders markdown and reports its cost as specified"),
        "the harness did not report its checks:\n{text}"
    );
    // The constructs are independently breakable, so all of them have to have
    // run — a harness that silently lifted only the parser would still exit 0.
    assert!(
        text.contains("rendering a table")
            && text.contains("rendering a list")
            && text.contains("freezing a streaming answer"),
        "every part must be exercised:\n{text}"
    );
    // A rendered answer puts model output into innerHTML. If these stop
    // running, the Answer tab is an injection sink.
    assert!(
        text.contains("escaping model output"),
        "the escaping checks must run:\n{text}"
    );
    // Three unrelated quantities share the cost box. The provider's raw total
    // used to sit unlabelled beside a baseline it has nothing to do with.
    assert!(
        text.contains("reporting what the turn cost"),
        "the cost-box checks must run:\n{text}"
    );
}
