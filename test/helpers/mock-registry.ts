import { createHash, randomUUID } from 'node:crypto';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { gzipTar, writeTar } from '../../src/archive-writers.js';
import { canonicalJson } from '../../src/oci.js';
import { makeFile, Tree } from '../../src/tree.js';

const sha = (b: Uint8Array | string): string => 'sha256:' + createHash('sha256').update(b).digest('hex');

export interface MockOptions {
  /** 'none' (default), 'basic', or 'bearer' (token endpoint on the same server). */
  readonly auth?: 'none' | 'basic' | 'bearer';
  readonly user?: string;
  readonly pass?: string;
  /** Bearer: allow anonymous pull tokens (default true). */
  readonly anonymousPull?: boolean;
  /** Serve blob GETs through a redirect to this base URL (a CDN) instead of directly. */
  readonly redirectBlobsTo?: string;
  /** Serve different bytes for this blob digest (corruption test). */
  readonly corrupt?: string;
  /** Answer GET manifest by digest with this other manifest (digest mismatch test). */
  readonly swapManifest?: Buffer;
  /** Act as a registry with the OCI 1.1 referrers API: a PUT manifest with a subject answers OCI-Subject. */
  readonly referrersApi?: boolean;
}

export interface MockRequest {
  readonly method: string;
  readonly url: string;
  readonly authorization: string | undefined;
}

export interface MockRegistry {
  /** `127.0.0.1:<port>`: usable as a registry host (loopback, so plain http is allowed). */
  readonly host: string;
  readonly requests: MockRequest[];
  readonly blobs: Map<string, Buffer>;
  /** `repo@digest` and `repo:tag` -> manifest bytes. */
  readonly manifests: Map<string, Buffer>;
  readonly tokenCalls: string[];
  /** Mutable: swap behaviour mid-test. */
  options: MockOptions;
  seedImage(repo: string, tag: string, img: SeedImage): SeededImage;
  close(): Promise<void>;
}

export interface SeedImage {
  /** Files per layer, bottom first. */
  readonly layers: ReadonlyArray<Record<string, string>>;
  readonly config?: Record<string, unknown>;
  readonly os?: string;
  readonly arch?: string;
  /** Use Docker v2 media types (manifest, config, layers) instead of OCI. */
  readonly docker?: boolean;
  readonly created?: string;
}

export interface SeededImage {
  readonly manifestDigest: string;
  readonly manifestBytes: Buffer;
  readonly configDigest: string;
  readonly layerDigests: string[];
  readonly diffIds: string[];
}

/** An in-memory OCI distribution registry, just enough for push/pull/BASE tests. */
export async function startMockRegistry(options: MockOptions = {}): Promise<MockRegistry> {
  const blobs = new Map<string, Buffer>();
  const manifests = new Map<string, Buffer>();
  const repoBlobs = new Map<string, Set<string>>();
  const uploads = new Map<string, string>();
  const requests: MockRequest[] = [];
  const tokenCalls: string[] = [];
  const reg = { options } as { options: MockOptions };
  let port = 0;

  const hasBlob = (repo: string, d: string): boolean => repoBlobs.get(repo)?.has(d) === true && blobs.has(d);
  const addBlob = (repo: string, d: string, b: Buffer): void => {
    blobs.set(d, b);
    let s = repoBlobs.get(repo);
    if (!s) repoBlobs.set(repo, (s = new Set()));
    s.add(d);
  };

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', `http://127.0.0.1:${port}`);
    const method = req.method ?? 'GET';
    requests.push({ method, url: url.pathname + url.search, authorization: req.headers.authorization });
    const o = reg.options;
    const send = (status: number, body?: Buffer | string, headers: Record<string, string> = {}): void => {
      res.writeHead(status, headers);
      res.end(body);
    };
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const body = Buffer.concat(chunks);

    if (url.pathname === '/token') {
      tokenCalls.push(url.search);
      const auth = req.headers.authorization;
      const scopes = url.searchParams.getAll('scope');
      const wantsPush = scopes.some((s) => s.includes('push'));
      const ok = auth === `Basic ${Buffer.from(`${o.user ?? 'u'}:${o.pass ?? 'p'}`).toString('base64')}`;
      if (ok) return send(200, JSON.stringify({ token: 'tok-full' }), { 'content-type': 'application/json' });
      if (!auth && !wantsPush && o.anonymousPull !== false) return send(200, JSON.stringify({ token: 'tok-anon' }), { 'content-type': 'application/json' });
      if (auth) return send(401, '{"errors":[{"code":"UNAUTHORIZED","message":"bad credentials"}]}');
      return send(200, JSON.stringify({ token: 'tok-denied' }), { 'content-type': 'application/json' }); // like Docker Hub: a token without the push scope
    }

    const writes = method === 'POST' || method === 'PUT';
    const m = /^\/v2\/(.+?)\/(blobs\/uploads|blobs|manifests)\/?(.*)$/.exec(url.pathname);
    const repo = m?.[1] ?? '';
    if (o.auth === 'basic' && req.headers.authorization !== `Basic ${Buffer.from(`${o.user ?? 'u'}:${o.pass ?? 'p'}`).toString('base64')}`) {
      return send(401, '{"errors":[{"code":"UNAUTHORIZED","message":"auth"}]}', { 'www-authenticate': 'Basic realm="mock"' });
    }
    if (o.auth === 'bearer') {
      const tok = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1];
      const good = tok === 'tok-full' || (tok === 'tok-anon' && !writes);
      if (!good) {
        const scope = m ? `repository:${repo}:${writes ? 'pull,push' : 'pull'}` : '';
        return send(401, '{"errors":[{"code":"UNAUTHORIZED","message":"auth"}]}', {
          'www-authenticate': `Bearer realm="http://127.0.0.1:${port}/token",service="mock"${scope ? `,scope="${scope}"` : ''}`,
        });
      }
    }

    if (url.pathname === '/v2/' || url.pathname === '/v2') return send(200, '{}');
    if (!m) return send(404, '{"errors":[{"code":"NAME_UNKNOWN","message":"nope"}]}');
    const kind = m[2];
    const rest = m[3] ?? '';

    if (kind === 'blobs' && (method === 'GET' || method === 'HEAD')) {
      const b = blobs.get(rest);
      if (!b || !hasBlob(repo, rest)) return send(404, '{"errors":[{"code":"BLOB_UNKNOWN","message":"no"}]}');
      if (method === 'HEAD') return send(200, undefined, { 'content-length': String(b.length) });
      if (o.redirectBlobsTo) return send(307, undefined, { location: `${o.redirectBlobsTo}/blob/${rest.slice(7)}` });
      const out = o.corrupt === rest ? Buffer.alloc(b.length, 0x41) : b;
      return send(200, out, { 'content-length': String(out.length) });
    }
    if (kind === 'blobs/uploads' && method === 'POST') {
      const mount = url.searchParams.get('mount');
      const from = url.searchParams.get('from');
      if (mount && from && hasBlob(from, mount)) {
        addBlob(repo, mount, blobs.get(mount) as Buffer);
        return send(201, undefined, { location: `/v2/${repo}/blobs/${mount}` });
      }
      const id = randomUUID();
      uploads.set(id, repo);
      return send(202, undefined, { location: `/v2/${repo}/blobs/uploads/${id}?_state=x`, 'content-length': '0' });
    }
    if (kind === 'blobs/uploads' && method === 'PUT') {
      const d = url.searchParams.get('digest') ?? '';
      if (!uploads.has(rest) || sha(body) !== d) return send(400, '{"errors":[{"code":"DIGEST_INVALID","message":"bad digest"}]}');
      uploads.delete(rest);
      addBlob(repo, d, body);
      return send(201, undefined, { location: `/v2/${repo}/blobs/${d}`, 'docker-content-digest': d });
    }
    if (kind === 'manifests' && method === 'PUT') {
      const d = sha(body);
      manifests.set(`${repo}@${d}`, body);
      if (!/^sha256:/.test(rest)) manifests.set(`${repo}:${rest}`, body);
      const subject = (JSON.parse(body.toString('utf8')) as { subject?: { digest?: unknown } }).subject?.digest;
      const extra: Record<string, string> = o.referrersApi && typeof subject === 'string' ? { 'oci-subject': subject } : {};
      return send(201, undefined, { 'docker-content-digest': d, ...extra });
    }
    if (kind === 'manifests' && (method === 'GET' || method === 'HEAD')) {
      let b = manifests.get(rest.startsWith('sha256:') ? `${repo}@${rest}` : `${repo}:${rest}`);
      if (!b) return send(404, '{"errors":[{"code":"MANIFEST_UNKNOWN","message":"no such manifest"}]}');
      if (o.swapManifest && rest.startsWith('sha256:')) b = o.swapManifest;
      const mt = (JSON.parse(b.toString('utf8')) as { mediaType?: string }).mediaType ?? 'application/vnd.oci.image.manifest.v1+json';
      return send(200, method === 'HEAD' ? undefined : b, { 'content-type': mt, 'docker-content-digest': sha(b) });
    }
    return send(405, '');
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  port = (server.address() as AddressInfo).port;

  const mock: MockRegistry = {
    host: `127.0.0.1:${port}`,
    requests,
    blobs,
    manifests,
    tokenCalls,
    get options() {
      return reg.options;
    },
    set options(v: MockOptions) {
      reg.options = v;
    },
    seedImage(repo, tag, img) {
      const docker = !!img.docker;
      const layerDigests: string[] = [];
      const diffIds: string[] = [];
      for (const files of img.layers) {
        const tar = writeTar(Tree.fromFiles(Object.entries(files).map(([p, c]) => [p, makeFile(new TextEncoder().encode(c), 'seed')] as const)), { epoch: 0 });
        const gz = Buffer.from(gzipTar(tar));
        addBlob(repo, sha(gz), gz);
        layerDigests.push(sha(gz));
        diffIds.push(sha(tar));
      }
      const config = Buffer.from(
        canonicalJson({
          architecture: img.arch ?? 'amd64',
          os: img.os ?? 'linux',
          created: img.created ?? '2024-01-02T03:04:05Z',
          config: img.config ?? {},
          rootfs: { type: 'layers', diff_ids: diffIds },
          history: diffIds.map((_, i) => ({ created: '2024-01-02T03:04:05Z', created_by: `seed layer ${i}` })),
        }),
      );
      addBlob(repo, sha(config), config);
      const manifestBytes = Buffer.from(
        canonicalJson({
          schemaVersion: 2,
          mediaType: docker ? 'application/vnd.docker.distribution.manifest.v2+json' : 'application/vnd.oci.image.manifest.v1+json',
          config: { mediaType: docker ? 'application/vnd.docker.container.image.v1+json' : 'application/vnd.oci.image.config.v1+json', digest: sha(config), size: config.length },
          layers: layerDigests.map((d) => ({
            mediaType: docker ? 'application/vnd.docker.image.rootfs.diff.tar.gzip' : 'application/vnd.oci.image.layer.v1.tar+gzip',
            digest: d,
            size: (blobs.get(d) as Buffer).length,
          })),
        }),
      );
      manifests.set(`${repo}@${sha(manifestBytes)}`, manifestBytes);
      manifests.set(`${repo}:${tag}`, manifestBytes);
      return { manifestDigest: sha(manifestBytes), manifestBytes, configDigest: sha(config), layerDigests, diffIds };
    },
    close: () => new Promise((r) => server.close(() => r())),
  };
  return mock;
}

/** A tiny "CDN" serving the mock's blobs without any authentication; records what it saw. */
export async function startMockCdn(registry: MockRegistry): Promise<{ url: string; requests: MockRequest[]; close(): Promise<void> }> {
  const requests: MockRequest[] = [];
  const server = http.createServer((req, res) => {
    requests.push({ method: req.method ?? 'GET', url: req.url ?? '', authorization: req.headers.authorization });
    const hex = /^\/blob\/([0-9a-f]{64})$/.exec(req.url ?? '')?.[1];
    const b = hex ? registry.blobs.get(`sha256:${hex}`) : undefined;
    if (!b) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200, { 'content-length': String(b.length) }).end(b);
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  return { url: `http://127.0.0.1:${port}`, requests, close: () => new Promise((r) => server.close(() => r())) };
}
