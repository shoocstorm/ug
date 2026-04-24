use ignore::WalkBuilder;
use napi_derive::napi;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tree_sitter::Parser;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(rename = "startLine")]
    pub start_line: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    pub docstring: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub hash: String,
    pub language: String,
    pub symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    pub files: Vec<FileNode>,
    pub stats: IndexStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    #[serde(rename = "totalFiles")]
    pub total_files: usize,
    #[serde(rename = "cachedFiles")]
    pub cached_files: usize,
    #[serde(rename = "totalSymbols")]
    pub total_symbols: usize,
    #[serde(rename = "indexingTimeMs")]
    pub indexing_time_ms: u64,
}

fn get_language_for_ext(ext: &str) -> Option<(tree_sitter::Language, &'static str)> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" => {
            Some((tree_sitter_typescript::language_typescript(), "typescript"))
        }
        "py" => Some((tree_sitter_python::language(), "python")),
        _ => None,
    }
}

fn process_file(path: &Path) -> Option<FileNode> {
    let ext = path.extension()?.to_str()?;
    let (lang, lang_name) = get_language_for_ext(ext)?;
    
    let content = fs::read_to_string(path).ok()?;
    let hash_data = fs::read(path).ok()?;
    let hash = blake3::hash(&hash_data).to_hex().to_string();
    
    let mut parser = Parser::new();
    parser.set_language(lang).ok()?;
    
    let tree = parser.parse(content.as_bytes(), None)?;
    let mut symbols = Vec::new();
    extract_symbols(&mut symbols, tree.root_node(), content.as_bytes(), lang_name);
    
    let path_str = path.to_string_lossy().to_string();
    for sym in symbols.iter_mut() {
        sym.file = path_str.clone();
    }
    
    Some(FileNode {
        path: path_str,
        hash,
        language: lang_name.to_string(),
        symbols,
    })
}

fn extract_symbols(symbols: &mut Vec<Symbol>, node: tree_sitter::Node, source: &[u8], language: &str) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;
    
    if language == "typescript" {
        if kind == "function_declaration" || kind == "method_definition" {
            if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                symbols.push(Symbol {
                    id: format!("fn:{}:{}", start, name),
                    name,
                    kind: kind.to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: None,
                });
            }
        } else if kind == "class_declaration" {
            if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                symbols.push(Symbol {
                    id: format!("class:{}:{}", start, name),
                    name,
                    kind: "class".to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: None,
                });
            }
        } else if kind == "interface_declaration" {
            if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                symbols.push(Symbol {
                    id: format!("interface:{}:{}", start, name),
                    name,
                    kind: "interface".to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: None,
                });
            }
        }
    } else if language == "python" {
        if kind == "function_definition" {
            if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                symbols.push(Symbol {
                    id: format!("fn:{}:{}", start, name),
                    name,
                    kind: "function".to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: None,
                });
            }
        } else if kind == "class_definition" {
            if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                symbols.push(Symbol {
                    id: format!("class:{}:{}", start, name),
                    name,
                    kind: "class".to_string(),
                    file: String::new(),
                    start_line: start,
                    end_line: end,
                    docstring: None,
                });
            }
        }
    }
    
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_symbols(symbols, child, source, language);
    }
}

fn get_node_text(node: Option<tree_sitter::Node>, source: &[u8]) -> Option<String> {
    let node = node?;
    let start = node.start_byte();
    let end = node.end_byte();
    if start < end {
        String::from_utf8(source[start..end].to_vec()).ok()
    } else {
        None
    }
}

#[napi]
pub fn index(path: String) -> String {
    let start = std::time::Instant::now();
    
    let walker = WalkBuilder::new(&path)
        .hidden(true)
        .git_ignore(true)
        .build();
    
    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;
    
    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = match path.extension() {
            Some(e) => e.to_str().unwrap_or(""),
            None => "",
        };
        if !["ts", "tsx", "js", "jsx", "py"].contains(&ext) {
            continue;
        }
        if path.to_string_lossy().contains("node_modules") || 
           path.to_string_lossy().contains(".git") ||
           path.to_string_lossy().contains("target") {
            continue;
        }
        
        if let Some(file_node) = process_file(path) {
            total_symbols += file_node.symbols.len();
            files.push(file_node);
        }
    }
    
    let elapsed = start.elapsed();
    let stats = IndexStats {
        total_files: files.len(),
        cached_files: 0,
        total_symbols,
        indexing_time_ms: elapsed.as_millis() as u64,
    };
    
    let result = IndexResult { files, stats };
    serde_json::to_string(&result).unwrap_or_default()
}

#[napi]
pub fn index_with_cache(path: String, cache_path: String) -> String {
    let start = std::time::Instant::now();
    
    let cache_file = Path::new(&cache_path).join("cache.json");
    let mut cached_hashes: HashMap<String, String> = HashMap::new();
    
    if cache_file.exists() {
        if let Ok(content) = fs::read_to_string(&cache_file) {
            if let Ok(hashes) = serde_json::from_str(&content) {
                cached_hashes = hashes;
            }
        }
    }
    
    let walker = WalkBuilder::new(&path)
        .hidden(true)
        .git_ignore(true)
        .build();
    
    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;
    let mut cached = 0;
    
    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = match path.extension() {
            Some(e) => e.to_str().unwrap_or(""),
            None => "",
        };
        if !["ts", "tsx", "js", "jsx", "py"].contains(&ext) {
            continue;
        }
        if path.to_string_lossy().contains("node_modules") || 
           path.to_string_lossy().contains(".git") ||
           path.to_string_lossy().contains("target") {
            continue;
        }
        
        let path_str = path.to_string_lossy().to_string();
        
        let hash_data = match fs::read(path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let hash = blake3::hash(&hash_data).to_hex().to_string();
        
        if let Some(cached_hash) = cached_hashes.get(&path_str) {
            if cached_hash == &hash {
                cached += 1;
                continue;
            }
        }
        
        if let Some(mut file_node) = process_file(path) {
            total_symbols += file_node.symbols.len();
            file_node.hash = hash.clone();
            files.push(file_node);
            cached_hashes.insert(path_str, hash);
        }
    }
    
    if let Ok(json) = serde_json::to_string(&cached_hashes) {
        let _ = fs::create_dir_all(&cache_path);
        let _ = fs::write(&cache_file, json);
    }
    
    let elapsed = start.elapsed();
    let stats = IndexStats {
        total_files: files.len(),
        cached_files: cached,
        total_symbols,
        indexing_time_ms: elapsed.as_millis() as u64,
    };
    
    let result = IndexResult { files, stats };
    serde_json::to_string(&result).unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GraphNodeType {
    File,
    Function,
    Class,
    Interface,
    Concept,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GraphEdgeType {
    DependsOn,
    Calls,
    Extends,
    References,
    Contains,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub node_type: GraphNodeType,
    pub file: Option<String>,
    #[serde(rename = "startLine")]
    pub start_line: Option<u32>,
    #[serde(rename = "endLine")]
    pub end_line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub edge_type: GraphEdgeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BfsResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub distances: HashMap<String, u32>,
}

fn build_graph_from_index(index_result: &IndexResult) -> GraphData {
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    
    for file in &index_result.files {
        let file_node_id = format!("file:{}", file.path);
        
        nodes.push(GraphNode {
            id: file_node_id.clone(),
            name: file.path.clone(),
            node_type: GraphNodeType::File,
            file: Some(file.path.clone()),
            start_line: None,
            end_line: None,
        });
        
        for symbol in &file.symbols {
            let node_type = match symbol.kind.as_str() {
                "function" | "function_declaration" | "method_definition" => GraphNodeType::Function,
                "class" | "class_declaration" => GraphNodeType::Class,
                "interface" | "interface_declaration" => GraphNodeType::Interface,
                _ => GraphNodeType::Function,
            };
            
            let sym_node_id = format!("{}:{}", symbol.kind, symbol.name);
            
            nodes.push(GraphNode {
                id: sym_node_id.clone(),
                name: symbol.name.clone(),
                node_type,
                file: Some(file.path.clone()),
                start_line: Some(symbol.start_line),
                end_line: Some(symbol.end_line),
            });
            
            edges.push(GraphEdge {
                source: file_node_id.clone(),
                target: sym_node_id,
                edge_type: GraphEdgeType::Contains,
            });
        }
    }
    
    GraphData { nodes, edges }
}

fn run_k_hop_bfs(graph: &GraphData, start_node_id: &str, k: u32) -> BfsResult {
    let mut di_graph: DiGraph<(), ()> = DiGraph::new();
    
    let index_map: HashMap<String, NodeIndex> = graph.nodes.iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), NodeIndex::new(i)))
        .collect();
    
    for _ in &graph.nodes {
        di_graph.add_node(());
    }
    
    for edge in &graph.edges {
        if let (Some(&src_idx), Some(&tgt_idx)) = (index_map.get(&edge.source), index_map.get(&edge.target)) {
            di_graph.add_edge(src_idx, tgt_idx, ());
        }
    }
    
    let start_idx = match index_map.get(start_node_id) {
        Some(idx) => *idx,
        None => return BfsResult {
            nodes: vec![],
            edges: vec![],
            distances: HashMap::new(),
        },
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
    
    let result_nodes: Vec<GraphNode> = graph.nodes.iter()
        .filter(|n| distances.contains_key(&n.id))
        .cloned()
        .collect();
    
    let result_edges: Vec<GraphEdge> = graph.edges.iter()
        .filter(|e| distances.contains_key(&e.source) && distances.contains_key(&e.target))
        .cloned()
        .collect();
    
    BfsResult {
        nodes: result_nodes,
        edges: result_edges,
        distances,
    }
}

#[napi]
pub fn build_graph(index_json: String) -> String {
    let index_result: IndexResult = match serde_json::from_str(&index_json) {
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