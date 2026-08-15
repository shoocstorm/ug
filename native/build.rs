use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    assemble_visualization();
    // Only the `ug-app` desktop binary needs a Tauri context, and that binary
    // is behind the `app` feature — see Cargo.toml. Cargo compiles build
    // scripts with the package's enabled features as `cfg`s, so this arm
    // switches off with the dependency itself.
    #[cfg(feature = "app")]
    tauri_build::build();
}

/// Stitch `src/vis/index.html` + `src/vis/css/*` + `src/vis/js/*` into the
/// single self-contained page the binary embeds.
///
/// The page ships as one file — `ug gen` writes it into a user's output
/// directory and it is opened straight from disk, so it cannot reference
/// sibling assets. But *editing* it as one file meant working in a
/// 15,000-line document, which is past the point where either a person or
/// an agent can hold enough of it in view to change one thing safely.
///
/// So the parts are the source of truth and this reassembles them. Two
/// decisions worth keeping:
///
/// 1. **The output goes to `OUT_DIR`, never back into `src/`.** A generated
///    file sitting in the source tree is one someone will eventually edit
///    directly — it looks like every other file, the change works when
///    tested, and the next `cargo build` silently discards it. Keeping the
///    artefact out of the tree makes that mistake impossible rather than
///    merely discouraged. It also stops `cargo build` from dirtying `src/`,
///    which can loop rebuilds and breaks read-only or vendored checkouts.
///
/// 2. **Order comes from the filename**, not from a list in this file. A
///    manifest is one more thing to keep in sync, and the failure when it
///    drifts is a silently reordered stylesheet. The numeric prefix is
///    visible in `ls`, so the concatenation order is evident from the
///    directory itself.
fn assemble_visualization() {
    let vis = PathBuf::from("src/vis");
    let skeleton_path = vis.join("index.html");

    println!("cargo:rerun-if-changed={}", skeleton_path.display());
    // The directories as well as the files: adding or deleting a part has
    // to trigger a rebuild too, and a per-file watch cannot see a new file.
    println!("cargo:rerun-if-changed={}", vis.join("css").display());
    println!("cargo:rerun-if-changed={}", vis.join("js").display());

    let skeleton = fs::read_to_string(&skeleton_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", skeleton_path.display()));

    let css = concat_parts(&vis.join("css"), "css");
    let js = concat_parts(&vis.join("js"), "js");

    // A `</style>` or `</script>` inside a part would close the block early
    // and silently truncate the page — the browser reports nothing, the
    // remainder becomes markup, and the app half-loads. This is a live
    // hazard, not a theoretical one: the JS already contains a
    // `<script type="module">` inside a string literal.
    reject_closing_tag(&css, "</style>", "css");
    reject_closing_tag(&js, "</script>", "js");

    for token in ["{{CSS}}", "{{JS}}"] {
        assert!(
            skeleton.matches(token).count() == 1,
            "{} must contain exactly one {token} placeholder",
            skeleton_path.display()
        );
    }

    let page = skeleton.replace("{{CSS}}", &css).replace("{{JS}}", &js);

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .join("visualization.html");
    fs::write(&out, page).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}

/// Every `*.<ext>` file in `dir`, joined in filename order.
fn concat_parts(dir: &Path, ext: &str) -> String {
    let mut parts: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    parts.sort();

    assert!(!parts.is_empty(), "no *.{ext} files in {}", dir.display());

    let mut chunks: Vec<String> = Vec::with_capacity(parts.len());
    for part in &parts {
        println!("cargo:rerun-if-changed={}", part.display());
        // Name each part in the assembled output. When something looks
        // wrong in a browser's dev tools, this is what points back at the
        // file to fix — without it the page is 15,000 anonymous lines
        // again, which is the problem this split exists to solve.
        let name = part.file_name().unwrap_or_default().to_string_lossy();
        let banner = match ext {
            "css" => format!("/* ==== {name} ==== */"),
            _ => format!("// ==== {name} ===="),
        };
        let body = fs::read_to_string(part)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", part.display()));
        // Drop the file's own trailing newline before joining. Keeping it
        // *and* joining with one would insert a blank line at every
        // boundary — harmless to a browser, but it means the assembled
        // page is no longer byte-identical to its parts, and that identity
        // is what makes `--verify-vis` a meaningful check.
        chunks.push(format!("{banner}\n{}", body.strip_suffix('\n').unwrap_or(&body)));
    }
    chunks.join("\n")
}

fn reject_closing_tag(body: &str, tag: &str, ext: &str) {
    if let Some(at) = body.find(tag) {
        let line = body[..at].matches('\n').count() + 1;
        panic!(
            "a src/vis/{ext} part contains a literal `{tag}` (assembled line {line}). \
             That would close the block early and truncate the page with no error in \
             the browser. Split the literal — e.g. `'<\\/scr' + 'ipt>'`."
        );
    }
}
