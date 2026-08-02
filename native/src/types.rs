use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(rename = "startLine")]
    pub start_line: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    pub docstring: Option<String>,
    #[serde(default)]
    pub signature: Option<Signature>,
    #[serde(default)]
    pub imports: Vec<ImportInfo>,
    #[serde(default)]
    pub exports: Vec<ExportInfo>,
    #[serde(default)]
    pub extends: Vec<String>,
    #[serde(default)]
    pub implements: Vec<String>,
    #[serde(default)]
    pub calls: Vec<String>,

    #[serde(default)]
    pub metrics: Option<SymbolMetrics>,

    /// Language-global unique name, when the language has one. Java fills
    /// this with `pkg.Type` for types and `pkg.Type#member` for members;
    /// languages without a module-path concept leave it `None`.
    ///
    /// `name` stays the *display* name (`OrderService.cancel`) — this is the
    /// key the graph builder resolves imports and calls against, so it has
    /// to be exact rather than readable.
    #[serde(rename = "qualifiedName", default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,

    /// [`Self::qualified_name`] of the type that declares this symbol, for
    /// members. Drives the `Contains` edge from a class to its methods and
    /// fields, and the supertype walk used to resolve inherited calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    /// Declaration-site annotations / decorators, in source order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Annotation>,

    /// Call sites with whatever receiver type the indexer could resolve
    /// locally. Parallel to (not a replacement for) [`Self::calls`], which
    /// stays a deduped list of bare callee names for display.
    #[serde(rename = "callRefs", default, skip_serializing_if = "Vec::is_empty")]
    pub call_refs: Vec<CallRef>,

    /// Qualified names of the module-level constants this symbol reads.
    ///
    /// Constants are where a codebase keeps its thresholds, limits and magic
    /// strings, and "what breaks if I change this cap" is a question the
    /// graph could not answer at all: a constant node had inbound `Contains`
    /// from its file and nothing else, so every constant looked dead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,

    /// Effective HTTP route this symbol serves, e.g. `GET /api/orders/{id}`,
    /// composed from type-level and member-level mapping annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// One declaration-site annotation. `name` is the simple name with any
/// package qualifier stripped (`GetMapping`, not
/// `org.springframework.web.bind.annotation.GetMapping`), because that is
/// how it is written at every call site and how people search for it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Annotation {
    pub name: String,
    /// Raw argument text with the enclosing parentheses stripped, capped in
    /// the extractor. `None` for a marker annotation like `@Override`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
}

/// A single call site, resolved as far as one file's worth of context
/// allows. The graph builder finishes the job against the cross-file
/// qualified-name index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallRef {
    /// Callee simple name. Constructors use `<init>`, matching the
    /// `qualified_name` the indexer assigns them.
    pub name: String,
    /// Qualified name of the type the call is dispatched on, when the
    /// receiver's declared type resolved. `None` for calls through an
    /// expression we can't type (a chained call, a lambda parameter).
    #[serde(rename = "ownerType", default, skip_serializing_if = "Option::is_none")]
    pub owner_type: Option<String>,
    /// Argument count, used to pick between overloads.
    #[serde(default)]
    pub argc: u32,

    /// Fully-qualified callee, when the indexer resolved the whole path
    /// outright rather than only its receiver.
    ///
    /// This is the case a *module*-structured language has and Java does not:
    /// `crate::project::read_meta(..)` names its target completely at the
    /// call site, with no dispatch to reason about. The graph builder tries
    /// this before [`Self::owner_type`] because an exact path needs no
    /// supertype walk and cannot be ambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified: Option<String>,

    /// Whether this call site constructs a value of [`Self::owner_type`]
    /// rather than invoking a member on one. Drives `Instantiates` instead
    /// of `Calls`.
    #[serde(rename = "isCtor", default, skip_serializing_if = "std::ops::Not::not")]
    pub is_ctor: bool,

    /// Whether the call site qualified its callee — with a receiver
    /// (`x.foo()`, `this.foo()`) or an explicit type (`Foo::bar()`) — as
    /// opposed to naming a free function directly (`foo()`, `a::b::foo()`).
    ///
    /// This gates the graph builder's bare-name fallback. When a receiver is
    /// present but [`Self::owner_type`] is `None`, the receiver is an
    /// expression we could not type — and matching the bare member name
    /// against every symbol in the repo is exactly the guess that turned
    /// every `.collect()` in a Rust codebase into an edge pointing at
    /// whatever local function happened to be called `collect`. A callee
    /// named without a receiver has no such problem: it is a free function
    /// in the file's own scope, and its bare name is what identifies it.
    #[serde(
        rename = "hasReceiver",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub has_receiver: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub params: Vec<Param>,
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub param_type: Option<String>,
    pub optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportInfo {
    pub path: String,
    pub imported: Vec<ImportedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedItem {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportInfo {
    pub name: String,
    pub alias: Option<String>,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub name: String,
    pub generic: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolMetrics {
    /// Lines the symbol spans, **inclusive** of both its first and last
    /// line — the same convention the line-span fallback uses for symbols
    /// with no metrics, so a Function and a Class are comparable.
    ///
    /// This is a span, so it counts blank and comment lines. Use
    /// [`Self::code_lines`] when you mean "lines of code".
    pub loc: u32,
    pub params: u32,
    #[serde(rename = "maxNesting")]
    pub max_nesting: u32,

    /// Comment lines inside the symbol's span. A line carrying both code
    /// and a trailing comment counts as code, not comment.
    ///
    /// Zero on a graph written before this field existed — which is
    /// indistinguishable from "genuinely uncommented" without the schema
    /// version in [`IndexStats`]. See `graph_schema_version`.
    #[serde(rename = "commentLines", default)]
    pub comment_lines: u32,

    /// Lines in the symbol's leading doc comment.
    ///
    /// Derived from the extracted docstring rather than from the lines
    /// above the symbol, because a Python docstring lives *inside* the
    /// function body — a position-based rule would be right for four
    /// languages and silently wrong for the fifth.
    #[serde(rename = "docLines", default)]
    pub doc_lines: u32,

    /// Span lines that are neither blank nor pure comment.
    ///
    /// The honest denominator for size questions: `loc` overstates by
    /// roughly 30% on commented code, so "functions longer than 50 lines"
    /// measured on `loc` is a different question than most people mean.
    #[serde(rename = "codeLines", default)]
    pub code_lines: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub hash: String,
    pub language: String,
    pub classification: Option<FileClassification>,
    pub symbols: Vec<Symbol>,
    pub lines: u32,
    #[serde(default)]
    pub imports: Vec<ImportInfo>,
    #[serde(default)]
    pub exports: Vec<ExportInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileClassification {
    Component,
    Page,
    Hook,
    Util,
    Service,
    Config,
    Type,
    Constant,
    Context,
    Reducer,
    Test,
    Asset,
    Documentation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderNode {
    pub path: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<FolderClassification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,
    #[serde(rename = "childFiles", default)]
    pub child_files: Vec<String>,
    #[serde(rename = "childFolders", default)]
    pub child_folders: Vec<String>,
    #[serde(rename = "totalFiles")]
    pub total_files: u32,
    #[serde(rename = "languageBreakdown", default)]
    pub language_breakdown: HashMap<String, u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderClassification {
    Source,
    Tests,
    Documentation,
    Examples,
    Config,
    Assets,
    Components,
    Pages,
    Hooks,
    Services,
    Contexts,
    Reducers,
    Utils,
    Types,
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    pub files: Vec<FileNode>,
    #[serde(default)]
    pub folders: Vec<FolderNode>,
    pub dependencies: Vec<Dependency>,
    pub stats: IndexStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub version: Option<String>,
    pub dev: bool,
    pub optional: bool,
}

/// What the current build writes into [`IndexStats::graph_schema_version`].
///
/// Bump whenever a *fact* is added that older graphs cannot have. This is
/// not the same as [`crate::indexer::INDEXER_VERSION`], which invalidates
/// the content cache: this one tells a *reader* of an already-written
/// `graph.json` which facts it is entitled to trust.
///
/// The distinction matters because of the failure this whole feature is
/// built around. A graph written before comment metrics existed answers
/// "how many functions have comments" with `0` — not an error, not an
/// empty result, just a wrong number that looks right. Version 2 is the
/// first to carry `comment_lines` / `doc_lines` / `code_lines`, `language`
/// and `classification`, so a reader seeing version < 2 can say "not
/// indexed — run `ug regen`" instead.
///
/// - **1** (or absent): pre-comment-metrics.
/// - **2**: comment/doc/code line counts, class metrics, file language and
///   classification on every node.
/// - **3**: cross-file call resolution for Rust, TypeScript and Python.
///   Version 2 and earlier resolved a callee by bare name and, failing that,
///   by the substring after its last dot — so a `Calls` edge could point at
///   any symbol sharing the callee's name, and a `::` path produced no edge
///   at all. A reader seeing version < 3 should treat call-graph answers
///   (`find_usages`, `impact`, `dead_code`) as indicative rather than
///   accurate, and say so, rather than reporting a confident wrong number.
pub const GRAPH_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    /// Which generation of facts this graph carries — see
    /// [`GRAPH_SCHEMA_VERSION`]. Absent (deserializing to 0) on any graph
    /// written before the field existed, which is by definition a v1
    /// graph; readers should treat 0 and 1 identically.
    #[serde(rename = "graphSchemaVersion", default)]
    pub graph_schema_version: u32,
    #[serde(rename = "totalFiles")]
    pub total_files: usize,
    #[serde(rename = "cachedFiles")]
    pub cached_files: usize,
    #[serde(rename = "totalSymbols")]
    pub total_symbols: usize,
    #[serde(rename = "totalFolders", default)]
    pub total_folders: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: u64,
    #[serde(rename = "indexingTimeMs")]
    pub indexing_time_ms: u64,
    #[serde(rename = "lastIndexedAt")]
    pub last_indexed_at: u64,
    #[serde(rename = "repoRoot")]
    pub repo_root: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum GraphNodeType {
    #[default]
    File,
    Folder,
    Function,
    Class,
    Interface,
    Concept,
    Dependency,
    Config,
    Constant,
    /// A named data binding that is not callable — a Java field, a
    /// module-level `let x = 1`. Distinct from `Function` because "how many
    /// functions does this class have" and "what does this call" are both
    /// wrong answers when a field is counted as code you can invoke, and
    /// distinct from `Constant` because a mutable field is not a constant.
    Variable,
    /// A network entry point — an HTTP endpoint declared by mapping
    /// annotations. Distinct from the handler `Function` it points at: the
    /// route is what callers of the *system* know, and it is what people
    /// search for ("the endpoint that cancels an order").
    Route,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum GraphEdgeType {
    DependsOn,
    Calls,
    Extends,
    Implements,
    References,
    Contains,
     Imports,
     Exports,
     Requires,
     Uses,
    /// Method → the supertype method it overrides. Separate from `Extends`
    /// (which is type-level) so "who overrides this" and "what does this
    /// class inherit from" stay different questions.
    Overrides,
    /// Caller → a type it constructs (`new Foo()`, `Foo { .. }`).
    ///
    /// Separate from `Calls` because constructing a value and invoking a
    /// method are different relationships, and conflating them made
    /// "who calls this class" a question with no meaningful answer. It is
    /// also the honest edge for a language where construction reaches no
    /// function at all — a Rust struct literal runs no code.
    Instantiates,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub node_type: GraphNodeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(rename = "startLine", skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(rename = "endLine", skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SymbolMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<GraphNodeSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<GraphNodeImport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<GraphNodeExport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<GraphNodeFolderMeta>,

    /// Mirrors [`Symbol::qualified_name`]. Kept on the node so cross-file
    /// resolution and the MCP tools can address a symbol unambiguously
    /// without re-deriving it from the id.
    #[serde(rename = "qualifiedName", default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,

    /// Mirrors [`Symbol::annotations`]. The storage layer turns these into
    /// retrieval text — for annotation-driven frameworks they carry more of
    /// a symbol's meaning than its body does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Annotation>,

    /// Mirrors [`Symbol::route`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,

    /// Language of the file this node comes from, as its indexer names it
    /// (`rust`, `typescript`, `java`, `python`, `markdown`).
    ///
    /// Computed by the indexer and, until now, dropped before the graph was
    /// written — so "what is this repo made of, by language" had no answer
    /// even though the fact existed one stage upstream. Stamped onto every
    /// node in a file, not just the File node, so a statistic can group by
    /// it without a join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// What kind of file this node lives in — see [`FileClassification`].
    ///
    /// The authoritative answer to "is this test code", which `is_test`
    /// previously had to guess from the path. Also what makes it possible
    /// to exclude config and asset files from code statistics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<FileClassification>,
}

/// Folder-specific metadata projected onto the generic GraphNode. Lifted from
/// `FolderNode` so the graph is self-contained for downstream consumers (the
/// visualizer doesn't need to cross-reference IndexResult.folders, and the
/// RAG layer can store / query folder context without a second table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeFolderMeta {
    pub depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<FolderClassification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,
    #[serde(rename = "totalFiles")]
    pub total_files: u32,
    #[serde(
        rename = "languageBreakdown",
        default,
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub language_breakdown: HashMap<String, u32>,
    /// Filled by the Semantic Enrichment phase. When present, the storage
    /// layer prefers this over the synthesized description for folder
    /// embeddings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeSignature {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    #[serde(rename = "returnType", skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeImport {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imported: Vec<GraphImportedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphImportedItem {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNodeExport {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphTypeRef {
    pub name: String,
    pub generic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub edge_type: GraphEdgeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<IndexStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionStats>,
}

/// How every call site in the repo was resolved, or why it wasn't.
///
/// Call-graph quality was previously unmeasurable: a wrong edge and a right
/// edge look identical in a total, and the failure this whole area is prone
/// to is the confident wrong answer. These counts make a regression visible
/// between two runs — a jump in `dropped_unresolved` means resolution got
/// worse, and a jump in `resolved_by_name` means it got less certain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolutionStats {
    /// Callee named outright by a module path — the strongest evidence.
    #[serde(rename = "resolvedQualified")]
    pub resolved_qualified: u32,
    /// Resolved through a receiver whose type this file could infer.
    #[serde(rename = "resolvedTyped")]
    pub resolved_typed: u32,
    /// Fell through to bare-name matching, which requires the name to be
    /// unique repo-wide or declared in the caller's own file.
    #[serde(rename = "resolvedByName")]
    pub resolved_by_name: u32,
    /// No edge drawn. Mostly correct — a call into the standard library or a
    /// third-party package *should* land here — but it is also where an
    /// ambiguous name ends up.
    #[serde(rename = "droppedUnresolved")]
    pub dropped_unresolved: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BfsResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub distances: std::collections::HashMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathResult {
    pub path: Vec<String>,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CentralityResult {
    pub degree_centrality: std::collections::HashMap<String, f64>,
    pub betweenness_centrality: std::collections::HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    pub has_cycles: bool,
    pub cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredEdgesResult {
    pub edges: Vec<GraphEdge>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub nodes: Vec<GraphNode>,
    pub count: usize,
}