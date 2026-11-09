import { test } from 'node:test';
import assert from 'node:assert/strict';
import { crc32, gzipSync } from 'node:zlib';
import { extractArchive } from '../src/resources/archive.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits } from '../src/resources/types.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { tar, tarHeader, zip } from './archive-fixtures.js';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
const paths = (t: Awaited<ReturnType<typeof extractArchive>>): string[] => t.entries().map(([p]) => p);

async function rejects(bytes: Uint8Array, code: ErrorCode, match: RegExp, limits: Partial<ArchiveLimits> = {}, origin = 'test'): Promise<void> {
  await assert.rejects(
    () => extractArchive(bytes, origin, { ...DEFAULT_ARCHIVE_LIMITS, ...limits }),
    (e: unknown) => {
      assert.ok(isSitefileError(e), String(e));
      assert.equal(e.code, code, e.message);
      assert.match(e.message, match);
      return true;
    },
  );
}

const sample = [
  { name: 'site/', type: '5' },
  { name: 'site/index.html', data: '<h1>hi</h1>' },
  { name: 'site/bin/run', data: '#!/bin/sh', mode: 0o700 },
  { name: 'site/css/a.css', data: 'a{}', mode: 0o600 },
];

test('tar, tgz and zip extract to the same tree; modes are normalized; top dir is kept', async () => {
  const zipEntries = [
    { name: 'site/', mode: 0o040755 },
    { name: 'site/index.html', data: '<h1>hi</h1>', deflate: true },
    { name: 'site/bin/run', data: '#!/bin/sh', mode: 0o100700 },
    { name: 'site/css/a.css', data: 'a{}', deflate: true, mode: 0o100600 },
  ];
  const trees = await Promise.all([extractArchive(tar(sample), 't'), extractArchive(gzipSync(tar(sample)), 't'), extractArchive(zip(zipEntries), 't')]);
  for (const t of trees) {
    assert.deepEqual(paths(t), ['/site/bin/run', '/site/css/a.css', '/site/index.html']);
    assert.equal(text(t.get('/site/index.html')?.bytes), '<h1>hi</h1>');
    assert.equal(t.get('/site/bin/run')?.mode, 0o755);
    assert.equal(t.get('/site/css/a.css')?.mode, 0o644);
  }
  assert.equal(trees[0]!.digest(), trees[1]!.digest());
  assert.equal(trees[0]!.digest(), trees[2]!.digest());
});

test('empty zip and empty tar give an empty tree; ./ prefixes are normalized', async () => {
  assert.equal((await extractArchive(zip([]), 't')).size, 0);
  assert.equal((await extractArchive(tar([]), 't')).size, 0);
  assert.deepEqual(paths(await extractArchive(tar([{ name: './a/b.txt', data: 'x' }]), 't')), ['/a/b.txt']);
});

test('pax and GNU long names are supported', async () => {
  const long = 'deep/' + 'd'.repeat(150) + '/file.txt';
  const body = ` path=${long}\n`;
  let len = body.length + 1;
  while (String(len).length + body.length !== len) len = String(len).length + body.length;
  const viaPax = tar([{ name: 'PaxHeader', type: 'x', data: `${len}${body}` }, { name: 'short', data: 'p' }]);
  assert.deepEqual(paths(await extractArchive(viaPax, 't')), ['/' + long]);
  const viaGnu = tar([{ name: '././@LongLink', type: 'L', data: long + '\0' }, { name: 'short', data: 'g' }]);
  assert.deepEqual(paths(await extractArchive(viaGnu, 't')), ['/' + long]);
});

test('unknown format and non-tar gzip are E_TYPE', async () => {
  await rejects(Buffer.from('just some text, not an archive'), 'E_TYPE', /not a recognized archive/);
  await rejects(new Uint8Array(0), 'E_TYPE', /not a recognized archive/);
  await rejects(gzipSync('hello'), 'E_TYPE', /not a tar archive/);
});

/** test/fixtures/gzip-wbn (from the source tree: the compiled test runs from lib/test). */
const GZIP_WBN = fileURLToPath(new URL('../../test/fixtures/gzip-wbn/', import.meta.url));

test('gzip-wrapped Web Bundles (fileable encode="wbn", packfile toArchive) import like plain ones (#141)', async () => {
  const expected = JSON.parse(readFileSync(`${GZIP_WBN}expected.json`, 'utf8')) as Record<string, { tree: string; files: Record<string, string> }>;
  assert.deepEqual(Object.keys(expected).sort(), ['fileable-encode-wbn.wbn', 'packfile-toarchive.wbn']);
  for (const [name, want] of Object.entries(expected)) {
    const bytes = readFileSync(`${GZIP_WBN}${name}`);
    assert.deepEqual([...bytes.subarray(0, 2)], [0x1f, 0x8b], `${name} is gzip`);
    const t = await extractArchive(bytes, name);
    assert.deepEqual(Object.fromEntries(t.entries().map(([p, n]) => [p, n.digest])), want.files, name);
    assert.equal(t.digest(), want.tree, name);
  }
  // the bundle's own limits apply to the decompressed stream
  const bundle = readFileSync(`${GZIP_WBN}packfile-toarchive.wbn`);
  await rejects(bundle, 'E_ARCHIVE_LIMIT', /^packfile: web bundle has more than 2 entries$/, { maxEntries: 2 }, 'packfile');
  await rejects(bundle, 'E_ARCHIVE_LIMIT', /^packfile: web bundle expands beyond 20 bytes$/, { maxTotalBytes: 20 }, 'packfile');
});

test('unsafe tar paths are rejected', async () => {
  for (const name of ['../evil', 'a/../../evil', '/etc/passwd', 'a\\b', 'C:/x', 'a/..']) {
    await rejects(tar([{ name, data: 'x' }]), 'E_ARCHIVE_UNSAFE', /unsafe archive entry/);
  }
  // A NUL cannot appear in a plain tar name (it terminates it) but can in a pax path.
  const rec = 'path=a\0b\n';
  const len = rec.length + 3;
  await rejects(tar([{ name: 'p', type: 'x', data: `${len} ${rec}` }, { name: 'x', data: 'x' }]), 'E_ARCHIVE_UNSAFE', /NUL/);
});

test('unsafe zip paths are rejected', async () => {
  for (const name of ['../evil', '/abs', 'a\\b', 'x/../../y', 'a\0b']) {
    await rejects(zip([{ name, data: 'x' }]), 'E_ARCHIVE_UNSAFE', /unsafe archive entry/);
  }
});

test('symlink, hard link and device entries are rejected', async () => {
  await rejects(tar([{ name: 'l', type: '2', linkname: '/etc/passwd' }]), 'E_ARCHIVE_UNSAFE', /symlink/);
  await rejects(tar([{ name: 'h', type: '1', linkname: 'x' }]), 'E_ARCHIVE_UNSAFE', /hard link/);
  await rejects(tar([{ name: 'd', type: '3' }]), 'E_ARCHIVE_UNSAFE', /special file/);
  await rejects(zip([{ name: 'l', data: '/etc/passwd', mode: 0o120777 }]), 'E_ARCHIVE_UNSAFE', /symlink/);
});

test('too many entries', async () => {
  const entries = Array.from({ length: 6 }, (_, i) => ({ name: `f${i}`, data: 'x' }));
  await rejects(tar(entries), 'E_ARCHIVE_LIMIT', /more than 5 entries/, { maxEntries: 5 });
  await rejects(gzipSync(tar(entries)), 'E_ARCHIVE_LIMIT', /more than 5 entries/, { maxEntries: 5 });
  await rejects(zip(entries), 'E_ARCHIVE_LIMIT', /more than 5 entries/, { maxEntries: 5 });
  assert.equal((await extractArchive(tar(entries), 't', { ...DEFAULT_ARCHIVE_LIMITS, maxEntries: 6 })).size, 6);
});

test('oversize total', async () => {
  const big = [{ name: 'a', data: 'x'.repeat(600) }, { name: 'b', data: 'y'.repeat(600) }];
  await rejects(tar(big), 'E_ARCHIVE_LIMIT', /expands beyond 1000 bytes/, { maxTotalBytes: 1000 });
  await rejects(zip(big), 'E_ARCHIVE_LIMIT', /expands beyond 1000 bytes/, { maxTotalBytes: 1000 });
  await rejects(tar([{ name: 'a', data: 'x'.repeat(5000) }]), 'E_ARCHIVE_LIMIT', /larger than 1000 bytes/, { maxTotalBytes: 1000 });
});

test('compression ratio bombs stop early', async () => {
  // 50 MB of zeros gzips to ~50 KB (ratio ~1000:1): rejected at the default 100:1 without inflating it all.
  const bomb = gzipSync(tar([{ name: 'zeros', data: Buffer.alloc(50 * 1024 * 1024) }]));
  await rejects(bomb, 'E_ARCHIVE_LIMIT', /compression ratio exceeds 100:1/);
  // A low total-size limit also stops it.
  await rejects(bomb, 'E_ARCHIVE_LIMIT', /expands beyond 1048576 bytes/, { maxTotalBytes: 1024 * 1024, maxRatio: 100_000 });
  // Zip: honest headers over the ratio, and headers that lie about the size.
  await rejects(zip([{ name: 'z', data: Buffer.alloc(5 * 1024 * 1024), deflate: true }]), 'E_ARCHIVE_LIMIT', /ratio/);
  await rejects(zip([{ name: 'z', data: Buffer.alloc(5 * 1024 * 1024), deflate: true, fakeSize: 10 }]), 'E_ARCHIVE_LIMIT', /expands/);
});

test('duplicate paths are E_CONFLICT', async () => {
  await rejects(tar([{ name: 'a', data: '1' }, { name: './a', data: '1' }]), 'E_CONFLICT', /duplicate/);
  await rejects(zip([{ name: 'a', data: '1' }, { name: 'a', data: '2' }]), 'E_CONFLICT', /duplicate/);
  await rejects(tar([{ name: 'a', data: '1' }, { name: 'a/b', data: '2' }]), 'E_CONFLICT', /./);
});

test('corrupt archives are E_ARCHIVE', async () => {
  const good = tar([{ name: 'a', data: 'x' }]);
  const badSum = Buffer.from(good);
  badSum[0] = 0x7a;
  await rejects(badSum, 'E_ARCHIVE', /checksum/);
  await rejects(Buffer.concat([tarHeader({ name: 'big' }, 4096), Buffer.alloc(100)]), 'E_ARCHIVE', /truncated/);
  await rejects(zip([{ name: 'a', data: 'x' }]).subarray(0, 30), 'E_ARCHIVE', /end-of-central-directory/);
});

// ---- review fixes

function withChecksum(h: Buffer): Buffer {
  h.write('        ', 148, 'latin1');
  let sum = 0;
  for (const b of h) sum += b;
  h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'latin1');
  return h;
}

test('the ustar name prefix is used for POSIX headers only, not old GNU ones', async () => {
  const posix = tarHeader({ name: 'file.txt' }, 1);
  posix.write('dir\0', 345, 'latin1');
  const gnu = tarHeader({ name: 'file.txt' }, 1);
  gnu.write('ustar  \0', 257, 'latin1'); // old GNU magic + version
  gnu.write('\x01\x02\x03atime', 345, 'latin1'); // atime/ctime live here in old GNU headers
  const one = (h: Buffer): Buffer => Buffer.concat([withChecksum(h), Buffer.from('x'), Buffer.alloc(511), Buffer.alloc(1024)]);
  assert.deepEqual(paths(await extractArchive(one(posix), 't')), ['/dir/file.txt']);
  assert.deepEqual(paths(await extractArchive(one(gnu), 't')), ['/file.txt']);
});

test('gzip size cap leaves room for tar headers and padding of every entry', async () => {
  // 2000 one-byte files: 2000 bytes of data but ~2 MB of tar framing.
  const entries = Array.from({ length: 2000 }, (_, i) => ({ name: `f${i}`, data: 'x' }));
  const t = await extractArchive(gzipSync(tar(entries)), 't', { ...DEFAULT_ARCHIVE_LIMITS, maxTotalBytes: 4000, maxEntries: 2000, maxRatio: 1_000_000 });
  assert.equal(t.size, 2000);
});

test('zip directories marked only by mode are directories', async () => {
  const z = zip([{ name: 'assets', mode: 0o040755 }, { name: 'assets/app.css', data: 'a{}' }]);
  assert.deepEqual(paths(await extractArchive(z, 't')), ['/assets/app.css']);
});

test('malformed pax records are E_ARCHIVE, not a size-limit error', async () => {
  const pax = (body: string): Buffer => {
    let len = body.length + 1;
    while (String(len).length + body.length !== len) len = String(len).length + body.length;
    return tar([{ name: 'PaxHeader', type: 'x', data: `${len}${body}` }, { name: 'a', data: 'x' }]);
  };
  await rejects(pax(' size=abc\n'), 'E_ARCHIVE', /malformed pax size 'abc'/);
  await rejects(pax(' novalue\n'), 'E_ARCHIVE', /malformed pax header record/);
});

test('a pax record without a space after the last complete one ends the header (it used to loop forever)', async () => {
  const t = await extractArchive(tar([{ name: 'PaxHeader', type: 'x', data: '10 path=p\n' + 'tail' }, { name: 'a', data: 'x' }]), 't');
  assert.deepEqual(paths(t), ['/p']);
  const u = await extractArchive(tar([{ name: 'PaxHeader', type: 'x', data: 'nospace' }, { name: 'a', data: 'x' }]), 't');
  assert.deepEqual(paths(u), ['/a']);
});

test('tar numeric fields are read like parseInt: white space and signs', async () => {
  const entry = (size: string, mode: string): Buffer => {
    const h = tarHeader({ name: 'f' }, 3);
    h.fill(0, 124, 136).write(size, 124, 'latin1');
    h.fill(0, 100, 108).write(mode, 100, 'latin1');
    h.fill(0x20, 148, 156);
    let sum = 0;
    for (const x of h) sum += x;
    h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'latin1');
    return Buffer.concat([h, Buffer.from('abc'), Buffer.alloc(509 + 1024)]);
  };
  const t = await extractArchive(entry(' \u00a0+3', '\v-5'), 't');
  assert.equal(text(t.get('/f')?.bytes), 'abc');
  assert.equal(t.get('/f')?.mode, 0o755); // -5 & 0o111 is non-zero
  await rejects(entry('-3', '644'), 'E_ARCHIVE_LIMIT', /archive entry larger than/);
});

test('trailing zero bytes after a gzip member are ignored; other trailing bytes must be a member', async () => {
  const gz = gzipSync(tar([{ name: 'a', data: 'x' }]));
  assert.deepEqual(paths(await extractArchive(Buffer.concat([gz, Buffer.alloc(9)]), 't')), ['/a']);
  await rejects(Buffer.concat([gz, Buffer.from([1, 2, 3])]), 'E_ARCHIVE', /corrupt compressed data/);
});

test('zip names: UTF-8 with bit 11, the 0x7075 Unicode Path field, UTF-8 or else CP437 without it', async () => {
  const cp437 = Uint8Array.from([0x63, 0x61, 0x66, 0x82, 0x2e, 0x74, 0x78, 0x74]); // "caf\x82.txt": é in CP437
  const unicode = (raw: Uint8Array, name: string, crc = crc32(raw)): Uint8Array => {
    const u = Buffer.from(name);
    const f = Buffer.alloc(9);
    f.writeUInt16LE(0x7075, 0);
    f.writeUInt16LE(5 + u.length, 2);
    f[4] = 1;
    f.writeUInt32LE(crc, 5);
    return Buffer.concat([f, u]);
  };
  const one = async (e: Parameters<typeof zip>[0][0]): Promise<string[]> => paths(await extractArchive(zip([e]), 't'));
  assert.deepEqual(await one({ name: cp437, data: 'x' }), ['/caf\u00e9.txt']);
  assert.deepEqual(await one({ name: cp437, data: 'x', flags: 0x800 }), ['/caf\ufffd.txt']);
  assert.deepEqual(await one({ name: 'caf\u00e9.txt', data: 'x' }), ['/caf\u00e9.txt']);
  assert.deepEqual(await one({ name: cp437, data: 'x', extra: unicode(cp437, 'r\u00e9sum\u00e9.txt') }), ['/r\u00e9sum\u00e9.txt']);
  // A stale field (its CRC is not the name's) is ignored, as is one with bit 11 set.
  assert.deepEqual(await one({ name: cp437, data: 'x', extra: unicode(cp437, 'stale.txt', 1) }), ['/caf\u00e9.txt']);
  assert.deepEqual(await one({ name: 'a.txt', data: 'x', flags: 0x800, extra: unicode(Buffer.from('a.txt'), 'b.txt') }), ['/a.txt']);
  // Decoded names are checked like any other.
  await rejects(zip([{ name: cp437, data: 'x', extra: unicode(cp437, '../evil') }]), 'E_ARCHIVE_UNSAFE', /'\.\.' segment/);
  await rejects(zip([{ name: cp437, data: '1' }, { name: 'caf\u00e9.txt', data: '2' }]), 'E_CONFLICT', /duplicate/);
});

test('a flipped byte in a stored entry fails the CRC-32 check (#183)', async () => {
  const z = zip([{ name: 'index.html', data: '<h1>hello</h1>' }]);
  assert.equal(paths(await extractArchive(z, 't')).length, 1);
  z[44] = z[44]! ^ 0x20;
  await rejects(z, 'E_ARCHIVE', /'index\.html' fails its CRC-32 check/);
  const e = zip([{ name: 'empty', data: '' }]);
  const at = e.length - 22 - 51 + 16; // the central CRC
  e[at] = e[at]! ^ 1;
  await rejects(e, 'E_ARCHIVE', /'empty' fails its CRC-32 check/);
});

test('formats sitefile does not read are named in the E_TYPE error (#221)', async () => {
  const pad = (m: number[]): Uint8Array => Buffer.concat([Buffer.from(m), Buffer.alloc(600)]);
  await rejects(pad([0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]), 'E_TYPE', /not a recognized archive \(it is xz-compressed, e\.g\. \.tar\.xz; recompress it as \.tar\.gz or zip\)$/);
  await rejects(pad([0x28, 0xb5, 0x2f, 0xfd]), 'E_TYPE', /\(it is zstd-compressed, e\.g\. \.tar\.zst;/);
  await rejects(Buffer.from('BZh91AY&SY'), 'E_TYPE', /\(it is bzip2-compressed/);
  await rejects(pad([0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]), 'E_TYPE', /\(it is a 7z archive; convert it to zip or tar\)$/);
  await rejects(Buffer.from('Rar!\x1a\x07\x01\x00', 'latin1'), 'E_TYPE', /\(it is a RAR archive; convert it to zip or tar\)$/);
  // Text that merely starts like a magic number is just unrecognized.
  await rejects(Buffer.from('BZhello'), 'E_TYPE', /\(expected tar, tar\.gz, zip or web bundle\)$/);
});
