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
//! [`crate::analyze::QUERYABLE_PROPERTIES`], and
//! `no_builtin_preset_reads_an_unindexed_property` enforces it. Querying
//! anything else does not error; it returns a confident zero. A preset
//! that reaches for a property the indexer does not yet produce is a
//! shipped bug, not a forward-looking one.
//!
//! **List presets return up to 200 rows, not 20.** Row ranges (see
//! [`crate::analyze::range`]) are a window over what the query
//! returned, so a `LIMIT 30` preset could never show row 31 however the
//! caller asked. Only the visible window is ever formatted, so the wider
//! limit costs memory rather than tokens.

/// Edge labels that mean "depends on" — re-exported so a reader of this
/// file finds the definition, not a fifth copy of it. See
/// [`crate::types::IMPACT_EDGES`] for why there were four.
///
/// Preset queries splice the GQL form in with `concat!` and
/// [`crate::impact_edges_gql`], because a `gql` field is a `&'static str`
/// and `concat!` only takes literals.
pub use crate::impact_edges_gql;
pub use crate::types::IMPACT_EDGES;

/// Node types that are code, as a GQL list literal.
///
/// Markdown headings are indexed as `Concept` nodes — 362 of 2280 in this
/// repo — so a statistic that forgets to exclude them is wrong on any
/// project with docs. `File` and `Folder` are containers, not symbols.
/// [`CODE_TYPES`] as a literal, for splicing into a preset's `gql` with
/// `concat!` — which only accepts literals, so the const alone cannot be
/// used there. `code_types_agree` keeps the two identical.
macro_rules! code_types {
    () => {
        "['Function', 'Class', 'Interface', 'Constant', 'Variable']"
    };
}

pub const CODE_TYPES: &str = code_types!();

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
    /// A list parameter takes a comma-separated string and binds it as a
    /// `QueryValue::List`, for `IN $name` membership predicates. Off for
    /// every existing scalar parameter.
    pub list: bool,
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
    /// What to run after this answer, as `(command, why)` pairs.
    ///
    /// A preset answers one question and immediately raises the next one,
    /// and the follow-up is often a query nobody would guess: listing one
    /// boundary kind needs `CONTAINS` against a comma-joined property,
    /// because `=` silently drops every symbol that carries two. Naming the
    /// command here puts it in the output an agent is already reading,
    /// rather than in documentation it is not.
    pub next: &'static [(&'static str, &'static str)],
}

pub fn find(name: &str) -> Option<&'static Preset> {
    BUILTIN.iter().find(|p| p.name == name)
}

pub fn all() -> &'static [Preset] {
    BUILTIN
}

const NO_PARAMS: &[PresetParam] = &[];

/// Filters for the boundary listing. Both default to "everything", so
/// `analyze boundaries` stays a one-word call until you want to narrow it.
///
/// These exist because without them the only way to ask "just the CLI ones"
/// was to list all 142 surfaces, page through them, and then hand-write the
/// GQL anyway — observed costing seven tool calls for a question that is one.
const BOUNDARY_FILTERS: &[PresetParam] = &[
    PresetParam {
        name: "kind",
        // Substring, not equality: `boundary_kinds` is a comma-joined list
        // because one symbol can be several things at once, and `=` silently
        // drops every symbol that carries two.
        description: "Only this boundary kind — e.g. 'cli.command', 'http.endpoint', 'http.client'. Matched as a substring, so a symbol carrying several kinds still matches. Run boundary_census to see which kinds this repo actually has. Default: every kind.",
        default: Some(ParamValue::Str("")),
        list: false,
    },
    PresetParam {
        name: "direction",
        description: "'inbound' for surfaces this system exposes (handlers, CLI commands, listeners) or 'outbound' for what it consumes (HTTP/DB clients). Default: both.",
        default: Some(ParamValue::Str("")),
        list: false,
    },
];

/// No follow-up worth naming: the answer is the end of the question.
const NO_NEXT: &[(&str, &str)] = &[];

const MIN_LOC: &[PresetParam] = &[PresetParam {
    name: "min_loc",
    description: "Line-span threshold (a span, so it counts blanks and comments).",
    default: Some(ParamValue::Int(50)),
    list: false,
}];

const TARGET: &[PresetParam] = &[PresetParam {
    name: "target",
    description: "Repo-relative file PATH whose dependents you want, e.g. 'src/auth.ts'. Not a node id — 'function:src/auth.ts:login' matches no file and returns zero rows.",
    default: None,
    list: false,
}];

/// A list of changed files, for the diff_* presets. Pass a comma-separated
/// string (the natural shape for `--arg files=a.ts,b.rs` and for a model's
/// `{"files": "a.ts,b.rs"}`); it is bound as a GQL list so `IN $files`
/// works. The blast-radius question is "across everything I just changed",
/// which is N files at once — not one target at a time.
const FILES: &[PresetParam] = &[PresetParam {
    name: "files",
    description: "Comma-separated repo-relative file PATHS that changed, e.g. 'src/auth.ts,src/db.ts'. Not node ids. Use the diff_* presets when a change spans several files at once.",
    default: None,
    list: true,
}];

/// A symbol node id (or exact name) for the `test_for` preset. Resolved
/// against `elementKey(n)` — pass an id from a prior `find_symbols` for the
/// unambiguous case.
const SYMBOL: &[PresetParam] = &[PresetParam {
    name: "symbol",
    description: "Node id of the symbol whose tests you want, e.g. 'function:src/auth.ts:login'. A bare name works but may match several symbols; resolve it with find_symbols first for an unambiguous answer.",
    default: None,
    list: false,
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
        next: NO_NEXT,
    },
    Preset {
        name: "biggest_files",
        category: Category::Census,
        // `CODE_TYPES`, not "everything that is not a File or Folder". A
        // markdown heading is indexed as a `Concept`, so the old filter
        // counted them as symbols and put `PERF-TUNING-JOURNEY.md` (90
        // headings) and two READMEs in the top of a list headed "where the
        // mass is". The constant right below this one documents that exact
        // trap; this preset was not using it.
        //
        // `code_lines` because a symbol count alone ranks a file of 163
        // one-line helpers above one holding five 300-line functions.
        description: "Files with the most indexed code symbols, and how many lines of code they hold — where the mass is.",
        params: NO_PARAMS,
        gql: concat!("MATCH (n) \
              WHERE n.node_type IN ", code_types!(), " AND n.file <> '' \
              RETURN n.file AS file, count(*) AS symbols, sum(n.code_lines) AS code_lines \
              ORDER BY symbols DESC, file ASC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "language_breakdown",
        category: Category::Census,
        // Same `CODE_TYPES` omission as `biggest_files`, and worse here
        // because both columns were wrong at once: markdown reported 645
        // "symbols" (they are headings) and 29,953 "code lines" (that is
        // prose — `line_metrics` has no comment syntax for markdown, so
        // every non-blank line counts as code). A language census that
        // lists markdown beside rust invites exactly the comparison the
        // numbers cannot support.
        //
        // Documentation is not lost, it is answered by the preset that can
        // answer it: `file_kinds` counts doc files and their lines.
        description: "What this repo is written in: code symbols and code lines per language. Docs are counted by file_kinds, not here.",
        params: NO_PARAMS,
        gql: concat!("MATCH (n) \
              WHERE n.language IS NOT NULL AND n.node_type IN ", code_types!(), " \
              RETURN n.language AS language, count(*) AS symbols, \
                     sum(n.code_lines) AS code_lines \
              ORDER BY symbols DESC, language ASC"),
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "file_kinds",
        category: Category::Census,
        description: "What kinds of file this repo holds: indexed files per extension, and the language each maps to.",
        params: NO_PARAMS,
        // Anchored on `:File` so `count(*)` is files, not the symbols
        // inside them — `language_breakdown` already counts those, and a
        // census that silently answered the other question would be
        // indistinguishable from this one.
        //
        // `language` joins the group key rather than being dropped: it is
        // what separates `.js` from `.ts` mapping to the same grammar, and
        // it is the column that explains a row a caller did not expect.
        // `files DESC, extension ASC` because a tie on the count must not
        // pick its own order (Agents.md §9c).
        gql: "MATCH (n:File) \
              RETURN n.extension AS extension, n.language AS language, \
                     count(*) AS files, sum(n.loc) AS lines \
              ORDER BY files DESC, extension ASC",
        headline: None,
        next: NO_NEXT,
    },
    // Two filters, both of which this preset shipped without and both of
    // which decided its whole first page.
    //
    // `is_test = 0`: with no such clause, five of the top twelve rows on
    // this repo were test scaffolding — `sample_graph`, `router_for`,
    // `fixture`, `targets`. A test fixture called by forty tests has a high
    // in-degree and a doc comment, which is exactly what this ranks on, and
    // "the reading order for a newcomer" that opens with a fixture is a
    // wrong answer in the shape of a right one.
    //
    // `loc >= 15`: the six rows underneath were 3-to-10-line delegating
    // helpers (`index`, `flag_value`, `project_dir`, `die`). They are
    // genuinely the most-called functions in the repo and there is nothing
    // to learn from reading them. `loc` is a *span*, so a three-line
    // function carrying a twelve-line doc comment still passes — which is
    // the right call: someone wrote twelve lines about it for a reason.
    Preset {
        name: "where_to_start",
        category: Category::Census,
        description: "Documented, heavily depended-upon symbols of some substance — the reading order for a newcomer. Excludes tests and one-line wrappers.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.has_doc = 1 AND n.is_test = 0 AND n.loc >= 15 \
                AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC, loc DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
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
        next: NO_NEXT,
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
        next: NO_NEXT,
    },
    Preset {
        name: "size_histogram",
        category: Category::Size,
        // `is_test = 0`, so this is the distribution whose tail
        // `long_functions` lists. Without it the histogram counted 1,871
        // test functions the tail-listing preset excludes, and the two
        // disagreed about the population they describe — which is worse
        // than either answer alone, because the reader assumes they match.
        description: "Distribution of non-test function length — the population long_functions takes the tail of.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WHERE n.is_test = 0 \
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
        next: NO_NEXT,
    },
    Preset {
        name: "god_classes",
        category: Category::Size,
        description: "The largest classes, structs, traits and interfaces by line span.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Class', 'Interface'] AND n.is_test = 0 \
              RETURN elementKey(n) AS id, n.loc AS loc, n.out_degree AS depends_on \
              ORDER BY loc DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
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
        next: NO_NEXT,
    },
    Preset {
        name: "classes_by_members",
        category: Category::Size,
        // This said "only populated for Java, Python, TS — check coverage",
        // which was wrong twice over. Rust `impl` blocks *do* attach their
        // methods to the type, so 107 of this repo's 311 types carry a
        // count; and the coverage line it told the reader to check was
        // itself reporting `members 2%` because its denominator was every
        // node in the graph rather than the types this query looks at. A
        // correct answer, with a description and a caveat both telling the
        // caller to throw it away.
        description: "Types with the most declared members — a class body's fields and methods, and a Rust type's impl blocks. A type nobody wrote members for is absent rather than zero.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Class', 'Interface'] AND n.members IS NOT NULL \
                AND n.is_test = 0 \
              RETURN elementKey(n) AS id, n.members AS members, n.loc AS loc \
              ORDER BY members DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "param_bloat",
        category: Category::Size,
        description: "Functions taking more than min_params arguments.",
        params: &[PresetParam {
            name: "min_params",
            description: "Parameter-count threshold.",
            default: Some(ParamValue::Int(5)),
            list: false,
        }],
        gql: "MATCH (n:Function) \
              WHERE n.params > $min_params AND n.is_test = 0 \
              RETURN elementKey(n) AS id, n.params AS params, n.loc AS loc \
              ORDER BY params DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "deep_nesting",
        category: Category::Size,
        description: "Functions nested at least min_depth levels deep — the hard-to-follow code.",
        params: &[PresetParam {
            name: "min_depth",
            description: "Nesting-depth threshold.",
            default: Some(ParamValue::Int(4)),
            list: false,
        }],
        gql: "MATCH (n:Function) \
              WHERE n.max_nesting >= $min_depth AND n.is_test = 0 \
              RETURN elementKey(n) AS id, n.max_nesting AS nesting, n.loc AS loc \
              ORDER BY nesting DESC, loc DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
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
                     sum(n.has_doc) AS with_doc_comment, \
                     sum(n.has_comments) * 100 / count(*) AS commented_pct \
              ORDER BY total DESC, kind ASC",
        headline: None,
        next: NO_NEXT,
    },
    // This preset is called `comment_density` and returned three raw sums
    // ordered by `code_lines DESC` — i.e. by how *big* each folder is. The
    // top row was the biggest folder, not the densest, and "where the prose
    // actually is" was answered with "wherever the code is".
    //
    // On this repo the two orders barely overlap: by size the list opens
    // with `vis/js` (14,176 code lines, 17% prose) and `cli` (17%); by
    // density it opens with `graph` (34%), `storage` (31%) and `indexer`
    // (31%), which sat 8th, 9th and 10th. A reader taking the first rows as
    // the answer got the opposite of the finding.
    //
    // `comment_lines` and `doc_lines` stay separate columns — the gap
    // between "has prose" and "has a doc comment" is the finding this
    // section exists for — and `prose_pct` sums them only to rank.
    Preset {
        name: "comment_density",
        category: Category::Documentation,
        description: "Comment-to-code line ratio per folder, densest first — where the prose actually is.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WHERE n.is_test = 0 \
              WITH n.folder AS folder, \
                   sum(n.code_lines) AS code_lines, \
                   sum(n.comment_lines) AS comment_lines, \
                   sum(n.doc_lines) AS doc_lines \
              WHERE code_lines > 50 \
              RETURN folder, code_lines, comment_lines, doc_lines, \
                     (comment_lines + doc_lines) * 100 / code_lines AS prose_pct \
              ORDER BY prose_pct DESC, code_lines DESC, folder ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
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
        next: NO_NEXT,
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
        next: NO_NEXT,
    },
    Preset {
        name: "doc_coverage",
        category: Category::Documentation,
        description: "How many symbols of each type carry a doc comment.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN n.node_type AS kind, count(*) AS total, sum(n.has_doc) AS documented, \
                     sum(n.has_doc) * 100 / count(*) AS documented_pct \
              ORDER BY total DESC, kind ASC",
        headline: None,
        next: NO_NEXT,
    },
    // "Least-documented first", ordered by `documented ASC` — a raw count,
    // so the ranking was decided by how *small* a folder is. A folder with
    // 5 symbols and 3 documented (60%) outranked one with 123 and 55 (45%)
    // as "worse documented", and on this repo it did: `native` (60%) came
    // third and `mcp` (45%) eighth.
    //
    // Sorting on the ratio is what the description always claimed, and
    // showing it is what lets a reader see that `vis/js` at 0/759 and a
    // 0/7 stub are not the same problem. `total DESC` breaks the ties,
    // which are dense once the key is a percentage — 0% covers four
    // folders here.
    Preset {
        name: "doc_coverage_by_folder",
        category: Category::Documentation,
        description: "Which folders are worst documented, by the fraction of symbols carrying a doc comment.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.node_type IN ['Function', 'Class', 'Interface'] AND n.is_test = 0 \
              WITH n.folder AS folder, count(*) AS total, sum(n.has_doc) AS documented \
              WHERE total >= 5 \
              RETURN folder, total, documented, documented * 100 / total AS documented_pct \
              ORDER BY documented_pct ASC, total DESC, folder ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
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
        next: NO_NEXT,
    },
    // ── dead code ─────────────────────────────────────────────────────
    //
    // `in_degree` counts resolved structural edges only. A symbol reached
    // by dynamic dispatch, reflection, or a string-keyed lookup has an
    // in-degree of zero and is not dead. These presets find *candidates*.
    //
    // On its own that made `dead_code` almost pure noise. Every candidate
    // it produced on this repository was checked by hand: 462 rows, 8 of
    // them dead. 1.7% of the list was worth reading, and those eight sat
    // underneath 454 trait methods reached through `dyn`, handlers named
    // in a `.route()` table, `serde` payloads that only ever arrive
    // deserialised, and JS functions called as `obj.method()`.
    //
    // `name_mentions = 0` is what makes it readable: it asks whether
    // anything in the repo still writes the name down, resolved or not,
    // which is the question in-degree was standing in for. Same eight
    // found, 111 rows instead of 462. Both conjuncts are needed —
    // `name_mentions` alone clears any symbol whose short name collides
    // with a live one, and `in_degree` alone is the 1.7%.
    //
    // `Constant` is in the node-type list because a dead `pub const` was
    // invisible to the old one; `Variable` is not, because it is JS
    // module state, which this indexer draws no `Uses` edges for — it
    // added 78 rows and not one true positive. See
    // `docs/dev/DEAD-CODE-AUDIT.md` for the audit and what still gets
    // through.
    Preset {
        name: "dead_code",
        category: Category::DeadCode,
        description: "Non-test symbols nothing resolves an edge to AND nothing mentions by name. Candidates, not proof — a name built at runtime is invisible here.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.in_degree = 0 AND n.name_mentions = 0 AND n.is_test = 0 \
                AND n.node_type IN ['Function', 'Class', 'Interface', 'Constant'] \
              RETURN elementKey(n) AS id, n.loc AS loc \
              ORDER BY loc DESC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
    },
    // `external_in_degree`, not `in_degree`. A File node's in-degree counts
    // only file-incident edges (`Imports`, `References`, `Exports`,
    // `DependsOn`), so a module whose functions are called from thirty other
    // files still reads as zero — Agents.md §11b. Asking it that way listed
    // **145 of this repo's 213 files**, including every test file, every
    // `.js` part and every doc, which is the same "1.7% of the list is worth
    // reading" failure the `dead_code` audit fixed with `name_mentions`.
    //
    // Three narrowings, each one cutting a category whose answer is
    // structurally fixed rather than interesting:
    //
    // - `external_in_degree = 0` — nothing outside reaches the file *or
    //   anything in it*. 145 → 31.
    // - `is_test = 0` — a test file is never imported by anything. That is
    //   what a test is, not a finding. 39 of the 145 were tests.
    // - not `documentation` — markdown and PDF are never imported either,
    //   and 19 of the remaining 31 rows were docs burying 7 real ones.
    //   Written as `IS NULL OR <>` because the classifier only has an
    //   opinion about 14% of files and a bare `<>` drops every NULL, i.e.
    //   every code file.
    //
    // Ordered, unlike the original. Five runs of the same binary over the
    // same store agreed, so the engine's scan order is deterministic — but
    // `LIMIT 200` with no `ORDER BY` still means a repo with more than 200
    // orphans drops rows by store order rather than by anything the caller
    // asked for, and a re-ingest is free to move that order (Agents.md §9c).
    Preset {
        name: "orphan_files",
        category: Category::DeadCode,
        description: "Code files nothing outside them reaches — no import, and no call into any symbol they hold. Excludes tests and docs, which are never imported by design.",
        params: NO_PARAMS,
        gql: "MATCH (n:File) \
              WHERE n.external_in_degree = 0 AND n.is_test = 0 \
                AND (n.classification IS NULL OR n.classification <> 'documentation') \
              RETURN elementKey(n) AS id, n.language AS language, n.loc AS lines \
              ORDER BY lines DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: &[
            ("analyze dead_code", "the symbol-level version of the same question"),
            ("find_usages <symbol>", "confirm one file's symbols really have no callers"),
        ],
    },
    // `is_test = 0` is what makes this preset say anything. Including
    // tests, the top of the list on this repo was `run` (14), `node` (14),
    // `args`, `argv`, `row`, `item` — per-test-module local helpers, in a
    // list whose stated purpose is to find duplication. Twenty rows, none
    // actionable. Excluding them leaves eight, and all eight are the same
    // function reimplemented once per language extractor: `collect_calls`,
    // `extract_params`, `record_type_refs`, `signature_type_refs`, `visit`.
    //
    // `folders` separates the two cases the description names: a name in
    // one folder is a family (five extractors implementing one interface);
    // a name spread across several is duplication. Without it every row
    // needs a `find_symbols` call to interpret.
    Preset {
        name: "duplicate_names",
        category: Category::DeadCode,
        description: "The same non-test function name defined in many places. `folders` tells the two cases apart: 1 is a per-variant family, several is duplication.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WHERE n.is_test = 0 \
              WITH n.name AS name, count(*) AS definitions, \
                   count(DISTINCT n.folder) AS folders \
              WHERE definitions > 3 \
              RETURN name, definitions, folders \
              ORDER BY definitions DESC, name ASC \
              LIMIT 200",
        headline: None,
        next: &[("find_symbols <name>", "where the definitions actually are")],
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
        next: NO_NEXT,
    },
    Preset {
        name: "fanout_offenders",
        category: Category::Architecture,
        description: "Symbols that reach out to more than min_fanout others — the code that touches everything.",
        params: &[PresetParam {
            name: "min_fanout",
            description: "Outbound-edge threshold.",
            default: Some(ParamValue::Int(20)),
            list: false,
        }],
        gql: "MATCH (n) \
              WHERE n.out_degree > $min_fanout AND n.is_test = 0 \
                AND n.node_type <> 'File' AND n.node_type <> 'Folder' \
              RETURN elementKey(n) AS id, n.out_degree AS depends_on, n.loc AS loc \
              ORDER BY depends_on DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "coupling_matrix",
        category: Category::Architecture,
        description: "Which folders depend on which, by edge count across the folder boundary.",
        params: NO_PARAMS,
        // Same edge set as every other dependency question — see
        // `IMPACT_EDGES`. This preset and `layering_violations` each
        // carried their own spelling, which dropped `Overrides` on top of
        // the `Instantiates`/`Uses` gap: three lists, one meaning, and only
        // the constant knew it was the answer (Agents.md §9c).
        gql: concat!("MATCH (a)-[:", impact_edges_gql!(), "]->(b) \
              WHERE a.folder <> b.folder \
              RETURN a.folder AS from_folder, b.folder AS to_folder, count(*) AS edges \
              ORDER BY edges DESC, from_folder ASC, to_folder ASC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
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
                list: false,
            },
            PresetParam {
                name: "to_prefix",
                description: "Folder prefix that layer should not reach directly, e.g. 'src/db'.",
                default: None,
                list: false,
            },
        ],
        gql: concat!("MATCH (a)-[:", impact_edges_gql!(), "]->(b) \
              WHERE a.folder STARTS WITH $from_prefix AND b.folder STARTS WITH $to_prefix \
              RETURN a.file AS from_file, b.file AS to_file, count(*) AS edges \
              ORDER BY edges DESC, from_file ASC, to_file ASC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
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
        next: &[
            ("analyze boundaries --arg kind=<kind from the rows above>", "every symbol of one kind, with its id and surface"),
            ("analyze boundaries", "the same surfaces listed one per row, with their ids"),
        ],
    },
    Preset {
        name: "boundaries",
        category: Category::Architecture,
        description: "Every system entry and exit point — REST handlers, queue listeners, CLI commands, outbound clients. Filter with kind and/or direction.",
        params: BOUNDARY_FILTERS,
        // An unset filter is the empty string, and both predicates are
        // written so that the empty string matches everything: `CONTAINS ''`
        // is true of every value, and the direction clause short-circuits on
        // its first branch. That keeps one query for the filtered and
        // unfiltered questions instead of four near-identical presets.
        gql: "MATCH (n) \
              WHERE n.boundary = 1 \
                AND n.boundary_kinds CONTAINS $kind \
                AND ($direction = '' \
                     OR ($direction = 'inbound' AND n.boundary_in = 1) \
                     OR ($direction = 'outbound' AND n.boundary_out = 1)) \
              RETURN elementKey(n) AS id, \
                     n.boundary_kinds AS kinds, \
                     n.boundary_detail AS surface, \
                     n.file AS file \
              ORDER BY kinds, file \
              LIMIT 200",
        headline: None,
        next: &[
            ("analyze boundaries --arg kind=http.client", "one kind only — no GQL needed"),
            ("analyze boundaries --arg direction=inbound", "only what this system exposes"),
            ("analyze boundary_impact --arg target=<file-or-symbol>", "what a change reaches through these"),
        ],
    },
    // ── tests ─────────────────────────────────────────────────────────
    // Two problems, and the column name was the smaller one. `functions`
    // counted *all* functions including the tests, so `native/tests` read
    // "459 functions, 459 tests" and every other row needed a subtraction
    // before it meant anything.
    //
    // The ordering was the real defect: `functions DESC` ranks by folder
    // size, so a preset called `test_ratio` put the biggest folder first
    // and buried the untested ones. `native/src/graph` (46 source
    // functions, 0 co-located tests) sat twelfth. Sorting on the ratio is
    // what the name always promised (Agents.md §11j).
    //
    // The denominator is all functions, not `source`, because a folder that
    // is entirely tests divides by zero — and OverGraph rejects the query
    // rather than returning a number, which is how this was found.
    Preset {
        name: "test_ratio",
        category: Category::Tests,
        description: "Least-tested folders first, by the share of functions that are tests. Counts where tests LIVE, so a repo with a top-level tests/ dir shows zeros next to its source folders — use untested_symbols for reachability.",
        params: NO_PARAMS,
        gql: "MATCH (n:Function) \
              WITH n.folder AS folder, count(*) AS all_functions, sum(n.is_test) AS tests \
              WHERE all_functions >= 5 \
              RETURN folder, all_functions - tests AS source, tests, \
                     tests * 100 / all_functions AS test_pct \
              ORDER BY test_pct ASC, source DESC, folder ASC \
              LIMIT 200",
        headline: None,
        next: &[("analyze untested_symbols", "which symbols no test reaches, wherever the tests live")],
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
        next: NO_NEXT,
    },
    Preset {
        name: "retest_scope",
        category: Category::Tests,
        description: "Which test files exercise code reachable from a target file — what to re-run after changing it.",
        params: TARGET,
        gql: concat!("MATCH (dep)-[:", impact_edges_gql!(), "*1..3]->(t) \
              WHERE t.file = $target AND dep.is_test = 1 \
              RETURN dep.file AS test_file, count(DISTINCT elementKey(dep)) AS test_symbols \
              ORDER BY test_symbols DESC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "test_for",
        category: Category::Tests,
        description: "Which test symbols reach a given code symbol — the missing code→test half of the test loop. Pass a node id (resolve a bare name with find_symbols first).",
        params: SYMBOL,
        // Anchored on `n` (the symbol under test) and walked inbound from
        // test symbols. 2 hops: a test usually calls the symbol directly or
        // through one helper, and an unanchored 3-hop walk from every test
        // node is the cap-blowing shape `untested_symbols` already documents.
        gql: concat!("MATCH (t)-[:", impact_edges_gql!(), "*1..2]->(n) \
              WHERE elementKey(n) = $symbol AND t.is_test = 1 \
              RETURN elementKey(t) AS test, t.file AS file, count(*) AS paths \
              ORDER BY paths DESC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "diff_retest_scope",
        category: Category::Tests,
        description: "Which test files to re-run after a change spanning several files — pass the changed paths at once. The multi-file form of retest_scope.",
        params: FILES,
        // Same reachability as retest_scope, but anchored on a *set* of
        // changed files via `IN $files`. Dependents inside the changed set
        // are excluded — a changed file reaching another changed file is the
        // change itself, not something a test run would catch.
        gql: concat!("MATCH (dep)-[:", impact_edges_gql!(), "*1..3]->(t) \
              WHERE t.file IN $files AND dep.is_test = 1 AND NOT (dep.file IN $files) \
              RETURN dep.file AS test_file, count(DISTINCT elementKey(dep)) AS test_symbols \
              ORDER BY test_symbols DESC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
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
        gql: concat!("MATCH (dep)-[:", impact_edges_gql!(), "*1..3]->(t) \
              WHERE t.file = $target AND dep.file <> $target \
              RETURN dep.file AS file, \
                     count(DISTINCT elementKey(dep)) AS dependents, \
                     count(DISTINCT CASE WHEN dep.is_test = 1 THEN elementKey(dep) END) AS tests \
              ORDER BY dependents DESC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "diff_impact",
        category: Category::Risk,
        description: "Blast radius of a change spanning several files — pass the changed paths at once. The multi-file form of impact; feed it `git diff --name-only`.",
        params: FILES,
        // Same DISTINCT trap as `impact`: a variable-length match yields one
        // row per path, so `count(DISTINCT elementKey(dep))` is the only
        // honest dependent count. `dep.file NOT IN $files` keeps a changed
        // file from counting as its own blast radius.
        gql: concat!("MATCH (dep)-[:", impact_edges_gql!(), "*1..3]->(t) \
              WHERE t.file IN $files AND NOT (dep.file IN $files) \
              RETURN dep.file AS file, \
                     count(DISTINCT elementKey(dep)) AS dependents, \
                     count(DISTINCT CASE WHEN dep.is_test = 1 THEN elementKey(dep) END) AS tests \
              ORDER BY dependents DESC \
              LIMIT 200"),
        headline: None,
        next: NO_NEXT,
    },
    Preset {
        name: "impact_summary",
        category: Category::Risk,
        description: "One-line blast radius for a file: how many symbols and files reach it.",
        params: TARGET,
        gql: concat!("MATCH (dep)-[:", impact_edges_gql!(), "*1..3]->(t) \
              WHERE t.file = $target AND dep.file <> $target \
              RETURN count(DISTINCT elementKey(dep)) AS dependents, \
                     count(DISTINCT dep.file) AS files_affected"),
        headline: Some("dependents"),
        next: NO_NEXT,
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
        // One row per boundary, not per path: a variable-length match yields
        // a row for every route through the graph, so the grouping implied
        // by `count(*)` is what collapses them back to the thing being
        // counted. Same trap the `impact` preset documents.
        //
        // `*1..3`, not the `*1..4` you might want for "a controller one
        // layer further out": the 4-hop version blows OverGraph's
        // `max_frontier` cap (65_536) on any centrally-depended file. The
        // single-MATCH `WHERE t.file = $target` does not force the planner
        // to anchor the variable-length piece on `t`, so it expands from
        // the un-anchored end and the frontier at hop 4 overflows. Three
        // hops is the most every other variable-length preset asks for and
        // is the ceiling that actually completes. The 4-hop wishful
        // version passed its test only because the seeded graph is 2 hops
        // deep — see `boundary_impact_reports_the_surface_a_change_is_visible_through`.
        gql: concat!("MATCH (b)-[:", impact_edges_gql!(), "*1..3]->(t) \
              WHERE t.file = $target AND b.boundary_in = 1 AND b.file <> $target \
              RETURN elementKey(b) AS surface, \
                     b.boundary_kinds AS kinds, \
                     b.boundary_detail AS exposed_as, \
                     count(*) AS paths \
              ORDER BY paths DESC \
              LIMIT 200"),
        headline: None,
        next: &[
            ("find_usages <symbol>", "the symbol-level callers, where this answer is file-level"),
            ("get_code <id>", "the source of a surface listed above"),
        ],
    },
    Preset {
        name: "risky_symbols",
        category: Category::Risk,
        description: "Large, undocumented, heavily depended-upon symbols — dangerous to touch.",
        params: NO_PARAMS,
        gql: "MATCH (n) \
              WHERE n.in_degree > 5 AND n.loc > 80 AND n.has_doc = 0 AND n.is_test = 0 \
                AND n.node_type IN ['Function', 'Class', 'Interface'] \
              RETURN elementKey(n) AS id, n.in_degree AS depended_on_by, n.loc AS loc \
              ORDER BY depended_on_by DESC, loc DESC, id ASC \
              LIMIT 200",
        headline: None,
        next: NO_NEXT,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The const and the macro form of the code-type list must agree —
    /// the same trap `impact_edges_agree` guards, one file over.
    #[test]
    fn code_types_agree() {
        assert_eq!(CODE_TYPES, code_types!());
    }

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
            // Scan the inside of relationship patterns rather than every
            // `*` in the query. The previous version skipped only a `*`
            // preceded by `(` — enough for `count(*)`, and it then read the
            // multiplication in `documented * 100 / total` as an unbounded
            // path. Anchoring on `[...]` is what separates the two: a path
            // bound can only appear inside a relationship pattern, and a
            // genuinely unbounded `[:Calls*]` still fails here, which the
            // "skip unless a digit follows" shortcut would not.
            for (open, _) in p.gql.match_indices('[') {
                let Some(close) = p.gql[open..].find(']') else {
                    continue;
                };
                let inside = &p.gql[open + 1..open + close];
                // Node-type list literals live in square brackets too.
                let Some(star) = inside.find('*') else {
                    continue;
                };
                let bound: String = inside[star + 1..]
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
