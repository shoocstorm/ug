//! Corpus statistics for BM25-weighted sparse retrieval.
//!
//! # Why this exists
//!
//! The keyword half of hybrid search is a sparse vector scored by plain dot
//! product (OverGraph accumulates `query_weight × stored_weight` over
//! posting lists — it has no notion of a scoring model). Filling those
//! vectors with raw term frequency, as the first implementation did, means
//! `the`, `let`, `self` and `return` count for exactly as much as
//! `buildSparseKeywordVector`. That is survivable when every query is a
//! distinctive identifier, and much less so now that document prose is
//! indexed too, because a natural-language query is mostly common words.
//!
//! BM25 fixes it, and — this is the part that makes it cheap — it
//! factorizes so that the engine never has to know about it:
//!
//! ```text
//! score = Σ  IDF(t) · [ tf(t,d)·(k1+1) / (tf(t,d) + k1·(1 − b + b·|d|/avgdl)) ]
//!            ------    ---------------------------------------------------
//!            query side                     document side
//! ```
//!
//! Store the document-side factor as the sparse weight, put IDF in the
//! *query* vector, and the dot product the engine already computes **is**
//! BM25. No second index, no full-text engine, no change to OverGraph.
//!
//! # Why `b = 0`
//!
//! Length normalization is the one term that coupleseach document's weight
//! to the corpus (through `avgdl`), which would mean every edit invalidates
//! every stored vector — incremental re-ingest would be dead. With `b = 0`
//! the document-side factor `tf·(k1+1)/(tf+k1)` depends only on the node
//! itself, so stored vectors stay valid exactly as they do today, and we
//! still get the two effects that matter most: IDF and term-frequency
//! saturation. Node spans are also far more uniform in length than the web
//! documents `b` was designed for, so it is the least valuable of the
//! three here.
//!
//! Document frequency therefore never invalidates a stored vector. It is
//! read at query time to weight the query, and at ingest only to decide
//! which terms survive the per-node dimension cap.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Filename of the stats sidecar, written next to the store's data
/// directory alongside `ug-meta.json`.
const STATS_FILE: &str = "ug-sparse-stats.json";

/// BM25 term-frequency saturation. The standard 1.2: the second occurrence
/// of a term adds much less than the first, the tenth almost nothing.
pub const BM25_K1: f32 = 1.2;

/// Document frequency at or below which a term is left out of the stored
/// map and reconstructed on read.
///
/// Terms appearing in exactly one node are the bulk of any code
/// vocabulary — every unique identifier, every typo, every hash — and they
/// all resolve to the same near-maximum IDF. Dropping them costs nothing
/// and typically shrinks the sidecar by more than half.
const OMIT_DF_AT_OR_BELOW: u32 = 1;

/// Per-corpus term statistics: how many documents there are, and how many
/// of them each dimension appears in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SparseStats {
    /// Total documents (nodes) the frequencies were computed over.
    pub total_docs: u32,
    /// `(dimension, document frequency)`, sorted by dimension. Stored as
    /// pairs rather than a JSON object because the keys are `u32` hashes —
    /// an object would quote every one of them.
    ///
    /// Dimensions with `df <= OMIT_DF_AT_OR_BELOW` are absent and treated
    /// as `df = 1` on lookup.
    df: Vec<(u32, u32)>,
}

impl SparseStats {
    /// Build from every document's sparse dimension set. `docs` yields one
    /// slice of dimension ids per node; duplicates within a node are fine,
    /// they count once.
    pub fn from_documents<'a, I>(docs: I) -> Self
    where
        I: IntoIterator<Item = &'a [u32]>,
    {
        let mut counts: HashMap<u32, u32> = HashMap::new();
        let mut total_docs = 0u32;
        for dims in docs {
            total_docs = total_docs.saturating_add(1);
            let mut seen: Vec<u32> = dims.to_vec();
            seen.sort_unstable();
            seen.dedup();
            for d in seen {
                *counts.entry(d).or_insert(0) += 1;
            }
        }
        let mut df: Vec<(u32, u32)> = counts
            .into_iter()
            .filter(|&(_, c)| c > OMIT_DF_AT_OR_BELOW)
            .collect();
        df.sort_unstable_by_key(|&(d, _)| d);
        Self { total_docs, df }
    }

    /// Documents containing `dim`. Absent dimensions read as 1 — either a
    /// hapax that was omitted, or a term this corpus has never seen, and
    /// both deserve the rare-term weighting.
    pub fn doc_freq(&self, dim: u32) -> u32 {
        match self.df.binary_search_by_key(&dim, |&(d, _)| d) {
            Ok(i) => self.df[i].1,
            Err(_) => 1,
        }
    }

    /// Inverse document frequency, Lucene's smoothed form:
    /// `ln(1 + (N − df + 0.5) / (df + 0.5))`.
    ///
    /// The `1 +` matters here beyond convention. OverGraph rejects negative
    /// sparse weights on both the stored *and* the query side, and the
    /// classic Robertson IDF goes negative once a term appears in more than
    /// half the corpus. This variant is strictly positive everywhere.
    pub fn idf(&self, dim: u32) -> f32 {
        let n = self.total_docs.max(1) as f32;
        let df = self.doc_freq(dim).min(self.total_docs.max(1)) as f32;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// Whether this carries anything usable. An empty corpus means every
    /// term is equally rare, so callers skip IDF weighting entirely rather
    /// than multiply everything by the same constant.
    pub fn is_empty(&self) -> bool {
        self.total_docs == 0
    }

    pub fn terms(&self) -> usize {
        self.df.len()
    }

    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(STATS_FILE)
    }

    /// Read the sidecar from a store directory. A missing or unreadable
    /// file yields `None` — keyword search then falls back to unweighted
    /// term frequency, which is what it did before BM25 existed.
    pub fn load(dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::path_in(dir)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path_in(dir), json)
    }
}

/// BM25's document-side term weight with `b = 0`: saturating in `tf`,
/// bounded above by `k1 + 1`.
///
/// `tf` is the term's accumulated weight in the node, which is not
/// necessarily an integer — source-body tokens enter discounted (see
/// `text::CODE_TOKEN_WEIGHT`), and that discount should survive
/// saturation rather than be rounded away.
pub fn saturate_tf(tf: f32) -> f32 {
    if tf <= 0.0 {
        return 0.0;
    }
    tf * (BM25_K1 + 1.0) / (tf + BM25_K1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_is_monotone_and_bounded() {
        let one = saturate_tf(1.0);
        let two = saturate_tf(2.0);
        let many = saturate_tf(100.0);

        assert!(two > one, "more occurrences still score higher");
        assert!(many < BM25_K1 + 1.0, "bounded by k1+1, got {many}");
        // The point of saturation: 100 occurrences are worth nowhere near
        // 100 times one occurrence.
        assert!(many < one * 3.0, "one={one} many={many}");
        assert_eq!(saturate_tf(0.0), 0.0);
    }

    #[test]
    fn idf_falls_as_a_term_spreads_and_never_goes_negative() {
        let docs: Vec<Vec<u32>> = vec![
            vec![1, 2],
            vec![1, 3],
            vec![1, 4],
            vec![1, 5],
        ];
        let stats = SparseStats::from_documents(docs.iter().map(|v| v.as_slice()));

        assert_eq!(stats.total_docs, 4);
        assert_eq!(stats.doc_freq(1), 4, "dim 1 is in every document");
        assert_eq!(stats.doc_freq(2), 1, "hapax reads back as 1");
        assert_eq!(stats.doc_freq(999), 1, "unseen term is treated as rare");

        assert!(
            stats.idf(2) > stats.idf(1),
            "a rare term must outweigh a ubiquitous one"
        );
        // Robertson IDF would be negative for a term in 4/4 documents;
        // OverGraph rejects negative weights, so this must not be.
        assert!(stats.idf(1) > 0.0, "got {}", stats.idf(1));
    }

    #[test]
    fn hapaxes_are_omitted_from_the_stored_map() {
        let docs: Vec<Vec<u32>> = vec![vec![1, 2, 3], vec![1, 4, 5]];
        let stats = SparseStats::from_documents(docs.iter().map(|v| v.as_slice()));
        // Only dim 1 appears twice; 2..5 are hapaxes and get dropped.
        assert_eq!(stats.terms(), 1, "only the repeated term is stored");
        assert_eq!(stats.doc_freq(3), 1, "dropped terms still read correctly");
    }

    #[test]
    fn repeats_within_one_document_count_once() {
        let docs: Vec<Vec<u32>> = vec![vec![7, 7, 7], vec![7]];
        let stats = SparseStats::from_documents(docs.iter().map(|v| v.as_slice()));
        assert_eq!(stats.doc_freq(7), 2, "document frequency, not term frequency");
    }

    #[test]
    fn round_trips_through_the_sidecar() {
        let dir = tempfile::TempDir::new().unwrap();
        let docs: Vec<Vec<u32>> = vec![vec![1, 2], vec![1, 2], vec![1, 3]];
        let stats = SparseStats::from_documents(docs.iter().map(|v| v.as_slice()));
        stats.save(dir.path()).unwrap();

        let back = SparseStats::load(dir.path()).expect("sidecar reloads");
        assert_eq!(back.total_docs, stats.total_docs);
        assert_eq!(back.doc_freq(1), 3);
        assert_eq!(back.doc_freq(2), 2);

        assert!(SparseStats::load(&dir.path().join("nope")).is_none());
    }
}
