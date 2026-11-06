import { realpath } from 'node:fs/promises';
import path from 'node:path';
import { resolveBase } from './base-image.js';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import { fetchRemoteImport, loadProgram, type BuildOptions, type RemoteEnv } from './executor.js';
import { parseManifest } from './oci.js';
import { parseRef, withDefaultRegistry } from './ref.js';
import { plan } from './planner.js';
import { Lockfile } from './resources/lockfile.js';
import { DEFAULT_FETCH_POLICY, type FetchPolicy } from './resources/types.js';
import { DEFAULT_VENDOR_DIR, serializeVendorManifest, vendorDirRel, writeVendorDir, type VendoredImage, type VendoredResource } from './resources/vendor.js';
import { Store } from './store.js';

export interface VendorResult {
  /** Real path of the vendor directory written. */
  readonly dir: string;
  readonly resources: number;
  readonly images: number;
  readonly blobs: number;
  /** Total size of the blobs. */
  readonly bytes: number;
  readonly lockfileWritten: boolean;
  readonly warnings: readonly string[];
}

/**
 * `sitefile vendor` (issue #96): fetch every remote IMPORT and the BASE image exactly as a build would
 * (same network policy, INTEGRITY and lockfile checks, and Sitefile.lock is updated the same way), then
 * replace the vendor directory (`options.vendorDir`, default `vendor/sitefile`) with their bytes under
 * content-addressed names and a manifest. Always fetches: an existing vendor directory is not read.
 */
export async function vendor(options: BuildOptions): Promise<VendorResult> {
  const { program, sitefilePath, label } = await loadProgram(options);
  const p = plan(program, options.buildArgs ?? {});
  const warnings: string[] = [];
  const warn = (m: string): void => {
    warnings.push(m);
    options.onWarning?.(m);
  };
  p.warnings.forEach(warn);
  const dir = vendorDirRel(options.vendorDir ?? DEFAULT_VENDOR_DIR);
  const contextReal = await realpath(options.contextDir).catch((e: unknown) => {
    throw new SitefileError('E_NOT_FOUND', `build context '${options.contextDir}' does not exist`, undefined, { cause: e });
  });
  const policy: FetchPolicy = { ...DEFAULT_FETCH_POLICY, ...options.fetchPolicy };
  const lockPath = options.lockfile === false ? undefined : (options.lockfile ?? path.join(path.dirname(sitefilePath), 'Sitefile.lock'));
  const lock = lockPath ? await Lockfile.load(lockPath) : undefined;
  const frozen = !!options.frozenLockfile;
  const update = !!options.updateLock;
  const env: RemoteEnv = { policy, lock, label, warn, frozen, update };

  const blobs = new Map<string, Uint8Array>();
  const resources: Record<string, VendoredResource> = {};
  for (const spec of p.imports) {
    if (spec.location.kind !== 'remote') continue;
    const url = spec.location.url;
    const { bytes, finalUrl } = await fetchRemoteImport(spec, url, env);
    const digest = sha256(bytes);
    blobs.set(digest, bytes);
    resources[url] = { integrity: digest, size: bytes.byteLength, resolved: finalUrl };
  }

  const images: Record<string, VendoredImage> = {};
  const baseInput = options.baseImage ?? (p.exportFormat === 'oci' ? p.exportBase : undefined);
  if (baseInput !== undefined) {
    const store = options.store ?? (await Store.open());
    const platform = options.platform ?? 'linux/amd64';
    const base = await resolveBase(baseInput, {
      store,
      platform,
      lock,
      frozen,
      update,
      registry: { ...options.registry, allowPrivateNetwork: options.registry?.allowPrivateNetwork ?? policy.allowPrivateNetwork },
      warn,
    });
    // resolveBase has validated the reference; this is its lockfile key.
    const ref = withDefaultRegistry(parseRef(baseInput));
    const registry = ref.registry as string;
    const lockKey = `${registry}/${ref.name}:${ref.tag ?? 'latest'}`;
    const root = ref.digest ?? lock?.getImage(lockKey)?.digest ?? base.manifestDigest;
    const add = async (digest: string, size?: number): Promise<Uint8Array> => {
      const bytes = await store.readBlob(digest, size);
      blobs.set(digest, bytes);
      return bytes;
    };
    await add(root);
    const m = parseManifest(await add(base.manifestDigest));
    for (const d of [m.config, ...m.layers]) await add(d.digest, d.size);
    const key = ref.digest !== undefined ? `${registry}/${ref.name}${ref.tag !== undefined ? `:${ref.tag}` : ''}@${ref.digest}` : lockKey;
    images[key] = { digest: root, platform, manifest: base.manifestDigest };
  }

  const text = serializeVendorManifest({ version: 1, resources, images });
  const written = await writeVendorDir(contextReal, dir, text, blobs);
  const lockfileWritten = lock && !frozen ? await lock.save() : false;
  let bytes = 0;
  for (const b of blobs.values()) bytes += b.byteLength;
  return { dir: written, resources: Object.keys(resources).length, images: Object.keys(images).length, blobs: blobs.size, bytes, lockfileWritten, warnings };
}
