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
    annotation_args, calculate_nesting, extract_return_type, first_string_arg, get_node_text,
};
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

    fn extract_symbols(&self, source: &[u8], root: Node, ctx: &FileContext) -> Vec<Symbol> {
        let mut scope = ImportScope::new("rust", module_path(ctx.path, "rust"), ctx.imports);
        // `mod cli;` is not an import, so it is deliberately absent from
        // `ImportInfo` — but it is still a name binding, and the only one
        // that makes `cli::run()` in this file mean `crate::cli::run`.
        for name in declared_child_modules(root, source) {
            scope.declare_child_module(&name);
        }
        let walk = Ctx {
            fields: collect_struct_fields(root, source),
            scope: &scope,
        };

        let mut symbols = Vec::new();
        let mut impl_traits: Vec<(String, String)> = Vec::new();
        visit(root, source, &walk, None, false, &mut impl_traits, &mut symbols);
        attach_impl_traits(&mut symbols, &impl_traits);
        symbols
    }
}

/// Everything the walk needs that the AST node itself doesn't carry.
struct Ctx<'a> {
    scope: &'a ImportScope,
    /// Written type name -> (field name -> written field type). Lets
    /// `self.store.upsert(..)` be typed without chasing the field back to its
    /// declaration at every call site.
    fields: HashMap<String, HashMap<String, String>>,
}

/// The `impl` block currently being walked.
struct ImplCtx {
    /// Written type name, e.g. `Db`.
    name: String,
    /// Qualified type name, e.g. `crate::storage::db::Db`.
    fqn: String,
}

/// AST walk. `imp` is `Some` when we're inside the body of an `impl …` block —
/// methods extracted there are renamed to `Type::method` so the graph can
/// disambiguate them from free functions or methods on other types, and they
/// carry `owner` so a call through a typed receiver can find them.
///
/// `test_ctx` is `true` while inside a `#[cfg(test)]` module. The marker is
/// propagated to every symbol extracted in that subtree (see
/// [`test_annotation`]), so the `is_test` fact reaches code that lives in a
/// `mod tests` block inside an otherwise production file.
///
/// `impl_traits` collects `(type fqn, trait fqn)` pairs for
/// `impl Trait for Type`. Those land on the *type* rather than on each
/// method — see [`attach_impl_traits`].
fn visit(
    node: Node,
    source: &[u8],
    ctx: &Ctx,
    imp: Option<&ImplCtx>,
    test_ctx: bool,
    impl_traits: &mut Vec<(String, String)>,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();

    match kind {
        "impl_item" => {
            // Resolve the type this impl is for, and (optionally) the
            // trait being implemented. Both become qualifiers on the
            // contained methods rather than top-level symbols.
            let type_name = get_node_text(node.child_by_field_name("type"), source)
                .map(|t| base_type_name(&t).to_string())
                .unwrap_or_default();
            if type_name.is_empty() {
                return;
            }
            let inner = ImplCtx {
                fqn: ctx.scope.qualify(&type_name),
                name: type_name,
            };

            if let Some(trait_name) = get_node_text(node.child_by_field_name("trait"), source) {
                if let Some(trait_fqn) = ctx.scope.resolve_type_ref(&trait_name) {
                    impl_traits.push((inner.fqn.clone(), trait_fqn));
                }
            }

            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    visit(child, source, ctx, Some(&inner), test_ctx, impl_traits, out);
                }
            }
            return;
        }
        "mod_item" => {
            // Step into module bodies so nested items still appear in
            // the symbol list. The `mod` itself isn't a symbol —
            // matches what other indexers (e.g. Python packages) do.
            // A `#[cfg(test)]` module marks everything inside it as
            // test code, whatever the file's own classification is.
            let nested = test_ctx || is_test_module(&node, source);
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    visit(child, source, ctx, imp, nested, impl_traits, out);
                }
            }
            return;
        }
        _ => {}
    }

    extract_symbol_from_node(&node, source, ctx, imp, test_ctx, out);

    // A trait's members are symbols in their own right. Without them the
    // trait is an empty node: `Overrides` has no declaration to point at, and
    // "who implements this method" — the question an interface exists to
    // pose — has no answer. Both required and defaulted members are taken,
    // owned by the trait exactly as an inherent method is owned by its type.
    if kind == "trait_item" {
        let trait_name = get_node_text(node.child_by_field_name("name"), source);
        if let (Some(name), Some(body)) = (trait_name, node.child_by_field_name("body")) {
            let owner = ImplCtx {
                fqn: ctx.scope.qualify(&name),
                name,
            };
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                extract_symbol_from_node(&child, source, ctx, Some(&owner), test_ctx, out);
            }
        }
        return;
    }

    // Descend into other nodes so e.g. functions inside a top-level
    // `mod foo { … }` block are found. Stop recursing into bodies that
    // would just re-emit nested locals — function bodies are intentionally
    // left alone because Rust closures/locals are not symbols we surface.
    if matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
    ) {
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, ctx, imp, test_ctx, impl_traits, out);
    }
}

/// Whether a `mod_item` node is gated behind `#[cfg(test)]`.
///
/// Rust's grammar puts the attribute on the module's preceding sibling, so
/// this reuses the same backward scan [`extract_attributes`] performs, but
/// reads the raw text rather than building `Annotation`s.
fn is_test_module(node: &Node, source: &[u8]) -> bool {
    extract_attributes(node, source)
        .iter()
        .any(|a| a.name == "cfg" && a.args.as_deref().is_some_and(|s| s.contains("test")))
}

fn extract_symbol_from_node(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    imp: Option<&ImplCtx>,
    test_ctx: bool,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;
    let docstring = extract_rust_docstring(node, source);
    let mut annotations = extract_attributes(node, source);
    if test_ctx {
        test_annotation(&mut annotations);
    }

    match kind {
        // `function_signature_item` is a trait member with no body
        // (`fn put(&self) -> u32;`). It carries everything that matters here
        // — name, signature, docs — and only `extract_calls` finds nothing,
        // which is correct: a declaration calls nothing.
        "function_item" | "function_signature_item" => {
            let Some(raw_name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // Methods inside an `impl Foo` block get qualified — the
            // graph layer keys IDs on `<file>:<line>:<name>` so this
            // also keeps `Foo::new` distinct from `Bar::new`.
            //
            // The *display* name keeps the `Type::method` spelling it has
            // always had, which is why Rust node ids survive this change
            // unaltered. What is new is `qualified_name` / `owner`, which
            // carry the module path the display name never could.
            let name = match imp {
                Some(i) => format!("{}::{}", i.name, raw_name),
                None => raw_name.clone(),
            };
            let qualified_name = Some(match imp {
                Some(i) => format!("{}{}{}", i.fqn, MEMBER_SEP, raw_name),
                None => ctx.scope.qualify(&raw_name),
            });

            let params = extract_params(node, source);
            let return_type = extract_return_type(node, source);
            let (calls, call_refs, uses, value_refs) =
                extract_calls(node, source, ctx, imp, &params);
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
                annotations: annotations.clone(),
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
                owner: imp.map(|i| i.fqn.clone()),
                metrics: Some(metrics),
                ..Default::default()
            });
        }
        "struct_item" | "enum_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            let item_kind = if kind == "struct_item" { "struct" } else { "enum" };
            out.push(Symbol {
                annotations: annotations.clone(),
                id: format!("{}:{}:{}", item_kind, start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
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
                ..Default::default()
            });
        }
        "trait_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // `trait Foo: Bar + Baz { … }` — super-traits land in
            // `extends` so the graph captures the trait hierarchy. They are
            // qualified here rather than left as written names because the
            // graph builder keys its supertype walk on qualified names.
            let extends = extract_trait_bounds(node, source)
                .iter()
                .filter_map(|b| ctx.scope.resolve_type_ref(b))
                .collect();
            out.push(Symbol {
                annotations: annotations.clone(),
                id: format!("trait:{}:{}", start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
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
                ..Default::default()
            });
        }
        "type_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                annotations: annotations.clone(),
                id: format!("type:{}:{}", start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
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
                ..Default::default()
            });
        }
        "const_item" | "static_item" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                annotations: annotations.clone(),
                id: format!("const:{}:{}", start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
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
                ..Default::default()
            });
        }
        "macro_definition" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            out.push(Symbol {
                annotations: annotations.clone(),
                id: format!("macro:{}:{}", start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
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
                ..Default::default()
            });
        }
        _ => {}
    }
}

/// Move each `impl Trait for Type` onto the *type* it names.
///
/// The trait used to be recorded on every method in the block, which put a
/// type-level fact on a member: it made `Implements` edges point from a
/// function to a trait, and left the type — the thing that actually
/// implements the trait — with no hierarchy at all. Recording it here instead
/// gives the graph builder both edges for free: `Implements` from the type,
/// and `Overrides` from each method that redeclares a trait member, because
/// its owner's supertypes now include the trait.
fn attach_impl_traits(symbols: &mut [Symbol], impl_traits: &[(String, String)]) {
    if impl_traits.is_empty() {
        return;
    }
    for sym in symbols.iter_mut() {
        // Members carry `owner`; only the type declaration itself should
        // collect the traits.
        if sym.owner.is_some() {
            continue;
        }
        let Some(fqn) = sym.qualified_name.as_ref() else {
            continue;
        };
        for (type_fqn, trait_fqn) in impl_traits {
            if type_fqn == fqn && !sym.implements.contains(trait_fqn) {
                sym.implements.push(trait_fqn.clone());
            }
        }
    }
}

/// Field name -> written field type, per struct in this file.
///
/// Collected in one pre-pass because a method body typing `self.store` needs
/// the declaration of `store`, which sits in a `struct_item` the walk may not
/// have reached yet.
fn collect_struct_fields(
    root: Node,
    source: &[u8],
) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    walk_struct_fields(root, source, &mut out);
    out
}

fn walk_struct_fields(
    node: Node,
    source: &[u8],
    out: &mut HashMap<String, HashMap<String, String>>,
) {
    if node.kind() == "struct_item" {
        if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
            let mut fields = HashMap::new();
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    if child.kind() != "field_declaration" {
                        continue;
                    }
                    let (Some(fname), Some(ftype)) = (
                        get_node_text(child.child_by_field_name("name"), source),
                        get_node_text(child.child_by_field_name("type"), source),
                    ) else {
                        continue;
                    };
                    fields.insert(fname, base_type_name(&ftype).to_string());
                }
            }
            out.insert(name, fields);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_struct_fields(child, source, out);
    }
}

// ---------------------------------------------------------------------------
// Call sites
// ---------------------------------------------------------------------------

/// Call sites inside one function body, resolved as far as one file's worth
/// of context allows.
///
/// Returns both the deduped display list (`Symbol::calls`) and the structured
/// sites (`Symbol::call_refs`). The display list now holds *bare callee
/// names*. It used to hold the raw source text of the callee expression, so
/// `a.iter().map(|x| { … }).collect()` contributed four entries, one of them
/// the entire closure body — and the graph builder then tried to resolve
/// those blobs by taking the substring after their last dot, which is how
/// `.collect()` came to be recorded as a call to whatever local function
/// happened to be named `collect`.
fn extract_calls(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    imp: Option<&ImplCtx>,
    params: &[Param],
) -> (Vec<String>, Vec<CallRef>, Vec<String>, Vec<String>) {
    let mut env = TypeEnv::new();

    if let Some(i) = imp {
        env.insert("self", i.fqn.clone());
        // Fields of the type this method hangs off, so `self.store.get(..)`
        // knows what it dispatches on.
        if let Some(fields) = ctx.fields.get(&i.name) {
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
            &body, source, ctx, imp, &mut env, &mut calls, &mut refs, &mut uses, &mut value_refs,
        );
    }
    (calls, refs, uses, value_refs)
}

#[allow(clippy::too_many_arguments)]
fn collect_calls(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    imp: Option<&ImplCtx>,
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
            // Locals are recorded as they are met, so a binding halfway down
            // a body types the calls below it. No block scoping — see
            // `TypeEnv`.
            "let_declaration" => record_local(&child, source, ctx, env),
            "call_expression" => {
                if let Some(r) = call_ref_for(&child, source, ctx, imp, env) {
                    push_call(calls, &r.name);
                    refs.push(r);
                }
                record_value_refs(&child, source, ctx, env, value_refs);
            }
            // `Foo { .. }` is the one construction form Rust spells
            // unambiguously. `Foo::new(..)` is a convention, not a language
            // feature, and is left as an ordinary call — which points at the
            // real `Foo::new` body and is the more useful edge anyway.
            "struct_expression" => {
                if let Some(ty) = get_node_text(child.child_by_field_name("name"), source) {
                    let bare = base_type_name(&ty);
                    push_call(calls, bare);
                    refs.push(CallRef {
                        name: CTOR.to_string(),
                        owner_type: ctx.scope.lookup(bare),
                        argc: 0,
                        // A struct literal has fields, not positional args.
                        first_string_arg: None,
                        qualified: None,
                        is_ctor: true,
                        // `<init>` is a sentinel, not a name anything is
                        // declared under, so the bare-name fallback has
                        // nothing to match and must not be tried.
                        has_receiver: true,
                    });
                }
            }
            "macro_invocation" => {
                if let Some(name) = get_node_text(child.child_by_field_name("macro"), source) {
                    let bare = name.rsplit("::").next().unwrap_or(&name).to_string();
                    push_call(calls, &bare);
                    refs.push(CallRef {
                        qualified: ctx.scope.resolve_path(&name),
                        name: bare,
                        owner_type: None,
                        argc: 0,
                        // A macro's tokens are not an argument list; the
                        // grammar gives them no `arguments` field to read.
                        first_string_arg: None,
                        is_ctor: false,
                        has_receiver: false,
                    });
                }
            }
            _ => {}
        }
        collect_calls(
            &child, source, ctx, imp, env, calls, refs, uses, value_refs,
        );
    }
}

/// Note a module-level symbol passed *as a value* — a bare identifier or
/// path in argument position (`post(api_chat)`, `.route("/x", get(h))`).
/// See the TypeScript indexer's equivalent for why this exists.
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
        let text = match arg.kind() {
            "identifier" | "scoped_identifier" => match get_node_text(Some(arg), source) {
                Some(t) => t,
                None => continue,
            },
            _ => continue,
        };
        let tail = text.rsplit("::").next().unwrap_or(&text);
        if env.contains_key(tail) {
            continue;
        }
        if looks_like_constant(tail) {
            continue;
        }
        if let Some(fqn) = ctx.scope.resolve_path(&text) {
            if !value_refs.contains(&fqn) {
                value_refs.push(fqn);
            }
        }
    }
}

/// Note a reference to a module-level constant, resolved through this file's
/// imports. Covers both `MAX_FOO` and `limits::MAX_FOO`.
fn record_constant_use(node: &Node, source: &[u8], ctx: &Ctx, uses: &mut Vec<String>) {
    let text = match node.kind() {
        "identifier" | "scoped_identifier" => match get_node_text(Some(*node), source) {
            Some(t) => t,
            None => return,
        },
        _ => return,
    };
    let tail = text.rsplit("::").next().unwrap_or(&text);
    if !looks_like_constant(tail) {
        return;
    }
    if let Some(fqn) = ctx.scope.resolve_path(&text) {
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

/// One `call_expression`, with whatever the file can tell us about where it
/// lands. `None` for a callee shape we can't name (a call through a closure
/// binding, an index expression).
fn call_ref_for(
    call: &Node,
    source: &[u8],
    ctx: &Ctx,
    imp: Option<&ImplCtx>,
    env: &TypeEnv,
) -> Option<CallRef> {
    let mut func = call.child_by_field_name("function")?;
    // `foo::<T>(..)` wraps the callee in a `generic_function`.
    if func.kind() == "generic_function" {
        func = func.child_by_field_name("function")?;
    }
    let argc = argument_count(call);
    let arg0 = first_string_arg(call, "arguments", source);

    match func.kind() {
        "identifier" => {
            let name = get_node_text(Some(func), source)?;
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
        "field_expression" => {
            let name = get_node_text(func.child_by_field_name("field"), source)?;
            let recv = func.child_by_field_name("value")?;
            Some(CallRef {
                owner_type: type_of_expr(&recv, source, ctx, imp, env),
                name,
                argc,
                first_string_arg: arg0,
                qualified: None,
                is_ctor: false,
                has_receiver: true,
            })
        }
        // `Type::assoc(..)` dispatches on a type; `module::free_fn(..)` names
        // its target outright. Rust spells types UpperCamelCase, which is
        // what tells the two apart — the same call Java's indexer makes for
        // a bare identifier in receiver position.
        "scoped_identifier" => {
            let text = get_node_text(Some(func), source)?;
            let (prefix, name) = text.rsplit_once("::")?;
            let head = prefix.rsplit("::").next().unwrap_or(prefix);
            if looks_like_type(head) {
                Some(CallRef {
                    owner_type: ctx.scope.resolve_path(prefix),
                    name: name.to_string(),
                    argc,
                    first_string_arg: arg0,
                    qualified: None,
                    is_ctor: false,
                    // The type qualifier is the whole identity of the
                    // callee. When it names something outside the repo
                    // (`String::new`), falling back to the bare `new` would
                    // reach for any local function of that name.
                    has_receiver: true,
                })
            } else {
                Some(CallRef {
                    qualified: ctx.scope.resolve_path(&text),
                    name: name.to_string(),
                    owner_type: None,
                    argc,
                    first_string_arg: arg0,
                    is_ctor: false,
                    has_receiver: false,
                })
            }
        }
        _ => None,
    }
}

/// The qualified type an expression evaluates to, or `None` when typing it
/// would mean following an arbitrary expression — which is the point at which
/// a receiver type stops being a fact and starts being a guess.
fn type_of_expr(
    node: &Node,
    source: &[u8],
    ctx: &Ctx,
    imp: Option<&ImplCtx>,
    env: &TypeEnv,
) -> Option<String> {
    match node.kind() {
        "self" => imp.map(|i| i.fqn.clone()),
        "identifier" => {
            let text = get_node_text(Some(*node), source)?;
            if let Some(t) = env.get(&text) {
                return Some(t.to_string());
            }
            if looks_like_type(&text) {
                return ctx.scope.lookup(&text);
            }
            None
        }
        // Only `self.field`. Anything deeper is an expression we'd be
        // inventing a type for.
        "field_expression" => {
            let value = node.child_by_field_name("value")?;
            if value.kind() != "self" {
                return None;
            }
            let field = get_node_text(node.child_by_field_name("field"), source)?;
            env.get(&format!("self.{}", field)).map(str::to_string)
        }
        "scoped_identifier" => ctx.scope.resolve_path(&get_node_text(Some(*node), source)?),
        _ => None,
    }
}

/// Record `let x: Foo = …` / `let x = Foo { .. }` / `let x = Foo::new(..)`.
fn record_local(decl: &Node, source: &[u8], ctx: &Ctx, env: &mut TypeEnv) {
    let Some(pattern) = decl.child_by_field_name("pattern") else {
        return;
    };
    // Destructuring binds several names to parts of a value; typing any of
    // them would take inference we don't have.
    if pattern.kind() != "identifier" {
        return;
    }
    let Some(name) = get_node_text(Some(pattern), source) else {
        return;
    };

    // An annotation is authoritative.
    if let Some(ty) = get_node_text(decl.child_by_field_name("type"), source) {
        if let Some(fqn) = ctx.scope.lookup(base_type_name(&ty)) {
            env.insert(name, fqn);
            return;
        }
    }

    let Some(value) = decl.child_by_field_name("value") else {
        return;
    };
    if let Some(fqn) = constructed_type(&value, source, ctx) {
        env.insert(name, fqn);
    }
}

/// The type a construction expression produces.
///
/// `Foo::new(..)` is read as producing a `Foo`. That is a convention rather
/// than a guarantee — the real return may be `Result<Foo>` — but the failure
/// is self-limiting: a wrongly typed receiver looks for a member the type
/// doesn't have, finds nothing, and the call site is dropped.
fn constructed_type(value: &Node, source: &[u8], ctx: &Ctx) -> Option<String> {
    match value.kind() {
        "struct_expression" => {
            let ty = get_node_text(value.child_by_field_name("name"), source)?;
            ctx.scope.lookup(base_type_name(&ty))
        }
        "call_expression" => {
            let func = value.child_by_field_name("function")?;
            if func.kind() != "scoped_identifier" {
                return None;
            }
            let text = get_node_text(Some(func), source)?;
            let (prefix, _) = text.rsplit_once("::")?;
            let head = prefix.rsplit("::").next().unwrap_or(prefix);
            if !looks_like_type(head) {
                return None;
            }
            ctx.scope.resolve_path(prefix)
        }
        _ => None,
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

/// Names of the submodules this file declares as living in *another* file —
/// `mod cli;`, whose contents are `cli.rs` or `cli/mod.rs`.
///
/// Only the bodiless form. An inline `mod tests { … }` is flattened by this
/// indexer: `visit` steps into the body without extending the module path, so
/// a function inside it is qualified `crate::foo::helper`, not
/// `crate::foo::tests::helper`. Binding `tests` to a path no symbol carries
/// would resolve `tests::helper()` to something that matches nothing — no
/// worse than today, but a claim the graph cannot honour, so it is left out.
///
/// Kept separate from [`walk_for_imports`] rather than folded into it: a
/// `mod` declaration is a name binding, not an import, and feeding it through
/// `ImportInfo` would also mint a file-to-file import edge and possibly an
/// external-dependency node for something that is neither.
fn declared_child_modules(root: Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    walk_for_child_modules(root, source, &mut out);
    out
}

fn walk_for_child_modules(node: Node, source: &[u8], out: &mut Vec<String>) {
    if node.kind() == "mod_item" && node.child_by_field_name("body").is_none() {
        if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
            out.push(name);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_child_modules(child, source, out);
    }
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

/// Outer attributes (`#[...]`) applied to an item, in source order.
///
/// Rust's grammar makes these *preceding siblings* of the item, not children
/// — the same shape `extract_rust_docstring` walks — so this scans backwards
/// and reverses at the end.
///
/// `#![...]` inner attributes are skipped: they configure the enclosing
/// module or crate, and attributing `#![no_std]` to whichever item happens
/// to follow it would be wrong rather than merely noisy.
///
/// The path keeps every segment (`tokio::main`, not `main`), because in Rust
/// the qualifier is what distinguishes an attribute macro from a plain one.
/// Boundary rules match those with a `*::main` pattern.
fn extract_attributes(node: &Node, source: &[u8]) -> Vec<Annotation> {
    let mut out = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "attribute_item" => {
                if let Some(attr) = p.named_child(0) {
                    // Positional, not by field name: tree-sitter-rust gives
                    // `attribute` an unnamed `identifier`/`scoped_identifier`
                    // followed by a `token_tree`, with no `path` or
                    // `arguments` fields to ask for.
                    let path = attr.named_child(0);
                    let args = attr.named_child(1).filter(|n| n.kind() == "token_tree");
                    if let Some(name) = get_node_text(path, source) {
                        out.push(Annotation {
                            name: name.trim().to_string(),
                            args: annotation_args(args, source),
                        });
                    }
                }
            }
            // Doc comments interleave freely with attributes above an item;
            // neither ends the run.
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = p.prev_sibling();
    }
    out.reverse();
    out
}

/// A `#[cfg(test)]` module gates everything inside it behind test builds,
/// but nothing in its subtree carries the attribute itself — a helper `fn`
/// inside `mod tests` has no `#[cfg(test)]` of its own. Tag the symbol so
/// the `is_test` fact can see it without conflating file classification
/// (a production file like `serve.rs` legitimately holds a `mod tests`).
///
/// The annotation is a compact string `cfg(test)` rather than the structural
/// `{ name: "cfg", args: Some("test") }` pair — it reads correctly in the
/// `annotations` fact and never round-trips through the AST again.
fn test_annotation(annotations: &mut Vec<Annotation>) {
    let tagged = annotations
        .iter()
        .any(|a| a.name == "cfg(test)" || a.name == "test");
    if !tagged {
        annotations.push(Annotation {
            name: "cfg(test)".to_string(),
            args: None,
        });
    }
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
