//! Every integration test in this directory, compiled into one binary —
//! the `integration` test target, declared explicitly in `Cargo.toml`.
//!
//! Cargo gives each `tests/*.rs` file its own executable by default. That is
//! 25 separate links of the same ~30 MB of statically linked dependency code
//! (tokio, reqwest, ort, five tree-sitter grammars, overgraph, ...), and on
//! macOS every one of those freshly linked Mach-Os is scanned by `syspolicyd`
//! and `XprotectService` the first time it runs. Sampling one relink on the
//! 18-core dev machine: 68s wall clock at 6% CPU, of which rustc accounted for
//! ~10s and Gatekeeper for ~58s. A change under `src/` relinks every binary,
//! so the suite paid that toll 25 times over.
//!
//! Declaring the files as modules of one harness collapses that to a single
//! link and a single scan. Measured, same 885 tests, after `touch src/lib.rs`:
//!
//! ```text
//!                binaries   wall clock   test execution
//!   before             27      17m 54s          4.274s
//!   after               3       2m 09s          4.278s
//! ```
//!
//! The execution phase is unchanged because nextest schedules process-per-test
//! from one global pool — binary count enters the build and list phases, never
//! the scheduler. Each test still runs in its own process, so nothing that was
//! isolated before is sharing state now.
//!
//! Two things happened after that table, and the second one matters more:
//!
//! 1. The `ug(bin test)` harness is gone. The CLI/server modules moved from
//!    `main.rs` into the library (see the module map in `src/lib.rs`) and
//!    `[[bin]] test = false` retired the second compile of `main.rs`, taking
//!    an edit under `src/` from four freshly linked Mach-Os to three and from
//!    233 MB of them to 207 MB. Same 888 tests, now split 607 in the lib
//!    harness and 281 here. Worth roughly a third of the CPU time of a
//!    rebuild-and-list cycle (~29.5s → ~19.9s of user+sys, medians of six and
//!    five alternating runs); wall clock on a loaded machine varies too much
//!    to quote.
//!
//! 2. The machine-level half of the fix, which is not in the repo: adding the
//!    app that hosts your shell (VS Code, Ghostty, Terminal) to System
//!    Settings → Privacy & Security → Developer Tools exempts what it spawns
//!    from the Gatekeeper assessment entirely. **Quit and relaunch the app
//!    after granting it** — TCC resolves against the responsible process. That
//!    is what turns the 2m 09s above into ~20-30s, and it is verifiable:
//!
//!    ```text
//!    log show --predicate 'process == "syspolicyd" AND \
//!      eventMessage CONTAINS "evaluateScanResult"' --start "<build start>"
//!    ```
//!
//!    Zero results across a rebuild that linked and ran all three binaries
//!    means the exemption is live. `syspolicyd` burning CPU anyway is it
//!    scanning some *other* app, not this build.
//!
//! **Adding a test file**: drop it in `tests/` and add a `mod` line below.
//! `autotests = false` in `Cargo.toml` means a file with no `mod` line here is
//! silently not compiled — the same trade every explicit manifest makes.
//!
//! **Running a subset** is now a filter rather than a `--test` flag, because
//! there is only one binary:
//! ```text
//! cargo nextest run -E 'test(/^search_test::/)'   # one former file
//! cargo nextest run -E 'test(the_name_of_a_test)' # one test
//! cargo nextest run --test integration            # everything here, no unit tests
//! cargo nextest run --lib                         # the unit tests, not these
//! ```

// Shared fixture helpers, not a test file. Declared here rather than inside
// `graph_bench` so the path resolves from the harness root.
mod graph_baseline;

mod analyze_test;
mod bm25_test;
mod boundary_test;
mod centrality_test;
mod cross_file_resolution_test;
mod embed_concurrency_test;
mod enum_names_test;
mod facts_test;
mod graph_bench;
mod graph_test;
mod incremental_ingest_test;
mod indexer_test;
mod java_indexer_test;
mod markdown_embedding_test;
mod neo4j_smoke;
mod neo4j_write_smoke;
mod pdf_indexer_test;
mod rerank_snippet_test;
mod rust_indexer_test;
mod search_test;
mod stable_ids_test;
mod storage_bench;
mod storage_test;
mod traversal_test;
mod vis_assembly_test;
