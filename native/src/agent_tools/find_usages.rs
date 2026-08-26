//! `find_usages` — agent tool.

use super::*;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FindUsagesParams {
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Transitive depth, 1-3. Default 1 = direct users only.
    pub hops: Option<u32>,
    /// Defaults to [`USAGE_EDGE_TYPES`].
    #[serde(alias = "edgeTypes", deserialize_with = "de_one_or_many")]
    pub edge_types: Vec<String>,
}

/// A line inside a caller that mentions the subject by name. Heuristic —
/// the name could appear in a comment or string — but each hit is a
/// jumpable `file:line` and saves a `get_code` round-trip per caller.
#[derive(Debug, Clone, Serialize)]
pub struct CallSite {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    #[serde(flatten)]
    pub symbol: SymbolRef,
    /// 1 = direct user of the subject; 2+ = reached transitively.
    pub depth: u32,
    /// Edge type connecting this user to `via_target`.
    pub via_edge: String,
    /// The node this user points at — the subject itself at depth 1.
    pub via_target: String,
    /// Populated for direct (depth 1) users only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSite>,
}

/// How many direct callers get a source scan, and how many matching lines
/// each contributes. Bounds the file reads on a hot symbol with hundreds of
/// callers.
const CALL_SITE_CALLER_CAP: usize = 20;
const CALL_SITE_PER_CALLER: usize = 3;
const CALL_SITE_TEXT_CHARS: usize = 160;

#[derive(Debug, Clone, Serialize)]
pub struct UsagesEntry {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<SymbolRef>,
    pub users: Vec<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindUsagesResult {
    pub hops: u32,
    pub edge_types: Vec<String>,
    pub nodes: Vec<UsagesEntry>,
}

impl FindUsagesResult {
    pub fn ok(&self) -> bool {
        self.nodes.iter().all(|n| n.error.is_none())
    }
}

/// Which direct users get their source scanned for call sites: depth-1,
/// file-bearing, capped.
///
/// Returned as indices so the scan and the pre-fetch that feeds it
/// ([`find_usages_source_ids`]) share one definition — a transport that
/// fetched a different set than the scan reads would silently produce
/// call-site-less results with no way to tell why.
fn call_site_candidates(users: &[Usage]) -> Vec<usize> {
    users
        .iter()
        .enumerate()
        .filter(|(_, u)| u.depth == 1 && u.symbol.file.is_some())
        .map(|(i, _)| i)
        .take(CALL_SITE_CALLER_CAP)
        .collect()
}

/// The caller's source, as lines, plus the 1-based file line its first
/// entry corresponds to.
///
/// Three sources, in order: the caller node's own captured span (exact, and
/// needs nothing on disk), the file's whole-file capture, then the working
/// tree. `files` caches the latter two so several callers in one file cost
/// a single lookup.
fn caller_lines(
    graph: &GraphData,
    src: SourceCtx,
    caller: &SymbolRef,
    file: &str,
    files: &mut HashMap<String, Option<Vec<String>>>,
) -> Option<(Vec<String>, usize)> {
    // The span capture is already exactly the caller's lines, so its first
    // line is the caller's start line and no clamping is needed.
    if let Some(stored) = src.node(&caller.id) {
        let lines: Vec<String> = stored.code.lines().map(|s| s.to_string()).collect();
        if !lines.is_empty() {
            return Some((lines, caller.start_line.unwrap_or(1) as usize));
        }
    }

    let whole = files.entry(file.to_string()).or_insert_with(|| {
        src.file(graph, file)
            .map(|s| s.code.clone())
            .or_else(|| std::fs::read_to_string(src.repo_root().join(file)).ok())
            .map(|c| c.split('\n').map(|s| s.to_string()).collect())
    });
    let whole = whole.as_ref()?;

    let from = caller.start_line.unwrap_or(1).saturating_sub(1) as usize;
    let to = caller
        .end_line
        .map(|e| e as usize)
        .unwrap_or(whole.len())
        .min(whole.len());
    if from >= to {
        return None;
    }
    Some((whole[from..to].to_vec(), from + 1))
}

/// Scan a caller's own source slice for lines mentioning `target_name`.
fn call_sites_for(
    graph: &GraphData,
    src: SourceCtx,
    caller: &SymbolRef,
    target_name: &str,
    files: &mut HashMap<String, Option<Vec<String>>>,
) -> Vec<CallSite> {
    if target_name.is_empty() {
        return vec![];
    }
    let Some(file) = caller.file.clone() else {
        return vec![];
    };
    let Some((lines, first_line)) = caller_lines(graph, src, caller, &file, files) else {
        return vec![];
    };

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.contains(target_name) {
            out.push(CallSite {
                line: first_line + i,
                text: line.trim().chars().take(CALL_SITE_TEXT_CHARS).collect(),
            });
            if out.len() >= CALL_SITE_PER_CALLER {
                break;
            }
        }
    }
    out
}

/// Node ids whose captured source a `find_usages` call will read.
///
/// Transports call this before the tool, fetch the ids from the project's
/// store, and pass the result back in — which is what lets the call-site
/// evidence come from the index rather than the working tree. The walk it
/// repeats is an in-memory pass over `graph.edges`; the alternative, a live
/// store handle inside the tool, would make a synchronous function block
/// inside whichever async runtime called it.
pub fn find_usages_source_ids(graph: &GraphData, p: &FindUsagesParams) -> Vec<String> {
    let walked = walk_usages(graph, p);
    let mut ids = Vec::new();
    for entry in &walked.nodes {
        for i in call_site_candidates(&entry.users) {
            let u = &entry.users[i];
            ids.push(u.symbol.id.clone());
            // The file's whole-file capture backs up any caller whose own
            // span was never captured.
            if let Some(f) = &u.symbol.file {
                ids.extend(whole_file_node_ids(graph, f).into_iter().map(String::from));
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

pub fn find_usages(
    graph: &GraphData,
    src: SourceCtx,
    p: &FindUsagesParams,
) -> FindUsagesResult {
    let mut result = walk_usages(graph, p);
    for entry in &mut result.nodes {
        // Evidence for direct users only: transitive ones don't mention the
        // subject by name, so scanning them would just produce noise.
        let Some(subject_name) = entry.subject.as_ref().map(|s| s.name.clone()) else {
            continue;
        };
        let mut files: HashMap<String, Option<Vec<String>>> = HashMap::new();
        for i in call_site_candidates(&entry.users) {
            let caller = entry.users[i].symbol.clone();
            entry.users[i].call_sites =
                call_sites_for(graph, src, &caller, &subject_name, &mut files);
        }
    }
    result
}

/// The inbound graph walk behind [`find_usages`], with no call sites
/// attached — the part that needs only `graph.json`.
fn walk_usages(graph: &GraphData, p: &FindUsagesParams) -> FindUsagesResult {
    let hops = p.hops.unwrap_or(1).clamp(1, 3);
    let edge_types: Vec<String> = if p.edge_types.is_empty() {
        USAGE_EDGE_TYPES.iter().map(|s| s.to_string()).collect()
    } else {
        p.edge_types.iter().map(|t| t.to_lowercase()).collect()
    };

    let by_id = by_id_map(graph);

    // Inbound adjacency, built once and shared across the batch: edges that
    // *end* at a node — their sources are its users.
    let mut inbound: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    for e in &graph.edges {
        let et = edge_type_str(&e.edge_type);
        // Allocation-free, as in `traverse` — see P11.12.
        if edge_types.iter().any(|t| t.eq_ignore_ascii_case(et)) {
            inbound
                .entry(&*e.target)
                .or_default()
                .push((&*e.source, et));
        }
    }

    let mut nodes = Vec::new();
    for node_id in &expand_node_refs(graph, &p.node_id, MAX_REF_EXPANSION) {
        let Some(subject) = by_id.get(node_id.as_str()) else {
            nodes.push(UsagesEntry {
                query: node_id.clone(),
                subject: None,
                users: vec![],
                error: Some(unresolved_ref_error(graph, node_id, MAX_REF_EXPANSION)),
            });
            continue;
        };

        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(node_id.as_str());
        let mut users: Vec<Usage> = Vec::new();
        let mut frontier: Vec<&str> = vec![node_id.as_str()];
        for depth in 1..=hops {
            let mut next: Vec<&str> = Vec::new();
            for target in &frontier {
                let Some(sources) = inbound.get(target) else {
                    continue;
                };
                for (src, et) in sources {
                    if seen.insert(src) {
                        let symbol = by_id
                            .get(src)
                            .map(|n| SymbolRef::from_node(n))
                            .unwrap_or_else(|| SymbolRef {
                                id: (*src).to_string(),
                                name: "(unknown node)".into(),
                                node_type: "?".into(),
                                file: None,
                                start_line: None,
                                end_line: None,
                                doc: None,
                                boundary: None,
                            });
                        users.push(Usage {
                            symbol,
                            depth,
                            via_edge: (*et).to_string(),
                            via_target: (*target).to_string(),
                            call_sites: vec![],
                        });
                        next.push(src);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }

        nodes.push(UsagesEntry {
            query: node_id.clone(),
            subject: Some(SymbolRef::from_node(subject)),
            users,
            error: None,
        });
    }

    FindUsagesResult {
        hops,
        edge_types,
        nodes,
    }
}

/// "3 of 14 users are system boundaries: 2 http.endpoint, 1 mq.listener."
///
/// `None` when none of them are, so the line only appears when it changes
/// the reader's conclusion.
fn boundary_summary(users: &[Usage]) -> Option<String> {
    let hits: Vec<&str> = users
        .iter()
        .filter_map(|u| u.symbol.boundary.as_deref())
        .collect();
    if hits.is_empty() {
        return None;
    }

    // Count by kind, dropping the direction prefix and the detail — the
    // breakdown is meant to say what sort of contract is at stake, not to
    // re-list every route.
    let mut kinds: Vec<(&str, usize)> = Vec::new();
    for label in &hits {
        for part in label.split(", ") {
            let Some(kind) = part.split_once(':').map(|(_, k)| k) else {
                continue;
            };
            let kind = kind.split(' ').next().unwrap_or(kind);
            match kinds.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => kinds.push((kind, 1)),
            }
        }
    }
    kinds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let breakdown: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();

    Some(format!(
        "⊕ {} of {} user(s) are system boundaries: {}.",
        hits.len(),
        users.len(),
        breakdown.join(", ")
    ))
}

pub fn render_find_usages(r: &FindUsagesResult, style: Render) -> String {
    let mut out = String::new();
    let names: HashMap<&str, &str> = r
        .nodes
        .iter()
        .flat_map(|e| {
            e.users
                .iter()
                .map(|u| (u.symbol.id.as_str(), u.symbol.name.as_str()))
                .chain(e.subject.iter().map(|s| (s.id.as_str(), s.name.as_str())))
        })
        .collect();

    for (i, e) in r.nodes.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(err) = &e.error {
            line(&mut out, &format!("✗ {}", err));
            continue;
        }
        let subject = e.subject.as_ref().expect("subject set when error is none");
        line(
            &mut out,
            &format!(
                "{}  {}",
                style.heading(&format!("Usages of {} {}", subject.node_type, subject.name)),
                style.dim(&subject.loc())
            ),
        );
        line(
            &mut out,
            &style.dim(&format!(
                "hops={} · edges=[{}] · {} user(s)",
                r.hops,
                r.edge_types.join(", "),
                e.users.len()
            )),
        );
        // "What breaks if I change this" is the question this tool claims to
        // answer, and a user count alone cannot: eleven internal callers are
        // a refactor, one REST handler among them is an API change. Called
        // out above the list because it decides whether the list needs
        // reading at all.
        if let Some(summary) = boundary_summary(&e.users) {
            line(&mut out, &style.bold(&summary));
        }
        out.push('\n');

        if e.users.is_empty() {
            line(
                &mut out,
                &format!("Nothing points at this node via [{}].", r.edge_types.join(", ")),
            );
            line(
                &mut out,
                &format!(
                    "Try more hops, different edge types ({} lists what this graph has), or {} for outbound dependencies.",
                    style.id("graph_schema"),
                    style.id("traverse")
                ),
            );
            continue;
        }

        for u in &e.users {
            let via = if u.depth > 1 {
                let target = names.get(u.via_target.as_str()).copied().unwrap_or(&u.via_target);
                style.dim(&format!("—{}→ {} (hop {})", u.via_edge, target, u.depth))
            } else {
                style.dim(&format!("—{}→", u.via_edge))
            };
            line(
                &mut out,
                &format!(
                    "- {} {}  {} {}",
                    u.symbol.node_type,
                    style.bold(&u.symbol.name),
                    style.dim(&u.symbol.loc()),
                    via
                ),
            );
            line(&mut out, &format!("  id: {}", style.id(&u.symbol.id)));
            // The summary line above says how many users are boundaries;
            // this says which, so the reader does not have to re-derive it
            // from the names.
            if let Some(b) = &u.symbol.boundary {
                line(&mut out, &format!("  {}", style.bold(&format!("boundary: {}", b))));
            }
            for site in &u.call_sites {
                line(
                    &mut out,
                    &format!(
                        "    {}:{}  {}",
                        u.symbol.file.as_deref().unwrap_or("?"),
                        site.line,
                        style.id(&site.text)
                    ),
                );
            }
        }
        if e.users.iter().any(|u| !u.call_sites.is_empty()) {
            line(
                &mut out,
                &style.dim(
                    "(call-site lines matched by name — a hit inside a comment or string is possible)",
                ),
            );
        }
    }
    next_actions(
        &mut out,
        style,
        &[
            ("get_code <id>", "to read a caller"),
            ("find_usages <id> --hops 2", "for transitive users"),
        ],
    );
    out
}
