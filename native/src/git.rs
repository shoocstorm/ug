//! Soft git integration: what changed, and where.
//!
//! `ug` indexes a tree, not a history — nothing else in the crate needs git
//! except [`crate::cli::hook`], which shells out to install hooks. This
//! module is the other half: it reads a *diff* so [`crate::walk`] can seed a
//! walkthrough from a change instead of from a question.
//!
//! **git is a soft dependency and stays one.** Every entry point returns
//! [`GitError`] rather than panicking or exiting, every variant carries a
//! sentence a user can act on ([`GitError::hint`]) and a stable
//! [`GitError::code`] the web UI switches on, and nothing here is reached
//! unless the caller asked for a diff. A machine with no git, or a project
//! that is not a working tree, loses `ug walk` and keeps everything else.
//!
//! Two design notes worth keeping:
//!
//! 1. **One `git diff -U0` call does the whole job.** The patch header
//!    carries the status (`new file mode`, `rename from`), the `@@` lines
//!    carry the changed line ranges, and the `+`/`-` lines carry the
//!    counts — so the alternative (`--name-status`, then `--numstat`, then
//!    the patch) is three process spawns to learn what one already said.
//!    `-U0` is what makes the ranges exact: with context lines the hunk
//!    spans code that did not change, and every unchanged neighbouring
//!    symbol joins the walk.
//!
//! 2. **Line ranges are on the new side.** A walk visits code that exists,
//!    so the ranges are the ones you could open in an editor right now.
//!    Pure deletions have no new-side extent; they collapse to the point
//!    they were removed from, which still lands inside the enclosing symbol.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Git's hash of the empty tree. Diffing a root commit against this is how
/// you see it as "everything added" — `<root>^` does not resolve, so the
/// usual parent form fails on exactly the commit a new repo has.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Files carried out of one diff. A branch-length range can touch
/// thousands; past this the answer is a report, not a walk, and every
/// extra file is prompt budget the guide spends without visiting.
pub const MAX_DIFF_FILES: usize = 400;

/// Commits offered to a picker by default.
pub const DEFAULT_COMMIT_LIMIT: usize = 30;

// ── errors ─────────────────────────────────────────────────────────────────

/// Why a git question could not be answered. Every variant is a *state of
/// the user's machine*, not a bug, so each one carries the sentence that
/// tells them what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// No `git` on `PATH`.
    NotInstalled,
    /// There is a git, but this directory is not inside a working tree.
    NotARepo(PathBuf),
    /// A repository with no commits yet.
    NoCommits,
    /// A revision the user named that git does not recognise.
    BadRev { rev: String, detail: String },
    /// git ran and failed for some other reason; `detail` is its stderr.
    Failed { what: String, detail: String },
}

impl GitError {
    /// Stable machine-readable tag. The web UI switches on this to decide
    /// which banner to raise, so these strings are API — renaming one is a
    /// breaking change to `/api/git/status`.
    pub fn code(&self) -> &'static str {
        match self {
            GitError::NotInstalled => "not_installed",
            GitError::NotARepo(_) => "not_a_repo",
            GitError::NoCommits => "no_commits",
            GitError::BadRev { .. } => "bad_rev",
            GitError::Failed { .. } => "failed",
        }
    }

    /// One sentence the user can act on. Paired with `Display` (which says
    /// what happened) to say what to do next.
    pub fn hint(&self) -> String {
        match self {
            GitError::NotInstalled => {
                "Install git and re-run — every other ug command works without it.".to_string()
            }
            GitError::NotARepo(p) => format!(
                "Run this from inside a git working tree, or point --repo-root at one ({} is not in a repository).",
                p.display()
            ),
            GitError::NoCommits => {
                "Make a commit first, or walk the uncommitted changes with `ug walk working`."
                    .to_string()
            }
            GitError::BadRev { rev, .. } => format!(
                "Check the spelling of `{}` — `ug walk --commits` lists the recent commits by hash.",
                rev
            ),
            GitError::Failed { .. } => {
                "Re-run the same range with git itself to see the full error.".to_string()
            }
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::NotInstalled => write!(f, "git is not installed (or not on PATH)"),
            GitError::NotARepo(p) => write!(f, "{} is not inside a git repository", p.display()),
            GitError::NoCommits => write!(f, "this repository has no commits yet"),
            GitError::BadRev { rev, detail } => {
                if detail.is_empty() {
                    write!(f, "git does not know the revision `{}`", rev)
                } else {
                    write!(f, "git does not know the revision `{}`: {}", rev, detail)
                }
            }
            GitError::Failed { what, detail } => {
                if detail.is_empty() {
                    write!(f, "git {} failed", what)
                } else {
                    write!(f, "git {} failed: {}", what, detail)
                }
            }
        }
    }
}

impl std::error::Error for GitError {}

// ── plumbing ───────────────────────────────────────────────────────────────

/// Is there a usable `git` on this machine? Probed once and remembered:
/// the answer cannot change inside a process, and `/api/git/status` is
/// polled by a page that re-renders on every mode switch.
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Run git in `dir` and return stdout.
///
/// The `-c` prefixes are not preferences, they are correctness:
/// `core.quotePath=false` keeps non-ASCII paths readable instead of
/// `\303\251`-escaped, and `core.pager=cat` stops a user's `core.pager`
/// setting from wrapping the output in anything. `--no-ext-diff` and
/// `GIT_OPTIONAL_LOCKS=0` keep a configured difftool and the index lock
/// out of a read-only question.
fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    if !available() {
        return Err(GitError::NotInstalled);
    }
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.quotePath=false", "-c", "core.pager=cat"])
        .args(args)
        .output()
        .map_err(|e| GitError::Failed {
            what: args.first().copied().unwrap_or("command").to_string(),
            detail: e.to_string(),
        })?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(GitError::Failed {
            what: args.first().copied().unwrap_or("command").to_string(),
            detail,
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Top of the working tree containing `dir`, canonicalised.
///
/// Canonicalisation is load-bearing, for the reason `cli::hook` records:
/// git reports `/tmp/...` where the rest of `ug` stores `/private/tmp/...`,
/// and an uncanonicalised root makes every later path comparison lie.
pub fn repo_root(dir: &Path) -> Result<PathBuf, GitError> {
    let top = git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|e| match e {
            GitError::NotInstalled => e,
            _ => GitError::NotARepo(dir.to_path_buf()),
        })?
        .trim()
        .to_string();
    if top.is_empty() {
        return Err(GitError::NotARepo(dir.to_path_buf()));
    }
    let p = PathBuf::from(top);
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Does `HEAD` resolve? False in a repository whose first commit has not
/// been made — where `git diff HEAD` fails with a message about an
/// ambiguous argument rather than saying what is actually wrong.
fn has_commits(root: &Path) -> bool {
    git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

// ── repository state ───────────────────────────────────────────────────────

/// What the UI needs to decide whether to offer a walk at all, and what to
/// pre-select when it does.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoStatus {
    pub root: String,
    /// Current branch, or a short sha when detached.
    pub branch: String,
    pub head: Option<String>,
    pub head_subject: Option<String>,
    /// Are there uncommitted changes to walk right now?
    pub dirty: bool,
    /// Is anything staged? Distinguishes "uncommitted" from "staged".
    pub staged: bool,
    /// `main`, `master`, whatever `origin/HEAD` points at — the natural
    /// other end of a "what does this branch change" comparison. `None`
    /// when the repo has no such branch, which is common enough (a
    /// detached CI checkout) that guessing would be worse than omitting.
    pub default_branch: Option<String>,
}

/// Probe the repository containing `dir`.
pub fn status(dir: &Path) -> Result<RepoStatus, GitError> {
    let root = repo_root(dir)?;
    if !has_commits(&root) {
        return Err(GitError::NoCommits);
    }
    let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let head = git(&root, &["rev-parse", "HEAD"]).ok().map(|s| s.trim().to_string());
    let head_subject = git(&root, &["log", "-1", "--pretty=format:%s"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // `--porcelain` is the stable, parse-friendly form; we only need
    // "is it empty" and "does any line have a staged column".
    let porcelain = git(&root, &["status", "--porcelain"]).unwrap_or_default();
    let dirty = porcelain.lines().any(|l| !l.trim().is_empty());
    let staged = porcelain
        .lines()
        .any(|l| l.len() >= 2 && !matches!(&l[0..1], " " | "?" | "!"));
    Ok(RepoStatus {
        root: root.display().to_string(),
        branch: if branch.is_empty() || branch == "HEAD" {
            head.as_deref().map(short_sha).unwrap_or_default()
        } else {
            branch
        },
        head,
        head_subject,
        dirty,
        staged,
        default_branch: default_branch(&root),
    })
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The branch a change is naturally compared against. `origin/HEAD` is the
/// authoritative answer when the remote published one; the two fallbacks
/// are only consulted because a shallow or remote-less clone never has it.
fn default_branch(root: &Path) -> Option<String> {
    if let Ok(s) = git(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let s = s.trim();
        if let Some(name) = s.strip_prefix("origin/") {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    for cand in ["main", "master", "develop"] {
        if git(root, &["rev-parse", "--verify", "--quiet", cand]).is_ok_and(|s| !s.trim().is_empty())
        {
            return Some(cand.to_string());
        }
    }
    None
}

// ── commits ────────────────────────────────────────────────────────────────

/// One commit, shaped for a picker: enough to recognise it without opening
/// it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    /// `git`'s own relative date ("3 hours ago") — the form people read.
    pub relative: String,
    /// ISO-8601, for sorting and tooltips.
    pub date: String,
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// Field and record separators. ASCII unit/record separators cannot occur
/// in a commit subject or an author name, which `\t` and `\n` both can.
const FS: char = '\x1f';
const RS: char = '\x1e';

/// The most recent `limit` commits reachable from `rev` (default `HEAD`).
///
/// `--shortstat` rides along in the same call: the picker shows "4 files,
/// +112/-30" next to every entry, and fetching that per commit afterwards
/// would be one process spawn per row.
pub fn recent_commits(dir: &Path, limit: usize, rev: Option<&str>) -> Result<Vec<Commit>, GitError> {
    let root = repo_root(dir)?;
    if !has_commits(&root) {
        return Err(GitError::NoCommits);
    }
    let n = limit.clamp(1, 500).to_string();
    let pretty = format!("--pretty=format:{RS}%H{FS}%h{FS}%s{FS}%an{FS}%ar{FS}%aI");
    let mut args: Vec<&str> = vec!["log", "-n", &n, &pretty, "--shortstat"];
    if let Some(r) = rev {
        args.push(r);
    }
    let out = git(&root, &args).map_err(|e| match (rev, &e) {
        (Some(r), GitError::Failed { detail, .. }) => GitError::BadRev {
            rev: r.to_string(),
            detail: detail.clone(),
        },
        _ => e,
    })?;
    Ok(parse_commit_log(&out))
}

/// Parse the `%H…%aI` + `--shortstat` stream `recent_commits` asks for.
/// Split out so the record shape is testable without a repository.
fn parse_commit_log(out: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    for rec in out.split(RS) {
        let rec = rec.trim_start_matches('\n');
        if rec.trim().is_empty() {
            continue;
        }
        // The record is one `%`-formatted line, then git's own shortstat
        // line (absent for merges and empty commits).
        let mut lines = rec.lines();
        let Some(head) = lines.next() else { continue };
        let f: Vec<&str> = head.split(FS).collect();
        if f.len() < 6 {
            continue;
        }
        let (files, insertions, deletions) = lines
            .find(|l| l.contains("changed"))
            .map(parse_shortstat)
            .unwrap_or((0, 0, 0));
        commits.push(Commit {
            sha: f[0].to_string(),
            short: f[1].to_string(),
            subject: f[2].to_string(),
            author: f[3].to_string(),
            relative: f[4].to_string(),
            date: f[5].to_string(),
            files,
            insertions,
            deletions,
        });
    }
    commits
}

/// ` 3 files changed, 42 insertions(+), 7 deletions(-)` → `(3, 42, 7)`.
/// Each clause is optional — a pure addition has no deletions clause.
fn parse_shortstat(line: &str) -> (u32, u32, u32) {
    let mut out = (0u32, 0u32, 0u32);
    for clause in line.split(',') {
        let clause = clause.trim();
        let n: u32 = clause
            .split_whitespace()
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        if clause.contains("file") {
            out.0 = n;
        } else if clause.contains("insertion") {
            out.1 = n;
        } else if clause.contains("deletion") {
            out.2 = n;
        }
    }
    out
}

// ── revision specs ─────────────────────────────────────────────────────────

/// What the user asked to walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevSpec {
    /// Everything not committed — staged and unstaged together. The default,
    /// because it is the change you are in the middle of making.
    Working,
    /// The index alone.
    Staged,
    /// One commit, against its parent.
    Commit(String),
    /// A range, in either of git's spellings (`a..b`, `a...b`).
    Range(String),
}

impl RevSpec {
    /// Interpret a user-typed spec. Empty means "what I have not committed",
    /// which is what someone typing `ug walk` with nothing else means.
    pub fn parse(raw: &str) -> RevSpec {
        let s = raw.trim();
        match s.to_ascii_lowercase().as_str() {
            "" | "." | "working" | "worktree" | "uncommitted" | "dirty" => RevSpec::Working,
            "staged" | "cached" | "index" => RevSpec::Staged,
            _ if s.contains("..") => RevSpec::Range(s.to_string()),
            _ => RevSpec::Commit(s.to_string()),
        }
    }

    /// How this reads in a heading.
    pub fn label(&self) -> String {
        match self {
            RevSpec::Working => "uncommitted changes".to_string(),
            RevSpec::Staged => "staged changes".to_string(),
            RevSpec::Commit(c) => format!("commit {}", c),
            RevSpec::Range(r) => format!("range {}", r),
        }
    }

    /// The canonical string form, round-trippable through `parse`.
    pub fn as_arg(&self) -> String {
        match self {
            RevSpec::Working => "working".to_string(),
            RevSpec::Staged => "staged".to_string(),
            RevSpec::Commit(c) => c.clone(),
            RevSpec::Range(r) => r.clone(),
        }
    }
}

// ── diffs ──────────────────────────────────────────────────────────────────

/// What happened to a file in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeStatus::Added => "added",
            ChangeStatus::Modified => "modified",
            ChangeStatus::Deleted => "deleted",
            ChangeStatus::Renamed => "renamed",
        }
    }
}

/// An inclusive 1-based line range on the *new* side of the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    /// Do these two ranges share a line? The whole hunk→symbol mapping is
    /// this predicate, so it lives next to the type rather than in the
    /// caller.
    pub fn overlaps(&self, start: u32, end: u32) -> bool {
        self.start <= end && start <= self.end
    }
}

/// One `@@` block: where it lands, and how much it moved.
///
/// The counts are per hunk rather than only per file because a walk stops
/// at *symbols*, and "+40/-2" against a 900-line file says nothing about
/// the function being visited. With `-U0` every line in `range` is an
/// added line, so a hunk's own numbers attribute cleanly to whichever
/// symbol encloses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Hunk {
    pub range: LineRange,
    pub added: u32,
    pub removed: u32,
}

/// One file's worth of a diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileChange {
    /// Path on the new side, repo-root relative. For a deletion this is
    /// the path the file had.
    pub path: String,
    /// Where a rename came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub added: u32,
    pub removed: u32,
    /// Changed hunks, positioned on the new side. Empty for deletions and
    /// for binary files.
    pub hunks: Vec<Hunk>,
    pub binary: bool,
}

/// A diff, plus the commits that produced it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffSummary {
    /// The spec as given, canonicalised (`RevSpec::as_arg`).
    pub spec: String,
    /// How it reads in a heading ("commit a1b2c3d", "uncommitted changes").
    pub label: String,
    pub files: Vec<FileChange>,
    pub insertions: u32,
    pub deletions: u32,
    /// Commits in the range, newest first. Empty for working/staged.
    pub commits: Vec<Commit>,
    /// True when the file list hit [`MAX_DIFF_FILES`].
    pub truncated: bool,
}

/// A commit subject is one line by construction, but nothing stops that
/// line being 200 characters — and `label` is a heading, a tour title and
/// a picker row. Past this it is truncated.
const MAX_SUBJECT_CHARS: usize = 72;

fn short_subject(subject: &str) -> String {
    if subject.chars().count() <= MAX_SUBJECT_CHARS {
        return subject.to_string();
    }
    let head: String = subject.chars().take(MAX_SUBJECT_CHARS - 1).collect();
    format!("{}…", head.trim_end())
}

impl DiffSummary {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The change named in as few words as possible — `commit a1b2c3d`,
    /// not the commit's whole subject.
    ///
    /// `label` is what a heading wants; this is what a *sentence* wants.
    /// Interpolating a 200-character subject mid-warning makes the warning
    /// unreadable, which is the one thing a warning cannot be.
    pub fn short_label(&self) -> String {
        match self.commits.first() {
            Some(c) if self.commits.len() == 1 => format!("commit {}", c.short),
            Some(_) => format!("`{}`", self.spec),
            None => self.label.clone(),
        }
    }

    /// Every changed path, in diff order.
    pub fn paths(&self) -> Vec<&str> {
        self.files.iter().map(|f| f.path.as_str()).collect()
    }
}

/// Read a diff for `spec` from the repository containing `dir`.
pub fn diff(dir: &Path, spec: &RevSpec) -> Result<DiffSummary, GitError> {
    let root = repo_root(dir)?;
    let needs_head = !matches!(spec, RevSpec::Range(_));
    if needs_head && !has_commits(&root) {
        return Err(GitError::NoCommits);
    }

    // Resolved separately from the diff so an unknown revision reports as
    // a bad rev with the name the user typed, rather than as a diff that
    // failed for some unstated reason.
    let target = match spec {
        RevSpec::Commit(c) => Some(resolve(&root, c)?),
        _ => None,
    };

    let mut args: Vec<String> = vec![
        "diff".into(),
        "-U0".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--find-renames".into(),
    ];
    match spec {
        RevSpec::Working => args.push("HEAD".into()),
        RevSpec::Staged => args.push("--cached".into()),
        RevSpec::Commit(_) => {
            let sha = target.as_deref().unwrap_or_default();
            // A root commit has no parent, so the usual `<sha>^ <sha>` form
            // fails on exactly the commit a fresh repository has. Diffing
            // against the empty tree says the true thing: all of it is new.
            let parent = git(&root, &["rev-parse", "--verify", "--quiet", &format!("{sha}^")])
                .map(|s| s.trim().to_string())
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| EMPTY_TREE.to_string());
            args.push(parent);
            args.push(sha.to_string());
        }
        RevSpec::Range(r) => args.push(r.clone()),
    }
    args.push("--".into());

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let patch = git(&root, &argv).map_err(|e| match (spec, &e) {
        (RevSpec::Range(r), GitError::Failed { detail, .. }) => GitError::BadRev {
            rev: r.clone(),
            detail: detail.clone(),
        },
        _ => e,
    })?;

    let (mut files, truncated) = parse_patch(&patch);
    // `git diff HEAD` does not see untracked files, so the single most
    // common walk — "what am I in the middle of changing" — would silently
    // omit the new file you just wrote. It is not in the diff because git
    // has never been told about it, which is not the same as unchanged.
    if matches!(spec, RevSpec::Working) {
        files.extend(untracked_as_added(&root, files.len()));
    }
    let insertions = files.iter().map(|f| f.added).sum();
    let deletions = files.iter().map(|f| f.removed).sum();

    // The commits behind the diff, so a walk can say "these three commits"
    // rather than "this range". Best-effort: a range git can diff but not
    // log is odd but not a reason to fail the walk.
    let commits = match spec {
        RevSpec::Commit(_) => target
            .as_deref()
            .and_then(|sha| recent_commits(&root, 1, Some(sha)).ok())
            .unwrap_or_default(),
        RevSpec::Range(r) => recent_commits(&root, 50, Some(r)).unwrap_or_default(),
        _ => Vec::new(),
    };

    Ok(DiffSummary {
        spec: spec.as_arg(),
        label: match spec {
            RevSpec::Commit(_) => {
                let c = commits.first();
                match c {
                    Some(c) => format!("commit {} — {}", c.short, short_subject(&c.subject)),
                    None => spec.label(),
                }
            }
            _ => spec.label(),
        },
        files,
        insertions,
        deletions,
        commits,
        truncated,
    })
}

/// Untracked files, as though the whole file had just been added.
///
/// `--exclude-standard` is what keeps this honest: it applies `.gitignore`,
/// so `target/` and `node_modules/` stay out. Line counts come from the
/// files themselves, which is why this is capped — a walk is worth one
/// pass over a handful of new files, not over a directory someone forgot
/// to ignore.
fn untracked_as_added(root: &Path, already: usize) -> Vec<FileChange> {
    /// Enough for a feature's worth of new files; past this the answer is
    /// "you have untracked noise", not a walk.
    const MAX_UNTRACKED: usize = 50;
    /// Counting lines in a huge blob to decide it is one stop is not worth
    /// the read.
    const MAX_BYTES: u64 = 2 * 1024 * 1024;

    let budget = MAX_DIFF_FILES.saturating_sub(already).min(MAX_UNTRACKED);
    if budget == 0 {
        return Vec::new();
    }
    let Ok(listing) = git(root, &["ls-files", "--others", "--exclude-standard"]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rel in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if out.len() >= budget {
            break;
        }
        let path = root.join(rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_BYTES {
            continue;
        }
        // A file git cannot read as UTF-8 is treated as binary: no hunks,
        // so nothing tries to map lines onto it.
        let (lines, binary) = match std::fs::read_to_string(&path) {
            Ok(text) => (text.lines().count() as u32, false),
            Err(_) => (0, true),
        };
        let hunks = if binary || lines == 0 {
            Vec::new()
        } else {
            vec![Hunk {
                range: LineRange { start: 1, end: lines },
                added: lines,
                removed: 0,
            }]
        };
        out.push(FileChange {
            path: rel.to_string(),
            old_path: None,
            status: ChangeStatus::Added,
            added: lines,
            removed: 0,
            hunks,
            binary,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Turn a user-typed revision into a sha, reporting the name they typed
/// when it does not resolve.
fn resolve(root: &Path, rev: &str) -> Result<String, GitError> {
    // `^{commit}` peels a tag to the commit it points at and rejects a ref
    // that names a tree or a blob — both of which `rev-parse` alone would
    // happily hand back, to fail confusingly one call later.
    git(root, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
        .map_err(|e| match e {
            GitError::Failed { detail, .. } => GitError::BadRev {
                rev: rev.to_string(),
                detail,
            },
            other => other,
        })
        .and_then(|s| {
            let s = s.trim().to_string();
            if s.is_empty() {
                Err(GitError::BadRev {
                    rev: rev.to_string(),
                    detail: String::new(),
                })
            } else {
                Ok(s)
            }
        })
}

/// Parse a `git diff -U0` patch into per-file changes.
///
/// Pure and separately tested: this is the part that silently mis-maps a
/// walk if it drifts, and a fixture patch exercises it far more cheaply
/// than building a repository per case.
fn parse_patch(patch: &str) -> (Vec<FileChange>, bool) {
    let mut files: Vec<FileChange> = Vec::new();
    let mut truncated = false;
    let mut cur: Option<FileChange> = None;
    // Set by `rename from`, consumed by the `rename to` that follows it.
    let mut rename_from: Option<String> = None;

    macro_rules! flush {
        () => {
            if let Some(mut f) = cur.take() {
                // A deleted file has no new side, but its trailing hunk is
                // spelled `@@ -1,2 +0,0 @@` — the same shape as lines
                // removed from the very top of a file that still exists,
                // where line 1 *is* the right place to stop. Only the file
                // status tells the two apart, so the ambiguity is resolved
                // here rather than in the hunk parser.
                if f.status == ChangeStatus::Deleted {
                    f.hunks.clear();
                }
                if files.len() < MAX_DIFF_FILES {
                    files.push(f);
                } else {
                    truncated = true;
                }
            }
        };
    }

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush!();
            rename_from = None;
            let (a, b) = split_diff_header(rest);
            // The header path is provisional: `--- /dev/null` and
            // `+++ b/...` below correct it for adds and deletes. It is the
            // only path a binary file ever gets, though, so it is set here.
            cur = Some(FileChange {
                path: b.or(a).unwrap_or_default(),
                old_path: None,
                status: ChangeStatus::Modified,
                added: 0,
                removed: 0,
                hunks: Vec::new(),
                binary: false,
            });
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };

        if line.starts_with("new file mode") {
            f.status = ChangeStatus::Added;
        } else if line.starts_with("deleted file mode") {
            f.status = ChangeStatus::Deleted;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            rename_from = Some(from.to_string());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            f.status = ChangeStatus::Renamed;
            f.old_path = rename_from.take();
            f.path = to.to_string();
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            f.binary = true;
        } else if let Some(p) = line.strip_prefix("--- ") {
            if p != "/dev/null" {
                if let Some(stripped) = strip_prefix_marker(p) {
                    // Only informative for a delete — otherwise `+++` wins.
                    if f.status == ChangeStatus::Deleted {
                        f.path = stripped;
                    }
                }
            }
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if p != "/dev/null" {
                if let Some(stripped) = strip_prefix_marker(p) {
                    f.path = stripped;
                }
            }
        } else if line.starts_with("@@") {
            if let Some(range) = parse_hunk_header(line) {
                f.hunks.push(Hunk {
                    range,
                    added: 0,
                    removed: 0,
                });
            }
        } else if let Some(rest) = line.strip_prefix('+') {
            // `+++` is handled above, so anything left starting with `+`
            // is content. (With -U0 there are no context lines at all.)
            let _ = rest;
            f.added += 1;
            if let Some(h) = f.hunks.last_mut() {
                h.added += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            f.removed += 1;
            if let Some(h) = f.hunks.last_mut() {
                h.removed += 1;
            }
        }
    }
    flush!();
    (files, truncated)
}

/// `a/src/x.rs b/src/x.rs` → the two paths.
///
/// Paths containing a space make this genuinely ambiguous — git's own
/// answer is to quote them, which `strip_quotes` undoes. For the unquoted
/// case the `a/`…` b/` split is the best available guess, and it is what
/// git's own tooling does.
fn split_diff_header(rest: &str) -> (Option<String>, Option<String>) {
    if let Some(at) = rest.find(" b/") {
        let a = strip_prefix_marker(&rest[..at]);
        let b = strip_prefix_marker(&rest[at + 1..]);
        return (a, b);
    }
    let mut it = rest.split_whitespace();
    (
        it.next().and_then(strip_prefix_marker),
        it.next().and_then(strip_prefix_marker),
    )
}

/// Drop git's `a/` / `b/` diff prefix and any quoting.
fn strip_prefix_marker(p: &str) -> Option<String> {
    let p = strip_quotes(p.trim());
    if p == "/dev/null" {
        return None;
    }
    let out = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(&p)
        .to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Undo git's C-style quoting of exotic paths. `core.quotePath=false`
/// already covers non-ASCII, so what is left is control characters,
/// quotes and backslashes.
fn strip_quotes(p: &str) -> String {
    if !(p.starts_with('"') && p.ends_with('"') && p.len() >= 2) {
        return p.to_string();
    }
    let inner = &p[1..p.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// `@@ -12,3 +12,5 @@ fn foo()` → the new-side range, 1-based inclusive.
///
/// A zero-length new side (`+12,0`) is a pure deletion: nothing to visit at
/// line 12, but the code that used to be there was inside something, and
/// that something is what the walk should stop at. Collapsing it to a
/// single line at the deletion point is what makes a delete-only hunk map
/// to the enclosing function instead of to nothing.
fn parse_hunk_header(line: &str) -> Option<LineRange> {
    let body = line.strip_prefix("@@")?;
    let end = body.find("@@")?;
    let plus = body[..end].split('+').nth(1)?.trim();
    let mut parts = plus.split(',');
    let start: u32 = parts.next()?.trim().parse().ok()?;
    let count: u32 = match parts.next() {
        Some(c) => c.trim().parse().ok()?,
        None => 1,
    };
    if count == 0 {
        let at = start.max(1);
        return Some(LineRange { start: at, end: at });
    }
    Some(LineRange {
        start: start.max(1),
        end: start.max(1) + count - 1,
    })
}

// ── line-number drift ──────────────────────────────────────────────────────

/// Which of the working tree's files differ from `tip`.
///
/// A diff's line numbers describe the tree at its *new* side, while the
/// graph describes the tree on disk. For uncommitted work those are the
/// same tree and the mapping is exact; for an older commit they are not,
/// and a hunk at line 606 can land inside whatever occupies line 606
/// today. That failure is silent and looks exactly like a correct answer,
/// which is the one kind this codebase refuses to ship — so the walk
/// checks, and says so.
///
/// One `git diff --name-only` for the whole set rather than a blob-hash
/// probe per file: the same answer, one process spawn instead of N.
pub fn drifted_since(dir: &Path, tip: &str) -> Result<HashSet<String>, GitError> {
    let root = repo_root(dir)?;
    let out = git(&root, &["diff", "--name-only", tip, "--"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| strip_quotes(l))
        .collect())
}

/// The revision a diff's *new* side describes, or `None` when that side is
/// the working tree (where nothing can have drifted).
pub fn new_side_rev(spec: &RevSpec, diff: &DiffSummary) -> Option<String> {
    match spec {
        // The new side is the working tree / the index, which is what the
        // graph was built from.
        RevSpec::Working | RevSpec::Staged => None,
        RevSpec::Commit(_) => diff.commits.first().map(|c| c.sha.clone()),
        // `a..b` and `a...b` both end at `b`.
        RevSpec::Range(r) => {
            let tip = r.rsplit("..").next().unwrap_or("").trim();
            if tip.is_empty() {
                // `a..` means `a..HEAD`.
                Some("HEAD".to_string())
            } else {
                Some(tip.to_string())
            }
        }
    }
}

// ── path rebasing ──────────────────────────────────────────────────────────

/// Re-express git's repo-root-relative paths against the root the *index*
/// was built from.
///
/// These are frequently not the same directory: this very repository is
/// indexed from `native/` while git's root is a level above it, so every
/// path git reports would miss every node in the graph. Paths outside the
/// project root are dropped — they changed, but this index has nothing to
/// say about them.
pub fn rebase_paths(diff: &mut DiffSummary, git_root: &Path, project_root: &Path) {
    let git_root = std::fs::canonicalize(git_root).unwrap_or_else(|_| git_root.to_path_buf());
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let Ok(prefix) = project_root.strip_prefix(&git_root) else {
        // The project root is not inside the git root (or is the git root,
        // where `strip_prefix` yields an empty prefix and the loop below is
        // a no-op). Either way there is nothing to rebase.
        return;
    };
    let prefix = prefix.to_string_lossy().replace('\\', "/");
    if prefix.is_empty() {
        return;
    }
    let prefix = format!("{}/", prefix.trim_end_matches('/'));
    diff.files.retain_mut(|f| {
        let Some(rel) = f.path.strip_prefix(&prefix) else {
            return false;
        };
        let rel = rel.to_string();
        f.old_path = f
            .old_path
            .as_deref()
            .and_then(|p| p.strip_prefix(&prefix))
            .map(str::to_string);
        f.path = rel;
        true
    });
    diff.insertions = diff.files.iter().map(|f| f.added).sum();
    diff.deletions = diff.files.iter().map(|f| f.removed).sum();
}

/// Index a diff's files by path, for the hunk→symbol join.
pub fn by_path(diff: &DiffSummary) -> HashMap<&str, &FileChange> {
    diff.files.iter().map(|f| (f.path.as_str(), f)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
diff --git a/src/auth.rs b/src/auth.rs
index 1111111..2222222 100644
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,2 +10,4 @@ fn login()
-old one
-old two
+new one
+new two
+new three
+new four
@@ -40,0 +42,1 @@ fn logout()
+added line
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,3 @@
+a
+b
+c
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
index 4444444..0000000
--- a/src/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-x
-y
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 98%
rename from src/old_name.rs
rename to src/new_name.rs
index 5555555..6666666 100644
diff --git a/logo.png b/logo.png
index 7777777..8888888 100644
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn parses_every_file_status() {
        let (files, truncated) = parse_patch(PATCH);
        assert!(!truncated);
        assert_eq!(files.len(), 5);
        assert_eq!(files[0].path, "src/auth.rs");
        assert_eq!(files[0].status, ChangeStatus::Modified);
        assert_eq!(files[1].status, ChangeStatus::Added);
        assert_eq!(files[2].status, ChangeStatus::Deleted);
        assert_eq!(files[2].path, "src/gone.rs");
        assert_eq!(files[3].status, ChangeStatus::Renamed);
        assert_eq!(files[3].path, "src/new_name.rs");
        assert_eq!(files[3].old_path.as_deref(), Some("src/old_name.rs"));
        assert!(files[4].binary);
    }

    #[test]
    fn hunk_ranges_are_new_side_and_inclusive() {
        let (files, _) = parse_patch(PATCH);
        assert_eq!(
            files[0].hunks,
            vec![
                Hunk { range: LineRange { start: 10, end: 13 }, added: 4, removed: 2 },
                Hunk { range: LineRange { start: 42, end: 42 }, added: 1, removed: 0 },
            ]
        );
        // A deleted file has no new side to visit.
        assert!(files[2].hunks.is_empty());
    }

    #[test]
    fn counts_added_and_removed_lines() {
        let (files, _) = parse_patch(PATCH);
        assert_eq!((files[0].added, files[0].removed), (5, 2));
        assert_eq!((files[1].added, files[1].removed), (3, 0));
        assert_eq!((files[2].added, files[2].removed), (0, 2));
    }

    #[test]
    fn zero_length_new_side_collapses_to_the_deletion_point() {
        // A hunk that only removes lines: nothing to show at the new-side
        // position, but the enclosing symbol is still what changed.
        let r = parse_hunk_header("@@ -12,4 +11,0 @@ fn f()").expect("parse");
        assert_eq!(r, LineRange { start: 11, end: 11 });
    }

    #[test]
    fn hunk_header_without_a_count_means_one_line() {
        let r = parse_hunk_header("@@ -1 +1 @@").expect("parse");
        assert_eq!(r, LineRange { start: 1, end: 1 });
    }

    #[test]
    fn overlap_is_inclusive_at_both_ends() {
        let h = LineRange { start: 10, end: 20 };
        assert!(h.overlaps(20, 30), "touching at the top edge overlaps");
        assert!(h.overlaps(1, 10), "touching at the bottom edge overlaps");
        assert!(h.overlaps(12, 13), "fully contained overlaps");
        assert!(!h.overlaps(21, 30));
        assert!(!h.overlaps(1, 9));
    }

    #[test]
    fn rev_specs_round_trip() {
        for (raw, want) in [
            ("", RevSpec::Working),
            ("  ", RevSpec::Working),
            ("working", RevSpec::Working),
            ("STAGED", RevSpec::Staged),
            ("HEAD~3..HEAD", RevSpec::Range("HEAD~3..HEAD".into())),
            ("main...feature", RevSpec::Range("main...feature".into())),
            ("a1b2c3d", RevSpec::Commit("a1b2c3d".into())),
        ] {
            let spec = RevSpec::parse(raw);
            assert_eq!(spec, want, "parsing {raw:?}");
            assert_eq!(RevSpec::parse(&spec.as_arg()), want, "round-tripping {raw:?}");
        }
    }

    #[test]
    fn parses_the_commit_log_stream() {
        let log = format!(
            "{RS}abc123{FS}abc123d{FS}Fix the thing{FS}Ada{FS}2 hours ago{FS}2026-09-13T10:00:00+00:00\n \
             3 files changed, 42 insertions(+), 7 deletions(-)\n\
             {RS}def456{FS}def456a{FS}Merge branch 'x'{FS}Bob{FS}3 days ago{FS}2026-09-10T10:00:00+00:00\n"
        );
        let commits = parse_commit_log(&log);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "Fix the thing");
        assert_eq!(
            (commits[0].files, commits[0].insertions, commits[0].deletions),
            (3, 42, 7)
        );
        // A merge with no shortstat line reports zeros rather than
        // inheriting the previous record's numbers.
        assert_eq!(
            (commits[1].files, commits[1].insertions, commits[1].deletions),
            (0, 0, 0)
        );
    }

    #[test]
    fn shortstat_clauses_are_each_optional() {
        assert_eq!(parse_shortstat(" 1 file changed, 5 insertions(+)"), (1, 5, 0));
        assert_eq!(parse_shortstat(" 2 files changed, 3 deletions(-)"), (2, 0, 3));
    }

    #[test]
    fn rebases_paths_onto_the_indexed_root() {
        let mut d = DiffSummary {
            spec: "working".into(),
            label: "uncommitted changes".into(),
            files: vec![
                FileChange {
                    path: "native/src/a.rs".into(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                    added: 2,
                    removed: 1,
                    hunks: vec![Hunk {
                        range: LineRange { start: 1, end: 2 },
                        added: 2,
                        removed: 1,
                    }],
                    binary: false,
                },
                FileChange {
                    path: "docs/README.md".into(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                    added: 9,
                    removed: 9,
                    hunks: vec![],
                    binary: false,
                },
            ],
            insertions: 11,
            deletions: 10,
            commits: vec![],
            truncated: false,
        };
        let tmp = std::env::temp_dir().join("ug-git-rebase-test");
        let project = tmp.join("native");
        std::fs::create_dir_all(&project).expect("mkdir");
        rebase_paths(&mut d, &tmp, &project);
        // The file inside the indexed root is kept, rebased; the one
        // outside it is dropped rather than reported as an unmatched path.
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].path, "src/a.rs");
        assert_eq!((d.insertions, d.deletions), (2, 1));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_runaway_commit_subject_is_truncated() {
        assert_eq!(short_subject("short one"), "short one");
        let long = short_subject(&"x".repeat(200));
        assert!(long.ends_with('…'));
        assert_eq!(long.chars().count(), MAX_SUBJECT_CHARS);
    }

    #[test]
    fn the_short_label_names_the_change_without_its_subject() {
        let commit = Commit {
            sha: "a".repeat(40),
            short: "a1b2c3d".into(),
            subject: "an extremely long subject that would swallow a sentence".into(),
            author: "x".into(),
            relative: "now".into(),
            date: "d".into(),
            files: 1,
            insertions: 1,
            deletions: 0,
        };
        let mut d = DiffSummary {
            spec: "HEAD".into(),
            label: "commit a1b2c3d — an extremely long subject…".into(),
            files: vec![],
            insertions: 0,
            deletions: 0,
            commits: vec![commit.clone()],
            truncated: false,
        };
        assert_eq!(d.short_label(), "commit a1b2c3d");
        // A range is named by the range, not by its newest commit.
        d.spec = "main..HEAD".into();
        d.commits = vec![commit.clone(), commit];
        assert_eq!(d.short_label(), "`main..HEAD`");
        // Uncommitted work has no commits and is already short.
        d.commits = vec![];
        d.label = "uncommitted changes".into();
        assert_eq!(d.short_label(), "uncommitted changes");
    }

    #[test]
    fn the_new_side_of_a_range_is_its_tip() {
        let empty = DiffSummary {
            spec: String::new(),
            label: String::new(),
            files: vec![],
            insertions: 0,
            deletions: 0,
            commits: vec![],
            truncated: false,
        };
        let rev = |r: &str| new_side_rev(&RevSpec::parse(r), &empty);
        assert_eq!(rev("main..HEAD").as_deref(), Some("HEAD"));
        assert_eq!(rev("main...feature").as_deref(), Some("feature"));
        // `a..` is git's spelling of `a..HEAD`.
        assert_eq!(rev("abc123..").as_deref(), Some("HEAD"));
        // Uncommitted work is already the tree the graph describes.
        assert_eq!(rev("working"), None);
        assert_eq!(rev("staged"), None);
    }

    #[test]
    fn errors_carry_a_code_and_a_hint() {
        for e in [
            GitError::NotInstalled,
            GitError::NotARepo(PathBuf::from("/tmp/x")),
            GitError::NoCommits,
            GitError::BadRev { rev: "nope".into(), detail: String::new() },
            GitError::Failed { what: "diff".into(), detail: "boom".into() },
        ] {
            assert!(!e.code().is_empty());
            assert!(!e.hint().is_empty(), "{e} has no hint");
            assert!(!e.to_string().is_empty());
        }
    }
}
