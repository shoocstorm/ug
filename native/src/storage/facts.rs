//! Queryable per-node facts, derived once at ingest and stored as
//! properties.
//!
//! Everything a whole-repo statistical question needs — "how many methods
//! are longer than 50 lines", "which symbols does nothing call" — has to be
//! a *stored property*, because that is the only thing a query language can
//! filter and aggregate on. `graph.json` carries these facts today and the
//! store dropped them, which is why the store could answer "find me
//! something like X" but not "how many X are there".
//!
//! Two rules shape what belongs here:
//!
//! 1. **Derivable per node, once.** A fact that needs the whole graph
//!    (degrees) is fine because it is computed once into a [`FactContext`];
//!    a fact that needs a second query at read time is not.
//! 2. **Booleans are stored as `0`/`1` integers.** GQL has no boolean
//!    aggregate, so "what fraction has docs" is `sum(has_doc) / count(*)`.
//!    Storing `true`/`false` would make the most common shape of question
//!    impossible to express.

use crate::types::{
    BoundaryDirection, FileClassification, GraphData, GraphEdgeType, GraphNode, GraphNodeType,
};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// A stored fact value, in the small set of shapes every backend can hold.
///
/// Deliberately not `overgraph::PropValue`: `NodeRow` is the
/// backend-portable DTO and Neo4j has to be able to write these too.
#[derive(Debug, Clone, PartialEq)]
pub enum FactValue {
    Int(i64),
    Str(String),
}

impl FactValue {
    /// Booleans enter the store as 0/1 so they can be summed. See the
    /// module note.
    pub fn from_bool(b: bool) -> Self {
        FactValue::Int(if b { 1 } else { 0 })
    }
}

/// Facts attached to one node, keyed by property name.
pub type Facts = BTreeMap<String, FactValue>;

/// Graph-wide context needed by facts that are not local to a node.
///
/// Built once per ingest so `compute` stays O(1) per node.
pub struct FactContext<'a> {
    /// Inbound edges excluding `Contains`, i.e. "how much code depends on
    /// this". `Contains` is pure structure (folder→file→symbol) and would
    /// give every symbol an in-degree of 1 for free, drowning the signal.
    in_degree: HashMap<&'a str, u32>,
    /// Outbound edges, same exclusion.
    out_degree: HashMap<&'a str, u32>,
    /// Outbound `Contains` edges from a type to the members it declares.
    ///
    /// The one place `Contains` is the signal rather than the noise. Only
    /// meaningful for languages whose class body encloses its members —
    /// Java has 451 such edges in the bundled sample; Rust has none,
    /// because `impl` blocks sit outside the struct they extend. See
    /// [`compute`] for how that asymmetry is kept honest.
    members: HashMap<&'a str, u32>,
    /// How many times each *short* symbol name is mentioned by some other
    /// node, keyed by that name.
    ///
    /// The companion to [`Self::in_degree`], and the reason `dead_code` is
    /// worth reading. An in-degree of zero means *the resolver drew no
    /// edge*, which is a much weaker claim than "nothing uses this": a
    /// trait method reached through `dyn Trait`, a handler named in a
    /// `.route()` table, a struct that only ever arrives through
    /// `serde`, and a JS function called as `obj.method()` all have an
    /// in-degree of zero and are all live. Every zero-in-degree symbol in
    /// this repository was checked by hand: 454 of 462 were false
    /// positives that way.
    ///
    /// What separates them is that something, somewhere, still writes the
    /// name down. So this counts raw name mentions across the graph before
    /// resolution — every callee name, implemented trait, imported item,
    /// parameter and return type, and every word of prose — attributing
    /// each to the node that wrote it and skipping the node's own name, so
    /// a symbol does not mention itself into life.
    ///
    /// Keyed by short name, not by id: an unresolved mention is *only* a
    /// name, which is what makes it unresolved. Two symbols sharing a short
    /// name therefore share a count, and the error is one-directional — a
    /// dead `open` is hidden by a live `open` elsewhere, never the reverse.
    /// That is the right way to be wrong for a list of candidates.
    name_mentions: HashMap<&'a str, u32>,
    /// Whether this graph was written by a build that records comment
    /// metrics. A graph older than that answers "how many functions have
    /// comments" with zero, which is worse than refusing.
    has_line_metrics: bool,
    /// Whether this graph was written by a build that detects system
    /// boundaries.
    ///
    /// The same trap as [`Self::has_line_metrics`], and a nastier one here:
    /// most symbols in any repo genuinely are not boundaries, so a graph
    /// that simply never looked is indistinguishable from one that looked
    /// and found nothing. `boundary = 0` everywhere reads as a finished
    /// measurement. Omitting the facts makes the coverage line say
    /// NOT INDEXED instead.
    has_boundaries: bool,
}

impl<'a> FactContext<'a> {
    pub fn new(graph: &'a GraphData) -> Self {
        // Keys borrowed from the graph. Owning them meant a ~141-character
        // allocation per edge endpoint — roughly 1.5 million per call, and
        // this is built twice per ingest (once to plan, once to build rows).
        // Same shape as P11.11 and P10.7. See P11.12 in
        // docs/dev/PERF-TUNING-JOURNEY.md.
        let mut in_degree: HashMap<&'a str, u32> = HashMap::new();
        let mut out_degree: HashMap<&'a str, u32> = HashMap::new();
        let mut members: HashMap<&'a str, u32> = HashMap::new();
        for e in &graph.edges {
            if matches!(e.edge_type, GraphEdgeType::Contains) {
                *members.entry(&e.source).or_insert(0) += 1;
                continue;
            }
            *in_degree.entry(&e.target).or_insert(0) += 1;
            *out_degree.entry(&e.source).or_insert(0) += 1;
        }
        // Same borrowing discipline as the degree maps above: every key is
        // a subslice of a node field, so a pass over ~5.7k nodes allocates
        // only the map itself.
        let mut name_mentions: HashMap<&'a str, u32> = HashMap::new();
        let mut scratch: Vec<&'a str> = Vec::new();
        for n in &graph.nodes {
            if !mentions_count_from(n) {
                continue;
            }
            let own = short_name(&n.name);
            scratch.clear();
            collect_mentions(n, &mut scratch);
            for m in &scratch {
                if !m.is_empty() && *m != own {
                    *name_mentions.entry(m).or_insert(0) += 1;
                }
            }
        }

        let schema = graph
            .stats
            .as_ref()
            .map(|s| s.graph_schema_version)
            .unwrap_or(0);
        Self {
            in_degree,
            out_degree,
            members,
            name_mentions,
            has_line_metrics: schema >= 2,
            has_boundaries: schema >= 4,
        }
    }
}

/// Path segments that mark a file as test code.
///
/// Now the *fallback*, not the rule: the indexer's `FileClassification`
/// reaches `GraphNode` and takes precedence where it exists (see
/// [`compute`]). This still matters for graphs written before that landed,
/// which is the only case left — every `FileClassification` variant is a
/// decision, so a node that has one never reaches here.
///
/// Every marker is anchored — on a `/` (start of a path segment) or on the
/// `.`/`_` that delimits a filename suffix. An unanchored `test_` matched
/// mid-word and swept in production code: `latest_version.rs`,
/// `greatest_hits.rs`, `fastest_path.ts`, `contest_rules.py` all contain
/// `test_` and all read as test code. Anchoring to `/test_` keeps the real
/// case (`src/test_thing.py`, where the marker starts a segment) and drops
/// the false ones.
const TEST_PATH_MARKERS: &[&str] = &[
    "/test/",
    "/tests/",
    "/__tests__/",
    "/spec/",
    "/test_",
    "_test.",
    "_tests.",
    ".test.",
    ".tests.",
    ".spec.",
    "_spec.",
    "_specs.",
];

/// Annotation names that mark a symbol as test code.
///
/// Matched against the annotation's **last segment**, case-insensitively:
/// `#[tokio::test]`, `#[async_std::test]` and Java's `@Test` all reduce to
/// `test`. Matching the whole name is what missed them — `is_test_node`
/// compared `name == "test"` exactly, so of the 174 `tokio::test` functions
/// in this repository, only those that also sat inside a `#[cfg(test)] mod`
/// were recognised. The rest read as production code, which is what
/// `untested_symbols` then reported them as.
///
/// The JUnit lifecycle names earn their place separately: `@BeforeEach` on a
/// `FooTestBase` class is the only marker such a file carries, since it is
/// neither in a `/test/` path nor annotated `@Test`.
///
/// Deliberately absent: JUnit 4's bare `@Before` / `@After`. They are common
/// in older Java, but "test" is not in the name and nothing in the indexed
/// corpora needed them — an ambiguous marker added on a guess is how
/// `latest_version.rs` once read as test code.
const TEST_ANNOTATIONS: &[&str] = &[
    // Rust `#[test]` / `#[tokio::test]`; Java + TestNG `@Test`.
    "test",
    // JUnit 5.
    "parameterizedtest",
    "repeatedtest",
    "testfactory",
    "testtemplate",
    // JUnit lifecycle — only ever on a test class.
    "beforeeach",
    "aftereach",
    "beforeall",
    "afterall",
    "beforeclass",
    "afterclass",
];

/// Does this annotation mark test code?
///
/// Three shapes, because three ecosystems spell it differently:
///
/// 1. `cfg(test)` — Rust, exact. Everything inside a `#[cfg(test)] mod` is
///    test code even when the file around it is not.
/// 2. Anything under `pytest.` — `pytest.fixture`, `pytest.mark.asyncio`.
///    The prefix is unambiguous, and the *last* segment is not: matching on
///    `asyncio` would be wrong and matching on `mark` meaningless.
/// 3. The last `::`- or `.`-delimited segment, against [`TEST_ANNOTATIONS`].
fn annotation_marks_test(name: &str) -> bool {
    if name == "cfg(test)" {
        return true;
    }
    if name.len() > 7 && name[..7].eq_ignore_ascii_case("pytest.") {
        return true;
    }
    let last = name.rsplit(['.', ':']).next().unwrap_or(name);
    TEST_ANNOTATIONS.iter().any(|m| last.eq_ignore_ascii_case(m))
}

fn looks_like_test(file: &str) -> bool {
    // Leading separator so a top-level `tests/` directory matches the same
    // `/tests/` marker as a nested one, without a second set of patterns.
    let probe = format!("/{}", file);
    let lower = probe.to_ascii_lowercase();
    TEST_PATH_MARKERS.iter().any(|m| lower.contains(m))
}

/// Is this node test code?
///
/// The single definition, because two of them would disagree. This backs the
/// stored `is_test` fact — which every `analyze` test preset filters on,
/// `test_for` included — and [`crate::agent_tools::context`], which has to
/// find a symbol's tests straight from `graph.json` with no store open. A
/// second heuristic in the second caller would mean `ug context` and
/// `ug analyze test_for` disagreeing about what a test is, on the same repo,
/// in the same session.
///
/// Three signals, most specific first:
///
/// 1. A per-symbol test annotation — see [`annotation_marks_test`], which
///    covers `#[test]`, `#[tokio::test]`, `#[cfg(test)]`, `@Test`, the
///    JUnit 5 family and anything under `pytest.`. A helper inside a
///    `#[cfg(test)] mod tests` in an otherwise production file is test code,
///    and only the marker knows that.
/// 2. The indexer's `FileClassification`, which saw the file's contents
///    rather than just its name. It has no "unknown" variant — every variant
///    is a decision — so `Some(c)` means the classifier had an opinion, and
///    its answer stands in both directions. Letting `Some(Util)` fall through
///    to the path heuristic below is how a `fastest_path.ts` got relabelled a
///    test by its name.
/// 3. The path markers, for graphs written before classification reached the
///    node.
///
/// Nodes without a file (and `Folder` nodes, whose `file` is their own path)
/// are never tests.
pub fn is_test_node(n: &GraphNode) -> bool {
    if matches!(n.node_type, GraphNodeType::Folder) {
        return false;
    }
    let Some(file) = n.file.as_deref().filter(|s| !s.is_empty()) else {
        return false;
    };
    if n.annotations
        .iter()
        .any(|a| annotation_marks_test(&a.name))
    {
        return true;
    }
    match &n.classification {
        Some(c) => *c == FileClassification::Test,
        None => looks_like_test(file),
    }
}

/// Stable lowercase name for a file classification.
///
/// Spelled out rather than derived from `Debug`, so a rename of the enum
/// variant cannot silently change a stored property that queries and
/// saved presets filter on.
pub(crate) fn classification_str(c: &FileClassification) -> &'static str {
    match c {
        FileClassification::Component => "component",
        FileClassification::Page => "page",
        FileClassification::Hook => "hook",
        FileClassification::Util => "util",
        FileClassification::Service => "service",
        FileClassification::Config => "config",
        FileClassification::Type => "type",
        FileClassification::Constant => "constant",
        FileClassification::Context => "context",
        FileClassification::Reducer => "reducer",
        FileClassification::Test => "test",
        FileClassification::Asset => "asset",
        FileClassification::Documentation => "documentation",
    }
}

/// Whether a node's text counts as somebody still using a name.
///
/// Code only. A markdown heading is indexed as a `Concept` whose docstring
/// is the section body, and prose *about* the codebase is not a use of it —
/// most sharply when the prose is a dead-code audit. Writing "this symbol
/// is dead, nothing references it" into `docs/` puts that symbol's name in
/// a `Concept` docstring, lifts its `name_mentions` to 1 and drops it out
/// of `dead_code`: the act of recording the finding erases it. Every symbol
/// on this repo's audit list vanished that way, on the commit that wrote
/// the audit down.
///
/// The same trap bites in code, one name at a time: naming a dead symbol in
/// a doc comment anywhere revives it. That is the intended behaviour —
/// prose beside live code is evidence — but it means an example in a doc
/// comment should never use a real symbol's name.
///
/// `File` and `Folder` are excluded for the duller reason that their names
/// are paths.
fn mentions_count_from(n: &GraphNode) -> bool {
    matches!(
        n.node_type,
        GraphNodeType::Function
            | GraphNodeType::Class
            | GraphNodeType::Interface
            | GraphNodeType::Constant
            | GraphNodeType::Variable
    )
}

/// True for the characters an identifier is made of, in every language the
/// indexer reads. `$` earns its place for JS.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The last identifier in a possibly-qualified name.
///
/// `Db::open_inner` → `open_inner`, `obj.method` → `method`,
/// `[Symbol.iterator]` → `iterator`. Trailing punctuation is trimmed
/// rather than split on, because a JS computed-member definition ends in
/// `]` and splitting alone would key it under `iterator]`, which nothing
/// would ever mention.
///
/// Qualifiers are dropped on both sides of the comparison: a call is
/// recorded as whatever the source wrote (`self.foo`, `Type::foo`, `foo`),
/// and matching those to a definition is exactly the resolution step that
/// has already failed by the time this matters.
fn short_name(name: &str) -> &str {
    let Some((end, c)) = name.char_indices().rev().find(|(_, c)| is_ident_char(*c)) else {
        return "";
    };
    let end = end + c.len_utf8();
    let start = name[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_ident_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    &name[start..end]
}

/// Push every identifier-shaped run in `text` onto `out`.
///
/// Walks `char_indices` rather than bytes: doc comments in this repo
/// contain `×`, `→` and em dashes, and stepping a non-identifier byte at a
/// time lands inside one of them and panics.
fn push_identifiers<'a>(text: &'a str, out: &mut Vec<&'a str>) {
    let mut run: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (is_ident_char(c), run) {
            (true, None) => run = Some(i),
            (false, Some(start)) => {
                out.push(&text[start..i]);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run {
        out.push(&text[start..]);
    }
}

/// Every name this one node writes down, whatever the resolver made of it.
///
/// Prose counts. A symbol whose doc comment is the last thing in the repo
/// that names it is still forgotten code, but a symbol another symbol's
/// doc comment explains is not — and telling those apart is the whole
/// point of a candidate list someone has to read.
fn collect_mentions<'a>(n: &'a GraphNode, out: &mut Vec<&'a str>) {
    for c in n.calls.iter().chain(&n.implements).chain(&n.extends) {
        out.push(short_name(c));
    }
    for im in &n.imports {
        for item in &im.imported {
            out.push(short_name(&item.name));
            if let Some(alias) = &item.alias {
                out.push(short_name(alias));
            }
        }
    }
    if let Some(sig) = &n.signature {
        for p in &sig.params {
            push_identifiers(&p.name, out);
            if let Some(t) = &p.param_type {
                push_identifiers(t, out);
            }
        }
        if let Some(r) = &sig.return_type {
            push_identifiers(r, out);
        }
    }
    if let Some(doc) = &n.docstring {
        push_identifiers(doc, out);
    }
}

/// Parent directory of a repo-relative file path, `""` for a file at the
/// repo root. Used to group statistics by module without a query-time
/// string function.
fn folder_of(file: &str) -> &str {
    match file.rfind('/') {
        Some(ix) => &file[..ix],
        None => "",
    }
}

/// Lowercase file extension of a repo-relative path, without the dot —
/// `"rs"`, `"md"`, `"tsx"`. `None` for a path that has none.
///
/// `Path::extension` rather than a `rfind('.')`, so it agrees with
/// `indexer::process_file`, which derives the extension the same way to
/// decide whether to index the file at all. That agreement is the whole
/// point: a file with no extension is never indexed, so every File node in
/// the graph carries this fact and a census grouped on it is complete.
/// Spelled bare rather than `".rs"` to match `language`, which is also
/// stored as the value a caller would type in `WHERE n.extension = 'rs'`.
fn extension_of(file: &str) -> Option<String> {
    Path::new(file)
        .extension()?
        .to_str()
        .map(|e| e.to_ascii_lowercase())
}

/// Lines the node spans, inclusive.
///
/// Prefers the indexer's `metrics.loc` and falls back to the line range.
/// Both are inclusive of their first and last line, so a Function (which
/// has metrics) and a Concept (which may not) are comparable.
///
/// This is a *span*: it counts blank and comment lines. The `code_lines`
/// fact is the one to use when you mean "lines of code" — on commented
/// code the two differ by roughly 30%.
fn span_loc(n: &GraphNode) -> Option<u32> {
    if let Some(m) = &n.metrics {
        return Some(m.loc);
    }
    match (n.start_line, n.end_line) {
        (Some(s), Some(e)) if e >= s => Some(e - s + 1),
        _ => None,
    }
}

/// Derive every stored fact for one node.
pub fn compute(n: &GraphNode, ctx: &FactContext) -> Facts {
    let mut f = Facts::new();

    if let Some(loc) = span_loc(n) {
        f.insert("loc".into(), FactValue::Int(loc as i64));
    }
    if let Some(m) = &n.metrics {
        f.insert("params".into(), FactValue::Int(m.params as i64));
        f.insert("max_nesting".into(), FactValue::Int(m.max_nesting as i64));

        // Only when the graph is new enough to actually carry them.
        // Writing `comment_lines = 0` from a graph indexed before the
        // metric existed would produce the exact failure this design
        // exists to prevent: a confident zero that reads as a measurement.
        // Omitted, the property shows up as NOT INDEXED in every answer's
        // coverage line, which tells the caller to reindex.
        if ctx.has_line_metrics {
            f.insert("comment_lines".into(), FactValue::Int(m.comment_lines as i64));
            f.insert("doc_lines".into(), FactValue::Int(m.doc_lines as i64));
            f.insert("code_lines".into(), FactValue::Int(m.code_lines as i64));
            f.insert(
                "has_comments".into(),
                FactValue::from_bool(m.comment_lines > 0 || m.doc_lines > 0),
            );
        }
    }

    f.insert(
        "has_doc".into(),
        FactValue::from_bool(n.docstring.as_deref().is_some_and(|d| !d.trim().is_empty())),
    );

    if let Some(lang) = n.language.as_deref().filter(|s| !s.is_empty()) {
        f.insert("language".into(), FactValue::Str(lang.to_string()));
    }
    if let Some(c) = &n.classification {
        f.insert(
            "classification".into(),
            FactValue::Str(classification_str(c).to_string()),
        );
    }

    // Members are only recorded where the graph genuinely has them. A
    // Rust struct's methods live in a separate `impl` block, so it has no
    // `Contains` edges and gets no `members` fact — absent rather than a
    // zero that would rank every Rust type as memberless. The coverage
    // line makes the partial population visible.
    if matches!(n.node_type, GraphNodeType::Class | GraphNodeType::Interface) {
        if let Some(count) = ctx.members.get(n.id.as_str()).copied().filter(|c| *c > 0) {
            f.insert("members".into(), FactValue::Int(count as i64));
        }
    }

    // Folder, extension and is_test are only meaningful for nodes that
    // live in a file. Folder nodes carry their own path in `file`, which
    // would make `folder` self-referential, so they are excluded.
    if !matches!(n.node_type, GraphNodeType::Folder) {
        if let Some(file) = n.file.as_deref().filter(|s| !s.is_empty()) {
            f.insert("folder".into(), FactValue::Str(folder_of(file).to_string()));
            f.insert("is_test".into(), FactValue::from_bool(is_test_node(n)));
            if let Some(ext) = extension_of(file) {
                f.insert("extension".into(), FactValue::Str(ext));
            }
        }
    }

    f.insert(
        "in_degree".into(),
        FactValue::Int(ctx.in_degree.get(n.id.as_str()).copied().unwrap_or(0) as i64),
    );
    f.insert(
        "out_degree".into(),
        FactValue::Int(ctx.out_degree.get(n.id.as_str()).copied().unwrap_or(0) as i64),
    );

    // Only for symbols. A File or Folder node's `name` is a path, whose
    // last identifier is an extension (`rs`), and counting how often the
    // word "rs" appears would be noise wearing a fact's clothes.
    if !matches!(n.node_type, GraphNodeType::File | GraphNodeType::Folder) {
        f.insert(
            "name_mentions".into(),
            FactValue::Int(
                ctx.name_mentions
                    .get(short_name(&n.name))
                    .copied()
                    .unwrap_or(0) as i64,
            ),
        );
    }

    if let Some(q) = n.qualified_name.as_deref().filter(|s| !s.is_empty()) {
        f.insert("qualified_name".into(), FactValue::Str(q.to_string()));
    }
    if let Some(r) = n.route.as_deref().filter(|s| !s.is_empty()) {
        f.insert("route".into(), FactValue::Str(r.to_string()));
    }
    if !n.annotations.is_empty() {
        // Joined rather than nested: the store holds scalars, and the
        // shape queries actually want is "does this contain X".
        let names: Vec<&str> = n.annotations.iter().map(|a| a.name.as_str()).collect();
        f.insert("annotations".into(), FactValue::Str(names.join(",")));
    }

    // Only on a graph that actually looked — see `FactContext::has_boundaries`
    // for why a zero here would be a lie rather than a measurement.
    if ctx.has_boundaries {
        let inbound = n
            .boundaries
            .iter()
            .any(|b| b.direction == BoundaryDirection::Inbound);
        let outbound = n
            .boundaries
            .iter()
            .any(|b| b.direction == BoundaryDirection::Outbound);

        f.insert(
            "boundary".into(),
            FactValue::from_bool(!n.boundaries.is_empty()),
        );
        f.insert("boundary_in".into(), FactValue::from_bool(inbound));
        f.insert("boundary_out".into(), FactValue::from_bool(outbound));

        // Same comma-joined shape as `annotations`, and for the same reason:
        // the question is "does this contain X". Deduped because one symbol
        // registering six routes is one `http.endpoint`, not six.
        if !n.boundaries.is_empty() {
            f.insert(
                "boundary_kinds".into(),
                FactValue::Str(joined(n.boundaries.iter().map(|b| b.kind.as_str()))),
            );
            f.insert(
                "boundary_protocols".into(),
                FactValue::Str(joined(n.boundaries.iter().map(|b| b.protocol.as_str()))),
            );

            // The names of the surfaces themselves — `GET /api/orders/{id}`,
            // `orders.inbound`, a cron expression. `route` already carries
            // the HTTP case, but only that case and only for Java; a queue
            // listener's destination appears in no other property, and it is
            // the string a person actually searches for.
            let detail = joined(n.boundaries.iter().filter_map(|b| b.detail.as_deref()));
            if !detail.is_empty() {
                f.insert("boundary_detail".into(), FactValue::Str(detail));
            }
        }
    }

    f
}

/// Comma-join in first-seen order, dropping repeats.
///
/// Order is stable rather than sorted so the string reads the way the source
/// does, and `stored_row_matches` can compare two ingests of an unchanged
/// file byte-for-byte instead of re-upserting it every run.
fn joined<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Annotation, Boundary, GraphEdge, SymbolMetrics};

    fn node(id: &str, file: Option<&str>) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: id.to_string(),
            node_type: GraphNodeType::Function,
            file: file.map(String::from),
            ..Default::default()
        }
    }

    /// A context from a graph with **no** stats block, i.e. one whose
    /// schema version is unknown and therefore pre-v2.
    fn ctx_of(edges: Vec<GraphEdge>) -> FactContext<'static> {
        // Leaked: `FactContext` borrows its keys from the graph now, and these
        // helpers feed ~50 call sites that would otherwise each have to own
        // one. Test-only, a handful of edges each, and the process exits.
        let g: &'static GraphData = Box::leak(Box::new(GraphData {
            nodes: vec![],
            edges,
            stats: None,
            resolution: None,
        }));
        FactContext::new(g)
    }

    /// A context from a graph stamped with the given schema version.
    fn ctx_at_version(version: u32, edges: Vec<GraphEdge>) -> FactContext<'static> {
        let g: &'static GraphData = Box::leak(Box::new(GraphData {
            nodes: vec![],
            edges,
            stats: Some(crate::types::IndexStats {
                graph_schema_version: version,
                total_files: 0,
                cached_files: 0,
                total_symbols: 0,
                total_folders: 0,
                total_lines: 0,
                indexing_time_ms: 0,
                last_indexed_at: 0,
                repo_root: String::new(),
            }),
            resolution: None,
        }));
        FactContext::new(g)
    }

    fn with_line_metrics(comment: u32, doc: u32, code: u32) -> Option<SymbolMetrics> {
        Some(SymbolMetrics {
            loc: 40,
            params: 1,
            max_nesting: 2,
            comment_lines: comment,
            doc_lines: doc,
            code_lines: code,
        })
    }

    fn edge(source: &str, target: &str, edge_type: GraphEdgeType) -> GraphEdge {
        GraphEdge {
            source: source.into(),
            target: target.into(),
            edge_type,
        }
    }

    #[test]
    fn loc_prefers_metrics_but_falls_back_to_the_line_span() {
        let mut n = node("f", Some("src/a.rs"));
        n.start_line = Some(10);
        n.end_line = Some(20);
        let f = compute(&n, &ctx_of(vec![]));
        // 10..=20 inclusive is 11 lines, not 10.
        assert_eq!(f["loc"], FactValue::Int(11));

        n.metrics = Some(SymbolMetrics {
            loc: 7,
            params: 2,
            max_nesting: 1,
            ..Default::default()
        });
        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["loc"], FactValue::Int(7), "metrics win over the span");
        assert_eq!(f["params"], FactValue::Int(2));
        assert_eq!(f["max_nesting"], FactValue::Int(1));
    }

    /// Class nodes carry no metrics, so without the span fallback every
    /// "how big are the classes" question would return nothing.
    #[test]
    fn nodes_without_metrics_still_get_a_size() {
        let mut n = node("C", Some("src/a.rs"));
        n.node_type = GraphNodeType::Class;
        n.start_line = Some(1);
        n.end_line = Some(50);
        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["loc"], FactValue::Int(50));
    }

    #[test]
    fn a_node_with_no_line_range_has_no_loc() {
        let f = compute(&node("f", Some("src/a.rs")), &ctx_of(vec![]));
        assert!(!f.contains_key("loc"), "absent, not zero");
    }

    #[test]
    fn booleans_are_stored_as_ints_so_they_can_be_summed() {
        let mut n = node("f", Some("src/a.rs"));
        assert_eq!(compute(&n, &ctx_of(vec![]))["has_doc"], FactValue::Int(0));
        n.docstring = Some("what it does".into());
        assert_eq!(compute(&n, &ctx_of(vec![]))["has_doc"], FactValue::Int(1));
    }

    #[test]
    fn whitespace_only_docstrings_do_not_count_as_documentation() {
        let mut n = node("f", Some("src/a.rs"));
        n.docstring = Some("   \n  ".into());
        assert_eq!(compute(&n, &ctx_of(vec![]))["has_doc"], FactValue::Int(0));
    }

    #[test]
    fn test_files_are_detected_at_any_depth_including_the_repo_root() {
        for path in [
            "tests/foo.rs",
            "native/tests/foo.rs",
            "src/__tests__/a.ts",
            "src/a.test.ts",
            "src/a_test.go",
            "src/test_thing.py",
        ] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f["is_test"], FactValue::Int(1), "{path} should read as test");
        }
        for path in ["src/latest.rs", "src/contest.ts", "src/a.rs"] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(
                f["is_test"],
                FactValue::Int(0),
                "{path} should not read as test"
            );
        }
    }

    #[test]
    fn folder_is_the_parent_dir_and_empty_at_the_repo_root() {
        let f = compute(&node("f", Some("a/b/c.rs")), &ctx_of(vec![]));
        assert_eq!(f["folder"], FactValue::Str("a/b".into()));
        let f = compute(&node("f", Some("c.rs")), &ctx_of(vec![]));
        assert_eq!(f["folder"], FactValue::Str("".into()));
    }

    /// The census groups on this, so a spelling that varies with the
    /// author's shift key would split one extension across two rows —
    /// and a dot in a directory name must not be read as one.
    #[test]
    fn extension_is_lowercase_bare_and_absent_when_there_is_none() {
        for (path, want) in [
            ("a/b/c.rs", "rs"),
            ("src/App.TSX", "tsx"),
            ("src/a.test.ts", "ts"),
            ("docs/v1.2/readme.md", "md"),
        ] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f["extension"], FactValue::Str(want.into()), "{path}");
        }
        // Never indexed in the first place — `indexer::process_file`
        // returns `None` without an extension — so the fact is absent
        // rather than a `""` group nothing can explain.
        for path in ["Makefile", ".gitignore", "src/LICENSE"] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f.get("extension"), None, "{path}");
        }
    }

    /// `Contains` is folder→file→symbol structure. Counting it would give
    /// every symbol in the repo an in-degree of at least 1 and make
    /// "what does nothing depend on" answer "nothing".
    #[test]
    fn degrees_ignore_contains_edges() {
        let ctx = ctx_of(vec![
            edge("file:a", "f", GraphEdgeType::Contains),
            edge("caller", "f", GraphEdgeType::Calls),
            edge("other", "f", GraphEdgeType::References),
            edge("f", "callee", GraphEdgeType::Calls),
        ]);
        let f = compute(&node("f", Some("src/a.rs")), &ctx);
        assert_eq!(f["in_degree"], FactValue::Int(2), "Calls + References only");
        assert_eq!(f["out_degree"], FactValue::Int(1));
    }

    #[test]
    fn unreferenced_nodes_report_zero_rather_than_nothing() {
        let f = compute(&node("f", Some("src/a.rs")), &ctx_of(vec![]));
        // Absent would make `where in_degree = 0` (dead-code sweeps) miss
        // exactly the nodes it is looking for.
        assert_eq!(f["in_degree"], FactValue::Int(0));
        assert_eq!(f["out_degree"], FactValue::Int(0));
    }

    #[test]
    fn optional_java_facts_appear_only_when_present() {
        let mut n = node("f", Some("src/A.java"));
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("qualified_name"));
        assert!(!f.contains_key("route"));
        assert!(!f.contains_key("annotations"));

        n.qualified_name = Some("com.x.A#f".into());
        n.route = Some("GET /a".into());
        n.annotations = vec![
            Annotation {
                name: "Test".into(),
                args: None,
            },
            Annotation {
                name: "Override".into(),
                args: None,
            },
        ];
        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["qualified_name"], FactValue::Str("com.x.A#f".into()));
        assert_eq!(f["route"], FactValue::Str("GET /a".into()));
        assert_eq!(f["annotations"], FactValue::Str("Test,Override".into()));
    }

    /// The single most important behaviour in this module.
    ///
    /// A graph indexed before comment metrics existed has `comment_lines:
    /// 0` on every symbol, because that is what `#[serde(default)]` does.
    /// Storing that would answer "how many functions have comments" with a
    /// confident, wrong zero. Omitting it makes the property report as NOT
    /// INDEXED in every answer's coverage line instead.
    #[test]
    fn line_metrics_are_omitted_on_a_graph_too_old_to_have_them() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(0, 0, 0);

        let old = compute(&n, &ctx_of(vec![]));
        for key in ["comment_lines", "doc_lines", "code_lines", "has_comments"] {
            assert!(
                !old.contains_key(key),
                "{key} must be absent, not zero, on a pre-v2 graph"
            );
        }
        // Facts that always existed are unaffected.
        assert_eq!(old["params"], FactValue::Int(1));
    }

    #[test]
    fn line_metrics_are_stored_on_a_current_graph() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(6, 3, 22);

        let f = compute(&n, &ctx_at_version(2, vec![]));
        assert_eq!(f["comment_lines"], FactValue::Int(6));
        assert_eq!(f["doc_lines"], FactValue::Int(3));
        assert_eq!(f["code_lines"], FactValue::Int(22));
        assert_eq!(f["has_comments"], FactValue::Int(1));
    }

    /// `has_doc` and `has_comments` measure different things, and the gap
    /// between them is usually the finding: a function explained entirely
    /// in inline comments is undocumented by one measure and commented by
    /// the other.
    #[test]
    fn inline_comments_count_as_commented_but_not_as_documented() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(9, 0, 30);

        let f = compute(&n, &ctx_at_version(2, vec![]));
        assert_eq!(f["has_comments"], FactValue::Int(1));
        assert_eq!(f["has_doc"], FactValue::Int(0), "no doc comment");
    }

    #[test]
    fn a_symbol_with_no_prose_at_all_reports_neither() {
        let mut n = node("f", Some("src/a.rs"));
        n.metrics = with_line_metrics(0, 0, 30);

        let f = compute(&n, &ctx_at_version(2, vec![]));
        assert_eq!(f["has_comments"], FactValue::Int(0));
        assert_eq!(f["has_doc"], FactValue::Int(0));
    }

    fn boundary(kind: &str, protocol: &str, direction: BoundaryDirection, detail: &str) -> Boundary {
        Boundary {
            kind: kind.into(),
            direction,
            protocol: protocol.into(),
            detail: Some(detail.into()),
            source: "test".into(),
        }
    }

    #[test]
    fn a_boundary_becomes_flags_and_joined_strings() {
        let mut n = node("f", Some("src/a.rs"));
        n.boundaries = vec![
            boundary(
                "http.endpoint",
                "http",
                BoundaryDirection::Inbound,
                "GET /orders",
            ),
            boundary("db.access", "jdbc", BoundaryDirection::Outbound, "orders"),
        ];

        let f = compute(&n, &ctx_at_version(4, vec![]));
        assert_eq!(f["boundary"], FactValue::Int(1));
        assert_eq!(f["boundary_in"], FactValue::Int(1));
        assert_eq!(f["boundary_out"], FactValue::Int(1));
        assert_eq!(
            f["boundary_kinds"],
            FactValue::Str("http.endpoint,db.access".into())
        );
        assert_eq!(f["boundary_protocols"], FactValue::Str("http,jdbc".into()));
        assert_eq!(
            f["boundary_detail"],
            FactValue::Str("GET /orders,orders".into())
        );
    }

    #[test]
    fn a_symbol_that_is_no_boundary_reports_a_measured_zero() {
        let n = node("f", Some("src/a.rs"));

        let f = compute(&n, &ctx_at_version(4, vec![]));
        assert_eq!(f["boundary"], FactValue::Int(0));
        assert_eq!(f["boundary_in"], FactValue::Int(0));
        assert_eq!(f["boundary_out"], FactValue::Int(0));
        // The descriptive columns stay absent: there is nothing to describe,
        // and an empty string would be a value queries could match on.
        for key in ["boundary_kinds", "boundary_protocols", "boundary_detail"] {
            assert!(!f.contains_key(key), "{key} should be absent");
        }
    }

    /// The failure this gating exists to prevent. Most symbols in any repo
    /// are not boundaries, so a graph that never looked and a graph that
    /// looked and found none produce the same `0` — and only one of them is
    /// an answer.
    #[test]
    fn a_graph_predating_boundaries_omits_them_rather_than_reporting_none() {
        let mut n = node("f", Some("src/a.rs"));
        n.boundaries = vec![boundary(
            "http.endpoint",
            "http",
            BoundaryDirection::Inbound,
            "GET /orders",
        )];

        let old = compute(&n, &ctx_at_version(3, vec![]));
        for key in [
            "boundary",
            "boundary_in",
            "boundary_out",
            "boundary_kinds",
            "boundary_protocols",
            "boundary_detail",
        ] {
            assert!(
                !old.contains_key(key),
                "{key} must be absent, not zero, on a pre-v4 graph"
            );
        }
    }

    #[test]
    fn repeated_boundary_kinds_are_recorded_once() {
        // A route-registration function declares six endpoints. It is one
        // `http.endpoint`, not six, or `boundary_census` would count the
        // function once per route it happens to register.
        let mut n = node("routes", Some("src/a.rs"));
        n.boundaries = (0..6)
            .map(|i| {
                boundary(
                    "http.endpoint",
                    "http",
                    BoundaryDirection::Inbound,
                    &format!("GET /r{i}"),
                )
            })
            .collect();

        let f = compute(&n, &ctx_at_version(4, vec![]));
        assert_eq!(f["boundary_kinds"], FactValue::Str("http.endpoint".into()));
        // Details are all distinct, so all six survive — that is the list
        // someone reads to find the route they care about.
        assert_eq!(
            f["boundary_detail"],
            FactValue::Str("GET /r0,GET /r1,GET /r2,GET /r3,GET /r4,GET /r5".into())
        );
    }

    #[test]
    fn language_and_classification_reach_the_store() {
        let mut n = node("f", Some("src/a.rs"));
        n.language = Some("rust".into());
        n.classification = Some(FileClassification::Service);

        let f = compute(&n, &ctx_of(vec![]));
        assert_eq!(f["language"], FactValue::Str("rust".into()));
        assert_eq!(f["classification"], FactValue::Str("service".into()));
    }

    /// The classifier saw the file's contents; the path heuristic only saw
    /// its name. Where they disagree, the classifier wins.
    #[test]
    fn classification_outranks_the_path_heuristic_for_is_test() {
        // A path that looks nothing like a test, classified as one.
        let mut n = node("f", Some("src/checkout.rs"));
        n.classification = Some(FileClassification::Test);
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));

        // No classification at all: fall back to the path, as before.
        let n = node("f", Some("tests/checkout.rs"));
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));

        // The direction this used to get wrong. "Where they disagree the
        // classifier wins" has to hold both ways: a file the classifier
        // positively called `Util` is not a test, however test-like its name
        // reads. Previously the `Some(_)` arm fell through to the path
        // heuristic, so this returned 1 and the file dropped out of every
        // statistic that filters `is_test = 0`.
        let mut n = node("f", Some("src/utils/test_helpers.rs"));
        n.classification = Some(FileClassification::Util);
        assert_eq!(
            compute(&n, &ctx_of(vec![]))["is_test"],
            FactValue::Int(0),
            "a classified non-test must not be relabelled by its path"
        );

        // Same in the other direction: a test-shaped classification on a
        // test-shaped path still agrees.
        let mut n = node("f", Some("tests/checkout.rs"));
        n.classification = Some(FileClassification::Test);
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));
    }

    /// A `#[cfg(test)] mod` in a production file is test code, and only the
    /// indexer's per-symbol marker can say so — the file classifies as
    /// `Service` and its name carries no test marker.
    #[test]
    fn a_cfg_test_annotation_overrides_a_production_classification() {
        let mut n = node("f", Some("src/server.rs"));
        n.classification = Some(FileClassification::Service);
        n.annotations.push(Annotation {
            name: "cfg(test)".into(),
            args: None,
        });
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));

        let mut n = node("f", Some("src/server.rs"));
        n.annotations.push(Annotation {
            name: "test".into(),
            args: None,
        });
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(1));

        // And the marker must not leak: an ordinary annotation never turns
        // a Service file into a test.
        let mut n = node("f", Some("src/server.rs"));
        n.classification = Some(FileClassification::Service);
        n.annotations.push(Annotation {
            name: "allow(dead_code)".into(),
            args: None,
        });
        assert_eq!(compute(&n, &ctx_of(vec![]))["is_test"], FactValue::Int(0));
    }

    /// The bug: `is_test_node` compared the annotation name *exactly* against
    /// `"test"`, so `#[tokio::test]` — the standard Rust async test attribute,
    /// 174 of them in this repository — read as production code unless the
    /// function also sat inside a `#[cfg(test)] mod`. `untested_symbols` then
    /// reported real tests as untested.
    #[test]
    fn a_qualified_test_attribute_is_still_a_test() {
        for name in [
            "tokio::test",
            "async_std::test",
            "actix_web::test",
            // Java and TestNG spell it with a capital.
            "Test",
            "ParameterizedTest",
            "RepeatedTest",
            // Only marker a `FooTestBase` class carries.
            "BeforeEach",
            "AfterAll",
            // Anything pytest touches is test code; the *last* segment here
            // is `asyncio`, which is why the prefix rule exists.
            "pytest.mark.asyncio",
            "pytest.fixture",
        ] {
            let mut n = node("f", Some("src/server.rs"));
            n.classification = Some(FileClassification::Service);
            n.annotations.push(Annotation {
                name: name.into(),
                args: None,
            });
            assert_eq!(
                compute(&n, &ctx_of(vec![]))["is_test"],
                FactValue::Int(1),
                "#[{name}] marks test code"
            );
        }
    }

    /// The matching is on the last segment, so it must not sweep in an
    /// annotation that merely *contains* one of the markers.
    #[test]
    fn an_annotation_that_is_not_a_test_marker_stays_production() {
        for name in [
            "derive",
            "serde",
            "napi",
            "Override",
            "Inject",
            // `latest` ends in "test" — the same mid-word trap the path
            // markers already guard against, one layer up.
            "latest",
            "contest",
            "protest",
            // A source annotation that only ever accompanies a real marker
            // must not count on its own — it appears on helper methods too.
            "MethodSource",
            "ValueSource",
        ] {
            let mut n = node("f", Some("src/server.rs"));
            n.classification = Some(FileClassification::Service);
            n.annotations.push(Annotation {
                name: name.into(),
                args: None,
            });
            assert_eq!(
                compute(&n, &ctx_of(vec![]))["is_test"],
                FactValue::Int(0),
                "#[{name}] is not a test marker"
            );
        }
    }

    /// `router_tests.rs` and `chat_api_tests.rs` are this repository's own
    /// spelling, and the singular `_test.` marker missed every one of them.
    #[test]
    fn the_plural_filename_forms_read_as_tests() {
        for path in [
            "src/serve/router_tests.rs",
            "src/serve/chat_api_tests.rs",
            "lib/foo.tests.ts",
            "lib/bar_specs.rb",
        ] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f["is_test"], FactValue::Int(1), "{path} should read as test");
        }

        // And the plural marker must stay anchored on its underscore, the
        // same way the singular one is.
        for path in ["src/my_latests.rs", "src/protests.py", "src/contests.go"] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(
                f["is_test"],
                FactValue::Int(0),
                "{path} is production code, not a test"
            );
        }
    }

    /// `test_` was matched anywhere in the path, so ordinary words ending in
    /// "test" swept real code into the test bucket. Markers are anchored now.
    #[test]
    fn test_marker_does_not_match_mid_word() {
        for path in [
            "src/config/latest_version.rs",
            "src/greatest_hits.rs",
            "src/fastest_path.ts",
            "src/protest_form.tsx",
            "src/contest_rules.py",
        ] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(
                f["is_test"],
                FactValue::Int(0),
                "{path} is production code, not a test"
            );
        }

        // The real case the marker exists for still matches: `test_` at the
        // start of a filename.
        for path in ["src/test_thing.py", "test_top_level.py"] {
            let f = compute(&node("f", Some(path)), &ctx_of(vec![]));
            assert_eq!(f["is_test"], FactValue::Int(1), "{path} should read as test");
        }
    }

    /// A Rust struct's methods live in a separate `impl` block, so it has
    /// no `Contains` edges. Reporting `members: 0` would rank every Rust
    /// type as memberless against Java types that genuinely nest theirs.
    #[test]
    fn members_is_absent_rather_than_zero_when_the_language_does_not_nest() {
        let mut n = node("class:src/a.rs:S", Some("src/a.rs"));
        n.node_type = GraphNodeType::Class;
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("members"));

        let ctx = ctx_of(vec![
            edge("class:src/a.rs:S", "fn:one", GraphEdgeType::Contains),
            edge("class:src/a.rs:S", "fn:two", GraphEdgeType::Contains),
        ]);
        assert_eq!(compute(&n, &ctx)["members"], FactValue::Int(2));
    }

    #[test]
    fn only_types_get_a_members_fact() {
        let ctx = ctx_of(vec![edge("f", "g", GraphEdgeType::Contains)]);
        // A File also has Contains edges, but "members" is a property of a
        // type, and a File already reports its symbol count another way.
        let f = compute(&node("f", Some("src/a.rs")), &ctx);
        assert!(!f.contains_key("members"), "Function nodes have no members");
    }

    #[test]
    fn folder_nodes_get_no_self_referential_folder_fact() {
        let mut n = node("folder:src", Some("src"));
        n.node_type = GraphNodeType::Folder;
        let f = compute(&n, &ctx_of(vec![]));
        assert!(!f.contains_key("folder"));
        assert!(!f.contains_key("is_test"));
    }


    /// A context from a graph that actually has nodes, which is what
    /// `name_mentions` is derived from.
    fn ctx_of_nodes(nodes: Vec<GraphNode>) -> FactContext<'static> {
        let g: &'static GraphData = Box::leak(Box::new(GraphData {
            nodes,
            edges: vec![],
            stats: None,
            resolution: None,
        }));
        FactContext::new(g)
    }

    /// The point of the fact. `render` is called by `draw`, but the
    /// resolver could not place the callee, so no edge exists and
    /// `in_degree` is 0. The raw name is still sitting in `draw.calls`,
    /// and that is the difference between unresolved and unused.
    #[test]
    fn an_unresolved_call_still_counts_as_a_mention() {
        let mut caller = node("draw", Some("src/a.rs"));
        caller.calls = vec!["render".into()];
        let target = node("render", Some("src/b.rs"));

        let ctx = ctx_of_nodes(vec![caller, target.clone()]);
        assert_eq!(compute(&target, &ctx)["in_degree"], FactValue::Int(0));
        assert_eq!(compute(&target, &ctx)["name_mentions"], FactValue::Int(1));
    }

    /// Qualifiers are dropped on both sides: the source wrote
    /// `self.open(..)` or `Db::open(..)`, and matching that to a
    /// definition is the resolution step that has already failed.
    #[test]
    fn a_qualified_call_matches_the_short_definition_name() {
        for spelling in ["self.open", "Db::open", "open", "[Symbol.open]"] {
            let mut caller = node("run", Some("src/a.rs"));
            caller.calls = vec![spelling.into()];
            let target = node("Db::open", Some("src/b.rs"));

            let ctx = ctx_of_nodes(vec![caller, target.clone()]);
            assert_eq!(
                compute(&target, &ctx)["name_mentions"],
                FactValue::Int(1),
                "{spelling} should reach Db::open"
            );
        }
    }

    /// Otherwise every recursive function, and every symbol whose own doc
    /// comment names it, would mention itself out of the candidate list.
    #[test]
    fn a_symbol_never_mentions_itself_into_life() {
        let mut n = node("recurse", Some("src/a.rs"));
        n.calls = vec!["recurse".into()];
        n.docstring = Some("`recurse` recurses.".into());

        let ctx = ctx_of_nodes(vec![n.clone()]);
        assert_eq!(compute(&n, &ctx)["name_mentions"], FactValue::Int(0));
    }

    /// A `serde` payload is never *called*; it appears as somebody's
    /// parameter or return type and arrives deserialised. Reading
    /// signatures is what keeps those off the list.
    #[test]
    fn a_type_named_only_in_a_signature_is_mentioned() {
        let mut user = node("handler", Some("src/a.rs"));
        user.signature = Some(crate::types::GraphNodeSignature {
            params: vec![crate::types::Param {
                name: "body".into(),
                param_type: Some("Json<SearchArgs>".into()),
                optional: false,
                default: None,
            }],
            return_type: Some("Result<Reply, Error>".into()),
        });
        let args = node("SearchArgs", Some("src/b.rs"));
        let reply = node("Reply", Some("src/b.rs"));
        let unrelated = node("Unmentioned", Some("src/b.rs"));

        let ctx = ctx_of_nodes(vec![user, args.clone(), reply.clone(), unrelated.clone()]);
        assert_eq!(compute(&args, &ctx)["name_mentions"], FactValue::Int(1));
        assert_eq!(compute(&reply, &ctx)["name_mentions"], FactValue::Int(1));
        assert_eq!(compute(&unrelated, &ctx)["name_mentions"], FactValue::Int(0));
    }

    /// Prose counts. A symbol another symbol's doc comment explains is
    /// not forgotten code, and the list exists to be read by someone who
    /// then has to decide.
    #[test]
    fn a_name_that_survives_only_in_prose_is_mentioned() {
        let mut explainer = node("caller", Some("src/a.rs"));
        explainer.docstring = Some("Superseded by `fast_path`; see the note.".into());
        let target = node("fast_path", Some("src/b.rs"));

        let ctx = ctx_of_nodes(vec![explainer, target.clone()]);
        assert_eq!(compute(&target, &ctx)["name_mentions"], FactValue::Int(1));
    }

    /// Doc comments in this repository contain `×`, `→` and em dashes.
    /// Walking the prose a byte at a time to find identifier runs lands
    /// inside one of them and panics, which is how this was found.
    #[test]
    fn multibyte_prose_does_not_split_a_char() {
        let mut explainer = node("caller", Some("src/a.rs"));
        explainer.docstring = Some("rust×150 — files → target_fn, ≈2×".into());
        let target = node("target_fn", Some("src/b.rs"));

        let ctx = ctx_of_nodes(vec![explainer, target.clone()]);
        assert_eq!(compute(&target, &ctx)["name_mentions"], FactValue::Int(1));
    }

    /// A File node's `name` is a path, whose last identifier is an
    /// extension. Counting how often the word `rs` appears is noise
    /// wearing a fact's clothes, so the fact is absent rather than wrong.
    #[test]
    fn containers_carry_no_name_mentions() {
        for t in [GraphNodeType::File, GraphNodeType::Folder] {
            let mut n = node("src/a.rs", Some("src/a.rs"));
            n.node_type = t;
            assert!(!compute(&n, &ctx_of_nodes(vec![])).contains_key("name_mentions"));
        }

        let n = node("f", Some("src/a.rs"));
        assert!(compute(&n, &ctx_of_nodes(vec![])).contains_key("name_mentions"));
    }
}
