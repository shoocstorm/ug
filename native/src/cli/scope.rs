//! Saying which project a command is working against.
//!
//! Every project-scoped command resolves its target through a fallback chain
//! (`-n/--name` → the active project → the cwd's basename → the most recently
//! updated project), and none of them used to say which link fired. That is
//! the failure mode this exists to close: run from the wrong directory, `ug
//! analyze` answers confidently about a different repo, and a git hook logs a
//! refresh without naming what it refreshed.
//!
//! One line per distinct project a command touches, on **stderr** — the
//! machine-readable half of every command (`--json`, `-o`) goes to stdout, so
//! a banner there would have broken `| jq` exactly the way the startup logo
//! once did. `--no-banner` (or `UG_NO_BANNER=1`) turns it off.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_RESET, C_YELLOW};

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
    if seen.contains(&key) {
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

/// Warn that the index this command is about to answer from is behind the
/// tree, naming the files that drifted.
///
/// The CLI half of the MCP server's `staleness_note` (`src/mcp/mod.rs`), and
/// the same argument: `get_code` reads the live working tree and flags drift,
/// but `find_usages`, `analyze`, `traverse` and `shortest_path` read the
/// indexed graph and return the identical shape whether it is current or forty
/// commits behind. That asymmetry is the one failure an agent cannot see from
/// the output — a stale blast radius looks exactly like a true one — and it
/// bites hardest right after the agent's own edits, which is precisely when it
/// asks. Git hooks close the gap at commit boundaries; this closes it in
/// between by saying so.
///
/// stderr, deduplicated per project and suppressed by `--no-banner` for the
/// same reasons as [`announce`] — this rides alongside the banner and must not
/// reach the stdout that `--json` and `-o` promise to keep parseable. Keyed
/// separately so a command that announces a scope still gets its warning.
pub(crate) fn announce_staleness(data_dir: &Path) {
    // Both reads are cheap and skipped entirely when the banner is off, so a
    // `--no-banner` run pays nothing for a line it would not print. Doing this
    // before the stat walk matters: `staleness` is one stat per indexed file.
    if SILENT.load(Ordering::Relaxed) || std::env::var_os("UG_NO_BANNER").is_some() {
        return;
    }
    let Some(meta) = project::read_meta(data_dir) else {
        return;
    };
    let Some(stale) = project::staleness(data_dir, &meta) else {
        return;
    };
    // `is_stale` is already false for a vanished repo: an index frozen against
    // a moved checkout is not drift the caller can fix with `ug update`.
    if !stale.is_stale() {
        return;
    }

    let mut counts = Vec::new();
    if stale.changed > 0 {
        counts.push(format!("{} changed", stale.changed));
    }
    if stale.missing > 0 {
        counts.push(format!("{} deleted", stale.missing));
    }
    emit(
        format!("stale:{}", data_dir.display()),
        format!(
            "{C_YELLOW}⚠{C_RESET} index is behind the tree {C_DIM}·{C_RESET} {} of {} indexed \
             files {C_DIM}·{C_RESET} {}\n  {C_DIM}Structural answers describe the last index. \
             Refresh: {C_RESET}{C_CYAN}ug update <file>...{C_RESET}{C_DIM} (fast) or \
             {C_RESET}{C_CYAN}ug gen -n {}{C_RESET}{C_DIM}.{C_RESET}",
            counts.join(", "),
            stale.files,
            stale.changed_summary(),
            meta.name,
        ),
    );
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
