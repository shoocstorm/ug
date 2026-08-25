//! `context — the curated bundle` — agent tool.

use super::*;

/// The roles a [`ContextItem`] can carry, in the order they are assembled,
/// budgeted and rendered.
///
/// The order is a priority claim, not a taxonomy. Working on a symbol, an
/// agent needs its body before anything else; then who breaks if it changes;
/// then what proves it still works; then what it leans on; then the prose.
/// When the budget runs out it runs out from the right.
pub const CONTEXT_ROLES: &[&str] = &["target", "caller", "test", "dependency", "doc"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContextParams {
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    #[serde(alias = "maxChars")]
    pub max_chars: Option<usize>,
    /// Keep only these roles. Empty means all of [`CONTEXT_ROLES`].
    #[serde(deserialize_with = "de_one_or_many")]
    pub include: Vec<String>,
}

/// Default budget for one pack.
///
/// Deliberately well under `get_code`'s 20k: the point of this tool is to be
/// cheaper than the five calls it replaces, and a pack that costs more than
/// `get_code` alone would be a worse deal dressed as a better one.
const CONTEXT_DEFAULT_MAX_CHARS: usize = 12_000;

/// Ceiling on the share of the budget the target's own body may take, so one
/// enormous function cannot crowd out every caller and test — the parts that
/// answer questions the body cannot.
const CONTEXT_TARGET_SHARE: f64 = 0.5;

const CONTEXT_CALLERS_CAP: usize = 12;
const CONTEXT_TESTS_CAP: usize = 8;
const CONTEXT_DEPS_CAP: usize = 15;
const CONTEXT_DOCS_CAP: usize = 5;

/// How much prose a `doc` item carries. Enough to tell whether the section is
/// the one you want; `get_code` on its id gives the rest.
const CONTEXT_DOC_PREVIEW_CHARS: usize = 400;

/// Rendering overhead every pack pays regardless of contents: the header, the
/// id line, the budget line, the section rules and the trailing hint.
///
/// Charged to the budget up front rather than ignored, because `max_chars`
/// has to bound what the caller actually receives. Counting only the payload
/// made a 500-char pack return 894 — a 79% overshoot, worst exactly when the
/// caller is being careful about tokens.
const CONTEXT_CHROME_RESERVE: usize = 260;

/// Per-item rendering overhead: the `- Type Name  file:line` bullet, the
/// `id:` line beneath it, and the indentation around the `why` and call-site
/// lines.
const CONTEXT_ITEM_CHROME: usize = 26;

#[derive(Debug, Clone, Serialize)]
pub struct ContextItem {
    /// One of [`CONTEXT_ROLES`] — why this item is in the pack.
    ///
    /// The label is the feature. A bundle of related symbols with no stated
    /// reason for each is a pile an agent has to re-derive; labelled, it can
    /// drop the half it does not need without a second round trip.
    pub role: &'static str,
    /// The specific relationship, e.g. `calls the target` or `tested at 2 hops`.
    pub why: String,
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// Present for `target` (its body) and `doc` (its prose). Callers,
    /// dependencies and tests travel as signatures plus evidence — their
    /// bodies are a `get_code` away and would blow the budget here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSite>,
    /// Characters cut from this item to stay inside the budget.
    #[serde(skip_serializing_if = "is_zero")]
    pub truncated_chars: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Items left out, per role — because the budget ran out or the per-role cap
/// was hit. Not distinguished, because the caller's move is the same either
/// way: raise `max_chars`, or narrow `include` and ask again.
#[derive(Debug, Clone, Serialize)]
pub struct ContextDropped {
    pub role: &'static str,
    pub count: usize,
}

/// What one item will cost the budget once rendered.
///
/// Counted from the [`SymbolRef`] that actually gets emitted rather than from
/// the node, because the preview fields ride along with it — a doc preview is
/// up to [`DOC_PREVIEW_CHARS`] per item, and 15 dependencies' worth of it is a
/// quarter of the default budget. Costing the node's bare name instead is how
/// a "12000 char" pack quietly returns 18000.
fn context_item_cost(symbol: &SymbolRef, why: &str, extra: usize) -> usize {
    CONTEXT_ITEM_CHROME
        + why.len()
        + symbol.id.len()
        + symbol.name.len()
        + symbol.node_type.len()
        + symbol.file.as_deref().map_or(0, str::len)
        + symbol.doc.as_deref().map_or(0, str::len)
        + symbol.boundary.as_deref().map_or(0, str::len)
        + extra
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextResult {
    /// The reference as the caller wrote it.
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub items: Vec<ContextItem>,
    pub max_chars: usize,
    /// Characters spent, chrome included — the number to shrink `max_chars`
    /// against.
    ///
    /// Close but not exact: assembly and rendering are separate steps (the
    /// JSON envelope has no chrome at all), so the renderer's fixed overhead
    /// is charged as an estimate rather than measured. At the default budget
    /// the pack lands under `max_chars`; at very small ones it can run over
    /// by a couple of hundred characters. Treat it as a budget, not a
    /// guarantee.
    pub used_chars: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<ContextDropped>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ContextResult {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    fn failed(query: &str, max_chars: usize, error: String) -> Self {
        ContextResult {
            query: query.to_string(),
            target: None,
            items: Vec::new(),
            max_chars,
            used_chars: 0,
            dropped: Vec::new(),
            notes: Vec::new(),
            error: Some(error),
        }
    }

    fn count(&self, role: &str) -> usize {
        self.items.iter().filter(|i| i.role == role).count()
    }
}

/// Running character budget for one pack.
struct ContextBudget {
    max: usize,
    used: usize,
}

impl ContextBudget {
    fn left(&self) -> usize {
        self.max.saturating_sub(self.used)
    }

    /// Spend up to `want`, returning what was actually granted.
    fn take(&mut self, want: usize) -> usize {
        let granted = want.min(self.left());
        self.used += granted;
        granted
    }
}

/// Everything an agent needs to change one symbol safely, in one call.
///
/// Replaces the 4–6 round trips an agent otherwise spends assembling the same
/// picture — `get_code` → `find_usages` → `traverse` → `test_for` → read the
/// doc — with a single token-budgeted bundle whose every entry says why it is
/// there. No new analysis: this is assembly and budgeting over the existing
/// graph tools, which is why it needs neither an embedder nor the store.
///
/// Composition, in budget priority order:
///
/// - **target** — the symbol's own source, capped at [`CONTEXT_TARGET_SHARE`]
///   of the budget, read through [`get_code`] so it inherits the live-file
///   preference and the stale-span flagging.
/// - **caller** — direct (1-hop) inbound users with their call sites, from
///   [`find_usages`]. Signatures and evidence, not bodies.
/// - **test** — inbound users within 2 hops that [`is_test_node`] recognises.
///   Classified before callers, so a test that calls the target is listed once,
///   as a test: "who breaks" and "what re-verifies" are different questions and
///   the same node answering both would read as two callers.
/// - **dependency** — 1-hop outbound neighbours, signatures only, minus
///   `Contains` (the file that holds the symbol is not something it depends on).
/// - **doc** — `Concept` nodes adjacent in either direction, i.e. prose the
///   indexer linked to this symbol, with a preview.
///
/// [`is_test_node`]: crate::storage::facts::is_test_node
pub fn context(graph: &GraphData, src: SourceCtx, p: &ContextParams) -> ContextResult {
    let max_chars = p.max_chars.unwrap_or(CONTEXT_DEFAULT_MAX_CHARS);
    let query = p.node_id.first().cloned().unwrap_or_default();

    if query.trim().is_empty() {
        return ContextResult::failed(
            &query,
            max_chars,
            "context needs a symbol — a node id, an exact name, or a wildcard that matches exactly one symbol.".to_string(),
        );
    }

    let wants = |role: &str| {
        p.include.is_empty() || p.include.iter().any(|r| r.eq_ignore_ascii_case(role))
    };

    // One symbol, one budget. A pack is a claim about *this* symbol's
    // neighbourhood; batching several would silently divide the budget between
    // them and produce a thin, misleading answer for each.
    let target_id = match resolve_single_ref(graph, &query) {
        Ok(id) => id,
        Err(e) => return ContextResult::failed(&query, max_chars, e),
    };
    let by_id = by_id_map(graph);
    let Some(target_node) = by_id.get(target_id.as_str()).copied() else {
        return ContextResult::failed(
            &query,
            max_chars,
            unresolved_ref_error(graph, &query, MAX_REF_EXPANSION),
        );
    };

    let mut out = ContextResult {
        query: query.clone(),
        target: Some(SymbolRef::from_node(target_node)),
        items: Vec::new(),
        max_chars,
        used_chars: 0,
        dropped: Vec::new(),
        notes: Vec::new(),
        error: None,
    };
    if p.node_id.len() > 1 {
        out.notes.push(format!(
            "context takes one symbol; using {} and ignoring the other {}.",
            query,
            p.node_id.len() - 1
        ));
    }
    // A misspelled role filters everything out, and an empty pack with no
    // explanation reads as "this symbol has no callers" — the confidently
    // wrong answer this whole tool exists to avoid. `--include callers` is the
    // obvious way to get this wrong, since every section heading is plural.
    let unknown: Vec<&str> = p
        .include
        .iter()
        .map(String::as_str)
        .filter(|r| !CONTEXT_ROLES.iter().any(|k| k.eq_ignore_ascii_case(r)))
        .collect();
    if !unknown.is_empty() {
        out.notes.push(format!(
            "unknown role(s) in include: {} — valid roles are {}. Nothing was kept for them.",
            unknown.join(", "),
            CONTEXT_ROLES.join(", ")
        ));
    }

    // Seeded with the chrome rather than deducted from `max`, so `used_chars`
    // reports what the caller will actually receive and can be compared
    // directly against the `max_chars` they asked for.
    let mut budget = ContextBudget {
        max: max_chars,
        used: CONTEXT_CHROME_RESERVE.min(max_chars),
    };
    let mut dropped: Vec<(&'static str, usize)> = Vec::new();

    // ── target ─────────────────────────────────────────────────────────
    if wants("target") {
        let share = ((max_chars as f64) * CONTEXT_TARGET_SHARE) as usize;
        let allowance = share.min(budget.left());
        let got = get_code(
            graph,
            src,
            &GetCodeParams {
                node_id: vec![target_id.clone()],
                file: None,
                start_line: None,
                end_line: None,
                range: None,
                max_chars: Some(allowance),
                no_doc: false,
            },
        );
        for slice in got.slices {
            if let Some(err) = slice.error {
                out.notes.push(format!("target source unavailable: {}", err));
                continue;
            }
            // `get_code` flags a span it could not trust; that warning is
            // about the whole pack, since every line number below was read
            // from the same index.
            if let Some(stale) = slice.stale {
                out.notes.push(stale);
            }
            let code = slice.code.unwrap_or_default();
            let why = "the symbol you asked about".to_string();
            let mut symbol = SymbolRef::from_node(target_node);
            // `get_code` returns the doc comment separately from the body, and
            // it is the single most useful piece of prose about the symbol.
            // Dropping it would make `context` strictly worse than the
            // `get_code` it is meant to replace — prefer the full comment
            // `get_code` recovered over the node's truncated preview.
            if let Some(doc) = slice.doc.filter(|d| !d.trim().is_empty()) {
                symbol.doc = Some(doc);
            }
            budget.take(context_item_cost(&symbol, &why, code.len()));
            out.items.push(ContextItem {
                role: "target",
                why,
                symbol,
                code: Some(code),
                call_sites: Vec::new(),
                truncated_chars: slice.truncated_chars,
            });
        }
    }

    // ── callers and tests: one inbound walk, split by role ──────────────
    if wants("caller") || wants("test") {
        let usages = find_usages(
            graph,
            src,
            &FindUsagesParams {
                node_id: vec![target_id.clone()],
                // 2, because `test_for` walks 1..2: a test usually reaches the
                // symbol directly or through one helper.
                hops: Some(2),
                edge_types: Vec::new(),
            },
        );
        let (mut callers, mut tests) = (0usize, 0usize);
        for entry in &usages.nodes {
            for user in &entry.users {
                let Some(node) = by_id.get(user.symbol.id.as_str()).copied() else {
                    continue;
                };
                // A `Concept` pointing at the symbol is prose about it, not
                // code that breaks when it changes. It is a real inbound edge,
                // so `find_usages` returns it — but here it belongs to the
                // `doc` role, and counting it as a caller would both overstate
                // the blast radius and list the same node twice.
                if matches!(node.node_type, GraphNodeType::Concept) {
                    continue;
                }
                let is_test = crate::storage::facts::is_test_node(node);
                // Depth-2 non-tests are dropped on purpose: they do not
                // mention the target by name, so they carry no evidence and
                // would pad the pack with plausible-looking noise.
                if !is_test && user.depth > 1 {
                    continue;
                }
                let (role, cap, seen) = if is_test {
                    ("test", CONTEXT_TESTS_CAP, &mut tests)
                } else {
                    ("caller", CONTEXT_CALLERS_CAP, &mut callers)
                };
                if !wants(role) {
                    continue;
                }
                if *seen >= cap {
                    dropped.push((role, 1));
                    continue;
                }
                // Arrow notation, matching `render_find_usages` — the reader
                // should not have to learn a second spelling of the same edge.
                let why = match (is_test, user.depth) {
                    (true, 1) => format!("test —{}→ target", user.via_edge),
                    (true, d) => format!("test, reaches target in {} hops", d),
                    (false, _) => format!("this —{}→ target", user.via_edge),
                };
                let cost = context_item_cost(
                    &user.symbol,
                    &why,
                    user.call_sites.iter().map(|c| c.text.len() + 8).sum(),
                );
                if budget.left() < cost {
                    dropped.push((role, 1));
                    continue;
                }
                budget.take(cost);
                *seen += 1;
                out.items.push(ContextItem {
                    role,
                    why,
                    symbol: user.symbol.clone(),
                    code: None,
                    call_sites: user.call_sites.clone(),
                    truncated_chars: 0,
                });
            }
        }
    }

    // ── dependencies and docs: one outbound/incident pass ───────────────
    if wants("dependency") || wants("doc") {
        let mut deps: Vec<(&GraphNode, &'static str)> = Vec::new();
        let mut docs: Vec<(&GraphNode, &'static str)> = Vec::new();
        let mut seen_ids: Vec<&str> = vec![target_id.as_str()];
        for e in &graph.edges {
            let et = edge_type_str(&e.edge_type);
            let other = if &*e.source == target_id.as_str() {
                &*e.target
            } else if &*e.target == target_id.as_str() {
                // Inbound edges are the callers' business, except that a
                // Concept pointing *at* this symbol is documentation of it —
                // which is the direction doc links actually run.
                &*e.source
            } else {
                continue;
            };
            let Some(node) = by_id.get(other).copied() else {
                continue;
            };
            if seen_ids.contains(&node.id.as_str()) {
                continue;
            }
            if matches!(node.node_type, GraphNodeType::Concept) {
                seen_ids.push(node.id.as_str());
                docs.push((node, et));
            } else if &*e.source == target_id.as_str()
                // `Contains` is structure, not dependence: the file that holds
                // a function is not something the function relies on.
                && !matches!(e.edge_type, GraphEdgeType::Contains)
                && !matches!(node.node_type, GraphNodeType::Folder | GraphNodeType::File)
            {
                seen_ids.push(node.id.as_str());
                deps.push((node, et));
            }
        }

        if wants("dependency") {
            for (i, (node, et)) in deps.iter().enumerate() {
                let why = format!("target —{}→ this", et);
                let symbol = SymbolRef::from_node(node);
                let cost = context_item_cost(&symbol, &why, 0);
                if i >= CONTEXT_DEPS_CAP || budget.left() < cost {
                    dropped.push(("dependency", deps.len() - i));
                    break;
                }
                budget.take(cost);
                out.items.push(ContextItem {
                    role: "dependency",
                    why,
                    symbol,
                    code: None,
                    call_sites: Vec::new(),
                    truncated_chars: 0,
                });
            }
        }

        if wants("doc") {
            for (i, (node, _)) in docs.iter().enumerate() {
                let prose: String = node
                    .docstring
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(CONTEXT_DOC_PREVIEW_CHARS)
                    .collect();
                let why = "prose the indexer linked to this symbol".to_string();
                let symbol = SymbolRef::from_node(node);
                let cost = context_item_cost(&symbol, &why, prose.len());
                if i >= CONTEXT_DOCS_CAP || budget.left() < cost {
                    dropped.push(("doc", docs.len() - i));
                    break;
                }
                budget.take(cost);
                out.items.push(ContextItem {
                    role: "doc",
                    why,
                    symbol,
                    code: (!prose.is_empty()).then_some(prose),
                    call_sites: Vec::new(),
                    truncated_chars: 0,
                });
            }
        }
    }

    // Collapse the per-item drops into one count per role, in role order, so
    // the caller sees "6 dependency" rather than six identical lines.
    for role in CONTEXT_ROLES {
        let count: usize = dropped.iter().filter(|(r, _)| r == role).map(|(_, c)| c).sum();
        if count > 0 {
            out.dropped.push(ContextDropped { role, count });
        }
    }
    out.used_chars = budget.used;
    out
}

/// Ids whose captured source a `context` call reads: the target, plus the
/// direct users whose call sites get scanned.
pub fn context_source_ids(graph: &GraphData, p: &ContextParams) -> Vec<String> {
    let Some(query) = p.node_id.first() else {
        return Vec::new();
    };
    let Ok(target_id) = resolve_single_ref(graph, query) else {
        return Vec::new();
    };
    let mut ids = get_code_source_ids(
        graph,
        &GetCodeParams {
            node_id: vec![target_id.clone()],
            file: None,
            start_line: None,
            end_line: None,
            range: None,
            max_chars: None,
            no_doc: false,
        },
    );
    ids.extend(find_usages_source_ids(
        graph,
        &FindUsagesParams {
            node_id: vec![target_id],
            hops: Some(2),
            edge_types: Vec::new(),
        },
    ));
    ids.sort();
    ids.dedup();
    ids
}

/// Plural section heading for a role. Spelled out because appending `s`
/// yields "dependencys".
fn context_role_plural(role: &str) -> &str {
    match role {
        "caller" => "callers",
        "test" => "tests",
        "dependency" => "dependencies",
        "doc" => "docs",
        other => other,
    }
}

pub fn render_context(r: &ContextResult, style: Render) -> String {
    let mut out = String::new();
    if let Some(err) = &r.error {
        line(&mut out, err);
        return out;
    }

    if let Some(t) = &r.target {
        line(
            &mut out,
            &format!(
                "{} {} {}  {}",
                style.heading("Context for"),
                t.node_type,
                style.bold(&t.name),
                t.loc()
            ),
        );
        line(&mut out, &format!("id: {}", style.id(&t.id)));
    }

    let counts: Vec<String> = CONTEXT_ROLES
        .iter()
        .filter(|role| **role != "target")
        .filter_map(|role| {
            let n = r.count(role);
            (n > 0).then(|| format!("{} {}", n, role))
        })
        .collect();
    line(
        &mut out,
        &style.dim(&format!(
            "{} chars of {} budget{}{}",
            r.used_chars,
            r.max_chars,
            if counts.is_empty() {
                String::new()
            } else {
                format!(" · {}", counts.join(", "))
            },
            if r.dropped.is_empty() {
                String::new()
            } else {
                let d: Vec<String> = r
                    .dropped
                    .iter()
                    .map(|d| format!("{} {}", d.count, d.role))
                    .collect();
                format!(" · not shown: {}", d.join(", "))
            }
        )),
    );

    for note in &r.notes {
        line(&mut out, &format!("⚠ {}", note));
    }

    for role in CONTEXT_ROLES {
        let items: Vec<&ContextItem> = r.items.iter().filter(|i| i.role == *role).collect();
        if items.is_empty() {
            continue;
        }
        out.push('\n');
        line(
            &mut out,
            &style.bold(&match *role {
                "target" => "── target ──".to_string(),
                other => format!("── {} ({}) ──", context_role_plural(other), items.len()),
            }),
        );
        for item in items {
            if *role == "target" {
                if let Some(doc) = &item.symbol.doc {
                    line(&mut out, &style.dim(doc));
                }
                if let Some(code) = &item.code {
                    out.push_str(code);
                    if !code.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if item.truncated_chars > 0 {
                    line(
                        &mut out,
                        &style.dim(&format!(
                            "… {} more chars — get_code {} for the whole body",
                            item.truncated_chars, item.symbol.id
                        )),
                    );
                }
                continue;
            }
            item.symbol.render_bullet(&mut out, style);
            line(&mut out, &format!("  {}", style.dim(&item.why)));
            for cs in &item.call_sites {
                line(&mut out, &format!("    {}  {}", cs.line, cs.text));
            }
            if let Some(prose) = &item.code {
                line(&mut out, &format!("    {}", prose.replace('\n', "\n    ")));
            }
        }
    }

    out.push('\n');
    line(
        &mut out,
        &style.dim(
            "Next: get_code <id> for any item's full body · find_usages <id> --hops 2 for transitive users",
        ),
    );
    out
}
