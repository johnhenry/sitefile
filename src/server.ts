import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { SitefileError } from './errors.js';
import { headersFor, matchRedirect, parseHeaderRules, parseRedirectRules } from './serve-rules.js';
import type { Tree } from './tree.js';
import { nfc } from './paths.js';
import { mimeTypeFor } from './wbn.js';

export interface SiteServerOptions {
  /** Fall back to `/index.html` for unknown extension-less paths (single-page apps). */
  readonly spa?: boolean;
  /** One access-log line per request: `METHOD /path STATUS BYTES MSms`. */
  readonly log?: (line: string) => void;
  /** Sent as `x-sitefile-server` so `servers` can tell its own servers from a reused port. */
  readonly serverId?: string;
  /**
   * `serve --watch`: answer {@link RELOAD_PATH} with an event stream and add
   * {@link RELOAD_SCRIPT} to HTML responses. Never set otherwise, and never written to exports.
   */
  readonly liveReload?: LiveReload;
  /**
   * When the served files last changed (Unix seconds), read per request: sent as `Last-Modified` on
   * file responses, and `If-Modified-Since` (without `If-None-Match`) gives `304`. Unset: neither.
   */
  readonly lastModified?: () => number | undefined;
}

/** Reserved for the live-reload event stream while `serve --watch` runs. */
export const RELOAD_PATH = '/__sitefile/reload';
/** Added to served HTML (before the last `</body>`, else at the end) under `serve --watch`. */
export const RELOAD_SCRIPT = `<script>new EventSource("${RELOAD_PATH}").onmessage=function(){location.reload()}</script>`;

/** Open live-reload streams; {@link LiveReload.reload} tells every page to reload. */
export class LiveReload {
  private readonly clients = new Set<http.ServerResponse>();

  /** Hold `res` open as a text/event-stream until the client goes away. */
  attach(res: http.ServerResponse, headers: Record<string, string>): void {
    res.writeHead(200, { ...headers, 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    res.write('retry: 1000\n\n');
    this.clients.add(res);
    res.on('close', () => this.clients.delete(res));
  }

  reload(): void {
    for (const c of this.clients) c.write('data: reload\n\n');
  }

  get size(): number {
    return this.clients.size;
  }
}

/** `html` with {@link RELOAD_SCRIPT} inserted before the last `</body` (ASCII case-insensitive), else appended. */
export function injectReloadScript(html: Uint8Array): Uint8Array {
  const buf = Buffer.from(html.buffer, html.byteOffset, html.byteLength);
  const at = buf.toString('latin1').toLowerCase().lastIndexOf('</body');
  const script = Buffer.from(RELOAD_SCRIPT);
  return at === -1 ? Buffer.concat([buf, script]) : Buffer.concat([buf.subarray(0, at), script, buf.subarray(at)]);
}

const NOT_FOUND_PAGE = '<!doctype html><meta charset="utf-8"><title>404 Not Found</title><h1>404 Not Found</h1>\n';
const BAD_REQUEST_PAGE = '<!doctype html><meta charset="utf-8"><title>400 Bad Request</title><h1>400 Bad Request</h1>\n';
const METHOD_PAGE = '<!doctype html><meta charset="utf-8"><title>405 Method Not Allowed</title><h1>405 Method Not Allowed</h1>\n';

/** Decode a request path into safe segments, or undefined when it is malformed or unsafe. */
export function safeSegments(rawPath: string): string[] | undefined {
  if (!rawPath.startsWith('/') || rawPath.includes('\\') || rawPath.includes('\0')) return undefined;
  const out: string[] = [];
  for (const raw of rawPath.split('/')) {
    if (raw === '') continue;
    let seg: string;
    try {
      seg = decodeURIComponent(raw);
    } catch {
      return undefined;
    }
    // A decoded '/', '\\', NUL, control character, '.' or '..' could change what is read.
    if (seg === '.' || seg === '..' || /[\\/\u0000-\u001f\u007f]/.test(seg)) return undefined;
    // Tree paths are NFC; a request for an NFD spelling finds the same file.
    out.push(nfc(seg));
  }
  return out;
}

/**
 * The origin form of a request target: an absolute-form target (`http://host/path?q`, RFC 9112
 * section 3.2.2, sent by proxies and some clients) loses its scheme and authority (`/path?q`, or
 * `/` for an empty path); any other target is returned as it is. The authority is not checked.
 */
export function originForm(target: string): string {
  const m = /^https?:\/\/[^/?#]*/i.exec(target);
  if (!m) return target;
  const rest = target.slice(m[0].length);
  return rest.startsWith('/') ? rest : '/' + rest;
}

const DAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`, RFC 9110 section 5.6.7) for Unix seconds. */
export function httpDate(secs: number): string {
  return new Date(secs * 1000).toUTCString();
}

/** Unix seconds of an IMF-fixdate, or undefined for anything else (the obsolete formats too). */
export function parseHttpDate(value: string): number | undefined {
  const m = /^(Mon|Tue|Wed|Thu|Fri|Sat|Sun), ([0-9]{2}) (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) ([0-9]{4}) ([0-9]{2}):([0-9]{2}):([0-9]{2}) GMT$/.exec(value.trim());
  if (!m) return undefined;
  const [day, month, year, h, min, sec] = [Number(m[2]), MONTHS.indexOf(m[3]!), Number(m[4]), Number(m[5]), Number(m[6]), Number(m[7])];
  if (day < 1 || h > 23 || min > 59 || sec > 59) return undefined;
  const ms = Date.UTC(year, month, day, h, min, sec);
  const d = new Date(ms);
  if (d.getUTCDate() !== day || DAYS[d.getUTCDay()] !== m[1]) return undefined;
  return ms / 1000;
}

/** One link-value as Node's `writeEarlyHints` accepts it: `<uri>` and `; param[=value]` pairs. */
const LINK_VALUE = /^(?:<[^>\r\n]*>)(?:\s*;\s*[^;"\s]+(?:=(")?[^;"\s]*\1)?)*$/;

/**
 * The link-values of a `Link` header that become `103 Early Hints`: split at commas outside `<…>`
 * and quotes, kept when well formed and `rel=preload` or `rel=preconnect` (ASCII case-insensitive).
 */
export function earlyHints(link: string): string[] {
  const parts: string[] = [];
  let cur = '';
  let angle = false;
  let quote = false;
  for (const c of link) {
    if (c === '<' && !quote) angle = true;
    else if (c === '>' && !quote) angle = false;
    else if (c === '"' && !angle) quote = !quote;
    if (c === ',' && !angle && !quote) {
      parts.push(cur);
      cur = '';
    } else cur += c;
  }
  parts.push(cur);
  return parts
    .map((p) => p.trim())
    .filter((p) => {
      if (!LINK_VALUE.test(p)) return false;
      const v = p.replace(/[A-Z]/g, (c) => c.toLowerCase()).replace(/[ \t]/g, '');
      return ['rel=preload', 'rel="preload"', 'rel=preconnect', 'rel="preconnect"'].some((t) => v.includes(t));
    });
}

/** A `bytes=` value's number: digits only; more than 15 digits counts as larger than any file. */
function rangeNumber(s: string): number | undefined {
  if (!/^[0-9]+$/.test(s)) return undefined;
  return s.length > 15 ? Number.POSITIVE_INFINITY : Number(s);
}

/**
 * The byte range a `Range` header asks of a body of `size` bytes: `[start, end]` (inclusive),
 * `'unsatisfiable'` (416), or undefined when the header is absent, malformed, not in bytes, or
 * asks for several ranges (those are ignored: the whole body is sent, as RFC 9110 allows).
 */
export function byteRange(header: string | undefined, size: number): [number, number] | 'unsatisfiable' | undefined {
  if (header === undefined) return undefined;
  const v = header.trim();
  if (v.slice(0, 6).toLowerCase() !== 'bytes=') return undefined;
  const spec = v.slice(6).trim();
  if (spec.includes(',')) return undefined;
  const dash = spec.indexOf('-');
  if (dash === -1) return undefined;
  const first = spec.slice(0, dash).trim();
  const last = spec.slice(dash + 1).trim();
  if (first === '') {
    const n = rangeNumber(last);
    if (n === undefined) return undefined;
    if (n === 0 || size === 0) return 'unsatisfiable';
    return [Math.max(0, size - n), size - 1];
  }
  const start = rangeNumber(first);
  if (start === undefined) return undefined;
  const end = last === '' ? Number.POSITIVE_INFINITY : rangeNumber(last);
  if (end === undefined || end < start) return undefined;
  if (start >= size) return 'unsatisfiable';
  return [start, Math.min(end, size - 1)];
}

function etagMatches(header: string | undefined, etag: string): boolean {
  if (header === undefined) return false;
  if (header.trim() === '*') return true;
  return header.split(',').some((t) => t.trim().replace(/^W\//, '') === etag);
}

function printable(s: string): string {
  return s.replace(/[^\x21-\x7e]/g, (c) => `%${c.charCodeAt(0).toString(16).padStart(2, '0')}`).slice(0, 512);
}

/**
 * An HTTP server for an immutable Tree. GET and HEAD only; nothing outside the tree is
 * ever reachable because lookups are exact map keys built from validated segments.
 * Honours `/404.html`, `/_redirects` and `/_headers` (which it never serves), answers
 * `/about` with `/about.html` when there is no such file or directory, and redirects `/about/`
 * to `/about` when only `/about.html` answers it.
 */
export function createSiteServer(site: Tree | (() => Tree), options: SiteServerOptions = {}): http.Server {
  // A function is read per request (`serve --watch` swaps in each good rebuild).
  const current = typeof site === 'function' ? site : (): Tree => site;
  const base: Record<string, string> = { 'x-content-type-options': 'nosniff', 'cache-control': 'no-cache' };
  if (options.serverId) base['x-sitefile-server'] = options.serverId;
  // `_redirects` / `_headers` are parsed once per tree (a watched server gets a new tree per rebuild).
  let rulesTree: Tree | undefined;
  let rules: { redirects: ReturnType<typeof parseRedirectRules>; headers: ReturnType<typeof parseHeaderRules> } = { redirects: [], headers: [] };
  const rulesFor = (t: Tree): typeof rules => {
    if (t !== rulesTree) {
      const text = (p: string): string => {
        const f = t.get(p);
        return f ? new TextDecoder().decode(f.bytes) : '';
      };
      rules = { redirects: parseRedirectRules(text('/_redirects')), headers: parseHeaderRules(text('/_headers')) };
      rulesTree = t;
    }
    return rules;
  };

  const handler = (req: http.IncomingMessage, res: http.ServerResponse): void => {
    const started = process.hrtime.bigint();
    const method = req.method ?? 'GET';
    const target = req.url ?? '/';
    const url = originForm(target);
    let bytes = 0;
    const finish = (status: number, headers: Record<string, string | number>, body: Uint8Array | string | undefined): void => {
      const buf = body === undefined ? undefined : typeof body === 'string' ? Buffer.from(body) : body;
      // 103 Early Hints before an HTML page whose `Link` (from `_headers`) preloads or preconnects.
      const link = headers.link;
      const html = String(headers['content-type'] ?? '').startsWith('text/html');
      if (status === 200 && method === 'GET' && html && typeof link === 'string' && (req.httpVersionMajor > 1 || req.httpVersionMinor >= 1)) {
        const hints = earlyHints(link);
        if (hints.length > 0) res.writeEarlyHints({ link: hints });
      }
      res.writeHead(status, { ...base, ...headers });
      if (buf !== undefined && method !== 'HEAD' && status !== 304) {
        bytes = buf.byteLength;
        res.end(buf);
      } else {
        res.end();
      }
      const ms = Number((process.hrtime.bigint() - started) / 1000n) / 1000;
      // `servers` probes carry the server's own id; they would only clutter the access log.
      if (options.serverId !== undefined && req.headers['x-sitefile-probe'] === options.serverId) return;
      options.log?.(`${method} ${printable(target)} ${status} ${bytes} ${ms.toFixed(1)}ms`);
    };
    const page = (status: number, html: string, extra: Record<string, string> = {}): void =>
      finish(status, { 'content-type': 'text/html; charset=utf-8', 'content-length': Buffer.byteLength(html), ...extra }, html);

    if (method !== 'GET' && method !== 'HEAD') return page(405, METHOD_PAGE, { allow: 'GET, HEAD' });
    const tree = current();
    const { redirects, headers: headerRules } = rulesFor(tree);
    const isFile = (p: string): boolean => tree.isFile(p) && p !== '/_redirects' && p !== '/_headers';

    const q = url.indexOf('?');
    const hash = url.indexOf('#');
    const end = Math.min(q === -1 ? url.length : q, hash === -1 ? url.length : hash);
    const rawPath = url.slice(0, end);
    if (options.liveReload && rawPath === RELOAD_PATH) {
      if (method === 'HEAD') return finish(200, { 'content-type': 'text/event-stream' }, undefined);
      return options.liveReload.attach(res, base); // not access-logged: one long request per open page
    }
    const search = q === -1 ? '' : url.slice(q, hash === -1 || hash < q ? undefined : hash);
    const segs = safeSegments(rawPath);
    if (!segs) return page(400, BAD_REQUEST_PAGE);
    const trailingSlash = rawPath.endsWith('/');
    const vpath = '/' + segs.join('/');
    // `_headers` patterns match the request path (with its trailing slash).
    const custom = headersFor(headerRules, vpath + (trailingSlash && segs.length > 0 ? '/' : ''));

    const sendFile = (status: number, served: string): void => {
      const file = tree.get(served)!;
      const type = mimeTypeFor(served);
      const live = options.liveReload !== undefined && type.startsWith('text/html');
      const body = live ? injectReloadScript(file.bytes) : file.bytes;
      if (status !== 200) return finish(status, { 'content-type': type, 'content-length': body.byteLength, ...custom }, body);
      // An injected page is a different representation of the file: give it its own ETag.
      const etag = `"${file.digest.replace(/^sha256:/, '')}${live ? '-live' : ''}"`;
      const modified = options.lastModified?.();
      const lm: Record<string, string> = modified === undefined ? {} : { 'last-modified': httpDate(modified) };
      if (etagMatches(req.headers['if-none-match'], etag)) return finish(304, { ...custom, etag, ...lm }, undefined);
      // If-Modified-Since only counts without If-None-Match (RFC 9110 section 13.2.2).
      const ims = req.headers['if-none-match'] === undefined ? req.headers['if-modified-since'] : undefined;
      const since = ims === undefined ? undefined : parseHttpDate(ims);
      if (modified !== undefined && since !== undefined && modified <= since) return finish(304, { ...custom, etag, ...lm }, undefined);
      // A single byte range of a GET (`If-Range` must name this ETag); anything else gets the whole file.
      const ifRange = req.headers['if-range'] as string | undefined;
      const range = method === 'GET' && (ifRange === undefined || ifRange.trim() === etag) ? byteRange(req.headers.range, body.byteLength) : undefined;
      if (range === 'unsatisfiable') {
        return finish(416, { ...custom, 'accept-ranges': 'bytes', 'content-range': `bytes */${body.byteLength}`, 'content-length': 0 }, undefined);
      }
      if (range !== undefined) {
        const [start, end] = range;
        const part = body.subarray(start, end + 1);
        return finish(
          206,
          { 'content-type': type, 'content-length': part.byteLength, ...custom, etag, ...lm, 'accept-ranges': 'bytes', 'content-range': `bytes ${start}-${end}/${body.byteLength}` },
          part,
        );
      }
      finish(200, { 'content-type': type, 'content-length': body.byteLength, ...custom, etag, ...lm, 'accept-ranges': 'bytes' }, body);
    };
    /** The file for a path: exact, `<path>.html` (clean URLs), or `<dir>/index.html` with a trailing slash. */
    const resolve = (p: string, slash: boolean): string | undefined => {
      if (isFile(p) && !(slash && p !== '/')) return p;
      if (!slash && p !== '/' && isFile(`${p}.html`)) return `${p}.html`;
      if (slash && tree.isDirectory(p)) {
        const index = p === '/' ? '/index.html' : `${p}/index.html`;
        if (isFile(index)) return index;
      }
      return undefined;
    };
    /** Apply the first matching `_redirects` rule (forced ones, or the others); false when none applies. */
    const redirect = (forced: boolean): boolean => {
      for (const r of redirects) {
        if (r.force !== forced) continue;
        const rest = matchRedirect(r, vpath);
        if (rest === undefined) continue;
        const target = r.splat ? r.to.split(':splat').join(rest.split('/').map(encodeURIComponent).join('/')) : r.to;
        if (r.status !== 200 && r.status !== 404) {
          finish(r.status, { location: target.includes('?') ? target : target + search, 'content-length': 0 }, undefined);
          return true;
        }
        // 200 (rewrite) and 404: serve the target path's file with that status.
        const tq = target.indexOf('?');
        const tpath = tq === -1 ? target : target.slice(0, tq);
        const tsegs = safeSegments(tpath);
        const served = tsegs ? resolve('/' + tsegs.join('/'), tpath.endsWith('/')) : undefined;
        if (served === undefined) continue;
        sendFile(r.status, served);
        return true;
      }
      return false;
    };

    if (redirect(true)) return;
    let served = resolve(vpath, trailingSlash);
    if (served === undefined && !trailingSlash && tree.isDirectory(vpath)) {
      const location = '/' + segs.map(encodeURIComponent).join('/') + '/' + search;
      return finish(301, { location, 'content-length': 0 }, undefined);
    }
    if (served === undefined && redirect(false)) return;
    // `/about/` when only `/about.html` answers: redirect to the clean URL `/about` (relative links in the
    // page then resolve as they do at `/about`). `_redirects` rules for the path come first.
    if (served === undefined && trailingSlash && segs.length > 0 && isFile(`${vpath}.html`)) {
      const location = '/' + segs.map(encodeURIComponent).join('/') + search;
      return finish(301, { location, 'content-length': 0 }, undefined);
    }
    if (served === undefined && options.spa && !/\.[^/.]+$/.test(segs[segs.length - 1] ?? '') && isFile('/index.html')) served = '/index.html';
    if (served !== undefined) return sendFile(200, served);
    if (isFile('/404.html')) return sendFile(404, '/404.html');
    page(404, NOT_FOUND_PAGE, custom);
  };

  const server = http.createServer(handler);
  server.on('clientError', (_err, socket) => {
    if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n');
    else socket.destroy();
  });
  return server;
}

/** Listen on host:port (port 0 = any free port); EADDRINUSE and friends become E_SERVER. */
export function listen(server: http.Server, host: string, port: number): Promise<{ host: string; port: number }> {
  return new Promise((resolve, reject) => {
    const onError = (e: NodeJS.ErrnoException): void => {
      const why =
        e.code === 'EADDRINUSE'
          ? `port ${port} on ${host} is already in use`
          : e.code === 'EACCES'
            ? `permission denied binding ${host}:${port}`
            : `cannot listen on ${host}:${port}: ${e.message}`;
      reject(new SitefileError('E_SERVER', why, undefined, { cause: e }));
    };
    server.once('error', onError);
    server.listen(port, host, () => {
      server.off('error', onError);
      resolve({ host, port: (server.address() as AddressInfo).port });
    });
  });
}

export function closeServer(server: http.Server): Promise<void> {
  return new Promise((resolve) => {
    server.close(() => resolve());
    server.closeAllConnections();
  });
}

export function formatUrl(host: string, port: number): string {
  const shown = host === '0.0.0.0' || host === '::' ? '127.0.0.1' : host;
  return `http://${shown.includes(':') ? `[${shown}]` : shown}:${port}/`;
}
