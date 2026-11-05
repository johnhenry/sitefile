import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseYaml } from '../src/yaml.js';
import { isSitefileError } from '../src/errors.js';

const y = (t: string) => parseYaml(t, 'data.yml');

function fails(text: string, line: number, column: number, match?: RegExp, firstLine = 1): void {
  assert.throws(
    () => parseYaml(text, 'data.yml', firstLine),
    (e: unknown) => {
      assert.ok(isSitefileError(e, 'E_DATA'), String(e));
      assert.equal(e.span?.file, 'data.yml');
      assert.equal(e.span?.line, line, e.message);
      assert.equal(e.span?.column, column, e.message);
      if (match) assert.match(e.detail, match);
      return true;
    },
  );
}

test('empty documents are null', () => {
  assert.equal(y(''), null);
  assert.equal(y('\n\n'), null);
  assert.equal(y('# only a comment\n'), null);
  assert.equal(y('---\n'), null);
  assert.equal(y('---\n...\n'), null);
});

test('every scalar type', () => {
  assert.deepEqual(
    y('s: Hello World\ni: 3\nneg: -7\nd: -1.5\nt: true\nf: false\nn: null\ntilde: ~\nempty:\nstr: 12abc\ndate: 2026-01-02\nver: 1.2.3\nq: "true"\nq2: \'42\'\n'),
    { s: 'Hello World', i: 3, neg: -7, d: -1.5, t: true, f: false, n: null, tilde: null, empty: null, str: '12abc', date: '2026-01-02', ver: '1.2.3', q: 'true', q2: '42' },
  );
});

test('quoting and escapes', () => {
  assert.deepEqual(y(`a: "x \\"y\\" \\\\ \\n\\t z"\nb: 'it''s'\ne: "a # b"\nf: 'a: b'\n`), {
    a: 'x "y" \\ \n\t z',
    b: "it's",
    e: 'a # b',
    f: 'a: b',
  });
  assert.deepEqual(y('"quoted key": 1\n\'single\': 2\n"a: b": 3\n'), { 'quoted key': 1, single: 2, 'a: b': 3 });
});

test('top-level scalars', () => {
  assert.equal(y('hello'), 'hello');
  assert.equal(y('42'), 42);
  assert.equal(y('"x"'), 'x');
  assert.deepEqual(y('[1, 2]'), [1, 2]);
});

test('comments and blank lines', () => {
  assert.deepEqual(y('# top\n\na: 1 # trailing\nb: http://x.com/#frag\n\n  # indented comment\nc: "v" # note\n'), {
    a: 1,
    b: 'http://x.com/#frag',
    c: 'v',
  });
});

test('nested mappings', () => {
  assert.deepEqual(y('site:\n  name: Demo\n  author:\n    first: A\n    last: B\n  tags: x\nother: 1\n'), {
    site: { name: 'Demo', author: { first: 'A', last: 'B' }, tags: 'x' },
    other: 1,
  });
});

test('sequences, including nested and indentless', () => {
  assert.deepEqual(y('- a\n- 2\n- "c d"\n-\n- ~\n'), ['a', 2, 'c d', null, null]);
  assert.deepEqual(y('tags:\n  - one\n  # c\n  - two\nflat:\n- a\n- b\nnext: x\n'), {
    tags: ['one', 'two'],
    flat: ['a', 'b'],
    next: 'x',
  });
  assert.deepEqual(y('- - 1\n  - 2\n- - 3\n'), [[1, 2], [3]]);
  assert.deepEqual(y('-\n  - 1\n  - 2\n'), [[1, 2]]);
});

test('sequences of mappings', () => {
  const doc = 'nav:\n  - name: Home\n    url: /\n  - name: About\n    url: /about\n    sub:\n      - name: Team\n        url: /team\n  -\n    name: Last\n';
  assert.deepEqual(y(doc), {
    nav: [
      { name: 'Home', url: '/' },
      { name: 'About', url: '/about', sub: [{ name: 'Team', url: '/team' }] },
      { name: 'Last' },
    ],
  });
  assert.deepEqual(y('- a: 1\n  b:\n    - x\n    - y\n- c: 2\n'), [{ a: 1, b: ['x', 'y'] }, { c: 2 }]);
});

test('mapping of sequences of sequences', () => {
  assert.deepEqual(y('m:\n  - - a\n    - b\n  - - c\n'), { m: [['a', 'b'], ['c']] });
});

test('flow collections', () => {
  assert.deepEqual(y('a: [a, "b c", 3, true, \'x\', null]\nb: []\nc: [a, b,]\nd: {}\n'), {
    a: ['a', 'b c', 3, true, 'x', null],
    b: [],
    c: ['a', 'b'],
    d: {},
  });
  assert.deepEqual(y('a: {x: 1, y: [2, 3], z: {k: v}}\nb: [1, [2, [3]], {a: b}]\n'), {
    a: { x: 1, y: [2, 3], z: { k: 'v' } },
    b: [1, [2, [3]], { a: 'b' }],
  });
  assert.deepEqual(y('a: {"q k": 1, \'s\': "v,}", e: , n: ~}  # c\n'), { a: { 'q k': 1, s: 'v,}', e: null, n: null } });
  assert.deepEqual(y('- [1, 2]\n- {a: 1}\n'), [[1, 2], { a: 1 }]);
  assert.deepEqual(y('u: [http://x.com/a, 1]\n'), { u: ['http://x.com/a', 1] });
});

test('block scalars: literal', () => {
  assert.deepEqual(y('a: |\n  line1\n  line2\n\n  line4\nb: next\n'), { a: 'line1\nline2\n\nline4\n', b: 'next' });
  assert.deepEqual(y('a: |\n  x\n    indented\n  y\n'), { a: 'x\n  indented\ny\n' });
  assert.deepEqual(y('a: |   # note\n  hi\n'), { a: 'hi\n' });
});

test('block scalars: chomping', () => {
  assert.deepEqual(y('a: |-\n  x\n  y\n\n\nb: 1\n'), { a: 'x\ny', b: 1 });
  assert.deepEqual(y('a: |\n  x\n\n\nb: 1\n'), { a: 'x\n', b: 1 });
  assert.deepEqual(y('a: |+\n  x\n\n\nb: 1\n'), { a: 'x\n\n\n', b: 1 });
  assert.deepEqual(y('a: >-\n  x\n  y\nb: >+\n  p\n\nc: 1\n'), { a: 'x y', b: 'p\n\n', c: 1 });
  assert.deepEqual(y('a: |\nb: >-\nc: 1\n'), { a: '', b: '', c: 1 });
});

test('block scalars: folded', () => {
  assert.deepEqual(y('a: >\n  one\n  two\n\n  three\n    more\n  four\n'), { a: 'one two\nthree\n  more\nfour\n' });
  assert.deepEqual(y('a: >\n  p\n\n\n  q\n'), { a: 'p\n\nq\n' });
});

test('block scalars in sequences, nested maps and at top level', () => {
  assert.deepEqual(y('- |\n  a\n  b\n- >-\n  c\n  d\n- k: |\n    deep\n  j: 1\n'), ['a\nb\n', 'c d', { k: 'deep\n', j: 1 }]);
  assert.deepEqual(y('o:\n  body: |\n    # not a comment\n    key: not a key\n  after: 1\n'), {
    o: { body: '# not a comment\nkey: not a key\n', after: 1 },
  });
  assert.equal(y('|\n  top\n'), 'top\n');
});

test('document markers', () => {
  assert.deepEqual(y('---\na: 1\n'), { a: 1 });
  assert.deepEqual(y('# c\n---\na: 1\n...\n'), { a: 1 });
  assert.deepEqual(y('a: 1\n...\n# trailing comment\n\n'), { a: 1 });
  assert.deepEqual(y('---\r\na: 1\r\nb: [x]\r\n'), { a: 1, b: ['x'] });
  assert.deepEqual(y('\ufeffa: 1'), { a: 1 });
});

test('key forms', () => {
  assert.deepEqual(y('1: a\ntrue: b\n"null": c\nurl: http://x.com:80/p\n'), { 1: 'a', true: 'b', null: 'c', url: 'http://x.com:80/p' });
  assert.deepEqual(y('key with spaces: v\n'), { 'key with spaces': 'v' });
});

test('prototype safety', () => {
  const r = y('__proto__: x\nnested:\n  __proto__:\n    polluted: true\n') as Record<string, unknown>;
  assert.equal(Object.getPrototypeOf(r), Object.prototype);
  assert.deepEqual(Object.keys(r), ['__proto__', 'nested']);
  assert.equal(r['__proto__'], 'x');
  const n = r.nested as Record<string, unknown>;
  assert.equal(Object.getPrototypeOf(n), Object.prototype);
  assert.deepEqual(n['__proto__'], { polluted: true });
  assert.equal(({} as Record<string, unknown>).polluted, undefined);
  const f = y('a: {__proto__: 1, b: 2}') as { a: Record<string, unknown> };
  assert.deepEqual(Object.keys(f.a), ['__proto__', 'b']);
  assert.equal(Object.getPrototypeOf(f.a), Object.prototype);
});

test('duplicate keys', () => {
  fails('a: 1\nb: 2\na: 3\n', 3, 1, /duplicate key 'a'/);
  fails('x:\n  a: 1\n  a: 2\n', 3, 3, /duplicate key 'a'/);
  fails('a: {b: 1, b: 2}\n', 1, 11, /duplicate key 'b'/);
  fails('- k: 1\n  k: 2\n', 2, 3, /duplicate/);
});

test('tabs in indentation', () => {
  fails('a:\n\tb: 1\n', 2, 1, /tab/);
  fails('a:\n  b: 1\n \tc: 2\n', 3, 2, /tab/);
  assert.deepEqual(y('a:\t1\n'), { a: 1 });
});

test('indentation errors', () => {
  fails('a: 1\n  b: 2\n', 2, 3, /indentation/);
  fails('a:\n    b: 1\n  c: 2\n', 3, 3, /indentation/);
  fails('- a\n  - b\n', 2, 3, /indentation|content/);
  fails('a: 1\n- b\n', 2, 1, /without a preceding key/);
  fails('a: 1\nplain\n', 2, 1, /key: value/);
  fails('a: |\n    x\n  y\n', 3, 3, /block scalar/);
});

test('unsupported features', () => {
  fails('a: &x 1\n', 1, 4, /anchors/);
  fails('a: *x\n', 1, 4, /anchors/);
  fails('a: !!str 1\n', 1, 4, /tags/);
  fails('&x a: 1\n', 1, 1, /anchors/);
  fails('a: [&x 1]\n', 1, 5, /anchors/);
  fails('<<: {a: 1}\n', 1, 1, /merge/);
  fails('a: {<<: 1}\n', 1, 5, /merge/);
  fails('? a\n: b\n', 1, 1, /complex/);
  fails('a: 1\n---\nb: 2\n', 2, 1, /multiple documents/);
  fails('---\na: 1\n---\nb: 2\n', 3, 1, /multiple documents/);
  fails('a: 1\n...\nb: 2\n', 3, 1, /multiple documents/);
  fails('--- text\n', 1, 5, /marker/);
  fails('a: b: c\n', 1, 4, /nested maps/);
  fails('a: - b\n', 1, 4, /list item/);
  fails('a: |2\n  x\n', 1, 4, /indentation indicators/);
  fails('a: | junk\n  x\n', 1, 4, /block scalar header/);
  fails('a: [a: 1]\n', 1, 5, /unsupported list item/);
});

test('scalar and flow errors', () => {
  fails('a: "open\n', 1, 4, /unterminated quoted/);
  fails('a: "x" y\n', 1, 8, /unexpected text/);
  fails('a: "bad \\q"\n', 1, 9, /escape/);
  fails('a: [1, 2\n', 1, 4, /unterminated list/);
  fails('a: [1, [2]\n', 1, 4, /unterminated list/);
  fails('a: {x: 1\n', 1, 4, /unterminated map/);
  fails('a: [1,,2]\n', 1, 7, /empty list item/);
  fails('a: {x}\n', 1, 5, /':'/);
  fails('a: [1] x\n', 1, 8, /unexpected text/);
  assert.deepEqual(y('a: [1 2, x y]'), { a: ['1 2', 'x y'] });
});

test('depth cap', () => {
  const ok = '['.repeat(60) + ']'.repeat(60);
  assert.ok(Array.isArray((y(`a: ${ok}`) as { a: unknown }).a));
  fails(`a: ${'['.repeat(70)}${']'.repeat(70)}\n`, 1, 4 + 63, /deeper than 64/);
  const deepBlock = Array.from({ length: 70 }, (_, i) => `${'  '.repeat(i)}k${i}:`).join('\n') + ' 1\n';
  assert.throws(() => y(deepBlock), (e: unknown) => isSitefileError(e, 'E_DATA') && /deeper than 64/.test(e.detail));
  const shallow = Array.from({ length: 30 }, (_, i) => `${'  '.repeat(i)}k${i}:`).join('\n') + ' 1\n';
  assert.ok(y(shallow));
  assert.throws(() => y('- '.repeat(70) + 'x'), (e: unknown) => isSitefileError(e, 'E_DATA'));
});

test('firstLine offsets error lines', () => {
  fails('a: 1\nb: [1\n', 11, 4, /unterminated list/, 10);
  fails('a: 1\na: 2\n', 6, 1, /duplicate/, 5);
  fails('a:\n\tb\n', 3, 1, /tab/, 2);
  fails('a: |\n   x\n  y\n', 12, 3, /block scalar/, 10);
  fails('x:\n  - k: 1\n    k: 2\n', 4, 5, /duplicate/, 2);
});

test('columns inside sequences of mappings', () => {
  fails('- a: 1\n  b: "open\n', 2, 6, /unterminated quoted/);
  fails('x:\n  - a: &z 1\n', 2, 8, /anchors/);
});
