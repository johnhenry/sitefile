import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm, readFile, writeFile, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Lockfile } from '../src/resources/lockfile.js';
import { isSitefileError } from '../src/errors.js';

const H1 = 'sha256:' + 'a'.repeat(64);
const H2 = 'sha256:' + 'b'.repeat(64);

async function withDir(fn: (dir: string) => Promise<void>): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), 'sitefile-lock-'));
  try {
    await fn(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test('missing lockfile loads empty and is not created when unused', () =>
  withDir(async (dir) => {
    const path = join(dir, 'Sitefile.lock');
    const lock = await Lockfile.load(path);
    assert.equal(lock.get('https://x/'), undefined);
    assert.equal(await lock.save(), false);
    await assert.rejects(stat(path));
  }));

test('records entries with sorted keys and trailing newline', () =>
  withDir(async (dir) => {
    const path = join(dir, 'Sitefile.lock');
    const lock = await Lockfile.load(path);
    lock.record('https://z.example/b', { integrity: H2, size: 2, resolved: 'https://z.example/b' });
    lock.record('https://a.example/a', { integrity: H1, size: 1, resolved: 'https://cdn.example/a' });
    assert.equal(await lock.save(), true);
    const text = await readFile(path, 'utf8');
    assert.equal(
      text,
      JSON.stringify(
        {
          version: 1,
          resources: {
            'https://a.example/a': { integrity: H1, size: 1, resolved: 'https://cdn.example/a' },
            'https://z.example/b': { integrity: H2, size: 2, resolved: 'https://z.example/b' },
          },
        },
        null,
        2,
      ) + '\n',
    );
    assert.equal(await lock.save(), false, 'unchanged content is not rewritten');
  }));

test('only used entries are kept; unchanged files are not rewritten', () =>
  withDir(async (dir) => {
    const path = join(dir, 'Sitefile.lock');
    const first = await Lockfile.load(path);
    first.record('https://a/', { integrity: H1, size: 1, resolved: 'https://a/' });
    first.record('https://b/', { integrity: H2, size: 2, resolved: 'https://b/' });
    await first.save();

    const second = await Lockfile.load(path);
    assert.deepEqual(second.get('https://b/'), { integrity: H2, size: 2, resolved: 'https://b/' });
    second.markUsed('https://a/');
    second.markUsed('https://b/');
    second.markUsed('https://not-there/');
    assert.equal(await second.save(), false);

    const third = await Lockfile.load(path);
    third.markUsed('https://a/');
    assert.equal(await third.save(), true);
    const data = JSON.parse(await readFile(path, 'utf8'));
    assert.deepEqual(Object.keys(data.resources), ['https://a/']);
  }));

test('malformed lockfiles fail with E_VALIDATION', () =>
  withDir(async (dir) => {
    const path = join(dir, 'Sitefile.lock');
    const cases = [
      '{not json',
      '[]',
      '{"version":2,"resources":{}}',
      '{"version":1,"resources":[]}',
      `{"version":1,"resources":{"https://a/":{"integrity":"md5:x","size":1,"resolved":"https://a/"}}}`,
      `{"version":1,"resources":{"https://a/":{"integrity":"${H1}","size":-1,"resolved":"https://a/"}}}`,
      `{"version":1,"resources":{"https://a/":{"integrity":"${H1}","size":1}}}`,
    ];
    for (const c of cases) {
      await writeFile(path, c);
      await assert.rejects(Lockfile.load(path), (e: unknown) => isSitefileError(e, 'E_VALIDATION'), c);
    }
  }));
