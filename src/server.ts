import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { SitefileError } from './errors.js';
import { headersFor, matchRedirect, parseHeaderRules, parseRedirectRules } from './serve-rules.js';
import type { Tree } from './tree.js';
import { nfc } from './paths.js';
import { mimeTypeFor } from './wbn.js';
import { RELOAD_CLIENT } from './reload-client.js';

export interface SiteServerOptions {
  /** Fall back to `/index.html` for unknown extension-less paths (single-page apps). */
  readonly spa?: boolean;
  /** One access-log line per request: `METHOD /path STATUS BYTES MSms`. */
  readonly log?: (line: string) => void;
  /** Sent as `x-sitefile-server` so `servers` can tell its own servers from a reused port. */
  readonly serverId?: string;
  /**
   * `serve --watch`: answer {@link RELOAD_PATH} with an event stream and {@link RELOAD_SCRIPT_PATH}
   * with the client script, and add {@link RELOAD_SCRIPT} to HTML navigations. Never set otherwise,
   * and never written to exports.
   */
  readonly liveReload?: LiveReload;
  /** Problems a person should know about (without the `warning: ` prefix), e.g. a CSP that blocks live reload. */
  readonly warn?: (message: string) => void;
}

/** Reserved for the live-reload event stream while `serve --watch` runs. */
export const RELOAD_PATH = '/__sitefile/reload';
/** Reserved for the live-reload client script ({@link RELOAD_CLIENT}) while `serve --watch` runs. */
export const RELOAD_SCRIPT_PATH = '/__sitefile/reload.js';
/** Added to navigated HTML (before the last `</body>`, else at the end) under `serve --watch`. */
export const RELOAD_SCRIPT = `<script src="${RELOAD_SCRIPT_PATH}"></script>`;
export { RELOAD_CLIENT };

/**
 * Whether a request is a page navigation, which gets the reload script: `Sec-Fetch-Mode: navigate`
 * when the browser sends it, else `Sec-Fetch-Dest` document/iframe/frame, else an `Accept` naming
 * `text/html`. A `fetch()` of an HTML fragment is none of these and is served unchanged.
 */
export function isNavigation(headers: Readonly<Record<string, string | string[] | undefined>>): boolean {
  const one = (k: string): string | undefined => {
    const v = headers[k];
    return Array.isArray(v) ? v.join(', ') : v;
  };
  const mode = one('sec-fetch-mode');
  if (mode !== undefined) return mode.trim().toLowerCase() === 'navigate';
  const dest = one('sec-fetch-dest');
  if (dest !== undefined) return ['document', 'iframe', 'frame'].includes(dest.trim().toLowerCase());
  return (one('accept') ?? '').toLowerCase().includes('text/html');
}

/**
 * The directive of a `Content-Security-Policy` value that blocks live reload: `script-src` when
 * a same-origin script cannot run, `connect-src` when the event stream cannot connect, else
 * undefined. A comma separates policies, and each one is enforced. Only `'self'`, `*`, `http:` and
 * `https:` count as allowing the server's own origin (host sources are not matched), and
 * `'strict-dynamic'` makes `'self'` ignored for scripts.
 */
export function cspBlocksReload(value: string): 'script-src' | 'connect-src' | undefined {
  for (const policy of value.split(',')) {
    const directives = new Map<string, string[]>();
    for (const d of policy.split(';')) {
      const [name, ...sources] = d.trim().split(/[\t\n\f\r ]+/).filter((t) => t !== '');
      if (name !== undefined && !directives.has(name.toLowerCase())) directives.set(name.toLowerCase(), sources.map((t) => t.toLowerCase()));
    }
    const effective = (names: string[]): string[] | undefined => names.map((n) => directives.get(n)).find((v) => v !== undefined);
    const allows = (sources: string[] | undefined, script: boolean): boolean =>
      sources === undefined ||
      (!(script && sources.includes("'strict-dynamic'")) && sources.some((t) => t === "'self'" || t === '*' || t === 'http:' || t === 'https:'));
    if (!allows(effective(['script-src-elem', 'script-src', 'default-src']), true)) return 'script-src';
    if (!allows(effective(['connect-src', 'default-src']), false)) return 'connect-src';
  }
  return undefined;
}

/** What the client script does besides reloading (`serve --watch` flags). */
export interface LiveReloadOptions {
  /** Unregister the page's service workers and empty its Cache Storage before each reload (default true; `--keep-service-workers` turns it off). */
  readonly resetServiceWorkers?: boolean;
}

/** The script served at {@link RELOAD_SCRIPT_PATH}: {@link RELOAD_CLIENT} called with `options` (keys in a fixed order). */
export function clientScript(options: LiveReloadOptions = {}): string {
  return `${RELOAD_CLIENT}({"resetServiceWorkers":${options.resetServiceWorkers !== false}});\n`;
}

/** Open live-reload streams; {@link LiveReload.reload} tells every page to reload. */
export class LiveReload {
  private readonly clients = new Set<http.ServerResponse>();
  /** The client script for this session ({@link clientScript}). */
  readonly script: string;

  constructor(options: LiveReloadOptions = {}) {
    this.script = clientScript(options);
  }

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
 * Honours `/404.html`, `/_redirects` and `/_headers` (which it never serves), and answers
 * `/about` with `/about.html` when there is no such file or directory.
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

  // One warning per policy text: a site usually sends the same CSP on every page.
  const warnedCsp = new Set<string>();
  const warnCsp = (served: string, policy: string | undefined): void => {
    if (policy === undefined || options.warn === undefined || warnedCsp.has(policy)) return;
    const blocked = cspBlocksReload(policy);
    if (blocked === undefined) return;
    warnedCsp.add(policy);
    const what = blocked === 'script-src' ? `scripts from 'self' (${RELOAD_SCRIPT_PATH})` : `connections to 'self' (${RELOAD_PATH})`;
    options.warn(`live reload: the Content-Security-Policy that _headers sets for ${served} does not allow ${what}, so open pages will not reload: ${policy}`);
  };

  const handler = (req: http.IncomingMessage, res: http.ServerResponse): void => {
    const started = process.hrtime.bigint();
    const method = req.method ?? 'GET';
    const target = req.url ?? '/';
    const url = originForm(target);
    let bytes = 0;
    const finish = (status: number, headers: Record<string, string | number>, body: Uint8Array | string | undefined, quiet = false): void => {
      const buf = body === undefined ? undefined : typeof body === 'string' ? Buffer.from(body) : body;
      res.writeHead(status, { ...base, ...headers });
      if (buf !== undefined && method !== 'HEAD' && status !== 304) {
        bytes = buf.byteLength;
        res.end(buf);
      } else {
        res.end();
      }
      const ms = Number((process.hrtime.bigint() - started) / 1000n) / 1000;
      if (quiet) return;
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
    if (options.liveReload && rawPath === RELOAD_SCRIPT_PATH) {
      // not access-logged either: every page load fetches it
      const js = options.liveReload.script;
      return finish(200, { 'content-type': 'text/javascript; charset=utf-8', 'content-length': Buffer.byteLength(js) }, js, true);
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
      const live = options.liveReload !== undefined && type.startsWith('text/html') && isNavigation(req.headers);
      if (live) warnCsp(served, custom['content-security-policy']);
      const body = live ? injectReloadScript(file.bytes) : file.bytes;
      if (status !== 200) return finish(status, { 'content-type': type, 'content-length': body.byteLength, ...custom }, body);
      // An injected page is a different representation of the file: give it its own ETag.
      const etag = `"${file.digest.replace(/^sha256:/, '')}${live ? '-live' : ''}"`;
      if (etagMatches(req.headers['if-none-match'], etag)) return finish(304, { ...custom, etag }, undefined);
      finish(200, { 'content-type': type, 'content-length': body.byteLength, ...custom, etag }, body);
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
