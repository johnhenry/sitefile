import { test } from 'node:test';
import assert from 'node:assert/strict';
import { IgnoreRules } from '../src/resources/ignore.js';

const ig = (text: string, rel: string, dir = false): boolean => IgnoreRules.parse(text).ignores(rel, dir);

test('blank lines and comments are skipped; escapes make them literal', () => {
  const r = IgnoreRules.parse('\n# comment\n   \n\\#hash\n');
  assert.equal(r.size, 1);
  assert.ok(r.ignores('#hash', false));
  assert.ok(IgnoreRules.parse('\\!bang').ignores('!bang', false));
});

test('unanchored patterns match at any depth; anchored ones only at the root', () => {
  assert.ok(ig('*.log', 'a.log'));
  assert.ok(ig('*.log', 'x/y/a.log'));
  assert.ok(!ig('*.log', 'a.logx'));
  assert.ok(ig('/build', 'build', true));
  assert.ok(!ig('/build', 'src/build', true));
  assert.ok(ig('docs/tmp', 'docs/tmp'));
  assert.ok(!ig('docs/tmp', 'x/docs/tmp'));
});

test('trailing slash matches directories only', () => {
  assert.ok(ig('cache/', 'cache', true));
  assert.ok(ig('cache/', 'a/cache', true));
  assert.ok(!ig('cache/', 'cache', false));
});

test('* and ? do not cross slashes', () => {
  assert.ok(ig('a/*.txt', 'a/b.txt'));
  assert.ok(!ig('a/*.txt', 'a/b/c.txt'));
  assert.ok(ig('f?.md', 'fo.md'));
  assert.ok(!ig('f?.md', 'f/.md'));
  assert.ok(!ig('f?.md', 'f.md'));
});

test('** matches across directories', () => {
  assert.ok(ig('**/foo', 'foo'));
  assert.ok(ig('**/foo', 'a/b/foo'));
  assert.ok(ig('a/**', 'a/b/c'));
  assert.ok(!ig('a/**', 'a', true));
  assert.ok(ig('a/**/z', 'a/z'));
  assert.ok(ig('a/**/z', 'a/b/c/z'));
  assert.ok(!ig('a/**/z', 'b/a/z'));
});

test('regex metacharacters are literal', () => {
  assert.ok(ig('a+b(1).txt', 'a+b(1).txt'));
  assert.ok(!ig('a.txt', 'axtxt'));
});

test('last matching rule wins; ! re-includes', () => {
  const rules = '*.md\n!keep.md\n';
  assert.ok(ig(rules, 'a.md'));
  assert.ok(!ig(rules, 'keep.md'));
  assert.ok(ig('*.md\n!keep.md\nkeep.md', 'keep.md'));
});

test('CRLF files and trailing spaces', () => {
  assert.ok(ig('a.txt  \r\nb.txt\r\n', 'a.txt'));
  assert.ok(ig('a.txt  \r\nb.txt\r\n', 'b.txt'));
});

test('empty rules ignore nothing', () => {
  assert.ok(!IgnoreRules.empty.ignores('anything', false));
});
