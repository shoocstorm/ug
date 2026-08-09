//! `ug chat` — one-shot and REPL RAG chat over an indexed project,
//! including the tool-calling bridge that lets the model reach the same
//! agent tools the CLI exposes.

use std::path::PathBuf;

use ultragraph::agent_tools;
use ultragraph::storage::{
    open_store, DEFAULT_CONTEXT_CHARS, Direction, Embedder, KnowledgeStore, RankStrategy,
};
use ultragraph::{C_BLUE, C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW};

use crate::{chat, config};

use super::agent::{agent_repo_root, load_agent_graph};
use super::args::{first_positional, flag_value, has_flag, multi_flag};
use super::embed::{embedder_from_chat_args, tokio_runtime};
use super::io::{write_file, write_or_print};
use super::store::single_store_spec_from_args;

pub(crate) fn chat_client_from_args(args: &[String]) -> chat::ChatClient {
    let cfg = chat_config_from_args(args);
    eprintln!(
        "{C_CYAN}▸{C_RESET} Chat: model={C_BOLD}{}{C_RESET}, base_url={}, temperature={}, max_tokens={}",
        cfg.model, cfg.base_url, cfg.temperature, cfg.max_tokens
    );
    chat::ChatClient::new(cfg).unwrap_or_else(|e| {
        eprintln!("failed to build chat client: {}", e);
        std::process::exit(1);
    })
}

fn chat_config_from_args(args: &[String]) -> chat::ChatConfig {
    let base_url_flag = flag_value(args, &["--chat-base-url"])
        .or_else(|| flag_value(args, &["--base-url"]));
    let (base_url, _) = config::resolve_pref_cfg(base_url_flag, "chat.base_url");
    let api_key_flag = flag_value(args, &["--chat-api-key"])
        .or_else(|| flag_value(args, &["--api-key"]));
    let (api_key, _) = config::resolve_pref_cfg(api_key_flag, "chat.api_key");
    let (model, _) = config::resolve_pref_cfg(flag_value(args, &["--chat-model"]), "chat.model");
    let (temp_raw, _) =
        config::resolve_pref_cfg(flag_value(args, &["--temperature"]), "chat.temperature");
    let temperature = temp_raw.and_then(|s| s.parse().ok());
    let (max_tok_raw, _) =
        config::resolve_pref_cfg(flag_value(args, &["--max-tokens"]), "chat.max_tokens");
    let max_tokens = max_tok_raw.and_then(|s| s.parse().ok());
    let (timeout_raw, _) =
        config::resolve_pref_cfg(flag_value(args, &["--chat-timeout"]), "chat.timeout_secs");
    let timeout = timeout_raw.and_then(|s| s.parse().ok());
    chat::ChatConfig::with_overrides(base_url, api_key, model, temperature, max_tokens, timeout)
}

pub(crate) fn run_chat(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_chat_help();
        return;
    }

    // Value-bearing flags so the first non-flag positional becomes the
    // (optional) one-shot prompt — anything else drops us into REPL mode.
    let value_flags = [
        "-n",
        "--name",
        "-k",
        "--limit",
        "--hops",
        "--strategy",
        "--direction",
        "-t",
        "--edge-type",
        "--max-chars",
        "--repo-root",
        "--base-url",
        "--api-key",
        "--model",
        "--embedding-dim",
        "--embedding-model",
        "--embedding-base-url",
        "--embedding-api-key",
        "--chat-base-url",
        "--chat-api-key",
        "--chat-model",
        "--temperature",
        "--max-tokens",
        "--chat-timeout",
        "--system",
        "--filter",
        "--db",
        "-o",
        "--output",
        "--dest",
        "--neo4j-uri",
        "--neo4j-user",
        "--neo4j-password",
        "--neo4j-database",
    ];

    let oneshot_query = first_positional(args, &value_flags);
    let json_output = has_flag(args, "--json");
    let show_context = has_flag(args, "--show-context") || has_flag(args, "-v");
    let no_snippets = has_flag(args, "--no-snippets");
    // Reasoning models spend most of the wall-clock deliberating; the
    // answer is grounded in retrieved context either way.
    let think = has_flag(args, "--think");
    // Tools are on by default: an answer that can check itself against the
    // graph beats one that can only paraphrase what retrieval happened to find.
    let no_tools = has_flag(args, "--no-tools");
    let max_tool_rounds: usize = flag_value(args, &["--max-tool-rounds"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .min(8);

    let k: usize = flag_value(args, &["-k", "--limit"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let hops: u32 = flag_value(args, &["--hops"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let max_chars: usize = flag_value(args, &["--max-chars"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTEXT_CHARS);
    let strategy = flag_value(args, &["--strategy"])
        .map(|s| RankStrategy::from_str_lossy(&s))
        .unwrap_or(RankStrategy::Ppr);
    let direction = flag_value(args, &["--direction"])
        .map(|s| Direction::from_str_lossy(&s))
        .unwrap_or(Direction::Both);
    let edge_types = multi_flag(args, &["-t", "--edge-type"]);
    let repo_root: PathBuf = flag_value(args, &["--repo-root"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let system_prompt = flag_value(args, &["--system"]);
    let where_clause = flag_value(args, &["--filter"]);
    let output_path = flag_value(args, &["-o", "--output"]);

    let embedder = embedder_from_chat_args(args);
    let chat_client = chat_client_from_args(args);
    let rt = tokio_runtime();

    rt.block_on(async {
        let dim = embedder.config().dim as u32;
        let spec = single_store_spec_from_args(args, dim);
        let store = open_store(&spec)
            .await
            .unwrap_or_else(|e| {
                eprintln!("failed to open {} store: {}", spec.name(), e);
                std::process::exit(1);
            });
        // Shared with the tool runner, which outlives any single turn.
        let store: std::sync::Arc<dyn KnowledgeStore> = std::sync::Arc::from(store);
        let embedder = std::sync::Arc::new(embedder);

        let edge_types_owned: Option<Vec<String>> = if edge_types.is_empty() {
            None
        } else {
            Some(edge_types)
        };

        // The same graph toolbox the UI and MCP clients get, so a terminal
        // answer is reached the same way as one in the browser. `--no-tools`
        // opts out; a project without a graph.json simply has none.
        let runner = if no_tools {
            None
        } else {
            Some(cli_tool_runner(args, store.clone(), embedder.clone()))
        };
        let toolbox = runner.as_ref().map(|run| chat::ToolBox {
            schemas: crate::mcp::tools::openai_tool_schemas(),
            run,
            max_rounds: max_tool_rounds,
            max_result_chars: 6_000,
        });

        let opts_factory = |q: &str| {
            let mut o = chat::ChatRagOptions::new();
            o.k = k;
            o.hops = hops;
            o.strategy = strategy;
            o.direction = direction;
            o.edge_types = edge_types_owned.as_deref();
            o.include_snippets = !no_snippets;
            o.max_context_chars = max_chars;
            o.where_clause = where_clause.as_deref();
            o.system_prompt = system_prompt.as_deref();
            o.fast = !think;
            let _ = q; // q reserved for future per-call overrides
            o
        };

        // Tokens stream to the terminal as they arrive unless the output
        // is structured (--json) or the user opts out (--no-stream).
        let no_stream = has_flag(args, "--no-stream");

        match oneshot_query {
            Some(q) => {
                if json_output || no_stream {
                    let outcome = match chat::run_chat_rag(
                        store.as_ref(),
                        &embedder,
                        &chat_client,
                        repo_root.as_path(),
                        &q,
                        &[],
                        opts_factory(&q),
                        toolbox.as_ref(),
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("chat failed: {}", e);
                            std::process::exit(1);
                        }
                    };

                    if json_output {
                        let body = chat_outcome_to_json(&q, &outcome);
                        let text = serde_json::to_string_pretty(&body).unwrap_or_default();
                        write_or_print(output_path.as_deref(), &text, "chat result");
                    } else {
                        print_chat_outcome(&q, &outcome, show_context);
                        if let Some(p) = output_path.as_deref() {
                            write_file(p, &outcome.answer);
                            println!("Wrote answer to {}", p);
                        }
                    }
                } else {
                    let outcome = match stream_chat_turn(
                        store.as_ref(),
                        &embedder,
                        &chat_client,
                        repo_root.as_path(),
                        &q,
                        &[],
                        opts_factory(&q),
                        toolbox.as_ref(),
                        show_context,
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("chat failed: {}", e);
                            std::process::exit(1);
                        }
                    };
                    if let Some(p) = output_path.as_deref() {
                        write_file(p, &outcome.answer);
                        println!("Wrote answer to {}", p);
                    }
                }
            }
            None => {
                if json_output {
                    eprintln!("Error: --json requires a one-shot prompt; cannot pair with REPL mode.");
                    std::process::exit(2);
                }
                run_chat_repl(
                    store.as_ref(),
                    &embedder,
                    &chat_client,
                    repo_root.as_path(),
                    opts_factory,
                    toolbox.as_ref(),
                    show_context,
                    no_stream,
                )
                .await;
            }
        }
    });
}

/// The graph toolbox for `ug chat`, over this project's own graph and
/// store — the same tools the MCP server and `ug serve` expose, so an
/// answer in the terminal is reached the same way as one in the browser.
///
/// Returns the pieces the caller must keep alive: the runner closure is
/// borrowed by the `ToolBox`, so both have to outlive the chat turn.
fn cli_tool_runner(
    args: &[String],
    store: std::sync::Arc<dyn KnowledgeStore>,
    embedder: std::sync::Arc<Embedder>,
) -> impl Fn(&str, serde_json::Value) -> futures::future::BoxFuture<'static, Result<String, String>>
{
    let (graph, raw, graph_path) = load_agent_graph(args);
    let repo_root = agent_repo_root(&graph, &graph_path);
    let graph = std::sync::Arc::new(graph);
    let raw = std::sync::Arc::new(raw);

    move |name: &str, args: serde_json::Value| {
        let name = name.to_string();
        let graph = graph.clone();
        let raw = raw.clone();
        let repo_root = repo_root.clone();
        let graph_path = graph_path.clone();
        let store = store.clone();
        let embedder = embedder.clone();
        Box::pin(async move {
            let mut args = args;
            crate::mcp::tools::normalize_args(&name, &mut args);
            match name.as_str() {
                // The two search tools need the vector store; everything
                // else answers from the loaded graph.
                "search" | "semantic_search" => {
                    chat::run_search_tool(&name, &args, &*store, Some(&embedder), repo_root.as_path())
                        .await
                }
                // Statistics come from the store's indexed properties, not the
                // graph — the one advertised tool `run_tool` cannot answer.
                "code_query" => crate::mcp::run_code_query_json(&*store, &args).await,
                _ => {
                    crate::mcp::tools::reject_if_store_backed(&name)?;
                    // Chat already holds this project's store open, so the
                    // source pre-fetch is one lookup rather than another open.
                    let indexed = agent_tools::IndexedSource::load(
                        &*store,
                        &agent_tools::source_node_ids(&name, &graph, &args),
                    )
                    .await;
                    let out = ultragraph::agent_tools::run_tool(
                        &name,
                        &graph,
                        &raw,
                        agent_tools::SourceCtx::new(&indexed, repo_root.as_path()),
                        graph_path.as_path(),
                        args,
                        Some(ultragraph::agent_tools::Render::Markdown),
                    )?;
                    Ok(match out {
                        ultragraph::agent_tools::ToolOutput::Text(t) => t,
                        ultragraph::agent_tools::ToolOutput::Json(v) => {
                            serde_json::to_string_pretty(&v).unwrap_or_default()
                        }
                    })
                }
            }
        }) as futures::future::BoxFuture<'static, Result<String, String>>
    }
}

fn chat_outcome_to_json(query: &str, outcome: &chat::ChatRagOutcome) -> serde_json::Value {
    let citations: Vec<serde_json::Value> = outcome
        .context
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            serde_json::json!({
                "index": i + 1,
                "id": it.id,
                "name": it.name,
                "node_type": it.node_type,
                "file": it.file,
                "start_line": it.start_line,
                "end_line": it.end_line,
                "description": it.description,
                "distance": it.distance,
                "hop": it.hop,
                "snippet": it.snippet,
            })
        })
        .collect();
    serde_json::json!({
        "query": query,
        "answer": outcome.answer,
        "citations": citations,
        "seed_id": outcome.context.seed_id,
        "retrieval_ms": outcome.retrieval_ms,
        "completion_ms": outcome.completion_ms,
        "usage": outcome.usage,
    })
}

fn print_context_items(items: &[ultragraph::storage::ContextItem]) {
    println!("{C_BOLD}{C_MAGENTA}Retrieved context ({} items):{C_RESET}", items.len());
    for (i, it) in items.iter().enumerate() {
        let line_label = if it.start_line > 0 {
            format!(":{}-{}", it.start_line, it.end_line)
        } else {
            String::new()
        };
        println!(
            "  {C_CYAN}[#{}]{C_RESET} {C_BOLD}{}{C_RESET} {C_YELLOW}({}){C_RESET} {} {}{}",
            i + 1,
            it.name,
            it.node_type,
            if it.file.is_empty() { "<unknown>" } else { it.file.as_str() },
            line_label,
            if it.hop > 0 {
                format!(" {}hop={}{}", C_BLUE, it.hop, C_RESET)
            } else {
                String::new()
            }
        );
    }
    println!();
}

fn print_chat_meta(outcome: &chat::ChatRagOutcome) {
    println!(
        "{C_CYAN}▸{C_RESET} retrieval={}ms · completion={}ms · {} citation(s){}{}",
        outcome.retrieval_ms,
        outcome.completion_ms,
        outcome.context.items.len(),
        match outcome.tool_calls {
            0 => String::new(),
            n => format!(" · {} tool call(s)", n),
        },
        match &outcome.usage {
            Some(u) => format!(
                " · tokens prompt={} completion={} total={}",
                u.prompt_tokens.unwrap_or(0),
                u.completion_tokens.unwrap_or(0),
                u.total_tokens.unwrap_or(0),
            ),
            None => String::new(),
        }
    );
}

fn print_chat_outcome(query: &str, outcome: &chat::ChatRagOutcome, show_context: bool) {
    println!();
    println!("{C_BOLD}{C_CYAN}❯ Query:{C_RESET} {}", query);
    println!();
    if show_context {
        print_context_items(&outcome.context.items);
    }
    println!("{C_BOLD}{C_GREEN}Answer:{C_RESET}");
    println!("{}", outcome.answer.trim_end());
    println!();
    print_chat_meta(outcome);
}

/// One RAG turn with live token streaming to the terminal: a transient
/// "retrieving" line while search runs, the context list (when enabled)
/// as soon as it's ready, provider reasoning dimmed, then answer tokens
/// as they arrive. Falls back to a single chunk automatically when the
/// provider doesn't stream (handled in `run_chat_rag_stream`).
async fn stream_chat_turn(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat_client: &chat::ChatClient,
    repo_root: &std::path::Path,
    query: &str,
    history: &[chat::ChatMessage],
    opts: chat::ChatRagOptions<'_>,
    toolbox: Option<&chat::ToolBox<'_>>,
    show_context: bool,
) -> Result<chat::ChatRagOutcome, Box<dyn std::error::Error + Send + Sync>> {
    use std::io::Write;

    println!();
    println!("{C_BOLD}{C_CYAN}❯ Query:{C_RESET} {}", query);
    println!();
    eprint!("{C_DIM}⣾ retrieving context…{C_RESET}");
    let _ = std::io::stderr().flush();

    let mut in_reasoning = false;
    let mut printed_answer_header = false;
    let outcome = chat::run_chat_rag_stream(
        store,
        embedder,
        chat_client,
        repo_root,
        query,
        history,
        opts,
        toolbox,
        |ctx| {
            // Clear the transient retrieval line before real output.
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
            if show_context {
                print_context_items(&ctx.items);
            }
        },
        |t: chat::ToolEvent| {
            // One line per tool call so a long agentic turn is legible.
            match &t.summary {
                None => eprintln!("{C_DIM}  ▸ {} {}{C_RESET}", t.name, t.args),
                Some(sum) => eprintln!("{C_DIM}  ✓ {} — {}{C_RESET}", t.name, sum),
            }
        },
        |d| {
            if let Some(r) = &d.reasoning {
                if !in_reasoning {
                    println!("{C_DIM}Reasoning:{C_RESET}");
                    print!("{C_DIM}");
                    in_reasoning = true;
                }
                print!("{}", r);
            }
            if let Some(c) = &d.content {
                if in_reasoning {
                    print!("{C_RESET}");
                    println!();
                    println!();
                    in_reasoning = false;
                }
                if !printed_answer_header {
                    println!("{C_BOLD}{C_GREEN}Answer:{C_RESET}");
                    printed_answer_header = true;
                }
                print!("{}", c);
            }
            let _ = std::io::stdout().flush();
        },
    )
    .await?;
    if in_reasoning {
        print!("{C_RESET}");
    }
    println!();
    println!();
    print_chat_meta(&outcome);
    Ok(outcome)
}

async fn run_chat_repl<'a, F>(
    store: &dyn KnowledgeStore,
    embedder: &Embedder,
    chat_client: &chat::ChatClient,
    repo_root: &std::path::Path,
    mut opts_factory: F,
    toolbox: Option<&chat::ToolBox<'_>>,
    show_context: bool,
    no_stream: bool,
) where
    F: for<'b> FnMut(&'b str) -> chat::ChatRagOptions<'a>,
{
    use std::io::{BufRead, Write};
    println!();
    println!("{C_BOLD}{C_MAGENTA}UltraGraph Chat — interactive RAG REPL{C_RESET}");
    println!("{C_CYAN}Type a question and press Enter. Commands: /quit /reset /context on|off /help{C_RESET}");
    println!();

    let mut history: Vec<chat::ChatMessage> = Vec::new();
    let mut show_ctx = show_context;
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();

    loop {
        print!("{C_BOLD}{C_GREEN}you ❯ {C_RESET}");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        match handle.read_line(&mut buf) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {}", e);
                break;
            }
        }
        let q = buf.trim();
        if q.is_empty() {
            continue;
        }
        match q {
            "/quit" | "/exit" | ":q" => break,
            "/reset" => {
                history.clear();
                println!("{C_YELLOW}(history cleared){C_RESET}");
                continue;
            }
            "/context on" => {
                show_ctx = true;
                println!("{C_YELLOW}(context display: on){C_RESET}");
                continue;
            }
            "/context off" => {
                show_ctx = false;
                println!("{C_YELLOW}(context display: off){C_RESET}");
                continue;
            }
            "/help" | "/?" => {
                println!("Commands: /quit, /reset, /context on|off, /help");
                continue;
            }
            _ => {}
        }

        let opts = opts_factory(q);
        let outcome = if no_stream {
            match chat::run_chat_rag(
                store, embedder, chat_client, repo_root, q, &history, opts, toolbox,
            )
            .await
            {
                Ok(o) => {
                    print_chat_outcome(q, &o, show_ctx);
                    o
                }
                Err(e) => {
                    eprintln!("{C_YELLOW}chat error:{C_RESET} {}", e);
                    continue;
                }
            }
        } else {
            match stream_chat_turn(
                store,
                embedder,
                chat_client,
                repo_root,
                q,
                &history,
                opts,
                toolbox,
                show_ctx,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("{C_YELLOW}chat error:{C_RESET} {}", e);
                    continue;
                }
            }
        };

        // Keep the last 6 exchanges to bound prompt growth.
        history.push(chat::ChatMessage::new("user", q.to_string()));
        history.push(chat::ChatMessage::new("assistant", outcome.answer.clone()));
        let max_history = 12;
        if history.len() > max_history {
            let drop_n = history.len() - max_history;
            history.drain(0..drop_n);
        }
    }
}

fn print_chat_help() {
    println!(
        "  {C_BOLD}{C_MAGENTA}💬 ug chat{C_RESET}  {C_YELLOW}— RAG-grounded chat against the knowledge graph{C_RESET}"
    );
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!(
        "  {C_CYAN}query{C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}hybrid retrieval (PPR){C_RESET} {C_BOLD}→{C_RESET} {C_CYAN}LLM completion{C_RESET}"
    );
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug chat [\"<one-shot prompt>\"] [options]");
    println!("  Omit the prompt to drop into an interactive REPL with conversational history.");
    println!();
    println!("{C_BOLD}Retrieval (matches `ug search`):{C_RESET}");
    println!("  {C_CYAN}-n, --name{C_RESET} <name>        Project name (default: cwd basename, else most recent under ~/.ug)");
    println!("  {C_CYAN}--db{C_RESET} <dir>               OverGraph directory (default: the -n project's, else the active one)");
    println!("  {C_CYAN}-k, --limit{C_RESET} <n>          Context items to retrieve (default: 8)");
    println!("  {C_CYAN}--direction{C_RESET} <dir>        outbound|inbound|both (default: both)");
    println!("  {C_CYAN}-t, --edge-type{C_RESET} <t>      Restrict expansion to edge type (repeatable)");
    println!("  {C_CYAN}--filter{C_RESET} <sql>           Optional SQL WHERE clause for the seed filter");
    println!("  {C_CYAN}--max-chars{C_RESET} <n>          Context char budget (default: 12000)");
    println!("  {C_CYAN}--no-snippets{C_RESET}            Don't read source snippets from disk");
    println!("  {C_CYAN}--think{C_RESET}                  Let a reasoning model deliberate (slower, rarely better)");
    println!("  {C_CYAN}--no-tools{C_RESET}               Answer from retrieved context only — no graph tool calls");
    println!("  {C_CYAN}--max-tool-rounds{C_RESET} <n>    Cap tool-calling rounds (default: 4, max 8)");
    println!("  {C_CYAN}--repo-root{C_RESET} <path>       Repo root for snippet resolution (default: cwd)");
    println!();
    println!("{C_BOLD}Chat model:{C_RESET}");
    println!("  {C_CYAN}--chat-model{C_RESET} <name>      Chat completion model (e.g. gpt-4o-mini)");
    println!("  {C_CYAN}--base-url{C_RESET} <url>         OpenAI-compatible base URL (shared by chat + embeddings)");
    println!("  {C_CYAN}--api-key{C_RESET} <key>          Bearer token (shared by chat + embeddings)");
    println!("  {C_CYAN}--chat-base-url{C_RESET} <url>    Override base URL for chat only");
    println!("  {C_CYAN}--chat-api-key{C_RESET} <key>     Override bearer token for chat only");
    println!("  {C_CYAN}--temperature{C_RESET} <f>        Sampling temperature (default: 0.2)");
    println!("  {C_CYAN}--max-tokens{C_RESET} <n>         Max completion tokens (default: 1024)");
    println!("  {C_CYAN}--chat-timeout{C_RESET} <secs>    HTTP timeout for chat calls (default: 180)");
    println!("  {C_CYAN}--system{C_RESET} <text>          Override the default RAG system prompt");
    println!("  {C_DIM}Persist any of these once with `ug config set chat.model …` — flags/env vars still win.{C_RESET}");
    println!();
    println!("{C_BOLD}Embedding (for retrieval; in-process by default):{C_RESET}");
    println!("  {C_CYAN}--embedding-model{C_RESET} <name>   Embedding model (falls back to --model)");
    println!("  {C_CYAN}--embedding-base-url{C_RESET} <url> Override base URL for embeddings only");
    println!("  {C_CYAN}--embedding-api-key{C_RESET} <key>  Override bearer token for embeddings only");
    println!("  {C_CYAN}--embedding-dim{C_RESET} <n>        Vector dim override (auto-probed otherwise)");
    println!();
    println!("{C_BOLD}Output:{C_RESET}");
    println!("  {C_CYAN}--json{C_RESET}                   Emit a single JSON document (answer + citations)");
    println!("  {C_CYAN}--show-context, -v{C_RESET}       Print the retrieved citations alongside the answer");
    println!("  {C_CYAN}-o, --output{C_RESET} <file>      Write the answer (or JSON) to a file");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_MAGENTA}ug chat{C_RESET} \"how does graph ingest work?\" \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --api-key 12345 \\");
    println!("    --chat-model Qwen3.6-35B-A3B-MLX-8bit \\");
    println!("    --embedding-model Qwen3-Embedding-4B-4bit-DWQ");
    println!();
    println!("  {C_MAGENTA}ug chat{C_RESET} --json -v \\");
    println!("    \"explain the PPR seed pool logic\" \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --chat-model my-model");
    println!();
    println!("  {C_MAGENTA}ug chat{C_RESET} \\");
    println!("    --base-url http://127.0.0.1:8000/v1 --chat-model my-model     {C_YELLOW}# interactive REPL{C_RESET}");
}
