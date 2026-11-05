import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, type Server, type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { fetchRemote, validateHop, createGuardedLookup } from '../src/resources/https.js';
import { DEFAULT_FETCH_POLICY, type FetchPolicy } from '../src/resources/types.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';

const LOCAL: FetchPolicy = { ...DEFAULT_FETCH_POLICY, allowHttp: true, allowPrivateNetwork: true };

type Handler = (req: IncomingMessage, res: ServerResponse, base: string) => void;
const servers: Server[] = [];

async function serve(handler: Handler): Promise<string> {
  let base = '';
  const server = createServer((req, res) => handler(req, res, base));
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  return base;
}

after(() => {
  for (const s of servers) {
    s.closeAllConnections();
    s.close();
  }
});

async function rejectsWith(p: Promise<unknown>, code: ErrorCode): Promise<void> {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), `expected SitefileError, got ${String(e)}`);
    assert.equal(e.code, code, e.message);
    return true;
  });
}

test('fetches a body with headers and media type', async () => {
  let seen: IncomingMessage | undefined;
  const base = await serve((req, res) => {
    seen = req;
    res.setHeader('content-type', 'Text/Plain; charset=utf-8');
    res.end('hello');
  });
  const r = await fetchRemote(base + '/a.txt', LOCAL);
  assert.equal(Buffer.from(r.bytes).toString(), 'hello');
  assert.equal(r.finalUrl, base + '/a.txt');
  assert.equal(r.mediaType, 'text/plain');
  assert.equal(seen?.headers['accept-encoding'], 'identity');
  assert.equal(seen?.headers['user-agent'], 'sitefile/0.1');
  assert.equal(seen?.headers.cookie, undefined);
  assert.equal(seen?.headers.authorization, undefined);
});

test('http is rejected by default', async () => {
  const base = await serve((_q, res) => res.end('x'));
  await rejectsWith(fetchRemote(base + '/', DEFAULT_FETCH_POLICY), 'E_PROTOCOL');
});

test('unsupported schemes are rejected', async () => {
  await rejectsWith(fetchRemote('ftp://example.com/x', LOCAL), 'E_PROTOCOL');
  await rejectsWith(fetchRemote('file:///etc/passwd', LOCAL), 'E_PROTOCOL');
  await rejectsWith(fetchRemote('not a url', LOCAL), 'E_PROTOCOL');
});

test('loopback is blocked by default even with allowHttp', async () => {
  const base = await serve((_q, res) => res.end('x'));
  const port = new URL(base).port;
  const policy = { ...DEFAULT_FETCH_POLICY, allowHttp: true };
  await rejectsWith(fetchRemote(`http://127.0.0.1:${port}/`, policy), 'E_SSRF_BLOCKED');
  await rejectsWith(fetchRemote(`http://localhost:${port}/`, policy), 'E_SSRF_BLOCKED');
  await rejectsWith(fetchRemote(`http://[::1]:${port}/`, policy), 'E_SSRF_BLOCKED');
  await rejectsWith(fetchRemote(`https://localhost:${port}/`, DEFAULT_FETCH_POLICY), 'E_SSRF_BLOCKED');
});

test('localhost by name works when private network is allowed', async () => {
  const base = await serve((_q, res) => res.end('named'));
  const port = new URL(base).port;
  // localhost may resolve to ::1 first; the server listens on 127.0.0.1, so
  // only assert we did not get an SSRF block.
  try {
    const r = await fetchRemote(`http://localhost:${port}/`, LOCAL);
    assert.equal(Buffer.from(r.bytes).toString(), 'named');
  } catch (e) {
    assert.ok(isSitefileError(e) && e.code !== 'E_SSRF_BLOCKED', String(e));
  }
});

test('credentials in URLs are rejected without leaking them', async () => {
  const base = await serve((_q, res) => res.end('x'));
  const u = new URL(base);
  u.username = 'user';
  u.password = 'hunter2';
  await assert.rejects(fetchRemote(u.href, LOCAL), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_PROTOCOL'));
    assert.ok(!e.message.includes('hunter2'));
    return true;
  });
});

test('redirects are followed and finalUrl reported', async () => {
  const base = await serve((req, res, b) => {
    if (req.url === '/start') {
      res.writeHead(302, { location: '/middle' });
      res.end();
    } else if (req.url === '/middle') {
      res.writeHead(308, { location: `${b}/end` });
      res.end();
    } else res.end('done');
  });
  const r = await fetchRemote(base + '/start', LOCAL);
  assert.equal(Buffer.from(r.bytes).toString(), 'done');
  assert.equal(r.finalUrl, base + '/end');
});

test('redirect loops and too many redirects fail', async () => {
  const base = await serve((req, res) => {
    if (req.url === '/loop') {
      res.writeHead(301, { location: '/loop' });
      res.end();
      return;
    }
    const n = Number(req.url?.slice(1));
    if (n > 0) {
      res.writeHead(307, { location: `/${n - 1}` });
      res.end();
    } else res.end('ok');
  });
  await rejectsWith(fetchRemote(base + '/loop', LOCAL), 'E_REDIRECT');
  await rejectsWith(fetchRemote(base + '/3', { ...LOCAL, maxRedirects: 2 }), 'E_REDIRECT');
  const r = await fetchRemote(base + '/2', { ...LOCAL, maxRedirects: 2 });
  assert.equal(Buffer.from(r.bytes).toString(), 'ok');
});

test('redirect without Location fails', async () => {
  const base = await serve((_q, res) => {
    res.writeHead(302);
    res.end();
  });
  await rejectsWith(fetchRemote(base + '/', LOCAL), 'E_REDIRECT');
});

test('every redirect hop is re-validated', async () => {
  const base = await serve((req, res) => {
    const targets: Record<string, string> = {
      '/ftp': 'ftp://example.com/file',
      '/creds': 'http://u:p@127.0.0.1/',
      '/private': 'http://10.255.255.1/',
      '/downgrade': 'http://203.0.113.1/',
    };
    res.writeHead(302, { location: targets[req.url ?? ''] ?? '/' });
    res.end();
  });
  await rejectsWith(fetchRemote(base + '/ftp', LOCAL), 'E_PROTOCOL');
  await rejectsWith(fetchRemote(base + '/creds', LOCAL), 'E_PROTOCOL');
  // Policy is constant across hops, so private-target and downgrade checks
  // for redirect targets are exercised on validateHop below.
});

test('validateHop: private targets and https->http downgrades', () => {
  const policy = DEFAULT_FETCH_POLICY;
  assert.throws(() => validateHop(new URL('http://example.com/'), policy), (e) => isSitefileError(e, 'E_PROTOCOL'));
  assert.throws(() => validateHop(new URL('https://10.255.255.1/'), policy), (e) => isSitefileError(e, 'E_SSRF_BLOCKED'));
  assert.throws(() => validateHop(new URL('https://169.254.169.254/'), policy), (e) => isSitefileError(e, 'E_SSRF_BLOCKED'));
  assert.throws(() => validateHop(new URL('https://[::ffff:127.0.0.1]/'), policy), (e) => isSitefileError(e, 'E_SSRF_BLOCKED'));
  assert.throws(() => validateHop(new URL('https://[fd00::1]/'), policy), (e) => isSitefileError(e, 'E_SSRF_BLOCKED'));
  // Decimal / hex IPv4 forms are canonicalized by URL.
  assert.throws(() => validateHop(new URL('https://2130706433/'), policy), (e) => isSitefileError(e, 'E_SSRF_BLOCKED'));
  assert.throws(() => validateHop(new URL('https://0x7f.1/'), policy), (e) => isSitefileError(e, 'E_SSRF_BLOCKED'));
  validateHop(new URL('https://93.184.216.34/'), policy);
  validateHop(new URL('https://example.com/'), policy);
  validateHop(new URL('https://10.255.255.1/'), { ...policy, allowPrivateNetwork: true });
});

test('guarded lookup blocks names resolving to private addresses (both callback shapes)', async () => {
  const lookup = createGuardedLookup(DEFAULT_FETCH_POLICY);
  for (const all of [true, false]) {
    const err = await new Promise<unknown>((resolve) => {
      lookup('localhost', { all } as never, ((e: unknown) => resolve(e)) as never);
    });
    assert.ok(isSitefileError(err, 'E_SSRF_BLOCKED'), String(err));
  }
  const open = createGuardedLookup(LOCAL);
  const arr = await new Promise<unknown>((resolve, reject) => {
    open('localhost', { all: true } as never, ((e: unknown, a: unknown) => (e ? reject(e) : resolve(a))) as never);
  });
  assert.ok(Array.isArray(arr) && arr.length > 0);
  const single = await new Promise<[unknown, unknown]>((resolve, reject) => {
    open('localhost', {} as never, ((e: unknown, a: unknown, f: unknown) => (e ? reject(e) : resolve([a, f]))) as never);
  });
  assert.equal(typeof single[0], 'string');
  assert.ok(single[1] === 4 || single[1] === 6);
});

test('non-200 status fails', async () => {
  const base = await serve((_q, res) => {
    res.statusCode = 404;
    res.end('nope');
  });
  await rejectsWith(fetchRemote(base + '/missing', LOCAL), 'E_FETCH_STATUS');
});

test('Content-Length over the cap fails before reading', async () => {
  const base = await serve((_q, res) => {
    res.setHeader('content-length', '1000');
    res.write('x'.repeat(10));
    // never finish: the client must bail on the header alone
  });
  await rejectsWith(fetchRemote(base + '/', { ...LOCAL, maxBytes: 100, timeoutMs: 5000 }), 'E_TOO_LARGE');
});

test('streamed body over the cap fails', async () => {
  const base = await serve((_q, res) => {
    // chunked, no content-length
    res.write('x'.repeat(64));
    res.write('x'.repeat(64));
    res.end('x'.repeat(64));
  });
  await rejectsWith(fetchRemote(base + '/', { ...LOCAL, maxBytes: 100 }), 'E_TOO_LARGE');
  const r = await fetchRemote(base + '/', { ...LOCAL, maxBytes: 192 });
  assert.equal(r.bytes.length, 192);
});

test('overall timeout fires for a hanging server', async () => {
  const base = await serve(() => {
    /* never respond */
  });
  const t0 = Date.now();
  await rejectsWith(fetchRemote(base + '/', { ...LOCAL, timeoutMs: 150 }), 'E_FETCH_TIMEOUT');
  assert.ok(Date.now() - t0 < 2000);
});

test('timeout also covers a stalled body', async () => {
  const base = await serve((_q, res) => {
    res.write('partial');
  });
  await rejectsWith(fetchRemote(base + '/', { ...LOCAL, timeoutMs: 150 }), 'E_FETCH_TIMEOUT');
});

test('compressed content-encoding is rejected', async () => {
  const base = await serve((_q, res) => {
    res.setHeader('content-encoding', 'gzip');
    res.end('not really gzip');
  });
  await rejectsWith(fetchRemote(base + '/', LOCAL), 'E_FETCH');
});

test('connection errors become E_FETCH', async () => {
  const base = await serve((_q, res) => res.end());
  const s = servers.pop()!;
  await new Promise<void>((r) => s.close(() => r()));
  await rejectsWith(fetchRemote(base + '/', LOCAL), 'E_FETCH');
});
