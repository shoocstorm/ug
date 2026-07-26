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
    let file_stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let file_name = file_stem.to_lowercase();

    // Markdown, PDF and office documents land here before any of the path
    // heuristics so a `docs/components/intro.md` (or a `.pdf`/`.docx`
    // shipped under `components/`) doesn't get misclassified as a
    // component. Kept in sync with `document::is_supported_ext`.
    const DOCUMENT_EXTS: &[&str] = &[
        ".md", ".mdx", ".markdown", ".pdf",
        ".doc", ".docx", ".docm", ".dot", ".dotm", ".dotx", ".odt", ".ott", ".rtf",
        ".xls", ".xlsx", ".xlsm", ".xlsb", ".ods", ".ots",
        ".ppt", ".pptx", ".pptm", ".pot", ".potm", ".potx", ".odp", ".otp",
    ];
    if DOCUMENT_EXTS.iter().any(|ext| path_lower.ends_with(ext)) {
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

    if is_config_file(&path_lower, &file_name) {
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
    //
    // Tested against the original stem, not the lowercased `file_name`: the
    // case test was previously applied to a string that had already been
    // lowercased, so it could never be true and the ALL_CAPS half of this
    // rule never fired at all.
    let is_all_caps = file_stem.chars().any(|c| c.is_alphabetic())
        && file_stem
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase());
    if is_all_caps
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

/// Extensions whose *content* is configuration rather than code.
///
/// None of these are in `common::SUPPORTED_EXTS` yet, so this list cannot
/// fire today. It is here so the rule is right by construction when one of
/// them starts being indexed — the alternative is rediscovering the
/// distinction below at that point.
const CONFIG_DATA_EXTS: &[&str] = &[
    "json", "yaml", "yml", "toml", "ini", "cfg", "conf", "properties", "env",
];

/// Whether this path *is* configuration, as opposed to source code that
/// happens to deal with configuration.
///
/// The distinction matters more than the other classifications because
/// `Config` is the only one that changes a node's *type* in the graph
/// (`graph.rs` maps it to `GraphNodeType::Config`); the rest are metadata
/// labels. So a false positive here doesn't just mislabel a file, it moves it
/// out of the `File` type entirely — wrong colour in the visualization, wrong
/// answer to a type-filtered search, and it reads as a non-code artifact.
///
/// The rule used to be "named `config` or `settings`, or living under a
/// `/config/` directory", which classified `native/src/config.rs` — a Rust
/// module implementing config handling, full of functions — as a config file.
/// A directory name and a module name are both too weak to override "this is
/// a source file". What survives is convention that unambiguously marks a
/// file as configuration:
///
/// - a configuration data format ([`CONFIG_DATA_EXTS`]);
/// - the `*.config.*` entry-point convention (`vite.config.ts`,
///   `babel.config.js`, `next.config.mjs`);
/// - a dotfile `rc` (`.eslintrc.js`, `.babelrc`, `.prettierrc.yml`).
///
/// Deliberately *not* matched: a code module named `config` or `settings`
/// (Django's `settings.py`, this crate's `config.rs`). Those are code that
/// expresses configuration, and they carry symbols the graph should keep
/// treating as code.
fn is_config_file(path_lower: &str, file_stem: &str) -> bool {
    let ext = Path::new(path_lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if CONFIG_DATA_EXTS.contains(&ext) {
        return true;
    }
    // `file_stem` for `vite.config.ts` is `vite.config`, so the suffix test
    // catches the convention without catching a bare `config`.
    if file_stem.ends_with(".config") {
        return true;
    }
    file_stem.starts_with('.') && file_stem.ends_with("rc")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(path: &str) -> Option<FileClassification> {
        classify_file(path, &[])
    }

    #[test]
    fn a_source_module_named_config_stays_code() {
        // The reported bug: a Rust module implementing config handling was
        // typed as a Config node, so it stopped looking like code.
        assert_eq!(classify("native/src/config.rs"), None);
        assert_eq!(classify("src/settings.py"), None);
        assert_eq!(classify("app/config.ts"), None);
    }

    #[test]
    fn a_config_directory_does_not_override_being_source() {
        // `src/config/` in a TS app holds config *modules*; a directory name
        // is too weak a signal to retype the files inside it.
        assert_eq!(classify("src/config/database.ts"), None);
        assert_eq!(classify("src/config/index.ts"), None);
    }

    #[test]
    fn the_config_entry_point_convention_is_honoured() {
        let cfg = Some(FileClassification::Config);
        assert_eq!(classify("vite.config.ts"), cfg);
        assert_eq!(classify("babel.config.js"), cfg);
        assert_eq!(classify("next.config.mjs"), cfg);
        assert_eq!(classify(".eslintrc.js"), cfg);
        assert_eq!(classify(".prettierrc"), cfg);
    }

    #[test]
    fn config_data_formats_are_config_once_indexed() {
        let cfg = Some(FileClassification::Config);
        assert_eq!(classify("tsconfig.json"), cfg);
        assert_eq!(classify("deploy/values.yaml"), cfg);
        assert_eq!(classify("Cargo.toml"), cfg);
    }

    // ---- the ladder, rung by rung ----------------------------------------
    //
    // `classify_file` is a first-match-wins chain, so these pin two separate
    // things: that each rule fires at all, and that inserting a rule later
    // can't shadow an earlier one. The ordering tests below are the ones that
    // matter — a wrong order produces a plausible-looking answer, which is
    // exactly the failure that goes unnoticed.

    #[test]
    fn every_rung_of_the_ladder_fires() {
        use FileClassification::*;
        let cases: &[(&str, FileClassification)] = &[
            ("README.md", Documentation),
            ("spec/api.pdf", Documentation),
            ("src/foo.test.ts", Test),
            ("src/foo.spec.tsx", Test),
            ("src/components/Button.tsx", Component),
            ("src/MyComponent.ts", Component),
            ("src/pages/Home.tsx", Page),
            ("app/routes/login.ts", Page),
            ("src/hooks/useAuth.ts", Hook),
            ("src/useModal.ts", Hook),
            ("src/services/api.ts", Service),
            ("src/authService.ts", Service),
            ("src/contexts/Theme.tsx", Context),
            ("src/ThemeContext.tsx", Context),
            ("src/reducers/cart.ts", Reducer),
            ("src/cartReducer.ts", Reducer),
            ("src/utils/date.ts", Util),
            ("src/dateHelper.ts", Util),
            ("vite.config.ts", Config),
            ("src/types/api.ts", Type),
            ("src/apiTypes.ts", Type),
            ("src/MAX_RETRIES.ts", Constant),
            ("assets/logo.svg", Asset),
        ];
        for (path, want) in cases {
            assert_eq!(classify(path).as_ref(), Some(want), "path {path}");
        }
    }

    #[test]
    fn an_uncategorised_source_file_stays_uncategorised() {
        // Most files match nothing, and that is a real answer — not a reason
        // to reach for the nearest plausible label.
        assert_eq!(classify("src/main.rs"), None);
        assert_eq!(classify("native/src/graph.rs"), None);
        assert_eq!(classify("src/lib/parse.ts"), None);
    }

    #[test]
    fn documents_win_over_every_path_heuristic() {
        use FileClassification::*;
        // A doc's extension is checked before any directory rule, so a doc
        // filed under a code-shaped directory is still a doc.
        for path in [
            "docs/components/intro.md",
            "docs/hooks/useThing.md",
            "src/utils/README.md",
            "src/services/notes.pdf",
        ] {
            assert_eq!(classify(path).as_ref(), Some(&Documentation), "path {path}");
        }
    }

    #[test]
    fn tests_win_over_the_directory_they_live_in() {
        use FileClassification::*;
        for path in [
            "src/components/Button.test.tsx",
            "src/hooks/useAuth.spec.ts",
            "src/services/api.test.ts",
        ] {
            assert_eq!(classify(path).as_ref(), Some(&Test), "path {path}");
        }
    }

    #[test]
    fn the_specific_beats_the_general_across_the_chain() {
        use FileClassification::*;
        // Each of these matches two rules; the earlier, more specific one
        // must win. Written as pairs so the intent survives a reordering.
        let cases: &[(&str, FileClassification)] = &[
            // component before page
            ("src/pages/components/Card.tsx", Component),
            // hook before service
            ("src/services/useFetch.ts", Hook),
            // service before util
            ("src/utils/authService.ts", Service),
            // util before type
            ("src/utils/apiTypes.ts", Util),
            // config convention before type
            ("src/types/vite.config.ts", Config),
        ];
        for (path, want) in cases {
            assert_eq!(classify(path).as_ref(), Some(want), "path {path}");
        }
    }

    #[test]
    fn classification_is_case_insensitive_on_the_path() {
        use FileClassification::*;
        assert_eq!(classify("SRC/Components/Button.TSX").as_ref(), Some(&Component));
        assert_eq!(classify("README.MD").as_ref(), Some(&Documentation));
        assert_eq!(classify("Vite.Config.TS").as_ref(), Some(&Config));
    }

    #[test]
    fn the_all_caps_constant_rule_fires_and_does_not_overreach() {
        use FileClassification::*;
        // Regression: the case test ran against an already-lowercased stem,
        // so the ALL_CAPS half of this rule was unreachable.
        assert_eq!(classify("src/MAX_RETRIES.ts").as_ref(), Some(&Constant));
        assert_eq!(classify("src/API_URL.ts").as_ref(), Some(&Constant));
        assert_eq!(classify("src/_404.ts").as_ref(), Some(&Constant));
        // Mixed case is an ordinary module.
        assert_eq!(classify("src/MaxRetries.ts"), None);
        assert_eq!(classify("src/maxRetries.ts"), None);
        // A single letter is not a constant module.
        assert_eq!(classify("src/i.ts"), None);
    }

    #[test]
    fn a_context_module_is_recognised_from_its_exports() {
        // The one symbol-shape fallback: no path heuristic matched, so the
        // decision comes from what the file exports.
        let sym = Symbol {
            id: "s".into(),
            name: "ThemeProvider".into(),
            kind: "function_declaration".into(),
            file: String::new(),
            start_line: 1,
            end_line: 9,
            docstring: None,
            signature: None,
            imports: Vec::new(),
            exports: vec![crate::types::ExportInfo {
                name: "ThemeProvider".into(),
                alias: None,
                is_default: false,
            }],
            extends: Vec::new(),
            implements: Vec::new(),
            calls: Vec::new(),
            metrics: None,
        };
        assert_eq!(
            classify_file("src/theme.tsx", std::slice::from_ref(&sym)),
            Some(FileClassification::Context)
        );
        // Without the export it stays uncategorised rather than guessing.
        let mut plain = sym.clone();
        plain.exports.clear();
        assert_eq!(classify_file("src/theme.tsx", &[plain]), None);
    }
}
