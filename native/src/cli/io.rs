//! Process-level output and failure primitives: writing a result to a
//! file or stdout, exiting with a diagnosable message, and the panic hook
//! that keeps a crash from hanging on non-daemonized worker threads.

use std::fs;
use std::path::Path;

use ultragraph::{C_RED, C_RESET};

pub(crate) fn write_file(path: &str, data: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, data).unwrap_or_else(|e| die(1, format!("failed to write {path}: {e}")));
}

/// A command's failure: the message the user sees and the exit code that
/// goes with it.
///
/// Commands return this rather than calling [`die`] themselves, so the
/// process ends in exactly one place — `cli::dispatch`. That is what makes a
/// command callable from a test: `run_find_usages` reporting "no such node"
/// used to take the test process down with it, so every error path in the
/// command layer was unreachable and untested.
#[derive(Debug, PartialEq)]
pub(crate) struct CliError {
    pub(crate) code: i32,
    pub(crate) message: String,
}

impl CliError {
    /// The ordinary failure: exit code 1.
    pub(crate) fn new(msg: impl std::fmt::Display) -> Self {
        CliError { code: 1, message: msg.to_string() }
    }

    /// Usage errors — a bad flag or a missing argument — which the CLI
    /// reports as 2, matching the convention `--dest` already used.
    pub(crate) fn usage(msg: impl std::fmt::Display) -> Self {
        CliError { code: 2, message: msg.to_string() }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What every `run_*` entry point returns.
pub(crate) type CliResult = Result<(), CliError>;

/// Turn a command's outcome into a process exit. The one place the CLI
/// terminates on a handled failure.
pub(crate) fn exit_with(result: CliResult) {
    if let Err(e) = result {
        die(e.code, e.message);
    }
}

/// Print a user-facing error to stderr and exit with `code`. Used in place
/// of `panic!`/`expect` on paths a user reaches through normal operation
/// (missing file, corrupt graph, bad flag, store error) so the failure is a
/// one-line message + exit code. The release profile sets `panic = "abort"`,
/// so a `panic!` on these paths is a bare `SIGABRT` with no message — this
/// replaces that with something diagnosable.
pub(crate) fn die(code: i32, msg: impl std::fmt::Display) -> ! {
    eprintln!("{C_RED}error:{C_RESET} {msg}");
    std::process::exit(code);
}

/// If `output_path` is set, write to it and print a confirmation;
/// otherwise dump the payload to stdout.
pub(crate) fn write_or_print(output_path: Option<&str>, data: &str, label: &str) {
    match output_path {
        Some(p) => {
            if Path::new(p).is_dir() {
                eprintln!(
                    "Error: '{}' is a directory, not a file. Omit -o flag or specify a file path.",
                    p
                );
                std::process::exit(1);
            }
            write_file(p, data);
            println!("Wrote {} to {}", label, p);
        }
        None => println!("{}", data),
    }
}

/// Force-exit on panic so the process actually terminates. The local
/// (fastembed/ONNX) backend spawns rayon + ORT worker threads that are
/// not daemonized — a normal panic prints the message but then hangs
/// forever waiting for those threads, leaving Ctrl+C as the only way
/// out. Installing this hook keeps the default panic message but
/// forces a hard exit immediately after.
pub(crate) fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        std::process::exit(101);
    }));
}
