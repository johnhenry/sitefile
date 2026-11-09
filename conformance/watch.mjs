// Watch-mode conformance (conformance/run.mjs): run `serve --watch` with both CLIs on the same
// context, drive each through the same script of HTTP requests, file edits and event-stream
// reads, and require the same transcript: status, the headers in SHOWN, body bytes, the events
// (heartbeat comments skipped) and the `warning:` lines on stderr.
//
// A case: { name, files: { 'rel/path': 'content' }, args?: [...], steps: [...] }. Steps:
//   { get: '/path', headers?: {...}, method? }  record one response
//   { post: '/path', body, headers? }           record one response to a POST (SELF in a header: the server's origin)
//   { log: true }                               record the `browser ...` lines of stdout so far
//   { open: true }                              open the event stream (records its headers and first event)
//   { next: true }                              record the events since the last step: `building` events are
//                                               left out, and `ok` events too unless they are all there is
//                                               (a file system may report one save twice: the second rebuild
//                                               changes nothing and sends ok)
//   { write: { 'rel/path': 'content' } }        edit files in the context, then wait until a rebuild has finished
//                                               and the server has been quiet for a moment
//   { wait: ms }                                sleep
//   { next: true, ms: 1000 }                    the same, waiting at most ms for an event ('(timeout)' when none came)
//   { open: true, lastEventId: 'x' }            open it with `Last-Event-ID: x` ('previous': the instance ID the
//                                               last stream started with; a different one makes the server send reload)
//   { restart: true }                           `server restart` (detached cases), then wait until it answers
//   { open: true, client: 'id', as: 'a' }       open another stream, as page `id` (`?client=id`, --sync) under
//                                               the name `a`; `{ next: true, of: 'a' }` reads it
//   { log: 'client' }                           record the lines that start with `client ` instead
//   { get: '/path', mask: true }                record the body with ISO times as <ts>
// A case with `detach: true` runs `serve -d --watch` (named, on a free port, so a restart keeps the
// URL) and reads the server's log instead of the output; it is removed at the end.
// `watch: false` leaves out --watch (`serve [-d] --reload ...`: pages reload after a restart);
// `prebuild: [...]` runs `build <context> ...` first, and `source` is the operand to serve
// instead of the context (an image built by prebuild with -t, say).
// Every stream starts with `id: <instance>` (random): it is not recorded.
// Each implementation gets its own copy of the context and its own SITEFILE_HOME.

import { spawn } from 'node:child_process';
import { promises as fs, readFileSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';

const SHOWN = ['content-type', 'content-length', 'cache-control', 'etag', 'location', 'content-security-policy', 'x-accel-buffering', 'clear-site-data', 'vary'];
const NAV = { 'sec-fetch-mode': 'navigate', 'sec-fetch-dest': 'document', accept: 'text/html,*/*;q=0.8' };
const FETCH = { 'sec-fetch-mode': 'cors', 'sec-fetch-dest': 'empty', accept: '*/*' };
const SITEFILE = 'FROM scratch\nCOPY ./public/ /\nEXPORT /\n';

export const WATCH_CASES = [
  {
    name: 'watch/reload-script',
    files: {
      Sitefile: SITEFILE,
      'public/index.html': '<html><body><h1>v1</h1></body></html>',
      'public/frag.html': '<li>fragment</li>',
      'public/style.css': 'a{}',
    },
    steps: [
      { get: '/', headers: NAV },
      { get: '/', headers: FETCH },
      { get: '/frag.html', headers: FETCH },
      { get: '/frag.html', headers: { accept: 'text/html' } },
      { get: '/frag.html' },
      { get: '/style.css', headers: NAV },
      { get: '/__sitefile/reload.js' },
      { get: '/__sitefile/reload.js', method: 'HEAD' },
      { open: true },
      { write: { 'public/index.html': '<h1>v2</h1>' } },
      { next: true },
      { get: '/', headers: NAV },
    ],
  },
  {
    name: 'watch/csp',
    files: {
      Sitefile: SITEFILE,
      'public/index.html': '<h1>self</h1>',
      'public/strict/index.html': '<h1>strict</h1>',
      'public/noconnect/index.html': '<h1>no connect</h1>',
      'public/_headers':
        "/\n  Content-Security-Policy: script-src 'self'\n/strict/*\n  Content-Security-Policy: script-src 'nonce-abc'\n/noconnect/*\n  Content-Security-Policy: default-src 'none'; script-src 'self'\n",
    },
    steps: [
      { get: '/', headers: NAV },
      { get: '/strict/', headers: NAV },
      { get: '/strict/', headers: NAV },
      { get: '/noconnect/', headers: NAV },
    ],
  },
  {
    name: 'watch/keep-service-workers',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>sw</h1>', 'public/sw.js': 'self.addEventListener("fetch", () => {});' },
    args: ['--keep-service-workers'],
    steps: [{ get: '/__sitefile/reload.js' }, { get: '/sw.js' }],
  },
  {
    name: 'watch/plain-dir',
    files: { 'index.html': '<link rel="stylesheet" href="/s.css"><h1>p1</h1>', 's.css': 'a{}', '.env': 'SECRET=1', '.well-known/x.txt': 'x' },
    steps: [
      { get: '/', headers: NAV },
      { get: '/.env' },
      { get: '/.well-known/x.txt' },
      { open: true },
      { write: { 's.css': 'a{color:red}' } },
      { next: true },
      { get: '/s.css' },
      { write: { 'index.html': '<h1>p2</h1>' } },
      { next: true },
      { get: '/', headers: NAV },
    ],
  },
  {
    name: 'watch/css-inject-build',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>x</h1>', 'public/a.css': 'a{}', 'public/b.css': 'b{}' },
    steps: [{ open: true }, { write: { 'public/a.css': 'a{color:red}' } }, { next: true }, { write: { 'public/index.html': '<h1>y</h1>' } }, { next: true }],
  },
  {
    name: 'watch/no-css-inject',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>x</h1>', 'public/a.css': 'a{}' },
    args: ['--no-css-inject'],
    steps: [{ get: '/__sitefile/reload.js' }, { open: true }, { write: { 'public/a.css': 'a{color:red}' } }, { next: true }],
  },
  {
    name: 'watch/console',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>x</h1>' },
    args: ['--console'],
    steps: [
      { get: '/__sitefile/reload.js' },
      { post: '/__sitefile/console', headers: { origin: 'SELF' }, body: '{"level":"error","message":"Uncaught Error: boom\\nnext","url":"http://x/app.js","line":3,"col":9,"stack":"Error: boom\\n    at f (app.js:3:9)"}' },
      { post: '/__sitefile/console', headers: { origin: 'SELF' }, body: '{"level":"log","message":"hello","line":2.5}' },
      { post: '/__sitefile/console', headers: { origin: 'http://evil.example' }, body: '{"level":"error","message":"x"}' },
      { post: '/__sitefile/console', headers: {}, body: '{"level":"error","message":"x"}' },
      { post: '/__sitefile/console', headers: { origin: 'SELF' }, body: '{"level":"trace"}' },
      { post: '/__sitefile/console', headers: { origin: 'SELF' }, body: 'x'.repeat(17000) },
      { get: '/__sitefile/console' },
      { wait: 200 },
      { log: true },
    ],
  },
  {
    name: 'watch/build-error',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>x</h1>' },
    steps: [
      { open: true },
      { write: { Sitefile: 'FROM scratch\nCOPY ./missing/ /\nEXPORT /\n' } },
      { next: true },
      { get: '/', headers: NAV },
      { write: { Sitefile: SITEFILE } },
      { next: true },
      { write: { 'public/index.html': '<h1>y</h1>' } },
      { next: true },
    ],
  },
  {
    name: 'watch/navigate-to-changed',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>i</h1>', 'public/posts/a b/index.html': '<h1>a</h1>', 'public/x.html': '<h1>x</h1>' },
    args: ['--navigate-to-changed'],
    steps: [
      { open: true },
      { write: { 'public/posts/a b/index.html': '<h1>A</h1>' } },
      { next: true },
      { write: { 'public/x.html': '<h1>X</h1>', 'public/index.html': '<h1>I</h1>' } },
      { next: true },
    ],
  },
  {
    name: 'watch/morph',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>i</h1>', 'public/b.html': '<h1>b</h1>', 'public/app.js': '1' },
    steps: [
      { open: true },
      { write: { 'public/index.html': '<h1>I</h1>' } },
      { next: true },
      { write: { 'public/b.html': '<h1>B</h1>', 'public/app.js': '2' } },
      { next: true },
    ],
  },
  {
    name: 'watch/detached-dir',
    detach: true,
    files: { 'index.html': '<link rel="stylesheet" href="/s.css"><h1>d1</h1>', 's.css': 'a{}' },
    steps: [
      { get: '/', headers: NAV },
      { get: '/__sitefile/reload.js' },
      { open: true },
      { write: { 's.css': 'a{color:red}' } },
      { next: true },
      { write: { 'index.html': '<h1>d2</h1>' } },
      { next: true },
      { get: '/', headers: NAV },
      { open: true, lastEventId: 'previous' },
      { next: true, ms: 1000 },
      { restart: true },
      { open: true, lastEventId: 'previous' },
      { next: true },
      { get: '/', headers: NAV },
    ],
  },
  {
    name: 'watch/detached-poll',
    detach: true,
    args: ['--poll-interval', '50'],
    files: { 'index.html': '<h1>p1</h1>' },
    steps: [{ open: true }, { write: { 'index.html': '<h1>p2</h1>' } }, { next: true }, { get: '/', headers: NAV }],
  },
  {
    name: 'watch/sync',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>x</h1>' },
    args: ['--sync=scroll'],
    steps: [
      { get: '/__sitefile/reload.js' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/"}' },
      { open: true, client: 'aaaaaaaaaaaaaaaa', as: 'a' },
      { open: true, client: 'bbbbbbbbbbbbbbbb', as: 'b' },
      { open: true, client: 'not-an-id', as: 'c' },
      { get: '/__sitefile/clients', mask: true },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/","title":" Home\\n page "}' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/docs/#top","title":"Docs","nav":true}' },
      { next: true, of: 'b' },
      { next: true, of: 'c' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"bbbbbbbbbbbbbbbb","url":"/docs/","scroll":[0,5000.0]}' },
      { next: true, of: 'a' },
      { next: true, of: 'a', ms: 300 },
      { next: true, of: 'b', ms: 300 },
      { get: '/__sitefile/clients', mask: true },
      { get: '/__sitefile/clients', method: 'HEAD' },
      { get: '/__sitefile/clients', headers: { origin: 'http://127.0.0.1' } },
      { get: '/__sitefile/clients', headers: { 'sec-fetch-mode': 'cors' } },
      { post: '/__sitefile/clients', headers: { origin: 'SELF' }, body: '' },
      { post: '/__sitefile/client', headers: { origin: 'http://evil.example' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/"}' },
      { post: '/__sitefile/client', headers: {}, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/"}' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"//evil.example/"}' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/","scroll":[0.5,1]}' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: 'x'.repeat(5000) },
      { get: '/__sitefile/client' },
      { wait: 200 },
      { log: 'client' },
    ],
  },
  {
    name: 'watch/no-sync',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>x</h1>' },
    steps: [
      { get: '/__sitefile/clients' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"aaaaaaaaaaaaaaaa","url":"/"}' },
      { open: true, client: 'aaaaaaaaaaaaaaaa', as: 'a' },
      { get: '/__sitefile/clients' },
    ],
  },
  {
    name: 'watch/detached-sync',
    detach: true,
    args: ['--sync', '--log-format', 'json'],
    files: { 'index.html': '<h1>d</h1>' },
    steps: [
      { get: '/__sitefile/reload.js' },
      { open: true, client: 'cccccccccccccccc', as: 'a' },
      { post: '/__sitefile/client', headers: { origin: 'SELF' }, body: '{"id":"cccccccccccccccc","url":"/","title":"D","scroll":[0,1]}' },
      { get: '/__sitefile/clients', mask: true },
      { wait: 200 },
      { log: '{"type":"client"' },
      { restart: true },
      { get: '/__sitefile/reload.js' },
    ],
  },
  {
    // johnhenry/sitefile#317: --reload without --watch, on an image: the script and the stream, and
    // reload after a restart. --reload full is kept in the record.
    name: 'reload/detached-image',
    detach: true,
    watch: false,
    prebuild: ['-t', 'conf-reload:1'],
    source: 'conf-reload:1',
    args: ['--reload', 'full'],
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>img</h1>', 'public/frag.html': '<li>f</li>' },
    steps: [
      { get: '/', headers: NAV },
      { get: '/frag.html', headers: FETCH },
      { get: '/__sitefile/reload.js' },
      { open: true },
      { open: true, lastEventId: 'previous' },
      { next: true, ms: 1000 },
      { restart: true },
      { get: '/__sitefile/reload.js' },
      { open: true, lastEventId: 'previous' },
      { next: true },
      { get: '/', headers: NAV },
    ],
  },
  {
    // A bare --reload (before the operand) in the foreground, on a plain directory: morph, a page from
    // an earlier instance reloads, and the CSP warning.
    name: 'reload/foreground',
    watch: false,
    source: 'public',
    args: ['--reload'],
    files: {
      'public/index.html': '<h1>fg</h1>',
      'public/strict/index.html': '<h1>strict</h1>',
      'public/_headers': "/strict/*\n  Content-Security-Policy: script-src 'nonce-abc'\n",
    },
    steps: [
      { get: '/', headers: NAV },
      { get: '/strict/', headers: NAV },
      { get: '/__sitefile/reload.js' },
      { open: true, lastEventId: '0123456789abcdef' },
      { next: true },
    ],
  },
  {
    name: 'watch/reload-full',
    files: { Sitefile: SITEFILE, 'public/index.html': '<h1>i</h1>' },
    args: ['--reload', 'full'],
    steps: [{ get: '/__sitefile/reload.js' }],
  },
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A port that was free a moment ago (a detached case binds it, so `server restart` keeps the URL). */
function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

function request(port, { method = 'GET', path: p, headers = {}, body }) {
  return new Promise((resolve) => {
    const req = http.request({ host: '127.0.0.1', port, path: p, method, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const shown = {};
        for (const k of SHOWN) if (res.headers[k] !== undefined) shown[k] = res.headers[k];
        resolve({ status: res.statusCode, headers: shown, body: Buffer.concat(chunks).toString('latin1') });
      });
    });
    req.on('error', (e) => resolve({ error: e.message }));
    req.end(body);
  });
}

function openEvents(port, lastEventId, client) {
  return new Promise((resolve) => {
    const headers = lastEventId === undefined ? {} : { 'last-event-id': lastEventId };
    const p = client === undefined ? '/__sitefile/reload' : `/__sitefile/reload?client=${client}`;
    const req = http.get({ host: '127.0.0.1', port, path: p, headers, agent: false }, (res) => {
      let buf = '';
      const queue = [];
      const waiters = [];
      const stream = { id: undefined };
      res.setEncoding('utf8');
      res.on('data', (c) => {
        buf += c;
        let i;
        while ((i = buf.indexOf('\n\n')) !== -1) {
          const ev = buf.slice(0, i);
          buf = buf.slice(i + 2);
          if (ev.startsWith(':')) continue; // heartbeat
          if (/^id: [0-9a-f]{16}$/.test(ev)) {
            stream.id = ev.slice(4); // the server instance: random, so kept out of the transcript
            continue;
          }
          const w = waiters.shift();
          if (w) w(ev);
          else queue.push(ev);
        }
      });
      const shown = {};
      for (const k of SHOWN) if (res.headers[k] !== undefined) shown[k] = res.headers[k];
      resolve({
        stream,
        status: res.statusCode,
        headers: shown,
        next: (ms = 15000) =>
          queue.length
            ? Promise.resolve(queue.shift())
            : new Promise((r) => {
                const t = setTimeout(() => r('(timeout)'), ms);
                waiters.push((e) => {
                  clearTimeout(t);
                  r(e);
                });
              }),
        /** Every event received and not yet taken. */
        drain: () => queue.splice(0),
        close: () => req.destroy(),
      });
    });
    req.on('error', () => resolve(undefined));
  });
}

async function runOne(c, cmd, work, home) {
  await fs.rm(work, { recursive: true, force: true });
  for (const [rel, content] of Object.entries(c.files)) {
    await fs.mkdir(path.dirname(path.join(work, rel)), { recursive: true });
    await fs.writeFile(path.join(work, rel), content);
  }
  const env = { ...process.env, SITEFILE_HOME: home };
  const watch = c.watch !== false;
  if (c.prebuild) await new Promise((r) => spawn(cmd[0], [...cmd.slice(1), 'build', work, '--no-lockfile', ...c.prebuild], { env, stdio: 'ignore' }).once('exit', r));
  const operand = c.source === undefined ? work : c.prebuild ? c.source : path.join(work, c.source);
  const fixedPort = c.detach ? await freePort() : 0;
  const child = c.detach
    ? spawn(cmd[0], [...cmd.slice(1), 'serve', '-d', ...(watch ? ['--watch'] : []), '--name', 'conf-watch', '-p', `127.0.0.1:${fixedPort}`, ...(c.args ?? []), operand], { env, stdio: ['ignore', 'pipe', 'pipe'] })
    : spawn(cmd[0], [...cmd.slice(1), 'serve', ...(watch ? ['--watch', '--no-lockfile'] : []), ...(c.args ?? []), operand], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (d) => (stdout += d));
  child.stderr.on('data', (d) => (stderr += d));
  const transcript = [];
  let events;
  let lastId;
  /** Streams opened `as` a name (pages of `--sync` cases). */
  const named = new Map();
  /** A detached server's log (its lines start with a time stamp). */
  let logFile;
  const output = () => {
    if (!logFile) return stdout + stderr;
    try {
      return readFileSync(logFile, 'utf8').replace(/^\S+ /gm, '');
    } catch {
      return '';
    }
  };
  // Builds and re-reads that have finished (their last line).
  const finished = () => output().match(/^(Built |Read |Build failed|Read failed)/gm)?.length ?? 0;
  /** Wait until no build finishes for `ms`. */
  const quiet = async (ms = 500) => {
    let n = finished();
    for (let still = 0; still < ms; still += 50) {
      await sleep(50);
      if (finished() !== n) {
        n = finished();
        still = 0;
      }
    }
  };
  try {
    const deadline = Date.now() + 30000;
    if (c.detach) {
      await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
      const id = /^Started conf-watch \(([0-9a-f]{12})\)/m.exec(stdout)?.[1];
      if (child.exitCode !== 0 || id === undefined) return { transcript: [`did not start: ${stderr.trim()}`], stderr };
      logFile = path.join(home, 'servers', `${id}.log`);
    } else {
      while (!(watch ? /Watching / : /^Serving /m).test(stdout)) {
        if (Date.now() > deadline || child.exitCode !== null) return { transcript: [`did not start: ${stderr.trim()}`], stderr };
        await sleep(20);
      }
    }
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(stdout)?.[1]);
    for (const s of c.steps) {
      if (s.get !== undefined) {
        const r = await request(port, { method: s.method, path: s.get, headers: s.headers });
        if (s.mask && r.body !== undefined) r.body = r.body.replace(/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z/g, '<ts>');
        transcript.push({ step: `${s.method ?? 'GET'} ${s.get}`, ...r });
      }
      else if (s.post !== undefined) {
        // SELF in a header value is this server's origin.
        const headers = Object.fromEntries(Object.entries(s.headers ?? {}).map(([k, v]) => [k, v.replace('SELF', `http://127.0.0.1:${port}`)]));
        transcript.push({ step: `POST ${s.post}`, ...(await request(port, { method: 'POST', path: s.post, headers, body: s.body })) });
      } else if (s.log) {
        const prefix = s.log === true ? 'browser' : s.log;
        // JSON lines carry a time stamp: kept out of the transcript.
        const lines = output().split('\n').filter((l) => l.startsWith(prefix)).map((l) => l.replace(/"ts":"[^"]*"/, '"ts":"<ts>"'));
        transcript.push({ step: 'log', lines });
      } else if (s.open && s.as !== undefined) {
        named.get(s.as)?.close();
        const ev = await openEvents(port, undefined, s.client);
        named.set(s.as, ev);
        transcript.push({ step: `open ${s.as}`, status: ev?.status, headers: ev?.headers, event: await ev?.next() });
      } else if (s.next && s.of !== undefined) {
        const ev = named.get(s.of);
        let got = ev?.drain() ?? [];
        if (got.length === 0 && ev) got = [await ev.next(s.ms)];
        transcript.push({ step: `next ${s.of}`, events: got });
      } else if (s.open) {
        await quiet();
        events?.close();
        events = await openEvents(port, s.lastEventId === 'previous' ? lastId : s.lastEventId);
        const event = await events?.next();
        lastId = events?.stream.id;
        transcript.push({ step: 'open', ...(s.lastEventId !== undefined ? { lastEventId: s.lastEventId } : {}), status: events?.status, headers: events?.headers, event });
      } else if (s.restart) {
        events?.close();
        events = undefined;
        const code = await new Promise((r) => spawn(cmd[0], [...cmd.slice(1), 'server', 'restart', 'conf-watch'], { env, stdio: 'ignore' }).once('exit', r));
        transcript.push({ step: 'restart', code });
      } else if (s.next) {
        let got = events?.drain() ?? [];
        if (got.length === 0 && events) got = [await events.next(s.ms)];
        const OK = 'data: {"type":"ok"}';
        const shown = got.filter((e) => e !== 'data: {"type":"building"}');
        const rest = shown.filter((e) => e !== OK);
        transcript.push({ step: 'next', events: rest.length > 0 ? rest : shown.length > 0 ? [OK] : [] });
      } else if (s.write) {
        const before = finished();
        for (const [rel, content] of Object.entries(s.write)) await fs.writeFile(path.join(work, rel), content);
        const deadline = Date.now() + 20000;
        while (finished() === before && Date.now() < deadline) await sleep(25);
        await quiet();
      } else if (s.wait) await sleep(s.wait);
    }
  } finally {
    events?.close();
    for (const ev of named.values()) ev?.close();
    child.kill('SIGINT');
    await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
    if (c.detach) await new Promise((r) => spawn(cmd[0], [...cmd.slice(1), 'server', 'rm', '-f', 'conf-watch'], { env, stdio: 'ignore' }).once('exit', r));
  }
  const warnings = (c.detach ? output() : stderr).split('\n').filter((l) => l.startsWith('warning:')).map((l) => l.replaceAll(work, '<ctx>'));
  transcript.push({ step: 'warnings', warnings });
  return { transcript };
}

/** Run every watch case with both implementations; returns run.mjs result rows. */
export async function runWatchCases({ impls, tmp, only }) {
  const rows = [];
  for (const c of WATCH_CASES) {
    if (only && !c.name.includes(only)) continue;
    const got = {};
    for (const [impl, cmd] of impls) {
      const dir = path.join(tmp, impl, c.name.replace(/[/:]/g, '_'));
      const r = await runOne(c, cmd, path.join(dir, 'ctx'), path.join(tmp, impl, '.home'));
      // Error locations name each implementation's own copy of the context.
      const own = `/${impl}/${path.basename(dir)}/`;
      got[impl] = { transcript: JSON.parse(JSON.stringify(r.transcript).replaceAll(own, `/<impl>/${path.basename(dir)}/`)) };
    }
    const detail = [];
    const a = got.ts.transcript;
    const b = got.rust.transcript;
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      const x = JSON.stringify(a[i]);
      const y = JSON.stringify(b[i]);
      if (x !== y) {
        detail.push(`step ${i + 1} differs:\nts   ${x}\nrust ${y}`);
        break;
      }
    }
    rows.push({ name: c.name, compare: 'watch', pass: detail.length === 0, detail, transcript: a });
  }
  return rows;
}
