// Exercises the local-mode streaming JSON parser that lives in
// `src/vis/js/00-preamble.js` (P12.2).
//
// The parser is the one piece of the vis layer that hand-rolls something the
// platform normally does: it splits a byte stream at JSON element boundaries
// so a graph past V8's 536,870,888-character string ceiling can still be
// loaded. A scanner like that fails at seams — a chunk boundary inside a
// string, inside an escape, between a key and its colon — and every one of
// those failures produces a *wrong graph* rather than an error, so the check
// that matters is equality against the platform's own parser at every chunk
// size, not a smoke test at one.
//
// The whole part file is evaluated rather than the parser extracted from it:
// an extractor is a second copy of the thing under test, and drifts.
//
//   node graph_json_stream.mjs <path-to-00-preamble.js> [graph.json …]
//
// Any extra arguments are real graph files. Each is parsed by the streaming
// parser and compared, element for element, against `JSON.parse` — in pieces
// where the document is too large for one string, which is the case this
// exists for.

import fs from 'node:fs';
import assert from 'node:assert/strict';

const [preamble, ...fixtures] = process.argv.slice(2);
const src = fs.readFileSync(preamble, 'utf8');
const { createGraphJsonParser } = new Function(
    src + '\n; return { createGraphJsonParser };',
)();

const enc = new TextEncoder();

function parse(bytes, chunkSize) {
    const p = createGraphJsonParser();
    for (let i = 0; i < bytes.length; i += chunkSize) {
        p.push(new Uint8Array(
            bytes.buffer, bytes.byteOffset + i,
            Math.min(chunkSize, bytes.length - i),
        ));
    }
    return p.end();
}

// ── Every seam, on documents small enough to split one byte at a time ──
//
// A one-byte chunk size puts a boundary between every pair of bytes in the
// document, which is the only way to be sure the carry paths are all reached.
const documents = [
    '{}',
    '{"a":1}',
    '{"nodes":[],"edges":[]}',
    '{"nodes":[{"id":"a"}],"edges":[]}',
    '{"a":"x","b":true,"c":null,"d":-1.5e3,"e":[1,2,3],"f":{"g":[{"h":"}"}]}}',
    // structural characters inside strings, and an escaped backslash before a quote
    '{"s":"a\\"b,]}","arr":["}",  "]" , "\\\\" ],"n":3}',
    '  {  "a" : [ 1 , 2 ] , "b" : { "c" : 4 }  }  ',
    // multi-byte UTF-8, which a byte-level scanner must not split a decode across
    '{"u":"café 你好 😀","arr":["ééé","😀"]}',
    '{"deep":[[[[1]]],{"x":[{"y":[]}]}],"t":"end"}',
    '{"nums":[0,1,-2,3.5,1e10,true,false,null],"z":0}',
    // the shape the product actually ships
    '{"nodes":[{"id":"folder:.","name":".","node_type":"Folder"}],'
    + '"edges":[{"source":"a","target":"b","edge_type":"Imports"}],'
    + '"stats":{"totalFiles":1},"resolution":{"resolvedTyped":0}}',
];

let parses = 0;
for (const text of documents) {
    const want = JSON.parse(text);
    const bytes = enc.encode(text);
    for (const chunk of [1, 2, 3, 5, 7, 13, 64, 4096]) {
        assert.deepEqual(parse(bytes, chunk), want,
            `document ${JSON.stringify(text)} at chunk size ${chunk}`);
        parses++;
    }
}
console.log(`ok  ${parses} parses of ${documents.length} documents, chunk sizes 1..4096`);

// ── Malformed input must be rejected, not silently half-parsed ──
const malformed = ['[1,2]', '{"a":', '{"a":1', '{"a":[1,2', 'null', '{"a" 1}', '{'];
for (const bad of malformed) {
    assert.throws(() => parse(enc.encode(bad), 3),
        `expected ${JSON.stringify(bad)} to be rejected`);
}
console.log(`ok  ${malformed.length} malformed documents rejected`);

// ── Real graphs, against the platform parser ──
for (const file of fixtures) {
    if (!fs.existsSync(file)) { console.log(`--  ${file} absent, skipped`); continue; }
    const bytes = fs.readFileSync(file);
    const got = parse(bytes, 65536);

    // Slice each top-level array out of the file by its own key so the
    // platform parser can act as a reference even for a document no single
    // string can hold — the case this parser exists for.
    const bounds = (name) => {
        const key = Buffer.from(`"${name}":[`);
        const at = bytes.indexOf(key);
        if (at < 0) return null;
        const start = at + key.length - 1;
        let depth = 0, inStr = false, esc = false;
        for (let i = start; i < bytes.length; i++) {
            const b = bytes[i];
            if (inStr) { if (esc) esc = false; else if (b === 92) esc = true; else if (b === 34) inStr = false; continue; }
            if (b === 34) inStr = true;
            else if (b === 91 || b === 123) depth++;
            else if (b === 93 || b === 125) { if (--depth === 0) return [start, i + 1]; }
        }
        throw new Error(`unterminated ${name} in ${file}`);
    };
    // …and split that range at element boundaries so each piece is under the
    // ceiling too.
    const pieces = (from, to, limit) => {
        const out = [];
        let depth = 0, inStr = false, esc = false, last = from + 1;
        for (let i = last; i < to - 1; i++) {
            const b = bytes[i];
            if (inStr) { if (esc) esc = false; else if (b === 92) esc = true; else if (b === 34) inStr = false; continue; }
            if (b === 34) inStr = true;
            else if (b === 91 || b === 123) depth++;
            else if (b === 93 || b === 125) depth--;
            else if (b === 44 && depth === 0 && i - last >= limit) { out.push([last, i]); last = i + 1; }
        }
        out.push([last, to - 1]);
        return out;
    };

    for (const name of ['nodes', 'edges']) {
        const range = bounds(name);
        if (!range) continue;
        let off = 0, n = 0;
        for (const [f, t] of pieces(range[0], range[1], 300e6)) {
            const ref = JSON.parse('[' + bytes.toString('utf8', f, t) + ']');
            assert.deepEqual(got[name].slice(off, off + ref.length), ref,
                `${file}: ${name}[${off}..${off + ref.length})`);
            off += ref.length; n++;
        }
        assert.equal(off, got[name].length, `${file}: ${name} length`);
        console.log(`ok  ${file}  ${name}: ${off} identical to JSON.parse (${n} piece(s))`);
    }
}
