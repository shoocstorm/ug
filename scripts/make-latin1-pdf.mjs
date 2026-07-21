#!/usr/bin/env node
// Regenerates native/tests/fixtures/latin1.pdf.
//
// The suite needs a PDF whose text is multi-byte once extracted, to prove
// the extractor decodes PDF's encodings and that truncate() respects char
// boundaries. The older emoji fixture can't serve that any more: its glyphs
// come from an embedded font with no usable Unicode mapping, so PDFium
// returns nothing for it.
//
// This uses Helvetica with WinAnsiEncoding — a base-14 font, nothing
// embedded — so any conformant extractor can read it. The high Latin-1
// bytes (é ü ï ß) become multi-byte UTF-8 after extraction.

import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT = join(
  dirname(dirname(fileURLToPath(import.meta.url))),
  'native/tests/fixtures/latin1.pdf',
);

// Octal escapes are Latin-1 code points: é=351 ü=374 ï=357 ß=337
const TEXT = 'caf\\351 m\\374nchen na\\357ve \\337';

const stream = `BT /F1 18 Tf 20 40 Td (${TEXT}) Tj ET`;
const objs = [
  null,
  '<< /Type /Catalog /Pages 2 0 R >>',
  '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
  '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 100] /Contents 4 0 R ' +
    '/Resources << /Font << /F1 5 0 R >> >> >>',
  `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`,
  '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>',
];

let out = '%PDF-1.4\n';
const offsets = [];
for (let i = 1; i < objs.length; i++) {
  offsets[i] = out.length;
  out += `${i} 0 obj\n${objs[i]}\nendobj\n`;
}

// Cross-reference table: byte offset of every object, 10 digits zero-padded.
const xref = out.length;
out += `xref\n0 ${objs.length}\n0000000000 65535 f \n`;
for (let i = 1; i < objs.length; i++) {
  out += `${String(offsets[i]).padStart(10, '0')} 00000 n \n`;
}
out += `trailer\n<< /Size ${objs.length} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;

// latin1, not utf8 — the octal escapes above address single bytes.
writeFileSync(OUT, Buffer.from(out, 'latin1'));
console.log(`wrote ${OUT} (${out.length} bytes)`);
