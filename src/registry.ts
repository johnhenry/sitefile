import { request as httpRequest, type ClientRequest, type IncomingMessage, type RequestOptions } from 'node:http';
import { request as httpsRequest, type RequestOptions as HttpsRequestOptions } from 'node:https';
import { isIP } from 'node:net';
import { pipeline } from 'node:stream/promises';
import type { Readable } from 'node:stream';
import * as tls from 'node:tls';
import { sha256 } from './digest.js';
import { SitefileError, isSitefileError } from './errors.js';
import { MEDIA, type Descriptor } from './oci.js';
import { registryApiHost } from './ref.js';
import type { Credentials } from './registry-auth.js';
import { createGuardedLookup, redactUrl } from './resources/https.js';
import { isPublicAddress } from './resources/netpolicy.js';
import { DEFAULT_FETCH_POLICY } from './resources/types.js';

/**
 * Client for the OCI distribution API (distribution-spec v1.1): `/v2/` probe, Bearer-token and Basic
 * auth, manifest and blob GET/HEAD, monolithic blob upload, cross-repository mount, manifest PUT.
 *
 * Transport policy (the registry host is named by the user, so its address may be private):
 *   - https, except for loopback registries (`localhost`, 127.0.0.0/8, ::1) and hosts passed as
 *     insecure registries, which try https first and fall back to http;
 *   - redirects are followed manually (GET/HEAD only), at most 5, every hop re-validated. Registries
 *     redirect blob downloads to a CDN: a target on another origin must be https (unless both the
 *     registry and the target are loopback), must not carry credentials in the URL, and must resolve
 *     to public addresses (checked at connect time) unless `allowPrivateNetwork`;
 *   - the Authorization header is sent to the registry's own origin only, never to a redirect target;
 *   - token endpoints (the `realm` of a Bearer challenge) obey the same cross-origin rules;
 *   - bodies are never decompressed; responses are bounded by the limits below.
 * Tokens and passwords live in memory only and never appear in an error message.
 */
export interface RegistryLimits {
  /** Largest blob accepted (bytes). */
  readonly maxBlobBytes: number;
  /** Largest total of all blobs of one image (bytes). */
  readonly maxTotalBytes: number;
  /** Largest manifest / token response (bytes). */
  readonly maxManifestBytes: number;
  /** Idle/connect timeout per request (ms). */
  readonly timeoutMs: number;
  /** Time budget for one blob transfer (ms). */
  readonly blobDeadlineMs: number;
}

export const DEFAULT_REGISTRY_LIMITS: RegistryLimits = {
  maxBlobBytes: 2 * 1024 ** 3,
  maxTotalBytes: 8 * 1024 ** 3,
  maxManifestBytes: 4 * 1024 ** 2,
  timeoutMs: 30_000,
  blobDeadlineMs: 30 * 60_000,
};

export interface RegistryOptions {
  /** Registries (`host[:port]`) that may be reached over plain http. */
  readonly insecureRegistries?: readonly string[] | undefined;
  /** Let redirects and token endpoints on other origins resolve to private addresses. */
  readonly allowPrivateNetwork?: boolean | undefined;
  /** Asked lazily, only when the registry demands authentication. */
  readonly credentials?: (() => Promise<Credentials | undefined>) | undefined;
  readonly limits?: Partial<RegistryLimits> | undefined;
  /** Extra PEM CA certificate(s) (tests / private CAs). */
  readonly ca?: string | Buffer | undefined;
}

export interface Body {
  readonly length: number;
  readonly open: () => Readable | Uint8Array;
}

const REDIRECTS = new Set([301, 302, 303, 307, 308]);
const MAX_REDIRECTS = 5;
const NAME_RE = /^[a-z0-9]+(?:[._-][a-z0-9]+)*(?:\/[a-z0-9]+(?:[._-][a-z0-9]+)*)*$/;
const REF_RE = /^(?:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}|sha256:[0-9a-f]{64})$/;

export const MANIFEST_ACCEPT = [MEDIA.manifest, MEDIA.index, MEDIA.dockerManifest, MEDIA.dockerList].join(', ');

export interface ManifestResponse {
  readonly bytes: Uint8Array;
  readonly digest: string;
  readonly mediaType: string;
}

function hostnameOf(hostPort: string): string {
  if (hostPort.startsWith('[')) return hostPort.slice(1, hostPort.indexOf(']'));
  return hostPort.replace(/:\d+$/, '');
}

export function isLoopbackHost(hostname: string): boolean {
  const h = hostname.toLowerCase();
  return h === 'localhost' || h.endsWith('.localhost') || h === '::1' || /^127\.\d+\.\d+\.\d+$/.test(h);
}

function unwrap(e: unknown, depth = 0): SitefileError | undefined {
  if (depth > 4 || e === null || typeof e !== 'object') return undefined;
  if (isSitefileError(e)) return e;
  const x = e as { cause?: unknown; errors?: unknown };
  if (Array.isArray(x.errors)) for (const i of x.errors) {
    const f = unwrap(i, depth + 1);
    if (f) return f;
  }
  return unwrap(x.cause, depth + 1);
}

function parseChallenge(header: string | string[] | undefined): { scheme: string; params: Record<string, string> } | undefined {
  const h = Array.isArray(header) ? header[0] : header;
  if (!h) return undefined;
  const m = /^\s*([A-Za-z]+)\s*(.*)$/s.exec(h);
  if (!m) return undefined;
  const params: Record<string, string> = {};
  for (const p of (m[2] ?? '').matchAll(/([A-Za-z_]+)="([^"]*)"/g)) params[(p[1] as string).toLowerCase()] = p[2] as string;
  return { scheme: (m[1] as string).toLowerCase(), params };
}

async function readBody(res: IncomingMessage, max: number): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let n = 0;
  try {
    for await (const c of res as AsyncIterable<Buffer>) {
      n += c.length;
      if (n > max) {
        res.destroy();
        throw new SitefileError('E_TOO_LARGE', `registry response exceeded ${max} bytes`);
      }
      chunks.push(c);
    }
  } catch (e) {
    if (isSitefileError(e)) throw e;
    throw new SitefileError('E_REGISTRY', `error reading registry response: ${(e as Error).message}`, undefined, { cause: e });
  }
  return Buffer.concat(chunks, n);
}

function clean(s: string): string {
  // eslint-disable-next-line no-control-regex
  return s.replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, 200);
}

export class RegistryClient {
  readonly limits: RegistryLimits;
  private origin: URL | undefined;
  private readonly insecure: boolean;
  private readonly loopback: boolean;
  private readonly host: string;
  private readonly tokens = new Map<string, string>();
  private creds: { value: Credentials | undefined } | undefined;

  /** `registry` is the canonical registry name (`docker.io`, `ghcr.io`, `localhost:5000`). */
  constructor(
    readonly registry: string,
    private readonly options: RegistryOptions = {},
  ) {
    this.host = registryApiHost(registry);
    this.limits = { ...DEFAULT_REGISTRY_LIMITS, ...options.limits };
    this.loopback = isLoopbackHost(hostnameOf(this.host));
    this.insecure = this.loopback || (options.insecureRegistries ?? []).some((h) => h.toLowerCase() === this.host.toLowerCase() || h.toLowerCase() === registry.toLowerCase());
  }

  // ------------------------------------------------------------ transport

  private sameOrigin(u: URL): boolean {
    return this.origin !== undefined && u.protocol === this.origin.protocol && u.host === this.origin.host;
  }

  /** Refuse hops that leave the registry's origin unless they are safe. */
  private checkTarget(u: URL): void {
    if (this.sameOrigin(u)) return;
    const host = u.hostname.replace(/^\[|\]$/g, '');
    if (u.username !== '' || u.password !== '') throw new SitefileError('E_PROTOCOL', `refusing a URL with credentials (${redactUrl(u)})`);
    const loopbackPair = this.loopback && isLoopbackHost(host);
    if (u.protocol !== 'https:' && !(u.protocol === 'http:' && loopbackPair)) {
      throw new SitefileError('E_PROTOCOL', `${this.registry}: refusing to follow a redirect or token realm to ${redactUrl(u)}: it must use https`);
    }
    if (isIP(host) !== 0 && !isPublicAddress(host) && !this.options.allowPrivateNetwork && !loopbackPair) {
      throw new SitefileError('E_SSRF_BLOCKED', `${this.registry}: refusing to connect to non-public address ${host} (${redactUrl(u)}); use --allow-private-network to permit it`);
    }
  }

  private sendOnce(method: string, url: URL, headers: Record<string, string>, body: Body | undefined, timeoutMs: number): Promise<IncomingMessage> {
    return new Promise((resolve, reject) => {
      const same = this.sameOrigin(url);
      const host = url.hostname.replace(/^\[|\]$/g, '');
      const loopbackPair = this.loopback && isLoopbackHost(host);
      const policy = { ...DEFAULT_FETCH_POLICY, allowPrivateNetwork: same || loopbackPair || !!this.options.allowPrivateNetwork };
      const h: Record<string, string> = { 'accept-encoding': 'identity', 'user-agent': 'sitefile/0.1', ...headers };
      if (body) h['content-length'] = String(body.length);
      const o: RequestOptions = { method, headers: h, lookup: createGuardedLookup(policy), agent: false, timeout: timeoutMs };
      if (url.protocol === 'https:' && this.options.ca !== undefined) {
        const get = (tls as { getCACertificates?: (t?: string) => string[] }).getCACertificates;
        (o as HttpsRequestOptions).ca = [...(get ? get('default') : tls.rootCertificates), this.options.ca as string];
      }
      let req: ClientRequest;
      try {
        req = (url.protocol === 'https:' ? httpsRequest : httpRequest)(url, o);
      } catch (e) {
        reject(new SitefileError('E_REGISTRY', `cannot request ${redactUrl(url)}: ${(e as Error).message}`, undefined, { cause: e }));
        return;
      }
      req.on('timeout', () => req.destroy(new SitefileError('E_FETCH_TIMEOUT', `${method} ${redactUrl(url)} timed out after ${timeoutMs} ms`)));
      req.on('response', resolve);
      req.on('error', (e) => reject(unwrap(e) ?? new SitefileError('E_REGISTRY', `${method} ${redactUrl(url)} failed: ${e.message}`, undefined, { cause: e })));
      if (!body) {
        req.end();
        return;
      }
      const data = body.open();
      if (data instanceof Uint8Array) req.end(data);
      else pipeline(data, req).catch((e: unknown) => req.destroy(e as Error));
    });
  }

  /** One logical request: follows redirects (GET/HEAD) re-validating each hop; auth only at the registry origin. */
  private async follow(method: string, url: URL, headers: Record<string, string>, body: Body | undefined, auth: string | undefined, timeoutMs: number): Promise<IncomingMessage> {
    let cur = url;
    for (let hops = 0; ; hops++) {
      this.checkTarget(cur);
      const h = { ...headers };
      if (auth && this.sameOrigin(cur)) h.authorization = auth;
      const res = await this.sendOnce(method, cur, h, body, timeoutMs);
      const status = res.statusCode ?? 0;
      if (!REDIRECTS.has(status) || (method !== 'GET' && method !== 'HEAD')) return res;
      const loc = res.headers.location;
      res.resume();
      if (!loc) throw new SitefileError('E_REDIRECT', `redirect ${status} from ${redactUrl(cur)} has no Location header`);
      if (hops >= MAX_REDIRECTS) throw new SitefileError('E_REDIRECT', `too many redirects from ${redactUrl(url)} (limit ${MAX_REDIRECTS})`);
      try {
        cur = new URL(loc, cur);
      } catch (e) {
        throw new SitefileError('E_REDIRECT', `invalid redirect Location from ${redactUrl(cur)}`, undefined, { cause: e });
      }
    }
  }

  /** Find the base URL: https, or (insecure registries only) http when https is unreachable. */
  private async init(): Promise<URL> {
    if (this.origin) return this.origin;
    const https = new URL(`https://${this.host}/`);
    if (!this.insecure) {
      this.origin = https;
      return https;
    }
    this.origin = https;
    try {
      const r = await this.sendOnce('GET', new URL('/v2/', https), {}, undefined, Math.min(5000, this.limits.timeoutMs));
      r.resume();
      return https;
    } catch (e) {
      if (isSitefileError(e) && e.code !== 'E_REGISTRY' && e.code !== 'E_FETCH_TIMEOUT') throw e;
      this.origin = new URL(`http://${this.host}/`);
      return this.origin;
    }
  }

  private async getCreds(): Promise<Credentials | undefined> {
    this.creds ??= { value: await this.options.credentials?.() };
    return this.creds.value;
  }

  // ------------------------------------------------------------ auth

  private async fetchToken(chal: { params: Record<string, string> }, scope: string, creds: Credentials | undefined): Promise<string> {
    const realm = chal.params.realm;
    let url: URL;
    try {
      url = new URL(realm ?? '');
    } catch {
      throw new SitefileError('E_AUTH', `${this.registry}: the registry's token challenge has no usable realm`);
    }
    if (chal.params.service) url.searchParams.set('service', chal.params.service);
    for (const s of scope.split(' ').filter(Boolean)) url.searchParams.append('scope', s);
    const headers: Record<string, string> = { accept: 'application/json' };
    let method = 'GET';
    let body: Body | undefined;
    if (creds?.identityToken) {
      method = 'POST';
      const form = Buffer.from(
        new URLSearchParams({ grant_type: 'refresh_token', service: chal.params.service ?? '', scope, client_id: 'sitefile', refresh_token: creds.identityToken }).toString(),
      );
      url.search = '';
      headers['content-type'] = 'application/x-www-form-urlencoded';
      body = { length: form.length, open: () => form };
    } else if (creds?.username && creds.password !== undefined) {
      headers.authorization = `Basic ${Buffer.from(`${creds.username}:${creds.password}`).toString('base64')}`;
    }
    const res = await this.follow(method, url, headers, body, undefined, this.limits.timeoutMs);
    if (res.statusCode !== 200) {
      res.resume();
      const s = res.statusCode ?? 0;
      throw new SitefileError('E_AUTH', `${this.registry}: the token endpoint ${redactUrl(url)} answered HTTP ${s}${s === 401 || s === 403 ? ' (credentials rejected or missing)' : ''}`);
    }
    let j: { token?: unknown; access_token?: unknown };
    try {
      j = JSON.parse((await readBody(res, this.limits.maxManifestBytes)).toString('utf8')) as typeof j;
    } catch (e) {
      if (isSitefileError(e)) throw e;
      throw new SitefileError('E_AUTH', `${this.registry}: the token endpoint returned unreadable output`);
    }
    const token = typeof j.token === 'string' && j.token !== '' ? j.token : typeof j.access_token === 'string' && j.access_token !== '' ? j.access_token : undefined;
    if (!token) throw new SitefileError('E_AUTH', `${this.registry}: the token endpoint returned no token`);
    return token;
  }

  private async authenticate(chal: { scheme: string; params: Record<string, string> }, scope: string, useCreds: boolean): Promise<string> {
    const needCreds = useCreds || chal.scheme === 'basic';
    const creds = needCreds ? await this.getCreds() : undefined;
    if (needCreds && !creds) {
      throw new SitefileError(
        'E_AUTH',
        `${this.registry}: authentication required but no credentials were found (use "docker login ${this.registry}", $SITEFILE_REGISTRY_AUTH, or --username with --password-stdin)`,
      );
    }
    if (chal.scheme === 'basic') {
      if (!creds?.username || creds.password === undefined) throw new SitefileError('E_AUTH', `${this.registry}: Basic authentication needs a username and password`);
      return `Basic ${Buffer.from(`${creds.username}:${creds.password}`).toString('base64')}`;
    }
    if (chal.scheme === 'bearer') return `Bearer ${await this.fetchToken(chal, chal.params.scope || scope, creds)}`;
    throw new SitefileError('E_AUTH', `${this.registry}: unsupported authentication scheme '${clean(chal.scheme)}'`);
  }

  /** Send a request, answering a 401 challenge (anonymous token first, then credentials). */
  private async call(method: string, url: URL, o: { scope: string; headers?: Record<string, string>; body?: Body; timeoutMs?: number }): Promise<IncomingMessage> {
    await this.init();
    const timeout = o.timeoutMs ?? this.limits.timeoutMs;
    let usedCreds = false;
    for (let attempt = 0; ; attempt++) {
      const res = await this.follow(method, url, o.headers ?? {}, o.body, this.tokens.get(o.scope), timeout);
      if (res.statusCode !== 401 || attempt >= 2) return res;
      const chal = parseChallenge(res.headers['www-authenticate']);
      res.resume();
      if (!chal) return res;
      // Pushing needs credentials from the start; for pulls try an anonymous token before them.
      let wantCreds = o.scope.split(' ').some((x) => x.endsWith(',push') || x.endsWith(':push'));
      if (attempt === 1) {
        if (usedCreds) throw new SitefileError('E_AUTH', `${this.registry}: access denied (the credentials were rejected or lack permission)`);
        wantCreds = true;
      }
      this.tokens.set(o.scope, await this.authenticate(chal, o.scope, wantCreds));
      usedCreds = wantCreds || chal.scheme === 'basic';
    }
  }

  private v2(p: string): URL {
    return new URL(`/v2/${p}`, this.origin);
  }

  private async fail(res: IncomingMessage, what: string): Promise<never> {
    const status = res.statusCode ?? 0;
    let detail = '';
    try {
      const j = JSON.parse((await readBody(res, 64 * 1024)).toString('utf8')) as { errors?: Array<{ code?: unknown; message?: unknown }> };
      detail = (j.errors ?? [])
        .slice(0, 3)
        .map((e) => [e.code, e.message].filter((x): x is string => typeof x === 'string').map(clean).join(': '))
        .filter(Boolean)
        .join('; ');
    } catch {
      res.destroy();
    }
    const msg = `${this.registry}: ${what} failed: HTTP ${status}${detail ? ` (${detail})` : ''}`;
    if (status === 401 || status === 403) throw new SitefileError('E_AUTH', `${msg}; check your credentials and permissions`);
    if (status === 404) throw new SitefileError('E_NOT_FOUND', msg);
    throw new SitefileError('E_REGISTRY', msg);
  }

  private repoPath(repo: string): string {
    if (!NAME_RE.test(repo)) throw new SitefileError('E_USAGE', `invalid repository name '${repo}'`);
    return repo;
  }

  // ------------------------------------------------------------ API

  /** `GET /v2/`: confirms the endpoint speaks the distribution API (authenticating if it demands it). */
  async ping(): Promise<void> {
    const res = await this.call('GET', new URL('/v2/', await this.init()), { scope: '' });
    if (res.statusCode === 200) {
      res.resume();
      return;
    }
    await this.fail(res, 'GET /v2/');
  }

  async getManifest(repo: string, reference: string): Promise<ManifestResponse> {
    if (!REF_RE.test(reference)) throw new SitefileError('E_USAGE', `invalid reference '${reference}'`);
    await this.init();
    const res = await this.call('GET', this.v2(`${this.repoPath(repo)}/manifests/${reference}`), { scope: `repository:${repo}:pull`, headers: { accept: MANIFEST_ACCEPT } });
    if (res.statusCode !== 200) return this.fail(res, `GET manifest ${repo}:${clean(reference)}`);
    const bytes = new Uint8Array(await readBody(res, this.limits.maxManifestBytes));
    const digest = sha256(bytes);
    if (reference.startsWith('sha256:') && digest !== reference) {
      throw new SitefileError('E_INTEGRITY', `${this.registry}/${repo}: manifest content does not match the requested digest ${reference}`);
    }
    const claimed = res.headers['docker-content-digest'];
    if (typeof claimed === 'string' && claimed !== digest) {
      throw new SitefileError('E_INTEGRITY', `${this.registry}/${repo}: manifest does not match the digest the registry reported (${clean(claimed)})`);
    }
    let mediaType = String(res.headers['content-type'] ?? '').split(';')[0]?.trim().toLowerCase() ?? '';
    if (mediaType === '' || mediaType === 'application/json' || mediaType === 'text/plain') {
      try {
        const j = JSON.parse(Buffer.from(bytes).toString('utf8')) as { mediaType?: unknown };
        if (typeof j.mediaType === 'string') mediaType = j.mediaType;
      } catch {
        /* handled by the caller */
      }
    }
    return { bytes, digest, mediaType };
  }

  async hasBlob(repo: string, digest: string): Promise<boolean> {
    await this.init();
    const res = await this.call('HEAD', this.v2(`${this.repoPath(repo)}/blobs/${digest}`), { scope: `repository:${repo}:pull,push` });
    res.resume();
    if (res.statusCode === 200) return true;
    if (res.statusCode === 404) return false;
    return this.fail(res, `HEAD blob ${digest}`);
  }

  /** Stream a blob's bytes (redirects followed under the policy above). The caller verifies size and digest. */
  async *getBlob(repo: string, desc: Descriptor): AsyncGenerator<Buffer> {
    await this.init();
    if (desc.size > this.limits.maxBlobBytes) {
      throw new SitefileError('E_TOO_LARGE', `blob ${desc.digest} is ${desc.size} bytes, exceeding the limit of ${this.limits.maxBlobBytes} bytes`);
    }
    const res = await this.call('GET', this.v2(`${this.repoPath(repo)}/blobs/${desc.digest}`), { scope: `repository:${repo}:pull`, timeoutMs: this.limits.timeoutMs });
    if (res.statusCode !== 200) return void (await this.fail(res, `GET blob ${desc.digest}`));
    const enc = String(res.headers['content-encoding'] ?? '').trim().toLowerCase();
    if (enc !== '' && enc !== 'identity') {
      res.destroy();
      throw new SitefileError('E_REGISTRY', `${this.registry}: blob ${desc.digest} came with content-encoding '${clean(enc)}'; only identity is accepted`);
    }
    const declared = Number(res.headers['content-length']);
    if (Number.isFinite(declared) && declared > desc.size) {
      res.destroy();
      throw new SitefileError('E_TOO_LARGE', `blob ${desc.digest}: the server sent ${declared} bytes for a ${desc.size}-byte blob`);
    }
    const timer = setTimeout(() => res.destroy(new SitefileError('E_FETCH_TIMEOUT', `downloading blob ${desc.digest} exceeded ${this.limits.blobDeadlineMs} ms`)), this.limits.blobDeadlineMs);
    timer.unref?.();
    try {
      for await (const chunk of res as AsyncIterable<Buffer>) yield chunk;
    } catch (e) {
      throw unwrap(e) ?? new SitefileError('E_REGISTRY', `error downloading blob ${desc.digest}: ${(e as Error).message}`, undefined, { cause: e });
    } finally {
      clearTimeout(timer);
      res.destroy();
    }
  }

  /**
   * Make a blob exist in `repo`: tries a cross-repository mount from `mountFrom` (same registry) when
   * given, else a monolithic upload (POST then PUT).
   */
  async pushBlob(repo: string, desc: Descriptor, body: Body, mountFrom?: string): Promise<'mounted' | 'uploaded'> {
    await this.init();
    const scope = `repository:${repo}:pull,push${mountFrom ? ` repository:${mountFrom}:pull` : ''}`;
    const start = this.v2(`${this.repoPath(repo)}/blobs/uploads/`);
    if (mountFrom) {
      start.searchParams.set('mount', desc.digest);
      start.searchParams.set('from', this.repoPath(mountFrom));
    }
    const post = await this.call('POST', start, { scope });
    if (post.statusCode === 201) {
      post.resume();
      return 'mounted';
    }
    if (post.statusCode !== 202) return this.fail(post, `start upload of ${desc.digest}`);
    post.resume();
    const loc = post.headers.location;
    if (!loc) throw new SitefileError('E_REGISTRY', `${this.registry}: upload response has no Location header`);
    let target: URL;
    try {
      target = new URL(loc, start);
    } catch (e) {
      throw new SitefileError('E_REGISTRY', `${this.registry}: invalid upload Location`, undefined, { cause: e });
    }
    target.searchParams.set('digest', desc.digest);
    const put = await this.call('PUT', target, { scope, headers: { 'content-type': 'application/octet-stream' }, body, timeoutMs: this.limits.blobDeadlineMs });
    if (put.statusCode === 201 || put.statusCode === 204) {
      put.resume();
      return 'uploaded';
    }
    return this.fail(put, `upload of ${desc.digest}`);
  }

  /** `PUT` a manifest at a tag or digest; returns the digest the registry confirms. */
  async putManifest(repo: string, reference: string, bytes: Uint8Array, mediaType: string): Promise<string> {
    if (!REF_RE.test(reference)) throw new SitefileError('E_USAGE', `invalid reference '${reference}'`);
    await this.init();
    const res = await this.call('PUT', this.v2(`${this.repoPath(repo)}/manifests/${reference}`), {
      scope: `repository:${repo}:pull,push`,
      headers: { 'content-type': mediaType },
      body: { length: bytes.byteLength, open: () => bytes },
    });
    if (res.statusCode !== 201 && res.statusCode !== 200) return this.fail(res, `PUT manifest ${repo}:${clean(reference)}`);
    res.resume();
    const digest = sha256(bytes);
    const claimed = res.headers['docker-content-digest'];
    if (typeof claimed === 'string' && claimed !== digest) {
      throw new SitefileError('E_INTEGRITY', `${this.registry}/${repo}: the registry stored the manifest as ${clean(claimed)}, expected ${digest}`);
    }
    return digest;
  }
}
