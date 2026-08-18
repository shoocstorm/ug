use crate::indexer::{normalize_path, resolve_relative};
use crate::types::{
    GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeFolderMeta, GraphNodeType,
};
use petgraph::graph::{DiGraph, NodeIndex};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};

/// Extension permutations tried when resolving an import path against the
/// file index. Empty string is included so that imports already carrying an
/// extension (`./foo.ts`, markdown links to `PROGRESS.md`) succeed without
/// any extra work. Order matters: the first match wins, so list the most
/// specific candidates first.
const FILE_RESOLVE_EXT_CANDIDATES: &[&str] = &[
    "",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".py",
    ".java",
    ".md",
    ".mdx",
    ".markdown",
    "/index.ts",
    "/index.tsx",
    "/index.js",
    "/index.jsx",
    "/index.md",
    "/README.md",
    "/__init__.py",
];

/// How far up a type hierarchy an inherited member is looked for. Deep
/// enough for any real class chain; bounded so a cycle in a malformed graph
/// can't spin.
const MAX_SUPERTYPE_DEPTH: usize = 8;

/// Cap on the implementations a single interface call expands to. A call on
/// a `Repository` in a large codebase can have dozens of implementors, and
/// wiring every one turns a precise edge back into noise.
const MAX_DISPATCH_FANOUT: usize = 8;

/// Cap on the types a single on-demand (`import a.b.*`) import wires up.
const MAX_WILDCARD_FANOUT: usize = 64;

/// Cross-file resolution tables, keyed on the qualified names languages
/// like Java supply.
///
/// The language-agnostic resolver ([`resolve_symbol`]) matches on bare
/// names, which is adequate where names are near-unique per file. It is not
/// adequate for Java: `save`, `execute` and `handle` appear in every layer,
/// and picking "the first registered id" makes each such edge a coin flip.
/// These tables let a call that knows its receiver's type land on exactly
/// one method, and only fall back to name matching when it doesn't.
#[derive(Default)]
struct QualifiedIndex {
    /// Every qualified name in the repo -> its node id, whatever kind of
    /// symbol it names.
    ///
    /// This is the map a *module*-structured language resolves against.
    /// `crate::project::read_meta(..)` names its target completely at the
    /// call site — there is no receiver, no dispatch and nothing to
    /// disambiguate, so one exact lookup is the whole job. Java never needs
    /// it (a Java call site names a member and a receiver, never a path),
    /// which is why the tables below came first.
    by_qualified: HashMap<String, String>,
    /// `pkg.Type#member` -> candidate node ids, each with its parameter
    /// count so overloads can be told apart.
    members: HashMap<String, Vec<(String, u32)>>,
    /// `pkg.Type` -> node id.
    types: HashMap<String, String>,
    /// Package -> ids of the types declared directly in it, for wildcard
    /// imports.
    packages: HashMap<String, Vec<String>>,
    /// Node id -> the file node id it belongs to.
    file_of: HashMap<String, String>,
    /// `pkg.Type` -> its qualified supertypes.
    supers: HashMap<String, Vec<String>>,
    /// `pkg.Type` -> the qualified types that extend or implement it.
    subs: HashMap<String, Vec<String>>,
}

impl QualifiedIndex {
    /// Node id for `owner#member` taking `argc` arguments, searching the
    /// owner first and then up its supertypes.
    ///
    /// Overload choice prefers an exact parameter-count match and falls back
    /// to the first declaration, so a call through a varargs or defaulted
    /// signature still resolves rather than dropping.
    fn method(&self, owner: &str, member: &str, argc: u32) -> Option<String> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut frontier: Vec<&str> = vec![owner];

        for _ in 0..MAX_SUPERTYPE_DEPTH {
            let mut next: Vec<&str> = Vec::new();
            for ty in frontier {
                if !seen.insert(ty) {
                    continue;
                }
                if let Some(candidates) = self.members.get(&format!("{}#{}", ty, member)) {
                    if let Some((id, _)) = candidates.iter().find(|(_, n)| *n == argc) {
                        return Some(id.clone());
                    }
                    if let Some((id, _)) = candidates.first() {
                        return Some(id.clone());
                    }
                }
                if let Some(supers) = self.supers.get(ty) {
                    next.extend(supers.iter().map(String::as_str));
                }
            }
            if next.is_empty() {
                return None;
            }
            frontier = next;
        }
        None
    }

    /// Qualified types that implement or extend `owner`, transitively.
    ///
    /// This is what answers "where does this actually execute?" in a
    /// codebase written against interfaces. A call to `OrderRepository.save`
    /// where `OrderRepository` is an interface reaches no running code on
    /// its own; the implementations are the code, and with constructor or
    /// field injection they are also the only wiring that exists.
    fn implementors(&self, owner: &str, cap: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut frontier: Vec<&str> = vec![owner];

        for _ in 0..MAX_SUPERTYPE_DEPTH {
            let mut next: Vec<&str> = Vec::new();
            for ty in frontier {
                let Some(subs) = self.subs.get(ty) else {
                    continue;
                };
                for sub in subs {
                    if !seen.insert(sub.as_str()) {
                        continue;
                    }
                    out.push(sub.clone());
                    if out.len() >= cap {
                        return out;
                    }
                    next.push(sub.as_str());
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }
}

fn build_graph_from_index(index_result: &crate::types::IndexResult) -> GraphData {
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    // Same name can exist in many files (e.g. helper `parse` in three
    // modules). Keep every match so cross-file resolvers can prefer the
    // one in the same file as the caller.
    let mut symbol_id_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut qualified = QualifiedIndex::default();
    // `route:<METHOD> <path>` nodes, deduped across the repo — two handlers
    // mapped to the same route are a real thing (different HTTP verbs share
    // a path) and should meet at one node.
    let mut route_ids: HashSet<String> = HashSet::new();
    // Node ids that hold a value rather than behaviour, so a `Uses` edge can
    // be restricted to the references that mean something.
    let mut constant_ids: HashSet<String> = HashSet::new();
    let mut resolution = crate::types::ResolutionStats::default();

    let (path_index, basename_index) = build_file_indexes(&index_result.files);

    // Pass -1: declared third-party dependencies. `GraphNodeType::Dependency`
    // has existed since the beginning and nothing ever created one, so the
    // manifest the indexer parses had no representation in the graph at all.
    let mut dependency_ids: HashMap<&str, String> = HashMap::new();
    for dep in &index_result.dependencies {
        let id = format!("dep:{}", dep.name);
        dependency_ids.insert(dep.name.as_str(), id.clone());
        nodes.push(GraphNode {
            id,
            name: dep.name.clone(),
            node_type: GraphNodeType::Dependency,
            docstring: dep.version.clone().map(|v| format!("version {}", v)),
            ..Default::default()
        });
    }

    // Pass 0: folder forest. Folder nodes carry filesystem hierarchy that no
    // single FileNode captures (`src/components/` vs `tests/components/`), so
    // adding them lets the visualizer render the project tree and lets the
    // RAG retriever climb from a leaf file to its containing folder for a
    // higher-level summary. Edges:
    //   - parent_folder -> child_folder  (Contains)
    //   - folder        -> immediate file (Contains, only when the file
    //                                       resolved into a graph node above)
    // Folder nodes get an `id` of `folder:<path>` mirroring `file:<path>`.
    for f in &index_result.folders {
        let folder_id = format!("folder:{}", f.path);
        let parent_id = f.parent.as_ref().map(|p| format!("folder:{}", p));

        nodes.push(GraphNode {
            id: folder_id.clone(),
            name: f.name.clone(),
            node_type: GraphNodeType::Folder,
            file: None,
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            // Pre-enrichment we don't have a written summary; the storage
            // text builder synthesizes one from the meta below.
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: Some(GraphNodeFolderMeta {
                depth: f.depth,
                parent: f.parent.clone(),
                classification: f.classification.clone(),
                readme: f.readme.clone(),
                total_files: f.total_files,
                language_breakdown: f.language_breakdown.clone(),
                summary: f.summary.clone(),
            }),
            ..Default::default()
        });

        if let Some(pid) = parent_id {
            edges.push(GraphEdge {
                source: pid,
                target: folder_id.clone(),
                edge_type: GraphEdgeType::Contains,
            });
        }

        // Wire each immediate file under this folder. We resolve through
        // path_index (rather than format!("file:{}", path)) so a file that
        // failed to parse and never produced a FileNode silently drops out
        // instead of leaving a dangling target.
        for child_file_path in &f.child_files {
            if let Some(file_id) = path_index.get(child_file_path) {
                edges.push(GraphEdge {
                    source: folder_id.clone(),
                    target: file_id.clone(),
                    edge_type: GraphEdgeType::Contains,
                });
            }
        }
    }

    // Pass 1: build all file & symbol nodes and populate symbol_id_map so
    // later passes (calls/extends/implements/imports) can resolve targets to
    // real node IDs even when the target is defined later or in another file.
    for file in &index_result.files {
        let normalized_file_path = normalize_path(&file.path);
        let file_node_id = format!("file:{}", normalized_file_path);

        let file_node_type = match &file.classification {
            Some(crate::types::FileClassification::Config) => GraphNodeType::Config,
            _ => GraphNodeType::File,
        };

        nodes.push(GraphNode {
            id: file_node_id.clone(),
            name: normalized_file_path.clone(),
            node_type: file_node_type,
            file: Some(normalized_file_path.clone()),
            start_line: None,
            end_line: None,
            metrics: None,
            signature: None,
            docstring: None,
            imports: file.imports.iter().map(|imp| crate::types::GraphNodeImport {
                path: imp.path.clone(),
                imported: imp.imported.iter().map(|i| crate::types::GraphImportedItem {
                    name: i.name.clone(),
                    alias: i.alias.clone(),
                }).collect(),
            }).collect(),
            exports: file.exports.iter().map(|exp| crate::types::GraphNodeExport {
                name: exp.name.clone(),
                alias: exp.alias.clone(),
                is_default: exp.is_default,
            }).collect(),
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            language: Some(file.language.clone()),
            classification: file.classification.clone(),
            ..Default::default()
        });

        // Stack of (heading_level, sym_node_id) maintained per file while
        // walking symbols in source order. Used to resolve the parent of
        // each markdown heading: pop any heading on top whose level is
        // greater-or-equal to the current one, and the stack's new top is
        // the parent (the file node when the stack is empty). Non-markdown
        // files never push to the stack.
        let mut heading_stack: Vec<(usize, String)> = Vec::new();

        let sym_ids = symbol_node_ids(file, &normalized_file_path);

        for (sym_idx, sym) in file.symbols.iter().enumerate() {
            let heading_level = parse_heading_level(&sym.kind);

            let node_type = if heading_level.is_some() {
                GraphNodeType::Concept
            } else {
                match sym.kind.as_str() {
                    "function" | "function_declaration" | "method_definition" => GraphNodeType::Function,
                    "class" | "class_declaration" => GraphNodeType::Class,
                    "interface" | "interface_declaration" => GraphNodeType::Interface,
                    // A data binding is not callable. `private final Bot
                    // bot;` is a field, and calling it a Function made every
                    // "how many functions" statistic and every function-typed
                    // filter wrong. The one shape that genuinely *is* a
                    // function — JS/TS `const foo = () => ...` — is settled
                    // upstream: the TypeScript indexer emits kind `function`
                    // for a declarator with a function initializer, so it
                    // never reaches this arm.
                    //
                    // `MAX_RETRIES` still reads as a constant by name, the
                    // same rule Python's module-level bindings get below.
                    "variable" | "variable_declaration" => {
                        if crate::indexer::scope::looks_like_constant(simple_name(&sym.name)) {
                            GraphNodeType::Constant
                        } else {
                            GraphNodeType::Variable
                        }
                    }
                    "type" | "type_alias_declaration" => GraphNodeType::Interface,
                    // Rust kinds — structs/enums map to Class, traits and
                    // type aliases map to Interface; macros fall through to
                    // Function via the catch-all (they're callable).
                    "struct" | "enum" => GraphNodeType::Class,
                    "trait" | "type_alias" => GraphNodeType::Interface,
                    "constant" => GraphNodeType::Constant,
                    // A Python module-level binding is whatever its name
                    // says it is. `MAX_RETRIES = 3` is a constant; `app =
                    // Flask(__name__)` is a variable. Neither is a function.
                    "assignment" => {
                        if crate::indexer::scope::looks_like_constant(simple_name(&sym.name)) {
                            GraphNodeType::Constant
                        } else {
                            GraphNodeType::Variable
                        }
                    }
                    _ => GraphNodeType::Function,
                }
            };

            let sym_node_id = sym_ids[sym_idx].clone();
            if node_type == GraphNodeType::Constant {
                constant_ids.insert(sym_node_id.clone());
            }

            let signature = sym.signature.as_ref().map(|s| crate::types::GraphNodeSignature {
                params: s.params.iter().map(|p| crate::types::Param {
                    name: p.name.clone(),
                    param_type: p.param_type.clone(),
                    optional: p.optional,
                    default: p.default.clone(),
                }).collect(),
                return_type: s.return_type.clone(),
            });

            let imports = sym.imports.iter().map(|imp| crate::types::GraphNodeImport {
                path: imp.path.clone(),
                imported: imp.imported.iter().map(|i| crate::types::GraphImportedItem {
                    name: i.name.clone(),
                    alias: i.alias.clone(),
                }).collect(),
            }).collect();

            let exports = sym.exports.iter().map(|exp| crate::types::GraphNodeExport {
                name: exp.name.clone(),
                alias: exp.alias.clone(),
                is_default: exp.is_default,
            }).collect();

            nodes.push(GraphNode {
                id: sym_node_id.clone(),
                name: sym.name.clone(),
                node_type,
                file: Some(normalized_file_path.clone()),
                start_line: Some(sym.start_line),
                end_line: Some(sym.end_line),
                metrics: sym.metrics.clone(),
                signature,
                docstring: sym.docstring.clone(),
                imports,
                exports,
                extends: sym.extends.clone(),
                implements: sym.implements.clone(),
                calls: sym.calls.clone(),
                folder: None,
                qualified_name: sym.qualified_name.clone(),
                annotations: sym.annotations.clone(),
                route: sym.route.clone(),
                boundaries: sym.boundaries.clone(),
                // Carried down from the file onto every symbol in it, so
                // "group by language" or "exclude test code" is a scan
                // rather than a join back to the File node.
                language: Some(file.language.clone()),
                classification: file.classification.clone(),
            });

            // Register the symbol under every name a later pass might use to
            // reach it, before any edge resolution runs.
            if let Some(fqn) = &sym.qualified_name {
                qualified
                    .file_of
                    .insert(sym_node_id.clone(), file_node_id.clone());
                // Every qualified symbol, member or not. Two symbols cannot
                // share a qualified name without one shadowing the other, so
                // first-wins here is a tie-break between duplicates rather
                // than a choice between candidates.
                qualified
                    .by_qualified
                    .entry(fqn.clone())
                    .or_insert_with(|| sym_node_id.clone());

                match &sym.owner {
                    // A member: `pkg.Type#name`, keyed with its arity.
                    Some(_) => {
                        let argc = sym
                            .signature
                            .as_ref()
                            .map(|s| s.params.len() as u32)
                            .unwrap_or(0);
                        qualified
                            .members
                            .entry(fqn.clone())
                            .or_default()
                            .push((sym_node_id.clone(), argc));
                    }
                    // A type: also index it by package for wildcard imports,
                    // and record both directions of its hierarchy.
                    None => {
                        qualified.types.insert(fqn.clone(), sym_node_id.clone());
                        if let Some((pkg, _)) = fqn.rsplit_once('.') {
                            qualified
                                .packages
                                .entry(pkg.to_string())
                                .or_default()
                                .push(sym_node_id.clone());
                        }
                    }
                }

                // Types nested inside another type still declare members, so
                // the hierarchy is recorded for anything with supertypes.
                let supers: Vec<String> = sym
                    .extends
                    .iter()
                    .chain(sym.implements.iter())
                    .cloned()
                    .collect();
                if !supers.is_empty() {
                    for s in &supers {
                        qualified
                            .subs
                            .entry(s.clone())
                            .or_default()
                            .push(fqn.clone());
                    }
                    qualified.supers.insert(fqn.clone(), supers);
                }
            }

            // An HTTP route is a node in its own right: it is the name the
            // outside world knows this code by, and the string people search
            // for. Attaching it to the file as well as the handler keeps it
            // reachable from the structural spine.
            if let Some(route) = sym.route.as_ref().filter(|r| !r.is_empty()) {
                let route_id = format!("route:{}", route);
                if route_ids.insert(route_id.clone()) {
                    nodes.push(GraphNode {
                        id: route_id.clone(),
                        name: route.clone(),
                        node_type: GraphNodeType::Route,
                        file: Some(normalized_file_path.clone()),
                        ..Default::default()
                    });
                }
                edges.push(GraphEdge {
                    source: file_node_id.clone(),
                    target: route_id.clone(),
                    edge_type: GraphEdgeType::Contains,
                });
                edges.push(GraphEdge {
                    source: route_id,
                    target: sym_node_id.clone(),
                    edge_type: GraphEdgeType::References,
                });
            }

            if let Some(level) = heading_level {
                while let Some(&(top_level, _)) = heading_stack.last() {
                    if top_level < level {
                        break;
                    }
                    heading_stack.pop();
                }
                let parent_id = heading_stack
                    .last()
                    .map(|(_, id)| id.clone())
                    .unwrap_or_else(|| file_node_id.clone());

                edges.push(GraphEdge {
                    source: parent_id,
                    target: sym_node_id.clone(),
                    edge_type: GraphEdgeType::Contains,
                });

                heading_stack.push((level, sym_node_id.clone()));
                // Heading text is intentionally kept out of `symbol_id_map`:
                // a heading "Setup" must not be a target for code-side
                // call/extends/implements resolution.
            } else {
                symbol_id_map
                    .entry(sym.name.clone())
                    .or_default()
                    .push(sym_node_id.clone());

                // A qualified language displays members as `Type.member`, so
                // the bare member name would otherwise be unreachable by the
                // name-matching resolver — and that resolver is the fallback
                // for every call whose receiver we couldn't type.
                if sym.qualified_name.is_some() {
                    if let Some((_, simple)) = sym.name.rsplit_once('.') {
                        symbol_id_map
                            .entry(simple.to_string())
                            .or_default()
                            .push(sym_node_id.clone());
                    }
                }

                edges.push(GraphEdge {
                    source: file_node_id.clone(),
                    target: sym_node_id.clone(),
                    edge_type: GraphEdgeType::Contains,
                });

                // Members are additionally nested under the type that
                // declares them. The file edge above is kept so every
                // consumer that walks file -> symbol still works; this adds
                // the level Java actually organises code at, which is what
                // makes "what is on this class" a single hop.
                if let Some(owner_fqn) = &sym.owner {
                    if let Some(owner_id) = qualified.types.get(owner_fqn) {
                        edges.push(GraphEdge {
                            source: owner_id.clone(),
                            target: sym_node_id.clone(),
                            edge_type: GraphEdgeType::Contains,
                        });
                    }
                }
            }
        }
    }

    // Pass 2: resolve calls/extends/implements through symbol_id_map. Names
    // like `this.foo` or `obj.foo` fall back to the trailing segment so
    // member-access calls hit the right method node. When a name has matches
    // in multiple files we prefer the one in the same file as the caller.
    for file in &index_result.files {
        let normalized_file_path = normalize_path(&file.path);
        // Recomputed rather than carried over from pass 1, but through the
        // same function — the two passes must agree on every id, and
        // `symbol_node_ids` is deterministic for a given `FileNode`.
        let sym_ids = symbol_node_ids(file, &normalized_file_path);
        for (sym_idx, sym) in file.symbols.iter().enumerate() {
            if parse_heading_level(&sym.kind).is_some() {
                continue;
            }
            let sym_node_id = sym_ids[sym_idx].clone();

            for extended in &sym.extends {
                if let Some(target_id) = qualified
                    .types
                    .get(extended)
                    .cloned()
                    .or_else(|| resolve_symbol(&symbol_id_map, extended, &normalized_file_path))
                {
                    edges.push(GraphEdge {
                        source: sym_node_id.clone(),
                        target: target_id,
                        edge_type: GraphEdgeType::Extends,
                    });
                }
            }

            for implemented in &sym.implements {
                if let Some(target_id) = qualified
                    .types
                    .get(implemented)
                    .cloned()
                    .or_else(|| resolve_symbol(&symbol_id_map, implemented, &normalized_file_path))
                {
                    edges.push(GraphEdge {
                        source: sym_node_id.clone(),
                        target: target_id,
                        edge_type: GraphEdgeType::Implements,
                    });
                }
            }

            // A method that redeclares an inherited signature gets an
            // `Overrides` edge to the declaration it replaces. Without it,
            // "who implements this interface method" has no answer in the
            // graph — `Implements` is type-level and stops at the class.
            if let (Some(fqn), Some(owner)) = (&sym.qualified_name, &sym.owner) {
                if let Some((_, member)) = fqn.rsplit_once('#') {
                    let argc = sym
                        .signature
                        .as_ref()
                        .map(|s| s.params.len() as u32)
                        .unwrap_or(0);
                    for supertype in qualified.supers.get(owner).into_iter().flatten() {
                        if let Some(target_id) = qualified.method(supertype, member, argc) {
                            if target_id != sym_node_id {
                                edges.push(GraphEdge {
                                    source: sym_node_id.clone(),
                                    target: target_id,
                                    edge_type: GraphEdgeType::Overrides,
                                });
                            }
                        }
                    }
                }
            }

            // Constants a body reads. Only edges into an actual Constant or
            // Config node are drawn: a `SCREAMING_CASE` name that resolves to
            // something else is an enum variant or an external, and neither
            // is what "who uses this setting" means.
            for used in &sym.uses {
                let Some(target_id) = qualified.by_qualified.get(used) else {
                    continue;
                };
                if !constant_ids.contains(target_id) {
                    continue;
                }
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: target_id.clone(),
                    edge_type: GraphEdgeType::Uses,
                });
            }

            // Functions passed as values — callbacks, route handlers,
            // listeners. These are never invoked here, so they draw a
            // `References` edge rather than a `Calls` one; that edge is also
            // the only thing standing between a registered handler and the
            // `dead_code` query.
            for referenced in &sym.value_refs {
                let Some(target_id) = qualified.by_qualified.get(referenced) else {
                    continue;
                };
                edges.push(GraphEdge {
                    source: sym_node_id.clone(),
                    target: target_id.clone(),
                    edge_type: GraphEdgeType::References,
                });
                resolution.resolved_qualified += 1;
            }

            if sym.call_refs.is_empty() {
                // Languages that report only bare callee names.
                for called in &sym.calls {
                    if let Some(target_id) =
                        resolve_symbol(&symbol_id_map, called, &normalized_file_path)
                    {
                        edges.push(GraphEdge {
                            source: sym_node_id.clone(),
                            target: target_id,
                            edge_type: GraphEdgeType::Calls,
                        });
                        resolution.resolved_by_name += 1;
                    } else {
                        resolution.dropped_unresolved += 1;
                    }
                }
            } else {
                // Typed call sites. `sym.calls` is deliberately *not* walked
                // as well: it holds the same call sites stripped of their
                // receivers, and resolving those by name is exactly the
                // guesswork the receiver types exist to replace.
                for call in &sym.call_refs {
                    let mut resolved = false;

                    // An exact path needs no dispatch and cannot be
                    // ambiguous, so it is tried first. This is the case that
                    // used to be dropped outright: `resolve_symbol` split
                    // only on `.`, so every `a::b::c(..)` in a Rust file
                    // failed both its lookups and produced no edge at all.
                    if let Some(fqn) = &call.qualified {
                        if let Some(target_id) = qualified.by_qualified.get(fqn) {
                            edges.push(GraphEdge {
                                source: sym_node_id.clone(),
                                target: target_id.clone(),
                                edge_type: edge_for_call(call),
                            });
                            resolution.resolved_qualified += 1;
                            resolved = true;
                        }
                    }

                    if let Some(owner) = &call.owner_type {
                        if let Some(target_id) = qualified.method(owner, &call.name, call.argc) {
                            edges.push(GraphEdge {
                                source: sym_node_id.clone(),
                                target: target_id,
                                edge_type: edge_for_call(call),
                            });
                            resolution.resolved_typed += 1;
                            resolved = true;
                        }

                        // A construction whose type declares no constructor
                        // symbol — a Rust struct literal, a Python class with
                        // no `__init__` — still instantiates the type, so the
                        // edge lands on the type node itself.
                        if !resolved && call.is_ctor {
                            if let Some(target_id) = qualified.types.get(owner) {
                                edges.push(GraphEdge {
                                    source: sym_node_id.clone(),
                                    target: target_id.clone(),
                                    edge_type: GraphEdgeType::Instantiates,
                                });
                                resolution.resolved_typed += 1;
                                resolved = true;
                            }
                        }

                        // Dispatch: a call typed against an interface or an
                        // abstract base runs in the implementations, so the
                        // edge is drawn to them too. Construction is not
                        // dispatched — `new Foo()` runs `Foo`'s constructor,
                        // never a subtype's.
                        if !call.is_ctor {
                            for impl_fqn in qualified.implementors(owner, MAX_DISPATCH_FANOUT) {
                                if let Some(target_id) =
                                    qualified.method(&impl_fqn, &call.name, call.argc)
                                {
                                    edges.push(GraphEdge {
                                        source: sym_node_id.clone(),
                                        target: target_id,
                                        edge_type: GraphEdgeType::Calls,
                                    });
                                    resolution.resolved_typed += 1;
                                    resolved = true;
                                }
                            }
                        }
                    }

                    // Last resort: match the bare callee name — but only for
                    // a callee named without a receiver. A member call whose
                    // receiver we could not type has already had its one
                    // honest chance; see `CallRef::has_receiver`.
                    if !resolved && !call.has_receiver {
                        if let Some(target_id) =
                            resolve_symbol(&symbol_id_map, &call.name, &normalized_file_path)
                        {
                            edges.push(GraphEdge {
                                source: sym_node_id.clone(),
                                target: target_id,
                                edge_type: edge_for_call(call),
                            });
                            resolution.resolved_by_name += 1;
                            resolved = true;
                        }
                    }

                    if !resolved {
                        resolution.dropped_unresolved += 1;
                    }
                }
            }
        }
    }

    // Pass 3: resolve file-level imports against the file index. We emit:
    // - one `Imports` edge file→file when the target path resolves to a known
    //   file (markdown link, TS relative import, etc.)
    // - one `References` edge file→symbol per imported name that matches a
    //   symbol the indexer recorded
    // Bare unresolved imports (package names, dead links) are dropped to
    // keep the visualization free of orphan-target edges.
    for file in &index_result.files {
        let normalized_file_path = normalize_path(&file.path);
        let file_node_id = format!("file:{}", normalized_file_path);

        for import in &file.imports {
            // Qualified-name resolution first. An import path like
            // `com.example.svc` is a *package*, not a location: handing it to
            // the filesystem resolver below finds nothing, and its basename
            // fallback would go looking for a file called `com`. Resolving
            // against declared qualified names is the only thing that works,
            // and it is also exact — no basename near-miss.
            let targets = resolve_qualified_import(&qualified, import, &file.language);
            for target_id in &targets {
                edges.push(GraphEdge {
                    source: file_node_id.clone(),
                    target: target_id.clone(),
                    edge_type: GraphEdgeType::References,
                });
                if let Some(target_file_id) = qualified.file_of.get(target_id) {
                    if target_file_id != &file_node_id {
                        edges.push(GraphEdge {
                            source: file_node_id.clone(),
                            target: target_file_id.clone(),
                            edge_type: GraphEdgeType::Imports,
                        });
                    }
                }
            }
            if !targets.is_empty() {
                continue;
            }

            if !import.path.is_empty() {
                if let Some(target_file_id) = resolve_import_to_file_id(
                    &normalized_file_path,
                    &import.path,
                    &path_index,
                    &basename_index,
                ) {
                    if target_file_id != file_node_id {
                        edges.push(GraphEdge {
                            source: file_node_id.clone(),
                            target: target_file_id,
                            edge_type: GraphEdgeType::Imports,
                        });
                    }
                    continue;
                }
            }

            // Nothing in the repo answers to this specifier, so it may name a
            // third-party package. `IndexResult.dependencies` has always been
            // parsed and never used; this is what makes "which files pull in
            // axum" answerable.
            if let Some(dep_id) = dependency_ids.get(dependency_root(&import.path)) {
                edges.push(GraphEdge {
                    source: file_node_id.clone(),
                    target: dep_id.clone(),
                    edge_type: GraphEdgeType::DependsOn,
                });
            }

            for imp in &import.imported {
                if let Some(target_sym_id) =
                    resolve_symbol(&symbol_id_map, &imp.name, &normalized_file_path)
                {
                    edges.push(GraphEdge {
                        source: file_node_id.clone(),
                        target: target_sym_id,
                        edge_type: GraphEdgeType::References,
                    });
                }
            }
        }

        for exp in &file.exports {
            if let Some(target_sym_id) =
                resolve_symbol(&symbol_id_map, &exp.name, &normalized_file_path)
            {
                edges.push(GraphEdge {
                    source: file_node_id.clone(),
                    target: target_sym_id,
                    edge_type: GraphEdgeType::Exports,
                });
            }
        }
    }

    dedupe_edges(&mut edges);

    GraphData {
        nodes,
        edges,
        stats: Some(index_result.stats.clone()),
        resolution: Some(resolution),
    }
}

/// Node ids an import statement names, resolved through qualified names.
///
/// Returns empty for any import that doesn't resolve this way — a relative
/// TypeScript path, a markdown link, a package that isn't in the repo — so
/// the caller can fall through to filesystem resolution unchanged.
fn resolve_qualified_import(
    qualified: &QualifiedIndex,
    import: &crate::types::ImportInfo,
    language: &str,
) -> Vec<String> {
    if qualified.by_qualified.is_empty() || import.path.is_empty() {
        return Vec::new();
    }

    // An import path is written in the importing language's own vocabulary,
    // and so is the qualified name it has to match. Hardcoding Java's `.`
    // here meant `use crate::storage::db::Db` composed the fqn
    // `crate::storage::db.Db`, matched nothing, and fell through to the
    // filesystem resolver — which then looked for a directory called
    // `crate::storage`.
    let sep = crate::indexer::scope::module_sep(language);

    let mut out = Vec::new();
    for imp in &import.imported {
        if imp.name == "*" {
            // On-demand import: every type in the package, bounded. A
            // `java.util.*` in a repo that declares no `java.util` package
            // simply contributes nothing.
            if let Some(ids) = qualified.packages.get(&import.path) {
                out.extend(ids.iter().take(MAX_WILDCARD_FANOUT).cloned());
            }
            continue;
        }
        let fqn = format!("{}{}{}", import.path, sep, imp.name);
        if let Some(id) = qualified.by_qualified.get(&fqn) {
            out.push(id.clone());
            continue;
        }
        // `import static a.b.C.member;` — the qualifier is the type and the
        // trailing name is one of its members.
        if let Some(candidates) = qualified.members.get(&format!("{}#{}", import.path, imp.name)) {
            out.extend(candidates.iter().map(|(id, _)| id.clone()));
        }
    }
    out
}

/// Build the lookup tables used to resolve import paths to file node IDs.
///
/// `path_index` maps every spelling we want to recognise (with extension,
/// without extension) onto a single file node ID. `basename_index` is the
/// last-resort fallback: when a markdown link or import doesn't carry enough
/// path context to resolve uniquely, we look it up by basename and pick the
/// closest match. Multiple files can share a basename (`README.md` in N
/// directories), so the value is a list and disambiguation happens at lookup.
fn build_file_indexes(
    files: &[crate::types::FileNode],
) -> (HashMap<String, String>, HashMap<String, Vec<String>>) {
    let mut path_index: HashMap<String, String> = HashMap::new();
    let mut basename_index: HashMap<String, Vec<String>> = HashMap::new();

    for file in files {
        let normalized = normalize_path(&file.path);
        let id = format!("file:{}", normalized);

        path_index.insert(normalized.clone(), id.clone());

        // Also key on the path with its extension stripped so an import like
        // `./utils` resolves to a `./utils.ts` file in one lookup.
        if let Some(dot_idx) = normalized.rfind('.') {
            // Only strip if the dot is in the basename, not in some parent
            // directory like `my.module/file`.
            let last_slash = normalized.rfind('/').map(|i| i + 1).unwrap_or(0);
            if dot_idx >= last_slash {
                path_index
                    .entry(normalized[..dot_idx].to_string())
                    .or_insert_with(|| id.clone());
            }
        }

        let basename = match normalized.rfind('/') {
            Some(idx) => &normalized[idx + 1..],
            None => &normalized,
        };
        basename_index
            .entry(basename.to_string())
            .or_default()
            .push(id.clone());

        if let Some(dot_idx) = basename.rfind('.') {
            basename_index
                .entry(basename[..dot_idx].to_string())
                .or_default()
                .push(id.clone());
        }
    }

    (path_index, basename_index)
}

/// Resolve a raw import target to a file node ID, walking through several
/// progressively looser strategies:
///
/// 1. join with the source file's directory and look up exactly
/// 2. try common extensions / index files at that joined location
/// 3. look up the unjoined import path (covers absolute and root-anchored
///    imports the indexer records verbatim)
/// 4. basename fallback - useful for markdown links that drop the directory
///    (`[…](README.md)` resolving to `docs/README.md` from a sibling file)
///
/// Returns `None` for genuine externals (npm packages, dead links) so the
/// caller can drop the edge instead of leaving an orphan in the graph.
fn resolve_import_to_file_id(
    src_file_path: &str,
    import_path: &str,
    path_index: &HashMap<String, String>,
    basename_index: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let cleaned = import_path.split('#').next().unwrap_or(import_path);
    let cleaned = cleaned.split('?').next().unwrap_or(cleaned);
    if cleaned.is_empty() {
        return None;
    }

    let resolved = resolve_relative(src_file_path, cleaned);
    if let Some(id) = lookup_with_extensions(&resolved, path_index) {
        return Some(id);
    }

    let direct = normalize_path(cleaned);
    if direct != resolved {
        if let Some(id) = lookup_with_extensions(&direct, path_index) {
            return Some(id);
        }
    }

    let basename = match cleaned.rfind('/') {
        Some(idx) => &cleaned[idx + 1..],
        None => cleaned,
    };
    if !basename.is_empty() {
        if let Some(id) = lookup_basename(basename, src_file_path, basename_index) {
            return Some(id);
        }
        if let Some(dot_idx) = basename.rfind('.') {
            if let Some(id) =
                lookup_basename(&basename[..dot_idx], src_file_path, basename_index)
            {
                return Some(id);
            }
        }
    }

    None
}

fn lookup_with_extensions(base: &str, path_index: &HashMap<String, String>) -> Option<String> {
    for ext in FILE_RESOLVE_EXT_CANDIDATES {
        let candidate = if ext.is_empty() {
            base.to_string()
        } else {
            format!("{}{}", base, ext)
        };
        if let Some(id) = path_index.get(&candidate) {
            return Some(id.clone());
        }
    }
    None
}

/// Pick the basename match whose path shares the longest directory prefix
/// with the source file. Ties (or no shared prefix) fall through to the
/// first registered entry, which is good enough for the visualization.
fn lookup_basename(
    basename: &str,
    src_file_path: &str,
    basename_index: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let candidates = basename_index.get(basename)?;
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }

    let src_norm = normalize_path(src_file_path);
    let mut best: Option<(usize, &String)> = None;
    for cand in candidates {
        let cand_path = cand.strip_prefix("file:").unwrap_or(cand.as_str());
        let shared = shared_prefix_len(&src_norm, cand_path);
        match best {
            Some((cur_len, _)) if shared <= cur_len => {}
            _ => best = Some((shared, cand)),
        }
    }
    best.map(|(_, id)| id.clone())
}

fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// Assign a stable node id to every symbol in `file`, returned aligned
/// with `file.symbols`.
///
/// Ids are `<kind>:<file>:<name>` (headings: `heading:<file>:<name>`, since
/// heading level carries no disambiguation worth encoding). When the same
/// `(kind, name)` appears more than once in one file — a method defined in
/// both an inherent and a trait `impl`, several `From` impls, repeated
/// markdown headings — the second and later occurrences get a `#N` suffix
/// in source order.
///
/// **Line numbers are deliberately absent.** They used to sit in the id,
/// which meant every symbol below an edit was assigned a brand-new id on
/// the next index: it re-embedded as if new, and its previous row was
/// orphaned in the store forever. Measured on a real graph, dropping the
/// line costs almost nothing in uniqueness — 98.7% of symbols are already
/// unique on `(kind, file, name)` alone, because names arrive qualified
/// (`Db::upsert_nodes`, not `upsert_nodes`) — while making ids survive
/// unrelated edits elsewhere in the file.
///
/// The residual instability is narrow: reordering two same-named overloads
/// within one file swaps their ids. That is rare, and self-corrects on the
/// next index.
///
/// Must stay deterministic for a given `FileNode` — `build_graph` calls it
/// once per pass and the passes have to agree.
fn symbol_node_ids(file: &crate::types::FileNode, normalized_file_path: &str) -> Vec<String> {
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let mut out = Vec::with_capacity(file.symbols.len());

    for sym in &file.symbols {
        let prefix = if parse_heading_level(&sym.kind).is_some() {
            "heading".to_string()
        } else {
            sym.kind.clone()
        };
        let base = format!("{}:{}:{}", prefix, normalized_file_path, sym.name);

        let n = seen.entry((prefix, sym.name.clone())).or_insert(0);
        *n += 1;
        out.push(if *n == 1 {
            base
        } else {
            format!("{}#{}", base, n)
        });
    }
    out
}

/// The last segment of a display name: `PlaylistCmd.bot` -> `bot`.
///
/// Members carry their owning type as a prefix so five different `execute`
/// methods stay distinguishable, but a naming-convention test has to run on
/// the identifier alone — `Config.MAX_RETRIES` is not screaming-case, and
/// `MAX_RETRIES` is.
fn simple_name(display: &str) -> &str {
    display.rsplit('.').next().unwrap_or(display)
}

/// Parse a markdown heading kind like `heading_3` into its level (1-6).
/// Returns `None` for non-heading symbol kinds, so non-markdown files
/// short-circuit cheaply on the prefix check.
fn parse_heading_level(kind: &str) -> Option<usize> {
    let level: usize = kind.strip_prefix("heading_")?.parse().ok()?;
    if (1..=6).contains(&level) {
        Some(level)
    } else {
        None
    }
}

/// The package a bare import specifier belongs to.
///
/// `@scope/pkg/sub` -> `@scope/pkg`, `lodash/merge` -> `lodash`,
/// `os.path` -> `os`, `serde::de` -> `serde`. Anything relative is not a
/// package and returns something no manifest will list.
fn dependency_root(spec: &str) -> &str {
    if spec.starts_with('.') || spec.starts_with('/') {
        return spec;
    }
    if let Some(rest) = spec.strip_prefix('@') {
        // A scoped npm package is two segments, not one.
        let mut it = rest.splitn(3, '/');
        return match (it.next(), it.next()) {
            (Some(scope), Some(pkg)) => &spec[..1 + scope.len() + 1 + pkg.len()],
            _ => spec,
        };
    }
    let end = spec
        .find("::")
        .into_iter()
        .chain(spec.find('/'))
        .chain(spec.find('.'))
        .min()
        .unwrap_or(spec.len());
    &spec[..end]
}

/// Which edge a resolved call site produces.
///
/// Construction and invocation are different relationships, so they get
/// different edges — see [`GraphEdgeType::Instantiates`].
fn edge_for_call(call: &crate::types::CallRef) -> GraphEdgeType {
    if call.is_ctor {
        GraphEdgeType::Instantiates
    } else {
        GraphEdgeType::Calls
    }
}

/// Look up a symbol by name, preferring a match in the caller's own file.
///
/// This is the fallback resolver: it runs for call sites no qualified name or
/// receiver type could place. Dotted or scoped names (`this.foo`,
/// `obj.bar.baz`, `a::b::c`) fall back to their trailing identifier so a
/// member access still reaches the method node when only the bare name is in
/// the map.
fn resolve_symbol(
    map: &HashMap<String, Vec<String>>,
    name: &str,
    caller_file: &str,
) -> Option<String> {
    if let Some(id) = pick_best(map.get(name), caller_file) {
        return Some(id);
    }
    // `::` as well as `.`: Rust spells every associated call and module path
    // with it, and splitting on `.` alone meant `Db::open` matched nothing
    // and was silently dropped.
    let tail = name.rsplit(['.', ':']).next()?;
    if tail == name {
        return None;
    }
    pick_best(map.get(tail), caller_file)
}

/// One node id for a name, or `None` when the name does not identify one.
///
/// # Why an ambiguous name resolves to nothing
///
/// This used to return `candidates[0]` — the first id registered under the
/// name — whenever the caller's own file declared no match. The comment
/// called that "deterministic", and it was: deterministically arbitrary. A
/// repo with a `parse` helper in three modules got an edge to whichever one
/// happened to be indexed first, for every cross-file caller.
///
/// The cost was not confined to genuinely ambiguous names. Because the
/// caller above falls back to a dotted name's trailing segment, every
/// `.collect()` in a Rust file looked up `collect`, and any repo that
/// happened to declare a function by that name collected an inbound edge
/// from every iterator chain in the codebase.
///
/// So: a unique name resolves, a name the caller's own file declares
/// resolves, and anything else resolves to nothing. `find_usages` returning
/// an empty result has to mean "no known caller" rather than "possibly the
/// wrong one", or none of the answers built on it can be trusted.
fn pick_best(candidates: Option<&Vec<String>>, caller_file: &str) -> Option<String> {
    let candidates = candidates?;
    match candidates.len() {
        0 => None,
        1 => Some(candidates[0].clone()),
        _ => {
            // Symbol ids encode `<kind>:<file>:<name>`, so a caller-local
            // declaration is recognisable by its file segment. Shadowing
            // makes this the right answer rather than merely a tie-break.
            let needle = format!(":{}:", caller_file);
            candidates.iter().find(|id| id.contains(&needle)).cloned()
        }
    }
}

fn dedupe_edges(edges: &mut Vec<GraphEdge>) {
    let mut seen: HashMap<(String, String, GraphEdgeType), bool> = HashMap::new();
    edges.retain(|e| {
        let key = (e.source.clone(), e.target.clone(), e.edge_type.clone());
        if seen.contains_key(&key) {
            false
        } else {
            seen.insert(key, true);
            true
        }
    });
}

fn run_k_hop_bfs(graph: &GraphData, start_node_id: &str, k: u32) -> crate::types::BfsResult {
    let (di_graph, index_map) = build_di_graph(graph);

    let start_idx = match index_map.get(start_node_id) {
        Some(idx) => *idx,
        None => {
            return crate::types::BfsResult {
                nodes: vec![],
                edges: vec![],
                distances: HashMap::new(),
            }
        }
    };

    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut queue: Vec<(NodeIndex, u32)> = vec![(start_idx, 0)];
    let mut visited: HashMap<NodeIndex, bool> = HashMap::new();

    while let Some((node_idx, dist)) = queue.pop() {
        if dist > k {
            continue;
        }
        if visited.get(&node_idx) == Some(&true) {
            continue;
        }
        visited.insert(node_idx, true);

        let node_id = graph.nodes[node_idx.index()].id.clone();
        distances.insert(node_id.clone(), dist);

        for neighbor in di_graph.neighbors(node_idx) {
            if !visited.contains_key(&neighbor) {
                queue.push((neighbor, dist + 1));
            }
        }
    }

    let result_nodes: Vec<GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| distances.contains_key(&n.id))
        .cloned()
        .collect();

    let result_edges: Vec<GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| distances.contains_key(&e.source) && distances.contains_key(&e.target))
        .cloned()
        .collect();

    crate::types::BfsResult {
        nodes: result_nodes,
        edges: result_edges,
        distances,
    }
}

pub fn build_graph(index_json: String) -> String {
    let index_result: crate::types::IndexResult = match serde_json::from_str(&index_json) {
        Ok(r) => r,
        Err(_) => return "{}".to_string(),
    };

    let graph = build_graph_from_index(&index_result);
    serde_json::to_string(&graph).unwrap_or_default()
}

pub fn k_hop_bfs(graph_json: String, start_node_id: String, k: u32) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let result = run_k_hop_bfs(&graph, &start_node_id, k);
    serde_json::to_string(&result).unwrap_or_default()
}

fn build_di_graph(graph: &GraphData) -> (DiGraph<(), ()>, HashMap<String, NodeIndex>) {
    let mut di_graph: DiGraph<(), ()> = DiGraph::new();
    let index_map: HashMap<String, NodeIndex> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), NodeIndex::new(i)))
        .collect();

    for _ in &graph.nodes {
        di_graph.add_node(());
    }

    for edge in &graph.edges {
        if let (Some(&src_idx), Some(&tgt_idx)) = (
            index_map.get(&edge.source),
            index_map.get(&edge.target),
        ) {
            di_graph.add_edge(src_idx, tgt_idx, ());
        }
    }

    (di_graph, index_map)
}

pub fn filter_edges_by_type(graph_json: String, edge_types: Vec<String>) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let filtered: Vec<GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| {
            edge_types.iter().any(|t| {
                let et_str = format!("{:?}", e.edge_type);
                et_str.to_lowercase() == t.to_lowercase()
            })
        })
        .cloned()
        .collect();

    let result = crate::types::FilteredEdgesResult {
        count: filtered.len(),
        edges: filtered,
    };

    serde_json::to_string(&result).unwrap_or_default()
}

/// Keyword-based search over graph nodes. Matches `keyword` (case-insensitive
/// substring) against each node's `name` and `docstring`. When `node_types`
/// is provided and non-empty, only nodes whose `node_type` (lowercased) is in
/// the list are considered. An empty `keyword` returns every node that passes
/// the type filter.
pub fn graph_keyword_search(
    graph_json: String,
    keyword: String,
    node_types: Option<Vec<String>>,
) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let needle = keyword.to_lowercase();
    let type_filter: Option<Vec<String>> = node_types
        .map(|v| v.into_iter().map(|t| t.to_lowercase()).collect::<Vec<_>>())
        .filter(|v| !v.is_empty());

    let matched: Vec<GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| {
            if let Some(types) = &type_filter {
                let nt = format!("{:?}", n.node_type).to_lowercase();
                if !types.contains(&nt) {
                    return false;
                }
            }

            if needle.is_empty() {
                return true;
            }

            let name_match = n.name.to_lowercase().contains(&needle);
            let doc_match = n
                .docstring
                .as_ref()
                .map(|d| d.to_lowercase().contains(&needle))
                .unwrap_or(false);

            name_match || doc_match
        })
        .cloned()
        .collect();

    let result = crate::types::SearchResult {
        count: matched.len(),
        nodes: matched,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

pub fn find_shortest_path(graph_json: String, source_id: String, target_id: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let (di_graph, index_map) = build_di_graph(&graph);

    let source_idx = match index_map.get(&source_id) {
        Some(idx) => *idx,
        None => {
            let result = crate::types::PathResult {
                path: vec![],
                found: false,
                length: None,
            };
            return serde_json::to_string(&result).unwrap_or_default();
        }
    };

    let target_idx = match index_map.get(&target_id) {
        Some(idx) => *idx,
        None => {
            let result = crate::types::PathResult {
                path: vec![],
                found: false,
                length: None,
            };
            return serde_json::to_string(&result).unwrap_or_default();
        }
    };

    let mut queue: Vec<(NodeIndex, Vec<String>)> = vec![(source_idx, vec![source_id.clone()])];
    let mut visited: HashMap<NodeIndex, bool> = HashMap::new();

    while !queue.is_empty() {
        let (node_idx, path) = queue.remove(0);
        if node_idx == target_idx {
            let path_len = path.len() as u32;
            let result = crate::types::PathResult {
                path: path.clone(),
                found: true,
                length: Some(path_len - 1),
            };
            return serde_json::to_string(&result).unwrap_or_default();
        }

        if visited.get(&node_idx) == Some(&true) {
            continue;
        }
        visited.insert(node_idx, true);

        for neighbor in di_graph.neighbors(node_idx) {
            if !visited.contains_key(&neighbor) {
                let mut new_path = path.clone();
                let neighbor_id = graph.nodes[neighbor.index()].id.clone();
                new_path.push(neighbor_id);
                queue.push((neighbor, new_path));
            }
        }
    }

    let result = crate::types::PathResult {
        path: vec![],
        found: false,
        length: None,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

/// Parse-and-score convenience wrapper over [`calculate_centrality_graph`].
///
/// Callers that already hold a `GraphData` should use that function directly —
/// on a large graph this wrapper's `from_str` is the dominant cost, and it
/// throws the parse away on return.
pub fn calculate_centrality(graph_json: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };
    calculate_centrality_graph(&graph)
}

/// Forward adjacency in compressed-sparse-row form, over node *indices*.
///
/// Brandes walks the whole adjacency once per source node, so the layout it
/// reads from is the hot structure in the whole computation: one flat
/// `targets` array read sequentially beats a `Vec<Vec<_>>` (a pointer chase
/// per node) and beats petgraph's edge list (a pointer chase per edge).
struct Csr {
    /// `offsets[i]..offsets[i + 1]` is node `i`'s slice of `targets`.
    offsets: Vec<usize>,
    targets: Vec<u32>,
}

impl Csr {
    /// Build forward adjacency, dropping edges whose endpoints aren't nodes.
    ///
    /// Parallel edges are collapsed. `build_graph` already dedupes by
    /// (source, target, type), but two *different* types between the same
    /// pair — `A Calls B` and `A Uses B` — still arrive as two edges, and to
    /// a shortest-path count those would read as two distinct routes and
    /// double σ. One hop between two nodes is one route however many
    /// relationships it stands for.
    fn build(graph: &GraphData, id_to_idx: &HashMap<&str, u32>, n: usize) -> Self {
        let mut lists: Vec<Vec<u32>> = vec![Vec::new(); n];
        for e in &graph.edges {
            if let (Some(&s), Some(&t)) = (
                id_to_idx.get(e.source.as_str()),
                id_to_idx.get(e.target.as_str()),
            ) {
                lists[s as usize].push(t);
            }
        }

        let mut offsets = Vec::with_capacity(n + 1);
        let mut targets = Vec::new();
        offsets.push(0);
        for list in lists.iter_mut() {
            list.sort_unstable();
            list.dedup();
            targets.extend_from_slice(list);
            offsets.push(targets.len());
        }

        Csr { offsets, targets }
    }

    #[inline]
    fn neighbors(&self, i: usize) -> &[u32] {
        &self.targets[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// Per-worker scratch for Brandes, allocated once and reset per source.
///
/// The reset is a `fill` (a memset) rather than `n` map insertions, and
/// `pred`'s inner vectors are `clear`ed so they keep their capacity — after
/// the first few sources the whole traversal is allocation-free.
struct BrandesScratch {
    dist: Vec<i32>,
    sigma: Vec<f64>,
    delta: Vec<f64>,
    pred: Vec<Vec<u32>>,
    /// Nodes in BFS discovery order. Popping this is what gives the
    /// non-increasing-distance order the accumulation needs, so no sort.
    stack: Vec<u32>,
    queue: VecDeque<u32>,
    /// Running betweenness for every source this worker has handled.
    betweenness: Vec<f64>,
}

impl BrandesScratch {
    fn new(n: usize) -> Self {
        BrandesScratch {
            dist: vec![-1; n],
            sigma: vec![0.0; n],
            delta: vec![0.0; n],
            pred: vec![Vec::new(); n],
            stack: Vec::with_capacity(n),
            queue: VecDeque::with_capacity(n),
            betweenness: vec![0.0; n],
        }
    }

    /// One source's contribution, accumulated into `self.betweenness`.
    fn run_source(&mut self, s: usize, csr: &Csr) {
        self.dist.fill(-1);
        self.sigma.fill(0.0);
        self.delta.fill(0.0);
        for p in self.pred.iter_mut() {
            p.clear();
        }
        self.stack.clear();
        self.queue.clear();

        self.sigma[s] = 1.0;
        self.dist[s] = 0;
        self.queue.push_back(s as u32);

        while let Some(v) = self.queue.pop_front() {
            let vi = v as usize;
            self.stack.push(v);
            let dv = self.dist[vi];
            let sigma_v = self.sigma[vi];
            for &w in csr.neighbors(vi) {
                let wi = w as usize;
                // First time seen: this is a shortest path to w by BFS order.
                if self.dist[wi] < 0 {
                    self.dist[wi] = dv + 1;
                    self.queue.push_back(w);
                }
                // Re-read `dist[wi]`, never a copy taken before the line
                // above — reading it once up front is what made the previous
                // implementation compare against a stale `-1`, so σ never
                // propagated and every betweenness score came out zero.
                if self.dist[wi] == dv + 1 {
                    self.sigma[wi] += sigma_v;
                    self.pred[wi].push(v);
                }
            }
        }

        // Dependency accumulation, in reverse BFS order. Every node on the
        // stack was reached, so σ[w] ≥ 1 and the division is safe.
        while let Some(w) = self.stack.pop() {
            let wi = w as usize;
            let coeff = (1.0 + self.delta[wi]) / self.sigma[wi];
            for &v in &self.pred[wi] {
                self.delta[v as usize] += self.sigma[v as usize] * coeff;
            }
            if wi != s {
                self.betweenness[wi] += self.delta[wi];
            }
        }
    }
}

/// Degree + Brandes betweenness centrality over an already-parsed graph.
///
/// Betweenness is directed, counts ordered pairs `(s, t)` with `s != t` and
/// neither equal to the scored node, and is normalized by `(n-1)(n-2)`.
///
/// This is O(V·E) and runs to completion on the calling thread; async callers
/// must push it onto `spawn_blocking` rather than awaiting it inline. Sources
/// are scored across rayon's pool, so it will use every core it can get.
///
/// Everything below indexes nodes by position rather than by id string. The
/// previous implementation rebuilt four `HashMap<String, _>` covering the
/// whole graph *per source node* and cloned an id on every edge relaxation —
/// O(V²) allocations before any arithmetic. See P1.1 in
/// `docs/dev/PERF-TUNING-JOURNEY.md`.
pub fn calculate_centrality_graph(graph: &GraphData) -> String {
    let n = graph.nodes.len();
    if n == 0 {
        let result = crate::types::CentralityResult {
            degree_centrality: HashMap::new(),
            betweenness_centrality: HashMap::new(),
        };
        return serde_json::to_string(&result).unwrap_or_default();
    }
    let nf = n as f64;

    // Last duplicate id wins, matching what `build_di_graph`'s `collect()`
    // has always done for a graph that somehow carries two nodes with one id.
    let mut id_to_idx: HashMap<&str, u32> = HashMap::with_capacity(n);
    for (i, node) in graph.nodes.iter().enumerate() {
        id_to_idx.insert(node.id.as_str(), i as u32);
    }

    // Degree: both endpoints of every edge whose endpoints both resolve.
    let mut degree: Vec<f64> = vec![0.0; n];
    for e in &graph.edges {
        if let Some(&s) = id_to_idx.get(e.source.as_str()) {
            degree[s as usize] += 1.0;
        }
        if let Some(&t) = id_to_idx.get(e.target.as_str()) {
            degree[t as usize] += 1.0;
        }
    }
    if n > 1 {
        for d in degree.iter_mut() {
            *d /= nf - 1.0;
        }
    }

    let mut betweenness: Vec<f64> = vec![0.0; n];
    if n > 1 {
        let csr = Csr::build(graph, &id_to_idx, n);

        // One scratch buffer per worker, reused across every source that
        // worker handles, so the O(V)-sized allocations happen `threads`
        // times rather than once per source.
        //
        // Partitioned by stride rather than by contiguous block, and *not*
        // through `par_iter().fold()`: fold builds one accumulator per split
        // chunk, and rayon chooses how many of those to make, so a scratch
        // that costs ~8 MB at 162k nodes would be allocated an unbounded
        // number of times. Striding fixes the count at `threads` while still
        // spreading the expensive sources — the ones inside a large connected
        // component — evenly, which a contiguous split would pile onto
        // whichever worker drew that range.
        let threads = rayon::current_num_threads().max(1);
        betweenness = (0..threads)
            .into_par_iter()
            .map(|t| {
                let mut scratch = BrandesScratch::new(n);
                let mut s = t;
                while s < n {
                    scratch.run_source(s, &csr);
                    s += threads;
                }
                scratch.betweenness
            })
            .reduce(
                || vec![0.0; n],
                |mut acc, part| {
                    for (a, b) in acc.iter_mut().zip(part) {
                        *a += b;
                    }
                    acc
                },
            );

        let normalizer = (nf - 1.0) * (nf - 2.0);
        if normalizer > 0.0 {
            for b in betweenness.iter_mut() {
                *b /= normalizer;
            }
        }
    }

    // Back to id-keyed maps for the wire format. Duplicate ids collapse with
    // the last one winning, as they did before.
    let mut degree_centrality: HashMap<String, f64> = HashMap::with_capacity(n);
    let mut betweenness_centrality: HashMap<String, f64> = HashMap::with_capacity(n);
    for (i, node) in graph.nodes.iter().enumerate() {
        degree_centrality.insert(node.id.clone(), degree[i]);
        betweenness_centrality.insert(node.id.clone(), betweenness[i]);
    }

    let result = crate::types::CentralityResult {
        degree_centrality,
        betweenness_centrality,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

/// Parse-and-detect convenience wrapper over [`detect_cycles_graph`]. See the
/// note on [`calculate_centrality`] about the cost of the parse.
pub fn detect_cycles(graph_json: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };
    detect_cycles_graph(&graph)
}

/// Cycle detection over an already-parsed graph. Like
/// [`calculate_centrality_graph`], this is CPU-bound and must not be awaited
/// inline on an async runtime thread.
pub fn detect_cycles_graph(graph: &GraphData) -> String {
    let (di_graph, index_map) = build_di_graph(graph);
    let mut visited: HashMap<String, bool> = HashMap::new();
    let mut rec_stack: HashMap<String, bool> = HashMap::new();
    let mut cycles: Vec<Vec<String>> = vec![];

    for node in &graph.nodes {
        if !visited.contains_key(&node.id) {
            detect_cycles_dfs(
                &di_graph,
                &graph.nodes,
                &index_map,
                &node.id,
                &mut visited,
                &mut rec_stack,
                &mut vec![],
                &mut cycles,
            );
        }
    }

    let unique_cycles: Vec<Vec<String>> = cycles
        .into_iter()
        .map(|mut c| { c.sort(); c })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let result = crate::types::CycleResult {
        has_cycles: !unique_cycles.is_empty(),
        cycles: unique_cycles,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

fn detect_cycles_dfs(
    di_graph: &DiGraph<(), ()>,
    nodes: &[GraphNode],
    index_map: &HashMap<String, NodeIndex>,
    node_id: &str,
    visited: &mut HashMap<String, bool>,
    rec_stack: &mut HashMap<String, bool>,
    path: &mut Vec<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    visited.insert(node_id.to_string(), true);
    rec_stack.insert(node_id.to_string(), true);
    path.push(node_id.to_string());

    if let Some(&idx) = index_map.get(node_id) {
        for neighbor_idx in di_graph.neighbors(idx) {
            let neighbor_id = nodes[neighbor_idx.index()].id.clone();

            if !visited.contains_key(&neighbor_id) {
                detect_cycles_dfs(
                    di_graph, nodes, index_map,
                    &neighbor_id, visited, rec_stack, path, cycles,
                );
            } else if rec_stack.get(&neighbor_id) == Some(&true) {
                let mut cycle = vec![];
                let start_pos = path.iter().position(|n| n == &neighbor_id).unwrap();
                for (i, n) in path.iter().enumerate() {
                    if i >= start_pos {
                        cycle.push(n.clone());
                    }
                }
                cycle.push(neighbor_id.clone());
                cycles.push(cycle);
            }
        }
    }

    path.pop();
    rec_stack.insert(node_id.to_string(), false);
}
#[allow(dead_code)]
fn __bench_probe_1() -> u32 { 1 }

#[allow(dead_code)]
fn __bench_probe_2() -> u32 { 2 }

#[allow(dead_code)]
fn __bench_probe_3() -> u32 { 3 }

#[allow(dead_code)]
fn __bench_probe_4() -> u32 { 4 }

#[allow(dead_code)]
fn __bench_probe_5() -> u32 { 5 }

#[allow(dead_code)]
fn __bench_probe_6() -> u32 { 6 }

#[allow(dead_code)]
fn __bench_probe_7() -> u32 { 7 }

#[allow(dead_code)]
fn __bench_probe_8() -> u32 { 8 }

#[allow(dead_code)]
fn __bench_probe_9() -> u32 { 9 }
