import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { isSitefileError } from '../src/errors.js';
import { parseLocalRef, parseRef, refKey } from '../src/ref.js';
import { extractArchive } from '../src/resources/archive.js';
import { shortId, Store } from '../src/store.js';
import { makeFile, Tree } from '../src/tree.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

const enc = (s: string) => new TextEncoder().encode(s);
const mk = (files: Record<string, string>) => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
const blobCount = async (home: string) => (await fs.readdir(path.join(home, 'blobs', 'sha256'))).length;

test('reference grammar: valid and invalid references', () => {
  const ok: Array<[string, string | undefined, string, string | undefined]> = [
    ['site', undefined, 'site', undefined],
    ['site:v1.2_x-3', undefined, 'site', 'v1.2_x-3'],
    ['my.org/team/site-a_b:latest', 'my.org', 'team/site-a_b', 'latest'],
    ['team/site-a_b:latest', undefined, 'team/site-a_b', 'latest'],
    ['localhost:5000/site:1', 'localhost:5000', 'site', '1'],
    ['registry.example.com/x/y', 'registry.example.com', 'x/y', undefined],
  ];
  for (const [input, registry, name, tag] of ok) {
    const r = parseRef(input);
    assert.deepEqual([r.registry, r.name, r.tag], [registry, name, tag], input);
  }
  const digest = 'sha256:' + 'a'.repeat(64);
  assert.equal(parseRef(`site:1@${digest}`).digest, digest);
  assert.equal(refKey(parseRef('site')), 'site:latest');
  for (const bad of ['', 'Site', 'a//b', '-a', 'a-', 'a:', 'a:-x', 'a:' + 'x'.repeat(129), 'a b', 'a@sha256:short', 'a/B', '_a', 'a..', 'a/.b']) {
    assert.throws(() => parseRef(bad), (e) => isSitefileError(e, 'E_USAGE'), `'${bad}' should be invalid`);
  }
  // a registry host is parsed, but only local references are usable
  assert.throws(() => parseLocalRef('docker.io/library/nginx'), (e) => isSitefileError(e, 'E_UNSUPPORTED') && /registries are not supported yet/.test((e as Error).message));
});

test('the store is an OCI image layout; add, tag and list', async () => {
  const home = await tmpDir();
  const store = await Store.open(home);
  assert.equal(await fs.readFile(path.join(home, 'oci-layout'), 'utf8'), '{"imageLayoutVersion":"1.0.0"}');
  const t = mk({ '/index.html': 'hi', '/a/b.txt': 'x' });
  const { id, created } = await store.addImage(t, { title: 'demo' });
  assert.equal(created, true);
  assert.match(id, /^sha256:[0-9a-f]{64}$/);
  await store.tag(id, parseLocalRef('demo'));
  await store.tag(id, parseLocalRef('demo:v1'));
  const idx = JSON.parse(await fs.readFile(path.join(home, 'index.json'), 'utf8'));
  assert.equal(idx.mediaType, 'application/vnd.oci.image.index.v1+json');
  assert.deepEqual(idx.manifests.map((m: { annotations: Record<string, string> }) => m.annotations['org.opencontainers.image.ref.name']).sort(), ['demo:latest', 'demo:v1']);
  const [img] = await store.list();
  assert.equal(img!.id, id);
  assert.deepEqual([...img!.tags].sort(), ['demo:latest', 'demo:v1']);
  assert.equal(img!.files, 2);
  assert.equal(img!.size, 3);
  assert.equal((await store.loadTree(id)).digest(), t.digest());
});

test('image ID is stable for identical output and the second add is a no-op', async () => {
  const store = await Store.open(await tmpDir());
  const t = mk({ '/index.html': 'same' });
  const a = await store.addImage(t, { title: 'x' }, new Date('2020-01-01'));
  const b = await store.addImage(mk({ '/index.html': 'same' }), { title: 'x' }, new Date('2030-01-01'));
  assert.equal(a.id, b.id);
  assert.equal(b.created, false);
  assert.equal((await store.list()).length, 1);
  assert.equal((await store.list())[0]!.created, '2020-01-01T00:00:00.000Z', 'first-stored time is kept');
  const c = await store.addImage(mk({ '/index.html': 'different' }), { title: 'x' });
  assert.notEqual(c.id, a.id);
});

test('blobs are shared: a layer common to two images is stored once', async () => {
  const home = await tmpDir();
  const store = await Store.open(home);
  const t = mk({ '/index.html': 'shared layer' });
  await store.addImage(t, { title: 'a' });
  assert.equal(await blobCount(home), 3); // manifest, config, layer
  await store.addImage(t, { title: 'b' }); // new manifest + config, same layer
  assert.equal(await blobCount(home), 5);
});

test('tag moves, rmi and prune', async () => {
  const home = await tmpDir();
  const store = await Store.open(home);
  const a = (await store.addImage(mk({ '/i.html': 'one' }))).id;
  const b = (await store.addImage(mk({ '/i.html': 'two' }))).id;
  await store.tag(a, parseLocalRef('site:1'));
  await store.tag(b, parseLocalRef('site:1')); // moves the tag; a becomes untagged
  const list = await store.list();
  assert.deepEqual(list.find((i) => i.id === a)!.tags, []);
  assert.deepEqual(list.find((i) => i.id === b)!.tags, ['site:1']);
  const before = await blobCount(home);
  const pruned = await store.prune();
  assert.equal(pruned.manifests, 1);
  assert.equal(pruned.blobs, 3);
  assert.ok(pruned.bytes > 0);
  assert.equal(await blobCount(home), before - 3);
  assert.equal((await store.list()).length, 1);
  // two tags: removing one keeps the image; removing by ID needs -f
  await store.tag(b, parseLocalRef('other'));
  assert.deepEqual(await store.removeImage('other'), { untagged: ['other:latest'], deleted: undefined });
  await store.tag(b, parseLocalRef('other'));
  await assert.rejects(store.removeImage(shortId(b)), (e) => isSitefileError(e, 'E_STORE'));
  const r = await store.removeImage(shortId(b), true);
  assert.deepEqual(r.untagged.sort(), ['other:latest', 'site:1']);
  assert.equal(r.deleted, b);
  assert.equal((await store.list()).length, 0);
  assert.equal((await store.prune()).blobs, 3);
  assert.equal(await blobCount(home), 0);
  await assert.rejects(store.removeImage('site:1'), (e) => isSitefileError(e, 'E_NOT_FOUND'));
});

test('short IDs resolve when unique; refs, full IDs and digests too', async () => {
  const store = await Store.open(await tmpDir());
  const { id } = await store.addImage(mk({ '/x': '1' }));
  await store.tag(id, parseLocalRef('x'));
  const hex = id.slice(7);
  assert.equal(await store.resolve(hex.slice(0, 12)), id);
  assert.equal(await store.resolve(id), id);
  assert.equal(await store.resolve('x'), id);
  assert.equal(await store.resolve('x:latest'), id);
  assert.equal(await store.resolve(`x@${id}`), id);
  await assert.rejects(store.resolve(hex.slice(0, 11)), (e) => isSitefileError(e, 'E_NOT_FOUND')); // too short to be an ID
  await assert.rejects(store.resolve('nope'), (e) => isSitefileError(e, 'E_NOT_FOUND'));
  await assert.rejects(store.resolve('registry.io/x'), (e) => isSitefileError(e, 'E_UNSUPPORTED'));
});

test('corrupt blobs are detected (E_STORE)', async () => {
  for (const which of ['layer', 'config', 'manifest'] as const) {
    const home = await tmpDir();
    const store = await Store.open(home);
    const { id } = await store.addImage(mk({ '/index.html': 'content ' + which }));
    const d = await store.details(id);
    const digest = which === 'layer' ? d.manifest.layers[0]!.digest : which === 'config' ? d.manifest.config.digest : id;
    const file = path.join(home, 'blobs', 'sha256', digest.slice(7));
    const bytes = await fs.readFile(file);
    bytes[bytes.length - 3] = bytes[bytes.length - 3]! ^ 0xff;
    await fs.writeFile(file, bytes);
    const p = which === 'layer' ? store.loadTree(id) : store.details(id);
    await assert.rejects(p, (e) => isSitefileError(e, 'E_STORE') && /corrupt|does not match/.test((e as Error).message), which);
  }
  const home = await tmpDir();
  const store = await Store.open(home);
  const { id } = await store.addImage(mk({ '/a': 'b' }));
  await fs.rm(path.join(home, 'blobs', 'sha256', (await store.details(id)).manifest.layers[0]!.digest.slice(7)));
  await assert.rejects(store.loadTree(id), (e) => isSitefileError(e, 'E_STORE') && /missing/.test((e as Error).message));
});

test('CLI: build -t stores, images lists, inspect, tag, rmi, prune', async () => {
  const home = await tmpDir();
  const ctx = await makeSite();
  let r = await runCli(home, ['build', ctx, '-t', 'blog', '-t', 'blog:v1', '-t', 'team/blog:2']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /Tagged blog:latest/);
  await assert.rejects(fs.stat(path.join(ctx, 'dist')), 'with -t and no -o nothing is written to the context');
  r = await runCli(home, ['images']);
  assert.match(r.stdout, /^REPOSITORY\s+TAG\s+IMAGE ID\s+CREATED\s+FILES\s+SIZE/);
  assert.match(r.stdout, /blog\s+latest\s+[0-9a-f]{12}\s+.*ago\s+3\s+\d+B/);
  assert.match(r.stdout, /team\/blog\s+2\s/);
  r = await runCli(home, ['images', '--json']);
  const rows = JSON.parse(r.stdout) as Array<{ repository: string; tag: string; id: string; files: number; size: number }>;
  assert.equal(rows.length, 3);
  assert.equal(new Set(rows.map((x) => x.id)).size, 1);
  const id = rows[0]!.id;
  r = await runCli(home, ['images', '-q']);
  assert.equal(r.stdout, `${id.slice(7, 19)}\n`);
  r = await runCli(home, ['images', '--digests']);
  assert.match(r.stdout, new RegExp(id));
  r = await runCli(home, ['image', 'inspect', 'blog:v1', '--json']);
  const info = JSON.parse(r.stdout);
  assert.equal(info.id, id);
  assert.equal(info.root, '/srv/www');
  assert.deepEqual(info.files.map((f: { path: string }) => f.path), ['/about/index.html', '/index.html', '/style.css']);
  r = await runCli(home, ['image', 'inspect', id.slice(7, 19)]);
  assert.match(r.stdout, /Tags:\s+blog:latest, blog:v1, team\/blog:2/);
  r = await runCli(home, ['tag', 'blog', 'copy:prod']);
  assert.equal(r.code, 0, r.stderr);
  r = await runCli(home, ['rmi', 'blog:v1', 'copy:prod']);
  assert.match(r.stdout, /Untagged: blog:v1/);
  assert.equal((await runCli(home, ['rmi', 'ghost'])).code, 1);
  assert.equal((await runCli(home, ['tag', 'blog', 'Bad Name'])).code, 2);
  assert.equal((await runCli(home, ['tag', 'blog', 'docker.io/x/y'])).code, 1);
  assert.equal((await runCli(home, ['build', ctx, '-t', 'UPPER'])).code, 2);
  // rebuilding with changed content leaves the old image untagged; prune reclaims it
  await fs.writeFile(path.join(ctx, 'public', 'index.html'), 'changed');
  assert.equal((await runCli(home, ['build', ctx, '-t', 'blog', '-t', 'team/blog:2'])).code, 0);
  r = await runCli(home, ['images']);
  assert.match(r.stdout, /<none>\s+<none>\s+/);
  r = await runCli(home, ['image', 'prune']);
  assert.match(r.stdout, /Deleted 1 image and 3 blobs/);
  assert.match(r.stdout, /Total reclaimed space: \d/);
  assert.doesNotMatch((await runCli(home, ['images'])).stdout, /<none>/);
  assert.equal((await runCli(home, ['image', 'diff', 'blog', 'blog'])).stdout, '');
});

test('CLI: image diff lists added, removed and changed paths', async () => {
  const home = await tmpDir();
  const ctx = await makeSite();
  assert.equal((await runCli(home, ['build', ctx, '-t', 'd:1'])).code, 0);
  await fs.rm(path.join(ctx, 'public', 'about'), { recursive: true });
  await fs.writeFile(path.join(ctx, 'public', 'style.css'), 'x');
  await fs.writeFile(path.join(ctx, 'public', 'new.txt'), 'n');
  assert.equal((await runCli(home, ['build', ctx, '-t', 'd:2'])).code, 0);
  const r = await runCli(home, ['image', 'diff', 'd:1', 'd:2']);
  assert.equal(r.stdout, 'D /about/index.html\nA /new.txt\nC /style.css\n');
});

test('CLI: save to a directory and a zip round-trips the files', async () => {
  const home = await tmpDir();
  const out = await tmpDir();
  const ctx = await makeSite();
  assert.equal((await runCli(home, ['build', ctx, '-t', 'site'])).code, 0);
  const store = await Store.open(home);
  const want = await store.loadTree(await store.resolve('site'));
  let r = await runCli(home, ['save', 'site', '-o', path.join(out, 'dir')]);
  assert.equal(r.code, 0, r.stderr);
  assert.equal(await fs.readFile(path.join(out, 'dir', 'about', 'index.html'), 'utf8'), '<h1>About</h1>');
  r = await runCli(home, ['save', 'site', '-o', path.join(out, 'site.zip')]);
  assert.equal(r.code, 0, r.stderr);
  const zipped = await extractArchive(new Uint8Array(await fs.readFile(path.join(out, 'site.zip'))), 'z');
  assert.equal(zipped.digest(), want.digest());
  for (const name of ['site.tar', 'site.tar.gz', 'site.tgz']) {
    assert.equal((await runCli(home, ['save', 'site', '-o', path.join(out, name)])).code, 0);
    const t = await extractArchive(new Uint8Array(await fs.readFile(path.join(out, name))), 'z');
    assert.equal(t.digest(), want.digest(), name);
  }
  // refuses to clobber without --force
  assert.equal((await runCli(home, ['save', 'site', '-o', path.join(out, 'dir')])).code, 1);
  assert.equal((await runCli(home, ['save', 'site', '-o', path.join(out, 'dir'), '--force'])).code, 0);
  assert.equal((await runCli(home, ['save', 'site'])).code, 2);
});

test('CLI: save as an OCI archive or layout is the stored image, byte for byte', async () => {
  const home = await tmpDir();
  const out = await tmpDir();
  const ctx = await makeSite();
  assert.equal((await runCli(home, ['build', ctx, '-t', 'site'])).code, 0);
  const store = await Store.open(home);
  const id = await store.resolve('site');
  assert.equal((await runCli(home, ['save', 'site', '-o', path.join(out, 'site.oci.tar')])).code, 0);
  assert.equal((await runCli(home, ['save', 'site', '--format', 'oci', '-o', path.join(out, 'layout')])).code, 0);
  const archive = await extractArchive(new Uint8Array(await fs.readFile(path.join(out, 'site.oci.tar'))), 'a');
  const index = JSON.parse(await fs.readFile(path.join(out, 'layout', 'index.json'), 'utf8'));
  assert.equal(index.manifests[0].digest, id);
  assert.equal(index.manifests[0].annotations['org.opencontainers.image.ref.name'], 'site:latest');
  assert.ok(archive.get(`/blobs/sha256/${id.slice(7)}`));
  for (const [p, n] of archive.entries()) {
    if (p.startsWith('/blobs/')) assert.deepEqual(new Uint8Array(await fs.readFile(path.join(out, 'layout', p))), n.bytes);
  }
});

test('build-arg values are never written to the store', async () => {
  const home = await tmpDir();
  const secret = 'hunter2-super-secret-value';
  const ctx = await makeSite({}, 'FROM scratch\nARG TOKEN = "none"\nCOPY ./public/ /\nEXPORT /\n');
  const r = await runCli(home, ['build', ctx, '-t', 'sec', '--build-arg', `TOKEN=${secret}`]);
  assert.equal(r.code, 0, r.stderr);
  const walk = async (dir: string): Promise<string[]> =>
    (await fs.readdir(dir, { withFileTypes: true })).flatMap((e) => (e.isDirectory() ? [] : [path.join(dir, e.name)])).concat(
      ...(await Promise.all((await fs.readdir(dir, { withFileTypes: true })).filter((e) => e.isDirectory()).map((e) => walk(path.join(dir, e.name))))),
    );
  const { gunzipSync } = await import('node:zlib');
  for (const f of await walk(home)) {
    const b = await fs.readFile(f);
    assert.ok(!b.includes(secret), `${f} contains the build arg value`);
    try {
      assert.ok(!Buffer.from(gunzipSync(b)).includes(secret));
    } catch (e) {
      if ((e as Error).name === 'AssertionError') throw e; // not gzip: fine
    }
  }
});

test('concurrent CLI calls do not corrupt the index', async () => {
  const home = await tmpDir();
  const ctx = await makeSite();
  const runs = await Promise.all(Array.from({ length: 8 }, (_, i) => runCli(home, ['build', ctx, '-t', `c${i}`])));
  for (const r of runs) assert.equal(r.code, 0, r.stderr);
  const rows = JSON.parse((await runCli(home, ['images', '--json'])).stdout) as Array<{ repository: string }>;
  assert.deepEqual(rows.map((x) => x.repository).sort(), Array.from({ length: 8 }, (_, i) => `c${i}`));
  await assert.rejects(fs.stat(path.join(home, 'lock')));
});

test('per-command --help and unknown subcommands', async () => {
  const home = await tmpDir();
  for (const cmd of [['images'], ['tag'], ['rmi'], ['save'], ['run'], ['ps'], ['stop'], ['rm'], ['logs'], ['restart'], ['image', 'inspect'], ['image', 'prune'], ['image', 'diff']]) {
    const r = await runCli(home, [...cmd, '--help']);
    assert.equal(r.code, 0, cmd.join(' '));
    assert.match(r.stdout, /^Usage: sitefile /, cmd.join(' '));
  }
  assert.equal((await runCli(home, ['image', 'bogus'])).code, 2);
  assert.match((await runCli(home, ['--help'])).stdout, /sitefile run /);
});
