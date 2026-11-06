import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// The Rust HTML/CSS reference scanner is checked against rust/tests/fixtures/links/links.json
// (rust/tests/links.rs). This keeps the fixture honest: the TypeScript scanner must still produce
// every recorded result. After an intended change, regenerate it:
//   node rust/tests/fixtures/links/generate.mjs

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dir = path.join(root, 'rust/tests/fixtures/links');

interface Recorded {
  kind: string;
  input: string;
}

test('links fixture matches the TypeScript scanner', async () => {
  const { loadLib, record } = (await import(pathToFileURL(path.join(dir, 'record.mjs')).href)) as {
    loadLib: (root: string) => Promise<unknown>;
    record: (lib: unknown, kind: string, input: string) => Recorded;
  };
  const lib = await loadLib(root);
  const cases = JSON.parse(readFileSync(path.join(dir, 'links.json'), 'utf8')) as Recorded[];
  assert.ok(cases.length >= 300, `only ${cases.length} cases`);
  let stale = 0;
  for (const c of cases) {
    const now = record(lib, c.kind, c.input);
    if (JSON.stringify(now) !== JSON.stringify(c)) {
      if (stale++ < 3) assert.deepEqual(now, c, `stale fixture for ${JSON.stringify(c.input)}; run node rust/tests/fixtures/links/generate.mjs`);
    }
  }
  assert.equal(stale, 0);
});
