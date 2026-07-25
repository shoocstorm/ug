//! Guided graph tours: turn a question into an ordered, narrated
//! walkthrough of the knowledge graph.
//!
//! Pipeline:
//!   1. GraphRAG retrieval (`chat::retrieve_context`) — the same PPR-fused
//!      semantic + structural search chat uses, so a tour visits the nodes
//!      that actually matter for the question. The candidate set is then
//!      *diversified* (a cap per file) so one big file can't monopolise
//!      the itinerary, and each candidate gets a short code snippet so the
//!      guide narrates from real code rather than from descriptions alone.
//!   2. An LLM "tour guide" pass that *orders* a subset of those nodes into
//!      a coherent narrative (entry point → detail → payoff) and writes
//!      per-stop narration. The guide references items by their `[#N]`
//!      number so every stop binds back to a real graph node id, and it is
//!      shown the edges *between* candidates so "follow the flow" means
//!      following actual graph structure. A malformed reply gets one
//!      repair round-trip before we fall back to a ranked itinerary.
//!   3. A `Tour` whose stops carry `node_id`/`file`/lines — enough for the
//!      UI to fly the camera and for the CLI to print an itinerary — plus
//!      the route edges between consecutive stops, the candidate pack, and
//!      (optionally) the raw plan the model produced, so the UI can show
//!      the user exactly what the LLM generated.
//!
//! Shared by `ug tour` (CLI) and `POST /api/tour` (serve/UI) so both
//! entry points produce identical tours.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use ultragraph::storage::{
    read_snippet, traverse_filtered, ContextItem, Direction, Embedder, KnowledgeStore, RankStrategy,
};

use crate::chat::{retrieve_context, ChatClient, ChatMessage, ChatRagOptions, Usage};

/// Default number of stops when the caller doesn't specify one. Small
/// enough to stay a "guided" tour rather than a full dump.
pub const DEFAULT_MAX_STOPS: usize = 10;

/// Hard ceiling on stops, shared by the CLI and `/api/tour`.
pub const MAX_STOPS_LIMIT: usize = 40;

/// Per-stop snippet cap. A tour shows a *taste* of each node, and this
/// also stops one huge (e.g. minified) file from dominating the payload.
const TOUR_SNIPPET_MAX_CHARS: usize = 900;
const TOUR_SNIPPET_MAX_LINES: usize = 22;

/// Tighter budget for the code shown to the *planner*: enough to tell what
/// a candidate does, small enough that a dozen of them still fit the prompt.
const PROMPT_SNIPPET_MAX_CHARS: usize = 480;
const PROMPT_SNIPPET_MAX_LINES: usize = 14;

/// How many candidates we read source for / offer to the guide. Beyond
/// this the prompt stops being a menu and starts being a haystack — but a
/// long tour needs a menu at least as long as its itinerary, so the cap
/// grows with the stop budget.
const MIN_PROMPT_ITEMS: usize = 24;
const MAX_PROMPT_ITEMS: usize = 64;

fn prompt_item_cap(max_stops: usize) -> usize {
    (max_stops + 8).clamp(MIN_PROMPT_ITEMS, MAX_PROMPT_ITEMS)
}

/// Cap on the rendered `[#a] --rel--> [#b]` link map.
const MAX_LINK_LINES: usize = 120;

/// Completion budget a tour needs. A plan is one JSON object holding
/// several multi-sentence narrations, and reasoning models spend
/// *thousands* of tokens thinking before the first `{` — at the
/// 1024-token chat default the object gets cut off mid-string and the
/// whole plan is lost. `max_tokens` is a ceiling, not a reservation, so a
/// generous one costs nothing for models that answer straight away.
pub const TOUR_MIN_COMPLETION_TOKENS: u32 = 32_768;

/// Matching HTTP timeout. Raising the token ceiling without raising this
/// just trades a truncated plan for a timed-out one: a local model
/// emitting 30k tokens can easily run past the 180s chat default.
pub const TOUR_MIN_TIMEOUT_SECS: u64 = 900;

/// Default per-file candidate cap. Two chunks of the same file is a
/// detail pass; five is a file dump wearing a tour's clothes.
const DEFAULT_MAX_PER_FILE: usize = 2;

/// A step of the planning pipeline, reported as it happens. Planning a
/// tour against a local model can take minutes — most of it spent waiting
/// on tokens — so callers get a running account instead of a spinner.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TourProgress {
    /// GraphRAG retrieval started.
    Retrieving,
    Retrieved {
        candidates: usize,
        retrieval_ms: u128,
    },
    /// Reading source for the candidates the guide will be shown.
    ReadingCode { items: usize },
    /// Probing the graph for edges between candidates.
    Linking { edges: usize },
    /// The guide has been given the prompt; tokens are next.
    Planning {
        model: String,
        prompt_chars: usize,
        candidates_shown: usize,
        max_stops: usize,
    },
    /// Throttled progress while the model writes. `reasoning_chars`
    /// counts think-out-loud text providers stream separately.
    Writing {
        chars: usize,
        reasoning_chars: usize,
        elapsed_ms: u128,
    },
    /// The first reply was unusable; asking for a repair.
    Repairing { reason: String },
    /// Binding the plan back onto graph nodes.
    Assembling { stops: usize },
}

/// How often `Writing` events are emitted while tokens stream in.
const PROGRESS_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// A progress sink. `Send` so it can live inside a spawned SSE task.
pub type ProgressFn<'a> = &'a mut (dyn FnMut(TourProgress) + Send);

/// A graph edge between two nodes on the tour (or between two candidates).
#[derive(Clone, Debug, Serialize)]
pub struct TourEdge {
    pub source: String,
    pub target: String,
    pub edge_type: String,
}

/// The relationship carrying us from the previous stop to this one, when
/// the graph actually has one. `reverse` means the edge points backwards
/// (this stop is the *source*), which the UI renders as "← called by".
#[derive(Clone, Debug, Serialize)]
pub struct StopLink {
    pub edge_type: String,
    pub reverse: bool,
}

/// One stop on the tour: a graph node plus the guide's narration for it.
#[derive(Clone, Debug, Serialize)]
pub struct TourStop {
    /// 1-based index into the retrieved context pack the guide referenced
    /// (the `[#N]` label). Handy for debugging a plan against its context.
    pub ref_index: usize,
    pub node_id: String,
    pub name: String,
    pub node_type: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Short headline for the stop (the guide's, else the node name).
    pub title: String,
    /// The narration read aloud / displayed at this stop.
    pub narration: String,
    pub snippet: Option<String>,
    /// Graph edge connecting the previous stop to this one, when one
    /// exists — lets the UI narrate the transition honestly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_from_prev: Option<StopLink>,
}

/// A candidate the retrieval pass surfaced, whether or not the guide used
/// it. Surfaced so the UI can show what the tour was chosen *from*.
#[derive(Clone, Debug, Serialize)]
pub struct TourCandidate {
    pub ref_index: usize,
    pub node_id: String,
    pub name: String,
    pub node_type: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub distance: f32,
    pub hop: u32,
    /// True when the guide visited this candidate.
    pub used: bool,
    /// False when the char budget cut it before the model ever saw it.
    pub shown_to_guide: bool,
}

/// A `ref` the model emitted that we couldn't turn into a stop.
#[derive(Clone, Debug, Serialize)]
pub struct DroppedRef {
    pub raw: String,
    pub reason: String,
}

/// Everything about the planning pass itself: what we asked, what the
/// model said, and what we made of it. Powers the UI's "view the JSON the
/// LLM generated" panel and makes bad tours debuggable instead of magic.
#[derive(Clone, Debug, Serialize, Default)]
pub struct TourDebug {
    pub system_prompt: String,
    pub user_prompt: String,
    /// The model's reply, verbatim (fences, prose and all).
    pub raw_response: String,
    /// The plan as we parsed it — the JSON object the model *meant*.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<serde_json::Value>,
    /// True when the first reply was unusable and we asked for a repair.
    pub repaired: bool,
    /// True when the reply we ended up with stops mid-object — almost
    /// always the completion token budget running out.
    pub truncated: bool,
    /// The repair exchange, when one happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair_response: Option<String>,
    pub dropped: Vec<DroppedRef>,
}

/// A complete guided tour: framing + ordered stops + a closing takeaway.
#[derive(Clone, Debug, Serialize)]
pub struct Tour {
    pub query: String,
    pub title: String,
    pub intro: String,
    pub outro: String,
    pub stops: Vec<TourStop>,
    /// The GraphRAG seed node (highest-ranked hit) — the natural place to
    /// open the camera before the first stop.
    pub seed_id: Option<String>,
    /// True when the itinerary fell back to ranked order because the LLM
    /// plan was missing or unusable, rather than a curated narrative.
    pub fallback: bool,
    /// Edges among the visited nodes — the "route" the UI can draw.
    pub route: Vec<TourEdge>,
    /// The retrieved pack the guide chose from.
    pub candidates: Vec<TourCandidate>,
    /// Non-fatal things the user should know about this tour.
    pub warnings: Vec<String>,
    /// Planning-pass transcript; `None` when the caller opted out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug: Option<TourDebug>,
    pub retrieval_ms: u128,
    pub completion_ms: u128,
    pub usage: Option<Usage>,
}

impl Tour {
    /// Blank slate every constructor fills in. Keeps the several
    /// tour-shaped constructors below from drifting apart.
    fn skeleton(query: &str) -> Self {
        Tour {
            query: query.to_string(),
            title: default_title(query),
            intro: String::new(),
            outro: String::new(),
            stops: Vec::new(),
            seed_id: None,
            fallback: false,
            route: Vec::new(),
            candidates: Vec::new(),
            warnings: Vec::new(),
            debug: None,
            retrieval_ms: 0,
            completion_ms: 0,
            usage: None,
        }
    }
}

/// Per-request tour knobs. Mirrors the retrieval subset of
/// `ChatRagOptions` plus `max_stops` (how many nodes the guide may visit).
#[derive(Clone, Debug)]
pub struct TourOptions<'a> {
    pub k: usize,
    pub hops: u32,
    pub max_stops: usize,
    pub strategy: RankStrategy,
    pub direction: Direction,
    pub edge_types: Option<&'a [String]>,
    pub include_snippets: bool,
    pub max_context_chars: usize,
    pub where_clause: Option<&'a str>,
    /// Cap on candidates drawn from any single file (0 = no cap).
    pub max_per_file: usize,
    /// Attach the planning transcript to the result (`Tour::debug`).
    pub include_debug: bool,
    /// Stream the completion so token-level progress can be reported.
    /// Only worth turning on when someone is watching a progress sink.
    pub stream: bool,
}

impl<'a> TourOptions<'a> {
    pub fn new() -> Self {
        Self {
            // A tour wants a slightly wider net than chat: more candidate
            // nodes gives the guide room to build a real narrative arc.
            k: 14,
            hops: 2,
            max_stops: DEFAULT_MAX_STOPS,
            strategy: RankStrategy::Ppr,
            direction: Direction::Both,
            edge_types: None,
            include_snippets: true,
            max_context_chars: crate::chat::DEFAULT_CTX_MAX_CHARS,
            where_clause: None,
            max_per_file: DEFAULT_MAX_PER_FILE,
            include_debug: true,
            stream: false,
        }
    }
}

impl<'a> Default for TourOptions<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// System prompt for the tour-guide planning pass. The stop budget is
/// interpolated so the model plans to the length the caller actually
/// asked for, instead of writing ten stops we then truncate mid-arc.
pub fn tour_system_prompt(max_stops: usize) -> String {
    let hi = max_stops.max(1);
    // Give the model a floor as well as a ceiling: "3 to 6" produces a
    // tighter arc than "up to 6", which tends to sprawl to the limit.
    let lo = hi.saturating_sub(2).clamp(1, hi).max(if hi >= 3 { 3 } else { hi });
    let count = if lo == hi {
        format!("exactly {} stops", hi)
    } else {
        format!("between {} and {} stops", lo, hi)
    };
    format!(
        "You are UltraGraph's Tour Guide. You are given a numbered set of code/knowledge context \
items ([#1], [#2], …) retrieved from a knowledge graph over the user's repository, a LINKS list of \
the graph edges between those items, and a question. Design a short guided walking tour that \
ANSWERS the question by visiting a subset of these items in a logical narrative order — begin at \
the entry point and follow the flow of control, data, or dependencies from there.\n\n\
Return ONLY a single JSON object — no prose before or after, no markdown code fences — of exactly \
this shape:\n\
{{\n\
  \"title\": \"<a short, engaging title for the tour>\",\n\
  \"intro\": \"<1-2 sentences framing what we'll walk through>\",\n\
  \"stops\": [\n\
    {{ \"ref\": <the [#N] number of the item to visit>, \"title\": \"<short stop headline, max 6 words>\", \
\"narration\": \"<2-4 sentences: this item's role in answering the question, naming the concrete \
symbols involved, and how it connects to the previous or next stop>\" }}\n\
  ],\n\
  \"outro\": \"<1-2 sentences answering the question outright>\"\n\
}}\n\n\
Rules:\n\
- Use only `ref` numbers that appear in the provided items; each item at most once.\n\
- Plan {count}. Skip items that don't advance the narrative — a shorter, coherent tour beats a \
complete one.\n\
- Order the stops as a story (entry → detail → payoff), NOT by relevance score.\n\
- Prefer consecutive stops that are connected in LINKS, and when they are, say so in the narration \
(\"…which calls…\", \"…imported by…\").\n\
- Ground every narration in the shown code and descriptions; never invent code, files, or symbols \
that aren't present.\n\
- Write narration in a warm, second-person guide voice (\"Notice how…\", \"From here we follow…\"), \
plain prose, no markdown.\n\
- The outro must actually answer the question, not just summarise the walk.",
        count = count
    )
}

/// Sent when the first reply couldn't be parsed or bound to real nodes.
fn repair_prompt(max_ref: usize, problem: &str) -> String {
    format!(
        "{problem}\n\nReply again with ONLY the JSON object described earlier — no prose, no \
markdown fences, and do not think out loud or explain your choices: the JSON object must be the \
entire reply, starting with `{{` and ending with `}}`. Every \"ref\" must be an integer between 1 \
and {max_ref} taken from the numbered items above, and each may appear at most once. Keep each \
narration to two sentences so the object finishes."
    )
}

// ---------- LLM plan shapes ----------

#[derive(Deserialize)]
struct PlanRaw {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    intro: Option<String>,
    #[serde(default)]
    outro: Option<String>,
    #[serde(default, alias = "tour", alias = "steps", alias = "itinerary")]
    stops: Option<Vec<PlanStop>>,
}

#[derive(Deserialize)]
struct PlanStop {
    // Accept the common field spellings a model might emit.
    #[serde(default, alias = "index", alias = "n", alias = "item", alias = "id")]
    r#ref: Option<serde_json::Value>,
    #[serde(default, alias = "headline")]
    title: Option<String>,
    #[serde(default, alias = "text", alias = "description", alias = "body")]
    narration: Option<String>,
}

/// Coerce a model-supplied `ref` (number, or a string like "#3"/"3.")
/// into a 1-based index.
fn coerce_ref(v: &serde_json::Value) -> Option<usize> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().map(|x| x as usize),
        serde_json::Value::String(s) => {
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        _ => None,
    }
}

/// Render a `ref` for a diagnostic message without pretending it parsed.
fn ref_display(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "(missing)".to_string(),
    }
}

/// Drop a leading `<think>…</think>` block. Reasoning models emit their
/// scratchpad first, and it is full of braces and quotes that would
/// otherwise confuse the object scan below.
fn strip_reasoning(raw: &str) -> &str {
    for close in ["</think>", "</thinking>", "</reasoning>"] {
        if let Some(pos) = raw.rfind(close) {
            return &raw[pos + close.len()..];
        }
    }
    raw
}

/// Every balanced, string-aware `{…}` span at the top level of `text`.
/// Scanning (rather than "first `{` to last `}`") is what makes this
/// survive a model that thinks out loud in prose containing braces.
fn top_level_objects(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        let mut end = None;
        let mut j = i;
        while j < bytes.len() {
            let c = bytes[j];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(j);
                    break;
                }
            }
            j += 1;
        }
        match end {
            Some(e) => {
                out.push(&text[start..=e]);
                i = e + 1;
            }
            // Unbalanced from here on (usually a truncated reply) — nothing
            // further can close, so stop.
            None => break,
        }
    }
    out
}

/// How much a parsed object looks like the plan we asked for, so a stray
/// `{"a": 1}` inside the model's reasoning never beats the real answer.
fn plan_score(v: &serde_json::Value) -> u32 {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return 0,
    };
    let mut score = 1;
    if obj.get("stops").and_then(|s| s.as_array()).is_some_and(|a| !a.is_empty()) {
        score += 4;
    }
    for key in ["title", "intro", "outro"] {
        if obj.contains_key(key) {
            score += 1;
        }
    }
    score
}

/// Pull the plan object out of a model response: a direct parse first,
/// then the best-scoring balanced object found anywhere in the reply
/// (handles ```json fences, prose, and reasoning preambles).
fn extract_json(raw: &str) -> Option<serde_json::Value> {
    let trimmed = strip_reasoning(raw).trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    let mut best: Option<(u32, usize, serde_json::Value)> = None;
    for candidate in top_level_objects(trimmed) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) else {
            continue;
        };
        let score = plan_score(&v);
        let len = candidate.len();
        // Highest score wins; the longest object breaks ties, since a
        // truncated fragment is always shorter than the real plan.
        if best.as_ref().is_none_or(|(bs, bl, _)| score > *bs || (score == *bs && len > *bl)) {
            best = Some((score, len, v));
        }
    }
    best.map(|(_, _, v)| v)
}

/// Best-effort extraction of the plan object from a model response.
fn parse_plan(raw: &str) -> Option<PlanRaw> {
    serde_json::from_value::<PlanRaw>(extract_json(raw)?).ok()
}

/// The same object as generic JSON, for the debug/plan viewer.
fn plan_value(raw: &str) -> Option<serde_json::Value> {
    extract_json(raw)
}

/// Build a `TourStop` from a retrieved context item + guide-supplied text.
fn stop_from_item(
    ref_index: usize,
    item: &ContextItem,
    title: Option<&str>,
    narration: &str,
) -> TourStop {
    let title = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| item.name.clone());
    TourStop {
        ref_index,
        node_id: item.id.clone(),
        name: item.name.clone(),
        node_type: item.node_type.clone(),
        file: item.file.clone(),
        start_line: item.start_line,
        end_line: item.end_line,
        title,
        narration: narration.trim().to_string(),
        snippet: item.snippet.clone(),
        edge_from_prev: None,
    }
}

/// What `assemble_from_plan` produces: the tour plus why refs were lost.
struct Assembled {
    tour: Tour,
    dropped: Vec<DroppedRef>,
}

/// Turn a parsed LLM plan into a `Tour`, binding each `ref` to a real
/// retrieved item. Returns `None` when no stop resolves to a valid item —
/// the caller then repairs or falls back to a ranked itinerary.
fn assemble_from_plan(
    query: &str,
    items: &[ContextItem],
    plan: PlanRaw,
    max_stops: usize,
) -> Option<Assembled> {
    let plan_stops = plan.stops?;
    let mut stops: Vec<TourStop> = Vec::new();
    let mut dropped: Vec<DroppedRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut truncated = false;
    for ps in &plan_stops {
        if stops.len() >= max_stops {
            truncated = true;
            break;
        }
        let Some(idx) = ps.r#ref.as_ref().and_then(coerce_ref) else {
            dropped.push(DroppedRef {
                raw: ref_display(ps.r#ref.as_ref()),
                reason: "not a usable item number".into(),
            });
            continue;
        };
        if idx == 0 || idx > items.len() {
            dropped.push(DroppedRef {
                raw: idx.to_string(),
                reason: format!("outside the retrieved set (1..{})", items.len()),
            });
            continue;
        }
        let item = &items[idx - 1];
        // One node per tour — revisiting the same node breaks the camera
        // narrative and reads as a stutter.
        if !seen.insert(item.id.clone()) {
            dropped.push(DroppedRef {
                raw: idx.to_string(),
                reason: "already visited earlier on the tour".into(),
            });
            continue;
        }
        let narration = ps.narration.as_deref().unwrap_or("").trim();
        let narration = if narration.is_empty() {
            item.description.clone()
        } else {
            narration.to_string()
        };
        stops.push(stop_from_item(idx, item, ps.title.as_deref(), &narration));
    }
    if stops.is_empty() {
        return None;
    }

    let mut tour = Tour::skeleton(query);
    tour.title = plan
        .title
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_title(query));
    tour.intro = plan.intro.map(|s| s.trim().to_string()).unwrap_or_default();
    tour.outro = plan.outro.map(|s| s.trim().to_string()).unwrap_or_default();
    tour.stops = stops;
    if truncated {
        tour.warnings.push(format!(
            "The guide planned more than {} stops; the itinerary was trimmed to your budget.",
            max_stops
        ));
    }
    Some(Assembled { tour, dropped })
}

/// Ranked-order itinerary used when the LLM plan is missing/unusable, or
/// when no chat model is available at all. Narration is each node's own
/// description, so the tour still works with retrieval-only.
fn fallback_tour(query: &str, items: &[ContextItem], max_stops: usize) -> Tour {
    let stops: Vec<TourStop> = items
        .iter()
        .take(max_stops)
        .enumerate()
        .map(|(i, item)| {
            let narration = if item.description.trim().is_empty() {
                format!(
                    "{} ({}) — part of the neighbourhood most relevant to your question.",
                    item.name, item.node_type
                )
            } else {
                item.description.clone()
            };
            stop_from_item(i + 1, item, None, &narration)
        })
        .collect();
    let mut tour = Tour::skeleton(query);
    tour.intro = format!(
        "The most relevant parts of the codebase for \u{201c}{}\u{201d}, in ranked order.",
        query
    );
    tour.stops = stops;
    tour.fallback = true;
    tour
}

fn empty_tour(query: &str, retrieval_ms: u128) -> Tour {
    let mut tour = Tour::skeleton(query);
    tour.intro =
        "No matching nodes were found in this project's knowledge graph for that question."
            .to_string();
    tour.fallback = true;
    tour.retrieval_ms = retrieval_ms;
    tour
}

fn default_title(query: &str) -> String {
    let q = query.trim();
    if q.is_empty() {
        "Guided tour".to_string()
    } else {
        format!("Tour: {}", q)
    }
}

/// Build the retrieval options for a tour. Snippets are deliberately
/// forced OFF here: `search_kb` budgets its result set by total snippet
/// chars, so a single huge (e.g. minified) node would otherwise collapse
/// the whole tour to one stop. We read bounded snippets ourselves
/// afterwards — see `bounded_snippet`.
fn retrieval_opts<'a>(opts: &TourOptions<'a>) -> ChatRagOptions<'a> {
    let mut rag = ChatRagOptions::new();
    // Never retrieve fewer candidates than the itinerary could hold — a
    // 20-stop tour drawn from 14 candidates is not a tour, it's the list.
    rag.k = opts.k.max(opts.max_stops + 4);
    rag.hops = opts.hops;
    rag.strategy = opts.strategy;
    rag.direction = opts.direction;
    rag.edge_types = opts.edge_types;
    rag.include_snippets = false;
    rag.max_context_chars = opts.max_context_chars;
    rag.where_clause = opts.where_clause;
    rag
}

/// Read a source snippet clipped to `max_lines` / `max_chars`. Long
/// (minified) lines are clipped char-wise so we never split a UTF-8
/// sequence mid-byte, and one such line can't eat the whole budget.
fn bounded_snippet(
    repo_root: &std::path::Path,
    file: &str,
    start_line: u32,
    end_line: u32,
    max_chars: usize,
    max_lines: usize,
) -> Option<String> {
    let full = read_snippet(repo_root, file, start_line, end_line)?;
    let mut out = String::new();
    for (i, line) in full.lines().enumerate() {
        if i >= max_lines || out.len() >= max_chars {
            out.push('\u{2026}');
            break;
        }
        if line.chars().count() > max_chars {
            out.extend(line.chars().take(max_chars));
            out.push('\u{2026}');
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    let trimmed = out.trim_end_matches('\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Attach a short, bounded source snippet to each stop (best effort). A
/// stop whose file can't be read simply keeps whatever snippet it had.
fn attach_snippets(tour: &mut Tour, repo_root: &std::path::Path) {
    for stop in &mut tour.stops {
        if let Some(s) = bounded_snippet(
            repo_root,
            &stop.file,
            stop.start_line,
            stop.end_line,
            TOUR_SNIPPET_MAX_CHARS,
            TOUR_SNIPPET_MAX_LINES,
        ) {
            stop.snippet = Some(s);
        }
    }
}

/// Give the *planner* a taste of each candidate's real code. Without this
/// the guide narrates from descriptions alone, which is where invented
/// details creep in.
fn attach_prompt_snippets(items: &mut [ContextItem], repo_root: &std::path::Path, cap: usize) {
    for item in items.iter_mut().take(cap) {
        if item.snippet.is_some() {
            continue;
        }
        item.snippet = bounded_snippet(
            repo_root,
            &item.file,
            item.start_line,
            item.end_line,
            PROMPT_SNIPPET_MAX_CHARS,
            PROMPT_SNIPPET_MAX_LINES,
        );
    }
}

/// Front-load file diversity: the first `max_per_file` candidates from any
/// one file keep their ranked position, the rest fall to the back of the
/// pack (where the char budget is likely to cut them). Nothing is deleted,
/// so a genuinely file-local question can still visit the same file twice.
fn diversify(items: Vec<ContextItem>, max_per_file: usize) -> Vec<ContextItem> {
    if max_per_file == 0 {
        return items;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut primary: Vec<ContextItem> = Vec::with_capacity(items.len());
    let mut overflow: Vec<ContextItem> = Vec::new();
    for item in &items {
        let key = if item.file.is_empty() { item.id.as_str() } else { item.file.as_str() };
        let c = counts.entry(key).or_insert(0);
        *c += 1;
        if *c <= max_per_file {
            primary.push(item.clone());
        } else {
            overflow.push(item.clone());
        }
    }
    primary.extend(overflow);
    primary
}

/// Fetch the 1-hop neighbourhood of every candidate and keep only the
/// edges whose *both* endpoints are candidates. That subgraph is what the
/// guide needs to order stops by real control/data flow. Best effort: a
/// backend hiccup just means a link-free prompt.
async fn candidate_edges(
    store: &dyn KnowledgeStore,
    items: &[ContextItem],
    edge_types: Option<&[String]>,
    cap: usize,
) -> Vec<TourEdge> {
    let ids: Vec<String> = items.iter().take(cap).map(|i| i.id.clone()).collect();
    if ids.len() < 2 {
        return Vec::new();
    }
    let in_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let result = match traverse_filtered(store, &ids, 1, edge_types, Direction::Both).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "tour: candidate edge probe failed; planning without links");
            return Vec::new();
        }
    };
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut out = Vec::new();
    for e in result.edges {
        if e.source == e.target
            || !in_set.contains(e.source.as_str())
            || !in_set.contains(e.target.as_str())
        {
            continue;
        }
        let key = (e.source.clone(), e.edge_type.clone(), e.target.clone());
        if !seen.insert(key) {
            continue;
        }
        out.push(TourEdge {
            source: e.source,
            target: e.target,
            edge_type: e.edge_type,
        });
    }
    out
}

/// Number the context items for the prompt, returning how many actually
/// fit the char budget — refs beyond that were never shown to the model.
fn render_numbered_items(items: &[ContextItem], max_chars: usize, cap: usize) -> (String, usize) {
    let mut out = String::with_capacity(items.len() * 320);
    let mut shown = 0usize;
    for (i, item) in items.iter().take(cap).enumerate() {
        let loc = if item.start_line > 0 && item.end_line >= item.start_line {
            format!(
                "{}:{}-{}",
                if item.file.is_empty() { "<unknown>" } else { item.file.as_str() },
                item.start_line,
                item.end_line
            )
        } else if item.file.is_empty() {
            "<unknown>".to_string()
        } else {
            item.file.clone()
        };
        let mut block = format!("[#{}] {} ({}) — {}\n", i + 1, item.name, item.node_type, loc);
        if !item.description.trim().is_empty() {
            block.push_str(item.description.trim());
            block.push('\n');
        }
        if let Some(snippet) = item.snippet.as_ref().filter(|s| !s.trim().is_empty()) {
            block.push_str("```\n");
            block.push_str(snippet.trim_end_matches('\n'));
            block.push_str("\n```\n");
        }
        block.push('\n');

        if !out.is_empty() && out.len() + block.len() > max_chars {
            break;
        }
        out.push_str(&block);
        shown = i + 1;
    }
    (out, shown)
}

/// Render the candidate subgraph as `[#a] --calls--> [#b]` lines, limited
/// to items the model can actually see.
fn render_links(items: &[ContextItem], edges: &[TourEdge], shown: usize) -> String {
    if shown < 2 || edges.is_empty() {
        return String::new();
    }
    let index: HashMap<&str, usize> = items
        .iter()
        .take(shown)
        .enumerate()
        .map(|(i, it)| (it.id.as_str(), i + 1))
        .collect();
    let mut lines: Vec<String> = Vec::new();
    for e in edges {
        let (Some(a), Some(b)) = (index.get(e.source.as_str()), index.get(e.target.as_str())) else {
            continue;
        };
        lines.push(format!("[#{}] --{}--> [#{}]", a, e.edge_type, b));
        if lines.len() >= MAX_LINK_LINES {
            break;
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("LINKS (graph edges between the items above):\n{}\n", lines.join("\n"))
}

/// Assemble the tour-guide prompt (system + numbered context + links +
/// question). Returns the messages plus how many items were shown.
fn build_plan_messages(
    query: &str,
    items: &[ContextItem],
    edges: &[TourEdge],
    ctx_max_chars: usize,
    max_stops: usize,
) -> (Vec<ChatMessage>, usize) {
    let (rendered, shown) = render_numbered_items(items, ctx_max_chars, prompt_item_cap(max_stops));
    let links = render_links(items, edges, shown);
    let mut user = format!(
        "Question: {}\n\nContext items:\n\n{}\n",
        query,
        rendered.trim_end()
    );
    if !links.is_empty() {
        user.push('\n');
        user.push_str(&links);
    }
    user.push_str("\nDesign the guided tour as JSON now.");
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: tour_system_prompt(max_stops),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ];
    (messages, shown)
}

/// Sum the token counts of two completions (the plan pass and, when it
/// happens, the repair pass) so `usage` reflects what the tour cost.
fn merge_usage(a: Option<Usage>, b: Option<Usage>) -> Option<Usage> {
    match (a, b) {
        (None, x) => x,
        (x, None) => x,
        (Some(x), Some(y)) => {
            let add = |l: Option<u32>, r: Option<u32>| match (l, r) {
                (None, v) => v,
                (v, None) => v,
                (Some(l), Some(r)) => Some(l + r),
            };
            Some(Usage {
                prompt_tokens: add(x.prompt_tokens, y.prompt_tokens),
                completion_tokens: add(x.completion_tokens, y.completion_tokens),
                total_tokens: add(x.total_tokens, y.total_tokens),
            })
        }
    }
}

/// Fill in `edge_from_prev` for each stop and collect the edges among the
/// visited nodes so the UI can draw the route through the graph.
fn bind_route(tour: &mut Tour, edges: &[TourEdge]) {
    let stop_ids: HashSet<&str> = tour.stops.iter().map(|s| s.node_id.as_str()).collect();
    tour.route = edges
        .iter()
        .filter(|e| stop_ids.contains(e.source.as_str()) && stop_ids.contains(e.target.as_str()))
        .cloned()
        .collect();

    for i in 1..tour.stops.len() {
        let prev = tour.stops[i - 1].node_id.clone();
        let cur = tour.stops[i].node_id.clone();
        let link = tour
            .route
            .iter()
            .find(|e| e.source == prev && e.target == cur)
            .map(|e| StopLink {
                edge_type: e.edge_type.clone(),
                reverse: false,
            })
            .or_else(|| {
                tour.route
                    .iter()
                    .find(|e| e.source == cur && e.target == prev)
                    .map(|e| StopLink {
                        edge_type: e.edge_type.clone(),
                        reverse: true,
                    })
            });
        tour.stops[i].edge_from_prev = link;
    }
}

/// Record the candidate pack on the tour, flagging which items the guide
/// visited and which never made the prompt.
fn bind_candidates(tour: &mut Tour, items: &[ContextItem], shown: usize) {
    let used: HashSet<&str> = tour.stops.iter().map(|s| s.node_id.as_str()).collect();
    tour.candidates = items
        .iter()
        .enumerate()
        .map(|(i, it)| TourCandidate {
            ref_index: i + 1,
            node_id: it.id.clone(),
            name: it.name.clone(),
            node_type: it.node_type.clone(),
            file: it.file.clone(),
            start_line: it.start_line,
            end_line: it.end_line,
            distance: it.distance,
            hop: it.hop,
            used: used.contains(it.id.as_str()),
            shown_to_guide: i < shown,
        })
        .collect();
}

/// Post-flight sanity notes: things that are worth telling the user
/// without failing the tour.
fn add_quality_warnings(tour: &mut Tour, items: &[ContextItem], dropped: &[DroppedRef]) {
    if !dropped.is_empty() {
        tour.warnings.push(format!(
            "{} planned stop(s) didn't match a retrieved node and were skipped.",
            dropped.len()
        ));
    }
    if tour.stops.len() == 1 {
        tour.warnings
            .push("Only one stop could be planned — try rephrasing, or raise the hop count.".into());
    }
    // A tour that never visits the top-ranked hit is often the guide
    // wandering off; surface it rather than silently shipping it.
    if let (Some(seed), false) = (tour.seed_id.as_deref(), tour.stops.is_empty()) {
        if !tour.stops.iter().any(|s| s.node_id == seed) {
            let name = items
                .iter()
                .find(|i| i.id == seed)
                .map(|i| i.name.clone())
                .unwrap_or_else(|| seed.to_string());
            tour.warnings
                .push(format!("The top-ranked match for your question ({}) isn't on the tour.", name));
        }
    }
    if tour.stops.len() > 1 {
        let linked = tour.stops.iter().skip(1).filter(|s| s.edge_from_prev.is_some()).count();
        if linked == 0 {
            tour.warnings.push(
                "No direct graph edges connect consecutive stops — the order is thematic, not structural."
                    .into(),
            );
        }
    }
}

/// Did the completion stop because it hit the token ceiling? The
/// provider's `finish_reason` is authoritative; without one, a reply that
/// opens an object and never closes it is the giveaway.
fn looks_truncated(raw: &str, finish_reason: Option<&str>) -> bool {
    if let Some(r) = finish_reason {
        return r.eq_ignore_ascii_case("length");
    }
    raw.contains('{') && top_level_objects(strip_reasoning(raw)).is_empty()
}

/// A client with enough completion budget — and enough wall-clock — to
/// finish a plan. Values the caller raised themselves are left alone, so
/// an explicit `--max-tokens` / `--chat-timeout` always wins.
fn planning_client(chat: &ChatClient) -> Option<ChatClient> {
    let cfg = chat.config();
    let raise_tokens = cfg.max_tokens <= crate::chat::DEFAULT_MAX_TOKENS;
    let raise_timeout = cfg.timeout_secs <= crate::chat::DEFAULT_TIMEOUT_SECS;
    if !raise_tokens && !raise_timeout {
        return None;
    }
    let mut raised = cfg.clone();
    if raise_tokens {
        raised.max_tokens = TOUR_MIN_COMPLETION_TOKENS;
    }
    if raise_timeout {
        raised.timeout_secs = TOUR_MIN_TIMEOUT_SECS;
    }
    ChatClient::new(raised).ok()
}

/// One completion, with token-level progress when `stream` is on.
///
/// Streaming is the only way to tell "the model is thinking" apart from
/// "the connection is dead" on a minutes-long local completion. A provider
/// that rejects `stream: true` falls back to the blocking call, so turning
/// this on can't break a working setup.
async fn complete_tracked(
    chat: &ChatClient,
    messages: &[ChatMessage],
    stream: bool,
    on_progress: ProgressFn<'_>,
) -> Result<(String, Option<Usage>, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    if !stream {
        let (text, usage, finish) = chat.complete_with_reason(messages).await?;
        return Ok((text, usage, finish));
    }

    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut chars = 0usize;
    let mut reasoning_chars = 0usize;
    let mut finish: Option<String> = None;

    let outcome = {
        let progress = &mut *on_progress;
        chat.complete_stream(messages, |d| {
            if let Some(c) = &d.content {
                chars += c.len();
            }
            if let Some(r) = &d.reasoning {
                reasoning_chars += r.len();
            }
            if let Some(f) = &d.finish_reason {
                finish = Some(f.clone());
            }
            if last_tick.elapsed() >= PROGRESS_TICK {
                last_tick = Instant::now();
                progress(TourProgress::Writing {
                    chars,
                    reasoning_chars,
                    elapsed_ms: started.elapsed().as_millis(),
                });
            }
        })
        .await
    };

    match outcome {
        Ok((content, reasoning, usage)) => {
            on_progress(TourProgress::Writing {
                chars,
                reasoning_chars,
                elapsed_ms: started.elapsed().as_millis(),
            });
            // Some providers put the whole reply on the reasoning channel;
            // if `content` came back empty, that's where the JSON is.
            let text = if content.trim().is_empty() { reasoning } else { content };
            Ok((text, usage, finish))
        }
        // The endpoint refused SSE — take the blocking path instead.
        Err(crate::chat::ChatError::BadStatus(code, _)) => {
            tracing::debug!(status = code, "tour: streaming rejected; falling back to a blocking completion");
            let (text, usage, reason) = chat.complete_with_reason(messages).await?;
            Ok((text, usage, reason))
        }
        Err(e) => Err(Box::new(e)),
    }
}

/// Run the planning pass, repairing one bad reply before giving up.
/// Returns the assembled tour (if any) plus the debug transcript.
async fn run_plan(
    chat: &ChatClient,
    messages: Vec<ChatMessage>,
    query: &str,
    items: &[ContextItem],
    max_stops: usize,
    stream: bool,
    on_progress: ProgressFn<'_>,
) -> Result<(Option<Assembled>, TourDebug, Option<Usage>), Box<dyn std::error::Error + Send + Sync>>
{
    let mut debug = TourDebug {
        system_prompt: messages.first().map(|m| m.content.clone()).unwrap_or_default(),
        user_prompt: messages.get(1).map(|m| m.content.clone()).unwrap_or_default(),
        ..Default::default()
    };

    let (answer, usage, finish) = complete_tracked(chat, &messages, stream, on_progress).await?;
    debug.raw_response = answer.clone();
    debug.plan = plan_value(&answer);

    let first = parse_plan(&answer)
        .and_then(|plan| assemble_from_plan(query, items, plan, max_stops));
    if let Some(a) = first {
        debug.dropped = a.dropped.clone();
        return Ok((Some(a), debug, usage));
    }

    // One repair round-trip. Models that fence their JSON, think out loud,
    // or invent refs usually get it right when told what went wrong.
    let truncated = looks_truncated(&answer, finish.as_deref());
    let problem = if truncated {
        "Your previous reply was cut off before the JSON object closed. Keep the narration brief so the whole object fits."
    } else if debug.plan.is_none() {
        "Your previous reply was not valid JSON."
    } else {
        "Your previous reply had no stop that referenced a real item number."
    };
    let mut repair = messages;
    repair.push(ChatMessage {
        role: "assistant".into(),
        content: answer,
    });
    repair.push(ChatMessage {
        role: "user".into(),
        content: repair_prompt(items.len().min(prompt_item_cap(max_stops)), problem),
    });

    debug.repaired = true;
    on_progress(TourProgress::Repairing {
        reason: problem.to_string(),
    });
    let (retry, usage2, finish2) = complete_tracked(chat, &repair, stream, on_progress).await?;
    debug.repair_response = Some(retry.clone());
    debug.truncated = looks_truncated(&retry, finish2.as_deref());
    if let Some(v) = plan_value(&retry) {
        debug.plan = Some(v);
    }
    let assembled =
        parse_plan(&retry).and_then(|plan| assemble_from_plan(query, items, plan, max_stops));
    if let Some(a) = &assembled {
        debug.dropped = a.dropped.clone();
    }
    Ok((assembled, debug, merge_usage(usage, usage2)))
}

/// Retrieve + shape the candidate pack shared by both planning paths.
async fn gather_candidates(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    repo_root: &std::path::Path,
    query: &str,
    opts: &TourOptions<'_>,
    with_snippets: bool,
    on_progress: ProgressFn<'_>,
) -> Result<(Vec<ContextItem>, Option<String>, u128), Box<dyn std::error::Error + Send + Sync>> {
    let rag = retrieval_opts(opts);
    on_progress(TourProgress::Retrieving);
    let t_ret = Instant::now();
    let context = retrieve_context(store, embedder, repo_root, query, &rag).await?;
    let retrieval_ms = t_ret.elapsed().as_millis();
    let mut items = diversify(context.items, opts.max_per_file);
    on_progress(TourProgress::Retrieved {
        candidates: items.len(),
        retrieval_ms,
    });
    if with_snippets {
        let cap = prompt_item_cap(opts.max_stops);
        attach_prompt_snippets(&mut items, repo_root, cap);
        on_progress(TourProgress::ReadingCode {
            items: items.iter().take(cap).filter(|i| i.snippet.is_some()).count(),
        });
    }
    Ok((items, context.seed_id, retrieval_ms))
}

/// No-op progress sink for callers that don't want a running commentary.
fn silent_progress() -> impl FnMut(TourProgress) + Send {
    |_| {}
}

/// Plan a guided tour for `query`: retrieve, ask the guide to order and
/// narrate a subset, and bind the result back to real graph nodes. Always
/// returns a `Tour` — on any planning failure it degrades to a ranked
/// itinerary rather than erroring, so the UI/CLI always has something to
/// show.
pub async fn plan_tour(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat: &ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    opts: TourOptions<'_>,
) -> Result<Tour, Box<dyn std::error::Error + Send + Sync>> {
    let mut quiet = silent_progress();
    plan_tour_with_progress(store, embedder, chat, repo_root, query, opts, &mut quiet).await
}

/// As [`plan_tour`], but reporting each step to `on_progress` as it
/// happens — what the SSE route and the CLI's live status line consume.
///
/// [`plan_tour`]: plan_tour
pub async fn plan_tour_with_progress(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat: &ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    opts: TourOptions<'_>,
    on_progress: ProgressFn<'_>,
) -> Result<Tour, Box<dyn std::error::Error + Send + Sync>> {
    let (items, seed_id, retrieval_ms) =
        gather_candidates(store, embedder, repo_root, query, &opts, true, on_progress).await?;

    if items.is_empty() {
        return Ok(empty_tour(query, retrieval_ms));
    }

    // A long itinerary needs a long menu — and enough prompt budget to show
    // it. Both scale with the stop count, but only when the caller left them
    // at their defaults.
    let cap = prompt_item_cap(opts.max_stops);
    let ctx_chars = if opts.max_context_chars <= crate::chat::DEFAULT_CTX_MAX_CHARS {
        (cap * 700).clamp(crate::chat::DEFAULT_CTX_MAX_CHARS, 48_000)
    } else {
        opts.max_context_chars
    };

    let edges = candidate_edges(store, &items, opts.edge_types, cap).await;
    on_progress(TourProgress::Linking { edges: edges.len() });

    let (messages, shown) = build_plan_messages(query, &items, &edges, ctx_chars, opts.max_stops);

    // Plan with a completion budget big enough to hold the whole object.
    let raised = planning_client(chat);
    let planner = raised.as_ref().unwrap_or(chat);

    on_progress(TourProgress::Planning {
        model: planner.config().model.clone(),
        prompt_chars: messages.iter().map(|m| m.content.len()).sum(),
        candidates_shown: shown,
        max_stops: opts.max_stops,
    });

    let t_cmp = Instant::now();
    let (assembled, debug, usage) = run_plan(
        planner,
        messages,
        query,
        &items,
        opts.max_stops,
        opts.stream,
        on_progress,
    )
    .await?;
    let completion_ms = t_cmp.elapsed().as_millis();

    let dropped = assembled.as_ref().map(|a| a.dropped.clone()).unwrap_or_default();
    let truncated = debug.truncated;
    let mut tour = match assembled {
        Some(a) => a.tour,
        None => {
            let mut t = fallback_tour(query, &items, opts.max_stops);
            t.warnings.push(
                "The tour guide's plan couldn't be used, so this is a ranked itinerary."
                    .to_string(),
            );
            if truncated {
                t.warnings.push(format!(
                    "The guide's reply was cut off before it finished the plan — raise the model's \
max-tokens (currently {}) or lower --max-stops.",
                    planner.config().max_tokens
                ));
            }
            t
        }
    };

    on_progress(TourProgress::Assembling {
        stops: tour.stops.len(),
    });

    tour.seed_id = seed_id;
    tour.retrieval_ms = retrieval_ms;
    tour.completion_ms = completion_ms;
    tour.usage = usage;
    bind_route(&mut tour, &edges);
    bind_candidates(&mut tour, &items, shown);
    add_quality_warnings(&mut tour, &items, &dropped);
    if opts.include_debug {
        tour.debug = Some(debug);
    }
    if opts.include_snippets {
        attach_snippets(&mut tour, repo_root);
    } else {
        for stop in &mut tour.stops {
            stop.snippet = None;
        }
    }
    Ok(tour)
}

/// Retrieval-only tour: skip the LLM and build a ranked itinerary. Used
/// when no chat model is configured, so `ug tour` still does something
/// useful with just the vector store.
pub async fn plan_tour_no_llm(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    repo_root: &std::path::Path,
    query: &str,
    opts: TourOptions<'_>,
) -> Result<Tour, Box<dyn std::error::Error + Send + Sync>> {
    let mut quiet = silent_progress();
    let (items, seed_id, retrieval_ms) =
        gather_candidates(store, embedder, repo_root, query, &opts, false, &mut quiet).await?;

    let mut tour = if items.is_empty() {
        empty_tour(query, retrieval_ms)
    } else {
        fallback_tour(query, &items, opts.max_stops)
    };
    tour.seed_id = seed_id;
    tour.retrieval_ms = retrieval_ms;
    if !items.is_empty() {
        let edges = candidate_edges(store, &items, opts.edge_types, prompt_item_cap(opts.max_stops)).await;
        bind_route(&mut tour, &edges);
        // No guide ran, so nothing was "shown to" one.
        bind_candidates(&mut tour, &items, 0);
    }
    if opts.include_snippets {
        attach_snippets(&mut tour, repo_root);
    }
    Ok(tour)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(idx: usize) -> ContextItem {
        ContextItem {
            id: format!("file:src/a{}.rs", idx),
            name: format!("fn_{}", idx),
            node_type: "Function".into(),
            file: format!("src/a{}.rs", idx),
            start_line: 10,
            end_line: 20,
            description: format!("does thing {}", idx),
            distance: 0.1 * idx as f32,
            hop: 0,
            snippet: Some(format!("fn fn_{}() {{}}", idx)),
        }
    }

    fn item_in(idx: usize, file: &str) -> ContextItem {
        let mut it = item(idx);
        it.file = file.to_string();
        it
    }

    #[test]
    fn parses_clean_json_plan() {
        let raw = r#"{"title":"T","intro":"i","outro":"o","stops":[
            {"ref":2,"title":"start","narration":"here"},
            {"ref":1,"title":"next","narration":"there"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2), item(3)];
        let tour = assemble_from_plan("q", &items, plan, 8).expect("assemble").tour;
        assert_eq!(tour.stops.len(), 2);
        // Order preserved from the plan, not the ranking.
        assert_eq!(tour.stops[0].node_id, "file:src/a2.rs");
        assert_eq!(tour.stops[1].node_id, "file:src/a1.rs");
        assert_eq!(tour.stops[0].title, "start");
        assert!(!tour.fallback);
    }

    #[test]
    fn parses_json_wrapped_in_fences_and_prose() {
        let raw = "Sure! Here is the tour:\n```json\n{\"stops\":[{\"ref\":\"#1\",\"narration\":\"x\"}]}\n```\nEnjoy.";
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1)];
        let tour = assemble_from_plan("q", &items, plan, 8).expect("assemble").tour;
        assert_eq!(tour.stops.len(), 1);
        assert_eq!(tour.stops[0].node_id, "file:src/a1.rs");
    }

    #[test]
    fn drops_out_of_range_and_duplicate_refs_with_reasons() {
        let raw = r#"{"stops":[{"ref":99,"narration":"x"},{"ref":1,"narration":"a"},{"ref":1,"narration":"dup"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2)];
        let a = assemble_from_plan("q", &items, plan, 8).expect("assemble");
        assert_eq!(a.tour.stops.len(), 1, "99 out of range and second ref:1 deduped");
        assert_eq!(a.tour.stops[0].node_id, "file:src/a1.rs");
        assert_eq!(a.dropped.len(), 2);
        assert!(a.dropped[0].reason.contains("outside"));
        assert!(a.dropped[1].reason.contains("already visited"));
    }

    #[test]
    fn no_valid_stops_returns_none() {
        let raw = r#"{"stops":[{"ref":99,"narration":"x"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1)];
        assert!(assemble_from_plan("q", &items, plan, 8).is_none());
    }

    #[test]
    fn fallback_uses_ranked_order_and_descriptions() {
        let items = vec![item(1), item(2), item(3)];
        let tour = fallback_tour("how", &items, 2);
        assert!(tour.fallback);
        assert_eq!(tour.stops.len(), 2);
        assert_eq!(tour.stops[0].node_id, "file:src/a1.rs");
        assert_eq!(tour.stops[0].narration, "does thing 1");
    }

    #[test]
    fn max_stops_caps_plan_and_warns() {
        let raw = r#"{"stops":[{"ref":1,"narration":"a"},{"ref":2,"narration":"b"},{"ref":3,"narration":"c"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2), item(3)];
        let tour = assemble_from_plan("q", &items, plan, 2).expect("assemble").tour;
        assert_eq!(tour.stops.len(), 2);
        assert!(tour.warnings.iter().any(|w| w.contains("trimmed")));
    }

    #[test]
    fn garbage_response_fails_to_parse() {
        assert!(parse_plan("not json at all").is_none());
        assert!(plan_value("not json at all").is_none());
    }

    #[test]
    fn plan_value_returns_the_object_for_the_viewer() {
        let v = plan_value("prefix ```json\n{\"title\":\"T\",\"stops\":[]}\n``` suffix")
            .expect("value");
        assert_eq!(v["title"], "T");
    }

    #[test]
    fn parses_through_a_reasoning_preamble_full_of_braces() {
        // A reasoning model narrating the schema it was given: the prose has
        // braces, so "first { to last }" would fail outright.
        let raw = "Thinking Process:\n\
            *   **Output Format:** Strict JSON ({ \"title\", \"intro\", \"stops\": [...] }).\n\
            *   Constraint: { at most once }\n\n\
            Final answer:\n\
            {\"title\":\"T\",\"intro\":\"i\",\"stops\":[{\"ref\":2,\"narration\":\"x\"}],\"outro\":\"o\"}";
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2)];
        let tour = assemble_from_plan("q", &items, plan, 8).expect("assemble").tour;
        assert_eq!(tour.title, "T");
        assert_eq!(tour.stops[0].node_id, "file:src/a2.rs");
    }

    #[test]
    fn strips_think_tags_before_scanning() {
        let raw = "<think>Maybe {\"stops\":[{\"ref\":9}]} would work? No.</think>\
                   {\"stops\":[{\"ref\":1,\"narration\":\"real\"}]}";
        let v = plan_value(raw).expect("value");
        assert_eq!(v["stops"][0]["ref"], 1);
    }

    #[test]
    fn prefers_the_object_that_looks_like_a_plan() {
        let raw = "{\"note\":\"scratch\"}\nand then\n{\"title\":\"T\",\"stops\":[{\"ref\":1}]}";
        let v = plan_value(raw).expect("value");
        assert_eq!(v["title"], "T");
    }

    #[test]
    fn truncated_object_yields_nothing_rather_than_garbage() {
        let raw = "{\"title\":\"T\",\"stops\":[{\"ref\":1,\"narration\":\"cut off mid";
        assert!(plan_value(raw).is_none());
        assert!(top_level_objects(raw).is_empty());
    }

    #[test]
    fn brace_inside_a_string_does_not_break_scanning() {
        let raw = "{\"stops\":[{\"ref\":1,\"narration\":\"uses fn f() { g(); } here\"}]}";
        let v = plan_value(raw).expect("value");
        assert_eq!(v["stops"][0]["ref"], 1);
    }

    #[test]
    fn prompt_menu_grows_with_the_stop_budget() {
        assert_eq!(prompt_item_cap(4), MIN_PROMPT_ITEMS, "short tours use the floor");
        assert_eq!(prompt_item_cap(20), 28, "a long tour needs more candidates than stops");
        assert_eq!(prompt_item_cap(MAX_STOPS_LIMIT), 48);
        assert!(prompt_item_cap(1000) <= MAX_PROMPT_ITEMS);
    }

    #[test]
    fn system_prompt_carries_the_stop_budget() {
        assert!(tour_system_prompt(6).contains("between 4 and 6 stops"));
        assert!(tour_system_prompt(3).contains("exactly 3 stops"));
        assert!(tour_system_prompt(1).contains("exactly 1 stops"));
    }

    #[test]
    fn diversify_front_loads_file_variety() {
        let items = vec![
            item_in(1, "src/big.rs"),
            item_in(2, "src/big.rs"),
            item_in(3, "src/big.rs"),
            item_in(4, "src/other.rs"),
        ];
        let out = diversify(items, 2);
        let files: Vec<&str> = out.iter().map(|i| i.file.as_str()).collect();
        assert_eq!(files, vec!["src/big.rs", "src/big.rs", "src/other.rs", "src/big.rs"]);
    }

    #[test]
    fn diversify_off_when_cap_is_zero() {
        let items = vec![item_in(1, "a.rs"), item_in(2, "a.rs"), item_in(3, "a.rs")];
        assert_eq!(diversify(items.clone(), 0).len(), 3);
        assert_eq!(diversify(items, 0)[2].name, "fn_3");
    }

    #[test]
    fn render_numbered_items_reports_how_many_fit() {
        let items = vec![item(1), item(2), item(3)];
        let (text, shown) = render_numbered_items(&items, 100_000, 24);
        assert_eq!(shown, 3);
        assert!(text.contains("[#3] fn_3 (Function) — src/a3.rs:10-20"));
        // A tight budget always keeps at least the first item.
        let (_, shown_small) = render_numbered_items(&items, 10, 24);
        assert_eq!(shown_small, 1);
        // The cap alone can also cut the menu short.
        let (_, shown_capped) = render_numbered_items(&items, 100_000, 2);
        assert_eq!(shown_capped, 2);
    }

    #[test]
    fn links_render_only_for_visible_items() {
        let items = vec![item(1), item(2), item(3)];
        let edges = vec![
            TourEdge {
                source: "file:src/a1.rs".into(),
                target: "file:src/a2.rs".into(),
                edge_type: "calls".into(),
            },
            TourEdge {
                source: "file:src/a2.rs".into(),
                target: "file:src/a3.rs".into(),
                edge_type: "imports".into(),
            },
        ];
        let out = render_links(&items, &edges, 2);
        assert!(out.contains("[#1] --calls--> [#2]"));
        assert!(!out.contains("imports"), "item #3 wasn't shown to the guide");
    }

    #[test]
    fn bind_route_labels_transitions_in_both_directions() {
        let items = vec![item(1), item(2), item(3)];
        let plan = parse_plan(
            r#"{"stops":[{"ref":1,"narration":"a"},{"ref":2,"narration":"b"},{"ref":3,"narration":"c"}]}"#,
        )
        .unwrap();
        let mut tour = assemble_from_plan("q", &items, plan, 8).unwrap().tour;
        let edges = vec![
            TourEdge {
                source: "file:src/a1.rs".into(),
                target: "file:src/a2.rs".into(),
                edge_type: "calls".into(),
            },
            // Reverse direction between stops 2 and 3.
            TourEdge {
                source: "file:src/a3.rs".into(),
                target: "file:src/a2.rs".into(),
                edge_type: "imports".into(),
            },
        ];
        bind_route(&mut tour, &edges);
        assert!(tour.stops[0].edge_from_prev.is_none());
        let l1 = tour.stops[1].edge_from_prev.as_ref().unwrap();
        assert_eq!(l1.edge_type, "calls");
        assert!(!l1.reverse);
        let l2 = tour.stops[2].edge_from_prev.as_ref().unwrap();
        assert_eq!(l2.edge_type, "imports");
        assert!(l2.reverse);
        assert_eq!(tour.route.len(), 2);
    }

    #[test]
    fn bind_candidates_flags_used_and_unseen() {
        let items = vec![item(1), item(2), item(3)];
        let plan = parse_plan(r#"{"stops":[{"ref":2,"narration":"b"}]}"#).unwrap();
        let mut tour = assemble_from_plan("q", &items, plan, 8).unwrap().tour;
        bind_candidates(&mut tour, &items, 2);
        assert!(!tour.candidates[0].used);
        assert!(tour.candidates[1].used);
        assert!(tour.candidates[1].shown_to_guide);
        assert!(!tour.candidates[2].shown_to_guide);
    }

    #[test]
    fn warns_when_the_top_hit_is_skipped() {
        let items = vec![item(1), item(2)];
        let plan = parse_plan(r#"{"stops":[{"ref":2,"narration":"b"}]}"#).unwrap();
        let mut tour = assemble_from_plan("q", &items, plan, 8).unwrap().tour;
        tour.seed_id = Some("file:src/a1.rs".into());
        add_quality_warnings(&mut tour, &items, &[]);
        assert!(tour.warnings.iter().any(|w| w.contains("fn_1")));
    }

    #[test]
    fn merge_usage_sums_both_passes() {
        let a = Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
        };
        let b = Usage {
            prompt_tokens: Some(3),
            completion_tokens: None,
            total_tokens: Some(4),
        };
        let m = merge_usage(Some(a), Some(b)).unwrap();
        assert_eq!(m.prompt_tokens, Some(13));
        assert_eq!(m.completion_tokens, Some(5));
        assert_eq!(m.total_tokens, Some(19));
    }
}
