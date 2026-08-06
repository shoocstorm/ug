//! Bundled visualization assets, so `ug gen` can produce a self-contained
//! output directory — and `ug serve` can serve the UI — without needing
//! the source tree at runtime.
//!
//! The page is assembled by `build.rs` from `src/vis/index.html` +
//! `src/vis/css/*` + `src/vis/js/*` — edit those, not the output. It lands
//! in OUT_DIR rather than the source tree precisely so there is no
//! generated copy sitting around to edit by mistake.

pub(crate) const VIS_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/visualization.html"));
pub(crate) const VIS_BUNDLE: &[u8] = include_bytes!("./vis/ug-vis.bundle.js");
pub(crate) const VIS_FAVICON: &[u8] = include_bytes!("./vis/favicon.svg");
pub(crate) const VIS_MD: &str = include_str!("../../README.md");
