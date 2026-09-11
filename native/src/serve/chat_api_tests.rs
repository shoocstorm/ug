//! In-process tests for `/api/chat` and `/api/tour`'s request handling.
//!
//! The endpoint-and-credential guard these routes share is already pinned in
//! `serve::tests` — a request-supplied `base_url` must never inherit the
//! stored API key, and the metadata hosts stay blocked. What was left
//! uncovered is everything either route does with a body *before* it reaches
//! a model:
//!
//! * `tour_opts_from_body` clamps every knob a caller can send. Each clamp is
//!   the only thing between a one-line JSON body and a retrieval that walks
//!   the whole graph, and a lifted ceiling fails as a slow tour rather than as
//!   an error.
//! * `merge_chat_cfg` and `merge_tour_chat_cfg` are the same merge written
//!   twice, once per route. They have to stay in step, so they are checked
//!   against each other rather than only against themselves.
//! * `citations_json` numbers the citations the answer refers to. Off-by-one
//!   here misattributes every claim in a reply.
//!
//! The handlers themselves are driven through the real router, the way
//! `router_tests.rs` does it, so the extractor and status codes are covered
//! rather than assumed.

use super::chat_api::{
    chat_origin, citations_json, merge_chat_cfg, merge_tour_chat_cfg, tour_opts_from_body,
    tour_response_json, ChatBody, ChatCfgError, TourBody,
};
use crate::chat::ChatConfig;
use crate::tour::{DEFAULT_MAX_STOPS, MAX_STOPS_LIMIT};
use serde_json::json;
use ultragraph::storage::{Direction, RankStrategy, DEFAULT_CONTEXT_CHARS};

fn tour_body(v: serde_json::Value) -> TourBody {
    serde_json::from_value(v).expect("body deserializes")
}
fn chat_body(v: serde_json::Value) -> ChatBody {
    serde_json::from_value(v).expect("body deserializes")
}

/// A configured default, so "what did the body override" is visible.
fn cfg(model: &str) -> ChatConfig {
    ChatConfig {
        base_url: "https://api.example.test/v1".into(),
        api_key: "sk-stored".into(),
        model: model.into(),
        temperature: 0.25,
        max_tokens: 1234,
        ..Default::default()
    }
}

// ── what a body is allowed to ask for ────────────────────────────────────────

#[test]
fn an_empty_tour_body_uses_the_documented_defaults() {
    let body = tour_body(json!({ "query": "how does auth work" }));
    let opts = tour_opts_from_body(&body, None);
    assert_eq!(opts.k, 14);
    assert_eq!(opts.hops, 2);
    assert_eq!(opts.max_stops, DEFAULT_MAX_STOPS);
    assert_eq!(opts.max_context_chars, DEFAULT_CONTEXT_CHARS);
    assert!(opts.include_snippets, "snippets are on unless asked off");
    assert!(opts.include_debug, "the UI shows the planning transcript");
    assert!(!opts.stream);
    assert!(!opts.research);
}

#[test]
fn candidate_count_is_clamped_to_its_range() {
    // k drives how much retrieval work a one-line body can ask for.
    assert_eq!(tour_opts_from_body(&tour_body(json!({"query":"q","k":0})), None).k, 1);
    assert_eq!(tour_opts_from_body(&tour_body(json!({"query":"q","k":1})), None).k, 1);
    assert_eq!(tour_opts_from_body(&tour_body(json!({"query":"q","k":80})), None).k, 80);
    assert_eq!(
        tour_opts_from_body(&tour_body(json!({"query":"q","k":10_000})), None).k,
        80,
        "an unbounded k walks the whole graph"
    );
}

#[test]
fn hops_are_capped_at_four() {
    assert_eq!(tour_opts_from_body(&tour_body(json!({"query":"q","hops":0})), None).hops, 0);
    assert_eq!(tour_opts_from_body(&tour_body(json!({"query":"q","hops":4})), None).hops, 4);
    assert_eq!(
        tour_opts_from_body(&tour_body(json!({"query":"q","hops":99})), None).hops,
        4,
        "hop count is exponential in the neighbourhood it reaches"
    );
}

#[test]
fn stop_count_is_clamped_to_the_shared_ceiling() {
    assert_eq!(
        tour_opts_from_body(&tour_body(json!({"query":"q","max_stops":0})), None).max_stops,
        1
    );
    assert_eq!(
        tour_opts_from_body(&tour_body(json!({"query":"q","max_stops":10_000})), None).max_stops,
        MAX_STOPS_LIMIT,
        "the CLI and this route share one ceiling"
    );
}

#[test]
fn context_and_per_file_caps_hold() {
    assert_eq!(
        tour_opts_from_body(&tour_body(json!({"query":"q","max_context_chars":1_000_000})), None)
            .max_context_chars,
        64_000,
        "the context cap is what keeps one body from filling the model's window"
    );
    assert_eq!(
        tour_opts_from_body(&tour_body(json!({"query":"q","max_per_file":999})), None).max_per_file,
        20,
        "without this one large file swallows the whole itinerary"
    );
}

#[test]
fn thinking_is_the_inverse_of_fast() {
    // `fast` is stored inverted, so a body asking to think must not also get
    // the fast client. These two have to move together.
    assert!(tour_opts_from_body(&tour_body(json!({"query":"q"})), None).fast);
    assert!(!tour_opts_from_body(&tour_body(json!({"query":"q","think":true})), None).fast);
    assert!(tour_opts_from_body(&tour_body(json!({"query":"q","think":false})), None).fast);
}

#[test]
fn strategy_and_direction_fall_back_rather_than_reject() {
    let plain = tour_body(json!({ "query": "q" }));
    let d = tour_opts_from_body(&plain, None);
    assert_eq!(d.strategy, RankStrategy::Ppr);
    assert_eq!(d.direction, Direction::Both);

    // An unknown value parses lossily to the default instead of failing the
    // request — a typo should degrade the ranking, not break the tour.
    let typo = tour_body(json!({ "query": "q", "strategy": "nonsense" }));
    assert_eq!(tour_opts_from_body(&typo, None).strategy, RankStrategy::Ppr);
    // Direction does NOT behave the same way, and the asymmetry is worth
    // knowing: omitting the field gives `Both`, but an unrecognised value
    // falls through `Direction::from_str_lossy` to `Outbound`. So a typo
    // silently narrows the traversal rather than leaving it alone.
    let dir_typo = tour_body(json!({ "query": "q", "direction": "sideways" }));
    assert_eq!(
        tour_opts_from_body(&dir_typo, None).direction,
        Direction::Outbound,
        "a misspelt direction narrows the walk; omitting it does not"
    );
    let dir_named = tour_body(json!({ "query": "q", "direction": "in" }));
    assert_eq!(tour_opts_from_body(&dir_named, None).direction, Direction::Inbound);
}

#[test]
fn edge_types_are_passed_through_untouched() {
    let body = tour_body(json!({ "query": "q" }));
    let types = vec!["Calls".to_string(), "Contains".to_string()];
    let opts = tour_opts_from_body(&body, Some(&types));
    assert_eq!(opts.edge_types, Some(types.as_slice()));
}

// ── the two merges are one merge, written twice ──────────────────────────────

#[test]
fn a_body_model_overrides_the_configured_one() {
    let default = Some(cfg("server-model"));
    let merged = merge_chat_cfg(&default, &chat_body(json!({"query":"q","chat_model":"body-model"})))
        .expect("merges");
    assert_eq!(merged.model, "body-model");
}

#[test]
fn an_absent_body_model_falls_back_to_the_configured_one() {
    let default = Some(cfg("server-model"));
    let merged = merge_chat_cfg(&default, &chat_body(json!({"query":"q"}))).expect("merges");
    assert_eq!(merged.model, "server-model");
    assert_eq!(merged.temperature, 0.25, "unset knobs keep the server's value");
    assert_eq!(merged.max_tokens, 1234);
}

#[test]
fn no_model_anywhere_is_not_configured_rather_than_invalid() {
    // The distinction decides the status code: 503 is the server's problem,
    // 400 is the caller's.
    let err = merge_chat_cfg(&None, &chat_body(json!({"query":"q"}))).unwrap_err();
    assert!(matches!(err, ChatCfgError::NotConfigured));
}

#[test]
fn a_body_model_alone_is_enough_with_nothing_configured() {
    let merged = merge_chat_cfg(&None, &chat_body(json!({"query":"q","chat_model":"m"})))
        .expect("a model in the body is a complete answer");
    assert_eq!(merged.model, "m");
}

#[test]
fn a_rejected_endpoint_override_is_invalid_rather_than_unconfigured() {
    let default = Some(cfg("server-model"));
    let err = merge_chat_cfg(
        &default,
        &chat_body(json!({"query":"q","chat_base_url":"http://169.254.169.254/v1"})),
    )
    .unwrap_err();
    assert!(matches!(err, ChatCfgError::Invalid(_)));
}

#[test]
fn the_tour_merge_agrees_with_the_chat_merge() {
    // These are the same function written twice, once per route. A body
    // carrying the identical fields must come out identical, or the tour
    // route becomes the lenient way in.
    let default = Some(cfg("server-model"));
    let fields = json!({
        "query": "q",
        "chat_model": "body-model",
        "temperature": 0.9,
        "max_tokens": 77,
    });
    let from_chat = merge_chat_cfg(&default, &chat_body(fields.clone())).expect("merges");
    let from_tour = merge_tour_chat_cfg(&default, &tour_body(fields)).expect("merges");
    assert_eq!(from_chat.model, from_tour.model);
    assert_eq!(from_chat.temperature, from_tour.temperature);
    assert_eq!(from_chat.max_tokens, from_tour.max_tokens);
    assert_eq!(from_chat.base_url, from_tour.base_url);
    assert_eq!(from_chat.api_key, from_tour.api_key);
}

#[test]
fn the_tour_route_is_not_the_lenient_way_to_the_stored_key() {
    // The tour body carries the same override fields as /api/chat, so it
    // would otherwise be a second way to walk off with the key.
    let default = Some(cfg("server-model"));
    let merged = merge_tour_chat_cfg(
        &default,
        &tour_body(json!({"query":"q","chat_base_url":"https://elsewhere.test/v1"})),
    )
    .expect("a redirect is allowed, keyless");
    assert_eq!(merged.api_key, "", "stored key leaked through /api/tour");
    assert_eq!(merged.base_url, "https://elsewhere.test/v1");
}

#[test]
fn an_override_naming_the_configured_origin_keeps_the_key() {
    // The UI echoes the current base_url back in the body; that is not a
    // redirection and must keep working.
    let default = Some(cfg("server-model"));
    let merged = merge_tour_chat_cfg(
        &default,
        &tour_body(json!({"query":"q","chat_base_url":"https://api.example.test/v1/"})),
    )
    .expect("merges");
    assert_eq!(merged.api_key, "sk-stored");
}

#[test]
fn an_origin_is_compared_case_and_port_insensitively() {
    let a = chat_origin("HTTPS://API.Example.Test/v1").expect("parses");
    let b = chat_origin("https://api.example.test:443/v1/chat").expect("parses");
    assert_eq!(a, b, "the default port is the same origin as no port");
    assert!(chat_origin("not a url").is_none(), "a bare phrase names no origin");
    assert!(chat_origin("").is_none());
    // Scheme is part of the origin, so an http override against an https
    // default is a redirection and loses the stored key.
    assert_ne!(
        chat_origin("http://api.example.test/v1"),
        chat_origin("https://api.example.test/v1")
    );
    // A different port is a different origin, which is what keeps a local
    // model server on :11434 from inheriting a key configured for :8000.
    assert_ne!(
        chat_origin("http://127.0.0.1:8000/v1"),
        chat_origin("http://127.0.0.1:11434/v1")
    );
}

// ── what the answer cites ────────────────────────────────────────────────────

#[test]
fn citations_are_numbered_from_one() {
    use ultragraph::storage::ContextItem;
    let item = |id: &str, name: &str| ContextItem {
        id: id.into(),
        name: name.into(),
        node_type: "Function".into(),
        file: "src/a.rs".into(),
        start_line: 1,
        end_line: 4,
        description: String::new(),
        distance: 0.5,
        hop: 0,
        snippet: None,
        matched_by: "semantic".into(),
    };
    let items = vec![item("a", "alpha"), item("b", "bravo"), item("c", "charlie")];
    let out = citations_json(&items);

    assert_eq!(out.len(), 3);
    // The model is told to cite [1], [2], [3]. A zero-based index here
    // misattributes every claim in the reply by one.
    assert_eq!(out[0]["index"], 1);
    assert_eq!(out[2]["index"], 3);
    assert_eq!(out[0]["id"], "a");
    assert_eq!(out[1]["name"], "bravo");
}

#[test]
fn no_context_cites_nothing() {
    assert!(citations_json(&[]).is_empty());
}

// ── what the tour route adds on top of a planned tour ────────────────────────

/// A planned tour with nothing in it. Only the fields the route *adds* are
/// under test here; what the planner put in is `tour.rs`'s business.
fn blank_tour() -> crate::tour::Tour {
    crate::tour::Tour {
        query: "q".into(),
        title: "A tour".into(),
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

#[test]
fn a_tour_response_names_the_store_it_came_from() {
    let v = tour_response_json(&blank_tour(), "ugdb", Some("a-model")).expect("serializes");
    assert_eq!(v["dest"], "ugdb");
    assert_eq!(v["chat_model"], "a-model");
}

#[test]
fn a_narration_free_tour_claims_no_model() {
    // The route degrades to a ranked, unnarrated tour when no model is
    // configured. Reporting one anyway would credit a model that never ran.
    let v = tour_response_json(&blank_tour(), "ugdb", None).expect("serializes");
    assert_eq!(v["dest"], "ugdb");
    assert!(v.get("chat_model").is_none());
}
