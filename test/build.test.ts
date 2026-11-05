import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, symlink, stat } from 'node:fs/promises';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { build, buildToDirectory, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { sha256 } from '../src/digest.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-build-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

async function run(files: Record<string, string>, extra: Partial<BuildOptions> = {}) {
  const dir = await project(files);
  return { dir, result: await build({ contextDir: dir, lockfile: false, ...extra }) };
}

async function rejectsWith(p: Promise<unknown>, code: ErrorCode, line?: number): Promise<void> {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), `expected SitefileError, got ${String(e)}`);
    assert.equal(e.code, code, e.message);
    if (line !== undefined) assert.equal(e.span?.line, line, e.message);
    return true;
  });
}

test('hello: ARG/LET, COPY dir contents, RENDER with html escaping', async () => {
  const { result } = await run(
    {
      Sitefile: [
        'FROM scratch',
        'ARG TITLE = "Hello"',
        'LET GREETING = "Welcome to ${TITLE}"',
        'COPY ./public/ /',
        'RENDER --escape=html ./index.html TO /index.html',
        'EXPORT /',
      ].join('\n'),
      'public/style.css': 'body{}',
      'public/img/a.txt': 'a',
      'index.html': '<title>${TITLE}</title><p>${GREETING}</p>',
    },
    { buildArgs: { TITLE: 'Tom & <Jerry>' } },
  );
  assert.deepEqual(
    result.output.entries().map(([p]) => p),
    ['/img/a.txt', '/index.html', '/style.css'],
  );
  assert.equal(
    text(result.output.get('/index.html')?.bytes),
    '<title>Tom &amp; &lt;Jerry&gt;</title><p>Welcome to Tom &amp; &lt;Jerry&gt;</p>',
  );
});

test('missing required ARG fails; typed ARG coerces build args', async () => {
  await rejectsWith(run({ Sitefile: 'FROM scratch\nARG NAME\nEXPORT /' }).then((r) => r.result), 'E_MISSING_ARG', 2);
  await rejectsWith(
    run({ Sitefile: 'FROM scratch\nARG N: number = 1\nEXPORT /' }, { buildArgs: { N: 'abc' } }).then((r) => r.result),
    'E_TYPE',
  );
  const { result } = await run(
    { Sitefile: 'FROM scratch\nARG N: number = 1\nRENDER ./t TO /n.txt\nEXPORT /', t: '${N}' },
    { buildArgs: { N: '42', EXTRA: 'x' } },
  );
  assert.equal(text(result.output.get('/n.txt')?.bytes), '42');
  assert.ok(result.warnings.some((w) => w.includes('EXTRA')));
});

test('COPY file destination semantics', async () => {
  const { result } = await run({
    Sitefile: [
      'FROM scratch',
      'COPY ./a.txt /exact.txt',
      'COPY ./a.txt /dir/',
      'COPY ./b.txt /dir',
      'EXPORT /',
    ].join('\n'),
    'a.txt': 'A',
    'b.txt': 'B',
  });
  assert.deepEqual(
    result.output.entries().map(([p]) => p),
    ['/dir/a.txt', '/dir/b.txt', '/exact.txt'],
  );
});

test('conflicting writes fail unless --overwrite; identical writes are fine', async () => {
  const files = { 'a.txt': 'A', 'b.txt': 'B', 'a2.txt': 'A' };
  await rejectsWith(
    run({ ...files, Sitefile: 'FROM scratch\nCOPY ./a.txt /x\nCOPY ./b.txt /x\nEXPORT /' }).then((r) => r.result),
    'E_CONFLICT',
    3,
  );
  const ok = await run({ ...files, Sitefile: 'FROM scratch\nCOPY ./a.txt /x\nCOPY ./a2.txt /x\nCOPY --overwrite ./b.txt /x\nEXPORT /' });
  assert.equal(text(ok.result.output.get('/x')?.bytes), 'B');
  await rejectsWith(
    run({ ...files, Sitefile: 'FROM scratch\nCOPY ./a.txt /x\nCOPY ./b.txt /x/y\nEXPORT /' }).then((r) => r.result),
    'E_CONFLICT',
  );
});

test('local path escapes are rejected', async () => {
  await rejectsWith(run({ Sitefile: 'FROM scratch\nCOPY ../etc/passwd /p\nEXPORT /' }).then((r) => r.result), 'E_PATH_ESCAPE', 2);
  await rejectsWith(run({ Sitefile: 'FROM scratch\nCOPY /etc/passwd /p\nEXPORT /' }).then((r) => r.result), 'E_PATH_ESCAPE', 2);
  const dir = await project({ Sitefile: 'FROM scratch\nCOPY ./link /p\nEXPORT /' });
  await symlink('/etc/hosts', path.join(dir, 'link'));
  await rejectsWith(build({ contextDir: dir, lockfile: false }), 'E_PATH_ESCAPE');
});

test('local IMPORTs: tree, json and text values in templates', async () => {
  const { result } = await run({
    Sitefile: [
      'FROM scratch',
      'IMPORT assets FROM ./assets',
      'IMPORT site FROM ./site.json AS json',
      'IMPORT note FROM ./note.txt AS text',
      'COPY assets:/css/ /static/',
      'COPY assets:/logo.svg /logo.svg',
      'RENDER ./page.html TO /index.html',
      'EXPORT /',
    ].join('\n'),
    'assets/css/main.css': 'css',
    'assets/logo.svg': '<svg/>',
    'site.json': '{"title":"Docs","links":[{"href":"/a"}]}',
    'note.txt': 'hi',
    'page.html': '${site.title} ${site.links.0.href} ${note} $${literal}',
  });
  assert.equal(text(result.output.get('/index.html')?.bytes), 'Docs /a hi ${literal}');
  assert.ok(result.output.get('/static/main.css'));
  assert.ok(result.output.get('/logo.svg'));
});

test('value imports cannot be copied; templates reject unknown names with template position', async () => {
  await rejectsWith(
    run({ Sitefile: 'FROM scratch\nIMPORT d FROM ./d.json AS json\nCOPY d: /d.json\nEXPORT /', 'd.json': '{}' }).then((r) => r.result),
    'E_TYPE',
  );
  await assert.rejects(
    run({ Sitefile: 'FROM scratch\nRENDER ./t.html TO /t.html\nEXPORT /', 't.html': 'line1\n  ${NOPE}' }).then((r) => r.result),
    (e: unknown) => isSitefileError(e) && e.code === 'E_UNDEFINED' && e.span?.line === 2 && e.span.column === 3,
  );
});

test('remote sources are refused when given as COPY sources or blocked protocols', async () => {
  await rejectsWith(run({ Sitefile: 'FROM scratch\nIMPORT x FROM ftp://example.com/a\nEXPORT /' }).then((r) => r.result), 'E_PROTOCOL');
  await rejectsWith(run({ Sitefile: 'FROM scratch\nIMPORT x FROM http://example.com/a\nEXPORT /' }).then((r) => r.result), 'E_PROTOCOL');
});

// ------------------------------------------------------------ remote (M1)

let server: Server;
let base = '';
let body = 'remote content v1';
async function startServer(): Promise<void> {
  if (server) return;
  server = createServer((req, res) => {
    if (req.url === '/lib.js') {
      res.writeHead(200, { 'content-type': 'text/javascript' });
      res.end(body);
    } else if (req.url === '/moved') {
      res.writeHead(302, { location: '/lib.js' });
      res.end();
    } else {
      res.writeHead(404);
      res.end();
    }
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
}
after(() => server?.close());

const permissive = { fetchPolicy: { allowHttp: true, allowPrivateNetwork: true } };

test('M1 acceptance: pinned remote file is copied into the output', async () => {
  await startServer();
  body = 'remote content v1';
  const pin = sha256(body);
  const { result } = await run(
    { Sitefile: `FROM scratch\nIMPORT lib FROM ${base}/moved INTEGRITY ${pin}\nCOPY lib: /js/\nEXPORT /` },
    permissive,
  );
  assert.equal(text(result.output.get('/js/lib.js')?.bytes), body);
});

test('integrity mismatch fails', async () => {
  await startServer();
  body = 'remote content v1';
  await rejectsWith(
    run({ Sitefile: `FROM scratch\nIMPORT lib FROM ${base}/lib.js INTEGRITY ${sha256('other')}\nEXPORT /` }, permissive).then((r) => r.result),
    'E_INTEGRITY',
  );
});

test('remote imports are blocked by default network policy', async () => {
  await startServer();
  await rejectsWith(run({ Sitefile: `FROM scratch\nIMPORT lib FROM ${base}/lib.js\nEXPORT /` }).then((r) => r.result), 'E_PROTOCOL');
  await rejectsWith(
    run({ Sitefile: `FROM scratch\nIMPORT lib FROM ${base}/lib.js\nEXPORT /` }, { fetchPolicy: { allowHttp: true } }).then((r) => r.result),
    'E_SSRF_BLOCKED',
  );
});

test('lockfile records unpinned URLs, detects drift, and honours --frozen-lockfile', async () => {
  await startServer();
  body = 'v1';
  const dir = await project({ Sitefile: `FROM scratch\nIMPORT lib FROM ${base}/lib.js\nCOPY lib: /lib.js\nEXPORT /` });
  const opts: BuildOptions = { contextDir: dir, ...permissive };

  const first = await build(opts);
  assert.equal(first.lockfileWritten, true);
  assert.ok(first.warnings.some((w) => w.includes('not pinned')));
  const lock = JSON.parse(await readFile(path.join(dir, 'Sitefile.lock'), 'utf8'));
  assert.equal(lock.resources[`${base}/lib.js`].integrity, sha256('v1'));

  assert.equal((await build(opts)).lockfileWritten, false);
  await build({ ...opts, frozenLockfile: true });

  body = 'v2';
  await rejectsWith(build(opts), 'E_LOCK_MISMATCH', 2);
  await rejectsWith(build({ ...opts, frozenLockfile: true }), 'E_LOCK_MISMATCH');
  const updated = await build({ ...opts, updateLock: true });
  assert.equal(text(updated.output.get('/lib.js')?.bytes), 'v2');

  const fresh = await project({ Sitefile: `FROM scratch\nIMPORT lib FROM ${base}/lib.js\nEXPORT /` });
  await rejectsWith(build({ contextDir: fresh, ...permissive, frozenLockfile: true }), 'E_LOCK_MISSING');
});

// ------------------------------------------------------------ export

test('buildToDirectory writes files and skips its own output when copying the context', async () => {
  const dir = await project({ Sitefile: 'FROM scratch\nCOPY ./ /\nEXPORT /', 'a.txt': 'A', '.git/HEAD': 'ref' });
  const out = path.join(dir, 'dist');
  // Leftover from an interrupted export: must not be copied.
  await mkdir(path.join(dir, '.dist.sitefile-tmp-abc123'));
  await writeFile(path.join(dir, '.dist.sitefile-tmp-abc123', 'junk'), 'x');
  await buildToDirectory({ contextDir: dir, outDir: out, lockfile: false });
  const second = await buildToDirectory({ contextDir: dir, outDir: out, lockfile: false });
  assert.deepEqual(
    second.output.entries().map(([p]) => p),
    ['/Sitefile', '/a.txt'],
  );
  assert.equal(await readFile(path.join(out, 'a.txt'), 'utf8'), 'A');
  assert.ok((await stat(path.join(out, 'Sitefile'))).isFile());
});

// ---- stages ---------------------------------------------------------------------

const paths = (t: { entries(): Array<[string, unknown]> }): string[] => t.entries().map(([p]) => p);

test('multi-stage build: COPY --from, RENDER --from and FROM <stage>', async () => {
  const { result } = await run({
    Sitefile: [
      'FROM scratch AS build',
      'COPY ./src/ /out/',
      'COPY ./src/a.txt /top.txt',
      'FROM scratch AS site',
      'COPY --from=build /out/ /',
      'COPY --from=build /top.txt /files/',
      'COPY --from=build /out/a.txt /single.txt',
      'FROM site',
      'RENDER --from=build /out/a.txt TO /rendered.txt',
      'COPY ./extra.txt /extra.txt',
      'EXPORT /',
    ].join('\n'),
    'src/a.txt': 'A',
    'extra.txt': 'X',
  });
  assert.deepEqual(paths(result.output), ['/a.txt', '/extra.txt', '/files/top.txt', '/rendered.txt', '/single.txt']);
  assert.equal(text(result.output.get('/files/top.txt')?.bytes), 'A');
  assert.match(result.output.get('/a.txt')?.origin ?? '', /Sitefile:5 COPY$/);
  assert.equal(result.plan.stages.length, 3);
});

test('stages do not leak into each other', async () => {
  const { result } = await run({
    Sitefile: 'FROM scratch AS a\nCOPY ./x /x\nFROM scratch\nCOPY ./y /y\nEXPORT /',
    x: 'x',
    y: 'y',
  });
  assert.deepEqual(paths(result.output), ['/y']);
});

test('FROM <stage> copies the tree and can be changed independently', async () => {
  const { result } = await run({
    Sitefile: 'FROM scratch AS a\nCOPY ./x /x\nFROM a AS b\nCOPY ./y /y\nFROM a\nCOPY --from=b /y /z\nEXPORT /',
    x: 'x',
    y: 'y',
  });
  assert.deepEqual(paths(result.output), ['/x', '/z']);
});

test('FROM <local dir> seeds a stage, with sandbox rules', async () => {
  const { result } = await run({
    Sitefile: 'FROM ./base\nCOPY ./extra.txt /extra.txt\nEXPORT /',
    'base/index.html': 'hi',
    'base/css/a.css': 'a',
    'extra.txt': 'e',
  });
  assert.deepEqual(paths(result.output), ['/css/a.css', '/extra.txt', '/index.html']);
  assert.match(result.output.get('/index.html')?.origin ?? '', /Sitefile:1 FROM$/);

  const sub = await run({ Sitefile: 'FROM base/css\nEXPORT /', 'base/css/a.css': 'a' });
  assert.deepEqual(paths(sub.result.output), ['/a.css']);

  await rejectsWith(run({ Sitefile: 'FROM ../etc\nEXPORT /' }).then((r) => r.result), 'E_PATH_ESCAPE', 1);
  await rejectsWith(run({ Sitefile: 'FROM /etc\nEXPORT /' }).then((r) => r.result), 'E_PATH_ESCAPE', 1);
  await rejectsWith(run({ Sitefile: 'FROM ./f.txt\nEXPORT /', 'f.txt': 'x' }).then((r) => r.result), 'E_TYPE', 1);
  await rejectsWith(run({ Sitefile: 'FROM ./missing\nEXPORT /' }).then((r) => r.result), 'E_NOT_FOUND', 1);
  const dir = await project({ Sitefile: 'FROM ./link\nEXPORT /' });
  await symlink(os.tmpdir(), path.join(dir, 'link'));
  await rejectsWith(build({ contextDir: dir, lockfile: false }), 'E_PATH_ESCAPE', 1);
});

test('COPY --from errors and conflicts', async () => {
  const base = 'FROM scratch AS a\nCOPY ./x /x\nFROM scratch\n';
  const files = { x: 'x' };
  await rejectsWith(run({ ...files, Sitefile: base + 'COPY --from=a /nope /\nEXPORT /' }).then((r) => r.result), 'E_NOT_FOUND', 4);
  await rejectsWith(run({ ...files, Sitefile: base + 'COPY --from=a x /\nEXPORT /' }).then((r) => r.result), 'E_PATH', 4);
  await rejectsWith(run({ ...files, Sitefile: base + 'COPY --from=a /../x /\nEXPORT /' }).then((r) => r.result), 'E_PATH', 4);
  await rejectsWith(run({ ...files, Sitefile: base + 'COPY --from=zz /x /\nEXPORT /' }).then((r) => r.result), 'E_UNDEFINED', 4);
  await rejectsWith(
    run({ ...files, y: 'y', Sitefile: base + 'COPY ./y /x\nCOPY --from=a /x /x\nEXPORT /' }).then((r) => r.result),
    'E_CONFLICT',
    5,
  );
  const ok = await run({ ...files, y: 'y', Sitefile: base + 'COPY ./y /x\nCOPY --overwrite --from=a /x /x\nEXPORT /' });
  assert.equal(text(ok.result.output.get('/x')?.bytes), 'x');
});
