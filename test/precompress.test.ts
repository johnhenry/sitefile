// PRECOMPRESS: .gz sidecars from the shared deflate encoder (docs/LANGUAGE.md, "Precompressed sidecars").
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { gunzipSync } from 'node:zlib';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { compressible } from '../src/precompress.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function run(files: Record<string, string>) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-pc-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  const warnings: string[] = [];
  const r = await build({ contextDir: dir, lockfile: false, onWarning: (m) => warnings.push(m) });
  return { ...r, printed: warnings.map((m) => m.replace(/[^\s']*Sitefile:/g, 'Sitefile:')) };
}

const big = 'body { color: red; }\n'.repeat(100);

test('parse and plan: PRECOMPRESS forms and errors', () => {
  const [a] = parseSitefile('precompress --overwrite GZIP /assets/ "/**/*.css" min 10');
  assert.ok(a?.kind === 'PRECOMPRESS' && a.overwrite && a.formats.join() === 'gzip' && a.patterns.length === 2 && a.min);
  const [b] = parseSitefile('PRECOMPRESS /a/');
  assert.ok(b?.kind === 'PRECOMPRESS' && b.formats.join() === 'gzip' && !b.min);
  assert.throws(() => parseSitefile('PRECOMPRESS br /a/'), (e: unknown) => isSitefileError(e) && e.code === 'E_UNSUPPORTED' && /no Brotli encoder/.test(e.detail));
  assert.throws(() => parseSitefile('PRECOMPRESS gzip zstd /a/'), /no Zstandard encoder/);
  assert.throws(() => parseSitefile('PRECOMPRESS gzip gzip /a/'), /gzip given twice/);
  assert.throws(() => parseSitefile('PRECOMPRESS gzip'), /expected a path pattern/);
  assert.throws(() => parseSitefile('PRECOMPRESS /a MIN'), /expected minimum size in bytes/);
  assert.throws(() => parseSitefile('PRECOMPRESS /a MIN 1 x'), /unexpected 'x'/);
  const p = plan(parseProgram('ARG N = 5\nCOPY ./x /x\nPRECOMPRESS /a/ MIN ${N}\nEXPORT /'));
  assert.deepEqual(p.stages[0]!.operations[1], { kind: 'PRECOMPRESS', formats: ['gzip'], patterns: ['/a/**'], min: 5, overwrite: false, span: { file: 'Sitefile', line: 3, column: 1 } });
  assert.equal((plan(parseProgram('COPY ./x /x\nPRECOMPRESS /a/\nEXPORT /')).stages[0]!.operations[1] as { min: number }).min, 1024);
  assert.throws(() => plan(parseProgram('COPY ./x /x\nPRECOMPRESS /a/ MIN -1\nEXPORT /')), /'-1' is not a size/);
  assert.throws(() => plan(parseProgram('COPY ./x /x\nPRECOMPRESS a/\nEXPORT /')), /PRECOMPRESS pattern 'a\/' must be an absolute path/);
});

test('compressible types', () => {
  for (const p of ['/a.css', '/a.JS', '/x/y.svg', '/m.webmanifest', '/f.woff.ttf']) assert.ok(compressible(p), p);
  for (const p of ['/a.png', '/a.woff2', '/a.gz', '/LICENSE', '/.css', '/a.br']) assert.ok(!compressible(p), p);
});

test('build: sidecars for compressible files at least MIN bytes that get smaller', async () => {
  const r = await run({
    'public/a.css': big,
    'public/small.css': 'a{}',
    'public/i.png': big,
    'public/sub/b.js': big,
    'public/random.txt': 'x7Kq',
    Sitefile: 'COPY ./public/ /\nPRECOMPRESS / MIN 4\nEXPORT /\n',
  });
  const paths = r.output.entries().map(([p]) => p);
  assert.deepEqual(paths, ['/a.css', '/a.css.gz', '/i.png', '/random.txt', '/small.css', '/sub/b.js', '/sub/b.js.gz']);
  const gz = r.output.get('/a.css.gz')!;
  assert.equal(gunzipSync(gz.bytes).toString(), big);
  assert.deepEqual([...gz.bytes.slice(0, 10)], [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3], 'fixed header: no mtime, no name');
  assert.equal(gz.origin.replace(/[^\s']*Sitefile:/g, 'Sitefile:'), 'Sitefile:2 PRECOMPRESS');
  // Default MIN 1024: the 2100-byte files only.
  const d = await run({ 'public/a.css': big, 'public/m.css': 'p{}'.repeat(100), Sitefile: 'COPY ./public/ /\nPRECOMPRESS /*.css\nEXPORT /\n' });
  assert.deepEqual(d.output.entries().map(([p]) => p), ['/a.css', '/a.css.gz', '/m.css']);
});

test('build: no match warns; a different existing sidecar conflicts unless --overwrite', async () => {
  const none = await run({ 'a.css': big, Sitefile: 'COPY ./a.css /\nPRECOMPRESS /assets/\nEXPORT /\n' });
  assert.deepEqual(none.printed, ['Sitefile:2 PRECOMPRESS: no files match /assets/**']);
  await assert.rejects(run({ 'a.css': big, 'old.gz': 'x', Sitefile: 'COPY ./a.css /\nCOPY ./old.gz /a.css.gz\nPRECOMPRESS /a.css\nEXPORT /\n' }), (e: unknown) => isSitefileError(e) && e.code === 'E_CONFLICT');
  const ok = await run({ 'a.css': big, 'old.gz': 'x', Sitefile: 'COPY ./a.css /\nCOPY ./old.gz /a.css.gz\nPRECOMPRESS --overwrite /a.css\nEXPORT /\n' });
  assert.equal(gunzipSync(ok.output.get('/a.css.gz')!.bytes).toString(), big);
});
