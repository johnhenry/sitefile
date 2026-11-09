// serve's proxy rules and --proxy routes (issue #172).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { promises as fs } from 'node:fs';
import { closeServer, createSiteServer, listen, type ServeProxy } from '../src/server.js';
import { parseProxyRoute } from '../src/cli/servers.js';
import { DEFAULT_FETCH_POLICY } from '../src/resources/types.js';
import { BasicAuth, makeVerifier } from '../src/serve-auth.js';
import { makeFile, Tree } from '../src/tree.js';
import { runCli, tmpDir } from './helpers/cli.js';

const enc = (s: string) => new TextEncoder().encode(s);
const servers: http.Server[] = [];
after(async () => {
  for (const s of servers) {
    s.closeAllConnections();
    await new Promise((r) => s.close(r));
  }
});

/** An upstream that echoes what it got, with a few extra answers. */
async function upstream(): Promise<{ origin: string; seen: Array<{ url: string; headers: http.IncomingHttpHeaders; method: string }> }> {
  const seen: Array<{ url: string; headers: http.IncomingHttpHeaders; method: string }> = [];
  const s = http.createServer((req, res) => {
    seen.push({ url: req.url ?? '', headers: req.headers, method: req.method ?? '' });
    if (req.url?.includes('moved')) {
      res.writeHead(302, { location: '/elsewhere' });
      return res.end();
    }
    if (req.url?.includes('slow')) return; // never answers
    if (req.url?.includes('big')) {
      res.writeHead(200, { 'content-type': 'text/plain' });
      return res.end('x'.repeat(2000));
    }
    res.setHeader('set-cookie', ['a=1', 'b=2']);
    res.writeHead(201, { 'content-type': 'application/json', 'x-up': 'yes', connection: 'close', 'cache-control': 'max-age=5' });
    res.end(JSON.stringify({ url: req.url }));
  });
  servers.push(s);
  await new Promise<void>((r) => s.listen(0, '127.0.0.1', r));
  return { origin: `http://127.0.0.1:${(s.address() as AddressInfo).port}`, seen };
}

function proxy(routes: ServeProxy['routes'], allowHosts: string[], extra: Partial<typeof DEFAULT_FETCH_POLICY> = {}): ServeProxy {
  return { routes, policy: { ...DEFAULT_FETCH_POLICY, allowHttp: true, allowPrivateNetwork: true, ...extra, network: { strict: true, allowHosts, ...(extra.network ?? {}) } } };
}

async function site(files: Record<string, string>, options: Parameters<typeof createSiteServer>[1]): Promise<{ base: string; stop: () => Promise<void> }> {
  const tree = Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  const server = createSiteServer(tree, options);
  const addr = await listen(server, '127.0.0.1', 0);
  return { base: `http://127.0.0.1:${addr.port}`, stop: () => closeServer(server) };
}

const get = (url: string, init: RequestInit = {}) => fetch(url, { redirect: 'manual', ...init });

test('--proxy: the rest of the path and the query go along; credentials do not; hop-by-hop headers are dropped', async () => {
  const up = await upstream();
  const s = await site({ '/index.html': 'home' }, { proxy: proxy([{ prefix: '/api', target: `${up.origin}/v1` }], ['127.0.0.1']) });
  try {
    const r = await get(`${s.base}/api/a%20b/c?q=1`, { headers: { cookie: 'secret=1', authorization: 'Bearer x', accept: 'text/plain', 'accept-language': 'fr' } });
    assert.equal(r.status, 201);
    assert.deepEqual(await r.json(), { url: '/v1/a%20b/c?q=1' });
    assert.deepEqual(r.headers.getSetCookie(), ['a=1', 'b=2']);
    assert.equal(r.headers.get('x-up'), 'yes');
    assert.equal(r.headers.get('cache-control'), 'max-age=5');
    assert.notEqual(r.headers.get('connection'), 'close');
    const got = up.seen.at(-1)!;
    assert.equal(got.headers.cookie, undefined);
    assert.equal(got.headers.authorization, undefined);
    assert.equal(got.headers.accept, 'text/plain');
    assert.equal(got.headers['accept-language'], 'fr');
    // The prefix itself, and a redirect passed through as it is.
    assert.deepEqual(await (await get(`${s.base}/api`)).json(), { url: '/v1' });
    const moved = await get(`${s.base}/api/moved`);
    assert.equal(moved.status, 302);
    assert.equal(moved.headers.get('location'), '/elsewhere');
    // HEAD is HEAD upstream.
    const head = await get(`${s.base}/api/x`, { method: 'HEAD' });
    assert.equal(head.status, 201);
    assert.equal(up.seen.at(-1)!.method, 'HEAD');
    // Other methods stay 405.
    assert.equal((await get(`${s.base}/api/x`, { method: 'POST' })).status, 405);
  } finally {
    await s.stop();
  }
});

test('_redirects proxy rules: only to allowed hosts; a file answers a non-forced rule first; 200! goes first', async () => {
  const up = await upstream();
  const port = new URL(up.origin).port;
  const files = {
    '/index.html': 'home',
    '/f/x': 'file',
    '/_redirects': `/api/* ${up.origin}/r/:splat 200\n/f/* ${up.origin}/f/:splat 200\n/forced ${up.origin}/forced 200!\n/other/* http://localhost:${port}/:splat 200\n`,
  };
  const s = await site(files, { proxy: proxy([], ['127.0.0.1']) });
  try {
    assert.deepEqual(await (await get(`${s.base}/api/z?k=v`)).json(), { url: '/r/z?k=v' });
    assert.equal(await (await get(`${s.base}/f/x`)).text(), 'file');
    assert.deepEqual(await (await get(`${s.base}/f/y`)).json(), { url: '/f/y' });
    assert.deepEqual(await (await get(`${s.base}/forced`)).json(), { url: '/forced' });
    const refused = await get(`${s.base}/other/z`);
    assert.equal(refused.status, 502);
    assert.match(await refused.text(), /^502 Bad Gateway: error\[E_HOST_BLOCKED\]: refusing to connect to localhost: .*add --allow-hosts localhost\n$/);
  } finally {
    await s.stop();
  }
  // Without proxy settings (a detached server) every proxy rule is refused.
  const bare = await site(files, {});
  try {
    const r = await get(`${bare.base}/api/z`);
    assert.equal(r.status, 502);
    assert.match(await r.text(), /^502 Bad Gateway: error\[E_PROTOCOL\]/); // the default policy: https only, then the host list
  } finally {
    await bare.stop();
  }
});

test('--auth covers proxied paths; their credentials stay here; --compress leaves proxied bodies alone', async () => {
  const up = await upstream();
  const auth = new BasicAuth([{ user: 'u', verifier: makeVerifier('pw', undefined, 1000) }]);
  const files = { '/index.html': 'home', '/_redirects': `/rule/* ${up.origin}/r/:splat 200\n` };
  const s = await site(files, { auth, compress: true, proxy: proxy([{ prefix: '/api', target: up.origin }], ['127.0.0.1']) });
  try {
    const before = up.seen.length;
    for (const p of ['/api/x', '/rule/x']) {
      const denied = await get(`${s.base}${p}`);
      assert.equal(denied.status, 401, p);
      assert.match(denied.headers.get('www-authenticate') ?? '', /^Basic /);
    }
    assert.equal(up.seen.length, before, 'nothing reached the upstream');
    const authorization = `Basic ${Buffer.from('u:pw').toString('base64')}`;
    for (const [p, url] of [['/api/x', '/x'], ['/rule/x', '/r/x']] as const) {
      assert.deepEqual(await (await get(`${s.base}${p}`, { headers: { authorization } })).json(), { url }, p);
      assert.equal(up.seen.at(-1)!.headers.authorization, undefined);
    }
    // A large compressible upstream body is passed on as it came, not gzipped here.
    const big = await get(`${s.base}/api/big`, { headers: { authorization, 'accept-encoding': 'gzip' } });
    assert.equal(big.status, 200);
    assert.equal(big.headers.get('content-encoding'), null);
    assert.equal(await big.text(), 'x'.repeat(2000));
  } finally {
    await s.stop();
  }
});

test('the fetch policy applies: http, private addresses, timeouts', async () => {
  const up = await upstream();
  const route = [{ prefix: '/api', target: `${up.origin}` }];
  const noHttp = await site({}, { proxy: proxy(route, ['127.0.0.1'], { allowHttp: false }) });
  const noPrivate = await site({}, { proxy: proxy(route, ['127.0.0.1'], { allowPrivateNetwork: false }) });
  const slow = await site({}, { proxy: proxy(route, ['127.0.0.1'], { network: { strict: true, allowHosts: ['127.0.0.1'], readTimeoutMs: 200 } }) });
  try {
    assert.match(await (await get(`${noHttp.base}/api/x`)).text(), /E_PROTOCOL/);
    assert.match(await (await get(`${noPrivate.base}/api/x`)).text(), /E_SSRF_BLOCKED/);
    const t = await get(`${slow.base}/api/slow`);
    assert.equal(t.status, 502);
    assert.match(await t.text(), /E_FETCH_TIMEOUT/);
  } finally {
    await noHttp.stop();
    await noPrivate.stop();
    await slow.stop();
  }
});

test('--proxy parsing and the CLI', async () => {
  assert.deepEqual(parseProxyRoute('/api=https://x.example/v1'), { prefix: '/api', target: 'https://x.example/v1' });
  assert.deepEqual(parseProxyRoute('/api/=http://localhost:3000'), { prefix: '/api', target: 'http://localhost:3000' });
  assert.deepEqual(parseProxyRoute('/=https://x.example'), { prefix: '/', target: 'https://x.example' });
  for (const bad of ['api=https://x', '/api', '/api=ftp://x', '/a b=https://x', '/api=https://', '/:x=https://y']) {
    assert.throws(() => parseProxyRoute(bad), /--proxy expects \/prefix=http\(s\):\/\/host\[\/path\]/, bad);
  }
  const d = await tmpDir();
  await fs.mkdir(path.join(d, 'site'));
  const r = await runCli(path.join(d, 'home'), ['serve', '-d', '--proxy', '/api=https://x.example', './site'], d);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--proxy is for a server in the foreground \(or --watch\), not -d/);
});
