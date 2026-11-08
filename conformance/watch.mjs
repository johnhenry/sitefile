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
//   { next: true }                              record the next event
//   { write: { 'rel/path': 'content' } }        edit files in the context (then wait for the rebuild via next)
//   { wait: ms }                                sleep
// Each implementation gets its own copy of the context and its own SITEFILE_HOME.

import { spawn } from 'node:child_process';
import { promises as fs } from 'node:fs';
import http from 'node:http';
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
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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

function openEvents(port) {
  return new Promise((resolve) => {
    const req = http.get({ host: '127.0.0.1', port, path: '/__sitefile/reload', agent: false }, (res) => {
      let buf = '';
      const queue = [];
      const waiters = [];
      res.setEncoding('utf8');
      res.on('data', (c) => {
        buf += c;
        let i;
        while ((i = buf.indexOf('\n\n')) !== -1) {
          const ev = buf.slice(0, i);
          buf = buf.slice(i + 2);
          if (ev.startsWith(':')) continue; // heartbeat
          const w = waiters.shift();
          if (w) w(ev);
          else queue.push(ev);
        }
      });
      const shown = {};
      for (const k of SHOWN) if (res.headers[k] !== undefined) shown[k] = res.headers[k];
      resolve({
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
  const child = spawn(cmd[0], [...cmd.slice(1), 'serve', '--watch', '--no-lockfile', ...(c.args ?? []), work], {
    env: { ...process.env, SITEFILE_HOME: home },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (d) => (stdout += d));
  child.stderr.on('data', (d) => (stderr += d));
  const transcript = [];
  let events;
  try {
    const deadline = Date.now() + 30000;
    while (!/Watching /.test(stdout)) {
      if (Date.now() > deadline || child.exitCode !== null) return { transcript: [`did not start: ${stderr.trim()}`], stderr };
      await sleep(20);
    }
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(stdout)?.[1]);
    for (const s of c.steps) {
      if (s.get !== undefined) transcript.push({ step: `${s.method ?? 'GET'} ${s.get}`, ...(await request(port, { method: s.method, path: s.get, headers: s.headers })) });
      else if (s.post !== undefined) {
        // SELF in a header value is this server's origin.
        const headers = Object.fromEntries(Object.entries(s.headers ?? {}).map(([k, v]) => [k, v.replace('SELF', `http://127.0.0.1:${port}`)]));
        transcript.push({ step: `POST ${s.post}`, ...(await request(port, { method: 'POST', path: s.post, headers, body: s.body })) });
      } else if (s.log) transcript.push({ step: 'log', lines: stdout.split('\n').filter((l) => l.startsWith('browser')) });
      else if (s.open) {
        events = await openEvents(port);
        transcript.push({ step: 'open', status: events?.status, headers: events?.headers, event: await events?.next() });
      } else if (s.next) transcript.push({ step: 'next', event: await events?.next() });
      else if (s.write) {
        for (const [rel, content] of Object.entries(s.write)) await fs.writeFile(path.join(work, rel), content);
      } else if (s.wait) await sleep(s.wait);
    }
  } finally {
    events?.close();
    child.kill('SIGINT');
    await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
  }
  const warnings = stderr.split('\n').filter((l) => l.startsWith('warning:')).map((l) => l.replaceAll(work, '<ctx>'));
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
      got[impl] = await runOne(c, cmd, path.join(dir, 'ctx'), path.join(tmp, impl, '.home'));
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
