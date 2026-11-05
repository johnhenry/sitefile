import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, readFile, rm, cp, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// lib/test/cli.test.js -> repo root
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function sitefile(args: string[], cwd = root): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd }, (err, stdout, stderr) => {
      const code = err ? (typeof err.code === 'number' ? err.code : 1) : 0;
      resolve({ code, stdout, stderr });
    });
  });
}

async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-cli-'));
  temps.push(d);
  return d;
}

test('MVP acceptance: examples/hello builds dist/index.html with the title and copied CSS', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'hello');
  await cp(path.join(root, 'examples', 'hello'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /Exported 2 files/);
  const html = await readFile(path.join(ctx, 'dist', 'index.html'), 'utf8');
  assert.match(html, /<title>Hello<\/title>/);
  assert.match(html, /Welcome to Hello\./);
  assert.equal(
    await readFile(path.join(ctx, 'dist', 'style.css'), 'utf8'),
    await readFile(path.join(root, 'examples', 'hello', 'public', 'style.css'), 'utf8'),
  );

  const again = await sitefile(['build', ctx, '--build-arg', 'TITLE=<Sitefile>']);
  assert.equal(again.code, 0, again.stderr);
  assert.match(await readFile(path.join(ctx, 'dist', 'index.html'), 'utf8'), /<title>&lt;Sitefile&gt;<\/title>/);
});

test('validate and inspect', async () => {
  const hello = path.join(await tmp(), 'hello');
  await cp(path.join(root, 'examples', 'hello'), hello, { recursive: true });
  const v = await sitefile(['validate', hello]);
  assert.equal(v.code, 0, v.stderr);
  assert.match(v.stdout, /OK \(0 imports, 2 operations\)/);

  const i = await sitefile(['inspect', hello, '--json']);
  assert.equal(i.code, 0, i.stderr);
  const data = JSON.parse(i.stdout);
  assert.deepEqual(
    data.files.map((f: { path: string }) => f.path),
    ['/index.html', '/style.css'],
  );
  await assert.rejects(stat(path.join(hello, 'dist')), 'inspect must not write output');
});

test('errors print code and location and exit 1; usage errors exit 2', async () => {
  const dir = await tmp();
  const { writeFile } = await import('node:fs/promises');
  await writeFile(path.join(dir, 'Sitefile'), 'FROM scratch\nRUN rm -rf /\nEXPORT /\n');
  const r = await sitefile(['validate', dir]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /error\[E_UNSUPPORTED\]: .*Sitefile:2:1:/);

  const u = await sitefile(['build', dir, '--bogus']);
  assert.equal(u.code, 2);
  const n = await sitefile(['frobnicate']);
  assert.equal(n.code, 2);
});

test('refuses to replace a non-empty directory outside the context without --force', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'hello');
  await cp(path.join(root, 'examples', 'hello'), ctx, { recursive: true });
  const outside = path.join(work, 'precious');
  const { mkdir, writeFile } = await import('node:fs/promises');
  await mkdir(outside);
  await writeFile(path.join(outside, 'keep.txt'), 'keep');

  const r = await sitefile(['build', ctx, '-o', outside]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_EXPORT_UNSAFE/);
  assert.equal(await readFile(path.join(outside, 'keep.txt'), 'utf8'), 'keep');

  const forced = await sitefile(['build', ctx, '-o', outside, '--force']);
  assert.equal(forced.code, 0, forced.stderr);
  await assert.rejects(stat(path.join(outside, 'keep.txt')));

  const unsafe = await sitefile(['build', ctx, '-o', work, '--force']);
  assert.equal(unsafe.code, 1);
  assert.match(unsafe.stderr, /E_EXPORT_UNSAFE/);
});
