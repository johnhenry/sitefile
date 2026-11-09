import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { compilePattern, PatternError, patternMatches } from '../src/pattern.js';
import { fuzzCases, type FuzzCase } from './helpers/pattern-fuzz.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const fixtures = path.join(root, 'rust/tests/fixtures/pattern');
interface Vector {
  readonly pattern: string;
  readonly match?: readonly string[];
  readonly reject?: readonly string[];
  readonly error?: string;
}
const vectors = JSON.parse(readFileSync(path.join(fixtures, 'vectors.json'), 'utf8')) as Vector[];

/** JavaScript's own answer with the u flag, or undefined when it rejects the pattern. */
function jsRegExp(src: string): RegExp | undefined {
  try {
    return new RegExp(src, 'u');
  } catch {
    return undefined;
  }
}

test('shared vectors (rust/tests/fixtures/pattern/vectors.json): matches, rejects and errors', () => {
  assert.ok(vectors.length > 50);
  for (const v of vectors) {
    if (v.error !== undefined) {
      assert.throws(() => compilePattern(v.pattern), (e: unknown) => e instanceof PatternError && e.message === v.error, v.pattern);
      continue;
    }
    const p = compilePattern(v.pattern);
    for (const s of v.match ?? []) assert.ok(patternMatches(p, s), `${v.pattern} should match ${JSON.stringify(s)}`);
    for (const s of v.reject ?? []) assert.ok(!patternMatches(p, s), `${v.pattern} should not match ${JSON.stringify(s)}`);
  }
});

test('accepted patterns mean what JavaScript RegExp (u flag) means, and are valid there', () => {
  const check = (src: string, inputs: readonly string[]): void => {
    const re = jsRegExp(src);
    assert.ok(re, `${src} is accepted but JavaScript rejects it`);
    const p = compilePattern(src);
    for (const s of inputs) assert.equal(patternMatches(p, s), re.test(s), `${src} on ${JSON.stringify(s)}`);
  };
  for (const v of vectors) if (v.error === undefined) check(v.pattern, [...(v.match ?? []), ...(v.reject ?? [])]);
  let accepted = 0;
  for (const c of fuzzCases(3000, 7)) {
    if (typeof c.result === 'string') continue;
    accepted++;
    check(c.pattern, c.inputs);
  }
  assert.ok(accepted > 1000, `only ${accepted} random patterns compiled`);
});

test('fuzz.json (the Rust differential fixture) is what the TypeScript engine answers', () => {
  const committed = JSON.parse(readFileSync(path.join(fixtures, 'fuzz.json'), 'utf8')) as FuzzCase[];
  assert.deepEqual(committed, fuzzCases(committed.length, 135), 'regenerate with node rust/tests/fixtures/pattern/generate.mjs');
});

test('matching is linear: nested quantifiers on a long non-matching string', () => {
  const p = compilePattern('^(a+)+$');
  const t = Date.now();
  assert.equal(patternMatches(p, 'a'.repeat(5000) + 'b'), false);
  assert.ok(Date.now() - t < 2000);
});
