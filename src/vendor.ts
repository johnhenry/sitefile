import { realpath } from 'node:fs/promises';
import path from 'node:path';
import { resolveBase } from './base-image.js';
import { sha256 } from './digest.js';
import { SitefileError, type SourceSpan } from './errors.js';
import { fetchGitImport, fetchRemoteImport, gitSelectionBytes, loadProgram, readImportedSitefile, rootSitefileSource, warnUnusedSecrets, type BuildOptions, type RemoteEnv, type SitefileSource } from './executor.js';
import type { GitLocation, GitSelection } from './resources/git.js';
import { redactError, redactSecrets } from './secrets.js';
import { parseManifest } from './oci.js';
import { parseRef, withDefaultRegistry } from './ref.js';
import { importedStageRefs, plan, type ImportSpec, type Plan, type Stage } from './planner.js';
import { Lockfile } from './resources/lockfile.js';
import { DEFAULT_FETCH_POLICY, type FetchPolicy } from './resources/types.js';
import { DEFAULT_VENDOR_DIR, serializeVendorManifest, vendorDirRel, writeVendorDir, type VendoredGit, type VendoredImage, type VendoredResource } from './resources/vendor.js';
import { DEFAULT_ARCHIVE_LIMITS } from './resources/types.js';
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
  const secrets = options.secrets;
  if (!secrets?.size) return vendorUnredacted(options);
  const onWarning = options.onWarning;
  try {
    const r = await vendorUnredacted({ ...options, ...(onWarning ? { onWarning: (m: string) => onWarning(redactSecrets(m, secrets)) } : {}) });
    return { ...r, warnings: r.warnings.map((w) => redactSecrets(w, secrets)) };
  } catch (e) {
    throw redactError(e, secrets);
  }
}

async function vendorUnredacted(options: BuildOptions): Promise<VendorResult> {
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
  warnUnusedSecrets(p, options.secrets, warn);
  const env: RemoteEnv = { policy, lock, label, warn, frozen, update, archiveLimits: { ...DEFAULT_ARCHIVE_LIMITS, ...options.archiveLimits }, ...(options.secrets ? { secrets: options.secrets } : {}) };

  const blobs = new Map<string, Uint8Array>();
  const resources: Record<string, VendoredResource> = {};
  const git: Record<string, VendoredGit> = {};
  /** A Git import's content, vendored (a Sitefile imported from Git is read from it too). */
  const vendorGit = async (spec: ImportSpec, loc: GitLocation, e: RemoteEnv): Promise<{ commit: string; selection: GitSelection }> => {
    const got = await fetchGitImport(spec, loc, e);
    const bytes = gitSelectionBytes(got.selection);
    const digest = sha256(bytes);
    blobs.set(digest, bytes);
    git[loc.url] = { commit: got.commit, kind: got.selection.kind, integrity: digest, size: bytes.byteLength, ...(got.selection.kind === 'file' ? { mode: got.selection.mode === 0o755 ? '755' : '644' } : {}) };
    return got;
  };
  /** Every remote input of `sp` (a Sitefile read from `source`), then those of the Sitefiles it imports AS sitefile. */
  const vendorImports = async (sp: Plan, source: SitefileSource): Promise<void> => {
    const e: RemoteEnv = { ...env, label: source.label };
    for (const spec of sp.imports) {
      if (spec.as === 'sitefile') {
        const sub = await readImportedSitefile(spec, source, importedStageRefs(sp.stages), (s, loc) => vendorGit(s, loc, e));
        sub.plan.warnings.forEach(warn);
        await vendorImports(sub.plan, sub.source);
        nestedImages.push(...sub.plan.stages);
        continue;
      }
      if (spec.location.kind === 'git') {
        await vendorGit(spec, spec.location, e);
        continue;
      }
      if (spec.location.kind !== 'remote') continue;
      const url = spec.location.url;
      const { bytes, finalUrl } = await fetchRemoteImport(spec, url, e);
      const digest = sha256(bytes);
      blobs.set(digest, bytes);
      resources[url] = { integrity: digest, size: bytes.byteLength, resolved: finalUrl };
    }
  };
  /** The stages of imported Sitefiles: their registry FROM images are vendored after this Sitefile's. */
  const nestedImages: Stage[] = [];
  await vendorImports(p, await rootSitefileSource(options, label, sitefilePath));

  const images: Record<string, VendoredImage> = {};
  const platform = options.platform ?? 'linux/amd64';
  let store: Store | undefined;
  /** Resolve an image as a build would and vendor what an offline pull of it needs. */
  const vendorImage = async (input: string, lead: string, enforcePlatform: boolean, span?: SourceSpan): Promise<void> => {
    store ??= options.store ?? (await Store.open());
    const s = store;
    const base = await resolveBase(input, {
      store: s,
      platform,
      lock,
      frozen,
      update,
      registry: { ...options.registry, allowPrivateNetwork: options.registry?.allowPrivateNetwork ?? policy.allowPrivateNetwork, network: options.registry?.network ?? policy.network },
      warn,
      lead,
      enforcePlatform,
      ...(span ? { span } : {}),
    });
    // resolveBase has validated the reference; this is its lockfile key.
    const ref = withDefaultRegistry(parseRef(input));
    const registry = ref.registry as string;
    const lockKey = `${registry}/${ref.name}:${ref.tag ?? 'latest'}`;
    const root = ref.digest ?? lock?.getImage(lockKey)?.digest ?? base.manifestDigest;
    const add = async (digest: string, size?: number): Promise<Uint8Array> => {
      const bytes = await s.readBlob(digest, size);
      blobs.set(digest, bytes);
      return bytes;
    };
    await add(root);
    const m = parseManifest(await add(base.manifestDigest));
    for (const d of [m.config, ...m.layers]) await add(d.digest, d.size);
    const key = ref.digest !== undefined ? `${registry}/${ref.name}${ref.tag !== undefined ? `:${ref.tag}` : ''}@${ref.digest}` : lockKey;
    images[key] = { digest: root, platform, manifest: base.manifestDigest };
  };
  // FROM image stage bases with a registry (local-store images are not vendored), then the BASE.
  for (const st of [...p.stages, ...nestedImages]) if (st.base.kind === 'image' && !st.base.local) await vendorImage(st.base.ref, `${st.base.lead} image`, false, st.base.span);
  const baseInput = options.baseImage ?? (p.exportFormat === 'oci' ? p.exportBase : undefined);
  if (baseInput !== undefined) await vendorImage(baseInput, 'BASE', true);

  const text = serializeVendorManifest({ version: 1, resources, images, git });
  const written = await writeVendorDir(contextReal, dir, text, blobs);
  const lockfileWritten = lock && !frozen ? await lock.save() : false;
  let bytes = 0;
  for (const b of blobs.values()) bytes += b.byteLength;
  return { dir: written, resources: Object.keys(resources).length + Object.keys(git).length, images: Object.keys(images).length, blobs: blobs.size, bytes, lockfileWritten, warnings };
}
