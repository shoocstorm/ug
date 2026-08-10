//! TypeScript / JavaScript indexer. Handles `.ts`, `.tsx`, `.js`, `.jsx`.
//!
//! The TypeScript grammar covers JavaScript as a superset, so a single
//! tree-sitter parser is reused for all four extensions.

use crate::indexer::common::{
    annotation_args, calculate_nesting, extract_docstring, extract_params_from_signature,
    extract_return_type, first_string_arg, get_node_text,
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

    fn extract_symbols(&self, source: &[u8], root: Node, ctx: &FileContext) -> Vec<Symbol> {
        let scope = ImportScope::new(
            "typescript",
            module_path(ctx.path, "typescript"),
            ctx.imports,
        );
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
    /// Written class name -> (property name -> written property type), so
    /// `this.store.save(..)` can be typed.
    fields: HashMap<String, HashMap<String, String>>,
}

/// The class or interface currently being walked.
struct OwnerCtx {
    /// Written name, e.g. `OrderService`.
    name: String,
    /// Qualified name, e.g. `src/svc/order.OrderService`.
    fqn: String,
}

/// Recursive AST walk. Each node is offered to `extract_symbol_from_node`,
/// then we descend into every child unconditionally - nested classes /
/// functions all surface as their own symbols.
///
/// `owner` is the class or interface whose body we are inside. The walk used
/// to carry nothing at all, so a `method_definition` became a symbol named
/// just `save`, with no link to the type declaring it. That is why a
/// TypeScript class had no members in the graph and why `obj.save()` had
/// nothing to resolve against but every other `save` in the repo.
fn visit(
    node: Node,
    source: &[u8],
    ctx: &Ctx,
    owner: Option<&OwnerCtx>,
    symbols: &mut Vec<Symbol>,
) {
    extract_symbol_from_node(&node, source, ctx, owner, symbols);

    let inner = match node.kind() {
        "class_declaration" | "interface_declaration" => {
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

/// Property name -> written type, per class in this file.
///
/// Covers both `private store: Store;` field declarations and the
/// constructor-parameter-property shorthand (`constructor(private store:
/// Store)`), which is how most dependency-injected TypeScript declares its
/// collaborators.
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
    if node.kind() == "class_declaration" {
        if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
            let mut fields: HashMap<String, String> = HashMap::new();
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for member in body.named_children(&mut cursor) {
                    match member.kind() {
                        "public_field_definition" => {
                            let (Some(fname), Some(ftype)) = (
                                get_node_text(member.child_by_field_name("name"), source),
                                annotated_type(&member, source),
                            ) else {
                                continue;
                            };
                            fields.insert(fname, ftype);
                        }
                        // `constructor(private store: Store)` declares a
                        // field and a parameter in one breath.
                        "method_definition" => {
                            if get_node_text(member.child_by_field_name("name"), source).as_deref()
                                != Some("constructor")
                            {
                                continue;
                            }
                            let Some(params) = member.child_by_field_name("parameters") else {
                                continue;
                            };
                            let mut pc = params.walk();
                            for p in params.named_children(&mut pc) {
                                let inner = if p.kind() == "required_parameter" {
                                    p
                                } else {
                                    continue;
                                };
                                let (Some(pname), Some(ptype)) = (
                                    parameter_binding(&inner, source),
                                    annotated_type(&inner, source),
                                ) else {
                                    continue;
                                };
                                fields.insert(pname, ptype);
                            }
                        }
                        _ => {}
                    }
                }
            }
            out.insert(name, fields);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_class_fields(child, source, out);
    }
}

/// The name a parameter binds, skipping any modifiers in front of it.
///
/// `constructor(private store: Store)` puts an `accessibility_modifier`
/// first, so reading "the first named child" yields `private` — which then
/// becomes both the recorded parameter name and, for the field shorthand,
/// the key nothing can ever look up.
fn parameter_binding(param: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = param.walk();
    for child in param.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "accessibility_modifier" | "override_modifier" | "readonly" | "type_annotation"
        ) {
            continue;
        }
        if let Some(text) = get_node_text(Some(child), source) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// The bare type name from a `type` annotation, whose text carries a leading
/// colon on this grammar.
fn annotated_type(node: &Node, source: &[u8]) -> Option<String> {
    let raw = get_node_text(node.child_by_field_name("type"), source)?;
    let cleaned = raw.trim_start_matches(':').trim();
    let bare = base_type_name(cleaned);
    (!bare.is_empty()).then(|| bare.to_string())
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

    let annotations = extract_decorators(node, source);

    match kind {
        // `method_signature` is an interface member. Extracting it makes an
        // interface a node with contents rather than an empty one, and gives
        // an implementing class's method something to `Overrides`.
        "function_declaration" | "method_definition" | "method_signature" => {
            let Some(raw_name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // A free function is not a member, however deeply it nests.
            let owner = (kind != "function_declaration").then_some(owner).flatten();

            // Members display as `Type.member`, matching Java. That reads
            // better and, more importantly, splits into searchable words:
            // `OrderService.cancel` gives four where `cancel` gives one.
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
            let (calls, call_refs, uses) = extract_calls(node, source, ctx, owner, &params);
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
                call_refs,
                uses,
                qualified_name,
                owner: owner.map(|o| o.fqn.clone()),
                metrics: Some(metrics),
                annotations,
                ..Default::default()
            });
        }
        "class_declaration" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            // Heritage names are qualified through this file's imports: the
            // graph builder keys its supertype walk on qualified names, and a
            // written `Store` means whichever `Store` this file imported.
            let extends = qualify_heritage(extract_extends(node, source), ctx);
            let implements = qualify_heritage(extract_implements(node, source), ctx);
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
                implements,
                calls: Vec::new(),
                metrics: None,
                annotations,
                ..Default::default()
            });
        }
        "interface_declaration" => {
            let Some(name) = get_node_text(node.child_by_field_name("name"), source) else {
                return;
            };
            let extends = qualify_heritage(extract_extends(node, source), ctx);
            out.push(Symbol {
                id: format!("interface:{}:{}", start, name),
                qualified_name: Some(ctx.scope.qualify(&name)),
                name,
                kind: "interface".to_string(),
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
                ..Default::default()
            });
        }
        // `const foo = () => …` and `let x = 1` at module level.
        //
        // `lexical_declaration` was absent here, and `variable_declaration`
        // has no `name` field to read — so no `const`/`let`/`var` binding
        // ever became a symbol. That is the *common* shape of a function in
        // this language, so it has to be one.
        //
        // Which of the two a binding is gets decided *here*, off the
        // initializer, rather than downstream off the kind string. `graph.rs`
        // used to call every `variable` a Function to cover the arrow case,
        // which made a Java field a Function too; it can only stop doing that
        // if this indexer says which bindings are actually callable.
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
                let is_fn = binds_a_function(&decl);
                // `const handler = () => …` is the common shape of a function
                // in this language, so its body's call sites matter as much
                // as a `function` declaration's.
                let (calls, call_refs, uses) = extract_calls(&decl, source, ctx, owner, &[]);
                out.push(Symbol {
                    id: format!("{}:{}:{}", if is_fn { "fn" } else { "var" }, start, name),
                    qualified_name: Some(ctx.scope.qualify(&name)),
                    name,
                    kind: if is_fn { "function" } else { "variable" }.to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: extract_docstring(node, source),
                    signature: None,
                    imports: Vec::new(),
                    exports: Vec::new(),
                    extends: Vec::new(),
                    implements: Vec::new(),
                    calls,
                    call_refs,
                    uses,
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
                qualified_name: Some(ctx.scope.qualify(&name)),
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

/// Decorators on a class, method or interface member, in source order.
///
/// The grammar attaches them two different ways depending on the construct
/// and the grammar version — as leading children of the declaration, or as
/// preceding siblings when the declaration is wrapped (an `export class`).
/// Both are checked, and a duplicate cannot arise because only one of the
/// two ever holds them for a given node.
///
/// Names keep their dotted receiver for the same reason Python's do — see
/// `python::extract_decorators`.
fn extract_decorators(node: &Node, source: &[u8]) -> Vec<Annotation> {
    let mut out = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Decorators lead the declaration; stop at the first thing that is
        // not one so a `@Injectable` on a *parameter* deeper in the body
        // cannot be read as decorating the method.
        if child.kind() == "decorator" {
            push_decorator(&child, source, &mut out);
        } else if !child.is_extra() && child.kind() != "comment" {
            break;
        }
    }
    if !out.is_empty() {
        return out;
    }

    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "decorator" {
            break;
        }
        push_decorator(&p, source, &mut out);
        prev = p.prev_named_sibling();
    }
    // Walking backwards yields them bottom-up; source order is what the
    // `Annotation` contract promises.
    out.reverse();
    out
}

fn push_decorator(node: &Node, source: &[u8], out: &mut Vec<Annotation>) {
    let Some(expr) = node.named_child(0) else {
        return;
    };
    let (name_node, args) = if expr.kind() == "call_expression" {
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

/// Qualify written heritage names (`extends Base`, `implements Store`)
/// through this file's imports, so they key the same way declarations do.
fn qualify_heritage(written: Vec<String>, ctx: &Ctx) -> Vec<String> {
    written
        .iter()
        .filter_map(|w| ctx.scope.resolve_type_ref(w))
        .collect()
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
) -> (Vec<String>, Vec<CallRef>, Vec<String>) {
    let mut env = TypeEnv::new();

    if let Some(o) = owner {
        env.insert("this", o.fqn.clone());
        if let Some(fields) = ctx.fields.get(&o.name) {
            for (fname, ftype) in fields {
                if let Some(fqn) = ctx.scope.lookup(ftype) {
                    env.insert(format!("this.{}", fname), fqn);
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
    collect_calls(
        node, source, ctx, owner, &mut env, &mut calls, &mut refs, &mut uses,
    );
    (calls, refs, uses)
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
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        record_constant_use(&child, source, ctx, uses);
        match child.kind() {
            "variable_declarator" => record_local(&child, source, ctx, env),
            "call_expression" => {
                if let Some(r) = call_ref_for(&child, source, ctx, owner, env) {
                    push_call(calls, &r.name);
                    refs.push(r);
                }
            }
            "new_expression" => {
                if let Some(ty) = get_node_text(child.child_by_field_name("constructor"), source) {
                    let bare = base_type_name(&ty);
                    push_call(calls, bare);
                    refs.push(CallRef {
                        name: CTOR.to_string(),
                        owner_type: ctx.scope.lookup(bare),
                        argc: argument_count(&child),
                        first_string_arg: first_string_arg(&child, "arguments", source),
                        qualified: None,
                        is_ctor: true,
                        has_receiver: true,
                    });
                }
            }
            _ => {}
        }
        collect_calls(&child, source, ctx, owner, env, calls, refs, uses);
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
        "member_expression" => {
            let name = get_node_text(func.child_by_field_name("property"), source)?;
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
        "this" => owner.map(|o| o.fqn.clone()),
        "identifier" => {
            let text = get_node_text(Some(*node), source)?;
            if let Some(t) = env.get(&text) {
                return Some(t.to_string());
            }
            // A capitalised bare identifier in receiver position is a class
            // or namespace, i.e. a static call.
            if looks_like_type(&text) {
                return ctx.scope.lookup(&text);
            }
            None
        }
        // Only `this.field`. Anything deeper is an expression we would be
        // inventing a type for.
        "member_expression" => {
            let object = node.child_by_field_name("object")?;
            if object.kind() != "this" {
                return None;
            }
            let field = get_node_text(node.child_by_field_name("property"), source)?;
            env.get(&format!("this.{}", field)).map(str::to_string)
        }
        _ => None,
    }
}

/// Record `const x: Foo = …` and `const x = new Foo()`.
fn record_local(decl: &Node, source: &[u8], ctx: &Ctx, env: &mut TypeEnv) {
    let Some(name_node) = decl.child_by_field_name("name") else {
        return;
    };
    if name_node.kind() != "identifier" {
        return;
    }
    let Some(name) = get_node_text(Some(name_node), source) else {
        return;
    };

    if let Some(ty) = annotated_type(decl, source) {
        if let Some(fqn) = ctx.scope.lookup(&ty) {
            env.insert(name, fqn);
            return;
        }
    }

    let Some(value) = decl.child_by_field_name("value") else {
        return;
    };
    if value.kind() != "new_expression" {
        return;
    }
    let Some(ty) = get_node_text(value.child_by_field_name("constructor"), source) else {
        return;
    };
    if let Some(fqn) = ctx.scope.lookup(base_type_name(&ty)) {
        env.insert(name, fqn);
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
            // it is the first named child that isn't a modifier. Reading a
            // `name` field always came back empty, which emptied the whole
            // AST branch and sent every function to the regex fallback below;
            // that regex reads any word inside the first `(...)`, so
            // `f(a, b = 1)` came out with three parameters: `a`, `b` and `1`.
            let Some(name) = parameter_binding(&child, source) else {
                continue;
            };
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

/// Does this `variable_declarator` bind a callable rather than data?
///
/// `const handler = () => …` and `const run = async function () {…}` are
/// functions that happen to be spelled as bindings; `const MAX = 3` and
/// `let client = new Client()` are not. The distinction is the whole reason
/// the graph can stop treating every binding as callable — see the caller.
///
/// Type assertions are unwrapped (`const f = (() => …) as Handler`), since
/// the assertion changes the declared type, not what the value is.
fn binds_a_function(decl: &Node) -> bool {
    let Some(mut value) = decl.child_by_field_name("value") else {
        return false;
    };
    loop {
        match value.kind() {
            "arrow_function" | "function_expression" | "function" | "generator_function" => {
                return true
            }
            "as_expression" | "satisfies_expression" | "parenthesized_expression" => {
                let Some(inner) = value.named_child(0) else {
                    return false;
                };
                value = inner;
            }
            _ => return false,
        }
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
        let imports = idx.extract_imports(src.as_bytes(), root);
        let ctx = FileContext {
            path: "src/sample.ts",
            imports: &imports,
        };
        (
            idx.extract_symbols(src.as_bytes(), root, &ctx),
            imports,
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
        // Heritage names are qualified through the file's imports, because
        // that is what the graph builder's supertype walk is keyed on. With
        // no import in scope they resolve against this module.
        let (symbols, _, _) = parse("class Svc extends Base implements Api, Closeable {}\n");
        let c = find(&symbols, "Svc");
        assert_eq!(c.kind, "class");
        assert_eq!(c.extends, vec!["src/sample.Base"]);
        assert_eq!(c.implements, vec!["src/sample.Api", "src/sample.Closeable"]);
    }

    #[test]
    fn a_parameterised_supertype_reduces_to_its_base_name() {
        // `Base<T>` matches no declared class, so the generic argument has
        // to come off or the edge is dropped.
        let (symbols, _, _) = parse("class A extends Base<Order> implements Api<string> {}\n");
        let c = find(&symbols, "A");
        assert_eq!(c.extends, vec!["src/sample.Base"]);
        assert_eq!(c.implements, vec!["src/sample.Api"]);
    }

    #[test]
    fn an_interface_records_its_extends_list() {
        let (symbols, _, _) = parse("interface Api extends Base, Other { go(): void }\n");
        let i = find(&symbols, "Api");
        assert_eq!(i.kind, "interface");
        assert_eq!(i.extends, vec!["src/sample.Base", "src/sample.Other"]);
    }

    #[test]
    fn a_class_with_no_heritage_records_none() {
        let (symbols, _, _) = parse("class Plain { run() {} }\n");
        let c = find(&symbols, "Plain");
        assert!(c.extends.is_empty() && c.implements.is_empty());
    }

    #[test]
    fn a_top_level_arrow_const_becomes_a_symbol() {
        // The common shape of a function in this language, so it is emitted
        // as one — the graph no longer has to guess from the `variable` kind.
        let (symbols, _, _) = parse("export const handler = (n: number) => n + 1;\n");
        let v = find(&symbols, "handler");
        assert_eq!(v.kind, "function");
    }

    #[test]
    fn a_top_level_data_const_is_not_a_function() {
        let (symbols, _, _) = parse("export const client = new Client();\nlet count = 0;\n");
        assert_eq!(find(&symbols, "client").kind, "variable");
        assert_eq!(find(&symbols, "count").kind, "variable");
    }

    #[test]
    fn a_function_expression_and_an_asserted_arrow_are_both_functions() {
        let (symbols, _, _) = parse(
            "const run = async function () { return 1; };\n\
             const typed = ((n: number) => n) as Handler;\n",
        );
        assert_eq!(find(&symbols, "run").kind, "function");
        assert_eq!(find(&symbols, "typed").kind, "function");
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
        // A method displays as `Type.member` now, so that its name says which
        // type it is on and so that it splits into more than one search term.
        assert_eq!(find(&symbols, "Svc.run").kind, "method_definition");
        assert_eq!(find(&symbols, "Api").kind, "interface");
        assert_eq!(find(&symbols, "Alias").kind, "type");
        assert_eq!(find(&symbols, "Svc.run").calls, vec!["helper"]);
        // An interface's members are symbols too, so the interface is a node
        // with contents rather than an empty one.
        assert_eq!(find(&symbols, "Api.go").kind, "method_signature");
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

    /// TypeScript now takes the same precise-resolution path Java does, so
    /// its symbols must carry qualified names and typed call sites. This
    /// test used to assert the exact opposite — that all three stayed empty —
    /// which is a fair summary of why `obj.save()` had nothing to resolve
    /// against but every other `save` in the repo.
    #[test]
    fn typescript_symbols_carry_qualified_names_and_owners() {
        let (symbols, _, _) = parse("class C { m() {} }\nexport const x = 1;\n");

        let class = find(&symbols, "C");
        assert_eq!(class.qualified_name.as_deref(), Some("src/sample.C"));
        assert!(class.owner.is_none(), "a class is not a member");

        let method = find(&symbols, "C.m");
        assert_eq!(method.qualified_name.as_deref(), Some("src/sample.C#m"));
        assert_eq!(method.owner.as_deref(), Some("src/sample.C"));

        // Undecorated code carries no annotations. Route *composition* stays
        // Java-only: joining a class-level prefix to a member-level path is
        // a Spring/JAX-RS convention, so a Nest `@Get('/x')` reports its path
        // as the decorator's argument rather than as a composed
        // `Symbol::route`.
        for s in &symbols {
            assert!(s.annotations.is_empty(), "{}", s.name);
            assert!(s.route.is_none(), "{}", s.name);
        }
    }

    #[test]
    fn decorators_are_captured_on_classes_and_methods() {
        let (symbols, _, _) = parse(
            r#"
            @Controller('orders')
            class OrderController {
                @Get(':id')
                find(id: string) {}
            }
            "#,
        );

        let class = find(&symbols, "OrderController");
        assert_eq!(class.annotations.len(), 1);
        assert_eq!(class.annotations[0].name, "Controller");
        assert_eq!(class.annotations[0].args.as_deref(), Some("'orders'"));

        let method = find(&symbols, "OrderController.find");
        assert_eq!(method.annotations.len(), 1);
        assert_eq!(method.annotations[0].name, "Get");
        assert_eq!(method.annotations[0].args.as_deref(), Some("':id'"));
    }

    #[test]
    fn a_marker_decorator_has_no_arguments() {
        let (symbols, _, _) = parse("@Injectable()\nclass Svc {}\n");

        let c = find(&symbols, "Svc");
        assert_eq!(c.annotations.len(), 1);
        assert_eq!(c.annotations[0].name, "Injectable");
        assert!(c.annotations[0].args.is_none(), "empty parens are no args");
    }

    #[test]
    fn a_decorator_on_an_exported_class_is_still_found() {
        // The `export` wrapper is the case where the grammar hangs the
        // decorator off a sibling rather than off the declaration.
        let (symbols, _, _) = parse("@Controller()\nexport class Api {}\n");

        let c = find(&symbols, "Api");
        assert_eq!(
            c.annotations.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["Controller"]
        );
    }

    #[test]
    fn stacked_decorators_are_kept_in_source_order() {
        let (symbols, _, _) = parse(
            r#"
            class C {
                @Get('/x')
                @UseGuards(AuthGuard)
                handle() {}
            }
            "#,
        );

        let names: Vec<&str> = find(&symbols, "C.handle")
            .annotations
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, vec!["Get", "UseGuards"]);
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
