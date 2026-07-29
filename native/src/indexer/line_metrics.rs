//! Per-line classification of a source file, and the comment/code counts
//! derived from it.
//!
//! This exists so "how much of this repo is documented" can be answered
//! with a number rather than a guess. `has_doc` only says whether a doc
//! comment exists; it says nothing about a function with forty lines of
//! inline `//` explaining a subtle algorithm, which is the better-commented
//! of the two.
//!
//! **Computed once per file, in the shared indexer path** — not in each of
//! the five language extractors. Two reasons, and the second is the real
//! one:
//!
//! 1. Every language gains the metric at the same time, including any added
//!    later.
//! 2. The definition of "a comment" cannot drift between languages. Five
//!    implementations would eventually disagree about block comments,
//!    doc comments, or trailing comments, and the resulting cross-language
//!    statistic would be quietly meaningless.
//!
//! The scan is line-oriented and runs over the whole file once, so a symbol
//! only slices the result. Doing it per symbol would both re-scan shared
//! text and — worse — start each symbol with no idea whether it opened
//! inside a block comment.

/// What one line of source is.
///
/// A line with code *and* a trailing comment counts as [`Code`]: it is a
/// line of the program that happens to carry a note, and counting it as a
/// comment would make `code_lines` under-report real work.
///
/// [`Code`]: LineKind::Code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Blank,
    Comment,
    Code,
}

/// Which comment syntaxes a language uses.
///
/// Language-aware on purpose. A universal `#`-is-a-comment rule reads
/// Rust's `#[derive(Debug)]` and C's `#include` as comments, which would
/// not just be slightly wrong — on an attribute-heavy Rust file it would
/// invert the answer.
#[derive(Debug, Clone, Copy)]
pub struct CommentSyntax {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
}

const SLASH: CommentSyntax = CommentSyntax {
    line: &["//"],
    block: Some(("/*", "*/")),
};

const HASH: CommentSyntax = CommentSyntax {
    line: &["#"],
    block: None,
};

const NONE: CommentSyntax = CommentSyntax {
    line: &[],
    block: None,
};

/// Comment syntax for a language, by the name its indexer reports.
///
/// An unknown language gets [`NONE`] rather than a guess: reporting zero
/// comments is visible in the coverage line as an unpopulated property,
/// whereas guessing produces a plausible number nobody can audit.
pub fn syntax_for(language: &str) -> CommentSyntax {
    match language.to_ascii_lowercase().as_str() {
        "rust" | "typescript" | "javascript" | "java" | "go" | "c" | "cpp" | "csharp"
        | "kotlin" | "swift" | "scala" | "php" => SLASH,
        "python" | "ruby" | "shell" | "bash" | "yaml" | "toml" | "r" | "perl" => HASH,
        _ => NONE,
    }
}

/// Classify every line of `content`, 0-indexed.
///
/// Tracks string literals well enough not to read `"// not a comment"` or a
/// URL inside a string as a comment, and carries block-comment state across
/// lines. It is not a parser and does not need to be: the failure modes
/// left (a comment marker inside a raw string with unusual delimiters) shift
/// a count by a line or two, and every consumer of these numbers is looking
/// at ratios across thousands of them.
pub fn classify_lines(content: &str, syntax: CommentSyntax) -> Vec<LineKind> {
    let mut out = Vec::new();
    let mut in_block = false;

    for line in content.lines() {
        let (kind, still_in_block) = classify_one(line, syntax, in_block);
        in_block = still_in_block;
        out.push(kind);
    }
    out
}

fn classify_one(line: &str, syntax: CommentSyntax, mut in_block: bool) -> (LineKind, bool) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // A blank line inside a block comment is still comment territory,
        // but it is prose whitespace, not a commented line. Counting it
        // would inflate every multi-paragraph doc block.
        return (LineKind::Blank, in_block);
    }

    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    let mut saw_code = false;
    let mut saw_comment = false;

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();

        if in_block {
            saw_comment = true;
            match syntax.block.and_then(|(_, close)| rest.find(close)) {
                Some(pos) => {
                    let close_len = syntax.block.map(|(_, c)| c.chars().count()).unwrap_or(2);
                    i += pos + close_len;
                    in_block = false;
                }
                None => break,
            }
            continue;
        }

        let c = chars[i];

        if let Some(quote) = in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        if matches!(c, '"' | '\'' | '`') {
            in_string = Some(c);
            saw_code = true;
            i += 1;
            continue;
        }

        if let Some((open, _)) = syntax.block {
            if rest.starts_with(open) {
                in_block = true;
                saw_comment = true;
                i += open.chars().count();
                continue;
            }
        }

        if syntax.line.iter().any(|m| rest.starts_with(m)) {
            saw_comment = true;
            // Everything after a line-comment marker is comment, so there
            // is nothing left on this line to classify.
            break;
        }

        if !c.is_whitespace() {
            saw_code = true;
        }
        i += 1;
    }

    // Code wins over comment: a line that does work and also explains
    // itself is a line of code.
    let kind = if saw_code {
        LineKind::Code
    } else if saw_comment {
        LineKind::Comment
    } else {
        LineKind::Blank
    };
    (kind, in_block)
}

/// Comment and code line counts for one 1-based, inclusive line range.
///
/// Returns `(comment_lines, code_lines)`. Blank lines are in neither — a
/// span's blank count is `loc - comment - code`, and reporting it as code
/// is what makes a naive "lines of code" number 30% too big.
pub fn count_range(kinds: &[LineKind], start_line: u32, end_line: u32) -> (u32, u32) {
    if start_line == 0 || end_line < start_line {
        return (0, 0);
    }
    let lo = (start_line - 1) as usize;
    let hi = ((end_line - 1) as usize).min(kinds.len().saturating_sub(1));
    if lo >= kinds.len() {
        return (0, 0);
    }

    let mut comments = 0u32;
    let mut code = 0u32;
    for kind in &kinds[lo..=hi] {
        match kind {
            LineKind::Comment => comments += 1,
            LineKind::Code => code += 1,
            LineKind::Blank => {}
        }
    }
    (comments, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, syntax: CommentSyntax) -> Vec<LineKind> {
        classify_lines(src, syntax)
    }

    #[test]
    fn rust_attributes_are_code_not_comments() {
        // The reason this module takes a language at all. A universal
        // `#`-comment rule would call every derive and every cfg a comment.
        let src = "#[derive(Debug)]\n#[cfg(test)]\nstruct S;\n";
        assert_eq!(kinds(src, syntax_for("rust")), vec![LineKind::Code; 3]);
    }

    #[test]
    fn python_hashes_are_comments() {
        let src = "# explain the thing\nx = 1\n";
        assert_eq!(
            kinds(src, syntax_for("python")),
            vec![LineKind::Comment, LineKind::Code]
        );
    }

    #[test]
    fn a_trailing_comment_leaves_the_line_as_code() {
        let src = "let x = 1; // why one\n";
        assert_eq!(kinds(src, SLASH), vec![LineKind::Code]);
    }

    #[test]
    fn block_comments_span_lines_and_close_correctly() {
        let src = "/* one\n   two */\nlet x = 1;\n";
        assert_eq!(
            kinds(src, SLASH),
            vec![LineKind::Comment, LineKind::Comment, LineKind::Code]
        );
    }

    #[test]
    fn code_after_a_block_comment_closes_on_the_same_line_is_code() {
        let src = "/* note */ let x = 1;\n";
        assert_eq!(kinds(src, SLASH), vec![LineKind::Code]);
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_not_a_comment() {
        let src = "let url = \"https://example.com\";\nlet s = \"// not a comment\";\n";
        assert_eq!(kinds(src, SLASH), vec![LineKind::Code, LineKind::Code]);
    }

    #[test]
    fn blank_lines_inside_a_block_comment_stay_blank() {
        // Otherwise every paragraph break in a doc block inflates the
        // comment count.
        let src = "/* one\n\n   two */\n";
        assert_eq!(
            kinds(src, SLASH),
            vec![LineKind::Comment, LineKind::Blank, LineKind::Comment]
        );
    }

    #[test]
    fn doc_comments_count_as_comments() {
        let src = "/// what it does\n//! module note\nfn f() {}\n";
        assert_eq!(
            kinds(src, SLASH),
            vec![LineKind::Comment, LineKind::Comment, LineKind::Code]
        );
    }

    #[test]
    fn an_unknown_language_reports_no_comments_rather_than_guessing() {
        let src = "# maybe a comment?\nmaybe code\n";
        assert_eq!(
            kinds(src, syntax_for("cobol")),
            vec![LineKind::Code, LineKind::Code]
        );
    }

    #[test]
    fn counting_a_range_is_inclusive_at_both_ends() {
        let src = "fn f() {\n    // why\n\n    do_it();\n}\n";
        let k = kinds(src, SLASH);
        let (comments, code) = count_range(&k, 1, 5);
        assert_eq!(comments, 1);
        assert_eq!(code, 3, "signature, call and closing brace");
        // Blank lines are in neither bucket.
        assert_eq!(5 - comments - code, 1);
    }

    #[test]
    fn a_range_past_the_end_of_the_file_is_clamped_not_panicked() {
        let k = kinds("fn f() {}\n", SLASH);
        assert_eq!(count_range(&k, 1, 9_999), (0, 1));
        assert_eq!(count_range(&k, 50, 60), (0, 0));
        // A zero start_line would underflow the 1-based conversion.
        assert_eq!(count_range(&k, 0, 3), (0, 0));
    }
}
