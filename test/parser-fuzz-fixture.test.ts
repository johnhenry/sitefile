import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// The Rust parser is checked against rust/tests/fixtures/sitefile/fuzz.json
// (language::differential). This keeps the fixture honest: the TypeScript
// parser, validator and planner must still produce every recorded result.
// After an intended language change, regenerate it:
//   node rust/tests/fixtures/sitefile/generate.mjs

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dir = path.join(root, 'rust/tests/fixtures/sitefile');

interface Recorded {
  input: string;
  parse: unknown;
  plan?: unknown;
}

test('parser fuzz fixture matches the TypeScript reference', async () => {
  const { loadLib, record } = (await import(pathToFileURL(path.join(dir, 'record.mjs')).href)) as {
    loadLib: (root: string) => Promise<unknown>;
    record: (lib: unknown, input: string) => Recorded;
  };
  const lib = await loadLib(root);
  const cases = JSON.parse(readFileSync(path.join(dir, 'fuzz.json'), 'utf8')) as Recorded[];
  assert.ok(cases.length >= 300, `only ${cases.length} cases`);
  let stale = 0;
  for (const c of cases) {
    const now = record(lib, c.input);
    if (JSON.stringify(now) !== JSON.stringify(c)) {
      if (stale++ < 3) assert.deepEqual(now, c, `stale fixture for ${JSON.stringify(c.input)}; run node rust/tests/fixtures/sitefile/generate.mjs`);
    }
  }
  assert.equal(stale, 0);
  for (const c of cases) assert.ok(!('crash' in (c.parse as object)) && !(c.plan && 'crash' in (c.plan as object)), `TypeScript crashed on ${JSON.stringify(c.input)}`);
});
