//! Saying which project a command is working against.
//!
//! Every project-scoped command resolves its target through a fallback chain
//! (`-n/--name` → the active project → the cwd's basename → the most recently
//! updated project), and none of them used to say which link fired. That is
//! the failure mode this exists to close: run from the wrong directory, `ug
//! query` answers confidently about a different repo, and a git hook logs a
//! refresh without naming what it refreshed.
//!
//! One line per distinct project a command touches, on **stderr** — the
//! machine-readable half of every command (`--json`, `-o`) goes to stdout, so
//! a banner there would have broken `| jq` exactly the way the startup logo
//! once did. `--no-banner` (or `UG_NO_BANNER=1`) turns it off.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET};

use crate::project;

use super::args::flag_value;

/// Scopes already announced this process, keyed by the path they resolved to.
///
/// A single command routinely resolves the same project twice — the agent
/// tools load `graph.json` and then the `ugdb` beside it, `ug gen` names its
/// project and then hands the same path to the store layer. It is one scope
/// and it gets one line; a genuinely different second scope (a `-i` graph from
/// one project against a `--db` from another) still gets its own.
static ANNOUNCED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static SILENT: AtomicBool = AtomicBool::new(false);

/// Suppress the banner for this process — `--no-banner`, consumed in
/// [`super::run`] before any subcommand parses arguments.
pub(crate) fn silence() {
    SILENT.store(true, Ordering::Relaxed);
}

/// Shorten `$HOME/x` to `~/x`. The banner leads a command's output and the
/// home prefix is the least informative half of every path in it.
fn tilde(p: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn emit(key: String, line: String) {
    if SILENT.load(Ordering::Relaxed) || std::env::var_os("UG_NO_BANNER").is_some() {
        return;
    }
    // A poisoned lock here means another thread panicked mid-announce; the
    // list is a plain Vec of strings and cannot be left inconsistent, so
    // recovering beats taking the whole command down over a banner.
    let mut seen = ANNOUNCED.lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|k| *k == key) {
        return;
    }
    seen.push(key);
    eprintln!("{}", line);
}

/// The banner: which project, which repo it indexes, where its data lives,
/// and which resolution rule picked it.
pub(crate) fn announce(name: &str, data_dir: &Path, repo_root: &str, why: &str) {
    let repo = if repo_root.trim().is_empty() {
        "(no repo recorded)".to_string()
    } else {
        tilde(Path::new(repo_root))
    };
    emit(
        data_dir.display().to_string(),
        format!(
            "{C_CYAN}▸{C_RESET} project {C_BOLD}{}{C_RESET} {C_DIM}·{C_RESET} {} \
             {C_DIM}· data {} · [{}]{C_RESET}",
            name,
            repo,
            tilde(data_dir),
            why
        ),
    );
}

/// Announce the project that owns `path` — a `graph.json`, an `ugdb` dir, or
/// anything else sitting inside a project directory.
///
/// A path outside `~/.ug` (an explicit `-i` or `--db`) has no `project.json`
/// to name it, so the path itself is printed: it is still the whole answer to
/// "what is this command reading".
pub(crate) fn announce_data(kind: &str, path: &Path, why: &str) {
    let dir = path.parent().unwrap_or(path);
    match project::read_meta(dir) {
        Some(meta) => announce(&meta.name, dir, &meta.repo_root, why),
        None => emit(
            path.display().to_string(),
            format!(
                "{C_CYAN}▸{C_RESET} {} {C_BOLD}{}{C_RESET}{C_DIM} · [{}]{C_RESET}",
                kind,
                tilde(path),
                why
            ),
        ),
    }
}

/// Which link of the resolution chain a command will follow, as a label.
///
/// Mirrors [`project::resolve_active_project_name`] when `honors_active`, and
/// [`project::resolve_project_name`] when not — the two chains commands
/// actually use, so the label never claims a rule that did not fire.
pub(crate) fn why_project(args: &[String], honors_active: bool) -> &'static str {
    if flag_value(args, &["-n", "--name"]).is_some() {
        "-n/--name"
    } else if honors_active && project::get_active_project().is_some() {
        "active project"
    } else {
        "current directory"
    }
}
