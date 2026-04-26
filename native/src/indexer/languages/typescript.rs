//! TypeScript / JavaScript indexer. Handles `.ts`, `.tsx`, `.js`, `.jsx`.
//!
//! The TypeScript grammar covers JavaScript as a superset, so a single
//! tree-sitter parser is reused for all four extensions.

use crate::indexer::common::{
    calculate_nesting, extract_docstring, extract_function_calls, extract_params_from_signature,
    extract_return_type, get_node_text,
};
use crate::indexer::languages::LanguageIndexer;
use crate::types::{
    ExportInfo, ImportInfo, ImportedItem, Param, Signature, Symbol, SymbolMetrics, TypeRef,
};
use std::collections::HashMap;
use tree_sitter::Node;

pub struct TypeScriptIndexer;

impl LanguageIndexer for TypeScriptIndexer {
    fn name(&self) -> &'static str {
        "typescript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "js", "jsx"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_typescript::language_typescript()
    }

    fn extract_imports(&self, source: &[u8], _root: Node) -> Vec<ImportInfo> {
        // Imports are extracted via regex rather than the AST: it's faster
        // and resilient to grammar version drift in tree-sitter-typescript.
        extract_imports_via_regex(source)
    }

    fn extract_exports(&self, source: &[u8], root: Node) -> Vec<ExportInfo> {
        extract_exports_from_ast(&root, source)
    }

    fn extract_symbols(&self, source: &[u8], root: Node) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        visit(root, source, &mut symbols);
        symbols
    }
}

/// Recursive AST walk. Each node is offered to `extract_symbol_from_node`,
/// then we descend into every child unconditionally - nested classes /
/// functions all surface as their own symbols.
fn visit(node: Node, source: &[u8], symbols: &mut Vec<Symbol>) {
    extract_symbol_from_node(&node, source, symbols);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, symbols);
    }
}

/// Aggregate every `import` / `import type` statement in the file by source
/// path. The two regexes overlap intentionally: the second catches the
/// type-only form which the first won't match.
fn extract_imports_via_regex(source: &[u8]) -> Vec<ImportInfo> {
    let source_str = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut import_lookup: HashMap<String, ImportInfo> = HashMap::new();

    // `import { a, b as c } from 'x'`, `import * as ns from 'x'`,
    // `import x from 'y'`.
    if let Ok(re) = regex::Regex::new(
        r#"import\s+(?:\{([^}]+)\}|\*\s+as\s+(\w+)|(\w+))\s+from\s+['"]([^'"]+)['"]"#,
    ) {
        for cap in re.captures_iter(source_str) {
            let names = if let Some(matched) = cap.get(1) {
                // Named imports: split the brace contents on commas.
                matched
                    .as_str()
                    .split(',')
                    .map(|s| {
                        let name = s.trim().split(" as ").next().unwrap_or(s.trim()).to_string();
                        ImportedItem { name, alias: None }
                    })
                    .collect::<Vec<_>>()
            } else if let Some(alias) = cap.get(2) {
                // `* as ns` namespace import.
                vec![ImportedItem {
                    name: alias.as_str().to_string(),
                    alias: None,
                }]
            } else if let Some(name) = cap.get(3) {
                // Default import.
                vec![ImportedItem {
                    name: name.as_str().to_string(),
                    alias: None,
                }]
            } else {
                Vec::new()
            };

            let path = cap
                .get(4)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !path.is_empty() {
                merge_import(&mut import_lookup, path, names);
            }
        }
    }

    // `import type { X } from 'y'`.
    if let Ok(re) = regex::Regex::new(r#"import\s+type\s+\{([^}]+)\}\s+from\s+['"]([^'"]+)['"]"#) {
        for cap in re.captures_iter(source_str) {
            let names = cap
                .get(1)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|s| {
                            let name =
                                s.trim().split(" as ").next().unwrap_or(s.trim()).to_string();
                            ImportedItem { name, alias: None }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let path = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !path.is_empty() {
                merge_import(&mut import_lookup, path, names);
            }
        }
    }

    import_lookup.into_values().collect()
}

fn merge_import(
    lookup: &mut HashMap<String, ImportInfo>,
    path: String,
    names: Vec<ImportedItem>,
) {
    lookup
        .entry(path.clone())
        .and_modify(|info| info.imported.extend(names.clone()))
        .or_insert(ImportInfo {
            path,
            imported: names,
        });
}

/// Walk top-level `export` clauses and `export … from '…'` re-exports.
fn extract_exports_from_ast(node: &Node, source: &[u8]) -> Vec<ExportInfo> {
    let mut exports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "export_clause" => collect_export_specifiers(&child, source, &mut exports),
            "re_export_statement" | "export_statement" => {
                // Only treat this as a re-export when there's a `source`
                // field; otherwise it's something like `export const x = …`
                // which we surface as a regular symbol elsewhere.
                if let Some(source_node) = child.child_by_field_name("source") {
                    let re_export_path =
                        get_node_text(Some(source_node), source).unwrap_or_default();
                    if !re_export_path.is_empty() {
                        collect_export_specifiers(&child, source, &mut exports);
                    }
                }
            }
            _ => {}
        }
    }
    exports
}

fn collect_export_specifiers(node: &Node, source: &[u8], exports: &mut Vec<ExportInfo>) {
    let mut cursor = node.walk();
    for spec in node.children(&mut cursor) {
        if spec.kind() != "export_specifier" {
            continue;
        }
        let name = get_node_text(spec.child_by_field_name("name"), source).unwrap_or_default();
        let alias = spec
            .child_by_field_name("alias")
            .and_then(|n| get_node_text(Some(n), source));
        exports.push(ExportInfo {
            name,
            alias,
            is_default: false,
        });
    }
}

/// If `node` is a TS/JS top-level construct we care about (function, class,
/// interface, variable, type alias), append the matching `Symbol` to `out`.
fn extract_symbol_from_node(node: &Node, source: &[u8], out: &mut Vec<Symbol>) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;

    match kind {
        "function_declaration" | "method_definition" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            let params = extract_params(node, source);
            let return_type = extract_return_type(node, source);
            let calls = extract_function_calls(node, source);
            let extends = extract_extends(node, source);
            let implements = extract_implements(node, source);
            let docstring = extract_docstring(node, source);
            let metrics = SymbolMetrics {
                loc: end.saturating_sub(start),
                params: params.len() as u32,
                max_nesting: calculate_nesting(node),
            };

            out.push(Symbol {
                id: format!("fn:{}:{}", start, name),
                name,
                kind: kind.to_string(),
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
                extends,
                implements,
                calls,
                metrics: Some(metrics),
            });
        }
        "class_declaration" => {
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
                implements: extract_implements(node, source),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "interface_declaration" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // Members are computed but not yet surfaced on `Symbol` - kept
            // behind `_members` to make future wiring obvious.
            let _members = extract_interface_members(node, source);
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
                extends: extract_extends(node, source),
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "variable_declaration" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
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
        "type_alias_declaration" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("type:{}:{}", start, name),
                name,
                kind: "type".to_string(),
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
        _ => {}
    }
}

/// Collect parameters from a function-like node. Walks the `parameters`
/// field for each TS-specific parameter node kind, then falls back to a
/// regex over the source if the AST yielded nothing.
fn extract_params(node: &Node, source: &[u8]) -> Vec<Param> {
    let mut params = Vec::new();

    if let Some(params_node) = node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.children(&mut cursor) {
            if !matches!(
                child.kind(),
                "required_parameter" | "optional_parameter" | "rest_parameter"
            ) {
                continue;
            }
            let name =
                get_node_text(child.child_by_field_name("name"), source).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let param_type = get_node_text(child.child_by_field_name("type"), source);
            let optional = child.kind() == "optional_parameter";
            let default = get_node_text(child.child_by_field_name("default"), source);

            params.push(Param {
                name,
                param_type,
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

/// Read the `superclass` field: `class Foo extends Bar { … }` -> `["Bar"]`.
fn extract_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();
    if let Some(superclass) = node.child_by_field_name("superclass") {
        if let Some(name) = get_node_text(Some(superclass), source) {
            extends.push(name);
        }
    }
    extends
}

/// Read the `protocols` field: `class Foo implements I1, I2 { … }` ->
/// `["I1", "I2"]`.
fn extract_implements(node: &Node, source: &[u8]) -> Vec<String> {
    let mut implements = Vec::new();
    if let Some(protocols) = node.child_by_field_name("protocols") {
        let mut cursor = protocols.walk();
        for child in protocols.children(&mut cursor) {
            if let Some(name) = get_node_text(Some(child), source) {
                implements.push(name);
            }
        }
    }
    implements
}

/// Pull property and method signatures out of an interface body. Currently
/// computed but not surfaced on `Symbol`; kept as a building block for an
/// upcoming richer type model.
#[allow(dead_code)]
fn extract_interface_members(node: &Node, source: &[u8]) -> Vec<TypeRef> {
    let mut members = Vec::new();

    let Some(body) = node.child_by_field_name("body") else {
        return members;
    };

    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "property_signature" => {
                let name = get_node_text(child.child_by_field_name("name"), source)
                    .unwrap_or_default();
                let mut type_refs = extract_type_refs(&child, source);
                if let Some(tr) = type_refs.pop() {
                    members.push(TypeRef {
                        name: format!("{}: {}", name, tr.name),
                        generic: tr.generic,
                    });
                } else {
                    members.push(TypeRef {
                        name,
                        generic: None,
                    });
                }
            }
            "method_signature" => {
                let name = get_node_text(child.child_by_field_name("name"), source)
                    .unwrap_or_default();
                let params = extract_params(&child, source);
                let return_type = extract_return_type(&child, source);

                let sig = format!(
                    "{}({}) => {}",
                    name,
                    params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    return_type.unwrap_or_default()
                );
                members.push(TypeRef {
                    name: sig,
                    generic: None,
                });
            }
            _ => {}
        }
    }
    members
}

/// Collect type annotations attached to children of `node`. Currently
/// dormant; only used by `extract_interface_members`.
#[allow(dead_code)]
fn extract_type_refs(node: &Node, source: &[u8]) -> Vec<TypeRef> {
    let mut type_refs = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_annotation" | "attribute" => {
                if let Some(type_node) = child.child_by_field_name("type") {
                    if let Some(type_str) = get_node_text(Some(type_node), source) {
                        // Split off the generic parameters: `Array<T>` ->
                        // (`Array`, `T`).
                        let parts: Vec<&str> = type_str.splitn(2, '<').collect();
                        let name = parts[0].to_string();
                        let generic =
                            parts.get(1).map(|s| s.trim_end_matches('>').to_string());
                        type_refs.push(TypeRef { name, generic });
                    }
                }
            }
            "variable_declarator" => {
                if let Some(type_node) = child.child_by_field_name("type") {
                    if let Some(type_str) = get_node_text(Some(type_node), source) {
                        type_refs.push(TypeRef {
                            name: type_str,
                            generic: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    type_refs
}
