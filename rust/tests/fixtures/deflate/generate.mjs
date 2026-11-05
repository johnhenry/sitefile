// Regenerates the deflate/gzip differential fixtures from the TypeScript
// encoder (src/deflate.ts).
//
//   npm ci && npm run build        # at the repository root (builds lib/)
//   node rust/tests/fixtures/deflate/generate.mjs
//
// Each case describes its input as a recipe (so the file stays small) plus the
// sha256 of the input, of the raw deflate stream and of the gzip member.
// Checked byte for byte by `deflate::tests::identical_to_typescript_encoder`,
// which rebuilds the inputs with the same recipes. Every input also has to
// round-trip through node:zlib here.
import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const { deflateRaw, gzip } = await import(path.join(root, 'lib/src/deflate.js'));

/** mulberry32 bytes (top 8 bits of each output); mirrored by `random` in deflate.rs. */
function random(n, seed) {
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

const VOCAB = ['the', 'site', 'file', 'build', 'layer', '<div>', '</div>', 'image', 'static', 'deflate', 'class="x"', 'a', 'of', 'and', 'to', '{}'];
function words(n, seed) {
  const r = random(n * 2, seed);
  let s = '';
  for (let i = 0; i < n; i++) s += VOCAB[r[2 * i] & 15] + (r[2 * i + 1] < 32 ? '\n' : ' ');
  return new TextEncoder().encode(s);
}

function build(recipe) {
  const parts = recipe.map(([kind, a, b, c]) => {
    switch (kind) {
      case 'hex':
        return Uint8Array.from(a.match(/../g) ?? [], (h) => parseInt(h, 16));
      case 'text':
        return new TextEncoder().encode(a.repeat(b));
      case 'random':
        return random(a, b);
      case 'fill':
        return new Uint8Array(b).fill(a);
      case 'mod':
        return Uint8Array.from({ length: a }, (_, i) => Math.floor(i / c) % b);
      case 'words':
        return words(a, b);
      default:
        throw new Error(kind);
    }
  });
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

const html = '<!doctype html><html><head><title>Post</title><link rel="stylesheet" href="/css/site.css"></head><body><article class="post">\n';
const cases = [
  ['empty', []],
  ['one byte 00', [['hex', '00']]],
  ['one byte ff', [['hex', 'ff']]],
  ['two bytes', [['hex', '6162']]],
  ['three bytes', [['hex', '616263']]],
  ['aaaa', [['text', 'a', 4]]],
  ['abcabc', [['text', 'abc', 100]]],
  ['zeros 100k', [['fill', 0, 100000]]],
  ['run 775', [['fill', 7, 258 * 3 + 1]]],
  ['random 1', [['random', 1, 1]]],
  ['random 1000', [['random', 1000, 2]]],
  ['random 70000', [['random', 70000, 3]]],
  ['html repeated', [['text', html, 3000]]],
  ['mod 251', [['mod', 200000, 251, 1]]],
  ['mod 3 slow', [['mod', 300000, 3, 7]]],
  ['words 20k', [['words', 20000, 5]]],
  ['words 300k (>1 MB)', [['words', 300000, 6]]],
  ['text then noise', [['text', html, 8000], ['random', 200000, 4], ['words', 50000, 7]]],
  ['noise then text', [['random', 40000, 8], ['text', 'abcdefgh', 5000]]],
  ['far 3-byte matches', [['text', 'xyz', 1], ['random', 5000, 10], ['text', 'xyz', 1], ['random', 40000, 11], ['text', 'xyzw', 3]]],
  ...[16383, 16384, 16385, 32769, 65535, 65536].map((n) => [`random ${n} (block boundary)`, [['random', n, n]]]),
  ...[16383, 16384, 16385].map((n) => [`words ${n}`, [['words', n, n]]]),
  ['skewed alphabet', [['words', 1000, 12], ['fill', 255, 3], ['mod', 5000, 2, 13]]],
];

const sha = (b) => 'sha256:' + createHash('sha256').update(b).digest('hex');
const out = [];
for (const [name, input] of cases) {
  const bytes = build(input);
  const raw = deflateRaw(bytes);
  const gz = gzip(bytes);
  if (Buffer.compare(zlib.inflateRawSync(raw), Buffer.from(bytes)) !== 0) throw new Error(`${name}: raw round trip`);
  if (Buffer.compare(zlib.gunzipSync(gz), Buffer.from(bytes)) !== 0) throw new Error(`${name}: gzip round trip`);
  out.push({ name, input, length: bytes.length, inputDigest: sha(bytes), rawLength: raw.length, raw: sha(raw), gzip: sha(gz) });
}
writeFileSync(path.join(here, 'fixtures.json'), JSON.stringify({ cases: out }, null, 1) + '\n');
console.log(`${out.length} deflate fixtures`);
