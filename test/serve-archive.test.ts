import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { writeTar, writeTgz, writeZip } from '../src/archive-writers.js';
import { SitefileError } from '../src/errors.js';
import { blobPathOf, buildOciImage, layoutTree, ociArchive } from '../src/oci.js';
import { DEFAULT_ARCHIVE_LIMITS } from '../src/resources/types.js';
import { loadArchiveTree, loadServedTree, ServerRegistry } from '../src/servers.js';
import { Store } from '../src/store.js';
import { makeFile, Tree } from '../src/tree.js';
import { writeWebBundle } from '../src/wbn.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

const enc = (s: string) => new TextEncoder().encode(s);
const treeOf = (files: Record<string, string>) => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
const site = () => treeOf({ '/index.html': 'hi', '/.env': 'secret', '/.well-known/security.txt': 'x', '/sub/a.txt': 'a' });
const files = (t: Tree) => t.entries().map(([p, n]) => [p, Buffer.from(n.bytes).toString()]);

/** `site()` as zip, tar, tgz, wbn and an OCI archive, written into `dir`. */
async function writeArchives(dir: string): Promise<Array<[string, string]>> {
  const s = site();
  const all: Array<[string, Uint8Array]> = [
    ['site.zip', writeZip(s)],
    ['site.tar', writeTar(s)],
    ['site.tgz', writeTgz(s)],
    ['site.wbn', writeWebBundle(s, 'https://example.com/')],
    ['site.oci.tar', ociArchive(buildOciImage(s), 'site:1', undefined)],
  ];
  for (const [n, b] of all) await fs.writeFile(path.join(dir, n), b);
  return all.map(([n]) => [n, path.join(dir, n)]);
}

async function code(p: Promise<unknown>): Promise<{ code: string; message: string }> {
  try {
    await p;
  } catch (e) {
    assert.ok(e instanceof SitefileError, String(e));
    return { code: e.code, message: e.message };
  }
  throw new Error('expected an error');
}

test('archive files are served in every format: zip, tar, tgz, wbn like a directory, an OCI archive like its image', async () => {
  const dir = await tmpDir();
  const store = await Store.open(path.join(dir, 'home'));
  for (const [name, file] of await writeArchives(dir)) {
    const t = await loadServedTree(store, { kind: 'archive', path: file });
    if (name === 'site.oci.tar') assert.deepEqual(files(t), files(site()), name);
    else assert.deepEqual(files(t), [['/.well-known/security.txt', 'x'], ['/index.html', 'hi'], ['/sub/a.txt', 'a']], name);
  }
});

test('archive errors name the file and what it is not', async () => {
  const dir = await tmpDir();
  const notes = path.join(dir, 'notes.txt');
  await fs.writeFile(notes, 'just text');
  assert.deepEqual(await code(loadArchiveTree(notes)), { code: 'E_TYPE', message: `${notes}: not a recognized archive (expected tar, tar.gz, tar.zst, zip or web bundle)` });
  const gone = path.join(dir, 'gone.zip');
  assert.deepEqual(await code(loadArchiveTree(gone)), { code: 'E_NOT_FOUND', message: `'${gone}' does not exist` });
  assert.deepEqual(await code(loadArchiveTree(dir)), { code: 'E_TYPE', message: `'${dir}' is not a file` });
});

test('archive limits are enforced for every format, the file size and the compression ratio', async () => {
  const dir = await tmpDir();
  const all = await writeArchives(dir);
  for (const [name, file] of all) {
    for (const limits of [{ ...DEFAULT_ARCHIVE_LIMITS, maxEntries: 2 }, { ...DEFAULT_ARCHIVE_LIMITS, maxTotalBytes: 3 }]) {
      assert.equal((await code(loadArchiveTree(file, limits))).code, 'E_ARCHIVE_LIMIT', name);
    }
    await loadArchiveTree(file);
  }
  const big = path.join(dir, 'big.bin');
  await fs.writeFile(big, 'x'.repeat(5000));
  assert.deepEqual(await code(loadArchiveTree(big, { maxEntries: 0, maxTotalBytes: 0, maxRatio: 100 })), {
    code: 'E_ARCHIVE_LIMIT',
    message: `${big}: archive expands beyond 0 bytes`,
  });
  const bomb = path.join(dir, 'bomb.tgz');
  await fs.writeFile(bomb, writeTgz(Tree.fromFiles([['/zeros', makeFile(new Uint8Array(8 << 20), 't')]])));
  const e = await code(loadArchiveTree(bomb));
  assert.equal(e.code, 'E_ARCHIVE_LIMIT');
  assert.match(e.message, /compression ratio exceeds 100:1/);
});

test('OCI archives check every blob they read', async () => {
  const dir = await tmpDir();
  const img = buildOciImage(site());
  const layout = layoutTree(img);
  const withFile = (p: string, body?: string) => Tree.fromFiles([...layout.entries().filter(([q]) => q !== p), ...(body === undefined ? [] : [[p, makeFile(enc(body), 't')] as const])]);
  const cases: Array<[string, Tree, string]> = [
    ['tampered', withFile(blobPathOf(img.configDigest), '{}'), 'does not match its digest'],
    ['no-layer', withFile(blobPathOf(img.layerDigest)), 'OCI archive has no blob'],
    ['no-index', withFile('/index.json'), 'OCI archive has no index.json'],
    ['bad-index', withFile('/index.json', '[]'), 'OCI archive index.json is corrupt'],
  ];
  for (const [name, t, why] of cases) {
    const p = path.join(dir, `${name}.oci.tar`);
    await fs.writeFile(p, writeTar(t));
    const e = await code(loadArchiveTree(p));
    assert.equal(e.code, 'E_ARCHIVE', name);
    assert.ok(e.message.startsWith(`${p}: `) && e.message.includes(why), `${name}: ${e.message}`);
  }
});

const homes: string[] = [];
after(async () => {
  for (const h of homes) {
    const r = await runCli(h, ['servers', '-a', '--json']);
    try {
      for (const s of JSON.parse(r.stdout) as Array<{ id: string }>) await runCli(h, ['server', 'rm', '-f', s.id]);
    } catch {
      /* nothing to clean */
    }
  }
});

test('serve -d site.zip: a bare file name, the path as SITE, an archive record, and restart re-reads it', async () => {
  const home = await tmpDir();
  homes.push(home);
  const dir = await tmpDir('sitefile-serve-');
  await fs.writeFile(path.join(dir, 'site.zip'), writeZip(treeOf({ '/index.html': 'one' })));
  let r = await runCli(home, ['serve', '-d', '--name', 'zipped', 'site.zip'], dir);
  assert.equal(r.code, 0, r.stderr);
  const file = path.join(dir, 'site.zip');
  const url = new RegExp(`Started zipped \\([0-9a-f]{12}\\) serving ${file.replace(/[./]/g, '\\$&')} at (http://\\S+)`).exec(r.stdout)?.[1];
  assert.ok(url, r.stdout);
  assert.equal(await (await fetch(url)).text(), 'one');
  const [listed] = JSON.parse((await runCli(home, ['servers', '--json'])).stdout) as Array<{ id: string; site: string }>;
  assert.equal(listed!.site, file);
  const state = JSON.parse(await fs.readFile(path.join(home, 'servers', `${listed!.id}.json`), 'utf8'));
  assert.deepEqual(state.source, { kind: 'archive', path: file });
  const reg = await ServerRegistry.open(await Store.open(home));
  assert.deepEqual((await reg.read(listed!.id))?.source, { kind: 'archive', path: file });

  await fs.writeFile(file, writeZip(treeOf({ '/index.html': 'two' })));
  r = await runCli(home, ['server', 'restart', 'zipped']);
  assert.equal(r.code, 0, r.stderr);
  assert.equal(await (await fetch(/at (http:\/\/\S+)/.exec(r.stdout)![1]!)).text(), 'two');
  assert.equal((await runCli(home, ['server', 'rm', '-f', 'zipped'])).code, 0);
});

test('serve -d ./x.oci.tar serves what build -o x.oci.tar wrote; a non-archive file is E_TYPE and leaves no record', async () => {
  const home = await tmpDir();
  homes.push(home);
  const ctx = await makeSite();
  const out = path.join(await tmpDir('sitefile-serve-'), 'x.oci.tar');
  assert.equal((await runCli(home, ['build', ctx, '-o', out])).code, 0);
  let r = await runCli(home, ['serve', '-d', out]);
  assert.equal(r.code, 0, r.stderr);
  const url = /at (http:\/\/\S+)/.exec(r.stdout)![1]!;
  assert.match(await (await fetch(url + 'about/')).text(), /About/);

  const notes = path.join(path.dirname(out), 'notes.txt');
  await fs.writeFile(notes, 'plain');
  r = await runCli(home, ['serve', '-d', notes]);
  assert.equal(r.code, 1);
  assert.equal(r.stderr, `error[E_TYPE]: ${notes}: not a recognized archive (expected tar, tar.gz, tar.zst, zip or web bundle)\n`);
  r = await runCli(home, ['serve', '-d', path.join(path.dirname(out), 'gone.zip')]);
  assert.match(r.stderr, /^error\[E_NOT_FOUND\]: '.*gone\.zip' is not a directory or a file\n/);
  assert.equal((JSON.parse((await runCli(home, ['servers', '-a', '--json'])).stdout) as unknown[]).length, 1);
});
