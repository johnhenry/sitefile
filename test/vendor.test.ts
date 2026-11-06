import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { gzipSync } from 'node:zlib';
import { sha256 } from '../src/digest.js';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { parseVendorManifest, vendorDirRel } from '../src/resources/vendor.js';
import { vendor } from '../src/vendor.js';
import { tar } from './archive-fixtures.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';
import { startMockRegistry } from './helpers/mock-registry.js';

// sitefile vendor (issue #96).

let requests = 0;
let lib = 'console.log(1)';
const archive = gzipSync(tar([{ name: 'docs/a.txt', data: 'archived' }]));
const server: Server = createServer((req, res) => {
  requests++;
  if (req.url === '/lib.js') {
    res.writeHead(200, { 'content-type': 'text/javascript' });
    res.end(lib);
  } else if (req.url === '/moved') {
    res.writeHead(302, { location: '/lib.js' });
    res.end();
  } else if (req.url === '/docs.tgz') {
    res.writeHead(200, { 'content-type': 'application/gzip' });
    res.end(archive);
  } else if (req.url === '/other.txt') {
    res.writeHead(200);
    res.end('other');
  } else {
    res.writeHead(404);
    res.end();
  }
});
await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
after(() => server.close());
const NET = ['--allow-http', '--allow-private-network'];

const sitefile = (extra = ''): string =>
  `FROM scratch\nCOPY ./public/ /\nCOPY ./ /src/\nIMPORT lib FROM ${base}/moved\nIMPORT docs FROM ${base}/docs.tgz AS tree INTEGRITY ${sha256(archive)}\nCOPY lib: /lib.js\nCOPY docs:/docs/ /docs/\n${extra}EXPORT /\n`;

const read = (p: string): Promise<string> => fs.readFile(p, 'utf8');

test('vendor writes content-addressed blobs and a manifest; builds then need no network (nor the network flags)', async () => {
  lib = 'console.log(1)';
  const home = await tmpDir();
  const ctx = await makeSite({}, sitefile());
  const v = await runCli(home, ['vendor', ctx, ...NET]);
  assert.equal(v.code, 0, v.stderr);
  const vdir = path.join(ctx, 'vendor', 'sitefile');
  assert.match(v.stdout, /^Vendored 2 resources and 0 images \(2 blobs, \d+ bytes\) to .*vendor\/sitefile\nUpdated Sitefile\.lock\n$/);
  const manifest = parseVendorManifest(await fs.readFile(path.join(vdir, 'manifest.json')), 'm');
  assert.deepEqual(Object.keys(manifest.resources), [`${base}/docs.tgz`, `${base}/moved`]);
  assert.equal(manifest.resources[`${base}/moved`]?.resolved, `${base}/lib.js`);
  assert.equal(manifest.resources[`${base}/moved`]?.integrity, sha256(lib));
  assert.deepEqual((await fs.readdir(path.join(vdir, 'blobs', 'sha256'))).sort(), [sha256(lib), sha256(archive)].map((d) => d.slice(7)).sort());
  assert.equal(await read(path.join(vdir, 'blobs', 'sha256', sha256(lib).slice(7))), lib);
  const lock = JSON.parse(await read(path.join(ctx, 'Sitefile.lock')));
  assert.equal(lock.resources[`${base}/moved`].integrity, sha256(lib));
  // the manifest is stable: vendoring again rewrites the same bytes
  const before = await read(path.join(vdir, 'manifest.json'));
  assert.equal((await runCli(home, ['vendor', ctx, ...NET])).code, 0);
  assert.equal(await read(path.join(vdir, 'manifest.json')), before);

  // offline: no request reaches the server, and no network flag is needed
  const n = requests;
  const out = path.join(ctx, 'dist');
  const b = await runCli(home, ['build', ctx, '-o', out, '--frozen-lockfile']);
  assert.equal(b.code, 0, b.stderr);
  assert.equal(requests, n, 'the build did not fetch anything');
  assert.equal(b.stderr, '');
  assert.equal(await read(path.join(out, 'lib.js')), lib);
  assert.equal(await read(path.join(out, 'docs', 'a.txt')), 'archived');
  // the vendor directory is never part of a directory copy
  await assert.rejects(fs.stat(path.join(out, 'src', 'vendor')));
  assert.ok(await fs.stat(path.join(out, 'src', 'Sitefile')));
});

test('vendored copies are verified: corruption, INTEGRITY and the lockfile', async () => {
  lib = 'console.log(2)';
  const ctx = await makeSite({}, sitefile());
  await vendor({ contextDir: ctx, fetchPolicy: { allowHttp: true, allowPrivateNetwork: true } });
  const blob = path.join(ctx, 'vendor', 'sitefile', 'blobs', 'sha256', sha256(lib).slice(7));
  await fs.writeFile(blob, 'tampered');
  await assert.rejects(build({ contextDir: ctx }), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_INTEGRITY'), String(e));
    assert.match(e.message, /Sitefile:4:1: vendored copy of '.*\/moved' \(vendor\/sitefile\/blobs\/sha256\/[0-9a-f]{64}\) is corrupt: expected sha256:[0-9a-f]{64}, got sha256:[0-9a-f]{64}; run 'sitefile vendor' again/);
    return true;
  });
  // a consistent but different vendored copy is caught by the lockfile, like a changed download
  await fs.rm(blob);
  const manifestPath = path.join(ctx, 'vendor', 'sitefile', 'manifest.json');
  const m = JSON.parse(await read(manifestPath));
  m.resources[`${base}/moved`].integrity = sha256('other');
  m.resources[`${base}/moved`].size = 5;
  await fs.writeFile(manifestPath, JSON.stringify(m));
  await fs.writeFile(path.join(ctx, 'vendor', 'sitefile', 'blobs', 'sha256', sha256('other').slice(7)), 'other');
  await assert.rejects(build({ contextDir: ctx }), (e) => isSitefileError(e, 'E_LOCK_MISMATCH'));
  // a missing blob is an error, not a silent fetch
  await fs.rm(path.join(ctx, 'vendor', 'sitefile', 'blobs', 'sha256', sha256('other').slice(7)));
  await assert.rejects(build({ contextDir: ctx }), (e) => isSitefileError(e, 'E_NOT_FOUND'));
  // a malformed manifest
  await fs.writeFile(manifestPath, '{"version":2}');
  await assert.rejects(build({ contextDir: ctx }), (e) => isSitefileError(e, 'E_VALIDATION') && /malformed vendor manifest vendor\/sitefile\/manifest\.json: unsupported version 2/.test((e as Error).message));
});

test('a URL that is not vendored is fetched, with a warning', async () => {
  const home = await tmpDir();
  const ctx = await makeSite({}, sitefile());
  assert.equal((await runCli(home, ['vendor', ctx, ...NET])).code, 0);
  await fs.writeFile(path.join(ctx, 'Sitefile'), sitefile(`IMPORT other FROM ${base}/other.txt\nCOPY other: /other.txt\n`));
  const n = requests;
  const b = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), ...NET]);
  assert.equal(b.code, 0, b.stderr);
  assert.equal(requests, n + 1);
  assert.match(b.stderr, /warning: .*Sitefile:8: IMPORT other: '.*\/other\.txt' is not vendored in vendor\/sitefile; fetching it \(run 'sitefile vendor' to add it\)/);
});

test('--vendor-dir: custom location, validation, and refusing to replace other directories', async () => {
  const home = await tmpDir();
  const ctx = await makeSite({ 'third_party/README': 'not ours' }, sitefile());
  const v = await runCli(home, ['vendor', ctx, ...NET, '--vendor-dir', 'deps/offline/']);
  assert.equal(v.code, 0, v.stderr);
  assert.ok(await fs.stat(path.join(ctx, 'deps', 'offline', 'manifest.json')));
  // the default directory does not exist, so a plain build would fetch; the explicit one is used
  const n = requests;
  const b = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--vendor-dir', 'deps/offline']);
  assert.equal(b.code, 0, b.stderr);
  assert.equal(requests, n);
  for (const bad of ['../x', '/abs', '.', '']) {
    const r = await runCli(home, ['vendor', ctx, '--vendor-dir', bad]);
    assert.equal(r.code, 2, bad);
    assert.match(r.stderr, /--vendor-dir expects a relative path inside the build context/);
  }
  const missing = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'dist'), '--vendor-dir', 'nope']);
  assert.equal(missing.code, 1);
  assert.match(missing.stderr, /error\[E_NOT_FOUND\]: vendor directory 'nope' has no manifest\.json; run 'sitefile vendor --vendor-dir nope' first/);
  const other = await runCli(home, ['vendor', ctx, ...NET, '--vendor-dir', 'third_party']);
  assert.equal(other.code, 1);
  assert.match(other.stderr, /error\[E_EXPORT_UNSAFE\]: 'third_party' exists and is not a sitefile vendor directory/);
  assert.equal(await read(path.join(ctx, 'third_party', 'README')), 'not ours');
  // a symlink out of the context is refused
  const outside = await tmpDir();
  await fs.symlink(outside, path.join(ctx, 'escape'));
  const esc = await runCli(home, ['vendor', ctx, ...NET, '--vendor-dir', 'escape/v']);
  assert.equal(esc.code, 1);
  assert.match(esc.stderr, /E_PATH_ESCAPE/);
  assert.deepEqual(await fs.readdir(outside), []);
  assert.equal(vendorDirRel('a//b/'), 'a/b');
});

test('vendor honours --frozen-lockfile and INTEGRITY like a build', async () => {
  const home = await tmpDir();
  const ctx = await makeSite({}, sitefile());
  const f = await runCli(home, ['vendor', ctx, ...NET, '--frozen-lockfile']);
  assert.equal(f.code, 1);
  assert.match(f.stderr, /E_LOCK_MISSING/);
  await assert.rejects(fs.stat(path.join(ctx, 'vendor')));
  const bad = await makeSite({}, `FROM scratch\nIMPORT lib FROM ${base}/lib.js INTEGRITY ${sha256('nope')}\nEXPORT /\n`);
  const r = await runCli(home, ['vendor', bad, ...NET]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_INTEGRITY/);
  await assert.rejects(fs.stat(path.join(bad, 'vendor')));
});

test('BASE images are vendored as OCI blobs; an offline build gives the same image', async () => {
  const reg = await startMockRegistry();
  try {
    reg.seedImage('lib/base', '1', { layers: [{ '/srv/base.txt': 'base' }, { '/srv/www/index.html': 'base index' }], config: { Cmd: ['serve'] } });
    const ctx = await makeSite({}, `FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci BASE ${reg.host}/lib/base:1 AT /srv/www\n`);
    const online = await runCli(await tmpDir(), ['build', ctx, '-o', path.join(ctx, 'online.oci.tar'), '--no-lockfile']);
    assert.equal(online.code, 0, online.stderr);
    const v = await runCli(await tmpDir(), ['vendor', ctx]);
    assert.equal(v.code, 0, v.stderr);
    assert.match(v.stdout, /^Vendored 0 resources and 1 image \(4 blobs, \d+ bytes\)/);
    const m = parseVendorManifest(await fs.readFile(path.join(ctx, 'vendor', 'sitefile', 'manifest.json')), 'm');
    const entry = m.images[`${reg.host}/lib/base:1`];
    assert.ok(entry);
    assert.equal(entry.platform, 'linux/amd64');
    // a fresh store and no registry requests: everything comes from vendor/
    const n = reg.requests.length;
    const home = await tmpDir();
    const off = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'offline.oci.tar'), '--frozen-lockfile']);
    assert.equal(off.code, 0, off.stderr);
    assert.equal(reg.requests.length, n, 'the registry was not contacted');
    const id = (s: string) => /oci image (sha256:[0-9a-f]{64})/.exec(s)?.[1];
    assert.equal(id(off.stdout), id(online.stdout));
    // the vendor directory is not inside the image
    assert.doesNotMatch((await runCli(home, ['build', ctx, '-o', path.join(ctx, 'files'), '--format', 'directory', '--frozen-lockfile'])).stdout, /vendor/);
    await assert.rejects(fs.stat(path.join(ctx, 'files', 'vendor')));
  } finally {
    await reg.close();
  }
});
