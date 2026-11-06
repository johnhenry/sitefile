import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { sha256 } from '../src/digest.js';
import { build } from '../src/executor.js';
import { makeClient } from '../src/base-image.js';
import {
  CACHE_ARTIFACT_TYPE,
  CACHE_BLOB_MEDIA,
  CACHE_INDEX_MEDIA,
  EMPTY_MEDIA,
  encodeCacheIndex,
  encodeCacheManifest,
  parseCacheIndex,
  parseCacheRef,
  RegistryCache,
} from '../src/registry-cache.js';
import type { CacheEntry } from '../src/plugins/cache.js';
import { runCli, tmpDir } from './helpers/cli.js';
import { startMockRegistry, type MockRegistry } from './helpers/mock-registry.js';

const SECRET = 'cache-s3cr3t-do-not-push';
// Not deterministic on purpose: equal outputs from two homes prove the second build reused the first's result.
const PLUGIN = 'export function transform(r) { return r.input.text.toUpperCase() + " " + Math.random(); }\n';
const SITEFILE = 'FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\nTRANSFORM ./a.txt WITH up TO /a.txt\nTRANSFORM ./b.txt WITH up TO /b.txt\nEXPORT /\n';

async function withRegistry<T>(opts: Parameters<typeof startMockRegistry>[0], fn: (r: MockRegistry) => Promise<T>): Promise<T> {
  const r = await startMockRegistry(opts);
  try {
    return await fn(r);
  } finally {
    await r.close();
  }
}

async function site(sitefile = SITEFILE): Promise<string> {
  const dir = await tmpDir('sitefile-rcache-ctx-');
  await fs.mkdir(path.join(dir, 'plugins'));
  await fs.writeFile(path.join(dir, 'plugins', 'up.mjs'), PLUGIN);
  await fs.writeFile(path.join(dir, 'a.txt'), 'alpha');
  await fs.writeFile(path.join(dir, 'b.txt'), 'beta');
  await fs.writeFile(path.join(dir, 'Sitefile'), sitefile);
  return dir;
}

/** A fresh SITEFILE_HOME plus an empty DOCKER_CONFIG (never the user's ~/.docker). */
async function home(): Promise<{ home: string; env: Record<string, string> }> {
  const h = await tmpDir('sitefile-rcache-home-');
  const docker = await tmpDir('sitefile-rcache-docker-');
  return { home: h, env: { DOCKER_CONFIG: docker, SITEFILE_REGISTRY_AUTH: '', SITEFILE_CACHE_MAX: '' } };
}

async function cli(h: { home: string; env: Record<string, string> }, args: string[], env: Record<string, string> = {}) {
  return runCli(h.home, args, undefined, { ...h.env, ...env });
}

const read = (dir: string, f: string): Promise<string> => fs.readFile(path.join(dir, f), 'utf8');

test('--cache-to pushes an OCI artifact; --cache-from in a fresh home reuses its results', async () => {
  await withRegistry({}, async (reg) => {
    const ctx = await site();
    const ref = `${reg.host}/team/site-cache:main`;
    const h1 = await home();
    const a = await cli(h1, ['build', ctx, '-o', path.join(ctx, 'out1'), '--allow-plugins', '--no-lockfile', '--cache-to', ref]);
    assert.equal(a.code, 0, a.stderr);
    assert.match(a.stderr, /^Cache to .+site-cache:main: 2 entries, 4 blobs uploaded, 0 mounted, 0 already present \(sha256:[0-9a-f]{64}\)$/m);

    const manifest = reg.manifests.get('team/site-cache:main');
    assert.ok(manifest, 'manifest pushed under the tag');
    const m = JSON.parse(manifest.toString('utf8'));
    assert.equal(m.mediaType, 'application/vnd.oci.image.manifest.v1+json');
    assert.equal(m.artifactType, CACHE_ARTIFACT_TYPE);
    assert.deepEqual(m.config, { digest: sha256('{}'), mediaType: EMPTY_MEDIA, size: 2 });
    assert.equal(m.layers[0].mediaType, CACHE_INDEX_MEDIA);
    assert.equal(m.layers.length, 3);
    assert.ok(m.layers.slice(1).every((l: { mediaType: string }) => l.mediaType === CACHE_BLOB_MEDIA));
    const index = reg.blobs.get(m.layers[0].digest) as Buffer;
    const ix = JSON.parse(index.toString('utf8'));
    assert.equal(ix.version, 1);
    assert.equal(Object.keys(ix.entries).length, 2);
    // the index is canonical: re-encoding it gives the same bytes
    assert.equal(encodeCacheIndex(parseCacheIndex(index, new Map(m.layers.slice(1).map((l: { digest: string; size: number }) => [l.digest.slice(7), l.size])))).toString('utf8'), index.toString('utf8'));

    // A fresh home: the plugin output (random) comes back from the registry.
    const h2 = await home();
    const b = await cli(h2, ['build', ctx, '-o', path.join(ctx, 'out2'), '--allow-plugins', '--no-lockfile', '--cache-from', ref]);
    assert.equal(b.code, 0, b.stderr);
    assert.match(b.stderr, /^Cache from .+site-cache:main: 2 entries$/m);
    assert.equal(await read(ctx, 'out2/a.txt'), await read(ctx, 'out1/a.txt'));
    assert.equal(await read(ctx, 'out2/b.txt'), await read(ctx, 'out1/b.txt'));
    // ... and is now in the local cache: a third build needs no registry
    const before = reg.requests.length;
    const c = await cli(h2, ['build', ctx, '-o', path.join(ctx, 'out3'), '--allow-plugins', '--no-lockfile']);
    assert.equal(c.code, 0, c.stderr);
    assert.equal(reg.requests.length, before);
    assert.equal(await read(ctx, 'out3/a.txt'), await read(ctx, 'out1/a.txt'));
  });
});

test('blobs are fetched lazily, only for keys the build looks up', async () => {
  await withRegistry({}, async (reg) => {
    const ctx = await site();
    const ref = `${reg.host}/team/lazy:1`;
    const h1 = await home();
    assert.equal((await cli(h1, ['build', ctx, '-o', path.join(ctx, 'o1'), '--allow-plugins', '--no-lockfile', '--cache-to', ref])).code, 0);
    // a build with only one of the two TRANSFORMs fetches one result blob (plus the index)
    const one = await site('FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\nTRANSFORM ./a.txt WITH up TO /a.txt\nEXPORT /\n');
    const h2 = await home();
    const start = reg.requests.length;
    const r = await cli(h2, ['build', one, '-o', path.join(one, 'o'), '--allow-plugins', '--no-lockfile', '--cache-from', ref]);
    assert.equal(r.code, 0, r.stderr);
    const gets = reg.requests.slice(start).filter((q) => q.method === 'GET' && q.url.includes('/blobs/'));
    assert.equal(gets.length, 2, gets.map((g) => g.url).join('\n'));
    assert.equal(await read(one, 'o/a.txt'), await read(ctx, 'o1/a.txt'));
  });
});

test('pushing again uploads nothing new; identical entries give identical bytes from any home or context', async () => {
  await withRegistry({}, async (reg) => {
    const det = 'export function transform(r) { return r.input.text.toUpperCase(); }\n';
    const mk = async (): Promise<string> => {
      const d = await site();
      await fs.writeFile(path.join(d, 'plugins', 'up.mjs'), det);
      return d;
    };
    const [c1, c2] = [await mk(), await mk()];
    const h1 = await home();
    const h2 = await home();
    const ref1 = `${reg.host}/det/one:1`;
    const ref2 = `${reg.host}/det/two:1`;
    assert.equal((await cli(h1, ['build', c1, '-o', path.join(c1, 'o'), '--allow-plugins', '--no-lockfile', '--cache-to', ref1])).code, 0);
    const again = await cli(h1, ['build', c1, '-o', path.join(c1, 'o'), '--allow-plugins', '--no-lockfile', '--cache-to', ref1]);
    assert.equal(again.code, 0, again.stderr);
    assert.match(again.stderr, /2 entries, 0 blobs uploaded, 0 mounted, 4 already present/);
    // other context path, other home, other repository: same manifest; blobs held by the --cache-from repo are mounted
    const r2 = await cli(h2, ['build', c2, '-o', path.join(c2, 'o'), '--allow-plugins', '--no-lockfile', '--cache-from', ref1, '--cache-to', ref2]);
    assert.equal(r2.code, 0, r2.stderr);
    assert.match(r2.stderr, /2 entries, 2 blobs uploaded, 2 mounted, 0 already present/);
    const m1 = reg.manifests.get('det/one:1') as Buffer;
    const m2 = reg.manifests.get('det/two:1') as Buffer;
    assert.equal(sha256(m1), sha256(m2));
    // nothing local in what was pushed
    for (const [, b] of reg.blobs) {
      const t = b.toString('latin1');
      for (const p of [h1.home, h2.home, c1, c2, 'plugins', '/Users/', '/tmp']) assert.ok(!t.includes(p), `pushed bytes contain ${p}`);
    }
  });
});

test('credentials come from $SITEFILE_REGISTRY_AUTH; secrets are never pushed or printed; missing auth only warns', async () => {
  await withRegistry({ auth: 'bearer', user: 'ci', pass: SECRET, anonymousPull: false }, async (reg) => {
    const ctx = await site();
    const ref = `${reg.host}/private/cache:1`;
    const h = await home();
    const noAuth = await cli(h, ['build', ctx, '-o', path.join(ctx, 'o'), '--allow-plugins', '--no-lockfile', '--cache-from', ref, '--cache-to', ref]);
    assert.equal(noAuth.code, 0, noAuth.stderr);
    assert.match(noAuth.stderr, /^warning: --cache-from .+ not used: .+$/m);
    assert.match(noAuth.stderr, /^warning: --cache-to .+ not pushed: .*no credentials were found.*$/m);
    const auth = JSON.stringify({ [reg.host]: `ci:${SECRET}` });
    const ok = await cli(h, ['build', ctx, '-o', path.join(ctx, 'o'), '--allow-plugins', '--no-lockfile', '--cache-to', ref], { SITEFILE_REGISTRY_AUTH: auth });
    assert.equal(ok.code, 0, ok.stderr);
    assert.match(ok.stderr, /^Cache to /m);
    const h2 = await home();
    const pulled = await cli(h2, ['build', ctx, '-o', path.join(ctx, 'o2'), '--allow-plugins', '--no-lockfile', '--cache-from', ref], { SITEFILE_REGISTRY_AUTH: auth });
    assert.match(pulled.stderr, /^Cache from .+: 2 entries$/m);
    assert.equal(await read(ctx, 'o2/a.txt'), await read(ctx, 'o/a.txt'));
    for (const out of [noAuth, ok, pulled]) assert.ok(!(out.stdout + out.stderr).includes(SECRET));
    for (const [, b] of reg.blobs) assert.ok(!b.toString('latin1').includes(SECRET));
  });
});

test('registry failures warn and the build still succeeds: missing ref, not a cache, corrupt blob, unreachable registry', async () => {
  await withRegistry({}, async (reg) => {
    const ctx = await site();
    const h = await home();
    const go = (extra: string[], env: Record<string, string> = {}) => cli(h, ['build', ctx, '-o', path.join(ctx, 'o'), '--allow-plugins', '--no-lockfile', ...extra], env);

    let r = await go(['--cache-from', `${reg.host}/none/here:1`]);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stderr, /^warning: --cache-from .+\/none\/here:1 not used: .+HTTP 404.*$/m);

    reg.seedImage('img/base', '1', { layers: [{ '/a': 'a' }] });
    r = await go(['--cache-from', `${reg.host}/img/base:1`]);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stderr, /not used: .+ is not a sitefile cache \(artifactType missing\)$/m);

    // a corrupt result blob: the source is dropped for this build, the plugin runs instead
    const ref = `${reg.host}/team/c:1`;
    const h1 = await home();
    assert.equal((await cli(h1, ['build', ctx, '-o', path.join(ctx, 'o1'), '--allow-plugins', '--no-lockfile', '--cache-to', ref])).code, 0);
    const m = JSON.parse((reg.manifests.get('team/c:1') as Buffer).toString('utf8'));
    reg.options = { corrupt: m.layers[1].digest };
    const h3 = await home();
    r = await cli(h3, ['build', ctx, '-o', path.join(ctx, 'o'), '--allow-plugins', '--no-lockfile', '--cache-from', ref]);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stderr, /^warning: --cache-from .+team\/c:1: cached blob sha256:[0-9a-f]{64} does not match its digest or size; not using it for the rest of this build$/m);
    assert.match(await read(ctx, 'o/a.txt'), /^ALPHA /);
    const blobs = await fs.readdir(path.join(h3.home, 'cache', 'transform', 'blobs'));
    assert.ok(!blobs.some((b) => b.endsWith('.tmp')), 'no temp file left behind');
    reg.options = {};

    // an unreachable registry (a closed port on loopback)
    r = await go(['--cache-from', '127.0.0.1:1/x/y:1', '--cache-to', '127.0.0.1:1/x/y:1']);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stderr, /^warning: --cache-from 127\.0\.0\.1:1\/x\/y:1 not used: /m);
    assert.match(r.stderr, /^warning: --cache-to 127\.0\.0\.1:1\/x\/y:1 not pushed: /m);
  });
});

test('a failed build pushes nothing', async () => {
  await withRegistry({}, async (reg) => {
    const ctx = await site('FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\nTRANSFORM ./a.txt WITH up TO /a.txt\nCOPY ./missing.txt /m.txt\nEXPORT /\n');
    const r = await cli(await home(), ['build', ctx, '-o', path.join(ctx, 'o'), '--allow-plugins', '--no-lockfile', '--cache-to', `${reg.host}/team/f:1`]);
    assert.equal(r.code, 1);
    assert.equal(reg.manifests.size, 0);
    assert.ok(!reg.requests.some((q) => q.method === 'PUT' || q.method === 'POST'));
  });
});

test('$SITEFILE_CACHE_MAX bounds what a build fetches', async () => {
  await withRegistry({}, async (reg) => {
    const ctx = await site();
    const ref = `${reg.host}/team/lim:1`;
    assert.equal((await cli(await home(), ['build', ctx, '-o', path.join(ctx, 'o1'), '--allow-plugins', '--no-lockfile', '--cache-to', ref])).code, 0);
    const r = await cli(await home(), ['build', ctx, '-o', path.join(ctx, 'o2'), '--allow-plugins', '--no-lockfile', '--cache-from', ref], { SITEFILE_CACHE_MAX: '10' });
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stderr, /^warning: --cache-from: not fetching more cached results: they would exceed SITEFILE_CACHE_MAX=10B$/m);
    assert.notEqual(await read(ctx, 'o2/a.txt'), await read(ctx, 'o1/a.txt'), 'the plugin ran again');
  });
});

test('usage errors: build only, one --cache-to, a registry host, a tag, not with --no-cache or --watch', async () => {
  const ctx = await site();
  const h = await home();
  const cases: Array<[string[], string]> = [
    [['inspect', ctx, '--cache-from', 'localhost:5000/c:1'], '--cache-from and --cache-to are only for build'],
    [['validate', ctx, '--cache-to', 'localhost:5000/c:1'], '--cache-from and --cache-to are only for build'],
    [['build', ctx, '--watch', '--cache-from', 'localhost:5000/c:1'], '--watch cannot be combined with --cache-from or --cache-to'],
    [['build', ctx, '--no-cache', '--cache-to', 'localhost:5000/c:1'], '--no-cache cannot be combined with --cache-from or --cache-to'],
    [['build', ctx, '--cache-to', 'localhost:5000/c:1', '--cache-to', 'localhost:5000/c:2'], '--cache-to can be given only once'],
    [['build', ctx, '--cache-from', 'team/cache:1'], "--cache-from 'team/cache:1' has no registry host (e.g. localhost:5000/team/site-cache:main)"],
    [['build', ctx, '--cache-to', `localhost:5000/c@sha256:${'a'.repeat(64)}`], `--cache-to 'localhost:5000/c@sha256:${'a'.repeat(64)}' must name a tag, not a digest`],
  ];
  for (const [args, msg] of cases) {
    const r = await cli(h, args);
    assert.equal(r.code, 2, `${args.join(' ')}: ${r.stderr}`);
    assert.equal(r.stderr.split('\n')[0], `error: ${msg}`);
  }
  const bad = await cli(h, ['build', ctx, '--cache-from', 'localhost:5000/BAD']);
  assert.equal(bad.code, 2);
  assert.match(bad.stderr, /^error: invalid image reference 'localhost:5000\/BAD'/);
});

test('index validation rejects unsafe or inconsistent entries', () => {
  const hex = 'a'.repeat(64);
  const key = 'b'.repeat(64);
  const blobs = new Map([[hex, 3]]);
  const enc = (o: unknown): Buffer => Buffer.from(JSON.stringify(o));
  const ok = { version: 1, entries: { [key]: { kind: 'files', files: [['x/y.txt', hex, 3]] } } };
  assert.equal(parseCacheIndex(enc(ok), blobs).size, 1);
  const bad: unknown[] = [
    { version: 2, entries: {} },
    { version: 1, entries: [] },
    { version: 1, entries: { nothex: { kind: 'file', files: [['', hex, 3]] } } },
    { version: 1, entries: { [key]: { kind: 'file', files: [['a', hex, 3]] } } },
    { version: 1, entries: { [key]: { kind: 'files', files: [['../x', hex, 3]] } } },
    { version: 1, entries: { [key]: { kind: 'files', files: [['/abs', hex, 3]] } } },
    { version: 1, entries: { [key]: { kind: 'files', files: [['a', hex, 3], ['a', hex, 3]] } } },
    { version: 1, entries: { [key]: { kind: 'files', files: [['a', hex, 4]] } } },
    { version: 1, entries: { [key]: { kind: 'files', files: [['a', 'c'.repeat(64), 3]] } } },
    { version: 1, entries: { [key]: { kind: 'dir', files: [] } } },
  ];
  for (const b of bad) assert.throws(() => parseCacheIndex(enc(b), blobs), /the cache index is invalid/, JSON.stringify(b));
  assert.throws(() => parseCacheIndex(Buffer.from('{'), blobs), /the cache index is invalid/);
  // manifests list blob layers by digest, after the index
  const m = JSON.parse(encodeCacheManifest(Buffer.from('{}'), [['f'.repeat(64), 1], ['0'.repeat(64), 2]]).toString('utf8'));
  assert.deepEqual(m.layers.map((l: { digest: string }) => l.digest.slice(7, 8)), ['4', '0', 'f']);
});

test('library API: a remote cache hit starts no plugin process', async () => {
  await withRegistry({}, async (reg) => {
    const ctx = await site();
    const client = (registry: string) => makeClient(registry, { credentials: async () => undefined });
    const quiet = { warn: () => {}, notice: () => {}, limit: 0, client };
    const to = parseCacheRef('--cache-to', `${reg.host}/lib/cache:1`);
    const first = new RegistryCache({ ...quiet, from: [], to });
    const dir1 = path.join(await tmpDir(), 'transform');
    const a = await build({ contextDir: ctx, lockfile: false, allowAllPlugins: true, cacheDir: dir1, remoteCache: first });
    assert.equal(first.usedEntries.size, 2);
    await first.push(dir1);
    const second = new RegistryCache({ ...quiet, from: [to] });
    await second.load();
    const b = await build({ contextDir: ctx, lockfile: false, allowAllPlugins: true, cacheDir: path.join(await tmpDir(), 'transform'), remoteCache: second });
    assert.equal(b.transformStats.pluginsSpawned, 0);
    assert.equal(b.transformStats.hits, 2);
    assert.equal(second.fetched, 2);
    assert.equal(b.output.digest(), a.output.digest());
  });
});

test('golden bytes: the index and manifest encodings (rust/src/registry_cache.rs checks the same digests)', () => {
  const [a, b, c, d] = ['a', 'b', 'c', 'd'].map((x) => x.repeat(64)) as [string, string, string, string];
  const entries = new Map<string, CacheEntry>([
    [c, { kind: 'files', files: [['x/y.txt', d, 5], ['z.txt', a, 3]] }],
    [b, { kind: 'file', files: [['', a, 3]] }],
  ]);
  const index = encodeCacheIndex(entries);
  assert.equal(sha256(index), 'sha256:a240770e79b2f3734cbef07e5478cdbaee4db476baf9e09d563a338275760f67');
  assert.equal(sha256(encodeCacheManifest(index, [[d, 5], [a, 3]])), 'sha256:b36173b65e53cb51601880e5f54e3ca044ff17783b7cfb510551b9efc44f12fa');
});
