import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { cp, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { bakePlanJson, bakePlanText, buildArgv, parseBakeDefinition, resolveBake, type BakePlan } from '../src/bake.js';
import { isSitefileError } from '../src/errors.js';
import { EXPORT_FORMATS } from '../src/language/ast.js';
import { parseYaml } from '../src/yaml.js';

// `sitefile bake` (issue #92): several builds from one bake file.

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function plan(yaml: string, selected: string[] = []): BakePlan {
  return resolveBake(parseBakeDefinition(parseYaml(yaml, 'b.yaml'), 'b.yaml'), 'site/b.yaml', selected);
}
const names = (p: BakePlan): string[] => p.targets.map((t) => t.name);

function fails(yaml: string, code: string, detail: string, selected: string[] = []): void {
  assert.throws(
    () => plan(yaml, selected),
    (e: unknown) => {
      assert.ok(isSitefileError(e), String(e));
      assert.equal(e.code, code);
      assert.equal(e.detail, detail);
      return true;
    },
  );
}

test('defaults: context of the bake file, <context>/Sitefile, dist/<name> (with the format extension)', () => {
  const p = plan('targets:\n  a:\n  b:\n    context: ./b\n    format: zip\n  c:\n    output: /tmp/x/../out.zip\n');
  assert.deepEqual(names(p), ['a', 'b', 'c']);
  const [a, b, c] = p.targets;
  assert.deepEqual([a!.context, a!.file, a!.output], ['site', 'site/Sitefile', 'site/dist/a']);
  assert.deepEqual([b!.context, b!.file, b!.output, b!.format], ['site/b', 'site/b/Sitefile', 'site/dist/b.zip', 'zip']);
  assert.equal(c!.output, '/tmp/out.zip');
  assert.deepEqual(buildArgv(a!), ['build', 'site', '-f', 'site/Sitefile', '-o', 'site/dist/a']);
});

const MATRIX = `groups:
  default: [site]
  all: [default, other]
targets:
  base:
    args: {A: 1, B: x}
    tags: [t]
  site:
    inherits: base
    matrix:
      l: [en, fr]
      e: [s, p]
    args: {B: '\${l}-\${e}'}
    output: dist/\${l}/\${e}
    tags: ['img:\${l}-\${e}']
  other:
    target: build
    arg-files: [a.env]
`;

test('matrix: one target per combination, named <target>-<values>, first variable slowest; inherits merges args', () => {
  const p = plan(MATRIX);
  assert.deepEqual(names(p), ['site-en-s', 'site-en-p', 'site-fr-s', 'site-fr-p']);
  const t = p.targets[1]!;
  assert.deepEqual(t.args, { A: '1', B: 'en-p' });
  assert.equal(t.output, 'site/dist/en/p');
  assert.deepEqual(t.tags, ['img:en-p']);
  assert.deepEqual(t.matrix, { l: 'en', e: 'p' });
  assert.deepEqual(buildArgv(t), ['build', 'site', '-f', 'site/Sitefile', '-o', 'site/dist/en/p', '--build-arg', 'A=1', '--build-arg', 'B=en-p', '-t', 'img:en-p']);
  const o = plan(MATRIX, ['other']).targets[0]!;
  assert.deepEqual(buildArgv(o), ['build', 'site', '-f', 'site/Sitefile', '-o', 'site/dist/other', '--target', 'build', '--build-arg-file', 'site/a.env']);
});

test('selection: groups nest, a matrix target selects every variant, a variant selects itself, no duplicates', () => {
  assert.deepEqual(names(plan(MATRIX, ['all'])), ['site-en-s', 'site-en-p', 'site-fr-s', 'site-fr-p', 'other']);
  assert.deepEqual(names(plan(MATRIX, ['site-fr-p', 'other', 'site'])), ['site-fr-p', 'other', 'site-en-s', 'site-en-p', 'site-fr-s']);
  // Without a default group: every target in file order.
  assert.deepEqual(names(plan('targets:\n  b:\n  a:\n')), ['b', 'a']);
  fails(MATRIX, 'E_USAGE', "bake: no target or group named 'nope' (targets: base, site, other; groups: default, all)", ['nope']);
});

test('conflicts: same or nested outputs, shared tags', () => {
  fails('targets:\n  a:\n    output: out\n  b:\n    output: out/\n', 'E_CONFLICT', "bake targets 'a' and 'b' both write 'site/out'");
  fails('targets:\n  a:\n    output: out\n  b:\n    output: out/b\n', 'E_CONFLICT', "bake targets 'a' and 'b' write overlapping outputs 'site/out' and 'site/out/b'");
  fails('targets:\n  a:\n    tags: [x]\n  b:\n    tags: [y, x]\n', 'E_CONFLICT', "bake targets 'a' and 'b' both tag 'x'");
  // Only the selected targets are checked.
  assert.deepEqual(names(plan('targets:\n  a:\n    output: out\n  b:\n    output: out\n', ['a'])), ['a']);
});

test('bake file errors are E_DATA naming the file', () => {
  const data = (yaml: string, detail: string): void => fails(yaml, 'E_DATA', `b.yaml: ${detail}`);
  data('targets:\n  a:\n    inherits: b\n  b:\n    inherits: [a]\n', 'inherits cycle: a -> b -> a');
  data('targets:\n  a:\n    inherits: x\n', "target 'a' inherits unknown target 'x'");
  data('targets:\n  a:\n    matrix: {l: [en]}\n    output: ${x}\n', "target 'a-en': output: '${x}' is not a matrix variable (l)");
  data('targets:\n  a:\n    bogus: 1\n', "target 'a': unknown key 'bogus' (expected context, file, args, arg-files, target, output, format, tags, matrix, inherits)");
  data('targets:\n  a:\n    format: rar\n', `target 'a': format must be one of ${EXPORT_FORMATS.join(', ')} (or tar.gz), got 'rar'`);
  data('targets:\n  a:\n    args: {x-y: 1}\n', "target 'a': 'x-y' is not a valid ARG name");
  data('targets:\n  a:\n    args: {X: [1]}\n', "target 'a': args.X must be a string, number or boolean");
  data('targets:\n  a:\n    matrix: {l: []}\n', "target 'a': matrix.l must be a non-empty list of strings, numbers or booleans");
  data('targets:\n  a:\n    tags: x\n', "target 'a': tags must be a list of strings");
  data('groups:\n  a: [a]\ntargets:\n  a:\n', "'a' is both a target and a group");
  data('groups:\n  default: [g]\n  g: [default]\ntargets:\n  a:\n', "group 'default' includes itself (default -> g -> default)");
  data('groups:\n  default: [x]\ntargets:\n  a:\n', "group 'default': no target or group named 'x'");
  data("targets:\n  a:\n    matrix: {v: ['x y', 'x?y']}\n", "two targets are named 'a-x_y' (from 'a' and 'a')");
  data('targets: {}\n', 'targets must be a non-empty mapping of name: target');
  data('targets:\n  a b:\n', "'a b' is not a valid target name (letters, digits, '.', '_', '-')");
  data('target:\n  a:\n', "unknown key 'target' (expected targets, groups)");
});

test('--print text and JSON', () => {
  const p = plan(MATRIX, ['site-en-s', 'other']);
  assert.equal(
    bakePlanText(p),
    [
      'site-en-s',
      '  context   site',
      '  file      site/Sitefile',
      '  output    site/dist/en/s',
      '  arg       A=1',
      '  arg       B=en-s',
      '  tag       img:en-s',
      '  matrix    l=en e=s',
      'other',
      '  context   site',
      '  file      site/Sitefile',
      '  target    build',
      '  output    site/dist/other',
      '  arg-file  site/a.env',
      '2 targets from site/b.yaml',
      '',
    ].join('\n'),
  );
  const j = bakePlanJson(p) as { file: string; targets: Array<Record<string, unknown>> };
  assert.equal(j.file, 'site/b.yaml');
  assert.deepEqual(Object.keys(j.targets[0]!), ['name', 'context', 'file', 'args', 'argFiles', 'target', 'output', 'format', 'tags', 'matrix']);
  assert.equal(j.targets[1]!['matrix'], null);
});

// ---------------------------------------------------------------- CLI

function sitefile(args: string[], cwd: string, home: string): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd, env: { ...process.env, SITEFILE_HOME: home } }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

async function example(): Promise<{ dir: string; home: string }> {
  const work = await mkdtemp(path.join(os.tmpdir(), 'sitefile-bake-'));
  temps.push(work);
  const dir = path.join(work, 'bake');
  await cp(path.join(root, 'examples', 'bake'), dir, { recursive: true, filter: (s) => path.basename(s) !== 'dist' });
  return { dir, home: path.join(work, '.home') };
}

test('CLI: bake builds every variant of examples/bake side by side, in plan order', async () => {
  const { dir, home } = await example();
  const r = await sitefile(['bake', '-j', '3'], dir, home);
  assert.equal(r.code, 0, r.stderr);
  const lines = r.stdout.trimEnd().split('\n');
  assert.deepEqual(
    lines.map((l) => l.replace(/ \(sha256:.*\)$/, '')),
    [
      '[site-en-staging] Exported 2 files to dist/staging/en',
      '[site-en-production] Exported 2 files to dist/production/en',
      '[site-fr-staging] Exported 2 files to dist/staging/fr',
      '[site-fr-production] Exported 2 files to dist/production/fr',
      'Baked 4 targets',
    ],
  );
  const fr = await readFile(path.join(dir, 'dist', 'production', 'fr', 'index.html'), 'utf8');
  assert.match(fr, /<html lang="fr">/);
  assert.match(fr, /href="https:\/\/example.com\/fr\/"/);
  assert.match(await readFile(path.join(dir, 'dist', 'staging', 'en', 'index.html'), 'utf8'), /staging\.example\.com\/en\//);
});

test('CLI: a failing target fails the bake (exit 1) after the others finish; --build-arg applies to every target', async () => {
  const { dir, home } = await example();
  await writeFile(
    path.join(dir, 'sitefile.bake.yaml'),
    'targets:\n  good:\n    args: {LOCALE: fr}\n  bad:\n    args: {LOCALE: de}\n  also:\n    output: out/also\n',
  );
  const r = await sitefile(['bake', '--build-arg', 'ENV=ci'], dir, home);
  assert.equal(r.code, 1);
  assert.match(r.stdout, /^\[good\] Exported 2 files to dist\/good /m);
  assert.match(r.stdout, /^\[also\] Exported 2 files to out\/also /m);
  assert.doesNotMatch(r.stdout, /Baked/);
  assert.match(r.stderr, /^\[bad\] error\[E_NOT_FOUND\]: /m);
  assert.match(r.stderr, /^error\[E_BAKE\]: 1 of 3 targets failed: bad$/m);
  assert.match(await readFile(path.join(dir, 'out', 'also', 'index.html'), 'utf8'), /<p class="env">ci<\/p>/);
});

test('CLI: --print builds nothing; usage errors', async () => {
  const { dir, home } = await example();
  const r = await sitefile(['bake', '--print', '--json', 'en'], dir, home);
  assert.equal(r.code, 0, r.stderr);
  assert.deepEqual((JSON.parse(r.stdout) as { targets: Array<{ name: string }> }).targets.map((t) => t.name), ['site-en-staging', 'site-en-production']);
  await assert.rejects(stat(path.join(dir, 'dist')));
  for (const [args, message] of [
    [['--json'], 'bake: --json is only for --print'],
    [['--cache-to', 'localhost:5000/c'], "bake: --cache-to is not supported (each target would replace the others' results); push from one 'sitefile build --cache-to'"],
    [['-j', '0'], '--jobs expects a positive integer'],
    [['--frozen-lockfile', '--update-lock'], '--frozen-lockfile and --update-lock conflict'],
    [['nope'], "bake: no target or group named 'nope' (targets: common, site; groups: default, en)"],
    [['-f', 'bake.toml'], "bake file 'bake.toml': unknown format (use a .yaml, .yml or .json file)"],
  ] as const) {
    const u = await sitefile(['bake', ...args], dir, home);
    assert.equal(u.code, 2, `${args.join(' ')}: ${u.stderr}`);
    assert.equal(u.stderr.split('\n')[0], `error: ${message}`);
  }
  const none = await sitefile(['bake'], path.dirname(dir), home);
  assert.equal(none.code, 1);
  assert.match(none.stderr, /^error\[E_NOT_FOUND\]: no bake file: sitefile\.bake\.yaml, sitefile\.bake\.yml, sitefile\.bake\.json not found/);
});

test('CLI: --profile <dir> writes one trace per target', async () => {
  const { dir, home } = await example();
  const r = await sitefile(['bake', 'en', '--profile', 'traces'], dir, home);
  assert.equal(r.code, 0, r.stderr);
  for (const name of ['site-en-staging', 'site-en-production']) {
    const trace = JSON.parse(await readFile(path.join(dir, 'traces', `${name}.json`), 'utf8')) as { traceEvents: unknown[] };
    assert.ok(trace.traceEvents.length > 0);
  }
});
