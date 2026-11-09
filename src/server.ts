import http from 'node:http';
import https from 'node:https';
import os from 'node:os';
import type { AddressInfo } from 'node:net';
import { SitefileError } from './errors.js';
import { headersFor, matchRedirect, parseHeaderRules, parseRedirectRules } from './serve-rules.js';
import type { Tree } from './tree.js';
import { nfc } from './paths.js';
import { gzip } from './deflate.js';
import { mimeTypeFor } from './wbn.js';
import { RELOAD_CLIENT } from './reload-client.js';
import type { BasicAuth } from './serve-auth.js';

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
  /**
   * When the served files last changed (Unix seconds), read per request: sent as `Last-Modified` on
   * file responses, and `If-Modified-Since` (without `If-None-Match`) gives `304`. Unset: neither.
   */
  readonly lastModified?: () => number | undefined;
  /** Problems a person should know about (without the `warning: ` prefix), e.g. a CSP that blocks live reload. */
  readonly warn?: (message: string) => void;
  /**
   * DNS-rebinding guard: when set, a request whose `Host` is not `localhost`, a `*.localhost` name,
   * an IP address or a name this list allows gets `403` ({@link hostAllowed}). `*` allows every
   * host; unset: no check (the CLI always sets it).
   */
  readonly allowedHosts?: readonly string[];
  /** `--cors`: `Access-Control-Allow-Origin: *` on every response (`_headers` still wins) and `OPTIONS` preflights answered with `204`. */
  readonly cors?: boolean;
  /**
   * `--isolate`: cross-origin isolation (SharedArrayBuffer, wasm threads) on every response:
   * `Cross-Origin-Opener-Policy: same-origin`, this `Cross-Origin-Embedder-Policy` and
   * `Cross-Origin-Resource-Policy: same-origin`. `_headers` still wins.
   */
  readonly isolate?: 'require-corp' | 'credentialless';
  /** `--max-age`, `--immutable`, `--max-age-html`: a production `Cache-Control` preview for files ({@link cacheControlFor}). */
  readonly cache?: CachePreview;
  /** `--log-format`: access-log lines as text (the default, `GET /path 200 1234 1.2ms`) or one JSON object each ({@link accessLogLine}). */
  readonly logFormat?: 'text' | 'json';
  /** `--log-ip`: add the client address to access-log lines. */
  readonly logIp?: boolean;
  /** `--quiet`: no access-log lines (other log lines still go to {@link log}). */
  readonly quiet?: boolean;
  /** `--auth`, `--auth-file`: every request needs one of these credentials (`Authorization: Basic`), else `401`. */
  readonly auth?: BasicAuth;
  /** `--max-connections-per-ip`: open connections one non-loopback client may hold (0 or unset: no limit); a connection over it gets `503` with `Retry-After: 1` and is closed. */
  readonly maxConnectionsPerIp?: number;
  /** Count loopback clients against {@link maxConnectionsPerIp} too (tests only). */
  readonly limitLoopback?: boolean;
  /** `--random-route`: the site answers only under this prefix (`/<random>`); other paths are a plain `404`. */
  readonly basePath?: string;
  /** `--allow-ip`, `--deny-ip` (CIDRs, {@link parseCidr}): a non-loopback client they refuse gets `403` before anything else. */
  readonly ipRules?: { readonly allow: readonly Cidr[]; readonly deny: readonly Cidr[] };
  /** `X-Robots-Tag: noindex` on every response (the CLI sets it on non-loopback binds). */
  readonly noindex?: boolean;
  /** Add the client address and device (from `User-Agent`) to access-log lines of non-loopback clients. */
  readonly clientInfo?: boolean;
  /** `--tls-cert`, `--tls-key`: serve HTTPS with this PEM certificate chain and private key. */
  readonly tls?: { readonly cert: string | Uint8Array; readonly key: string | Uint8Array };
  /** `--compress`: gzip compressible files of at least 1024 bytes on the fly when no precompressed variant exists. */
  readonly compress?: boolean;
  /**
   * Where file contents come from when the tree holds only digests (a zip served in place,
   * `openZipFile`): `read` returns a file's bytes (undefined: use the node's), `deflated` a
   * deflated file's raw deflate stream, sent as `content-encoding: gzip` to clients that accept it.
   */
  readonly content?: SiteContent;
}

/** Contents read on demand ({@link SiteServerOptions.content}). */
export interface SiteContent {
  read(path: string): Uint8Array | undefined;
  deflated?(path: string): { readonly deflate: Uint8Array; readonly crc: number; readonly size: number } | undefined;
}

/** Whether an `accept-encoding` header accepts gzip (`gzip` or `x-gzip` with a non-zero q). */
export function acceptsGzip(header: string | undefined): boolean {
  if (header === undefined) return false;
  return header.split(',').some((part) => {
    const [name, ...params] = part.trim().split(';');
    if (name?.trim().toLowerCase() !== 'gzip' && name?.trim().toLowerCase() !== 'x-gzip') return false;
    const q = params.map((x) => x.trim()).find((x) => x.toLowerCase().startsWith('q='));
    return q === undefined || Number(q.slice(2)) > 0;
  });
}

/** A gzip member around a raw deflate stream: fixed header (no name, mtime 0, OS unknown), CRC-32 and size. */
export function gzipMember(deflate: Uint8Array, crc: number, size: number): Uint8Array {
  const head = Buffer.from([0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
  const tail = Buffer.alloc(8);
  tail.writeUInt32LE(crc >>> 0, 0);
  tail.writeUInt32LE(size % 2 ** 32, 4);
  return Buffer.concat([head, deflate, tail]);
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
const FORBIDDEN_PAGE = '<!doctype html><meta charset="utf-8"><title>403 Forbidden</title><h1>403 Forbidden</h1>\n';
const UNAUTHORIZED_PAGE = '<!doctype html><meta charset="utf-8"><title>401 Unauthorized</title><h1>401 Unauthorized</h1>\n';
const FORBIDDEN_HOST_PAGE =
  '<!doctype html><meta charset="utf-8"><title>403 Forbidden</title><h1>403 Forbidden</h1><p>This server answers requests for localhost and IP addresses only. To allow another host name, restart it with <code>--allowed-host &lt;name&gt;</code>.</p>\n';
const ERROR_PAGE = '<!doctype html><meta charset="utf-8"><title>500 Internal Server Error</title><h1>500 Internal Server Error</h1>\n';
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
/** One access-log request, as {@link accessLogLine} writes it. */
export interface AccessLogEntry {
  readonly ts: string;
  readonly method: string;
  /** The request target as received (made printable: other bytes become `%xx`). */
  readonly target: string;
  readonly status: number;
  readonly bytes: number;
  readonly ms: number;
  /** `Content-Range` of a `206`/`416`, else undefined. */
  readonly range?: string;
  /** The client address (`--log-ip`, or a non-loopback client with client info), else undefined. */
  readonly remote?: string;
  /** The client's device ({@link deviceOf}), for non-loopback clients with client info. */
  readonly device?: string;
}

/**
 * An access-log line. Text: `METHOD target STATUS BYTES MSms` (then ` <remote>` with `--log-ip`). JSON, a stable
 * contract (docs/CLI.md): `{"type":"request","ts","method","path","query","status","bytes","ms","encoding","range"}`
 * and `"remote"` with `--log-ip`, in that order; `path` and `query` split the target at the first `?` (the `?`
 * dropped), `ms` has one decimal, `range` is the response's `Content-Range` or null.
 */
export function accessLogLine(format: 'text' | 'json', e: AccessLogEntry): string {
  const target = printable(e.target);
  if (format === 'text') return `${e.method} ${target} ${e.status} ${e.bytes} ${e.ms.toFixed(1)}ms${e.remote !== undefined ? ` ${e.remote}` : ''}${e.device !== undefined ? ` ${e.device}` : ''}`;
  const q = target.indexOf('?');
  const path = q === -1 ? target : target.slice(0, q);
  const query = q === -1 ? '' : target.slice(q + 1);
  return (
    `{"type":"request","ts":${JSON.stringify(e.ts)},"method":${JSON.stringify(e.method)},"path":${JSON.stringify(path)},"query":${JSON.stringify(query)},` +
    `"status":${e.status},"bytes":${e.bytes},"ms":${e.ms.toFixed(1)},"encoding":"identity","range":${e.range === undefined ? 'null' : JSON.stringify(e.range)}` +
    `${e.remote !== undefined ? `,"remote":${JSON.stringify(e.remote)}` : ''}${e.device !== undefined ? `,"device":${JSON.stringify(e.device)}` : ''}}`
  );
}

/** A log line that is not a request, as a JSON object (`--log-format json`): `warning: …` and `error…` lines get their own types. */
export function jsonLogLine(ts: string, line: string): string {
  const [type, message] = line.startsWith('warning: ') ? ['warning', line.slice(9)] : line.startsWith('error') ? ['error', line] : ['message', line];
  return `{"type":${JSON.stringify(type)},"ts":${JSON.stringify(ts)},"message":${JSON.stringify(message)}}`;
}

/** `{"type":"started","ts","site","files","url"}`: a server is listening (`--log-format json`). */
export function startedLogLine(ts: string, site: string, files: number, url: string): string {
  return `{"type":"started","ts":${JSON.stringify(ts)},"site":${JSON.stringify(site)},"files":${files},"url":${JSON.stringify(url)}}`;
}

/** `{"type":"stopped","ts","signal"}`: a server shuts down (`--log-format json`). */
export function stoppedLogLine(ts: string, signal: string): string {
  return `{"type":"stopped","ts":${JSON.stringify(ts)},"signal":${JSON.stringify(signal)}}`;
}

/**
 * A detached server's log line as JSON (`server logs --json`): JSON lines as they are; a text line
 * (`<ISO time> <rest>`) as a request, started, stopped, warning, error or message object.
 */
export function logLineAsJson(line: string): string {
  if (line.startsWith('{')) return line;
  const m = /^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z) (.*)$/.exec(line);
  if (!m) return jsonLogLine('', line);
  const [, ts, rest] = m as unknown as [string, string, string];
  const r = /^([A-Z][A-Z-]*) (\S+) ([0-9]{3}) ([0-9]+) ([0-9]+\.[0-9])ms(?: (\S+))?$/.exec(rest);
  if (r) {
    return accessLogLine('json', { ts, method: r[1]!, target: r[2]!, status: Number(r[3]), bytes: Number(r[4]), ms: Number(r[5]), ...(r[6] !== undefined ? { remote: r[6] } : {}) });
  }
  const started = /^serving (.+) \(([0-9]+) files\) at (\S+)$/.exec(rest);
  if (started) return startedLogLine(ts, started[1]!, Number(started[2]), started[3]!);
  const stopped = /^received (SIG[A-Z]+), shutting down$/.exec(rest);
  if (stopped) return stoppedLogLine(ts, stopped[1]!);
  return jsonLogLine(ts, rest);
}

/**
 * Whether an `Accept-Encoding` value accepts `coding`: listed (or `*`) with a q-value above 0, ASCII
 * case-insensitive; an explicit entry wins over `*`.
 */
export function acceptsEncoding(header: string | undefined, coding: string): boolean {
  if (header === undefined) return false;
  let star: boolean | undefined;
  for (const part of header.split(',')) {
    const [name, ...params] = part.split(';').map((x) => x.trim());
    const n = (name ?? '').replace(/[A-Z]/g, (c) => c.toLowerCase());
    let q = 1;
    for (const p of params) {
      const m = /^[qQ][ \t]*=[ \t]*([0-9.]+)$/.exec(p);
      if (m) q = Number(m[1]);
    }
    const ok = Number.isFinite(q) && q > 0;
    if (n === coding) return ok;
    if (n === '*') star = ok;
  }
  return star ?? false;
}

/** Media types worth compressing: `text/*`, JSON, XML, SVG, JavaScript, wasm and `+json`/`+xml` types. */
export function isCompressible(type: string): boolean {
  const t = (type.split(';')[0] ?? '').trim();
  return t.startsWith('text/') || t.endsWith('+json') || t.endsWith('+xml') || ['application/json', 'application/xml', 'application/javascript', 'application/wasm', 'image/svg+xml'].includes(t);
}

/** Production cache headers to preview instead of `no-cache` (`serve --max-age`). */
export interface CachePreview {
  /** Seconds for files other than HTML: `public, max-age=N`. */
  readonly maxAge?: number;
  /** Adds `immutable` to the non-HTML value (needs {@link maxAge}). */
  readonly immutable?: boolean;
  /** Seconds for HTML pages (absent: they keep `no-cache`, the common production practice). */
  readonly maxAgeHtml?: number;
}

/** The `Cache-Control` a file of media `type` gets under `cache`, or undefined to keep `no-cache`. */
export function cacheControlFor(type: string, cache: CachePreview | undefined): string | undefined {
  if (cache === undefined) return undefined;
  if (type.startsWith('text/html')) return cache.maxAgeHtml === undefined ? undefined : `public, max-age=${cache.maxAgeHtml}`;
  if (cache.maxAge === undefined) return undefined;
  return `public, max-age=${cache.maxAge}${cache.immutable ? ', immutable' : ''}`;
}

/**
 * The origin form of a request target: an absolute-form target (`http://host/path?q`, RFC 9112
 * section 3.2.2, sent by proxies and some clients) loses its scheme and authority (`/path?q`, or
 * `/` for an empty path); any other target is returned as it is. The authority is not checked.
 */
/** `Access-Control-Request-Headers` echoed in a preflight answer, when it is a plain list of header names. */
export function preflightHeaders(value: string | undefined): string | undefined {
  const v = value?.trim();
  return v !== undefined && v !== '' && /^[A-Za-z0-9 \t,!#$%&'*+\-.^_`|~]+$/.test(v) ? v : undefined;
}

/** Loopback, also as an IPv4-mapped IPv6 address (`::ffff:127.0.0.1`). */
function isLoopbackAddress(ip: string): boolean {
  return ip === '::1' || /^(::ffff:)?127\./i.test(ip);
}

/** A parsed CIDR block: 4 or 16 address bytes and a prefix length. */
export interface Cidr {
  readonly bytes: readonly number[];
  readonly prefix: number;
}

/** Address bytes of an IPv4 or IPv6 literal (`::ffff:a.b.c.d` becomes IPv4), else undefined. */
export function ipBytes(text: string): number[] | undefined {
  const t = text.replace(/^::ffff:(?=[0-9]+\.)/i, '');
  if (isIpv4(t)) return t.split('.').map(Number);
  if (!/^[0-9A-Fa-f:]+$/.test(t) || !t.includes(':')) return undefined;
  const halves = t.split('::');
  if (halves.length > 2) return undefined;
  const words = (h: string): number[] | undefined => {
    if (h === '') return [];
    const out: number[] = [];
    for (const w of h.split(':')) {
      if (!/^[0-9A-Fa-f]{1,4}$/.test(w)) return undefined;
      out.push(parseInt(w, 16));
    }
    return out;
  };
  const a = words(halves[0]!);
  const b = halves.length === 2 ? words(halves[1]!) : [];
  if (a === undefined || b === undefined) return undefined;
  const fill = halves.length === 2 ? 8 - a.length - b.length : 0;
  if (fill < (halves.length === 2 ? 1 : 0) || a.length + b.length + fill !== 8) return undefined;
  return [...a, ...new Array<number>(fill).fill(0), ...b].flatMap((w) => [w >> 8, w & 0xff]);
}

/** `a.b.c.d/n`, `x::y/n` or a bare address (a /32 or /128), else undefined. */
export function parseCidr(text: string): Cidr | undefined {
  const [addr, len, extra] = text.trim().split('/');
  if (extra !== undefined || addr === undefined) return undefined;
  const bytes = ipBytes(addr);
  if (bytes === undefined) return undefined;
  const max = bytes.length * 8;
  if (len !== undefined && !/^[0-9]{1,3}$/.test(len)) return undefined;
  const prefix = len === undefined ? max : Number(len);
  return prefix <= max ? { bytes, prefix } : undefined;
}

function cidrContains(c: Cidr, ip: readonly number[]): boolean {
  if (c.bytes.length !== ip.length) return false;
  for (let bit = 0; bit < c.prefix; bit++) {
    const mask = 0x80 >> bit % 8;
    if ((c.bytes[bit >> 3]! & mask) !== (ip[bit >> 3]! & mask)) return false;
  }
  return true;
}

/** Whether `--allow-ip`/`--deny-ip` let a client in: loopback always; else not denied, and allowed when an allow list is given. */
export function ipAllowed(rules: SiteServerOptions['ipRules'], remote: string): boolean {
  if (rules === undefined || isLoopbackAddress(remote)) return true;
  const ip = ipBytes(remote);
  if (ip === undefined) return false;
  if (rules.deny.some((c) => cidrContains(c, ip))) return false;
  return rules.allow.length === 0 || rules.allow.some((c) => cidrContains(c, ip));
}

/** A short device name from a `User-Agent` (`iPhone`, `iPad`, `Android`, `Windows`, `Mac`, `Linux`), or `other`. */
export function deviceOf(userAgent: string | undefined): string {
  const ua = userAgent ?? '';
  for (const [needle, name] of [['iPhone', 'iPhone'], ['iPad', 'iPad'], ['Android', 'Android'], ['Windows', 'Windows'], ['Macintosh', 'Mac'], ['Linux', 'Linux']] as const) {
    if (ua.includes(needle)) return name;
  }
  return 'other';
}

/** Whether `s` is a dotted-decimal IPv4 address. */
function isIpv4(s: string): boolean {
  const parts = s.split('.');
  return parts.length === 4 && parts.every((p) => /^[0-9]{1,3}$/.test(p) && Number(p) <= 255);
}

/**
 * The DNS-rebinding guard of {@link SiteServerOptions.allowedHosts}: whether a `Host` header value
 * may be answered. Always allowed: no list, `*` in the list, no `Host`, an IP address (a rebinding
 * attack needs a name), `localhost` and `*.localhost`. Otherwise the name (ASCII lowercase, without
 * port and trailing dot) must equal an entry, or end with an entry that starts with `.` (or equal
 * it without the dot).
 */
export function hostAllowed(allowed: readonly string[] | undefined, host: string | undefined): boolean {
  if (allowed === undefined || allowed.includes('*') || host === undefined) return true;
  const h = host.trim();
  if (h === '') return true;
  if (h.startsWith('[')) {
    const close = h.indexOf(']');
    return close > 1 && /^[0-9A-Fa-f:.]+$/.test(h.slice(1, close)) && /^(:[0-9]*)?$/.test(h.slice(close + 1));
  }
  const m = /^(.*?)(:[0-9]*)?$/.exec(h)!;
  let name = m[1]!.replace(/[A-Z]/g, (c) => c.toLowerCase());
  if (name.endsWith('.')) name = name.slice(0, -1);
  if (isIpv4(name) || name === 'localhost' || name.endsWith('.localhost')) return true;
  return allowed.some((e) => (e.startsWith('.') ? name === e.slice(1) || name.endsWith(e) : name === e));
}

/** An `--allowed-host` value, lowercased: `*`, or a host name (letters, digits, `-`, `.`; a leading `.` for subdomains). */
export function parseAllowedHost(value: string): string | undefined {
  const v = value.trim().replace(/[A-Z]/g, (c) => c.toLowerCase());
  if (v === '*') return v;
  return /^\.?[a-z0-9-]+(\.[a-z0-9-]+)*\.?$/.test(v) ? v.replace(/\.$/, '') : undefined;
}

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
  if (options.cors) base['access-control-allow-origin'] = '*';
  if (options.noindex) base['x-robots-tag'] = 'noindex';
  if (options.isolate !== undefined) {
    base['cross-origin-opener-policy'] = 'same-origin';
    base['cross-origin-embedder-policy'] = options.isolate;
    base['cross-origin-resource-policy'] = 'same-origin';
  }
  // `_redirects` / `_headers` are parsed once per tree (a watched server gets a new tree per rebuild).
  let rulesTree: Tree | undefined;
  let rules: { redirects: ReturnType<typeof parseRedirectRules>; headers: ReturnType<typeof parseHeaderRules> } = { redirects: [], headers: [] };
  const rulesFor = (t: Tree): typeof rules => {
    if (t !== rulesTree) {
      const text = (p: string): string => {
        const f = t.get(p);
        return f ? new TextDecoder().decode(options.content?.read(p) ?? f.bytes) : '';
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

  /** --compress: gzip output by file digest (at most 256 files), so a file is compressed once. */
  const gzipCache = new Map<string, Uint8Array>();
  const gzipped = (digest: string, bytes: Uint8Array): Uint8Array => {
    let out = gzipCache.get(digest);
    if (out === undefined) {
      if (gzipCache.size >= 256) gzipCache.clear();
      out = gzip(bytes);
      gzipCache.set(digest, out);
    }
    return out;
  };
  /** Connections over --max-connections-per-ip: they get one 503. */
  const busy = new WeakSet<object>();
  const handler = (req: http.IncomingMessage, res: http.ServerResponse): void => {
    const started = process.hrtime.bigint();
    const method = req.method ?? 'GET';
    /** A non-loopback client of a server with client info: its address and device go into the access log. */
    const shared = !!options.clientInfo && !isLoopbackAddress(req.socket.remoteAddress ?? '');
    const target = req.url ?? '/';
    const url = originForm(target);
    let bytes = 0;
    const finish = (status: number, headers: Record<string, string | number>, body: Uint8Array | string | undefined, quiet = false): void => {
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
      if (quiet) return;
      // `servers` probes carry the server's own id; they would only clutter the access log.
      if (options.serverId !== undefined && req.headers['x-sitefile-probe'] === options.serverId) return;
      if (options.quiet) return;
      const range = headers['content-range'];
      options.log?.(
        accessLogLine(options.logFormat ?? 'text', {
          ts: new Date().toISOString(),
          method,
          target,
          status,
          bytes,
          ms,
          ...(range !== undefined ? { range: String(range) } : {}),
          ...(options.logIp || shared ? { remote: req.socket.remoteAddress ?? '' } : {}),
          ...(shared ? { device: deviceOf(req.headers['user-agent']) } : {}),
        }),
      );
    };
    const page = (status: number, html: string, extra: Record<string, string> = {}): void =>
      finish(status, { 'content-type': 'text/html; charset=utf-8', 'content-length': Buffer.byteLength(html), ...extra }, html);

    if (!ipAllowed(options.ipRules, req.socket.remoteAddress ?? '')) return page(403, FORBIDDEN_PAGE);
    if (busy.has(req.socket)) return finish(503, { 'retry-after': '1', 'content-length': 0, connection: 'close' }, undefined);
    // DNS rebinding: a page on another name must not read this server (before anything else).
    const hostHeader = req.headers.host;
    if (!hostAllowed(options.allowedHosts, hostHeader)) return page(403, FORBIDDEN_HOST_PAGE);
    // Basic authentication covers everything, the live-reload endpoints too.
    if (options.auth !== undefined && !options.auth.allows(req.headers.authorization)) {
      return page(401, UNAUTHORIZED_PAGE, { 'www-authenticate': 'Basic realm="sitefile", charset="UTF-8"' });
    }

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
    if (options.cors && method === 'OPTIONS') {
      // A CORS preflight: any path, nothing is looked up.
      const allowHeaders = preflightHeaders(req.headers['access-control-request-headers']);
      return finish(
        204,
        { 'access-control-allow-methods': 'GET, HEAD', ...(allowHeaders !== undefined ? { 'access-control-allow-headers': allowHeaders } : {}), 'access-control-max-age': '600' },
        undefined,
      );
    }
    if (method !== 'GET' && method !== 'HEAD') return page(405, METHOD_PAGE, { allow: options.cors ? 'GET, HEAD, OPTIONS' : 'GET, HEAD' });
    const tree = current();
    const { redirects, headers: headerRules } = rulesFor(tree);
    const isFile = (p: string): boolean => tree.isFile(p) && p !== '/_redirects' && p !== '/_headers';

    const q = url.indexOf('?');
    const hash = url.indexOf('#');
    const end = Math.min(q === -1 ? url.length : q, hash === -1 ? url.length : hash);
    let rawPath = url.slice(0, end);
    const prefix = options.basePath;
    const search0 = q === -1 ? '' : url.slice(q, hash === -1 || hash < q ? undefined : hash);
    if (prefix !== undefined && !rawPath.startsWith('/__sitefile/')) {
      // --random-route: only paths under the secret prefix exist (the reserved /__sitefile/ ones too).
      if (rawPath === prefix) return finish(301, { location: `${prefix}/${search0}`, 'content-length': 0 }, undefined);
      if (!rawPath.startsWith(`${prefix}/`)) return page(404, NOT_FOUND_PAGE);
      rawPath = rawPath.slice(prefix.length);
    }
    /** A redirect target inside the site: under the secret prefix with --random-route. */
    const loc = (l: string): string => (prefix !== undefined && l.startsWith('/') && !l.startsWith('//') ? prefix + l : l);
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
      // A deflated zip entry goes to a gzip client as is, wrapped in a gzip header and trailer (not for a
      // range request: ranges are served from the identity body).
      const deflated = !live && req.headers.range === undefined && acceptsGzip(req.headers['accept-encoding']) ? options.content?.deflated?.(served) : undefined;
      let body: Uint8Array;
      try {
        body = deflated ? gzipMember(deflated.deflate, deflated.crc, deflated.size) : (options.content?.read(served) ?? file.bytes);
      } catch (e) {
        options.log?.(`error: ${e instanceof SitefileError ? e.message : String(e)}`);
        return page(500, ERROR_PAGE);
      }
      if (live) body = injectReloadScript(body);
      // Otherwise: a precompressed `<path>.br` / `<path>.gz` the client accepts (br first), else gzip on the fly
      // under --compress. Range requests and live pages are always identity.
      const ae = req.headers['accept-encoding'];
      const hasBr = !deflated && status === 200 && isFile(`${served}.br`);
      const hasGz = !deflated && status === 200 && isFile(`${served}.gz`);
      const onTheFly = !deflated && status === 200 && !!options.compress && isCompressible(type) && body.byteLength >= 1024;
      const negotiated = !live && (hasBr || hasGz || onTheFly);
      let coding: 'br' | 'gzip' | undefined;
      if (negotiated && req.headers.range === undefined) {
        if (hasBr && acceptsEncoding(ae, 'br')) [coding, body] = ['br', tree.get(`${served}.br`)!.bytes];
        else if (hasGz && acceptsEncoding(ae, 'gzip')) [coding, body] = ['gzip', tree.get(`${served}.gz`)!.bytes];
        else if (onTheFly && acceptsEncoding(ae, 'gzip')) [coding, body] = ['gzip', gzipped(file.digest, body)];
      }
      const vary: Record<string, string> = deflated || negotiated ? { vary: 'accept-encoding' } : {};
      const encoding: Record<string, string> = deflated ? { 'content-encoding': 'gzip', ...vary } : coding ? { 'content-encoding': coding, ...vary } : vary;
      if (status !== 200) return finish(status, { 'content-type': type, 'content-length': body.byteLength, ...encoding, ...custom }, body);
      const preview = cacheControlFor(type, options.cache);
      const cc: Record<string, string> = preview === undefined ? {} : { 'cache-control': preview };
      // An injected page and a gzip response are other representations of the file: they get their own ETags.
      const etag = `"${file.digest.replace(/^sha256:/, '')}${live ? '-live' : ''}${deflated ? '-gzip' : coding ? `-${coding}` : ''}"`;
      const modified = options.lastModified?.();
      const lm: Record<string, string> = modified === undefined ? {} : { 'last-modified': httpDate(modified) };
      if (etagMatches(req.headers['if-none-match'], etag)) return finish(304, { ...encoding, ...cc, ...custom, etag, ...lm }, undefined);
      // If-Modified-Since only counts without If-None-Match (RFC 9110 section 13.2.2).
      const ims = req.headers['if-none-match'] === undefined ? req.headers['if-modified-since'] : undefined;
      const since = ims === undefined ? undefined : parseHttpDate(ims);
      if (modified !== undefined && since !== undefined && modified <= since) return finish(304, { ...encoding, ...cc, ...custom, etag, ...lm }, undefined);
      // A single byte range of a GET (`If-Range` must name this ETag); anything else gets the whole file.
      const ifRange = req.headers['if-range'] as string | undefined;
      const range = method === 'GET' && (ifRange === undefined || ifRange.trim() === etag) ? byteRange(req.headers.range, body.byteLength) : undefined;
      if (range === 'unsatisfiable') {
        return finish(416, { ...custom, ...vary, 'accept-ranges': 'bytes', 'content-range': `bytes */${body.byteLength}`, 'content-length': 0 }, undefined);
      }
      if (range !== undefined) {
        const [start, end] = range;
        const part = body.subarray(start, end + 1);
        return finish(
          206,
          { 'content-type': type, 'content-length': part.byteLength, ...cc, ...custom, etag, ...lm, ...vary, 'accept-ranges': 'bytes', 'content-range': `bytes ${start}-${end}/${body.byteLength}` },
          part,
        );
      }
      finish(200, { 'content-type': type, 'content-length': body.byteLength, ...encoding, ...cc, ...custom, etag, ...lm, 'accept-ranges': 'bytes' }, body);
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
          finish(r.status, { location: loc(target.includes('?') ? target : target + search), 'content-length': 0 }, undefined);
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
      const location = loc('/' + segs.map(encodeURIComponent).join('/') + '/' + search);
      return finish(301, { location, 'content-length': 0 }, undefined);
    }
    if (served === undefined && redirect(false)) return;
    // `/about/` when only `/about.html` answers: redirect to the clean URL `/about` (relative links in the
    // page then resolve as they do at `/about`). `_redirects` rules for the path come first.
    if (served === undefined && trailingSlash && segs.length > 0 && isFile(`${vpath}.html`)) {
      const location = loc('/' + segs.map(encodeURIComponent).join('/') + search);
      return finish(301, { location, 'content-length': 0 }, undefined);
    }
    if (served === undefined && options.spa && !/\.[^/.]+$/.test(segs[segs.length - 1] ?? '') && isFile('/index.html')) served = '/index.html';
    if (served !== undefined) return sendFile(200, served);
    if (isFile('/404.html')) return sendFile(404, '/404.html');
    page(404, NOT_FOUND_PAGE, custom);
  };

  // HTTPS when a certificate is given (same handler; an https.Server has the http.Server API).
  const server = options.tls
    ? (https.createServer({ cert: Buffer.from(options.tls.cert), key: Buffer.from(options.tls.key) }, handler) as unknown as http.Server)
    : http.createServer(handler);
  const limit = options.maxConnectionsPerIp ?? 0;
  if (limit > 0) {
    const perIp = new Map<string, number>();
    server.on('connection', (socket) => {
      const ip = socket.remoteAddress ?? '';
      if (!options.limitLoopback && isLoopbackAddress(ip)) return;
      const n = (perIp.get(ip) ?? 0) + 1;
      perIp.set(ip, n);
      if (n > limit) busy.add(socket);
      socket.once('close', () => {
        const left = (perIp.get(ip) ?? 1) - 1;
        if (left === 0) perIp.delete(ip);
        else perIp.set(ip, left);
      });
    });
  }
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

/**
 * The addresses other machines can reach a server bound to all interfaces at: the up, non-loopback
 * interface addresses, IPv4 for `0.0.0.0`, IPv4 then IPv6 for `::` (IPv6 link-local `fe80::/10`
 * left out: it needs a zone), each family sorted as text, without duplicates. Empty for any other bind.
 */
export function networkAddresses(bindHost: string): string[] {
  if (bindHost !== '0.0.0.0' && bindHost !== '::') return [];
  const v4 = new Set<string>();
  const v6 = new Set<string>();
  for (const list of Object.values(os.networkInterfaces())) {
    for (const a of list ?? []) {
      if (a.internal) continue;
      if (a.family === 'IPv4') v4.add(a.address);
      else if (bindHost === '::' && !/^fe[89ab]/i.test(a.address)) v6.add(a.address.toLowerCase());
    }
  }
  const sorted = (xs: Set<string>): string[] => [...xs].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  return [...sorted(v4), ...sorted(v6)];
}

/** {@link networkAddresses} as URLs. */
export function networkUrls(bindHost: string, port: number, scheme: 'http' | 'https' = 'http'): string[] {
  return networkAddresses(bindHost).map((a) => formatUrl(a, port, scheme));
}

export function formatUrl(host: string, port: number, scheme: 'http' | 'https' = 'http'): string {
  const shown = host === '0.0.0.0' || host === '::' ? '127.0.0.1' : host;
  return `${scheme}://${shown.includes(':') ? `[${shown}]` : shown}:${port}/`;
}
