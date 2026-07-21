# Test fixtures

Tiny binary inputs the test suite needs at runtime. Kept here so tests
are reproducible without network access.

## PDF fixtures

| File | Pages | Content | Extractable? | Source |
|---|---|---|---|---|
| `hello.pdf` | 1 | "Hello World!" | yes | Derived from the public `assets/example.pdf` shipped with the [`lopdf`](https://crates.io/crates/lopdf) crate (MIT). |
| `latin1.pdf` | 1 | "café münchen naïve ß" | yes | Generated — see below. |
| `unicode.pdf` | 1 | "😀 🔧 🔨" | **no** | Derived from `assets/unicode.pdf` shipped with the same crate (MIT). |

These exercise the document indexing path in `indexer/document.rs`, which
reads PDFs through `liteparse`'s bundled PDFium backend (Office formats are
converted to PDF via LibreOffice first, then parsed the same way).

**On `unicode.pdf`:** its emoji are drawn through an embedded font with no
usable Unicode mapping, so PDFium extracts nothing from it. That is not a
bug — it is the ordinary case of a scanned or image-only PDF, and
`pdf_without_extractable_text_degrades_gracefully` pins the expected
behaviour: the file is still indexed, still produces one page symbol, and
that symbol is named "…(no text)" with an empty docstring.

Multi-byte round-tripping is covered by `latin1.pdf` instead, whose high
Latin-1 bytes become multi-byte UTF-8 after extraction. It uses a base-14
font with `WinAnsiEncoding` and embeds nothing, so any conformant extractor
can read it. Regenerate with:

```bash
node scripts/make-latin1-pdf.mjs
```

The suite asserts on extracted text, so swapping any fixture means updating
those expectations.
