// Generates the content-type table that `sitefile serve` and Web Bundle export
// use (src/mime-data.ts and rust/src/mime_data.rs) from the list below.
//
//   node tools/gen-mime.mjs           # rewrite both files
//   node tools/gen-mime.mjs --check   # exit 1 when either file is out of date
//
// The types follow nginx's conf/mime.types where it has the extension, and the
// IANA registrations (via mime-db) for the rest. Types marked `text` are sent
// with `; charset=utf-8`. Both files hold exactly the same entries, sorted by
// extension; lookups are by the lowercase extension of the last path segment.
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/** [extension, type, text] */
const TYPES = [
  // documents and markup
  ['html', 'text/html', true],
  ['htm', 'text/html', true],
  ['xhtml', 'application/xhtml+xml', true],
  ['css', 'text/css', true],
  ['txt', 'text/plain', true],
  ['md', 'text/markdown', true],
  ['markdown', 'text/markdown', true],
  ['csv', 'text/csv', true],
  ['tsv', 'text/tab-separated-values', true],
  ['ics', 'text/calendar', true],
  ['vtt', 'text/vtt', true],
  ['xml', 'application/xml', true],
  ['xsl', 'application/xml', true],
  ['xslt', 'application/xslt+xml', true],
  ['rss', 'application/rss+xml', true],
  ['atom', 'application/atom+xml', true],
  ['yaml', 'application/yaml', true],
  ['yml', 'application/yaml', true],
  ['toml', 'application/toml', true],
  ['pdf', 'application/pdf', false],
  ['epub', 'application/epub+zip', false],
  // scripts and data
  ['js', 'text/javascript', true],
  ['mjs', 'text/javascript', true],
  ['cjs', 'text/javascript', true],
  ['json', 'application/json', true],
  ['map', 'application/json', true],
  ['jsonld', 'application/ld+json', true],
  ['geojson', 'application/geo+json', true],
  ['ndjson', 'application/x-ndjson', true],
  ['webmanifest', 'application/manifest+json', true],
  ['wasm', 'application/wasm', false],
  // images
  ['svg', 'image/svg+xml', true],
  ['png', 'image/png', false],
  ['apng', 'image/apng', false],
  ['jpg', 'image/jpeg', false],
  ['jpeg', 'image/jpeg', false],
  ['gif', 'image/gif', false],
  ['webp', 'image/webp', false],
  ['avif', 'image/avif', false],
  ['jxl', 'image/jxl', false],
  ['bmp', 'image/bmp', false],
  ['tif', 'image/tiff', false],
  ['tiff', 'image/tiff', false],
  ['heic', 'image/heic', false],
  ['heif', 'image/heif', false],
  ['ico', 'image/x-icon', false],
  // fonts
  ['woff', 'font/woff', false],
  ['woff2', 'font/woff2', false],
  ['ttf', 'font/ttf', false],
  ['otf', 'font/otf', false],
  ['ttc', 'font/collection', false],
  ['eot', 'application/vnd.ms-fontobject', false],
  // audio and video
  ['mp4', 'video/mp4', false],
  ['m4v', 'video/mp4', false],
  ['webm', 'video/webm', false],
  ['ogv', 'video/ogg', false],
  ['mov', 'video/quicktime', false],
  ['mkv', 'video/x-matroska', false],
  ['mp3', 'audio/mpeg', false],
  ['m4a', 'audio/mp4', false],
  ['aac', 'audio/aac', false],
  ['oga', 'audio/ogg', false],
  ['ogg', 'audio/ogg', false],
  ['opus', 'audio/ogg', false],
  ['weba', 'audio/webm', false],
  ['wav', 'audio/wav', false],
  ['flac', 'audio/flac', false],
  ['m3u8', 'application/vnd.apple.mpegurl', false],
  ['mpd', 'application/dash+xml', true],
  // 3D models
  ['gltf', 'model/gltf+json', true],
  ['glb', 'model/gltf-binary', false],
  // archives
  ['wbn', 'application/webbundle', false],
  ['zip', 'application/zip', false],
  ['gz', 'application/gzip', false],
  ['tar', 'application/x-tar', false],
];

/**
 * Extensionless well-known paths (johnhenry/sitefile#180), checked before the
 * extension: [exact tree path, type, text]. Apple accepts the association file
 * at the root too; RFC 8615 names, WebFinger (RFC 7033), host-meta (RFC 6415),
 * OpenID Connect and OAuth server metadata (RFC 8414), NodeInfo.
 */
const WELL_KNOWN = [
  ['/.well-known/apple-app-site-association', 'application/json', true],
  ['/apple-app-site-association', 'application/json', true],
  ['/.well-known/openid-configuration', 'application/json', true],
  ['/.well-known/oauth-authorization-server', 'application/json', true],
  ['/.well-known/webfinger', 'application/jrd+json', true],
  ['/.well-known/nodeinfo', 'application/json', true],
  ['/.well-known/host-meta', 'application/xrd+xml', true],
];

const seen = new Set();
for (const [ext] of TYPES) {
  if (seen.has(ext) || ext !== ext.toLowerCase()) throw new Error(`duplicate or non-lowercase extension: ${ext}`);
  seen.add(ext);
}
for (const [p] of WELL_KNOWN) {
  const base = p.slice(p.lastIndexOf('/') + 1);
  if (base.includes('.') || seen.has(p)) throw new Error(`well-known path with an extension, or a duplicate: ${p}`);
  seen.add(p);
}
const wellKnown = WELL_KNOWN.map(([p, type, text]) => [p, text ? `${type}; charset=utf-8` : type]).sort((a, b) => (a[0] < b[0] ? -1 : 1));
const rows = TYPES.map(([ext, type, text]) => [ext, text ? `${type}; charset=utf-8` : type]).sort((a, b) => (a[0] < b[0] ? -1 : 1));

const header = [
  '// GENERATED by tools/gen-mime.mjs; do not edit.',
  '// Content types for serve and Web Bundle export: exact well-known paths without an extension first, then',
  `// the lowercase file extension (${wellKnown.length} + ${rows.length} entries, each list sorted).`,
  '',
];
const ts = [
  ...header,
  '// prettier-ignore',
  'export const WELL_KNOWN_TYPES: ReadonlyArray<readonly [string, string]> = [',
  ...wellKnown.map(([e, t]) => `  [${JSON.stringify(e)}, ${JSON.stringify(t)}],`),
  '];',
  '',
  '// prettier-ignore',
  'export const MIME_TYPES: ReadonlyArray<readonly [string, string]> = [',
  ...rows.map(([e, t]) => `  [${JSON.stringify(e)}, ${JSON.stringify(t)}],`),
  '];',
  '',
].join('\n');
const rs = [
  ...header,
  '#[rustfmt::skip]',
  'pub const WELL_KNOWN_TYPES: &[(&str, &str)] = &[',
  ...wellKnown.map(([e, t]) => `    (${JSON.stringify(e)}, ${JSON.stringify(t)}),`),
  '];',
  '',
  '#[rustfmt::skip]',
  'pub const MIME_TYPES: &[(&str, &str)] = &[',
  ...rows.map(([e, t]) => `    (${JSON.stringify(e)}, ${JSON.stringify(t)}),`),
  '];',
  '',
].join('\n');

const outputs = [
  [path.join(root, 'src', 'mime-data.ts'), ts],
  [path.join(root, 'rust', 'src', 'mime_data.rs'), rs],
];
if (process.argv.includes('--check')) {
  let stale = false;
  for (const [file, text] of outputs) {
    let have = '';
    try {
      have = readFileSync(file, 'utf8');
    } catch {}
    if (have !== text) {
      console.error(`${path.relative(root, file)} is out of date: run node tools/gen-mime.mjs`);
      stale = true;
    }
  }
  process.exit(stale ? 1 : 0);
}
for (const [file, text] of outputs) writeFileSync(file, text);
console.log(`wrote ${wellKnown.length} well-known paths and ${rows.length} content types to ${outputs.map(([f]) => path.relative(root, f)).join(' and ')}`);
