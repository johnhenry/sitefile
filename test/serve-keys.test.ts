// `serve` keys, `--open[=path]` and `--qr` (issue #224).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { promises as fs, readFileSync } from 'node:fs';
import path from 'node:path';
import { keyAction, openUrl, serveUrls, urlsText, KEYS_HELP } from '../src/cli/serve-keys.js';
import { networkUrls } from '../src/server.js';
import { optionalOpen } from '../src/cli/index.js';
import { encodeQr, qrTerminal } from '../src/qr.js';
import { cliPath, runCli, tmpDir } from './helpers/cli.js';

test('keys, URLs and --open', () => {
  assert.equal(keyAction('o'.charCodeAt(0)), 'open');
  assert.equal(keyAction('U'.charCodeAt(0)), 'urls');
  assert.equal(keyAction('r'.charCodeAt(0)), 'restart');
  assert.equal(keyAction('c'.charCodeAt(0)), 'clear');
  assert.equal(keyAction('q'.charCodeAt(0)), 'quit');
  assert.equal(keyAction(3), 'quit');
  assert.equal(keyAction(4), 'quit');
  assert.equal(keyAction('h'.charCodeAt(0)), 'help');
  assert.equal(keyAction('x'.charCodeAt(0)), undefined);
  assert.equal(openUrl('http://127.0.0.1:8/', '/a/b'), 'http://127.0.0.1:8/a/b');
  assert.equal(openUrl('http://127.0.0.1:8/', ''), 'http://127.0.0.1:8/');
  assert.equal(urlsText({ local: 'http://127.0.0.1:8/', network: [] }), '  Local:   http://127.0.0.1:8/\n');
  const withNet = urlsText({ local: 'https://127.0.0.1:8/', network: ['https://192.0.2.7:8/', 'https://[2001:db8::7]:8/'] });
  assert.equal(withNet, `  Local:   https://127.0.0.1:8/\n  Network: https://192.0.2.7:8/\n  Network: https://[2001:db8::7]:8/\n${qrTerminal(encodeQr('https://192.0.2.7:8/')!)}`);
  assert.deepEqual(optionalOpen(['./site', '--open', '-p', '8080']), ['./site', '--open=', '-p', '8080']);
  assert.deepEqual(optionalOpen(['--open=/docs/', '--', '--open']), ['--open=/docs/', '--', '--open']);
});

test('u lists the start line\'s network URLs, https:// for a TLS server (johnhenry/sitefile#300)', () => {
  assert.deepEqual(serveUrls('127.0.0.1', 8, '/docs/', 'https'), { local: 'https://127.0.0.1:8/docs/', network: [] });
  const all = serveUrls('0.0.0.0', 8, '/', 'https');
  assert.equal(all.local, 'https://127.0.0.1:8/');
  assert.deepEqual(all.network, networkUrls('0.0.0.0', 8, 'https'));
  assert.deepEqual(serveUrls('0.0.0.0', 8).network, networkUrls('0.0.0.0', 8));
});

async function waitFor(get: () => string, what: string): Promise<string> {
  for (const start = Date.now(); Date.now() - start < 60_000; await new Promise((r) => setTimeout(r, 50))) {
    const t = get();
    if (t.includes(what)) return t;
  }
  assert.fail(`waiting for ${JSON.stringify(what)} in ${JSON.stringify(get())}`);
}

test('serve: --qr, --open, then o u r h q from the keyboard', async () => {
  const d = await tmpDir('sitefile-keys-');
  await fs.mkdir(path.join(d, 'site'));
  await fs.writeFile(path.join(d, 'site', 'index.html'), 'hi');
  const browser = path.join(d, 'browser.sh');
  await fs.writeFile(browser, `#!/bin/sh\necho "$@" >> ${d}/opened\n`, { mode: 0o755 });
  const child = spawn(process.execPath, [cliPath, 'serve', './site', '--open=/docs/', '--qr', '-p', '127.0.0.1:0'], {
    cwd: d,
    env: { ...process.env, SITEFILE_HOME: path.join(d, 'home'), SITEFILE_SERVE_KEYS: '1', BROWSER: browser },
    stdio: ['pipe', 'pipe', 'ignore'],
  });
  let out = '';
  child.stdout.on('data', (c: Buffer) => (out += c.toString()));
  const exited = new Promise<number | null>((r) => child.on('exit', (code) => r(code)));
  const text = await waitFor(() => out, 'Press h for keys\n');
  const url = /at (http:\/\/\S+)/.exec(text)![1]!;
  assert.ok(text.includes(`(Ctrl-C to stop)\n${qrTerminal(encodeQr(url)!)}Press h for keys\n`), text);
  const opened = await waitFor(() => readSync(path.join(d, 'opened')), '\n');
  assert.equal(opened, `${url}docs/\n`);
  child.stdin.write('u');
  await waitFor(() => out, `  Local:   ${url}\n`);
  await fs.writeFile(path.join(d, 'site', 'new.html'), 'new');
  child.stdin.write('r');
  await waitFor(() => out, 'Re-read ');
  assert.match(out, /Re-read \S+site \(2 files\)\n/);
  child.stdin.write('h');
  await waitFor(() => out, KEYS_HELP);
  child.stdin.write('o');
  await waitFor(() => readSync(path.join(d, 'opened')), `${url}docs/\n${url}docs/\n`);
  child.stdin.write('q');
  assert.equal(await exited, 0);
});

test('serve --tls: o and u use https:// (johnhenry/sitefile#300)', async (t) => {
  const { createTestPki, opensslAvailable } = await import('./helpers/tls.js');
  if (!opensslAvailable()) return t.skip('openssl is not installed');
  const pki = createTestPki();
  const d = await tmpDir('sitefile-keys-');
  try {
    await fs.mkdir(path.join(d, 'site'));
    await fs.writeFile(path.join(d, 'site', 'index.html'), 'hi');
    const { cert, key } = pki.issue('IP:127.0.0.1');
    await fs.writeFile(path.join(d, 'cert.pem'), cert);
    await fs.writeFile(path.join(d, 'key.pem'), key);
    const browser = path.join(d, 'browser.sh');
    await fs.writeFile(browser, `#!/bin/sh\necho "$@" >> ${d}/opened\n`, { mode: 0o755 });
    const child = spawn(process.execPath, [cliPath, 'serve', './site', '--tls-cert', 'cert.pem', '--tls-key', 'key.pem', '-p', '127.0.0.1:0'], {
      cwd: d,
      env: { ...process.env, SITEFILE_HOME: path.join(d, 'home'), SITEFILE_SERVE_KEYS: '1', BROWSER: browser },
      stdio: ['pipe', 'pipe', 'ignore'],
    });
    let out = '';
    child.stdout.on('data', (c: Buffer) => (out += c.toString()));
    const exited = new Promise<number | null>((r) => child.on('exit', (code) => r(code)));
    const text = await waitFor(() => out, 'Press h for keys\n');
    const url = /at (https:\/\/\S+)/.exec(text)![1]!;
    child.stdin.write('u');
    await waitFor(() => out, `  Local:   ${url}\n`);
    child.stdin.write('o');
    assert.equal(await waitFor(() => readSync(path.join(d, 'opened')), '\n'), `${url}\n`);
    child.stdin.write('q');
    assert.equal(await exited, 0);
  } finally {
    pki.dispose();
  }
});

function readSync(p: string): string {
  try {
    return readFileSync(p, 'utf8');
  } catch {
    return '';
  }
}

test('--open and --qr are for the foreground; a bare --open before the source is not its value', async () => {
  const d = await tmpDir('sitefile-keys-');
  await fs.mkdir(path.join(d, 'site'));
  const r = await runCli(path.join(d, 'home'), ['serve', '-d', '--open', './site'], d);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--open and --qr are for a server in the foreground, not -d/);
});
