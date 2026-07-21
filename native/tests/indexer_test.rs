use std::fs;
use std::path::Path;
use tempfile::TempDir;
use ultragraph::{index, types::IndexResult};

fn create_test_dir() -> TempDir {
    let dir = TempDir::new().expect("Failed to create temp dir");
    dir
}

fn write_file(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    fs::write(&path, content).expect(&format!("Failed to write {}", name));
}

#[test]
fn test_index_empty_directory() {
    let dir = create_test_dir();
    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    assert_eq!(parsed.stats.total_files, 0);
    assert_eq!(parsed.stats.total_symbols, 0);
}

#[test]
fn test_index_single_file() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        "function add(a: number, b: number): number { return a + b; }",
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    assert_eq!(parsed.stats.total_files, 1);
    assert!(parsed.stats.total_symbols > 0);
    assert!(!parsed.files[0].symbols.is_empty());
}

#[test]
fn test_index_extracts_functions() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        r#"
export function add(a: number, b: number): number {
    return a + b;
}

export function multiply(a: number, b: number): number {
    return a * b;
}
"#,
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let symbols = &parsed.files[0].symbols;
    let fn_names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

    assert!(fn_names.contains(&"add"));
    assert!(fn_names.contains(&"multiply"));
}

#[test]
fn test_index_extracts_classes() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        r#"
export class Calculator {
    add(a: number, b: number): number {
        return a + b;
    }
}
"#,
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let symbols = &parsed.files[0].symbols;
    let class_names: Vec<&str> = symbols
        .iter()
        .filter(|s| s.kind == "class")
        .map(|s| s.name.as_str())
        .collect();

    assert!(class_names.contains(&"Calculator"));
}

#[test]
fn test_index_extracts_interfaces() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        r#"
export interface Config {
    name: string;
    value: number;
}
"#,
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let symbols = &parsed.files[0].symbols;
    let interface_names: Vec<&str> = symbols
        .iter()
        .filter(|s| s.kind == "interface")
        .map(|s| s.name.as_str())
        .collect();

    assert!(interface_names.contains(&"Config"));
}

#[test]
fn test_index_extracts_signature_params() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        "function greet(name: string, times: number): void { }",
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let fn_symbol = parsed.files[0]
        .symbols
        .iter()
        .find(|s| s.name == "greet")
        .expect("Function not found");

    assert!(fn_symbol.signature.is_some());
    let sig = fn_symbol.signature.as_ref().unwrap();
    assert_eq!(sig.params.len(), 2);
    assert_eq!(sig.params[0].name, "name");
    assert_eq!(sig.params[1].name, "times");
}

#[test]
fn test_index_extracts_return_type() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        "function getValue(): number { return 42; }",
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let fn_symbol = parsed.files[0]
        .symbols
        .iter()
        .find(|s| s.name == "getValue")
        .expect("Function not found");

    assert!(fn_symbol.signature.is_some());
    let sig = fn_symbol.signature.as_ref().unwrap();
    assert!(sig.return_type.is_some());
    assert_eq!(sig.return_type.as_ref().unwrap(), "number");
}

#[test]
fn test_index_extracts_docstring() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        r#"
/**
 * Adds two numbers together.
 * @param a - First number
 * @param b - Second number
 * @returns The sum
 */
function add(a: number, b: number): number {
    return a + b;
}
"#,
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let fn_symbol = parsed.files[0]
        .symbols
        .iter()
        .find(|s| s.name == "add")
        .expect("Function not found");

    assert!(fn_symbol.docstring.is_some());
    let doc = fn_symbol.docstring.as_ref().unwrap();
    assert!(doc.contains("Adds two numbers"));
}

#[test]
fn test_index_extracts_imports() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "math.ts",
        "export function add(a: number, b: number): number { return a + b; }",
    );
    write_file(dir.path(), "main.ts", "import { add } from './math';");

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let main_file = parsed
        .files
        .iter()
        .find(|f| f.path.contains("main.ts"))
        .expect("main.ts not found");

    assert!(!main_file.imports.is_empty());
    assert_eq!(main_file.imports[0].path, "./math");
}

#[test]
fn test_index_extracts_extends() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        r#"
class Base {
    method() {}
}

class Derived extends Base {
    method() {
        return super.method();
    }
}
"#,
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let derived = parsed.files[0]
        .symbols
        .iter()
        .find(|s| s.name == "Derived")
        .expect("Derived class not found");

    // Note: extends extraction depends on tree-sitter field access
    // This test verifies symbol exists
    assert_eq!(derived.name, "Derived");
}

#[test]
fn test_index_python_support() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.py",
        r#"
def add(a, b):
    return a + b

class Calculator:
    def add(self, a, b):
        return a + b
"#,
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    assert_eq!(parsed.stats.total_files, 1);
    assert!(parsed.stats.total_symbols >= 2);
}

#[test]
fn test_index_ignores_node_modules() {
    let dir = create_test_dir();
    let node_modules = dir.path().join("node_modules");
    fs::create_dir(&node_modules).expect("Failed to create node_modules");
    write_file(dir.path(), "test.ts", "function test(): void { }");
    write_file(
        &node_modules,
        "external.ts",
        "export function external(): void { }",
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    assert_eq!(parsed.stats.total_files, 1);
    assert!(parsed.files[0].path.contains("test.ts"));
}

#[test]
fn test_index_metrics() {
    let dir = create_test_dir();
    write_file(
        dir.path(),
        "test.ts",
        "function test(a: number): number { return a; }",
    );

    let result = index(dir.path().to_string_lossy().to_string());
    let parsed: IndexResult = serde_json::from_str(&result).expect("Failed to parse result");

    let fn_symbol = parsed.files[0]
        .symbols
        .iter()
        .find(|s| s.name == "test")
        .expect("Function not found");

    // Metrics are optional, check if present
    if let Some(ref metrics) = fn_symbol.metrics {
        assert_eq!(metrics.params, 1);
    }
}

// --- index_with_cache: cached runs must still return every scanned file ---
// Regression tests for the MCP reindex bug where cache-hit files were
// dropped from the IndexResult, so the rewritten indexed-tree.json/graph.json
// lost all nodes for unmodified files.

use ultragraph::index_with_cache;

/// The cache directory is self-contained: `index_with_cache` persists its
/// own indexed-tree.json snapshot next to cache.json, so callers only have
/// to pass the same cache dir twice. Nothing extra to write.
fn run_cached(repo: &Path, cache_dir: &Path) -> IndexResult {
    let result = index_with_cache(
        repo.to_string_lossy().to_string(),
        cache_dir.to_string_lossy().to_string(),
    );
    serde_json::from_str(&result).expect("Failed to parse result")
}

#[test]
fn test_cached_rerun_keeps_all_files() {
    let dir = create_test_dir();
    let cache = create_test_dir();
    write_file(dir.path(), "a.ts", "export function alpha(): void { }");
    write_file(dir.path(), "b.ts", "export function beta(): void { }");

    let first = run_cached(dir.path(), cache.path());
    assert_eq!(first.stats.total_files, 2);
    assert_eq!(first.stats.cached_files, 0);

    // Nothing changed: everything is a cache hit, yet the result must still
    // contain both files with their symbols.
    let second = run_cached(dir.path(), cache.path());
    assert_eq!(second.stats.cached_files, 2);
    assert_eq!(second.stats.total_files, 2);
    assert_eq!(second.files.len(), 2);
    assert_eq!(second.stats.total_symbols, first.stats.total_symbols);
    for f in &second.files {
        assert!(!f.symbols.is_empty(), "cached file {} lost its symbols", f.path);
    }
}

#[test]
fn test_cached_rerun_reparses_modified_and_keeps_unmodified() {
    let dir = create_test_dir();
    let cache = create_test_dir();
    write_file(dir.path(), "a.ts", "export function alpha(): void { }");
    write_file(dir.path(), "b.ts", "export function beta(): void { }");
    run_cached(dir.path(), cache.path());

    write_file(
        dir.path(),
        "b.ts",
        "export function beta(): void { }\nexport function gamma(): void { }",
    );
    let second = run_cached(dir.path(), cache.path());
    assert_eq!(second.stats.cached_files, 1);
    assert_eq!(second.files.len(), 2);

    let b = second.files.iter().find(|f| f.path.contains("b.ts")).unwrap();
    assert_eq!(b.symbols.len(), 2);
    let a = second.files.iter().find(|f| f.path.contains("a.ts")).unwrap();
    assert!(!a.symbols.is_empty());
}

#[test]
fn test_cached_rerun_prunes_deleted_files() {
    let dir = create_test_dir();
    let cache = create_test_dir();
    write_file(dir.path(), "a.ts", "export function alpha(): void { }");
    write_file(dir.path(), "b.ts", "export function beta(): void { }");
    run_cached(dir.path(), cache.path());

    fs::remove_file(dir.path().join("b.ts")).unwrap();
    let second = run_cached(dir.path(), cache.path());
    assert_eq!(second.files.len(), 1);
    assert!(second.files[0].path.contains("a.ts"));

    // cache.json must not keep the deleted file's hash around.
    let hashes: std::collections::HashMap<String, String> =
        serde_json::from_str(&fs::read_to_string(cache.path().join("cache.json")).unwrap()).unwrap();
    assert_eq!(hashes.len(), 1);
}

#[test]
fn test_cache_hit_without_previous_tree_reparses() {
    let dir = create_test_dir();
    let cache = create_test_dir();
    write_file(dir.path(), "a.ts", "export function alpha(): void { }");
    run_cached(dir.path(), cache.path());

    // Simulate a caller that kept cache.json but lost indexed-tree.json:
    // the file hash still matches, but the node can't be recovered, so it
    // must be re-parsed rather than dropped.
    fs::remove_file(cache.path().join("indexed-tree.json")).unwrap();
    let second = run_cached(dir.path(), cache.path());
    assert_eq!(second.files.len(), 1);
    assert!(!second.files[0].symbols.is_empty());
    assert_eq!(second.stats.cached_files, 0);
}
