//! The assembled visualization page is well-formed.
//!
//! `build.rs` stitches `src/vis/index.html` + `src/vis/css/*` +
//! `src/vis/js/*` into one self-contained page. The failure mode that
//! matters is silent: a stray `</script>` in a JS part, a lost placeholder,
//! or a part that stops being picked up produces a page the browser loads
//! *without complaint* and half-renders. build.rs catches the first two at
//! compile time; this catches the shape of the result.
//!
//! These read the parts from the source tree rather than the build output,
//! so they fail on the thing a person would actually get wrong — editing a
//! part — rather than on the assembly, which has no branches.

use std::fs;
use std::path::{Path, PathBuf};

fn vis_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/vis")
}

fn parts(sub: &str, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(vis_dir().join(sub))
        .expect("part directory exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    v.sort();
    v
}

/// The single assembled page, rebuilt the same way `build.rs` does it.
fn assembled() -> String {
    let skeleton = fs::read_to_string(vis_dir().join("index.html")).expect("skeleton");
    let join = |files: Vec<PathBuf>| {
        files
            .iter()
            .map(|p| {
                let b = fs::read_to_string(p).expect("part readable");
                b.strip_suffix('\n').unwrap_or(&b).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    skeleton
        .replace("{{CSS}}", &join(parts("css", "css")))
        .replace("{{JS}}", &join(parts("js", "js")))
}

#[test]
fn every_part_directory_has_parts() {
    assert!(!parts("css", "css").is_empty(), "no css parts");
    assert!(!parts("js", "js").is_empty(), "no js parts");
}

/// Order is carried by the filename, so a part without a numeric prefix
/// would sort somewhere arbitrary — and a stylesheet in the wrong order is
/// a bug nobody notices until something looks subtly wrong.
#[test]
fn parts_are_ordered_by_a_numeric_prefix() {
    for (sub, ext) in [("css", "css"), ("js", "js")] {
        let mut seen: Vec<u32> = Vec::new();
        for p in parts(sub, ext) {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let prefix: String = name.chars().take_while(char::is_ascii_digit).collect();
            let n: u32 = prefix
                .parse()
                .unwrap_or_else(|_| panic!("{name}: parts need a numeric prefix, e.g. 04-{name}"));
            assert!(!seen.contains(&n), "{name}: duplicate order prefix {n}");
            seen.push(n);
        }
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{sub}: prefixes not ascending");
    }
}

/// The hazard build.rs guards at compile time, asserted here too so the
/// reason survives: a literal closing tag inside a part ends the block
/// early and the browser reports nothing at all.
#[test]
fn no_part_contains_a_literal_closing_tag() {
    for p in parts("css", "css") {
        let body = fs::read_to_string(&p).unwrap();
        assert!(!body.contains("</style>"), "{}: contains </style>", p.display());
    }
    for p in parts("js", "js") {
        let body = fs::read_to_string(&p).unwrap();
        assert!(
            !body.contains("</script>"),
            "{}: contains a literal </script>, which would truncate the page",
            p.display()
        );
    }
}

#[test]
fn the_skeleton_has_exactly_one_of_each_placeholder() {
    let skeleton = fs::read_to_string(vis_dir().join("index.html")).unwrap();
    for token in ["{{CSS}}", "{{JS}}"] {
        assert_eq!(
            skeleton.matches(token).count(),
            1,
            "index.html needs exactly one {token}"
        );
    }
}

/// The shape the browser has to see: one style block, one script block,
/// each opened and closed exactly once. Getting this wrong is how a
/// half-loading page happens.
#[test]
fn the_assembled_page_is_well_formed() {
    let page = assembled();
    assert_eq!(page.matches("<style>").count(), 1, "style open");
    assert_eq!(page.matches("</style>").count(), 1, "style close");
    assert_eq!(page.matches("</script>").count(), 1, "script close");
    assert!(page.contains(r#"<script type="module">"#), "script open");
    assert!(page.find("<style>") < page.find("</style>"), "style block inverted");

    // Every part must actually land in the output — a file that stops being
    // picked up removes a feature silently.
    for (sub, ext) in [("css", "css"), ("js", "js")] {
        for p in parts(sub, ext) {
            let body = fs::read_to_string(&p).unwrap();
            let probe = body
                .lines()
                .find(|l| l.trim().len() > 20)
                .expect("part has content");
            assert!(
                page.contains(probe.trim()),
                "{} did not reach the assembled page",
                p.display()
            );
        }
    }
}

/// The Insights pane is the newest feature in the page and the one most
/// likely to be split across parts by a later edit; this pins the pieces
/// that have to travel together.
#[test]
fn the_insights_pane_survives_assembly() {
    let page = assembled();
    for probe in [
        r#"data-sub="insights""#,   // subtab button
        r#"id="ins-presets""#,      // markup
        "function wireInsights",    // behaviour
        ".ins-preset {",            // styling
        "/api/presets",             // the endpoint it reads
    ] {
        assert!(page.contains(probe), "assembled page is missing {probe:?}");
    }
}
