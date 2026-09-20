// The Answer tab's markdown, run under `node`.
//
// An answer arrives as markdown and is read as HTML, and two parts of
// `src/vis/js/06-chat.js` stand between them:
//
//   1. `renderMarkdown` — the parser. Everything it does not understand
//      reaches the reader as punctuation: a comparison table becomes a wall
//      of pipes, a nested bullet restarts the list at the top. It also puts
//      *model output* through `innerHTML`, which is untrusted by
//      construction, so every construct it grew has to escape.
//   2. `stableEnd` — where a half-written answer may be frozen. Freeze
//      inside a fence or in the middle of a list and the streamed render
//      disagrees with the finished one; freeze nowhere and the whole answer
//      is re-parsed on every token.
//
// Both are lifted out of the shipped part with a string slice, so this
// cannot pass against a copy that has drifted from what ships.
//
// Booting the real page to check this instead is a runaway CPU load — see
// Agents.md §10r.
//
// argv: <06-chat.js>

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [chatPath] = process.argv.slice(2);
const chatSrc = fs.readFileSync(chatPath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

// Lift one `function name(...) { ... }` out of a concatenated part. The parts
// are indented to a fixed depth inside the page's module, so the closing brace
// at that depth ends the function.
function lift(src, name) {
    const start = src.indexOf('\n        function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found — has it been renamed?`);
    const end = src.indexOf('\n        }', start);
    if (end < 0) throw new Error(`${name} has no closing brace at part depth`);
    return src.slice(start, end + 10);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-md-'));
const mod = path.join(tmp, 'md.mjs');
fs.writeFileSync(mod, `
    // The page's escapeHtml goes through a detached element; this is the same
    // set of characters without a DOM.
    function escapeHtml(text) {
        if (text == null) return '';
        return String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    }
    ${lift(chatSrc, 'renderMarkdown')}
    ${lift(chatSrc, 'stableEnd')}
    export { renderMarkdown, stableEnd };
`);
const M = await import(mod);
const md = M.renderMarkdown;

// ── 1. What the reader sees ──────────────────────────────────────────────────

console.log('rendering an answer');

must('a paragraph is a paragraph', md('hello there') === '<p>hello there</p>');
must('a heading keeps its level', md('### Why').includes('<h3 class="md-h">Why</h3>'));
must('emphasis renders', md('**bold** and *thin*')
    === '<p><strong>bold</strong> and <em>thin</em></p>');
must('inline code renders', md('call `searchKb` here').includes('<code>searchKb</code>'));

const fence = md('before\n\n```rust\nlet x = 1;\n```\n\nafter');
must('a fence becomes a code block', fence.includes('<pre class="md-code" data-lang="rust">'));
must('a fence keeps its body', fence.includes('let x = 1;'));
must('a fence is not re-parsed', md('```\n**not bold**\n```').includes('**not bold**'));

// ── 2. Tables ────────────────────────────────────────────────────────────────
//
// Models answer comparisons with tables. Before the parser knew them, every
// one of those arrived as a wall of pipes and dashes.

console.log('rendering a table');

const table = md('| Node | Count |\n| --- | ---: |\n| File | 12 |\n| Fn | 340 |');
must('a pipe table becomes a table', table.includes('<table class="md-table">'));
must('the row above the dashes is the header', table.includes('<th>Node</th>'));
must('the body rows are rows',
    table.includes('<td>File</td>') && table.includes('>340</td>'));
must('a right-aligned column says so', table.includes('<th class="md-end">Count</th>'));
must('a table can scroll on its own', table.includes('md-tablewrap'));
must('a table ends where it ends',
    md('| a | b |\n| - | - |\n| 1 | 2 |\n\nafter').includes('<p>after</p>'));
must('a centred column says so',
    md('| a |\n| :-: |\n| 1 |').includes('<th class="md-mid">a</th>'));
// A sentence with a pipe in it is a sentence, not a one-row table.
must('prose with a pipe is still prose',
    md('run `a | b` to pipe').startsWith('<p>') && !md('run a | b to pipe').includes('<table'));
must('a bullet list is not a delimiter row',
    !md('- one | two\n- three').includes('<table'));

// ── 3. Lists ─────────────────────────────────────────────────────────────────

console.log('rendering a list');

const nested = md('- outer\n  - inner\n- back');
must('a nested bullet nests', /<li>outer<ul class="md-list"><li>inner<\/li><\/ul><\/li>/.test(nested));
must('a nested list returns to its parent', nested.endsWith('<li>back</li></ul>'));
must('an ordered list is ordered', md('1. one\n2. two').startsWith('<ol class="md-list">'));
must('switching marker switches list',
    md('1. one\n\n- two').includes('</ol><ul class="md-list">'));

// A blank line between items is a loose list, not two lists — and two <ol>s
// is a list that silently restarts at 1.
const loose = md('1. one\n\n2. two\n\n3. three');
must('a blank line between items keeps one list',
    (loose.match(/<ol/g) || []).length === 1);
must('a paragraph after a list ends the list',
    md('- one\n\nplain').includes('</ul><p>plain</p>'));

const task = md('- [x] done\n- [ ] todo');
must('a checked task is checked', task.includes('<span class="md-box on"></span>done'));
must('an unchecked task is not', task.includes('<span class="md-box"></span>todo'));
must('a task item drops its bullet', task.includes('<li class="md-check">'));

// A wrapped item and an indented fence both belong to the item above them.
must('an indented line continues its item',
    md('- one\n  and more').includes('<li>one and more</li>'));
const itemFence = md('- one\n  ```\n  x\n  ```');
must('a fence indented under an item stays in the item',
    itemFence.includes('<li>one<pre class="md-code">'));
// The indent in front of the fence is the item's, not the code's.
must('an indented fence is not indented code',
    itemFence.includes('<code>x</code>'));
must('a fence at the margin ends the list',
    md('- one\n```\nx\n```').includes('</ul><pre class="md-code">'));

// ── 4. Model output is untrusted ─────────────────────────────────────────────
//
// Every construct the parser grew is another path from model text into
// innerHTML. A table cell and a list item are not safer than a paragraph.

console.log('escaping model output');

const hostile = '<img src=x onerror=alert(1)>';
must('a paragraph escapes markup', !md(hostile).includes('<img'));
must('a table cell escapes markup',
    !md(`| a |\n| - |\n| ${hostile} |`).includes('<img'));
must('a table header escapes markup',
    !md(`| ${hostile} |\n| - |\n| x |`).includes('<img'));
must('a list item escapes markup', !md(`- ${hostile}`).includes('<img'));
must('a task item escapes markup', !md(`- [ ] ${hostile}`).includes('<img'));
must('an item continuation escapes markup',
    !md(`- one\n  ${hostile}`).includes('<img'));
must('a fence escapes markup', !md('```\n' + hostile + '\n```').includes('<img'));
// Only http(s) links are emitted; a javascript: URL must stay text.
must('a javascript: link is not a link',
    !md('[click](javascript:alert(1))').includes('<a '));

must('a citation marker becomes a chip',
    md('see [#3] for this').includes('<span class="md-cite" data-cite="3">[#3]</span>'));

// ── 5. Where a half-written answer may be frozen ──────────────────────────────
//
// The streamed render freezes finished blocks and re-parses only the tail.
// Freezing in the wrong place is not a slow render, it is a wrong one.

console.log('freezing a streaming answer');

const end = (t, from = 0) => M.stableEnd(t, from);

must('nothing is frozen before the first blank line', end('a partial sent') === 0);
// A blank line only ends a block once the line after it says so, so the
// freeze runs one line behind the stream — never ahead of it.
must('a paragraph is not frozen until something follows it', end('one\n\ntwo') === 0);
must('a finished paragraph is frozen', end('one\n\ntwo\n') === 5);
must('an open fence freezes nothing', end('```\ncode\n\nmore\n') === 0);
must('a closed fence can be frozen', end('```\nx\n```\n\nafter\n') > 0);
must('a list is not frozen mid-way', end('- one\n\n- two\n') === 0);
must('a list is frozen once something else follows',
    end('- one\n\n- two\n\nplain\n') > 0);
must('freezing starts where it left off', end('one\n\ntwo\n\nthree\n', 5) === 10);

// The invariant behind the whole scheme: streaming an answer through the
// freeze loop must land on exactly what rendering it whole produces.
const DOCS = [
    'Short answer.',
    '# Head\n\nA paragraph with `code` and [#1].\n\n- one\n- two\n\nTail line.',
    'Intro\n\n```rust\nfn main() {\n\n    let x = 1;\n}\n```\n\nDone.',
    'Before\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\nAfter\n\n1. one\n\n2. two',
    '- outer\n  - inner\n\n- back\n\nplain paragraph\n\n## End',
];
let drift = 0;
for (const doc of DOCS) {
    let frozen = 0, html = '';
    for (let n = 1; n <= doc.length; n++) {
        const seen = doc.slice(0, n);
        const cut = M.stableEnd(seen, frozen);
        if (cut > frozen) { html += md(seen.slice(frozen, cut)); frozen = cut; }
    }
    if (html + md(doc.slice(frozen)) !== md(doc)) drift++;
}
must('streaming a doc renders what rendering it whole renders', drift === 0);

// ── 6. What the turn cost ────────────────────────────────────────────────────
//
// Three unrelated quantities share this box — what the question put in front
// of the model, what the endpoint charged, and what the same evidence costs
// read whole. The provider's raw total used to sit unlabelled in the meta
// strip as `tokens=90168`, beside a `~90,108` baseline it had nothing to do
// with; two numbers that look identical and mean different things.

console.log('reporting what the turn cost');

const costMod = path.join(tmp, 'cost.mjs');
fs.writeFileSync(costMod, `
    function escapeHtml(t) { return String(t == null ? '' : t); }
    function stubNode(tag) {
        return {
            tagName: tag, className: '', textContent: '', innerHTML: '', children: [],
            appendChild(k) { this.children.push(k); return k; },
        };
    }
    const document = { createElement: stubNode };
    ${lift(chatSrc, 'buildCostBox')}
    export { buildCostBox };
`);
const B = await import(costMod);

const flat = (node) => [node, ...node.children.flatMap(flat)];
const render = (done) => flat(B.buildCostBox(done));

const DONE = {
    cost: {
        context_tokens: 10256, tool_tokens: 16935, answer_tokens: 1619,
        sent_tokens: 27191, whole_files: 9, whole_file_tokens: 90108,
        saved_ratio: 3.3,
    },
    usage: { total_tokens: 90168 },
    tool_rounds: 3,
};

const nodes = render(DONE);
const text = nodes.map(n => n.textContent + ' ' + n.innerHTML).join('\n');
const rowFor = (label) => nodes.find(n => String(n.innerHTML).includes(label));

must('the parts of the question are listed', ['Retrieved pack', 'Tool results', 'Answer']
    .every(l => rowFor(l)));
must('the numbers are grouped for reading', text.includes('10,256') && text.includes('90,168'));

// The fix: the provider's total is labelled and inside the box.
must("the endpoint's own total is labelled", !!rowFor('Billed by the model'));
must('the billed figure is exact, not marked with ~',
    text.includes('>90,168<') && !text.includes('~90,168'));
must('why billed exceeds the evidence is explained',
    text.includes('re-sent') && text.includes('3 rounds'));

// …and the baseline is not one of the parts, nor part of the bill.
const baseline = rowFor('cited file');
must('the baseline names its file count', !!baseline && baseline.innerHTML.includes('9 cited files'));
must('the baseline is set apart', !!baseline && baseline.className.includes('baseline'));
must('the baseline says it was not spent',
    text.includes('not something this turn spent'));
must('estimates are marked as estimates', text.includes('~10,256') && text.includes('~90,108'));
must('the estimate caveat is present', text.includes('no tokenizer'));

// A turn with no tools and no resolvable files must not invent either row.
const bare = render({ cost: { context_tokens: 10, tool_tokens: 0, answer_tokens: 5, sent_tokens: 15, whole_files: 0 } });
const bareText = bare.map(n => n.textContent + ' ' + n.innerHTML).join('\n');
must('no billed row without a provider count', !bareText.includes('Billed by the model'));
must('no comparison without a measured file', !bareText.includes('read whole'));

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failures === 0
    ? 'the answer renders markdown and reports its cost as specified'
    : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
