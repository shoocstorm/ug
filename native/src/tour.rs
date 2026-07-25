//! Guided graph tours: turn a question into an ordered, narrated
//! walkthrough of the knowledge graph.
//!
//! Pipeline:
//!   1. GraphRAG retrieval (`chat::retrieve_context`) — the same PPR-fused
//!      semantic + structural search chat uses, so a tour visits the nodes
//!      that actually matter for the question.
//!   2. An LLM "tour guide" pass that *orders* a subset of those nodes into
//!      a coherent narrative (entry point → detail → payoff) and writes
//!      per-stop narration. The guide references items by their `[#N]`
//!      number so every stop binds back to a real graph node id.
//!   3. A `Tour` whose stops carry `node_id`/`file`/lines — enough for the
//!      UI to fly the camera and for the CLI to print an itinerary.
//!
//! Shared by `ug tour` (CLI) and `POST /api/tour` (serve/UI) so both
//! entry points produce identical tours.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use ultragraph::storage::{
    read_snippet, ContextItem, Direction, Embedder, KnowledgeStore, RankStrategy,
};

use crate::chat::{retrieve_context, ChatClient, ChatMessage, ChatRagOptions, Usage};

/// Default number of stops when the caller doesn't specify one. Small
/// enough to stay a "guided" tour rather than a full dump.
pub const DEFAULT_MAX_STOPS: usize = 8;

/// Per-stop snippet cap. A tour shows a *taste* of each node, and this
/// also stops one huge (e.g. minified) file from dominating the payload.
const TOUR_SNIPPET_MAX_CHARS: usize = 900;
const TOUR_SNIPPET_MAX_LINES: usize = 22;

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
    pub retrieval_ms: u128,
    pub completion_ms: u128,
    pub usage: Option<Usage>,
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
}

impl<'a> TourOptions<'a> {
    pub fn new() -> Self {
        Self {
            // A tour wants a slightly wider net than chat: more candidate
            // nodes gives the guide room to build a real narrative arc.
            k: 12,
            hops: 2,
            max_stops: DEFAULT_MAX_STOPS,
            strategy: RankStrategy::Ppr,
            direction: Direction::Both,
            edge_types: None,
            include_snippets: true,
            max_context_chars: crate::chat::DEFAULT_CTX_MAX_CHARS,
            where_clause: None,
        }
    }
}

impl<'a> Default for TourOptions<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// System prompt for the tour-guide planning pass. Asks for strict JSON
/// so the plan maps cleanly back onto real nodes.
pub const TOUR_SYSTEM_PROMPT: &str = "You are UltraGraph's Tour Guide. You are given a numbered set of \
code/knowledge context items ([#1], [#2], …) retrieved from a knowledge graph over the user's \
repository, plus a question. Design a short guided walking tour that ANSWERS the question by visiting a \
subset of these items in a logical narrative order — typically begin at the entry point and follow the \
flow of control, data, or dependencies from there.\n\n\
Return ONLY a single JSON object — no prose before or after, no markdown code fences — of exactly this shape:\n\
{\n\
  \"title\": \"<a short, engaging title for the tour>\",\n\
  \"intro\": \"<1-2 sentences framing what we'll walk through>\",\n\
  \"stops\": [\n\
    { \"ref\": <the [#N] number of the item to visit>, \"title\": \"<short stop headline>\", \"narration\": \"<2-4 sentences: this item's role in answering the question, and how it connects to the previous or next stop>\" }\n\
  ],\n\
  \"outro\": \"<1-2 sentences with the takeaway>\"\n\
}\n\n\
Rules:\n\
- Use only `ref` numbers that appear in the provided items.\n\
- Order the stops as a story (entry → detail → conclusion), NOT by relevance score.\n\
- Prefer 4 to 8 stops; skip items that don't advance the narrative.\n\
- Ground every narration in the shown items; never invent code that isn't present.\n\
- Write narration in a warm, second-person guide voice (\"Notice how…\", \"From here we follow…\").";

// ---------- LLM plan shapes ----------

#[derive(Deserialize)]
struct PlanRaw {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    intro: Option<String>,
    #[serde(default)]
    outro: Option<String>,
    #[serde(default)]
    stops: Option<Vec<PlanStop>>,
}

#[derive(Deserialize)]
struct PlanStop {
    // Accept the common field spellings a model might emit.
    #[serde(default, alias = "index", alias = "n", alias = "item", alias = "id")]
    r#ref: Option<serde_json::Value>,
    #[serde(default)]
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

/// Best-effort extraction of the JSON object from a model response.
/// Tries a direct parse, then the substring between the first `{` and the
/// last `}` (handles stray prose or ```json fences around the object).
fn parse_plan(raw: &str) -> Option<PlanRaw> {
    let trimmed = raw.trim();
    if let Ok(p) = serde_json::from_str::<PlanRaw>(trimmed) {
        return Some(p);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<PlanRaw>(&trimmed[start..=end]).ok()
}

/// Build a `TourStop` from a retrieved context item + guide-supplied text.
fn stop_from_item(ref_index: usize, item: &ContextItem, title: Option<&str>, narration: &str) -> TourStop {
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
    }
}

/// Turn a parsed LLM plan into a `Tour`, binding each `ref` to a real
/// retrieved item. Returns `None` when no stop resolves to a valid item —
/// the caller then falls back to a ranked itinerary.
fn assemble_from_plan(query: &str, items: &[ContextItem], plan: PlanRaw, max_stops: usize) -> Option<Tour> {
    let plan_stops = plan.stops?;
    let mut stops: Vec<TourStop> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ps in &plan_stops {
        if stops.len() >= max_stops {
            break;
        }
        let Some(idx) = ps.r#ref.as_ref().and_then(coerce_ref) else {
            continue;
        };
        if idx == 0 || idx > items.len() {
            continue;
        }
        let item = &items[idx - 1];
        // One node per tour — revisiting the same node breaks the camera
        // narrative and reads as a stutter.
        if !seen.insert(item.id.clone()) {
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
    Some(Tour {
        query: query.to_string(),
        title: plan
            .title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_title(query)),
        intro: plan.intro.map(|s| s.trim().to_string()).unwrap_or_default(),
        outro: plan.outro.map(|s| s.trim().to_string()).unwrap_or_default(),
        stops,
        seed_id: None,
        fallback: false,
        retrieval_ms: 0,
        completion_ms: 0,
        usage: None,
    })
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
    Tour {
        query: query.to_string(),
        title: default_title(query),
        intro: format!(
            "The most relevant parts of the codebase for \u{201c}{}\u{201d}, in ranked order.",
            query
        ),
        outro: String::new(),
        stops,
        seed_id: None,
        fallback: true,
        retrieval_ms: 0,
        completion_ms: 0,
        usage: None,
    }
}

fn empty_tour(query: &str, retrieval_ms: u128) -> Tour {
    Tour {
        query: query.to_string(),
        title: default_title(query),
        intro: "No matching nodes were found in this project's knowledge graph for that question."
            .to_string(),
        outro: String::new(),
        stops: Vec::new(),
        seed_id: None,
        fallback: true,
        retrieval_ms,
        completion_ms: 0,
        usage: None,
    }
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
/// the whole tour to one stop. We attach bounded snippets per chosen stop
/// afterwards instead — see `attach_snippets`.
fn retrieval_opts<'a>(opts: &TourOptions<'a>) -> ChatRagOptions<'a> {
    let mut rag = ChatRagOptions::new();
    rag.k = opts.k;
    rag.hops = opts.hops;
    rag.strategy = opts.strategy;
    rag.direction = opts.direction;
    rag.edge_types = opts.edge_types;
    rag.include_snippets = false;
    rag.max_context_chars = opts.max_context_chars;
    rag.where_clause = opts.where_clause;
    rag
}

/// Read a short, bounded source snippet for each stop (best effort). Runs
/// only when the caller wants snippets; a stop whose file can't be read
/// simply keeps `snippet: None`.
fn attach_snippets(tour: &mut Tour, repo_root: &std::path::Path) {
    for stop in &mut tour.stops {
        let Some(full) = read_snippet(repo_root, &stop.file, stop.start_line, stop.end_line) else {
            continue;
        };
        let mut out = String::new();
        for (i, line) in full.lines().enumerate() {
            if i >= TOUR_SNIPPET_MAX_LINES || out.len() >= TOUR_SNIPPET_MAX_CHARS {
                out.push('\u{2026}');
                break;
            }
            // Clip pathologically long (minified) lines so one line can't
            // blow the whole budget. Char-based so we never split a UTF-8
            // sequence mid-byte.
            if line.chars().count() > TOUR_SNIPPET_MAX_CHARS {
                out.extend(line.chars().take(TOUR_SNIPPET_MAX_CHARS));
                out.push('\u{2026}');
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        let trimmed = out.trim_end_matches('\n');
        if !trimmed.is_empty() {
            stop.snippet = Some(trimmed.to_string());
        }
    }
}

/// Assemble the tour-guide prompt (system + numbered context + question).
fn build_plan_messages(query: &str, items: &[ContextItem], ctx_max_chars: usize) -> Vec<ChatMessage> {
    let rendered = crate::chat::render_context(items, ctx_max_chars);
    let user = format!(
        "Question: {}\n\nContext items:\n\n{}\n\nDesign the guided tour as JSON now.",
        query,
        rendered.trim_end()
    );
    vec![
        ChatMessage {
            role: "system".into(),
            content: TOUR_SYSTEM_PROMPT.into(),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ]
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
    let rag = retrieval_opts(&opts);
    let t_ret = Instant::now();
    let context = retrieve_context(store, embedder, repo_root, query, &rag).await?;
    let retrieval_ms = t_ret.elapsed().as_millis();

    if context.items.is_empty() {
        return Ok(empty_tour(query, retrieval_ms));
    }

    let messages = build_plan_messages(query, &context.items, opts.max_context_chars);
    let t_cmp = Instant::now();
    let (answer, usage) = chat.complete(&messages).await?;
    let completion_ms = t_cmp.elapsed().as_millis();

    let mut tour = parse_plan(&answer)
        .and_then(|plan| assemble_from_plan(query, &context.items, plan, opts.max_stops))
        .unwrap_or_else(|| fallback_tour(query, &context.items, opts.max_stops));

    tour.seed_id = context.seed_id.clone();
    tour.retrieval_ms = retrieval_ms;
    tour.completion_ms = completion_ms;
    tour.usage = usage;
    if opts.include_snippets {
        attach_snippets(&mut tour, repo_root);
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
    let rag = retrieval_opts(&opts);
    let t_ret = Instant::now();
    let context = retrieve_context(store, embedder, repo_root, query, &rag).await?;
    let retrieval_ms = t_ret.elapsed().as_millis();

    let mut tour = if context.items.is_empty() {
        empty_tour(query, retrieval_ms)
    } else {
        fallback_tour(query, &context.items, opts.max_stops)
    };
    tour.seed_id = context.seed_id.clone();
    tour.retrieval_ms = retrieval_ms;
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

    #[test]
    fn parses_clean_json_plan() {
        let raw = r#"{"title":"T","intro":"i","outro":"o","stops":[
            {"ref":2,"title":"start","narration":"here"},
            {"ref":1,"title":"next","narration":"there"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2), item(3)];
        let tour = assemble_from_plan("q", &items, plan, 8).expect("assemble");
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
        let tour = assemble_from_plan("q", &items, plan, 8).expect("assemble");
        assert_eq!(tour.stops.len(), 1);
        assert_eq!(tour.stops[0].node_id, "file:src/a1.rs");
    }

    #[test]
    fn drops_out_of_range_and_duplicate_refs() {
        let raw = r#"{"stops":[{"ref":99,"narration":"x"},{"ref":1,"narration":"a"},{"ref":1,"narration":"dup"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2)];
        let tour = assemble_from_plan("q", &items, plan, 8).expect("assemble");
        assert_eq!(tour.stops.len(), 1, "99 out of range and second ref:1 deduped");
        assert_eq!(tour.stops[0].node_id, "file:src/a1.rs");
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
    fn max_stops_caps_plan() {
        let raw = r#"{"stops":[{"ref":1,"narration":"a"},{"ref":2,"narration":"b"},{"ref":3,"narration":"c"}]}"#;
        let plan = parse_plan(raw).expect("parse");
        let items = vec![item(1), item(2), item(3)];
        let tour = assemble_from_plan("q", &items, plan, 2).expect("assemble");
        assert_eq!(tour.stops.len(), 2);
    }

    #[test]
    fn garbage_response_fails_to_parse() {
        assert!(parse_plan("not json at all").is_none());
    }
}
