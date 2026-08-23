//! `ug tour` — a narrated walkthrough of a codebase: planning the route,
//! reporting progress while it is generated, and rendering it for a
//! terminal or a file.

use std::path::PathBuf;

use ultragraph::storage::{DEFAULT_CONTEXT_CHARS, Direction, RankStrategy};
use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW};

use crate::tour;

use super::args::{first_positional, flag_value, has_flag, multi_flag};
use super::chat::chat_client_from_args;
use super::embed::{embedder_from_chat_args, tokio_runtime};
use super::io::{write_file, write_or_print};
use super::dest::{open_store_or_exit, single_store_spec_from_args};

pub(crate) fn run_tour(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_tour_help();
        return;
    }

    // Value-bearing flags so the first bare positional is the question.
    let value_flags = [
        "-n", "--name", "-k", "--limit", "--hops", "--max-stops", "--strategy", "--direction",
        "-t", "--edge-type", "--max-chars", "--max-per-file", "--repo-root", "--base-url",
        "--api-key", "--model",
        "--embedding-dim", "--embedding-model", "--embedding-base-url", "--embedding-api-key",
        "--chat-base-url", "--chat-api-key", "--chat-model", "--temperature", "--max-tokens",
        "--chat-timeout", "--filter", "--db", "-o", "--output", "--dest", "--neo4j-uri", "--neo4j-user",
        "--neo4j-password", "--neo4j-database",
    ];

    let query = match first_positional(args, &value_flags) {
        Some(q) => q,
        None => {
            eprintln!(
                "Usage: ug tour <question> [-k <n>] [--hops <n>] [--max-stops <n>] [--no-llm] [--json] [-o <file>]\n       (run `ug tour -h` for the full flag list)"
            );
            std::process::exit(1);
        }
    };

    let json_output = has_flag(args, "--json");
    let no_llm = has_flag(args, "--no-llm");
    let no_snippets = has_flag(args, "--no-snippets");
    // Print the guide's raw JSON plan alongside the itinerary — the CLI
    // twin of the web UI's "view plan JSON" panel.
    let show_plan = has_flag(args, "--show-plan");
    // Reasoning models spend most of a tour's wall-clock deliberating, so
    // the guide is asked not to unless the user wants it.
    let think = has_flag(args, "--think");
    let k: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(14);
    let hops: u32 = flag_value(args, &["--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let max_stops: usize = flag_value(args, &["--max-stops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(tour::DEFAULT_MAX_STOPS)
        .clamp(1, tour::MAX_STOPS_LIMIT);
    let max_chars: usize = flag_value(args, &["--max-chars"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTEXT_CHARS);
    // Candidates drawn from any one file, so a big file can't become the
    // whole tour. 0 disables the cap.
    let max_per_file: usize = flag_value(args, &["--max-per-file"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let strategy = flag_value(args, &["--strategy"])
        .map(|s| RankStrategy::from_str_lossy(&s))
        .unwrap_or(RankStrategy::Ppr);
    let direction = flag_value(args, &["--direction"])
        .map(|s| Direction::from_str_lossy(&s))
        .unwrap_or(Direction::Both);
    let edge_types = multi_flag(args, &["-t", "--edge-type"]);
    let where_clause = flag_value(args, &["--filter"]);
    let repo_root: PathBuf = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let output_path = flag_value(args, &["-o", "--output"]);

    let embedder = embedder_from_chat_args(args);
    let rt = tokio_runtime();

    rt.block_on(async {
        let dim = embedder.config().dim as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store_or_exit(&spec).await;

        let edge_types_owned: Option<Vec<String>> = if edge_types.is_empty() {
            None
        } else {
            Some(edge_types.clone())
        };
        let mut opts = tour::TourOptions::new();
        opts.k = k;
        opts.hops = hops;
        opts.max_stops = max_stops;
        opts.strategy = strategy;
        opts.direction = direction;
        opts.edge_types = edge_types_owned.as_deref();
        opts.include_snippets = !no_snippets;
        opts.max_context_chars = max_chars;
        opts.where_clause = where_clause.as_deref();
        opts.max_per_file = max_per_file;
        // The transcript is only worth carrying when something will print it.
        opts.include_debug = json_output || show_plan;
        opts.fast = !think;

        let result = if no_llm {
            eprintln!("{C_CYAN}▸{C_RESET} Planning tour (ranked, no LLM)…");
            tour::plan_tour_no_llm(store.as_ref(), &embedder, repo_root.as_path(), &query, opts.clone())
                .await
        } else {
            let chat_client = chat_client_from_args(args);
            eprintln!("{C_CYAN}▸{C_RESET} Planning tour for {C_BOLD}\u{201c}{}\u{201d}{C_RESET}…", query);
            // A local model can spend minutes on this; stream so the wait
            // has a visible pulse instead of a frozen terminal.
            opts.stream = true;
            let mut on_progress = tour_progress_printer();
            match tour::plan_tour_with_progress(
                store.as_ref(),
                &embedder,
                &chat_client,
                repo_root.as_path(),
                &query,
                opts.clone(),
                None,
                &mut on_progress,
            )
            .await
            {
                Ok(t) => Ok(t),
                Err(e) => {
                    eprintln!(
                        "{C_YELLOW}▸{C_RESET} tour guide (LLM) unavailable ({}); falling back to a ranked itinerary.",
                        e
                    );
                    tour::plan_tour_no_llm(
                        store.as_ref(),
                        &embedder,
                        repo_root.as_path(),
                        &query,
                        opts.clone(),
                    )
                    .await
                }
            }
        };

        let the_tour = match result {
            Ok(t) => t,
            Err(e) => {
                eprintln!("tour failed: {}", e);
                std::process::exit(1);
            }
        };

        if json_output {
            let text = serde_json::to_string_pretty(&the_tour).unwrap_or_default();
            write_or_print(output_path.as_deref(), &text, "tour");
        } else {
            print!("{}", render_tour(&the_tour, true));
            if show_plan {
                print!("{}", render_tour_plan(&the_tour, true));
            }
            if let Some(p) = output_path.as_deref() {
                write_file(p, &render_tour(&the_tour, false));
                println!("Wrote tour to {}", p);
            }
        }
    });
}

/// Word-wrap `text` to `width` columns, prefixing every line with `indent`.
fn wrap_indent(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    for (pi, para) in text.split('\n').enumerate() {
        if pi > 0 {
            out.push('\n');
        }
        let mut line_len = 0usize;
        let mut first_word = true;
        out.push_str(indent);
        for word in para.split_whitespace() {
            let wlen = word.chars().count();
            if !first_word && line_len + 1 + wlen > width {
                out.push('\n');
                out.push_str(indent);
                line_len = 0;
                first_word = true;
            }
            if !first_word {
                out.push(' ');
                line_len += 1;
            }
            out.push_str(word);
            line_len += wlen;
            first_word = false;
        }
    }
    out
}

/// A progress sink that keeps the terminal alive during a long plan.
/// Phase changes print a line; token counts rewrite one status line in
/// place (`\r`) so a five-minute completion doesn't scroll the screen.
fn tour_progress_printer() -> impl FnMut(tour::TourProgress) + Send {
    use std::io::Write;
    let mut writing = false;
    move |p| {
        let mut err = std::io::stderr();
        // Close off the in-place token line before printing anything else.
        let end_writing = |err: &mut std::io::Stderr, writing: &mut bool| {
            if *writing {
                let _ = writeln!(err);
                *writing = false;
            }
        };
        match p {
            tour::TourProgress::Retrieving => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · searching the graph…{C_RESET}");
            }
            tour::TourProgress::Retrieved { candidates, retrieval_ms } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(
                    err,
                    "{C_DIM}  · {} candidate(s) in {}ms{C_RESET}",
                    candidates, retrieval_ms
                );
            }
            tour::TourProgress::ReadingCode { items } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · read source for {} candidate(s){C_RESET}", items);
            }
            tour::TourProgress::Linking { edges } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · {} edge(s) between candidates{C_RESET}", edges);
            }
            tour::TourProgress::Planning { model, prompt_chars, candidates_shown, max_stops } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(
                    err,
                    "{C_DIM}  · asking {}{C_RESET}{C_DIM} for up to {} stop(s) from {} item(s) ({} char prompt){C_RESET}",
                    model, max_stops, candidates_shown, prompt_chars
                );
            }
            tour::TourProgress::Writing { chars, reasoning_chars, elapsed_ms } => {
                let secs = elapsed_ms as f64 / 1000.0;
                // ~4 chars/token is close enough for a progress read-out.
                let tokens = (chars + reasoning_chars) as f64 / 4.0;
                let rate = if secs > 0.0 { tokens / secs } else { 0.0 };
                let _ = write!(
                    err,
                    "\r{C_DIM}  · writing… ~{:.0} tokens · {:.0}/s · {:.0}s{C_RESET}\x1b[K",
                    tokens, rate, secs
                );
                let _ = err.flush();
                writing = true;
            }
            tour::TourProgress::Drafted { index, stop } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(
                    err,
                    "{C_GREEN}  ✓{C_RESET} {C_DIM}stop {} ready — {}{C_RESET}",
                    index + 1,
                    stop.title
                );
            }
            tour::TourProgress::Tool { name, args, summary, .. } => {
                end_writing(&mut err, &mut writing);
                match summary {
                    None => { let _ = writeln!(err, "{C_DIM}  ▸ {} {}{C_RESET}", name, args); }
                    Some(sum) => { let _ = writeln!(err, "{C_DIM}  ✓ {} — {}{C_RESET}", name, sum); }
                }
            }
            tour::TourProgress::Repairing { .. } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_YELLOW}  · reply unusable; asking again{C_RESET}");
            }
            tour::TourProgress::Assembling { stops } => {
                end_writing(&mut err, &mut writing);
                let _ = writeln!(err, "{C_DIM}  · binding {} stop(s) to graph nodes{C_RESET}", stops);
            }
        }
    }
}

/// Render a `Tour` as a terminal itinerary. `color` toggles ANSI so the
/// same routine produces a clean plain-text file with `-o`.
fn render_tour(t: &tour::Tour, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let bold = c(C_BOLD);
    let reset = c(C_RESET);
    let cyan = c(C_CYAN);
    let dim = c(C_DIM);
    let green = c(C_GREEN);
    let yellow = c(C_YELLOW);
    let magenta = c(C_MAGENTA);

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("{bold}{cyan}❯ {}{reset}\n", t.title));
    if t.fallback {
        out.push_str(&format!(
            "{dim}  (ranked itinerary — no tour-guide LLM configured; pass --chat-model to narrate){reset}\n"
        ));
    }
    if !t.intro.is_empty() {
        out.push('\n');
        out.push_str(&wrap_indent(&t.intro, 76, "  "));
        out.push('\n');
    }

    if t.stops.is_empty() {
        out.push('\n');
        out.push_str(&format!("{yellow}  No stops on this tour.{reset}\n"));
        return out;
    }

    let total = t.stops.len();
    for (i, s) in t.stops.iter().enumerate() {
        out.push('\n');
        let loc = if s.start_line > 0 {
            format!(
                "{}:{}{}",
                if s.file.is_empty() { "<unknown>" } else { s.file.as_str() },
                s.start_line,
                if s.end_line > s.start_line { format!("-{}", s.end_line) } else { String::new() }
            )
        } else if !s.file.is_empty() {
            s.file.clone()
        } else {
            String::new()
        };
        // The graph edge we followed to get here, when there is one — the
        // itinerary should read as a walk, not a list.
        if let Some(link) = s.edge_from_prev.as_ref() {
            out.push_str(&format!(
                "{dim}  │  {} {}{reset}\n",
                if link.reverse { "\u{2190}" } else { "\u{2192}" },
                link.edge_type
            ));
        }
        out.push_str(&format!(
            "{green}  ●{reset} {dim}Stop {}/{}{reset} · {bold}{}{reset} {dim}({}){reset}\n",
            i + 1,
            total,
            s.title,
            s.node_type
        ));
        if !loc.is_empty() {
            out.push_str(&format!("{dim}     {}{reset}\n", loc));
        }
        if !s.narration.is_empty() {
            out.push_str(&wrap_indent(&s.narration, 74, "     "));
            out.push('\n');
        }
        if let Some(snip) = s.snippet.as_ref() {
            let snip = snip.trim_end_matches('\n');
            if !snip.is_empty() {
                for line in snip.lines().take(6) {
                    out.push_str(&format!("{dim}     │ {}{reset}\n", line));
                }
                if snip.lines().count() > 6 {
                    out.push_str(&format!("{dim}     │ …{reset}\n"));
                }
            }
        }
    }

    if !t.outro.is_empty() {
        out.push('\n');
        out.push_str(&format!("{magenta}  ✦{reset} "));
        // Continue the outro after the marker, wrapped and re-indented.
        let wrapped = wrap_indent(&t.outro, 74, "     ");
        out.push_str(wrapped.trim_start());
        out.push('\n');
    }

    if !t.warnings.is_empty() {
        out.push('\n');
        for w in &t.warnings {
            out.push_str(&format!("{yellow}  !{reset} {dim}{}{reset}\n", w));
        }
    }

    out.push('\n');
    let mut meta = format!("retrieval={}ms", t.retrieval_ms);
    if t.completion_ms > 0 {
        meta.push_str(&format!(" · guide={}ms", t.completion_ms));
    }
    meta.push_str(&format!(" · {} stop(s)", total));
    if !t.candidates.is_empty() {
        meta.push_str(&format!(" of {} candidate(s)", t.candidates.len()));
    }
    if let Some(u) = &t.usage {
        if let Some(tk) = u.total_tokens {
            meta.push_str(&format!(" · tokens={}", tk));
        }
    }
    out.push_str(&format!("{cyan}▸{reset} {dim}{}{reset}\n", meta));
    out
}

/// Pretty-print the guide's raw plan (`--show-plan`): the JSON object the
/// model produced, plus any refs we couldn't bind to a node.
fn render_tour_plan(t: &tour::Tour, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let bold = c(C_BOLD);
    let reset = c(C_RESET);
    let cyan = c(C_CYAN);
    let dim = c(C_DIM);
    let yellow = c(C_YELLOW);

    let mut out = String::new();
    let Some(d) = t.debug.as_ref() else {
        out.push_str(&format!(
            "\n{dim}  (no plan transcript — this itinerary didn't go through the tour guide){reset}\n"
        ));
        return out;
    };

    out.push_str(&format!("\n{bold}{cyan}❯ Guide plan (JSON){reset}\n"));
    if d.repaired {
        out.push_str(&format!(
            "{yellow}  !{reset} {dim}the first reply was unusable; this is the repaired one{reset}\n"
        ));
    }
    let body = match d.plan.as_ref() {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| d.raw_response.clone()),
        None => d.raw_response.clone(),
    };
    for line in body.lines() {
        out.push_str(&format!("{dim}  │ {reset}{}\n", line));
    }
    if !d.dropped.is_empty() {
        out.push('\n');
        for dr in &d.dropped {
            out.push_str(&format!(
                "{yellow}  !{reset} {dim}ref {} skipped — {}{reset}\n",
                dr.raw, dr.reason
            ));
        }
    }
    out
}

fn print_tour_help() {
    println!("  {C_CYAN}ug tour{C_RESET}  {C_YELLOW}— guided, narrated walk through the graph{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug tour <question> [options]");
    println!();
    println!("  GraphRAG picks the nodes that matter for your question, an LLM");
    println!("  \u{201c}tour guide\u{201d} orders them into a narrative and narrates each stop,");
    println!("  and the result is an ordered itinerary bound to real graph nodes.");
    println!("  In the web UI ({C_CYAN}ug serve{C_RESET}) the same tour flies the camera stop to stop.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>       Candidate nodes to retrieve (default: 14)");
    println!("  {C_CYAN}--hops{C_RESET} <n>            Graph expansion hops (default: 2)");
    println!("  {C_CYAN}--max-stops{C_RESET} <n>       Max stops on the tour (default: {}, max {})", tour::DEFAULT_MAX_STOPS, tour::MAX_STOPS_LIMIT);
    println!("  {C_CYAN}--max-per-file{C_RESET} <n>    Candidates kept per file, 0 = no cap (default: 2)");
    println!("  {C_YELLOW}--no-llm{C_RESET}             Skip the guide; emit a ranked itinerary from retrieval only");
    println!("  {C_CYAN}--no-snippets{C_RESET}         Omit code snippets from stops");
    println!("  {C_CYAN}--think{C_RESET}               Let a reasoning model deliberate (slower, rarely better)");
    println!("  {C_CYAN}--show-plan{C_RESET}           Print the raw JSON plan the guide produced");
    println!("  {C_CYAN}--strategy{C_RESET} <s>        Rank strategy (ppr|semantic|…, default: ppr)");
    println!("  {C_CYAN}--direction{C_RESET} <d>       Edge direction (out|in|both, default: both)");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>   Restrict expansion to an edge type (repeatable)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>        WHERE clause over node columns");
    println!("  {C_CYAN}-n, --name{C_RESET} <project>  Project under ~/.ug (default: cwd basename)");
    println!("  {C_CYAN}--db{C_RESET} <dir>             OverGraph directory (default: the -n project's, else the active one)");
    println!("  {C_CYAN}--json{C_RESET}                Emit the tour as JSON (node ids, timings, usage)");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>   Write the itinerary/JSON to a file");
    println!();
    println!("  Chat/embedding endpoint flags match {C_CYAN}ug chat{C_RESET}: {C_CYAN}--chat-model{C_RESET}, {C_CYAN}--base-url{C_RESET},");
    println!("  {C_CYAN}--api-key{C_RESET}, {C_CYAN}--temperature{C_RESET}, {C_CYAN}--max-tokens{C_RESET}, … (or persist via {C_CYAN}ug config set{C_RESET}).");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug tour{C_RESET} \"how does authentication work?\"");
    println!("  {C_CYAN}ug tour{C_RESET} \"the request lifecycle\" --max-stops 6 --hops 3");
    println!("  {C_CYAN}ug tour{C_RESET} \"how are nodes embedded?\" --show-plan");
    println!("  {C_CYAN}ug tour{C_RESET} \"error handling\" --no-llm --json -o tour.json");
}
