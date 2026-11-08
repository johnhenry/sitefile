import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcess } from 'node:child_process';
import http from 'node:http';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { closeServer, createSiteServer, clientScript, cspBlocksReload, injectReloadScript, isNavigation, listen, LiveReload, RELOAD_CLIENT, RELOAD_PATH, RELOAD_SCRIPT, RELOAD_SCRIPT_PATH } from '../src/server.js';
import { makeFile, Tree } from '../src/tree.js';
import { isIgnoredChange, RebuildScheduler } from '../src/watch.js';
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

test('injectReloadScript inserts before the last </body>, else appends', () => {
  assert.equal(dec(injectReloadScript(enc('<p>a</p></BODY></html>'))), `<p>a</p>${RELOAD_SCRIPT}</BODY></html>`);
  assert.equal(dec(injectReloadScript(enc('<body></body><!-- </body> --></body>'))), `<body></body><!-- </body> -->${RELOAD_SCRIPT}</body>`);
  assert.equal(dec(injectReloadScript(enc('<h1>x</h1>'))), `<h1>x</h1>${RELOAD_SCRIPT}`);
  assert.equal(RELOAD_SCRIPT, `<script src="${RELOAD_SCRIPT_PATH}"></script>`);
});

test('clientScript passes the session options to the client', () => {
  assert.equal(clientScript(), `${RELOAD_CLIENT}({"resetServiceWorkers":true});\n`);
  assert.equal(clientScript({ resetServiceWorkers: false }), `${RELOAD_CLIENT}({"resetServiceWorkers":false});\n`);
  assert.equal(new LiveReload({ resetServiceWorkers: false }).script, clientScript({ resetServiceWorkers: false }));
  // the client unregisters service workers and empties Cache Storage before reloading
  assert.match(RELOAD_CLIENT, /getRegistrations\(\)/);
  assert.match(RELOAD_CLIENT, /caches\.delete\(k\)/);
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
function events(port: number): Promise<{ next: () => Promise<string>; close: () => void; headers: http.IncomingHttpHeaders }> {
  return new Promise((resolve, reject) => {
    const req = http.get({ host: '127.0.0.1', port, path: RELOAD_PATH, agent: false }, (res) => {
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
    assert.ok(js.body.endsWith('({"resetServiceWorkers":true});\n'));
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
    assert.equal(await ev.next(), 'data: reload');
    r = await get(port, '/', NAV);
    assert.equal(r.body, `<h1>v2</h1>${RELOAD_SCRIPT}`, 'the swapped tree is served');
    assert.equal(log.filter((l) => l.includes('/__sitefile/')).length, 0, 'the event stream and the script are not access-logged');
    ev.close();
  } finally {
    await closeServer(server);
    await closeServer(plain);
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
    assert.equal(await ev.next(), 'data: reload');
    r = await get(port, '/', NAV);
    assert.equal(r.body, `<h1>v2</h1>${RELOAD_SCRIPT}`);

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
