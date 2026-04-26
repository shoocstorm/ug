//! Indexer entry-point and orchestration.
//!
//! This file is intentionally small. It owns the public NAPI exports
//! (`index`, `index_with_cache`) and the per-file pipeline that ties
//! together:
//!
//! - file discovery (`common::scan_files`)
//! - tree-sitter parsing (using the grammar from the registered
//!   `LanguageIndexer` for the file's extension)
//! - symbol / import / export extraction (delegated to the indexer)
//! - file classification (`classifier::classify_file`)
//! - cache key computation (`common::compute_hash`)
//! - dependency extraction from `package.json` (`package_json::…`)
//!
//! Adding a new language is purely additive - see `languages.rs`.

mod classifier;
mod common;
mod languages;
mod package_json;

use crate::types::{FileNode, IndexResult, IndexStats};
use napi_derive::napi;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tree_sitter::Parser;

use classifier::classify_file;
pub use common::{normalize_path, resolve_relative};
use common::{compute_hash, resolve_import_refs, scan_files};
use package_json::extract_package_json_dependencies;

/// Parse a single source file end-to-end and return the resulting
/// [`FileNode`]. Returns `None` for unsupported extensions, unreadable files,
/// or content that tree-sitter fails to parse.
pub fn process_file(path: &Path) -> Option<FileNode> {
    let ext = path.extension()?.to_str()?;
    let indexer = languages::for_extension(ext)?;

    let content = fs::read_to_string(path).ok()?;
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    let mut parser = Parser::new();
    parser.set_language(indexer.tree_sitter_language()).ok()?;
    let tree = parser.parse(content.as_bytes(), None)?;
    let root = tree.root_node();
    let source = content.as_bytes();

    let imports = indexer.extract_imports(source, root);
    let exports = indexer.extract_exports(source, root);
    let mut symbols = indexer.extract_symbols(source, root);

    // Stamp the file path onto every symbol now that it's known. Doing this
    // here keeps each language indexer focused on AST extraction and unaware
    // of where the file lives on disk. The path is normalized so downstream
    // ID derivation is stable regardless of how the user invoked the CLI
    // (`./src/foo.ts` and `src/foo.ts` collapse to the same key).
    let path_str = normalize_path(&path.to_string_lossy());
    for sym in symbols.iter_mut() {
        sym.file = path_str.clone();
    }

    resolve_import_refs(&mut symbols, &imports);
    let classification = classify_file(&path_str, &symbols);

    Some(FileNode {
        path: path_str,
        hash,
        language: indexer.name().to_string(),
        classification,
        symbols,
        imports,
        exports,
    })
}

/// Index every supported source file under `path`. Returns a JSON-encoded
/// [`IndexResult`].
#[napi]
pub fn index(path: String) -> String {
    let start = std::time::Instant::now();
    let files_paths = scan_files(&path);
    let dependencies = extract_package_json_dependencies(&path);

    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;

    for file_path in files_paths {
        if let Some(file_node) = process_file(&file_path) {
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

    serde_json::to_string(&IndexResult {
        files,
        dependencies,
        stats,
    })
    .unwrap_or_default()
}

/// Index every supported source file under `path`, skipping files whose
/// blake3 hash matches the value stored in `<cache_path>/cache.json` from a
/// previous run. The cache file is rewritten with the latest hashes once
/// indexing is complete.
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
    let dependencies = extract_package_json_dependencies(&path);
    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;
    let mut cached = 0;

    for file_path in files_paths {
        // Normalize so the cache key matches what `process_file` stamps onto
        // the FileNode. Without this, a cache built from `./src/foo.ts` would
        // miss a path stored as `src/foo.ts` and re-index every run.
        let path_str = normalize_path(&file_path.to_string_lossy());
        let hash = match compute_hash(&file_path) {
            Some(h) => h,
            None => continue,
        };

        // Cache hit: skip parsing entirely. We deliberately don't push a
        // FileNode for cached files - callers merge with the previous run's
        // output if they need a complete view.
        if cached_hashes.get(&path_str) == Some(&hash) {
            cached += 1;
            continue;
        }

        if let Some(mut file_node) = process_file(&file_path) {
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

    serde_json::to_string(&IndexResult {
        files,
        dependencies,
        stats,
    })
    .unwrap_or_default()
}
