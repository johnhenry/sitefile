import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdtemp, mkdir, readdir, rm, symlink, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { substituteArgs } from '../src/plugins/exec.js';
import { runCli, tmpDir } from './helpers/cli.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function dir(prefix: string): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), prefix));
  temps.push(d);
  return d;
}

/**
 * A fake external tool. The first argument selects a mode; the last argument is the
 * output file. "dump" records what the host gave it.
 */
const FAKE = (execPath: string): string => `#!${execPath}
const fs = require('node:fs');
const [mode, ...rest] = process.argv.slice(2);
const out = rest[rest.length - 1];
const listing = fs.readdirSync('.').sort();
switch (mode) {
  case 'dump':
    fs.writeFileSync(out, JSON.stringify({ argv: rest.slice(0, -1), env: process.env, cwd: process.cwd(), files: listing, stdinIsNull: fs.fstatSync(0).isCharacterDevice() }));
    break;
  case 'sleep':
    setTimeout(() => fs.writeFileSync(out, 'late'), 5000);
    break;
  case 'fail':
    console.error('first line');
    console.error('boom: bad input');
    process.exit(3);
  case 'nowrite':
    break;
  case 'big':
    fs.writeFileSync(out, Buffer.alloc(100000, 1));
    break;
  case 'noisy':
    console.log('to stdout');
    console.error('to stderr');
    fs.writeFileSync(out, 'ok');
    break;
  case 'symlink':
    fs.symlinkSync('/etc/hosts', out);
    break;
  case 'copy':
    fs.copyFileSync(rest[0], out);
    break;
}
`;

async function project(files: Record<string, string>): Promise<string> {
  const d = await dir('sitefile-exec-');
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(d, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return d;
}

/** Install the fake tool outside the build context and return its absolute path. */
async function fakeTool(): Promise<string> {
  const d = await dir('sitefile-faketool-');
  const file = path.join(d, 'fake');
  await writeFile(file, FAKE(process.execPath));
  await chmod(file, 0o755);
  return file;
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

async function run(files: Record<string, string>, extra: Partial<BuildOptions> = {}) {
  return build({ contextDir: await project(files), lockfile: false, ...extra });
}
async function rejects(files: Record<string, string>, code: ErrorCode, match: RegExp, extra: Partial<BuildOptions> = {}): Promise<void> {
  await assert.rejects(run(files, extra), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.match(e.message, match);
    return true;
  });
}

const sitefile = (transform: string): string => `FROM scratch\nTOOL fake\n${transform}\nEXPORT /`;

test('TOOL / ARGS parse and validate', () => {
  const prog = parseProgram(
    sitefile('IMPORT xs FROM ./x.json AS json\nTRANSFORM ./a.txt WITH fake TO /a.out OPTIONS w=3 ARGS -f "lavfi x" --w={option.w} {input} {output}'),
  );
  const t = prog.instructions.find((i) => i.kind === 'TRANSFORM');
  assert.ok(t?.kind === 'TRANSFORM' && t.args?.length === 5);

  const fails = (src: string, code: ErrorCode, line: number): void =>
    assert.throws(() => parseProgram(src), (e: unknown) => isSitefileError(e, code) && e.span?.line === line, src);
  fails('FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a\nEXPORT /', 'E_VALIDATION', 3); // no ARGS
  fails('FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a ARGS -i {input}\nEXPORT /', 'E_VALIDATION', 3); // no {output}
  fails('FROM scratch\nTOOL t\nIMPORT xs FROM ./x.json AS json\nTRANSFORM WITH t TO /a ARGS {input} {output} FOR x IN xs\nEXPORT /', 'E_VALIDATION', 4); // {input} without source
  fails('FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a ARGS {option.nope} {output}\nEXPORT /', 'E_UNDEFINED', 3);
  fails('FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /a ARGS x\nEXPORT /', 'E_VALIDATION', 3); // ARGS on a PLUGIN
  fails('FROM scratch\nTOOL t\nTOOL t\nEXPORT /', 'E_DUPLICATE', 3);
  fails('FROM scratch\nARG t = 1\nTOOL t\nEXPORT /', 'E_DUPLICATE', 3);
  // ffmpeg's %{pts} and any unknown {braces} are literal, not placeholders.
  parseProgram('FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a ARGS -vf drawtext=text=%{pts}{x} {output}\nEXPORT /');
});

test('a TOOL that is not allowed fails with the exact flag to add', async () => {
  await rejects({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS dump {output}') }, 'E_UNSUPPORTED', /add --allow-exec fake\b/);
});

test('--allow-exec that matches nothing on PATH, a relative path, and an unused name', async () => {
  const files = { 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS dump {output}') };
  await rejects(files, 'E_NOT_FOUND', /no executable 'fake' found on PATH/, { allowExec: ['fake'] });
  await rejects(files, 'E_USAGE', /must be absolute/, { allowExec: ['fake=bin/fake'] });
  await rejects(files, 'E_NOT_FOUND', /not an executable file/, { allowExec: ['fake=/nonexistent/fake'] });
  await rejects(files, 'E_USAGE', /NAME or NAME=/, { allowExec: ['bad-name'] });
  const tool = await fakeTool();
  const r = await run(files, { allowExec: [`fake=${tool}`, 'unused'] });
  assert.ok(r.warnings.some((w) => /--allow-exec unused does not match any TOOL/.test(w)));
});

test('argv placeholders, no shell, scrubbed environment, temp-dir cwd, closed stdin', async () => {
  const tool = await fakeTool();
  const notices: string[] = [];
  process.env['SITEFILE_SECRET_PROBE'] = 'hunter2';
  const hostile = '; echo pwned > /tmp/pwned-by-sitefile $(id) `id` | cat && *';
  const r = await run(
    {
      'in.txt': 'hello',
      Sitefile: [
        'FROM scratch',
        'ARG HOSTILE',
        'TOOL fake',
        'TRANSFORM ./in.txt WITH fake TO /out.json OPTIONS size=640x480 note="a b" ARGS dump -i {input} --size={option.size} "--note={option.note}" "${HOSTILE}" "$(whoami)" ";" "%{pts}" "{unknown}" {output}',
        'EXPORT /',
      ].join('\n'),
    },
    { allowExec: [`fake=${tool}`], buildArgs: { HOSTILE: hostile }, onNotice: (m) => notices.push(m) },
  );
  assert.deepEqual(notices, [`tool fake -> ${await realpathOf(tool)}`]);
  assert.equal(r.tools[0]?.path, await realpathOf(tool));
  const got = JSON.parse(text(r.output.get('/out.json')?.bytes)) as { argv: string[]; env: Record<string, string>; cwd: string; files: string[]; stdinIsNull: boolean };
  assert.deepEqual(got.argv, ['-i', 'input.txt', '--size=640x480', '--note=a b', hostile, '$(whoami)', ';', '%{pts}', '{unknown}']);
  // Only PATH (macOS adds __CF_USER_TEXT_ENCODING itself); the build's own environment does not leak.
  assert.deepEqual(Object.keys(got.env).filter((k) => k !== '__CF_USER_TEXT_ENCODING'), ['PATH']);
  assert.ok(!('SITEFILE_SECRET_PROBE' in got.env));
  assert.match(path.basename(got.cwd), /^sitefile-tool-/);
  assert.deepEqual(got.files, ['input.txt']);
  assert.equal(got.stdinIsNull, true);
  // The temp dir is removed afterwards.
  assert.ok(!(await readdir(os.tmpdir())).includes(path.basename(got.cwd)));
  // Nothing interpreted the hostile text.
  await assert.rejects(readdir('/tmp/pwned-by-sitefile'));
  delete process.env['SITEFILE_SECRET_PROBE'];
});

async function realpathOf(p: string): Promise<string> {
  return (await import('node:fs/promises')).realpath(p);
}

test('substituteArgs replaces only known placeholders, once, without re-scanning values', () => {
  assert.deepEqual(substituteArgs(['{input}', 'x={output}', '{option.a}', '{option.missing}|', '{other}'], 'input.txt', 'output.mp4', { a: '{output}' }), [
    'input.txt',
    'x=output.mp4',
    '{output}',
    '|',
    '{other}',
  ]);
});

test('a system tool by PATH name: /bin/cp copies the input', async () => {
  const cp = '/bin/cp';
  const old = process.env['PATH'];
  process.env['PATH'] = `${path.dirname(cp)}${path.delimiter}${old ?? ''}`;
  try {
    const r = await run({ 'a.txt': 'copy me', Sitefile: 'FROM scratch\nTOOL cp\nTRANSFORM ./a.txt WITH cp TO /b.txt ARGS {input} {output}\nEXPORT /' }, { allowExec: ['cp'] });
    assert.equal(text(r.output.get('/b.txt')?.bytes), 'copy me');
  } finally {
    process.env['PATH'] = old;
  }
});

test('FOR with a TOOL: loop values reach ARGS and dest', async () => {
  const tool = await fakeTool();
  const r = await run(
    {
      'xs.json': '[{"id":"a","w":"1"},{"id":"b","w":"2"}]',
      Sitefile: 'FROM scratch\nTOOL fake\nIMPORT xs FROM ./xs.json AS json\nTRANSFORM WITH fake TO /o/${x.id}.json FOR x IN xs ARGS dump --w=${x.w} {output}\nEXPORT /',
    },
    { allowExec: [`fake=${tool}`] },
  );
  assert.deepEqual(JSON.parse(text(r.output.get('/o/a.json')?.bytes)).argv, ['--w=1']);
  assert.deepEqual(JSON.parse(text(r.output.get('/o/b.json')?.bytes)).argv, ['--w=2']);
  assert.equal(r.transformStats.toolRuns, 2);
});

test('non-zero exit is E_PLUGIN with the last stderr lines', async () => {
  const tool = await fakeTool();
  await rejects({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS fail {output}') }, 'E_PLUGIN', /TOOL fake: exited with code 3\nfirst line\nboom: bad input/, { allowExec: [`fake=${tool}`] });
});

test('a tool that runs too long is killed (--plugin-timeout)', async () => {
  const tool = await fakeTool();
  const t0 = Date.now();
  await rejects({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS sleep {output}') }, 'E_PLUGIN', /timed out after 300 ms/, {
    allowExec: [`fake=${tool}`],
    toolPolicy: { timeoutMs: 300 },
  });
  assert.ok(Date.now() - t0 < 4000);
});

test('missing output, oversized output, and a symlinked output are rejected', async () => {
  const tool = await fakeTool();
  const t = (mode: string): Record<string, string> => ({ 'a.txt': 'x', Sitefile: sitefile(`TRANSFORM ./a.txt WITH fake TO /o.bin ARGS ${mode} {output}`) });
  await rejects(t('nowrite'), 'E_PLUGIN', /did not write \{output\}/, { allowExec: [`fake=${tool}`] });
  await rejects(t('big'), 'E_PLUGIN', /output is larger than 1000 bytes/, { allowExec: [`fake=${tool}`], toolPolicy: { maxOutputBytes: 1000 } });
  await rejects(t('symlink'), 'E_PLUGIN', /not a regular file/, { allowExec: [`fake=${tool}`] });
});

test('stdout and stderr become warnings, prefixed with the tool name', async () => {
  const tool = await fakeTool();
  const r = await run({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS noisy {output}') }, { allowExec: [`fake=${tool}`] });
  assert.ok(r.warnings.includes('[tool fake] to stdout'));
  assert.ok(r.warnings.includes('[tool fake] to stderr'));
});

test('a directory destination and a value-import source are rejected for a TOOL', async () => {
  const tool = await fakeTool();
  await rejects({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /d/ ARGS noisy {output}') }, 'E_PLUGIN', /directory destination/, { allowExec: [`fake=${tool}`] });
  await rejects(
    { 'c.json': '{}', Sitefile: 'FROM scratch\nTOOL fake\nIMPORT c FROM ./c.json AS json\nTRANSFORM c: WITH fake TO /o ARGS noisy {output}\nEXPORT /' },
    'E_TYPE',
    /file source, not a value import/,
    { allowExec: [`fake=${tool}`] },
  );
});

test('--allow-exec follows a symlink to the real binary, and prints it', async () => {
  const tool = await fakeTool();
  const d = await dir('sitefile-link-');
  const link = path.join(d, 'fake');
  await symlink(tool, link);
  const r = await run({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS noisy {output}') }, { allowExec: [`fake=${link}`] });
  assert.equal(r.tools[0]?.path, await realpathOf(tool));
});

test('CLI: --allow-exec is required, repeated flags work, the resolved path is printed', async () => {
  const tool = await fakeTool();
  const home = await tmpDir();
  const ctx = await project({ 'a.txt': 'x', Sitefile: sitefile('TRANSFORM ./a.txt WITH fake TO /o ARGS noisy {output}') });
  const denied = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist')]);
  assert.equal(denied.code, 1);
  assert.match(denied.stderr, /error\[E_UNSUPPORTED\].*add --allow-exec fake/);
  const ok = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--allow-exec', `fake=${tool}`]);
  assert.equal(ok.code, 0, ok.stderr);
  assert.match(ok.stderr, /tool fake -> /);
});

const ffmpeg = (() => {
  try {
    return execFileSync('/bin/sh', ['-c', 'command -v ffmpeg'], { encoding: 'utf8' }).trim();
  } catch {
    return '';
  }
})();

test('ffmpeg: renders a 1-second clip, deterministically with bitexact flags', { skip: ffmpeg ? false : 'ffmpeg is not installed' }, async () => {
  const sf = [
    'FROM scratch',
    'TOOL ffmpeg',
    'TRANSFORM --overwrite WITH ffmpeg TO /clip.mp4 OPTIONS size=160x120 ARGS -hide_banner -loglevel error -f lavfi -i "color=c=black:s={option.size}:d=1:r=10" -c:v mpeg4 -fflags +bitexact -flags:v +bitexact -map_metadata -1 {output}',
    'EXPORT /',
  ].join('\n');
  const a = await run({ Sitefile: sf }, { allowExec: ['ffmpeg'] });
  const clip = a.output.get('/clip.mp4')?.bytes;
  assert.ok(clip && clip.byteLength > 500);
  assert.equal(text(clip.slice(4, 8)), 'ftyp');
  const b = await run({ Sitefile: sf }, { allowExec: ['ffmpeg'] });
  assert.deepEqual(b.output.get('/clip.mp4')?.bytes, clip);
});
