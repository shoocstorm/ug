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

/// Hosts GitHub serves release assets from. A `browser_download_url`
/// pointing anywhere else means the release metadata is steering the
/// download off-platform, which is not something we follow.
const GITHUB_ASSET_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Is `url` an HTTPS URL on a GitHub asset host?
fn is_github_asset_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .split('@') // no userinfo smuggling a different host past us
        .next_back()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    GITHUB_ASSET_HOSTS.contains(&host.as_str())
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Pull the digest out of a `shasum`-style file (`<hex>  <filename>`),
/// tolerating a bare digest on its own. `None` if the first token isn't
/// 64 hex characters.
fn parse_sha256_file(body: &str) -> Option<String> {
    let token = body.split_whitespace().next()?;
    let ok = token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit());
    ok.then(|| token.to_ascii_lowercase())
}

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
        println!("  {C_CYAN}--allow-unverified{C_RESET}  Install even when the release publishes no .sha256");
        println!("               checksum. Skips the integrity check — avoid unless you have");
        println!("               verified the archive yourself.");
        return;
    }

    let check_only = has_flag(args, "--check");
    let force = has_flag(args, "-f") || has_flag(args, "--force");
    let allow_unverified = has_flag(args, "--allow-unverified");
    // `--allow-unverified` takes no value, so it needs no entry here: any
    // `-`-prefixed arg is already skipped when hunting for the positional.
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
            // The tag is pasted straight into a URL path, so keep it to the
            // shape a tag actually has — otherwise `../..`-style input walks
            // the request onto a different API endpoint entirely.
            if !tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
                || tag.contains("..")
            {
                die(&format!("invalid release tag: {tag}"));
            }
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

    // The download URL comes out of the API response, so it steers this
    // fetch wherever it likes. Pin it to GitHub's own asset hosts: a release
    // body that points somewhere else is not a release we install from.
    let asset_url = |name: &str| -> Option<String> {
        release["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .filter(|u| is_github_asset_url(u))
            .map(str::to_string)
    };

    let download_url = asset_url(&archive).unwrap_or_else(|| {
        die(&format!(
            "no {archive} asset with a github.com download URL found on release {tag} \
             — has it finished building?"
        ))
    });

    // Expected digest, published alongside the archive by the release
    // workflow. Absent means we cannot verify what we're about to install
    // and then execute, so the upgrade stops unless explicitly overridden.
    let checksum_asset = format!("{archive}.sha256");
    let expected_sha = match asset_url(&checksum_asset) {
        Some(url) => {
            let body: String = rt
                .block_on(async {
                    client.get(&url).send().await?.error_for_status()?.text().await
                })
                .unwrap_or_else(|e: reqwest::Error| {
                    die(&format!("failed to fetch {checksum_asset}: {e}"))
                });
            match parse_sha256_file(&body) {
                Some(d) => Some(d),
                None => die(&format!("{checksum_asset} is not a readable sha256 digest")),
            }
        }
        None if allow_unverified => {
            eprintln!(
                "{C_YELLOW}⚠{C_RESET} release {tag} publishes no {checksum_asset} — installing \
                 unverified because --allow-unverified was passed."
            );
            None
        }
        None => {
            eprintln!(
                "{C_YELLOW}error:{C_RESET} release {tag} publishes no {checksum_asset}, so the \
                 archive's integrity cannot be verified."
            );
            eprintln!(
                "  Install it anyway with {C_CYAN}ug upgrade {tag} --allow-unverified{C_RESET}, \
                 or download it manually:"
            );
            eprintln!("  {C_CYAN}https://github.com/{UPGRADE_REPO}/releases/tag/{tag}{C_RESET}");
            std::process::exit(1);
        }
    };

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

    // Verify before the bytes ever reach the filesystem. TLS says we talked
    // to GitHub; the digest says GitHub gave us the artifact the release
    // actually published — and this archive is about to be unpacked over the
    // install directory and executed.
    if let Some(expected) = &expected_sha {
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            die(&format!(
                "checksum mismatch for {archive}\n  expected {expected}\n  got      {actual}\n\
                 Refusing to install. This is either a corrupted download or a tampered archive."
            ));
        }
        println!("{C_GREEN}✓{C_RESET} Checksum verified ({C_DIM}sha256:{}{C_RESET})", &actual[..16]);
    }

    let pid = std::process::id();

    // Stage → swap: extract beside the live tree, then two renames. The
    // stage/backup dirs are pid-suffixed so a concurrent or crashed
    // upgrade can't collide with this one.
    //
    // The archive is staged inside `install_root` too, not the shared system
    // temp dir: a world-writable directory plus a predictable filename is a
    // window in which another local user can swap the verified archive for
    // their own between the write and the extract.
    let stage = install_root.join(format!(".ug.new-{pid}"));
    let backup = install_root.join(format!(".ug.old-{pid}"));
    let tmp_archive = install_root.join(format!(".ug-upgrade-{pid}.tar.gz"));
    if let Err(e) = fs::create_dir_all(&install_root) {
        die(&format!("failed to create {}: {e}", install_root.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&install_root, fs::Permissions::from_mode(0o700));
    }
    fs::write(&tmp_archive, &bytes)
        .unwrap_or_else(|e| die(&format!("failed to write {}: {e}", tmp_archive.display())));
    drop(bytes);
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
    // Extraction hardening. Both GNU tar and bsdtar refuse absolute and
    // `../` member paths unless `-P`/`--absolute-names` is given, which is
    // exactly why it never is here. `--no-same-owner` is the one that isn't
    // consistent between them when running as root, so it's explicit.
    let tar_ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tmp_archive)
        .arg("--no-same-owner")
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

#[cfg(test)]
mod tests {
    use super::{is_github_asset_url, parse_sha256_file, sha256_hex, version_triple};

    #[test]
    fn only_github_https_asset_urls_are_followed() {
        for ok in [
            "https://github.com/shoocstorm/ug/releases/download/v1.0.0/ultragraph-macos-arm64.tar.gz",
            "https://objects.githubusercontent.com/x/y",
            "https://release-assets.githubusercontent.com/a",
        ] {
            assert!(is_github_asset_url(ok), "{ok} should be accepted");
        }
        for bad in [
            // The release JSON steering the download off-platform.
            "https://evil.tld/ultragraph-macos-arm64.tar.gz",
            // Plaintext, so the bytes are attacker-writable in transit.
            "http://github.com/x",
            // Userinfo trying to make the real host look like github.com.
            "https://github.com@evil.tld/x",
            "https://notgithub.com/x",
            "ftp://github.com/x",
            "",
        ] {
            assert!(!is_github_asset_url(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn sha256_file_parsing_matches_shasum_output() {
        let digest = sha256_hex(b"hello");
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );

        // `shasum -a 256` / `sha256sum` format, and a bare digest.
        assert_eq!(
            parse_sha256_file(&format!("{digest}  ultragraph-macos-arm64.tar.gz\n")),
            Some(digest.clone())
        );
        assert_eq!(parse_sha256_file(&digest), Some(digest.clone()));
        assert_eq!(
            parse_sha256_file(&digest.to_uppercase()),
            Some(digest),
            "digest comparison is case-insensitive"
        );

        for bad in ["", "not-a-digest", "abc123  file.tar.gz", "<html>404</html>"] {
            assert_eq!(parse_sha256_file(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn version_triple_orders_releases() {
        assert!(version_triple("v0.2.0") > version_triple("v0.1.12"));
        assert_eq!(version_triple("v0.2"), (0, 2, 0));
        assert_eq!(version_triple("0.2.0-rc1"), (0, 2, 0));
    }
}
