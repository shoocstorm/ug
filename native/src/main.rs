//! `ug` — the UltraGraph command-line binary.
//!
//! A shim, deliberately. The CLI itself (and the server, the MCP server, and
//! everything they share) lives in the library — see the module map at the
//! top of `src/lib.rs` for why. Keeping this file empty of logic is what lets
//! `[[bin]] test = false` in `Cargo.toml` drop a whole link unit from every
//! `cargo test`: there is nothing here left to test.

fn main() {
    ultragraph::cli::run();
}
