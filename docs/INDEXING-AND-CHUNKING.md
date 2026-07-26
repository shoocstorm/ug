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
| `.java` | tree-sitter (`languages/java.rs`) | one symbol | Function / Class / Interface | name + Javadoc + signature + inline comments | the symbol's line span |
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

`node_modules/`, `.git/`, `target/`, anything matched by the repo's
`.gitignore`, the built-in artifact globs (`*.min.js`, `*.bundle.js`,
`dist/`), and any glob in the `UG_IGNORE` env var.

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
- **capped at 1,500 bytes**, char-boundary-safe, with a trailing `…`. Long
  sections keep their leading paragraphs, which in practice state the topic.

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
  `Related:` list. A file classified `Config` (under a `/config/` directory, or
  named `config` / `settings`) becomes a `Config` node instead of a `File`
  node; the content handling is identical. Anything ending in a document
  extension is classified `Documentation` before any path heuristic runs, so
  `docs/components/intro.md` is not mistaken for a component.
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

**Sparse channel** — `build_node_sparse_vector` tokenizes `node_text` at weight
1.0 plus `code` at weight 0.35, capped at 512 dimensions, keeping the heaviest
terms. This is why keyword search found markdown content even while the dense
side was blind to it, and why fenced code excluded from the embedding is still
searchable. Both ingest and query use the same tokenizer (FNV-1a over
lowercased alphanumeric runs, plus identifier splitting), so dimensions collide
by construction.

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
currently carry that flag.

## 6. Caps, in one place

| Cap | Value | Where |
|---|---|---|
| Markdown section prose | 1,500 bytes | `SECTION_TEXT_CAP`, `languages/markdown.rs` |
| Document page text | 8,192 bytes | `PAGE_TEXT_CAP`, `indexer/document.rs` |
| Document page name | 100 bytes | `NAME_CAP`, `indexer/document.rs` |
| Extracted comments per node | 600 chars | `MAX_COMMENT_CHARS`, `storage/comments.rs` |
| `Related:` names per node | 24 | `MAX_RELATED`, `storage/text.rs` |
| Sparse dimensions per node | 512 | `MAX_SPARSE_DIMS`, `storage/text.rs` |
| Captured `code` | uncapped | `storage/source.rs` |
| Dense vector dimension | 384 (default) | `EMBEDDING_DIM`, `storage/embed.rs` |

Measured on a representative graph, the median `node_text` uses about 6% of the
embedder's 512-token window and p99 about 200 tokens — so the binding
constraint on what goes in is *signal*, not budget.

## 7. Adding a new file type

1. Add the extensions to `common::SUPPORTED_EXTS`.
2. Text formats: add a `LanguageIndexer` under `indexer/languages/` and register
   it in `languages::for_extension`. Binary formats: extend
   `document::is_supported_ext` and the `process_file` short-circuit.
3. Decide the chunk unit and set `start_line` / `end_line` to it.
4. **Decide what `docstring` holds.** For a prose format that is the chunk's
   text (capped); for a code format it is the written doc comment only.
5. If the format is prose, add its extensions to `PROSE_EXTS` in
   `storage/ingest.rs` so the code-comment scanner doesn't mangle it.
6. Add the extensions to `DOCUMENT_EXTS` in `indexer/classifier.rs` if they
   should classify as `Documentation` regardless of path.
7. Nothing in `storage/` needs to change — `node_text`, both vectors, capture
   and staleness all follow from the `Symbol` fields.

Steps 4 and 5 are the ones that were missed for markdown, and the symptom was
silent: search still worked through the sparse channel, so the dense side being
empty showed up only when reading a node's raw embedded text.
