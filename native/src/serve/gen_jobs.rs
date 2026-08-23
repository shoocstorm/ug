//! `gen_jobs.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock, RwLock};

// ---------- Background `ug gen` jobs (KB Manager wizard) ----------

#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum GenJobStatus {
    Running,
    Done,
    Error,
}

/// State for one wizard-triggered generation, run as a `ug gen`
/// subprocess so the pipeline logic isn't duplicated here. Streamed
/// stdout/stderr lines accumulate in `log` for the client to poll.
pub(crate) struct GenJob {
    pub(crate) status: GenJobStatus,
    pub(crate) log: Vec<String>,
    pub(crate) project_name: Option<String>,
    pub(crate) error: Option<String>,
}

/// In-memory registry of generation jobs, keyed by a per-process
/// monotonic id. Local dev tool, single user — no persistence or
/// eviction needed; the process restarting clears it.
pub(crate) struct GenJobs {
    pub(crate) next_id: AtomicU64,
    pub(crate) jobs: RwLock<HashMap<String, Arc<RwLock<GenJob>>>>,
}

impl GenJobs {
    pub(crate) fn new() -> Self {
        GenJobs {
            next_id: AtomicU64::new(1),
            jobs: RwLock::new(HashMap::new()),
        }
    }
}

/// Render `bytes` as a stream's current log line: overwrite the still-open
/// entry at `open_idx` if there is one, otherwise append a new entry and
/// mark it open. The log only ever grows, so the index stays valid.
fn write_gen_log_line(job: &RwLock<GenJob>, open_idx: &mut Option<usize>, bytes: &[u8]) {
    let line = strip_ansi(&String::from_utf8_lossy(bytes));
    let mut j = job.write().expect("job poisoned");
    match *open_idx {
        Some(i) if i < j.log.len() => j.log[i] = line,
        _ => {
            j.log.push(line);
            *open_idx = Some(j.log.len() - 1);
        }
    }
}

/// Stream one of the `ug gen` child's output pipes into the job log.
///
/// Splits on `\r` as well as `\n`: the pipeline prints long-phase progress
/// via `print!("\r…")` rewrites, so with a plain line reader an entire
/// phase (e.g. embedding thousands of nodes) surfaces as one giant line
/// only after its terminating `\n` — until then the log looks finished
/// while the job is still running. A `\r` rewrite updates the stream's
/// open log entry in place, and unterminated output is flushed after
/// every read so `print!` phase headers appear immediately. The open
/// entry is tracked per stream so interleaved stdout/stderr lines don't
/// overwrite each other.
pub(crate) async fn pump_gen_output<R>(mut stream: R, job: Arc<RwLock<GenJob>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    let mut partial: Vec<u8> = Vec::new();
    let mut open_idx: Option<usize> = None;
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            match b {
                b'\n' | b'\r' => {
                    if !partial.is_empty() {
                        write_gen_log_line(&job, &mut open_idx, &partial);
                        partial.clear();
                    } else if b == b'\n' && open_idx.is_none() {
                        // Bare println!() — preserve the blank line.
                        job.write().expect("job poisoned").log.push(String::new());
                    }
                    if b == b'\n' {
                        open_idx = None;
                    }
                }
                _ => partial.push(b),
            }
        }
        // `partial` keeps accumulating until a separator arrives; the
        // flush just renders its current state, so a line split across
        // reads is re-rendered whole on the next pass.
        if !partial.is_empty() {
            write_gen_log_line(&job, &mut open_idx, &partial);
        }
    }
    if !partial.is_empty() {
        write_gen_log_line(&job, &mut open_idx, &partial);
    }
}

/// Strip ANSI SGR escape sequences (`\x1b[...m`) from CLI output so the
/// wizard's plain-text log viewer doesn't show raw color codes.
pub(crate) fn strip_ansi(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("valid regex"));
    re.replace_all(s, "").into_owned()
}
