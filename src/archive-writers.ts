import { gzipSync, deflateRawSync } from 'node:zlib';
import { SitefileError } from './errors.js';
import type { Tree } from './tree.js';

/**
 * Deterministic archive writers. The same tree always yields byte-identical
 * output (for a given Node/zlib version): entries are sorted by path, there are
 * no directory entries, uid/gid are 0, there are no user/group names, modes are
 * the tree's 0644/0755, and every timestamp is fixed.
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

// ------------------------------------------------------------------ CRC-32

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

export function crc32(data: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < data.length; i++) c = CRC_TABLE[(c ^ data[i]!) & 0xff]! ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

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

/** Zip (no zip64). Deflate when it shrinks the data, otherwise stored. */
export function writeZip(tree: Tree, options: WriterOptions = {}): Uint8Array {
  const entries = entriesOf(tree);
  if (entries.length > ZIP_MAX_ENTRIES) {
    throw new SitefileError('E_EXPORT', `zip export supports at most ${ZIP_MAX_ENTRIES} entries (zip64 is not implemented); got ${entries.length}`);
  }
  const { time, date } = dosDateTime(options.epoch);
  const chunks: Uint8Array[] = [];
  const central: Uint8Array[] = [];
  let offset = 0;
  for (const e of entries) {
    if (e.nameBytes.length > 0xffff) throw new SitefileError('E_EXPORT', `zip entry name is too long (${e.nameBytes.length} bytes): '${e.name.slice(0, 60)}…'`);
    const deflated = deflateRawSync(e.bytes, { level: 9 });
    const stored = deflated.length >= e.bytes.length;
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
    lh.writeUInt16LE(0, 28);

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
    // extra, comment, disk number, internal attrs = 0
    ch.writeUInt32LE(((0o100000 | e.mode) << 16) >>> 0, 38);
    ch.writeUInt32LE(offset, 42);

    chunks.push(lh, e.nameBytes, data);
    central.push(ch, e.nameBytes);
    offset += 30 + e.nameBytes.length + data.length;
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

/** Reproducible gzip of an already-written tar. */
export function gzipTar(tar: Uint8Array): Uint8Array {
  const gz = Buffer.from(gzipSync(tar, { level: 9 }));
  gz[9] = 3; // OS = Unix, whatever platform zlib was built for
  return gz;
}
