import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { SitefileError } from './errors.js';
import { headersFor, matchRedirect, parseHeaderRules, parseRedirectRules } from './serve-rules.js';
import { deltaFor } from './dictionary.js';
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

/** Heartbeat interval on the event stream. */
export const HEARTBEAT_MS = 15_000;
/** Headers of the event stream (also on HEAD): no proxy buffering (`X-Accel-Buffering`), no compression or other transforms. */
export const STREAM_HEADERS: Readonly<Record<string, string>> = {
  'content-type': 'text/event-stream',
  'cache-control': 'no-cache, no-transform',
  'x-accel-buffering': 'no',
};

/** What the client script does besides reloading (`serve --watch` flags). */
export interface LiveReloadOptions {
  /** Unregister the page's service workers and empty its Cache Storage before each reload (default true; `--keep-service-workers` turns it off). */
  readonly resetServiceWorkers?: boolean;
  /** Swap changed stylesheets in place instead of reloading when only CSS changed (default true; `--no-css-inject` turns it off). */
  readonly cssInject?: boolean;
  /** Forward the page's `console.log`/`console.info` too, not only errors and warnings (`--console`). */
  readonly console?: boolean;
  /** Show a Rebuilding badge and a build-error overlay in the page (default true; `--no-overlay` turns it off). */
  readonly overlay?: boolean;
  /** After a rebuild that changed exactly one page, go to that page (`--navigate-to-changed`; default false). */
  readonly navigateToChanged?: boolean;
  /** `--reload`: `morph` (default) updates a page in place when only HTML changed; `full` always reloads. */
  readonly reload?: 'full' | 'morph';
}

/** The script served at {@link RELOAD_SCRIPT_PATH}: {@link RELOAD_CLIENT} called with `options` (keys in a fixed order). */
export function clientScript(options: LiveReloadOptions = {}): string {
  return `${RELOAD_CLIENT}({"resetServiceWorkers":${options.resetServiceWorkers !== false},"cssInject":${options.cssInject !== false},"console":${options.console === true},"overlay":${options.overlay !== false},"navigateToChanged":${options.navigateToChanged === true},"reload":"${options.reload ?? 'morph'}"});\n`;
}

/** Reserved for browser console messages (POST) while `serve --watch` runs. */
export const CONSOLE_PATH = '/__sitefile/console';
/** Largest console message body accepted (bytes). */
export const MAX_CONSOLE_BODY = 16 * 1024;
/** At most this many console messages are logged per {@link CONSOLE_WINDOW_MS}. */
export const CONSOLE_LIMIT = 50;
export const CONSOLE_WINDOW_MS = 10_000;

/** `s` with each run of control characters made one space, trimmed, and cut to `max` code points (then `...`). */
function cleanText(s: string, max: number): string {
  const t = [...s.replace(/[\u0000-\u001f\u007f]+/g, ' ').trim()];
  return t.length > max ? t.slice(0, max).join('') + '...' : t.join('');
}

/**
 * The server-log lines for one console message body (JSON `{level, message, stack, url, line, col}`),
 * or undefined when it is not one. `level` is error, warn, log or info; log and info give no lines
 * unless `all` (`--console`). The first line is `browser <level>: <message> (<url>:<line>:<col>)`;
 * up to 5 stack lines follow, indented.
 */
export function consoleLines(body: string, all: boolean): string[] | undefined {
  let v: unknown;
  try {
    v = JSON.parse(body);
  } catch {
    return undefined;
  }
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return undefined;
  const o = v as Record<string, unknown>;
  const level = o.level;
  if (level !== 'error' && level !== 'warn' && level !== 'log' && level !== 'info') return undefined;
  if ((level === 'log' || level === 'info') && !all) return [];
  const message = cleanText(typeof o.message === 'string' ? o.message : '', 1000);
  const num = (x: unknown): number | undefined => (typeof x === 'number' && Number.isInteger(x) && x >= 0 && x <= Number.MAX_SAFE_INTEGER ? x : undefined);
  const url = typeof o.url === 'string' ? cleanText(o.url, 300) : '';
  const line = num(o.line);
  const col = line === undefined ? undefined : num(o.col);
  const at = url === '' ? '' : ` (${url}${line === undefined ? '' : `:${line}`}${col === undefined ? '' : `:${col}`})`;
  const out = [`browser ${level}: ${message}${at}`];
  if (typeof o.stack === 'string') {
    const frames = o.stack
      .split('\n')
      .map((l) => cleanText(l, 300))
      .filter((l) => l !== '' && l !== message);
    for (const f of frames.slice(0, 5)) out.push(`    ${f}`);
  }
  return out;
}

/** A console POST comes from a page of this server: `Origin` is `http(s)://<Host>`, and `Sec-Fetch-Site` (when sent) is same-origin. */
export function isSameOriginPost(headers: Readonly<Record<string, string | string[] | undefined>>): boolean {
  const one = (k: string): string | undefined => {
    const x = headers[k];
    return Array.isArray(x) ? x.join(', ') : x;
  };
  const origin = one('origin');
  const host = one('host');
  if (origin === undefined || host === undefined || host === '') return false;
  const site = one('sec-fetch-site');
  if (site !== undefined && site.trim().toLowerCase() !== 'same-origin') return false;
  return origin === `http://${host}` || origin === `https://${host}`;
}

/** The event that tells every page to reload. */
export const RELOAD_EVENT = '{"type":"reload"}';
/** Sent when a rebuild starts (pages show a Rebuilding badge). */
export const BUILDING_EVENT = '{"type":"building"}';
/** Sent after a good rebuild that changed nothing (pages clear the badge and any error overlay). */
export const OK_EVENT = '{"type":"ok"}';

/** The event for a failed rebuild: `{"type":"error","code","message"[,"file","line","column"]}`. */
export function errorEvent(e: SitefileError): string {
  return JSON.stringify({
    type: 'error',
    code: e.code,
    message: e.detail,
    ...(e.span ? { file: e.span.file, line: e.span.line, column: e.span.column } : {}),
  });
}

/**
 * The URL path that serves `file`: `/dir/` for `/dir/index.html` (and `/` for `/index.html`),
 * else the file path; each segment percent-encoded like encodeURIComponent.
 */
export function pageUrlFor(file: string): string {
  const segs = file.split('/').slice(1);
  if (segs[segs.length - 1] === 'index.html') segs[segs.length - 1] = '';
  return '/' + segs.map(encodeURIComponent).join('/');
}

/**
 * The live-reload event for a new tree (undefined when no file changed): `{"type":"css","paths":[...]}`
 * when `cssInject` is on and every changed file is a stylesheet that is in both trees (added or
 * removed files always reload), else {@link RELOAD_EVENT}; with `navigate`, a reload where exactly
 * one HTML file was added or changed also names its page: `{"type":"reload","path":"/posts/a/"}`.
 * A reload where only existing HTML files changed (nothing added or removed, no other type) is
 * marked `"morph":true`: pages may update in place. Paths are sorted.
 */
export function reloadEventFor(prev: Tree, next: Tree, cssInject: boolean, navigate = false): string | undefined {
  const changed: string[] = [];
  const pages: string[] = [];
  let cssOnly = cssInject;
  let htmlOnly = true;
  for (const [p, f] of next.entries()) {
    const old = prev.get(p);
    if (old !== undefined && old.digest === f.digest && old.mode === f.mode) continue;
    changed.push(p);
    const html = mimeTypeFor(p).startsWith('text/html');
    if (html) pages.push(p);
    if (old === undefined || !html) htmlOnly = false;
    if (old === undefined || !mimeTypeFor(p).startsWith('text/css')) cssOnly = false;
  }
  for (const [p] of prev.entries()) {
    if (!next.isFile(p)) {
      changed.push(p);
      cssOnly = false;
      htmlOnly = false;
    }
  }
  if (changed.length === 0) return undefined;
  if (!cssOnly) {
    const path = navigate && pages.length === 1 ? pageUrlFor(pages[0]!) : undefined;
    if (path === undefined && !htmlOnly) return RELOAD_EVENT;
    return JSON.stringify({ type: 'reload', ...(path !== undefined ? { path } : {}), ...(htmlOnly ? { morph: true } : {}) });
  }
  return JSON.stringify({ type: 'css', paths: changed.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)) });
}

/** Open live-reload streams; {@link LiveReload.send} gives every page an event ({@link LiveReload.reload}: reload). */
export class LiveReload {
  private readonly clients = new Set<http.ServerResponse>();
  /** The client script for this session ({@link clientScript}). */
  readonly script: string;

  /** Time between heartbeat comments on an open stream (15 s; tests pass less). */
  private readonly heartbeatMs: number;

  constructor(options: LiveReloadOptions = {}, heartbeatMs = HEARTBEAT_MS) {
    this.script = clientScript(options);
    this.heartbeatMs = heartbeatMs;
    this.console = options.console === true;
  }

  /**
   * Hold `res` open as a text/event-stream until the client goes away. The headers keep proxies
   * and tunnels from buffering or compressing it, and a comment every 15 s keeps idle
   * connections from being cut (and finds clients that went away).
   */
  attach(res: http.ServerResponse, headers: Record<string, string>): void {
    res.writeHead(200, { ...headers, ...STREAM_HEADERS });
    res.write('retry: 1000\n\n');
    // A page opened while the last build is broken shows the error at once.
    if (this.failure !== undefined) res.write(`data: ${this.failure}\n\n`);
    this.clients.add(res);
    const beat = setInterval(() => res.write(': ping\n\n'), this.heartbeatMs);
    beat.unref();
    res.on('close', () => {
      clearInterval(beat);
      this.clients.delete(res);
    });
  }

  readonly console: boolean;
  private consoleWindow = 0;
  private consoleCount = 0;

  /**
   * Whether one more console message may be logged now (at most {@link CONSOLE_LIMIT} per
   * {@link CONSOLE_WINDOW_MS}); the first refused one in a window also returns a notice line.
   */
  admitConsole(now: number): { ok: boolean; notice?: string } {
    if (now - this.consoleWindow >= CONSOLE_WINDOW_MS) {
      this.consoleWindow = now;
      this.consoleCount = 0;
    }
    this.consoleCount++;
    if (this.consoleCount <= CONSOLE_LIMIT) return { ok: true };
    return this.consoleCount === CONSOLE_LIMIT + 1 ? { ok: false, notice: `browser: more than ${CONSOLE_LIMIT} console messages in 10 s; dropping the rest` } : { ok: false };
  }

  /** The last error event, until a good build: sent to streams that open meanwhile. */
  private failure: string | undefined;

  /** Send `event` (one line of JSON) to every open page. */
  send(event: string): void {
    if (event.startsWith('{"type":"error"')) this.failure = event;
    else if (event !== BUILDING_EVENT) this.failure = undefined;
    for (const c of this.clients) c.write(`data: ${event}\n\n`);
  }

  reload(): void {
    this.send(RELOAD_EVENT);
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

/** A request header as one string (repeated headers joined with `, `). */
function header(v: string | string[] | undefined): string | undefined {
  return Array.isArray(v) ? v.join(', ') : v;
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

    const live = options.liveReload;
    if (live && url.split(/[?#]/, 1)[0] === CONSOLE_PATH) {
      // Browser console messages (the client script sends them; not access-logged).
      if (method !== 'POST') return finish(405, { allow: 'POST', 'content-length': 0 }, undefined, true);
      if (!isSameOriginPost(req.headers)) return finish(403, { 'content-length': 0 }, undefined, true);
      const chunks: Buffer[] = [];
      let size = 0;
      let tooBig = false;
      req.on('data', (c: Buffer) => {
        size += c.byteLength;
        if (size > MAX_CONSOLE_BODY) tooBig = true;
        else chunks.push(c);
      });
      req.on('end', () => {
        if (tooBig) return finish(413, { 'content-length': 0, connection: 'close' }, undefined, true);
        const lines = consoleLines(Buffer.concat(chunks).toString('utf8'), live.console);
        if (lines === undefined) return finish(400, { 'content-length': 0 }, undefined, true);
        if (lines.length > 0) {
          const admitted = live.admitConsole(Date.now());
          if (admitted.notice !== undefined) options.log?.(admitted.notice);
          if (admitted.ok) for (const l of lines) options.log?.(l);
        }
        finish(204, {}, undefined, true);
      });
      return;
    }
    if (method !== 'GET' && method !== 'HEAD') return page(405, METHOD_PAGE, { allow: 'GET, HEAD' });
    const tree = current();
    const { redirects, headers: headerRules } = rulesFor(tree);
    const isFile = (p: string): boolean => tree.isFile(p) && p !== '/_redirects' && p !== '/_headers';

    const q = url.indexOf('?');
    const hash = url.indexOf('#');
    const end = Math.min(q === -1 ? url.length : q, hash === -1 ? url.length : hash);
    const rawPath = url.slice(0, end);
    if (options.liveReload && rawPath === RELOAD_PATH) {
      if (method === 'HEAD') return finish(200, { ...STREAM_HEADERS }, undefined);
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
      // A `.dcz` sidecar (build --experimental-dictionary): the response varies with the dictionary offered.
      const vary: Record<string, string> = tree.isFile(`${served}.dcz`) ? { vary: 'accept-encoding, available-dictionary' } : {};
      const delta = live ? undefined : deltaFor(tree, served, header(req.headers['available-dictionary']), header(req.headers['accept-encoding']));
      // An injected page or a delta is a different representation of the file: give it its own ETag.
      const etag = `"${file.digest.replace(/^sha256:/, '')}${live ? '-live' : delta ? '-dcz' : ''}"`;
      if (etagMatches(req.headers['if-none-match'], etag)) return finish(304, { ...custom, ...vary, etag }, undefined);
      if (delta) return finish(200, { 'content-type': type, 'content-length': delta.byteLength, ...custom, ...vary, 'content-encoding': 'dcz', etag }, delta);
      finish(200, { 'content-type': type, 'content-length': body.byteLength, ...custom, ...vary, etag }, body);
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
