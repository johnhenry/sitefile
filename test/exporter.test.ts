import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { isSitefileError } from '../src/errors.js';
import { exportDirectory, isExporterTempName, TEMP_PREFIX_PATTERN } from '../src/exporter.js';
import { makeFile, Tree, type FileNode } from '../src/tree.js';

const enc = (s: string) => new TextEncoder().encode(s);

async function withTmp(fn: (dir: string) => Promise<void>): Promise<void> {
  const dir = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-export-test-')));
  try {
    await fn(dir);
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
}

function tree(files: Record<string, string | [string, number]>): Tree {
  const list: Array<[string, FileNode]> = Object.entries(files).map(([p, v]) =>
    typeof v === 'string' ? [p, makeFile(enc(v), 'test')] : [p, makeFile(enc(v[0]), 'test', v[1])],
  );
  return Tree.fromFiles(list);
}

const isCode = (code: string) => (e: unknown) => isSitefileError(e) && e.code === code;

async function siblingsOk(parent: string): Promise<void> {
  const names = await fs.readdir(parent);
  assert.deepEqual(names.filter((n) => TEMP_PREFIX_PATTERN.test(n)), [], `leftover temp siblings: ${names}`);
}

test('writes files and nested dirs with bytes and modes', async () => {
  await withTmp(async (ctx) => {
    const out = path.join(ctx, 'dist');
    const t = tree({ '/index.html': '<h1>hi</h1>', '/a/b/c.txt': 'deep', '/bin/run.sh': ['#!/bin/sh\n', 0o755] });
    const res = await exportDirectory(t, out, { contextDir: ctx });
    assert.deepEqual(res, { outDir: out, files: 3 });
    assert.equal(await fs.readFile(path.join(out, 'index.html'), 'utf8'), '<h1>hi</h1>');
    assert.equal(await fs.readFile(path.join(out, 'a/b/c.txt'), 'utf8'), 'deep');
    assert.equal((await fs.stat(path.join(out, 'bin/run.sh'))).mode & 0o777, 0o755);
    assert.equal((await fs.stat(path.join(out, 'index.html'))).mode & 0o777, 0o644);
    await siblingsOk(ctx);
  });
});

test('mode is exact regardless of umask', async () => {
  await withTmp(async (ctx) => {
    const prev = process.umask(0o077);
    try {
      const out = path.join(ctx, 'dist');
      await exportDirectory(tree({ '/x': ['x', 0o755], '/y': 'y' }), out, { contextDir: ctx });
      assert.equal((await fs.stat(path.join(out, 'x'))).mode & 0o777, 0o755);
      assert.equal((await fs.stat(path.join(out, 'y'))).mode & 0o777, 0o644);
    } finally {
      process.umask(prev);
    }
  });
});

test('creates missing parents; empty tree exports an empty dir', async () => {
  await withTmp(async (ctx) => {
    const out = path.join(ctx, 'deep', 'er', 'dist');
    const res = await exportDirectory(Tree.empty, out, { contextDir: ctx });
    assert.equal(res.files, 0);
    assert.deepEqual(await fs.readdir(out), []);
    await siblingsOk(path.dirname(out));
  });
});

test('replaces an existing out dir inside the context', async () => {
  await withTmp(async (ctx) => {
    const out = path.join(ctx, 'dist');
    await fs.mkdir(path.join(out, 'stale'), { recursive: true });
    await fs.writeFile(path.join(out, 'stale', 'old.txt'), 'old');
    await exportDirectory(tree({ '/new.txt': 'new' }), out, { contextDir: ctx });
    assert.deepEqual(await fs.readdir(out), ['new.txt']);
    await siblingsOk(ctx);
  });
});

test('relative out dirs resolve against cwd', async () => {
  await withTmp(async (ctx) => {
    const prev = process.cwd();
    process.chdir(ctx);
    try {
      const res = await exportDirectory(tree({ '/a': 'a' }), 'dist', { contextDir: '.' });
      assert.equal(res.outDir, path.join(ctx, 'dist'));
    } finally {
      process.chdir(prev);
    }
  });
});

test('refuses out == context and out containing context', async () => {
  await withTmp(async (root) => {
    const ctx = path.join(root, 'project');
    await fs.mkdir(ctx);
    await fs.writeFile(path.join(ctx, 'Sitefile'), 'x');
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), ctx, { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), path.join(ctx, '.'), { contextDir: ctx }), isCode('E_EXPORT_UNSAFE'));
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), root, { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
    // Through a symlink to the context.
    const link = path.join(root, 'link');
    await fs.symlink(ctx, link);
    await assert.rejects(
      exportDirectory(tree({ '/a': 'a' }), path.join(link, 'sub', '..'), { contextDir: ctx, force: true }),
      isCode('E_EXPORT_UNSAFE'),
    );
    assert.equal(await fs.readFile(path.join(ctx, 'Sitefile'), 'utf8'), 'x');
    await siblingsOk(root);
  });
});

test('refuses filesystem root and home', async () => {
  await withTmp(async (ctx) => {
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), '/', { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), os.homedir(), { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
  });
});

test('non-empty out outside the context needs --force', async () => {
  await withTmp(async (root) => {
    const ctx = path.join(root, 'ctx');
    const out = path.join(root, 'out');
    await fs.mkdir(ctx);
    await fs.mkdir(out);
    await fs.writeFile(path.join(out, 'keep.txt'), 'keep');
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), out, { contextDir: ctx }), (e: unknown) =>
      isCode('E_EXPORT_UNSAFE')(e) && /--force/.test((e as Error).message),
    );
    assert.deepEqual(await fs.readdir(out), ['keep.txt']);
    await exportDirectory(tree({ '/a': 'a' }), out, { contextDir: ctx, force: true });
    assert.deepEqual(await fs.readdir(out), ['a']);
    await siblingsOk(root);
  });
});

test('empty or missing out outside the context is fine', async () => {
  await withTmp(async (root) => {
    const ctx = path.join(root, 'ctx');
    const out = path.join(root, 'out');
    await fs.mkdir(ctx);
    await fs.mkdir(out);
    await exportDirectory(tree({ '/a': 'a' }), out, { contextDir: ctx });
    assert.deepEqual(await fs.readdir(out), ['a']);
    await exportDirectory(tree({ '/b': 'b' }), path.join(root, 'fresh'), { contextDir: ctx });
    assert.deepEqual(await fs.readdir(path.join(root, 'fresh')), ['b']);
  });
});

test('out that is a file or symlink fails with E_EXPORT', async () => {
  await withTmp(async (ctx) => {
    const file = path.join(ctx, 'dist');
    await fs.writeFile(file, 'x');
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), file, { contextDir: ctx, force: true }), isCode('E_EXPORT'));
    assert.equal(await fs.readFile(file, 'utf8'), 'x');
    const target = path.join(ctx, 'target');
    await fs.mkdir(target);
    const link = path.join(ctx, 'linked');
    await fs.symlink(target, link);
    await assert.rejects(exportDirectory(tree({ '/a': 'a' }), link, { contextDir: ctx, force: true }), isCode('E_EXPORT'));
    assert.deepEqual(await fs.readdir(target), []);
    await siblingsOk(ctx);
  });
});

test('rejects unsafe artifact paths before writing', async () => {
  await withTmp(async (ctx) => {
    const out = path.join(ctx, 'dist');
    const fake = { entries: () => [['/a\\b', makeFile(enc('x'), 't')]] } as unknown as Tree;
    await assert.rejects(exportDirectory(fake, out, { contextDir: ctx }), isCode('E_EXPORT'));
    const fake2 = { entries: () => [['/a/../b', makeFile(enc('x'), 't')]] } as unknown as Tree;
    await assert.rejects(exportDirectory(fake2, out, { contextDir: ctx }), isCode('E_EXPORT'));
    await assert.rejects(fs.stat(out), { code: 'ENOENT' });
    await siblingsOk(ctx);
  });
});

test('a failed write leaves the previous out dir intact', async () => {
  await withTmp(async (ctx) => {
    const out = path.join(ctx, 'dist');
    await fs.mkdir(out);
    await fs.writeFile(path.join(out, 'prev.txt'), 'prev');
    // A 300-byte file name exceeds NAME_MAX on every common filesystem.
    const t = tree({ '/ok.txt': 'ok', [`/${'n'.repeat(300)}`]: 'too long' });
    await assert.rejects(exportDirectory(t, out, { contextDir: ctx }), isCode('E_EXPORT'));
    assert.deepEqual(await fs.readdir(out), ['prev.txt']);
    assert.equal(await fs.readFile(path.join(out, 'prev.txt'), 'utf8'), 'prev');
    await siblingsOk(ctx);
  });
});

test('isExporterTempName', () => {
  assert.equal(isExporterTempName('.dist.sitefile-tmp-0a1b2c', 'dist'), true);
  assert.equal(isExporterTempName('.dist.sitefile-old-0a1b2c', 'dist'), true);
  assert.equal(isExporterTempName('.dist.sitefile-tmp-0a1b2c', 'out'), false);
  assert.equal(isExporterTempName('dist', 'dist'), false);
  assert.equal(isExporterTempName('.dist.sitefile-new-0a1b2c', 'dist'), false);
});
