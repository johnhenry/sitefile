import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, request as httpRequest, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { createServer as createHttpsServer } from 'node:https';
import { connect as netConnect, createServer as createNetServer, type AddressInfo, type Socket } from 'node:net';
import { fetchRemote } from '../src/resources/https.js';
import { DEFAULT_FETCH_POLICY, type FetchPolicy } from '../src/resources/types.js';
import { hostMatches, mergeNetwork, networkFromEnv, networkProblem, parseFetchTimeout, parseProxy, proxyFor, type NetworkSettings } from '../src/resources/network.js';
import { RegistryClient } from '../src/registry.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { createTestPki, opensslAvailable, SKIP_REASON, type TestPki } from './helpers/tls.js';
import { startMockRegistry } from './helpers/mock-registry.js';
import { runCli } from './helpers/cli.js';

const LOCAL: FetchPolicy = { ...DEFAULT_FETCH_POLICY, allowHttp: true, allowPrivateNetwork: true };
const net = (network: NetworkSettings, extra: Partial<FetchPolicy> = {}): FetchPolicy => ({ ...LOCAL, network, ...extra });

const servers: Array<Server | ReturnType<typeof createNetServer>> = [];
const sockets: Socket[] = [];
after(() => {
  for (const s of sockets) s.destroy();
  for (const s of servers) {
    (s as Server).closeAllConnections?.();
    s.close();
  }
});

async function listen(server: Server | ReturnType<typeof createNetServer>): Promise<number> {
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  return (server.address() as AddressInfo).port;
}

async function serve(handler: (req: IncomingMessage, res: ServerResponse, base: string) => void): Promise<string> {
  let base = '';
  const port = await listen(createServer((req, res) => handler(req, res, base)));
  base = `http://127.0.0.1:${port}`;
  return base;
}

interface Seen {
  readonly line: string;
  readonly auth: string | undefined;
}

/**
 * A test proxy. Absolute-form requests are forwarded (or answered with `answer` when given); CONNECT is
 * tunnelled to the target, except `auth.test` (407) and `refused.test` (403). Every request is recorded.
 */
async function startProxy(answer?: string): Promise<{ url: string; seen: Seen[] }> {
  const seen: Seen[] = [];
  const server = createServer((req, res) => {
    seen.push({ line: `${req.method} ${req.url}`, auth: req.headers['proxy-authorization'] });
    if (answer !== undefined) {
      res.end(answer);
      return;
    }
    const target = new URL(req.url ?? '');
    const up = httpRequest(target, { method: req.method, headers: { ...req.headers, host: target.host } }, (r) => {
      res.writeHead(r.statusCode ?? 502, r.headers);
      r.pipe(res);
    });
    up.on('error', () => res.destroy());
    req.pipe(up);
  });
  server.on('connect', (req: IncomingMessage, client: Socket, head: Buffer) => {
    sockets.push(client);
    seen.push({ line: `CONNECT ${req.url}`, auth: req.headers['proxy-authorization'] });
    const [host, port] = [(req.url ?? '').replace(/:\d+$/, '').replace(/^\[|\]$/g, ''), Number(/:(\d+)$/.exec(req.url ?? '')?.[1] ?? 443)];
    if (host === 'auth.test') return void client.end('HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\n\r\n');
    if (host === 'refused.test') return void client.end('HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n');
    const upstream = netConnect(port, host === 'tunnel.test' ? '127.0.0.1' : host, () => {
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head.length) upstream.write(head);
      upstream.pipe(client);
      client.pipe(upstream);
    });
    sockets.push(upstream);
    upstream.on('error', () => client.destroy());
    client.on('error', () => upstream.destroy());
  });
  const port = await listen(server);
  return { url: `http://127.0.0.1:${port}`, seen };
}

async function rejectsWith(p: Promise<unknown>, code: ErrorCode, detail?: string | RegExp): Promise<SitefileErrorLike> {
  let caught: SitefileErrorLike | undefined;
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), `expected SitefileError, got ${String(e)}`);
    assert.equal(e.code, code, e.message);
    if (typeof detail === 'string') assert.equal(e.detail, detail);
    else if (detail) assert.match(e.detail, detail);
    caught = e;
    return true;
  });
  return caught as SitefileErrorLike;
}
interface SitefileErrorLike {
  readonly code: string;
  readonly detail: string;
  readonly message: string;
}

// ------------------------------------------------------------------ patterns and configuration

test('host patterns: domains, wildcards, ports, IPs and CIDR networks', () => {
  for (const p of ['example.com', '.example.com', '*.example.com', 'EXAMPLE.com.']) {
    assert.ok(hostMatches('example.com', 443, p), p);
    assert.ok(hostMatches('a.b.example.com', 443, p), p);
    assert.ok(!hostMatches('badexample.com', 443, p), p);
    assert.ok(!hostMatches('example.org', 443, p), p);
  }
  assert.ok(hostMatches('anything.at.all', 1, '*'));
  assert.ok(hostMatches('example.com', 8443, 'example.com:8443'));
  assert.ok(!hostMatches('example.com', 443, 'example.com:8443'));
  assert.ok(hostMatches('10.1.2.3', 443, '10.0.0.0/8'));
  assert.ok(!hostMatches('11.1.2.3', 443, '10.0.0.0/8'));
  assert.ok(hostMatches('::1', 443, '::1'));
  assert.ok(hostMatches('::1', 8080, '[::1]:8080'));
  assert.ok(!hostMatches('::1', 443, '[::1]:8080'));
  assert.ok(hostMatches('fd12::1', 443, 'fd00::/8'));
  assert.ok(hostMatches('1.2.3.4', 443, '0.0.0.0/0'));
  assert.ok(!hostMatches('10.0.0.1', 443, 'example.com'));
  assert.ok(!hostMatches('localhost', 443, '127.0.0.1'));
  assert.ok(!hostMatches('::ffff:10.0.0.1', 443, '10.0.0.0/8'));
  for (const bad of ['a b', 'exa!mple.com', '10.0.0.0/33', 'host:0', 'host:99999', '[::1', 'x:y']) {
    assert.match(networkProblem({ denyHosts: [bad] }) ?? '', /^invalid deny-hosts pattern/, bad);
  }
});

test('proxy URLs: http only, credentials decoded and never shown', () => {
  const p = parseProxy('http://user:p%40ss@proxy.local:3128');
  assert.equal(p.host, 'proxy.local');
  assert.equal(p.port, 3128);
  assert.equal(p.authorization, `Basic ${Buffer.from('user:p@ss').toString('base64')}`);
  assert.equal(p.shown, 'http://proxy.local:3128');
  assert.equal(parseProxy('http://proxy').port, 80);
  for (const bad of ['https://proxy:1', 'socks5://p:1', 'http://p:1/path', 'proxy:1', 'https://u:secret@p:1']) {
    assert.throws(() => parseProxy(bad), (e: unknown) => isSitefileError(e, 'E_USAGE') && !(e as Error).message.includes('secret'), bad);
  }
  const n: NetworkSettings = { proxy: 'http://proxy:3128', noProxy: ['internal.corp', '10.0.0.0/8'] };
  assert.equal(proxyFor(new URL('https://a.internal.corp/'), n), undefined);
  assert.equal(proxyFor(new URL('https://10.2.3.4/'), n), undefined);
  assert.equal(proxyFor(new URL('https://example.com/'), n)?.port, 3128);
});

test('SITEFILE_* environment, never HTTP(S)_PROXY; flags merge on top', () => {
  const n = networkFromEnv({
    SITEFILE_PROXY: 'http://p:1',
    SITEFILE_NO_PROXY: 'localhost, 127.0.0.1',
    SITEFILE_DENY_HOSTS: 'a.com b.com',
    SITEFILE_FETCH_TIMEOUT: 'connect=500, read=2000',
    HTTPS_PROXY: 'http://ignored:9',
    HTTP_PROXY: 'http://ignored:9',
  });
  assert.deepEqual(n, { proxy: 'http://p:1', noProxy: ['localhost', '127.0.0.1'], denyHosts: ['a.com', 'b.com'], connectTimeoutMs: 500, readTimeoutMs: 2000 });
  assert.deepEqual(networkFromEnv({ HTTPS_PROXY: 'http://ignored:9', NO_PROXY: '*' }), {});
  assert.deepEqual(parseFetchTimeout('750'), { connectTimeoutMs: 750, readTimeoutMs: 750 });
  assert.throws(() => parseFetchTimeout('fast'), (e: unknown) => isSitefileError(e, 'E_USAGE'));
  assert.throws(() => parseFetchTimeout('connect=0'), (e: unknown) => isSitefileError(e, 'E_USAGE'));
  const m = mergeNetwork(n, { denyHosts: ['b.com', 'c.com'], readTimeoutMs: 9 });
  assert.deepEqual(m.denyHosts, ['a.com', 'b.com', 'c.com']);
  assert.equal(m.connectTimeoutMs, 500);
  assert.equal(m.readTimeoutMs, 9);
  // --proxy '' turns SITEFILE_PROXY off.
  const off = mergeNetwork({ proxy: 'http://p:1' }, { proxy: '' });
  assert.equal(networkProblem(off), undefined);
  assert.equal(proxyFor(new URL('https://example.com/'), off), undefined);
});

// ------------------------------------------------------------------ proxy

test('http through the proxy is absolute-form and carries the proxy credentials', async () => {
  const proxy = await startProxy('via proxy');
  const p = net({ proxy: proxy.url.replace('http://', 'http://u:p%40ss@') });
  const r = await fetchRemote('http://example.test/x?q=1', p);
  assert.equal(Buffer.from(r.bytes).toString(), 'via proxy');
  assert.equal(proxy.seen[0]?.line, 'GET http://example.test/x?q=1');
  assert.equal(proxy.seen[0]?.auth?.toLowerCase(), `basic ${Buffer.from('u:p@ss').toString('base64')}`.toLowerCase());
});

test('a refused tunnel names the proxy without its credentials', async () => {
  const proxy = await startProxy();
  const p = net({ proxy: proxy.url.replace('http://', 'http://u:secret@') });
  await rejectsWith(fetchRemote('https://refused.test/a.css', p), 'E_FETCH', `proxy ${proxy.url} refused to connect to refused.test:443`);
  const e = await rejectsWith(fetchRemote('https://auth.test:8443/', p), 'E_FETCH', `proxy ${proxy.url} requires authentication (HTTP 407)`);
  assert.ok(!e.message.includes('secret'));
  assert.equal(proxy.seen[0]?.line, 'CONNECT refused.test:443');
  assert.equal(proxy.seen[0]?.auth, `Basic ${Buffer.from('u:secret').toString('base64')}`);
});

test('no-proxy hosts are reached directly', async () => {
  const base = await serve((_q, res) => res.end('direct'));
  const port = new URL(base).port;
  // Nothing listens on the proxy port: using it would fail.
  const dead = await listen(createNetServer());
  servers.pop()?.close();
  for (const bypass of ['127.0.0.1', '127.0.0.0/8', `127.0.0.1:${port}`, '*']) {
    const r = await fetchRemote(`${base}/`, net({ proxy: `http://127.0.0.1:${dead}`, noProxy: ['other.test', bypass] }));
    assert.equal(Buffer.from(r.bytes).toString(), 'direct', bypass);
  }
});

test('a proxy does not widen the private-network rule', async () => {
  const proxy = await startProxy('x');
  const target = await serve((_q, res) => res.end('x'));
  const p = net({ proxy: proxy.url }, { allowPrivateNetwork: false });
  // An IP literal is refused before anything is sent; a name is resolved here first.
  await rejectsWith(fetchRemote(`${target}/`, p), 'E_SSRF_BLOCKED');
  await rejectsWith(fetchRemote(`http://localhost:${new URL(target).port}/`, p), 'E_SSRF_BLOCKED', /resolves to non-public address/);
  assert.deepEqual(proxy.seen, [], 'the proxy was contacted');
});

const skipTls = opensslAvailable() ? false : SKIP_REASON;
let pki: TestPki | undefined;
before(() => {
  if (!skipTls) pki = createTestPki();
});
after(() => pki?.dispose());

test('https goes through a CONNECT tunnel, verified end to end', { skip: skipTls }, async () => {
  const { cert, key } = pki!.issue('IP:127.0.0.1,DNS:tunnel.test');
  const server = createHttpsServer({ cert, key }, (_q, res) => res.end('secure'));
  const port = await listen(server);
  const proxy = await startProxy();
  const p: FetchPolicy = { ...DEFAULT_FETCH_POLICY, allowPrivateNetwork: true, ca: pki!.caCert, network: { proxy: proxy.url } };
  // tunnel.test exists only for the proxy (it maps it to 127.0.0.1); the certificate is checked for that name.
  const r = await fetchRemote(`https://tunnel.test:${port}/a`, p);
  assert.equal(Buffer.from(r.bytes).toString(), 'secure');
  assert.equal(proxy.seen[0]?.line, `CONNECT tunnel.test:${port}`);
  // A certificate for another name fails, through the tunnel as directly.
  const other = pki!.issue('DNS:other.example');
  const bad = createHttpsServer({ cert: other.cert, key: other.key }, (_q, res) => res.end('x'));
  const badPort = await listen(bad);
  await rejectsWith(fetchRemote(`https://tunnel.test:${badPort}/`, p), 'E_FETCH');
});

// ------------------------------------------------------------------ host rules

test('host rules apply to every hop; deny beats allow; a denied host never reaches the proxy', async () => {
  const base = await serve((req, res, b) => {
    if (req.url === '/start') {
      res.writeHead(302, { location: `http://localhost:${new URL(b).port}/end` });
      res.end();
    } else res.end('end');
  });
  const start = `${base}/start`;
  const both: NetworkSettings = { allowHosts: ['127.0.0.1', 'localhost'] };
  assert.equal(Buffer.from((await fetchRemote(start, net(both))).bytes).toString(), 'end');
  await rejectsWith(
    fetchRemote(start, net({ ...both, denyHosts: ['localhost'] })),
    'E_HOST_BLOCKED',
    `refusing to connect to localhost: it matches deny-hosts rule 'localhost' (http://localhost:${new URL(base).port}/end)`,
  );
  await rejectsWith(fetchRemote(start, net({ allowHosts: ['127.0.0.1'] })), 'E_HOST_BLOCKED', /it matches no allow-hosts rule/);
  const proxy = await startProxy('x');
  await rejectsWith(fetchRemote('http://denied.test/', net({ proxy: proxy.url, denyHosts: ['*.denied.test'] })), 'E_HOST_BLOCKED');
  assert.deepEqual(proxy.seen, []);
});

test('the CLI reads SITEFILE_DENY_HOSTS and --allow-hosts and names the rule', async () => {
  const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
  const os = await import('node:os');
  const path = await import('node:path');
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-net-'));
  try {
    await writeFile(path.join(dir, 'Sitefile'), 'FROM scratch\nIMPORT css FROM https://cdn.example.com/a.css\nCOPY css: /a.css\nEXPORT /\n');
    const home = path.join(dir, 'home');
    const denied = await runCli(home, ['build', dir, '-o', path.join(dir, 'out'), '--allow-hosts', 'example.com'], undefined, { SITEFILE_DENY_HOSTS: 'cdn.example.com' });
    assert.equal(denied.code, 1);
    assert.match(denied.stderr, /error\[E_HOST_BLOCKED\]: \S*Sitefile:2:1: refusing to connect to cdn\.example\.com: it matches deny-hosts rule 'cdn\.example\.com'/);
    const bad = await runCli(home, ['build', dir, '--proxy', 'socks5://u:pw@p:1']);
    assert.equal(bad.code, 2);
    assert.match(bad.stderr, /invalid proxy URL 'socks5:\/\/p:1'/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

// ------------------------------------------------------------------ timeouts

test('the read timeout fires before the total timeout (silent server, stalled body)', async () => {
  const silent = await serve(() => {});
  const stalled = await serve((_q, res) => {
    res.writeHead(200, { 'content-length': '10' });
    res.write('abc');
  });
  for (const base of [silent, stalled]) {
    const t0 = Date.now();
    await rejectsWith(fetchRemote(`${base}/`, net({ readTimeoutMs: 200 }, { timeoutMs: 20_000 })), 'E_FETCH_TIMEOUT', `fetching ${base}/: no data for 200 ms (read timeout)`);
    assert.ok(Date.now() - t0 < 2000);
  }
});

test('the total timeout still applies with longer connect/read timeouts', async () => {
  const base = await serve(() => {});
  await rejectsWith(fetchRemote(`${base}/`, net({ readTimeoutMs: 60_000, connectTimeoutMs: 60_000 }, { timeoutMs: 200 })), 'E_FETCH_TIMEOUT', `fetching ${base}/ timed out after 200 ms`);
});

test('the connect timeout bounds an unanswered connection', async () => {
  // 10.255.255.1 is unrouted: packets are dropped (a connect timeout) on most networks, or refused
  // at once. Either way it must not wait for the 20 s total.
  const t0 = Date.now();
  const p = fetchRemote('http://10.255.255.1:81/', net({ connectTimeoutMs: 300 }, { timeoutMs: 20_000 }));
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e));
    if (e.code === 'E_FETCH_TIMEOUT') assert.equal(e.detail, 'fetching http://10.255.255.1:81/: connecting timed out after 300 ms (connect timeout)');
    else assert.equal(e.code, 'E_FETCH', e.message);
    return true;
  });
  assert.ok(Date.now() - t0 < 5000);
});

// ------------------------------------------------------------------ registry

test('registry: host rules cover the registry itself; read timeout replaces limits.timeoutMs', async () => {
  const client = new RegistryClient('127.0.0.1:9', { network: { denyHosts: ['127.0.0.1'], readTimeoutMs: 1234 } });
  assert.equal(client.limits.timeoutMs, 1234);
  await rejectsWith(client.getManifest('a/b', 'latest'), 'E_HOST_BLOCKED', /^refusing to connect to 127\.0\.0\.1: it matches deny-hosts rule '127\.0\.0\.1' \(https:\/\/127\.0\.0\.1:9\/v2\//);
});

test('registry: requests go through the proxy (a loopback registry over http)', async () => {
  const reg = await startMockRegistry();
  const proxy = await startProxy();
  try {
    const seeded = reg.seedImage('lib/site', '1', { layers: [{ '/index.html': 'hi' }] });
    const client = new RegistryClient(reg.host, { network: { proxy: proxy.url } });
    const m = await client.getManifest('lib/site', '1');
    assert.equal(m.digest, seeded.manifestDigest);
    // https first (tunnelled, then refused by the plain-http registry), then http in absolute form.
    assert.ok(proxy.seen.some((s) => s.line === `CONNECT ${reg.host}`), JSON.stringify(proxy.seen));
    assert.ok(proxy.seen.some((s) => s.line === `GET http://${reg.host}/v2/lib/site/manifests/1`), JSON.stringify(proxy.seen));
    // Bypassed: nothing more reaches the proxy.
    const n = proxy.seen.length;
    const direct = new RegistryClient(reg.host, { network: { proxy: proxy.url, noProxy: ['127.0.0.1'] } });
    assert.equal((await direct.getManifest('lib/site', '1')).digest, seeded.manifestDigest);
    assert.equal(proxy.seen.length, n);
  } finally {
    await reg.close();
  }
});
