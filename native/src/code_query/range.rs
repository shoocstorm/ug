//! Which rows of a result to show.
//!
//! An agent that got 20 of 122 rows and wants the next 20 has, without
//! this, one option: re-run the query with a bigger limit and re-read every
//! row it already saw. That is the expensive half of pagination — the
//! *reading*, not the computing — and it gets worse with each page.
//!
//! So a range is a **window over rows the query already produced**. It
//! changes nothing about what the engine computes, which is what keeps the
//! reported totals honest: "rows 41–60 of 122" is the same 122 no matter
//! which window you ask for.
//!
//! The syntax is deliberately liberal. These all parse:
//!
//! ```text
//! 10          top 10        1-10        1..10
//! 11-35       11 to 35      11..35
//! 34-end      34-           34..
//! ```
//!
//! Being strict here would buy nothing: every rejected spelling costs a
//! round-trip to a caller who was already unambiguous about what it wanted.

/// A 1-based, inclusive window of result rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRange {
    /// First row to show, 1-based.
    pub start: usize,
    /// Last row to show, inclusive. `None` means "to the end".
    pub end: Option<usize>,
}

/// Most rows one call will render, however wide a range is asked for.
///
/// A window is a token budget as much as a slice: `34-end` on a 10,000-row
/// result must not become a 10,000-row answer. The cap is reported when it
/// bites, so the caller knows to ask for the next window rather than
/// assuming it saw everything.
pub const MAX_WINDOW: usize = 200;

impl RowRange {
    /// The default window: the first `n` rows.
    pub fn first(n: usize) -> Self {
        RowRange {
            start: 1,
            end: Some(n),
        }
    }

    /// Resolve against a result of `total` rows, returning the 0-based
    /// half-open slice bounds. Empty when the window starts past the end.
    pub fn slice(&self, total: usize) -> (usize, usize) {
        let start = self.start.saturating_sub(1).min(total);
        let end = self
            .end
            .map(|e| e.min(total))
            .unwrap_or(total)
            .min(start + MAX_WINDOW)
            .max(start);
        (start, end)
    }

    /// Whether the window asked for more rows than [`MAX_WINDOW`] allows.
    pub fn is_capped(&self, total: usize) -> bool {
        let (start, end) = self.slice(total);
        end - start == MAX_WINDOW && total > end
    }
}

/// Parse a range expression. `None` for anything unrecognisable, so the
/// caller can report the input rather than silently showing row 1.
pub fn parse(input: &str) -> Option<RowRange> {
    let s = input
        .trim()
        .to_ascii_lowercase()
        // "top 10" and "first 10" are how the request is usually phrased;
        // strip the word and the number carries the meaning.
        .replace("top", " ")
        .replace("first", " ")
        .replace("rows", " ")
        .replace("row", " ")
        .replace("to", "-")
        .replace("..", "-")
        .replace(' ', "");

    if s.is_empty() {
        return None;
    }

    // A bare number is "the first N".
    if let Ok(n) = s.parse::<usize>() {
        return (n > 0).then(|| RowRange::first(n));
    }

    let (lo, hi) = s.split_once('-')?;
    let start: usize = lo.parse().ok().filter(|n| *n > 0)?;

    // Open-ended: "34-", "34-end", "34-all".
    if hi.is_empty() || hi == "end" || hi == "all" || hi == "last" {
        return Some(RowRange { start, end: None });
    }

    let end: usize = hi.parse().ok()?;
    if end < start {
        return None;
    }
    Some(RowRange {
        start,
        end: Some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: usize, end: Option<usize>) -> Option<RowRange> {
        Some(RowRange { start, end })
    }

    #[test]
    fn a_bare_number_is_the_first_n() {
        assert_eq!(parse("10"), r(1, Some(10)));
        assert_eq!(parse(" 25 "), r(1, Some(25)));
    }

    #[test]
    fn the_phrasings_an_agent_actually_writes_all_parse() {
        // Each of these was worth accepting rather than bouncing back.
        for input in ["top 10", "first 10", "1-10", "1..10", "1 to 10"] {
            assert_eq!(parse(input), r(1, Some(10)), "failed on {input:?}");
        }
    }

    #[test]
    fn closed_ranges_are_inclusive_at_both_ends() {
        assert_eq!(parse("11-35"), r(11, Some(35)));
        assert_eq!(parse("22-66"), r(22, Some(66)));
        assert_eq!(parse("11..35"), r(11, Some(35)));
        assert_eq!(parse("rows 11 to 35"), r(11, Some(35)));
    }

    #[test]
    fn open_ended_ranges_run_to_the_end() {
        for input in ["34-end", "34-", "34..", "34-all", "34-last"] {
            assert_eq!(parse(input), r(34, None), "failed on {input:?}");
        }
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed_at() {
        // Silently showing row 1 for an input the caller meant as a window
        // would be a wrong answer that looks right.
        assert_eq!(parse(""), None);
        assert_eq!(parse("banana"), None);
        assert_eq!(parse("35-11"), None, "backwards range");
        assert_eq!(parse("0"), None, "rows are 1-based");
        assert_eq!(parse("0-10"), None);
    }

    #[test]
    fn slicing_is_zero_based_half_open() {
        assert_eq!(RowRange::first(20).slice(122), (0, 20));
        assert_eq!(parse("11-35").unwrap().slice(122), (10, 35));
        assert_eq!(parse("34-end").unwrap().slice(50), (33, 50));
    }

    #[test]
    fn a_window_past_the_end_is_empty_rather_than_clamped_to_the_last_page() {
        // Clamping would hand back rows the caller did not ask for and had
        // probably already seen.
        let (start, end) = parse("200-300").unwrap().slice(50);
        assert_eq!(start, end, "empty");
    }

    #[test]
    fn a_partly_past_the_end_window_returns_what_exists() {
        assert_eq!(parse("40-100").unwrap().slice(50), (39, 50));
    }

    #[test]
    fn an_unbounded_window_is_capped_and_says_so() {
        let open = parse("1-end").unwrap();
        assert_eq!(open.slice(10_000), (0, MAX_WINDOW));
        assert!(open.is_capped(10_000));

        // Not capped when the result fits.
        assert_eq!(open.slice(30), (0, 30));
        assert!(!open.is_capped(30));
    }

    #[test]
    fn an_exactly_full_window_at_the_end_of_the_data_is_not_reported_as_capped() {
        let open = parse("1-end").unwrap();
        assert!(
            !open.is_capped(MAX_WINDOW),
            "showing every row is not truncation"
        );
    }
}
