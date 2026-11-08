import { isIP } from 'node:net';
import { SitefileError, type SourceSpan } from '../errors.js';

/**
 * Explicit network settings for everything sitefile fetches (IMPORT over http(s), Git, registries).
 * Nothing here is read from the environment implicitly: `HTTP(S)_PROXY` stays ignored, and the CLI
 * reads `SITEFILE_PROXY` and friends through {@link networkFromEnv}. Embedders pass this in code
 * (`FetchPolicy.network`, `RegistryOptions.network`).
 */
export interface NetworkSettings {
  /** `http://[user:password@]host[:port]`: a CONNECT tunnel for https targets, absolute-form requests for http. */
  readonly proxy?: string | undefined;
  /** Hosts reached directly, never through the proxy (`NO_PROXY` syntax, see {@link hostMatches}). */
  readonly noProxy?: readonly string[] | undefined;
  /** When non-empty, only hosts matching one of these patterns may be contacted. */
  readonly allowHosts?: readonly string[] | undefined;
  /** Hosts never contacted; a deny rule beats an allow rule. */
  readonly denyHosts?: readonly string[] | undefined;
  /** Time limit for establishing the TCP connection (to the target, or to the proxy), ms. */
  readonly connectTimeoutMs?: number | undefined;
  /** Longest wait for data once connected (idle time), ms. */
  readonly readTimeoutMs?: number | undefined;
}

/** A configured HTTP proxy, parsed. */
export interface ProxyConfig {
  readonly host: string;
  readonly port: number;
  /** `Basic …` when the URL carries credentials. */
  readonly authorization?: string | undefined;
  /** The proxy URL without credentials, for messages. */
  readonly shown: string;
}

/** Split a list given as one string (commas and/or whitespace). */
export function splitList(raw: string | undefined): string[] {
  return (raw ?? '')
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** A proxy URL with any credentials removed. */
function shownProxy(raw: string): string {
  try {
    const u = new URL(raw);
    u.username = '';
    u.password = '';
    return u.href.replace(/\/$/, '');
  } catch {
    return '<invalid url>';
  }
}

/** Percent-decode, keeping invalid escapes as written (bytes decoded as UTF-8, lossily). */
function percentDecode(s: string): string {
  const bytes: number[] = [];
  for (let i = 0; i < s.length; ) {
    if (s[i] === '%' && /^[0-9a-fA-F]{2}$/.test(s.slice(i + 1, i + 3))) {
      bytes.push(parseInt(s.slice(i + 1, i + 3), 16));
      i += 3;
    } else {
      const c = Buffer.from(s[i] as string, 'utf8');
      bytes.push(...c);
      i += 1;
    }
  }
  return Buffer.from(bytes).toString('utf8');
}

/** Parse a proxy URL, or throw E_USAGE. Credentials never appear in the message. */
export function parseProxy(raw: string): ProxyConfig {
  const bad = () => new SitefileError('E_USAGE', `invalid proxy URL '${shownProxy(raw)}': expected http://[user:password@]host[:port]`);
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw bad();
  }
  if (u.protocol !== 'http:' || u.hostname === '' || (u.pathname !== '/' && u.pathname !== '') || u.search !== '' || u.hash !== '') throw bad();
  const host = u.hostname.replace(/^\[|\]$/g, '');
  const port = u.port === '' ? 80 : Number(u.port);
  const user = percentDecode(u.username);
  const pass = percentDecode(u.password);
  const authorization = user !== '' || pass !== '' ? `Basic ${Buffer.from(`${user}:${pass}`).toString('base64')}` : undefined;
  return { host, port, authorization, shown: shownProxy(raw) };
}

// ------------------------------------------------------------------ host patterns

interface Pattern {
  /** `*`: everything. */
  readonly any?: true;
  /** Domain: the host itself or any subdomain. */
  readonly domain?: string;
  /** IP network: family, address as bigint, prefix length. */
  readonly net?: { readonly family: 4 | 6; readonly addr: bigint; readonly prefix: number };
  readonly port?: number;
}

function ipToBig(ip: string): { family: 4 | 6; addr: bigint } | undefined {
  const family = isIP(ip);
  if (family === 4) {
    const addr = ip.split('.').reduce((acc, b) => (acc << 8n) | BigInt(Number(b)), 0n);
    return { family: 4, addr };
  }
  if (family === 6) {
    let s = ip.toLowerCase();
    const v4 = /(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(s);
    if (v4) {
      const [b0, b1, b2, b3] = v4.slice(1).map(Number) as [number, number, number, number];
      s = s.slice(0, v4.index) + ((b0 << 8) | b1).toString(16) + ':' + ((b2 << 8) | b3).toString(16);
    }
    const [head, tail] = s.split('::') as [string, string | undefined];
    const h = head ? head.split(':') : [];
    const t = tail ? tail.split(':') : [];
    const groups = [...h, ...(tail === undefined ? [] : new Array<string>(8 - h.length - t.length).fill('0')), ...t];
    if (groups.length !== 8) return undefined;
    return { family: 6, addr: groups.reduce((acc, g) => (acc << 16n) | BigInt(parseInt(g, 16)), 0n) };
  }
  return undefined;
}

const HOSTNAME_RE = /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/;

/** Parse one pattern; undefined when it is not valid. */
function parsePattern(raw: string): Pattern | undefined {
  let p = raw.trim().toLowerCase();
  if (p === '') return undefined;
  if (p === '*') return { any: true };
  let port: number | undefined;
  // [v6]:port, [v6], v4:port, name:port. A bare IPv6 (two or more colons) has no port.
  const bracket = /^\[([^\]]+)\](?::(\d+))?$/.exec(p);
  if (bracket) {
    p = bracket[1] as string;
    if (bracket[2] !== undefined) port = Number(bracket[2]);
  } else if ((p.match(/:/g) ?? []).length === 1) {
    const i = p.indexOf(':');
    const ps = p.slice(i + 1);
    if (!/^\d+$/.test(ps)) return undefined;
    port = Number(ps);
    p = p.slice(0, i);
  }
  if (port !== undefined && (port < 1 || port > 65535)) return undefined;
  const withPort = <T extends Pattern>(x: T): Pattern => (port === undefined ? x : { ...x, port });
  const slash = p.indexOf('/');
  if (slash !== -1) {
    const ip = ipToBig(p.slice(0, slash));
    const ps = p.slice(slash + 1);
    if (!ip || !/^\d+$/.test(ps)) return undefined;
    const prefix = Number(ps);
    if (prefix > (ip.family === 4 ? 32 : 128)) return undefined;
    return withPort({ net: { ...ip, prefix } });
  }
  const ip = ipToBig(p);
  if (ip) return withPort({ net: { ...ip, prefix: ip.family === 4 ? 32 : 128 } });
  if (p.startsWith('*.')) p = p.slice(2);
  else if (p.startsWith('.')) p = p.slice(1);
  if (p.endsWith('.')) p = p.slice(0, -1);
  if (!HOSTNAME_RE.test(p)) return undefined;
  return withPort({ domain: p });
}

/**
 * True when `host` (as in a URL, IPv6 without brackets) on `port` matches `pattern`:
 *   `*`                      every host
 *   `example.com`            example.com and every subdomain (`.example.com` and `*.example.com` too)
 *   `10.0.0.0/8`, `fd00::/8` IP-literal hosts in that network; `192.0.2.1`, `::1` that address
 *   any of these + `:port`   only that port (`[::1]:8080` for IPv6)
 * Patterns match the host as written in the URL: a name pattern never matches an IP literal and an
 * IP pattern never matches a name, whatever it resolves to (the private-address rule covers that).
 */
export function hostMatches(host: string, port: number, pattern: string): boolean {
  const p = parsePattern(pattern);
  return p !== undefined && matches(normalizeHost(host), port, p);
}

function normalizeHost(host: string): string {
  let h = host.toLowerCase();
  if (h.startsWith('[') && h.endsWith(']')) h = h.slice(1, -1);
  if (h.endsWith('.')) h = h.slice(0, -1);
  return h;
}

function matches(host: string, port: number, p: Pattern): boolean {
  if (p.any) return true;
  if (p.port !== undefined && p.port !== port) return false;
  if (p.domain !== undefined) return host === p.domain || host.endsWith(`.${p.domain}`);
  if (p.net) {
    const ip = ipToBig(host);
    if (!ip || ip.family !== p.net.family) return false;
    const bits = BigInt(ip.family === 4 ? 32 : 128);
    const shift = bits - BigInt(p.net.prefix);
    return ip.addr >> shift === p.net.addr >> shift;
  }
  return false;
}

/** The first pattern of `list` that matches, if any. */
function firstMatch(host: string, port: number, list: readonly string[] | undefined): string | undefined {
  const h = normalizeHost(host);
  for (const raw of list ?? []) {
    const p = parsePattern(raw);
    if (p && matches(h, port, p)) return raw.trim();
  }
  return undefined;
}

/** Default port of an http(s) URL. */
export function urlPort(url: URL): number {
  return url.port !== '' ? Number(url.port) : url.protocol === 'http:' ? 80 : 443;
}

/**
 * Apply the allow/deny host rules to one hop (every redirect, token realm and blob CDN hop included).
 * Throws E_HOST_BLOCKED naming the rule; a deny rule beats an allow rule.
 */
export function checkHostRules(url: URL, network: NetworkSettings | undefined, shown: string, span?: SourceSpan): void {
  if (!network) return;
  const host = normalizeHost(url.hostname);
  const port = urlPort(url);
  const denied = firstMatch(host, port, network.denyHosts);
  if (denied !== undefined) {
    throw new SitefileError('E_HOST_BLOCKED', `refusing to connect to ${host}: it matches deny-hosts rule '${denied}' (${shown})`, span);
  }
  if ((network.allowHosts ?? []).length > 0 && firstMatch(host, port, network.allowHosts) === undefined) {
    throw new SitefileError('E_HOST_BLOCKED', `refusing to connect to ${host}: it matches no allow-hosts rule (${shown})`, span);
  }
}

/** The proxy to use for `url`, or undefined for a direct connection (no proxy, or a no-proxy match). */
export function proxyFor(url: URL, network: NetworkSettings | undefined): ProxyConfig | undefined {
  if (!network?.proxy) return undefined;
  if (firstMatch(url.hostname, urlPort(url), network.noProxy) !== undefined) return undefined;
  return parseProxy(network.proxy);
}

// ------------------------------------------------------------------ configuration

/** A usage problem in `network` (bad proxy URL, host pattern or timeout), or undefined. */
export function networkProblem(network: NetworkSettings): string | undefined {
  if (network.proxy) {
    try {
      parseProxy(network.proxy);
    } catch (e) {
      return (e as Error).message;
    }
  }
  for (const [name, list] of [
    ['no-proxy', network.noProxy],
    ['allow-hosts', network.allowHosts],
    ['deny-hosts', network.denyHosts],
  ] as const) {
    for (const p of list ?? []) {
      if (!parsePattern(p)) return `invalid ${name} pattern '${p}': expected *, a host name (example.com, .example.com, *.example.com), an IP address or a CIDR network, optionally with :port`;
    }
  }
  for (const [name, v] of [
    ['connect timeout', network.connectTimeoutMs],
    ['read timeout', network.readTimeoutMs],
  ] as const) {
    if (v !== undefined && (!Number.isSafeInteger(v) || v <= 0)) return `the ${name} must be a positive integer (ms)`;
  }
  return undefined;
}

function positiveMs(raw: string, what: string): number {
  if (!/^\d+$/.test(raw) || Number(raw) <= 0 || !Number.isSafeInteger(Number(raw))) {
    throw new SitefileError('E_USAGE', `${what} expects a positive integer (ms), got '${raw}'`);
  }
  return Number(raw);
}

/** Parse `SITEFILE_FETCH_TIMEOUT`: `<ms>` (connect and read) or `connect=<ms>,read=<ms>` (either part optional). */
export function parseFetchTimeout(raw: string): { connectTimeoutMs?: number; readTimeoutMs?: number } {
  const s = raw.trim();
  if (s === '') return {};
  if (/^\d+$/.test(s)) {
    const ms = positiveMs(s, 'SITEFILE_FETCH_TIMEOUT');
    return { connectTimeoutMs: ms, readTimeoutMs: ms };
  }
  const out: { connectTimeoutMs?: number; readTimeoutMs?: number } = {};
  for (const part of s.split(',')) {
    const m = /^\s*(connect|read)\s*=\s*(\S*)\s*$/.exec(part);
    if (!m) throw new SitefileError('E_USAGE', `SITEFILE_FETCH_TIMEOUT expects <ms> or connect=<ms>,read=<ms>, got '${raw}'`);
    const ms = positiveMs(m[2] as string, `SITEFILE_FETCH_TIMEOUT ${m[1]}`);
    if (m[1] === 'connect') out.connectTimeoutMs = ms;
    else out.readTimeoutMs = ms;
  }
  return out;
}

/**
 * Network settings from `SITEFILE_PROXY`, `SITEFILE_NO_PROXY`, `SITEFILE_ALLOW_HOSTS`,
 * `SITEFILE_DENY_HOSTS` (comma- or space-separated) and `SITEFILE_FETCH_TIMEOUT`. Empty values are unset.
 * `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` are never read.
 */
export function networkFromEnv(env: NodeJS.ProcessEnv = process.env): NetworkSettings {
  const proxy = (env.SITEFILE_PROXY ?? '').trim();
  return {
    ...(proxy !== '' ? { proxy } : {}),
    ...(splitList(env.SITEFILE_NO_PROXY).length ? { noProxy: splitList(env.SITEFILE_NO_PROXY) } : {}),
    ...(splitList(env.SITEFILE_ALLOW_HOSTS).length ? { allowHosts: splitList(env.SITEFILE_ALLOW_HOSTS) } : {}),
    ...(splitList(env.SITEFILE_DENY_HOSTS).length ? { denyHosts: splitList(env.SITEFILE_DENY_HOSTS) } : {}),
    ...parseFetchTimeout(env.SITEFILE_FETCH_TIMEOUT ?? ''),
  };
}

/** `over` on top of `base`: the proxy and timeouts are replaced, the host lists are joined (duplicates dropped). */
export function mergeNetwork(base: NetworkSettings, over: NetworkSettings): NetworkSettings {
  const join = (a: readonly string[] | undefined, b: readonly string[] | undefined) => {
    const all = [...new Set([...(a ?? []), ...(b ?? [])])];
    return all.length ? all : undefined;
  };
  const out: Record<string, unknown> = {
    proxy: over.proxy ?? base.proxy,
    noProxy: join(base.noProxy, over.noProxy),
    allowHosts: join(base.allowHosts, over.allowHosts),
    denyHosts: join(base.denyHosts, over.denyHosts),
    connectTimeoutMs: over.connectTimeoutMs ?? base.connectTimeoutMs,
    readTimeoutMs: over.readTimeoutMs ?? base.readTimeoutMs,
  };
  for (const k of Object.keys(out)) if (out[k] === undefined) delete out[k];
  return out as NetworkSettings;
}

/** True when any setting is present. */
export function hasNetworkSettings(n: NetworkSettings | undefined): boolean {
  return !!n && Object.values(n).some((v) => v !== undefined && !(Array.isArray(v) && v.length === 0));
}
