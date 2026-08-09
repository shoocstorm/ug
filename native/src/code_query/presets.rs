//! The built-in preset registry: named statistical questions, each one a
//! GQL string.
//!
//! Presets exist because composing a query costs an agent a few hundred
//! reasoning tokens and calling one by name costs about twenty. They are
//! also the honest place to encode the things that are easy to get wrong
//! and impossible to notice — see [`IMPACT_EDGES`] and the `DISTINCT`
//! note on the reachability presets.
//!
//! **Every preset here must run against the facts ingest actually
//! writes** — the authoritative list is
//! [`crate::code_query::QUERYABLE_PROPERTIES`], and
//! `no_builtin_preset_reads_an_unindexed_property` enforces it. Querying
//! anything else does not error; it returns a confident zero. A preset
//! that reaches for a property the indexer does not yet produce is a
//! shipped bug, not a forward-looking one.
//!
//! **List presets return up to 200 rows, not 20.** Row ranges (see
//! [`crate::code_query::range`]) are a window over what the query
//! returned, so a `LIMIT 30` preset could never show row 31 however the
//! caller asked. Only the visible window is ever formatted, so the wider
//! limit costs memory rather than tokens.

/// Edge labels that mean "depends on", for reachability presets.
///
/// `Contains` is deliberately absent. It is pure structure
/// (Folder→File→Symbol), so including it would make every symbol in a
/// file a "dependent" of its neighbours and turn a blast radius into a
/// directory listing.
pub const IMPACT_EDGES: &str = "Calls|References|Imports|Extends|Implements|Overrides";

/// Node types that are code, as a GQL list literal.
///
/// Markdown headings are indexed as `Concept` nodes — 362 of 2280 in this
/// repo — so a statistic that forgets to exclude them is wrong on any
/// project with docs. `File` and `Folder` are containers, not symbols.
pub const CODE_TYPES: &str = "['Function', 'Class', 'Interface', 'Constant', 'Variable']";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Census,
    Size,
    Documentation,
    DeadCode,
    Architecture,
    Tests,
    Risk,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Census => "census",
            Category::Size => "size",
            Category::Documentation => "documentation",
            Category::DeadCode => "dead code",
            Category::Architecture => "architecture",
            Category::Tests => "tests",
            Category::Risk => "risk",
        }
    }
}

/// A preset argument, bound as a GQL parameter — never interpolated into
/// the query text.
#[derive(Debug, Clone, Copy)]
pub struct PresetParam {
    pub name: &'static str,
    pub description: &'static str,
    /// `None` makes the parameter required.
    pub default: Option<ParamValue>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamValue {
    Int(i64),
    Str(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub name: &'static str,
    pub category: Category,
    /// One line, written for an agent choosing between presets.
    pub description: &'static str,
    pub params: &'static [PresetParam],
    pub gql: &'static str,
    /// Column whose first-row value summarises the whole answer, for the
    /// headline and for the viz layer's preset cards. `None` when the
    /// answer is inherently a table.
    pub headline: Option<&'static str>,
}

pub fn find(name: &str) -> Option<&'static Preset> {
    BUILTIN.iter().find(|p| p.name == name)
}

pub fn all() -> &'static [Preset] {
    BUILTIN
}

const NO_PARAMS: &[PresetParam] = &[];

const MIN_LOC: &[PresetParam] = &[PresetParam {
    name: "min_loc",
    description: "Line-span threshold (a span, so it counts blanks and comments).",
    default: Some(ParamValue::Int(50)),
}];

const TARGET: &[PresetParam] = &[PresetParam {
    name: "target",
    description: "Repo-relative file path whose dependents you want, e.g. 'src/auth.ts'.",
    default: None,
}];

pub static BUILTIN: &[Preset] = &[
    // ── census ────────────────────────────────────────────────────────
    Preset {
        name: "repo_census",
        category: Category::Census,
        description: "What this repo is made of: indexed nodes by type.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              RETURN n.node_type AS kind, count(*) AS symbols \
              ORDER BY symbols DESC",
        headline: None,
    },
    Preset {
        name: "biggest_files",
        category: Category::Census,
        description: "Files with the most indexed symbols — where the mass is.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type <> 'File' AND n.node_type <> 'Folder' AND n.file <> '' \
              RETURN n.file AS file, count(*) AS symbols \
              ORDER BY symbols DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "language_breakdown",
        category: Category::Census,
        description: "What this repo is written in: symbols and code lines per language.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.language IS NOT NULL AND n.node_type <> 'File' AND n.node_type <> 'Folder' \
              RETURN n.language AS language, count(*) AS symbols, \
                     sum(n.code_lines) AS code_lines \
              ORDER BY symbols DESC",
        headline: None,
    },
    Preset {
        name: "file_kinds",
        category: Category::Census,
        description: "What the files in this repo are for — the indexer's own classification.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.classification IS NOT NULL AND n.node_type <> 'Folder' \
              RETURN n.classification AS kind, count(*) AS symbols \
              ORDER BY symbols DESC",
        headline: None,
    },
    Preset {
        name: "where_to_start",
        category: Category::Census,
        description: "Documented, heavily depended-upon symbols — the reading order for a newcomer.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.has_doc = 1 AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC \
              LIMIT 200",
        headline: None,
    },
    // ── size and shape ────────────────────────────────────────────────
    Preset {
        name: "long_functions",
        category: Category::Size,
        description: "Non-test functions longer than min_loc lines, longest first.",
        params: MIN_LOC,
        gql: "MATCH (n:Function) \
              WHERE n.loc > $min_loc AND n.is_test = 0 \
              RETURN elementKey(n) AS id, n.loc AS loc, n.max_nesting AS nesting \
              ORDER BY loc DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "long_functions_by_folder",
        category: Category::Size,
        description: "Where the long functions cluster: count and average length per folder.",
        params: MIN_LOC,
        gql: "MATCH (n:Function) \
              WHERE n.loc > $min_loc AND n.is_test = 0 \
              WITH n.folder AS folder, count(*) AS functions, avg(n.loc) AS avg_loc \
              WHERE functions >= 2 \
              RETURN folder, functions, avg_loc \
              ORDER BY functions DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "size_histogram",
        category: Category::Size,
        description: "Distribution of function length across the whole repo.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              RETURN CASE \
                       WHEN n.loc > 200 THEN 'e. 200+' \
                       WHEN n.loc > 100 THEN 'd. 101-200' \
                       WHEN n.loc > 50  THEN 'c. 51-100' \
                       WHEN n.loc > 20  THEN 'b. 21-50' \
                       ELSE 'a. 0-20' \
                     END AS bucket, \
                     count(*) AS functions \
              ORDER BY bucket ASC",
        headline: None,
    },
    Preset {
        name: "god_classes",
        category: Category::Size,
        description: "The largest classes, structs, traits and interfaces by line span.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.loc AS loc, n.out_degree AS depends_on \
              ORDER BY loc DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "long_functions_by_code",
        category: Category::Size,
        description: "Functions over min_loc lines of ACTUAL code — blanks and comments excluded, unlike long_functions.",
        params: MIN_LOC,
        gql: "MATCH (n:Function) \
              WHERE n.code_lines > $min_loc AND n.is_test = 0 \
              RETURN elementKey(n) AS id, n.code_lines AS code_lines, n.loc AS span, \
                     n.max_nesting AS nesting \
              ORDER BY code_lines DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "classes_by_members",
        category: Category::Size,
        // Only meaningful where the language nests members inside the type
        // body. A Rust struct's methods live in a separate `impl` block, so
        // it has no members fact at all and the coverage line will say so
        // rather than ranking every Rust type as memberless.
        description: "Types with the most declared members. Only populated for languages that nest members in the type body (Java, Python, TS) — check coverage.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Class', 'Interface'] AND n.members IS NOT NULL \
              RETURN elementKey(n) AS id, n.members AS members, n.loc AS loc \
              ORDER BY members DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "param_bloat",
        category: Category::Size,
        description: "Functions taking more than min_params arguments.",
        params: &[PresetParam {
            name: "min_params",
            description: "Parameter-count threshold.",
            default: Some(ParamValue::Int(5)),
        }],
        gql: "MATCH (n:Function) \
              WHERE n.params > $min_params \
              RETURN elementKey(n) AS id, n.params AS params, n.loc AS loc \
              ORDER BY params DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "deep_nesting",
        category: Category::Size,
        description: "Functions nested at least min_depth levels deep — the hard-to-follow code.",
        params: &[PresetParam {
            name: "min_depth",
            description: "Nesting-depth threshold.",
            default: Some(ParamValue::Int(4)),
        }],
        gql: "MATCH (n:Function) \
              WHERE n.max_nesting >= $min_depth \
              RETURN elementKey(n) AS id, n.max_nesting AS nesting, n.loc AS loc \
              ORDER BY nesting DESC \
              LIMIT 200",
        headline: None,
    },
    // ── documentation ─────────────────────────────────────────────────
    //
    // Two different questions live here and they are easy to confuse.
    // `has_doc` is a *doc comment* flag; `has_comments` also counts inline
    // prose. A function with twenty lines of `//` explaining a subtle
    // algorithm and no leading doc block is undocumented by the first
    // measure and well commented by the second. Presets say which they
    // mean in their description, because the gap between the two numbers
    // is often the actual finding.
    Preset {
        name: "comment_coverage",
        category: Category::Documentation,
        description: "How many symbols carry any prose at all — doc comment or inline — by type.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN n.node_type AS kind, \
                     count(*) AS total, \
                     sum(n.has_comments) AS commented, \
                     sum(n.has_doc) AS with_doc_comment \
              ORDER BY total DESC",
        headline: None,
    },
    Preset {
        name: "comment_density",
        category: Category::Documentation,
        description: "Comment-to-code line ratio per folder — where the prose actually is.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WHERE n.is_test = 0 \
              WITH n.folder AS folder, \
                   sum(n.code_lines) AS code_lines, \
                   sum(n.comment_lines) AS comment_lines, \
                   sum(n.doc_lines) AS doc_lines \
              WHERE code_lines > 50 \
              RETURN folder, code_lines, comment_lines, doc_lines \
              ORDER BY code_lines DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "token_docs",
        category: Category::Documentation,
        description: "Symbols whose doc comment is a single line — present, but saying nothing.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.doc_lines = 1 AND n.is_test = 0 \
                AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.code_lines AS code_lines, \
                     n.in_degree AS depended_on_by \
              ORDER BY code_lines DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "undercommented_complexity",
        category: Category::Documentation,
        description: "Long, deeply nested functions with no prose of any kind — the hardest code to pick up.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WHERE n.has_comments = 0 AND n.is_test = 0 \
                AND n.code_lines > 40 AND n.max_nesting >= 3 \
              RETURN elementKey(n) AS id, n.code_lines AS code_lines, \
                     n.max_nesting AS nesting \
              ORDER BY code_lines DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "doc_coverage",
        category: Category::Documentation,
        description: "How many symbols of each type carry a doc comment.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN n.node_type AS kind, count(*) AS total, sum(n.has_doc) AS documented \
              ORDER BY total DESC",
        headline: None,
    },
    Preset {
        name: "doc_coverage_by_folder",
        category: Category::Documentation,
        description: "Which folders are worst documented, least-documented first.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Function', 'Class', 'Interface'] AND n.is_test = 0 \
              WITH n.folder AS folder, count(*) AS total, sum(n.has_doc) AS documented \
              WHERE total >= 5 \
              RETURN folder, total, documented \
              ORDER BY documented ASC, total DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "undocumented_hotspots",
        category: Category::Documentation,
        description: "Undocumented symbols that many others depend on — the worst docs gaps.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.has_doc = 0 AND n.is_test = 0 \
                AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC \
              LIMIT 200",
        headline: None,
    },
    // ── dead code ─────────────────────────────────────────────────────
    //
    // `in_degree` counts resolved structural edges only. A symbol reached
    // by dynamic dispatch, reflection, or a string-keyed lookup has an
    // in-degree of zero and is not dead. These presets find *candidates*.
    Preset {
        name: "dead_code",
        category: Category::DeadCode,
        description: "Non-test symbols nothing resolves an edge to. Candidates, not proof — dynamic dispatch is invisible here.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.in_degree = 0 AND n.is_test = 0 \
                AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.loc AS loc \
              ORDER BY loc DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "orphan_files",
        category: Category::DeadCode,
        description: "Files nothing imports or references.",
        params: NO_PARAMS,
        gql: "MATCH (n:File) \
              WHERE n.in_degree = 0 \
              RETURN elementKey(n) AS id \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "duplicate_names",
        category: Category::DeadCode,
        description: "The same function name defined in many places — possible duplication or a naming convention.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WITH n.name AS name, count(*) AS definitions \
              WHERE definitions > 3 \
              RETURN name, definitions \
              ORDER BY definitions DESC \
              LIMIT 200",
        headline: None,
    },
    // ── architecture ──────────────────────────────────────────────────
    Preset {
        name: "dependency_fanin",
        category: Category::Architecture,
        description: "The most depended-upon symbols in the repo.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "fanout_offenders",
        category: Category::Architecture,
        description: "Symbols that reach out to more than min_fanout others — the code that touches everything.",
        params: &[PresetParam {
            name: "min_fanout",
            description: "Outbound-edge threshold.",
            default: Some(ParamValue::Int(20)),
        }],
        gql: "MATCH (n) \
              WHERE n.out_degree > $min_fanout AND n.node_type <> 'File' AND n.node_type <> 'Folder' \
              RETURN elementKey(n) AS id, n.out_degree AS depends_on, n.loc AS loc \
              ORDER BY depends_on DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "coupling_matrix",
        category: Category::Architecture,
        description: "Which folders depend on which, by edge count across the folder boundary.",
        params: NO_PARAMS,
        gql: "MATCH (a)-[:Calls|References|Imports|Extends|Implements]->(b) \
              WHERE a.folder <> b.folder \
              RETURN a.folder AS from_folder, b.folder AS to_folder, count(*) AS edges \
              ORDER BY edges DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "layering_violations",
        category: Category::Architecture,
        description: "Edges from one layer straight into another it should not reach — pass two path prefixes.",
        params: &[
            PresetParam {
                name: "from_prefix",
                description: "Folder prefix of the calling layer, e.g. 'src/ui'.",
                default: None,
            },
            PresetParam {
                name: "to_prefix",
                description: "Folder prefix that layer should not reach directly, e.g. 'src/db'.",
                default: None,
            },
        ],
        gql: "MATCH (a)-[:Calls|References|Imports]->(b) \
              WHERE a.folder STARTS WITH $from_prefix AND b.folder STARTS WITH $to_prefix \
              RETURN a.file AS from_file, b.file AS to_file, count(*) AS edges \
              ORDER BY edges DESC \
              LIMIT 200",
        headline: None,
    },
    // ── boundaries ────────────────────────────────────────────────────
    Preset {
        name: "boundary_census",
        category: Category::Architecture,
        description: "What surfaces this system exposes and consumes, by kind and direction.",
        params: NO_PARAMS,
        // Grouping on the joined string rather than on a single kind: a
        // symbol can be several things at once (an endpoint that also calls
        // out), and splitting that into one row per kind would double-count
        // the symbol. The combination is the honest unit.
        gql: "MATCH (n) \
              WHERE n.boundary = 1 \
              RETURN n.boundary_kinds AS kinds, \
                     n.boundary_protocols AS protocols, \
                     sum(n.boundary_in) AS inbound, \
                     sum(n.boundary_out) AS outbound, \
                     count(*) AS symbols \
              ORDER BY symbols DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "boundaries",
        category: Category::Architecture,
        description: "Every system entry and exit point — REST handlers, queue listeners, CLI commands, outbound clients.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.boundary = 1 \
              RETURN elementKey(n) AS id, \
                     n.boundary_kinds AS kinds, \
                     n.boundary_detail AS surface, \
                     n.file AS file \
              ORDER BY kinds, file \
              LIMIT 200",
        headline: None,
    },
    // ── tests ─────────────────────────────────────────────────────────
    Preset {
        name: "test_ratio",
        category: Category::Tests,
        description: "Test versus source function counts per folder.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WITH n.folder AS folder, count(*) AS functions, sum(n.is_test) AS tests \
              WHERE functions >= 5 \
              RETURN folder, functions, tests \
              ORDER BY functions DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "untested_symbols",
        category: Category::Tests,
        description: "Source functions no test reaches within 2 hops, most depended-upon first.",
        params: NO_PARAMS,
        // Two things here are the result of the query failing outright,
        // not of taste:
        //
        // 1. The subquery needs its own RETURN. `EXISTS { MATCH … WHERE … }`
        //    is a parse error in this engine.
        // 2. The bound is 2 hops, not 3. This is the one preset with an
        //    *unanchored* variable-length walk — it expands from every
        //    test symbol rather than from one named file — and at 3 hops
        //    that exceeds the engine's frontier cap and errors on a repo
        //    this size. Two hops answers the same question in ~180ms.
        //    Widening it is not a tuning knob; it is how this breaks.
        gql: "MATCH (n:Function) \
              WHERE n.is_test = 0 AND n.in_degree > 0 \
                AND NOT EXISTS { \
                      MATCH (t)-[:Calls|References*1..2]->(n) WHERE t.is_test = 1 RETURN t \
                    } \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "retest_scope",
        category: Category::Tests,
        description: "Which test files exercise code reachable from a target file — what to re-run after changing it.",
        params: TARGET,
        gql: "MATCH (dep)-[:Calls|References|Imports|Extends|Implements|Overrides*1..3]->(t) \
              WHERE t.file = $target AND dep.is_test = 1 \
              RETURN dep.file AS test_file, count(DISTINCT elementKey(dep)) AS test_symbols \
              ORDER BY test_symbols DESC \
              LIMIT 200",
        headline: None,
    },
    // ── risk ──────────────────────────────────────────────────────────
    Preset {
        name: "impact",
        category: Category::Risk,
        description: "Blast radius of changing a file: which files hold symbols that reach it within 3 hops.",
        params: TARGET,
        // `count(DISTINCT elementKey(dep))` rather than `count(*)`: a
        // variable-length match yields one row per *path*, so a plain
        // count reports the number of routes to the target, not the
        // number of dependents. On this repo that is the difference
        // between 948 and 11.
        gql: "MATCH (dep)-[:Calls|References|Imports|Extends|Implements|Overrides*1..3]->(t) \
              WHERE t.file = $target AND dep.file <> $target \
              RETURN dep.file AS file, \
                     count(DISTINCT elementKey(dep)) AS dependents, \
                     sum(dep.is_test) AS test_paths \
              ORDER BY dependents DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "impact_summary",
        category: Category::Risk,
        description: "One-line blast radius for a file: how many symbols and files reach it.",
        params: TARGET,
        gql: "MATCH (dep)-[:Calls|References|Imports|Extends|Implements|Overrides*1..3]->(t) \
              WHERE t.file = $target AND dep.file <> $target \
              RETURN count(DISTINCT elementKey(dep)) AS dependents, \
                     count(DISTINCT dep.file) AS files_affected",
        headline: Some("dependents"),
    },
    Preset {
        name: "boundary_impact",
        category: Category::Risk,
        description: "Which externally-visible surfaces a change to this file reaches — the blast radius that leaves the system.",
        params: TARGET,
        // The question `impact` cannot answer. "11 dependents" says how much
        // code moves; this says how much of what moves is a contract someone
        // outside the repo already depends on, which is what decides whether
        // a change needs a version bump, a migration or a deprecation notice.
        //
        // 4 hops rather than 3: a boundary is usually a controller sitting
        // one layer further out than the callers `impact` is written for.
        // One row per boundary, not per path: a variable-length match yields
        // a row for every route through the graph, so the grouping implied
        // by `count(*)` is what collapses them back to the thing being
        // counted. Same trap the `impact` preset documents.
        gql: "MATCH (b)-[:Calls|References|Imports|Extends|Implements|Overrides*1..4]->(t) \
              WHERE t.file = $target AND b.boundary_in = 1 AND b.file <> $target \
              RETURN elementKey(b) AS surface, \
                     b.boundary_kinds AS kinds, \
                     b.boundary_detail AS exposed_as, \
                     count(*) AS paths \
              ORDER BY paths DESC \
              LIMIT 200",
        headline: None,
    },
    Preset {
        name: "risky_symbols",
        category: Category::Risk,
        description: "Large, undocumented, heavily depended-upon symbols — dangerous to touch.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.in_degree > 5 AND n.loc > 80 AND n.has_doc = 0 \
                AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC \
              LIMIT 200",
        headline: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_names_are_unique() {
        let mut seen: Vec<&str> = BUILTIN.iter().map(|p| p.name).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate preset name");
    }

    /// A parameter the query never binds is dead weight in the manifest;
    /// a `$placeholder` with no declared parameter fails at execution
    /// with an engine error an agent cannot act on.
    #[test]
    fn declared_params_match_the_placeholders_in_the_query() {
        for p in BUILTIN {
            for param in p.params {
                let placeholder = format!("${}", param.name);
                assert!(
                    p.gql.contains(&placeholder),
                    "{}: declares `{}` but never binds it",
                    p.name,
                    param.name
                );
            }
            // Every `$name` in the text must be declared. Scan rather than
            // trust, since the two live in different halves of the record.
            for (i, _) in p.gql.match_indices('$') {
                let name: String = p.gql[i + 1..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                assert!(
                    p.params.iter().any(|q| q.name == name),
                    "{}: uses `${}` but does not declare it",
                    p.name,
                    name
                );
            }
        }
    }

    /// Presets run in ReadOnly mode, so a mutation would be rejected at
    /// execution — but shipping one at all means the preset was never
    /// run, and that is what this catches.
    #[test]
    fn presets_are_read_only() {
        for p in BUILTIN {
            let upper = p.gql.to_ascii_uppercase();
            for verb in ["CREATE ", "MERGE ", "DELETE ", "SET ", "REMOVE ", "DROP "] {
                assert!(
                    !upper.contains(verb),
                    "{}: preset contains the mutating verb {}",
                    p.name,
                    verb.trim()
                );
            }
        }
    }

    /// Every variable-length path needs a finite upper bound: an
    /// unbounded `*` walks to the engine's `max_path_hops` and reports a
    /// truncated blast radius as though it were complete.
    #[test]
    fn variable_length_paths_are_bounded() {
        for p in BUILTIN {
            for (i, _) in p.gql.match_indices('*') {
                // `count(*)` is the aggregate wildcard, not a path bound.
                if p.gql.as_bytes().get(i.wrapping_sub(1)) == Some(&b'(') {
                    continue;
                }
                let bound: String = p.gql[i + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                let (lo, hi) = bound.split_once("..").unwrap_or(("", ""));
                assert!(
                    lo.parse::<u8>().is_ok() && hi.parse::<u8>().is_ok(),
                    "{}: variable-length path `*{}` is not bounded as `*N..M`",
                    p.name,
                    bound
                );
            }
        }
    }

    #[test]
    fn find_resolves_a_known_preset_and_rejects_a_typo() {
        assert!(find("long_functions").is_some());
        assert!(find("long_function").is_none());
    }
}
