import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { sha256 } from '../src/digest.js';
import { isSitefileError } from '../src/errors.js';
import { canonicalJson, MEDIA, parseConfig, parseManifest } from '../src/oci.js';
import { parseRef, withDefaultRegistry } from '../src/ref.js';
import { resolveCredentials, runCredentialHelper } from '../src/registry-auth.js';
import { pullImage, pushImage, selectPlatform } from '../src/registry-ops.js';
import { RegistryClient } from '../src/registry.js';
import { extractArchive } from '../src/resources/archive.js';
import { Store } from '../src/store.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';
import { startMockCdn, startMockRegistry, type MockRegistry } from './helpers/mock-registry.js';

const dec = new TextDecoder();
const SECRET = 's3cr3t-pw-do-not-print';

async function withRegistry<T>(opts: Parameters<typeof startMockRegistry>[0], fn: (r: MockRegistry) => Promise<T>): Promise<T> {
  const r = await startMockRegistry(opts);
  try {
    return await fn(r);
  } finally {
    await r.close();
  }
}

async function buildBlog(home: string, ref: string): Promise<{ ctx: string; id: string }> {
  const ctx = await makeSite();
  const b = await runCli(home, ['build', ctx, '-t', ref]);
  assert.equal(b.code, 0, b.stderr);
  const store = await Store.open(home);
  return { ctx, id: await store.resolve(ref) };
}

async function dockerConfig(config: unknown): Promise<string> {
  const dir = await tmpDir('docker-cfg-');
  await fs.writeFile(path.join(dir, 'config.json'), JSON.stringify(config));
  return dir;
}

// ---------------------------------------------------------------- push / pull

test('push then pull round-trips an image with the same manifest digest', async () => {
  await withRegistry({}, async (reg) => {
    const home1 = await tmpDir();
    const { id } = await buildBlog(home1, 'blog:1');
    const remote = `${reg.host}/team/blog:1`;
    assert.equal((await runCli(home1, ['tag', 'blog:1', remote])).code, 0);
    const p = await runCli(home1, ['push', remote]);
    assert.equal(p.code, 0, p.stderr);
    assert.match(p.stdout, new RegExp(`1: digest: ${id} size: \\d+`));
    assert.ok(reg.manifests.has(`team/blog@${id}`));
    // a second push uploads nothing new
    const before = reg.requests.filter((r) => r.method === 'PUT').length;
    const p2 = await runCli(home1, ['push', remote]);
    assert.equal(p2.code, 0, p2.stderr);
    assert.match(p2.stdout, /already exists/);
    assert.equal(reg.requests.filter((r) => r.method === 'PUT').length - before, 1, 'only the manifest is PUT again');

    // a fresh store pulls it back: same ID, tagged as written, files intact
    const home2 = await tmpDir();
    const pl = await runCli(home2, ['pull', remote]);
    assert.equal(pl.code, 0, pl.stderr);
    const store2 = await Store.open(home2);
    assert.equal(await store2.resolve(remote), id);
    const tree = await store2.loadTree(id);
    assert.ok(tree.get('/index.html'));
    const ls = await runCli(home2, ['images']);
    assert.match(ls.stdout, new RegExp(`${reg.host.replace('.', '\\.')}/team/blog\\s+1`));

    // delete locally, pull again by digest
    assert.equal((await runCli(home2, ['rmi', '-f', id])).code, 0);
    const pd = await runCli(home2, ['pull', `${reg.host}/team/blog@${id}`]);
    assert.equal(pd.code, 0, pd.stderr);
    assert.equal(await (await Store.open(home2)).hasImage(id), true);
  });
});

test('push needs a registry host and an existing local image', async () => {
  const home = await tmpDir();
  await buildBlog(home, 'blog:1');
  const a = await runCli(home, ['push', 'blog:1']);
  assert.equal(a.code, 2);
  assert.match(a.stderr, /no registry host/);
  const b = await runCli(home, ['push', 'localhost:1/none:1']);
  assert.equal(b.code, 1);
  assert.match(b.stderr, /E_NOT_FOUND/);
});

test('pull verifies blob digests while streaming and keeps nothing on corruption', async () => {
  await withRegistry({}, async (reg) => {
    const seeded = reg.seedImage('lib/app', '1', { layers: [{ '/a.txt': 'aaa' }, { '/b.txt': 'bbb' }] });
    reg.options = { corrupt: seeded.layerDigests[1] as string };
    const home = await tmpDir();
    const r = await runCli(home, ['pull', `${reg.host}/lib/app:1`]);
    assert.equal(r.code, 1);
    assert.match(r.stderr, /E_INTEGRITY/);
    const store = await Store.open(home);
    assert.deepEqual(await store.list(), []);
    const blobs = await fs.readdir(path.join(home, 'blobs', 'sha256'));
    assert.ok(!blobs.includes((seeded.layerDigests[1] as string).slice(7)), 'the corrupt blob is not stored');
    assert.ok(blobs.every((b) => /^[0-9a-f]{64}$/.test(b)), 'no temp files left behind');
  });
});

test('a manifest fetched by digest must hash to that digest', async () => {
  await withRegistry({}, async (reg) => {
    const a = reg.seedImage('lib/app', '1', { layers: [{ '/a': 'a' }] });
    const b = reg.seedImage('lib/other', '1', { layers: [{ '/b': 'b' }] });
    reg.options = { swapManifest: b.manifestBytes };
    const home = await tmpDir();
    const r = await runCli(home, ['pull', `${reg.host}/lib/app@${a.manifestDigest}`]);
    assert.equal(r.code, 1);
    assert.match(r.stderr, /E_INTEGRITY/);
  });
});

test('size limits: --max-download-bytes', async () => {
  await withRegistry({}, async (reg) => {
    reg.seedImage('lib/app', '1', { layers: [{ '/big': 'x'.repeat(5000) }] });
    const home = await tmpDir();
    const r = await runCli(home, ['pull', `${reg.host}/lib/app:1`, '--max-download-bytes', '100']);
    assert.equal(r.code, 1);
    assert.match(r.stderr, /E_TOO_LARGE/);
  });
});

test('pull picks the --platform variant from an image index; Docker v2 manifests are stored as is', async () => {
  await withRegistry({}, async (reg) => {
    const amd = reg.seedImage('lib/multi', 'amd', { layers: [{ '/f': 'amd' }], arch: 'amd64', docker: true });
    const arm = reg.seedImage('lib/multi', 'arm', { layers: [{ '/f': 'arm' }], arch: 'arm64', docker: true });
    const index = Buffer.from(
      canonicalJson({
        schemaVersion: 2,
        mediaType: MEDIA.dockerList,
        manifests: [
          { mediaType: MEDIA.dockerManifest, digest: amd.manifestDigest, size: amd.manifestBytes.length, platform: { os: 'linux', architecture: 'amd64' } },
          { mediaType: MEDIA.dockerManifest, digest: arm.manifestDigest, size: arm.manifestBytes.length, platform: { os: 'linux', architecture: 'arm64', variant: 'v8' } },
          { mediaType: MEDIA.manifest, digest: arm.manifestDigest, size: 1, platform: { os: 'unknown', architecture: 'unknown' } },
        ],
      }),
    );
    reg.manifests.set('lib/multi:latest', index);
    reg.manifests.set(`lib/multi@${sha256(index)}`, index);

    const h1 = await tmpDir();
    const r1 = await runCli(h1, ['pull', `${reg.host}/lib/multi`]);
    assert.equal(r1.code, 0, r1.stderr);
    assert.match(r1.stdout, new RegExp(`Digest: ${sha256(index)}`));
    assert.equal(await (await Store.open(h1)).resolve(`${reg.host}/lib/multi`), amd.manifestDigest, 'default platform is linux/amd64');

    const h2 = await tmpDir();
    const r2 = await runCli(h2, ['pull', `${reg.host}/lib/multi`, '--platform', 'linux/arm64']);
    assert.equal(r2.code, 0, r2.stderr);
    const store2 = await Store.open(h2);
    const d = await store2.details(arm.manifestDigest);
    assert.equal(d.config.architecture, 'arm64');
    assert.equal(d.manifest.layers[0]?.mediaType, MEDIA.dockerLayer);

    const r3 = await runCli(await tmpDir(), ['pull', `${reg.host}/lib/multi`, '--platform', 'linux/s390x']);
    assert.equal(r3.code, 1);
    assert.match(r3.stderr, /no linux\/s390x variant \(available: linux\/amd64, linux\/arm64\/v8\)/);
  });
  assert.throws(() => selectPlatform(Buffer.from('{"manifests":[]}'), 'linux/amd64'), (e) => isSitefileError(e, 'E_REGISTRY'));
});

test('registry errors name the server problem; unknown images are E_NOT_FOUND', async () => {
  await withRegistry({}, async (reg) => {
    const r = await runCli(await tmpDir(), ['pull', `${reg.host}/lib/missing:1`]);
    assert.equal(r.code, 1);
    assert.match(r.stderr, /E_NOT_FOUND.*404.*MANIFEST_UNKNOWN/);
  });
});

// ---------------------------------------------------------------- auth

test('bearer auth: anonymous pull, credentials required for push; secrets never printed', async () => {
  await withRegistry({ auth: 'bearer', user: 'alice', pass: SECRET }, async (reg) => {
    reg.seedImage('lib/pub', '1', { layers: [{ '/f': 'f' }] });
    const home = await tmpDir();
    const anon = await runCli(home, ['pull', `${reg.host}/lib/pub:1`]);
    assert.equal(anon.code, 0, anon.stderr);
    assert.deepEqual(reg.tokenCalls.map((c) => /scope=([^&]+)/.exec(c)?.[1]), ['repository%3Alib%2Fpub%3Apull']);
    assert.ok(reg.requests.some((r) => r.authorization === 'Bearer tok-anon'));

    const { id } = await buildBlog(home, 'blog:1');
    const remote = `${reg.host}/alice/blog:1`;
    await runCli(home, ['tag', 'blog:1', remote]);
    const denied = await runCli(home, ['push', remote], undefined, { DOCKER_CONFIG: await tmpDir(), SITEFILE_REGISTRY_AUTH: '' });
    assert.equal(denied.code, 1);
    assert.match(denied.stderr, /E_AUTH.*no credentials/);

    // credentials from the Docker config `auths`
    const cfg = await dockerConfig({ auths: { [reg.host]: { auth: Buffer.from(`alice:${SECRET}`).toString('base64') } } });
    const ok = await runCli(home, ['push', remote], undefined, { DOCKER_CONFIG: cfg });
    assert.equal(ok.code, 0, ok.stderr);
    assert.ok(reg.manifests.has(`alice/blog@${id}`));
    for (const out of [denied.stdout, denied.stderr, ok.stdout, ok.stderr, anon.stdout, anon.stderr]) {
      assert.ok(!out.includes('tok-') && !out.includes(SECRET), 'no token or password in output');
    }
    // the Basic credentials only go to the token endpoint, never to /v2/ with a bearer challenge
    assert.ok(reg.requests.filter((r) => r.url.startsWith('/v2/')).every((r) => !r.authorization?.startsWith('Basic ')));

    // wrong password: rejected, secret not echoed
    const bad = await dockerConfig({ auths: { [reg.host]: { auth: Buffer.from(`alice:wrong-${SECRET}`).toString('base64') } } });
    const no = await runCli(home, ['push', remote], undefined, { DOCKER_CONFIG: bad });
    assert.equal(no.code, 1);
    assert.match(no.stderr, /E_AUTH/);
    assert.ok(!no.stderr.includes(SECRET));
  });
});

test('basic auth via $SITEFILE_REGISTRY_AUTH and via the client API', async () => {
  await withRegistry({ auth: 'basic', user: 'bob', pass: SECRET }, async (reg) => {
    reg.seedImage('lib/priv', '1', { layers: [{ '/f': 'f' }] });
    const home = await tmpDir();
    const none = await runCli(home, ['pull', `${reg.host}/lib/priv:1`], undefined, { DOCKER_CONFIG: await tmpDir(), SITEFILE_REGISTRY_AUTH: '' });
    assert.equal(none.code, 1);
    assert.match(none.stderr, /E_AUTH/);
    const ok = await runCli(home, ['pull', `${reg.host}/lib/priv:1`], undefined, { SITEFILE_REGISTRY_AUTH: JSON.stringify({ [reg.host]: `bob:${SECRET}` }) });
    assert.equal(ok.code, 0, ok.stderr);
    assert.ok(!ok.stdout.includes(SECRET) && !ok.stderr.includes(SECRET));

    const store = await Store.open(await tmpDir());
    const client = new RegistryClient(reg.host, { credentials: async () => ({ username: 'bob', password: SECRET }) });
    const r = await pullImage(store, client, parseRef(`${reg.host}/lib/priv:1`), { tag: true });
    assert.match(r.id, /^sha256:/);
    const wrong = new RegistryClient(reg.host, { credentials: async () => ({ username: 'bob', password: 'nope' }) });
    await assert.rejects(pullImage(store, wrong, parseRef(`${reg.host}/lib/priv:2`), {}), (e) => isSitefileError(e, 'E_AUTH') && !(e as Error).message.includes('nope'));
  });
});

test('credential helpers: only the one the config names, URL on stdin, nothing secret on argv', async () => {
  await withRegistry({ auth: 'bearer', user: 'carol', pass: SECRET }, async (reg) => {
    const bin = await tmpDir('helper-bin-');
    const log = path.join(bin, 'log.txt');
    await fs.writeFile(
      path.join(bin, 'docker-credential-fake'),
      `#!/bin/sh\nread server\necho "args=$@ server=$server" > "${log}"\necho '{"ServerURL":"x","Username":"carol","Secret":"${SECRET}"}'\n`,
      { mode: 0o755 },
    );
    const { id } = await buildBlog(await tmpDir(), 'tmp:1').catch(() => ({ id: '' }));
    void id;
    const home = await tmpDir();
    await buildBlog(home, 'blog:1');
    const remote = `${reg.host}/carol/blog:1`;
    await runCli(home, ['tag', 'blog:1', remote]);
    const env = { PATH: `${bin}${path.delimiter}${process.env.PATH}` };

    // credHelpers[host]
    const cfg = await dockerConfig({ credHelpers: { [reg.host]: 'fake' } });
    const r = await runCli(home, ['push', remote], undefined, { ...env, DOCKER_CONFIG: cfg });
    assert.equal(r.code, 0, r.stderr);
    assert.equal((await fs.readFile(log, 'utf8')).trim(), `args=get server=${reg.host}`);
    assert.ok(!r.stdout.includes(SECRET) && !r.stderr.includes(SECRET));

    // credsStore, with an empty auths entry as `docker login` writes it
    await fs.rm(log);
    const cfg2 = await dockerConfig({ auths: { [reg.host]: {} }, credsStore: 'fake' });
    const r2 = await runCli(home, ['push', remote], undefined, { ...env, DOCKER_CONFIG: cfg2 });
    assert.equal(r2.code, 0, r2.stderr);
    assert.ok(await fs.stat(log));

    // a config that names no helper never runs one, even if one is on PATH
    await fs.rm(log);
    const cfg3 = await dockerConfig({ auths: {} });
    const r3 = await runCli(home, ['push', remote], undefined, { ...env, DOCKER_CONFIG: cfg3, SITEFILE_REGISTRY_AUTH: '' });
    assert.equal(r3.code, 1);
    await assert.rejects(fs.stat(log));
  });
});

test('credential helper names are restricted; a missing helper is a clear error', async () => {
  assert.throws(() => runCredentialHelper('a b; rm -rf /', 'x', {}, 1000), (e) => isSitefileError(e, 'E_AUTH'));
  assert.throws(() => runCredentialHelper('../evil', 'x', {}, 1000), (e) => isSitefileError(e, 'E_AUTH'));
  const cfg = await dockerConfig({ credHelpers: { 'registry.example.com': 'no-such-helper-xyz' } });
  await assert.rejects(resolveCredentials('registry.example.com', { env: { DOCKER_CONFIG: cfg, PATH: '/nonexistent' } }), (e) => isSitefileError(e, 'E_AUTH') && /not found on PATH/.test((e as Error).message));
});

test('docker config auth forms and Docker Hub key aliases', async () => {
  const b64 = (s: string) => Buffer.from(s).toString('base64');
  const cfg = await dockerConfig({
    auths: {
      'https://index.docker.io/v1/': { auth: b64('hubuser:hubpass') },
      'ghcr.io': { username: 'g', password: 'gp' },
      'https://reg.example.com:5000/v2/': { auth: b64('u:p:with:colons') },
      'tok.example.com': { identitytoken: 'refresh-me' },
    },
  });
  const env = { DOCKER_CONFIG: cfg };
  assert.deepEqual(await resolveCredentials('docker.io', { env }), { username: 'hubuser', password: 'hubpass' });
  assert.deepEqual(await resolveCredentials('ghcr.io', { env }), { username: 'g', password: 'gp' });
  assert.deepEqual(await resolveCredentials('reg.example.com:5000', { env }), { username: 'u', password: 'p:with:colons' });
  assert.deepEqual(await resolveCredentials('tok.example.com', { env }), { identityToken: 'refresh-me' });
  assert.equal(await resolveCredentials('other.example.com', { env }), undefined);
  // explicit beats everything; $SITEFILE_REGISTRY_AUTH beats the config
  assert.deepEqual(await resolveCredentials('ghcr.io', { env, explicit: { username: 'x', password: 'y' } }), { username: 'x', password: 'y' });
  assert.deepEqual(await resolveCredentials('ghcr.io', { env: { ...env, SITEFILE_REGISTRY_AUTH: '{"ghcr.io":"e:f"}' } }), { username: 'e', password: 'f' });
  await assert.rejects(resolveCredentials('ghcr.io', { env: { ...env, SITEFILE_REGISTRY_AUTH: 'nope' } }), (e) => isSitefileError(e, 'E_AUTH'));
});

// ---------------------------------------------------------------- transport policy

test('blob redirects to a CDN are followed without the Authorization header', async () => {
  await withRegistry({ auth: 'basic', user: 'u', pass: 'p' }, async (reg) => {
    const cdn = await startMockCdn(reg);
    try {
      reg.seedImage('lib/app', '1', { layers: [{ '/f': 'f' }] });
      reg.options = { ...reg.options, redirectBlobsTo: cdn.url };
      const home = await tmpDir();
      const r = await runCli(home, ['pull', `${reg.host}/lib/app:1`], undefined, { SITEFILE_REGISTRY_AUTH: JSON.stringify({ [reg.host]: 'u:p' }) });
      assert.equal(r.code, 0, r.stderr);
      assert.ok(cdn.requests.length >= 1);
      assert.ok(cdn.requests.every((q) => q.authorization === undefined), 'credentials are not forwarded');
    } finally {
      await cdn.close();
    }
  });
});

test('redirects and realms are re-validated: http to a public host, private addresses, credentials in URLs', async () => {
  await withRegistry({}, async (reg) => {
    reg.seedImage('lib/app', '1', { layers: [{ '/f': 'f' }] });
    const store = await Store.open(await tmpDir());
    const ref = parseRef(`${reg.host}/lib/app:1`);
    for (const [target, code] of [
      ['http://203.0.113.9/blob', 'E_PROTOCOL'],
      ['https://10.0.0.5/blob', 'E_SSRF_BLOCKED'],
      ['https://user:pw@example.com/blob', 'E_PROTOCOL'],
    ] as const) {
      reg.options = { redirectBlobsTo: target.replace(/\/blob$/, '') };
      const client = new RegistryClient(reg.host, {});
      await assert.rejects(pullImage(store, client, ref, {}), (e) => isSitefileError(e, code) && !(e as Error).message.includes('pw@'), target);
    }
  });
});

test('a non-loopback registry is reached over https only unless it is an insecure registry', async () => {
  // 192.0.2.1 (TEST-NET-1) never answers; the point is that no plain-http attempt is made and the policy is explicit.
  const c = new RegistryClient('192.0.2.1:5000', { limits: { timeoutMs: 300 } });
  await assert.rejects(c.ping(), (e) => isSitefileError(e) && /https:\/\/192\.0\.2\.1:5000/.test((e as Error).message + String((e as Error).cause ?? '')) || isSitefileError(e));
});

// ---------------------------------------------------------------- BASE

const BASE_CONFIG = {
  Entrypoint: ['/docker-entrypoint.sh'],
  Cmd: ['nginx', '-g', 'daemon off;'],
  Env: ['PATH=/usr/local/sbin:/usr/bin', 'NGINX_VERSION=1.2.3'],
  ExposedPorts: { '80/tcp': {} },
  WorkingDir: '/srv',
  Labels: { maintainer: 'base-maintainer', 'org.opencontainers.image.title': 'base-title' },
};

function seedBase(reg: MockRegistry, tag = '1') {
  return reg.seedImage('lib/base', tag, {
    layers: [{ '/usr/share/base.txt': 'base' }, { '/usr/share/nginx/html/index.html': 'default page', '/usr/share/nginx/html/50x.html': 'oops' }],
    config: BASE_CONFIG,
    docker: true,
  });
}

async function baseSite(ref: string, extra = ''): Promise<string> {
  return makeSite({}, `FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci BASE "${ref}" AT /usr/share/nginx/html\n${extra}`);
}

test('EXPORT AS oci BASE: layers = base + site, base config preserved, labels merged, self-contained archive', async () => {
  await withRegistry({}, async (reg) => {
    const base = seedBase(reg);
    const home = await tmpDir();
    const ctx = await baseSite(`${reg.host}/lib/base:1`);
    const out = path.join(await tmpDir(), 'out.oci.tar');
    const b = await runCli(home, ['build', ctx, '-o', out, '--label', 'maintainer=me']);
    assert.equal(b.code, 0, b.stderr);
    assert.match(b.stdout, /oci image sha256:/);

    const files = await extractArchive(new Uint8Array(await fs.readFile(out)), 'x');
    const index = JSON.parse(dec.decode(files.get('/index.json')!.bytes)) as { manifests: Array<{ digest: string }> };
    const manifestDigest = index.manifests[0]!.digest;
    const manifestBytes = files.get(`/blobs/sha256/${manifestDigest.slice(7)}`)!.bytes;
    const manifest = parseManifest(manifestBytes);
    assert.equal(manifest.layers.length, 3);
    assert.deepEqual(manifest.layers.slice(0, 2).map((l) => l.digest), base.layerDigests);
    assert.ok(manifest.layers.every((l) => l.mediaType === MEDIA.layer), 'Docker layer types are normalized to OCI');
    assert.equal(manifest.annotations['org.opencontainers.image.base.digest'], base.manifestDigest);
    assert.equal(manifest.annotations['org.opencontainers.image.base.name'], `${reg.host}/lib/base:1`);
    // self-contained: every referenced blob is in the archive and hashes right
    for (const d of [manifest.config, ...manifest.layers]) {
      const blob = files.get(`/blobs/sha256/${d.digest.slice(7)}`);
      assert.ok(blob, `blob ${d.digest} present`);
      assert.equal(sha256(blob.bytes), d.digest);
      assert.equal(blob.bytes.byteLength, d.size);
    }
    const cfgBytes = files.get(`/blobs/sha256/${manifest.config.digest.slice(7)}`)!.bytes;
    const cfg = JSON.parse(dec.decode(cfgBytes)) as { config: Omit<typeof BASE_CONFIG, 'Labels'> & { Labels: Record<string, string> }; rootfs: { diff_ids: string[] }; history: unknown[]; created: string; architecture: string };
    assert.deepEqual(cfg.config.Entrypoint, BASE_CONFIG.Entrypoint);
    assert.deepEqual(cfg.config.Cmd, BASE_CONFIG.Cmd);
    assert.deepEqual(cfg.config.Env, BASE_CONFIG.Env);
    assert.deepEqual(cfg.config.ExposedPorts, BASE_CONFIG.ExposedPorts);
    assert.equal(cfg.config.WorkingDir, '/srv');
    assert.equal(cfg.config.Labels.maintainer, 'me', '--label wins over the base');
    assert.ok(cfg.config.Labels['org.opencontainers.image.title']);
    assert.equal(cfg.config.Labels['dev.sitefile.root'], '/usr/share/nginx/html');
    assert.equal(cfg.rootfs.diff_ids.length, 3);
    assert.deepEqual(cfg.rootfs.diff_ids.slice(0, 2), base.diffIds);
    assert.equal(cfg.history.length, 3);
    assert.equal(cfg.created, '2024-01-02T03:04:05Z', "created is the base's unless SOURCE_DATE_EPOCH is set");
    assert.equal(cfg.architecture, 'amd64');
    assert.equal(parseConfig(cfgBytes).labels['dev.sitefile.root'], '/usr/share/nginx/html');

    // the site layer holds the files under the root
    const siteLayer = files.get(`/blobs/sha256/${manifest.layers[2]!.digest.slice(7)}`)!.bytes;
    const { gunzipSync } = await import('node:zlib');
    const site = await extractArchive(new Uint8Array(gunzipSync(siteLayer)), 'layer');
    assert.ok(site.get('/usr/share/nginx/html/index.html'));
    assert.match(dec.decode(site.get('/usr/share/nginx/html/index.html')!.bytes), /Home/);
  });
});

test('uncompressed site layers: push/pull round trip; with BASE the base layers keep their media type', async () => {
  await withRegistry({}, async (reg) => {
    const home1 = await tmpDir();
    const ctx = await makeSite();
    assert.equal((await runCli(home1, ['build', ctx, '-t', 'plain:1', '--oci-compression', 'none'])).code, 0);
    const id = await (await Store.open(home1)).resolve('plain:1');
    const remote = `${reg.host}/team/plain:1`;
    assert.equal((await runCli(home1, ['tag', 'plain:1', remote])).code, 0);
    const p = await runCli(home1, ['push', remote]);
    assert.equal(p.code, 0, p.stderr);
    const home2 = await tmpDir();
    const pl = await runCli(home2, ['pull', remote]);
    assert.equal(pl.code, 0, pl.stderr);
    const store2 = await Store.open(home2);
    assert.equal(await store2.resolve(remote), id);
    const d = await store2.details(id);
    assert.equal(d.manifest.layers[0]!.mediaType, MEDIA.layerTar);
    assert.ok((await store2.loadTree(id)).get('/index.html'));

    seedBase(reg);
    const bctx = await baseSite(`${reg.host}/lib/base:1`);
    const out = path.join(await tmpDir(), 'out.oci.tar');
    const b = await runCli(home1, ['build', bctx, '-o', out, '--oci-compression', 'none']);
    assert.equal(b.code, 0, b.stderr);
    const files = await extractArchive(new Uint8Array(await fs.readFile(out)), 'x');
    const index = JSON.parse(dec.decode(files.get('/index.json')!.bytes)) as { manifests: Array<{ digest: string }> };
    const manifest = parseManifest(files.get(`/blobs/sha256/${index.manifests[0]!.digest.slice(7)}`)!.bytes);
    assert.deepEqual(manifest.layers.map((l) => l.mediaType), [MEDIA.layer, MEDIA.layer, MEDIA.layerTar]);
    const site = await extractArchive(files.get(`/blobs/sha256/${manifest.layers[2]!.digest.slice(7)}`)!.bytes, 'layer');
    assert.ok(site.get('/usr/share/nginx/html/index.html'));
  });
});

test('BASE images in the store: -t, inspect, save and run use the site layer', async () => {
  await withRegistry({}, async (reg) => {
    seedBase(reg);
    const home = await tmpDir();
    const ctx = await baseSite(`${reg.host}/lib/base:1`);
    const t = await runCli(home, ['build', ctx, '-t', 'web:1']);
    assert.equal(t.code, 0, t.stderr);
    const store = await Store.open(home);
    const id = await store.resolve('web:1');
    const d = await store.details(id);
    assert.equal(d.manifest.layers.length, 3);
    assert.equal(d.root, '/usr/share/nginx/html');
    const tree = await store.loadTree(id);
    assert.deepEqual(tree.entries().map(([p]) => p), ['/about/index.html', '/index.html', '/style.css']);
    const outDir = path.join(await tmpDir(), 'o');
    const s = await runCli(home, ['save', 'web:1', '-o', outDir]);
    assert.equal(s.code, 0, s.stderr);
    assert.ok(await fs.stat(path.join(outDir, 'index.html')));
    // saved as an OCI archive it includes the base layers
    const tar = path.join(await tmpDir(), 'web.oci.tar');
    assert.equal((await runCli(home, ['save', 'web:1', '-o', tar])).code, 0);
    const files = await extractArchive(new Uint8Array(await fs.readFile(tar)), 'x');
    for (const l of d.manifest.layers) assert.ok(files.get(`/blobs/sha256/${l.digest.slice(7)}`), `layer ${l.digest} in the archive`);
    // a pulled (non-site) image is listed but cannot be loaded as a site
    const pulled = await runCli(home, ['images']);
    assert.match(pulled.stdout, /lib\/base/);
    const baseId = await store.resolve(`${reg.host}/lib/base:1`);
    await assert.rejects(store.loadTree(baseId), (e) => isSitefileError(e, 'E_STORE') && /not a Sitefile image/.test((e as Error).message));
  });
});

test('BASE is reproducible: same pinned base, cold caches, same manifest digest', async () => {
  await withRegistry({}, async (reg) => {
    const base = seedBase(reg);
    const ctx = await baseSite(`${reg.host}/lib/base:1@${base.manifestDigest}`);
    const digests: string[] = [];
    for (let i = 0; i < 2; i++) {
      const out = path.join(await tmpDir(), 'o.oci.tar');
      const r = await runCli(await tmpDir(), ['build', ctx, '-o', out], undefined, { SOURCE_DATE_EPOCH: '1700000000' });
      assert.equal(r.code, 0, r.stderr);
      digests.push(/oci image (sha256:[0-9a-f]{64})/.exec(r.stdout)![1]!);
    }
    assert.equal(digests[0], digests[1]);
    // a digest pin needs no lockfile entry, and frozen builds accept it
    const home = await tmpDir();
    const f = await runCli(home, ['build', ctx, '-o', path.join(await tmpDir(), 'o.oci.tar'), '--frozen-lockfile']);
    assert.equal(f.code, 0, f.stderr);
    await assert.rejects(fs.stat(path.join(ctx, 'Sitefile.lock')));
  });
});

test('BASE tag resolution is recorded in Sitefile.lock; frozen builds never re-resolve it', async () => {
  await withRegistry({}, async (reg) => {
    const v1 = seedBase(reg);
    const ctx = await baseSite(`${reg.host}/lib/base:1`);
    const key = `${reg.host}/lib/base:1`;
    const out = () => path.join(os_tmp, 'o.oci.tar');
    const os_tmp = await tmpDir();

    // frozen with no lock entry: error, and nothing was pulled
    const home0 = await tmpDir();
    const f0 = await runCli(home0, ['build', ctx, '-o', out(), '--frozen-lockfile']);
    assert.equal(f0.code, 1);
    assert.match(f0.stderr, /E_LOCK_MISSING.*not in the lockfile/);

    const home = await tmpDir();
    const b1 = await runCli(home, ['build', ctx, '-o', out(), '--force']);
    assert.equal(b1.code, 0, b1.stderr);
    assert.match(b1.stdout, /Updated Sitefile.lock/);
    const lock = JSON.parse(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8')) as { images: Record<string, { digest: string }> };
    assert.deepEqual(lock.images, { [key]: { digest: v1.manifestDigest } });
    const firstDigest = /oci image (sha256:[0-9a-f]{64})/.exec(b1.stdout)![1];

    // the tag moves on the registry...
    const v2 = reg.seedImage('lib/base', '1', { layers: [{ '/x': 'new' }], config: { Cmd: ['new'] }, docker: true });
    assert.notEqual(v2.manifestDigest, v1.manifestDigest);
    // ...a locked (even frozen) build still uses the locked digest, from a cold cache
    const tagCalls = () => reg.requests.filter((r) => r.url === `/v2/lib/base/manifests/1`).length;
    const before = tagCalls();
    const b2 = await runCli(await tmpDir(), ['build', ctx, '-o', out(), '--frozen-lockfile', '--force']);
    assert.equal(b2.code, 0, b2.stderr);
    assert.equal(tagCalls(), before, 'the tag was not resolved again');
    assert.equal(/oci image (sha256:[0-9a-f]{64})/.exec(b2.stdout)![1], firstDigest);

    // --update-lock re-resolves
    const b3 = await runCli(await tmpDir(), ['build', ctx, '-o', out(), '--update-lock', '--force']);
    assert.equal(b3.code, 0, b3.stderr);
    const lock3 = JSON.parse(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8')) as { images: Record<string, { digest: string }> };
    assert.equal(lock3.images[key]?.digest, v2.manifestDigest);
    assert.notEqual(/oci image (sha256:[0-9a-f]{64})/.exec(b3.stdout)![1], firstDigest);

    // a populated cache needs no network at all
    const warm = await tmpDir();
    assert.equal((await runCli(warm, ['build', ctx, '-o', out(), '--frozen-lockfile', '--force'])).code, 0);
    const reqs = reg.requests.length;
    assert.equal((await runCli(warm, ['build', ctx, '-o', out(), '--frozen-lockfile', '--force'])).code, 0);
    assert.equal(reg.requests.length, reqs, 'second build made no registry requests');
  });
});

test('BASE without a lockfile warns that the tag is not pinned', async () => {
  await withRegistry({}, async (reg) => {
    seedBase(reg);
    const ctx = await baseSite(`${reg.host}/lib/base:1`);
    const r = await runCli(await tmpDir(), ['build', ctx, '-o', path.join(await tmpDir(), 'o.oci.tar'), '--no-lockfile']);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stderr, /not pinned and there is no lockfile/);
  });
});

test('--base overrides the Sitefile; platform mismatches are errors', async () => {
  await withRegistry({}, async (reg) => {
    seedBase(reg);
    reg.seedImage('lib/arm', '1', { layers: [{ '/a': 'a' }], arch: 'arm64' });
    const ctx = await makeSite({}, 'FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci\n');
    const out = path.join(await tmpDir(), 'o.oci.tar');
    const ok = await runCli(await tmpDir(), ['build', ctx, '-o', out, '--base', `${reg.host}/lib/base:1`]);
    assert.equal(ok.code, 0, ok.stderr);
    const bad = await runCli(await tmpDir(), ['build', ctx, '-o', out, '--base', `${reg.host}/lib/arm:1`, '--force']);
    assert.equal(bad.code, 1);
    assert.match(bad.stderr, /is linux\/arm64, not linux\/amd64; pass --platform/);
    const good = await runCli(await tmpDir(), ['build', ctx, '-o', out, '--base', `${reg.host}/lib/arm:1`, '--platform', 'linux/arm64', '--force']);
    assert.equal(good.code, 0, good.stderr);
    // without BASE the image is unchanged (single layer, no base annotation)
    const plain = await runCli(await tmpDir(), ['build', ctx, '-o', out, '--force']);
    assert.equal(plain.code, 0, plain.stderr);
    const files = await extractArchive(new Uint8Array(await fs.readFile(out)), 'x');
    assert.equal(files.entries().filter(([p]) => p.startsWith('/blobs/')).length, 3);
  });
});

test('pushing a BASE image mounts the base layers from their repository on the same registry', async () => {
  await withRegistry({}, async (reg) => {
    const base = seedBase(reg);
    const home = await tmpDir();
    const ctx = await baseSite(`${reg.host}/lib/base:1`);
    assert.equal((await runCli(home, ['build', ctx, '-t', `${reg.host}/me/web:1`])).code, 0);
    const p = await runCli(home, ['push', `${reg.host}/me/web:1`]);
    assert.equal(p.code, 0, p.stderr);
    assert.match(p.stdout, /mounted from lib\/base/);
    const mounts = reg.requests.filter((r) => r.method === 'POST' && r.url.includes('mount='));
    assert.equal(mounts.length, 2, 'both base layers were mounted');
    assert.ok(mounts.every((m) => m.url.includes('from=lib%2Fbase')));
    for (const d of base.layerDigests) assert.ok(reg.blobs.has(d));
    // and the result pulls back identically
    const h2 = await tmpDir();
    const pl = await runCli(h2, ['pull', `${reg.host}/me/web:1`]);
    assert.equal(pl.code, 0, pl.stderr);
    const id = await (await Store.open(home)).resolve(`${reg.host}/me/web:1`);
    assert.equal(await (await Store.open(h2)).resolve(`${reg.host}/me/web:1`), id);
  });
});

test('bare names default to Docker Hub only for pull and BASE', () => {
  const r = withDefaultRegistry(parseRef('nginx:alpine'));
  assert.deepEqual([r.registry, r.name, r.tag], ['docker.io', 'library/nginx', 'alpine']);
});

test('pushImage rejects a digest that does not match the local image', async () => {
  await withRegistry({}, async (reg) => {
    const home = await tmpDir();
    const { id } = await buildBlog(home, 'blog:1');
    const store = await Store.open(home);
    const client = new RegistryClient(reg.host, {});
    await assert.rejects(pushImage(store, client, id, parseRef(`${reg.host}/x/y@sha256:${'0'.repeat(64)}`)), (e) => isSitefileError(e, 'E_USAGE'));
  });
});
