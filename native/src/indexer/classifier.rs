//! File classification heuristics.
//!
//! Classifies a source file into a `FileClassification` based on its path
//! and the symbols it exposes. Pure functions over inputs, no I/O. Order of
//! checks matters: the first match wins, so the more specific patterns
//! (tests, components, pages) are checked before the more general ones
//! (utils, types, constants).

use crate::types::{FileClassification, Symbol};
use std::path::Path;

/// Best-effort classification of a source file. Walks a series of cheap path
/// and name heuristics first, then falls back to symbol-shape inspection.
/// Returns `None` when no heuristic fires - callers should treat that as
/// "uncategorised".
pub fn classify_file(path: &str, symbols: &[Symbol]) -> Option<FileClassification> {
    let path_lower = path.to_lowercase();
    let file_name = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Markdown and PDF land here before any of the path heuristics so a
    // `docs/components/intro.md` (or a `.pdf` shipped under `components/`)
    // doesn't get misclassified as a component.
    if path_lower.ends_with(".md")
        || path_lower.ends_with(".mdx")
        || path_lower.ends_with(".markdown")
        || path_lower.ends_with(".pdf")
    {
        return Some(FileClassification::Documentation);
    }

    // Tests come first: a `Button.test.tsx` should never be misread as a
    // component just because of the directory it sits in.
    if path_lower.contains(".test.")
        || path_lower.contains(".spec.")
        || file_name.ends_with(".test")
        || file_name.ends_with(".spec")
    {
        return Some(FileClassification::Test);
    }

    if path_lower.contains("/components/")
        || path_lower.contains("/component/")
        || file_name.contains("component")
    {
        return Some(FileClassification::Component);
    }

    if path_lower.contains("/pages/")
        || path_lower.contains("/page/")
        || path_lower.contains("/routes/")
        || (file_name == "index" && path_lower.contains("/page"))
    {
        return Some(FileClassification::Page);
    }

    if path_lower.contains("/hooks/")
        || path_lower.contains("/hook/")
        || file_name.starts_with("use")
    {
        return Some(FileClassification::Hook);
    }

    if path_lower.contains("/services/")
        || path_lower.contains("/service/")
        || file_name.ends_with("service")
    {
        return Some(FileClassification::Service);
    }

    if path_lower.contains("/contexts/")
        || path_lower.contains("/context/")
        || file_name.ends_with("context")
    {
        return Some(FileClassification::Context);
    }

    if path_lower.contains("/reducers/")
        || path_lower.contains("/reducer/")
        || file_name.ends_with("reducer")
    {
        return Some(FileClassification::Reducer);
    }

    if path_lower.contains("/utils/")
        || path_lower.contains("/util/")
        || path_lower.contains("/helpers/")
        || path_lower.contains("/helper/")
        || file_name.ends_with("util")
        || file_name.ends_with("helper")
    {
        return Some(FileClassification::Util);
    }

    if path_lower.contains("/config/") || file_name == "config" || file_name == "settings" {
        return Some(FileClassification::Config);
    }

    if file_name.ends_with("type")
        || file_name.ends_with("types")
        || path_lower.contains("/types/")
    {
        return Some(FileClassification::Type);
    }

    // ALL_CAPS or pure-digit-with-underscore filenames look like constant
    // modules (`MAX_RETRIES.ts`, `_404.ts`). Length > 1 prevents matches on
    // single-char names like `i`.
    if file_name.chars().all(|c| c.is_uppercase())
        || (file_name.chars().all(|c| c.is_ascii_digit() || c == '_') && file_name.len() > 1)
    {
        return Some(FileClassification::Constant);
    }

    if path_lower.ends_with(".png")
        || path_lower.ends_with(".jpg")
        || path_lower.ends_with(".svg")
        || path_lower.ends_with(".ico")
        || path_lower.ends_with(".gif")
    {
        return Some(FileClassification::Asset);
    }

    // Symbol-shape fallback: a file that exports something ending in
    // `Provider` or `Context` is almost certainly a React context module
    // even if its path didn't match any of the directory heuristics above.
    if symbols.iter().any(|s| {
        matches!(
            s.kind.as_str(),
            "function_declaration" | "function" | "method_definition"
        )
    }) {
        let exports: Vec<&str> = symbols
            .iter()
            .filter_map(|s| s.exports.first().map(|e| e.name.as_str()))
            .collect();
        if exports
            .iter()
            .any(|e| e.ends_with("Provider") || e.ends_with("Context"))
        {
            return Some(FileClassification::Context);
        }
    }

    None
}
