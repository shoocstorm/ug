//! `ug app` — the Tauri desktop shell around the visualization.

use ultragraph::{C_BOLD, C_CYAN, C_RESET, C_YELLOW};

use crate::serve;

use super::args::{flag_value, flag_value_or, has_flag};

/// `ug app` — launches the native desktop shell (Tauri) for the vis
/// layer. The webview just points at a `ug serve` URL, so this starts a
/// server first (in a background thread, in-process — no extra child
/// for it) and waits for it to answer before handing its URL to the
/// `ug-app` binary (built alongside `ug` — see native/src/bin/ug_app.rs).
/// All `ug serve` flags (`-i`, `--project`, `-p`, `--host`, etc.) pass
/// through untouched.
pub(crate) fn run_app(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        print_app_help();
        return;
    }

    let port: u16 = flag_value(args, &["-p", "--port"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let host = flag_value_or(args, &["--host"], "127.0.0.1");

    // `current_exe()` can return the invoked path rather than the resolved
    // one when `ug` is reached through a symlink (e.g. the installer's
    // `~/.local/bin/ug` -> `~/.local/share/ultragraph/.ug/ug`), which would
    // make us look for `ug-app` next to the symlink instead of next to the
    // real binary. Canonicalize first so we always look in the right dir.
    let app_path = std::env::current_exe().ok().and_then(|exe| {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        exe.parent().map(|d| {
            d.join(if cfg!(windows) { "ug-app.exe" } else { "ug-app" })
        })
    });
    let app_path = match app_path {
        Some(p) if p.exists() => p,
        _ => {
            eprintln!("Couldn't find the `ug-app` binary next to `ug` — the desktop shell wasn't bundled with this build.");
            eprintln!("Falling back to the browser instead: {C_CYAN}ug serve{C_RESET}, then open http://{host}:{port}");
            std::process::exit(1);
        }
    };

    let serve_args = args.to_vec();
    std::thread::spawn(move || {
        serve::run_serve(&serve_args);
    });

    let addr = format!("{host}:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if std::net::TcpStream::connect(&addr).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("Timed out waiting for `ug serve` to come up on {addr} — starting the app window anyway.");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let url = format!("http://{host}:{port}");
    println!("{C_CYAN}▸{C_RESET} Launching desktop app against {C_BOLD}{url}{C_RESET}");

    let status = std::process::Command::new(&app_path)
        .env("UG_APP_URL", &url)
        .status();

    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("Failed to launch {}: {}", app_path.display(), e);
            std::process::exit(1);
        }
    }
}

fn print_app_help() {
    println!("  {C_CYAN}ug app{C_RESET}  {C_YELLOW}— open the native desktop shell for the vis layer{C_RESET}");
    println!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    println!();
    println!("{C_BOLD}Usage:{C_RESET}  ug app [serve options]");
    println!();
    println!("  Starts {C_CYAN}ug serve{C_RESET} (in-process, same as running it directly) and opens");
    println!("  a native window (Tauri) pointed at it — an alternative to opening");
    println!("  http://localhost:8080 in a browser tab. Accepts every {C_CYAN}ug serve{C_RESET}");
    println!("  flag (-i, --project, -p/--port, --host, --watch, --no-db, ...); see");
    println!("  {C_CYAN}ug serve -h{C_RESET} for the full list.");
    println!();
    println!("{C_BOLD}Examples:{C_RESET}");
    println!("  {C_CYAN}ug app{C_RESET}                       {C_YELLOW}# all projects under ~/.ug{C_RESET}");
    println!("  {C_CYAN}ug app{C_RESET} --project myrepo -p 9000");
}
