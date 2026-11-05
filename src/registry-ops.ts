import { Buffer } from 'node:buffer';
import { SitefileError } from './errors.js';
import { MEDIA, parseConfig, parseManifest, parsePlatform, type Descriptor, type ParsedManifest } from './oci.js';
import { parseRef, withDefaultRegistry, type ImageRef } from './ref.js';
import type { RegistryClient } from './registry.js';
import { shortId, Store } from './store.js';

const CONFIG_TYPES = new Set<string>([MEDIA.config, MEDIA.dockerConfig]);
const INDEX_TYPES = new Set<string>([MEDIA.index, MEDIA.dockerList]);
const MANIFEST_TYPES = new Set<string>([MEDIA.manifest, MEDIA.dockerManifest]);

interface IndexJson {
  manifests?: Array<{ mediaType?: string; digest?: string; size?: number; platform?: { os?: string; architecture?: string; variant?: string }; annotations?: Record<string, string> }>;
}

/** Pick the manifest descriptor for `os/arch` from an image index / manifest list. */
export function selectPlatform(indexBytes: Uint8Array, platform: string): Descriptor {
  const want = parsePlatform(platform);
  let j: IndexJson;
  try {
    j = JSON.parse(Buffer.from(indexBytes).toString('utf8')) as IndexJson;
  } catch (e) {
    throw new SitefileError('E_REGISTRY', 'the registry returned an unreadable image index', undefined, { cause: e });
  }
  const list = Array.isArray(j.manifests) ? j.manifests : [];
  const have: string[] = [];
  const hits: Array<{ d: Descriptor; variant: string }> = [];
  for (const m of list) {
    const p = m.platform;
    if (!p || p.architecture === 'unknown' || typeof m.digest !== 'string' || !/^sha256:[0-9a-f]{64}$/.test(m.digest) || typeof m.size !== 'number') continue;
    have.push(`${p.os}/${p.architecture}${p.variant ? `/${p.variant}` : ''}`);
    if (p.os === want.os && p.architecture === want.arch) {
      hits.push({ d: { mediaType: String(m.mediaType ?? MEDIA.manifest), digest: m.digest, size: m.size }, variant: p.variant ?? '' });
    }
  }
  const best = hits.find((h) => h.variant === '') ?? hits[0];
  if (!best) {
    throw new SitefileError('E_REGISTRY', `the image has no ${platform} variant (available: ${[...new Set(have)].join(', ') || 'none'}); pass --platform`);
  }
  return best.d;
}

async function fetchManifestCached(store: Store, client: RegistryClient, repo: string, reference: string): Promise<{ bytes: Uint8Array; digest: string; mediaType: string }> {
  if (reference.startsWith('sha256:') && (await store.hasBlob(reference))) {
    try {
      const bytes = await store.readBlob(reference);
      return { bytes, digest: reference, mediaType: sniffMediaType(bytes) };
    } catch {
      /* corrupt cache entry: fetch again */
    }
  }
  const m = await client.getManifest(repo, reference);
  await store.putSmallBlob(m.bytes);
  return { ...m, mediaType: m.mediaType || sniffMediaType(m.bytes) };
}

function sniffMediaType(bytes: Uint8Array): string {
  try {
    const j = JSON.parse(Buffer.from(bytes).toString('utf8')) as { mediaType?: unknown; manifests?: unknown };
    if (typeof j.mediaType === 'string') return j.mediaType;
    return Array.isArray(j.manifests) ? MEDIA.index : MEDIA.manifest;
  } catch {
    return '';
  }
}

export interface PullOptions {
  /** `os/arch` used to pick from an index (default linux/amd64). */
  readonly platform?: string | undefined;
  /** Fail when the image's own platform differs from `platform` (BASE does; `pull` only with --platform). */
  readonly enforcePlatform?: boolean | undefined;
  /** Fetch this digest (an index or a manifest) instead of resolving the reference's tag. */
  readonly rootDigest?: string | undefined;
  /** Tag the image locally as the reference (registry/name:tag). */
  readonly tag?: boolean | undefined;
  readonly onEvent?: ((message: string) => void) | undefined;
}

export interface PullResult {
  /** The platform manifest digest: the local image ID. */
  readonly id: string;
  /** What the reference resolved to (an index digest for multi-platform images, else `id`). */
  readonly rootDigest: string;
  readonly manifest: ParsedManifest;
  readonly config: Readonly<Record<string, unknown>>;
}

/** Resolve a tag to the digest it currently points at (one manifest GET), caching the manifest. */
export async function resolveDigest(store: Store, client: RegistryClient, ref: ImageRef): Promise<string> {
  const m = await fetchManifestCached(store, client, ref.name, ref.tag ?? 'latest');
  return m.digest;
}

/** Pull an image into the store: verifies every blob's sha256 while streaming, then registers the manifest. */
export async function pullImage(store: Store, client: RegistryClient, ref: ImageRef, o: PullOptions = {}): Promise<PullResult> {
  if (ref.registry === undefined) throw new SitefileError('E_USAGE', 'internal: pullImage needs a registry reference');
  const platform = o.platform ?? 'linux/amd64';
  const say = o.onEvent ?? (() => undefined);
  const repo = ref.name;
  const reference = o.rootDigest ?? ref.digest ?? ref.tag ?? 'latest';
  const limits = client.limits;

  const root = await fetchManifestCached(store, client, repo, reference);
  let manifest = root;
  if (INDEX_TYPES.has(root.mediaType)) {
    const d = selectPlatform(root.bytes, platform);
    manifest = await fetchManifestCached(store, client, repo, d.digest);
  } else if (!MANIFEST_TYPES.has(root.mediaType)) {
    throw new SitefileError('E_UNSUPPORTED', `${client.registry}/${repo}: unsupported manifest type '${root.mediaType || 'unknown'}' (expected an OCI or Docker v2 image)`);
  }

  let parsed: ParsedManifest;
  try {
    parsed = parseManifest(manifest.bytes);
  } catch (e) {
    throw new SitefileError('E_REGISTRY', `${client.registry}/${repo}: ${(e as Error).message}`, undefined, { cause: e });
  }
  if (!CONFIG_TYPES.has(parsed.config.mediaType)) {
    throw new SitefileError('E_UNSUPPORTED', `${client.registry}/${repo}: the artifact is not a container image (config type '${parsed.config.mediaType}')`);
  }
  const blobs: Descriptor[] = [parsed.config, ...parsed.layers];
  let total = 0;
  for (const d of blobs) {
    if (d.size > limits.maxBlobBytes) throw new SitefileError('E_TOO_LARGE', `blob ${d.digest} is ${d.size} bytes, exceeding the limit of ${limits.maxBlobBytes} bytes (--max-download-bytes)`);
    total += d.size;
  }
  if (total > limits.maxTotalBytes) throw new SitefileError('E_TOO_LARGE', `${client.registry}/${repo} totals ${total} bytes, exceeding the limit of ${limits.maxTotalBytes} bytes`);

  const unique = [...new Map(blobs.map((d) => [d.digest, d])).values()];
  let next = 0;
  const worker = async (): Promise<void> => {
    for (;;) {
      const d = unique[next++];
      if (!d) return;
      if (await store.hasBlob(d.digest, d.size)) {
        say(`${shortId(d.digest)}: already exists`);
        continue;
      }
      await store.importBlob(d.digest, d.size, client.getBlob(repo, d), limits.maxBlobBytes);
      say(`${shortId(d.digest)}: pulled (${d.size} bytes)`);
    }
  };
  await Promise.all(Array.from({ length: Math.min(3, unique.length) }, worker));

  const configJson = JSON.parse(Buffer.from(await store.readBlob(parsed.config.digest, parsed.config.size)).toString('utf8')) as Record<string, unknown>;
  if (o.enforcePlatform) {
    const want = parsePlatform(platform);
    const cfg = parseConfig(Buffer.from(JSON.stringify(configJson)));
    if (cfg.os !== want.os || cfg.architecture !== want.arch) {
      throw new SitefileError('E_REGISTRY', `${client.registry}/${repo} is ${cfg.os}/${cfg.architecture}, not ${platform}; pass --platform`);
    }
  }
  const tagRef: ImageRef | undefined = o.tag ? { registry: ref.registry, name: ref.name, tag: ref.tag ?? 'latest', digest: undefined } : undefined;
  const id = await store.addManifest(manifest.bytes, tagRef);
  return { id, rootDigest: root.digest, manifest: parsed, config: configJson };
}

export interface PushOptions {
  readonly onEvent?: ((message: string) => void) | undefined;
}

export interface PushResult {
  readonly digest: string;
  readonly size: number;
  readonly uploaded: number;
  readonly mounted: number;
  readonly existing: number;
}

/** Push a stored image to `ref` (a registry reference): blobs that are missing, then the manifest. */
export async function pushImage(store: Store, client: RegistryClient, id: string, ref: ImageRef, o: PushOptions = {}): Promise<PushResult> {
  const say = o.onEvent ?? (() => undefined);
  const repo = ref.name;
  if (ref.digest !== undefined && ref.digest !== id) {
    throw new SitefileError('E_USAGE', `the reference pins ${ref.digest} but the local image is ${id}`);
  }
  const manifestBytes = await store.readBlob(id);
  const parsed = parseManifest(manifestBytes);
  const ann = parsed.annotations['org.opencontainers.image.base.name'];
  // Layers of the BASE image can be mounted from its repository when it lives on this registry.
  let mountFrom: string | undefined;
  let baseLayers = new Set<string>();
  if (ann) {
    try {
      const b = withDefaultRegistry(parseRef(ann));
      if (b.registry === client.registry && b.name !== repo) {
        mountFrom = b.name;
        baseLayers = new Set(parsed.layers.slice(0, -1).map((l) => l.digest));
      }
    } catch {
      /* not a reference we understand */
    }
  }
  await client.ping();
  const counts = { uploaded: 0, mounted: 0, existing: 0 };
  const seen = new Set<string>();
  for (const d of [parsed.config, ...parsed.layers]) {
    if (seen.has(d.digest)) continue;
    seen.add(d.digest);
    if (await client.hasBlob(repo, d.digest)) {
      counts.existing++;
      say(`${shortId(d.digest)}: layer already exists`);
      continue;
    }
    if (!(await store.hasBlob(d.digest, d.size))) throw new SitefileError('E_STORE', `blob ${d.digest} is missing from the local store; cannot push`);
    const r = await client.pushBlob(repo, d, { length: d.size, open: () => store.blobStream(d.digest) }, baseLayers.has(d.digest) ? mountFrom : undefined);
    counts[r]++;
    say(`${shortId(d.digest)}: ${r === 'mounted' ? 'mounted from ' + mountFrom : 'pushed'} (${d.size} bytes)`);
  }
  let mediaType: string = MEDIA.manifest;
  try {
    const j = JSON.parse(Buffer.from(manifestBytes).toString('utf8')) as { mediaType?: unknown };
    if (typeof j.mediaType === 'string') mediaType = j.mediaType;
  } catch {
    /* parseManifest already succeeded */
  }
  const digest = await client.putManifest(repo, ref.tag ?? ref.digest ?? 'latest', manifestBytes, mediaType);
  if (digest !== id) throw new SitefileError('E_INTEGRITY', `the registry reports digest ${digest}, expected ${id}`);
  return { digest, size: manifestBytes.byteLength, ...counts };
}
