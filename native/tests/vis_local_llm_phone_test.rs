//! How the in-browser model loads on a phone, checked under `node`.
//!
//! iOS closes a Safari tab that grows past its memory limit, and there is no
//! error to catch: the page reloads. `src/vis/js/28-local-llm.js` stays under
//! that limit by offering a phone only a small file, loading it on the CPU at
//! a short context window with a lean KV cache, and picking the wllama build
//! Safari can actually compile.
//!
//! Every one of those is a branch a desktop never takes, so a regression here
//! is invisible from the machine the code is written on — the first symptom
//! is a phone reloading the page. `tests/js/local_llm_phone.mjs` lifts the
//! decisions out of the shipped part with a string slice, so it cannot pass
//! against a copy that has drifted from what ships.
//!
//! **This needs `node` on `PATH`**, like `vis_ask_dispatch_test.rs`, and fails
//! loudly rather than skipping.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_native() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn a_phone_loads_a_model_it_can_hold() {
    let native = repo_native();
    let harness = native.join("tests/js/local_llm_phone.mjs");
    let out = Command::new("node")
        .arg(&harness)
        .arg(native.join("src/vis/js/28-local-llm.js"))
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "could not run `node` (needed by this test): {e}\nharness: {}",
                harness.display()
            )
        });
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "phone load check failed:\n{text}");
    assert!(
        text.contains("the phone load plan holds"),
        "the harness did not report its checks:\n{text}"
    );
    // The halves that break independently: what a phone is offered, how it is
    // loaded, which build runs, and what happens to a prompt too long for it.
    for section in [
        "a phone is offered only what it can hold",
        "a phone starts lighter and steps down",
        "the build is chosen by compiling",
        "a prompt is cut to the window",
    ] {
        assert!(text.contains(section), "{section} must be exercised:\n{text}");
    }
}
