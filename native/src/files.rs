//! The files a project's index holds, and where each stands — behind
//! `ug files`, the MCP `files` tool and `POST /api/tools/files`.
//!
//! The question every consumer of a project eventually asks — "which files
//! are in here, and are they current?" — had no answer but reading
//! `project.json`, which made ug's storage layout an interface. This is the
//! interface instead, one implementation for all three transports.
//!
//! Cheap by construction: the list comes from `project.json` (what `ug list`
//! reads) and each file costs one `stat`, the same per-file check
//! [`project::staleness`] counts with — so `ug list` saying "2 changed" and
//! `files` with `status: changed` listing two files can never disagree. No
//! `graph.json` parse, no store open.
//!
//! Paging follows `ug analyze`: a rendered answer is a window (50 rows by
//! default, capped at 200) that names the next one; the JSON envelope is
//! every match unless a window is asked for, because a program reading it
//! wants the list, not a page of it.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::analyze::range::{self, RowRange};
use crate::pattern::{Mode, Pattern};
use crate::project::{self, FileState, ProjectMeta};
use crate::{C_BOLD, C_CYAN, C_DIM, C_GREEN, C_RED, C_RESET, C_YELLOW};

/// Rows a rendered answer shows when no window is given — `ug analyze`'s default.
pub(crate) const DEFAULT_ROWS: usize = 50;

/// One indexed file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FileRow {
    /// Repo-relative, `/`-separated — the spelling the graph uses.
    pub path: String,
    /// Lower-case extension without the dot; empty when there is none.
    pub ext: String,
    /// The indexer's language name (`typescript`, `markdown`, `pdf`, …).
    pub language: String,
    /// `docs` (Markdown, PDF) or `code`.
    pub kind: &'static str,
    pub bytes: u64,
    /// mtime, epoch seconds; 0 when the file is gone.
    pub modified: u64,
    pub state: FileState,
}

fn ext_of(path: &str) -> String {
    Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// The same language names the indexer stamps on `FileNode.language`.
fn language_of(ext: &str) -> String {
    match crate::indexer::languages::for_extension(ext) {
        Some(l) => l.name().to_string(),
        None if ext == "pdf" => "pdf".to_string(),
        None => ext.to_string(),
    }
}

fn kind_of(language: &str) -> &'static str {
    if matches!(language, "markdown" | "pdf") { "docs" } else { "code" }
}

/// Stat every indexed file of a project. One `stat` each, like `ug list`.
pub(crate) fn rows(dir: &Path, meta: &ProjectMeta) -> Vec<FileRow> {
    let (files, _, _) = project::indexed_file_list(dir, meta);
    let built_at = project::index_built_at(dir);
    let root = Path::new(&meta.repo_root);
    files
        .into_iter()
        .map(|path| {
            let check = project::check_file(root, &path, built_at);
            let ext = ext_of(&path);
            let language = language_of(&ext);
            FileRow {
                kind: kind_of(&language),
                path,
                ext,
                language,
                bytes: check.bytes,
                modified: check.modified,
                state: check.state,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortBy {
    /// Largest first.
    Size,
    /// Most recently modified first.
    Modified,
}

/// What to list: filters (all must hold; several patterns are alternatives;
/// an empty set means "any"), an order, and a window.
#[derive(Debug, Default)]
pub(crate) struct FileQuery {
    patterns: Vec<Pattern>,
    exts: Vec<String>,
    langs: Vec<String>,
    kind: Option<&'static str>,
    states: Vec<FileState>,
    /// `None` sorts by path.
    sort: Option<SortBy>,
    /// `None`: every match (JSON) or the first [`DEFAULT_ROWS`] (rendered).
    pub window: Option<RowRange>,
}

/// Values given as `ts,md` or `[".ts", "md"]`: split on commas, trimmed,
/// lower-cased, the leading dot dropped.
fn listed<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    values
        .iter()
        .flat_map(|v| v.as_ref().split(','))
        .map(|v| v.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .collect()
}

/// A query as a transport received it, before validation: CLI flags or JSON
/// fields, split into values but not yet understood.
#[derive(Debug, Default)]
pub(crate) struct RawQuery {
    pub patterns: Vec<String>,
    pub exts: Vec<String>,
    pub langs: Vec<String>,
    pub kind: Option<String>,
    pub states: Vec<String>,
    pub sort: Option<String>,
    pub limit: Option<String>,
    pub range: Option<String>,
}

impl FileQuery {
    /// Validate a raw query. Every transport goes through here, so a value
    /// one rejects, all reject, with the same message.
    pub(crate) fn new(raw: &RawQuery) -> Result<FileQuery, String> {
        let mut q = FileQuery::default();
        for p in &raw.patterns {
            q.patterns.push(Pattern::new(p, Mode::Path).map_err(|e| format!("bad pattern {p:?}: {e}"))?);
        }
        q.exts = listed(&raw.exts);
        q.langs = listed(&raw.langs);
        q.kind = match raw.kind.as_deref().map(|k| k.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => None,
            Some("code") => Some("code"),
            Some("docs" | "doc" | "documents") => Some("docs"),
            Some(other) => return Err(format!("kind takes code or docs, not {other:?}")),
        };
        for s in listed(&raw.states) {
            q.states.push(match s.as_str() {
                "fresh" => FileState::Fresh,
                "changed" => FileState::Changed,
                "missing" | "deleted" => FileState::Missing,
                other => return Err(format!("status takes fresh, changed or missing, not {other:?}")),
            });
        }
        q.sort = match raw.sort.as_deref().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "path" | "name") => None,
            Some("size") => Some(SortBy::Size),
            Some("modified" | "mtime" | "time") => Some(SortBy::Modified),
            Some(other) => return Err(format!("sort takes path, size or modified, not {other:?}")),
        };
        q.window = match (raw.range.as_deref().map(str::trim).filter(|r| !r.is_empty()), raw.limit.as_deref()) {
            (Some(raw), _) => Some(range::parse(raw).ok_or_else(|| {
                format!(
                    "Could not read {raw:?} as a row range. Use a count (`20`), a closed window \
                     (`11-35`), or an open one (`34-end`). Rows are 1-based and inclusive."
                )
            })?),
            (None, Some(k)) => match k.trim().parse::<usize>() {
                Ok(n) if n > 0 => Some(RowRange::first(n)),
                _ => return Err(format!("limit takes a positive number, not {k:?}")),
            },
            (None, None) => None,
        };
        Ok(q)
    }

    /// From a JSON body — the MCP tool's arguments or `POST /api/tools/files`.
    /// `pattern`, `ext`, `lang` and `status` take a string or an array.
    pub(crate) fn from_json(args: &Value) -> Result<FileQuery, String> {
        let many = |key: &str| -> Vec<String> {
            match args.get(key) {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                _ => Vec::new(),
            }
        };
        let one = |key: &str| -> Option<String> {
            match args.get(key) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            }
        };
        FileQuery::new(&RawQuery {
            patterns: many("pattern"),
            exts: many("ext"),
            langs: many("lang"),
            kind: one("kind"),
            states: many("status"),
            sort: one("sort"),
            limit: one("limit"),
            range: one("range"),
        })
    }

    /// A pattern without a `/` matches the file's name as well as its whole
    /// path — `'*.ts'` finds `src/a.ts`, the way `.gitignore` reads it. With a
    /// `/` it is a path pattern (`'src/**/*.rs'`).
    fn keeps(&self, row: &FileRow) -> bool {
        let name = row.path.rsplit('/').next().unwrap_or(&row.path);
        let pattern_ok = self.patterns.is_empty()
            || self.patterns.iter().any(|p| p.matches(&row.path) || (!p.as_str().contains('/') && p.matches(name)));
        pattern_ok
            && (self.exts.is_empty() || self.exts.contains(&row.ext))
            && (self.langs.is_empty() || self.langs.contains(&row.language))
            && self.kind.is_none_or(|k| k == row.kind)
            && (self.states.is_empty() || self.states.contains(&row.state))
    }

    pub(crate) fn apply(&self, mut rows: Vec<FileRow>) -> Vec<FileRow> {
        rows.retain(|r| self.keeps(r));
        match self.sort {
            None => rows.sort_by(|a, b| a.path.cmp(&b.path)),
            Some(SortBy::Size) => rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path))),
            Some(SortBy::Modified) => rows.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path))),
        }
        rows
    }
}

/// What a `files` call found: every match, and which slice of them to show.
#[derive(Debug)]
pub(crate) struct FilesAnswer {
    pub project: String,
    pub repo_root: String,
    pub built_at: Option<u64>,
    pub repo_missing: bool,
    /// Files in the index, before filtering.
    pub indexed: usize,
    pub matched: Vec<FileRow>,
    /// 0-based, half-open.
    pub from: usize,
    pub to: usize,
    /// The window asked for more than the 200-row cap (rendered answers only).
    pub capped: bool,
}

/// Run `query` against a project. `rendered` picks the window rules: a
/// rendered answer pages (default 50, at most 200 a window); JSON returns
/// every match unless a window was asked for, and honours it uncapped.
pub(crate) fn run(dir: &Path, meta: &ProjectMeta, query: &FileQuery, rendered: bool) -> FilesAnswer {
    let all = rows(dir, meta);
    let indexed = all.len();
    let matched = query.apply(all);
    let total = matched.len();
    let window = if rendered { Some(query.window.unwrap_or(RowRange::first(DEFAULT_ROWS))) } else { query.window };
    let (from, to) = match window {
        None => (0, total),
        Some(w) if rendered => w.slice(total),
        Some(w) => {
            let start = w.start.saturating_sub(1).min(total);
            (start, w.end.map(|e| e.min(total)).unwrap_or(total).max(start))
        }
    };
    FilesAnswer {
        project: meta.name.clone(),
        repo_root: meta.repo_root.clone(),
        built_at: project::index_built_at(dir),
        repo_missing: !meta.repo_root.is_empty() && !Path::new(&meta.repo_root).exists(),
        indexed,
        capped: rendered && window.is_some_and(|w| w.is_capped(total)),
        matched,
        from,
        to,
    }
}

/// The project a transport named, or why it can't be listed. A project
/// directory with no `project.json` was never generated.
pub(crate) fn project_at(dir: &Path) -> Result<(PathBuf, ProjectMeta), String> {
    project::read_meta(dir)
        .map(|meta| (dir.to_path_buf(), meta))
        .ok_or_else(|| format!("{} holds no generated project — run `ug gen` first.", dir.display()))
}

fn counts(rows: &[FileRow]) -> (usize, usize, usize) {
    rows.iter().fold((0, 0, 0), |(f, c, m), r| match r.state {
        FileState::Fresh => (f + 1, c, m),
        FileState::Changed => (f, c + 1, m),
        FileState::Missing => (f, c, m + 1),
    })
}

/// The JSON envelope, the same over `ug files --json` and `POST
/// /api/tools/files`: what matched, the window shown, and per-status counts
/// over every match (not just the window), so a caller can report "3
/// changed" without fetching all the rows.
pub(crate) fn to_json(a: &FilesAnswer) -> Value {
    let (fresh, changed, missing) = counts(&a.matched);
    json!({
        "project": a.project,
        "repoRoot": a.repo_root,
        "builtAt": a.built_at,
        "repoMissing": a.repo_missing,
        "indexed": a.indexed,
        "total": a.matched.len(),
        "from": if a.to > a.from { a.from + 1 } else { a.from },
        "to": a.to,
        "counts": { "fresh": fresh, "changed": changed, "missing": missing },
        "files": a.matched[a.from..a.to].iter().map(|r| json!({
            "path": r.path,
            "ext": r.ext,
            "language": r.language,
            "kind": r.kind,
            "bytes": r.bytes,
            "modified": r.modified,
            "status": r.state.as_str(),
        })).collect::<Vec<_>>(),
    })
}

/// The table. `next` words the call for the window after this one, in the
/// caller's own terms (a CLI command, an MCP argument); `color` is off for
/// anything but a terminal.
pub(crate) fn render(a: &FilesAnswer, color: bool, next: &dyn Fn(usize, usize) -> String) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let (bold, cyan, dim, green, red, reset, yellow) = (c(C_BOLD), c(C_CYAN), c(C_DIM), c(C_GREEN), c(C_RED), c(C_RESET), c(C_YELLOW));
    let mut out = String::new();
    if a.repo_missing {
        out.push_str(&format!(
            "{yellow}⚠ the indexed folder {} is gone — every file reads as missing.{reset}\n",
            a.repo_root
        ));
    }
    if a.indexed == 0 {
        out.push_str(&format!(
            "{} has no indexed files. Run {cyan}ug gen -n {}{reset} to index its folder.\n",
            a.project, a.project
        ));
        return out;
    }
    let total = a.matched.len();
    if total == 0 {
        out.push_str(&format!("No files match — the index holds {} files.\n", a.indexed));
        return out;
    }
    if a.from >= a.to {
        out.push_str(&format!(
            "Nothing in that window — {} file{} matched. Ask for range 1-{}.\n",
            total,
            if total == 1 { "" } else { "s" },
            total.min(DEFAULT_ROWS)
        ));
        return out;
    }

    let page = &a.matched[a.from..a.to];
    let width = page.iter().map(|r| r.path.chars().count()).max().unwrap_or(4).clamp(4, 72);
    let lang_w = page.iter().map(|r| r.language.len()).max().unwrap_or(4).max(4);
    out.push_str(&format!(
        "{bold}{:<width$}  {:<lang_w$}  {:<4}  {:>8}  {:<19}  STATUS{reset}\n",
        "PATH", "LANG", "KIND", "SIZE", "MODIFIED"
    ));
    for r in page {
        let (status, tint) = match r.state {
            FileState::Fresh => ("fresh", green),
            FileState::Changed => ("changed", yellow),
            FileState::Missing => ("missing", red),
        };
        let path = if r.path.chars().count() > width {
            let tail: String = r.path.chars().rev().take(width - 1).collect::<Vec<_>>().into_iter().rev().collect();
            format!("…{tail}")
        } else {
            r.path.clone()
        };
        let size = if r.state == FileState::Missing { "-".to_string() } else { format_bytes(r.bytes) };
        out.push_str(&format!(
            "{:<width$}  {:<lang_w$}  {:<4}  {:>8}  {:<19}  {tint}{status}{reset}\n",
            path,
            r.language,
            r.kind,
            size,
            project::format_epoch(r.modified),
        ));
    }

    let (fresh, changed, missing) = counts(&a.matched);
    let mut parts = vec![format!("files {}–{} of {}", a.from + 1, a.to, total)];
    if total < a.indexed {
        parts.push(format!("{} indexed", a.indexed));
    }
    parts.push(format!("{fresh} fresh"));
    if changed > 0 {
        parts.push(format!("{changed} changed"));
    }
    if missing > 0 {
        parts.push(format!("{missing} missing"));
    }
    out.push_str(&format!("\n{dim}{}{reset}\n", parts.join(" · ")));
    if a.to < total {
        let cap = if a.capped { " (window capped at 200 rows)" } else { "" };
        out.push_str(&format!("{dim}next: {}{cap}{reset}\n", next(a.to + 1, (a.to + DEFAULT_ROWS).min(total))));
    }
    if changed + missing > 0 {
        out.push_str(&format!("{dim}refresh: {reset}{cyan}ug gen -n {}{reset}\n", a.project));
    }
    out
}

/// Bytes at human scale, as `ug list` prints them.
fn format_bytes(bytes: u64) -> String {
    crate::cli::projects::format_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn row(path: &str, bytes: u64, modified: u64, state: FileState) -> FileRow {
        let ext = ext_of(path);
        let language = language_of(&ext);
        FileRow { kind: kind_of(&language), path: path.into(), ext, language, bytes, modified, state }
    }

    fn sample() -> Vec<FileRow> {
        vec![
            row("src/fare.ts", 300, 30, FileState::Fresh),
            row("src/refund.tsx", 900, 10, FileState::Changed),
            row("docs/policy.md", 100, 20, FileState::Fresh),
            row("docs/manual.pdf", 5000, 5, FileState::Missing),
            row("lib/util.py", 50, 40, FileState::Fresh),
            row("README.md", 10, 1, FileState::Fresh),
        ]
    }

    fn select(args: Value) -> Vec<String> {
        FileQuery::from_json(&args).unwrap().apply(sample()).into_iter().map(|r| r.path).collect()
    }

    #[test]
    fn files_carry_the_indexers_language_and_kind() {
        let r = sample();
        assert_eq!((r[0].language.as_str(), r[0].kind), ("typescript", "code"));
        assert_eq!((r[1].language.as_str(), r[1].ext.as_str()), ("typescript", "tsx"));
        assert_eq!((r[2].language.as_str(), r[2].kind), ("markdown", "docs"));
        assert_eq!((r[3].language.as_str(), r[3].kind), ("pdf", "docs"));
        assert_eq!(r[4].language, "python");
    }

    #[test]
    fn a_pattern_without_a_slash_matches_file_names_anywhere() {
        assert_eq!(select(json!({ "pattern": "*.md" })), ["README.md", "docs/policy.md"]);
        assert_eq!(select(json!({ "pattern": "*{fare,refund}*" })), ["src/fare.ts", "src/refund.tsx"]);
        // Case-insensitive, whole-name: a literal is an exact name, not a substring.
        assert_eq!(select(json!({ "pattern": "readme.MD" })), ["README.md"]);
        assert!(select(json!({ "pattern": "fare" })).is_empty());
    }

    #[test]
    fn a_pattern_with_a_slash_matches_the_path_and_several_are_alternatives() {
        assert_eq!(select(json!({ "pattern": "src/*" })), ["src/fare.ts", "src/refund.tsx"]);
        assert_eq!(select(json!({ "pattern": "**/*.py" })), ["lib/util.py"]);
        assert!(select(json!({ "pattern": "docs/*/x.md" })).is_empty(), "* stops at /");
        assert_eq!(select(json!({ "pattern": ["*.py", "README.md"] })), ["README.md", "lib/util.py"]);
    }

    #[test]
    fn extension_language_kind_and_status_narrow_together() {
        assert_eq!(select(json!({ "ext": "ts,TSX" })), ["src/fare.ts", "src/refund.tsx"]);
        assert_eq!(select(json!({ "ext": [".md", "py"] })), ["README.md", "docs/policy.md", "lib/util.py"]);
        assert_eq!(select(json!({ "lang": "pdf" })), ["docs/manual.pdf"]);
        assert_eq!(select(json!({ "kind": "docs" })), ["README.md", "docs/manual.pdf", "docs/policy.md"]);
        assert_eq!(select(json!({ "status": ["changed", "missing"] })), ["docs/manual.pdf", "src/refund.tsx"]);
        assert_eq!(select(json!({ "kind": "code", "status": "fresh", "pattern": "src/**" })), ["src/fare.ts"]);
    }

    #[test]
    fn sorts_by_path_size_or_modified() {
        assert_eq!(select(json!({}))[0], "README.md");
        assert_eq!(select(json!({ "sort": "size" }))[..2], ["docs/manual.pdf", "src/refund.tsx"]);
        assert_eq!(select(json!({ "sort": "modified" }))[..2], ["lib/util.py", "src/fare.ts"]);
    }

    #[test]
    fn bad_values_are_errors_not_empty_results() {
        // (An unclosed `[` is not one: the pattern dialect reads it as a literal.)
        for bad in [
            json!({ "kind": "binary" }),
            json!({ "status": "stale" }),
            json!({ "sort": "random" }),
            json!({ "limit": 0 }),
            json!({ "range": "banana" }),
        ] {
            assert!(FileQuery::from_json(&bad).is_err(), "{bad}");
        }
        // A stringified number is what models send; it is a number.
        assert_eq!(FileQuery::from_json(&json!({ "limit": "5" })).unwrap().window, Some(RowRange::first(5)));
        assert_eq!(FileQuery::from_json(&json!({ "limit": 5, "range": "11-20" })).unwrap().window, range::parse("11-20"), "range wins");
    }

    fn answer(rows: Vec<FileRow>, indexed: usize, rendered: bool, window: Option<RowRange>) -> FilesAnswer {
        let total = rows.len();
        let w = if rendered { Some(window.unwrap_or(RowRange::first(DEFAULT_ROWS))) } else { window };
        let (from, to) = match w {
            None => (0, total),
            Some(w) => w.slice(total),
        };
        FilesAnswer {
            project: "tidewater".into(),
            repo_root: "/r".into(),
            built_at: None,
            repo_missing: false,
            indexed,
            matched: rows,
            from,
            to,
            capped: false,
        }
    }

    #[test]
    fn a_rendered_answer_says_where_it_is_and_how_to_get_the_next_page() {
        let rows: Vec<FileRow> = (0..120).map(|i| row(&format!("src/f{i:03}.ts"), 10, 1, FileState::Fresh)).collect();
        let text = render(&answer(rows.clone(), 130, true, None), false, &|a, b| format!("files --range {a}-{b}"));
        assert!(text.contains("src/f049.ts") && !text.contains("src/f050.ts"), "{text}");
        assert!(text.contains("files 1–50 of 120 · 130 indexed · 120 fresh"), "{text}");
        assert!(text.contains("next: files --range 51-100"), "{text}");
        assert!(!text.contains('\x1b'), "no colour when asked for none");
        let last = render(&answer(rows, 120, true, range::parse("101-end")), false, &|_, _| unreachable!());
        assert!(last.contains("files 101–120 of 120") && !last.contains("next:"), "{last}");
    }

    #[test]
    fn a_rendered_answer_names_drift_and_how_to_refresh_it() {
        let text = render(&answer(sample(), 6, true, None), false, &|_, _| String::new());
        assert!(text.contains("1 changed · 1 missing"), "{text}");
        assert!(text.contains("refresh: ug gen -n tidewater"), "{text}");
        assert!(render(&answer(vec![], 6, true, None), false, &|_, _| String::new()).contains("No files match — the index holds 6 files."));
        assert!(render(&answer(vec![], 0, true, None), false, &|_, _| String::new()).contains("has no indexed files"));
    }

    #[test]
    fn json_counts_every_match_and_lists_the_window() {
        let a = answer(sample(), 9, false, range::parse("2-3"));
        let v = to_json(&a);
        assert_eq!((v["indexed"].as_u64(), v["total"].as_u64(), v["from"].as_u64(), v["to"].as_u64()), (Some(9), Some(6), Some(2), Some(3)));
        assert_eq!(v["counts"], json!({ "fresh": 4, "changed": 1, "missing": 1 }));
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], json!({
            "path": "src/refund.tsx", "ext": "tsx", "language": "typescript", "kind": "code",
            "bytes": 900, "modified": 10, "status": "changed",
        }));
    }

    #[test]
    fn json_returns_everything_unless_asked_and_ignores_the_table_cap() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut m = ProjectMeta::new("p", &repo.path().to_string_lossy(), 1, 1);
        m.files = (0..300).map(|i| format!("f{i:03}.md")).collect();
        m.doc_nodes = 1;
        let all = run(data.path(), &m, &FileQuery::default(), false);
        assert_eq!((all.from, all.to, all.capped), (0, 300, false));
        let q = FileQuery::from_json(&json!({ "range": "1-end" })).unwrap();
        assert_eq!(run(data.path(), &m, &q, false).to, 300, "JSON honours 1-end");
        let table = run(data.path(), &m, &q, true);
        assert_eq!((table.to, table.capped), (range::MAX_WINDOW, true), "the table keeps analyze's cap");
        assert_eq!(run(data.path(), &m, &FileQuery::default(), true).to, DEFAULT_ROWS);
    }

    #[test]
    fn rows_report_each_file_against_the_index() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/a.ts"), "export const a = 1;").unwrap();
        std::fs::write(repo.path().join("notes.md"), "# N").unwrap();
        std::fs::write(data.path().join("graph.json"), "{}").unwrap();
        let mut m = ProjectMeta::new("p", &repo.path().to_string_lossy(), 2, 1);
        m.files = vec!["notes.md".into(), "src/a.ts".into(), "gone.py".into()];
        m.code_nodes = 1;
        let r = rows(data.path(), &m);
        let got: Vec<_> = r.iter().map(|r| (r.path.as_str(), r.state, r.bytes)).collect();
        assert_eq!(got, [("notes.md", FileState::Fresh, 3), ("src/a.ts", FileState::Fresh, 19), ("gone.py", FileState::Missing, 0)]);
        assert!(project_at(data.path()).is_err(), "no project.json: never generated");
    }
}
