// The info panel's checkable halves, run under `node`.
//
// `src/vis/js/14-interaction.js` is where every way of picking a node ends up,
// and `handleClick` — 415 lines of it, with 22 symbols depending on it — turns
// the chosen node into the panel the reader acts on. Four of its parts fail
// quietly rather than loudly, and none of them was covered:
//
//   1. `mdToHtml` renders a docstring into `innerHTML`. It is a hand-rolled
//      markdown renderer, so it is also the panel's injection surface: a
//      `javascript:` link that survives it is a live one.
//   2. `longFieldRow` / `chipRow` build the field rows. They decide when a
//      value is too long to show inline and when a name list collapses, and
//      both put repository content — symbol names, file paths, docstrings —
//      through `innerHTML`.
//   3. `parseChunkText` reads the stored chunk text back into fields. A
//      mis-parse shows the wrong prose under the right heading, with nothing
//      on the console.
//   4. `findNodeByName` decides which chips link. Its server-mode branch must
//      memoise *misses*, or the re-render it triggers queues the same name
//      again forever.
//
// All four are read out of the real parts with a string slice rather than
// transcribed, so this cannot pass against a copy that has drifted from what
// ships.
//
// Booting the real page to check this instead is a runaway CPU load — see
// Agents.md §10r.
//
// argv: <14-interaction.js> <17-info-drag.js> <02-dialogs.js>

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [interactionPath, dragPath, dialogsPath] = process.argv.slice(2);
const src = fs.readFileSync(interactionPath, 'utf8');
const dragSrc = fs.readFileSync(dragPath, 'utf8');
const dialogsSrc = fs.readFileSync(dialogsPath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

// Lift one `function name(...) { ... }` out of a concatenated part. The parts
// are indented to a fixed depth inside the page's module, so the closing brace
// at that depth ends the function. `indent` is that depth: 8 for a part's own
// functions, deeper for one nested inside another.
function lift(source, name, indent = 8) {
    const pad = ' '.repeat(indent);
    const start = source.indexOf('\n' + pad + 'function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found at indent ${indent} — has it been renamed?`);
    const end = source.indexOf('\n' + pad + '}', start);
    if (end < 0) throw new Error(`${name} has no closing brace at indent ${indent}`);
    return source.slice(start, end + pad.length + 2);
}
function liftConst(source, decl, close) {
    const start = source.indexOf('\n        const ' + decl);
    if (start < 0) throw new Error(`${decl} not found`);
    const end = source.indexOf('\n        ' + close, start);
    if (end < 0) throw new Error(`${decl} has no ${close}`);
    return source.slice(start, end + close.length + 9);
}
// A run of declarations that belong together. `fieldLabel` through `chipRow`
// are four helpers and two thresholds in one contiguous block inside
// `handleClick`; slicing the block keeps them in step with each other.
function sliceBetween(source, from, to) {
    const start = source.indexOf(from);
    if (start < 0) throw new Error(`start marker not found: ${from}`);
    const end = source.indexOf(to, start);
    if (end < 0) throw new Error(`end marker not found: ${to}`);
    return source.slice(start, end);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-interaction-'));
const write = (name, body) => {
    const p = path.join(tmp, name);
    fs.writeFileSync(p, body);
    return p;
};

// `escapeHtml` escapes by round-tripping through a text node, so the stub has
// to serialise the way a text node does: `&`, `<` and `>` and nothing else.
// The quote in particular survives, which is why `escapeHtml` is not safe on
// its own inside an attribute — pinned below rather than assumed away.
const DOM_STUB = `
    globalThis.document = {
        createElement() {
            let text = '';
            return {
                set textContent(v) { text = v == null ? '' : String(v); },
                get textContent() { return text; },
                get innerHTML() {
                    return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
                },
            };
        },
    };
`;

// ── 1. A docstring rendered as markdown ──────────────────────────────────────

const mdMod = write('md.mjs', `
    ${lift(src, 'mdToHtml')}
    export { mdToHtml };
`);
const M = await import(mdMod);
const md = s => M.mdToHtml(s);

console.log('rendering a docstring as markdown');

must('a paragraph is a paragraph', md('hello world') === '<p>hello world</p>');
must('consecutive lines join into one paragraph',
    md('one\ntwo') === '<p>one two</p>');
must('a blank line starts a new paragraph',
    md('one\n\ntwo') === '<p>one</p>\n<p>two</p>');
must('a heading becomes a heading', md('## Title') === '<h2>Title</h2>');
must('heading depth is capped at four',
    md('###### Deep') === '<h4>Deep</h4>');
must('a bullet list is a list',
    md('- a\n- b') === '<ul>\n<li>a</li>\n<li>b</li>\n</ul>');
must('a numbered list is an ordered list',
    md('1. a\n2. b') === '<ol>\n<li>a</li>\n<li>b</li>\n</ol>');
// Switching list type without closing the first one nests `<ol>` inside `<ul>`,
// which renders as one run-on list.
must('switching list type closes the first list',
    md('- a\n1. b').startsWith('<ul>\n<li>a</li>\n</ul>\n<ol>'));
must('a rule is a rule', md('---') === '<hr>');
must('a quote is a blockquote', md('> quoted') === '<blockquote>quoted</blockquote>');
must('a fence becomes a code block',
    md('```rs\nlet x = 1;\n```') === '<pre><code>let x = 1;</code></pre>');
must('inline code is code', md('use `foo` here').includes('<code>foo</code>'));
must('bold is strong', md('**loud**') === '<p><strong>loud</strong></p>');
must('italic is emphasis', md('*soft*') === '<p><em>soft</em></p>');

// The panel writes this into `innerHTML`. Everything below is the difference
// between rendering a docstring and running one.
must('a tag in prose is escaped',
    md('<script>alert(1)</script>') === '<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>');
must('a tag in a heading is escaped', !md('# <img src=x>').includes('<img'));
must('a tag in a list item is escaped', !md('- <img src=x>').includes('<img'));
must('a tag in a quote is escaped', !md('> <img src=x>').includes('<img'));
must('code fence content is escaped',
    md('```\n<script>x</script>\n```').includes('&lt;script&gt;'));
must('an ampersand is escaped once', md('a & b') === '<p>a &amp; b</p>');

// A link's href goes in unescaped, so the scheme test is the only thing
// standing between a docstring and a script URL.
const href = s => (md(s).match(/href="([^"]*)"/) || [])[1];
must('an http link keeps its target', href('[t](https://example.com)') === 'https://example.com');
must('a root-relative link is kept', href('[t](/docs/x)') === '/docs/x');
must('an anchor link is kept', href('[t](#section)') === '#section');
must('a mailto link is kept', href('[t](mailto:a@b.c)') === 'mailto:a@b.c');
must('a javascript: link is neutralised', href('[t](javascript:alert(1))') === '#');
must('a data: link is neutralised', href('[t](data:text/html,<script>)') === '#');
must('a vbscript: link is neutralised', href('[t](vbscript:msgbox)') === '#');
// `//host/path` is a protocol-relative URL, not a site-relative path. It
// used to pass the filter that exists to allow `/docs/x`.
must('a scheme-relative link is neutralised', href('[t](//evil.test/x)') === '#');
must('a site-relative link still works after that', href('[t](/docs/x)') === '/docs/x');
must('case does not smuggle a scheme through', href('[t](JaVaScRiPt:alert(1))') === '#');
must('a link opens without handing over the opener',
    md('[t](https://example.com)').includes('rel="noopener noreferrer"'));
must('an image becomes its alt text, not a fetch',
    md('![alt](https://evil.test/x.png)') === '<p><em>alt</em></p>');

// ── 2. A stored chunk read back into fields ──────────────────────────────────

const chunkMod = write('chunk.mjs', `
    ${lift(src, 'parseChunkText')}
    export { parseChunkText };
`);
const P = await import(chunkMod);

console.log('reading a chunk back into fields');

const whole = P.parseChunkText(
    'Function: search_kb. Finds things in the store. Signature: fn search_kb(q: &str) -> Hits. '
    + 'Notes: the caller owns the budget. Related: rank_hits, build_query'
);
must('the head becomes the heading', whole.heading === 'Function: search_kb');
must('the prose after the head is the description',
    whole.description === 'Finds things in the store');
must('the signature is its own field',
    whole.signature === 'fn search_kb(q: &str) -> Hits');
must('the notes are their own field', whole.notes === 'the caller owns the budget');
must('related names are split and trimmed',
    JSON.stringify(whole.related) === JSON.stringify(['rank_hits', 'build_query']));

// Each section is optional, and a missing one must not swallow the next.
// A file's name carries its extension, so the head's terminating dot is not
// the first dot in it. Cutting at the first one put "js. …" at the front of
// every file node's description.
const bare = P.parseChunkText('File: src/q.rs. Just prose here');
must('a dotted name is not cut at its first dot',
    bare.heading === 'File: src/q.rs');
must('the text after a dotted name is the description',
    bare.description === 'Just prose here');
const dotted = P.parseChunkText('Function: store.upsertNodes. Writes rows');
must('a dotted symbol name survives too',
    dotted.heading === 'Function: store.upsertNodes' && dotted.description === 'Writes rows');
// The type half stays dot-free, so prose containing a colon cannot be taken
// for the head of a chunk that has none.
const prose = P.parseChunkText('Some prose. Note: a remark here');
must('a colon in mid-prose is not a head', prose.heading === '');
must('absent sections stay empty, not undefined',
    bare.signature === '' && bare.notes === '' && bare.related.length === 0);

const noHead = P.parseChunkText('just some prose with no head');
must('prose with no head is all description',
    noHead.heading === '' && noHead.description === 'just some prose with no head');

// `Related:` is searched for first and cuts the body, so a chunk carrying both
// must not leave the related list inside the notes.
const both = P.parseChunkText('Concept: x. Body. Notes: a note. Related: one, two');
must('notes stop where related begins', both.notes === 'a note');
must('related survives notes above it',
    JSON.stringify(both.related) === JSON.stringify(['one', 'two']));

must('an empty chunk yields empty fields',
    P.parseChunkText('').heading === '' && P.parseChunkText('').description === '');

// ── 3. The panel's field rows ────────────────────────────────────────────────

const fieldsMod = write('fields.mjs', `
    ${DOM_STUB}
    ${lift(dragSrc, 'escapeHtml')}
    ${liftConst(dialogsSrc, 'FIELD_DOCS = {', '};')}

    // Only whether a name resolves matters to a chip; which node it resolves
    // to is \`findNodeByName\`'s own business, checked in its own section.
    const known = new Map();
    const state = { nodeById: known };
    function findNodeByName(n) { return known.get(n) || null; }

    ${sliceBetween(src, '            const fieldLabel = (key) => {', '            // The title carries')}
    export { fieldLabel, fieldRow, longFieldRow, chipRow, known, LONG_FIELD_CHARS, CHIPS_INLINE_MAX };
`);
const F = await import(fieldsMod);

console.log('building the panel fields');

must('a field row carries the documented label',
    F.fieldRow('name', 'x').includes('Name'));
must('a field row carries its value', F.fieldRow('name', 'searchKb').includes('searchKb'));
must('an undocumented key falls back to the key itself',
    F.fieldLabel('mystery').includes('mystery'));

// Long values collapse, because a docstring rendered inline pushes the tabs —
// and everything else worth clicking — off the panel.
const shortDoc = 'a'.repeat(F.LONG_FIELD_CHARS);
const longDoc = 'a'.repeat(F.LONG_FIELD_CHARS + 1);
must('a value at the threshold stays a plain row',
    !F.longFieldRow('docstring', shortDoc).includes('<details'));
must('a value past the threshold collapses',
    F.longFieldRow('docstring', longDoc).includes('<details'));
must('a collapsed value still carries the whole text',
    F.longFieldRow('docstring', longDoc).includes('info-full'));
must('a collapsed value counts its characters',
    F.longFieldRow('docstring', 'b'.repeat(2000)).includes('2,000 chars'));

// A mid-word cut reads as corruption rather than as a preview.
const words = ('alpha bravo charlie delta echo foxtrot golf hotel india juliet '
    + 'kilo lima mike november oscar papa quebec romeo sierra tango').repeat(2);
const preview = F.longFieldRow('docstring', words).match(/info-preview">([^<]*)…/)[1];
// The cut lands at a space in the source, so the preview is a whole run of
// words. A mid-word truncation reads as corruption rather than as a preview.
must('the preview is a prefix of the value', words.startsWith(preview));
must('the preview ends on a word, not mid-word',
    words[preview.length] === ' ' || preview.length === words.length);
must('the preview does not trail whitespace', preview === preview.trimEnd());
must('newlines are flattened out of the preview',
    !F.longFieldRow('docstring', 'alpha\nbeta '.repeat(40))
        .match(/info-preview">([^<]*)…/)[1].includes('\n'));

// ── the field rows are an innerHTML sink ──
must('a tag in a short value is escaped',
    !F.longFieldRow('docstring', '<img src=x onerror=alert(1)>').includes('<img src'));
must('a tag in a long value is escaped in both the preview and the full text',
    !F.longFieldRow('docstring', '<img src=x>'.repeat(60)).includes('<img src=x>'));

// ── chips ──
must('an empty name list renders nothing', F.chipRow('calls', []) === '');
must('a list of blanks renders nothing', F.chipRow('calls', [null, '', undefined]) === '');

const few = ['a', 'b', 'c'];
must('a short list stays inline', !F.chipRow('calls', few).includes('<details'));
must('a short list shows every name',
    few.every(n => F.chipRow('calls', few).includes('>' + n + '<')));

const many = Array.from({ length: F.CHIPS_INLINE_MAX + 1 }, (_, i) => 'fn' + i);
const atMax = many.slice(0, F.CHIPS_INLINE_MAX);
must('a list at the inline limit stays inline',
    !F.chipRow('calls', atMax).includes('<details'));
must('a list past the inline limit collapses',
    F.chipRow('calls', many).includes('<details'));
must('a collapsed chip list still holds every name',
    many.every(n => F.chipRow('calls', many).includes('>' + n + '<')));
must('a collapsed chip list counts itself',
    F.chipRow('calls', many).includes(String(many.length)));
// The count's noun comes from the field, so "calls" and "names" do not swap.
must('a collapsed call list counts calls', F.chipRow('calls', many).includes('calls</span>'));
must('a collapsed extends list counts names', F.chipRow('extends', many).includes('names</span>'));

F.known.set('resolved', { id: 'function:src/a.rs:resolved' });
const mixed = F.chipRow('calls', ['resolved', 'external']);
must('a name that resolves becomes a linked chip', mixed.includes('info-chip linked'));
must('a linked chip carries the node it goes to',
    mixed.includes('data-id="function:src/a.rs:resolved"'));
must('a name that does not resolve stays a plain chip',
    mixed.includes('class="info-chip"'));
must('an unresolved chip says why it is not a link',
    mixed.includes('Not indexed as its own node'));
must('a hostile chip name is escaped',
    !F.chipRow('calls', ['<img src=x>']).includes('<img src'));

// `escapeHtml` round-trips through a text node, which replaces `&`, `<` and
// `>` and leaves the quote alone. Every attribute it feeds is double-quoted,
// so a name carrying a quote would close the attribute early. Pinned as the
// guarantee it actually gives, so the next change to these rows knows which
// half it has: escaped as text, not safe as an attribute.
const quoted = F.chipRow('calls', ['a"b']);
must('a chip escapes the three text-node characters',
    !F.chipRow('calls', ['<x>&y']).includes('<x>')
    && F.chipRow('calls', ['<x>&y']).includes('&lt;x&gt;&amp;y'));
must('a quote in a name is NOT escaped — this is a known limit, not a pass',
    quoted.includes('a"b'));

// ── 4. Which chips link ──────────────────────────────────────────────────────

const nameMod = write('name.mjs', `
    const state = {};
    let probed = [];
    function queueNameProbe(name) { probed.push(name); }
    ${lift(src, 'findNodeByName')}
    export { findNodeByName, state };
    export const probes = () => probed;
    export const resetProbes = () => { probed = []; };
`);
const N = await import(nameMod);

console.log('resolving a chip name to a node');

N.state.nodeById = null;
must('no node map resolves nothing', N.findNodeByName('anything') === null);

// An exact id hit wins before any name scan — the chip text is often the id.
const byId = new Map([['function:src/a.rs:foo', { id: 'function:src/a.rs:foo' }]]);
N.state.nodeById = byId;
N.state.graph = { nodes: [] };
must('an exact id resolves straight away',
    N.findNodeByName('function:src/a.rs:foo').id === 'function:src/a.rs:foo');

// Local mode: a name index built once from the graph, full name and basename.
N.state.nodeById = new Map();
N.state._nameIndex = null;
N.state.nodeStore = null;
N.state.graph = { nodes: [
    { id: 'n1', name: 'src/auth/login.ts' },
    { id: 'n2', name: 'searchKb' },
    { id: 'n3', name: 'src/other/login.ts' },
] };
must('a full name resolves', N.findNodeByName('src/auth/login.ts').id === 'n1');
must('a bare name resolves', N.findNodeByName('searchKb').id === 'n2');
must('a basename resolves to the first node that claimed it',
    N.findNodeByName('login.ts').id === 'n1');
must('an unknown name resolves to nothing', N.findNodeByName('nope') === null);

// Server mode: the panel re-renders when probes land, so every probe must
// leave a memo — misses included — or the re-render queues the same name
// again and the page never settles.
N.state.nodeById = new Map([['n9', { id: 'n9' }]]);
N.state.nodeStore = {};
N.state._nameMemo = null;
N.resetProbes();
must('an unknown name in server mode renders unlinked for now',
    N.findNodeByName('later') === null);
must('an unknown name in server mode asks the server',
    JSON.stringify(N.probes()) === JSON.stringify(['later']));

N.state._nameMemo.set('later', null);
N.resetProbes();
must('a name the server said it does not have stays unresolved',
    N.findNodeByName('later') === null);
must('a memoised miss is not asked for twice — this is the render loop',
    N.probes().length === 0);

N.state._nameMemo.set('found', 'n9');
N.resetProbes();
must('a name the server resolved comes back as its node',
    N.findNodeByName('found').id === 'n9');
must('a memoised hit is not asked for again', N.probes().length === 0);

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failures === 0
    ? 'the info panel renders and resolves as specified'
    : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
