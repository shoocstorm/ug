//! `ug` — the UltraGraph command-line binary.
//!
//! This file is deliberately only the entry point. The CLI itself lives in
//! [`cli`], one module per command group; the HTTP server in [`serve`], the
//! MCP server in [`mcp`], and the pieces both share (`chat`, `config`,
//! `project`, `tour`) beside them.

mod assets;
mod chat;
mod cli;
mod config;
mod mcp;
mod project;
mod serve;
mod tour;

fn main() {
    cli::run();
}
