//! Pull the prose out of a node's source body.
//!
//! Inline comments are the one place in a codebase where intent is written
//! in the language people actually query in. That matters here because
//! only about a third of functions in a typical repo carry a doc comment,
//! so for most nodes the embedding text has no prose in it at all — while
//! the body immediately below often explains exactly why the code exists.
//!
//! Extraction runs over the source span already captured at ingest (see
//! [`crate::storage::source`]), so it costs no extra I/O and needs no
//! per-language parser. A regex-free scanner handles the three comment
//! syntaxes that cover every language the indexer supports: `//`, `/* */`,
//! and `#`.
//!
//! Three things get filtered out, in order of how much damage they'd do:
//!
//! 1. **Commented-out code.** `// let x = foo();` is not prose, and repos
//!    are full of it. Scored heuristically on symbol density and code-like
//!    endings rather than parsed.
//! 2. **Repeated banners.** Licence headers and file preambles otherwise
//!    appear verbatim on every node in a file — and identically across
//!    every file — swamping the signal they're meant to add.
//! 3. **Machine directives.** `eslint-disable`, `#!/usr/bin/env`, doc
//!    attributes and their kin are addressed to tools, not readers.

/// Cap on extracted comment text per node. Comments are a supplement to
/// the name and docstring, not a replacement — and the embedder's window,
/// while roomy, is not unlimited.
const MAX_COMMENT_CHARS: usize = 600;

/// A comment line must be at least this long to be worth indexing, which
/// drops separators (`// ---`), closing markers, and stray `//`.
const MIN_COMMENT_LEN: usize = 12;

/// Proportion of non-alphanumeric, non-space characters above which a line
/// reads as code rather than prose.
const CODE_SYMBOL_RATIO: f32 = 0.28;

/// Extract prose comments from a source span, in source order, joined into
/// one string. Empty when the span has nothing worth indexing.
///
/// `seen_banner` carries state across the nodes of a graph so a repeated
/// header is indexed at most once; pass a fresh set per graph.
pub fn extract_prose_comments(code: &str, seen_banner: &mut std::collections::HashSet<u64>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut total = 0usize;

    for raw in comment_lines(code) {
        let line = raw.trim();
        if !is_prose(line) {
            continue;
        }
        // A comment block that already appeared elsewhere in this graph is
        // a banner, not a description of this node.
        let key = fnv1a64(line.as_bytes());
        if !seen_banner.insert(key) {
            continue;
        }
        total += line.len() + 1;
        out.push(line.to_string());
        if total >= MAX_COMMENT_CHARS {
            break;
        }
    }

    let mut joined = out.join(" ");
    if joined.len() > MAX_COMMENT_CHARS {
        let cut = joined
            .char_indices()
            .nth(MAX_COMMENT_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(joined.len());
        joined.truncate(cut);
    }
    joined
}

/// Yield the text of every comment in `code`, one entry per line, with the
/// comment markers stripped.
///
/// Deliberately not a parser: it tracks string literals well enough to
/// avoid treating `"http://x"` as a comment, and that is the only case
/// that matters in practice.
fn comment_lines(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = code.chars().collect();
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    let mut in_block = false;
    let mut block_buf = String::new();

    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();

        if in_block {
            if c == '*' && next == Some('/') {
                out.extend(block_buf.lines().map(strip_block_prefix));
                block_buf.clear();
                in_block = false;
                i += 2;
                continue;
            }
            block_buf.push(c);
            i += 1;
            continue;
        }

        if let Some(q) = in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += 1;
            continue;
        }

        match c {
            '"' | '\'' | '`' => {
                in_string = Some(c);
                i += 1;
            }
            '/' if next == Some('/') => {
                let start = i + 2;
                let end = bytes[start..]
                    .iter()
                    .position(|&x| x == '\n')
                    .map(|p| start + p)
                    .unwrap_or(bytes.len());
                out.push(bytes[start..end].iter().collect::<String>());
                i = end;
            }
            '/' if next == Some('*') => {
                in_block = true;
                i += 2;
            }
            '#' => {
                let start = i + 1;
                let end = bytes[start..]
                    .iter()
                    .position(|&x| x == '\n')
                    .map(|p| start + p)
                    .unwrap_or(bytes.len());
                out.push(bytes[start..end].iter().collect::<String>());
                i = end;
            }
            _ => i += 1,
        }
    }
    if in_block && !block_buf.is_empty() {
        out.extend(block_buf.lines().map(strip_block_prefix));
    }
    out
}

/// Strip the leading `*` decoration block comments conventionally carry.
fn strip_block_prefix(line: &str) -> String {
    line.trim().trim_start_matches('*').trim().to_string()
}

/// Whether a comment line reads as prose a person would query for.
fn is_prose(line: &str) -> bool {
    let line = line.trim_start_matches(['/', '*', '!', '#', '-', '=']).trim();
    if line.len() < MIN_COMMENT_LEN {
        return false;
    }
    // Addressed to tooling, not to a reader.
    let lower = line.to_ascii_lowercase();
    const DIRECTIVES: &[&str] = &[
        "eslint", "prettier", "tslint", "clippy", "rustfmt", "noqa", "type:",
        "@ts-", "pylint", "coverage:", "safety:", "allow(", "deny(", "cfg(",
        "!/usr/", "!/bin/", "-*- coding",
    ];
    if DIRECTIVES.iter().any(|d| lower.starts_with(d) || lower.contains(d)) {
        return false;
    }
    // Commented-out code: dense in punctuation, or ending the way a
    // statement does.
    if line.ends_with(';') || line.ends_with('{') || line.ends_with("=> {") {
        return false;
    }
    let symbols = line
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .count();
    if symbols as f32 / line.len() as f32 > CODE_SYMBOL_RATIO {
        return false;
    }
    // Needs at least a couple of real words to be a sentence fragment.
    line.split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphabetic()))
        .count()
        >= 3
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn extract(code: &str) -> String {
        extract_prose_comments(code, &mut HashSet::new())
    }

    #[test]
    fn pulls_line_and_block_prose() {
        let code = r#"
// Retry with backoff because the upstream rate limits aggressively.
fn go() {
    /* The first attempt is deliberately immediate so the happy
       path stays fast. */
    let x = 1;
}
"#;
        let got = extract(code);
        assert!(got.contains("Retry with backoff because the upstream rate limits"));
        assert!(got.contains("happy"));
    }

    #[test]
    fn drops_commented_out_code() {
        let code = "\
// let previous = compute(a, b);
// self.cache.insert(key, value);
// This explains why the cache exists at all.
";
        let got = extract(code);
        assert!(got.contains("This explains why the cache exists"));
        assert!(!got.contains("compute"), "commented-out code kept: {got}");
        assert!(!got.contains("cache.insert"), "commented-out code kept: {got}");
    }

    #[test]
    fn drops_tool_directives_and_separators() {
        let code = "\
// eslint-disable-next-line no-console
// ----------------------------------
#!/usr/bin/env python
// A genuine sentence about the behaviour here.
";
        let got = extract(code);
        assert_eq!(got, "A genuine sentence about the behaviour here.");
    }

    #[test]
    fn ignores_comment_markers_inside_strings() {
        let code = r#"let url = "https://example.com/path"; // Points at the public mirror."#;
        let got = extract(code);
        assert!(got.contains("Points at the public mirror"));
        assert!(!got.contains("example.com"), "string body leaked: {got}");
    }

    #[test]
    fn a_repeated_banner_is_indexed_once_per_graph() {
        let banner = "// Copyright the authors. Licensed under the Apache License.\n";
        let mut seen = HashSet::new();
        let first = extract_prose_comments(banner, &mut seen);
        let second = extract_prose_comments(banner, &mut seen);
        assert!(!first.is_empty(), "first occurrence is kept");
        assert!(second.is_empty(), "repeat is dropped: {second}");
    }

    #[test]
    fn output_is_bounded() {
        let long: String = (0..500)
            .map(|i| format!("// Sentence number {i} explaining the behaviour in detail.\n"))
            .collect();
        assert!(extract(&long).len() <= MAX_COMMENT_CHARS);
    }

    #[test]
    fn python_and_shell_hash_comments_work() {
        let code = "# Normalises the payload before it reaches the queue.\nx = 1\n";
        assert!(extract(code).contains("Normalises the payload"));
    }
}
