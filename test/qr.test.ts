// The QR encoder behind `serve --qr` (issue #224).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { encodeQr, qrTerminal, type QrCode } from '../src/qr.js';
import { repoRoot } from './helpers/cli.js';

const FIXTURES = path.join(repoRoot, 'test', 'fixtures', 'qr');

// ISO/IEC 18004 Table C.1, level M, masks 0..7.
const FORMAT_M = ['101010000010010', '101000100100101', '101111001111100', '101101101001011', '100010111111001', '100000011001110', '100111110010111', '100101010100000'];

/** The 15 format bits as the standard places them next to the top-left finder (bit 14 first). */
function formatBits(q: QrCode): string {
  const m = (x: number, y: number): string => ((q.modules[y] as boolean[])[x] ? '1' : '0');
  const lowToHigh = [m(8, 0), m(8, 1), m(8, 2), m(8, 3), m(8, 4), m(8, 5), m(8, 7), m(8, 8), m(7, 8), m(5, 8), m(4, 8), m(3, 8), m(2, 8), m(1, 8), m(0, 8)];
  return lowToHigh.reverse().join('');
}

// GF(256) with the QR polynomial, for an independent syndrome check.
const EXP: number[] = [];
const LOG: number[] = new Array<number>(256).fill(0);
for (let i = 0, x = 1; i < 255; i++) {
  EXP[i] = x;
  LOG[x] = i;
  x <<= 1;
  if (x & 0x100) x ^= 0x11d;
}
const mul = (a: number, b: number): number => (a === 0 || b === 0 ? 0 : (EXP[((LOG[a] as number) + (LOG[b] as number)) % 255] as number));

/** Read the data back: unmask, read the zigzag, de-interleave, check every block's syndromes, parse byte mode. */
function decode(q: QrCode, mask: number, eccPerBlock: number, numBlocks: number): string {
  const n = q.size;
  // Function modules, as the standard defines them.
  const fn = Array.from({ length: n }, () => new Array<boolean>(n).fill(false));
  const mark = (x0: number, y0: number, w: number, h: number) => {
    for (let y = y0; y < y0 + h; y++) for (let x = x0; x < x0 + w; x++) if (x >= 0 && y >= 0 && x < n && y < n) (fn[y] as boolean[])[x] = true;
  };
  mark(0, 0, 9, 9);
  mark(n - 8, 0, 8, 9);
  mark(0, n - 8, 9, 8);
  mark(6, 0, 1, n);
  mark(0, 6, n, 1);
  const v = (n - 17) / 4;
  if (v >= 2) {
    const numAlign = Math.floor(v / 7) + 2;
    const step = v === 32 ? 26 : Math.ceil((v * 4 + 4) / (numAlign * 2 - 2)) * 2;
    const pos = [6];
    for (let p = n - 7; pos.length < numAlign; p -= step) pos.splice(1, 0, p);
    for (const a of pos) for (const b of pos) if (!((a === 6 && b === 6) || (a === 6 && b === n - 7) || (a === n - 7 && b === 6))) mark(a - 2, b - 2, 5, 5);
  }
  if (v >= 7) {
    mark(n - 11, 0, 3, 6);
    mark(0, n - 11, 6, 3);
  }
  const masked = (x: number, y: number): boolean => {
    const inv = [(x + y) % 2 === 0, y % 2 === 0, x % 3 === 0, (x + y) % 3 === 0, (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0, ((x * y) % 2) + ((x * y) % 3) === 0, (((x * y) % 2) + ((x * y) % 3)) % 2 === 0, (((x + y) % 2) + ((x * y) % 3)) % 2 === 0][mask];
    return ((q.modules[y] as boolean[])[x] as boolean) !== inv;
  };
  const bits: number[] = [];
  for (let right = n - 1; right >= 1; right -= 2) {
    if (right === 6) right = 5;
    for (let vert = 0; vert < n; vert++) {
      for (let j = 0; j < 2; j++) {
        const x = right - j;
        const y = ((right + 1) & 2) === 0 ? n - 1 - vert : vert;
        if (!(fn[y] as boolean[])[x]) bits.push(masked(x, y) ? 1 : 0);
      }
    }
  }
  const total = Math.floor(bits.length / 8);
  const cw: number[] = [];
  for (let i = 0; i < total * 8; i += 8) cw.push(bits.slice(i, i + 8).reduce((a, b) => (a << 1) | b, 0));
  const shortLen = Math.floor(total / numBlocks);
  const numShort = numBlocks - (total % numBlocks);
  const blocks: number[][] = Array.from({ length: numBlocks }, () => []);
  let k = 0;
  for (let i = 0; i < shortLen - eccPerBlock + 1; i++) {
    for (let b = 0; b < numBlocks; b++) if (i < shortLen - eccPerBlock || b >= numShort) (blocks[b] as number[]).push(cw[k++] as number);
  }
  for (let i = 0; i < eccPerBlock; i++) for (let b = 0; b < numBlocks; b++) (blocks[b] as number[]).push(cw[k++] as number);
  const data: number[] = [];
  for (const block of blocks) {
    // A codeword polynomial is divisible by the generator: it vanishes at α^0 … α^(ecc-1).
    for (let r = 0; r < eccPerBlock; r++) {
      let s = 0;
      for (const c of block) s = mul(s, EXP[r] as number) ^ c;
      assert.equal(s, 0, `syndrome ${r}`);
    }
    data.push(...block.slice(0, block.length - eccPerBlock));
  }
  const dbits = data.flatMap((b) => [7, 6, 5, 4, 3, 2, 1, 0].map((i) => (b >> i) & 1));
  const read = (from: number, len: number): number => dbits.slice(from, from + len).reduce((a, b) => (a << 1) | b, 0);
  assert.equal(read(0, 4), 4, 'byte mode');
  const countBits = v < 10 ? 8 : 16;
  const len = read(4, countBits);
  const out: number[] = [];
  for (let i = 0; i < len; i++) out.push(read(4 + countBits + i * 8, 8));
  return Buffer.from(out).toString('utf8');
}

const ECC = [-1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26];
const BLOCKS = [-1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5];

test('the format bits are the standard ones, and the data decodes back with valid error correction', () => {
  for (const text of ['https://example.com/', 'http://192.168.1.20:8080/', 'x', `https://example.com/${'p/'.repeat(48)}`, 'héllo ✓']) {
    const q = encodeQr(text);
    assert.ok(q);
    assert.equal(q.size, q.version * 4 + 17);
    const f = formatBits(q);
    const mask = FORMAT_M.indexOf(f);
    assert.ok(mask >= 0, `format ${f} for ${text}`);
    assert.equal(decode(q, mask, ECC[q.version] as number, BLOCKS[q.version] as number), text);
  }
  assert.equal(encodeQr('https://example.com/')?.version, 2);
  assert.equal(encodeQr(`https://example.com/${'p/'.repeat(48)}`)?.version, 7);
  assert.equal(encodeQr('x'.repeat(3000)), undefined);
});

test('version 7+ symbols carry the standard version information', () => {
  const q = encodeQr(`https://example.com/${'p/'.repeat(48)}`) as QrCode;
  const n = q.size;
  let bits = '';
  for (let i = 17; i >= 0; i--) bits += (q.modules[Math.floor(i / 3)] as boolean[])[n - 11 + (i % 3)] ? '1' : '0';
  assert.equal(bits, '000111110010010100'); // Table D.1, version 7
});

test('the terminal rendering matches the fixtures Rust checks too', async () => {
  const files = (await fs.readdir(FIXTURES)).filter((f) => f.endsWith('.txt'));
  assert.ok(files.length >= 3);
  for (const f of files) {
    const text = await fs.readFile(path.join(FIXTURES, f), 'utf8');
    const nl = text.indexOf('\n');
    const input = text.slice(0, nl).replace(/^# /, '');
    assert.equal(qrTerminal(encodeQr(input) as QrCode), text.slice(nl + 1), f);
  }
});
