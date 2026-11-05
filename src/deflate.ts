/**
 * Deterministic DEFLATE (RFC 1951) and gzip (RFC 1952) encoder.
 *
 * `rust/src/deflate.rs` implements the same algorithm and must produce the same
 * bytes for every input, so that archives and OCI layers (and therefore image
 * IDs) are identical between the two implementations. Any change here must be
 * made in both files, and the differential fixtures regenerated
 * (`node rust/tests/fixtures/deflate/generate.mjs`). Decompression is not
 * affected: any inflater reads this output.
 *
 * Specification (every choice below is part of the output format):
 *
 * LZ77 matching
 *  - Window 32768 bytes; matches are 3..258 bytes at distance 1..32768.
 *  - Hash of position p (only when p + 3 <= n): x = d[p] << 16 | d[p+1] << 8 | d[p+2];
 *    h = (x * 0x9E3779B1 mod 2^32) >> 17 (15 bits). `head[h]` holds the most
 *    recent inserted position with that hash, `prev[p & 32767]` the position
 *    that was in `head[h]` when p was inserted (-1 = none).
 *  - findMatch(p): if p + 3 > n, no match. Otherwise walk the chain starting
 *    at head[h(p)]: candidate c is considered while c >= 0, p - c <= 32768 and
 *    fewer than MAX_CHAIN (64) candidates have been considered; the next
 *    candidate is prev[c & 32767], and the walk stops if that is not < c.
 *    For each candidate, len = length of the common prefix of d[c..] and
 *    d[p..], at most min(258, n - p). A candidate replaces the best only if
 *    len > bestLen (so on ties the nearest wins); the walk stops once
 *    bestLen >= 258 (or reaches min(258, n - p)). Then p is inserted.
 *    A best match of length 3 at distance > 4096 is dropped (no match).
 *  - Every position p with p + 3 <= n is inserted exactly once, in increasing
 *    order, before findMatch is called for any later position.
 *  - Parsing (lazy, one position at a time): at p, m = findMatch(p). If
 *    m.len < 3: literal d[p], p += 1. Otherwise, while m.len < LAZY_LIMIT (16)
 *    and p + 1 < n: m2 = findMatch(p + 1), searching at most MAX_CHAIN / 4 (16)
 *    candidates when m.len >= GOOD_LENGTH (8); if m2.len > m.len, emit literal
 *    d[p], p += 1, m = m2 and repeat; else stop. Then emit match m at p, insert
 *    the positions up to p + m.len - 1 that are not yet inserted, p += m.len.
 *
 * Blocks
 *  - Symbols (literals and length/distance pairs) are buffered. At the start
 *    of each parsing step (p < n), if the buffer holds >= BLOCK_SYMBOLS (16384)
 *    symbols it is written as a non-final block. When the input is exhausted
 *    the remaining symbols (none only for empty input) are written as the
 *    final block (BFINAL = 1).
 *  - Each block is written as whichever of stored, fixed Huffman or dynamic
 *    Huffman takes the fewest bits, given the current bit position; on a tie
 *    the earlier of stored, fixed, dynamic wins. Stored is a candidate only
 *    when the block covers <= 65535 input bytes; its cost is 3 header bits,
 *    padding to a byte boundary, 32 bits of LEN/NLEN and 8 bits per byte.
 *  - Huffman code lengths (literal/length tree max 15 bits, distance tree max
 *    15, code-length tree max 7) come from package-merge: leaves are the
 *    symbols with nonzero frequency sorted by (frequency, symbol); L - 1 times,
 *    pair consecutive items of the previous list (dropping an odd last item)
 *    into packages and merge them with the leaves by weight, a leaf before a
 *    package on equal weight; a symbol's length is the number of times it
 *    occurs in the first 2k - 2 items of the final list (k = leaf count). If
 *    fewer than two symbols are used, the used one (if any) and the lowest
 *    unused symbols are added until there are two, each with length 1.
 *    Codes are canonical (RFC 1951 3.2.2).
 *  - Dynamic header: HLIT = max(257, last used literal/length symbol + 1),
 *    HDIST = max(1, last used distance symbol + 1) (an unused tree still has
 *    two length-1 codes, so this is at least 2); HCLEN = max(4, last nonzero
 *    entry in the order 16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1,15 + 1).
 *    The HLIT + HDIST lengths form one sequence, run-length coded left to
 *    right: for value v with run r of equal values starting here, if v = 0:
 *    r >= 11 → 18 (min(r, 138)), r >= 3 → 17 (r), else a single 0; if v != 0:
 *    emit v once, then while the rest of the run is >= 3, 16 (min(rest, 6)).
 *
 * gzip: header 1f 8b 08, FLG 0, MTIME 0, XFL 0, OS 3 (Unix), then the raw
 * deflate stream, CRC-32 and ISIZE (little-endian).
 */

const WSIZE = 32768;
const WMASK = WSIZE - 1;
const HASH_BITS = 15;
const MAX_MATCH = 258;
const MIN_MATCH = 3;
const MAX_CHAIN = 64;
const LAZY_LIMIT = 16;
const GOOD_LENGTH = 8;
const TOO_FAR = 4096;
const BLOCK_SYMBOLS = 16384;
const STORED_MAX = 65535;

const LENGTH_BASE = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CL_ORDER = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/** match length (3..258) → length code index 0..28 */
const LEN_CODE = new Uint8Array(MAX_MATCH + 1);
/** distance - 1 → distance code, for distances 1..32768 */
const DIST_CODE = new Uint8Array(WSIZE);
for (let c = 0; c < 29; c++) {
  const end = c === 28 ? 259 : LENGTH_BASE[c + 1]!;
  for (let l = LENGTH_BASE[c]!; l < end; l++) LEN_CODE[l] = c;
}
LEN_CODE[258] = 28;
for (let c = 0; c < 30; c++) {
  const end = c === 29 ? WSIZE + 1 : DIST_BASE[c + 1]!;
  for (let d = DIST_BASE[c]!; d < end; d++) DIST_CODE[d - 1] = c;
}

const FIXED_LIT_LEN = new Uint8Array(288);
for (let i = 0; i < 288; i++) FIXED_LIT_LEN[i] = i < 144 ? 8 : i < 256 ? 9 : i < 280 ? 7 : 8;
const FIXED_DIST_LEN = new Uint8Array(30).fill(5);
const FIXED_LIT_CODE = canonicalCodes(FIXED_LIT_LEN);
const FIXED_DIST_CODE = canonicalCodes(FIXED_DIST_LEN);

/** Package-merge length-limited Huffman code lengths. */
export function codeLengths(freq: ArrayLike<number>, maxBits: number): Uint8Array {
  const n = freq.length;
  const lens = new Uint8Array(n);
  const used: number[] = [];
  for (let i = 0; i < n; i++) if (freq[i]! > 0) used.push(i);
  if (used.length < 2) {
    const set = new Set(used);
    for (let i = 0; set.size < 2 && i < n; i++) set.add(i);
    for (const s of set) lens[s] = 1;
    return lens;
  }
  used.sort((a, b) => freq[a]! - freq[b]! || a - b);
  const k = used.length;
  const leafW = used.map((s) => freq[s]!);
  // Each level: weights, and for every item either a leaf index (>= 0) or a package (-1 - index of its first child).
  const levels: { w: number[]; item: number[] }[] = [];
  levels.push({ w: leafW.slice(), item: used.map((_, i) => i) });
  for (let l = 1; l < maxBits; l++) {
    const prev = levels[l - 1]!;
    const pw: number[] = [];
    for (let i = 0; i + 1 < prev.w.length; i += 2) pw.push(prev.w[i]! + prev.w[i + 1]!);
    const w: number[] = [];
    const item: number[] = [];
    let a = 0;
    let b = 0;
    while (a < k || b < pw.length) {
      if (b >= pw.length || (a < k && leafW[a]! <= pw[b]!)) {
        w.push(leafW[a]!);
        item.push(a++);
      } else {
        w.push(pw[b]!);
        item.push(-1 - 2 * b++);
      }
    }
    levels.push({ w, item });
  }
  // Select the first 2k-2 items of the last level and walk down.
  let take = 2 * k - 2;
  for (let l = maxBits - 1; l >= 0; l--) {
    const { item } = levels[l]!;
    let next = 0;
    for (let i = 0; i < take; i++) {
      const it = item[i]!;
      if (it >= 0) lens[used[it]!]!++;
      else next = Math.max(next, -1 - it + 2);
    }
    take = next; // packages are a prefix of the previous level's pairs
  }
  return lens;
}

/** Canonical codes, bit-reversed for LSB-first output. */
function canonicalCodes(lens: Uint8Array): Uint16Array {
  const count = new Uint16Array(16);
  for (const l of lens) count[l]!++;
  count[0] = 0;
  const next = new Uint16Array(16);
  let code = 0;
  for (let b = 1; b < 16; b++) {
    code = (code + count[b - 1]!) << 1;
    next[b] = code;
  }
  const codes = new Uint16Array(lens.length);
  for (let i = 0; i < lens.length; i++) {
    const l = lens[i]!;
    if (l === 0) continue;
    let c = next[l]!++;
    let r = 0;
    for (let j = 0; j < l; j++) {
      r = (r << 1) | (c & 1);
      c >>= 1;
    }
    codes[i] = r;
  }
  return codes;
}

class BitWriter {
  buf: Uint8Array;
  pos = 0;
  bits = 0;
  nbits = 0;
  constructor(size: number) {
    this.buf = new Uint8Array(Math.max(64, size));
  }
  ensure(extra: number): void {
    if (this.pos + extra <= this.buf.length) return;
    const b = new Uint8Array(Math.max(this.buf.length * 2, this.pos + extra));
    b.set(this.buf.subarray(0, this.pos));
    this.buf = b;
  }
  /** Write up to 16 bits; the caller ensures space (4 bytes per call is plenty). */
  put(value: number, n: number): void {
    this.bits |= value << this.nbits;
    this.nbits += n;
    while (this.nbits >= 8) {
      this.buf[this.pos++] = this.bits & 0xff;
      this.bits >>>= 8;
      this.nbits -= 8;
    }
  }
  alignByte(): void {
    if (this.nbits > 0) {
      this.buf[this.pos++] = this.bits & 0xff;
      this.bits = 0;
      this.nbits = 0;
    }
  }
  /** Total bits written so far. */
  get bitPos(): number {
    return this.pos * 8 + this.nbits;
  }
}

interface DynamicHeader {
  litLens: Uint8Array;
  distLens: Uint8Array;
  hlit: number;
  hdist: number;
  hclen: number;
  clLens: Uint8Array;
  /** RLE symbols of the code-length sequence; symbols 16..18 are followed by their extra value. */
  rle: number[];
  bits: number;
}

function dynamicHeader(litFreq: Uint32Array, distFreq: Uint32Array): DynamicHeader {
  const litLens = codeLengths(litFreq, 15);
  const distLens = codeLengths(distFreq, 15);
  let hlit = 286;
  while (hlit > 257 && litLens[hlit - 1] === 0) hlit--;
  let hdist = 30;
  while (hdist > 1 && distLens[hdist - 1] === 0) hdist--;
  const seq = new Uint8Array(hlit + hdist);
  seq.set(litLens.subarray(0, hlit), 0);
  seq.set(distLens.subarray(0, hdist), hlit);
  const rle: number[] = [];
  const clFreq = new Uint32Array(19);
  for (let i = 0; i < seq.length; ) {
    const v = seq[i]!;
    let r = 1;
    while (i + r < seq.length && seq[i + r] === v) r++;
    if (v === 0) {
      if (r >= 11) {
        const m = Math.min(r, 138);
        rle.push(18, m - 11);
        clFreq[18]!++;
        i += m;
      } else if (r >= 3) {
        rle.push(17, r - 3);
        clFreq[17]!++;
        i += r;
      } else {
        rle.push(0);
        clFreq[0]!++;
        i += 1;
      }
    } else {
      rle.push(v);
      clFreq[v]!++;
      i += 1;
      let rest = r - 1;
      while (rest >= 3) {
        const m = Math.min(rest, 6);
        rle.push(16, m - 3);
        clFreq[16]!++;
        i += m;
        rest -= m;
      }
    }
  }
  const clLens = codeLengths(clFreq, 7);
  let hclen = 19;
  while (hclen > 4 && clLens[CL_ORDER[hclen - 1]!] === 0) hclen--;
  let bits = 5 + 5 + 4 + 3 * hclen;
  for (let s = 0; s < 19; s++) bits += clFreq[s]! * clLens[s]!;
  bits += clFreq[16]! * 2 + clFreq[17]! * 3 + clFreq[18]! * 7;
  return { litLens, distLens, hlit, hdist, hclen, clLens, rle, bits };
}

class Encoder {
  readonly d: Uint8Array;
  readonly n: number;
  readonly head: Int32Array;
  /** prev[q & WMASK] is only read for positions q inserted in this run, so it needs no reset. */
  readonly prev: Int32Array;
  /** next position to insert */
  ins = 0;
  // Symbol buffer: sDist 0 = literal (sLen = byte), else match length / distance.
  readonly sLen: Uint16Array;
  readonly sDist: Uint16Array;
  count = 0;
  blockStart = 0;
  readonly litFreq = new Uint32Array(286);
  readonly distFreq = new Uint32Array(30);
  readonly out: BitWriter;
  matchLen = 0;
  matchDist = 0;

  constructor(d: Uint8Array, t: Tables) {
    this.d = d;
    this.n = d.length;
    this.head = t.head.fill(-1);
    this.prev = t.prev;
    this.sLen = t.sLen;
    this.sDist = t.sDist;
    this.out = new BitWriter((d.length >>> 1) + 1024);
  }

  insertUpTo(end: number): void {
    const d = this.d;
    const last = Math.min(end, this.n - 2);
    const head = this.head;
    const prev = this.prev;
    for (let p = this.ins; p < last; p++) {
      const h = Math.imul((d[p]! << 16) | (d[p + 1]! << 8) | d[p + 2]!, 0x9e3779b1) >>> 17;
      prev[p & WMASK] = head[h]!;
      head[h] = p;
    }
    if (end > this.ins) this.ins = end;
  }

  /**
   * Sets matchLen/matchDist for position p (all earlier positions inserted), then inserts p.
   * Only matches longer than `atLeast` are reported (the lazy step needs nothing shorter);
   * this gives the same result as a full search followed by that comparison.
   */
  findMatch(p: number, atLeast: number, maxChain: number): void {
    const d = this.d;
    const n = this.n;
    this.matchLen = 0;
    this.matchDist = 0;
    if (p + MIN_MATCH > n) return;
    const h = Math.imul((d[p]! << 16) | (d[p + 1]! << 8) | d[p + 2]!, 0x9e3779b1) >>> 17;
    const head = this.head;
    const prev = this.prev;
    const maxLen = Math.min(MAX_MATCH, n - p);
    if (atLeast >= maxLen) {
      this.insertUpTo(p + 1);
      return;
    }
    let c = head[h]!;
    let chain = maxChain;
    let best = atLeast;
    let bestDist = 0;
    const limit = p > WSIZE ? p - WSIZE : 0; // -1 (empty) is below it too
    let scanEnd = d[p + best]!;
    while (c >= limit && chain-- > 0) {
      // A candidate can only be longer than `best` if it matches at offset `best`.
      if (d[c + best] === scanEnd) {
        let len = 0;
        while (len < maxLen && d[c + len] === d[p + len]) len++;
        if (len > best) {
          best = len;
          bestDist = p - c;
          if (len >= maxLen) break;
          scanEnd = d[p + best]!;
        }
      }
      const nx = prev[c & WMASK]!;
      if (nx >= c) break;
      c = nx;
    }
    prev[p & WMASK] = head[h]!;
    head[h] = p;
    this.ins = p + 1;
    if (best === MIN_MATCH && bestDist > TOO_FAR) best = 0;
    if (best >= MIN_MATCH && bestDist > 0) {
      this.matchLen = best;
      this.matchDist = bestDist;
    }
  }

  literal(p: number): void {
    const b = this.d[p]!;
    this.sLen[this.count] = b;
    this.sDist[this.count++] = 0;
    this.litFreq[b]!++;
  }

  match(len: number, dist: number): void {
    this.sLen[this.count] = len;
    this.sDist[this.count++] = dist;
    this.litFreq[257 + LEN_CODE[len]!]!++;
    this.distFreq[DIST_CODE[dist - 1]!]!++;
  }

  run(): Uint8Array {
    const n = this.n;
    let p = 0;
    while (p < n) {
      if (this.count >= BLOCK_SYMBOLS) this.flush(p, false);
      this.findMatch(p, 0, MAX_CHAIN);
      let len = this.matchLen;
      let dist = this.matchDist;
      if (len < MIN_MATCH) {
        this.literal(p);
        p++;
        continue;
      }
      while (len < LAZY_LIMIT && p + 1 < n) {
        this.findMatch(p + 1, len, len >= GOOD_LENGTH ? MAX_CHAIN >> 2 : MAX_CHAIN);
        if (this.matchLen > len) {
          this.literal(p);
          p++;
          len = this.matchLen;
          dist = this.matchDist;
        } else break;
      }
      this.match(len, dist);
      this.insertUpTo(p + len);
      p += len;
    }
    this.flush(n, true);
    this.out.alignByte();
    return this.out.buf.subarray(0, this.out.pos);
  }

  /** Write the buffered symbols, which cover input [blockStart, end). */
  flush(end: number, final: boolean): void {
    const out = this.out;
    const span = end - this.blockStart;
    this.litFreq[256] = 1;
    const lf = this.litFreq;
    const df = this.distFreq;
    // Data bits common to every Huffman variant: extra bits.
    let extra = 0;
    for (let c = 0; c < 29; c++) extra += lf[257 + c]! * LENGTH_EXTRA[c]!;
    for (let c = 0; c < 30; c++) extra += df[c]! * DIST_EXTRA[c]!;
    let fixedBits = 3 + extra;
    for (let s = 0; s < 286; s++) fixedBits += lf[s]! * FIXED_LIT_LEN[s]!;
    for (let s = 0; s < 30; s++) fixedBits += df[s]! * 5;
    const dyn = dynamicHeader(lf, df);
    let dynBits = 3 + dyn.bits + extra;
    for (let s = 0; s < 286; s++) dynBits += lf[s]! * dyn.litLens[s]!;
    for (let s = 0; s < 30; s++) dynBits += df[s]! * dyn.distLens[s]!;
    let storedBits = Infinity;
    if (span <= STORED_MAX) storedBits = 3 + ((8 - ((out.bitPos + 3) % 8)) % 8) + 32 + 8 * span;

    out.ensure(Math.ceil(Math.min(storedBits, fixedBits, dynBits) / 8) + 16);
    const bfinal = final ? 1 : 0;
    if (storedBits <= fixedBits && storedBits <= dynBits) {
      out.put(bfinal, 3); // BTYPE 00
      out.alignByte();
      out.put(span & 0xff, 8);
      out.put(span >>> 8, 8);
      out.put(~span & 0xff, 8);
      out.put((~span >>> 8) & 0xff, 8);
      out.buf.set(this.d.subarray(this.blockStart, end), out.pos);
      out.pos += span;
    } else if (fixedBits <= dynBits) {
      out.put(bfinal | (1 << 1), 3);
      this.writeSymbols(FIXED_LIT_CODE, FIXED_LIT_LEN, FIXED_DIST_CODE, FIXED_DIST_LEN);
    } else {
      out.put(bfinal | (2 << 1), 3);
      out.put(dyn.hlit - 257, 5);
      out.put(dyn.hdist - 1, 5);
      out.put(dyn.hclen - 4, 4);
      for (let i = 0; i < dyn.hclen; i++) out.put(dyn.clLens[CL_ORDER[i]!]!, 3);
      const clCodes = canonicalCodes(dyn.clLens);
      const rle = dyn.rle;
      for (let i = 0; i < rle.length; i++) {
        const s = rle[i]!;
        out.put(clCodes[s]!, dyn.clLens[s]!);
        if (s === 16) out.put(rle[++i]!, 2);
        else if (s === 17) out.put(rle[++i]!, 3);
        else if (s === 18) out.put(rle[++i]!, 7);
      }
      this.writeSymbols(canonicalCodes(dyn.litLens), dyn.litLens, canonicalCodes(dyn.distLens), dyn.distLens);
    }
    this.count = 0;
    this.blockStart = end;
    lf.fill(0);
    df.fill(0);
  }

  writeSymbols(litCode: Uint16Array, litLen: Uint8Array, distCode: Uint16Array, distLen: Uint8Array): void {
    const out = this.out;
    const sLen = this.sLen;
    const sDist = this.sDist;
    // Inlined bit writer for speed.
    const buf = out.buf;
    let pos = out.pos;
    let bits = out.bits;
    let nbits = out.nbits;
    for (let i = 0; i < this.count; i++) {
      const dist = sDist[i]!;
      if (dist === 0) {
        const b = sLen[i]!;
        bits |= litCode[b]! << nbits;
        nbits += litLen[b]!;
      } else {
        const len = sLen[i]!;
        const lc = LEN_CODE[len]!;
        bits |= litCode[257 + lc]! << nbits;
        nbits += litLen[257 + lc]!;
        const le = LENGTH_EXTRA[lc]!;
        if (le) {
          bits |= (len - LENGTH_BASE[lc]!) << nbits;
          nbits += le;
        }
        while (nbits >= 8) {
          buf[pos++] = bits & 0xff;
          bits >>>= 8;
          nbits -= 8;
        }
        const dc = DIST_CODE[dist - 1]!;
        bits |= distCode[dc]! << nbits;
        nbits += distLen[dc]!;
        const de = DIST_EXTRA[dc]!;
        if (de) {
          while (nbits >= 8) {
            buf[pos++] = bits & 0xff;
            bits >>>= 8;
            nbits -= 8;
          }
          bits |= (dist - DIST_BASE[dc]!) << nbits;
          nbits += de;
        }
      }
      while (nbits >= 8) {
        buf[pos++] = bits & 0xff;
        bits >>>= 8;
        nbits -= 8;
      }
    }
    out.pos = pos;
    out.bits = bits;
    out.nbits = nbits;
    out.put(litCode[256]!, litLen[256]!);
  }
}

interface Tables {
  head: Int32Array;
  prev: Int32Array;
  sLen: Uint16Array;
  sDist: Uint16Array;
}
/** Reused between calls (encoding is synchronous), so small zip entries do not each allocate ~200 kB. */
let spare: Tables | undefined;

/** Raw DEFLATE stream (zip method 8). */
export function deflateRaw(data: Uint8Array): Uint8Array {
  const t = spare ?? {
    head: new Int32Array(1 << HASH_BITS),
    prev: new Int32Array(WSIZE),
    sLen: new Uint16Array(BLOCK_SYMBOLS + MAX_MATCH + 8),
    sDist: new Uint16Array(BLOCK_SYMBOLS + MAX_MATCH + 8),
  };
  spare = undefined;
  try {
    const out = new Encoder(data, t).run();
    // The output buffer was sized for the worst case; copy small results so they do not pin it.
    return out.length < out.buffer.byteLength >>> 1 ? out.slice() : out;
  } finally {
    spare = t;
  }
}

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

/** gzip member with a fixed header (no mtime, no name, XFL 0, OS 3). */
export function gzip(data: Uint8Array): Uint8Array {
  const body = deflateRaw(data);
  const out = new Uint8Array(10 + body.length + 8);
  out.set([0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3], 0);
  out.set(body, 10);
  const v = new DataView(out.buffer, out.byteOffset, out.byteLength);
  v.setUint32(10 + body.length, crc32(data), true);
  v.setUint32(14 + body.length, data.length >>> 0, true);
  return out;
}
