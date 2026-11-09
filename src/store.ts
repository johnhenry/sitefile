import { createHash, randomBytes } from 'node:crypto';
import { createReadStream, promises as fs } from 'node:fs';
import type { Readable } from 'node:stream';
import os from 'node:os';
import path from 'node:path';
import { decodeReferrersIndex, encodeReferrersIndex, type Referrer, type ReferrerDescriptor } from './attest.js';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import {
  ANN,
  blobPathOf,
  buildOciImage,
  decodeIndex,
  encodeIndex,
  extractImageFiles,
  layoutTree,
  OCI_LAYOUT_BYTES,
  parseConfig,
  parseManifest,
  siteLayerIndex,
  siteLayerIndices,
  type IndexEntry,
  type OciOptions,
  type ParsedConfig,
  type ParsedManifest,
} from './oci.js';
import { looksLikeImageId, parseLocalRef, refKey, type ImageRef } from './ref.js';
import type { Tree } from './tree.js';

/**
 * The local image store IS an OCI image layout (image-spec v1.1) at $SITEFILE_HOME (default ~/.sitefile):
 *
 *   oci-layout           {"imageLayoutVersion":"1.0.0"}
 *   index.json           image index. Each entry is a manifest descriptor; tags are entries annotated
 *                        org.opencontainers.image.ref.name = "name:tag". An image with no tag is a
 *                        "dangling" entry without that annotation. dev.sitefile.created records when
 *                        the image was first stored (bookkeeping only; no digest depends on it).
 *   blobs/sha256/<hex>   manifest, config and layer blobs (identical layers share a blob)
 *   referrers/sha256/<hex>  attestations of image <hex> (`build -t --provenance/--sbom`): an OCI image
 *                        index of artifact manifests whose subject is that image (their blobs are in blobs/)
 *   servers/             site server state and logs (see servers.ts)
 *   lock                 advisory lock for mutations
 *
 * IMAGE ID = the manifest digest. All writes are temp file + rename, mutations hold the lock, and every
 * blob is digest-verified when read (corruption is E_STORE).
 */

export interface ImageSummary {
  /** Full `sha256:<hex>` image ID (the manifest digest). */
  readonly id: string;
  readonly tags: readonly string[];
  /** When the image was first stored. */
  readonly created: string;
  readonly files: number;
  /** Total bytes of the site's files (uncompressed). */
  readonly size: number;
}

export interface ImageDetails extends ImageSummary {
  readonly manifest: ParsedManifest;
  readonly config: ParsedConfig;
  readonly root: string;
  /** Compressed layer size in bytes. */
  readonly layerSize: number;
}

const HEX64 = /^[0-9a-f]{64}$/;

export function resolveStoreHome(env: NodeJS.ProcessEnv = process.env): string {
  const h = env.SITEFILE_HOME;
  return path.resolve(h !== undefined && h !== '' ? h : path.join(os.homedir(), '.sitefile'));
}

function storeError(msg: string, cause?: unknown): SitefileError {
  return new SitefileError('E_STORE', msg, undefined, cause === undefined ? undefined : { cause });
}

function errCode(e: unknown): string | undefined {
  return (e as NodeJS.ErrnoException | null)?.code;
}

export function shortId(id: string): string {
  return id.replace(/^sha256:/, '').slice(0, 12);
}

export class Store {
  private constructor(readonly home: string) {}

  /** The store at `home` if one exists there (nothing is created). */
  static async openExisting(home: string = resolveStoreHome()): Promise<Store | undefined> {
    const st = await fs.stat(path.join(home, 'blobs', 'sha256')).catch(() => undefined);
    return st?.isDirectory() ? new Store(path.resolve(home)) : undefined;
  }

  static async open(home: string = resolveStoreHome()): Promise<Store> {
    const s = new Store(path.resolve(home));
    await fs.mkdir(path.join(s.home, 'blobs', 'sha256'), { recursive: true, mode: 0o700 });
    await fs.writeFile(path.join(s.home, 'oci-layout'), OCI_LAYOUT_BYTES, { flag: 'wx', mode: 0o600 }).catch((e: unknown) => {
      if (errCode(e) !== 'EEXIST') throw e;
    });
    return s;
  }

  private blobFile(digest: string): string {
    return path.join(this.home, blobPathOf(digest));
  }

  // ------------------------------------------------------------ locking

  /** Run `fn` holding the store lock (an exclusive lock file; stale locks are broken). */
  async withLock<T>(fn: () => Promise<T>): Promise<T> {
    const lock = path.join(this.home, 'lock');
    const deadline = Date.now() + 20_000;
    for (;;) {
      try {
        const fh = await fs.open(lock, 'wx', 0o600);
        await fh.writeFile(String(process.pid));
        await fh.close();
        break;
      } catch (e) {
        if (errCode(e) !== 'EEXIST') throw storeError(`cannot lock the store at '${this.home}': ${(e as Error).message}`, e);
        if (await this.lockIsStale(lock)) {
          await fs.rm(lock, { force: true });
          continue;
        }
        if (Date.now() > deadline) throw storeError(`timed out waiting for the store lock '${lock}'`);
        await new Promise((r) => setTimeout(r, 20 + Math.random() * 30));
      }
    }
    try {
      return await fn();
    } finally {
      await fs.rm(lock, { force: true });
    }
  }

  private async lockIsStale(lock: string): Promise<boolean> {
    try {
      const [text, st] = await Promise.all([fs.readFile(lock, 'utf8'), fs.stat(lock)]);
      const pid = Number(text);
      if (Date.now() - st.mtimeMs > 120_000) return true;
      if (!Number.isInteger(pid) || pid <= 0) return Date.now() - st.mtimeMs > 2_000; // still being written
      try {
        process.kill(pid, 0);
        return false;
      } catch (e) {
        return errCode(e) === 'ESRCH';
      }
    } catch {
      return false; // vanished or unreadable: retry the open
    }
  }

  private async writeAtomic(dest: string, data: Uint8Array | string): Promise<void> {
    const tmp = path.join(path.dirname(dest), `.tmp-${randomBytes(6).toString('hex')}`);
    try {
      await fs.writeFile(tmp, data, { mode: 0o600 });
      await fs.rename(tmp, dest);
    } catch (e) {
      await fs.rm(tmp, { force: true });
      throw storeError(`cannot write '${dest}': ${(e as Error).message}`, e);
    }
  }

  // ------------------------------------------------------------ index

  private async readIndex(): Promise<IndexEntry[]> {
    let bytes: Buffer;
    try {
      bytes = await fs.readFile(path.join(this.home, 'index.json'));
    } catch (e) {
      if (errCode(e) === 'ENOENT') return [];
      throw storeError(`cannot read index.json: ${(e as Error).message}`, e);
    }
    try {
      return decodeIndex(bytes);
    } catch (e) {
      throw storeError(`index.json is corrupt: ${(e as Error).message}`, e);
    }
  }

  /** Normalize (one dangling entry per untagged image, none for tagged ones), sort, write. */
  private async writeIndex(entries: readonly IndexEntry[]): Promise<void> {
    const tagged = new Set(entries.filter((e) => e.refName).map((e) => e.digest));
    const seenDangling = new Set<string>();
    const out = entries.filter((e) => {
      if (e.refName) return true;
      if (tagged.has(e.digest) || seenDangling.has(e.digest)) return false;
      seenDangling.add(e.digest);
      return true;
    });
    out.sort((a, b) => (a.digest + (a.refName ?? '') < b.digest + (b.refName ?? '') ? -1 : 1));
    await this.writeAtomic(path.join(this.home, 'index.json'), encodeIndex(out));
  }

  /** `name:tag` → image ID. */
  async refs(): Promise<Record<string, string>> {
    const out: Record<string, string> = {};
    for (const e of await this.readIndex()) if (e.refName) out[e.refName] = e.digest;
    return out;
  }

  // ------------------------------------------------------------ blobs

  /** Read a blob and verify its digest (and size when given). */
  async readBlob(digest: string, size?: number): Promise<Uint8Array> {
    let bytes: Uint8Array;
    try {
      bytes = new Uint8Array(await fs.readFile(this.blobFile(digest)));
    } catch (e) {
      if (errCode(e) === 'ENOENT') throw storeError(`blob ${digest} is missing`, e);
      throw storeError(`cannot read blob ${digest}: ${(e as Error).message}`, e);
    }
    if ((size !== undefined && bytes.byteLength !== size) || sha256(bytes) !== digest) {
      throw storeError(`blob ${digest} is corrupt: its content does not match its digest`);
    }
    return bytes;
  }

  private async putBlob(digest: string, bytes: Uint8Array): Promise<void> {
    const bp = this.blobFile(digest);
    const st = await fs.stat(bp).catch(() => undefined);
    if (st?.isFile() && st.size === bytes.byteLength) return this.touchBlob(digest);
    await this.writeAtomic(bp, bytes);
  }

  /**
   * Mark a blob as just used (its mtime is the recency the size limit goes by, see cache-limit.ts;
   * atime is often disabled). Never fails.
   */
  async touchBlob(digest: string): Promise<void> {
    const now = new Date();
    await fs.utimes(this.blobFile(digest), now, now).catch(() => undefined);
  }

  /** True when the blob exists with the given size (content is verified when it was written). */
  async hasBlob(digest: string, size?: number): Promise<boolean> {
    const st = await fs.stat(this.blobFile(digest)).catch(() => undefined);
    return !!st?.isFile() && (size === undefined || st.size === size);
  }

  /** Stream a stored blob (for upload); write-time verification means no re-hash here. */
  blobStream(digest: string): Readable {
    return createReadStream(this.blobFile(digest));
  }

  /**
   * Store a blob from a stream, verifying its size and sha256 while it is written (to a temp file
   * that is renamed into place only when both match). `maxBytes` bounds what is accepted.
   */
  async importBlob(digest: string, size: number, source: AsyncIterable<Uint8Array>, maxBytes: number = size): Promise<void> {
    const dest = this.blobFile(digest);
    const tmp = path.join(path.dirname(dest), `.tmp-${randomBytes(6).toString('hex')}`);
    const hash = createHash('sha256');
    let total = 0;
    const fh = await fs.open(tmp, 'wx', 0o600);
    try {
      for await (const chunk of source) {
        total += chunk.byteLength;
        if (total > size || total > maxBytes) throw new SitefileError('E_TOO_LARGE', `blob ${digest} is larger than its declared size (${size} bytes)`);
        hash.update(chunk);
        await fh.write(chunk);
      }
      await fh.close();
      if (total !== size) throw new SitefileError('E_INTEGRITY', `blob ${digest} is truncated: got ${total} of ${size} bytes`);
      if (`sha256:${hash.digest('hex')}` !== digest) throw new SitefileError('E_INTEGRITY', `blob ${digest}: content does not match its digest`);
      await fs.rename(tmp, dest);
    } catch (e) {
      await fh.close().catch(() => undefined);
      await fs.rm(tmp, { force: true });
      throw e;
    }
  }

  /** Store a small blob (manifest, config, index) given its bytes; the digest is computed. */
  async putSmallBlob(bytes: Uint8Array): Promise<string> {
    const digest = sha256(bytes);
    await this.putBlob(digest, bytes);
    return digest;
  }

  /**
   * Register a pulled image manifest (all its blobs must already be stored): it becomes an index
   * entry, optionally tagged with `key` (replacing that tag's previous target).
   */
  async addManifest(manifestBytes: Uint8Array, tagRef?: ImageRef, now: Date = new Date()): Promise<string> {
    const id = await this.putSmallBlob(manifestBytes);
    let parsed: ParsedManifest;
    try {
      parsed = parseManifest(manifestBytes);
    } catch (e) {
      throw storeError(`manifest ${id} is not a usable image manifest: ${(e as Error).message}`, e);
    }
    for (const d of [parsed.config, ...parsed.layers]) {
      if (!(await this.hasBlob(d.digest, d.size))) throw storeError(`image ${shortId(id)} is incomplete: blob ${d.digest} is missing`);
    }
    await this.withLock(async () => {
      const entries = await this.readIndex();
      if (!entries.some((e) => e.digest === id)) {
        await this.writeIndex([...entries, { digest: id, size: manifestBytes.byteLength, stored: now.toISOString() }]);
      }
    });
    if (tagRef) await this.tag(id, tagRef);
    return id;
  }

  /** Whether an image with this manifest digest is in the index. */
  async hasImage(id: string): Promise<boolean> {
    return (await this.readIndex()).some((e) => e.digest === id);
  }

  // ------------------------------------------------------------ images

  /** Store a tree as an OCI image (idempotent: identical content and options give the same ID). Untagged. */
  async addImage(tree: Tree, options: OciOptions = {}, now: Date = new Date()): Promise<{ id: string; created: boolean }> {
    const img = buildOciImage(tree, options);
    return this.withLock(async () => {
      await this.putBlob(img.layerDigest, img.layerBytes);
      for (const [d, b] of img.extraLayers ?? []) await this.putBlob(d, b);
      await this.putBlob(img.configDigest, img.configBytes);
      await this.putBlob(img.manifestDigest, img.manifestBytes);
      const entries = await this.readIndex();
      if (entries.some((e) => e.digest === img.manifestDigest)) return { id: img.manifestDigest, created: false };
      await fs.rm(this.referrersFile(img.manifestDigest), { force: true }); // left over from a removed image
      await this.writeIndex([...entries, { digest: img.manifestDigest, size: img.manifestBytes.byteLength, stored: now.toISOString() }]);
      return { id: img.manifestDigest, created: true };
    });
  }

  /** Manifest and config of an image, digest-verified (not its layer). */
  async details(id: string): Promise<ImageDetails> {
    const entries = (await this.readIndex()).filter((e) => e.digest === id);
    const first = entries[0];
    if (!first) throw new SitefileError('E_NOT_FOUND', `no such image: ${shortId(id)}`);
    const manifestBytes = await this.readBlob(id, first.size);
    let manifest: ParsedManifest;
    let config: ParsedConfig;
    try {
      manifest = parseManifest(manifestBytes);
      config = parseConfig(await this.readBlob(manifest.config.digest, manifest.config.size));
    } catch (e) {
      if (e instanceof SitefileError) throw e;
      throw storeError(`image ${shortId(id)} is corrupt: ${(e as Error).message}`, e);
    }
    const stored = entries.map((e) => e.stored).filter((s): s is string => !!s).sort()[0] ?? config.created;
    return {
      id,
      tags: entries.flatMap((e) => (e.refName ? [e.refName] : [])),
      created: stored,
      files: Number(manifest.annotations[ANN.files] ?? 0),
      size: Number(manifest.annotations[ANN.size] ?? 0),
      manifest,
      config,
      root: config.labels[ANN.root] ?? '/',
      layerSize: manifest.layers[siteLayerIndex(manifest, config)]?.size ?? manifest.layers[0]?.size ?? 0,
    };
  }

  /**
   * Load an image's files (under its root, or under `o.root`), verifying manifest, config, layer digest
   * and diff_id. `o.origin` is what the files' origins and error messages name (default `image:<short id>`).
   */
  async loadTree(id: string, o: { readonly origin?: string; readonly root?: string } = {}): Promise<Tree> {
    const d = await this.details(id);
    const origin = o.origin ?? `image:${shortId(id)}`;
    const layers = siteLayerIndices(d.manifest, d.config).map((i) => d.manifest.layers[i]!);
    if (layers.length === 0) throw new SitefileError('E_STORE', `${origin}: not a Sitefile image (it has ${d.manifest.layers.length} layers and no site layer)`);
    const blobs: Uint8Array[] = [];
    for (const l of layers) blobs.push(await this.readBlob(l.digest, l.size));
    return extractImageFiles(d.manifest, d.config, blobs, origin, o.root);
  }

  /**
   * The image as a standalone OCI layout Tree (for `save`), optionally annotated with a tag.
   * Includes every layer, so an image built on a BASE is self-contained.
   */
  async layoutOf(id: string, refName?: string): Promise<Tree> {
    const d = await this.details(id);
    const site = d.manifest.layers[siteLayerIndex(d.manifest, d.config)];
    if (!site) throw new SitefileError('E_STORE', `image ${shortId(id)} is not a Sitefile image and cannot be saved`);
    const base = new Map<string, Uint8Array>();
    for (const l of d.manifest.layers) if (l.digest !== site.digest) base.set(l.digest, await this.readBlob(l.digest, l.size));
    return layoutTree(
      {
        manifestBytes: await this.readBlob(id),
        manifestDigest: id,
        configBytes: await this.readBlob(d.manifest.config.digest),
        configDigest: d.manifest.config.digest,
        layerBytes: await this.readBlob(site.digest, site.size),
        layerDigest: site.digest,
      },
      refName,
      base,
      await this.referrers(id),
    );
  }

  // ------------------------------------------------------------ referrers (attestations)

  private referrersFile(id: string): string {
    blobPathOf(id); // validates the digest
    return path.join(this.home, 'referrers', 'sha256', id.slice(7));
  }

  /** Descriptors of the attestations stored for image `id` (none: empty). */
  async referrerDescriptors(id: string): Promise<ReferrerDescriptor[]> {
    let bytes: Buffer;
    try {
      bytes = await fs.readFile(this.referrersFile(id));
    } catch (e) {
      if (errCode(e) === 'ENOENT') return [];
      throw storeError(`cannot read the attestations of ${shortId(id)}: ${(e as Error).message}`, e);
    }
    try {
      return decodeReferrersIndex(bytes);
    } catch (e) {
      throw storeError(`the attestations of ${shortId(id)} are corrupt: ${(e as Error).message}`, e);
    }
  }

  /** The attestations stored for image `id`, with their blobs (digest-verified). */
  async referrers(id: string): Promise<Referrer[]> {
    const out: Referrer[] = [];
    for (const d of await this.referrerDescriptors(id)) {
      const manifestBytes = await this.readBlob(d.digest, d.size);
      let m: ParsedManifest;
      try {
        m = parseManifest(manifestBytes);
      } catch (e) {
        throw storeError(`attestation ${shortId(d.digest)} of ${shortId(id)} is corrupt: ${(e as Error).message}`, e);
      }
      const blobs: Array<readonly [string, Uint8Array]> = [[d.digest, manifestBytes]];
      for (const b of [m.config, ...m.layers]) blobs.push([b.digest, await this.readBlob(b.digest, b.size)]);
      out.push({ descriptor: d, blobs });
    }
    return out;
  }

  /**
   * Attach attestations to stored image `id`: their blobs are written, and each replaces a stored
   * attestation with the same predicate type (others are kept).
   */
  async putReferrers(id: string, referrers: readonly Referrer[]): Promise<void> {
    if (referrers.length === 0) return;
    await this.withLock(async () => {
      if (!(await this.readIndex()).some((e) => e.digest === id)) throw new SitefileError('E_NOT_FOUND', `no such image: ${shortId(id)}`);
      for (const r of referrers) for (const [d, b] of r.blobs) await this.putBlob(d, b);
      const kind = (d: ReferrerDescriptor): string => `${d.artifactType ?? ''}\0${d.annotations?.['in-toto.io/predicate-type'] ?? ''}`;
      const fresh = new Set(referrers.map((r) => kind(r.descriptor)));
      const kept = (await this.referrerDescriptors(id)).filter((d) => !fresh.has(kind(d)) && !referrers.some((r) => r.descriptor.digest === d.digest));
      const file = this.referrersFile(id);
      await fs.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
      await this.writeAtomic(file, encodeReferrersIndex([...kept, ...referrers.map((r) => r.descriptor)]));
    });
  }

  /** Blob hexes of the attestations of `id` (manifests, configs, layers); unreadable ones count as none. */
  private async referrerBlobHexes(id: string): Promise<string[]> {
    const out: string[] = [];
    for (const d of await this.referrerDescriptors(id).catch(() => [])) {
      out.push(d.digest.slice(7));
      try {
        const m = parseManifest(await this.readBlob(d.digest, d.size));
        for (const b of [m.config, ...m.layers]) out.push(b.digest.slice(7));
      } catch {
        /* missing or corrupt: nothing more to keep */
      }
    }
    return out;
  }

  /** Every stored image, newest first. */
  async list(): Promise<ImageSummary[]> {
    const out: ImageSummary[] = [];
    for (const id of [...new Set((await this.readIndex()).map((e) => e.digest))]) {
      const { id: _i, tags, created, files, size } = await this.details(id);
      out.push({ id, tags, created, files, size });
    }
    return out.sort((a, b) => (a.created < b.created ? 1 : a.created > b.created ? -1 : a.id < b.id ? -1 : 1));
  }

  private async idsWithPrefix(input: string): Promise<string[]> {
    const hex = input.replace(/^sha256:/, '');
    return [...new Set((await this.readIndex()).map((e) => e.digest))].filter((id) => id.startsWith(`sha256:${hex}`));
  }

  /** Resolve a reference or (short) image ID to a full image ID. */
  async resolve(input: string): Promise<string> {
    if (looksLikeImageId(input)) {
      const matches = await this.idsWithPrefix(input);
      if (matches.length === 1) return matches[0] as string;
      if (matches.length > 1) throw storeError(`ambiguous image ID '${input}' matches ${matches.length} images`);
    }
    const ref = parseLocalRef(input);
    if (ref.digest !== undefined) {
      if (!(await this.readIndex()).some((e) => e.digest === ref.digest)) throw new SitefileError('E_NOT_FOUND', `no such image: ${input}`);
      return ref.digest;
    }
    const id = (await this.refs())[refKey(ref)];
    if (id === undefined) throw new SitefileError('E_NOT_FOUND', `no such image: ${input}`);
    return id;
  }

  /** Point `name:tag` at an image (replacing any previous target). Returns the `name:tag` key. */
  async tag(id: string, ref: ImageRef): Promise<string> {
    if (ref.digest !== undefined) {
      throw new SitefileError('E_USAGE', 'a tag must be name[:tag] or registry/name[:tag] (no digest)');
    }
    const key = refKey(ref);
    await this.withLock(async () => {
      const entries = await this.readIndex();
      const mine = entries.find((e) => e.digest === id);
      if (!mine) throw new SitefileError('E_NOT_FOUND', `no such image: ${shortId(id)}`);
      const previous = entries.find((e) => e.refName === key);
      const next = entries.filter((e) => e.refName !== key);
      // The old target stays as an untagged image if this was its last tag (writeIndex adds the dangling entry).
      if (previous && !next.some((e) => e.digest === previous.digest)) next.push({ ...previous, refName: undefined });
      const stored = entries.find((e) => e.digest === id && e.stored)?.stored;
      next.push({ digest: id, size: mine.size, refName: key, stored });
      await this.writeIndex(next);
    });
    return key;
  }

  /**
   * `rmi`: remove a tag, or an image by ID. The image leaves the index when no tag references it
   * (blobs are reclaimed by `prune`). Removing by ID while several tags point at it needs `force`.
   */
  async removeImage(input: string, force = false): Promise<{ untagged: string[]; deleted: string | undefined }> {
    return this.withLock(async () => {
      let entries = await this.readIndex();
      const untagged: string[] = [];
      let id: string;
      const idMatches = looksLikeImageId(input) ? await this.idsWithPrefix(input) : [];
      if (idMatches.length > 0 || /^sha256:[0-9a-f]{64}$/.test(input)) {
        id = await this.resolve(input);
        const tags = entries.filter((e) => e.digest === id && e.refName).map((e) => e.refName as string);
        if (tags.length > 1 && !force) {
          throw storeError(`image ${shortId(id)} is referenced by ${tags.length} tags (${tags.join(', ')}); remove a tag, or use -f`);
        }
        untagged.push(...tags);
        entries = entries.filter((e) => e.digest !== id);
      } else {
        const ref = parseLocalRef(input);
        if (ref.digest !== undefined) throw new SitefileError('E_USAGE', 'rmi takes name[:tag] or an image ID, not name@digest');
        const key = refKey(ref);
        const target = entries.find((e) => e.refName === key);
        if (!target) throw new SitefileError('E_NOT_FOUND', `no such image: ${input}`);
        id = target.digest;
        untagged.push(key);
        entries = entries.filter((e) => e.refName !== key);
        if (entries.some((e) => e.digest === id)) {
          await this.writeIndex(entries);
          return { untagged, deleted: undefined };
        }
      }
      await this.writeIndex(entries);
      await fs.rm(this.referrersFile(id), { force: true });
      return { untagged, deleted: id };
    });
  }

  /**
   * What the size limit may evict: `live` holds the blob hexes of tagged images (never evicted),
   * `untagged` each untagged image with the blobs it uses. An unreadable tagged manifest throws
   * (its blobs are unknown, so nothing may be evicted).
   */
  async gcView(): Promise<{ live: Set<string>; untagged: Array<{ id: string; blobs: string[] }> }> {
    const entries = await this.readIndex();
    const blobsOf = async (id: string): Promise<string[]> => {
      let m: ParsedManifest;
      try {
        m = parseManifest(await this.readBlob(id));
      } catch (e) {
        if (e instanceof SitefileError) throw e;
        throw storeError(`image ${shortId(id)} is corrupt: ${(e as Error).message}`, e);
      }
      return [...[id, m.config.digest, ...m.layers.map((l) => l.digest)].map((d) => d.slice(7)), ...(await this.referrerBlobHexes(id))];
    };
    const live = new Set<string>();
    for (const id of new Set(entries.filter((e) => e.refName).map((e) => e.digest))) for (const h of await blobsOf(id)) live.add(h);
    const untagged: Array<{ id: string; blobs: string[] }> = [];
    for (const id of new Set(entries.filter((e) => !e.refName).map((e) => e.digest))) {
      if (live.has(id.slice(7))) continue;
      untagged.push({ id, blobs: await blobsOf(id).catch(() => [id.slice(7)]) });
    }
    return { live, untagged };
  }

  /** Remove an untagged image from the index, with its attestation list (its blobs stay). The caller holds the lock. */
  async dropUntaggedLocked(id: string): Promise<void> {
    const entries = await this.readIndex();
    if (entries.some((e) => e.digest === id && e.refName)) return;
    await this.writeIndex(entries.filter((e) => e.digest !== id));
    await fs.rm(this.referrersFile(id), { force: true });
  }

  /** Drop untagged images from the index, then delete every blob no remaining image uses. */
  async prune(): Promise<{ manifests: number; blobs: number; bytes: number }> {
    return this.withLock(async () => {
      const entries = await this.readIndex();
      const keep = entries.filter((e) => e.refName);
      const dropped = new Set(entries.filter((e) => !e.refName).map((e) => e.digest));
      for (const e of keep) dropped.delete(e.digest);
      await this.writeIndex(keep);
      const live = new Set<string>();
      for (const id of new Set(keep.map((e) => e.digest))) {
        const d = await this.details(id);
        live.add(id.slice(7));
        live.add(d.manifest.config.digest.slice(7));
        for (const l of d.manifest.layers) live.add(l.digest.slice(7));
        for (const h of await this.referrerBlobHexes(id)) live.add(h);
      }
      // Attestation lists of images that are gone.
      const refDir = path.join(this.home, 'referrers', 'sha256');
      for (const name of await fs.readdir(refDir).catch(() => [] as string[])) {
        if (HEX64.test(name) && !keep.some((e) => e.digest === `sha256:${name}`)) await fs.rm(path.join(refDir, name), { force: true });
      }
      let blobs = 0;
      let bytes = 0;
      const dir = path.join(this.home, 'blobs', 'sha256');
      for (const name of await fs.readdir(dir)) {
        const full = path.join(dir, name);
        const st = await fs.stat(full).catch(() => undefined);
        if (!st?.isFile()) continue;
        if (HEX64.test(name) ? live.has(name) : Date.now() - st.mtimeMs < 3_600_000) continue; // keep live blobs and fresh temp files
        await fs.rm(full, { force: true });
        blobs++;
        bytes += st.size;
      }
      return { manifests: dropped.size, blobs, bytes };
    });
  }
}
