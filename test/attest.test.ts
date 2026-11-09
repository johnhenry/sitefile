import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { ATTEST, EMPTY_DIGEST, isSecretArgName, referrersTag, SITEFILE_VERSION } from '../src/attest.js';
import { sha256 } from '../src/digest.js';
import { parseManifest } from '../src/oci.js';
import { extractArchive } from '../src/resources/archive.js';
import { Store } from '../src/store.js';
import type { Tree } from '../src/tree.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';
import { startMockRegistry, type MockRegistry } from './helpers/mock-registry.js';

const dec = new TextDecoder();

interface IndexJson {
  manifests: Array<{ digest: string; size: number; artifactType?: string; annotations?: Record<string, string> }>;
}

const SITEFILE = 'FROM scratch\nARG TITLE = "t"\nIMPORT css FROM ./public/style.css\nCOPY ./public/ /\nCOPY css:/ /copied.css\nEXPORT / AS oci\n';

async function readLayout(file: string): Promise<{ files: Tree; index: IndexJson; json: (digest: string) => Record<string, any> }> {
  const files = await extractArchive(new Uint8Array(await fs.readFile(file)), 'x');
  const blob = (d: string): Uint8Array => {
    const f = files.get(`/blobs/sha256/${d.slice(7)}`);
    assert.ok(f, `blob ${d} is in the layout`);
    assert.equal(sha256(f.bytes), d);
    return f.bytes;
  };
  return { files, index: JSON.parse(dec.decode(files.get('/index.json')!.bytes)) as IndexJson, json: (d) => JSON.parse(dec.decode(blob(d))) as Record<string, any> };
}

/** The in-toto statement of an attestation manifest. */
function statementOf(json: (d: string) => Record<string, any>, manifestDigest: string): Record<string, any> {
  const m = json(manifestDigest);
  assert.equal(m.artifactType, ATTEST.inToto);
  assert.equal(m.config.digest, EMPTY_DIGEST);
  assert.equal(m.layers.length, 1);
  return json(m.layers[0].digest);
}

test('secret-looking build-arg names are recognized by word', () => {
  for (const n of ['API_TOKEN', 'token', 'DB_PASSWORD', 'AWS_SECRET_ACCESS_KEY', 'NPM_AUTH', 'GITHUB_APIKEY', 'my_credentials']) assert.ok(isSecretArgName(n), n);
  for (const n of ['TITLE', 'TOKENIZER', 'KEYBOARD', 'MONKEY', 'AUTHOR', 'SITE_URL']) assert.ok(!isSecretArgName(n), n);
  assert.equal(referrersTag('sha256:' + 'a'.repeat(64)), 'sha256-' + 'a'.repeat(64));
});

test('--provenance/--sbom: attestations are referrers of an unchanged image, deterministic, without timestamps', async () => {
  const home = await tmpDir();
  const ctx = await makeSite({}, SITEFILE);
  const dir = await tmpDir();
  const plain = path.join(dir, 'plain.oci.tar');
  const att = path.join(dir, 'att.oci.tar');
  const again = path.join(dir, 'again.oci.tar');
  const args = ['--no-lockfile', '--build-arg', 'TITLE=Docs', '--build-arg', 'API_TOKEN=hunter2'];
  assert.equal((await runCli(home, ['build', ctx, '-o', plain, ...args])).code, 0);
  const b = await runCli(home, ['build', ctx, '-o', att, '--provenance', '--sbom', ...args]);
  assert.equal(b.code, 0, b.stderr);
  assert.equal((await runCli(home, ['build', ctx, '-o', again, '--sbom', '--provenance', ...args])).code, 0);
  assert.deepEqual(await fs.readFile(att), await fs.readFile(again), 'byte-identical on rebuild');

  const p = await readLayout(plain);
  const a = await readLayout(att);
  assert.equal(p.index.manifests.length, 1);
  assert.equal(a.index.manifests.length, 3);
  const image = a.index.manifests[0]!;
  assert.equal(image.digest, p.index.manifests[0]!.digest, 'the image ID does not change');
  assert.deepEqual(
    a.index.manifests.slice(1).map((m) => [m.artifactType, m.annotations?.[ATTEST.predicateAnnotation]]),
    [
      [ATTEST.inToto, ATTEST.slsa],
      [ATTEST.inToto, ATTEST.cyclonedx],
    ],
  );
  for (const m of a.index.manifests.slice(1)) {
    const manifest = a.json(m.digest);
    assert.deepEqual(manifest.subject, { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: image.digest, size: image.size });
    assert.equal(parseManifest(a.files.get(`/blobs/sha256/${m.digest.slice(7)}`)!.bytes).layers[0]!.mediaType, ATTEST.inToto);
  }
  const text = dec.decode(await fs.readFile(att));
  assert.ok(!text.includes('hunter2'), 'secret-looking build args are never recorded');
  assert.ok(!/"(startedOn|finishedOn|timestamp)"/.test(text), 'no timestamps without SOURCE_DATE_EPOCH');

  const prov = statementOf(a.json, a.index.manifests[1]!.digest);
  assert.equal(prov._type, ATTEST.statement);
  assert.deepEqual(prov.subject, [{ name: '_', digest: { sha256: image.digest.slice(7) } }]);
  assert.equal(prov.predicateType, ATTEST.slsa);
  const def = prov.predicate.buildDefinition;
  assert.deepEqual(def.externalParameters, { sitefile: 'Sitefile', args: { TITLE: 'Docs' }, redactedArgs: ['API_TOKEN'], platform: 'linux/amd64' });
  const sitefileDigest = sha256(new Uint8Array(await fs.readFile(path.join(ctx, 'Sitefile'))));
  const cssDigest = sha256(new Uint8Array(await fs.readFile(path.join(ctx, 'public/style.css'))));
  assert.deepEqual(def.resolvedDependencies, [
    { name: 'sitefile', uri: 'Sitefile', digest: { sha256: sitefileDigest.slice(7) } },
    { name: 'import:css', uri: './public/style.css', digest: { sha256: cssDigest.slice(7) } },
  ]);
  assert.deepEqual(prov.predicate.runDetails, { builder: { id: ATTEST.builderId, version: { sitefile: SITEFILE_VERSION } } });

  const sbom = statementOf(a.json, a.index.manifests[2]!.digest).predicate;
  assert.equal(sbom.bomFormat, 'CycloneDX');
  assert.equal(sbom.specVersion, '1.6');
  assert.equal(sbom.metadata.component.version, image.digest);
  const files = sbom.components.filter((c: { type: string }) => c.type === 'file');
  assert.deepEqual(
    files.map((c: { name: string }) => c.name),
    ['/srv/www/about/index.html', '/srv/www/copied.css', '/srv/www/index.html', '/srv/www/style.css'],
  );
  const css = files.find((c: { name: string }) => c.name === '/srv/www/copied.css');
  assert.deepEqual(css.hashes, [{ alg: 'SHA-256', content: cssDigest.slice(7) }]);
  assert.deepEqual(css.properties, [
    { name: 'sitefile:mode', value: '644' },
    { name: 'sitefile:origin', value: 'Sitefile:5 COPY' },
  ]);
});

test('SOURCE_DATE_EPOCH stamps the attestations; the layout directory form holds the same blobs', async () => {
  const home = await tmpDir();
  const ctx = await makeSite({}, SITEFILE);
  const dir = await tmpDir();
  const env = { SOURCE_DATE_EPOCH: '1700000000' };
  const r = await runCli(home, ['build', ctx, '-o', path.join(dir, 'x.oci.tar'), '--no-lockfile', '--provenance', '--sbom'], undefined, env);
  assert.equal(r.code, 0, r.stderr);
  const a = await readLayout(path.join(dir, 'x.oci.tar'));
  const prov = statementOf(a.json, a.index.manifests[1]!.digest);
  assert.deepEqual(prov.predicate.runDetails.metadata, { startedOn: '2023-11-14T22:13:20Z', finishedOn: '2023-11-14T22:13:20Z' });
  assert.equal(statementOf(a.json, a.index.manifests[2]!.digest).predicate.metadata.timestamp, '2023-11-14T22:13:20Z');

  const d = await runCli(home, ['build', ctx, '-o', path.join(dir, 'layout'), '--format', 'oci', '--no-lockfile', '--provenance', '--sbom'], undefined, env);
  assert.equal(d.code, 0, d.stderr);
  const index = await fs.readFile(path.join(dir, 'layout', 'index.json'));
  assert.deepEqual(new Uint8Array(index), a.files.get('/index.json')!.bytes);
});

test('--provenance needs an oci image (export or -t)', async () => {
  const home = await tmpDir();
  const ctx = await makeSite();
  const r = await runCli(home, ['build', ctx, '-o', path.join(await tmpDir(), 'out'), '--no-lockfile', '--sbom']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--provenance and --sbom attach attestations to an oci image/);
  const t = await runCli(home, ['build', ctx, '-t', 'site:1', '--no-lockfile', '--sbom']);
  assert.equal(t.code, 0, t.stderr);
});

async function withRegistry<T>(opts: Parameters<typeof startMockRegistry>[0], fn: (r: MockRegistry) => Promise<T>): Promise<T> {
  const r = await startMockRegistry(opts);
  try {
    return await fn(r);
  } finally {
    await r.close();
  }
}

test('-t keeps attestations in the store: save includes them, prune keeps them, push sends them as referrers', async () => {
  await withRegistry({}, async (reg) => {
    const home = await tmpDir();
    const ctx = await makeSite({}, SITEFILE);
    const dir = await tmpDir();
    const remote = `${reg.host}/team/site:1`;
    const built = path.join(dir, 'built.oci.tar');
    const b = await runCli(home, ['build', ctx, '-o', built, '--no-lockfile', '--provenance', '--sbom', '-t', remote]);
    assert.equal(b.code, 0, b.stderr);
    const store = await Store.open(home);
    const id = await store.resolve(remote);
    assert.equal((await store.referrerDescriptors(id)).length, 2);

    // save writes the same layout as the build
    assert.equal((await runCli(home, ['image', 'prune'])).code, 0);
    const saved = path.join(dir, 'saved.oci.tar');
    const s = await runCli(home, ['save', remote, '-o', saved]);
    assert.equal(s.code, 0, s.stderr);
    assert.deepEqual(await fs.readFile(saved), await fs.readFile(built), 'save after prune = build output');

    // a rebuild with --sbom only replaces the SBOM and keeps the provenance
    assert.equal((await runCli(home, ['build', ctx, '--no-lockfile', '--sbom', '-t', remote])).code, 0);
    assert.equal((await store.referrerDescriptors(id)).length, 2);

    // push: the registry has no referrers API, so the fallback tag lists both
    const p = await runCli(home, ['push', remote]);
    assert.equal(p.code, 0, p.stderr);
    assert.match(p.stdout, /: attestation pushed \(https:\/\/slsa\.dev\/provenance\/v1\)\n/);
    assert.match(p.stdout, /: attestation pushed \(https:\/\/cyclonedx\.org\/bom\)\n/);
    const tag = referrersTag(id);
    assert.match(p.stdout, new RegExp(`referrers tag ${tag}: 2 attestations\\n1: digest: ${id} size: \\d+\\n$`));
    assert.equal(reg.manifests.get(`team/site:1`)?.toString('utf8') !== undefined, true);
    assert.equal(sha256(new Uint8Array(reg.manifests.get('team/site:1')!)), id, 'the tag still names the image');
    const fallback = JSON.parse(reg.manifests.get(`team/site:${tag}`)!.toString('utf8')) as IndexJson;
    const descs = await store.referrerDescriptors(id);
    assert.deepEqual(fallback.manifests.map((m) => m.digest), descs.map((d) => d.digest));
    for (const d of descs) assert.ok(reg.manifests.has(`team/site@${d.digest}`));
    assert.ok(reg.blobs.has(EMPTY_DIGEST));

    // again: nothing new; entries other tools added to the fallback index are kept
    const foreign = { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: 'sha256:' + 'b'.repeat(64), size: 1, artifactType: 'application/example' };
    reg.manifests.set(`team/site:${tag}`, Buffer.from(JSON.stringify({ schemaVersion: 2, mediaType: 'application/vnd.oci.image.index.v1+json', manifests: [foreign] })));
    const p2 = await runCli(home, ['push', remote]);
    assert.equal(p2.code, 0, p2.stderr);
    const merged = JSON.parse(reg.manifests.get(`team/site:${tag}`)!.toString('utf8')) as IndexJson;
    assert.deepEqual(merged.manifests.map((m) => m.digest), [foreign.digest, ...descs.map((d) => d.digest)]);
    const p3 = await runCli(home, ['push', remote]);
    assert.match(p3.stdout, new RegExp(`referrers tag ${tag}: up to date\\n`));

    // rmi + prune drops the attestation list and its blobs
    assert.equal((await runCli(home, ['rmi', remote])).code, 0);
    assert.equal((await runCli(home, ['image', 'prune'])).code, 0);
    assert.deepEqual(await store.referrerDescriptors(id), []);
    assert.equal(await store.hasBlob(descs[0]!.digest), false);
  });
});

test('a registry with the referrers API gets no fallback tag', async () => {
  await withRegistry({ referrersApi: true }, async (reg) => {
    const home = await tmpDir();
    const ctx = await makeSite({}, SITEFILE);
    const remote = `${reg.host}/team/site:2`;
    assert.equal((await runCli(home, ['build', ctx, '--no-lockfile', '--provenance', '-t', remote])).code, 0);
    const p = await runCli(home, ['push', remote]);
    assert.equal(p.code, 0, p.stderr);
    assert.doesNotMatch(p.stdout, /referrers tag/);
    assert.ok(![...reg.manifests.keys()].some((k) => k.includes(':sha256-')));
  });
});

test('images without attestations push exactly as before', async () => {
  await withRegistry({}, async (reg) => {
    const home = await tmpDir();
    const ctx = await makeSite({}, SITEFILE);
    const remote = `${reg.host}/team/site:3`;
    assert.equal((await runCli(home, ['build', ctx, '--no-lockfile', '-t', remote])).code, 0);
    const p = await runCli(home, ['push', remote]);
    assert.equal(p.code, 0, p.stderr);
    assert.doesNotMatch(p.stdout, /attestation|referrers/);
    assert.equal(reg.requests.filter((r) => r.method === 'PUT' && r.url.includes('/manifests/')).length, 1);
  });
});

test('with a BASE: the provenance and the SBOM name the base image digest; base layers stay out of the file list', async () => {
  await withRegistry({}, async (reg) => {
    const base = reg.seedImage('lib/base', '1', { layers: [{ '/etc/motd': 'base\n' }], config: { Cmd: ['/bin/serve'] } });
    const home = await tmpDir();
    const ctx = await makeSite({}, `FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci BASE "${reg.host}/lib/base:1" AT /usr/share/html\n`);
    const out = path.join(await tmpDir(), 'b.oci.tar');
    const b = await runCli(home, ['build', ctx, '-o', out, '--provenance', '--sbom']);
    assert.equal(b.code, 0, b.stderr);
    const a = await readLayout(out);
    const deps = statementOf(a.json, a.index.manifests[1]!.digest).predicate.buildDefinition.resolvedDependencies as Array<Record<string, unknown>>;
    assert.deepEqual(deps.at(-1), { name: 'base', uri: `${reg.host}/lib/base:1`, digest: { sha256: base.manifestDigest.slice(7) } });
    const comps = statementOf(a.json, a.index.manifests[2]!.digest).predicate.components as Array<Record<string, unknown>>;
    assert.deepEqual(comps[0], { type: 'container', 'bom-ref': 'base', name: `${reg.host}/lib/base:1`, version: base.manifestDigest });
    assert.ok(comps.slice(1).every((c) => String(c.name).startsWith('/usr/share/html/')));
  });
});
