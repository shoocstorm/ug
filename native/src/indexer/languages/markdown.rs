//! Markdown indexer. Handles `.md`, `.mdx`, `.markdown`.
//!
//! Markdown isn't a programming language, so the mapping into the symbol
//! model is intentionally loose:
//!
//! - **Symbols**: every ATX heading (`#`, `##`, …) becomes a symbol whose
//!   `kind` is `heading_<level>`. Headings inside fenced code blocks are
//!   ignored - they're code, not document structure.
//! - **Imports**: every link or image whose target is a local relative
//!   path is recorded as an `ImportInfo`, so the graph layer can connect
//!   docs to the source files / sibling docs they reference. URLs,
//!   `mailto:` links and pure anchors are skipped.
//! - **Exports**: markdown has no export concept.
//!
//! Extraction is regex-based on the source. Tree-sitter-md splits markdown
//! across two grammars (block + inline) and we only need a small slice of
//! the structure, so a hand-rolled scan is simpler and good enough.

use crate::indexer::languages::LanguageIndexer;
use crate::types::{ExportInfo, ImportInfo, ImportedItem, Symbol};
use std::collections::HashMap;
use tree_sitter::Node;

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
fn extract_headings(source: &[u8]) -> Vec<Symbol> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let lines: Vec<&str> = source_str.lines().collect();
    let total_lines = lines.len() as u32;

    // First pass: collect (start_line, level, name) for every heading.
    let mut raw: Vec<(u32, usize, String)> = Vec::new();
    let mut in_fence = false;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
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

        let name = raw[i].2.clone();
        out.push(Symbol {
            id: format!("heading:{}:{}", start_line, name),
            name,
            kind: format!("heading_{}", level),
            file: String::new(),
            start_line,
            end_line,
            docstring: None,
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

/// Pull out every `[text](target)` and `![alt](target)` whose target is a
/// local relative path. Aggregates by path so a document that references the
/// same file three times produces one `ImportInfo`.
fn extract_local_links(source: &[u8]) -> Vec<ImportInfo> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // The optional `(?:\s+"[^"]*")?` group swallows the title attribute
    // that markdown allows after the URL (`[t](u "title")`).
    let re = match regex::Regex::new(r#"!?\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)"#) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut by_path: HashMap<String, ImportInfo> = HashMap::new();
    for cap in re.captures_iter(source_str) {
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
