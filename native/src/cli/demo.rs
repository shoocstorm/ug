//! `ug demo` — publish an indexed repo as a static, self-contained web
//! demo: index → graph → a folder anyone can host.
//!
//! ## Why this is not `ug gen -o <dir>`
//!
//! `gen` produces a *project*: a graph plus a database, vectors, a
//! `project.json`, and a server (`ug serve`) that owns all of it. None of
//! that can be published — the database is an embedded store, the vectors
//! are megabytes of floats nobody's browser wants, and every interesting
//! endpoint needs a process running on the reader's machine.
//!
//! What *can* be published is the part that was always self-contained: the
//! graph and the page that draws it. So this command stops after
//! `build_graph`, writes `graph.json` next to a copy of the visualization,
//! and wraps that copy in [`assets::VIS_DEMO_SHIM`] — a static stand-in for
//! the server (see `src/vis/demo-shim.js`). The result is three files and a
//! favicon that work from any static host, including `file://`-adjacent
//! setups like `python3 -m http.server`.
//!
//! ## The point of it
//!
//! Nobody installs a graph tool to find out whether they want a graph tool.
//! `docs/ug-website/demo/` is the answer: a visitor flies a real indexed
//! repo before they have installed anything, and the demo deploys with the
//! rest of the site because `firebase.json` publishes the website folder
//! wholesale.
//!
//! ## What it deliberately does not do
//!
//! No database, no embedder, no chat, no tours — those need the local index
//! and there is nothing honest to publish for them. The shim says so in the
//! UI rather than letting them fail silently.
//!
//! ## Paths are scrubbed
//!
//! A graph carries the absolute path it was indexed from — in `stats.repoRoot`,
//! and occasionally inside a docstring that quotes one. Publishing that
//! publishes the author's home directory layout, so [`scrub_paths`] rewrites
//! both before anything is written.

use std::fs;
use std::path::Path;

use ultragraph::{build_graph, index, C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

use super::args::{first_positional, flag_value, has_flag};
use super::io::die;

/// The element count past which the page stops drawing the whole graph and
/// switches to solo mode — one node and its neighbourhood at a time.
///
/// Mirrors `SOLO_THRESHOLD` in `src/vis/js/16-solo-view.js`, which is the
/// source of truth; `the_solo_threshold_matches_the_renderer` fails if the
/// two drift. Duplicated rather than derived because it is needed on the
/// other side of a language boundary, and the cost of being wrong is a demo
/// that silently opens on an empty canvas — correct behaviour for a large
/// repo, and a bad first impression for a demo, which is exactly the kind of
/// change worth a warning at publish time.
const SOLO_THRESHOLD: usize = 1_000_000;

/// A short hash of everything that decides what the demo *renders*: the
/// assembled visualization page, the shim wrapped around it, and both
/// renderer bundles.
///
/// Stamped into `demo.json` so staleness becomes a *fact* rather than
/// something a person has to remember. The published page is a copy of a
/// page that lives inside this binary, and a copy has no way to notice its
/// original moved — edit `src/vis/js/11-render-three.js`, and the live demo
/// keeps serving the old renderer with nothing anywhere to say so. The
/// fingerprint is what `the_published_demo_page_is_not_stale` compares, so
/// that silence turns into a failing test naming the command that fixes it.
///
/// The bundles are in the hash because they are shipped files too. Re-vendor
/// cosmos.gl without them and the demo keeps serving the old renderer with
/// an `index.html` that still matches — the exact silent staleness this
/// guard exists to prevent, one level down.
fn vis_fingerprint() -> String {
    fingerprint_of(&[
        crate::assets::VIS_HTML.as_bytes(),
        crate::assets::VIS_DEMO_SHIM.as_bytes(),
        crate::assets::VIS_THREEJS_BUNDLE,
        crate::assets::VIS_COSMOS_BUNDLE,
    ])
}

/// The hash itself, over its inputs — split out from [`vis_fingerprint`] so
/// a test can prove it actually reacts to a change in any of them, which is
/// the whole property the staleness guard rests on.
fn fingerprint_of(parts: &[&[u8]]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
        // A separator after every part, so a byte moving across the boundary
        // between two of them still changes the hash.
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex()[..16].to_string()
}

/// Where `ug demo` writes when the caller names no `-o`.
///
/// The command exists for the website's demo page, so when it is run from a
/// checkout that has one, that is the answer. Anywhere else it stays in the
/// cwd rather than inventing a path into a tree that does not exist.
fn default_output() -> &'static str {
    if Path::new("docs/ug-website").is_dir() {
        "docs/ug-website/demo"
    } else {
        "ug-demo"
    }
}

/// Last path component of `path`, or `path` itself if it has none.
fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Rewrite the two absolute paths a published graph would otherwise leak.
///
/// Applied to the *serialized* graph rather than field by field, because the
/// leak is not confined to a known field: `stats.repoRoot` is the obvious
/// one, but an indexed docstring that happens to quote a local path carries
/// the same information and there is no schema position to look it up at.
/// One pass over the JSON text catches both.
///
/// Replacing raw text inside JSON is safe here specifically because these
/// are filesystem paths: on the platforms `ug` builds for they contain no
/// character `serde_json` escapes, so the bytes in the document are the
/// bytes being searched for. The repo root goes first — it is the longer,
/// more specific prefix, and scrubbing the home directory out from under it
/// would leave the rest of the layout behind.
fn scrub_paths(graph: &str, repo_root: &str, repo_label: &str) -> String {
    let mut out = graph.replace(repo_root, repo_label);
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().into_owned();
        // Guard against a degenerate home (`/`), which would rewrite every
        // path separator in the document.
        if home.len() > 1 {
            out = out.replace(&home, "~");
        }
    }
    out
}

/// Point `stats.repoRoot` at something a stranger can read.
///
/// The UI shows this verbatim as the "Repo" row and derives the catalog's
/// repo label from its last path segment, so it cannot simply be dropped.
/// A source URL is the most useful thing to put there — the last segment of
/// `https://github.com/owner/repo` is still `repo`, so the label keeps
/// working — and the bare repo name is the fallback.
fn retarget_repo_root(graph: &mut serde_json::Value, public_root: &str) {
    if let Some(stats) = graph.get_mut("stats").and_then(|s| s.as_object_mut()) {
        stats.insert(
            "repoRoot".to_string(),
            serde_json::Value::String(public_root.to_string()),
        );
    }
}

/// Build the demo's copy of the visualization page.
///
/// Three edits to the page `ug serve` ships, and no more — every behavioural
/// difference lives in the shim, so this stays a wrapper rather than a fork:
///
/// 1. The manifest + shim, injected as a classic `<script>` in `<head>`.
///    Classic scripts run at parse time and the app is a deferred module, so
///    the shim's `fetch` patch is installed before any app code runs. That
///    ordering is the entire reason the demo needs no conditionals in
///    `src/vis/js/`.
/// 2. `/favicon.svg` → `favicon.svg`, so the folder resolves standalone
///    instead of only at a site root.
/// 3. The `<title>`, which is what a shared link shows.
fn build_demo_page(manifest: &serde_json::Value, label: &str) -> String {
    let page = crate::assets::VIS_HTML;

    // `</` inside the manifest would close the script block early and
    // truncate the page with no error in the browser — the same hazard
    // build.rs guards for the JS parts. `\/` is an escape both JSON and JS
    // string literals read as a plain slash.
    let manifest_js = serde_json::to_string(manifest)
        .unwrap_or_else(|e| die(1, format!("cannot serialize the demo manifest: {e}")))
        .replace("</", "<\\/");

    if crate::assets::VIS_DEMO_SHIM.to_lowercase().contains("</script") {
        die(
            1,
            "src/vis/demo-shim.js contains a literal closing script tag, which would \
             truncate the demo page with no error in the browser. Split the literal.",
        );
    }

    let block = format!(
        "<script>\nwindow.UG_DEMO = {manifest_js};\n{}\n</script>\n</head>",
        crate::assets::VIS_DEMO_SHIM
    );

    if page.matches("</head>").count() != 1 {
        die(
            1,
            "the assembled visualization page needs exactly one </head> for the demo \
             shim to be injected before it",
        );
    }
    let page = page.replace("</head>", &block);

    // Both are single occurrences in `src/vis/index.html`; a miss means the
    // skeleton changed, and silently publishing a page that points at a
    // favicon which is not there is worse than saying so.
    let page = page.replace(r#"href="/favicon.svg""#, r#"href="favicon.svg""#);
    page.replace(
        "<title>Knowledge Graph visualization | UltraGraph</title>",
        &format!("<title>{} · ug live demo</title>", html_escape(label)),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// One counter out of the graph's `stats` block, or 0 if it isn't there.
fn stat_u64(graph: &serde_json::Value, key: &str) -> u64 {
    graph
        .get("stats")
        .and_then(|s| s.get(key))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `--page-only`: rebuild the published page from this binary, leaving the
/// graph snapshot alone.
///
/// This exists so the staleness guard is *affordable*. A vis edit invalidates
/// the page and nothing else — the snapshot is a point in time and is
/// supposed to be older than the code — but a full re-publish also rewrites
/// `graph.json`, and `stats.lastIndexedAt` moves on every run, so 2.7 MB of
/// git churn would land on every CSS tweak. A guard that expensive gets
/// deleted rather than obeyed. This path writes ~910 KB and touches nothing
/// else, so keeping the demo current costs a second and a small diff.
fn run_page_refresh(output_dir: &str, args: &[String]) {
    let manifest_path = format!("{output_dir}/demo.json");
    let existing = fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        die(
            1,
            format!(
                "cannot read {manifest_path}: {e}\n       \
                 --page-only refreshes an existing demo. Publish one first:\n       \
                 ug demo -i <repo> -o {output_dir}"
            ),
        )
    });
    let mut manifest: serde_json::Value = serde_json::from_str(&existing)
        .unwrap_or_else(|e| die(1, format!("{manifest_path} is not valid JSON: {e}")));

    // Everything about *the repo* is carried over untouched — it describes
    // the snapshot on disk, which this command is deliberately not rebuilding.
    // Only what the page is built from moves.
    let obj = manifest
        .as_object_mut()
        .unwrap_or_else(|| die(1, format!("{manifest_path} is not a JSON object")));
    for (key, value) in [
        ("ugVersion", serde_json::json!(env!("CARGO_PKG_VERSION"))),
        ("visFingerprint", serde_json::json!(vis_fingerprint())),
        ("generatedAt", serde_json::json!(now_secs())),
    ] {
        obj.insert(key.to_string(), value);
    }
    // Flags still win when given, so a link target can be corrected without
    // a re-index.
    if let Some(label) = flag_value(args, &["--label"]) {
        obj.insert("label".into(), serde_json::json!(label));
    }
    if let Some(site) = flag_value(args, &["--site"]) {
        obj.insert(
            "install".into(),
            serde_json::json!(format!("{}#get-started", site.trim_end_matches('#'))),
        );
    }

    let label = manifest
        .get("label")
        .and_then(|l| l.as_str())
        .unwrap_or("this repository")
        .to_string();
    let page = build_demo_page(&manifest, &label);

    let write = |name: &str, bytes: &[u8]| {
        let path = format!("{output_dir}/{name}");
        fs::write(&path, bytes).unwrap_or_else(|e| die(1, format!("failed to write {path}: {e}")));
        bytes.len() as u64
    };
    let html = write("index.html", page.as_bytes());
    write("threejs-vis.bundle.js", crate::assets::VIS_THREEJS_BUNDLE);
    write("cosmos-vis.bundle.js", crate::assets::VIS_COSMOS_BUNDLE);
    write("favicon.svg", crate::assets::VIS_FAVICON);
    write("README.md", readme(&label).as_bytes());
    write(
        "demo.json",
        serde_json::to_string_pretty(&manifest)
            .unwrap_or_default()
            .as_bytes(),
    );

    println!(
        "{C_GREEN}✓ Refreshed the page{C_RESET} in {C_BOLD}{}{C_RESET} {C_DIM}({}){C_RESET}",
        output_dir,
        human_bytes(html)
    );
    println!(
        "  {C_DIM}graph.json left as it was — the snapshot is unchanged, only the visualization.{C_RESET}"
    );
}

pub(crate) fn run_demo(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_demo_help();
        return;
    }

    let output_dir_early = flag_value(args, &["-o", "--output"])
        .unwrap_or_else(|| default_output().to_string());
    if has_flag(args, "--page-only") {
        run_page_refresh(&output_dir_early, args);
        return;
    }

    let start = std::time::Instant::now();

    let input = flag_value(args, &["-i", "--input"])
        .or_else(|| {
            first_positional(
                args,
                &["-i", "--input", "-o", "--output", "--label", "--source-url", "--site"],
            )
        })
        .unwrap_or_else(|| ".".to_string());
    let output_dir = output_dir_early;

    // Canonicalized once, so the banner, the scrub and the derived label all
    // name the same resolved tree (Agents.md §9a).
    let repo_root = fs::canonicalize(&input)
        .unwrap_or_else(|e| die(1, format!("cannot resolve {input}: {e}")))
        .to_string_lossy()
        .into_owned();
    let repo_name = basename(&repo_root);
    let label = flag_value(args, &["--label"]).unwrap_or_else(|| repo_name.clone());
    let source_url = flag_value(args, &["--source-url"]);
    // The public stand-in for the repo root. A URL if the caller gave one —
    // it is strictly more useful to a reader than a bare name and still ends
    // in the repo's name, which is what the catalog label derives from.
    let public_root = source_url.clone().unwrap_or_else(|| repo_name.clone());
    // Where "Install ug" goes. Site-relative by default so a demo published
    // under the website keeps the visitor on whichever host they arrived on
    // — a preview channel, a local `python3 -m http.server`, or production.
    let site = flag_value(args, &["--site"]).unwrap_or_else(|| "/".to_string());
    let install_url = format!("{}#get-started", site.trim_end_matches('#'));

    println!(
        "⚡ Static demo: {C_BOLD}index → graph → publishable folder{C_RESET} {C_DIM}(no db, no vectors){C_RESET}"
    );
    println!("{C_CYAN}▸{C_RESET} Source  {C_YELLOW}{}{C_RESET}", repo_root);
    println!("{C_CYAN}▸{C_RESET} Output  {C_YELLOW}{}{C_RESET}", output_dir);

    let t0 = std::time::Instant::now();
    println!("{C_CYAN}▸{C_RESET} Indexing");
    let index_result = index(input.clone());
    let graph = build_graph(index_result);
    println!(
        "  {C_GREEN}✓ done{C_RESET} in {C_BOLD}{:?}{C_RESET}",
        t0.elapsed()
    );

    let mut parsed: serde_json::Value = serde_json::from_str(&graph)
        .unwrap_or_else(|e| die(1, format!("the graph this build produced is not valid JSON: {e}")));
    retarget_repo_root(&mut parsed, &public_root);
    let graph = serde_json::to_string(&parsed)
        .unwrap_or_else(|e| die(1, format!("cannot re-serialize the graph: {e}")));
    let graph = scrub_paths(&graph, &repo_root, &repo_name);

    let nodes = parsed.get("nodes").and_then(|n| n.as_array()).map_or(0, Vec::len);
    let edges = parsed.get("edges").and_then(|e| e.as_array()).map_or(0, Vec::len);
    if nodes == 0 {
        die(
            1,
            format!("indexing {repo_root} produced no nodes — nothing to publish"),
        );
    }

    let manifest = serde_json::json!({
        "label": label,
        "project": repo_name,
        "install": install_url,
        "source": public_root,
        "nodes": nodes,
        "edges": edges,
        // Carried so the landing page can state the demo's size without
        // downloading the 2.7 MB graph to count it — and, more to the point,
        // without anyone hand-copying four numbers that change on every
        // re-publish. See `.demo-facts` in docs/ug-website/index.html.
        "files": stat_u64(&parsed, "totalFiles"),
        "lines": stat_u64(&parsed, "totalLines"),
        "ugVersion": env!("CARGO_PKG_VERSION"),
        "visFingerprint": vis_fingerprint(),
        "generatedAt": now_secs(),
    });

    fs::create_dir_all(&output_dir)
        .unwrap_or_else(|e| die(1, format!("cannot create {output_dir}: {e}")));

    let write = |name: &str, bytes: &[u8]| -> u64 {
        let path = format!("{output_dir}/{name}");
        fs::write(&path, bytes).unwrap_or_else(|e| die(1, format!("failed to write {path}: {e}")));
        bytes.len() as u64
    };

    let page = build_demo_page(&manifest, &label);
    let mut total = 0u64;
    let sizes = [
        ("graph.json", write("graph.json", graph.as_bytes())),
        ("index.html", write("index.html", page.as_bytes())),
        (
            "threejs-vis.bundle.js",
            write("threejs-vis.bundle.js", crate::assets::VIS_THREEJS_BUNDLE),
        ),
        (
            "cosmos-vis.bundle.js",
            write("cosmos-vis.bundle.js", crate::assets::VIS_COSMOS_BUNDLE),
        ),
        (
            "favicon.svg",
            write("favicon.svg", crate::assets::VIS_FAVICON),
        ),
        (
            "demo.json",
            write(
                "demo.json",
                serde_json::to_string_pretty(&manifest)
                    .unwrap_or_default()
                    .as_bytes(),
            ),
        ),
        ("README.md", write("README.md", readme(&label).as_bytes())),
    ];

    println!("{C_BOLD}────────────────────────────────────────{C_RESET}");
    println!(
        "{C_GREEN}✓ Published{C_RESET} {C_BOLD}{}{C_RESET} — {} nodes, {} edges",
        label, nodes, edges
    );
    for (name, size) in sizes {
        total += size;
        println!(
            "  {C_GREEN}✓{C_RESET} {:<18}{C_DIM}{}{C_RESET}",
            name,
            human_bytes(size)
        );
    }
    println!("  {C_DIM}{} total{C_RESET}", human_bytes(total));

    // Not an error — solo mode is the right call at this size, and the page
    // offers a search box and the most-connected nodes to start from. But a
    // demo that opens on an empty canvas is a different demo from one that
    // opens on a graph, and that difference should never arrive silently
    // just because the indexed tree grew since the last publish.
    if nodes.max(edges) > SOLO_THRESHOLD {
        println!();
        println!(
            "{C_YELLOW}⚠{C_RESET}  {} elements is past the renderer's {} limit — the demo will open in",
            nodes.max(edges),
            SOLO_THRESHOLD
        );
        println!(
            "   {C_BOLD}solo mode{C_RESET}: an empty canvas asking the visitor to pick a node, rather than"
        );
        println!("   the whole graph. Publish a subtree instead if the first look matters:");
        println!("   {C_CYAN}ug demo -i <repo>/<subdir>{C_RESET}");
    }

    println!();
    // Served from the *parent*, and on a named port. Both matter:
    //   • the "Install ug" links are site-root-relative, so serving the demo
    //     folder itself points them at the demo rather than the landing page;
    //   • bare `http.server` binds 8000, which is the port every other local
    //     tool also reaches for — a neighbour's polling then fills the log
    //     with 404s that look like the demo failing, and are not.
    let parent = Path::new(&output_dir)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let leaf = basename(&output_dir);
    println!(
        "  {C_DIM}Preview:{C_RESET} {C_CYAN}cd {parent} && python3 -m http.server 8081 --bind 127.0.0.1{C_RESET}"
    );
    println!("           {C_DIM}then open{C_RESET} {C_CYAN}http://localhost:8081/{leaf}/{C_RESET}");
    println!("Total time: {C_BOLD}{:?}{C_RESET}", start.elapsed());
}

/// The note left in the published folder. Firebase's `ignore` list drops
/// `*.md`, so this never reaches the live site — it is there for whoever
/// opens the directory in the repo and wonders whether it is hand-written.
fn readme(label: &str) -> String {
    format!(
        "# UltraGraph demo — {label}\n\
         \n\
         **Generated. Do not edit by hand.** Every file here is rewritten by:\n\
         \n\
         ```bash\n\
         ug demo -i <repo> -o <this directory>\n\
         ```\n\
         \n\
         `index.html` is the same visualization page `ug serve` serves, wrapped in\n\
         a static stand-in for the server (`native/src/vis/demo-shim.js`). It reads\n\
         `graph.json` from this directory and needs no backend. Everything that\n\
         does need one — semantic search, chat, guided tours, statistics, source\n\
         preview — is off, and the page says so where a visitor would look.\n\
         \n\
         `demo.json` is the same manifest the page is built with: label, counts,\n\
         `ug` version, generation time.\n"
    )
}

fn print_demo_help() {
    println!();
    println!("{C_BOLD}ug demo{C_RESET} — publish an indexed repo as a static web demo");
    println!();
    println!("Indexes a repo and writes a folder any static host can serve: the graph,");
    println!("the visualization page, and nothing else. No database, no vectors, no");
    println!("server. Built for {C_CYAN}docs/ug-website/demo/{C_RESET}, which Firebase deploys with the");
    println!("rest of the site.");
    println!();
    println!("{C_BOLD}Usage{C_RESET}");
    println!("  {C_CYAN}ug demo{C_RESET} [<path>] [options]");
    println!();
    println!("{C_BOLD}Options{C_RESET}");
    println!("  {C_CYAN}-i, --input <path>{C_RESET}     Repo to index. Default: the current directory");
    println!("  {C_CYAN}-o, --output <dir>{C_RESET}     Where to publish. Default: docs/ug-website/demo when");
    println!("                         that website folder exists here, else ./ug-demo");
    println!("  {C_CYAN}--label <text>{C_RESET}         Name shown in the page. Default: the repo's directory name");
    println!("  {C_CYAN}--source-url <url>{C_RESET}     Public URL of the repo, shown in place of the local path");
    println!("  {C_CYAN}--site <url>{C_RESET}           Base URL the \"Install ug\" links point at. Default: /");
    println!("  {C_CYAN}--page-only{C_RESET}            Rebuild the page from this binary and leave graph.json");
    println!("                         alone. What to run after editing {C_CYAN}native/src/vis/{C_RESET} —");
    println!("                         the published demo embeds that page and goes stale.");
    println!();
    println!("{C_BOLD}Written{C_RESET}");
    println!("  graph.json          the indexed graph — what the page draws");
    println!("  index.html          the visualization, wrapped for static hosting");
    println!("  threejs-vis.bundle.js    the 3D renderer (three.js)");
    println!("  cosmos-vis.bundle.js  the 2D renderer (cosmos.gl)");
    println!("  favicon.svg  demo.json  README.md");
    println!();
    println!("{C_BOLD}Not included{C_RESET}  {C_DIM}— these need the local index and cannot be published{C_RESET}");
    println!("  Semantic and hybrid search · chat · guided tours · statistics and GQL");
    println!("  · source preview. Keyword search, filters, focus and walk all work.");
    println!();
    println!("{C_BOLD}Privacy{C_RESET}");
    println!("  A published graph contains file paths, symbol names and docstrings from");
    println!("  the repo. Local absolute paths are rewritten out, but nothing else is —");
    println!("  {C_YELLOW}only run this against code you are willing to publish.{C_RESET}");
    println!();
    println!("{C_BOLD}Examples{C_RESET}");
    println!("  {C_CYAN}ug demo{C_RESET}");
    println!("  {C_CYAN}ug demo -i ~/code/myrepo --label 'MyRepo' --source-url https://github.com/me/myrepo{C_RESET}");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The leak this command exists to not have: a published graph naming
    /// the author's home directory, both in the field that is meant to hold
    /// a path and in a docstring that merely quotes one.
    #[test]
    fn publishing_scrubs_local_paths_wherever_they_appear() {
        let root = "/home/someone/code/myrepo";
        let graph = format!(
            r#"{{"nodes":[{{"docstring":"see {root}/src/lib.rs"}}],"stats":{{"repoRoot":"{root}"}}}}"#
        );
        let scrubbed = scrub_paths(&graph, root, "myrepo");
        assert!(!scrubbed.contains(root), "repo root survived: {scrubbed}");
        assert!(scrubbed.contains("myrepo/src/lib.rs"), "{scrubbed}");
    }

    /// `stats.repoRoot` is rendered verbatim in the UI, so it has to be
    /// replaced rather than removed — and the catalog derives its repo label
    /// from the last path segment, which a URL still supplies.
    #[test]
    fn the_repo_root_is_retargeted_at_something_public() {
        let mut g: serde_json::Value =
            serde_json::from_str(r#"{"stats":{"repoRoot":"/home/someone/code/myrepo"}}"#).unwrap();
        retarget_repo_root(&mut g, "https://github.com/me/myrepo");
        assert_eq!(g["stats"]["repoRoot"], "https://github.com/me/myrepo");
    }

    /// A graph with no `stats` block must not gain one, and must not panic.
    #[test]
    fn retargeting_a_graph_without_stats_is_a_no_op() {
        let mut g: serde_json::Value = serde_json::from_str(r#"{"nodes":[]}"#).unwrap();
        retarget_repo_root(&mut g, "myrepo");
        assert!(g.get("stats").is_none());
    }

    /// The shim has to be installed before the app module runs, and the page
    /// has to keep the shape the browser needs: one style block, one module
    /// script, plus exactly the one classic script this adds.
    #[test]
    fn the_demo_page_carries_the_shim_ahead_of_the_app() {
        let manifest = serde_json::json!({ "label": "x", "nodes": 1, "edges": 0 });
        let page = build_demo_page(&manifest, "MyRepo");

        let shim_at = page.find("window.UG_DEMO =").expect("manifest injected");
        let app_at = page.find(r#"<script type="module">"#).expect("app module");
        assert!(shim_at < app_at, "the shim must parse before the app module");

        assert_eq!(page.matches("</script>").count(), 2, "app module + shim");
        assert_eq!(page.matches("</style>").count(), 1, "one style block");
        assert!(page.contains(r#"href="favicon.svg""#), "favicon made relative");
        assert!(!page.contains(r#"href="/favicon.svg""#), "absolute favicon left behind");
        assert!(page.contains("<title>MyRepo · ug live demo</title>"));
    }

    /// The staleness guard is only as good as this: a change to *any* input
    /// has to move the fingerprint, including one that merely shifts bytes
    /// across the boundary between two of them.
    #[test]
    fn the_fingerprint_reacts_to_each_of_its_inputs() {
        let fp = |parts: &[&str]| {
            fingerprint_of(&parts.iter().map(|p| p.as_bytes()).collect::<Vec<_>>())
        };
        let base = fp(&["<html>page</html>", "shim();", "three();", "cosmos();"]);
        for (i, label) in ["page", "shim", "three bundle", "cosmos bundle"].iter().enumerate() {
            let mut parts = vec!["<html>page</html>", "shim();", "three();", "cosmos();"];
            parts[i] = "changed";
            assert_ne!(base, fp(&parts), "{label} edit must move the fingerprint");
        }
        // Concatenating without a separator would hash these two identically.
        assert_ne!(
            fp(&["ab", "c"]),
            fp(&["a", "bc"]),
            "the boundary between parts must be part of the hash"
        );
        assert_eq!(base, fp(&["<html>page</html>", "shim();", "three();", "cosmos();"]), "stable");
    }

    /// **The live demo ships a copy of the visualization page, and a copy
    /// cannot notice its original changed.**
    ///
    /// This is the one failure in this area with no symptom: edit anything
    /// under `src/vis/`, and `cargo build`, every other test, and `ug serve`
    /// all keep working perfectly — while `https://ultra-graph.web.app/demo/`
    /// quietly keeps serving the page from whenever it was last published.
    /// Nothing points back at the edit, because the edit was fine. So the
    /// check has to live where a change to `src/vis/` is already going to
    /// run (`cargo nextest run`, per Agents.md §6), and it has to name the
    /// fix rather than just the fact.
    ///
    /// Fixing it is cheap on purpose — see `run_page_refresh`.
    #[test]
    fn the_published_demo_page_is_not_stale() {
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/ug-website/demo/demo.json");

        let Ok(body) = std::fs::read_to_string(&manifest_path) else {
            // A checkout without the demo is a checkout that cannot publish
            // it, which is worth saying — but it is not a reason to fail a
            // test run for someone who never touched the vis layer.
            eprintln!(
                "note: {} is missing — the live demo has not been published in this \
                 checkout. `./scripts/gen-demo.sh` creates it.",
                manifest_path.display()
            );
            return;
        };

        let manifest: serde_json::Value =
            serde_json::from_str(&body).expect("demo.json is valid JSON");
        let published = manifest
            .get("visFingerprint")
            .and_then(|f| f.as_str())
            .unwrap_or("<none>");

        assert_eq!(
            published,
            vis_fingerprint(),
            "\n\n\
             The live demo at docs/ug-website/demo/ was built from a different \
             visualization page than this one.\n\
             Something under native/src/vis/ changed since it was last published, so \
             the demo would ship the old page.\n\n\
             Refresh just the page (fast, leaves graph.json alone):\n    \
             cargo run --bin ug -- demo --page-only\n\n\
             Or re-publish the whole snapshot, when the graph itself should move too:\n    \
             ./scripts/gen-demo.sh\n"
        );
    }

    /// The publish-time warning is only worth anything if it fires at the
    /// count the renderer actually switches on. `16-solo-view.js` owns that
    /// number; this fails the moment the two stop agreeing.
    #[test]
    fn the_solo_threshold_matches_the_renderer() {
        let src = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/vis/js/16-solo-view.js"),
        )
        .expect("the solo-view part exists");
        let decl = src
            .split("const SOLO_THRESHOLD")
            .nth(1)
            .expect("16-solo-view.js declares SOLO_THRESHOLD");
        let value: usize = decl
            .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .expect("SOLO_THRESHOLD is a plain integer literal");
        assert_eq!(
            value, SOLO_THRESHOLD,
            "src/vis/js/16-solo-view.js moved SOLO_THRESHOLD to {value}; update the \
             constant in this module so `ug demo` still warns at the right size"
        );
    }

    /// A label carrying markup must not be able to close the title or open a
    /// tag of its own — it comes from `--label`, i.e. from outside.
    #[test]
    fn a_label_cannot_inject_markup() {
        let manifest = serde_json::json!({ "label": "x" });
        let page = build_demo_page(&manifest, "<img src=x onerror=alert(1)>");
        assert!(!page.contains("<img src=x"), "label was not escaped");
        assert!(page.contains("&lt;img src=x"));
    }

    /// The output default is a convenience, not a guess: it only points into
    /// the website when the website is actually there.
    #[test]
    fn the_default_output_follows_whether_a_website_folder_exists() {
        // Asserted against the repo's own layout, which is where the useful
        // branch is taken; the other is the literal fallback.
        let cwd = std::env::current_dir().unwrap();
        let expected = if cwd.join("docs/ug-website").is_dir() {
            "docs/ug-website/demo"
        } else {
            "ug-demo"
        };
        assert_eq!(default_output(), expected);
    }
}
