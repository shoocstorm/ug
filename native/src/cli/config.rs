//! `ug config` — reading, setting and clearing the persisted preferences
//! in `~/.ug/config.json`.

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_MAGENTA, C_RESET, C_YELLOW};

use crate::config;

use super::args::has_flag;

/// `ug config` — view and persist settings in `$UG_HOME/config.json`.
/// Persisted values sit below CLI flags and env vars in precedence, so
/// nothing here can silently hijack an explicit invocation; the
/// resolver prints a notice whenever a flag/env var overrides a saved
/// value.
pub(crate) fn run_config(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_config_help();
        return;
    }
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => run_config_list(),
        "path" => println!("{}", config::config_path().display()),
        "get" => {
            let Some(name) = args.get(1) else {
                eprintln!("Usage: ug config get <key>");
                std::process::exit(1);
            };
            let key = config_key_or_exit(name);
            match config::get(key.name) {
                Some(v) => println!("{}", v),
                None => {
                    eprintln!("{} is not set (run `ug config set {} <value>`)", key.name, key.name);
                    std::process::exit(1);
                }
            }
        }
        "set" => {
            let (Some(name), Some(value)) = (args.get(1), args.get(2)) else {
                eprintln!("Usage: ug config set <key> <value>");
                std::process::exit(1);
            };
            let key = config_key_or_exit(name);
            let path = config::config_path();
            let mut cfg = config::read_config_file(&path).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });
            if let Err(e) = config::value_set(&mut cfg, key, value) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            if let Err(e) = config::write_config_file(&path, &cfg) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            println!(
                "{C_GREEN}✓{C_RESET} {C_BOLD}{}{C_RESET} = {} → {}",
                key.name,
                config::display_value(key, value),
                path.display()
            );
            // Env vars are no longer consulted for config keys — `ug config
            // set` is the way to persist. This block intentionally empty to
            // make that clear; remove when the dust settles.
        }
        "unset" | "rm" => {
            let Some(name) = args.get(1) else {
                eprintln!("Usage: ug config unset <key>");
                std::process::exit(1);
            };
            let key = config_key_or_exit(name);
            let path = config::config_path();
            let mut cfg = config::read_config_file(&path).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });
            if !config::value_unset(&mut cfg, key) {
                println!("{} was not set — nothing to do", key.name);
                return;
            }
            if let Err(e) = config::write_config_file(&path, &cfg) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            println!("{C_GREEN}✓{C_RESET} unset {C_BOLD}{}{C_RESET}", key.name);
        }
        other => {
            eprintln!("Unknown config subcommand: {}", other);
            print_config_help();
            std::process::exit(1);
        }
    }
}

fn config_key_or_exit(name: &str) -> &'static config::ConfigKey {
    config::find_key(name).unwrap_or_else(|| {
        eprintln!("Unknown config key: {}", name);
        eprintln!("Known keys:");
        for k in config::CONFIG_KEYS {
            eprintln!("  {}", k.name);
        }
        std::process::exit(1);
    })
}

fn run_config_list() {
    let path = config::config_path();
    println!("{C_BOLD}UltraGraph config{C_RESET}  {C_DIM}{}{C_RESET}", path.display());
    println!("{C_DIM}precedence: CLI flag > this file > built-in default{C_RESET}");
    println!();
    for key in config::CONFIG_KEYS {
        let saved = config::get(key.name);
        let value_label = match &saved {
            Some(v) => format!("{C_CYAN}{}{C_RESET}", config::display_value(key, v)),
            None => format!("{C_DIM}(not set){C_RESET}"),
        };
        let overrides = key.flag.to_string();
        println!("  {C_BOLD}{:<18}{C_RESET} {}", key.name, value_label);
        println!("  {C_DIM}{:<18} {} [{}]{C_RESET}", "", key.desc, overrides);
    }
    println!();
    println!("Run {C_CYAN}ug config set <key> <value>{C_RESET} to change, {C_CYAN}ug doctor{C_RESET} to see effective values.");
}

fn print_config_help() {
    println!("  {C_CYAN}ug config{C_RESET}  {C_YELLOW}— view and persist defaults (chat model, endpoints, …){C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug config [list|get|set|unset|path] [<key>] [<value>]");
    println!();
    println!("  Saved to {C_CYAN}$UG_HOME/config.json{C_RESET} (default ~/.ug/config.json) and used by every");
    println!("  command as the fallback below CLI flags:");
    println!();
    println!("    {C_BOLD}CLI flag  >  ug config  >  built-in default{C_RESET}");
    println!();
    println!("  A flag that overrides a saved value prints a one-line notice.");
    println!();
    println!("{C_BOLD}Subcommands:{C_RESET}");
    println!("  {C_CYAN}list{C_RESET}               Show every key and its saved value (default)");
    println!("  {C_CYAN}get{C_RESET} <key>          Print one saved value");
    println!("  {C_CYAN}set{C_RESET} <key> <value>  Persist a value");
    println!("  {C_CYAN}unset{C_RESET} <key>        Remove a saved value");
    println!("  {C_CYAN}path{C_RESET}               Print the config file path");
    println!();
    println!("{C_BOLD}Keys:{C_RESET}");
    for key in config::CONFIG_KEYS {
        println!("  {C_CYAN}{:<18}{C_RESET} {}", key.name, key.desc);
    }
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_MAGENTA}ug config set{C_RESET} chat.model Qwen3.6-35B-A3B-MLX-8bit");
    println!("  {C_MAGENTA}ug config set{C_RESET} chat.base_url http://127.0.0.1:8000/v1");
    println!("  {C_MAGENTA}ug config get{C_RESET} chat.model");
    println!("  {C_MAGENTA}ug config unset{C_RESET} chat.model");
}
