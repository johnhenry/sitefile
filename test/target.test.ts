import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { cp, mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseProgram } from '../src/language/validate.js';
import { plan, targetPlan } from '../src/planner.js';
import { isSitefileError } from '../src/errors.js';

// `--target <stage>` (issue #83): build up to a named stage and export its whole tree.

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-target-'));
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

async function multistage(): Promise<{ work: string; ctx: string; home: string }> {
  const work = await tmp();
  const ctx = path.join(work, 'multistage');
  await cp(path.join(root, 'examples', 'multistage'), ctx, { recursive: true });
  await rm(path.join(ctx, 'dist'), { recursive: true, force: true });
  return { work, ctx, home: path.join(work, '.home') };
}

async function files(dir: string, rel = ''): Promise<string[]> {
  const out: string[] = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (e.isDirectory()) out.push(...(await files(path.join(dir, e.name), `${rel}/${e.name}`)));
    else out.push(`${rel}/${e.name}`);
  }
  return out.sort();
}

test('targetPlan keeps the stages up to the target and exports its whole tree without site files', () => {
  const p = plan(parseProgram('STAGE a\nCOPY ./x /x\nSTAGE b FROM a\nREDIRECT /o /n\nHEADER /* X-A: 1\nEXPORT /site AS zip', 'Sitefile'));
  const t = targetPlan(p, 'a');
  assert.deepEqual(t.stages.map((s) => s.name), ['a']);
  assert.equal(t.exportPath, '/');
  assert.equal(t.exportFormat, 'zip');
  assert.equal(t.redirects.length, 0);
  assert.equal(t.headers.length, 0);
  assert.equal(t.target, 'a');
  assert.equal(targetPlan(p, 'b').stages.length, 2);
  assert.equal(p.target, undefined);
  assert.throws(
    () => targetPlan(p, 'c'),
    (e: unknown) => isSitefileError(e) && e.code === 'E_USAGE' && e.detail === "--target 'c' does not name a stage (stages: a, b)",
  );
  const unnamed = plan(parseProgram('FROM scratch\nEXPORT /', 'Sitefile'));
  assert.throws(() => targetPlan(unnamed, 'a'), /the Sitefile has no named stages/);
});

test('build --target exports an earlier stage; the last stage is not run', async () => {
  const { work, ctx, home } = await multistage();
  const out = path.join(work, 'out');
  const r = await sitefile(['build', ctx, '--target', 'theme', '-o', out, '--no-lockfile'], home);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /^Exported 2 files to /);
  assert.deepEqual(await files(out), ['/css/site.css', '/vendor/index.html']);
  assert.equal(await readFile(path.join(out, 'vendor', 'index.html'), 'utf8'), '<h1>Home</h1>\n');

  const assets = await sitefile(['build', ctx, '--target', 'assets', '-o', out, '--force', '--no-lockfile'], home);
  assert.equal(assets.code, 0, assets.stderr);
  assert.deepEqual(await files(out), ['/static/site.css']);
});

test('--target applies the EXPORT format but not its path or _redirects/_headers', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'ctx');
  await cp(path.join(root, 'examples', 'multistage'), ctx, { recursive: true });
  await writeFile(path.join(ctx, 'Sitefile'), 'STAGE build\nCOPY <<X /out/a.txt\na\nX\nSTAGE site\nCOPY --from=build /out/ /site/\nREDIRECT /old /new\nEXPORT /site AS zip\n');
  const home = path.join(work, '.home');
  const r = await sitefile(['build', ctx, '--target', 'build', '--no-lockfile'], home);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /Exported 1 file to .*dist\.zip \(zip, /);
  const i = await sitefile(['inspect', ctx, '--target', 'build', '--no-lockfile', '--json'], home);
  assert.equal(i.code, 0, i.stderr);
  assert.deepEqual(JSON.parse(i.stdout).files.map((f: { path: string }) => f.path), ['/out/a.txt']);
  const full = await sitefile(['inspect', ctx, '--no-lockfile', '--json'], home);
  assert.deepEqual(JSON.parse(full.stdout).files.map((f: { path: string }) => f.path), ['/_redirects', '/a.txt']);
});

test('validate --target reports the target; unknown targets are usage errors naming the stages', async () => {
  const { ctx, home } = await multistage();
  const v = await sitefile(['validate', ctx, '--target', 'assets'], home);
  assert.equal(v.code, 0, v.stderr);
  assert.match(v.stdout, /OK \(0 imports, 2 operations, 2 stages, target assets\)\n$/);
  const j = await sitefile(['validate', ctx, '--target', 'theme', '--json'], home);
  const json = JSON.parse(j.stdout);
  assert.equal(json.target, 'theme');
  assert.equal(json.export, '/');
  assert.equal(json.stages.length, 1);
  assert.equal(JSON.parse((await sitefile(['validate', ctx, '--json'], home)).stdout).target, undefined);

  for (const cmd of ['build', 'validate', 'inspect']) {
    const bad = await sitefile([cmd, ctx, '--target', 'nope', '--no-lockfile'], home);
    assert.equal(bad.code, 2, cmd);
    assert.equal(bad.stderr, "error: --target 'nope' does not name a stage (stages: theme, assets)\nRun 'sitefile --help' for usage.\n");
  }
});

test('serve: --target needs --watch', async () => {
  const { ctx, home } = await multistage();
  const r = await sitefile(['serve', '--target', 'theme', ctx], home);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /^error: --target needs --watch\n/);
});
