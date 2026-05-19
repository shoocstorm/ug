//! PDF document indexer.
//!
//! Unlike the language modules under `indexer/languages/`, PDFs are
//! **binary** — they don't fit the tree-sitter pipeline (parse UTF-8
//! source → walk AST). We instead extract text per page using
//! [`pdf-extract`][1] (the most widely-used pure-Rust PDF text
//! extractor, built on `lopdf`) and emit one `Symbol` per page so each
//! page becomes its own indexable, searchable unit.
//!
//! ## Symbol model
//! - **One symbol per page**, `kind: "heading_1"`. Reusing the markdown
//!   heading kind means the existing graph layer turns each page into a
//!   `Concept` node and links it back to the parent `File` via a
//!   `Contains` edge — no special-case code in `graph.rs`.
//! - `name`: the first non-empty line of the page (truncated), falling
//!   back to `"Page N"`. Gives more useful UI labels than every node
//!   being literally `Page 1`, `Page 2`, …
//! - `docstring`: the page's full extracted text, capped at
//!   [`PAGE_TEXT_CAP`] bytes so a 50-page brochure doesn't blow the
//!   embedder's context window or the JSON payload.
//! - `start_line` / `end_line`: the page number (PDFs are not
//!   line-oriented; we repurpose the field as a page index).
//!
//! ## Extraction quality
//! `pdf-extract` is text-only — it does **not** recover document
//! structure (headings, lists, tables) the way pdfium or mupdf might.
//! That's a deliberate trade-off: pure-Rust, no native lib download,
//! reasonable quality for typical text PDFs. If higher fidelity is
//! needed later (font-size-based heading detection, table cells,
//! layout-aware paragraph splitting) `pdfium-render` is the upgrade
//! path; see `docs/MULTI-DEST-PLAN.md`-style follow-up.
//!
//! [1]: https://crates.io/crates/pdf-extract

use crate::indexer::common::{normalize_path, strip_repo_root};
use crate::types::{FileNode, Symbol};
use std::path::Path;

/// Per-page byte cap on the extracted text we keep in `docstring`. Set
/// generously enough for full-page prose, low enough that a 100-page PDF
/// can't dominate the embedder's per-batch token budget. Long pages get
/// truncated with a trailing `…`.
const PAGE_TEXT_CAP: usize = 8_192;

/// Hard cap on how many bytes of page text we use as the symbol `name`.
/// The first line of the page is usually short (heading / title), but
/// we cap defensively for the worst-case "100-character keyword
/// stuffing" first line.
const NAME_CAP: usize = 100;

/// Extract every page of `path` as a [`Symbol`]. Returns the wrapping
/// [`FileNode`] with `language = "pdf"`.
///
/// Errors short-circuit to `None` rather than propagating because the
/// indexer's contract is "skip files we can't parse"; the caller logs
/// the path that was skipped via the usual file-walker counters.
pub fn process_pdf(path: &Path, repo_root: Option<&str>) -> Option<FileNode> {
    let bytes = std::fs::read(path).ok()?;
    let hash = blake3::hash(&bytes).to_hex().to_string();

    // `extract_text_by_pages` is the one-shot per-page API. Errors
    // here are typically encrypted PDFs (which we don't handle) or
    // corrupt files — both are non-fatal at the index layer.
    let pages = match pdf_extract::extract_text_by_pages(path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "pdf-extract failed; skipping");
            return None;
        }
    };

    let path_str = normalize_path(&path.to_string_lossy());
    let path_str = match repo_root {
        Some(root) => strip_repo_root(&path_str, root),
        None => path_str,
    };

    let total_pages = pages.len() as u32;
    let mut symbols: Vec<Symbol> = Vec::with_capacity(pages.len());
    for (idx, raw) in pages.into_iter().enumerate() {
        let page_no = (idx + 1) as u32;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // Pure-image pages or scanned PDFs without OCR show up as
            // empty. Emit a stub symbol so the file's structure stays
            // visible in the UI — but no `docstring` so the embedder
            // doesn't waste budget on whitespace.
            symbols.push(page_symbol(
                page_no,
                format!("Page {} (no text)", page_no),
                None,
            ));
            continue;
        }
        let name = derive_page_name(trimmed, page_no);
        let docstring = truncate(trimmed, PAGE_TEXT_CAP);
        symbols.push(page_symbol(page_no, name, Some(docstring)));
    }

    // Stamp the file path on every symbol — mirrors what
    // `indexer::process_file` does for tree-sitter languages.
    for sym in symbols.iter_mut() {
        sym.file = path_str.clone();
    }

    let classification =
        crate::indexer::classifier::classify_file(&path_str, &symbols);

    Some(FileNode {
        path: path_str,
        hash,
        language: "pdf".to_string(),
        classification,
        symbols,
        // Repurpose `lines` as page count so the UI's per-file
        // "N lines" badge becomes "N pages" for PDFs.
        lines: total_pages,
        imports: Vec::new(),
        exports: Vec::new(),
    })
}

/// Build one `Symbol` for a PDF page. `start_line == end_line == page_no`
/// so downstream UI controls that key off line ranges (snippet readers,
/// scroll-to-line buttons) still get a stable number, even though the
/// underlying file is binary and `read_snippet` will silently no-op.
fn page_symbol(page_no: u32, name: String, docstring: Option<String>) -> Symbol {
    Symbol {
        id: format!("pdf_page:{}", page_no),
        name,
        kind: "heading_1".to_string(),
        file: String::new(),
        start_line: page_no,
        end_line: page_no,
        docstring,
        signature: None,
        imports: Vec::new(),
        exports: Vec::new(),
        extends: Vec::new(),
        implements: Vec::new(),
        calls: Vec::new(),
        metrics: None,
    }
}

/// Pick a human-friendly name for a page. We grab the first non-empty
/// line — usually the heading or first sentence — and fall back to
/// `Page N` when nothing meaningful is available. Always prefixed with
/// the page number so the UI still shows ordering.
fn derive_page_name(text: &str, page_no: u32) -> String {
    let first_line = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    if first_line.is_empty() {
        return format!("Page {}", page_no);
    }
    let snippet = truncate(&first_line, NAME_CAP);
    format!("p.{} · {}", page_no, snippet)
}

/// Truncate `s` to at most `cap` bytes on a char boundary, appending
/// `…` when truncation actually happened. Char-boundary-aware so we
/// never split a UTF-8 sequence — PDF text often contains ligatures
/// and accented characters that span multiple bytes.
fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        // "héllo" — é is two bytes; truncating to 2 must back up.
        let s = "héllo";
        assert_eq!(truncate(s, 100), "héllo");
        // cap=2 lands inside the é; the function should back up to
        // byte 1 (before é) and append the ellipsis.
        let out = truncate(s, 2);
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn derive_name_falls_back_when_empty() {
        assert_eq!(derive_page_name("   \n\n  ", 4), "Page 4");
    }

    #[test]
    fn derive_name_uses_first_nonblank_line() {
        let text = "\n\nIntroduction\nSecond line should not appear";
        let name = derive_page_name(text, 7);
        assert!(name.starts_with("p.7 · Introduction"));
        assert!(!name.contains("Second line"));
    }
}
