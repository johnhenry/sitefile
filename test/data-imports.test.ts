import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { makeFile, Tree } from '../src/tree.js';
import { writeWebBundle } from '../src/wbn.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string | Uint8Array>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-data-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}
const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
async function run(files: Record<string, string | Uint8Array>, extra: Partial<BuildOptions> = {}) {
  return build({ contextDir: await project(files), lockfile: false, ...extra });
}
async function rejects(files: Record<string, string | Uint8Array>, code: ErrorCode, match?: RegExp): Promise<void> {
  await assert.rejects(run(files), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    if (match) assert.match(e.message, match);
    return true;
  });
}

const bundle = (): Uint8Array =>
  writeWebBundle(
    Tree.fromFiles([
      ['/index.html', makeFile(new TextEncoder().encode('<h1>hi</h1>'), 't')],
      ['/app.js', makeFile(new TextEncoder().encode('console.log(1)'), 't')],
      ['/logo.png', makeFile(new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0xff]), 't')],
    ]),
    'https://bundle.example/',
  );

test('AS yaml: nested values in templates', async () => {
  const r = await run({
    'site.yaml': 'title: Docs\nnav:\n  - title: Home\n    url: /\n  - title: About\n    url: /about/\nmeta: {draft: false, n: 3}\n',
    Sitefile: 'FROM scratch\nIMPORT site FROM ./site.yaml AS yaml\nRENDER <<T TO /i.html\n${site.title}:${#each site.nav as l}[${l.title} ${l.url}]${/each} ${site.meta.n} ${#if site.meta.draft}D${else}P${/if}\nT\nEXPORT /',
  });
  assert.equal(text(r.output.get('/i.html')?.bytes), 'Docs:[Home /][About /about/] 3 P\n');
});

test('AS csv: rows are objects; FOR renders a page per row; SORT BY sorts numeric strings numerically', async () => {
  const r = await run({
    'people.csv': 'slug,name,age\nb,"Bo, Jr.",9\na,Al,10\nc,Cy,100\n',
    Sitefile: [
      'FROM scratch',
      'IMPORT people FROM ./people.csv AS csv SORT BY age DESC',
      'RENDER <<T TO /index.txt',
      '${#each people as p}${p.name}=${p.age};${/each}',
      'T',
      'RENDER --escape=html <<T TO /p/${person.slug}.html FOR person IN people',
      '<h1>${person.name}</h1>',
      'T',
      'EXPORT /',
    ].join('\n'),
  });
  assert.equal(text(r.output.get('/index.txt')?.bytes), 'Cy=100;Al=10;Bo, Jr.=9;\n');
  assert.deepEqual(r.output.entries().map(([p]) => p), ['/index.txt', '/p/a.html', '/p/b.html', '/p/c.html']);
  assert.equal(text(r.output.get('/p/b.html')?.bytes), '<h1>Bo, Jr.</h1>\n');
});

test('a directory AS json or AS yaml is a collection with slug, date and path', async () => {
  const r = await run({
    'items/2026-01-02-alpha.json': '{"title":"Alpha","rank":2}',
    'items/beta.json': '{"title":"Beta","rank":1,"slug":"b"}',
    'items/readme.txt': 'ignored',
    'cfg/a.yaml': 'title: A\n',
    'cfg/b.yml': '- 1\n- 2\n',
    Sitefile: [
      'FROM scratch',
      'IMPORT items FROM ./items AS json SORT BY rank',
      'IMPORT cfg FROM ./cfg AS yaml',
      'RENDER <<T TO /out.txt',
      '${#each items as i}${i.slug}|${i.title}|${#if i.date}${i.date}${else}-${/if}|${i.path};${/each}',
      '${#each cfg as c}${c.slug}:${#if c.title}${c.title}${else}${c.value.1}${/if};${/each}',
      'T',
      'EXPORT /',
    ].join('\n'),
  });
  assert.equal(
    text(r.output.get('/out.txt')?.bytes),
    'b|Beta|-|/beta.json;alpha|Alpha|2026-01-02|/2026-01-02-alpha.json;\na:A;b:2;\n',
  );
});

test('AS wbn: the bundle as data, with text content for textual responses', async () => {
  const r = await run({
    'b.wbn': bundle(),
    Sitefile: [
      'FROM scratch',
      'IMPORT b FROM ./b.wbn AS wbn',
      'RENDER <<T TO /list.txt',
      '${b.version} ${b.primaryUrl}',
      '${#each b.entries as e}${e.path} ${e.status} ${e.contentType} ${e.size} ${#if e.content}<${e.content|raw}>${else}binary${/if}',
      '${/each}',
      'T',
      'EXPORT /',
    ].join('\n'),
  });
  const out = text(r.output.get('/list.txt')?.bytes);
  assert.match(out, /^b2 https:\/\/bundle\.example\/\n/);
  assert.match(out, /\/app\.js 200 text\/javascript; charset=utf-8 14 <console\.log\(1\)>/);
  assert.match(out, /\/logo\.png 200 image\/png 5 binary/);
  assert.match(out, /\/index\.html 200 text\/html; charset=utf-8 11 <<h1>hi<\/h1>>/);
});

test('AS tree on a .wbn unpacks it into files', async () => {
  const r = await run({
    'b.wbn': bundle(),
    Sitefile: 'FROM scratch\nIMPORT b FROM ./b.wbn AS tree\nCOPY b:/ /vendor/\nEXPORT /',
  });
  assert.deepEqual(r.output.entries().map(([p]) => p), ['/vendor/app.js', '/vendor/index.html', '/vendor/logo.png']);
  assert.equal(text(r.output.get('/vendor/index.html')?.bytes), '<h1>hi</h1>');
});

test('data import errors', async () => {
  await rejects({ 'a.yaml': 'a: &x 1\nb: *x\n', Sitefile: 'FROM scratch\nIMPORT a FROM ./a.yaml AS yaml\nEXPORT /' }, 'E_DATA');
  await rejects({ 'a.csv': 'x,y\n1\n', Sitefile: 'FROM scratch\nIMPORT a FROM ./a.csv AS csv\nEXPORT /' }, 'E_DATA');
  await rejects({ 'a.wbn': 'not a bundle at all', Sitefile: 'FROM scratch\nIMPORT a FROM ./a.wbn AS wbn\nEXPORT /' }, 'E_ARCHIVE');
  await rejects(
    { 'a.yaml': 'k: v\n', Sitefile: 'FROM scratch\nIMPORT a FROM ./a.yaml AS yaml SORT BY k\nEXPORT /' },
    'E_TYPE',
    /SORT BY needs a list/,
  );
  await rejects({ 'a.yaml': 'k: v\n', Sitefile: 'FROM scratch\nIMPORT a FROM ./a.yaml AS yaml\nCOPY a: /a\nEXPORT /' }, 'E_TYPE');
  assert.throws(
    () => parseProgram('FROM scratch\nIMPORT a FROM ./a.txt AS text SORT BY x\nEXPORT /'),
    (e: unknown) => isSitefileError(e, 'E_VALIDATION') && e.span?.line === 2,
  );
});
