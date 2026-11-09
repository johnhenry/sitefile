import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import http from 'node:http';
import { zstdDecompressSync } from 'node:zlib';
import { acceptsDcz, addDictionaryDeltas, deltaFor } from '../src/dictionary.js';
import { closeServer, createSiteServer, listen } from '../src/server.js';
import { Tree, makeFile } from '../src/tree.js';
import { dczEncode, zstdCompress } from '../src/zstd.js';

const enc = new TextEncoder();
const sha = (b: Uint8Array): string => createHash('sha256').update(b).digest('hex');

test('zstd frames decode with libzstd (Node), with and without a dictionary', () => {
  const cases: Array<[Uint8Array, Uint8Array]> = [
    [new Uint8Array(0), new Uint8Array(0)],
    [enc.encode('hello'), new Uint8Array(0)],
    [enc.encode('abc'.repeat(1000)), new Uint8Array(0)],
    [randomBytes(200_000), new Uint8Array(0)],
    [new Uint8Array(1_000_000).fill(7), new Uint8Array(0)],
  ];
  const lib = randomBytes(300_000);
  cases.push([Buffer.concat([lib.subarray(0, 1000), enc.encode('CHANGED'), lib.subarray(1000, 200_000), randomBytes(500), lib.subarray(200_000)]), lib]);
  for (let i = 0; i < 100; i++) {
    const pick = (n: number): Uint8Array => Uint8Array.from({ length: n }, () => 97 + Math.floor(Math.random() * 3));
    cases.push([pick(Math.floor(Math.random() * 3000)), pick(Math.floor(Math.random() * 2000))]);
  }
  for (const [content, dict] of cases) {
    const frame = zstdCompress(content, dict);
    const back = zstdDecompressSync(frame, dict.length ? { dictionary: dict } : {});
    assert.deepEqual(new Uint8Array(back), new Uint8Array(content));
  }
});

test('the encoder writes the same bytes as the Rust port (rust/src/zstd.rs)', () => {
  let dict = '';
  for (let i = 0; i < 5000; i++) dict += `line ${(i * 7919) % 1000} of text\n`;
  const d = enc.encode(dict);
  const t = enc.encode(dict.replace('line 5 ', 'LINE 5 ') + 'tail');
  const delta = dczEncode(t, d);
  assert.equal(delta.length, 76);
  assert.equal(sha(delta), '7d60bc78bf0a87b0ec4cf342484cf47fecb394cee21c3777caea97414d527b29');
  assert.deepEqual([...delta.subarray(0, 8)], [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0, 0, 0]);
  assert.equal(Buffer.from(delta.subarray(8, 40)).toString('hex'), sha(d));
  const bin = Uint8Array.from({ length: 300_000 }, (_, i) => ((i * 2654435761) >>> 13) & 255);
  assert.equal(sha(zstdCompress(bin)), '238b55161a0431956632f8fd74aa7f5bbee71d5601993338815ad0e40f65b39c');
  assert.equal(sha(zstdCompress(new Uint8Array(0))), '75db1663d3ad0f2d7f360589df9ae3dbc57ec8a7569dd148ac48aff9a55cd3c5');
  assert.equal(sha(zstdCompress(d)), '43ef299268621da31f3f233d41474d2d94e7ab70f3d6de70509bc84dc8171d88');
});

function jsFile(edit: (s: string) => string = (s) => s): Uint8Array {
  let s = '';
  for (let i = 0; i < 3000; i++) s += `function f${i}(){return ${i}}\n`;
  return enc.encode(edit(s));
}

test('addDictionaryDeltas: a .dcz for changed assets, Use-As-Dictionary headers, conflicts refused', () => {
  const v1 = Tree.fromFiles([
    ['/app.js', makeFile(jsFile(), 't')],
    ['/same.css', makeFile(enc.encode('p{}'), 't')],
    ['/img.png', makeFile(enc.encode('png'), 't')],
  ]);
  const v2 = Tree.fromFiles([
    ['/app.js', makeFile(jsFile((s) => s.replace('return 5}', 'return 55}')), 't')],
    ['/same.css', makeFile(enc.encode('p{}'), 't')],
    ['/img.png', makeFile(enc.encode('png2'), 't')],
    ['/_headers', makeFile(enc.encode('/*\n  X-A: 1'), 't')],
  ]);
  const r = addDictionaryDeltas(v2, v1);
  assert.deepEqual(r.deltas, ['/app.js']);
  const dcz = r.tree.get('/app.js.dcz')!.bytes;
  assert.ok(dcz.length < 200, `delta of ${dcz.length} bytes`);
  assert.deepEqual(new Uint8Array(zstdDecompressSync(dcz.subarray(40), { dictionary: v1.get('/app.js')!.bytes })), v2.get('/app.js')!.bytes);
  assert.equal(
    new TextDecoder().decode(r.tree.get('/_headers')!.bytes),
    '/*\n  X-A: 1\n# Compression Dictionary Transport (sitefile build --experimental-dictionary)\n/app.js\n  Use-As-Dictionary: match="/app.js"\n/same.css\n  Use-As-Dictionary: match="/same.css"\n',
  );
  const clash = Tree.fromFiles([...v2.entries(), ['/app.js.dcz', makeFile(enc.encode('x'), 't')]]);
  assert.throws(() => addDictionaryDeltas(clash, v1), /E_CONFLICT|already exists/);
});

test('acceptsDcz and deltaFor', () => {
  assert.ok(acceptsDcz('gzip, br, zstd, dcb, dcz'));
  assert.ok(!acceptsDcz('gzip, dcz;q=0'));
  assert.ok(acceptsDcz('DCZ;q=0.5'));
  assert.ok(!acceptsDcz(undefined));
  const old = jsFile();
  const t = Tree.fromFiles([['/a.js', makeFile(jsFile((s) => s + '//x'), 't')], ['/a.js.dcz', makeFile(dczEncode(jsFile((s) => s + '//x'), old), 't')]]);
  const offer = `:${createHash('sha256').update(old).digest('base64')}:`;
  assert.ok(deltaFor(t, '/a.js', offer, 'dcz'));
  assert.equal(deltaFor(t, '/a.js', offer, 'gzip'), undefined);
  assert.equal(deltaFor(t, '/a.js', `:${createHash('sha256').update('other').digest('base64')}:`, 'dcz'), undefined);
  assert.equal(deltaFor(t, '/a.js', 'not-sf-binary', 'dcz'), undefined);
});

test('serve answers Available-Dictionary with the dcz delta', async () => {
  const old = jsFile();
  const now = jsFile((s) => s.replace('return 7}', 'return 77}'));
  const r = addDictionaryDeltas(Tree.fromFiles([['/app.js', makeFile(now, 't')]]), Tree.fromFiles([['/app.js', makeFile(old, 't')]]));
  const server = createSiteServer(r.tree);
  const { port } = await listen(server, '127.0.0.1', 0);
  const get = (headers: Record<string, string>): Promise<{ status: number; headers: http.IncomingHttpHeaders; body: Buffer }> =>
    new Promise((resolve, reject) => {
      http.get({ host: '127.0.0.1', port, path: '/app.js', headers }, (res) => {
        const chunks: Buffer[] = [];
        res.on('data', (c: Buffer) => chunks.push(c));
        res.on('end', () => resolve({ status: res.statusCode ?? 0, headers: res.headers, body: Buffer.concat(chunks) }));
      }).on('error', reject);
    });
  try {
    const plain = await get({});
    assert.equal(plain.headers['content-encoding'], undefined);
    assert.equal(plain.headers.vary, 'accept-encoding, available-dictionary');
    assert.equal(plain.headers['use-as-dictionary'], 'match="/app.js"');
    assert.deepEqual(new Uint8Array(plain.body), now);
    const d = await get({ 'accept-encoding': 'gzip, dcz', 'available-dictionary': `:${createHash('sha256').update(old).digest('base64')}:` });
    assert.equal(d.status, 200);
    assert.equal(d.headers['content-encoding'], 'dcz');
    assert.ok(String(d.headers.etag).endsWith('-dcz"'));
    assert.deepEqual(new Uint8Array(zstdDecompressSync(d.body.subarray(40), { dictionary: old })), now);
  } finally {
    await closeServer(server);
  }
});
