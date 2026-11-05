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
