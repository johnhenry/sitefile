import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm, mkdir, writeFile, symlink, link, chmod, realpath } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { LocalContext } from '../src/resources/local.js';
import { bytesToResource, verifyIntegrity } from '../src/resources/convert.js';
import { sha256 } from '../src/digest.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';

/** A temp dir with `ctx/` (the build context) and `outside/` next to it. */
async function withTemp(fn: (ctx: string, outside: string) => Promise<void>): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), 'sitefile-local-'));
  try {
    const ctx = join(dir, 'ctx');
    const outside = join(dir, 'outside');
    await mkdir(ctx);
    await mkdir(outside);
    await writeFile(join(outside, 'secret.txt'), 'secret');
    await fn(ctx, outside);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

async function rejectsWith(p: Promise<unknown>, code: ErrorCode): Promise<void> {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    return true;
  });
}

const text = (b: Uint8Array) => Buffer.from(b).toString();

test('open resolves the real root', () =>
  withTemp(async (ctx) => {
    const c = await LocalContext.open(ctx);
    assert.equal(c.root, await realpath(ctx));
    await rejectsWith(LocalContext.open(join(ctx, 'nope')), 'E_NOT_FOUND');
  }));

test('stat and readFile', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'src'));
    await writeFile(join(ctx, 'src', 'a.txt'), 'A');
    await writeFile(join(ctx, 'run.sh'), '#!/bin/sh');
    await chmod(join(ctx, 'run.sh'), 0o700);
    await chmod(join(ctx, 'src', 'a.txt'), 0o600);
    const c = await LocalContext.open(ctx);
    assert.equal(await c.stat(''), 'directory');
    assert.equal(await c.stat('src'), 'directory');
    assert.equal(await c.stat('src/a.txt'), 'file');
    await rejectsWith(c.stat('missing'), 'E_NOT_FOUND');
    await rejectsWith(c.stat('src/a.txt/x'), 'E_NOT_FOUND');

    const f = await c.readFile('src/a.txt');
    assert.equal(text(f.bytes), 'A');
    assert.equal(f.mode, 0o644);
    assert.equal(f.basename, 'a.txt');
    assert.equal(f.provenance.uri, './src/a.txt');
    assert.equal(f.provenance.digest, sha256('A'));
    assert.equal((await c.readFile('run.sh')).mode, 0o755);
    await rejectsWith(c.readFile('src'), 'E_UNSUPPORTED_FILE');
  }));

test('symlinks escaping the context are rejected (files and dirs)', () =>
  withTemp(async (ctx, outside) => {
    await symlink(join(outside, 'secret.txt'), join(ctx, 'leak.txt'));
    await symlink(outside, join(ctx, 'leakdir'));
    await symlink('../outside/secret.txt', join(ctx, 'rel-leak'));
    const c = await LocalContext.open(ctx);
    await rejectsWith(c.stat('leak.txt'), 'E_PATH_ESCAPE');
    await rejectsWith(c.readFile('leak.txt'), 'E_PATH_ESCAPE');
    await rejectsWith(c.readFile('rel-leak'), 'E_PATH_ESCAPE');
    await rejectsWith(c.stat('leakdir'), 'E_PATH_ESCAPE');
    await rejectsWith(c.readFile('leakdir/secret.txt'), 'E_PATH_ESCAPE');
    await rejectsWith(c.readTree('leakdir', 'test'), 'E_PATH_ESCAPE');
  }));

test('readTree fails naming a link that escapes', () =>
  withTemp(async (ctx, outside) => {
    await mkdir(join(ctx, 'site'));
    await writeFile(join(ctx, 'site', 'ok.txt'), 'ok');
    await symlink(outside, join(ctx, 'site', 'evil'));
    const c = await LocalContext.open(ctx);
    await assert.rejects(c.readTree('site', 'test'), (e: unknown) => {
      assert.ok(isSitefileError(e, 'E_PATH_ESCAPE'));
      assert.match(e.message, /site\/evil/);
      return true;
    });
  }));

test('symlinks inside the context are followed', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'real'));
    await writeFile(join(ctx, 'real', 'x.txt'), 'X');
    await mkdir(join(ctx, 'site'));
    await symlink('../real/x.txt', join(ctx, 'site', 'x-link.txt'));
    await symlink('../real', join(ctx, 'site', 'dir-link'));
    const c = await LocalContext.open(ctx);
    assert.equal(text((await c.readFile('site/x-link.txt')).bytes), 'X');
    assert.equal((await c.readFile('site/x-link.txt')).basename, 'x-link.txt');
    const t = await c.readTree('site', 'COPY');
    assert.deepEqual(
      t.tree.entries().map(([p]) => p),
      ['/dir-link/x.txt', '/x-link.txt'],
    );
  }));

test('directory symlink cycles are skipped', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'a', 'b'), { recursive: true });
    await writeFile(join(ctx, 'a', 'b', 'f.txt'), 'f');
    await symlink('..', join(ctx, 'a', 'b', 'up'));
    await symlink('.', join(ctx, 'a', 'self'));
    const c = await LocalContext.open(ctx);
    const t = await c.readTree('a', 'COPY');
    assert.deepEqual(t.tree.entries().map(([p]) => p), ['/b/f.txt']);
  }));

test('ignore list is honoured while walking', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, '.git'));
    await writeFile(join(ctx, '.git', 'HEAD'), 'ref');
    await mkdir(join(ctx, 'dist', 'x'), { recursive: true });
    await writeFile(join(ctx, 'dist', 'x', 'out.html'), 'o');
    await mkdir(join(ctx, 'sub', '.git'), { recursive: true });
    await writeFile(join(ctx, 'sub', '.git', 'keep'), 'k');
    await writeFile(join(ctx, 'index.html'), 'i');
    await symlink('.git', join(ctx, 'gitlink'));

    const def = await LocalContext.open(ctx);
    const d = await def.readTree('', 'COPY');
    assert.deepEqual(d.tree.entries().map(([p]) => p), ['/dist/x/out.html', '/index.html', '/sub/.git/keep']);

    const c = await LocalContext.open(ctx, { ignore: ['.git', 'dist/x'] });
    const t = await c.readTree('', 'COPY');
    assert.deepEqual(t.tree.entries().map(([p]) => p), ['/index.html', '/sub/.git/keep']);
    assert.equal(t.provenance.digest, t.tree.digest());
    assert.equal(t.provenance.uri, './');
    // The walk under dist skips the ignored subdir too.
    const sub = await c.readTree('dist', 'COPY');
    assert.equal(sub.tree.size, 0);
  }));

test('tree modes are normalized and output is deterministic', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'site', 'z'), { recursive: true });
    for (const n of ['b.txt', 'a.txt', 'C.txt', 'z/y.txt']) await writeFile(join(ctx, 'site', n), n);
    await chmod(join(ctx, 'site', 'a.txt'), 0o711);
    await chmod(join(ctx, 'site', 'b.txt'), 0o600);
    const c = await LocalContext.open(ctx);
    const t1 = await c.readTree('site', 'Sitefile:3 COPY');
    const t2 = await c.readTree('site', 'Sitefile:3 COPY');
    assert.deepEqual(t1.tree.entries().map(([p]) => p), ['/C.txt', '/a.txt', '/b.txt', '/z/y.txt']);
    assert.equal(t1.tree.get('/a.txt')?.mode, 0o755);
    assert.equal(t1.tree.get('/b.txt')?.mode, 0o644);
    assert.equal(t1.tree.get('/a.txt')?.origin, 'Sitefile:3 COPY');
    assert.equal(t1.provenance.digest, t2.provenance.digest);
    assert.equal(t1.provenance.uri, './site');
    await rejectsWith(c.readTree('site/a.txt', 'x'), 'E_UNSUPPORTED_FILE');
  }));

test('fifos and sockets are unsupported', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'site'));
    let made = false;
    try {
      execFileSync('mkfifo', [join(ctx, 'site', 'pipe')]);
      made = true;
    } catch {
      // mkfifo unavailable: fall back to a unix socket
    }
    let server: ReturnType<typeof createServer> | undefined;
    if (!made) {
      server = createServer();
      await new Promise<void>((r) => server!.listen(join(ctx, 'site', 'sock'), r));
    }
    try {
      const c = await LocalContext.open(ctx);
      await rejectsWith(c.readTree('site', 'COPY'), 'E_UNSUPPORTED_FILE');
      await rejectsWith(c.stat(made ? 'site/pipe' : 'site/sock'), 'E_UNSUPPORTED_FILE');
    } finally {
      server?.close();
    }
  }));

test('bytesToResource and verifyIntegrity', async () => {
  const info = { uri: 'https://x/a.json', resolvedUrl: 'https://cdn/a.json', basename: 'a.json' };
  const bytes = new TextEncoder().encode('{"a":[1,2]}');
  const f = await bytesToResource(bytes, 'file', { ...info, mode: 0o700 });
  assert.equal(f.kind, 'file');
  assert.ok(f.kind === 'file' && f.mode === 0o755 && f.basename === 'a.json');
  assert.equal(f.provenance.resolvedUrl, 'https://cdn/a.json');
  assert.equal(f.provenance.digest, sha256(bytes));
  const j = await bytesToResource(bytes, 'json', info);
  assert.ok(j.kind === 'value' && j.format === 'json');
  assert.deepEqual(j.value, { a: [1, 2] });
  const t = await bytesToResource(bytes, 'text', { uri: './a', basename: 'a' });
  assert.ok(t.kind === 'value' && t.format === 'text' && t.value === '{"a":[1,2]}');
  assert.equal('resolvedUrl' in t.provenance, false);
  await assert.rejects(() => bytesToResource(new Uint8Array([0xff, 0xfe]), 'text', info), (e) => isSitefileError(e, 'E_ENCODING'));
  await assert.rejects(() => bytesToResource(new Uint8Array([0xc3]), 'json', info), (e) => isSitefileError(e, 'E_ENCODING'));
  await assert.rejects(() => bytesToResource(new TextEncoder().encode('{nope'), 'json', info), (e) => isSitefileError(e, 'E_TYPE'));

  verifyIntegrity(bytes, sha256(bytes), info.uri);
  const wrong = 'sha256:' + '0'.repeat(64);
  assert.throws(() => verifyIntegrity(bytes, wrong, info.uri), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_INTEGRITY'));
    assert.ok(e.message.includes(wrong) && e.message.includes(sha256(bytes)));
    return true;
  });
});

// ------------------------------------------------------------ .sitefileignore

const treePaths = async (c: LocalContext): Promise<string[]> => (await c.readTree('', 'test')).tree.entries().map(([p]) => p);

test('.sitefileignore filters directory walks', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'src/cache'), { recursive: true });
    await mkdir(join(ctx, 'node_modules/x'), { recursive: true });
    await writeFile(join(ctx, '.sitefileignore'), '# c\nnode_modules/\n*.log\n/notes.txt\nsrc/cache/\n');
    await writeFile(join(ctx, 'a.html'), 'a');
    await writeFile(join(ctx, 'a.log'), 'l');
    await writeFile(join(ctx, 'notes.txt'), 'n');
    await writeFile(join(ctx, 'src/notes.txt'), 'n');
    await writeFile(join(ctx, 'src/b.log'), 'l');
    await writeFile(join(ctx, 'src/cache/c'), 'c');
    await writeFile(join(ctx, 'node_modules/x/i.js'), 'i');
    const c = await LocalContext.open(ctx);
    assert.deepEqual(await treePaths(c), ['/.sitefileignore', '/a.html', '/src/notes.txt']);
  }));

test('.sitefileignore cannot re-include a file under an excluded directory', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'build'));
    await writeFile(join(ctx, 'build/keep.txt'), 'k');
    await writeFile(join(ctx, 'ok.txt'), 'o');
    await writeFile(join(ctx, '.sitefileignore'), 'build/\n!build/keep.txt\n');
    assert.deepEqual(await treePaths(await LocalContext.open(ctx)), ['/.sitefileignore', '/ok.txt']);
  }));

test('.sitefileignore negation re-includes within a non-excluded directory', () =>
  withTemp(async (ctx) => {
    await writeFile(join(ctx, 'a.md'), 'a');
    await writeFile(join(ctx, 'keep.md'), 'k');
    await writeFile(join(ctx, '.sitefileignore'), '*.md\n!keep.md\n');
    assert.deepEqual(await treePaths(await LocalContext.open(ctx)), ['/.sitefileignore', '/keep.md']);
  }));

test('.sitefileignore never hides an explicitly named file or directory root', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'build'));
    await writeFile(join(ctx, 'build/x.txt'), 'x');
    await writeFile(join(ctx, 'secret.log'), 's');
    await writeFile(join(ctx, '.sitefileignore'), 'build/\n*.log\n');
    const c = await LocalContext.open(ctx);
    assert.equal(text((await c.readFile('secret.log')).bytes), 's');
    assert.deepEqual((await c.readTree('build', 'test')).tree.entries().map(([p]) => p), ['/x.txt']);
  }));

test('.sitefileignore can be disabled and is optional', () =>
  withTemp(async (ctx) => {
    await writeFile(join(ctx, 'a.log'), 'l');
    await writeFile(join(ctx, '.sitefileignore'), '*.log\n');
    assert.deepEqual(await treePaths(await LocalContext.open(ctx, { sitefileignore: false })), ['/.sitefileignore', '/a.log']);
    await rm(join(ctx, '.sitefileignore'));
    assert.deepEqual(await treePaths(await LocalContext.open(ctx)), ['/a.log']);
  }));

test('.sitefileignore symlinked outside the context is refused', () =>
  withTemp(async (ctx, outside) => {
    await writeFile(join(outside, 'rules'), '*.log\n');
    await symlink(join(outside, 'rules'), join(ctx, '.sitefileignore'));
    await rejectsWith(LocalContext.open(ctx), 'E_PATH_ESCAPE');
  }));

test('built-in ignores still apply with .sitefileignore present', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, '.git'));
    await writeFile(join(ctx, '.git/HEAD'), 'ref');
    await writeFile(join(ctx, '.sitefileignore'), '!.git\n');
    assert.deepEqual(await treePaths(await LocalContext.open(ctx)), ['/.sitefileignore']);
  }));

// ------------------------------------------------------------ hard links

test('hard link to a file outside the context is refused (readFile, readTree, symlink to it)', () =>
  withTemp(async (ctx, outside) => {
    await link(join(outside, 'secret.txt'), join(ctx, 'innocent.txt'));
    const c = await LocalContext.open(ctx);
    await rejectsWith(c.readFile('innocent.txt'), 'E_HARDLINK');
    await rejectsWith(c.readTree('', 'test'), 'E_HARDLINK');
    await symlink('innocent.txt', join(ctx, 'alias.txt'));
    await rejectsWith(c.readFile('alias.txt'), 'E_HARDLINK');
  }));

test('hard links are allowed with allowHardlinks', () =>
  withTemp(async (ctx, outside) => {
    await link(join(outside, 'secret.txt'), join(ctx, 'innocent.txt'));
    const c = await LocalContext.open(ctx, { allowHardlinks: true });
    assert.equal(text((await c.readFile('innocent.txt')).bytes), 'secret');
    assert.deepEqual(await treePaths(c), ['/innocent.txt']);
  }));

test('hard-linked files excluded by .sitefileignore are not reported', () =>
  withTemp(async (ctx, outside) => {
    await link(join(outside, 'secret.txt'), join(ctx, 'innocent.txt'));
    await writeFile(join(ctx, '.sitefileignore'), 'innocent.txt\n');
    assert.deepEqual(await treePaths(await LocalContext.open(ctx)), ['/.sitefileignore']);
  }));

test('regular files and directories are unaffected by the hard link check', () =>
  withTemp(async (ctx) => {
    await mkdir(join(ctx, 'd/e'), { recursive: true });
    await writeFile(join(ctx, 'd/e/f.txt'), 'f');
    const c = await LocalContext.open(ctx);
    assert.deepEqual(await treePaths(c), ['/d/e/f.txt']);
  }));

test('a dangling .sitefileignore symlink fails clearly instead of being skipped', () =>
  withTemp(async (ctx) => {
    await symlink(join(ctx, 'missing-ignore'), join(ctx, '.sitefileignore'));
    await assert.rejects(LocalContext.open(ctx), (e: unknown) => isSitefileError(e, 'E_NOT_FOUND') && /symlink to a missing file/.test(e.message));
    await LocalContext.open(ctx, { sitefileignore: false });
  }));
