import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { gunzipSync } from 'node:zlib';
import { sha256 } from '../src/digest.js';
import { isSitefileError } from '../src/errors.js';
import { ANN, buildOciImage, canonicalJson, decodeIndex, exportNameAnnotations, extractImageFiles, gitInfo, imageMeta, layoutTree, MEDIA, ociArchive, parseConfig, parseLabels, parseManifest } from '../src/oci.js';
import { extractArchive } from '../src/resources/archive.js';
import { makeFile, Tree } from '../src/tree.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

const enc = (s: string) => new TextEncoder().encode(s);
const tree = Tree.fromFiles([
  ['/index.html', makeFile(enc('<h1>hi</h1>'), 't')],
  ['/css/site.css', makeFile(enc('a{b:c}'), 't')],
  ['/bin/run.sh', makeFile(enc('#!/bin/sh\n'), 't', 0o755)],
]);

function tarNames(tar: Uint8Array): Array<{ name: string; type: string }> {
  const out: Array<{ name: string; type: string }> = [];
  for (let o = 0; o + 512 <= tar.length; ) {
    const h = Buffer.from(tar.subarray(o, o + 512));
    if (h.every((b) => b === 0)) break;
    const size = parseInt(h.toString('latin1', 124, 136).replace(/\0.*$/, ''), 8);
    out.push({ name: h.toString('utf8', 0, 100).replace(/\0.*$/, ''), type: h.toString('latin1', 156, 157) });
    o += 512 + Math.ceil(size / 512) * 512;
  }
  return out;
}

test('canonicalJson sorts keys and has no whitespace', () => {
  assert.equal(canonicalJson({ b: [1, { z: 1, a: 2 }], a: 'x', u: undefined }), '{"a":"x","b":[1,{"a":2,"z":1}]}');
});

test('an image parses back: index -> manifest -> config and layer digests, diff_id, extraction', async () => {
  const img = buildOciImage(tree, { title: 'demo', revision: 'a'.repeat(40), source: 'https://example.com/r.git' });
  const layout = layoutTree(img, 'demo:latest');
  assert.equal(new TextDecoder().decode(layout.get('/oci-layout')!.bytes), '{"imageLayoutVersion":"1.0.0"}');
  const [entry] = decodeIndex(layout.get('/index.json')!.bytes);
  assert.equal(entry!.refName, 'latest');
  assert.equal(JSON.parse(new TextDecoder().decode(layout.get('/index.json')!.bytes)).manifests[0].annotations['io.containerd.image.name'], 'docker.io/library/demo:latest');
  for (const [given, full, tag] of [
    ['blog:1', 'docker.io/library/blog:1', '1'],
    ['team/blog', 'docker.io/team/blog:latest', 'latest'],
    ['localhost:5000/blog:2', 'localhost:5000/blog:2', '2'],
    ['docker.io/library/nginx:alpine', 'docker.io/library/nginx:alpine', 'alpine'],
  ] as const) {
    assert.deepEqual(exportNameAnnotations(given), { 'io.containerd.image.name': full, 'org.opencontainers.image.ref.name': tag });
  }
  const blob = (d: string) => layout.get(`/blobs/sha256/${d.slice(7)}`)!.bytes;
  // every blob is stored under its own digest
  for (const [p, n] of layout.entries()) if (p.startsWith('/blobs/')) assert.equal(`sha256:${p.split('/').pop()}`, n.digest);
  const manifestBytes = blob(entry!.digest);
  assert.equal(sha256(manifestBytes), entry!.digest);
  assert.equal(manifestBytes.byteLength, entry!.size);
  const m = parseManifest(manifestBytes);
  assert.equal(JSON.parse(Buffer.from(manifestBytes).toString()).mediaType, MEDIA.manifest);
  assert.equal(JSON.parse(Buffer.from(manifestBytes).toString()).schemaVersion, 2);
  assert.equal(m.config.mediaType, MEDIA.config);
  const configBytes = blob(m.config.digest);
  assert.equal(sha256(configBytes), m.config.digest);
  assert.equal(configBytes.byteLength, m.config.size);
  const cfg = parseConfig(configBytes);
  assert.equal(cfg.os, 'linux');
  assert.equal(cfg.architecture, 'amd64');
  assert.equal(cfg.created, '1970-01-01T00:00:00Z');
  assert.equal(cfg.labels[ANN.root], '/srv/www');
  assert.equal(cfg.labels[ANN.title], 'demo');
  assert.equal(cfg.labels[ANN.revision], 'a'.repeat(40));
  assert.equal(cfg.labels[ANN.source], 'https://example.com/r.git');
  assert.equal(m.layers.length, 1);
  assert.equal(m.layers[0]!.mediaType, 'application/vnd.oci.image.layer.v1.tar+gzip');
  const gz = blob(m.layers[0]!.digest);
  assert.equal(sha256(gz), m.layers[0]!.digest);
  const tar = new Uint8Array(gunzipSync(gz));
  assert.equal(sha256(tar), cfg.diffIds[0], 'diff_id is the digest of the uncompressed tar');
  // parent directories of the root come first, then the files; ids/mtimes are fixed
  const names = tarNames(tar);
  assert.deepEqual(names.slice(0, 2), [{ name: 'srv/', type: '5' }, { name: 'srv/www/', type: '5' }]);
  assert.ok(names.some((n) => n.name === 'srv/www/index.html' && n.type === '0'));
  const all = await extractArchive(tar, 'test');
  assert.equal(all.subtree('/srv/www').digest(), tree.digest());
  assert.equal(all.get('/srv/www/bin/run.sh')!.mode, 0o755);
});

test('digests are reproducible: same tree, same manifest digest; options change it', () => {
  const a = buildOciImage(tree, {});
  const b = buildOciImage(Tree.fromFiles([...tree.entries()].reverse()), {});
  assert.equal(a.manifestDigest, b.manifestDigest);
  assert.equal(sha256(a.layerBytes), sha256(b.layerBytes));
  assert.notEqual(buildOciImage(tree, { root: '/var/www' }).manifestDigest, a.manifestDigest);
  const epoch = buildOciImage(tree, { epoch: 1_700_000_000 });
  assert.equal(parseConfig(epoch.configBytes).created, '2023-11-14T22:13:20Z');
  const arch = parseConfig(buildOciImage(tree, { platform: 'linux/arm64' }).configBytes);
  assert.equal(arch.architecture, 'arm64');
  assert.throws(() => buildOciImage(tree, { platform: 'nonsense' }), (e) => isSitefileError(e, 'E_USAGE'));
  assert.throws(() => buildOciImage(tree, { root: 'relative' }), (e) => isSitefileError(e, 'E_PATH'));
  assert.deepEqual(tarNames(new Uint8Array(gunzipSync(buildOciImage(tree, { root: '/' }).layerBytes))).map((n) => n.name)[0], 'bin/run.sh');
  assert.deepEqual(ociArchive(a, undefined, undefined), ociArchive(b, undefined, undefined));
});

test('two builds of the same Sitefile give an identical manifest digest (CLI, oci archive and layout directory)', async () => {
  const ctx = await makeSite();
  const out = await tmpDir();
  const digests: string[] = [];
  for (const name of ['one.oci.tar', 'two.oci.tar']) {
    const r = await runCli(out, ['build', ctx, '-o', path.join(out, name)]);
    assert.equal(r.code, 0, r.stderr);
    digests.push(/oci image (sha256:[0-9a-f]{64})/.exec(r.stdout)![1]!);
  }
  assert.equal(digests[0], digests[1]);
  assert.deepEqual(await fs.readFile(path.join(out, 'one.oci.tar')), await fs.readFile(path.join(out, 'two.oci.tar')));
  const r = await runCli(out, ['build', ctx, '--format', 'oci', '-o', path.join(out, 'layout')]);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, new RegExp(digests[0]!));
  const idx = decodeIndex(await fs.readFile(path.join(out, 'layout', 'index.json')));
  assert.equal(idx[0]!.digest, digests[0]);
  // the archive is the layout, tarred
  const files = await extractArchive(new Uint8Array(await fs.readFile(path.join(out, 'one.oci.tar'))), 'x');
  assert.equal(files.get('/index.json') !== undefined, true);
  assert.equal(files.get(`/blobs/sha256/${digests[0]!.slice(7)}`) !== undefined, true);
});

test('EXPORT ... AS oci [AT root] and CLI overrides', async () => {
  const ctx = await makeSite({}, 'FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci AT /var/www\n');
  const out = await tmpDir();
  const dest = path.join(out, 'default.oci.tar');
  // no -o: the default name is dist.oci.tar in the context
  const r = await runCli(out, ['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  const files = await extractArchive(new Uint8Array(await fs.readFile(path.join(ctx, 'dist.oci.tar'))), 'x');
  const entry = decodeIndex(files.get('/index.json')!.bytes)[0]!;
  const m = parseManifest(files.get(`/blobs/sha256/${entry.digest.slice(7)}`)!.bytes);
  const cfg = parseConfig(files.get(`/blobs/sha256/${m.config.digest.slice(7)}`)!.bytes);
  assert.equal(cfg.labels[ANN.root], '/var/www');
  const r2 = await runCli(out, ['build', ctx, '-o', dest, '--oci-root', '/srv/site', '--platform', 'linux/arm64']);
  assert.equal(r2.code, 0, r2.stderr);
  const f2 = await extractArchive(new Uint8Array(await fs.readFile(dest)), 'x');
  const e2 = decodeIndex(f2.get('/index.json')!.bytes)[0]!;
  const m2 = parseManifest(f2.get(`/blobs/sha256/${e2.digest.slice(7)}`)!.bytes);
  const c2 = parseConfig(f2.get(`/blobs/sha256/${m2.config.digest.slice(7)}`)!.bytes);
  assert.equal(c2.labels[ANN.root], '/srv/site');
  assert.equal(c2.architecture, 'arm64');
});

test('EXPORT ... AT with a non-oci format is rejected when planned', async () => {
  const ctx = await makeSite({}, 'FROM scratch\nCOPY ./public/ /\nEXPORT / AS zip AT /x\n');
  const r = await runCli(await tmpDir(), ['validate', ctx]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_VALIDATION.*AT applies only to AS oci/);
});

test('COMPRESSION none: the site layer is the plain tar (digest = diff_id) and still extracts', async () => {
  const gz = buildOciImage(tree, {});
  const img = buildOciImage(tree, { compression: 'none' });
  assert.equal(img.diffId, gz.diffId, 'same layer content');
  assert.equal(img.layerDigest, img.diffId);
  assert.equal(sha256(img.layerBytes), img.diffId);
  const m = parseManifest(img.manifestBytes);
  assert.equal(m.layers[0]!.mediaType, MEDIA.layerTar);
  assert.equal(m.layers[0]!.size, img.layerBytes.byteLength);
  assert.notEqual(img.manifestDigest, gz.manifestDigest);
  assert.equal(img.configDigest, gz.configDigest, 'the config only records the diff_id');
  const cfg = parseConfig(img.configBytes);
  assert.equal((await extractImageFiles(m, cfg, img.layerBytes, 'x')).digest(), tree.digest());
  // a tar layer whose bytes do not match its digest is still refused
  const bad = img.layerBytes.slice();
  bad[600] = bad[600]! ^ 1;
  await assert.rejects(extractImageFiles(m, cfg, bad, 'x'), (e) => isSitefileError(e, 'E_STORE'));
});

test('EXPORT ... COMPRESSION: parsed for oci (and wbn), rejected elsewhere or for unknown values; --oci-compression overrides', async () => {
  const home = await tmpDir();
  const bad = await makeSite({}, 'FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci COMPRESSION zstd\n');
  const r1 = await runCli(home, ['validate', bad]);
  assert.equal(r1.code, 1);
  assert.match(r1.stderr, /E_SYNTAX.*:3:29: unknown compression 'zstd' \(expected gzip or none\)/);
  const zip = await makeSite({}, 'FROM scratch\nCOPY ./public/ /\nEXPORT / AS zip COMPRESSION none\n');
  assert.match((await runCli(home, ['validate', zip])).stderr, /E_VALIDATION.*COMPRESSION applies only to AS oci and AS wbn/);
  const ok = await makeSite({}, 'FROM scratch\nCOPY ./public/ /\nEXPORT / AS oci AT /srv/site COMPRESSION None\n');
  const out = path.join(await tmpDir(), 'site.oci.tar');
  const layerType = async (args: string[]) => {
    const b = await runCli(home, ['build', ok, '-o', out, '--force', ...args]);
    assert.equal(b.code, 0, b.stderr);
    const f = await extractArchive(new Uint8Array(await fs.readFile(out)), 'x');
    const e = decodeIndex(f.get('/index.json')!.bytes)[0]!;
    return parseManifest(f.get(`/blobs/sha256/${e.digest.slice(7)}`)!.bytes).layers[0]!.mediaType;
  };
  assert.equal(await layerType([]), MEDIA.layerTar);
  assert.equal(await layerType(['--oci-compression', 'gzip']), MEDIA.layer);
  assert.equal(await layerType(['--compression', 'gzip']), MEDIA.layer);
  const u = await runCli(home, ['build', ok, '-o', out, '--force', '--oci-compression', 'lz4']);
  assert.equal(u.code, 2);
  assert.match(u.stderr, /--oci-compression expects gzip or none, got 'lz4'/);
  // -t stores the uncompressed image; the store reads it back (images, inspect, save, serve all use loadTree)
  const t = await runCli(home, ['build', ok, '-t', 'plain:1']);
  assert.equal(t.code, 0, t.stderr);
  const insp = await runCli(home, ['image', 'inspect', 'plain:1']);
  assert.match(insp.stdout, /Layer: +sha256:[0-9a-f]{64} \(.* tar\)/);
  const saved = path.join(await tmpDir(), 'o');
  assert.equal((await runCli(home, ['save', 'plain:1', '-o', saved])).code, 0);
  assert.ok(await fs.stat(path.join(saved, 'index.html')));
  // save picks a gzipped wbn from the extension, like build -o.
  const gz = path.join(await tmpDir(), 'site.wbn.gz');
  const s = await runCli(home, ['save', 'plain:1', '-o', gz, '--base-url', 'https://example.com/']);
  assert.equal(s.code, 0, s.stderr);
  const bytes = await fs.readFile(gz);
  assert.deepEqual([bytes[0], bytes[1], gunzipSync(bytes)[0]], [0x1f, 0x8b, 0x85]);
});

test('external OCI validators (skopeo, crane, oras, umoci, podman) when installed', async (t) => {
  const ctx = await makeSite();
  const out = await tmpDir();
  const layout = path.join(out, 'layout');
  const tarPath = path.join(out, 'site.oci.tar');
  assert.equal((await runCli(out, ['build', ctx, '--format', 'oci', '-o', layout])).code, 0);
  assert.equal((await runCli(out, ['build', ctx, '-o', tarPath])).code, 0);
  const have = (bin: string) => spawnSync(bin, ['--version'], { stdio: 'ignore' }).status !== null;
  const ran: string[] = [];
  const run = (bin: string, args: string[]) => {
    const r = spawnSync(bin, args, { encoding: 'utf8' });
    assert.equal(r.status, 0, `${bin} ${args.join(' ')}: ${r.stderr}${r.stdout}`);
    ran.push(bin);
  };
  if (have('skopeo')) run('skopeo', ['inspect', `oci:${layout}`]);
  if (have('crane')) run('crane', ['validate', '--tarball', tarPath]);
  if (have('oras')) run('oras', ['manifest', 'fetch', '--oci-layout', layout]);
  if (have('umoci')) run('umoci', ['stat', '--image', layout]);
  if (have('podman') && spawnSync('podman', ['info'], { stdio: 'ignore' }).status === 0) {
    run('podman', ['load', '-i', tarPath]);
  }
  if (ran.length === 0) t.skip('none of skopeo, crane, oras, umoci or a running podman is available');
});

test('git labels are opt-in; when on, credentials in the remote URL are dropped', async () => {
  const dir = await tmpDir();
  await fs.mkdir(path.join(dir, '.git', 'refs', 'heads'), { recursive: true });
  await fs.writeFile(path.join(dir, '.git', 'HEAD'), 'ref: refs/heads/main\n');
  await fs.writeFile(path.join(dir, '.git', 'refs', 'heads', 'main'), 'b'.repeat(40) + '\n');
  await fs.writeFile(path.join(dir, '.git', 'config'), '[remote "origin"]\n\turl = https://alice:ghp_SECRET@github.com/acme/site.git?x=1\n');

  const off = await imageMeta(dir);
  assert.equal(off.source, undefined);
  assert.equal(off.revision, undefined);

  const on = await imageMeta(dir, { git: true });
  assert.equal(on.revision, 'b'.repeat(40));
  assert.equal(on.source, 'https://github.com/acme/site.git');
  assert.doesNotMatch(JSON.stringify(await gitInfo(dir)), /SECRET|alice/);
});

test('--label adds config labels and validates keys', () => {
  assert.deepEqual({ ...parseLabels(['org.example.team=web', 'empty=']) }, { 'org.example.team': 'web', empty: '' });
  for (const bad of ['novalue', '=x', 'has space=x']) {
    assert.throws(() => parseLabels([bad]), (e: unknown) => isSitefileError(e, 'E_USAGE'), bad);
  }
  const tree = Tree.fromFiles([['/index.html', makeFile(new TextEncoder().encode('hi'), 't')]]);
  const img = buildOciImage(tree, { title: 'demo', labels: { 'org.example.team': 'web', [ANN.title]: 'override' } });
  const cfg = parseConfig(img.configBytes);
  assert.equal(cfg.labels['org.example.team'], 'web');
  assert.equal(cfg.labels[ANN.title], 'override');
});
