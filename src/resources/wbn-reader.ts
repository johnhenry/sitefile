import { SitefileError, type SourceSpan } from '../errors.js';
import { normalizeVirtual } from '../paths.js';
import { makeFile, Tree, type FileNode } from '../tree.js';
import { CborError, decodeCbor, readCborHead, type CborValue } from '../cbor.js';
import type { ArchiveLimits } from './types.js';

/**
 * Reader for unsigned Web Bundles, formats b1 and b2 (the WICG
 * "draft-yasskin-wpack-bundled-exchanges" lineage and
 * draft-ietf-wpack-bundled-responses). Layouts, as implemented by the WICG Go
 * reference reader (go/bundle/decoder.go):
 *
 *   b2: array(5) [ magic, version "b2\0\0", section-lengths, sections, length ]
 *   b1: array(6) [ magic, version "b1\0\0", primary URL (text), section-lengths, sections, length ]
 *
 * section-lengths is a byte string holding a CBOR array of (name, length) pairs;
 * `sections` is an array with one item per pair; `responses` must be last. The
 * trailing length is an 8-byte big-endian byte string equal to the bundle size.
 * index = { url => value }: b2 value = [offset, length]; b1 value =
 * [variants-value bstr, offset, length, ...] (empty variants-value: exactly one
 * location). Offsets are counted from the first byte of the responses section's
 * CBOR encoding. A response is array(2) [ bstr(CBOR map of bstr -> bstr incl.
 * ":status"), body bstr ].
 *
 * Signatures, manifest and any unknown section are length-checked and ignored;
 * nothing is verified or executed.
 */

export interface BundleEntry {
  url: string;
  status: number;
  headers: Record<string, string>;
  body: Uint8Array;
}

export interface WebBundle {
  version: 'b1' | 'b2';
  primaryUrl: string | undefined;
  entries: BundleEntry[];
}

const MAGIC = [0xf0, 0x9f, 0x8c, 0x90, 0xf0, 0x9f, 0x93, 0xa6];
/** Magic prefix: array head, bstr(8) head, then the 8 magic bytes. */
const HEAD_LEN = 10;
/** The trailing length field: bstr(8) head + 8 bytes. */
const TAIL_LEN = 9;
const MAX_SECTION_LENGTHS = 8192;

function hasMagic(b: Uint8Array): boolean {
  if (b.length < HEAD_LEN || (b[0] !== 0x85 && b[0] !== 0x86) || b[1] !== 0x48) return false;
  return MAGIC.every((x, i) => b[2 + i] === x);
}

export function isWebBundle(bytes: Uint8Array): boolean {
  return hasMagic(bytes);
}

export function readWebBundle(bytes: Uint8Array, origin: string, limits: ArchiveLimits, span?: SourceSpan): WebBundle {
  const bad = (why: string): SitefileError => new SitefileError('E_ARCHIVE', `${origin}: web bundle: ${why}`, span);
  try {
    return read(bytes, origin, limits, span, bad);
  } catch (e) {
    if (e instanceof CborError) throw bad(`malformed CBOR: ${e.message}`);
    throw e;
  }
}

function read(
  bytes: Uint8Array,
  origin: string,
  limits: ArchiveLimits,
  span: SourceSpan | undefined,
  bad: (why: string) => SitefileError,
): WebBundle {
  if (!hasMagic(bytes)) throw bad('bad magic bytes');
  if (bytes.length < HEAD_LEN + 5) throw bad('truncated header');
  const isB1 = bytes[0] === 0x86;
  if (bytes[HEAD_LEN] !== 0x44 || bytes[HEAD_LEN + 1] !== 0x62 || bytes[HEAD_LEN + 3] !== 0 || bytes[HEAD_LEN + 4] !== 0) {
    throw bad('unrecognized version field');
  }
  const verDigit = bytes[HEAD_LEN + 2];
  if (verDigit !== (isB1 ? 0x31 : 0x32)) {
    throw bad(verDigit === 0x31 || verDigit === 0x32 ? 'header does not match its version' : 'unsupported version (expected b1 or b2)');
  }
  const version = isB1 ? 'b1' : 'b2';
  let pos = HEAD_LEN + 5;

  let primaryUrl: string | undefined;
  if (isB1) {
    const p = decodeCbor(bytes.subarray(pos), { maxItems: 1 });
    if (typeof p.value !== 'string') throw bad('primary URL is not a text string');
    primaryUrl = p.value;
    pos += p.end;
  }

  // section-lengths
  const slHead = readCborHead(bytes, pos);
  if (slHead.major !== 2 || typeof slHead.arg !== 'number') throw bad('section-lengths is not a byte string');
  if (slHead.arg >= MAX_SECTION_LENGTHS) throw bad('section-lengths is too long');
  if (slHead.next + slHead.arg > bytes.length) throw bad('truncated section-lengths');
  const sl = decodeCbor(bytes.subarray(slHead.next, slHead.next + slHead.arg), { maxItems: MAX_SECTION_LENGTHS });
  if (!Array.isArray(sl.value) || sl.end !== slHead.arg || sl.value.length % 2 !== 0) {
    throw bad('malformed section-lengths');
  }
  const sections: Array<{ name: string; length: number; start: number }> = [];
  pos = slHead.next + slHead.arg;

  const secHead = readCborHead(bytes, pos);
  if (secHead.major !== 4) throw bad('sections is not an array');
  if (secHead.arg !== sl.value.length / 2) throw bad(`section-lengths declares ${sl.value.length / 2} sections but ${secHead.arg} are present`);
  let cursor = secHead.next;
  const seen = new Set<string>();
  for (let i = 0; i < sl.value.length; i += 2) {
    const name = sl.value[i];
    const length = sl.value[i + 1];
    if (typeof name !== 'string' || typeof length !== 'number') throw bad('malformed section-lengths entry');
    if (seen.has(name)) throw bad(`duplicate section '${name}'`);
    seen.add(name);
    sections.push({ name, length, start: cursor });
    cursor += length;
    if (cursor > bytes.length - TAIL_LEN) throw bad(`section '${name}' extends past the end of the bundle`);
  }
  const last = sections[sections.length - 1];
  if (!last || last.name !== 'responses') throw bad("the last section must be 'responses'");

  // trailing length
  if (cursor + TAIL_LEN !== bytes.length) throw bad('section lengths do not add up to the bundle size');
  if (bytes[cursor] !== 0x48) throw bad('malformed trailing length field');
  const declared = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getBigUint64(cursor + 1);
  if (declared !== BigInt(bytes.length)) throw bad(`length field says ${declared} bytes but the bundle is ${bytes.length}`);

  const section = (name: string): Uint8Array | undefined => {
    const s = sections.find((x) => x.name === name);
    return s && bytes.subarray(s.start, s.start + s.length);
  };

  const primarySection = section('primary');
  if (primarySection) {
    const p = decodeCbor(primarySection, { maxItems: 1 });
    if (typeof p.value !== 'string' || p.end !== primarySection.length) throw bad("malformed 'primary' section");
    primaryUrl = p.value;
  }
  if (primaryUrl !== undefined) {
    try {
      new URL(primaryUrl);
    } catch {
      throw bad(`primary URL '${primaryUrl}' is not an absolute URL`);
    }
  }

  const indexSection = section('index');
  if (!indexSection) throw bad("missing 'index' section");
  const responses = last;
  const indexHead = readCborHead(indexSection, 0);
  if (indexHead.major !== 5) throw bad('index is not a map');
  if (typeof indexHead.arg !== 'number' || indexHead.arg > limits.maxEntries) {
    throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: web bundle has more than ${limits.maxEntries} entries`, span);
  }
  const index = decodeCbor(indexSection);
  if (!(index.value instanceof Map) || index.end !== indexSection.length) throw bad('malformed index section');

  const parsed = new Map<string, { status: number; headers: Record<string, string>; body: Uint8Array }>();
  const entries: BundleEntry[] = [];
  let total = 0;
  for (const [url, value] of index.value) {
    if (typeof url !== 'string') throw bad('index key is not a text string');
    if (!Array.isArray(value)) throw bad(`index value for '${url}' is not an array`);
    let loc: CborValue[];
    if (isB1) {
      const variants = value[0];
      if (!(variants instanceof Uint8Array)) throw bad(`index value for '${url}' has no variants-value`);
      if (value.length !== 3) {
        throw bad(
          value.length % 2 === 1 && value.length > 3
            ? `'${url}' has multiple variants, which are not supported`
            : `index value for '${url}' has an invalid size`,
        );
      }
      loc = value.slice(1);
    } else {
      if (value.length !== 2) throw bad(`index value for '${url}' must be [offset, length]`);
      loc = value;
    }
    const [off, len] = loc;
    if (typeof off !== 'number' || typeof len !== 'number') throw bad(`index offset/length for '${url}' is not an unsigned integer`);
    if (off + len > responses.length) throw bad(`response for '${url}' lies outside the responses section`);

    const key = `${off}:${len}`;
    let r = parsed.get(key);
    if (!r) {
      r = parseResponse(bytes.subarray(responses.start + off, responses.start + off + len), url, bad);
      parsed.set(key, r);
    }
    total += r.body.byteLength;
    if (total > limits.maxTotalBytes) {
      throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: web bundle expands beyond ${limits.maxTotalBytes} bytes`, span);
    }
    entries.push({ url, status: r.status, headers: r.headers, body: r.body });
  }
  entries.sort((a, b) => (a.url < b.url ? -1 : a.url > b.url ? 1 : 0));
  return { version, primaryUrl, entries };
}

const asciiOf = (b: Uint8Array): string | undefined => {
  for (const x of b) if (x > 0x7e || x < 0x20) return undefined;
  return Buffer.from(b).toString('latin1');
};

function parseResponse(
  b: Uint8Array,
  url: string,
  bad: (why: string) => SitefileError,
): { status: number; headers: Record<string, string>; body: Uint8Array } {
  const r = decodeCbor(b, { maxItems: 16 });
  if (!Array.isArray(r.value) || r.end !== b.length || r.value.length !== 2) throw bad(`response for '${url}' is not [headers, body]`);
  const [hdrBytes, body] = r.value;
  if (!(hdrBytes instanceof Uint8Array) || !(body instanceof Uint8Array)) throw bad(`response for '${url}' must hold two byte strings`);
  const h = decodeCbor(hdrBytes, { maxItems: 100_000 });
  if (!(h.value instanceof Map) || h.end !== hdrBytes.length) throw bad(`response headers for '${url}' are not a CBOR map`);
  let status: number | undefined;
  const headers: Array<[string, string]> = [];
  for (const [k, v] of h.value) {
    if (!(k instanceof Uint8Array) || !(v instanceof Uint8Array)) throw bad(`response headers for '${url}' must map byte strings to byte strings`);
    const name = asciiOf(k);
    const value = asciiOf(v);
    if (name === undefined || value === undefined) throw bad(`non-ASCII response header for '${url}'`);
    if (name !== name.toLowerCase()) throw bad(`response header '${name}' for '${url}' is not lower-case`);
    if (name.startsWith(':')) {
      if (name !== ':status') throw bad(`unexpected pseudo-header '${name}' for '${url}'`);
      if (!/^\d{3}$/.test(value)) throw bad(`invalid :status '${value}' for '${url}'`);
      status = Number(value);
    } else {
      headers.push([name, value]);
    }
  }
  if (status === undefined) throw bad(`response for '${url}' has no :status`);
  return { status, headers: Object.fromEntries(headers), body };
}

// --------------------------------------------------------------- to a Tree

/**
 * Turn a bundle into a Tree. Only status-200 entries become files. When all
 * such URLs share one origin the artifact path is the URL path; otherwise each
 * path is prefixed with `/<host[:port]>`. A path ending in `/` maps to
 * `<path>index.html`; segments are percent-decoded.
 */
export function webBundleToTree(bundle: WebBundle, origin: string, span?: SourceSpan): Tree {
  const unsafe = (url: string, why: string): SitefileError =>
    new SitefileError('E_ARCHIVE_UNSAFE', `${origin}: unsafe web bundle URL '${url.replace(/\0/g, '\\0')}': ${why}`, span);

  interface Item { entry: BundleEntry; origin: string; host: string; path: string; query: boolean }
  const items: Item[] = [];
  for (const entry of bundle.entries) {
    if (entry.status !== 200) continue;
    const m = /^([A-Za-z][A-Za-z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/.exec(entry.url);
    if (!m) throw unsafe(entry.url, 'not an absolute http(s) URL');
    const scheme = m[1]!.toLowerCase();
    if (scheme !== 'http' && scheme !== 'https') throw unsafe(entry.url, `scheme '${scheme}:' is not allowed`);
    if (m[5] !== undefined) throw unsafe(entry.url, 'URL has a fragment');
    if (m[2]!.includes('@')) throw unsafe(entry.url, 'URL has credentials');
    let u: URL;
    try {
      u = new URL(`${scheme}://${m[2]}/`);
    } catch {
      throw unsafe(entry.url, 'invalid host');
    }
    const rawPath = m[3] === '' ? '/' : m[3]!;
    if (rawPath.includes('\\')) throw unsafe(entry.url, 'backslash in path');
    if (!rawPath.startsWith('/')) throw unsafe(entry.url, 'invalid path');
    const segs = rawPath.slice(1).split('/');
    const trailing = segs[segs.length - 1] === '';
    if (trailing) segs.pop();
    const decoded: string[] = [];
    for (const s of segs) {
      let d: string;
      try {
        d = decodeURIComponent(s);
      } catch {
        throw unsafe(entry.url, 'invalid percent-encoding in path');
      }
      if (d === '') throw unsafe(entry.url, 'empty path segment');
      if (d === '.' || d === '..') throw unsafe(entry.url, `'${d}' path segment`);
      if (d.includes('\0')) throw unsafe(entry.url, 'NUL byte in path');
      if (d.includes('\\')) throw unsafe(entry.url, 'backslash in path');
      if (d.includes('/')) throw unsafe(entry.url, 'encoded slash in path');
      decoded.push(d);
    }
    if (trailing || decoded.length === 0) decoded.push('index.html');
    items.push({ entry, origin: `${scheme}://${u.host}`, host: u.host, path: '/' + decoded.join('/'), query: m[4] !== undefined });
  }

  const multi = new Set(items.map((i) => i.origin)).size > 1;
  const files = new Map<string, { node: FileNode; url: string; query: boolean }>();
  for (const it of items) {
    const path = normalizeVirtual((multi ? `/${it.host}` : '') + it.path, span, 'web bundle URL path').path;
    const prev = files.get(path);
    if (prev && prev.url !== it.entry.url && (prev.query || it.query)) {
      throw unsafe(it.entry.url, `its query string makes it collide with '${prev.url}' at '${path}'`);
    }
    if (prev) {
      if (prev.node.digest !== makeFile(it.entry.body, '').digest) {
        throw new SitefileError('E_CONFLICT', `${origin}: '${prev.url}' and '${it.entry.url}' both map to '${path}' with different content`, span);
      }
      continue;
    }
    files.set(path, { node: makeFile(it.entry.body, `${origin}:${it.entry.url}`), url: it.entry.url, query: it.query });
  }
  return Tree.fromFiles([...files].map(([p, f]) => [p, f.node] as const));
}
