//! `watch.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::sync::Arc;
use std::time::Duration;

use super::projects_api::file_mtime;
use super::*;

// ---------- Watch (Phase 1.5) ----------

pub(crate) fn spawn_watch(state: ServeState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            // The snapshot carries the mtime it was read at, so "has this
            // changed" is answered by the context itself rather than by a
            // side table of last-seen mtimes here. That side table was the
            // only reason a *newly activated* project needed a priming tick,
            // and it could only ever track the active one.
            refresh_snapshot_if_stale(&state.registry.active_ctx()).await;
        }
    });
}

/// Reload a project's snapshot when its `graph.json` has changed since the
/// snapshot was read. No-op when it hasn't, or when there is no file to
/// compare against (the zero-project placeholder).
///
/// Every cached context needs this, not just the active one. `resolve_ctx`
/// loads a project on demand for any request carrying `?project=<name>` and
/// then keeps it in `loaded` indefinitely, while the watcher only ever looked
/// at the active project — so a CLI `ug gen` / `ug ingest` against some other
/// project landed, and every later request for it kept answering from the
/// pre-run graph, with no error and no staleness note. The MCP server has
/// always checked mtime on read (`mcp::Mcp::load_graph`); the two doors have
/// to agree about what the graph contains.
///
/// One `metadata` call inline, which is bounded; the read + parse behind it
/// scales with the graph, so it goes to `spawn_blocking`.
pub(crate) async fn refresh_snapshot_if_stale(ctx: &Arc<ProjectContext>) {
    let current = file_mtime(&ctx.graph_path);
    if current.is_none() {
        return; // no graph.json behind this context — nothing to compare
    }
    {
        let held = ctx.graph.read().expect("graph state poisoned");
        if held.mtime == current {
            return;
        }
    }

    let path = ctx.graph_path.clone();
    // Parse + recompress can take a few hundred ms on big graphs; do it off
    // the runtime so we don't stall HTTP handlers.
    let loaded = tokio::task::spawn_blocking(move || load_snapshot(&path)).await;
    match loaded {
        Ok(Ok(snap)) => {
            let bytes = snap.graph_bytes;
            let nodes = snap.parsed.nodes.len();
            let edges = snap.parsed.edges.len();
            if let Ok(mut w) = ctx.graph.write() {
                *w = snap;
                tracing::info!(
                    target: "ug::serve::watch",
                    project = %ctx.name,
                    path = %ctx.graph_path.display(),
                    bytes,
                    nodes,
                    edges,
                    "graph reloaded"
                );
            }
        }
        Ok(Err(e)) => tracing::warn!(
            target: "ug::serve::watch",
            project = %ctx.name,
            error = %e,
            "graph reload failed"
        ),
        Err(e) => tracing::warn!(
            target: "ug::serve::watch",
            project = %ctx.name,
            error = %e,
            "graph reload task failed"
        ),
    }
}
