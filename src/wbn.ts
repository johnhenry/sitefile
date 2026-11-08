import { SitefileError } from './errors.js';
import { cborArray, cborBytes, cborMap, cborText, cborUint } from './cbor.js';
import { headersFor, matchRedirect, parseHeaderRules, parseRedirectRules, type ServeHeaders, type ServeRedirect } from './serve-rules.js';
import type { Tree } from './tree.js';
import { MIME_TYPES, WELL_KNOWN_TYPES } from './mime-data.js';

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
 *
 * The tree's own `/_headers` and `/_redirects` (what HEADER and REDIRECT
 * write) are applied the way `sitefile serve` applies them (serve-rules.ts):
 * see {@link bundleResponses}.
 */
const MAGIC = Buffer.from('f09f8c90f09f93a6', 'hex');
const VERSION = Buffer.from([0x62, 0x32, 0x00, 0x00]);

/** Content types by extension and well-known path (tools/gen-mime.mjs; the same tables as rust/src/mime_data.rs). */
const MIME: ReadonlyMap<string, string> = new Map(MIME_TYPES);
const WELL_KNOWN: ReadonlyMap<string, string> = new Map(WELL_KNOWN_TYPES);

/**
 * The content type `serve` and Web Bundle export give a tree path: a well-known extensionless path
 * (`/.well-known/apple-app-site-association`, ...) by its exact name, else by the extension, ASCII
 * case-insensitively (`application/octet-stream` when unknown).
 */
export function mimeTypeFor(path: string): string {
  const known = WELL_KNOWN.get(path);
  if (known !== undefined) return known;
  const base = path.slice(path.lastIndexOf('/') + 1);
  const dot = base.lastIndexOf('.');
  const ext = dot < 0 ? '' : base.slice(dot + 1).replace(/[A-Z]/g, (c) => c.toLowerCase());
  return MIME.get(ext) ?? 'application/octet-stream';
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
  return writeWebBundleAt(tree, validateBaseUrl(baseUrl));
}

/** {@link writeWebBundle} at a base URL the caller has already validated (`isolated-app://<id>/` for swbn). */
export function writeWebBundleAt(tree: Tree, base: string): Uint8Array {
  const { responses, urls } = bundleResponses(tree);

  const index: Array<[Uint8Array, Uint8Array]> = [];
  // Offset of the first response = size of the responses array head.
  const offsets: number[] = [];
  let offset = headSize(responses.length);
  for (const r of responses) {
    offsets.push(offset);
    offset += r.length;
  }
  for (const [path, i] of urls) {
    // b2 index entries are [offset, length]; b1's leading variants value is gone.
    index.push([cborText(base + encodePath(path)), cborArray([cborUint(offsets[i]!), cborUint(responses[i]!.length)])]);
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

/** One encoded response: `[headers bstr, payload bstr]`, header names in CBOR map (bytewise) order. */
function encodeResponse(status: number, headers: Iterable<readonly [string, string]>, body: Uint8Array): Uint8Array {
  const map = cborMap([[bstr(':status'), bstr(String(status))], ...[...headers].map(([k, v]): [Uint8Array, Uint8Array] => [bstr(k), bstr(v)])]);
  return cborArray([cborBytes(map), cborBytes(body)]);
}

const isRedirectStatus = (s: number): boolean => s >= 301 && s <= 308;

/** The rule `serve` would apply first in one pass (forced or not), when it is an exact-path 3xx redirect. */
function exactRedirect(rules: readonly ServeRedirect[], forced: boolean, path: string): number | undefined {
  const i = rules.findIndex((r) => r.force === forced && matchRedirect(r, path) !== undefined);
  return i >= 0 && !rules[i]!.splat && isRedirectStatus(rules[i]!.status) ? i : undefined;
}

/**
 * The responses section and its URLs (tree paths, percent-encoded by the caller), as `sitefile
 * serve` would answer them:
 *
 * - Each file is a 200 with `content-type` and the `/_headers` blocks whose pattern matches its
 *   URL path (`headersFor`; a `content-type` there replaces the inferred one). A directory URL
 *   (`/docs/` for `/docs/index.html`) matches with its trailing slash, so it gets its own
 *   response when its headers differ from the file URL's.
 * - An exact-path `/_redirects` rule with a 3xx status becomes a response with that `:status`, a
 *   `location` of the rule's target as written, and an empty body: at its path when no file
 *   would be served there, and over the file when forced (`301!`). Like `serve`, the first rule
 *   matching the path decides; when that is a splat or a 200/404 rewrite, the bundle leaves the
 *   path to `/_redirects`. Splat rules and rewrites are not baked in.
 *
 * `/_headers` and `/_redirects` stay in the bundle as files. Responses are deduplicated (both URLs
 * of an index page share one when their headers agree; identical redirects share one).
 */
export function bundleResponses(tree: Tree): { responses: Uint8Array[]; urls: Array<[string, number]> } {
  const text = (p: string): string => {
    const f = tree.get(p);
    return f ? new TextDecoder().decode(f.bytes) : '';
  };
  const redirects = parseRedirectRules(text('/_redirects'));
  const headerRules: readonly ServeHeaders[] = parseHeaderRules(text('/_headers'));

  const responses: Uint8Array[] = [];
  const urls: Array<[string, number]> = [];
  const taken = new Set<string>();
  const redirectResponse = new Map<string, number>();
  const redirectTo = (rule: number): number => {
    const r = redirects[rule]!;
    const key = `${r.status} ${r.to}`;
    let i = redirectResponse.get(key);
    if (i === undefined) {
      i = responses.push(encodeResponse(r.status, [['location', r.to]], new Uint8Array(0))) - 1;
      redirectResponse.set(key, i);
    }
    return i;
  };
  const fileResponse = (p: string, bytes: Uint8Array, urlPath: string): Uint8Array => {
    const headers = new Map<string, string>([['content-type', mimeTypeFor(p)]]);
    for (const [k, v] of Object.entries(headersFor(headerRules, urlPath))) headers.set(k, v);
    return encodeResponse(200, headers, bytes);
  };

  for (const [p, node] of tree.entries()) {
    // [URL path, the path `serve` matches redirects against (no trailing slash)]
    const targets: Array<[string, string]> = [[p, p]];
    if (p === '/index.html' || p.endsWith('/index.html')) {
      const dir = p.slice(0, -'index.html'.length);
      targets.push([dir, dir === '/' ? '/' : dir.slice(0, -1)]);
    }
    let last: { bytes: Uint8Array; i: number } | undefined;
    for (const [urlPath, matchPath] of targets) {
      taken.add(urlPath);
      const forced = exactRedirect(redirects, true, matchPath);
      if (forced !== undefined) {
        urls.push([urlPath, redirectTo(forced)]);
        continue;
      }
      const r = fileResponse(p, node.bytes, urlPath);
      if (last === undefined || !Buffer.from(last.bytes).equals(r)) last = { bytes: r, i: responses.push(r) - 1 };
      urls.push([urlPath, last.i]);
    }
  }

  // Redirects at paths without a file: `serve` applies a forced rule first, then (when nothing
  // is served there: no file, no `<path>.html`, no directory) the first other matching rule.
  const servable = (p: string): boolean => tree.isFile(p) && p !== '/_redirects' && p !== '/_headers';
  for (const r of redirects) {
    if (r.splat || !isRedirectStatus(r.status) || taken.has(r.from)) continue;
    const path = r.from;
    taken.add(path);
    let rule = exactRedirect(redirects, true, path);
    if (rule === undefined) {
      const firstForced = redirects.some((x) => x.force && matchRedirect(x, path) !== undefined);
      const served = path === '/' ? servable('/index.html') : servable(path) || servable(`${path}.html`) || tree.isDirectory(path);
      if (firstForced || served) continue;
      rule = exactRedirect(redirects, false, path);
    }
    if (rule !== undefined) urls.push([path, redirectTo(rule)]);
  }
  return { responses, urls };
}

function headSize(n: number): number {
  if (n < 24) return 1;
  if (n < 0x100) return 2;
  if (n < 0x10000) return 3;
  if (n < 0x100000000) return 5;
  return 9;
}
