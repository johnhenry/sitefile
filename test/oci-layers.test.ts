import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { buildOciImage, layerOf, layoutTree, ociArchive, parseConfig, parseManifest, siteLayerIndices, ANN } from '../src/oci.js';
import { loadArchiveTree } from '../src/servers.js';
import { Store } from '../src/store.js';
import { Tree, makeFile } from '../src/tree.js';

const enc = new TextEncoder();
function site(change?: string): Tree {
  const files: Array<[string, ReturnType<typeof makeFile>]> = [];
  for (let i = 0; i < 300; i++) {
    const p = `/p/${i}.html`;
    files.push([p, makeFile(enc.encode(p === change ? 'changed' : `page ${i} ${'x'.repeat(1000)}`), 't')]);
  }
  return Tree.fromFiles(files);
}

test('--oci-layers 1 (the default) is the single-layer image it always was', () => {
  const a = buildOciImage(site());
  const b = buildOciImage(site(), { layers: 1 });
  assert.equal(a.manifestDigest, b.manifestDigest);
  assert.equal(parseManifest(a.manifestBytes).layers.length, 1);
  assert.equal(parseConfig(a.configBytes).labels[ANN.layers], undefined);
});

test('n site layers: files split by a hash of the path; a one-file change changes one layer', () => {
  const a = buildOciImage(site(), { layers: 8 });
  const b = buildOciImage(site('/p/42.html'), { layers: 8 });
  const ma = parseManifest(a.manifestBytes);
  const mb = parseManifest(b.manifestBytes);
  const cfg = parseConfig(a.configBytes);
  assert.equal(ma.layers.length, 8);
  assert.equal(cfg.labels[ANN.layers], '8');
  assert.equal(cfg.diffIds.length, 8);
  assert.deepEqual(siteLayerIndices(ma, cfg), [0, 1, 2, 3, 4, 5, 6, 7]);
  const changed = mb.layers.map((l, i) => (l.digest !== ma.layers[i]!.digest ? i : -1)).filter((i) => i >= 0);
  assert.deepEqual(changed, [layerOf('/p/42.html', 8)]);
  assert.ok(mb.layers[changed[0]!]!.size < mb.layers.reduce((n, l) => n + l.size, 0) / 4);
  assert.equal(a.extraLayers?.size, 7);
  assert.throws(() => buildOciImage(site(), { layers: 65 }), /--oci-layers expects an integer from 1 to 64/);
});

test('a layered image round-trips through the store, an OCI archive and a layout', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-layers-'));
  try {
    const store = await Store.open(path.join(dir, 'home'));
    const { id } = await store.addImage(site(), { layers: 5 });
    assert.equal((await store.loadTree(id)).digest(), site().digest());
    const layout = await store.layoutOf(id);
    assert.equal(layout.entries().filter(([p]) => p.startsWith('/blobs/')).length, 7); // manifest, config, 5 layers
    const img = buildOciImage(site(), { layers: 5 });
    assert.equal(id, img.manifestDigest);
    const file = path.join(dir, 'site.oci.tar');
    await writeFile(file, ociArchive(img, undefined, undefined));
    assert.equal((await loadArchiveTree(file)).digest(), site().digest());
    assert.equal(layoutTree(img).entries().length, 2 + 7);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
