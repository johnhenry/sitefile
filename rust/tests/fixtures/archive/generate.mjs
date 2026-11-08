// Regenerates the archive / web bundle reader differential fixture from the
// TypeScript reference (src/resources/archive.ts, src/resources/wbn-reader.ts).
//
//   npm ci && npm run build        # at the repository root (builds lib/)
//   node rust/tests/fixtures/archive/generate.mjs
//
// Writes fuzz.json next to this script: seeded tar, tar.gz, zip (stored,
// deflate, data descriptors, zip64 records) and web bundle (b1, b2) archives,
// built structurally and then mutated (truncated headers, bad offsets and
// sizes, long names, symlinks and hard links, duplicate and case/Unicode
// colliding names, compression bombs), each with the archive limits it was
// read with and what TypeScript made of it (format: record.mjs).
// Checked by the Rust test `resources::archive_differential` (results, error
// codes and messages must match; no panics; trees within the limits) and by
// test/archive-fuzz-fixture.test.ts (TypeScript still produces fuzz.json).
//
// For a larger local run (not committed):
//   FUZZ_OUT=/tmp/archive-fuzz.json FUZZ_COUNT=50000 FUZZ_SEED=7 node rust/tests/fixtures/archive/generate.mjs
//   SITEFILE_ARCHIVE_FUZZ=/tmp/archive-fuzz.json cargo test --release -- --ignored archive_differential_external
import { crc32, deflateRawSync, gzipSync } from 'node:zlib';
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadLib, record } from './record.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const lib = await loadLib(root);

// ---------------------------------------------------------------- random

// Seeded: mulberry32.
let seed = Number(process.env.FUZZ_SEED ?? 0xa4c41fe);
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const int = (n) => Math.floor(rand() * n);
const pick = (xs) => xs[int(xs.length)];
const chance = (p) => rand() < p;
const bytesOf = (n) => Buffer.from(Array.from({ length: n }, () => int(256)));
const B = (s) => (typeof s === 'string' ? Buffer.from(s, 'utf8') : Buffer.from(s));

// ---------------------------------------------------------------- vocabulary

/** Entry names (strings are UTF-8; Buffers are raw bytes, possibly invalid UTF-8). */
const SAFE = ['index.html', 'style.css', 'a/b.txt', 'a/b/c.txt', 'a/', 'dir/', 'dir/x', 'x', 'site/index.html', 'site/', 'site/a.css', 'p/q/r/s.txt', 'img/logo.png', 'caf\u00e9.txt'];
const NAMES = [
  'index.html', 'style.css', 'a/b.txt', 'a/b/c.txt', 'a/', 'a/b/', 'dir/', 'dir/x', 'x', 'X', 'README', 'readme',
  'site/index.html', 'site/', 'site/a.css', 'pkg/package.json', 'pkg/', 'A.txt', 'a.txt', 'a.TXT',
  // case and Unicode folding / normalization collisions
  'caf\u00e9.txt', 'cafe\u0301.txt', 'CAF\u00c9.txt', 'stra\u00dfe', 'STRASSE', 'strasse', '\ufb01le', 'file', 'FILE',
  '\u212a', 'k', 'K', '\u03c3', '\u03c2', '\u03a3', '\u0130', 'i', 'I', '\u0131', '\u00c5', 'A\u030a', '\u212b',
  '\ud83d\ude00.txt', '\u{1f600}/x', '\u200b', '\ufeffbom', 'a\u0000b', 'nul\u0000',
  // unsafe or odd paths
  '../x', 'a/../../x', 'a/..', '..', '.', './a', 'a/./b', 'a//b', '/abs', '//net/x', 'C:x', 'c:/x', 'a\\b', '\\x',
  '', '/', 'a/b/../c', '%2e%2e/x', 'a:b', 'CON', ' ', 'a ', 'trailing/', '.hidden', '...', 'a/.../b',
  'x'.repeat(99), 'y'.repeat(100), 'z'.repeat(101), 'd/'.repeat(60) + 'f', 'n'.repeat(155) + '/' + 'm'.repeat(90), 'w'.repeat(300),
  Buffer.from([0xff, 0xfe, 0x41]), Buffer.from([0xc3]), Buffer.from([0xe2, 0x82]), Buffer.from([0xed, 0xa0, 0x80]),
  Buffer.from([0xf0, 0x9f, 0x98]), Buffer.from([0x61, 0x80, 0x62]), Buffer.from([0xc0, 0xaf]), Buffer.from([0xf4, 0x90, 0x80, 0x80]),
];
const name = () => B(chance(0.7) ? pick(SAFE) : pick(NAMES));

function content() {
  switch (int(8)) {
    case 0: return Buffer.alloc(0);
    case 1: return B(pick(['hello', '<h1>${TITLE}</h1>', 'body{}', '\n', 'same', 'same', 'other']));
    case 2: return bytesOf(int(64));
    case 3: return Buffer.alloc(int(chance(0.2) ? 4000 : 600), pick([0, 0x41]));
    case 4: return bytesOf(int(chance(0.1) ? 1500 : 200));
    case 5: return B('x'.repeat(int(700)));
    default: return B(pick(['a', 'b', 'same', '']));
  }
}

const MODES = [0o644, 0o755, 0o600, 0o777, 0o100644, 0o4755, 0, 0o111, 0o7777, 0o1000000, 0o100];

// ---------------------------------------------------------------- tar

function octal(n, width) {
  return n.toString(8).padStart(width - 1, '0').slice(-(width - 1)) + '\0';
}

/** A numeric field in one of the encodings real tar writers (and fuzzers) use. */
function numField(n, width) {
  switch (chance(0.93) ? 0 : int(11)) {
    case 0: return Buffer.from(octal(n, width), 'latin1');
    case 1: return Buffer.from(n.toString(8).padStart(width - 2, ' ') + ' \0', 'latin1').subarray(0, width);
    case 2: { // GNU base-256
      const b = Buffer.alloc(width);
      let v = BigInt(n);
      for (let i = width - 1; i > 0; i--) { b[i] = Number(v & 0xffn); v >>= 8n; }
      b[0] = 0x80;
      return b;
    }
    case 3: return Buffer.from(('+' + n.toString(8)).padEnd(width, '\0'), 'latin1').subarray(0, width);
    case 4: return Buffer.from(('\t' + n.toString(8)).padEnd(width, '\0'), 'latin1').subarray(0, width);
    case 5: return Buffer.from((n.toString(8) + 'x9').padEnd(width, '\0'), 'latin1').subarray(0, width);
    case 6: return Buffer.from(('-' + n.toString(8)).padEnd(width, '\0'), 'latin1').subarray(0, width);
    case 7: return Buffer.alloc(width); // all NUL
    case 8: return Buffer.from(' '.repeat(width), 'latin1');
    case 9: { // base-256 with huge value
      const b = Buffer.alloc(width, 0xff);
      b[0] = pick([0x80, 0xff, 0x81]);
      return b;
    }
    default: return Buffer.from(('\u00a0' + n.toString(8)).padEnd(width, '\0'), 'latin1').subarray(0, width);
  }
}

function checksum(h) {
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 32 : h[i];
  return sum;
}

function tarHeader({ name: n, type = '0', mode = 0o644, size, prefix, magic, link }) {
  const h = Buffer.alloc(512);
  n.copy(h, 0, 0, Math.min(n.length, 100));
  numField(mode, 8).copy(h, 100);
  numField(0, 8).copy(h, 108);
  numField(0, 8).copy(h, 116);
  numField(size, 12).copy(h, 124);
  numField(0, 12).copy(h, 136);
  h[156] = typeof type === 'number' ? type : type.charCodeAt(0);
  if (link) link.copy(h, 157, 0, Math.min(link.length, 100));
  const m = magic ?? pick(['ustar\u000000', 'ustar\u000000', 'ustar  \u0000', '\u0000'.repeat(8), 'ustaR\u000000']);
  h.write(m, 257, 'latin1');
  if (prefix) prefix.copy(h, 345, 0, Math.min(prefix.length, 155));
  const sum = checksum(h);
  const sumField = pick([
    () => sum.toString(8).padStart(6, '0') + '\0 ',
    () => sum.toString(8).padStart(7, '0') + '\0',
    () => sum.toString(8).padStart(6, ' ') + ' \0',
    () => ' ' + sum.toString(8) + '\0',
  ])();
  h.write(sumField.slice(0, 8), 148, 'latin1');
  return h;
}

const pad512 = (n) => Buffer.alloc((512 - (n % 512)) % 512);

function paxBody(records) {
  return Buffer.concat(
    records.map(([k, v]) => {
      const kv = Buffer.concat([B(k), B('='), B(v), B('\n')]);
      let len = kv.length + 2;
      while (String(len).length + 1 + kv.length !== len) len = String(len).length + 1 + kv.length;
      if (chance(0.05)) len += pick([-3, -1, 1, 5, 1000]);
      return Buffer.concat([B(String(len) + ' '), kv]);
    }),
  );
}

function tarEntryBlocks() {
  const out = [];
  const push = (hdr, data) => {
    out.push(tarHeader({ ...hdr, size: hdr.size ?? data.length }), data, pad512(data.length));
  };
  const r = int(30);
  let n = name();
  if (r < 16) {
    // regular file (maybe with a ustar prefix)
    const data = content();
    let prefix;
    if (n.length > 100 && chance(0.6)) {
      const s = n.lastIndexOf(0x2f, 155);
      if (s > 0) { prefix = n.subarray(0, s); n = n.subarray(s + 1); }
    } else if (chance(0.05)) prefix = name();
    push({ name: n, type: pick(['0', '0', '\0', '7']), mode: pick(MODES), prefix }, data);
  } else if (r < 20) {
    push({ name: n, type: '5', mode: 0o755 }, chance(0.9) ? Buffer.alloc(0) : content());
  } else if (r < 23) {
    // GNU long name
    const long = Buffer.concat([name(), B('/'), name(), chance(0.5) ? B('\0') : Buffer.alloc(0)]);
    push({ name: B('././@LongLink'), type: 'L' }, long);
    const data = content();
    push({ name: n, type: pick(['0', '5', '2']) }, data);
  } else if (r < 27) {
    // pax extended header
    const recs = [];
    if (chance(0.8)) recs.push(['path', pick(NAMES).toString()]);
    if (chance(0.3)) recs.push(['size', pick(['0', '5', '12', '-1', '1e3', '', ' 7', '99999999999', '007'])]);
    if (chance(0.3)) recs.push([pick(['mtime', 'uid', 'linkpath', 'comment', '', 'x']), pick(['1', 'é', ''])]);
    if (chance(0.1)) recs.push(['', 'novalue']);
    let body = paxBody(recs);
    if (chance(0.05)) body = Buffer.concat([body, B(pick(['garbage', '12 x', '5 ab\n', '3 a=']))]);
    push({ name: B('PaxHeader/x'), type: pick(['x', 'x', 'g']) }, body);
    const data = content();
    push({ name: n, type: pick(['0', '0', '5']) }, data);
  } else if (r < 28) {
    push({ name: n, type: pick(['1', '2', '2']), link: name() }, Buffer.alloc(0));
  } else if (r < 29) {
    push({ name: n, type: pick(['3', '4', '6', 'K', 'V', 'S', 'A', 'g', 0x80, 0]) }, chance(0.5) ? Buffer.alloc(0) : content());
  } else {
    push({ name: n, type: pick(['g', 'K']) }, content());
    push({ name: name(), type: '0' }, content());
  }
  return out;
}

function tarArchive(bomb = false) {
  const parts = [];
  const n = chance(0.05) ? 0 : 1 + int(chance(0.1) ? 30 : 5);
  for (let i = 0; i < n; i++) parts.push(...tarEntryBlocks());
  if (bomb && chance(0.04)) {
    // A large file: compresses to a bomb inside gzip.
    const big = Buffer.alloc(pick([1 << 20, 1_100_000, 2_500_000]));
    parts.push(tarHeader({ name: B('big.bin'), size: big.length, magic: 'ustar\u000000' }), big, pad512(big.length));
  }
  parts.push(Buffer.alloc(pick([1024, 1024, 512, 0, 1536])));
  if (chance(0.05)) parts.push(bytesOf(int(700)));
  return Buffer.concat(parts);
}

// ---------------------------------------------------------------- gzip

function gzipOf(data) {
  switch (chance(0.7) ? 0 : int(6)) {
    case 0: return gzipSync(data, { level: pick([1, 6, 9]) });
    case 1: { // hand-made header with FNAME / FEXTRA / FCOMMENT / FHCRC
      const flags = pick([0x08, 0x04, 0x10, 0x02, 0x1c, 0xe0]);
      const head = [Buffer.from([0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 3])];
      if (flags & 0x04) head.push(Buffer.from([4, 0, 1, 2, 3, 4]));
      if (flags & 0x08) head.push(B('name.tar\0'));
      if (flags & 0x10) head.push(B('comment\0'));
      if (flags & 0x02) head.push(Buffer.from([0, 0]));
      const tail = Buffer.alloc(8);
      tail.writeUInt32LE(crc32(data), 0);
      tail.writeUInt32LE(data.length >>> 0, 4);
      return Buffer.concat([...head, deflateRawSync(data), tail]);
    }
    case 2: { // two members
      const cut = int(data.length + 1);
      return Buffer.concat([gzipSync(data.subarray(0, cut)), gzipSync(data.subarray(cut))]);
    }
    case 3: return Buffer.concat([gzipSync(data), pick([Buffer.alloc(int(16)), bytesOf(1 + int(30)), Buffer.from([0x1f, 0x8b])])]);
    case 4: { // bad CRC or ISIZE
      const g = gzipSync(data);
      g[g.length - 1 - int(8)] ^= 1 + int(255);
      return g;
    }
    default: return gzipSync(data, { level: 0 });
  }
}

// ---------------------------------------------------------------- zip

function zipEntry() {
  const r = int(20);
  let n = name();
  const data = r < 2 ? Buffer.alloc(0) : content();
  let isDir = false;
  if (r < 4) { isDir = true; if (chance(0.7) && !n.toString().endsWith('/')) n = Buffer.concat([n, B('/')]); }
  const method = isDir ? 0 : pick([0, 8, 8, 8, chance(0.1) ? pick([12, 14, 99, 1, 9]) : 8]);
  const unix = chance(0.8);
  let mode = isDir ? 0o040755 : pick([0o100644, 0o100755, 0o100600, 0o644]);
  if (chance(0.08)) mode = pick([0o120777, 0o010644, 0o020644, 0o060644, 0o140755, 0o170000, 0o040755]);
  return {
    name: n,
    data,
    method,
    unix,
    mode,
    descriptor: chance(0.2),
    descriptorSig: chance(0.5),
    encrypted: chance(0.02),
    zip64: chance(0.04),
    extra: chance(0.1) ? pick([Buffer.from([0x55, 0x54, 5, 0, 1, 0, 0, 0, 0]), bytesOf(int(12)), Buffer.from([1, 0, 8, 0, 1, 2, 3, 4, 5, 6, 7, 8])]) : Buffer.alloc(0),
    comment: chance(0.05) ? B('comment') : Buffer.alloc(0),
    lieUsize: chance(0.05) ? pick([0, 1, data.length + 1, Math.max(0, data.length - 1), 0xfffffffe, 2_000_000, 600 * 1024 * 1024]) : undefined,
    lieCsize: chance(0.04) ? pick([0, 1, 0xffff, 10_000_000]) : undefined,
    lieOffset: chance(0.03) ? pick([0, 1, 0xffffff, 0x7fffffff]) : undefined,
  };
}

function zipArchive() {
  const entries = Array.from({ length: chance(0.04) ? 0 : 1 + int(chance(0.1) ? 25 : 5) }, zipEntry);
  if (chance(0.03)) {
    // A compression bomb: megabytes of zeros, deflated, honest sizes.
    entries.push({ name: B('bomb.bin'), data: Buffer.alloc(pick([1_100_000, 3_000_000])), method: 8, unix: true, mode: 0o100644, extra: Buffer.alloc(0), comment: Buffer.alloc(0) });
  }
  const local = [];
  const central = [];
  let offset = chance(0.03) ? 4 : 0;
  if (offset) local.push(Buffer.from('PK\x05\x06', 'latin1')); // offsets shifted by a 4-byte prelude
  for (const e of entries) {
    const body = e.method === 8 ? deflateRawSync(e.data, { level: pick([1, 6, 9]) }) : e.data;
    const usize = e.lieUsize ?? e.data.length;
    const csize = e.lieCsize ?? body.length;
    const flags = (e.descriptor ? 8 : 0) | (e.encrypted ? 1 : 0) | (chance(0.3) ? 0x800 : 0);
    const crc = crc32(e.data);
    let extra = e.extra;
    let cs = csize;
    let us = usize;
    if (e.zip64) {
      const z = Buffer.alloc(20);
      z.writeUInt16LE(1, 0);
      z.writeUInt16LE(16, 2);
      z.writeBigUInt64LE(BigInt(usize), 4);
      z.writeBigUInt64LE(BigInt(csize), 12);
      extra = Buffer.concat([extra, z]);
      cs = 0xffffffff;
      us = 0xffffffff;
    }
    const l = Buffer.alloc(30);
    l.writeUInt32LE(0x04034b50, 0);
    l.writeUInt16LE(e.zip64 ? 45 : 20, 4);
    l.writeUInt16LE(flags, 6);
    l.writeUInt16LE(e.method, 8);
    l.writeUInt32LE(e.descriptor ? 0 : crc, 14);
    l.writeUInt32LE(e.descriptor ? 0 : cs >>> 0, 18);
    l.writeUInt32LE(e.descriptor ? 0 : us >>> 0, 22);
    l.writeUInt16LE(e.name.length, 26);
    l.writeUInt16LE(extra.length, 28);
    const parts = [l, e.name, extra, body];
    if (e.descriptor) {
      const d = Buffer.alloc(e.descriptorSig ? 16 : 12);
      let o = 0;
      if (e.descriptorSig) { d.writeUInt32LE(0x08074b50, 0); o = 4; }
      d.writeUInt32LE(crc, o);
      d.writeUInt32LE(csize >>> 0, o + 4);
      d.writeUInt32LE(usize >>> 0, o + 8);
      parts.push(d);
    }
    const c = Buffer.alloc(46);
    c.writeUInt32LE(0x02014b50, 0);
    c.writeUInt16LE(((e.unix ? 3 : 0) << 8) | 20, 4);
    c.writeUInt16LE(e.zip64 ? 45 : 20, 6);
    c.writeUInt16LE(flags, 8);
    c.writeUInt16LE(e.method, 10);
    c.writeUInt32LE(crc, 16);
    c.writeUInt32LE(cs >>> 0, 20);
    c.writeUInt32LE(us >>> 0, 24);
    c.writeUInt16LE(e.name.length, 28);
    c.writeUInt16LE(extra.length, 30);
    c.writeUInt16LE(e.comment.length, 32);
    c.writeUInt32LE(e.unix ? ((e.mode << 16) >>> 0) : pick([0, 0x10, 0x20]), 38);
    c.writeUInt32LE((e.lieOffset ?? offset) >>> 0, 42);
    local.push(...parts);
    central.push(c, e.name, extra, e.comment);
    if (chance(0.03)) central.push(c, e.name, extra, e.comment); // duplicate central record
    offset += parts.reduce((s, p) => s + p.length, 0);
  }
  const cd = Buffer.concat(central);
  const count = central.length / 4;
  const tail = [];
  const zip64End = chance(0.04);
  if (zip64End) {
    const z = Buffer.alloc(56);
    z.writeUInt32LE(0x06064b50, 0);
    z.writeBigUInt64LE(44n, 4);
    z.writeUInt16LE(45, 12);
    z.writeUInt16LE(45, 14);
    z.writeBigUInt64LE(BigInt(count), 24);
    z.writeBigUInt64LE(BigInt(count), 32);
    z.writeBigUInt64LE(BigInt(cd.length), 40);
    z.writeBigUInt64LE(BigInt(offset), 48);
    const loc = Buffer.alloc(20);
    loc.writeUInt32LE(0x07064b50, 0);
    loc.writeBigUInt64LE(BigInt(offset + cd.length), 8);
    loc.writeUInt32LE(1, 16);
    tail.push(z, loc);
  }
  const comment = chance(0.08) ? pick([B('zip comment'), bytesOf(int(40)), Buffer.from('PK\x05\x06', 'latin1')]) : Buffer.alloc(0);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  const cnt = zip64End && chance(0.6) ? 0xffff : count & 0xffff;
  end.writeUInt16LE(cnt, 8);
  end.writeUInt16LE(cnt, 10);
  end.writeUInt32LE(cd.length, 12);
  end.writeUInt32LE(zip64End && chance(0.5) ? 0xffffffff : chance(0.03) ? pick([0, offset + 1, 0xfffffff0]) : offset, 16);
  end.writeUInt16LE(chance(0.03) ? comment.length + pick([1, 100]) : comment.length, 20);
  const trailer = chance(0.03) ? bytesOf(int(30)) : Buffer.alloc(0);
  return Buffer.concat([...local, cd, ...tail, end, comment, trailer]);
}

// ---------------------------------------------------------------- CBOR (unsorted, raw)

function head(major, n) {
  const m = major << 5;
  const forms = [];
  if (n < 24) forms.push(Buffer.from([m | n]));
  if (n < 0x100) forms.push(Buffer.from([m | 24, n]));
  if (n < 0x10000) forms.push(Buffer.from([m | 25, n >> 8, n & 0xff]));
  if (n < 0x100000000) { const b = Buffer.alloc(5); b[0] = m | 26; b.writeUInt32BE(n, 1); forms.push(b); }
  const b = Buffer.alloc(9); b[0] = m | 27; b.writeBigUInt64BE(BigInt(n), 1); forms.push(b);
  // Mostly the shortest form; sometimes a longer (non-preferred) one.
  return chance(0.97) ? forms[0] : pick(forms);
}
const cUint = (n) => head(0, n);
const cBytes = (b) => Buffer.concat([head(2, b.length), b]);
const cText = (s) => { const b = B(s); return Buffer.concat([head(3, b.length), b]); };
const cArray = (items) => Buffer.concat([head(4, items.length), ...items]);
const cMap = (pairs) => Buffer.concat([head(5, pairs.length), ...pairs.flat()]);
/** Something that is not what the reader expects. */
const cJunk = () => pick([
  () => Buffer.from([0xf6]), () => Buffer.from([0xf5]), () => Buffer.from([0x20]), () => Buffer.from([0x3b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
  () => Buffer.from([0xfb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0]), () => Buffer.from([0xc0, 0x60]), () => Buffer.from([0x5f, 0x41, 0x61, 0xff]),
  () => Buffer.from([0x9f, 0xff]), () => Buffer.from([0x1c]), () => Buffer.from([0x7f, 0xff]), () => cText('x'), () => cBytes(B('x')),
  () => cArray([]), () => cMap([]), () => Buffer.from([0x63, 0xff, 0xfe, 0x41]), () => Buffer.from([0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
  () => Buffer.from([0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00]),
  () => Buffer.from([0xbf, 0xff]), () => Buffer.from([0xf8, 0x20]), () => Buffer.from([0xf9, 0x3c, 0x00]),
])();

// ---------------------------------------------------------------- web bundles

const HOSTS = [
  'e.x', 'e.x', 'e.x', 'f.y', 'E.X', 'e.x:443', 'e.x:8080', 'e.x:', 'e.x:0', 'e.x:65536', 'e.x:08', '127.0.0.1', '0x7f.1', '127.1', '2130706433',
  '1.2.3.4.', '256.1.1.1', '1.2.3.09', '0x', '[::1]', '[::ffff:1.2.3.4]', '[1:2:3:4:5:6:7:8]', '[::1', '[]', 'xn--caf-dma.x', 'caf\u00e9.x', 'CAF\u00c9.X',
  '\u0130.x', 'e%2Ex', 'e%41.x', 'e%zz.x', 'a b.x', 'e.x.', '.e.x', 'e..x', 'u:p@e.x', '@e.x', '', 'e_x', 'e\u3002x', '\u212a.x', 'xn--', 'xn--a.x',
  '\ufeffe.x', 'e.x\t', '\u00ad.x', 'e.x#', '1.2.3', '09.1', 'e.123', 'e.0x1', '%30.%30.%30.%30', '0300.0250.0.1', '\uff45.x',
];
const PATHS = [
  '/', '/', '/index.html', '/a.html', '/style.css', '/a/', '/a/b.txt', '/A.html', '/a.HTML', '', '/%7Ea', '/caf%C3%A9', '/cafe%CC%81', '/caf\u00e9',
  '/%2e%2e/x', '/%2E/x', '/..', '/./a', '/a//b', '/a%2Fb', '/a%5Cb', '/a\\b', '/%00', '/%zz', '/%C3', '/%ED%A0%80', '/a%20b', '/a b', '/\ud83d\ude00',
  '/x?q=1', '/x?', '/x#f', '/x?q#f', '/index.html?v=2', '/a/?x', '/%E2%80%8B', '/K', '/\u212a', '/stra%C3%9Fe', '/STRASSE', 'rel', '/a\n',
];
const SCHEMES = ['https://', 'https://', 'https://', 'http://', 'HTTPS://', 'Http://', 'ftp://', 'data:', 'https:', 'https:/', 'urn:', 'file://', 'ws://', '1x://', '://', ''];

/** Absolute or not, by the URL parser's rules (trimming, tabs, opaque hosts, file hosts). */
const ODD_URLS = [
  '\u0000https://e.x/', ' https://e.x/ ', 'ht\ttps://e.x/', 'https://e.\nx/', '\u0001\u0002tps://e.x/', 'x://a b/', 'x://a%20b/', 'x://[::1]:99999/',
  'x://h:80/', 'x://h:8a/', 'x://u@h/', 'x://u:p@h:1/', 'x://[::1/', 'x://h^/', 'x:///', 'x://', 'file:///a', 'file://h/a', 'file://h:1/a',
  'file://u@h/a', 'file://localhost/a', 'file:\\\\h\\a', 'file://[::1]/a', 'file://h%41/a', 'file://ab%zz/', 'file:a', 'ws://e.x:65535/', 'wss://e.x:65536/',
  'ftp://e.x/', 'https://e.x:/', 'https://:80/', 'x:/a', '1:x', 'x\u00e9://h/', 'https://e.x\u0000/',
];

function url() {
  if (chance(0.02)) return pick(ODD_URLS);
  if (chance(0.03)) return pick(['', 'https://', '\ufeffhttps://e.x/a', 'https://e.x/a\u2028', 'javascript:alert(1)', 'about:blank', 'https://e.x/#\n']);
  return pick(SCHEMES) + pick(HOSTS) + pick(PATHS);
}

const HEADER_NAMES = ['content-type', 'content-type', 'x-b', 'etag', 'content-type', 'content-type', 'cache-control', 'x-a', 'Content-Type', ':status', ':path', '', '1', '10', 'x\u00e9', 'x a', 'x\u007f'];
const HEADER_VALUES = ['text/html', 'text/plain', 'v1', 'text/html', 'text/css; charset=utf-8', '', 'a\u00e9', '\u0001', 'no-store', 'x'.repeat(30)];
const STATUS = ['200', '200', '200', '200', '404', '301', '20', '2000', '20x', '099', ' 200', '\u0662\u0660\u0660'];

function response(body) {
  const pairs = [];
  if (chance(0.97)) pairs.push([cBytes(B(':status')), cBytes(B(chance(0.85) ? '200' : pick(STATUS)))]);
  for (let i = int(3); i > 0; i--) pairs.push([cBytes(B(pick(HEADER_NAMES))), cBytes(B(pick(HEADER_VALUES)))]);
  if (chance(0.02)) pairs.push([cText('content-type'), cBytes(B('x'))]);
  if (chance(0.5)) pairs.reverse();
  const hdr = chance(0.02) ? cJunk() : cMap(pairs);
  const items = [cBytes(hdr), chance(0.02) ? cText('body') : cBytes(body)];
  if (chance(0.02)) items.push(cBytes(B('')));
  return cArray(items);
}

function webBundle() {
  const b1 = chance(0.35);
  const n = chance(0.05) ? 0 : 1 + int(chance(0.1) ? 20 : 5);
  const resp = [];
  const index = [];
  let off = 1; // head of the responses array, fixed up below
  const entries = Array.from({ length: n }, () => ({ url: url(), body: content() }));
  const encoded = entries.map((e) => response(e.body));
  const respHead = head(4, encoded.length);
  off = respHead.length;
  const locs = [];
  for (const r of encoded) { locs.push([off, r.length]); off += r.length; resp.push(r); }
  for (let i = 0; i < n; i++) {
    let [o, l] = locs[i];
    if (chance(0.03)) o += pick([1, -1, 1000, 0x7fffffff]);
    if (chance(0.03)) l += pick([1, -1, 1000]);
    if (chance(0.03) && i > 0) [o, l] = locs[0]; // shares another entry's response
    o = Math.max(0, o);
    l = Math.max(0, l);
    let value;
    if (b1) {
      value = chance(0.04) ? cArray([cBytes(B('')), cUint(o), cUint(l), cUint(o), cUint(l)]) : cArray([cBytes(chance(0.97) ? Buffer.alloc(0) : B('v')), cUint(o), cUint(l)]);
      if (chance(0.02)) value = cArray([cUint(o), cUint(l)]);
    } else {
      value = chance(0.98) ? cArray([cUint(o), cUint(l)]) : pick([() => cArray([cUint(o)]), () => cArray([cBytes(B('')), cUint(o), cUint(l)]), cJunk])();
    }
    const key = chance(0.02) ? cBytes(B(entries[i].url)) : cText(entries[i].url);
    index.push([key, value]);
    if (chance(0.03)) index.push([key, value]); // duplicate key
  }
  if (chance(0.5)) index.reverse();
  const indexSection = chance(0.02) ? cJunk() : cMap(index);
  const responsesSection = Buffer.concat([respHead, ...resp]);
  const primaryUrl = chance(0.8) ? (chance(0.9) ? pick(['https://e.x/', 'https://e.x/index.html']) : pick(['not a url', '', 'https://[::1', 'urn:x', url(), url(), pick(ODD_URLS)])) : undefined;

  let sections = [['index', indexSection]];
  if (!b1 && primaryUrl !== undefined) sections.push(['primary', chance(0.97) ? cText(primaryUrl) : cJunk()]);
  if (chance(0.1)) sections.push([pick(['manifest', 'signatures', 'critical', 'x', 'index']), pick([cText('https://e.x/m.json'), cArray([]), cJunk()])]);
  sections.push(['responses', responsesSection]);
  if (chance(0.03)) sections.reverse();
  if (chance(0.02)) sections = sections.filter(([s]) => s !== 'index');
  const lengths = sections.flatMap(([s, bytes]) => [cText(s), cUint(chance(0.02) ? bytes.length + pick([1, -1]) : bytes.length)]);
  let sl = cArray(lengths);
  if (chance(0.02)) sl = pick([cJunk(), cArray([...lengths, cText('extra')]), Buffer.concat([sl, B('x')])]);

  const parts = [
    Buffer.from([b1 ? 0x86 : 0x85]),
    cBytes(Buffer.from('f09f8c90f09f93a6', 'hex')),
    cBytes(Buffer.from(chance(0.97) ? (b1 ? 'b1\0\0' : 'b2\0\0') : pick(['b1\0\0', 'b2\0\0', 'b3\0\0', 'b2\0\x01', '1\0\0\0']), 'latin1')),
  ];
  if (b1) parts.push(chance(0.97) ? cText(primaryUrl ?? 'https://e.x/') : cJunk());
  parts.push(cBytes(sl), cArray(sections.map(([, s]) => s).slice(0, chance(0.98) ? undefined : -1)));
  const body = Buffer.concat(parts);
  const len = Buffer.alloc(8);
  len.writeBigUInt64BE(BigInt(body.length + 9 + (chance(0.03) ? pick([1, -1, 100]) : 0)));
  return Buffer.concat([body, Buffer.from([chance(0.98) ? 0x48 : 0x58]), len]);
}

/** After a mutation the trailing length field can be fixed so the reader looks deeper. */
function fixBundleLength(b) {
  if (b.length < 19) return b;
  const out = Buffer.from(b);
  out.writeBigUInt64BE(BigInt(out.length), out.length - 8);
  return out;
}

// ---------------------------------------------------------------- mutations

const INTERESTING = [0, 0xff, 0x7f, 0x80, 0x30, 0x37, 0x20, 0x2b, 0x2d, 0x09, 0x2f, 0x5c, 0x2e, 0x50, 0x4b, 0x1f, 0x8b, 0x18, 0x19, 0x1a, 0x1b, 0x1f];

function mutate(input) {
  let b = Buffer.from(input);
  const rounds = 1 + int(chance(0.2) ? 8 : 2);
  for (let r = 0; r < rounds && b.length > 0; r++) {
    const at = int(b.length);
    switch (int(11)) {
      case 0: b[at] ^= 1 << int(8); break;
      case 1: b[at] = pick(INTERESTING); break;
      case 2: b = b.subarray(0, int(b.length)); break; // truncate
      case 3: { const len = 1 + int(Math.min(64, b.length - at)); b = Buffer.concat([b.subarray(0, at), b.subarray(at + len)]); break; }
      case 4: b = Buffer.concat([b.subarray(0, at), bytesOf(1 + int(16)), b.subarray(at)]); break;
      case 5: { const len = 1 + int(Math.min(512, b.length - at)); b = Buffer.concat([b.subarray(0, at + len), b.subarray(at, at + len), b.subarray(at + len)]); break; }
      case 6: if (at + 4 <= b.length) b.writeUInt32LE(pick([0, 1, 0xffffffff, 0x7fffffff, 0x80000000, b.length, b.length + 1, 0xffff, 46, 30]) >>> 0, at); break;
      case 7: if (at + 2 <= b.length) b.writeUInt16LE(pick([0, 1, 0xffff, 0x7fff, 100, 512]), at); break;
      case 8: { const from = int(b.length); const len = 1 + int(Math.min(40, b.length - from)); b.copy(b, at, from, from + len); break; }
      case 9: b = Buffer.concat([b, bytesOf(int(64))]); break;
      default: b[at] = int(256);
    }
  }
  return b;
}

/** Mutate one tar header and (usually) fix its checksum so the reader parses it. */
function mutateTar(t) {
  const b = Buffer.from(t);
  const blocks = Math.floor(b.length / 512);
  if (blocks === 0) return b;
  for (let r = 1 + int(3); r > 0; r--) {
    const h = int(blocks) * 512;
    const field = pick([[0, 100], [100, 8], [124, 12], [124, 12], [156, 1], [257, 8], [345, 155], [148, 8]]);
    const at = h + field[0] + int(field[1]);
    b[at] = chance(0.5) ? pick(INTERESTING) : int(256);
    if (chance(0.85) && b.subarray(h, h + 512).some((x) => x !== 0)) {
      const sum = checksum(b.subarray(h, h + 512));
      b.write(sum.toString(8).padStart(6, '0') + '\0 ', h + 148, 'latin1');
    }
  }
  return b;
}

function noise() {
  const prefixes = [Buffer.alloc(0), Buffer.from([0x1f, 0x8b]), Buffer.from('PK\x03\x04', 'latin1'), Buffer.from('PK\x05\x06', 'latin1'), Buffer.from([0x85, 0x48, 0xf0, 0x9f, 0x8c, 0x90, 0xf0, 0x9f, 0x93, 0xa6]), Buffer.from([0x86, 0x48, 0xf0, 0x9f, 0x8c, 0x90, 0xf0, 0x9f, 0x93, 0xa6, 0x44, 0x62, 0x31, 0, 0])];
  const body = chance(0.3) ? Buffer.alloc(pick([0, 1, 511, 512, 1023, 1024, 1025])) : bytesOf(int(chance(0.2) ? 2000 : 200));
  return Buffer.concat([pick(prefixes), body]);
}

// ---------------------------------------------------------------- limits

const DEFAULT = [10_000, 500 * 1024 * 1024, 100];
function limits() {
  if (chance(0.75)) return DEFAULT;
  return [
    pick([0, 1, 2, 3, 5, 10, 10_000]),
    pick([0, 1, 10, 100, 1000, 4096, 65536, 1 << 20, 3_000_000, 500 * 1024 * 1024]),
    pick([1, 2, 10, 100, 1000]),
  ];
}

// ---------------------------------------------------------------- cases

function fuzzCase() {
  const kind = int(100);
  let bytes;
  if (kind < 4) bytes = noise();
  else if (kind < 30) {
    bytes = tarArchive();
    if (chance(0.4)) bytes = mutateTar(bytes);
    else if (chance(0.3)) bytes = mutate(bytes);
  } else if (kind < 50) {
    let t = tarArchive(true);
    if (chance(0.3)) t = mutateTar(t);
    bytes = gzipOf(t);
    if (chance(0.15)) bytes = mutate(bytes);
  } else if (kind < 75) {
    bytes = zipArchive();
    if (chance(0.4)) bytes = mutate(bytes);
  } else {
    bytes = webBundle();
    if (chance(0.4)) {
      bytes = mutate(bytes);
      if (chance(0.6)) bytes = fixBundleLength(bytes);
    }
  }
  return [bytes, limits()];
}

// ---------------------------------------------------------------- fixed cases

const tarOf = (...entries) =>
  Buffer.concat([
    ...entries.flatMap(([n, type, data]) => [tarHeader({ name: B(n), type, size: data.length, magic: 'ustar\u000000' }), data, pad512(data.length)]),
    Buffer.alloc(1024),
  ]);
/**
 * A stored zip of `[name, data, { flags, extra, method, body, crc, localName, localMethod, localSizes, offset }]`
 * entries, honest unless told otherwise (`local*` lie in the local header only, `offset` in the central one).
 */
function zipOf(...entries) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [n, data, o = {}] of entries) {
    const name = B(n);
    const extra = o.extra ?? Buffer.alloc(0);
    const body = o.body ?? data;
    const l = Buffer.alloc(30);
    l.writeUInt32LE(0x04034b50, 0);
    l.writeUInt16LE(20, 4);
    const lname = o.localName ? B(o.localName) : name;
    const [lc, lu] = o.localSizes ?? [body.length, data.length];
    l.writeUInt16LE(o.flags ?? 0, 6);
    l.writeUInt16LE(o.localMethod ?? o.method ?? 0, 8);
    l.writeUInt32LE(o.crc ?? crc32(data), 14);
    l.writeUInt32LE(lc, 18);
    l.writeUInt32LE(lu, 22);
    l.writeUInt16LE(lname.length, 26);
    l.writeUInt16LE(extra.length, 28);
    const c = Buffer.alloc(46);
    c.writeUInt32LE(0x02014b50, 0);
    c.writeUInt16LE((3 << 8) | 20, 4);
    c.writeUInt16LE(20, 6);
    c.writeUInt16LE(o.flags ?? 0, 8);
    c.writeUInt16LE(o.method ?? 0, 10);
    c.writeUInt32LE(o.crc ?? crc32(data), 16);
    c.writeUInt32LE(body.length, 20);
    c.writeUInt32LE(data.length, 24);
    c.writeUInt16LE(name.length, 28);
    c.writeUInt16LE(extra.length, 30);
    c.writeUInt32LE((0o100644 << 16) >>> 0, 38);
    c.writeUInt32LE(o.offset ?? offset, 42);
    local.push(l, lname, extra, body);
    central.push(c, name, extra);
    offset += 30 + lname.length + extra.length + body.length;
  }
  const cd = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(cd.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...local, cd, end]);
}
/**
 * A zip as a forced-ZIP64 streaming writer (Node archiver/zip-stream with forceZip64, Java) writes it:
 * data descriptors, central sizes and offsets 0xffffffff with a ZIP64 extra field, a zip64 end record
 * and locator, and 0xffff/0xffffffff in the end record. `o` breaks one part: `noExtra`, `shortExtra`,
 * `badLocator`, `keepEocd` (real counts in the end record), `noLocator`.
 */
function zip64Of(entries, o = {}) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [n, data] of entries) {
    const name = B(n);
    const body = deflateRawSync(data);
    const l = Buffer.alloc(30);
    l.writeUInt32LE(0x04034b50, 0);
    l.writeUInt16LE(45, 4);
    l.writeUInt16LE(8, 6);
    l.writeUInt16LE(8, 8);
    l.writeUInt16LE(name.length, 26);
    const d = Buffer.alloc(24);
    d.writeUInt32LE(0x08074b50, 0);
    d.writeUInt32LE(crc32(data), 4);
    d.writeBigUInt64LE(BigInt(body.length), 8);
    d.writeBigUInt64LE(BigInt(data.length), 16);
    let z = Buffer.alloc(28);
    z.writeUInt16LE(1, 0);
    z.writeUInt16LE(24, 2);
    z.writeBigUInt64LE(BigInt(data.length), 4);
    z.writeBigUInt64LE(BigInt(body.length), 12);
    z.writeBigUInt64LE(BigInt(offset), 20);
    if (o.noExtra) z = Buffer.alloc(0);
    if (o.shortExtra) { z = z.subarray(0, 20); z.writeUInt16LE(16, 2); }
    const c = Buffer.alloc(46);
    c.writeUInt32LE(0x02014b50, 0);
    c.writeUInt16LE((3 << 8) | 45, 4);
    c.writeUInt16LE(45, 6);
    c.writeUInt16LE(8, 8);
    c.writeUInt16LE(8, 10);
    c.writeUInt32LE(crc32(data), 16);
    c.writeUInt32LE(0xffffffff, 20);
    c.writeUInt32LE(0xffffffff, 24);
    c.writeUInt16LE(name.length, 28);
    c.writeUInt16LE(z.length, 30);
    c.writeUInt32LE((0o100644 << 16) >>> 0, 38);
    c.writeUInt32LE(0xffffffff, 42);
    local.push(l, name, body, d);
    central.push(c, name, z);
    offset += 30 + name.length + body.length + d.length;
  }
  const cd = Buffer.concat(central);
  const r = Buffer.alloc(56);
  r.writeUInt32LE(0x06064b50, 0);
  r.writeBigUInt64LE(44n, 4);
  r.writeUInt16LE(45, 12);
  r.writeUInt16LE(45, 14);
  r.writeBigUInt64LE(BigInt(entries.length), 24);
  r.writeBigUInt64LE(BigInt(entries.length), 32);
  r.writeBigUInt64LE(BigInt(cd.length), 40);
  r.writeBigUInt64LE(BigInt(offset), 48);
  const loc = Buffer.alloc(20);
  loc.writeUInt32LE(0x07064b50, 0);
  loc.writeBigUInt64LE(BigInt(o.badLocator ? offset + cd.length + 1 : offset + cd.length), 8);
  loc.writeUInt32LE(1, 16);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(o.keepEocd ? entries.length : 0xffff, 8);
  end.writeUInt16LE(o.keepEocd ? entries.length : 0xffff, 10);
  end.writeUInt32LE(0xffffffff, 12);
  end.writeUInt32LE(o.keepEocd ? offset : 0xffffffff, 16);
  return Buffer.concat([...local, cd, ...(o.noLocator ? [] : [r, loc]), end]);
}

/** Info-ZIP Unicode Path extra field (0x7075) for raw name `raw`, naming `unicode`. */
function unicodePath(raw, unicode, { crc = crc32(B(raw)), version = 1 } = {}) {
  const u = B(unicode);
  const f = Buffer.alloc(9);
  f.writeUInt16LE(0x7075, 0);
  f.writeUInt16LE(5 + u.length, 2);
  f[4] = version;
  f.writeUInt32LE(crc, 5);
  return Buffer.concat([f, u]);
}
const CP437_ALL = Buffer.from(Array.from({ length: 128 }, (_, i) => 0x80 + i));

/** Regressions and boundaries, always in the committed fixture. */
const EDGE = [
  // A pax record without a length prefix's space after a complete record (TypeScript looped forever).
  () => tarOf(['PaxHeader/x', 'x', B('11 path=a\u0000b\n')], ['a.txt', '0', B('hi')]),
  () => tarOf(['PaxHeader/x', 'x', B('nospace')], ['a.txt', '0', B('hi')]),
  () => tarOf(['PaxHeader/x', 'x', B('12 path=p.txt\n12 size=1')], ['a.txt', '0', B('hi')]),
  () => tarOf(['a.txt', '0', B('a')], ['A.TXT', '0', B('b')]),
  () => tarOf(['caf\u00e9', '0', B('a')], ['cafe\u0301', '0', B('a')]),
  () => tarOf(['x', '0', B('a')], ['x/y', '0', B('b')]),
  () => tarOf(['d/', '5', B('')], ['d', '0', B('b')]),
  () => tarOf(['l', '2', B('')]),
  () => tarOf(['h', '1', B('')]),
  () => Buffer.alloc(1024),
  () => gzipSync(Buffer.alloc(1024)),
  () => gzipSync(tarOf(['big', '0', Buffer.alloc(1_200_000)])),
  // Zip names (#182): CP437 without bit 11, UTF-8 with it, UTF-8 without it, the 0x7075 Unicode Path field.
  () => zipOf([CP437_ALL, B('all of CP437')], [Buffer.from([0x63, 0x61, 0x66, 0x82]), B('caf\u00e9 in CP437')]),
  () => zipOf([CP437_ALL, B('flagged: lossy UTF-8'), { flags: 0x800 }]),
  () => zipOf([B('caf\u00e9.txt'), B('UTF-8 without the flag')], [B('\u{1f600}'), B('x'), { flags: 0x800 }]),
  () => zipOf([Buffer.from([0x82]), B('a'), { extra: unicodePath(Buffer.from([0x82]), 'caf\u00e9/\u00e9t\u00e9.txt') }]),
  () => zipOf([Buffer.from([0x82]), B('a'), { extra: Buffer.concat([B('UT\u0005\u0000\u0001\u0000\u0000\u0000\u0000'), unicodePath(Buffer.from([0x82]), 'right.txt')]) }]),
  () => zipOf([Buffer.from([0x82]), B('bad crc'), { extra: unicodePath(Buffer.from([0x82]), 'wrong.txt', { crc: 1 }) }]),
  () => zipOf([Buffer.from([0x82]), B('version 2'), { extra: unicodePath(Buffer.from([0x82]), 'wrong.txt', { version: 2 }) }]),
  () => zipOf([Buffer.from([0x82]), B('invalid UTF-8'), { extra: Buffer.concat([unicodePath(Buffer.from([0x82]), 'x').subarray(0, 9), Buffer.from([0xff])]).fill(6, 2, 3).fill(0, 3, 4) }]),
  () => zipOf([B('ascii.txt'), B('flag set: the field is ignored'), { flags: 0x800, extra: unicodePath(B('ascii.txt'), 'other.txt') }]),
  () => zipOf([Buffer.from([0x82]), B('truncated field'), { extra: unicodePath(Buffer.from([0x82]), 'x.txt').subarray(0, 12) }]),
  () => zipOf([Buffer.from([0x82, 0x2f, 0x2e, 0x2e, 0x2f, 0x78]), B('unsafe after decoding')]),
  () => zipOf([Buffer.from([0x82]), B('unsafe via 0x7075'), { extra: unicodePath(Buffer.from([0x82]), '../evil') }]),
  () => zipOf([Buffer.from([0x82]), B('1')], [B('\u00e9'), B('2')]),
  // CRC-32 (#183): a stored entry with a flipped byte, a deflated one with a wrong CRC, an empty one.
  () => zipOf([B('index.html'), B('<h1>hello</h1>'), { body: B('<H1>hello</h1>') }]),
  () => zipOf([B('a.txt'), B('hello hello hello'), { method: 8, body: deflateRawSync(B('hello hello hello')), crc: 0x12345678 }]),
  () => zipOf([B('ok.txt'), B('fine')], [B('empty'), B(''), { crc: 1 }]),
  // Local headers vs the central directory, and overlapping entries (#184).
  () => zipOf([B('index.html'), B('hi'), { localName: 'evil.html' }]),
  () => zipOf([B('index.html'), B('hi'), { localName: 'INDEX.HTML' }]),
  () => zipOf([B('a.txt'), B('hi'), { localMethod: 8 }]),
  () => zipOf([B('a.txt'), B('hi'), { localSizes: [2, 3] }]),
  () => zipOf([B('a.txt'), B('hi'), { flags: 8, localSizes: [0, 0] }]), // data descriptor: local sizes may be 0
  () => zipOf([B('a.txt'), B('hi'), { flags: 8, localSizes: [7, 7] }]),
  () => zipOf([B('a.txt'), B('hello')], [B('a.txt'), B('hello'), { offset: 0 }]), // two records, one local entry
  // b.txt's local header and data lie inside a.txt's data (a non-recursive zip bomb's overlap).
  () => zipOf([B('a.txt'), zipOf([B('b.txt'), B('x')]).subarray(0, 36)], [B('b.txt'), B('x'), { offset: 35 }]),
  () => zipOf([B('d/'), B('')], [B('d/x'), B('y')]),
  // Python zipfile, force_zip64=True: 0xffffffff local sizes with a local ZIP64 field; seekable and streamed (data descriptors).
  () => Buffer.from('UEsDBC0AAAAIAAAAIVATjxBo//////////8PABQAc2l0ZS9pbmRleC5odG1sAQAQAAsAAAAAAAAACwAAAAAAAACzyTC0y8i00QdSAFBLAwQtAAAACAAAACFQe8wlO///////////CgAUAHNpdGUvYS5jc3MBABAAlgAAAAAAAAAIAAAAAAAAAEusrk0cfAgAUEsBAi0DLQAAAAgAAAAhUBOPEGgLAAAACwAAAA8AAAAAAAAAAAAAAIABAAAAAHNpdGUvaW5kZXguaHRtbFBLAQItAy0AAAAIAAAAIVB7zCU7CAAAAJYAAAAKAAAAAAAAAAAAAACAAUwAAABzaXRlL2EuY3NzUEsFBgAAAAACAAIAdQAAAJAAAAAAAA==', 'base64'),
  () => Buffer.from('UEsDBC0ACAAIAAAAIVAAAAAA//////////8PABQAc2l0ZS9pbmRleC5odG1sAQAQAAAAAAAAAAAAAAAAAAAAAACzyTC0y8i00QdSAFBLBwgTjxBoCwAAAAAAAAALAAAAAAAAAFBLAwQtAAgACAAAACFQAAAAAP//////////CgAUAHNpdGUvYS5jc3MBABAAAAAAAAAAAAAAAAAAAAAAAEusrk0cfAgAUEsHCHvMJTsIAAAAAAAAAJYAAAAAAAAAUEsBAi0DLQAIAAgAAAAhUBOPEGgLAAAACwAAAA8AAAAAAAAAAAAAAIABAAAAAHNpdGUvaW5kZXguaHRtbFBLAQItAy0ACAAIAAAAIVB7zCU7CAAAAJYAAAAKAAAAAAAAAAAAAACAAWQAAABzaXRlL2EuY3NzUEsFBgAAAAACAAIAdQAAAMAAAAAAAA==', 'base64'),
  // ZIP64 (#185): a forced-ZIP64 streaming writer, and broken variants.
  () => zip64Of([['site/index.html', B('<h1>hi</h1>')], ['site/a.css', B('a{}'.repeat(50))]]),
  () => zip64Of([['site/index.html', B('<h1>hi</h1>')]], { keepEocd: true }),
  () => zip64Of([['a.txt', B('x')]], { noExtra: true }),
  () => zip64Of([['a.txt', B('x')]], { shortExtra: true }),
  () => zip64Of([['a.txt', B('x')]], { badLocator: true }),
  () => zip64Of([['a.txt', B('x')]], { noLocator: true }),
  () => zip64Of(Array.from({ length: 3 }, (_, i) => [`f${i}`, B('x')])),
];

const fuzzOut = process.env.FUZZ_OUT;
const count = Number(process.env.FUZZ_COUNT ?? 400);
const seen = new Set();
let written = 0;
const tally = { ok: 0, error: 0, crash: 0, violation: 0 };
const codes = {};
const lines = [];
const edge = fuzzOut ? [] : EDGE.flatMap((f) => { const b = f(); return [[b, DEFAULT], [b, [1, 100, 1]]]; });
for (let n = 0; written < count + edge.length && n < count * 4; n++) {
  const [bytes, l] = n < edge.length ? edge[n] : fuzzCase();
  const input = bytes.toString('base64');
  if (!fuzzOut && n >= edge.length && input.length > 4000) continue; // keep the committed file small
  const key = input + l.join(',');
  if (seen.has(key)) continue;
  seen.add(key);
  const c = await record(lib, input, l);
  const k = Object.keys(c.result)[0];
  tally[k]++;
  if (k === 'error') codes[c.result.error[0]] = (codes[c.result.error[0]] ?? 0) + 1;
  lines.push(JSON.stringify(c));
  written++;
}
writeFileSync(fuzzOut ?? path.join(here, 'fuzz.json'), '[\n' + lines.join(',\n') + '\n]\n');
console.log(`${written} archives: ${JSON.stringify(tally)} ${JSON.stringify(codes)}`);
