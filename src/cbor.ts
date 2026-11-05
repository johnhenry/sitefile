/**
 * A minimal deterministic CBOR (RFC 8949) encoder: just unsigned integers,
 * byte strings, text strings, arrays and maps. Every value is encoded to bytes
 * eagerly; maps are emitted in the RFC 8949 section 4.2.1 core deterministic
 * order (keys sorted bytewise by their encoded form, which puts shorter first).
 */
const enc = new TextEncoder();

export function cborHead(major: number, n: number | bigint): Uint8Array {
  const m = major << 5;
  if (n < 24) return Uint8Array.of(m | Number(n));
  if (n < 0x100) return Uint8Array.of(m | 24, Number(n));
  if (n < 0x10000) return Uint8Array.of(m | 25, Number(n) >> 8, Number(n) & 0xff);
  if (n < 0x100000000) {
    const b = Buffer.alloc(5);
    b[0] = m | 26;
    b.writeUInt32BE(Number(n), 1);
    return b;
  }
  const b = Buffer.alloc(9);
  b[0] = m | 27;
  b.writeBigUInt64BE(BigInt(n), 1);
  return b;
}

const concat = (parts: Uint8Array[]): Uint8Array => Buffer.concat(parts);

export const cborUint = (n: number | bigint): Uint8Array => cborHead(0, n);
export const cborBytes = (b: Uint8Array): Uint8Array => concat([cborHead(2, b.length), b]);
export const cborText = (s: string): Uint8Array => {
  const b = enc.encode(s);
  return concat([cborHead(3, b.length), b]);
};
/** Array of already-encoded items. */
export const cborArray = (items: readonly Uint8Array[]): Uint8Array => concat([cborHead(4, items.length), ...items]);

/** Map of already-encoded [key, value] pairs, in deterministic key order. */
export function cborMap(entries: ReadonlyArray<readonly [Uint8Array, Uint8Array]>): Uint8Array {
  const sorted = [...entries].sort(([a], [b]) => Buffer.compare(a, b));
  for (let i = 1; i < sorted.length; i++) {
    if (Buffer.compare(sorted[i - 1]![0], sorted[i]![0]) === 0) throw new Error('duplicate CBOR map key');
  }
  return concat([cborHead(5, sorted.length), ...sorted.flatMap(([k, v]) => [k, v])]);
}

// ------------------------------------------------------------------ decoder

/**
 * Decoded CBOR. Unsigned integers above 2^53-1 and negative integers below
 * -(2^53-1) come back as bigint. Byte strings are views into the input (not
 * copies). Maps are `Map`s (never plain objects, so keys like `__proto__` are
 * inert); a key that appears twice is an error.
 */
export type CborValue = number | bigint | string | boolean | null | Uint8Array | CborValue[] | Map<CborValue, CborValue>;

export interface CborLimits {
  /** Maximum nesting of arrays/maps (default 32). */
  maxDepth?: number;
  /** Maximum number of data items decoded in total (default 1 000 000). */
  maxItems?: number;
}

export class CborError extends Error {
  constructor(message: string, readonly offset: number) {
    super(`${message} (at byte ${offset})`);
    this.name = 'CborError';
  }
}

/**
 * Read one item head (major type + argument) at `pos`. Definite lengths only:
 * additional info 28-31 (reserved and indefinite) is an error.
 */
export function readCborHead(bytes: Uint8Array, pos: number): { major: number; arg: number | bigint; next: number } {
  if (pos >= bytes.length) throw new CborError('unexpected end of input', pos);
  const ib = bytes[pos]!;
  const major = ib >> 5;
  const ai = ib & 31;
  const need = (n: number): void => {
    if (pos + 1 + n > bytes.length) throw new CborError('unexpected end of input', pos);
  };
  if (ai < 24) return { major, arg: ai, next: pos + 1 };
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  switch (ai) {
    case 24: need(1); return { major, arg: bytes[pos + 1]!, next: pos + 2 };
    case 25: need(2); return { major, arg: v.getUint16(pos + 1), next: pos + 3 };
    case 26: need(4); return { major, arg: v.getUint32(pos + 1), next: pos + 5 };
    case 27: {
      need(8);
      const n = v.getBigUint64(pos + 1);
      return { major, arg: n <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(n) : n, next: pos + 9 };
    }
    default:
      throw new CborError(ai === 31 ? 'indefinite-length items are not supported' : 'reserved additional information', pos);
  }
}

const utf8 = new TextDecoder('utf-8', { fatal: true });

/** Decode the single CBOR item at the start of `bytes`; `end` is the offset just past it. */
export function decodeCbor(bytes: Uint8Array, limits: CborLimits = {}): { value: CborValue; end: number } {
  const maxDepth = limits.maxDepth ?? 32;
  const maxItems = limits.maxItems ?? 1_000_000;
  let items = 0;

  const item = (pos: number, depth: number): { value: CborValue; end: number } => {
    if (++items > maxItems) throw new CborError(`more than ${maxItems} items`, pos);
    const { major, arg, next } = readCborHead(bytes, pos);
    const remaining = bytes.length - next;
    // A declared length can never exceed what is left of the input (every
    // element or byte takes at least one byte), so nothing is sized from it.
    const length = (min: number): number => {
      if (typeof arg === 'bigint' || arg * min > remaining) throw new CborError('declared length exceeds remaining input', pos);
      return arg;
    };
    switch (major) {
      case 0:
        return { value: arg, end: next };
      case 1:
        return { value: typeof arg === 'bigint' ? -1n - arg : -1 - arg, end: next };
      case 2: {
        const n = length(1);
        return { value: bytes.subarray(next, next + n), end: next + n };
      }
      case 3: {
        const n = length(1);
        try {
          return { value: utf8.decode(bytes.subarray(next, next + n)), end: next + n };
        } catch {
          throw new CborError('invalid UTF-8 in text string', next);
        }
      }
      case 4: {
        const n = length(1);
        if (depth >= maxDepth) throw new CborError(`nesting deeper than ${maxDepth}`, pos);
        const out: CborValue[] = [];
        let p = next;
        for (let i = 0; i < n; i++) {
          const r = item(p, depth + 1);
          out.push(r.value);
          p = r.end;
        }
        return { value: out, end: p };
      }
      case 5: {
        const n = length(2);
        if (depth >= maxDepth) throw new CborError(`nesting deeper than ${maxDepth}`, pos);
        const out = new Map<CborValue, CborValue>();
        const seen = new Set<string>();
        let p = next;
        for (let i = 0; i < n; i++) {
          const k = item(p, depth + 1);
          const id = Buffer.from(bytes.subarray(p, k.end)).toString('latin1');
          if (seen.has(id)) throw new CborError('duplicate map key', p);
          seen.add(id);
          const v = item(k.end, depth + 1);
          out.set(k.value, v.value);
          p = v.end;
        }
        return { value: out, end: p };
      }
      case 6:
        throw new CborError('tags are not supported', pos);
      default: {
        // major 7: only the simple values false, true and null.
        const ib = bytes[pos]!;
        if (ib === 0xf4) return { value: false, end: next };
        if (ib === 0xf5) return { value: true, end: next };
        if (ib === 0xf6) return { value: null, end: next };
        throw new CborError('unsupported simple value or float', pos);
      }
    }
  };
  return item(0, 0);
}
