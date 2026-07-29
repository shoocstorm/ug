//! Java indexer. Handles `.java`.
//!
//! Maps Java AST node kinds onto the same normalised symbol kinds used by
//! the other indexers (`function`, `class`, `interface`, `variable`) so the
//! graph builder doesn't need a Java-specific branch:
//!
//! - `class_declaration` / `enum_declaration` / `record_declaration` -> class
//! - `interface_declaration` / `annotation_type_declaration` -> interface
//! - `method_declaration` / `constructor_declaration` -> function
//! - `field_declaration` -> variable (one per declarator, so `int a, b;` -> 2)
//!
//! Exports are always empty — Java uses `public`/`protected`/etc. rather than
//! an explicit export concept, mirroring the Python indexer.
//!
//! # Why this indexer carries more machinery than its siblings
//!
//! In TypeScript a file path is an identity: `./utils` resolves to a file,
//! and a symbol's name is nearly unique within it. Java has neither
//! property. Identity lives in the *package*, which is declared in the file
//! rather than implied by its path, and the names people write are
//! deliberately generic — `execute`, `handle`, `save`, `Builder` recur in
//! every layer of a codebase. Three things follow, and each is handled here
//! rather than in the language-agnostic graph builder:
//!
//! 1. **Qualified names** (`Symbol::qualified_name`). Every type gets
//!    `pkg.Outer.Inner` and every member `pkg.Type#member`. Without them
//!    `import com.example.svc.OrderService;` has nothing to resolve against —
//!    the filesystem resolver in `graph.rs` looks for a *path* called
//!    `com.example.svc`, finds nothing, and drops the edge. That silently
//!    cost Java projects their entire file-to-file import layer.
//!
//! 2. **Receiver types** (`CallRef`). `orderRepo.save(x)` and
//!    `auditLog.save(x)` are different calls; recording both as `"save"`
//!    leaves the graph builder guessing between every `save` in the repo.
//!    We keep a per-method environment of declared types (fields, params,
//!    locals) and tag each call site with the type it dispatches on.
//!
//! 3. **Annotations** (`Symbol::annotations`, `Symbol::route`). In a Spring
//!    or JPA codebase the annotation *is* the semantics: what a class is
//!    for, which URL reaches a method, which table a type maps to. None of
//!    it appears in identifiers or Javadoc, so without extracting it the
//!    retrieval layer has no way to answer "where is the endpoint that
//!    cancels an order".
//!
//! Type resolution here is deliberately local — one file's imports, its own
//! declarations and the package it sits in. There is no classpath and no
//! second pass over the repo. Where that guesses wrong it guesses *outward*
//! (a JDK type resolving to a package-local name that matches nothing), and
//! the graph builder falls back to bare-name matching, which is where it
//! started.

use crate::indexer::common::{
    calculate_nesting, extract_params_from_signature, get_node_text, truncate_chars,
};
use crate::indexer::languages::LanguageIndexer;
use crate::types::{
    Annotation, CallRef, ExportInfo, ImportInfo, ImportedItem, Param, Signature, Symbol,
    SymbolMetrics,
};
use std::collections::{HashMap, HashSet};
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

    fn extract_imports(&self, source: &[u8], root: Node) -> Vec<ImportInfo> {
        let mut by_path: HashMap<String, ImportInfo> = HashMap::new();
        for raw in parse_imports(source, root) {
            let item = ImportedItem {
                name: raw.name,
                alias: None,
            };
            by_path
                .entry(raw.package.clone())
                .and_modify(|info| {
                    if !info.imported.iter().any(|i| i.name == item.name) {
                        info.imported.push(item.clone());
                    }
                })
                .or_insert(ImportInfo {
                    path: raw.package,
                    imported: vec![item],
                });
        }
        let mut out: Vec<ImportInfo> = by_path.into_values().collect();
        // Deterministic order — the graph builder walks these to emit edges,
        // and a HashMap's iteration order would make the output shuffle
        // between runs over an otherwise unchanged file.
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn extract_exports(&self, _source: &[u8], _root: Node) -> Vec<ExportInfo> {
        Vec::new()
    }

    fn extract_symbols(&self, source: &[u8], root: Node) -> Vec<Symbol> {
        let ctx = FileCtx::build(source, root);
        let mut symbols = Vec::new();
        let mut stack: Vec<TypeCtx> = Vec::new();
        visit(root, source, &ctx, &mut stack, &mut symbols);
        symbols
    }
}

// ---------------------------------------------------------------------------
// File-level context
// ---------------------------------------------------------------------------

/// One `import` statement, before it is folded into the by-package
/// [`ImportInfo`] shape the rest of the pipeline expects.
struct RawImport {
    /// Qualifier: `com.example.svc` from `import com.example.svc.Order;`.
    package: String,
    /// Trailing identifier, or `*` for an on-demand import.
    name: String,
    is_static: bool,
    is_wildcard: bool,
}

/// Everything one file knows about how to turn a simple type name into a
/// qualified one.
struct FileCtx {
    /// `com.example.svc`, or empty for the default package.
    package: String,
    /// Simple name -> qualified name, from single-type imports.
    imports: HashMap<String, String>,
    /// Packages imported on demand (`import java.util.*;`).
    wildcards: Vec<String>,
    /// Statically imported member -> the type that declares it, so an
    /// unqualified `assertTrue(...)` still lands on `org.junit.Assert`.
    static_imports: HashMap<String, String>,
    /// Simple names of every type declared anywhere in this file.
    local_types: HashSet<String>,
}

/// Types that are in scope without an import. Not the whole of `java.lang` —
/// just the names common enough that resolving them to a package-local
/// qualified name would be visibly wrong in the graph.
const JAVA_LANG_TYPES: &[&str] = &[
    "Object",
    "String",
    "Integer",
    "Long",
    "Double",
    "Float",
    "Boolean",
    "Byte",
    "Short",
    "Character",
    "Number",
    "Math",
    "System",
    "Thread",
    "Runnable",
    "Exception",
    "RuntimeException",
    "Error",
    "Throwable",
    "Class",
    "Enum",
    "Iterable",
    "Comparable",
    "CharSequence",
    "StringBuilder",
    "StringBuffer",
    "Void",
];

/// Type expressions that never name a user type.
const PRIMITIVES: &[&str] = &[
    "int", "long", "double", "float", "boolean", "char", "byte", "short", "void", "var",
];

impl FileCtx {
    fn build(source: &[u8], root: Node) -> Self {
        let mut ctx = FileCtx {
            package: extract_package(source, root),
            imports: HashMap::new(),
            wildcards: Vec::new(),
            static_imports: HashMap::new(),
            local_types: HashSet::new(),
        };

        for raw in parse_imports(source, root) {
            if raw.is_wildcard {
                ctx.wildcards.push(raw.package);
            } else if raw.is_static {
                // `import static a.b.C.member;` — the qualifier is the type.
                ctx.static_imports.insert(raw.name, raw.package);
            } else {
                let fqn = format!("{}.{}", raw.package, raw.name);
                ctx.imports.insert(raw.name, fqn);
            }
        }

        collect_local_types(root, source, &mut ctx.local_types);
        ctx
    }

    /// Qualified name for a type as written in source, or `None` when the
    /// expression names no type at all (a primitive, `var`, an empty slot).
    ///
    /// Resolution order follows Java's own, with one deliberate deviation:
    /// the current package is tried *before* on-demand (`.*`) imports.
    /// Getting that backwards would qualify a same-package `Order` as
    /// `java.util.Order`; getting it this way qualifies `List` as
    /// `com.example.List`. Both are wrong for one case — but the first loses
    /// an edge between two types that are actually in the graph, while the
    /// second produces a name that matches nothing, which is exactly what a
    /// JDK type should do.
    fn resolve_type(&self, raw: &str) -> Option<String> {
        let base = base_type_name(raw);
        if base.is_empty() || PRIMITIVES.contains(&base.as_str()) {
            return None;
        }

        // Already qualified, or a nested reference like `Map.Entry`.
        if let Some((head, rest)) = base.split_once('.') {
            return match self.imports.get(head) {
                Some(head_fqn) => Some(format!("{}.{}", head_fqn, rest)),
                None => Some(base),
            };
        }

        if let Some(fqn) = self.imports.get(&base) {
            return Some(fqn.clone());
        }
        if self.local_types.contains(&base) {
            return Some(self.qualify(&base));
        }
        if JAVA_LANG_TYPES.contains(&base.as_str()) {
            return Some(format!("java.lang.{}", base));
        }
        if !self.package.is_empty() {
            return Some(self.qualify(&base));
        }
        if self.wildcards.len() == 1 {
            return Some(format!("{}.{}", self.wildcards[0], base));
        }
        Some(base)
    }

    /// Prefix `name` with the file's package.
    fn qualify(&self, name: &str) -> String {
        if self.package.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", self.package, name)
        }
    }
}

/// `package com.example.svc;` -> `com.example.svc`.
fn extract_package(source: &[u8], root: Node) -> String {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "package_declaration" {
            continue;
        }
        let mut inner = child.walk();
        for part in child.children(&mut inner) {
            if matches!(part.kind(), "scoped_identifier" | "identifier") {
                return get_node_text(Some(part), source).unwrap_or_default();
            }
        }
    }
    String::new()
}

/// Walk the top-level `import_declaration` nodes.
///
/// The grammar gives us the dotted path as a single `scoped_identifier`, an
/// `asterisk` node for on-demand imports, and a bare `static` keyword token
/// we detect by scanning the statement's children.
fn parse_imports(source: &[u8], root: Node) -> Vec<RawImport> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }

        let mut path: Option<String> = None;
        let mut is_wildcard = false;
        let mut is_static = false;
        let mut inner = child.walk();
        for part in child.children(&mut inner) {
            match part.kind() {
                "scoped_identifier" | "identifier" => {
                    if path.is_none() {
                        path = get_node_text(Some(part), source);
                    }
                }
                "asterisk" => is_wildcard = true,
                "static" => is_static = true,
                _ => {}
            }
        }

        let Some(full) = path.filter(|p| !p.is_empty()) else {
            continue;
        };

        if is_wildcard {
            out.push(RawImport {
                package: full,
                name: "*".to_string(),
                is_static,
                is_wildcard: true,
            });
            continue;
        }

        // Split on the final `.` so `package` is the qualifier and `name` is
        // the imported identifier. A no-dot import (rare in Java but
        // syntactically possible) keeps the whole string as both, so the
        // symbol is still indexable.
        let (package, name) = match full.rfind('.') {
            Some(idx) => (full[..idx].to_string(), full[idx + 1..].to_string()),
            None => (full.clone(), full.clone()),
        };
        out.push(RawImport {
            package,
            name,
            is_static,
            is_wildcard: false,
        });
    }
    out
}

/// Simple names of every type declared in the file, nested ones included.
/// Needed before the main walk so a type referenced above its own
/// declaration still resolves.
fn collect_local_types(node: Node, source: &[u8], out: &mut HashSet<String>) {
    if is_type_decl(node.kind()) {
        if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
            out.insert(name);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_local_types(child, source, out);
    }
}

fn is_type_decl(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

/// Strip generics, array dimensions and varargs, leaving the bare type name.
/// `Map<String, List<Order>>[]` -> `Map`, `Order...` -> `Order`.
fn base_type_name(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(idx) = s.find('<') {
        s = &s[..idx];
    }
    s.trim()
        .trim_end_matches("...")
        .replace("[]", "")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Type scope
// ---------------------------------------------------------------------------

/// The enclosing type while walking its body.
struct TypeCtx {
    /// Qualified name: `com.example.svc.OrderService`.
    fqn: String,
    /// Display name relative to the package: `OrderService`, or
    /// `Outer.Inner` for a nested type.
    display: String,
    /// Field and record-component name -> qualified type, the starting
    /// environment for every method in this type.
    fields: HashMap<String, String>,
    /// Qualified supertypes, superclass first.
    supers: Vec<String>,
    /// Base path contributed by a type-level `@RequestMapping` / `@Path`.
    route_prefix: Option<String>,
}

fn visit(node: Node, source: &[u8], ctx: &FileCtx, stack: &mut Vec<TypeCtx>, out: &mut Vec<Symbol>) {
    if is_type_decl(node.kind()) {
        visit_type_decl(node, source, ctx, stack, out);
        return;
    }

    extract_member(&node, source, ctx, stack.last(), out);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, ctx, stack, out);
    }
}

fn visit_type_decl(
    node: Node,
    source: &[u8],
    ctx: &FileCtx,
    stack: &mut Vec<TypeCtx>,
    out: &mut Vec<Symbol>,
) {
    let Some(simple) = get_node_text(node.child_by_field_name("name"), source) else {
        return;
    };
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;

    let display = match stack.last() {
        Some(parent) => format!("{}.{}", parent.display, simple),
        None => simple.clone(),
    };
    let fqn = ctx.qualify(&display);

    let extends = resolve_all(&extract_class_extends(&node, source), ctx);
    let implements = resolve_all(&extract_implements(&node, source), ctx);
    let annotations = extract_annotations(&node, source);
    let route_prefix = mapping_path(&annotations);

    let symbol_kind = match kind {
        "interface_declaration" | "annotation_type_declaration" => "interface",
        _ => "class",
    };

    out.push(Symbol {
        id: format!("{}:{}:{}", symbol_kind, start, display),
        name: display.clone(),
        kind: symbol_kind.to_string(),
        file: String::new(),
        start_line: start,
        end_line: end,
        docstring: extract_javadoc(&node, source),
        signature: None,
        extends: extends.clone(),
        implements: implements.clone(),
        qualified_name: Some(fqn.clone()),
        owner: stack.last().map(|p| p.fqn.clone()),
        annotations,
        ..Default::default()
    });

    let mut supers = extends;
    supers.extend(implements);

    let type_ctx = TypeCtx {
        fqn,
        display,
        fields: collect_fields(&node, source, ctx),
        supers,
        route_prefix,
    };

    stack.push(type_ctx);
    // Only the body is walked: a type's own `modifiers`, `superclass` and
    // `interfaces` children have already been consumed above, and descending
    // into them would re-read annotation arguments as if they were code.
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            visit(child, source, ctx, stack, out);
        }
    }
    // Record components declare state but live on the declaration rather
    // than in the body, so they are emitted here rather than by the walk.
    if kind == "record_declaration" {
        emit_record_components(&node, source, stack.last(), out);
    }
    stack.pop();
}

/// Field and record-component types for one type declaration, used as the
/// base environment when typing call receivers inside its methods.
///
/// This is what makes dependency injection resolvable: an `@Autowired
/// OrderRepository repo` field is exactly the receiver of `repo.save(x)`,
/// and once its declared type is known the call lands on the interface —
/// from where the graph builder fans out to implementations.
fn collect_fields(node: &Node, source: &[u8], ctx: &FileCtx) -> HashMap<String, String> {
    let mut fields = HashMap::new();

    // Record components.
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if child.kind() != "formal_parameter" {
                continue;
            }
            if let (Some(name), Some(ty)) = (
                get_node_text(child.child_by_field_name("name"), source),
                get_node_text(child.child_by_field_name("type"), source),
            ) {
                if let Some(fqn) = ctx.resolve_type(&ty) {
                    fields.insert(name, fqn);
                }
            }
        }
    }

    let Some(body) = node.child_by_field_name("body") else {
        return fields;
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(ty) = get_node_text(child.child_by_field_name("type"), source) else {
            continue;
        };
        let Some(fqn) = ctx.resolve_type(&ty) else {
            continue;
        };
        let mut decls = child.walk();
        for decl in child.children(&mut decls) {
            if decl.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name) = get_node_text(decl.child_by_field_name("name"), source) {
                fields.insert(name, fqn.clone());
            }
        }
    }
    fields
}

fn emit_record_components(
    node: &Node,
    source: &[u8],
    owner: Option<&TypeCtx>,
    out: &mut Vec<Symbol>,
) {
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    let start = (params.start_position().row + 1) as u32;
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        let Some(name) = get_node_text(child.child_by_field_name("name"), source) else {
            continue;
        };
        push_variable(
            &name,
            start,
            (child.end_position().row + 1) as u32,
            None,
            extract_annotations(&child, source),
            owner,
            out,
        );
    }
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

fn extract_member(
    node: &Node,
    source: &[u8],
    ctx: &FileCtx,
    owner: Option<&TypeCtx>,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;

    match kind {
        "method_declaration" | "constructor_declaration" => {
            let name_text = get_node_text(node.child_by_field_name("name"), source).or_else(|| {
                // Constructors lack a `name` field on some grammar versions —
                // fall back to the enclosing type's own name.
                if kind == "constructor_declaration" {
                    owner.map(|o| simple_of(&o.display))
                } else {
                    None
                }
            });
            let Some(simple) = name_text else {
                return;
            };

            let params = extract_params(node, source);
            let return_type = get_node_text(node.child_by_field_name("type"), source);
            let annotations = extract_annotations(node, source);
            let (calls, call_refs) = extract_calls(node, source, ctx, owner, &params);

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

            // Constructors are addressed as `<init>` so `new Foo(...)` at a
            // call site resolves by qualified name like any other call, while
            // the display name stays what a reader would search for.
            let member = if kind == "constructor_declaration" {
                "<init>"
            } else {
                simple.as_str()
            };

            let name = qualified_display(owner, &simple);
            out.push(Symbol {
                id: format!("fn:{}:{}", start, name),
                name,
                kind: "function".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring: extract_javadoc(node, source),
                signature: Some(Signature {
                    params,
                    return_type,
                }),
                calls,
                metrics: Some(metrics),
                qualified_name: owner.map(|o| format!("{}#{}", o.fqn, member)),
                owner: owner.map(|o| o.fqn.clone()),
                route: http_route(owner, &annotations),
                annotations,
                call_refs,
                ..Default::default()
            });
        }
        "field_declaration" => {
            // A single `field_declaration` can declare multiple variables
            // (`int a, b, c;`); walk every `variable_declarator` so each
            // becomes its own symbol.
            let annotations = extract_annotations(node, source);
            let docstring = extract_javadoc(node, source);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name) = get_node_text(child.child_by_field_name("name"), source) else {
                    continue;
                };
                push_variable(
                    &name,
                    start,
                    end,
                    docstring.clone(),
                    annotations.clone(),
                    owner,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn push_variable(
    simple: &str,
    start: u32,
    end: u32,
    docstring: Option<String>,
    annotations: Vec<Annotation>,
    owner: Option<&TypeCtx>,
    out: &mut Vec<Symbol>,
) {
    let name = qualified_display(owner, simple);
    out.push(Symbol {
        id: format!("var:{}:{}", start, name),
        name,
        kind: "variable".to_string(),
        file: String::new(),
        start_line: start,
        end_line: end,
        docstring,
        qualified_name: owner.map(|o| format!("{}#{}", o.fqn, simple)),
        owner: owner.map(|o| o.fqn.clone()),
        annotations,
        ..Default::default()
    });
}

/// `OrderService.cancel` — the display name a member carries in the graph.
/// Prefixing with the owning type is what keeps five different `execute`
/// methods distinguishable in search results and in node ids.
fn qualified_display(owner: Option<&TypeCtx>, simple: &str) -> String {
    match owner {
        Some(o) => format!("{}.{}", o.display, simple),
        None => simple.to_string(),
    }
}

fn simple_of(display: &str) -> String {
    display.rsplit('.').next().unwrap_or(display).to_string()
}

fn resolve_all(names: &[String], ctx: &FileCtx) -> Vec<String> {
    names.iter().filter_map(|n| ctx.resolve_type(n)).collect()
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

/// Callee names plus their resolved receivers for one method body.
///
/// The returned `Vec<String>` keeps the old contract — deduped bare callee
/// names, used for display and for the language-agnostic fallback in the
/// graph builder. The `Vec<CallRef>` is the new one: one entry per call
/// site, tagged with the type it dispatches on wherever that was knowable.
fn extract_calls(
    node: &Node,
    source: &[u8],
    ctx: &FileCtx,
    owner: Option<&TypeCtx>,
    params: &[Param],
) -> (Vec<String>, Vec<CallRef>) {
    let mut env: HashMap<String, String> = owner.map(|o| o.fields.clone()).unwrap_or_default();
    for p in params {
        if let Some(fqn) = p.param_type.as_ref().and_then(|t| ctx.resolve_type(t)) {
            env.insert(p.name.clone(), fqn);
        }
    }

    let mut calls = Vec::new();
    let mut refs = Vec::new();
    collect_calls(node, source, ctx, owner, &mut env, &mut calls, &mut refs);
    (calls, refs)
}

#[allow(clippy::too_many_arguments)]
fn collect_calls(
    node: &Node,
    source: &[u8],
    ctx: &FileCtx,
    owner: Option<&TypeCtx>,
    env: &mut HashMap<String, String>,
    calls: &mut Vec<String>,
    refs: &mut Vec<CallRef>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // Locals are recorded as they are met, so a variable declared
            // halfway down a method types the calls below it. There is no
            // block scoping here: shadowing in a sibling block would leave
            // whichever declaration came last, which is a cheaper mistake
            // than not typing the receiver at all.
            "local_variable_declaration" => {
                if let Some(ty) = get_node_text(child.child_by_field_name("type"), source) {
                    if let Some(fqn) = ctx.resolve_type(&ty) {
                        let mut decls = child.walk();
                        for decl in child.children(&mut decls) {
                            if decl.kind() != "variable_declarator" {
                                continue;
                            }
                            if let Some(name) =
                                get_node_text(decl.child_by_field_name("name"), source)
                            {
                                env.insert(name, fqn.clone());
                            }
                        }
                    }
                }
            }
            "method_invocation" => {
                if let Some(name) = get_node_text(child.child_by_field_name("name"), source) {
                    push_call(calls, &name);
                    refs.push(CallRef {
                        owner_type: invocation_owner(&child, source, ctx, owner, env, &name),
                        argc: argument_count(&child),
                        name,
                    });
                }
            }
            "object_creation_expression" => {
                if let Some(ty) = get_node_text(child.child_by_field_name("type"), source) {
                    push_call(calls, &base_type_name(&ty));
                    refs.push(CallRef {
                        name: "<init>".to_string(),
                        owner_type: ctx.resolve_type(&ty),
                        argc: argument_count(&child),
                    });
                }
            }
            _ => {}
        }
        collect_calls(&child, source, ctx, owner, env, calls, refs);
    }
}

fn push_call(calls: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !calls.iter().any(|c| c == name) {
        calls.push(name.to_string());
    }
}

fn argument_count(call: &Node) -> u32 {
    call.child_by_field_name("arguments")
        .map(|a| a.named_child_count() as u32)
        .unwrap_or(0)
}

/// Qualified type a `method_invocation` dispatches on, or `None` when the
/// receiver is an expression we can't type (a chained call, a lambda
/// parameter, an array element).
fn invocation_owner(
    call: &Node,
    source: &[u8],
    ctx: &FileCtx,
    owner: Option<&TypeCtx>,
    env: &HashMap<String, String>,
    callee: &str,
) -> Option<String> {
    let Some(object) = call.child_by_field_name("object") else {
        // Unqualified: a static import if one matches, otherwise `this`.
        return ctx
            .static_imports
            .get(callee)
            .cloned()
            .or_else(|| owner.map(|o| o.fqn.clone()));
    };

    match object.kind() {
        "this" => owner.map(|o| o.fqn.clone()),
        "super" => owner.and_then(|o| o.supers.first().cloned()),
        "identifier" => {
            let text = get_node_text(Some(object), source)?;
            if let Some(fqn) = env.get(&text) {
                return Some(fqn.clone());
            }
            // Not a variable in scope: by Java convention a capitalised bare
            // identifier in receiver position is a type, i.e. a static call.
            if text.chars().next().is_some_and(|c| c.is_uppercase()) {
                return ctx.resolve_type(&text);
            }
            None
        }
        "field_access" => {
            // `this.repo.save(...)` — the only field access we can type
            // without following arbitrary expressions.
            let obj = object.child_by_field_name("object")?;
            if obj.kind() != "this" {
                return None;
            }
            let field = get_node_text(object.child_by_field_name("field"), source)?;
            env.get(&field).cloned()
        }
        "scoped_identifier" => ctx.resolve_type(&get_node_text(Some(object), source)?),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

/// Cap on stored annotation argument text. Long enough for a `@Query` with a
/// real statement in it, short enough that a giant array literal can't
/// dominate the node's retrieval text.
const MAX_ANNOTATION_ARGS: usize = 400;

/// Annotations on a declaration, read from its `modifiers` child.
fn extract_annotations(node: &Node, source: &[u8]) -> Vec<Annotation> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut inner = child.walk();
        for m in child.children(&mut inner) {
            match m.kind() {
                "marker_annotation" => {
                    if let Some(name) = annotation_name(&m, source) {
                        out.push(Annotation { name, args: None });
                    }
                }
                "annotation" => {
                    if let Some(name) = annotation_name(&m, source) {
                        let args = get_node_text(m.child_by_field_name("arguments"), source)
                            .map(|a| {
                                let trimmed =
                                    a.trim().trim_start_matches('(').trim_end_matches(')').trim();
                                truncate_chars(trimmed, MAX_ANNOTATION_ARGS)
                            })
                            .filter(|a| !a.is_empty());
                        out.push(Annotation { name, args });
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Simple name of an annotation: `@org.junit.Test` and `@Test` both give
/// `Test`, which is how they are written and searched for.
fn annotation_name(node: &Node, source: &[u8]) -> Option<String> {
    let raw = get_node_text(node.child_by_field_name("name"), source)?;
    Some(raw.rsplit('.').next().unwrap_or(&raw).to_string())
}

/// Spring's `@RequestMapping`/`@GetMapping` family and JAX-RS's `@Path`, as
/// used on a type to prefix every route its methods declare.
const PATH_ANNOTATIONS: &[&str] = &[
    "RequestMapping",
    "GetMapping",
    "PostMapping",
    "PutMapping",
    "DeleteMapping",
    "PatchMapping",
    "Path",
];

fn mapping_path(annotations: &[Annotation]) -> Option<String> {
    let ann = annotations
        .iter()
        .find(|a| PATH_ANNOTATIONS.contains(&a.name.as_str()))?;
    let args = ann.args.as_deref()?;
    named_or_first_string(args, &["path", "value"])
}

/// The HTTP verb an annotation implies, or `None` if it isn't a mapping.
fn http_verb(ann: &Annotation) -> Option<&'static str> {
    match ann.name.as_str() {
        "GetMapping" => Some("GET"),
        "PostMapping" => Some("POST"),
        "PutMapping" => Some("PUT"),
        "DeleteMapping" => Some("DELETE"),
        "PatchMapping" => Some("PATCH"),
        // JAX-RS puts the verb in a marker annotation of its own.
        "GET" => Some("GET"),
        "POST" => Some("POST"),
        "PUT" => Some("PUT"),
        "DELETE" => Some("DELETE"),
        "PATCH" => Some("PATCH"),
        "HEAD" => Some("HEAD"),
        "OPTIONS" => Some("OPTIONS"),
        "RequestMapping" => Some(
            ann.args
                .as_deref()
                .and_then(|a| named_ident(a, "method"))
                .map(|m| verb_from_request_method(&m))
                .unwrap_or("ANY"),
        ),
        _ => None,
    }
}

fn verb_from_request_method(raw: &str) -> &'static str {
    match raw.rsplit('.').next().unwrap_or(raw) {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "ANY",
    }
}

/// Effective route for a handler method: the type-level prefix joined with
/// whatever the method itself declares.
///
/// This is the single highest-value string a Java indexer can produce for
/// retrieval. `GET /api/orders/{id}` appears nowhere in the identifiers, the
/// Javadoc or the body — but it is exactly what someone types when they go
/// looking for the code behind an endpoint.
fn http_route(owner: Option<&TypeCtx>, annotations: &[Annotation]) -> Option<String> {
    let mapping = annotations.iter().find(|a| http_verb(a).is_some())?;
    let verb = http_verb(mapping)?;
    let own = mapping
        .args
        .as_deref()
        .and_then(|a| named_or_first_string(a, &["path", "value"]));

    let prefix = owner
        .and_then(|o| o.route_prefix.clone())
        .unwrap_or_default();
    Some(format!("{} {}", verb, join_route(&prefix, own.as_deref())))
}

/// Join a type-level prefix and a member-level path into one clean path.
fn join_route(prefix: &str, own: Option<&str>) -> String {
    let a = prefix.trim().trim_end_matches('/');
    let b = own.unwrap_or("").trim();
    let b = b.trim_start_matches('/').trim_end_matches('/');
    let lead = |s: &str| {
        if s.starts_with('/') {
            s.to_string()
        } else {
            format!("/{}", s)
        }
    };
    match (a.is_empty(), b.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => lead(b),
        (false, true) => lead(a),
        (false, false) => format!("{}/{}", lead(a), b),
    }
}

/// Value of `key = "…"` in an annotation argument list, falling back to the
/// first bare string literal (`@GetMapping("/x")` has no key at all).
pub(crate) fn named_or_first_string(args: &str, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(v) = named_string(args, key) {
            return Some(v);
        }
    }
    first_string_literal(args)
}

/// Value of `key = "…"`, or `None`. Matches on a whole identifier so `value`
/// doesn't also match inside `defaultValue`.
pub(crate) fn named_string(args: &str, key: &str) -> Option<String> {
    let idx = find_key(args, key)?;
    let rest = args[idx..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    // `topics = {"a", "b"}` — take the first element of an array form.
    let rest = rest.strip_prefix('{').unwrap_or(rest);
    first_string_literal(rest)
}

/// Value of `key = Some.Ident`, for arguments that aren't strings
/// (`method = RequestMethod.GET`).
fn named_ident(args: &str, key: &str) -> Option<String> {
    let idx = find_key(args, key)?;
    let rest = args[idx..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('{').unwrap_or(rest).trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '.' || c == '_'))
        .unwrap_or(rest.len());
    let ident = &rest[..end];
    if ident.is_empty() {
        None
    } else {
        Some(ident.to_string())
    }
}

/// Byte offset just past `key`, when `key` appears as a whole identifier.
fn find_key(args: &str, key: &str) -> Option<usize> {
    let bytes = args.as_bytes();
    let mut from = 0;
    while let Some(rel) = args[from..].find(key) {
        let start = from + rel;
        let end = start + key.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return Some(end);
        }
        from = end;
    }
    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// First double-quoted literal in `s`, honouring `\` escapes.
pub(crate) fn first_string_literal(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let mut out = String::new();
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Javadoc
// ---------------------------------------------------------------------------

/// Javadoc block immediately preceding a declaration.
///
/// The shared [`crate::indexer::common::extract_docstring`] scans a fixed
/// 200-byte window backwards from the node, which for Java truncates any
/// real Javadoc and, on an annotated declaration, usually finds only the
/// annotations. Walking the previous sibling instead gets the whole comment
/// regardless of length or of how many annotations sit in between — the
/// annotations are *inside* the declaration node, not before it.
fn extract_javadoc(node: &Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if !matches!(prev.kind(), "block_comment" | "comment") {
        return None;
    }
    let text = get_node_text(Some(prev), source)?;
    if !text.starts_with("/**") {
        return None;
    }
    clean_javadoc(&text)
}

/// Strip the comment framing and flatten the block tags into prose.
fn clean_javadoc(text: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line
            .trim()
            .trim_start_matches("/**")
            .trim_start_matches("*/")
            .trim_start_matches('*')
            .trim()
            .trim_end_matches("*/")
            .trim();
        if line.is_empty() {
            continue;
        }
        parts.push(rewrite_tag(line));
    }
    let joined = parts.join(" ").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// `@param id the order id` -> `param id: the order id`.
///
/// Keeping the name and the prose together is what makes a parameter
/// description searchable. The shared JSDoc implementation split `@param` on
/// the first `-` and kept only what came before it — a TypeScript convention
/// that Javadoc doesn't use, so it discarded the description entirely.
fn rewrite_tag(line: &str) -> String {
    let Some(rest) = line.strip_prefix('@') else {
        return line.to_string();
    };
    let (tag, body) = match rest.split_once(char::is_whitespace) {
        Some((t, b)) => (t, b.trim()),
        None => (rest, ""),
    };
    match tag {
        "param" => match body.split_once(char::is_whitespace) {
            Some((name, desc)) => format!("param {}: {}", name, desc.trim()),
            None => format!("param {}", body),
        },
        "return" | "returns" => format!("returns: {}", body),
        "throws" | "exception" => match body.split_once(char::is_whitespace) {
            Some((ex, desc)) => format!("throws {}: {}", ex, desc.trim()),
            None => format!("throws {}", body),
        },
        "deprecated" => format!("deprecated: {}", body),
        // `@see`, `@since`, `@author` and friends keep their word — it reads
        // fine inline and costs nothing.
        _ => format!("{} {}", tag, body).trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Signature / hierarchy
// ---------------------------------------------------------------------------

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
            let name = get_node_text(child.child_by_field_name("name"), source)
                .or_else(|| {
                    // `spread_parameter` wraps its name in a declarator.
                    let mut c = child.walk();
                    let found = child
                        .named_children(&mut c)
                        .find(|n| n.kind() == "variable_declarator");
                    found.and_then(|d| get_node_text(d.child_by_field_name("name"), source))
                })
                .unwrap_or_default();
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

/// `class Foo extends Bar { … }` -> `["Bar"]`, and the interface form
/// `interface Foo extends A, B` -> `["A", "B"]`.
///
/// Generic arguments are stripped: `extends AbstractRepo<Order>` has to
/// yield `AbstractRepo`, or it matches no declared type anywhere.
fn extract_class_extends(node: &Node, source: &[u8]) -> Vec<String> {
    // A class's superclass is a named field; an interface's `extends` list is
    // an unnamed `extends_interfaces` child, so it has to be found by kind.
    let target = node
        .child_by_field_name("superclass")
        .or_else(|| child_of_kind(node, "extends_interfaces"));
    let Some(target) = target else {
        return Vec::new();
    };
    let Some(text) = get_node_text(Some(target), source) else {
        return Vec::new();
    };
    split_type_list(text.trim().trim_start_matches("extends"))
}

fn child_of_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

/// `class Foo implements I1, I2 { … }` -> `["I1", "I2"]`.
fn extract_implements(node: &Node, source: &[u8]) -> Vec<String> {
    let Some(interfaces) = node
        .child_by_field_name("interfaces")
        .or_else(|| child_of_kind(node, "super_interfaces"))
    else {
        return Vec::new();
    };
    let Some(text) = get_node_text(Some(interfaces), source) else {
        return Vec::new();
    };
    split_type_list(text.trim().trim_start_matches("implements"))
}

/// Split `A<X, Y>, B` on the top-level commas only — a naive `split(',')`
/// would tear `Map<String, Order>` in half.
fn split_type_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut buf = String::new();
    for ch in raw.chars() {
        match ch {
            '<' => {
                depth += 1;
                buf.push(ch);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                buf.push(ch);
            }
            ',' if depth == 0 => {
                push_type(&mut out, &buf);
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    push_type(&mut out, &buf);
    out
}

fn push_type(out: &mut Vec<String>, raw: &str) {
    let name = base_type_name(raw);
    if !name.is_empty() {
        out.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> (Vec<Symbol>, Vec<ImportInfo>) {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_java::language()).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        let idx = JavaIndexer;
        (
            idx.extract_symbols(src.as_bytes(), root),
            idx.extract_imports(src.as_bytes(), root),
        )
    }

    fn find<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {:?}", names(symbols)))
    }

    fn names(symbols: &[Symbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn members_are_qualified_by_their_type_and_package() {
        let (symbols, _) = parse(
            r#"
            package com.example.svc;
            public class OrderService {
                private String id;
                public void cancel(String reason) {}
            }
            "#,
        );

        let class = find(&symbols, "OrderService");
        assert_eq!(
            class.qualified_name.as_deref(),
            Some("com.example.svc.OrderService")
        );
        assert_eq!(class.owner, None);

        let method = find(&symbols, "OrderService.cancel");
        assert_eq!(
            method.qualified_name.as_deref(),
            Some("com.example.svc.OrderService#cancel")
        );
        assert_eq!(method.owner.as_deref(), Some("com.example.svc.OrderService"));

        let field = find(&symbols, "OrderService.id");
        assert_eq!(
            field.qualified_name.as_deref(),
            Some("com.example.svc.OrderService#id")
        );
    }

    #[test]
    fn a_nested_type_carries_its_outer_type_in_both_names() {
        let (symbols, _) = parse(
            r#"
            package com.example;
            class Outer {
                static class Inner {
                    void go() {}
                }
            }
            "#,
        );
        let inner = find(&symbols, "Outer.Inner");
        assert_eq!(
            inner.qualified_name.as_deref(),
            Some("com.example.Outer.Inner")
        );
        assert_eq!(inner.owner.as_deref(), Some("com.example.Outer"));
        let go = find(&symbols, "Outer.Inner.go");
        assert_eq!(
            go.qualified_name.as_deref(),
            Some("com.example.Outer.Inner#go")
        );
    }

    #[test]
    fn a_constructor_is_addressed_as_init_but_displays_as_its_type() {
        let (symbols, _) = parse(
            r#"
            package com.example;
            class Order {
                Order(String id) {}
            }
            "#,
        );
        let ctor = find(&symbols, "Order.Order");
        assert_eq!(
            ctor.qualified_name.as_deref(),
            Some("com.example.Order#<init>")
        );
    }

    #[test]
    fn supertypes_are_resolved_and_stripped_of_generics() {
        let (symbols, _) = parse(
            r#"
            package com.example.svc;
            import com.example.base.AbstractRepo;
            import com.example.api.OrderApi;
            class OrderService extends AbstractRepo<Order, String> implements OrderApi, Closeable {}
            "#,
        );
        let class = find(&symbols, "OrderService");
        // The generic argument used to travel with the name, so `extends`
        // matched no declared type at all.
        assert_eq!(class.extends, vec!["com.example.base.AbstractRepo"]);
        assert_eq!(
            class.implements,
            vec!["com.example.api.OrderApi", "com.example.svc.Closeable"]
        );
    }

    #[test]
    fn a_call_is_tagged_with_the_type_of_its_receiver() {
        let (symbols, _) = parse(
            r#"
            package com.example.svc;
            import com.example.repo.OrderRepository;
            import com.example.audit.AuditLog;
            class OrderService {
                private OrderRepository repo;
                private AuditLog audit;
                void cancel(String id) {
                    repo.save(id);
                    audit.save(id);
                    this.helper();
                    helper();
                }
                void helper() {}
            }
            "#,
        );
        let cancel = find(&symbols, "OrderService.cancel");
        // Both `save` calls used to be indistinguishable — one bare name.
        assert_eq!(cancel.calls, vec!["save", "helper"]);
        let saves: Vec<Option<String>> = cancel
            .call_refs
            .iter()
            .filter(|c| c.name == "save")
            .map(|c| c.owner_type.clone())
            .collect();
        assert_eq!(saves.len(), 2);
        assert!(saves.contains(&Some("com.example.repo.OrderRepository".into())));
        assert!(saves.contains(&Some("com.example.audit.AuditLog".into())));

        // `this.helper()` and a bare `helper()` both land on the class.
        let helpers: Vec<Option<String>> = cancel
            .call_refs
            .iter()
            .filter(|c| c.name == "helper")
            .map(|c| c.owner_type.clone())
            .collect();
        assert_eq!(
            helpers,
            vec![
                Some("com.example.svc.OrderService".into()),
                Some("com.example.svc.OrderService".into())
            ]
        );
    }

    #[test]
    fn locals_static_calls_and_constructors_are_typed() {
        let (symbols, _) = parse(
            r#"
            package com.example.svc;
            import com.example.model.Order;
            import com.example.util.Ids;
            import static com.example.util.Assert.check;
            class Maker {
                Order build() {
                    Order o = new Order("x");
                    Ids.next();
                    check(o);
                    return o;
                }
            }
            "#,
        );
        let build = find(&symbols, "Maker.build");
        let owner = |name: &str| {
            build
                .call_refs
                .iter()
                .find(|c| c.name == name)
                .and_then(|c| c.owner_type.clone())
        };
        assert_eq!(owner("<init>"), Some("com.example.model.Order".into()));
        assert_eq!(owner("next"), Some("com.example.util.Ids".into()));
        assert_eq!(owner("check"), Some("com.example.util.Assert".into()));
    }

    #[test]
    fn a_same_package_type_resolves_without_an_import() {
        let (symbols, _) = parse(
            r#"
            package com.example.svc;
            class A {
                private Helper helper;
                void go() { helper.run(); }
            }
            class Helper { void run() {} }
            "#,
        );
        let go = find(&symbols, "A.go");
        assert_eq!(
            go.call_refs[0].owner_type.as_deref(),
            Some("com.example.svc.Helper")
        );
    }

    #[test]
    fn annotations_are_captured_with_their_arguments() {
        let (symbols, _) = parse(
            r#"
            package com.example.web;
            @RestController
            @RequestMapping("/api/orders")
            class OrderController {
                @GetMapping("/{id}")
                @Transactional
                Order find(String id) { return null; }
            }
            "#,
        );
        let class = find(&symbols, "OrderController");
        assert_eq!(class.annotations[0].name, "RestController");
        assert_eq!(class.annotations[1].name, "RequestMapping");
        assert_eq!(class.annotations[1].args.as_deref(), Some("\"/api/orders\""));

        let method = find(&symbols, "OrderController.find");
        let ann: Vec<&str> = method.annotations.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(ann, vec!["GetMapping", "Transactional"]);
    }

    #[test]
    fn a_route_composes_the_type_prefix_with_the_method_path() {
        let (symbols, _) = parse(
            r#"
            package com.example.web;
            @RequestMapping("/api/orders")
            class OrderController {
                @GetMapping("/{id}")
                Order find(String id) { return null; }

                @PostMapping
                Order create() { return null; }

                @RequestMapping(value = "/search", method = RequestMethod.POST)
                Order search() { return null; }
            }
            "#,
        );
        assert_eq!(
            find(&symbols, "OrderController.find").route.as_deref(),
            Some("GET /api/orders/{id}")
        );
        assert_eq!(
            find(&symbols, "OrderController.create").route.as_deref(),
            Some("POST /api/orders")
        );
        assert_eq!(
            find(&symbols, "OrderController.search").route.as_deref(),
            Some("POST /api/orders/search")
        );
    }

    #[test]
    fn a_method_without_a_mapping_has_no_route() {
        let (symbols, _) = parse(
            r#"
            package com.example.web;
            @RequestMapping("/api")
            class C { void helper() {} }
            "#,
        );
        assert_eq!(find(&symbols, "C.helper").route, None);
    }

    #[test]
    fn javadoc_survives_annotations_and_its_own_length() {
        let long_line = "x".repeat(300);
        let src = format!(
            r#"
            package com.example;
            class C {{
                /**
                 * Cancels an order and refunds the payment.
                 * {long_line}
                 * @param id the order id
                 * @return the cancelled order
                 * @throws IllegalStateException when already shipped
                 */
                @Override
                @Transactional
                Order cancel(String id) {{ return null; }}
            }}
            "#
        );
        let (symbols, _) = parse(&src);
        let doc = find(&symbols, "C.cancel").docstring.clone().unwrap();
        assert!(doc.starts_with("Cancels an order and refunds the payment."));
        assert!(doc.contains("param id: the order id"));
        assert!(doc.contains("returns: the cancelled order"));
        assert!(doc.contains("throws IllegalStateException: when already shipped"));
    }

    #[test]
    fn a_comment_belonging_to_something_else_is_not_borrowed() {
        let (symbols, _) = parse(
            r#"
            package com.example;
            class C {
                /** Belongs to first. */
                void first() {}
                void second() {}
            }
            "#,
        );
        assert!(find(&symbols, "C.first").docstring.is_some());
        assert_eq!(find(&symbols, "C.second").docstring, None);
    }

    #[test]
    fn imports_are_grouped_by_package_and_stable() {
        let (_, imports) = parse(
            r#"
            package com.example;
            import com.example.a.Foo;
            import com.example.a.Bar;
            import java.util.*;
            import static org.junit.Assert.assertTrue;
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
        assert_eq!(by_path["com.example.a"], vec!["Foo", "Bar"]);
        assert_eq!(by_path["java.util"], vec!["*"]);
        assert_eq!(by_path["org.junit.Assert"], vec!["assertTrue"]);
    }

    #[test]
    fn record_components_become_fields() {
        let (symbols, _) = parse(
            r#"
            package com.example;
            record Pair(String left, int right) {}
            "#,
        );
        assert_eq!(
            find(&symbols, "Pair.left").qualified_name.as_deref(),
            Some("com.example.Pair#left")
        );
        assert!(symbols.iter().any(|s| s.name == "Pair.right"));
    }

    #[test]
    fn an_interface_is_typed_as_an_interface_and_keeps_its_methods() {
        let (symbols, _) = parse(
            r#"
            package com.example;
            interface OrderApi extends Base {
                Order find(String id);
            }
            "#,
        );
        let api = find(&symbols, "OrderApi");
        assert_eq!(api.kind, "interface");
        assert_eq!(api.extends, vec!["com.example.Base"]);
        assert_eq!(
            find(&symbols, "OrderApi.find").qualified_name.as_deref(),
            Some("com.example.OrderApi#find")
        );
    }

    #[test]
    fn argument_counts_distinguish_overloads() {
        let (symbols, _) = parse(
            r#"
            package com.example;
            class C {
                void go() { send(); send("a"); send("a", 1); }
                void send() {}
                void send(String a) {}
                void send(String a, int b) {}
            }
            "#,
        );
        let counts: Vec<u32> = find(&symbols, "C.go")
            .call_refs
            .iter()
            .filter(|c| c.name == "send")
            .map(|c| c.argc)
            .collect();
        assert_eq!(counts, vec![0, 1, 2]);
    }

    #[test]
    fn a_file_with_no_package_still_produces_qualified_names() {
        let (symbols, _) = parse("class A { void go() {} }");
        assert_eq!(find(&symbols, "A").qualified_name.as_deref(), Some("A"));
        assert_eq!(
            find(&symbols, "A.go").qualified_name.as_deref(),
            Some("A#go")
        );
    }

    #[test]
    fn route_joining_normalises_slashes() {
        assert_eq!(join_route("/api/", Some("/x")), "/api/x");
        assert_eq!(join_route("api", Some("x")), "/api/x");
        assert_eq!(join_route("", Some("/x")), "/x");
        assert_eq!(join_route("/api", None), "/api");
        assert_eq!(join_route("", None), "/");
    }

    #[test]
    fn annotation_arguments_are_read_by_name_not_by_substring() {
        assert_eq!(
            named_or_first_string("value = \"/a\", defaultValue = \"/b\"", &["value"]),
            Some("/a".into())
        );
        // `value` must not match inside `defaultValue`.
        assert_eq!(named_string("defaultValue = \"/b\"", "value"), None);
        assert_eq!(
            named_or_first_string("\"/bare\"", &["path"]),
            Some("/bare".into())
        );
        assert_eq!(
            named_or_first_string("topics = {\"orders\", \"audit\"}", &["topics"]),
            Some("orders".into())
        );
    }

    #[test]
    fn base_type_name_strips_generics_and_arrays() {
        assert_eq!(base_type_name("Map<String, List<Order>>"), "Map");
        assert_eq!(base_type_name("Order[]"), "Order");
        assert_eq!(base_type_name("Order..."), "Order");
        assert_eq!(base_type_name("  Order  "), "Order");
    }
}

