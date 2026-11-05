import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildArgFileKind, loadBuildArgFile, parseEnvFile } from '../src/build-args.js';
import { isSitefileError } from '../src/errors.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});
async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-args-'));
  temps.push(d);
  return d;
}

test('.env parsing: comments, export, quotes, escapes, no expansion', () => {
  const env = parseEnvFile(
    [
      '# comment',
      '',
      'export TITLE=My site  # trailing comment',
      "SINGLE='literal $HOME \\n'",
      'DOUBLE="line1\\nline2 \\"q\\""',
      'MULTI="a',
      'b"',
      'EMPTY=',
      'URL=https://example.com/#anchor',
      'CMD=$(rm -rf /)',
      '  SPACED = value ',
    ].join('\n'),
    '.env',
  );
  assert.deepEqual({ ...env }, {
    TITLE: 'My site',
    SINGLE: 'literal $HOME \\n',
    DOUBLE: 'line1\nline2 "q"',
    MULTI: 'a\nb',
    EMPTY: '',
    URL: 'https://example.com/#anchor',
    CMD: '$(rm -rf /)',
    SPACED: 'value',
  });
});

test('.env errors carry the line number', () => {
  const at = (src: string, line: number): void =>
    assert.throws(() => parseEnvFile(src, '.env'), (e: unknown) => isSitefileError(e, 'E_DATA') && e.span?.line === line, src);
  at('A=1\nnot a pair\n', 2);
  at('A=1\nB="open\n\n', 2);
  at('A="x" junk\n', 1);
  at('1BAD=x\n', 1);
});

test('file kinds by extension', () => {
  assert.equal(buildArgFileKind('vars.json'), 'json');
  assert.equal(buildArgFileKind('cfg/args.YAML'), 'yaml');
  assert.equal(buildArgFileKind('x.yml'), 'yaml');
  assert.equal(buildArgFileKind('.env'), 'env');
  assert.equal(buildArgFileKind('.env.production'), 'env');
  assert.equal(buildArgFileKind('prod.env'), 'env');
  assert.throws(() => buildArgFileKind('vars.toml'), (e: unknown) => isSitefileError(e, 'E_USAGE'));
});

test('JSON and YAML files: flat scalars only', async () => {
  const d = await tmp();
  await writeFile(path.join(d, 'a.json'), '{"TITLE":"Docs","PORT":8080,"DEBUG":false}');
  assert.deepEqual({ ...(await loadBuildArgFile(path.join(d, 'a.json'))) }, { TITLE: 'Docs', PORT: '8080', DEBUG: 'false' });
  await writeFile(path.join(d, 'a.yaml'), 'TITLE: "Docs: v2"\nPORT: 8080\n');
  assert.deepEqual({ ...(await loadBuildArgFile(path.join(d, 'a.yaml'))) }, { TITLE: 'Docs: v2', PORT: '8080' });
  for (const [name, body] of [
    ['nested.json', '{"A":{"b":1}}'],
    ['list.json', '[1,2]'],
    ['null.yaml', 'A: null\n'],
    ['badkey.json', '{"not valid":1}'],
    ['broken.json', '{'],
  ] as const) {
    await writeFile(path.join(d, name), body);
    await assert.rejects(loadBuildArgFile(path.join(d, name)), (e: unknown) => isSitefileError(e, 'E_DATA'), name);
  }
  await assert.rejects(loadBuildArgFile(path.join(d, 'missing.json')), (e: unknown) => isSitefileError(e, 'E_NOT_FOUND'));
});

function sitefile(args: string[], env: NodeJS.ProcessEnv = {}): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { env: { ...process.env, ...env } }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

test('CLI: --build-arg-file precedence and --build-arg NAME from the environment', async () => {
  const d = await tmp();
  await writeFile(
    path.join(d, 'Sitefile'),
    'FROM scratch\nARG A = "default"\nARG B = "default"\nARG C = "default"\nARG N: number = 1\nRENDER <<T TO /out.txt\n${A} ${B} ${C} ${N}\nT\nEXPORT /\n',
  );
  await writeFile(path.join(d, 'one.json'), '{"A":"json","B":"json","N":7}');
  await writeFile(path.join(d, 'two.env'), 'B=env\n');
  const r = await sitefile(['build', d, '--build-arg-file', path.join(d, 'one.json'), '--build-arg-file', path.join(d, 'two.env'), '--build-arg', 'C']
    , { C: 'from-environment' });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(await readFile(path.join(d, 'dist', 'out.txt'), 'utf8'), 'json env from-environment 7\n');

  const override = await sitefile(['build', d, '--build-arg-file', path.join(d, 'one.json'), '--build-arg', 'A=cli']);
  assert.equal(override.code, 0, override.stderr);
  assert.match(await readFile(path.join(d, 'dist', 'out.txt'), 'utf8'), /^cli json default 7/);

  const unset = await sitefile(['build', d, '--build-arg', 'SITEFILE_SURELY_UNSET_VAR'], { SITEFILE_SURELY_UNSET_VAR: undefined });
  assert.equal(unset.code, 2);
  assert.match(unset.stderr, /SITEFILE_SURELY_UNSET_VAR is not set/);

  await writeFile(path.join(d, 'bad.json'), '{"N":"abc"}');
  const typed = await sitefile(['build', d, '--build-arg-file', path.join(d, 'bad.json')]);
  assert.equal(typed.code, 1);
  assert.match(typed.stderr, /E_TYPE/);

  const unknown = await sitefile(['build', d, '--build-arg-file', path.join(d, 'vars.toml')]);
  assert.equal(unknown.code, 2);
});
