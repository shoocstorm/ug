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
                ..Default::default()
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
                ..Default::default()
            });
        }
        // Module-level assignments like `X = 1`, so the indexer surfaces
        // top-level constants.
        //
        // The assigned name is the `left` field; it used to be read as
        // `target`, which the grammar never emits — so no Python assignment
        // ever produced a symbol at all.
        //
        // Restricted to module level, as the description above always said
        // it was. The walk descends into every body, so without the guard
        // each local variable would become its own graph node and bury the
        // module's real surface.
        "assignment" if is_module_level(node) => {
            let Some(target) = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("target"))
            else {
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
                ..Default::default()
            });
        }
        _ => {}
    }
}

/// Is this assignment part of the module's own surface, rather than a local
/// inside some body? An assignment sits inside an `expression_statement`,
/// which for a top-level binding is a direct child of the module.
fn is_module_level(node: &Node) -> bool {
    node.parent()
        .and_then(|stmt| stmt.parent())
        .is_none_or(|scope| scope.kind() == "module")
}

/// Collect parameters from a `def …` node, falling back to a regex on the
/// source if nothing came out of the AST.
///
/// The grammar has no `parameter` node kind — it spells each form
/// separately (`identifier`, `default_parameter`, `typed_parameter`,
/// `list_splat_pattern`, …). Matching on the kind that doesn't exist meant
/// the AST branch never produced anything, so every Python function fell
/// through to [`extract_params_from_signature`]. That regex reads the first
/// `(...)` group and picks up any word it finds, so `def f(a, b=30)` came
/// out with **three** parameters — `a`, `b` and `30`.
fn extract_params(node: &Node, source: &[u8]) -> Vec<Param> {
    let mut params = Vec::new();

    if let Some(params_node) = node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.named_children(&mut cursor) {
            let Some(p) = param_from_node(&child, source) else {
                continue;
            };
            params.push(p);
        }
    }

    if params.is_empty() {
        if let Some(node_text) = get_node_text(Some(*node), source) {
            params = extract_params_from_signature(&node_text);
        }
    }

    params
}

/// One parameter, whichever of the grammar's several shapes it takes.
/// `*args` / `**kwargs` keep their sigil so the rendered signature reads
/// the way the source does.
fn param_from_node(node: &Node, source: &[u8]) -> Option<Param> {
    let (name, param_type, default) = match node.kind() {
        "identifier" => (get_node_text(Some(*node), source)?, None, None),
        "typed_parameter" => (
            // The name is the first named child; `type` is a field.
            get_node_text(node.named_child(0), source)?,
            get_node_text(node.child_by_field_name("type"), source),
            None,
        ),
        "default_parameter" => (
            get_node_text(node.child_by_field_name("name"), source)?,
            None,
            get_node_text(node.child_by_field_name("value"), source),
        ),
        "typed_default_parameter" => (
            get_node_text(node.child_by_field_name("name"), source)?,
            get_node_text(node.child_by_field_name("type"), source),
            get_node_text(node.child_by_field_name("value"), source),
        ),
        "list_splat_pattern" | "dictionary_splat_pattern" => {
            (get_node_text(Some(*node), source)?, None, None)
        }
        // `/` and `*` markers carry no parameter.
        _ => return None,
    };

    let name = name.trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(Param {
        optional: default.is_some(),
        name,
        param_type,
        default,
    })
}

/// `class Foo(Bar, Baz):` -> `["Bar", "Baz"]`.
///
/// The field is `superclasses`; it used to be read as `bases`, which no
/// version of the grammar emits — so this returned an empty list for every
/// class ever indexed, and Python graphs had no `Extends` edges at all.
///
/// Only *named* children are walked: the raw child list also contains the
/// parentheses and commas, which would otherwise arrive as base classes.
/// Two shapes get unwrapped rather than taken verbatim:
///
/// - `Generic[T]` (a `subscript`) yields `Generic`, since the parameterised
///   form matches no declared class name.
/// - `metaclass=ABCMeta` (a `keyword_argument`) is dropped — it configures
///   the class rather than being a supertype of it.
fn extract_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();
    let Some(bases) = node
        .child_by_field_name("superclasses")
        .or_else(|| node.child_by_field_name("bases"))
    else {
        return extends;
    };

    let mut cursor = bases.walk();
    for child in bases.named_children(&mut cursor) {
        let named = match child.kind() {
            "keyword_argument" => continue,
            "subscript" => child.child_by_field_name("value").unwrap_or(child),
            _ => child,
        };
        if let Some(name) = get_node_text(Some(named), source) {
            let name = name.trim();
            if !name.is_empty() {
                extends.push(name.to_string());
            }
        }
    }
    extends
}


#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> (Vec<Symbol>, Vec<ImportInfo>) {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_python::language()).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        (
            PythonIndexer.extract_symbols(src.as_bytes(), root),
            PythonIndexer.extract_imports(src.as_bytes(), root),
        )
    }

    fn find<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| {
                let all: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
                panic!("no symbol named {name} in {all:?}")
            })
    }

    fn imports_of<'a>(imports: &'a [ImportInfo], path: &str) -> Vec<&'a str> {
        imports
            .iter()
            .filter(|i| i.path == path)
            .flat_map(|i| i.imported.iter().map(|x| x.name.as_str()))
            .collect()
    }

    // ---- registration ----------------------------------------------------

    #[test]
    fn the_indexer_registers_itself_for_py() {
        assert_eq!(PythonIndexer.name(), "python");
        assert_eq!(PythonIndexer.extensions(), &["py"]);
    }

    // ---- symbols ---------------------------------------------------------

    #[test]
    fn a_function_carries_its_signature_docstring_and_metrics() {
        let (symbols, _) = parse(
            r#"
def fetch(url, timeout=30):
    """Fetch a URL."""
    if url:
        for i in range(3):
            send(url)
    return None
"#,
        );
        let f = find(&symbols, "fetch");
        assert_eq!(f.kind, "function");

        let sig = f.signature.as_ref().expect("signature");
        let names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["url", "timeout"]);
        // A default makes the parameter optional and is kept verbatim, so
        // the rendered signature in the node panel matches the source.
        assert!(!sig.params[0].optional);
        assert!(sig.params[1].optional);
        assert_eq!(sig.params[1].default.as_deref(), Some("30"));

        let m = f.metrics.as_ref().expect("metrics");
        assert_eq!(m.params, 2);
        assert_eq!(m.max_nesting, 2, "a for inside an if is depth 2");
        assert!(m.loc > 0);
    }

    #[test]
    fn every_parameter_form_is_read_from_the_ast() {
        // Regression: the AST branch matched a `parameter` node kind that
        // the grammar never emits, so every function fell through to the
        // loose signature regex — which counted `30` in `b=30` as its own
        // parameter, inflating both the list and the `params` metric.
        let (symbols, _) = parse(
            "def f(a, b=30, c: int, d: str = 'x', *args, **kwargs):\n    pass\n",
        );
        let sig = find(&symbols, "f").signature.as_ref().unwrap();
        let names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "*args", "**kwargs"]);

        assert_eq!(sig.params[1].default.as_deref(), Some("30"));
        assert_eq!(sig.params[2].param_type.as_deref(), Some("int"));
        assert_eq!(sig.params[3].param_type.as_deref(), Some("str"));
        assert_eq!(sig.params[3].default.as_deref(), Some("'x'"));
        // Only the two with defaults are optional.
        assert_eq!(sig.params.iter().filter(|p| p.optional).count(), 2);
    }

    #[test]
    fn positional_and_keyword_only_markers_are_not_parameters() {
        // `/` and `*` shape the call; they are not arguments.
        let (symbols, _) = parse("def f(a, /, b, *, c):\n    pass\n");
        let sig = find(&symbols, "f").signature.as_ref().unwrap();
        let names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_function_with_no_parameters_reports_none() {
        let (symbols, _) = parse("def go():\n    pass\n");
        let f = find(&symbols, "go");
        assert!(f.signature.as_ref().unwrap().params.is_empty());
        assert_eq!(f.metrics.as_ref().unwrap().params, 0);
    }

    #[test]
    fn a_return_annotation_is_captured() {
        let (symbols, _) = parse("def n() -> int:\n    return 1\n");
        assert_eq!(
            find(&symbols, "n").signature.as_ref().unwrap().return_type.as_deref(),
            Some("int")
        );
    }

    #[test]
    fn a_method_inside_a_class_is_extracted_too() {
        // The walk descends into class bodies, so methods are their own
        // symbols rather than being folded into the class.
        let (symbols, _) = parse(
            r#"
class Client:
    def send(self, body):
        pass
"#,
        );
        assert_eq!(find(&symbols, "Client").kind, "class");
        assert_eq!(find(&symbols, "send").kind, "function");
    }

    #[test]
    fn calls_are_collected_from_anywhere_in_the_body() {
        let (symbols, _) = parse(
            r#"
def run(x):
    prepare()
    if x:
        for i in x:
            emit(i)
    return finish()
"#,
        );
        let calls = &find(&symbols, "run").calls;
        for expected in ["prepare", "emit", "finish"] {
            assert!(calls.iter().any(|c| c == expected), "{expected} in {calls:?}");
        }
    }

    #[test]
    fn class_bases_are_captured() {
        // Regression: the extractor read a field called `bases`, which the
        // grammar does not emit — so every Python class came out with no
        // supertypes and the graph had no Extends edges at all.
        let (symbols, _) = parse("class Sub(Base, Mixin):\n    pass\n");
        assert_eq!(find(&symbols, "Sub").extends, vec!["Base", "Mixin"]);
    }

    #[test]
    fn base_lists_drop_punctuation_metaclasses_and_type_parameters() {
        let (symbols, _) = parse(
            r#"
class A(Generic[T]):
    pass
class B(Base, metaclass=ABCMeta):
    pass
class C(pkg.mod.Base):
    pass
class D:
    pass
"#,
        );
        // `Generic[T]` has to reduce to `Generic` or it matches no class.
        assert_eq!(find(&symbols, "A").extends, vec!["Generic"]);
        // A metaclass configures the class; it is not a supertype.
        assert_eq!(find(&symbols, "B").extends, vec!["Base"]);
        // A dotted base keeps its qualifier for the resolver to split.
        assert_eq!(find(&symbols, "C").extends, vec!["pkg.mod.Base"]);
        assert!(find(&symbols, "D").extends.is_empty());
    }

    #[test]
    fn a_module_level_assignment_becomes_a_symbol() {
        // Regression: the assigned name was read from a `target` field the
        // grammar never emits, so no Python assignment produced a symbol and
        // top-level constants never reached the graph.
        let (symbols, _) = parse("MAX_RETRIES = 3\n");
        let s = find(&symbols, "MAX_RETRIES");
        assert_eq!(s.kind, "assignment");
        assert!(s.signature.is_none());
    }

    #[test]
    fn a_local_assignment_is_not_a_module_symbol() {
        // The walk descends into every body, so without the module-level
        // guard each temporary would become its own graph node and bury the
        // module's real surface.
        let (symbols, _) = parse(
            r#"
TOP = 1

def f():
    local = 2
    return local

class C:
    attr = 3
"#,
        );
        let all: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(all.contains(&"TOP"), "{all:?}");
        assert!(!all.contains(&"local"), "{all:?}");
        // A class attribute is scoped to the class, not the module.
        assert!(!all.contains(&"attr"), "{all:?}");
    }

    #[test]
    fn an_annotated_module_constant_is_captured() {
        let (symbols, _) = parse("TIMEOUT: int = 30\n");
        assert_eq!(find(&symbols, "TIMEOUT").kind, "assignment");
    }

    #[test]
    fn line_ranges_are_one_based_and_span_the_definition() {
        let (symbols, _) = parse("\n\ndef f():\n    pass\n");
        let f = find(&symbols, "f");
        assert_eq!(f.start_line, 3);
        assert!(f.end_line >= 4);
    }

    #[test]
    fn python_symbols_carry_no_java_only_metadata() {
        // The shared `Symbol` gained qualified names, annotations and typed
        // call refs for Java. Python must leave them empty, because the
        // graph builder's precise-resolution path keys off their presence.
        let (symbols, _) = parse("class C:\n    def m(self):\n        pass\n");
        for s in &symbols {
            assert!(s.qualified_name.is_none(), "{}", s.name);
            assert!(s.owner.is_none(), "{}", s.name);
            assert!(s.annotations.is_empty(), "{}", s.name);
            assert!(s.call_refs.is_empty(), "{}", s.name);
            assert!(s.route.is_none(), "{}", s.name);
        }
    }

    #[test]
    fn python_declares_no_exports() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_python::language()).unwrap();
        let src = "def public():\n    pass\n";
        let tree = parser.parse(src, None).unwrap();
        assert!(PythonIndexer
            .extract_exports(src.as_bytes(), tree.root_node())
            .is_empty());
    }

    // ---- imports ---------------------------------------------------------

    #[test]
    fn from_imports_group_by_module() {
        let (_, imports) = parse("from os.path import join, dirname\n");
        assert_eq!(imports_of(&imports, "os.path"), vec!["join", "dirname"]);
    }

    #[test]
    fn a_parenthesised_from_import_is_read() {
        let (_, imports) = parse("from pkg import (\n    a,\n    b,\n)\n");
        assert_eq!(imports_of(&imports, "pkg"), vec!["a", "b"]);
    }

    #[test]
    fn an_aliased_import_records_the_original_name() {
        let (_, imports) = parse("from pkg import thing as other\n");
        assert_eq!(imports_of(&imports, "pkg"), vec!["thing"]);
    }

    #[test]
    fn a_relative_import_keeps_its_dots_for_the_resolver() {
        // `graph.rs` joins the path against the importing file's directory,
        // so the leading dots have to survive extraction.
        let (_, imports) = parse("from .sibling import helper\n");
        assert_eq!(imports_of(&imports, ".sibling"), vec!["helper"]);
    }

    #[test]
    fn a_plain_import_names_its_trailing_segment() {
        let (_, imports) = parse("import os.path\n");
        assert_eq!(imports_of(&imports, "os.path"), vec!["path"]);
    }

    #[test]
    fn a_file_with_no_imports_yields_none() {
        let (_, imports) = parse("def f():\n    pass\n");
        assert!(imports.is_empty());
    }

    #[test]
    fn invalid_utf8_yields_no_imports_rather_than_panicking() {
        // The extractor works on raw bytes; a file that isn't valid UTF-8
        // has to degrade quietly.
        assert!(extract_imports_via_regex(&[0xff, 0xfe, 0x00]).is_empty());
    }
}

