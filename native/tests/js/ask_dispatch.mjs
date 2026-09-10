// The Ask bar's checkable halves, run under `node`.
//
// Everything the page can be asked now enters through one input, and three
// things in `src/vis/js/25-ask.js` decide what happens to it:
//
//   1. `classifyAsk` turns raw text into a mode. Get it wrong and typing a
//      symbol name spends a model call, or typing a question quietly
//      searches for a node literally named "how does auth work".
//   2. `buildHitRow` / `askProvenanceHtml` turn a hit into the row the user
//      reads. That row carries repository content — names, file paths — into
//      `innerHTML`, and it carries the provenance that makes a result
//      checkable rather than merely present.
//   3. `askBlock` / `clearAskStream` decide what the column holds. Every mode
//      writes through them, and appending instead of replacing is how the
//      panel used to grow a block per click of the mode strip.
//
// The first two are pure; the third needs only enough of a DOM to hold a few
// nodes. All three are read out of the real part with a string slice rather
// than transcribed, so this cannot pass against a copy that has drifted from
// what ships.
//
// Booting the real page to check this instead is a runaway CPU load — see
// Agents.md §10r.
//
// argv: <25-ask.js>

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [askPath] = process.argv.slice(2);
const askSrc = fs.readFileSync(askPath, 'utf8');

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
function liftConst(src, decl, close) {
    const start = src.indexOf('\n        const ' + decl);
    if (start < 0) throw new Error(`${decl} not found`);
    const end = src.indexOf('\n        ' + close, start);
    return src.slice(start, end + close.length + 9);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-ask-'));
const write = (name, body) => {
    const p = path.join(tmp, name);
    fs.writeFileSync(p, body);
    return p;
};

// ── 1. What a query means ────────────────────────────────────────────────────

const IDENT_MAX = askSrc.match(/const ASK_IDENT_MAX = (\d+);/);
if (!IDENT_MAX) throw new Error('ASK_IDENT_MAX not found');

const classifyMod = write('classify.mjs', `
    const ASK_IDENT_MAX = ${IDENT_MAX[1]};
    ${lift(askSrc, 'classifyAsk')}
    export { classifyAsk };
`);
const C = await import(classifyMod);

console.log('classifying a query');
const mode = t => C.classifyAsk(t).mode;
const query = t => C.classifyAsk(t).query;

must('an empty bar has no mode', mode('') === null && mode('   ') === null);

// Identifiers, paths and qualified ids are name lookups. These are what the
// old keyword box existed for, and they must not cost a retrieval round trip.
must('a bare identifier is a name lookup', mode('runChatTurn') === 'names');
must('snake_case is a name lookup', mode('search_kb') === 'names');
must('a qualified id is a name lookup', mode('crate::storage::query::search_kb') === 'names');
must('a dotted name is a name lookup', mode('storage.query.searchKb') === 'names');
must('a file path is a name lookup', mode('src/vis/js/25-ask.js') === 'names');
must('a windows path is a name lookup', mode('src\\\\vis\\\\index.html') === 'names');
must('a scoped package is a name lookup', mode('@cosmos.gl/graph') === 'names');
must('a hyphenated name is a name lookup', mode('node-index') === 'names');

// Anything with a space in it is a question, and questions go to retrieval —
// not to the model, which costs tokens nobody asked to spend.
must('a phrase goes to retrieval', mode('oauth login flow') === 'find');
must('a question goes to retrieval', mode('how does authentication work?') === 'find');
must('two words go to retrieval', mode('edge store') === 'find');
must('a question never defaults to the model', mode('how does authentication work?') !== 'answer');

// A long unbroken string is prose someone forgot to space, not a symbol.
must('a very long unspaced string is not a name lookup',
    mode('a'.repeat(Number(IDENT_MAX[1]) + 1)) === 'find');
must('a name at the length limit is still a name lookup',
    mode('a'.repeat(Number(IDENT_MAX[1]))) === 'names');

// The three prefixes are explicit instructions and outrank the shape rule.
must('# forces a name lookup', mode('#oauth login flow') === 'names');
must('# strips its own prefix', query('#searchKb') === 'searchKb');
must('? opens the preset browser', mode('?dead code') === 'insights');
must('? strips its own prefix', query('?dead code') === 'dead code');
must('> opens the action list', mode('>reset') === 'action');
must('> strips its own prefix', query('>reset view') === 'reset view');
must('a prefix with nothing after it still classifies',
    mode('#') === 'names' && query('#') === '');

must('surrounding space never changes the mode', mode('  runChatTurn  ') === 'names');
must('surrounding space is trimmed off the query', query('  runChatTurn  ') === 'runChatTurn');

// ── 2. What a row says ───────────────────────────────────────────────────────

const rowMod = write('row.mjs', `
    const escapeHtml = t => String(t).replace(/[&<>"']/g, c =>
        ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
    const truncateName = n => String(n);
    const nodeIconSvg = g => '<svg class="node-icon" data-g="' + g + '"></svg>';
    ${liftConst(askSrc, 'ASK_MATCH_TIP = {', '};')}
    ${lift(askSrc, 'askProvenanceHtml')}

    // buildHitRow needs a DOM. Only the markup it produces is under test, so
    // the element is a shim that records what was set on it.
    const state = { nodeById: new Map() };
    function stubEl() {
        return {
            className: '', innerHTML: '', dataset: {},
            _listeners: [],
            addEventListener(_, fn) { this._listeners.push(fn); },
            querySelector() { return { title: '' }; },
        };
    }
    globalThis.document = { createElement: stubEl };
    ${lift(askSrc, 'buildHitRow')}
    export { buildHitRow, askProvenanceHtml };
`);
const R = await import(rowMod);

console.log('rendering a result row');
const row = h => R.buildHitRow(h).innerHTML;

const plain = row({ id: 'a::b', name: 'searchKb', node_type: 'Function', file: 'src/q.rs', start_line: 10, end_line: 42 });
must('a row names the hit', plain.includes('searchKb'));
must('a row carries its type icon', plain.includes('data-g="Function"'));
must('a row shows where it lives', plain.includes('src/q.rs'));
must('a line span reads as a range', plain.includes('L10–42'));
must('a single line does not read as a range',
    row({ id: 'x', name: 'n', start_line: 7, end_line: 7 }).includes('L7'));
must('a hit with no file still renders',
    row({ id: 'x', name: 'n' }).includes('ask-row-head'));
must('a nameless hit falls back to its id', row({ id: 'only-an-id' }).includes('only-an-id'));

console.log('escaping repository content');
// Names and paths come straight out of the indexed repository and go into
// innerHTML. A repo is allowed to contain a file called `<img onerror=…>`.
const nasty = row({
    id: 'x', name: '<img src=x onerror=alert(1)>',
    file: '"><script>alert(2)</script>', node_type: 'Function',
});
must('a hostile name is escaped', !nasty.includes('<img src=x'));
must('a hostile path is escaped', !nasty.includes('<script>'));
must('the escaped name is still shown', nasty.includes('&lt;img src=x'));
const nastyMech = R.askProvenanceHtml({ matched_by: '"><script>alert(3)</script>' });
must('a hostile mechanism flag is escaped', !nastyMech.includes('<script>'));

console.log('showing how a hit was reached');
// `matched_by`, `hop` and the score all travel on every hybrid item and on
// every chat citation. Dropping them turns an argument back into a list.
const sem = R.askProvenanceHtml({ matched_by: 'semantic', hop: 0, score: 0.1234 });
must('a dense match says so', sem.includes('>semantic<'));
must('a dense match is styled as one', sem.includes('ask-match-semantic'));
must('a dense match explains itself', sem.includes('Dense vector match'));
must('a score is shown to three places', sem.includes('0.123'));
must('a zero hop is not shown as a hop', !sem.includes('ask-hop'));

const walked = R.askProvenanceHtml({ matched_by: 'graph', hop: 2, score: 0.5 });
must('a walked hit says how far it walked', walked.includes('2 hops'));
must('one hop is singular', R.askProvenanceHtml({ hop: 1 }).includes('1 hop<'));
must('a keyword match is styled apart from a dense one',
    R.askProvenanceHtml({ matched_by: 'keyword' }).includes('ask-match-keyword'));

// A name lookup has no provenance to show — it matched because you typed it.
must('a name hit claims no provenance', R.askProvenanceHtml({ name: 'x' }) === '');
must('a missing score is not printed as NaN',
    !R.askProvenanceHtml({ matched_by: 'semantic', score: undefined }).includes('NaN'));
must('a non-finite score is not printed',
    !R.askProvenanceHtml({ score: Infinity }).includes('ask-score'));
// Pure-vector rows arrive with `distance` where hybrid rows carry `score`.
must('a distance is shown when there is no score',
    R.askProvenanceHtml({ distance: 0.25 }).includes('0.250'));

// ── 3. One question, one block ───────────────────────────────────────────────
//
// Names, Find, Answer and Tour all write into `#ask-stream`, and they used to
// append: walking the mode strip over a single question stacked four results
// and the column only ever got longer. `askBlock` now retires what the last
// question left, so the invariant worth pinning is that opening a block
// leaves exactly one behind.

const streamMod = write('stream.mjs', `
    // A DOM small enough to hold the stream: children, class names, and a
    // class-only querySelector, which is all these functions touch.
    function stubNode(tag) {
        const node = {
            tagName: tag, className: '', textContent: '', title: '', type: '',
            dataset: {}, children: [], _listeners: [],
            classList: { add() {}, remove() {}, toggle() {} },
            addEventListener(_, fn) { this._listeners.push(fn); },
            scrollIntoView() {},
            append(...kids) { this.children.push(...kids); },
            appendChild(kid) { this.children.push(kid); return kid; },
            remove() {},
            querySelector(sel) {
                const want = sel.slice(1);
                for (const kid of this.children) {
                    if (String(kid.className).split(' ').includes(want)) return kid;
                    const deep = kid.querySelector(sel);
                    if (deep) return deep;
                }
                return null;
            },
        };
        Object.defineProperty(node, 'innerHTML', {
            get() { return node._html || ''; },
            set(v) { node._html = v; if (!v) node.children = []; },
        });
        return node;
    }

    const stream = stubNode('div');
    stream.className = 'ask-stream';
    globalThis.document = {
        createElement: stubNode,
        getElementById: id => (id === 'ask-stream' ? stream : null),
    };

    // The canvas half of a cleared stream. Only that it was told matters here.
    const state = { askLive: null, askCursor: 3 };
    let matches = null;
    function setAskMatches(hits) { matches = hits; }
    const askEl = id => document.getElementById(id);

    ${lift(askSrc, 'askBlockHandle')}
    ${lift(askSrc, 'askBlock')}
    ${lift(askSrc, 'clearAskStream')}
    export { askBlock, clearAskStream, stream, state };
    export const seenMatches = () => matches;
`);
const S = await import(streamMod);

console.log('keeping one question in the stream');
S.askBlock('find', 'how does serve work');
must('the first question opens a block', S.stream.children.length === 1);

S.askBlock('answer', 'how does serve work');
must('switching mode does not stack a second block', S.stream.children.length === 1);
must('the block left standing is the newest',
    S.stream.children[0].className.includes('mode-answer'));

// The mode strip runs Names · Find · Answer · Tour over one question. Four
// clicks used to mean four blocks.
['names', 'find', 'answer', 'tour'].forEach(m => S.askBlock(m, 'one question'));
must('walking the whole strip leaves one result', S.stream.children.length === 1);

const head = S.stream.children[0].querySelector('.ask-block-head');
must('the block says which mode produced it',
    head.querySelector('.ask-block-mode').textContent === 'tour');
must('the block quotes the question verbatim',
    head.querySelector('.ask-block-q').textContent === 'one question');

// A question typed into the bar is user text; it reaches the head as text and
// must never be parsed as markup on the way.
S.askBlock('names', '<img src=x onerror=alert(1)>');
const q = S.stream.children[0].querySelector('.ask-block-q');
must('a hostile question stays text', q.textContent === '<img src=x onerror=alert(1)>');
must('a hostile question is not markup', !String(q.innerHTML).includes('<img'));

// Clearing is what makes the replacement safe: the transient names block, the
// row cursor and the set lit up on the canvas all belong to the old question.
S.state.askLive = S.stream.children[0];
S.state.askCursor = 2;
S.clearAskStream();
must('clearing empties the stream', S.stream.children.length === 0);
must('clearing forgets the live block', S.state.askLive === null);
must('clearing forgets the highlighted row', S.state.askCursor === -1);
must('clearing takes the old matches off the canvas',
    Array.isArray(S.seenMatches()) && S.seenMatches().length === 0);

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failures === 0
    ? 'the ask bar classifies and renders as specified'
    : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
