//! Shell-style wildcard patterns — one dialect, every surface.
//!
//! `ug find_symbols 'run_*'` (CLI), `POST /api/tools/find_symbols` with
//! `{"name": "run_*"}` (HTTP) and the MCP `find_symbols` tool all match
//! through here, so a pattern an agent learns on one transport behaves
//! identically on the other two.
//!
//! The dialect is the one people already know from shells and `.gitignore`:
//!
//! | Syntax     | Matches                                              |
//! |------------|------------------------------------------------------|
//! | `*`        | any run of characters (not `/` in path mode)         |
//! | `**`       | any run of characters, `/` included (path mode)      |
//! | `?`        | exactly one character (not `/` in path mode)         |
//! | `[abc]`    | one character from the set                           |
//! | `[a-z]`    | one character from the range                         |
//! | `[!ab]`    | one character *not* in the set (`[^ab]` also works)  |
//! | `{a,b}`    | either alternative — nestable                        |
//! | `\*`       | a literal `*` (backslash escapes any metacharacter)  |
//!
//! Matching is case-insensitive and whole-string: `get_*` matches
//! `get_code` but not `httpget_code`. Wrap in `*` to match anywhere, or
//! use [`Pattern::containing`], which does that for you.
//!
//! Patterns compile to a regex rather than running a bespoke backtracker:
//! `regex` is already a dependency, it is linear-time (so a pathological
//! pattern from a model cannot hang a tool call), and its character-class
//! parsing is the part a hand-rolled matcher gets wrong.

use regex::{Regex, RegexBuilder};

/// The dialect in one line, for tool descriptions and API discovery.
///
/// Every surface that documents wildcards interpolates this rather than
/// re-typing it: a model reading two tool descriptions that spell the syntax
/// differently will conclude the two tools differ.
pub const SYNTAX_SUMMARY: &str = "* any run of chars, ? one char, [abc]/[a-z] a set or range, [!ab] negated, {a,b} alternatives, \\* a literal star; patterns match the whole name, and in paths * stops at / while **/ crosses directories";

/// The characters that turn a string into a pattern rather than a literal.
const METACHARS: &[char] = &['*', '?', '[', '{'];

/// Does this string ask for wildcard matching?
///
/// The gate every caller uses to decide between pattern semantics and the
/// literal behaviour it had before: a name with no metacharacter must keep
/// ranking exact > prefix > substring, and a file filter with none must keep
/// being a plain prefix. An escaped metacharacter (`\*`) is a literal and
/// does not count.
pub fn is_pattern(s: &str) -> bool {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if METACHARS.contains(&c) {
            return true;
        }
    }
    false
}

/// Strip the escaping from a literal that contains `\*`-style escapes, so a
/// caller that decided not to use pattern semantics still compares against
/// what the user meant.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// How `*` and `?` treat the path separator.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// `*` and `?` match any character. For identifiers, which have no
    /// internal structure worth protecting.
    Name,
    /// `*` and `?` stop at `/`, and `**` crosses directories — the
    /// convention every glob-taking tool uses for paths, and the reason
    /// `src/*.rs` does not match `src/a/b.rs`.
    Path,
}

/// A compiled wildcard pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    re: Regex,
    source: String,
}

impl Pattern {
    /// Compile `pat` for whole-string, case-insensitive matching.
    ///
    /// Only fails on a pattern the regex engine rejects — an unbalanced
    /// character class such as `[a-`. The error is phrased for whoever typed
    /// the pattern, since it reaches the CLI and the MCP client verbatim.
    pub fn new(pat: &str, mode: Mode) -> Result<Pattern, String> {
        let re = RegexBuilder::new(&to_regex(pat, mode))
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
            .map_err(|_| {
                format!(
                    "'{}' is not a valid wildcard pattern — check the [...] brackets. \
                     Use \\[ for a literal bracket.",
                    pat
                )
            })?;
        Ok(Pattern {
            re,
            source: pat.to_string(),
        })
    }

    /// Compile `pat` so it matches anywhere in the subject rather than
    /// having to cover all of it — `auth` finds `handles auth retries`.
    ///
    /// Used for prose (docstrings), where anchoring would mean writing
    /// `*word*` for every search. An explicit leading/trailing `*` is left
    /// alone, so the anchored and unanchored forms compose.
    pub fn containing(pat: &str, mode: Mode) -> Result<Pattern, String> {
        let mut wrapped = String::with_capacity(pat.len() + 2);
        if !pat.starts_with('*') {
            wrapped.push('*');
        }
        wrapped.push_str(pat);
        if !pat.ends_with('*') || pat.ends_with("\\*") {
            wrapped.push('*');
        }
        let mut p = Pattern::new(&wrapped, mode)?;
        // Report the pattern the caller passed, not the padded rewrite.
        p.source = pat.to_string();
        Ok(p)
    }

    pub fn matches(&self, s: &str) -> bool {
        self.re.is_match(s)
    }

    /// The pattern as written, for error messages and result echoes.
    pub fn as_str(&self) -> &str {
        &self.source
    }
}

/// Translate a glob into an anchored regex source.
///
/// Kept separate from [`Pattern::new`] so the translation is testable on its
/// own — the brace and character-class cases are where a glob implementation
/// usually goes wrong, and asserting on strings pins them down exactly.
fn to_regex(pat: &str, mode: Mode) -> String {
    // `*`/`?` in path mode must not cross a directory boundary; in name mode
    // there is nothing to protect.
    let (star, any) = match mode {
        Mode::Path => ("[^/]*", "[^/]"),
        Mode::Name => (".*", "."),
    };

    let chars: Vec<char> = pat.chars().collect();
    let mut out = String::from("^");
    let mut brace_depth = 0usize;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                // Escape: the next character is a literal, whatever it is.
                i += 1;
                if i < chars.len() {
                    out.push_str(&regex::escape(&chars[i].to_string()));
                }
                i += 1;
            }
            '*' => {
                let doubled = chars.get(i + 1) == Some(&'*');
                if doubled && mode == Mode::Path {
                    // `**/` should also match zero directories, so
                    // `src/**/*.rs` finds `src/main.rs` as well as
                    // `src/a/b.rs` — the behaviour every other glob tool has
                    // and the one an agent will assume.
                    if chars.get(i + 2) == Some(&'/') {
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                } else if doubled {
                    out.push_str(".*");
                    i += 2;
                } else {
                    out.push_str(star);
                    i += 1;
                }
            }
            '?' => {
                out.push_str(any);
                i += 1;
            }
            '[' => match char_class(&chars, i) {
                Some((class, next)) => {
                    out.push_str(&class);
                    i = next;
                }
                // Unterminated `[` is a literal bracket rather than an
                // error: `find_symbols 'arr['` should look for that name,
                // not refuse to run.
                None => {
                    out.push_str("\\[");
                    i += 1;
                }
            },
            '{' => {
                out.push_str("(?:");
                brace_depth += 1;
                i += 1;
            }
            '}' if brace_depth > 0 => {
                out.push(')');
                brace_depth -= 1;
                i += 1;
            }
            ',' if brace_depth > 0 => {
                out.push('|');
                i += 1;
            }
            _ => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }

    // An unclosed `{` leaves groups open; close them so the regex still
    // compiles and behaves like the alternation the user started writing.
    for _ in 0..brace_depth {
        out.push(')');
    }
    out.push('$');
    out
}

/// Translate `[...]` starting at `start` into a regex character class.
///
/// Returns the class and the index just past the closing `]`, or `None` if
/// there is no closing bracket. POSIX rules apply inside: a `]` in the first
/// position is a literal, and `!` or `^` at the front negates.
fn char_class(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 1;
    let mut class = String::from("[");
    if matches!(chars.get(i), Some('!') | Some('^')) {
        class.push('^');
        i += 1;
    }
    if chars.get(i) == Some(&']') {
        class.push_str("\\]");
        i += 1;
    }
    let mut closed = false;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' {
            closed = true;
            i += 1;
            break;
        }
        match c {
            // A range stays a range; everything else is escaped so a `[`,
            // `&` or `\` inside the class cannot start a regex construct.
            '-' => class.push('-'),
            '\\' => {
                i += 1;
                if let Some(next) = chars.get(i) {
                    class.push_str(&escape_in_class(*next));
                }
            }
            _ => class.push_str(&escape_in_class(c)),
        }
        i += 1;
    }
    if !closed {
        return None;
    }
    class.push(']');
    Some((class, i))
}

fn escape_in_class(c: char) -> String {
    match c {
        '\\' | ']' | '[' | '^' | '&' | '~' => format!("\\{}", c),
        _ => c.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, s: &str) -> bool {
        Pattern::new(pat, Mode::Name).unwrap().matches(s)
    }

    fn p(pat: &str, s: &str) -> bool {
        Pattern::new(pat, Mode::Path).unwrap().matches(s)
    }

    #[test]
    fn detects_patterns_and_literals() {
        assert!(is_pattern("run_*"));
        assert!(is_pattern("get?"));
        assert!(is_pattern("[abc]x"));
        assert!(is_pattern("{a,b}"));
        assert!(!is_pattern("run_serve"));
        assert!(!is_pattern("src/auth/"));
        // An escaped metacharacter is a literal, so the caller keeps its
        // non-pattern path.
        assert!(!is_pattern("literal\\*star"));
    }

    #[test]
    fn star_and_question_match_whole_name() {
        assert!(m("run_*", "run_serve"));
        assert!(m("*serve*", "run_serve_bg"));
        assert!(!m("run_*", "prerun_serve"), "matching is anchored");
        assert!(m("get?", "getX"));
        assert!(!m("get?", "getXY"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(m("run_*", "RUN_SERVE"));
        assert!(m("Find*", "findSymbols"));
    }

    #[test]
    fn character_classes_and_negation() {
        assert!(m("[gs]et_code", "get_code"));
        assert!(m("[gs]et_code", "set_code"));
        assert!(!m("[gs]et_code", "let_code"));
        assert!(m("[!x]et", "get"));
        assert!(!m("[!g]et", "get"));
        assert!(m("f[a-o]o", "foo"));
        assert!(!m("f[a-e]o", "foo"));
    }

    #[test]
    fn braces_expand_to_alternatives() {
        assert!(m("run_{serve,gen}", "run_serve"));
        assert!(m("run_{serve,gen}", "run_gen"));
        assert!(!m("run_{serve,gen}", "run_index"));
        assert!(p("src/**/*.{ts,tsx}", "src/app/ui/Button.tsx"));
    }

    #[test]
    fn path_mode_keeps_star_inside_one_segment() {
        assert!(p("src/*.rs", "src/main.rs"));
        assert!(!p("src/*.rs", "src/storage/db.rs"));
        assert!(p("src/**/*.rs", "src/storage/db.rs"));
        // `**/` matches zero directories too — the case a strict reading of
        // the glob would miss.
        assert!(p("src/**/*.rs", "src/main.rs"));
        assert!(p("**/test_*.py", "a/b/test_auth.py"));
    }

    #[test]
    fn escapes_make_metacharacters_literal() {
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axxb"));
        assert_eq!(unescape("a\\*b"), "a*b");
    }

    #[test]
    fn unterminated_bracket_is_a_literal_not_an_error() {
        assert!(m("arr[", "arr["));
    }

    #[test]
    fn containing_matches_anywhere_in_prose() {
        let pat = Pattern::containing("cache", Mode::Name).unwrap();
        assert!(pat.matches("Drops the stale cache entries."));
        assert_eq!(pat.as_str(), "cache", "echoes what the caller wrote");
        // An explicit wildcard still composes.
        let pat = Pattern::containing("invalidat*", Mode::Name).unwrap();
        assert!(pat.matches("Handles invalidation of the index."));
    }

    #[test]
    fn regex_metacharacters_in_a_literal_are_escaped() {
        assert!(m("a.b", "a.b"));
        assert!(!m("a.b", "axb"), "'.' is literal in a glob");
        assert!(m("f(x)+", "f(x)+"));
    }
}
