import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseCsv, parseCsvRecords } from '../src/csv.js';
import { isSitefileError } from '../src/errors.js';

const csv = (t: string) => parseCsvRecords(t, 'data.csv');

function fails(fn: () => unknown, line: number, column: number, match?: RegExp): void {
  assert.throws(fn, (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_DATA'), String(e));
    assert.equal(e.span?.file, 'data.csv');
    assert.equal(e.span?.line, line, e.message);
    assert.equal(e.span?.column, column, e.message);
    if (match) assert.match(e.detail, match);
    return true;
  });
}

test('basic records keep strings', () => {
  assert.deepEqual(csv('name,age,ok\nAda,36,true\nLin,,false\n'), [
    { name: 'Ada', age: '36', ok: 'true' },
    { name: 'Lin', age: '', ok: 'false' },
  ]);
});

test('empty and header-only input', () => {
  assert.deepEqual(csv(''), []);
  assert.deepEqual(csv('a,b\n'), []);
  assert.deepEqual(csv('a,b'), []);
  assert.deepEqual(parseCsv('', 'f', { header: false }), []);
});

test('trailing newline optional; CRLF; blank lines skipped', () => {
  const expected = [{ a: '1', b: '2' }, { a: '3', b: '4' }];
  assert.deepEqual(csv('a,b\n1,2\n3,4'), expected);
  assert.deepEqual(csv('a,b\n1,2\n3,4\n'), expected);
  assert.deepEqual(csv('a,b\r\n1,2\r\n3,4\r\n'), expected);
  assert.deepEqual(csv('a,b\n1,2\n\n3,4\n\n'), expected);
  assert.deepEqual(csv('a,b\r\n1,2\r\n\r\n3,4'), expected);
});

test('UTF-8 BOM is stripped', () => {
  assert.deepEqual(csv('\ufeffa,b\n1,2\n'), [{ a: '1', b: '2' }]);
  assert.deepEqual(Object.keys((csv('\ufeffa\n1\n')[0]) as object), ['a']);
});

test('quoted fields: escapes, delimiters, newlines', () => {
  assert.deepEqual(csv('a,b\n"x ""y"" z","1,2"\n"line1\nline2",""\n'), [
    { a: 'x "y" z', b: '1,2' },
    { a: 'line1\nline2', b: '' },
  ]);
  assert.deepEqual(csv('a,b\r\n"l1\r\nl2",x\r\n'), [{ a: 'l1\r\nl2', b: 'x' }]);
  assert.deepEqual(csv('"a b","c,d"\n1,2\n'), [{ 'a b': '1', 'c,d': '2' }]);
  assert.deepEqual(csv('a\n""""\n'), [{ a: '"' }]);
});

test('whitespace is preserved, not trimmed', () => {
  assert.deepEqual(csv('a,b\n x , y \n'), [{ a: ' x ', b: ' y ' }]);
});

test('trailing delimiter gives an empty last field', () => {
  assert.deepEqual(csv('a,b\n1,\n'), [{ a: '1', b: '' }]);
  assert.deepEqual(csv('a,b\n1,'), [{ a: '1', b: '' }]);
});

test('delimiters', () => {
  assert.deepEqual(parseCsv('a;b\n1,5;"x;y"\n', 'f', { delimiter: ';' }), [{ a: '1,5', b: 'x;y' }]);
  assert.deepEqual(parseCsv('a\tb\n1\t"x\ty"\n', 'f', { delimiter: '\t' }), [{ a: '1', b: 'x\ty' }]);
  assert.deepEqual(parseCsv('a,b\n1,2\n', 'f', { delimiter: ';' }) as unknown[], [{ 'a,b': '1,2' }]);
});

test('header: false returns string arrays, ragged allowed', () => {
  assert.deepEqual(parseCsv('a,b\n1\n2,3,4\n', 'f', { header: false }), [['a', 'b'], ['1'], ['2', '3', '4']]);
  assert.deepEqual(parseCsv('"x,y",z\n', 'f', { header: false }), [['x,y', 'z']]);
});

test('__proto__ column is plain data', () => {
  const [row] = csv('__proto__,b\nx,y\n') as Array<Record<string, string>>;
  assert.ok(row);
  assert.equal(Object.getPrototypeOf(row), Object.prototype);
  assert.deepEqual(Object.keys(row), ['__proto__', 'b']);
  assert.equal(row['__proto__'], 'x');
  assert.equal(({} as Record<string, unknown>).x, undefined);
});

test('header errors', () => {
  fails(() => csv('a,,c\n1,2,3\n'), 1, 3, /empty CSV column name/);
  fails(() => csv('a,b,a\n1,2,3\n'), 1, 5, /duplicate CSV column name 'a'/);
  fails(() => csv('\n\na,b,b\n'), 3, 5, /duplicate/);
});

test('field count mismatch reports the row line', () => {
  fails(() => csv('a,b\n1,2\n3\n'), 3, 1, /expected 2 fields but found 1/);
  fails(() => csv('a,b\n1,2,3\n'), 2, 1, /expected 2 fields but found 3/);
  fails(() => csv('a\n"x\ny"\n1,2\n'), 4, 1, /expected 1 field but found 2/);
});

test('unterminated quote', () => {
  fails(() => csv('a,b\n1,"open\nmore\n'), 2, 3, /unterminated/);
  fails(() => csv('"never closed'), 1, 1, /unterminated/);
});

test('quote in the middle of an unquoted field', () => {
  fails(() => csv('a,b\n1,x"y"\n'), 2, 4, /quote inside an unquoted field/);
  fails(() => csv('a"b,c\n'), 1, 2, /quote inside/);
});

test('text after a closing quote', () => {
  fails(() => csv('a,b\n"x"y,2\n'), 2, 4, /after closing quote/);
  fails(() => csv('a\n"x\ny"z\n'), 3, 3, /after closing quote/);
});
