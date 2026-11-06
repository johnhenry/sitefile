import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { Store } from '../src/store.js';
import { repoRoot, runCli, tmpDir } from './helpers/cli.js';

/**
 * Integration tests against a real container engine: `docker` when its server answers, else `podman`.
 * They pull official images (docker.io/library/registry:2 and nginx:alpine) and start containers on
 * 127.0.0.1, and skip with a reason when no engine is reachable so `npm test` passes anywhere.
 */
function run(cmd: string, args: string[], timeout = 300_000): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(cmd, args, { timeout, maxBuffer: 64 << 20 }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

async function detectEngine(): Promise<string | undefined> {
  if (process.env.SITEFILE_NO_ENGINE_TESTS) return undefined;
  const d = await run('docker', ['version', '--format', '{{.Server.Version}}'], 15_000);
  if (d.code === 0 && d.stdout.trim() !== '') return 'docker';
  const p = await run('podman', ['version', '--format', '{{.Server.Version}}'], 15_000);
  if (p.code === 0 && p.stdout.trim() !== '') return 'podman';
  return undefined;
}

const engine = await detectEngine();
const skip = engine ? false : 'no container engine reachable (docker version / podman version failed)';

async function freePort(): Promise<number> {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, '127.0.0.1', () => {
      const p = (s.address() as net.AddressInfo).port;
      s.close(() => resolve(p));
    });
  });
}

async function waitFor(url: string, tries = 40): Promise<Response> {
  let last: unknown;
  for (let i = 0; i < tries; i++) {
    try {
      const r = await fetch(url);
      if (r.ok) return r;
      last = new Error(`HTTP ${r.status}`);
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw last;
}

test('engine: EXPORT / AS oci BASE nginx:alpine runs under the container engine', { skip, timeout: 600_000 }, async () => {
  const e = engine as string;
  const home = await tmpDir();
  const ctx = path.join(repoRoot, 'examples', 'hello');
  const out = path.join(await tmpDir(), 'out.oci.tar');
  const tag = 'sitefile-test/hello:1';
  const name = 'sitefile-test-hello';
  const lockDir = await tmpDir();
  // examples/hello must stay untouched: build a copy-free variant with an explicit Sitefile and no lockfile
  const sitefile = path.join(lockDir, 'Sitefile');
  await fs.writeFile(
    sitefile,
    (await fs.readFile(path.join(ctx, 'Sitefile'), 'utf8')).replace('EXPORT / AS directory', 'EXPORT / AS oci BASE docker.io/library/nginx:alpine AT /usr/share/nginx/html'),
  );
  try {
    const b = await runCli(home, ['build', ctx, '-f', sitefile, '-o', out, '-t', tag, '--no-lockfile']);
    assert.equal(b.code, 0, b.stderr);
    const load = await run(e, ['load', '-i', out]);
    assert.equal(load.code, 0, load.stderr);
    const port = await freePort();
    const up = await run(e, ['run', '-d', '--name', name, '-p', `127.0.0.1:${port}:80`, tag]);
    assert.equal(up.code, 0, up.stderr);
    const res = await waitFor(`http://127.0.0.1:${port}/`);
    assert.match(await res.text(), /Welcome to Hello/);
    const css = await waitFor(`http://127.0.0.1:${port}/style.css`);
    assert.equal(css.status, 200);
  } finally {
    await run(e, ['rm', '-f', name]);
    await run(e, ['rmi', '-f', tag]);
  }
});

test('engine: push to a real registry:2 container, delete locally, pull back with the same digest', { skip, timeout: 600_000 }, async () => {
  const e = engine as string;
  const port = await freePort();
  const name = 'sitefile-test-registry';
  await run(e, ['rm', '-f', name]);
  try {
    const up = await run(e, ['run', '-d', '--rm', '-p', `127.0.0.1:${port}:5000`, '--name', name, 'docker.io/library/registry:2']);
    assert.equal(up.code, 0, up.stderr);
    await waitFor(`http://127.0.0.1:${port}/v2/`);
    const home = await tmpDir();
    const ctx = path.join(repoRoot, 'examples', 'blog');
    const remote = `localhost:${port}/test/blog:1`;
    const b = await runCli(home, ['build', ctx, '-t', 'blog:1']);
    assert.equal(b.code, 0, b.stderr);
    const store = await Store.open(home);
    const id = await store.resolve('blog:1');
    assert.equal((await runCli(home, ['tag', 'blog:1', remote])).code, 0);
    const p = await runCli(home, ['push', remote]);
    assert.equal(p.code, 0, p.stderr);
    assert.match(p.stdout, new RegExp(`digest: ${id}`));
    // the registry itself agrees on the digest
    const head = await fetch(`http://127.0.0.1:${port}/v2/test/blog/manifests/1`, { headers: { accept: 'application/vnd.oci.image.manifest.v1+json' } });
    assert.equal(head.headers.get('docker-content-digest'), id);
    // delete locally (tag, image, blobs), then pull into the same store
    assert.equal((await runCli(home, ['rmi', '-f', id])).code, 0);
    assert.equal((await runCli(home, ['image', 'prune'])).code, 0);
    assert.equal(await store.hasImage(id), false);
    const pl = await runCli(home, ['pull', remote]);
    assert.equal(pl.code, 0, pl.stderr);
    assert.equal(await store.resolve(remote), id);
    assert.ok((await store.loadTree(id)).get('/index.html'));

    // attestations: registry:2 has no referrers API, so they are listed by the sha256-<hex> fallback tag
    const att = `localhost:${port}/test/att:1`;
    const ba = await runCli(home, ['build', ctx, '-t', att, '--provenance', '--sbom']);
    assert.equal(ba.code, 0, ba.stderr);
    assert.equal(await store.resolve(att), id, 'attestations do not change the image ID');
    const pa = await runCli(home, ['push', att]);
    assert.equal(pa.code, 0, pa.stderr);
    const idx = await fetch(`http://127.0.0.1:${port}/v2/test/att/manifests/sha256-${id.slice(7)}`, { headers: { accept: 'application/vnd.oci.image.index.v1+json' } });
    assert.equal(idx.status, 200);
    const listed = ((await idx.json()) as { manifests: Array<{ digest: string; artifactType?: string }> }).manifests;
    assert.deepEqual(listed.map((m) => m.digest), (await store.referrerDescriptors(id)).map((d) => d.digest));
    for (const m of listed) {
      assert.equal(m.artifactType, 'application/vnd.in-toto+json');
      const r = await fetch(`http://127.0.0.1:${port}/v2/test/att/manifests/${m.digest}`, { headers: { accept: 'application/vnd.oci.image.manifest.v1+json' } });
      assert.equal(r.status, 200);
      assert.equal(((await r.json()) as { subject: { digest: string } }).subject.digest, id);
    }
  } finally {
    await run(e, ['rm', '-f', name]);
  }
});

test('engine: a saved archive loads by name (docker image inspect) and its files are byte-identical', { skip, timeout: 300_000 }, async () => {
  const e = engine as string;
  const home = await tmpDir();
  const ctx = path.join(repoRoot, 'examples', 'blog');
  const tag = 'sitefile-test-blog:1';
  const cname = 'sitefile-test-blog-copy';
  await run(e, ['rm', '-f', cname]);
  try {
    assert.equal((await runCli(home, ['build', ctx, '-t', tag])).code, 0);
    const tarFile = path.join(await tmpDir(), 'blog.oci.tar');
    assert.equal((await runCli(home, ['save', tag, '-o', tarFile])).code, 0);
    const id = await (await Store.open(home)).resolve(tag);
    const load = await run(e, ['load', '-i', tarFile]);
    assert.equal(load.code, 0, load.stderr);
    const insp = await run(e, ['image', 'inspect', tag, '--format', '{{.Id}}']);
    assert.equal(insp.code, 0, insp.stderr);
    assert.match(insp.stdout.trim(), new RegExp(id.slice(7)));
    const created = await run(e, ['create', '--name', cname, tag, '/none']);
    assert.equal(created.code, 0, created.stderr);
    const copied = await tmpDir();
    const cp = await run(e, ['cp', `${cname}:/srv/www/.`, copied]);
    assert.equal(cp.code, 0, cp.stderr);
    const fresh = path.join(await tmpDir(), 'fresh');
    assert.equal((await runCli(home, ['build', ctx, '-o', fresh])).code, 0);
    const list = async (d: string): Promise<string[]> =>
      (await fs.readdir(d, { recursive: true, withFileTypes: true })).filter((x) => x.isFile()).map((x) => path.relative(d, path.join(x.parentPath, x.name))).sort();
    const names = await list(fresh);
    assert.deepEqual(await list(copied), names);
    for (const n of names) assert.deepEqual(await fs.readFile(path.join(copied, n)), await fs.readFile(path.join(fresh, n)), n);
  } finally {
    await run(e, ['rm', '-f', cname]);
    await run(e, ['rmi', '-f', tag]);
  }
});
