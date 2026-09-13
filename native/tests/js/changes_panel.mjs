// The Changes panel's checkable halves, run under `node`.
//
// The panel that launches a walk has two jobs that can be wrong silently,
// and both put repository content on screen:
//
//   1. `changeBadge` says *why* a stop is on the walk. A `caller` is
//      unchanged code that merely reaches the diff; if it renders like a
//      `changed` stop — or renders with no badge at all — the reader
//      concludes the diff edited it. That is the one misreading this whole
//      feature exists to prevent.
//   2. `renderDiffPreview` shows what is about to be walked. It writes file
//      paths and a commit subject into the DOM, so it is also the place a
//      repository with a hostile branch name would reach `innerHTML`.
//
// Both are lifted out of the shipped part with a string slice rather than
// transcribed, so this cannot pass against a copy that has drifted.
//
// Booting the real page to check this instead is a runaway CPU load — see
// Agents.md §10r.
//
// argv: <27-changes.js> <index.html>

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [changesPath, htmlPath] = process.argv.slice(2);
const src = fs.readFileSync(changesPath, 'utf8');
const html = fs.readFileSync(htmlPath, 'utf8');

let failures = 0;
const must = (label, cond) => {
    if (!cond) failures++;
    console.log((cond ? '  ok   ' : '  FAIL ') + label);
};

/// Lift one `function name(...) { ... }` out of a concatenated part. The
/// parts are indented to a fixed depth inside the page's module, so the
/// closing brace at that depth ends the function.
function lift(name) {
    const start = src.indexOf('\n        function ' + name + '(');
    if (start < 0) throw new Error(`${name} not found — has it been renamed?`);
    const end = src.indexOf('\n        }', start);
    if (end < 0) throw new Error(`${name} has no closing brace at part depth`);
    return src.slice(start, end + 10);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ug-changes-'));
const write = (name, body) => {
    const p = path.join(tmp, name);
    fs.writeFileSync(p, body);
    return p;
};

/// Enough of an element to record what was set on it, plus the handful of
/// DOM calls these two functions make.
const STUB_DOM = `
    const escapeHtml = t => String(t).replace(/[&<>"']/g, c =>
        ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
    function stubEl(tag) {
        return {
            tag, className: '', title: '', textContent: '', value: '',
            dataset: {}, children: [], hidden: false,
            set innerHTML(v) { this._html = v; if (v === '') this.children = []; },
            get innerHTML() {
                // What a reader would see: the assigned markup, plus the
                // text of everything appended afterwards.
                return (this._html || '') + this.children.map(c => c.innerHTML || c.textContent).join(' ');
            },
            appendChild(c) { this.children.push(c); return c; },
            append(...cs) { cs.forEach(c => this.children.push(c)); },
            classList: {
                _s: new Set(),
                add(c) { this._s.add(c); },
                toggle(c, on) { if (on) this._s.add(c); else this._s.delete(c); },
            },
        };
    }
    globalThis.document = { createElement: stubEl };
`;

// ── 1. Why a stop is on the walk ─────────────────────────────────────────────

const badgeMod = write('badge.mjs', `
    ${STUB_DOM}
    ${lift('changeBadge')}
    export { changeBadge };
`);
const B = await import(badgeMod);

console.log('labelling a walk stop');

must('a tour stop gets no badge at all', B.changeBadge(null) === null);
must('an absent change is not an empty pill', B.changeBadge(undefined) === null);

const changed = B.changeBadge({ role: 'changed', added: 12, removed: 3 });
must('a changed stop says so', changed.textContent === 'changed');
must('a changed stop is styled as changed', changed.className.includes('changed'));
must('a changed stop shows how much moved',
    changed.children.some(c => c.textContent === '+12/-3'));

const caller = B.changeBadge({ role: 'caller', added: 0, removed: 0 });
must('a caller is labelled a caller', caller.textContent === 'caller');
// The distinction the feature exists to make: a caller must not be able to
// read as edited code.
must('a caller is not styled as changed', !caller.className.includes('changed'));
must('a caller carries no line counts',
    caller.children.length === 0);
must('a caller says it is unchanged', caller.title.includes('Unchanged'));

const test = B.changeBadge({ role: 'test', added: 0, removed: 0 });
must('a test is labelled a test', test.className.includes('test'));
must('a test says it is unchanged', test.title.includes('Unchanged'));

// A role the server grows later must not silently render as "changed".
const unknown = B.changeBadge({ role: 'context', added: 0, removed: 0 });
must('an unrecognised role keeps its own name', unknown.textContent === 'context');
must('an unrecognised role is not styled as changed',
    !unknown.className.includes('changed'));

// ── 2. The commit picker's rows ──────────────────────────────────────────────

const labelMod = write('label.mjs', `
    ${lift('commitLabel')}
    export { commitLabel };
`);
const L = await import(labelMod);

console.log('listing a commit');

const commit = {
    short: 'a1b2c3d',
    subject: 'fix(cli): stop exiting from inside a command',
    relative: '2 hours ago',
    files: 6,
    insertions: 699,
    deletions: 161,
};
const row = L.commitLabel(commit);
must('a row names the hash', row.includes('a1b2c3d'));
must('a row names the subject', row.includes('stop exiting'));
must('a row says how long ago', row.includes('2 hours ago'));
must('a row says how big', row.includes('6f +699/-161'));

// A dropdown of 30 rows is only scannable if no row can run away with the
// width.
const long = L.commitLabel({ ...commit, subject: 'x'.repeat(200) });
must('a long subject is truncated', long.includes('…') && long.length < 120);

// A merge commit has no shortstat, so its counts are zero — printing
// "0f +0/-0" would read as a broken commit rather than a merge.
const merge = L.commitLabel({
    short: 'deadbee', subject: "Merge branch 'x'", relative: '3 days ago',
    files: 0, insertions: 0, deletions: 0,
});
must('a commit with no stats omits the size', !merge.includes('0f'));
must('a commit with no stats still lists', merge.includes('deadbee'));

// ── 3. The preview, and what it puts on screen ───────────────────────────────

const previewMod = write('preview.mjs', `
    ${STUB_DOM}
    const CHG_PREVIEW_FILES = ${(src.match(/const CHG_PREVIEW_FILES = (\d+);/) || [, '40'])[1]};
    const els = {};
    function chgEl(id) { return (els[id] = els[id] || stubEl('div')); }
    ${lift('renderDiffPreview')}
    export { renderDiffPreview, chgEl };
`);
const P = await import(previewMod);

console.log('previewing a diff');

const file = (p, status, added, removed) => ({ path: p, status, added, removed });
P.renderDiffPreview(
    {
        label: 'commit a1b2c3d — do a thing',
        files: [file('src/a.rs', 'modified', 4, 1), file('src/b.rs', 'added', 9, 0)],
        insertions: 13,
        deletions: 1,
        commits: [],
    },
    []
);
const stat = P.chgEl('chg-stat').innerHTML;
must('the preview names the change', stat.includes('commit a1b2c3d'));
must('the preview counts the files', stat.includes('2 files'));
must('the preview counts the lines', stat.includes('+13') && stat.includes('-1'));
const rows = P.chgEl('chg-files').children;
must('every changed file is listed', rows.length === 2);
must('a file is marked with its status',
    rows[1].children.some(c => c.className.includes('added')));

// An empty diff is a legitimate answer, and must read as one rather than as
// a failure or as a blank panel.
P.renderDiffPreview({ label: 'staged changes', files: [], insertions: 0, deletions: 0, commits: [] }, []);
must('an empty diff says it is empty',
    P.chgEl('chg-stat').innerHTML.includes('no changes'));
must('an empty diff lists no files', P.chgEl('chg-files').children.length === 0);

// A huge diff is summarised, not dumped into a scroll box that never ends.
const many = Array.from({ length: 120 }, (_, i) => file(`src/f${i}.rs`, 'modified', 1, 0));
P.renderDiffPreview({ label: 'branch', files: many, insertions: 120, deletions: 0, commits: [] }, []);
const listed = P.chgEl('chg-files').children;
must('a huge diff is capped', listed.length <= 41);
must('a huge diff says how much it hid',
    listed[listed.length - 1].textContent.includes('more'));

// A file that has moved on since the diff is flagged, because its stops are
// then approximate — silence there looks exactly like an exact answer.
P.renderDiffPreview(
    { label: 'old commit', files: [file('src/a.rs', 'modified', 1, 1)], insertions: 1, deletions: 1, commits: [] },
    ['src/a.rs']
);
const drifted = P.chgEl('chg-files').children[0];
must('a drifted file is marked', drifted.className.includes('drift'));
must('a drifted file explains itself', drifted.children[1].title.includes('approximate'));

// The label is a commit subject and the paths are repository content. Both
// reach the DOM, and neither may be parsed as markup.
P.renderDiffPreview(
    {
        label: '<img src=x onerror=alert(1)>',
        files: [file('<script>alert(1)</script>.rs', 'modified', 1, 0)],
        insertions: 1,
        deletions: 0,
        commits: [],
    },
    []
);
console.log('escaping repository content');
must('a hostile commit subject is escaped',
    !P.chgEl('chg-stat').innerHTML.includes('<img'));
must('a hostile path is set as text, not markup',
    P.chgEl('chg-files').children[0].children[1].children[0].textContent
        === '<script>alert(1)</script>.rs');

// ── 4. Every element the panel reaches for exists ────────────────────────────
//
// The panel is wired by id, and a typo in one is silent: `getElementById`
// returns null, the listener is never attached, and the control simply does
// nothing when clicked. Nothing throws, nothing logs, and a screenshot looks
// correct. Checking the two files against each other is the only cheap way
// to catch it.

console.log('reaching for elements that exist');

const idsInHtml = new Set([...html.matchAll(/id="([^"]+)"/g)].map(m => m[1]));
const idsUsed = new Set([
    ...[...src.matchAll(/chgEl\('([^']+)'\)/g)].map(m => m[1]),
    ...[...src.matchAll(/getElementById\('([^']+)'\)/g)].map(m => m[1]),
    ...[...src.matchAll(/tourEl\('([^']+)'\)/g)].map(m => m[1]),
    ...[...src.matchAll(/askEl\('([^']+)'\)/g)].map(m => m[1]),
]);
const missing = [...idsUsed].filter(id => !idsInHtml.has(id));
must(`every id the panel uses is in the page${missing.length ? ' — missing: ' + missing.join(', ') : ''}`,
    missing.length === 0);
// The check is only meaningful if it actually found the ids.
must('the id scan found the panel', idsUsed.has('chg-run') && idsUsed.has('chg-commit'));

// The panel is useless unless something wires it at load.
must('the panel is wired at startup',
    fs.readFileSync(path.join(path.dirname(changesPath), '03-insights.js'), 'utf8')
        .includes('wireChanges()'));

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failures === 0
    ? 'the changes panel labels and previews as specified'
    : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
