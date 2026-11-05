import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { SitefileError } from './errors.js';
import type { Tree } from './tree.js';
import { mimeTypeFor } from './wbn.js';

export interface SiteServerOptions {
  /** Fall back to `/index.html` for unknown extension-less paths (single-page apps). */
  readonly spa?: boolean;
  /** One access-log line per request: `METHOD /path STATUS BYTES MSms`. */
  readonly log?: (line: string) => void;
  /** Sent as `x-sitefile-server` so `servers` can tell its own servers from a reused port. */
  readonly serverId?: string;
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
    out.push(seg);
  }
  return out;
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
 */
export function createSiteServer(tree: Tree, options: SiteServerOptions = {}): http.Server {
  const base: Record<string, string> = { 'x-content-type-options': 'nosniff', 'cache-control': 'no-cache' };
  if (options.serverId) base['x-sitefile-server'] = options.serverId;

  const handler = (req: http.IncomingMessage, res: http.ServerResponse): void => {
    const started = process.hrtime.bigint();
    const method = req.method ?? 'GET';
    const url = req.url ?? '/';
    let bytes = 0;
    const finish = (status: number, headers: Record<string, string | number>, body: Uint8Array | string | undefined): void => {
      const buf = body === undefined ? undefined : typeof body === 'string' ? Buffer.from(body) : body;
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
      options.log?.(`${method} ${printable(url)} ${status} ${bytes} ${ms.toFixed(1)}ms`);
    };
    const page = (status: number, html: string, extra: Record<string, string> = {}): void =>
      finish(status, { 'content-type': 'text/html; charset=utf-8', 'content-length': Buffer.byteLength(html), ...extra }, html);

    if (method !== 'GET' && method !== 'HEAD') return page(405, METHOD_PAGE, { allow: 'GET, HEAD' });

    const q = url.indexOf('?');
    const hash = url.indexOf('#');
    const end = Math.min(q === -1 ? url.length : q, hash === -1 ? url.length : hash);
    const rawPath = url.slice(0, end);
    const search = q === -1 ? '' : url.slice(q, hash === -1 || hash < q ? undefined : hash);
    const segs = safeSegments(rawPath);
    if (!segs) return page(400, BAD_REQUEST_PAGE);
    const trailingSlash = rawPath.endsWith('/');
    const vpath = '/' + segs.join('/');

    let served: string | undefined = tree.isFile(vpath) && !(trailingSlash && vpath !== '/') ? vpath : undefined;
    if (!served && tree.isDirectory(vpath)) {
      if (!trailingSlash) {
        const location = '/' + segs.map(encodeURIComponent).join('/') + '/' + search;
        return finish(301, { location, 'content-length': 0 }, undefined);
      }
      const index = vpath === '/' ? '/index.html' : `${vpath}/index.html`;
      if (tree.isFile(index)) served = index;
    }
    if (!served && options.spa && !/\.[^/.]+$/.test(segs[segs.length - 1] ?? '') && tree.isFile('/index.html')) served = '/index.html';
    const file = served === undefined ? undefined : tree.get(served);
    if (served === undefined || !file) return page(404, NOT_FOUND_PAGE);

    const etag = `"${file.digest.replace(/^sha256:/, '')}"`;
    const type = mimeTypeFor(served);
    if (etagMatches(req.headers['if-none-match'], etag)) return finish(304, { etag }, undefined);
    finish(200, { etag, 'content-type': type, 'content-length': file.bytes.byteLength }, file.bytes);
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
