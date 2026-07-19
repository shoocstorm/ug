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
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// File extensions we are willing to index. Add new entries when registering
/// a new language indexer in `super::languages`. `pdf` and the Word/Excel/
/// PowerPoint extensions are special-cased in `indexer::process_file` —
/// they're binary, so they bypass the tree-sitter pipeline and are handled
/// by `indexer::document::process_document`. Keep this list in sync with
/// `document::is_supported_ext`.
pub const SUPPORTED_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "py", "java", "rs", "md", "mdx", "markdown", "pdf",
    // Word
    "doc", "docx", "docm", "dot", "dotm", "dotx", "odt", "ott", "rtf",
    // Excel
    "xls", "xlsx", "xlsm", "xlsb", "ods", "ots",
    // PowerPoint
    "ppt", "pptx", "pptm", "pot", "potm", "potx", "odp", "otp",
];

/// Directory names that are always skipped during the file walk.
pub const IGNORED_DIRS: &[&str] = &["node_modules", ".git", "target"];

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
            let line = l.trim().trim_start_matches('*').trim();
            if line.is_empty() || line.starts_with("/**") || line.starts_with("*/") {
                None
            } else if line.starts_with("@param") {
                let parts: Vec<&str> = line.splitn(2, '-').collect();
                Some(format!(
                    "param: {}",
                    parts.get(0).unwrap_or(&line).trim().replace("@param", "")
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

/// Approximate the maximum nesting depth reachable beneath a function/class
/// definition. Cheap heuristic: only major scope-defining node kinds across
/// our supported languages are counted.
pub fn calculate_nesting(node: &Node) -> u32 {
    let mut max_nesting: u32 = 0;
    let mut current_nesting: u32 = 0;

    let kind = node.kind();
    if matches!(
        kind,
        "function_declaration"
            | "function_definition"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "class_declaration"
            | "class_definition"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
    ) {
        current_nesting += 1;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_nesting = calculate_nesting(&child);
        if child_nesting > max_nesting {
            max_nesting = child_nesting;
        }
    }

    current_nesting + max_nesting
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

    let node_text = get_node_text(Some(*node), source)?;
    let return_re = regex::Regex::new(r"\)\s*:\s*([^\s{]+)").ok()?;
    let cap = return_re.captures(&node_text)?;
    let return_match = cap.get(1)?;
    let return_type = return_match.as_str().to_string();
    if return_type.is_empty() {
        None
    } else {
        Some(return_type)
    }
}

/// Collect every callee name reachable anywhere beneath `node`. Recurses into
/// nested blocks so calls inside `if`/loops/closures are captured. Looks for
/// `call_expression` (TypeScript/JavaScript), `call` (Python),
/// `method_invocation` and `object_creation_expression` (Java).
pub fn extract_function_calls(node: &Node, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    collect_calls(node, source, &mut calls);
    calls
}

fn collect_calls(node: &Node, source: &[u8], calls: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "call_expression" | "call" => {
                let func_node = child
                    .child_by_field_name("function")
                    .or_else(|| child.child_by_field_name("callee"));
                if let Some(func) = func_node {
                    if let Some(name) = get_node_text(Some(func), source) {
                        if !calls.contains(&name) {
                            calls.push(name);
                        }
                    }
                }
            }
            "method_invocation" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    if let Some(name) = get_node_text(Some(name_node), source) {
                        if !calls.contains(&name) {
                            calls.push(name);
                        }
                    }
                }
            }
            "object_creation_expression" => {
                if let Some(type_node) = child.child_by_field_name("type") {
                    if let Some(name) = get_node_text(Some(type_node), source) {
                        if !calls.contains(&name) {
                            calls.push(name);
                        }
                    }
                }
            }
            _ => {}
        }
        collect_calls(&child, source, calls);
    }
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

    let re = match regex::Regex::new(r"(\w+)\s*(?::\s*([^\s,=]+))?") {
        Ok(r) => r,
        Err(_) => return params,
    };

    for cap in re.captures_iter(args.trim()) {
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
pub fn is_ignored_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    IGNORED_DIRS.iter().any(|&d| path_str.contains(d))
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
/// file. Hidden files, directories listed in `IGNORED_DIRS`, and build
/// artifacts matching `IGNORED_ARTIFACT_GLOBS` / `UG_IGNORE` are skipped.
pub fn scan_files(path: &str) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(path);
    builder.hidden(true).git_ignore(true);
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
