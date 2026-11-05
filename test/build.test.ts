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
import { gzipSync } from 'node:zlib';
import { tar, zip } from './archive-fixtures.js';

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
let archive: Buffer = Buffer.alloc(0);
async function startServer(): Promise<void> {
  if (server) return;
  server = createServer((req, res) => {
    if (req.url === '/lib.js') {
      res.writeHead(200, { 'content-type': 'text/javascript' });
      res.end(body);
    } else if (req.url === '/site.tgz') {
      res.writeHead(200, { 'content-type': 'application/gzip' });
      res.end(archive);
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

// ------------------------------------------------------------ archives

test('IMPORT … AS tree extracts a pinned remote archive; lockfile covers the archive bytes', async () => {
  await startServer();
  archive = gzipSync(tar([{ name: 'site/index.html', data: '<p>hi</p>' }, { name: 'site/a/b.css', data: 'b{}' }]));
  const pin = sha256(archive);
  const sitefile = `FROM scratch\nIMPORT site FROM ${base}/site.tgz AS tree INTEGRITY ${pin}\nCOPY site:/site/ /\nEXPORT /`;
  const dir = await project({ Sitefile: sitefile });
  const opts: BuildOptions = { contextDir: dir, ...permissive };
  const r = await build(opts);
  assert.deepEqual(r.output.entries().map(([p]) => p), ['/a/b.css', '/index.html']);
  assert.equal(r.resources.get('site')?.provenance.digest, pin);
  const lock = JSON.parse(await readFile(path.join(dir, 'Sitefile.lock'), 'utf8'));
  assert.equal(lock.resources[`${base}/site.tgz`].integrity, pin);

  // A wrong pin fails before anything is extracted.
  await rejectsWith(
    run({ Sitefile: sitefile.replace(pin, sha256('other')) }, permissive).then((x) => x.result),
    'E_INTEGRITY',
  );
  // A changed archive is caught by the lockfile.
  archive = gzipSync(tar([{ name: 'site/index.html', data: '<p>changed</p>' }]));
  await rejectsWith(build({ ...opts, frozenLockfile: true }), 'E_INTEGRITY');
});

test('unpinned remote archive is locked and drift is detected', async () => {
  await startServer();
  archive = gzipSync(tar([{ name: 'a.txt', data: 'one' }]));
  const dir = await project({ Sitefile: `FROM scratch\nIMPORT s FROM ${base}/site.tgz AS tree\nCOPY s: /\nEXPORT /` });
  const opts: BuildOptions = { contextDir: dir, ...permissive };
  await build(opts);
  archive = gzipSync(tar([{ name: 'a.txt', data: 'two' }]));
  await rejectsWith(build(opts), 'E_LOCK_MISMATCH');
});

test('remote archive limits are enforced and configurable', async () => {
  await startServer();
  archive = gzipSync(tar([{ name: '../evil', data: 'x' }]));
  const sf = { Sitefile: `FROM scratch\nIMPORT s FROM ${base}/site.tgz AS tree\nCOPY s: /\nEXPORT /` };
  await rejectsWith(run(sf, permissive).then((x) => x.result), 'E_ARCHIVE_UNSAFE', 2);
  archive = gzipSync(tar([{ name: 'a', data: '1' }, { name: 'b', data: '2' }]));
  await rejectsWith(run(sf, { ...permissive, archiveLimits: { maxEntries: 1 } }).then((x) => x.result), 'E_ARCHIVE_LIMIT', 2);
  const ok = await run(sf, permissive);
  assert.equal(ok.result.output.size, 2);
});

test('local archive files extract AS tree; a non-archive file is E_TYPE; directories are unchanged', async () => {
  const dir = await project({
    Sitefile: 'FROM scratch\nIMPORT z FROM ./site.zip AS tree INTEGRITY ' + sha256(zip([{ name: 'x.txt', data: 'zip!', deflate: true }])) + '\nCOPY z: /\nEXPORT /',
    'notes.txt': 'plain',
  });
  await writeFile(path.join(dir, 'site.zip'), zip([{ name: 'x.txt', data: 'zip!', deflate: true }]));
  const r = await build({ contextDir: dir, lockfile: false });
  assert.equal(text(r.output.get('/x.txt')?.bytes), 'zip!');

  await rejectsWith(
    run({ Sitefile: 'FROM scratch\nIMPORT n FROM ./notes.txt AS tree\nEXPORT /', 'notes.txt': 'plain' }).then((x) => x.result),
    'E_TYPE',
    2,
  );
  const d = await run({ Sitefile: 'FROM scratch\nIMPORT d FROM ./src AS tree\nCOPY d: /\nEXPORT /', 'src/a.txt': 'a' });
  assert.deepEqual(d.result.output.entries().map(([p]) => p), ['/a.txt']);
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
