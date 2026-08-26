//! Python indexer. Handles `.py`.

use crate::indexer::common::{
    annotation_args, calculate_nesting, extract_docstring, extract_params_from_signature,
    extract_return_type, first_string_arg, get_node_text, imports_in_stable_order};
use crate::indexer::languages::{FileContext, LanguageIndexer};
use crate::indexer::scope::{
    base_type_name, looks_like_constant, looks_like_type, module_path, ImportScope, TypeEnv,
    CTOR, MEMBER_SEP,
};
use crate::types::{
    Annotation, CallRef, ExportInfo, ImportInfo, ImportedItem, Param, Signature, Symbol,
    SymbolMetrics,
};
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

    fn extract_symbols(&self, source: &[u8], root: Node, ctx: &FileContext) -> Vec<Symbol> {
        let scope = ImportScope::new("python", module_path(ctx.path, "python"), ctx.imports);
        let walk = Ctx {
            fields: collect_class_fields(root, source),
            scope: &scope,
        };
        let mut symbols = Vec::new();
        visit(root, source, &walk, None, &mut symbols);
        symbols
    }
}

/// Everything the walk needs that the AST node itself doesn't carry.
struct Ctx<'a> {
    scope: &'a ImportScope,
    /// Written class name -> (attribute name -> written type), from
    /// `self.store: Store = …` annotations and annotated class-level
    /// attributes. Lets `self.store.save(..)` be typed.
    fields: HashMap<String, HashMap<String, String>>,
}

/// The class currently being walked.
struct OwnerCtx {
    /// Written name, e.g. `OrderService`.
    name: String,
    /// Qualified name, e.g. `pkg.svc.OrderService`.
    fqn: String,
}

/// Recursive AST walk.
///
/// `owner` is the class whose body we are inside. The walk used to carry
/// nothing, so a method became a symbol named just `save` with no link to its
/// class — which left `obj.save()` nothing to resolve against but every other
/// `save` in the repo.
fn visit(
    node: Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    symbols: &mut Vec<Symbol>,
) {
    extract_symbol_from_node(&node, source, ctx, owner, symbols);

    let inner = match node.kind() {
        "class_definition" => {
            get_node_text(node.child_by_field_name("name"), source).map(|name| OwnerCtx {
                fqn: ctx.scope.qualify(&name),
                name,
            })
        }
        _ => None,
    };
    let owner = inner.as_ref().or(owner);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, ctx, owner, symbols);
    }
}

/// Attribute name -> written type, per class in this file.
///
/// Python declares attribute types in two places, and both are common:
/// annotated class-level attributes (`store: Store`) and annotated
/// assignments to `self` inside `__init__` (`self.store: Store = store`).
fn collect_class_fields(root: Node, source: &[u8]) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    walk_class_fields(root, source, &mut out);
    out
}

fn walk_class_fields(
    node: Node,
    source: &[u8],
    out: &mut HashMap<String, HashMap<String, String>>,
) {
    if node.kind() == "class_definition" {
        if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
            let mut fields = HashMap::new();
            if let Some(body) = node.child_by_field_name("body") {
                collect_annotated_attrs(body, source, &mut fields);
            }
            out.insert(name, fields);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_class_fields(child, source, out);
    }
}

/// Gather `name: Type` and `self.name: Type` bindings anywhere under `node`.
fn collect_annotated_attrs(node: Node, source: &[u8], out: &mut HashMap<String, String>) {
    if node.kind() == "assignment" {
        if let (Some(left), Some(ty)) = (
            node.child_by_field_name("left"),
            get_node_text(node.child_by_field_name("type"), source),
        ) {
            if let Some(text) = get_node_text(Some(left), source) {
                let attr = text.strip_prefix("self.").unwrap_or(&text).trim();
                if !attr.is_empty() && !attr.contains('.') {
                    out.insert(attr.to_string(), base_type_name(&ty).to_string());
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_annotated_attrs(child, source, out);
    }
}


/// `from foo.bar import (a, b)` / `from foo import a, b as c`. Compiled once —
/// this runs per file. See the call site for why the name lists are written
/// the way they are.
fn from_import_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?m)^[ \t]*from\s+(\.[^ ]+|[a-zA-Z_][a-zA-Z0-9_.]*)\s+import\s+(?:\(([^)]+)\)|([a-zA-Z_][a-zA-Z0-9_, \t]*))"#,
        )
        .expect("from-import pattern is a literal")
    })
}

/// `import foo` / `import foo.bar`, anchored so it cannot re-match the tail
/// of a `from … import …` line.
fn plain_import_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(?m)^[ \t]*import\s+([a-zA-Z_][a-zA-Z0-9_.]*)"#)
            .expect("import pattern is a literal")
    })
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
    //
    // Anchored to the start of a line, and the unparenthesised name list
    // deliberately excludes newlines: `[\s]` there swallowed everything after
    // the import, so `from pkg.store import Store` two blank lines above a
    // `def run` recorded an imported name of `"Store\n\n\ndef run"`. That was
    // invisible while imports only drew file-to-file edges; it is not
    // invisible now that they decide which `save` a call means.
    {
        for cap in from_import_regex().captures_iter(source_str) {
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

    // `import foo` / `import foo.bar`, anchored to the start of a line so it
    // cannot re-match the tail of a `from foo import Bar` line the previous
    // regex already handled. It did exactly that, recording a second import
    // whose *path* was the imported name — so `from pkg.store import Store`
    // also produced `path: "Store"`, and the alias table then resolved
    // `Store` to `Store.Store`. The old `!path.contains("from")` guard could
    // never have caught it: it inspects the captured path, not the line.
    {
        for cap in plain_import_regex().captures_iter(source_str) {
            let path = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !path.is_empty() {
                import_lookup.entry(path.clone()).or_insert_with(|| ImportInfo {
                    path: path.clone(),
                    imported: vec![ImportedItem {
                        name: path.split('.').next_back().unwrap_or(&path).to_string(),
                        alias: None,
                    }],
                });
            }
        }
    }

    imports_in_stable_order(import_lookup)
}

/// Decorators applied to a `def` or `class`, in source order.
///
/// Python hangs them off a `decorated_definition` wrapper rather than on the
/// definition itself, so this walks up one level. The name keeps its dotted
/// receiver — `app.route`, not `route` — because in Python the receiver *is*
/// the framework: `@app.route` and `@celery.task` share a last segment with
/// plenty of ordinary methods, and dropping it would make a decorator
/// indistinguishable from any function of the same name. Boundary rules
/// match these with a `*.route`-style pattern, which is also what makes them
/// work when the app object is called `bp` or `api`.
fn extract_decorators(node: &Node, source: &[u8]) -> Vec<Annotation> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() != "decorator" {
            continue;
        }
        // The expression after the `@`: an identifier (`@staticmethod`), an
        // attribute (`@app.route`), or a call of either.
        let Some(expr) = child.named_child(0) else {
            continue;
        };
        let (name_node, args) = if expr.kind() == "call" {
            (
                expr.child_by_field_name("function"),
                annotation_args(expr.child_by_field_name("arguments"), source),
            )
        } else {
            (Some(expr), None)
        };
        if let Some(name) = get_node_text(name_node, source) {
            out.push(Annotation {
                name: name.trim().to_string(),
                args,
            });
        }
    }
    out
}

fn extract_symbol_from_node(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;

    match kind {
        "function_definition" => {
            let Some(raw_name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // Members display as `Type.member`, matching Java and
            // TypeScript — and matching how Python itself spells the
            // reference.
            let name = match owner {
                Some(o) => format!("{}.{}", o.name, raw_name),
                None => raw_name.clone(),
            };
            let qualified_name = Some(match owner {
                Some(o) => format!("{}{}{}", o.fqn, MEMBER_SEP, raw_name),
                None => ctx.scope.qualify(&raw_name),
            });

            let params = extract_params(node, source);
            let return_type = extract_return_type(node, source);
            let (calls, call_refs, uses, value_refs) =
                extract_calls(node, source, ctx, owner, &params);
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
                call_refs,
                uses,
                value_refs,
                qualified_name,
                owner: owner.map(|o| o.fqn.clone()),
                metrics: Some(metrics),
                annotations: extract_decorators(node, source),
                ..Default::default()
            });
        }
        "class_definition" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // Base classes are qualified through this file's imports, since
            // the graph builder's supertype walk keys on qualified names.
            let extends = extract_extends(node, source)
                .iter()
                .filter_map(|b| ctx.scope.resolve_type_ref(b))
                .collect();
            out.push(Symbol {
                id: format!("class:{}:{}", start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
                name,
                kind: "class".to_string(),
                file: String::new(),
                start_line: start,
                end_line: end,
                docstring: extract_docstring(node, source),
                signature: None,
                imports: Vec::new(),
                exports: Vec::new(),
                extends,
                implements: Vec::new(),
                calls: Vec::new(),
                metrics: None,
                annotations: extract_decorators(node, source),
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
                qualified_name: Some(ctx.scope.qualify(&name)),
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

// ---------------------------------------------------------------------------
// Call sites
// ---------------------------------------------------------------------------

/// Call sites inside one function body, with whatever receiver type this file
/// can supply. See the Rust indexer's equivalent for why the display list now
/// holds bare names rather than callee source text.
fn extract_calls(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    params: &[Param],
) -> (Vec<String>, Vec<CallRef>, Vec<String>, Vec<String>) {
    let mut env = TypeEnv::new();

    if let Some(o) = owner {
        env.insert("self", o.fqn.clone());
        if let Some(fields) = ctx.fields.get(&o.name) {
            for (fname, ftype) in fields {
                if let Some(fqn) = ctx.scope.lookup(ftype) {
                    env.insert(format!("self.{}", fname), fqn);
                }
            }
        }
    }
    for p in params {
        if let Some(t) = &p.param_type {
            if let Some(fqn) = ctx.scope.lookup(base_type_name(t)) {
                env.insert(p.name.clone(), fqn);
            }
        }
    }

    let mut calls = Vec::new();
    let mut refs = Vec::new();
    let mut uses = Vec::new();
    let mut value_refs = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(
            &body,
            source,
            ctx,
            owner,
            &mut env,
            &mut calls,
            &mut refs,
            &mut uses,
            &mut value_refs,
        );
    }
    (calls, refs, uses, value_refs)
}

#[allow(clippy::too_many_arguments)]
fn collect_calls(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    env: &mut TypeEnv,
    calls: &mut Vec<String>,
    refs: &mut Vec<CallRef>,
    uses: &mut Vec<String>,
    value_refs: &mut Vec<String>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        record_constant_use(&child, source, ctx, uses);
        match child.kind() {
            "assignment" => record_local(&child, source, ctx, env),
            "call" => {
                if let Some(r) = call_ref_for(&child, source, ctx, owner, env) {
                    push_call(calls, &r.name);
                    refs.push(r);
                }
                record_value_refs(&child, source, ctx, env, value_refs);
            }
            _ => {}
        }
        collect_calls(
            &child, source, ctx, owner, env, calls, refs, uses, value_refs,
        );
    }
}

/// Note a module-level symbol passed *as a value* — a bare identifier in
/// argument position (`event_bus.on("ready", handler)`,
/// `Thread(target=run)`). See the TypeScript indexer's equivalent.
fn record_value_refs(
    call: &Node,
    source: &[u8],
    ctx: &Ctx,
    env: &TypeEnv,
    value_refs: &mut Vec<String>,
) {
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    for arg in args.children(&mut cursor) {
        if arg.kind() != "identifier" {
            continue;
        }
        let Some(text) = get_node_text(Some(arg), source) else {
            continue;
        };
        if env.contains_key(&text) {
            continue;
        }
        if looks_like_constant(&text) {
            continue;
        }
        if let Some(fqn) = ctx.scope.lookup(&text) {
            if !value_refs.contains(&fqn) {
                value_refs.push(fqn);
            }
        }
    }
}

/// Note a reference to a module-level constant, resolved through this file's
/// imports. See `scope::looks_like_constant` for why the naming convention is
/// the filter.
fn record_constant_use(node: &Node, source: &[u8], ctx: &Ctx, uses: &mut Vec<String>) {
    if node.kind() != "identifier" {
        return;
    }
    let Some(text) = get_node_text(Some(*node), source) else {
        return;
    };
    if !looks_like_constant(&text) {
        return;
    }
    if let Some(fqn) = ctx.scope.lookup(&text) {
        if !uses.contains(&fqn) {
            uses.push(fqn);
        }
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

/// One `call`, with whatever the file can tell us about where it lands.
///
/// Python spells construction exactly like invocation — `Foo()` is both — so
/// the capitalisation convention is what separates them, the same rule used
/// to tell a type from a module elsewhere.
fn call_ref_for(
    call: &Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    env: &TypeEnv,
) -> Option<CallRef> {
    let func = call.child_by_field_name("function")?;
    let argc = argument_count(call);
    let arg0 = first_string_arg(call, "arguments", source);

    match func.kind() {
        "identifier" => {
            let name = get_node_text(Some(func), source)?;
            if looks_like_type(&name) {
                return Some(CallRef {
                    name: CTOR.to_string(),
                    owner_type: ctx.scope.lookup(&name),
                    argc,
                    first_string_arg: arg0,
                    qualified: None,
                    is_ctor: true,
                    has_receiver: true,
                });
            }
            Some(CallRef {
                qualified: ctx.scope.lookup(&name),
                name,
                owner_type: None,
                argc,
                first_string_arg: arg0,
                is_ctor: false,
                has_receiver: false,
            })
        }
        // `receiver.method(..)`
        "attribute" => {
            let name = get_node_text(func.child_by_field_name("attribute"), source)?;
            let recv = func.child_by_field_name("object")?;
            Some(CallRef {
                owner_type: type_of_expr(&recv, source, ctx, owner, env),
                name,
                argc,
                first_string_arg: arg0,
                qualified: None,
                is_ctor: false,
                has_receiver: true,
            })
        }
        _ => None,
    }
}

/// The qualified type an expression evaluates to, or `None` when typing it
/// would mean following an arbitrary expression.
fn type_of_expr(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    env: &TypeEnv,
) -> Option<String> {
    match node.kind() {
        "identifier" => {
            let text = get_node_text(Some(*node), source)?;
            if text == "self" {
                return owner.map(|o| o.fqn.clone());
            }
            if let Some(t) = env.get(&text) {
                return Some(t.to_string());
            }
            // A capitalised bare identifier in receiver position is a class,
            // i.e. a call on the class rather than an instance.
            if looks_like_type(&text) {
                return ctx.scope.lookup(&text);
            }
            None
        }
        // Only `self.attr`. Anything deeper is an expression we would be
        // inventing a type for.
        "attribute" => {
            let object = node.child_by_field_name("object")?;
            if get_node_text(Some(object), source).as_deref() != Some("self") {
                return None;
            }
            let attr = get_node_text(node.child_by_field_name("attribute"), source)?;
            env.get(&format!("self.{}", attr)).map(str::to_string)
        }
        _ => None,
    }
}

/// Record `x: Foo = …` and `x = Foo(..)`.
fn record_local(assign: &Node, source: &[u8], ctx: &Ctx, env: &mut TypeEnv) {
    let Some(left) = assign.child_by_field_name("left") else {
        return;
    };
    if left.kind() != "identifier" {
        return;
    }
    let Some(name) = get_node_text(Some(left), source) else {
        return;
    };

    // An annotation is authoritative.
    if let Some(ty) = get_node_text(assign.child_by_field_name("type"), source) {
        if let Some(fqn) = ctx.scope.lookup(base_type_name(&ty)) {
            env.insert(name, fqn);
            return;
        }
    }

    let Some(value) = assign.child_by_field_name("right") else {
        return;
    };
    if value.kind() != "call" {
        return;
    }
    let Some(func) = value.child_by_field_name("function") else {
        return;
    };
    let Some(text) = get_node_text(Some(func), source) else {
        return;
    };
    let bare = text.rsplit('.').next().unwrap_or(&text);
    if !looks_like_type(bare) {
        return;
    }
    if let Some(fqn) = ctx.scope.lookup(bare) {
        env.insert(name, fqn);
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
        let imports = PythonIndexer.extract_imports(src.as_bytes(), root);
        let ctx = FileContext {
            path: "pkg/sample.py",
            imports: &imports,
        };
        (
            PythonIndexer.extract_symbols(src.as_bytes(), root, &ctx),
            imports,
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
        // A method displays as `Type.member`, so its name says which class it
        // is on and splits into more than one search term.
        assert_eq!(find(&symbols, "Client.send").kind, "function");
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
        // Base names are qualified through the file's imports, because the
        // graph builder's supertype walk is keyed on qualified names. With no
        // import in scope they resolve against this module.
        let (symbols, _) = parse("class Sub(Base, Mixin):\n    pass\n");
        assert_eq!(
            find(&symbols, "Sub").extends,
            vec!["pkg.sample.Base", "pkg.sample.Mixin"]
        );
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
        assert_eq!(find(&symbols, "A").extends, vec!["pkg.sample.Generic"]);
        // A metaclass configures the class; it is not a supertype.
        assert_eq!(find(&symbols, "B").extends, vec!["pkg.sample.Base"]);
        // A base that is already an absolute module path stays as written,
        // rather than being re-rooted under the current module.
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

    /// Python now takes the same precise-resolution path Java does. This
    /// test used to assert the exact opposite — that qualified names, owners
    /// and typed call sites all stayed empty — which is a fair summary of why
    /// `obj.save()` had nothing to resolve against but every other `save` in
    /// the repo.
    #[test]
    fn python_symbols_carry_qualified_names_and_owners() {
        let (symbols, _) = parse("class C:\n    def m(self):\n        pass\n");

        let class = find(&symbols, "C");
        assert_eq!(class.qualified_name.as_deref(), Some("pkg.sample.C"));
        assert!(class.owner.is_none(), "a class is not a member");

        let method = find(&symbols, "C.m");
        assert_eq!(method.qualified_name.as_deref(), Some("pkg.sample.C#m"));
        assert_eq!(method.owner.as_deref(), Some("pkg.sample.C"));

        // Undecorated code carries no annotations. Route *composition*
        // stays Java-only — Python has no type-level path prefix to join
        // against, so a decorator's path is reported as the decorator's
        // argument rather than as a composed `Symbol::route`.
        for s in &symbols {
            assert!(s.annotations.is_empty(), "{}", s.name);
            assert!(s.route.is_none(), "{}", s.name);
        }
    }

    #[test]
    fn decorators_are_captured_with_their_receiver_and_arguments() {
        let (symbols, _) = parse(
            "@app.route(\"/users\", methods=[\"GET\"])\ndef list_users():\n    pass\n",
        );

        let f = find(&symbols, "list_users");
        assert_eq!(f.annotations.len(), 1);
        // Dotted, not bare: `@app.route` and a method called `route` are
        // different things, and the receiver is what says which.
        assert_eq!(f.annotations[0].name, "app.route");
        assert_eq!(
            f.annotations[0].args.as_deref(),
            Some("\"/users\", methods=[\"GET\"]")
        );
    }

    #[test]
    fn a_marker_decorator_has_no_arguments() {
        let (symbols, _) = parse("class C:\n    @staticmethod\n    def m():\n        pass\n");

        let m = find(&symbols, "C.m");
        assert_eq!(m.annotations.len(), 1);
        assert_eq!(m.annotations[0].name, "staticmethod");
        assert!(m.annotations[0].args.is_none());
    }

    #[test]
    fn decorators_on_a_class_are_captured_too() {
        let (symbols, _) = parse("@dataclass\nclass Point:\n    x: int\n");

        let c = find(&symbols, "Point");
        assert_eq!(c.annotations.len(), 1);
        assert_eq!(c.annotations[0].name, "dataclass");
    }

    #[test]
    fn stacked_decorators_are_kept_in_source_order() {
        let (symbols, _) = parse(
            "@app.route(\"/x\")\n@login_required\ndef view():\n    pass\n",
        );

        let names: Vec<&str> = find(&symbols, "view")
            .annotations
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, vec!["app.route", "login_required"]);
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

