import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import net from 'node:net';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { closeServer, createSiteServer, listen, safeSegments } from '../src/server.js';
import { makeFile, Tree } from '../src/tree.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

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

test('SPA fallback serves /index.html for unknown extension-less paths only', async () => {
  await withServer({ spa: true }, async (port) => {
    const r = await req(port, '/some/client/route');
    assert.equal(r.status, 200);
    assert.equal(r.body, '<h1>home</h1>');
    assert.equal((await req(port, '/missing.js')).status, 404, 'missing assets stay 404');
    assert.equal((await req(port, '/app.js')).body, 'console.log(1)');
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
  assert.match(await (await fetch(url! + 'client/route')).text(), /Home/, 'spa fallback');

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
