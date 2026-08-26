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

use crate::indexer::common::truncate_chars;
use crate::limits::EmbedBudget;
use crate::storage::sparse_stats::{saturate_tf, SparseStats};
use crate::types::{GraphData, GraphNode, GraphNodeFolderMeta, GraphNodeType};
use std::collections::HashMap;

/// Cap on related-name fan-out per node. Embedding context is bounded; a
/// hub node with thousands of edges would otherwise dominate every query.
pub const MAX_RELATED: usize = 128;

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

/// As [`build_node_text_with_comments`] with no comments and the default
/// budget. Kept for callers that have neither — tests, and the CLI
/// progress-reporting paths.
pub fn build_node_text(node: &GraphNode, related_names: &[&str]) -> String {
    build_node_text_with_comments(node, related_names, "", &EmbedBudget::default())
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
    related_names: &[&str],
    comments: &str,
    budget: &EmbedBudget,
) -> String {
    let kind = node.node_type.as_str();

    // For folders, prefer the full path over the basename so the embedding
    // text disambiguates same-named folders (`tests/components` vs
    // `src/components`). Other node types already encode location elsewhere.
    let name = match (&node.node_type, node.folder.as_ref()) {
        (GraphNodeType::Folder, Some(_)) => folder_path_from_id(&node.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| node.name.clone()),
        _ => node.name.clone(),
    };

    // The description is the one unbounded field — a markdown section's
    // prose or a PDF page's text can run to kilobytes. Capping it here
    // rather than in the indexer means the number can follow the loaded
    // model's window (see [`EmbedBudget`]), and that changing models needs
    // a re-embed rather than a re-index.
    let description = truncate_chars(&node_description(node), budget.description_chars);

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
    // Framework semantics lead, and are added whether or not the symbol is
    // documented: they say what the symbol *is* in a way its prose usually
    // assumes rather than states.
    let semantics = synthesize_framework_semantics(node);
    if !semantics.is_empty() {
        parts.push(semantics);
    }
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

/// Annotations whose meaning is entirely in their name, and the phrase that
/// says it in the words someone would search with.
///
/// This exists because annotation-driven frameworks put a symbol's purpose
/// somewhere the rest of the pipeline can't see it. `@Repository class
/// JdbcOrderStore` is a data-access component, but nothing in the
/// identifier, the (usually absent) Javadoc or the file path says
/// "repository", "data access" or "persistence". The class embeds as a bare
/// name and a path, and a query for "where do we persist orders" doesn't
/// reach it.
///
/// `@Override` is deliberately absent: it is on a large share of all methods
/// and says nothing that distinguishes one from another. The graph records
/// the same fact precisely, as an `Overrides` edge.
const ROLE_ANNOTATIONS: &[(&str, &str)] = &[
    ("RestController", "Spring REST controller"),
    ("Controller", "Spring MVC controller"),
    ("ControllerAdvice", "Spring controller advice, handles exceptions"),
    ("RestControllerAdvice", "Spring controller advice, handles exceptions"),
    ("Service", "Spring service bean"),
    ("Repository", "Spring data access repository"),
    ("Component", "Spring component bean"),
    ("Configuration", "Spring configuration class"),
    ("Bean", "Spring bean factory method"),
    ("FeignClient", "Feign declarative HTTP client"),
    ("Aspect", "AOP aspect"),
    ("Entity", "JPA persistent entity"),
    ("Embeddable", "JPA embeddable value type"),
    ("MappedSuperclass", "JPA mapped superclass"),
    ("Id", "primary key"),
    ("Transactional", "runs in a database transaction"),
    ("Async", "runs asynchronously"),
    ("Cacheable", "result is cached"),
    ("EventListener", "application event listener"),
    ("ExceptionHandler", "exception handler"),
    ("Deprecated", "deprecated"),
    ("Test", "test case"),
    ("ParameterizedTest", "parameterized test case"),
    ("SpringBootTest", "Spring Boot integration test"),
    ("Autowired", "injected dependency"),
    ("Inject", "injected dependency"),
];

/// Annotations whose meaning is in an argument, and the key to read it from.
/// The rendered phrase carries the *value* — a table name, a topic, a cron
/// expression — which is the part a query is likely to contain.
const VALUE_ANNOTATIONS: &[(&str, &[&str], &str)] = &[
    ("Table", &["name"], "mapped to table"),
    ("Column", &["name"], "database column"),
    ("JoinColumn", &["name"], "joined on column"),
    ("Query", &["value"], "query"),
    ("NamedQuery", &["query"], "named query"),
    ("KafkaListener", &["topics"], "consumes Kafka topic"),
    ("RabbitListener", &["queues"], "consumes queue"),
    ("JmsListener", &["destination"], "consumes JMS destination"),
    ("Scheduled", &["cron"], "scheduled on cron"),
    ("Value", &["value"], "configured by"),
    ("Qualifier", &["value"], "qualified as"),
    ("Profile", &["value"], "active in profile"),
    ("ConditionalOnProperty", &["name"], "enabled by property"),
    ("ConfigurationProperties", &["prefix"], "bound to config prefix"),
    ("RequestMapping", &["path", "value"], "base path"),
];

/// Prose for what a node's annotations and route say it is.
///
/// Empty for any node without them, so nothing changes for languages that
/// don't carry annotations.
fn synthesize_framework_semantics(node: &GraphNode) -> String {
    use crate::indexer::languages::java::{first_string_literal, named_or_first_string};

    let mut parts: Vec<String> = Vec::new();

    // The route leads. `GET /api/orders/{id}` is the highest-value string a
    // handler has and appears nowhere else in its text.
    if let Some(route) = node.route.as_deref().filter(|r| !r.is_empty()) {
        parts.push(format!("HTTP endpoint {}", route));
    }

    if node.annotations.is_empty() {
        return parts.join(". ");
    }

    for ann in &node.annotations {
        if let Some((_, phrase)) = ROLE_ANNOTATIONS.iter().find(|(n, _)| *n == ann.name) {
            parts.push((*phrase).to_string());
            continue;
        }
        if let Some((_, keys, phrase)) = VALUE_ANNOTATIONS.iter().find(|(n, _, _)| *n == ann.name) {
            // A route already covers the mapping annotations on a handler;
            // repeating the base path would only dilute it.
            if node.route.is_some() {
                continue;
            }
            let value = ann
                .args
                .as_deref()
                .and_then(|a| named_or_first_string(a, keys).or_else(|| first_string_literal(a)));
            if let Some(v) = value {
                parts.push(format!("{} {}", phrase, v.trim()));
            } else {
                parts.push((*phrase).to_string());
            }
        }
    }

    // Anything unrecognised still gets named. A house annotation
    // (`@AuditLogged`, `@Idempotent`) is a real fact about the symbol, and
    // its identifier splits into searchable words like any other.
    let unknown: Vec<String> = node
        .annotations
        .iter()
        .filter(|a| {
            !ROLE_ANNOTATIONS.iter().any(|(n, _)| *n == a.name)
                && !VALUE_ANNOTATIONS.iter().any(|(n, _, _)| *n == a.name)
                && a.name != "Override"
        })
        .map(|a| match humanize_identifier(&a.name) {
            Some(words) => format!("{} ({})", a.name, words),
            None => a.name.clone(),
        })
        .take(MAX_SYNOPSIS_NAMES)
        .collect();
    if !unknown.is_empty() {
        parts.push(format!("annotated {}", unknown.join(", ")));
    }

    parts.join(". ")
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
pub fn collect_related_names(graph: &GraphData) -> HashMap<&str, Vec<&str>> {
    let id_to_name: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect();

    // Borrowed throughout. Owning the keys and values cost four `String`
    // allocations per edge — ~3 million on a large repo — and most of them
    // were a 141-character id allocated only to hash it against an entry that
    // already existed. Same shape as P10.7 and P11.9. See P11.11 in
    // docs/dev/PERF-TUNING-JOURNEY.md.
    let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        if let Some(target_name) = id_to_name.get(&*edge.target) {
            out.entry(&edge.source).or_default().push(target_name);
        }
        if let Some(source_name) = id_to_name.get(&*edge.source) {
            out.entry(&edge.target).or_default().push(source_name);
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
pub const MAX_SPARSE_DIMS: usize = 512;

/// Sparse vector for a node: its embedding text at full weight, plus its
/// source body at [`CODE_TOKEN_WEIGHT`], with each term's accumulated
/// frequency put through BM25 saturation.
///
/// This is the counterpart to the dense vector, and the reason source code
/// is *not* embedded densely: the sparse side has no token-window limit,
/// gives exact identifier matches, and doesn't dilute the semantic signal
/// that names and docstrings carry.
///
/// # What BM25 contributes, and what it leaves to the query
///
/// Only the *document* half of BM25 lives here — the saturating term
/// frequency, with `b = 0` so no corpus-wide quantity enters and stored
/// vectors stay valid across incremental re-ingests. IDF is applied to the
/// query vector instead (see [`build_sparse_query_vector`]), and the dot
/// product OverGraph computes multiplies the two halves back together.
///
/// `stats` is used for one thing: ranking the truncation when a node has
/// more than [`MAX_SPARSE_DIMS`] terms. Without it the heaviest raw weights
/// win, which selects the *most common* words — precisely backwards. With
/// it, terms are kept by `saturated_tf × idf`, so a long file loses its
/// boilerplate rather than its distinctive identifiers. Passing `None` is
/// safe and reproduces the previous selection.
pub fn build_node_sparse_vector(
    node_text: &str,
    code: &str,
    stats: Option<&SparseStats>,
) -> Vec<(u32, f32)> {
    let mut weights: HashMap<u32, f32> = HashMap::new();
    accumulate_tokens(node_text, 1.0, &mut weights);
    if !code.is_empty() {
        accumulate_tokens(code, CODE_TOKEN_WEIGHT, &mut weights);
    }

    let mut out: Vec<(u32, f32)> = weights
        .into_iter()
        .map(|(dim, tf)| (dim, saturate_tf(tf)))
        .collect();

    if out.len() > MAX_SPARSE_DIMS {
        let keep_score = |dim: u32, w: f32| match stats.filter(|s| !s.is_empty()) {
            Some(s) => w * s.idf(dim),
            None => w,
        };
        // Ties are broken by dimension, and that is what makes this
        // reproducible. `weights` is a `HashMap`, so `out` arrives in an
        // order seeded per map; scores tie heavily (every term seen once
        // scores `saturate_tf(1.0)`), and an unstable sort followed by a
        // truncate then keeps a *different subset* of the tied terms on every
        // run. The graph was reproducible and the keyword index still was
        // not. See P11.10 in docs/dev/PERF-TUNING-JOURNEY.md.
        out.sort_unstable_by(|a, b| {
            keep_score(b.0, b.1)
                .partial_cmp(&keep_score(a.0, a.1))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out.truncate(MAX_SPARSE_DIMS);
    }
    // OverGraph requires ascending dimension ids.
    out.sort_unstable_by_key(|&(dim, _)| dim);
    out
}

/// The query half of BM25: each term weighted by its inverse document
/// frequency, so a query's common words stop counting as much as its rare
/// ones.
///
/// Falls back to the bare term-frequency vector when there are no corpus
/// statistics — a store ingested before the sidecar existed, or a backend
/// that doesn't keep one. Ranking is then what it was before, rather than
/// wrong.
///
/// Query-side weights must also be non-negative: OverGraph canonicalizes
/// the query vector through the same validation as a stored one. See
/// [`SparseStats::idf`] for why that picks the smoothed IDF formula.
pub fn build_sparse_query_vector(query: &str, stats: Option<&SparseStats>) -> Vec<(u32, f32)> {
    let mut v = build_sparse_keyword_vector(query);
    let Some(stats) = stats.filter(|s| !s.is_empty()) else {
        return v;
    };
    for (dim, weight) in v.iter_mut() {
        *weight *= stats.idf(*dim);
    }
    // An IDF of zero is possible only in degenerate corpora, but a
    // zero-weight dimension is dead payload either way.
    v.retain(|&(_, w)| w > 0.0);
    v
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

    // ---- P11.10: the truncation must not depend on HashMap order --------

    /// Over `MAX_SPARSE_DIMS` terms the vector is sorted by score and cut, and
    /// the scores tie heavily — every term seen once scores the same. Without
    /// a deterministic tie-break, *which* tied terms survive depends on the
    /// order a `HashMap` happened to yield, so the same node produced a
    /// different keyword vector on every run.
    ///
    /// Built here from many distinct single-occurrence tokens, which is the
    /// all-ties worst case.
    #[test]
    fn an_over_cap_vector_truncates_to_the_same_terms_every_time() {
        let text: String = (0..MAX_SPARSE_DIMS * 3)
            .map(|i| format!("term{i} "))
            .collect();

        let first = build_node_sparse_vector(&text, "", None);
        assert_eq!(first.len(), MAX_SPARSE_DIMS, "the fixture must exceed the cap");

        // Same input, repeatedly: a fresh `HashMap` each time, so a fresh
        // iteration order each time.
        for _ in 0..8 {
            assert_eq!(
                build_node_sparse_vector(&text, "", None),
                first,
                "truncation picked a different subset of the tied terms"
            );
        }
    }

    /// The same, with the corpus statistics in play — `keep_score` changes,
    /// the tie-break must still hold.
    #[test]
    fn the_tie_break_holds_with_idf_weighting_too() {
        let text: String = (0..MAX_SPARSE_DIMS * 2)
            .map(|i| format!("term{i} "))
            .collect();
        let docs: Vec<Vec<u32>> = (0..4)
            .map(|_| {
                build_node_sparse_vector(&text, "", None)
                    .into_iter()
                    .map(|(d, _)| d)
                    .collect()
            })
            .collect();
        let stats = SparseStats::from_documents(docs.iter().map(|d| d.as_slice()));

        let first = build_node_sparse_vector(&text, "", Some(&stats));
        for _ in 0..8 {
            assert_eq!(build_node_sparse_vector(&text, "", Some(&stats)), first);
        }
    }

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

        let with_code = build_node_sparse_vector(text, code, None);
        let without = build_node_sparse_vector(text, "", None);

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

    /// The corpus stats for a small synthetic set where one term is
    /// everywhere and one is rare.
    fn stats_for(docs: &[&str]) -> SparseStats {
        let dims: Vec<Vec<u32>> = docs
            .iter()
            .map(|d| {
                build_sparse_keyword_vector(d)
                    .into_iter()
                    .map(|(dim, _)| dim)
                    .collect()
            })
            .collect();
        SparseStats::from_documents(dims.iter().map(|d| d.as_slice()))
    }

    fn weight_of(v: &[(u32, f32)], term: &str) -> Option<f32> {
        let dim = build_sparse_keyword_vector(term).first()?.0;
        v.iter().find(|(d, _)| *d == dim).map(|(_, w)| *w)
    }

    #[test]
    fn idf_makes_a_rare_query_term_outweigh_a_ubiquitous_one() {
        // "the" is in every document; "quaternion" in one.
        let corpus = [
            "the parser reads the file",
            "the writer flushes the buffer",
            "the server accepts the request",
            "the quaternion rotates the mesh",
        ];
        let stats = stats_for(&corpus);

        let q = build_sparse_query_vector("the quaternion", Some(&stats));
        let common = weight_of(&q, "the").expect("common term present");
        let rare = weight_of(&q, "quaternion").expect("rare term present");
        assert!(
            rare > common * 2.0,
            "rare term must dominate: rare={rare} common={common}"
        );

        // Without stats the two are indistinguishable — the behaviour this
        // replaced, and still the fallback for stores with no sidecar.
        let flat = build_sparse_query_vector("the quaternion", None);
        assert_eq!(
            weight_of(&flat, "the"),
            weight_of(&flat, "quaternion"),
            "unweighted fallback treats every term alike"
        );
    }

    #[test]
    fn document_weights_saturate_rather_than_scale_with_repetition() {
        let once = build_node_sparse_vector("alpha", "", None);
        let many = build_node_sparse_vector("alpha alpha alpha alpha alpha alpha", "", None);

        let w1 = weight_of(&once, "alpha").unwrap();
        let w6 = weight_of(&many, "alpha").unwrap();
        assert!(w6 > w1, "repetition still counts for something");
        assert!(
            w6 < w1 * 2.0,
            "but nowhere near six times: once={w1} six={w6}"
        );
    }

    #[test]
    fn document_weights_never_go_negative() {
        // OverGraph rejects negative sparse weights on write.
        let v = build_node_sparse_vector("Function: parse. Reads settings.", "fn parse() {}", None);
        assert!(!v.is_empty());
        assert!(v.iter().all(|&(_, w)| w > 0.0), "{v:?}");
    }

    #[test]
    fn the_dimension_cap_keeps_distinctive_terms_over_common_ones() {
        // A node whose text is dominated by one very common word plus a
        // long tail of unique identifiers, sized past the cap.
        let mut text = String::new();
        for i in 0..MAX_SPARSE_DIMS + 200 {
            text.push_str(&format!("ident{} ", i));
        }
        // "the" repeated enough that raw term frequency would rank it top.
        for _ in 0..50 {
            text.push_str("the ");
        }

        // A corpus where "the" is everywhere and the identifiers are not.
        let corpus: Vec<String> = (0..20).map(|i| format!("the thing number {}", i)).collect();
        let refs: Vec<&str> = corpus.iter().map(|s| s.as_str()).collect();
        let stats = stats_for(&refs);

        let with_stats = build_node_sparse_vector(&text, "", Some(&stats));
        let without = build_node_sparse_vector(&text, "", None);

        assert_eq!(with_stats.len(), MAX_SPARSE_DIMS);
        assert!(
            weight_of(&without, "the").is_some(),
            "raw frequency keeps the most repeated word"
        );
        assert!(
            weight_of(&with_stats, "the").is_none(),
            "idf-ranked truncation drops the term that carries no information"
        );
    }

    #[test]
    fn sparse_vector_is_capped_and_dimension_sorted() {
        // A body far larger than any real function.
        let code: String = (0..5000).map(|i| format!("ident{} ", i)).collect();
        let v = build_node_sparse_vector("Function: big.", &code, None);

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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let text = build_node_text(&node, &[]);
        assert!(!text.contains("("), "no redundant parenthetical: {text}");
    }

    // ---- framework semantics ------------------------------------------
    //
    // These all pin the same idea: in an annotation-driven framework the
    // annotation carries meaning that appears in no identifier, no path and
    // no docstring, so unless it reaches the embedding text a natural
    // question about the system can't find the code that answers it.

    fn annotated(name: &str, annotations: &[(&str, Option<&str>)]) -> GraphNode {
        GraphNode {
            id: format!("class:src/A.java:{name}"),
            name: name.into(),
            node_type: GraphNodeType::Class,
            file: Some("src/A.java".into()),
            annotations: annotations
                .iter()
                .map(|(n, a)| crate::types::Annotation {
                    name: (*n).into(),
                    args: a.map(|s| s.to_string()),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_route_leads_the_description() {
        let mut n = annotated("OrderController.find", &[("GetMapping", Some("\"/{id}\""))]);
        n.route = Some("GET /api/orders/{id}".into());
        let text = build_node_text(&n, &[]);
        assert!(
            text.contains("HTTP endpoint GET /api/orders/{id}"),
            "the URL is the string people search with: {text}"
        );
    }

    #[test]
    fn a_framework_role_is_spelled_out_in_searchable_words() {
        let text = build_node_text(&annotated("JdbcOrderStore", &[("Repository", None)]), &[]);
        assert!(
            text.contains("Spring data access repository"),
            "nothing else in this node says what it is: {text}"
        );
    }

    #[test]
    fn a_value_carrying_annotation_contributes_its_value() {
        let text = build_node_text(
            &annotated(
                "Order",
                &[("Entity", None), ("Table", Some("name = \"orders\""))],
            ),
            &[],
        );
        assert!(text.contains("JPA persistent entity"), "{text}");
        assert!(
            text.contains("mapped to table orders"),
            "the table name is what a data question mentions: {text}"
        );
    }

    #[test]
    fn semantics_survive_alongside_a_docstring() {
        // Unlike the structural synopsis, these are not a fallback: a
        // documented handler still needs its URL in the text.
        let mut n = annotated("find", &[("Transactional", None)]);
        n.docstring = Some("Looks up an order.".into());
        let text = build_node_text(&n, &[]);
        assert!(text.contains("runs in a database transaction"), "{text}");
        assert!(text.contains("Looks up an order."), "{text}");
    }

    #[test]
    fn an_unrecognised_annotation_is_still_named_and_split() {
        let text = build_node_text(&annotated("pay", &[("AuditLogged", None)]), &[]);
        assert!(text.contains("AuditLogged"), "{text}");
        assert!(
            text.contains("audit logged"),
            "a house annotation splits into words like any identifier: {text}"
        );
    }

    #[test]
    fn override_is_left_out() {
        // It sits on a large share of all methods and distinguishes none of
        // them; the graph records the same fact precisely, as an edge.
        let text = build_node_text(&annotated("save", &[("Override", None)]), &[]);
        assert!(!text.contains("Override"), "{text}");
    }

    #[test]
    fn a_node_without_annotations_is_unchanged() {
        let plain = GraphNode {
            id: "function:src/a.ts:parse".into(),
            name: "parse".into(),
            node_type: GraphNodeType::Function,
            file: Some("src/a.ts".into()),
            docstring: Some("Parses input.".into()),
            ..Default::default()
        };
        let text = build_node_text(&plain, &[]);
        assert_eq!(text, "Function: parse. Parses input.. Related: ");
    }

    // ---- identifier splitting edge cases ------------------------------

    #[test]
    fn splitting_handles_degenerate_identifiers() {
        assert!(split_identifier("").is_empty());
        assert!(split_identifier("___").is_empty(), "separators only");
        assert_eq!(split_identifier("x"), vec!["x"]);
        assert_eq!(split_identifier("_leading"), vec!["leading"]);
        assert_eq!(split_identifier("trailing_"), vec!["trailing"]);
        assert_eq!(split_identifier("a__b"), vec!["a", "b"], "runs collapse");
    }

    #[test]
    fn an_all_caps_identifier_stays_one_word() {
        // `MAX` has no lower-case follower to mark a boundary, so splitting
        // it would produce three single letters and no useful token.
        assert_eq!(split_identifier("MAX"), vec!["max"]);
        assert_eq!(split_identifier("MAX_RETRIES"), vec!["max", "retries"]);
    }

    #[test]
    fn a_qualified_name_splits_on_its_separators() {
        // Java members arrive as `Type.member`, Rust as `Type::method`.
        assert_eq!(
            split_identifier("OrderService.cancel"),
            vec!["order", "service", "cancel"]
        );
        assert_eq!(
            split_identifier("Db::upsert_nodes"),
            vec!["db", "upsert", "nodes"]
        );
    }

    // ---- related-name handling ----------------------------------------

    #[test]
    fn related_names_are_deduped_sorted_and_capped() {
        // A hub node's neighbours would otherwise dominate the embedding
        // and crowd out its own terms.
        let mut nodes = vec![GraphNode {
            id: "hub".into(),
            name: "hub".into(),
            node_type: GraphNodeType::File,
            ..Default::default()
        }];
        let mut edges = Vec::new();
        for i in 0..(MAX_RELATED + 40) {
            nodes.push(GraphNode {
                id: format!("n{i}"),
                name: format!("leaf{i:04}"),
                node_type: GraphNodeType::Function,
                ..Default::default()
            });
            edges.push(crate::types::GraphEdge {
                source: "hub".into(),
                target: format!("n{i}").into(),
                edge_type: crate::types::GraphEdgeType::Contains,
            });
            // A duplicate edge must not produce a duplicate name.
            edges.push(crate::types::GraphEdge {
                source: "hub".into(),
                target: format!("n{i}").into(),
                edge_type: crate::types::GraphEdgeType::References,
            });
        }
        let graph = GraphData {
            nodes,
            edges,
            stats: None,
            resolution: None,
        };
        let related = collect_related_names(&graph);
        let hub = &related["hub"];
        assert_eq!(hub.len(), MAX_RELATED);
        assert!(hub.windows(2).all(|w| w[0] < w[1]), "sorted and deduped");
    }

    #[test]
    fn relatedness_is_bidirectional() {
        // An edge tells you as much about its target as its source, so both
        // ends carry the other's name.
        let graph = GraphData {
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    name: "alpha".into(),
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    name: "beta".into(),
                    ..Default::default()
                },
            ],
            edges: vec![crate::types::GraphEdge {
                source: "a".into(),
                target: "b".into(),
                edge_type: crate::types::GraphEdgeType::Calls,
            }],
            stats: None,
            resolution: None,
        };
        let related = collect_related_names(&graph);
        assert_eq!(related["a"], vec!["beta".to_string()]);
        assert_eq!(related["b"], vec!["alpha".to_string()]);
    }

    #[test]
    fn an_edge_to_a_missing_node_contributes_no_name() {
        // Dangling targets are dropped by the graph builder, but a
        // hand-built or legacy graph can still carry one.
        let graph = GraphData {
            nodes: vec![GraphNode {
                id: "a".into(),
                name: "alpha".into(),
                ..Default::default()
            }],
            edges: vec![crate::types::GraphEdge {
                source: "a".into(),
                target: "gone".into(),
                edge_type: crate::types::GraphEdgeType::Calls,
            }],
            stats: None,
            resolution: None,
        };
        let related = collect_related_names(&graph);
        assert!(related.get("a").is_none_or(|v| v.is_empty()));
    }

    // ---- description budget --------------------------------------------

    #[test]
    fn the_description_is_capped_by_the_budget_not_the_indexer() {
        // A markdown section or a PDF page can run to kilobytes; the cap
        // follows the loaded model's window so switching models needs a
        // re-embed rather than a re-index.
        let node = GraphNode {
            id: "concept:docs/a.md:big".into(),
            name: "big".into(),
            node_type: GraphNodeType::Concept,
            docstring: Some("word ".repeat(5_000)),
            ..Default::default()
        };
        let budget = EmbedBudget {
            description_chars: 100,
            ..EmbedBudget::default()
        };
        let text = build_node_text_with_comments(&node, &[], "", &budget);
        assert!(text.contains('…'), "should be truncated: {}", &text[..80]);
        assert!(text.len() < 400, "got {} bytes", text.len());
    }

    #[test]
    fn comments_ride_alongside_the_description_rather_than_replacing_it() {
        let node = GraphNode {
            id: "function:src/a.ts:parse".into(),
            name: "parse".into(),
            node_type: GraphNodeType::Function,
            docstring: Some("Parses input.".into()),
            ..Default::default()
        };
        let text =
            build_node_text_with_comments(&node, &[], "handles the legacy format", &EmbedBudget::default());
        assert!(text.contains("Parses input."), "{text}");
        assert!(text.contains("Notes: handles the legacy format."), "{text}");
    }

    #[test]
    fn a_folder_node_embeds_its_path_not_its_basename() {
        // `src/components` and `tests/components` are different folders and
        // must not embed identically.
        let node = GraphNode {
            id: "folder:tests/components".into(),
            name: "components".into(),
            node_type: GraphNodeType::Folder,
            folder: Some(GraphNodeFolderMeta {
                depth: 1,
                parent: Some("tests".into()),
                classification: None,
                readme: None,
                total_files: 3,
                language_breakdown: std::collections::BTreeMap::new(),
                summary: None,
            }),
            ..Default::default()
        };
        let text = build_node_text(&node, &[]);
        assert!(text.contains("tests/components"), "{text}");
    }
}
