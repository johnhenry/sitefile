// PRECACHE: a Workbox-style precache manifest (docs/LANGUAGE.md, "Precache manifest").
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { precacheJson } from '../src/precache.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function run(files: Record<string, string>) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-precache-'));
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

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
const rev = (s: string): string => createHash('sha256').update(s).digest('hex').slice(0, 10);

test('parse and plan: PRECACHE', () => {
  const [a] = parseSitefile('precache --overwrite /assets/ /index.html to /m.json into /sw.js');
  assert.ok(a?.kind === 'PRECACHE' && a.overwrite && a.patterns.length === 2 && a.dest && a.into);
  assert.throws(() => parseSitefile('PRECACHE TO /m.json'), /expected a path pattern/);
  assert.throws(() => parseSitefile('PRECACHE /a INTO /sw.js TO /m.json'), /unexpected 'TO'/);
  const p = plan(parseProgram('FROM scratch\nPRECACHE /a/\nEXPORT /www'));
  assert.deepEqual(p.stages[0]!.operations[0], { kind: 'PRECACHE', patterns: ['/a/**'], dest: '/sw-manifest.json', root: '/www', overwrite: false, span: { file: 'Sitefile', line: 2, column: 1 } });
  const into = plan(parseProgram('FROM scratch\nPRECACHE /a/ INTO /sw.js\nEXPORT /')).stages[0]!.operations[0]!;
  assert.ok(into.kind === 'PRECACHE' && into.dest === undefined && into.into === '/sw.js');
  assert.throws(() => plan(parseProgram('FROM scratch\nPRECACHE a/\nEXPORT /')), /PRECACHE pattern 'a\/' must be an absolute path/);
  assert.equal(precacheJson([]), '[]\n');
});

test('build: revisions, fingerprinted files, URLs below the EXPORT path, injection', async () => {
  const r = await run({
    'public/index.html': '<link rel=stylesheet href=app.css>',
    'public/app.css': 'a{}',
    'public/img/a b.png': 'P',
    'public/sw.js': 'precacheAndRoute(self.__WB_MANIFEST); // self.__WB_MANIFEST',
    Sitefile: 'COPY ./public/ /www/\nFINGERPRINT /www/app.css\nPRECACHE /www/ TO /www/precache.json INTO /www/sw.js\nEXPORT /www\n',
  });
  const css = r.output.entries().map(([p]) => p).find((p) => p.startsWith('/app.'))!;
  const index = text(r.output.get('/index.html')!.bytes);
  const manifest = `[\n  {"url": "${css}", "revision": null},\n  {"url": "/img/a%20b.png", "revision": "${rev('P')}"},\n  {"url": "/index.html", "revision": "${rev(index)}"}\n]\n`;
  assert.equal(text(r.output.get('/precache.json')!.bytes), manifest);
  assert.equal(text(r.output.get('/sw.js')!.bytes), `precacheAndRoute(${manifest.trimEnd()}); // self.__WB_MANIFEST`);
});

test('build: errors and warnings', async () => {
  const none = await run({ 'a.txt': 'x', Sitefile: 'COPY ./a.txt /\nPRECACHE /assets/\nEXPORT /\n' });
  assert.deepEqual(none.printed, ['Sitefile:2 PRECACHE: no files match /assets/**']);
  assert.equal(text(none.output.get('/sw-manifest.json')!.bytes), '[]\n');
  const code = (e: unknown) => isSitefileError(e) && e.code === 'E_VALIDATION';
  await assert.rejects(run({ 'a.txt': 'x', Sitefile: 'COPY ./a.txt /\nPRECACHE / INTO /sw.js\nEXPORT /\n' }), (e: unknown) => code(e) && /there is no such file/.test(String(e)));
  await assert.rejects(run({ 'sw.js': 'x', Sitefile: 'COPY ./sw.js /\nPRECACHE / INTO /sw.js\nEXPORT /\n' }), (e: unknown) => code(e) && /has no self.__WB_MANIFEST/.test(String(e)));
});
