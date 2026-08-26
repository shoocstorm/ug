//! Language-agnostic helpers shared by every language indexer.
//!
//! Anything in this file is intended to be reusable as new languages are
//! plugged in. The functions here only depend on tree-sitter, blake3 and the
//! filesystem - they know nothing about TypeScript, Python or any specific
//! grammar. When adding Java/Go/etc., prefer extending these helpers rather
//! than copying logic into the language module.

use crate::types::{ImportInfo, Param, Symbol};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// File extensions we are willing to index. Add new entries when registering
/// a new language indexer in `super::languages`. `pdf` is special-cased in
/// `indexer::process_file` — it's binary, so it bypasses the tree-sitter
/// pipeline and is handled by `indexer::document::process_document` (via
/// `pdf-extract`, pure Rust). Keep this list in sync with
/// `document::is_supported_ext`.
pub const SUPPORTED_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "py", "java", "rs", "md", "mdx", "markdown", "pdf",
];

/// Directory names that are always skipped during the file walk. Only names
/// that are *never* a legitimate source directory belong here — see
/// [`BUILD_OUTPUT_DIRS`] for the ones that depend on context.
pub const IGNORED_DIRS: &[&str] = &["node_modules", ".git"];

/// Directory names that hold build output for one toolchain but are a
/// perfectly ordinary source directory for another, paired with the build
/// descriptors whose presence identifies them.
///
/// `target` was previously ignored unconditionally, which is right for Maven
/// and Cargo and wrong for Java: `target` is a legal package name, so
/// `src/main/java/com/acme/target/` — and every class in it — vanished from
/// the index. Output directories sit next to the descriptor that generates
/// them, and source packages never do, so that is what we check.
pub const BUILD_OUTPUT_DIRS: &[(&str, &[&str])] = &[
    ("target", &["pom.xml", "build.sbt", "Cargo.toml"]),
    (
        "build",
        &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
    ),
    ("out", &["build.gradle", "build.gradle.kts", "pom.xml"]),
];

/// True when `path` is a build-output directory: its name matches one of
/// [`BUILD_OUTPUT_DIRS`] and its parent holds a descriptor that produces it.
pub fn is_build_output_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some((_, descriptors)) = BUILD_OUTPUT_DIRS.iter().find(|(d, _)| *d == name) else {
        return false;
    };
    if !path.is_dir() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    descriptors.iter().any(|d| parent.join(d).exists())
}

/// A node's source text, **borrowed** from `source`.
///
/// [`get_node_text`] copies; use this wherever the text is only read. The
/// difference matters in the per-symbol callers, where the copy is the whole
/// body of the function being described.
pub fn node_str<'a>(node: Node, source: &'a [u8]) -> Option<&'a str> {
    let (start, end) = (node.start_byte(), node.end_byte());
    if start < end {
        std::str::from_utf8(source.get(start..end)?).ok()
    } else {
        None
    }
}

/// Read a node's source text as UTF-8, returning `None` if the byte range is
/// invalid or the slice is not valid UTF-8.
pub fn get_node_text(node: Option<Node>, source: &[u8]) -> Option<String> {
    let node = node?;
    let start = node.start_byte();
    let end = node.end_byte();
    if start < end {
        String::from_utf8(source[start..end].to_vec()).ok()
    } else {
        None
    }
}

/// Drain a per-path import map into the `Vec<ImportInfo>` an extractor
/// returns, in a **deterministic** order.
///
/// Every language builds its imports in a `HashMap` keyed by source path, and
/// a `HashMap`'s iteration order is seeded per map — so draining one straight
/// into a `Vec` makes the output shuffle between runs over an unchanged file.
/// That is not cosmetic: the graph builder walks this list to emit `Imports`
/// and `References` edges, so the edge list reorders too, and the embedding
/// text is built from it and then *truncated to a budget* — so a different
/// order changes which terms survive the cut and the keyword index itself
/// stops being reproducible.
///
/// Java found this first and sorted locally; this is that fix, in the one
/// place every extractor already goes through, so a sixth language cannot
/// miss it. See P11.10 in docs/dev/PERF-TUNING-JOURNEY.md.
pub fn imports_in_stable_order(map: HashMap<String, ImportInfo>) -> Vec<ImportInfo> {
    let mut out: Vec<ImportInfo> = map.into_values().collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Truncate `s` to at most `cap` bytes on a char boundary, appending `…`
/// when truncation actually happened.
///
/// Boundary-aware so a multi-byte sequence is never split — the text this
/// runs over is extracted prose, which is full of accented characters,
/// ligatures and em-dashes. Shared by every extractor that caps text it
/// hands to the embedder.
pub fn truncate_chars(s: &str, cap: usize) -> String {
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

/// Cap on stored annotation / decorator / attribute argument text.
///
/// Long enough for a `@Query` with a real statement in it, short enough that
/// a giant array literal or a `#[derive(..)]` list cannot dominate the node's
/// retrieval text.
pub const MAX_ANNOTATION_ARGS: usize = 400;

/// Argument text of an annotation-like construct, with the enclosing
/// delimiters stripped and the result capped at [`MAX_ANNOTATION_ARGS`].
///
/// `None` for a marker with no arguments (`@Override`, `#[test]`), which is
/// the distinction [`crate::types::Annotation::args`] carries.
pub fn annotation_args(node: Option<Node>, source: &[u8]) -> Option<String> {
    let raw = get_node_text(node, source)?;
    let text = raw.trim();
    // Exactly one pair, not every bracket at each end: `("/users",
    // methods=["GET"])` legitimately ends in `])`, and stripping greedily
    // ate the list's own bracket and left unbalanced text behind.
    let inner = match (text.chars().next(), text.chars().last()) {
        (Some('('), Some(')')) | (Some('['), Some(']')) => &text[1..text.len() - 1],
        _ => text,
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }
    Some(truncate_chars(inner, MAX_ANNOTATION_ARGS))
}

/// The first argument of a call, when it is a plain string literal.
///
/// `call` is the invocation node; `arguments` is the field name its grammar
/// uses for the argument list (`"arguments"` everywhere the four indexers
/// care about). Returns the literal's text with the surrounding quotes
/// stripped, capped at [`crate::types::MAX_CALL_ARG`].
///
/// Deliberately shallow. It matches a *literal* in first position and
/// nothing else — not a variable, not a concatenation, not an f-string or
/// template with interpolation. The frameworks this exists for
/// (`app.get("/users", h)`, `Router::route("/x", get(h))`) all write the
/// path inline, and anything cleverer would be inventing a route that isn't
/// in the source. All four grammars name the node kind with "string" in it
/// (`string_literal` in Java, `string` in Python/Rust/TypeScript), which is
/// what the kind test keys on.
pub fn first_string_arg(call: &Node, arguments: &str, source: &[u8]) -> Option<String> {
    let args = call.child_by_field_name(arguments)?;
    let first = args.named_child(0)?;
    if !first.kind().contains("string") {
        return None;
    }
    let raw = get_node_text(Some(first), source)?;
    let text = raw
        .trim()
        // Rust byte/raw prefixes and Python's f/r/b prefixes sit outside the
        // quotes; strip the quote characters from both ends and let whatever
        // prefix remains fall away with them.
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '#');
    if text.is_empty() {
        return None;
    }
    Some(truncate_chars(text, crate::types::MAX_CALL_ARG))
}

/// Best-effort docstring extractor for JSDoc-style `/** ... */` blocks placed
/// immediately above a node. Languages that share this convention (TS, JS,
/// Java) get docstring support for free; languages with native docstring
/// conventions (e.g. Python triple-quoted strings) can override this in their
/// own indexer.
pub fn extract_docstring(node: &Node, source: &[u8]) -> Option<String> {
    let start_byte = node.start_byte();
    if start_byte < 6 {
        return None;
    }

    let search_range = 200.min(start_byte);
    let slice = &source[start_byte - search_range..start_byte];

    let start = slice.windows(3).rposition(|w| w == b"/**")?;
    let doc_start = start_byte - search_range + start;
    let doc = &source[doc_start..start_byte];

    if !doc.windows(2).any(|w| w == b"*/") {
        return None;
    }

    let text = String::from_utf8(doc.to_vec()).ok()?;
    let clean = text
        .lines()
        .filter_map(|l| {
            // The comment markers are stripped rather than used to drop the
            // whole line. Discarding any line starting with `/**` meant a
            // single-line `/** Does the thing. */` — the most common JSDoc
            // form there is — yielded nothing at all.
            let line = l.trim();
            let line = line.strip_prefix("/**").unwrap_or(line);
            let line = line.strip_suffix("*/").unwrap_or(line);
            let line = line.trim().trim_start_matches('*').trim();
            if line.is_empty() {
                None
            } else if line.starts_with("@param") {
                let parts: Vec<&str> = line.splitn(2, '-').collect();
                Some(format!(
                    "param: {}",
                    parts.first().unwrap_or(&line).trim().replace("@param", "")
                ))
            } else if line.starts_with("@return") || line.starts_with("@returns") {
                Some(format!(
                    "returns: {}",
                    line.replace("@return", "").replace("@returns", "").trim()
                ))
            } else {
                Some(line.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

/// Is this node kind a control-flow construct that indents its body?
///
/// Covers the four tree-sitter grammars we index. `else_clause` is
/// deliberately excluded: in `if a {} else if b {}` the else wraps another
/// `if`, so counting both would make a flat chain look deeply nested.
fn is_nesting_kind(kind: &str) -> bool {
    matches!(
        kind,
        // conditionals
        "if_statement"
            | "if_expression"
            // loops
            | "for_statement"
            | "for_expression"
            | "for_in_statement"
            | "enhanced_for_statement"
            | "while_statement"
            | "while_expression"
            | "loop_expression"
            | "do_statement"
            // branching
            | "match_expression"
            | "match_statement"
            | "switch_statement"
            | "switch_expression"
            // scoping / error handling
            | "try_statement"
            | "try_expression"
            | "catch_clause"
            | "with_statement"
    )
}

/// Maximum control-flow nesting depth *inside* a function or class body.
///
/// A function whose body is a flat sequence of statements scores 0; one with
/// a loop containing an `if` scores 2. This is the "how hairy is this" signal
/// that `project_overview` reports alongside LOC.
///
/// The previous implementation counted *declaration* kinds
/// (`function_declaration`, `class_declaration`, …) rather than control flow,
/// which meant two things: the number described how deeply nested the
/// declaration itself was rather than its body, and Rust — whose nodes are
/// `function_item` / `struct_item`, absent from that list — always scored 0.
pub fn calculate_nesting(node: &Node) -> u32 {
    fn walk(node: &Node, depth: u32) -> u32 {
        let depth = if is_nesting_kind(node.kind()) {
            depth + 1
        } else {
            depth
        };
        let mut max = depth;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            max = max.max(walk(&child, depth));
        }
        max
    }

    // Start beneath the definition: the function's own node is not nesting.
    let mut max = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        max = max.max(walk(&child, 0));
    }
    max
}

/// Extract a function's return type. Tries the tree-sitter `return_type`
/// field first; falls back to a regex on the function source for grammars
/// that don't surface a dedicated field. The regex is TypeScript-flavoured
/// (`): T`); it benignly fails for languages that use other syntaxes.
pub fn extract_return_type(node: &Node, source: &[u8]) -> Option<String> {
    if let Some(return_type) = node.child_by_field_name("return_type") {
        if let Some(text) = get_node_text(Some(return_type), source) {
            return Some(text.trim_start_matches(':').trim().to_string());
        }
    }

    // Borrowed, and the pattern compiled once. This ran per *function*: it
    // copied the entire body of every function in the repo onto the heap so a
    // regex could look at its first line, and compiled that regex each time.
    // See P10.8 in docs/dev/PERF-TUNING-JOURNEY.md.
    let node_text = node_str(*node, source)?;
    let cap = return_type_regex().captures(node_text)?;
    let return_match = cap.get(1)?;
    let return_type = return_match.as_str().to_string();
    if return_type.is_empty() {
        None
    } else {
        Some(return_type)
    }
}

// The shared `extract_function_calls` that used to live here is gone. It
// recorded the *source text* of each callee expression, which for a chained
// call meant the whole expression — closure bodies included — and left the
// graph builder to recover a name from it by taking the substring after the
// last dot. Every language now emits `CallRef`s naming the callee and, where
// it can, the type the call dispatches on; see `indexer/scope.rs` and each
// language module's `collect_calls`.

/// `): T` — the TypeScript-flavoured return annotation
/// [`extract_return_type`] falls back to. Compiled once: that function runs
/// per symbol, not per file.
fn return_type_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\)\s*:\s*([^\s{]+)").expect("return-type pattern is a literal")
    })
}

/// A loose `name: type` reader for [`extract_params_from_signature`].
/// Compiled once, for the same reason.
fn signature_param_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(\w+)\s*(?::\s*([^\s,=]+))?").expect("param pattern is a literal")
    })
}

/// Regex-based parameter extraction used as a fallback when the AST didn't
/// yield any parameters (e.g. a malformed file or a grammar quirk). Reads the
/// first `( ... )` group from the function source.
pub fn extract_params_from_signature(node_text: &str) -> Vec<Param> {
    let mut params = Vec::new();

    let open = match node_text.find('(') {
        Some(i) => i,
        None => return params,
    };
    let close = match node_text[open..].find(')') {
        Some(i) => i,
        None => return params,
    };
    let args = &node_text[open + 1..open + close];

    for cap in signature_param_regex().captures_iter(args.trim()) {
        if let Some(name_match) = cap.get(1) {
            let name = name_match.as_str().to_string();
            // Skip language keywords that the loose regex would otherwise
            // pick up when a parameter list is empty or contains noise.
            if name.is_empty() || matches!(name.as_str(), "function" | "class" | "interface") {
                continue;
            }
            let param_type = cap.get(2).map(|m| m.as_str().to_string());
            params.push(Param {
                name,
                param_type,
                optional: false,
                default: None,
            });
        }
    }

    params
}

/// True if the file's extension is one we have a registered indexer for.
/// Extension match is case-insensitive — scanners and document
/// exporters routinely produce `.PDF`, `.MD`, etc., and rejecting them
/// at the walker level would silently lose data.
pub fn is_supported_file(path: &Path) -> bool {
    let ext = match path.extension() {
        Some(e) => e.to_str().unwrap_or("").to_ascii_lowercase(),
        None => String::new(),
    };
    SUPPORTED_EXTS.contains(&ext.as_str())
}

/// True if the path passes through one of the always-ignored directories.
///
/// Compared per path *component*, not as a substring. The substring form
/// dropped any file whose path merely *contained* one of the names, so a
/// `src/targeting/` directory read as Maven's `target/`.
pub fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| IGNORED_DIRS.contains(&s))
    })
}

/// Generated build artifacts masquerading as source. Even when committed
/// (so `.gitignore` doesn't cover them), indexing these floods the graph
/// with thousands of minified/bundled symbols that drown real code in both
/// vector search and structural stats.
pub const IGNORED_ARTIFACT_GLOBS: &[&str] = &[
    "*.min.js", "*.min.mjs", "*.min.css",
    "*.bundle.js", "*.bundle.mjs", "*.bundle.css",
    "dist/",
];

/// Exclusion globs applied on top of `.gitignore`: the built-in artifact
/// patterns plus any comma-separated gitignore-style globs from `UG_IGNORE`
/// (e.g. `UG_IGNORE="vendor/,*.generated.ts"`). Uses the walker's override
/// mechanism (a `!` prefix inverts a whitelist entry into an exclusion), so
/// user patterns get full gitignore glob semantics for free.
fn artifact_overrides(root: &str) -> Option<ignore::overrides::Override> {
    let mut b = OverrideBuilder::new(root);
    for pat in IGNORED_ARTIFACT_GLOBS {
        b.add(&format!("!{pat}")).ok()?;
    }
    if let Ok(extra) = std::env::var("UG_IGNORE") {
        for pat in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            // A user typo shouldn't kill the whole scan — skip bad globs.
            let _ = b.add(&format!("!{pat}"));
        }
    }
    b.build().ok()
}

/// Walk `path` honouring `.gitignore` rules and return every supported source
/// file. Hidden files, directories listed in [`IGNORED_DIRS`], build output
/// identified by [`is_build_output_dir`], and artifacts matching
/// [`IGNORED_ARTIFACT_GLOBS`] / `UG_IGNORE` are skipped.
pub fn scan_files(path: &str) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(path);
    builder.hidden(true).git_ignore(true);
    // Applied as an entry filter rather than a glob so a matching directory
    // prunes its whole subtree, and so the sibling-descriptor probe runs
    // once per directory rather than once per file.
    builder.filter_entry(|e| !is_build_output_dir(e.path()));
    if let Some(overrides) = artifact_overrides(path) {
        builder.overrides(overrides);
    }

    builder
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && is_supported_file(e.path()) && !is_ignored_path(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// blake3 content hash of a file. Used by the cached indexer to skip files
/// whose contents haven't changed since the previous run.
pub fn compute_hash(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    Some(blake3::hash(&data).to_hex().to_string())
}

/// Normalize a path string into a canonical form used everywhere downstream:
/// - backslashes → forward slashes
/// - leading `./` stripped, mid-path `./` segments collapsed
/// - `..` collapsed against preceding segments where possible
/// - leading `..` segments preserved (the indexed root may sit above cwd)
///
/// Two different ways to spell the same file (`./docs/A.md`, `docs/A.md`,
/// `docs/./A.md`) all collapse to `docs/A.md` so the graph builder can
/// resolve cross-file links by exact-match lookup.
pub fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    let mut leading_parents: usize = 0;

    for segment in p.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                if !parts.is_empty() {
                    parts.pop();
                } else if !absolute {
                    leading_parents += 1;
                }
            }
            other => parts.push(other),
        }
    }

    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    for _ in 0..leading_parents {
        out.push_str("../");
    }
    out.push_str(&parts.join("/"));
    out
}

/// Strip the repo root prefix from an absolute path, returning a path
/// relative to the repo root. The output format matches `normalize_path`
/// output so cross-file references resolve correctly.
///
/// Example:
///   strip_repo_root("/Users/foo/myrepo/src/foo.ts", "/Users/foo/myrepo")
///   → "src/foo.ts"
pub fn strip_repo_root(absolute_path: &str, repo_root: &str) -> String {
    let normalized = normalize_path(absolute_path);
    let root = normalize_path(repo_root);
    if let Some(stripped) = normalized.strip_prefix(&root) {
        let result = stripped.trim_start_matches('/');
        if result.is_empty() {
            ".".to_string()
        } else {
            result.to_string()
        }
    } else {
        normalized
    }
}

/// Resolve `import_path` to a normalized path string, joining against the
/// source file's directory when the import is relative or bare. Strips any
/// `#fragment` and `?query` suffix the input may carry (markdown anchors,
/// build-tool query strings).
///
/// Absolute imports (`/foo`) are returned normalized but unjoined. Bare
/// specifiers like `lodash` will be joined with the source dir too — that
/// produces a path that won't match anything in the file index, which is
/// exactly the right behaviour for the resolver: package imports get
/// dropped silently.
pub fn resolve_relative(src_file: &str, import_path: &str) -> String {
    let import_path = import_path.split('#').next().unwrap_or(import_path);
    let import_path = import_path.split('?').next().unwrap_or(import_path);

    let normalized = normalize_path(import_path);
    if normalized.starts_with('/') {
        return normalized;
    }

    let src_normalized = normalize_path(src_file);
    let src_dir = match src_normalized.rfind('/') {
        Some(idx) => &src_normalized[..idx],
        None => "",
    };

    if src_dir.is_empty() {
        normalized
    } else {
        normalize_path(&format!("{}/{}", src_dir, normalized))
    }
}

/// For each symbol whose name matches an imported item, attach the
/// corresponding `ImportInfo` so the symbol carries a record of where it
/// came from.
pub fn resolve_import_refs(symbols: &mut [Symbol], imports: &[ImportInfo]) {
    for imp in imports {
        for sym in symbols.iter_mut() {
            for item in &imp.imported {
                if sym.name == item.name {
                    sym.imports.push(ImportInfo {
                        path: imp.path.clone(),
                        imported: vec![item.clone()],
                    });
                }
            }
        }
    }
}


#[cfg(test)]
mod stable_order_tests {
    use super::*;
    use crate::types::ImportedItem;

    fn info(path: &str) -> ImportInfo {
        ImportInfo {
            path: path.to_string(),
            imported: vec![ImportedItem {
                name: "x".to_string(),
                alias: None,
            }],
        }
    }

    /// Every language extractor builds its imports in a `HashMap` and drains
    /// it through this. A `HashMap`'s iteration order is seeded per map, so
    /// draining straight into a `Vec` made `graph.json` differ between two
    /// runs over an unchanged repo — and, because the embedding text is built
    /// from this list and then truncated to a budget, made the keyword index
    /// differ too. See P11.10 in docs/dev/PERF-TUNING-JOURNEY.md.
    #[test]
    fn imports_come_out_sorted_by_path_whatever_order_they_went_in() {
        let paths = ["zeta", "alpha", "mid", "beta", "yankee", "delta"];
        let expected: Vec<String> = {
            let mut p: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
            p.sort();
            p
        };

        // Fresh map each time — a fresh iteration order each time.
        for _ in 0..16 {
            let mut m: HashMap<String, ImportInfo> = HashMap::new();
            for p in paths {
                m.insert(p.to_string(), info(p));
            }
            let got: Vec<String> = imports_in_stable_order(m)
                .into_iter()
                .map(|i| i.path)
                .collect();
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn an_empty_map_yields_an_empty_list() {
        assert!(imports_in_stable_order(HashMap::new()).is_empty());
    }
}
