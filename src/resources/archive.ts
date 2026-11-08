import { crc32, gunzip as gunzipCb, inflateRaw as inflateRawCb, type ZlibOptions } from 'node:zlib';
import { promisify } from 'node:util';
import { SitefileError, type SourceSpan } from '../errors.js';
import { normalizeVirtual } from '../paths.js';
import { makeFile, Tree, type FileNode } from '../tree.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits } from './types.js';
import { isWebBundle, readWebBundle, webBundleToTree } from './wbn-reader.js';

/** Below this many output bytes the ratio limit is not enforced (tar block padding compresses extremely well). */
const RATIO_FLOOR = 1024 * 1024;

const gunzipAsync = promisify<Uint8Array, ZlibOptions, Buffer>(gunzipCb);
const inflateRawAsync = promisify<Uint8Array, ZlibOptions, Buffer>(inflateRawCb);

/** Tar framing per entry, worst case: header + padding + a pax/GNU long-name header and its padding. */
const TAR_OVERHEAD_PER_ENTRY = 4 * 512;

type Add = (name: string, kind: 'file' | 'dir', bytes: Uint8Array, mode: number) => void;

/**
 * Extract a tar, tar.gz/tgz, zip or Web Bundle (b1/b2) archive into a Tree. The format is detected
 * from magic bytes, never from the name. Nothing is inflated beyond the limits,
 * and unsafe entries (absolute or `..` paths, backslashes, NUL, symlinks, hard
 * links, devices) are rejected, not skipped. A single top-level directory is
 * kept as-is.
 */
export async function extractArchive(
  bytes: Uint8Array,
  origin: string,
  limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS,
  span?: SourceSpan,
): Promise<Tree> {
  if (isWebBundle(bytes)) return webBundleToTree(readWebBundle(bytes, origin, limits, span), origin, span);
  const files = new Map<string, FileNode>();
  const dirs = new Set<string>();
  let total = 0;
  const add: Add = (name, kind, data, mode) => {
    const path = archivePath(name, span);
    if (path === '/') return;
    if (files.has(path) || (kind === 'file' && dirs.has(path)) || (kind === 'dir' && files.has(path))) {
      throw new SitefileError('E_CONFLICT', `${origin}: duplicate archive entry '${name}'`, span);
    }
    if (kind === 'dir') {
      dirs.add(path);
      return;
    }
    if (files.size + 1 > limits.maxEntries) {
      throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive has more than ${limits.maxEntries} entries`, span);
    }
    total += data.byteLength;
    if (total > limits.maxTotalBytes) {
      throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive expands beyond ${limits.maxTotalBytes} bytes`, span);
    }
    files.set(path, makeFile(data, `${origin}:${name}`, mode));
  };

  if (isGzip(bytes)) {
    const tar = await gunzip(bytes, origin, limits, span);
    if (!isTar(tar)) throw new SitefileError('E_TYPE', `${origin}: gzip content is not a tar archive`, span);
    readTar(tar, origin, limits, span, add);
  } else if (isZip(bytes)) {
    await readZip(bytes, origin, limits, span, add);
  } else if (isTar(bytes)) {
    readTar(bytes, origin, limits, span, add);
  } else {
    throw new SitefileError('E_TYPE', `${origin}: not a recognized archive (expected tar, tar.gz, zip or web bundle)`, span);
  }
  return Tree.fromFiles(files);
}

const isGzip = (b: Uint8Array): boolean => b.length > 2 && b[0] === 0x1f && b[1] === 0x8b;
const isZip = (b: Uint8Array): boolean =>
  b.length >= 4 && b[0] === 0x50 && b[1] === 0x4b && ((b[2] === 3 && b[3] === 4) || (b[2] === 5 && b[3] === 6));
const isTar = (b: Uint8Array): boolean =>
  b.length >= 512 &&
  (ascii(b, 257, 262) === 'ustar' ||
    (tarChecksumOk(b, 0) && b.subarray(0, 512).some((x) => x !== 0)) ||
    (b.length === 1024 && b.every((x) => x === 0))); // an empty tar

function ascii(b: Uint8Array, from: number, to: number): string {
  return Buffer.from(b.subarray(from, to)).toString('latin1');
}

/** Validate an entry name and return its normalized virtual path. */
function archivePath(name: string, span?: SourceSpan): string {
  const bad = (why: string): SitefileError =>
    new SitefileError('E_ARCHIVE_UNSAFE', `unsafe archive entry '${name.replace(/\0/g, '\\0')}': ${why}`, span);
  if (name.includes('\0')) throw bad('NUL byte');
  if (name.includes('\\')) throw bad('backslash in path');
  if (name.startsWith('/') || /^[A-Za-z]:/.test(name)) throw bad('absolute path');
  if (name.split('/').includes('..')) throw bad("'..' segment");
  return normalizeVirtual('/' + name, span, 'archive entry').path;
}

// ------------------------------------------------------------------ gzip

async function gunzip(bytes: Uint8Array, origin: string, limits: ArchiveLimits, span?: SourceSpan): Promise<Uint8Array> {
  const byRatio = Math.max(bytes.length * limits.maxRatio, RATIO_FLOOR);
  // The inflated stream is tar: file data plus headers and padding for every entry.
  const byTotal = limits.maxTotalBytes + (limits.maxEntries + 2) * TAR_OVERHEAD_PER_ENTRY;
  try {
    // Async so a large archive doesn't stall other imports' sockets and timers.
    return await gunzipAsync(bytes, { maxOutputLength: Math.min(byRatio, byTotal) });
  } catch (e) {
    throw inflateError(e, origin, byRatio <= byTotal, limits, span);
  }
}

function inflateError(e: unknown, origin: string, ratio: boolean, limits: ArchiveLimits, span?: SourceSpan): SitefileError {
  if ((e as { code?: string }).code === 'ERR_BUFFER_TOO_LARGE') {
    return ratio
      ? new SitefileError('E_ARCHIVE_LIMIT', `${origin}: compression ratio exceeds ${limits.maxRatio}:1`, span)
      : new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive expands beyond ${limits.maxTotalBytes} bytes`, span);
  }
  return new SitefileError('E_ARCHIVE', `${origin}: corrupt compressed data`, span, { cause: e });
}

// ------------------------------------------------------------------- tar

function tarChecksumOk(b: Uint8Array, off: number): boolean {
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 32 : b[off + i]!;
  const stored = parseInt(ascii(b, off + 148, off + 156).replace(/[\0 ]/g, ''), 8);
  return stored === sum;
}

function tarNumber(b: Uint8Array, from: number, to: number): number {
  if (b[from]! & 0x80) {
    let n = b[from]! & 0x7f; // GNU base-256
    for (let i = from + 1; i < to; i++) n = n * 256 + b[i]!;
    return n;
  }
  const s = ascii(b, from, to).replace(/[\0 ]/g, '');
  return s === '' ? 0 : parseInt(s, 8);
}

function cString(b: Uint8Array, from: number, to: number): string {
  let end = from;
  while (end < to && b[end] !== 0) end++;
  return Buffer.from(b.subarray(from, end)).toString('utf8');
}

/** Supports ustar prefixes, GNU `L` long names and pax `path`/`size` records. */
function readTar(b: Uint8Array, origin: string, limits: ArchiveLimits, span: SourceSpan | undefined, add: Add): void {
  const bad = (why: string): SitefileError => new SitefileError('E_ARCHIVE', `${origin}: ${why}`, span);
  let off = 0;
  let nextName: string | undefined;
  let nextSize: number | undefined;
  let entries = 0;
  while (off + 512 <= b.length) {
    const h = off;
    if (b.subarray(h, h + 512).every((x) => x === 0)) return; // end-of-archive marker
    if (!tarChecksumOk(b, h)) throw bad('invalid tar header checksum');
    const type = String.fromCharCode(b[h + 156]!);
    const size = nextSize ?? tarNumber(b, h + 124, h + 136);
    if (!Number.isSafeInteger(size) || size < 0 || size > limits.maxTotalBytes) {
      throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive entry larger than ${limits.maxTotalBytes} bytes`, span);
    }
    if (h + 512 + size > b.length) throw bad('truncated tar archive');
    const data = b.subarray(h + 512, h + 512 + size);
    off = h + 512 + Math.ceil(size / 512) * 512;

    if (type === 'L') { nextName = cString(data, 0, data.length); continue; }
    if (type === 'x') { ({ path: nextName, size: nextSize } = parsePax(data, bad)); continue; }
    if (type === 'g' || type === 'K') continue; // global pax header / GNU long link name: unused
    if (++entries > limits.maxEntries) {
      throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive has more than ${limits.maxEntries} entries`, span);
    }
    let name = nextName;
    if (name === undefined) {
      name = cString(b, h, h + 100);
      // Only POSIX ustar ('ustar\0') has a name prefix; old GNU ('ustar ') keeps atime/ctime there.
      const prefix = ascii(b, h + 257, h + 263) === 'ustar\0' ? cString(b, h + 345, h + 500) : '';
      if (prefix) name = `${prefix}/${name}`;
    }
    nextName = nextSize = undefined;
    if (type === '0' || type === '\0' || type === '7') add(name, 'file', data, tarNumber(b, h + 100, h + 108));
    else if (type === '5') add(name, 'dir', data, 0o755);
    else {
      const what = type === '1' ? 'hard link' : type === '2' ? 'symlink' : 'special file';
      throw new SitefileError('E_ARCHIVE_UNSAFE', `unsafe archive entry '${name}': ${what} entries are not allowed`, span);
    }
  }
}

function parsePax(data: Uint8Array, bad: (why: string) => SitefileError): { path: string | undefined; size: number | undefined } {
  const out: { path: string | undefined; size: number | undefined } = { path: undefined, size: undefined };
  const buf = Buffer.from(data);
  let pos = 0;
  while (pos < buf.length) {
    const sp = buf.indexOf(0x20, pos);
    // No length prefix left: stop (without this a record missing its space looped forever).
    if (sp < 0) break;
    const len = parseInt(buf.toString('latin1', pos, sp), 10);
    if (!(len > sp - pos) || pos + len > buf.length) break;
    const kv = buf.toString('utf8', sp + 1, pos + len - 1); // drop trailing newline
    const eq = kv.indexOf('=');
    if (eq <= 0) throw bad('malformed pax header record');
    const key = kv.slice(0, eq);
    const value = kv.slice(eq + 1);
    if (key === 'path') out.path = value;
    if (key === 'size') {
      if (!/^\d+$/.test(value)) throw bad(`malformed pax size '${value}'`);
      out.size = Number(value);
    }
    pos += len;
  }
  return out;
}

// ------------------------------------------------------------------- zip

interface ZipEntry {
  name: string;
  rawName: Uint8Array;
  method: number;
  crc: number;
  csize: number;
  usize: number;
  local: number;
  mode: number;
  isDir: boolean;
}

/** IBM437, bytes 0x80–0xFF (0x00–0x7F are ASCII). */
const CP437_HIGH =
  '\u00c7\u00fc\u00e9\u00e2\u00e4\u00e0\u00e5\u00e7\u00ea\u00eb\u00e8\u00ef\u00ee\u00ec\u00c4\u00c5' +
  '\u00c9\u00e6\u00c6\u00f4\u00f6\u00f2\u00fb\u00f9\u00ff\u00d6\u00dc\u00a2\u00a3\u00a5\u20a7\u0192' +
  '\u00e1\u00ed\u00f3\u00fa\u00f1\u00d1\u00aa\u00ba\u00bf\u2310\u00ac\u00bd\u00bc\u00a1\u00ab\u00bb' +
  '\u2591\u2592\u2593\u2502\u2524\u2561\u2562\u2556\u2555\u2563\u2551\u2557\u255d\u255c\u255b\u2510' +
  '\u2514\u2534\u252c\u251c\u2500\u253c\u255e\u255f\u255a\u2554\u2569\u2566\u2560\u2550\u256c\u2567' +
  '\u2568\u2564\u2565\u2559\u2558\u2552\u2553\u256b\u256a\u2518\u250c\u2588\u2584\u258c\u2590\u2580' +
  '\u03b1\u00df\u0393\u03c0\u03a3\u03c3\u00b5\u03c4\u03a6\u0398\u03a9\u03b4\u221e\u03c6\u03b5\u2229' +
  '\u2261\u00b1\u2265\u2264\u2320\u2321\u00f7\u2248\u00b0\u2219\u00b7\u221a\u207f\u00b2\u25a0\u00a0';

/** Strict UTF-8 (a BOM is kept), or undefined. */
function utf8Strict(b: Uint8Array): string | undefined {
  try {
    return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(b);
  } catch {
    return undefined;
  }
}

/**
 * A zip entry name (APPNOTE 4.4.4, 4.6.9, appendix D). With general-purpose bit 11 it is UTF-8.
 * Without it: the Info-ZIP Unicode Path field (0x7075, version 1) when its CRC-32 matches the raw
 * name; else the raw name as UTF-8 when it is valid UTF-8 (macOS, 7-Zip and many tools write UTF-8
 * without the flag; CP437 text of non-ASCII letters is practically never valid UTF-8); else IBM437.
 */
function zipName(raw: Uint8Array, flags: number, extra: Uint8Array): string {
  if (flags & 0x800) return Buffer.from(raw).toString('utf8');
  for (let i = 0; i + 4 <= extra.length; ) {
    const id = extra[i]! | (extra[i + 1]! << 8);
    const size = extra[i + 2]! | (extra[i + 3]! << 8);
    if (i + 4 + size > extra.length) break;
    if (id === 0x7075 && size >= 5 && extra[i + 4] === 1) {
      const stored = (extra[i + 5]! | (extra[i + 6]! << 8) | (extra[i + 7]! << 16) | (extra[i + 8]! << 24)) >>> 0;
      const name = stored === crc32(raw) ? utf8Strict(extra.subarray(i + 9, i + 4 + size)) : undefined;
      if (name !== undefined) return name;
    }
    i += 4 + size;
  }
  const utf8 = utf8Strict(raw);
  if (utf8 !== undefined) return utf8;
  let out = '';
  for (const c of raw) out += c < 0x80 ? String.fromCharCode(c) : CP437_HIGH[c - 0x80]!;
  return out;
}

/** The data of the first extra field `id` in `extra`, or undefined. */
function extraField(extra: Uint8Array, id: number): Uint8Array | undefined {
  for (let i = 0; i + 4 <= extra.length; ) {
    const size = extra[i + 2]! | (extra[i + 3]! << 8);
    if (i + 4 + size > extra.length) return undefined;
    if ((extra[i]! | (extra[i + 1]! << 8)) === id) return extra.subarray(i + 4, i + 4 + size);
    i += 4 + size;
  }
  return undefined;
}

/** A little-endian 64-bit integer as a number (values beyond 2^53 are rounded; they fail the limits anyway). */
const u64 = (b: Uint8Array, at: number): number => Number(new DataView(b.buffer, b.byteOffset, b.byteLength).getBigUint64(at, true));

async function readZip(b: Uint8Array, origin: string, limits: ArchiveLimits, span: SourceSpan | undefined, add: Add): Promise<void> {
  const bad = (why: string): SitefileError => new SitefileError('E_ARCHIVE', `${origin}: ${why}`, span);
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  let eocd = -1;
  for (let i = b.length - 22; i >= Math.max(0, b.length - 22 - 0xffff); i--) {
    if (v.getUint32(i, true) === 0x06054b50) { eocd = i; break; }
  }
  if (eocd < 0) throw bad('zip end-of-central-directory record not found');
  const count = v.getUint16(eocd + 10, true);
  let p = v.getUint32(eocd + 16, true);
  if (count === 0xffff || p === 0xffffffff) throw bad('zip64 archives are not supported');
  if (count > limits.maxEntries) {
    throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive has more than ${limits.maxEntries} entries`, span);
  }

  const list: ZipEntry[] = [];
  let declared = 0;
  for (let i = 0; i < count; i++) {
    if (p + 46 > b.length || v.getUint32(p, true) !== 0x02014b50) throw bad('corrupt zip central directory');
    const nameLen = v.getUint16(p + 28, true);
    const advance = 46 + nameLen + v.getUint16(p + 30, true) + v.getUint16(p + 32, true);
    if (p + advance > b.length) throw bad('corrupt zip central directory');
    const unix = v.getUint16(p + 4, true) >> 8 === 3;
    const attr = v.getUint32(p + 38, true) >>> 16;
    const fileType = attr & 0o170000;
    const extraLen = v.getUint16(p + 30, true);
    const name = zipName(b.subarray(p + 46, p + 46 + nameLen), v.getUint16(p + 8, true), b.subarray(p + 46 + nameLen, p + 46 + nameLen + extraLen));
    const e: ZipEntry = {
      name,
      rawName: b.subarray(p + 46, p + 46 + nameLen),
      method: v.getUint16(p + 10, true),
      crc: v.getUint32(p + 16, true),
      csize: v.getUint32(p + 20, true),
      usize: v.getUint32(p + 24, true),
      local: v.getUint32(p + 42, true),
      mode: unix ? attr & 0o7777 : 0o644,
      // Some tools mark directories only by mode, without a trailing slash.
      isDir: name.endsWith('/') || (unix && fileType === 0o040000),
    };
    if (v.getUint16(p + 8, true) & 1) throw bad(`encrypted zip entry '${e.name}' is not supported`);
    if (e.csize === 0xffffffff || e.usize === 0xffffffff) throw bad('zip64 archives are not supported');
    if (unix && fileType !== 0 && fileType !== 0o100000 && fileType !== 0o040000) {
      throw new SitefileError('E_ARCHIVE_UNSAFE', `unsafe archive entry '${e.name}': symlink or special file entries are not allowed`, span);
    }
    declared += e.usize;
    list.push(e);
    p += advance;
  }
  if (declared > limits.maxTotalBytes) {
    throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: archive expands beyond ${limits.maxTotalBytes} bytes`, span);
  }
  if (declared > Math.max(b.length * limits.maxRatio, RATIO_FLOOR)) {
    throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: compression ratio exceeds ${limits.maxRatio}:1`, span);
  }

  // Every local header must agree with its central record, and no two entries' bytes may overlap:
  // otherwise other tools (unzip, browsers, Archive Utility) would see different files (#184).
  const unsafe = (e: ZipEntry, why: string): SitefileError =>
    new SitefileError('E_ARCHIVE_UNSAFE', `unsafe archive entry '${e.name}': ${why}`, span);
  const spans: Array<{ e: ZipEntry; from: number; start: number; end: number }> = [];
  for (const e of list) {
    const h = e.local;
    if (h + 30 > b.length || v.getUint32(h, true) !== 0x04034b50) throw bad(`corrupt zip local header for '${e.name}'`);
    const nameLen = v.getUint16(h + 26, true);
    const start = h + 30 + nameLen + v.getUint16(h + 28, true);
    if (start + e.csize > b.length) throw bad(`truncated zip entry '${e.name}'`);
    if (Buffer.compare(b.subarray(h + 30, h + 30 + nameLen), e.rawName) !== 0) throw unsafe(e, 'its local header has a different name');
    if (v.getUint16(h + 8, true) !== e.method) throw unsafe(e, 'its local header has a different compression method');
    // With a data descriptor (bit 3) the local sizes may be zero; the descriptor follows the data.
    if (!(v.getUint16(h + 6, true) & 8)) {
      let csize: number = v.getUint32(h + 18, true);
      let usize: number = v.getUint32(h + 22, true);
      if (csize === 0xffffffff || usize === 0xffffffff) {
        // A local ZIP64 extra field holds both sizes (APPNOTE 4.5.3); Python's force_zip64 writes one.
        const z = extraField(b.subarray(h + 30 + nameLen, start), 0x0001);
        if (z && z.length >= 16) [usize, csize] = [u64(z, 0), u64(z, 8)];
      }
      if (csize !== e.csize || usize !== e.usize) throw unsafe(e, 'its local header has different sizes');
    }
    spans.push({ e, from: h, start, end: start + e.csize });
  }
  const byOffset = [...spans].sort((a, c) => a.from - c.from);
  for (let i = 1; i < byOffset.length; i++) {
    const prev = byOffset[i - 1]!;
    const cur = byOffset[i]!;
    if (cur.from < prev.end) throw unsafe(cur.e, `its data overlaps entry '${prev.e.name}'`);
  }

  for (const { e, start } of spans) {
    if (e.isDir) { add(e.name, 'dir', new Uint8Array(0), 0o755); continue; }
    const raw = b.subarray(start, start + e.csize);
    let data: Uint8Array;
    if (e.method === 0) {
      data = raw;
    } else if (e.method === 8) {
      try {
        // Headers can lie: never inflate more than the declared size.
        data = await inflateRawAsync(raw, { maxOutputLength: Math.max(e.usize, 1) });
      } catch (err) {
        throw inflateError(err, origin, false, limits, span);
      }
    } else {
      throw bad(`zip entry '${e.name}' uses unsupported compression method ${e.method}`);
    }
    if (data.length !== e.usize) throw bad(`zip entry '${e.name}' size does not match its header`);
    // Inflate cannot notice a corrupt stored entry; the CRC-32 can.
    if (crc32(data) !== e.crc) throw bad(`zip entry '${e.name}' fails its CRC-32 check`);
    add(e.name, 'file', data, e.mode);
  }
}
