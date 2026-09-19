//! `find_symbols` — agent tool.

use super::*;

/// Params are canonical snake_case. The `alias` attributes accept the legacy
/// MCP camelCase spellings so existing agent calls keep working unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FindSymbolsParams {
    /// Direct node id lookup — O(1), skips the search entirely.
    #[serde(alias = "nodeId", alias = "nodeIds", deserialize_with = "de_one_or_many")]
    pub node_id: Vec<String>,
    /// Identifier, fragment, or wildcard pattern to match against node
    /// names. A pattern (`run_*`, `get?`, `[gs]et_code`, `{save,load}_*`)
    /// must match the whole name; a plain fragment keeps its
    /// exact > prefix > substring ranking.
    #[serde(deserialize_with = "de_one_or_many")]
    pub name: Vec<String>,
    /// Restrict to these node types (case-insensitive; wildcards allowed).
    #[serde(alias = "nodeTypes", deserialize_with = "de_one_or_many")]
    pub node_types: Vec<String>,
    /// Only symbols under this repo-relative path: a plain prefix
    /// (`src/auth/`) or a path glob (`src/**/*.ts`).
    #[serde(
        alias = "filePrefix",
        alias = "file",
        alias = "filePattern",
        alias = "file_pattern",
        alias = "path"
    )]
    pub file_prefix: Option<String>,
    #[serde(alias = "k")]
    pub limit: Option<usize>,
    /// Also match against docstrings, not just names. A docstring hit ranks
    /// below every name hit, since matching the identifier is the stronger
    /// signal.
    #[serde(alias = "includeDocs")]
    pub include_docs: bool,
    /// Keep only symbols that are system boundaries — REST handlers, queue
    /// listeners, CLI commands, outbound clients.
    ///
    /// The way to ask "what is this service's public surface" without
    /// knowing what any of it is called, which is exactly the position
    /// someone is in on their first day in an unfamiliar repo.
    #[serde(default)]
    pub boundary: bool,
}

const DEFAULT_SYMBOL_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct SymbolQueryResult {
    pub query: String,
    /// `"id"` for a direct lookup, `"name"` for a ranked name search,
    /// `"pattern"` for a wildcard match.
    pub kind: &'static str,
    pub total: usize,
    pub items: Vec<SymbolRef>,
    /// Names that matched but were removed by a filter. Only meaningful
    /// when `items` is empty, where it is the difference between "no such
    /// name" and "that name is not a boundary".
    #[serde(skip_serializing_if = "ExcludedByFilter::is_empty")]
    pub excluded: ExcludedByFilter,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How many name matches each filter removed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExcludedByFilter {
    #[serde(rename = "nodeType")]
    pub node_type: usize,
    #[serde(rename = "filePrefix")]
    pub file_prefix: usize,
    pub boundary: usize,
}

impl ExcludedByFilter {
    pub fn is_empty(&self) -> bool {
        self.node_type == 0 && self.file_prefix == 0 && self.boundary == 0
    }

    /// The filter that removed the most, as a phrase naming it and what
    /// relaxing it would do.
    pub fn dominant(&self) -> Option<(&'static str, usize)> {
        [
            ("boundary", self.boundary),
            ("filePrefix", self.file_prefix),
            ("nodeTypes", self.node_type),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FindSymbolsResult {
    pub queries: Vec<SymbolQueryResult>,
}

impl FindSymbolsResult {
    pub fn ok(&self) -> bool {
        self.queries.iter().all(|q| q.error.is_none())
    }
}

pub fn find_symbols(graph: &GraphData, p: &FindSymbolsParams) -> FindSymbolsResult {
    let limit = p.limit.unwrap_or(DEFAULT_SYMBOL_LIMIT);
    let mut queries = Vec::new();

    // A bad filter is the whole call's problem, not one query's — report it
    // once, against every query, instead of silently returning matches that
    // ignored the filter the caller asked for.
    let filters = type_matchers(&p.node_types).and_then(|types| {
        let file = match &p.file_prefix {
            Some(f) => Some(PathFilter::new(f)?),
            None => None,
        };
        Ok::<_, String>((types, file))
    });
    let (types, file_filter) = match filters {
        Ok(f) => f,
        Err(e) => {
            let queries = p
                .node_id
                .iter()
                .chain(p.name.iter())
                .map(|q| SymbolQueryResult {
                    query: q.clone(),
                    kind: "name",
                    total: 0,
                    items: vec![],
                    excluded: ExcludedByFilter::default(),
                    error: Some(e.clone()),
                })
                .collect();
            return FindSymbolsResult { queries };
        }
    };

    for id in &p.node_id {
        queries.push(match graph.nodes.iter().find(|n| n.id == *id) {
            Some(n) => SymbolQueryResult {
                query: id.clone(),
                kind: "id",
                total: 1,
                items: vec![SymbolRef::from_node(n)],
                excluded: ExcludedByFilter::default(),
                error: None,
            },
            None => SymbolQueryResult {
                query: id.clone(),
                kind: "id",
                total: 0,
                items: vec![],
                excluded: ExcludedByFilter::default(),
                error: Some(format!(
                    "No node with id '{}' — ids come from find_symbols, search or file_outline.",
                    id
                )),
            },
        });
    }

    for name in &p.name {
        let matcher = match Matcher::new(name, Mode::Name) {
            Ok(m) => m,
            Err(e) => {
                queries.push(SymbolQueryResult {
                    query: name.clone(),
                    kind: "pattern",
                    total: 0,
                    items: vec![],
                    excluded: ExcludedByFilter::default(),
                    error: Some(e),
                });
                continue;
            }
        };
        let glob = matcher.is_glob();
        let mut hits: Vec<(u8, &GraphNode)> = Vec::new();
        // Names that matched and were then excluded by a filter. "No
        // matches, try a shorter fragment" is the wrong advice when the
        // fragment was perfect and `boundary: true` is what dropped it —
        // the caller shortens a name that was never the problem.
        let mut excluded = ExcludedByFilter::default();
        for n in &graph.nodes {
            // Filters are applied after the name test, not before, so a
            // near-miss can be reported as one. The name test is pure, so
            // the order costs nothing but a little arithmetic on rows that
            // were going to be skipped anyway.
            let name_matches = if glob {
                matcher.matches(&n.name)
            } else {
                literal_rank(&n.name, &matcher, false, n) < 4
            };
            if !type_allowed(&types, n) {
                excluded.node_type += usize::from(name_matches);
                continue;
            }
            if let Some(f) = &file_filter {
                if !f.matches(n.file.as_deref().unwrap_or("")) {
                    excluded.file_prefix += usize::from(name_matches);
                    continue;
                }
            }
            if p.boundary && n.boundaries.is_empty() {
                excluded.boundary += usize::from(name_matches);
                continue;
            }
            // A wildcard is an explicit statement of what the name looks
            // like, so every hit is equally "exact" — there is no weaker
            // prefix/substring tier to fall back to. A literal keeps the
            // exact > prefix > substring ladder. Either way a docstring hit
            // ranks last: matching the identifier beats matching prose
            // about it.
            let rank = if glob {
                if matcher.matches(&n.name) {
                    0
                } else if p.include_docs && doc_hit(n, &matcher) {
                    3
                } else {
                    4
                }
            } else {
                literal_rank(&n.name, &matcher, p.include_docs, n)
            };
            if rank < 4 {
                hits.push((rank, n));
            }
        }
        // Literal queries tie-break on the shorter (closer) name. Pattern
        // queries are a listing rather than a ranked search, so they sort
        // alphabetically by name then file — stable and scannable, and it
        // does not shuffle `run_gen`/`run_index`/`run_serve` by length.
        if glob {
            hits.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.name.cmp(&b.1.name))
                    .then_with(|| a.1.file.cmp(&b.1.file))
            });
        } else {
            hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.len().cmp(&b.1.name.len())));
        }
        let total = hits.len();
        queries.push(SymbolQueryResult {
            query: name.clone(),
            kind: if glob { "pattern" } else { "name" },
            total,
            items: hits
                .iter()
                .take(limit)
                .map(|(_, n)| SymbolRef::from_node(n))
                .collect(),
            excluded,
            error: None,
        });
    }

    FindSymbolsResult { queries }
}

fn doc_hit(n: &GraphNode, matcher: &Matcher) -> bool {
    n.docstring
        .as_ref()
        .map(|d| matcher.matches_within(d))
        .unwrap_or(false)
}

/// exact > prefix > substring > docstring, for a literal query. `4` means
/// no match.
fn literal_rank(name: &str, matcher: &Matcher, include_docs: bool, n: &GraphNode) -> u8 {
    let Matcher::Literal(q) = matcher else {
        return 4;
    };
    let nm = name.to_lowercase();
    if nm == *q {
        0
    } else if nm.starts_with(q.as_str()) {
        1
    } else if nm.contains(q.as_str()) {
        2
    } else if include_docs && doc_hit(n, matcher) {
        3
    } else {
        4
    }
}

pub fn render_find_symbols(r: &FindSymbolsResult, style: Render) -> String {
    let mut out = String::new();
    for (i, q) in r.queries.iter().enumerate() {
        section_break(&mut out, i, style);
        if let Some(e) = &q.error {
            line(&mut out, &format!("✗ {}", e));
            continue;
        }
        if q.kind == "id" {
            line(&mut out, &style.heading("Node by direct id lookup"));
            out.push('\n');
        } else {
            // Say when a result was truncated *and* how to see the rest —
            // a bare "showing 20" leaves the reader to guess the flag.
            let showing = if q.total > q.items.len() {
                format!(
                    ", showing {} — raise {} for the rest",
                    q.items.len(),
                    style.id("limit")
                )
            } else {
                String::new()
            };
            let what = if q.kind == "pattern" {
                "Symbols matching pattern"
            } else {
                "Symbols matching"
            };
            line(
                &mut out,
                &format!(
                    "{} — {} match(es){}",
                    style.heading(&format!("{} '{}'", what, q.query)),
                    q.total,
                    showing
                ),
            );
            out.push('\n');
        }
        if q.items.is_empty() {
            // The name was right and a filter is what dropped it. Say that
            // instead of advising a shorter fragment, which would send the
            // caller to widen the one part of the query that worked.
            if let Some((filter, n)) = q.excluded.dominant() {
                line(
                    &mut out,
                    &format!(
                        "No matches. {} name{} match{} '{}' but {} excluded by {}. \
                         Drop that filter to see {}.",
                        n,
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "es" } else { "" },
                        q.query,
                        if n == 1 { "was" } else { "were" },
                        style.id(filter),
                        if n == 1 { "it" } else { "them" },
                    ),
                );
                continue;
            }
            // Each suggestion is a different failure: too specific a string,
            // too narrow a filter, or the wrong tool entirely.
            let widen = if q.kind == "pattern" {
                format!(
                    "Wildcards match the whole name — wrap the pattern in {} to match anywhere ({}).",
                    style.id("*"),
                    style.id("*auth*")
                )
            } else {
                format!(
                    "Try a shorter fragment, or a wildcard ({} matches the whole name).",
                    style.id("*auth*")
                )
            };
            line(&mut out, &format!("No matches. {}", widen));
            line(
                &mut out,
                &format!(
                    "Also worth trying: drop the type/file filters, add {} to scan docstrings, or use {} for a concept-level query.",
                    style.id("include_docs"),
                    style.id("search")
                ),
            );
            continue;
        }
        for item in &q.items {
            item.render_bullet(&mut out, style);
        }
    }
    next_actions(
        &mut out,
        style,
        &[
            ("get_code <id>", "for source"),
            ("find_usages <id>", "for callers"),
            ("traverse <id>", "for dependencies"),
        ],
    );
    out
}
