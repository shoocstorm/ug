//! `git_api.rs` — the change-shaped half of the API: what this repository
//! has changed, and the walk through it.
//!
//! Four routes, and the split between them is the point:
//!
//! - `GET /api/git/status`   — *can* this project be walked, and from where
//! - `GET /api/git/commits`  — what there is to walk (the picker's contents)
//! - `GET /api/git/diff`     — what a given spec actually touches (the preview)
//! - `POST /api/walk`        — the walk itself, JSON or SSE
//!
//! The first three exist so the UI never has to guess. A page that offers a
//! "walk a change" button on a machine with no git, or a commit dropdown it
//! populated by hoping, is a page that fails at the moment the user commits
//! to the action — which is the worst moment to find out. `status` is
//! therefore cheap, always answers, and answers *negatively* with a code
//! and a hint rather than with an HTTP error.
//!
//! Every git call is a blocking subprocess, so all four hop through
//! `spawn_blocking` rather than stalling a runtime worker.

use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::git::{self, GitError, RevSpec};
use crate::walk::{self, WalkOptions};

use super::api::{err_json, ok_json};
use super::chat_api::{merge_chat_overrides, ChatCfgError, ChatOverrides};
use crate::chat::ChatClient;
use super::*;

/// Turn a git failure into a JSON body the UI can switch on.
///
/// Always the same three fields — `error`, `code`, `hint` — so the client
/// has one shape to handle whatever went wrong, and never has to parse a
/// message to decide what to show.
fn git_err_json(e: &GitError) -> serde_json::Value {
    serde_json::json!({
        "error": e.to_string(),
        "code": e.code(),
        "hint": e.hint(),
    })
}

/// The HTTP status a git failure deserves.
///
/// "No git on this machine" and "not a working tree" are 200s carrying
/// `available: false`, not errors — see [`api_git_status`]. What reaches
/// here is a caller mistake (a revision that does not exist → 400) or a
/// genuine failure (→ 500).
fn git_status_code(e: &GitError) -> StatusCode {
    match e {
        GitError::BadRev { .. } => StatusCode::BAD_REQUEST,
        GitError::NotInstalled | GitError::NotARepo(_) | GitError::NoCommits => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        GitError::Failed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn git_err(e: &GitError) -> Response {
    (git_status_code(e), axum::Json(git_err_json(e))).into_response()
}

/// Run a blocking git call off the runtime's worker threads.
async fn blocking<T, F>(f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("git task failed: {}", e),
        )
    })
}

/// `GET /api/git/status` — is there a change to walk, and from where?
///
/// **Always 200.** "git is not installed" is a fact about the machine, not
/// a failed request, and a 503 here would make the UI's probe indistinguishable
/// from a server that is down. The body says `available: false` and carries
/// the code and the hint instead.
pub(crate) async fn api_git_status(State(state): State<ServeState>) -> Response {
    let root = state.repo_root();
    let probed = match blocking(move || git::status(&root)).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let body = match probed {
        Ok(s) => serde_json::json!({ "available": true, "repo": s }),
        Err(e) => {
            let mut v = git_err_json(&e);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("available".into(), serde_json::Value::Bool(false));
            }
            v
        }
    };
    ok_json(body.to_string())
}

#[derive(serde::Deserialize)]
pub(crate) struct CommitsQuery {
    limit: Option<usize>,
    /// Where to start the log. Defaults to `HEAD`; the UI passes a branch
    /// when the user picks one to compare against.
    rev: Option<String>,
}

/// `GET /api/git/commits?limit=30` — the commit picker's contents.
pub(crate) async fn api_git_commits(
    State(state): State<ServeState>,
    Query(q): Query<CommitsQuery>,
) -> Response {
    let root = state.repo_root();
    let limit = q.limit.unwrap_or(git::DEFAULT_COMMIT_LIMIT).clamp(1, 200);
    let rev = q.rev.filter(|r| !r.trim().is_empty());
    let got = match blocking(move || git::recent_commits(&root, limit, rev.as_deref())).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match got {
        Ok(commits) => ok_json(serde_json::json!({ "commits": commits }).to_string()),
        Err(e) => git_err(&e),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct DiffQuery {
    /// A revision spec in any of the forms [`RevSpec::parse`] accepts.
    /// Absent or empty means the uncommitted changes.
    spec: Option<String>,
}

/// `GET /api/git/diff?spec=HEAD` — what a spec touches, without walking it.
///
/// The preview behind the picker: file count, line counts and the file
/// list, so the user knows what they are about to walk before spending a
/// model on it.
pub(crate) async fn api_git_diff(
    State(state): State<ServeState>,
    Query(q): Query<DiffQuery>,
) -> Response {
    let root = state.repo_root();
    let spec = RevSpec::parse(&q.spec.unwrap_or_default());
    let got = match blocking(move || {
        walk::resolve_diff(&root, &spec).map(|d| {
            let drifted = walk::drifted_files(&root, &spec, &d);
            (d, drifted)
        })
    })
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match got {
        Ok((diff, drifted)) => ok_json(
            serde_json::json!({ "diff": diff, "drifted": drifted }).to_string(),
        ),
        Err(e) => git_err(&e),
    }
}

/// `POST /api/walk` body. The retrieval knobs `/api/tour` takes are absent
/// on purpose: a walk retrieves nothing.
#[derive(serde::Deserialize, Default)]
pub(crate) struct WalkBody {
    pub spec: Option<String>,
    pub max_stops: Option<usize>,
    /// Include the callers and tests the change reaches (default true).
    pub expand: Option<bool>,
    pub include_snippets: Option<bool>,
    pub include_debug: Option<bool>,
    pub stream: Option<bool>,
    pub think: Option<bool>,
    pub no_llm: Option<bool>,
    // Per-request model overrides, same names and meaning as `/api/tour`.
    pub chat_model: Option<String>,
    pub chat_base_url: Option<String>,
    pub chat_api_key: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

fn walk_opts(body: &WalkBody) -> WalkOptions {
    let mut o = WalkOptions::new();
    o.max_stops = body
        .max_stops
        .unwrap_or(o.max_stops)
        .clamp(1, crate::tour::MAX_STOPS_LIMIT);
    o.expand = body.expand.unwrap_or(true);
    o.include_snippets = body.include_snippets.unwrap_or(true);
    o.include_debug = body.include_debug.unwrap_or(true);
    o.stream = body.stream.unwrap_or(false);
    o.think = body.think.unwrap_or(false);
    o
}

/// The response shape: the tour, plus the diff it came from.
///
/// The diff is added *alongside* the tour rather than inside it, so a
/// client that already renders a tour renders a walk with no changes and
/// finds the change metadata in fields it may ignore.
pub(crate) fn walk_response_json(
    w: &walk::Walk,
    drifted: &[String],
    model: Option<&str>,
) -> serde_json::Value {
    let mut v = serde_json::to_value(&w.tour).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "diff".into(),
            serde_json::to_value(&w.diff).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "unmapped".into(),
            serde_json::to_value(&w.unmapped).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "drifted".into(),
            serde_json::to_value(drifted).unwrap_or(serde_json::Value::Null),
        );
        if let Some(m) = model {
            obj.insert("chat_model".into(), serde_json::Value::String(m.to_string()));
        }
    }
    v
}

/// Everything `/api/walk` needs that it can only get by shelling out.
struct Prepared {
    diff: crate::git::DiffSummary,
    drifted: Vec<String>,
}

/// Resolve the diff for a request, or the response that explains why not.
async fn prepare(state: &ServeState, spec: &RevSpec) -> Result<Prepared, Response> {
    let root = state.repo_root();
    let spec = spec.clone();
    let got = blocking(move || {
        walk::resolve_diff(&root, &spec).map(|d| {
            let drifted = walk::drifted_files(&root, &spec, &d);
            Prepared { diff: d, drifted }
        })
    })
    .await?;
    got.map_err(|e| git_err(&e))
}

/// `POST /api/walk` — plan a walkthrough of a change.
///
/// Needs neither the vector store nor an embedder: a walk reads
/// `graph.json`, which the server already has parsed and resident. The
/// language model is optional in the same way `/api/tour`'s is — without
/// one the response is the ranked itinerary, with a warning saying so.
pub(crate) async fn api_walk(
    State(state): State<ServeState>,
    Json(body): Json<WalkBody>,
) -> Response {
    let spec = RevSpec::parse(body.spec.as_deref().unwrap_or_default());

    let want_llm = !body.no_llm.unwrap_or(false);
    let chat_default = state
        .chat_default
        .read()
        .expect("chat_default poisoned")
        .clone();
    let chat_cfg = if want_llm {
        match merge_walk_chat_cfg(&chat_default, &body) {
            Ok(c) => Some(c),
            Err(ChatCfgError::NotConfigured) => None,
            Err(ChatCfgError::Invalid(msg)) => return err_json(StatusCode::BAD_REQUEST, &msg),
        }
    } else {
        None
    };

    let prepared = match prepare(&state, &spec).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if body.stream.unwrap_or(false) {
        return api_walk_stream(state, body, prepared, chat_cfg, want_llm);
    }

    let opts = walk_opts(&body);
    let repo_root = state.repo_root();
    let snapshot = state.snapshot();

    let mut used_model: Option<String> = None;
    let client = match chat_cfg {
        Some(cfg) => match ChatClient::new(cfg) {
            Ok(c) => {
                used_model = Some(c.config().model.clone());
                Some(c)
            }
            Err(e) => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    &format!("chat model could not be used: {}", e),
                )
            }
        },
        None => None,
    };

    let mut quiet = |_| {};
    let planned = walk::plan_walk(
        &snapshot.parsed,
        repo_root.as_path(),
        prepared.diff.clone(),
        &prepared.drifted,
        client.as_ref(),
        &opts,
        &mut quiet,
    )
    .await;

    let planned = match planned {
        Ok(w) => w,
        Err(e) if client.is_some() => {
            // The guide is optional; an unreachable model costs the
            // narration, not the walk.
            tracing::warn!(error = %e, "walk guide LLM failed; falling back to a ranked itinerary");
            used_model = None;
            let reason = e.to_string();
            let mut quiet = |_| {};
            match walk::plan_walk(
                &snapshot.parsed,
                repo_root.as_path(),
                prepared.diff,
                &prepared.drifted,
                None,
                &opts,
                &mut quiet,
            )
            .await
            {
                Ok(mut w) => {
                    w.tour.warnings.push(format!(
                        "The tour guide model was unreachable ({}); showing a ranked itinerary.",
                        reason
                    ));
                    w
                }
                Err(e) => {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("walk: {}", e),
                    )
                }
            }
        }
        Err(e) => {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("walk: {}", e))
        }
    };

    let mut planned = planned;
    if want_llm && used_model.is_none() && !planned.tour.stops.is_empty() && chat_default.is_none()
    {
        planned.tour.warnings.push(
            "No chat model is configured, so this is a ranked itinerary rather than a narrated walk."
                .to_string(),
        );
    }
    ok_json(
        walk_response_json(&planned, &prepared.drifted, used_model.as_deref()).to_string(),
    )
}

/// SSE variant of `/api/walk` — the same event vocabulary `/api/tour`
/// streams (`progress`, then one of `walk` / `error`), so the page's
/// reader is shared.
///
/// The terminal event is named `walk`, not `tour`: the payload carries
/// `diff` and per-stop `change`, and a client that sees `tour` is entitled
/// to assume neither is there.
fn api_walk_stream(
    state: ServeState,
    body: WalkBody,
    prepared: Prepared,
    chat_cfg: Option<ChatConfig>,
    want_llm: bool,
) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use futures::StreamExt;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    let repo_root = state.repo_root();
    let snapshot = state.snapshot();
    let opts = walk_opts(&body);

    tokio::spawn(async move {
        let emit = |name: &'static str, payload: serde_json::Value| {
            let _ = tx.send(SseEvent::default().event(name).data(payload.to_string()));
        };

        let mut used_model: Option<String> = None;
        let client = match chat_cfg {
            Some(cfg) => match ChatClient::new(cfg) {
                Ok(c) => {
                    used_model = Some(c.config().model.clone());
                    Some(c)
                }
                Err(e) => {
                    emit(
                        "error",
                        serde_json::json!({ "error": format!("chat model could not be used: {}", e) }),
                    );
                    return;
                }
            },
            None => None,
        };

        let emit_progress = emit;
        let mut on_progress = move |p: crate::tour::TourProgress| match serde_json::to_value(&p) {
            Ok(v) => emit_progress("progress", v),
            Err(e) => tracing::debug!(error = %e, "walk: progress encode failed"),
        };

        let result = walk::plan_walk(
            &snapshot.parsed,
            repo_root.as_path(),
            prepared.diff.clone(),
            &prepared.drifted,
            client.as_ref(),
            &opts,
            &mut on_progress,
        )
        .await;

        let result = match result {
            Ok(w) => Ok(w),
            Err(e) if client.is_some() => {
                used_model = None;
                let reason = e.to_string();
                emit(
                    "progress",
                    serde_json::json!({ "phase": "fallback", "reason": reason }),
                );
                let mut quiet = |_| {};
                walk::plan_walk(
                    &snapshot.parsed,
                    repo_root.as_path(),
                    prepared.diff,
                    &prepared.drifted,
                    None,
                    &opts,
                    &mut quiet,
                )
                .await
                .map(|mut w| {
                    w.tour.warnings.push(format!(
                        "The tour guide model was unreachable ({}); showing a ranked itinerary.",
                        reason
                    ));
                    w
                })
            }
            Err(e) => Err(e),
        };

        match result {
            Ok(mut w) => {
                if want_llm && used_model.is_none() && !w.tour.stops.is_empty() {
                    w.tour.warnings.push(
                        "No chat model is configured, so this is a ranked itinerary rather than a narrated walk."
                            .to_string(),
                    );
                }
                emit(
                    "walk",
                    walk_response_json(&w, &prepared.drifted, used_model.as_deref()),
                );
            }
            Err(e) => emit("error", serde_json::json!({ "error": format!("walk: {}", e) })),
        }
    });

    let stream =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Apply a walk request's model overrides on top of the server default.
///
/// Delegates to the tour's merge so the two routes cannot accept different
/// overrides or validate them differently — which is exactly the drift a
/// second hand-rolled merge would introduce.
fn merge_walk_chat_cfg(
    default: &Option<ChatConfig>,
    body: &WalkBody,
) -> Result<ChatConfig, ChatCfgError> {
    merge_chat_overrides(
        default,
        &ChatOverrides {
            model: body.chat_model.as_deref(),
            base_url: body.chat_base_url.as_deref(),
            api_key: body.chat_api_key.as_deref(),
            temperature: body.temperature,
            max_tokens: body.max_tokens,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_git_is_a_service_state_not_a_bad_request() {
        assert_eq!(
            git_status_code(&GitError::NotInstalled),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            git_status_code(&GitError::NotARepo("/tmp".into())),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // A revision the caller invented is the caller's mistake.
        assert_eq!(
            git_status_code(&GitError::BadRev {
                rev: "nope".into(),
                detail: String::new()
            }),
            StatusCode::BAD_REQUEST
        );
    }

    /// The UI switches on `code` and shows `hint`; both must always be
    /// there, whatever went wrong.
    #[test]
    fn every_git_error_body_carries_a_code_and_a_hint() {
        for e in [
            GitError::NotInstalled,
            GitError::NotARepo("/tmp/x".into()),
            GitError::NoCommits,
            GitError::BadRev { rev: "x".into(), detail: String::new() },
            GitError::Failed { what: "diff".into(), detail: "boom".into() },
        ] {
            let v = git_err_json(&e);
            assert!(v["code"].as_str().is_some_and(|s| !s.is_empty()), "{e}");
            assert!(v["hint"].as_str().is_some_and(|s| !s.is_empty()), "{e}");
            assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()), "{e}");
        }
    }

    #[test]
    fn walk_options_clamp_what_a_caller_asks_for() {
        let body = WalkBody {
            max_stops: Some(10_000),
            ..Default::default()
        };
        assert_eq!(walk_opts(&body).max_stops, crate::tour::MAX_STOPS_LIMIT);

        let body = WalkBody {
            max_stops: Some(0),
            ..Default::default()
        };
        assert_eq!(walk_opts(&body).max_stops, 1);
    }

    /// Expansion defaults on: "what does this change affect" is half the
    /// question, and a body that omits the field means the default, not
    /// `false`.
    #[test]
    fn expansion_is_on_unless_turned_off() {
        assert!(walk_opts(&WalkBody::default()).expand);
        assert!(!walk_opts(&WalkBody {
            expand: Some(false),
            ..Default::default()
        })
        .expand);
    }
}
