import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import net from 'node:net';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { accessLogLine, networkAddresses, networkUrls, byteRange, cacheControlFor, logLineAsJson, closeServer, createSiteServer, hostAllowed, preflightHeaders, parseAllowedHost, earlyHints, httpDate, parseHttpDate, listen, LiveReload, originForm, RELOAD_SCRIPT, safeSegments, normalizeBase } from '../src/server.js';
import { makeFile, Tree } from '../src/tree.js';
import { siteFileWarnings } from '../src/serve-rules.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

/** GET `url` as a page navigation (fetch() always sends `sec-fetch-mode: cors`, which is not one). */
function navigate(url: string): Promise<string> {
  return new Promise((resolve, reject) => {
    http
      .get(url, { headers: { 'sec-fetch-mode': 'navigate', 'sec-fetch-dest': 'document' } }, (res) => {
        const chunks: Buffer[] = [];
        res.on('data', (c: Buffer) => chunks.push(c));
        res.on('end', () => resolve(Buffer.concat(chunks).toString()));
      })
      .on('error', reject);
  });
}


const enc = (s: string) => new TextEncoder().encode(s);
const tree = Tree.fromFiles(
  Object.entries({
    '/index.html': '<h1>home</h1>',
    '/about/index.html': '<h1>about</h1>',
    '/app.js': 'console.log(1)',
    '/style.css': 'a{}',
    '/data.json': '{}',
    '/logo.png': 'PNG',
    '/blob.xyz': 'bin',
    '/page.html': 'page',
  }).map(([p, c]) => [p, makeFile(enc(c), 't')] as const),
);

interface Res {
  status: number;
  headers: http.IncomingHttpHeaders;
  body: string;
}
function req(port: number, rawPath: string, method = 'GET', headers: Record<string, string> = {}): Promise<Res> {
  return new Promise((resolve, reject) => {
    // http.request sends the path verbatim, so traversal attempts are not normalized away
    const r = http.request({ host: '127.0.0.1', port, path: rawPath, method, headers, agent: false }, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (c: Buffer) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode ?? 0, headers: res.headers, body: Buffer.concat(chunks).toString() }));
    });
    r.on('error', reject);
    r.end();
  });
}

async function withServer(opts: { spa?: boolean }, fn: (port: number, log: string[]) => Promise<void>): Promise<void> {
  const log: string[] = [];
  const server = createSiteServer(tree, { ...opts, log: (l) => log.push(l) });
  const { port } = await listen(server, '127.0.0.1', 0);
  try {
    await fn(port, log);
  } finally {
    await closeServer(server);
  }
}

test('serves index files, nested indexes and files with correct headers', async () => {
  await withServer({}, async (port) => {
    let r = await req(port, '/');
    assert.equal(r.status, 200);
    assert.equal(r.body, '<h1>home</h1>');
    assert.equal(r.headers['content-type'], 'text/html; charset=utf-8');
    assert.equal(r.headers['cache-control'], 'no-cache');
    assert.equal(r.headers['x-content-type-options'], 'nosniff');
    assert.match(String(r.headers.etag), /^"[0-9a-f]{64}"$/);
    r = await req(port, '/about/');
    assert.equal(r.body, '<h1>about</h1>');
    assert.equal((await req(port, '/?x=1#frag')).status, 200);
    assert.equal((await req(port, '/index.html')).body, '<h1>home</h1>');
  });
});

test('directory without a slash redirects (301) and keeps the query; no open redirects', async () => {
  await withServer({}, async (port) => {
    const r = await req(port, '/about?x=1');
    assert.equal(r.status, 301);
    assert.equal(r.headers.location, '/about/?x=1');
    const evil = await req(port, '//about');
    assert.equal(evil.status, 301);
    assert.equal(evil.headers.location, '/about/');
    assert.equal((await req(port, '/about/index.html/')).status, 404);
  });
});

test('404 for unknown paths, no directory listings, small error page', async () => {
  await withServer({}, async (port) => {
    const r = await req(port, '/missing');
    assert.equal(r.status, 404);
    assert.match(r.body, /404 Not Found/);
    assert.equal(r.headers['content-type'], 'text/html; charset=utf-8');
    // a directory with no index.html is a 404, not a listing
    const t = Tree.fromFiles([['/d/file.txt', makeFile(enc('x'), 't')]]);
    const s = createSiteServer(t);
    const { port: p2 } = await listen(s, '127.0.0.1', 0);
    try {
      assert.equal((await req(p2, '/d/')).status, 404);
      assert.equal((await req(p2, '/')).status, 404);
    } finally {
      await closeServer(s);
    }
  });
});

// johnhenry/sitefile#194: the fallback is decided by navigation, not by the look of the path.
test('SPA fallback answers navigations only, nearest app shell, exclusions, 200.html and --spa-fallback', async () => {
  const doc = { 'sec-fetch-mode': 'navigate', 'sec-fetch-dest': 'document' };
  await withServer({ spa: true }, async (port) => {
    let r = await req(port, '/some/client/route', 'GET', doc);
    assert.equal(r.status, 200);
    assert.equal(r.body, '<h1>home</h1>');
    assert.equal((await req(port, '/users/jane.doe', 'GET', doc)).body, '<h1>home</h1>', 'a dotted navigation path falls back');
    assert.equal((await req(port, '/users/jane.doe', 'GET', { accept: 'text/html,*/*' })).status, 200, 'Accept text/html without Sec-Fetch');
    assert.equal((await req(port, '/assets/app.js', 'GET', { 'sec-fetch-mode': 'no-cors', 'sec-fetch-dest': 'script', accept: '*/*' })).status, 404, 'a missing script stays 404');
    assert.equal((await req(port, '/some/route')).status, 404, 'no navigation headers: no fallback');
    r = await req(port, '/about/team', 'GET', doc);
    assert.equal(r.body, '<h1>about</h1>', 'the nearest index.html (a nested app root)');
    assert.equal((await req(port, '/app.js')).body, 'console.log(1)');
  });
  const files = { '/index.html': 'root', '/200.html': 'surge shell', '/admin/index.html': 'admin', '/api/x.json': '{}', '/app.html': 'app' };
  await withTree(files, { spa: true, spaExclude: ['/api/*', '/admin/raw'] }, async (port) => {
    assert.equal((await req(port, '/route', 'GET', doc)).body, 'surge shell', '200.html wins over index.html');
    assert.equal((await req(port, '/admin/users/5', 'GET', doc)).body, 'admin');
    assert.equal((await req(port, '/api/missing', 'GET', doc)).status, 404, 'excluded');
    assert.equal((await req(port, '/admin/raw', 'GET', doc)).status, 404, 'excluded');
  });
  await withTree(files, { spa: true, spaFallback: '/app.html' }, async (port) => {
    assert.equal((await req(port, '/admin/users/5', 'GET', doc)).body, 'app', '--spa-fallback is the only fallback');
  });
});

test('HEAD returns headers without a body; other methods are 405', async () => {
  await withServer({}, async (port) => {
    const g = await req(port, '/app.js');
    const h = await req(port, '/app.js', 'HEAD');
    assert.equal(h.status, 200);
    assert.equal(h.body, '');
    assert.equal(h.headers['content-length'], g.headers['content-length']);
    assert.equal(h.headers.etag, g.headers.etag);
    for (const m of ['POST', 'PUT', 'DELETE', 'PATCH', 'OPTIONS']) {
      const r = await req(port, '/', m);
      assert.equal(r.status, 405, m);
      assert.equal(r.headers.allow, 'GET, HEAD');
    }
  });
});

test('ETag and If-None-Match give 304', async () => {
  await withServer({}, async (port) => {
    const first = await req(port, '/style.css');
    const etag = String(first.headers.etag);
    for (const inm of [etag, `W/${etag}`, `"nope", ${etag}`, '*']) {
      const r = await req(port, '/style.css', 'GET', { 'if-none-match': inm });
      assert.equal(r.status, 304, inm);
      assert.equal(r.body, '');
      assert.equal(r.headers.etag, etag);
    }
    assert.equal((await req(port, '/style.css', 'GET', { 'if-none-match': '"other"' })).status, 200);
  });
});

test('content types come from the file extension', async () => {
  await withServer({}, async (port) => {
    const type = async (p: string) => (await req(port, p)).headers['content-type'];
    assert.match(String(await type('/app.js')), /^text\/javascript/);
    assert.match(String(await type('/style.css')), /^text\/css/);
    assert.match(String(await type('/data.json')), /^application\/json/);
    assert.equal(await type('/logo.png'), 'image/png');
    assert.equal(await type('/blob.xyz'), 'application/octet-stream');
  });
});

test('traversal, encoded separators, NUL and backslashes are rejected', async () => {
  await withServer({}, async (port) => {
    for (const p of ['/..%2f', '/..%2F..%2Fetc/passwd', '/%2e%2e/secret', '/../etc/passwd', '/a/../index.html', '/%00', '/index.html%00.png', '/..\\x', '/%5cwindows', '/a%2fb', '/%', '/%zz', '/.%2e/x', '/%c0%af']) {
      const r = await req(port, p);
      assert.ok(r.status === 400 || r.status === 404, `${p} -> ${r.status}`);
      assert.doesNotMatch(r.body, /root:|home/, p);
    }
    for (const p of ['/..%2f', '/%2e%2e/secret', '/../etc/passwd', '/%00', '/..\\x', '/a%2fb', '/%zz']) {
      assert.equal((await req(port, p)).status, 400, p);
    }
  });
  assert.deepEqual(safeSegments('/a/b'), ['a', 'b']);
  assert.deepEqual(safeSegments('/a%20b/c'), ['a b', 'c']);
  assert.equal(safeSegments('relative'), undefined);
});

test('absolute-form request targets are served by their path and query (johnhenry/sitefile#189)', async () => {
  await withServer({}, async (port, log) => {
    const home = await req(port, 'http://example.com/index.html');
    assert.equal(home.status, 200);
    assert.equal(home.body, '<h1>home</h1>');
    assert.equal((await req(port, 'http://example.com')).body, '<h1>home</h1>');
    assert.equal((await req(port, 'HTTPS://Example.COM:8443?x=1')).body, '<h1>home</h1>');
    assert.equal((await req(port, 'http://example.com/about/')).body, '<h1>about</h1>');
    const dir = await req(port, 'http://example.com/about?x=1');
    assert.equal(dir.status, 301);
    assert.equal(dir.headers.location, '/about/?x=1');
    assert.equal((await req(port, 'http://example.com/..%2f')).status, 400);
    assert.equal((await req(port, 'ftp://example.com/index.html')).status, 400);
    assert.match(log[0]!, /^GET http:\/\/example\.com\/index\.html 200 /);
  });
  assert.equal(originForm('http://h/a?b'), '/a?b');
  assert.equal(originForm('http://h?b'), '/?b');
  assert.equal(originForm('http://h'), '/');
  assert.equal(originForm('/a'), '/a');
  assert.equal(originForm('*'), '*');
});

test('byte ranges: 206 with Content-Range, 416, If-Range, HEAD and ignored multi-ranges (johnhenry/sitefile#166)', async () => {
  await withTree({ '/v.txt': '0123456789', '/empty.txt': '' }, {}, async (port) => {
    const full = await req(port, '/v.txt');
    assert.equal(full.status, 200);
    assert.equal(full.headers['accept-ranges'], 'bytes');
    const etag = String(full.headers.etag);
    const r = (range: string, extra: Record<string, string> = {}, method = 'GET') => req(port, '/v.txt', method, { range, ...extra });
    let a = await r('bytes=2-4');
    assert.equal(a.status, 206);
    assert.equal(a.body, '234');
    assert.equal(a.headers['content-range'], 'bytes 2-4/10');
    assert.equal(a.headers['content-length'], '3');
    assert.equal(a.headers['content-type'], 'text/plain; charset=utf-8');
    assert.equal(a.headers.etag, etag);
    assert.equal((await r('bytes=7-')).body, '789');
    assert.equal((await r('bytes=-3')).body, '789');
    assert.equal((await r('bytes=-30')).headers['content-range'], 'bytes 0-9/10');
    assert.equal((await r('bytes=8-100')).headers['content-range'], 'bytes 8-9/10');
    a = await r('bytes=10-');
    assert.equal(a.status, 416);
    assert.equal(a.headers['content-range'], 'bytes */10');
    assert.equal(a.body, '');
    assert.equal((await r('bytes=-0')).status, 416);
    assert.equal((await req(port, '/empty.txt', 'GET', { range: 'bytes=0-' })).status, 416);
    for (const ignored of ['bytes=0-1,4-5', 'bytes=5-2', 'items=0-1', 'bytes=a-b', 'bytes=1']) {
      const x = await r(ignored);
      assert.equal(x.status, 200, ignored);
      assert.equal(x.body, '0123456789', ignored);
    }
    assert.equal((await r('bytes=0-0', { 'if-range': etag })).status, 206);
    assert.equal((await r('bytes=0-0', { 'if-range': '"other"' })).status, 200);
    assert.equal((await r('bytes=0-0', { 'if-range': `W/${etag}` })).status, 200, 'If-Range is a strong comparison');
    assert.equal((await r('bytes=0-0', { 'if-none-match': etag })).status, 304);
    const head = await r('bytes=0-1', {}, 'HEAD');
    assert.equal(head.status, 200);
    assert.equal(head.headers['content-length'], '10');
    assert.equal(head.headers['accept-ranges'], 'bytes');
  });
  assert.deepEqual(byteRange('bytes=0-', 5), [0, 4]);
  assert.deepEqual(byteRange(' BYTES= 1 - 2 ', 5), [1, 2]);
  assert.deepEqual(byteRange('bytes=0-99999999999999999999', 5), [0, 4]);
  assert.equal(byteRange('bytes=99999999999999999999-', 5), 'unsatisfiable');
  assert.equal(byteRange(undefined, 5), undefined);
});

test('Last-Modified, If-Modified-Since and 103 Early Hints (johnhenry/sitefile#228)', async () => {
  const t = Tree.fromFiles(
    Object.entries({ '/index.html': '<h1>x</h1>', '/a.css': 'a{}', '/_headers': '/\n  Link: </a.css>; rel=preload; as=style\n' }).map(([p, c]) => [p, makeFile(enc(c), 't')] as const),
  );
  const at = 1700000000;
  const s = createSiteServer(t, { lastModified: () => at });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    const r = await req(port, '/a.css');
    assert.equal(r.headers['last-modified'], 'Tue, 14 Nov 2023 22:13:20 GMT');
    assert.equal((await req(port, '/a.css', 'GET', { 'if-modified-since': 'Tue, 14 Nov 2023 22:13:20 GMT' })).status, 304);
    assert.equal((await req(port, '/a.css', 'GET', { 'if-modified-since': 'Tue, 14 Nov 2023 22:13:19 GMT' })).status, 200);
    assert.equal((await req(port, '/a.css', 'GET', { 'if-modified-since': 'Tue, 14 Nov 2023 22:13:20 GMT', 'if-none-match': '"x"' })).status, 200);
    assert.equal((await req(port, '/missing')).headers['last-modified'], undefined);
    // The 103 comes first on the wire.
    const raw = await new Promise<string>((resolve, reject) => {
      const c = net.connect(port, '127.0.0.1', () => c.write('GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n'));
      let d = '';
      c.on('data', (x) => (d += x));
      c.on('close', () => resolve(d));
      c.on('error', reject);
    });
    assert.match(raw, /^HTTP\/1\.1 103 Early Hints\r\nLink: <\/a\.css>; rel=preload; as=style\r\n\r\nHTTP\/1\.1 200 OK/i);
  } finally {
    await closeServer(s);
  }
  assert.equal(httpDate(0), 'Thu, 01 Jan 1970 00:00:00 GMT');
  assert.equal(parseHttpDate('Sun, 06 Nov 1994 08:49:37 GMT'), 784111777);
  for (const bad of ['Mon, 06 Nov 1994 08:49:37 GMT', 'Sunday, 06-Nov-94 08:49:37 GMT', 'Sun Nov  6 08:49:37 1994', 'Sun, 31 Nov 1994 08:49:37 GMT', 'Sun, 06 Nov 1994 24:00:00 GMT']) {
    assert.equal(parseHttpDate(bad), undefined, bad);
  }
  assert.deepEqual(earlyHints('</a.css>; rel=preload; as=style, </b.js>; rel=stylesheet, <https://c.example>; REL="preconnect"'), ['</a.css>; rel=preload; as=style', '<https://c.example>; REL="preconnect"']);
  assert.deepEqual(earlyHints('</a,b.css>; rel=preload'), ['</a,b.css>; rel=preload']);
  assert.deepEqual(earlyHints('garbage rel=preload'), []);
  assert.deepEqual(earlyHints('</a>; title="a b"; rel=preload'), []);
});

test('Host allow-list: DNS rebinding guard (johnhenry/sitefile#190)', async () => {
  const s = createSiteServer(tree, { allowedHosts: ['example.com', '.dev.test'] });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    assert.equal((await req(port, '/')).status, 200, 'the default 127.0.0.1:<port> Host');
    const bad = await req(port, '/', 'GET', { host: 'evil.example' });
    assert.equal(bad.status, 403);
    assert.match(bad.body, /--allowed-host/);
    assert.equal((await req(port, '/', 'GET', { host: 'www.dev.test' })).status, 200);
    assert.equal((await req(port, '/', 'GET', { host: 'app.localhost:5173' })).status, 200);
  } finally {
    await closeServer(s);
  }
  const allowed = ['example.com', '.dev.test'];
  for (const h of [undefined, '', 'localhost', 'LocalHost:80', 'x.localhost', 'localhost.', '127.0.0.1:8080', '10.0.0.7', '[::1]:8080', '[fe80::1%25en0]', 'example.com', 'EXAMPLE.com.:1', 'dev.test', 'a.dev.test']) {
    assert.equal(hostAllowed(allowed, h), !String(h).includes('%25'), String(h));
  }
  for (const h of ['evil.example', 'www.example.com', 'notdev.test', 'localhost.evil', '1.2.3.4.evil', '999.1.1.1', '[::1', 'a:b:80']) assert.equal(hostAllowed(allowed, h), false, h);
  assert.equal(hostAllowed(undefined, 'evil.example'), true, 'no list: no check');
  assert.equal(hostAllowed(['*'], 'evil.example'), true);
  assert.equal(parseAllowedHost('Example.COM.'), 'example.com');
  assert.equal(parseAllowedHost('.dev.test'), '.dev.test');
  assert.equal(parseAllowedHost('*'), '*');
  for (const bad of ['', '.', 'a..b', 'a b', 'a:80', 'http://a', '*.a']) assert.equal(parseAllowedHost(bad), undefined, bad);
});

test('--cors: Access-Control-Allow-Origin and OPTIONS preflights (johnhenry/sitefile#171)', async () => {
  const s = createSiteServer(tree, { cors: true });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    assert.equal((await req(port, '/app.js')).headers['access-control-allow-origin'], '*');
    assert.equal((await req(port, '/missing')).headers['access-control-allow-origin'], '*');
    const pre = await req(port, '/app.js', 'OPTIONS', { 'access-control-request-headers': 'x-a, x-b' });
    assert.equal(pre.status, 204);
    assert.equal(pre.headers['access-control-allow-methods'], 'GET, HEAD');
    assert.equal(pre.headers['access-control-allow-headers'], 'x-a, x-b');
    assert.equal((await req(port, '/app.js', 'POST')).headers.allow, 'GET, HEAD, OPTIONS');
  } finally {
    await closeServer(s);
  }
  await withServer({}, async (port) => {
    assert.equal((await req(port, '/app.js')).headers['access-control-allow-origin'], undefined);
    assert.equal((await req(port, '/app.js', 'OPTIONS')).status, 405);
  });
  assert.equal(preflightHeaders(' x-a,x-b '), 'x-a,x-b');
  assert.equal(preflightHeaders('a"b'), undefined);
  assert.equal(preflightHeaders(''), undefined);
});

test('--isolate: COOP, COEP and CORP on every response (johnhenry/sitefile#197)', async () => {
  for (const coep of ['require-corp', 'credentialless'] as const) {
    const s = createSiteServer(tree, { isolate: coep });
    const { port } = await listen(s, '127.0.0.1', 0);
    try {
      for (const p of ['/app.js', '/missing']) {
        const r = await req(port, p);
        assert.equal(r.headers['cross-origin-opener-policy'], 'same-origin');
        assert.equal(r.headers['cross-origin-embedder-policy'], coep);
        assert.equal(r.headers['cross-origin-resource-policy'], 'same-origin');
      }
    } finally {
      await closeServer(s);
    }
  }
  await withServer({}, async (port) => assert.equal((await req(port, '/app.js')).headers['cross-origin-embedder-policy'], undefined));
});

test('--max-age/--immutable/--max-age-html: a production cache preview (johnhenry/sitefile#175)', async () => {
  const s = createSiteServer(tree, { cache: { maxAge: 600, immutable: true } });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    const r = await req(port, '/app.js');
    assert.equal(r.headers['cache-control'], 'public, max-age=600, immutable');
    assert.equal((await req(port, '/app.js', 'GET', { 'if-none-match': String(r.headers.etag) })).headers['cache-control'], 'public, max-age=600, immutable');
    assert.equal((await req(port, '/')).headers['cache-control'], 'no-cache');
    assert.equal((await req(port, '/missing')).headers['cache-control'], 'no-cache');
  } finally {
    await closeServer(s);
  }
  assert.equal(cacheControlFor('text/html; charset=utf-8', { maxAgeHtml: 0 }), 'public, max-age=0');
  assert.equal(cacheControlFor('text/css; charset=utf-8', { maxAgeHtml: 5 }), undefined);
  assert.equal(cacheControlFor('image/png', { maxAge: 5 }), 'public, max-age=5');
  assert.equal(cacheControlFor('image/png', undefined), undefined);
});

test('--log-format json, --log-ip, --quiet and server logs --json conversion (johnhenry/sitefile#173)', async () => {
  const lines: string[] = [];
  const s = createSiteServer(tree, { logFormat: 'json', logIp: true, log: (l) => lines.push(l) });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    await req(port, '/app.js?x=1');
    await new Promise((r) => setTimeout(r, 50));
    const o = JSON.parse(lines[0]!);
    assert.deepEqual(Object.keys(o), ['type', 'ts', 'method', 'path', 'query', 'status', 'bytes', 'ms', 'encoding', 'range', 'remote']);
    assert.equal(o.type, 'request');
    assert.equal(o.path, '/app.js');
    assert.equal(o.query, 'x=1');
    assert.equal(o.status, 200);
    assert.equal(o.remote, '127.0.0.1');
    assert.equal(o.range, null);
  } finally {
    await closeServer(s);
  }
  const quiet: string[] = [];
  const q = createSiteServer(tree, { quiet: true, log: (l) => quiet.push(l) });
  const qp = await listen(q, '127.0.0.1', 0);
  try {
    await req(qp.port, '/app.js');
    assert.deepEqual(quiet, []);
  } finally {
    await closeServer(q);
  }
  const e = { ts: '2026-01-01T00:00:00.000Z', method: 'GET', target: '/a?b', status: 206, bytes: 2, ms: 1.24, range: 'bytes 0-1/10' };
  assert.equal(accessLogLine('text', { ...e, remote: '::1' }), 'GET /a?b 206 2 1.2ms ::1');
  assert.equal(
    accessLogLine('json', e),
    '{"type":"request","ts":"2026-01-01T00:00:00.000Z","method":"GET","path":"/a","query":"b","status":206,"bytes":2,"ms":1.2,"encoding":"identity","range":"bytes 0-1/10"}',
  );
  assert.equal(
    logLineAsJson('2026-01-01T00:00:00.000Z GET /a?b 200 5 0.4ms'),
    '{"type":"request","ts":"2026-01-01T00:00:00.000Z","method":"GET","path":"/a","query":"b","status":200,"bytes":5,"ms":0.4,"encoding":"identity","range":null}',
  );
  assert.equal(
    logLineAsJson('2026-01-01T00:00:00.000Z serving ./site (3 files) at http://127.0.0.1:8080/'),
    '{"type":"started","ts":"2026-01-01T00:00:00.000Z","site":"./site","files":3,"url":"http://127.0.0.1:8080/"}',
  );
  assert.equal(logLineAsJson('2026-01-01T00:00:00.000Z received SIGTERM, shutting down'), '{"type":"stopped","ts":"2026-01-01T00:00:00.000Z","signal":"SIGTERM"}');
  assert.equal(logLineAsJson('2026-01-01T00:00:00.000Z warning: x "y"'), '{"type":"warning","ts":"2026-01-01T00:00:00.000Z","message":"x \\"y\\""}');
  assert.equal(logLineAsJson('    at stack'), '{"type":"message","ts":"","message":"    at stack"}');
  assert.equal(logLineAsJson('{"type":"request"}'), '{"type":"request"}');
});

test('network URLs for a server bound to all interfaces (johnhenry/sitefile#169)', () => {
  assert.deepEqual(networkAddresses('127.0.0.1'), []);
  assert.deepEqual(networkAddresses('::1'), []);
  const v4 = networkAddresses('0.0.0.0');
  for (const a of v4) assert.match(a, /^[0-9.]+$/);
  assert.ok(!v4.some((a) => a.startsWith('127.')), 'no loopback');
  assert.deepEqual(v4, [...v4].sort());
  const all = networkAddresses('::');
  assert.deepEqual(all.slice(0, v4.length), v4, 'IPv4 first');
  for (const a of all.slice(v4.length)) assert.ok(a.includes(':') && !/^fe[89ab]/.test(a) && a !== '::1', a);
  assert.deepEqual(networkUrls('0.0.0.0', 8080), v4.map((a) => `http://${a}:8080/`));
});

test('--max-connections-per-ip: a connection over the limit gets 503 with Retry-After (johnhenry/sitefile#178)', async () => {
  const s = createSiteServer(tree, { maxConnectionsPerIp: 2, limitLoopback: true });
  const { port } = await listen(s, '127.0.0.1', 0);
  const open = async (): Promise<net.Socket> => {
    const c = net.connect(port, '127.0.0.1');
    await new Promise((r) => c.once('connect', r));
    return c;
  };
  const ask = (c: net.Socket): Promise<string> =>
    new Promise((resolve) => {
      let d = '';
      c.on('data', (x) => {
        d += x;
        if (d.includes('\r\n\r\n')) resolve(d);
      });
      c.write('GET /app.js HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n');
    });
  try {
    const a = await open();
    const b = await open();
    const c = await open();
    await new Promise((r) => setTimeout(r, 50));
    assert.match(await ask(a), /^HTTP\/1\.1 200/);
    const busy = await ask(c);
    assert.match(busy, /^HTTP\/1\.1 503/);
    assert.match(busy, /retry-after: 1/i);
    b.destroy();
    c.destroy();
    await new Promise((r) => setTimeout(r, 50));
    const d = await open();
    assert.match(await ask(d), /^HTTP\/1\.1 200/, 'a slot is free again');
    a.destroy();
    d.destroy();
  } finally {
    await closeServer(s);
  }
  // Loopback clients are not limited unless asked.
  const t = createSiteServer(tree, { maxConnectionsPerIp: 1 });
  const tp = await listen(t, '127.0.0.1', 0);
  try {
    const x = net.connect(tp.port, '127.0.0.1');
    await new Promise((r) => x.once('connect', r));
    assert.equal((await req(tp.port, '/app.js')).status, 200);
    x.destroy();
  } finally {
    await closeServer(t);
  }
});

test('an access log line is written per request: method path status bytes ms', async () => {
  await withServer({}, async (port, log) => {
    await req(port, '/app.js');
    await req(port, '/nope');
    await req(port, '/', 'POST');
    assert.match(log[0]!, /^GET \/app\.js 200 14 \d+\.\d+ms$/);
    assert.match(log[1]!, /^GET \/nope 404 \d+ \d+\.\d+ms$/);
    assert.match(log[2]!, /^POST \/ 405 \d+ /);
  });
});

test('listen reports a port in use as E_SERVER', async () => {
  const blocker = net.createServer();
  await new Promise<void>((r) => blocker.listen(0, '127.0.0.1', r));
  const port = (blocker.address() as net.AddressInfo).port;
  try {
    const s = createSiteServer(tree);
    await assert.rejects(listen(s, '127.0.0.1', port), (e) => (e as { code?: string }).code === 'E_SERVER' && /already in use/.test((e as Error).message));
  } finally {
    blocker.close();
  }
});

// ------------------------------------------------------------------ detached lifecycle (real processes)

const homes: string[] = [];
after(async () => {
  // never leave servers behind, even when an assertion failed
  for (const h of homes) {
    const r = await runCli(h, ['servers', '-a', '--json']);
    try {
      for (const s of JSON.parse(r.stdout) as Array<{ id: string }>) await runCli(h, ['server', 'rm', '-f', s.id]);
    } catch {
      /* nothing to clean */
    }
  }
});

async function poll<T>(fn: () => Promise<T | undefined>, ms = 8000): Promise<T> {
  for (const end = Date.now() + ms; ; ) {
    const v = await fn().catch(() => undefined);
    if (v !== undefined) return v;
    if (Date.now() > end) throw new Error('timed out');
    await new Promise((r) => setTimeout(r, 50));
  }
}

test('serve -d / servers / server logs|stop|restart|rm lifecycle with a real port', async () => {
  const home = await tmpDir();
  homes.push(home);
  const ctx = await makeSite();
  assert.equal((await runCli(home, ['build', ctx, '-t', 'life'])).code, 0);
  let r = await runCli(home, ['serve', '-d', '--name', 'web', '--spa', 'life']);
  assert.equal(r.code, 0, r.stderr);
  const m = /Started web \(([0-9a-f]{12})\) serving life at (http:\/\/127\.0\.0\.1:(\d+)\/)/.exec(r.stdout);
  assert.ok(m, r.stdout);
  const [, id, url, port] = m!;
  assert.equal((await fetch(url!)).status, 200);
  assert.match(await (await fetch(url! + 'about/')).text(), /About/);
  assert.match(await navigate(url! + 'client/route'), /Home/, 'spa fallback');

  r = await runCli(home, ['servers']);
  assert.match(r.stdout, /^NAME\s+SITE\s+URL\s+STATUS\s+ID/);
  assert.match(r.stdout, new RegExp(`web\\s+life\\s+${url!.replace(/[./]/g, '\\$&')}\\s+running\\s+${id}`));
  const listed = JSON.parse((await runCli(home, ['server', 'ls', '--json'])).stdout) as Array<{ site: string; image?: string }>;
  assert.equal(listed[0]!.site, 'life');
  assert.equal(listed[0]!.image, undefined);
  const ps = JSON.parse((await runCli(home, ['servers', '--json'])).stdout) as Array<{ id: string; name: string; pid: number; status: string; port: number; host: string }>;
  assert.equal(ps.length, 1);
  assert.equal(ps[0]!.status, 'running');
  assert.equal(ps[0]!.host, '127.0.0.1');
  assert.equal(ps[0]!.port, Number(port));
  const state = JSON.parse(await fs.readFile(path.join(home, 'servers', `${id}.json`), 'utf8'));
  assert.equal(state.name, 'web');
  assert.ok(state.logFile.endsWith(`${id}.log`));

  // names are unique
  r = await runCli(home, ['serve', '-d', '--name', 'web', 'life']);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /error\[E_SERVER\].*already exists/);

  // logs: one line per request
  r = await runCli(home, ['server', 'logs', 'web']);
  assert.match(r.stdout, /serving life/);
  assert.match(r.stdout, /GET \/about\/ 200 \d+ \d+\.\d+ms/);
  assert.match((await runCli(home, ['server', 'logs', '-n', '1', id!])).stdout, /^\S+ GET \/client\/route 200/);

  // rm refuses a running server
  r = await runCli(home, ['server', 'rm', 'web']);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /is running/);

  // restart keeps name and id
  r = await runCli(home, ['server', 'restart', 'web']);
  assert.equal(r.code, 0, r.stderr);
  const url2 = /at (http:\/\/\S+)/.exec(r.stdout)![1]!;
  assert.equal((await fetch(url2)).status, 200);

  r = await runCli(home, ['server', 'stop', 'web']);
  assert.equal(r.code, 0, r.stderr);
  await assert.rejects(fetch(url2), 'the server is gone after stop');
  assert.doesNotMatch((await runCli(home, ['servers'])).stdout, /web/);
  r = await runCli(home, ['servers', '-a']);
  assert.match(r.stdout, /web\s+life\s+-\s+exited/);
  assert.equal((await runCli(home, ['server', 'stop', 'web'])).code, 0, 'stopping an exited server is a no-op');

  r = await runCli(home, ['server', 'rm', 'web']);
  assert.equal(r.code, 0, r.stderr);
  assert.equal((await runCli(home, ['servers', '-a'])).stdout.trim().split('\n').length, 1, 'only the header is left');
  await assert.rejects(fs.stat(path.join(home, 'servers', `${id}.json`)));
  assert.equal((await runCli(home, ['server', 'stop', 'web'])).code, 1);
});

test('serve -d on a directory, auto names, rm -f of a running server, stale entries show as exited', async () => {
  const home = await tmpDir();
  homes.push(home);
  const dir = await tmpDir('sitefile-serve-');
  await fs.writeFile(path.join(dir, 'index.html'), 'dir site');
  await fs.writeFile(path.join(dir, '.secret'), 'hidden');
  await fs.mkdir(path.join(dir, '.git'));
  await fs.writeFile(path.join(dir, '.git', 'config'), 'x');
  const r = await runCli(home, ['serve', '-d', './'], dir);
  assert.equal(r.code, 0, r.stderr);
  const m = /Started (site-[0-9a-f]{6}) \(([0-9a-f]{12})\) serving \S+ at (http:\/\/\S+)/.exec(r.stdout)!;
  assert.ok(m, r.stdout);
  const url = m[3]!;
  assert.equal(await (await fetch(url)).text(), 'dir site');
  for (const p of ['.secret', '.git/config']) assert.equal((await fetch(url + p)).status, 404, p);
  // kill -9: the entry is stale and must show as exited
  const pid = (JSON.parse((await runCli(home, ['servers', '--json'])).stdout) as Array<{ pid: number }>)[0]!.pid;
  process.kill(pid, 'SIGKILL');
  await poll(async () => ((await runCli(home, ['servers', '--json'])).stdout.trim() === '[]' ? true : undefined));
  assert.match((await runCli(home, ['servers', '-a'])).stdout, /exited/);
  // restart revives it; rm -f stops and removes a running server
  assert.equal((await runCli(home, ['server', 'restart', m[1]!])).code, 0);
  const again = JSON.parse((await runCli(home, ['servers', '--json'])).stdout) as Array<{ url: string }>;
  assert.equal(again.length, 1);
  assert.equal((await fetch(again[0]!.url)).status, 200);
  assert.equal((await runCli(home, ['server', 'rm', '-f', m[2]!])).code, 0);
  await assert.rejects(fetch(again[0]!.url));
});

test('serve -d refuses a directory with a symlink that escapes it', async () => {
  const home = await tmpDir();
  homes.push(home);
  const dir = await tmpDir('sitefile-serve-');
  await fs.writeFile(path.join(dir, 'index.html'), 'x');
  await fs.symlink('/etc/passwd', path.join(dir, 'leak'));
  const r = await runCli(home, ['serve', '-d', './'], dir);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_PATH_ESCAPE/);
  assert.doesNotMatch((await runCli(home, ['servers', '-a'])).stdout, /site-/);
});

test('run: bad -p values, unknown images, registries, port in use, foreground --name', async () => {
  const home = await tmpDir();
  homes.push(home);
  const ctx = await makeSite();
  assert.equal((await runCli(home, ['build', ctx, '-t', 'p'])).code, 0);
  for (const bad of ['99999', 'a:b:c', 'example.com:80', '8080:']) assert.equal((await runCli(home, ['serve', '-d', '-p', bad, 'p'])).code, 2, bad);
  assert.equal((await runCli(home, ['serve', '-d', 'nope'])).code, 1);
  const reg = await runCli(home, ['serve', '-d', 'docker.io/x/y']);
  assert.equal(reg.code, 1);
  assert.match(reg.stderr, /no such image: docker.io\/x\/y/);
  assert.equal((await runCli(home, ['serve', '--name', 'x', 'p'])).code, 2, '--name needs -d');
  const blocker = net.createServer();
  await new Promise<void>((r) => blocker.listen(0, '127.0.0.1', r));
  const port = (blocker.address() as net.AddressInfo).port;
  try {
    const r = await runCli(home, ['serve', '-d', '-p', `127.0.0.1:${port}`, 'p']);
    assert.equal(r.code, 1);
    assert.match(r.stderr, /error\[E_SERVER\].*already in use/);
    assert.doesNotMatch((await runCli(home, ['servers', '-a'])).stdout, /site-/, 'a failed start leaves no entry behind');
  } finally {
    blocker.close();
  }
});

// ---------------------------------------------------------------- 404.html, clean URLs, _redirects, _headers

async function withTree(files: Record<string, string>, opts: { spa?: boolean; spaExclude?: string[]; spaFallback?: string; base?: string }, fn: (port: number) => Promise<void>): Promise<void> {
  const t = Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  const s = createSiteServer(t, opts);
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    await fn(port);
  } finally {
    await closeServer(s);
  }
}

const SITE = {
  '/index.html': 'home',
  '/404.html': 'custom 404',
  '/about.html': 'about page',
  '/docs/index.html': 'docs',
  '/new.html': 'new',
  '/app/index.html': 'app shell',
  '/assets/a.css': 'a{}',
  '/_redirects': [
    '# comments and blank lines are skipped',
    '',
    '/old /new 301',
    '/temp /new 302',
    '/blog/* https://blog.example/:splat',
    '/app/* /app/index.html 200',
    '/gone /404.html 404',
    '/about /elsewhere 301',
    '/docs /forced 308!',
    '/users/:id /u/:id 301',
    '/x /y 301 Country=nz',
    '/bad /bad\u0001 301',
  ].join('\n'),
  '/_headers': [
    '/assets/*',
    '  Cache-Control: public, max-age=60',
    '  Content-Length: 1',
    '/*',
    '  X-Frame-Options: DENY',
    '  X-Bad: a\u0001b',
    '  Cache-Control: immutable',
    'https://other/*',
    '  X-Other: 1',
  ].join('\n'),
};

test('serve: /404.html is the not-found page, with status 404', async () => {
  await withTree(SITE, {}, async (port) => {
    const r = await req(port, '/nope');
    assert.equal(r.status, 404);
    assert.equal(r.body, 'custom 404');
    assert.equal(r.headers['content-type'], 'text/html; charset=utf-8');
    assert.equal(r.headers.etag, undefined);
  });
  await withTree({ '/index.html': 'x' }, {}, async (port) => assert.match((await req(port, '/nope')).body, /404 Not Found/));
});

test('serve: clean URLs: /about serves /about.html; a directory still redirects to its slash form', async () => {
  await withTree(SITE, {}, async (port) => {
    const r = await req(port, '/about');
    assert.equal(r.status, 200);
    assert.equal(r.body, 'about page');
    assert.equal(r.headers['content-type'], 'text/html; charset=utf-8');
    assert.equal((await req(port, '/about.html')).body, 'about page');
    assert.equal((await req(port, '/about/')).headers.location, '/elsewhere', 'no file at /about/, so the rule applies');
    assert.equal((await req(port, '/docs?q=1')).status, 308, 'a forced rule wins over the directory');
  });
  await withTree({ '/docs/index.html': 'docs' }, {}, async (port) => {
    const d = await req(port, '/docs');
    assert.equal(d.status, 301);
    assert.equal(d.headers.location, '/docs/');
  });
  // johnhenry/sitefile#188: `/about/` with only `/about.html` redirects to the clean URL.
  await withTree({ '/about.html': 'about page', '/a b.html': 'space', '/x/index.html': 'x', '/x.html': 'x file' }, { spa: true }, async (port) => {
    const r = await req(port, '/about/?q=1');
    assert.equal(r.status, 301);
    assert.equal(r.headers.location, '/about?q=1');
    assert.equal(r.body, '');
    assert.equal((await req(port, '/a%20b/')).headers.location, '/a%20b');
    assert.equal((await req(port, '/about')).body, 'about page');
    assert.equal((await req(port, '/about.html')).body, 'about page', 'the .html spelling still answers');
    assert.equal((await req(port, '/x/')).body, 'x', 'an index file wins over the redirect');
    assert.equal((await req(port, '/x')).body, 'x file');
    assert.equal((await req(port, '/about.html/')).status, 404);
  });
});

test('serve: _redirects: exact paths, splats, rewrites, 404s, shadowing and force', async () => {
  await withTree(SITE, {}, async (port) => {
    let r = await req(port, '/old?a=1');
    assert.equal(r.status, 301);
    assert.equal(r.headers.location, '/new?a=1');
    assert.equal((await req(port, '/old/')).headers.location, '/new', 'a trailing slash matches too');
    assert.equal((await req(port, '/temp')).status, 302);
    r = await req(port, '/blog/2024/a%20b');
    assert.equal(r.status, 301);
    assert.equal(r.headers.location, 'https://blog.example/2024/a%20b');
    assert.equal((await req(port, '/blog')).headers.location, 'https://blog.example/');
    r = await req(port, '/app/settings/profile');
    assert.equal(r.status, 200);
    assert.equal(r.body, 'app shell');
    r = await req(port, '/gone');
    assert.equal(r.status, 404);
    assert.equal(r.body, 'custom 404');
    assert.equal((await req(port, '/about')).body, 'about page', 'a file shadows a rule without !');
    r = await req(port, '/users/a%20b');
    assert.equal(r.status, 301, 'a placeholder matches one segment');
    assert.equal(r.headers.location, '/u/a%20b');
    assert.equal((await req(port, '/users/1/2')).status, 404, 'but only one');
    assert.equal((await req(port, '/x')).status, 404, 'Country= needs --country');
    assert.equal((await req(port, '/bad')).status, 404, 'control characters drop the rule');
    assert.equal((await req(port, '/_redirects')).status, 404, 'the rules files are not served');
    assert.equal((await req(port, '/_headers')).status, 404);
  });
});

test('serve: _headers: glob patterns, joined values, protected names and invalid lines', async () => {
  await withTree(SITE, {}, async (port) => {
    let r = await req(port, '/assets/a.css');
    assert.equal(r.status, 200);
    assert.equal(r.headers['cache-control'], 'public, max-age=60, immutable');
    assert.equal(r.headers['x-frame-options'], 'DENY');
    assert.equal(r.headers['content-length'], '3');
    assert.equal(r.headers['x-bad'], undefined);
    assert.equal(r.headers['x-other'], undefined);
    r = await req(port, '/');
    assert.equal(r.headers['cache-control'], 'immutable');
    r = await req(port, '/nope');
    assert.equal(r.headers['x-frame-options'], 'DENY', 'the 404 page gets headers too');
    r = await req(port, '/old');
    assert.equal(r.headers['x-frame-options'], undefined, 'redirects do not');
    const etag = String((await req(port, '/assets/a.css')).headers.etag);
    r = await req(port, '/assets/a.css', 'GET', { 'if-none-match': etag });
    assert.equal(r.status, 304);
    assert.equal(r.headers['cache-control'], 'public, max-age=60, immutable');
  });
});

// johnhenry/sitefile#193
test('serve: _redirects: placeholders, query matching, conditions, 410/451, warnings and log notes', async () => {
  const files = {
    '/index.html': 'home',
    '/gone.html': 'gone',
    '/es/index.html': 'hola',
    '/docs/404.html': 'docs 404',
    '/docs/a/index.html': 'a',
    '/_redirects': [
      '/blog/:year/:slug /posts/:year/:slug 301',
      '/store id=:id /items/:id 302',
      '/search q=cats /cats 302',
      '/old-store id=:id /new?item=:id 301',
      '/lang / 302 Language=es,pt-BR',
      '/ab /b-variant 302 Cookie=ab_test',
      '/geo /nz 302! Country=nz,au',
      '/removed /gone.html 410',
      '/legal /gone.html 451',
      '/files/* /f/:splat/:nope 301',
      '/a/*/b /c',
      '/x /y 999',
      '/admin /secret 200! Role=admin',
      '/q bad=%zz /y',
      '/z /y 301 Planet=mars',
    ].join('\n'),
    '/_headers': '  X-Orphan: 1\n/*\n  ! X-Detach\n  Content-Length: 1\n  Bad Name: x\nnot-a-path\n',
  };
  const t = Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  assert.deepEqual(siteFileWarnings(t), [
    "_redirects:11: ignored: '*' is only supported as the last segment of the source",
    '_redirects:12: ignored: unsupported status (supported: 200, 301, 302, 303, 307, 308, 404, 410, 451)',
    '_redirects:13: ignored: Role= conditions need Netlify Identity and are not supported',
    '_redirects:14: ignored: bad query parameter (expected name=value or name=:placeholder)',
    '_redirects:15: ignored: unsupported condition (supported: Language=, Country=, Cookie=)',
    '_headers:1: ignored: a header line without a path line above it',
    "_headers:3: ignored: '! Name' detach lines are not supported",
    '_headers:4: ignored: the server sets this header itself',
    '_headers:5: ignored: invalid header name',
    "_headers:6: ignored: a path pattern must start with '/' (indent header lines)",
  ]);
  const log: string[] = [];
  const s = createSiteServer(t, { country: 'NZ', log: (l) => log.push(l) });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    let r = await req(port, '/blog/2024/hello');
    assert.equal(r.headers.location, '/posts/2024/hello');
    r = await req(port, '/store?id=7&x=1');
    assert.equal(r.status, 302);
    assert.equal(r.headers.location, '/items/7', 'a query-matching rule passes on only what its target names');
    assert.equal((await req(port, '/store')).status, 404, 'the parameter is required');
    assert.equal((await req(port, '/search?q=cats')).headers.location, '/cats');
    assert.equal((await req(port, '/search?q=dogs')).status, 404);
    assert.equal((await req(port, '/old-store?id=a%20b')).headers.location, '/new?item=a%20b');
    assert.equal((await req(port, '/lang', 'GET', { 'accept-language': 'fr;q=1, es-MX;q=0.8' })).status, 302);
    assert.equal((await req(port, '/lang', 'GET', { 'accept-language': 'pt-br' })).status, 302);
    assert.equal((await req(port, '/lang', 'GET', { 'accept-language': 'es;q=0, pt' })).status, 404, 'q=0 refuses, pt is not pt-BR');
    assert.equal((await req(port, '/ab', 'GET', { cookie: 'x=1; ab_test=b' })).status, 302);
    assert.equal((await req(port, '/ab', 'GET', { cookie: 'ab_test_2=1' })).status, 404);
    assert.equal((await req(port, '/geo')).headers.location, '/nz');
    r = await req(port, '/removed');
    assert.equal(r.status, 410);
    assert.equal(r.body, 'gone');
    assert.equal((await req(port, '/legal')).status, 451);
    assert.equal((await req(port, '/files/a/b%20c')).headers.location, '/f/a/b%20c/:nope', 'unknown names stay as written');
    r = await req(port, '/docs/a/missing');
    assert.equal(r.status, 404);
    assert.equal(r.body, 'docs 404', 'the nearest 404.html');
    assert.equal((await req(port, '/docs/missing/')).body, 'docs 404');
    assert.match((await req(port, '/elsewhere')).body, /404 Not Found/, 'no root 404.html: the built-in page');
  } finally {
    await closeServer(s);
  }
  assert.match(log[0]!, /^GET \/blog\/2024\/hello 301 0 [0-9.]+ms \u2192 \/posts\/2024\/hello \(_redirects:1\)$/);
  assert.ok(log.some((l) => l.endsWith(' \u2192 /gone.html (_redirects:8)')), log.join('\n'));
  assert.ok(log.some((l) => /^GET \/store 404 \d+ [0-9.]+ms$/.test(l)), 'no note when no rule applied');
  // Without --country, Country= never matches.
  await withTree(files, {}, async (port) => assert.equal((await req(port, '/geo')).status, 404));
});

test('serve --strict-site-files refuses a site with ignored lines; --country is checked', async () => {
  const dir = await tmpDir();
  await fs.writeFile(path.join(dir, 'index.html'), 'x');
  await fs.writeFile(path.join(dir, '_redirects'), '/a/*/b /c\n');
  const r = await runCli(dir, ['serve', '--strict-site-files', dir + '/']);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /warning: _redirects:1: ignored: '\*' is only supported as the last segment of the source\n/);
  assert.match(r.stderr, /error\[E_VALIDATION\]: 1 line of _redirects\/_headers is ignored \(--strict-site-files\)/);
  const bad = await runCli(dir, ['serve', '--country', 'nzl', dir + '/']);
  assert.equal(bad.code, 2);
  assert.match(bad.stderr, /--country expects a two-letter country code such as nz, got 'nzl'/);
});

// johnhenry/sitefile#195
test('serve --base: the site under a path prefix; / redirects there, outside is 404', async () => {
  assert.equal(normalizeBase('repo'), '/repo/');
  assert.equal(normalizeBase('/a b//c/'), '/a%20b/c/');
  assert.equal(normalizeBase('https://user.github.io/repo/?x'), '/repo/');
  assert.equal(normalizeBase('https://example.com'), '/');
  assert.equal(normalizeBase('/../x'), undefined);
  const files = { '/index.html': 'home', '/docs/index.html': 'docs', '/about.html': 'about', '/404.html': 'site 404', '/_redirects': '/old /about 301\n/ext https://e.x/ 302\n' };
  await withTree(files, { base: '/repo/' }, async (port) => {
    let r = await req(port, '/?q=1');
    assert.equal(r.status, 302);
    assert.equal(r.headers.location, '/repo/?q=1');
    r = await req(port, '/repo');
    assert.equal(r.status, 301);
    assert.equal(r.headers.location, '/repo/');
    assert.equal((await req(port, '/repo/')).body, 'home');
    assert.equal((await req(port, '/repo/about')).body, 'about');
    assert.equal((await req(port, '/repo/docs')).headers.location, '/repo/docs/');
    assert.equal((await req(port, '/repo/about/')).headers.location, '/repo/about');
    assert.equal((await req(port, '/repo/old')).headers.location, '/repo/about', 'path targets are under the base');
    assert.equal((await req(port, '/repo/ext')).headers.location, 'https://e.x/');
    r = await req(port, '/repo/missing');
    assert.equal(r.status, 404);
    assert.equal(r.body, 'site 404');
    r = await req(port, '/about');
    assert.equal(r.status, 404);
    assert.match(r.body, /404 Not Found/, 'outside the base: the built-in page');
    assert.equal((await req(port, '/repository/')).status, 404);
  });
});

const NAV = { 'sec-fetch-mode': 'navigate', 'sec-fetch-dest': 'document' };

test('live reload: 404.html is injected too, and a swapped tree brings its own _redirects', async () => {
  const mk = (files: Record<string, string>): Tree => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  let current = mk({ '/index.html': 'home', '/404.html': '<p>gone</p></body>', '/_redirects': '/a /b 302\n' });
  const s = createSiteServer(() => current, { liveReload: new LiveReload() });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    let r = await req(port, '/missing', 'GET', NAV);
    assert.equal(r.status, 404);
    assert.equal(r.body, `<p>gone</p>${RELOAD_SCRIPT}</body>`);
    assert.equal(r.headers['content-length'], String(Buffer.byteLength(r.body)));
    assert.equal((await req(port, '/a')).headers.location, '/b');
    current = mk({ '/index.html': 'v2', '/_redirects': '/a /c 301\n' });
    r = await req(port, '/a');
    assert.equal(r.status, 301);
    assert.equal(r.headers.location, '/c');
    assert.equal((await req(port, '/', 'GET', NAV)).body, `v2${RELOAD_SCRIPT}`);
  } finally {
    await closeServer(s);
  }
});

test('live reload: a _headers CSP that blocks the script or the event stream is warned about once per policy', async () => {
  const mk = (files: Record<string, string>): Tree => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  const site = mk({
    '/index.html': 'home',
    '/a.html': 'a',
    '/ok.html': 'ok',
    '/frag.html': '<li>x</li>',
    '/_headers': "/*\n  Content-Security-Policy: script-src 'nonce-x'\n/ok\n  X-Ok: 1\n/frag\n  X-Frag: 1\n",
  });
  const warnings: string[] = [];
  const s = createSiteServer(site, { liveReload: new LiveReload(), warn: (m) => warnings.push(m) });
  const { port } = await listen(s, '127.0.0.1', 0);
  try {
    await req(port, '/', 'GET', { 'sec-fetch-mode': 'cors' });
    assert.deepEqual(warnings, [], 'only navigations are checked');
    await req(port, '/', 'GET', NAV);
    await req(port, '/a', 'GET', NAV);
    assert.deepEqual(warnings, [
      "live reload: the Content-Security-Policy that _headers sets for /index.html does not allow scripts from 'self' (/__sitefile/reload.js), so open pages will not reload: script-src 'nonce-x'",
    ]);
  } finally {
    await closeServer(s);
  }
});
