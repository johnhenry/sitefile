// CHECKLINKS (docs/LANGUAGE.md, "Link checking").
import { test, after, before } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, type Server } from 'node:http';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import type { AddressInfo } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { BuildMemo } from '../src/memo.js';
import { redirected } from '../src/checklinks.js';

const temps: string[] = [];
let server: Server;
let base = '';
let requests: string[] = [];
before(async () => {
  server = createServer((req, res) => {
    requests.push(`${req.method} ${req.url}`);
    if (req.url === '/ok') res.end('fine');
    else if (req.url === '/moved') res.writeHead(302, { location: '/ok' }).end();
    else res.writeHead(404).end();
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});
after(async () => {
  server.close();
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function context(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-links-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

const rel = (s: string): string => s.replace(/[^\s']*Sitefile:/g, 'Sitefile:');

async function run(files: Record<string, string>, extra: Parameters<typeof build>[0] extends infer O ? Partial<O> : never = {}) {
  const dir = await context(files);
  const warnings: string[] = [];
  try {
    await build({ contextDir: dir, lockfile: false, onWarning: (m) => warnings.push(m), ...extra });
    return { warnings: warnings.map(rel), error: undefined as string | undefined };
  } catch (e) {
    if (!isSitefileError(e)) throw e;
    assert.equal(e.code, 'E_BROKEN_LINK', e.message);
    return { warnings: warnings.map(rel), error: rel(e.message) };
  }
}

const SITE = {
  'public/index.html':
    '<link rel=stylesheet href="css/site.css">\n<a href="about">About</a> <a href="docs/">Docs</a> <a href="blog/">Blog</a>\n<img src="/img/missing.png"> <a href="#top">t</a> <a href="mailto:a@b">m</a>\n<a href="old/page">old</a> <a href="https://example.com/">ext</a> <a href="nope.html?x#y">nope</a>\n',
  'public/about.html': 'about',
  'public/docs/index.html': '<a href="../index.html">home</a>',
  'public/blog/post.html': '',
  'public/css/site.css': 'a { background: url(../img/bg.png) } @import "gone.css";',
};

test('parse and plan: CHECKLINKS forms', () => {
  const [c] = parseSitefile('checklinks --warn --external');
  assert.ok(c?.kind === 'CHECKLINKS' && c.warn && c.external);
  assert.throws(() => parseSitefile('CHECKLINKS /x'), /unexpected '\/x'/);
  assert.throws(() => parseSitefile('CHECKLINKS --overwrite'), /unknown flag --overwrite/);
  const p = plan(parseProgram('COPY ./a /www/a\nCHECKLINKS\nREDIRECT /old /new\nEXPORT /www'));
  assert.deepEqual(p.stages[0]!.operations[1], { kind: 'CHECKLINKS', warn: false, external: false, root: '/www', redirects: ['/old'], span: { file: 'Sitefile', line: 2, column: 1 } });
  assert.ok(redirected(['/blog/*'], '/blog/x/y') && redirected(['/a'], '/a') && !redirected(['/a'], '/a/') && !redirected(['/b*'], '/bc'));
});

test('build: every broken internal link fails the build, with its file and line', async () => {
  const r = await run({ ...SITE, Sitefile: 'COPY ./public/ /\nCHECKLINKS\nREDIRECT /old/* /new/:splat\nEXPORT /\n' });
  assert.equal(
    r.error,
    'Sitefile:2:1: CHECKLINKS: 5 broken links:\n' +
      '  /css/site.css:1: ../img/bg.png (not found: /img/bg.png)\n' +
      '  /css/site.css:1: gone.css (not found: /css/gone.css)\n' +
      '  /index.html:2: blog/ (not found: /blog/)\n' +
      '  /index.html:3: /img/missing.png (not found: /img/missing.png)\n' +
      '  /index.html:4: nope.html?x#y (not found: /nope.html)',
  );
});

test('build: --warn prints each one and keeps building; a clean tree prints nothing', async () => {
  const r = await run({ ...SITE, Sitefile: 'COPY ./public/ /\nCHECKLINKS --warn\nEXPORT /\n' });
  assert.equal(r.error, undefined);
  assert.equal(r.warnings.length, 6);
  assert.equal(r.warnings[4], 'Sitefile:2 CHECKLINKS: broken link /index.html:4: old/page (not found: /old/page)');
  const ok = await run({ 'a.html': '<a href="b">b</a>', 'b.html': '<a href="a.html#x">a</a>', Sitefile: 'COPY ./a.html ./b.html /\nCHECKLINKS\nEXPORT /\n'.replace('./a.html ./b.html /', './a.html /\nCOPY ./b.html /') });
  assert.deepEqual(ok, { warnings: [], error: undefined });
});

test('build: absolute links resolve under the EXPORT path; CHECKLINKS after FINGERPRINT sees the new names', async () => {
  const r = await run({
    'p/index.html': '<link rel=stylesheet href="/s.css"><img src="/x.png">',
    'p/s.css': 'a{}',
    Sitefile: 'COPY ./p/ /www/\nFINGERPRINT /www/*.css\nCHECKLINKS --warn\nEXPORT /www\n',
  });
  assert.deepEqual(r.warnings, ['Sitefile:3 CHECKLINKS: broken link /www/index.html:1: /x.png (not found: /www/x.png)']);
});

test('build: --external requests each http(s) URL once, under the network policy', async () => {
  requests = [];
  const page = `<a href="${base}/ok">a</a><a href="${base}/moved">b</a><a href="${base}/gone">c</a><a href="${base}/ok">d</a>`;
  const files = { 'index.html': page, Sitefile: 'COPY ./index.html /\nCHECKLINKS --warn --external\nEXPORT /\n' };
  const blocked = await run(files);
  assert.deepEqual(blocked.warnings, [
    `Sitefile:2 CHECKLINKS: broken link /index.html:1: ${base}/ok (plain http is checked only with --allow-http)`,
    `Sitefile:2 CHECKLINKS: broken link /index.html:1: ${base}/moved (plain http is checked only with --allow-http)`,
    `Sitefile:2 CHECKLINKS: broken link /index.html:1: ${base}/gone (plain http is checked only with --allow-http)`,
    `Sitefile:2 CHECKLINKS: broken link /index.html:1: ${base}/ok (plain http is checked only with --allow-http)`,
  ]);
  assert.deepEqual(requests, []);
  const r = await run(files, { fetchPolicy: { allowHttp: true, allowPrivateNetwork: true } });
  assert.deepEqual(r.warnings, [`Sitefile:2 CHECKLINKS: broken link /index.html:1: ${base}/gone (HTTP 404)`]);
  assert.deepEqual(requests, ['GET /ok', 'GET /moved', 'GET /ok', 'GET /gone']);
  // Without --external nothing is requested.
  requests = [];
  assert.deepEqual(await run({ ...files, Sitefile: 'COPY ./index.html /\nCHECKLINKS\nEXPORT /\n' }, { fetchPolicy: { allowHttp: true, allowPrivateNetwork: true } }), { warnings: [], error: undefined });
  assert.deepEqual(requests, []);
});

test('incremental: an unchanged CHECKLINKS step is reused and prints its warnings again', async () => {
  const memo = new BuildMemo();
  const dir = await context({ 'a.html': '<a href="b.html">b</a>', Sitefile: 'COPY ./a.html /\nCHECKLINKS --warn\nEXPORT /\n' });
  for (let i = 0; i < 2; i++) {
    const warnings: string[] = [];
    memo.begin();
    await build({ contextDir: dir, lockfile: false, memo, onWarning: (m) => warnings.push(m) });
    memo.finish(true);
    assert.deepEqual(warnings.map(rel), ['Sitefile:2 CHECKLINKS: broken link /a.html:1: b.html (not found: /b.html)']);
  }
});
