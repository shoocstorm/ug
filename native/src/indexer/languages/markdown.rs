//! Markdown indexer. Handles `.md`, `.mdx`, `.markdown`.
//!
//! Markdown isn't a programming language, so the mapping into the symbol
//! model is intentionally loose:
//!
//! - **Symbols**: every ATX heading (`#`, `##`, …) becomes a symbol whose
//!   `kind` is `heading_<level>`. Headings inside fenced code blocks are
//!   ignored - they're code, not document structure.
//! - **Docstrings**: the prose under a heading, up to the next heading. See
//!   [`section_prose`] — for a document this text *is* the description, so
//!   unlike a code symbol it belongs in the embedding.
//! - **Imports**: every link or image whose target is a local relative
//!   path is recorded as an `ImportInfo`, so the graph layer can connect
//!   docs to the source files / sibling docs they reference. URLs,
//!   `mailto:` links and pure anchors are skipped.
//! - **Exports**: markdown has no export concept.
//!
//! Extraction is regex-based on the source. Tree-sitter-md splits markdown
//! across two grammars (block + inline) and we only need a small slice of
//! the structure, so a hand-rolled scan is simpler and good enough.

use crate::indexer::common::truncate_chars;
use crate::indexer::languages::LanguageIndexer;
use crate::types::{ExportInfo, ImportInfo, ImportedItem, Symbol};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;
use tree_sitter::Node;

/// Per-section byte cap on the prose attached as `docstring`.
///
/// The embedder's window is 512 tokens and the node's name, structural
/// synopsis and `Related:` list all share it, so a section gets roughly a
/// third. Longer sections keep their leading paragraphs, which in practice
/// state what the section is about; the full text is still reachable —
/// ingest captures the whole span into the row's `code` column, which feeds
/// the sparse keyword index and every snippet read.
pub(crate) const SECTION_TEXT_CAP: usize = 1_500;

pub struct MarkdownIndexer;

impl LanguageIndexer for MarkdownIndexer {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md", "mdx", "markdown"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_md::language()
    }

    fn extract_imports(&self, source: &[u8], _root: Node) -> Vec<ImportInfo> {
        // TODO: resolve local links as references instead of imports
        extract_local_links(source)
    }

    fn extract_exports(&self, _source: &[u8], _root: Node) -> Vec<ExportInfo> {
        Vec::new()
    }

    fn extract_symbols(&self, source: &[u8], _root: Node) -> Vec<Symbol> {
        extract_headings(source)
    }
}

/// Scan the source line-by-line and emit one `Symbol` per ATX heading.
/// Tracks fenced-code state so `#` lines inside a ```` ``` ```` block don't
/// get mistaken for headings.
///
/// `end_line` spans the heading's section: from the heading line through the
/// line before the next heading of the same or higher precedence (lower or
/// equal level number), or through the last line of the file for the final
/// heading. This gives the Semantic Enrichment phase the full body of text
/// that belongs to each heading symbol.
///
/// `docstring` is narrower than that span on purpose — see
/// [`section_prose`].
fn extract_headings(source: &[u8]) -> Vec<Symbol> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let lines: Vec<&str> = source_str.lines().collect();
    let total_lines = lines.len() as u32;

    // First pass: collect (start_line, level, name) for every heading, plus
    // a per-line flag marking the fenced-code regions so the body extractor
    // can drop them without re-deriving fence state.
    let mut raw: Vec<(u32, usize, String)> = Vec::new();
    let mut fenced = vec![false; lines.len()];
    let mut in_fence = false;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            fenced[idx] = true;
            continue;
        }
        fenced[idx] = in_fence;
        if in_fence {
            continue;
        }

        let bytes = trimmed.as_bytes();
        let level = bytes.iter().take(7).take_while(|&&b| b == b'#').count();
        if level == 0 || level > 6 {
            continue;
        }

        // Require a space after the marker so `#word` (anchor / hex colour)
        // isn't treated as a heading. Empty headings (`#` alone) are also
        // skipped.
        let rest = &trimmed[level..];
        if !rest.starts_with(' ') {
            continue;
        }
        let name = rest.trim().trim_end_matches('#').trim().to_string();
        if name.is_empty() {
            continue;
        }

        raw.push(((idx + 1) as u32, level, name));
    }

    // Second pass: compute each heading's end_line by scanning forward for
    // the next heading whose level is shallower-or-equal (i.e. closes the
    // current section). Falls back to the file's last line for the tail.
    let mut out = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let (start_line, level, _) = raw[i];
        let end_line = raw[i + 1..]
            .iter()
            .find(|(_, l, _)| *l <= level)
            .map(|(next_start, _, _)| next_start.saturating_sub(1).max(start_line))
            .unwrap_or_else(|| total_lines.max(start_line));

        // The *own* body stops at the next heading of any level, while
        // `end_line` above runs on through the subsections.
        let body_end = raw
            .get(i + 1)
            .map(|(next_start, _, _)| next_start.saturating_sub(1))
            .unwrap_or(total_lines);
        let docstring = section_prose(&lines, &fenced, start_line, body_end);

        let name = raw[i].2.clone();
        out.push(Symbol {
            id: format!("heading:{}:{}", start_line, name),
            name,
            kind: format!("heading_{}", level),
            file: String::new(),
            start_line,
            end_line,
            docstring,
            signature: None,
            imports: Vec::new(),
            exports: Vec::new(),
            extends: Vec::new(),
            implements: Vec::new(),
            calls: Vec::new(),
            metrics: None,
        });
    }

    out
}

/// The prose belonging to a heading: the lines strictly after it, through
/// `end` (1-indexed, inclusive), with fenced code dropped, inline links
/// flattened to their text and whitespace collapsed onto one line. `None`
/// when nothing readable is left.
///
/// # Why this text, and why not the wider `end_line` span
///
/// A code symbol's body is deliberately kept out of the embedding (see
/// `storage::text`) — there the name and docstring carry the meaning and
/// the body only dilutes them. A document inverts that: the heading is a
/// two-word label and the paragraph under it is the entire description.
/// Without this, a `Concept` node embeds as `"Concept: <heading>. .
/// Related: …"` and is reachable only by queries that happen to echo the
/// heading's own words.
///
/// The body stops at the next heading of *any* level, whereas the symbol's
/// `end_line` runs through its subsections. Using the wider span would copy
/// every child's prose into each ancestor's vector, and make an edit to one
/// subsection re-embed the whole chain above it. The wide span is still what
/// snippet reads and the sparse index use — this narrowing applies only to
/// the text that gets embedded.
///
/// Fenced code is dropped for the same reason bodies are: it is punctuation-
/// heavy, it crowds out prose inside [`SECTION_TEXT_CAP`], and it is already
/// searchable through the sparse channel, which indexes the captured span
/// verbatim. A section that is *only* a code fence yields `None` and falls
/// back to the structural synopsis, which is the honest outcome.
fn section_prose(lines: &[&str], fenced: &[bool], heading_line: u32, end: u32) -> Option<String> {
    // The heading itself sits at index `heading_line - 1`, so its body
    // starts at `heading_line`.
    let from = heading_line as usize;
    let to = (end as usize).min(lines.len());
    if from >= to {
        return None;
    }

    let mut buf = String::new();
    for (offset, line) in lines[from..to].iter().enumerate() {
        if fenced[from + offset] {
            continue;
        }
        let text = flatten_links(line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(text);
        if buf.len() >= SECTION_TEXT_CAP {
            break;
        }
    }

    if buf.is_empty() {
        return None;
    }
    Some(truncate_chars(&buf, SECTION_TEXT_CAP))
}

/// `[text](target)` and `![alt](target)`. The optional `(?:\s+"[^"]*")?`
/// group swallows the title attribute markdown allows after the URL
/// (`[t](u "title")`).
///
/// Compiled once: [`flatten_links`] runs per line of every document.
fn link_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"!?\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)"#)
            .expect("markdown link pattern is a literal and must compile")
    })
}

/// Reduce `[docs](./guide.md)` to `docs`. URL targets are punctuation the
/// embedder cannot use, and the paths themselves are already captured
/// structurally by [`extract_local_links`].
fn flatten_links(line: &str) -> Cow<'_, str> {
    link_regex().replace_all(line, "$1")
}

/// Pull out every `[text](target)` and `![alt](target)` whose target is a
/// local relative path. Aggregates by path so a document that references the
/// same file three times produces one `ImportInfo`.
fn extract_local_links(source: &[u8]) -> Vec<ImportInfo> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut by_path: HashMap<String, ImportInfo> = HashMap::new();
    for cap in link_regex().captures_iter(source_str) {
        let text = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        let target = match cap.get(2) {
            Some(m) => m.as_str().trim(),
            None => continue,
        };
        if !is_local_target(target) {
            continue;
        }

        // Drop the `#section` suffix so `./guide.md#install` and
        // `./guide.md#usage` collapse onto a single import entry.
        let path = target.split('#').next().unwrap_or(target).to_string();
        if path.is_empty() {
            continue;
        }

        let item = ImportedItem {
            name: if text.is_empty() { path.clone() } else { text.to_string() },
            alias: None,
        };
        by_path
            .entry(path.clone())
            .and_modify(|info| {
                if !info.imported.iter().any(|i| i.name == item.name) {
                    info.imported.push(item.clone());
                }
            })
            .or_insert(ImportInfo {
                path,
                imported: vec![item],
            });
    }

    by_path.into_values().collect()
}

/// True if the link target points at something inside the project. Anything
/// with a URI scheme (`http:`, `mailto:`, `tel:`, `data:`, …), a
/// protocol-relative `//` prefix, or a bare `#anchor` is considered external
/// and ignored.
fn is_local_target(target: &str) -> bool {
    if target.is_empty() || target.starts_with('#') || target.starts_with("//") {
        return false;
    }
    if let Some(scheme_end) = target.find(':') {
        let scheme = &target[..scheme_end];
        let scheme_like = !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
        if scheme_like {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headings(src: &str) -> Vec<Symbol> {
        extract_headings(src.as_bytes())
    }

    fn doc_of<'a>(syms: &'a [Symbol], name: &str) -> Option<&'a str> {
        syms.iter()
            .find(|s| s.name == name)
            .and_then(|s| s.docstring.as_deref())
    }

    #[test]
    fn section_body_becomes_the_docstring() {
        let syms = headings(
            "# Guide\n\
             Install it with the package manager you already use.\n\
             \n\
             It needs no sidecar service.\n",
        );
        let doc = doc_of(&syms, "Guide").expect("body captured");
        assert!(doc.contains("Install it with the package manager"), "{doc}");
        // Paragraph breaks collapse — the embedder wants one run of prose.
        assert!(doc.contains("use. It needs no sidecar"), "{doc}");
        assert!(!doc.contains("# Guide"), "heading is the name, not the body: {doc}");
    }

    #[test]
    fn parent_does_not_swallow_its_subsections() {
        let syms = headings(
            "# Backends\n\
             Two backends ship in the box.\n\
             \n\
             ## Local ONNX\n\
             Runs on CPU with no server to start.\n\
             \n\
             ## Hosted\n\
             Points at an existing embedding endpoint.\n",
        );

        let parent = doc_of(&syms, "Backends").unwrap();
        assert!(parent.contains("Two backends ship"), "{parent}");
        assert!(
            !parent.contains("Runs on CPU"),
            "child prose must not land in the parent's vector: {parent}"
        );

        // The wide span is unchanged — snippet reads still get the subsections.
        let parent_sym = syms.iter().find(|s| s.name == "Backends").unwrap();
        assert_eq!(parent_sym.end_line, 8, "span still covers the children");

        assert!(doc_of(&syms, "Local ONNX").unwrap().contains("Runs on CPU"));
        assert!(doc_of(&syms, "Hosted").unwrap().contains("existing embedding endpoint"));
    }

    #[test]
    fn fenced_code_is_left_to_the_sparse_index() {
        let syms = headings(
            "## Install\n\
             Grab the binary:\n\
             ```bash\n\
             cargo install ultragraph --locked\n\
             ```\n\
             That is the whole setup.\n",
        );
        let doc = doc_of(&syms, "Install").unwrap();
        assert!(doc.contains("Grab the binary"), "{doc}");
        assert!(doc.contains("That is the whole setup"), "{doc}");
        assert!(!doc.contains("cargo install"), "fence dropped: {doc}");
    }

    #[test]
    fn a_section_that_is_only_code_has_no_docstring() {
        let syms = headings("## Example\n```rust\nlet x = 1;\n```\n");
        assert!(
            doc_of(&syms, "Example").is_none(),
            "falls back to the structural synopsis rather than embedding punctuation"
        );
    }

    #[test]
    fn links_are_flattened_to_their_text() {
        let syms = headings("# Overview\nSee the [ingest guide](./docs/ingest.md) first.\n");
        let doc = doc_of(&syms, "Overview").unwrap();
        assert!(doc.contains("See the ingest guide first"), "{doc}");
        assert!(!doc.contains("./docs/ingest.md"), "url is not query vocabulary: {doc}");
    }

    #[test]
    fn long_sections_are_capped() {
        let body: String = std::iter::repeat("the quick brown fox jumps over it. ")
            .take(200)
            .collect();
        let syms = headings(&format!("# Long\n{}\n", body));
        let doc = doc_of(&syms, "Long").unwrap();
        assert!(doc.len() <= SECTION_TEXT_CAP + 4, "capped, got {}", doc.len());
        assert!(doc.ends_with('…'), "truncation is visible: {doc}");
    }

    #[test]
    fn headings_inside_a_fence_are_not_symbols() {
        let syms = headings(
            "# Real\n\
             ```md\n\
             # Not a heading\n\
             ```\n\
             Tail prose.\n",
        );
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Real");
        let doc = syms[0].docstring.as_deref().unwrap();
        assert!(!doc.contains("Not a heading"), "{doc}");
        assert!(doc.contains("Tail prose"), "{doc}");
    }
}
