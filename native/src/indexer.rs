use crate::types::{FileNode, IndexResult, IndexStats, Symbol};
use ignore::WalkBuilder;
use napi_derive::napi;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tree_sitter::Parser;

const SUPPORTED_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "py"];
const IGNORED_DIRS: &[&str] = &["node_modules", ".git", "target"];

fn get_language_for_ext(ext: &str) -> Option<(tree_sitter::Language, &'static str)> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" => {
            Some((tree_sitter_typescript::language_typescript(), "typescript"))
        }
        "py" => Some((tree_sitter_python::language(), "python")),
        _ => None,
    }
}

pub fn process_file(path: &Path) -> Option<FileNode> {
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
        match kind {
            "function_declaration" | "method_definition" => {
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
            }
            "class_declaration" => {
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
            "interface_declaration" => {
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
            _ => {}
        }
    } else if language == "python" {
        match kind {
            "function_definition" => {
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
            }
            "class_definition" => {
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
            _ => {}
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

fn is_supported_file(path: &Path) -> bool {
    let ext = match path.extension() {
        Some(e) => e.to_str().unwrap_or(""),
        None => "",
    };
    SUPPORTED_EXTS.contains(&ext)
}

fn is_ignored_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    IGNORED_DIRS.iter().any(|&d| path_str.contains(d))
}

fn scan_files(path: &str) -> Vec<std::path::PathBuf> {
    let walker = WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .build();

    walker
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && is_supported_file(e.path()) && !is_ignored_path(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

fn compute_hash(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    Some(blake3::hash(&data).to_hex().to_string())
}

#[napi]
pub fn index(path: String) -> String {
    let start = std::time::Instant::now();
    let files_paths = scan_files(&path);

    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;

    for path in files_paths {
        if let Some(file_node) = process_file(&path) {
            total_symbols += file_node.symbols.len();
            files.push(file_node);
        }
    }

    let stats = IndexStats {
        total_files: files.len(),
        cached_files: 0,
        total_symbols,
        indexing_time_ms: start.elapsed().as_millis() as u64,
    };

    serde_json::to_string(&IndexResult { files, stats }).unwrap_or_default()
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

    let files_paths = scan_files(&path);
    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;
    let mut cached = 0;

    for path in files_paths {
        let path_str = path.to_string_lossy().to_string();
        let hash = match compute_hash(&path) {
            Some(h) => h,
            None => continue,
        };

        if cached_hashes.get(&path_str) == Some(&hash) {
            cached += 1;
            continue;
        }

        if let Some(mut file_node) = process_file(&path) {
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

    let stats = IndexStats {
        total_files: files.len(),
        cached_files: cached,
        total_symbols,
        indexing_time_ms: start.elapsed().as_millis() as u64,
    };

    serde_json::to_string(&IndexResult { files, stats }).unwrap_or_default()
}