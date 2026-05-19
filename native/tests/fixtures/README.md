# Test fixtures

Tiny binary inputs the test suite needs at runtime. Kept here so tests
are reproducible without network access.

## PDF fixtures

| File | Pages | Content | Source |
|---|---|---|---|
| `hello.pdf` | 1 | "Hello World!" | Derived from the public `assets/example.pdf` shipped with the [`lopdf`](https://crates.io/crates/lopdf) crate (MIT). |
| `unicode.pdf` | 1 | "😀 🔧 🔨" | Derived from `assets/unicode.pdf` shipped with the same crate (MIT). |

Both files exercise the `pdf-extract` text extraction path in
`indexer/pdf.rs`. They are small (<4 KB combined) and deterministic —
the test suite checks exact extracted text against expected values, so
swapping them out requires updating those expectations.
