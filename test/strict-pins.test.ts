import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { sha256 } from '../src/digest.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';
import { startMockRegistry } from './helpers/mock-registry.js';
import { extractArchive } from '../src/resources/archive.js';

// --strict-pins (issue #95): every remote input needs a pin; checked before anything is fetched.

let requests = 0;
const server: Server = createServer((req, res) => {
  requests++;
  if (req.url === '/lib.js') {
    res.writeHead(200, { 'content-type': 'text/javascript' });
    res.end('remote');
  } else {
    res.writeHead(404);
    res.end();
  }
});
await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
after(() => server.close());

const permissive = { fetchPolicy: { allowHttp: true, allowPrivateNetwork: true } };
const cliNet = ['--allow-http', '--allow-private-network'];

async function site(sitefile: string): Promise<string> {
  return makeSite({}, sitefile);
}

test('library: an unpinned remote IMPORT fails with E_UNPINNED at its line, before any fetch', async () => {
  const ctx = await site(`FROM scratch\nCOPY ./public/ /\nIMPORT lib FROM ${base}/lib.js\nEXPORT /\n`);
  const before = requests;
  await assert.rejects(build({ contextDir: ctx, ...permissive, strictPins: true }), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_UNPINNED'), String(e));
    assert.equal(e.span?.line, 3);
    assert.match(e.message, /IMPORT lib: '.*\/lib\.js' is not pinned \(--strict-pins\); add INTEGRITY sha256:<hex>, or record it in Sitefile\.lock/);
    return true;
  });
  assert.equal(requests, before, 'nothing was fetched');
  // without --strict-pins the same build succeeds (and records the URL)...
  const r = await build({ contextDir: ctx, ...permissive });
  assert.ok(r.lockfileWritten);
  // ...after which the lockfile entry is a pin
  await build({ contextDir: ctx, ...permissive, strictPins: true });
});

test('library: INTEGRITY is a pin; --no-lockfile leaves only INTEGRITY', async () => {
  const pinned = await site(`FROM scratch\nIMPORT lib FROM ${base}/lib.js INTEGRITY ${sha256('remote')}\nCOPY lib: /lib.js\nEXPORT /\n`);
  const opts: BuildOptions = { contextDir: pinned, ...permissive, strictPins: true, lockfile: false };
  assert.equal((await build(opts)).output.size, 1);
  const locked = await site(`FROM scratch\nIMPORT lib FROM ${base}/lib.js\nCOPY lib: /lib.js\nEXPORT /\n`);
  await build({ contextDir: locked, ...permissive });
  await build({ contextDir: locked, ...permissive, strictPins: true });
  await assert.rejects(build({ contextDir: locked, ...permissive, strictPins: true, lockfile: false }), (e) => isSitefileError(e, 'E_UNPINNED'));
});

test('local imports never need a pin', async () => {
  const ctx = await site('FROM scratch\nIMPORT p FROM ./public\nCOPY p: /\nEXPORT /\n');
  assert.equal((await build({ contextDir: ctx, lockfile: false, strictPins: true })).output.size, 3);
});

test('CLI: --strict-pins on build, inspect and validate; conflicts with --update-lock', async () => {
  const home = await tmpDir();
  const ctx = await site(`FROM scratch\nIMPORT lib FROM ${base}/lib.js\nCOPY lib: /lib.js\nEXPORT /\n`);
  const out = path.join(await tmpDir(), 'dist');
  for (const cmd of ['build', 'inspect', 'validate']) {
    const r = await runCli(home, [cmd, ctx, ...(cmd === 'build' ? ['-o', out] : []), ...cliNet, '--strict-pins']);
    assert.equal(r.code, 1, `${cmd}: ${r.stderr}`);
    assert.match(r.stderr, /^error\[E_UNPINNED\]: .*Sitefile:2:1: IMPORT lib: '.*' is not pinned \(--strict-pins\)/m, cmd);
  }
  const u = await runCli(home, ['build', ctx, '-o', out, '--strict-pins', '--update-lock']);
  assert.equal(u.code, 2);
  assert.match(u.stderr, /--strict-pins and --update-lock conflict/);
  // record it, then every command passes
  assert.equal((await runCli(home, ['build', ctx, '-o', out, ...cliNet])).code, 0);
  for (const cmd of ['build', 'inspect', 'validate']) {
    const r = await runCli(home, [cmd, ctx, ...(cmd === 'build' ? ['-o', out, '--force'] : []), ...cliNet, '--strict-pins']);
    assert.equal(r.code, 0, `${cmd}: ${r.stderr}`);
  }
});

test('BASE images: a tag needs a lockfile entry, a digest pin needs none', async () => {
  const reg = await startMockRegistry();
  try {
    const seeded = reg.seedImage('lib/base', '1', { layers: [{ '/srv/base.txt': 'base' }], config: { Cmd: ['serve'] } });
    const home = await tmpDir();
    const ctx = await site(`FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci BASE ${reg.host}/lib/base:1\n`);
    const out = () => path.join(ctx, `o${Math.random()}.oci.tar`);
    const before = reg.requests.length;
    for (const cmd of ['build', 'validate']) {
      const r = await runCli(home, [cmd, ctx, ...(cmd === 'build' ? ['-o', out()] : []), '--strict-pins']);
      assert.equal(r.code, 1, r.stderr);
      assert.match(
        r.stderr,
        new RegExp(`^error\\[E_UNPINNED\\]: BASE ${reg.host.replace(/\./g, '\\.')}/lib/base:1: '${reg.host.replace(/\./g, '\\.')}/lib/base:1' is not pinned \\(--strict-pins\\); pin it with @sha256:\\.\\.\\., or record it in Sitefile\\.lock`, 'm'),
      );
    }
    assert.equal(reg.requests.length, before, 'the registry was not contacted');
    // a non-oci export never resolves the BASE, so it is not checked
    const dir = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'plain'), '--format', 'directory', '--strict-pins']);
    assert.equal(dir.code, 0, dir.stderr);
    // record the tag, then strict builds pass
    assert.equal((await runCli(home, ['build', ctx, '-o', out()])).code, 0);
    assert.equal((await runCli(home, ['build', ctx, '-o', out(), '--strict-pins'])).code, 0);
    // a digest pin: no lockfile needed
    const pinned = await site(`FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci BASE ${reg.host}/lib/base:1@${seeded.manifestDigest}\n`);
    const r = await runCli(home, ['build', pinned, '-o', path.join(pinned, 'o.oci.tar'), '--strict-pins', '--no-lockfile']);
    assert.equal(r.code, 0, r.stderr);
    // --base is checked too
    const r2 = await runCli(home, ['build', pinned, '-o', path.join(pinned, 'p.oci.tar'), '--strict-pins', '--base', `${reg.host}/lib/base:2`]);
    assert.equal(r2.code, 1);
    assert.match(r2.stderr, /E_UNPINNED/);
    await assert.rejects(fs.stat(path.join(pinned, 'Sitefile.lock')));
  } finally {
    await reg.close();
  }
});

test('composes: --target checks every IMPORT, graph and why check pins, provenance records them', async () => {
  const reg = await startMockRegistry();
  try {
    const seeded = reg.seedImage('lib/base', '1', { layers: [{ '/etc/motd': 'base\n' }], config: { Cmd: ['serve'] } });
    const home = await tmpDir();
    const sf = `STAGE early\nCOPY ./public/ /\nSTAGE late FROM early\nIMPORT lib FROM ${base}/lib.js\nCOPY lib: /lib.js\nEXPORT / AS oci BASE ${reg.host}/lib/base:1\n`;
    const ctx = await site(sf);
    // --target stops before the import's stage, but every IMPORT still counts
    const t = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'early'), '--target', 'early', ...cliNet, '--strict-pins']);
    assert.equal(t.code, 1);
    assert.match(t.stderr, /E_UNPINNED.*IMPORT lib/);
    for (const cmd of [['graph', ctx], ['why', '/lib.js', ctx, ...cliNet]]) {
      const r = await runCli(home, [...cmd, '--strict-pins']);
      assert.equal(r.code, 1, cmd.join(' '));
      assert.match(r.stderr, /E_UNPINNED/);
    }
    assert.equal((await runCli(home, ['build', ctx, '-o', path.join(ctx, 'a.oci.tar'), ...cliNet])).code, 0);
    const out = path.join(ctx, 'b.oci.tar');
    const b = await runCli(home, ['build', ctx, '-o', out, ...cliNet, '--strict-pins', '--frozen-lockfile', '--provenance']);
    assert.equal(b.code, 0, b.stderr);
    const files = await extractArchive(new Uint8Array(await fs.readFile(out)), 'x');
    const json = (d: string) => JSON.parse(new TextDecoder().decode(files.get(`/blobs/sha256/${d.slice(7)}`)!.bytes));
    const index = JSON.parse(new TextDecoder().decode(files.get('/index.json')!.bytes));
    const statement = json(json(index.manifests[1].digest).layers[0].digest);
    const def = statement.predicate.buildDefinition;
    assert.equal(def.externalParameters.strictPins, true);
    const deps = def.resolvedDependencies as Array<Record<string, any>>;
    assert.deepEqual(deps.find((d) => d.name === 'import:lib')?.annotations, { 'dev.sitefile.pin': 'lockfile' });
    assert.deepEqual(deps.find((d) => d.name === 'base'), {
      name: 'base',
      uri: `${reg.host}/lib/base:1`,
      digest: { sha256: seeded.manifestDigest.slice(7) },
      annotations: { 'dev.sitefile.pin': 'lockfile' },
    });
    // without --strict-pins the provenance is unchanged
    const plain = path.join(ctx, 'c.oci.tar');
    assert.equal((await runCli(home, ['build', ctx, '-o', plain, ...cliNet, '--provenance'])).code, 0);
    assert.doesNotMatch((await fs.readFile(plain)).toString('latin1'), /strictPins|dev\.sitefile\.pin/);
  } finally {
    await reg.close();
  }
});
