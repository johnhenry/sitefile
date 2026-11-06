import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// The Rust archive and web bundle readers are checked against
// rust/tests/fixtures/archive/fuzz.json (resources::archive_differential).
// This keeps the fixture honest: the TypeScript readers must still produce
// every recorded result. After an intended reader change, regenerate it:
//   node rust/tests/fixtures/archive/generate.mjs

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dir = path.join(root, 'rust/tests/fixtures/archive');

interface Recorded {
  input: string;
  limits: [number, number, number];
  result: Record<string, unknown>;
  bundle?: unknown;
}

test('archive fuzz fixture matches the TypeScript reference', async () => {
  const { loadLib, record } = (await import(pathToFileURL(path.join(dir, 'record.mjs')).href)) as {
    loadLib: (root: string) => Promise<unknown>;
    record: (lib: unknown, input: string, limits: Recorded['limits']) => Promise<Recorded>;
  };
  const lib = await loadLib(root);
  const cases = JSON.parse(readFileSync(path.join(dir, 'fuzz.json'), 'utf8')) as Recorded[];
  assert.ok(cases.length >= 300, `only ${cases.length} cases`);
  assert.ok(cases.filter((c) => 'ok' in c.result).length >= 50, 'too few accepted archives');
  let stale = 0;
  for (const c of cases) {
    const now = await record(lib, c.input, c.limits);
    if (JSON.stringify(now) !== JSON.stringify(c)) {
      if (stale++ < 3) assert.deepEqual(now, c, `stale fixture for ${c.input.slice(0, 80)}; run node rust/tests/fixtures/archive/generate.mjs`);
    }
  }
  assert.equal(stale, 0);
  for (const c of cases) {
    assert.ok(!('crash' in c.result) && !('violation' in c.result), `TypeScript bug on ${c.input.slice(0, 80)}: ${JSON.stringify(c.result)}`);
  }
});
