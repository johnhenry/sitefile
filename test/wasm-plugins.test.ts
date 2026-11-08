import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { sha256 } from '../src/digest.js';

// lib/test/*.js → repo root
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const fixture = (name: string): Promise<Buffer> => readFile(path.join(root, 'test', 'fixtures', 'wasm', `${name}.wasm`));
const badge = (): Promise<Buffer> => readFile(path.join(root, 'examples', 'wasm', 'plugins', 'badge.wasm'));

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string | Uint8Array>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-wasm-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}
const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
const site = (wasm: Uint8Array, line = 'TRANSFORM WITH p TO /out.txt', extra: Record<string, string> = {}): Record<string, string | Uint8Array> => ({
  'p.wasm': wasm,
  ...extra,
  Sitefile: `FROM scratch\nPLUGIN p FROM ./p.wasm\n${line}\nEXPORT /`,
});
async function rejects(files: Record<string, string | Uint8Array>, code: ErrorCode, match: RegExp, extra: Partial<BuildOptions> = {}): Promise<void> {
  const dir = await project(files);
  await assert.rejects(build({ contextDir: dir, lockfile: false, ...extra }), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.match(e.message, match);
    return true;
  });
}

test('a wasm plugin can sit at the context root and returns text', async () => {
  const r = await build({ contextDir: await project(site(await fixture('const'))), lockfile: false });
  assert.equal(text(r.output.get('/out.txt')?.bytes), 'hello from wasm');
});

test('the request reaches the module (its length is visible to it)', async () => {
  const r = await build({ contextDir: await project(site(await fixture('echo-len'), 'TRANSFORM ./a.txt WITH p TO /out.txt OPTIONS k=v', { 'a.txt': 'abc' })), lockfile: false });
  assert.match(text(r.output.get('/out.txt')?.bytes), /^\d$/);
});

test('the Rust badge plugin: options-only, FOR over Markdown, option errors', async () => {
  const r = await build({
    contextDir: await project({
      'plugins/badge.wasm': await badge(),
      'posts/2026-01-01-a.md': '---\ntitle: A\n---\n' + 'word '.repeat(450) + '\n',
      Sitefile: [
        'FROM scratch',
        'PLUGIN badge FROM ./plugins/badge.wasm',
        'IMPORT posts FROM ./posts AS markdown',
        'TRANSFORM WITH badge TO /static.svg OPTIONS label=build value="passing <ok>" color=#4c1',
        'TRANSFORM WITH badge TO /read/${p.slug}.svg FOR p IN posts',
        'EXPORT /',
      ].join('\n'),
    }),
    lockfile: false,
  });
  const fixed = text(r.output.get('/static.svg')?.bytes);
  assert.match(fixed, /^<svg [^>]*aria-label="build: passing &lt;ok&gt;"/);
  assert.doesNotMatch(fixed, /<ok>/);
  assert.match(text(r.output.get('/read/a.svg')?.bytes), /3 min · 450 words/);

  await rejects(
    { 'plugins/badge.wasm': await badge(), Sitefile: 'FROM scratch\nPLUGIN b FROM ./plugins/badge.wasm\nTRANSFORM WITH b TO /x.svg OPTIONS value=x color="red;x"\nEXPORT /' },
    'E_PLUGIN',
    /option "color"/,
  );
});

test('modules that break the ABI are rejected when the build starts', async () => {
  await rejects(site(await fixture('imports')), 'E_PLUGIN', /can't import anything.*env\.now/);
  await rejects(site(await fixture('noexport')), 'E_PLUGIN', /must export function 'sitefile_transform'/);
  await rejects(site(new TextEncoder().encode('not wasm')), 'E_PLUGIN', /not a valid WebAssembly module/);
});

test('traps, bad results, plugin errors and timeouts are E_PLUGIN', async () => {
  await rejects(site(await fixture('trap')), 'E_PLUGIN', /unreachable/);
  await rejects(site(await fixture('oob')), 'E_PLUGIN', /outside memory/);
  await rejects(site(await fixture('badjson')), 'E_PLUGIN', /not UTF-8 JSON/);
  await rejects(site(await fixture('error')), 'E_PLUGIN', /boom: bad input/);
  await rejects(site(await fixture('loop')), 'E_PLUGIN', /timed out/, { pluginPolicy: { timeoutMs: 300 } });
});

test('--plugin-memory caps a wasm module\'s linear memory too (#157)', async () => {
  const wasm = await fixture('grow'); // grows by 2 MiB
  await rejects(site(wasm), 'E_PLUGIN', /used 2162688 bytes of memory, more than the 1048576 allowed/, { pluginPolicy: { maxMemoryMiB: 1 } });
  const r = await build({ contextDir: await project(site(wasm)), lockfile: false, pluginPolicy: { maxMemoryMiB: 4 } });
  assert.equal(text(r.output.get('/out.txt')?.bytes), 'grew');
});

test('INTEGRITY pins a wasm module', async () => {
  const wasm = await fixture('const');
  await rejects({ 'p.wasm': wasm, Sitefile: `FROM scratch\nPLUGIN p FROM ./p.wasm INTEGRITY ${sha256('other')}\nTRANSFORM WITH p TO /o\nEXPORT /` }, 'E_INTEGRITY', /./);
  const ok = await build({
    contextDir: await project({ 'p.wasm': wasm, Sitefile: `FROM scratch\nPLUGIN p FROM ./p.wasm INTEGRITY ${sha256(wasm)}\nTRANSFORM WITH p TO /o\nEXPORT /` }),
    lockfile: false,
  });
  assert.equal(text(ok.output.get('/o')?.bytes), 'hello from wasm');
});

test('wasm results are cached by module digest: a second build starts no worker', async () => {
  const dir = await project(site(await fixture('const')));
  const cacheDir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-wasm-cache-'));
  temps.push(cacheDir);
  const first = await build({ contextDir: dir, lockfile: false, cacheDir });
  assert.equal(first.transformStats?.misses, 1);
  const second = await build({ contextDir: dir, lockfile: false, cacheDir });
  assert.equal(second.transformStats?.hits, 1);
  assert.equal(second.transformStats?.pluginsSpawned, 0);
  assert.equal(text(second.output.get('/out.txt')?.bytes), 'hello from wasm');
});
