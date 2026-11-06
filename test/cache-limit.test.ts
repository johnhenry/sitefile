import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { cacheLimitFromEnv, cacheUsage, DEFAULT_CACHE_MAX, enforceCacheLimit, formatLimit, parseSize } from '../src/cache-limit.js';
import { TransformCache } from '../src/plugins/cache.js';
import { Store } from '../src/store.js';
import { makeFile, Tree } from '../src/tree.js';
import { runCli, tmpDir } from './helpers/cli.js';
import { parseLocalRef } from '../src/ref.js';

const enc = (s: string) => new TextEncoder().encode(s);
const mk = (files: Record<string, string>) => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
const HOUR = 3_600_000;
const later = () => Date.now() + 2 * HOUR; // everything is past its grace period
const exists = (p: string) => fs.stat(p).then(() => true, () => false);
async function age(file: string, msAgo: number): Promise<void> {
  const t = new Date(Date.now() - msAgo);
  await fs.utimes(file, t, t);
}
const hex = (c: string) => c.repeat(64);

test('parseSize, formatLimit and SITEFILE_CACHE_MAX', () => {
  assert.equal(parseSize('0'), 0);
  assert.equal(parseSize('1048576'), 1048576);
  assert.equal(parseSize('512K'), 512 * 1024);
  assert.equal(parseSize(' 512m '), 512 * 1024 ** 2);
  assert.equal(parseSize('2G'), 2 * 1024 ** 3);
  assert.equal(parseSize('2GiB'), 2 * 1024 ** 3);
  assert.equal(parseSize('2gb'), 2 * 1024 ** 3);
  assert.equal(parseSize('1.5G'), 1.5 * 1024 ** 3);
  assert.equal(parseSize('1T'), 1024 ** 4);
  assert.equal(parseSize('100B'), 100);
  for (const bad of ['', 'x', '-1', '1X', '1 GB extra', '1e3', '.5G']) assert.equal(parseSize(bad), undefined, bad);
  assert.equal(formatLimit(0), 'unlimited');
  assert.equal(formatLimit(1024 ** 3), '1GiB');
  assert.equal(formatLimit(512 * 1024 ** 2), '512MiB');
  assert.equal(formatLimit(1536 * 1024), '1536KiB');
  assert.equal(formatLimit(1000), '1000B');
  assert.equal(cacheLimitFromEnv({}), DEFAULT_CACHE_MAX);
  assert.equal(cacheLimitFromEnv({ SITEFILE_CACHE_MAX: '' }), DEFAULT_CACHE_MAX);
  assert.equal(cacheLimitFromEnv({ SITEFILE_CACHE_MAX: '0' }), 0);
  assert.equal(cacheLimitFromEnv({ SITEFILE_CACHE_MAX: '64M' }), 64 * 1024 ** 2);
  const warnings: string[] = [];
  assert.equal(cacheLimitFromEnv({ SITEFILE_CACHE_MAX: 'lots' }, (m) => warnings.push(m)), DEFAULT_CACHE_MAX);
  assert.deepEqual(warnings, ["SITEFILE_CACHE_MAX='lots' is not a size (bytes, or a number with K, M, G or T; 0 for no limit); using 1GiB"]);
});

test('TRANSFORM cache: least recently used entries go first; a hit refreshes recency; shared blobs stay while used', async () => {
  const home = await tmpDir();
  const c = new TransformCache(path.join(home, 'cache', 'transform'));
  await c.put('a', { kind: 'file', bytes: enc('A'.repeat(100)) });
  await c.put('b', { kind: 'file', bytes: enc('B'.repeat(100)) });
  await c.put('c', { kind: 'files', files: [['x', enc('C'.repeat(100))], ['y', enc('A'.repeat(100))]] }); // shares a's blob
  const entry = (k: string) => path.join(c.dir, 'entries', `${k}.json`);
  await age(entry('a'), 3 * HOUR);
  await age(entry('b'), 2 * HOUR);
  await age(entry('c'), 1 * HOUR);
  for (const n of await fs.readdir(path.join(c.dir, 'blobs'))) await age(path.join(c.dir, 'blobs', n), 3 * HOUR);
  assert.ok(await c.get('a'), 'hit: a becomes the most recently used');

  const [t] = await cacheUsage(home);
  assert.equal(t?.items, 3);
  // Drop one entry's worth: b is now the least recently used (a was just hit).
  const r = await enforceCacheLimit(home, (t?.evictable ?? 0) - 1, later());
  assert.equal(r.items, 1);
  assert.equal(await exists(entry('b')), false);
  assert.ok(await c.get('a'));
  assert.ok(await c.get('c'));
  assert.equal(r.before - r.after, r.bytes);

  // Evict everything evictable: c's and a's entries and all blobs.
  const all = await enforceCacheLimit(home, 1, later());
  assert.equal(all.items, 2);
  assert.equal(all.after, 0);
  assert.deepEqual(await fs.readdir(path.join(c.dir, 'blobs')), []);
});

test('nothing used within the grace period is evicted, even over the limit', async () => {
  const home = await tmpDir();
  const c = new TransformCache(path.join(home, 'cache', 'transform'));
  await c.put('a', { kind: 'file', bytes: enc('fresh') });
  const r = await enforceCacheLimit(home, 1);
  assert.equal(r.items, 0);
  assert.ok(await c.get('a'));
  // Under the limit: nothing is even read.
  assert.deepEqual(await enforceCacheLimit(home, 1024, later()), { items: 0, bytes: 0, before: r.before, after: r.before });
  assert.equal((await enforceCacheLimit(home, 0, later())).items, 0, '0 = no limit');
});

test('wasm module cache: LRU by mtime; temp files only after the long grace', async () => {
  const home = await tmpDir();
  const w = path.join(home, 'cache', 'wasm');
  await fs.mkdir(w, { recursive: true, mode: 0o700 });
  await fs.writeFile(path.join(w, `${hex('a')}.cwasm`), 'a'.repeat(50));
  await fs.writeFile(path.join(w, `${hex('b')}.cwasm`), 'b'.repeat(50));
  await fs.writeFile(path.join(w, `.${hex('c')}.1.0.tmp`), 'c'.repeat(50));
  await fs.writeFile(path.join(w, `.${hex('d')}.1.0.tmp`), 'd'.repeat(50));
  await age(path.join(w, `${hex('a')}.cwasm`), 30 * 60_000);
  await age(path.join(w, `${hex('b')}.cwasm`), 20 * 60_000);
  await age(path.join(w, `.${hex('c')}.1.0.tmp`), 30 * 60_000); // being written? kept
  await age(path.join(w, `.${hex('d')}.1.0.tmp`), 3 * HOUR); // leftover
  const u = (await cacheUsage(home)).find((x) => x.name === 'wasm');
  assert.deepEqual([u?.items, u?.bytes, u?.evictable], [2, 200, 200]);
  const r = await enforceCacheLimit(home, 120);
  assert.equal(r.items, 2);
  assert.deepEqual((await fs.readdir(w)).sort(), [`.${hex('c')}.1.0.tmp`, `${hex('b')}.cwasm`].sort());
});

test('image store: tagged images and their blobs are never evicted; untagged images and stray blobs are', async () => {
  const home = await tmpDir();
  const store = await Store.open(home);
  const site = mk({ '/index.html': 'shared layer' });
  const tagged = await store.addImage(site, { labels: { v: '1' } });
  await store.tag(tagged.id, parseLocalRef('site:1'));
  const untaggedShared = await store.addImage(site, { labels: { v: '2' } }); // same layer, other config
  const untaggedOwn = await store.addImage(mk({ '/index.html': 'other' }));
  const stray = path.join(home, 'blobs', 'sha256', hex('e'));
  await fs.writeFile(stray, 'stray');
  const freshTmp = path.join(home, 'blobs', 'sha256', '.tmp-abc');
  await fs.writeFile(freshTmp, 'pulling');
  const before = (await cacheUsage(home)).find((x) => x.name === 'store');
  assert.equal(before?.items, 10); // 3 + 2 (layer shared with the tagged one) + 3 + stray + tmp
  assert.ok((before?.evictable ?? 0) > 0 && (before?.evictable ?? 0) < (before?.bytes ?? 0));

  // 59 minutes later: still within the store's grace period, nothing goes.
  assert.equal((await enforceCacheLimit(home, 1, Date.now() + 59 * 60_000)).items, 0);
  await age(freshTmp, -3 * HOUR); // keep the temp file "fresh" relative to later()
  const r = await enforceCacheLimit(home, 1, later());
  assert.equal(r.items, 3, 'two untagged images and the stray blob');
  const ids = (await store.list()).map((i) => i.id);
  assert.deepEqual(ids, [tagged.id]);
  assert.ok(!ids.includes(untaggedShared.id) && !ids.includes(untaggedOwn.id));
  assert.equal((await store.loadTree(tagged.id)).size, 1, 'the tagged image is intact');
  assert.equal(await exists(stray), false);
  assert.equal(await exists(freshTmp), true);
  const after = (await cacheUsage(home)).find((x) => x.name === 'store');
  assert.equal(after?.evictable, 'pulling'.length);
  assert.equal(after?.items, 4);
});

test('CLI: SITEFILE_CACHE_MAX evicts after a build, cache du reports, bad values warn, nothing fails the build', async () => {
  const home = await tmpDir();
  const ctx = await tmpDir('sitefile-limit-ctx-');
  await fs.mkdir(path.join(ctx, 'public'));
  await fs.writeFile(path.join(ctx, 'public', 'index.html'), '<h1>x</h1>');
  await fs.writeFile(path.join(ctx, 'Sitefile'), 'FROM scratch\nCOPY ./public/ /\nEXPORT /\n');
  const c = new TransformCache(path.join(home, 'cache', 'transform'));
  await c.put('old', { kind: 'file', bytes: enc('o'.repeat(2000)) });
  await c.put('new', { kind: 'file', bytes: enc('n'.repeat(2000)) });
  for (const d of ['entries', 'blobs']) for (const n of await fs.readdir(path.join(c.dir, d))) await age(path.join(c.dir, d, n), 2 * HOUR);
  await age(path.join(c.dir, 'entries', 'new.json'), HOUR);

  const du = await runCli(home, ['cache', 'du', '--json'], ctx, { SITEFILE_CACHE_MAX: '3K' });
  assert.equal(du.code, 0, du.stderr);
  const j = JSON.parse(du.stdout) as { limit: number; evictable: number; caches: Array<{ name: string; items: number }> };
  assert.equal(j.limit, 3072);
  assert.equal(j.caches.map((x) => x.name).join(), 'transform,wasm,store,servers');
  assert.equal(j.caches[0]?.items, 2);
  const text = await runCli(home, ['cache', 'ls'], ctx, { SITEFILE_CACHE_MAX: '3K' });
  assert.match(text.stdout, /^CACHE +ITEMS +SIZE +EVICTABLE +PATH\ntransform +2 /);
  assert.match(text.stdout, /\nLimit: 3KiB \(SITEFILE_CACHE_MAX; /);

  const b = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist')], ctx, { SITEFILE_CACHE_MAX: '3K' });
  assert.equal(b.code, 0, b.stderr);
  assert.match(b.stderr, /^Evicted 1 cache item \(2\.\d+kB\) to stay under SITEFILE_CACHE_MAX=3KiB\n$/);
  assert.equal(await exists(path.join(c.dir, 'entries', 'old.json')), false);
  assert.ok(await c.get('new'));

  const bad = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--force'], ctx, { SITEFILE_CACHE_MAX: 'huge' });
  assert.equal(bad.code, 0, bad.stderr);
  assert.match(bad.stderr, /^warning: SITEFILE_CACHE_MAX='huge' is not a size/);

  // A broken store index: over the limit the store part cannot be planned -> a warning, exit 0.
  await fs.writeFile(path.join(home, 'blobs', 'sha256', hex('f')), 'x'.repeat(5000)).catch(async () => {
    await fs.mkdir(path.join(home, 'blobs', 'sha256'), { recursive: true });
    await fs.writeFile(path.join(home, 'blobs', 'sha256', hex('f')), 'x'.repeat(5000));
  });
  await fs.writeFile(path.join(home, 'index.json'), '{not json');
  const broken = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--force'], ctx, { SITEFILE_CACHE_MAX: '1K' });
  assert.equal(broken.code, 0, broken.stderr);
  assert.match(broken.stderr, /^warning: cache size limit not applied: .*index\.json is corrupt/m);

  assert.equal((await runCli(home, ['cache', 'prune', '--max-size', '0'])).code, 2);
  assert.equal((await runCli(home, ['cache', 'prune', '--max-size', '1M', '--older-than', '1'])).code, 2);
  await fs.rm(path.join(home, 'index.json'));
  const pr = await runCli(home, ['cache', 'prune', '--max-size', '1M']);
  assert.equal(pr.code, 0, pr.stderr);
  assert.equal(pr.stdout, 'Evicted 0 items\nTotal reclaimed space: 0B\n');
});
