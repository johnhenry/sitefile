import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdtemp, mkdir, readdir, readFile, rm, stat, utimes, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { build, type BuildOptions, type BuildResult } from '../src/executor.js';
import { cacheKey, canonicalJson, TransformCache } from '../src/plugins/cache.js';
import { runCli, tmpDir } from './helpers/cli.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function dir(prefix: string): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), prefix));
  temps.push(d);
  return d;
}
async function write(root: string, files: Record<string, string>): Promise<void> {
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(root, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
}
const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

const PLUGIN = `import { readFileSync } from 'node:fs';
const helper = () => readFileSync(new URL('./helper.txt', import.meta.url), 'utf8');
export function transform(req) {
  return JSON.stringify({ o: req.options, d: req.dest, t: req.input && req.input.text, data: req.data, item: req.item, h: helper() });
}`;

const SITEFILE = 'FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a.txt WITH p TO /a.json OPTIONS n=1\nEXPORT /';

interface Ctx {
  ctx: string;
  cacheDir: string;
  go: (extra?: Partial<BuildOptions>) => Promise<BuildResult>;
}

async function setup(files: Record<string, string> = {}): Promise<Ctx> {
  const ctx = await dir('sitefile-cache-ctx-');
  const cacheDir = path.join(await dir('sitefile-cache-home-'), 'cache', 'transform');
  await write(ctx, { 'plugins/p.mjs': PLUGIN, 'plugins/helper.txt': 'h1', 'a.txt': 'alpha', Sitefile: SITEFILE, ...files });
  return { ctx, cacheDir, go: (extra = {}) => build({ contextDir: ctx, lockfile: false, cacheDir, allowAllPlugins: true, ...extra }) };
}

test('a second build is served from the cache without starting the plugin', async () => {
  const t = await setup();
  const a = await t.go();
  assert.equal(a.transformStats.pluginsSpawned, 1);
  assert.equal(a.transformStats.misses, 1);
  assert.equal(a.transformStats.writes, 1);
  const b = await t.go();
  assert.equal(b.transformStats.pluginsSpawned, 0, 'the plugin process must not start on a hit');
  assert.equal(b.transformStats.pluginCalls, 0);
  assert.equal(b.transformStats.hits, 1);
  assert.equal(b.transformStats.misses, 0);
  assert.equal(b.output.digest(), a.output.digest());
  assert.equal(JSON.parse(text(b.output.get('/a.json')?.bytes)).t, 'alpha');
});

test('changing options, input, dest, data, plugin or a plugin sibling file misses', async () => {
  const t = await setup({ 'xs.json': '[{"k":"x"}]' });
  const sf = (opts: string, dest = '/a.json'): string => `FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a.txt WITH p TO ${dest} OPTIONS ${opts}\nEXPORT /`;
  await t.go();
  assert.equal((await t.go()).transformStats.hits, 1);

  await write(t.ctx, { Sitefile: sf('n=2') });
  let r = await t.go();
  assert.deepEqual([r.transformStats.hits, r.transformStats.misses, r.transformStats.pluginsSpawned], [0, 1, 1], 'options');

  await write(t.ctx, { Sitefile: sf('n=2', '/b.json') });
  r = await t.go();
  assert.equal(r.transformStats.misses, 1, 'dest');

  await write(t.ctx, { 'a.txt': 'alpha!' });
  r = await t.go();
  assert.equal(r.transformStats.misses, 1, 'input bytes');
  assert.equal((await t.go()).transformStats.hits, 1);

  await write(t.ctx, { 'plugins/p.mjs': PLUGIN + '\n// changed' });
  r = await t.go();
  assert.equal(r.transformStats.misses, 1, 'plugin module');
  assert.equal((await t.go()).transformStats.hits, 1);

  await write(t.ctx, { 'plugins/helper.txt': 'h2' });
  r = await t.go();
  assert.equal(r.transformStats.misses, 1, 'a file the plugin reads');
  assert.equal(JSON.parse(text(r.output.get('/b.json')?.bytes)).h, 'h2');

  // data and FOR items
  const data = (v: string): string => `FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nIMPORT xs FROM ./xs.json AS json\nTRANSFORM xs: WITH p TO /d.json\nTRANSFORM WITH p TO /i/\${x.k}.json FOR x IN xs\nEXPORT /${v}`;
  await write(t.ctx, { Sitefile: data('') });
  r = await t.go();
  assert.equal(r.transformStats.misses, 2);
  assert.equal((await t.go()).transformStats.hits, 2);
  await write(t.ctx, { 'xs.json': '[{"k":"x","extra":1}]' });
  r = await t.go();
  assert.equal(r.transformStats.misses, 2, 'data / item');
});

test('FOR items with {files} results round-trip through the cache', async () => {
  const t = await setup({
    'plugins/many.mjs': "export default { apiVersion: 1, transform: (r) => ({ files: { 'a.txt': r.item.k, 'sub/b.bin': new Uint8Array([1, 2, 3]) } }) };",
    'xs.json': '[{"k":"one"},{"k":"two"}]',
    Sitefile: 'FROM scratch\nPLUGIN many FROM ./plugins/many.mjs\nIMPORT xs FROM ./xs.json AS json\nTRANSFORM WITH many TO /o/${x.k}/ FOR x IN xs\nEXPORT /',
  });
  const a = await t.go();
  const b = await t.go();
  assert.equal(a.transformStats.writes, 2);
  assert.equal(b.transformStats.hits, 2);
  assert.equal(b.transformStats.pluginsSpawned, 0);
  assert.equal(b.output.digest(), a.output.digest());
  assert.equal(text(b.output.get('/o/two/a.txt')?.bytes), 'two');
  assert.deepEqual([...(b.output.get('/o/one/sub/b.bin')?.bytes ?? [])], [1, 2, 3]);
});

test('no cacheDir (--no-cache) neither reads nor writes', async () => {
  const t = await setup();
  await t.go();
  const r = await t.go({ cacheDir: undefined as unknown as string });
  assert.equal(r.transformStats.hits, 0);
  assert.equal(r.transformStats.pluginsSpawned, 1);
  const before = (await new TransformCache(t.cacheDir).info()).entries;
  await write(t.ctx, { 'a.txt': 'different' });
  await build({ contextDir: t.ctx, lockfile: false, allowAllPlugins: true });
  assert.equal((await new TransformCache(t.cacheDir).info()).entries, before, 'nothing was written');
});

test('a plugin can opt out with `export const cache = false`', async () => {
  const t = await setup({ 'plugins/p.mjs': PLUGIN + '\nexport const cache = false;' });
  const a = await t.go();
  const b = await t.go();
  assert.equal(a.transformStats.writes, 0);
  assert.equal(b.transformStats.pluginsSpawned, 1);
  assert.equal(b.transformStats.hits, 0);
  assert.equal((await new TransformCache(t.cacheDir).info()).entries, 0);
  // also as a default-exported object
  await write(t.ctx, { 'plugins/p.mjs': "export default { apiVersion: 1, cache: false, transform: () => 'x' };" });
  const c = await t.go();
  assert.equal(c.transformStats.writes, 0);
});

test('a JS plugin that does not export cache is cached with one warning; cache = true is silent (#160)', async () => {
  const two = 'FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a.txt WITH p TO /a.json OPTIONS n=1\nTRANSFORM ./a.txt WITH p TO /b.json OPTIONS n=2\nEXPORT /';
  const t = await setup({ Sitefile: two });
  const warned = (w: string[]): string[] => w.filter((m) => m.includes('does not export cache'));
  const w1: string[] = [];
  const a = await t.go({ onWarning: (m) => w1.push(m) });
  assert.equal(a.transformStats.writes, 2, 'still cached');
  assert.deepEqual(warned(w1), [
    'PLUGIN p does not export cache, so its results are reused as if it were deterministic: export const cache = true to confirm, or cache = false if a result can depend on the clock, randomness or an earlier call',
  ]);
  // A hit starts no plugin, so there is nothing to warn about.
  const w2: string[] = [];
  assert.equal((await t.go({ onWarning: (m) => w2.push(m) })).transformStats.hits, 2);
  assert.deepEqual(warned(w2), []);
  // Nothing is reused without a cache (or memo): no warning.
  const w3: string[] = [];
  await build({ contextDir: t.ctx, lockfile: false, allowAllPlugins: true, onWarning: (m) => w3.push(m) });
  assert.deepEqual(warned(w3), []);
  // cache = true: cached, silent; also on a default-exported object.
  for (const plugin of [PLUGIN + '\nexport const cache = true;', "export default { apiVersion: 1, cache: true, transform: (r) => r.dest };"]) {
    const u = await setup({ Sitefile: two, 'plugins/p.mjs': plugin });
    const w: string[] = [];
    const r = await u.go({ onWarning: (m) => w.push(m) });
    assert.equal(r.transformStats.writes, 2);
    assert.deepEqual(warned(w), []);
  }
  // cache = false: never cached, and no warning either.
  const f = await setup({ 'plugins/p.mjs': PLUGIN + '\nexport const cache = false;' });
  const w4: string[] = [];
  await f.go({ onWarning: (m) => w4.push(m) });
  assert.deepEqual(warned(w4), []);
  // Anything else is a load error.
  const bad = await setup({ 'plugins/p.mjs': PLUGIN + "\nexport const cache = 'yes';" });
  await assert.rejects(bad.go(), (e: Error) => e.message.includes('PLUGIN p: cache must be true or false, got "yes"'));
});

test('module state carries over between calls of one build (documented, #160)', async () => {
  const counter = "let n = 0; export const cache = false; export function transform() { return String(++n); }";
  const t = await setup({ 'plugins/p.mjs': counter, Sitefile: 'FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM WITH p TO /a.txt\nTRANSFORM WITH p TO /b.txt\nEXPORT /' });
  const r = await t.go();
  assert.deepEqual([text(r.output.get('/a.txt')?.bytes), text(r.output.get('/b.txt')?.bytes)], ['1', '2']);
});

test('a damaged entry or blob is ignored, rewritten, and then hits again', async () => {
  const t = await setup();
  await t.go();
  const entries = path.join(t.cacheDir, 'entries');
  const blobs = path.join(t.cacheDir, 'blobs');

  // 1. a blob whose bytes no longer match its digest
  const [blob] = await readdir(blobs);
  await writeFile(path.join(blobs, blob!), 'tampered');
  let r = await t.go();
  assert.equal(r.transformStats.corrupt, 1);
  assert.equal(r.transformStats.misses, 1);
  assert.equal(r.transformStats.writes, 1);
  assert.equal(JSON.parse(text(r.output.get('/a.json')?.bytes)).t, 'alpha', 'the rebuilt result is correct, not the tampered bytes');
  r = await t.go();
  assert.equal(r.transformStats.hits, 1);

  // 2. a manifest that is not JSON
  const [entry] = await readdir(entries);
  await writeFile(path.join(entries, entry!), '{not json');
  r = await t.go();
  assert.equal(r.transformStats.corrupt, 1);
  assert.equal(r.transformStats.writes, 1);

  // 3. a missing blob
  await rm(path.join(blobs, (await readdir(blobs))[0]!));
  r = await t.go();
  assert.equal(r.transformStats.corrupt, 1);
  assert.equal((await t.go()).transformStats.hits, 1);

  // 4. a truncated blob (right digest name, wrong size)
  await writeFile(path.join(blobs, (await readdir(blobs))[0]!), '');
  assert.equal((await t.go()).transformStats.corrupt, 1);
  assert.equal((await t.go()).transformStats.hits, 1);
});

test('an unwritable cache directory does not fail the build', async () => {
  const t = await setup();
  const blocker = path.join(await dir('sitefile-cache-block-'), 'file');
  await writeFile(blocker, 'not a dir');
  const r = await t.go({ cacheDir: path.join(blocker, 'cache') });
  assert.equal(r.transformStats.writes, 0);
  assert.equal(JSON.parse(text(r.output.get('/a.json')?.bytes)).t, 'alpha');
});

test('tool results are cached by binary identity + argv', async () => {
  const d = await dir('sitefile-cache-tool-');
  const tool = path.join(d, 'cpx');
  await writeFile(tool, `#!${process.execPath}\nrequire('node:fs').copyFileSync(process.argv[2], process.argv[3]);\n`);
  await chmod(tool, 0o755);
  const sf = (extra: string): string => `FROM scratch\nTOOL cpx\nTRANSFORM ./a.txt WITH cpx TO /b.txt ARGS ${extra} {input} {output}` + '\nEXPORT /';
  const t = await setup({ Sitefile: sf('') });
  const opts = { allowExec: [`cpx=${tool}`] };
  const a = await t.go(opts);
  assert.equal(a.transformStats.toolRuns, 1);
  const b = await t.go(opts);
  assert.equal(b.transformStats.toolRuns, 0);
  assert.equal(b.transformStats.hits, 1);
  assert.equal(text(b.output.get('/b.txt')?.bytes), 'alpha');
  // a different binary at the same name (size/mtime change) misses
  await writeFile(tool, `#!${process.execPath}\nrequire('node:fs').copyFileSync(process.argv[2], process.argv[3]); // v2\n`);
  await utimes(tool, new Date(), new Date(Date.now() + 5000));
  assert.equal((await t.go(opts)).transformStats.toolRuns, 1);
  // different argv misses
  await write(t.ctx, { Sitefile: sf('"$(x)"') });
  await assert.rejects(t.go(opts)); // cpx would copy "$(x)" as the input: fails, and is not cached
  assert.equal((await new TransformCache(t.cacheDir).info()).entries, 2, 'one entry per binary version; the failed run wrote nothing');
});

test('canonicalJson ignores key order; the key covers every part', () => {
  assert.equal(canonicalJson({ b: 1, a: { d: [1, { z: 1, y: undefined }], c: null } }), canonicalJson({ a: { c: null, d: [1, { z: 1 }] }, b: 1 }));
  const base = { code: { plugin: 'x' }, request: { dest: '/a', options: { n: '1' } } };
  const k = cacheKey(base);
  assert.match(k, /^[0-9a-f]{64}$/);
  assert.equal(cacheKey({ request: base.request, code: base.code }), k);
  assert.notEqual(cacheKey({ ...base, code: { plugin: 'y' } }), k);
  assert.notEqual(cacheKey({ ...base, request: { dest: '/a', options: { n: '2' } } }), k);
});

test('prune removes everything or only entries unused for N days; info counts', async () => {
  const t = await setup({ 'b.txt': 'beta', Sitefile: SITEFILE + '' });
  await write(t.ctx, { Sitefile: 'FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a.txt WITH p TO /a.json\nTRANSFORM ./b.txt WITH p TO /b.json\nEXPORT /' });
  await t.go();
  const cache = new TransformCache(t.cacheDir);
  assert.deepEqual({ ...(await cache.info()), dir: '' }, { dir: '', entries: 2, blobs: 2, bytes: (await cache.info()).bytes });
  // age one entry by 10 days
  const entries = path.join(t.cacheDir, 'entries');
  const [old] = await readdir(entries);
  const past = new Date(Date.now() - 10 * 86_400_000);
  await utimes(path.join(entries, old!), past, past);
  const r = await cache.prune(7 * 86_400_000);
  assert.deepEqual([r.entries, r.blobs], [1, 1]);
  assert.equal((await cache.info()).entries, 1);
  const all = await cache.prune();
  assert.deepEqual([all.entries, all.blobs], [1, 1]);
  assert.deepEqual(await cache.info(), { dir: t.cacheDir, entries: 0, blobs: 0, bytes: 0 });
});

test('entries record their PLUGIN or TOOL; usage and prune per plugin; older entries still read', async () => {
  const d = await dir('sitefile-cache-plugin-');
  const cache = new TransformCache(d);
  const hex = (s: string): string => createHash('sha256').update(s).digest('hex');
  await cache.put('k1', { kind: 'file', bytes: new TextEncoder().encode('alpha') }, 'shout');
  // Byte for byte what the Rust port writes (rust/src/plugins/cache.rs, the same test).
  assert.equal(await readFile(path.join(d, 'entries', 'k1.json'), 'utf8'), `{"version":1,"kind":"file","files":[["","${hex('alpha')}",5]],"plugin":"shout"}`);
  await cache.put('k2', { kind: 'file', bytes: new TextEncoder().encode('beta') });
  assert.ok(!(await readFile(path.join(d, 'entries', 'k2.json'), 'utf8')).includes('plugin'));
  assert.equal(text(((await cache.get('k2', 'shout')) as { bytes: Uint8Array }).bytes), 'beta');
  await cache.put('k3', { kind: 'file', bytes: new TextEncoder().encode('alpha') }, 'upper');
  await cache.put('k4', { kind: 'files', files: [['x', new TextEncoder().encode('xyz')]] }, 'upper');
  assert.deepEqual(await cache.usageByPlugin(), [
    { plugin: undefined, entries: 1, blobs: 1, bytes: 4 },
    { plugin: 'shout', entries: 1, blobs: 1, bytes: 5 },
    { plugin: 'upper', entries: 2, blobs: 2, bytes: 8 },
  ]);
  // A blob another plugin's entry still uses is kept; unnamed entries are kept.
  assert.deepEqual(await cache.prunePlugin('upper'), { entries: 2, blobs: 1, bytes: 3 });
  assert.ok((await cache.get('k1')) && (await cache.get('k2')));
  assert.equal(await cache.get('k3'), undefined);
  assert.equal((await cache.prunePlugin('shout', 3_600_000)).entries, 0);
  assert.deepEqual(await cache.prunePlugin('nobody'), { entries: 0, blobs: 0, bytes: 0 });
  // A blob whose entry has not landed yet (another build writing it) is not touched.
  await writeFile(path.join(d, 'blobs', hex('inflight')), 'inflight');
  assert.deepEqual([(await cache.prunePlugin('shout')).entries], [1]);
  assert.ok(await stat(path.join(d, 'blobs', hex('inflight'))));
});

test('CLI: cache prune --plugin deletes only that plugin\'s results', async () => {
  const home = await tmpDir();
  const ctx = await dir('sitefile-cache-cli-plugin-');
  await write(ctx, {
    'a.txt': 'alpha',
    'b.txt': 'beta',
    Sitefile: 'FROM scratch\nTOOL copya\nTOOL copyb\nTRANSFORM ./a.txt WITH copya TO /a.txt ARGS {input} {output}\nTRANSFORM ./b.txt WITH copyb TO /b.txt ARGS {input} {output}\nEXPORT /',
  });
  const b = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--allow-exec', 'copya=/bin/cp', '--allow-exec', 'copyb=/bin/cp']);
  assert.equal(b.code, 0, b.stderr);
  const pr = await runCli(home, ['cache', 'prune', '--plugin', 'copya']);
  assert.equal(pr.code, 0, pr.stderr);
  assert.equal(pr.stdout, 'Deleted 1 entry and 1 blob\nTotal reclaimed space: 5B\n');
  assert.equal(JSON.parse((await runCli(home, ['cache', 'info', '--json'])).stdout).entries, 1);
  const bad = await runCli(home, ['cache', 'prune', '--plugin', 'bad-name']);
  assert.equal(bad.code, 2);
  assert.match(bad.stderr, /--plugin expects a PLUGIN or TOOL name \(an identifier\), got 'bad-name'/);
  const conflict = await runCli(home, ['cache', 'prune', '--plugin', 'copyb', '--max-size', '1M']);
  assert.equal(conflict.code, 2);
  assert.match(conflict.stderr, /--plugin and --max-size conflict/);
});

test('CLI: builds cache under $SITEFILE_HOME/cache/transform; --no-cache skips it; cache info / prune', async () => {
  const home = await tmpDir();
  const ctx = await dir('sitefile-cache-cli-');
  await write(ctx, { 'plugins/p.mjs': PLUGIN, 'plugins/helper.txt': 'h', 'a.txt': 'alpha', Sitefile: SITEFILE });
  const out = path.join(ctx, 'dist');

  const nc = await runCli(home, ['build', ctx, '-o', out, '--no-cache', '--allow-plugin', 'p']);
  assert.equal(nc.code, 0, nc.stderr);
  await assert.rejects(stat(path.join(home, 'cache')), 'no cache dir with --no-cache');

  const first = await runCli(home, ['build', ctx, '-o', out, '--force', '--allow-plugin', 'p']);
  assert.equal(first.code, 0, first.stderr);
  const dirEntries = await readdir(path.join(home, 'cache', 'transform', 'entries'));
  assert.equal(dirEntries.length, 1);

  const info = await runCli(home, ['cache', 'info', '--json']);
  assert.equal(info.code, 0, info.stderr);
  const parsed = JSON.parse(info.stdout) as { dir: string; entries: number; blobs: number; bytes: number };
  assert.equal(parsed.entries, 1);
  assert.equal(parsed.dir, path.join(await (await import('node:fs/promises')).realpath(home), 'cache', 'transform'));
  assert.match((await runCli(home, ['cache', 'info'])).stdout, /Entries: 1/);

  const bad = await runCli(home, ['cache', 'prune', '--older-than', 'x']);
  assert.equal(bad.code, 2);
  assert.equal((await runCli(home, ['cache', 'bogus'])).code, 2);
  const pr = await runCli(home, ['cache', 'prune']);
  assert.equal(pr.code, 0, pr.stderr);
  assert.match(pr.stdout, /Deleted 1 entry and 1 blob/);
  assert.equal(JSON.parse((await runCli(home, ['cache', 'info', '--json'])).stdout).entries, 0);
  assert.ok((await readFile(path.join(out, 'a.json'), 'utf8')).includes('alpha'));
});
