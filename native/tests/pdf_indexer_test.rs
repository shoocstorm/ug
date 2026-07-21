//! End-to-end tests for the PDF indexing path.
//!
//! Covers: file-walker pickup, classification, per-page Symbol shape,
//! mixed PDF/markdown scans, and the graph-layer round-trip
//! (`File` → `Contains` → `Concept` page).
//!
//! Fixtures live in `tests/fixtures/` — see the README there for
//! provenance. Tests copy them into a per-test `TempDir` so they
//! don't leak state across runs and `scan_files`'s `.gitignore`
//! traversal doesn't see anything beyond the staged content.

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;
use ultragraph::types::{FileClassification, GraphData, GraphEdgeType, GraphNodeType, IndexResult};
use ultragraph::{build_graph, index};

/// Read a fixture PDF as bytes. `include_bytes!` is resolved at
/// compile-time so a missing fixture surfaces as a build error rather
/// than a runtime panic.
const HELLO_PDF: &[u8] = include_bytes!("fixtures/hello.pdf");
const UNICODE_PDF: &[u8] = include_bytes!("fixtures/unicode.pdf");
const LATIN1_PDF: &[u8] = include_bytes!("fixtures/latin1.pdf");

/// Write the bundled `hello.pdf` to a `<TempDir>/<name>.pdf` path.
/// Returns the temp dir (must be kept alive for the file to exist) and
/// the absolute file path the indexer should pick up.
fn stage_pdf(name: &str, bytes: &[u8]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join(name);
    fs::write(&path, bytes).expect("write fixture");
    (dir, path)
}

/// Convenience: index `dir` end-to-end and parse the JSON result.
fn run_index(dir: &TempDir) -> IndexResult {
    let json = index(dir.path().to_string_lossy().to_string());
    serde_json::from_str(&json).expect("index() returned invalid JSON")
}

#[test]
fn index_scan_picks_up_pdf_files() {
    let (dir, _) = stage_pdf("hello.pdf", HELLO_PDF);
    let result = run_index(&dir);
    assert_eq!(result.files.len(), 1, "expected hello.pdf to be indexed");
    let file = &result.files[0];
    assert_eq!(file.language, "pdf");
    assert!(file.path.ends_with("hello.pdf"));
}

#[test]
fn pdf_file_is_classified_as_documentation() {
    let (dir, _) = stage_pdf("manual.pdf", HELLO_PDF);
    let result = run_index(&dir);
    let file = &result.files[0];
    assert!(
        matches!(file.classification, Some(FileClassification::Documentation)),
        "PDFs should always classify as Documentation regardless of path, got: {:?}",
        file.classification
    );
}

#[test]
fn pdf_extracts_one_symbol_per_page() {
    let (dir, _) = stage_pdf("hello.pdf", HELLO_PDF);
    let result = run_index(&dir);
    let file = &result.files[0];
    // hello.pdf is a single-page document → exactly one symbol.
    assert_eq!(file.symbols.len(), 1);
    let sym = &file.symbols[0];
    assert_eq!(sym.kind, "heading_1", "PDF pages reuse heading_1 so the graph layer maps them to Concept");
    // `doc_page`, not `pdf_page`: the indexer that produces these also
    // handles Word/Excel/PowerPoint, so the id is format-agnostic.
    assert_eq!(sym.id, "doc_page:1");
    assert_eq!(sym.start_line, 1);
    assert_eq!(sym.end_line, 1);
}

#[test]
fn pdf_page_text_lands_in_docstring() {
    let (dir, _) = stage_pdf("hello.pdf", HELLO_PDF);
    let result = run_index(&dir);
    let sym = &result.files[0].symbols[0];
    let docstring = sym
        .docstring
        .as_deref()
        .expect("hello.pdf should produce a non-empty docstring");
    // pdf-extract returns the literal page text — search for the
    // canonical string so we don't pin on exact whitespace.
    assert!(
        docstring.contains("Hello World"),
        "extracted text should contain 'Hello World', got: {:?}",
        docstring
    );
}

#[test]
fn pdf_multibyte_text_survives_extraction() {
    let (dir, _) = stage_pdf("latin1.pdf", LATIN1_PDF);
    let result = run_index(&dir);
    let sym = &result.files[0].symbols[0];
    let docstring = sym.docstring.as_deref().unwrap_or("");
    // The fixture's high Latin-1 bytes become multi-byte UTF-8 once
    // extracted, which is the path that matters: the extractor has to
    // decode PDF's mixed encodings, and truncate() has to respect char
    // boundaries rather than slicing mid-codepoint.
    assert!(
        docstring.contains("café") && docstring.contains("münchen"),
        "expected accented text to survive extraction, got: {:?}",
        docstring
    );
    assert!(
        docstring.chars().any(|c| c.len_utf8() > 1),
        "fixture should contain at least one multi-byte character"
    );
}

#[test]
fn pdf_without_extractable_text_degrades_gracefully() {
    // unicode.pdf draws emoji through an embedded font with no usable
    // Unicode mapping, so the extractor legitimately gets nothing back.
    // That must still yield a well-formed page symbol rather than a panic
    // or a dropped file — an unreadable page is normal in the wild
    // (scans, image-only exports).
    let (dir, _) = stage_pdf("unicode.pdf", UNICODE_PDF);
    let result = run_index(&dir);
    assert_eq!(result.files.len(), 1, "the file must still be indexed");
    let file = &result.files[0];
    assert_eq!(file.language, "pdf");
    assert_eq!(file.symbols.len(), 1, "one page → one symbol, text or not");

    let sym = &file.symbols[0];
    assert_eq!(sym.id, "doc_page:1");
    assert_eq!(sym.start_line, 1);
    assert!(
        sym.docstring.as_deref().unwrap_or("").is_empty(),
        "no extractable text should mean no docstring, got: {:?}",
        sym.docstring
    );
    assert!(
        sym.name.contains("no text"),
        "the name should say the page had no text, got: {:?}",
        sym.name
    );
}

#[test]
fn pdf_page_name_includes_page_number_prefix() {
    let (dir, _) = stage_pdf("hello.pdf", HELLO_PDF);
    let result = run_index(&dir);
    let sym = &result.files[0].symbols[0];
    // Names follow "p.<n> · <preview>" so the UI shows ordering even
    // when the first line is identical across pages.
    assert!(
        sym.name.starts_with("p.1"),
        "page-1 name should start with 'p.1', got: {:?}",
        sym.name
    );
}

#[test]
fn pdf_repurposes_lines_as_page_count() {
    let (dir, _) = stage_pdf("hello.pdf", HELLO_PDF);
    let result = run_index(&dir);
    let file = &result.files[0];
    // hello.pdf is single-page → `lines` is repurposed as page count = 1.
    assert_eq!(file.lines, 1);
}

#[test]
fn pdf_and_markdown_coexist_in_same_scan() {
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("doc.pdf"), HELLO_PDF).expect("write pdf");
    fs::write(
        dir.path().join("notes.md"),
        "# First\n\nA paragraph.\n\n# Second\n\nMore.\n",
    )
    .expect("write md");

    let result = run_index(&dir);
    assert_eq!(result.files.len(), 2, "both files should be indexed");
    let langs: Vec<&str> = result.files.iter().map(|f| f.language.as_str()).collect();
    assert!(langs.contains(&"pdf"));
    assert!(langs.contains(&"markdown"));

    // Markdown's two headings + PDF's one page = 3 symbols total. This
    // gates a regression where the PDF path accidentally replaces the
    // language pipeline instead of branching off it.
    let total_symbols: usize = result.files.iter().map(|f| f.symbols.len()).sum();
    assert_eq!(total_symbols, 3);
}

#[test]
fn pdf_pages_become_concept_nodes_with_contains_edge_in_graph() {
    let (dir, _) = stage_pdf("doc.pdf", HELLO_PDF);
    let result = run_index(&dir);
    let index_json = serde_json::to_string(&result).expect("re-encode IndexResult");
    let graph_json = build_graph(index_json);
    let graph: GraphData = serde_json::from_str(&graph_json).expect("graph JSON");

    // File node + page-Concept node + folder root.
    let file_node = graph
        .nodes
        .iter()
        .find(|n| n.node_type == GraphNodeType::File && n.id.contains("doc.pdf"))
        .expect("file node");
    let concept_node = graph
        .nodes
        .iter()
        .find(|n| n.node_type == GraphNodeType::Concept && n.id.contains("doc.pdf"))
        .expect("page concept node");

    // The page node carries the extracted text — surface it to the UI
    // via `docstring` so semantic search can rank it.
    assert!(
        concept_node
            .docstring
            .as_deref()
            .unwrap_or("")
            .contains("Hello World"),
        "page concept node must carry the page text as docstring"
    );

    // Verify the structural edge from the file to its page exists.
    let has_contains = graph.edges.iter().any(|e| {
        e.edge_type == GraphEdgeType::Contains
            && e.source == file_node.id
            && e.target == concept_node.id
    });
    assert!(
        has_contains,
        "expected a Contains edge from {} to {}",
        file_node.id, concept_node.id
    );
}

#[test]
fn pdf_skips_files_pdf_extract_cannot_open() {
    // Drop a "PDF" that's just garbage bytes — pdf-extract returns an
    // error and our process_pdf returns None, so the file is silently
    // skipped (matches the indexer's contract for unparseable inputs).
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("broken.pdf"), b"this is not actually a PDF")
        .expect("write garbage");

    let result = run_index(&dir);
    assert!(
        result.files.iter().all(|f| !f.path.ends_with("broken.pdf")),
        "garbage PDF should be skipped, got files: {:?}",
        result.files.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
}

#[test]
fn pdf_uppercase_extension_still_indexed() {
    // Some scanners and document exporters spit out `.PDF` / `.Pdf`.
    // The walker lowercases the extension before checking
    // SUPPORTED_EXTS, so case mustn't change behaviour — losing
    // documents to a stray capital letter would be a footgun.
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("DOC.PDF"), HELLO_PDF).expect("write fixture");
    let result = run_index(&dir);
    let file = result
        .files
        .iter()
        .find(|f| f.path.to_lowercase().ends_with(".pdf"))
        .expect("uppercase-extension PDF should be indexed");
    assert_eq!(file.language, "pdf");
    assert_eq!(file.symbols.len(), 1);
}
