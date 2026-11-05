import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile, spawnSync } from 'node:child_process';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync } from 'node:zlib';
import { crc32, writeTar, writeTgz, writeZip } from '../src/archive-writers.js';
import { encodeTree } from '../src/export-formats.js';
import { exportFile } from '../src/exporter.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import type { ExportInstruction } from '../src/language/ast.js';
import { extractArchive } from '../src/resources/archive.js';
import { makeFile, Tree, type FileNode } from '../src/tree.js';
import { mimeTypeFor, validateBaseUrl, writeWebBundle } from '../src/wbn.js';

const enc = (s: string) => new TextEncoder().encode(s);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');

const temps: string[] = [];
after(async () => {
  for (const d of temps) await fs.rm(d, { recursive: true, force: true });
});
async function tmp(): Promise<string> {
  const d = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-fmt-')));
  temps.push(d);
  return d;
}

function mk(files: Record<string, string | [string, number]>): Tree {
  const list: Array<[string, FileNode]> = Object.entries(files).map(([p, v]) =>
    typeof v === 'string' ? [p, makeFile(enc(v), 'test')] : [p, makeFile(enc(v[0]), 'test', v[1])],
  );
  return Tree.fromFiles(list);
}

const sample = (): Tree =>
  mk({
    '/index.html': '<!doctype html><title>x</title><p>hello</p>'.repeat(20),
    '/css/site.css': 'body { color: red; }\n'.repeat(50),
    '/bin/run.sh': ['#!/bin/sh\necho hi\n', 0o755],
    '/img/blob.bin': String.fromCharCode(...Array.from({ length: 200 }, (_, i) => (i * 37) % 256)),
    '/empty.txt': '',
    '/ünï/cødé.txt': 'unicode name',
  });

const isCode = (code: ErrorCode) => (e: unknown) => isSitefileError(e) && e.code === code;

function sitefile(args: string[], cwd = root): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd, env: { ...process.env, SOURCE_DATE_EPOCH: '' } }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

// ------------------------------------------------------------ round trips

for (const [name, write] of [
  ['zip', writeZip],
  ['tar', writeTar],
  ['tgz', writeTgz],
] as const) {
  test(`${name}: round-trips through extractArchive with identical digest, modes and bytes`, async () => {
    const t = sample();
    const back = await extractArchive(write(t), 'test');
    assert.equal(back.digest(), t.digest());
    assert.equal(back.get('/bin/run.sh')?.mode, 0o755);
    assert.equal(back.get('/index.html')?.mode, 0o644);
  });

  test(`${name}: byte-identical across builds and independent of insertion order`, () => {
    const a = sample();
    const reversed = Tree.fromFiles([...sample().entries()].reverse());
    assert.deepEqual(Buffer.from(write(a)), Buffer.from(write(sample())));
    assert.deepEqual(Buffer.from(write(a)), Buffer.from(write(reversed)));
  });

  test(`${name}: empty tree is a valid archive`, async () => {
    const bytes = write(Tree.empty);
    assert.ok(bytes.length > 0);
    if (name !== 'zip') assert.equal((await extractArchive(bytes, 'test')).size, 0);
  });
}

test('SOURCE_DATE_EPOCH-style epoch changes timestamps but stays deterministic', () => {
  const t = sample();
  const a = writeZip(t, { epoch: 1_700_000_000 });
  assert.deepEqual(Buffer.from(a), Buffer.from(writeZip(t, { epoch: 1_700_000_000 })));
  assert.notDeepEqual(Buffer.from(a), Buffer.from(writeZip(t)));
  const tar = writeTar(t, { epoch: 1_700_000_000 });
  assert.equal(parseInt(Buffer.from(tar.subarray(136, 147)).toString('latin1'), 8), 1_700_000_000);
});

test('crc32 matches the known check value', () => {
  assert.equal(crc32(enc('123456789')), 0xcbf43926);
});

// ------------------------------------------------------------ zip details

test('zip: fixed DOS time, UTF-8 flag, unix modes, stored when deflate does not help', () => {
  const t = sample();
  const z = Buffer.from(writeZip(t));
  assert.equal(z.readUInt32LE(0), 0x04034b50);
  assert.equal(z.readUInt16LE(6) & 0x0800, 0x0800);
  assert.equal(z.readUInt16LE(10), 0); // 00:00:00
  assert.equal(z.readUInt16LE(12), (0 << 9) | (1 << 5) | 1); // 1980-01-01
  // Walk the central directory.
  const eocd = z.length - 22;
  const count = z.readUInt16LE(eocd + 10);
  assert.equal(count, t.size);
  let off = z.readUInt32LE(eocd + 16);
  const seen: Record<string, { method: number; attr: number; madeBy: number }> = {};
  for (let i = 0; i < count; i++) {
    const nlen = z.readUInt16LE(off + 28);
    const name = z.subarray(off + 46, off + 46 + nlen).toString('utf8');
    seen[name] = { method: z.readUInt16LE(off + 10), attr: z.readUInt32LE(off + 38) >>> 16, madeBy: z.readUInt16LE(off + 4) };
    off += 46 + nlen;
  }
  assert.deepEqual(Object.keys(seen), [...Object.keys(seen)].sort());
  assert.equal(seen['bin/run.sh']!.attr, 0o100755);
  assert.equal(seen['index.html']!.attr, 0o100644);
  assert.equal(seen['index.html']!.madeBy >> 8, 3);
  assert.equal(seen['index.html']!.method, 8); // compressible
  assert.equal(seen['empty.txt']!.method, 0); // deflate does not shrink it
  assert.ok('ünï/cødé.txt' in seen);
});

test('zip: more than 65535 entries fails with E_EXPORT', () => {
  const files: Array<[string, FileNode]> = [];
  for (let i = 0; i < 65536; i++) files.push([`/f${i}`, makeFile(new Uint8Array(0), 't')]);
  assert.throws(() => writeZip(Tree.fromFiles(files)), isCode('E_EXPORT'));
});

// ------------------------------------------------------------ tar details

function tarNames(tar: Uint8Array): Array<{ type: string; name: string; prefix: string; uid: string; uname: string; mtime: string }> {
  const out = [];
  const b = Buffer.from(tar);
  for (let off = 0; off + 512 <= b.length; ) {
    const h = b.subarray(off, off + 512);
    if (h.every((x) => x === 0)) break;
    const str = (a: number, z: number) => h.subarray(a, z).toString('utf8').replace(/\0.*$/s, '');
    const size = parseInt(str(124, 136), 8);
    out.push({ type: str(156, 157), name: str(0, 100), prefix: str(345, 500), uid: str(108, 116), uname: str(265, 297), mtime: str(136, 148) });
    off += 512 + Math.ceil(size / 512) * 512;
  }
  return out;
}

test('tar: ustar header fields are neutral and the checksum is valid', () => {
  const tar = Buffer.from(writeTar(sample()));
  assert.equal(tar.subarray(257, 263).toString('latin1'), 'ustar\0');
  const h = Buffer.from(tar.subarray(0, 512));
  const stored = parseInt(h.subarray(148, 154).toString('latin1'), 8);
  h.fill(0x20, 148, 156);
  assert.equal(
    h.reduce((n, x) => n + x, 0),
    stored,
  );
  const names = tarNames(tar);
  assert.deepEqual(
    names.map((n) => n.name),
    names.map((n) => n.name).slice().sort(),
  );
  for (const n of names) {
    assert.equal(parseInt(n.uid, 8), 0);
    assert.equal(n.uname, '');
    assert.equal(parseInt(n.mtime, 8), 0);
  }
  assert.equal(tar.length % 512, 0);
});

test('tar: names over 100 bytes use the prefix field; over 255 use a pax path header', async () => {
  const seg = 'a'.repeat(50);
  const medium = `/${seg}/${seg}/${seg}/file.txt`; // 156 bytes: prefix split
  const long = `/${[seg, seg, seg, seg, seg, seg].join('/')}/deep.txt`; // > 255: pax
  const oneBig = `/${'x'.repeat(130)}.txt`; // no slash to split on: pax
  const t = mk({ [medium]: 'm', [long]: 'l', [oneBig]: 'o', '/short.txt': 's' });
  const tar = writeTar(t);
  const entries = tarNames(tar);
  assert.ok(entries.some((e) => e.prefix !== '' && e.type === '0'), 'prefix split used');
  assert.ok(entries.filter((e) => e.type === 'x').length >= 2, 'pax headers used');
  for (const w of [tar, writeTgz(t)]) {
    const back = await extractArchive(w, 'test');
    assert.equal(back.digest(), t.digest());
    assert.ok(back.isFile(long));
    assert.ok(back.isFile(oneBig));
    assert.ok(back.isFile(medium));
  }
});

test('tar: non-ASCII long names survive pax', async () => {
  const p = `/${'é'.repeat(120)}.txt`; // 240 bytes, one segment
  const t = mk({ [p]: 'x' });
  assert.equal((await extractArchive(writeTar(t), 'test')).digest(), t.digest());
});

test('tgz: gzip header has mtime 0 and decompresses to the tar', () => {
  const t = sample();
  const gz = Buffer.from(writeTgz(t));
  assert.equal(gz.readUInt32LE(4), 0);
  assert.equal(gz[9], 3);
  assert.deepEqual(gunzipSync(gz), Buffer.from(writeTar(t)));
});

// ------------------------------------------------------- system tools

function hasTool(cmd: string, arg: string): boolean {
  const r = spawnSync(cmd, [arg], { stdio: 'ignore' });
  return !r.error;
}

test('unzip accepts the zip', { skip: hasTool('unzip', '-v') ? false : 'unzip not installed' }, async () => {
  const dir = await tmp();
  const f = path.join(dir, 'a.zip');
  await fs.writeFile(f, writeZip(sample()));
  const l = spawnSync('unzip', ['-l', f], { encoding: 'utf8' });
  assert.equal(l.status, 0, l.stderr);
  assert.match(l.stdout, /index\.html/);
  const t = spawnSync('unzip', ['-t', f], { encoding: 'utf8' });
  assert.equal(t.status, 0, t.stdout + t.stderr);
  assert.match(t.stdout, /No errors detected/);
});

test('tar accepts the tar.gz and tar', { skip: hasTool('tar', '--version') ? false : 'tar not installed' }, async () => {
  const dir = await tmp();
  const seg = 'a'.repeat(50);
  const t = mk({ '/index.html': 'hi', '/bin/x.sh': ['#!/bin/sh\n', 0o755], [`/${[seg, seg, seg, seg, seg, seg].join('/')}/deep.txt`]: 'd' });
  const gz = path.join(dir, 'a.tar.gz');
  const plain = path.join(dir, 'a.tar');
  await fs.writeFile(gz, writeTgz(t));
  await fs.writeFile(plain, writeTar(t));
  for (const [flag, file] of [['-tzf', gz], ['-tf', plain]] as const) {
    const r = spawnSync('tar', [flag, file], { encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /index\.html/);
    assert.match(r.stdout, /deep\.txt/);
  }
  const x = path.join(dir, 'x');
  await fs.mkdir(x);
  assert.equal(spawnSync('tar', ['-xzf', gz, '-C', x]).status, 0);
  assert.equal((await fs.stat(path.join(x, 'bin/x.sh'))).mode & 0o777 & ~0o022, 0o755 & ~0o022);
});

// ------------------------------------------------------------- web bundle

// Test-only CBOR decoder (unsigned ints, bytes, text, arrays, maps).
type Cbor = number | bigint | Buffer | string | Cbor[] | Map<string, Cbor>;
function cborItem(b: Buffer, off: number): { v: Cbor; end: number } {
  const ib = b[off]!;
  const major = ib >> 5;
  const info = ib & 31;
  let n: number;
  let p = off + 1;
  if (info < 24) n = info;
  else if (info === 24) (n = b[p]!), (p += 1);
  else if (info === 25) (n = b.readUInt16BE(p)), (p += 2);
  else if (info === 26) (n = b.readUInt32BE(p)), (p += 4);
  else if (info === 27) (n = Number(b.readBigUInt64BE(p))), (p += 8);
  else throw new Error(`unsupported CBOR info ${info}`);
  switch (major) {
    case 0:
      return { v: n, end: p };
    case 2:
      return { v: b.subarray(p, p + n), end: p + n };
    case 3:
      return { v: b.subarray(p, p + n).toString('utf8'), end: p + n };
    case 4: {
      const items: Cbor[] = [];
      for (let i = 0; i < n; i++) {
        const r = cborItem(b, p);
        items.push(r.v);
        p = r.end;
      }
      return { v: items, end: p };
    }
    case 5: {
      const m = new Map<string, Cbor>();
      for (let i = 0; i < n; i++) {
        const k = cborItem(b, p);
        const v = cborItem(b, k.end);
        m.set(Buffer.isBuffer(k.v) ? k.v.toString('latin1') : String(k.v), v.v);
        p = v.end;
      }
      return { v: m, end: p };
    }
    default:
      throw new Error(`unsupported CBOR major ${major}`);
  }
}

/** Test-only decoder for the b2 layout: [magic, version, section-lengths, sections, length]. */
function decodeBundle(bytes: Uint8Array) {
  const b = Buffer.from(bytes);
  assert.equal(b[0], 0x85, 'b2: top-level array of 5');
  const items: Array<{ v: Cbor; start: number; end: number }> = [];
  let p = 1;
  // magic, version, section-lengths
  for (let i = 0; i < 3; i++) {
    const r = cborItem(b, p);
    items.push({ v: r.v, start: p, end: r.end });
    p = r.end;
  }
  const sectionLengths = cborItem(items[2]!.v as Buffer, 0).v as Cbor[];
  const names = sectionLengths.filter((_, i) => i % 2 === 0) as string[];
  assert.equal(b[p], 0x80 + names.length, `sections: array of ${names.length}`);
  p += 1;
  const sections = new Map<string, { v: Cbor; start: number; end: number }>();
  for (const name of names) {
    const r = cborItem(b, p);
    sections.set(name, { v: r.v, start: p, end: r.end });
    p = r.end;
  }
  assert.equal(names[names.length - 1], 'responses', 'responses section is last');
  const len = cborItem(b, p);
  assert.equal(len.end, b.length, 'nothing after the length field');
  const idx = sections.get('index')!;
  const resp = sections.get('responses')!;
  return {
    magic: items[0]!.v as Buffer,
    version: items[1]!.v as Buffer,
    primary: (sections.get('primary')?.v as string | undefined) ?? '',
    sectionLengths,
    index: idx.v as Map<string, Cbor>,
    indexLen: idx.end - idx.start,
    respBytes: b.subarray(resp.start, resp.end),
    responses: resp.v as Cbor[],
    totalLength: (len.v as Buffer).readBigUInt64BE(0),
    size: b.length,
  };
}

test('wbn: structure, index, responses, primary URL and exact bytes', () => {
  const t = mk({
    '/index.html': '<h1>home</h1>',
    '/about/index.html': '<h1>about</h1>',
    '/css/site.css': 'p{}',
    '/img/a b.png': 'PNG',
    '/data.json': '{"a":1}',
    '/blob': 'raw',
  });
  const w = decodeBundle(writeWebBundle(t, 'https://example.com/site/'));
  assert.equal(w.magic.toString('hex'), 'f09f8c90f09f93a6');
  assert.equal(Buffer.from('🌐📦').toString('hex'), 'f09f8c90f09f93a6');
  assert.equal(w.version.toString('latin1'), 'b2\0\0');
  assert.equal(w.primary, 'https://example.com/site/');
  assert.equal(w.totalLength, BigInt(w.size));
  const primaryLen = 2 + 'https://example.com/site/'.length; // text(25): 2-byte CBOR head
  assert.deepEqual(w.sectionLengths, ['index', w.indexLen, 'primary', primaryLen, 'responses', w.respBytes.length]);

  const urls = [...w.index.keys()].sort();
  assert.deepEqual(urls, [
    'https://example.com/site/',
    'https://example.com/site/about/',
    'https://example.com/site/about/index.html',
    'https://example.com/site/blob',
    'https://example.com/site/css/site.css',
    'https://example.com/site/data.json',
    'https://example.com/site/img/a%20b.png',
    'https://example.com/site/index.html',
  ]);
  assert.equal(w.responses.length, t.size);

  const expectType: Record<string, string> = {
    'index.html': 'text/html; charset=utf-8',
    'site.css': 'text/css; charset=utf-8',
    'a%20b.png': 'image/png',
    'data.json': 'application/json; charset=utf-8',
    blob: 'application/octet-stream',
  };
  for (const [url, loc] of w.index) {
    // b2 index entries are [offset, length] (no variants value).
    assert.equal((loc as Cbor[]).length, 2);
    const [offset, length] = loc as [number, number];
    const r = cborItem(w.respBytes, offset);
    assert.equal(r.end - offset, length);
    const [headerBytes, payload] = r.v as [Buffer, Buffer];
    const headers = cborItem(headerBytes, 0).v as Map<string, Cbor>;
    assert.equal((headers.get(':status') as Buffer).toString(), '200');
    const rel = url.slice('https://example.com/site/'.length);
    const name = rel.slice(rel.lastIndexOf('/') + 1) || 'index.html';
    assert.equal((headers.get('content-type') as Buffer).toString(), expectType[name]);
    // payload equals the file the URL maps to
    const filePath = url.endsWith('/') ? `/${decodeURI(rel)}index.html` : `/${decodeURI(rel)}`;
    assert.deepEqual(payload, Buffer.from(t.get(filePath)!.bytes), url);
  }
  // Directory URL and /index.html share one response.
  assert.deepEqual(w.index.get('https://example.com/site/'), w.index.get('https://example.com/site/index.html'));
});

test('wbn: no /index.html gives an empty primary URL; output is deterministic', () => {
  const t = mk({ '/a.txt': 'a' });
  assert.equal(decodeBundle(writeWebBundle(t, 'https://example.com/')).primary, '');
  const s = sample();
  assert.deepEqual(Buffer.from(writeWebBundle(s, 'https://example.com/')), Buffer.from(writeWebBundle(Tree.fromFiles([...s.entries()].reverse()), 'https://example.com/')));
  const empty = decodeBundle(writeWebBundle(Tree.empty, 'https://example.com/'));
  assert.equal(empty.responses.length, 0);
});

test('wbn: many responses use a multi-byte array head and still have correct offsets', () => {
  const files: Array<[string, FileNode]> = [];
  for (let i = 0; i < 300; i++) files.push([`/f${String(i).padStart(3, '0')}.txt`, makeFile(enc(`file ${i}`), 't')]);
  const w = decodeBundle(writeWebBundle(Tree.fromFiles(files), 'https://example.com/'));
  assert.equal(w.index.size, 300);
  const [offset, length] = w.index.get('https://example.com/f299.txt') as [number, number];
  const r = cborItem(w.respBytes, offset);
  assert.equal(r.end - offset, length);
  assert.equal(((r.v as Cbor[])[1] as Buffer).toString(), 'file 299');
});

test('wbn: base URL validation', () => {
  for (const bad of [undefined, '', 'example.com/', 'http://example.com/', 'https://example.com', 'https://u:p@example.com/', 'https://example.com/?q=1', 'https://example.com/#x']) {
    assert.throws(() => writeWebBundle(Tree.empty, bad), isCode('E_VALIDATION'), String(bad));
  }
  assert.equal(validateBaseUrl('https://example.com/a/'), 'https://example.com/a/');
});

test('wbn: MIME table', () => {
  const cases: Record<string, string> = {
    '/a.html': 'text/html; charset=utf-8',
    '/a.CSS': 'text/css; charset=utf-8',
    '/a.js': 'text/javascript; charset=utf-8',
    '/a.mjs': 'text/javascript; charset=utf-8',
    '/a.svg': 'image/svg+xml; charset=utf-8',
    '/a.txt': 'text/plain; charset=utf-8',
    '/a.webmanifest': 'application/manifest+json; charset=utf-8',
    '/a.jpeg': 'image/jpeg',
    '/a.wasm': 'application/wasm',
    '/a.woff2': 'font/woff2',
    '/a.mp4': 'video/mp4',
    '/a.mp3': 'audio/mpeg',
    '/a.unknown': 'application/octet-stream',
    '/.hidden': 'application/octet-stream',
    '/dir.d/noext': 'application/octet-stream',
  };
  for (const [p, m] of Object.entries(cases)) assert.equal(mimeTypeFor(p), m, p);
});

// ---------------------------------------------------------------- parser

function exportOf(src: string): ExportInstruction {
  return parseProgram(src).instructions.find((i): i is ExportInstruction => i.kind === 'EXPORT')!;
}

test('parser: EXPORT AS formats, tar.gz alias and BASE', () => {
  for (const [as, fmt] of [['directory', 'directory'], ['zip', 'zip'], ['TAR', 'tar'], ['tgz', 'tgz'], ['tar.gz', 'tgz'], ['wbn', 'wbn']]) {
    assert.equal(exportOf(`FROM scratch\nEXPORT / AS ${as}\n`).format, fmt);
  }
  assert.equal(exportOf('FROM scratch\nEXPORT /\n').format, 'directory');
  const w = exportOf('FROM scratch\nEXPORT / AS wbn BASE https://example.com/\n');
  assert.equal(w.format, 'wbn');
  assert.ok(w.base);
  assert.throws(() => parseProgram('FROM scratch\nEXPORT / AS rar\n'), isCode('E_UNSUPPORTED'));
  assert.throws(() => parseProgram('FROM scratch\nEXPORT / AS wbn BASE\n'), isCode('E_SYNTAX'));
});

// ---------------------------------------------------------- file safety

test('exportFile: writes, replaces inside the context, mode 0644, no temp leftovers', async () => {
  const ctx = await tmp();
  const out = path.join(ctx, 'dist.zip');
  const r = await exportFile(enc('one'), out, { contextDir: ctx });
  assert.deepEqual(r, { outFile: out, bytes: 3 });
  await exportFile(enc('two'), out, { contextDir: ctx }); // inside the context: replaced without --force
  assert.equal(await fs.readFile(out, 'utf8'), 'two');
  assert.equal((await fs.stat(out)).mode & 0o777, 0o644);
  assert.deepEqual(await fs.readdir(ctx), ['dist.zip']);
});

test('exportFile: existing file outside the context needs --force', async () => {
  const ctx = await tmp();
  const other = await tmp();
  const out = path.join(other, 'a.zip');
  await fs.writeFile(out, 'precious');
  await assert.rejects(exportFile(enc('new'), out, { contextDir: ctx }), isCode('E_EXPORT_UNSAFE'));
  assert.equal(await fs.readFile(out, 'utf8'), 'precious');
  await exportFile(enc('new'), out, { contextDir: ctx, force: true });
  assert.equal(await fs.readFile(out, 'utf8'), 'new');
  // A new file outside the context needs no --force.
  await exportFile(enc('x'), path.join(other, 'fresh', 'b.zip'), { contextDir: ctx });
});

test('exportFile: refuses a directory, a symlink, the context, a parent of the context, root and home', async () => {
  const ctx = await tmp();
  const other = await tmp();
  await fs.mkdir(path.join(other, 'adir'));
  await assert.rejects(exportFile(enc('x'), path.join(other, 'adir'), { contextDir: ctx, force: true }), isCode('E_EXPORT'));
  await fs.writeFile(path.join(other, 'real'), 'r');
  await fs.symlink(path.join(other, 'real'), path.join(other, 'link'));
  await assert.rejects(exportFile(enc('x'), path.join(other, 'link'), { contextDir: ctx, force: true }), isCode('E_EXPORT'));
  assert.equal(await fs.readFile(path.join(other, 'real'), 'utf8'), 'r');
  await assert.rejects(exportFile(enc('x'), ctx, { contextDir: ctx }), isCode('E_EXPORT_UNSAFE'));
  await assert.rejects(exportFile(enc('x'), path.dirname(ctx), { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
  await assert.rejects(exportFile(enc('x'), path.parse(ctx).root, { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
  await assert.rejects(exportFile(enc('x'), os.homedir(), { contextDir: ctx, force: true }), isCode('E_EXPORT_UNSAFE'));
  assert.deepEqual((await fs.readdir(other)).sort(), ['adir', 'link', 'real']);
});

test('exportFile: destination changed during the write aborts and cleans up only its own temp file', async () => {
  const ctx = await tmp();
  const out = path.join(ctx, 'a.zip');
  await fs.writeFile(out, 'old');
  await assert.rejects(
    exportFile(enc('new'), out, {
      contextDir: ctx,
      beforeSwap: async () => {
        await fs.rm(out);
        await fs.writeFile(out, 'attacker'); // different inode
      },
    }),
    isCode('E_EXPORT_UNSAFE'),
  );
  assert.equal(await fs.readFile(out, 'utf8'), 'attacker');
  assert.deepEqual(await fs.readdir(ctx), ['a.zip']);

  const fresh = path.join(ctx, 'b.zip');
  await assert.rejects(
    exportFile(enc('new'), fresh, { contextDir: ctx, beforeSwap: async () => fs.writeFile(fresh, 'appeared') }),
    isCode('E_EXPORT_UNSAFE'),
  );
  assert.equal(await fs.readFile(fresh, 'utf8'), 'appeared');
});

// ------------------------------------------------------------------- CLI

async function project(sitefileExport: string): Promise<string> {
  const ctx = await tmp();
  await fs.mkdir(path.join(ctx, 'public'));
  await fs.writeFile(path.join(ctx, 'public', 'index.html'), '<h1>hi</h1>');
  await fs.writeFile(path.join(ctx, 'public', 'app.css'), 'p { color: red }\n'.repeat(10));
  await fs.writeFile(path.join(ctx, 'Sitefile'), `FROM scratch\nCOPY ./public/ /\n${sitefileExport}\n`);
  return ctx;
}

async function expectTree(file: string, ctx: string): Promise<void> {
  const back = await extractArchive(await fs.readFile(file), 'cli');
  assert.equal(back.size, 2);
  assert.equal(back.get('/index.html')?.digest, makeFile(await fs.readFile(path.join(ctx, 'public/index.html')), 'x').digest);
}

test('CLI: --format zip writes <context>/dist.zip and prints a summary', async () => {
  const ctx = await project('EXPORT / AS directory');
  const r = await sitefile(['build', ctx, '--format', 'zip']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /Exported 2 files to .*dist\.zip \(zip, \d+ bytes, sha256:[0-9a-f]{64}\)/);
  await expectTree(path.join(ctx, 'dist.zip'), ctx);
  await assert.rejects(fs.stat(path.join(ctx, 'dist')));
  // Rebuild with the archive already inside the context: it must not end up inside itself.
  const again = await sitefile(['build', ctx, '--format', 'zip']);
  assert.equal(again.code, 0, again.stderr);
  assert.match(again.stdout, /Exported 2 files/);
  assert.deepEqual((await fs.readdir(ctx)).sort(), ['Sitefile', 'dist.zip', 'public']);
});

test('CLI: byte-identical archives from two builds', async () => {
  const ctx = await project('EXPORT / AS directory');
  for (const [fmt, ext] of [['zip', 'zip'], ['tar', 'tar'], ['tgz', 'tar.gz'], ['tar.gz', 'tar.gz']] as const) {
    const a = path.join(ctx, `a.${ext}`);
    const b = path.join(await tmp(), `b.${ext}`);
    assert.equal((await sitefile(['build', ctx, '--format', fmt, '-o', a])).code, 0);
    assert.equal((await sitefile(['build', ctx, '--format', fmt, '-o', b])).code, 0);
    assert.deepEqual(await fs.readFile(a), await fs.readFile(b), fmt);
  }
});

test('CLI: -o x.tar.gz with --format tar.gz; defaults dist.tar.gz and dist.tar', async () => {
  const ctx = await project('EXPORT / AS directory');
  const x = path.join(ctx, 'x.tar.gz');
  const r = await sitefile(['build', ctx, '--format', 'tar.gz', '-o', x]);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /x\.tar\.gz \(tar\.gz, \d+ bytes, sha256:/);
  await expectTree(x, ctx);
  assert.equal((await sitefile(['build', ctx, '--format', 'tgz'])).code, 0);
  await expectTree(path.join(ctx, 'dist.tar.gz'), ctx);
  assert.equal((await sitefile(['build', ctx, '--format', 'tar'])).code, 0);
  await expectTree(path.join(ctx, 'dist.tar'), ctx);
});

test('CLI: the --out extension picks the format when --format is not given', async () => {
  const ctx = await project('EXPORT / AS directory');
  for (const [name, label] of [['site.zip', 'zip'], ['site.tar', 'tar'], ['site.tgz', 'tar.gz'], ['site.tar.gz', 'tar.gz']] as const) {
    const out = path.join(ctx, name);
    const r = await sitefile(['build', ctx, '-o', out]);
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stdout, new RegExp(`\\(${label.replace('.', '\\.')}, \\d+ bytes`));
    await expectTree(out, ctx);
  }
  // An explicit --format wins over the extension.
  const d = path.join(ctx, 'odd.zip');
  assert.equal((await sitefile(['build', ctx, '-o', d, '--format', 'directory'])).code, 0);
  assert.ok((await fs.stat(d)).isDirectory());
});

test('CLI: EXPORT AS zip in the Sitefile; --format overrides it', async () => {
  const ctx = await project('EXPORT / AS zip');
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  await expectTree(path.join(ctx, 'dist.zip'), ctx);
  const o = await sitefile(['build', ctx, '--format', 'directory']);
  assert.equal(o.code, 0, o.stderr);
  assert.equal(await fs.readFile(path.join(ctx, 'dist', 'index.html'), 'utf8'), '<h1>hi</h1>');
  const bad = await sitefile(['build', ctx, '--format', 'rar']);
  assert.equal(bad.code, 2);
  assert.match(bad.stderr, /--format expects/);
});

test('CLI: wbn with BASE in the Sitefile', async () => {
  const ctx = await project('EXPORT / AS wbn BASE https://example.com/app/');
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /dist\.wbn \(wbn, \d+ bytes, sha256:/);
  const w = decodeBundle(await fs.readFile(path.join(ctx, 'dist.wbn')));
  assert.equal(w.primary, 'https://example.com/app/');
  assert.ok(w.index.has('https://example.com/app/app.css'));
});

test('CLI: wbn with --base-url (overrides BASE) and a variable in BASE', async () => {
  const ctx = await project('EXPORT / AS wbn');
  const r = await sitefile(['build', ctx, '--format', 'wbn', '--base-url', 'https://cli.example/']);
  assert.equal(r.code, 0, r.stderr);
  assert.equal(decodeBundle(await fs.readFile(path.join(ctx, 'dist.wbn'))).primary, 'https://cli.example/');

  const v = await project('EXPORT / AS wbn BASE "https://${HOST}/"');
  await fs.writeFile(path.join(v, 'Sitefile'), 'ARG HOST = "var.example"\nFROM scratch\nCOPY ./public/ /\nEXPORT / AS wbn BASE "https://${HOST}/"\n');
  assert.equal((await sitefile(['build', v])).code, 0);
  assert.equal(decodeBundle(await fs.readFile(path.join(v, 'dist.wbn'))).primary, 'https://var.example/');

  const o = await project('EXPORT / AS wbn BASE https://sitefile.example/');
  assert.equal((await sitefile(['build', o, '--base-url', 'https://cli.example/'])).code, 0);
  assert.equal(decodeBundle(await fs.readFile(path.join(o, 'dist.wbn'))).primary, 'https://cli.example/');
});

test('CLI: wbn without a base URL, or with a bad one, fails and writes nothing', async () => {
  const ctx = await project('EXPORT / AS wbn');
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_VALIDATION.*base URL/);
  const bad = await sitefile(['build', ctx, '--base-url', 'http://example.com/']);
  assert.equal(bad.code, 1);
  assert.match(bad.stderr, /must be https/);
  assert.deepEqual((await fs.readdir(ctx)).sort(), ['Sitefile', 'public']);
});

test('CLI: file output safety (existing file outside context, directory, context itself)', async () => {
  const ctx = await project('EXPORT / AS zip');
  const other = await tmp();
  const out = path.join(other, 'site.zip');
  await fs.writeFile(out, 'keep me');
  const r = await sitefile(['build', ctx, '-o', out]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_EXPORT_UNSAFE.*--force/);
  assert.equal(await fs.readFile(out, 'utf8'), 'keep me');
  assert.equal((await sitefile(['build', ctx, '-o', out, '--force'])).code, 0);
  await expectTree(out, ctx);

  await fs.mkdir(path.join(other, 'd'));
  const d = await sitefile(['build', ctx, '-o', path.join(other, 'd')]);
  assert.equal(d.code, 1);
  assert.match(d.stderr, /E_EXPORT.*is a directory/);

  const c = await sitefile(['build', ctx, '-o', ctx]);
  assert.equal(c.code, 1);
  assert.match(c.stderr, /E_EXPORT_UNSAFE/);
  assert.deepEqual(await fs.readdir(path.join(other, 'd')), []);
});

test('encodeTree dispatches every archive format', () => {
  const t = sample();
  for (const f of ['zip', 'tar', 'tgz'] as const) assert.ok(encodeTree(t, f).length > 0);
  assert.ok(encodeTree(t, 'wbn', { baseUrl: 'https://example.com/' }).length > 0);
  assert.throws(() => encodeTree(t, 'wbn'), isCode('E_VALIDATION'));
});

test('wbn: the reference dump-bundle tool reads the bundle (skipped if not installed)', async (t) => {
  const gopath = spawnSync('go', ['env', 'GOPATH'], { encoding: 'utf8' });
  const tool = gopath.status === 0 ? path.join(gopath.stdout.trim(), 'bin', 'dump-bundle') : '';
  if (!tool || !(await fs.stat(tool).then(() => true, () => false))) {
    t.skip('dump-bundle not installed (go install github.com/WICG/webpackage/go/bundle/cmd/dump-bundle@latest)');
    return;
  }
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-wbn-'));
  try {
    const file = path.join(dir, 'b.wbn');
    await fs.writeFile(file, writeWebBundle(mk({ '/index.html': '<h1>hi</h1>', '/a.css': 'a{}' }), 'https://example.com/'));
    const r = spawnSync(tool, ['-i', file], { encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /Version: b2/);
    assert.match(r.stdout, /Primary URL: https:\/\/example\.com\//);
    for (const url of ['https://example.com/', 'https://example.com/index.html', 'https://example.com/a.css']) {
      assert.ok(r.stdout.includes(`> :url: ${url}\n`), url);
    }
    assert.match(r.stdout, /Content-Type: \[text\/css; charset=utf-8\]/);
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
});
