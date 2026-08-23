//! `get_code` — agent tool.

use super::*;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCodeParams {
    /// Read exactly these symbols' line ranges.
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Repo-relative path, used when `node_id` is empty.
    pub file: Option<String>,
    #[serde(alias = "startLine", alias = "start")]
    pub start_line: Option<usize>,
    #[serde(alias = "endLine", alias = "end")]
    pub end_line: Option<usize>,
    /// The line window as one value — `"11-35"`, `"34-end"`, `"20"` — in the
    /// same dialect `analyze` uses for row windows, and parsed by the same
    /// code. `start_line`/`end_line` win when both are given, so the two
    /// spellings can never disagree about what was asked for.
    pub range: Option<String>,
    #[serde(alias = "maxChars")]
    pub max_chars: Option<usize>,
    /// Drop the leading doc-comment preview from each slice. Set by the
    /// CLI's `--no-doc` when the caller already saw it (e.g. via
    /// `find_symbols --include-docs`) and wants only the body.
    #[serde(default)]
    pub no_doc: bool,
}

const DEFAULT_MAX_CHARS: usize = 20_000;

#[derive(Debug, Clone, Serialize)]
pub struct CodeSlice {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Characters dropped to honour `max_chars`; 0 when nothing was cut.
    pub truncated_chars: usize,
    /// Set when the slice may not be what it claims. Two cases:
    /// - **live file, indexed copy disagrees** — the slice is current source
    ///   read from disk, but the node's recorded `start`/`end` came from an
    ///   older capture, so the span may point at the wrong lines.
    /// - **indexed copy, file changed** — the slice is the stale captured
    ///   text served because the repo is absent.
    /// Either way the code is still returned; the flag tells the caller not
    /// to trust line numbers as current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub use crate::storage::source::{IndexedSource, StoredSource};

/// Where a read tool gets source text from, in priority order.
///
/// `indexed` is the copy ingest captured into the project's store. It is
/// the answer that needs no repo — the whole point of the type — and it is
/// also the *consistent* answer, since it is what the node's description
/// and embedding were built from.
///
/// `repo_root` points at the working tree. [`get_code`] prefers it over the
/// captured copy whenever the file actually exists on disk: an agent
/// reading source in a live editing session needs the current lines, and a
/// capture is only ever as current as the last `ug gen`. The captured
/// copy stays the fallback for when the repo is absent (a moved checkout,
/// or a machine that only ever held the index), and the two are compared
/// so a drift between them is flagged rather than silently trusted.
///
/// It is allowed to point at nothing — a path that no longer exists simply
/// means the working-tree half yields nothing, not an error.
#[derive(Clone, Copy)]
pub struct SourceCtx<'a> {
    indexed: Option<&'a IndexedSource>,
    repo_root: &'a Path,
}

impl<'a> SourceCtx<'a> {
    /// The full context: indexed source + working tree, live file preferred.
    pub fn new(indexed: &'a IndexedSource, repo_root: &'a Path) -> Self {
        SourceCtx {
            indexed: Some(indexed),
            repo_root,
        }
    }

    /// Working tree only, for callers with no store at hand (tests, and
    /// the legacy path where a project has a graph but was never ingested).
    pub fn repo_only(repo_root: &'a Path) -> Self {
        SourceCtx {
            indexed: None,
            repo_root,
        }
    }

    pub fn repo_root(&self) -> &'a Path {
        self.repo_root
    }

    /// Captured source for one node id.
    pub fn node(&self, id: &str) -> Option<&'a StoredSource> {
        self.indexed?.node(id)
    }

    /// Captured source for a whole file, via its File node.
    ///
    /// The indexer emits one range-less node per file whose capture is the
    /// entire file (see [`capture_graph_code`]), which is what makes an
    /// arbitrary `get_code --file X --start 180 --end 210` answerable from
    /// the index instead of from disk.
    ///
    /// [`capture_graph_code`]: crate::storage::capture_graph_code
    pub fn file(&self, graph: &GraphData, file: &str) -> Option<&'a StoredSource> {
        let indexed = self.indexed?;
        whole_file_node_ids(graph, file)
            .into_iter()
            .find_map(|id| indexed.node(id))
    }
}

/// Ids of the range-less nodes that carry `file`'s whole-file capture.
///
/// Plural because a File node and a Config node can both cover one path;
/// callers take the first that actually has captured code.
pub fn whole_file_node_ids<'a>(graph: &'a GraphData, file: &str) -> Vec<&'a str> {
    graph
        .nodes
        .iter()
        .filter(|n| n.start_line.is_none() && n.file.as_deref() == Some(file))
        .map(|n| n.id.as_str())
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct GetCodeResult {
    pub slices: Vec<CodeSlice>,
}

impl GetCodeResult {
    pub fn ok(&self) -> bool {
        self.slices.iter().all(|s| s.error.is_none())
    }
}

/// Read source for nodes, or for a file/line range.
///
/// The working tree wins when it has the file — an agent reading source in
/// a live session wants current lines, not a capture — and the indexed copy
/// answers when the repo is not on this machine. Whichever copy is served,
/// a difference between the two is flagged on the slice (`stale`) rather
/// than silently trusted, so the caller knows whether a line range is the
/// current truth or the node's recorded span.
pub fn get_code(graph: &GraphData, src: SourceCtx, p: &GetCodeParams) -> GetCodeResult {
    let max_chars = p.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
    let mut slices = Vec::new();

    // Resolve the line window before reading anything: a malformed `range`
    // is the caller's mistake, and reporting it beats silently serving line
    // 1 to EOF as if that were what they asked for.
    let (start_line, end_line) = match line_window(p) {
        Ok(w) => w,
        Err(e) => return GetCodeResult { slices: vec![err_slice(p.file.as_deref().unwrap_or(""), e)] },
    };

    if p.node_id.is_empty() {
        let Some(file) = p.file.as_deref() else {
            return GetCodeResult {
                slices: vec![CodeSlice {
                    title: String::new(),
                    file: None,
                    start_line: None,
                    end_line: None,
                    total_lines: None,
                    doc: None,
                    code: None,
                    truncated_chars: 0,
                    stale: None,
                    error: Some("Pass node_id (one or more ids) or file.".into()),
                }],
            };
        };
        let file = strip_file_id_prefix(file);
        slices.push(read_slice(
            graph,
            src,
            file,
            start_line.unwrap_or(1),
            end_line.unwrap_or(usize::MAX),
            None,
            max_chars,
        ));
        return GetCodeResult { slices };
    }

    for id in &expand_node_refs(graph, &p.node_id, MAX_REF_EXPANSION) {
        let Some(n) = graph.nodes.iter().find(|n| n.id == *id) else {
            slices.push(err_slice(
                id,
                unresolved_ref_error(graph, id, MAX_REF_EXPANSION),
            ));
            continue;
        };
        let Some(f) = &n.file else {
            slices.push(err_slice(
                id,
                format!(
                    "Node '{}' ({}) has no source file.",
                    id,
                    node_type_str(&n.node_type)
                ),
            ));
            continue;
        };
        let start = n.start_line.unwrap_or(1) as usize;
        // No end line means "the whole file" (File nodes carry no range at
        // all), not "one line".
        let end = n.end_line.map(|v| v as usize).unwrap_or({
            if n.start_line.is_some() {
                start
            } else {
                usize::MAX
            }
        });
        // Live working tree first — current by definition — then the index
        // for when the repo is absent. The captured hash is passed through
        // so a live read that disagrees with what was indexed can be flagged.
        let indexed_hash = src.node(id).map(|s| s.file_hash.as_str());
        slices.push(
            live_slice(src.repo_root(), f, start, end, Some(n), max_chars, indexed_hash)
                .unwrap_or_else(|| match src.node(id) {
                    Some(stored) => {
                        stored_slice(stored, src.repo_root(), f, start, end, n, max_chars)
                    }
                    None => read_slice(graph, src, f, start, end, Some(n), max_chars),
                }),
        );
    }

    let mut result = GetCodeResult { slices };
    if p.no_doc {
        for s in &mut result.slices {
            s.doc = None;
        }
    }
    result
}

/// Build a slice from indexed source, flagging it when the file it came
/// from no longer hashes the same.
fn stored_slice(
    src: &StoredSource,
    repo_root: &Path,
    file: &str,
    start: usize,
    end: usize,
    node: &GraphNode,
    max_chars: usize,
) -> CodeSlice {
    let total_lines = src.code.lines().count();
    let (code, truncated_chars) = if src.code.len() > max_chars {
        let cut = src.code.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(src.code.len());
        (src.code[..cut].to_string(), src.code.len() - cut)
    } else {
        (src.code.clone(), 0)
    };
    let stale = stale_note(repo_root, file, &src.file_hash);
    CodeSlice {
        title: format!("{} {}", node_type_str(&node.node_type), node.name),
        file: Some(file.to_string()),
        start_line: Some(start),
        end_line: Some(if end == usize::MAX { total_lines } else { end }),
        total_lines: Some(total_lines),
        doc: node.docstring.clone(),
        code: Some(code),
        truncated_chars,
        stale,
        error: None,
    }
}

/// The `(start, end)` lines a `get_code` call asks for, from either spelling.
///
/// `range` is parsed by [`crate::analyze::range`] — the same parser behind
/// `analyze`'s row windows — so `--range 11-35` means the same shape of
/// thing in both commands and every spelling that works in one works in the
/// other (`11-35`, `11..35`, `34-end`, `34-`, `20`, `top 20`). What it does
/// *not* borrow is that module's `MAX_WINDOW` row cap: a line window is
/// bounded by `max_chars` instead, and silently truncating a 300-line
/// function at line 200 would be a wrong answer that looks right.
///
/// Explicit `start_line`/`end_line` win over `range`, so a caller that sets
/// both gets the more specific one rather than an arbitrary tiebreak.
pub(crate) fn line_window(p: &GetCodeParams) -> Result<(Option<usize>, Option<usize>), String> {
    let Some(raw) = p.range.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((p.start_line, p.end_line));
    };
    let w = crate::analyze::range::parse(raw).ok_or_else(|| {
        format!(
            "Could not read {:?} as a line range. Use a count (`20` = the first 20 lines), \
             a closed range (`11-35`), or an open one (`34-end`).",
            raw
        )
    })?;
    Ok((
        p.start_line.or(Some(w.start)),
        p.end_line.or(w.end),
    ))
}

fn err_slice(title: &str, error: String) -> CodeSlice {
    CodeSlice {
        title: title.to_string(),
        file: None,
        start_line: None,
        end_line: None,
        total_lines: None,
        doc: None,
        code: None,
        truncated_chars: 0,
        stale: None,
        error: Some(error),
    }
}

/// Slice `start..=end` out of a whole file, from the working tree when it
/// has it and from the index otherwise.
///
/// The working tree is tried first — for a file/line-range read the caller
/// almost always wants the *current* lines (an editing agent paging through
/// a symbol), and the index is only the fallback for when the repo is not
/// on this machine. The index also covers arbitrary ranges, since the file's
/// range-less node holds the entire text rather than one symbol's span.
fn read_slice(
    graph: &GraphData,
    src: SourceCtx,
    file: &str,
    start: usize,
    end: usize,
    node: Option<&GraphNode>,
    max_chars: usize,
) -> CodeSlice {
    let title = match node {
        Some(n) => format!("{} {}", node_type_str(&n.node_type), n.name),
        None => file.to_string(),
    };

    let indexed_hash = src.file(graph, file).map(|s| s.file_hash.as_str());
    if let Some(slice) = live_slice(src.repo_root(), file, start, end, node, max_chars, indexed_hash) {
        return slice;
    }

    // Repo absent (or file gone): the indexed copy is the only source left.
    match src.file(graph, file) {
        Some(s) => {
            let all: Vec<&str> = s.code.split('\n').collect();
            let from = start.max(1).min(all.len());
            let to = end.min(all.len()).max(from);
            let mut text = all[from - 1..to].join("\n");
            let char_count = text.chars().count();
            let mut truncated = 0;
            if char_count > max_chars {
                truncated = char_count - max_chars;
                text = text.chars().take(max_chars).collect();
            }
            CodeSlice {
                title,
                file: Some(file.to_string()),
                start_line: Some(from),
                end_line: Some(to),
                total_lines: Some(all.len()),
                doc: node.and_then(|n| n.docstring.clone()),
                code: Some(text),
                truncated_chars: truncated,
                // The working tree could not be read at all, so this capture
                // is the answer. `stale_note` only fires when the file *is*
                // on disk but no longer hashes the same — which here means
                // "readable but changed since capture", the signal worth carrying.
                stale: stale_note(src.repo_root(), file, &s.file_hash),
                error: None,
            }
        }
        None => err_slice(&title, unreadable_reason(src.repo_root(), file)),
    }
}

/// Slice `start..=end` out of the file as it currently sits on disk, or
/// `None` when the file is not readable from the working tree.
///
/// The live copy is current by definition — the reason it is preferred — but
/// a node's recorded `start`/`end` were captured at index time, so when the
/// live file disagrees with the indexed hash the slice carries a `stale`
/// note: the lines shown are real and current, but they came from a span that
/// may have moved. `indexed_hash` is `None` when nothing was captured, in
/// which case there is nothing to disagree with and no flag is set.
fn live_slice(
    repo_root: &Path,
    file: &str,
    start: usize,
    end: usize,
    node: Option<&GraphNode>,
    max_chars: usize,
    indexed_hash: Option<&str>,
) -> Option<CodeSlice> {
    let content = std::fs::read_to_string(repo_root.join(file)).ok()?;
    let all: Vec<&str> = content.split('\n').collect();
    let from = start.max(1).min(all.len());
    let to = end.min(all.len()).max(from);
    let mut text = all[from - 1..to].join("\n");
    let char_count = text.chars().count();
    let mut truncated = 0;
    if char_count > max_chars {
        truncated = char_count - max_chars;
        text = text.chars().take(max_chars).collect();
    }
    let title = match node {
        Some(n) => format!("{} {}", node_type_str(&n.node_type), n.name),
        None => file.to_string(),
    };
    // Only flag when there is an indexed copy to compare against, and only
    // when the live file actually differs from it. A matching hash means the
    // span and the source agree.
    let stale = indexed_hash.and_then(|h| {
        let live = blake3::hash(content.as_bytes()).to_hex();
        if live.as_str() == h {
            None
        } else {
            Some(format!(
                "{} has changed since indexing — showing the live working tree; \
                 the recorded span may be stale, re-run `ug gen` to refresh",
                file
            ))
        }
    });
    Some(CodeSlice {
        title,
        file: Some(file.to_string()),
        start_line: Some(from),
        end_line: Some(to),
        total_lines: Some(all.len()),
        doc: node.and_then(|n| n.docstring.clone()),
        code: Some(text),
        truncated_chars: truncated,
        stale,
        error: None,
    })
}

/// Why neither the index nor the working tree could produce `file`.
///
/// Distinguishes a deleted repo from a deleted file: the fix is different
/// (restore the path vs re-run `ug gen`), and "not found under repo root"
/// reads as a lie when the root itself is gone.
fn unreadable_reason(repo_root: &Path, file: &str) -> String {
    if !repo_root.exists() {
        format!(
            "{} was not captured in the index, and the repo path {} is no longer available — \
             re-run `ug gen` from a checkout to capture it",
            file,
            repo_root.display()
        )
    } else {
        format!(
            "{} not found under repo root {} — the index may be stale (re-run ug gen).",
            file,
            repo_root.display()
        )
    }
}

/// The warning to attach to indexed source whose file has since changed.
/// `None` when the file still matches, and also when it cannot be read at
/// all — a missing working tree is the expected case here, not a staleness
/// signal, so it must not raise a false alarm.
fn stale_note(repo_root: &Path, file: &str, file_hash: &str) -> Option<String> {
    match crate::storage::file_matches_hash(repo_root, file, file_hash) {
        Some(false) => Some(format!(
            "{} has changed since indexing — this is the indexed copy; re-run `ug gen` to refresh",
            file
        )),
        _ => None,
    }
}

pub fn render_get_code(r: &GetCodeResult, style: Render) -> String {
    let mut out = String::new();
    for (i, s) in r.slices.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(e) = &s.error {
            line(&mut out, &format!("✗ {}", e));
            continue;
        }
        line(
            &mut out,
            &format!(
                "{}  —  {}:{}-{}",
                style.bold(&s.title),
                s.file.as_deref().unwrap_or("?"),
                s.start_line.unwrap_or(0),
                s.end_line.unwrap_or(0)
            ),
        );
        if let Some(d) = &s.doc {
            line(&mut out, &style.dim(&format!("doc: {}", d)));
        }
        // Loud rather than dim: an agent acting on out-of-date source is
        // the failure this whole column exists to make visible.
        if let Some(why) = &s.stale {
            line(&mut out, &format!("⚠ {}", why));
        }
        out.push('\n');
        if style == Render::Markdown {
            line(&mut out, "```");
        }
        line(&mut out, s.code.as_deref().unwrap_or(""));
        if style == Render::Markdown {
            line(&mut out, "```");
        }
        if s.truncated_chars > 0 {
            out.push('\n');
            line(
                &mut out,
                &style.dim(&format!(
                    "(truncated — {} more chars; narrow the line range or raise max_chars)",
                    s.truncated_chars
                )),
            );
        }
    }
    out
}
