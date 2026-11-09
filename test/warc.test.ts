import { test } from 'node:test';
import assert from 'node:assert/strict';
import { gunzipSync } from 'node:zlib';
import { looksLikeWarc, readWarc, writeWarc } from '../src/warc.js';
import { encodeTree, formatFromExtension, compressionFromExtension, defaultOutName } from '../src/export-formats.js';
import { Tree, makeFile } from '../src/tree.js';

const enc = new TextEncoder();
const dec = new TextDecoder();

function site(): Tree {
  return Tree.fromFiles(
    (
      [
        ['/index.html', '<p>home</p>'],
        ['/docs/index.html', '<p>docs</p>'],
        ['/css/a b.css', 'p{}'],
        ['/_headers', '/css/*\n  Cache-Control: max-age=60\n/*\n  X-Frame-Options: DENY\n'],
        ['/_redirects', '/old /new 301\n'],
      ] as Array<[string, string]>
    ).map(([p, s]) => [p, makeFile(enc.encode(s), 't')]),
  );
}

test('warc: a warcinfo record, then one response per file (and per directory index), deterministic', () => {
  const w = dec.decode(writeWarc(site(), { baseUrl: 'https://example.org/' }));
  assert.ok(w.startsWith('WARC/1.1\r\nWARC-Type: warcinfo\r\nWARC-Record-ID: <urn:uuid:'));
  const uris = [...w.matchAll(/^WARC-Target-URI: (\S+)/gm)].map((m) => m[1]);
  assert.deepEqual(uris, ['https://example.org/css/a%20b.css', 'https://example.org/docs/index.html', 'https://example.org/docs/', 'https://example.org/index.html', 'https://example.org/']);
  assert.match(w, /HTTP\/1.1 200 OK\r\nContent-Type: text\/css; charset=utf-8\r\nContent-Length: 3\r\ncache-control: max-age=60\r\nx-frame-options: DENY\r\n\r\np\{\}/);
  assert.match(w, /^WARC-Date: 1970-01-01T00:00:00Z\r$/m);
  assert.doesNotMatch(w, /_redirects|_headers/);
  assert.match(dec.decode(writeWarc(site(), { baseUrl: 'https://example.org/', epoch: 1700000000 })), /^WARC-Date: 2023-11-14T22:13:20Z\r$/m);
  assert.deepEqual(writeWarc(site(), { baseUrl: 'https://example.org/' }), encodeTree(site(), 'warc', { baseUrl: 'https://example.org/' }));
  assert.throws(() => writeWarc(site(), {}), /warc export needs a base URL/);
});

test('warc: read back (plain and gzipped per record); only 200 responses of the first host', () => {
  const plain = writeWarc(site(), { baseUrl: 'https://example.org/' });
  const gz = writeWarc(site(), { baseUrl: 'https://example.org/', compression: 'gzip' });
  assert.equal(gz[0], 0x1f);
  assert.deepEqual(new Uint8Array(gunzipSync(gz)), plain);
  assert.ok(looksLikeWarc(plain) && looksLikeWarc(gz) && !looksLikeWarc(enc.encode('PK')));
  for (const bytes of [plain, gz]) {
    const t = readWarc(bytes, 'site.warc');
    assert.deepEqual(t.entries().map(([p]) => p), ['/css/a b.css', '/docs/index.html', '/index.html']);
    assert.equal(dec.decode(t.get('/docs/index.html')!.bytes), '<p>docs</p>');
  }
  const extra =
    'WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: https://other.example/x\r\nContent-Type: application/http; msgtype=response\r\nContent-Length: 21\r\n\r\nHTTP/1.1 200 OK\r\n\r\nX\r\n\r\n' +
    'WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: https://example.org/gone\r\nContent-Type: application/http\r\nContent-Length: 26\r\n\r\nHTTP/1.1 404 Not Found\r\n\r\n\r\n\r\n';
  const t = readWarc(new Uint8Array([...plain, ...enc.encode(extra)]), 'x.warc');
  assert.equal(t.get('/x'), undefined);
  assert.equal(t.get('/gone'), undefined);
});

test('warc: unsafe paths, truncation and limits are refused', () => {
  const rec = (uri: string, body = 'x'): string => {
    const block = `HTTP/1.1 200 OK\r\n\r\n${body}`;
    return `WARC/1.1\r\nWARC-Type: response\r\nWARC-Target-URI: ${uri}\r\nContent-Type: application/http\r\nContent-Length: ${block.length}\r\n\r\n${block}\r\n\r\n`;
  };
  assert.throws(() => readWarc(enc.encode(rec('https://h/a/%2e%2e/b')), 'x'), /E_ARCHIVE_UNSAFE|unsafe path/);
  assert.throws(() => readWarc(enc.encode(rec('https://h/a%2Fb')), 'x'), /unsafe path/);
  assert.throws(() => readWarc(enc.encode(rec('https://h/a').slice(0, 60)), 'x'), /truncated/);
  assert.throws(() => readWarc(enc.encode(rec('https://h/a') + rec('https://h/b')), 'x', { maxEntries: 1, maxTotalBytes: 100, maxRatio: 100 }), /more than 1 entries/);
  assert.throws(() => readWarc(enc.encode(rec('https://h/a', 'y'.repeat(200))), 'x', { maxEntries: 10, maxTotalBytes: 100, maxRatio: 100 }), /beyond 100 bytes/);
});

test('warc names', () => {
  assert.equal(formatFromExtension('a.WARC.gz'), 'warc');
  assert.equal(compressionFromExtension('a.warc.gz', 'warc'), 'gzip');
  assert.equal(defaultOutName('warc', 'gzip'), 'dist.warc.gz');
});

test('serve and IMPORT read a .warc.gz like other archives', async () => {
  const { loadArchiveTree } = await import('../src/servers.js');
  const { bytesToResource } = await import('../src/resources/convert.js');
  const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
  const os = await import('node:os');
  const path = await import('node:path');
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-warc-'));
  try {
    const f = path.join(dir, 'site.warc.gz');
    await writeFile(f, writeWarc(site(), { baseUrl: 'https://example.org/', compression: 'gzip' }));
    assert.deepEqual((await loadArchiveTree(f)).entries().map(([p]) => p), ['/css/a b.css', '/docs/index.html', '/index.html']);
    const r = await bytesToResource(writeWarc(site(), { baseUrl: 'https://example.org/' }), 'tree', { uri: 'site.warc', basename: 'site.warc' });
    assert.equal(r.kind, 'tree');
    assert.equal(r.kind === 'tree' ? r.tree.size : 0, 3);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
