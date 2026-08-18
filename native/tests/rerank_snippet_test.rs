//! P4.2 (`mmr_rerank`) and P4.3 (`SnippetCache`).
//!
//! Both are pure caching changes, so the thing worth asserting is that they
//! changed nothing: the rerank must return the same picks in the same order
//! as the naive form, and a cached snippet must equal an uncached one.

use std::path::Path;
use ultragraph::storage::{mmr_rerank, read_snippet, snippet_for, NodeRow, SnippetCache};
use ultragraph::storage::query::SearchHit;

fn row(id: &str, vector: Vec<f32>) -> NodeRow {
    NodeRow {
        id: id.to_string(),
        name: id.to_string(),
        node_type: "Function".to_string(),
        description: String::new(),
        file: String::new(),
        start_line: 0,
        end_line: 0,
        last_update_at: 0,
        node_text: String::new(),
        vector,
        code: String::new(),
        file_hash: String::new(),
        facts: Default::default(),
    }
}

fn hit(id: &str, vector: Vec<f32>) -> SearchHit {
    SearchHit {
        node: row(id, vector),
        distance: 0.0,
    }
}

/// Reference implementation: the straightforward MMR the optimised one
/// replaced, kept here so "bit-identical" is a test rather than a claim.
fn mmr_reference(query: &[f32], candidates: Vec<SearchHit>, k: usize, lambda: f32) -> Vec<SearchHit> {
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        if a.is_empty() || b.is_empty() || a.len() != b.len() {
            return 0.0;
        }
        let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..a.len() {
            dot += a[i] * b[i];
            na += a[i] * a[i];
            nb += b[i] * b[i];
        }
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        dot / (na.sqrt() * nb.sqrt())
    }

    if candidates.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut remaining = candidates;
    let mut picked: Vec<SearchHit> = Vec::new();
    let lambda = lambda.clamp(0.0, 1.0);

    while picked.len() < k && !remaining.is_empty() {
        let mut best_idx = 0usize;
        let mut best_score = f32::MIN;
        for (i, cand) in remaining.iter().enumerate() {
            let rel = cosine(&cand.node.vector, query);
            let div = picked
                .iter()
                .map(|p| cosine(&cand.node.vector, &p.node.vector))
                .fold(f32::MIN, f32::max);
            let div = if div == f32::MIN { 0.0 } else { div };
            let score = lambda * rel - (1.0 - lambda) * div;
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        picked.push(remaining.swap_remove(best_idx));
    }
    picked
}

/// Deterministic spread of vectors, some near-duplicates so the diversity
/// term actually changes the ordering.
fn corpus(n: usize, dim: usize) -> Vec<SearchHit> {
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f32 / (1u64 << 53) as f32
    };
    (0..n)
        .map(|i| {
            let v: Vec<f32> = (0..dim).map(|_| next() - 0.5).collect();
            hit(&format!("n{i}"), v)
        })
        .collect()
}

#[test]
fn rerank_matches_the_reference_exactly() {
    let dim = 16;
    let query: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.37).sin()).collect();

    for &k in &[1usize, 3, 8, 25] {
        for &lambda in &[0.0f32, 0.3, 0.5, 0.7, 1.0] {
            let cands = corpus(40, dim);
            let want = mmr_reference(&query, cands.clone(), k, lambda);
            let got = mmr_rerank(&query, cands, k, lambda);

            let want_ids: Vec<&str> = want.iter().map(|h| h.node.id.as_str()).collect();
            let got_ids: Vec<&str> = got.iter().map(|h| h.node.id.as_str()).collect();
            assert_eq!(got_ids, want_ids, "k={k} lambda={lambda}");
        }
    }
}

#[test]
fn rerank_handles_the_degenerate_inputs() {
    let query = vec![1.0f32, 0.0, 0.0];
    assert!(mmr_rerank(&query, vec![], 5, 0.5).is_empty());
    assert!(mmr_rerank(&query, vec![hit("a", vec![1.0, 0.0, 0.0])], 0, 0.5).is_empty());

    // k larger than the candidate set returns everything, once.
    let got = mmr_rerank(
        &query,
        vec![hit("a", vec![1.0, 0.0, 0.0]), hit("b", vec![0.0, 1.0, 0.0])],
        10,
        0.5,
    );
    assert_eq!(got.len(), 2);

    // A zero vector scores 0 rather than NaN-ing the comparison.
    let got = mmr_rerank(
        &query,
        vec![hit("zero", vec![0.0, 0.0, 0.0]), hit("a", vec![1.0, 0.0, 0.0])],
        2,
        0.5,
    );
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].node.id, "a", "the zero vector should not win");

    // Mismatched width is tolerated, not panicked on.
    let got = mmr_rerank(&query, vec![hit("wide", vec![1.0; 9])], 1, 0.5);
    assert_eq!(got.len(), 1);
}

// ---------- P4.3 ----------

#[test]
fn cached_snippets_equal_uncached_ones() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one\ntwo\nthree\nfour\nfive\n").unwrap();

    let mut cache = SnippetCache::default();
    // Several spans from the same file: the case the cache exists for.
    for (start, end) in [(1u32, 2u32), (2, 4), (3, 3), (1, 5)] {
        let mut r = row("x", vec![]);
        r.file = "a.rs".to_string();
        r.start_line = start;
        r.end_line = end;

        let uncached = snippet_for(&r, root);
        let cached = cache.snippet_for(&r, root);
        assert_eq!(cached, uncached, "span {start}..{end}");
        assert_eq!(cached, read_snippet(root, "a.rs", start, end));
    }
}

#[test]
fn the_cache_agrees_on_the_edge_cases() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one\ntwo\n").unwrap();

    let mut cache = SnippetCache::default();
    let mut check = |file: &str, start: u32, end: u32| {
        let mut r = row("x", vec![]);
        r.file = file.to_string();
        r.start_line = start;
        r.end_line = end;
        assert_eq!(
            cache.snippet_for(&r, root),
            snippet_for(&r, root),
            "file={file} {start}..{end}"
        );
    };

    check("a.rs", 0, 2); // zero start
    check("a.rs", 2, 1); // inverted range
    check("a.rs", 9, 12); // past the end
    check("", 1, 2); // no file
    check("missing.rs", 1, 2); // unreadable — and cached as such
    check("missing.rs", 1, 2); // second ask must agree with the first
}

/// A row carrying its own captured code never touches the filesystem, cached
/// or not.
#[test]
fn stored_code_wins_over_the_file() {
    let root = Path::new("/definitely/not/a/real/path");
    let mut r = row("x", vec![]);
    r.file = "a.rs".to_string();
    r.start_line = 1;
    r.end_line = 2;
    r.code = "captured\n".to_string();

    let mut cache = SnippetCache::default();
    assert_eq!(cache.snippet_for(&r, root).as_deref(), Some("captured\n"));
    assert_eq!(snippet_for(&r, root).as_deref(), Some("captured\n"));
}
