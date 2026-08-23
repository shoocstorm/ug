//! `host_guard.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::OnceLock;

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use super::*;

/// bare-IP cases below need no configuration.
pub(crate) fn extra_allowed_hosts() -> &'static HashSet<String> {
    static HOSTS: OnceLock<HashSet<String>> = OnceLock::new();
    HOSTS.get_or_init(|| {
        std::env::var("UG_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().trim_matches(['[', ']']).to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect()
    })
}

/// Strip the `:port` and any IPv6 brackets from a `Host`/`Origin` authority,
/// returning the lowercased hostname.
///
/// Splitting on the *last* colon is wrong for a bracketless IPv6 literal, so
/// bracketed forms are unwrapped first and anything still holding more than
/// one colon is treated as a bare IPv6 address rather than host:port.
pub(crate) fn host_label(authority: &str) -> String {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    if a.matches(':').count() > 1 {
        return a.to_ascii_lowercase(); // bare IPv6 literal
    }
    a.split(':').next().unwrap_or_default().to_ascii_lowercase()
}

/// Does `host` name *this* machine, as opposed to some attacker-controlled
/// domain that merely resolves to it right now?
///
/// An IP literal is accepted outright: a browser sends the hostname the page
/// was loaded from, so a rebinding attack always arrives carrying a domain
/// name, never a bare address. That keeps `--host 0.0.0.0` reachable over the
/// LAN by IP while still rejecting `http://evil.tld` rebound to 127.0.0.1.
pub(crate) fn is_allowed_host(host: &str) -> bool {
    let h = host_label(host);
    if h.is_empty() {
        return false;
    }
    h == "localhost"
        || h.ends_with(".localhost")
        || h.parse::<IpAddr>().is_ok()
        || extra_allowed_hosts().contains(&h)
}

/// Reject requests whose `Host` or `Origin` names a domain this server
/// doesn't answer to.
///
/// This is the DNS-rebinding defense, and it is what makes the rest of the
/// server's "it only listens on loopback" assumption actually hold. The
/// `CorsLayer` below stops a cross-origin page from *reading* a response, but
/// rebinding sidesteps CORS entirely: the attacker's own domain is re-pointed
/// at 127.0.0.1, so the browser considers the request same-origin and hands
/// over the reply. The one thing that still distinguishes it from a genuine
/// local request is the `Host` header, which carries the attacker's domain.
///
/// `Origin` is checked with the same predicate so a cross-site form post — a
/// "simple" request that needs no preflight and so is never blocked by CORS —
/// can't reach a state-changing route either.
pub(crate) async fn guard_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        // HTTP/2 carries the authority in the URI instead of a Host header.
        .or_else(|| req.uri().host().map(str::to_string));

    if let Some(host) = host {
        if !is_allowed_host(&host) {
            tracing::warn!(%host, "rejected request with a non-local Host header");
            return err_json(
                StatusCode::FORBIDDEN,
                "Host header is not a local address — refusing the request (set \
                 UG_ALLOWED_HOSTS if this server is behind a reverse proxy)",
            );
        }
    }

    // `Origin: null` (sandboxed iframe, file://) is not a host we can check
    // and not one we should trust with a state change.
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        let allowed = origin
            .split("://")
            .nth(1)
            .is_some_and(is_allowed_host);
        if !allowed {
            tracing::warn!(%origin, "rejected request with a cross-site Origin header");
            return err_json(StatusCode::FORBIDDEN, "cross-site Origin is not allowed");
        }
    }

    next.run(req).await
}
