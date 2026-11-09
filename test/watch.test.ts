import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcess } from 'node:child_process';
import http from 'node:http';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { CLIENT_REPORT_LIMIT, clientLogLine, isOperatorRequest, parseClientReport, streamClientId } from '../src/server.js';
import { BUILDING_EVENT, closeServer, pageUrlFor, CONSOLE_LIMIT, errorEvent, OK_EVENT, consoleLines, createSiteServer, isSameOriginPost, clientScript, RELOAD_EVENT, reloadEventFor, cspBlocksReload, injectReloadScript, isNavigation, listen, LiveReload, RELOAD_CLIENT, RELOAD_PATH, RELOAD_SCRIPT, RELOAD_SCRIPT_PATH } from '../src/server.js';
import { makeFile, Tree } from '../src/tree.js';
import { SitefileError } from '../src/errors.js';
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
  assert.equal(clientScript(), `${RELOAD_CLIENT}({"resetServiceWorkers":true,"cssInject":true,"console":false,"overlay":true,"navigateToChanged":false,"reload":"morph"});\n`);
  assert.equal(clientScript({ resetServiceWorkers: false }), `${RELOAD_CLIENT}({"resetServiceWorkers":false,"cssInject":true,"console":false,"overlay":true,"navigateToChanged":false,"reload":"morph"});\n`);
  assert.equal(clientScript({ cssInject: false }), `${RELOAD_CLIENT}({"resetServiceWorkers":true,"cssInject":false,"console":false,"overlay":true,"navigateToChanged":false,"reload":"morph"});\n`);
  assert.equal(new LiveReload({ resetServiceWorkers: false }).script, clientScript({ resetServiceWorkers: false }));
  // the client unregisters service workers and empties Cache Storage before reloading
  assert.match(RELOAD_CLIENT, /getRegistrations\(\)/);
  assert.match(RELOAD_CLIENT, /caches\.delete\(k\)/);
});

test('errorEvent: code, message and the Sitefile location', () => {
  assert.equal(
    errorEvent(new SitefileError('E_SYNTAX', 'unknown instruction "X"', { file: '/c/Sitefile', line: 3, column: 1 })),
    '{"type":"error","code":"E_SYNTAX","message":"unknown instruction \\"X\\"","file":"/c/Sitefile","line":3,"column":1}',
  );
  assert.equal(errorEvent(new SitefileError('E_NOT_FOUND', 'no such file')), '{"type":"error","code":"E_NOT_FOUND","message":"no such file"}');
  assert.equal(BUILDING_EVENT, '{"type":"building"}');
  assert.equal(OK_EVENT, '{"type":"ok"}');
  assert.match(RELOAD_CLIENT, /attachShadow/, 'the overlay is in a shadow root');
  assert.equal(clientScript({ overlay: false }).endsWith('"overlay":false,"navigateToChanged":false,"reload":"morph"});\n'), true);
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

test('--sync: the client script option, reports, stream IDs, the operator check and log lines', () => {
  assert.ok(clientScript({ sync: 'nav' }).endsWith('"reload":"morph","sync":"nav"});\n'));
  assert.ok(clientScript({ sync: 'scroll' }).endsWith('"reload":"morph","sync":"scroll"});\n'));
  assert.ok(clientScript().endsWith('"reload":"morph"});\n'), 'nothing without --sync');
  const id = '0123456789abcdef';
  assert.deepEqual(parseClientReport(`{"id":"${id}","url":"/docs/#a","title":" Docs\\n  page ","nav":true}`), { id, url: '/docs/#a', title: 'Docs   page', nav: true });
  assert.deepEqual(parseClientReport(`{"id":"${id}","url":"/","scroll":[0,10000]}`), { id, url: '/', title: '', nav: false, scroll: [0, 10000] });
  assert.equal(parseClientReport(`{"id":"${id}","url":"/","title":"${'x'.repeat(300)}"}`)?.title, `${'x'.repeat(200)}...`);
  for (const bad of [
    '',
    'x',
    '[]',
    `{"url":"/"}`,
    `{"id":"0123456789ABCDEF","url":"/"}`,
    `{"id":"${id}"}`,
    `{"id":"${id}","url":"//evil.example/"}`,
    `{"id":"${id}","url":"https://x/"}`,
    `{"id":"${id}","url":"/a b"}`,
    `{"id":"${id}","url":"/${'a'.repeat(2048)}"}`,
    `{"id":"${id}","url":"/","nav":1}`,
    `{"id":"${id}","url":"/","title":5}`,
    `{"id":"${id}","url":"/","scroll":[1]}`,
    `{"id":"${id}","url":"/","scroll":[0.5,1]}`,
    `{"id":"${id}","url":"/","scroll":[0,10001]}`,
  ]) {
    assert.equal(parseClientReport(bad), undefined, bad);
  }
  assert.equal(streamClientId(`?client=${id}`), id);
  assert.equal(streamClientId(`?x=1&client=${id}&client=fedcba9876543210`), id, 'the first');
  assert.equal(streamClientId('?client=nothex'), undefined);
  assert.equal(streamClientId(''), undefined);
  assert.equal(isOperatorRequest('127.0.0.1', {}), true);
  assert.equal(isOperatorRequest('::ffff:127.0.0.1', { 'user-agent': 'curl' }), true);
  assert.equal(isOperatorRequest('192.168.1.2', {}), false, 'not loopback');
  assert.equal(isOperatorRequest('127.0.0.1', { origin: 'http://127.0.0.1:1' }), false, 'a page');
  assert.equal(isOperatorRequest('127.0.0.1', { 'sec-fetch-site': 'same-origin' }), false, 'a page');
  assert.equal(isOperatorRequest('127.0.0.1', { 'sec-fetch-mode': 'navigate' }), false, 'a browser tab');
  const e = { ts: '2026-01-02T03:04:05.006Z', event: 'navigate' as const, id, url: '/a/', title: 'A "page"', remote: '127.0.0.1', device: 'Mac' };
  assert.equal(clientLogLine('text', e), `client navigate ${id} /a/ 127.0.0.1 Mac`);
  assert.equal(clientLogLine('json', e), `{"type":"client","ts":"2026-01-02T03:04:05.006Z","event":"navigate","id":"${id}","url":"/a/","title":"A \\"page\\"","remote":"127.0.0.1","device":"Mac"}`);
  assert.equal(clientLogLine('json', { ...e, title: '' }), `{"type":"client","ts":"2026-01-02T03:04:05.006Z","event":"navigate","id":"${id}","url":"/a/","remote":"127.0.0.1","device":"Mac"}`);
});

test('--sync: pages report where they are, the others follow, /__sitefile/clients lists them; off without --sync', async () => {
  const tree = Tree.fromFiles([['/index.html', makeFile(enc('<h1>x</h1>'), 't')]]);
  const log: string[] = [];
  const live = new LiveReload({ sync: 'scroll' });
  const server = createSiteServer(() => tree, { liveReload: live, log: (l) => log.push(l), logFormat: 'json' });
  const plain = createSiteServer(() => tree, { liveReload: new LiveReload(), log: () => undefined });
  const { port } = await listen(server, '127.0.0.1', 0);
  const { port: plainPort } = await listen(plain, '127.0.0.1', 0);
  const A = 'aaaaaaaaaaaaaaaa';
  const B = 'bbbbbbbbbbbbbbbb';
  const post = (p: number, body: string, headers: Record<string, string> = { origin: `http://127.0.0.1:${p}` }): Promise<number> =>
    new Promise((resolve, reject) => {
      const r = http.request({ host: '127.0.0.1', port: p, path: '/__sitefile/client', method: 'POST', headers: { 'content-type': 'text/plain', ...headers }, agent: false }, (res) => {
        res.resume();
        res.on('end', () => resolve(res.statusCode ?? 0));
      });
      r.on('error', reject);
      r.end(body);
    });
  const report = (o: object): Promise<number> => post(port, JSON.stringify(o));
  try {
    assert.equal(await report({ id: A, url: '/' }), 404, 'no open stream for this page');
    const a = await events(port, { path: `${RELOAD_PATH}?client=${A}`, headers: { 'user-agent': 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)' } });
    const b = await events(port, { path: `${RELOAD_PATH}?client=${B}` });
    for (const ev of [a, b]) {
      assert.equal(await ev.next(), 'retry: 1000');
      assert.match(await ev.next(), /^id: /);
    }
    let list = JSON.parse((await get(port, '/__sitefile/clients')).body) as { clients: Array<Record<string, string>> };
    assert.deepEqual(list.clients.map((c) => [c.id, c.url, c.device]), [[A, '', 'iPhone'], [B, '', 'other']].sort((x, y) => (x[0]! < y[0]! ? -1 : 1)));
    assert.equal(await report({ id: A, url: '/', title: 'Home' }), 204);
    assert.equal(await report({ id: A, url: '/docs/', title: 'Docs', nav: true }), 204);
    assert.equal(await b.next(), 'data: {"type":"navigate","url":"/docs/"}', 'the other page follows');
    assert.equal(await report({ id: B, url: '/docs/', scroll: [0, 5000] }), 204);
    assert.equal(await a.next(), 'data: {"type":"scroll","url":"/docs/","x":0,"y":5000}');
    live.send(OK_EVENT);
    assert.equal(await a.next(), `data: ${OK_EVENT}`, 'a page is not sent its own navigation');
    assert.equal(await b.next(), `data: ${OK_EVENT}`, 'nor its own scrolling');
    const lines = log.map((l) => JSON.parse(l) as Record<string, unknown>);
    assert.deepEqual(
      lines.map((l) => [l.type, l.event, l.id, l.url, l.title, l.device]),
      [
        ['client', 'connect', A, '/', 'Home', 'iPhone'],
        ['client', 'navigate', A, '/docs/', 'Docs', 'iPhone'],
        ['client', 'connect', B, '/docs/', undefined, 'other'],
      ],
      'connect, a new URL; no access-log lines',
    );
    const clients = await get(port, '/__sitefile/clients');
    assert.equal(clients.headers['content-type'], 'application/json');
    assert.equal(clients.headers['cache-control'], 'no-store');
    list = JSON.parse(clients.body) as { clients: Array<Record<string, string>> };
    const ofA = list.clients.find((c) => c.id === A)!;
    assert.deepEqual(Object.keys(ofA), ['id', 'url', 'title', 'remote', 'device', 'since', 'lastSeen']);
    assert.deepEqual([ofA.url, ofA.title, ofA.remote], ['/docs/', 'Docs', '127.0.0.1']);
    assert.equal((await get(port, '/__sitefile/clients', { origin: `http://127.0.0.1:${port}` })).status, 403, 'not to pages');
    assert.equal((await get(port, '/__sitefile/clients', { 'sec-fetch-mode': 'cors' })).status, 403, 'not to pages');
    assert.equal(await post(port, JSON.stringify({ id: A, url: '/' }), { origin: 'http://evil.example' }), 403);
    assert.equal(await post(port, '{"id":"x"}'), 400);
    assert.equal(await post(port, JSON.stringify({ id: A, url: '/', title: 'x'.repeat(5000) })), 413);
    assert.equal((await get(port, '/__sitefile/client')).status, 405);
    let limited = 0;
    for (let i = 0; i < CLIENT_REPORT_LIMIT + 5; i++) if ((await report({ id: B, url: '/docs/' })) === 429) limited++;
    assert.ok(limited >= 5, `rate-limited per page (${limited})`);
    a.close();
    await sleep(100);
    list = JSON.parse((await get(port, '/__sitefile/clients')).body) as { clients: Array<Record<string, string>> };
    assert.deepEqual(list.clients.map((c) => c.id), [B], 'a page whose stream closed is no longer listed');
    b.close();
    // without --sync the paths are not reserved, and a stream's client= is ignored
    assert.equal((await get(plainPort, '/__sitefile/clients')).status, 404);
    assert.equal(await post(plainPort, JSON.stringify({ id: A, url: '/' })), 405);
  } finally {
    await closeServer(server);
    await closeServer(plain);
  }
});

test('--navigate-to-changed: a reload that changed exactly one page names its URL', () => {
  const t = (files: Record<string, string>) => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  const a = t({ '/index.html': 'i', '/posts/a/index.html': 'a', '/posts/b/index.html': 'b', '/x.html': 'x', '/s.css': 's', '/app.js': '1' });
  const same = { '/index.html': 'i', '/posts/a/index.html': 'a', '/posts/b/index.html': 'b', '/x.html': 'x', '/s.css': 's', '/app.js': '1' };
  assert.equal(reloadEventFor(a, t({ ...same, '/posts/a/index.html': 'A' }), true, true), '{"type":"reload","path":"/posts/a/","morph":true}');
  assert.equal(reloadEventFor(a, t({ ...same, '/posts/a/index.html': 'A' }), true, false), '{"type":"reload","morph":true}', 'off by default');
  assert.equal(reloadEventFor(a, t({ ...same, '/x.html': 'X', '/app.js': '2' }), true, true), '{"type":"reload","path":"/x.html"}', 'other files may change too');
  assert.equal(reloadEventFor(a, t({ ...same, '/index.html': 'I', '/posts/a/index.html': 'A' }), true, true), '{"type":"reload","morph":true}', 'two pages changed');
  assert.equal(reloadEventFor(a, t({ ...same, '/app.js': '2' }), true, true), RELOAD_EVENT, 'no page changed');
  assert.equal(reloadEventFor(a, t({ ...same, '/s.css': 'S' }), true, true), '{"type":"css","paths":["/s.css"]}');
  assert.equal(reloadEventFor(a, t({ ...same, '/new/index.html': 'n' }), true, true), '{"type":"reload","path":"/new/"}', 'a new page');
  assert.equal(pageUrlFor('/index.html'), '/');
  assert.equal(pageUrlFor('/a b/index.html'), '/a%20b/');
  assert.equal(pageUrlFor('/caf\u00e9.html'), '/caf%C3%A9.html');
});

test('--reload morph: only reloads where nothing but existing HTML changed are marked morph', () => {
  const t = (files: Record<string, string>) => Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(enc(c), 't')] as const));
  const a = t({ '/index.html': 'i', '/b.html': 'b', '/app.js': '1', '/s.css': 's' });
  const m = (files: Record<string, string>) => reloadEventFor(a, t({ '/index.html': 'i', '/b.html': 'b', '/app.js': '1', '/s.css': 's', ...files }), true);
  assert.equal(m({ '/index.html': 'I' }), '{"type":"reload","morph":true}');
  assert.equal(m({ '/index.html': 'I', '/b.html': 'B' }), '{"type":"reload","morph":true}');
  assert.equal(m({ '/index.html': 'I', '/app.js': '2' }), RELOAD_EVENT, 'a script changed');
  assert.equal(m({ '/index.html': 'I', '/s.css': 'S' }), RELOAD_EVENT, 'a stylesheet too');
  assert.equal(m({ '/c.html': 'c' }), RELOAD_EVENT, 'a page added');
  assert.equal(reloadEventFor(a, t({ '/index.html': 'I', '/app.js': '1', '/s.css': 's' }), true), RELOAD_EVENT, 'a page removed');
  assert.equal(clientScript({ reload: 'full' }).endsWith('"reload":"full"});\n'), true);
  // the client compares the scripts, keeps its own nodes and the focused field, and restores scroll
  for (const re of [/scriptsOf\(doc\) !== scriptsOf\(document\)/, /document\.activeElement/, /sessionStorage\.setItem\(scrollKey/, /options\.reload !== "morph"/]) assert.match(RELOAD_CLIENT, re);
});

test('the client script parses with every option', () => {
  for (const o of [{}, { resetServiceWorkers: false, cssInject: false, console: true, overlay: false, navigateToChanged: true, reload: 'full' as const }, { sync: 'nav' as const }, { sync: 'scroll' as const }]) {
    assert.doesNotThrow(() => new Function(clientScript(o)));
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
function events(port: number, through?: { proxyPort?: number; lastEventId?: string; path?: string; headers?: Record<string, string> }): Promise<{ next: () => Promise<string>; close: () => void; headers: http.IncomingHttpHeaders }> {
  return new Promise((resolve, reject) => {
    const target = through?.proxyPort !== undefined ? { port: through.proxyPort } : { port };
    const headers = { ...(through?.lastEventId !== undefined ? { 'last-event-id': through.lastEventId } : {}), ...through?.headers };
    const req = http.get({ host: '127.0.0.1', ...target, path: through?.path ?? RELOAD_PATH, headers, agent: false }, (res) => {
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
    assert.ok(js.body.endsWith('({"resetServiceWorkers":true,"cssInject":true,"console":false,"overlay":true,"navigateToChanged":false,"reload":"morph"});\n'));
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
    assert.match(await ev.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
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

test('live reload: every stream starts with the server instance; a page of an earlier instance (Last-Event-ID) reloads', async () => {
  const live = new LiveReload();
  assert.match(live.instance, /^[0-9a-f]{16}$/);
  assert.notEqual(live.instance, new LiveReload().instance, 'random per server process');
  const server = createSiteServer(() => Tree.empty, { liveReload: live });
  const { port } = await listen(server, '127.0.0.1', 0);
  try {
    for (const [lastEventId, reload] of [[undefined, false], [live.instance, false], ['0123456789abcdef', true]] as const) {
      const ev = await events(port, lastEventId === undefined ? undefined : { lastEventId });
      assert.equal(await ev.next(), 'retry: 1000');
      assert.equal(await ev.next(), `id: ${live.instance}`);
      if (reload) assert.equal(await ev.next(), `data: ${RELOAD_EVENT}`, 'a page of an earlier instance reloads');
      live.send(OK_EVENT);
      assert.equal(await ev.next(), `data: ${OK_EVENT}`, reload ? 'then events as usual' : 'no reload first');
      ev.close();
    }
  } finally {
    await closeServer(server);
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
    assert.match(await ev.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
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
    assert.match(RELOAD_CLIENT, /new URL\(sync \? "reload\?client=" \+ clientId : "reload", base\)/);
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
  assert.match(r.stderr, /^error: -d --watch serves a plain directory, and .* has a Sitefile: build it with serve --watch in the foreground\n/);
  r = await runCli(home, ['serve', '-d', '--watch', '--no-css-inject', path.join(site, 'public')]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /^error: --no-css-inject is for serve --watch in the foreground, not -d\n/);
  r = await runCli(home, ['serve', '-d', '--watch', path.join(site, 'nope')]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /^error\[E_NOT_FOUND\]: '.*nope' is not a directory\n/);
  r = await runCli(home, ['serve', '--build-arg', 'A=1', './x/']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--build-arg needs --watch/);
  r = await runCli(home, ['serve', '--watch', '--reload', 'soft', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--reload expects full or morph, got 'soft'/);
  r = await runCli(home, ['serve', '--keep-service-workers', './x/']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--keep-service-workers needs --watch/);
  r = await runCli(home, ['serve', '--sync', './x/']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /^error: --sync needs --watch\n/);
  r = await runCli(home, ['serve', '--watch', '--sync=all', site]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /^error: --sync takes no value or =scroll, got 'all'\n/);
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
    assert.match(await ev.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
    await fs.writeFile(path.join(site, 'public', 'index.html'), '<h1>v2</h1>');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"reload","morph":true}', 'only HTML changed: pages may morph');
    r = await get(port, '/', NAV);
    assert.equal(r.body, `<h1>v2</h1>${RELOAD_SCRIPT}`);
    // only a stylesheet of the build changed: pages swap it in place
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:1px}');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"css","paths":["/style.css"]}');

    // a failed rebuild: error printed, an error event, the last good build still served
    const good = await fs.readFile(path.join(site, 'Sitefile'), 'utf8');
    await fs.writeFile(path.join(site, 'Sitefile'), 'FROM scratch\nCOPY ./missing/ /\nEXPORT /\n');
    await w.waitFor(/Build failed; keeping the last good output/);
    assert.match(w.stderr(), /error\[E_/);
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    const failed = await ev.next();
    assert.match(failed, /^data: \{"type":"error","code":"E_[A-Z_]+","message":"[^"]+","file":"[^"]*Sitefile","line":2,"column":\d+\}$/);
    assert.equal((await get(port, '/', NAV)).body, `<h1>v2</h1>${RELOAD_SCRIPT}`);
    // a page opened while the build is broken gets the error at once
    const late = await events(port);
    assert.equal(await late.next(), 'retry: 1000');
    assert.match(await late.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
    assert.equal(await late.next(), failed);
    late.close();
    // fixed, with the same output as before: ok (the overlay clears, no reload)
    await fs.writeFile(path.join(site, 'Sitefile'), good);
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"ok"}');
    const fresh = await events(port);
    assert.equal(await fresh.next(), 'retry: 1000');
    assert.match(await fresh.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
    fresh.close();
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
    assert.match(await ev.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
    await fs.writeFile(path.join(dir, 's.css'), 'a{color:red}');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"css","paths":["/s.css"]}');
    assert.equal((await get(port, '/s.css')).body, 'a{color:red}');
    await fs.writeFile(path.join(dir, 'index.html'), '<h1>p2</h1>');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"reload","morph":true}');
    await w.waitFor(/^Change detected, re-reading$/m, 2);
    assert.equal((await get(port, '/', NAV)).body, `<h1>p2</h1>${RELOAD_SCRIPT}`);
    ev.close();
    assert.deepEqual((await fs.readdir(dir)).sort(), ['.env', 'index.html', 's.css'], 'nothing is written to the directory');
  } finally {
    assert.equal(await w.stop(), 0);
  }
});

test('serve -d --watch ./dir: the detached server re-reads the directory, live-reloads, keeps the watch over a restart, and reloads pages left open', async () => {
  const home = await tmpDir('sitefile-home-');
  const dir = await tmpDir('sitefile-plain-');
  await fs.writeFile(path.join(dir, 'index.html'), '<link rel="stylesheet" href="/s.css"><h1>d1</h1>');
  await fs.writeFile(path.join(dir, 's.css'), 'a{}');
  const port = await new Promise<number>((resolve) => {
    const s = http.createServer().listen(0, '127.0.0.1', () => {
      const p = (s.address() as { port: number }).port;
      s.close(() => resolve(p));
    });
  });
  let r = await runCli(home, ['serve', '-d', '--watch', '--name', 'dw', '-p', `127.0.0.1:${port}`, '--poll-interval', '50', dir]);
  assert.equal(r.code, 0, r.stderr);
  try {
    const state = JSON.parse(await fs.readFile(path.join(home, 'servers', `${/\((\w{12})\)/.exec(r.stdout)![1]}.json`), 'utf8'));
    assert.equal(state.watch, true);
    assert.equal(state.pollMs, 50);
    assert.equal((await get(port, '/', NAV)).body, `<link rel="stylesheet" href="/s.css"><h1>d1</h1>${RELOAD_SCRIPT}`);
    const ev = await events(port);
    assert.equal(await ev.next(), 'retry: 1000');
    const id = (await ev.next()).slice(4);
    assert.match(id, /^[0-9a-f]{16}$/);
    await fs.writeFile(path.join(dir, 's.css'), 'a{color:red}');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"css","paths":["/s.css"]}');
    await fs.writeFile(path.join(dir, 'index.html'), '<h1>d2</h1>');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"reload","morph":true}');
    assert.equal((await get(port, '/', NAV)).body, `<h1>d2</h1>${RELOAD_SCRIPT}`);
    ev.close();
    const log = await runCli(home, ['server', 'logs', 'dw']);
    assert.match(log.stdout, /Z Change detected, re-reading\n.*Z Read 2 files \(sha256:[0-9a-f]{64}\)\n/);

    r = await runCli(home, ['server', 'restart', 'dw']);
    assert.equal(r.code, 0, r.stderr);
    const ps = JSON.parse((await runCli(home, ['servers', '--json'])).stdout) as { name: string; watch?: boolean; pollMs?: number }[];
    assert.deepEqual(ps.filter((s) => s.name === 'dw').map((s) => [s.watch, s.pollMs]), [[true, 50]], 'restart keeps the watch');
    const again = await events(port, { lastEventId: id });
    assert.equal(await again.next(), 'retry: 1000');
    assert.notEqual(await again.next(), `id: ${id}`, 'a new instance');
    assert.equal(await again.next(), 'data: {"type":"reload"}', 'pages left open across the restart reload');
    await fs.writeFile(path.join(dir, 'index.html'), '<h1>d3</h1>');
    assert.equal(await again.next(), 'data: {"type":"building"}');
    assert.equal(await again.next(), 'data: {"type":"reload","morph":true}', 'the restarted server still watches');
    again.close();
  } finally {
    await runCli(home, ['server', 'rm', '-f', 'dw']);
  }
});

test('serve -d --watch --sync=scroll: recorded (and kept over a restart); pages are listed at /__sitefile/clients and logged as client lines', async () => {
  const home = await tmpDir('sitefile-home-');
  const dir = await tmpDir('sitefile-plain-');
  await fs.writeFile(path.join(dir, 'index.html'), '<h1>s</h1>');
  let r = await runCli(home, ['serve', '-d', '--watch', '--sync=scroll', '--log-format', 'json', '--name', 'ds', dir]);
  assert.equal(r.code, 0, r.stderr);
  try {
    const port = Number(/127\.0\.0\.1:(\d+)/.exec(r.stdout)![1]);
    const state = JSON.parse(await fs.readFile(path.join(home, 'servers', `${/\((\w{12})\)/.exec(r.stdout)![1]}.json`), 'utf8'));
    assert.equal(state.sync, 'scroll');
    assert.ok((await get(port, RELOAD_SCRIPT_PATH)).body.endsWith(',"sync":"scroll"});\n'));
    const id = 'c0ffee00c0ffee00';
    const ev = await events(port, { path: `${RELOAD_PATH}?client=${id}` });
    assert.equal(await ev.next(), 'retry: 1000');
    await ev.next();
    const status = await new Promise<number>((resolve, reject) => {
      const q = http.request({ host: '127.0.0.1', port, path: '/__sitefile/client', method: 'POST', headers: { origin: `http://127.0.0.1:${port}` }, agent: false }, (res) => {
        res.resume();
        res.on('end', () => resolve(res.statusCode ?? 0));
      });
      q.on('error', reject);
      q.end(JSON.stringify({ id, url: '/', title: 'S' }));
    });
    assert.equal(status, 204);
    const list = JSON.parse((await get(port, '/__sitefile/clients')).body) as { clients: Array<{ id: string; url: string; title: string }> };
    assert.deepEqual(list.clients.map((c) => [c.id, c.url, c.title]), [[id, '/', 'S']]);
    ev.close();
    const lines = (await runCli(home, ['server', 'logs', 'ds'])).stdout.split('\n').filter((l) => l.includes('"type":"client"'));
    assert.equal(lines.length, 1);
    assert.match(lines[0]!, new RegExp(`"event":"connect","id":"${id}","url":"/","title":"S","remote":"127\\.0\\.0\\.1","device":"other"\\}$`));
    r = await runCli(home, ['server', 'restart', 'ds']);
    assert.equal(r.code, 0, r.stderr);
    const ps = JSON.parse((await runCli(home, ['servers', '--json'])).stdout) as { name: string; sync?: string }[];
    assert.deepEqual(ps.filter((s) => s.name === 'ds').map((s) => s.sync), ['scroll'], 'restart keeps --sync');
  } finally {
    await runCli(home, ['server', 'rm', '-f', 'ds']);
  }
});

test('serve --watch --no-css-inject: a stylesheet-only rebuild reloads', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite({ 'public/index.html': '<h1>x</h1>' });
  const w = startCli(home, ['serve', '--watch', '--no-css-inject', site]);
  try {
    await w.waitFor(/Watching /);
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(w.stdout())?.[1]);
    assert.match((await get(port, '/__sitefile/reload.js')).body, /"cssInject":false,"console":false,"overlay":true,"navigateToChanged":false,"reload":"morph"\}\);\n$/);
    const ev = await events(port);
    assert.equal(await ev.next(), 'retry: 1000');
    assert.match(await ev.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
    await fs.writeFile(path.join(site, 'public', 'style.css'), 'body{margin:3px}');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"reload"}');
    ev.close();
  } finally {
    assert.equal(await w.stop(), 0);
  }
});

test('serve --watch --navigate-to-changed: editing a post\'s Markdown names that post\'s URL', async () => {
  const home = await tmpDir('sitefile-home-');
  const site = await makeSite(
    { 'posts/2026-01-01-first.md': '# First\n\nOne.\n', 'posts/2026-01-02-second.md': '# Second\n\nTwo.\n', 'templates/post.html': '<main>${post.content}</main>' },
    'FROM scratch\nIMPORT posts FROM ./posts AS markdown\nRENDER --escape=html ./templates/post.html TO /posts/${post.slug}/index.html FOR post IN posts\nEXPORT /\n',
  );
  const w = startCli(home, ['serve', '--watch', '--navigate-to-changed', site]);
  try {
    await w.waitFor(/Watching /);
    const port = Number(/at http:\/\/127\.0\.0\.1:(\d+)\//.exec(w.stdout())?.[1]);
    assert.match((await get(port, '/__sitefile/reload.js')).body, /"navigateToChanged":true,"reload":"morph"\}\);\n$/);
    const ev = await events(port);
    assert.equal(await ev.next(), 'retry: 1000');
    assert.match(await ev.next(), /^id: [0-9a-f]{16}$/, 'the server instance');
    await fs.writeFile(path.join(site, 'posts', '2026-01-02-second.md'), '# Second\n\nTwo, edited.\n');
    assert.equal(await ev.next(), 'data: {"type":"building"}');
    assert.equal(await ev.next(), 'data: {"type":"reload","path":"/posts/second/","morph":true}');
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
