import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { surt, writeWacz } from '../src/wacz.js';
import { writeWarc } from '../src/warc.js';
import { encodeTree, formatFromExtension, defaultOutName } from '../src/export-formats.js';
import { extractArchive } from '../src/resources/archive.js';
import { Tree, makeFile } from '../src/tree.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const BASE = 'https://www.example.org/';

function site(): Tree {
  return Tree.fromFiles(
    (
      [
        ['/index.html', '<!doctype html><title>\n  Home  page </title><p>home</p>'],
        ['/docs/index.html', '<p>docs</p>'],
        ['/about.html', '<TITLE lang=en>About</TITLE>'],
        ['/css/a b.css', 'p{}'],
        ['/_headers', '/css/*\n  Cache-Control: max-age=60\n'],
      ] as Array<[string, string]>
    ).map(([p, s]) => [p, makeFile(enc.encode(s), 't')]),
  );
}

const sha = (b: Uint8Array): string => 'sha256:' + createHash('sha256').update(b).digest('hex');

test('wacz: the WARC, a CDXJ index, a page list and a datapackage; deterministic', async () => {
  const bytes = writeWacz(site(), { baseUrl: BASE, epoch: 1700000000 });
  assert.deepEqual(bytes, writeWacz(site(), { baseUrl: BASE, epoch: 1700000000 }));
  assert.deepEqual(bytes, encodeTree(site(), 'wacz', { baseUrl: BASE, epoch: 1700000000 }));
  const z = await extractArchive(bytes, 't');
  assert.deepEqual(
    z.entries().map(([p]) => p),
    ['/archive/data.warc.gz', '/datapackage-digest.json', '/datapackage.json', '/indexes/index.cdx', '/pages/pages.jsonl'],
  );
  const warc = new Uint8Array(z.get('/archive/data.warc.gz')!.bytes);
  // The same records as AS warc COMPRESSION gzip.
  assert.deepEqual(warc, writeWarc(site(), { baseUrl: BASE, epoch: 1700000000, compression: 'gzip' }));

  const cdx = dec.decode(z.get('/indexes/index.cdx')!.bytes).trimEnd().split('\n');
  assert.equal(cdx.length, 6);
  assert.deepEqual([...cdx].sort(), cdx);
  for (const line of cdx) {
    const [key, ts, json] = line.split(/ (?=\S)/, 3) as [string, string, string];
    const e = JSON.parse(json) as { url: string; offset: number; length: number; digest: string; filename: string; status: string; mime: string };
    assert.equal(ts, '20231114221320');
    assert.equal(key, surt(e.url));
    assert.equal(e.filename, 'data.warc.gz');
    assert.equal(e.status, '200');
    // offset/length address one gzip member holding the record for this URL.
    const record = dec.decode(gunzipSync(warc.subarray(e.offset, e.offset + e.length)));
    assert.match(record, new RegExp(`^WARC-Target-URI: ${e.url.replace(/[.?]/g, '\\$&')}\r$`, 'm'));
    assert.match(record, new RegExp(`^WARC-Payload-Digest: ${e.digest}\r$`, 'm'));
  }
  assert.ok(cdx.some((l) => l.startsWith('org,example)/css/a%20b.css ')));
  assert.match(cdx.find((l) => l.startsWith('org,example)/css/'))!, /"mime":"text\/css"/);

  const pages = dec.decode(z.get('/pages/pages.jsonl')!.bytes).trimEnd().split('\n').map((l) => JSON.parse(l) as Record<string, string>);
  assert.deepEqual(pages[0], { format: 'json-pages-1.0', id: 'pages', title: 'All Pages' });
  assert.deepEqual(
    pages.slice(1).map((p) => [p.url, p.title]),
    [
      ['https://www.example.org/about.html', 'About'],
      ['https://www.example.org/docs/', undefined],
      ['https://www.example.org/', 'Home page'],
    ],
  );
  assert.equal(pages[1]!.ts, '2023-11-14T22:13:20Z');
  assert.match(pages[1]!.id!, /^[0-9a-f]{32}$/);

  const pkg = JSON.parse(dec.decode(z.get('/datapackage.json')!.bytes)) as { wacz_version: string; resources: Array<{ path: string; hash: string; bytes: number }> };
  assert.equal(pkg.wacz_version, '1.1.1');
  for (const r of pkg.resources) {
    const b = z.get('/' + r.path)!.bytes;
    assert.equal(r.hash, sha(b));
    assert.equal(r.bytes, b.byteLength);
  }
  const digest = JSON.parse(dec.decode(z.get('/datapackage-digest.json')!.bytes)) as { path: string; hash: string };
  assert.deepEqual(digest, { path: 'datapackage.json', hash: sha(z.get('/datapackage.json')!.bytes) });
});

test('wacz: BASE is required; names and SURT keys', () => {
  assert.throws(() => writeWacz(site(), {}), /wacz export needs a base URL/);
  assert.equal(formatFromExtension('site.WACZ'), 'wacz');
  assert.equal(defaultOutName('wacz'), 'dist.wacz');
  assert.equal(surt('https://www.Example.com/a/b?x'), 'com,example)/a/b?x');
  assert.equal(surt('http://example.org'), 'org,example)/');
  assert.equal(surt('ftp://x'), 'ftp://x');
});
