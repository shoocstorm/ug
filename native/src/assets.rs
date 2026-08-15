//! Bundled visualization assets, so `ug gen` can produce a self-contained
//! output directory — and `ug serve` can serve the UI — without needing
//! the source tree at runtime.
//!
//! The page is assembled by `build.rs` from `src/vis/index.html` +
//! `src/vis/css/*` + `src/vis/js/*` — edit those, not the output. It lands
//! in OUT_DIR rather than the source tree precisely so there is no
//! generated copy sitting around to edit by mistake.

/// The assembled visualization page.
///
/// Two things ship this: `ug serve`, which serves it straight from the
/// binary, and `ug demo`, which writes a *copy* into
/// `docs/ug-website/demo/`. The copy is the one that can go quietly wrong —
/// it has no way to notice this changed. `cli::demo::vis_fingerprint`
/// stamps a hash of this constant into the published `demo.json` so
/// `the_published_demo_page_is_not_stale` can catch it; `ug demo
/// --page-only` is the fix.
pub(crate) const VIS_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/visualization.html"));
pub(crate) const VIS_BUNDLE: &[u8] = include_bytes!("./vis/ug-vis.bundle.js");
pub(crate) const VIS_FAVICON: &[u8] = include_bytes!("./vis/favicon.svg");
pub(crate) const VIS_MD: &str = include_str!("../../README.md");

/// The static-demo wrapper `ug demo` injects into its copy of the page.
///
/// Deliberately *not* under `src/vis/js/` — build.rs concatenates that
/// directory into every build of the page, and the demo shim must ship only
/// in a published snapshot. See the file's own header for what it does.
pub(crate) const VIS_DEMO_SHIM: &str = include_str!("./vis/demo-shim.js");
