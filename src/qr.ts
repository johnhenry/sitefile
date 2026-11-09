/**
 * A QR Code encoder (ISO/IEC 18004), enough for `serve --qr` (issue #224): byte mode, error correction
 * level M, the smallest version (1–40) that fits, and the mask with the lowest penalty. Ported as
 * rust/src/qr.rs; both produce the same modules (test/fixtures/qr/*.txt are checked by both).
 */

// Error correction level M: codewords per block and number of blocks, by version (index 0 unused).
const ECC_PER_BLOCK = [-1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28];
const NUM_BLOCKS = [-1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21, 23, 25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49];
/** Format bits for level M are 00. */
const ECL_BITS = 0;

/** Raw data modules (bits) available in a version, after function patterns. */
function rawDataModules(ver: number): number {
  let result = (16 * ver + 128) * ver + 64;
  if (ver >= 2) {
    const numAlign = Math.floor(ver / 7) + 2;
    result -= (25 * numAlign - 10) * numAlign - 55;
    if (ver >= 7) result -= 36;
  }
  return result;
}

function dataCodewords(ver: number): number {
  return Math.floor(rawDataModules(ver) / 8) - (ECC_PER_BLOCK[ver] as number) * (NUM_BLOCKS[ver] as number);
}

// ---------------------------------------------------------------- Reed-Solomon over GF(2^8/0x11D)

function gfMul(x: number, y: number): number {
  let z = 0;
  for (let i = 7; i >= 0; i--) {
    z = (z << 1) ^ ((z >>> 7) * 0x11d);
    z ^= ((y >>> i) & 1) * x;
  }
  return z & 0xff;
}

function rsDivisor(degree: number): number[] {
  const result = new Array<number>(degree).fill(0);
  result[degree - 1] = 1;
  let root = 1;
  for (let i = 0; i < degree; i++) {
    for (let j = 0; j < degree; j++) {
      result[j] = gfMul(result[j] as number, root);
      if (j + 1 < degree) result[j] = (result[j] as number) ^ (result[j + 1] as number);
    }
    root = gfMul(root, 0x02);
  }
  return result;
}

function rsRemainder(data: readonly number[], divisor: readonly number[]): number[] {
  const result = divisor.map(() => 0);
  for (const b of data) {
    const factor = b ^ (result.shift() as number);
    result.push(0);
    divisor.forEach((coef, i) => (result[i] = (result[i] as number) ^ gfMul(coef, factor)));
  }
  return result;
}

// ---------------------------------------------------------------- the symbol

/** A QR Code: `size` × `size` modules, `true` = dark. */
export interface QrCode {
  readonly version: number;
  readonly size: number;
  readonly modules: readonly (readonly boolean[])[];
}

class Builder {
  readonly size: number;
  readonly modules: boolean[][];
  readonly isFunction: boolean[][];

  constructor(readonly version: number) {
    this.size = version * 4 + 17;
    this.modules = Array.from({ length: this.size }, () => new Array<boolean>(this.size).fill(false));
    this.isFunction = Array.from({ length: this.size }, () => new Array<boolean>(this.size).fill(false));
  }

  set(x: number, y: number, dark: boolean): void {
    (this.modules[y] as boolean[])[x] = dark;
    (this.isFunction[y] as boolean[])[x] = true;
  }

  alignmentPositions(): number[] {
    if (this.version === 1) return [];
    const numAlign = Math.floor(this.version / 7) + 2;
    const step = this.version === 32 ? 26 : Math.ceil((this.version * 4 + 4) / (numAlign * 2 - 2)) * 2;
    const result = [6];
    for (let pos = this.size - 7; result.length < numAlign; pos -= step) result.splice(1, 0, pos);
    return result;
  }

  drawFunctionPatterns(): void {
    for (let i = 0; i < this.size; i++) {
      this.set(6, i, i % 2 === 0);
      this.set(i, 6, i % 2 === 0);
    }
    this.finder(3, 3);
    this.finder(this.size - 4, 3);
    this.finder(3, this.size - 4);
    const align = this.alignmentPositions();
    const n = align.length;
    for (let i = 0; i < n; i++) {
      for (let j = 0; j < n; j++) {
        if (!((i === 0 && j === 0) || (i === 0 && j === n - 1) || (i === n - 1 && j === 0))) this.alignment(align[i] as number, align[j] as number);
      }
    }
    this.formatBits(0); // placeholder, overwritten after masking
    this.versionBits();
  }

  finder(x: number, y: number): void {
    for (let dy = -4; dy <= 4; dy++) {
      for (let dx = -4; dx <= 4; dx++) {
        const dist = Math.max(Math.abs(dx), Math.abs(dy));
        const xx = x + dx;
        const yy = y + dy;
        if (xx >= 0 && xx < this.size && yy >= 0 && yy < this.size) this.set(xx, yy, dist !== 2 && dist !== 4);
      }
    }
  }

  alignment(x: number, y: number): void {
    for (let dy = -2; dy <= 2; dy++) for (let dx = -2; dx <= 2; dx++) this.set(x + dx, y + dy, Math.max(Math.abs(dx), Math.abs(dy)) !== 1);
  }

  formatBits(mask: number): void {
    const data = (ECL_BITS << 3) | mask;
    let rem = data;
    for (let i = 0; i < 10; i++) rem = (rem << 1) ^ ((rem >>> 9) * 0x537);
    const bits = ((data << 10) | rem) ^ 0x5412;
    const bit = (i: number): boolean => ((bits >>> i) & 1) !== 0;
    for (let i = 0; i <= 5; i++) this.set(8, i, bit(i));
    this.set(8, 7, bit(6));
    this.set(8, 8, bit(7));
    this.set(7, 8, bit(8));
    for (let i = 9; i < 15; i++) this.set(14 - i, 8, bit(i));
    for (let i = 0; i < 8; i++) this.set(this.size - 1 - i, 8, bit(i));
    for (let i = 8; i < 15; i++) this.set(8, this.size - 15 + i, bit(i));
    this.set(8, this.size - 8, true);
  }

  versionBits(): void {
    if (this.version < 7) return;
    let rem = this.version;
    for (let i = 0; i < 12; i++) rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
    const bits = (this.version << 12) | rem;
    for (let i = 0; i < 18; i++) {
      const dark = ((bits >>> i) & 1) !== 0;
      const a = this.size - 11 + (i % 3);
      const b = Math.floor(i / 3);
      this.set(a, b, dark);
      this.set(b, a, dark);
    }
  }

  drawCodewords(data: readonly number[]): void {
    let i = 0;
    for (let right = this.size - 1; right >= 1; right -= 2) {
      if (right === 6) right = 5;
      for (let vert = 0; vert < this.size; vert++) {
        for (let j = 0; j < 2; j++) {
          const x = right - j;
          const upward = ((right + 1) & 2) === 0;
          const y = upward ? this.size - 1 - vert : vert;
          if (!(this.isFunction[y] as boolean[])[x] && i < data.length * 8) {
            (this.modules[y] as boolean[])[x] = (((data[i >>> 3] as number) >>> (7 - (i & 7))) & 1) !== 0;
            i++;
          }
        }
      }
    }
  }

  applyMask(mask: number): void {
    for (let y = 0; y < this.size; y++) {
      for (let x = 0; x < this.size; x++) {
        let invert: boolean;
        switch (mask) {
          case 0: invert = (x + y) % 2 === 0; break;
          case 1: invert = y % 2 === 0; break;
          case 2: invert = x % 3 === 0; break;
          case 3: invert = (x + y) % 3 === 0; break;
          case 4: invert = (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0; break;
          case 5: invert = ((x * y) % 2) + ((x * y) % 3) === 0; break;
          case 6: invert = (((x * y) % 2) + ((x * y) % 3)) % 2 === 0; break;
          default: invert = (((x + y) % 2) + ((x * y) % 3)) % 2 === 0;
        }
        const row = this.modules[y] as boolean[];
        if (!(this.isFunction[y] as boolean[])[x] && invert) row[x] = !row[x];
      }
    }
  }

  penalty(): number {
    const n = this.size;
    const m = this.modules;
    const at = (x: number, y: number): boolean => (m[y] as boolean[])[x] as boolean;
    let result = 0;
    // Runs of five or more same-colored modules, and finder-like patterns, in rows and columns.
    for (const horizontal of [true, false]) {
      for (let a = 0; a < n; a++) {
        let runColor = false;
        let runLen = 0;
        const history = [0, 0, 0, 0, 0, 0, 0];
        for (let b = 0; b < n; b++) {
          const c = horizontal ? at(b, a) : at(a, b);
          if (c === runColor) {
            runLen++;
            if (runLen === 5) result += 3;
            else if (runLen > 5) result++;
          } else {
            finderPenaltyAddHistory(runLen, history, n);
            if (!runColor) result += finderPenaltyCountPatterns(history, n) * 40;
            runColor = c;
            runLen = 1;
          }
        }
        result += finderPenaltyTerminateAndCount(runColor, runLen, history, n) * 40;
      }
    }
    // 2×2 blocks of one color.
    for (let y = 0; y < n - 1; y++) {
      for (let x = 0; x < n - 1; x++) {
        const c = at(x, y);
        if (c === at(x + 1, y) && c === at(x, y + 1) && c === at(x + 1, y + 1)) result += 3;
      }
    }
    // Balance of dark modules.
    let dark = 0;
    for (const row of m) for (const c of row) if (c) dark++;
    const total = n * n;
    const k = Math.ceil(Math.abs(dark * 20 - total * 10) / total) - 1;
    result += k * 10;
    return result;
  }
}

function finderPenaltyCountPatterns(h: readonly number[], size: number): number {
  void size;
  const n = h[1] as number;
  const core = n > 0 && h[2] === n && h[3] === n * 3 && h[4] === n && h[5] === n;
  return (core && (h[0] as number) >= n * 4 && (h[6] as number) >= n ? 1 : 0) + (core && (h[6] as number) >= n * 4 && (h[0] as number) >= n ? 1 : 0);
}

function finderPenaltyTerminateAndCount(runColor: boolean, runLen: number, history: number[], size: number): number {
  let len = runLen;
  if (runColor) {
    finderPenaltyAddHistory(len, history, size);
    len = 0;
  }
  len += size;
  finderPenaltyAddHistory(len, history, size);
  return finderPenaltyCountPatterns(history, size);
}

function finderPenaltyAddHistory(runLen: number, history: number[], size: number): void {
  let len = runLen;
  if (history[0] === 0) len += size;
  history.pop();
  history.unshift(len);
}

/** Encode `text` (UTF-8) as a QR Code, or undefined when it is too long for version 40. */
export function encodeQr(text: string): QrCode | undefined {
  const bytes = [...new TextEncoder().encode(text)];
  let version = 1;
  for (; version <= 40; version++) {
    const countBits = version < 10 ? 8 : 16;
    if (4 + countBits + bytes.length * 8 <= dataCodewords(version) * 8) break;
  }
  if (version > 40) return undefined;
  // Bit stream: mode 0100 (byte), count, data, terminator, padding.
  const bits: number[] = [];
  const append = (val: number, len: number): void => {
    for (let i = len - 1; i >= 0; i--) bits.push((val >>> i) & 1);
  };
  append(4, 4);
  append(bytes.length, version < 10 ? 8 : 16);
  for (const b of bytes) append(b, 8);
  const capacity = dataCodewords(version) * 8;
  append(0, Math.min(4, capacity - bits.length));
  append(0, (8 - (bits.length % 8)) % 8);
  for (let pad = 0xec; bits.length < capacity; pad ^= 0xec ^ 0x11) append(pad, 8);
  const data: number[] = [];
  for (let i = 0; i < bits.length; i += 8) data.push(bits.slice(i, i + 8).reduce((a, b) => (a << 1) | b, 0));
  // Split into blocks, add error correction, interleave.
  const numBlocks = NUM_BLOCKS[version] as number;
  const blockEcc = ECC_PER_BLOCK[version] as number;
  const rawCodewords = Math.floor(rawDataModules(version) / 8);
  const numShort = numBlocks - (rawCodewords % numBlocks);
  const shortLen = Math.floor(rawCodewords / numBlocks);
  const divisor = rsDivisor(blockEcc);
  const blocks: number[][] = [];
  for (let i = 0, k = 0; i < numBlocks; i++) {
    const dat = data.slice(k, k + shortLen - blockEcc + (i < numShort ? 0 : 1));
    k += dat.length;
    const ecc = rsRemainder(dat, divisor);
    if (i < numShort) dat.push(0);
    blocks.push([...dat, ...ecc]);
  }
  const codewords: number[] = [];
  for (let i = 0; i < (blocks[0] as number[]).length; i++) {
    blocks.forEach((block, j) => {
      if (i !== shortLen - blockEcc || j >= numShort) codewords.push(block[i] as number);
    });
  }
  // Draw, then keep the mask with the lowest penalty (the first one on a tie).
  let best: Builder | undefined;
  let bestPenalty = Infinity;
  for (let mask = 0; mask < 8; mask++) {
    const b = new Builder(version);
    b.drawFunctionPatterns();
    b.drawCodewords(codewords);
    b.applyMask(mask);
    b.formatBits(mask);
    const p = b.penalty();
    if (p < bestPenalty) {
      best = b;
      bestPenalty = p;
    }
  }
  const b = best as Builder;
  return { version, size: b.size, modules: b.modules };
}

/**
 * The code for a terminal: two rows of modules per line with `▀`, `▄`, `█` and spaces, and a quiet zone
 * of two modules, dark on a light background (the half blocks are the light parts' complement).
 */
export function qrTerminal(code: QrCode): string {
  const quiet = 2;
  const n = code.size + quiet * 2;
  const dark = (x: number, y: number): boolean => {
    const xx = x - quiet;
    const yy = y - quiet;
    return xx >= 0 && yy >= 0 && xx < code.size && yy < code.size && ((code.modules[yy] as boolean[])[xx] as boolean);
  };
  let out = '';
  for (let y = 0; y < n; y += 2) {
    let line = '';
    for (let x = 0; x < n; x++) {
      const top = !dark(x, y);
      const bottom = y + 1 < n ? !dark(x, y + 1) : false;
      line += top && bottom ? '█' : top ? '▀' : bottom ? '▄' : ' ';
    }
    out += `${line}\n`;
  }
  return out;
}
