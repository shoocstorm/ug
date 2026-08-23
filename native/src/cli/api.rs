//! `ug api` — prints the catalogue of HTTP endpoints `ug serve` exposes,
//! so the server's surface is discoverable without reading the router.
//!
//! The table itself is [`crate::serve::endpoints`], beside the routes it
//! describes; this module only formats it.

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW};

use super::args::has_flag;
use crate::serve::endpoints::API_ENDPOINTS;


/// `ug api` — reference listing of every HTTP endpoint `ug serve`
/// exposes, for users/agents who want to hit the REST API directly
/// instead of (or alongside) the CLI. Every row is an HTTP route, so
/// all of them require `ug serve` to be running to reach at all; the
/// "CLI equivalent" column instead flags which ones have a plain CLI
/// subcommand that does the same thing without a server.
pub(crate) fn run_api(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_api_help();
        return;
    }

    if has_flag(args, "--json") {
        let sections: Vec<serde_json::Value> = API_ENDPOINTS
            .iter()
            .map(|(section, entries)| {
                serde_json::json!({
                    "section": section,
                    "endpoints": entries.iter().map(|e| serde_json::json!({
                        "method": e.method,
                        "path": e.path,
                        "description": e.desc,
                        "availability": e.availability,
                        "cli_equivalent": e.cli_equivalent,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "requires_serve": true, "sections": sections }))
                .unwrap_or_default()
        );
        return;
    }

    println!("{C_BOLD}ug serve — HTTP API reference{C_RESET}");
    println!(
        "Every endpoint below is only reachable while {C_CYAN}ug serve{C_RESET} is running (default http://localhost:8080)."
    );
    println!(
        "{C_DIM}\"CLI equivalent\" marks endpoints whose capability is also available as a plain CLI command, no server needed.{C_RESET}"
    );
    println!();

    for (section, entries) in API_ENDPOINTS {
        println!("{C_BOLD}{}{C_RESET}", section);
        for e in *entries {
            let method_color = if e.method == "GET" { C_CYAN } else { C_MAGENTA };
            println!(
                "  {}{:<5}{C_RESET} {C_BOLD}{:<24}{C_RESET} {}",
                method_color, e.method, e.path, e.desc
            );
            let cli_note = match e.cli_equivalent {
                Some(cmd) => format!("{C_GREEN}CLI equivalent: {}{C_RESET}", cmd),
                None => format!("{C_DIM}serve-only (no CLI equivalent){C_RESET}"),
            };
            println!("        {C_YELLOW}{}{C_RESET}  ·  {}", e.availability, cli_note);
        }
        println!();
    }

    println!("Run {C_CYAN}ug api --json{C_RESET} for machine-readable output.");
}

fn print_api_help() {
    println!("  {C_CYAN}ug api{C_RESET}  {C_YELLOW}— list every HTTP endpoint `ug serve` exposes{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug api [--json]");
    println!();
    println!("  Prints a reference table of every route registered by {C_CYAN}ug serve{C_RESET}'s");
    println!("  HTTP server: method, path, what it does, when it 503s/is empty, and");
    println!("  whether the same capability also exists as a plain CLI subcommand");
    println!("  that works without a server running.");
    println!();
    println!("{C_BOLD}Options:{C_RESET}");
    println!("  {C_CYAN}--json{C_RESET}  Emit the same listing as machine-readable JSON");
}
