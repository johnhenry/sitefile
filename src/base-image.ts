import { SitefileError, type SourceSpan } from './errors.js';
import type { BaseImage, Descriptor } from './oci.js';
import { parseRef, withDefaultRegistry, type ImageRef } from './ref.js';
import { resolveCredentials, type Credentials } from './registry-auth.js';
import { pullImage, resolveDigest } from './registry-ops.js';
import { RegistryClient, type RegistryOptions } from './registry.js';
import type { Lockfile } from './resources/lockfile.js';
import type { Vendor } from './resources/vendor.js';
import type { Store } from './store.js';

/** Registries reachable over plain http: `--insecure-registry` flags plus $SITEFILE_INSECURE_REGISTRIES (comma-separated). */
export function insecureRegistriesFromEnv(extra: readonly string[] = [], env: NodeJS.ProcessEnv = process.env): string[] {
  const fromEnv = (env.SITEFILE_INSECURE_REGISTRIES ?? '').split(',').map((s) => s.trim()).filter(Boolean);
  return [...new Set([...extra, ...fromEnv])];
}

/** Build a client for a (canonical) registry name, with credentials resolved lazily. */
export function makeClient(registry: string, options: RegistryOptions & { readonly explicitCredentials?: Credentials | undefined } = {}): RegistryClient {
  const { explicitCredentials, ...rest } = options;
  return new RegistryClient(registry, {
    ...rest,
    credentials: rest.credentials ?? (() => resolveCredentials(registry, { explicit: explicitCredentials })),
  });
}

export interface ResolveBaseOptions {
  readonly store: Store;
  /** `os/arch` of the base to use (default linux/amd64). */
  readonly platform?: string | undefined;
  readonly lock?: Lockfile | undefined;
  /** `--frozen-lockfile`: never resolve a tag; it must be in the lockfile (or pinned by digest). */
  readonly frozen?: boolean | undefined;
  /** `--update-lock`: re-resolve the tag and re-record it. */
  readonly update?: boolean | undefined;
  /** `sitefile vendor`'s directory: a tag it recorded is used when the lockfile has none, and its blobs seed the store. */
  readonly vendor?: Vendor | undefined;
  readonly registry?: RegistryOptions | undefined;
  readonly warn?: ((message: string) => void) | undefined;
  readonly span?: SourceSpan | undefined;
}

/** A BASE reference (a bare name means Docker Hub), how it is shown, and its `Sitefile.lock` `images` key. */
export function baseRef(input: string, span?: SourceSpan): { ref: ImageRef; registry: string; display: string; lockKey: string } {
  let ref: ImageRef;
  try {
    ref = withDefaultRegistry(parseRef(input));
  } catch (e) {
    if (e instanceof SitefileError) throw new SitefileError(e.code, `BASE: ${e.detail}`, span, { cause: e });
    throw e;
  }
  const registry = ref.registry as string;
  const display = `${registry}/${ref.name}${ref.tag !== undefined || ref.digest === undefined ? `:${ref.tag ?? 'latest'}` : ''}`;
  const lockKey = `${registry}/${ref.name}:${ref.tag ?? 'latest'}`;
  return { ref, registry, display, lockKey };
}

/**
 * Resolve `EXPORT ... AS oci BASE <image>` to a base image whose blobs are in the local store.
 *
 *   - `image@sha256:...` is pinned by the Sitefile itself and used as is;
 *   - `image:tag` is looked up in Sitefile.lock (`images`) and fetched BY DIGEST, so a tag that has
 *     moved on the registry does not change the build; without a lock entry the tag is resolved once and
 *     recorded (an error with --frozen-lockfile; re-resolved with --update-lock);
 *   - a base already in the store (matching manifest and blobs) is used without any network access.
 * A bare name (`nginx:alpine`) means Docker Hub (`docker.io/library/nginx`).
 */
export async function resolveBase(input: string, o: ResolveBaseOptions): Promise<BaseImage> {
  const { ref, registry, display, lockKey } = baseRef(input, o.span);
  const client = makeClient(registry, o.registry ?? {});

  let rootDigest: string | undefined = ref.digest;
  let fresh = false;
  if (rootDigest === undefined) {
    const locked = o.update ? undefined : o.lock?.getImage(lockKey);
    const vendored = locked || o.update || o.frozen ? undefined : o.vendor?.image(lockKey);
    if (locked) {
      rootDigest = locked.digest;
      o.lock?.markImageUsed(lockKey);
    } else if (vendored) {
      rootDigest = vendored.digest;
      o.lock?.recordImage(lockKey, { digest: rootDigest });
    } else if (o.frozen) {
      throw new SitefileError('E_LOCK_MISSING', `BASE ${input}: '${lockKey}' is not in the lockfile (--frozen-lockfile); pin it with @sha256:... or run a build without --frozen-lockfile to record it`, o.span);
    } else {
      rootDigest = await resolveDigest(o.store, client, ref);
      fresh = true;
      if (o.lock) o.lock.recordImage(lockKey, { digest: rootDigest });
      else o.warn?.(`BASE ${input} is not pinned and there is no lockfile: the build is not reproducible (use ${display}@sha256:... or a Sitefile.lock)`);
    }
  }
  // Vendored blobs go into the store first, so the pull below finds them and needs no network.
  if (o.vendor) await o.vendor.seedStore(o.store, rootDigest, o.platform ?? 'linux/amd64');
  const pulled = await pullImage(o.store, client, ref, {
    platform: o.platform,
    enforcePlatform: true,
    rootDigest,
    tag: fresh,
  });
  return { name: display, manifestDigest: pulled.id, layers: pulled.manifest.layers, config: pulled.config };
}

/** Read the blobs of `layers` from the store (verifying each digest), for writing a self-contained layout. */
export async function readLayerBlobs(store: Store, layers: readonly Descriptor[]): Promise<Map<string, Uint8Array>> {
  const out = new Map<string, Uint8Array>();
  for (const l of layers) if (!out.has(l.digest)) out.set(l.digest, await store.readBlob(l.digest, l.size));
  return out;
}
