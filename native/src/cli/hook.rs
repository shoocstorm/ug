//! `ug hook` — let git refresh the graph, so nobody has to remember to.
//!
//! `ug update <file>` already exists and is fast, but it is manual: an agent
//! (or a person) has to call it after every edit burst, and the one time it
//! is forgotten is the time `find_usages` answers from a stale graph while
//! `get_code` answers from the live tree. The two look identical and only one
//! is right — that asymmetry is what breaks trust in the structural tools.
//!
//! Git already knows exactly when the tree moved and which paths moved with
//! it, so the fix is to hang `ug update` off the hooks that fire on those
//! events: `post-commit`, `post-merge`, `post-checkout`, `post-rewrite`.
//!
//! The installed hook scripts hold no logic. Each is a two-line shell stub
//! that calls back into `ug hook run <name> -- "$@"`; the path computation,
//! the project lookup and the update itself live here, in Rust, where they
//! can be tested and fixed without rewriting anyone's `.git/hooks`.
//!
//! The stub is wrapped in `# >>> ug hook >>>` markers so it can share a hook
//! file with whatever was already there: install appends, uninstall strips
//! its own block and leaves the rest alone.

use std::path::{Path, PathBuf};
use std::process::Command;

use ultragraph::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RESET, C_YELLOW};

use crate::project;

use super::args::{first_positional, flag_value, has_flag};

/// Opening marker of the block `ug hook install` owns inside a hook file.
const BEGIN: &str = "# >>> ug hook (ultragraph) >>>";
/// Closing marker. Everything between the two is ours to rewrite or remove.
const END: &str = "# <<< ug hook (ultragraph) <<<";

/// A branch switch can touch thousands of files. The path list is a *report*
/// — `ug update` re-indexes the whole repo through the parse cache either way
/// — so there is nothing to gain from an argv that long.
const MAX_PATHS: usize = 100;

/// The four git hooks that mean "the working tree just moved".
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hook {
    /// `git commit`, `git commit --amend`, `git cherry-pick`, `git revert`.
    PostCommit,
    /// `git merge`, `git pull`.
    PostMerge,
    /// `git checkout <branch>`, `git switch`.
    PostCheckout,
    /// `git rebase`, and anything else that rewrites history.
    PostRewrite,
}

const HOOKS: [Hook; 4] = [
    Hook::PostCommit,
    Hook::PostMerge,
    Hook::PostCheckout,
    Hook::PostRewrite,
];

impl Hook {
    fn name(self) -> &'static str {
        match self {
            Hook::PostCommit => "post-commit",
            Hook::PostMerge => "post-merge",
            Hook::PostCheckout => "post-checkout",
            Hook::PostRewrite => "post-rewrite",
        }
    }

    fn why(self) -> &'static str {
        match self {
            Hook::PostCommit => "after a commit, amend, cherry-pick or revert",
            Hook::PostMerge => "after a merge or pull",
            Hook::PostCheckout => "after switching branches",
            Hook::PostRewrite => "after a rebase rewrites commits",
        }
    }

    fn from_name(s: &str) -> Option<Hook> {
        HOOKS.into_iter().find(|h| h.name() == s)
    }
}

// ── git plumbing ───────────────────────────────────────────────────────────

/// Run `git` in `dir` and return trimmed stdout, or `None` when git is
/// missing or the command failed.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Top of the working tree containing `dir`, canonicalised (Agents.md §9a:
/// git reports `/tmp/...` where the rest of `ug` stores `/private/tmp/...`,
/// and an uncanonicalised root makes every later path comparison lie).
fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let top = git(dir, &["rev-parse", "--show-toplevel"]).ok_or_else(|| {
        format!(
            "{} is not inside a git repository — git hooks are the mechanism here, \
             so there is nothing to install into.",
            dir.display()
        )
    })?;
    let p = PathBuf::from(top);
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Where this repo's hooks actually live.
///
/// `core.hooksPath` wins when set — husky and lefthook both set it, and
/// writing into `.git/hooks` while git reads `.husky/` installs a hook that
/// never runs.
fn hooks_dir(root: &Path) -> Result<PathBuf, String> {
    if let Some(cfg) = git(root, &["config", "--get", "core.hooksPath"]) {
        if !cfg.is_empty() {
            let p = PathBuf::from(&cfg);
            return Ok(if p.is_absolute() { p } else { root.join(p) });
        }
    }
    // `--git-path` resolves to the common dir for a linked worktree, which
    // is where git looks for hooks in that case too.
    let rel = git(root, &["rev-parse", "--git-path", "hooks"])
        .ok_or_else(|| "could not ask git where its hooks directory is".to_string())?;
    let p = PathBuf::from(&rel);
    Ok(if p.is_absolute() { p } else { root.join(p) })
}

// ── which paths did this event touch ───────────────────────────────────────

/// The `git` arguments whose output lists the paths a hook run should
/// refresh, or `None` when this invocation is a no-op.
///
/// Separated from running git so the interesting part — which two commits
/// each hook compares, and when it declines — is testable.
fn diff_args(hook: Hook, git_args: &[String], stdin: &str) -> Option<Vec<String>> {
    let names = |a: &str, b: &str| {
        Some(vec![
            "diff".to_string(),
            "--name-only".to_string(),
            a.to_string(),
            b.to_string(),
        ])
    };
    match hook {
        // The commit that was just made, against its parent. A merge commit
        // prints nothing here (diff-tree needs `-m` for those), which is
        // correct: post-merge already covered it.
        Hook::PostCommit => Some(
            ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        // git sets ORIG_HEAD to the pre-merge tip.
        Hook::PostMerge => names("ORIG_HEAD", "HEAD"),
        // args: <prev-head> <new-head> <branch-flag>. Flag 0 is a file
        // checkout, where both heads are the same commit and the diff would
        // be empty — the paths that changed are not derivable, so decline
        // rather than re-index on a guess.
        Hook::PostCheckout => {
            let prev = git_args.first()?;
            let new = git_args.get(1)?;
            if git_args.get(2).map(String::as_str) != Some("1") || prev == new {
                return None;
            }
            // A fresh clone reports an all-zero previous head; there is no
            // graph to keep fresh yet.
            if prev.chars().all(|c| c == '0') {
                return None;
            }
            names(prev, new)
        }
        // stdin carries `<old-sha> <new-sha>` per rewritten commit. The
        // first old sha is the oldest thing that moved; HEAD is where the
        // rewrite landed.
        Hook::PostRewrite => {
            let first_old = stdin.lines().find_map(|l| l.split_whitespace().next())?;
            names(first_old, "HEAD")
        }
    }
}

// ── the hook script ────────────────────────────────────────────────────────

/// The marked block installed into a hook file: a stub that hands control
/// straight back to `ug hook run`.
///
/// `UG_HOOK_DISABLE=1` skips it for one command (`UG_HOOK_DISABLE=1 git
/// rebase ...`), `-x` skips it when the binary has been moved or removed,
/// and the trailing `|| true` means a broken index can never fail a git
/// operation.
fn hook_block(hook: Hook, bin: &str, project: &str) -> String {
    format!(
        "{BEGIN}\n\
         # Refreshes the ug knowledge graph {why}.\n\
         # Managed by `ug hook install`; remove with `ug hook uninstall`.\n\
         # Set UG_HOOK_DISABLE=1 to skip it for one command.\n\
         if [ \"${{UG_HOOK_DISABLE:-0}}\" != \"1\" ] && [ -x {bin} ]; then\n\
         \tUG_QUIET_LOGO=1 {bin} hook run {name} --name {project} -- \"$@\" || true\n\
         fi\n\
         {END}\n",
        why = hook.why(),
        bin = sh_quote(bin),
        name = hook.name(),
        project = sh_quote(project),
    )
}

/// Single-quote a value for `sh`, so a path with a space or an apostrophe
/// cannot end the argument early or start a command.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Splice our block into a hook file's existing content.
///
/// Three cases, and the third is the one that matters: a repo that already
/// has a `post-commit` hook keeps it, with ours appended.
fn splice(existing: &str, block: &str) -> (String, Placement) {
    if existing.trim().is_empty() {
        return (format!("#!/bin/sh\n{}", block), Placement::Created);
    }
    if let Some((head, tail)) = split_block(existing) {
        return (format!("{head}{block}{tail}"), Placement::Updated);
    }
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block);
    (out, Placement::Merged)
}

/// Split a hook file around our block: `(everything before, everything
/// after)`. `None` when the block isn't there.
fn split_block(content: &str) -> Option<(String, String)> {
    let start = content.find(BEGIN)?;
    let end = content[start..].find(END).map(|i| start + i + END.len())?;
    // Swallow the newline that terminates the END marker, so removing the
    // block doesn't leave a blank line behind on every uninstall/reinstall.
    let end = if content[end..].starts_with('\n') { end + 1 } else { end };
    Some((content[..start].to_string(), content[end..].to_string()))
}

/// What `splice` did, for the report.
#[derive(PartialEq, Debug)]
enum Placement {
    Created,
    Updated,
    Merged,
}

/// Remove our block. Returns `None` when there was nothing of ours in the
/// file, and `Some(None)` when what is left is not worth keeping as a hook.
fn unsplice(content: &str) -> Option<Option<String>> {
    let (head, tail) = split_block(content)?;
    // Undo the blank line `splice` inserts to separate our block from a hook
    // that was already there, so a hook file survives install → uninstall
    // byte-identical however many times it goes round.
    let head = match head.strip_suffix("\n\n") {
        Some(trimmed) => format!("{trimmed}\n"),
        None => head,
    };
    let rest = format!("{head}{tail}");
    let meaningful = rest
        .lines()
        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'));
    Some(if meaningful { Some(rest) } else { None })
}

// ── install / uninstall / status ───────────────────────────────────────────

/// Absolute path to this binary, for baking into the hook scripts. Git may
/// run hooks from a GUI client whose `PATH` never saw a shell profile, so a
/// bare `ug` is not good enough.
fn ug_bin() -> String {
    std::env::var("UG_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::current_exe().ok().map(|p| {
                std::fs::canonicalize(&p)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .unwrap_or_else(|| "ug".to_string())
}

/// Resolve which project the hooks should refresh, and complain when that
/// project does not describe this repo — a hook pointed at the wrong project
/// silently refreshes a graph nobody is asking about.
fn resolve_project(args: &[String], root: &Path) -> Result<String, String> {
    let name = project::resolve_active_project_name(args, ".");
    let dir = project::project_dir(&name);
    let meta = project::read_meta(&dir).ok_or_else(|| {
        format!(
            "No indexed project named {} under {}. Run `ug gen -i {}` first — \
             a hook has nothing to refresh until the graph exists.",
            name,
            project::ug_home().display(),
            root.display()
        )
    })?;
    let recorded = PathBuf::from(&meta.repo_root);
    let recorded = std::fs::canonicalize(&recorded).unwrap_or(recorded);
    if recorded != root {
        eprintln!(
            "{C_YELLOW}⚠{C_RESET}  Project {C_BOLD}{}{C_RESET} indexes {}, not {}.",
            name,
            recorded.display(),
            root.display()
        );
        eprintln!(
            "   Pass {C_CYAN}-n <project>{C_RESET} to hook up the one that indexes this repo."
        );
    }
    Ok(name)
}

/// Make a hook file executable — git skips hooks that are not.
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Install the four hooks into the repo containing `dir`. Shared by
/// `ug hook install` and `ug connect --hooks`.
pub(crate) fn install(args: &[String], dir: &Path) -> Result<(), String> {
    let root = repo_root(dir)?;
    let project = resolve_project(args, &root)?;
    let hooks = hooks_dir(&root)?;
    std::fs::create_dir_all(&hooks)
        .map_err(|e| format!("failed to create {}: {}", hooks.display(), e))?;
    let bin = ug_bin();

    for hook in HOOKS {
        let path = hooks.join(hook.name());
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let (content, placement) = splice(&existing, &hook_block(hook, &bin, &project));
        std::fs::write(&path, content)
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        make_executable(&path)
            .map_err(|e| format!("failed to make {} executable: {}", path.display(), e))?;
        let note = match placement {
            Placement::Created => String::new(),
            Placement::Updated => format!(" {C_DIM}(refreshed){C_RESET}"),
            Placement::Merged => {
                format!(" {C_DIM}(appended to your existing hook){C_RESET}")
            }
        };
        println!(
            "  {C_GREEN}✓{C_RESET} {:<14} {C_DIM}{}{C_RESET}{}",
            hook.name(),
            hook.why(),
            note
        );
    }

    println!();
    println!(
        "{C_GREEN}✓{C_RESET} Hooks installed in {} — {C_BOLD}{}{C_RESET} now refreshes itself.",
        hooks.display(),
        project
    );
    println!(
        "{C_DIM}  repo {} → data {}{C_RESET}",
        root.display(),
        project::project_dir(&project).display()
    );
    println!(
        "{C_DIM}  Each run logs to {}. Skip once with UG_HOOK_DISABLE=1.{C_RESET}",
        log_path(&project).display()
    );
    Ok(())
}

/// Whether every ug block is already spliced into this repo's hooks:
/// `Some(false)` for a git repo without them, `None` when there is no repo (or
/// no git) to install into at all.
///
/// This exists for callers that want to *offer* the install rather than run it
/// — `ug connect` asks, and the answer only makes sense in a repo that would
/// benefit. A partial install counts as not installed, because that is the
/// state re-running `install` fixes.
pub(crate) fn installed_in(dir: &Path) -> Option<bool> {
    let root = repo_root(dir).ok()?;
    let hooks = hooks_dir(&root).ok()?;
    Some(HOOKS.iter().all(|h| {
        std::fs::read_to_string(hooks.join(h.name()))
            .map(|c| c.contains(BEGIN))
            .unwrap_or(false)
    }))
}

/// Whether `install` would find a graph to point the hooks at. Offering the
/// install before `ug gen` has run only earns the caller an error, so the
/// offer is replaced by the `ug gen` advice in that case.
pub(crate) fn has_indexed_project(args: &[String]) -> bool {
    let name = project::resolve_active_project_name(args, ".");
    project::read_meta(&project::project_dir(&name)).is_some()
}

fn run_install(args: &[String]) -> Result<(), String> {
    install(args, &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn run_uninstall() -> Result<(), String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = repo_root(&cwd)?;
    let hooks = hooks_dir(&root)?;

    let mut removed = 0;
    for hook in HOOKS {
        let path = hooks.join(hook.name());
        let Ok(existing) = std::fs::read_to_string(&path) else {
            continue;
        };
        match unsplice(&existing) {
            None => {}
            Some(None) => {
                let _ = std::fs::remove_file(&path);
                removed += 1;
                println!("  {C_GREEN}✓{C_RESET} removed {}", path.display());
            }
            Some(Some(rest)) => {
                std::fs::write(&path, rest)
                    .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
                removed += 1;
                println!(
                    "  {C_GREEN}✓{C_RESET} removed the ug block from {} {C_DIM}(your hook kept){C_RESET}",
                    path.display()
                );
            }
        }
    }
    if removed == 0 {
        println!("{C_YELLOW}•{C_RESET} No ug hooks in {} — nothing to do.", hooks.display());
    }
    Ok(())
}

fn run_status() -> Result<(), String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = repo_root(&cwd)?;
    let hooks = hooks_dir(&root)?;

    println!("{C_BOLD}git hooks{C_RESET} {C_DIM}{}{C_RESET}", hooks.display());
    let mut installed = 0;
    for hook in HOOKS {
        let content = std::fs::read_to_string(hooks.join(hook.name())).unwrap_or_default();
        let state = if content.contains(BEGIN) {
            installed += 1;
            format!("{C_GREEN}✓{C_RESET} installed {C_DIM}— {}{C_RESET}", hook.why())
        } else if !content.trim().is_empty() {
            format!("{C_YELLOW}·{C_RESET} {C_YELLOW}another hook is here{C_RESET} {C_DIM}— `ug hook install` appends to it{C_RESET}")
        } else {
            format!("{C_DIM}· not installed{C_RESET}")
        };
        println!("  {:<14} {}", hook.name(), state);
    }
    println!();
    if installed == 0 {
        println!(
            "Run {C_CYAN}ug hook install{C_RESET} to keep the graph in step with this repo."
        );
    } else if let Some(project) = hook_project(&hooks) {
        println!(
            "Refreshing project {C_BOLD}{}{C_RESET}; last run logged to {}",
            project,
            log_path(&project).display()
        );
        // The repo the *project* records, not `root` — when they disagree the
        // hooks are refreshing a graph of some other tree, which is the whole
        // reason `resolve_project` warns at install time.
        match project::read_meta(&project::project_dir(&project)) {
            Some(meta) if !meta.repo_root.is_empty() => {
                println!("{C_DIM}  indexes {}{C_RESET}", meta.repo_root);
            }
            _ => println!(
                "{C_YELLOW}·{C_RESET} {C_BOLD}{}{C_RESET} has no project.json — run {C_CYAN}ug gen{C_RESET}.",
                project
            ),
        }
        if let Some(age) = project::pending_vectors_age(&project::project_dir(&project)) {
            println!(
                "{C_YELLOW}·{C_RESET} Vectors owed for {} — hooks skip embedding to stay fast.",
                humanize(age)
            );
            println!(
                "  Run {C_CYAN}ug ingest -n {}{C_RESET} to catch semantic search up. \
                 {C_DIM}Everything else is current.{C_RESET}",
                project
            );
        }
    }
    Ok(())
}

/// Coarse "how long ago" for the pending-vectors report. Coarse on purpose:
/// the useful distinction is minutes vs. days, not seconds.
fn humanize(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=90 => "under a minute".to_string(),
        91..=5400 => format!("{}m", secs / 60),
        5401..=172_800 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Read the project name back out of an installed hook, so `status` reports
/// what will actually run rather than what would be resolved today.
fn hook_project(hooks: &Path) -> Option<String> {
    for hook in HOOKS {
        let Ok(content) = std::fs::read_to_string(hooks.join(hook.name())) else {
            continue;
        };
        if let Some(rest) = content.split("--name ").nth(1) {
            if let Some(name) = rest.split(" --").next() {
                return Some(name.trim().trim_matches('\'').to_string());
            }
        }
    }
    None
}

// ── the hook body: `ug hook run <name> -- <git args>` ──────────────────────

fn log_path(project: &str) -> PathBuf {
    project::project_dir(project).join("hook.log")
}

/// What the installed stubs call. Everything here runs inside a git
/// operation, so it stays quiet on success (one line), never blocks longer
/// than the update takes, and always exits 0.
fn run_run(args: &[String]) {
    let (own, git_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let Some(hook) = first_positional(own, &["--name", "-n"]).and_then(|n| Hook::from_name(&n))
    else {
        eprintln!("ug hook run: expected one of {}", HOOKS.map(|h| h.name()).join(", "));
        return;
    };
    let project = flag_value(own, &["--name", "-n"])
        .unwrap_or_else(|| project::resolve_active_project_name(own, "."));

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Ok(root) = repo_root(&cwd) else { return };

    // Only post-rewrite reads stdin, and reading it when git sent nothing
    // would block the hook forever.
    let stdin = if hook == Hook::PostRewrite {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
        buf
    } else {
        String::new()
    };

    let Some(diff) = diff_args(hook, &git_args, &stdin) else { return };
    let refs: Vec<&str> = diff.iter().map(String::as_str).collect();
    let Some(out) = git(&root, &refs) else { return };
    let indexed = project::read_meta(&project::project_dir(&project))
        .map(|m| m.files)
        .unwrap_or_default();
    let paths: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|p| worth_updating(&root, p, &indexed))
        .take(MAX_PATHS)
        .map(str::to_string)
        .collect();
    if paths.is_empty() {
        return;
    }

    let started = std::time::Instant::now();
    match spawn_update(&project, &root, &paths, hook) {
        Ok(true) => println!(
            // Named, not just counted: a repo can have several projects
            // indexing it, and `git commit` output is the only place a user
            // sees which one the hook keeps current.
            "{C_DIM}ug: {} refreshed for {} file(s) in {:.1}s{}{C_RESET}",
            project,
            paths.len(),
            started.elapsed().as_secs_f32(),
            // Named on every run, not just the first: the whole point of
            // skipping vectors is that someone has to know they are owed.
            // No age here — it is nearly always "just now", and `ug hook
            // status` is where the number means something.
            match project::pending_vectors_age(&project::project_dir(&project)) {
                Some(_) => format!(" · vectors pending (ug ingest -n {})", project),
                None => String::new(),
            }
        ),
        Ok(false) => eprintln!(
            "{C_YELLOW}ug: refresh of {} failed{C_RESET} — see {}",
            project,
            log_path(&project).display()
        ),
        Err(e) => eprintln!(
            "{C_YELLOW}ug: refresh of {} skipped{C_RESET} — {}",
            project, e
        ),
    }
}

/// Whether a path from a git diff is worth handing to `ug update`.
///
/// Keeps anything that exists (it may be new, or edited) and anything the
/// index already holds (so a deletion re-indexes the file away). Drops the
/// remainder: a path that is gone *and* was never indexed — a deleted asset,
/// a removed lockfile, a `.husky` script.
///
/// `ug update` treats those as an error, deliberately, because for a person
/// typing a filename the alternative is a silent no-op on a typo. A hook
/// passes whatever git printed, where an unindexed deletion is ordinary and
/// failing the whole refresh over one is not.
fn worth_updating(root: &Path, rel: &str, indexed: &[String]) -> bool {
    root.join(rel).exists() || indexed.iter().any(|f| f == rel)
}

/// Run `ug update` as a child and put its output in the project's `hook.log`.
///
/// A child rather than a direct call because `ug update` narrates: progress
/// bars, per-file counts, embedder timings. That is the right output for
/// someone who typed the command and pure noise in the middle of `git
/// commit`, and a log file is the only place it can go without threading a
/// quiet flag through the whole pipeline.
///
/// The output is captured rather than piped straight to the file so it can be
/// flattened to plain text first — see [`plain_text`].
fn spawn_update(
    project: &str,
    root: &Path,
    paths: &[String],
    hook: Hook,
) -> Result<bool, String> {
    let dir = project::project_dir(project);
    if !dir.exists() {
        return Err(format!("project {} is not indexed (run `ug gen`)", project));
    }

    let out = Command::new(ug_bin())
        .current_dir(root)
        .arg("update")
        .arg("-n")
        .arg(project)
        // Vectors are the slow part and the least urgent: skipping them keeps
        // graph.json and the store's structure current — which is what blast
        // radius reads — in a fraction of the time. `ug ingest` catches the
        // embeddings up. See `ug hook -h`.
        .arg("--no-embed")
        .args(paths)
        .env("UG_QUIET_LOGO", "1")
        .output()
        .map_err(|e| format!("could not run `ug update`: {}", e))?;

    let log = log_path(project);
    // Truncated, not appended: what matters is why the last run failed, and
    // an unbounded log inside the project dir is a slow leak.
    //
    // The header names every input the run depended on — which git hook fired,
    // which project it refreshed, which repo it read, which binary ran, and
    // which files were handed over. Reading a bare "graph refresh failed" and
    // then having to reconstruct all of that from the shell history is what
    // made the old one-line header useless for auditing.
    let body = format!(
        "── ug hook {hook} · {when} ──\n\
         project:  {project}\n\
         repo:     {repo}\n\
         data:     {data}\n\
         ug:       {bin} (v{version})\n\
         files:    {count}\n{files}\
         exit:     {exit}\n\
         ──\n{stdout}{stderr}",
        hook = hook.name(),
        when = project::format_epoch(project::now_epoch()),
        project = project,
        repo = root.display(),
        data = dir.display(),
        bin = ug_bin(),
        version = env!("CARGO_PKG_VERSION"),
        count = paths.len(),
        files = paths
            .iter()
            .map(|p| format!("          {}\n", p))
            .collect::<String>(),
        exit = match out.status.code() {
            Some(c) => c.to_string(),
            None => "signal".to_string(),
        },
        stdout = plain_text(&out.stdout),
        stderr = plain_text(&out.stderr),
    );
    std::fs::write(&log, body).map_err(|e| format!("cannot write {}: {}", log.display(), e))?;
    Ok(out.status.success())
}

/// Flatten terminal output into something `less` will open as text.
///
/// `ug update` writes for a terminal: colour escapes, and progress bars that
/// redraw by returning to the start of the line with `\r`. Written verbatim
/// to a file both survive — the escapes are control bytes, which is exactly
/// what makes a pager call the log "a binary file", and the `\r` redraws
/// stack every intermediate percentage onto one unreadable line.
///
/// So: keep only what the terminal would have been left showing (the text
/// after the last `\r` of each line), and drop the escape sequences.
fn plain_text(raw: &[u8]) -> String {
    let mut out = String::new();
    for line in String::from_utf8_lossy(raw).split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        // `rsplit` on '\r' is the final redraw — the 100% line, not the
        // eleven partial ones before it.
        let final_paint = body.rsplit('\r').next().unwrap_or("");
        let stripped = strip_ansi(final_paint);
        let stripped = stripped.trim_end();
        if stripped.is_empty() && newline.is_empty() {
            continue;
        }
        out.push_str(stripped);
        out.push_str(newline);
    }
    out
}

/// Remove ANSI escape sequences. Covers CSI (`ESC [ … final`) — every colour
/// code `ug` emits — and drops a bare trailing `ESC` rather than keeping a
/// control byte.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameter bytes, then one final byte in 0x40..=0x7E.
            Some('[') => {
                for f in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&f) {
                        break;
                    }
                }
            }
            // Any other two-character escape (or a stray ESC at the end):
            // both bytes go.
            _ => {}
        }
    }
    out
}

// ── entry point ────────────────────────────────────────────────────────────

/// `ug hook <install|uninstall|status|run>`.
pub(crate) fn run_hook(args: &[String]) {
    if has_flag(args, "-h") || has_flag(args, "--help") {
        println!("{}", hook_help_text());
        return;
    }
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let result = match sub {
        "install" => run_install(&rest),
        "uninstall" | "remove" => run_uninstall(),
        "status" => run_status(),
        // The stubs' entry point. It never fails a git operation, so it
        // reports through its own output rather than an exit code.
        "run" => {
            run_run(&rest);
            Ok(())
        }
        other => Err(format!(
            "Unknown `ug hook` subcommand '{}'. Expected install, uninstall, status or run.",
            other
        )),
    };
    if let Err(e) = result {
        eprintln!("{C_YELLOW}⚠{C_RESET}  {}", e);
        std::process::exit(1);
    }
}

/// Built as a string so a test can read it.
pub(crate) fn hook_help_text() -> String {
    let mut o = String::new();
    macro_rules! line {
        ($($arg:tt)*) => { o.push_str(&format!("{}\n", format_args!($($arg)*))) };
    }
    line!("  {C_BOLD}{C_GREEN}★ ug hook{C_RESET}  {C_YELLOW}— let git keep the graph fresh{C_RESET}");
    line!("  {C_BOLD}{C_CYAN}────────────────────────────────────────────────────────{C_RESET}");
    line!("");
    line!("  Installs git hooks that run {C_CYAN}ug update{C_RESET} on the paths each event");
    line!("  touched, so the structural tools ({C_CYAN}find_usages{C_RESET}, {C_CYAN}ug query{C_RESET}, blast radius)");
    line!("  never answer from a graph the repo has moved on from.");
    line!("");
    line!("{C_BOLD}Usage:{C_RESET}  ug hook install [-n <project>]   {C_DIM}write the hooks{C_RESET}");
    line!("        ug hook uninstall                {C_DIM}remove them again{C_RESET}");
    line!("        ug hook status                   {C_DIM}what is installed (default){C_RESET}");
    line!("");
    line!("{C_BOLD}Hooks installed:{C_RESET}");
    for hook in HOOKS {
        line!("  {C_CYAN}{:<14}{C_RESET} {C_DIM}{}{C_RESET}", hook.name(), hook.why());
    }
    line!("");
    line!("  An existing hook of the same name is kept — ug appends its own block,");
    line!("  marked so {C_CYAN}ug hook uninstall{C_RESET} can strip exactly that much back out.");
    line!("  {C_CYAN}core.hooksPath{C_RESET} is honoured, so husky and lefthook repos work.");
    line!("");
    line!("{C_BOLD}Fast on purpose — vectors are the one thing left behind:{C_RESET}");
    line!("  Hook runs pass {C_CYAN}--no-embed{C_RESET}, which {C_BOLD}still ingests into the db{C_RESET} — nodes,");
    line!("  edges, facts and keyword statistics all land, only the vectors are");
    line!("  skipped, and no embedding model is loaded (most of a small run).");
    line!("  So {C_BOLD}find_usages, ug query, diff_impact and blast radius stay exact{C_RESET}.");
    line!("  {C_DIM}(Not to be confused with {C_RESET}{C_CYAN}--no-ingest{C_RESET}{C_DIM}, which writes nothing to the db at");
    line!("  all and would leave ug query stale too. See {C_RESET}{C_CYAN}ug update -h{C_RESET}{C_DIM}.){C_RESET}");
    line!("  Catch semantic search up whenever you like: {C_CYAN}ug ingest -n <project>{C_RESET}.");
    line!("  {C_DIM}It embeds only the nodes still owed one. Until then {C_RESET}{C_CYAN}ug hook status{C_RESET}{C_DIM} and each");
    line!("  hook run say how far behind they are.{C_RESET}");
    line!("");
    line!("{C_BOLD}While they are installed:{C_RESET}");
    line!("  {C_CYAN}UG_HOOK_DISABLE=1 git rebase …{C_RESET}  {C_DIM}skip the refresh for one command{C_RESET}");
    line!("  {C_DIM}Each run prints one line and logs the detail to ~/.ug/<project>/hook.log.{C_RESET}");
    line!("  {C_DIM}A failed refresh never fails the git command.{C_RESET}");
    line!("");
    line!("{C_BOLD}Examples:{C_RESET}");
    line!("  {C_CYAN}ug hook install{C_RESET}                  {C_DIM}# hooks for the active project{C_RESET}");
    line!("  {C_CYAN}ug hook install -n myrepo{C_RESET}        {C_DIM}# … for a named one{C_RESET}");
    line!("  {C_CYAN}ug connect claude --hooks{C_RESET}        {C_DIM}# wire the agent and the hooks in one go{C_RESET}");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    /// The whole point of the marker block: a repo that already has a
    /// `post-commit` hook must keep it, and must get it back intact.
    #[test]
    fn an_existing_hook_survives_install_and_uninstall() {
        let mine = "#!/bin/sh\necho 'my hook'\n";
        let block = hook_block(Hook::PostCommit, "/usr/local/bin/ug", "demo");

        let (merged, placement) = splice(mine, &block);
        assert_eq!(placement, Placement::Merged);
        assert!(merged.contains("echo 'my hook'"));
        assert!(merged.contains("hook run post-commit"));

        // Re-installing replaces our block rather than stacking a second.
        let (again, placement) = splice(&merged, &block);
        assert_eq!(placement, Placement::Updated);
        assert_eq!(again.matches(BEGIN).count(), 1);

        let left = unsplice(&again).expect("our block is in there");
        assert_eq!(left, Some(mine.to_string()), "the user's hook must come back byte-identical");
    }

    /// A hook file that is only ours is deleted, not left as an empty stub
    /// that looks installed.
    #[test]
    fn a_hook_file_we_created_is_removed_whole() {
        let (content, placement) = splice("", &hook_block(Hook::PostMerge, "/bin/ug", "demo"));
        assert_eq!(placement, Placement::Created);
        assert!(content.starts_with("#!/bin/sh\n"));
        assert_eq!(unsplice(&content), Some(None));
    }

    #[test]
    fn a_foreign_hook_is_left_alone_by_uninstall() {
        assert_eq!(unsplice("#!/bin/sh\nexit 0\n"), None);
    }

    /// The stub runs inside `git commit`; a path with a space in it must not
    /// turn into two arguments or a second command.
    #[test]
    fn the_stub_quotes_the_paths_it_bakes_in() {
        let block = hook_block(Hook::PostCommit, "/Users/a b/ug", "my project");
        assert!(block.contains("'/Users/a b/ug'"));
        assert!(block.contains("--name 'my project'"));
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        // Never fails the git command, and stays skippable.
        assert!(block.contains("|| true"));
        assert!(block.contains("UG_HOOK_DISABLE"));
    }

    #[test]
    fn each_hook_diffs_the_right_two_commits() {
        assert_eq!(
            diff_args(Hook::PostCommit, &[], ""),
            Some(argv("diff-tree --no-commit-id --name-only -r HEAD"))
        );
        assert_eq!(
            diff_args(Hook::PostMerge, &[], ""),
            Some(argv("diff --name-only ORIG_HEAD HEAD"))
        );
        assert_eq!(
            diff_args(Hook::PostCheckout, &argv("aaa bbb 1"), ""),
            Some(argv("diff --name-only aaa bbb"))
        );
        assert_eq!(
            diff_args(Hook::PostRewrite, &[], "old1 new1\nold2 new2\n"),
            Some(argv("diff --name-only old1 HEAD"))
        );
    }

    /// The cases where re-indexing would be guesswork or busywork.
    #[test]
    fn a_hook_run_declines_when_nothing_can_be_derived() {
        // A file checkout (flag 0) reports the same commit twice — the paths
        // that moved are not in the diff.
        assert_eq!(diff_args(Hook::PostCheckout, &argv("aaa aaa 0"), ""), None);
        // A fresh clone has no previous head, and no graph either.
        assert_eq!(
            diff_args(Hook::PostCheckout, &argv("0000000000000000000000000000000000000000 bbb 1"), ""),
            None
        );
        // No rewritten commits on stdin.
        assert_eq!(diff_args(Hook::PostRewrite, &[], ""), None);
    }

    /// The log is written from a child that was formatting for a terminal.
    /// Left verbatim, the escape bytes make `less` refuse to open it as text
    /// and the `\r` redraws pile every intermediate percentage onto one line.
    #[test]
    fn the_log_is_plain_text_a_pager_will_open() {
        let raw = b"\x1b[36m\xe2\x96\xb8\x1b[0m Writing: \x1b[33m 10.0%\x1b[0m (1/10)\r\
                    \x1b[36m\xe2\x96\xb8\x1b[0m Writing: \x1b[32m100.0% done\x1b[0m\n\
                    plain line\n";
        let out = plain_text(raw);
        assert_eq!(out, "▸ Writing: 100.0% done\nplain line\n");
        assert!(!out.contains('\u{1b}'), "escape bytes survived: {:?}", out);
        assert!(!out.contains('\r'), "carriage returns survived: {:?}", out);
    }

    #[test]
    fn ansi_stripping_keeps_the_text_and_drops_the_codes() {
        assert_eq!(strip_ansi("\x1b[1;32mgreen\x1b[0m text"), "green text");
        assert_eq!(strip_ansi("no codes"), "no codes");
        // A truncated sequence at the end must not leave a control byte.
        assert_eq!(strip_ansi("cut\x1b"), "cut");
        assert_eq!(strip_ansi("cut\x1b[3"), "cut");
    }

    /// A commit that removes something the index never held — a deleted
    /// asset, a lockfile — must not fail the whole refresh.
    #[test]
    fn paths_git_names_but_the_index_cannot_use_are_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("live.rs"), "fn main() {}").expect("write");
        let indexed = vec!["gone.rs".to_string()];

        // Exists on disk: new or edited, either way re-index it.
        assert!(worth_updating(root, "live.rs", &indexed));
        // Gone, but the index holds it: this is the deletion to apply.
        assert!(worth_updating(root, "gone.rs", &indexed));
        // Gone and never indexed: nothing to do, and not an error.
        assert!(!worth_updating(root, "assets/logo.png", &indexed));
    }

    #[test]
    fn the_hook_asks_for_the_fast_update() {
        // Loading the embedding model costs more than the entire structural
        // refresh; a hook that paid it on every commit would be the reason
        // people uninstall this.
        let block = hook_block(Hook::PostCommit, "/bin/ug", "demo");
        assert!(block.contains("hook run post-commit"));
        assert!(
            hook_help_text().contains("--no-embed") || hook_help_text().contains("ug ingest"),
            "`ug hook -h` must say how to catch the vectors up"
        );
    }

    #[test]
    fn the_project_name_can_be_read_back_out_of_an_installed_hook() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("post-commit"),
            hook_block(Hook::PostCommit, "/bin/ug", "myrepo"),
        )
        .expect("write hook");
        assert_eq!(hook_project(dir.path()), Some("myrepo".to_string()));
    }

    #[test]
    fn the_help_names_every_hook_and_the_escape_hatch() {
        let help = hook_help_text();
        for expected in [
            "post-commit",
            "post-merge",
            "post-checkout",
            "post-rewrite",
            "UG_HOOK_DISABLE",
            "ug hook uninstall",
            "core.hooksPath",
        ] {
            assert!(help.contains(expected), "`ug hook -h` is missing {expected}");
        }
    }
}
