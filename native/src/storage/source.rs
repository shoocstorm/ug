//! Capture each node's source text at ingest time.
//!
//! Without this the store is a pointer index: it can tell an agent *where*
//! code lives but never *what it says*, so every read goes back to the
//! working tree. Two problems follow from that. The row's description and
//! the code an agent reads can disagree, because one is a snapshot from
//! index time and the other is live. And a line range that has merely
//! shifted still resolves — the agent silently receives the wrong lines,
//! with no error to notice.
//!
//! Storing the span alongside the row makes the two consistent by
//! construction, and pairing it with the file's content hash makes
//! staleness *checkable* rather than invisible.
//!
//! Cost, measured on a real graph: the captured spans came to 1.04x the
//! raw source, and the source itself was smaller than the dense vectors
//! already in the same database. Overlap between a class node and its
//! methods is negligible in practice because extractors give class nodes
//! narrow declaration ranges.

use crate::storage::store::KnowledgeStore;
use crate::types::GraphData;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A node's captured source plus the hash of the file it came from.
#[derive(Debug, Clone, Default)]
pub struct CapturedCode {
    pub code: String,
    pub file_hash: String,
}

/// Source captured at index time for one node, as read back from a store.
///
/// The write-side twin of [`CapturedCode`]; separate because the two travel
/// in opposite directions — one is produced from the working tree by
/// [`capture_graph_code`], the other is what a read tool gets back when the
/// working tree is no longer there.
#[derive(Debug, Clone, Default)]
pub struct StoredSource {
    pub code: String,
    pub file_hash: String,
}

/// The captured source a read tool needs, keyed by node id.
///
/// This is what lets `get_code`, `find_usages` and friends answer after the
/// repo has moved, been deleted, or was never on this machine: ingest wrote
/// each node's span into the store, so the source is project data under
/// `~/.ug/<project>/` rather than something to go looking for on disk.
///
/// Deliberately a plain pre-fetched map rather than a live store handle.
/// The read tools are synchronous and are called from a blocking CLI, an
/// async MCP server and an async HTTP handler alike; making them own an
/// async lookup would force one of those three to block inside a runtime.
/// Transports resolve the ids they need up front (see
/// [`crate::agent_tools::source_node_ids`]) and hand over the result.
#[derive(Debug, Clone, Default)]
pub struct IndexedSource {
    by_node: HashMap<String, StoredSource>,
}

impl IndexedSource {
    pub fn is_empty(&self) -> bool {
        self.by_node.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_node.len()
    }

    /// Captured source for a node id, or `None` when the index has none —
    /// a row written before the column existed, or a file that could not be
    /// read at ingest time (binary, generated, already deleted).
    pub fn node(&self, id: &str) -> Option<&StoredSource> {
        self.by_node.get(id).filter(|s| !s.code.is_empty())
    }

    pub fn insert(&mut self, id: impl Into<String>, src: StoredSource) {
        if !src.code.is_empty() {
            self.by_node.insert(id.into(), src);
        }
    }

    /// Fetch the captured source for `ids` from a store.
    ///
    /// Best effort by design: a store that cannot be opened, a backend
    /// without the column, or ids it has never seen all yield an empty (or
    /// partial) result, and every caller falls back to the working tree for
    /// what is missing. Reading source must not start failing because the
    /// database is unavailable — that was the behaviour before the store
    /// captured anything at all.
    pub async fn load(store: &dyn KnowledgeStore, ids: &[String]) -> Self {
        let mut out = Self::default();
        if ids.is_empty() {
            return out;
        }
        if let Ok(rows) = store.nodes_by_ids(ids).await {
            for r in rows {
                out.insert(
                    r.id,
                    StoredSource {
                        code: r.code,
                        file_hash: r.file_hash,
                    },
                );
            }
        }
        out
    }
}

/// Read every file the graph references once, and slice out each node's
/// span from it.
///
/// Reading per file rather than per node matters: nodes are many and files
/// are few, and the naive shape re-reads a whole file for every symbol in
/// it. Peak memory is bounded by the captured spans, not by the repo —
/// file contents are dropped as soon as the file's nodes are sliced.
///
/// Files that cannot be read are skipped silently; their nodes simply get
/// no captured code and fall back to the filesystem at read time. That is
/// the right behaviour for binaries, generated paths, and anything already
/// deleted between indexing and ingest.
pub fn capture_graph_code(graph: &GraphData, repo_root: &Path) -> HashMap<String, CapturedCode> {
    // Group node indices by file so each file is opened exactly once.
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        if let Some(f) = n.file.as_deref() {
            if !f.is_empty() {
                by_file.entry(f).or_default().push(i);
            }
        }
    }

    let mut out: HashMap<String, CapturedCode> = HashMap::new();
    for (file, indices) in by_file {
        let abs: PathBuf = if Path::new(file).is_absolute() {
            PathBuf::from(file)
        } else {
            repo_root.join(file)
        };
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        let file_hash = blake3::hash(&bytes).to_hex().to_string();
        let Ok(content) = String::from_utf8(bytes) else {
            // Binary (PDF, image). The indexer may still have produced
            // nodes for it via the document extractor, but there is no
            // meaningful text span to slice.
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();

        for i in indices {
            let n = &graph.nodes[i];
            // No range means the node *is* the file (File/Config nodes),
            // so its code is the whole thing.
            let code = match (n.start_line, n.end_line) {
                (Some(s), Some(e)) => slice_lines(&lines, s, e),
                _ => content.clone(),
            };
            if code.is_empty() {
                continue;
            }
            out.insert(
                n.id.clone(),
                CapturedCode {
                    code,
                    file_hash: file_hash.clone(),
                },
            );
        }
    }
    out
}

/// Join `start..=end` (1-indexed, inclusive) back into a string.
/// Out-of-range bounds clamp rather than panic — an index can lag the file
/// it describes.
fn slice_lines(lines: &[&str], start: u32, end: u32) -> String {
    if start == 0 || end < start {
        return String::new();
    }
    let from = (start as usize - 1).min(lines.len());
    let to = (end as usize).min(lines.len());
    if from >= to {
        return String::new();
    }
    let mut s = String::new();
    for line in &lines[from..to] {
        s.push_str(line);
        s.push('\n');
    }
    s
}

/// Whether `file` on disk still hashes to `expected`.
///
/// Used to tell an agent that stored code is stale instead of serving it
/// as though it were current. `None` when the file cannot be read at all,
/// which callers report as "missing" rather than "stale".
pub fn file_matches_hash(repo_root: &Path, file: &str, expected: &str) -> Option<bool> {
    if file.is_empty() || expected.is_empty() {
        return None;
    }
    let abs: PathBuf = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        repo_root.join(file)
    };
    let bytes = std::fs::read(abs).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string() == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GraphData, GraphNode, GraphNodeType};

    fn node(id: &str, file: &str, start: Option<u32>, end: Option<u32>) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: id.into(),
            node_type: GraphNodeType::Function,
            file: Some(file.into()),
            start_line: start,
            end_line: end,
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec![],
            implements: vec![],
            calls: vec![],
            folder: None,
            ..Default::default()
        }
    }

    #[test]
    fn captures_spans_and_whole_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();

        let graph = GraphData {
            nodes: vec![
                node("sym", "a.rs", Some(2), Some(3)),
                node("whole", "a.rs", None, None),
            ],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let got = capture_graph_code(&graph, dir.path());

        assert_eq!(got["sym"].code, "two\nthree\n");
        assert_eq!(got["whole"].code, "one\ntwo\nthree\nfour\n");
        assert_eq!(
            got["sym"].file_hash, got["whole"].file_hash,
            "same file, same hash"
        );
        assert!(!got["sym"].file_hash.is_empty());
    }

    #[test]
    fn out_of_range_lines_clamp_instead_of_panicking() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\n").unwrap();

        let graph = GraphData {
            nodes: vec![node("past_eof", "a.rs", Some(2), Some(999))],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let got = capture_graph_code(&graph, dir.path());
        assert_eq!(got["past_eof"].code, "two\n");
    }

    #[test]
    fn missing_files_are_skipped_not_fatal() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = GraphData {
            nodes: vec![node("gone", "nope.rs", Some(1), Some(2))],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        assert!(capture_graph_code(&graph, dir.path()).is_empty());
    }

    #[test]
    fn hash_check_detects_edits_and_absence() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\n").unwrap();
        let graph = GraphData {
            nodes: vec![node("n", "a.rs", Some(1), Some(1))],
            edges: vec![],
            stats: None,
            resolution: None,
        };
        let hash = capture_graph_code(&graph, dir.path())["n"].file_hash.clone();

        assert_eq!(file_matches_hash(dir.path(), "a.rs", &hash), Some(true));
        std::fs::write(dir.path().join("a.rs"), "changed\n").unwrap();
        assert_eq!(file_matches_hash(dir.path(), "a.rs", &hash), Some(false));
        assert_eq!(file_matches_hash(dir.path(), "missing.rs", &hash), None);
    }
}
