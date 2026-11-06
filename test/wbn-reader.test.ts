import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { cborArray, cborBytes, cborMap, cborText, cborUint, decodeCbor, CborError } from '../src/cbor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { extractArchive } from '../src/resources/archive.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits } from '../src/resources/types.js';
import { isWebBundle, readWebBundle, webBundleToTree, type WebBundle } from '../src/resources/wbn-reader.js';
import { makeFile, Tree } from '../src/tree.js';
import { writeWebBundle } from '../src/wbn.js';

const enc = (s: string) => new TextEncoder().encode(s);
const dec = (b: Uint8Array) => new TextDecoder().decode(b);
const isCode = (code: ErrorCode) => (e: unknown) => isSitefileError(e, code);
const L = DEFAULT_ARCHIVE_LIMITS;

function mk(files: Record<string, string>): Tree {
  return Tree.fromFiles(Object.entries(files).map(([p, v]) => [p, makeFile(enc(v), 'test')] as const));
}
const content = (t: Tree): Record<string, string> => Object.fromEntries(t.entries().map(([p, n]) => [p, dec(n.bytes)]));

// ---------------------------------------------------------------- builders

const MAGIC = Buffer.from('f09f8c90f09f93a6', 'hex');
interface Resp { status?: string; body: string; headers?: Array<[string, string]> }

function response(r: Resp): Uint8Array {
  const h = cborMap([
    [cborBytes(enc(':status')), cborBytes(enc(r.status ?? '200'))],
    ...(r.headers ?? [['content-type', 'text/plain']]).map(([k, v]) => [cborBytes(enc(k)), cborBytes(enc(v))] as const),
  ]);
  return cborArray([cborBytes(h), cborBytes(enc(r.body))]);
}

interface Built { index: Uint8Array; responses: Uint8Array }
/** Each URL gets its own response; `b1` index values carry a leading empty variants-value. */
function build(urls: Record<string, Resp>, b1 = false): Built {
  const rs = Object.values(urls).map(response);
  let off = rs.length < 24 ? 1 : 3;
  const idx: Array<[Uint8Array, Uint8Array]> = [];
  Object.keys(urls).forEach((u, i) => {
    const loc = [cborUint(off), cborUint(rs[i]!.length)];
    idx.push([cborText(u), cborArray(b1 ? [cborBytes(new Uint8Array(0)), ...loc] : loc)]);
    off += rs[i]!.length;
  });
  return { index: cborMap(idx), responses: cborArray(rs) };
}

interface Opts {
  b1?: boolean;
  primary?: string;
  sections?: Array<[string, Uint8Array]>;
  /** Replace the declared section lengths / the trailing length. */
  lengths?: Array<[string, number]>;
  total?: number;
}
function bundle(b: Built, o: Opts = {}): Uint8Array {
  const sections = o.sections ?? [['index', b.index], ['responses', b.responses]];
  const lens = o.lengths ?? sections.map(([n, x]) => [n, x.length] as [string, number]);
  const sl = cborArray(lens.flatMap(([n, l]) => [cborText(n), cborUint(l)]));
  const parts = [
    Buffer.from([o.b1 ? 0x86 : 0x85]),
    cborBytes(MAGIC),
    cborBytes(Buffer.from(o.b1 ? 'b1\0\0' : 'b2\0\0', 'latin1')),
    ...(o.b1 ? [cborText(o.primary ?? 'https://x.test/')] : []),
    cborBytes(sl),
    cborArray(sections.map(([, x]) => x)),
  ];
  const total = parts.reduce((n, x) => n + x.length, 0) + 9;
  const len = Buffer.alloc(8);
  len.writeBigUInt64BE(BigInt(o.total ?? total));
  return Buffer.concat([...parts, cborBytes(len)]);
}

const read = (b: Uint8Array, limits: ArchiveLimits = L) => readWebBundle(b, 'in.wbn', limits);
const simple = { 'https://x.test/index.html': { body: '<h1>x</h1>' }, 'https://x.test/a.css': { body: 'a{}' } };

// ------------------------------------------------------------------ round trip

test('round trip: writeWebBundle -> readWebBundle', () => {
  const tree = mk({ '/index.html': '<h1>hi</h1>', '/css/a.css': 'a{}', '/sub/index.html': 'sub', '/img/a b.png': 'png' });
  const bytes = writeWebBundle(tree, 'https://example.com/site/');
  assert.ok(isWebBundle(bytes));
  const b = read(bytes);
  assert.equal(b.version, 'b2');
  assert.equal(b.primaryUrl, 'https://example.com/site/');
  assert.deepEqual(
    b.entries.map((e) => e.url),
    [
      'https://example.com/site/css/a.css',
      'https://example.com/site/img/a%20b.png',
      'https://example.com/site/index.html',
      'https://example.com/site/sub/',
      'https://example.com/site/sub/index.html',
      'https://example.com/site/',
    ].sort(),
  );
  const css = b.entries.find((e) => e.url.endsWith('a.css'))!;
  assert.equal(css.status, 200);
  assert.equal(css.headers['content-type'], 'text/css; charset=utf-8');
  assert.equal(dec(css.body), 'a{}');
});

test('round trip: extractArchive gives back the same files and digests', async () => {
  const tree = mk({ '/index.html': '<h1>hi</h1>', '/css/a.css': 'a{}', '/img/a b.png': 'png', '/sub/index.html': 'sub' });
  const out = await extractArchive(writeWebBundle(tree, 'https://example.com/'), 'in.wbn');
  assert.deepEqual(content(out), content(tree));
  for (const [p, n] of tree.entries()) assert.equal(out.get(p)?.digest, n.digest);
  assert.equal(out.get('/css/a.css')?.origin, 'in.wbn:https://example.com/css/a.css');
  assert.equal(out.get('/css/a.css')?.mode, 0o644);
});

test('round trip: an empty tree', async () => {
  const out = await extractArchive(writeWebBundle(Tree.empty, 'https://example.com/'), 'in.wbn');
  assert.equal(out.size, 0);
});

test('isWebBundle: b1 and b2 magic, nothing else', () => {
  assert.ok(isWebBundle(bundle(build(simple))));
  assert.ok(isWebBundle(bundle(build(simple, true), { b1: true })));
  assert.ok(!isWebBundle(enc('hello world, not a bundle')));
  assert.ok(!isWebBundle(new Uint8Array(0)));
  assert.ok(!isWebBundle(Buffer.concat([Buffer.from([0x84, 0x48]), MAGIC])));
});

// --------------------------------------------------------------------- b1

test('b1: primary URL in the header, single-variant index entries', () => {
  const b = read(bundle(build(simple, true), { b1: true, primary: 'https://x.test/index.html' }));
  assert.equal(b.version, 'b1');
  assert.equal(b.primaryUrl, 'https://x.test/index.html');
  assert.deepEqual(b.entries.map((e) => e.url), ['https://x.test/a.css', 'https://x.test/index.html']);
  assert.equal(dec(b.entries[1]!.body), '<h1>x</h1>');
});

test('b1: extractArchive maps it to a tree', async () => {
  const out = await extractArchive(bundle(build(simple, true), { b1: true }), 'in.wbn');
  assert.deepEqual(content(out), { '/a.css': 'a{}', '/index.html': '<h1>x</h1>' });
});

test('b1: a URL with multiple variants is an error', () => {
  const r = response({ body: 'x' });
  const responses = cborArray([r, r]);
  const v = cborBytes(enc('Accept-Language;en;fr'));
  const index = cborMap([
    [cborText('https://x.test/'), cborArray([v, cborUint(1), cborUint(r.length), cborUint(1 + r.length), cborUint(r.length)])],
  ]);
  assert.throws(() => read(bundle({ index, responses }, { b1: true })), (e) => isSitefileError(e, 'E_ARCHIVE') && /multiple variants/.test((e as Error).message));
});

test('b1: a single named variant is read', () => {
  const r = response({ body: 'x' });
  const index = cborMap([[cborText('https://x.test/'), cborArray([cborBytes(enc('Accept;text/html')), cborUint(1), cborUint(r.length)])]]);
  assert.equal(read(bundle({ index, responses: cborArray([r]) }, { b1: true })).entries.length, 1);
});

test('b1 header with b2 version field is rejected', () => {
  const bytes = Buffer.from(bundle(build(simple, true), { b1: true }));
  bytes[12] = 0x32; // "b2"
  assert.throws(() => read(bytes), isCode('E_ARCHIVE'));
});

// ------------------------------------------------------------------ url -> path

test('single origin: paths are URL pathnames; directory URLs map to index.html; percent-decoding', () => {
  const urls = {
    'https://x.test/': { body: 'root' },
    'https://x.test/docs/': { body: 'docs' },
    'https://x.test/a%20b/c%C3%A9.txt': { body: 'enc' },
    'https://x.test/deep/er/f.js': { body: 'js' },
  };
  assert.deepEqual(content(webBundleToTree(read(bundle(build(urls))), 'in.wbn')), {
    '/index.html': 'root',
    '/docs/index.html': 'docs',
    '/a b/cé.txt': 'enc',
    '/deep/er/f.js': 'js',
  });
});

test('multiple origins: paths are prefixed with the host (and port)', () => {
  const urls = {
    'https://a.test/index.html': { body: 'A' },
    'https://b.test:8443/x/': { body: 'B' },
    'http://c.test': { body: 'C' },
  };
  assert.deepEqual(content(webBundleToTree(read(bundle(build(urls))), 'in.wbn')), {
    '/a.test/index.html': 'A',
    '/b.test:8443/x/index.html': 'B',
    '/c.test/index.html': 'C',
  });
});

test('non-200 entries are skipped, and do not take part in the origin decision or URL checks', () => {
  const urls = {
    'https://x.test/ok.txt': { body: 'ok' },
    'https://other.test/missing': { body: 'nf', status: '404' },
    'https://x.test/moved': { body: '', status: '301', headers: [['location', '/ok.txt']] as Array<[string, string]> },
    'data:text/plain,hi': { body: 'weird', status: '204' },
  };
  const b = read(bundle(build(urls)));
  assert.equal(b.entries.find((e) => e.url.endsWith('/moved'))!.headers['location'], '/ok.txt');
  assert.deepEqual(content(webBundleToTree(b, 'in.wbn')), { '/ok.txt': 'ok' });
});

test('aliases with identical bytes are fine; different bytes conflict', () => {
  const same = { 'https://x.test/': { body: 'same' }, 'https://x.test/index.html': { body: 'same' } };
  assert.deepEqual(content(webBundleToTree(read(bundle(build(same))), 'in.wbn')), { '/index.html': 'same' });
  const diff = { 'https://x.test/': { body: 'one' }, 'https://x.test/index.html': { body: 'two' } };
  assert.throws(() => webBundleToTree(read(bundle(build(diff))), 'in.wbn'), isCode('E_CONFLICT'));
});

test('a file and a directory at the same path conflict', () => {
  const urls = { 'https://x.test/a': { body: 'file' }, 'https://x.test/a/b': { body: 'nested' } };
  assert.throws(() => webBundleToTree(read(bundle(build(urls))), 'in.wbn'), isCode('E_CONFLICT'));
});

const entry = (url: string, status = 200): BundleEntry => ({ url, status, headers: {}, body: enc('x') });
type BundleEntry = WebBundle['entries'][number];
const treeOf = (...urls: string[]) => webBundleToTree({ version: 'b2', primaryUrl: undefined, entries: urls.map((u) => entry(u)) }, 'in.wbn');

test('unsafe URLs are rejected with E_ARCHIVE_UNSAFE', () => {
  for (const url of [
    'https://x.test/a/../b',
    'https://x.test/%2e%2e/b',
    'https://x.test/a/%2E%2e/b',
    'https://x.test/a/./b',
    'https://x.test/a%2Fb',
    'https://x.test/a%00b',
    'https://x.test/a\\b',
    'https://x.test/a%5Cb',
    'https://x.test//double',
    'https://x.test/a//b',
    'https://x.test/%ff',
    'https://x.test/page#frag',
    'https://user:pw@x.test/page',
    'ftp://x.test/file',
    'file:///etc/passwd',
    'not a url',
    'https://x.test/a/%2e',
  ]) {
    assert.throws(() => treeOf(url), isCode('E_ARCHIVE_UNSAFE'), url);
  }
});

test('query strings: ok alone, rejected when they collide with another URL', () => {
  assert.deepEqual(content(treeOf('https://x.test/page?lang=en')), { '/page': 'x' });
  assert.throws(() => treeOf('https://x.test/page?lang=en', 'https://x.test/page?lang=fr'), isCode('E_ARCHIVE_UNSAFE'));
  assert.throws(() => treeOf('https://x.test/page', 'https://x.test/page?x=1'), isCode('E_ARCHIVE_UNSAFE'));
  assert.throws(() => treeOf('https://x.test/?x=1', 'https://x.test/index.html'), isCode('E_ARCHIVE_UNSAFE'));
});

// ---------------------------------------------------------------- corruption

const good = () => Buffer.from(bundle(build(simple)));

test('corrupt: bad magic, truncated, empty', () => {
  const g = good();
  const badMagic = Buffer.from(g);
  badMagic[5] = 0;
  assert.throws(() => read(badMagic), isCode('E_ARCHIVE'));
  for (const n of [0, 5, 10, 12, 20, g.length - 1, g.length - 9, Math.floor(g.length / 2)]) {
    assert.throws(() => read(g.subarray(0, n)), isCode('E_ARCHIVE'), `truncated to ${n}`);
  }
  assert.throws(() => read(Buffer.concat([g, Buffer.from([0])])), isCode('E_ARCHIVE'));
});

test('corrupt: unsupported version', () => {
  const g = good();
  g[12] = 0x33;
  assert.throws(() => read(g), (e) => isSitefileError(e, 'E_ARCHIVE') && /unsupported version/.test((e as Error).message));
});

test('corrupt: trailing length field does not match', () => {
  const b = build(simple);
  assert.throws(() => read(bundle(b, { total: 1234 })), (e) => isSitefileError(e, 'E_ARCHIVE') && /length field/.test((e as Error).message));
});

test('corrupt: declared section lengths do not add up', () => {
  const b = build(simple);
  assert.throws(() => read(bundle(b, { lengths: [['index', b.index.length + 3], ['responses', b.responses.length]] })), isCode('E_ARCHIVE'));
  assert.throws(() => read(bundle(b, { lengths: [['index', b.index.length], ['responses', b.responses.length - 1]] })), isCode('E_ARCHIVE'));
  assert.throws(() => read(bundle(b, { lengths: [['index', 1e9], ['responses', b.responses.length]] })), isCode('E_ARCHIVE'));
});

test('corrupt: index offsets out of range', () => {
  const r = response({ body: 'x' });
  const responses = cborArray([r]);
  for (const [off, len] of [[1, r.length + 1], [5000, 3], [2 ** 40, 1]] as const) {
    const index = cborMap([[cborText('https://x.test/'), cborArray([cborUint(off), cborUint(len)])]]);
    assert.throws(() => read(bundle({ index, responses })), isCode('E_ARCHIVE'), `${off},${len}`);
  }
});

test('corrupt: index offset pointing into the middle of a response', () => {
  const r = response({ body: 'xxxx' });
  const index = cborMap([[cborText('https://x.test/'), cborArray([cborUint(3), cborUint(r.length - 3)])]]);
  assert.throws(() => read(bundle({ index, responses: cborArray([r]) })), isCode('E_ARCHIVE'));
});

test('corrupt: missing responses section, wrong order, duplicates, missing index', () => {
  const b = build(simple);
  assert.throws(() => read(bundle(b, { sections: [['index', b.index]] })), isCode('E_ARCHIVE'));
  assert.throws(() => read(bundle(b, { sections: [['responses', b.responses], ['index', b.index]] })), isCode('E_ARCHIVE'));
  assert.throws(() => read(bundle(b, { sections: [['index', b.index], ['index', b.index], ['responses', b.responses]] })), isCode('E_ARCHIVE'));
  assert.throws(() => read(bundle(b, { sections: [['responses', b.responses]] })), isCode('E_ARCHIVE'));
});

test('corrupt: sections array count differs from section-lengths', () => {
  const b = build(simple);
  const g = bundle(b, { lengths: [['index', b.index.length], ['extra', 0], ['responses', b.responses.length]] });
  assert.throws(() => read(g), isCode('E_ARCHIVE'));
});

test('corrupt: bad response shapes', () => {
  const mkResp = (resp: Uint8Array) => {
    const index = cborMap([[cborText('https://x.test/'), cborArray([cborUint(1), cborUint(resp.length)])]]);
    return bundle({ index, responses: cborArray([resp]) });
  };
  const hdr = (pairs: Array<[string, string]>) => cborBytes(cborMap(pairs.map(([k, v]) => [cborBytes(enc(k)), cborBytes(enc(v))] as const)));
  const body = cborBytes(enc('b'));
  // no :status
  assert.throws(() => read(mkResp(cborArray([hdr([['content-type', 'x']]), body]))), isCode('E_ARCHIVE'));
  // bad :status
  assert.throws(() => read(mkResp(cborArray([hdr([[':status', '20x']]), body]))), isCode('E_ARCHIVE'));
  // other pseudo header
  assert.throws(() => read(mkResp(cborArray([hdr([[':status', '200'], [':path', '/']]), body]))), isCode('E_ARCHIVE'));
  // upper-case header name
  assert.throws(() => read(mkResp(cborArray([hdr([[':status', '200'], ['Content-Type', 'x']]), body]))), isCode('E_ARCHIVE'));
  // text instead of bstr in the header map
  const textHdr = cborBytes(cborMap([[cborText(':status'), cborText('200')]]));
  assert.throws(() => read(mkResp(cborArray([textHdr, body]))), isCode('E_ARCHIVE'));
  // three items
  assert.throws(() => read(mkResp(cborArray([hdr([[':status', '200']]), body, body]))), isCode('E_ARCHIVE'));
  // not an array
  assert.throws(() => read(mkResp(body)), isCode('E_ARCHIVE'));
});

test('corrupt: extractArchive reports E_ARCHIVE for a damaged bundle', async () => {
  const g = good();
  await assert.rejects(extractArchive(g.subarray(0, g.length - 4), 'in.wbn'), isCode('E_ARCHIVE'));
});

test('unknown, signatures and manifest sections are ignored', () => {
  const b = build(simple);
  const junk = enc('not even cbor \xff');
  const g = bundle(b, {
    sections: [['index', b.index], ['signatures', junk], ['manifest', junk], ['x-future', junk], ['responses', b.responses]],
  });
  assert.equal(read(g).entries.length, 2);
});

test('primary section is read; an invalid one is rejected', () => {
  const b = build(simple);
  const withPrimary = bundle(b, { sections: [['index', b.index], ['primary', cborText('https://x.test/')], ['responses', b.responses]] });
  assert.equal(read(withPrimary).primaryUrl, 'https://x.test/');
  const broken = bundle(b, { sections: [['index', b.index], ['primary', cborUint(5)], ['responses', b.responses]] });
  assert.throws(() => read(broken), isCode('E_ARCHIVE'));
  const relative = bundle(b, { sections: [['index', b.index], ['primary', cborText('/rel')], ['responses', b.responses]] });
  assert.throws(() => read(relative), isCode('E_ARCHIVE'));
});

// -------------------------------------------------------------------- limits

test('limits: entry count and total body bytes -> E_ARCHIVE_LIMIT', async () => {
  const g = bundle(build({ 'https://x.test/a': { body: 'aaaa' }, 'https://x.test/b': { body: 'bbbb' }, 'https://x.test/c': { body: 'cccc' } }));
  assert.equal(read(g, { ...L, maxEntries: 3 }).entries.length, 3);
  assert.throws(() => read(g, { ...L, maxEntries: 2 }), isCode('E_ARCHIVE_LIMIT'));
  assert.equal(read(g, { ...L, maxTotalBytes: 12 }).entries.length, 3);
  assert.throws(() => read(g, { ...L, maxTotalBytes: 11 }), isCode('E_ARCHIVE_LIMIT'));
  await assert.rejects(extractArchive(g, 'in.wbn', { ...L, maxEntries: 1 }), isCode('E_ARCHIVE_LIMIT'));
});

test('limits: many aliases of one big body count per entry', () => {
  const big = response({ body: 'x'.repeat(100) });
  const loc = (u: string) => [cborText(u), cborArray([cborUint(1), cborUint(big.length)])] as [Uint8Array, Uint8Array];
  const index = cborMap(['a', 'b', 'c'].map((s) => loc(`https://x.test/${s}`)));
  const g = bundle({ index, responses: cborArray([big]) });
  assert.throws(() => read(g, { ...L, maxTotalBytes: 250 }), isCode('E_ARCHIVE_LIMIT'));
});

test('a declared index size beyond the input does not allocate', () => {
  const index = Buffer.from([0xbb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // map(2^64-1)
  const g = bundle({ index, responses: cborArray([]) });
  assert.throws(() => read(g), isCode('E_ARCHIVE_LIMIT'));
  const g2 = bundle({ index: Buffer.from([0xb9, 0x01, 0x00]), responses: cborArray([]) }); // map(256), no content
  assert.throws(() => read(g2), isCode('E_ARCHIVE'));
});

test('a negative index offset or section length is rejected (it read outside the responses section)', () => {
  const r = response({ body: 'x' });
  const nint = Buffer.from([0x20]); // -1
  const index = cborMap([[cborText('https://x.test/a'), cborArray([nint, cborUint(r.length)])]]);
  assert.throws(() => read(bundle({ index, responses: cborArray([r]) })), /index offset\/length for 'https:\/\/x.test\/a' is not an unsigned integer/);
  const g = Buffer.from(bundle(build(simple)));
  // section-lengths: ["index", n, "responses", m] -> make n negative (uint8 head 0x18 -> nint8 head 0x38)
  const at = g.indexOf(Buffer.from('index')) + 5;
  assert.equal(g[at], 0x18);
  g[at] = 0x38;
  assert.throws(() => read(g), /malformed section-lengths entry/);
});

test('header names that are array indices come first, like Object.fromEntries', () => {
  const b = bundle(build({ 'https://x.test/a': { body: 'x', headers: [['x-b', '1'], ['10', 'a'], ['2', 'b']] } }));
  assert.deepEqual(Object.keys(read(b).entries[0]!.headers), ['2', '10', 'x-b']);
});

// --------------------------------------------------------- cross-check tool

test('dump-bundle lists the same URLs as our reader (skipped if not installed)', async (t) => {
  const gopath = spawnSync('go', ['env', 'GOPATH'], { encoding: 'utf8' });
  const tool = gopath.status === 0 ? path.join(gopath.stdout.trim(), 'bin', 'dump-bundle') : '';
  if (!tool || !(await fs.stat(tool).then(() => true, () => false))) {
    t.skip('dump-bundle not installed (go install github.com/WICG/webpackage/go/bundle/cmd/dump-bundle@latest)');
    return;
  }
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-wbn-reader-'));
  try {
    const bytes = writeWebBundle(mk({ '/index.html': 'hi', '/a/b.css': 'b{}', '/c d.txt': 'c' }), 'https://example.com/');
    const file = path.join(dir, 'b.wbn');
    await fs.writeFile(file, bytes);
    const r = spawnSync(tool, ['-i', file], { encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
    const theirs = [...r.stdout.matchAll(/^> :url: (.+)$/gm)].map((m) => m[1]!).sort();
    const ours = read(bytes).entries.map((e) => e.url).sort();
    assert.deepEqual(ours, theirs);
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------- CBOR decoder

const h = (s: string) => Buffer.from(s.replace(/\s/g, ''), 'hex');

test('cbor decode: unsigned and negative integers, including 64-bit', () => {
  assert.deepEqual(decodeCbor(h('00')), { value: 0, end: 1 });
  assert.deepEqual(decodeCbor(h('17')), { value: 23, end: 1 });
  assert.deepEqual(decodeCbor(h('1818')), { value: 24, end: 2 });
  assert.deepEqual(decodeCbor(h('190100')), { value: 256, end: 3 });
  assert.deepEqual(decodeCbor(h('1a00010000')), { value: 65536, end: 5 });
  assert.deepEqual(decodeCbor(h('1b0000000100000000')), { value: 2 ** 32, end: 9 });
  assert.deepEqual(decodeCbor(h('1bffffffffffffffff')), { value: 2n ** 64n - 1n, end: 9 });
  assert.deepEqual(decodeCbor(h('20')), { value: -1, end: 1 });
  assert.deepEqual(decodeCbor(h('3863')), { value: -100, end: 2 });
  assert.deepEqual(decodeCbor(h('3bffffffffffffffff')), { value: -(2n ** 64n), end: 9 });
});

test('cbor decode: byte and text strings', () => {
  const b = decodeCbor(h('43010203'));
  assert.deepEqual([...(b.value as Uint8Array)], [1, 2, 3]);
  assert.equal(b.end, 4);
  assert.deepEqual(decodeCbor(h('6568656c6c6f')), { value: 'hello', end: 6 });
  assert.deepEqual(decodeCbor(h('60')), { value: '', end: 1 });
  assert.equal(decodeCbor(Buffer.concat([cborText('héllo 🌐')])).value, 'héllo 🌐');
  assert.throws(() => decodeCbor(h('62c328')), CborError); // invalid UTF-8
});

test('cbor decode: arrays and maps (maps are Maps, so __proto__ is inert)', () => {
  assert.deepEqual(decodeCbor(h('83010203')), { value: [1, 2, 3], end: 4 });
  const m = decodeCbor(cborMap([[cborText('__proto__'), cborUint(1)], [cborText('a'), cborArray([cborUint(2)])]])).value as Map<unknown, unknown>;
  assert.ok(m instanceof Map);
  assert.equal(m.get('__proto__'), 1);
  assert.deepEqual(m.get('a'), [2]);
  assert.equal(({} as Record<string, unknown>)['polluted'], undefined);
  assert.deepEqual(decodeCbor(h('80')).value, []);
  // duplicate keys
  assert.throws(() => decodeCbor(h('a2 6161 01 6161 02')), CborError);
  assert.throws(() => decodeCbor(h('a2 4101 01 4101 02')), CborError);
});

test('cbor decode: simple values; tags, floats, indefinite lengths are errors', () => {
  assert.deepEqual(decodeCbor(h('f4')), { value: false, end: 1 });
  assert.deepEqual(decodeCbor(h('f5')), { value: true, end: 1 });
  assert.deepEqual(decodeCbor(h('f6')), { value: null, end: 1 });
  assert.throws(() => decodeCbor(h('f7')), CborError); // undefined
  assert.throws(() => decodeCbor(h('f93c00')), CborError); // half float
  assert.throws(() => decodeCbor(h('fb3ff0000000000000')), CborError); // double
  assert.throws(() => decodeCbor(h('f814')), CborError); // 2-byte simple value
  assert.throws(() => decodeCbor(h('c11a514b67b0')), CborError); // tag
  assert.throws(() => decodeCbor(h('9f01ff')), CborError); // indefinite array
  assert.throws(() => decodeCbor(h('5f4101ff')), CborError); // indefinite bstr
  assert.throws(() => decodeCbor(h('1c')), CborError); // reserved info
});

test('cbor decode: reports the end offset and leaves trailing bytes alone', () => {
  assert.equal(decodeCbor(h('8201 02 ffff')).end, 3);
});

test('cbor decode: depth and item limits', () => {
  const nest = (n: number) => Buffer.concat([Buffer.alloc(n, 0x81), Buffer.from([0x00])]);
  assert.equal(decodeCbor(nest(32)).end, 33);
  assert.throws(() => decodeCbor(nest(33)), /nesting/);
  assert.throws(() => decodeCbor(nest(5), { maxDepth: 4 }), /nesting/);
  assert.doesNotThrow(() => decodeCbor(nest(4), { maxDepth: 4 }));
  assert.throws(() => decodeCbor(nest(100_000)), CborError); // no stack overflow
  assert.doesNotThrow(() => decodeCbor(h('83010203'), { maxItems: 4 }));
  assert.throws(() => decodeCbor(h('83010203'), { maxItems: 3 }), /items/);
});

test('cbor decode: a declared length larger than the input is rejected before allocating', () => {
  for (const hex of ['5b00000000ffffffff', '5bffffffffffffffff', '9bffffffffffffffff', 'bbffffffffffffffff', '7a7fffffff61', '9a7fffffff', '5805 0102', '82 01']) {
    assert.throws(() => decodeCbor(h(hex)), CborError, hex);
  }
  assert.throws(() => decodeCbor(new Uint8Array(0)), CborError);
  assert.throws(() => decodeCbor(h('19 01')), CborError);
});

test('cbor decode: round trips the encoder', () => {
  const v = decodeCbor(cborArray([cborUint(7), cborText('t'), cborBytes(enc('b')), cborMap([[cborText('k'), cborUint(1)]])])).value as unknown[];
  assert.equal(v[0], 7);
  assert.equal(v[1], 't');
  assert.equal(dec(v[2] as Uint8Array), 'b');
  assert.equal((v[3] as Map<string, number>).get('k'), 1);
});
