import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer as createHttpServer, type IncomingMessage, type ServerResponse, type Server } from 'node:http';
import { createServer as createHttpsServer } from 'node:https';
import type { AddressInfo } from 'node:net';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fetchRemote } from '../src/resources/https.js';
import { DEFAULT_FETCH_POLICY, type FetchPolicy } from '../src/resources/types.js';
import { build } from '../src/executor.js';
import { sha256 } from '../src/digest.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { createTestPki, opensslAvailable, SKIP_REASON, type TestPki } from './helpers/tls.js';

// Real TLS end to end: allowHttp stays false, so https:// is exercised for real.
const skip = opensslAvailable() ? false : SKIP_REASON;

type Handler = (req: IncomingMessage, res: ServerResponse, base: string) => void;

let pki: TestPki;
let good: { cert: string; key: string };
let wrongName: { cert: string; key: string };
const servers: Server[] = [];
const temps: string[] = [];

before(() => {
  if (skip) return;
  pki = createTestPki();
  good = pki.issue('IP:127.0.0.1,DNS:localhost');
  wrongName = pki.issue('DNS:other.example');
});

after(async () => {
  for (const s of servers) {
    s.closeAllConnections();
    s.close();
  }
  if (!skip) pki.dispose();
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function listen(server: Server, scheme: 'http' | 'https', base: (b: string) => void): Promise<string> {
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const url = `${scheme}://127.0.0.1:${(server.address() as AddressInfo).port}`;
  base(url);
  return url;
}

async function serveTls(handler: Handler, keys = good): Promise<string> {
  let base = '';
  const server = createHttpsServer(keys, (req, res) => handler(req, res, base));
  return listen(server, 'https', (b) => (base = b));
}

async function servePlain(handler: Handler): Promise<string> {
  let base = '';
  const server = createHttpServer((req, res) => handler(req, res, base));
  return listen(server, 'http', (b) => (base = b));
}

const policy = (): FetchPolicy => ({ ...DEFAULT_FETCH_POLICY, allowPrivateNetwork: true, ca: pki.caCert });

async function rejectsWith(p: Promise<unknown>, code: ErrorCode): Promise<unknown> {
  let cause: unknown;
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), `expected SitefileError, got ${String(e)}`);
    assert.equal(e.code, code, e.message);
    cause = e.cause;
    return true;
  });
  return cause;
}

test('https fetch succeeds with a custom CA', { skip }, async () => {
  const base = await serveTls((_q, res) => {
    res.setHeader('content-type', 'text/plain');
    res.end('secure hello');
  });
  const r = await fetchRemote(base + '/a.txt', policy());
  assert.equal(Buffer.from(r.bytes).toString(), 'secure hello');
  assert.equal(r.finalUrl, base + '/a.txt');
  assert.equal(r.mediaType, 'text/plain');
});

test('custom CA also accepts a Buffer', { skip }, async () => {
  const base = await serveTls((_q, res) => res.end('buf'));
  const r = await fetchRemote(base + '/', { ...policy(), ca: Buffer.from(pki.caCert) });
  assert.equal(Buffer.from(r.bytes).toString(), 'buf');
});

test('untrusted certificate (no ca) fails with E_FETCH and a cert error cause', { skip }, async () => {
  const base = await serveTls((_q, res) => res.end('nope'));
  const cause = await rejectsWith(
    fetchRemote(base + '/', { ...DEFAULT_FETCH_POLICY, allowPrivateNetwork: true }),
    'E_FETCH',
  );
  assert.match(String((cause as { code?: string }).code), /CERT|SELF_SIGNED|UNABLE_TO_VERIFY/);
});

test('hostname mismatch fails with E_FETCH even when the CA is trusted', { skip }, async () => {
  const base = await serveTls((_q, res) => res.end('nope'), wrongName);
  const cause = await rejectsWith(fetchRemote(base + '/', policy()), 'E_FETCH');
  assert.equal((cause as { code?: string }).code, 'ERR_TLS_CERT_ALTNAME_INVALID');
});

test('https redirecting to http is refused end to end with E_PROTOCOL', { skip }, async () => {
  const plain = await servePlain((_q, res) => res.end('downgraded'));
  const base = await serveTls((_q, res) => {
    res.statusCode = 302;
    res.setHeader('location', plain + '/x');
    res.end();
  });
  await rejectsWith(fetchRemote(base + '/', policy()), 'E_PROTOCOL');
});

test('https to https redirect is followed', { skip }, async () => {
  const base = await serveTls((req, res) => {
    if (req.url === '/start') {
      res.statusCode = 301;
      res.setHeader('location', '/final');
      res.end();
    } else {
      res.end('landed');
    }
  });
  const r = await fetchRemote(base + '/start', policy());
  assert.equal(Buffer.from(r.bytes).toString(), 'landed');
  assert.equal(r.finalUrl, base + '/final');
});

test('build() over https: integrity pin and lockfile', { skip }, async () => {
  let body = 'remote v1';
  const base = await serveTls((_q, res) => res.end(body));
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-tls-'));
  temps.push(dir);
  const fetchPolicy = { allowPrivateNetwork: true, ca: pki.caCert };

  // Pinned: matching integrity passes, mismatch fails.
  await writeFile(
    path.join(dir, 'Sitefile'),
    `FROM scratch\nIMPORT lib FROM ${base}/lib.js INTEGRITY ${sha256('remote v1')}\nCOPY lib: /lib.js\nEXPORT /\n`,
  );
  const ok = await build({ contextDir: dir, lockfile: false, fetchPolicy });
  assert.equal(new TextDecoder().decode(ok.output.get('/lib.js')?.bytes), 'remote v1');

  await writeFile(
    path.join(dir, 'Sitefile'),
    `FROM scratch\nIMPORT lib FROM ${base}/lib.js INTEGRITY ${sha256('other')}\nEXPORT /\n`,
  );
  await rejectsWith(build({ contextDir: dir, lockfile: false, fetchPolicy }), 'E_INTEGRITY');

  // Unpinned: lockfile is written, then drift is detected.
  await writeFile(path.join(dir, 'Sitefile'), `FROM scratch\nIMPORT lib FROM ${base}/lib.js\nCOPY lib: /lib.js\nEXPORT /\n`);
  const first = await build({ contextDir: dir, fetchPolicy });
  assert.equal(first.lockfileWritten, true);
  const lock = JSON.parse(await readFile(path.join(dir, 'Sitefile.lock'), 'utf8'));
  assert.equal(lock.resources[`${base}/lib.js`].integrity, sha256('remote v1'));
  assert.equal((await build({ contextDir: dir, fetchPolicy })).lockfileWritten, false);

  body = 'remote v2';
  await rejectsWith(build({ contextDir: dir, fetchPolicy }), 'E_LOCK_MISMATCH');

  // Without the CA the same build cannot reach the server.
  await rejectsWith(build({ contextDir: dir, lockfile: false, fetchPolicy: { allowPrivateNetwork: true } }), 'E_FETCH');
});
