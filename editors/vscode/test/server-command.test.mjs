// Unit tests of the server-command resolution (dist/serverCommand.mjs, built
// from src/serverCommand.ts by `npm run build`; no VS Code runtime needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  DEFAULT_SERVER_ARGS,
  DEFAULT_SERVER_PATH,
  DOCUMENT_SELECTOR,
  RestartBudget,
  resolveServerCommand,
} from '../dist/serverCommand.mjs';

const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));

/** An env whose filesystem is the given set of executable paths. */
const fs = (files, extra = {}) => ({ isExecutable: (f) => files.includes(f), platform: 'linux', ...extra });

test('defaults: sitefile on PATH with args ["lsp"]', () => {
  const r = resolveServerCommand({}, fs(['/opt/bin/sitefile'], { PATH: '/usr/bin:/opt/bin' }));
  assert.deepEqual(r, { ok: true, command: '/opt/bin/sitefile', args: ['lsp'] });
  assert.deepEqual(resolveServerCommand({ path: '', args: null }, fs(['/usr/bin/sitefile'], { PATH: '/usr/bin' })), {
    ok: true,
    command: '/usr/bin/sitefile',
    args: ['lsp'],
  });
});

test('the first PATH entry wins; empty entries are skipped', () => {
  const r = resolveServerCommand({ path: 'sitefile' }, fs(['/a/sitefile', '/b/sitefile'], { PATH: '::/a:/b' }));
  assert.equal(r.ok && r.command, '/a/sitefile');
});

test('missing binary on PATH is reported, not thrown', () => {
  const r = resolveServerCommand({}, fs([], { PATH: '/usr/bin' }));
  assert.equal(r.ok, false);
  assert.equal(r.reason, 'not-found');
  assert.equal(r.requested, 'sitefile');
  assert.match(r.message, /not found on PATH/);
  assert.equal(resolveServerCommand({}, fs([], {})).reason, 'not-found');
});

test('absolute, ~ and workspace-relative paths', () => {
  const env = fs(['/abs/sitefile', '/home/me/.cargo/bin/sitefile', '/work/rust/target/release/sitefile'], {
    home: '/home/me',
    workspaceFolder: '/work',
  });
  assert.equal(resolveServerCommand({ path: '/abs/sitefile' }, env).command, '/abs/sitefile');
  assert.equal(resolveServerCommand({ path: '~/.cargo/bin/sitefile' }, env).command, '/home/me/.cargo/bin/sitefile');
  assert.equal(
    resolveServerCommand({ path: 'rust/target/release/sitefile' }, env).command,
    '/work/rust/target/release/sitefile',
  );
  assert.equal(
    resolveServerCommand({ path: './rust/target/release/sitefile' }, env).command,
    '/work/rust/target/release/sitefile',
  );
  // A path is never looked up on PATH.
  const r = resolveServerCommand({ path: '/nope/sitefile' }, { ...env, PATH: '/abs' });
  assert.equal(r.ok, false);
  assert.match(r.message, /'\/nope\/sitefile' is not an executable file/);
  // Relative path without a workspace folder.
  const r2 = resolveServerCommand({ path: 'bin/sitefile' }, fs(['/x/bin/sitefile']));
  assert.equal(r2.ok, false);
  assert.match(r2.message, /no workspace folder/);
});

test('custom args are passed through unchanged; bad settings are invalid', () => {
  const env = fs(['/usr/bin/sitefile'], { PATH: '/usr/bin' });
  assert.deepEqual(resolveServerCommand({ args: ['lsp', '--stdio'] }, env).args, ['lsp', '--stdio']);
  assert.deepEqual(resolveServerCommand({ args: [] }, env).args, []);
  assert.equal(resolveServerCommand({ args: 'lsp' }, env).reason, 'invalid');
  assert.equal(resolveServerCommand({ args: ['lsp', 1] }, env).reason, 'invalid');
  assert.equal(resolveServerCommand({ path: 42 }, env).reason, 'invalid');
});

test('Windows: PATHEXT, ; delimiter and backslash paths', () => {
  const env = {
    platform: 'win32',
    PATH: 'C:\\tools;C:\\cargo\\bin',
    PATHEXT: '.COM;.EXE',
    isExecutable: (f) => f === 'C:\\cargo\\bin\\sitefile.EXE' || f === 'D:\\s\\sitefile.exe',
  };
  assert.equal(resolveServerCommand({}, env).command, 'C:\\cargo\\bin\\sitefile.EXE');
  assert.equal(resolveServerCommand({ path: 'D:\\s\\sitefile.exe' }, env).command, 'D:\\s\\sitefile.exe');
});

test('real filesystem check: a directory or missing file is not executable', () => {
  assert.equal(resolveServerCommand({ path: '/' }).ok, false);
  assert.equal(resolveServerCommand({ path: '/definitely/not/here/sitefile' }).ok, false);
  const sh = resolveServerCommand({ path: 'sh', args: [] }, { PATH: '/bin:/usr/bin' });
  assert.equal(sh.ok, true);
});

test('restart budget: at most 4 crashes in 3 minutes', () => {
  const b = new RestartBudget();
  assert.deepEqual([0, 1, 2, 3].map((t) => b.record(t)), [true, true, true, true]);
  assert.equal(b.record(4), false);
  assert.equal(b.record(3 * 60 * 1000 + 3), true, 'old crashes fall out of the window');
  b.reset();
  assert.equal(new RestartBudget(1, 10).record(0), true);
});

test('package.json contributions match the extension constants', () => {
  assert.equal(pkg.main, './dist/extension.js');
  assert.deepEqual(pkg.activationEvents, ['onLanguage:sitefile']);
  const props = pkg.contributes.configuration.properties;
  assert.equal(props['sitefile.server.path'].default, DEFAULT_SERVER_PATH);
  assert.deepEqual(props['sitefile.server.args'].default, DEFAULT_SERVER_ARGS);
  assert.equal(props['sitefile.server.enabled'].default, true);
  assert.deepEqual(props['sitefile.trace.server'].enum, ['off', 'messages', 'verbose']);
  assert.ok(pkg.contributes.commands.some((c) => c.command === 'sitefile.restartServer'));
  assert.deepEqual(
    DOCUMENT_SELECTOR.map((s) => [s.scheme, s.language]),
    [
      ['file', 'sitefile'],
      ['untitled', 'sitefile'],
    ],
  );
  assert.ok(pkg.contributes.languages.some((l) => l.id === 'sitefile'));
  assert.ok(pkg.dependencies['vscode-languageclient']);
});
