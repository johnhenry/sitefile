import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync, inflateRawSync, deflateRawSync } from 'node:zlib';
import { codeLengths, crc32, deflateRaw, gzip } from '../src/deflate.js';

/** mulberry32 bytes, the same sequence as rust/tests/fixtures/deflate/generate.mjs and deflate.rs. */
function random(n: number, seed: number): Uint8Array {
  const out = new Uint8Array(n);
  let s = seed | 0;
  for (let i = 0; i < n; i++) {
    s = (s + 0x6d2b79f5) | 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    out[i] = ((t ^ (t >>> 14)) >>> 0) >>> 24;
  }
  return out;
}

const enc = new TextEncoder();
const text = 'The quick brown fox jumps over the lazy dog. <p class="post">Lorem ipsum dolor sit amet</p>\n';

function inputs(): Array<[string, Uint8Array]> {
  const big = new Uint8Array(text.length * 15000 + 200000);
  big.set(enc.encode(text.repeat(15000)));
  big.set(random(200000, 4), text.length * 15000);
  const list: Array<[string, Uint8Array]> = [
    ['empty', new Uint8Array(0)],
    ['1 byte', Uint8Array.of(0)],
    ['1 byte ff', Uint8Array.of(255)],
    ['2 bytes', enc.encode('ab')],
    ['3 bytes', enc.encode('abc')],
    ['zeros', new Uint8Array(100000)],
    ['run of 775', new Uint8Array(775).fill(7)],
    ['random 1000', random(1000, 2)],
    ['random 70000', random(70000, 3)],
    ['text', enc.encode(text.repeat(2000))],
    ['mod 251', Uint8Array.from({ length: 200000 }, (_, i) => i % 251)],
    ['slow mod 3', Uint8Array.from({ length: 300000 }, (_, i) => 97 + (Math.floor(i / 7) % 3))],
    ['>1 MB mixed', big],
  ];
  for (const n of [16383, 16384, 16385, 65535, 65536, 32769]) list.push([`random ${n}`, random(n, n)]);
  return list;
}

test('deflateRaw and gzip round-trip through node:zlib', () => {
  for (const [name, input] of inputs()) {
    assert.deepEqual(new Uint8Array(inflateRawSync(deflateRaw(input))), input, name);
    const gz = gzip(input);
    assert.deepEqual([...gz.subarray(0, 10)], [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3], `${name}: fixed header`);
    assert.deepEqual(new Uint8Array(gunzipSync(gz)), input, name);
  }
});

test('output is deterministic and close to zlib level 9', () => {
  const t = enc.encode(text.repeat(3000));
  assert.deepEqual(deflateRaw(t), deflateRaw(t.slice()));
  const ours = deflateRaw(t).length;
  const z9 = deflateRawSync(t, { level: 9 }).length;
  assert.ok(ours <= z9 * 1.15, `${ours} vs zlib-9 ${z9}`);
});

test('empty input is one fixed block; incompressible input is stored', () => {
  assert.deepEqual([...deflateRaw(new Uint8Array(0))], [0x03, 0x00]);
  assert.equal(deflateRaw(random(10000, 9)).length, 10000 + 5);
});

test('package-merge code lengths are limited and complete', () => {
  const fib = [1, 1];
  while (fib.length < 30) fib.push(fib.at(-1)! + fib.at(-2)!);
  const l = codeLengths(fib, 15);
  assert.ok([...l].every((x) => x >= 1 && x <= 15));
  assert.equal([...l].reduce((s, x) => s + 2 ** -x, 0), 1);
  assert.deepEqual([...codeLengths([0, 0, 0], 7)], [1, 1, 0]);
  assert.deepEqual([...codeLengths([0, 0, 5], 7)], [1, 0, 1]);
});

test('crc32 check value', () => {
  assert.equal(crc32(enc.encode('123456789')), 0xcbf43926);
});

test('the checked-in Rust differential fixtures match this encoder', () => {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
  const fx = JSON.parse(readFileSync(path.join(root, 'rust/tests/fixtures/deflate/fixtures.json'), 'utf8')) as {
    cases: Array<{ name: string; length: number; rawLength: number; raw: string; gzip: string; input: unknown[] }>;
  };
  assert.ok(fx.cases.length >= 20);
  // Recipes are rebuilt by the generator; here only the self-contained ones are checked (the rest is covered by regenerating).
  const sha = (b: Uint8Array) => 'sha256:' + createHash('sha256').update(b).digest('hex');
  let checked = 0;
  for (const c of fx.cases) {
    const parts = c.input as Array<[string, ...unknown[]]>;
    if (!parts.every((p) => p[0] === 'random' || p[0] === 'fill')) continue;
    const bufs = parts.map((p) => (p[0] === 'random' ? random(p[1] as number, p[2] as number) : new Uint8Array(p[2] as number).fill(p[1] as number)));
    const input = new Uint8Array(bufs.reduce((n, b) => n + b.length, 0));
    let o = 0;
    for (const b of bufs) {
      input.set(b, o);
      o += b.length;
    }
    assert.equal(sha(deflateRaw(input)), c.raw, c.name);
    assert.equal(sha(gzip(input)), c.gzip, c.name);
    checked++;
  }
  assert.ok(checked >= 8, `${checked} fixtures checked`);
});
