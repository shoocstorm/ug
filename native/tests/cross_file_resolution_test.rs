//! Cross-file call resolution: what a `Calls` edge is allowed to mean.
//!
//! Every other indexer test stages a *single* file, which is exactly the case
//! where name matching cannot go wrong. The defects this file guards were all
//! invisible at that scope:
//!
//! - `.collect()` on a standard-library iterator produced an edge into a
//!   local function that happened to be named `collect` — 246 of them in
//!   this repo's own graph, plus 138 for `.get()` and 99 for `.find()`.
//! - `a::b::c(..)` produced no edge at all, because the resolver split
//!   callee names on `.` and never on `::`. Every Rust module-path call in
//!   the repo was silently absent.
//! - A name declared in three files resolved to whichever was indexed first,
//!   so two of the three callers got a deterministic wrong answer.
//!
//! The assertions therefore come in pairs: an edge that must exist, and an
//! edge that must *not*. A resolver can pass the first half by guessing.

use std::collections::HashSet;
use std::fs;
use tempfile::TempDir;
use ultragraph::types::{GraphData, GraphEdgeType, GraphNode};
use ultragraph::{build_graph, index};

fn stage(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    for (rel, content) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn run(files: &[(&str, &str)]) -> GraphData {
    let dir = stage(files);
    let json = index(dir.path().to_string_lossy().to_string());
    serde_json::from_str(&build_graph(json)).expect("build_graph returned invalid JSON")
}

fn node<'a>(graph: &'a GraphData, name: &str) -> &'a GraphNode {
    let hits: Vec<&GraphNode> = graph.nodes.iter().filter(|n| n.name == name).collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one node named {name:?}, got {:?}",
        hits.iter().map(|n| &n.id).collect::<Vec<_>>()
    );
    hits[0]
}

/// Display names of everything `source` points at along `edge_type`.
fn targets(graph: &GraphData, source: &str, edge_type: GraphEdgeType) -> HashSet<String> {
    let src = node(graph, source);
    let by_id: std::collections::HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect();
    graph
        .edges
        .iter()
        .filter(|e| e.source == src.id && e.edge_type == edge_type)
        .filter_map(|e| by_id.get(e.target.as_str()).map(|s| s.to_string()))
        .collect()
}

// ─── Rust ───────────────────────────────────────────────────────────────

/// `use crate::util;` followed by `util::helper()` is the shape that made up
/// most of the 1,619 `::` paths this repo dropped on the floor: the callee is
/// named completely at the call site, and the old resolver — which only ever
/// split on `.` — matched neither the whole string nor any tail of it.
#[test]
fn rust_a_module_path_call_reaches_the_other_file() {
    let graph = run(&[
        (
            "src/util.rs",
            "pub fn helper() -> u32 { 1 }\npub fn unrelated() -> u32 { 2 }\n",
        ),
        (
            "src/main.rs",
            "use crate::util;\npub fn run() -> u32 { util::helper() }\n",
        ),
    ]);

    let called = targets(&graph, "run", GraphEdgeType::Calls);
    assert!(called.contains("helper"), "got {called:?}");
    assert!(
        !called.contains("unrelated"),
        "resolution must name one function, not a module's worth: {called:?}"
    );
}

/// `mod cli;` is a name binding even though it is not an import, and it is
/// the *only* thing that makes `cli::run()` mean `crate::cli::run`.
///
/// The decoys matter: three modules declare `run`, so the bare-name fallback
/// cannot choose between them and correctly declines. An edge to the right
/// one therefore proves the path resolved, not that a guess got lucky. This
/// is the shape of `main.rs` calling `cli::run()` in this very repo, where
/// the call was silently dropped and `crate::cli::run` read as dead code.
#[test]
fn rust_a_mod_declaration_binds_its_child_module() {
    let graph = run(&[
        ("src/cli/mod.rs", "pub fn run() -> u32 { 1 }\n"),
        ("src/mcp/mod.rs", "pub fn run() -> u32 { 2 }\n"),
        ("src/code_query/mod.rs", "pub fn run() -> u32 { 3 }\n"),
        (
            "src/main.rs",
            "mod cli;\nmod mcp;\nmod code_query;\npub fn start() -> u32 { cli::run() }\n",
        ),
    ]);

    let called = targets(&graph, "start", GraphEdgeType::Calls);
    assert!(called.contains("run"), "no call edge at all: {called:?}");

    // …and to the `run` in cli, not to either decoy.
    let edge = graph
        .edges
        .iter()
        .find(|e| e.edge_type == GraphEdgeType::Calls && e.source.contains("start"))
        .expect("call edge");
    assert!(
        edge.target.contains("src/cli/mod.rs"),
        "resolved to the wrong module: {}",
        edge.target
    );
}

/// A `mod` declaration binds *that one name*, not every path-shaped call in
/// the file. A head segment the file never declared is another crate, and
/// re-rooting it under the current module would match nothing while looking
/// like an answer.
///
/// The decoy `run` is what makes this a test of path resolution: with a
/// single candidate the bare-name fallback would resolve `serde_json::run()`
/// on its own — pre-existing behaviour, and not what is under test here.
#[test]
fn rust_a_mod_declaration_binds_only_the_name_it_declares() {
    let graph = run(&[
        ("src/cli/mod.rs", "pub fn run() -> u32 { 1 }\n"),
        ("src/mcp/mod.rs", "pub fn run() -> u32 { 2 }\n"),
        (
            "src/main.rs",
            "mod cli;\nmod mcp;\npub fn start() -> u32 { serde_json::run() }\n",
        ),
    ]);

    assert!(
        targets(&graph, "start", GraphEdgeType::Calls).is_empty(),
        "an external crate path must not bind to a declared module's function"
    );
}

/// An inline `mod` is flattened by this indexer — its members keep the
/// file's module path — so binding its name would promise a path no symbol
/// carries. The test that matters is that nothing *wrong* is produced.
#[test]
fn rust_an_inline_mod_does_not_invent_a_path() {
    let graph = run(&[
        ("src/other/mod.rs", "pub fn helper() -> u32 { 9 }\n"),
        (
            "src/main.rs",
            "mod other;\n\
             mod helpers { pub fn helper() -> u32 { 1 } }\n\
             pub fn start() -> u32 { other::helper() }\n",
        ),
    ]);

    // `other::helper` is a declared child module and resolves; the inline
    // `helpers` module must not have shadowed or redirected it.
    let edge = graph
        .edges
        .iter()
        .find(|e| e.edge_type == GraphEdgeType::Calls && e.source.contains("start"))
        .expect("call edge");
    assert!(
        edge.target.contains("src/other/mod.rs"),
        "resolved to the wrong helper: {}",
        edge.target
    );
}

/// A fully-rooted path needs no `use` at all.
#[test]
fn rust_a_crate_rooted_path_resolves_without_an_import() {
    let graph = run(&[
        ("src/project.rs", "pub fn read_meta() -> u32 { 7 }\n"),
        (
            "src/main.rs",
            "pub fn run() -> u32 { crate::project::read_meta() }\n",
        ),
    ]);

    assert!(targets(&graph, "run", GraphEdgeType::Calls).contains("read_meta"));
}

/// The `.collect()` bug, reproduced exactly: a repo that declares a function
/// named `collect` must not collect an inbound edge from every iterator
/// chain in the codebase. Note the local `collect` here is *unique* — the
/// first fix (declining to choose between candidates) does not catch this
/// one, because there is only ever one candidate.
#[test]
fn rust_a_stdlib_method_does_not_bind_to_a_local_function_of_the_same_name() {
    let graph = run(&[
        ("src/helpers.rs", "pub fn collect() -> u32 { 0 }\n"),
        (
            "src/main.rs",
            r#"
pub fn run(xs: Vec<u32>) -> Vec<u32> {
    xs.iter().map(|x| x + 1).collect()
}
"#,
        ),
    ]);

    let called = targets(&graph, "run", GraphEdgeType::Calls);
    assert!(
        !called.contains("collect"),
        "`.collect()` is a standard-library method on an untypeable receiver; \
         binding it to a local `collect` is the defect this test exists for: {called:?}"
    );
}

/// Two types declaring the same method name is the case bare-name matching
/// cannot survive. The receiver is typed from the `let` binding.
#[test]
fn rust_a_typed_receiver_picks_the_right_types_method() {
    let graph = run(&[
        (
            "src/store.rs",
            "pub struct Store;\nimpl Store { pub fn save(&self) -> u32 { 1 } }\n",
        ),
        (
            "src/audit.rs",
            "pub struct Audit;\nimpl Audit { pub fn save(&self) -> u32 { 2 } }\n",
        ),
        (
            "src/main.rs",
            r#"
use crate::store::Store;
pub fn run() -> u32 {
    let s = Store {};
    s.save()
}
"#,
        ),
    ]);

    let called = targets(&graph, "run", GraphEdgeType::Calls);
    assert!(called.contains("Store::save"), "got {called:?}");
    assert!(
        !called.contains("Audit::save"),
        "a typed receiver must reach one method, not every method of that name: {called:?}"
    );
}

/// `self` types the receiver from the enclosing `impl`.
#[test]
fn rust_a_self_call_stays_within_its_own_type() {
    let graph = run(&[
        (
            "src/other.rs",
            "pub struct Other;\nimpl Other { pub fn step(&self) -> u32 { 9 } }\n",
        ),
        (
            "src/engine.rs",
            r#"
pub struct Engine;
impl Engine {
    pub fn step(&self) -> u32 { 1 }
    pub fn drive(&self) -> u32 { self.step() }
}
"#,
        ),
    ]);

    let called = targets(&graph, "Engine::drive", GraphEdgeType::Calls);
    assert!(called.contains("Engine::step"), "got {called:?}");
    assert!(!called.contains("Other::step"), "got {called:?}");
}

/// When nothing types the call and the name is declared in several files,
/// the honest answer is no edge. This used to return `candidates[0]`.
#[test]
fn rust_an_ambiguous_bare_name_produces_no_edge_rather_than_a_guess() {
    let graph = run(&[
        ("src/a.rs", "pub fn parse() -> u32 { 1 }\n"),
        ("src/b.rs", "pub fn parse() -> u32 { 2 }\n"),
        (
            "src/main.rs",
            // No `use`, so `parse` could be either — and the file declares
            // no `parse` of its own to prefer.
            "pub fn run() -> u32 { parse() }\n",
        ),
    ]);

    let called = targets(&graph, "run", GraphEdgeType::Calls);
    assert!(
        !called.contains("parse"),
        "two files declare `parse`; picking one is a coin flip: {called:?}"
    );
}

/// The same call *with* an import is no longer ambiguous.
#[test]
fn rust_an_import_disambiguates_a_name_two_files_declare() {
    let graph = run(&[
        ("src/a.rs", "pub fn parse() -> u32 { 1 }\n"),
        ("src/b.rs", "pub fn parse() -> u32 { 2 }\n"),
        (
            "src/main.rs",
            "use crate::b::parse;\npub fn run() -> u32 { parse() }\n",
        ),
    ]);

    let src_b = graph
        .nodes
        .iter()
        .find(|n| n.name == "parse" && n.file.as_deref() == Some("src/b.rs"))
        .expect("src/b.rs should declare parse");
    let run_node = node(&graph, "run");

    let landed: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.source == run_node.id && e.edge_type == GraphEdgeType::Calls)
        .map(|e| e.target.as_str())
        .collect();
    assert_eq!(
        landed,
        vec![src_b.id.as_str()],
        "the import names which `parse` is meant"
    );
}

/// A struct literal constructs a value without calling anything, so it is an
/// `Instantiates` edge to the type rather than a `Calls` edge to a function
/// that does not exist.
#[test]
fn rust_a_struct_literal_instantiates_the_type() {
    let graph = run(&[
        ("src/cfg.rs", "pub struct Config { pub n: u32 }\n"),
        (
            "src/main.rs",
            "use crate::cfg::Config;\npub fn run() -> Config { Config { n: 1 } }\n",
        ),
    ]);

    assert!(targets(&graph, "run", GraphEdgeType::Instantiates).contains("Config"));
    assert!(!targets(&graph, "run", GraphEdgeType::Calls).contains("Config"));
}

/// `impl Trait for Type` is a fact about the type. Recording it on each
/// method instead left `Implements` edges running from functions to traits
/// and gave the implementing type no hierarchy at all.
#[test]
fn rust_a_trait_impl_links_the_type_and_lets_its_methods_override() {
    let graph = run(&[
        (
            "src/store.rs",
            "pub trait Store { fn put(&self) -> u32; }\n",
        ),
        (
            "src/mem.rs",
            r#"
use crate::store::Store;
pub struct Mem;
impl Store for Mem {
    fn put(&self) -> u32 { 1 }
}
"#,
        ),
    ]);

    assert!(
        targets(&graph, "Mem", GraphEdgeType::Implements).contains("Store"),
        "the type implements the trait"
    );
    // The trait's own member is a node too. It was not, before: a trait
    // declaration's methods were never extracted, so the trait was an empty
    // node and `Overrides` had nothing to point at.
    assert!(
        targets(&graph, "Mem::put", GraphEdgeType::Overrides).contains("Store::put"),
        "and its method overrides the trait's declaration"
    );
    // The importing file reaches the imported one. Rust import edges did not
    // resolve at all before: the qualified-import path composed its lookup
    // key with Java's `.`, so `crate::store::Store` was sought as
    // `crate::store.Store` and always missed.
    let importer = graph
        .nodes
        .iter()
        .find(|n| n.name == "src/mem.rs")
        .expect("mem.rs file node");
    let store_file = graph
        .nodes
        .iter()
        .find(|n| n.name == "src/store.rs")
        .expect("store.rs file node");
    assert!(
        graph.edges.iter().any(|e| e.source == importer.id
            && e.target == store_file.id
            && e.edge_type == GraphEdgeType::Imports),
        "a `use crate::store::Store` should link the two files"
    );
}

/// A type's members hang off the type, not only off the file. This is what
/// makes "what is on this struct" a single hop.
#[test]
fn rust_a_type_contains_its_own_methods() {
    let graph = run(&[(
        "src/db.rs",
        "pub struct Db;\nimpl Db { pub fn open(&self) {} pub fn close(&self) {} }\n",
    )]);

    let held = targets(&graph, "Db", GraphEdgeType::Contains);
    assert!(held.contains("Db::open"), "got {held:?}");
    assert!(held.contains("Db::close"), "got {held:?}");
}

/// Qualified names are what every lookup above keys on, so their shape is
/// part of the contract rather than an implementation detail.
#[test]
fn rust_qualified_names_are_rooted_at_the_crate() {
    let graph = run(&[(
        "src/storage/db.rs",
        "pub struct Db;\nimpl Db { pub fn open(&self) {} }\npub fn helper() {}\n",
    )]);

    assert_eq!(
        node(&graph, "Db").qualified_name.as_deref(),
        Some("crate::storage::db::Db")
    );
    assert_eq!(
        node(&graph, "Db::open").qualified_name.as_deref(),
        Some("crate::storage::db::Db#open")
    );
    assert_eq!(
        node(&graph, "helper").qualified_name.as_deref(),
        Some("crate::storage::db::helper")
    );
}

// ─── TypeScript ─────────────────────────────────────────────────────────

/// A named import says which module a call means. Before, the callee was
/// matched by bare name against every symbol in the repo and the import —
/// which the indexer had already parsed — was never consulted.
#[test]
fn ts_a_named_import_resolves_the_call_to_that_module() {
    let graph = run(&[
        (
            "src/util.ts",
            "export function helper(): number { return 1; }\n",
        ),
        ("src/other.ts", "export function helper(): number { return 2; }\n"),
        (
            "src/main.ts",
            "import { helper } from './util';\nexport function run(): number { return helper(); }\n",
        ),
    ]);

    let util_helper = graph
        .nodes
        .iter()
        .find(|n| n.name == "helper" && n.file.as_deref() == Some("src/util.ts"))
        .expect("util helper");
    let run_node = node(&graph, "run");
    let landed: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.source == run_node.id && e.edge_type == GraphEdgeType::Calls)
        .map(|e| e.target.as_str())
        .collect();
    assert_eq!(landed, vec![util_helper.id.as_str()], "the import decides");
}

/// A method reached through a `this` field lands on the declaring class. The
/// constructor-parameter-property shorthand is how most injected TypeScript
/// declares its collaborators, so it has to be understood.
#[test]
fn ts_a_field_typed_receiver_picks_the_right_class() {
    let graph = run(&[
        (
            "src/store.ts",
            "export class Store { save(): number { return 1; } }\n",
        ),
        (
            "src/audit.ts",
            "export class Audit { save(): number { return 2; } }\n",
        ),
        (
            "src/svc.ts",
            r#"
import { Store } from './store';
export class Svc {
  constructor(private store: Store) {}
  run(): number { return this.store.save(); }
}
"#,
        ),
    ]);

    let called = targets(&graph, "Svc.run", GraphEdgeType::Calls);
    assert!(called.contains("Store.save"), "got {called:?}");
    assert!(!called.contains("Audit.save"), "got {called:?}");
}

/// The TypeScript equivalent of the `.collect()` defect.
#[test]
fn ts_a_builtin_method_does_not_bind_to_a_local_function_of_the_same_name() {
    let graph = run(&[
        ("src/helpers.ts", "export function map(): number { return 0; }\n"),
        (
            "src/main.ts",
            "export function run(xs: number[]): number[] { return xs.map(x => x + 1); }\n",
        ),
    ]);

    assert!(
        !targets(&graph, "run", GraphEdgeType::Calls).contains("map"),
        "`xs.map` is Array.prototype.map, not the local `map`"
    );
}

/// A class's methods hang off the class. TypeScript classes had no members in
/// the graph at all, because the walk never tracked which class it was in.
#[test]
fn ts_a_class_contains_its_methods_and_implements_its_interface() {
    let graph = run(&[
        ("src/api.ts", "export interface Api { go(): void; }\n"),
        (
            "src/impl.ts",
            r#"
import { Api } from './api';
export class Svc implements Api {
  go(): void {}
  helper(): void {}
}
"#,
        ),
    ]);

    let held = targets(&graph, "Svc", GraphEdgeType::Contains);
    assert!(held.contains("Svc.go"), "got {held:?}");
    assert!(held.contains("Svc.helper"), "got {held:?}");

    assert!(targets(&graph, "Svc", GraphEdgeType::Implements).contains("Api"));
    assert!(
        targets(&graph, "Svc.go", GraphEdgeType::Overrides).contains("Api.go"),
        "an interface member is a node its implementation can override"
    );
}

/// `new Foo()` constructs; it is not a call to a function named `Foo`.
#[test]
fn ts_a_new_expression_instantiates_the_class() {
    let graph = run(&[
        ("src/store.ts", "export class Store { }\n"),
        (
            "src/main.ts",
            "import { Store } from './store';\nexport function run(): Store { return new Store(); }\n",
        ),
    ]);

    assert!(targets(&graph, "run", GraphEdgeType::Instantiates).contains("Store"));
    assert!(!targets(&graph, "run", GraphEdgeType::Calls).contains("Store"));
}

// ─── Python ─────────────────────────────────────────────────────────────

/// `from pkg.util import helper` says which `helper` is meant.
#[test]
fn py_a_from_import_resolves_the_call_to_that_module() {
    let graph = run(&[
        ("pkg/util.py", "def helper():\n    return 1\n"),
        ("pkg/other.py", "def helper():\n    return 2\n"),
        (
            "pkg/main.py",
            "from pkg.util import helper\n\n\ndef run():\n    return helper()\n",
        ),
    ]);

    let util_helper = graph
        .nodes
        .iter()
        .find(|n| n.name == "helper" && n.file.as_deref() == Some("pkg/util.py"))
        .expect("util helper");
    let run_node = node(&graph, "run");
    let landed: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.source == run_node.id && e.edge_type == GraphEdgeType::Calls)
        .map(|e| e.target.as_str())
        .collect();
    assert_eq!(landed, vec![util_helper.id.as_str()]);
}

/// An annotated `self` attribute types the receiver, so `self.store.save()`
/// reaches one class's method rather than every `save` in the repo.
#[test]
fn py_a_self_attribute_picks_the_right_class() {
    let graph = run(&[
        ("pkg/store.py", "class Store:\n    def save(self):\n        return 1\n"),
        ("pkg/audit.py", "class Audit:\n    def save(self):\n        return 2\n"),
        (
            "pkg/svc.py",
            r#"
from pkg.store import Store


class Svc:
    def __init__(self, store: Store):
        self.store: Store = store

    def run(self):
        return self.store.save()
"#,
        ),
    ]);

    let called = targets(&graph, "Svc.run", GraphEdgeType::Calls);
    assert!(called.contains("Store.save"), "got {called:?}");
    assert!(!called.contains("Audit.save"), "got {called:?}");
}

/// Python's `.get()` on a dict must not bind to a local `get`.
#[test]
fn py_a_builtin_method_does_not_bind_to_a_local_function_of_the_same_name() {
    let graph = run(&[
        ("pkg/helpers.py", "def get():\n    return 0\n"),
        (
            "pkg/main.py",
            "def run(d):\n    return d.get('k')\n",
        ),
    ]);

    assert!(
        !targets(&graph, "run", GraphEdgeType::Calls).contains("get"),
        "`d.get` is a dict method on an untypeable receiver"
    );
}

/// A class's methods hang off the class, and a base class is linked.
#[test]
fn py_a_class_contains_its_methods_and_extends_its_base() {
    let graph = run(&[
        ("pkg/base.py", "class Base:\n    def go(self):\n        pass\n"),
        (
            "pkg/impl.py",
            "from pkg.base import Base\n\n\nclass Svc(Base):\n    def go(self):\n        pass\n\n    def helper(self):\n        pass\n",
        ),
    ]);

    let held = targets(&graph, "Svc", GraphEdgeType::Contains);
    assert!(held.contains("Svc.go"), "got {held:?}");
    assert!(held.contains("Svc.helper"), "got {held:?}");

    assert!(targets(&graph, "Svc", GraphEdgeType::Extends).contains("Base"));
    assert!(
        targets(&graph, "Svc.go", GraphEdgeType::Overrides).contains("Base.go"),
        "a subclass method overrides the base declaration it replaces"
    );
}

/// Python spells construction exactly like invocation, so the capitalisation
/// convention is what separates `Store()` from `helper()`.
#[test]
fn py_calling_a_class_instantiates_it() {
    let graph = run(&[
        ("pkg/store.py", "class Store:\n    pass\n"),
        (
            "pkg/main.py",
            "from pkg.store import Store\n\n\ndef run():\n    return Store()\n",
        ),
    ]);

    assert!(targets(&graph, "run", GraphEdgeType::Instantiates).contains("Store"));
    assert!(!targets(&graph, "run", GraphEdgeType::Calls).contains("Store"));
}

// ─── Uses and DependsOn ─────────────────────────────────────────────────

/// A constant used to have one inbound edge — `Contains`, from its own file —
/// which made every threshold and limit in the repo look like dead code and
/// left "what breaks if I change this cap" unanswerable.
#[test]
fn a_constant_records_who_reads_it() {
    let graph = run(&[
        ("src/limits.rs", "pub const MAX_HOPS: u32 = 8;\n"),
        (
            "src/walk.rs",
            "use crate::limits::MAX_HOPS;\npub fn walk(n: u32) -> bool { n < MAX_HOPS }\n",
        ),
    ]);

    assert!(
        targets(&graph, "walk", GraphEdgeType::Uses).contains("MAX_HOPS"),
        "reading a constant is a dependency on it"
    );
}

/// A lower-case local is not a constant, and a `Uses` edge to every name a
/// body mentions would bury the graph.
#[test]
fn an_ordinary_local_is_not_recorded_as_a_constant_use() {
    let graph = run(&[(
        "src/main.rs",
        "pub fn run() -> u32 { let total = 1; total + 1 }\n",
    )]);

    assert!(targets(&graph, "run", GraphEdgeType::Uses).is_empty());
}

/// `Dependency` was a declared node type that nothing ever created, so the
/// manifest the indexer had already parsed appeared nowhere in the graph.
#[test]
fn a_declared_package_becomes_a_node_its_importers_depend_on() {
    let graph = run(&[
        (
            "package.json",
            r#"{ "name": "app", "dependencies": { "lodash": "^4.17.0" } }"#,
        ),
        (
            "src/main.ts",
            "import { merge } from 'lodash';\nexport function run() { return merge({}, {}); }\n",
        ),
    ]);

    let dep = graph
        .nodes
        .iter()
        .find(|n| n.name == "lodash")
        .expect("lodash should be a Dependency node");
    assert_eq!(dep.node_type, ultragraph::types::GraphNodeType::Dependency);

    let importer = graph
        .nodes
        .iter()
        .find(|n| n.name == "src/main.ts")
        .expect("main.ts file node");
    assert!(
        graph.edges.iter().any(|e| e.source == importer.id
            && e.target == dep.id
            && e.edge_type == GraphEdgeType::DependsOn),
        "the importing file depends on the package"
    );
}

// ─── Staleness ──────────────────────────────────────────────────────────

/// A graph written before cross-file resolution existed does not fail — it
/// answers, with callers a name match invented and module-path calls missing
/// entirely, and the answer looks exactly like a correct one. `graph_schema`
/// is the manifest tool, so it is where that has to be said.
#[test]
fn graph_schema_flags_a_graph_written_before_cross_file_resolution() {
    let fresh = run(&[("src/main.rs", "pub fn run() {}\n")]);
    let path = std::path::Path::new("graph.json");
    assert!(
        !ultragraph::agent_tools::graph_schema(&fresh, path).stale_call_graph,
        "a graph this build just wrote is current"
    );

    let mut old = fresh.clone();
    if let Some(stats) = old.stats.as_mut() {
        stats.graph_schema_version = 2;
    }
    assert!(ultragraph::agent_tools::graph_schema(&old, path).stale_call_graph);

    let rendered = ultragraph::agent_tools::render_graph_schema(
        &ultragraph::agent_tools::graph_schema(&old, path),
        ultragraph::agent_tools::Render::Markdown,
    );
    assert!(
        rendered.contains("ug gen"),
        "the note has to say what to do about it: {rendered}"
    );
}

// ─── Shared ─────────────────────────────────────────────────────────────

/// The call list stored on each node is a display field, and it used to hold
/// the raw source text of the callee expression — including, for a chained
/// call over a closure, the entire closure body. This repo's own graph
/// carried 2,030 such blobs, the longest 2,948 characters.
#[test]
fn rust_the_stored_call_list_holds_names_not_source_text() {
    let graph = run(&[(
        "src/main.rs",
        r#"
pub fn run(xs: Vec<u32>) -> Vec<u32> {
    xs.iter()
        .map(|x| {
            let doubled = x * 2;
            doubled + 1
        })
        .collect()
}
"#,
    )]);

    for call in &node(&graph, "run").calls {
        assert!(
            !call.contains('\n') && call.len() < 40,
            "call entries must be bare callee names, got {call:?}"
        );
    }
}
