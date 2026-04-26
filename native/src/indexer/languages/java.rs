//! Java indexer. Handles `.java`.
//!
//! Maps Java AST node kinds onto the same normalised symbol kinds used by
//! the Python indexer (`function`, `class`, `interface`, `variable`) so the
//! graph builder doesn't need a Java-specific branch:
//!
//! - `class_declaration` / `enum_declaration` / `record_declaration` -> class
//! - `interface_declaration` -> interface
//! - `method_declaration` / `constructor_declaration` -> function
//! - `field_declaration` -> variable (one per declarator, so `int a, b;` -> 2)
//!
//! Imports are extracted via regex (clean grammar, fast scan); exports are
//! always empty - Java uses `public`/`protected`/etc. rather than an
//! explicit export concept, mirroring the Python indexer.

use crate::indexer::common::{
    calculate_nesting, extract_docstring, extract_function_calls, extract_params_from_signature,
    get_node_text,
};
use crate::indexer::languages::LanguageIndexer;
use crate::types::{ExportInfo, ImportInfo, ImportedItem, Param, Signature, Symbol, SymbolMetrics};
use std::collections::HashMap;
use tree_sitter::Node;

pub struct JavaIndexer;

impl LanguageIndexer for JavaIndexer {
    fn name(&self) -> &'static str {
        "java"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["java"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_java::language()
    }

    fn extract_imports(&self, source: &[u8], _root: Node) -> Vec<ImportInfo> {
        extract_imports_via_regex(source)
    }

    fn extract_exports(&self, _source: &[u8], _root: Node) -> Vec<ExportInfo> {
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

/// Aggregate `import a.b.C;`, `import a.b.*;` and `import static a.b.C.x;`
/// statements by source path. The package portion (`a.b`) becomes the
/// `ImportInfo.path`, the trailing identifier (`C` or `x`) the imported
/// name. Wildcard imports use `*` as the name.
fn extract_imports_via_regex(source: &[u8]) -> Vec<ImportInfo> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut by_path: HashMap<String, ImportInfo> = HashMap::new();

    let re = match regex::Regex::new(
        r#"import\s+(?:static\s+)?([a-zA-Z_][\w.]*)(\s*\.\s*\*)?\s*;"#,
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    for cap in re.captures_iter(source_str) {
        let full = match cap.get(1) {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };
        let is_wildcard = cap.get(2).is_some();

        let (path, name) = if is_wildcard {
            (full.clone(), "*".to_string())
        } else {
            // Split on the final `.` so `path` is the qualifier and `name`
            // is the imported identifier. A no-dot import (rare in Java but
            // syntactically possible) keeps the whole string as both path
            // and name so the symbol is still indexable.
            match full.rfind('.') {
                Some(idx) => (full[..idx].to_string(), full[idx + 1..].to_string()),
                None => (full.clone(), full.clone()),
            }
        };

        if path.is_empty() {
            continue;
        }
        let item = ImportedItem { name, alias: None };
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

fn extract_symbol_from_node(node: &Node, source: &[u8], out: &mut Vec<Symbol>) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;

    match kind {
        "method_declaration" | "constructor_declaration" => {
            // Constructors lack a `name` field on some grammar versions -
            // walk back up to the enclosing class for a fallback name.
            let name = get_node_text(node.child_by_field_name("name"), source).or_else(|| {
                if kind == "constructor_declaration" {
                    find_enclosing_type_name(node, source)
                } else {
                    None
                }
            });
            let Some(name) = name else { return };

            let params = extract_params(node, source);
            let return_type = get_node_text(node.child_by_field_name("type"), source);
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
        "class_declaration" | "enum_declaration" | "record_declaration" => {
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
                extends: extract_class_extends(node, source),
                implements: extract_class_implements(node, source),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "interface_declaration" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("interface:{}:{}", start, name),
                name,
                kind: "interface".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring: extract_docstring(node, source),
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends: extract_interface_extends(node, source),
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "field_declaration" => {
            // A single `field_declaration` can declare multiple variables
            // (`int a, b, c;`); walk every `variable_declarator` so each
            // becomes its own symbol.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name) = get_node_text(child.child_by_field_name("name"), source) else {
                    continue;
                };
                out.push(Symbol {
                    id: format!("var:{}:{}", start, name),
                    name,
                    kind: "variable".to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: extract_docstring(node, source),
                    signature: None,
                    imports: Vec::new(),
                    exports: Vec::new(),
                    extends: Vec::new(),
                    implements: Vec::new(),
                    calls: Vec::new(),
                    metrics: None,
                });
            }
        }
        _ => {}
    }
}

/// Walk parent links until we hit a class/enum/record/interface declaration
/// and return its `name` field text. Used as a fallback for constructors
/// whose own `name` field is missing on some grammar versions.
fn find_enclosing_type_name(node: &Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(n) = current {
        if matches!(
            n.kind(),
            "class_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "interface_declaration"
        ) {
            return get_node_text(n.child_by_field_name("name"), source);
        }
        current = n.parent();
    }
    None
}

/// Walk the `parameters` field, picking up each `formal_parameter` /
/// `spread_parameter`. Falls back to a regex over the source if the AST
/// yielded nothing (e.g. a malformed file).
fn extract_params(node: &Node, source: &[u8]) -> Vec<Param> {
    let mut params = Vec::new();

    if let Some(params_node) = node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.children(&mut cursor) {
            if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
                continue;
            }
            let name =
                get_node_text(child.child_by_field_name("name"), source).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let param_type = get_node_text(child.child_by_field_name("type"), source);

            params.push(Param {
                name,
                param_type,
                optional: false,
                default: None,
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

/// `class Foo extends Bar { … }` -> `["Bar"]`. Java only allows a single
/// superclass; the trait still returns a `Vec<String>` for symmetry.
fn extract_class_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();
    if let Some(superclass) = node.child_by_field_name("superclass") {
        if let Some(text) = get_node_text(Some(superclass), source) {
            // The `superclass` node's text is `extends X` - strip the keyword.
            let stripped = text.trim_start_matches("extends").trim();
            if !stripped.is_empty() {
                extends.push(stripped.to_string());
            }
        }
    }
    extends
}

/// `class Foo implements I1, I2 { … }` -> `["I1", "I2"]`.
fn extract_class_implements(node: &Node, source: &[u8]) -> Vec<String> {
    let mut implements = Vec::new();
    if let Some(interfaces) = node.child_by_field_name("interfaces") {
        if let Some(text) = get_node_text(Some(interfaces), source) {
            let stripped = text.trim_start_matches("implements").trim();
            for part in stripped.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    implements.push(p.to_string());
                }
            }
        }
    }
    implements
}

/// `interface Foo extends Bar, Baz { … }` -> `["Bar", "Baz"]`. The named
/// field is `extends_interfaces` on most grammar versions but `extends` on a
/// few older ones; try both.
fn extract_interface_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();
    let target = node
        .child_by_field_name("extends_interfaces")
        .or_else(|| node.child_by_field_name("extends"));
    if let Some(ext) = target {
        if let Some(text) = get_node_text(Some(ext), source) {
            let stripped = text.trim_start_matches("extends").trim();
            for part in stripped.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    extends.push(p.to_string());
                }
            }
        }
    }
    extends
}
