import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { sha256 } from '../src/digest.js';
import { formatOutdated } from '../src/cli/outdated.js';
import { newerVersionTag, outdated } from '../src/outdated.js';
import { RegistryClient } from '../src/registry.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';
import { startMockRegistry } from './helpers/mock-registry.js';

// sitefile outdated (issue #104).

const bodies: Record<string, string> = { '/a.js': 'a1', '/b.js': 'b1', '/c.js': 'c1' };
const server: Server = createServer((req, res) => {
  const b = bodies[req.url ?? ''];
  res.writeHead(b === undefined ? 404 : 200);
  res.end(b ?? '');
});
await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
after(() => server.close());
const permissive = { fetchPolicy: { allowHttp: true, allowPrivateNetwork: true } };

test('version-like tags: same shape, greater, highest', () => {
  const tags = ['1.25-alpine', '1.27-alpine', '1.26-alpine', '1.27.1-alpine', '1.30', 'v1.40-alpine', 'latest', '1.9-alpine', '1.100-alpine-slim'];
  assert.equal(newerVersionTag('1.25-alpine', tags), '1.27-alpine');
  assert.equal(newerVersionTag('1.27-alpine', tags), undefined);
  assert.equal(newerVersionTag('1.9', ['1.10', '1.09', '1.11', '2', 'x']), '1.11');
  assert.equal(newerVersionTag('v1', ['v2', 'v10', '3']), 'v10');
  assert.equal(newerVersionTag('1.2.3', ['1.2.4', '1.3.0', '01.3.1']), '01.3.1');
  assert.equal(newerVersionTag('latest', tags), undefined);
  assert.equal(newerVersionTag('1.0', ['1.00', '1.0']), undefined);
});

test('imports: current, changed, unpinned and errors; nothing is written', async () => {
  bodies['/a.js'] = 'a1';
  bodies['/b.js'] = 'b1';
  const ctx = await makeSite(
    {},
    `FROM scratch\nIMPORT local FROM ./public\nIMPORT a FROM ${base}/a.js INTEGRITY ${sha256('a1')}\nIMPORT b FROM ${base}/b.js\nIMPORT c FROM ${base}/c.js\nIMPORT gone FROM ${base}/gone.js\nEXPORT /\n`,
  );
  const lock = { version: 1, resources: { [`${base}/b.js`]: { integrity: sha256('b1'), size: 2, resolved: `${base}/b.js` } } };
  const lockText = JSON.stringify(lock, null, 2) + '\n';
  await fs.writeFile(path.join(ctx, 'Sitefile.lock'), lockText);
  const r1 = await outdated({ contextDir: ctx, ...permissive });
  assert.deepEqual(
    r1.inputs.map((i) => [i.name, i.status, i.pinnedBy]),
    [
      ['a', 'current', 'integrity'],
      ['b', 'current', 'lockfile'],
      ['c', 'unpinned', null],
      ['gone', 'error', null],
    ],
  );
  assert.equal(r1.inputs[2]?.latest, sha256('c1'));
  assert.deepEqual(r1.inputs[3]?.error, { code: 'E_FETCH_STATUS', message: `GET ${base}/gone.js returned HTTP 404` });
  assert.deepEqual(r1.summary, { current: 2, changed: 0, unpinned: 1, 'digest-only': 0, error: 1 });

  bodies['/a.js'] = 'a2';
  bodies['/b.js'] = 'b2';
  const r2 = await outdated({ contextDir: ctx, ...permissive });
  assert.deepEqual(r2.inputs.map((i) => i.status), ['changed', 'changed', 'unpinned', 'error']);
  assert.equal(r2.inputs[1]?.latest, sha256('b2'));
  // never modifies the lockfile
  assert.equal(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8'), lockText);
  // --no-lockfile: only INTEGRITY counts
  const r3 = await outdated({ contextDir: ctx, ...permissive, lockfile: false });
  assert.equal(r3.inputs[1]?.status, 'unpinned');

  const text = formatOutdated(r2);
  const h = (s: string): string => sha256(s).slice(7, 19);
  assert.equal(
    text,
    [
      `KIND    NAME  SOURCE${' '.repeat(base.length + 8 - 6)}  PINNED        BY         LATEST        STATUS    NEWER TAG`,
      `import  a     ${base}/a.js     ${h('a1')}  integrity  ${h('a2')}  changed   -`,
      `import  b     ${base}/b.js     ${h('b1')}  lockfile   ${h('b2')}  changed   -`,
      `import  c     ${base}/c.js     -             -          ${h('c1')}  unpinned  -`,
      `import  gone  ${base}/gone.js  -             -          -             error     -`,
      `gone: error[E_FETCH_STATUS]: GET ${base}/gone.js returned HTTP 404`,
      '4 remote inputs: 2 changed, 1 unpinned, 1 error',
      '',
    ].join('\n'),
  );
});

test('CLI: text and --json; no remote inputs', async () => {
  const home = await tmpDir();
  bodies['/a.js'] = 'a1';
  const ctx = await makeSite({}, `FROM scratch\nIMPORT a FROM ${base}/a.js INTEGRITY ${sha256('a1')}\nEXPORT /\n`);
  const t = await runCli(home, ['outdated', ctx, '--allow-http', '--allow-private-network']);
  assert.equal(t.code, 0, t.stderr);
  assert.match(t.stdout, /\n1 remote input: 1 current\n$/);
  const j = await runCli(home, ['outdated', ctx, '--allow-http', '--allow-private-network', '--json']);
  assert.equal(j.code, 0, j.stderr);
  assert.deepEqual(JSON.parse(j.stdout), {
    inputs: [{ kind: 'import', name: 'a', source: `${base}/a.js`, pin: sha256('a1'), pinnedBy: 'integrity', latest: sha256('a1'), status: 'current', newerTag: null, error: null }],
    summary: { current: 1, changed: 0, unpinned: 0, 'digest-only': 0, error: 0 },
  });
  // the default network policy applies: a loopback http URL is an error row, not a crash
  const blocked = await runCli(home, ['outdated', ctx]);
  assert.equal(blocked.code, 0);
  assert.match(blocked.stdout, /a: error\[E_PROTOCOL\]/);
  const none = await runCli(home, ['outdated', await makeSite()]);
  assert.equal(none.stdout, 'No remote inputs: the Sitefile has no remote IMPORT, no registry FROM image and no BASE image\n');
  await assert.rejects(fs.stat(path.join(home, 'index.json')), 'the image store was not touched');
});

test('BASE: a moved tag, a digest pin, digest-only references and newer version tags', async () => {
  const reg = await startMockRegistry({ tagPageSize: 2 });
  try {
    const img = (n: string) => ({ layers: [{ '/srv/v.txt': n }], config: { Cmd: [n] } });
    const v1 = reg.seedImage('lib/base', '1.0.0', img('1'));
    reg.seedImage('lib/base', '1.1.0', img('2'));
    reg.seedImage('lib/base', '1.10.0', img('3'));
    reg.seedImage('lib/base', '1.2.0-alpine', img('4'));
    reg.seedImage('lib/base', 'latest', img('5'));
    const ref = `${reg.host}/lib/base:1.0.0`;
    const ctx = await makeSite({}, `FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci BASE ${ref}\n`);
    const unlocked = await outdated({ contextDir: ctx });
    assert.deepEqual(unlocked.inputs[0], {
      kind: 'base', name: 'BASE', source: ref, pin: null, pinnedBy: null, latest: v1.manifestDigest, status: 'unpinned', newerTag: '1.10.0', error: null,
    });
    await fs.writeFile(path.join(ctx, 'Sitefile.lock'), JSON.stringify({ version: 1, resources: {}, images: { [ref]: { digest: v1.manifestDigest } } }));
    assert.equal((await outdated({ contextDir: ctx })).inputs[0]?.status, 'current');
    const moved = reg.seedImage('lib/base', '1.0.0', img('moved'));
    const r = await outdated({ contextDir: ctx });
    assert.equal(r.inputs[0]?.status, 'changed');
    assert.equal(r.inputs[0]?.latest, moved.manifestDigest);
    assert.equal(r.inputs[0]?.pin, v1.manifestDigest);
    // a tag with a digest pin is compared with the digest
    const pinned = await outdated({ contextDir: ctx, lockfile: false, baseImage: `${ref}@${v1.manifestDigest}` });
    assert.deepEqual([pinned.inputs[0]?.status, pinned.inputs[0]?.pinnedBy, pinned.inputs[0]?.source], ['changed', 'digest', `${ref}@${v1.manifestDigest}`]);
    // digest-only: nothing to follow, no request
    const n = reg.requests.length;
    const only = await outdated({ contextDir: ctx, baseImage: `${reg.host}/lib/base@${v1.manifestDigest}` });
    assert.deepEqual([only.inputs[0]?.status, only.inputs[0]?.latest], ['digest-only', null]);
    assert.equal(reg.requests.length, n);
    // a non-version tag has no newer tag; a missing one is an error row
    assert.equal((await outdated({ contextDir: ctx, baseImage: `${reg.host}/lib/base:latest` })).inputs[0]?.newerTag, null);
    const missing = await outdated({ contextDir: ctx, baseImage: `${reg.host}/lib/nope:1` });
    assert.equal(missing.inputs[0]?.status, 'error');
    assert.equal(missing.inputs[0]?.error?.code, 'E_NOT_FOUND');
    // tag pagination follows Link headers
    assert.deepEqual(await new RegistryClient(reg.host).listTags('lib/base'), ['1.0.0', '1.1.0', '1.10.0', '1.2.0-alpine', 'latest']);
  } finally {
    await reg.close();
  }
});
