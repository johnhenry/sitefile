// The editor query copies (Zed, Neovim, Helix) are generated from ../queries
// and must be up to date; they must also compile against the grammar.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { generate } from '../scripts/sync-queries.mjs';
import { language, treeSitter } from './helpers.mjs';

const generated = generate();

test('editor queries are in sync (npm run sync-queries)', () => {
  for (const [file, text] of generated) {
    assert.ok(existsSync(file), `${file} is missing`);
    assert.equal(readFileSync(file, 'utf8'), text, `${file} is out of date`);
  }
});

test('editor queries compile against the grammar', () => {
  for (const file of generated.keys()) {
    treeSitter(['query', ...language(), file, 'test/fixtures/edge-cases.Sitefile']);
  }
});

/** https://zed.dev/docs/extensions/languages#syntax-highlighting */
const ZED_CAPTURES = new Set(
  'attribute boolean comment comment.doc constant constant.builtin constructor embedded emphasis emphasis.strong enum function hint keyword label link_text link_uri number operator predictive preproc primary property punctuation punctuation.bracket punctuation.delimiter punctuation.list_marker punctuation.special string string.escape string.regex string.special string.special.symbol tag tag.doctype text.literal title type type.builtin variable variable.special variable.parameter variant'.split(
    ' ',
  ),
);

test('Zed highlights use only Zed capture names', () => {
  const [file] = [...generated.keys()].filter((f) => f.includes(`zed${path.sep}`) && f.endsWith('highlights.scm'));
  const names = [...readFileSync(file, 'utf8').matchAll(/@([A-Za-z][\w.]*)/g)].map((m) => m[1]);
  for (const n of names) assert.ok(ZED_CAPTURES.has(n), `@${n} is not a Zed capture`);
});
