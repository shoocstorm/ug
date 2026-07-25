## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. LLM Features Must Feel Fast

**Most of the wall-clock is the model writing tokens. Budget them like money.**

`ug` is routinely pointed at a local endpoint doing ~30 tokens/second. At that rate a
feature is fast or slow depending almost entirely on how many tokens it asks for and how
long the user waits before seeing anything. Four rules, in order of impact:

1. **Turn thinking off unless the task needs it.** A reasoning model spends tens of
   thousands of tokens deliberating *before* its first useful character — the guided tour
   took over ten minutes this way. Prompt instructions do NOT stop it: thinking lives in
   the chat template. Send the real switches instead — `chat::no_think_body()`
   (`chat_template_kwargs.enable_thinking:false` + `reasoning_effort:"low"`), which
   `ChatClient` merges via `ChatConfig::extra_body` and retries without on a 4xx. Same
   tour, same model: **10+ min → 9.4 s**. Give users `--think` to opt back in.
2. **Show progress from the first second.** Stream the completion and report phases
   (`TourProgress` → SSE `event: progress`, or the CLI's in-place status line): what was
   retrieved, which model is writing, tokens so far, rate, elapsed. A silent wait feels
   broken; a narrated one feels like work being done.
3. **Deliver partial results as they arrive.** Don't wait for the whole reply to be
   parseable. `StopScanner` pulls each finished stop out of the still-streaming JSON so the
   tour starts walking on stop 1 — roughly half the wait removed on every tour.
4. **Ask for less.** Shorter prompts (clip snippets and descriptions; only send code for
   the top candidates) and shorter outputs (cap narration length explicitly) cut generation
   time directly. The tour's prompt went 10.8k → 6.9k chars with no loss of quality.

Verify with a real slow endpoint, not a mock: mocks answer instantly and hide exactly the
problem you're trying to fix.

## 6. Test Before Commit

**Always verify changes with tests before marking a task complete.**

### Rust Tests (Native Code)
- Run `cd native && cargo test` to execute all Rust tests
- Tests are in `native/tests/`: `indexer_test.rs`, `graph_test.rs`, `search_test.rs`,
  `storage_test.rs`, `rust_indexer_test.rs`, `pdf_indexer_test.rs`, `storage_bench.rs`,
  and the `#[ignore]`-gated `neo4j_smoke.rs` / `neo4j_write_smoke.rs`
- **Run these tests after every code change in the native folder**
- If adding new functionality, add corresponding test cases to the test files
- Ensure all tests pass before completing a phase

### Verification Checklist
```
1. cd native && cargo test              → all tests must pass
2. cd native && cargo build --release   → native module must build
3. ./native/target/release/ug help      → CLI works
```

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
