//! End-to-end tests for the Rust language indexer.
//!
//! Covers the full pipeline: file-walker pickup → tree-sitter parse →
//! symbol / import extraction → graph construction. Tests stage real
//! `.rs` source into a per-test `TempDir` so they don't depend on
//! anything outside `tests/`.

use std::fs;
use tempfile::TempDir;
use ultragraph::types::{GraphData, GraphEdgeType, GraphNodeType, IndexResult};
use ultragraph::{build_graph, index};

/// Write `content` to `<TempDir>/<name>.rs` and return both. The dir
/// must be kept alive for the file to exist; `scan_files` walks the
/// dir path on the next `index()` call.
fn stage_rs(content: &str, name: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join(name);
    fs::write(&path, content).expect("write fixture");
    (dir, path)
}

fn run_index(dir: &TempDir) -> IndexResult {
    let json = index(dir.path().to_string_lossy().to_string());
    serde_json::from_str(&json).expect("index() returned invalid JSON")
}

// ─── Walker / language registration ─────────────────────────────────

#[test]
fn index_picks_up_rs_files_as_rust() {
    let (dir, _) = stage_rs("pub fn hi() {}\n", "lib.rs");
    let result = run_index(&dir);
    assert_eq!(result.files.len(), 1, "expected a single .rs file");
    let file = &result.files[0];
    assert_eq!(file.language, "rust");
    assert!(file.path.ends_with("lib.rs"));
}

#[test]
fn rs_uppercase_extension_still_indexed() {
    // Mirrors the PDF case: scanners / batch tools sometimes emit
    // upper-case extensions. The walker lowercases before matching
    // SUPPORTED_EXTS so `.RS` files still get indexed.
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("MAIN.RS"), "pub fn hi() {}\n").expect("write .RS");
    let result = run_index(&dir);
    let file = result
        .files
        .iter()
        .find(|f| f.path.to_lowercase().ends_with(".rs"))
        .expect("uppercase-extension .RS should be indexed");
    assert_eq!(file.language, "rust");
}

// ─── Symbol extraction ──────────────────────────────────────────────

#[test]
fn extracts_free_function_with_signature() {
    let src = r#"
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;
    let (dir, _) = stage_rs(src, "math.rs");
    let result = run_index(&dir);
    let sym = result.files[0]
        .symbols
        .iter()
        .find(|s| s.name == "add")
        .expect("add fn should be extracted");
    assert_eq!(sym.kind, "function");
    let sig = sym.signature.as_ref().expect("signature populated");
    let param_names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(param_names, vec!["a", "b"]);
    assert_eq!(sig.params[0].param_type.as_deref(), Some("i32"));
    assert_eq!(sig.return_type.as_deref(), Some("i32"));
}

#[test]
fn extracts_struct_enum_trait_type_alias() {
    let src = r#"
pub struct Point { x: f64, y: f64 }
pub enum Color { Red, Green, Blue }
pub trait Shape: Send + Sync {
    fn area(&self) -> f64;
}
pub type Coord = (f64, f64);
"#;
    let (dir, _) = stage_rs(src, "shapes.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let by_kind: std::collections::HashMap<&str, &str> = symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.name.as_str()))
        .collect();
    assert_eq!(by_kind.get("struct"), Some(&"Point"));
    assert_eq!(by_kind.get("enum"), Some(&"Color"));
    assert_eq!(by_kind.get("trait"), Some(&"Shape"));
    assert_eq!(by_kind.get("type_alias"), Some(&"Coord"));
}

#[test]
fn extracts_constants_and_statics() {
    let src = r#"
const MAX: usize = 1024;
static GREETING: &str = "hi";
"#;
    let (dir, _) = stage_rs(src, "consts.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let names: Vec<&str> = symbols
        .iter()
        .filter(|s| s.kind == "constant")
        .map(|s| s.name.as_str())
        .collect();
    assert!(names.contains(&"MAX"));
    assert!(names.contains(&"GREETING"));
}

#[test]
fn extracts_macro_definition() {
    let src = r#"
macro_rules! shout {
    ($x:expr) => { println!("{}!", $x) };
}
"#;
    let (dir, _) = stage_rs(src, "mac.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let m = symbols
        .iter()
        .find(|s| s.kind == "macro")
        .expect("macro_rules! shout should be extracted");
    assert_eq!(m.name, "shout");
}

#[test]
fn impl_methods_are_qualified_with_type_name() {
    let src = r#"
pub struct Counter { n: u32 }
impl Counter {
    pub fn new() -> Self { Counter { n: 0 } }
    pub fn inc(&mut self) { self.n += 1; }
}
"#;
    let (dir, _) = stage_rs(src, "counter.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let method_names: Vec<&str> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        method_names.contains(&"Counter::new"),
        "impl methods must be qualified, got: {:?}",
        method_names
    );
    assert!(method_names.contains(&"Counter::inc"));
}

#[test]
fn trait_impl_records_implements_relation() {
    let src = r#"
pub trait Greet {
    fn hello(&self) -> &str;
}
pub struct Robot;
impl Greet for Robot {
    fn hello(&self) -> &str { "beep" }
}
"#;
    let (dir, _) = stage_rs(src, "robot.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let hello = symbols
        .iter()
        .find(|s| s.name == "Robot::hello")
        .expect("Robot::hello should be extracted");
    assert_eq!(hello.implements, vec!["Greet".to_string()]);
}

#[test]
fn trait_super_bounds_land_in_extends() {
    let src = r#"
pub trait Foo: std::fmt::Debug + Send {
    fn bar(&self);
}
"#;
    let (dir, _) = stage_rs(src, "bounds.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let foo = symbols
        .iter()
        .find(|s| s.kind == "trait" && s.name == "Foo")
        .expect("Foo trait");
    // The bounds parser may collapse spaces — assert presence rather
    // than exact-string equality.
    let joined = foo.extends.join("|");
    assert!(joined.contains("Debug"), "extends should mention Debug: {:?}", foo.extends);
    assert!(joined.contains("Send"), "extends should mention Send: {:?}", foo.extends);
}

#[test]
fn nested_mod_items_are_extracted() {
    let src = r#"
pub mod inner {
    pub fn deep() -> u8 { 42 }
    pub struct Hidden;
}
"#;
    let (dir, _) = stage_rs(src, "nested.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"deep"), "nested fn missing in {:?}", names);
    assert!(names.contains(&"Hidden"), "nested struct missing in {:?}", names);
}

// ─── Doc comments ──────────────────────────────────────────────────

#[test]
fn outer_doc_comments_become_docstrings() {
    let src = r#"
/// First line of docs.
/// Second line continues.
pub fn documented() {}

pub fn undocumented() {}
"#;
    let (dir, _) = stage_rs(src, "docs.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let documented = symbols
        .iter()
        .find(|s| s.name == "documented")
        .expect("documented fn");
    let docs = documented.docstring.as_deref().unwrap_or("");
    assert!(docs.contains("First line"), "missing doc text: {:?}", docs);
    assert!(docs.contains("Second line"), "missing 2nd line: {:?}", docs);

    let undocumented = symbols
        .iter()
        .find(|s| s.name == "undocumented")
        .expect("undocumented fn");
    assert!(undocumented.docstring.is_none(), "should have no docstring");
}

#[test]
fn regular_comments_are_not_treated_as_docstrings() {
    let src = r#"
// Not a doc comment.
pub fn plain() {}
"#;
    let (dir, _) = stage_rs(src, "plain.rs");
    let symbols = run_index(&dir).files.remove(0).symbols;
    let plain = symbols.iter().find(|s| s.name == "plain").unwrap();
    assert!(
        plain.docstring.is_none(),
        "// (non-doc) comments must not become docstrings, got: {:?}",
        plain.docstring
    );
}

// ─── Imports ───────────────────────────────────────────────────────

#[test]
fn use_declarations_become_imports() {
    let src = r#"
use std::collections::{HashMap, HashSet};
use serde::Serialize;
use std::io::Read as IoRead;
pub fn _placeholder() {}
"#;
    let (dir, _) = stage_rs(src, "uses.rs");
    let imports = run_index(&dir).files.remove(0).imports;

    let by_path: std::collections::HashMap<String, Vec<String>> = imports
        .iter()
        .map(|i| {
            (
                i.path.clone(),
                i.imported.iter().map(|x| x.name.clone()).collect(),
            )
        })
        .collect();

    let coll = by_path
        .get("std::collections")
        .expect("std::collections key");
    assert!(coll.contains(&"HashMap".to_string()));
    assert!(coll.contains(&"HashSet".to_string()));

    let serde = by_path.get("serde").expect("serde key");
    assert_eq!(serde, &vec!["Serialize".to_string()]);

    // `Read as IoRead` — name should be the original (`Read`), alias
    // preserved on the ImportedItem.
    let io = imports
        .iter()
        .find(|i| i.path == "std::io")
        .expect("std::io key");
    let read = io
        .imported
        .iter()
        .find(|it| it.name == "Read")
        .expect("Read item");
    assert_eq!(read.alias.as_deref(), Some("IoRead"));
}

#[test]
fn wildcard_use_is_recorded() {
    let src = "use foo::bar::*;\npub fn _p() {}\n";
    let (dir, _) = stage_rs(src, "wild.rs");
    let imports = run_index(&dir).files.remove(0).imports;
    let bar = imports
        .iter()
        .find(|i| i.path == "foo::bar")
        .expect("foo::bar key");
    assert!(bar.imported.iter().any(|i| i.name == "*"));
}

// ─── Mixed-language scan ───────────────────────────────────────────

#[test]
fn rust_and_other_languages_coexist_in_same_scan() {
    let dir = TempDir::new().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn hi() {}\npub struct S;\n",
    )
    .unwrap();
    fs::write(dir.path().join("notes.md"), "# Heading\n\nbody\n").unwrap();
    fs::write(dir.path().join("util.py"), "def util():\n    return 1\n").unwrap();

    let result = run_index(&dir);
    let langs: std::collections::HashSet<&str> =
        result.files.iter().map(|f| f.language.as_str()).collect();
    assert!(langs.contains("rust"));
    assert!(langs.contains("markdown"));
    assert!(langs.contains("python"));
}

// ─── Graph layer round-trip ────────────────────────────────────────

#[test]
fn rust_symbols_become_correct_graph_node_types() {
    let src = r#"
pub struct Point;
pub enum Color { R, G, B }
pub trait Shape { fn area(&self) -> f64; }
pub type Coord = (f64, f64);
pub fn helper() {}
"#;
    let (dir, _) = stage_rs(src, "lib.rs");
    let result = run_index(&dir);
    let index_json = serde_json::to_string(&result).unwrap();
    let graph: GraphData = serde_json::from_str(&build_graph(index_json)).unwrap();

    let lookup = |name: &str, expected: GraphNodeType| {
        let n = graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("missing graph node for {}", name));
        assert!(
            std::mem::discriminant(&n.node_type) == std::mem::discriminant(&expected),
            "node {} should be {:?}, got {:?}",
            name,
            expected,
            n.node_type
        );
    };
    lookup("Point", GraphNodeType::Class);
    lookup("Color", GraphNodeType::Class);
    lookup("Shape", GraphNodeType::Interface);
    lookup("Coord", GraphNodeType::Interface);
    lookup("helper", GraphNodeType::Function);
}

#[test]
fn file_contains_edges_to_each_rust_symbol() {
    let src = r#"
pub fn a() {}
pub struct B;
pub trait C { fn m(&self); }
"#;
    let (dir, _) = stage_rs(src, "lib.rs");
    let result = run_index(&dir);
    let graph: GraphData =
        serde_json::from_str(&build_graph(serde_json::to_string(&result).unwrap())).unwrap();

    let file_node = graph
        .nodes
        .iter()
        .find(|n| matches!(n.node_type, GraphNodeType::File) && n.id.contains("lib.rs"))
        .expect("file node");
    let target_names = ["a", "B", "C"];
    for sym_name in target_names {
        let sym_node = graph
            .nodes
            .iter()
            .find(|n| n.name == sym_name)
            .unwrap_or_else(|| panic!("symbol node {} missing", sym_name));
        let has_contains = graph.edges.iter().any(|e| {
            matches!(e.edge_type, GraphEdgeType::Contains)
                && e.source == file_node.id
                && e.target == sym_node.id
        });
        assert!(
            has_contains,
            "File node should Contain symbol '{}'",
            sym_name
        );
    }
}

/// `metrics.max_nesting` measures control-flow depth *inside* a body.
///
/// Regression test: the original heuristic counted declaration node kinds
/// (`function_declaration`, `class_declaration`, …). Rust's nodes are
/// `function_item` / `struct_item`, so every Rust symbol scored 0 — which
/// made the metric useless on this very repo.
#[test]
fn rust_metrics_report_control_flow_nesting() {
    let (dir, _) = stage_rs(
        r#"
pub fn flat(a: u32, b: u32) -> u32 {
    let c = a + b;
    c
}

pub fn one_level(items: &[u32]) -> u32 {
    let mut total = 0;
    for i in items {
        total += i;
    }
    total
}

pub fn three_levels(items: &[u32]) -> u32 {
    let mut total = 0;
    for i in items {
        if *i > 0 {
            match i {
                _ => total += i,
            }
        }
    }
    total
}
"#,
        "nesting.rs",
    );
    let result = run_index(&dir);
    let symbols = &result.files[0].symbols;

    let nesting = |name: &str| -> u32 {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("missing symbol {}", name))
            .metrics
            .as_ref()
            .expect("rust functions should carry metrics")
            .max_nesting
    };

    assert_eq!(nesting("flat"), 0, "a straight-line body has no nesting");
    assert_eq!(nesting("one_level"), 1, "a single for-loop is one level");
    assert_eq!(nesting("three_levels"), 3, "for > if > match is three levels");

    // LOC and params should keep working alongside it.
    let flat = symbols.iter().find(|s| s.name == "flat").unwrap();
    let m = flat.metrics.as_ref().unwrap();
    assert_eq!(m.params, 2);
    assert!(m.loc >= 3, "loc should span the body, got {}", m.loc);
}
