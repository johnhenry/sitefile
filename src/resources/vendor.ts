import { randomBytes } from 'node:crypto';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { INTEGRITY_RE, sha256 } from '../digest.js';
import { SitefileError, isSitefileError, type SourceSpan } from '../errors.js';
import { MEDIA, parseManifest } from '../oci.js';
import { selectPlatform } from '../registry-ops.js';
import type { Store } from '../store.js';
import type { LocalContext } from './local.js';

/**
 * The vendor directory written by `sitefile vendor` (issue #96): every remote input's bytes under
 * content-addressed names, plus a manifest that maps URLs and image tags to them.
 *
 *   <dir>/manifest.json          {"version":1,"resources":{url:{integrity,size,resolved}},"images":{ref:{digest,platform,manifest}}}
 *   <dir>/blobs/sha256/<hex>     IMPORT bodies and BASE image blobs (index, manifest, config, layers)
 *
 * Builds prefer these copies over the network and verify each one against its digest (and, as for any
 * fetch, against INTEGRITY and Sitefile.lock). Nothing secret is written: URLs with credentials are
 * rejected before any fetch, and registry tokens or passwords never leave memory.
 */
export const DEFAULT_VENDOR_DIR = 'vendor/sitefile';
export const VENDOR_MANIFEST = 'manifest.json';

export interface VendoredResource {
  /** `sha256:<hex>` of the body: also its blob name. */
  readonly integrity: string;
  readonly size: number;
  /** Final URL after redirects, as the lockfile records it. */
  readonly resolved: string;
}

export interface VendoredImage {
  /** What the reference resolved to (an index for multi-platform images, else the manifest). */
  readonly digest: string;
  /** The platform that was vendored (`os/arch`). */
  readonly platform: string;
  /** That platform's manifest digest (the image ID). */
  readonly manifest: string;
}

export interface VendorManifest {
  readonly version: 1;
  readonly resources: Readonly<Record<string, VendoredResource>>;
  readonly images: Readonly<Record<string, VendoredImage>>;
}

/** Validate a vendor directory: a relative path inside the build context. Returns it normalized (POSIX, no trailing slash). */
export function vendorDirRel(input: string): string {
  const n = path.posix.normalize(input).replace(/\/+$/, '');
  if (input === '' || path.posix.isAbsolute(input) || input.includes('\\') || n === '.' || n === '..' || n.startsWith('../')) {
    throw new SitefileError('E_USAGE', `--vendor-dir expects a relative path inside the build context, got '${input}'`);
  }
  return n;
}

const blobRel = (dir: string, digest: string): string => `${dir}/blobs/sha256/${digest.slice('sha256:'.length)}`;

/**
 * The vendor directory a build uses: `explicit` (which must have a manifest), else the default one if
 * its manifest exists, else none.
 */
export async function findVendorDir(contextReal: string, explicit: string | undefined): Promise<string | undefined> {
  const dir = vendorDirRel(explicit ?? DEFAULT_VENDOR_DIR);
  const exists = await fs.stat(path.join(contextReal, ...dir.split('/'), VENDOR_MANIFEST)).then(
    (st) => st.isFile(),
    () => false,
  );
  if (exists) return dir;
  if (explicit !== undefined) {
    throw new SitefileError('E_NOT_FOUND', `vendor directory '${dir}' has no ${VENDOR_MANIFEST}; run 'sitefile vendor --vendor-dir ${dir}' first`);
  }
  return undefined;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Parse and check a vendor manifest. Errors are E_VALIDATION naming the file. */
export function parseVendorManifest(bytes: Uint8Array, shown: string): VendorManifest {
  const bad = (why: string): SitefileError => new SitefileError('E_VALIDATION', `malformed vendor manifest ${shown}: ${why}`);
  let data: unknown;
  try {
    data = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    throw bad('invalid JSON');
  }
  if (!isObject(data)) throw bad('expected a JSON object');
  if (data.version !== 1) throw bad(`unsupported version ${JSON.stringify(data.version) ?? 'undefined'} (expected 1)`);
  const resources: Record<string, VendoredResource> = {};
  const images: Record<string, VendoredImage> = {};
  const rs = data.resources ?? {};
  if (!isObject(rs)) throw bad('"resources" must be an object');
  for (const [url, e] of Object.entries(rs)) {
    if (!isObject(e) || typeof e.integrity !== 'string' || !INTEGRITY_RE.test(e.integrity) || typeof e.size !== 'number' || !Number.isSafeInteger(e.size) || e.size < 0 || typeof e.resolved !== 'string') {
      throw bad(`entry for ${url} needs "integrity" (sha256:<64 hex>), "size" and "resolved"`);
    }
    resources[url] = { integrity: e.integrity, size: e.size, resolved: e.resolved };
  }
  const is = data.images ?? {};
  if (!isObject(is)) throw bad('"images" must be an object');
  for (const [ref, e] of Object.entries(is)) {
    if (!isObject(e) || typeof e.digest !== 'string' || !INTEGRITY_RE.test(e.digest) || typeof e.platform !== 'string' || typeof e.manifest !== 'string' || !INTEGRITY_RE.test(e.manifest)) {
      throw bad(`image entry for ${ref} needs "digest", "platform" and "manifest"`);
    }
    images[ref] = { digest: e.digest, platform: e.platform, manifest: e.manifest };
  }
  return { version: 1, resources, images };
}

/** The manifest as `sitefile vendor` writes it: sorted keys, two-space indent, trailing newline. */
export function serializeVendorManifest(m: VendorManifest): string {
  const resources: Record<string, VendoredResource> = {};
  for (const url of Object.keys(m.resources).sort()) {
    const e = m.resources[url]!;
    resources[url] = { integrity: e.integrity, size: e.size, resolved: e.resolved };
  }
  const images: Record<string, VendoredImage> = {};
  for (const ref of Object.keys(m.images).sort()) {
    const e = m.images[ref]!;
    images[ref] = { digest: e.digest, platform: e.platform, manifest: e.manifest };
  }
  return JSON.stringify({ version: 1, resources, images }, null, 2) + '\n';
}

function isIndex(bytes: Uint8Array): boolean {
  try {
    const j = JSON.parse(new TextDecoder().decode(bytes)) as { mediaType?: unknown; manifests?: unknown };
    if (typeof j.mediaType === 'string') return j.mediaType === MEDIA.index || j.mediaType === MEDIA.dockerList;
    return Array.isArray(j.manifests);
  } catch {
    return false;
  }
}

/** A vendor directory opened through the build's sandboxed context reader. */
export class Vendor {
  private constructor(
    /** Context-relative directory. */
    readonly dir: string,
    private readonly local: LocalContext,
    readonly manifest: VendorManifest,
  ) {}

  static async open(local: LocalContext, dir: string): Promise<Vendor> {
    const rel = `${dir}/${VENDOR_MANIFEST}`;
    const f = await local.readFile(rel);
    return new Vendor(dir, local, parseVendorManifest(f.bytes, rel));
  }

  resource(url: string): VendoredResource | undefined {
    return Object.hasOwn(this.manifest.resources, url) ? this.manifest.resources[url] : undefined;
  }

  image(ref: string): VendoredImage | undefined {
    return Object.hasOwn(this.manifest.images, ref) ? this.manifest.images[ref] : undefined;
  }

  /** The vendored body of `url`, verified against its digest. */
  async readResource(url: string, e: VendoredResource, span?: SourceSpan): Promise<{ bytes: Uint8Array; finalUrl: string }> {
    return { bytes: await this.readBlob(e.integrity, `vendored copy of '${url}'`, span), finalUrl: e.resolved };
  }

  private async readBlob(digest: string, what: string, span?: SourceSpan): Promise<Uint8Array> {
    const rel = blobRel(this.dir, digest);
    const { bytes } = await this.local.readFile(rel, span);
    const got = sha256(bytes);
    if (got !== digest) {
      throw new SitefileError('E_INTEGRITY', `${what} (${rel}) is corrupt: expected ${digest}, got ${got}; run 'sitefile vendor' again`, span);
    }
    return bytes;
  }

  private async hasBlob(digest: string): Promise<boolean> {
    try {
      return (await this.local.stat(blobRel(this.dir, digest))) === 'file';
    } catch (e) {
      if (isSitefileError(e, 'E_NOT_FOUND')) return false;
      throw e;
    }
  }

  /**
   * Copy the vendored blobs of the image `rootDigest` (for `platform`) into the store, each verified,
   * so pulling it finds everything locally. Stops quietly at the first blob that is not vendored: the
   * pull then fetches the rest from the registry.
   */
  async seedStore(store: Store, rootDigest: string, platform: string): Promise<void> {
    if (!(await this.hasBlob(rootDigest))) return;
    const root = await this.readBlob(rootDigest, 'vendored image blob');
    await store.putSmallBlob(root);
    let manifest = root;
    if (isIndex(root)) {
      let d;
      try {
        d = selectPlatform(root, platform);
      } catch {
        return;
      }
      if (!(await this.hasBlob(d.digest))) return;
      manifest = await this.readBlob(d.digest, 'vendored image blob');
      await store.putSmallBlob(manifest);
    }
    let parsed;
    try {
      parsed = parseManifest(manifest);
    } catch {
      return;
    }
    for (const d of [parsed.config, ...parsed.layers]) {
      if (await store.hasBlob(d.digest, d.size)) continue;
      if (!(await this.hasBlob(d.digest))) return;
      const bytes = await this.readBlob(d.digest, 'vendored image blob');
      await store.importBlob(d.digest, d.size, (async function* () {
        yield bytes;
      })());
    }
  }
}

/** The nearest existing ancestor's real path, with the missing tail appended. */
async function realpathLoose(p: string): Promise<string> {
  const tail: string[] = [];
  let cur = p;
  for (;;) {
    try {
      return path.join(await fs.realpath(cur), ...tail.reverse());
    } catch {
      const parent = path.dirname(cur);
      if (parent === cur) return p;
      tail.push(path.basename(cur));
      cur = parent;
    }
  }
}

const inside = (root: string, real: string): boolean => real.startsWith(root.endsWith(path.sep) ? root : root + path.sep);

/**
 * Replace `<contextReal>/<dir>` with a vendor directory holding `manifestText` and `blobs`: written to a
 * temporary sibling, then swapped in. Refuses a directory that resolves outside the context, a symlink,
 * and an existing directory that is not a vendor directory (no manifest.json).
 */
export async function writeVendorDir(contextReal: string, dir: string, manifestText: string, blobs: ReadonlyMap<string, Uint8Array>): Promise<string> {
  const target = path.join(contextReal, ...dir.split('/'));
  const parent = path.dirname(target);
  if (!inside(contextReal, await realpathLoose(target))) {
    throw new SitefileError('E_PATH_ESCAPE', `vendor directory '${dir}' resolves outside the build context`);
  }
  const st = await fs.lstat(target).catch(() => undefined);
  if (st?.isSymbolicLink()) throw new SitefileError('E_EXPORT_UNSAFE', `vendor directory '${dir}' is a symlink; refusing to replace it`);
  if (st && !st.isDirectory()) throw new SitefileError('E_EXPORT_UNSAFE', `vendor directory '${dir}' exists and is not a directory`);
  if (st) {
    const names = await fs.readdir(target);
    if (names.length > 0 && !names.includes(VENDOR_MANIFEST)) {
      throw new SitefileError('E_EXPORT_UNSAFE', `'${dir}' exists and is not a sitefile vendor directory (no ${VENDOR_MANIFEST}); remove it or choose another --vendor-dir`);
    }
  }
  await fs.mkdir(parent, { recursive: true }).catch((e: unknown) => {
    throw new SitefileError('E_EXPORT', `cannot write vendor directory '${dir}': ${(e as Error).message}`, undefined, { cause: e });
  });
  const parentReal = await fs.realpath(parent);
  if (!inside(contextReal, parentReal) && parentReal !== contextReal) {
    throw new SitefileError('E_PATH_ESCAPE', `vendor directory '${dir}' resolves outside the build context`);
  }
  const tag = `${process.pid}-${randomBytes(4).toString('hex')}`;
  const tmp = path.join(parent, `.${path.basename(target)}.sitefile-vendor-tmp-${tag}`);
  const old = path.join(parent, `.${path.basename(target)}.sitefile-vendor-old-${tag}`);
  try {
    await fs.mkdir(path.join(tmp, 'blobs', 'sha256'), { recursive: true });
    for (const [digest, bytes] of [...blobs].sort((a, b) => (a[0] < b[0] ? -1 : 1))) {
      await fs.writeFile(path.join(tmp, 'blobs', 'sha256', digest.slice('sha256:'.length)), bytes);
    }
    await fs.writeFile(path.join(tmp, VENDOR_MANIFEST), manifestText);
    if (st) await fs.rename(target, old);
    await fs.rename(tmp, target);
  } catch (e) {
    await fs.rm(tmp, { recursive: true, force: true });
    if (st && !(await fs.lstat(target).catch(() => undefined))) await fs.rename(old, target).catch(() => undefined);
    throw new SitefileError('E_EXPORT', `cannot write vendor directory '${dir}': ${(e as Error).message}`, undefined, { cause: e });
  }
  await fs.rm(old, { recursive: true, force: true });
  return target;
}
