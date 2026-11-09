import { crc32, deflateRaw, gzip } from './deflate.js';
import { SitefileError } from './errors.js';
import type { Tree } from './tree.js';

/**
 * Deterministic archive writers. The same tree always yields byte-identical
 * output: entries are sorted by path, there are
 * no directory entries, uid/gid are 0, there are no user/group names, modes are
 * the tree's 0644/0755, and every timestamp is fixed. Compression uses the
 * deterministic encoder in deflate.ts (shared with the Rust port), so zip,
 * tar.gz and OCI layer bytes are the same in both implementations.
 */
export interface WriterOptions {
  /**
   * Seconds since the Unix epoch used for every entry timestamp (the
   * `SOURCE_DATE_EPOCH` convention). Default: 1980-01-01 00:00 for zip (the DOS
   * epoch) and 0 for tar. The gzip header mtime is always 0.
   */
  readonly epoch?: number | undefined;
  /** tar only: directory entries (relative, no slashes at the ends, e.g. `srv`, `srv/www`) written before the files. */
  readonly dirs?: readonly string[] | undefined;
}

/** Read `SOURCE_DATE_EPOCH`; undefined if unset, E_USAGE if malformed. */
export function sourceDateEpoch(env: NodeJS.ProcessEnv = process.env): number | undefined {
  const raw = env.SOURCE_DATE_EPOCH;
  if (raw === undefined || raw === '') return undefined;
  if (!/^\d{1,12}$/.test(raw)) throw new SitefileError('E_USAGE', `SOURCE_DATE_EPOCH must be a non-negative integer, got '${raw}'`);
  return Number(raw);
}

const enc = new TextEncoder();

function entriesOf(tree: Tree): Array<{ name: string; nameBytes: Uint8Array; bytes: Uint8Array; mode: number }> {
  return tree.entries().map(([p, n]) => {
    if (!p.startsWith('/') || p.endsWith('/') || p.includes('\0') || p.includes('\\') || p.split('/').some((s, i) => i > 0 && (s === '' || s === '.' || s === '..'))) {
      throw new SitefileError('E_EXPORT', `invalid artifact path '${p}'`);
    }
    const name = p.slice(1);
    return { name, nameBytes: enc.encode(name), bytes: n.bytes, mode: n.mode };
  });
}

export { crc32 };

// --------------------------------------------------------------------- zip

const ZIP_MAX_ENTRIES = 0xffff;
const ZIP_MAX = 0xffffffff;

function dosDateTime(epoch: number | undefined): { time: number; date: number } {
  if (epoch === undefined) return { time: 0, date: (0 << 9) | (1 << 5) | 1 }; // 1980-01-01 00:00:00
  const d = new Date(Math.max(epoch, 315532800) * 1000); // clamp to 1980-01-01
  const year = Math.min(d.getUTCFullYear(), 2107);
  // Seconds are stored in 2-second units.
  return {
    time: (d.getUTCHours() << 11) | (d.getUTCMinutes() << 5) | (d.getUTCSeconds() >> 1),
    date: ((year - 1980) << 9) | ((d.getUTCMonth() + 1) << 5) | d.getUTCDate(),
  };
}

/** File extensions (ASCII, lowercase) whose content is already compressed; zip stores them without deflating. */
export const COMPRESSED_MEDIA = [
  'png', 'jpg', 'jpeg', 'gif', 'webp', 'avif', 'heic', 'jxl',
  'woff', 'woff2',
  'mp3', 'm4a', 'aac', 'ogg', 'oga', 'opus', 'flac',
  'mp4', 'm4v', 'webm', 'mov', 'mkv',
  'zip', 'gz', 'tgz', 'br', 'zst', 'xz', 'bz2', '7z', 'rar', 'wbn',
];
const COMPRESSED = new Set(COMPRESSED_MEDIA);

/** Whether a zip entry name ends in an already-compressed media extension ({@link COMPRESSED_MEDIA}), ASCII case-insensitively. */
export function isCompressedMedia(name: string): boolean {
  const base = name.slice(name.lastIndexOf('/') + 1);
  const dot = base.lastIndexOf('.');
  return dot > 0 && COMPRESSED.has(base.slice(dot + 1).replace(/[A-Z]/g, (c) => c.toLowerCase()));
}

/**
 * The extended-timestamp extra field (0x5455 "UT", APPNOTE 4.4.6, Info-ZIP extrafld.txt) with the
 * modification time only: unlike the DOS time, which unzippers read as local time, it is UTC
 * seconds, so SOURCE_DATE_EPOCH survives extraction in any time zone. The same 9 bytes go in the
 * local and the central header. Without an epoch it says 1980-01-01, as the DOS time does.
 */
function extendedTimestamp(epoch: number | undefined): Buffer {
  const ut = Buffer.alloc(9);
  ut.writeUInt16LE(0x5455, 0);
  ut.writeUInt16LE(5, 2);
  ut[4] = 1; // mtime present
  ut.writeUInt32LE(Math.min(Math.max(epoch ?? 315532800, 0), 0xffffffff), 5);
  return ut;
}

/**
 * Zip (no zip64). Deflate when it shrinks the data, otherwise stored.
 *
 * `first` (tree paths, e.g. `['/mimetype']` for EPUB): these entries come first, in that order, stored
 * and without an extra field (the EPUB container requires its `mimetype` entry that way); the rest
 * follow in path order as usual.
 */
export function writeZip(tree: Tree, options: WriterOptions = {}, first: readonly string[] = []): Uint8Array {
  const all = entriesOf(tree);
  const firstNames = first.map((p) => p.slice(1));
  const plain = new Set(firstNames);
  const entries = [...firstNames.flatMap((n) => all.filter((e) => e.name === n)), ...all.filter((e) => !plain.has(e.name))];
  if (entries.length > ZIP_MAX_ENTRIES) {
    throw new SitefileError('E_EXPORT', `zip export supports at most ${ZIP_MAX_ENTRIES} entries (zip64 is not implemented); got ${entries.length}`);
  }
  const { time, date } = dosDateTime(options.epoch);
  const ut = extendedTimestamp(options.epoch);
  const chunks: Uint8Array[] = [];
  const central: Uint8Array[] = [];
  let offset = 0;
  for (const e of entries) {
    if (e.nameBytes.length > 0xffff) throw new SitefileError('E_EXPORT', `zip entry name is too long (${e.nameBytes.length} bytes): '${e.name.slice(0, 60)}…'`);
    const isPlain = plain.has(e.name);
    // Already-compressed media is stored as is: deflate would only spend time to gain nothing (#203).
    const deflated = isPlain || isCompressedMedia(e.name) ? undefined : deflateRaw(e.bytes);
    const stored = deflated === undefined || deflated.length >= e.bytes.length;
    const extra = isPlain ? Buffer.alloc(0) : ut;
    const data: Uint8Array = stored ? e.bytes : deflated;
    if (e.bytes.length > ZIP_MAX || data.length > ZIP_MAX) {
      throw new SitefileError('E_EXPORT', `zip export does not support files of 4 GiB or more (zip64 is not implemented): '/${e.name}'`);
    }
    if (offset > ZIP_MAX) throw new SitefileError('E_EXPORT', 'zip export does not support archives of 4 GiB or more (zip64 is not implemented)');
    const method = stored ? 0 : 8;
    const crc = crc32(e.bytes);

    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0);
    lh.writeUInt16LE(20, 4); // version needed
    lh.writeUInt16LE(0x0800, 6); // UTF-8 names
    lh.writeUInt16LE(method, 8);
    lh.writeUInt16LE(time, 10);
    lh.writeUInt16LE(date, 12);
    lh.writeUInt32LE(crc, 14);
    lh.writeUInt32LE(data.length, 18);
    lh.writeUInt32LE(e.bytes.length, 22);
    lh.writeUInt16LE(e.nameBytes.length, 26);
    lh.writeUInt16LE(extra.length, 28);

    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0);
    ch.writeUInt16LE((3 << 8) | 20, 4); // made by: Unix, spec 2.0
    ch.writeUInt16LE(20, 6);
    ch.writeUInt16LE(0x0800, 8);
    ch.writeUInt16LE(method, 10);
    ch.writeUInt16LE(time, 12);
    ch.writeUInt16LE(date, 14);
    ch.writeUInt32LE(crc, 16);
    ch.writeUInt32LE(data.length, 20);
    ch.writeUInt32LE(e.bytes.length, 24);
    ch.writeUInt16LE(e.nameBytes.length, 28);
    ch.writeUInt16LE(extra.length, 30);
    // comment, disk number, internal attrs = 0
    ch.writeUInt32LE(((0o100000 | e.mode) << 16) >>> 0, 38);
    ch.writeUInt32LE(offset, 42);

    chunks.push(lh, e.nameBytes, extra, data);
    central.push(ch, e.nameBytes, extra);
    offset += 30 + e.nameBytes.length + extra.length + data.length;
  }
  const cdSize = central.reduce((n, c) => n + c.length, 0);
  if (offset + cdSize > ZIP_MAX) throw new SitefileError('E_EXPORT', 'zip export does not support archives of 4 GiB or more (zip64 is not implemented)');
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(cdSize, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...chunks, ...central, end]);
}

// --------------------------------------------------------------------- tar

const BLOCK = 512;
const TAR_MAX_SIZE = 8 ** 11 - 1; // 11 octal digits

function putOctal(h: Buffer, at: number, width: number, value: number): void {
  h.write(value.toString(8).padStart(width - 1, '0') + '\0', at, 'latin1');
}

/** Largest prefix of `s` (UTF-8) not longer than `max` bytes, cut on a character boundary. */
function utf8Head(s: string, max: number): Uint8Array {
  const b = enc.encode(s);
  if (b.length <= max) return b;
  let end = max;
  while (end > 0 && (b[end]! & 0xc0) === 0x80) end--;
  return b.subarray(0, end);
}

function tarHeader(name: Uint8Array, prefix: Uint8Array, size: number, mode: number, mtime: number, type: string): Buffer {
  const h = Buffer.alloc(BLOCK);
  h.set(name, 0);
  putOctal(h, 100, 8, mode);
  putOctal(h, 108, 8, 0); // uid
  putOctal(h, 116, 8, 0); // gid
  putOctal(h, 124, 12, size);
  putOctal(h, 136, 12, mtime);
  h.fill(0x20, 148, 156); // checksum placeholder
  h.write(type, 156, 'latin1');
  h.write('ustar\0', 257, 'latin1');
  h.write('00', 263, 'latin1');
  h.set(prefix, 345);
  let sum = 0;
  for (const b of h) sum += b;
  h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'latin1');
  return h;
}

function padTo512(data: Uint8Array): Uint8Array[] {
  const rem = data.length % BLOCK;
  return rem === 0 ? [data] : [data, Buffer.alloc(BLOCK - rem)];
}

function paxRecord(key: string, value: string): Uint8Array {
  const body = enc.encode(` ${key}=${value}\n`);
  // The length counts its own decimal digits.
  let len = body.length + 1;
  while (String(len).length + body.length !== len) len = String(len).length + body.length;
  return Buffer.concat([Buffer.from(String(len), 'latin1'), body]);
}

/** Split `name` for the ustar name/prefix fields, or undefined if it cannot fit. */
function ustarSplit(nameBytes: Uint8Array, name: string): { name: Uint8Array; prefix: Uint8Array } | undefined {
  if (nameBytes.length <= 100) return { name: nameBytes, prefix: new Uint8Array(0) };
  for (let i = 0; i < nameBytes.length; i++) {
    if (nameBytes[i] !== 0x2f) continue;
    const prefix = nameBytes.subarray(0, i);
    const rest = nameBytes.subarray(i + 1);
    if (prefix.length > 155) break;
    if (rest.length <= 100 && rest.length > 0) return { name: rest, prefix };
  }
  void name;
  return undefined;
}

/** POSIX ustar; long names use the prefix field, then a pax `path` extended header. */
export function writeTar(tree: Tree, options: WriterOptions = {}): Uint8Array {
  const mtime = options.epoch ?? 0;
  const chunks: Uint8Array[] = [];
  let n = 0;
  for (const d of options.dirs ?? []) chunks.push(tarHeader(enc.encode(d + '/'), new Uint8Array(0), 0, 0o755, mtime, '5'));
  for (const e of entriesOf(tree)) {
    if (e.bytes.length > TAR_MAX_SIZE) {
      throw new SitefileError('E_EXPORT', `tar export does not support files of 8 GiB or more: '/${e.name}'`);
    }
    let split = ustarSplit(e.nameBytes, e.name);
    if (!split) {
      const rec = paxRecord('path', e.name);
      const pname = enc.encode(`PaxHeaders/${n++}`);
      chunks.push(tarHeader(pname, new Uint8Array(0), rec.length, 0o644, mtime, 'x'), ...padTo512(rec));
      split = { name: utf8Head(e.name, 100), prefix: new Uint8Array(0) };
    }
    chunks.push(tarHeader(split.name, split.prefix, e.bytes.length, e.mode, mtime, '0'), ...padTo512(e.bytes));
  }
  chunks.push(Buffer.alloc(BLOCK * 2));
  return Buffer.concat(chunks);
}

/** gzip of the tar, header mtime 0 and no filename, so the stream is reproducible. */
export function writeTgz(tree: Tree, options: WriterOptions = {}): Uint8Array {
  return gzipTar(writeTar(tree, options));
}

/** Reproducible gzip of an already-written tar (see deflate.ts for the exact format). */
export function gzipTar(tar: Uint8Array): Uint8Array {
  return gzip(tar);
}
