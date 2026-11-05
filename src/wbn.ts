import { SitefileError } from './errors.js';
import { cborArray, cborBytes, cborMap, cborText, cborUint } from './cbor.js';
import type { Tree } from './tree.js';

/**
 * Unsigned Web Bundle, format version "b2"
 * (draft-ietf-wpack-bundled-responses). Layout, as one CBOR array:
 *
 *   [ magic h'F09F8C90F09F93A6' ("🌐📦"), version h'62320000' ("b2\0\0"),
 *     section-lengths (bstr holding a CBOR array of name, length, ...),
 *     sections (array: index, [primary,] responses),
 *     length (8-byte big-endian bstr: size of the whole bundle) ]
 *
 * (b1 had six items with the primary URL in the header and variants in the
 * index; b2 has five, so the header starts 0x85.) Verified against the WICG
 * reference reader `dump-bundle` (see test/export-formats.test.ts).
 *
 * index    = { url => [ offset, length ] }; offset/length
 *            locate the response inside the responses section, counted from
 *            the first byte of that section's CBOR encoding.
 * responses = [ [ headers (bstr holding { h':status' => h'200', h'content-type' => ... }), payload bstr ] ... ]
 *
 * primary  = text URL, present only when /index.html exists. No signatures.
 */
const MAGIC = Buffer.from('f09f8c90f09f93a6', 'hex');
const VERSION = Buffer.from([0x62, 0x32, 0x00, 0x00]);

const TEXT_CHARSET = '; charset=utf-8';

const MIME: Readonly<Record<string, string>> = {
  html: 'text/html' + TEXT_CHARSET,
  htm: 'text/html' + TEXT_CHARSET,
  css: 'text/css' + TEXT_CHARSET,
  js: 'text/javascript' + TEXT_CHARSET,
  mjs: 'text/javascript' + TEXT_CHARSET,
  json: 'application/json' + TEXT_CHARSET,
  svg: 'image/svg+xml' + TEXT_CHARSET,
  txt: 'text/plain' + TEXT_CHARSET,
  xml: 'application/xml' + TEXT_CHARSET,
  webmanifest: 'application/manifest+json' + TEXT_CHARSET,
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  ico: 'image/x-icon',
  wasm: 'application/wasm',
  woff: 'font/woff',
  woff2: 'font/woff2',
  pdf: 'application/pdf',
  mp4: 'video/mp4',
  webm: 'video/webm',
  mp3: 'audio/mpeg',
};

export function mimeTypeFor(path: string): string {
  const base = path.slice(path.lastIndexOf('/') + 1);
  const dot = base.lastIndexOf('.');
  const ext = dot < 0 ? '' : base.slice(dot + 1).toLowerCase();
  return MIME[ext] ?? 'application/octet-stream';
}

/** An absolute https URL ending in `/`, without credentials, query or fragment. Returns its normalized form. */
export function validateBaseUrl(raw: string | undefined): string {
  if (raw === undefined || raw === '') {
    throw new SitefileError('E_VALIDATION', "wbn export needs a base URL: use 'EXPORT <path> AS wbn BASE https://example.com/' or --base-url");
  }
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new SitefileError('E_VALIDATION', `invalid base URL '${raw}': not an absolute URL`);
  }
  if (u.protocol !== 'https:') throw new SitefileError('E_VALIDATION', `invalid base URL '${raw}': must be https`);
  if (u.username || u.password) throw new SitefileError('E_VALIDATION', `invalid base URL '${raw}': must not contain credentials`);
  if (u.search || u.hash || raw.includes('?') || raw.includes('#')) {
    throw new SitefileError('E_VALIDATION', `invalid base URL '${raw}': must not have a query or fragment`);
  }
  if (!raw.endsWith('/')) throw new SitefileError('E_VALIDATION', `invalid base URL '${raw}': must end with '/'`);
  return u.href;
}

function encodePath(p: string): string {
  return p
    .slice(1)
    .split('/')
    .map((seg) => encodeURIComponent(seg))
    .join('/');
}

const enc = new TextEncoder();
const bstr = (s: string): Uint8Array => cborBytes(enc.encode(s));

export function writeWebBundle(tree: Tree, baseUrl: string | undefined): Uint8Array {
  const base = validateBaseUrl(baseUrl);
  const entries = tree.entries();

  const responses: Uint8Array[] = [];
  const index: Array<[Uint8Array, Uint8Array]> = [];
  // Offset of the first response = size of the responses array head.
  let offset = headSize(entries.length);
  for (const [p, node] of entries) {
    const headers = cborMap([
      [bstr(':status'), bstr('200')],
      [bstr('content-type'), bstr(mimeTypeFor(p))],
    ]);
    const response = cborArray([cborBytes(headers), cborBytes(node.bytes)]);
    // b2 index entries are [offset, length]; b1's leading variants value is gone.
    const loc = [cborUint(offset), cborUint(response.length)];
    const urls = [base + encodePath(p)];
    if (p === '/index.html' || p.endsWith('/index.html')) urls.push(base + encodePath(p.slice(0, -'index.html'.length)));
    for (const url of urls) index.push([cborText(url), cborArray(loc)]);
    responses.push(response);
    offset += response.length;
  }
  const responsesSection = cborArray(responses);
  const indexSection = cborMap(index);
  // b2 moves the primary URL out of the header into an optional "primary"
  // section; sections are ordered index, primary, responses (always last).
  const sections: Array<[string, Uint8Array]> = [['index', indexSection]];
  if (tree.isFile('/index.html')) sections.push(['primary', cborText(base)]);
  sections.push(['responses', responsesSection]);
  const sectionLengths = cborArray(sections.flatMap(([name, bytes]) => [cborText(name), cborUint(bytes.length)]));

  // The trailing length field is fixed-size, so the total is computable first.
  const parts = [
    Buffer.from([0x85]), // b2: array(5) = magic, version, section-lengths, sections, length
    cborBytes(MAGIC),
    cborBytes(VERSION),
    cborBytes(sectionLengths),
    cborArray(sections.map(([, bytes]) => bytes)),
  ];
  const lenField = 1 + 8; // bstr(8) head + 8 bytes
  const total = parts.reduce((n, b) => n + b.length, 0) + lenField;
  const len = Buffer.alloc(8);
  len.writeBigUInt64BE(BigInt(total));
  return Buffer.concat([...parts, cborBytes(len)]);
}

function headSize(n: number): number {
  if (n < 24) return 1;
  if (n < 0x100) return 2;
  if (n < 0x10000) return 3;
  if (n < 0x100000000) return 5;
  return 9;
}
