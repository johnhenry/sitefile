// Every query file compiles against the grammar and captures what it should
// on test/fixtures/edge-cases.Sitefile.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { EMBEDS } from '../keywords.js';
import { grammarDir, language, treeSitter } from './helpers.mjs';

const fixture = 'test/fixtures/edge-cases.Sitefile';
const lines = readFileSync(path.join(grammarDir, fixture), 'utf8').split('\n');

/** Run a query; returns [{ capture, text, row }] (text for single-line captures only). */
function query(file, source = fixture) {
  const out = treeSitter(['query', ...language(), file, source]);
  return [...out.matchAll(/capture: (?:\d+ - )?([\w.]+), start: \((\d+), (\d+)\), end: \((\d+), (\d+)\)(?:, text: `([^`]*)`)?/g)].map((m) => ({
    capture: m[1],
    row: Number(m[2]),
    text: m[6] ?? (m[2] === m[4] ? lines[Number(m[2])].slice(Number(m[3]), Number(m[5])) : undefined),
  }));
}

test('every query file compiles and matches something', () => {
  for (const f of readdirSync(path.join(grammarDir, 'queries'))) {
    assert.ok(query(`queries/${f}`).length > 0, f);
  }
});

const has = (caps, capture, text) => caps.some((c) => c.capture === capture && c.text === text);

test('highlights: keywords, names, values and template tags', () => {
  const caps = query('queries/highlights.scm');
  for (const [capture, text] of [
    ['keyword', 'arg'],
    ['keyword', 'Copy'],
    ['keyword', 'INTEGRITY'],
    ['keyword.repeat', 'FOR'],
    ['comment', '# a comment inside a continuation'],
    ['keyword.directive', '# sitefile=1'],
    ['variable', 'title'],
    ['type.builtin', 'number'],
    ['type.builtin', 'JSON'],
    ['type.builtin', 'tar.gz'],
    ['module', 'data'],
    ['module', 'f'],
    ['label', 'build'],
    ['label', 'final'],
    ['constant.builtin', 'scratch'],
    ['constant.builtin', 'HTML'],
    ['attribute', 'overwrite'],
    ['function', 'og'],
    ['function.call', 'magick'],
    ['variable.builtin', '{input}'],
    ['property', 'order'],
    ['property', 'size'],
    ['string.special.path', '/tabs.txt'],
    ['string.special', 'sha256:ce488399c576d775b6c36a355b949a2da5992646f53e0e4e574618dc19648aaa'],
    ['number', '3'],
    ['boolean', 'false'],
    ['operator', '<<-'],
    ['label', 'EOF'],
    ['keyword.repeat', '#each'],
    ['keyword.conditional', '#if'],
    ['keyword.conditional', 'else'],
    ['function.builtin', 'raw'],
    ['variable.member', 'link'],
    ['string.escape', '$${'],
    ['punctuation.special', '${'],
  ]) {
    assert.ok(has(caps, capture, text), `@${capture} \`${text}\``);
  }
  // `color=#6d28d9`-style words and an unknown flag value stay plain.
  assert.ok(!caps.some((c) => c.capture === 'comment' && c.text?.startsWith('#6d')));
});

test('highlights: no node gets two captures', () => {
  const caps = query('queries/highlights.scm', '../../examples/inline/Sitefile');
  const seen = new Map();
  const out = treeSitter(['query', ...language(), 'queries/highlights.scm', '../../examples/inline/Sitefile']);
  for (const m of out.matchAll(/capture: (?:\d+ - )?([\w.]+), start: (\(\d+, \d+\)), end: (\(\d+, \d+\))/g)) {
    if (m[1].startsWith('_')) continue;
    const key = `${m[2]}-${m[3]}`;
    assert.ok(!seen.has(key) || seen.get(key) === m[1], `${key} captured as @${seen.get(key)} and @${m[1]}`);
    seen.set(key, m[1]);
  }
  assert.ok(caps.length > 0);
});

test('injections: heredoc bodies by delimiter, RENDER and COPY', () => {
  const out = treeSitter(['query', ...language(), 'queries/injections.scm', fixture]);
  const bodies = [...out.matchAll(/capture: (?:\d+ - )?injection\.content, start: \((\d+), \d+\)/g)].map((m) => lines[Number(m[1]) - 1]);
  // The header line before each injected body.
  assert.deepEqual(bodies, [
    'COPY <<JSON /data.json',
    'render --escape=HTML <<-html to /page.html',
    'RENDER <<MARKDOWN TO /notes.md',
    'RENDER <<JS TO /app.js',
    'RENDER <<YML TO /site.yml',
    'RENDER <<SVG TO /logo.svg',
    'RENDER <<HTML TO /unclosed.html',
  ]);
  for (const e of EMBEDS) assert.match(out + readFileSync(path.join(grammarDir, 'queries/injections.scm'), 'utf8'), new RegExp(`"${e.lang}"`));
});

test('highlights and injections agree on which delimiters embed a language', () => {
  const hl = readFileSync(path.join(grammarDir, 'queries/highlights.scm'), 'utf8');
  const notAnyOf = /#not-any-of\? @_delimiter([^)]*)\)/.exec(hl);
  assert.ok(notAnyOf);
  const listed = [...notAnyOf[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]).sort();
  const expected = EMBEDS.flatMap((e) => e.names.flatMap((n) => [n, n.toLowerCase()])).sort();
  assert.deepEqual(listed, expected);
});

test('outline: stages, imports, plugins and tools', () => {
  const caps = query('queries/outline.scm');
  const names = caps.filter((c) => c.capture === 'name').map((c) => c.text);
  for (const n of ['build', 'final', 'data', 'notes', 'og', 'magick']) assert.ok(names.includes(n), n);
});

test('folds: heredoc instructions and continuations', () => {
  const out = treeSitter(['query', ...language(), 'queries/folds.scm', fixture]);
  const multiLine = [...out.matchAll(/start: \((\d+), \d+\), end: \((\d+), \d+\)/g)].filter((m) => m[1] !== m[2]);
  assert.ok(multiLine.length >= 10, `${multiLine.length} multi-line folds`);
});
