import { deflateRawSync } from 'node:zlib';

/** Hand-built archive fixtures; no binary files are committed. */

export interface TarEntry {
  name: string;
  data?: string | Uint8Array;
  /** '0' file (default), '5' dir, '2' symlink, '1' hard link, 'x' pax, 'L' GNU long name. */
  type?: string;
  mode?: number;
  linkname?: string;
}

function octal(n: number, width: number): string {
  return n.toString(8).padStart(width - 1, '0') + '\0';
}

export function tarHeader(e: TarEntry, size: number): Buffer {
  const h = Buffer.alloc(512);
  h.write(e.name, 0, 100, 'utf8');
  h.write(octal(e.mode ?? 0o644, 8), 100, 'latin1');
  h.write(octal(0, 8), 108, 'latin1');
  h.write(octal(0, 8), 116, 'latin1');
  h.write(octal(size, 12), 124, 'latin1');
  h.write(octal(0, 12), 136, 'latin1');
  h.write('        ', 148, 'latin1');
  h.write(e.type ?? '0', 156, 'latin1');
  if (e.linkname) h.write(e.linkname, 157, 100, 'utf8');
  h.write('ustar\x0000', 257, 'latin1');
  let sum = 0;
  for (const b of h) sum += b;
  h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'latin1');
  return h;
}

export function tar(entries: TarEntry[]): Buffer {
  const parts: Buffer[] = [];
  for (const e of entries) {
    const data = Buffer.from(e.data ?? '');
    parts.push(tarHeader(e, data.length), data, Buffer.alloc((512 - (data.length % 512)) % 512));
  }
  parts.push(Buffer.alloc(1024));
  return Buffer.concat(parts);
}

export interface ZipEntry {
  name: string;
  data?: string | Uint8Array;
  deflate?: boolean;
  /** Unix mode in the external attributes (e.g. 0o120777 for a symlink). */
  mode?: number;
  /** Lie about the uncompressed size in the headers. */
  fakeSize?: number;
}

export function zip(entries: ZipEntry[]): Buffer {
  const local: Buffer[] = [];
  const central: Buffer[] = [];
  let offset = 0;
  for (const e of entries) {
    const raw = Buffer.from(e.data ?? '');
    const body = e.deflate ? deflateRawSync(raw) : raw;
    const name = Buffer.from(e.name);
    const usize = e.fakeSize ?? raw.length;
    const l = Buffer.alloc(30);
    l.writeUInt32LE(0x04034b50, 0);
    l.writeUInt16LE(20, 4);
    l.writeUInt16LE(e.deflate ? 8 : 0, 8);
    l.writeUInt32LE(body.length, 18);
    l.writeUInt32LE(usize, 22);
    l.writeUInt16LE(name.length, 26);
    const c = Buffer.alloc(46);
    c.writeUInt32LE(0x02014b50, 0);
    c.writeUInt16LE((3 << 8) | 20, 4);
    c.writeUInt16LE(20, 6);
    c.writeUInt16LE(e.deflate ? 8 : 0, 10);
    c.writeUInt32LE(body.length, 20);
    c.writeUInt32LE(usize, 24);
    c.writeUInt16LE(name.length, 28);
    c.writeUInt32LE(((e.mode ?? (e.name.endsWith('/') ? 0o040755 : 0o100644)) << 16) >>> 0, 38);
    c.writeUInt32LE(offset, 42);
    local.push(l, name, body);
    central.push(c, name);
    offset += l.length + name.length + body.length;
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
