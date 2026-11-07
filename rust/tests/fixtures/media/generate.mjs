// Regenerates the image header differential fixture from the TypeScript implementation.
//
//   npm ci && npm run build        # at the repository root (builds lib/)
//   node rust/tests/fixtures/media/generate.mjs
//
// Writes fuzz.json: [{name, bytes (base64), ok: [width, height] | null (an SVG), error: [code, detail]}],
// imageSize (src/media.ts) on the repository's test images, small hand-made headers (PNG, GIF,
// JPEG with EXIF orientation, WebP VP8/VP8L/VP8X, AVIF with ispe and irot) and seeded mutations
// and truncations of them. Checked by the Rust test `media::tests::differential_fixtures`.
// FUZZ_COUNT (default 1200) mutated inputs.
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const { imageSize } = await import(pathToFileURL(path.join(root, 'lib/src/media.js')).href);
const { SitefileError } = await import(pathToFileURL(path.join(root, 'lib/src/errors.js')).href);

let seed = Number(process.env.FUZZ_SEED ?? 0x1a6e);
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const int = (n) => Math.floor(rand() * n);
const pick = (xs) => xs[int(xs.length)];

const bytes = (...parts) => Uint8Array.from(parts.flatMap((p) => (typeof p === 'string' ? [...p].map((c) => c.charCodeAt(0)) : Array.isArray(p) ? p : [p])));
const be32 = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
const be16 = (n) => [(n >>> 8) & 255, n & 255];
const le16 = (n) => [n & 255, (n >>> 8) & 255];
const le24 = (n) => [n & 255, (n >>> 8) & 255, (n >>> 16) & 255];
const le32 = (n) => [n & 255, (n >>> 8) & 255, (n >>> 16) & 255, (n >>> 24) & 255];
const box = (type, ...payload) => {
  const body = bytes(...payload);
  return [...be32(body.length + 8), ...[...type].map((c) => c.charCodeAt(0)), ...body];
};

/** EXIF APP1 with orientation `o` (byte order `bo`: 'II' or 'MM'). */
const exif = (o, bo = 'II') => {
  const w16 = bo === 'II' ? le16 : be16;
  const w32 = bo === 'II' ? le32 : be32;
  const tiff = bytes(bo, w16(42), w32(8), w16(1), w16(0x0112), w16(3), w32(1), w16(o), [0, 0], w32(0));
  const data = bytes('Exif\0\0', [...tiff]);
  return [0xff, 0xe1, ...be16(data.length + 2), ...data];
};
const sof = (m, w, h) => [0xff, m, ...be16(17), 8, ...be16(h), ...be16(w), 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1];

const SEEDS = {
  'a.png': bytes(0x89, 'PNG\r\n\x1a\n', be32(13), 'IHDR', be32(640), be32(480), 8, 6, 0, 0, 0),
  'a.gif': bytes('GIF89a', le16(320), le16(200), 0, 0, 0),
  'a.jpg': bytes(0xff, 0xd8, [0xff, 0xe0, 0, 16, ...'JFIF\0'.split('').map((c) => c.charCodeAt(0)), 1, 1, 0, 0, 1, 0, 1, 0, 0], sof(0xc0, 800, 600), 0xff, 0xd9),
  'rot6.jpg': bytes(0xff, 0xd8, exif(6), sof(0xc2, 800, 600), 0xff, 0xda),
  'rot3-mm.jpg': bytes(0xff, 0xd8, exif(3, 'MM'), 0xff, 0xff, sof(0xc1, 120, 90)),
  'rot8-mm.jpg': bytes(0xff, 0xd8, exif(8, 'MM'), sof(0xcd, 7, 9)),
  'a.webp': bytes('RIFF', le32(30), 'WEBP', 'VP8 ', le32(10), [0, 0, 0], 0x9d, 0x01, 0x2a, le16(1024), le16(768)),
  'b.webp': bytes('RIFF', le32(30), 'WEBP', 'VP8L', le32(5), 0x2f, le32((99 & 0x3fff) | ((49 & 0x3fff) << 14))),
  'c.webp': bytes('RIFF', le32(30), 'WEBP', 'VP8X', le32(10), [0, 0, 0, 0], le24(1919), le24(1079)),
  'a.avif': bytes(
    box('ftyp', 'avif', be32(0), 'mif1', 'avif'),
    box(
      'meta',
      be32(0),
      box('pitm', be32(0), be16(1)),
      box('iprp', box('ipco', box('ispe', be32(0), be32(1280), be32(720)), box('irot', 1)), box('ipma', be32(0), be32(1), be16(1), 2, 0x81, 0x02)),
    ),
  ),
  'b.avif': bytes(
    box('ftyp', 'heic', be32(0), 'avis'),
    box('meta', be32(0), box('pitm', be32(0x01000000), be32(7)), box('iprp', box('ipco', box('ispe', be32(0), be32(64), be32(32))), box('ipma', be32(0x01000001), be32(1), be32(7), 1, be16(0x8001)))),
  ),
  'logo.svg': bytes('<svg xmlns="http://www.w3.org/2000/svg"/>'),
  'text.png': bytes('not an image at all'),
};
for (const f of ['tiny.png', 'tiny.jpg', 'rotated.jpg', 'tiny.gif', 'tiny-lossy.webp', 'bomb.png']) {
  SEEDS[f] = new Uint8Array(readFileSync(path.join(root, 'plugins/image/testdata', f)));
}
for (const f of ['sky.jpg', 'badge.png']) SEEDS[f] = new Uint8Array(readFileSync(path.join(root, 'examples/image/photos', f))).subarray(0, 768);

function record(name, b) {
  try {
    const s = imageSize(b, '/' + name);
    return { name, bytes: Buffer.from(b).toString('base64'), ok: s ? [s.width, s.height] : null };
  } catch (e) {
    if (!(e instanceof SitefileError)) throw e;
    return { name, bytes: Buffer.from(b).toString('base64'), error: [e.code, e.detail] };
  }
}

function mutate(b) {
  const out = Uint8Array.from(b);
  const r = rand();
  if (r < 0.35) return out.subarray(0, int(out.length + 1)); // truncate
  if (r < 0.7) {
    for (let n = 1 + int(3); n > 0; n--) out[int(out.length)] = pick([0, 0xff, 1, int(256), 0x7f, 0x80]);
    return out;
  }
  if (r < 0.85) {
    // A byte in the first 64 (headers live there).
    out[int(Math.min(64, out.length))] = int(256);
    return out;
  }
  const at = int(out.length);
  return Uint8Array.from([...out.subarray(0, at), ...Array.from({ length: 1 + int(8) }, () => int(256)), ...out.subarray(at)]);
}

const cases = Object.entries(SEEDS).map(([n, b]) => record(n, b));
const names = Object.keys(SEEDS);
const count = Number(process.env.FUZZ_COUNT ?? 1200);
for (let k = 0; k < count; k++) {
  const n = pick(names);
  cases.push(record(n, mutate(SEEDS[n])));
}
writeFileSync(path.join(here, 'fuzz.json'), '[\n' + cases.map((c) => JSON.stringify(c)).join(',\n') + '\n]\n');
console.log(`${cases.length} inputs (${cases.filter((c) => c.ok).length} sizes, ${cases.filter((c) => c.error).length} errors)`);
