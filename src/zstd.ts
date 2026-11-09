import { createHash } from 'node:crypto';

/**
 * A small, deterministic Zstandard (RFC 8878) encoder for Compression Dictionary Transport deltas
 * (`dcz`, issue #215), ported byte for byte as rust/src/zstd.rs. It is not libzstd: it trades ratio
 * for a short, specified algorithm that both implementations run identically.
 *
 * - Matching: greedy, over the dictionary followed by the content. At each position the candidates are
 *   the previous match's offset, the same offset in the dictionary, offsets up to 16 away from the
 *   previous one (nearest first, smaller first), then the last position the same 4 bytes (a 32-bit little-endian key)
 *   started at; the longest match wins (the earlier candidate on a tie), if it is at least 4 bytes. Every
 *   position is inserted into the table, including those inside matches.
 * - Blocks hold at most 128 KiB of output. A block is compressed (raw literals, sequences with the
 *   predefined FSE tables) unless its raw form is no larger, then it is stored raw. Offsets are always
 *   written as new offsets (offset + 3), never as repeat codes.
 * - The frame has a window descriptor large enough for the dictionary and the content, a 4-byte
 *   content size and no checksum or dictionary ID (the dictionary is raw content, identified by the
 *   `dcz` header's SHA-256).
 */

const MAX_BLOCK = 128 * 1024;
const MIN_MATCH = 4;
/** How far from the previous match's offset to look (bytes inserted or deleted since). */
const NEAR = 16;

// ---------------------------------------------------------------- FSE (predefined tables only)

const LL_DIST = [4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1, -1, -1, -1, -1];
const ML_DIST = [
  1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1,
  -1,
];
const OF_DIST = [1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1];

interface CTable {
  readonly log: number;
  readonly next: number[];
  readonly deltaNbBits: number[];
  readonly deltaFindState: number[];
}

const highbit = (v: number): number => 31 - Math.clz32(v);

/** FSE compression table for a normalized distribution (as zstd's FSE_buildCTable builds it). */
function buildCTable(dist: readonly number[], log: number): CTable {
  const size = 1 << log;
  const symbols = new Array<number>(size).fill(0);
  let high = size - 1;
  for (let s = 0; s < dist.length; s++) if (dist[s] === -1) symbols[high--] = s;
  const step = (size >> 1) + (size >> 3) + 3;
  let pos = 0;
  for (let s = 0; s < dist.length; s++) {
    const n = dist[s] as number;
    for (let i = 0; i < n; i++) {
      symbols[pos] = s;
      do pos = (pos + step) & (size - 1);
      while (pos > high);
    }
  }
  const cumul: number[] = [0];
  for (let s = 0; s < dist.length; s++) cumul.push((cumul[s] as number) + ((dist[s] as number) === -1 ? 1 : (dist[s] as number)));
  const next = new Array<number>(size).fill(0);
  const at = cumul.slice();
  for (let u = 0; u < size; u++) {
    const s = symbols[u] as number;
    next[at[s]!++] = size + u;
  }
  const deltaNbBits: number[] = [];
  const deltaFindState: number[] = [];
  let total = 0;
  for (let s = 0; s < dist.length; s++) {
    const n = dist[s] as number;
    if (n === -1 || n === 1) {
      deltaNbBits.push((log << 16) - size);
      deltaFindState.push(total - 1);
      total++;
    } else {
      const maxBitsOut = log - highbit(n - 1);
      deltaNbBits.push((maxBitsOut << 16) - (n << maxBitsOut));
      deltaFindState.push(total - n);
      total += n;
    }
  }
  return { log, next, deltaNbBits, deltaFindState };
}

const LL_TABLE = buildCTable(LL_DIST, 6);
const ML_TABLE = buildCTable(ML_DIST, 6);
const OF_TABLE = buildCTable(OF_DIST, 5);

/** Little-endian bit writer (zstd's BIT_CStream). */
class Bits {
  private readonly out: number[] = [];
  private acc = 0n;
  private n = 0;
  add(value: number, bits: number): void {
    if (bits === 0) return;
    this.acc |= (BigInt(value) & ((1n << BigInt(bits)) - 1n)) << BigInt(this.n);
    this.n += bits;
    while (this.n >= 8) {
      this.out.push(Number(this.acc & 0xffn));
      this.acc >>= 8n;
      this.n -= 8;
    }
  }
  /** The end mark (a 1 bit), then the last partial byte. */
  close(): Uint8Array {
    this.add(1, 1);
    if (this.n > 0) this.out.push(Number(this.acc & 0xffn));
    return new Uint8Array(this.out);
  }
}

class FseState {
  value = 0;
  constructor(private readonly t: CTable) {}
  init(symbol: number): void {
    const nbBitsOut = ((this.t.deltaNbBits[symbol] as number) + (1 << 15)) >> 16;
    const v = (nbBitsOut << 16) - (this.t.deltaNbBits[symbol] as number);
    this.value = this.t.next[(v >> nbBitsOut) + (this.t.deltaFindState[symbol] as number)] as number;
  }
  encode(bits: Bits, symbol: number): void {
    const nbBitsOut = (this.value + (this.t.deltaNbBits[symbol] as number)) >> 16;
    bits.add(this.value, nbBitsOut);
    this.value = this.t.next[(this.value >> nbBitsOut) + (this.t.deltaFindState[symbol] as number)] as number;
  }
  flush(bits: Bits): void {
    bits.add(this.value, this.t.log);
  }
}

// ---------------------------------------------------------------- codes

const LL_BASE = [16, 18, 20, 22, 24, 28, 32, 40, 48, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];
const LL_EXTRA = [1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
const ML_BASE = [35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027, 2051, 4099, 8195, 16387, 32771, 65539];
const ML_EXTRA = [1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/** [code, extra bits, extra value] of a literal length. */
function llCode(ll: number): [number, number, number] {
  if (ll < 16) return [ll, 0, 0];
  let i = LL_BASE.length - 1;
  while ((LL_BASE[i] as number) > ll) i--;
  return [16 + i, LL_EXTRA[i] as number, ll - (LL_BASE[i] as number)];
}

/** [code, extra bits, extra value] of a match length (at least 3). */
function mlCode(ml: number): [number, number, number] {
  if (ml < 35) return [ml - 3, 0, 0];
  let i = ML_BASE.length - 1;
  while ((ML_BASE[i] as number) > ml) i--;
  return [32 + i, ML_EXTRA[i] as number, ml - (ML_BASE[i] as number)];
}

interface Sequence {
  readonly ll: number;
  readonly ml: number;
  readonly offset: number;
}

// ---------------------------------------------------------------- blocks

function literalsHeader(n: number): number[] {
  if (n < 32) return [n << 3];
  if (n < 4096) return [(1 << 2) | ((n & 0xf) << 4), n >> 4];
  return [(3 << 2) | ((n & 0xf) << 4), (n >> 4) & 0xff, n >> 12];
}

function sequencesHeader(n: number): number[] {
  if (n < 128) return [n];
  if (n < 0x7f00) return [(n >> 8) + 0x80, n & 0xff];
  return [0xff, (n - 0x7f00) & 0xff, (n - 0x7f00) >> 8];
}

function concat(parts: readonly (Uint8Array | readonly number[])[]): Uint8Array {
  let n = 0;
  for (const p of parts) n += p.length;
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function compressedBlock(literals: Uint8Array, seqs: readonly Sequence[]): Uint8Array {
  const head = literalsHeader(literals.length);
  const seqHead = sequencesHeader(seqs.length);
  if (seqs.length === 0) return concat([head, literals, seqHead]);
  const modes = [0]; // predefined modes for literal lengths, offsets and match lengths
  const codes = seqs.map((s) => {
    const offBase = s.offset + 3;
    const of = highbit(offBase);
    return { ll: llCode(s.ll), ml: mlCode(s.ml), of: [of, of, offBase - (1 << of)] as [number, number, number] };
  });
  const bits = new Bits();
  const ll = new FseState(LL_TABLE);
  const ml = new FseState(ML_TABLE);
  const of = new FseState(OF_TABLE);
  const last = codes[codes.length - 1]!;
  ml.init(last.ml[0]);
  of.init(last.of[0]);
  ll.init(last.ll[0]);
  const extras = (c: (typeof codes)[number]): void => {
    bits.add(c.ll[2], c.ll[1]);
    bits.add(c.ml[2], c.ml[1]);
    bits.add(c.of[2], c.of[1]);
  };
  extras(last);
  for (let i = codes.length - 2; i >= 0; i--) {
    const c = codes[i]!;
    of.encode(bits, c.of[0]);
    ml.encode(bits, c.ml[0]);
    ll.encode(bits, c.ll[0]);
    extras(c);
  }
  ml.flush(bits);
  of.flush(bits);
  ll.flush(bits);
  return concat([head, literals, seqHead, modes, bits.close()]);
}

function blockHeader(last: boolean, type: number, size: number): number[] {
  const v = (last ? 1 : 0) | (type << 1) | (size << 3);
  return [v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff];
}

/** One frame of `content` compressed against the raw-content `dictionary`. */
export function zstdCompress(content: Uint8Array, dictionary: Uint8Array = new Uint8Array(0)): Uint8Array {
  const all = new Uint8Array(dictionary.length + content.length);
  all.set(dictionary, 0);
  all.set(content, dictionary.length);
  const key = (p: number): number => (all[p]! | (all[p + 1]! << 8) | (all[p + 2]! << 16) | (all[p + 3]! << 24)) >>> 0;
  const table = new Map<number, number>();
  const insert = (p: number): void => {
    if (p + MIN_MATCH <= all.length) table.set(key(p), p);
  };
  for (let p = 0; p < dictionary.length; p++) insert(p);

  const out: Array<Uint8Array | number[]> = [[0x28, 0xb5, 0x2f, 0xfd]];
  // Window: the smallest power of two (at least 1 KiB) covering the dictionary and the content.
  const need = Math.max(all.length, 1);
  let windowLog = 10;
  while (2 ** windowLog < need) windowLog++;
  out.push([0x80, (windowLog - 10) << 3]); // FCS field 4 bytes, not single segment, no checksum, no dict ID
  out.push([content.length & 0xff, (content.length >>> 8) & 0xff, (content.length >>> 16) & 0xff, (content.length >>> 24) & 0xff]);

  let pos = dictionary.length;
  const end = all.length;
  let lastOffset = 0;
  if (pos === end) {
    out.push(blockHeader(true, 0, 0));
    return concat(out);
  }
  while (pos < end) {
    const blockEnd = Math.min(end, pos + MAX_BLOCK);
    const blockStart = pos;
    const literals: number[] = []; // at most MAX_BLOCK
    const seqs: Sequence[] = [];
    let run = 0;
    while (pos < blockEnd) {
      let len = 0;
      let cand = -1;
      const matchLen = (c: number): number => {
        let n = 0;
        while (pos + n < blockEnd && all[c + n] === all[pos + n]) n++;
        return n;
      };
      // The previous match's offset first (an edited file keeps its alignment), then the hash table.
      if (lastOffset > 0 && pos - lastOffset >= 0) {
        len = matchLen(pos - lastOffset);
        cand = pos - lastOffset;
      }
      const tryOffset = (off: number): void => {
        if (off <= 0 || pos - off < 0 || pos - off === cand) return;
        const n = matchLen(pos - off);
        if (n > len) {
          len = n;
          cand = pos - off;
        }
      };
      // The same place in the dictionary (the previous version of the file), then offsets near the
      // previous one (a small insertion or deletion since the last match).
      tryOffset(dictionary.length);
      if (lastOffset > 0) {
        for (let d = 1; d <= NEAR; d++) {
          tryOffset(lastOffset - d);
          tryOffset(lastOffset + d);
        }
      }
      if (pos + MIN_MATCH <= end) {
        const c = table.get(key(pos));
        if (c !== undefined && c !== cand) {
          const n = matchLen(c);
          if (n > len) {
            len = n;
            cand = c;
          }
        }
      }
      if (len >= MIN_MATCH) {
        lastOffset = pos - cand;
        seqs.push({ ll: run, ml: len, offset: pos - cand });
        run = 0;
        for (let k = 0; k < len; k++) insert(pos + k);
        pos += len;
      } else {
        literals.push(all[pos]!);
        run++;
        insert(pos);
        pos++;
      }
    }
    const raw = all.subarray(blockStart, blockEnd);
    // Trailing literals after the last sequence are part of the literals section.
    const block = compressedBlock(new Uint8Array(literals), seqs);
    const last = blockEnd === end;
    if (block.length < raw.length) out.push(blockHeader(last, 2, block.length), block);
    else out.push(blockHeader(last, 0, raw.length), raw);
  }
  return concat(out);
}

/** The `dcz` header: a skippable-frame magic, then the SHA-256 of the dictionary. */
export function dczEncode(content: Uint8Array, dictionary: Uint8Array): Uint8Array {
  const hash = createHash('sha256').update(dictionary).digest();
  const frame = zstdCompress(content, dictionary);
  const out = new Uint8Array(40 + frame.length);
  out.set([0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00], 0);
  out.set(hash, 8);
  out.set(frame, 40);
  return out;
}
