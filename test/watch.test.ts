import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcess } from 'node:child_process';
import http from 'node:http';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { closeServer, CONSOLE_LIMIT, consoleLines, createSiteServer, isSameOriginPost, clientScript, RELOAD_EVENT, reloadEventFor, cspBlocksReload, injectReloadScript, isNavigation, listen, LiveReload, RELOAD_CLIENT, RELOAD_PATH, RELOAD_SCRIPT, RELOAD_SCRIPT_PATH } from '../src/server.js';
import { makeFile, Tree } from '../src/tree.js';
import { isIgnoredChange, pollSnapshot, RebuildScheduler } from '../src/watch.js';
import { pollOption } from '../src/cli/watch.js';
import { cliPath, makeSite, runCli, tmpDir } from './helpers/cli.js';

const enc = (s: string) => new TextEncoder().encode(s);
const dec = (b: Uint8Array) => new TextDecoder().decode(b);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
/** What a browser sends for a page navigation (only those get the reload script). */
const NAV = { 'sec-fetch-mode': 'navigate', 'sec-fetch-dest': 'document', accept: 'text/html,*/*;q=0.8' };

// ------------------------------------------------------------ units

test('RebuildScheduler debounces a burst into one build and coalesces changes during a build', async () => {
  const builds: string[][] = [];
  let release: (() => void) | undefined;
  const s = new RebuildScheduler(async (changed) => {
    builds.push([...changed].sort());
    if (builds.length === 1) await new Promise<void>((r) => (release = r));
  }, 30);
  s.notify('a');
  s.notify('b');
  s.notify('a');
  await sleep(80);
  assert.deepEqual(builds, [['a', 'b']], 'a burst is one build');
  // the first build is still running: these are coalesced into exactly one follow-up
  s.notify('c');
  s.notify('d');
  await sleep(60);
  assert.equal(builds.length, 1, 'nothing starts while a build runs');
  release?.();
  await sleep(100);
  assert.deepEqual(builds, [['a', 'b'], ['c', 'd']]);
  assert.equal(s.busy, false);
  s.close();
  s.notify('e');
  await sleep(60);
  assert.equal(builds.length, 2, 'closed: no more builds');
});

test('isIgnoredChange skips .git, exporter temp siblings and ignored paths', () => {
  const root = path.resolve('/ctx');
  const ignored = [path.join(root, 'dist'), path.resolve('/home/.sitefile')];
  const ig = (p: string) => isIgnoredChange(path.join(root, p), [root], ignored);
  assert.equal(ig('public/index.html'), false);
  assert.equal(ig('Sitefile'), false);
  assert.equal(ig('dist'), true);
  assert.equal(ig('dist/index.html'), true);
  assert.equal(ig('distillery/x'), false, 'prefix of a name is not inside it');
  assert.equal(ig('.git/index'), true);
  assert.equal(ig('sub/.git/HEAD'), true);
  assert.equal(ig('.dist.sitefile-tmp-0a1b2c/index.html'), true);
  assert.equal(ig('.dist.sitefile-old-ff'), true);
  assert.equal(isIgnoredChange(path.resolve('/home/.sitefile/cache/x'), [root], ignored), true);
});

test('pollSnapshot sees added, changed and removed files and skips ignored ones; pollOption', async () => {
  const dir = await tmpDir('sitefile-poll-');
  await fs.mkdir(path.join(dir, 'sub'));
  await fs.writeFile(path.join(dir, 'sub', 'a.txt'), 'a');
  await fs.mkdir(path.join(dir, '.git'));
  await fs.writeFile(path.join(dir, '.git', 'HEAD'), 'x');
  const missing = path.join(dir, '..', 'nope-' + path.basename(dir));
  const spec = { dirs: [dir], files: [missing], ignore: (p: string) => isIgnoredChange(p, [dir], []) };
  const one = await pollSnapshot(spec);
  assert.deepEqual([...one.keys()].sort(), [missing, path.join(dir, 'sub'), path.join(dir, 'sub', 'a.txt')].sort());
  assert.equal(one.get(missing), 'missing');
  await fs.writeFile(path.join(dir, 'sub', 'a.txt'), 'abc');
  await fs.writeFile(path.join(dir, 'b.txt'), 'b');
  const two = await pollSnapshot(spec);
  assert.notEqual(two.get(path.join(dir, 'sub', 'a.txt')), one.get(path.join(dir, 'sub', 'a.txt')));
  assert.ok(two.has(path.join(dir, 'b.txt')));
  await fs.rm(path.join(dir, 'b.txt'));
  assert.equal((await pollSnapshot(spec)).has(path.join(dir, 'b.txt')), false);

  assert.equal(pollOption(undefined, undefined), undefined);
  assert.equal(pollOption(true, undefined), 1000);
  assert.equal(pollOption(undefined, '250'), 250, 'an interval implies --poll');
  for (const bad of ['5', '1.5', 'x', '-1', '', '+50']) assert.throws(() => pollOption(true, bad), /--poll-interval expects a whole number of milliseconds \(10 or more\)/);
});

test('injectReloadScript inserts before the last </body>, else appends', () => {
  assert.equal(dec(injectReloadScript(enc('<p>a</p></BODY></html>'))), `<p>a</p>${RELOAD_SCRIPT}</BODY></html>`);
  assert.equal(dec(injectReloadScript(enc('<body></body><!-- </body> --></body>'))), `<body></body><!-- </body> -->${RELOAD_SCRIPT}</body>`);
  assert.equal(dec(injectReloadScript(enc('<h1>x</h1>'))), `<h1>x</h1>${RELOAD_SCRIPT}`);
  assert.equal(RELOAD_SCRIPT, `<script src="${RELOAD_SCRIPT_PATH}"></script>`);
});

test('clientScript passes the session options to the client', () => {
  assert.equal(clientScript(), `${RELOAD_CLIENT}({"resetServiceWorkers":true,"cssInject":true,"console":false});\n`);
  assert.equal(clientScript({ resetServiceWorkers: false }), `${RELOAD_CLIENT}({"resetServiceWorkers":false,"cssInject":true,"console":false});\n`);
  assert.equal(clientScript({ cssInject: false }), `${RELOAD_CLIENT}({"resetServiceWorkers":true,"cssInject":false,"console":false});\n`);
  assert.equal(new LiveReload({ resetServiceWorkers: false }).script, clientScript({ resetServiceWorkers: false }));
  // the client unregisters service workers and empties Cache Storage before reloading
  assert.match(RELOAD_CLIENT, /getRegistrations\(\)/);
  assert.match(RELOAD_CLIENT, /caches\.delete\(k\)/);
});

test('reloadEventFor: nothing, css (only changed stylesheets in both trees) or reload', () => {
  const t = (files: Record<string, string>) => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  const a = t({ '/index.html': 'i', '/a.css': 'a', '/z/b.css': 'b' });
  assert.equal(reloadEventFor(a, t({ '/index.html': 'i', '/a.css': 'a', '/z/b.css': 'b' }), true), undefined, 'no change, no event');
  assert.equal(reloadEventFor(a, t({ '/index.html': 'i', '/a.css': 'A', '/z/b.css': 'B' }), true), '{"type":"css","paths":["/a.css","/z/b.css"]}');
  assert.equal(reloadEventFor(a, t({ '/index.html': 'i', '/a.css': 'A', '/z/b.css': 'b' }), false), RELOAD_EVENT, '--no-css-inject');
  assert.equal(reloadEventFor(a, t({ '/index.html': 'I', '/a.css': 'A', '/z/b.css': 'b' }), true), RELOAD_EVENT, 'HTML changed too');
  assert.equal(reloadEventFor(a, t({ '/index.html': 'i', '/a.css': 'a', '/z/b.css': 'b', '/c.css': 'c' }), true), RELOAD_EVENT, 'an added stylesheet');
  assert.equal(reloadEventFor(a, t({ '/index.html': 'i', '/a.css': 'a' }), true), RELOAD_EVENT, 'a removed stylesheet');
  assert.equal(RELOAD_EVENT, '{"type":"reload"}');
});

test('consoleLines: levels, cleaning, location, stack and --console', () => {
  assert.deepEqual(consoleLines('{"level":"error","message":"Uncaught TypeError: x\\nis undefined","url":"http://h/app.js","line":3,"col":5}', false), [
    'browser error: Uncaught TypeError: x is undefined (http://h/app.js:3:5)',
  ]);
  assert.deepEqual(consoleLines('{"level":"warn","message":"w","url":"http://h/","line":2}', false), ['browser warn: w (http://h/:2)']);
  assert.deepEqual(consoleLines('{"level":"warn","message":"w","url":"http://h/","col":2}', false), ['browser warn: w (http://h/)'], 'col needs line');
  assert.deepEqual(consoleLines('{"level":"error","message":"m","line":1.5,"url":"u"}', false), ['browser error: m (u)']);
  assert.deepEqual(consoleLines('{"level":"log","message":"hi"}', false), [], 'log needs --console');
  assert.deepEqual(consoleLines('{"level":"info","message":"hi"}', true), ['browser info: hi']);
  assert.deepEqual(consoleLines('{"level":"error","message":"E: m","stack":"E: m\\n    at f (a.js:1:2)\\n\\n    at g (b.js:3:4)"}', false), [
    'browser error: E: m',
    '    at f (a.js:1:2)',
    '    at g (b.js:3:4)',
  ]);
  const long = consoleLines(JSON.stringify({ level: 'error', message: '\u{1F600}'.repeat(1200) }), false)!;
  assert.equal(long[0], `browser error: ${'\u{1F600}'.repeat(1000)}...`, 'cut at 1000 code points');
  const frames = consoleLines(JSON.stringify({ level: 'error', message: 'm', stack: Array.from({ length: 9 }, (_, i) => `at f${i}`).join('\n') }), false)!;
  assert.equal(frames.length, 6, 'at most 5 stack lines');
  for (const bad of ['', 'x', '[]', 'null', '{"level":"debug","message":"m"}', '{"message":"m"}']) assert.equal(consoleLines(bad, true), undefined, bad);
});

test('isSameOriginPost: Origin must be this Host; Sec-Fetch-Site same-origin when sent', () => {
  assert.equal(isSameOriginPost({ host: '127.0.0.1:8080', origin: 'http://127.0.0.1:8080' }), true);
  assert.equal(isSameOriginPost({ host: 'x.example', origin: 'https://x.example', 'sec-fetch-site': 'same-origin' }), true);
  assert.equal(isSameOriginPost({ host: '127.0.0.1:8080', origin: 'http://evil.example' }), false);
  assert.equal(isSameOriginPost({ host: '127.0.0.1:8080' }), false, 'no Origin');
  assert.equal(isSameOriginPost({ host: '127.0.0.1:8080', origin: 'http://127.0.0.1:8080', 'sec-fetch-site': 'cross-site' }), false);
  assert.equal(isSameOriginPost({ host: '127.0.0.1:8080', origin: 'null' }), false);
});

test('POST /__sitefile/console: same-origin only, size-capped, rate-limited, never access-logged; only under --watch', async () => {
  const tree = Tree.fromFiles([['/index.html', makeFile(enc('<h1>x</h1>'), 't')]]);
  const log: string[] = [];
  const server = createSiteServer(() => tree, { liveReload: new LiveReload(), log: (l) => log.push(l) });
  const plain = createSiteServer(tree, { log: () => undefined });
  const { port } = await listen(server, '127.0.0.1', 0);
  const { port: plainPort } = await listen(plain, '127.0.0.1', 0);
  const post = (p: number, body: string, headers: Record<string, string> = { origin: `http://127.0.0.1:${p}` }): Promise<number> =>
    new Promise((resolve, reject) => {
      const r = http.request({ host: '127.0.0.1', port: p, path: '/__sitefile/console', method: 'POST', headers: { 'content-type': 'text/plain', ...headers }, agent: false }, (res) => {
        res.resume();
        res.on('end', () => resolve(res.statusCode ?? 0));
      });
      r.on('error', reject);
      r.end(body);
    });
  try {
    assert.equal(await post(port, '{"level":"error","message":"Uncaught Error: boom","url":"http://127.0.0.1/app.js","line":1,"col":7}'), 204);
    assert.deepEqual(log, ['browser error: Uncaught Error: boom (http://127.0.0.1/app.js:1:7)'], 'the message, and no access-log line');
    assert.equal(await post(port, '{"level":"error","message":"x"}', { origin: 'http://evil.example' }), 403);
    assert.equal(await post(port, '{"level":"error","message":"x"}', {}), 403, 'no Origin');
    assert.equal(await post(port, 'not json'), 400);
    assert.equal(await post(port, JSON.stringify({ level: 'error', message: 'x'.repeat(20000) })), 413);
    assert.equal((await get(port, '/__sitefile/console')).status, 405);
    assert.equal(await post(plainPort, '{"level":"error","message":"x"}'), 405, 'only under --watch');
    log.length = 0;
    for (let i = 0; i < CONSOLE_LIMIT + 5; i++) await post(port, JSON.stringify({ level: 'warn', message: `w${i}` }));
    assert.equal(log.length, CONSOLE_LIMIT, 'rate-limited (the first message above was in this window too)');
    assert.match(log[log.length - 1]!, /^browser: more than 50 console messages in 10 s; dropping the rest$/);
  } finally {
    await closeServer(server);
    await closeServer(plain);
  }
});

test('isNavigation: Sec-Fetch-Mode, then Sec-Fetch-Dest, then Accept', () => {
  assert.equal(isNavigation({ 'sec-fetch-mode': 'navigate' }), true);
  assert.equal(isNavigation({ 'sec-fetch-mode': 'cors', 'sec-fetch-dest': 'document', accept: 'text/html' }), false, 'fetch() of HTML');
  assert.equal(isNavigation({ 'sec-fetch-mode': 'same-origin', accept: 'text/html' }), false);
  assert.equal(isNavigation({ 'sec-fetch-dest': 'document' }), true);
  assert.equal(isNavigation({ 'sec-fetch-dest': 'iframe' }), true);
  assert.equal(isNavigation({ 'sec-fetch-dest': 'empty', accept: 'text/html' }), false);
  assert.equal(isNavigation({ accept: 'Text/HTML,application/xhtml+xml' }), true, 'older browsers: Accept');
  assert.equal(isNavigation({ accept: '*/*' }), false, 'fetch() default, curl');
  assert.equal(isNavigation({}), false);
});

test('cspBlocksReload: script-src, connect-src, default-src fallbacks and several policies', () => {
  assert.equal(cspBlocksReload("script-src 'self'"), undefined);
  assert.equal(cspBlocksReload("default-src 'self'"), undefined);
  assert.equal(cspBlocksReload("default-src 'none'; script-src 'self'"), 'connect-src');
  assert.equal(cspBlocksReload("default-src 'none'; script-src 'self'; connect-src 'self'"), undefined);
  assert.equal(cspBlocksReload("script-src 'nonce-abc'"), 'script-src');
  assert.equal(cspBlocksReload("script-src 'self' 'strict-dynamic'"), 'script-src', "'strict-dynamic' ignores 'self'");
  assert.equal(cspBlocksReload("script-src-elem 'none'; script-src 'self'"), 'script-src', 'script-src-elem wins');
  assert.equal(cspBlocksReload("SCRIPT-SRC 'SELF'; script-src 'none'"), undefined, 'case-insensitive, the first directive counts');
  assert.equal(cspBlocksReload('script-src https:; connect-src *'), undefined);
  assert.equal(cspBlocksReload("img-src 'none'"), undefined);
  assert.equal(cspBlocksReload("script-src 'self', script-src 'none'"), 'script-src', 'each policy is enforced');
});

interface Res {
  status: number;
  headers: http.IncomingHttpHeaders;
  body: string;
}
function get(port: number, p: string, headers: Record<string, string> = {}): Promise<Res> {
  return new Promise((resolve, reject) => {
    http
      .get({ host: '127.0.0.1', port, path: p, headers, agent: false }, (res) => {
        const chunks: Buffer[] = [];
        res.on('data', (c: Buffer) => chunks.push(c));
        res.on('end', () => resolve({ status: res.statusCode ?? 0, headers: res.headers, body: Buffer.concat(chunks).toString() }));
      })
      .on('error', reject);
  });
}

/** Open the event stream; `next()` resolves with the next complete event. */
function events(port: number, through?: { proxyPort: number }): Promise<{ next: () => Promise<string>; close: () => void; headers: http.IncomingHttpHeaders }> {
  return new Promise((resolve, reject) => {
    const target = through ? { port: through.proxyPort } : { port };
    const req = http.get({ host: '127.0.0.1', ...target, path: RELOAD_PATH, agent: false }, (res) => {
      let buf = '';
      const waiters: Array<(e: string) => void> = [];
      const queue: string[] = [];
      res.setEncoding('utf8');
      res.on('data', (c: string) => {
        buf += c;
        let i;
        while ((i = buf.indexOf('\n\n')) !== -1) {
          const ev = buf.slice(0, i);
          buf = buf.slice(i + 2);
          const w = waiters.shift();
          if (w) w(ev);
          else queue.push(ev);
        }
      });
      resolve({
        headers: res.headers,
        next: () => {
          const q = queue.shift();
          return q !== undefined ? Promise.resolve(q) : new Promise((r) => waiters.push(r));
        },
        close: () => req.destroy(),
      });
    });
    req.on('error', reject);
  });
}

test('live reload: HTML gets the script (own ETag), other files and non-watch servers do not; reload() reaches open pages', async () => {
  let tree = Tree.fromFiles([
    ['/index.html', makeFile(enc('<html><body><h1>v1</h1></body></html>'), 't')],
    ['/style.css', makeFile(enc('a{}'), 't')],
  ]);
  const live = new LiveReload();
  const log: string[] = [];
  const server = createSiteServer(() => tree, { liveReload: live, log: (l) => log.push(l) });
  const plain = createSiteServer(tree);
  const { port } = await listen(server, '127.0.0.1', 0);
  const { port: plainPort } = await listen(plain, '127.0.0.1', 0);
  try {
    let r = await get(port, '/', NAV);
    assert.equal(r.body, `<html><body><h1>v1</h1>${RELOAD_SCRIPT}</body></html>`);
    assert.equal(Number(r.headers['content-length']), Buffer.byteLength(r.body));
    assert.match(String(r.headers.etag), /-live"$/);
    assert.equal((await get(port, '/', { ...NAV, 'if-none-match': String(r.headers.etag) })).status, 304);
    assert.equal((await get(port, '/style.css', NAV)).body, 'a{}');
    // fetch('/') from a page script: not a navigation, served as the file is
    const f = await get(port, '/', { 'sec-fetch-mode': 'cors', 'sec-fetch-dest': 'empty', accept: '*/*' });
    assert.equal(f.body, '<html><body><h1>v1</h1></body></html>', 'a fetch() of HTML is untouched');
    assert.doesNotMatch(String(f.headers.etag), /-live"$/);
    const js = await get(port, RELOAD_SCRIPT_PATH);
    assert.equal(js.status, 200);
    assert.equal(js.body, clientScript());
    assert.ok(js.body.startsWith(RELOAD_CLIENT));
    assert.ok(js.body.endsWith('({"resetServiceWorkers":true,"cssInject":true,"console":false});\n'));
    assert.equal(js.headers['content-type'], 'text/javascript; charset=utf-8');
    assert.equal(js.headers['cache-control'], 'no-cache');
    assert.equal(Number(js.headers['content-length']), Buffer.byteLength(js.body));
    const p = await get(plainPort, '/', NAV);
    assert.equal(p.body, '<html><body><h1>v1</h1></body></html>', 'no injection without --watch');
    assert.equal((await get(plainPort, RELOAD_PATH)).status, 404, 'the path is only reserved under --watch');
    assert.equal((await get(plainPort, RELOAD_SCRIPT_PATH)).status, 404);

    const ev = await events(port);
    assert.equal(ev.headers['content-type'], 'text/event-stream');
    assert.equal(await ev.next(), 'retry: 1000');
    tree = Tree.fromFiles([['/index.html', makeFile(enc('<h1>v2</h1>'), 't')]]);
    live.reload();
    assert.equal(await ev.next(), 'data: {"type":"reload"}');
    r = await get(port, '/', NAV);
    assert.equal(r.body, `<h1>v2</h1>${RELOAD_SCRIPT}`, 'the swapped tree is served');
    assert.equal(log.filter((l) => l.includes('/__sitefile/')).length, 0, 'the event stream and the script are not access-logged');
    ev.close();
  } finally {
    await closeServer(server);
    await closeServer(plain);
  }
});

/**
 * A reverse proxy that behaves like nginx's default: it buffers each upstream response until it
 * ends, unless the upstream says `X-Accel-Buffering: no`.
 */
async function bufferingProxy(upstreamPort: number): Promise<{ port: number; close: () => Promise<void> }> {
  const proxy = http.createServer((req, res) => {
    const up = http.request({ host: '127.0.0.1', port: upstreamPort, path: req.url, method: req.method, headers: req.headers }, (r) => {
      const streaming = String(r.headers['x-accel-buffering']).toLowerCase() === 'no';
      if (streaming) {
        res.writeHead(r.statusCode ?? 502, r.headers);
        res.flushHeaders();
        r.pipe(res);
        return;
      }
      const chunks: Buffer[] = [];
      r.on('data', (c: Buffer) => chunks.push(c));
      r.on('end', () => res.writeHead(r.statusCode ?? 502, r.headers).end(Buffer.concat(chunks)));
    });
    up.on('error', () => res.destroy());
    req.pipe(up);
  });
  await new Promise<void>((r) => proxy.listen(0, '127.0.0.1', r));
  return {
    port: (proxy.address() as { port: number }).port,
    close: () => {
      proxy.closeAllConnections();
      return new Promise((r) => proxy.close(() => r()));
    },
  };
}

test('live reload through a buffering reverse proxy: unbuffered, untransformed stream with a heartbeat', async () => {
  const tree = Tree.fromFiles([['/index.html', makeFile(enc('<h1>v1</h1>'), 't')]]);
  const live = new LiveReload({}, 50);
  const server = createSiteServer(() => tree, { liveReload: live });
  const { port } = await listen(server, '127.0.0.1', 0);
  const proxy = await bufferingProxy(port);
  try {
    const ev = await events(port, { proxyPort: proxy.port });
    assert.equal(ev.headers['x-accel-buffering'], 'no');
    assert.equal(ev.headers['cache-control'], 'no-cache, no-transform');
    assert.equal(await ev.next(), 'retry: 1000');
    assert.equal(await ev.next(), ': ping', 'a comment keeps the idle connection alive');
    live.reload();
    let e = await ev.next();
    while (e === ': ping') e = await ev.next();
    assert.equal(e, 'data: {"type":"reload"}', 'the event arrives through the proxy before the stream ends');
    ev.close();
    const head = await new Promise<http.IncomingHttpHeaders>((resolve, reject) =>
      http.request({ host: '127.0.0.1', port, path: RELOAD_PATH, method: 'HEAD', agent: false }, (r) => resolve(r.headers)).on('error', reject).end(),
    );
    assert.equal(head['x-accel-buffering'], 'no');
    assert.equal(head['cache-control'], 'no-cache, no-transform');
    // the client reconnects with backoff when an answer is not an event stream, and finds the
    // stream relative to its own URL (another public origin or a base path)
    assert.match(RELOAD_CLIENT, /new URL\("reload", base\)/);
    assert.match(RELOAD_CLIENT, /delay = Math\.min\(delay \* 2, 30000\)/);
  } finally {
    await proxy.close();
    await closeServer(server);
  }
});

// ------------------------------------------------------------ CLI

interface Proc {
  child: ChildProcess;
  stdout: () => string;
  stderr: () => string;
  /** Wait until stdout+stderr match `re` `count` times. */
  waitFor: (re: RegExp, count?: number, ms?: number) => Promise<void>;
  stop: () => Promise<number>;
}

function startCli(home: string, args: string[]): Proc {
  const child = spawn(process.execPath, [cliPath, ...args], { env: { ...process.env, SITEFILE_HOME: home }, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  let err = '';
  child.stdout?.on('data', (c: Buffer) => (out += c.toString()));
  child.stderr?.on('data', (c: Buffer) => (err += c.toString()));
  const exited = new Promise<number>((r) => child.once('exit', (code) => r(code ?? -1)));
  return {
    child,
    stdout: () => out,
    stderr: () => err,
    async waitFor(re, count = 1, ms = 15000) {
      const g = new RegExp(re.source, re.flags.includes('g') ? re.flags : re.flags + 'g');
      const end = Date.now() + ms;
      while (((out + err).match(g)?.length ?? 0) < count) {
        if (Date.now() > end) throw new Error(`timed out waiting for ${re} x${count}\n--- stdout\n${out}\n--- stderr\n${err}`);
        await sleep(25);
      }
    },
    async stop() {
      if (child.exitCode === null) child.kill('SIGINT');
      return exited;
    },
  };
}

const count = (s: string, re: RegExp) => s.match(new RegExp(re.source, 'gm'))?.length ?? 0;

test('build --watch: rebuilds on context, Sitefile and --build-arg-file changes; a failed rebuild keeps the last good output', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite(
    { 'templates/index.html': '<title>${TITLE}</title>' },
    'ARG TITLE = "Home"\nCOPY ./public/ /\nRENDER ./templates/index.html TO /title.html\nEXPORT /\n',
  );
  const args = path.join(site, 'args.env');
  await fs.writeFile(args, 'TITLE=One\n');
  const w = startCli(home, ['build', '--watch', site, '--build-arg-file', args, '--no-cache']);
  try {
    await w.waitFor(/Watching .* for changes \(Ctrl-C to stop\)/);
    assert.match(w.stdout(), /^Exported 4 files to /);
    const index = () => fs.readFile(path.join(site, 'dist', 'title.html'), 'utf8');
    assert.equal(await index(), '<title>One</title>');

    // a context file
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:1px}');
    await w.waitFor(/^Exported /m, 2);
    assert.equal(await fs.readFile(path.join(site, 'dist', 'style.css'), 'utf8'), 'body{margin:1px}');

    // a --build-arg-file
    await fs.writeFile(args, 'TITLE=Two\n');
    await w.waitFor(/^Exported /m, 3);
    assert.equal(await index(), '<title>Two</title>');

    // a broken Sitefile: the error is printed and dist/ is left as it was
    const sitefile = path.join(site, 'Sitefile');
    const good = await fs.readFile(sitefile, 'utf8');
    await fs.writeFile(sitefile, good + 'BOGUS instruction\n');
    await w.waitFor(/Build failed; keeping the last good output/);
    assert.match(w.stderr(), /error\[E_[A-Z_]+\]: .*Sitefile:5:1/);
    assert.equal(await index(), '<title>Two</title>');
    assert.deepEqual((await fs.readdir(site)).filter((n) => n.includes('sitefile-')), [], 'no temp siblings left');

    // fixed again
    await fs.writeFile(sitefile, good.replace('COPY', '# changed\nCOPY'));
    await w.waitFor(/^Exported /m, 4);

    // the build's own writes (dist/, temp dirs) never trigger another build
    await sleep(600);
    assert.equal(count(w.stdout(), /^Exported /m), 4, 'no rebuild loop');
    assert.equal(count(w.stdout(), /^Change detected, rebuilding$/m), 4);
    // .git is ignored
    await fs.mkdir(path.join(site, '.git'), { recursive: true });
    await fs.writeFile(path.join(site, '.git', 'HEAD'), 'ref: x\n');
    await sleep(500);
    assert.equal(count(w.stdout(), /^Exported /m), 4, '.git changes are ignored');
  } finally {
    assert.equal(await w.stop(), 0, `exit code after Ctrl-C\n${w.stderr()}`);
  }
});

test('build --watch: a burst of changes is one rebuild', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite();
  const w = startCli(home, ['build', '--watch', site]);
  try {
    await w.waitFor(/Watching /);
    for (let i = 0; i < 5; i++) await fs.writeFile(path.join(site, 'public', `f${i}.txt`), String(i));
    await w.waitFor(/^Exported 8 files/m);
    await sleep(500);
    assert.equal(count(w.stdout(), /^Exported /m), 2, w.stdout());
  } finally {
    await w.stop();
  }
});

test('build --watch: usage errors', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite();
  let r = await runCli(home, ['validate', '--watch', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /^error: --watch is only for build \(and serve --watch\), not validate\n/);
  r = await runCli(home, ['build', '--watch', '-t', 'x', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /^error: --watch cannot be combined with -t\n/);
  r = await runCli(home, ['build', '--watch', path.join(site, 'nope')]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /^error\[E_NOT_FOUND\]: '.*nope' is not a directory\n/);
  r = await runCli(home, ['build', '--watch', site, '--build-arg', '=x']);
  assert.equal(r.code, 2, 'a usage error in the first build ends the watch');
  assert.match(r.stderr, /^error: --build-arg expects NAME=value, got '=x'\n/);
  r = await runCli(home, ['serve', '--watch', '-d', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--watch cannot be combined with -d/);
  r = await runCli(home, ['serve', '--build-arg', 'A=1', './x/']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--build-arg needs --watch/);
  r = await runCli(home, ['serve', '--keep-service-workers', './x/']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--keep-service-workers needs --watch/);
});

test('serve --watch: serves the build, live-reloads after a good rebuild, keeps serving on failure', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite({ 'public/index.html': '<html><body><h1>v1</h1></body></html>' });
  const w = startCli(home, ['serve', '--watch', site]);
  try {
    await w.waitFor(/Watching .* for changes/);
    assert.match(w.stdout(), /^Built 3 files \(sha256:[0-9a-f]{64}\)\nServing .* \(3 files\) at http:\/\/127\.0\.0\.1:(\d+)\/ {2}\(Ctrl-C to stop\)\n/);
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(w.stdout())?.[1]);
    let r = await get(port, '/', NAV);
    assert.equal(r.body, `<html><body><h1>v1</h1>${RELOAD_SCRIPT}</body></html>`);
    assert.equal((await get(port, '/style.css', NAV)).body, 'body{margin:0}', 'no script outside HTML');
    assert.equal((await get(port, RELOAD_SCRIPT_PATH)).body, clientScript());

    const ev = await events(port);
    assert.equal(await ev.next(), 'retry: 1000');
    await fs.writeFile(path.join(site, 'public', 'index.html'), '<h1>v2</h1>');
    assert.equal(await ev.next(), 'data: {"type":"reload"}');
    r = await get(port, '/', NAV);
    assert.equal(r.body, `<h1>v2</h1>${RELOAD_SCRIPT}`);
    // only a stylesheet of the build changed: pages swap it in place
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:1px}');
    assert.equal(await ev.next(), 'data: {"type":"css","paths":["/style.css"]}');

    // a failed rebuild: error printed, no reload, the last good build still served
    await fs.writeFile(path.join(site, 'Sitefile'), 'FROM scratch\nCOPY ./missing/ /\nEXPORT /\n');
    await w.waitFor(/Build failed; keeping the last good output/);
    assert.match(w.stderr(), /error\[E_/);
    assert.equal((await get(port, '/', NAV)).body, `<h1>v2</h1>${RELOAD_SCRIPT}`);
    ev.close();
    assert.deepEqual(await fs.readdir(site).then((n) => n.filter((x) => x === 'dist')), [], 'serve --watch writes no export');
  } finally {
    assert.equal(await w.stop(), 0);
  }
});

test('build --watch --poll-interval: polling sees changes; --poll needs --watch', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite({});
  const w = startCli(home, ['build', '--watch', '--poll-interval', '50', site]);
  try {
    await w.waitFor(/Watching /);
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:2px}');
    await w.waitFor(/^Exported /m, 2);
    assert.equal(await fs.readFile(path.join(site, 'dist', 'style.css'), 'utf8'), 'body{margin:2px}');
    await fs.writeFile(path.join(site, 'public', 'new.txt'), 'new');
    await w.waitFor(/^Exported /m, 3);
    assert.equal(await fs.readFile(path.join(site, 'dist', 'new.txt'), 'utf8'), 'new');
  } finally {
    assert.equal(await w.stop(), 0);
  }
  let r = await runCli(home, ['build', '--poll', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--poll needs --watch/);
  r = await runCli(home, ['build', '--watch', '--poll-interval', 'soon', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--poll-interval expects a whole number of milliseconds \(10 or more\), got 'soon'/);
  r = await runCli(home, ['serve', '--poll', './x/']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--poll needs --watch/);
});

test('serve --watch on a directory without a Sitefile: served as it is, re-read on change, CSS swapped', async () => {
  const home = await tmpDir('sitefile-home-');
  const dir = await tmpDir('sitefile-plain-');
  await fs.writeFile(path.join(dir, 'index.html'), '<link rel="stylesheet" href="/s.css"><h1>p1</h1>');
  await fs.writeFile(path.join(dir, 's.css'), 'a{}');
  await fs.writeFile(path.join(dir, '.env'), 'SECRET=1');
  const w = startCli(home, ['serve', '--watch', dir]);
  try {
    await w.waitFor(/Watching .* for changes/);
    assert.match(w.stdout(), /^Read 2 files \(sha256:[0-9a-f]{64}\)\nServing .* \(2 files\) at /);
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(w.stdout())?.[1]);
    assert.equal((await get(port, '/', NAV)).body, `<link rel="stylesheet" href="/s.css"><h1>p1</h1>${RELOAD_SCRIPT}`);
    assert.equal((await get(port, '/.env')).status, 404, 'hidden files are not served');
    const ev = await events(port);
    assert.equal(await ev.next(), 'retry: 1000');
    await fs.writeFile(path.join(dir, 's.css'), 'a{color:red}');
    assert.equal(await ev.next(), 'data: {"type":"css","paths":["/s.css"]}');
    assert.equal((await get(port, '/s.css')).body, 'a{color:red}');
    await fs.writeFile(path.join(dir, 'index.html'), '<h1>p2</h1>');
    assert.equal(await ev.next(), 'data: {"type":"reload"}');
    await w.waitFor(/^Change detected, re-reading$/m, 2);
    assert.equal((await get(port, '/', NAV)).body, `<h1>p2</h1>${RELOAD_SCRIPT}`);
    ev.close();
    assert.deepEqual((await fs.readdir(dir)).sort(), ['.env', 'index.html', 's.css'], 'nothing is written to the directory');
  } finally {
    assert.equal(await w.stop(), 0);
  }
});

test('serve --watch --no-css-inject: a stylesheet-only rebuild reloads', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite({ 'public/index.html': '<h1>x</h1>' });
  const w = startCli(home, ['serve', '--watch', '--no-css-inject', site]);
  try {
    await w.waitFor(/Watching /);
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(w.stdout())?.[1]);
    assert.match((await get(port, '/__sitefile/reload.js')).body, /"cssInject":false\}\);\n$/);
    const ev = await events(port);
    assert.equal(await ev.next(), 'retry: 1000');
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:3px}');
    assert.equal(await ev.next(), 'data: {"type":"reload"}');
    ev.close();
  } finally {
    assert.equal(await w.stop(), 0);
  }
});

test('build --watch: a failing first build keeps watching and recovers', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite({}, 'FROM scratch\nCOPY ./nope/ /\nEXPORT /\n');
  const w = startCli(home, ['build', '--watch', site]);
  try {
    await w.waitFor(/Watching /);
    assert.match(w.stderr(), /^error\[E_/);
    await fs.writeFile(path.join(site, 'Sitefile'), 'FROM scratch\nCOPY ./public/ /\nEXPORT /\n');
    await w.waitFor(/^Exported 3 files/m);
  } finally {
    assert.equal(await w.stop(), 0);
  }
});

test('build --watch rebuilds incrementally (the --profile trace shows reused steps); SITEFILE_NO_INCREMENTAL=1 turns it off', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite({ 'templates/t.html': '<p>${TITLE}</p>' }, 'ARG TITLE = "T"\nCOPY ./public/ /\nRENDER ./templates/t.html TO /t.html\nEXPORT /\n');
  const trace = path.join(home, 'trace.json');
  const memo = async (): Promise<unknown[]> =>
    (JSON.parse(await fs.readFile(trace, 'utf8')) as { traceEvents: Array<{ cat?: string; args: Record<string, unknown> }> }).traceEvents
      .filter((e) => e.cat === 'operation')
      .map((e) => e.args['memo']);
  const w = startCli(home, ['build', '--watch', site, '--profile', trace]);
  try {
    await w.waitFor(/Watching /);
    assert.deepEqual(await memo(), ['miss', 'miss']);
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:2px}');
    await w.waitFor(/^Exported /m, 2);
    // COPY read a changed file; RENDER read nothing that changed, so its file is reused.
    // (The trace is written just after the Exported line.)
    const end = Date.now() + 10000;
    while (JSON.stringify(await memo().catch(() => [])) !== '["miss","files"]' && Date.now() < end) await sleep(25);
    assert.deepEqual(await memo(), ['miss', 'files']);
    assert.equal(await fs.readFile(path.join(site, 'dist', 't.html'), 'utf8'), '<p>T</p>');
    assert.equal(await fs.readFile(path.join(site, 'dist', 'style.css'), 'utf8'), 'body{margin:2px}');
  } finally {
    await w.stop();
  }
  const off = spawn(process.execPath, [cliPath, 'build', '--watch', site, '--profile', trace], {
    env: { ...process.env, SITEFILE_HOME: home, SITEFILE_NO_INCREMENTAL: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let out = '';
  off.stdout.on('data', (c: Buffer) => (out += c.toString()));
  try {
    const end = Date.now() + 15000;
    while (!/Watching /.test(out) && Date.now() < end) await sleep(25);
    assert.deepEqual(await memo(), [undefined, undefined]);
  } finally {
    off.kill('SIGINT');
  }
});
