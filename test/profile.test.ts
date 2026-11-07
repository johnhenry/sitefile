import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { cp, mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Profiler } from '../src/profile.js';
import { build } from '../src/executor.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-profile-'));
  temps.push(d);
  return d;
}

function sitefile(args: string[], home: string): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd: root, env: { ...process.env, SITEFILE_HOME: home } }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

interface Event {
  name: string;
  cat?: string;
  ph: string;
  ts?: number;
  dur?: number;
  pid: number;
  tid: number;
  args: Record<string, unknown>;
}

async function trace(file: string): Promise<Event[]> {
  const doc = JSON.parse(await readFile(file, 'utf8')) as { traceEvents: Event[]; displayTimeUnit: string };
  assert.equal(doc.displayTimeUnit, 'ms');
  for (const e of doc.traceEvents) {
    assert.equal(e.pid, 1);
    if (e.ph === 'X') {
      assert.ok(Number.isInteger(e.ts) && e.ts! >= 0, `${e.name}: ts`);
      assert.ok(Number.isInteger(e.dur) && e.dur! >= 0, `${e.name}: dur`);
    } else assert.equal(e.ph, 'M');
  }
  return doc.traceEvents;
}

const spans = (events: Event[]): string[] => events.filter((e) => e.ph === 'X').map((e) => `${e.cat} ${e.name}`);

test('Profiler: events in reserved order; open events end incomplete; unstarted ones are left out', () => {
  const p = new Profiler('build');
  const a = p.begin('a', 'build', { x: 1 });
  const b = p.reserve('b', 'import', {}, 2);
  p.nameThread(2, 'import b');
  p.reserve('never', 'import', {}, 3);
  b.start();
  b.end({ kind: 'file' });
  a.end({ x: 2, y: true });
  p.begin('open', 'operation');
  const doc = JSON.parse(p.toJSON()) as { traceEvents: Event[] };
  const ev = doc.traceEvents;
  assert.deepEqual(
    ev.map((e) => e.name),
    ['process_name', 'thread_name', 'thread_name', 'a', 'b', 'open'],
  );
  assert.deepEqual(ev[3]!.args, { x: 2, y: true });
  assert.equal(ev[4]!.tid, 2);
  assert.deepEqual(ev[5]!.args, { incomplete: true });
});

test('build --profile: imports, stages, operations and the export, in Sitefile order', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'blog');
  await cp(path.join(root, 'examples', 'blog'), ctx, { recursive: true });
  const prof = path.join(work, 'trace.json');
  const r = await sitefile(['build', ctx, '-o', path.join(work, 'out'), '--no-lockfile', '--profile', prof], path.join(work, 'home'));
  assert.equal(r.code, 0, r.stderr);
  const ev = await trace(prof);
  const label = path.relative(root, path.join(ctx, 'Sitefile'));
  assert.deepEqual(spans(ev), [
    'command build',
    'build parse',
    'import import posts',
    'stage stage #1',
    `operation ${label}:14 COPY`,
    `operation ${label}:15 RENDER`,
    `operation ${label}:16 RENDER`,
    `operation ${label}:20 FEED`,
    `operation ${label}:21 SITEMAP`,
    `operation ${label}:25 FINGERPRINT`,
    `operation ${label}:28 CHECKLINKS`,
    'export export',
  ]);
  const imp = ev.find((e) => e.name === 'import posts')!;
  assert.equal(imp.tid, 2);
  assert.deepEqual(imp.args, { uri: './posts', as: 'markdown', kind: 'value' });
  assert.ok(ev.some((e) => e.ph === 'M' && e.name === 'thread_name' && e.tid === 2 && e.args['name'] === 'import posts'));
  assert.deepEqual(ev.find((e) => e.name.endsWith(':21 SITEMAP'))!.args, { line: 21, files: 7 });
  assert.deepEqual(ev.find((e) => e.name === 'export')!.args, { format: 'directory' });
  // The command spans everything else.
  const cmd = ev.find((e) => e.cat === 'command')!;
  for (const e of ev.filter((x) => x.ph === 'X' && x.tid === 1)) assert.ok(e.ts! >= cmd.ts! && e.ts! + e.dur! <= cmd.ts! + cmd.dur!, e.name);
});

test('build --profile: TRANSFORM calls record cache misses, then hits', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'wasm');
  await cp(path.join(root, 'examples', 'wasm'), ctx, { recursive: true });
  const prof = path.join(work, 'trace.json');
  const home = path.join(work, 'home');
  const args = ['build', ctx, '-o', path.join(work, 'out'), '--force', '--no-lockfile', '--profile', prof];
  assert.equal((await sitefile(args, home)).code, 0);
  const first = (await trace(prof)).filter((e) => e.cat === 'transform');
  assert.ok(first.length > 0);
  assert.ok(first.every((e) => e.args['cache'] === 'miss' && e.args['tool'] === false && e.name === `TRANSFORM ${e.args['plugin']} ${e.args['dest']}`));
  assert.ok((await trace(prof)).some((e) => e.cat === 'plugin' && e.name.startsWith('PLUGIN ')));
  assert.equal((await sitefile(args, home)).code, 0);
  assert.ok((await trace(prof)).filter((e) => e.cat === 'transform').every((e) => e.args['cache'] === 'hit'));
  assert.equal((await sitefile([...args, '--no-cache'], home)).code, 0);
  assert.ok((await trace(prof)).filter((e) => e.cat === 'transform').every((e) => e.args['cache'] === 'off'));
});

test('build --profile: a failed build still writes the trace, with the failing step incomplete', async () => {
  const work = await tmp();
  await writeFile(path.join(work, 'a.txt'), 'a');
  await writeFile(path.join(work, 'b.txt'), 'b');
  await writeFile(path.join(work, 'Sitefile'), 'FROM scratch\nCOPY ./a.txt /x.txt\nCOPY ./b.txt /x.txt\nEXPORT /\n');
  const prof = path.join(work, 'trace.json');
  const r = await sitefile(['build', work, '-o', path.join(work, 'out'), '--no-lockfile', '--profile', prof], path.join(work, 'home'));
  assert.equal(r.code, 1);
  assert.match(r.stderr, /error\[E_CONFLICT\]/);
  const ev = (await trace(prof)).filter((e) => e.ph === 'X');
  assert.deepEqual(
    ev.map((e) => [e.name.replace(/^.*Sitefile:/, ''), e.args['incomplete'] ?? false]),
    [
      ['build', false],
      ['parse', false],
      ['stage #1', true],
      ['2 COPY', false],
      ['3 COPY', true],
    ],
  );
});

test('--profile: a trace inside the context is not an input; validate and inspect; unwritable path', async () => {
  const work = await tmp();
  await mkdir(path.join(work, 'src'));
  await writeFile(path.join(work, 'src', 'a.txt'), 'a');
  await writeFile(path.join(work, 'Sitefile'), 'FROM scratch\nCOPY ./ /\nEXPORT /\n');
  const home = path.join(work, 'home');
  const prof = path.join(work, 'src', 'trace.json');
  const out = path.join(work, 'out');
  for (let i = 0; i < 2; i++) {
    const r = await sitefile(['build', work, '-o', out, '--force', '--no-lockfile', '--profile', prof], home);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stdout, /Exported 2 files/); // Sitefile and src/a.txt, never src/trace.json
  }
  const v = await sitefile(['validate', work, '--profile', prof], home);
  assert.equal(v.code, 0, v.stderr);
  assert.deepEqual(spans(await trace(prof)), ['command validate', 'build parse']);
  const i = await sitefile(['inspect', work, '--profile', prof], home);
  assert.equal(i.code, 0, i.stderr);
  assert.deepEqual(spans(await trace(prof)).slice(0, 2), ['command inspect', 'build parse']);
  assert.ok(!spans(await trace(prof)).includes('export export'));

  const bad = await sitefile(['build', work, '-o', out, '--force', '--no-lockfile', '--profile', path.join(work, 'missing', 'trace.json')], home);
  assert.equal(bad.code, 1);
  assert.match(bad.stderr, /error\[E_EXPORT\]: cannot write the --profile file '.*missing\/trace\.json'/);
  const empty = await sitefile(['build', work, '--profile', ''], home);
  assert.equal(empty.code, 2);
  assert.match(empty.stderr, /--profile expects a file path/);
});

test('build() with a profiler (library API)', async () => {
  const work = await tmp();
  await writeFile(path.join(work, 'Sitefile'), 'STAGE a FROM scratch\nCOPY <<T /a.txt\nhi\nT\nFROM a\nCOPY --from=a /a.txt /b.txt\nEXPORT /\n');
  const p = new Profiler('build');
  await build({ contextDir: work, lockfile: false, profiler: p });
  const names = (JSON.parse(p.toJSON()) as { traceEvents: Event[] }).traceEvents.filter((e) => e.ph === 'X').map((e) => [e.name.replace(/^.*Sitefile:/, ''), e.args['from']]);
  assert.deepEqual(names, [
    ['parse', undefined],
    ['stage a', 'scratch'],
    ['2 COPY', undefined],
    ['stage #2', 'a'],
    ['6 COPY', undefined],
  ]);
});

test('--profile with --target traces only the stages built; why and graph are traced too', async () => {
  const work = await tmp();
  await writeFile(path.join(work, 'Sitefile'), 'STAGE a FROM scratch\nCOPY <<T /a.txt\nhi\nT\nSTAGE b FROM a\nCOPY --from=a /a.txt /b.txt\nEXPORT /\n');
  const home = path.join(work, 'home');
  const prof = path.join(work, 'trace.json');
  const short = async (): Promise<string[]> => (await trace(prof)).filter((e) => e.ph === 'X').map((e) => `${e.cat} ${e.name.replace(/^.*Sitefile:/, '')}`);
  const t = await sitefile(['build', work, '-o', path.join(work, 'out'), '--no-lockfile', '--target', 'a', '--profile', prof], home);
  assert.equal(t.code, 0, t.stderr);
  assert.deepEqual(await short(), ['command build', 'build parse', 'stage stage a', 'operation 2 COPY', 'export export']);
  assert.equal((await trace(prof)).find((e) => e.name === 'parse')!.args['target'], 'a');
  const w = await sitefile(['why', '/b.txt', work, '--no-lockfile', '--profile', prof], home);
  assert.equal(w.code, 0, w.stderr);
  assert.match(w.stdout, /b\.txt/);
  assert.deepEqual(await short(), ['command why', 'build parse', 'stage stage a', 'operation 2 COPY', 'stage stage b', 'operation 6 COPY']);
  const g = await sitefile(['graph', work, '--profile', prof], home);
  assert.equal(g.code, 0, g.stderr);
  assert.deepEqual(await short(), ['command graph', 'build parse']);
});
