use crate::indexer::{normalize_path, resolve_relative};
use crate::types::{
    GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeFolderMeta, GraphNodeType,
};
use napi_derive::napi;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

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

fn build_graph_from_index(index_result: &crate::types::IndexResult) -> GraphData {
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    // Same name can exist in many files (e.g. helper `parse` in three
    // modules). Keep every match so cross-file resolvers can prefer the
    // one in the same file as the caller.
    let mut symbol_id_map: HashMap<String, Vec<String>> = HashMap::new();

    let (path_index, basename_index) = build_file_indexes(&index_result.files);

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
        });

        // Stack of (heading_level, sym_node_id) maintained per file while
        // walking symbols in source order. Used to resolve the parent of
        // each markdown heading: pop any heading on top whose level is
        // greater-or-equal to the current one, and the stack's new top is
        // the parent (the file node when the stack is empty). Non-markdown
        // files never push to the stack.
        let mut heading_stack: Vec<(usize, String)> = Vec::new();

        for sym in &file.symbols {
            let heading_level = parse_heading_level(&sym.kind);

            let node_type = if heading_level.is_some() {
                GraphNodeType::Concept
            } else {
                match sym.kind.as_str() {
                    "function" | "function_declaration" | "method_definition" => GraphNodeType::Function,
                    "class" | "class_declaration" => GraphNodeType::Class,
                    "interface" | "interface_declaration" => GraphNodeType::Interface,
                    "variable" | "variable_declaration" => GraphNodeType::Function,
                    "type" | "type_alias_declaration" => GraphNodeType::Interface,
                    _ => GraphNodeType::Function,
                }
            };

            // Symbol IDs are scoped by `<file>:<line>:<name>` so two files
            // declaring the same `class Foo` get distinct nodes. Headings
            // use a slimmer `<file>:<line>` key since markdown headings carry
            // no kind variation worth disambiguating on.
            let sym_node_id = if heading_level.is_some() {
                format!("heading:{}:{}", normalized_file_path, sym.start_line)
            } else {
                format!(
                    "{}:{}:{}:{}",
                    sym.kind, normalized_file_path, sym.start_line, sym.name
                )
            };

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
            });

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

                edges.push(GraphEdge {
                    source: file_node_id.clone(),
                    target: sym_node_id.clone(),
                    edge_type: GraphEdgeType::Contains,
                });
            }
        }
    }

    // Pass 2: resolve calls/extends/implements through symbol_id_map. Names
    // like `this.foo` or `obj.foo` fall back to the trailing segment so
    // member-access calls hit the right method node. When a name has matches
    // in multiple files we prefer the one in the same file as the caller.
    for file in &index_result.files {
        let normalized_file_path = normalize_path(&file.path);
        for sym in &file.symbols {
            if parse_heading_level(&sym.kind).is_some() {
                continue;
            }
            let sym_node_id = format!(
                "{}:{}:{}:{}",
                sym.kind, normalized_file_path, sym.start_line, sym.name
            );

            for extended in &sym.extends {
                if let Some(target_id) = resolve_symbol(&symbol_id_map, extended, &normalized_file_path) {
                    edges.push(GraphEdge {
                        source: sym_node_id.clone(),
                        target: target_id,
                        edge_type: GraphEdgeType::Extends,
                    });
                }
            }

            for implemented in &sym.implements {
                if let Some(target_id) = resolve_symbol(&symbol_id_map, implemented, &normalized_file_path) {
                    edges.push(GraphEdge {
                        source: sym_node_id.clone(),
                        target: target_id,
                        edge_type: GraphEdgeType::Implements,
                    });
                }
            }

            for called in &sym.calls {
                if let Some(target_id) = resolve_symbol(&symbol_id_map, called, &normalized_file_path) {
                    edges.push(GraphEdge {
                        source: sym_node_id.clone(),
                        target: target_id,
                        edge_type: GraphEdgeType::Calls,
                    });
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
                }
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
    }
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

/// Look up a symbol by name, preferring matches in the caller's own file.
///
/// Names like `this.foo` or `obj.bar.baz` fall back to the trailing
/// identifier so member-access call expressions resolve to the method node
/// when only the bare method name is in the map. When several files declare
/// the same name (a `parse` helper in three modules), we pick the match in
/// `caller_file` if there is one — otherwise the first registered ID, which
/// keeps the graph deterministic for the same input.
fn resolve_symbol(
    map: &HashMap<String, Vec<String>>,
    name: &str,
    caller_file: &str,
) -> Option<String> {
    if let Some(id) = pick_best(map.get(name), caller_file) {
        return Some(id);
    }
    let tail = name.rsplit('.').next()?;
    if tail == name {
        return None;
    }
    pick_best(map.get(tail), caller_file)
}

fn pick_best(candidates: Option<&Vec<String>>, caller_file: &str) -> Option<String> {
    let candidates = candidates?;
    if candidates.is_empty() {
        return None;
    }
    // Symbol IDs encode `<kind>:<file>:<line>:<name>` - prefer an ID whose
    // `<file>` segment matches the caller. Falls through to the first
    // registered entry so behaviour is stable when there's no same-file
    // match (cross-file calls, externals).
    let needle = format!(":{}:", caller_file);
    for id in candidates {
        if id.contains(&needle) {
            return Some(id.clone());
        }
    }
    Some(candidates[0].clone())
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

#[napi]
pub fn build_graph(index_json: String) -> String {
    let index_result: crate::types::IndexResult = match serde_json::from_str(&index_json) {
        Ok(r) => r,
        Err(_) => return "{}".to_string(),
    };

    let graph = build_graph_from_index(&index_result);
    serde_json::to_string(&graph).unwrap_or_default()
}

#[napi]
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

#[napi]
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
#[napi]
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

#[napi]
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

#[napi]
pub fn calculate_centrality(graph_json: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let n = graph.nodes.len() as f64;
    if n == 0.0 {
        let result = crate::types::CentralityResult {
            degree_centrality: HashMap::new(),
            betweenness_centrality: HashMap::new(),
        };
        return serde_json::to_string(&result).unwrap_or_default();
    }

    let mut degree_centrality: HashMap<String, f64> = HashMap::new();
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut out_degree: HashMap<String, usize> = HashMap::new();

    for node in &graph.nodes {
        degree_centrality.insert(node.id.clone(), 0.0);
        in_degree.insert(node.id.clone(), 0);
        out_degree.insert(node.id.clone(), 0);
    }

    for edge in &graph.edges {
        if let Some(c) = degree_centrality.get_mut(&edge.source) {
            *c += 1.0;
        }
        if let Some(c) = degree_centrality.get_mut(&edge.target) {
            *c += 1.0;
        }
        if let Some(c) = out_degree.get_mut(&edge.source) {
            *c += 1;
        }
        if let Some(c) = in_degree.get_mut(&edge.target) {
            *c += 1;
        }
    }

    for (_, c) in &mut degree_centrality {
        if n > 1.0 {
            *c /= n - 1.0;
        }
    }

    let mut betweenness: HashMap<String, f64> = HashMap::new();
    for node in &graph.nodes {
        betweenness.insert(node.id.clone(), 0.0);
    }

    if n > 1.0 {
    let (di_graph, index_map) = build_di_graph(&graph);

    for node in &graph.nodes {
        let mut pred: HashMap<String, Vec<String>> = HashMap::new();
        let mut dist: HashMap<String, i32> = HashMap::new();
        let mut sigma: HashMap<String, usize> = HashMap::new();
        let mut delta: HashMap<String, f64> = HashMap::new();

        for n in &graph.nodes {
            pred.insert(n.id.clone(), vec![]);
            dist.insert(n.id.clone(), -1);
            sigma.insert(n.id.clone(), 0);
            delta.insert(n.id.clone(), 0.0);
        }
        sigma.insert(node.id.clone(), 1);
        dist.insert(node.id.clone(), 0);

        let source_idx = *index_map.get(&node.id).unwrap();
        let mut queue: Vec<NodeIndex> = vec![source_idx];

        while !queue.is_empty() {
            let v_idx = queue.remove(0);
            let v_id = graph.nodes[v_idx.index()].id.clone();
            let v_dist = *dist.get(&v_id).unwrap();

            for w_idx in di_graph.neighbors(v_idx) {
                let w_id = graph.nodes[w_idx.index()].id.clone();
                let w_dist = *dist.get(&w_id).unwrap();

                if w_dist == -1 {
                    *dist.get_mut(&w_id).unwrap() = v_dist + 1;
                    queue.push(w_idx);
                }

                if v_dist + 1 == w_dist {
                    let sigma_v = *sigma.get(&v_id).unwrap();
                    *sigma.get_mut(&w_id).unwrap() += sigma_v;
                    pred.get_mut(&w_id).unwrap().push(v_id.clone());
                }
            }
        }

        let node_ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
        let mut ordered: Vec<String> = node_ids.iter()
            .filter(|id| *dist.get(*id).unwrap() > 0)
            .cloned()
            .collect();
        ordered.sort_by(|a, b| {
            dist.get(b).unwrap().cmp(dist.get(a).unwrap())
        });

        for w in &ordered {
            for v in pred.get(w).unwrap_or(&vec![]) {
                let sigma_v = *sigma.get(v).unwrap() as f64;
                let sigma_w = *sigma.get(w).unwrap() as f64;
                let delta_v = *delta.get(v).unwrap();
                if sigma_w > 0.0 {
                    let contribution = (sigma_v / sigma_w) * (1.0 + delta_v);
                    *delta.get_mut(w).unwrap() += contribution;
                }
            }
            if w != &node.id {
                *betweenness.get_mut(w).unwrap() += delta.get(w).unwrap();
            }
        }
    }

    let normalizer = (n - 1.0) * (n - 2.0);
    if normalizer > 0.0 {
        for (_, c) in &mut betweenness {
            *c /= normalizer;
        }
    }
    }

    let result = crate::types::CentralityResult {
        degree_centrality,
        betweenness_centrality: betweenness,
    };
    serde_json::to_string(&result).unwrap_or_default()
}

#[napi]
pub fn detect_cycles(graph_json: String) -> String {
    let graph: GraphData = match serde_json::from_str(&graph_json) {
        Ok(g) => g,
        Err(_) => return "{}".to_string(),
    };

    let (di_graph, index_map) = build_di_graph(&graph);
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