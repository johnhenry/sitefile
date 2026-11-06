import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { Store } from '../src/store.js';
import { makeFile, Tree } from '../src/tree.js';
import { makeSite, repoRoot, runCli, tmpDir } from './helpers/cli.js';
import { startMockRegistry, type MockRegistry } from './helpers/mock-registry.js';

const enc = (s: string): Uint8Array => new TextEncoder().encode(s);
const text = (t: Tree, p: string): string => new TextDecoder().decode(t.get(p)!.bytes);
const escape = (s: string): string => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** A theme image in the store at `home`, tagged `tag` (rooted at /srv/www, as `sitefile build -t` makes it). */
async function storeTheme(home: string, tag: string, files: Record<string, string>): Promise<string> {
  const store = await Store.open(home);
  const tree = Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 'test')] as const));
  const { id } = await store.addImage(tree, { root: '/srv/www' });
  const { parseLocalRef } = await import('../src/ref.js');
  await store.tag(id, parseLocalRef(tag));
  return id;
}

/** A Sitefile theme image on the mock registry: one layer under /srv/www with the dev.sitefile.root label. */
function seedTheme(reg: MockRegistry, repo: string, tag: string, css: string): string {
  return reg.seedImage(repo, tag, {
    layers: [{ '/srv/www/theme.css': css, '/srv/www/layout/index.html': '<main>theme</main>' }],
    config: { Labels: { 'dev.sitefile.root': '/srv/www' } },
  }).manifestDigest;
}

test('FROM image: a local-store image (no registry host) seeds the stage; nothing is locked', async () => {
  const home = await tmpDir();
  const id = await storeTheme(home, 'theme:1', { '/theme.css': 'a{}', '/index.html': 'theme index' });
  const ctx = await makeSite({}, 'FROM image theme:1 AS theme\nCOPY --overwrite ./public/index.html /index.html\nEXPORT /\n');
  const store = await Store.open(home);
  const r = await build({ contextDir: ctx, store });
  assert.deepEqual(r.output.entries().map(([p]) => p).sort(), ['/index.html', '/theme.css']);
  assert.equal(text(r.output, '/theme.css'), 'a{}');
  assert.match(r.output.get('/theme.css')!.origin, new RegExp(`^image theme:1@${id}:`));
  assert.deepEqual(r.images, [{ name: 'theme:1', digest: id, line: 1, local: true }]);
  assert.equal(r.lockfileWritten, false);
  await assert.rejects(fs.stat(path.join(ctx, 'Sitefile.lock')));
  // without --overwrite, a different file at the same path is a conflict naming the image
  const clash = await makeSite({}, 'FROM image theme:1\nCOPY ./public/index.html /index.html\nEXPORT /\n');
  await assert.rejects(build({ contextDir: clash, store }), (e: unknown) => isSitefileError(e, 'E_CONFLICT') && /image theme:1@sha256:/.test(e.message));
  // by image ID and by name@digest, AT picks a directory inside the image
  for (const ref of [id.slice(7, 19), `theme@${id}`]) {
    const at = await makeSite({}, `FROM image ${ref} AT /srv\nEXPORT /\n`);
    assert.deepEqual((await build({ contextDir: at, store })).output.entries().map(([p]) => p).sort(), ['/www/index.html', '/www/theme.css']);
  }
  // a missing local image is E_NOT_FOUND at the FROM line
  const missing = await makeSite({}, 'ARG V=9\nSTAGE s FROM image theme:${V}\nEXPORT /\n');
  await assert.rejects(build({ contextDir: missing, store }), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_NOT_FOUND'), String(e));
    assert.equal(e.span?.line, 2);
    assert.match(e.message, /STAGE s FROM image theme:9: no such image in the local store; build it with -t theme:9/);
    return true;
  });
});

test('FROM image: invalid references and AT paths fail validation', async () => {
  const home = await tmpDir();
  for (const [sf, code, re] of [
    ['FROM image Bad:1\nEXPORT /\n', 'E_VALIDATION', /Sitefile:1:12: FROM image: invalid image reference 'Bad:1'/],
    ['FROM image t:1 AT rel\nEXPORT /\n', 'E_PATH', /image directory 'rel' must be an absolute artifact path/],
    ['FROM image t:1 AT /a/../b\nEXPORT /\n', 'E_PATH', /must not contain '\.\.'/],
    ['FROM image ${NOPE}\nEXPORT /\n', 'E_UNDEFINED', /undefined variable \$\{NOPE\}/],
  ] as const) {
    const ctx = await makeSite({}, sf);
    const r = await runCli(home, ['validate', ctx]);
    assert.equal(r.code, 1, sf);
    assert.match(r.stderr, new RegExp(`error\\[${code}\\]`), sf);
    assert.match(r.stderr.replace(/^error\[\w+\]: /, ''), re, r.stderr);
  }
});

test('FROM image: a registry tag is resolved once, locked, and fetched by digest', async () => {
  const reg = await startMockRegistry();
  try {
    const d1 = seedTheme(reg, 'me/theme', '1', 'one{}');
    const home = await tmpDir();
    const ref = `${reg.host}/me/theme:1`;
    const ctx = await makeSite({}, `STAGE theme FROM image ${ref}\nFROM theme\nCOPY ./public/about/ /about/\nEXPORT /\n`);
    const out = () => path.join(ctx, `o${Math.random()}`);

    // --frozen-lockfile with no entry, and --strict-pins, fail before any registry request
    const before = reg.requests.length;
    const frozen = await runCli(home, ['build', ctx, '-o', out(), '--frozen-lockfile']);
    assert.equal(frozen.code, 1);
    assert.match(frozen.stderr, new RegExp(`error\\[E_LOCK_MISSING\\]: \\S*Sitefile:1:\\d+: STAGE theme FROM image ${escape(ref)}: '${escape(ref)}' is not in the lockfile`));
    for (const cmd of ['build', 'validate', 'graph']) {
      const strict = await runCli(home, [cmd, ctx, ...(cmd === 'build' ? ['-o', out()] : []), '--strict-pins']);
      assert.equal(strict.code, 1, cmd);
      assert.match(strict.stderr, new RegExp(`error\\[E_UNPINNED\\]: \\S*Sitefile:1:\\d+: STAGE theme FROM image ${escape(ref)}: '${escape(ref)}' is not pinned`));
    }
    assert.equal(reg.requests.length, before, 'the registry was not contacted');

    const first = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist')]);
    assert.equal(first.code, 0, first.stderr);
    assert.equal(await fs.readFile(path.join(ctx, 'dist', 'theme.css'), 'utf8'), 'one{}');
    assert.equal(await fs.readFile(path.join(ctx, 'dist', 'about', 'index.html'), 'utf8'), '<h1>About</h1>');
    const lock = JSON.parse(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8'));
    assert.deepEqual(lock.images, { [ref]: { digest: d1 } });

    // the tag moves: builds keep the locked digest; --update-lock takes the new one
    const d2 = seedTheme(reg, 'me/theme', '1', 'two{}');
    assert.notEqual(d1, d2);
    const fresh = await tmpDir();
    for (const h of [home, fresh]) {
      const again = await runCli(h, ['build', ctx, '-o', path.join(ctx, 'dist'), '--frozen-lockfile', '--strict-pins']);
      assert.equal(again.code, 0, again.stderr);
      assert.equal(await fs.readFile(path.join(ctx, 'dist', 'theme.css'), 'utf8'), 'one{}');
    }
    const od = await runCli(home, ['outdated', ctx, '--json']);
    const rows = JSON.parse(od.stdout).inputs;
    assert.deepEqual(rows.map((i: { kind: string; name: string; status: string; pin: string; latest: string }) => [i.kind, i.name, i.status, i.pin, i.latest]), [['image', 'theme', 'changed', d1, d2]]);
    const upd = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--update-lock']);
    assert.equal(upd.code, 0, upd.stderr);
    assert.equal(await fs.readFile(path.join(ctx, 'dist', 'theme.css'), 'utf8'), 'two{}');
    assert.equal(JSON.parse(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8')).images[ref].digest, d2);

    // a --target build that stops before the image stage keeps its lockfile entry
    const t = await makeSite({}, `STAGE early\nCOPY ./public/ /\nSTAGE theme FROM image ${ref}\nEXPORT /\n`);
    assert.equal((await runCli(home, ['build', t, '-o', path.join(t, 'a')])).code, 0);
    const lockBefore = await fs.readFile(path.join(t, 'Sitefile.lock'), 'utf8');
    assert.equal((await runCli(home, ['build', t, '-o', path.join(t, 'b'), '--target', 'early'])).code, 0);
    assert.equal(await fs.readFile(path.join(t, 'Sitefile.lock'), 'utf8'), lockBefore);

    // graph and why
    const g = JSON.parse((await runCli(home, ['graph', ctx, '--json'])).stdout);
    assert.ok(g.nodes.some((n: { id: string; kind: string }) => n.id === `image:${ref}` && n.kind === 'image'));
    assert.ok(g.edges.some((e: { from: string; to: string; kind: string }) => e.from === `image:${ref}` && e.to === 'stage:theme' && e.kind === 'base'));
    const why = await runCli(home, ['why', '/theme.css', ctx]);
    assert.equal(why.code, 0, why.stderr);
    assert.match(why.stdout, new RegExp(`STAGE theme FROM image|FROM image ${escape(ref)}`));
    assert.match(why.stdout, new RegExp(`image ${escape(ref)}@${d2}:`));
  } finally {
    await reg.close();
  }
});

test('FROM image: a digest pin needs no lockfile; a non-Sitefile image needs AT; vendor and provenance include it', async () => {
  const reg = await startMockRegistry();
  try {
    const d = seedTheme(reg, 'me/theme', '3', 'three{}');
    const foreign = reg.seedImage('lib/nginx', '1', { layers: [{ '/etc/motd': 'hi', '/usr/share/nginx/html/index.html': 'nginx' }] }).manifestDigest;
    const multi = reg.seedImage('lib/multi', '1', { layers: [{ '/a': 'a' }, { '/b': 'b' }] }).manifestDigest;
    const home = await tmpDir();
    const pinned = await makeSite({}, `FROM image ${reg.host}/me/theme@${d}\nEXPORT /\n`);
    const r = await runCli(home, ['build', pinned, '-o', path.join(pinned, 'dist'), '--strict-pins', '--no-lockfile']);
    assert.equal(r.code, 0, r.stderr);
    assert.equal(await fs.readFile(path.join(pinned, 'dist', 'theme.css'), 'utf8'), 'three{}');

    // an unlabelled single-layer image is rooted at '/'; AT picks the web root
    const ng = await makeSite({}, `FROM image ${reg.host}/lib/nginx:1@${foreign} AT /usr/share/nginx/html\nEXPORT /\n`);
    assert.equal((await runCli(home, ['build', ng, '-o', path.join(ng, 'dist')])).code, 0);
    assert.deepEqual(await fs.readdir(path.join(ng, 'dist')), ['index.html']);
    const mu = await makeSite({}, `FROM image ${reg.host}/lib/multi:1@${multi}\nEXPORT /\n`);
    const bad = await runCli(home, ['build', mu, '-o', path.join(mu, 'dist')]);
    assert.equal(bad.code, 1);
    assert.match(bad.stderr, /error\[E_STORE\]: \S*Sitefile:1:\d+: image .*lib\/multi:1@sha256:[0-9a-f]+: not a Sitefile image/);

    // vendor: the image's blobs go into the vendor directory; a build with an empty store uses them
    const v = await makeSite({}, `FROM image ${reg.host}/me/theme:3\nEXPORT / AS oci\n`);
    const vr = await runCli(home, ['vendor', v]);
    assert.equal(vr.code, 0, vr.stderr);
    const manifest = JSON.parse(await fs.readFile(path.join(v, 'vendor', 'sitefile', 'manifest.json'), 'utf8'));
    assert.deepEqual(Object.keys(manifest.images), [`${reg.host}/me/theme:3`]);
    await reg.close();
    const offline = await runCli(await tmpDir(), ['build', v, '-o', path.join(v, 'site.oci.tar'), '--frozen-lockfile', '--provenance']);
    assert.equal(offline.code, 0, offline.stderr);
  } finally {
    await reg.close().catch(() => undefined);
  }
});

test('examples/theme as an image, consumed by examples/themed-site', async () => {
  const home = await tmpDir();
  const out = await tmpDir();
  const examples = path.join(repoRoot, 'examples');
  const theme = await runCli(home, ['build', path.join(examples, 'theme'), '-t', 'theme:1', '-o', path.join(out, 'theme')]);
  assert.equal(theme.code, 0, theme.stderr);
  const site = await runCli(home, ['build', path.join(examples, 'themed-site'), '-o', path.join(out, 'site')]);
  assert.equal(site.code, 0, site.stderr);
  const html = await fs.readFile(path.join(out, 'site', 'index.html'), 'utf8');
  assert.match(html, /<title>A themed site<\/title>/);
  assert.match(html, /href="\/assets\/theme.css"/);
  assert.ok((await fs.stat(path.join(out, 'site', 'assets', 'theme.css'))).isFile());
  await assert.rejects(fs.stat(path.join(examples, 'themed-site', 'Sitefile.lock')));
});
