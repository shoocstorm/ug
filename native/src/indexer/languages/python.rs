//! Python indexer. Handles `.py`.

use crate::indexer::common::{
    calculate_nesting, extract_docstring, extract_function_calls, extract_params_from_signature,
    extract_return_type, get_node_text,
};
use crate::indexer::languages::LanguageIndexer;
use crate::types::{ExportInfo, ImportInfo, ImportedItem, Param, Signature, Symbol, SymbolMetrics};
use std::collections::HashMap;
use tree_sitter::Node;

pub struct PythonIndexer;

impl LanguageIndexer for PythonIndexer {
    fn name(&self) -> &'static str {
        "python"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_python::language()
    }

    fn extract_imports(&self, source: &[u8], _root: Node) -> Vec<ImportInfo> {
        extract_imports_via_regex(source)
    }

    fn extract_exports(&self, _source: &[u8], _root: Node) -> Vec<ExportInfo> {
        // Python has no first-class export concept comparable to JS/TS:
        // anything not prefixed with `_` is publicly accessible. Returning
        // an empty list matches the previous behaviour and leaves room for
        // an `__all__`-based extractor later.
        Vec::new()
    }

    fn extract_symbols(&self, source: &[u8], root: Node) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        visit(root, source, &mut symbols);
        symbols
    }
}

fn visit(node: Node, source: &[u8], symbols: &mut Vec<Symbol>) {
    extract_symbol_from_node(&node, source, symbols);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, symbols);
    }
}

/// Aggregate `from … import …` and bare `import …` statements by source path.
fn extract_imports_via_regex(source: &[u8]) -> Vec<ImportInfo> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut import_lookup: HashMap<String, ImportInfo> = HashMap::new();

    // `from foo.bar import (a, b)` / `from foo import a, b as c`. Two
    // capture groups for the imported names cover the parenthesised and
    // unparenthesised forms.
    if let Ok(re) = regex::Regex::new(
        r#"from\s+(\.[^ ]+|[a-zA-Z_][a-zA-Z0-9_.]*)\s+import\s+(?:\(([^)]+)\)|([a-zA-Z_][a-zA-Z0-9_,\s]*))"#,
    ) {
        for cap in re.captures_iter(source_str) {
            let path = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let names_str = cap
                .get(2)
                .or_else(|| cap.get(3))
                .map(|m| m.as_str())
                .unwrap_or("*");
            let names: Vec<ImportedItem> = names_str
                .split(',')
                .map(|s| {
                    let name = s.trim().split(" as ").next().unwrap_or(s.trim()).to_string();
                    ImportedItem { name, alias: None }
                })
                .filter(|i| !i.name.is_empty())
                .collect();

            if !path.is_empty() {
                import_lookup
                    .entry(path.clone())
                    .and_modify(|info| info.imported.extend(names.clone()))
                    .or_insert(ImportInfo {
                        path,
                        imported: names,
                    });
            }
        }
    }

    // `import foo` / `import foo.bar`. The `from`-filter is a defensive
    // guard against the regex matching the tail of `from foo import ...`
    // lines that the previous regex already handled.
    if let Ok(re) = regex::Regex::new(r#"import\s+([a-zA-Z_][a-zA-Z0-9_.]*)"#) {
        for cap in re.captures_iter(source_str) {
            let path = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !path.is_empty() && !path.contains("from") {
                import_lookup.entry(path.clone()).or_insert_with(|| ImportInfo {
                    path: path.clone(),
                    imported: vec![ImportedItem {
                        name: path.split('.').last().unwrap_or(&path).to_string(),
                        alias: None,
                    }],
                });
            }
        }
    }

    import_lookup.into_values().collect()
}

fn extract_symbol_from_node(node: &Node, source: &[u8], out: &mut Vec<Symbol>) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;

    match kind {
        "function_definition" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            let params = extract_params(node, source);
            let return_type = extract_return_type(node, source);
            let calls = extract_function_calls(node, source);
            let docstring = extract_docstring(node, source);
            let metrics = SymbolMetrics {
                loc: end.saturating_sub(start),
                params: params.len() as u32,
                max_nesting: calculate_nesting(node),
            };

            out.push(Symbol {
                id: format!("fn:{}:{}", start, name),
                name,
                kind: "function".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring,
                signature: Some(Signature {
                    params,
                    return_type,
                }),
                imports: Vec::new(),
                exports: Vec::new(),
                extends: Vec::new(),
                implements: Vec::new(),
                calls,
                metrics: Some(metrics),
            });
        }
        "class_definition" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("class:{}:{}", start, name),
                name,
                kind: "class".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring: extract_docstring(node, source),
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends: extract_extends(node, source),
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "assignment" => {
            // Module-level assignments like `X = 1` get captured as their
            // own symbols so the indexer surfaces top-level constants.
            let Some(target) = node.child_by_field_name("target") else {
                return;
            };
            let Some(name) = get_node_text(Some(target), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("assign:{}:{}", start, name),
                name,
                kind: "assignment".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
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
        _ => {}
    }
}

/// Collect parameters from a `def …` node. Walks each `parameter` child of
/// the `parameters` field, then falls back to a regex on the source if
/// nothing came out of the AST.
fn extract_params(node: &Node, source: &[u8]) -> Vec<Param> {
    let mut params = Vec::new();

    if let Some(params_node) = node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.children(&mut cursor) {
            if child.kind() != "parameter" {
                continue;
            }
            let name =
                get_node_text(child.child_by_field_name("name"), source).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let default = get_node_text(child.child_by_field_name("default"), source);
            let optional = default.is_some();

            params.push(Param {
                name,
                param_type: None,
                optional,
                default,
            });
        }
    }

    if params.is_empty() {
        if let Some(node_text) = get_node_text(Some(*node), source) {
            params = extract_params_from_signature(&node_text);
        }
    }

    params
}

/// `class Foo(Bar, Baz):` -> `["Bar", "Baz"]`. Reads each child of the
/// `bases` field directly so both single and multi-base forms work.
fn extract_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();
    if let Some(bases) = node.child_by_field_name("bases") {
        let mut cursor = bases.walk();
        for child in bases.children(&mut cursor) {
            if let Some(name) = get_node_text(Some(child), source) {
                extends.push(name);
            }
        }
    }
    extends
}
