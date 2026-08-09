//! System boundaries: where a repo's code meets everything that is not the
//! repo.
//!
//! A call graph answers "what calls what". It cannot answer the question
//! people actually ask before a change — "who outside this system depends on
//! it" — because the edges that leave the process are exactly the ones a
//! call graph does not have. A `@JmsListener` has no inbound call anywhere in
//! the source; a `@GetMapping` handler's real callers are HTTP clients that
//! were never indexed. Both look like dead code, and both are contracts.
//!
//! This module tags those symbols. It is a **post-pass**, not per-language
//! extraction, and that is the point: by the time it runs every symbol in a
//! file is available, so a rule can look at the symbol's own annotations, the
//! annotations on its declaring type, its supertypes and its call sites
//! without any language module knowing that "boundary" is a concept. Adding a
//! framework is one row in [`RULES`] plus one test.
//!
//! ## What counts
//!
//! Inbound is a way *in*: an HTTP endpoint, a queue listener, a CLI command,
//! a scheduled job. Outbound is a way *out*: an HTTP client, a database call,
//! a queue producer.
//!
//! Outbound detection is call-site based and therefore fuzzier than inbound,
//! so it is deliberately narrow. A rule fires on a curated *client type*
//! (`RestTemplate`, `JdbcTemplate`, `KafkaTemplate`) rather than on a verb
//! like `save` or `send`, and a service method that calls a repository
//! interface is **not** tagged — the repository is where the system actually
//! reaches the database, and tagging every transitive caller would make
//! `boundary_out` mean "touches business logic".

use crate::types::{Annotation, Boundary, BoundaryDirection, CallRef, Symbol};
use std::collections::HashMap;

/// Cap on boundaries recorded for one symbol.
///
/// A route-registration function (`app.get(..)` twenty times in one
/// `setupRoutes`) is a real and common shape, and every one of those routes
/// is worth having. But the tags ride along on every node in `graph.json` and
/// in the store, so an unbounded list would let one pathological function
/// dominate the graph's size.
const MAX_PER_SYMBOL: usize = 12;

/// One framework rule. The extension seam — see the module docs.
#[derive(Debug)]
pub struct Rule {
    /// Language this applies to, as the indexer names it (`java`, `python`,
    /// `typescript`, `rust`). `""` means every language.
    pub lang: &'static str,
    /// Stable id, recorded on the tag as [`Boundary::source`]. Must be
    /// unique across [`RULES`]; a test enforces it.
    pub id: &'static str,
    pub match_on: Match,
    pub kind: &'static str,
    pub direction: BoundaryDirection,
    pub protocol: &'static str,
    pub detail: Detail,
}

/// What makes a symbol a boundary.
#[derive(Debug)]
pub enum Match {
    /// A declaration-site annotation / decorator / attribute on the symbol.
    ///
    /// The pattern is either an exact simple name (`GetMapping`) or a
    /// `*.suffix` wildcard (`*.route`), which exists because a Flask route is
    /// written against whatever the app object happens to be called —
    /// `@app.route`, `@bp.route`, `@api.route` are the same framework.
    Annotation(&'static str),
    /// An annotation on the type that declares this symbol. Lets a rule say
    /// "a `@Get` method, but only inside an `@Controller`".
    OwnerAnnotation(&'static str),
    /// The symbol extends or implements this type. Generic arguments are
    /// stripped before matching, so `JpaRepository` matches
    /// `JpaRepository<Order, Long>`.
    Supertype(&'static str),
    /// The symbol calls this.
    ///
    /// `owner: Some(ty)` matches the receiver's resolved type and is the
    /// precise form. `owner: None` falls back to bare callee names, which is
    /// all a language with weak receiver typing can offer — useful, but it
    /// will match any function of that name, so keep the name distinctive.
    /// `name: "*"` matches any member of the owner type.
    Callee {
        owner: Option<&'static str>,
        name: &'static str,
    },
    /// A process entry point: a free function with this name.
    EntryFn(&'static str),
    /// The indexer already composed an HTTP route for this symbol.
    ///
    /// One row instead of twelve. `Symbol::route` is set precisely when a
    /// Spring `@*Mapping` or a JAX-RS verb annotation resolved, so restating
    /// that annotation list here would be a second copy of a table
    /// `languages/java.rs` already owns and would drift from it.
    HasRoute,
    /// Every sub-match must hit. The first sub-match supplies the hit that
    /// [`Detail`] reads its text from.
    All(&'static [Match]),
}

/// Where the tag's human-facing [`Boundary::detail`] comes from.
#[derive(Debug)]
pub enum Detail {
    None,
    /// The route the indexer already composed — `GET /api/orders/{id}`.
    Route,
    /// Named arguments of the matched annotation, first one that is present.
    /// `@JmsListener(destination = "orders.q")` with `["destination"]`.
    AnnotationArg(&'static [&'static str]),
    /// The matched call's first string literal — the Express/axum path.
    CalleeArg,
}

/// What a [`Match`] hit on, so [`Detail`] can read text out of it.
enum Hit<'a> {
    Ann(&'a Annotation),
    Call(&'a CallRef),
    Bare,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

pub const RULES: &[Rule] = &[
    // ── Java: inbound ──────────────────────────────────────────────────
    Rule {
        lang: "java",
        id: "http.route",
        match_on: Match::HasRoute,
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::Route,
    },
    Rule {
        lang: "java",
        id: "jms.JmsListener",
        match_on: Match::Annotation("JmsListener"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "jms",
        detail: Detail::AnnotationArg(&["destination", "value"]),
    },
    Rule {
        lang: "java",
        id: "kafka.KafkaListener",
        match_on: Match::Annotation("KafkaListener"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "kafka",
        detail: Detail::AnnotationArg(&["topics", "value"]),
    },
    Rule {
        lang: "java",
        id: "amqp.RabbitListener",
        match_on: Match::Annotation("RabbitListener"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "amqp",
        detail: Detail::AnnotationArg(&["queues", "value"]),
    },
    Rule {
        lang: "java",
        id: "sqs.SqsListener",
        match_on: Match::Annotation("SqsListener"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "sqs",
        detail: Detail::AnnotationArg(&["value"]),
    },
    Rule {
        lang: "java",
        id: "jms.MessageListener",
        match_on: Match::Supertype("MessageListener"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "jms",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "spring.Scheduled",
        match_on: Match::Annotation("Scheduled"),
        kind: "scheduled.job",
        direction: BoundaryDirection::Inbound,
        protocol: "schedule",
        detail: Detail::AnnotationArg(&["cron", "fixedRate", "fixedDelay"]),
    },
    Rule {
        lang: "java",
        id: "servlet.WebServlet",
        match_on: Match::Annotation("WebServlet"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["urlPatterns", "value"]),
    },
    Rule {
        lang: "java",
        id: "stomp.MessageMapping",
        match_on: Match::Annotation("MessageMapping"),
        kind: "ws.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "websocket",
        detail: Detail::AnnotationArg(&["value"]),
    },
    Rule {
        lang: "java",
        id: "java.main",
        match_on: Match::EntryFn("main"),
        kind: "cli.command",
        direction: BoundaryDirection::Inbound,
        protocol: "cli",
        detail: Detail::None,
    },
    // ── Java: outbound ─────────────────────────────────────────────────
    Rule {
        lang: "java",
        id: "feign.FeignClient",
        match_on: Match::Annotation("FeignClient"),
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["url", "name", "value"]),
    },
    Rule {
        lang: "java",
        id: "spring.RestTemplate",
        match_on: Match::Callee {
            owner: Some("RestTemplate"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "spring.WebClient",
        match_on: Match::Callee {
            owner: Some("WebClient"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "jdk.HttpClient",
        match_on: Match::Callee {
            owner: Some("HttpClient"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "spring.JdbcTemplate",
        match_on: Match::Callee {
            owner: Some("JdbcTemplate"),
            name: "*",
        },
        kind: "db.access",
        direction: BoundaryDirection::Outbound,
        protocol: "jdbc",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "jpa.EntityManager",
        match_on: Match::Callee {
            owner: Some("EntityManager"),
            name: "*",
        },
        kind: "db.access",
        direction: BoundaryDirection::Outbound,
        protocol: "jpa",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "jpa.Repository",
        match_on: Match::Supertype("JpaRepository"),
        kind: "db.access",
        direction: BoundaryDirection::Outbound,
        protocol: "jpa",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "spring.CrudRepository",
        match_on: Match::Supertype("CrudRepository"),
        kind: "db.access",
        direction: BoundaryDirection::Outbound,
        protocol: "jpa",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "kafka.KafkaTemplate",
        match_on: Match::Callee {
            owner: Some("KafkaTemplate"),
            name: "*",
        },
        kind: "mq.producer",
        direction: BoundaryDirection::Outbound,
        protocol: "kafka",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "jms.JmsTemplate",
        match_on: Match::Callee {
            owner: Some("JmsTemplate"),
            name: "*",
        },
        kind: "mq.producer",
        direction: BoundaryDirection::Outbound,
        protocol: "jms",
        detail: Detail::None,
    },
    Rule {
        lang: "java",
        id: "amqp.RabbitTemplate",
        match_on: Match::Callee {
            owner: Some("RabbitTemplate"),
            name: "*",
        },
        kind: "mq.producer",
        direction: BoundaryDirection::Outbound,
        protocol: "amqp",
        detail: Detail::None,
    },
    // ── Python: inbound ────────────────────────────────────────────────
    //
    // The `*.` wildcards are load-bearing here: Flask and FastAPI routes are
    // declared against whatever the app or blueprint object is named, and
    // `@app.route`, `@bp.route` and `@api.route` are the same framework.
    Rule {
        lang: "python",
        id: "py.route",
        match_on: Match::Annotation("*.route"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path", "rule"]),
    },
    Rule {
        lang: "python",
        id: "py.get",
        match_on: Match::Annotation("*.get"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "python",
        id: "py.post",
        match_on: Match::Annotation("*.post"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "python",
        id: "py.put",
        match_on: Match::Annotation("*.put"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "python",
        id: "py.delete",
        match_on: Match::Annotation("*.delete"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "python",
        id: "py.patch",
        match_on: Match::Annotation("*.patch"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "python",
        id: "click.command",
        match_on: Match::Annotation("*.command"),
        kind: "cli.command",
        direction: BoundaryDirection::Inbound,
        protocol: "cli",
        detail: Detail::AnnotationArg(&["name"]),
    },
    Rule {
        lang: "python",
        id: "celery.task",
        match_on: Match::Annotation("*.task"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "celery",
        detail: Detail::AnnotationArg(&["name"]),
    },
    Rule {
        lang: "python",
        id: "celery.shared_task",
        match_on: Match::Annotation("shared_task"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "celery",
        detail: Detail::None,
    },
    // ── Python: outbound ───────────────────────────────────────────────
    Rule {
        lang: "python",
        id: "py.requests",
        match_on: Match::Callee {
            owner: Some("requests"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::None,
    },
    Rule {
        lang: "python",
        id: "py.httpx",
        match_on: Match::Callee {
            owner: Some("httpx"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::None,
    },
    // ── TypeScript: inbound ────────────────────────────────────────────
    //
    // NestJS verb decorators are bare (`@Get()`), and `Get` is a plausible
    // name for anything, so these require the class to be an `@Controller`.
    // The Java rules need no such guard because `@GetMapping` is unambiguous
    // on its own.
    Rule {
        lang: "typescript",
        id: "nest.Get",
        match_on: Match::All(&[
            Match::Annotation("Get"),
            Match::OwnerAnnotation("Controller"),
        ]),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "typescript",
        id: "nest.Post",
        match_on: Match::All(&[
            Match::Annotation("Post"),
            Match::OwnerAnnotation("Controller"),
        ]),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "typescript",
        id: "nest.Put",
        match_on: Match::All(&[
            Match::Annotation("Put"),
            Match::OwnerAnnotation("Controller"),
        ]),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "typescript",
        id: "nest.Delete",
        match_on: Match::All(&[
            Match::Annotation("Delete"),
            Match::OwnerAnnotation("Controller"),
        ]),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "typescript",
        id: "nest.MessagePattern",
        match_on: Match::Annotation("MessagePattern"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "nest",
        detail: Detail::AnnotationArg(&["cmd"]),
    },
    Rule {
        lang: "typescript",
        id: "nest.EventPattern",
        match_on: Match::Annotation("EventPattern"),
        kind: "mq.listener",
        direction: BoundaryDirection::Inbound,
        protocol: "nest",
        detail: Detail::AnnotationArg(&["cmd"]),
    },
    // Express is deliberately absent. It registers routes by calling
    // `app.get('/users', h)`, and the only matcher that could reach it here
    // is a bare callee name — which would also match `map.get(k)` and
    // `cache.get(id)` and tag half a TypeScript codebase as HTTP endpoints.
    // `CallRef` records the receiver's resolved *type*, not the identifier
    // `app`, so there is no precise form available; a missing boundary is
    // recoverable, a graph full of invented ones is not.
    //
    // ── TypeScript: outbound ───────────────────────────────────────────
    Rule {
        lang: "typescript",
        id: "ts.fetch",
        match_on: Match::Callee {
            owner: None,
            name: "fetch",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::CalleeArg,
    },
    Rule {
        lang: "typescript",
        id: "ts.axios",
        match_on: Match::Callee {
            owner: Some("axios"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::CalleeArg,
    },
    // ── Rust: inbound ──────────────────────────────────────────────────
    Rule {
        lang: "rust",
        id: "actix.get",
        match_on: Match::Annotation("get"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "rust",
        id: "actix.post",
        match_on: Match::Annotation("post"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "rust",
        id: "actix.put",
        match_on: Match::Annotation("put"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    Rule {
        lang: "rust",
        id: "actix.delete",
        match_on: Match::Annotation("delete"),
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::AnnotationArg(&["path"]),
    },
    // axum builds its router by calling, so the handler is named at the
    // registration site rather than decorated at its own.
    Rule {
        lang: "rust",
        id: "axum.route",
        match_on: Match::Callee {
            owner: Some("Router"),
            name: "route",
        },
        kind: "http.endpoint",
        direction: BoundaryDirection::Inbound,
        protocol: "http",
        detail: Detail::CalleeArg,
    },
    Rule {
        lang: "rust",
        id: "rust.main",
        match_on: Match::EntryFn("main"),
        kind: "cli.command",
        direction: BoundaryDirection::Inbound,
        protocol: "cli",
        detail: Detail::None,
    },
    Rule {
        lang: "rust",
        id: "clap.command",
        match_on: Match::Annotation("command"),
        kind: "cli.command",
        direction: BoundaryDirection::Inbound,
        protocol: "cli",
        detail: Detail::AnnotationArg(&["name"]),
    },
    // ── Rust: outbound ─────────────────────────────────────────────────
    Rule {
        lang: "rust",
        id: "reqwest.Client",
        match_on: Match::Callee {
            owner: Some("Client"),
            name: "*",
        },
        kind: "http.client",
        direction: BoundaryDirection::Outbound,
        protocol: "http",
        detail: Detail::None,
    },
];

// ---------------------------------------------------------------------------
// The post-pass
// ---------------------------------------------------------------------------

/// Tag every symbol in one file with the boundaries it represents.
///
/// Two passes rather than one because a rule may need to read a *sibling*
/// symbol (the declaring type's annotations) while we are writing to the
/// symbol in hand.
pub fn annotate(language: &str, symbols: &mut [Symbol]) {
    if symbols.is_empty() {
        return;
    }

    // Owner lookup by qualified name. Only languages that assign qualified
    // names populate this; the rest simply never match `OwnerAnnotation`.
    let by_qname: HashMap<&str, usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.qualified_name.as_deref().map(|q| (q, i)))
        .collect();

    let found: Vec<Vec<Boundary>> = symbols
        .iter()
        .map(|sym| {
            let owner = sym
                .owner
                .as_deref()
                .and_then(|o| by_qname.get(o))
                .map(|&i| &symbols[i]);
            boundaries_for(language, sym, owner)
        })
        .collect();

    for (sym, b) in symbols.iter_mut().zip(found) {
        sym.boundaries = b;
    }
}

/// Every boundary one symbol represents, deduplicated and capped.
fn boundaries_for(language: &str, sym: &Symbol, owner: Option<&Symbol>) -> Vec<Boundary> {
    let mut out: Vec<Boundary> = Vec::new();

    for rule in RULES {
        if !rule.lang.is_empty() && rule.lang != language {
            continue;
        }
        for hit in hits(&rule.match_on, sym, owner) {
            let b = Boundary {
                kind: rule.kind.to_string(),
                direction: rule.direction,
                protocol: rule.protocol.to_string(),
                detail: detail_for(&rule.detail, &hit, sym),
                source: rule.id.to_string(),
            };
            // Two rules can legitimately describe the same surface (a JAX-RS
            // handler that also implements a listener interface); the same
            // rule can fire once per call site. Identity is the tag itself.
            if !out.contains(&b) && out.len() < MAX_PER_SYMBOL {
                out.push(b);
            }
        }
        if out.len() >= MAX_PER_SYMBOL {
            break;
        }
    }
    out
}

/// Every way `m` matches `sym`. Empty when it does not match at all.
fn hits<'a>(m: &Match, sym: &'a Symbol, owner: Option<&'a Symbol>) -> Vec<Hit<'a>> {
    match m {
        Match::Annotation(pat) => sym
            .annotations
            .iter()
            .filter(|a| ann_matches(pat, &a.name))
            .map(Hit::Ann)
            .collect(),

        Match::OwnerAnnotation(pat) => owner
            .map(|o| {
                o.annotations
                    .iter()
                    .filter(|a| ann_matches(pat, &a.name))
                    .map(Hit::Ann)
                    .collect()
            })
            .unwrap_or_default(),

        Match::Supertype(want) => {
            let found = sym
                .extends
                .iter()
                .chain(sym.implements.iter())
                .any(|s| simple_name(s) == *want);
            if found {
                vec![Hit::Bare]
            } else {
                vec![]
            }
        }

        Match::Callee { owner: ty, name } => match ty {
            Some(want) => sym
                .call_refs
                .iter()
                .filter(|c| {
                    (*name == "*" || c.name == *name)
                        && c.owner_type
                            .as_deref()
                            .is_some_and(|o| simple_name(o) == *want)
                })
                .map(Hit::Call)
                .collect(),
            // No receiver type to check, so match on the bare callee name.
            // `calls` is the deduped display list and is populated by every
            // language, including ones whose `call_refs` carry no types.
            None => {
                let refs: Vec<Hit> = sym
                    .call_refs
                    .iter()
                    .filter(|c| c.name == *name)
                    .map(Hit::Call)
                    .collect();
                if refs.is_empty() && sym.calls.iter().any(|c| c == *name) {
                    vec![Hit::Bare]
                } else {
                    refs
                }
            }
        },

        Match::EntryFn(want) => {
            let is_fn = sym.kind == "function";
            if is_fn && simple_name(&sym.name) == *want {
                vec![Hit::Bare]
            } else {
                vec![]
            }
        }

        Match::HasRoute => match sym.route.as_deref().filter(|r| !r.is_empty()) {
            Some(_) => vec![Hit::Bare],
            None => vec![],
        },

        Match::All(parts) => {
            let Some((first, rest)) = parts.split_first() else {
                return vec![];
            };
            if rest.iter().any(|p| hits(p, sym, owner).is_empty()) {
                return vec![];
            }
            // The first sub-match is what `Detail` reads from.
            hits(first, sym, owner)
        }
    }
}

fn detail_for(d: &Detail, hit: &Hit, sym: &Symbol) -> Option<String> {
    let text = match d {
        Detail::None => return None,
        Detail::Route => sym.route.clone(),
        Detail::AnnotationArg(keys) => match hit {
            Hit::Ann(a) => a.args.as_deref().and_then(|args| {
                // Java's parser first: it understands `key = "value"`, which
                // is the form that carries the name when there are several
                // arguments. Its literal scan is double-quote only, so the
                // fallback covers the languages that quote differently —
                // `@Get(':id')` in TypeScript, `@app.route('/x')` in Python.
                crate::indexer::languages::java::named_or_first_string(args, keys)
                    .or_else(|| first_quoted(args))
            }),
            _ => None,
        },
        Detail::CalleeArg => match hit {
            Hit::Call(c) => c.first_string_arg.clone(),
            _ => None,
        },
    };
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The first quoted run in some argument text, whichever quote style it uses.
///
/// Deliberately naive — no escape handling — because the strings this reaches
/// for are route paths, queue names and cron expressions, none of which
/// contain an escaped quote. Java's richer parser runs first and handles the
/// cases that need it.
fn first_quoted(args: &str) -> Option<String> {
    let (i, quote) = args
        .char_indices()
        .find(|(_, c)| matches!(c, '"' | '\'' | '`'))?;
    let rest = &args[i + quote.len_utf8()..];
    let end = rest.find(quote)?;
    let text = &rest[..end];
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Match an annotation pattern against an annotation's name.
///
/// `*.suffix` matches any dotted name ending in `.suffix`; anything else is
/// an exact match. See [`Match::Annotation`] for why the wildcard exists.
fn ann_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => name
            .rsplit_once('.')
            .is_some_and(|(_, last)| last == suffix),
        None => name == pattern,
    }
}

/// Last path segment of a possibly-qualified, possibly-generic type name.
/// `org.acme.JpaRepository<Order, Long>` → `JpaRepository`.
fn simple_name(raw: &str) -> &str {
    let base = raw.split('<').next().unwrap_or(raw).trim();
    let base = base.rsplit("::").next().unwrap_or(base);
    base.rsplit('.').next().unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for r in RULES {
            assert!(seen.insert(r.id), "duplicate rule id {}", r.id);
        }
    }

    #[test]
    fn every_rule_names_a_real_language() {
        // `""` is the wildcard. Everything else must be a language the
        // indexer actually reports, or the rule can never fire and nobody
        // would ever find out.
        const KNOWN: &[&str] = &["", "java", "python", "typescript", "javascript", "rust"];
        for r in RULES {
            assert!(KNOWN.contains(&r.lang), "unknown lang {:?} on {}", r.lang, r.id);
        }
    }

    #[test]
    fn every_kind_is_a_dotted_family_role() {
        for r in RULES {
            let (family, role) = r
                .kind
                .split_once('.')
                .unwrap_or_else(|| panic!("kind {:?} on {} is not family.role", r.kind, r.id));
            assert!(
                !family.is_empty() && !role.is_empty(),
                "kind {:?} on {} has an empty half",
                r.kind,
                r.id
            );
            assert!(
                r.kind.chars().all(|c| c.is_ascii_lowercase() || c == '.'),
                "kind {:?} on {} must be lowercase",
                r.kind,
                r.id
            );
            assert!(!r.protocol.is_empty(), "{} has no protocol", r.id);
        }
    }

    #[test]
    fn a_wildcard_annotation_matches_any_receiver() {
        assert!(ann_matches("*.route", "app.route"));
        assert!(ann_matches("*.route", "bp.route"));
        assert!(!ann_matches("*.route", "route"));
        assert!(!ann_matches("*.route", "app.get"));
        assert!(ann_matches("GetMapping", "GetMapping"));
        assert!(!ann_matches("GetMapping", "PostMapping"));
    }

    #[test]
    fn simple_name_strips_packages_and_generics() {
        assert_eq!(simple_name("org.acme.JpaRepository<Order, Long>"), "JpaRepository");
        assert_eq!(simple_name("crate::http::Client"), "Client");
        assert_eq!(simple_name("RestTemplate"), "RestTemplate");
    }
}
