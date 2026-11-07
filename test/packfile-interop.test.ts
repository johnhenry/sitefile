// Interop with @johnhenry/packfile, the family's reader and server for Web
// Bundles. Sitefile has no runtime dependency on packfile: the two meet at the
// file format. These tests build a site with the real CLI, once as a directory
// and once as `.wbn`, and require packfile to read the bundle back into exactly
// the directory's files and to serve them.
//
// Sitefile writes a plain (uncompressed) b2 Web Bundle. packfile's own archive
// format (`toArchive`/`fromArchive`) is gzip around the same bundle, under the
// fixed base URL https://packfile.invalid/. The last test pins that difference
// down, so a change on either side shows up here.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { createRouter, fromArchive } from '@johnhenry/packfile';
import { fromWebBundle } from '@johnhenry/packfile/web-bundle';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');

const temps: string[] = [];
after(async () => {
  for (const d of temps) await fs.rm(d, { recursive: true, force: true });
});

function sitefile(args: string[], cwd: string): Promise<void> {
  return new Promise((resolve, reject) => {
    execFile(process.execPath, [cli, ...args], { cwd, env: { ...process.env, SOURCE_DATE_EPOCH: '' } }, (err, _stdout, stderr) => {
      if (err) reject(new Error(`sitefile ${args.join(' ')} failed:\n${stderr}`));
      else resolve();
    });
  });
}

const BINARY = Buffer.from(Array.from({ length: 256 }, (_, i) => i));

// A small site with HTML, CSS, a binary file, a nested directory and a
// non-ASCII path, built through RENDER and COPY like a real one.
async function context(): Promise<string> {
  const dir = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-packfile-')));
  temps.push(dir);
  await fs.mkdir(path.join(dir, 'public', 'css'), { recursive: true });
  await fs.mkdir(path.join(dir, 'public', 'docs', 'ünï'), { recursive: true });
  await fs.writeFile(path.join(dir, 'public', 'css', 'site.css'), 'body { color: #333; }\n');
  await fs.writeFile(path.join(dir, 'public', 'logo.bin'), BINARY);
  await fs.writeFile(path.join(dir, 'public', 'docs', 'ünï', 'page.html'), '<p>unicode path</p>\n');
  await fs.writeFile(
    path.join(dir, 'Sitefile'),
    [
      'ARG TITLE = "Interop"',
      'COPY ./public/ /',
      'RENDER --escape=html <<HTML TO /index.html',
      '<!doctype html><title>${TITLE}</title><link rel="stylesheet" href="/css/site.css">',
      'HTML',
      'EXPORT / AS directory',
      '',
    ].join('\n'),
  );
  return dir;
}

async function readTree(dir: string, prefix = ''): Promise<Map<string, Buffer>> {
  const out = new Map<string, Buffer>();
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    const rel = prefix + entry.name;
    const abs = path.join(dir, entry.name);
    if (entry.isDirectory()) for (const [k, v] of await readTree(abs, rel + '/')) out.set(k, v);
    else out.set(rel, await fs.readFile(abs));
  }
  return out;
}

async function buildBoth(base: string): Promise<{ files: Map<string, Buffer>; bundle: Buffer }> {
  const ctx = await context();
  await sitefile(['build', ctx, '-o', path.join(ctx, 'out')], ctx);
  await sitefile(['build', ctx, '-o', path.join(ctx, 'site.wbn'), '--base-url', base], ctx);
  return { files: await readTree(path.join(ctx, 'out')), bundle: await fs.readFile(path.join(ctx, 'site.wbn')) };
}

function sameFiles(actual: Map<string, { data: Uint8Array }>, expected: Map<string, Buffer>): void {
  assert.deepEqual([...actual.keys()].sort(), [...expected.keys()].sort());
  for (const [p, bytes] of expected) assert.ok(Buffer.from(actual.get(p)!.data).equals(bytes), `bytes differ for ${p}`);
}

test('packfile fromWebBundle reads a Sitefile .wbn back into the same files as the directory export', async () => {
  const base = 'https://site.example/';
  const { files, bundle } = await buildBoth(base);
  assert.ok(files.has('index.html') && files.has('logo.bin') && files.has('docs/ünï/page.html'));
  sameFiles(fromWebBundle(bundle, { baseURL: base }), files);
});

test('packfile createRouter serves a Sitefile .wbn: bytes, MIME type and 404', async () => {
  const base = 'https://site.example/';
  const { files, bundle } = await buildBoth(base);
  const router = createRouter(fromWebBundle(bundle, { baseURL: base }));

  const index = await router(new Request(new URL('/index.html', base)));
  assert.equal(index.status, 200);
  assert.match(index.headers.get('content-type') ?? '', /^text\/html/);
  assert.ok(Buffer.from(await index.arrayBuffer()).equals(files.get('index.html')!));
  assert.match(files.get('index.html')!.toString(), /<title>Interop<\/title>/);

  const logo = await router(new Request(new URL('/logo.bin', base)));
  assert.equal(logo.status, 200);
  assert.ok(Buffer.from(await logo.arrayBuffer()).equals(BINARY));

  assert.equal((await router(new Request(new URL('/missing.html', base)))).status, 404);
});

test("packfile's archive format is gzip(Sitefile's .wbn) under https://packfile.invalid/", async () => {
  const { files, bundle } = await buildBoth('https://packfile.invalid/');
  // Uncompressed, the bundle is already what fromArchive reads...
  sameFiles(await fromArchive(bundle, { compressed: false }), files);
  // ...and gzip is the only layer between it and packfile's default archive format.
  sameFiles(await fromArchive(gzipSync(bundle)), files);
  await assert.rejects(fromArchive(bundle));
});
