import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build, fileDest, runBounded, starName, type BuildOptions, type BuildResult } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { BuildMemo } from '../src/memo.js';
import { Profiler } from '../src/profile.js';
import { why } from '../src/explain.js';

// TRANSFORM over a directory (issue #127): one plugin or tool call per file, in path order.

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string | Uint8Array>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-tdir-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

/** A JS plugin: a directory destination gets `<stem>.txt` describing the request; a file destination gets the description. */
const ECHO = `export async function transform(r) {
  const what = r.input.path + ' ' + r.input.name + ' -> ' + r.dest + (r.options.tag ? ' ' + r.options.tag : '');
  if (r.input.name === 'bad.txt') throw new Error('cannot read ' + r.input.name);
  if (/\\.[a-z]+$/.test(r.dest)) return what;
  return { files: { [r.input.name.replace(/\\.[^.]*$/, '') + '.txt']: what } };
}
`;

async function echoSite(lines: string, files: Record<string, string>): Promise<string> {
  return project({ 'plugins/echo.mjs': ECHO, ...files, Sitefile: `PLUGIN echo FROM ./plugins/echo.mjs\n${lines}\nEXPORT /\n` });
}

const opts = (dir: string, extra: Partial<BuildOptions> = {}): BuildOptions => ({ contextDir: dir, lockfile: false, allowPlugin: ['echo'], ...extra });
const text = (r: BuildResult, p: string): string => new TextDecoder().decode(r.output.get(p)!.bytes);
/** An origin without the context path in front of the Sitefile label. */
const origin = (o: string): string => o.replace(/^.*Sitefile:/, 'Sitefile:');

async function fails(dir: string, code: ErrorCode, extra: Partial<BuildOptions> = {}): Promise<string> {
  let message = '';
  await assert.rejects(build(opts(dir, extra)), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    message = e.message;
    return true;
  });
  return message;
}

const PHOTOS = { 'photos/b.jpg': 'b', 'photos/a.png': 'a', 'photos/trip/c.jpg': 'c', 'photos/trip/notes.md': 'n', 'photos/drafts/d.jpg': 'd' };

test('destinations: a directory per file, *.ext and *', () => {
  assert.equal(fileDest('/img/', 'a.jpg'), '/img/');
  assert.equal(fileDest('/img', 'trip/a.jpg'), '/img/trip/');
  assert.equal(fileDest('/', 'trip/a.jpg'), '/trip/');
  assert.equal(fileDest('/t/*.png', 'trip/a.jpg'), '/t/trip/a.png');
  assert.equal(fileDest('/t/*.png', 'a.tar.gz'), '/t/a.tar.png');
  assert.equal(fileDest('/t/*.png', 'README'), '/t/README.png');
  assert.equal(fileDest('/t/*.png', '.hidden'), '/t/.hidden.png');
  assert.equal(fileDest('/t/*', 'trip/a.jpg'), '/t/trip/a.jpg');
  assert.equal(fileDest('/*.css', 'x/a.scss'), '/x/a.css');
  assert.equal(starName('/a/*.png'), '*.png');
  assert.equal(starName('/a/*'), '*');
  for (const d of ['/a/', '/a/x*.png', '/a/*.', '/a/*.p*', '/a*/b', '/a/**']) assert.equal(starName(d), undefined, d);
});

test('runBounded: index order, a bounded number at once, and no new starts after a failure', async () => {
  let running = 0;
  let peak = 0;
  const started: number[] = [];
  const ok = await runBounded(10, 3, async (i) => {
    started.push(i);
    running++;
    peak = Math.max(peak, running);
    await new Promise((r) => setTimeout(r, (10 - i) * 2));
    running--;
    return i * 2;
  });
  assert.equal(peak, 3);
  assert.deepEqual(started.slice(0, 3), [0, 1, 2]);
  assert.deepEqual(ok, Array.from({ length: 10 }, (_, i) => ({ ok: true, value: i * 2 })));
  // 1 fails at once: 0 and 2 (already started) finish, nothing later starts.
  started.length = 0;
  const failed = await runBounded(10, 3, async (i) => {
    started.push(i);
    if (i === 1) throw new Error('one');
    await new Promise((r) => setTimeout(r, 20));
    return i;
  });
  assert.deepEqual(started, [0, 1, 2]);
  assert.deepEqual(failed.slice(0, 3).map((r) => r?.ok), [true, false, true]);
  assert.ok(failed.slice(3).every((r) => r === undefined));
});

test('a directory source: one call per file in path order, each with its own input and destination', async () => {
  const dir = await echoSite('TRANSFORM ./photos/ WITH echo TO /out OPTIONS tag=t', PHOTOS);
  const r = await build(opts(dir));
  assert.deepEqual(
    r.output.entries().map(([p, n]) => [p, origin(n.origin), new TextDecoder().decode(n.bytes)]),
    [
      ['/out/a.txt', 'Sitefile:2 TRANSFORM ./photos/a.png', './photos/a.png a.png -> /out t'],
      ['/out/b.txt', 'Sitefile:2 TRANSFORM ./photos/b.jpg', './photos/b.jpg b.jpg -> /out t'],
      ['/out/drafts/d.txt', 'Sitefile:2 TRANSFORM ./photos/drafts/d.jpg', './photos/drafts/d.jpg d.jpg -> /out/drafts t'],
      ['/out/trip/c.txt', 'Sitefile:2 TRANSFORM ./photos/trip/c.jpg', './photos/trip/c.jpg c.jpg -> /out/trip t'],
      ['/out/trip/notes.txt', 'Sitefile:2 TRANSFORM ./photos/trip/notes.md', './photos/trip/notes.md notes.md -> /out/trip t'],
    ],
  );
  assert.equal(r.transformStats.pluginCalls, 5);
  assert.equal(r.transformStats.pluginsSpawned, 1);
});

test('--include and --exclude pick the files; .sitefileignore applies; * and *.ext name one file per input', async () => {
  const dir = await echoSite(
    [
      'TRANSFORM --include=*.jpg --exclude=drafts/ ./photos/ WITH echo TO /jpg/*.webp',
      'TRANSFORM --include=trip/** --include=a.png --exclude=*.md ./photos WITH echo TO /same/*',
    ].join('\n'),
    { ...PHOTOS, '.sitefileignore': 'photos/b.jpg\n' },
  );
  const r = await build(opts(dir));
  assert.deepEqual(
    r.output.entries().map(([p]) => [p, text(r, p)]),
    [
      ['/jpg/trip/c.webp', './photos/trip/c.jpg c.jpg -> /jpg/trip/c.webp'],
      ['/same/a.png', './photos/a.png a.png -> /same/a.png'],
      ['/same/trip/c.jpg', './photos/trip/c.jpg c.jpg -> /same/trip/c.jpg'],
    ],
  );
});

test('import and stage directories; their labels name each file', async () => {
  const dir = await echoSite(
    [
      'IMPORT pics FROM ./photos AS tree',
      'STAGE s',
      'COPY ./photos/trip /in/',
      'FROM scratch',
      'TRANSFORM --exclude=*.md pics:/trip WITH echo TO /a/*.x',
      'TRANSFORM --exclude=*.md --from=s /in WITH echo TO /b/*.x',
      'TRANSFORM --include=a.png pics: WITH echo TO /c/*.x',
    ].join('\n'),
    PHOTOS,
  );
  const r = await build(opts(dir));
  assert.equal(text(r, '/a/c.x'), 'pics:/trip/c.jpg c.jpg -> /a/c.x');
  assert.equal(text(r, '/b/c.x'), '--from=s /in/c.jpg c.jpg -> /b/c.x');
  assert.equal(text(r, '/c/a.x'), 'pics:/a.png a.png -> /c/a.x');
});

test('errors: what --include/--exclude, * and FOR accept', async () => {
  const syntax = (line: string): string => {
    try {
      parseProgram(`PLUGIN echo FROM ./plugins/echo.mjs\n${line}\nEXPORT /\n`);
    } catch (e) {
      assert.ok(isSitefileError(e));
      assert.equal(e.code, 'E_SYNTAX', e.message);
      return e.message;
    }
    assert.fail(`${line} parsed`);
  };
  assert.match(syntax('TRANSFORM --include=#x ./photos WITH echo TO /o/'), /--include=#x is not a pattern/);
  assert.match(syntax('TRANSFORM --exclude=a --include=/ ./photos WITH echo TO /o/'), /--include=\/ is not a pattern/);
  assert.match(syntax('TRANSFORM --include=*.jpg WITH echo TO /o/ FOR p IN ps'), /--include needs a source directory/);
  assert.match(syntax('TRANSFORM --exclude=x <<EOF WITH echo TO /o\nbody\nEOF'), /--exclude cannot be combined with an inline/);
  assert.match(syntax('TRANSFORM --exclude=x ./photos WITH echo TO /o/ FOR p IN ps'), /--exclude filters a directory source, which cannot be combined with FOR/);
  assert.match(syntax('TRANSFORM --include ./photos WITH echo TO /o/'), /--include requires a value/);

  const file = await echoSite('TRANSFORM --include=*.png ./photos/a.png WITH echo TO /o/', PHOTOS);
  assert.match(await fails(file, 'E_TYPE'), /--include and --exclude filter a directory, but '.\/photos\/a.png' is a file/);
  const star = await echoSite('TRANSFORM ./photos/a.png WITH echo TO /o/*.webp', PHOTOS);
  assert.match(await fails(star, 'E_TYPE'), /a '\*' name stands for each file of a directory source/);
  const each = await echoSite('IMPORT l FROM ./l.json AS json\nTRANSFORM ./photos WITH echo TO /o/${i}/ FOR i IN l', { ...PHOTOS, 'l.json': '["x"]' });
  assert.match(await fails(each, 'E_TYPE'), /'.\/photos' is a directory, which runs the plugin once per file and cannot be combined with FOR/);
  const value = await echoSite('IMPORT l FROM ./l.json AS json\nTRANSFORM --exclude=x l: WITH echo TO /o/', { 'l.json': '["x"]' });
  assert.match(await fails(value, 'E_TYPE'), /filter a directory, but 'l:' is a value import/);
  const empty = await echoSite('TRANSFORM --include=*.gif ./photos WITH echo TO /o/\nCOPY ./photos/a.png /a.png', PHOTOS);
  const warnings: string[] = [];
  await build(opts(empty, { onWarning: (w) => warnings.push(w) }));
  assert.ok(warnings.some((w) => w.endsWith("Sitefile:2 TRANSFORM: --include/--exclude leave no files of './photos' to transform")), warnings.join('\n'));
});

test('a failing file fails the build, named; a plugin that returns one file needs * or *.ext', async () => {
  const bad = await echoSite('TRANSFORM ./in/ WITH echo TO /o/', { 'in/a.txt': 'a', 'in/bad.txt': 'x', 'in/c.txt': 'c' });
  assert.match(await fails(bad, 'E_PLUGIN'), /Sitefile:2:1: \.\/in\/bad\.txt: PLUGIN echo: cannot read bad\.txt/);
  const one = await project({
    'plugins/one.mjs': 'export const transform = (r) => "x";\n',
    'in/a.txt': 'a',
    Sitefile: 'PLUGIN one FROM ./plugins/one.mjs\nTRANSFORM ./in WITH one TO /o\nEXPORT /\n',
  });
  assert.match(await fails(one, 'E_PLUGIN', { allowPlugin: ['one'] }), /\.\/in\/a\.txt: PLUGIN one returned one file, but '\/o\/' is a directory destination \(use '\/o\/\*' or '\/o\/\*\.ext'/);
});

test('two inputs writing one path: identical content is kept once, different content is E_CONFLICT naming both', async () => {
  const plugin = 'export const transform = (r) => ({ files: { "same.txt": r.options.v ?? r.input.text } });\n';
  const same = await project({ 'plugins/p.mjs': plugin, 'in/a.txt': 'A', 'in/b.txt': 'B', Sitefile: 'PLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./in WITH p TO /o OPTIONS v=1\nEXPORT /\n' });
  const r = await build(opts(same, { allowPlugin: ['p'] }));
  assert.deepEqual(r.output.entries().map(([p, n]) => [p, origin(n.origin)]), [['/o/same.txt', 'Sitefile:2 TRANSFORM ./in/a.txt']]);
  const differ = await project({ 'plugins/p.mjs': plugin, 'in/a.txt': 'A', 'in/b.txt': 'B', Sitefile: 'PLUGIN p FROM ./plugins/p.mjs\nTRANSFORM --overwrite ./in WITH p TO /o\nEXPORT /\n' });
  assert.match(
    await fails(differ, 'E_CONFLICT', { allowPlugin: ['p'] }),
    /'\/o\/same\.txt' is written for two inputs with different content \(\S*Sitefile:2 TRANSFORM \.\/in\/a\.txt and \S*Sitefile:2 TRANSFORM \.\/in\/b\.txt\)/,
  );
});

test('results are cached per file, under the same key as a one-file TRANSFORM', async () => {
  const cacheDir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-tdir-cache-'));
  temps.push(cacheDir);
  const one = await echoSite('TRANSFORM ./photos/a.png WITH echo TO /out/', PHOTOS);
  const first = await build(opts(one, { cacheDir }));
  assert.equal(first.transformStats.misses, 1);
  const all = await echoSite('TRANSFORM ./photos/ WITH echo TO /out/', PHOTOS);
  const second = await build(opts(all, { cacheDir }));
  assert.equal(second.transformStats.hits, 1); // a.png, from the one-file build
  assert.equal(second.transformStats.misses, 4);
  const third = await build(opts(all, { cacheDir }));
  assert.equal(third.transformStats.hits, 5);
  assert.equal(third.transformStats.pluginsSpawned, 0);
  assert.equal(third.output.digest(), second.output.digest());
});

test('incremental rebuilds re-run only the files that changed; --profile has one event per file', async () => {
  const dir = await echoSite('TRANSFORM --exclude=*.md ./photos/ WITH echo TO /out/', PHOTOS);
  const memo = new BuildMemo();
  const first = await build(opts(dir, { memo }));
  assert.equal(first.transformStats.pluginCalls, 4);
  const again = await build(opts(dir, { memo }));
  assert.equal(again.transformStats.pluginCalls, 0);
  await writeFile(path.join(dir, 'photos/trip/c.jpg'), 'changed');
  await writeFile(path.join(dir, 'photos/e.jpg'), 'new');
  const profiler = new Profiler('build');
  const edited = await build(opts(dir, { memo, profiler }));
  assert.equal(edited.transformStats.pluginCalls, 2);
  const clean = await build(opts(dir));
  assert.equal(edited.output.digest(), clean.output.digest());
  const events = (JSON.parse(profiler.toJSON()) as { traceEvents: Array<{ name: string; cat: string; args: Record<string, unknown> }> }).traceEvents.filter((e) => e.cat === 'transform');
  assert.deepEqual(
    events.map((e) => [e.name, e.args['input'], e.args['cache']]),
    [
      ['TRANSFORM echo /out', './photos/e.jpg', 'off'],
      ['TRANSFORM echo /out/trip', './photos/trip/c.jpg', 'off'],
    ],
  );
});

test('sitefile why names the input file of a directory TRANSFORM', async () => {
  const dir = await echoSite('TRANSFORM ./photos/ WITH echo TO /out/', PHOTOS);
  const r = await build(opts(dir, { recordSteps: true }));
  const report = why(r, '/out/trip/c.txt', dir);
  assert.deepEqual(report.history.map((e) => e.inputs), [['./photos/trip/c.jpg', 'plugin echo']]);
});

/** A tool that copies its input, sleeping longer for earlier files and logging one line each. */
const SLOW = (execPath: string): string => `#!${execPath}
const fs = require('node:fs');
const [input, output] = process.argv.slice(2);
const body = fs.readFileSync(input, 'utf8');
console.error('log ' + body.trim());
setTimeout(() => fs.writeFileSync(output, body.toUpperCase()), Number(body.trim().length > 0 ? 400 - body.trim().charCodeAt(0) * 3 : 0));
`;

test('a TOOL over a directory: parallel runs, output and logs in path order, * / *.ext required', async () => {
  const dir = await project({ 'src/a.txt': 'a\n', 'src/b.txt': 'b\n', 'src/c/d.txt': 'd\n', 'src/e.md': 'e\n' });
  const bin = path.join(dir, 'bin', 'slow');
  await mkdir(path.dirname(bin), { recursive: true });
  await writeFile(bin, SLOW(process.execPath));
  await chmod(bin, 0o755);
  await writeFile(path.join(dir, 'Sitefile'), 'TOOL slow\nTRANSFORM --exclude=*.md ./src/ WITH slow TO /out/*.up ARGS {input} {output}\nEXPORT /\n');
  const warnings: string[] = [];
  const r = await build({ contextDir: dir, lockfile: false, allowExec: [`slow=${bin}`], onWarning: (w) => warnings.push(w) });
  assert.deepEqual(r.output.entries().map(([p]) => [p, text(r, p)]), [['/out/a.up', 'A\n'], ['/out/b.up', 'B\n'], ['/out/c/d.up', 'D\n']]);
  assert.deepEqual(warnings.filter((w) => w.includes('log ')).map((w) => w.replace(/^.*log /, '')), ['a', 'b', 'd']);
  assert.equal(r.transformStats.toolRuns, 3);

  await writeFile(path.join(dir, 'Sitefile'), 'TOOL slow\nTRANSFORM ./src WITH slow TO /out ARGS {input} {output}\nEXPORT /\n');
  await assert.rejects(build({ contextDir: dir, lockfile: false, allowExec: [`slow=${bin}`] }), (e: unknown) => {
    assert.ok(isSitefileError(e));
    assert.equal(e.code, 'E_PLUGIN');
    assert.match(e.message, /TOOL slow writes one file per input, but '\/out' is a directory destination: use '\/out\/\*' to keep each file's name, or '\/out\/\*\.ext'/);
    return true;
  });
});

test('the image plugin over a directory writes what one TRANSFORM per file writes', async () => {
  const wasm = await readFile(path.join(root, 'plugins', 'image', 'image.wasm'));
  const img = (n: string): Promise<Buffer> => readFile(path.join(root, 'plugins', 'image', 'testdata', n));
  const files = { 'plugins/image.wasm': wasm, 'in/a.png': await img('tiny.png'), 'in/b.jpg': await img('tiny.jpg'), 'in/c/d.gif': await img('tiny.gif'), 'in/c/r.jpg': await img('rotated.jpg') };
  const each = await project({
    ...files,
    Sitefile: [
      'PLUGIN image FROM ./plugins/image.wasm',
      ...['a.png', 'b.jpg'].map((f) => `TRANSFORM ./in/${f} WITH image TO /img/ OPTIONS widths=4,8 format=png,webp`),
      ...['d.gif', 'r.jpg'].map((f) => `TRANSFORM ./in/c/${f} WITH image TO /img/c/ OPTIONS widths=4,8 format=png,webp`),
      'TRANSFORM ./in/b.jpg WITH image TO /t/b.jpg OPTIONS widths=5',
      'TRANSFORM ./in/c/r.jpg WITH image TO /t/c/r.jpg OPTIONS widths=5',
      'EXPORT /',
    ].join('\n'),
  });
  const dir = await project({
    ...files,
    Sitefile: 'PLUGIN image FROM ./plugins/image.wasm\nTRANSFORM ./in WITH image TO /img/ OPTIONS widths=4,8 format=png,webp\nTRANSFORM --include=*.jpg ./in/ WITH image TO /t/* OPTIONS widths=5\nEXPORT /\n',
  });
  const a = await build({ contextDir: each, lockfile: false });
  const b = await build({ contextDir: dir, lockfile: false });
  assert.deepEqual(b.output.entries().map(([p]) => p), a.output.entries().map(([p]) => p));
  assert.equal(b.output.digest(), a.output.digest());
  assert.equal(b.transformStats.pluginsSpawned, 1);
});
