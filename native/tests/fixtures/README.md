# Test fixtures

Tiny binary inputs the test suite needs at runtime. Kept here so tests
are reproducible without network access.

## PDF fixtures

| File | Pages | Content | Extractable? | Source |
|---|---|---|---|---|
| `hello.pdf` | 1 | "Hello World!" | yes | Derived from the public `assets/example.pdf` shipped with the [`lopdf`](https://crates.io/crates/lopdf) crate (MIT). |
| `latin1.pdf` | 1 | "café münchen naïve ß" | yes | Generated — see below. |
| `unicode.pdf` | 1 | "😀 🔧 🔨" | yes (emoji) | Derived from `assets/unicode.pdf` shipped with the same crate (MIT). |

These exercise the document indexing path in `indexer/document.rs`, which
reads PDFs through [`pdf-extract`](https://crates.io/crates/pdf-extract) —
a pure-Rust extractor with no native dependencies.

**On `unicode.pdf`:** its emoji are drawn through an embedded font. The
previous backend (pdfium) had no usable mapping for it and extracted
nothing; `pdf-extract` decodes the glyphs correctly. `pdf_unicode_emoji_
survive_extraction` pins that — and the file must always be indexed and
yield a well-formed page symbol regardless of whether the extractor
returns text. (The image-only / scanned-PDF case — where the extractor
legitimately returns an empty page — is still handled defensively in
`process_document`, which emits a "Page N (no text)" stub symbol with no
docstring so the file's structure stays visible.)

Multi-byte round-tripping is covered by `latin1.pdf` instead, whose high
Latin-1 bytes become multi-byte UTF-8 after extraction. It uses a base-14
font with `WinAnsiEncoding` and embeds nothing, so any conformant extractor
can read it. Regenerate with:

```bash
node scripts/make-latin1-pdf.mjs
```

The suite asserts on extracted text, so swapping any fixture means updating
those expectations.
