//! Rust indexer. Handles `.rs`.
//!
//! Maps the Rust item set onto the project's `Symbol` model:
//!
//! | Tree-sitter node          | `Symbol.kind`   | Notes                                                          |
//! |---------------------------|-----------------|----------------------------------------------------------------|
//! | `function_item`           | `function`      | Top-level fn or inside a `mod`. Methods are handled below.     |
//! | `struct_item`             | `struct`        | → `Class` in the graph.                                        |
//! | `enum_item`               | `enum`          | → `Class` in the graph.                                        |
//! | `trait_item`              | `trait`         | → `Interface`. Super-trait bounds land in `extends`.           |
//! | `impl_item`               | (no symbol)     | Walked into; methods become `function` with name `Type::method`.|
//! |                           |                 | For `impl Trait for Type`, the method also carries `implements: [Trait]`. |
//! | `type_item`               | `type_alias`    | → `Interface`.                                                 |
//! | `const_item`/`static_item`| `constant`      | Top-level constants get their own symbol.                      |
//! | `macro_definition`        | `macro`         | declarative `macro_rules!`. Proc macros surface as `function`. |
//! | `mod_item`                | (no symbol)     | Walked into so nested items still get extracted.               |
//!
//! Doc comments (`///` and `//!`) on consecutive lines immediately
//! preceding an item are collapsed into the symbol's `docstring`.
//! `use` declarations become `ImportInfo` entries keyed by the first
//! crate / module segment.

use crate::indexer::common::{
    calculate_nesting, extract_function_calls, extract_return_type, get_node_text,
};
use crate::indexer::languages::LanguageIndexer;
use crate::types::{ExportInfo, ImportInfo, ImportedItem, Param, Signature, Symbol, SymbolMetrics};
use std::collections::HashMap;
use tree_sitter::Node;

pub struct RustIndexer;

impl LanguageIndexer for RustIndexer {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_rust::language()
    }

    fn extract_imports(&self, source: &[u8], root: Node) -> Vec<ImportInfo> {
        let mut imports: HashMap<String, ImportInfo> = HashMap::new();
        walk_for_imports(root, source, &mut imports);
        imports.into_values().collect()
    }

    fn extract_exports(&self, _source: &[u8], _root: Node) -> Vec<ExportInfo> {
        // Rust has no separate export list — `pub` visibility on each
        // item is the equivalent, and every `pub fn` / `pub struct` is
        // already surfaced as its own Symbol. Re-emitting them here
        // would just duplicate work for downstream consumers.
        Vec::new()
    }

    fn extract_symbols(&self, source: &[u8], root: Node) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        visit(root, source, /* impl_owner */ None, /* impl_trait */ None, &mut symbols);
        symbols
    }
}

/// AST walk. `impl_owner` is `Some(type_name)` when we're inside the
/// body of an `impl …` block — methods extracted in that scope are
/// renamed to `Type::method` so the graph can disambiguate them from
/// free functions or methods on other types. `impl_trait` is the trait
/// being implemented for `impl Trait for Type` blocks.
fn visit(
    node: Node,
    source: &[u8],
    impl_owner: Option<&str>,
    impl_trait: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();

    match kind {
        "impl_item" => {
            // Resolve the type this impl is for, and (optionally) the
            // trait being implemented. Both become qualifiers on the
            // contained methods rather than top-level symbols.
            let type_name =
                get_node_text(node.child_by_field_name("type"), source).unwrap_or_default();
            let trait_name = get_node_text(node.child_by_field_name("trait"), source);
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    visit(child, source, Some(&type_name), trait_name.as_deref(), out);
                }
            }
            return;
        }
        "mod_item" => {
            // Step into module bodies so nested items still appear in
            // the symbol list. The `mod` itself isn't a symbol —
            // matches what other indexers (e.g. Python packages) do.
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    visit(child, source, impl_owner, impl_trait, out);
                }
            }
            return;
        }
        _ => {}
    }

    extract_symbol_from_node(&node, source, impl_owner, impl_trait, out);

    // Descend into other nodes so e.g. functions inside a top-level
    // `mod foo { … }` block are found. Stop recursing into bodies that
    // would just re-emit nested locals — function bodies are intentionally
    // left alone because Rust closures/locals are not symbols we surface.
    if matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
    ) {
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, impl_owner, impl_trait, out);
    }
}

fn extract_symbol_from_node(
    node: &Node,
    source: &[u8],
    impl_owner: Option<&str>,
    impl_trait: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;
    let docstring = extract_rust_docstring(node, source);

    match kind {
        "function_item" => {
            let Some(raw_name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // Methods inside an `impl Foo` block get qualified — the
            // graph layer keys IDs on `<file>:<line>:<name>` so this
            // also keeps `Foo::new` distinct from `Bar::new`.
            let name = match impl_owner {
                Some(owner) if !owner.is_empty() => format!("{}::{}", owner, raw_name),
                _ => raw_name,
            };

            let params = extract_params(node, source);
            let return_type = extract_return_type(node, source);
            let calls = extract_function_calls(node, source);
            let metrics = SymbolMetrics {
                loc: end.saturating_sub(start),
                params: params.len() as u32,
                max_nesting: calculate_nesting(node),
            };

            let implements = impl_trait
                .map(|t| vec![t.to_string()])
                .unwrap_or_default();

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
                implements,
                calls,
                metrics: Some(metrics),
            });
        }
        "struct_item" | "enum_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            let item_kind = if kind == "struct_item" { "struct" } else { "enum" };
            out.push(Symbol {
                id: format!("{}:{}:{}", item_kind, start, name),
                name,
                kind: item_kind.to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring,
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends: Vec::new(),
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "trait_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // `trait Foo: Bar + Baz { … }` — super-traits land in
            // `extends` so the graph captures the trait hierarchy.
            let extends = extract_trait_bounds(node, source);
            out.push(Symbol {
                id: format!("trait:{}:{}", start, name),
                name,
                kind: "trait".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring,
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends,
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "type_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("type:{}:{}", start, name),
                name,
                kind: "type_alias".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring,
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends: Vec::new(),
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "const_item" | "static_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("const:{}:{}", start, name),
                name,
                kind: "constant".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring,
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends: Vec::new(),
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
            });
        }
        "macro_definition" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                id: format!("macro:{}:{}", start, name),
                name,
                kind: "macro".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring,
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

/// Extract parameters from a Rust `function_item`. Walks each
/// `parameter`/`self_parameter` child of the `parameters` field.
/// `self` / `&self` / `&mut self` count as the first parameter but
/// carry no type or default — keeps method arity honest.
fn extract_params(node: &Node, source: &[u8]) -> Vec<Param> {
    let mut params = Vec::new();
    let Some(params_node) = node.child_by_field_name("parameters") else {
        return params;
    };
    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        match child.kind() {
            "self_parameter" => {
                let text = get_node_text(Some(child), source).unwrap_or_else(|| "self".into());
                params.push(Param {
                    name: text,
                    param_type: None,
                    optional: false,
                    default: None,
                });
            }
            "parameter" => {
                let pattern_text = get_node_text(child.child_by_field_name("pattern"), source);
                let type_text = get_node_text(child.child_by_field_name("type"), source);
                let name = pattern_text.unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                params.push(Param {
                    name,
                    param_type: type_text,
                    optional: false,
                    default: None,
                });
            }
            _ => {}
        }
    }
    params
}

/// Extract super-trait bounds from a `trait_item`'s `bounds` field.
/// `trait Foo: Display + Send` → `["Display", "Send"]`.
fn extract_trait_bounds(node: &Node, source: &[u8]) -> Vec<String> {
    let Some(bounds) = node.child_by_field_name("bounds") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = bounds.walk();
    for child in bounds.children(&mut cursor) {
        let kind = child.kind();
        // The `bounds` field contains the bound nodes plus the leading
        // `:` and `+` separators; skip those tokens and extract the
        // textual representation of each named bound.
        if matches!(kind, ":" | "+") {
            continue;
        }
        if let Some(t) = get_node_text(Some(child), source) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                out.push(t);
            }
        }
    }
    out
}

/// Walk the AST collecting `use_declaration` and `extern_crate_declaration`
/// nodes. The first crate / module segment becomes the import path so
/// callers can resolve cross-file `use crate::foo::Bar` to a `foo`-rooted
/// file edge the same way TypeScript's `import` resolution works.
fn walk_for_imports(node: Node, source: &[u8], out: &mut HashMap<String, ImportInfo>) {
    match node.kind() {
        "use_declaration" => {
            if let Some(arg) = node.child_by_field_name("argument") {
                let raw = get_node_text(Some(arg), source).unwrap_or_default();
                parse_use_tree(&raw, out);
            }
            return;
        }
        "extern_crate_declaration" => {
            // `extern crate foo;` — collapse into a single-item import.
            if let Some(crate_name_node) = node
                .children(&mut node.walk())
                .find(|c| c.kind() == "identifier")
            {
                if let Some(name) = get_node_text(Some(crate_name_node), source) {
                    out.entry(name.clone()).or_insert(ImportInfo {
                        path: name.clone(),
                        imported: vec![ImportedItem { name, alias: None }],
                    });
                }
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_imports(child, source, out);
    }
}

/// Parse the argument of a `use` declaration into one or more import
/// records. Handles the common shapes:
///   - `use foo::Bar;`            → path `foo`, name `Bar`
///   - `use foo::{Bar, Baz};`     → path `foo`, names `Bar`, `Baz`
///   - `use foo::Bar as Qux;`     → path `foo`, name `Bar` (alias `Qux`)
///   - `use foo::*;`              → path `foo`, name `*`
///   - `use crate::a::b::Bar;`    → path `crate::a::b`, name `Bar`
///
/// Nested groups (`use foo::{a, b::{c, d}}`) are flattened to their
/// leaf names with the longest common prefix as the path. Edge cases
/// fall back to recording the whole text as the import path with no
/// names — better to over-record than to silently drop something the
/// graph layer might want.
fn parse_use_tree(raw: &str, out: &mut HashMap<String, ImportInfo>) {
    let cleaned = raw.replace(['\n', '\t'], " ");
    let cleaned = cleaned.trim().trim_end_matches(';');
    let mut imports: Vec<(String, ImportedItem)> = Vec::new();
    expand_use(cleaned, "", &mut imports);
    for (path, item) in imports {
        out.entry(path.clone())
            .and_modify(|info| {
                if !info.imported.iter().any(|i| i.name == item.name) {
                    info.imported.push(item.clone());
                }
            })
            .or_insert(ImportInfo {
                path: path.clone(),
                imported: vec![item],
            });
    }
}

/// Recursive use-tree expansion. `prefix` accumulates the parent path
/// segments; `tree` is the unprocessed remainder.
fn expand_use(tree: &str, prefix: &str, out: &mut Vec<(String, ImportedItem)>) {
    let tree = tree.trim();
    if tree.is_empty() {
        return;
    }
    // Brace-group on the right: `prefix::{a, b::{c}}`.
    if let Some(brace) = tree.find('{') {
        let close = match find_matching_brace(tree, brace) {
            Some(idx) => idx,
            None => {
                // Malformed — record the whole thing as a single import.
                push_simple(tree, prefix, out);
                return;
            }
        };
        let path_part = tree[..brace].trim_end_matches(':').trim();
        let inner = &tree[brace + 1..close];
        let new_prefix = combine_prefix(prefix, path_part);
        for chunk in split_top_level_commas(inner) {
            expand_use(&chunk, &new_prefix, out);
        }
        return;
    }
    push_simple(tree, prefix, out);
}

/// Emit one import for a leaf `use` segment like `foo::bar::Baz` or
/// `foo::bar::Baz as Qux` or `foo::*`. Splits on `::` to derive the
/// (path, item) pair; everything before the last segment becomes the
/// path, the last segment becomes the item name.
fn push_simple(segment: &str, prefix: &str, out: &mut Vec<(String, ImportedItem)>) {
    let combined = combine_prefix(prefix, segment.trim());
    if combined.is_empty() {
        return;
    }
    // Handle `as` alias.
    let (left, alias) = match combined.split_once(" as ") {
        Some((l, a)) => (l.trim().to_string(), Some(a.trim().to_string())),
        None => (combined, None),
    };
    let mut parts: Vec<&str> = left.split("::").collect();
    if parts.is_empty() {
        return;
    }
    let name = parts.pop().unwrap_or("").to_string();
    let path = parts.join("::");
    if name.is_empty() {
        return;
    }
    out.push((
        if path.is_empty() { name.clone() } else { path },
        ImportedItem { name, alias },
    ));
}

/// Find the matching `}` for the `{` at `open_idx`. Returns the index
/// of the closer or `None` if braces don't balance.
fn find_matching_brace(s: &str, open_idx: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    for (i, b) in bytes.iter().enumerate().skip(open_idx) {
        match *b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a string on top-level commas (skipping commas inside nested
/// `{ … }` groups). Used to break apart `a, b::{c, d}, e` into three
/// children for recursive expansion.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut buf = String::new();
    for c in s.chars() {
        match c {
            '{' => {
                depth += 1;
                buf.push(c);
            }
            '}' => {
                depth -= 1;
                buf.push(c);
            }
            ',' if depth == 0 => {
                if !buf.trim().is_empty() {
                    out.push(buf.trim().to_string());
                }
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    out
}

fn combine_prefix(prefix: &str, suffix: &str) -> String {
    let suffix = suffix.trim_end_matches("::").trim_start_matches("::").trim();
    if prefix.is_empty() {
        return suffix.to_string();
    }
    if suffix.is_empty() {
        return prefix.to_string();
    }
    format!("{}::{}", prefix, suffix)
}

/// Collapse the run of `///` (outer) or `//!` (inner) doc-comment lines
/// directly above a symbol's start row into a single docstring. Walks
/// the node's previous siblings via the parent's children — Rust's
/// tree-sitter grammar emits each comment as a separate top-level
/// node, so we can scan backwards from the symbol.
fn extract_rust_docstring(node: &Node, source: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    let siblings: Vec<Node> = parent.children(&mut cursor).collect();
    let self_idx = siblings
        .iter()
        .position(|n| n.id() == node.id())
        .unwrap_or(siblings.len());
    if self_idx == 0 {
        return None;
    }

    let mut collected: Vec<String> = Vec::new();
    let mut expected_row = node.start_position().row;
    for sib in siblings[..self_idx].iter().rev() {
        if sib.kind() != "line_comment" {
            break;
        }
        // Comment must be on the line immediately above the next
        // already-collected element (or the symbol itself for the
        // first one). Anything else breaks the doc run.
        let sib_end_row = sib.end_position().row;
        if sib_end_row + 1 != expected_row {
            break;
        }
        let text = get_node_text(Some(*sib), source).unwrap_or_default();
        let stripped = if let Some(rest) = text.strip_prefix("///") {
            Some(rest.trim_start_matches(['/', ' ']).to_string())
        } else if let Some(rest) = text.strip_prefix("//!") {
            Some(rest.trim_start_matches(['!', ' ']).to_string())
        } else {
            break;
        };
        match stripped {
            Some(s) => {
                collected.push(s);
                expected_row = sib.start_position().row;
            }
            None => break,
        }
    }

    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let joined = collected
        .iter()
        .map(|s| s.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = joined.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    fn collect(raw: &str) -> Vec<(String, String, Option<String>)> {
        let mut map: HashMap<String, ImportInfo> = HashMap::new();
        parse_use_tree(raw, &mut map);
        let mut out: Vec<(String, String, Option<String>)> = map
            .into_iter()
            .flat_map(|(path, info)| {
                info.imported
                    .into_iter()
                    .map(move |it| (path.clone(), it.name, it.alias))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn simple_use_path() {
        assert_eq!(
            collect("std::collections::HashMap;"),
            vec![("std::collections".into(), "HashMap".into(), None)]
        );
    }

    #[test]
    fn brace_group_expands() {
        let got = collect("std::io::{Read, Write};");
        assert_eq!(
            got,
            vec![
                ("std::io".into(), "Read".into(), None),
                ("std::io".into(), "Write".into(), None),
            ]
        );
    }

    #[test]
    fn alias_is_captured() {
        assert_eq!(
            collect("foo::Bar as Baz;"),
            vec![("foo".into(), "Bar".into(), Some("Baz".into()))]
        );
    }

    #[test]
    fn nested_brace_groups_flatten() {
        let got = collect("a::{b, c::{d, e}};");
        assert_eq!(
            got,
            vec![
                ("a".into(), "b".into(), None),
                ("a::c".into(), "d".into(), None),
                ("a::c".into(), "e".into(), None),
            ]
        );
    }

    #[test]
    fn wildcard_use() {
        assert_eq!(
            collect("foo::bar::*;"),
            vec![("foo::bar".into(), "*".into(), None)]
        );
    }
}
