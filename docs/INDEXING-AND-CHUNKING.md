# Indexing & Chunking: how each file type enters the knowledge base

> **Scope.** What UltraGraph does between "a file on disk" and "a row in the
> store with a dense vector, a sparse vector, and a retrievable snippet."
> For where those rows physically live, see `GRAPH-STORAGE.md`; for which
> model produces the vectors, see `EMBEDDING-BACKENDS.md`.

## 1. The shape of a row

Every node in the store carries four pieces of text, and they are **not** the
same thing. Most confusion about "why did search return that" comes from
conflating them.

| Field | Built by | Feeds | Rule of thumb |
|---|---|---|---|
| `node_text` | `storage::text::build_node_text_with_comments` | the **dense vector** | What the node *means*, in prose. Never raw code. |
| `code` | `storage::source::capture_graph_code` | the **sparse vector** (weight 0.35) and every snippet read | The node's exact source span, verbatim. |
| `description` | the indexer's `Symbol.docstring` | UI, MCP result headers | The human-written (or extracted) description. |
| `file_hash` | blake3 of the whole file at index time | staleness checks | Lets `get_code` say "this file changed since indexing". |

`node_text` follows a fixed template (`storage/text.rs:180`):

```
{Type}: {name} ({name split into words}). {description}. Notes: {comments}. Related: {up to 24 neighbour names}
```

`{description}` is resolved by `node_description()` with this fall-through:

1. `folder.summary` (folder nodes, once semantic enrichment has run)
2. the node's `docstring`
3. a synthesized folder synopsis (`"documentation folder, 12 markdown files, depth 2"`)
4. a synthesized code synopsis (`"defined in src/net/pool.ts; extends BasePool"`)

**Everything below is really about one question: what does each file type put
into step 2?**

## 2. Pipelines by file type

Three pipelines produce nodes, plus one derivation that produces no file
content at all.

| Extensions | Pipeline | Chunk unit | Node type | Goes into the dense vector | Stored as `code` |
|---|---|---|---|---|---|
| `.ts .tsx .js .jsx` | tree-sitter (`languages/typescript.rs`) | one symbol | Function (incl. `const f = () =>`) / Class / Interface (incl. type alias) | name + JSDoc + signature + inline comments | the symbol's line span |
| `.py` | tree-sitter (`languages/python.rs`) | one symbol | Function (incl. module-level assignments) / Class | name + docstring + signature + inline comments | the symbol's line span |
| `.java` | tree-sitter (`languages/java.rs`) | one symbol | Function / Class / Interface / Route | name (qualified `Type.member`) + Javadoc + signature + **framework semantics** + inline comments | the symbol's line span |
| `.rs` | tree-sitter (`languages/rust.rs`) | one symbol | Function (incl. macros) / Class (struct, enum) / Interface (trait, type alias) / Constant | name + `///` docs + signature + inline comments | the symbol's line span |
| `.md .mdx .markdown` | line scanner (`languages/markdown.rs`) | **one heading section** | Concept | name + **the section's prose** | the section span, *including* subsections |
| `.pdf` | liteparse + PDFium (`indexer/document.rs`) | **one page** | Concept | name + **the page's full text** | *(nothing — binary)* |
| Word / Excel / PowerPoint | LibreOffice → PDF → liteparse | **one page** | Concept | name + **the page's full text** | *(nothing — binary)* |
| any indexed file | `graph.rs` | the whole file | File, or Config when classified as such | path (split into words) | the whole file |
| — (derived) | `indexer/folder.rs` | one directory | Folder | path + classification + language breakdown | *(nothing — not a file)* |

Two things this table is deliberately explicit about:

- **Code symbols do not embed their bodies. Documents do.** Section 4 explains
  why that asymmetry is correct rather than an inconsistency.
- **Only the extensions listed are indexed at all.** `common::SUPPORTED_EXTS`
  is the whole list. JSON, YAML, TOML, `.txt`, `.rst` and `.adoc` produce **no
  nodes today** — `package.json` is read for dependency metadata only, and
  that metadata does not become graph nodes.

### Skipped before any of this

`node_modules/`, `.git/`, anything matched by the repo's `.gitignore`, the
built-in artifact globs (`*.min.js`, `*.bundle.js`, `dist/`), and any glob in
the `UG_IGNORE` env var.

Build output (`target/`, `build/`, `out/`) is skipped only when the build
descriptor that produces it — `pom.xml`, `Cargo.toml`, `build.gradle` — sits
in the same directory. Skipping those names unconditionally is wrong for
Java, where `target` is a legal package name: `src/main/java/com/acme/target/`
is source, and a directory beside a `pom.xml` is not.

## 3. Chunking, per pipeline

### 3.1 Code (tree-sitter)

The AST is the chunker: one node per declared symbol, at whatever granularity
the language module extracts. Chunks do not overlap in any meaningful way —
extractors give class nodes a narrow declaration range, so a class and its
methods barely double-count.

The symbol's whole span is captured into `code`, with no cap. The dense text
gets the name, the docstring, a rendered signature, and prose comments lifted
from the body.

**Comment extraction** (`storage/comments.rs`) is the one part that reads the
body for embedding purposes, and it filters hard: commented-out code (scored
on symbol density), repeated banners and licence headers (deduped graph-wide),
and machine directives (`eslint-disable`, shebangs, `#[allow(...)]`). Capped
at 600 chars per node.

### 3.1a Cross-file call resolution

A `Calls` edge is a claim that *this* function calls *that* one. Making the
claim true takes more than a callee's name, and until schema version 3 only
the Java pipeline tried. Rust, TypeScript and Python recorded the raw source
text of each callee expression and resolved it by matching that text — or, on
a miss, the substring after its last dot — against every symbol in the repo,
taking the first registered candidate.

Three things followed, all of them measured on this repo's own graph:

- **Fabricated edges.** `.collect()` matched a local function named `collect`
  and produced 246 edges into it; `.get()` produced 138 and `.find()` 99.
  Roughly 18% of the call graph pointed somewhere its call site did not.
- **Missing edges.** The callee text was split on `.` and never on `::`, so
  all 1,619 Rust module-path calls resolved to nothing — including 247 that
  named a function in the repo (`crate::project::read_meta`,
  `agent_tools::find_usages`).
- **Blobs.** The "callee text" of a chained call included the whole
  expression, closure bodies and all: 2,030 entries spanning multiple lines,
  the longest 2,948 characters.

Resolution now runs in three tiers, strongest first, and **stops rather than
guesses**.

**1. The qualified path.** Every symbol gets a repo-unique name derived from
its file's module path — `crate::storage::db::Db#open`, `src/svc/order.OrderService#cancel`,
`pkg.svc.OrderService#cancel`. A call that names its target completely
(`crate::project::read_meta(..)`, or `agent_tools::find_usages(..)` via a
`use` binding) is one exact lookup with nothing to disambiguate.

**2. The receiver's type.** Each indexer keeps a per-function environment of
declared types — fields, parameters, locals, `self`/`this` — and tags each
call site with the type it dispatches on. `orderRepo.save(x)` and
`auditLog.save(x)` become different edges. Where the receiver is an interface
or trait, the edge is drawn to the implementations too (capped at 8), which
in a dependency-injected codebase is the only path from a caller to running
code. TypeScript's constructor-parameter-property shorthand
(`constructor(private store: Store)`) and Python's annotated `self.store: Store`
both count as declarations.

**3. The bare name — but only sometimes.** A callee written *without* a
receiver is a free function in the file's own scope, so matching its name is
sound. A callee written *with* one (`x.save()`, `String::new()`) has already
had its honest chance: if the receiver could not be typed, the name is not
tried, because that is precisely how `.collect()` became an edge.

At every tier an ambiguous name resolves to **nothing**. A name declared in
one place resolves; a name the caller's own file declares resolves; a name
three modules declare does not. `find_usages` returning an empty result has
to mean "no known caller" rather than "possibly the wrong one", or nothing
built on it can be trusted.

`ug graph` prints the tally, so the quality is measurable between runs rather
than by eyeball:

```
calls: 4496 resolved (3033 by path, 733 by receiver type, 730 by name), 20140 unresolved
```

A large `unresolved` count is healthy — every call into the standard library
or a third-party package belongs there. What matters is the *movement*
between two runs of the same repo.

Two consequences worth knowing:

- **Member nodes are named `Type.member`.** Java always did this; TypeScript
  and Python now do too, which changes their node ids and costs a one-time
  re-embed on the first `ug regen`. It buys correct `Contains` edges from a
  type to its members, and it splits into searchable words —
  `OrderService.cancel` gives four where `cancel` gives one. Rust keeps its
  existing `Type::method` spelling, so Rust ids are unchanged.
- **Construction is not a call.** `new Foo()`, `Foo { .. }` and Python's
  `Foo()` emit `Instantiates`, pointing at the constructor where one is
  declared and at the type itself where none is.

### 3.1b Annotations, decorators and attributes

All four code pipelines record what is written above a declaration, into the
same `Symbol.annotations` field: Java annotations, Python decorators,
TypeScript decorators and Rust attributes. For annotation-driven frameworks
this carries more of a symbol's meaning than its body does.

| Language | Source | Name recorded |
|---|---|---|
| Java | `modifiers` on the declaration | simple name — `@org.junit.Test` → `Test` |
| Python | `decorated_definition` | **dotted** — `@app.route` → `app.route` |
| TypeScript | `decorator` on the class/method | simple name — `@Get(':id')` → `Get` |
| Rust | preceding `#[...]` items | **full path** — `#[tokio::main]` → `tokio::main` |

Python and Rust keep the qualifier because there it *is* the identity: a
Flask route is written against whatever the app object is called, and
`@app.route`, `@bp.route` and `@api.route` are one framework — while a bare
`route` is a plausible name for any method. Rust's `#[tokio::main]` is not
`main`. Java strips the package because that is how Java itself is written
and searched. Inner `#![...]` attributes are skipped: they configure the
enclosing module, not the next item.

**Java annotations become prose.** `@Repository`, `@Entity`, `@Table(name =
"orders")`, `@Transactional`, `@Query`, `@KafkaListener` and the mapping
family are rendered into the dense text as the words someone would search
with ("Spring data access repository", "mapped to table orders"). Mapping
annotations additionally compose a route — `GET /api/orders/{id}` — which
becomes both a field on the handler and a `Route` node of its own. That
string appears in no identifier, no path and no Javadoc, and it is exactly
what a question about an endpoint contains.

Route *composition* — joining a type-level prefix to a member-level path —
stays Java-only, because it is a Spring/JAX-RS convention. Elsewhere the path
is reported as the decorator's argument instead (see §3.1c).

Java's qualified names come from the `package` declaration in the source
rather than from the file's path, which is why it needs none of the module-path
machinery in §3.1a — it is also why Java had all of this first. Resolution is
still local to one file: its imports, its own declarations, and its package.
There is no classpath, and a JDK type that resolves outward to a
package-local name simply matches nothing.

### 3.1c System boundaries

A call graph answers "what calls what". It cannot answer "who outside this
system depends on it", because the edges that leave the process are exactly
the ones it does not have. A `@JmsListener` has no inbound call anywhere in
the source; a `@GetMapping` handler's real callers are HTTP clients that were
never indexed. Both look like dead code, and both are contracts.

So a post-pass (`indexer/boundary.rs`) tags them. It runs after extraction,
when every symbol in the file is available, so a rule can read the symbol's
own annotations, the annotations on its declaring type, its supertypes and
its call sites — without any language module knowing "boundary" is a concept.

Each tag carries a `kind` (`http.endpoint`, `mq.listener`, `cli.command`,
`http.client`, `db.access`, `mq.producer`, `scheduled.job`, `ws.endpoint`), a
`direction` (inbound = a way in, outbound = a way out), a `protocol` (`http`,
`jms`, `kafka`, `amqp`, `jdbc`, `cli`, …), a `detail` (the route, the queue
name, the cron expression) and a `source` — the id of the rule that fired, so
a wrong tag is traceable to one table row rather than to "the indexer".

A symbol can carry several. A `@GetMapping` method that calls
`repository.save(..)` is an inbound HTTP surface *and* an outbound
persistence one, and collapsing that to one tag would lose whichever half was
checked second.

**Boundary-ness is not a node type.** A Spring handler stays a `Function`.
Promoting it would silently change the answer to "how many functions does
this repo have", along with `dead_code`, `long_functions` and every other
count that filters on node type.

**Adding a framework is one row** in `boundary::RULES` plus one test. A rule
matches on an annotation (exact, or a `*.route` wildcard), an annotation on
the declaring type, a supertype, a call site, a `main`-style entry point, or
all of several at once.

Two deliberate restraints, both about not inventing boundaries:

- **Outbound rules fire on a curated client type** (`RestTemplate`,
  `JdbcTemplate`, `KafkaTemplate`), never on a verb like `save` or `send`. A
  service method that calls a repository interface is **not** tagged — the
  repository is where the system actually reaches the database, and tagging
  every transitive caller would make "outbound" mean "touches business logic".
- **Express is not supported.** It registers routes by calling
  `app.get('/users', h)`, and the only matcher that could reach it is a bare
  callee name — which would also match `map.get(k)` and tag half a TypeScript
  codebase as HTTP endpoints. `CallRef` records the receiver's resolved
  *type*, not the identifier `app`. A missing boundary is recoverable; a graph
  full of invented ones is not. The same limit applies to axum routers built
  by chaining, where the receiver cannot be typed.

At query time this becomes the `boundary`, `boundary_in`, `boundary_out`,
`boundary_kinds`, `boundary_protocols` and `boundary_detail` properties, the
`boundaries` / `boundary_census` / `boundary_impact` presets, a `boundary`
field on every agent-tool result, and a dashed ring in the visualization.

### 3.2 Markdown

The chunk is a **heading section**, and two different ranges are computed for
it:

- **`start_line` … `end_line`** — the heading through the line before the next
  heading of the *same or shallower* level. An `#` H1 therefore spans all of
  its `##` children. This is the span captured into `code`, so snippet reads
  and the sparse index see the whole subtree.
- **the embedded body** — the heading through the line before the next heading
  of *any* level (`section_prose`, `languages/markdown.rs`). This is the
  narrower "own body" and it is what becomes the `docstring`.

Using the narrower range for the vector is the point. The wide span would copy
every child's prose into each ancestor's vector — blurring both — and would
make an edit to one subsection re-embed the entire chain of headings above it.

Before reaching the embedder the body is cleaned:

- **fenced code blocks are dropped.** Same reason code bodies aren't embedded:
  punctuation-dense, crowds out prose inside the cap, and already reachable
  through the sparse channel, which indexes the captured span verbatim. A
  section that is *only* a fence gets no docstring and falls back to the
  structural synopsis — the honest outcome.
- **inline links collapse to their text.** `[ingest guide](./ingest.md)` →
  `ingest guide`. The URL is not query vocabulary, and the path is already an
  `Imports` edge.
- **blank lines collapse**, so the section arrives as one run of prose.
- **capped at 8,192 bytes** (`SECTION_TEXT_HARD_CAP`) for storage. That is a
  bound on `graph.json`, not on the embedding — how much of it reaches the
  vector is decided later, per §6.2.

Markdown also **opts out of comment extraction** (`storage/ingest.rs`,
`PROSE_EXTS`). The comment scanner treats `#` as a line-comment marker — which
in markdown is every heading — and its string-literal tracking trips on
ordinary apostrophes and backticks. What it returned for a document was
mangled prose, not comments.

Local links (`[text](./path)`) become `ImportInfo` entries, so the graph
connects docs to the files and sibling docs they reference. URLs, `mailto:`
and bare `#anchor` targets are ignored.

### 3.3 Binary documents (PDF, Word, Excel, PowerPoint)

The chunk is a **page**. PDFs are read directly through liteparse's bundled
PDFium; office formats are converted to PDF by shelling out to a local
LibreOffice (`soffice`) first, so a host without LibreOffice on `PATH` simply
skips those files.

- `name` — the page's first non-empty line, capped at 100 chars, prefixed
  `p.3 · `.
- `docstring` — the page's full extracted text, capped at **8,192 bytes**.
  Pages are the chunk, and there is no finer structure to split on, so the cap
  is much looser than markdown's.
- `start_line` / `end_line` — both the page number; these formats are not
  line-oriented, and the field is repurposed as a page index.
- Empty pages (scans with no OCR — OCR is off) still emit a stub symbol so the
  document's structure shows in the UI, but carry no docstring.

No `code` is captured: `capture_graph_code` skips files that aren't valid
UTF-8. Snippet reads for these nodes return nothing, which is why the page
text lives in `docstring` instead.

### 3.4 File, Config and Folder nodes

- **File / Config** — one per indexed file, spanning the whole file. No
  docstring, so the embedding text is the path plus its split words plus the
  `Related:` list. Anything ending in a document extension is classified
  `Documentation` before any path heuristic runs, so `docs/components/intro.md`
  is not mistaken for a component.

  `Config` is the **only** classification that changes a node's *type* — every
  other one is a metadata label — so its rule is deliberately narrow. It fires
  for a configuration data format (`.json`, `.yaml`, `.toml`, …), the
  `*.config.*` entry-point convention (`vite.config.ts`), or a dotfile `rc`
  (`.eslintrc.js`). It does **not** fire for a source module merely *named*
  `config` or `settings`, nor for source files sitting under a `/config/`
  directory: `native/src/config.rs` is a Rust module full of functions, and
  typing it as `Config` moved it out of `File` entirely — wrong colour in the
  visualization, wrong answer to a type-filtered search, and it read as a
  non-code artifact. A directory name and a module name are both too weak to
  override "this is source code".
- **Folder** — derived from the file set, never parsed. Pre-enrichment the
  embedding text is synthesized from classification, language breakdown and
  depth. Folder nodes use their **full path** as the name so `tests/components`
  and `src/components` don't collide in vector space.

## 4. Why code bodies stay out and document bodies go in

These look contradictory. They are not — the material is different.

For a **function**, the name and docstring carry the meaning and the body is
implementation. Embedding the body would: defeat incremental re-ingest (the
text is otherwise whitespace- and body-independent, so reformatting costs zero
re-embeds); overflow the 512-token window for roughly 10% of bodies, silently;
and dilute the name/docstring signal with `let`, `self`, `return`. Code belongs
in the sparse index and the stored column.

For a **document section**, the body *is* the description. The heading is a
two-word label. Without the prose, a section embeds as:

```
Concept: Why bother with a local backend?. . Related: Embedding Backends
```

— reachable only by queries that happen to echo the heading's own words. With
it:

```
Concept: Why bother with a local backend? (why bother with a local backend).
The previous default was a hosted endpoint. That forced every contributor to
spin up an embedding server before any ingest could run, and to manage GPU
memory for that sidecar. Most users just want to index a repo and get a
knowledge graph.. Related: Embedding Backends
```

The re-embed-churn argument also inverts. A code body edit that leaves the
docstring alone is usually not a change in meaning; a prose edit always is, so
re-embedding it is the *correct* outcome, not a cost. Narrowing the body to the
section's own lines keeps that churn local to the edited section.

## 5. What this means at query time

**Dense channel** — matches `node_text`. Post-change, a natural-language
question about a documented concept reaches the doc section directly rather
than only via its heading words.

**Sparse channel — BM25.** `build_node_sparse_vector` tokenizes `node_text` at
weight 1.0 plus `code` at weight 0.35, then puts each term's accumulated
frequency through BM25 saturation. Both ingest and query use the same tokenizer
(FNV-1a over lowercased alphanumeric runs, plus identifier splitting), so
dimensions collide by construction. This is why keyword search finds markdown
content the dense side may have trimmed, and why fenced code excluded from the
embedding is still searchable.

See §5.1 for how BM25 fits an engine that only computes dot products.

**Snippets** — `query::snippet_for` returns the stored `code` column and only
falls back to reading the working tree when that column is empty: folders,
binary documents, rows whose capture failed, and rows written before the column
existed. So a search does **not** fan out into filesystem reads, and the text
an agent sees is the same text the row was embedded from. Re-run `ug gen` on a
store predating that column to stop the fallback reads.

**Staleness** — `file_hash` makes drift checkable instead of invisible. A line
range that has merely shifted still *resolves* against a changed file and would
silently return the wrong lines; `get_code` compares the hash and flags the
slice instead (`agent_tools.rs`, `stored_slice`). Search results do not
currently carry that flag, but the Indexed tab does.

### 5.1 BM25 without a second index

OverGraph's sparse scoring is a plain dot product over posting lists —
`score += query_weight × stored_weight` (`sparse_postings.rs`). It has no
notion of a scoring model, no IDF, no `df`. That turns out to be exactly what
BM25 needs, because BM25 factorizes:

```
score = Σ  IDF(t) · [ tf(t,d)·(k1+1) / (tf(t,d) + k1·(1 − b + b·|d|/avgdl)) ]
           ------    ---------------------------------------------------
           query side                     document side
```

Store the document-side factor as the sparse weight, put IDF in the **query**
vector, and the dot product the engine already computes *is* BM25 — exactly,
not approximately. No second index, no full-text engine, no OverGraph change.
This is the same trick Qdrant, Vespa and Pinecone sparse indexes use.

**`b = 0`, deliberately.** Length normalization is the one term that couples a
document's weight to the corpus (through `avgdl`), so any edit would invalidate
every stored vector and incremental re-ingest would be dead. With `b = 0` the
document factor `tf·(k1+1)/(tf+k1)` depends only on the node itself, stored
vectors stay valid exactly as before, and the two effects that matter most —
IDF and term-frequency saturation — are both kept. Node spans are also far more
uniform in length than the web documents `b` was designed for.

**Where the statistics live.** `refresh_sparse_stats` counts document frequency
over the whole graph during ingest, in the same pass that already has every
node's text in memory, and writes `ug-sparse-stats.json` next to
`ug-meta.json`. Terms appearing in a single node are omitted and read back as
`df = 1` — they are most of any code vocabulary and all share the same
near-maximum IDF, so dropping them typically halves the sidecar.

Document frequency therefore never invalidates a stored vector. It is read at
query time to weight the query, and at ingest only to rank the per-node
dimension cap.

**IDF must be positive.** OverGraph rejects negative sparse weights on the
query side as well as the stored side (the query goes through the same
`canonicalize_sparse_vector`), and the classic Robertson IDF goes negative once
a term appears in more than half the corpus. So `SparseStats::idf` uses
Lucene's smoothed form, `ln(1 + (N − df + 0.5)/(df + 0.5))`, which is strictly
positive everywhere.

**The dimension cap got fixed on the way.** `MAX_SPARSE_DIMS` used to keep the
heaviest *raw frequency* terms — i.e. the most repeated common words, precisely
backwards. It now keeps the highest `saturated_tf × idf`, so a long file loses
its boilerplate instead of its distinctive identifiers.

**Fallbacks.** A store with no stats sidecar (ingested before this existed)
weights queries by plain term frequency — the previous behaviour, not an error.
Neo4j is unaffected either way: it has no sparse-vector type and scores its
keyword leg through a Lucene full-text index, which is already BM25.

## 6. Caps, in one place

| Cap | Value | Stage | Where |
|---|---|---|---|
| Embedded description | derived from the model | embed | `EmbedBudget`, `limits.rs` |
| Markdown section capture | 8,192 bytes | index | `SECTION_TEXT_HARD_CAP`, `languages/markdown.rs` |
| Document page text | 8,192 bytes | index | `PAGE_TEXT_CAP`, `indexer/document.rs` |
| Document page name | 100 bytes | index | `NAME_CAP`, `indexer/document.rs` |
| Extracted comments per node | 600 chars | embed | `MAX_COMMENT_CHARS`, `storage/comments.rs` |
| `Related:` names per node | 128 | embed | `MAX_RELATED`, `storage/text.rs` |
| Sparse dimensions per node | 512 | embed | `MAX_SPARSE_DIMS`, `storage/text.rs` |
| Search result budget | 60,000 chars | retrieve | `DEFAULT_CONTEXT_CHARS`, `storage/query.rs` |
| MCP snippet preview | 1,200 chars | retrieve | `SNIPPET_PREVIEW_CHARS`, `mcp/format.rs` |
| Captured `code` | uncapped | index | `storage/source.rs` |
| Dense vector dimension | 384 (default) | embed | `EMBEDDING_DIM`, `storage/embed.rs` |

**Stage** says what it costs to change one. `index` needs a re-index; `embed`
needs a re-embed, which `ug gen` does automatically because the text changes;
`retrieve` takes effect on the next query with no re-index at all.

Measured on a representative graph, the median `node_text` uses about 6% of the
embedder's 512-token window and p99 about 200 tokens — so for most nodes the
binding constraint on what goes in is *signal*, not budget.

### 6.1 Where a user sees them

These caps decide what a node's vector can match on at all, so they are
published rather than left to be inferred from a chunk that looks cut off:

- **`GET /api/capabilities`** returns a `limits` object: `caps[]` (each with
  `id`, `label`, `value`, `unit`, `stage`, `extensions`, `effect` and the
  `source` constant), plus `embedder_model` and `embedder_token_window`.
- **The visualization's Indexed tab** shows an *Indexing limits* section listing
  the caps that apply to that node's file type, marking the ones that
  measurably bit it — a truncation ellipsis, a `Related:` list that came back
  exactly full, or a keyword vector at the dimension cap. The section
  auto-expands when something was reached, and the summary reads
  `Indexing limits — 1 reached`.
- **A *Captured source* section**, collapsed, showing the `code` column
  verbatim — the snapshot an agent's snippet reads return and the keyword index
  was built from. Deliberately separate from the Source tab, which reads the
  same span live from disk (falling back to this captured code when the repo
  path is gone): when the two disagree the store is stale, and seeing both is
  the only way to tell that from the UI. Capped at 20,000 chars
  for display, with the full length reported.
- **A *Storage metadata* section**, also collapsed: dense and sparse vector
  sizes, embedded-text length, when the row last changed, and the file hash
  with a live staleness check. It auto-expands and its summary reads
  `Storage metadata — stale` when the file has changed since indexing.

  These come from `GET /api/db/node/:id`, which returns them under a
  `storage` key. They are computed only for that single-row hydrate, not for
  `/api/db/traverse` — the staleness check hashes the file on disk and
  `sparse_dims` rebuilds the keyword vector, which is fine per click and not
  fine per traversal node.

Long values collapse. A document section's `docstring` is now a paragraph
rather than a sentence, so the field block's **Docstring** row and the Indexed
tab's **Description** and **Notes from comments** fields show a one-line preview
plus a character count above 180 chars, and expand on click. List-valued fields
(**Calls out to**, **Extends**, **Implements**) render as chips that wrap,
collapse past eight entries, and navigate when the name resolves to an indexed
node.

### 6.4 Panel vocabulary

The node panel shows four data sources side by side, and the cost of confusing
them is misreading a search result. So the naming distinguishes them, and every
label, tab and section carries a tooltip saying where its value came from, what
reads it, and how it relates to the other fields:

| Surface | Source | Reads |
|---|---|---|
| field block | `graph.json`, hydrated from `/api/db/node` | the node as it was **indexed** |
| **Source** tab | working tree via `/api/file`, falling back to the indexed copy (`NodeRow.code`) when the repo path is gone — the response carries `"source": "filesystem"|"db"` | the file as it is **now** (or as it was **indexed**, when the repo is gone) |
| **Indexed** tab | vector store, via `/api/db/node` | what search **matches against** |
| **Hierarchy** tab | `Contains` edges | containment only |
| **Related** tab | all edges | the neighbourhood ranking expands into |

`FIELD_DOCS`, `TAB_DOCS`, `EDGE_DOCS` and `STAGE_DOCS` in `visualization.html`
hold those explanations, keyed rather than inlined so a label and its
explanation cannot drift apart. Labels that carry one are marked with a dotted
underline — without the cue nobody hovers, and the explanations might as well
not exist.

Two renames worth knowing if you have older screenshots: the **Preview** tab is
now **Source**, and the **Chunk** tab is now **Indexed**. "Chunk" was a term of
art that also collided with this document's use of the word for a unit of
indexing, and the Source tab's content label said `Chunk` while meaning "a line
span rather than the whole file" — it now says `Line span` or `Whole file`.

`native/src/limits.rs` is the single list. Every entry reads the constant that
enforces the behaviour, and a unit test asserts the published numbers still
track those constants — the one value defined in that module is the description
budget, which is not a constant at all (§6.2). Adding a cap means adding an
entry there.

### 6.2 The description budget follows the model

The embedding model's own input window binds above every number in the table,
and it applies with **no truncation marker anywhere** — the tokenizer simply
stops reading. So the one cap that could not be a constant is the description
budget: it exists *because of* the window, and the user picks the model.

`EmbedBudget::resolve` derives it:

```
budget  = clamp(window_tokens × 3.7 − reserve, 500, 8_000)
reserve = 150 (type + name + split name) + MAX_COMMENT_CHARS
```

`bge-small-en-v1.5` (512 tokens) → **1,144 chars**; `nomic-embed-text-v1.5`
(8,192) → the 8,000 ceiling; an unrecognised or remote model → a fixed 1,500,
which is what markdown sections were hard-coded to before this existed, so
behaviour for those models is unchanged.

The reserve deliberately **excludes** the `Related:` list. Position in the
template decides who loses to the tokenizer, and `Related:` comes last —
so an overflowing node drops neighbour names, never its description. Reserving
for them would shrink the description to protect the designated casualty.
`EmbedBudget::related_advisory` reports when `MAX_RELATED` exceeds what fits,
because the knob to turn there is `MAX_RELATED`, not `--section-cap`.

**Why this is an embed-stage cap.** It used to live in `markdown.rs` as
`SECTION_TEXT_CAP = 1500`, applied while reading files — before any embedder
existed, so the number could only ever have been a guess about the model. Two
things follow from moving it: the guess is replaced by the actual window, and
switching models now needs a re-embed rather than a re-index. The indexer keeps
the full prose up to `SECTION_TEXT_HARD_CAP`; only the vector is trimmed.

Applying it uniformly to every node's description also **fixes the PDF case**
noted in earlier revisions of this doc: a page is captured at 8 KB for storage
and display, and now only what fits the window reaches the vector, rather than
overflowing it silently.

**Adjusting it.** In precedence order:

1. `--section-cap <chars>` on `ug gen` / `ug ingest`.
2. `ug config set embed.section_cap <chars>` to persist it.
3. Otherwise auto-derived from the model.

`ug gen` prints the resolved number, its origin, and a warning when the two
disagree:

```
▸ Embedding budget: 1144 chars per description (512 token window, derived from the model)
⚠  description budget is 4000 chars but BAAI/bge-small-en-v1.5 reads only ~1894 chars
   (512 tokens) — text past that is dropped by the tokenizer with no marker
```

`/api/capabilities` carries the same information as `budget_source`,
`embedder_token_window`, `advisory` and `related_advisory`.

### 6.3 Switching models is now safe

The store records the model it was ingested with in `ug-meta.json`, and
`plan_incremental_ingest` refuses to reuse a stored vector when that model has
changed.

This closes a silent correctness hole. The dim check alone does not catch a
model swap — `bge-small-en-v1.5` and `all-MiniLM-L6-v2` are both 384-dimensional,
so switching between them left `node_text` identical, the planner called every
row unchanged, and the store ended up holding vectors from two incompatible
embedding spaces with nothing to indicate it. An unrecorded model (a store
written before the field existed) reads as "unknown", not "mismatched", so
older stores keep their incremental behaviour.

## 7. Adding a new file type

1. Add the extensions to `common::SUPPORTED_EXTS`.
2. Text formats: add a `LanguageIndexer` under `indexer/languages/` and register
   it in `languages::for_extension`. Binary formats: extend
   `document::is_supported_ext` and the `process_file` short-circuit.
3. Decide the chunk unit and set `start_line` / `end_line` to it.
4. **Decide what `docstring` holds.** For a prose format that is the chunk's
   text, capped only for storage — the embedding budget (§6.2) trims it later,
   so do not pre-truncate to a guess about the model.
5. If the format is prose, add its extensions to `PROSE_EXTS` in
   `storage/ingest.rs` so the code-comment scanner doesn't mangle it.
6. Add the extensions to `DOCUMENT_EXTS` in `indexer/classifier.rs` if they
   should classify as `Documentation` regardless of path.
7. Nothing in `storage/` needs to change — `node_text`, both vectors, capture
   and staleness all follow from the `Symbol` fields.
8. Add any new cap to `native/src/limits.rs` so it reaches
   `/api/capabilities` and the Indexed tab instead of silently shaping results.

Steps 4 and 5 are the ones that were missed for markdown, and the symptom was
silent: search still worked through the sparse channel, so the dense side being
empty showed up only when reading a node's raw embedded text.
