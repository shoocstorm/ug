//! Text shaping for node embeddings.
//!
//! The format follows the spec in docs/GRAPH-STORAGE.md:
//! `"{type}: {name}. {description}. Related: {list_of_related_names}"`
//!
//! `description` falls back to the docstring; `related` is the union of
//! neighbour node names reachable via any edge (in either direction). We
//! cap related names so a hub node like `index.ts` doesn't blow the
//! embedding context.
//!
//! Folder nodes carry no docstring at index time. Pre-enrichment we
//! synthesize a description from the folder's classification and language
//! breakdown so the embedding still has retrieval signal; once the
//! Semantic Enrichment phase fills `folder.summary` we prefer that.
//!
//! # What goes in the embedding text, and what doesn't
//!
//! Measured on a representative graph: ~47% of nodes have neither a
//! docstring nor a signature, and the median `node_text` uses about 6% of
//! the embedder's 512-token window (p99 ~200 tokens). So the constraint
//! here is *signal*, not budget — there is ample room, and the job is to
//! find things worth putting in it.
//!
//! Source code is deliberately excluded. Embedding bodies would defeat
//! incremental re-ingest (this text is otherwise whitespace- and
//! body-independent, so reformatting costs zero re-embeds), ~10% of bodies
//! overflow the window and get silently truncated, and body tokens dilute
//! the docstring/name signal. Code belongs in the sparse index and in a
//! stored column, not in the dense vector.
//!
//! What fills that room, in the order it was added:
//!
//! 1. **Identifier splitting** — [`split_identifier`]. The name is the
//!    whole description for the 47%, so it is split into words alongside
//!    the exact form.
//! 2. **Structural synthesis** — [`synthesize_code_synopsis`], the code
//!    counterpart of the folder synopsis. Contributes the file path, which
//!    is real prose signal that appeared nowhere in the text before.
//! 3. **Inline comments** — [`build_node_text_with_comments`], filtered by
//!    [`crate::storage::comments`]. Usually the only natural language a
//!    symbol has, written in the vocabulary people query in.
//!
//! Still open: **LLM summarization**, deliberately deferred. If added it
//! should be opt-in (`ug gen --enrich`) and cached on the code's content
//! hash, so unchanged bodies reuse their summary and the incremental path
//! keeps working.

use crate::types::{GraphData, GraphNode, GraphNodeFolderMeta, GraphNodeType};
use std::collections::HashMap;

/// Cap on related-name fan-out per node. Embedding context is bounded; a
/// hub node with thousands of edges would otherwise dominate every query.
const MAX_RELATED: usize = 24;

/// Split an identifier into its constituent words.
///
/// `buildSparseKeywordVector` → `["build", "sparse", "keyword", "vector"]`,
/// `Db::upsert_nodes` → `["db", "upsert", "nodes"]`,
/// `parseXMLDocument` → `["parse", "xml", "document"]`.
///
/// Handles three boundary kinds: non-alphanumeric separators, lower→upper
/// transitions (camelCase), and acronym→word runs (`XMLDoc` splits before
/// the `D`, not after the `X`). Digits stay attached to the letters they
/// follow, so `utf8` survives as one word rather than becoming `utf` + `8`.
///
/// Why this matters: 47% of nodes in a typical graph carry no docstring
/// and no signature, so the identifier *is* the entire description. Left
/// unsplit, a camelCase name is one opaque token that no natural-language
/// query can reach — and the sparse tokenizer's own comment already
/// assumed ident splitting was happening when it wasn't.
pub fn split_identifier(ident: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = ident.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        if !ch.is_alphanumeric() {
            if !buf.is_empty() {
                words.push(std::mem::take(&mut buf));
            }
            continue;
        }
        if ch.is_uppercase() && !buf.is_empty() {
            let prev = chars[i - 1];
            // lower→upper is always a boundary (`buildSparse`). upper→upper
            // is only a boundary when the *next* char is lower, which marks
            // the last capital as the start of a new word (`XMLDoc`).
            let next_is_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_is_lower) {
                words.push(std::mem::take(&mut buf));
            }
        }
        for c in ch.to_lowercase() {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        words.push(buf);
    }
    words
}

/// Human-readable rendering of an identifier: its words, space-joined.
/// Returns `None` when splitting yields nothing new (single lowercase
/// word), so callers can skip appending a redundant duplicate.
fn humanize_identifier(ident: &str) -> Option<String> {
    let words = split_identifier(ident);
    if words.len() < 2 {
        return None;
    }
    let joined = words.join(" ");
    if joined == ident.to_lowercase() {
        return None;
    }
    Some(joined)
}

pub fn build_node_text(node: &GraphNode, related_names: &[String]) -> String {
    build_node_text_with_comments(node, related_names, "")
}

/// As [`build_node_text`], plus prose lifted from the node's own source
/// comments.
///
/// Inline comments are usually the only natural language attached to an
/// undocumented symbol, and they are written in the vocabulary people
/// query in — which is exactly what a dense embedding can use and what a
/// bare identifier cannot supply. See [`crate::storage::comments`] for what
/// gets filtered out before it reaches here.
///
/// The cost is churn: editing a comment now changes the text and triggers
/// a re-embed of that node. That is the correct outcome — the node's
/// described meaning did change — but it does raise the price of a
/// comment-only edit from zero.
pub fn build_node_text_with_comments(
    node: &GraphNode,
    related_names: &[String],
    comments: &str,
) -> String {
    let kind = format!("{:?}", node.node_type);

    // For folders, prefer the full path over the basename so the embedding
    // text disambiguates same-named folders (`tests/components` vs
    // `src/components`). Other node types already encode location elsewhere.
    let name = match (&node.node_type, node.folder.as_ref()) {
        (GraphNodeType::Folder, Some(_)) => folder_path_from_id(&node.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| node.name.clone()),
        _ => node.name.clone(),
    };

    let description = node_description(node);

    let related = if related_names.is_empty() {
        String::new()
    } else {
        related_names.join(", ")
    };

    // The exact identifier stays first so exact-name queries still match
    // it verbatim; the split form rides alongside in parentheses so a
    // natural-language query ("build sparse keyword vector") can reach a
    // node whose only signal is its name. Omitted when splitting adds
    // nothing, to avoid embedding the same word twice.
    //
    // Deliberately *not* applied to `related` — those names are context
    // rather than primary signal, and splitting all 24 of them would
    // roughly triple the text while diluting the node's own terms.
    let name_field = match humanize_identifier(&name) {
        Some(words) => format!("{} ({})", name, words),
        None => name,
    };

    let notes = if comments.trim().is_empty() {
        String::new()
    } else {
        format!(" Notes: {}.", comments.trim())
    };

    format!(
        "{}: {}. {}.{} Related: {}",
        kind, name_field, description, notes, related
    )
}

/// Per-node description used inside the embedding text. Falls through:
/// 1. `folder.summary` for folder nodes once enrichment fills it
/// 2. `docstring` for any node that has one
/// 3. synthesized folder synopsis from classification + breakdown + counts
/// 4. empty string for everything else (matches old behaviour)
fn node_description(node: &GraphNode) -> String {
    if let Some(meta) = &node.folder {
        if let Some(summary) = meta.summary.as_ref() {
            let trimmed = summary.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    // Docstring and signature both carry retrieval signal — join whatever
    // is present so typed-but-undocumented symbols still embed usefully.
    let mut parts: Vec<String> = Vec::new();
    if let Some(doc) = &node.docstring {
        let trimmed = doc.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    if let Some(sig) = &node.signature {
        let rendered = render_signature(sig);
        if !rendered.is_empty() {
            parts.push(format!("Signature: {rendered}"));
        }
    }
    if !parts.is_empty() {
        return parts.join(". ");
    }

    if matches!(node.node_type, GraphNodeType::Folder) {
        if let Some(meta) = &node.folder {
            return synthesize_folder_synopsis(meta);
        }
    }

    // Nothing written about this node — fall back to what the graph knows
    // structurally rather than embedding an empty description.
    synthesize_code_synopsis(node)
}

/// Render `(name?: type, ...) -> return` from a node's structured signature.
/// Empty when the signature carries no information at all.
fn render_signature(sig: &crate::types::GraphNodeSignature) -> String {
    if sig.params.is_empty() && sig.return_type.is_none() {
        return String::new();
    }
    let params: Vec<String> = sig
        .params
        .iter()
        .map(|p| {
            let mut s = p.name.clone();
            if p.optional {
                s.push('?');
            }
            if let Some(t) = &p.param_type {
                s.push_str(": ");
                s.push_str(t);
            }
            if let Some(d) = &p.default {
                s.push_str(" = ");
                s.push_str(d);
            }
            s
        })
        .collect();
    let mut out = format!("({})", params.join(", "));
    if let Some(ret) = &sig.return_type {
        out.push_str(" -> ");
        out.push_str(ret);
    }
    out
}

/// Build a one-line description from a folder's structural metadata. Used
/// pre-enrichment so the folder node still carries retrieval signal.
/// Example output: "components folder, 8 typescript and 2 markdown files
/// (depth 2)".
/// Cap on names listed in a synthesized synopsis, so a hub node's
/// relations don't crowd out its own terms.
const MAX_SYNOPSIS_NAMES: usize = 8;

/// Describe a code node that has neither a docstring nor a signature,
/// using structure the graph already knows.
///
/// This exists because ~47% of nodes in a typical graph reach the embedder
/// with an empty description — their text is a bare `"Function: name. .
/// Related: …"`. Folder nodes already got this treatment (see
/// [`synthesize_folder_synopsis`]); this extends the same idea to code.
///
/// The file path is the valuable part: it is real natural-language signal
/// ("storage", "indexer", "backends") that appeared nowhere in the
/// embedding text before, and it is stable across edits.
///
/// `calls` is deliberately left out even though the node carries it. The
/// `Related:` list already names those neighbours, and call lists churn on
/// every body edit — folding them in here would re-embed undocumented
/// functions each time their body changed, for signal that is already
/// present.
fn synthesize_code_synopsis(node: &GraphNode) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(file) = node.file.as_deref().filter(|f| !f.is_empty()) {
        parts.push(format!("defined in {}", file));
    }
    if !node.extends.is_empty() {
        parts.push(format!("extends {}", join_capped(&node.extends)));
    }
    if !node.implements.is_empty() {
        parts.push(format!("implements {}", join_capped(&node.implements)));
    }

    parts.join("; ")
}

fn join_capped(names: &[String]) -> String {
    names
        .iter()
        .take(MAX_SYNOPSIS_NAMES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

fn synthesize_folder_synopsis(meta: &GraphNodeFolderMeta) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(class) = meta.classification.as_ref() {
        parts.push(format!("{} folder", classification_label(class)));
    } else if meta.depth == 0 {
        parts.push("project root".to_string());
    } else {
        parts.push("folder".to_string());
    }

    if meta.total_files > 0 {
        parts.push(format_breakdown(meta));
    }

    parts.push(format!("depth {}", meta.depth));

    parts.join(", ")
}

fn classification_label(class: &crate::types::FolderClassification) -> &'static str {
    use crate::types::FolderClassification::*;
    match class {
        Source => "source",
        Tests => "tests",
        Documentation => "documentation",
        Examples => "examples",
        Config => "config",
        Assets => "assets",
        Components => "components",
        Pages => "pages",
        Hooks => "hooks",
        Services => "services",
        Contexts => "contexts",
        Reducers => "reducers",
        Utils => "utils",
        Types => "types",
        Mixed => "mixed",
    }
}

/// Format the language breakdown like "8 typescript and 2 markdown files".
/// When the breakdown is empty (extension we don't recognise), falls back to
/// just the file count.
fn format_breakdown(meta: &GraphNodeFolderMeta) -> String {
    if meta.language_breakdown.is_empty() {
        return format!("{} files", meta.total_files);
    }
    let mut entries: Vec<(&String, &u32)> = meta.language_breakdown.iter().collect();
    // Largest-first so the dominant language leads. Stable on ties via name.
    entries.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let labelled: Vec<String> = entries
        .iter()
        .map(|(lang, count)| format!("{} {}", count, lang))
        .collect();
    format!("{} files", labelled.join(" and "))
}

/// Strip the `folder:` prefix from a folder node ID. Returns `None` if the ID
/// doesn't carry that prefix - shouldn't happen for a Folder node, but the
/// caller falls back to the basename in that case.
fn folder_path_from_id(id: &str) -> Option<&str> {
    id.strip_prefix("folder:")
}

/// Build a `node_id -> [neighbour names]` map by walking every edge in
/// `graph`. Both endpoints of an edge contribute to each other so the
/// embedded text reflects bidirectional context.
pub fn collect_related_names(graph: &GraphData) -> HashMap<String, Vec<String>> {
    let id_to_name: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect();

    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for edge in &graph.edges {
        if let Some(target_name) = id_to_name.get(edge.target.as_str()) {
            out.entry(edge.source.clone())
                .or_default()
                .push(target_name.to_string());
        }
        if let Some(source_name) = id_to_name.get(edge.source.as_str()) {
            out.entry(edge.target.clone())
                .or_default()
                .push(source_name.to_string());
        }
    }

    for v in out.values_mut() {
        v.sort();
        v.dedup();
        if v.len() > MAX_RELATED {
            v.truncate(MAX_RELATED);
        }
    }

    out
}

/// Build a sparse keyword vector for OverGraph's hybrid search. The
/// dimension space is the FNV-1a hash of each lowercase alphanumeric
/// token; the weight is term frequency. The same hash + tokenizer are
/// used at ingest and at query time so tokens collide deterministically.
///
/// This is the v1 FTS replacement (MIGRATION-OVERGRAPH §3.3 Option 1).
/// It has no IDF — common words count as much as rare ones — but for
/// code symbol queries (mostly distinctive identifiers) it gives
/// roughly BM25-equivalent recall with zero extra dependencies.
pub fn build_sparse_keyword_vector(text: &str) -> Vec<(u32, f32)> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut weights: HashMap<u32, f32> = HashMap::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            buf.push(ch);
        } else if !buf.is_empty() {
            emit_run(&buf, &mut weights);
            buf.clear();
        }
    }
    if !buf.is_empty() {
        emit_run(&buf, &mut weights);
    }
    // Sort by dimension id so output is deterministic and matches the
    // canonical form OverGraph expects for sparse vectors.
    let mut out: Vec<(u32, f32)> = weights.into_iter().collect();
    out.sort_unstable_by_key(|&(dim, _)| dim);
    out
}

/// Weight applied to tokens coming from a node's source body, relative to
/// the 1.0 of its name/docstring/signature text.
///
/// Bodies are far longer than the descriptive text, so at equal weight
/// their tokens would swamp the vector — and this tokenizer has no IDF to
/// discount the boilerplate (`let`, `self`, `return`) that dominates them.
/// Discounting keeps code reachable by keyword without letting it outvote
/// the terms that actually name the node.
const CODE_TOKEN_WEIGHT: f32 = 0.35;

/// Cap on dimensions kept per node. A large function tokenizes into
/// thousands of terms; without a cap the sparse index would grow faster
/// than the dense one it is supposed to complement.
const MAX_SPARSE_DIMS: usize = 512;

/// Sparse vector for a node: its embedding text at full weight, plus its
/// source body at [`CODE_TOKEN_WEIGHT`].
///
/// This is the counterpart to the dense vector, and the reason source code
/// is *not* embedded densely: the sparse side has no token-window limit,
/// gives exact identifier matches, and doesn't dilute the semantic signal
/// that names and docstrings carry.
pub fn build_node_sparse_vector(node_text: &str, code: &str) -> Vec<(u32, f32)> {
    let mut weights: HashMap<u32, f32> = HashMap::new();
    accumulate_tokens(node_text, 1.0, &mut weights);
    if !code.is_empty() {
        accumulate_tokens(code, CODE_TOKEN_WEIGHT, &mut weights);
    }

    let mut out: Vec<(u32, f32)> = weights.into_iter().collect();
    if out.len() > MAX_SPARSE_DIMS {
        // Keep the heaviest terms, then restore dimension order —
        // OverGraph expects sparse vectors sorted by dimension id.
        out.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(MAX_SPARSE_DIMS);
    }
    out.sort_unstable_by_key(|&(dim, _)| dim);
    out
}

/// Tokenize `text` and add each token's frequency, scaled by `weight`.
fn accumulate_tokens(text: &str, weight: f32, out: &mut HashMap<u32, f32>) {
    let mut scratch: HashMap<u32, f32> = HashMap::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            buf.push(ch);
        } else if !buf.is_empty() {
            emit_run(&buf, &mut scratch);
            buf.clear();
        }
    }
    if !buf.is_empty() {
        emit_run(&buf, &mut scratch);
    }
    for (dim, w) in scratch {
        *out.entry(dim).or_insert(0.0) += w * weight;
    }
}

/// Emit one alphanumeric run: the run itself, plus its constituent words
/// when it is a compound identifier.
///
/// Both forms are emitted so the two query styles keep working against
/// one index: an exact identifier query (`buildSparseKeywordVector`)
/// still lands on the whole-run dimension and scores it on top of the
/// word dimensions, while a prose query reaches the words alone. Ingest
/// and query share this function, so the dimension space stays
/// consistent by construction.
fn emit_run(run: &str, out: &mut HashMap<u32, f32>) {
    let lowered = run.to_lowercase();
    emit_token(&lowered, out);
    let words = split_identifier(run);
    if words.len() > 1 {
        for w in words {
            emit_token(&w, out);
        }
    }
}

fn emit_token(token: &str, out: &mut HashMap<u32, f32>) {
    // 2-char minimum filters out one-letter accidental tokens from
    // ident splits. 32-char cap stops pathological URLs / hashes from
    // dominating the vector.
    let len = token.len();
    if !(2..=32).contains(&len) {
        return;
    }
    let dim = fnv1a_u32(token.as_bytes());
    *out.entry(dim).or_insert(0.0) += 1.0;
}

/// Reciprocal Rank Fusion of two ranked lists. The standard RRF
/// constant `c = 60` (Cormack et al., 2009) — small enough to keep
/// rank-1 hits dominant, large enough that ties don't collapse to zero.
/// Output is sorted by fused score descending; output length is
/// capped at `k`. Both backends use this for hybrid search:
/// OverGraph's native fusion plus Neo4j's app-side fusion of vector +
/// full-text results.
pub fn reciprocal_rank_fusion(
    left: Vec<(crate::storage::db::NodeRow, f32)>,
    right: Vec<(crate::storage::db::NodeRow, f32)>,
    k: usize,
) -> Vec<(crate::storage::db::NodeRow, f32)> {
    const C: f32 = 60.0;
    let mut scored: HashMap<String, (crate::storage::db::NodeRow, f32)> = HashMap::new();
    for (rank, (row, _)) in left.into_iter().enumerate() {
        let s = 1.0 / (C + rank as f32 + 1.0);
        let id = row.id.clone();
        scored.entry(id).or_insert((row, 0.0)).1 += s;
    }
    for (rank, (row, _)) in right.into_iter().enumerate() {
        let s = 1.0 / (C + rank as f32 + 1.0);
        let id = row.id.clone();
        scored.entry(id).or_insert((row, 0.0)).1 += s;
    }
    let mut out: Vec<(crate::storage::db::NodeRow, f32)> = scored.into_values().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    out
}

/// FNV-1a 32-bit hash. Mirrors the algorithm OverGraph uses internally
/// (its `fnv1a` is 64-bit but the principle is identical) so test
/// fixtures can predict dimension ids.
fn fnv1a_u32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod sparse_tests {
    use super::*;

    #[test]
    fn tokenizer_lowercases_and_splits() {
        let v = build_sparse_keyword_vector("Hello, World! Hello.");
        // "hello" twice, "world" once → 2 distinct dimensions
        assert_eq!(v.len(), 2);
        let total: f32 = v.iter().map(|(_, w)| *w).sum();
        assert_eq!(total, 3.0);
    }

    #[test]
    fn empty_text_yields_empty_vector() {
        assert!(build_sparse_keyword_vector("").is_empty());
        assert!(build_sparse_keyword_vector("!!!").is_empty());
    }

    #[test]
    fn deterministic_dim_ids() {
        let a = build_sparse_keyword_vector("foo bar");
        let b = build_sparse_keyword_vector("foo bar");
        assert_eq!(a, b);
    }

    #[test]
    fn splits_camel_snake_and_acronyms() {
        assert_eq!(
            split_identifier("buildSparseKeywordVector"),
            ["build", "sparse", "keyword", "vector"]
        );
        assert_eq!(split_identifier("Db::upsert_nodes"), ["db", "upsert", "nodes"]);
        assert_eq!(split_identifier("parseXMLDocument"), ["parse", "xml", "document"]);
        assert_eq!(split_identifier("HTTPServer"), ["http", "server"]);
        assert_eq!(split_identifier("plain"), ["plain"]);
        assert_eq!(split_identifier(""), Vec::<String>::new());
    }

    #[test]
    fn digits_stay_with_the_word_they_follow() {
        assert_eq!(split_identifier("utf8Decode"), ["utf8", "decode"]);
        assert_eq!(split_identifier("blake3"), ["blake3"]);
    }

    #[test]
    fn compound_identifier_is_reachable_by_prose_and_by_exact_name() {
        let doc = build_sparse_keyword_vector("buildSparseKeywordVector");
        let prose = build_sparse_keyword_vector("build sparse keyword vector");
        let exact = build_sparse_keyword_vector("buildSparseKeywordVector");

        let dims = |v: &Vec<(u32, f32)>| v.iter().map(|(d, _)| *d).collect::<Vec<_>>();
        // Prose query overlaps every word dimension...
        for d in dims(&prose) {
            assert!(dims(&doc).contains(&d), "prose term {d} missing from doc");
        }
        // ...and the exact identifier still matches the whole-run dimension.
        assert_eq!(dims(&doc), dims(&exact));
        assert!(
            doc.len() > prose.len(),
            "doc carries the compound dimension on top of the word ones"
        );
    }

    #[test]
    fn code_tokens_are_searchable_but_outweighed_by_the_node_text() {
        let text = "Function: parseConfig. Reads settings.";
        let code = "fn parseConfig() { let raw = fs::read(); toml::from_str(&raw) }";

        let with_code = build_node_sparse_vector(text, code);
        let without = build_node_sparse_vector(text, "");

        let dim = |t: &str| {
            let v = build_sparse_keyword_vector(t);
            v[0].0
        };
        let weight_of = |v: &Vec<(u32, f32)>, d: u32| {
            v.iter().find(|(x, _)| *x == d).map(|(_, w)| *w)
        };

        // A term that appears only in the body is reachable...
        let toml = dim("toml");
        assert!(weight_of(&without, toml).is_none(), "not in the text alone");
        assert!(weight_of(&with_code, toml).is_some(), "body term indexed");

        // ...but weighs less than a term from the node's own text.
        let parse = dim("parse");
        assert!(
            weight_of(&with_code, parse).unwrap() > weight_of(&with_code, toml).unwrap(),
            "descriptive terms must outrank body terms"
        );
    }

    #[test]
    fn sparse_vector_is_capped_and_dimension_sorted() {
        // A body far larger than any real function.
        let code: String = (0..5000).map(|i| format!("ident{} ", i)).collect();
        let v = build_node_sparse_vector("Function: big.", &code);

        assert!(v.len() <= 512, "capped, got {}", v.len());
        assert!(
            v.windows(2).all(|w| w[0].0 < w[1].0),
            "OverGraph requires ascending dimension ids"
        );
    }

    #[test]
    fn humanized_name_lands_in_node_text() {
        // A node with no docstring and no signature — the 47% case, where
        // the identifier is the only retrieval signal there is.
        let node = GraphNode {
            id: "function:src/a.ts:1:buildSparseKeywordVector".into(),
            name: "buildSparseKeywordVector".into(),
            node_type: GraphNodeType::Function,
            file: Some("src/a.ts".into()),
            start_line: Some(1),
            end_line: Some(9),
            metrics: None,
            signature: None,
            docstring: None,
            imports: Vec::new(),
            exports: Vec::new(),
            extends: Vec::new(),
            implements: Vec::new(),
            calls: Vec::new(),
            folder: None,
        };
        let text = build_node_text(&node, &[]);
        assert!(text.contains("buildSparseKeywordVector"), "exact name kept: {text}");
        assert!(text.contains("build sparse keyword vector"), "words added: {text}");
    }

    #[test]
    fn undocumented_node_falls_back_to_structural_synopsis() {
        let mut n = GraphNode {
            id: "class:src/net/pool.ts:ConnectionPool".into(),
            name: "ConnectionPool".into(),
            node_type: GraphNodeType::Class,
            file: Some("src/net/pool.ts".into()),
            start_line: Some(1),
            end_line: Some(40),
            metrics: None,
            signature: None,
            docstring: None,
            imports: vec![],
            exports: vec![],
            extends: vec!["BasePool".into()],
            implements: vec!["Closeable".into()],
            calls: vec!["connect".into()],
            folder: None,
        };
        let text = build_node_text(&n, &[]);
        assert!(text.contains("defined in src/net/pool.ts"), "path is signal: {text}");
        assert!(text.contains("extends BasePool"), "{text}");
        assert!(text.contains("implements Closeable"), "{text}");
        assert!(
            !text.contains("calls connect"),
            "call lists churn per edit and Related: already covers them: {text}"
        );

        // A real docstring must win outright — the synopsis is a fallback.
        n.docstring = Some("Pools TCP connections.".into());
        let documented = build_node_text(&n, &[]);
        assert!(documented.contains("Pools TCP connections."));
        assert!(
            !documented.contains("defined in"),
            "synopsis must not dilute a written description: {documented}"
        );
    }

    #[test]
    fn already_readable_names_are_not_duplicated() {
        let node = GraphNode {
            id: "concept:docs/a.md:1:overview".into(),
            name: "overview".into(),
            node_type: GraphNodeType::Concept,
            file: Some("docs/a.md".into()),
            start_line: Some(1),
            end_line: Some(2),
            metrics: None,
            signature: None,
            docstring: None,
            imports: Vec::new(),
            exports: Vec::new(),
            extends: Vec::new(),
            implements: Vec::new(),
            calls: Vec::new(),
            folder: None,
        };
        let text = build_node_text(&node, &[]);
        assert!(!text.contains("("), "no redundant parenthetical: {text}");
    }
}
