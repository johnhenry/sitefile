import { createHash } from 'node:crypto';
import { constants, gunzipSync } from 'node:zlib';
import { gzip } from './deflate.js';
import { SitefileError, type SourceSpan } from './errors.js';
import { encodePath, percentDecode } from './links.js';
import { headersFor, parseHeaderRules } from './serve-rules.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits } from './resources/types.js';
import { makeFile, Tree, type FileNode } from './tree.js';
import { mimeTypeFor, validateBaseUrl } from './wbn.js';

/**
 * WARC (ISO 28500, WARC/1.1) export and import (issue #206).
 *
 * `EXPORT / AS warc BASE <url>` writes a `warcinfo` record, then one `response` record per file (path
 * order) at `<base><path>`: `HTTP/1.1 200 OK`, the Content-Type from the Web Bundle MIME table (or the
 * site's `_headers`), Content-Length, and every `_headers` rule that matches the path. `_headers` and
 * `_redirects` are site configuration, not records. A directory's `index.html` is recorded a second
 * time at the directory URL (`<base>sub/`), as a replay tool requests it. Record IDs are UUIDs derived
 * from the record's content and every record is dated `SOURCE_DATE_EPOCH` (default the Unix epoch), so
 * the file is deterministic. `COMPRESSION gzip` (or `-o x.warc.gz`) writes each record as its own gzip
 * member, the usual `.warc.gz` layout.
 *
 * Reading (`IMPORT … AS tree`, `serve`): the `200` responses of the first response's host; a URL path
 * ending in `/` is that directory's `index.html`; the first record for a path wins. Paths are checked
 * like archive entries and the archive limits apply.
 */

const enc = new TextEncoder();
const CRLF = '\r\n';
const DATE_ZERO = '1970-01-01T00:00:00Z';
const SKIP = new Set(['/_headers', '/_redirects']);

function sha256Hex(data: Uint8Array | string): string {
  return createHash('sha256').update(data).digest('hex');
}

/** A deterministic `urn:uuid:` (UUID version 8) from the hash of `seed`. */
function recordId(seed: string): string {
  const h = sha256Hex(seed);
  const v = '8' + h.slice(13, 16);
  const variant = ((parseInt(h.slice(16, 17), 16) & 3) | 8).toString(16) + h.slice(17, 20);
  return `<urn:uuid:${h.slice(0, 8)}-${h.slice(8, 12)}-${v}-${variant}-${h.slice(20, 32)}>`;
}

export function isoDate(epoch: number | undefined): string {
  if (epoch === undefined) return DATE_ZERO;
  return new Date(epoch * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

function concat(parts: readonly Uint8Array[]): Uint8Array {
  let n = 0;
  for (const p of parts) n += p.byteLength;
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.byteLength;
  }
  return out;
}

function record(fields: ReadonlyArray<readonly [string, string]>, block: Uint8Array): Uint8Array {
  const head = ['WARC/1.1', ...fields.map(([k, v]) => `${k}: ${v}`), `Content-Length: ${block.byteLength}`, '', ''].join(CRLF);
  return concat([enc.encode(head), block, enc.encode(CRLF + CRLF)]);
}

export interface WarcOptions {
  readonly baseUrl?: string | undefined;
  /** SOURCE_DATE_EPOCH: the WARC-Date of every record. */
  readonly epoch?: number | undefined;
  readonly compression?: 'gzip' | 'none' | undefined;
}

/** One WARC record and, for a response, what an index needs (WACZ). */
export interface WarcRecord {
  readonly bytes: Uint8Array;
  readonly response?: { readonly uri: string; readonly path: string; readonly type: string; readonly digest: string };
}

/** `EXPORT … AS warc`: the tree as WARC/1.1 records (see the top of this file). */
export function writeWarc(tree: Tree, options: WarcOptions): Uint8Array {
  const records = warcRecords(tree, options).map((r) => r.bytes);
  return options.compression === 'gzip' ? concat(records.map((r) => gzip(r))) : concat(records);
}

/** The records {@link writeWarc} writes, in order. */
export function warcRecords(tree: Tree, options: WarcOptions): WarcRecord[] {
  if (options.baseUrl === undefined || options.baseUrl === '') {
    throw new SitefileError('E_VALIDATION', "warc export needs a base URL: use 'EXPORT <path> AS warc BASE https://example.com/' or --base-url");
  }
  const base = validateBaseUrl(options.baseUrl);
  const date = isoDate(options.epoch);
  const rules = parseHeaderRules(new TextDecoder().decode(tree.get('/_headers')?.bytes ?? new Uint8Array(0)));
  const records: WarcRecord[] = [];
  const info = enc.encode(`software: sitefile${CRLF}format: WARC File Format 1.1${CRLF}`);
  const infoId = recordId(`warcinfo\0${base}`);
  records.push({
    bytes: record(
      [
        ['WARC-Type', 'warcinfo'],
        ['WARC-Record-ID', infoId],
        ['WARC-Date', date],
        ['Content-Type', 'application/warc-fields'],
      ],
      info,
    ),
  });
  const response = (uri: string, path: string, node: FileNode): WarcRecord => {
    const headers = headersFor(rules, path);
    const type = headers['content-type'] ?? mimeTypeFor(path);
    const lines = ['HTTP/1.1 200 OK', `Content-Type: ${type}`, `Content-Length: ${node.bytes.byteLength}`];
    for (const [k, v] of Object.entries(headers)) if (k !== 'content-type') lines.push(`${k}: ${v}`);
    const block = concat([enc.encode(lines.join(CRLF) + CRLF + CRLF), node.bytes]);
    const blockDigest = `sha256:${sha256Hex(block)}`;
    const bytes = record(
      [
        ['WARC-Type', 'response'],
        ['WARC-Record-ID', recordId(`response\0${uri}\0${blockDigest}`)],
        ['WARC-Warcinfo-ID', infoId],
        ['WARC-Date', date],
        ['WARC-Target-URI', uri],
        ['WARC-Payload-Digest', `sha256:${sha256Hex(node.bytes)}`],
        ['WARC-Block-Digest', blockDigest],
        ['Content-Type', 'application/http;msgtype=response'],
      ],
      block,
    );
    return { bytes, response: { uri, path, type, digest: `sha256:${sha256Hex(node.bytes)}` } };
  };
  for (const [p, n] of tree.entries()) {
    if (SKIP.has(p)) continue;
    records.push(response(base + encodePath(p.slice(1)), p, n));
    if (p === '/index.html' || p.endsWith('/index.html')) {
      const dir = p.slice(0, -'index.html'.length);
      records.push(response(base + encodePath(dir.slice(1)), p, n));
    }
  }
  return records;
}

/** True when `bytes` start a WARC file, plain or gzipped (the gzip is inflated to check). */
export function looksLikeWarc(bytes: Uint8Array): boolean {
  const magic = (b: Uint8Array): boolean => b.byteLength >= 5 && new TextDecoder().decode(b.subarray(0, 5)) === 'WARC/';
  if (magic(bytes)) return true;
  if (bytes[0] !== 0x1f || bytes[1] !== 0x8b) return false;
  try {
    // Inflate only the start of the stream (a truncated prefix, flushed): enough to see the magic.
    return magic(new Uint8Array(gunzipSync(bytes.subarray(0, 4096), { finishFlush: constants.Z_SYNC_FLUSH })));
  } catch {
    return false;
  }
}

function inflate(bytes: Uint8Array, origin: string, limits: ArchiveLimits, span?: SourceSpan): Uint8Array {
  const cap = limits.maxTotalBytes + limits.maxEntries * 4096 + 65536;
  let out: Uint8Array;
  try {
    out = new Uint8Array(gunzipSync(bytes, { maxOutputLength: cap }));
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === 'ERR_BUFFER_TOO_LARGE') throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: WARC expands beyond ${cap} bytes`, span);
    throw new SitefileError('E_ARCHIVE', `${origin}: corrupt gzip in WARC`, span);
  }
  if (out.byteLength > 1024 * 1024 && out.byteLength > bytes.byteLength * limits.maxRatio) {
    throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: WARC compression ratio exceeds ${limits.maxRatio}`, span);
  }
  return out;
}

function indexOf(b: Uint8Array, pat: Uint8Array, from: number): number {
  outer: for (let i = from; i + pat.length <= b.length; i++) {
    for (let k = 0; k < pat.length; k++) if (b[i + k] !== pat[k]) continue outer;
    return i;
  }
  return -1;
}

const BLANK = enc.encode('\r\n\r\n');
const latin1 = new TextDecoder('latin1');

/** Header lines (`Name: value`, names lower-cased) of a CRLF-separated head. */
function parseFields(lines: readonly string[]): Map<string, string> {
  const m = new Map<string, string>();
  for (const l of lines) {
    const i = l.indexOf(':');
    if (i <= 0) continue;
    const k = l.slice(0, i).trim().toLowerCase();
    if (!m.has(k)) m.set(k, l.slice(i + 1).trim());
  }
  return m;
}

/** `scheme://host/path` → [host, path] (lower-case host, path without query or fragment). */
function splitUrl(uri: string): [string, string] | undefined {
  const m = /^https?:\/\/([^/?#]*)([^?#]*)/i.exec(uri);
  if (!m) return undefined;
  return [(m[1] as string).toLowerCase(), (m[2] as string) === '' ? '/' : (m[2] as string)];
}

/** The files of a WARC (plain or gzipped): see the top of this file. */
export function readWarc(input: Uint8Array, origin: string, limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS, span?: SourceSpan): Tree {
  const bytes = input[0] === 0x1f && input[1] === 0x8b ? inflate(input, origin, limits, span) : input;
  const bad = (why: string): SitefileError => new SitefileError('E_ARCHIVE', `${origin}: ${why}`, span);
  const files = new Map<string, FileNode>();
  let host: string | undefined;
  let total = 0;
  let at = 0;
  while (at < bytes.length) {
    // Records are separated by CRLF CRLF; tolerate extra blank lines between them.
    while (at < bytes.length && (bytes[at] === 0x0d || bytes[at] === 0x0a)) at++;
    if (at >= bytes.length) break;
    const headEnd = indexOf(bytes, BLANK, at);
    if (headEnd === -1) throw bad('truncated WARC record header');
    const lines = latin1.decode(bytes.subarray(at, headEnd)).split('\r\n');
    if (!/^WARC\/1\.[01]$/.test(lines[0] ?? '')) throw bad(`not a WARC record at byte ${at}`);
    const fields = parseFields(lines.slice(1));
    const lenText = fields.get('content-length') ?? '';
    if (!/^[0-9]{1,15}$/.test(lenText)) throw bad('WARC record without a valid Content-Length');
    const start = headEnd + 4;
    const end = start + Number(lenText);
    if (end > bytes.length) throw bad('truncated WARC record');
    at = end;
    if (fields.get('warc-type') !== 'response' || !(fields.get('content-type') ?? '').toLowerCase().startsWith('application/http')) continue;
    const target = splitUrl(fields.get('warc-target-uri') ?? '');
    if (!target) continue;
    const block = bytes.subarray(start, end);
    const httpEnd = indexOf(block, BLANK, 0);
    if (httpEnd === -1) continue;
    const status = latin1.decode(block.subarray(0, Math.min(httpEnd, 64))).split('\r\n')[0] ?? '';
    if (!/^HTTP\/\d(\.\d)? 200( |$)/.test(status)) continue;
    const [h, rawPath] = target;
    host ??= h;
    if (h !== host) continue;
    const segs = rawPath.split('/').slice(1).map(percentDecode);
    if (segs[segs.length - 1] === '') segs[segs.length - 1] = 'index.html';
    for (const s of segs) {
      if (s === '' || s === '.' || s === '..' || s.includes('\0') || s.includes('\\') || s.includes('/')) {
        throw new SitefileError('E_ARCHIVE_UNSAFE', `${origin}: unsafe path in WARC record for ${fields.get('warc-target-uri')}`, span);
      }
    }
    const p = '/' + segs.join('/');
    if (files.has(p)) continue;
    const body = block.subarray(httpEnd + 4);
    total += body.byteLength;
    if (files.size + 1 > limits.maxEntries) throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: more than ${limits.maxEntries} entries`, span);
    if (total > limits.maxTotalBytes) throw new SitefileError('E_ARCHIVE_LIMIT', `${origin}: WARC expands beyond ${limits.maxTotalBytes} bytes`, span);
    files.set(p, makeFile(new Uint8Array(body), `${origin}`));
  }
  try {
    return Tree.fromFiles(files);
  } catch (e) {
    if (e instanceof SitefileError) throw new SitefileError('E_ARCHIVE_UNSAFE', `${origin}: ${e.message}`, span, { cause: e });
    throw e;
  }
}
