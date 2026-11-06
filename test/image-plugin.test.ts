import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { sha256 } from '../src/digest.js';

// The first-party image plugin (plugins/image, docs/PLUGINS.md "The image plugin").
// The same digests are pinned in rust/src/plugins/wasm.rs, so V8 and wasmtime
// must produce the same bytes.

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasm = (): Promise<Buffer> => readFile(path.join(root, 'plugins', 'image', 'image.wasm'));
const testdata = (name: string): Promise<Buffer> => readFile(path.join(root, 'plugins', 'image', 'testdata', name));

/** Tree digest of examples/image (pinned here and in the Rust tests). */
const EXAMPLE_DIGEST = 'sha256:bfdec3ec231a04bad5cb5a6a67013587065650b49f2f7de5bbda67d78daacfaf';
/** tiny.jpg at widths 16,24 as jpeg, png and webp (directory mode). */
const TINY_DIGEST = 'sha256:ab43d0d4f76f6f28cf1ce0876b71822985342702ae1568817401ca7239f8dfc0';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string | Uint8Array>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-image-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

async function site(line: string, images: Record<string, Uint8Array>): Promise<string> {
  return project({ 'plugins/image.wasm': await wasm(), ...images, Sitefile: `PLUGIN image FROM ./plugins/image.wasm\n${line}\nEXPORT /\n` });
}

async function pluginError(line: string, images: Record<string, Uint8Array>): Promise<string> {
  const dir = await site(line, images);
  let message = '';
  await assert.rejects(build({ contextDir: dir, lockfile: false }), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, 'E_PLUGIN', e.message);
    message = e.message;
    return true;
  });
  return message;
}

test('the example builds without --allow-plugin, and its copy of the plugin is the pinned one', async () => {
  const bytes = await wasm();
  const copy = await readFile(path.join(root, 'examples', 'image', 'plugins', 'image.wasm'));
  assert.ok(copy.equals(bytes), 'examples/image/plugins/image.wasm must equal plugins/image/image.wasm (run plugins/image/build.sh)');
  const sitefile = await readFile(path.join(root, 'examples', 'image', 'Sitefile'), 'utf8');
  assert.ok(sitefile.includes(`INTEGRITY ${sha256(bytes)}`), 'examples/image/Sitefile must pin the current image.wasm');

  const r = await build({ contextDir: path.join(root, 'examples', 'image'), lockfile: false });
  assert.deepEqual(
    r.output.entries().map(([p]) => p),
    ['/img/badge-128.png', '/img/badge-128.webp', '/img/badge-64.png', '/img/badge-64.webp', '/img/badge.json', '/img/sky-1200.jpg', '/img/sky-400.jpg', '/img/sky-800.jpg', '/img/sky-thumb.png', '/img/sky.json', '/index.html'],
  );
  const meta = JSON.parse(new TextDecoder().decode(r.output.get('/img/sky.json')!.bytes)) as Record<string, unknown>;
  assert.equal(meta['version'], 1);
  assert.deepEqual(meta['source'], { name: 'sky.jpg', format: 'jpeg', width: 1600, height: 1000 });
  assert.equal(meta['width'], 1200);
  assert.equal(meta['height'], 750);
  assert.equal(meta['src'], '/img/sky-1200.jpg');
  assert.equal(meta['srcset'], '/img/sky-400.jpg 400w, /img/sky-800.jpg 800w, /img/sky-1200.jpg 1200w');
  const variants = meta['variants'] as Array<{ path: string; size: number }>;
  for (const v of variants) assert.equal(r.output.get(v.path)?.bytes.length, v.size, v.path);
  assert.equal(r.output.digest(), EXAMPLE_DIGEST);
});

test('every supported input format, several widths and output formats', async () => {
  const dir = await site('TRANSFORM ./tiny.jpg WITH image TO /out/ OPTIONS widths=16,24,100 format=jpeg,png,webp quality=70', { 'tiny.jpg': await testdata('tiny.jpg') });
  const r = await build({ contextDir: dir, lockfile: false });
  assert.deepEqual(
    r.output.entries().map(([p]) => p),
    ['/out/tiny-16.jpg', '/out/tiny-16.png', '/out/tiny-16.webp', '/out/tiny-24.jpg', '/out/tiny-24.png', '/out/tiny-24.webp', '/out/tiny.json'],
  );
  assert.equal(r.output.digest(), TINY_DIGEST);

  for (const [name, w, h] of [['tiny.png', 40, 30], ['tiny.gif', 30, 20], ['tiny-lossy.webp', 36, 24], ['rotated.jpg', 32, 48]] as const) {
    const d = await site(`TRANSFORM ./${name} WITH image TO /out/ OPTIONS name=x`, { [name]: await testdata(name) });
    const out = await build({ contextDir: d, lockfile: false });
    const meta = JSON.parse(new TextDecoder().decode(out.output.get('/out/x.json')!.bytes)) as { width: number; height: number };
    assert.deepEqual([meta.width, meta.height], [w, h], name);
  }
});

test('a decompression bomb, an unknown option and a non-image fail with E_PLUGIN', async () => {
  const start = Date.now();
  const bomb = await pluginError('TRANSFORM ./bomb.png WITH image TO /out/', { 'bomb.png': await testdata('bomb.png') });
  assert.match(bomb, /PLUGIN image.*60000×60000 = 3600000000 pixels, more than the 40000000 allowed/);
  assert.ok(Date.now() - start < 10_000, 'refused from the header, not after decoding');
  assert.match(await pluginError('TRANSFORM ./tiny.png WITH image TO /out/ OPTIONS widht=10', { 'tiny.png': await testdata('tiny.png') }), /unknown option "widht"/);
  assert.match(await pluginError('TRANSFORM ./x.png WITH image TO /out/', { 'x.png': new TextEncoder().encode('not an image') }), /not a PNG, JPEG, GIF or WebP image/);
});

test('results are cached by content digest: an unchanged image is not decoded again', async () => {
  const dir = await site('TRANSFORM ./tiny.png WITH image TO /out/ OPTIONS widths=10,20', { 'tiny.png': await testdata('tiny.png') });
  const cacheDir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-image-cache-'));
  temps.push(cacheDir);
  const first = await build({ contextDir: dir, lockfile: false, cacheDir });
  assert.equal(first.transformStats.misses, 1);
  const second = await build({ contextDir: dir, lockfile: false, cacheDir });
  assert.equal(second.transformStats.hits, 1);
  assert.equal(second.transformStats.pluginsSpawned, 0);
  assert.equal(second.output.digest(), first.output.digest());
});

test('light fuzzing through V8: truncated and corrupted images give an image or E_PLUGIN', async () => {
  let seed = 0x1234567;
  const next = (): number => {
    seed ^= seed << 13;
    seed ^= seed >>> 17;
    seed ^= seed << 5;
    return seed >>> 0;
  };
  for (const name of ['tiny.png', 'tiny.jpg', 'tiny.gif', 'tiny-lossy.webp']) {
    const original = await testdata(name);
    for (let round = 0; round < 6; round++) {
      const b = Buffer.from(original);
      const mutated = round % 2 === 0 ? b.subarray(0, next() % b.length) : (b.writeUInt8(next() % 256, next() % b.length), b);
      const dir = await site(`TRANSFORM ./in WITH image TO /out/ OPTIONS widths=8 max_pixels=100000`, { in: mutated });
      try {
        const r = await build({ contextDir: dir, lockfile: false });
        assert.ok(r.output.get('/out/in.json'), `${name} round ${round}`);
      } catch (e) {
        assert.ok(isSitefileError(e) && e.code === 'E_PLUGIN', `${name} round ${round}: ${String(e)}`);
      }
    }
  }
});
