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
                // A re-export carries a `source`; its specifiers name what
                // is being forwarded.
                // Either form keeps its specifiers in an `export_clause`
                // child rather than directly on the statement, so that is
                // what gets walked. `export * from '…'` names nothing and
                // contributes no specifiers.
                if let Some(clause) = child_of_kind(&child, "export_clause") {
                    collect_export_specifiers(&clause, source, &mut exports);
                    continue;
                }
                // `export function f() {}` / `export default class X {}`.
                //
                // This arm used to be missing entirely: the comment said such
                // a form was "surfaced as a regular symbol elsewhere", which
                // is true of the *symbol* but left `FileNode.exports` empty
                // for every TypeScript file — so the graph's `Exports` edges,
                // and the classifier's export-shape fallback, had nothing to
                // work with.
                if let Some(decl) = child.child_by_field_name("declaration") {
                    let is_default = child_of_kind(&child, "default").is_some();
                    for name in declared_names(&decl, source) {
                        exports.push(ExportInfo {
                            name,
                            alias: None,
                            is_default,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    exports
}

/// The name(s) a declaration introduces. A `lexical_declaration` can bind
/// several at once (`export const a = 1, b = 2`); everything else binds one.
fn declared_names(decl: &Node, source: &[u8]) -> Vec<String> {
    match decl.kind() {
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = decl.walk();
            decl.named_children(&mut cursor)
                .filter(|c| c.kind() == "variable_declarator")
                .filter_map(|c| get_node_text(c.child_by_field_name("name"), source))
                .collect()
        }
        _ => get_node_text(decl.child_by_field_name("name"), source)
            .into_iter()
            .collect(),
    }
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
                // Inclusive of both the first and last line, matching the
                // span fallback used for symbols that carry no metrics.
                // These two disagreed by one before, which made `loc` mean
                // subtly different things for a Function and a Class.
                loc: end.saturating_sub(start) + 1,
                params: params.len() as u32,
                max_nesting: calculate_nesting(node),
                // Comment/doc/code counts are filled in one shared pass
                // over the file — see `indexer::annotate_line_metrics`.
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            });
        }
        // `const foo = () => …` and `let x = 1` at module level.
        //
        // `lexical_declaration` was absent here, and `variable_declaration`
        // has no `name` field to read — so no `const`/`let`/`var` binding
        // ever became a symbol. That is the *common* shape of a function in
        // this language: `graph.rs` maps the `variable` kind onto a Function
        // node precisely because of it, and had nothing to map.
        //
        // Restricted to top level on purpose. The walk descends into every
        // body, so emitting a symbol per declaration anywhere would turn
        // each local into a graph node and bury the module's real surface.
        "lexical_declaration" | "variable_declaration" if is_top_level(node) => {
            let mut cursor = node.walk();
            let declarators: Vec<Node> = node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "variable_declarator")
                .collect();
            for decl in declarators {
                let Some(name) = get_node_text(decl.child_by_field_name("name"), source) else {
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
                    calls: extract_function_calls(&decl, source),
                    metrics: None,
                    ..Default::default()
                });
            }
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
                ..Default::default()
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
        for child in params_node.named_children(&mut cursor) {
            if !matches!(
                child.kind(),
                "required_parameter" | "optional_parameter" | "rest_parameter"
            ) {
                continue;
            }
            // The parameter's binding has no `name` field on this grammar —
            // it is simply the first named child. Reading a `name` field
            // always came back empty, which emptied the whole AST branch and
            // sent every function to the regex fallback below; that regex
            // reads any word inside the first `(...)`, so `f(a, b = 1)` came
            // out with three parameters: `a`, `b` and `1`.
            let Some(name) = get_node_text(child.named_child(0), source) else {
                continue;
            };
            let name = name.trim().to_string();
            if name.is_empty() {
                continue;
            }
            // `type_annotation` text carries its leading colon.
            let param_type = get_node_text(child.child_by_field_name("type"), source)
                .map(|t| t.trim_start_matches(':').trim().to_string())
                .filter(|t| !t.is_empty());
            let default = default_value(&child, source);

            params.push(Param {
                name,
                param_type,
                optional: child.kind() == "optional_parameter" || default.is_some(),
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

/// Is this declaration part of the module's own surface, rather than a
/// local inside some body? Accepts both the bare form and the one wrapped
/// in an `export_statement`.
fn is_top_level(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    match parent.kind() {
        "program" => true,
        "export_statement" => parent
            .parent()
            .is_some_and(|grandparent| grandparent.kind() == "program"),
        _ => false,
    }
}

/// The initialiser of a defaulted parameter, i.e. whatever follows the `=`.
///
/// There is no `default` field to read: the grammar labels the `=` token
/// itself `value`, so the initialiser is the named node that comes after it.
fn default_value(param: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = param.walk();
    let children: Vec<Node> = param.children(&mut cursor).collect();
    let eq = children.iter().position(|c| c.kind() == "=")?;
    children
        .iter()
        .skip(eq + 1)
        .find(|c| c.is_named())
        .and_then(|c| get_node_text(Some(*c), source))
}

/// `class Foo extends Bar { … }` -> `["Bar"]`, and
/// `interface Foo extends A, B { … }` -> `["A", "B"]`.
///
/// The grammar exposes neither through a field. A class's heritage is an
/// unnamed `class_heritage` child holding an `extends_clause`; an
/// interface's is an `extends_type_clause`. This used to read a
/// `superclass` field that no version emits, so **no TypeScript class or
/// interface has ever contributed an `Extends` edge**.
fn extract_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();

    if let Some(heritage) = child_of_kind(node, "class_heritage") {
        if let Some(clause) = child_of_kind(&heritage, "extends_clause") {
            push_heritage_types(&clause, source, &mut extends);
        }
    }
    if let Some(clause) = child_of_kind(node, "extends_type_clause") {
        push_heritage_types(&clause, source, &mut extends);
    }
    extends
}

/// `class Foo implements I1, I2 { … }` -> `["I1", "I2"]`.
fn extract_implements(node: &Node, source: &[u8]) -> Vec<String> {
    let mut implements = Vec::new();
    if let Some(heritage) = child_of_kind(node, "class_heritage") {
        if let Some(clause) = child_of_kind(&heritage, "implements_clause") {
            push_heritage_types(&clause, source, &mut implements);
        }
    }
    implements
}

fn child_of_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

/// Collect the type names named by an `extends` / `implements` clause.
///
/// Only named children are walked, so the keyword and the commas stay out.
/// A parameterised supertype (`extends Base<T>`) reduces to `Base`: the
/// graph resolves supertypes by declared name, and the generic form matches
/// none of them.
fn push_heritage_types(clause: &Node, source: &[u8], out: &mut Vec<String>) {
    let mut cursor = clause.walk();
    for child in clause.named_children(&mut cursor) {
        // `type_arguments` is a sibling of the name inside a `generic_type`,
        // and also appears directly under an `extends_clause`.
        if child.kind() == "type_arguments" {
            continue;
        }
        let Some(text) = get_node_text(Some(child), source) else {
            continue;
        };
        let base = text.split('<').next().unwrap_or(&text).trim();
        if !base.is_empty() && !out.iter().any(|e| e == base) {
            out.push(base.to_string());
        }
    }
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


#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> (Vec<Symbol>, Vec<ImportInfo>, Vec<ExportInfo>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_typescript::language_typescript())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        let idx = TypeScriptIndexer;
        (
            idx.extract_symbols(src.as_bytes(), root),
            idx.extract_imports(src.as_bytes(), root),
            idx.extract_exports(src.as_bytes(), root),
        )
    }

    fn find<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        symbols.iter().find(|s| s.name == name).unwrap_or_else(|| {
            let all: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
            panic!("no symbol named {name} in {all:?}")
        })
    }

    fn names(symbols: &[Symbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.name.as_str()).collect()
    }

    // ---- registration ----------------------------------------------------

    #[test]
    fn one_grammar_serves_all_four_extensions() {
        // The TS grammar is a superset of JS, which is why `.js` and `.jsx`
        // route here rather than to a parser of their own.
        assert_eq!(TypeScriptIndexer.name(), "typescript");
        assert_eq!(TypeScriptIndexer.extensions(), &["ts", "tsx", "js", "jsx"]);
    }

    // ---- symbols ---------------------------------------------------------

    #[test]
    fn a_function_carries_its_signature_and_metrics() {
        let (symbols, _, _) = parse(
            r#"
function greet(name: string, times = 1): string {
    if (times) {
        for (const t of []) { emit(t); }
    }
    return name;
}
"#,
        );
        let f = find(&symbols, "greet");
        assert_eq!(f.kind, "function_declaration");

        let sig = f.signature.as_ref().expect("signature");
        // Regression: the AST branch read a `name` field the grammar does
        // not expose, so every function fell through to the loose regex —
        // which read `1` in `times = 1` as a third parameter.
        assert_eq!(
            sig.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["name", "times"]
        );
        assert_eq!(sig.params[0].param_type.as_deref(), Some("string"));
        assert_eq!(sig.params[1].default.as_deref(), Some("1"));
        assert!(sig.params[1].optional, "a default makes it optional");
        assert_eq!(sig.return_type.as_deref(), Some("string"));

        let m = f.metrics.as_ref().unwrap();
        assert_eq!(m.params, 2);
        assert_eq!(m.max_nesting, 2, "a for inside an if is depth 2");
    }

    #[test]
    fn optional_and_rest_parameters_are_read() {
        let (symbols, _, _) = parse("function f(a?: number, ...rest: string[]) {}\n");
        let sig = find(&symbols, "f").signature.as_ref().unwrap();
        // The rest parameter keeps its spread so the rendered signature
        // reads the way the source does.
        assert_eq!(
            sig.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "...rest"]
        );
        assert!(sig.params[0].optional);
        assert_eq!(sig.params[0].param_type.as_deref(), Some("number"));
        assert_eq!(sig.params[1].param_type.as_deref(), Some("string[]"));
    }

    #[test]
    fn a_class_records_what_it_extends_and_implements() {
        // Regression: `extends` read a `superclass` field that no version of
        // the grammar emits, so no TypeScript class ever produced an
        // `Extends` edge. The heritage is an unnamed `class_heritage` child.
        let (symbols, _, _) = parse("class Svc extends Base implements Api, Closeable {}\n");
        let c = find(&symbols, "Svc");
        assert_eq!(c.kind, "class");
        assert_eq!(c.extends, vec!["Base"]);
        assert_eq!(c.implements, vec!["Api", "Closeable"]);
    }

    #[test]
    fn a_parameterised_supertype_reduces_to_its_base_name() {
        // `Base<T>` matches no declared class, so the generic argument has
        // to come off or the edge is dropped.
        let (symbols, _, _) = parse("class A extends Base<Order> implements Api<string> {}\n");
        let c = find(&symbols, "A");
        assert_eq!(c.extends, vec!["Base"]);
        assert_eq!(c.implements, vec!["Api"]);
    }

    #[test]
    fn an_interface_records_its_extends_list() {
        let (symbols, _, _) = parse("interface Api extends Base, Other { go(): void }\n");
        let i = find(&symbols, "Api");
        assert_eq!(i.kind, "interface");
        assert_eq!(i.extends, vec!["Base", "Other"]);
    }

    #[test]
    fn a_class_with_no_heritage_records_none() {
        let (symbols, _, _) = parse("class Plain { run() {} }\n");
        let c = find(&symbols, "Plain");
        assert!(c.extends.is_empty() && c.implements.is_empty());
    }

    #[test]
    fn a_top_level_arrow_const_becomes_a_symbol() {
        // The common shape of a function in this language. `graph.rs` maps
        // the `variable` kind onto a Function node precisely for it, and
        // until now had nothing to map: no const binding was ever emitted.
        let (symbols, _, _) = parse("export const handler = (n: number) => n + 1;\n");
        let v = find(&symbols, "handler");
        assert_eq!(v.kind, "variable");
    }

    #[test]
    fn one_declaration_binding_several_names_yields_several_symbols() {
        let (symbols, _, _) = parse("const a = 1, b = 2;\n");
        assert!(names(&symbols).contains(&"a"));
        assert!(names(&symbols).contains(&"b"));
    }

    #[test]
    fn a_local_declaration_is_not_a_module_symbol() {
        // The walk descends into every body; emitting a symbol per local
        // would turn each temporary into a graph node and bury the real
        // module surface.
        let (symbols, _, _) = parse(
            r#"
export const top = 1;
function outer() {
    const local = 2;
    let alsoLocal = 3;
    return local + alsoLocal;
}
"#,
        );
        assert!(names(&symbols).contains(&"top"));
        assert!(!names(&symbols).contains(&"local"), "{:?}", names(&symbols));
        assert!(!names(&symbols).contains(&"alsoLocal"));
    }

    #[test]
    fn a_const_records_the_calls_in_its_initialiser() {
        let (symbols, _, _) = parse("export const wired = build(makeDeps());\n");
        let calls = &find(&symbols, "wired").calls;
        assert!(calls.iter().any(|c| c == "build"), "{calls:?}");
        assert!(calls.iter().any(|c| c == "makeDeps"), "{calls:?}");
    }

    #[test]
    fn methods_classes_interfaces_and_type_aliases_all_surface() {
        let (symbols, _, _) = parse(
            r#"
class Svc { run() { helper(); } }
interface Api { go(): void }
type Alias = string;
"#,
        );
        assert_eq!(find(&symbols, "Svc").kind, "class");
        assert_eq!(find(&symbols, "run").kind, "method_definition");
        assert_eq!(find(&symbols, "Api").kind, "interface");
        assert_eq!(find(&symbols, "Alias").kind, "type");
        assert_eq!(find(&symbols, "run").calls, vec!["helper"]);
    }

    #[test]
    fn a_jsdoc_block_becomes_the_docstring() {
        let (symbols, _, _) = parse(
            r#"
/** Greets a person. */
function greet() {}
"#,
        );
        assert_eq!(
            find(&symbols, "greet").docstring.as_deref(),
            Some("Greets a person.")
        );
    }

    #[test]
    fn typescript_symbols_carry_no_java_only_metadata() {
        // The precise-resolution path in `graph.rs` keys off the presence of
        // a qualified name; a stray one here would route TS through it.
        let (symbols, _, _) = parse("class C { m() {} }\nexport const x = 1;\n");
        for s in &symbols {
            assert!(s.qualified_name.is_none(), "{}", s.name);
            assert!(s.annotations.is_empty(), "{}", s.name);
            assert!(s.call_refs.is_empty(), "{}", s.name);
            assert!(s.route.is_none(), "{}", s.name);
        }
    }

    // ---- imports ---------------------------------------------------------

    #[test]
    fn named_default_and_namespace_imports_are_grouped_by_path() {
        let (_, imports, _) = parse(
            r#"
import { a, b as c } from './x';
import def from './y';
import * as ns from './z';
"#,
        );
        let by_path: HashMap<&str, Vec<&str>> = imports
            .iter()
            .map(|i| {
                (
                    i.path.as_str(),
                    i.imported.iter().map(|x| x.name.as_str()).collect(),
                )
            })
            .collect();
        // An aliased import records the original name, which is what the
        // exporting file actually declares.
        assert_eq!(by_path["./x"], vec!["a", "b"]);
        assert_eq!(by_path["./y"], vec!["def"]);
        assert_eq!(by_path["./z"], vec!["ns"]);
    }

    #[test]
    fn a_type_only_import_is_still_a_dependency() {
        let (_, imports, _) = parse("import type { T } from './t';\n");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "./t");
        assert_eq!(imports[0].imported[0].name, "T");
    }

    #[test]
    fn both_quote_styles_are_accepted() {
        let (_, imports, _) = parse("import { a } from \"./double\";\n");
        assert_eq!(imports[0].path, "./double");
    }

    #[test]
    fn a_file_with_no_imports_yields_none() {
        let (_, imports, _) = parse("export function f() {}\n");
        assert!(imports.is_empty());
    }

    #[test]
    fn invalid_utf8_yields_no_imports_rather_than_panicking() {
        assert!(extract_imports_via_regex(&[0xff, 0xfe, 0x00]).is_empty());
    }

    // ---- exports ---------------------------------------------------------

    #[test]
    fn an_exported_declaration_is_recorded() {
        // Regression: only re-export forms were handled, so
        // `FileNode.exports` was empty for essentially every real module —
        // leaving the graph's `Exports` edges and the classifier's
        // export-shape fallback with nothing to read.
        let (_, _, exports) = parse(
            r#"
export function f() {}
export class C {}
export interface I {}
export type T = string;
export const v = 1;
"#,
        );
        let got: Vec<&str> = exports.iter().map(|e| e.name.as_str()).collect();
        for expected in ["f", "C", "I", "T", "v"] {
            assert!(got.contains(&expected), "{expected} missing from {got:?}");
        }
        assert!(exports.iter().all(|e| !e.is_default));
    }

    #[test]
    fn a_default_export_is_flagged() {
        let (_, _, exports) = parse("export default function main() {}\n");
        let e = exports.iter().find(|e| e.name == "main").expect("main");
        assert!(e.is_default);
    }

    #[test]
    fn an_export_clause_lists_each_specifier() {
        let (_, _, exports) = parse("const a = 1;\nconst b = 2;\nexport { a, b };\n");
        let got: Vec<&str> = exports.iter().map(|e| e.name.as_str()).collect();
        assert!(got.contains(&"a") && got.contains(&"b"), "{got:?}");
    }

    #[test]
    fn a_re_export_keeps_its_specifiers() {
        let (_, _, exports) = parse("export { thing } from './other';\n");
        assert!(exports.iter().any(|e| e.name == "thing"), "{exports:?}");
    }

    #[test]
    fn a_module_that_exports_nothing_reports_nothing() {
        let (_, _, exports) = parse("function private_() {}\n");
        assert!(exports.is_empty());
    }
}
