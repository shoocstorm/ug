//! Every cap that shapes what reaches the embedder or the agent, in one
//! machine-readable list.
//!
//! These caps are not incidental — they decide what a node's vector can
//! possibly match on. A description longer than [`EmbedBudget`] embeds only
//! its opening paragraphs; a node with more neighbours than
//! [`MAX_RELATED`][rel] embeds only the first of them. A user who does not
//! know that reads a search miss as "semantic search is bad" rather than
//! "that sentence was past the cap".
//!
//! So the numbers are published: `/api/capabilities` serves this list and
//! the visualization's Chunk tab shows which of them applied to the node on
//! screen, and which actually bit.
//!
//! Every entry in [`all`] reads the constant that actually enforces the
//! behaviour, so a published number cannot drift from the real one. The one
//! exception is [`EmbedBudget`], which is defined here because it is not a
//! constant at all — it is derived from the loaded model's token window.
//! Adding a cap means adding an entry here — see
//! `docs/INDEXING-AND-CHUNKING.md` §6.
//!
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

/// Rough chars-per-token for the text we embed — English prose with
/// identifiers mixed in. Subword tokenizers average ~4 for plain English
/// and less for code; 3.7 keeps the derived budget slightly conservative,
/// which is the right direction to be wrong in when overflow is silent.
const CHARS_PER_TOKEN: f32 = 3.7;

/// Chars of the template that precede the description: the type prefix and
/// the name in both its exact and split forms.
const NAME_RESERVE_CHARS: usize = 150;

/// Typical rendered length of one neighbour name in the `Related:` list,
/// used only to *warn* about the list overflowing the window.
const AVG_RELATED_NAME_CHARS: usize = 20;

/// Chars reserved out of the window for everything that competes with the
/// description.
///
/// Deliberately excludes the `Related:` list. Ordering in the template
/// decides who loses a fight with the tokenizer, and `Related:` comes last
/// — so when the text overflows, neighbour names are what get dropped, not
/// the description. Reserving for them would shrink the description to
/// protect the field that is already the designated casualty.
///
/// `Notes:` *does* sit before `Related:`, so its cap is reserved: it is
/// read from [`MAX_COMMENT_CHARS`] rather than duplicated, so changing one
/// cap cannot silently desynchronise the other.
///
/// [`MAX_COMMENT_CHARS`]: crate::storage::comments::MAX_COMMENT_CHARS
fn template_reserve_chars() -> usize {
    NAME_RESERVE_CHARS + crate::storage::comments::MAX_COMMENT_CHARS
}

/// Floor on the derived budget. Below this a description is too clipped to
/// carry meaning, and a model with a window that small is better rejected
/// than quietly served.
const MIN_DESCRIPTION_CHARS: usize = 500;

/// Ceiling on the derived budget. A long-window model could take far more,
/// but past this the description crowds out nothing and starts costing
/// ingest time and store size for diminishing retrieval value.
const MAX_DESCRIPTION_CHARS: usize = 8_000;

/// Budget used when the active model's window is unknown — an unrecognised
/// local alias, or any remote endpoint. Matches the cap markdown sections
/// were hard-coded to before the budget was derived, so behaviour for those
/// models is unchanged.
const DEFAULT_DESCRIPTION_CHARS: usize = 1_500;

/// Where a resolved budget's number came from. Surfaced so a user can tell
/// "the tool picked this for my model" from "I set this".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSource {
    /// An explicit `--section-cap` / `embed.section_cap` setting.
    Flag,
    /// Derived from the active model's token window.
    Auto,
    /// The model's window is unknown, so the fixed fallback applies.
    Default,
}

/// How much of a node's description may enter the embedding text.
///
/// This is the cap that used to live in the markdown indexer as a
/// hard-coded 1,500 bytes. It moved here for two reasons. It is really a
/// property of the *model* — the number exists only because the embedder
/// truncates at its window — and the indexer runs before any embedder is
/// constructed, so it could not have known the right value even in
/// principle. And as an embed-stage cap it is recomputable: switching to a
/// longer-window model needs a re-embed, not a re-index.
///
/// It applies to every node's description uniformly, which also fixes the
/// PDF case: pages are captured at [`crate::indexer::document::PAGE_TEXT_CAP`]
/// (8 KB) for storage and display, and only what fits the window reaches
/// the vector.
#[derive(Debug, Clone, Copy)]
pub struct EmbedBudget {
    pub description_chars: usize,
    /// The active model's window, when known. `None` means the budget fell
    /// back to [`BudgetSource::Default`].
    pub window_tokens: Option<u32>,
    pub source: BudgetSource,
}

impl Default for EmbedBudget {
    fn default() -> Self {
        Self {
            description_chars: DEFAULT_DESCRIPTION_CHARS,
            window_tokens: None,
            source: BudgetSource::Default,
        }
    }
}

impl EmbedBudget {
    /// Resolve the budget for `model`, with `override_chars` winning when
    /// set (an explicit `--section-cap`).
    ///
    /// The derivation reserves [`template_reserve_chars`] for the parts of
    /// the template that precede the description, then clamps. With the
    /// default 512-token model that lands near the old hard-coded 1,500;
    /// with an 8,192-token model it opens up to the ceiling.
    pub fn resolve(model: &str, override_chars: Option<usize>) -> Self {
        let window = model_token_window(model);
        if let Some(chars) = override_chars.filter(|c| *c > 0) {
            return Self {
                description_chars: chars,
                window_tokens: window,
                source: BudgetSource::Flag,
            };
        }
        match window {
            Some(tokens) => {
                let raw = (tokens as f32 * CHARS_PER_TOKEN) as usize;
                let usable = raw.saturating_sub(template_reserve_chars());
                Self {
                    description_chars: usable.clamp(MIN_DESCRIPTION_CHARS, MAX_DESCRIPTION_CHARS),
                    window_tokens: Some(tokens),
                    source: BudgetSource::Auto,
                }
            }
            None => Self::default(),
        }
    }

    /// Advice for the operator, or `None` when the budget and the model are
    /// a sensible pair. Rendered by `ug gen` and carried on
    /// `/api/capabilities` — this is the "remind me to adjust" half of
    /// letting users pick their own model.
    pub fn advisory(&self, model: &str) -> Option<String> {
        let window = self.window_tokens?;
        let usable = (window as f32 * CHARS_PER_TOKEN) as usize;
        if self.description_chars + template_reserve_chars() > usable {
            return Some(format!(
                "description budget is {} chars but {} reads only ~{} chars ({} tokens) — \
                 text past that is dropped by the tokenizer with no marker",
                self.description_chars, model, usable, window
            ));
        }
        // Only worth nagging about when the headroom is large enough that
        // raising the cap would actually change what gets embedded.
        if self.source == BudgetSource::Flag && self.description_chars * 2 < usable {
            return Some(format!(
                "{} reads ~{} chars ({} tokens) but the description budget is pinned to {} — \
                 drop --section-cap to use the whole window",
                model, usable, window, self.description_chars
            ));
        }
        None
    }

    /// Whether a full `Related:` list cannot fit the window after the
    /// description, i.e. whether hub nodes are spending `MAX_RELATED` on
    /// names the embedder will never read.
    ///
    /// Separate from [`advisory`] because it reports a different decision:
    /// `MAX_RELATED` is the knob to turn, not `--section-cap`, and the
    /// description is unaffected either way — `Related:` comes last in the
    /// template, so it is what the tokenizer drops.
    ///
    /// [`advisory`]: EmbedBudget::advisory
    pub fn related_advisory(&self) -> Option<String> {
        let window = self.window_tokens?;
        let usable = (window as f32 * CHARS_PER_TOKEN) as usize;
        let related = crate::storage::text::MAX_RELATED * AVG_RELATED_NAME_CHARS;
        let spent = self.description_chars + template_reserve_chars();
        if spent + related <= usable {
            return None;
        }
        let room = usable.saturating_sub(spent) / AVG_RELATED_NAME_CHARS;
        Some(format!(
            "MAX_RELATED is {} but only about {} neighbour names fit the remaining window — \
             `Related:` is last in the template, so the rest are dropped by the tokenizer \
             (the description is unaffected)",
            crate::storage::text::MAX_RELATED, room
        ))
    }
}

const MARKDOWN_EXTS: &[&str] = &["md", "mdx", "markdown"];
const DOCUMENT_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "docm", "dot", "dotm", "dotx", "odt", "ott", "rtf", "xls", "xlsx",
    "xlsm", "xlsb", "ods", "ots", "ppt", "pptx", "pptm", "pot", "potm", "potx", "odp", "otp",
];
const CODE_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "py", "java", "rs"];

/// Every published cap, in pipeline order.
///
/// Takes the resolved [`EmbedBudget`] because one of the caps is no longer
/// a constant — it is derived from the active model's window, so the
/// published list can only be correct if it knows which model is loaded.
pub fn all(budget: &EmbedBudget) -> Vec<Limit> {
    vec![
        Limit {
            id: "description_chars",
            label: "Embedded description",
            value: budget.description_chars as u64,
            unit: "chars",
            stage: Stage::Embed,
            extensions: &[],
            effect: "How much of a node's description reaches the vector — a markdown \
                     section's prose, a PDF page's text, or a doc comment. Past it the \
                     text is still stored and searchable by keyword, but not embedded.",
            source: "limits.rs:EmbedBudget",
        },
        Limit {
            id: "markdown_section_text",
            label: "Markdown section capture",
            value: crate::indexer::languages::markdown::SECTION_TEXT_HARD_CAP as u64,
            unit: "bytes",
            stage: Stage::Index,
            extensions: MARKDOWN_EXTS,
            effect: "How much of a heading's prose is kept at index time. The embedded \
                     slice is the smaller `description_chars`; this cap only bounds what \
                     the graph file carries.",
            source: "indexer/languages/markdown.rs:SECTION_TEXT_HARD_CAP",
        },
        Limit {
            id: "document_page_text",
            label: "Document page capture",
            value: crate::indexer::document::PAGE_TEXT_CAP as u64,
            unit: "bytes",
            stage: Stage::Index,
            extensions: DOCUMENT_EXTS,
            effect: "How much of each PDF/Office page is extracted. These files have no \
                     captured source, so text past this cap is not retrievable at all.",
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
            effect: "Neighbour names folded into the embedding as context, alphabetically. \
                     Sits last in the template, so it is also the first field the \
                     model's token window truncates.",
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
/// same aliases [`crate::storage::embed::local`] resolves. Unknown or remote
/// models return `None`: reporting a guess would be worse than reporting
/// nothing, since the whole point is to tell a user where their text stops
/// counting.
///
/// Keep in sync with `resolve_model` in `storage/embed/local.rs` when adding
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
        let all = all(&EmbedBudget::default());
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
        let budget = EmbedBudget::default();
        let by_id = |id: &str| all(&budget).into_iter().find(|l| l.id == id).unwrap().value;
        assert_eq!(
            by_id("markdown_section_text"),
            crate::indexer::languages::markdown::SECTION_TEXT_HARD_CAP as u64
        );
        assert_eq!(
            by_id("related_names"),
            crate::storage::text::MAX_RELATED as u64
        );
        assert_eq!(by_id("description_chars"), budget.description_chars as u64);
    }

    #[test]
    fn the_default_model_window_is_known() {
        // bge-small is the default; if its window ever stops resolving, the
        // UI silently loses the one cap that binds above all the others.
        assert_eq!(model_token_window("BAAI/bge-small-en-v1.5"), Some(512));
        assert_eq!(model_token_window("bge-small"), Some(512));
        assert_eq!(model_token_window("some-private-model"), None);
    }

    #[test]
    fn budget_tracks_the_model_window() {
        // The 512-token default lands near the 1,500 the markdown indexer
        // used to hard-code — the derivation replaces that number without
        // moving it much.
        let small = EmbedBudget::resolve("bge-small-en-v1.5", None);
        assert_eq!(small.source, BudgetSource::Auto);
        assert_eq!(small.window_tokens, Some(512));
        assert!(
            (1_000..=1_600).contains(&small.description_chars),
            "got {}",
            small.description_chars
        );
        // And it must actually fit, reserve included — the whole point.
        let usable = (512.0 * CHARS_PER_TOKEN) as usize;
        assert!(small.description_chars + template_reserve_chars() <= usable);
        assert!(small.advisory("bge-small-en-v1.5").is_none());

        // A long-window model opens up to the ceiling.
        let long = EmbedBudget::resolve("nomic-embed-text-v1.5", None);
        assert_eq!(long.description_chars, MAX_DESCRIPTION_CHARS);

        // An unknown or remote model keeps the previous fixed behaviour.
        let unknown = EmbedBudget::resolve("some-private-model", None);
        assert_eq!(unknown.source, BudgetSource::Default);
        assert_eq!(unknown.description_chars, DEFAULT_DESCRIPTION_CHARS);
    }

    #[test]
    fn an_override_wins_and_is_labelled_as_such() {
        let b = EmbedBudget::resolve("bge-small-en-v1.5", Some(4_000));
        assert_eq!(b.description_chars, 4_000);
        assert_eq!(b.source, BudgetSource::Flag);
        // ...and is called out, because 4,000 chars is well past what a
        // 512-token model can read.
        let advice = b.advisory("bge-small-en-v1.5").expect("over-window is advised");
        assert!(advice.contains("dropped by the tokenizer"), "{advice}");
    }

    #[test]
    fn a_large_related_cap_is_reported_against_a_small_window() {
        // MAX_RELATED is a compile-time choice, so this asserts the
        // relationship rather than a fixed verdict: whether the advisory
        // fires must agree with whether the names actually fit.
        let small = EmbedBudget::resolve("bge-small-en-v1.5", None);
        let usable = (512.0 * CHARS_PER_TOKEN) as usize;
        let needed = small.description_chars
            + template_reserve_chars()
            + crate::storage::text::MAX_RELATED * AVG_RELATED_NAME_CHARS;
        assert_eq!(
            small.related_advisory().is_some(),
            needed > usable,
            "advisory must fire exactly when the Related: list overflows"
        );

        // A long-window model has room for the whole list.
        assert!(EmbedBudget::resolve("nomic-embed-text-v1.5", None)
            .related_advisory()
            .is_none());
    }

    #[test]
    fn a_pinned_cap_far_under_a_long_window_is_advised_too() {
        let b = EmbedBudget::resolve("nomic-embed-text-v1.5", Some(1_000));
        let advice = b.advisory("nomic-embed-text-v1.5").expect("under-use is advised");
        assert!(advice.contains("--section-cap"), "{advice}");

        // An auto-derived budget is by construction a good fit — no nag.
        assert!(EmbedBudget::resolve("nomic-embed-text-v1.5", None)
            .advisory("nomic-embed-text-v1.5")
            .is_none());
    }
}
