// MIME: custom Content-Types as /_headers rules (docs/LANGUAGE.md, "Site files").
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { bundleResponses } from '../src/wbn.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function run(files: Record<string, string>) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-mime-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return build({ contextDir: dir, lockfile: false, onWarning: () => {} });
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

test('parse and plan: MIME', () => {
  const [m] = parseSitefile('mime .USDZ model/vnd.usdz+zip');
  assert.ok(m?.kind === 'MIME');
  assert.throws(() => parseSitefile('MIME .x'), /expected media type/);
  assert.throws(() => parseSitefile('MIME .x a/b c'), /unexpected 'c'/);
  const p = plan(parseProgram('FROM scratch\nMIME .USDZ Model/Vnd.USDZ+zip\nEXPORT /'));
  assert.deepEqual(p.headers, [{ pattern: '/*.usdz', name: 'Content-Type', value: 'model/vnd.usdz+zip', overwrite: false, span: { file: 'Sitefile', line: 2, column: 1 }, instruction: 'MIME' }]);
  assert.throws(() => plan(parseProgram('FROM scratch\nMIME usdz a/b\nEXPORT /')), /must be a dot and letters/);
  assert.throws(() => plan(parseProgram('FROM scratch\nMIME .a/b c/d\nEXPORT /')), /must be a dot/);
  assert.throws(() => plan(parseProgram('FROM scratch\nMIME .x text/plain;charset=utf-8\nEXPORT /')), /without parameters/);
  assert.throws(() => plan(parseProgram('FROM scratch\nMIME .x a/b\nMIME .X c/d\nEXPORT /')), /MIME \.x given twice \(line 2/);
});

test('build: /_headers rules in source order with HEADER, and the Web Bundle content-type', async () => {
  const r = await run({ 'public/m.usdz': 'U', 'public/a/s.ts': 'S', Sitefile: 'COPY ./public/ /\nHEADER /* X-A: 1\nMIME .usdz model/vnd.usdz+zip\nMIME .ts video/mp2t\nEXPORT /\n' });
  assert.equal(text(r.output.get('/_headers')!.bytes), '/*\n  X-A: 1\n/*.usdz\n  Content-Type: model/vnd.usdz+zip\n/*.ts\n  Content-Type: video/mp2t\n');
  const only = await run({ 'public/m.usdz': 'U', Sitefile: 'COPY ./public/ /\nMIME .usdz model/vnd.usdz+zip\nEXPORT /\n' });
  assert.match(only.output.get('/_headers')!.origin, /Sitefile:2 MIME$/);
  const { responses } = bundleResponses(r.output);
  const types = responses.map((b) => Buffer.from(b).toString('latin1')).filter((s) => s.includes('content-type'));
  assert.ok(types.some((s) => s.includes('model/vnd.usdz+zip')) && types.some((s) => s.includes('video/mp2t')), 'the Web Bundle types the files');
});
