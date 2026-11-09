import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { extractArchive, isOsMetadata, withoutOsMetadata } from '../src/resources/archive.js';
import { loadArchiveTree, loadDirectoryTree } from '../src/servers.js';
import { zip } from './archive-fixtures.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});
async function project(files: Record<string, string | Uint8Array>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-osmeta-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

/** What Finder's "Compress" makes of a folder `site` (plus Windows' and Finder's per-folder files). */
export const finderZip = (): Uint8Array =>
  zip([
    { name: 'site/', mode: 0o040755 },
    { name: 'site/index.html', data: '<h1>hi</h1>', deflate: true },
    { name: 'site/.DS_Store', data: 'Bud1' },
    { name: 'site/Icon\r', data: '' },
    { name: 'site/img/Thumbs.db', data: 'x' },
    { name: 'site/img/Desktop.ini', data: 'x' },
    { name: 'site/img/a.png', data: 'png' },
    { name: '__MACOSX/', mode: 0o040755 },
    { name: '__MACOSX/site/', mode: 0o040755 },
    { name: '__MACOSX/site/._index.html', data: 'Mac OS X' },
    { name: '__MACOSX/site/img/._a.png', data: 'Mac OS X' },
  ]);

test('isOsMetadata: __MACOSX anywhere, AppleDouble, .DS_Store, Icon\\r, Thumbs.db and desktop.ini in any case', () => {
  for (const p of ['/__MACOSX/a', '/x/__MACOSX/y', '/._a.html', '/d/._x', '/.DS_Store', '/d/Icon\r', '/THUMBS.DB', '/d/Desktop.ini']) assert.ok(isOsMetadata(p), p);
  for (const p of ['/index.html', '/_a', '/.well-known/x', '/Icon', '/thumbs.db.bak', '/__MACOSX.txt', '/a.DS_Store', '/.ds_store_x']) assert.ok(!isOsMetadata(p), p);
});

test('a Finder zip loses its OS metadata and is a single top-level folder', async () => {
  const raw = await extractArchive(finderZip(), 'site.zip');
  const { tree, skipped } = withoutOsMetadata(raw);
  assert.equal(skipped, 6);
  assert.deepEqual(tree.entries().map(([p]) => p), ['/site/img/a.png', '/site/index.html']);
  assert.deepEqual(new Set(tree.entries().map(([p]) => p.split('/')[1])), new Set(['site']));
});

test('IMPORT AS tree drops OS metadata with one warning; keepOsMetadata keeps it', async () => {
  const files = { 'site.zip': finderZip(), Sitefile: 'FROM scratch\nIMPORT s FROM ./site.zip AS tree\nCOPY s:/ /\nEXPORT /' };
  const warnings: string[] = [];
  const r = await build({ contextDir: await project(files), lockfile: false, onWarning: (m) => warnings.push(m) });
  assert.deepEqual(r.output.entries().map(([p]) => p), ['/site/img/a.png', '/site/index.html']);
  assert.deepEqual(warnings, [
    'IMPORT s: skipped 6 OS metadata files (__MACOSX, ._*, .DS_Store, Thumbs.db, desktop.ini, Icon\\r); --keep-os-metadata keeps them',
  ]);
  const kept: string[] = [];
  const k = await build({ contextDir: await project(files), lockfile: false, keepOsMetadata: true, onWarning: (m) => kept.push(m) });
  assert.equal(k.output.size, 8);
  // Kept metadata is reported only by the export's portability check (Icon\r, __MACOSX without .nojekyll).
  assert.deepEqual(kept.filter((m) => !m.startsWith('EXPORT /: ')), []);
  assert.ok(kept.some((m) => m.includes("'/site/Icon\r' cannot be extracted on Windows")), kept.join('\n'));
});

test('serve: archives and directories are served without OS metadata unless kept', async () => {
  const dir = await project({ 'site.zip': finderZip(), 'd/index.html': 'hi', 'd/Thumbs.db': 'x', 'd/__MACOSX/._index.html': 'x', 'd/desktop.ini': 'x' });
  const warnings: string[] = [];
  const zipTree = await loadArchiveTree(path.join(dir, 'site.zip'), undefined, { warn: (m) => warnings.push(m) });
  assert.deepEqual(zipTree.entries().map(([p]) => p), ['/site/img/a.png', '/site/index.html']);
  const keptZip = await loadArchiveTree(path.join(dir, 'site.zip'), undefined, { keepOsMetadata: true });
  // Hidden files (.DS_Store, ._*) are still not served.
  assert.deepEqual(keptZip.entries().map(([p]) => p).filter((p) => p.includes('.DS_Store')), []);
  assert.equal(keptZip.size, 5);
  const dirTree = await loadDirectoryTree(path.join(dir, 'd'), { warn: (m) => warnings.push(m) });
  assert.deepEqual(dirTree.entries().map(([p]) => p), ['/index.html']);
  assert.equal((await loadDirectoryTree(path.join(dir, 'd'), { keepOsMetadata: true })).size, 3);
  assert.equal(warnings.length, 2);
  assert.match(warnings[0]!, /site\.zip: skipped 6 OS metadata files/);
  assert.match(warnings[1]!, /: skipped 3 OS metadata files/);
});
