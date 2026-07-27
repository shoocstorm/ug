//! Document indexer for binary formats: PDF plus Word/Excel/PowerPoint (and
//! their OpenDocument/legacy variants).
//!
//! Unlike the language modules under `indexer/languages/`, these are
//! **binary** — they don't fit the tree-sitter pipeline (parse UTF-8 source
//! → walk AST). We use [`liteparse`][1] instead: PDFs are read directly via
//! its bundled PDFium backend; everything else is first converted to PDF by
//! shelling out to a local LibreOffice install (`soffice`), then parsed the
//! same way. Office support is therefore best-effort — hosts without
//! LibreOffice on `PATH` simply have those files skipped, same as a
//! corrupt/encrypted PDF.
//!
//! ## Symbol model
//! - **One symbol per page**, `kind: "heading_1"`. Reusing the markdown
//!   heading kind means the existing graph layer turns each page into a
//!   `Concept` node and links it back to the parent `File` via a `Contains`
//!   edge — no special-case code in `graph.rs`.
//! - `name`: the first non-empty line of the page (truncated), falling back
//!   to `"Page N"`. Gives more useful UI labels than every node being
//!   literally `Page 1`, `Page 2`, …
//! - `docstring`: the page's full extracted text, capped at
//!   [`PAGE_TEXT_CAP`] bytes so a 50-page brochure doesn't blow the
//!   embedder's context window or the JSON payload.
//! - `start_line` / `end_line`: the page number (these formats are not
//!   line-oriented; we repurpose the field as a page index).
//!
//! [1]: https://github.com/run-llama/liteparse

use crate::indexer::common::{normalize_path, strip_repo_root, truncate_chars};
use crate::types::{FileNode, Symbol};
use liteparse::config::ImageMode;
use liteparse::{LiteParse, LiteParseConfig, OutputFormat};
use std::path::Path;
use std::sync::OnceLock;

/// Per-page byte cap on the extracted text we keep in `docstring`. Set
/// generously enough for full-page prose, low enough that a 100-page
/// document can't dominate the embedder's per-batch token budget. Long
/// pages get truncated with a trailing `…`.
pub(crate) const PAGE_TEXT_CAP: usize = 8_192;

/// Hard cap on how many bytes of page text we use as the symbol `name`.
/// The first line of the page is usually short (heading / title), but we
/// cap defensively for the worst-case "100-character keyword stuffing"
/// first line.
pub(crate) const NAME_CAP: usize = 100;

/// Word-processor extensions liteparse converts to PDF via LibreOffice
/// before parsing.
const WORD_EXTS: &[&str] = &["doc", "docx", "docm", "dot", "dotm", "dotx", "odt", "ott", "rtf"];
/// Spreadsheet extensions.
const EXCEL_EXTS: &[&str] = &["xls", "xlsx", "xlsm", "xlsb", "ods", "ots"];
/// Presentation extensions.
const POWERPOINT_EXTS: &[&str] = &["ppt", "pptx", "pptm", "pot", "potm", "potx", "odp", "otp"];

/// All extensions this module handles, including `pdf`. Kept in sync with
/// `common::SUPPORTED_EXTS` — see the note there.
pub fn is_supported_ext(ext: &str) -> bool {
    ext == "pdf"
        || WORD_EXTS.contains(&ext)
        || EXCEL_EXTS.contains(&ext)
        || POWERPOINT_EXTS.contains(&ext)
}

/// Human-readable language tag for a supported extension, used as
/// `FileNode.language` and in classification/UI badges.
fn language_for(ext: &str) -> &'static str {
    if ext == "pdf" {
        "pdf"
    } else if WORD_EXTS.contains(&ext) {
        "word"
    } else if EXCEL_EXTS.contains(&ext) {
        "excel"
    } else if POWERPOINT_EXTS.contains(&ext) {
        "powerpoint"
    } else {
        "document"
    }
}

/// Lazily-built multi-thread tokio runtime shared by every call into
/// liteparse's async API. `process_file`'s caller loop is plain sync code
/// (no ambient tokio context), and building a fresh runtime per file would
/// spawn a new worker pool for every document — so this is built once per
/// process and reused.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for document indexing")
    })
}

/// Extract every page of `path` as a [`Symbol`]. Returns the wrapping
/// [`FileNode`] with `language` set per [`language_for`].
///
/// Errors (parse failure, missing LibreOffice for office formats, encrypted
/// PDFs, …) short-circuit to `None` rather than propagating — the indexer's
/// contract is "skip files we can't parse"; the caller logs the path that
/// was skipped via the usual file-walker counters.
pub fn process_document(path: &Path, repo_root: Option<&str>) -> Option<FileNode> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let bytes = std::fs::read(path).ok()?;
    let hash = blake3::hash(&bytes).to_hex().to_string();

    let config = LiteParseConfig {
        output_format: OutputFormat::Text,
        ocr_enabled: false,
        image_mode: ImageMode::Off,
        extract_links: false,
        quiet: true,
        ..Default::default()
    };
    let parser = LiteParse::new(config);

    let path_str_in = path.to_string_lossy().to_string();
    let result = runtime().block_on(async { parser.parse(&path_str_in).await });
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "liteparse failed; skipping");
            return None;
        }
    };

    let path_str = normalize_path(&path.to_string_lossy());
    let path_str = match repo_root {
        Some(root) => strip_repo_root(&path_str, root),
        None => path_str,
    };

    let total_pages = result.pages.len() as u32;
    let mut symbols: Vec<Symbol> = Vec::with_capacity(result.pages.len());
    for page in &result.pages {
        let page_no = page.page_number as u32;
        let trimmed = page.text.trim();
        if trimmed.is_empty() {
            // Pure-image pages or scanned documents without OCR show up as
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
        let docstring = truncate_chars(trimmed, PAGE_TEXT_CAP);
        symbols.push(page_symbol(page_no, name, Some(docstring)));
    }

    // Stamp the file path on every symbol — mirrors what
    // `indexer::process_file` does for tree-sitter languages.
    for sym in symbols.iter_mut() {
        sym.file = path_str.clone();
    }

    let classification = crate::indexer::classifier::classify_file(&path_str, &symbols);

    Some(FileNode {
        path: path_str,
        hash,
        language: language_for(&ext).to_string(),
        classification,
        symbols,
        // Repurpose `lines` as page count so the UI's per-file "N lines"
        // badge becomes "N pages" for these formats.
        lines: total_pages,
        imports: Vec::new(),
        exports: Vec::new(),
    })
}

/// Build one `Symbol` for a page. `start_line == end_line == page_no` so
/// downstream UI controls that key off line ranges (snippet readers,
/// scroll-to-line buttons) still get a stable number, even though the
/// underlying file is binary and `read_snippet` will silently no-op.
fn page_symbol(page_no: u32, name: String, docstring: Option<String>) -> Symbol {
    Symbol {
        id: format!("doc_page:{}", page_no),
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
        ..Default::default()
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
    let snippet = truncate_chars(&first_line, NAME_CAP);
    format!("p.{} · {}", page_no, snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        // "héllo" — é is two bytes; truncating to 2 must back up.
        let s = "héllo";
        assert_eq!(truncate_chars(s, 100), "héllo");
        // cap=2 lands inside the é; the function should back up to
        // byte 1 (before é) and append the ellipsis.
        let out = truncate_chars(s, 2);
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

    #[test]
    fn language_for_groups_extensions() {
        assert_eq!(language_for("pdf"), "pdf");
        assert_eq!(language_for("docx"), "word");
        assert_eq!(language_for("xlsx"), "excel");
        assert_eq!(language_for("pptx"), "powerpoint");
    }

    #[test]
    fn is_supported_ext_covers_office_formats() {
        assert!(is_supported_ext("pdf"));
        assert!(is_supported_ext("docx"));
        assert!(is_supported_ext("xlsx"));
        assert!(is_supported_ext("pptx"));
        assert!(!is_supported_ext("exe"));
    }

    // ---- derive_page_name edge cases ----------------------------------

    #[test]
    fn a_long_first_line_is_capped_on_a_char_boundary() {
        // A page whose first line is a wall of text — common in scanned
        // documents with no heading structure. The name has to stay short
        // enough to render in a list without becoming the list.
        let name = derive_page_name(&"é".repeat(400), 1);
        let prefix = "p.1 · ";
        let snippet = name.strip_prefix(prefix).expect("prefixed with the page no");
        assert!(name.ends_with('…'), "should be truncated: {name}");
        assert!(name.is_char_boundary(name.len()));
        // `truncate_chars` caps bytes and backs up to a boundary, so the
        // snippet is at most the cap plus the ellipsis it appends.
        assert!(
            snippet.len() <= NAME_CAP + '…'.len_utf8(),
            "snippet {} bytes",
            snippet.len()
        );
    }

    #[test]
    fn a_first_line_exactly_at_the_cap_is_not_truncated() {
        let line = "a".repeat(NAME_CAP);
        let name = derive_page_name(&line, 2);
        assert_eq!(name, format!("p.2 · {line}"));
        assert!(!name.ends_with('…'));
    }

    #[test]
    fn leading_whitespace_is_stripped_from_the_chosen_line() {
        // PDF extraction routinely leaves indentation on the heading line.
        assert_eq!(derive_page_name("\t   Chapter One   \n", 3), "p.3 · Chapter One");
    }

    #[test]
    fn a_page_of_only_whitespace_falls_back_to_its_number() {
        // Blank pages, separator pages and image-only pages all land here.
        for text in ["", "\n", "   ", "\n\t \r\n  \n"] {
            assert_eq!(derive_page_name(text, 9), "Page 9", "text {text:?}");
        }
    }

    #[test]
    fn page_numbering_is_carried_verbatim_including_zero() {
        assert_eq!(derive_page_name("Intro", 0), "p.0 · Intro");
        assert_eq!(derive_page_name("", 0), "Page 0");
        assert!(derive_page_name("Intro", u32::MAX).starts_with(&format!("p.{} · ", u32::MAX)));
    }

    // ---- page_symbol ---------------------------------------------------

    #[test]
    fn a_page_symbol_collapses_its_line_range_onto_the_page_number() {
        // The file is binary, so there are no lines to point at. Both ends
        // carry the page number instead, which keeps every UI control that
        // keys off a line range (snippet reader, scroll-to-line) working on
        // a stable value rather than a zero.
        let s = page_symbol(7, "p.7 · Intro".into(), Some("body text".into()));
        assert_eq!(s.start_line, 7);
        assert_eq!(s.end_line, 7);
        assert_eq!(s.id, "doc_page:7");
        assert_eq!(s.docstring.as_deref(), Some("body text"));
    }

    #[test]
    fn a_page_symbol_is_a_top_level_heading_so_it_graphs_as_a_concept() {
        // `heading_1` is what `graph.rs` parses into a Concept node and
        // hangs directly off the file; any other kind would nest pages
        // under each other or type them as code.
        let s = page_symbol(1, "p.1".into(), None);
        assert_eq!(s.kind, "heading_1");
        assert_eq!(crate::indexer::document::tests::heading_level(&s.kind), Some(1));
    }

    /// Mirror of `graph::parse_heading_level`, which is private to that
    /// module — kept here so the kind above is checked against the parse it
    /// has to satisfy rather than against a bare string.
    fn heading_level(kind: &str) -> Option<usize> {
        kind.strip_prefix("heading_")?.parse().ok()
    }

    #[test]
    fn a_page_symbol_carries_no_code_structure() {
        // Pages have no signature, imports or calls; leaving stray defaults
        // here would show up as empty sections in the node panel.
        let s = page_symbol(2, "p.2".into(), None);
        assert!(s.signature.is_none());
        assert!(s.metrics.is_none());
        assert!(s.imports.is_empty() && s.exports.is_empty());
        assert!(s.extends.is_empty() && s.implements.is_empty() && s.calls.is_empty());
        assert!(s.annotations.is_empty() && s.call_refs.is_empty());
        assert!(s.qualified_name.is_none() && s.owner.is_none() && s.route.is_none());
        // `file` is stamped later by the caller, once the path is known.
        assert!(s.file.is_empty());
    }
}
