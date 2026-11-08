import { lookup as dnsLookup, type LookupAddress } from 'node:dns';
import { request as httpRequest, type IncomingMessage, type ClientRequest, type RequestOptions } from 'node:http';
import { request as httpsRequest, type RequestOptions as HttpsRequestOptions } from 'node:https';
import * as tls from 'node:tls';
import { isIP, type LookupFunction, type Socket } from 'node:net';
import { SitefileError, isSitefileError, type SourceSpan } from '../errors.js';
import type { FetchPolicy } from './types.js';
import { isPublicAddress } from './netpolicy.js';
import { checkHostRules, proxyFor, type NetworkSettings, type ProxyConfig } from './network.js';

const REDIRECT_STATUSES = new Set([301, 302, 303, 307, 308]);
const USER_AGENT = 'sitefile/0.1';

export interface FetchResult {
  readonly bytes: Uint8Array;
  /** URL of the final hop after redirects. */
  readonly finalUrl: string;
  /** Media type essence from `content-type` (lowercase, parameters removed). */
  readonly mediaType?: string;
}

/**
 * What to send. `headers` go to every hop; `originHeaders` (credentials: IMPORT … SECRET) only to hops
 * on the first URL's origin (scheme, host and port), so a redirect elsewhere never receives them.
 * A request with a body (POST) never follows redirects (E_REDIRECT).
 */
export interface FetchRequest {
  readonly method?: 'GET' | 'POST';
  readonly body?: Uint8Array;
  readonly headers?: Readonly<Record<string, string>>;
  readonly originHeaders?: Readonly<Record<string, string>>;
}

/** A URL rendered without any userinfo, safe for error messages. */
export function redactUrl(url: URL | string): string {
  try {
    const u = new URL(String(url));
    u.username = '';
    u.password = '';
    return u.href;
  } catch {
    return '<invalid url>';
  }
}

function stripBrackets(host: string): string {
  return host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host;
}

/**
 * Validate one hop before connecting: scheme, credentials and IP-literal
 * hosts. Hostnames are checked at connect time by the guarded lookup.
 */
export function validateHop(url: URL, policy: FetchPolicy, span?: SourceSpan): void {
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && policy.allowHttp)) {
    const hint = url.protocol === 'http:' ? ' (plain http requires --allow-http)' : '';
    throw new SitefileError('E_PROTOCOL', `unsupported protocol '${url.protocol}' for ${redactUrl(url)}${hint}`, span);
  }
  if (url.username !== '' || url.password !== '') {
    throw new SitefileError(
      'E_PROTOCOL',
      `URL ${redactUrl(url)} contains credentials; credentials in URLs are not allowed`,
      span,
    );
  }
  const host = stripBrackets(url.hostname);
  if (host === '') throw new SitefileError('E_PROTOCOL', `URL ${redactUrl(url)} has no host`, span);
  checkHostRules(url, policy.network, redactUrl(url), span);
  if (isIP(host) !== 0 && !policy.allowPrivateNetwork && !isPublicAddress(host)) {
    throw new SitefileError(
      'E_SSRF_BLOCKED',
      `refusing to connect to non-public address ${host} (${redactUrl(url)}); use --allow-private-network to permit it`,
      span,
    );
  }
}

/**
 * A `lookup` for http(s).request that resolves every address and fails with
 * E_SSRF_BLOCKED if any is non-public. Runs at connect time, so DNS rebinding
 * between validation and connection cannot bypass it.
 */
export function createGuardedLookup(policy: FetchPolicy, span?: SourceSpan): LookupFunction {
  return (hostname, options, callback) => {
    dnsLookup(hostname, { ...options, all: true }, (err, addresses) => {
      if (err) {
        (callback as (e: Error) => void)(err);
        return;
      }
      const list = addresses as LookupAddress[];
      if (list.length === 0) {
        (callback as (e: Error) => void)(new SitefileError('E_FETCH', `no addresses found for ${hostname}`, span));
        return;
      }
      if (!policy.allowPrivateNetwork) {
        const bad = list.find((a) => !isPublicAddress(a.address));
        if (bad) {
          (callback as (e: Error) => void)(
            new SitefileError(
              'E_SSRF_BLOCKED',
              `refusing to connect to ${hostname}: it resolves to non-public address ${bad.address}; use --allow-private-network to permit it`,
              span,
            ),
          );
          return;
        }
      }
      if (options.all) {
        (callback as (e: null, a: LookupAddress[]) => void)(null, list);
      } else {
        const first = list[0]!;
        (callback as (e: null, a: string, f: number) => void)(null, first.address, first.family);
      }
    });
  };
}

/** Find a SitefileError carried by a socket/request error, if any. */
function findSitefileError(e: unknown, depth = 0): SitefileError | undefined {
  if (depth > 4 || e === null || typeof e !== 'object') return undefined;
  if (isSitefileError(e)) return e;
  const anyE = e as { cause?: unknown; errors?: unknown };
  if (Array.isArray(anyE.errors)) {
    for (const inner of anyE.errors) {
      const found = findSitefileError(inner, depth + 1);
      if (found) return found;
    }
  }
  return findSitefileError(anyE.cause, depth + 1);
}

interface HopState {
  req?: ClientRequest;
  res?: IncomingMessage;
  /** A connect/read timeout that destroyed the request (reported instead of the stream error). */
  failure?: SitefileError;
}

/**
 * Fetch a remote resource under the given network policy. Redirects are
 * followed manually and every hop is re-validated; one deadline covers the
 * whole fetch.
 */
export async function fetchRemote(url: string, policy: FetchPolicy, span?: SourceSpan, request: FetchRequest = {}): Promise<FetchResult> {
  let current: URL;
  try {
    current = new URL(url);
  } catch (e) {
    throw new SitefileError('E_PROTOCOL', `invalid URL '${redactUrl(url)}'`, span, { cause: e });
  }
  const origin = current.origin;
  const method = request.method ?? 'GET';

  const controller = new AbortController();
  const state: HopState = {};
  const timer = setTimeout(() => {
    controller.abort();
    state.req?.destroy();
    state.res?.destroy();
  }, policy.timeoutMs);
  timer.unref?.();
  const timeoutError = () =>
    new SitefileError(
      'E_FETCH_TIMEOUT',
      `fetching ${redactUrl(url)} timed out after ${policy.timeoutMs} ms`,
      span,
    );

  try {
    let redirects = 0;
    for (;;) {
      validateHop(current, policy, span);
      if (controller.signal.aborted) throw timeoutError();

      // Credentials only ever go to the origin they were given for (not after a redirect elsewhere).
      const headers = current.origin === origin ? { ...request.headers, ...request.originHeaders } : { ...request.headers };
      const res = await openHop(current, policy, controller.signal, state, span, timeoutError, method, headers, request.body);
      const status = res.statusCode ?? 0;

      if (REDIRECT_STATUSES.has(status) && method !== 'GET') {
        res.destroy();
        throw new SitefileError('E_REDIRECT', `${method} ${redactUrl(current)} was redirected (HTTP ${status}); redirects are only followed for GET`, span);
      }
      if (REDIRECT_STATUSES.has(status)) {
        const location = res.headers.location;
        res.destroy();
        state.req?.destroy();
        if (!location) {
          throw new SitefileError('E_REDIRECT', `redirect ${status} from ${redactUrl(current)} has no Location header`, span);
        }
        redirects++;
        if (redirects > policy.maxRedirects) {
          throw new SitefileError(
            'E_REDIRECT',
            `too many redirects fetching ${redactUrl(url)} (limit ${policy.maxRedirects})`,
            span,
          );
        }
        let next: URL;
        try {
          next = new URL(location, current);
        } catch (e) {
          throw new SitefileError('E_REDIRECT', `invalid redirect Location from ${redactUrl(current)}`, span, { cause: e });
        }
        current = next;
        continue;
      }

      if (status !== 200) {
        res.destroy();
        throw new SitefileError('E_FETCH_STATUS', `${method} ${redactUrl(current)} returned HTTP ${status}`, span);
      }

      const encoding = String(res.headers['content-encoding'] ?? '').trim().toLowerCase();
      if (encoding !== '' && encoding !== 'identity') {
        res.destroy();
        throw new SitefileError(
          'E_FETCH',
          `${redactUrl(current)} responded with content-encoding '${encoding}'; only identity is accepted`,
          span,
        );
      }

      const lengthHeader = res.headers['content-length'];
      if (lengthHeader !== undefined) {
        const declared = Number(lengthHeader);
        if (Number.isFinite(declared) && declared > policy.maxBytes) {
          res.destroy();
          throw new SitefileError(
            'E_TOO_LARGE',
            `${redactUrl(current)} is ${declared} bytes, exceeding the limit of ${policy.maxBytes} bytes`,
            span,
          );
        }
      }

      const chunks: Buffer[] = [];
      let total = 0;
      try {
        for await (const chunk of res as AsyncIterable<Buffer>) {
          total += chunk.length;
          if (total > policy.maxBytes) {
            res.destroy();
            throw new SitefileError(
              'E_TOO_LARGE',
              `${redactUrl(current)} exceeded the limit of ${policy.maxBytes} bytes`,
              span,
            );
          }
          chunks.push(chunk);
        }
      } catch (e) {
        if (isSitefileError(e)) throw e;
        if (controller.signal.aborted) throw timeoutError();
        if (state.failure) throw state.failure;
        throw new SitefileError('E_FETCH', `error reading ${redactUrl(current)}: ${(e as Error).message}`, span, {
          cause: e,
        });
      }
      if (controller.signal.aborted) throw timeoutError();

      const buf = Buffer.concat(chunks, total);
      const bytes = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
      const ct = res.headers['content-type'];
      const mediaType = ct ? ct.split(';')[0]!.trim().toLowerCase() || undefined : undefined;
      return mediaType === undefined ? { bytes, finalUrl: current.href } : { bytes, finalUrl: current.href, mediaType };
    }
  } finally {
    clearTimeout(timer);
    state.res?.destroy();
    state.req?.destroy();
  }
}

async function openHop(
  url: URL,
  policy: FetchPolicy,
  signal: AbortSignal,
  state: HopState,
  span: SourceSpan | undefined,
  timeoutError: () => SitefileError,
  method: 'GET' | 'POST',
  extra: Readonly<Record<string, string>>,
  body: Uint8Array | undefined,
): Promise<IncomingMessage> {
  const options: RequestOptions = {
    method,
    headers: { 'accept-encoding': 'identity', 'user-agent': USER_AGENT, accept: '*/*', ...extra, ...(body ? { 'content-length': String(body.byteLength) } : {}) },
    lookup: createGuardedLookup(policy, span),
    agent: false,
    signal,
  };
  if (url.protocol === 'https:' && policy.ca !== undefined) {
    // Additive: setting `ca` replaces Node's whole default store, so start from
    // that store (bundled roots plus NODE_EXTRA_CA_CERTS / --use-system-ca),
    // not just the bundled roots, and add the extra CA(s).
    (options as HttpsRequestOptions).ca = [...defaultCaCertificates(), policy.ca];
  }
  const shown = redactUrl(url);
  const fail = (e: SitefileError) => {
    state.failure = e;
    return e;
  };
  let route: Route;
  try {
    route = await routeRequest(url, options, policy, { signal, span, shown, fail });
  } catch (e) {
    if (signal.aborted) throw timeoutError();
    throw e;
  }
  return new Promise((resolve, reject) => {
    let req: ClientRequest;
    try {
      req = route.url ? route.fn(route.url, route.options) : route.fn(route.options);
    } catch (e) {
      reject(new SitefileError('E_FETCH', `cannot request ${shown}: ${(e as Error).message}`, span, { cause: e }));
      return;
    }
    state.req = req;
    state.res = undefined;
    state.failure = undefined;
    armTimeouts(req, policy.network, route.proxied, (kind) => req.destroy(fail(hopTimeout(kind, shown, policy.network, span))));
    req.on('response', (res) => {
      state.res = res;
      resolve(res);
    });
    req.on('error', (e) => {
      if (signal.aborted) {
        reject(timeoutError());
        return;
      }
      const inner = findSitefileError(e);
      if (inner) {
        reject(inner);
        return;
      }
      reject(new SitefileError('E_FETCH', `fetching ${shown} failed: ${e.message}`, span, { cause: e }));
    });
    req.end(body);
  });
}

// ------------------------------------------------------------------ proxy and connect/read timeouts

/** How to send one request: directly, or through the configured proxy. */
export interface Route {
  readonly fn: typeof httpRequest;
  /** The URL to pass to `fn`; undefined when `options` carry the proxy's host, port and absolute path. */
  readonly url?: URL | undefined;
  readonly options: RequestOptions;
  /** True when the connection is to a proxy (the connect timeout then covers the tunnel). */
  readonly proxied: boolean;
}

export interface RouteContext {
  readonly signal?: AbortSignal | undefined;
  readonly span?: SourceSpan | undefined;
  /** The hop's URL as shown in messages. */
  readonly shown: string;
  /** Records a timeout error before the request is destroyed with it. */
  readonly fail?: ((e: SitefileError) => SitefileError) | undefined;
}

/** E_FETCH_TIMEOUT for a connect or read (idle) timeout. */
export function hopTimeout(kind: 'connect' | 'read', shown: string, network: NetworkSettings | undefined, span?: SourceSpan): SitefileError {
  return kind === 'connect'
    ? new SitefileError('E_FETCH_TIMEOUT', `fetching ${shown}: connecting timed out after ${network?.connectTimeoutMs} ms (connect timeout)`, span)
    : new SitefileError('E_FETCH_TIMEOUT', `fetching ${shown}: no data for ${network?.readTimeoutMs} ms (read timeout)`, span);
}

/**
 * Apply `network`'s connect timeout (until the TCP connection is up) and read timeout (socket idle time
 * once connected) to a request; `onTimeout` destroys it. For a tunnelled request the socket is already
 * connected (the tunnel had the connect timeout), so only the read timeout applies.
 */
export function armTimeouts(req: ClientRequest, network: NetworkSettings | undefined, _proxied: boolean, onTimeout: (kind: 'connect' | 'read') => void): void {
  const connectMs = network?.connectTimeoutMs;
  if (connectMs) {
    const t = setTimeout(() => onTimeout('connect'), connectMs);
    t.unref?.();
    const clear = () => clearTimeout(t);
    req.once('socket', (socket: Socket) => {
      if (socket.connecting) socket.once('connect', clear);
      else clear();
    });
    req.once('close', clear);
    req.once('error', clear);
  }
  const readMs = network?.readTimeoutMs;
  if (readMs) req.setTimeout(readMs, () => onTimeout('read'));
}

/** Resolve `hostname` and apply the private-address rule, as a direct connection would at connect time. */
function checkResolved(hostname: string, policy: FetchPolicy, span: SourceSpan | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    createGuardedLookup(policy, span)(hostname, { all: true }, (err: NodeJS.ErrnoException | null) => {
      if (!err) {
        resolve();
        return;
      }
      if (isSitefileError(err)) {
        reject(err);
        return;
      }
      reject(
        new SitefileError(
          'E_FETCH',
          `cannot resolve ${hostname} to check it against the private-network rule before using the proxy: ${err.code ?? err.message}`,
          span,
        ),
      );
    });
  });
}

/**
 * Route one request: directly (with the guarded lookup in `options`), or through `policy.network.proxy`
 * unless the host is in its no-proxy list. Through a proxy, the target host is still resolved here and
 * refused if it is non-public (unless `allowPrivateNetwork`), so a proxy never widens the policy; the
 * proxy itself is named by the user and may be on a private address.
 */
export async function routeRequest(url: URL, options: RequestOptions, policy: FetchPolicy, ctx: RouteContext): Promise<Route> {
  const https = url.protocol === 'https:';
  const proxy = proxyFor(url, policy.network);
  if (!proxy) return { fn: https ? httpsRequest : httpRequest, url, options, proxied: false };
  const host = url.hostname.replace(/^\[|\]$/g, '');
  if (!policy.allowPrivateNetwork && isIP(host) === 0) await checkResolved(host, policy, ctx.span);
  const port = url.port !== '' ? Number(url.port) : https ? 443 : 80;
  const { lookup: _lookup, agent: _agent, ...rest } = options;
  if (!https) {
    const target = new URL(url.href);
    target.hash = '';
    return {
      fn: httpRequest,
      options: {
        ...rest,
        host: proxy.host,
        port: proxy.port,
        path: target.href,
        agent: false,
        headers: { ...(options.headers as Record<string, string>), host: url.host, ...(proxy.authorization ? { 'proxy-authorization': proxy.authorization } : {}) },
      },
      proxied: true,
    };
  }
  const socket = await openTunnel(proxy, host, port, policy.network, ctx);
  const ca = (options as HttpsRequestOptions).ca;
  return {
    fn: httpsRequest,
    url,
    options: {
      ...rest,
      createConnection: () =>
        tls.connect({ socket, host, ...(isIP(host) === 0 ? { servername: host } : {}), ...(ca !== undefined ? { ca } : {}), ALPNProtocols: ['http/1.1'] }),
    } as RequestOptions,
    proxied: true,
  };
}

/** A CONNECT tunnel to `host:port` through `proxy`. */
function openTunnel(proxy: ProxyConfig, host: string, port: number, network: NetworkSettings | undefined, ctx: RouteContext): Promise<Socket> {
  const authority = `${isIP(host) === 6 ? `[${host}]` : host}:${port}`;
  return new Promise((resolve, reject) => {
    let req: ClientRequest;
    try {
      req = httpRequest({
        host: proxy.host,
        port: proxy.port,
        method: 'CONNECT',
        path: authority,
        agent: false,
        headers: { host: authority, 'user-agent': USER_AGENT, ...(proxy.authorization ? { 'proxy-authorization': proxy.authorization } : {}) },
        ...(ctx.signal ? { signal: ctx.signal } : {}),
      });
    } catch (e) {
      reject(new SitefileError('E_FETCH', `fetching ${ctx.shown} failed: proxy ${proxy.shown}: ${(e as Error).message}`, ctx.span, { cause: e }));
      return;
    }
    const record = ctx.fail ?? ((e: SitefileError) => e);
    armTimeouts(req, network, true, (kind) => req.destroy(record(hopTimeout(kind, ctx.shown, network, ctx.span))));
    const refused = (status: number | undefined) =>
      status === 407
        ? new SitefileError('E_FETCH', `proxy ${proxy.shown} requires authentication (HTTP 407)`, ctx.span)
        : new SitefileError('E_FETCH', `proxy ${proxy.shown} refused to connect to ${authority}`, ctx.span);
    req.on('connect', (res, socket, head) => {
      if (res.statusCode !== 200) {
        socket.destroy();
        reject(refused(res.statusCode));
        return;
      }
      socket.setTimeout(0);
      socket.removeAllListeners('timeout');
      if (head.length) socket.unshift(head);
      resolve(socket);
    });
    req.on('response', (res) => {
      res.resume();
      reject(refused(res.statusCode));
    });
    req.on('error', (e) => {
      const inner = findSitefileError(e);
      reject(inner ?? new SitefileError('E_FETCH', `fetching ${ctx.shown} failed: proxy ${proxy.shown}: ${e.message}`, ctx.span, { cause: e }));
    });
    req.end();
  });
}

/** Node's default CA store; `tls.getCACertificates` exists from Node 22.15/23.5. */
function defaultCaCertificates(): readonly string[] {
  const get = (tls as { getCACertificates?: (type?: string) => string[] }).getCACertificates;
  return get ? get('default') : tls.rootCertificates;
}
