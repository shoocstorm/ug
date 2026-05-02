//! Folder-node derivation.
//!
//! Folder hierarchy carries semantic information that no individual file
//! captures: the difference between `src/components/` and `tests/components/`,
//! a `docs/2026/january/` knowledge-base layout, the `lib/` vs `app/` split.
//! The Semantic Enrichment phase will consume these nodes and write a
//! one-line summary per folder; the graph layer can then connect folders to
//! their files / subfolders so retrieval can climb the tree.
//!
//! Folders are *derived* from the indexed file set rather than parsed - we
//! never touch tree-sitter here. A folder exists in the output iff at least
//! one indexed file lives inside it (directly or transitively). A synthetic
//! root folder with path `"."` is always emitted as the anchor of the forest,
//! even when every file lives inside a subdirectory; this keeps consumers
//! from special-casing "is this top-level?" everywhere.

use crate::indexer::languages;
use crate::types::{FolderClassification, FolderNode};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

/// File names (case-insensitive) treated as the README/landing doc for the
/// containing folder. First match in directory order wins.
const README_NAMES: &[&str] = &[
    "readme.md",
    "readme.mdx",
    "readme.markdown",
    "_index.md",
    "_index.mdx",
    "index.md",
    "index.mdx",
];

/// Path used for the synthetic project-root folder. Mirrors `cd .` semantics:
/// the parent of every top-level file/folder. Chosen over `""` so the path is
/// always a valid lookup key.
const ROOT_PATH: &str = ".";

/// Build the folder forest with paths relative to the repo root.
/// This is the preferred function when the indexer has already computed `repo_root`.
pub fn extract_folders_relative(repo_root: &str) -> Vec<FolderNode> {
    let repo_root = crate::indexer::common::normalize_path(repo_root);
    let mut folder_paths: BTreeSet<String> = BTreeSet::new();
    folder_paths.insert(ROOT_PATH.to_string());

    let mut file_paths: Vec<String> = Vec::new();

    for file_node in crate::indexer::common::scan_files(&repo_root) {
        let normalized = crate::indexer::common::normalize_path(&file_node.to_string_lossy());
        let relative = crate::indexer::common::strip_repo_root(&normalized, &repo_root);
        file_paths.push(relative.clone());
        folder_paths.insert(ROOT_PATH.to_string());
        let mut cursor = parent_folder(&relative);
        while cursor != ROOT_PATH {
            folder_paths.insert(cursor.clone());
            cursor = parent_folder(&cursor);
        }
    }

    if folder_paths.len() <= 1 && file_paths.is_empty() {
        return Vec::new();
    }

    let mut child_files: HashMap<String, Vec<String>> = HashMap::new();
    let mut child_folders: HashMap<String, Vec<String>> = HashMap::new();
    let mut total_files: HashMap<String, u32> = HashMap::new();
    let mut lang_breakdown: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for folder in &folder_paths {
        child_files.insert(folder.clone(), Vec::new());
        child_folders.insert(folder.clone(), Vec::new());
        total_files.insert(folder.clone(), 0);
        lang_breakdown.insert(folder.clone(), HashMap::new());
    }

    for folder in &folder_paths {
        if folder == ROOT_PATH {
            continue;
        }
        let parent = parent_folder(folder);
        if let Some(siblings) = child_folders.get_mut(&parent) {
            siblings.push(folder.clone());
        }
    }

    for file in &file_paths {
        let parent = parent_folder(file);
        if let Some(files) = child_files.get_mut(&parent) {
            files.push(file.clone());
        }
        let lang = language_for_path(file);
        let mut cursor = parent;
        loop {
            if let Some(c) = total_files.get_mut(&cursor) {
                *c += 1;
            }
            if let Some(name) = lang {
                let entry = lang_breakdown.entry(cursor.clone()).or_default();
                *entry.entry(name.to_string()).or_insert(0) += 1;
            }
            if cursor == ROOT_PATH {
                break;
            }
            cursor = parent_folder(&cursor);
        }
    }

    let mut out: Vec<FolderNode> = folder_paths
        .iter()
        .map(|path| {
            let name = folder_name(path);
            let parent = if path == ROOT_PATH {
                None
            } else {
                Some(parent_folder(path))
            };
            let depth = if path == ROOT_PATH {
                0
            } else {
                path.split('/').count() as u32
            };
            let mut files = child_files.remove(path).unwrap_or_default();
            files.sort();
            let mut folders = child_folders.remove(path).unwrap_or_default();
            folders.sort();
            let total = total_files.get(path).copied().unwrap_or(0);
            let breakdown = lang_breakdown.remove(path).unwrap_or_default();
            let readme = files.iter().find(|f| is_readme(f)).cloned();
            let classification = classify_folder(path, &breakdown, total);

            FolderNode {
                path: path.clone(),
                name,
                parent,
                depth,
                classification,
                readme,
                child_files: files,
                child_folders: folders,
                total_files: total,
                language_breakdown: breakdown,
                summary: None,
            }
        })
        .collect();

    out.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.path.cmp(&b.path)));
    out
}

/// Parent of a normalized path. Files/folders at the top level return the
/// synthetic root marker so the bubble-up loop in `extract_folders` always
/// terminates on `ROOT_PATH`.
fn parent_folder(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => ROOT_PATH.to_string(),
    }
}

fn folder_name(path: &str) -> String {
    if path == ROOT_PATH {
        return ROOT_PATH.to_string();
    }
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Map a file's extension to the indexer's language name, or `None` when no
/// indexer is registered (in practice this can't happen because `scan_files`
/// already filters by `SUPPORTED_EXTS`, but treat it defensively).
fn language_for_path(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?;
    languages::for_extension(ext).map(|i| i.name())
}

fn is_readme(file_path: &str) -> bool {
    let name = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    README_NAMES.iter().any(|n| *n == name)
}

/// Best-effort folder classification. Path-name heuristics fire first - they
/// reflect strong project conventions (`tests/`, `docs/`, `components/`) and
/// rarely mislead. When the folder name is uninformative, fall back to the
/// language breakdown: an all-markdown folder is documentation, an all-code
/// folder is source, anything else is mixed. Returns `None` only for empty
/// folders that contain neither files nor recognised subfolder names.
fn classify_folder(
    path: &str,
    breakdown: &HashMap<String, u32>,
    total: u32,
) -> Option<FolderClassification> {
    let lower = path.to_lowercase();
    let last = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    if matches!(
        last.as_str(),
        "tests" | "test" | "__tests__" | "spec" | "specs"
    ) || lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
    {
        return Some(FolderClassification::Tests);
    }
    if matches!(last.as_str(), "docs" | "doc" | "documentation" | "wiki")
        || lower.contains("/docs/")
        || lower.contains("/documentation/")
    {
        return Some(FolderClassification::Documentation);
    }
    if matches!(
        last.as_str(),
        "examples" | "example" | "samples" | "demo" | "demos"
    ) || lower.contains("/examples/")
        || lower.contains("/samples/")
    {
        return Some(FolderClassification::Examples);
    }
    if matches!(last.as_str(), "config" | "configs" | "settings") || lower.contains("/config/") {
        return Some(FolderClassification::Config);
    }
    if matches!(
        last.as_str(),
        "assets" | "static" | "public" | "images" | "img" | "media"
    ) || lower.contains("/assets/")
        || lower.contains("/static/")
        || lower.contains("/public/")
    {
        return Some(FolderClassification::Assets);
    }
    if matches!(last.as_str(), "components" | "component") {
        return Some(FolderClassification::Components);
    }
    if matches!(last.as_str(), "pages" | "page" | "routes" | "route") {
        return Some(FolderClassification::Pages);
    }
    if matches!(last.as_str(), "hooks" | "hook") {
        return Some(FolderClassification::Hooks);
    }
    if matches!(last.as_str(), "services" | "service") {
        return Some(FolderClassification::Services);
    }
    if matches!(last.as_str(), "contexts" | "context") {
        return Some(FolderClassification::Contexts);
    }
    if matches!(last.as_str(), "reducers" | "reducer") {
        return Some(FolderClassification::Reducers);
    }
    if matches!(
        last.as_str(),
        "utils" | "util" | "helpers" | "helper" | "lib"
    ) || lower.contains("/utils/")
        || lower.contains("/helpers/")
    {
        return Some(FolderClassification::Utils);
    }
    if matches!(last.as_str(), "types" | "type" | "typings") {
        return Some(FolderClassification::Types);
    }

    if total == 0 {
        return None;
    }
    let markdown_count = breakdown.get("markdown").copied().unwrap_or(0);
    if markdown_count == total {
        return Some(FolderClassification::Documentation);
    }
    let code_count: u32 = ["typescript", "javascript", "python", "java"]
        .iter()
        .map(|l| breakdown.get(*l).copied().unwrap_or(0))
        .sum();
    if code_count == total {
        return Some(FolderClassification::Source);
    }
    Some(FolderClassification::Mixed)
}
