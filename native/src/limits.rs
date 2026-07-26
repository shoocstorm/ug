//! Every cap that shapes what reaches the embedder or the agent, in one
//! machine-readable list.
//!
//! These caps are not incidental — they decide what a node's vector can
//! possibly match on. A markdown section longer than
//! [`SECTION_TEXT_CAP`][sec] embeds only its opening paragraphs; a node with
//! more than [`MAX_RELATED`][rel] neighbours embeds only the first 24 names.
//! A user who does not know that reads a search miss as "semantic search is
//! bad" rather than "that sentence was past the cap".
//!
//! So the numbers are published: `/api/capabilities` serves this list and
//! the visualization's Chunk tab shows which of them applied to the node on
//! screen, and which actually bit.
//!
//! This module deliberately holds **no cap values of its own**. Every entry
//! reads the constant that actually enforces the behaviour, so the published
//! number cannot drift from the real one. Adding a cap means adding an entry
//! here — see `docs/INDEXING-AND-CHUNKING.md` §6.
//!
//! [sec]: crate::indexer::languages::markdown::SECTION_TEXT_CAP
//! [rel]: crate::storage::text::MAX_RELATED

use serde::Serialize;

/// Where in the pipeline a cap is applied. Determines whether re-indexing
/// is needed to feel a change in it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Applied while reading files. Changing it requires a re-index.
    Index,
    /// Applied while building the text that gets embedded. Changing it
    /// requires re-embedding (`ug gen` picks this up automatically —
    /// the text changes, so the incremental planner re-embeds).
    Embed,
    /// Applied when answering a query. Changing it takes effect
    /// immediately, with no re-index.
    Retrieve,
}

/// One published cap.
#[derive(Debug, Clone, Serialize)]
pub struct Limit {
    /// Stable machine key, e.g. `"markdown_section_text"`. Clients match on
    /// this; `label` is free to be reworded.
    pub id: &'static str,
    pub label: &'static str,
    pub value: u64,
    /// `"bytes"`, `"chars"`, `"names"`, `"dimensions"` — what `value` counts.
    pub unit: &'static str,
    pub stage: Stage,
    /// File extensions the cap applies to. Empty means every node.
    pub extensions: &'static [&'static str],
    /// One sentence, addressed to a user: what is lost when this cap bites.
    pub effect: &'static str,
    /// Path to the constant, so the number is traceable to code.
    pub source: &'static str,
}

const MARKDOWN_EXTS: &[&str] = &["md", "mdx", "markdown"];
const DOCUMENT_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "docm", "dot", "dotm", "dotx", "odt", "ott", "rtf", "xls", "xlsx",
    "xlsm", "xlsb", "ods", "ots", "ppt", "pptx", "pptm", "pot", "potm", "potx", "odp", "otp",
];
const CODE_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "py", "java", "rs"];

/// Every published cap, in pipeline order.
pub fn all() -> Vec<Limit> {
    vec![
        Limit {
            id: "markdown_section_text",
            label: "Markdown section prose",
            value: crate::indexer::languages::markdown::SECTION_TEXT_CAP as u64,
            unit: "bytes",
            stage: Stage::Index,
            extensions: MARKDOWN_EXTS,
            effect: "A heading's prose is embedded up to this length; past it the \
                     section keeps its opening paragraphs and the rest is searchable \
                     only by keyword.",
            source: "indexer/languages/markdown.rs:SECTION_TEXT_CAP",
        },
        Limit {
            id: "document_page_text",
            label: "Document page text",
            value: crate::indexer::document::PAGE_TEXT_CAP as u64,
            unit: "bytes",
            stage: Stage::Index,
            extensions: DOCUMENT_EXTS,
            effect: "Each PDF/Office page is stored and embedded up to this length. \
                     These files have no captured source, so text past the cap is not \
                     retrievable at all.",
            source: "indexer/document.rs:PAGE_TEXT_CAP",
        },
        Limit {
            id: "document_page_name",
            label: "Document page title",
            value: crate::indexer::document::NAME_CAP as u64,
            unit: "bytes",
            stage: Stage::Index,
            extensions: DOCUMENT_EXTS,
            effect: "How much of a page's first line becomes the node's name.",
            source: "indexer/document.rs:NAME_CAP",
        },
        Limit {
            id: "node_comments",
            label: "Extracted comments per node",
            value: crate::storage::comments::MAX_COMMENT_CHARS as u64,
            unit: "chars",
            stage: Stage::Embed,
            extensions: CODE_EXTS,
            effect: "Prose comments lifted from a symbol's body, after filtering out \
                     commented-out code, banners and tool directives.",
            source: "storage/comments.rs:MAX_COMMENT_CHARS",
        },
        Limit {
            id: "related_names",
            label: "Related names per node",
            value: crate::storage::text::MAX_RELATED as u64,
            unit: "names",
            stage: Stage::Embed,
            extensions: &[],
            effect: "Neighbour names folded into the embedding as context. A hub node \
                     with more neighbours embeds only the first 24, alphabetically.",
            source: "storage/text.rs:MAX_RELATED",
        },
        Limit {
            id: "sparse_dimensions",
            label: "Keyword dimensions per node",
            value: crate::storage::text::MAX_SPARSE_DIMS as u64,
            unit: "dimensions",
            stage: Stage::Embed,
            extensions: &[],
            effect: "Distinct terms kept in the keyword vector, heaviest first. A long \
                     file loses its rarest terms from keyword search.",
            source: "storage/text.rs:MAX_SPARSE_DIMS",
        },
        Limit {
            id: "search_context_chars",
            label: "Search result budget",
            value: crate::storage::query::DEFAULT_CONTEXT_CHARS as u64,
            unit: "chars",
            stage: Stage::Retrieve,
            extensions: &[],
            effect: "Total snippet text one search returns before it stops adding \
                     results. Per-call overridable; no re-index needed to change it.",
            source: "storage/query.rs:DEFAULT_CONTEXT_CHARS",
        },
    ]
}

/// Maximum input length, in tokens, of a known embedding model — the cap
/// that binds *above* every cap in [`all`], and the one users cannot see
/// because the tokenizer applies it silently.
///
/// Values are the model cards' published max sequence length, keyed by the
/// same aliases [`crate::storage::embed_local`] resolves. Unknown or remote
/// models return `None`: reporting a guess would be worse than reporting
/// nothing, since the whole point is to tell a user where their text stops
/// counting.
///
/// Keep in sync with `resolve_model` in `storage/embed_local.rs` when adding
/// a model there.
pub fn model_token_window(model: &str) -> Option<u32> {
    let lowered = model.trim().to_ascii_lowercase();
    let canon = lowered.rsplit('/').next().unwrap_or(&lowered);
    match canon {
        "bge-small-en-v1.5" | "bge-small-en" | "bge-small" | "bge-base-en-v1.5" | "bge-base-en"
        | "bge-base" | "bge-large-en-v1.5" | "bge-large-en" | "bge-large"
        | "bge-small-zh-v1.5" | "bge-small-zh" => Some(512),
        "multilingual-e5-small" | "e5-small" | "multilingual-e5-base" | "e5-base"
        | "multilingual-e5-large" | "e5-large" => Some(512),
        "mxbai-embed-large-v1" | "mxbai-large" | "mxbai" => Some(512),
        "nomic-embed-text-v1.5" | "nomic-embed" | "nomic" | "nomic-embed-text-v1" => Some(8192),
        "jina-embeddings-v2-base-code" | "jina-code" => Some(8192),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_limit_is_populated_and_uniquely_keyed() {
        let all = all();
        let mut ids: Vec<&str> = all.iter().map(|l| l.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "ids must be unique — clients key on them");

        for l in &all {
            assert!(l.value > 0, "{} has no value", l.id);
            assert!(!l.effect.is_empty(), "{} needs a user-facing effect", l.id);
            assert!(l.source.contains(".rs:"), "{} must cite its constant", l.id);
        }
    }

    #[test]
    fn published_values_track_the_real_constants() {
        // The point of this module is that it cannot drift. If someone
        // changes a cap and not its entry, this fails.
        let by_id = |id: &str| all().into_iter().find(|l| l.id == id).unwrap().value;
        assert_eq!(
            by_id("markdown_section_text"),
            crate::indexer::languages::markdown::SECTION_TEXT_CAP as u64
        );
        assert_eq!(
            by_id("related_names"),
            crate::storage::text::MAX_RELATED as u64
        );
    }

    #[test]
    fn the_default_model_window_is_known() {
        // bge-small is the default; if its window ever stops resolving, the
        // UI silently loses the one cap that binds above all the others.
        assert_eq!(model_token_window("BAAI/bge-small-en-v1.5"), Some(512));
        assert_eq!(model_token_window("bge-small"), Some(512));
        assert_eq!(model_token_window("some-private-model"), None);
    }
}
