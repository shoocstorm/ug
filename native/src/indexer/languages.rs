//! Language indexer registry.
//!
//! Each supported language lives in its own submodule and implements the
//! [`LanguageIndexer`] trait. The trait deliberately owns *only* the
//! language-specific concerns - file walking, hashing, classification and
//! caching are language-agnostic and live in sibling modules.
//!
//! ## Adding a new language (e.g. Java)
//!
//! 1. Create `languages/java.rs` with a `JavaIndexer` struct that implements
//!    [`LanguageIndexer`].
//! 2. Declare the submodule (`mod java;`) below.
//! 3. Add an entry to [`for_extension`] mapping the file extensions to the
//!    new indexer.
//! 4. Add the same extensions to `super::common::SUPPORTED_EXTS` so the file
//!    walker picks them up.
//! 5. Add a `tree-sitter-java` dependency in `Cargo.toml`.
//!
//! No other module needs to change.

mod java;
mod markdown;
mod python;
mod typescript;

use crate::types::{ExportInfo, ImportInfo, Symbol};
use tree_sitter::Node;

/// Per-language extraction strategy. Implementations are stateless singletons
/// reachable via [`for_extension`].
pub trait LanguageIndexer: Send + Sync {
    /// Display name surfaced on `FileNode.language`.
    fn name(&self) -> &'static str;

    /// File extensions (lower-case, no leading dot) handled by this indexer.
    fn extensions(&self) -> &'static [&'static str];

    /// The tree-sitter grammar used to parse files of this language.
    fn tree_sitter_language(&self) -> tree_sitter::Language;

    /// Parse the file's top-level imports.
    fn extract_imports(&self, source: &[u8], root: Node) -> Vec<ImportInfo>;

    /// Parse the file's top-level exports.
    fn extract_exports(&self, source: &[u8], root: Node) -> Vec<ExportInfo>;

    /// Walk the AST and extract every symbol the language exposes
    /// (functions, classes, variables, type aliases, etc.).
    fn extract_symbols(&self, source: &[u8], root: Node) -> Vec<Symbol>;
}

/// Look up the indexer responsible for a given file extension. Returns
/// `None` for any extension we don't have a registered handler for.
pub fn for_extension(ext: &str) -> Option<&'static dyn LanguageIndexer> {
    // Stateless singletons - cheaper than allocating per call and easy to
    // hand back as `&'static dyn LanguageIndexer`.
    static TYPESCRIPT: typescript::TypeScriptIndexer = typescript::TypeScriptIndexer;
    static PYTHON: python::PythonIndexer = python::PythonIndexer;
    static JAVA: java::JavaIndexer = java::JavaIndexer;
    static MARKDOWN: markdown::MarkdownIndexer = markdown::MarkdownIndexer;

    if TYPESCRIPT.extensions().contains(&ext) {
        Some(&TYPESCRIPT)
    } else if PYTHON.extensions().contains(&ext) {
        Some(&PYTHON)
    } else if JAVA.extensions().contains(&ext) {
        Some(&JAVA)
    } else if MARKDOWN.extensions().contains(&ext) {
        Some(&MARKDOWN)
    } else {
        None
    }
}
