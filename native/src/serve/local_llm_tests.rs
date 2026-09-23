//! Hub-level tests for the in-browser model bridge.
//!
//! These drive [`LocalLlm`] directly rather than through the router, because
//! the interesting states are the ones a browser is *not* in: no tab attached,
//! a tab that vanished mid-answer, two tabs open. The HTTP surface (503s,
//! cross-origin isolation headers, the wllama asset routes, attach/detach
//! moving `chat_default`) is covered in `router_tests.rs`, which has the
//! machinery to build a real `ServeState`.

use super::*;

fn hub() -> LocalLlm {
    LocalLlm::new(8080)
}

fn attached(hub: &LocalLlm, client: &str, n_ctx: u32) -> Attached {
    let a = Attached {
        model: "qwen3-0.6b".into(),
        label: "Qwen3 0.6B".into(),
        n_ctx,
        supports_tools: true,
        backend: "wasm".into(),
        client_id: client.into(),
        since: 0,
    };
    hub.lock().attached = Some(a.clone());
    a
}

#[test]
fn a_request_with_no_tab_attached_is_refused_immediately() {
    let hub = hub();
    assert!(matches!(
        hub.dispatch(json!({})),
        Err(DispatchError::NotAttached)
    ));
}

#[test]
fn a_model_whose_tab_has_gone_is_refused_rather_than_queued() {
    // The attachment outlives the event stream for as long as it takes the
    // server to notice the disconnect. A job dispatched in that window would
    // wait out the full 15-minute timeout against nobody.
    let hub = hub();
    attached(&hub, "ghost", 4096);
    assert!(matches!(
        hub.dispatch(json!({})),
        Err(DispatchError::ClientGone)
    ));
}

#[tokio::test]
async fn a_job_reaches_the_tab_and_its_answer_comes_back() {
    let hub = hub();
    let mut events = hub.connect("tab-1".into());
    attached(&hub, "tab-1", 4096);

    let (id, mut rx) = hub
        .dispatch(json!({ "messages": [{ "role": "user", "content": "hi" }] }))
        .expect("dispatches to the connected tab");

    // The tab sees the job on its event stream…
    let sent = events.recv().await.expect("job event");
    let wire = format!("{sent:?}");
    assert!(wire.contains(&id), "the job event names the job: {wire}");

    // …and answers it in pieces, then finally.
    assert!(hub.push(
        &id,
        JobEvent::Delta {
            content: "hel".into()
        }
    ));
    assert!(hub.push(
        &id,
        JobEvent::Done(Box::new(Completion {
            content: "hello".into(),
            ..Default::default()
        }))
    ));

    match rx.recv().await {
        Some(JobEvent::Delta { content }) => assert_eq!(content, "hel"),
        other => panic!("expected a delta, got {other:?}"),
    }
    match rx.recv().await {
        Some(JobEvent::Done(done)) => assert_eq!(done.content, "hello"),
        other => panic!("expected the completion, got {other:?}"),
    }
}

#[tokio::test]
async fn closing_the_tab_fails_the_answer_it_was_in_the_middle_of() {
    let hub = hub();
    let _events = hub.connect("tab-1".into());
    attached(&hub, "tab-1", 4096);
    let (_id, mut rx) = hub.dispatch(json!({})).expect("dispatched");

    hub.disconnect("tab-1");

    match rx.recv().await {
        Some(JobEvent::Failed(msg)) => assert!(
            msg.contains("tab"),
            "the caller is told what actually happened: {msg}"
        ),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(
        hub.attached().is_none(),
        "the model goes with the tab that was serving it"
    );
}

#[tokio::test]
async fn a_second_tab_disconnecting_leaves_the_serving_tab_alone() {
    let hub = hub();
    let _serving = hub.connect("tab-1".into());
    let _other = hub.connect("tab-2".into());
    attached(&hub, "tab-1", 4096);

    assert!(hub.disconnect("tab-2").is_none());
    assert!(
        hub.attached().is_some(),
        "an unrelated tab closing must not stop the model"
    );
}

#[tokio::test]
async fn abandoning_a_job_tells_the_tab_to_stop() {
    // Nothing else can: the browser is mid-generation in a worker, and the
    // only way it learns the answer is no longer wanted is this event.
    let hub = hub();
    let mut events = hub.connect("tab-1".into());
    attached(&hub, "tab-1", 4096);
    let (id, _rx) = hub.dispatch(json!({})).expect("dispatched");
    let _job_event = events.recv().await;

    hub.retire(&id, true);

    let cancel = format!("{:?}", events.recv().await.expect("cancel event"));
    assert!(cancel.contains("cancel"), "got {cancel}");
    assert!(
        !hub.push(&id, JobEvent::Delta { content: "x".into() }),
        "a retired job accepts nothing further"
    );
}

// Through `plan_prompt`, because that is the budget chat and tour clamp to.
// It used to go through an `Attached::context_chars` that nothing else called,
// so the invariant was being asserted of a number no turn was ever sized by —
// and that number had already drifted from this one by a system prompt.
fn budget_for(n_ctx: u32) -> usize {
    let hub = hub();
    attached(&hub, "c", n_ctx);
    hub.plan_prompt(false)
        .context_chars
        .expect("a browser model always has a budget")
}

#[test]
fn the_retrieval_budget_follows_the_context_window() {
    let small = budget_for(4096);
    let large = budget_for(16384);

    // Both leave room for the answer they are also allowed to generate — the
    // two numbers come from the same reserve on purpose.
    assert!(small < large);
    let completion = Attached {
        n_ctx: 4096,
        ..attached(&hub(), "c", 4096)
    }
    .completion_tokens() as usize;
    assert!(
        small + (completion * CHARS_PER_TOKEN) < 4096 * CHARS_PER_TOKEN,
        "context + completion must fit the window"
    );
    // The default 60 kB chat budget is an order of magnitude past a 4k window,
    // which is the whole reason this clamp exists.
    assert!(small < 20_000);
}

#[test]
fn a_tiny_window_still_gets_a_usable_answer_budget() {
    let tiny = attached(&hub(), "c", 512);
    assert_eq!(tiny.completion_tokens(), MIN_COMPLETION_TOKENS);
    // The window is smaller than the reserve plus the system prompt, so every
    // subtraction saturates to zero and only the floor is left standing.
    assert!(budget_for(512) >= 2_000);
}

#[test]
fn a_hosted_models_token_ceiling_is_cut_down_to_browser_size() {
    // A tour asks for 32768 max_tokens. Passing that through is a promise of
    // a twelve-minute wait on a CPU-bound 0.6B.
    let body = json!({
        "messages": [{ "role": "user", "content": "plan a tour" }],
        "max_tokens": 32768,
        "temperature": 0.2,
    });
    let out = browser_request(&body, 1024);
    assert_eq!(out["max_tokens"], json!(1024));
    assert_eq!(out["temperature"], json!(0.2));
    assert!(out.get("tools").is_none(), "no tools were asked for");
}

#[test]
fn tools_are_forwarded_so_the_agentic_loop_still_works() {
    let body = json!({
        "messages": [],
        "tools": [{ "type": "function", "function": { "name": "search" } }],
        "tool_choice": "auto",
    });
    let out = browser_request(&body, 512);
    assert_eq!(out["tools"][0]["function"]["name"], json!("search"));
    assert_eq!(out["tool_choice"], json!("auto"));
}

#[test]
fn a_tour_cap_never_lets_the_auto_scaled_prompt_back_through() {
    // `plan_tour` grows the planning prompt when `max_context_chars` is at or
    // below the default — so clamping that field alone is not a clamp at all.
    let hub = hub();
    attached(&hub, "tab-1", 4096);
    let plan = hub.plan_prompt(false);
    let budget = plan.context_chars.expect("a browser model has a budget");

    let mut opts = crate::tour::TourOptions::new();
    clamp_tour_opts(&plan, &mut opts);
    assert_eq!(opts.max_context_chars, budget);
    assert_eq!(opts.context_hard_cap, Some(budget));
    // A stop is a menu entry in the prompt *and* a narration in the answer,
    // so the itinerary is bounded by the same window.
    assert!(opts.max_stops <= plan.ui.unwrap().stops);
    assert!(opts.hops <= plan.ui.unwrap().hops);

    // A cap already present is only ever tightened.
    opts.context_hard_cap = Some(budget / 2);
    clamp_tour_opts(&plan, &mut opts);
    assert_eq!(opts.context_hard_cap, Some(budget / 2));
}

#[test]
fn no_browser_model_means_no_clamp() {
    let plan = hub().plan_prompt(false);
    let mut opts = crate::tour::TourOptions::new();
    let before = (opts.max_context_chars, opts.max_stops, opts.hops);
    clamp_tour_opts(&plan, &mut opts);
    assert_eq!(
        (opts.max_context_chars, opts.max_stops, opts.hops),
        before,
        "a hosted model's turn is none of this module's business"
    );
    assert_eq!(opts.context_hard_cap, None);
}

#[test]
fn the_controls_are_capped_to_what_the_window_can_carry() {
    // The panel caps its own inputs from these, and the routes apply them
    // again — a tab that has not seen them, or a curl, must not be able to
    // ask for a pack that cannot fit.
    let hub = hub();

    attached(&hub, "tab-1", 4096);
    let small = hub.plan_prompt(true).ui.expect("limits");
    attached(&hub, "tab-1", 16_384);
    let large = hub.plan_prompt(true).ui.expect("limits");

    assert!(small.chat_k < large.chat_k, "{small:?} vs {large:?}");
    assert!(small.stops <= large.stops);
    assert!(small.chat_k >= 2 && small.stops >= 3, "never unusable: {small:?}");
    assert!(large.chat_k <= 50 && large.results <= 50);
}

#[test]
fn the_bridge_url_is_loopback_and_recognises_itself() {
    let hub = hub();
    let a = attached(&hub, "tab-1", 4096);
    let cfg = hub.bridge_config(&a);
    assert!(
        cfg.base_url.starts_with("http://127.0.0.1:"),
        "the server calls itself, so the URL must not depend on --host: {}",
        cfg.base_url
    );
    assert!(hub.owns(&cfg));

    let elsewhere = crate::chat::ChatConfig::with_overrides(
        Some("https://api.openai.com/v1".into()),
        None,
        Some("gpt-4o-mini".into()),
        None,
        None,
        None,
    );
    assert!(!hub.owns(&elsewhere));
}

#[test]
fn the_real_bound_port_wins_over_the_configured_one() {
    // `ug serve -p 0` is a real thing, and a bridge URL built from the
    // requested port would point at nothing.
    let hub = LocalLlm::new(0);
    hub.set_port(54321);
    let a = attached(&hub, "tab-1", 4096);
    assert_eq!(
        hub.bridge_config(&a).base_url,
        "http://127.0.0.1:54321/api/llm/local/v1"
    );
}

#[test]
fn a_model_with_no_tool_template_is_not_handed_tools() {
    // Chat turns its seed retrieval off when tools are on, because the model
    // is expected to search for itself. A model that cannot call a tool would
    // then answer from no context at all — worse than having no toolbox.
    let hub = hub();
    assert!(
        hub.plan_prompt(true).schemas.is_some(),
        "no attachment means the caller decides"
    );

    let mut sanity = attached(&hub, "tab-1", 32_768);
    sanity.supports_tools = false;
    hub.lock().attached = Some(sanity);
    let plan = hub.plan_prompt(true);
    assert!(plan.schemas.is_none(), "no template, no tools — at any window");
    assert!(
        plan.context_chars.is_some_and(|c| c > 2_000),
        "and the seed pack comes back to fill the gap"
    );
}

#[test]
fn the_toolbox_gives_way_before_the_window_does() {
    // The bug this is about: twelve tool schemas are ~35 kB of JSON, about
    // 9 900 tokens, so a 4k-window model's *first* request was 10 183 tokens
    // and llama.cpp refused it before a single retrieved line was added.
    let hub = hub();

    attached(&hub, "tab-1", 4096);
    let small = hub.plan_prompt(true);
    assert!(
        small.schemas.is_none(),
        "a 4k window cannot hold any toolbox and still answer"
    );
    assert!(small.context_chars.is_some_and(|c| c >= 2_000));

    attached(&hub, "tab-1", 8192);
    let medium = hub.plan_prompt(true);
    let schemas = medium.schemas.expect("8k fits the compact toolbox");
    assert!(
        schemas.len() < crate::mcp::tools::openai_tool_schemas().len(),
        "…the compact one, not all twelve"
    );
    assert!(
        tokens_of_json(&schemas) < 4_000,
        "compact means compact: {} tokens",
        tokens_of_json(&schemas)
    );
    assert_eq!(medium.tool_rounds, Some(BROWSER_TOOL_ROUNDS));
    assert!(
        medium.tool_result_chars.is_some_and(|c| c < 60_000),
        "one 60 kB tool result is five times this model's whole window"
    );

    // Whatever tier is chosen, the pieces have to add up to less than the
    // window — which is the sum that was failing.
    for n_ctx in [2048, 4096, 8192, 16_384, 32_768] {
        attached(&hub, "tab-1", n_ctx);
        let plan = hub.plan_prompt(true);
        let tools = plan.schemas.as_deref().map_or(0, tokens_of_json);
        let results = plan.tool_result_chars.unwrap_or(0) / CHARS_PER_TOKEN
            * plan.tool_rounds.unwrap_or(0);
        let context = plan.context_chars.unwrap_or(0) / CHARS_PER_TOKEN;
        let worst_case = tools as usize
            + results.max(context)
            + hub.attached().unwrap().completion_tokens() as usize;
        assert!(
            worst_case < n_ctx as usize,
            "n_ctx={n_ctx}: planned {worst_case} tokens of prompt + answer"
        );
    }
}

#[test]
fn a_compact_description_keeps_the_first_sentence() {
    let long = "Find inbound references to a symbol — callers of a function.                 Convenience wrapper over traverse with direction='inbound'.                 Use this when the user asks 'who uses X'.";
    let short = shorten(long, COMPACT_DESC_CHARS);
    assert!(short.starts_with("Find inbound references"));
    assert!(!short.contains("Convenience wrapper"));
    assert_eq!(shorten("Already short.", 200), "Already short.");
}

#[test]
#[ignore = "prints the tier table; run with --ignored --nocapture when tuning"]
fn print_the_budget_table() {
    let hub = hub();
    println!(
        "full toolbox: {} tokens, compact: {} tokens, minimal: {} tokens, suffix: {}, system: {}",
        tokens_of_json(&crate::mcp::tools::openai_tool_schemas()),
        tokens_of_json(&compact_tool_schemas()),
        tokens_of_json(&minimal_tool_schemas()),
        tokens_of_text(crate::chat::TOOL_SYSTEM_SUFFIX),
        tokens_of_text(crate::chat::DEFAULT_SYSTEM_PROMPT),
    );
    for n_ctx in [2048, 4096, 8192, 16_384, 32_768] {
        attached(&hub, "t", n_ctx);
        let p = hub.plan_prompt(true);
        println!(
            "n_ctx={n_ctx:>6}  tools={:<8} ctx_chars={:<7} result_chars={:<7} rounds={:?}",
            p.schemas.as_ref().map_or("none".into(), |s| format!("{}", s.len())),
            p.context_chars.unwrap_or(0),
            p.tool_result_chars.unwrap_or(0),
            p.tool_rounds,
        );
    }
}
