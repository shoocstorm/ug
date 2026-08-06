//! `ug upgrade` — self-update from the GitHub release for this platform.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

use super::args::{first_positional, has_flag};
use super::embed::tokio_runtime;

/// GitHub repo the prebuilt release archives are published to. Must match
/// `REPO` in install.sh — `ug upgrade` is that script's self-update twin.
const UPGRADE_REPO: &str = "shoocstorm/ug";

/// Leading numeric triple of a `v1.2.3`-style tag; non-digit suffixes
/// (`-rc1`) and missing parts read as 0, so `v0.2` == `0.2.0`.
fn version_triple(v: &str) -> (u64, u64, u64) {
    let mut nums = v.trim().trim_start_matches('v').splitn(3, '.').map(|part| {
        part.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
    )
}

/// `ug upgrade` — self-update the standalone prebuilt install from the
/// latest GitHub release (or a pinned `vX.Y.Z`). Mirrors install.sh: it
/// looks up the release via the GitHub API, downloads the matching
/// `ultragraph-<os-arch>.tar.gz` asset, unpacks it into
/// `$UG_INSTALL_ROOT/.ug` (default `~/.local/share/ultragraph/.ug`), and
/// refreshes the `$UG_BIN_DIR/ug` symlink. The new tree is staged next to
/// the live one and swapped in with two renames, so a failed download or
/// extraction never leaves a half-written install — and replacing the
/// directory the running binary lives in is safe on Unix (the process
/// keeps its inode). From-source checkouts are refused unless `--force`,
/// which (re)installs the release to the standard location anyway.
pub(crate) fn run_upgrade(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("Usage: {C_BOLD}ug upgrade{C_RESET} [<version>] [--check] [-f, --force]");
        println!("  Check GitHub for a newer release and self-update the standalone install.");
        println!();
        println!("  {C_CYAN}<version>{C_RESET}    Pin a specific release tag (e.g. v0.2.0) instead of latest");
        println!("  {C_CYAN}--check{C_RESET}      Only report whether an update is available; install nothing");
        println!("  {C_CYAN}-f, --force{C_RESET}  Reinstall even when already up to date, and allow installing");
        println!("               the prebuilt release from a from-source checkout");
        return;
    }

    let check_only = has_flag(args, "--check");
    let force = has_flag(args, "-f") || has_flag(args, "--force");
    let pinned = first_positional(args, &[]);

    fn die(msg: &str) -> ! {
        eprintln!("{C_YELLOW}error:{C_RESET} {msg}");
        std::process::exit(1);
    }

    // Same OS/arch → asset mapping as install.sh. Windows ships a zip we
    // don't self-extract, so it gets the manual-download pointer too.
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("linux", "x86_64") => "linux-x64",
        (os, arch) => {
            eprintln!("`ug upgrade` has no self-installable archive for {os}/{arch}.");
            eprintln!(
                "Download a release manually: {C_CYAN}https://github.com/{UPGRADE_REPO}/releases/latest{C_RESET}"
            );
            std::process::exit(1);
        }
    };
    let archive = format!("ultragraph-{asset}.tar.gz");

    let current = env!("CARGO_PKG_VERSION");
    let release_url = match &pinned {
        Some(v) => {
            let tag = if v.starts_with('v') { v.clone() } else { format!("v{v}") };
            format!("https://api.github.com/repos/{UPGRADE_REPO}/releases/tags/{tag}")
        }
        None => format!("https://api.github.com/repos/{UPGRADE_REPO}/releases/latest"),
    };

    println!(
        "{C_CYAN}▸{C_RESET} Current version {C_BOLD}v{current}{C_RESET} — checking {}...",
        pinned.as_deref().unwrap_or("latest release")
    );

    let rt = tokio_runtime();
    let client = reqwest::Client::builder()
        .user_agent(concat!("ug/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|e| die(&format!("failed to build HTTP client: {e}")));

    let release: serde_json::Value = rt
        .block_on(async {
            client
                .get(&release_url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
        })
        .unwrap_or_else(|e: reqwest::Error| {
            die(&format!("release lookup failed ({release_url}): {e}"))
        });

    let tag = release["tag_name"].as_str().unwrap_or_default().to_string();
    if tag.is_empty() {
        die("release has no tag_name — unexpected GitHub API response");
    }
    let newer = version_triple(&tag) > version_triple(current);

    if check_only {
        if newer {
            println!(
                "{C_GREEN}▸{C_RESET} Update available: {C_BOLD}v{current}{C_RESET} → {C_BOLD}{tag}{C_RESET}"
            );
            println!("Run {C_CYAN}ug upgrade{C_RESET} to install it.");
        } else {
            println!("{C_GREEN}✓{C_RESET} Already up to date (v{current} is the latest release).");
        }
        return;
    }
    if !newer && pinned.is_none() && !force {
        println!("{C_GREEN}✓{C_RESET} Already up to date (v{current} is the latest release).");
        println!("{C_DIM}Pass --force to reinstall anyway.{C_RESET}");
        return;
    }

    let home = dirs::home_dir()
        .unwrap_or_else(|| die("cannot determine your home directory"));
    let install_root = std::env::var("UG_INSTALL_ROOT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("share").join("ultragraph"));
    let dot_ug = install_root.join(".ug");

    // Refuse to "upgrade" a from-source checkout: replacing
    // ~/.local/share/ultragraph wouldn't touch the binary being run, which
    // would just look like the upgrade silently didn't take.
    let exe = std::env::current_exe()
        .ok()
        .map(|e| fs::canonicalize(&e).unwrap_or(e));
    let canon_dot_ug = fs::canonicalize(&dot_ug).unwrap_or_else(|_| dot_ug.clone());
    let is_prebuilt = exe.as_ref().is_some_and(|e| e.starts_with(&canon_dot_ug));
    if !is_prebuilt && !force {
        eprintln!(
            "{C_YELLOW}This `ug` is not the prebuilt install{C_RESET} (running from {}).",
            exe.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_else(|| "<unknown>".into())
        );
        eprintln!(
            "`ug upgrade` manages the standalone install at {} — for a source checkout, `git pull` and rebuild instead.",
            dot_ug.display()
        );
        eprintln!(
            "Re-run with {C_CYAN}--force{C_RESET} to install {tag} to the standard location anyway."
        );
        std::process::exit(1);
    }

    let download_url = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(archive.as_str()))
        .and_then(|a| a["browser_download_url"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            die(&format!("no {archive} asset found on release {tag} — has it finished building?"))
        });

    println!("{C_CYAN}▸{C_RESET} Downloading {C_BOLD}{tag}{C_RESET} ({archive})...");
    let bytes = rt
        .block_on(async {
            use futures::StreamExt;
            use std::io::{IsTerminal, Write};
            let resp = client.get(&download_url).send().await?.error_for_status()?;
            let total = resp.content_length();
            let mut buf: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
            let mut stream = resp.bytes_stream();
            // Redraw only on whole-percent changes, and only on a real
            // terminal — piped output would otherwise collect every `\r`
            // frame as its own line.
            let tty = std::io::stdout().is_terminal();
            let mut last_pct: u64 = u64::MAX;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                buf.extend_from_slice(&chunk);
                if let Some(t) = total.filter(|&t| t > 0) {
                    let pct = buf.len() as u64 * 100 / t;
                    if tty && pct != last_pct {
                        last_pct = pct;
                        print!(
                            "\r  {:.1} / {:.1} MB ({pct}%)",
                            buf.len() as f64 / 1e6,
                            t as f64 / 1e6
                        );
                        let _ = std::io::stdout().flush();
                    }
                }
            }
            if tty && last_pct != u64::MAX {
                println!();
            } else {
                println!("  {:.1} MB downloaded", buf.len() as f64 / 1e6);
            }
            Ok::<_, reqwest::Error>(buf)
        })
        .unwrap_or_else(|e| die(&format!("download failed: {e}")));

    let pid = std::process::id();
    let tmp_archive = std::env::temp_dir().join(format!("ug-upgrade-{pid}.tar.gz"));
    fs::write(&tmp_archive, &bytes)
        .unwrap_or_else(|e| die(&format!("failed to write {}: {e}", tmp_archive.display())));
    drop(bytes);

    // Stage → swap: extract beside the live tree, then two renames. The
    // stage/backup dirs are pid-suffixed so a concurrent or crashed
    // upgrade can't collide with this one.
    let stage = install_root.join(format!(".ug.new-{pid}"));
    let backup = install_root.join(format!(".ug.old-{pid}"));
    let cleanup = |paths: &[&Path]| {
        for p in paths {
            if p.exists() {
                let _ = fs::remove_dir_all(p);
                let _ = fs::remove_file(p);
            }
        }
    };

    println!("{C_CYAN}▸{C_RESET} Installing to {}...", dot_ug.display());
    let _ = fs::remove_dir_all(&stage);
    if let Err(e) = fs::create_dir_all(&stage) {
        cleanup(&[&tmp_archive]);
        die(&format!("failed to create {}: {e}", stage.display()));
    }
    let tar_ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tmp_archive)
        .arg("-C")
        .arg(&stage)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    cleanup(&[&tmp_archive]);
    if !tar_ok || !stage.join("ug").exists() {
        cleanup(&[&stage]);
        die("failed to extract the release archive (is `tar` on your PATH?)");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for bin in ["ug", "ug-app"] {
            let p = stage.join(bin);
            if p.exists() {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o755));
            }
        }
    }

    if dot_ug.exists() {
        if let Err(e) = fs::rename(&dot_ug, &backup) {
            cleanup(&[&stage]);
            die(&format!("failed to move the old install aside: {e}"));
        }
    }
    if let Err(e) = fs::rename(&stage, &dot_ug) {
        // Put the old tree back so the existing install keeps working.
        if backup.exists() {
            let _ = fs::rename(&backup, &dot_ug);
        }
        cleanup(&[&stage]);
        die(&format!("failed to activate the new install: {e}"));
    }
    cleanup(&[&backup]);

    // Refresh the launcher symlink (`ln -sf` in install.sh). A regular
    // file at that path is the user's own — warn, never clobber it.
    #[cfg(unix)]
    {
        let bin_dir = std::env::var("UG_BIN_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("bin"));
        let link = bin_dir.join("ug");
        let link_is_file = link
            .symlink_metadata()
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        if link_is_file {
            eprintln!(
                "{C_YELLOW}⚠{C_RESET} {} exists and is a regular file — leaving it alone. The new binary is at {}",
                link.display(),
                dot_ug.join("ug").display()
            );
        } else {
            let _ = fs::create_dir_all(&bin_dir);
            if link.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&link);
            }
            if let Err(e) = std::os::unix::fs::symlink(dot_ug.join("ug"), &link) {
                eprintln!(
                    "{C_YELLOW}⚠{C_RESET} could not refresh symlink {}: {e}",
                    link.display()
                );
            }
        }
    }

    let confirmed = std::process::Command::new(dot_ug.join("ug"))
        .arg("-v")
        .env("UG_QUIET_LOGO", "1")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    println!();
    println!("{C_GREEN}✓{C_RESET} {C_BOLD}Upgraded to {tag}{C_RESET}");
    if let Some(v) = confirmed {
        println!("  {C_DIM}{v}{C_RESET}");
    }
    println!("  {C_DIM}(restart any running `ug serve` / MCP server to pick it up){C_RESET}");
}
