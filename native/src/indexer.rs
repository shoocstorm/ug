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
//! - line-metric annotation (`line_metrics`), shared by every language
//! - file classification (`classifier::classify_file`)
//! - cache key computation (`common::compute_hash`)
//! - dependency extraction from `package.json` (`package_json::…`)
//!
//! Adding a new language is purely additive - see `languages.rs`.

pub(crate) mod boundary;
mod classifier;
pub(crate) mod common;
pub(crate) mod document;
mod folder;
pub(crate) mod languages;
pub(crate) mod line_metrics;
mod package_json;
pub(crate) mod scope;

use crate::types::{FileNode, IndexResult, IndexStats, Symbol, SymbolMetrics};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tree_sitter::Parser;

/// Extraction-format version, stored in the cache under
/// [`CACHE_VERSION_KEY`].
///
/// The incremental cache keys on file *content*, which is exactly right for
/// detecting edits and exactly wrong for detecting a change to the indexer:
/// after an extractor gains a capability, every unchanged file is a cache
/// hit and keeps its old, poorer symbols forever. Bumping this discards the
/// cache once, so the improvement actually reaches an existing install.
///
/// Bump on any change to what the extractors *produce*.
///
/// 3: comment/doc/code line metrics on every symbol, metrics on types that
/// previously had none, and an inclusive `loc`. Without this bump an
/// existing install would keep every unchanged file's cached symbols and
/// the new metrics would appear only on files someone happened to edit —
/// producing a repo-wide statistic computed over an arbitrary subset.
///
/// 4: qualified names, owners and typed call sites for Rust, TypeScript and
/// Python — the facts cross-file call resolution is built on. A cached
/// symbol from version 3 has none of them, and the graph builder reads their
/// absence as "this language reports bare names only", so a stale cache
/// would silently keep a repo on the old name-matching path.
///
/// 5: system boundaries on every symbol, and declaration-site annotations for
/// Python, TypeScript and Rust. Boundaries are derived from those annotations
/// and from call sites, so a cached symbol from version 4 carries neither the
/// input nor the output — it would simply look like a file with no entry
/// points, which is indistinguishable from the truth for most files and
/// therefore impossible to notice.
///
/// 6: Rust `mod` declarations bind their child module, so a `cli::run()`
/// written in the file that declares `mod cli;` resolves to `crate::cli::run`
/// instead of the literal `cli::run`, which matched no declaration. The
/// resolved path is stored *in* the cached `CallRef`, so without this bump an
/// existing install keeps the unresolved one for every file it does not
/// happen to re-parse — and the resulting call graph would be repaired only
/// in the files someone edited.
const INDEXER_VERSION: &str = "6";

/// Reserved key in `cache.json`. Prefixed and suffixed so it cannot collide
/// with a repo-relative path.
const CACHE_VERSION_KEY: &str = "__ug_indexer_version__";

use classifier::classify_file;
pub use common::{normalize_path, resolve_relative};
use common::{compute_hash, resolve_import_refs, scan_files};
use package_json::extract_package_json_dependencies;

/// Parse a single source file end-to-end and return the resulting
/// [`FileNode`]. Returns `None` for unsupported extensions, unreadable files,
/// or content that tree-sitter fails to parse.
///
/// If `repo_root` is provided, file paths will be made relative to it
/// to reduce output size.
pub fn process_file(path: &Path, repo_root: Option<&str>) -> Option<FileNode> {
    let ext = path.extension()?.to_str()?.to_lowercase();

    // PDF/Word/Excel/PowerPoint are binary and have no tree-sitter grammar —
    // short-circuit to the dedicated extractor, which returns the same
    // FileNode shape as the language pipeline below.
    if document::is_supported_ext(&ext) {
        return document::process_document(path, repo_root);
    }

    let indexer = languages::for_extension(&ext)?;

    let content = fs::read_to_string(path).ok()?;
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    let mut parser = Parser::new();
    parser.set_language(indexer.tree_sitter_language()).ok()?;
    let tree = parser.parse(content.as_bytes(), None)?;
    let root = tree.root_node();
    let source = content.as_bytes();

    // The repo-relative path is computed *before* extraction because
    // qualified names are derived from it: `crate::storage::db::Db#open` is
    // only knowable to an extractor that can see where its file sits. It is
    // still stamped onto each symbol afterwards, so extractors that don't
    // care (Java, Markdown) stay unaware of the filesystem.
    let path_str = normalize_path(&path.to_string_lossy());
    let path_str = match repo_root {
        Some(root) => common::strip_repo_root(&path_str, root),
        None => path_str,
    };

    let imports = indexer.extract_imports(source, root);
    let exports = indexer.extract_exports(source, root);
    let ctx = languages::FileContext {
        path: &path_str,
        imports: &imports,
    };
    let mut symbols = indexer.extract_symbols(source, root, &ctx);

    for sym in symbols.iter_mut() {
        sym.file = path_str.clone();
    }

    resolve_import_refs(&mut symbols, &imports);
    // One pass over the file's lines for every symbol in it — see
    // `line_metrics` for why this is here and not in the five extractors.
    annotate_line_metrics(&mut symbols, &content, indexer.name());
    // Likewise a post-pass: a boundary rule reads a symbol's annotations, its
    // declaring type's annotations and its call sites, all of which exist
    // only once the whole file has been extracted.
    boundary::annotate(indexer.name(), &mut symbols);
    let classification = classify_file(&path_str, &symbols);

    Some(FileNode {
        path: path_str,
        hash,
        language: indexer.name().to_string(),
        classification,
        symbols,
        lines: content.lines().count() as u32,
        imports,
        exports,
    })
}

/// Fill the comment/doc/code line counts on every symbol in one file.
///
/// Runs after extraction rather than inside it so all five languages get
/// the same definition of "a comment", and so the file's lines are
/// classified once instead of once per symbol.
///
/// A symbol with no `metrics` gets one: Class and Interface nodes carry no
/// metrics from most extractors, and leaving them out here would mean
/// "which classes are worst documented" quietly answers about functions
/// only. `loc` is filled from the span for those, inclusive of both ends,
/// matching what the extractors now produce.
fn annotate_line_metrics(symbols: &mut [Symbol], content: &str, language: &str) {
    let syntax = line_metrics::syntax_for(language);
    let kinds = line_metrics::classify_lines(content, syntax);

    for sym in symbols.iter_mut() {
        let (comments, code) = line_metrics::count_range(&kinds, sym.start_line, sym.end_line);
        let doc_lines = sym
            .docstring
            .as_deref()
            .map(|d| d.lines().filter(|l| !l.trim().is_empty()).count() as u32)
            .unwrap_or(0);

        let span = sym.end_line.saturating_sub(sym.start_line) + 1;
        let metrics = sym.metrics.get_or_insert_with(|| SymbolMetrics {
            loc: span,
            ..Default::default()
        });
        metrics.comment_lines = comments;
        metrics.code_lines = code;
        metrics.doc_lines = doc_lines;
    }
}

/// Live single-line progress meter for the index stage. Callers wrap the
/// call in a `Mutex` guard so parallel workers never interleave the cursor
/// line — the print itself is not thread-safe across `\r` overwrites.
fn print_index_progress(done: usize, total: usize) {
    let pct = if total == 0 {
        100.0
    } else {
        done as f32 / total as f32 * 100.0
    };
    print!(
        "\r{}▸{} Indexing: {}{:>6.1}%{} ({}/{})",
        crate::C_CYAN,
        crate::C_RESET,
        crate::C_YELLOW,
        pct,
        crate::C_RESET,
        done,
        total
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Index every supported source file under `path`. Returns a JSON-encoded
/// [`IndexResult`].
pub fn index(path: String) -> String {
    let start = std::time::Instant::now();

    // Compute canonical repo root first, before scanning files
    let canonical_root = Path::new(&path).canonicalize().unwrap_or_else(|_| PathBuf::from(&path));
    let repo_root = canonical_root.to_string_lossy().to_string();

    // Walk the *canonical* root, not the path as given. `repo_root` above is
    // canonicalized and `strip_repo_root` is a prefix match: handed
    // `/var/folders/…` against a root of `/private/var/folders/…` — which is
    // what macOS produces for any symlinked parent — nothing strips and every
    // path in the graph stays absolute. Harmless when paths were only
    // display strings; not harmless now that qualified names are derived
    // from them.
    let files_paths = scan_files(&repo_root);
    let dependencies = extract_package_json_dependencies(&path);

    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;
    let mut total_lines = 0u64;

    let total_files = files_paths.len();
    let done = AtomicUsize::new(0);
    let print_mu = Mutex::new(());

    // Parse every file in parallel. The tree-sitter parse + symbol extraction
    // is pure CPU work per file with no shared mutable state, so this scales
    // near-linearly with core count. Results are tagged with their scan index
    // and sorted back into order afterwards — downstream consumers are
    // order-stable, but preserving scan order avoids node-id drift.
    let mut parsed: Vec<(usize, FileNode)> = files_paths
        .par_iter()
        .enumerate()
        .filter_map(|(i, file_path)| {
            let node = process_file(file_path, Some(&repo_root));
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            {
                let _g = print_mu.lock().unwrap();
                print_index_progress(n, total_files);
            }
            node.map(|fnode| (i, fnode))
        })
        .collect();
    parsed.sort_by_key(|(i, _)| *i);

    for (_, fnode) in parsed {
        total_symbols += fnode.symbols.len();
        total_lines += fnode.lines as u64;
        files.push(fnode);
    }
    println!(
        "\r{}▸{} Indexing: {}100.0% ({}/{}){} {}✓ done{}",
        crate::C_CYAN,
        crate::C_RESET,
        crate::C_GREEN,
        total_files,
        total_files,
        crate::C_RESET,
        crate::C_GREEN,
        crate::C_RESET
    );

    let folders = folder::extract_folders_relative(&repo_root);

    let last_indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let stats = IndexStats {
        graph_schema_version: crate::types::GRAPH_SCHEMA_VERSION,
        total_files: files.len(),
        cached_files: 0,
        total_symbols,
        total_folders: folders.len(),
        total_lines,
        indexing_time_ms: start.elapsed().as_millis() as u64,
        last_indexed_at,
        repo_root,
    };

    serde_json::to_string(&IndexResult {
        files,
        folders,
        dependencies,
        stats,
    })
    .unwrap_or_default()
}

/// Index every supported source file under `path`, skipping files whose
/// blake3 hash matches the value stored in `<cache_path>/cache.json` from a
/// previous run. The cache file is rewritten with the latest hashes once
/// indexing is complete.
pub fn index_with_cache(path: String, cache_path: String) -> String {
    let start = std::time::Instant::now();

    // Compute canonical repo root first
    let canonical_root = Path::new(&path).canonicalize().unwrap_or_else(|_| PathBuf::from(&path));
    let repo_root = canonical_root.to_string_lossy().to_string();

    let cache_file = Path::new(&cache_path).join("cache.json");
    let mut cached_hashes: HashMap<String, String> = HashMap::new();

    if cache_file.exists() {
        if let Ok(content) = fs::read_to_string(&cache_file) {
            if let Ok(hashes) = serde_json::from_str::<HashMap<String, String>>(&content) {
                // A cache written by a different extraction format holds
                // symbols this build no longer agrees with. Drop it whole
                // rather than serve a graph that is half-upgraded.
                if hashes.get(CACHE_VERSION_KEY).map(String::as_str) == Some(INDEXER_VERSION) {
                    cached_hashes = hashes;
                }
            }
        }
    }

    // Previous run's FileNodes, keyed by repo-relative path. The returned
    // IndexResult must cover every scanned file — callers overwrite
    // indexed-tree.json / graph.json wholesale — so a cache hit is only
    // usable if the file's node can be recovered from the previous tree.
    let mut prev_files: HashMap<String, FileNode> = HashMap::new();
    let prev_tree = Path::new(&cache_path).join("indexed-tree.json");
    if let Ok(content) = fs::read_to_string(&prev_tree) {
        if let Ok(prev) = serde_json::from_str::<IndexResult>(&content) {
            for f in prev.files {
                prev_files.insert(f.path.clone(), f);
            }
        }
    }

    // Walk the *canonical* root, not the path as given. `repo_root` above is
    // canonicalized and `strip_repo_root` is a prefix match: handed
    // `/var/folders/…` against a root of `/private/var/folders/…` — which is
    // what macOS produces for any symlinked parent — nothing strips and every
    // path in the graph stays absolute. Harmless when paths were only
    // display strings; not harmless now that qualified names are derived
    // from them.
    let files_paths = scan_files(&repo_root);
    let dependencies = extract_package_json_dependencies(&path);
    let mut total_symbols = 0;
    let mut total_lines = 0u64;
    let mut cached = 0;
    // Rebuilt from scratch each run so hashes of deleted files get pruned.
    let mut new_hashes: HashMap<String, String> = HashMap::new();

    let total_files = files_paths.len();
    let print_mu = Mutex::new(());

    // Phase 1 — hash every file in parallel. `compute_hash` reads the file
    // and runs blake3; cheap per file but independent, and the reads add up
    // across a large repo. Scan index is carried through so the final order
    // matches the single-threaded path.
    let mut hashed: Vec<(usize, PathBuf, String, String)> = files_paths
        .par_iter()
        .enumerate()
        .filter_map(|(i, file_path)| {
            let normalized = normalize_path(&file_path.to_string_lossy());
            let relative = common::strip_repo_root(&normalized, &repo_root);
            let hash = compute_hash(file_path)?;
            Some((i, file_path.clone(), relative, hash))
        })
        .collect();

    // Phase 2 — split cache hits from parse misses, sequentially. A hit
    // reuses the previous run's FileNode verbatim (no parse, no embed);
    // only misses reach the expensive phase 3. Sequential here keeps the
    // large `prev_files` map lock-free on the hot path.
    let mut by_index: Vec<(usize, FileNode)> = Vec::with_capacity(hashed.len());
    let mut misses: Vec<(usize, PathBuf, String, String)> = Vec::new();

    for (i, file_path, relative, hash) in hashed.drain(..) {
        if cached_hashes.get(&relative) == Some(&hash) {
            if let Some(prev) = prev_files.remove(&relative) {
                cached += 1;
                total_symbols += prev.symbols.len();
                total_lines += prev.lines as u64;
                new_hashes.insert(relative, hash);
                by_index.push((i, prev));
                continue;
            }
        }
        misses.push((i, file_path, relative, hash));
    }

    // Phase 3 — parse the misses in parallel. This is the tree-sitter work
    // and the dominant cost on a cold run; on a warm re-run `misses` is
    // small and this returns quickly. Progress is reported over `misses`
    // because that is the only phase a user ever waits on.
    let miss_count = misses.len();
    let done = AtomicUsize::new(0);
    let parsed: Vec<(usize, String, String, FileNode)> = misses
        .par_iter()
        .filter_map(|(i, file_path, relative, hash)| {
            let mut fnode = process_file(file_path, Some(&repo_root))?;
            fnode.hash = hash.clone();
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            {
                let _g = print_mu.lock().unwrap();
                print_index_progress(n, miss_count);
            }
            Some((*i, relative.clone(), hash.clone(), fnode))
        })
        .collect();

    for (i, relative, hash, fnode) in parsed {
        total_symbols += fnode.symbols.len();
        total_lines += fnode.lines as u64;
        new_hashes.insert(relative, hash);
        by_index.push((i, fnode));
    }
    by_index.sort_by_key(|(i, _)| *i);
    let files: Vec<FileNode> = by_index.into_iter().map(|(_, f)| f).collect();

    println!(
        "\r{}▸{} Indexing: {}100.0% ({}/{}){} {}✓ done{} ({} cached)",
        crate::C_CYAN,
        crate::C_RESET,
        crate::C_GREEN,
        total_files,
        total_files,
        crate::C_RESET,
        crate::C_GREEN,
        crate::C_RESET,
        cached
    );

    let _ = fs::create_dir_all(&cache_path);
    new_hashes.insert(CACHE_VERSION_KEY.to_string(), INDEXER_VERSION.to_string());
    if let Ok(json) = serde_json::to_string(&new_hashes) {
        let _ = fs::write(&cache_file, json);
    }

    let folders = folder::extract_folders_relative(&repo_root);

    let last_indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let stats = IndexStats {
        graph_schema_version: crate::types::GRAPH_SCHEMA_VERSION,
        total_files: files.len(),
        cached_files: cached,
        total_symbols,
        total_folders: folders.len(),
        total_lines,
        indexing_time_ms: start.elapsed().as_millis() as u64,
        last_indexed_at,
        repo_root,
    };

    let json = serde_json::to_string(&IndexResult {
        files,
        folders,
        dependencies,
        stats,
    })
    .unwrap_or_default();

    // Snapshot the tree next to cache.json so the *next* run can recover
    // FileNodes for its cache hits. Without this the cache can never hit:
    // `cached_hashes` would match but `prev_files` would always be empty,
    // because callers write their tree wherever `-o` points — which usually
    // isn't the cache directory. Keeping the snapshot here makes the cache
    // directory self-contained and independent of where output goes.
    if !json.is_empty() {
        let _ = fs::write(Path::new(&cache_path).join("indexed-tree.json"), &json);
    }

    json
}
