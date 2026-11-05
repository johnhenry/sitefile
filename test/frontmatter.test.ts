import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseFrontmatter } from '../src/frontmatter.js';
import { isSitefileError } from '../src/errors.js';

const fm = (t: string) => parseFrontmatter(t, 'post.md');

function fails(text: string, line: number, column: number, match?: RegExp): void {
  assert.throws(
    () => fm(text),
    (e: unknown) => {
      assert.ok(isSitefileError(e, 'E_FRONTMATTER'), String(e));
      assert.equal(e.span?.file, 'post.md');
      assert.equal(e.span?.line, line, e.message);
      assert.equal(e.span?.column, column, e.message);
      if (match) assert.match(e.detail, match);
      return true;
    },
  );
}

test('no frontmatter returns the text untouched', () => {
  const r = fm('# Hello\n---\nx\n');
  assert.deepEqual(r.data, {});
  assert.equal(r.body, '# Hello\n---\nx\n');
  assert.equal(r.bodyLine, 1);
});

test('empty input', () => {
  assert.deepEqual(fm(''), { data: {}, body: '', bodyLine: 1 });
});

test('--- not on its own line is not frontmatter', () => {
  assert.equal(fm('--- x\na: 1\n---\n').bodyLine, 1);
  assert.equal(fm('\n---\na: 1\n---\n').bodyLine, 1);
});

test('basic scalars and body line', () => {
  const r = fm('---\ntitle: Hello World\ncount: 3\nratio: -1.5\ndraft: false\npub: true\nnone: null\ntilde: ~\nempty:\n---\nBody\n');
  assert.deepEqual(r.data, { title: 'Hello World', count: 3, ratio: -1.5, draft: false, pub: true, none: null, tilde: null, empty: null });
  assert.equal(r.body, 'Body\n');
  assert.equal(r.bodyLine, 11);
});

test('dates stay strings; numbers with text stay strings', () => {
  const r = fm('---\ndate: 2026-01-02\nv: 1.2.3\nn: 12abc\n---\n');
  assert.deepEqual(r.data, { date: '2026-01-02', v: '1.2.3', n: '12abc' });
  assert.equal(typeof r.data.date, 'string');
});

test('quoted strings and escapes', () => {
  const r = fm(`---\na: "x \\"y\\" \\\\ \\n\\t z"\nb: 'it''s'\nc: "true"\nd: '42'\ne: "a # b"\n---\n`);
  assert.deepEqual(r.data, { a: 'x "y" \\ \n\t z', b: "it's", c: 'true', d: '42', e: 'a # b' });
});

test('comments', () => {
  const r = fm('---\n# a comment\n\ntitle: Hi # trailing\nurl: http://x.com/#frag\nq: "v" # note\n---\n');
  assert.deepEqual(r.data, { title: 'Hi', url: 'http://x.com/#frag', q: 'v' });
});

test('inline lists', () => {
  const r = fm('---\ntags: [a, "b c", 3, true, \'x\', null]\nnone: []\ntrail: [a, b,]\n---\n');
  assert.deepEqual(r.data, { tags: ['a', 'b c', 3, true, 'x', null], none: [], trail: ['a', 'b'] });
});

test('block lists', () => {
  const r = fm('---\ntags:\n  - one\n  - "two words"\n  # c\n  - 3\nnext: x\nflat:\n- a\n- b\n---\n');
  assert.deepEqual(r.data, { tags: ['one', 'two words', 3], next: 'x', flat: ['a', 'b'] });
});

test('CRLF and BOM', () => {
  const r = fm('\ufeff---\r\ntitle: Hi\r\ntags: [a]\r\n---\r\nBody\r\nmore');
  assert.deepEqual(r.data, { title: 'Hi', tags: ['a'] });
  assert.equal(r.body, 'Body\r\nmore');
  assert.equal(r.bodyLine, 5);
});

test('closing with ... and empty frontmatter, no trailing newline', () => {
  assert.deepEqual(fm('---\na: 1\n...\nB'), { data: { a: 1 }, body: 'B', bodyLine: 4 });
  assert.deepEqual(fm('---\n---'), { data: {}, body: '', bodyLine: 3 });
  assert.deepEqual(fm('---\n---\n'), { data: {}, body: '', bodyLine: 3 });
});

test('body may contain --- later', () => {
  const r = fm('---\na: 1\n---\nx\n---\ny');
  assert.equal(r.body, 'x\n---\ny');
});

test('keys with dashes and underscores; value containing colon without space', () => {
  const r = fm('---\n_a-b_1: http://x.com:80/p\n---\n');
  assert.deepEqual(r.data, { '_a-b_1': 'http://x.com:80/p' });
});

test('__proto__ key is data, not a prototype write', () => {
  const r = fm('---\n__proto__: x\n---\n');
  assert.equal(Object.getPrototypeOf(r.data), Object.prototype);
  assert.deepEqual(Object.keys(r.data), ['__proto__']);
  assert.equal(r.data['__proto__'], 'x');
});

test('unterminated frontmatter', () => {
  fails('---\na: 1\n', 1, 1, /unterminated/);
});

test('duplicate keys', () => {
  fails('---\na: 1\nb: 2\na: 3\n---\n', 4, 1, /duplicate key 'a'/);
});

test('nested maps, flow maps, block scalars and nested lists parse', () => {
  assert.deepEqual(fm('---\na:\n  b: 1\n  c: [x, {y: 2}]\n---\n').data, { a: { b: 1, c: ['x', { y: 2 }] } });
  assert.deepEqual(fm('---\na: {b: 1}\nl:\n  - [1]\n  - n: 1\n    m: 2\nt: [1, [2]]\n---\n').data, {
    a: { b: 1 },
    l: [[1], { n: 1, m: 2 }],
    t: [1, [2]],
  });
  assert.deepEqual(fm('---\na: |\n  x\n  y\nb: >-\n  p\n  q\n1bad: x\n---\n').data, { a: 'x\ny\n', b: 'p q', '1bad': 'x' });
});

test('errors inside nested structures carry file line numbers', () => {
  fails('---\na:\n  b: 1\n  b: 2\n---\n', 4, 3, /duplicate key 'b'/);
  fails('---\na:\n\tb: 1\n---\n', 3, 1, /tab/);
});

test('indentation and nested maps on one line', () => {
  fails('---\na: b: c\n---\n', 2, 4, /nested maps/);
  fails('---\n  x: 1\n---\n', 2, 3, /indentation/);
});

test('anchors and tags', () => {
  fails('---\na: &x 1\n---\n', 2, 4, /anchors/);
  fails('---\na: *x\n---\n', 2, 4, /anchors/);
});

test('invalid lines', () => {
  fails('---\njust text\n---\n', 2, 1, /key: value/);
  fails('---\n- orphan\n---\n', 2, 1, /without a preceding key/);
  fails('---\na: x\n- y\n---\n', 3, 1, /without a preceding key|list/);
});

test('bad quotes and lists', () => {
  fails('---\na: "open\n---\n', 2, 4, /unterminated quoted/);
  fails('---\na: "x" y\n---\n', 2, 8, /unexpected text/);
  fails('---\na: "bad \\q"\n---\n', 2, 9, /escape/);
  fails('---\na: [1, 2\n---\n', 2, 4, /unterminated list/);
  fails('---\na: [1,,2]\n---\n', 2, 7, /empty list item/);
  fails('---\na: [1] x\n---\n', 2, 8, /unexpected text/);
});
