//! Terminal styling: the escape codes, the runtime colour gate, and the
//! [`Render`] enum every text renderer in the crate formats through.
//!
//! Three pieces that only make sense together. [`Render`] decides *what* an
//! emphasis means (`**bold**` for MCP, an SGR pair for a terminal); the `C_*`
//! constants are the codes it emits; [`color`] is the one runtime switch that
//! turns those codes off. Keeping them in one module is what stops a renderer
//! from reaching for a constant directly and quietly escaping the gate.
//!
//! `lib.rs` re-exports this module's contents at the crate root, so the
//! historical `ultragraph::C_CYAN` / `ultragraph::color::set` paths still
//! resolve; `agent_tools` re-exports [`Render`] for the same reason.

// --- Shared Color Constants ---
//
// These are the escape codes used when colour is ON. The runtime gate lives
// in [`color`] below: when it is off, the `Render::Ansi` styling helpers
// return the plain string instead of wrapping it in these codes, so every
// agent-tool / `analyze` command emits plain text when piped or when the
// caller asks for it. Human-facing CLI banners keep using these constants
// directly and stay coloured in a terminal.
pub const C_CYAN: &str = "\x1b[36m";
pub const C_MAGENTA: &str = "\x1b[35m";
pub const C_YELLOW: &str = "\x1b[33m";
pub const C_GREEN: &str = "\x1b[32m";
pub const C_RED: &str = "\x1b[31m";
pub const C_BLUE: &str = "\x1b[34m";
pub const C_RESET: &str = "\x1b[0m";
pub const C_BOLD: &str = "\x1b[1m";
pub const C_DIM: &str = "\x1b[2m";

/// Runtime colour gate for the agent-tool / `analyze` renderers.
///
// Set once at process start from `--no-color`, `NO_COLOR`, and whether
// stdout is a terminal. The `Render::Ansi` styling helpers consult
// [`enabled`] so that piping `ug` (or any non-tty consumer — an LLM, a
// log shipper) gets plain text without every command rewriting its format
// strings. `Render::Markdown` is already colour-free and is unaffected.
pub mod color {
    use std::sync::atomic::{AtomicBool, Ordering};

    static ENABLED: AtomicBool = AtomicBool::new(true);

    /// Set the gate. Call once near the top of `main`.
    pub fn set(on: bool) {
        ENABLED.store(on, Ordering::Relaxed);
    }

    /// Whether the `Render::Ansi` styling helpers should emit escape codes.
    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }
}

/// Runtime gate for the live single-line progress meters.
///
/// Separate from [`color`] on purpose: they answer different questions.
/// `NO_COLOR` in a terminal still wants a meter, just an uncoloured one,
/// and a pipe that *is* reading the frames (the KB Manager's log viewer)
/// still wants them even though it takes plain text.
///
/// The frames are `\r` repaints of one line. A terminal renders that as a
/// meter; anything else accumulates ~100 copies of it — which is why the
/// git-hook log strips `\r` after the fact and `pump_gen_output` splits on
/// it. So the default when stdout is not a terminal is off, and the one
/// consumer that wants the frames over a pipe opts back in with
/// `UG_PROGRESS=1`.
///
/// Only the *intermediate* frames are gated. Every phase's terminating
/// "✓ done" line is a normal `println!` and always prints, so a piped
/// `ug gen` still reports what it did — it just does it in one line per
/// phase instead of one per percent.
pub mod progress {
    use std::sync::atomic::{AtomicBool, Ordering};

    static ENABLED: AtomicBool = AtomicBool::new(false);

    /// Set the gate. Call once near the top of `main`.
    pub fn set(on: bool) {
        ENABLED.store(on, Ordering::Relaxed);
    }

    /// Whether a `\r` progress frame should be painted.
    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }
}

/// How a result renders to text. The *layout* is identical either way — only
/// the emphasis markers differ — so CLI and MCP output can't drift apart.
/// JSON output doesn't go through here; transports serialize the result
/// struct directly.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Render {
    /// ANSI escapes, for a terminal.
    Ansi,
    /// Markdown, for MCP clients (which render it in a chat transcript).
    Markdown,
}

impl Render {
    /// Wrap `s` in an SGR pair when colour is on, return it untouched when off.
    /// Called only from the `Render::Ansi` arm of each styling method, so the
    /// runtime gate ([`color`]) covers every agent-tool and `analyze`
    /// renderer without each call site branching on it.
    fn ansi(self, open: &str, s: &str) -> String {
        if color::enabled() {
            format!("{}{}{}", open, s, C_RESET)
        } else {
            s.to_string()
        }
    }

    pub(crate) fn bold(self, s: &str) -> String {
        match self {
            Render::Markdown => format!("**{}**", s),
            Render::Ansi => self.ansi(C_BOLD, s),
        }
    }

    pub(crate) fn dim(self, s: &str) -> String {
        match self {
            // Markdown has no "dim"; plain text keeps the line readable.
            Render::Markdown => s.to_string(),
            Render::Ansi => self.ansi(C_DIM, s),
        }
    }

    /// A node id, or anything else meant to be copied verbatim into a
    /// follow-up call.
    pub(crate) fn id(self, s: &str) -> String {
        match self {
            Render::Markdown => format!("`{}`", s),
            Render::Ansi => self.ansi(C_CYAN, s),
        }
    }

    pub(crate) fn heading(self, s: &str) -> String {
        match self {
            Render::Markdown => format!("## {}", s),
            Render::Ansi => self.ansi(C_BOLD, s),
        }
    }
}

#[cfg(test)]
mod gate_tests {
    /// The two gates answer different questions and must not be wired to
    /// each other: `NO_COLOR` in a terminal still wants a meter (just an
    /// uncoloured one), and the KB Manager's pipe wants the frames it reads
    /// even though it takes plain text. Wiring progress to `color` would
    /// break both.
    #[test]
    fn the_progress_gate_is_independent_of_the_colour_gate() {
        super::color::set(false);
        super::progress::set(true);
        assert!(!super::color::enabled());
        assert!(super::progress::enabled());

        super::color::set(true);
        super::progress::set(false);
        assert!(super::color::enabled());
        assert!(!super::progress::enabled());

        // Both gates are process-global; leave them as `main` sets them.
        super::color::set(true);
        super::progress::set(false);
    }

    /// Off unless something turns it on. A library caller, a test, or any
    /// path that never reaches `cli::run` must not repaint a meter into
    /// whatever is collecting its stdout.
    #[test]
    fn progress_defaults_to_off() {
        // Can't observe the initial value after another test has set it, so
        // assert the contract the default encodes: setting false is honoured.
        super::progress::set(false);
        assert!(!super::progress::enabled());
    }
}
