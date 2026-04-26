//! Language-agnostic helpers shared by every language indexer.
//!
//! Anything in this file is intended to be reusable as new languages are
//! plugged in. The functions here only depend on tree-sitter, blake3 and the
//! filesystem - they know nothing about TypeScript, Python or any specific
//! grammar. When adding Java/Go/etc., prefer extending these helpers rather
//! than copying logic into the language module.

use crate::types::{ImportInfo, Param, Symbol};
use ignore::WalkBuilder;
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// File extensions we are willing to index. Add new entries when registering
/// a new language indexer in `super::languages`.
pub const SUPPORTED_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "py"];

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
            | "class_declaration"
            | "class_definition"
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
/// `call_expression` (TypeScript/JavaScript) and `call` (Python).
pub fn extract_function_calls(node: &Node, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    collect_calls(node, source, &mut calls);
    calls
}

fn collect_calls(node: &Node, source: &[u8], calls: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "call_expression" | "call") {
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
pub fn is_supported_file(path: &Path) -> bool {
    let ext = match path.extension() {
        Some(e) => e.to_str().unwrap_or(""),
        None => "",
    };
    SUPPORTED_EXTS.contains(&ext)
}

/// True if the path passes through one of the always-ignored directories.
pub fn is_ignored_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    IGNORED_DIRS.iter().any(|&d| path_str.contains(d))
}

/// Walk `path` honouring `.gitignore` rules and return every supported source
/// file. Hidden files and directories listed in `IGNORED_DIRS` are skipped.
pub fn scan_files(path: &str) -> Vec<PathBuf> {
    let walker = WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .build();

    walker
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
